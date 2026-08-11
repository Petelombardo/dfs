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
    /// Liveness probe. Deliberately payload-free and answered at the very top of
    /// the request dispatch, before any lock or map access, so it is the cheapest
    /// possible "are you actually processing requests?" check. Used by the
    /// black-hole tripwire (a normal request outstanding past ~5s fires a Ping on
    /// a fresh connection): a healthy-but-slow node still answers Pong instantly,
    /// a wedged node — every worker parked, port still LISTENing — cannot, which
    /// is what distinguishes "slow" from "hung" without guessing from a timeout.
    Ping,
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
        /// (file, chunk index) slot backstop — see ReadChunkRange's file_id/chunk_idx.
        #[serde(default)]
        file_id: Option<FileId>,
        #[serde(default)]
        chunk_idx: Option<u64>,
    },

    /// Read a byte range from a chunk (for striped multi-replica reads)
    ReadChunkRange {
        chunk_id: ChunkId,
        offset: u64,
        length: u64,
        /// Client's cached metadata write_seq for staleness detection
        #[serde(default)]
        client_write_seq: Option<u64>,
        /// The logical (file, chunk index) slot this read is for. When `chunk_id`
        /// can't be resolved — e.g. a rewrite retired it while the client's chunk
        /// map was stale — the server falls back to whatever chunk_id currently
        /// occupies this slot, which is the authoritative answer to "what content
        /// belongs at this position" and never depends on a best-effort alias
        /// having been populated. Optional because internal/striped read paths
        /// don't always have file context; the backstop only fires when both are
        /// present.
        #[serde(default)]
        file_id: Option<FileId>,
        #[serde(default)]
        chunk_idx: Option<u64>,
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
        /// Lowest write_seq this push's chunk_locations fully accounts for. Equal to
        /// metadata.write_seq for a standalone push; lower when the client's
        /// MetadataQueue coalesced multiple pending pushes for this file into one
        /// (their chunk_locations were unioned before sending — see
        /// MetadataQueue::push_inner). Lets the server tell "this jump in write_seq
        /// is fully accounted for by coalescing" apart from "some write_seq in
        /// between was never represented in any push that reached us" — see
        /// handle_put_file_metadata's gap check.
        covers_from_write_seq: u64,
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
        /// Per-slot generation (the client's monotone new_chunk_seq) of `location`'s
        /// chunk — the authoritative causal ordering the receiver feeds to
        /// `location_supersedes` so a REORDERED broadcast for a retired generation
        /// can't revert the slot's current pointer (fix S, 2026-07-24 VM-108). None
        /// from legacy/mixed-version senders → receiver falls back to cws/written_at.
        #[serde(default)]
        generation: Option<u64>,
    },

    /// Batch replicate chunk locations — one round-trip replaces N×ReplicateChunkLocation.
    /// Sent by the leader at the end of each heal/discovery cycle.
    ReplicateChunkLocations {
        locations: Vec<ChunkLocation>,
    },

    /// Targeted, immediate heal: copy `chunk_id` to `target_node` specifically, instead
    /// of the normal healer's capacity-aware candidate selection (which is seeded from
    /// the chunk's content hash and so can pick a different node on every fold
    /// generation of the same slot). Sent by a client that already knows, from its own
    /// `canonical_write_nodes` tracking, exactly which node a chunk_idx's replica
    /// dropped out from — restoring to that same expected node instead of an
    /// independently re-derived one. See handle_heal_chunk_to_node's doc comment.
    HealChunkToNode {
        chunk_id: ChunkId,
        target_node: NodeId,
        file_id: Option<FileId>,
    },

    /// Broadcast a completed background fold's token→real-chunk redirect to every
    /// other online node (deferred chunk-patch consolidation — see PATCH_STATE_TABLE
    /// in metadata.rs). PATCH_STATE_TABLE is otherwise node-local and never
    /// disseminated, which left two gaps a public_token that outlives its own node's
    /// knowledge would fall into: (1) a read of `public_token` served by any other
    /// node hard-fails (no local Pending/Folded row to resolve through, and the
    /// token itself is never a real on-disk file), and (2) GetFileInfo answered by
    /// any other node reports the token's own frozen ChunkLocation (whatever node
    /// count it had at patch time) forever, instead of `real_chunk_id`'s actual,
    /// growing replica set. Sent once, right after the local fold flips
    /// patch_state to Folded, alongside the existing ReplicateChunkLocation
    /// broadcast for `real_chunk_id`'s own location.
    ReplicatePatchFold {
        public_token: ChunkId,
        real_chunk_id: ChunkId,
        file_id: FileId,
        chunk_idx: u64,
        /// real_chunk_id's own ChunkLocation, carried so the receiver can
        /// correct its in-memory chunk_map directly instead of depending on
        /// the sibling ReplicateChunkLocation broadcast (a separate,
        /// independent fire-and-forget message) having already landed.
        /// `#[serde(default)]`/Option so an older peer's un-upgraded binary
        /// omitting this field still deserializes (None falls back to the
        /// pre-2026-08-07 lookup-dependent behavior). Root-caused 2026-08-07:
        /// a lost/delayed ReplicateChunkLocation left chunk_map permanently
        /// stale on receiving nodes, surfacing as a real client EIO once the
        /// orphaned token was later garbage-collected.
        #[serde(default)]
        location: Option<ChunkLocation>,
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
        /// Client-assigned sequence number for this (file_id, chunk_idx) slot —
        /// see CHUNK_SEQ_TABLE's doc comment in dfs-server/src/metadata.rs. None
        /// from a caller that hasn't adopted per-slot sequencing yet. Currently
        /// record-only: the server stores it but does not yet gate on it.
        #[serde(default)]
        new_chunk_seq: Option<u64>,
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
        /// Client-assigned sequence number for this (file_id, chunk_idx) slot —
        /// see CHUNK_SEQ_TABLE's doc comment in dfs-server/src/metadata.rs. None
        /// from a caller that hasn't adopted per-slot sequencing yet. Currently
        /// record-only: the server stores it but does not yet gate on it.
        #[serde(default)]
        new_chunk_seq: Option<u64>,
    },

    /// Explicitly fold a slot's accumulated Pending patches into real content
    /// right now. Sent identically by the client to every replica it's
    /// targeting for a slot, roughly every 8s of continuous active patching
    /// (see multi_patch_chunk_on_replicas_inner's active-fold timer) — this
    /// is now the *primary* fold trigger, replacing each server's own
    /// independent per-node timer. Root-caused 2026-07-11: two replicas
    /// polling independently (whether on wall-clock time or patch count)
    /// can decide "fold now" at different logical points in an identical
    /// patch stream, producing two different results for what should be the
    /// same accumulator generation ("REPLICA DISAGREEMENT"). A single
    /// client-issued command delivered to all replicas removes that race by
    /// construction — every replica folds in response to the same external
    /// event instead of its own clock. Each server's debounce_fold_slot timer
    /// still exists as a catch-all for a client that disappears before ever
    /// sending this.
    ForceFold {
        file_id: FileId,
        chunk_idx: u64,
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

    /// Ask the leader to authoritatively re-derive the current chunk_id for one
    /// (file_id, chunk_idx) slot from CHUNK_TABLE, bypassing its own possibly-
    /// stale in-memory chunk_map cache for that lookup — because
    /// `distrust_chunk_id` was just proven wrong by a real read failure (every
    /// replica the client tried said "not found"). A new variant rather than a
    /// field added to GetFileChunkMap above: wire messages are bincode-
    /// serialized (positional, non-self-describing), so a new field on an
    /// existing, already-widely-used variant carries the same backward-
    /// compatibility hazard documented for persisted structs (see
    /// feedback_bincode_field_addition_not_backward_compatible in project
    /// memory) — a new variant doesn't change any existing variant's shape.
    ///
    /// Answered via Response::FileChunkMap (0-or-1-element `locations`, empty
    /// only if the slot has no CHUNK_TABLE record at all) — same response type
    /// GetFileChunkMap uses, no new response shape needed. Leader-only, same
    /// as GetFileChunkMap (chunk_map only exists there).
    RevalidateChunkSlot {
        file_id: FileId,
        chunk_idx: u64,
        distrust_chunk_id: ChunkId,
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

    /// Ask a node for every chunk_id currently outstanding in its LOCAL
    /// PATCH_STATE_TABLE (deferred chunk-patch consolidation), Pending or Folded.
    /// PATCH_STATE_TABLE is node-local and never disseminated — a patch can be
    /// applied by any node in the cluster, not just the leader — so the leader's
    /// healing discovery pass must union this across every online node instead of
    /// trusting its own local table alone. Without this, a token created on a
    /// follower is invisible to the leader's exclusion check, gets classified as
    /// an ordinary chunk, and HasChunks correctly (but misleadingly) reports it
    /// absent everywhere — since a token is never a real on-disk file — which can
    /// stall its replica count forever or, worse, walk it into the DATA LOSS purge
    /// path (see PATCH_STATE_TABLE's doc comment in metadata.rs).
    GetPatchTokenIds,

    /// Ask a node for the base_chunk_id/delta_chunk_id set of every currently-
    /// Pending row in its OWN local PATCH_STATE_TABLE (not the token keys — see
    /// GetPatchTokenIds for those). Used by handle_confirm_chunks_live so a
    /// live-file orphan sweep's authorization check unions this across every
    /// online node instead of trusting whichever single node answers the RPC
    /// (almost always the leader) — a patch can land on any node, not just the
    /// leader, and PATCH_STATE_TABLE is node-local, never disseminated. Without
    /// this, a chunk still a live Pending base on a follower the leader has
    /// already itself moved past looks "not live" to the leader's own three
    /// sources, gets authorized for deletion, and a real data-loss incident
    /// results the moment the requesting node's own physical copy is the last
    /// one anywhere still relied on by a client's in-flight patch (2026-07-10,
    /// VM-111 install: gluster4 asked leader gluster1 — who had already moved
    /// past the chunk itself — deleted its own copy, RF dropped from 3 to 2
    /// exactly on the two nodes the client's deterministic replica selection
    /// kept picking, EIO).
    GetPendingPatchChunkIds,

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

    /// Combined request added 2026-08-11 for the live-file-orphan-sweep leader
    /// authorization path (authorize_live_file_orphan_deletes, healing.rs):
    /// that path used to issue GetNodeStats + GetPendingPatchChunkIds as two
    /// separate sequential RPCs per peer, on every sweep page — real RPC-count
    /// cost that scales with cluster size and sweep frequency. This combines
    /// exactly the two fields that call site actually needs (peer uptime,
    /// peer's pending patch chunk ids) into one round trip. Deliberately NOT a
    /// change to GetNodeStats or GetPendingPatchChunkIds themselves — both have
    /// other, unrelated callers (dfs-admin and handle_confirm_chunks_live
    /// respectively) that don't need or want the other field bundled in.
    /// Returns OrphanAuthInfo.
    GetOrphanAuthInfo,

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

    /// Live-update one or more healing tuning knobs (bandwidth ceiling, concurrency,
    /// transfer timeout). `None` fields are left unchanged — this is a partial patch,
    /// not a full replace. Applied immediately in-memory and persisted to config.toml
    /// so it survives a restart. Handled by `Server::handle_set_healing_tuning`.
    /// Appended at the end of the enum, not inserted mid-list — see
    /// TriggerPhantomReconciliation's doc comment above for why.
    SetHealingTuning {
        link_bandwidth_mb: Option<usize>,
        heal_max_pct: Option<f64>,
        heal_max_concurrent: Option<usize>,
        heal_transfer_timeout_secs: Option<u64>,
        /// Appended at the end, not inserted mid-list — bincode is positional, so a
        /// mid-list insertion would misalign every field after it on wire-version skew.
        #[serde(default)]
        heal_max_concurrent_per_node: Option<usize>,
        /// Delay (seconds) before a "never fully replicated" chunk becomes eligible
        /// for healing. Same append-only reasoning as the field above.
        #[serde(default)]
        healing_delay_secs: Option<u64>,
    },

    /// Live-update the cluster's replication factor. Applied immediately in-memory
    /// (shared Arc<AtomicUsize> between Server and HealingManager) and persisted to
    /// config.toml. Handled by `Server::handle_set_replication_factor`.
    SetReplicationFactor {
        replication_factor: usize,
    },

    /// Sent by a node about to fold a base chunk into a new one — tells the
    /// leader (which owns all healing decisions/state) to drop `chunk_id` from
    /// its heal queue and in-flight set, and tombstone it so any heal already
    /// past those guards discards its result instead of committing a
    /// ChunkLocation update for an identity the fold is about to retire.
    /// Handled locally without an RPC when the caller already is the leader.
    /// Appended at the end of the enum, not inserted mid-list — see
    /// TriggerPhantomReconciliation's doc comment for why (bincode is
    /// positional; a mid-list insertion breaks wire compatibility with any
    /// peer running a binary built before the insertion).
    CancelHealing {
        chunk_id: ChunkId,
    },

    /// Undo a prior CancelHealing for `chunk_id` — sent when the fold that
    /// requested it turns out to be a no-op (the "base chunk" never actually
    /// got superseded, so it's still fine, even necessary, to heal normally).
    RetractHealingCancellation {
        chunk_id: ChunkId,
    },

    /// Forward chunks to the leader for immediate (delay-bypassed) healing — sent
    /// by `HealingManager::queue_chunks_immediate` when the calling node isn't the
    /// leader itself, since `pending_healing` is only ever drained on the leader
    /// (see that function's doc comment for the dead-end bug this closes). Handled
    /// locally without an RPC when the caller already is the leader, same as
    /// CancelHealing above.
    QueueChunksForHealing {
        chunk_ids: Vec<ChunkId>,
        /// True only for genuine URGENT_SINGLE_REPLICA emergencies — routes
        /// through HealingManager's dedicated heal_semaphore_urgent/
        /// node_inflight_urgent pools instead of competing with routine backlog.
        /// `#[serde(default)]` so an older peer's un-upgraded binary sending this
        /// request without the field still deserializes (treated as non-urgent,
        /// the safe default).
        #[serde(default)]
        urgent: bool,
    },

    /// Batch, authoritative chunk-location replication — the healer's completion
    /// path uses this instead of one `ReplicateChunkLocation` per chunk per peer.
    /// "Authoritative" means: apply the SAME merge rules as the single-item
    /// `ReplicateChunkLocation` (fresh-write timestamp stamping, under-RF union,
    /// ts-guarded expansion, stale-early-write ignore, ts-guarded trim/same-count
    /// acceptance) — see `Server::merge_replicated_chunk_location`. Deliberately
    /// NOT routed through the existing `ReplicateChunkLocations` batch handler,
    /// which has weaker self-report merge rules (ignores same-count updates and
    /// trims when existing >= RF) tuned for a follower's periodic self-push of its
    /// own locally-held records — those weaker rules would silently drop an
    /// authoritative heal result (e.g. ghost-node replacement {A,B,dead} →
    /// {A,B,D}, same node count, which self-report rules reject as "stale").
    /// Appended at the end of the enum, not inserted mid-list — see
    /// TriggerPhantomReconciliation's doc comment for why (bincode is positional;
    /// a mid-list insertion breaks wire compatibility with any peer running a
    /// binary built before the insertion).
    ReplicateChunkLocationsV2 {
        locations: Vec<ChunkLocation>,
    },

    /// Peer-to-peer pre-fold arbitration for the debounce/idle-timer fold path
    /// (OverlayForkCtx::coordinate_and_fold_slot / debounce_fold_slot) — closes
    /// a real incident where two replicas' independent debounce timers each
    /// folded from a possibly-divergent local accumulator, producing two
    /// different chunk_ids for the same slot (see fold_slot_coordinated's doc
    /// comment for the full history: three prior mitigation attempts never
    /// closed this, only shrank the window). Sent by whichever replica's
    /// debounce timer fires first, to every OTHER online replica of the slot
    /// (normally exactly one, given dual-RF), BEFORE that node ever calls
    /// fold_slot_now for this generation. A single round trip doubles as both
    /// the hash-agreement check (base_chunk_id + delta_chunk_id are themselves
    /// content-addressed, so equality proves byte-identical accumulators with
    /// no extra hashing) and the exclusive-lock acquisition (a Granted
    /// response is the peer's promise not to independently fold this exact
    /// slot itself until released or the lease TTL expires).
    /// Appended at the end of the enum, not inserted mid-list — bincode is
    /// positional (see ReplicateChunkLocationsV2's doc comment for the
    /// incident this rule exists to prevent).
    ProposeFold {
        file_id: FileId,
        chunk_idx: u64,
        proposer: NodeId,
        /// Wall-clock ms (dfs_common::types::current_timestamp_ms(), matching
        /// CompactionIntent::proposed_at_ms's precision) — used ONLY to break
        /// a simultaneous mutual-proposal collision, never to decide data
        /// correctness. Both sides evaluate the same (proposed_at_ms,
        /// proposer) tuple, so clock skew can only affect *which* side wins,
        /// not whether both sides agree on the outcome.
        proposed_at_ms: u64,
        /// This node's current PatchState::Pending.base_chunk_id for the slot.
        base_chunk_id: ChunkId,
        /// This node's current PatchState::Pending.delta_chunk_id — itself a
        /// content hash of the full accumulated delta bytes, so two nodes
        /// reporting the same value have byte-identical accumulators by
        /// construction. No separate hash-exchange step is needed.
        delta_chunk_id: ChunkId,
        /// Cheap stat (ChunkStorage::get_chunk_size), not a read — lets the
        /// receiving side, on a same-base/different-delta disagreement, tell
        /// which side's accumulator is strictly more complete (patches are
        /// appended in order from a shared base, so a longer delta file is a
        /// safe proxy for "has seen more of the same stream") without reading
        /// either delta's bytes.
        delta_size_hint: u64,
    },

    /// Release a lease granted by a prior ProposeFold — sent once, right
    /// after the proposer's own fold_slot_coordinated(Wave) call returns
    /// (success or failure), to every peer it proposed to. Best-effort/no
    /// retry: a lost release just means the peer waits out
    /// FOLD_COORD_LOCK_TTL instead of noticing immediately, the same
    /// crash-safety net that already covers the proposer dying mid-fold.
    ReleaseFoldLock {
        file_id: FileId,
        chunk_idx: u64,
        holder: NodeId,
        outcome: FoldReleaseOutcome,
    },

    /// Operational visibility: cumulative-since-startup counts of every RPC
    /// this node has handled, bucketed by class (peer healing/delete/fold/
    /// gossip/other, client full-patch/multi-patch/fold/other, admin), plus
    /// local chunk-delete counts by reason tag (reuses delete_chunk's
    /// existing reason strings — see RpcClassCounts' doc comment). dfs-admin
    /// only. Appended at the end of the enum, not inserted mid-list — bincode
    /// is positional (see ReplicateChunkLocationsV2's doc comment for the
    /// incident this rule exists to prevent).
    GetRpcClassCounts,

    /// Ask a peer what a given identifier actually resolves to in its local
    /// PATCH_STATE_TABLE — added 2026-08-07 to close a gap where a node with
    /// no local history for a slot could be handed a peer-reported "current"
    /// identifier that turns out to be a still-pending public token (never
    /// itself real, directly-readable content — see PATCH_TOKEN_MARKER's doc
    /// comment) and, having no way to tell the two apart, silently wire the
    /// token in as a new patch's base_chunk_id, which no fold can ever verify
    /// afterward — a permanent, silent dead end, not merely a stale-retry.
    /// Answered from local patch_state only, no forwarding — the caller
    /// already targets a specific, presumably-authoritative node (typically
    /// one of a ChunkLocation's own `nodes`). APPENDED at end to preserve
    /// wire compatibility, matching every other addition in this enum.
    GetPatchState {
        public_token: ChunkId,
    },

    /// Diagnostic: sample of the leader's pending_healing queue, oldest-first,
    /// capped at `limit` — added 2026-08-07 during a live staging incident
    /// where Pending sat in the thousands with In-flight persistently 0 and
    /// GetHealingStatus's aggregate counts gave no way to see WHY (stuck
    /// waiting on a source? already at RF but never cleared? genuinely
    /// mid-transfer?). dfs-admin healing pending. Leader-only; a non-leader
    /// answers with an empty sample (its own pending_healing is always empty
    /// — see live_chunk_ids_cache's doc comment on why only the leader runs
    /// discovery/drain at all). APPENDED at end to preserve wire compatibility.
    GetPendingHealingSample {
        limit: usize,
    },
}

/// See Request::ProposeFold's doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposeFoldOutcome {
    /// Agreement confirmed (or proposer's accumulator is strictly more
    /// complete) — responder promises to stand down for FOLD_COORD_LOCK_TTL.
    Granted,
    /// Responder had nothing pending for this slot at all — nothing to
    /// protect, proposer may proceed. Functionally identical to Granted for
    /// the proposer; kept distinct only for observability.
    NothingPending,
    /// Responder already promised this exact slot to a THIRD node (RF>2
    /// corner case — unreachable in a dual-RF cluster, which has only one peer).
    DeclinedAlreadyLocked,
    /// Responder is already mid-fold (or mid-ordinary-patch-write) for this
    /// exact slot right now (chunk_patch_locks contended) — it is already the
    /// de facto coordinator; proposer should stand down and adopt normally.
    DeclinedAlreadyFolding,
    /// Simultaneous mutual proposal: proposer lost the (proposed_at_ms,
    /// node_id) tiebreak. Proposer should NOT retry this slot itself — the
    /// winning side's own inbound ProposeFold (already in flight the other
    /// direction) is what drives the actual fold.
    DeclinedCollision,
    /// Same base_chunk_id, but responder's own delta_chunk_id disagrees and
    /// its delta_size_hint is larger — proposer's accumulator is stale.
    /// Responder proactively kicks off its own coordinate_and_fold_slot in
    /// the background when it returns this.
    DeclinedStaleProposer,
    /// Responder's base_chunk_id disagrees entirely — proposer's view of
    /// "current generation" for this slot is stale (a fold already advanced
    /// it, whose broadcast hasn't reached the proposer yet). Proposer should
    /// not fold; a retry after normal backoff resolves once dissemination
    /// catches up.
    DeclinedBaseMismatch,
    /// ANOMALY: same base_chunk_id AND same delta_size_hint, but different
    /// delta_chunk_id — should be impossible from correctly-ordered patch
    /// delivery. Neither side folds; logged loudly on the responder.
    DeclinedDivergentAccumulators,
}

