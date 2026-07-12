#!/bin/bash
# Repro: MultiPatch "REPLICA DISAGREEMENT" during hot-chunk writes, optionally
# with a node self-restart mid-burst.
#
# Root-caused 2026-07-11 (VM 111 install on staging): a chunk landed on only
# 1 of 2 targeted replicas at some point, and every subsequent patch to that
# same (file, chunk_idx) slot then disagreed across replicas. Confirmed via
# matched fold traces on staging that fold() itself is deterministic given a
# matching delta history (same records -> same hash on different nodes) --
# so a disagreement on patch N means the two replicas' histories had ALREADY
# forked before patch N arrived, most likely from an earlier patch that was
# accepted as "done" despite landing on only one of the two targeted nodes.
#
# This script fsyncs after every small pwrite (matching the fsync-heavy
# pattern seen right before every disagreement instance in the staging log)
# to force each write out as its own network round trip instead of letting
# the client's own write-buffer coalesce them locally into one batch.
#
# Usage: bash scripts/repro_replica_disagreement.sh [duration_secs] [--kill]
#   --kill  also kill+restart one storage node partway through the burst,
#           simulating the exact self-restart-mid-write staging scenario.
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-repro-disagree
MOUNT=/tmp/dfs-repro-disagree-mount
LOG=/tmp/dfs-repro-disagree-logs
CLUSTER="127.0.0.1:8960,127.0.0.1:8961,127.0.0.1:8962,127.0.0.1:8963,127.0.0.1:8964"
BIN="$REPO/target/release"
DURATION="${1:-30}"
DO_KILL="${2:-}"

cleanup_all() {
    pkill -f "dfs-server.*dfs-repro-disagree" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$BASE" "$MOUNT" "$LOG"
mkdir -p "$MOUNT" "$LOG" "$BASE"

echo "=== Initializing 5-node cluster (debug logging) ==="
declare -a SERVER_PIDS
for i in 1 2 3 4 5; do
    NODE_DIR="$BASE/node${i}"
    PORT=$((8960 + i - 1))
    "$BIN/dfs-server" init --data-dir "$NODE_DIR/data" --meta-dir "$NODE_DIR/metadata" --config "$NODE_DIR/config.toml" >/dev/null
    sed -i "s/listen_addr = \"0.0.0.0:8900\"/listen_addr = \"127.0.0.1:${PORT}\"/" "$NODE_DIR/config.toml"
    if [ $i -gt 1 ]; then
        sed -i 's/seed_nodes = \[\]/seed_nodes = ["127.0.0.1:8960"]/' "$NODE_DIR/config.toml"
    fi
done
for i in 1 2 3 4 5; do
    nohup "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" --log-level debug >> "$LOG/server${i}.log" 2>&1 &
    SERVER_PIDS[$i]=$!
done
sleep 3

nohup "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
echo "Mounted."

IMG="$MOUNT/hotchunk.img"
echo "=== Writing 4MB base (single chunk) ==="
dd if=/dev/urandom of="$IMG" bs=4M count=1 2>/dev/null
sync
sleep 1

echo "=== Starting hot-chunk write storm (fsync-per-write, single 4MB region) for ${DURATION}s ==="
python3 - "$IMG" "$DURATION" > "$LOG/writer.log" 2>&1 <<'PYEOF' &
import os, sys, random, time
img, duration = sys.argv[1], int(sys.argv[2])
fd = os.open(img, os.O_WRONLY)
end = time.time() + duration
n = 0
while time.time() < end:
    length = random.choice([4096, 8192, 16384])
    off = random.randint(0, 4*1024*1024 - length)
    data = os.urandom(length)
    os.pwrite(fd, data, off)
    os.fsync(fd)
    n += 1
os.close(fd)
print(f"wrote {n} patches")
PYEOF
WRITER_PID=$!

if [ "$DO_KILL" = "--kill" ]; then
    KILL_WAIT=$(python3 -c "print($DURATION * 0.4)")
    sleep "$KILL_WAIT"
    KILL_NODE=2
    echo "=== Killing node${KILL_NODE} (simulating self-restart mid-burst) ==="
    kill -9 "${SERVER_PIDS[$KILL_NODE]}" 2>/dev/null
    sleep 1.5
    NODE_DIR="$BASE/node${KILL_NODE}"
    nohup "$BIN/dfs-server" start --config "$NODE_DIR/config.toml" --log-level debug >> "$LOG/server${KILL_NODE}.log" 2>&1 &
    SERVER_PIDS[$KILL_NODE]=$!
    echo "=== node${KILL_NODE} restarted ==="
fi

wait "$WRITER_PID"
echo "=== Writer done: $(cat "$LOG/writer.log") ==="

sync
sleep 2

echo ""
echo "=== Results ==="
DISAGREEMENTS=$(grep -c "REPLICA DISAGREEMENT" "$LOG/client.log" 2>/dev/null || echo 0)
UNDER_REPLICATED=$(grep -c "landed on only" "$LOG/client.log" 2>/dev/null || echo 0)
echo "REPLICA DISAGREEMENT count: $DISAGREEMENTS"
echo "Under-replicated-landing count: $UNDER_REPLICATED"

if [ "$DISAGREEMENTS" -gt 0 ]; then
    echo ""
    echo "=== First disagreement, with context ==="
    grep -n "REPLICA DISAGREEMENT\|landed on only\|backfilled\|still under-replicated" "$LOG/client.log" | head -20
fi

echo ""
echo "Logs: $LOG/client.log, $LOG/server{1..5}.log"
echo "Cluster left running for inspection — use dfs-admin --cluster 127.0.0.1:8960 ... to poke at it."
echo "Run 'bash $0 cleanup' or pkill manually when done."
