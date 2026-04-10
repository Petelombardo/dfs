# DFS — Distributed File System

A high-performance distributed file system written in Rust with FUSE support. Designed for reliability and speed on real hardware — currently running a 5-node ARM64 cluster serving a live HDHomeRun DVR.

## Features

- **Distributed storage** across N nodes via consistent hashing
- **Configurable replication** (default RF=3) with automatic healing
- **Leader-coordinated healing** — a single elected leader coordinates all replica repair and cleanup, eliminating duplicate work and split-brain corruption
- **FUSE mount** — appears as a normal directory to any application
- **SQLite-aware I/O** — disables direct I/O for `.db-shm` files so SQLite shared-memory mmap works correctly
- **Write-behind buffer** — sequential writes (e.g. DVR recording) are buffered and flushed as full 4MB chunks for HDD efficiency
- **Seek optimization** — partial-chunk reads on seek so video players don't wait for a full 4MB chunk before playing
- **Connection pooling** — per-peer TCP connection pools on both client and server; bounded with 5s connect timeouts to prevent fd exhaustion under load
- **Chunk-level cache warming** — sliding window of 1000 chunks ahead of current read position pre-cached in memory
- **Permanent missing-chunk blocklist** — unrecoverable chunks are detected after one failed round-trip to all online nodes and never retried, preventing connection storms
- **Capacity-aware placement** — new chunks go to the nodes with the most free space
- **Gossip-based failure detection** — heartbeats carry cluster-wide health gossip for fast convergence after rolling restarts

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    dfs-client (FUSE)                     │
│  - Presents a normal filesystem via /mnt/...             │
│  - Chunks files into 4MB pieces on write                 │
│  - Fetches chunk map from leader on open                 │
│  - Reads: parallel fetches with shared 20-slot semaphore │
│  - Writes: buffered → flush on full chunk or close       │
└────────────────────────┬────────────────────────────────┘
                         │ TCP :8900
         ┌───────────────┼───────────────┐
         ▼               ▼               ▼
   ┌──────────┐    ┌──────────┐    ┌──────────┐
   │ dfs-server│   │dfs-server│   │dfs-server│  ...up to N nodes
   │ (leader) │    │          │   │          │
   └──────────┘    └──────────┘   └──────────┘
        │
        │  Leader responsibilities (min-NodeId wins):
        │  • Serves GetFileChunkMap to clients
        │  • Coordinates healing (under/over replication)
        │  • Issues DeleteChunkReplica for cleanup
        │
        └── All nodes: store chunks, serve reads,
            heartbeat every 10s with gossip payload
```

### Leader Election

Leadership requires no external coordination. Every node independently computes:

> **leader = online node with the minimum NodeId**

NodeIds are stable UUIDs persisted to disk — they survive restarts. On every heartbeat, each node re-adds the sender to its cluster view (`add_node` is idempotent), ensuring purged or failed nodes are re-discovered as soon as they come back online. With 5s heartbeats and a 30s failure timeout, leadership converges within one heartbeat cycle after any topology change.

### Healing

The leader runs a healing check every 60 seconds, capped at **10 operations per cycle** with a **200ms pause between each** to avoid connection storms. Both under-replication (add a replica) and over-replication (remove a replica) are throttled by the same budget. Failed heal attempts count toward the budget too, so a permanently-missing chunk can't spin the loop.

### Chunk Storage

- Files are split into 4MB chunks, each identified by a BLAKE3 hash of its content
- Chunk locations are stored in a per-node Sled embedded database
- The leader maintains an in-memory `FileId → Vec<ChunkLocation>` map for fast `GetFileChunkMap` responses; all nodes keep it current so leadership handoff is seamless
- Chunks are stored as flat files under `data_dir/`

## Staging Cluster

```
Storage nodes (dfs-server):
  node1  192.168.1.10:8900   (leader — lowest NodeId)
  node2  192.168.1.11:8900
  node3  192.168.1.12:8900
  node4  192.168.1.13:8900
  node5  192.168.1.14:8900

Client nodes (dfs-client, FUSE mount at /mnt/test):
  client1     192.168.1.20
  client2     192.168.1.21
  client3     192.168.1.22

