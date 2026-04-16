use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::SocketAddr;
use uuid::Uuid;

/// Unique identifier for a node in the cluster
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub struct NodeId(pub Uuid);

impl NodeId {
    /// Generate a new random NodeId
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Create from a UUID
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Get byte representation
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// Create from bytes
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(Uuid::from_bytes(bytes))
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Information about a cluster node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Unique node identifier
    pub id: NodeId,

    /// Node's network address
    pub addr: SocketAddr,

    /// Optional human-readable name
    pub name: Option<String>,

    /// Node status
    pub status: NodeStatus,

    /// Last heartbeat timestamp (Unix epoch seconds)
    pub last_heartbeat: u64,
}

impl NodeInfo {
    pub fn new(id: NodeId, addr: SocketAddr, name: Option<String>) -> Self {
        Self {
            id,
            addr,
            name,
            status: NodeStatus::Online,
            last_heartbeat: current_timestamp(),
        }
    }

    /// Update heartbeat to current time
    pub fn update_heartbeat(&mut self) {
        self.last_heartbeat = current_timestamp();
    }

    /// Check if node has failed based on timeout
    pub fn is_failed(&self, timeout_secs: u64) -> bool {
        let now = current_timestamp();
        now - self.last_heartbeat > timeout_secs
    }
}

/// Node status in the cluster
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeStatus {
    /// Node is online and healthy
    Online,
    /// Node is suspected to have failed (no heartbeat)
    Suspected,
    /// Node has failed
    Failed,
    /// Node is leaving the cluster gracefully
    Leaving,
}

/// Compact node health information for gossiping
/// This is exchanged between nodes to maintain consistent cluster views
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeHealthGossip {
    /// Node identifier
    pub node_id: NodeId,
    /// Unix timestamp when sender last heard from this node
    pub last_seen: u64,
    /// Sender's opinion of this node's status
    pub status: NodeStatus,
}

/// Unique identifier for a chunk of data
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkId {
    /// Blake3 hash of the chunk data
    pub hash: [u8; 32],
}

impl ChunkId {
    /// Create from a hash
    pub fn from_hash(hash: [u8; 32]) -> Self {
        Self { hash }
    }

    /// Get hex string representation
    pub fn to_hex(&self) -> String {
        hex::encode(self.hash)
    }

    /// Get path components for storage (first 2 bytes for directory sharding)
    pub fn storage_path_components(&self) -> (String, String, String) {
        let hex = self.to_hex();
        let dir1 = &hex[0..2];
        let dir2 = &hex[2..4];
        (dir1.to_string(), dir2.to_string(), hex)
    }

    /// Get byte representation for storage
    pub fn as_bytes(&self) -> &[u8] {
        &self.hash
    }

    /// Create from bytes
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() == 32 {
            let mut hash = [0u8; 32];
            hash.copy_from_slice(bytes);
            Some(Self { hash })
        } else {
            None
        }
    }
}

impl fmt::Display for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// Unique identifier for a file
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileId(pub Uuid);

impl FileId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(Uuid::from_bytes(bytes))
    }
}

impl Default for FileId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Metadata about a file in the filesystem
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    /// Unique file identifier
    pub id: FileId,

    /// File path (relative to mount point)
    pub path: String,

    /// File size in bytes
    pub size: u64,

    /// DEPRECATED: Legacy chunks field for backward compatibility
    /// Use chunk_locations instead - this will be removed in future versions
    #[serde(default)]
    pub chunks: Vec<ChunkId>,

    /// DEPRECATED: Legacy chunk sizes field for backward compatibility
    /// Use chunk_locations[].size instead - this will be removed in future versions
    #[serde(default)]
    pub chunk_sizes: Vec<u64>,

    /// Creation timestamp (Unix epoch seconds)
    pub created_at: u64,

    /// Last modification timestamp
    pub modified_at: u64,

    /// Unix permissions
    pub mode: u32,

    /// Owner user ID
    pub uid: u32,

    /// Owner group ID
    pub gid: u32,

    /// File type
    pub file_type: FileType,

    /// Chunk locations with replica node tracking (optional, for dual-replica writes)
    /// Added at END for bincode backward compatibility
    /// When present, this takes precedence over chunks/chunk_sizes fields
    #[serde(default)]
    pub chunk_locations: Vec<ChunkLocation>,

    /// Monotonically increasing sequence number assigned by the client before
    /// enqueueing each metadata write. The server uses this to reject out-of-order
    /// deliveries: if stored write_seq > incoming write_seq, the write is stale and
    /// dropped. Defaults to 0 for legacy records (no ordering enforcement).
    #[serde(default)]
    pub write_seq: u64,
}

impl FileMetadata {
    pub fn new(path: String, file_type: FileType) -> Self {
        let now = current_timestamp();
        Self {
            id: FileId::new(),
            path,
            size: 0,
            chunks: Vec::new(),  // Deprecated, kept for backward compat
            chunk_sizes: Vec::new(),  // Deprecated, kept for backward compat
            created_at: now,
            modified_at: now,
            mode: if file_type == FileType::Directory {
                0o755
            } else {
                0o644
            },
            uid: 0,
            gid: 0,
            file_type,
            chunk_locations: Vec::new(),
            write_seq: 0,
        }
    }

    /// Get chunk IDs from either chunk_locations or legacy chunks field
    pub fn get_chunk_ids(&self) -> Vec<ChunkId> {
        if !self.chunk_locations.is_empty() {
            self.chunk_locations.iter().map(|loc| loc.chunk_id).collect()
        } else {
            self.chunks.clone()
        }
    }

