#!/bin/bash
# Simulates DVR-like write pattern:
# - Continuous writes to growing file
# - Periodic stats/seeks to track progress
# - Tests metadata consistency across nodes

set -e

MOUNT_POINT="/mnt/test"
TEST_FILE="$MOUNT_POINT/dvr_recording.ts"
DURATION=30  # seconds

echo "=== DVR Write Pattern Test ==="
echo "Simulating continuous recording with periodic stat checks"
echo "Duration: ${DURATION}s"
echo ""

# Remove old test file
rm -f "$TEST_FILE"

# Start background writer - simulates DVR recording
# Writes 1MB chunks continuously
(
    for i in $(seq 1 $DURATION); do
        dd if=/dev/zero bs=1M count=1 >> "$TEST_FILE" 2>/dev/null
        sleep 1
    done
) &
WRITER_PID=$!

# Monitor file growth - simulates DVR app checking progress
echo "Time | Size | Chunks | Pattern"
echo "-----+------+--------+---------"

PREV_SIZE=0
for i in $(seq 1 $DURATION); do
    sleep 1

    if [ -f "$TEST_FILE" ]; then
        SIZE=$(stat -c %s "$TEST_FILE" 2>/dev/null || echo "0")

        # Check if size decreased (indicates corruption/restart)
        if [ $SIZE -lt $PREV_SIZE ]; then
            echo "$i | $SIZE | ERROR | SIZE DECREASED FROM $PREV_SIZE - FILE RESTART DETECTED!"
        elif [ $SIZE -eq $PREV_SIZE ]; then
            echo "$i | $SIZE | ?? | No growth"
        else
            CHUNKS=$((SIZE / 1048576))
            echo "$i | $SIZE | $CHUNKS | Growing"
        fi

        PREV_SIZE=$SIZE
    else
        echo "$i | N/A | N/A | File not found"
    fi
done

# Wait for writer to finish
wait $WRITER_PID 2>/dev/null || true

# Final check
if [ -f "$TEST_FILE" ]; then
    FINAL_SIZE=$(stat -c %s "$TEST_FILE")
    EXPECTED_SIZE=$((DURATION * 1048576))
    echo ""
    echo "=== Final Results ==="
    echo "Expected size: $EXPECTED_SIZE bytes (${DURATION}MB)"
    echo "Actual size:   $FINAL_SIZE bytes"

    if [ $FINAL_SIZE -eq $EXPECTED_SIZE ]; then
        echo "✓ Test PASSED - File size correct"
    else
        echo "✗ Test FAILED - Size mismatch (diff: $((EXPECTED_SIZE - FINAL_SIZE)) bytes)"
    fi
fi
