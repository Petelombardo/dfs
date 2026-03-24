pub mod config;
pub mod types;
pub mod protocol;
pub mod hash;

// Re-export commonly used types
pub use config::Config;
pub use types::{
    ChunkId, ChunkLocation, FileId, FileMetadata, FileType, NodeId, NodeInfo, NodeStatus,
};
pub use protocol::{
    ClusterMessage, ErrorCode, Message, MessageEnvelope, MetadataOperation, Request, RequestId,
    Response,
};
pub use hash::{compute_chunk_hash, verify_chunk_hash, ConsistentHashRing};
