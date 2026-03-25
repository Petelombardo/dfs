#!/bin/bash
# Setup a multi-node DFS cluster for testing
# Usage: ./setup-cluster.sh <num_nodes>

set -e

NUM_NODES=${1:-3}
BASE_DIR="/tmp/dfs-test"
BASE_PORT=8900

echo "Setting up ${NUM_NODES}-node DFS cluster..."

# Clean up any existing test cluster
echo "Cleaning up existing cluster..."
pkill -f "dfs-server" || true
fusermount -u /tmp/dfs-mount 2>/dev/null || true
rm -rf "${BASE_DIR}"
mkdir -p "${BASE_DIR}"

# Initialize nodes
for i in $(seq 1 $NUM_NODES); do
    NODE_DIR="${BASE_DIR}/node${i}"
    PORT=$((BASE_PORT + i - 1))

    echo "Initializing node ${i} on port ${PORT}..."

    ./target/release/dfs-server init \
        --data-dir "${NODE_DIR}/data" \
        --meta-dir "${NODE_DIR}/metadata" \
        --config "${NODE_DIR}/config.toml"

    # Update configuration
    # Set listen address
    sed -i "s/listen_addr = \"0.0.0.0:8900\"/listen_addr = \"0.0.0.0:${PORT}\"/" "${NODE_DIR}/config.toml"

    # For nodes 2+, add node 1 as seed node (replace the empty array in [cluster] section)
    if [ $i -gt 1 ]; then
        sed -i 's/seed_nodes = \[\]/seed_nodes = ["127.0.0.1:'${BASE_PORT}'"]/' "${NODE_DIR}/config.toml"
    fi
done

echo ""
echo "✓ ${NUM_NODES} nodes initialized"
echo ""
echo "Start the cluster with:"
echo "  ./scripts/start-cluster.sh ${NUM_NODES}"
echo ""
echo "Or start manually:"
for i in $(seq 1 $NUM_NODES); do
    PORT=$((BASE_PORT + i - 1))
    echo "  RUST_LOG=info ./target/release/dfs-server start --config ${BASE_DIR}/node${i}/config.toml &"
done
echo ""
echo "Mount the filesystem:"
echo "  mkdir -p /tmp/dfs-mount"
echo "  ./target/release/dfs-client mount /tmp/dfs-mount --cluster 127.0.0.1:${BASE_PORT}"
