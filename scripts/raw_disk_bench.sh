#!/bin/bash
# Raw-disk write benchmark: sequential, whole-chunk overwrite, multi-patch.
#
# Usage: ./scripts/raw_disk_bench.sh [mount_point] [label]
#   mount_point  default: current directory. Run this either as
#                   /path/to/raw_disk_bench.sh /mnt/test
#                 from anywhere, or `cd /mnt/test` and run the script with
#                 no args (e.g. via its full path) -- both write the bench
#                 files into /mnt/test.
#   label        default: timestamp; recorded alongside results for tracking
#
# Test 1 (sequential): writes a new SEQ_SIZE_MB file in 1MB writes, one
#   fsync at the end -- baseline fresh-chunk write throughput, no existing
#   chunks to replace or patch.
#
# Test 2 (whole-chunk overwrite): pre-fills a CHUNK_SIZE-aligned raw disk
#   image, then overwrites NUM_CHUNK_OPS whole 4MB chunks (chunk-aligned
#   offset, exactly CHUNK_SIZE bytes, fsync per op). This is the
#   "is_full_replacement" path: the client has the complete new chunk
#   content, so it skips MultiPatch and does a fresh parallel dual-replica
#   write with no server-side read of the old chunk.
#
# Test 3 (multi-patch): pre-fills a DISK_SIZE_MB raw disk image, then
#   performs NUM_OPS writes at random offsets with random sizes (1KB-1MB),
#   fsync'ing after each write. None of these cover a full chunk, so each
#   goes through the MultiPatch/PatchChunk RPC -- the server reads the
#   existing chunk and patches it. This is the realistic small-write
#   VM-disk pattern and the slow path.
#
# A fixed RNG seed makes the multi-patch op sequence (offsets + sizes)
# identical across runs, so results before/after a change are directly
# comparable. Keep SEQ_SIZE_MB / NUM_CHUNK_OPS / DISK_SIZE_MB / NUM_OPS
# unchanged between runs you want to compare.
#
# Default sizes are chosen so each test takes roughly 10-20s at a
# conservative ~1.8MB/s. Sequential and whole-chunk writes are expected to
# be much faster than that (no server-side read), so they'll finish
# sooner -- that's fine, the reported MB/s is what matters.
#
# Results are appended to $CSV (default /tmp/dfs-bench-results.csv) for
# tracking over time.

set -e

MOUNT=${1:-.}
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

SEQ_SIZE_MB=${SEQ_SIZE_MB:-32}
NUM_CHUNK_OPS=${NUM_CHUNK_OPS:-8}     # 8 * 4MB = 32MB of whole-chunk overwrites
DISK_SIZE_MB=${DISK_SIZE_MB:-64}
NUM_OPS=${NUM_OPS:-50}

echo "=== DFS Raw-Disk Write Benchmark: $LABEL ==="
echo "Mount: $MOUNT"
echo "Date: $(date)"
echo "Params: SEQ_SIZE_MB=$SEQ_SIZE_MB NUM_CHUNK_OPS=$NUM_CHUNK_OPS DISK_SIZE_MB=$DISK_SIZE_MB NUM_OPS=$NUM_OPS"
echo ""

rm -f "$SEQ_FILE" "$CHUNK_FILE" "$PATCH_FILE"

MOUNT="$MOUNT" SEQ_FILE="$SEQ_FILE" CHUNK_FILE="$CHUNK_FILE" PATCH_FILE="$PATCH_FILE" \
SEQ_SIZE_MB="$SEQ_SIZE_MB" NUM_CHUNK_OPS="$NUM_CHUNK_OPS" DISK_SIZE_MB="$DISK_SIZE_MB" NUM_OPS="$NUM_OPS" \
LABEL="$LABEL" CSV="$CSV" \
python3 - <<'PYEOF'
import os, random, time

seq_file     = os.environ['SEQ_FILE']
chunk_file   = os.environ['CHUNK_FILE']
patch_file   = os.environ['PATCH_FILE']
seq_size_mb  = int(os.environ['SEQ_SIZE_MB'])
num_chunk_ops= int(os.environ['NUM_CHUNK_OPS'])
disk_size_mb = int(os.environ['DISK_SIZE_MB'])
num_ops      = int(os.environ['NUM_OPS'])
label        = os.environ['LABEL']
csv_path     = os.environ['CSV']

MB = 1024 * 1024
CHUNK_SIZE = 4 * MB  # matches CHUNK_SIZE in dfs-client/dfs-server

# --- Test 1: Sequential write (new file, fresh chunks) ---
print(f"--- Test 1: Sequential write ({seq_size_mb}MB, fsync at end) ---")
buf = os.urandom(MB)
start = time.time()
with open(seq_file, 'wb') as f:
    for _ in range(seq_size_mb):
        f.write(buf)
    f.flush()
    os.fsync(f.fileno())
