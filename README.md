# DFS — Distributed File System

A high-performance distributed file system written in Rust with FUSE support. Designed for reliability and speed on real hardware — currently running a 5-node ARM64 cluster serving a live HDHomeRun DVR and Proxmox VM disk images.

## Features

- **Distributed storage** across N nodes with configurable replication (default RF=3)
- **FUSE mount** — appears as a normal directory to any application
- **Leader election** — minimum-NodeId wins; no external coordinator required
- **Automatic healing** — leader detects and repairs under/over-replicated chunks
- **Write-behind buffer** — sequential writes buffered into full 4MB chunks for HDD efficiency
- **Per-chunk write serialization** — `(ino, chunk_idx)` mutex ensures correct ordering across all write paths; different chunks on the same file proceed in parallel
- **Optimistic PatchChunk protocol** — server validates chunk ID on receipt and returns `ChunkStale` with the correct ID if the client is behind; client retries in one round-trip
- **Multi-path direct I/O** — SQLite databases, sparse writes, and VM disk patches bypass the write buffer and go direct to PatchChunk/WriteChunk
- **MultiPatch RPC** — multiple dirty byte ranges within one chunk are sent in a single request; server applies atomically, client optionally pre-computes the post-patch hash to skip the server's read-back pass
- **Runtime isolation** — reads, writes, and flushes run on separate Tokio runtimes so a slow path can't starve others
- **SQLite-aware I/O** — direct I/O for `.db`/`.sqlite` with kernel page-cache for `.db-shm` (mmap compatibility)
- **Connection pooling** — per-peer TCP pools on client and server; bounded with timeouts to prevent fd exhaustion
- **Gossip-based failure detection** — heartbeats carry cluster-wide health for fast convergence
- **Ghost-node pruning** — stale replica references removed after 300s of confirmed absence
- **Permanent missing-chunk blocklist** — unrecoverable chunks are never retried, preventing connection storms

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                     dfs-client (FUSE)                        │
│                                                              │
│  Write paths (all serialized per (ino, chunk_idx)):          │
│   • Sequential append  → write buffer → 4MB flush            │
│   • Overwrite (buffered)→ write buffer → MultiPatch/Patch    │
│   • Sparse write       → PatchChunk (existing chunk) or      │
│                          WriteChunk (new region)             │
│   • SQLite random write→ read-modify-write → WriteChunk      │
│   • VM disk patch      → write buffer → MultiPatch           │
│                                                              │
│  Read paths:                                                 │
│   • Sequential: pipelined prefetch, sliding window cache     │
│   • Random: per-chunk fetch with Moka chunk cache            │
│   • Live recording: chunk map updated on each open()         │
│                                                              │
│  Metadata:                                                   │
│   • Batched queue — coalesces rapid updates, TTL=1 broadcast │
│   • write_seq counter prevents stale overwrites              │
└─────────────────────┬────────────────────────────────────────┘
                      │ TCP :8900
        ┌─────────────┼─────────────┐
        ▼             ▼             ▼
  ┌──────────┐  ┌──────────┐  ┌──────────┐
  │dfs-server│  │dfs-server│  │dfs-server│  ...up to N nodes
  │ (leader) │  │          │  │          │
  └──────────┘  └──────────┘  └──────────┘
       │
       │  Leader responsibilities:
       │  • Authoritative chunk map (FileId → Vec<ChunkLocation>)
       │  • Healing coordination (under/over-replication)
       │  • Metadata sequence arbiter
       │
       └── All nodes: store chunks, serve reads/writes,
           maintain local chunk map, heartbeat every 10s
