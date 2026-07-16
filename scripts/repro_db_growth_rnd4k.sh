#!/bin/bash
# Phase 0 repro: metadata redb file growth during kdiskmark-style RND4K writes
# to a 1GB file.
#
# Background (see /root/.claude/plans/sunny-launching-nygaard.md): a real
# kdiskmark RND4K run against a 1GB file grew the metadata DB by ~750MB.
# Root cause (verified in code) is that every 4K write independently commits
# put_patch_state_pending + put_chunk_seq as separate redb write transactions
# on every replica node (server.rs:8433/8652/8946). This script reproduces
# sustained 4K random writes locally and samples every node's metadata.redb
# file size every 5s throughout, so the per-call-site [META TXN] counters
# (dfs-server/src/metadata.rs's note_txn / dfs-server/src/server.rs's 60s
# compaction-loop log line) can be read back afterward to attribute the
# growth to specific call sites.
#
# Usage: bash scripts/repro_db_growth_rnd4k.sh [duration_secs]
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-mount
LOG=/tmp/dfs-test-logs/repro-rnd4k
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
CSV="$LOG/db_growth.csv"

DURATION="${1:-120}"
SAMPLE_INTERVAL_SECS=5
FILE_MB=1024   # 1GB target file
FILE="$MOUNT/rnd4k.bin"

# Same memory-scaled-cache override rationale as test_local_suite.sh.
export DFS_CHUNK_RING_CAPACITY=8
export DFS_DELTA_RING_CAPACITY=8
export DFS_MAX_CACHE_CHUNKS=8
export DFS_WRITE_BUFFER_CAP_MB=32

declare -A SERVER_PID
CLIENT_PID=""
SAMPLER_PID=""
FIO_PID=""

# kill_pid_and_wait <pid>: SIGTERM, then bounded poll for actual exit before
# falling back to -9. Never pkill -f — see feedback_pkill_f_self_match_footgun.md.
kill_pid_and_wait() {
    local pid="$1"
    [ -z "$pid" ] && return 0
    kill "$pid" 2>/dev/null || true
    local waited=0
    while kill -0 "$pid" 2>/dev/null; do
        sleep 0.1
        waited=$((waited + 1))
        [ "$waited" -gt 50 ] && break   # 5s cap
    done
    kill -0 "$pid" 2>/dev/null && kill -9 "$pid" 2>/dev/null || true
}

cleanup() {
    echo "=== Cleaning up ==="
    kill_pid_and_wait "$FIO_PID"
    kill_pid_and_wait "$SAMPLER_PID"
    kill_pid_and_wait "$CLIENT_PID"
    fusermount -u "$MOUNT" 2>/dev/null || true
    for i in 1 2 3 4 5; do
        kill_pid_and_wait "${SERVER_PID[$i]:-}"
    done
}
trap cleanup EXIT

# Discover and kill any leftover processes from a prior aborted run of THIS
# script (same BASE/MOUNT paths) via pgrep -> kill-by-PID. No pkill -f.
for pid in $(pgrep -f "dfs-server start --config $BASE/node" 2>/dev/null || true); do
    kill -9 "$pid" 2>/dev/null || true
done
for pid in $(pgrep -f "dfs-client mount $MOUNT" 2>/dev/null || true); do
    kill -9 "$pid" 2>/dev/null || true
done
sleep 0.5
fusermount -u "$MOUNT" 2>/dev/null || true
sudo rm -rf "$BASE" "$MOUNT" "$LOG" 2>/dev/null || rm -rf "$BASE" "$MOUNT" "$LOG" 2>/dev/null || true
mkdir -p "$MOUNT" "$LOG"

echo "=== Starting 5-node cluster ==="
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null

for i in 1 2 3 4 5; do
    RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        > "$LOG/server${i}.log" 2>&1 &
    SERVER_PID[$i]=$!
done
sleep 3

RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
CLIENT_PID=$!
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
echo "Mounted."

# ── Find each node's metadata redb file, and sample all 5 into the CSV ─────
find_redb_file() {
    local metadir="$1"
    if [ -f "$metadir/metadata.redb" ]; then
        echo "$metadir/metadata.redb"
    else
        find "$metadir" -maxdepth 1 -name "*.redb" 2>/dev/null | head -1
    fi
}

echo "epoch_secs,node,db_bytes" > "$CSV"
sample_once() {
    local ts
    ts=$(date +%s)
    for i in 1 2 3 4 5; do
        local f sz
        f=$(find_redb_file "$BASE/node${i}/metadata")
        sz=0
        [ -n "$f" ] && [ -f "$f" ] && sz=$(stat -c%s "$f" 2>/dev/null || echo 0)
        echo "${ts},node${i},${sz}" >> "$CSV"
    done
}

