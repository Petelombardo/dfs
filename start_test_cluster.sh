#!/bin/bash
# Start local test cluster

# Kill any existing servers
pkill -9 dfs-server 2>/dev/null
rm -rf /tmp/dfs-test

# Create test directories
mkdir -p /tmp/dfs-test/{node1,node2,node3}/{data,metadata}

# Create config files
cat > /tmp/dfs-test/node1/config.toml <<EOF
[node]
listen_addr = "127.0.0.1:8900"

[storage]
data_dir = "/tmp/dfs-test/node1/data"
metadata_dir = "/tmp/dfs-test/node1/metadata"
chunk_size_mb = 4

[cluster]
seed_nodes = []
heartbeat_interval_secs = 10
failure_timeout_secs = 30

[replication]
replication_factor = 3
healing_delay_secs = 300
auto_heal = true
scrub_interval_hours = 24
EOF

cat > /tmp/dfs-test/node2/config.toml <<EOF
[node]
listen_addr = "127.0.0.1:8901"

[storage]
data_dir = "/tmp/dfs-test/node2/data"
metadata_dir = "/tmp/dfs-test/node2/metadata"
chunk_size_mb = 4

[cluster]
seed_nodes = ["127.0.0.1:8900"]
heartbeat_interval_secs = 10
failure_timeout_secs = 30

[replication]
replication_factor = 3
healing_delay_secs = 300
auto_heal = true
scrub_interval_hours = 24
EOF

cat > /tmp/dfs-test/node3/config.toml <<EOF
[node]
listen_addr = "127.0.0.1:8902"

[storage]
data_dir = "/tmp/dfs-test/node3/data"
metadata_dir = "/tmp/dfs-test/node3/metadata"
chunk_size_mb = 4

[cluster]
seed_nodes = ["127.0.0.1:8900"]
heartbeat_interval_secs = 10
failure_timeout_secs = 30

[replication]
replication_factor = 3
healing_delay_secs = 300
auto_heal = true
scrub_interval_hours = 24
EOF

# Start servers
echo "Starting servers..."
for i in 1 2 3; do
    nohup target/release/dfs-server start \
        --config /tmp/dfs-test/node$i/config.toml \
        --log-level debug \
        > /tmp/dfs-server-$i.log 2>&1 &
done

sleep 2
echo "Started servers:"
ps aux | grep dfs-server | grep -v grep
