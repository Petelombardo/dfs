#!/bin/bash
# Memory-growth repro: compares dfs-server RSS behavior under a large sequential
# write vs. a large volume of small in-place patches to a similarly-sized file.
#
# Motivated by the 2026-07-18 staging incident: gluster1 nearly OOM'd (3.8GB SBC,
# down to ~50MB available) during a ~90-minute VM install, then gluster2 was
# actually OOM-killed (anon-rss 2.98GB) during a VM backup restore — both heavy,
# sustained-write workloads. User's hypothesis: the leak (if real) is in the
# patch/fold path specifically, not plain sequential writes.
#
# Usage: ./scripts/repro_patch_memory_growth.sh
#
# Samples RSS of all 5 local dfs-server processes every 2s throughout:
#   Phase A: SEQ_SIZE_MB sequential write (one pass, periodic fsync), then a
#            settle period — does RSS plateau/drop, or keep climbing after
#            writes stop?
#   Phase B: write a file of the same total size once (unmeasured, to establish
#            it), then apply NUM_PATCHES small in-place patches on top of it
#            (measured), then the same settle period.
#
# Output: CSV per phase at $LOG/rss_seq.csv and $LOG/rss_patch.csv
# (columns: elapsed_s,node,rss_kb), plus a printed summary comparing peak RSS,
# end-of-write RSS, and post-settle RSS for both phases.
set -u

REPO=/builds/dfs
BIN=$REPO/target/release
LOG=/tmp/dfs-test-logs/repro-patch-memgrowth
MOUNT=/tmp/dfs-mount
CLUSTER=127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904

SEQ_SIZE_MB=${SEQ_SIZE_MB:-50}
PATCH_FILE_SIZE_MB=${PATCH_FILE_SIZE_MB:-50}
NUM_PATCHES=${NUM_PATCHES:-3000}
PATCH_MIN_BYTES=${PATCH_MIN_BYTES:-4096}
PATCH_MAX_BYTES=${PATCH_MAX_BYTES:-65536}
SETTLE_SECS=${SETTLE_SECS:-90}
SAMPLE_INTERVAL=${SAMPLE_INTERVAL:-2}

rm -rf "$LOG"
mkdir -p "$LOG"

echo "=== Setting up 5-node cluster ==="
bash "$REPO/scripts/setup-cluster.sh" 5 > "$LOG/init.log" 2>&1 || { echo "init failed, see $LOG/init.log"; exit 1; }

declare -a PIDS
for i in 1 2 3 4 5; do
    RUST_LOG=info "$BIN/dfs-server" start --config /tmp/dfs-test/node$i/config.toml \
        > "$LOG/server$i.log" 2>&1 &
    PIDS[$i]=$!
done
echo "server PIDs: ${PIDS[1]} ${PIDS[2]} ${PIDS[3]} ${PIDS[4]} ${PIDS[5]}"
sleep 4

RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level info &
CLIENT_PID=$!
sleep 2
mountpoint -q "$MOUNT" || { echo "mount failed"; exit 1; }
echo "mounted, client PID $CLIENT_PID"

sample_rss() { # csv_path duration_secs
    local csv=$1 dur=$2
    local t0=$(date +%s)
    while :; do
        local now=$(date +%s)
        local elapsed=$((now - t0))
        [ "$elapsed" -ge "$dur" ] && break
        for i in 1 2 3 4 5; do
            local pid=${PIDS[$i]}
            local rss=$(awk '/VmRSS/{print $2}' /proc/$pid/status 2>/dev/null)
            [ -n "$rss" ] && echo "$elapsed,node$i,$rss" >> "$csv"
        done
        sleep "$SAMPLE_INTERVAL"
    done
}

summarize() { # csv_path label
    local csv=$1 label=$2
    echo "--- $label ---"
    for i in 1 2 3 4 5; do
        python3 - "$csv" "node$i" <<'PYEOF'
import sys, csv as csvmod
path, node = sys.argv[1], sys.argv[2]
rows = []
try:
    with open(path) as f:
        for line in f:
            e, n, r = line.strip().split(",")
            if n == node:
                rows.append((int(e), int(r)))
except FileNotFoundError:
    pass
if not rows:
    print(f"  {node}: no samples")
else:
    start_rss = rows[0][1]
    peak = max(r for _, r in rows)
    end = rows[-1][1]
    print(f"  {node}: start={start_rss//1024}MB peak={peak//1024}MB end_of_window={end//1024}MB delta_start_to_end={(end-start_rss)//1024}MB")
PYEOF
    done
}

# ── Phase A: sequential write ────────────────────────────────────────────
echo ""
echo "=== Phase A: sequential write (${SEQ_SIZE_MB}MB, one pass) ==="
SEQ_CSV="$LOG/rss_seq.csv"
: > "$SEQ_CSV"
SEQ_FILE="$MOUNT/memgrowth-seq.bin"
rm -f "$SEQ_FILE"

# background sampler covers the write + settle window
WRITE_PLUS_SETTLE_GUESS=$((SEQ_SIZE_MB / 20 + SETTLE_SECS + 30))
sample_rss "$SEQ_CSV" "$WRITE_PLUS_SETTLE_GUESS" &
SAMPLER_PID=$!