/// See Request::ReleaseFoldLock's doc comment. Purely informational for the
/// receiver's own logging — the grant is cleared identically either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FoldReleaseOutcome {
    Completed,
    Failed,
}

/// Wire-format mirror of dfs-server's internal `metadata::PatchState`, for
/// Request::GetPatchState's response. Kept as a separate type (not the
/// server-internal one reused directly) since dfs-common must not depend on
/// dfs-server — see that request's doc comment for why this RPC exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RemotePatchState {
    Pending {
        base_chunk_id: ChunkId,
        delta_chunk_id: ChunkId,
        size: usize,
        written_at: u64,
        client_write_seq: Option<u64>,
    },
    Folded(ChunkId),
}

/// Response types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    /// Reply to a Ping liveness probe. Payload-free — its mere arrival is the signal.
    Pong,
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

    /// Response to GetPatchTokenIds: every chunk_id currently outstanding in the
    /// responding node's local PATCH_STATE_TABLE.
    PatchTokenIds {
        ids: Vec<ChunkId>,
    },

    /// Response to GetPendingPatchChunkIds: the responding node's own
    /// all_pending_patch_chunk_ids() — base_chunk_id/delta_chunk_id for every
    /// currently-Pending row in its local PATCH_STATE_TABLE.
    PendingPatchChunkIds {
        ids: Vec<ChunkId>,
    },

    /// Response to GetOrphanAuthInfo — see that request's doc comment.
    /// `pending_patch_chunk_ids` is the same union GetPendingPatchChunkIds
    /// returns (base/delta inputs + outstanding tokens); `uptime_secs` is the
    /// same field NodeStats carries.
    OrphanAuthInfo {
        uptime_secs: u64,
        pending_patch_chunk_ids: Vec<ChunkId>,
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
        /// Configured link-bandwidth baseline (MB/s) — the adaptive controller's 100% mark.
        #[serde(default)]
        link_bandwidth_mb: usize,
        /// Configured healing bandwidth ceiling, percent of link_bandwidth_mb (10-100).
        #[serde(default)]
        heal_max_pct: f64,
        /// Configured max concurrent outstanding heal transfers.
        #[serde(default)]
        heal_max_concurrent: usize,
        /// Configured per-transfer timeout in seconds.
        #[serde(default)]
        heal_transfer_timeout_secs: u64,
        /// Configured max concurrent heal transfers any single node may be party to.
        /// Appended at the end, not inserted mid-list — bincode is positional, so a
        /// mid-list insertion would misalign every field after it on wire-version skew.
        #[serde(default)]
        heal_max_concurrent_per_node: usize,
        /// Patches currently pending their background fold, cluster-wide on this
        /// node (deferred chunk-patch consolidation) — PATCH_STATE_TABLE rows still
        /// in the Pending state. Same acceptable field-addition reasoning as the
        /// fields above (live, non-persisted RPC response).
        #[serde(default)]
        pending_patches_outstanding: usize,
        /// Configured delay (seconds) before a "never fully replicated" chunk becomes
        /// eligible for healing — see HealingManager::should_heal's doc comment.
        /// Appended at the end for the same bincode-positional reason as the field above.
        #[serde(default)]
        healing_delay_secs: u64,
        /// Live count of dirty_patch_slots — chunk-patch accumulators actively
        /// tracked for a background fold on THIS node right now, distinct from
        /// pending_patches_outstanding above (a durable PATCH_STATE_TABLE row
        /// count, unaffected by process restarts). dirty_patch_slots is in-
        /// memory-only and this is what was missing 2026-08-08 when a node's
        /// sustained CPU load from a stuck-fold retry storm was invisible in
        /// this same status output — pending_patches_outstanding read 0 on the
        /// leader (a different node) while the actual backlog sat on a
        /// follower, and nothing here showed dirty_patch_slots at all.
        /// Appended at the end for the same bincode-positional reason as the
        /// fields above.
        #[serde(default)]
        dirty_fold_slots: usize,
        /// Of dirty_fold_slots, how many have crossed
        /// MAX_FOLD_FAILURES_BEFORE_ESCALATION — i.e. are backed off to the
        /// 30-minute escalated retry interval rather than genuinely fresh or
        /// recently-touched. A nonzero count here on a healthy-looking node is
        /// exactly the signal that was missing: chronically-failing folds
        /// consuming real CPU on every retry, invisible in pending_count/
        /// in_flight_count/stalled_count (all of which are about under-
        /// replication healing, a completely different subsystem from fold
        /// coalescing). Appended at the end for the same reason as above.
        #[serde(default)]
        dirty_fold_slots_escalated: usize,
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
        /// This replica's recorded chunk_seq for the slot after applying, if the
        /// request carried one — see CHUNK_SEQ_TABLE's doc comment. None if the
        /// request didn't carry new_chunk_seq.
        #[serde(default)]
        chunk_seq: Option<u64>,
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
        /// This replica's recorded chunk_seq for the slot after applying, if the
        /// request carried one — see CHUNK_SEQ_TABLE's doc comment. None if the
        /// request didn't carry new_chunk_seq.
        #[serde(default)]
        chunk_seq: Option<u64>,
    },

    /// Response to ForceFold — the slot's real, folded content identity. The
    /// client uses this directly as the base chunk_id for its next patch to
    /// this slot, same as it would use MultiPatchResult's new_chunk_id.
    ForceFoldResult {
        real_chunk_id: ChunkId,
        size: usize,
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

    /// The leader's detect_metadata_write_seq_gap found a genuine write_seq gap for this
    /// file (see PutFileMetadata's covers_from_write_seq doc comment) — some prior push
    /// never reached any leader (client crash before delivery, or a queue bug), so this
    /// leader's chunk_locations for the file may be incomplete. The incoming push that
    /// triggered this was still accepted and persisted as normal; this is purely an
    /// additional signal on top of that Ok. Client should follow up by sending a full
    /// authoritative chunk_locations snapshot for this file (covers_from_write_seq=0, so
    /// this same detector never re-flags the resync push itself) — the existing
    /// union-only chunk_map merge (merge_file_metadata, see 08a6201) makes this safe to
    /// send at any time: it can only fill in what the leader is missing, never delete.
    /// APPENDED at end to preserve wire compatibility — see ChunkStale's doc comment on
    /// this rule.
    ResyncMetadataRequested {
        file_id: FileId,
    },

    /// Response to ProposeFold — see Request::ProposeFold's doc comment.
    ProposeFoldResult {
        outcome: ProposeFoldOutcome,
    },

    /// Response to GetRpcClassCounts: cumulative-since-startup RPC counts by
    /// class, plus local chunk-delete counts by reason tag (reuses
    /// delete_chunk's existing reason strings, e.g. ("live_file_orphan_sweep",
    /// 42) — an open-ended list since the tag set lives in dfs-server's
    /// storage.rs, not the wire protocol, so this side stays generic). Both
    /// in-memory only, not durable. APPENDED at end to preserve wire
    /// compatibility with older nodes, matching NodeStats' shape above.
    RpcClassCounts {
        peer_healing: u64,
        peer_delete_ops: u64,
        peer_fold: u64,
        peer_gossip: u64,
        peer_other: u64,
        client_full_patch: u64,
        client_multi_patch: u64,
        client_fold: u64,
        client_other: u64,
        admin: u64,
        delete_reasons: Vec<(String, u64)>,
    },

    /// Response to Request::GetPatchState. None means this node has no local
    /// patch_state record for the requested token — either it's genuinely
    /// unknown here, or the id was never a real token to begin with (see
    /// PATCH_TOKEN_MARKER's doc comment on the ~1/65536-per-chunk false-
    /// positive rate the caller must tolerate). APPENDED at end to preserve
    /// wire compatibility, matching every other addition in this enum.
    PatchStateResult {
        state: Option<RemotePatchState>,
    },

    /// Response to Request::GetPendingHealingSample. Oldest-first, capped at
    /// the request's `limit`. `total_pending` is the true total queue depth
    /// (matching GetHealingStatus's Pending count) so the caller knows how
    /// representative this sample is. APPENDED at end to preserve wire
    /// compatibility.
    PendingHealingSample {
        entries: Vec<PendingHealingEntry>,
        total_pending: usize,
    },
}

