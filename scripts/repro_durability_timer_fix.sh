#!/bin/bash
# Fast, focused validation of the durability-flush-timer fix (metadata.rs:
# start_durability_flush_timer). Does NOT call sync/dfs_sync at all — the point
# is to confirm the periodic backstop alone (no client-driven durability) is
# enough to survive a kill -9 within DURABILITY_FLUSH_MAX_AGE (2s) of the write.
#
# Before this fix: two separate full runs (repro_orphan_sweep_stale_chunkmap.sh)
# showed a patch's PATCH_STATE_TABLE row did NOT survive a kill -9 even a few
# hundred ms after the write — proving Durability::None writes could sit
# unflushed indefinitely under idle load. This is the quick counter-check.
#
# Usage: bash scripts/repro_durability_timer_fix.sh

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-durability-mount
LOG=/tmp/dfs-durability-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
FILE="$MOUNT/durability.img"
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

stop_node() {
    local i=$1
    kill -9 "${SERVER_PID[$i]}" 2>/dev/null || true
    wait "${SERVER_PID[$i]}" 2>/dev/null || true
    unset SERVER_PID[$i]
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$MOUNT" "$LOG" 2>/dev/null || true
mkdir -p "$MOUNT" "$LOG"

echo "=== Starting 5-node cluster ==="
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

echo "=== Writing baseline + patch 1 (no sync anywhere in this script) ==="
python3 -c "
with open('$FILE', 'wb') as f:
    f.write(bytes([0xAA]) * $CHUNK_SIZE)
"
python3 -c "
import os
fd = os.open('$FILE', os.O_RDWR)
os.pwrite(fd, bytes([0xBB]) * 4096, 1000000)
os.close(fd)
"

echo "=== Determining which node(s) hold this chunk ==="
REPLICA_NODES=()
for i in $(seq 1 50); do
    REPLICA_NODES=()
    for n in 1 2 3 4 5; do
        if grep -qE "MultiPatch:|MERGE-TRACE" "$LOG/server${n}.log" 2>/dev/null; then
            REPLICA_NODES+=("$n")
        fi
    done
    if [ "${#REPLICA_NODES[@]}" -ge 2 ]; then break; fi
    sleep 0.1
done
echo "  Nodes: ${REPLICA_NODES[*]:-none}"
if [ "${#REPLICA_NODES[@]}" -lt 1 ]; then
    echo "  No replica found — aborting"
    kill "$CLIENT_PID" 2>/dev/null; fusermount -u "$MOUNT" 2>/dev/null; pkill -9 -f "target/release/dfs-server" 2>/dev/null
    exit 2
fi
TARGET="${REPLICA_NODES[0]}"

echo "=== Waiting 3s (past DURABILITY_FLUSH_MAX_AGE=2s) — NO sync call, purely the periodic timer ==="
sleep 3

echo "=== Confirming the timer actually fired on node${TARGET} before we kill it ==="
grep -c "op:flush_durability\|FlushDurability" "$LOG/server${TARGET}.log" 2>/dev/null || echo "0"

echo "=== Killing node${TARGET} with kill -9 (simulated crash) ==="
LOGMARK=$(wc -l < "$LOG/server${TARGET}.log" 2>/dev/null || echo 0)
stop_node "$TARGET"

echo "=== Restarting node${TARGET} ==="
start_node "$TARGET"
sleep 3

echo ""
echo "=== node${TARGET}'s resume-sweep activity after restart ==="
tail -n +"$LOGMARK" "$LOG/server${TARGET}.log" | grep -iE "resume sweep|MEM DIAG" | head -10

echo ""
FOUND=$(tail -n +"$LOGMARK" "$LOG/server${TARGET}.log" | grep -c "patch_state resume sweep: found" || true)
if [ "$FOUND" -gt 0 ]; then
    echo "=== RESULT: PASS — resume sweep found the Pending row; it survived the crash ==="
    RESULT=0
else
    echo "=== RESULT: FAIL — resume sweep found nothing; the write did NOT survive the crash ==="
    RESULT=1
fi

kill "$CLIENT_PID" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -9 -f "target/release/dfs-server" 2>/dev/null || true
echo "Done. Logs in $LOG/"
exit $RESULT