    /// Get chunk sizes from either chunk_locations or legacy chunk_sizes field
    pub fn get_chunk_sizes(&self) -> Vec<u64> {
        if !self.chunk_locations.is_empty() {
            self.chunk_locations.iter().map(|loc| loc.size as u64).collect()
        } else {
            self.chunk_sizes.clone()
        }
    }
}

/// Type of file system entry
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileType {
    RegularFile,
    Directory,
    Symlink,
}

/// Legacy ChunkLocation format (before sparse file support - 4 fields)
/// This is used to deserialize old metadata from bincode format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkLocationV0 {
    pub chunk_id: ChunkId,
    pub nodes: Vec<NodeId>,
    pub size: usize,
    pub checksum: [u8; 32],
}

/// Legacy ChunkLocation format (5 fields, before written_at was added)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkLocationV1 {
    pub chunk_id: ChunkId,
    pub nodes: Vec<NodeId>,
    pub size: usize,
    pub checksum: [u8; 32],
    pub file_offset: Option<u64>,
}

/// Information about where a chunk is stored (current format - 6 fields)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkLocation {
    /// Chunk identifier
    pub chunk_id: ChunkId,

    /// Nodes that have this chunk
    pub nodes: Vec<NodeId>,

    /// Chunk size in bytes
    pub size: usize,

    /// Checksum for verification
    pub checksum: [u8; 32],

    /// File offset where this chunk starts (for sparse file support)
    /// If None, chunks are assumed to be sequential (legacy behavior)
    pub file_offset: Option<u64>,

    /// Unix timestamp (seconds) when this chunk location record was first written.
    /// The healer uses this to skip recently-written chunks during orphan purge,
    /// guarding against the race between ReplicateChunkLocation arriving before
    /// the corresponding put_file_metadata has committed.
    /// None for records written before this field was added (treated as old).
    pub written_at: Option<u64>,
}

impl ChunkLocation {
    pub fn written_at_secs(&self) -> u64 {
        self.written_at.unwrap_or(0)
    }
}

impl From<ChunkLocationV0> for ChunkLocation {
    fn from(v0: ChunkLocationV0) -> Self {
        ChunkLocation {
            chunk_id: v0.chunk_id,
            nodes: v0.nodes,
            size: v0.size,
            checksum: v0.checksum,
            file_offset: None,
            written_at: None,
        }
    }
}

impl From<ChunkLocationV1> for ChunkLocation {
    fn from(v1: ChunkLocationV1) -> Self {
        ChunkLocation {
            chunk_id: v1.chunk_id,
            nodes: v1.nodes,
            size: v1.size,
            checksum: v1.checksum,
            file_offset: v1.file_offset,
            written_at: None,
        }
    }
}

/// FileMetadata format before written_at was added to ChunkLocation (uses ChunkLocationV1)
/// This is used to deserialize metadata written after sparse file support but before written_at.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadataV1 {
    pub id: FileId,
    pub path: String,
    pub size: u64,
    #[serde(default)]
    pub chunks: Vec<ChunkId>,
    #[serde(default)]
    pub chunk_sizes: Vec<u64>,
    pub created_at: u64,
    pub modified_at: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub file_type: FileType,
    #[serde(default)]
    pub chunk_locations: Vec<ChunkLocationV1>,
}

impl From<FileMetadataV1> for FileMetadata {
    fn from(v1: FileMetadataV1) -> Self {
        FileMetadata {
            id: v1.id,
            path: v1.path,
            size: v1.size,
            chunks: v1.chunks,
            chunk_sizes: v1.chunk_sizes,
            created_at: v1.created_at,
            modified_at: v1.modified_at,
            mode: v1.mode,
            uid: v1.uid,
            gid: v1.gid,
            file_type: v1.file_type,
            chunk_locations: v1.chunk_locations.into_iter().map(|loc| loc.into()).collect(),
            write_seq: 0,
        }
    }
}

/// Legacy FileMetadata format (before sparse file support with ChunkLocationV0)
/// This is used to deserialize old metadata from bincode format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadataV0 {
    pub id: FileId,
    pub path: String,
    pub size: u64,
    #[serde(default)]
    pub chunks: Vec<ChunkId>,
    #[serde(default)]
    pub chunk_sizes: Vec<u64>,
    pub created_at: u64,
    pub modified_at: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub file_type: FileType,
    #[serde(default)]
    pub chunk_locations: Vec<ChunkLocationV0>,
}

impl From<FileMetadataV0> for FileMetadata {
    fn from(v0: FileMetadataV0) -> Self {
        FileMetadata {
            id: v0.id,
            path: v0.path,
            size: v0.size,
            chunks: v0.chunks,
            chunk_sizes: v0.chunk_sizes,
            created_at: v0.created_at,
            modified_at: v0.modified_at,
            mode: v0.mode,
            uid: v0.uid,
            gid: v0.gid,
            file_type: v0.file_type,
            chunk_locations: v0.chunk_locations.into_iter().map(|loc| loc.into()).collect(),
            write_seq: 0,
        }
    }
}

/// Get current Unix timestamp in seconds
pub fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

// Add hex dependency for ChunkId
mod hex {
    pub fn encode(bytes: [u8; 32]) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_id_creation() {
        let id1 = NodeId::new();
        let id2 = NodeId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_chunk_id_path_components() {
        let hash = [0u8; 32];
        let chunk_id = ChunkId::from_hash(hash);
        let (dir1, dir2, full) = chunk_id.storage_path_components();
        assert_eq!(dir1, "00");
        assert_eq!(dir2, "00");
        assert_eq!(full.len(), 64);
    }

    #[test]
    fn test_file_metadata_creation() {
        let meta = FileMetadata::new("/test.txt".to_string(), FileType::RegularFile);
        assert_eq!(meta.path, "/test.txt");
        assert_eq!(meta.size, 0);
        assert_eq!(meta.file_type, FileType::RegularFile);
    }
}
