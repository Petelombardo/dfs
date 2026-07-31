#!/bin/bash
# Side-by-side measurement of RND4K drain behavior: how much data sits
# unflushed at the moment the write workload stops, and how long the
# trailing sync takes to reach full durability. Built to validate lowering
# SLOT_DIRTY_FLUSH_THRESHOLD_BYTES (fuse_impl.rs) — the fragmentation-gated
# safety net that lets a scattered-write slot flush before it's full or idle.
# Confirmed via manual log inspection that at CHUNK_SIZE/4 (1MB) this repro's
# parameters (1GB file, 8 threads, 20s) never cross the threshold within the
# test window — zero MultiPatch RPCs fire until the trailing sync forces a
# full drain. This script quantifies that gap and the improvement from
# lowering the threshold.
#
# Usage: bash scripts/repro_rnd4k_drain_lag.sh [file_size_mb] [duration_sec] [num_threads]

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-drainlag-mount
LOG=/tmp/dfs-drainlag-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
TESTFILE="$MOUNT/drainlag.img"

FILE_SIZE_MB=${1:-1024}
DURATION=${2:-20}
NUM_THREADS=${3:-8}

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

echo "=== Pre-allocating ${FILE_SIZE_MB}MB test file ==="
dd if=/dev/zero of="$TESTFILE" bs=1M count="$FILE_SIZE_MB" 2>/dev/null
sync "$MOUNT"
sleep 1
LOGMARK=$(wc -l < "$LOG/client.log")

echo "=== Firing $NUM_THREADS threads of sustained random 4K writes for ${DURATION}s ==="
run_worker() {
    local id=$1
    python3 -u - <<PYEOF &
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
for pid in "${PIDS[@]}"; do wait "$pid" 2>/dev/null || true; done

echo ""
echo "=== Aggregate admission throughput ==="
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
print(f"Aggregate: {total_mb:.2f}MB in ${DURATION}s, {total_mb/$DURATION:.3f} MB/s, {total_iops:.1f} iops")
PYEOF

echo ""
echo "=== MultiPatch (real drain) activity DURING the write window ==="
tail -n +"$LOGMARK" "$LOG/client.log" > /tmp/drainlag_during.log
DURING_COUNT=$(grep -c "MultiPatch:" /tmp/drainlag_during.log)
DURING_PATCHES=$(grep -oP '(?<=, )\d+(?= patches)' /tmp/drainlag_during.log | awk '{s+=$1} END {print s+0}')
echo "MultiPatch RPCs during write window: ${DURING_COUNT} (${DURING_PATCHES} total patches)"

echo ""
echo "=== Peak buffered/uncommitted bytes at end of write window (WBSTATS) ==="
PEAK=$(grep -oP '(?<=WBSTATS buffered=)\d+' /tmp/drainlag_during.log | sort -n | tail -1)
CAP=$(grep -oP '(?<=cap=)\d+' /tmp/drainlag_during.log | tail -1)
echo "peak_buffered=${PEAK:-0} cap=${CAP:-0}"

echo ""
echo "=== Trailing sync: time to reach full durability after write window ends ==="
T0=$(date +%s.%N)
sync "$MOUNT"
T1=$(date +%s.%N)
DRAIN_S=$(echo "$T1 - $T0" | bc)
echo "post_test_drain_s=${DRAIN_S}"
echo "total_time_to_durability_s=$(echo "$DURATION + $DRAIN_S" | bc) (${DURATION}s write window + drain)"

rm -f "$TESTFILE"
kill "$CLIENT_PID" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -f "dfs-server" 2>/dev/null || true
echo "Done. Logs in $LOG/"
