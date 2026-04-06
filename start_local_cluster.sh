#!/bin/bash
# Start local 3-node DFS cluster from /mnt/storage/dfs{1,2,3}

set -e

echo "======================================================================"
echo "  Starting Local DFS Cluster (3 nodes)"
echo "======================================================================"
echo

# Kill any existing servers
echo "[1/3] Stopping any running DFS servers..."
pkill -9 dfs-server 2>/dev/null || true
sleep 1

# Start servers
echo "[2/3] Starting DFS nodes..."

for i in 1 2 3; do
    PORT=$((8899 + i))
    echo "  Starting Node $i on port $PORT..."
    nohup target/release/dfs-server start \
        --config /mnt/storage/dfs$i/config/config.toml \
        --log-level info \
        > /mnt/storage/dfs$i/server.log 2>&1 &
    sleep 0.5
done

sleep 3

# Check status
echo "[3/3] Checking cluster status..."
echo
RUNNING=$(ps aux | grep "[d]fs-server start" | wc -l)

echo "======================================================================"
echo "  Cluster Status"
echo "======================================================================"
echo
echo "Servers running: $RUNNING/3"
echo
echo "Node 1: 127.0.0.1:8900  (data: /mnt/storage/dfs1/data)"
echo "Node 2: 127.0.0.1:8901  (data: /mnt/storage/dfs2/data)"
echo "Node 3: 127.0.0.1:8902  (data: /mnt/storage/dfs3/data)"
echo
echo "Logs:"
echo "  /mnt/storage/dfs1/server.log"
echo "  /mnt/storage/dfs2/server.log"
echo "  /mnt/storage/dfs3/server.log"
echo
echo "Cluster endpoints for client:"
echo "  127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902"
echo
echo "======================================================================"

if [ "$RUNNING" -eq 3 ]; then
    echo "✓ Cluster started successfully!"
    echo
    echo "To mount: ./mount_local_cluster.sh /tmp/dfs-mount"
else
    echo "✗ Warning: Not all servers started ($RUNNING/3 running)"
    echo "   Check logs for errors"
    exit 1
fi
