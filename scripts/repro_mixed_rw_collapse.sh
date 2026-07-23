#!/bin/bash
# Minimal reproduction of the mixed read/write collapse seen in kdiskmark's
# R70%/W30% column (2026-07-23, server4 / VM-100):
#
#              Read     Write    Mix      blend prediction   actual vs pred
#   SEQ1M     163.77    83.29    38.96      139.6             3.6x worse
#   RND4K       7.60    12.07     0.27        8.94           33x worse
#   RND4K IOPS  1900     3019       67        2236           33x worse
#
# The mix number sitting BELOW both pure numbers is the finding: reads alone
# are fine, writes alone are fine, only the interaction is pathological.
# 67 IOPS on random 4K is ~15ms/op — a serialization limit, not bandwidth.
#
# Staging forensics already ruled out the server: APTIMING on gluster1-5 for
# the benchmark file was a flat ~170-210ms median (write stage 115-150ms) in
# EVERY phase, mixed or not. That's the known open apply_patch tax, uniform
# across phases, so it cannot be what makes mixed specifically collapse.
# That points the finger at the client's read path when a writer is live —
# fuse_impl.rs:5821-5971, which has three serialization points that exist
# ONLY when write_buffers holds a slot for the chunk being read:
#   1. :5830  full `shard.lock().await` — read waits on the writer's flush
#   2. :5938  `slot.flushing` 5ms-granularity poll loop (up to 5s)
#   3. :5893  slot-overlap splice — forces a network read even though the
#             bytes are already in local RAM
#
# This script is deliberately the SMALLEST thing that separates those from
# ordinary load: same file, same access pattern, same op size, three phases,
# one variable changed (is there a concurrent writer or not).
#
#   Phase A: 1 reader alone       -> baseline read IOPS
#   Phase B: 1 writer alone       -> baseline write IOPS
#   Phase C: 1 reader + 1 writer  -> mixed; reader IOPS is the number that matters
#
# A healthy system gives phase-C reader IOPS within ~2x of phase A. The bug
# reproduces if phase-C reader IOPS collapses by an order of magnitude.
#
# Two things this script gets right that a naive version does not:
#
#   O_DIRECT. The first cut used ordinary buffered pread() and measured
#   218,019 IOPS at 851 MB/s / p50=0.00ms — the kernel page cache, having just
#   written the file. Not one read reached FUSE. kdiskmark uses O_DIRECT and
#   QEMU runs these disks cache=none, so O_DIRECT is what the real number was
#   measured through. Without it this script measures nothing.
#
#   Cache ratio, not cache size. server4's client log for the real run:
#     "Chunk cache sizing: 11874 MB available, target 18%, using 128 chunks (512 MB)"
#     "Write buffer cap sizing: ... -> 1024 MB"
#   against a 1 GiB (256-chunk) working set — so the chunk cache held exactly
#   50% of it, and the write buffer 1.0x. Dev has nowhere near the RAM for a
#   1 GiB file, but the absolute sizes were never the point: what governs how
#   often a read has to leave the client is the working-set-to-cache RATIO.
#   So we shrink the caches to preserve 50% / 1.0x at whatever file size dev
#   can afford, via DFS_MAX_CACHE_CHUNKS and DFS_WRITE_BUFFER_CAP_MB.
#
# Usage: bash scripts/repro_mixed_rw_collapse.sh [file_size_mb] [duration_sec]

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-mixed-mount
LOG=/tmp/dfs-mixed-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
TESTFILE="$MOUNT/mixed.img"

FILE_SIZE_MB=${1:-128}   # 32 chunks at 4MB — enough that random 4K lands on a
                         # different chunk almost every op, like the real 1GiB run.
                         # Was 256; halved after a 256MB run took the dev box down
                         # (see the SAFETY RAILS block below).
DURATION=${2:-20}

# ---------------------------------------------------------------- SAFETY RAILS
# A 256MB run hard-locked this machine on 2026-07-23 — no ping, no Num Lock LED,
# hard power cycle required. The box has 7.9GB RAM and NO SWAP, and the root fs
# was at 97% (1006MB free) when the run started. On a swapless box, heavy dirty
# writeback into a nearly-full filesystem puts the kernel into direct reclaim it
# can't escape: userspace stops, then so does everything else. The OOM killer
# never gets a clean shot, which is why nothing was killed and the console died.
#
# The workload amplifies far more than its nominal size: a 128MB working set
# becomes ~1.7GB of cluster data across 5 nodes once replicas and the patch
# deltas from a few hundred thousand random 4K writes are counted (measured
# ~13x for a 256MB file -> 3.4GB). So the footprint must be checked against
# actual free space BEFORE starting, not assumed.
#
# Two rails, because a pre-flight check alone can't see growth during the run:
#   1. pre-flight: refuse to start without real headroom in disk AND memory
#   2. watchdog:   1s poll that kills the cluster the moment either floor is
#                  crossed, so the test dies instead of the machine
# cgroups aren't available here (no systemd as PID 1, no cgroup v2), so the
# watchdog is the enforcement mechanism, not a nicety.
DISK_FLOOR_MB=${DISK_FLOOR_MB:-1500}   # abort if free disk drops below this
MEM_FLOOR_MB=${MEM_FLOOR_MB:-1200}     # abort if MemAvailable drops below this
EST_FOOTPRINT_MB=$(( FILE_SIZE_MB * 15 ))

