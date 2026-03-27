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

    /// Check if a chunk exists
    HasChunk {
        chunk_id: ChunkId,
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

    /// Delete file by path
    DeleteFile {
        path: String,
    },

    /// Replicate a chunk to this node
    ReplicateChunk {
        chunk_id: ChunkId,
        data: Vec<u8>,
        checksum: [u8; 32],
    },

    /// Replicate metadata to this node (internal cluster operation)
    ReplicateMetadata {
        metadata: FileMetadata,
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

    /// Chunk IDs response (for WriteFile)
    ChunkIds {
        chunk_ids: Vec<ChunkId>,
        chunk_sizes: Vec<u64>,
    },

    /// Cluster status response
    ClusterStatus {
        nodes: Vec<NodeInfo>,
        total_nodes: usize,
        healthy_nodes: usize,
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
}

/// Cluster management messages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClusterMessage {
    /// Heartbeat to indicate node is alive
    Heartbeat {
        node_info: NodeInfo,
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
