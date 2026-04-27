#!/bin/bash
# Simple write performance test with debug logging
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

echo "=== Starting 3-node cluster ==="
bash "$REPO/scripts/setup-cluster.sh" 3 2>/dev/null

for i in 1 2 3; do
    RUST_LOG=debug "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        > "$LOG/server${i}.log" 2>&1 &
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

# Pre-generate test data to eliminate /dev/urandom overhead
echo "=== Pre-generating 40MB test file (10 chunks) ==="
dd if=/dev/urandom of=/tmp/perf-test-data.bin bs=1M count=40 2>/dev/null
echo "Done. Test data ready."
echo ""

echo "=== Starting write test at $(date +%H:%M:%S.%3N) ==="
START_TIME=$(date +%s%3N)

# Single large sequential write
time cp /tmp/perf-test-data.bin "$MOUNT/perf-test.bin"
sync

END_TIME=$(date +%s%3N)
DURATION_MS=$((END_TIME - START_TIME))
DURATION_SEC=$(echo "scale=3; $DURATION_MS / 1000" | bc)
THROUGHPUT=$(echo "scale=2; 40 / $DURATION_SEC" | bc)

echo "=== Write completed at $(date +%H:%M:%S.%3N) ==="
echo "Duration: ${DURATION_SEC}s"
echo "Throughput: ${THROUGHPUT} MB/s"
echo ""

echo "=== Analyzing debug log ==="
echo "Total log lines: $(wc -l < $LOG/client-debug.log)"
echo ""
echo "Write operations:"
grep -c "Writing.*bytes with synchronous" "$LOG/client-debug.log" || echo "0"
echo ""
echo "First 5 writes:"
grep "Writing.*bytes with synchronous" "$LOG/client-debug.log" | head -5
echo ""
echo "Last 5 writes:"
grep "Writing.*bytes with synchronous" "$LOG/client-debug.log" | tail -5
echo ""

echo "=== Logs saved to $LOG ==="
echo "  - client-debug.log (full debug output)"
echo "  - server{1,2,3}.log"
echo ""
echo "Use 'grep' to analyze timing:"
echo "  grep 'Writing.*bytes' $LOG/client-debug.log"
echo "  grep 'Dual-replica write complete' $LOG/client-debug.log"
