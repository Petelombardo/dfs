#!/bin/bash
# Basic file operations test - create files, copy to DFS, verify integrity

set -e

MOUNT_POINT="/tmp/dfs-test-mount"
TEST_DIR="/tmp/dfs-basic-test"

echo "=== Basic DFS Operations Test ==="
echo ""

# Check if mounted
if ! mountpoint -q "$MOUNT_POINT"; then
    echo "ERROR: DFS not mounted at $MOUNT_POINT"
    exit 1
fi

# Create test directory
rm -rf "$TEST_DIR"
mkdir -p "$TEST_DIR"

echo "Test 1: Small file (1MB) with random data"
echo "Creating local file..."
dd if=/dev/urandom of="$TEST_DIR/test_1mb.bin" bs=1M count=1 2>/dev/null
LOCAL_MD5=$(md5sum "$TEST_DIR/test_1mb.bin" | awk '{print $1}')
echo "Local MD5: $LOCAL_MD5"

echo "Copying to DFS..."
cp "$TEST_DIR/test_1mb.bin" "$MOUNT_POINT/test_1mb.bin"

echo "Verifying..."
DFS_MD5=$(md5sum "$MOUNT_POINT/test_1mb.bin" | awk '{print $1}')
echo "DFS MD5:   $DFS_MD5"

if [ "$LOCAL_MD5" = "$DFS_MD5" ]; then
    echo "✓ 1MB file integrity verified"
else
    echo "✗ FAILED: MD5 mismatch"
    exit 1
fi

echo ""
echo "Test 2: Medium file (10MB) with random data"
dd if=/dev/urandom of="$TEST_DIR/test_10mb.bin" bs=1M count=10 2>/dev/null
LOCAL_MD5=$(md5sum "$TEST_DIR/test_10mb.bin" | awk '{print $1}')
echo "Local MD5: $LOCAL_MD5"

cp "$TEST_DIR/test_10mb.bin" "$MOUNT_POINT/test_10mb.bin"
DFS_MD5=$(md5sum "$MOUNT_POINT/test_10mb.bin" | awk '{print $1}')
echo "DFS MD5:   $DFS_MD5"

if [ "$LOCAL_MD5" = "$DFS_MD5" ]; then
    echo "✓ 10MB file integrity verified"
else
    echo "✗ FAILED: MD5 mismatch"
    exit 1
fi

echo ""
echo "Test 3: Large file (50MB) with random data"
dd if=/dev/urandom of="$TEST_DIR/test_50mb.bin" bs=1M count=50 2>/dev/null
LOCAL_MD5=$(md5sum "$TEST_DIR/test_50mb.bin" | awk '{print $1}')
echo "Local MD5: $LOCAL_MD5"

cp "$TEST_DIR/test_50mb.bin" "$MOUNT_POINT/test_50mb.bin"
DFS_MD5=$(md5sum "$MOUNT_POINT/test_50mb.bin" | awk '{print $1}')
echo "DFS MD5:   $DFS_MD5"

if [ "$LOCAL_MD5" = "$DFS_MD5" ]; then
    echo "✓ 50MB file integrity verified"
else
    echo "✗ FAILED: MD5 mismatch"
    exit 1
fi

echo ""
echo "Test 4: Copy file back from DFS and verify"
ORIG_10MB_MD5=$(md5sum "$TEST_DIR/test_10mb.bin" | awk '{print $1}')
cp "$MOUNT_POINT/test_10mb.bin" "$TEST_DIR/test_10mb_copy.bin"
COPY_MD5=$(md5sum "$TEST_DIR/test_10mb_copy.bin" | awk '{print $1}')
echo "Original MD5: $ORIG_10MB_MD5"
echo "Copy MD5:     $COPY_MD5"

if [ "$ORIG_10MB_MD5" = "$COPY_MD5" ]; then
    echo "✓ Round-trip copy verified"
else
    echo "✗ FAILED: Round-trip MD5 mismatch"
    exit 1
fi

echo ""
echo "Test 5: Multiple small files"
for i in {1..10}; do
    dd if=/dev/urandom of="$TEST_DIR/small_$i.bin" bs=1K count=100 2>/dev/null
    cp "$TEST_DIR/small_$i.bin" "$MOUNT_POINT/small_$i.bin"
