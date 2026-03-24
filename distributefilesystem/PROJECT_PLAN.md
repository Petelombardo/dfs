# Distributed Filesystem (DFS) - Project Plan

## Project Overview
A high-performance, data-loss-resistant distributed filesystem built in Rust using FUSE.

## Design Principles
- **KISS** (Keep It Simple, Stupid)
- High performance with minimal data loss risk
- Easy node management (add/remove)
- Automatic healing with delayed start (300s)
- Simple UX for administration

## Core Architecture Decisions

### System Design
- **Language**: Rust (compiled to native executables)
- **Deployment**: Native systemd services (not containers)
- **Replication**: Default 3 copies, configurable
- **Metadata**: Sled embedded database (pure Rust)
- **Network**: TCP with binary protocol (tokio async)
- **Placement**: Consistent hashing ring
- **Min Cluster**: 1 node (testing), 3 nodes (production)

### Data Storage
- **Data Path**: Configurable, default `/var/lib/dfs/data/`
- **Metadata Path**: Configurable, default `/var/lib/dfs/metadata/`
- **Config Path**: `/etc/dfs/config.toml`
- **Chunk Storage**: `{data-dir}/chunks/{hash[0:2]}/{hash[2:4]}/{full-hash}`

### Healing & Rebalancing
- 300 second delay before healing starts (accounts for reboots)
- Automatic cleanup of excess replicas
- Rebalance on node add/remove only
- Background scrubbing for verification

## Project Structure

```
distributefilesystem/
├── Cargo.toml              # Workspace root
├── dfs-common/             # Shared library
│   ├── src/
│   │   ├── lib.rs
│   │   ├── types.rs        # Common types
│   │   ├── protocol.rs     # Network protocol
│   │   ├── hash.rs         # Consistent hashing
│   │   └── config.rs       # Configuration
│   └── Cargo.toml
├── dfs-server/             # Storage node daemon
│   ├── src/
│   │   ├── main.rs
│   │   ├── storage.rs      # Local storage management
│   │   ├── metadata.rs     # Metadata operations
│   │   ├── network.rs      # Node-to-node communication
│   │   ├── replication.rs  # Replication logic
│   │   ├── healing.rs      # Self-healing operations
│   │   └── cluster.rs      # Cluster membership
│   └── Cargo.toml
├── dfs-client/             # FUSE mount client
│   ├── src/
│   │   ├── main.rs
│   │   ├── fuse_impl.rs    # FUSE operations
│   │   └── client.rs       # Network client
│   └── Cargo.toml
├── dfs-admin/              # Admin CLI tool
│   ├── src/
│   │   └── main.rs
│   └── Cargo.toml
├── PROJECT_PLAN.md         # This file
├── PROGRESS.md             # Implementation progress tracker
└── README.md               # User documentation
```

## Implementation Phases

### Phase 1: Foundation (Days 1-2)
**Goal**: Basic project structure and core types

- [ ] Install Rust toolchain
- [ ] Create Cargo workspace
- [ ] Define common types (NodeId, ChunkId, FileMetadata)
- [ ] Implement configuration parsing (config.toml)
- [ ] Create consistent hashing ring
- [ ] Define network protocol messages (binary serialization)
- [ ] Basic logging setup

**Deliverable**: Compiled binaries (empty functionality)

### Phase 2: Local Storage (Days 3-4)
**Goal**: Single node can store and retrieve data

- [ ] Implement chunk storage on local filesystem
- [ ] Sled metadata database integration
- [ ] File chunking and reassembly
- [ ] Checksum generation and verification
- [ ] Local CRUD operations (create, read, update, delete)
- [ ] Write-ahead logging for durability

**Deliverable**: Single-node storage works

### Phase 3: Network Layer (Days 5-6)
**Goal**: Nodes can communicate

- [ ] TCP server/client with tokio
- [ ] Binary protocol implementation (using bincode)
- [ ] Connection pooling
- [ ] Node-to-node RPC (remote procedure calls)
- [ ] Heartbeat/gossip protocol for failure detection
- [ ] Cluster membership management

**Deliverable**: Nodes can talk to each other

### Phase 4: Distributed Operations (Days 7-9)
**Goal**: Multi-node storage and retrieval

