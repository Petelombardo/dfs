#!/bin/bash
# Write performance test with DEBUG logging on client AND servers
set -e

REPO=/home/petelombardo/dfs
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-mount
LOG=/tmp/dfs-perf-logs
BIN="$REPO/target/release"

echo "=== Cleaning up ==="
pkill -f "dfs-server" 2>/dev/null || true
pkill -f "dfs-client" 2>/dev/null || true
sleep 0.5
fusermount -u $MOUNT 2>/dev/null || true
sudo rm -rf $BASE $LOG $MOUNT 2>/dev/null || rm -rf $BASE $LOG $MOUNT 2>/dev/null || true
mkdir -p $MOUNT $LOG

echo "=== Starting 3-node cluster with DEBUG logging ==="
bash "$REPO/scripts/setup-cluster.sh" 3 2>/dev/null

# Start servers with DEBUG logging
for i in 1 2 3; do
    echo "  Starting server $i with DEBUG logging..."
    RUST_LOG=debug "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        > "$LOG/server${i}-debug.log" 2>&1 &
done
sleep 3

echo "=== Starting client with DEBUG logging ==="
RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" \
    --cluster "127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902" \
    --log-file "$LOG/client-debug.log" \
    --allow-other \
    --log-level debug &
CLIENT_PID=$!
sleep 2

if ! mountpoint -q "$MOUNT"; then
    echo "MOUNT FAILED"
    tail -50 "$LOG/client-debug.log"
    exit 1
fi

echo "=== Mounted successfully ==="
echo ""

# Pre-generate small test file (just 16MB = 4 chunks for faster iteration)
echo "=== Pre-generating 16MB test file (4 chunks) ==="
dd if=/dev/urandom of=/tmp/perf-test-small.bin bs=1M count=16 2>/dev/null
echo "Done. Test data ready."
echo ""

echo "=== Starting write test at $(date +%H:%M:%S.%3N) ==="
START_TIME=$(date +%s%3N)

# Single large sequential write
cp /tmp/perf-test-small.bin "$MOUNT/perf-test.bin"
sync

END_TIME=$(date +%s%3N)
DURATION_MS=$((END_TIME - START_TIME))
DURATION_SEC=$(echo "scale=3; $DURATION_MS / 1000" | bc)
THROUGHPUT=$(echo "scale=2; 16 / $DURATION_SEC" | bc)

echo "=== Write completed at $(date +%H:%M:%S.%3N) ==="
echo "Duration: ${DURATION_SEC}s"
echo "Throughput: ${THROUGHPUT} MB/s"
echo ""

echo "=== Analyzing logs ==="
echo ""
echo "CLIENT LOG:"
echo "  Total lines: $(wc -l < $LOG/client-debug.log)"
echo "  Write operations: $(grep -c "Writing.*bytes with synchronous" "$LOG/client-debug.log" || echo "0")"
echo ""

echo "SERVER LOGS:"
for i in 1 2 3; do
    LINES=$(wc -l < "$LOG/server${i}-debug.log")
    WRITES=$(grep -c "WriteChunk request" "$LOG/server${i}-debug.log" 2>/dev/null || echo "0")
    echo "  Server $i: $LINES lines, $WRITES WriteChunk requests"
done
echo ""

echo "=== Write timing breakdown (client side) ==="
grep "Writing.*bytes with synchronous" "$LOG/client-debug.log" | while read line; do
    echo "$line"
done
echo ""

echo "=== Write completion timing (client side) ==="
grep "Dual-replica write complete" "$LOG/client-debug.log" | while read line; do
    echo "$line"
done
echo ""

echo "=== Server 1 WriteChunk requests ==="
grep "WriteChunk request" "$LOG/server1-debug.log" 2>/dev/null | head -10 || echo "None found"
echo ""

echo "=== Logs saved to $LOG ==="
echo "  - client-debug.log"
echo "  - server{1,2,3}-debug.log"
echo ""
echo "Next steps:"
echo "  1. Check time between 'Writing' and 'write complete' messages"
echo "  2. Check server logs for slow operations"
echo "  3. Look for lock contention or blocking"
