#!/bin/bash
# Quick write performance test using the currently running local cluster

set -e

BLUE='\033[0;34m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BLUE}=== DFS Write Performance Analysis ===${NC}\n"

# Configuration
MOUNT_POINT="/tmp/dfs-test-mount"
TEST_SIZES=(1 10 50)  # MB
LOG_DIR="/tmp"

# Check if mounted
if ! mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
    echo -e "${RED}Error: $MOUNT_POINT is not mounted${NC}"
    exit 1
fi

echo -e "${GREEN}✓ Filesystem mounted at $MOUNT_POINT${NC}\n"

# Function to extract timing from logs
analyze_logs() {
    local size=$1
    local server_log="/tmp/dfs-server-8900.log"

    echo -e "\n${YELLOW}Analyzing ${size}MB write...${NC}"

    # Get the last write operation from logs
    echo "Server-side metrics:"
    tail -100 "$server_log" | grep -E "Writing|Chunking|complete|throughput|quorum" | tail -10 || echo "  No metrics found in logs"

    # Calculate averages
    echo ""
    echo "Breakdown of last write operation:"
    tail -100 "$server_log" | grep -E "Chunking took" | tail -1
    tail -100 "$server_log" | grep -E "quorum write took" | tail -1
    tail -100 "$server_log" | grep -E "complete in" | tail -1
}

# Run tests
for SIZE in "${TEST_SIZES[@]}"; do
    echo -e "\n${BLUE}=== Testing ${SIZE}MB write ===${NC}"

    # Generate test data
    TEST_FILE="/tmp/test_${SIZE}mb.bin"
    dd if=/dev/urandom of="$TEST_FILE" bs=1M count=$SIZE 2>&1 | grep -v records

    # Clear any cached data
    sync
    echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null 2>&1 || true

    # Perform write with timing
    echo "Writing to $MOUNT_POINT/test_${SIZE}mb.bin..."
    START=$(date +%s.%N)
    cp "$TEST_FILE" "$MOUNT_POINT/test_${SIZE}mb.bin"
    sync
    END=$(date +%s.%N)

    # Calculate metrics
    ELAPSED=$(echo "$END - $START" | bc)
    THROUGHPUT=$(echo "scale=2; $SIZE / $ELAPSED" | bc)

    echo -e "${GREEN}✓ Write complete${NC}"
    echo "  Total time: ${ELAPSED}s"
    echo "  Throughput: ${THROUGHPUT} MB/s"

    # Analyze logs for this write
    sleep 1
    analyze_logs $SIZE

    # Cleanup
    rm -f "$TEST_FILE"
    rm -f "$MOUNT_POINT/test_${SIZE}mb.bin" 2>/dev/null || true
done

# Summary analysis
echo -e "\n${BLUE}=== Performance Summary ===${NC}\n"

echo "Server logs location: /tmp/dfs-server-*.log"
echo ""
echo "Key metrics to look for in logs:"
echo "  1. 'Chunking took' - Blake3 hashing time"
echo "  2. 'quorum write took' - Time to write to 2 replicas"
echo "  3. 'complete in' - Total server-side processing"
echo "  4. 'throughput' - Server-reported MB/s"
echo ""

# Show recent performance data
echo "Recent writes from server log:"
tail -50 /tmp/dfs-server-8900.log | grep -E "throughput|complete" | tail -5

echo -e "\n${YELLOW}To see detailed real-time logs:${NC}"
echo "  tail -f /tmp/dfs-server-8900.log"
echo ""

# Network latency consideration
echo -e "${YELLOW}Network Latency Analysis:${NC}"
echo "Testing localhost latency..."
ping -c 3 127.0.0.1 | grep "time="

echo -e "\n${BLUE}=== Test Complete ===${NC}"
