#!/bin/bash
# More aggressive DVR simulation:
# - Appends to file
# - Multiple concurrent readers checking file status
# - Simulates the seek/stat pattern that media apps do

set -e

MOUNT_POINT="/mnt/test"
TEST_FILE="$MOUNT_POINT/stream.ts"
DURATION=60

echo "=== Aggressive DVR Stress Test ==="
echo "Simulating DVR recording with concurrent readers"
echo "Duration: ${DURATION}s"
echo ""

# Clean start
rm -f "$TEST_FILE"

# Background writer - appends continuously
(
    for i in $(seq 1 $DURATION); do
        # Write 1MB chunk
        dd if=/dev/zero bs=1M count=1 oflag=append conv=notrunc >> "$TEST_FILE" 2>/dev/null
        sleep 1
    done
) &
WRITER_PID=$!

# Multiple concurrent readers - simulates DVR app + player
for reader_id in 1 2 3; do
    (
        for i in $(seq 1 $DURATION); do
            if [ -f "$TEST_FILE" ]; then
                # Stat the file (DVR checks size for progress)
                SIZE=$(stat -c %s "$TEST_FILE" 2>/dev/null || echo "0")

                # Try to read the file (player checking if playable)
                if [ $SIZE -gt 4194304 ]; then  # > 4MB
                    # Read first 1MB and last 1MB (typical player seek pattern)
                    dd if="$TEST_FILE" of=/dev/null bs=1M count=1 skip=0 2>/dev/null || true
                    CHUNKS=$((SIZE / 1048576))
                    if [ $CHUNKS -gt 2 ]; then
                        dd if="$TEST_FILE" of=/dev/null bs=1M count=1 skip=$((CHUNKS - 2)) 2>/dev/null || true
                    fi
                fi
            fi
            sleep 2
        done
    ) &
    READER_PIDS[$reader_id]=$!
done

# Monitor for corruption
echo "Time | Size | Delta | Status"
echo "-----+------+-------+---------"

PREV_SIZE=0
CORRUPTION_DETECTED=0

for i in $(seq 1 $DURATION); do
    sleep 1

    if [ -f "$TEST_FILE" ]; then
        SIZE=$(stat -c %s "$TEST_FILE" 2>/dev/null || echo "0")
        DELTA=$((SIZE - PREV_SIZE))

        # Check for corruption indicators
        if [ $SIZE -lt $PREV_SIZE ]; then
            echo "$i | $SIZE | $DELTA | CORRUPTION: SIZE DECREASED!"
            CORRUPTION_DETECTED=1
        elif [ $SIZE -eq 0 ] && [ $i -gt 2 ]; then
            echo "$i | $SIZE | $DELTA | CORRUPTION: FILE EMPTY!"
            CORRUPTION_DETECTED=1
        elif [ $SIZE -eq $PREV_SIZE ]; then
            echo "$i | $SIZE | $DELTA | Stalled"
        else
            MB=$((SIZE / 1048576))
            echo "$i | $SIZE | $DELTA | OK (${MB}MB)"
        fi

        PREV_SIZE=$SIZE
    else
        if [ $i -gt 2 ]; then
            echo "$i | N/A | N/A | CORRUPTION: FILE VANISHED!"
            CORRUPTION_DETECTED=1
        else
            echo "$i | N/A | N/A | Waiting for file..."
        fi
    fi
done

# Cleanup
wait $WRITER_PID 2>/dev/null || true
for pid in "${READER_PIDS[@]}"; do
    kill $pid 2>/dev/null || true
    wait $pid 2>/dev/null || true
done

# Final verification
echo ""
echo "=== Final Results ==="
if [ -f "$TEST_FILE" ]; then
    FINAL_SIZE=$(stat -c %s "$TEST_FILE")
    EXPECTED_SIZE=$((DURATION * 1048576))
    echo "Expected size: $EXPECTED_SIZE bytes (${DURATION}MB)"
    echo "Actual size:   $FINAL_SIZE bytes"

    if [ $CORRUPTION_DETECTED -eq 1 ]; then
        echo "✗ Test FAILED - Corruption detected during recording"
        exit 1
    elif [ $FINAL_SIZE -ne $EXPECTED_SIZE ]; then
        echo "✗ Test FAILED - Size mismatch"
        exit 1
    else
        echo "✓ Test PASSED - No corruption detected"
    fi
else
    echo "✗ Test FAILED - File does not exist"
    exit 1
fi
