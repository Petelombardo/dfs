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
    /// Node is pausing itself to run a full offline metadata compaction (no
    /// concurrent write churn to reconcile against, unlike the normal online
    /// path). Only self-elected when every other cluster member is confirmed
    /// Online — see ClusterManager::all_members_online and the CompactionIntent
    /// race-avoidance broadcast. Auto-recovers without a process restart: the
    /// listener and heartbeat sender pause, compaction runs, then both resume
    /// and the node's next heartbeat is picked up by the same Leaving-grace
    /// rejoin path ConnectionPressure/Shutdown already use.
    PlannedCompaction,
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

/// Reserved 2-byte prefix marking a MultiPatch "patch token" identity — see
/// `Server::patch_token_hash`'s doc comment in dfs-server for the full
/// rationale. A patch token's chunk_id is deliberately NOT a content hash
/// (`blake3("dfs-patch-token" || delta.hash)`), but until 2026-08-04 nothing
/// about its 32 raw bytes reflected that: it was statistically
/// indistinguishable from a real content-addressed chunk, and every place
/// that needed to tell them apart had to trust a `PATCH_STATE_TABLE` lookup —
/// a lookup shown to fail in two separate real incidents the same day (a
/// false "disk corruption" storm during healing, and a real, permanent
/// data-loss eviction by the disk-orphan-sweep). Forcing these two bytes
/// makes token identity self-describing: any caller can check
/// `ChunkId::looks_like_patch_token` with no database round trip and no
/// dependency on whether some other table's row happened to survive.
///
/// Two bytes gives a genuine content hash a 1/65536 chance of coincidentally
/// matching this prefix. Every consumer of `looks_like_patch_token` only
/// ever uses a `true` result to SKIP a destructive or verification step,
/// never to trigger one — so a false positive on an ordinary chunk fails
/// safe (treated a little more conservatively), never unsafe.
///
/// Tokens minted before this constant existed don't carry the marker and
/// still rely entirely on the pre-existing `PATCH_STATE_TABLE`-based checks,
/// unchanged, as a fallback — this is an additional, stronger guarantee for
/// tokens minted going forward, not a replacement.
pub const PATCH_TOKEN_MARKER: [u8; 2] = [0xDF, 0x7C];

/// Unique identifier for a chunk of data
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChunkId {
    /// Blake3 hash of the chunk data
    pub hash: [u8; 32],
}

impl ChunkId {
    /// Create from a hash
    pub fn from_hash(hash: [u8; 32]) -> Self {
        Self { hash }
    }

    /// True if this identity's first two bytes match the reserved
    /// patch-token marker — see `PATCH_TOKEN_MARKER`'s doc comment. A cheap,
    /// always-correct, zero-lookup recognition check.
    pub fn looks_like_patch_token(&self) -> bool {
        self.hash[0] == PATCH_TOKEN_MARKER[0] && self.hash[1] == PATCH_TOKEN_MARKER[1]
    }

    /// The identity apply_patch mints for an unfolded MultiPatch accumulator
    /// round: `blake3("dfs-patch-token" || delta_chunk_id.hash)`, domain-
    /// separated from any real content hash so nothing can mistake a pending
    /// overlay for directly-readable content, with `PATCH_TOKEN_MARKER`
    /// forced into the first two bytes so the identity is also self-describing
    /// (see that constant's doc comment). Single source of truth for this
    /// formula — dfs-server's `Server::patch_token_hash` and any test that
    /// needs a real token identity should call this directly rather than
    /// duplicating the construction.
    pub fn patch_token_identity(delta_chunk_id: ChunkId) -> ChunkId {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"dfs-patch-token");
        hasher.update(&delta_chunk_id.hash);
        let mut hash = *hasher.finalize().as_bytes();
        hash[0] = PATCH_TOKEN_MARKER[0];
        hash[1] = PATCH_TOKEN_MARKER[1];
        ChunkId { hash }
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

