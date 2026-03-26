# DFS - Distributed File System

A high-performance distributed file system written in Rust with FUSE support, designed for reliability, scalability, and performance.

## Features

- **Distributed Storage**: Data is automatically distributed across multiple nodes using consistent hashing
- **Replication**: Configurable replication factor (default RF=3) ensures data durability
- **FUSE Mount**: Standard filesystem interface - mount as a regular directory
- **Self-Healing**: Automatic data repair and rebalancing on node failures
- **Cluster Resilience**: Persistent peer discovery allows nodes to rejoin after restart
- **Chunk-based Storage**: Files are split into 4MB chunks with BLAKE3 hashing for integrity
- **Performance**: Optimized read/write with caching and parallel operations

## Architecture

### Components

- **dfs-server**: Storage node daemon that stores and serves chunks
- **dfs-client**: FUSE client for mounting the filesystem
- **dfs-admin**: Administrative CLI for cluster management
- **dfs-common**: Shared library with protocols and data structures

### Key Technologies

- Rust for memory safety and performance
- Tokio async runtime for concurrent operations
- FUSE for filesystem integration
- Sled embedded database for metadata
- BLAKE3 for cryptographic hashing
- Bincode for efficient serialization

## Quick Start

### Prerequisites

- Rust 1.70+ (`cargo` and `rustc`)
- Linux with FUSE support

### Build

```bash
cargo build --release
```

### Initialize a Node

```bash
# Initialize storage directories and config
sudo dfs-server init \
  --data-dir /var/lib/dfs/data \
  --meta-dir /var/lib/dfs/metadata \
  --config /etc/dfs/config.toml

# Edit config to set:
# - listen_addr (e.g., "0.0.0.0:8900")
# - seed_nodes (addresses of existing cluster nodes)
```

### Start Server

```bash
sudo dfs-server start --config /etc/dfs/config.toml
```

### Mount Client

```bash
# Mount on a directory
dfs-client mount /mnt/dfs --cluster 10.0.1.10:8900

# Use the filesystem
cd /mnt/dfs
echo "Hello DFS" > test.txt
cat test.txt
```

## Configuration

Example `/etc/dfs/config.toml`:

```toml
[node]
listen_addr = "0.0.0.0:8900"

[storage]
data_dir = "/var/lib/dfs/data"
metadata_dir = "/var/lib/dfs/metadata"
chunk_size_mb = 4

[replication]
replication_factor = 3
auto_heal = true
healing_delay_secs = 30
scrub_interval_hours = 24

[cluster]
heartbeat_interval_secs = 10
failure_timeout_secs = 30
seed_nodes = [
    "10.0.1.10:8900",
    "10.0.1.11:8900",
    "10.0.1.12:8900"
]
```

## Cluster Management

```bash
# View cluster status
dfs-admin cluster status --config /etc/dfs/config.toml

# List all nodes
dfs-admin cluster nodes --config /etc/dfs/config.toml

# Trigger healing check
dfs-admin heal check --config /etc/dfs/config.toml

# View storage statistics
dfs-server status --config /etc/dfs/config.toml
```

## Development Status

✅ **Completed Features**:
- Core distributed storage engine
- Consistent hashing for data placement
- Chunk replication with quorum writes
- FUSE filesystem interface
- Heartbeat-based failure detection
- Automatic healing and rebalancing
- Persistent peer discovery
- Read/write caching

🚧 **In Progress**:
- Performance optimizations (write throughput)
- Admin CLI enhancements
- Graceful node removal

📋 **Planned**:
- Encryption at rest and in transit
- Authentication and authorization
- Compression support
- Web dashboard for monitoring
- Metrics and alerting integration

## Performance

Current benchmarks with 3-node ARM64 cluster:
- **Servers**: 3x Odroid M1S with NVME backend storage
- **Client**: NanoPi R3 (FUSE mount)

Performance:
- **Read**: 23 MB/s (with chunk caching)
- **Write**: 14-15 MB/s (investigating optimizations)

## Documentation

- [Cluster Rejoin Plan](CLUSTER-REJOIN-PLAN.md)
- [Performance Investigation](PERFORMANCE-INVESTIGATION.md)
- [Quick Start Guide](QUICK-START.md)
- [Testing Guide](TESTING.md)

## Contributing

Contributions welcome! This is an experimental project for learning distributed systems concepts.

## License

MIT

## Author

Built with assistance from Claude Code.
