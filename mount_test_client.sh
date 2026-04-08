#!/bin/bash
# Mount DFS client for local testing

set -e

MOUNT_POINT="/tmp/dfs-test-mount"

# Create mount point
mkdir -p "$MOUNT_POINT"

# Check if already mounted
if mountpoint -q "$MOUNT_POINT"; then
    echo "Already mounted at $MOUNT_POINT"
    exit 0
fi

# Mount DFS
echo "Mounting DFS at $MOUNT_POINT..."
./target/release/dfs-client mount \
    --cluster 127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902 \
    --foreground \
    --log-level debug \
    "$MOUNT_POINT" &

# Wait for mount
sleep 2

if mountpoint -q "$MOUNT_POINT"; then
    echo "✓ DFS mounted successfully at $MOUNT_POINT"
else
    echo "✗ Failed to mount DFS"
    exit 1
fi
