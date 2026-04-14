use crate::types::{ChunkId, ChunkLocation, FileId, FileMetadata, NodeId, NodeInfo};
use serde::{Deserialize, Serialize};

/// Messages exchanged between nodes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// Request messages
    Request(Request),

    /// Response messages
    Response(Response),

    /// Cluster management messages
    Cluster(ClusterMessage),
}

/// Request types sent between nodes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Read a chunk
    ReadChunk {
        chunk_id: ChunkId,
        /// Optional hint: this chunk is part of a sequential read pattern
        /// Format: (current_chunk_index, total_chunks_in_file)
        /// Server can use this to prefetch subsequent chunks into cache
        /// This is purely a hint - server may ignore it
        #[serde(default)]
        sequential_hint: Option<(u64, u64)>,
    },

    /// Read a byte range from a chunk (for striped multi-replica reads)
    ReadChunkRange {
        chunk_id: ChunkId,
        offset: u64,
        length: u64,
    },

    /// Write a chunk
    WriteChunk {
        chunk_id: ChunkId,
        data: Vec<u8>,
        checksum: [u8; 32],
    },

    /// Delete a chunk
    DeleteChunk {
        chunk_id: ChunkId,
    },

    /// Delete an excess chunk replica — leader-coordinated cleanup only.
    /// leader_id: the NodeId of the node issuing this instruction — recipient
    /// validates that the sender is actually the current leader before executing.
    DeleteChunkReplica {
        chunk_id: ChunkId,
        leader_id: NodeId,
    },

    /// Check if a chunk exists
    HasChunk {
        chunk_id: ChunkId,
    },

    /// Check if multiple chunks exist — single round trip for efficient healing scans.
    /// Returns a parallel Vec<bool> with one entry per chunk_id.
    HasChunks {
        chunk_ids: Vec<ChunkId>,
    },

    /// Get file metadata by file ID
    GetFileMetadata {
        file_id: FileId,
    },

    /// Get file metadata by path (for FUSE client)
    GetFileMetadataByPath {
        path: String,
        /// Optional timestamp for conditional fetch (HTTP-style If-Modified-Since)
        /// If provided and metadata hasn't changed, returns NotModified
        if_modified_since: Option<u64>,
    },

    /// Update file metadata
    UpdateFileMetadata {
        metadata: FileMetadata,
    },

    /// Put file metadata (create or update)
    PutFileMetadata {
        metadata: FileMetadata,
    },

    /// List directory contents
    ListDirectory {
        path: String,
    },

    /// Write file data (returns chunk IDs)
    WriteFile {
        data: Vec<u8>,
    },

    /// Write file data locally only (no replication, returns chunk IDs)
    /// Used for optimized RF=3+ writes where client sends to 2 servers in parallel
    /// and healing creates the 3rd replica in background
    WriteFileLocalOnly {
        data: Vec<u8>,
    },

    /// Delete file by path
    DeleteFile {
        path: String,
    },

    /// Rename/move file (atomic operation)
    RenameFile {
        old_path: String,
        new_path: String,
    },

    /// Replicate a chunk to this node
    ReplicateChunk {
        chunk_id: ChunkId,
        data: Vec<u8>,
        checksum: [u8; 32],
    },

    /// Instruct this node to push a chunk it holds to a target node.
    /// Used by the leader to coordinate healing without proxying data through itself.
    /// The receiving node reads the chunk locally and sends ReplicateChunk to target_addr.
    /// leader_id: the NodeId of the node issuing this instruction — recipient validates
    /// that the sender is actually the current leader before executing.
    PushChunkTo {
        chunk_id: ChunkId,
        target_addr: std::net::SocketAddr,
        leader_id: NodeId,
    },

    /// Replicate metadata to this node (internal cluster operation)
    ReplicateMetadata {
        metadata: FileMetadata,
    },

    /// Delete metadata from this node (internal cluster operation)
    DeleteMetadata {
        file_id: FileId,
        path: String,
        chunk_ids: Vec<ChunkId>,
    },

    /// Replicate chunk location to this node (internal cluster operation)
    ReplicateChunkLocation {
        location: ChunkLocation,
    },

    /// Batch replicate chunk locations — one round-trip replaces N×ReplicateChunkLocation.
    /// Sent by the leader at the end of each heal/discovery cycle.
    ReplicateChunkLocations {
        locations: Vec<ChunkLocation>,
    },

    /// Batch replicate file metadata — one round-trip replaces N×ReplicateMetadata.
    /// Used by the metadata retry loop to drain failed replications efficiently.
    ReplicateMetadataBatch {
        items: Vec<FileMetadata>,
    },

    /// Hint to server: warm cache with these chunks (best-effort prefetch)
    /// Client sends this when it detects sequential reads and wants to minimize
    /// future read latency by having chunks pre-loaded into server's LRU cache
    PrefetchHint {
        chunk_ids: Vec<ChunkId>,
    },

    // Admin requests
    /// Get cluster status
    GetClusterStatus,

    /// Get storage statistics
    GetStorageStats,

    /// Get healing status
    GetHealingStatus,

    /// Trigger manual scrub
    TriggerScrub,

    /// Enable healing
    EnableHealing,

    /// Disable healing
    DisableHealing,

    /// Trigger immediate healing check
    TriggerHealing,

    /// Trigger healing for a specific file by path or file ID (UUID).
    /// Leader queues all of the file's chunks into pending_healing immediately,
    /// bypassing the normal delay. Useful for testing or forcing a specific file
    /// to be re-replicated without waiting for the full discovery pass.
    HealFile {
        /// File path (e.g. /podman/dvr/recordings/foo.mpg) or UUID string
        path: String,
    },

    /// Trigger metadata repair: rebuild path index and chunk map from file records.
    /// Safe to run at any time; runs in background and does not block responses.
    TriggerMetadataRepair,

    /// Get file information with chunk locations
    GetFileInfo {
        path: String,
    },

    /// Get chunk replica locations
    GetChunkReplicas {
        chunk_id: ChunkId,
    },

    /// Remove a node from the cluster
    RemoveNode {
        node_id: NodeId,
    },

    /// List all files in metadata database (admin operation)
    ListAllFiles,

    /// Purge file metadata from database without deleting chunks (admin operation for fixing corruption)
    PurgeFileMetadata {
        path: String,
    },

    /// Purge file metadata by ID (for corrupted path indexes)
    PurgeFileMetadataById {
        file_id: FileId,
    },

    /// Get file information by file ID (UUID)
    GetFileInfoById {
        file_id: FileId,
    },

    /// Get the full chunk location map for a file from the leader.
    /// Only the leader maintains this in-memory map; followers serve chunk data.
    GetFileChunkMap {
        file_id: FileId,
    },

    /// Append data to an existing file. The server handles chunk alignment:
    /// if the file's last chunk is partial (< 4MB), the server reads it back,
    /// prepends it to `data`, writes complete 4MB chunks + new partial tail,
    /// updates FileMetadata atomically, and returns the updated metadata.
    ///
    /// `expected_offset` is a CAS guard — server rejects with OffsetMismatch
    /// if file.size != expected_offset, preventing double-appends on retry.
    AppendFile {
        file_id: FileId,
        data: Vec<u8>,
        expected_offset: u64,
    },

    /// Purge a chunk location record from this node's routing table.
    /// Sent by the leader to all followers after the leader purges an orphaned
    /// chunk: record, so follower DBs drain in sync with the leader rather than
    /// accumulating stale records forever (which was causing OOM).
    /// Does NOT delete chunk data — only the routing metadata entry.
    PurgeChunkLocation {
        chunk_id: ChunkId,
    },

    /// Batch purge chunk location records — one round-trip replaces N×PurgeChunkLocation.
    /// Sent by the leader after an orphan purge sweep.
    PurgeChunkLocations {
        chunk_ids: Vec<ChunkId>,
    },

    /// Metadata reconciliation — sent by the leader after a repair pass.
    /// Contains the authoritative set of live file IDs. Followers remove any
    /// file: and path: records whose ID is not in this set, eliminating stale
    /// entries that accumulated from missed deletes. Chunk data is never touched.
    ReconcileMetadata {
        live_file_ids: Vec<FileId>,
    },
}

