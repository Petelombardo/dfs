#!/bin/bash
# Raw-disk write+read benchmark: sequential write, whole-chunk overwrite, multi-patch,
# sequential read, random read.
#
# Usage: ./scripts/raw_disk_bench.sh [mount_point] [label]
#   mount_point  default: current directory. Run from a NON-DFS location, e.g.:
#                   ~/dfs/scripts/raw_disk_bench.sh /mnt/test
#                 The script restarts dfs-client mid-run to clear the in-memory
#                 LRU cache before the read tests, so the script file itself must
#                 NOT be on the DFS mount.
#   label        default: timestamp; recorded alongside results for tracking
#
# Sequence:
#   T1  Sequential write  (32MB new file, 1 fsync at end)
#   T2  Whole-chunk write (8x4MB chunks, fsync/op — is_full_replacement path)
#   T3  Multi-patch write (50 ops 1KB-1MB, fsync/op — PatchChunk RPC path)
#       [pre-fill T4/T5 read test files — NOT timed]
#       [systemctl restart dfs-client  — clears in-memory chunk_cache LRU]
#       [echo 3 > /proc/sys/vm/drop_caches — clears OS page cache]
#   T4  Sequential read   (32MB file, 256KB blocks)
#   T5  Random read       (50 ops 1KB-512KB, seed=42)
#   T6  Concurrent random read  (QD=CONCURRENCY, 4KB ops, seed=43 — mirrors KDiskMark RND4K QxT1)
#   T7  Concurrent random write (QD=CONCURRENCY, 4KB ops, seed=44, single fsync at end)
#
# Fixed RNG seeds make op sequences identical across runs for before/after comparison.
# Results are appended to $CSV (default /tmp/dfs-bench-results.csv).

set -e

if [ -z "$1" ]; then
    echo "Usage: $0 <mount_point> [label]" >&2
    echo "  e.g.: $0 /mnt/test" >&2
    exit 1
fi

MOUNT=$1
LABEL=${2:-$(date +%Y%m%d-%H%M%S)}
CSV=${CSV:-/tmp/dfs-bench-results.csv}

if [ ! -d "$MOUNT" ]; then
    echo "Error: '$MOUNT' is not a directory" >&2
    exit 1
fi
MOUNT=$(cd "$MOUNT" && pwd)

SEQ_FILE="$MOUNT/bench-seq.bin"
CHUNK_FILE="$MOUNT/bench-wholechunk.img"
PATCH_FILE="$MOUNT/bench-multipatch.img"
READ_SEQ_FILE="$MOUNT/bench-seqread.bin"
READ_RAND_FILE="$MOUNT/bench-randread.img"

SEQ_SIZE_MB=${SEQ_SIZE_MB:-32}
NUM_CHUNK_OPS=${NUM_CHUNK_OPS:-8}
DISK_SIZE_MB=${DISK_SIZE_MB:-64}
NUM_OPS=${NUM_OPS:-50}
CONCURRENCY=${CONCURRENCY:-32}
CONCURRENT_OPS=${CONCURRENT_OPS:-400}

echo "=== DFS Raw-Disk Write+Read Benchmark: $LABEL ==="
echo "Mount: $MOUNT"
echo "Date: $(date)"
echo "Params: SEQ_SIZE_MB=$SEQ_SIZE_MB NUM_CHUNK_OPS=$NUM_CHUNK_OPS DISK_SIZE_MB=$DISK_SIZE_MB NUM_OPS=$NUM_OPS CONCURRENCY=$CONCURRENCY CONCURRENT_OPS=$CONCURRENT_OPS"
echo ""

CONCURRENT_READ_FILE="$MOUNT/bench-concurrent-read.img"
CONCURRENT_WRITE_FILE="$MOUNT/bench-concurrent-write.img"

rm -f "$SEQ_FILE" "$CHUNK_FILE" "$PATCH_FILE" "$READ_SEQ_FILE" "$READ_RAND_FILE" \
      "$CONCURRENT_READ_FILE" "$CONCURRENT_WRITE_FILE"

