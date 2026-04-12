#!/bin/bash
# Performance benchmark: sequential write, DVR-like fsync pattern, small-write fsync pattern
# Usage: ./scripts/perf-test.sh <mount_point> <label>
# Run once on main, once on the branch, compare output.

MOUNT=${1:-/tmp/dfs-mount}
LABEL=${2:-"unlabeled"}
RESULTS=/tmp/perf-results-${LABEL}.txt
ADMIN=./target/release/dfs-admin

echo "=== DFS Performance Test: ${LABEL} ===" | tee $RESULTS
echo "Mount: $MOUNT" | tee -a $RESULTS
echo "Date: $(date)" | tee -a $RESULTS
echo "" | tee -a $RESULTS

rm -f $MOUNT/perf-*.bin

# -----------------------------------------------------------------------
# Test 1: Pure sequential write (no fsync) — best-case throughput
# -----------------------------------------------------------------------
echo "--- Test 1: Sequential write 200MB (no fsync) ---" | tee -a $RESULTS
dd if=/dev/urandom of=$MOUNT/perf-seq.bin bs=1M count=200 2>&1 | tee -a $RESULTS
echo "" | tee -a $RESULTS

# -----------------------------------------------------------------------
# Test 2: Sequential write with fsync every 4MB (chunk-aligned — ideal)
# -----------------------------------------------------------------------
echo "--- Test 2: Sequential write 200MB, fsync every 4MB (aligned) ---" | tee -a $RESULTS
DFS_MOUNT=$MOUNT python3 - <<'PYEOF' | tee -a $RESULTS
import os, time
mount = os.environ['DFS_MOUNT']
path = mount + '/perf-fsync-aligned.bin'
chunk = 4 * 1024 * 1024
total = 200 * 1024 * 1024
written = 0
data = os.urandom(chunk)
start = time.time()
with open(path, 'wb') as f:
    while written < total:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())
        written += chunk
elapsed = time.time() - start
mb = total / (1024*1024)
fsyncs = total // chunk
print(f"  {mb:.0f}MB in {elapsed:.2f}s = {mb/elapsed:.1f} MB/s  ({fsyncs} fsyncs, one per 4MB chunk)")
PYEOF
echo "" | tee -a $RESULTS

# -----------------------------------------------------------------------
# Test 3: DVR-like — write 188KB every 100ms, fsync every 2s
# HDHomeRun DVR: ~3.5 MB/s MPEG-TS stream, periodic fsyncs
# Simulates 20 seconds of recording (~70MB)
# -----------------------------------------------------------------------
echo "--- Test 3: DVR-like ~3.5MB/s, fsync every 2s (20s sim) ---" | tee -a $RESULTS
DFS_MOUNT=$MOUNT python3 - <<'PYEOF' | tee -a $RESULTS
import os, time
mount = os.environ['DFS_MOUNT']
path = mount + '/perf-dvr.bin'
write_chunk = 188 * 1024   # 188KB per write
write_interval = 0.1        # 100ms between writes
fsync_every = 20            # fsync every 20 writes = every ~2s
total_writes = 200          # 20 seconds of simulation
written = 0
fsyncs = 0
data = os.urandom(write_chunk)
start = time.time()
with open(path, 'wb') as f:
    for i in range(total_writes):
        t0 = time.time()
        f.write(data)
        written += write_chunk
        if (i + 1) % fsync_every == 0:
            f.flush()
            os.fsync(f.fileno())
            fsyncs += 1
        # Pace writes to simulate real DVR timing
        elapsed_write = time.time() - t0
        sleep = write_interval - elapsed_write
        if sleep > 0:
            time.sleep(sleep)
elapsed = time.time() - start
mb = written / (1024*1024)
print(f"  {mb:.1f}MB in {elapsed:.1f}s = {mb/elapsed:.2f} MB/s  ({fsyncs} fsyncs)")
PYEOF
echo "" | tee -a $RESULTS

# -----------------------------------------------------------------------
# Test 4: Pathological — 100KB write + fsync, repeated 100 times
# Worst case for partial-tail read-back amplification
# -----------------------------------------------------------------------
echo "--- Test 4: Pathological small-write fsync (100KB + fsync x100) ---" | tee -a $RESULTS
DFS_MOUNT=$MOUNT python3 - <<'PYEOF' | tee -a $RESULTS
import os, time
mount = os.environ['DFS_MOUNT']
path = mount + '/perf-small-fsync.bin'
write_chunk = 100 * 1024
iterations = 100
data = os.urandom(write_chunk)
start = time.time()
with open(path, 'wb') as f:
    for i in range(iterations):
        f.write(data)
        f.flush()
        os.fsync(f.fileno())
elapsed = time.time() - start
total_mb = (write_chunk * iterations) / (1024*1024)
print(f"  {total_mb:.1f}MB in {elapsed:.2f}s = {total_mb/elapsed:.1f} MB/s  ({iterations} fsyncs)")
print(f"  Per-fsync latency: {elapsed/iterations*1000:.1f}ms avg")
PYEOF
echo "" | tee -a $RESULTS

# -----------------------------------------------------------------------
# Chunk shape check — how fragmented are the files?
# -----------------------------------------------------------------------
echo "--- Chunk counts per file ---" | tee -a $RESULTS
for f in perf-seq.bin perf-fsync-aligned.bin perf-dvr.bin perf-small-fsync.bin; do
    INFO=$($ADMIN --cluster 127.0.0.1:8900 file info /$f 2>&1)
    SIZE=$(echo "$INFO" | grep "^Size:" | awk '{print $2}')
    CHUNKS=$(echo "$INFO" | grep "^Chunks:" | awk '{print $2}')
    echo "  $f: ${SIZE} bytes, ${CHUNKS} chunks" | tee -a $RESULTS
done
echo "" | tee -a $RESULTS

echo "--- Chunk node distribution (perf-seq.bin) ---" | tee -a $RESULTS
$ADMIN --cluster 127.0.0.1:8900 file info /perf-seq.bin 2>&1 \
    | grep -E "^[a-f0-9]{16}" | awk '{print $3, $4}' | sort | uniq -c | tee -a $RESULTS
echo "" | tee -a $RESULTS

echo "Results saved to: $RESULTS"
