#!/bin/bash
# Test that chunk alignment prevents tiny chunks during file close/reopen

set -e

echo "=== Testing Chunk Alignment Fix ==="
echo ""

# Start local cluster
echo "Starting local DFS cluster..."
./scripts/start-local-cluster.sh > /dev/null 2>&1
sleep 2

MOUNT="/tmp/test-alignment"
TEST_FILE="$MOUNT/test.dat"

# Clean up any existing mount
fusermount -u "$MOUNT" 2>/dev/null || true
rm -rf "$MOUNT"
mkdir -p "$MOUNT"

# Start client
echo "Starting FUSE client with write buffering..."
nohup target/release/dfs-client mount "$MOUNT" \
    --cluster 127.0.0.1:8081 \
    --log-level info \
    --write-buffer \
    --log-file /tmp/test-alignment.log \
    > /dev/null 2>&1 &

sleep 2

if ! mountpoint -q "$MOUNT"; then
    echo "ERROR: Failed to mount"
    exit 1
fi

echo "Test 1: Write 5MB, close file (should create 1 chunk of 4MB + keep 1MB buffered)"
dd if=/dev/zero of="$TEST_FILE" bs=1M count=5 2>/dev/null
sync
sleep 1

# Check chunk structure
CHUNKS=$(./target/release/dfs-admin file "/test.dat" --cluster 127.0.0.1:8081 2>/dev/null | grep "Total chunks:" | awk '{print $3}')
echo "  Chunks created: $CHUNKS (expected: 1, since 1MB stays buffered)"

echo ""
echo "Test 2: Reopen and append 3MB (buffer now has 1MB + 3MB = 4MB, should flush on close)"
dd if=/dev/zero bs=1M count=3 >> "$TEST_FILE" 2>/dev/null
sync
sleep 1

CHUNKS=$(./target/release/dfs-admin file "/test.dat" --cluster 127.0.0.1:8081 2>/dev/null | grep "Total chunks:" | awk '{print $3}')
echo "  Chunks created: $CHUNKS (expected: 2, both 4MB chunks)"

echo ""
echo "Test 3: Check chunk sizes"
./target/release/dfs-admin file "/test.dat" --cluster 127.0.0.1:8081 2>/dev/null | grep -A 5 "Chunk sizes"

# Cleanup
echo ""
echo "Cleaning up..."
fusermount -u "$MOUNT"
./scripts/stop-local-cluster.sh > /dev/null 2>&1
rm -rf "$MOUNT"

echo ""
echo "=== Test Complete ==="