free_disk_mb() { df -Pm / | awk 'NR==2{print $4}'; }
free_mem_mb()  { awk '/^MemAvailable:/{print int($2/1024)}' /proc/meminfo; }

echo "=== Pre-flight ==="
D=$(free_disk_mb); M=$(free_mem_mb)
echo "  free disk: ${D}MB   free mem: ${M}MB   (no swap: $(swapon --show --noheadings 2>/dev/null | wc -l) devices)"
echo "  this run needs roughly ${EST_FOOTPRINT_MB}MB of disk (${FILE_SIZE_MB}MB working set x ~15 amplification)"
NEED_DISK=$(( EST_FOOTPRINT_MB + DISK_FLOOR_MB ))
if [ "$D" -lt "$NEED_DISK" ]; then
    echo "  ABORT: need ${NEED_DISK}MB free disk (footprint + ${DISK_FLOOR_MB}MB floor), have ${D}MB."
    echo "  Free space or lower FILE_SIZE_MB. Do NOT run this near a full disk — it locks the box."
    exit 1
fi
if [ "$M" -lt 2500 ]; then
    echo "  ABORT: need 2500MB MemAvailable to start, have ${M}MB."
    exit 1
fi
echo "  OK."

# Preserve server4's ratios (see header): chunk cache = 50% of working set,
# write-buffer cap = 1.0x working set. Overridable for sweeping the ratio.
CHUNKS_IN_FILE=$(( FILE_SIZE_MB / 4 ))
export DFS_MAX_CACHE_CHUNKS=${DFS_MAX_CACHE_CHUNKS:-$(( CHUNKS_IN_FILE / 2 ))}
export DFS_WRITE_BUFFER_CAP_MB=${DFS_WRITE_BUFFER_CAP_MB:-$FILE_SIZE_MB}

# REQUIRED on dev, and the first run here died without it. Each dfs-server sizes
# its cache budget from calculate_server_cache_budget_mb(), which reads TOTAL
# system RAM and takes 18% — with no knowledge that 4 siblings are sharing the
# same box. On this 7.9GB dev machine that is 1427MB each, 7.1GB across 5 nodes,
# and phase C OOM-killed two of them (anon-rss 1.7GB and 2.6GB, confirmed in
# dmesg) which silently contaminated the read-latency measurement with dead
# replicas. In staging each node is alone on its hardware so 18% is fine there.
# Scaled here to keep staging's per-node cache-to-working-set ratio (~0.67x):
# 2% of 7.9GB = ~158MB per node against a 256MB working set.
export DFS_SERVER_CACHE_BUDGET_PERCENT=${DFS_SERVER_CACHE_BUDGET_PERCENT:-2}

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
SERVER_PIDS=()
for i in 1 2 3 4 5; do
    RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        > "$LOG/server${i}.log" 2>&1 &
    SERVER_PIDS+=($!)
done
sleep 3

echo "Cache tuning: DFS_MAX_CACHE_CHUNKS=$DFS_MAX_CACHE_CHUNKS (file is $CHUNKS_IN_FILE chunks)," \
     "DFS_WRITE_BUFFER_CAP_MB=$DFS_WRITE_BUFFER_CAP_MB," \
     "DFS_SERVER_CACHE_BUDGET_PERCENT=$DFS_SERVER_CACHE_BUDGET_PERCENT"
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level info &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
CLIENT_PID=$(pgrep -f "dfs-client mount $MOUNT" | head -1)
echo "Mounted. Client PID=$CLIENT_PID"

