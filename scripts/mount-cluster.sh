#!/bin/bash
# Mount the local DFS cluster at /tmp/dfs-mount (default) or a specified path

MOUNTPOINT="${1:-/tmp/dfs-mount}"
LOG="/tmp/dfs-client.log"

# Check if servers are running
RUNNING=$(ps aux | grep "[d]fs-server start" | wc -l)
if [ "$RUNNING" -eq 0 ]; then
    echo "✗ Cluster is not running. Run ./scripts/start-cluster.sh first."
    exit 1
fi

# Unmount if already mounted
if mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
    echo "Unmounting existing mount at $MOUNTPOINT..."
    fusermount -u "$MOUNTPOINT" 2>/dev/null || true
    sleep 1
fi

mkdir -p "$MOUNTPOINT"

echo "Mounting DFS cluster at $MOUNTPOINT..."

target/release/dfs-client mount \
    --cluster 127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902 \
    --log-level info \
    --log-file "$LOG" \
    --allow-other \
    "$MOUNTPOINT" &

CLIENT_PID=$!
sleep 2

if mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
    echo "✓ Mounted at $MOUNTPOINT (PID $CLIENT_PID)"
    echo "  Log: $LOG"
    echo "  To unmount: fusermount -u $MOUNTPOINT"
else
    echo "✗ Failed to mount. Check $LOG for details."
    kill $CLIENT_PID 2>/dev/null || true
    exit 1
fi
