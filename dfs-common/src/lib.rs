pub mod config;
pub mod types;
pub mod protocol;
pub mod hash;
pub mod memory;
pub mod storage_stats;

// Re-export commonly used types
pub use config::Config;
pub use types::{
    deserialize_file_metadata, ChunkId, ChunkLocation, FileId, FileMetadata, FileType,
    LeaveReason, NodeHealthGossip, NodeId, NodeInfo, NodeStatus, PATCH_TOKEN_MARKER,
};
pub use protocol::{
    ClusterMessage, DeleteQueueEntry, ErrorCode, FoldReleaseOutcome, Message, MessageEnvelope,
    MetadataOperation, ProposeFoldOutcome, Request, RequestId, Response,
};
pub use hash::{compute_chunk_hash, compute_chunk_hash_at, verify_chunk_hash, ConsistentHashRing};
pub use memory::{calculate_cache_capacity, calculate_server_cache_budget_mb, get_available_memory, get_total_memory};
pub use storage_stats::calculate_usable_capacity;
