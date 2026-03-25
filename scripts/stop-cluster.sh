#!/bin/bash
# Stop all nodes in the DFS cluster

echo "Stopping DFS cluster..."

# Kill all dfs-server processes
pkill -f "dfs-server start" || true

# Kill all dfs-client processes
pkill -f "dfs-client mount" || true

# Unmount FUSE filesystem
fusermount -u /tmp/dfs-mount 2>/dev/null || true

# Clean up PID files
rm -f /tmp/dfs-test/node*/server.pid

echo "✓ DFS cluster stopped"
