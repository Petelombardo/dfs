# DFS API Reference

## Overview
This document provides a complete reference for all public APIs, key functions, and integration points in the DFS system. Use this to understand how components interact and how to implement clients.

---

## Core Types (dfs-common)

### NodeId
```rust
pub struct NodeId(pub Uuid);
impl NodeId {
    pub fn new() -> Self;
    pub fn from_uuid(uuid: Uuid) -> Self;
}
```
Unique identifier for each node in the cluster.

### ChunkId
```rust
pub struct ChunkId {
    pub hash: [u8; 32],  // Blake3 hash
}
impl ChunkId {
    pub fn from_hash(hash: [u8; 32]) -> Self;
    pub fn to_hex(&self) -> String;
    pub fn storage_path_components(&self) -> (String, String, String);
}
```
Unique identifier for data chunks (based on content hash).

### FileId
```rust
pub struct FileId(pub Uuid);
```
Unique identifier for files in the filesystem.

### FileMetadata
```rust
pub struct FileMetadata {
    pub id: FileId,
    pub path: String,
    pub size: u64,
    pub chunks: Vec<ChunkId>,
    pub created_at: u64,
    pub modified_at: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub file_type: FileType,
}
impl FileMetadata {
    pub fn new(path: String, file_type: FileType) -> Self;
}
```

### FileType
```rust
pub enum FileType {
    RegularFile,
    Directory,
    Symlink,
}
```

### NodeInfo
```rust
pub struct NodeInfo {
    pub id: NodeId,
    pub addr: SocketAddr,
    pub name: Option<String>,
    pub status: NodeStatus,
    pub last_heartbeat: u64,
}
impl NodeInfo {
    pub fn new(id: NodeId, addr: SocketAddr, name: Option<String>) -> Self;
    pub fn update_heartbeat(&mut self);
    pub fn is_failed(&self, timeout_secs: u64) -> bool;
}
```

### NodeStatus
```rust
pub enum NodeStatus {
    Online,
    Suspected,
    Failed,
    Leaving,
}
```

### ChunkLocation
```rust
pub struct ChunkLocation {
    pub chunk_id: ChunkId,
    pub nodes: Vec<NodeId>,
    pub size: usize,
    pub checksum: [u8; 32],
}
```

---

## Network Protocol (dfs-common)

### Message Types
```rust
pub enum Message {
    Request(Request),
    Response(Response),
    Cluster(ClusterMessage),
}
```

### Request
```rust
pub enum Request {
    ReadChunk { chunk_id: ChunkId },
    WriteChunk { chunk_id: ChunkId, data: Vec<u8>, checksum: [u8; 32] },
    DeleteChunk { chunk_id: ChunkId },
    HasChunk { chunk_id: ChunkId },
    ReplicateChunk { chunk_id: ChunkId, data: Vec<u8>, checksum: [u8; 32] },
    GetFileMetadata { file_id: FileId },
    UpdateFileMetadata { metadata: FileMetadata },
    ListDirectory { path: String },
}
```

### Response
```rust
pub enum Response {
    Ok { data: Option<Vec<u8>> },
    ChunkData { chunk_id: ChunkId, data: Vec<u8> },
    FileMetadata { metadata: FileMetadata },
    DirectoryListing { entries: Vec<FileMetadata> },
    Bool { value: bool },
    Error { message: String, code: ErrorCode },
}
```

### ClusterMessage
```rust
pub enum ClusterMessage {
    Heartbeat { node_info: NodeInfo },
    Join { node_info: NodeInfo },
    Leave { node_id: NodeId },
    NodeFailed { node_id: NodeId },
    NodeRecovered { node_id: NodeId },
    GetClusterInfo,
    ClusterInfo { nodes: Vec<NodeInfo> },
    MetadataUpdate { metadata: FileMetadata, operation: MetadataOperation },
}
```