    /// Target path for a symlink (only set when file_type == Symlink). Stored inline
    /// in metadata rather than as chunk data — symlink targets are always short, so
    /// this avoids allocating/replicating a chunk just to hold a few bytes of text.
    /// None for non-symlinks and for records written before this field existed.
    #[serde(default)]
    pub symlink_target: Option<String>,
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
            symlink_target: None,
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

/// Pre-`symlink_target` on-disk shape of [`FileMetadata`]. bincode is a positional,
/// non-self-describing format: unlike JSON, `#[serde(default)]` cannot rescue a
/// struct deserialize when the trailing field's bytes simply aren't in the buffer —
/// it errors with `UnexpectedEof` instead of substituting the default (confirmed by
/// reproduction: staging's entire FILE_TABLE became undeserializable, reported as
/// "Total Files: 0", the moment `symlink_target` was added without this fallback).
/// Every FileMetadata record written before that field existed hits exactly this.
/// This struct must never be changed to track FileMetadata — it's a frozen fixture
/// that `deserialize_file_metadata` falls back to, not a schema to keep in sync.
#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct FileMetadataLegacyV0 {
    id: FileId,
    path: String,
    size: u64,
    created_at: u64,
    modified_at: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    file_type: FileType,
    chunk_locations: Arc<Vec<ChunkLocation>>,
    write_seq: u64,
}

impl From<FileMetadataLegacyV0> for FileMetadata {
    fn from(v: FileMetadataLegacyV0) -> Self {
        FileMetadata {
            id: v.id,
            path: v.path,
            size: v.size,
            created_at: v.created_at,
            modified_at: v.modified_at,
            mode: v.mode,
            uid: v.uid,
            gid: v.gid,
            file_type: v.file_type,
            chunk_locations: v.chunk_locations,
            write_seq: v.write_seq,
            symlink_target: None,
        }
    }
}

