#!/bin/bash
# Phase 0 repro: metadata redb file growth during a healing backlog drain.
#
# Background (see /root/.claude/plans/sunny-launching-nygaard.md): a real
# incident on gluster1 showed the metadata DB grow 257.5MB -> 514.5MB in
# about a minute while draining a ~500-chunk healing backlog. Root cause
# (verified in code) is one redb write transaction per healed chunk on
# several call sites (put_chunk_location, delete_pending_healing, per-peer
# broadcast, ...). This script reproduces a comparable backlog locally and
# samples every node's metadata.redb file size every 5s while it drains, so
# the per-call-site [META TXN] counters (dfs-server/src/metadata.rs's
# note_txn / dfs-server/src/server.rs's 60s compaction-loop log line) can be
# read back afterward to attribute the growth to specific call sites.
#
# Usage: bash scripts/repro_db_growth_heal.sh [num_files] [kill_node]
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-mount
LOG=/tmp/dfs-test-logs/repro-heal
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
CSV="$LOG/db_growth.csv"

NUM_FILES_MB="${1:-500}"    # single file of this many 4MB chunks (~500 => ~500 chunks, matches the real incident)
KILL_NODE="${2:-3}"         # 1-5: which node to kill+wipe to create the healing backlog
SAMPLE_INTERVAL_SECS=5
MAX_WAIT_SECS=600           # 10 minutes

# Same memory-scaled-cache override rationale as test_local_suite.sh: 5 servers
# + 1 client sharing one dev box would otherwise each independently claim a
# "reasonable" chunk_ring/write-buffer budget in ignorance of the other five.
export DFS_CHUNK_RING_CAPACITY=8
export DFS_DELTA_RING_CAPACITY=8
export DFS_MAX_CACHE_CHUNKS=8
export DFS_WRITE_BUFFER_CAP_MB=32

declare -A SERVER_PID
CLIENT_PID=""

# kill_pid_and_wait <pid>: SIGTERM, then bounded poll for actual exit before
# falling back to -9. Mirrors test_local_suite.sh's kill_client_and_wait —
# never pkill -f (see feedback_pkill_f_self_match_footgun.md: pkill -f can
# match its own invoking command line on a remote/SSH invocation and kill the
# wrong thing). Every kill in this script goes through a saved PID.
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
    kill_pid_and_wait "$CLIENT_PID"
    fusermount -u "$MOUNT" 2>/dev/null || true
    for i in 1 2 3 4 5; do
        kill_pid_and_wait "${SERVER_PID[$i]:-}"
    done
}
trap cleanup EXIT

# Discover and kill any leftover processes from a prior aborted run of THIS
# script (same BASE/MOUNT paths) — via pgrep for PIDs, then kill each PID
# individually. No pkill -f anywhere in this script.
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

# ── Find each node's metadata redb file ─────────────────────────────────────
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

# ── Write ~NUM_FILES_MB * 4MB chunks through the mount ──────────────────────
echo "=== Writing ${NUM_FILES_MB}x4MB (~${NUM_FILES_MB} chunks) ==="
dd if=/dev/urandom of="$MOUNT/heal_src.bin" bs=4M count="$NUM_FILES_MB" 2>&1 | tail -3
sync "$MOUNT"
sleep 1
echo "Write complete."
sample_once   # baseline sample before the kill

# ── Kill + wipe one storage node to create a real healing backlog ──────────
echo "=== Killing and wiping node${KILL_NODE} (data+metadata) ==="
kill_pid_and_wait "${SERVER_PID[$KILL_NODE]:-}"
rm -rf "$BASE/node${KILL_NODE}/data" "$BASE/node${KILL_NODE}/metadata"
mkdir -p "$BASE/node${KILL_NODE}/data" "$BASE/node${KILL_NODE}/metadata"

echo "=== Restarting node${KILL_NODE} (fresh, empty data+metadata — same node_id from config.toml) ==="
RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${KILL_NODE}/config.toml" \
    >> "$LOG/server${KILL_NODE}.log" 2>&1 &
SERVER_PID[$KILL_NODE]=$!
sleep 5

# Speed up healing_delay_secs (default 10s from setup-cluster.sh; T38 uses the
# same 2s override) so the backlog visibly drains within this script's
# sampling window instead of waiting on the debounce timer.
"$BIN/dfs-admin" --cluster "$CLUSTER" healing set --healing-delay-secs 2 >/dev/null 2>&1 || true
"$BIN/dfs-admin" --cluster "$CLUSTER" healing trigger >/dev/null 2>&1 || true

# ── Sample every 5s until the healing backlog drains or MAX_WAIT_SECS ──────
echo "=== Sampling DB growth every ${SAMPLE_INTERVAL_SECS}s (up to ${MAX_WAIT_SECS}s) while healing drains ==="
START_TS=$(date +%s)
DRAINED=0
while true; do
    sample_once
    STATUS=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json healing status 2>/dev/null || echo '{}')
    QUEUE=$(echo "$STATUS" | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
    print(d.get('pending_count', 0) + d.get('in_flight_count', 0))
except Exception:
    print('?')
" 2>/dev/null || echo "?")
    ELAPSED=$(( $(date +%s) - START_TS ))
    LATEST_SIZE=$(tail -5 "$CSV" | awk -F, '{sum+=$3} END {printf "%.1f", sum/1048576}')
    echo "  t=${ELAPSED}s heal_queue=${QUEUE} total_db_across_nodes=${LATEST_SIZE}MB"
    if [ "$QUEUE" = "0" ]; then
        echo "  Healing backlog drained."
        DRAINED=1
        break
    fi
    if [ "$ELAPSED" -ge "$MAX_WAIT_SECS" ]; then
        echo "  WARN: timed out after ${MAX_WAIT_SECS}s waiting for healing to drain."
        break
    fi
    sleep "$SAMPLE_INTERVAL_SECS"
done
sample_once   # final sample after drain/timeout

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
echo "Drained: $DRAINED"

# Leader's [META TXN] lines — parse the leader address out of `healing status`'s
# non-json header line ("DFS Healing Status (leader: 127.0.0.1:PORT)") rather
# than re-deriving it, since dfs-admin already resolves it the same way the
# cluster itself would (min NodeId among Online nodes).
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
echo "Full server logs: $LOG/server{1..5}.log"
echo "Client log: $LOG/client.log"