### MessageEnvelope
```rust
pub struct MessageEnvelope {
    pub request_id: RequestId,
    pub message: Message,
}
impl MessageEnvelope {
    pub fn new(request_id: RequestId, message: Message) -> Self;
    pub fn to_bytes(&self) -> Result<Vec<u8>, bincode::Error>;
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, bincode::Error>;
}
```

---

## Configuration (dfs-common)

### Config
```rust
pub struct Config {
    pub node: NodeConfig,
    pub storage: StorageConfig,
    pub cluster: ClusterConfig,
    pub replication: ReplicationConfig,
}
impl Config {
    pub fn from_file(path: &Path) -> Result<Self>;
    pub fn to_file(&self, path: &Path) -> Result<()>;
    pub fn chunk_size_bytes(&self) -> usize;
}
```

### NodeConfig
```rust
pub struct NodeConfig {
    pub listen_addr: SocketAddr,  // Default: 0.0.0.0:8900
    pub name: Option<String>,
}
```

### StorageConfig
```rust
pub struct StorageConfig {
    pub data_dir: PathBuf,         // Default: /var/lib/dfs/data
    pub metadata_dir: PathBuf,     // Default: /var/lib/dfs/metadata
    pub chunk_size_mb: usize,      // Default: 4
}
```

### ClusterConfig
```rust
pub struct ClusterConfig {
    pub seed_nodes: Vec<SocketAddr>,
    pub heartbeat_interval_secs: u64,  // Default: 10
    pub failure_timeout_secs: u64,     // Default: 30
}
```

### ReplicationConfig
```rust
pub struct ReplicationConfig {
    pub replication_factor: usize,      // Default: 3
    pub healing_delay_secs: u64,        // Default: 300
    pub auto_heal: bool,                // Default: true
    pub scrub_interval_hours: u64,      // Default: 24
}
```

---

## Hashing (dfs-common)

### ConsistentHashRing
```rust
pub struct ConsistentHashRing;
impl ConsistentHashRing {
    pub fn new(virtual_nodes: usize) -> Self;
    pub fn add_node(&mut self, node_id: NodeId);
    pub fn remove_node(&mut self, node_id: &NodeId);
    pub fn get_nodes(&self, chunk_id: &ChunkId, n: usize) -> Vec<NodeId>;
    pub fn get_primary_node(&self, chunk_id: &ChunkId) -> Option<NodeId>;
    pub fn nodes(&self) -> &[NodeId];
    pub fn node_count(&self) -> usize;
}
```

### Hash Functions
```rust
pub fn compute_chunk_hash(data: &[u8]) -> [u8; 32];
pub fn verify_chunk_hash(data: &[u8], expected: &[u8; 32]) -> bool;
```

---

## Storage Layer (dfs-server)

### ChunkStorage
```rust
pub struct ChunkStorage;
impl ChunkStorage {
    pub fn new(data_dir: PathBuf) -> Result<Self>;
    pub fn write_chunk(&self, chunk_id: &ChunkId, data: &[u8]) -> Result<()>;
    pub fn read_chunk(&self, chunk_id: &ChunkId) -> Result<Vec<u8>>;
    pub fn read_and_verify_chunk(&self, chunk_id: &ChunkId) -> Result<Vec<u8>>;
    pub fn has_chunk(&self, chunk_id: &ChunkId) -> bool;
    pub fn delete_chunk(&self, chunk_id: &ChunkId) -> Result<()>;
    pub fn get_stats(&self) -> Result<StorageStats>;
    pub fn list_chunks(&self) -> Result<Vec<ChunkId>>;
}
```

Storage path: `{data_dir}/chunks/{hash[0:2]}/{hash[2:4]}/{full_hash}`

### StorageStats
```rust
pub struct StorageStats {
    pub total_chunks: usize,
    pub total_bytes: u64,
}
```

---

## Metadata Layer (dfs-server)