/// Deserialize a bincode-encoded FileMetadata, tolerating records written before any
/// field appended after `write_seq` existed (see `FileMetadataLegacyV0`). Every read
/// of a stored FileMetadata blob (FILE_TABLE, PATH_TABLE, dissemination queue, etc.)
/// must go through this rather than calling `bincode::deserialize` directly — that
/// bypass is exactly what broke staging (see FileMetadataLegacyV0's doc comment).
/// Any future field added to FileMetadata needs the same treatment: add it to this
/// function's fallback chain (deserialize current shape, then try each prior legacy
/// shape in turn) rather than assuming `#[serde(default)]` alone is enough.
pub fn deserialize_file_metadata(bytes: &[u8]) -> bincode::Result<FileMetadata> {
    match bincode::deserialize::<FileMetadata>(bytes) {
        Ok(m) => Ok(m),
        Err(_) => bincode::deserialize::<FileMetadataLegacyV0>(bytes).map(Into::into),
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

/// Get current Unix timestamp in milliseconds — used where second granularity
/// would produce too many spurious ties (e.g. CompactionIntent race arbitration).
pub fn current_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
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

    /// 2026-08-04: patch_token_identity must ALWAYS carry the reserved marker,
    /// deterministically, regardless of the delta chunk it's derived from —
    /// this is the whole basis for every zero-lookup looks_like_patch_token
    /// check downstream.
    #[test]
    fn patch_token_identity_always_carries_the_marker() {
        for seed in [b"delta-a".as_slice(), b"delta-b", b"", b"a very different length input entirely"] {
            let delta = ChunkId::from_hash(*blake3::hash(seed).as_bytes());
            let token = ChunkId::patch_token_identity(delta);
            assert_eq!(&token.hash[0..2], &PATCH_TOKEN_MARKER[..],
                "patch_token_identity must always force the marker prefix, got {:?}", token.hash);
            assert!(token.looks_like_patch_token(),
                "a freshly-minted token must recognize itself via looks_like_patch_token");
        }
    }

    /// Deterministic: the same delta must always produce the same token identity
    /// (apply_patch relies on this for idempotent re-derivation of an
    /// in-flight accumulator's own token across retries).
    #[test]
    fn patch_token_identity_is_deterministic() {
        let delta = ChunkId::from_hash(*blake3::hash(b"same-delta-every-time").as_bytes());
        let token_a = ChunkId::patch_token_identity(delta);
        let token_b = ChunkId::patch_token_identity(delta);
        assert_eq!(token_a, token_b);
    }

    /// Statistical sanity for the marker's collision assumption: real content
    /// hashes (uniformly-distributed blake3 output, unrelated to the token
    /// formula) essentially never coincidentally match the 2-byte marker.
    /// Can't assert "never" — asserts none do across a large, deterministic
    /// sample, documenting the ~1/65536-per-chunk expected rate this is meant
    /// to illustrate rather than tightly bound (a flaky statistical test would
    /// be worse than no test here).
    #[test]
    fn ordinary_content_hashes_essentially_never_collide_with_the_marker() {
        let mut collisions = 0u32;
        const SAMPLE: u32 = 20_000;
        for i in 0..SAMPLE {
            let hash = *blake3::hash(format!("ordinary-content-chunk-{}", i).as_bytes()).as_bytes();
            if hash[0] == PATCH_TOKEN_MARKER[0] && hash[1] == PATCH_TOKEN_MARKER[1] {
                collisions += 1;
            }
        }
        // Expected collisions over 20,000 samples at 1/65536 each: ~0.3. Allow
        // a generous margin (up to 5) rather than asserting exactly 0/1, since
        // this is a real (tiny) probability, not a bug, if it ever fires once —
        // the design's own safety argument (PATCH_TOKEN_MARKER's doc comment)
        // is that a false positive here fails safe, not that it can't happen.
        assert!(collisions <= 5,
            "expected roughly 0 collisions out of {} ordinary hashes against the 2-byte \
             marker (~1/65536 each), got {} — marker collision rate is much higher than \
             designed for", SAMPLE, collisions);
    }

    #[test]
    fn test_file_metadata_creation() {
        let meta = FileMetadata::new("/test.txt".to_string(), FileType::RegularFile);
        assert_eq!(meta.path, "/test.txt");
        assert_eq!(meta.size, 0);
        assert_eq!(meta.file_type, FileType::RegularFile);
    }

    // Regression test for the 2026-07-06 staging incident: adding symlink_target with
    // only #[serde(default)] silently broke every pre-existing FILE_TABLE record
    // (bincode::deserialize::<FileMetadata> errored with UnexpectedEof instead of
    // defaulting the field), which made the whole cluster report 0 files. This
    // constructs bytes shaped like a record written before symlink_target existed
    // (via FileMetadataLegacyV0 directly, not FileMetadata::new(), so this test still
    // catches a regression even if a future field is added the same wrong way) and
    // verifies deserialize_file_metadata() still reads it correctly.
    #[test]
    fn test_deserialize_file_metadata_reads_legacy_pre_symlink_target_records() {
        let legacy = FileMetadataLegacyV0 {
            id: FileId::new(),
            path: "/podman".to_string(),
            size: 4096,
            created_at: 1_700_000_000,
            modified_at: 1_700_000_001,
            mode: 0o755,
            uid: 0,
            gid: 0,
            file_type: FileType::Directory,
            chunk_locations: Arc::new(Vec::new()),
            write_seq: 3,
        };
        let bytes = bincode::serialize(&legacy).unwrap();

        // The raw bincode call this test guards against — must fail on legacy bytes,
        // confirming this test would have caught the incident.
        assert!(bincode::deserialize::<FileMetadata>(&bytes).is_err());

        let recovered = deserialize_file_metadata(&bytes).expect("must recover legacy record");
        assert_eq!(recovered.path, "/podman");
        assert_eq!(recovered.size, 4096);
        assert_eq!(recovered.write_seq, 3);
        assert_eq!(recovered.symlink_target, None);

        // Current-shape records must still round-trip normally.
        let mut fresh = FileMetadata::new("/t47_link.txt".to_string(), FileType::Symlink);
        fresh.symlink_target = Some("t47_target.txt".to_string());
        let fresh_bytes = bincode::serialize(&fresh).unwrap();
        let fresh_recovered = deserialize_file_metadata(&fresh_bytes).unwrap();
        assert_eq!(fresh_recovered.symlink_target, Some("t47_target.txt".to_string()));
    }
}
