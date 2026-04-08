#!/bin/bash
# Simple write performance profiling using existing instrumentation
# Captures timing from logs to identify bottlenecks

set -e

BLUE='\033[0;34m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${BLUE}=== DFS Write Performance Profiling ===${NC}\n"

# Check if servers are running
if ! pgrep -f "dfs-server.*8001" > /dev/null; then
    echo -e "${YELLOW}Server on port 8001 not running${NC}"
    echo "Start servers with: ./scripts/start-local-cluster.sh"
    exit 1
fi

echo -e "${GREEN}Servers detected${NC}\n"

# Test configuration
TEST_SIZE_MB=${1:-10}  # Default 10MB
TEST_FILE="/tmp/dfs_write_test_${TEST_SIZE_MB}mb.bin"
MOUNT_POINT="/mnt/test"

echo "Test configuration:"
echo "  Size: ${TEST_SIZE_MB} MB"
echo "  Test file: $TEST_FILE"
echo ""

# Generate test data
echo -e "${YELLOW}Generating test data...${NC}"
dd if=/dev/zero of="$TEST_FILE" bs=1M count=$TEST_SIZE_MB 2>&1 | tail -1

# Run the write test with detailed logging
echo -e "\n${YELLOW}Running write test...${NC}\n"

# Method 1: Direct write to FUSE mount (if mounted)
if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
    echo -e "${GREEN}Writing via FUSE mount at $MOUNT_POINT${NC}"

    # Capture client logs
    CLIENT_LOG="/tmp/dfs_client_write.log"
    journalctl -u dfs-client -f > "$CLIENT_LOG" 2>&1 &
    CLIENT_LOG_PID=$!

    # Capture server logs
    SERVER_LOG="/tmp/dfs_server_write.log"
    journalctl -u dfs-server@8001 -u dfs-server@8002 -u dfs-server@8003 -f > "$SERVER_LOG" 2>&1 &
    SERVER_LOG_PID=$!

    sleep 1

    # Perform write
    START_TIME=$(date +%s.%N)
    cp "$TEST_FILE" "$MOUNT_POINT/test_write.bin"
    sync
    END_TIME=$(date +%s.%N)

    sleep 2

    # Stop log capture
    kill $CLIENT_LOG_PID $SERVER_LOG_PID 2>/dev/null || true

    # Calculate throughput
    ELAPSED=$(echo "$END_TIME - $START_TIME" | bc)
    THROUGHPUT=$(echo "scale=2; $TEST_SIZE_MB / $ELAPSED" | bc)

    echo -e "\n${GREEN}Write completed${NC}"
    echo "Time: ${ELAPSED}s"
    echo "Throughput: ${THROUGHPUT} MB/s"

    # Analyze logs
    echo -e "\n${BLUE}=== Performance Analysis ===${NC}\n"

    echo "Client-side metrics:"
    grep -E "(Writing|Chunking|Client write|complete|throughput)" "$CLIENT_LOG" 2>/dev/null | tail -20 || echo "No client metrics found"

    echo -e "\nServer-side metrics:"
    grep -E "(Writing|Chunking|Chunk.*complete|quorum|throughput)" "$SERVER_LOG" 2>/dev/null | tail -20 || echo "No server metrics found"

    echo -e "\n${YELLOW}Detailed timing breakdown:${NC}"
    echo "From client logs:"
    grep -E "serialize|send|recv|deserialize" "$CLIENT_LOG" 2>/dev/null | tail -10 || echo "  No timing details found"

    echo -e "\nFrom server logs:"
    grep -E "chunking|metadata|storage" "$SERVER_LOG" 2>/dev/null | tail -10 || echo "  No timing details found"

    # Save logs
    echo -e "\n${GREEN}Logs saved:${NC}"
    echo "  Client: $CLIENT_LOG"
    echo "  Server: $SERVER_LOG"

else
    echo -e "${YELLOW}FUSE not mounted - using direct server connection${NC}"
    echo "Mount the filesystem first with the dfs-client"
fi

# Cleanup
rm -f "$TEST_FILE"

echo -e "\n${BLUE}=== Profiling Complete ===${NC}"
echo ""
echo "To analyze bottlenecks:"
echo "  1. Check 'Chunking' time (Blake3 hashing)"
echo "  2. Check 'serialize/deserialize' time (bincode overhead)"
echo "  3. Check 'send/recv' time (network latency)"
echo "  4. Check 'quorum write' time (disk I/O + metadata)"
echo "  5. Compare total throughput to disk capabilities"