### MetadataStore
```rust
pub struct MetadataStore;
impl MetadataStore {
    pub fn new(metadata_dir: PathBuf) -> Result<Self>;

    // File operations
    pub fn put_file(&self, metadata: &FileMetadata) -> Result<()>;
    pub fn get_file(&self, file_id: &FileId) -> Result<Option<FileMetadata>>;
    pub fn get_file_by_path(&self, path: &str) -> Result<Option<FileMetadata>>;
    pub fn delete_file(&self, file_id: &FileId) -> Result<()>;
    pub fn list_files(&self) -> Result<Vec<FileMetadata>>;
    pub fn list_directory(&self, dir_path: &str) -> Result<Vec<FileMetadata>>;

    // Chunk location operations
    pub fn put_chunk_location(&self, location: &ChunkLocation) -> Result<()>;
    pub fn get_chunk_location(&self, chunk_id: &ChunkId) -> Result<Option<ChunkLocation>>;
    pub fn delete_chunk_location(&self, chunk_id: &ChunkId) -> Result<()>;

    pub fn flush(&self) -> Result<()>;
    pub fn get_stats(&self) -> Result<MetadataStats>;
}
```

### MetadataStats
```rust
pub struct MetadataStats {
    pub file_count: usize,
    pub size_on_disk: u64,
}
```

---

## Chunking (dfs-server)

### Chunker
```rust
pub struct Chunker;
impl Chunker {
    pub fn new(chunk_size: usize) -> Self;
    pub fn chunk_data(&self, data: &[u8]) -> Vec<(ChunkId, Vec<u8>)>;
    pub fn chunk_reader<R: Read>(&self, reader: R) -> Result<Vec<(ChunkId, Vec<u8>)>>;
    pub fn reassemble_chunks(&self, chunks: Vec<Vec<u8>>) -> Vec<u8>;
    pub fn calculate_chunk_count(&self, file_size: u64) -> usize;
}
```

---

## Cluster Management (dfs-server)

### ClusterManager
```rust
pub struct ClusterManager;
impl ClusterManager {
    pub fn new(
        local_node_id: NodeId,
        local_addr: SocketAddr,
        heartbeat_interval: u64,
        failure_timeout: u64
    ) -> Self;

    pub fn local_node_id(&self) -> NodeId;
    pub fn local_addr(&self) -> SocketAddr;

    pub async fn add_node(&self, node_info: NodeInfo) -> Result<()>;
    pub async fn remove_node(&self, node_id: &NodeId) -> Result<()>;
    pub async fn update_heartbeat(&self, node_id: &NodeId) -> Result<()>;
    pub async fn get_node(&self, node_id: &NodeId) -> Option<NodeInfo>;
    pub async fn get_all_nodes(&self) -> Vec<NodeInfo>;
    pub async fn online_node_count(&self) -> usize;

    pub async fn get_nodes_for_chunk(&self, chunk_id: &ChunkId, count: usize) -> Vec<NodeId>;
    pub async fn get_primary_node(&self, chunk_id: &ChunkId) -> Option<NodeId>;

    pub async fn start_failure_detector(self: Arc<Self>);
    pub async fn mark_node_recovered(&self, node_id: &NodeId) -> Result<()>;
    pub async fn get_stats(&self) -> ClusterStats;
}
```

---

## Network Layer (dfs-server)

### MessageHandler Trait
```rust
pub trait MessageHandler: Send + Sync {
    fn handle_request(&self, request: Request)
        -> Pin<Box<dyn Future<Output = Response> + Send + '_>>;

    fn handle_cluster_message(&self, message: ClusterMessage)
        -> Pin<Box<dyn Future<Output = Response> + Send + '_>>;
}
```

### NetworkServer
```rust
pub struct NetworkServer<H: MessageHandler>;
impl<H: MessageHandler + 'static> NetworkServer<H> {
    pub fn new(listen_addr: SocketAddr, handler: Arc<H>) -> Self;
    pub async fn start(&mut self) -> Result<()>;
    pub async fn shutdown(&mut self) -> Result<()>;
}
```

### NetworkClient
```rust
pub struct NetworkClient;
impl NetworkClient {
    pub fn new() -> Self;
    pub async fn send_message(
        &self,
        target: SocketAddr,
        message: Message
    ) -> Result<MessageEnvelope>;
}
```