Data/config path on each storage node: /mnt/dfs/
Active workload: HDHomeRun DVR via Kodi/Emby at /mnt/test/podman/dvr/
```

## Building

```bash
cargo build --release
```

Binaries are output to `target/release/`: `dfs-server`, `dfs-client`, `dfs-admin`.

## Deploying

### Deploy to existing nodes

```bash
# Rebuild and push to all 5 storage nodes + all client nodes
./deploy-build
```

### Add a new storage node

```bash
# First node (new cluster)
./deploy-node.sh 192.168.1.10

# Additional node joining an existing cluster
./deploy-node.sh 192.168.1.15 192.168.1.10
```

`deploy-node.sh` will:
1. Copy `dfs-server` and `dfs-admin` binaries to the node
2. Create the data/metadata/config directory layout under `/mnt/dfs/`
3. Write `config.toml` with the node's IP and seed node address
4. Install and enable the systemd unit with `LimitNOFILE=65536`
5. Start the service and verify it's active

The new node will contact the seed, exchange heartbeats with the full cluster, and begin receiving chunk replicas automatically.

### Add a new client node

1. Copy `dfs-client` to `/usr/bin/dfs-client` on the client machine
2. Install the systemd unit:

```ini
[Unit]
Description=DFS FUSE Client
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/bin/dfs-client mount /mnt/test \
    --cluster 192.168.1.10 \
    --log-level info \
    --allow-other \
    --log-file /var/log/dfs-client.log
Restart=on-failure
RestartSec=5
CapabilityBoundingSet=CAP_SYS_ADMIN
AmbientCapabilities=CAP_SYS_ADMIN
ExecStop=/bin/fusermount -u /mnt/test
TimeoutStopSec=10

[Install]
WantedBy=multi-user.target
```

The `--cluster` flag accepts any storage node address; the client will discover the rest of the cluster automatically.

## Configuration Reference

`/mnt/dfs/config/config.toml`:

```toml
[node]
listen_addr = "192.168.1.10:8900"   # this node's IP:port

[storage]
data_dir      = "/mnt/dfs/data"
metadata_dir  = "/mnt/dfs/metadata"
chunk_size_mb = 4

[cluster]
seed_nodes             = ["192.168.1.11:8900"]  # any existing node; empty for first node
heartbeat_interval_secs = 10
failure_timeout_secs    = 30

[replication]
replication_factor  = 3
healing_delay_secs  = 300   # wait 5min before healing to let transient failures resolve
auto_heal           = true
scrub_interval_hours = 24
```

## Local Development

Run a 3-node cluster on your dev machine:

```bash
cargo build --release
./start_local_cluster.sh      # starts nodes on :8900, :8901, :8902
./mount_local_cluster.sh /tmp/dfs-mount
```

Logs go to `/mnt/storage/dfs{1,2,3}/server.log`.

## Administration

```bash
# Cluster health
dfs-admin -c 192.168.1.10:8900 cluster status

# List all files
dfs-admin -c 192.168.1.10:8900 file list

# Inspect a file's chunk locations
dfs-admin -c 192.168.1.10:8900 file info '/podman/dvr/recordings/Today/Today.mpg'

# Trigger a healing pass (leader only)
dfs-admin -c 192.168.1.10:8900 healing trigger

# Purge corrupt file metadata (leaves chunk data intact)
dfs-admin -c 192.168.1.10:8900 file purge '/podman/dvr/recordings/Today/Today.mpg'
```

## Performance

5-node ARM64 cluster, spinning HDD backend:

| Operation | Throughput |
|-----------|-----------|
| Sequential read (DVR playback) | ~90 MB/s |
| Sequential write (DVR recording) | ~45 MB/s |
| Random read (seek / fast-forward) | ~25 MB/s |

## Known Limitations

- No encryption at rest or in transit
- No authentication — trust your network
- Chunk blocklist is in-memory only; clears on restart (by design — lets recovered nodes be re-checked)
- Graceful node removal (draining replicas before shutdown) not yet implemented

## License

MIT
