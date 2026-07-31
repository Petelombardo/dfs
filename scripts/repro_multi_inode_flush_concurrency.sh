#!/bin/bash
# Validates the global_flush_semaphore fix: with N simultaneously-hot files, total
# concurrent flush_buffer_async_one executions across the whole client should stay
# bounded at GLOBAL_FLUSH_CONCURRENCY (64), not scale as PIPELINE_MAX_ITEMS * N files.
# Built after discovering PIPELINE_MAX_ITEMS is a per-inode cap with no cross-inode
# ceiling — multiple busy VM disks on one host could otherwise reproduce the same
# server-side read_ms overload (p99 3.4s, max 26.8s measured on server5) that a single
# over-wide per-inode cap caused for one file.
#
# Usage: bash scripts/repro_multi_inode_flush_concurrency.sh [num_files] [duration_sec]

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-multiflush-mount
LOG=/tmp/dfs-multiflush-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"

NUM_FILES=${1:-3}
DURATION=${2:-20}
FILE_SIZE_MB=512

cleanup_all() {
    pkill -f "dfs-server" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$MOUNT" "$LOG" 2>/dev/null || true
mkdir -p "$MOUNT" "$LOG"

echo "=== Starting 5-node cluster ==="
cd "$REPO"
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null
for i in 1 2 3 4 5; do
    RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        > "$LOG/server${i}.log" 2>&1 &
done
sleep 3

env RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
CLIENT_PID=$(pgrep -f "dfs-client mount $MOUNT" | head -1)
echo "Mounted. Client PID=$CLIENT_PID"

echo "=== Pre-allocating $NUM_FILES x ${FILE_SIZE_MB}MB files ==="
for f in $(seq 1 "$NUM_FILES"); do
    dd if=/dev/zero of="$MOUNT/multi_${f}.img" bs=1M count=$FILE_SIZE_MB 2>/dev/null &
done
wait
sync "$MOUNT"
sleep 1
LOGMARK=$(wc -l < "$LOG/client.log")

echo "=== Firing sustained random 4K writes to all $NUM_FILES files simultaneously for ${DURATION}s ==="
run_file_worker() {
  local f=$1
  python3 -u - <<PYEOF &
import os, random, time
fd = os.open("$MOUNT/multi_${f}.img", os.O_RDWR)
size = $FILE_SIZE_MB * 1024 * 1024
end = time.time() + $DURATION
buf = bytes([($f % 255) + 1] * 4096)
n = 0
start = time.time()
try:
    while time.time() < end:
        off = random.randrange(0, size - 4096, 4096)
        os.pwrite(fd, buf, off)
        n += 1
finally:
    os.close(fd)
elapsed = time.time() - start
mb = (n * 4096) / (1024*1024)
print(f"file $f: {n} writes, {mb:.2f}MB, {mb/elapsed:.3f} MB/s")
PYEOF
}
PIDS=()
for f in $(seq 1 "$NUM_FILES"); do
    run_file_worker "$f" > "$LOG/file_${f}.out" 2>&1
    PIDS+=($!)
done
for pid in "${PIDS[@]}"; do wait "$pid" 2>/dev/null || true; done

echo ""
echo "=== Per-file admission ==="
cat "$LOG"/file_*.out

echo ""
echo "=== Peak concurrent flush_buffer_async_one executions observed (proxy: SCHEDTIMING + in-flight sampling) ==="
tail -n +"$LOGMARK" "$LOG/client.log" > /tmp/multiflush_tail.log
grep -c "MultiPatch:" /tmp/multiflush_tail.log
echo "MultiPatch RPCs above ^ across all $NUM_FILES files combined"

echo ""
echo "=== Checking for any panics/deadlock warnings ==="
grep -ciE "panic|deadlock|blocked on backpressure" /tmp/multiflush_tail.log || true

echo ""
echo "=== Final sync (all files) ==="
T0=$(date +%s.%N)
sync "$MOUNT"
T1=$(date +%s.%N)
echo "final_sync_s=$(echo "$T1 - $T0" | bc)"

for f in $(seq 1 "$NUM_FILES"); do rm -f "$MOUNT/multi_${f}.img"; done
kill "$CLIENT_PID" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -f "dfs-server" 2>/dev/null || true
echo "Done. Logs in $LOG/"