---

## Server (dfs-server)

### Server
```rust
pub struct Server;
impl Server {
    pub fn new(
        storage: Arc<ChunkStorage>,
        metadata: Arc<MetadataStore>,
        chunk_size: usize,
        cluster: Arc<ClusterManager>,
        replication_factor: usize
    ) -> Self;

    pub fn cluster(&self) -> Arc<ClusterManager>;

    // Request handlers (implements MessageHandler)
    pub async fn handle_request(&self, request: Request) -> Response;

    // High-level operations
    pub async fn write_data(&self, data: &[u8]) -> Result<Vec<ChunkId>>;
    pub async fn read_data(&self, chunk_ids: &[ChunkId]) -> Result<Vec<u8>>;
}
```

**Write Flow:**
1. Chunk data into 4MB pieces
2. For each chunk:
   - Get N target nodes via consistent hash
   - Write to nodes in parallel
   - Wait for quorum (N/2+1) success
   - Store chunk location metadata
3. Return chunk IDs

**Read Flow:**
1. Try local storage first
2. If not found, query metadata for locations
3. Try each node until success
4. Return data

---

## Healing System (dfs-server)

### HealingManager
```rust
pub struct HealingManager;
impl HealingManager {
    pub fn new(
        storage: Arc<ChunkStorage>,
        metadata: Arc<MetadataStore>,
        cluster: Arc<ClusterManager>,
        replication_factor: usize,
        healing_delay_secs: u64,
        scrub_interval_hours: u64,
        auto_heal: bool
    ) -> Self;

    pub async fn start(self: Arc<Self>);
    pub async fn get_stats(&self) -> HealingStats;
}
```

**Healing Process:**
1. Every 60 seconds: check all local chunks
2. For each chunk:
   - Query metadata for locations
   - Count alive replicas
   - If under-replicated:
     - First time: mark with timestamp
     - After 300s delay: re-replicate
   - If over-replicated: cleanup excess
3. Scrubber runs every 24h:
   - Verify checksums on all chunks
   - Mark corrupted chunks for healing

### HealingStats
```rust
pub struct HealingStats {
    pub pending_healing: usize,
    pub auto_heal_enabled: bool,
    pub healing_delay_secs: u64,
}
```

---

## Server Startup Sequence

```rust
// 1. Load configuration
let config = Config::from_file(&config_path)?;

// 2. Initialize storage
let storage = Arc::new(ChunkStorage::new(config.storage.data_dir)?);

// 3. Initialize metadata
let metadata = Arc::new(MetadataStore::new(config.storage.metadata_dir)?);

// 4. Initialize cluster
let node_id = NodeId::new();
let cluster = Arc::new(ClusterManager::new(
    node_id,
    config.node.listen_addr,
    config.cluster.heartbeat_interval_secs,
    config.cluster.failure_timeout_secs,
));

// 5. Create server
let server = Arc::new(Server::new(
    storage.clone(),
    metadata.clone(),
    config.chunk_size_bytes(),
    cluster.clone(),
    config.replication.replication_factor,
));

// 6. Start failure detector
server.cluster().start_failure_detector().await;

// 7. Start healing manager
let healing = Arc::new(HealingManager::new(
    storage,
    metadata,
    cluster,
    config.replication.replication_factor,
    config.replication.healing_delay_secs,
    config.replication.scrub_interval_hours,
    config.replication.auto_heal,
));
healing.clone().start().await;

// 8. Start network server
let mut net_server = NetworkServer::new(config.node.listen_addr, server.clone());
tokio::spawn(async move {
    net_server.start().await
});
```

---

## Key Algorithms

### Consistent Hashing
- 100 virtual nodes per physical node
- Chunks map to nodes via Blake3(chunk_id) % ring_size
- Replication: take next N unique nodes on ring

### Quorum Writes
- Write to N nodes (N = replication_factor)
- Wait for N/2+1 to succeed
- Return error if quorum fails

