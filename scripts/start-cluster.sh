#!/bin/bash
# Start all nodes in a DFS cluster
# Usage: ./start-cluster.sh <num_nodes>

NUM_NODES=${1:-3}
BASE_DIR="/tmp/dfs-test"
BASE_PORT=8900

echo "Starting ${NUM_NODES}-node DFS cluster..."
echo ""

# Check if nodes are initialized
if [ ! -d "${BASE_DIR}/node1" ]; then
    echo "Error: Cluster not initialized. Run ./scripts/setup-cluster.sh first"
    exit 1
fi

# Start all nodes in background
for i in $(seq 1 $NUM_NODES); do
    NODE_DIR="${BASE_DIR}/node${i}"
    PORT=$((BASE_PORT + i - 1))

    if [ ! -f "${NODE_DIR}/config.toml" ]; then
        echo "Error: Node ${i} not initialized"
        exit 1
    fi

    echo "Starting node ${i} on port ${PORT}..."
    RUST_LOG=info ./target/release/dfs-server start \
        --config "${NODE_DIR}/config.toml" \
        2>&1 | sed "s/^/[node${i}] /" &

    # Store PID for later
    echo $! > "${NODE_DIR}/server.pid"

    # Small delay between starts
    sleep 0.5
done

echo ""
echo "✓ All ${NUM_NODES} nodes started"
echo ""
echo "Wait a few seconds for cluster to form, then check status:"
echo "  ./target/release/dfs-admin --cluster 127.0.0.1:${BASE_PORT} cluster status"
echo ""
echo "Mount the filesystem:"
echo "  mkdir -p /tmp/dfs-mount"
echo "  ./target/release/dfs-client mount /tmp/dfs-mount --cluster 127.0.0.1:${BASE_PORT}"
echo ""
echo "Stop the cluster:"
echo "  ./scripts/stop-cluster.sh"
