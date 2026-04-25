pub mod config;
pub mod types;
pub mod protocol;
pub mod hash;
pub mod memory;

// Re-export commonly used types
pub use config::Config;
pub use types::{
    ChunkId, ChunkLocation, FileId, FileMetadata, FileType, NodeHealthGossip, NodeId, NodeInfo,
    NodeStatus,
};
pub use protocol::{
    ClusterMessage, DeleteQueueEntry, ErrorCode, Message, MessageEnvelope, MetadataOperation,
    Request, RequestId, Response,
};
pub use hash::{compute_chunk_hash, compute_chunk_hash_at, verify_chunk_hash, ConsistentHashRing};
pub use memory::{calculate_cache_capacity, get_available_memory, get_total_memory};
