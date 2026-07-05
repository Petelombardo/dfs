use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
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

    /// Unix timestamp when this node entered Leaving status (0 = not leaving).
    #[serde(default)]
    pub leaving_at: u64,

    /// Why this node is leaving (None if not leaving).
    #[serde(default)]
    pub leave_reason: Option<LeaveReason>,

    /// Available disk bytes on this node (0 = unknown). Populated by ClusterStatus responses.
    #[serde(default)]
    pub available_bytes: u64,

    /// Total disk bytes on this node (0 = unknown). Populated by ClusterStatus responses.
    #[serde(default)]
    pub total_bytes: u64,
}

impl NodeInfo {
    pub fn new(id: NodeId, addr: SocketAddr, name: Option<String>) -> Self {
        Self {
            id,
            addr,
            name,
            status: NodeStatus::Online,
            last_heartbeat: current_timestamp(),
            leaving_at: 0,
            leave_reason: None,
            available_bytes: 0,
            total_bytes: 0,
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

/// Why a node is leaving — carried in GracefulLeave broadcasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaveReason {
    /// Controlled shutdown (systemctl stop / Ctrl+C).
    Shutdown,
    /// All TCP connection slots are exhausted and the node is stepping down from
    /// leadership until pressure drops. It will auto-recover without restarting.
    ConnectionPressure,
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

    /// Chunk locations with replica node tracking.
    /// Arc-wrapped so cloning a FileMetadata (which happens very frequently — on every
    /// cache snapshot, every flush's dir_cache update, every network send prep) is an
    /// O(1) refcount bump instead of an O(n) deep copy of every ChunkLocation (each of
    /// which itself owns a Vec<NodeId>, so a naive clone is one heap allocation per
    /// chunk, twice — see the patch-timing test that caught this: patch latency scaled
    /// ~4-7x for an 8x larger file, uniformly regardless of patch position, which only
    /// makes sense for a cost proportional to total chunk count hit on every flush).
    /// Mutators use Arc::make_mut (copy-on-write — cheap when this is the sole owner,
    /// which is the common case). Serializes byte-identical to a plain Vec on the wire
    /// (see the workspace Cargo.toml's serde "rc" feature) — this is an in-memory
    /// representation change only, not a protocol change.
    pub chunk_locations: Arc<Vec<ChunkLocation>>,

    /// Monotonically increasing sequence number assigned by the client before
    /// enqueueing each metadata write. The server uses this to reject out-of-order
    /// deliveries: if stored write_seq > incoming write_seq, the write is stale and
    /// dropped. 0 means unsequenced (no ordering enforcement).
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
            created_at: now,
            modified_at: now,
            mode: if file_type == FileType::Directory { 0o755 } else { 0o644 },
            uid: 0,
            gid: 0,
            file_type,
            chunk_locations: Arc::new(Vec::new()),
            write_seq: 0,
        }
    }

    pub fn get_chunk_ids(&self) -> Vec<ChunkId> {
        self.chunk_locations.iter().map(|loc| loc.chunk_id).collect()
    }

    pub fn get_chunk_sizes(&self) -> Vec<u64> {
        self.chunk_locations.iter().map(|loc| loc.size as u64).collect()
    }

    /// Look up a chunk location by chunk index (chunk_idx * CHUNK_SIZE = file_offset).
    /// chunk_locations is a sparse list sorted by file_offset, NOT a dense array —
    /// indexing by chunk_idx as usize gives wrong results for sparse files with gaps.
    pub fn chunk_location_for_idx(&self, chunk_idx: u64) -> Option<&ChunkLocation> {
        const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
        let target_offset = chunk_idx * CHUNK_SIZE;
        // chunk_locations is sorted by file_offset (None entries treated as u64::MAX, at end).
        let pos = self.chunk_locations
            .binary_search_by(|l| l.file_offset.unwrap_or(u64::MAX).cmp(&target_offset))
            .ok()?;
        Some(&self.chunk_locations[pos])
    }

    /// Mutable version of chunk_location_for_idx.
    pub fn chunk_location_for_idx_mut(&mut self, chunk_idx: u64) -> Option<&mut ChunkLocation> {
        const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
        let target_offset = chunk_idx * CHUNK_SIZE;
        let pos = self.chunk_locations
            .binary_search_by(|l| l.file_offset.unwrap_or(u64::MAX).cmp(&target_offset))
            .ok()?;
        Some(&mut Arc::make_mut(&mut self.chunk_locations)[pos])
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

    /// File offset where this chunk starts (for sparse file support)
    /// If None, chunks are assumed to be sequential (legacy behavior)
    pub file_offset: Option<u64>,

    /// Unix timestamp (seconds) when this chunk location record was first written.
    /// The healer uses this to skip recently-written chunks during orphan purge,
    /// guarding against the race between ReplicateChunkLocation arriving before
    /// the corresponding put_file_metadata has committed.
    /// None for records written before this field was added (treated as old).
    pub written_at: Option<u64>,

    /// Client's monotone write_seq at the time this chunk was patched.
    /// Carried through MultiPatch → RCL so the leader can order concurrent patch
    /// notifications from the same file without relying on wall-clock time.
    /// Higher value = newer patch. None for fresh writes and legacy records.
    #[serde(default)]
    pub client_write_seq: Option<u64>,

    /// File this chunk's ChunkId was derived from (chunk_id is now
    /// blake3(file_id || file_offset || data), so this is the file_id input
    /// to that hash). Used to re-verify a chunk's content hash during healing
    /// and scrubbing. None for records written before this field was added or
    /// reconstructed without file context — verification is skipped in that case.
    #[serde(default)]
    pub file_id: Option<FileId>,
}

impl ChunkLocation {
    pub fn written_at_secs(&self) -> u64 {
        self.written_at.unwrap_or(0)
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