MOUNT="$MOUNT" SEQ_FILE="$SEQ_FILE" CHUNK_FILE="$CHUNK_FILE" PATCH_FILE="$PATCH_FILE" \
READ_SEQ_FILE="$READ_SEQ_FILE" READ_RAND_FILE="$READ_RAND_FILE" \
CONCURRENT_READ_FILE="$CONCURRENT_READ_FILE" CONCURRENT_WRITE_FILE="$CONCURRENT_WRITE_FILE" \
SEQ_SIZE_MB="$SEQ_SIZE_MB" NUM_CHUNK_OPS="$NUM_CHUNK_OPS" DISK_SIZE_MB="$DISK_SIZE_MB" NUM_OPS="$NUM_OPS" \
CONCURRENCY="$CONCURRENCY" CONCURRENT_OPS="$CONCURRENT_OPS" \
LABEL="$LABEL" CSV="$CSV" \
python3 - <<'PYEOF'
import os, random, subprocess, time
from concurrent.futures import ThreadPoolExecutor

mount         = os.environ['MOUNT']
seq_file      = os.environ['SEQ_FILE']
chunk_file    = os.environ['CHUNK_FILE']
patch_file    = os.environ['PATCH_FILE']
read_seq_file = os.environ['READ_SEQ_FILE']
read_rand_file= os.environ['READ_RAND_FILE']
conc_read_file  = os.environ['CONCURRENT_READ_FILE']
conc_write_file = os.environ['CONCURRENT_WRITE_FILE']
seq_size_mb   = int(os.environ['SEQ_SIZE_MB'])
num_chunk_ops = int(os.environ['NUM_CHUNK_OPS'])
disk_size_mb  = int(os.environ['DISK_SIZE_MB'])
num_ops       = int(os.environ['NUM_OPS'])
concurrency   = int(os.environ['CONCURRENCY'])
concurrent_ops= int(os.environ['CONCURRENT_OPS'])
label         = os.environ['LABEL']
csv_path      = os.environ['CSV']

MB         = 1024 * 1024
CHUNK_SIZE = 4 * MB

# ── Test 1: Sequential write ───────────────────────────────────────────────
print(f"--- Test 1: Sequential write ({seq_size_mb}MB, fsync at end) ---")
buf = os.urandom(MB)
t0 = time.time()
with open(seq_file, 'wb') as f:
    for _ in range(seq_size_mb):
        f.write(buf)
    f.flush()
    os.fsync(f.fileno())
seq_elapsed = time.time() - t0
seq_mbps = seq_size_mb / seq_elapsed
print(f"  {seq_size_mb}MB in {seq_elapsed:.2f}s = {seq_mbps:.2f} MB/s\n")

# ── Test 2: Whole-chunk overwrite ──────────────────────────────────────────
print(f"--- Test 2: Whole-chunk overwrite ({num_chunk_ops} x 4MB, fsync per op) ---")
print(f"  Pre-filling {num_chunk_ops * 4}MB (not timed)...")
zero_chunk = b'\x00' * CHUNK_SIZE
with open(chunk_file, 'wb') as f:
    for _ in range(num_chunk_ops):
        f.write(zero_chunk)
    f.flush(); os.fsync(f.fileno())

fd = os.open(chunk_file, os.O_WRONLY)
t0 = time.time()
for i in range(num_chunk_ops):
    os.lseek(fd, i * CHUNK_SIZE, os.SEEK_SET)
    os.write(fd, os.urandom(CHUNK_SIZE))
    os.fsync(fd)
chunk_elapsed = time.time() - t0
os.close(fd)
chunk_mb   = num_chunk_ops * CHUNK_SIZE / MB
chunk_mbps = chunk_mb / chunk_elapsed
print(f"  {num_chunk_ops} ops, {chunk_mb:.0f}MB in {chunk_elapsed:.2f}s = {chunk_mbps:.2f} MB/s\n")