```

### Write Path Detail

Every write to a file goes through one of four paths depending on context. All paths acquire a per-`(ino, chunk_idx)` mutex before the network call and hold it through the `metadata_cache` update, ensuring that concurrent writes to the same chunk from different paths are serialized in arrival order. Writes to different chunks of the same file proceed in parallel.

**1. Sequential append (DVR streaming, large file writes)**
The kernel delivers writes sequentially. The client buffers them into 4MB slots. When a slot fills, the flush worker sends it to two replica nodes in parallel via `WriteChunk`. The background ticker also flushes partial slots on a 50ms interval or when `release()`/`fsync()` is called.

**2. In-place overwrite of existing chunks (VM disk patching, Kodi metadata)**
The write buffer accumulates dirty byte ranges within the slot. On flush, the client computes the post-patch content locally (from chunk cache or a single replica fetch), then sends all dirty ranges in one `MultiPatch` RPC. The server applies them atomically and renames the chunk file — no read-back needed when the client pre-computes the hash.

**3. Sparse write (jump past EOF)**
If the write offset is within an already-committed chunk, `PatchChunk` patches it in place. If it's beyond all chunks, `WriteChunk` stores a new chunk at that offset.

**4. SQLite random write**
Reads all affected chunks, merges the new data, and writes back the result via `WriteChunk`. Locks all affected chunk indices before reading to prevent a concurrent write from interleaving.

### Optimistic PatchChunk Protocol

Every `PatchChunk` and `MultiPatch` request now carries `file_id` and `chunk_idx` in addition to the chunk hash. On receipt, the server checks its local chunk map:

- **Match** → apply the patch, return `PatchChunkResult { new_chunk_id }` — one round-trip, same as before
- **Mismatch** → return `ChunkStale { current_chunk_id, current_nodes }` without applying anything

On `ChunkStale`, the client updates its local chunk map and retries once with the corrected ID. In practice, conflicts only occur when two sessions patch the same chunk concurrently; the per-chunk mutex prevents this on the same client, and `ChunkStale` handles cross-client conflicts.

### Metadata Queue

Metadata updates are batched in a FIFO queue (one entry per file, deduplicated by file_id). The queue drains to the leader synchronously on `release()` and `fsync()`, and on a 100ms batched broadcast to followers. Each update carries a `write_seq` counter; the server rejects any update with a lower sequence than its current record, preventing stale overwrites from racing flushes.

### Leader Election

Leadership requires no external coordination. Every node independently computes:

> **leader = online node with the minimum NodeId**

NodeIds are stable UUIDs persisted to disk. On every heartbeat, each node re-adds the sender to its cluster view — `add_node` is idempotent, so purged or failed nodes are re-discovered as soon as they come back online.

### Healing

The leader runs a healing check every 60 seconds, processing up to 1000 chunks per cycle with 2 ops concurrently and a 50ms cooldown between each. On gigabit, a 4MB chunk transfers in ~33ms; a 20GB file heals in ~2 minutes. Ghost-node references (chunks whose known replicas all report "not found") are moved to the stalled set after 300s and removed from the active healing queue so they don't spin the loop.

### Chunk Storage

- Files split into 4MB chunks, each identified by a BLAKE3 hash seeded with the chunk's file offset (position-aware — the same data at different offsets produces different IDs)
- Chunk files stored flat under `data_dir/`
- Per-node Sled embedded database persists chunk locations; leader maintains an in-memory `FileId → Vec<ChunkLocation>` map for fast metadata responses
- All nodes keep the chunk map current so leadership handoff is seamless

## Staging Cluster

```
Storage nodes (dfs-server, /mnt/gluster/dfs/):
  gluster1  10.25.1.58:8900  (typically leader)
  gluster2  10.25.1.57:8900
  gluster3  10.25.1.60:8900
  gluster4  10.25.1.64:8900
  gluster5  10.25.1.100:8900

Client (dfs-client, FUSE mount at /mnt/test):
  nanopir3  10.25.1.x

Active workloads:
  /mnt/test/podman/dvr/          HDHomeRun DVR via podman-compose
  /mnt/test/images/              Proxmox VM disk images (raw + qcow2)
```

## Building

```bash
cargo build --release
```

Binaries output to `target/release/`: `dfs-server`, `dfs-client`, `dfs-admin`.

## Local Development & Testing

```bash
# Full 5-node local suite (T1–T22), mounts at /tmp/dfs-mount
bash scripts/test_local_suite.sh
```

Test logs go to `/tmp/dfs-test-logs/`. Per-test snapshots are written to `T<N>.log` for isolated post-mortem debugging. The client runs at debug log level.

## Deploying

```bash
# Copy binaries to storage nodes and restart via systemd
for host in gluster1 gluster2 gluster3 gluster4 gluster5; do
  scp target/release/dfs-server root@${host}.local:/usr/local/bin/dfs-server
  ssh root@${host}.local systemctl restart dfs-server
