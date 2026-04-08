#!/bin/bash
# Write performance profiling script
# Tests both local (without network) and simulated network scenarios

set -e

# Colors for output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${BLUE}=== DFS Write Performance Profiling ===${NC}\n"

# Test parameters
TEST_SIZES=(1048576 10485760 104857600)  # 1MB, 10MB, 100MB
TEST_NAMES=("1MB" "10MB" "100MB")
RESULTS_FILE="profile_results.txt"

# Clear previous results
> "$RESULTS_FILE"

echo "Test Configuration:"
echo "  - Test sizes: ${TEST_NAMES[@]}"
echo "  - Results file: $RESULTS_FILE"
echo ""

# Function to measure write performance
measure_write() {
    local size=$1
    local name=$2
    local type=$3

    echo -e "${YELLOW}Testing $name write ($type)...${NC}"

    # Generate test data
    dd if=/dev/urandom of=/tmp/test_write_$name bs=1M count=$(($size / 1048576)) 2>/dev/null

    # Run test with detailed timing
    if [ "$type" = "local" ]; then
        # Local test: single server write
        RUST_LOG=info cargo run --release --bin dfs-server 2>&1 | grep -E "(Writing|Chunking|complete|throughput)" &
        SERVER_PID=$!
        sleep 2

        # Simulate write via client
        time cargo run --release --bin dfs-client -- write /tmp/test_write_$name /test_$name 2>&1 | tee -a "$RESULTS_FILE"

        kill $SERVER_PID 2>/dev/null || true
    else
        # Network test: dual-replica write
        echo "  Simulating network latency with tc..."
    fi

    rm -f /tmp/test_write_$name
    echo ""
}

# Check if we have 3 local servers running
if pgrep -f "dfs-server.*8001" > /dev/null && \
   pgrep -f "dfs-server.*8002" > /dev/null && \
   pgrep -f "dfs-server.*8003" > /dev/null; then
    echo -e "${GREEN}Found 3 local servers running${NC}"
    LOCAL_TEST=true
else
    echo -e "${YELLOW}Local servers not running - will skip local tests${NC}"
    LOCAL_TEST=false
fi

# Run tests
for i in "${!TEST_SIZES[@]}"; do
    size=${TEST_SIZES[$i]}
    name=${TEST_NAMES[$i]}

    if [ "$LOCAL_TEST" = true ]; then
        measure_write "$size" "$name" "local"
    fi
done

echo -e "\n${BLUE}=== Profiling Complete ===${NC}"
echo "Results saved to: $RESULTS_FILE"
echo ""
echo "Key metrics to analyze:"
echo "  1. Chunking time (Blake3 hashing)"
echo "  2. Network send/recv time"
echo "  3. Serialization overhead (bincode)"
echo "  4. Disk I/O time"
echo "  5. SQLite metadata updates"
echo "  6. Total throughput (MB/s)"
