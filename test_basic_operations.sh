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

# Cleanup existing test files
echo ""
echo "Cleaning up earlier test files..."
rm -rf "$MOUNT_POINT/test_"* "$MOUNT_POINT/small_"* "$MOUNT_POINT/testdir"

echo ""
echo "Test 11: Large file delete (400MB / ~100 chunks)"
echo "Writing 400MB file to DFS..."
dd if=/dev/urandom of="$TEST_DIR/test_400mb.bin" bs=1M count=400 2>/dev/null
LOCAL_MD5=$(md5sum "$TEST_DIR/test_400mb.bin" | awk '{print $1}')
echo "Local MD5: $LOCAL_MD5"

cp "$TEST_DIR/test_400mb.bin" "$MOUNT_POINT/test_400mb.bin"
DFS_MD5=$(md5sum "$MOUNT_POINT/test_400mb.bin" | awk '{print $1}')
echo "DFS MD5:   $DFS_MD5"

if [ "$LOCAL_MD5" != "$DFS_MD5" ]; then
    echo "✗ FAILED: MD5 mismatch before delete"
    exit 1
fi
echo "✓ 400MB file written and verified"

# Record which chunk files exist on the local nodes before delete
CHUNK_DIR_1="/mnt/storage/dfs1/data/chunks"
CHUNK_DIR_2="/mnt/storage/dfs2/data/chunks"
CHUNK_DIR_3="/mnt/storage/dfs3/data/chunks"
CHUNKS_BEFORE=$(find "$CHUNK_DIR_1" "$CHUNK_DIR_2" "$CHUNK_DIR_3" -type f 2>/dev/null | wc -l)
echo "Chunk files on disk before delete: $CHUNKS_BEFORE"

echo "Deleting 400MB file..."
DELETE_START=$(date +%s%3N)
rm "$MOUNT_POINT/test_400mb.bin"
DELETE_END=$(date +%s%3N)
DELETE_MS=$((DELETE_END - DELETE_START))
echo "rm returned in ${DELETE_MS}ms (should be fast — async delete)"

# Verify file is gone from namespace immediately
if [ -f "$MOUNT_POINT/test_400mb.bin" ]; then
    echo "✗ FAILED: File still visible in namespace after delete"
    exit 1
fi
echo "✓ File gone from namespace immediately"

# Verify rm was non-blocking (should complete in well under 5 seconds even for large files)
if [ "$DELETE_MS" -gt 5000 ]; then
    echo "✗ FAILED: rm blocked for ${DELETE_MS}ms — delete should be non-blocking"
    exit 1
fi
echo "✓ Delete was non-blocking (${DELETE_MS}ms)"

# Wait for the async drain worker to complete chunk deletion
echo "Waiting for async chunk deletion to complete (up to 60s)..."
WAITED=0
while [ $WAITED -lt 60 ]; do
    sleep 2
    WAITED=$((WAITED + 2))
    CHUNKS_AFTER=$(find "$CHUNK_DIR_1" "$CHUNK_DIR_2" "$CHUNK_DIR_3" -type f 2>/dev/null | wc -l)
    if [ "$CHUNKS_AFTER" -lt "$CHUNKS_BEFORE" ]; then
        echo "✓ Chunks deleted from disk after ${WAITED}s (${CHUNKS_BEFORE} -> ${CHUNKS_AFTER} chunk files)"
        break
    fi
done

if [ "$CHUNKS_AFTER" -ge "$CHUNKS_BEFORE" ]; then
    echo "✗ FAILED: Chunk count did not decrease after ${WAITED}s (before=$CHUNKS_BEFORE after=$CHUNKS_AFTER)"
    exit 1
fi

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
echo "  ✓ Large file delete (400MB, non-blocking, async chunk cleanup)"
echo ""
echo "Total data tested: ~461MB"
