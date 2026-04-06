#!/bin/bash
# Test script to simulate HDHomeRun failover behavior
# This should reproduce the tiny chunk issue

set -e

MOUNT_POINT="/tmp/test-dfs-mount"
TEST_FILE="$MOUNT_POINT/test_failover.dat"

echo "=== HDHomeRun Failover Simulation Test ==="
echo ""

# Clean up any existing mount
fusermount -u "$MOUNT_POINT" 2>/dev/null || true
rm -rf "$MOUNT_POINT"
mkdir -p "$MOUNT_POINT"

# Start local DFS cluster
echo "Starting local DFS cluster..."
./scripts/start-local-cluster.sh

# Start client with write buffering
echo "Starting FUSE client with write buffering..."
nohup target/release/dfs-client mount "$MOUNT_POINT" \
    --cluster 127.0.0.1:8081 \
    --log-level info \
    --write-buffer \
    --log-file /tmp/test-failover-client.log \
    > /tmp/test-failover-startup.log 2>&1 &

sleep 2

# Check if mount succeeded
if ! mountpoint -q "$MOUNT_POINT"; then
    echo "ERROR: Failed to mount filesystem"
    cat /tmp/test-failover-startup.log
    exit 1
fi

echo "Mounted successfully at $MOUNT_POINT"
echo ""

# Phase 1: Simulate normal recording (400 chunks of 4MB each = 1.6GB)
echo "Phase 1: Writing 400 chunks of 4MB each (simulating normal recording)..."
dd if=/dev/zero of="$TEST_FILE" bs=4M count=400 2>&1 | tail -1
echo "Phase 1 complete: $(stat -c%s "$TEST_FILE") bytes written"
echo ""

# CRITICAL: Close the file to simulate DVR closing during failover
echo "Simulating file close (HDHomeRun failover)..."
sync
sleep 5  # Let background flush complete
echo ""

# Phase 2: Reopen and append tiny chunks (simulating failover writes)
echo "Phase 2: Appending 100 tiny chunks (12 bytes to 24KB) simulating post-failover..."
for i in {1..100}; do
    # Random sizes between 12 bytes and 24KB
    size=$((RANDOM % 24576 + 12))
    dd if=/dev/zero bs=$size count=1 >> "$TEST_FILE" 2>/dev/null
done
echo "Phase 2 complete: $(stat -c%s "$TEST_FILE") bytes total"
echo ""

# Close the file
sync
sleep 5

# Analyze the resulting chunk structure
echo "Analyzing chunk structure..."
./target/release/dfs-admin file "/test_failover.dat" --cluster 127.0.0.1:8081 | grep -E "(Total chunks|Chunk sizes)" | head -20

echo ""
echo "=== Test Complete ===="
echo "Check if we created tiny chunks like the real file:"
echo "Expected: 400 chunks of 4MB, then many tiny chunks"

# Cleanup
fusermount -u "$MOUNT_POINT"
./scripts/stop-local-cluster.sh
