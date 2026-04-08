#!/bin/bash

# Test script to verify write and read operations are independent (non-blocking)
# Tests: 1) Write only, 2) Read only, 3) Simultaneous write + read

set -e

MOUNT_POINT="/mnt/test"
TEST_FILE="$MOUNT_POINT/test_data.bin"
WRITE_SIZE_MB=500
READ_SIZE_MB=500

echo "================================================"
echo "Testing Write/Read Independence"
echo "================================================"
echo "Mount point: $MOUNT_POINT"
echo "Test file: $TEST_FILE"
echo "Write size: ${WRITE_SIZE_MB}MB"
echo "Read size: ${READ_SIZE_MB}MB"
echo ""

# Clean up any existing test file
rm -f "$TEST_FILE"

# Test 1: Write only
echo "Test 1: Write Only (Baseline)"
echo "-----------------------------------"
WRITE_START=$(date +%s.%N)
dd if=/dev/zero of="$TEST_FILE" bs=1M count=$WRITE_SIZE_MB status=progress 2>&1 | tail -3
WRITE_END=$(date +%s.%N)
WRITE_TIME=$(echo "$WRITE_END - $WRITE_START" | bc)
WRITE_SPEED=$(echo "scale=2; $WRITE_SIZE_MB / $WRITE_TIME" | bc)
echo ""
echo "Write completed in ${WRITE_TIME}s (${WRITE_SPEED} MB/s)"
echo ""
sync

sleep 2

# Test 2: Read only
echo "Test 2: Read Only (Baseline)"
echo "-----------------------------------"
# Clear page cache
echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null
READ_START=$(date +%s.%N)
dd if="$TEST_FILE" of=/dev/null bs=1M count=$READ_SIZE_MB status=progress 2>&1 | tail -3
READ_END=$(date +%s.%N)
READ_TIME=$(echo "$READ_END - $READ_START" | bc)
READ_SPEED=$(echo "scale=2; $READ_SIZE_MB / $READ_TIME" | bc)
echo ""
echo "Read completed in ${READ_TIME}s (${READ_SPEED} MB/s)"
echo ""

sleep 2

# Clean up for simultaneous test
rm -f "$TEST_FILE"
sleep 1

# Test 3: Simultaneous write and read
echo "Test 3: Simultaneous Write + Read"
echo "-----------------------------------"
echo "Starting write and read operations in parallel..."

# Create a fresh file for the simultaneous test
dd if=/dev/zero of="$TEST_FILE" bs=1M count=$WRITE_SIZE_MB > /dev/null 2>&1
sync
echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null

echo ""
echo "Starting simultaneous operations..."

# Start write operation in background
(
    WRITE_FILE="$MOUNT_POINT/test_write_concurrent.bin"
    WRITE_START=$(date +%s.%N)
    dd if=/dev/zero of="$WRITE_FILE" bs=1M count=$WRITE_SIZE_MB > /dev/null 2>&1
    WRITE_END=$(date +%s.%N)
    CONCURRENT_WRITE_TIME=$(echo "$WRITE_END - $WRITE_START" | bc)
    echo "$CONCURRENT_WRITE_TIME" > /tmp/concurrent_write_time
) &
WRITE_PID=$!

# Small delay to ensure write starts first
sleep 0.5

# Start read operation in background
(
    READ_START=$(date +%s.%N)
    dd if="$TEST_FILE" of=/dev/null bs=1M count=$READ_SIZE_MB > /dev/null 2>&1
    READ_END=$(date +%s.%N)
    CONCURRENT_READ_TIME=$(echo "$READ_END - $READ_START" | bc)
    echo "$CONCURRENT_READ_TIME" > /tmp/concurrent_read_time
) &
READ_PID=$!

# Wait for both operations to complete
wait $WRITE_PID
wait $READ_PID

CONCURRENT_WRITE_TIME=$(cat /tmp/concurrent_write_time)
CONCURRENT_READ_TIME=$(cat /tmp/concurrent_read_time)

CONCURRENT_WRITE_SPEED=$(echo "scale=2; $WRITE_SIZE_MB / $CONCURRENT_WRITE_TIME" | bc)
CONCURRENT_READ_SPEED=$(echo "scale=2; $READ_SIZE_MB / $CONCURRENT_READ_TIME" | bc)

echo ""
echo "Concurrent write completed in ${CONCURRENT_WRITE_TIME}s (${CONCURRENT_WRITE_SPEED} MB/s)"
echo "Concurrent read completed in ${CONCURRENT_READ_TIME}s (${CONCURRENT_READ_SPEED} MB/s)"
echo ""

# Analysis
echo "================================================"
echo "ANALYSIS"
echo "================================================"
echo ""
echo "Baseline Performance:"
echo "  Write: ${WRITE_TIME}s (${WRITE_SPEED} MB/s)"
echo "  Read:  ${READ_TIME}s (${READ_SPEED} MB/s)"
echo ""
echo "Concurrent Performance:"
echo "  Write: ${CONCURRENT_WRITE_TIME}s (${CONCURRENT_WRITE_SPEED} MB/s)"
echo "  Read:  ${CONCURRENT_READ_TIME}s (${CONCURRENT_READ_SPEED} MB/s)"
echo ""

# Calculate performance degradation
WRITE_DEGRADATION=$(echo "scale=2; ($CONCURRENT_WRITE_TIME / $WRITE_TIME - 1) * 100" | bc)
READ_DEGRADATION=$(echo "scale=2; ($CONCURRENT_READ_TIME / $READ_TIME - 1) * 100" | bc)

echo "Performance Change:"
echo "  Write: ${WRITE_DEGRADATION}% slower"
echo "  Read:  ${READ_DEGRADATION}% slower"
echo ""

# Determine if operations are blocking
if (( $(echo "$WRITE_DEGRADATION < 10" | bc -l) )) && (( $(echo "$READ_DEGRADATION < 10" | bc -l) )); then
    echo "✓ RESULT: Operations appear to be INDEPENDENT (minimal blocking)"
    echo "  Both operations maintained >90% of baseline performance"
elif (( $(echo "$WRITE_DEGRADATION > 50" | bc -l) )) || (( $(echo "$READ_DEGRADATION > 50" | bc -l) )); then
    echo "✗ RESULT: Operations appear to be BLOCKING each other"
    echo "  Significant performance degradation detected"
else
    echo "~ RESULT: Moderate interference detected"
    echo "  Some contention but not complete blocking"
fi
echo ""

# Clean up
rm -f "$TEST_FILE" "$MOUNT_POINT/test_write_concurrent.bin"
rm -f /tmp/concurrent_write_time /tmp/concurrent_read_time

echo "Test complete!"