# Watchdog: the thing that stands between this test and another power cycle.
# Polls every second and SIGKILLs the whole cluster the instant free disk or
# MemAvailable crosses a floor. Kills by recorded PID, never `pkill -f` — that
# can match its own command line and take out the shell before reaching the
# real target. Deliberately kill -9 and deliberately no grace period: by the
# time these floors are hit, a swapless box is already minutes from livelock and
# a polite shutdown is exactly what won't complete.
WATCHDOG_TRIPPED="$LOG/WATCHDOG_TRIPPED"
rm -f "$WATCHDOG_TRIPPED"
(
    while :; do
        sleep 1
        d=$(free_disk_mb); m=$(free_mem_mb)
        if [ "$d" -lt "$DISK_FLOOR_MB" ] || [ "$m" -lt "$MEM_FLOOR_MB" ]; then
            echo "disk=${d}MB mem=${m}MB (floors: disk=${DISK_FLOOR_MB}MB mem=${MEM_FLOOR_MB}MB)" > "$WATCHDOG_TRIPPED"
            kill -9 "${SERVER_PIDS[@]}" "$CLIENT_PID" 2>/dev/null
            exit 0
        fi
    done
) &
WATCHDOG_PID=$!
echo "Watchdog armed (PID=$WATCHDOG_PID): aborts below ${DISK_FLOOR_MB}MB disk / ${MEM_FLOOR_MB}MB mem."

# Any exit path — success, error, or Ctrl-C — must stop the watchdog and the
# cluster, or a killed script leaves 5 servers writing to a nearly-full disk.
trap 'kill "$WATCHDOG_PID" 2>/dev/null; kill "${SERVER_PIDS[@]}" "$CLIENT_PID" 2>/dev/null; fusermount -u "$MOUNT" 2>/dev/null; exit 130' INT TERM

echo "=== Pre-allocating ${FILE_SIZE_MB}MB test file ==="
dd if=/dev/urandom of="$TESTFILE" bs=1M count="$FILE_SIZE_MB" 2>/dev/null
sync "$MOUNT"
# Belt-and-braces alongside O_DIRECT: the pre-allocation just pulled the whole
# file through the page cache, and a warm page cache is exactly what made the
# first version of this script report 218k IOPS.
sync; echo 3 > /proc/sys/vm/drop_caches 2>/dev/null || true
sleep 1
echo "Committed."

# 4K ops at uniformly random 4K-aligned offsets over the whole file, O_DIRECT
# so every op actually reaches FUSE — matching kdiskmark RND4K Q1T1 against a
# cache=none QEMU disk. O_DIRECT needs the buffer itself memory-aligned, hence
# mmap rather than a plain bytes object.
# Sets WORKER_PID. Callers MUST `wait "$WORKER_PID"` explicitly and never bare
# `wait` — the 5 dfs-servers and the dfs-client are background jobs of this same
# shell and never exit, so a bare `wait` hangs the script forever (it did).
worker() {
    local mode=$1 out=$2
    python3 - <<PYEOF > "$out" 2>&1 &
import os, random, time, mmap, sys

BS = 4096
try:
    fd = os.open("$TESTFILE", os.O_RDWR | os.O_DIRECT)
except OSError as e:
    print(f"$mode: O_DIRECT open failed ({e}) — refusing to measure through the "
          f"page cache, which would report a fictitious number")
    sys.exit(1)

size = $FILE_SIZE_MB * 1024 * 1024
wbuf = mmap.mmap(-1, BS); wbuf.write(b'\xa5' * BS)
rbuf = mmap.mmap(-1, BS)
n = 0
errs = 0
lat = []
start = time.time(); end = start + $DURATION
# Do NOT abort on EIO. A concurrent-writer run makes reads fail outright, and
# dying on the first one throws away the measurement AND the error rate — the
# two numbers we're here for. Count them and keep going, like an application
# with retries would.
try:
    while time.time() < end:
        off = random.randrange(0, size - BS, BS)
        t0 = time.time()
        try:
            if "$mode" == "read":
                os.preadv(fd, [rbuf], off)
            else:
                os.pwritev(fd, [wbuf], off)
            lat.append(time.time() - t0)
            n += 1
        except OSError:
            errs += 1
finally:
    os.close(fd)
el = time.time() - start
lat.sort()
pct = lambda q: lat[min(int(len(lat)*q), len(lat)-1)]*1000 if lat else 0
tot = n + errs
print(f"$mode: {n} ops, {n/el:.1f} iops, {(n*BS)/(1024*1024)/el:.3f} MB/s, "
      f"p50={pct(.5):.2f}ms p95={pct(.95):.2f}ms max={pct(1.0):.1f}ms, "
      f"errors={errs} ({100.0*errs/tot if tot else 0:.2f}%)")
PYEOF
    WORKER_PID=$!
}

echo ""
echo "=== Phase A: reader alone (${DURATION}s) ==="
worker read "$LOG/A_read.out"; wait "$WORKER_PID"
cat "$LOG/A_read.out"

echo ""
echo "=== Phase B: writer alone (${DURATION}s) ==="
worker write "$LOG/B_write.out"; wait "$WORKER_PID"
sync "$MOUNT"
cat "$LOG/B_write.out"

