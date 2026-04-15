#!/bin/bash
# Test sparse file write/read correctness on DFS.
# Simulates exactly what Proxmox does with a qcow2 VM disk:
#   1. truncate-grow (qemu-img create)
#   2. sequential append writes (qcow2 header)
#   3. random overwrites at scattered offsets (fdisk partition table, mkfs superblocks)
#   4. read-back verification at all written offsets
#
# Does not require nbd or loop devices — tests DFS directly via the FUSE mount.

set -e

MOUNT=/tmp/dfs-mount
FILE=$MOUNT/images/test-sparse.bin
SIZE=$((256 * 1024 * 1024))  # 256 MiB — large enough to exercise sparse regions
PASS=0
FAIL=0

cleanup() {
    rm -f "$FILE" 2>/dev/null || true
}
trap cleanup EXIT

fail() { echo "  ✗ FAIL: $1"; FAIL=$((FAIL+1)); }
pass() { echo "  ✓ PASS: $1"; PASS=$((PASS+1)); }

# ── 0. Preflight ──────────────────────────────────────────────────────────────
echo "[0/7] Preflight..."
if ! mountpoint -q "$MOUNT"; then
    echo "ERROR: DFS not mounted at $MOUNT"
    echo "  Run: ./start_local_cluster.sh && ./mount_local_cluster.sh $MOUNT"
    exit 1
fi
mkdir -p "$MOUNT/images"
echo "  DFS mounted at $MOUNT — OK"

# ── 1. Truncate-grow (qemu-img create equivalent) ─────────────────────────────
echo "[1/7] Truncate-grow to ${SIZE} bytes (simulates qemu-img create)..."
truncate -s $SIZE "$FILE"
ACTUAL_SIZE=$(stat -c%s "$FILE")
if [ "$ACTUAL_SIZE" -eq "$SIZE" ]; then
    pass "file size is $SIZE after truncate"
else
    fail "expected size $SIZE, got $ACTUAL_SIZE"
fi

# ── 2. Sequential writes at offset 0 (qcow2 header region) ───────────────────
echo "[2/7] Sequential writes at offset 0 (simulates qcow2 header)..."
HEADER="QFI\xfb"  # qcow2 magic bytes
printf 'QCOW2_HEADER_SIMULATION_DATA_BLOCK_0000' | dd of="$FILE" bs=512 count=1 conv=notrunc 2>/dev/null
printf 'QCOW2_HEADER_SIMULATION_DATA_BLOCK_0001' | dd of="$FILE" bs=512 seek=1 count=1 conv=notrunc 2>/dev/null
printf 'QCOW2_HEADER_SIMULATION_DATA_BLOCK_0002' | dd of="$FILE" bs=512 seek=2 count=1 conv=notrunc 2>/dev/null
sync
ACTUAL=$(dd if="$FILE" bs=512 count=1 2>/dev/null)
if echo "$ACTUAL" | grep -q "QCOW2_HEADER_SIMULATION"; then
    pass "sequential header write/read at offset 0"
else
    fail "header not readable at offset 0 (got: ${ACTUAL:0:40})"
fi

# ── 3. Random write at 1MiB offset (fdisk GPT header) ────────────────────────
echo "[3/7] Random write at 1MiB offset (simulates fdisk GPT header)..."
OFFSET_1M=$((1 * 1024 * 1024))  # = 2048 * 512
# Write a 512-byte block; payload in first 40 bytes, rest zero-padded by printf
{ printf 'PARTITION_TABLE_GPT_HEADER_AT_1MIB_____'; dd if=/dev/zero bs=1 count=472 2>/dev/null; } \
    | dd of="$FILE" bs=512 seek=$(( OFFSET_1M / 512 )) count=1 conv=notrunc 2>/dev/null
sync
ACTUAL=$(dd if="$FILE" bs=512 skip=$(( OFFSET_1M / 512 )) count=1 2>/dev/null | head -c 40)
if [ "$ACTUAL" = "PARTITION_TABLE_GPT_HEADER_AT_1MIB_____" ]; then
    pass "random write/read at 1MiB offset"
else
    fail "1MiB offset mismatch (got: '${ACTUAL:0:40}')"
fi

# ── 4. Random write at 128MiB offset (mkfs superblock backup) ────────────────
echo "[4/7] Random write at 128MiB offset (simulates mkfs backup superblock)..."
OFFSET_128M=$((128 * 1024 * 1024))  # = 262144 * 512
{ printf 'EXT4_BACKUP_SUPERBLOCK_AT_128MIB_______'; dd if=/dev/zero bs=1 count=472 2>/dev/null; } \
    | dd of="$FILE" bs=512 seek=$(( OFFSET_128M / 512 )) count=1 conv=notrunc 2>/dev/null
sync
ACTUAL=$(dd if="$FILE" bs=512 skip=$(( OFFSET_128M / 512 )) count=1 2>/dev/null | head -c 40)
if [ "$ACTUAL" = "EXT4_BACKUP_SUPERBLOCK_AT_128MIB_______" ]; then
    pass "random write/read at 128MiB offset"
else
    fail "128MiB offset mismatch (got: '${ACTUAL:0:40}')"
fi

# ── 5. Hole reads return zeros ────────────────────────────────────────────────
echo "[5/7] Unwritten sparse regions return zeros..."
OFFSET_64M=$((64 * 1024 * 1024))
ZERO_CHECK=$(dd if="$FILE" bs=4096 skip=$((OFFSET_64M / 4096)) count=1 2>/dev/null | tr -d '\0' | wc -c)
if [ "$ZERO_CHECK" -eq 0 ]; then
    pass "unwritten region at 64MiB returns zeros"
else
    fail "unwritten region at 64MiB contains non-zero bytes ($ZERO_CHECK bytes)"
fi

# ── 6. Overwrite existing data ───────────────────────────────────────────────
echo "[6/7] Overwrite existing data (simulates mkfs writing over fdisk GPT)..."
{ printf 'EXT4_SUPERBLOCK_OVERWROTE_GPT_HEADER___'; dd if=/dev/zero bs=1 count=472 2>/dev/null; } \
    | dd of="$FILE" bs=512 seek=$(( OFFSET_1M / 512 )) count=1 conv=notrunc 2>/dev/null
sync
ACTUAL=$(dd if="$FILE" bs=512 skip=$(( OFFSET_1M / 512 )) count=1 2>/dev/null | head -c 40)
if [ "$ACTUAL" = "EXT4_SUPERBLOCK_OVERWROTE_GPT_HEADER___" ]; then
    pass "overwrite at 1MiB: new data readable"
else
    fail "overwrite at 1MiB: got '${ACTUAL:0:40}'"
fi

# ── 7. Verify header still intact after overwrites elsewhere ─────────────────
echo "[7/7] Verify offset-0 header unaffected by writes at other offsets..."
ACTUAL=$(dd if="$FILE" bs=512 count=1 2>/dev/null)
if echo "$ACTUAL" | grep -q "QCOW2_HEADER_SIMULATION"; then
    pass "offset-0 header intact after writes at other offsets"
else
    fail "offset-0 header corrupted (got: '${ACTUAL:0:40}')"
fi

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
echo "Results: $PASS passed, $FAIL failed"
if [ "$FAIL" -eq 0 ]; then
    echo "✓ ALL TESTS PASSED"
    exit 0
else
    echo "✗ SOME TESTS FAILED"
    exit 1
fi
