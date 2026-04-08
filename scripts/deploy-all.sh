#!/bin/bash
set -e

echo "Deploying instrumented binaries..."

# Deploy server to all gluster nodes
for node in gluster1 gluster2 gluster3; do
    echo "Deploying to $node..."
    scp target/release/dfs-server root@$node:/usr/local/bin/dfs-server
    ssh root@$node "systemctl restart dfs-server"
done

# Deploy client
echo "Deploying client to nanopir3..."
scp target/release/dfs-client root@nanopir3:/usr/local/bin/dfs-client

echo "Deployment complete!"