echo ""
echo "=== Phase C: reader + writer concurrently (${DURATION}s) ==="
worker read  "$LOG/C_read.out";  C_READ_PID=$WORKER_PID
worker write "$LOG/C_write.out"; C_WRITE_PID=$WORKER_PID
wait "$C_READ_PID"; wait "$C_WRITE_PID"
sync "$MOUNT"

if [ -f "$WATCHDOG_TRIPPED" ]; then
    echo ""
    echo "!!! WATCHDOG TRIPPED — run aborted to protect the machine:"
    echo "!!!   $(cat "$WATCHDOG_TRIPPED")"
    echo "!!! Results below are meaningless. Free resources or lower FILE_SIZE_MB."
    kill "$WATCHDOG_PID" 2>/dev/null || true
    rm -rf "$BASE" "$MOUNT" 2>/dev/null || true
    exit 1
fi

# A node that died mid-phase makes reads stall on a dead replica, which looks
# exactly like the bug we're hunting. Never report those numbers as a result.
ALIVE=$(pgrep -cf "dfs-server start --config $BASE/node")
if [ "$ALIVE" -ne 5 ]; then
    echo ""
    echo "!!! INVALID RUN: only $ALIVE/5 servers alive at end of phase C."
    echo "!!! Read latencies below are contaminated by dead replicas, not the bug."
    dmesg -T 2>/dev/null | grep -i "killed process.*dfs-server" | tail -5
    echo "!!! Lower DFS_SERVER_CACHE_BUDGET_PERCENT (currently $DFS_SERVER_CACHE_BUDGET_PERCENT) or FILE_SIZE_MB and re-run."
fi
cat "$LOG/C_read.out" "$LOG/C_write.out"

echo ""
echo "=== VERDICT ==="
python3 - <<PYEOF
import re
def g(fn):
    s = open(fn).read()
    m = re.search(r'([\d.]+) iops.*p50=([\d.]+)ms p95=([\d.]+)ms max=([\d.]+)ms, errors=(\d+) \(([\d.]+)%\)', s)
    if not m:
        raise SystemExit(f"could not parse {fn}:\n{s}")
    return tuple(float(m.group(i)) for i in range(1, 7))
a, b, cr, cw = g("$LOG/A_read.out"), g("$LOG/B_write.out"), g("$LOG/C_read.out"), g("$LOG/C_write.out")
for name, v in (("read  alone", a), ("write alone", b), ("read  mixed", cr), ("write mixed", cw)):
    print(f"  {name} : {v[0]:8.1f} iops  p50={v[1]:6.2f}ms p95={v[2]:7.2f}ms "
          f"max={v[3]:8.1f}ms  errors={int(v[4])} ({v[5]:.2f}%)")
print()
print(f"  READ  degradation under concurrent writer: {a[0]/cr[0]:6.1f}x")
print(f"  WRITE degradation under concurrent reader: {b[0]/cw[0]:6.1f}x")
print(f"  mixed aggregate {cr[0]+cw[0]:.1f} iops vs 0.7*read+0.3*write blend {0.7*a[0]+0.3*b[0]:.1f} iops")
if cr[4] or cw[4]:
    print(f"\n  >>> I/O ERRORS under concurrent access: {int(cr[4])} read, {int(cw[4])} write.")
    print("      A read whose chunk map still points at a chunk_id the writer just")
    print("      replaced fails outright — see 'Failed to open chunk file' in client.log.")
if a[0]/cr[0] > 5 or b[0]/cw[0] > 5:
    print("\n  >>> REPRODUCED: order-of-magnitude mixed-mode collapse.")
elif not (cr[4] or cw[4]):
    print("\n  >>> not reproduced at this scale.")
PYEOF

echo ""
echo "=== Client-side signals during phase C ==="
grep -c "LOCKTIMING" "$LOG/client.log" 2>/dev/null | sed 's/^/  LOCKTIMING lines: /'
grep -c "MPTIMING"   "$LOG/client.log" 2>/dev/null | sed 's/^/  MPTIMING lines:   /'
echo "  Full logs: $LOG/"

echo ""
echo "=== Cleanup ==="
kill "$WATCHDOG_PID" 2>/dev/null || true
kill "$CLIENT_PID" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -f "dfs-server" 2>/dev/null || true
# Reclaim the cluster data. A ${FILE_SIZE_MB}MB working set expands to several GB
# across 5 nodes once replicas and the patch deltas this workload generates are
# counted (measured 2.6GB for a 256MB file), and dev has little headroom — left
# behind, successive runs fill the disk. Logs are kept; they're small and are the
# point of the exercise.
rm -rf "$BASE" "$MOUNT" 2>/dev/null || true
echo "Done. Free space: $(df -h / | awk 'NR==2{print $4}'). Logs kept in $LOG/"
