use crate::types::{ChunkId, ChunkLocation, FileId, FileMetadata, NodeId, NodeInfo};
use serde::{Deserialize, Serialize};

/// Write a ChunkData response using split-frame encoding:
///   [4B envelope len][bincode envelope (data=empty)][4B raw len][raw bytes]
/// This avoids a full bincode copy of the chunk payload.
pub async fn write_chunk_response<W>(
    stream: &mut W,
    envelope: &MessageEnvelope,
    raw_data: &[u8],
) -> std::io::Result<()>
where
    W: tokio::io::AsyncWriteExt + Unpin,
{
    let frame = envelope.to_bytes()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let frame_len = frame.len() as u32;
    let data_len = raw_data.len() as u32;

    // Write header (envelope length + envelope + data length) as one small buffer,
    // then write raw_data directly — avoids copying the full chunk payload (~4MB)
    // into a Vec just to hand it to write_all. Works regardless of NIC scatter-gather
    // support; TCP is a byte stream so the receiver sees no boundary between the two
    // writes. The original single-buffer approach was added to avoid four separate tiny
    // writes; this preserves that for the header while skipping the payload copy.
    let mut header = Vec::with_capacity(4 + frame.len() + 4);
    header.extend_from_slice(&frame_len.to_be_bytes());
    header.extend_from_slice(&frame);
    header.extend_from_slice(&data_len.to_be_bytes());

    stream.write_all(&header).await?;
    stream.write_all(raw_data).await?;
    stream.flush().await
}