# ── Test 3: Multi-patch write ──────────────────────────────────────────────
print(f"--- Test 3: Multi-patch write ({num_ops} ops, 1KB-1MB, fsync per op) ---")
disk_size = disk_size_mb * MB
print(f"  Pre-filling {disk_size_mb}MB (not timed)...")
zero = b'\x00' * MB
with open(patch_file, 'wb') as f:
    for _ in range(disk_size_mb):
        f.write(zero)
    f.flush(); os.fsync(f.fileno())

random.seed(42)
ops = [(random.randint(0, disk_size - (s := random.randint(1024, MB))), s) for _ in range(num_ops)]
# Regenerate cleanly (above is a bit tricky with walrus; redo simply):
random.seed(42)
ops = []
for _ in range(num_ops):
    s = random.randint(1024, MB)
    o = random.randint(0, disk_size - s)
    ops.append((o, s))
total_patch_bytes = sum(s for _, s in ops)

fd = os.open(patch_file, os.O_WRONLY)
t0 = time.time()
for off, sz in ops:
    os.lseek(fd, off, os.SEEK_SET)
    os.write(fd, os.urandom(sz))
    os.fsync(fd)
patch_elapsed = time.time() - t0
os.close(fd)
patch_mb   = total_patch_bytes / MB
patch_mbps = patch_mb / patch_elapsed
patch_iops = num_ops / patch_elapsed
print(f"  {num_ops} ops, {patch_mb:.2f}MB in {patch_elapsed:.2f}s = {patch_mbps:.2f} MB/s ({patch_iops:.1f} ops/s)\n")

# ── Pre-fill read test files (not timed) ──────────────────────────────────
print("--- Pre-filling read test files (not timed) ---")
print(f"  {seq_size_mb}MB sequential read file...")
with open(read_seq_file, 'wb') as f:
    for _ in range(seq_size_mb):
        f.write(os.urandom(MB))
    f.flush(); os.fsync(f.fileno())

print(f"  {disk_size_mb}MB random read file...")
with open(read_rand_file, 'wb') as f:
    for _ in range(disk_size_mb):
        f.write(os.urandom(MB))
    f.flush(); os.fsync(f.fileno())
print("")

# ── Restart dfs-client to clear in-memory chunk_cache LRU ─────────────────
print("--- Restarting dfs-client (clears in-memory LRU) ---")
try:
    subprocess.run(['systemctl', 'restart', 'dfs-client'], check=True, timeout=30)
    print("  Restarted. Waiting for mount...")
    for i in range(60):
        time.sleep(1)
        try:
            os.stat(mount)
            print(f"  Mount back after {i+1}s.")
            break
        except OSError:
            pass
    else:
        print("  WARNING: mount did not come back within 60s — read results may be invalid.")
except Exception as e:
    print(f"  WARNING: could not restart dfs-client ({e}).")
    print("  Reads will use warm cache — results are not a cold-cache baseline.")

# ── Drop OS page cache ─────────────────────────────────────────────────────
try:
    with open('/proc/sys/vm/drop_caches', 'w') as f:
        f.write('3\n')
    print("  OS page cache dropped.")
except Exception as e:
    print(f"  Note: could not drop page cache ({e}).")
print("")

# ── Test 4: Sequential read ────────────────────────────────────────────────
print(f"--- Test 4: Sequential read ({seq_size_mb}MB, 256KB blocks, cold cache) ---")
READ_BLOCK = 256 * 1024
t0 = time.time()
read_total = 0
with open(read_seq_file, 'rb') as f:
    while True:
        data = f.read(READ_BLOCK)
        if not data:
            break
        read_total += len(data)
seq_read_elapsed = time.time() - t0
seq_read_mbps = (read_total / MB) / seq_read_elapsed
print(f"  {read_total // MB}MB in {seq_read_elapsed:.2f}s = {seq_read_mbps:.2f} MB/s\n")

# ── Test 5: Random read ────────────────────────────────────────────────────
print(f"--- Test 5: Random read ({num_ops} ops, 1KB-512KB, seed=42, cold cache) ---")
rand_size = disk_size_mb * MB
random.seed(42)
rand_ops = []
for _ in range(num_ops):
    sz  = random.randint(1024, 512 * 1024)
    off = random.randint(0, rand_size - sz)
    rand_ops.append((off, sz))