MOUNT="$MOUNT" SEQ_FILE="$SEQ_FILE" SEQ_SIZE_MB="$SEQ_SIZE_MB" python3 - <<'PYEOF'
import os, time
mount = os.environ['MOUNT']
seq_file = os.environ['SEQ_FILE']
size_mb = int(os.environ['SEQ_SIZE_MB'])
MB = 1024 * 1024
buf = os.urandom(4 * MB)
t0 = time.time()
with open(seq_file, 'wb') as f:
    written = 0
    while written < size_mb * MB:
        f.write(buf)
        written += len(buf)
        if written % (64 * MB) == 0:
            f.flush()
            os.fsync(f.fileno())
    f.flush()
    os.fsync(f.fileno())
print(f"Phase A write done: {written//MB}MB in {time.time()-t0:.1f}s")
PYEOF
sync "$MOUNT"
echo "Phase A: write complete, settling for ${SETTLE_SECS}s..."
sleep "$SETTLE_SECS"
kill "$SAMPLER_PID" 2>/dev/null
wait "$SAMPLER_PID" 2>/dev/null
summarize "$SEQ_CSV" "Phase A: sequential write"

# ── Phase B: heavy patch writes ──────────────────────────────────────────
echo ""
echo "=== Phase B: establish ${PATCH_FILE_SIZE_MB}MB file, then ${NUM_PATCHES} small patches ==="
PATCH_FILE="$MOUNT/memgrowth-patch.bin"
rm -f "$PATCH_FILE"
MOUNT="$MOUNT" PATCH_FILE="$PATCH_FILE" PATCH_FILE_SIZE_MB="$PATCH_FILE_SIZE_MB" python3 - <<'PYEOF'
import os
mount = os.environ['MOUNT']
patch_file = os.environ['PATCH_FILE']
size_mb = int(os.environ['PATCH_FILE_SIZE_MB'])
MB = 1024 * 1024
buf = os.urandom(4 * MB)
with open(patch_file, 'wb') as f:
    written = 0
    while written < size_mb * MB:
        f.write(buf)
        written += len(buf)
    f.flush()
    os.fsync(f.fileno())
print(f"Phase B base file established: {written//MB}MB")
PYEOF
sync "$MOUNT"
echo "Phase B: base file established, starting patch storm..."

PATCH_CSV="$LOG/rss_patch.csv"
: > "$PATCH_CSV"
PATCH_PLUS_SETTLE_GUESS=$((NUM_PATCHES / 200 + SETTLE_SECS + 60))
sample_rss "$PATCH_CSV" "$PATCH_PLUS_SETTLE_GUESS" &
SAMPLER_PID=$!

MOUNT="$MOUNT" PATCH_FILE="$PATCH_FILE" PATCH_FILE_SIZE_MB="$PATCH_FILE_SIZE_MB" \
NUM_PATCHES="$NUM_PATCHES" PATCH_MIN_BYTES="$PATCH_MIN_BYTES" PATCH_MAX_BYTES="$PATCH_MAX_BYTES" \
python3 - <<'PYEOF'
import os, random, time
mount = os.environ['MOUNT']
patch_file = os.environ['PATCH_FILE']
size_mb = int(os.environ['PATCH_FILE_SIZE_MB'])
num_patches = int(os.environ['NUM_PATCHES'])
pmin = int(os.environ['PATCH_MIN_BYTES'])
pmax = int(os.environ['PATCH_MAX_BYTES'])
MB = 1024 * 1024
size = size_mb * MB

random.seed(7)
fd = os.open(patch_file, os.O_RDWR)
t0 = time.time()
total_bytes = 0
for i in range(num_patches):
    sz = random.randint(pmin, pmax)
    off = random.randint(0, size - sz)
    data = os.urandom(sz)
    os.pwrite(fd, data, off)
    total_bytes += sz
    if i % 500 == 0:
        os.fsync(fd)
os.fsync(fd)
os.close(fd)
print(f"Phase B patch storm done: {num_patches} patches, {total_bytes//MB}MB total, {time.time()-t0:.1f}s")
PYEOF
sync "$MOUNT"
echo "Phase B: patch storm complete, settling for ${SETTLE_SECS}s..."
sleep "$SETTLE_SECS"
kill "$SAMPLER_PID" 2>/dev/null
wait "$SAMPLER_PID" 2>/dev/null
summarize "$PATCH_CSV" "Phase B: heavy patch writes"

echo ""
echo "=== Comparison ==="
echo "CSVs: $SEQ_CSV  $PATCH_CSV"
echo "(peak - start) and (end_of_window - start) per node, both phases, printed above."
echo "If Phase B's deltas are much larger than Phase A's for the same total data volume,"
echo "or Phase B fails to plateau/drop during its settle window while Phase A does,"
echo "that points at the patch/fold path specifically."

echo ""
echo "=== Cleanup ==="
kill "$CLIENT_PID" 2>/dev/null
sleep 1
fusermount -u "$MOUNT" 2>/dev/null
for i in 1 2 3 4 5; do
    kill "${PIDS[$i]}" 2>/dev/null
done
echo "Logs preserved in $LOG"