/// Response types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    /// Success with optional data
    Ok {
        data: Option<Vec<u8>>,
    },

    /// Success with chunk data
    ChunkData {
        chunk_id: ChunkId,
        data: Vec<u8>,
        /// Cache statistics for flow control
        /// Format: (cache_hits, cache_capacity, cache_size)
        /// Client can use this to throttle reads if cache is under pressure
        #[serde(default)]
        cache_stats: Option<(usize, usize, usize)>,
    },

    /// Success with file metadata
    FileMetadata {
        metadata: FileMetadata,
    },

    /// Metadata not modified (conditional GET returned 304)
    NotModified,

    /// Success with directory listing
    DirectoryListing {
        entries: Vec<FileMetadata>,
    },

    /// Boolean response (for HasChunk, etc.)
    Bool {
        value: bool,
    },

    /// Parallel boolean response (for HasChunks) — one entry per requested chunk_id.
    BoolVec {
        values: Vec<bool>,
    },

    /// Chunk IDs response (for WriteFile)
    ChunkIds {
        chunk_ids: Vec<ChunkId>,
        chunk_sizes: Vec<u64>,
        /// All NodeIds that received replicas of the written chunks.
        /// One entry per chunk_id (same ordering). Empty means unknown (legacy).
        #[serde(default)]
        replica_nodes_per_chunk: Vec<Vec<crate::NodeId>>,
    },

    /// Cluster status response
    ClusterStatus {
        nodes: Vec<NodeInfo>,
        total_nodes: usize,
        healthy_nodes: usize,
        chunk_size_mb: usize,
        /// NodeId of the current cluster leader (min NodeId among online nodes)
        #[serde(default)]
        leader_node_id: Option<NodeId>,
        /// Replication factor configured on this node
        #[serde(default)]
        replication_factor: usize,
    },

    /// Storage statistics response
    StorageStats {
        total_chunks: usize,
        total_size: u64,
        replication_factor: usize,
        nodes_count: usize,
        /// Total disk space in bytes (smallest node capacity * node count / replication factor)
        total_space: u64,
        /// Free disk space in bytes (smallest node free space * node count / replication factor)
        free_space: u64,
        /// Available disk space in bytes (smallest node available * node count / replication factor)
        available_space: u64,
    },

    /// Healing status response
    HealingStatus {
        enabled: bool,
        pending_count: usize,
        in_flight_count: usize,
        stalled_count: usize,
        last_check: u64,
    },

    /// File info with chunk locations
    FileInfo {
        metadata: FileMetadata,
        chunk_locations: Vec<ChunkLocation>,
    },

    /// Chunk replicas response
    ChunkReplicas {
        chunk_id: ChunkId,
        nodes: Vec<NodeId>,
    },

    /// File list response (admin operation)
    FileList {
        files: Vec<FileMetadata>,
        total_count: usize,
    },

    /// Full chunk location map for a file (leader-served)
    FileChunkMap {
        file_id: FileId,
        /// All chunk locations for the file, in order
        locations: Vec<ChunkLocation>,
        /// Server-side modified_at timestamp so client can detect changes
        modified_at: u64,
    },

    /// Returned by AppendFile on success. Contains the authoritative updated
    /// FileMetadata so the client doesn't need a follow-up GetFileMetadata call.
    AppendFileResult {
        metadata: FileMetadata,
        /// Bytes remaining in the current partial chunk before the next chunk boundary.
        /// When this reaches 0, the chunk is sealed and the client should rotate to a
        /// different primary node for the next AppendFile call to distribute write load.
        remaining_in_chunk: u64,
    },

    /// Prefetch hint acknowledged (best-effort, no guarantee)
    PrefetchAccepted {
        /// Number of chunks that will be prefetched
        accepted: usize,
    },

    /// Error response
    Error {
        message: String,
        code: ErrorCode,
    },
}

