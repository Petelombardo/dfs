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

    /// Chunk IDs that make up this file (in order)
    pub chunks: Vec<ChunkId>,

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
}

impl FileMetadata {
    pub fn new(path: String, file_type: FileType) -> Self {
        let now = current_timestamp();
        Self {
            id: FileId::new(),
            path,
            size: 0,
            chunks: Vec::new(),
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

/// Information about where a chunk is stored
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
