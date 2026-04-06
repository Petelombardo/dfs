#!/bin/bash
# Mount the local DFS cluster

if [ -z "$1" ]; then
    echo "Usage: $0 <mountpoint>"
    echo "Example: $0 /tmp/dfs-mount"
    exit 1
fi

MOUNTPOINT="$1"

echo "======================================================================"
echo "  Mounting DFS Cluster"
echo "======================================================================"
echo

# Check if servers are running
RUNNING=$(ps aux | grep "[d]fs-server start" | wc -l)
if [ "$RUNNING" -ne 3 ]; then
    echo "✗ Error: Cluster is not fully running ($RUNNING/3 servers)"
    echo "  Run ./start_local_cluster.sh first"
    exit 1
fi

# Unmount if already mounted
if mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
    echo "Unmounting existing mount at $MOUNTPOINT..."
    fusermount -u "$MOUNTPOINT" 2>/dev/null || true
    sleep 1
fi

# Create mountpoint if it doesn't exist
if [ ! -d "$MOUNTPOINT" ]; then
    echo "Creating mountpoint: $MOUNTPOINT"
    mkdir -p "$MOUNTPOINT"
fi

# Mount the filesystem
echo "Mounting DFS cluster at $MOUNTPOINT..."
echo

target/release/dfs-client mount \
    --cluster 127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902 \
    --foreground \
    --log-level info \
    "$MOUNTPOINT" > /tmp/dfs-client.log 2>&1 &

CLIENT_PID=$!
sleep 2

# Check if mount succeeded
if mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
    echo "======================================================================"
    echo "  Mount Successful"
    echo "======================================================================"
    echo
    echo "Mounted at:     $MOUNTPOINT"
    echo "Cluster nodes:  127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902"
    echo "Client PID:     $CLIENT_PID"
    echo "Client log:     /tmp/dfs-client.log"
    echo
    echo "To unmount:"
    echo "  fusermount -u $MOUNTPOINT"
    echo
    echo "======================================================================"
else
    echo "✗ Error: Failed to mount filesystem"
    echo "  Check /tmp/dfs-client.log for details"
    kill $CLIENT_PID 2>/dev/null || true
    exit 1
fi