rand_total_bytes = sum(sz for _, sz in rand_ops)

fd = os.open(read_rand_file, os.O_RDONLY)
t0 = time.time()
for off, sz in rand_ops:
    os.lseek(fd, off, os.SEEK_SET)
    os.read(fd, sz)
rand_read_elapsed = time.time() - t0
os.close(fd)
rand_read_mb   = rand_total_bytes / MB
rand_read_mbps = rand_read_mb / rand_read_elapsed
rand_read_iops = num_ops / rand_read_elapsed
print(f"  {num_ops} ops, {rand_read_mb:.2f}MB in {rand_read_elapsed:.2f}s = {rand_read_mbps:.2f} MB/s ({rand_read_iops:.1f} ops/s)\n")

# ── Test 6: Concurrent random read (QD=concurrency, 4KB ops) ──────────────
# Mirrors KDiskMark's RND4K QxT1: fixed 4KB block, many requests dispatched
# with up to `concurrency` outstanding at once, rather than one at a time.
# Each worker opens its own fd (pread is thread-safe re: position, but a
# shared fd still serializes the underlying file object in CPython) so
# threads truly issue concurrent syscalls instead of queuing on one fd.
print(f"--- Test 6: Concurrent random read (QD={concurrency}, {concurrent_ops} ops, 4KB, cold cache) ---")
CONC_BLOCK = 4096
conc_read_size = disk_size_mb * MB
with open(conc_read_file, 'wb') as f:
    for _ in range(disk_size_mb):
        f.write(os.urandom(MB))
    f.flush(); os.fsync(f.fileno())

random.seed(43)
conc_read_offsets = [
    random.randint(0, conc_read_size - CONC_BLOCK) // CONC_BLOCK * CONC_BLOCK
    for _ in range(concurrent_ops)
]

def do_concurrent_read(off):
    fd = os.open(conc_read_file, os.O_RDONLY)
    try:
        return len(os.pread(fd, CONC_BLOCK, off))
    finally:
        os.close(fd)

t0 = time.time()
with ThreadPoolExecutor(max_workers=concurrency) as pool:
    conc_read_bytes = sum(pool.map(do_concurrent_read, conc_read_offsets))
conc_read_elapsed = time.time() - t0
conc_read_mb   = conc_read_bytes / MB
conc_read_mbps = conc_read_mb / conc_read_elapsed
conc_read_iops = concurrent_ops / conc_read_elapsed
print(f"  {concurrent_ops} ops, {conc_read_mb:.2f}MB in {conc_read_elapsed:.2f}s = {conc_read_mbps:.2f} MB/s ({conc_read_iops:.1f} ops/s)\n")

# ── Test 7: Concurrent random write (QD=concurrency, 4KB ops) ─────────────
# Single fsync at the end (not per-op) — matches how a real high-queue-depth
# writer behaves (durability commit on demand, not on every 4KB op) and
# avoids conflating this concurrency test with the fsync-per-op durability
# cost already covered by Test 3.
print(f"--- Test 7: Concurrent random write (QD={concurrency}, {concurrent_ops} ops, 4KB, single fsync) ---")
conc_write_size = disk_size_mb * MB
with open(conc_write_file, 'wb') as f:
    for _ in range(disk_size_mb):
        f.write(b'\x00' * MB)
    f.flush(); os.fsync(f.fileno())

random.seed(44)
conc_write_offsets = [
    random.randint(0, conc_write_size - CONC_BLOCK) // CONC_BLOCK * CONC_BLOCK
    for _ in range(concurrent_ops)
]

def do_concurrent_write(off):
    fd = os.open(conc_write_file, os.O_WRONLY)
    try:
        return os.pwrite(fd, os.urandom(CONC_BLOCK), off)
    finally:
        os.close(fd)

t0 = time.time()
with ThreadPoolExecutor(max_workers=concurrency) as pool:
    conc_write_bytes = sum(pool.map(do_concurrent_write, conc_write_offsets))
