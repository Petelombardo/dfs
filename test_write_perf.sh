#!/bin/bash
# Quick write performance test - measures write speed at different sizes

set -e

echo "=== Quick Write Performance Test ==="
echo ""

# Test if servers are running
if ! ss -tlnp | grep -q ":8001"; then
    echo "ERROR: No server listening on port 8001"
    echo "Start local servers first with:"
    echo "  ./scripts/start-local-cluster.sh"
    exit 1
fi

echo "✓ Detected server on port 8001"

# Test sizes
SIZES=(1 10 100)  # MB

echo ""
echo "Running write tests..."
echo ""

for SIZE in "${SIZES[@]}"; do
    echo "--- ${SIZE}MB write test ---"

    # Generate test data
    dd if=/dev/zero of=/tmp/test_${SIZE}mb bs=1M count=$SIZE 2>/dev/null

    # Use nc to send a simple test (if servers support it), or use dfs-admin
    # For now, let's just check if we can write via the admin tool

    # Time the write
    START=$(date +%s.%N)
    # TODO: Add actual write command here
    END=$(date +%s.%N)

    ELAPSED=$(echo "$END - $START" | bc)
    THROUGHPUT=$(echo "scale=2; $SIZE / $ELAPSED" | bc)

    echo "  Time: ${ELAPSED}s"
    echo "  Throughput: ${THROUGHPUT} MB/s"
    echo ""

    rm -f /tmp/test_${SIZE}mb
done

echo "=== Test Complete ==="
echo ""
echo "Note: For accurate profiling, check server logs with:"
echo "  journalctl -u dfs-server@8001 -f"