done

# Copy client binary and restart
scp target/release/dfs-client root@nanopir3.local:/usr/local/bin/dfs-client
ssh root@nanopir3.local systemctl restart dfs-client
```

Always ask before touching staging.

## Administration

```bash
# Cluster health
dfs-admin --cluster 10.25.1.58:8900 cluster status

# Healing status
dfs-admin --cluster 10.25.1.58:8900 healing status

# Trigger immediate healing pass
dfs-admin --cluster 10.25.1.58:8900 healing trigger

# Rebuild chunk map from on-disk file records (clears ghost references)
dfs-admin --cluster 10.25.1.58:8900 healing repair

# List all files
dfs-admin --cluster 10.25.1.58:8900 file list

# Inspect a file's chunk locations
dfs-admin --cluster 10.25.1.58:8900 file info '/podman/dvr/recordings/show.mpg'

# Trigger healing for a specific file
dfs-admin --cluster 10.25.1.58:8900 healing file '/podman/dvr/recordings/show.mpg'
```

## Environment Variables

All env vars are read at server startup. Restart `dfs-server` after changing them.

| Variable | Default | Description |
|----------|---------|-------------|
| `DFS_HEAL_BANDWIDTH_MB` | `32` | Initial token-bucket rate for healer chunk transfers (MB/s). The adaptive controller adjusts this at runtime — this value is used only for the initial rate before the controller's first evaluation. |
| `DFS_HEAL_MAX_CONCURRENT` | `8` | Maximum number of simultaneous outbound heal chunk transfers. Caps concurrency independently of bandwidth pacing. |
| `DFS_HEAL_TRANSFER_TIMEOUT_SECS` | `120` | Per-chunk heal transfer timeout in seconds. Timed-out chunks remain in the pending queue and are retried on the next drain cycle. |
| `DFS_LINK_BANDWIDTH_MB` | `100` | Assumed node-to-node link capacity in MB/s (1 Gbps ≈ 100 MB/s). Used as the 100% baseline for the adaptive bandwidth formula. |
| `DFS_HEAL_MAX_PCT` | `60` | Maximum percentage of link bandwidth the healer may use. The default of 60% is the logical ceiling when client and heal traffic share a single interface — at 60% the healer consumes more bandwidth than writes can ever produce, so the queue cannot grow unboundedly. Set higher (e.g. `90`) when storage nodes have a dedicated server-to-server interface that is separate from the client-facing interface. Accepts integer values 10–100. |

### Adaptive Heal Bandwidth

The healer automatically scales its bandwidth between 10% and `DFS_HEAL_MAX_PCT` based on queue depth and growth rate:

- **< 100 chunks pending**: floor rate (10%) — trivially small queue, don't compete with writes
- **100–1000 chunks**: proportional scale 10% → max%, boosted if the queue is growing fast
- **≥ 1000 chunks**: maximum rate — queue is deep enough that durability takes priority

The current rate is visible in `dfs-admin healing status` as `Bandwidth: NMB/s`.

## Performance

5-node ARM64 cluster (OrangePi 5), spinning HDD backend, GbE network:

| Operation | Throughput |
|-----------|------------|
| Sequential read (DVR playback) | ~87 MB/s |
| Sequential write (DVR recording) | ~44 MB/s |
| Random 12KB patch (VM disk) | ~17ms/patch |

Sequential reads are served from the Moka chunk cache when warm; cold reads are network-bound. Writes use the runtime-isolated flush pipeline so concurrent reads don't stall write acknowledgements.

## Known Limitations

- No encryption at rest or in transit
- No authentication — trust your network
- Chunk blocklist is in-memory only; clears on restart (by design — lets recovered nodes be re-checked)
- Graceful node removal (draining replicas before shutdown) not yet implemented
- `qcow2` disk images work but are not as well tested as raw images

## License

MIT