# ── Create the 1GB target file ──────────────────────────────────────────────
echo "=== Creating ${FILE_MB}MB file on the mount ==="
dd if=/dev/urandom of="$FILE" bs=4M count=$((FILE_MB / 4)) 2>&1 | tail -3
sync "$MOUNT"
sleep 1
sample_once   # baseline sample before the write storm

# ── Background sampler: every SAMPLE_INTERVAL_SECS for the whole run ───────
(
    while true; do
        sample_once
        sleep "$SAMPLE_INTERVAL_SECS"
    done
) &
SAMPLER_PID=$!

# ── RND4K write storm ────────────────────────────────────────────────────────
if command -v fio >/dev/null 2>&1; then
    echo "=== Running fio RND4K for ${DURATION}s ==="
    fio --name=rnd4k --filename="$FILE" --rw=randwrite --bs=4k --size=1G \
        --runtime="$DURATION" --time_based --ioengine=psync --direct=0 \
        > "$LOG/fio.log" 2>&1 &
    FIO_PID=$!
    wait "$FIO_PID"
    FIO_PID=""
    tail -20 "$LOG/fio.log"
else
    echo "=== fio not found — falling back to a dd-loop RND4K for ${DURATION}s ==="
    (
        NUM_BLOCKS=$(( FILE_MB * 1024 * 1024 / 4096 ))
        END_TS=$(( $(date +%s) + DURATION ))
        COUNT=0
        while [ "$(date +%s)" -lt "$END_TS" ]; do
            BLOCK=$(( (RANDOM * RANDOM + RANDOM) % NUM_BLOCKS ))
            dd if=/dev/urandom of="$FILE" bs=4096 count=1 seek="$BLOCK" conv=notrunc 2>/dev/null
            COUNT=$((COUNT + 1))
        done
        echo "dd-loop fallback: wrote $COUNT random 4K writes" > "$LOG/fio.log"
    ) &
    FIO_PID=$!
    wait "$FIO_PID"
    FIO_PID=""
    cat "$LOG/fio.log"
fi

sync "$MOUNT"
sleep 2

# Stop the background sampler and take one final sample after everything settles.
kill_pid_and_wait "$SAMPLER_PID"
SAMPLER_PID=""
sample_once

echo ""
echo "=== Results ==="
echo "CSV: $CSV"
for i in 1 2 3 4 5; do
    FIRST=$(awk -F, -v n="node${i}" '$2==n {print $3; exit}' "$CSV")
    LAST=$(awk -F, -v n="node${i}" '$2==n {v=$3} END {print v}' "$CSV")
    FIRST=${FIRST:-0}; LAST=${LAST:-0}
    DELTA=$((LAST - FIRST))
    printf "  node%d: %d -> %d bytes (delta %+d, %.1fMB)\n" "$i" "$FIRST" "$LAST" "$DELTA" "$(echo "$DELTA/1048576" | bc -l 2>/dev/null || echo 0)"
done

# Leader's [META TXN] lines — see repro_db_growth_heal.sh's matching comment
# for why we parse `healing status`'s header rather than re-deriving the
# leader address ourselves.
LEADER_LINE=$("$BIN/dfs-admin" --cluster "$CLUSTER" healing status 2>/dev/null | head -1)
LEADER_ADDR=$(echo "$LEADER_LINE" | grep -oP '(?<=leader: )[^)]+' || true)
LEADER_PORT=$(echo "$LEADER_ADDR" | cut -d: -f2)
if [ -n "$LEADER_PORT" ]; then
    LEADER_NODE=$((LEADER_PORT - 8900 + 1))
    echo ""
    echo "=== Leader is node${LEADER_NODE} ($LEADER_ADDR) — [META TXN] lines from its log ==="
    grep "\[META TXN\]" "$LOG/server${LEADER_NODE}.log" 2>/dev/null || echo "  (none logged yet — cycle may not have completed; the compaction-check loop that logs these runs every 60s)"
else
    echo "Could not determine leader; dumping [META TXN] lines from all server logs:"
    grep "\[META TXN\]" "$LOG"/server*.log 2>/dev/null || echo "  (none logged yet)"
fi

echo ""
echo "All server logs (incl. every node's [META TXN] lines): $LOG/server{1..5}.log"
echo "Client log: $LOG/client.log"
echo "fio/fallback log: $LOG/fio.log"