fsync_fd = os.open(conc_write_file, os.O_WRONLY)
os.fsync(fsync_fd)
os.close(fsync_fd)
conc_write_elapsed = time.time() - t0
conc_write_mb   = conc_write_bytes / MB
conc_write_mbps = conc_write_mb / conc_write_elapsed
conc_write_iops = concurrent_ops / conc_write_elapsed
print(f"  {concurrent_ops} ops, {conc_write_mb:.2f}MB in {conc_write_elapsed:.2f}s = {conc_write_mbps:.2f} MB/s ({conc_write_iops:.1f} ops/s)\n")

# ── Summary ────────────────────────────────────────────────────────────────
print("=== Summary ===")
print(f"  Sequential write : {seq_mbps:6.2f} MB/s  ({seq_size_mb}MB in {seq_elapsed:.2f}s)")
print(f"  Whole-chunk write: {chunk_mbps:6.2f} MB/s  ({chunk_mb:.0f}MB / {num_chunk_ops} ops in {chunk_elapsed:.2f}s)")
print(f"  Multi-patch write: {patch_mbps:6.2f} MB/s  ({patch_mb:.2f}MB / {num_ops} ops in {patch_elapsed:.2f}s, {patch_iops:.1f} ops/s)")
print(f"  Sequential read  : {seq_read_mbps:6.2f} MB/s  ({read_total // MB}MB in {seq_read_elapsed:.2f}s)")
print(f"  Random read      : {rand_read_mbps:6.2f} MB/s  ({rand_read_mb:.2f}MB / {num_ops} ops in {rand_read_elapsed:.2f}s, {rand_read_iops:.1f} ops/s)")
print(f"  Concurrent read  : {conc_read_mbps:6.2f} MB/s  ({conc_read_mb:.2f}MB / {concurrent_ops} ops, QD={concurrency} in {conc_read_elapsed:.2f}s, {conc_read_iops:.1f} ops/s)")
print(f"  Concurrent write : {conc_write_mbps:6.2f} MB/s  ({conc_write_mb:.2f}MB / {concurrent_ops} ops, QD={concurrency} in {conc_write_elapsed:.2f}s, {conc_write_iops:.1f} ops/s)")

write_header = not os.path.exists(csv_path)
with open(csv_path, 'a') as f:
    if write_header:
        f.write("timestamp,label,"
                "seq_mb,seq_s,seq_mbps,"
                "chunk_ops,chunk_mb,chunk_s,chunk_mbps,"
                "patch_ops,patch_mb,patch_s,patch_mbps,patch_iops,"
                "seq_read_mb,seq_read_s,seq_read_mbps,"
                "rand_read_ops,rand_read_mb,rand_read_s,rand_read_mbps,rand_read_iops,"
                "conc_qd,conc_read_ops,conc_read_mb,conc_read_s,conc_read_mbps,conc_read_iops,"
                "conc_write_ops,conc_write_mb,conc_write_s,conc_write_mbps,conc_write_iops\n")
    f.write(f"{time.strftime('%Y-%m-%d %H:%M:%S')},{label},"
            f"{seq_size_mb},{seq_elapsed:.4f},{seq_mbps:.4f},"
            f"{num_chunk_ops},{chunk_mb:.4f},{chunk_elapsed:.4f},{chunk_mbps:.4f},"
            f"{num_ops},{patch_mb:.4f},{patch_elapsed:.4f},{patch_mbps:.4f},{patch_iops:.4f},"
            f"{read_total // MB},{seq_read_elapsed:.4f},{seq_read_mbps:.4f},"
            f"{num_ops},{rand_read_mb:.4f},{rand_read_elapsed:.4f},{rand_read_mbps:.4f},{rand_read_iops:.4f},"
            f"{concurrency},{concurrent_ops},{conc_read_mb:.4f},{conc_read_elapsed:.4f},{conc_read_mbps:.4f},{conc_read_iops:.4f},"
            f"{concurrent_ops},{conc_write_mb:.4f},{conc_write_elapsed:.4f},{conc_write_mbps:.4f},{conc_write_iops:.4f}\n")
print(f"\nResults appended to {csv_path}")
PYEOF