/// One entry in a Response::PendingHealingSample. See that response's doc
/// comment for why this exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingHealingEntry {
    pub chunk_id: ChunkId,
    pub age_secs: u64,
    pub in_flight: bool,
    pub stalled: bool,
    /// None = alive_nodes_cache has no entry yet (this chunk would hit the
    /// cache-miss/probe path on its next drain_heal_queue cycle). Some(n) =
    /// last-known count of confirmed-alive replicas.
    pub cached_alive_count: Option<usize>,
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
        /// Sender's current heal bandwidth target (MB/s), Some only when the sender
        /// believes itself to be leader (the only node with an accurate heal queue
        /// depth). Lets followers throttle heal transfers off the cluster-wide
        /// signal instead of their own always-empty local queue.
        #[serde(default)]
        heal_bandwidth_target_mb: Option<usize>,
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

    /// Best-effort announcement that a node intends to self-elect for a planned
    /// offline compaction (see LeaveReason::PlannedCompaction). Every peer records
    /// the earliest intent it's seen recently; the proposer waits briefly after
    /// broadcasting and only proceeds if its own (node_id, proposed_at_ms) is the
    /// winner (earliest proposed_at_ms, node_id as tiebreak) — this is what
    /// prevents two nodes from both going offline for compaction at once, without
    /// needing a leader-arbitrated lock (whichever node happens to be leader
    /// needs to be able to compact too, via the same path as everyone else).
    CompactionIntent {
        node_id: NodeId,
        proposed_at_ms: u64,
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
