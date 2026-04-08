#!/bin/bash
# Test seeking performance in DFS

set -e

MOUNT_POINT="/tmp/dfs-test-mount"
TEST_FILE="$MOUNT_POINT/seek_test.dat"
FILE_SIZE_MB=100

echo "=== DFS Seek Performance Test ==="
echo ""

# Create test file
echo "Creating ${FILE_SIZE_MB}MB test file..."
dd if=/dev/urandom of="$TEST_FILE" bs=1M count=$FILE_SIZE_MB 2>&1 | grep -v records

echo ""
echo "Test 1: Sequential read (baseline)"
time dd if="$TEST_FILE" of=/dev/null bs=1M 2>&1 | grep -E "copied|MB/s"

echo ""
echo "Test 2: Seek to middle (50MB offset)"
time dd if="$TEST_FILE" of=/dev/null bs=1M skip=50 count=1 2>&1 | grep -E "copied|MB/s"

echo ""
echo "Test 3: Seek to end (99MB offset)"
time dd if="$TEST_FILE" of=/dev/null bs=1M skip=99 count=1 2>&1 | grep -E "copied|MB/s"

echo ""
echo "Test 4: Multiple random seeks (simulating video seeking)"
for offset in 10 30 50 70 90 20 60 40 80; do
    echo -n "Seek to ${offset}MB: "
    time dd if="$TEST_FILE" of=/dev/null bs=1M skip=$offset count=1 2>&1 | grep copied
done

echo ""
echo "Test 5: Read after seek (1MB at offset 50MB)"
time (dd if="$TEST_FILE" of=/dev/null bs=1M skip=50 count=1 && echo "Done") 2>&1 | grep -E "copied|Done|real"

echo ""
echo "Test 6: Comparison - Local file seek performance"
LOCAL_TEST="/tmp/local_seek_test.dat"
echo "Creating local test file..."
dd if=/dev/urandom of="$LOCAL_TEST" bs=1M count=$FILE_SIZE_MB 2>&1 | grep -v records

echo ""
echo "Local file - Seek to middle (50MB offset)"
time dd if="$LOCAL_TEST" of=/dev/null bs=1M skip=50 count=1 2>&1 | grep -E "copied|MB/s"

echo ""
echo "Local file - Multiple seeks"
for offset in 10 30 50 70 90; do
    echo -n "Local seek to ${offset}MB: "
    time dd if="$LOCAL_TEST" of=/dev/null bs=1M skip=$offset count=1 2>&1 | grep copied
done

# Cleanup
rm -f "$LOCAL_TEST"

echo ""
echo "=== Seek Performance Test Complete ==="
