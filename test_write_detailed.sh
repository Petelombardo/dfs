#!/bin/bash
# Detailed write profiling with stage-by-stage timing

set -e

BLUE='\033[0;34m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${BLUE}=== Detailed Write Performance Analysis ===${NC}\n"

# Start a fresh client with verbose logging
MOUNT_POINT="/tmp/dfs-perf-test"
mkdir -p "$MOUNT_POINT"

# Kill existing client if any
sudo pkill -9 dfs-client 2>/dev/null || true
sudo fusermount -u "$MOUNT_POINT" 2>/dev/null || true
sleep 1

echo "Starting client with verbose logging..."
RUST_LOG=debug sudo ./target/release/dfs-client mount "$MOUNT_POINT" \
    -c 127.0.0.1:8900 --write-buffer > /tmp/dfs-client-perf.log 2>&1 &
CLIENT_PID=$!

sleep 2

if ! mountpoint -q "$MOUNT_POINT"; then
    echo "Failed to mount"
    cat /tmp/dfs-client-perf.log | tail -20
    exit 1
fi

echo -e "${GREEN}✓ Client mounted at $MOUNT_POINT${NC}\n"

# Run write test
TEST_SIZE=10  # 10MB
echo -e "${YELLOW}Testing ${TEST_SIZE}MB write...${NC}"

# Generate test data
dd if=/dev/urandom of=/tmp/test_perf.bin bs=1M count=$TEST_SIZE 2>&1 | grep -v records
echo ""

# Clear caches
sync
echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null 2>&1 || true

# Perform write
echo "Writing to filesystem..."
START=$(date +%s.%N)
sudo cp /tmp/test_perf.bin "$MOUNT_POINT/test.bin"
sudo sync
END=$(date +%s.%N)

ELAPSED=$(echo "$END - $START" | bc)
THROUGHPUT=$(echo "scale=2; $TEST_SIZE / $ELAPSED" | bc)

echo -e "${GREEN}✓ Write complete${NC}"
echo "  Time: ${ELAPSED}s"
echo "  Throughput: ${THROUGHPUT} MB/s"
echo ""

# Wait for logs to flush
sleep 2

# Analyze client logs
echo -e "${BLUE}=== Client-Side Analysis ===${NC}\n"
echo "Write operations:"
grep -E "write:|Writing" /tmp/dfs-client-perf.log | tail -10

echo -e "\nCache/buffer activity:"
grep -E "Flushing|buffer" /tmp/dfs-client-perf.log | tail -10

echo -e "\nClient performance metrics:"
grep -E "throughput|complete" /tmp/dfs-client-perf.log | tail -10

# Analyze server logs
echo -e "\n${BLUE}=== Server-Side Analysis ===${NC}\n"
echo "Recent server writes:"
tail -100 /tmp/dfs-server-8900.log | grep -E "Writing|Chunking|Local write|throughput" | tail -10

# Cleanup
sudo fusermount -u "$MOUNT_POINT" 2>/dev/null || true
kill $CLIENT_PID 2>/dev/null || true
rm -f /tmp/test_perf.bin

echo -e "\n${BLUE}=== Summary ===${NC}"
echo "Client log: /tmp/dfs-client-perf.log"
echo "Server log: /tmp/dfs-server-8900.log"
echo ""
echo "Analysis:"
echo "  End-to-end: ${THROUGHPUT} MB/s"
echo "  Server capability: ~110-160 MB/s (from earlier logs)"
echo "  Network latency: <0.1ms (localhost)"
echo ""
echo "Potential bottlenecks:"
echo "  - FUSE overhead (kernel<->userspace)"
echo "  - Write buffering/flushing strategy"
echo "  - Client-side serialization"
echo "  - TCP connection overhead"