### Healing Delay
- Detect under-replication
- Wait 300 seconds before healing
- Prevents premature rebalancing during reboots

### Scrubbing
- Read and verify checksum of every chunk
- Runs every 24 hours (configurable)
- Marks corrupted chunks for healing

---

## Network Protocol Details

### Message Framing
```
[4 bytes: length (big-endian u32)]
[N bytes: bincode-serialized MessageEnvelope]
```

### Connection Handling
- Server: accepts connections, spawns task per connection
- Client: creates connection per request (connection pooling TODO)
- Async I/O with tokio
- 8KB read buffers

---

## File Organization

```
/var/lib/dfs/
├── data/
│   └── chunks/
│       ├── 00/
│       │   ├── 00/
│       │   │   └── 0000...abc (64-char hex filename)
│       │   ├── 01/
│       │   └── ...
│       ├── 01/
│       └── ...
└── metadata/
    └── (sled database files)
```

---

## Testing

All modules have comprehensive unit tests:
- 28 tests passing
- Tests use tempfile for isolation
- Async tests with tokio::test

---

## Next Steps (Phase 6)

**FUSE Client Implementation:**

Key requirements:
1. Implement FUSE operations trait (fuser crate)
2. Map FUSE calls to Server API calls
3. Handle file operations: open, read, write, close
4. Handle directory operations: readdir, mkdir, rmdir
5. Handle metadata operations: getattr, setattr
6. Implement caching for performance
7. Connect to cluster (any node)

**Critical Server APIs for FUSE:**
- `Server::write_data(data)` → write file content
- `Server::read_data(chunk_ids)` → read file content
- `MetadataStore::put_file(metadata)` → create/update file
- `MetadataStore::get_file_by_path(path)` → lookup file
- `MetadataStore::list_directory(path)` → list dir contents
- `MetadataStore::delete_file(file_id)` → remove file

**FUSE Operations to Implement:**
- lookup: get_file_by_path
- getattr: get_file (return stat info)
- read: get_file → read_data(chunks)
- write: chunk data → write_data → update metadata
- readdir: list_directory
- mkdir: create FileMetadata with Directory type
- unlink: delete_file
- open/release: manage file handles

---

## Performance Characteristics

**SBC Optimizations:**
- 4MB chunks (memory-efficient)
- No checksum verification on read (CPU-friendly)
- Sled database (low memory)
- 8KB network buffers
- Async I/O (no thread-per-connection)
- Quorum writes (don't wait for all)
- 60s healing check interval
- Connection reuse

**Expected Performance:**
- Single node: ~disk speed
- Multi-node read: ~network speed (100Mbps-1Gbps)
- Multi-node write: limited by slowest of quorum nodes
- Metadata ops: fast (Sled is in-memory + disk)

---

## Error Handling

All public APIs return `Result<T>` or `Option<T>`.

Common error scenarios:
- Network failures: retry with different node
- Node failures: quorum handles gracefully
- Checksum mismatches: detected during scrubbing
- Under-replication: automatic healing after 300s
- Metadata not found: return None or NotFound error

---

## Security Considerations (Future)

Currently no security implemented. Future additions:
- TLS for node-to-node communication
- Authentication for clients
- Access control lists
- Encryption at rest
- Secure checksums (authenticated)

---

## Deployment

**Requirements:**
- Linux with FUSE kernel module
- Rust 1.94+
- Network connectivity between nodes
- Disk space for data + metadata

**Commands:**
```bash
# Initialize node
dfs-server init --data-dir /mnt/storage --meta-dir /var/lib/dfs/meta

# Start server
dfs-server start --config /etc/dfs/config.toml

# Check status
dfs-server status --config /etc/dfs/config.toml

# Mount (Phase 6)
dfs-client mount /mnt/dfs --cluster node1:8900,node2:8900,node3:8900
```

---

**This API reference covers all major components and interfaces. Use this as your guide for Phase 6 FUSE implementation!**
