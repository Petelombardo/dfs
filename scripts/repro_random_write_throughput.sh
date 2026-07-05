#!/bin/bash
# Measure sustained random 4K write throughput against a large pre-existing
# file, mimicking kdiskmark's RND4K Q32T1/Q1T1 pattern against a VM disk.
#
# Built to catch the 2026-07-05 regression: raising the write-buffer cap's
# accounting to correctly charge gap-fill memory (resident_bytes() fix)
# caused random-write throughput to collapse ~40x (3MB/s -> 0.07MB/s) on
# server5, because random access across a multi-GB disk touches a new chunk
# almost every write, each paying the full gap-fill cost against the cap.
# Sequential writes/reads were unaffected (same/adjacent chunk reused across
# many writes, amortizing the cost) — this script specifically targets the
# random case to catch a regression the other repro scripts wouldn't.
#
# Usage: bash scripts/repro_random_write_throughput.sh [file_size_mb] [duration_sec] [num_threads]

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-throughput-mount
LOG=/tmp/dfs-throughput-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
TESTFILE="$MOUNT/throughput.img"

FILE_SIZE_MB=${1:-1024}   # match kdiskmark's typical ~1GiB test region
DURATION=${2:-30}
NUM_THREADS=${3:-8}       # roughly approximates Q32T1-ish concurrency without the python-loop overhead skewing results

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

env RUST_LOG=info ${MALLOC_ARENA_MAX:+MALLOC_ARENA_MAX="$MALLOC_ARENA_MAX"} \
    ${DFS_WRITE_BUFFER_CAP_MB:+DFS_WRITE_BUFFER_CAP_MB="$DFS_WRITE_BUFFER_CAP_MB"} \
    "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level info &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
CLIENT_PID=$(pgrep -f "dfs-client mount $MOUNT" | head -1)
echo "Mounted. Client PID=$CLIENT_PID"

echo "=== Pre-allocating ${FILE_SIZE_MB}MB test file (establishes existing chunks) ==="
dd if=/dev/zero of="$TESTFILE" bs=1M count="$FILE_SIZE_MB" 2>/dev/null
sync "$MOUNT"
sleep 1
echo "Initial write committed."

echo "=== Firing $NUM_THREADS threads of sustained random 4K writes for ${DURATION}s ==="

run_worker() {
    local id=$1
    python3 - <<PYEOF &
import os, random, time

fd = os.open("$TESTFILE", os.O_RDWR)
size = $FILE_SIZE_MB * 1024 * 1024
end = time.time() + $DURATION
buf = bytes([($id % 255) + 1] * 4096)
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
print(f"worker $id: {n} writes, {mb:.2f}MB, {mb/elapsed:.3f} MB/s, {n/elapsed:.1f} iops")
PYEOF
}

PIDS=()
for id in $(seq 0 $(( NUM_THREADS - 1 ))); do
    run_worker "$id" > "$LOG/worker_$id.out" 2>&1
    PIDS+=($!)
done
for pid in "${PIDS[@]}"; do
    wait "$pid" 2>/dev/null || true
done

echo ""
echo "=== Per-worker results ==="
cat "$LOG"/worker_*.out

echo ""
echo "=== Aggregate throughput ==="
python3 - <<PYEOF
import re, glob
total_mb = 0.0
total_iops = 0.0
for fn in glob.glob("$LOG/worker_*.out"):
    with open(fn) as f:
        content = f.read()
    m = re.search(r'([\d.]+)MB, ([\d.]+) MB/s, ([\d.]+) iops', content)
    if m:
        total_mb += float(m.group(1))
        total_iops += float(m.group(3))
print(f"Total: {total_mb:.2f}MB written across $NUM_THREADS threads in ${DURATION}s")
print(f"Aggregate throughput: {total_mb/$DURATION:.3f} MB/s, {total_iops:.1f} iops")
PYEOF

echo ""
echo "=== Peak RSS during test ==="
sync "$MOUNT"
sleep 1
RSS_KB=$(awk '/^VmRSS/{print $2}' /proc/$CLIENT_PID/status 2>/dev/null)
echo "Final RSS: ${RSS_KB:-unknown} kB"

echo ""
echo "=== Cleanup ==="
kill "$CLIENT_PID" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -f "dfs-server" 2>/dev/null || true
echo "Done. Logs in $LOG/"
