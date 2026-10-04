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
    ChunkLocationReceipt, ClusterMessage, DeleteQueueEntry, ErrorCode, FoldReleaseOutcome, Message, MessageEnvelope,
    MetadataOperation, PeerFilter, PeerFilterMode, PendingHealingEntry, ProposeFoldOutcome, RemotePatchState, Request,
    RequestId, Response, SlotAuditEntry, SlotAuditFinding, SlotAuditMismatch,
    LeaseStatusReport, NodeLeaseState, SlotIsr, StallTarget, Ballot, PrepareReply, AcceptReply, WriteOrderTag,
};
pub use hash::{compute_chunk_hash, compute_chunk_hash_at, verify_chunk_hash, ConsistentHashRing};
pub use memory::{calculate_cache_capacity, calculate_server_cache_budget_mb, get_available_memory, get_total_memory};
pub use storage_stats::calculate_usable_capacity;

/// `{:?}` of `value`, cut off after `max` bytes, for logging requests and responses whose
/// payloads can be megabytes (a 4 MB WriteChunk printed as ~19 MB of decimal bytes per debug
/// line filled the dev box's disk). Formatting stops at the cap: the writer refuses the rest,
/// so the cost is bounded too, not just the output.
pub fn debug_truncated<T: std::fmt::Debug + ?Sized>(value: &T, max: usize) -> String {
    struct Capped { out: String, max: usize, truncated: bool }
    impl std::fmt::Write for Capped {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            let room = self.max.saturating_sub(self.out.len());
            if s.len() <= room {
                self.out.push_str(s);
                return Ok(());
            }
            let mut cut = room;
            while cut > 0 && !s.is_char_boundary(cut) { cut -= 1; }
            self.out.push_str(&s[..cut]);
            self.truncated = true;
            Err(std::fmt::Error)
        }
    }
    let mut w = Capped { out: String::new(), max, truncated: false };
    let _ = std::fmt::write(&mut w, format_args!("{:?}", value));
    if w.truncated { w.out.push_str("…[truncated]"); }
    w.out
}

#[cfg(test)]
mod debug_truncated_tests {
    #[test]
    fn caps_large_values_and_leaves_small_ones_alone() {
        let big = vec![7u8; 4 * 1024 * 1024];
        let s = super::debug_truncated(&big, 500);
        assert!(s.len() < 520 && s.ends_with("…[truncated]"));
        assert_eq!(super::debug_truncated(&(1, "x"), 500), "(1, \"x\")");
    }
}
