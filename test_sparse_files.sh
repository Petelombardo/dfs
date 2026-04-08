#!/bin/bash
# Test sparse file support locally

set -e

echo "=== Testing Sparse File Support ==="
echo ""

# Check if local cluster is running
if ! pgrep -f "dfs-server" > /dev/null; then
    echo "ERROR: Local cluster not running. Please start with ./start_test_cluster.sh"
    exit 1
fi

MOUNT_POINT="/tmp/dfs-test-mount"

if ! mountpoint -q "$MOUNT_POINT"; then
    echo "ERROR: DFS not mounted at $MOUNT_POINT"
    echo "Please mount with: ./mount_test_client.sh"
    exit 1
fi

echo "DFS is mounted at $MOUNT_POINT"
echo ""

# Test 1: Non-sequential write with dd (write at offset 1GB)
echo "Test 1: Non-sequential write with dd seek"
echo "Creating sparse file with 1-byte write at 1GB offset..."
dd if=/dev/zero of="$MOUNT_POINT/sparse_test.img" bs=1 seek=$((1024*1024*1024)) count=1 2>&1 | grep -v records

# Check file size
FILE_SIZE=$(stat -c%s "$MOUNT_POINT/sparse_test.img")
EXPECTED_SIZE=$((1024*1024*1024 + 1))

if [ "$FILE_SIZE" -eq "$EXPECTED_SIZE" ]; then
    echo "✓ File size correct: $FILE_SIZE bytes (1GB + 1 byte)"
else
    echo "✗ FAILED: Expected $EXPECTED_SIZE bytes, got $FILE_SIZE"
    exit 1
fi

# Test 2: Read from hole (should return zeros)
echo ""
echo "Test 2: Reading from hole (first 1MB should be all zeros)"
dd if="$MOUNT_POINT/sparse_test.img" bs=1M count=1 2>/dev/null | hexdump -C | head -20

# Check if first MB is all zeros
NONZERO_COUNT=$(dd if="$MOUNT_POINT/sparse_test.img" bs=1M count=1 2>/dev/null | od -An -td1 | grep -v '^ *0$' | wc -l)

if [ "$NONZERO_COUNT" -eq 0 ]; then
    echo "✓ First 1MB is all zeros (hole detected correctly)"
else
    echo "✗ FAILED: Found non-zero bytes in hole region"
    exit 1
fi

# Test 3: Read the actual data at 1GB offset
echo ""
echo "Test 3: Reading actual data at 1GB offset"
dd if="$MOUNT_POINT/sparse_test.img" bs=1 skip=$((1024*1024*1024)) count=1 2>/dev/null | hexdump -C

DATA_BYTE=$(dd if="$MOUNT_POINT/sparse_test.img" bs=1 skip=$((1024*1024*1024)) count=1 2>/dev/null | od -An -td1 | tr -d ' ')

if [ "$DATA_BYTE" -eq 0 ]; then
    echo "✓ Data byte at 1GB is 0 (correct)"
else
    echo "✗ FAILED: Expected 0, got $DATA_BYTE"
    exit 1
fi

# Test 4: Multiple non-sequential writes
echo ""
echo "Test 4: Multiple non-sequential writes"
rm -f "$MOUNT_POINT/sparse_test.img"

echo "Writing 'A' at offset 0..."
echo -n "A" > "$MOUNT_POINT/sparse_test.img"

echo "Writing 'B' at offset 1MB..."
dd if=/dev/zero of="$MOUNT_POINT/sparse_test.img" bs=1 seek=$((1024*1024)) count=0 2>/dev/null
echo -n "B" | dd of="$MOUNT_POINT/sparse_test.img" bs=1 seek=$((1024*1024)) conv=notrunc 2>/dev/null

echo "Writing 'C' at offset 2MB..."
echo -n "C" | dd of="$MOUNT_POINT/sparse_test.img" bs=1 seek=$((2*1024*1024)) conv=notrunc 2>/dev/null

# Read and verify
BYTE_0=$(dd if="$MOUNT_POINT/sparse_test.img" bs=1 count=1 2>/dev/null)
BYTE_1M=$(dd if="$MOUNT_POINT/sparse_test.img" bs=1 skip=$((1024*1024)) count=1 2>/dev/null)
BYTE_2M=$(dd if="$MOUNT_POINT/sparse_test.img" bs=1 skip=$((2*1024*1024)) count=1 2>/dev/null)

echo "Byte at offset 0: '$BYTE_0'"
echo "Byte at offset 1MB: '$BYTE_1M'"
echo "Byte at offset 2MB: '$BYTE_2M'"

if [ "$BYTE_0" = "A" ] && [ "$BYTE_1M" = "B" ] && [ "$BYTE_2M" = "C" ]; then
    echo "✓ All non-sequential writes successful"
else
    echo "✗ FAILED: Non-sequential writes incorrect"
    exit 1
fi

# Cleanup
rm -f "$MOUNT_POINT/sparse_test.img"

echo ""
echo "=== All Tests Passed! ==="
echo ""
echo "Sparse file support is working correctly:"
echo "  ✓ Non-sequential writes"
echo "  ✓ Hole reading (returns zeros)"
echo "  ✓ Multiple sparse writes"
echo ""
echo "Next: Test with SQLite database creation"
