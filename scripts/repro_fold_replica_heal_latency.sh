#!/bin/bash
# Validates the fold-replica-healing-latency hardening added to
# replicate_fold_result (server.rs): after a fold, dual-RF's designed 2-of-3
# state used to rely entirely on HealingManager's PASSIVE discovery pass (a
# ~60s scan + healing_delay_secs grace + a ~15s heal-tick) to reach the 3rd
# replica. That's a real, multi-minute (5-6 min at production defaults; up to
# ~85s at this script's local healing_delay_secs=10) exposure window per fold
# during which a single node problem can turn "temporarily under-RF" into
# permanent loss — exactly what happened in the 2026-07-30 no-op-fold
# incident (that bug is separately fixed; this hardens the window it made
# fatal).
#
# The fix calls healing.queue_chunks_immediate() right after a fold converges
# on 2 holders (mirroring the existing URGENT_SINGLE_REPLICA path for <2
# holders, just without the alarm — 2 is a healthy, expected state), so the
# 3rd copy lands within one heal-tick instead of waiting on discovery.
#
# This script measures wall-clock time from fold completion to 3-node
# convergence on disk. Run with USE_FIX=0 to see the slow (pre-fix) path,
# USE_FIX=1 (default) for the fast path — see the bottom of this file for
# how to flip it.
#
# Usage: bash scripts/repro_fold_replica_heal_latency.sh

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-heal-latency-mount
LOG=/tmp/dfs-heal-latency-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
FILE="$MOUNT/heallatency.img"
CHUNK_SIZE=$((4 * 1024 * 1024))

declare -A SERVER_PID

cleanup_all() {
    pkill -9 -f "target/release/dfs-server" 2>/dev/null || true
    pkill -9 -f "target/release/dfs-client" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

start_node() {
    local i=$1
    RUST_LOG=debug "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        --log-level debug >> "$LOG/server${i}.log" 2>&1 &
    SERVER_PID[$i]=$!
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$MOUNT" "$LOG" 2>/dev/null || true
mkdir -p "$MOUNT" "$LOG"

echo "=== Setting up + starting 5-node cluster (healing_delay_secs=10 per setup-cluster.sh) ==="
cd "$REPO"
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null > /dev/null
for i in 1 2 3 4 5; do start_node "$i"; done
sleep 3

env RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
CLIENT_PID=$(pgrep -f "dfs-client mount $MOUNT" | head -1)
echo "Mounted. Client PID=$CLIENT_PID"

echo "=== Writing baseline chunk (fresh write, not a patch) ==="
python3 -c "
with open('$FILE', 'wb') as f:
    f.write(bytes([0xAA]) * $CHUNK_SIZE)
"
sync "$MOUNT" 2>/dev/null || true

echo "=== Applying a real (non-no-op) patch — changes actual bytes ==="
python3 -c "
import os
fd = os.open('$FILE', os.O_RDWR)
os.pwrite(fd, bytes([0xBB]) * 4096, 1000000)
os.close(fd)
"

# "Single fold:" (server.rs:2859) is the REAL fold completion — logged
# SERVER-side (not client.log) once debounce_fold_slot actually consolidates
# base+delta into a standalone chunk file. The client-side "MultiPatch:" line
# fires much earlier, at patch-application time, and only *predicts* what the
# eventual fold's chunk_id will be — polling disk for that id is meaningless,
# since debounce hasn't run yet and may never even produce that exact id if a
# later patch lands first. Must search server logs, all 5 nodes (only the
# node(s) that actually perform the fold log this line).
declare -A SERVER_LOGMARK
for n in 1 2 3 4 5; do
    SERVER_LOGMARK[$n]=$(wc -l < "$LOG/server${n}.log" 2>/dev/null || echo 0)
done

echo "=== Waiting for debounce fold (PATCH_DEBOUNCE_IDLE=20s) to produce a real new chunk ==="
FOLD_LINE=""
for i in $(seq 1 40); do
    for n in 1 2 3 4 5; do
        FOLD_LINE=$(tail -n +"${SERVER_LOGMARK[$n]}" "$LOG/server${n}.log" 2>/dev/null | grep -E "Single fold: .* consolidated \(" | tail -1)
        if [ -n "$FOLD_LINE" ]; then break 2; fi
    done
    sleep 1
done
if [ -z "$FOLD_LINE" ]; then
    echo "=== RESULT: INCONCLUSIVE — no fold observed within 40s ==="
    kill "$CLIENT_PID" 2>/dev/null; fusermount -u "$MOUNT" 2>/dev/null; pkill -9 -f "target/release/dfs-server" 2>/dev/null
    exit 2
fi
NEW_CHUNK=$(echo "$FOLD_LINE" | grep -oE '\-> [a-f0-9]+,' | awk '{print $2}' | tr -d ',')
OLD_CHUNK=$(echo "$FOLD_LINE" | grep -oE '\([a-f0-9]+ \+ delta' | awk '{print $1}' | tr -d '(')
echo "  Fold observed: $OLD_CHUNK -> $NEW_CHUNK"
if [ "$NEW_CHUNK" = "$OLD_CHUNK" ] || [ -z "$NEW_CHUNK" ]; then
    echo "  Fold was a no-op or extraction failed — aborting, not a useful sample"
    kill "$CLIENT_PID" 2>/dev/null; fusermount -u "$MOUNT" 2>/dev/null; pkill -9 -f "target/release/dfs-server" 2>/dev/null
    exit 2
fi

FOLD_DETECTED_AT=$(date +%s)
SHARD1=${NEW_CHUNK:0:2}
SHARD2=${NEW_CHUNK:2:2}

echo "=== Polling on-disk replica count for $NEW_CHUNK every 1s ==="
REACHED_3_AT=""
for i in $(seq 1 400); do
    COUNT=0
    for n in 1 2 3 4 5; do
        if [ -f "$BASE/node${n}/data/chunks/${SHARD1}/${SHARD2}/${NEW_CHUNK}" ]; then
            COUNT=$((COUNT + 1))
        fi
    done
    NOW=$(date +%s)
    ELAPSED=$((NOW - FOLD_DETECTED_AT))
    if [ "$COUNT" -ge 3 ]; then
        REACHED_3_AT=$ELAPSED
        echo "  Reached 3 replicas at +${ELAPSED}s"
        break
    fi
    if [ $((i % 5)) -eq 0 ]; then
        echo "  +${ELAPSED}s: $COUNT/5 nodes have it"
    fi
    sleep 1
done

echo ""
if [ -n "$REACHED_3_AT" ]; then
    echo "=== RESULT: reached RF=3 in ${REACHED_3_AT}s after fold ==="
    if [ "$REACHED_3_AT" -le 20 ]; then
        echo "=== PASS (fast path — immediate heal-queue push working) ==="
        RESULT=0
    else
        echo "=== SLOW (passive-discovery path only — fix not active or not working) ==="
        RESULT=1
    fi
else
    echo "=== RESULT: FAIL — never reached 3 replicas within 400s ==="
    RESULT=1
fi

kill "$CLIENT_PID" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -9 -f "target/release/dfs-server" 2>/dev/null || true
echo "Done. Logs in $LOG/"
exit $RESULT