seq_elapsed = time.time() - start
seq_mbps = seq_size_mb / seq_elapsed
print(f"  {seq_size_mb}MB in {seq_elapsed:.2f}s = {seq_mbps:.2f} MB/s")
print("")

# --- Test 2: Whole-chunk overwrite (full-replacement fast path) ---
print(f"--- Test 2: Whole-chunk overwrite ({num_chunk_ops} x 4MB chunks, fsync per op) ---")
chunk_disk_size = num_chunk_ops * CHUNK_SIZE
print(f"  Pre-filling {chunk_disk_size // MB}MB raw disk image (not timed)...")
zero_chunk = b'\x00' * CHUNK_SIZE
with open(chunk_file, 'wb') as f:
    for _ in range(num_chunk_ops):
        f.write(zero_chunk)
    f.flush()
    os.fsync(f.fileno())

fd = os.open(chunk_file, os.O_WRONLY)
start = time.time()
for i in range(num_chunk_ops):
    os.lseek(fd, i * CHUNK_SIZE, os.SEEK_SET)
    os.write(fd, os.urandom(CHUNK_SIZE))
    os.fsync(fd)
chunk_elapsed = time.time() - start
os.close(fd)

chunk_mb = (num_chunk_ops * CHUNK_SIZE) / MB
chunk_mbps = chunk_mb / chunk_elapsed
print(f"  {num_chunk_ops} ops, {chunk_mb:.2f}MB total in {chunk_elapsed:.2f}s "
      f"= {chunk_mbps:.2f} MB/s")
print("")

# --- Test 3: Multi-patch write (partial-chunk overwrite, PatchChunk RPC) ---
print(f"--- Test 3: Multi-patch write ({num_ops} ops, 1KB-1MB each, fsync per op) ---")
disk_size = disk_size_mb * MB
print(f"  Pre-filling {disk_size_mb}MB raw disk image (not timed)...")
zero = b'\x00' * MB
with open(patch_file, 'wb') as f:
    for _ in range(disk_size_mb):
        f.write(zero)
    f.flush()
    os.fsync(f.fileno())

random.seed(42)  # fixed seed: identical op sequence across runs for fair comparison
ops = []
for _ in range(num_ops):
    size = random.randint(1024, MB)
    offset = random.randint(0, disk_size - size)
    ops.append((offset, size))
total_bytes = sum(size for _, size in ops)

fd = os.open(patch_file, os.O_WRONLY)
start = time.time()
for offset, size in ops:
    os.lseek(fd, offset, os.SEEK_SET)
    os.write(fd, os.urandom(size))
    os.fsync(fd)
patch_elapsed = time.time() - start
os.close(fd)

patch_mb = total_bytes / MB
patch_mbps = patch_mb / patch_elapsed
patch_iops = num_ops / patch_elapsed
print(f"  {num_ops} ops, {patch_mb:.2f}MB total in {patch_elapsed:.2f}s "
      f"= {patch_mbps:.2f} MB/s ({patch_iops:.1f} ops/s)")
print("")

# --- Summary ---
print("=== Summary ===")
print(f"  Sequential       : {seq_mbps:6.2f} MB/s  ({seq_size_mb}MB in {seq_elapsed:.2f}s)")
print(f"  Whole-chunk write: {chunk_mbps:6.2f} MB/s  ({chunk_mb:.2f}MB / {num_chunk_ops} ops in {chunk_elapsed:.2f}s)")
print(f"  Multi-patch write: {patch_mbps:6.2f} MB/s  ({patch_mb:.2f}MB / {num_ops} ops in {patch_elapsed:.2f}s, {patch_iops:.1f} ops/s)")

write_header = not os.path.exists(csv_path)
with open(csv_path, 'a') as f:
    if write_header:
        f.write("timestamp,label,"
                "seq_mb,seq_seconds,seq_mbps,"
                "chunk_ops,chunk_mb,chunk_seconds,chunk_mbps,"
                "patch_ops,patch_mb,patch_seconds,patch_mbps,patch_iops\n")
    f.write(f"{time.strftime('%Y-%m-%d %H:%M:%S')},{label},"
            f"{seq_size_mb},{seq_elapsed:.4f},{seq_mbps:.4f},"
            f"{num_chunk_ops},{chunk_mb:.4f},{chunk_elapsed:.4f},{chunk_mbps:.4f},"
            f"{num_ops},{patch_mb:.4f},{patch_elapsed:.4f},{patch_mbps:.4f},{patch_iops:.4f}\n")

print(f"\nResults appended to {csv_path}")
PYEOF

# Final flush to ensure everything is durable on storage nodes (not timed).
sync "$MOUNT"