/// Error codes for protocol operations
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    NotFound,
    AlreadyExists,
    PermissionDenied,
    IOError,
    NetworkError,
    ChecksumMismatch,
    InvalidRequest,
    InternalError,
    /// AppendFile: file.size != expected_offset
    OffsetMismatch,
}

/// Cluster management messages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClusterMessage {
    /// Heartbeat to indicate node is alive
    /// Now includes cluster view for gossip protocol
    Heartbeat {
        node_info: NodeInfo,
        #[serde(default)]
        cluster_view: Vec<crate::NodeHealthGossip>,
    },

    /// Join cluster request
    Join {
        node_info: NodeInfo,
    },

    /// Leave cluster announcement
    Leave {
        node_id: NodeId,
    },

    /// Node failure detected
    NodeFailed {
        node_id: NodeId,
    },

    /// Node recovered
    NodeRecovered {
        node_id: NodeId,
    },

    /// Request cluster membership information
    GetClusterInfo,

    /// Cluster membership information
    ClusterInfo {
        nodes: Vec<NodeInfo>,
    },

    /// Metadata update broadcast (for consistency)
    MetadataUpdate {
        metadata: FileMetadata,
        operation: MetadataOperation,
    },

    /// Request to join the cluster
    JoinRequest {
        node_info: NodeInfo,
    },

    /// Response to join request
    JoinResponse {
        accepted: bool,
        cluster_nodes: Vec<NodeInfo>,
    },

    /// Broadcast that a node has joined
    NodeJoined {
        node_info: NodeInfo,
    },
}

/// Type of metadata operation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetadataOperation {
    Create,
    Update,
    Delete,
}

/// Request ID for tracking request/response pairs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(pub u64);

impl RequestId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

/// Message envelope with request ID for tracking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEnvelope {
    pub request_id: RequestId,
    pub message: Message,
}

impl MessageEnvelope {
    pub fn new(request_id: RequestId, message: Message) -> Self {
        Self {
            request_id,
            message,
        }
    }

    /// Serialize to bytes using bincode
    pub fn to_bytes(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }

    /// Deserialize from bytes using bincode
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, bincode::Error> {
        bincode::deserialize(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_serialization() {
        let msg = Message::Request(Request::HasChunk {
            chunk_id: ChunkId::from_hash([0u8; 32]),
        });
        let envelope = MessageEnvelope::new(RequestId::new(1), msg);

        let bytes = envelope.to_bytes().unwrap();
        let decoded = MessageEnvelope::from_bytes(&bytes).unwrap();

        assert_eq!(envelope.request_id, decoded.request_id);
    }
}
