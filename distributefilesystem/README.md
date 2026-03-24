# DFS - Distributed Filesystem

A high-performance, data-loss-resistant distributed filesystem built in Rust using FUSE.

## Features

- **High Performance**: Native Rust implementation with zero-cost abstractions
- **Data Safety**: Configurable replication (default 3x) with checksums and automatic healing
- **KISS Design**: Simple to deploy and manage - single binary, minimal configuration
- **Automatic Healing**: Self-healing with configurable delay (default 300s) to handle reboots
- **Consistent Hashing**: Balanced data distribution across nodes
- **FUSE Integration**: Mount as a standard filesystem

## Project Status

🚧 **Under Active Development** - Phase 1 (Foundation) Complete

See [PROJECT_PLAN.md](PROJECT_PLAN.md) for the full implementation roadmap.

## Architecture

- **dfs-server**: Storage node daemon
- **dfs-client**: FUSE mount client
- **dfs-admin**: Cluster administration tool
- **dfs-common**: Shared library (types, protocol, hashing)

## Quick Start

### Prerequisites

- Rust 1.94+ (install from https://rustup.rs)
- Linux with FUSE support

### Build

```bash
cargo build --release
```

### Configuration

1. Copy example config:
```bash
cp config.example.toml /etc/dfs/config.toml
```

2. Edit configuration:
```bash
# Customize paths, replication factor, etc.
vim /etc/dfs/config.toml
```

### Running (Coming Soon)

```bash
# Initialize first node
dfs-server init --data-dir /mnt/storage

# Start server
dfs-server start

# Join additional nodes to cluster
dfs-admin node add <node-ip>:8900

# Mount filesystem
dfs-client mount /mnt/dfs --cluster node1:8900,node2:8900
```

## Design Decisions

- **Chunk Size**: 4MB (configurable) - balanced for most workloads
- **Replication**: Write to N nodes with quorum acknowledgment
- **Metadata**: Sled embedded database (pure Rust, transactional)
- **Network**: TCP with binary protocol (bincode serialization)
- **Healing Delay**: 300 seconds to avoid premature data movement during reboots

## Development

### Run Tests

```bash
cargo test
```

### Check Code

```bash
cargo clippy
cargo fmt
```

## Contributing

See [PROJECT_PLAN.md](PROJECT_PLAN.md) for the development roadmap and current phase.

## License

MIT License - See LICENSE file for details
