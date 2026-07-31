#!/bin/bash
# Reproduce the CUMULATIVE write slowdown found 2026-07-22 on server4/server5:
# each successive pass of the same workload, against the same still-mounted client,
# gets slower — random writes ~10x slower by the 2nd pass, sequential ~2x — and
# throughput snaps back to full speed the moment the client is restarted.
#
# Root cause this is built to detect: the flush drain was O(N^2) in the number of
# simultaneously-dirty chunks (N = InodeWriteBuffer::active_chunks). The background
# ticker's self-refill loop called flush_one_chunk once per chunk flushed, and each
# call re-ran up to three O(N) candidate scans, plus the loop itself added
# has_flushable_slot() and a bytes_now scan — ~5*O(N) per chunk, N chunks to drain,
# across up to PIPELINE_MAX_ITEMS concurrent tasks all scanning the same set. Wide
# random writes keep N large (hundreds-to-thousands), so the cost compounds and the
# backlog never fully clears between passes; a restart resets N to 0.
#
# The sharp fingerprint is NOT wall-clock alone (that's noisy) but CLIENT CPU SECONDS
# PER PASS for a FIXED amount of submitted work. Under an O(N^2) drain, CPU per pass
# climbs steeply pass-over-pass while bytes written stay constant. Under a linear
# drain it stays roughly flat.
#
# Usage: bash scripts/repro_cumulative_write_decay.sh [file_size_mb] [passes] [seconds_per_pass] [threads]

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-decay-mount
LOG=/tmp/dfs-decay-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
TESTFILE="$MOUNT/decay.img"

FILE_SIZE_MB=${1:-2048}
PASSES=${2:-4}
DURATION=${3:-15}
NUM_THREADS=${4:-8}

# A pass is a regression if it drops below this fraction of pass 1's throughput.
DECAY_FLOOR=0.60
# ...or if it burns more than this multiple of pass 1's client CPU for the same work.
CPU_CEILING=2.5

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

start_client() {
    # info level matches production (see feedback_repro_fidelity) — debug logging
    # is itself O(writes) and would mask the CPU signal we're trying to measure.
    "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
        --log-file "$LOG/client.log" --allow-other --log-level info &
    sleep 2
    mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
    CLIENT_PID=$(pgrep -f "dfs-client mount $MOUNT" | head -1)
}

# Total CPU seconds (utime+stime) consumed by the client process so far.
client_cpu_s() {
    local pid=$1
    awk '{print ($14 + $15) / '"$(getconf CLK_TCK)"'}' "/proc/$pid/stat" 2>/dev/null || echo 0
}

start_client
echo "Mounted. Client PID=$CLIENT_PID"

echo "=== Pre-allocating ${FILE_SIZE_MB}MB test file ==="
dd if=/dev/zero of="$TESTFILE" bs=1M count="$FILE_SIZE_MB" 2>/dev/null
sync "$MOUNT"
sleep 1

run_pass() {
    local label=$1
    local pass_log="$LOG/pass_${label}"
    rm -f "${pass_log}"_*.out

    local cpu_before wall_before
    cpu_before=$(client_cpu_s "$CLIENT_PID")
    wall_before=$(date +%s.%N)

    for id in $(seq 0 $(( NUM_THREADS - 1 ))); do
        python3 -u - > "${pass_log}_${id}.out" 2>&1 <<PYEOF &
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
print(f"{mb:.4f} {mb/elapsed:.4f} {n/elapsed:.2f}")
PYEOF
    done
    wait

    # Include the trailing drain: durability, not just admission into the buffer.
    sync "$MOUNT"
    local wall_after cpu_after
    wall_after=$(date +%s.%N)
    cpu_after=$(client_cpu_s "$CLIENT_PID")

    python3 - <<PYEOF
import glob
mb = 0.0
for fn in glob.glob("${pass_log}_*.out"):
    parts = open(fn).read().split()
    if len(parts) >= 3:
        mb += float(parts[0])
wall = $wall_after - $wall_before
cpu = $cpu_after - $cpu_before
with open("$LOG/results.tsv", "a") as f:
    f.write(f"$label\t{mb:.4f}\t{wall:.4f}\t{mb/wall:.4f}\t{cpu:.4f}\n")
print(f"  pass $label: {mb:.1f}MB in {wall:.2f}s to durability = {mb/wall:.3f} MB/s | client CPU {cpu:.2f}s")
PYEOF
}

rm -f "$LOG/results.tsv"
echo ""
echo "=== $PASSES passes x ${DURATION}s x ${NUM_THREADS} threads, NO restart between passes ==="
for p in $(seq 1 "$PASSES"); do
    run_pass "$p"
done

echo ""
echo "=== Control: restart client, then repeat the same pass ==="
pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 1
start_client
echo "  Client restarted, PID=$CLIENT_PID"
sleep 1
run_pass "restart"

echo ""
echo "=== Verdict ==="
python3 - <<PYEOF
rows = []
for line in open("$LOG/results.tsv"):
    label, mb, wall, mbps, cpu = line.split("\t")
    rows.append((label, float(mb), float(wall), float(mbps), float(cpu)))

passes = [r for r in rows if r[0] != "restart"]
restart = next((r for r in rows if r[0] == "restart"), None)
first = passes[0]
last = passes[-1]

print(f"{'pass':>8} {'MB':>9} {'MB/s':>9} {'CPU s':>8} {'vs pass1':>9} {'CPU vs p1':>10}")
for label, mb, wall, mbps, cpu in rows:
    ratio = mbps / first[3] if first[3] else 0
    cpu_ratio = cpu / first[4] if first[4] else 0
    print(f"{label:>8} {mb:9.1f} {mbps:9.3f} {cpu:8.2f} {ratio:8.2f}x {cpu_ratio:9.2f}x")

fail = False
decay = last[3] / first[3] if first[3] else 0
print()
print(f"throughput pass{last[0]} / pass1 = {decay:.2f}x (floor {$DECAY_FLOOR})")
if decay < $DECAY_FLOOR:
    print(f"  FAIL: cumulative throughput decay across passes")
    fail = True
else:
    print(f"  PASS: no severe cumulative throughput decay")

cpu_growth = last[4] / first[4] if first[4] else 0
print(f"client CPU pass{last[0]} / pass1 = {cpu_growth:.2f}x for equal work (ceiling {$CPU_CEILING})")
if cpu_growth > $CPU_CEILING:
    print(f"  FAIL: client CPU per unit of work grows across passes (superlinear drain)")
    fail = True
else:
    print(f"  PASS: client CPU per unit of work stays bounded")

if restart:
    # The tell-tale: if a restart is much faster than the pass right before it,
    # the slowdown lived in per-process accumulated state, not the cluster/disk.
    recov = restart[3] / last[3] if last[3] else 0
    print(f"restart / pass{last[0]} = {recov:.2f}x")
    if recov > 1.5 and decay < $DECAY_FLOOR:
        print(f"  (restart recovers throughput — confirms client-side accumulated state)")

print()
print("RESULT: " + ("FAIL" if fail else "PASS"))
PYEOF

echo ""
echo "=== Spin-warning count (self-refill loop safety valve) ==="
SPINS=$(grep -c "self-refill loop" "$LOG/client.log" 2>/dev/null || echo 0)
echo "self-refill spin warnings: $SPINS (expect 0; 495 were seen live on server4 while degraded)"

echo ""
echo "Logs: $LOG   Results: $LOG/results.tsv"
cleanup_all