/// Read the raw payload that follows a split-frame ChunkData envelope.
pub async fn read_chunk_payload<R>(stream: &mut R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncReadExt + Unpin,
{
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Write a write request using split-frame encoding:
///   [4B envelope len][bincode envelope (data=empty)][4B raw len][raw bytes]
/// This avoids bincode serialization overhead on large write payloads.
pub async fn write_split_frame_request<W>(
    stream: &mut W,
    encoded_envelope: &[u8],
    raw_data: &[u8],
) -> std::io::Result<()>
where
    W: tokio::io::AsyncWriteExt + Unpin,
{
    let envelope_len = encoded_envelope.len() as u32;
    let data_len = raw_data.len() as u32;

    // Same two-write approach as write_chunk_response: header in one small buffer,
    // raw_data written directly to avoid a ~4MB copy. See that function's comment.
    let mut header = Vec::with_capacity(4 + encoded_envelope.len() + 4);
    header.extend_from_slice(&envelope_len.to_be_bytes());
    header.extend_from_slice(encoded_envelope);
    header.extend_from_slice(&data_len.to_be_bytes());

    stream.write_all(&header).await?;
    stream.write_all(raw_data).await?;
    stream.flush().await
}

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

fn default_true() -> bool { true }

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
        /// Client's cached metadata write_seq for this file.
        /// Server uses this to detect stale metadata and self-heal by pulling
        /// fresh metadata from the leader before serving the read.
        #[serde(default)]
        client_write_seq: Option<u64>,
    },

    /// Read a byte range from a chunk (for striped multi-replica reads)
    ReadChunkRange {
        chunk_id: ChunkId,
        offset: u64,
        length: u64,
        /// Client's cached metadata write_seq for staleness detection
        #[serde(default)]
        client_write_seq: Option<u64>,
    },

    /// Write a chunk
    WriteChunk {
        chunk_id: ChunkId,
        data: Vec<u8>,
        checksum: [u8; 32],
    },

    /// Delete a chunk (legacy — kept for DeleteChunkReplica compat; prefer DeleteChunksBatch)
    DeleteChunk {
        chunk_id: ChunkId,
    },

    /// Mark a chunk as tombstoned on this node. The HasChunks handler returns false for
    /// tombstoned chunks so the healer never selects this node as a source, preventing it
    /// from replicating the old chunk back to the two patched replicas during the window
    /// between a dual-RF MultiPatch and the metadata commit. Cleared automatically when
    /// the chunk is physically deleted (DeleteChunk / DeleteChunksBatch).
    TombstoneChunk {
        chunk_id: ChunkId,
    },

    /// Delete an excess chunk replica — leader-coordinated cleanup only.
    /// leader_id: the NodeId of the node issuing this instruction — recipient
    /// validates that the sender is actually the current leader before executing.
    DeleteChunkReplica {
        chunk_id: ChunkId,
        leader_id: NodeId,
    },

    /// Leader-to-follower: delete all listed chunks from local storage and metadata.
    /// Sent once per peer by the delete drain worker. Recipient acks after all chunks
    /// are deleted (missing chunks are not an error — already gone is fine).
    DeleteChunksBatch {
        file_id: FileId,
        path: String,
        chunk_ids: Vec<ChunkId>,
    },

    /// Leader-to-all: the queued deletion for file_id is fully complete — remove it
    /// from the local sled delete queue. Fire-and-forget; idempotent.
    ClearDeleteQueueEntry {
        file_id: FileId,
    },

    /// Leader polls all nodes for their pending delete queues on startup/election.
    GetDeleteQueue,

    /// Check if a chunk exists
    HasChunk {
        chunk_id: ChunkId,
    },

    /// Check if multiple chunks exist — single round trip for efficient healing scans.
    /// Returns a parallel Vec<bool> with one entry per chunk_id.
    HasChunks {
        chunk_ids: Vec<ChunkId>,
    },

    /// Verify that a chunk's on-disk data matches its content-addressed ID.
    /// The hash is file-scoped and position-aware (Blake3 of file_id || file_offset || data),
    /// so the caller must supply the file_offset and file_id stored in ChunkLocation.
    /// file_id of None skips verification (legacy record). Returns ChunkValid.
    VerifyChunkIntegrity {
        chunk_id: ChunkId,
        file_offset: u64,
        #[serde(default)]
        file_id: Option<FileId>,
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
        file_id: FileId,
    },

    /// Write file data locally only (no replication, returns chunk IDs)
    /// Used for optimized RF=3+ writes where client sends to 2 servers in parallel
    /// and healing creates the 3rd replica in background.
    /// file_offset and file_id are mixed into the chunk hash to prevent
    /// deduplication aliasing: identical blocks at different file positions, or
    /// at the same position in different files, must get distinct ChunkIds.
    WriteFileLocalOnly {
        data: Vec<u8>,
        file_offset: u64,
        file_id: FileId,
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
        /// Original write timestamp (Unix seconds) from ChunkLocation.written_at.
        /// Receiver sets the chunk file's mtime to this value so all replicas share
        /// the same mtime, enabling scrub-time corruption detection via mtime comparison.
        #[serde(default)]
        written_at: Option<u64>,
        /// When true this is a background healing transfer. The receiver uses idle
        /// I/O priority for its fsync so healing doesn't compete with client writes.
        #[serde(default)]
        background: bool,
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
        /// Remaining hops. Receiver stores locally, then if ttl > 0 forwards to all
        /// other nodes with ttl-1. Client sends ttl=1; prevents storms while ensuring
        /// every node receives every write regardless of which node the client hit first.
        #[serde(default)]
        ttl: u8,
    },

    /// Delete metadata from this node (internal cluster operation)
    DeleteMetadata {
        file_id: FileId,
        path: String,
        chunk_ids: Vec<ChunkId>,
        /// Remaining hops — same TTL semantics as ReplicateMetadata.
        #[serde(default)]
        ttl: u8,
    },

    /// Delete only the path→file_id index entry on this node (used by rename).
    /// Does NOT delete the file record or chunk locations — those belong to the renamed file.
    DeletePathIndex {
        path: String,
    },

    /// Replicate chunk location to this node (internal cluster operation)
    ReplicateChunkLocation {
        location: ChunkLocation,
        /// File that owns this chunk — enables targeted chunk_map update instead of
        /// scanning all files by file_offset (which matches offset=0 on every file).
        #[serde(default)]
        file_id: Option<FileId>,
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


    /// Apply a small patch to an existing chunk without transferring the full chunk.
    /// Server reads the chunk locally, splices in the patch bytes, recomputes the
    /// position-aware Blake3 hash, and writes the result as a new chunk file.
    /// Used by the client instead of client-side read-modify-write for random small writes.
    PatchChunk {
        /// Existing chunk to patch
        chunk_id: ChunkId,
        /// File this chunk belongs to — used by the server to validate chunk_id against
        /// its local chunk map (if the server's record for (file_id, chunk_idx) differs
        /// from chunk_id it returns ChunkStale instead of applying the patch), and mixed
        /// into the post-patch chunk hash so the new ChunkId stays file-scoped.
        file_id: FileId,
        /// Index of this chunk within the file (chunk_file_offset / CHUNK_SIZE).
        #[serde(default)]
        chunk_idx: Option<u64>,
        /// Absolute file offset of the start of this chunk (for position-aware hash)
        chunk_file_offset: u64,
        /// Byte offset within the chunk where the patch data begins
        intra_offset: usize,
        /// The patch bytes to splice in
        data: Vec<u8>,
    },

    /// Apply multiple non-contiguous byte-range patches to a chunk in a single RPC.
    /// The server reads the chunk once, applies all patches in order, writes the
    /// result. Avoids serial round-trips and server-side zero-fill gaps that occur
    /// when issuing separate PatchChunk calls for disjoint dirty ranges.
    MultiPatch {
        /// Existing chunk to patch
        chunk_id: ChunkId,
        /// File this chunk belongs to — for server-side chunk_id validation, and
        /// mixed into the post-patch chunk hash so the new ChunkId stays file-scoped.
        file_id: FileId,
        /// Index of this chunk within the file.
        #[serde(default)]
        chunk_idx: Option<u64>,
        /// Absolute file offset of the start of this chunk (for position-aware hash)
        chunk_file_offset: u64,
        /// Patches to apply: (intra_chunk_offset, data) pairs, applied in order.
        patches: Vec<(usize, Vec<u8>)>,
        /// Client-computed hash of the post-patch content. When Some, the server skips
        /// the read-back pass and renames directly to this hash — eliminating the full
        /// chunk read from the patch hot path. The server still writes and syncs the
        /// patch bytes; only the hash verification read is skipped.
        #[serde(default)]
        expected_new_chunk_id: Option<ChunkId>,
        /// Client's current write_seq for this file. Carried through to RCL so the
        /// leader can order concurrent patch notifications using a monotone client-side
        /// counter instead of wall-clock timestamps (which are unreliable across nodes).
        #[serde(default)]
        client_write_seq: Option<u64>,
        /// Other chunk_ids the client is about to MultiPatch to this same server in the
        /// current flush cycle. Server calls start_prefetch_for_patch for each, overlapping
        /// their disk reads with the time their patch payloads spend in transit.
        /// Computed fresh at dispatch time so later RPCs reflect the shrinking pending set.
        /// Server skips any chunk already being prefetched (contains_key guard).
        #[serde(default)]
        prefetch_hints: Option<Vec<ChunkId>>,
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

    /// Trigger full integrity repair on a specific file: verifies chunk hashes on all
    /// replica nodes, removes corrupt copies, heals under-replicated chunks, and trims
    /// over-replicated ones. When force=true the leadership grace period is bypassed so
    /// this can be used immediately after a leader election for manual recovery.
    RepairFile {
        /// File path (e.g. /podman/dvr/recordings/foo.mpg) or UUID string
        path: String,
        /// Bypass the post-election grace period for destructive operations
        force: bool,
    },

    /// Query a node for the physical sizes of chunks it owns.
    /// Used by quorum-based metadata repair to determine the authoritative file size
    /// from physical consensus rather than trusting any single node's metadata.
    /// Returns only chunks this node actually has on disk; missing entries mean
    /// the node does not hold that chunk.
    QueryChunkSizes {
        chunk_ids: Vec<ChunkId>,
    },

    /// Get file information with chunk locations
    GetFileInfo {
        path: String,
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
        /// When true the receiver broadcasts to peers. Set to false on peer broadcasts
        /// to prevent exponential storm — only the originating node propagates.
        #[serde(default = "default_true")]
        propagate: bool,
    },

    /// Get file information by file ID (UUID)
    GetFileInfoById {
        file_id: FileId,
    },

    /// Get a windowed slice of the chunk location map for a file from the leader.
    /// Only the leader maintains this in-memory map; followers serve chunk data.
    /// `from_chunk` is the first chunk index to return; `count` is the max number
    /// of chunks to return. Use from_chunk=0, count=u32::MAX for the full map.
    GetFileChunkMap {
        file_id: FileId,
        from_chunk: u32,
        count: u32,
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

    /// Ask another node whether each of these chunk_ids is still referenced by a
    /// live file, per ITS OWN local file metadata. Used by a non-leader node before
    /// fast-evicting a chunk it locally believes is orphaned — cross-checks against
    /// the leader (normally the most caught-up replica) instead of trusting only its
    /// own potentially-stale metadata copy.
    ConfirmChunksLive {
        chunk_ids: Vec<ChunkId>,
    },

    /// Ask any node to immediately run its local orphan reconciliation sweep instead
    /// of waiting for the next scheduled cycle. Safety gating (age grace, two-pass
    /// confirmation, leader cross-check or all-nodes-stability) still applies —
    /// this only skips the wait, never the checks.
    TriggerOrphanCleanup,

    /// Metadata reconciliation — sent by the leader after a repair pass.
    /// Contains the authoritative set of live file IDs. Followers remove any
    /// file: and path: records whose ID is not in this set, eliminating stale
    /// entries that accumulated from missed deletes. Chunk data is never touched.
    ReconcileMetadata {
        live_file_ids: Vec<FileId>,
    },

    /// Ask a node for its current metadata sequence number.
    /// Used by a newly-elected leader to determine how far behind each follower is.
    GetMetadataSequence,

    /// Ask a node for its full file inventory: Vec<(FileId, modified_at)>.
    /// Used by a newly-elected leader to find metadata it is missing or that is
    /// newer on a follower (e.g. written during a brief leader outage).
    GetFileInventory,

    /// Request specific file metadata records by ID.
    /// Sent by the new leader to pull records it is missing from a follower.
    GetFileMetadataBatch {
        file_ids: Vec<FileId>,
    },

    /// Leader-to-follower dissemination: deliver a batch of metadata updates in
    /// sequence order. The sequence number is the leader's monotonic counter.
    /// Followers store `last_received_sequence` and ack; leader removes from queue.
    DisseminateMetadata {
        items: Vec<FileMetadata>,
        up_to_sequence: u64,
    },

    /// Request per-node ops/sec statistics (reads, writes, metadata).
    /// Returns NodeStats. Safe to call on any node at any time.
    GetNodeStats,

    /// Trigger an immediate phantom-replica reconciliation pass: verifies actual
    /// presence on every listed node for every live chunk and prunes confirmed-
    /// absent ones, queuing under-RF results for immediate healing. Independent
    /// of the normal discovery cadence — see run_phantom_reconciliation_pass.
    /// Appended at the end of the enum, not inserted mid-list: Request/Response
    /// use plain derive(Serialize, Deserialize) with no explicit tag, so bincode
    /// encodes variants by positional index — inserting a variant in the middle
    /// shifts every later variant's wire index and breaks compatibility with any
    /// peer running a binary built before the insertion (see incident: gluster1
    /// healing recovery, 2026-06-20, where this exact mistake corrupted
    /// GetFileChunkMap on the wire for an unrebuilt client).
    TriggerPhantomReconciliation,

    /// Debug: return CHUNK_TABLE's raw stored ChunkLocation for a chunk_id, with no
    /// inline-merge or resolve_chunk_nodes fallback applied — the only way to see
    /// ground truth when the merged view (file info, GetFileChunkMap) is suspected
    /// of masking what's actually persisted. Added 2026-06-20 while chasing a chunk
    /// that never appeared in any healer log despite being queued for healing twice.
    DebugGetRawChunkLocation {
        chunk_id: ChunkId,
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
        /// Always empty when transmitted over the wire (split-frame protocol sends data separately).
        /// Populated in-process only when constructing a response for immediate network write
        /// without going through bincode serialization.
        data: Vec<u8>,
        /// Cache statistics for flow control
        /// Format: (cache_hits, cache_capacity, cache_size)
        /// Client can use this to throttle reads if cache is under pressure
        #[serde(default)]
        cache_stats: Option<(usize, usize, usize)>,
        /// Zero-copy payload: set by server handlers to avoid cloning chunk data into `data`.
        /// The network layer uses this Arc directly if present, bypassing the `data` Vec.
        /// Never serialized — always None after deserialization.
        #[serde(skip)]
        arc_data: Option<std::sync::Arc<Vec<u8>>>,
        /// Optional sub-range (start, end) into `arc_data`.  When `Some`, the network
        /// layer writes only `arc_data[start..end]` on the wire — used by ranged reads
        /// (striped half-chunk fetches) to avoid cloning the slice out of the cached Arc.
        /// Ignored when `arc_data` is None. Never serialized.
        #[serde(skip)]
        arc_range: Option<(usize, usize)>,
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

    /// Response to QueryChunkSizes: physical size of each chunk on this node.
    /// Only chunks present on disk are included; absent chunks are omitted.
    ChunkSizes {
        /// Parallel to request chunk_ids: actual on-disk byte count, or 0 if not present.
        sizes: Vec<u64>,
    },

    /// Boolean response (for HasChunk, etc.)
    Bool {
        value: bool,
    },

    /// Chunk integrity response (for VerifyChunkIntegrity).
    /// `found` = the chunk file exists on this node.
    /// `valid` = found AND hash matches (true only when found is also true).
    /// found=false means ghost replica; found=true, valid=false means corruption.
    ChunkValid {
        found: bool,
        valid: bool,
    },

    /// Parallel boolean response (for HasChunks) — one entry per requested chunk_id.
    BoolVec {
        values: Vec<bool>,
    },

    /// Response to ConfirmChunksLive: the subset of the requested chunk_ids that the
    /// responding node's own file metadata still references. Anything NOT in this
    /// list should be treated as "not confirmed live" by the caller — but absence
    /// alone is not sufficient to delete; see ConfirmChunksLive's doc comment.
    ChunkLiveness {
        live: Vec<ChunkId>,
    },

    /// Chunk IDs response (for WriteFile)
    ChunkIds {
        chunk_ids: Vec<ChunkId>,
        chunk_sizes: Vec<u64>,
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
        /// NodeId of the node that answered this request (its own heartbeat is N/A)
        #[serde(default)]
        local_node_id: Option<NodeId>,
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
        #[serde(default)]
        bandwidth_mb: usize,
    },

    /// File info with chunk locations
    FileInfo {
        metadata: FileMetadata,
        chunk_locations: Vec<ChunkLocation>,
    },

    /// File list response (admin operation)
    FileList {
        files: Vec<FileMetadata>,
        total_count: usize,
    },

    /// Windowed chunk location map for a file (leader-served)
    FileChunkMap {
        file_id: FileId,
        /// Chunk locations for the requested window, in order
        locations: Vec<ChunkLocation>,
        /// Index of the first chunk in `locations` (matches the requested from_chunk)
        from_chunk: u32,
        /// Total number of chunks in the file (so client knows the full extent)
        total_chunks: u32,
        /// Server-side write_seq (clock-agnostic) so client can detect changes
        write_seq: u64,
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

    /// Response to PatchChunk — new chunk identity after patch applied
    PatchChunkResult {
        /// Blake3 hash of the patched content (position-aware, same scheme as WriteFileLocalOnly)
        new_chunk_id: ChunkId,
        /// Chunk size in bytes after patch
        size: usize,
    },

    /// Response to MultiPatch — new chunk identity after all patches applied
    MultiPatchResult {
        new_chunk_id: ChunkId,
        size: usize,
        /// Server-side timestamp (ms since epoch) when the patch was applied.
        /// The client MUST use this as written_at for the new ChunkLocation so that
        /// future guard comparisons are all in server time, preventing clock-skew
        /// between client and server from defeating the stale-broadcast guard.
        patch_ts: Option<u64>,
    },

    /// Returned when a client sends PutFileMetadata to a non-leader.
    /// The client should redirect to leader_addr and retry immediately.
    NotLeader {
        leader_addr: Option<std::net::SocketAddr>,
    },

    /// Response to GetMetadataSequence — the node's last-received sequence number.
    MetadataSequence {
        sequence: u64,
    },

    /// Response to GetDeleteQueue — all pending delete queue entries on this node.
    DeleteQueue {
        entries: Vec<DeleteQueueEntry>,
    },

    /// Response to GetFileInventory — compact list of all known files.
    FileInventory {
        /// (file_id, modified_at) for every file record on this node.
        entries: Vec<(FileId, u64)>,
    },

    /// Response to GetFileMetadataBatch — full metadata for the requested files.
    FileMetadataBatch {
        items: Vec<FileMetadata>,
    },

    /// Error response
    Error {
        message: String,
        code: ErrorCode,
    },

    /// The chunk_id in a PatchChunk/MultiPatch request doesn't match the server's
    /// record for (file_id, chunk_idx). Patch was NOT applied. Client should update
    /// its local chunk map to use current_chunk_id and retry.
    /// MUST stay at the end of this enum — appending preserves existing variant
    /// indices so old servers remain wire-compatible with new clients/servers.
    ChunkStale {
        /// The chunk_id the server believes is current for this (file_id, chunk_idx).
        current_chunk_id: ChunkId,
        /// Replica nodes holding current_chunk_id.
        current_nodes: Vec<NodeId>,
    },

    /// Per-node ops/sec statistics (response to GetNodeStats).
    /// APPENDED at end to preserve wire compatibility with older nodes.
    NodeStats {
        /// Reads in the most recently completed 1-second window.
        reads_live: u64,
        /// Writes in the most recently completed 1-second window.
        writes_live: u64,
        /// Metadata ops in the most recently completed 1-second window.
        meta_live: u64,
        /// Peak reads/s over the last hour (0 if uptime < 1s).
        reads_peak_1h: u64,
        /// Peak writes/s over the last hour.
        writes_peak_1h: u64,
        /// Peak meta ops/s over the last hour.
        meta_peak_1h: u64,
        /// Peak total ops/s over the last hour (may exceed sum of individual peaks).
        total_peak_1h: u64,
        /// Average reads/s over the last hour.
        reads_avg_1h: u64,
        /// Average writes/s over the last hour.
        writes_avg_1h: u64,
        /// Average meta ops/s over the last hour.
        meta_avg_1h: u64,
        /// Node uptime in seconds.
        uptime_secs: u64,
        /// Currently active inbound TCP connections.
        #[serde(default)]
        active_connections: u64,
        /// Maximum allowed inbound TCP connections.
        #[serde(default)]
        max_connections: u64,
    },

    /// Response to DebugGetRawChunkLocation. `location` is None if CHUNK_TABLE has
    /// no record at all for the requested chunk_id.
    DebugRawChunkLocation {
        location: Option<ChunkLocation>,
    },
}

/// A pending file deletion entry — stored in each node's sled delete queue.
/// Chunk data is not deleted until the leader drains this entry to all peers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteQueueEntry {
    pub file_id: FileId,
    pub path: String,
    pub chunk_ids: Vec<ChunkId>,
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
    /// Server is at connection limit and cannot accept more work right now
    ServerBusy,
    /// Node is intentionally leaving — client should immediately retry with another node
    NodeLeaving,
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

    /// Broadcast when a node assumes leadership.
    /// Recipients with lower-priority NodeId (higher UUID) concede immediately
    /// by marking the sender Online in their gossip view, causing is_leader()
    /// to re-evaluate and yield to the true leader.
    LeaderAnnouncement {
        node_id: NodeId,
        addr: std::net::SocketAddr,
    },

    /// Broadcast when a node is leaving intentionally (shutdown or connection pressure).
    /// Peers mark the node Leaving immediately — no need to wait for the heartbeat
    /// timeout to expire.  The node gets a grace window to come back before healing starts.
    GracefulLeave {
        node_id: NodeId,
        addr: std::net::SocketAddr,
        reason: crate::types::LeaveReason,
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