- [ ] Distributed write (replicate to N nodes)
- [ ] Quorum reads (read from any replica)
- [ ] Proxy requests to correct nodes
- [ ] Consistent hashing placement
- [ ] Metadata synchronization across nodes
- [ ] Transaction coordination

**Deliverable**: Multi-node cluster stores data

### Phase 5: Replication & Healing (Days 10-12)
**Goal**: Automatic data protection

- [ ] Monitor replication factor
- [ ] Detect under-replicated chunks
- [ ] 300-second delay timer before healing
- [ ] Re-replicate missing data
- [ ] Detect and cleanup excess replicas
- [ ] Background scrubbing
- [ ] Self-healing automation

**Deliverable**: System survives node failures

### Phase 6: FUSE Client (Days 13-15)
**Goal**: Mount as a filesystem

- [ ] FUSE integration (using `fuser` crate)
- [ ] Basic operations (open, read, write, close)
- [ ] Directory operations (readdir, mkdir, rmdir)
- [ ] Metadata operations (stat, chmod, chown)
- [ ] Client-side caching
- [ ] Handle node failures gracefully

**Deliverable**: Can mount and use as normal filesystem

### Phase 7: Admin Tools (Days 16-17)
**Goal**: Easy cluster management

- [ ] `dfs-server init` - initialize node
- [ ] `dfs-server start` - start daemon
- [ ] `dfs-admin cluster status` - show cluster health
- [ ] `dfs-admin node add` - add node to cluster
- [ ] `dfs-admin node remove` - remove node (triggers rebalance)
- [ ] `dfs-client mount` - mount filesystem
- [ ] `dfs-client unmount` - unmount filesystem

**Deliverable**: Simple CLI for all operations

### Phase 8: Testing & Refinement (Days 18-20)
**Goal**: Production-ready reliability

- [ ] Unit tests for core components
- [ ] Integration tests (multi-node scenarios)
- [ ] Chaos testing (random node failures)
- [ ] Performance benchmarking
- [ ] Memory leak testing
- [ ] Documentation improvements
- [ ] Error message improvements

**Deliverable**: Stable, tested system

### Phase 9: Performance Optimization (Days 21-23)
**Goal**: Maximum throughput

- [ ] Profile hot paths
- [ ] Optimize chunk size
- [ ] Parallel I/O operations
- [ ] Zero-copy optimizations
- [ ] Connection pooling tuning
- [ ] Async I/O improvements
- [ ] Memory usage optimization

**Deliverable**: Fast distributed filesystem

### Phase 10: Production Features (Days 24-25)
**Goal**: Nice-to-haves

- [ ] Systemd service files
- [ ] Installation scripts
- [ ] Configuration examples
- [ ] Monitoring/metrics endpoint
- [ ] Optional TLS support
- [ ] Rack/zone awareness (placement policies)
- [ ] Volume/namespace isolation

**Deliverable**: Production-ready deployment

## Key Features Summary

### Must-Have (MVP)
✅ Multi-node distributed storage
✅ Configurable replication (default 3)
✅ Automatic healing (300s delay)
✅ FUSE mount client
✅ Simple add/remove nodes
✅ Consistent hashing placement
✅ Checksums and verification
✅ Crash recovery (WAL)

### Nice-to-Have (Future)
- TLS encryption
- Erasure coding (space efficiency)
- Snapshots
- Compression
- Quotas
- Access control lists
- Web dashboard
- Prometheus metrics

## Success Criteria

1. **Correctness**: No data loss under normal failures
2. **Performance**: Match or exceed GlusterFS baseline
3. **Simplicity**: Single binary, simple commands
4. **Reliability**: Automatic recovery from node failures
5. **Usability**: Clear error messages, good UX

## Timeline Estimate

- **Aggressive**: 15-20 days (full-time work)
- **Realistic**: 4-6 weeks (part-time work)
- **Conservative**: 2-3 months (learning + building)

Since we're building together, we'll iterate quickly!

## Next Steps

1. Install Rust toolchain
2. Create Cargo workspace
3. Begin Phase 1 implementation
4. Test as we build each component

---

**Last Updated**: 2026-03-24
**Status**: Planning Phase