done

echo "Verifying 10 small files..."
FAILED=0
for i in {1..10}; do
    LOCAL_MD5=$(md5sum "$TEST_DIR/small_$i.bin" | awk '{print $1}')
    DFS_MD5=$(md5sum "$MOUNT_POINT/small_$i.bin" | awk '{print $1}')
    if [ "$LOCAL_MD5" != "$DFS_MD5" ]; then
        echo "✗ File $i failed: $LOCAL_MD5 != $DFS_MD5"
        FAILED=1
    fi
done

if [ "$FAILED" -eq 0 ]; then
    echo "✓ All 10 small files verified"
else
    echo "✗ FAILED: Some small files corrupted"
    exit 1
fi

echo ""
echo "Test 6: Text file operations"
echo "The quick brown fox jumps over the lazy dog" > "$MOUNT_POINT/test.txt"
CONTENT=$(cat "$MOUNT_POINT/test.txt")

if [ "$CONTENT" = "The quick brown fox jumps over the lazy dog" ]; then
    echo "✓ Text file read/write verified"
else
    echo "✗ FAILED: Text content mismatch"
    exit 1
fi

echo ""
echo "Test 7: File deletion"
rm "$MOUNT_POINT/test.txt"
if [ ! -f "$MOUNT_POINT/test.txt" ]; then
    echo "✓ File deletion verified"
else
    echo "✗ FAILED: File still exists after deletion"
    exit 1
fi

echo ""
echo "Test 8: Directory operations"
mkdir -p "$MOUNT_POINT/testdir/subdir"
echo "test content" > "$MOUNT_POINT/testdir/subdir/file.txt"

if [ -f "$MOUNT_POINT/testdir/subdir/file.txt" ]; then
    NESTED_CONTENT=$(cat "$MOUNT_POINT/testdir/subdir/file.txt")
    if [ "$NESTED_CONTENT" = "test content" ]; then
        echo "✓ Nested directory operations verified"
    else
        echo "✗ FAILED: Nested file content mismatch"
        exit 1
    fi
else
    echo "✗ FAILED: Nested file not found"
    exit 1
fi

echo ""
echo "Test 9: List directory"
FILE_COUNT=$(ls "$MOUNT_POINT" | wc -l)
echo "Files in mount point: $FILE_COUNT"

if [ "$FILE_COUNT" -ge 13 ]; then  # At least our test files
    echo "✓ Directory listing works"
else
    echo "✗ FAILED: Expected at least 13 files, got $FILE_COUNT"
    exit 1
fi

echo ""
echo "Test 10: File size verification"
SIZE_1MB=$(stat -c%s "$MOUNT_POINT/test_1mb.bin")
SIZE_10MB=$(stat -c%s "$MOUNT_POINT/test_10mb.bin")
SIZE_50MB=$(stat -c%s "$MOUNT_POINT/test_50mb.bin")

echo "1MB file size:  $SIZE_1MB bytes (expected: 1048576)"
echo "10MB file size: $SIZE_10MB bytes (expected: 10485760)"
echo "50MB file size: $SIZE_50MB bytes (expected: 52428800)"

if [ "$SIZE_1MB" -eq 1048576 ] && [ "$SIZE_10MB" -eq 10485760 ] && [ "$SIZE_50MB" -eq 52428800 ]; then
    echo "✓ File sizes correct"
else
    echo "✗ FAILED: File size mismatch"
    exit 1
fi

# Cleanup
echo ""
echo "Cleaning up..."
rm -rf "$MOUNT_POINT/test_"* "$MOUNT_POINT/small_"* "$MOUNT_POINT/testdir"
rm -rf "$TEST_DIR"

echo ""
echo "=== All Basic Tests Passed! ==="
echo ""
echo "Summary:"
echo "  ✓ 1MB file integrity (random data)"
echo "  ✓ 10MB file integrity (random data)"
echo "  ✓ 50MB file integrity (random data)"
echo "  ✓ Round-trip copy"
echo "  ✓ Multiple small files (10 x 100KB)"
echo "  ✓ Text file operations"
echo "  ✓ File deletion"
echo "  ✓ Nested directories"
echo "  ✓ Directory listing"
echo "  ✓ File size verification"
echo ""
echo "Total data tested: ~61MB"
