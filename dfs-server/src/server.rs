use crate::chunker::Chunker;
use crate::cluster::ClusterManager;
use crate::healing::HealingManager;
use crate::metadata::MetadataStore;
use crate::network::{MessageHandler, NetworkClient};
use crate::stats::OpsTracker;
use crate::storage::ChunkStorage;
use anyhow::{Context, Result};
use dfs_common::{
    ChunkId, ChunkLocation, ClusterMessage, ErrorCode, FileId, FileMetadata,
    Message, NodeId, Request, Response,
};
use dashmap::DashMap;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Cached storage statistics to avoid expensive stat calls
#[derive(Clone)]
struct StorageStatsCache {
    total_chunks: usize,
    total_space: u64,
    free_space: u64,
    available_space: u64,
    timestamp: std::time::Instant,
}

/// Main server context holding all components
/// This is the core of the DFS node
pub struct Server {
    /// Local chunk storage
    storage: Arc<ChunkStorage>,

    /// Metadata store
    metadata: Arc<MetadataStore>,

    /// File chunker
    chunker: Arc<Chunker>,

    /// Cluster manager
    cluster: Arc<ClusterManager>,

    /// Network client for talking to other nodes
    client: Arc<NetworkClient>,

    /// Replication factor
    replication_factor: usize,

    /// Metadata directory path for persisting peer list
    metadata_dir: PathBuf,

    /// Storage stats cache with 10-second TTL
    storage_stats_cache: Arc<RwLock<Option<StorageStatsCache>>>,

    /// Prefetch concurrency limiter - prevents prefetch from overwhelming disk I/O
    /// Limits low-priority prefetch operations while allowing unlimited high-priority reads
    prefetch_semaphore: Arc<tokio::sync::Semaphore>,

    /// Outbound broadcast concurrency limiter - caps total in-flight cluster RPCs
    /// (metadata replication, purge broadcasts) to prevent fd exhaustion when
    /// many operations fan out to all 5 nodes simultaneously.
    broadcast_semaphore: Arc<tokio::sync::Semaphore>,

    /// Separate semaphore for delete fan-out tasks (leader-only Ok(None) path).
    /// Kept small (4) so a burst of deletes doesn't starve metadata broadcasts
    /// or exhaust the broadcast_semaphore and stall heartbeat-adjacent operations.
    delete_semaphore: Arc<tokio::sync::Semaphore>,

    /// Leader-maintained in-memory chunk map: FileId -> Vec<ChunkLocation>.
    /// Only the leader serves GetFileChunkMap requests from this map.
    /// All nodes keep it up-to-date so leadership transitions are seamless.
    /// Updated on every write, heal, and chunk-location replication.
    chunk_map: Arc<DashMap<FileId, (Vec<ChunkLocation>, u64)>>,

    /// Permanently-missing chunk blocklist.
    /// Chunks that recently failed reads from all online nodes. Prevents connection
    /// storms on repeated retries. TTL-based: entries expire after 5 minutes so chunks
    /// become retryable after a node recovers. Never grows unboundedly.
    missing_chunks: Arc<RwLock<std::collections::HashMap<ChunkId, std::time::Instant>>>,

    /// Reference to the healing manager — set after construction via set_healing_manager().
    /// Used by admin handlers to query status and trigger immediate heal cycles.
    healing: Arc<RwLock<Option<Arc<HealingManager>>>>,

    /// Follower-to-leader forward queue.
    /// When this node is a follower and receives a ReplicateMetadata, it enqueues
    /// the metadata here. A background task forwards each entry to the current leader
    /// with retries and a 1-minute deadline. On leader change: if we became leader,
    /// drain immediately as local stores; otherwise re-route to the new leader.
    leader_forward_queue: Arc<tokio::sync::Mutex<std::collections::VecDeque<(FileMetadata, std::time::Instant)>>>,

    /// Notifier for the leader forward queue background task.
    leader_forward_notify: Arc<tokio::sync::Notify>,

    /// Short-term gossip ring: metadata written by this node in the last ~30s.
    /// Keyed by FileId for O(1) insert+dedup — replaces the old Mutex<VecDeque>
    /// which caused contention at >100 writes/sec (rsync storms, bulk copies).
    recent_writes: Arc<DashMap<FileId, (FileMetadata, std::time::Instant)>>,

    /// Dirty-file buffer for the metadata healer.
    /// handle_put_file_metadata inserts here after every write; the healer drains
    /// it every 5s and pushes authoritative metadata (rebuilt from chunk_map) to
    /// all online followers. DashMap deduplicates by FileId so write storms produce
    /// at most one push per file per 5s window.
    pending_broadcasts: Arc<DashMap<FileId, FileMetadata>>,

    /// Tracks the highest write_seq seen per file across PutFileMetadata calls.
    /// Used to bypass the per-chunk written_at guard when a newer write_seq arrives:
    /// write_seq is a monotonically-increasing client-managed counter (clock-agnostic)
    /// that reliably identifies whether incoming metadata is from a newer session than
    /// what's stored. Any PutFileMetadata with write_seq > stored bypasses the guard
    /// entirely — the entire incoming metadata is accepted as authoritative.
    file_write_seqs: Arc<DashMap<FileId, u64>>,

    /// Delete tombstone set: FileId -> deleted_at instant.
    /// Any PutFileMetadata that arrives for an ID in this set within TOMBSTONE_TTL
    /// is rejected so that in-flight seq=0 creates can't resurrect a deleted file.
    /// Entries are expired lazily on each put check.
    delete_tombstones: Arc<DashMap<FileId, std::time::Instant>>,

    /// Chunk-level tombstones: chunk_ids that must not be used as a heal source.
    /// Set synchronously by TombstoneChunk during dual-RF MultiPatch so the healer
    /// can't replicate old_chunk_id back to the patched replicas before metadata commits.
    /// Cleared when the chunk is physically deleted (DeleteChunk / DeleteChunksBatch).
    chunk_tombstones: Arc<dashmap::DashSet<dfs_common::ChunkId>>,

    /// Notifier for the delete drain worker — fired when a new DeleteQueueEntry is added.
    delete_drain_notify: Arc<tokio::sync::Notify>,

    /// Channel to a dedicated sled-write worker thread.
    /// All metadata put_file calls are serialized through here — one std::thread
    /// processes them sequentially, eliminating the futex pile-up from concurrent
    /// spawn_blocking calls all contending on sled's internal write lock.
    /// Wrapped in Mutex<Option<...>> so drain_sled_writes() can close the channel
    /// by dropping the sender, then wait for the worker thread to drain completely.
    sled_write_tx: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<FileMetadata>>>>,

    /// Notified by the sled-write worker when it has drained all pending items
    /// and exited (after the sender is dropped). Used by drain_sled_writes().
    sled_write_done: Arc<tokio::sync::Notify>,

    /// Per-chunk serialization locks for MultiPatch.
    /// Serializes all patches to the same (FileId, chunk_idx) pair so that two
    /// concurrent patches (A→B and B→C) cannot race in spawn_blocking: without
    /// this, the second patch passes the stale check and enters spawn_blocking
    /// while the first is still renaming A→B, and fails with "file not found".
    chunk_patch_locks: Arc<DashMap<(FileId, u64), Arc<tokio::sync::Mutex<()>>>>,

    /// Per-node ops/sec tracker — read, write, and metadata op counts in a
    /// 3600-bucket ring (one bucket per second). Near-zero overhead: atomic
    /// fetch_add per op, single mutex acquire per second for the ring write.
    ops_tracker: Arc<OpsTracker>,

    /// Shared connection semaphore from the NetworkServer.
    /// None until set_conn_semaphore() is called from main after the server starts.
    conn_semaphore: Arc<RwLock<Option<Arc<tokio::sync::Semaphore>>>>,
}

impl Server {
    /// Create a new server instance
    pub fn new(
        storage: Arc<ChunkStorage>,
        metadata: Arc<MetadataStore>,
        chunk_size: usize,
        cluster: Arc<ClusterManager>,
        replication_factor: usize,
        metadata_dir: PathBuf,
    ) -> Self {
        // Create tombstones before the struct so the sled_write_tx worker can
        // capture a clone and guard against writing metadata for deleted files.
        let delete_tombstones: Arc<DashMap<FileId, std::time::Instant>> = Arc::new(DashMap::new());
        let tombstones_for_worker = delete_tombstones.clone();

        // Build sled worker channel and done-notify before the struct literal so the
        // thread captures the correct Arc clones (struct fields can't cross-reference
        // each other within a single literal).
        let sled_write_done: Arc<tokio::sync::Notify> = Arc::new(tokio::sync::Notify::new());
        let sled_write_tx: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<FileMetadata>>>> = {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<FileMetadata>();
            let meta_bg = metadata.clone();
            let done_notify = sled_write_done.clone();
            std::thread::spawn(move || {
                while let Some(m) = rx.blocking_recv() {
                    // Guard: don't resurrect a file that was deleted after this
                    // write was queued (race between ReplicateChunkLocation and
                    // handle_delete_file). The tombstone is set by handle_delete_file
                    // before removing from sled, so if it's present here the delete
                    // wins and we discard the stale metadata update.
                    if tombstones_for_worker.contains_key(&m.id) {
                        debug!("sled_write_worker: skipping tombstoned file {} ({})", m.path, m.id);
                        continue;
                    }
                    if let Err(e) = meta_bg.put_file(&m) {
                        warn!("sled_write_worker: put_file failed for {}: {}", m.path, e);
                    }
                }
                // All pending items have been committed — signal drain_sled_writes().
                done_notify.notify_waiters();
            });
            Arc::new(std::sync::Mutex::new(Some(tx)))
        };

        let server = Self {
            storage,
            metadata: metadata.clone(),
            chunker: Arc::new(Chunker::new(chunk_size)),
            cluster,
            client: Arc::new(NetworkClient::new()),
            replication_factor,
            metadata_dir,
            storage_stats_cache: Arc::new(RwLock::new(None)),
            // Allow 8 concurrent prefetch operations for faster cache warming
            // With modern HDDs and read-ahead, parallel reads are efficient
            // Real client reads bypass this limit (high priority)
            prefetch_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
            // Cap total outbound broadcast RPCs to 20 at a time across all operations.
            // 5 nodes × 4 concurrent fan-outs = 20 max simultaneous cluster connections,
            // well within the 65536 fd limit even under heavy delete/heal load.
            broadcast_semaphore: Arc::new(tokio::sync::Semaphore::new(20)),
            delete_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
            chunk_map: Arc::new(DashMap::new()),
            missing_chunks: Arc::new(RwLock::new(std::collections::HashMap::new())),
            healing: Arc::new(RwLock::new(None)),
            leader_forward_queue: Arc::new(tokio::sync::Mutex::new(std::collections::VecDeque::new())),
            leader_forward_notify: Arc::new(tokio::sync::Notify::new()),
            recent_writes: Arc::new(DashMap::new()),
            pending_broadcasts: Arc::new(DashMap::new()),
            delete_tombstones,
            chunk_tombstones: Arc::new(dashmap::DashSet::new()),
            file_write_seqs: Arc::new(DashMap::new()),
            delete_drain_notify: Arc::new(tokio::sync::Notify::new()),
            chunk_patch_locks: Arc::new(DashMap::new()),
            ops_tracker: Arc::new(OpsTracker::new()),
            conn_semaphore: Arc::new(RwLock::new(None)),
            sled_write_done,
            sled_write_tx,
        };

        server
    }

    pub fn metadata_store(&self) -> Arc<MetadataStore> {
        self.metadata.clone()
    }

    /// Close the sled-write channel and wait for the worker thread to commit all
    /// pending metadata writes before returning. Called during graceful shutdown so
    /// no queued PutFileMetadata writes are lost when the process exits.
    pub async fn drain_sled_writes(&self) {
        // Drop the sender — this closes the channel and signals the worker to drain.
        self.sled_write_tx.lock().unwrap().take();
        // Wait for the worker thread to finish committing all remaining items.
        self.sled_write_done.notified().await;
        // All items are now committed with Durability::None (in OS page cache).
        // One empty Durability::Immediate commit promotes the entire accumulated list
        // to physical disk via fdatasync — same trick used by compact_db().
        if let Err(e) = self.metadata.flush_durable() {
            warn!("drain_sled_writes: flush_durable failed: {}", e);
        }
        info!("drain_sled_writes: all pending metadata writes flushed to disk");
    }

    /// Rebuild the in-memory chunk map by scanning all file metadata.
    /// Called once at startup; incremental updates happen via chunk_map_update().
    /// Uses scan_files (streaming) to avoid loading the entire metadata set into
    /// RAM at once — on a node with 535 MB of sled metadata, list_files() was
    /// materialising a 2 GB Vec<FileMetadata> and triggering OOM-like behaviour.
    pub fn rebuild_chunk_map_from_metadata(&self) {
        let chunk_map = self.chunk_map.clone();
        let file_write_seqs = self.file_write_seqs.clone();
        let metadata = self.metadata.clone();

        std::thread::spawn(move || {
            let mut built = 0usize;
            let mut total = 0usize;

            let result = metadata.scan_files(|file| {
                total += 1;
                if !file.chunk_locations.is_empty() {
                    // or_insert_with: don't overwrite entries already added by incoming RCLs
                    // while the scan is running — the RCL is always newer than redb state.
                    chunk_map.entry(file.id).or_insert_with(|| (file.chunk_locations.clone(), file.write_seq));
                    built += 1;
                }
                // Seed file_write_seqs so the bypass guard has a correct baseline
                // on startup — prevents stale messages from a previous session
                // (with lower write_seq) from bypassing the guard after restart.
                if file.write_seq > 0 {
                    file_write_seqs.insert(file.id, file.write_seq);
                }
                Ok(())
            });

            match result {
                Ok(()) => info!("Chunk map built: {} / {} files indexed", built, total),
                Err(e) => warn!("Chunk map build failed partway through: {}", e),
            }
        });
    }

    /// Update the chunk map for a single file — called after every metadata write or heal.
    async fn chunk_map_update(&self, metadata: &FileMetadata) {
        if !metadata.chunk_locations.is_empty() {
            // Atomic staleness guard: build the chunk_locations to store, preserving any
            // existing chunk whose written_at is newer than the incoming one. This prevents
            // the GHOST-reversion race where a concurrent RCL updates the chunk_map to a
            // newer chunk Y AFTER a broadcast pre-guard read it (and decided to keep Y) but
            // BEFORE this insert runs — causing the stale broadcast to overwrite Y with X.
            //
            // Moving the guard here makes the check+insert atomic for all callers, not just
            // the ones with pre-guards (handle_replicate_metadata[_batch]). The disk-existence
            // check is kept for the diagnostic warning but the BLOCK is timestamp-only so it
            // applies even when neither file exists locally (e.g., on non-replica nodes).
            let new_locs: Vec<ChunkLocation> = if let Some(existing_entry) = self.chunk_map.get(&metadata.id) {
                let (existing_locs, _) = existing_entry.value();
                let mut locs = metadata.chunk_locations.clone();
                for loc in locs.iter_mut() {
                    if loc.file_offset.is_none() { continue; }
                    if let Some(old_loc) = existing_locs.iter().find(|l| l.file_offset == loc.file_offset) {
                        if old_loc.chunk_id == loc.chunk_id { continue; }
                        // Staleness guard: prefer client_write_seq (monotone, clock-agnostic)
                        // then fall back to written_at. Rejects stale follower metadata that
                        // arrives via push_held_file_metadata_to with a lower write_seq.
                        let stale = match (loc.client_write_seq, old_loc.client_write_seq) {
                            (Some(inc), Some(ext)) => inc < ext,
                            (Some(_), None)        => false, // incoming has seq → newer
                            (None, Some(_))        => true,  // existing has seq, incoming doesn't → stale
                            (None, None)           => {
                                if let Some(incoming_ts) = loc.written_at {
                                    let existing_ts = old_loc.written_at.unwrap_or(0);
                                    existing_ts > incoming_ts
                                } else {
                                    // No seq or timestamp on either side (pre-Fix-C legacy data).
                                    // Treat as stale if the incoming chunk_id doesn't exist on
                                    // disk — it was already superseded by a later patch.
                                    !self.storage.get_chunk_path(&loc.chunk_id).exists()
                                }
                            }
                        };
                        if stale {
                            let incoming_missing = !self.storage.get_chunk_path(&loc.chunk_id).exists();
                            if incoming_missing {
                                warn!("[GHOST-reversion] chunk_map_update: path={} offset={:?} seq={} OLD={} (cws={:?}) → NEW={} (exists=false cws={:?}) — stale broadcast BLOCKED",
                                    metadata.path, loc.file_offset, metadata.write_seq,
                                    old_loc.chunk_id, old_loc.client_write_seq,
                                    loc.chunk_id, loc.client_write_seq);
                            }
                            *loc = old_loc.clone();
                        }
                    }
                }
                locs
            } else {
                metadata.chunk_locations.clone()
            };
            self.chunk_map.insert(metadata.id, (new_locs, metadata.write_seq));
        } else if self.chunk_map.contains_key(&metadata.id) {
            // Empty chunk_locations on a file that already has a chunk_map entry means
            // truncate-to-zero. Reset the entry instead of leaving it untouched —
            // otherwise the stale pre-truncate chunks would linger in chunk_map and
            // get resurrected by handle_put_file_metadata's chunk_map union (below)
            // on the next write to this file.
            self.chunk_map.insert(metadata.id, (vec![], metadata.write_seq));
        }
        // If the file has no chunks yet (new empty file) and no map entry exists,
        // no map entry is needed — chunk_map_update_location_for_file will create
        // one lazily on the first ReplicateChunkLocation.
    }

    /// Update a single chunk location within the chunk map (used during healing).
    /// Finds all files that reference this chunk and patches the location in place.
    async fn chunk_map_update_location(&self, location: &ChunkLocation) {
        for mut entry in self.chunk_map.iter_mut() {
            let (locs, _) = entry.value_mut();
            for loc in locs.iter_mut() {
                if loc.chunk_id == location.chunk_id {
                    // Exact match — update in place.
                    *loc = location.clone();
                    return;
                }
            }
            // Do NOT fall back to file_offset matching when file_id is unknown.
            // chunk_map_update_location is only called from handle_replicate_chunk_location
            // when file_id=None (fresh writes from write_chunk_to_replicas). Every file
            // has a chunk at file_offset=0, so a scan-all-files match by offset alone is
            // non-deterministic and corrupts the wrong file's chunk_map entry (T8 race:
            // t8_big.bin's ReplicateChunkLocation can hit t8_persist.txt first).
            // Exact chunk_id match above covers the update case; PutFileMetadata from
            // flush_metadata_sync is the authoritative update for fresh writes.
        }
    }

    /// Targeted variant: update a single chunk location for a known file_id.
    /// Avoids the scan-all-files fallback that `chunk_map_update_location` uses,
    /// which incorrectly matches file_offset=0 on the first file it finds rather
    /// than the actual file being updated.
    ///
    /// Called exclusively from handle_replicate_chunk_location — a client-sent message
    /// reporting the live result of a just-completed MultiPatch.
    ///
    /// Ordering for the file_offset path: prefer client_write_seq (monotone counter from
    /// the client, clock-agnostic) when available; fall back to written_at (server-side
    /// timestamp) for legacy records that predate the client_write_seq field.
    /// Fresh writes carry client_write_seq=None so any patch (seq > 0) always wins.
    async fn chunk_map_update_location_for_file(&self, file_id: FileId, location: &ChunkLocation) {
        // Use entry() to atomically create-or-get: for brand-new files that have no
        // chunk_map entry yet, this inserts an empty Vec so subsequent logic can push
        // the first chunk in. Without this, every ReplicateChunkLocation for a new file
        // was a silent no-op and chunk_map stayed empty for the entire recording session.
        // Seed the 2nd field from this location's client_write_seq (clock-agnostic) —
        // not wall-clock — since chunk_map's 2nd field is write_seq-space.
        let mut entry = self.chunk_map.entry(file_id)
            .or_insert_with(|| (vec![], location.client_write_seq.unwrap_or(0)));
        let (locs, _) = entry.value_mut();
        // First try exact chunk_id match.
        for loc in locs.iter_mut() {
            if loc.chunk_id == location.chunk_id {
                *loc = location.clone();
                return;
            }
        }
        // Fallback: match by file_offset (covers PatchChunk where chunk_id changes).
        // Guard: reject if incoming is older than what we already have.
        // Prefer client_write_seq (clock-agnostic monotone counter) over written_at.
        if let Some(file_offset) = location.file_offset {
            for loc in locs.iter_mut() {
                if loc.file_offset == Some(file_offset) {
                    let should_update = match (location.client_write_seq, loc.client_write_seq) {
                        (Some(inc), Some(ext)) => inc >= ext,
                        (Some(_), None)        => true,  // incoming has seq, existing is legacy → accept
                        (None, Some(_))        => false, // existing has seq, incoming is legacy → keep
                        (None, None)           => {
                            // Legacy path: both records predate client_write_seq → use written_at
                            location.written_at.unwrap_or(0) >= loc.written_at.unwrap_or(0)
                        }
                    };
                    if should_update {
                        *loc = location.clone();
                    } else {
                        // Stale RCL rejected: log so we can confirm the guard is working.
                        debug!("[RCL-stale-rejected] file={:?} offset={:?} kept={} (seq={:?}) dropped={} (seq={:?})",
                            file_id, location.file_offset, loc.chunk_id, loc.client_write_seq,
                            location.chunk_id, location.client_write_seq);
                    }
                    return;
                }
            }
        }
        // No existing entry matched by chunk_id or file_offset — new chunk for this file.
        // Push so chunk_map grows incrementally as chunks are written rather than waiting
        // until file close (PutFileMetadata) to learn about the file's chunks.
        locs.push(location.clone());
    }

    /// Remove a file from the chunk map (on deletion).
    async fn chunk_map_remove(&self, file_id: &FileId) {
        self.chunk_map.remove(file_id);
    }

    /// Find which file a chunk belongs to by scanning the chunk_map.
    /// Returns None if chunk not found in any file.
    fn find_file_by_chunk(&self, chunk_id: &ChunkId) -> Option<FileId> {
        for entry in self.chunk_map.iter() {
            let file_id = *entry.key();
            let (locations, _) = entry.value();
            if locations.iter().any(|loc| &loc.chunk_id == chunk_id) {
                return Some(file_id);
            }
        }
        None
    }

    /// Pull fresh metadata from the leader and store it locally.
    /// Used for self-healing when we detect we have stale metadata.
    async fn pull_metadata_from_leader(&self, file_id: FileId) -> Result<()> {
        let leader_addr = self.cluster.get_leader_addr().await
            .ok_or_else(|| anyhow::anyhow!("No leader available"))?;

        let req = Request::GetFileInfoById { file_id };
        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            self.client.send_message(leader_addr, Message::Request(req)),
        ).await??;

        match result.message {
            Message::Response(Response::FileMetadata { metadata }) => {
                // Storage nodes now update their chunk_map atomically after each patch,
                // so they may be AHEAD of the leader (async metadata queue lag). Only
                // accept leader data if it is strictly newer than our local copy.
                // Applying stale leader metadata would regress our chunk_map from the
                // current chunk_id (Y, just patched) back to the old one (X, pre-patch).
                let local_seq = self.metadata.get_file(&file_id)
                    .ok().flatten()
                    .map(|m| m.write_seq)
                    .unwrap_or(0);
                if metadata.write_seq > local_seq {
                    self.metadata.put_file(&metadata)?;
                    self.chunk_map_update(&metadata).await;
                    info!("Successfully pulled fresh metadata from leader: file_id={} seq={} size={}",
                          file_id, metadata.write_seq, metadata.size);
                } else {
                    info!("Skipping leader metadata pull — local seq={} >= leader seq={} for file_id={}; \
                           storage node is ahead (patch in flight to leader)",
                          local_seq, metadata.write_seq, file_id);
                }
                Ok(())
            }
            Message::Response(Response::Error { message, .. }) => {
                Err(anyhow::anyhow!("Leader returned error: {}", message))
            }
            _ => {
                Err(anyhow::anyhow!("Unexpected response from leader"))
            }
        }
    }

    /// Get reference to cluster manager
    pub fn cluster(&self) -> Arc<ClusterManager> {
        self.cluster.clone()
    }

    /// Get reference to network client
    pub fn network_client(&self) -> Arc<NetworkClient> {
        self.client.clone()
    }

    /// Wire in the healing manager after construction.
    /// Called from main() once both Server and HealingManager are created.
    pub async fn set_healing_manager(&self, healing: Arc<HealingManager>) {
        *self.healing.write().await = Some(healing);
    }

    /// Share the NetworkServer's connection semaphore with the Server.
    /// Called from main() after NetworkServer is created, before it starts.
    pub async fn set_conn_semaphore(&self, sem: Arc<tokio::sync::Semaphore>) {
        *self.conn_semaphore.write().await = Some(sem);
    }

    /// Background task: monitor TCP connection slot pressure and step down from
    /// leadership when all slots are exhausted for a sustained period.
    ///
    /// Thresholds (MAX_CONNECTIONS = 128):
    ///   75% used (96+)  → WARN on each accept (done in network.rs)
    ///   100% for 30s    → check CLOSE_WAIT; if real load, GracefulLeave
    ///   recovery >50%   → announce_recovery
    ///   100% for 5 min  → exit(1) so systemd can restart cleanly
    pub fn start_conn_pressure_watchdog(self: Arc<Self>) {
        use crate::network::MAX_CONNECTIONS;
        tokio::spawn(async move {
            let mut full_since: Option<std::time::Instant> = None;
            let mut stepped_down = false;

            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

                let sem = match self.conn_semaphore.read().await.clone() {
                    Some(s) => s,
                    None => continue, // not wired up yet
                };

                let available = sem.available_permits();
                let in_use = MAX_CONNECTIONS - available;

                if in_use >= MAX_CONNECTIONS {
                    // Fully exhausted.
                    let since = *full_since.get_or_insert(std::time::Instant::now());
                    let secs_full = since.elapsed().as_secs();

                    // Last resort: if still stuck after 5 minutes, restart.
                    if secs_full >= 300 {
                        tracing::error!(
                            "Connection slots exhausted for {}s — restarting via exit(1)",
                            secs_full
                        );
                        std::process::exit(1);
                    }

                    // After 30s at 100%, check whether it's CLOSE_WAIT or real load.
                    if secs_full >= 30 && !stepped_down {
                        let port = self.cluster.local_addr().port();
                        let close_wait = count_close_wait_connections(port);
                        let half = MAX_CONNECTIONS / 2;

                        if close_wait >= half {
                            // Mostly leaked CLOSE_WAIT — keepalive will clean these up,
                            // no need to step down yet.
                            tracing::warn!(
                                "Connection pressure: {}/{} in use, {} CLOSE_WAIT — waiting for keepalive cleanup",
                                in_use, MAX_CONNECTIONS, close_wait
                            );
                        } else {
                            // Genuinely active connections: step down from leadership.
                            tracing::error!(
                                "Connection pressure: {}/{} in use ({} CLOSE_WAIT) for {}s — stepping down from leadership",
                                in_use, MAX_CONNECTIONS, close_wait, secs_full
                            );
                            self.cluster.announce_leaving(dfs_common::LeaveReason::ConnectionPressure).await;
                            stepped_down = true;
                        }
                    }
                } else {
                    // Pressure has eased.
                    full_since = None;
                    if stepped_down && available > MAX_CONNECTIONS / 2 {
                        stepped_down = false;
                        self.cluster.announce_recovery().await;
                    }
                }
            }
        });
    }

    /// Start the leader metadata dissemination loop.
    ///
    /// Every 5 seconds (when this node is the leader), drain the per-follower
    /// sled queue and send any pending metadata updates to each follower that
    /// is currently online. Entries are removed only after the follower acks.
    ///
    /// On leader election (was_leader → is_leader transition), this loop also
    /// runs a catch-up pass: it queries each follower's last received sequence
    /// and re-enqueues any metadata the follower is missing.
    pub fn start_metadata_dissemination_loop(self: Arc<Self>) {
        let server = self.clone();
        tokio::spawn(async move {
            let mut was_leader = false;
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

                let is_leader = server.cluster.is_leader().await;

                // On leadership acquisition, announce to all peers immediately so any
                // concurrent split-brain leader with a higher NodeId concedes.
                if is_leader && !was_leader {
                    // Carry over the post-election grace period (LEADER_CHANGE_GRACE_SECS)
                    // if this node was already the leader before a restart, instead of
                    // resetting it to zero every time the perpetual lowest-NodeId leader
                    // restarts. A genuinely different leader still gets a fresh grace period.
                    let now_secs = dfs_common::types::current_timestamp();
                    let local_id = server.cluster.local_node_id();
                    let (prev_leader, prev_since) = server.metadata.get_leader_state().unwrap_or((None, None));
                    let became_leader_at_secs = crate::cluster::resolve_became_leader_epoch(prev_leader, prev_since, local_id, now_secs);
                    server.cluster.set_became_leader_epoch(became_leader_at_secs, now_secs).await;
                    if let Err(e) = server.metadata.put_leader_state(local_id, became_leader_at_secs) {
                        warn!("Failed to persist leader state: {}", e);
                    }
                    info!("Became leader — announcing leadership to all peers");
                    let nodes = server.cluster.get_all_nodes().await;
                    let local_id = server.cluster.local_node_id();
                    let local_addr = server.cluster.local_addr();
                    for node in &nodes {
                        if node.id == local_id { continue; }
                        let msg = dfs_common::Message::Cluster(
                            dfs_common::protocol::ClusterMessage::LeaderAnnouncement {
                                node_id: local_id,
                                addr: local_addr,
                            }
                        );
                        let client = server.client.clone();
                        let addr = node.addr;
                        tokio::spawn(async move {
                            let _ = client.send_message(addr, msg).await;
                        });
                    }
                    info!("Became leader — spawning metadata catch-up for all followers");
                    let server_catchup = server.clone();
                    tokio::spawn(async move {
                        let result = tokio::time::timeout(
                            tokio::time::Duration::from_secs(120),
                            server_catchup.run_metadata_catchup(),
                        ).await;
                        if result.is_err() {
                            warn!("Metadata catch-up timed out after 120s — dissemination loop will cover remaining gaps");
                        }
                    });
                }
                was_leader = is_leader;

                if !is_leader {
                    continue;
                }

                // Periodically re-run the full pull-merge catchup so the leader stays
                // converged with followers even when not re-electing. Without this, files
                // written to followers (pre-TTL or during brief leader unreachability) never
                // reach the leader until a restart. Run every 5 minutes.
                {
                    static LAST_CATCHUP: std::sync::OnceLock<std::sync::Mutex<std::time::Instant>> = std::sync::OnceLock::new();
                    let last = LAST_CATCHUP.get_or_init(|| std::sync::Mutex::new(std::time::Instant::now() - std::time::Duration::from_secs(300)));
                    let should_run = {
                        let t = last.lock().unwrap();
                        t.elapsed() >= std::time::Duration::from_secs(300)
                    };
                    if should_run {
                        *last.lock().unwrap() = std::time::Instant::now();
                        let server_catchup = server.clone();
                        tokio::spawn(async move {
                            let result = tokio::time::timeout(
                                std::time::Duration::from_secs(120),
                                server_catchup.run_metadata_catchup(),
                            ).await;
                            if result.is_err() {
                                warn!("Periodic metadata catch-up timed out after 120s");
                            }
                        });
                    }
                }

                // Drain the queue for every online follower.
                let nodes = server.cluster.get_all_nodes().await;
                let local_id = server.cluster.local_node_id();

                for node in &nodes {
                    if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                        continue;
                    }

                    // Compact + drain + filter — all blocking sled ops, off the async thread.
                    let metadata_for_drain = server.metadata.clone();
                    let node_id_for_drain = node.id;
                    let drain_result = tokio::task::spawn_blocking(move || {
                        metadata_for_drain.compact_meta_queue_for_node(node_id_for_drain)?;
                        let items = metadata_for_drain.drain_meta_queue_for_node(node_id_for_drain)?;
                        if items.is_empty() {
                            return Ok::<_, anyhow::Error>(None);
                        }
                        let up_to_sequence = items.last().map(|(s, _)| *s).unwrap_or(0);
                        // Filter out creates for files deleted since they were queued.
                        let batch: Vec<FileMetadata> = items.into_iter().filter_map(|(_, m)| {
                            match metadata_for_drain.get_file(&m.id) {
                                Ok(Some(_)) => Some(m),
                                _ => {
                                    debug!("disseminate: skipping deleted file {} ({}) from queue", m.path, m.id);
                                    None
                                }
                            }
                        }).collect();
                        Ok(Some((batch, up_to_sequence)))
                    }).await;

                    let (metadata_batch, up_to_sequence) = match drain_result {
                        Ok(Ok(Some(v))) => v,
                        Ok(Ok(None)) => continue,
                        Ok(Err(e)) => { warn!("meta_queue drain error for {}: {}", node.id, e); continue; }
                        Err(e) => { warn!("meta_queue drain panic for {}: {}", node.id, e); continue; }
                    };
                    let count = metadata_batch.len();

                    let req = Request::DisseminateMetadata {
                        items: metadata_batch,
                        up_to_sequence,
                    };

                    let result = tokio::time::timeout(
                        tokio::time::Duration::from_secs(10),
                        server.client.send_message(node.addr, Message::Request(req)),
                    ).await;

                    match result {
                        Ok(Ok(_)) => {
                            debug!("Disseminated {} metadata items to {} (seq≤{})", count, node.id, up_to_sequence);
                            let metadata_for_ack = server.metadata.clone();
                            let node_id_for_ack = node.id;
                            if let Err(e) = tokio::task::spawn_blocking(move ||
                                metadata_for_ack.ack_meta_queue_for_node(node_id_for_ack, up_to_sequence)
                            ).await {
                                warn!("meta_queue ack error for {}: {}", node.id, e);
                            }
                        }
                        Ok(Err(e)) => {
                            debug!("Metadata dissemination to {} failed (will retry): {}", node.id, e);
                        }
                        Err(_) => {
                            debug!("Metadata dissemination to {} timed out (will retry)", node.id);
                        }
                    }
                }
            }
        });
    }

    /// Start the follower-to-leader forward queue background task.
    ///
    /// When a follower receives ReplicateMetadata it enqueues the metadata here.
    /// This task drains the queue by forwarding each entry to the current leader:
    ///   - Retries with backoff up to a 1-minute deadline per entry.
    ///   - On leader change: if we became the leader, store locally and expire.
    ///                       if it's a new remote leader, re-route there.
    pub fn start_leader_forward_loop(self: Arc<Self>) {
        let server = self.clone();
        tokio::spawn(async move {
            let mut prev_leader: Option<std::net::SocketAddr> = None;
            loop {
                // Wait for something to appear, or wake on leader change (poll every 5s max).
                tokio::select! {
                    _ = server.leader_forward_notify.notified() => {}
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                }

                let current_leader = server.cluster.get_leader_addr().await;
                let leader_changed = current_leader != prev_leader;
                prev_leader = current_leader;

                // On leader change, check if we became leader — drain queue locally if so.
                if leader_changed && server.cluster.is_leader().await {
                    let mut queue = server.leader_forward_queue.lock().await;
                    if !queue.is_empty() {
                        info!("leader_forward: became leader — draining {} queued items locally", queue.len());
                        while let Some((metadata, _enqueued_at)) = queue.pop_front() {
                            // Skip files that were deleted since they were enqueued.
                            if server.metadata.get_file(&metadata.id).ok().flatten().is_none() {
                                debug!("leader_forward: skipping deleted file {} on leader drain", metadata.path);
                                continue;
                            }
                            match server.metadata.put_file(&metadata) {
                                Ok(_) => { server.chunk_map_update(&metadata).await; }
                                Err(e) => warn!("leader_forward: local store failed for {}: {}", metadata.path, e),
                            }
                        }
                    }
                    continue;
                }

                let leader_addr = match current_leader {
                    Some(addr) => addr,
                    None => continue, // No known leader yet — wait.
                };

                // Don't forward to ourselves.
                if leader_addr == server.cluster.local_addr() {
                    continue;
                }

                let mut backoff_ms = 2_000u64;
                loop {
                    let entry = {
                        let queue = server.leader_forward_queue.lock().await;
                        queue.front().cloned()
                    };
                    let (metadata, enqueued_at) = match entry {
                        Some(e) => e,
                        None => break,
                    };

                    // Expired — drop it (periodic catchup covers remaining gaps).
                    if enqueued_at.elapsed() > std::time::Duration::from_secs(60) {
                        warn!("leader_forward: dropping expired entry for {} (>60s)", metadata.path);
                        server.leader_forward_queue.lock().await.pop_front();
                        backoff_ms = 2_000;
                        continue;
                    }

                    // Re-check leader — may have changed while we were working.
                    let now_leader = server.cluster.get_leader_addr().await;
                    if now_leader != Some(leader_addr) {
                        break;
                    }

                    // Don't forward if the file was deleted locally since it was enqueued.
                    // Sending PutFileMetadata to the leader for a deleted file resurrects it.
                    if server.metadata.get_file(&metadata.id).ok().flatten().is_none() {
                        debug!("leader_forward: skipping deleted file {} ({})", metadata.path, metadata.id);
                        server.leader_forward_queue.lock().await.pop_front();
                        backoff_ms = 2_000;
                        continue;
                    }

                    let req = Request::PutFileMetadata { metadata: metadata.clone() };
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        server.client.send_message(leader_addr, Message::Request(req)),
                    ).await {
                        Ok(Ok(env)) => match env.message {
                            Message::Response(Response::Ok { .. }) => {
                                debug!("leader_forward: delivered {} to leader {}", metadata.path, leader_addr);
                                server.leader_forward_queue.lock().await.pop_front();
                                backoff_ms = 2_000; // reset for next entry
                            }
                            Message::Response(Response::NotLeader { leader_addr: redirect }) => {
                                debug!("leader_forward: NotLeader, redirecting to {:?}", redirect);
                                break;
                            }
                            other => {
                                warn!("leader_forward: unexpected response for {}: {:?}", metadata.path, other);
                                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                                backoff_ms = (backoff_ms * 2).min(30_000);
                            }
                        },
                        Ok(Err(e)) => {
                            debug!("leader_forward: send failed for {} to {}: {}", metadata.path, leader_addr, e);
                            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                            backoff_ms = (backoff_ms * 2).min(30_000);
                        }
                        Err(_) => {
                            debug!("leader_forward: timeout forwarding {} to {}", metadata.path, leader_addr);
                            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                            backoff_ms = (backoff_ms * 2).min(30_000);
                        }
                    }
                }
            }
        });
    }

    /// Start the chunk-location sync loop (follower side).
    ///
    /// Pushes all local chunk location records to the current leader whenever:
    ///   - the server starts up
    ///   - the leader address changes or appears (new leader / None→Some)
    ///   - any peer node recovers or joins (the notify fires regardless of whether
    ///     that peer is the leader — a recovering follower should also push in case
    ///     it was the node whose locations are missing from the stable leader)
    ///
    /// This fills the gap caused by the write path: each replica node stores only
    /// a single-node location record locally; the client broadcasts the full record
    /// to the leader, but if that broadcast is lost the leader ends up with no record
    /// at all even though data exists on disk.  Proactive push on every leader/node
    /// transition ensures the leader can always reconstruct the full replica picture.
    pub fn start_chunk_location_sync_loop(self: Arc<Self>) {
        let server = self;
        tokio::spawn(async move {
            let mut prev_leader: Option<std::net::SocketAddr> = None;
            let notify = server.cluster.node_recovered_notify.clone();

            // First iteration triggers immediately (startup).
            let mut node_event = true;

            loop {
                if !node_event {
                    // Wake on node recovery/join OR poll every 30s as a backstop.
                    tokio::select! {
                        _ = notify.notified() => { node_event = true; }
                        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                    }
                }

                let current_leader = server.cluster.get_leader_addr().await;
                let leader_changed = current_leader != prev_leader;
                prev_leader = current_leader;

                // Push if leader changed/appeared, OR if a node event fired (a peer
                // recovered or joined — that node may now need us to push our locations
                // to the stable leader so the leader can see the full replica set).
                let should_push = leader_changed || node_event;
                node_event = false;

                if !should_push {
                    continue;
                }

                let leader_addr = match current_leader {
                    Some(addr) => addr,
                    None => continue,
                };

                // If we ARE the leader, no need to push to ourselves.
                if leader_addr == server.cluster.local_addr() {
                    continue;
                }

                info!("chunk_location_sync: pushing local locations to leader {}", leader_addr);
                if let Err(e) = Self::push_locations_to(&server, leader_addr).await {
                    warn!("chunk_location_sync: push failed: {}", e);
                }
                // Also push file metadata for files this node physically holds.
                // This fills leader gaps that arise from rolling restarts where the
                // leader rebuilt its chunk_map before all followers were available.
                // Only files where this node is a data holder are pushed — spectator
                // metadata (known via gossip but no local chunk file) is not authoritative
                // and is excluded. The leader's written_at guard prevents regressions.
                if let Err(e) = Self::push_held_file_metadata_to(&server, leader_addr).await {
                    warn!("chunk_location_sync: file metadata push failed: {}", e);
                }
            }
        });
    }

    /// Push file metadata for files where this node is a data holder to `target`.
    /// Uses ReplicateMetadataBatch — fills leader gaps without bumping write_seq
    /// or triggering an immediate re-broadcast. The leader's sled write_seq ordering
    /// and chunk_map written_at guard both prevent regressions from stale pushes.
    async fn push_held_file_metadata_to(server: &Arc<Self>, target: std::net::SocketAddr) -> anyhow::Result<()> {
        let my_node_id = server.cluster.local_node_id();
        let metadata = server.metadata.clone();

        let files: Vec<dfs_common::FileMetadata> = tokio::task::spawn_blocking(move || {
            let mut held = Vec::new();
            let _ = metadata.scan_files(|file| {
                // Only push files where we actually hold at least one chunk on disk.
                if file.chunk_locations.iter().any(|loc| loc.nodes.contains(&my_node_id)) {
                    held.push(file);
                }
                Ok(())
            });
            held
        }).await?;

        if files.is_empty() {
            return Ok(());
        }

        let total = files.len();
        let mut sent = 0usize;
        for batch in files.chunks(100) {
            let req = dfs_common::Request::ReplicateMetadataBatch { items: batch.to_vec() };
            server.client.send_message(target, dfs_common::Message::Request(req)).await?;
            sent += batch.len();
        }
        info!("chunk_location_sync: pushed file metadata for {}/{} held files to {}", sent, total, target);
        Ok(())
    }

    /// Push chunk locations to `target` in 500-record batches.
    /// Only pushes locations for chunks this node physically holds on disk.
    /// Pushing stale routing-table entries for chunks we no longer have would
    /// create ghost replicas on the leader via the expansion rule — if our
    /// sled record predates a ghost-prune and has a larger node set, the leader
    /// would re-introduce the ghost on every chunk_location_sync cycle.
    async fn push_locations_to(server: &Arc<Self>, target: std::net::SocketAddr) -> anyhow::Result<()> {
        let my_node_id = server.cluster.local_node_id();
        let storage = server.storage.clone();
        let metadata = server.metadata.clone();
        let locations = tokio::task::spawn_blocking(move || {
            let all = metadata.list_all_chunk_locations()?;
            let held: Vec<_> = all.into_iter()
                .filter(|loc| loc.nodes.contains(&my_node_id) && storage.has_chunk(&loc.chunk_id))
                .collect();
            Ok::<_, anyhow::Error>(held)
        }).await??;

        if locations.is_empty() {
            return Ok(());
        }

        let total = locations.len();
        let mut sent = 0usize;
        for batch in locations.chunks(500) {
            let req = dfs_common::Request::ReplicateChunkLocations {
                locations: batch.to_vec(),
            };
            server.client.send_message(target, dfs_common::Message::Request(req)).await?;
            sent += batch.len();
        }
        info!("chunk_location_sync: pushed {}/{} locally-held locations to {}", sent, total, target);
        Ok(())
    }

    /// Catch-up pass run when this node becomes leader.
    ///
    /// Two-phase process:
    ///
    /// Phase 1 — Pull merge: query every follower's file inventory (FileId, modified_at).
    /// For any file that exists on a follower but NOT on us (or has a newer modified_at),
    /// fetch the full metadata and store it locally.  This recovers writes that landed on
    /// a follower during the window between the previous leader writing and crashing.
    ///
    /// Phase 2 — Push enqueue: for every follower, enqueue all files they are missing
    /// (determined by comparing our inventory against theirs after the pull merge).
    /// The dissemination loop delivers these within 5 seconds.
    async fn run_metadata_catchup(&self) {
        let nodes = self.cluster.get_all_nodes().await;
        let local_id = self.cluster.local_node_id();

        // Build our own inventory once (blocking sled scan).
        let metadata = self.metadata.clone();
        let my_inventory_result = tokio::task::spawn_blocking(move || metadata.get_file_inventory()).await;
        let my_inventory: std::collections::HashMap<FileId, u64> = match my_inventory_result {
            Ok(Ok(v)) => v.into_iter().collect(),
            Ok(Err(e)) => { warn!("catchup: failed to build local inventory: {}", e); return; }
            Err(e) => { warn!("catchup: spawn_blocking panic building inventory: {}", e); return; }
        };

        // Build the set of files that are queued for deletion on this node.
        // The leader is always in the initial DeleteFile fanout, so any pending
        // delete will have an entry here. Catchup must not pull these files from
        // followers — doing so would resurrect files the drain worker is about to
        // remove. This is the primary cause of "all deleted files come back after
        // redeploy": catchup fires immediately on leader election while the drain
        // loop waits 30 s before its first pass.
        let pending_delete_ids: std::collections::HashSet<FileId> = self.metadata
            .get_all_pending_deletes()
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.file_id)
            .collect();
        if !pending_delete_ids.is_empty() {
            info!("catchup: skipping {} file(s) queued for deletion", pending_delete_ids.len());
        }

        info!("catchup: starting with {} local file records", my_inventory.len());

        // --- Phase 1: pull anything we're missing from each follower ---
        // Collect all file IDs we pull so we can update our inventory for Phase 2.
        let mut pulled_total = 0usize;

        for node in &nodes {
            if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                continue;
            }

            // Fetch the follower's inventory.
            let inv_result = tokio::time::timeout(
                tokio::time::Duration::from_secs(10),
                self.client.send_message(node.addr, Message::Request(Request::GetFileInventory)),
            ).await;

            let follower_inventory: Vec<(FileId, u64)> = match inv_result {
                Ok(Ok(env)) => match env.message {
                    Message::Response(Response::FileInventory { entries }) => entries,
                    other => {
                        warn!("catchup: unexpected inventory response from {}: {:?}", node.id, other);
                        continue;
                    }
                },
                Ok(Err(e)) => { warn!("catchup: inventory fetch from {} failed: {}", node.id, e); continue; }
                Err(_) => { warn!("catchup: inventory fetch from {} timed out", node.id); continue; }
            };

            // Find files the follower has that we don't, or that are newer on the follower.
            // Skip files in the pending delete queue — they were removed from our DB by
            // handle_delete_file and would otherwise be resurrected here.
            let missing: Vec<FileId> = follower_inventory.iter()
                .filter_map(|(id, follower_write_seq)| {
                    if pending_delete_ids.contains(id) {
                        return None;
                    }
                    match my_inventory.get(id) {
                        None => Some(*id),  // we don't have it at all
                        Some(our_write_seq) if follower_write_seq > our_write_seq => Some(*id),
                        _ => None,
                    }
                })
                .collect();

            if missing.is_empty() {
                debug!("catchup: {} has nothing we're missing", node.id);
                continue;
            }

            info!("catchup: {} has {} records we need — fetching", node.id, missing.len());

            // Fetch the full metadata for missing/stale records in batches of 200.
            for chunk in missing.chunks(200) {
                let batch_result = tokio::time::timeout(
                    tokio::time::Duration::from_secs(15),
                    self.client.send_message(node.addr, Message::Request(Request::GetFileMetadataBatch {
                        file_ids: chunk.to_vec(),
                    })),
                ).await;

                let batch: Vec<FileMetadata> = match batch_result {
                    Ok(Ok(env)) => match env.message {
                        Message::Response(Response::FileMetadataBatch { items }) => items,
                        other => {
                            warn!("catchup: unexpected batch response from {}: {:?}", node.id, other);
                            continue;
                        }
                    },
                    Ok(Err(e)) => { warn!("catchup: batch fetch from {} failed: {}", node.id, e); continue; }
                    Err(_) => { warn!("catchup: batch fetch from {} timed out", node.id); continue; }
                };

                // Store each record locally if it's still newer than what we have.
                let metadata = self.metadata.clone();
                let chunk_map = self.chunk_map.clone();
                let batch_clone = batch.clone();
                let store_result = tokio::task::spawn_blocking(move || {
                    let mut stored = 0usize;
                    for item in &batch_clone {
                        // Re-check: only store if still missing or newer.
                        let should_store = match metadata.get_file(&item.id)? {
                            None => true,
                            Some(existing) => item.write_seq > existing.write_seq,
                        };
                        if should_store {
                            metadata.put_file(item)?;
                            stored += 1;
                        }
                    }
                    Ok::<usize, anyhow::Error>(stored)
                }).await;

                match store_result {
                    Ok(Ok(n)) => {
                        pulled_total += n;
                        // Update chunk map for stored records.
                        for item in &batch {
                            self.chunk_map_update(item).await;
                        }
                    }
                    Ok(Err(e)) => warn!("catchup: store error pulling from {}: {}", node.id, e),
                    Err(e) => warn!("catchup: spawn_blocking panic storing pull from {}: {}", node.id, e),
                }
            }
        }

        if pulled_total > 0 {
            info!("catchup: pulled {} records from followers — leader DB is now authoritative", pulled_total);
        }

        // --- Phase 2: re-enqueue everything for followers that are behind ---
        // Rebuild our inventory after the pull merge.
        let metadata = self.metadata.clone();
        let updated_inventory_result = tokio::task::spawn_blocking(move || metadata.get_file_inventory()).await;
        let updated_inventory: std::collections::HashMap<FileId, u64> = match updated_inventory_result {
            Ok(Ok(v)) => v.into_iter().collect(),
            Ok(Err(e)) => { warn!("catchup: failed to rebuild inventory for push phase: {}", e); return; }
            Err(e) => { warn!("catchup: spawn_blocking panic rebuilding inventory: {}", e); return; }
        };

        for node in &nodes {
            if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                continue;
            }

            // Fetch the follower's inventory again (or reuse if we already have it).
            // For simplicity, re-fetch — inventories are compact (~24 bytes per file).
            let inv_result = tokio::time::timeout(
                tokio::time::Duration::from_secs(10),
                self.client.send_message(node.addr, Message::Request(Request::GetFileInventory)),
            ).await;

            let follower_has: std::collections::HashSet<FileId> = match inv_result {
                Ok(Ok(env)) => match env.message {
                    Message::Response(Response::FileInventory { entries }) => entries.into_iter().map(|(id, _)| id).collect(),
                    _ => std::collections::HashSet::new(),
                },
                _ => std::collections::HashSet::new(),
            };

            // Enqueue everything the follower is missing, excluding files queued for deletion.
            let node_id = node.id;
            let to_enqueue: Vec<FileId> = updated_inventory.keys()
                .filter(|id| !follower_has.contains(id) && !pending_delete_ids.contains(id))
                .copied()
                .collect();

            if to_enqueue.is_empty() {
                debug!("catchup: {} is up-to-date after pull merge", node.id);
                continue;
            }

            info!("catchup: enqueuing {} missing records for {}", to_enqueue.len(), node.id);

            let metadata = self.metadata.clone();
            let result = tokio::task::spawn_blocking(move || {
                metadata.scan_all_files(|meta| {
                    if to_enqueue.contains(&meta.id) {
                        let seq = metadata.next_meta_sequence()?;
                        metadata.enqueue_meta_for_node(node_id, seq, &meta)?;
                    }
                    Ok(())
                })
            }).await;

            match result {
                Ok(Ok(scanned)) => debug!("catchup: scanned {} records, enqueued for {}", scanned, node_id),
                Ok(Err(e)) => warn!("catchup: enqueue error for {}: {}", node_id, e),
                Err(e) => warn!("catchup: spawn_blocking panic for {}: {}", node_id, e),
            }
        }

        info!("catchup: complete");
    }

    /// Handle an incoming request message
    pub async fn handle_request(&self, request: Request) -> Response {
        match request {
            Request::ReadChunk { chunk_id, sequential_hint, client_write_seq } => {
                self.ops_tracker.inc_read();
                if let Some((idx, total)) = sequential_hint {
                    debug!("ReadChunk {} with sequential hint: {}/{} chunks", chunk_id, idx, total);
                    // TODO: Use hint for server-side prefetching
                }
                self.handle_read_chunk(chunk_id, client_write_seq).await
            },
            Request::ReadChunkRange { chunk_id, offset, length, client_write_seq } => {
                self.ops_tracker.inc_read();
                self.handle_read_chunk_range(chunk_id, offset, length, client_write_seq).await
            }
            Request::WriteChunk {
                chunk_id,
                data,
                checksum,
            } => {
                self.ops_tracker.inc_write();
                self.handle_write_chunk(chunk_id, data, checksum).await
            }
            Request::TombstoneChunk { chunk_id } => {
                self.chunk_tombstones.insert(chunk_id);
                Response::Ok { data: None }
            }
            Request::DeleteChunk { chunk_id } => self.handle_delete_chunk(chunk_id).await,
            Request::DeleteChunksBatch { file_id, path, chunk_ids } => {
                self.handle_delete_chunks_batch(file_id, path, chunk_ids).await
            }
            Request::ClearDeleteQueueEntry { file_id } => {
                self.handle_clear_delete_queue_entry(file_id).await
            }
            Request::GetDeleteQueue => self.handle_get_delete_queue().await,
            Request::HasChunk { chunk_id } => self.handle_has_chunk(chunk_id).await,
            Request::HasChunks { chunk_ids } => self.handle_has_chunks(chunk_ids).await,
            Request::ReplicateChunk {
                chunk_id,
                data,
                checksum,
                written_at,
            } => self.handle_replicate_chunk(chunk_id, data, checksum, written_at).await,
            Request::PushChunkTo { chunk_id, target_addr, leader_id } => {
                self.handle_push_chunk_to(chunk_id, target_addr, leader_id).await
            }
            Request::DeleteChunkReplica { chunk_id, leader_id } => {
                self.handle_delete_chunk_replica(chunk_id, leader_id).await
            }
            Request::ReplicateMetadata { metadata, ttl } => {
                self.handle_replicate_metadata(metadata, ttl).await
            }
            Request::ReplicateMetadataBatch { items } => {
                self.handle_replicate_metadata_batch(items).await
            }
            Request::DeleteMetadata { file_id, path, chunk_ids, ttl } => {
                self.handle_delete_metadata(file_id, path, chunk_ids, ttl).await
            }
            Request::DeletePathIndex { path } => {
                self.handle_delete_path_index(path).await
            }
            Request::ReplicateChunkLocation { location, file_id } => {
                self.handle_replicate_chunk_location(location, file_id).await
            }
            Request::ReplicateChunkLocations { locations } => {
                self.handle_replicate_chunk_locations(locations).await
            }
            Request::PurgeChunkLocation { chunk_id } => {
                self.handle_purge_chunk_location(chunk_id).await
            }
            Request::PurgeChunkLocations { chunk_ids } => {
                self.handle_purge_chunk_locations(chunk_ids).await
            }
            Request::ReconcileMetadata { live_file_ids } => {
                self.handle_reconcile_metadata(live_file_ids).await
            }
            Request::GetMetadataSequence => {
                self.handle_get_metadata_sequence().await
            }
            Request::DisseminateMetadata { items, up_to_sequence } => {
                self.handle_disseminate_metadata(items, up_to_sequence).await
            }
            Request::GetFileInventory => {
                self.handle_get_file_inventory().await
            }
            Request::GetFileMetadataBatch { file_ids } => {
                self.handle_get_file_metadata_batch(file_ids).await
            }
            Request::PrefetchHint { chunk_ids } => {
                self.handle_prefetch_hint(chunk_ids).await
            }
            Request::GetFileMetadataByPath { path, if_modified_since } => {
                self.ops_tracker.inc_meta();
                self.handle_get_file_metadata_by_path(path, if_modified_since).await
            }
            Request::PutFileMetadata { metadata } => {
                self.ops_tracker.inc_meta();
                self.handle_put_file_metadata(metadata).await
            }
            Request::ListDirectory { path } => {
                self.ops_tracker.inc_meta();
                self.handle_list_directory(path).await
            }
            Request::WriteFile { data, file_id } => {
                self.ops_tracker.inc_write();
                self.handle_write_file(data, file_id).await
            }
            Request::WriteFileLocalOnly { data, file_offset, file_id } => {
                self.ops_tracker.inc_write();
                self.handle_write_file_local_only(data, file_offset, file_id).await
            }
            Request::PatchChunk { chunk_id, file_id, chunk_idx, chunk_file_offset, intra_offset, data } => {
                self.ops_tracker.inc_write();
                self.handle_patch_chunk(chunk_id, file_id, chunk_idx, chunk_file_offset, intra_offset, data).await
            }
            Request::MultiPatch { chunk_id, file_id, chunk_idx, chunk_file_offset, patches, expected_new_chunk_id, client_write_seq } => {
                self.ops_tracker.inc_write();
                self.handle_multi_patch(chunk_id, file_id, chunk_idx, chunk_file_offset, patches, expected_new_chunk_id, client_write_seq).await
            }
            Request::DeleteFile { path } => {
                self.ops_tracker.inc_meta();
                self.handle_delete_file(path).await
            }
            Request::RenameFile { old_path, new_path } => {
                self.ops_tracker.inc_meta();
                self.handle_rename_file(old_path, new_path).await
            }

            // Admin requests
            Request::GetClusterStatus => self.handle_get_cluster_status().await,
            Request::GetStorageStats => self.handle_get_storage_stats().await,
            Request::GetHealingStatus => self.handle_get_healing_status().await,
            Request::TriggerScrub => self.handle_trigger_scrub().await,
            Request::EnableHealing => self.handle_enable_healing().await,
            Request::DisableHealing => self.handle_disable_healing().await,
            Request::TriggerHealing => self.handle_trigger_healing().await,
            Request::TriggerMetadataRepair => self.handle_trigger_metadata_repair().await,
            Request::QueryChunkSizes { chunk_ids } => self.handle_query_chunk_sizes(chunk_ids).await,
            Request::HealFile { path } => self.handle_heal_file(path).await,
            Request::VerifyChunkIntegrity { chunk_id, file_offset, file_id } => {
                let found = self.storage.has_chunk(&chunk_id);
                let valid = found && self.storage.verify_chunk_at(&chunk_id, file_offset, file_id);
                Response::ChunkValid { found, valid }
            }
            Request::RepairFile { path, force } => self.handle_repair_file(path, force).await,
            Request::GetFileInfo { path } => self.handle_get_file_info(path).await,
            Request::GetFileInfoById { file_id } => self.handle_get_file_info_by_id(file_id).await,
            Request::RemoveNode { node_id } => self.handle_remove_node(node_id).await,
            Request::ListAllFiles => self.handle_list_all_files().await,
            Request::PurgeFileMetadata { path } => self.handle_purge_file_metadata(path).await,
            Request::PurgeFileMetadataById { file_id, propagate } => {
                self.handle_purge_file_metadata_by_id(file_id, propagate).await
            }
            Request::GetFileChunkMap { file_id, from_chunk, count } => {
                self.handle_get_file_chunk_map(file_id, from_chunk, count).await
            }

            Request::AppendFile { file_id, data, expected_offset } => {
                self.ops_tracker.inc_write();
                self.handle_append_file(file_id, data, expected_offset).await
            }

            Request::GetNodeStats => {
                use crate::network::MAX_CONNECTIONS;
                let snap = self.ops_tracker.get_stats();
                let (active_conn, max_conn) = {
                    let sem = self.conn_semaphore.read().await;
                    match sem.as_ref() {
                        Some(s) => ((MAX_CONNECTIONS - s.available_permits()) as u64, MAX_CONNECTIONS as u64),
                        None => (0, MAX_CONNECTIONS as u64),
                    }
                };
                Response::NodeStats {
                    reads_live: snap.reads_live,
                    writes_live: snap.writes_live,
                    meta_live: snap.meta_live,
                    reads_peak_1h: snap.reads_peak_1h,
                    writes_peak_1h: snap.writes_peak_1h,
                    meta_peak_1h: snap.meta_peak_1h,
                    total_peak_1h: snap.total_peak_1h,
                    reads_avg_1h: snap.reads_avg_1h,
                    writes_avg_1h: snap.writes_avg_1h,
                    meta_avg_1h: snap.meta_avg_1h,
                    uptime_secs: snap.uptime_secs,
                    active_connections: active_conn,
                    max_connections: max_conn,
                }
            }

            _ => Response::Error {
                message: "Request type not yet implemented".to_string(),
                code: ErrorCode::InternalError,
            },
        }
    }

    /// Handle read chunk request (try local first, then forward to other nodes)
    async fn handle_read_chunk(&self, chunk_id: ChunkId, client_write_seq: Option<u64>) -> Response {
        debug!("Handling read chunk: {}", chunk_id);

        // Client-driven staleness detection: if client has newer metadata than us,
        // self-heal by pulling fresh metadata from leader before serving the read.
        if let Some(client_seq) = client_write_seq {
            if let Some(file_id) = self.find_file_by_chunk(&chunk_id) {
                if let Ok(Some(our_meta)) = self.metadata.get_file(&file_id) {
                    if client_seq > our_meta.write_seq {
                        info!("Stale metadata detected: client has seq={}, we have seq={} for file_id={}, pulling from leader",
                              client_seq, our_meta.write_seq, file_id);
                        if let Err(e) = self.pull_metadata_from_leader(file_id).await {
                            warn!("Failed to pull fresh metadata from leader: {}", e);
                            // Continue anyway - serve what we have
                        }
                    }
                }
            }
        }

        // Serve from local storage only — never proxy to other nodes.
        // If the client sends a ReadChunk to a node that doesn't hold the chunk,
        // the client's fallback logic will retry a different replica. Proxying
        // causes cascading timeouts: a node under load holds up all its request
        // handlers waiting for remote fetches, starving heartbeats.
        match self.storage.read_chunk_arc(&chunk_id) {
            Ok(arc) => {
                let (capacity, size) = self.storage.get_cache_stats();
                let cache_stats = Some((0, capacity, size));
                Response::ChunkData { chunk_id, data: vec![], cache_stats, arc_data: Some(arc), arc_range: None }
            }
            Err(_) => {
                Response::Error {
                    message: format!("Chunk {} not found on this node", chunk_id),
                    code: ErrorCode::NotFound,
                }
            }
        }
    }

    /// Handle read chunk range request (for striped multi-replica reads)
    async fn handle_read_chunk_range(&self, chunk_id: ChunkId, offset: u64, length: u64, client_write_seq: Option<u64>) -> Response {
        debug!("Handling read chunk range: {} offset={} length={}", chunk_id, offset, length);

        // Client-driven staleness detection (same as handle_read_chunk)
        if let Some(client_seq) = client_write_seq {
            if let Some(file_id) = self.find_file_by_chunk(&chunk_id) {
                if let Ok(Some(our_meta)) = self.metadata.get_file(&file_id) {
                    if client_seq > our_meta.write_seq {
                        info!("Stale metadata detected in range read: client seq={}, our seq={} for file_id={}, pulling from leader",
                              client_seq, our_meta.write_seq, file_id);
                        if let Err(e) = self.pull_metadata_from_leader(file_id).await {
                            warn!("Failed to pull fresh metadata from leader: {}", e);
                        }
                    }
                }
            }
        }

        // Use a seeked partial read — avoids loading the full 4MB chunk from disk
        // on a cache miss when the caller only needs a small byte range.
        // On a cache hit the slice is still copied from the warm Arc (negligible cost).
        match self.storage.read_chunk_range_partial(&chunk_id, offset as usize, length as usize) {
            Ok(data) => {
                debug!("Returning {} bytes from chunk {} (requested {}, offset {})",
                       data.len(), chunk_id, length, offset);

                let (capacity, size) = self.storage.get_cache_stats();
                let cache_stats = Some((0, capacity, size));

                Response::ChunkData {
                    chunk_id,
                    data,
                    cache_stats,
                    arc_data: None,
                    arc_range: None,
                }
            }
            Err(e) => {
                warn!("Failed to read chunk range {} offset={} length={}: {}",
                      chunk_id, offset, length, e);
                Response::Error {
                    message: format!("Failed to read chunk range: {}", e),
                    code: ErrorCode::NotFound,
                }
            }
        }
    }

    /// Handle write chunk request (local write + replication)
    async fn handle_write_chunk(
        &self,
        chunk_id: ChunkId,
        data: Vec<u8>,
        checksum: [u8; 32],
    ) -> Response {
        debug!("Handling write chunk: {} ({} bytes)", chunk_id, data.len());

        // Verify checksum matches chunk_id
        if checksum != chunk_id.hash {
            return Response::Error {
                message: "Checksum mismatch".to_string(),
                code: ErrorCode::ChecksumMismatch,
            };
        }

        // Write locally
        if let Err(e) = self.storage.write_chunk(&chunk_id, &data) {
            warn!("Failed to write chunk {}: {}", chunk_id, e);
            return Response::Error {
                message: format!("Failed to write chunk: {}", e),
                code: ErrorCode::IOError,
            };
        }

        // Update chunk location metadata: add local node if not already present.
        // Don't push if the location already has >= RF nodes — this node is receiving a
        // healing PushChunkTo and the healer will broadcast the authoritative updated
        // location after it gets our Ok. Adding ourselves here before that broadcast
        // would leave a stale 4th-node record that the merge logic then perpetuates.
        let local_node_id = self.cluster.local_node_id();
        if let Ok(mut location) = self.get_or_create_chunk_location(&chunk_id, data.len()).await {
            if !location.nodes.contains(&local_node_id) && location.nodes.len() < self.replication_factor {
                location.nodes.push(local_node_id);
                let _ = self.metadata.put_chunk_location(&location);
            }
        }

        Response::Ok { data: None }
    }

    /// Handle replicate chunk request (replication from another node)
    async fn handle_replicate_chunk(
        &self,
        chunk_id: ChunkId,
        data: Vec<u8>,
        checksum: [u8; 32],
        written_at: Option<u64>,
    ) -> Response {
        debug!("Handling replicate chunk: {} ({} bytes)", chunk_id, data.len());

        let response = self.handle_write_chunk(chunk_id, data, checksum).await;

        // Preserve the original write timestamp so scrub can detect corruption
        // by comparing chunk mtime against ChunkLocation.written_at.
        if matches!(response, Response::Ok { .. }) {
            if let Some(ts) = written_at {
                self.storage.set_chunk_mtime(&chunk_id, ts);
            }
        }

        response
    }

    /// Validate that the given node_id is actually the current leader per our gossip view.
    /// Returns an error response if validation fails.
    async fn validate_leader(&self, claimed_leader_id: dfs_common::NodeId) -> Option<Response> {
        if !self.cluster.is_leader_id(claimed_leader_id).await {
            let msg = format!(
                "Rejected: sender {} is not the current leader per this node's cluster view",
                claimed_leader_id
            );
            warn!("{}", msg);
            Some(Response::Error {
                message: msg,
                code: ErrorCode::InvalidRequest,
            })
        } else {
            None
        }
    }

    /// Handle push-chunk-to request: read chunk locally and send it to target_addr.
    /// Called by the leader during healing — the leader never touches the data itself.
    async fn handle_push_chunk_to(&self, chunk_id: ChunkId, target_addr: std::net::SocketAddr, leader_id: dfs_common::NodeId) -> Response {
        if let Some(err) = self.validate_leader(leader_id).await {
            return err;
        }
        info!("PushChunkTo: pushing chunk {} to {}", chunk_id, target_addr);

        let data = match self.storage.read_chunk(&chunk_id) {
            Ok(d) => d,
            Err(e) => {
                warn!("PushChunkTo: chunk {} not found locally: {}", chunk_id, e);
                return Response::Error {
                    message: format!("Chunk not found locally: {}", e),
                    code: ErrorCode::NotFound,
                };
            }
        };

        // Fetch location metadata for written_at and file_offset.
        let loc = self.metadata.get_chunk_location(&chunk_id).ok().flatten();
        let written_at = loc.as_ref().and_then(|l| l.written_at);

        // Verify chunk content before propagating — catches disk corruption at the
        // source so we don't spread bad data to the rest of the cluster.
        // The hash is file-scoped and position-aware: blake3(file_id || file_offset || data).
        // We can only verify if we have both the file_offset and file_id from the chunk location.
        if let (Some(offset), Some(file_id)) = (
            loc.as_ref().and_then(|l| l.file_offset),
            loc.as_ref().and_then(|l| l.file_id),
        ) {
            let actual_hash = dfs_common::compute_chunk_hash_at(&data, offset, file_id);
            if actual_hash != chunk_id.hash {
                warn!("PushChunkTo: chunk {} at offset {} failed content hash verification — disk corruption detected, refusing to propagate",
                    chunk_id, offset);
                return Response::Error {
                    message: format!("Chunk {} content hash mismatch (disk corruption)", chunk_id),
                    code: ErrorCode::ChecksumMismatch,
                };
            }
        }

        let request = Request::ReplicateChunk {
            chunk_id,
            data,
            checksum: chunk_id.hash,
            written_at,
        };

        match self.client.send_message(target_addr, Message::Request(request)).await {
            Ok(envelope) => {
                if matches!(envelope.message, Message::Response(Response::Ok { .. })) {
                    info!("PushChunkTo: chunk {} successfully pushed to {}", chunk_id, target_addr);
                    Response::Ok { data: None }
                } else {
                    warn!("PushChunkTo: target {} rejected chunk {}", target_addr, chunk_id);
                    Response::Error {
                        message: format!("Target rejected chunk: {:?}", envelope.message),
                        code: ErrorCode::IOError,
                    }
                }
            }
            Err(e) => {
                warn!("PushChunkTo: failed to push chunk {} to {}: {}", chunk_id, target_addr, e);
                Response::Error {
                    message: format!("Failed to push chunk to target: {}", e),
                    code: ErrorCode::IOError,
                }
            }
        }
    }

    /// Handle delete-chunk-replica request: leader-initiated excess replica cleanup.
    async fn handle_delete_chunk_replica(&self, chunk_id: ChunkId, leader_id: dfs_common::NodeId) -> Response {
        if let Some(err) = self.validate_leader(leader_id).await {
            return err;
        }
        info!("DeleteChunkReplica: deleting excess replica of {} on this node", chunk_id);
        match self.storage.delete_chunk(&chunk_id) {
            Ok(_) => Response::Ok { data: None },
            Err(e) => {
                warn!("DeleteChunkReplica: failed to delete {}: {}", chunk_id, e);
                Response::Error {
                    message: format!("Failed to delete chunk: {}", e),
                    code: ErrorCode::IOError,
                }
            }
        }
    }

    /// Handle replicate metadata request (metadata replication from another node)
    async fn handle_replicate_metadata(&self, metadata: FileMetadata, ttl: u8) -> Response {
        debug!("Handling replicate metadata: {} (ttl={})", metadata.path, ttl);

        // Tombstone check: if this file was recently deleted on this node, reject the
        // replicate so we don't resurrect a deleted file from an in-flight broadcast.
        const TOMBSTONE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
        if let Some(entry) = self.delete_tombstones.get(&metadata.id) {
            if entry.value().elapsed() < TOMBSTONE_TTL {
                debug!(
                    "[META SERVER] tombstone-reject replicate path={} id={} seq={}",
                    metadata.path, metadata.id, metadata.write_seq
                );
                return Response::Ok { data: None };
            } else {
                drop(entry);
                self.delete_tombstones.remove(&metadata.id);
            }
        }

        // Update in-memory state immediately, then hand the sled write to the
        // dedicated worker so we never block an async thread on sled I/O.
        // Under a 5000-file touch storm the leader broadcasts ~20k RPCs; without
        // this, every follower's Tokio runtime stalls on concurrent sled writes.
        //
        // Protect freshly-patched locations from being overwritten by stale broadcasts.
        // A concurrent release/fsync may commit pre-patch metadata to the leader before
        // our MultiPatch completes. The leader then broadcasts the stale state here. If
        // our chunk_map already holds a newer written_at for a chunk (stamped by
        // handle_multi_patch), keep it rather than reverting to the old chunk_id.
        // write_seq bypass: if incoming is from a newer session, accept as-is.
        // Otherwise, prefer the chunk_map's chunk_id for any offset already present —
        // chunk_map is updated only by RCL (authoritative, sent immediately after each
        // successful patch) and should never be regressed by a metadata broadcast.
        let stored_write_seq_rm = self.file_write_seqs.get(&metadata.id).map(|v| *v).unwrap_or(0);
        let bypass_rm = metadata.write_seq > stored_write_seq_rm;
        if bypass_rm {
            self.file_write_seqs.insert(metadata.id, metadata.write_seq);
        }
        // Same guard as handle_replicate_metadata_batch — see comment there.
        let metadata = {
            let mut m = metadata;
            if let Some(map_entry) = self.chunk_map.get(&m.id) {
                let (map_locs, _) = map_entry.value();
                for loc in m.chunk_locations.iter_mut() {
                    if let Some(file_offset) = loc.file_offset {
                        if let Some(map_loc) = map_locs.iter().find(|l| l.file_offset == Some(file_offset)) {
                            if map_loc.chunk_id != loc.chunk_id {
                                if let Some(incoming_ts) = loc.written_at {
                                    // (Some, *): incoming is a patch result — compare timestamps.
                                    let existing_ts = map_loc.written_at.unwrap_or(0);
                                    if existing_ts > incoming_ts {
                                        *loc = map_loc.clone();
                                    }
                                } else if map_loc.written_at.is_some() && !bypass_rm {
                                    // (None, Some): incoming is a fresh-write snapshot, existing
                                    // is a patch result. Same/older session: keep the patch.
                                    // bypass=true means newer write_seq (new session) → accept.
                                    *loc = map_loc.clone();
                                }
                                // (None, None) or (None, Some)+bypass: accept incoming.
                            }
                        }
                    }
                }
            }
            m
        };
        self.chunk_map_update(&metadata).await;
        if let Some(tx) = self.sled_write_tx.lock().unwrap().as_ref() {
            let _ = tx.send(metadata.clone());
        }

        // TTL>0: forward to all other nodes with ttl-1 so every node gets
        // every write regardless of which node the client contacted first.
        if ttl > 0 {
            let cluster = self.cluster.clone();
            let client = self.client.clone();
            let local_id = self.cluster.local_node_id();
            let metadata_clone = metadata.clone();
            let sem = self.broadcast_semaphore.clone();
            tokio::spawn(async move {
                let _permit = sem.acquire().await.ok();
                let nodes = cluster.get_all_nodes().await;
                for node in &nodes {
                    if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                        continue;
                    }
                    let req = Request::ReplicateMetadata { metadata: metadata_clone.clone(), ttl: ttl - 1 };
                    if let Err(e) = client.send_message(node.addr, Message::Request(req)).await {
                        warn!("TTL forward ReplicateMetadata to {} failed: {}", node.id, e);
                    }
                }
            });
        }
        // Only enqueue toward leader if ttl > 0, meaning this write came
        // from a client directly to this follower (not from a leader broadcast).
        // When ttl == 0 the leader is the sender — forwarding back creates a storm.
        if ttl > 0 && !self.cluster.is_leader().await {
            let leader_addr = self.cluster.get_leader_addr().await;
            let local_addr = self.cluster.local_addr();
            // Only enqueue if the leader is a different node (not us).
            if leader_addr.map(|a| a != local_addr).unwrap_or(true) {
                let mut queue = self.leader_forward_queue.lock().await;
                // Deduplicate: replace any existing entry for the same file_id
                // so only the latest snapshot is in-flight.
                queue.retain(|(m, _)| m.id != metadata.id);
                queue.push_back((metadata.clone(), std::time::Instant::now()));
                self.leader_forward_notify.notify_one();
            }
        }
        debug!("Successfully replicated metadata for {}", metadata.path);
        Response::Ok { data: None }
    }

    /// Handle delete metadata replication (internal cluster operation)
    async fn handle_delete_metadata(&self, file_id: FileId, path: String, chunk_ids: Vec<ChunkId>, ttl: u8) -> Response {
        info!("[META SERVER] delete path={} id={} chunks={} ttl={}", path, file_id, chunk_ids.len(), ttl);

        // Record tombstone before deleting so that any concurrent ReplicateMetadata
        // arriving on this node is also blocked from resurrecting the file.
        self.delete_tombstones.insert(file_id, std::time::Instant::now());
        // Purge from the coalescing broadcast buffer. Without this, a create that
        // landed in pending_broadcasts moments before this delete would be flushed
        // to followers 100ms later, resurrecting the file there.
        self.pending_broadcasts.remove(&file_id);

        if let Err(e) = self.metadata.delete_file(&file_id) {
            warn!("Failed to delete file record {} on peer: {}", file_id, e);
        }
        if let Err(e) = self.metadata.delete_path_index(&path) {
            warn!("Failed to delete path index {} on peer: {}", path, e);
        }
        for chunk_id in &chunk_ids {
            if let Err(e) = self.metadata.delete_chunk_location(chunk_id) {
                warn!("Failed to delete chunk location {} on peer: {}", chunk_id, e);
            }
        }
        self.chunk_map_remove(&file_id).await;

        // TTL>0: forward to all other nodes so the delete reaches every node
        // regardless of which node originally processed the DeleteFile.
        if ttl > 0 {
            let cluster = self.cluster.clone();
            let client = self.client.clone();
            let local_id = self.cluster.local_node_id();
            let sem = self.broadcast_semaphore.clone();
            let path_clone = path.clone();
            let chunk_ids_clone = chunk_ids.clone();
            tokio::spawn(async move {
                let _permit = sem.acquire().await.ok();
                let nodes = cluster.get_all_nodes().await;
                for node in &nodes {
                    if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                        continue;
                    }
                    let req = Request::DeleteMetadata {
                        file_id,
                        path: path_clone.clone(),
                        chunk_ids: chunk_ids_clone.clone(),
                        ttl: ttl - 1,
                    };
                    if let Err(e) = client.send_message(node.addr, Message::Request(req)).await {
                        warn!("TTL forward DeleteMetadata to {} failed: {}", node.id, e);
                    }
                }
            });
        }

        debug!("Successfully deleted metadata and {} chunk locations for {} on peer", chunk_ids.len(), path);
        Response::Ok { data: None }
    }

    /// Handle path-index-only deletion (used by rename to clean up stale old-path entries on peers).
    async fn handle_delete_path_index(&self, path: String) -> Response {
        if let Err(e) = self.metadata.delete_path_index(&path) {
            warn!("Failed to delete path index {} on peer: {}", path, e);
        }
        debug!("Deleted path index entry for {} on peer", path);
        Response::Ok { data: None }
    }

    /// Handle replicate chunk location (internal cluster operation)
    async fn handle_replicate_chunk_location(&self, location: ChunkLocation, file_id: Option<FileId>) -> Response {
        info!("Handling replicate chunk location: {} (nodes: {:?})", location.chunk_id, location.nodes);

        // Assign server-side timestamp to fresh-write locations (written_at=None).
        // Same reason as in handle_put_file_metadata: T_now on the leader is always
        // greater than any previous patch timestamp, so fresh writes win the guard.
        let location = if location.written_at.is_none() {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            ChunkLocation { written_at: Some(now_ms), ..location }
        } else {
            location
        };

        // Merge strategy: take the larger node set, preserving the existing record's
        // file_offset and written_at if the incoming doesn't carry them.
        //
        // Chunks are content-addressed — same chunk_id always means same bytes, so
        // any node listed in either record genuinely holds (or held) the data.  The
        // healer's DeleteChunkReplica path explicitly removes nodes *and then*
        // broadcasts the trimmed set; that trim broadcast will always have fewer nodes
        // than the current record, so we need to accept it.
        //
        // Rule: if incoming.nodes.len() > existing.nodes.len(), take incoming (expansion).
        //       if incoming.nodes.len() <= existing.nodes.len() AND incoming.nodes.len() < RF,
        //         this is a stale early-write broadcast arriving after healing — ignore it.
        //       if incoming.nodes.len() <= existing.nodes.len() AND incoming.nodes.len() >= RF,
        //         this is a legitimate healer trim or same-size update — accept it.
        //
        // This prevents the cycle: write broadcasts {A,B} → healer heals to {A,B,C} →
        // stale write broadcast arrives, replaces with {A,B} → healer heals to {A,B,D}
        // → accumulate nodes D,E,... = over-replication.
        let rf = self.replication_factor;
        let merged_location = match self.metadata.get_chunk_location(&location.chunk_id) {
            Ok(Some(existing)) => {
                let incoming_count = location.nodes.len();
                let existing_count = existing.nodes.len();
                let nodes = if incoming_count < rf && existing_count < rf {
                    // Both sides are under-RF — union the node sets.  This handles the
                    // startup-sync case where followers each push their single-node record
                    // and we need to accumulate them rather than overwrite.
                    let mut merged: Vec<_> = existing.nodes.clone();
                    for n in &location.nodes {
                        if !merged.contains(n) {
                            merged.push(*n);
                        }
                    }
                    if merged.len() > existing_count {
                        debug!("Merging chunk location for {} ({} + {} → {} nodes)",
                               location.chunk_id, existing_count, incoming_count, merged.len());
                    }
                    merged
                } else if incoming_count > existing_count {
                    // Expansion — only accept if the incoming record is at least as fresh
                    // as the existing one.  A re-joining node's stale sled data can have
                    // a larger node count than the healed record (it predates the ghost
                    // prune), and accepting it blindly creates new ghost replicas.
                    let ts_ok = location.written_at.unwrap_or(0) >= existing.written_at.unwrap_or(0);
                    if ts_ok {
                        debug!("Expanding chunk location for {} ({} → {} nodes)",
                               location.chunk_id, existing_count, incoming_count);
                        location.nodes.clone()
                    } else {
                        debug!("Stale expansion for {} ({} → {} nodes, ts {} < {}) — keeping existing",
                               location.chunk_id, existing_count, incoming_count,
                               location.written_at.unwrap_or(0), existing.written_at.unwrap_or(0));
                        existing.nodes.clone()
                    }
                } else if incoming_count < rf && existing_count >= rf {
                    // Stale early-write broadcast arriving after healing — ignore.
                    debug!("Ignoring stale chunk location broadcast for {} ({} nodes incoming, existing has {}, RF={})",
                           location.chunk_id, incoming_count, existing_count, rf);
                    return Response::Ok { data: None };
                } else {
                    // Healer trim or same-size update — accept only if the incoming
                    // record is at least as fresh as the existing one.  A stale follower
                    // sync pushing a same-count but different-nodes record (e.g. the old
                    // set before a ghost was pruned) would otherwise revert the healer's
                    // work every 30 seconds.
                    let ts_ok = location.written_at.unwrap_or(0) >= existing.written_at.unwrap_or(0);
                    if ts_ok {
                        debug!("Updating chunk location for {} ({} → {} nodes)",
                               location.chunk_id, existing_count, incoming_count);
                        location.nodes.clone()
                    } else {
                        debug!("Stale same-count update for {} ({} nodes, ts {} < {}) — keeping existing",
                               location.chunk_id, existing_count,
                               location.written_at.unwrap_or(0), existing.written_at.unwrap_or(0));
                        existing.nodes.clone()
                    }
                };
                ChunkLocation {
                    chunk_id: location.chunk_id,
                    nodes,
                    size: location.size,
                    checksum: location.checksum,
                    file_offset: location.file_offset.or(existing.file_offset),
                    written_at: existing.written_at.or(location.written_at),
                    client_write_seq: location.client_write_seq.or(existing.client_write_seq),
                    file_id: location.file_id.or(existing.file_id),
                }
            }
            Ok(None) => {
                debug!("Creating new chunk location for {}", location.chunk_id);
                location.clone()
            }
            Err(e) => {
                warn!("Failed to get existing chunk location: {}, using new location", e);
                location.clone()
            }
        };

        // Store merged location
        match self.metadata.put_chunk_location(&merged_location) {
            Ok(_) => {
                // Patch the in-memory chunk map. When file_id is known, do a targeted
                // update — no need to scan all files. Without file_id (legacy path),
                // fall back to the scan-based update.
                if let Some(fid) = file_id {
                    self.chunk_map_update_location_for_file(fid, &merged_location).await;
                } else {
                    self.chunk_map_update_location(&merged_location).await;
                }

                // Also update the file metadata record in sled so DisseminateMetadata
                // broadcasts the correct chunk_id to followers. Without this, the file
                // record retains the old chunk_id and periodic dissemination overwrites
                // the in-memory fix, causing perpetual ChunkStale on followers.
                let sled_file_id = file_id.or_else(|| {
                    // Legacy fallback: find the file by chunk_id in the chunk_map.
                    merged_location.file_offset.and_then(|file_offset| {
                        for entry in self.chunk_map.iter() {
                            let fid = *entry.key();
                            let (locs, _) = entry.value();
                            if locs.iter().any(|l| l.file_offset == Some(file_offset) && l.chunk_id == merged_location.chunk_id) {
                                return Some(fid);
                            }
                        }
                        None
                    })
                });
                if let Some(fid) = sled_file_id {
                    if let Ok(Some(mut file_meta)) = self.metadata.get_file(&fid) {
                        let mut updated = false;
                        for loc in file_meta.chunk_locations.iter_mut() {
                            if loc.chunk_id == merged_location.chunk_id {
                                // Same chunk_id: pure node-list merge — ordering doesn't apply.
                                *loc = merged_location.clone();
                                updated = true;
                                break;
                            } else if loc.file_offset == merged_location.file_offset {
                                // Different chunk_id at same offset: guard against stale RCL
                                // arriving after a newer patch already updated the sled.
                                let should_update = match (merged_location.client_write_seq, loc.client_write_seq) {
                                    (Some(inc), Some(ext)) => inc >= ext,
                                    (Some(_), None)        => true,
                                    (None, Some(_))        => false,
                                    (None, None)           => true, // no ordering info — accept (existing behavior)
                                };
                                if should_update {
                                    *loc = merged_location.clone();
                                    updated = true;
                                } else {
                                    debug!("RCL sled update skipped: stale client_write_seq {:?} < existing {:?} for file {:?} offset {:?}",
                                        merged_location.client_write_seq, loc.client_write_seq, fid, loc.file_offset);
                                }
                                break;
                            }
                        }
                        if updated && !self.delete_tombstones.contains_key(&fid) {
                            if let Some(tx) = self.sled_write_tx.lock().unwrap().as_ref() {
                                let _ = tx.send(file_meta);
                            }
                        }
                    }
                }

                info!("Successfully replicated chunk location for {} (total nodes: {})",
                      merged_location.chunk_id, merged_location.nodes.len());

                // Do NOT re-broadcast to followers. The client sends ReplicateChunkLocation
                // only to the leader; followers receive authoritative full-file state via
                // PutFileMetadata (flush_metadata_sync) with write_seq ordering at the end
                // of each flush cycle. Broadcasting per-chunk location updates to followers
                // creates stale races: a late-arriving broadcast for an older chunk_id can
                // overwrite a newer patch result, making the chunk_map point to a renamed
                // (deleted) file and causing all subsequent patches to fail with NotFound.

                Response::Ok { data: None }
            }
            Err(e) => {
                warn!("Failed to replicate chunk location: {}", e);
                Response::Error {
                    message: format!("Failed to replicate chunk location: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle purge chunk location — remove an orphaned chunk: routing record.
    /// Sent by the cluster leader after it purges an orphan so that all followers
    /// stay in sync and don't accumulate stale records indefinitely.
    async fn handle_purge_chunk_location(&self, chunk_id: ChunkId) -> Response {
        debug!("Handling purge chunk location: {}", chunk_id);
        match self.metadata.delete_chunk_location(&chunk_id) {
            Ok(_) => {
                debug!("Purged chunk location record: {}", chunk_id);
                Response::Ok { data: None }
            }
            Err(e) => {
                warn!("Failed to purge chunk location {}: {}", chunk_id, e);
                Response::Error {
                    message: format!("Failed to purge chunk location: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    async fn handle_purge_chunk_locations(&self, chunk_ids: Vec<ChunkId>) -> Response {
        debug!("Handling batch purge of {} chunk locations", chunk_ids.len());
        let mut failed = 0usize;
        for chunk_id in &chunk_ids {
            if let Err(e) = self.metadata.delete_chunk_location(chunk_id) {
                warn!("Failed to purge chunk location {}: {}", chunk_id, e);
                failed += 1;
            } else {
                // Also delete the physical file — followers that missed DeleteChunk RPCs
                // while offline accumulate orphaned chunk files that routing-table-only
                // purges would leave on disk forever.
                if let Err(e) = self.storage.delete_chunk(chunk_id) {
                    debug!("Orphan {} not on local disk (ok): {}", chunk_id, e);
                }
            }
        }
        if failed == 0 {
            Response::Ok { data: None }
        } else {
            Response::Error {
                message: format!("Failed to purge {}/{} chunk locations", failed, chunk_ids.len()),
                code: ErrorCode::InternalError,
            }
        }
    }

    /// Handle ReconcileMetadata from the leader.
    /// Removes any file: and path: records whose ID is not in the live set.
    /// Runs in a blocking thread since it scans sled. Safe to call on any node.
    async fn handle_reconcile_metadata(&self, live_file_ids: Vec<dfs_common::FileId>) -> Response {
        let live_ids: std::collections::HashSet<dfs_common::FileId> =
            live_file_ids.into_iter().collect();
        let id_count = live_ids.len();
        let metadata = self.metadata.clone();
        match tokio::task::spawn_blocking(move || metadata.remove_unlisted_files(&live_ids)).await {
            Ok(Ok(removed)) => {
                if removed > 0 {
                    info!("ReconcileMetadata: removed {} stale records ({} live file IDs from leader)", removed, id_count);
                } else {
                    debug!("ReconcileMetadata: no stale records found ({} live file IDs from leader)", id_count);
                }
                Response::Ok { data: None }
            }
            Ok(Err(e)) => {
                warn!("ReconcileMetadata failed: {}", e);
                Response::Error {
                    message: format!("ReconcileMetadata failed: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
            Err(e) => {
                warn!("ReconcileMetadata spawn_blocking panicked: {}", e);
                Response::Error {
                    message: "ReconcileMetadata internal error".to_string(),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    async fn handle_replicate_chunk_locations(&self, locations: Vec<ChunkLocation>) -> Response {
        debug!("Handling batch replicate of {} chunk locations", locations.len());

        // Build the live chunk set once. Only accept locations for chunks that are
        // referenced by at least one active file. This prevents rejoining nodes from
        // resurrecting deleted files' chunks: when a node was offline, the leader deleted
        // those routing entries, but the node still has them in its local sled and pushes
        // them all back via chunk_location_sync on rejoin — causing the healer to
        // replicate them cluster-wide and inflating disk usage on every node.
        let live_chunks: std::collections::HashSet<dfs_common::ChunkId> = {
            let metadata = self.metadata.clone();
            match tokio::task::spawn_blocking(move || metadata.live_chunk_ids()).await {
                Ok(Ok(ids)) => ids,
                _ => {
                    // Can't load live set — accept everything to avoid silently dropping
                    // valid locations (safe fallback: healer will orphan-purge stale ones).
                    warn!("handle_replicate_chunk_locations: failed to load live chunk IDs, accepting all {} locations",
                          locations.len());
                    locations.iter().map(|l| l.chunk_id).collect()
                }
            }
        };

        let mut failed = 0usize;
        let mut rejected = 0usize;
        for location in &locations {
            // Reject locations for chunks not referenced by any live file.
            if !live_chunks.contains(&location.chunk_id) {
                rejected += 1;
                continue;
            }

            // Re-use existing merge logic from the single-item handler inline.
            let existing = self.metadata.get_chunk_location(&location.chunk_id)
                .ok().flatten();
            let incoming_count = location.nodes.len();
            let existing_count = existing.as_ref().map_or(0, |e| e.nodes.len());
            let rf = self.replication_factor;
            let nodes = if incoming_count > existing_count {
                location.nodes.clone()
            } else if incoming_count < rf && existing_count >= rf {
                continue; // stale early-write — skip
            } else {
                location.nodes.clone()
            };
            let merged = ChunkLocation {
                chunk_id: location.chunk_id,
                nodes,
                size: location.size,
                checksum: location.checksum,
                file_offset: location.file_offset.or_else(|| existing.as_ref().and_then(|e| e.file_offset)),
                written_at: location.written_at.or_else(|| existing.as_ref().and_then(|e| e.written_at)),
                client_write_seq: None,
                file_id: location.file_id.or_else(|| existing.as_ref().and_then(|e| e.file_id)),
            };
            if let Err(e) = self.metadata.put_chunk_location(&merged) {
                warn!("Failed to replicate chunk location {}: {}", location.chunk_id, e);
                failed += 1;
            }
        }
        if rejected > 0 {
            info!("handle_replicate_chunk_locations: rejected {} stale orphan locations (deleted files), accepted {}",
                  rejected, locations.len().saturating_sub(rejected + failed));
        }
        if failed == 0 {
            Response::Ok { data: None }
        } else {
            Response::Error {
                message: format!("Failed to replicate {}/{} chunk locations", failed, locations.len()),
                code: ErrorCode::InternalError,
            }
        }
    }

    async fn handle_replicate_metadata_batch(&self, items: Vec<FileMetadata>) -> Response {
        debug!("Handling batch replicate of {} metadata items", items.len());
        const TOMBSTONE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
        for metadata in items {
            // Tombstone check: reject items for recently-deleted files.
            // Matches the check in handle_replicate_metadata — without this, a
            // create flushed from pending_broadcasts after the delete tombstone was
            // set could resurrect the file on this follower.
            if let Some(entry) = self.delete_tombstones.get(&metadata.id) {
                if entry.value().elapsed() < TOMBSTONE_TTL {
                    continue;
                }
            }
            // write_seq bypass: newer session → accept as-is.
            // Same/old session → prefer chunk_map (chunk_map is updated only by RCL,
            // so it holds the authoritative current chunk_id; the broadcast may carry
            // a snapshot from before a concurrent patch and must not regress the map).
            let stored_write_seq_b = self.file_write_seqs.get(&metadata.id).map(|v| *v).unwrap_or(0);
            let bypass_b = metadata.write_seq > stored_write_seq_b;
            if bypass_b {
                self.file_write_seqs.insert(metadata.id, metadata.write_seq);
            }
            // Apply written_at guard unconditionally — bypass or not — but ONLY when the
            // incoming chunk has an explicit timestamp (written_at=Some(ts)). Fresh writes
            // use written_at=None (client deliberately omits timestamps to avoid clock-skew
            // issues) and must always be accepted. Patches always carry Some(patch_ts).
            //
            // The guard fires on bypass too because a concurrent FAP can send PutFileMetadata
            // for chunk_74 (seq=N, pre-patch snapshot of chunk_0) while the chunk_0 patch
            // is still in progress. seq=N bypasses, but its chunk_0 snapshot has an OLDER
            // ts than the patch result already on disk. Without the guard, this broadcast
            // reverts the chunk_map to the pre-patch hash — which no longer exists as a file —
            // creating the ghost that causes infinite ChunkStale retry loops.
            let metadata = {
                let mut m = metadata;
                if let Some(map_entry) = self.chunk_map.get(&m.id) {
                    let (map_locs, _) = map_entry.value();
                    for loc in m.chunk_locations.iter_mut() {
                        if let Some(file_offset) = loc.file_offset {
                            if let Some(map_loc) = map_locs.iter().find(|l| l.file_offset == Some(file_offset)) {
                                if map_loc.chunk_id != loc.chunk_id {
                                    if let Some(incoming_ts) = loc.written_at {
                                        // (Some, *): incoming is a patch result — compare timestamps.
                                        let existing_ts = map_loc.written_at.unwrap_or(0);
                                        if existing_ts > incoming_ts {
                                            *loc = map_loc.clone();
                                        }
                                    } else if map_loc.written_at.is_some() && !bypass_b {
                                        // (None, Some): incoming is a fresh-write snapshot, existing
                                        // is a patch result. Same/older session: the patch supersedes
                                        // this fresh write — keep existing. Accepting the None would
                                        // revert chunk_map to a hash that no longer exists → ghost.
                                        // bypass=true means newer write_seq (new session) → accept.
                                        *loc = map_loc.clone();
                                    }
                                    // (None, None) or (None, Some)+bypass: accept incoming.
                                }
                            }
                        }
                    }
                }
                m
            };
            self.chunk_map_update(&metadata).await;
            if let Some(tx) = self.sled_write_tx.lock().unwrap().as_ref() {
                let _ = tx.send(metadata.clone());
            }
        }
        Response::Ok { data: None }
    }

    /// Handle prefetch hint - warm cache with requested chunks (best-effort, low priority)
    /// Client sends this when it detects sequential reads to minimize future latency
    ///
    /// This runs in background with:
    /// - Concurrency limiting (max 2 concurrent prefetches via semaphore)
    /// - Throttling (50ms delay between chunks to spread I/O load)
    /// - Real client reads bypass the semaphore (they are high priority)
    async fn handle_prefetch_hint(&self, chunk_ids: Vec<ChunkId>) -> Response {
        info!("Received prefetch hint for {} chunks", chunk_ids.len());

        let storage = self.storage.clone();
        let semaphore = self.prefetch_semaphore.clone();
        let chunk_ids_clone = chunk_ids.clone();

        // Spawn background task to warm cache (non-blocking, best-effort, low priority)
        tokio::spawn(async move {
            let mut warmed = 0;
            let mut failed = 0;
            let mut skipped = 0;

            for chunk_id in chunk_ids_clone {
                // Acquire semaphore permit to limit concurrent prefetch operations
                // This prevents prefetch from overwhelming disk I/O
                let permit = match semaphore.try_acquire() {
                    Ok(p) => p,
                    Err(_) => {
                        // Too many prefetches in flight, skip this chunk
                        skipped += 1;
                        debug!("Skipping prefetch for chunk {} (too many in flight)", chunk_id);
                        continue;
                    }
                };

                match storage.warm_cache(&chunk_id) {
                    Ok(true) => {
                        warmed += 1;
                        debug!("Warmed cache for chunk {}", chunk_id);
                    }
                    Ok(false) => {
                        debug!("Chunk {} already in cache", chunk_id);
                    }
                    Err(e) => {
                        failed += 1;
                        debug!("Failed to warm cache for chunk {}: {}", chunk_id, e);
                    }
                }

                drop(permit); // Release semaphore

                // Minimal throttle to prevent CPU spinning, but allow aggressive prefetch
                // HDD read-ahead and OS page cache make sequential reads efficient
                tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
            }

            if warmed > 0 || failed > 0 || skipped > 0 {
                info!("Prefetch completed: {} warmed, {} failed, {} skipped", warmed, failed, skipped);
            }
        });

        // Return immediately - prefetch happens in background
        Response::PrefetchAccepted {
            accepted: chunk_ids.len(),
        }
    }

    /// Handle delete chunk request
    async fn handle_delete_chunk(&self, chunk_id: ChunkId) -> Response {
        debug!("Handling delete chunk: {}", chunk_id);

        // Clear tombstone: the chunk is being physically removed, so the healer
        // guard is no longer needed.
        self.chunk_tombstones.remove(&chunk_id);

        // Always delete the chunk location record, even if the chunk data isn't here.
        // A node may have a location record without the actual bytes — that stale record
        // must be purged too, otherwise it causes ghost entries after delete+rewrite.
        if let Err(e) = self.metadata.delete_chunk_location(&chunk_id) {
            warn!("Failed to delete chunk location record for {}: {}", chunk_id, e);
        }

        match self.storage.delete_chunk(&chunk_id) {
            Ok(_) => Response::Ok { data: None },
            Err(_) => {
                // Chunk not present locally — that's fine, location record already cleaned up.
                Response::Ok { data: None }
            }
        }
    }

    /// Handle has chunk request
    async fn handle_has_chunk(&self, chunk_id: ChunkId) -> Response {
        let exists = self.storage.has_chunk(&chunk_id);
        Response::Bool { value: exists }
    }

    async fn handle_has_chunks(&self, chunk_ids: Vec<ChunkId>) -> Response {
        // A tombstoned chunk must not be reported as present — the healer would
        // otherwise select this node as a source and replicate the old chunk_id
        // back to the two dual-RF patched replicas before metadata is committed.
        let values = chunk_ids.iter()
            .map(|id| !self.chunk_tombstones.contains(id) && self.storage.has_chunk(id))
            .collect();
        Response::BoolVec { values }
    }

    /// Write data to the cluster with replication
    pub async fn write_data(&self, data: &[u8], file_id: dfs_common::FileId) -> Result<Vec<(ChunkId, u64, Vec<dfs_common::NodeId>)>> {
        let total_start = std::time::Instant::now();
        info!("Writing {} bytes to cluster", data.len());

        // Chunk the data
        let chunk_start = std::time::Instant::now();
        let chunks = self.chunker.chunk_data(data, file_id);
        let chunk_time = chunk_start.elapsed();
        info!("Chunking took {:?} for {} chunks", chunk_time, chunks.len());

        // Process ALL chunks in parallel for maximum throughput
        let mut chunk_tasks = Vec::new();

        for (chunk_id, chunk_data) in chunks {
            let cluster = self.cluster.clone();
            let storage = self.storage.clone();
            let metadata = self.metadata.clone();
            let client = self.client.clone();
            let replication_factor = self.replication_factor;

            // Spawn a task for each chunk
            let task = tokio::spawn(async move {
                let chunk_total_start = std::time::Instant::now();

                // Determine target nodes using capacity-aware placement
                // This prefers nodes with more available space
                let target_nodes = cluster
                    .get_nodes_with_capacity_awareness(&chunk_id, replication_factor)
                    .await;

                if target_nodes.is_empty() {
                    anyhow::bail!("No nodes available for chunk {}", chunk_id);
                }

                debug!(
                    "Replicating chunk {} to {} nodes",
                    chunk_id,
                    target_nodes.len()
                );

                // Optimized replication strategy:
                // - RF=2: Write to 2 nodes synchronously (quorum=2)
                // - RF=3: Write to 2 nodes synchronously, 3rd replica happens in background
                // This reduces client bandwidth and network hops for RF=3
                let immediate_replicas = if replication_factor >= 3 {
                    2  // For RF=3+, only write 2 copies immediately
                } else {
                    replication_factor  // For RF=1 or RF=2, write all immediately
                };

                let quorum = immediate_replicas;

                // Spawn parallel replication tasks
                let mut quorum_tasks = Vec::new();
                let mut async_tasks = Vec::new();

                for (idx, node_id) in target_nodes.iter().enumerate() {
                    let node_id = *node_id;
                    let chunk_id = chunk_id;
                    let chunk_data = chunk_data.clone();

                    // First 'quorum' nodes: wait for these
                    // Remaining nodes: fire-and-forget (async replication)
                    let is_quorum_node = idx < quorum;

                    if node_id == cluster.local_node_id() {
                        // Local write
                        let storage = storage.clone();
                        let task = tokio::spawn(async move {
                            storage.write_chunk(&chunk_id, &chunk_data).is_ok()
                        });

                        if is_quorum_node {
                            quorum_tasks.push(task);
                        } else {
                            async_tasks.push(task);
                        }
                    } else {
                        // Remote write
                        let cluster = cluster.clone();
                        let client = client.clone();

                        let task = tokio::spawn(async move {
                            if let Some(node_info) = cluster.get_node(&node_id).await {
                                let request = Request::ReplicateChunk {
                                    chunk_id,
                                    data: chunk_data,
                                    checksum: chunk_id.hash,
                                    written_at: None,
                                };

                                match client
                                    .send_message(node_info.addr, Message::Request(request))
                                    .await
                                {
                                    Ok(response) => matches!(
                                        response.message,
                                        Message::Response(Response::Ok { .. })
                                    ),
                                    Err(_) => false,
                                }
                            } else {
                                false
                            }
                        });

                        if is_quorum_node {
                            quorum_tasks.push(task);
                        } else {
                            async_tasks.push(task);
                        }
                    }
                }

                // Wait ONLY for quorum tasks (fast path)
                let quorum_start = std::time::Instant::now();
                let mut success_count = 0;
                for task in quorum_tasks {
                    if let Ok(true) = task.await {
                        success_count += 1;
                    }
                }
                let quorum_time = quorum_start.elapsed();

                if success_count < quorum {
                    anyhow::bail!(
                        "Failed to achieve quorum for chunk {} ({}/{})",
                        chunk_id,
                        success_count,
                        quorum
                    );
                }

                info!("Chunk {} quorum write took {:?} ({} nodes)", chunk_id, quorum_time, success_count);

                // Async tasks continue in background - no waiting!
                // Auto-healing will catch any failures later
                debug!(
                    "Chunk {} written to quorum ({} nodes), {} additional replicas in progress",
                    chunk_id,
                    success_count,
                    async_tasks.len()
                );

                // Store chunk location metadata
                let location = ChunkLocation {
                    chunk_id,
                    nodes: target_nodes.clone(),
                    size: chunk_data.len(),
                    checksum: chunk_id.hash,
                    file_offset: None,  // Server-side replication doesn't track file offsets
                    written_at: None,
                    client_write_seq: None,
                    file_id: Some(file_id),
                };

                let metadata_start = std::time::Instant::now();
                metadata
                    .put_chunk_location(&location)
                    .context("Failed to store chunk location")?;
                let metadata_time = metadata_start.elapsed();

                // Replicate chunk location metadata to all other nodes asynchronously
                // This ensures all servers know about chunk locations for consistency
                let nodes = cluster.get_all_nodes().await;
                let local_id = cluster.local_node_id();

                info!("Replicating chunk location for {} to {} nodes", chunk_id, nodes.len() - 1);

                for node in nodes {
                    // Skip self and offline nodes
                    if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                        continue;
                    }

                    let client_clone = client.clone();
                    let location_clone = location.clone();
                    let node_addr = node.addr;
                    let node_id = node.id;
                    let chunk_id_clone = chunk_id;

                    // Fire-and-forget: spawn individual replication tasks
                    tokio::spawn(async move {
                        info!("Sending chunk location {} to node {}", chunk_id_clone, node_id);
                        let request = Request::ReplicateChunkLocation {
                            location: location_clone,
                            file_id: None,
                        };

                        if let Err(e) = client_clone.send_message(node_addr, Message::Request(request)).await {
                            warn!("Failed to replicate chunk location {} to node {}: {}", chunk_id_clone, node_id, e);
                        } else {
                            info!("Successfully sent chunk location {} to node {}", chunk_id_clone, node_id);
                        }
                    });
                }

                let chunk_total_time = chunk_total_start.elapsed();
                info!("Chunk {} complete in {:?} (metadata: {:?})", chunk_id, chunk_total_time, metadata_time);

                Ok::<(ChunkId, u64, Vec<dfs_common::NodeId>), anyhow::Error>((chunk_id, chunk_data.len() as u64, target_nodes))
            });

            chunk_tasks.push(task);
        }

        // Wait for all chunk tasks to complete in parallel
        let gather_start = std::time::Instant::now();
        let mut chunk_ids_with_sizes = Vec::new();
        for task in chunk_tasks {
            match task.await {
                Ok(Ok(chunk_id_with_size_and_nodes)) => chunk_ids_with_sizes.push(chunk_id_with_size_and_nodes),
                Ok(Err(e)) => return Err(e),
                Err(e) => anyhow::bail!("Chunk task panicked: {}", e),
            }
        }
        let gather_time = gather_start.elapsed();

        let total_time = total_start.elapsed();
        let throughput = (data.len() as f64 / 1024.0 / 1024.0) / total_time.as_secs_f64();
        info!("Write complete: {} bytes in {:?} ({:.2} MB/s) - gather: {:?}",
              data.len(), total_time, throughput, gather_time);

        info!("Successfully wrote {} chunks", chunk_ids_with_sizes.len());
        Ok(chunk_ids_with_sizes)
    }

    /// Write data locally only (no replication)
    /// Used for optimized RF=3+ writes where client sends to 2 servers in parallel
    /// Healing creates the 3rd replica in background
    pub async fn write_data_local_only(&self, data: &[u8], file_id: dfs_common::FileId) -> Result<Vec<(ChunkId, u64)>> {
        let total_start = std::time::Instant::now();
        info!("Writing {} bytes locally (no replication)", data.len());

        // Chunk the data
        let chunks = self.chunker.chunk_data(data, file_id);
        info!("Chunked into {} chunks (local write only)", chunks.len());

        // Write all chunks locally in parallel
        let mut chunk_tasks = Vec::new();

        for (chunk_id, chunk_data) in chunks {
            let storage = self.storage.clone();
            let metadata = self.metadata.clone();
            let cluster = self.cluster.clone();
            let client = self.client.clone();
            let local_node_id = self.cluster.local_node_id();

            let task = tokio::spawn(async move {
                // Write chunk locally
                storage.write_chunk(&chunk_id, &chunk_data)
                    .context(format!("Failed to write chunk {} locally", chunk_id))?;

                // Store chunk location metadata (with only local node)
                let location = ChunkLocation {
                    chunk_id,
                    nodes: vec![local_node_id],  // Only local node
                    size: chunk_data.len(),
                    checksum: chunk_id.hash,
                    file_offset: None,  // Server-side local-only writes don't track file offsets
                    written_at: None,
                    client_write_seq: None,
                    file_id: Some(file_id),
                };

                metadata.put_chunk_location(&location)
                    .context("Failed to store chunk location")?;

                // Do NOT broadcast a single-node location here. The client is the authoritative
                // source for the complete replica set — it knows all nodes it wrote to and
                // broadcasts a full ChunkLocation (e.g. {nodes: [A, B]}) after both parallel
                // writes succeed. Broadcasting a single-node record here races with that broadcast
                // and causes the healer to see under-replicated chunks before the client has
                // finished, triggering premature healing and resulting in 5× over-replication.

                Ok::<(ChunkId, u64), anyhow::Error>((chunk_id, chunk_data.len() as u64))
            });

            chunk_tasks.push(task);
        }

        // Wait for all chunks to complete
        let mut chunk_ids_with_sizes = Vec::new();
        for task in chunk_tasks {
            match task.await {
                Ok(Ok(chunk_id_with_size)) => chunk_ids_with_sizes.push(chunk_id_with_size),
                Ok(Err(e)) => return Err(e),
                Err(e) => anyhow::bail!("Chunk task panicked: {}", e),
            }
        }

        let total_time = total_start.elapsed();
        let throughput = (data.len() as f64 / 1024.0 / 1024.0) / total_time.as_secs_f64();
        info!("Local write complete: {} bytes in {:?} ({:.2} MB/s) - {} chunks",
              data.len(), total_time, throughput, chunk_ids_with_sizes.len());

        Ok(chunk_ids_with_sizes)
    }

    pub async fn write_data_local_only_at(&self, data: &[u8], file_offset: u64, file_id: dfs_common::FileId) -> Result<Vec<(ChunkId, u64)>> {
        let total_start = std::time::Instant::now();
        info!("Writing {} bytes locally (no replication) at offset {}", data.len(), file_offset);

        let chunks = self.chunker.chunk_data_at(data, file_offset, file_id);
        info!("Chunked into {} chunks (local write only)", chunks.len());

        let local_node_id = self.cluster.local_node_id();
        let mut chunk_tasks = Vec::new();

        for (chunk_id, chunk_data) in chunks {
            let storage = self.storage.clone();
            let metadata = self.metadata.clone();

            let task = tokio::spawn(async move {
                storage.write_chunk(&chunk_id, &chunk_data)
                    .context(format!("Failed to write chunk {} locally", chunk_id))?;

                let location = ChunkLocation {
                    chunk_id,
                    nodes: vec![local_node_id],
                    size: chunk_data.len(),
                    checksum: chunk_id.hash,
                    file_offset: None,
                    written_at: None,
                    client_write_seq: None,
                    file_id: Some(file_id),
                };

                metadata.put_chunk_location(&location)
                    .context("Failed to store chunk location")?;

                Ok::<(ChunkId, u64), anyhow::Error>((chunk_id, chunk_data.len() as u64))
            });

            chunk_tasks.push(task);
        }

        let mut chunk_ids_with_sizes = Vec::new();
        for task in chunk_tasks {
            match task.await {
                Ok(Ok(chunk_id_with_size)) => chunk_ids_with_sizes.push(chunk_id_with_size),
                Ok(Err(e)) => return Err(e),
                Err(e) => anyhow::bail!("Chunk task panicked: {}", e),
            }
        }

        let total_time = total_start.elapsed();
        let throughput = (data.len() as f64 / 1024.0 / 1024.0) / total_time.as_secs_f64();
        info!("Local write complete: {} bytes in {:?} ({:.2} MB/s) - {} chunks",
              data.len(), total_time, throughput, chunk_ids_with_sizes.len());

        Ok(chunk_ids_with_sizes)
    }

    /// Read data from the cluster by chunk IDs
    pub async fn read_data(&self, chunk_ids: &[ChunkId]) -> Result<Vec<u8>> {
        info!("Reading {} chunks from cluster", chunk_ids.len());

        let mut all_chunks = Vec::new();

        for chunk_id in chunk_ids {
            let chunk_data = self.read_chunk(chunk_id).await?;
            all_chunks.push(chunk_data);
        }

        // Reassemble chunks
        let data = self.chunker.reassemble_chunks(all_chunks);

        info!("Successfully read {} bytes", data.len());
        Ok(data)
    }

    /// Read a single chunk from the cluster
    async fn read_chunk(&self, chunk_id: &ChunkId) -> Result<Vec<u8>> {
        // Try reading from local storage (OS page cache handles caching automatically)
        if let Ok(data) = self.storage.read_chunk(chunk_id) {
            debug!("Read chunk {} from local storage", chunk_id);
            return Ok(data);
        }

        // Fast path: chunk recently failed from all online nodes — skip until TTL expires.
        const MISSING_TTL: std::time::Duration = std::time::Duration::from_secs(300);
        {
            let map = self.missing_chunks.read().await;
            if let Some(&blocklisted_at) = map.get(chunk_id) {
                if blocklisted_at.elapsed() < MISSING_TTL {
                    anyhow::bail!("Chunk {} is temporarily unavailable (blocklisted)", chunk_id);
                }
                // TTL expired — fall through and retry
            }
        }

        // Get chunk location from metadata
        let location = self
            .metadata
            .get_chunk_location(chunk_id)
            .context("Failed to get chunk location")?
            .ok_or_else(|| anyhow::anyhow!("Chunk location not found"))?;

        // Try reading from remote nodes
        let mut online_nodes_tried = 0;
        let mut online_nodes_total = 0;

        for node_id in &location.nodes {
            if node_id == &self.cluster.local_node_id() {
                continue; // Already tried local
            }

            if let Some(node_info) = self.cluster.get_node(node_id).await {
                if node_info.status != dfs_common::NodeStatus::Online {
                    continue;
                }
                online_nodes_total += 1;

                let request = Request::ReadChunk {
                    chunk_id: *chunk_id,
                    sequential_hint: None,
                    client_write_seq: None,
                };

                let result = tokio::time::timeout(
                    tokio::time::Duration::from_secs(10),
                    self.client.send_message(node_info.addr, Message::Request(request)),
                ).await;
                match result {
                    Ok(Ok(response)) => match response.message {
                        Message::Response(Response::ChunkData { data, .. }) => {
                            debug!("Read chunk {} from remote node {}", chunk_id, node_id);
                            return Ok(data);
                        }
                        _ => { online_nodes_tried += 1; continue; }
                    },
                    Ok(Err(e)) => {
                        warn!("Failed to read from node {}: {}", node_id, e);
                        online_nodes_tried += 1;
                        continue;
                    }
                    Err(_) => {
                        warn!("Read timeout from node {} for chunk {}", node_id, chunk_id);
                        online_nodes_tried += 1;
                        continue;
                    }
                }
            }
        }

        // If every online node we tried failed, temporarily blocklist this chunk
        // to avoid connection storms. TTL expires after 5 minutes (node may recover).
        if online_nodes_tried > 0 && online_nodes_tried >= online_nodes_total {
            warn!("Chunk {} unavailable from all {} online nodes — blocklisting for 5m",
                  chunk_id, online_nodes_tried);
            let mut map = self.missing_chunks.write().await;
            map.insert(*chunk_id, std::time::Instant::now());
            // Prune stale entries while we hold the write lock.
            const MISSING_TTL: std::time::Duration = std::time::Duration::from_secs(300);
            map.retain(|_, t| t.elapsed() < MISSING_TTL);
        }

        anyhow::bail!("Failed to read chunk {} from any node", chunk_id)
    }

    /// Get or create chunk location metadata
    async fn get_or_create_chunk_location(
        &self,
        chunk_id: &ChunkId,
        size: usize,
    ) -> Result<ChunkLocation> {
        if let Ok(Some(location)) = self.metadata.get_chunk_location(chunk_id) {
            Ok(location)
        } else {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            Ok(ChunkLocation {
                chunk_id: *chunk_id,
                nodes: Vec::new(),
                size,
                checksum: chunk_id.hash,
                file_offset: None,
                written_at: Some(now_ms),
                client_write_seq: None,
                file_id: None,
            })
        }
    }

    /// Handle get file metadata by path request
    async fn handle_get_file_metadata_by_path(&self, path: String, if_modified_since: Option<u64>) -> Response {
        debug!("Handling get file metadata by path: {} (if_modified_since: {:?})", path, if_modified_since);

        // Offload synchronous RocksDB read to blocking thread pool.
        let metadata = self.metadata.clone();
        let path_clone = path.clone();
        let lookup = tokio::task::spawn_blocking(move || metadata.get_file_by_path(&path_clone)).await;

        match lookup {
            Err(e) => {
                warn!("spawn_blocking panicked in get_file_metadata_by_path: {}", e);
                return Response::Error {
                    message: "Internal error fetching metadata".to_string(),
                    code: ErrorCode::InternalError,
                };
            }
            Ok(result) => match result {
            Ok(Some(mut metadata)) => {
                // Always prefer the in-memory chunk_map over sled for chunk_locations.
                // The sled write worker is async — reads immediately after a write will
                // hit sled before the worker commits, returning stale chunk IDs (T7/T20).
                // The chunk_map is updated synchronously in handle_put_file_metadata
                // before the client gets its Ok response, so it's always current.
                if let Some(entry) = self.chunk_map.get(&metadata.id) {
                    let (map_locs, map_write_seq) = entry.value();
                    if !map_locs.is_empty() {
                        metadata.chunk_locations = map_locs.clone();
                        if *map_write_seq > metadata.write_seq {
                            metadata.write_seq = *map_write_seq;
                        }
                    }
                }

                // Check if client has provided a cached write_seq (clock-agnostic —
                // modified_at is user-settable via setattr and not safe for this check).
                if let Some(cached_write_seq) = if_modified_since {
                    if metadata.write_seq <= cached_write_seq {
                        debug!("Metadata not modified for {}: write_seq {} <= {}", path, metadata.write_seq, cached_write_seq);
                        return Response::NotModified;
                    }
                }

                Response::FileMetadata { metadata }
            }
            Ok(None) => {
                // Not found locally. If we are a follower, metadata might not have
                // replicated yet — forward to leader for authoritative answer.
                // If we are the leader, file definitively doesn't exist.
                if !self.cluster.is_leader().await {
                    if let Some(leader_addr) = self.cluster.get_leader_addr().await {
                        debug!("File {} not found on follower — forwarding query to leader {}", path, leader_addr);
                        let forward_result = tokio::time::timeout(
                            std::time::Duration::from_secs(3),
                            self.client.send_message(leader_addr, Message::Request(Request::GetFileMetadataByPath {
                                path: path.clone(),
                                if_modified_since,
                            }))
                        ).await;

                        match forward_result {
                            Ok(Ok(envelope)) => {
                                if let Message::Response(response) = envelope.message {
                                    debug!("Follower forwarded {} to leader, got: {:?}", path,
                                           if matches!(response, Response::FileMetadata { .. }) { "FileMetadata" }
                                           else { "NotFound/Error" });
                                    return response;
                                }
                            }
                            Ok(Err(e)) => {
                                warn!("Failed to forward {} query to leader {}: {}", path, leader_addr, e);
                                // Fall through to NotFound
                            }
                            Err(_) => {
                                warn!("Timeout forwarding {} query to leader {}", path, leader_addr);
                                // Fall through to NotFound
                            }
                        }
                    } else {
                        debug!("File {} not found on follower but no leader available", path);
                        // Fall through to NotFound
                    }
                }
                Response::Error {
                    message: "File not found".to_string(),
                    code: ErrorCode::NotFound,
                }
            }
            Err(e) => {
                warn!("Failed to get file metadata: {}", e);
                Response::Error {
                    message: format!("Failed to get file metadata: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        } // end inner match result
        } // end outer match lookup
    }

    /// Immediately broadcast metadata to all online followers with the given TTL.
    /// Fire-and-forget (spawned). Used by the leader for low-latency propagation;
    /// the dissemination queue is the durable catch-up path for offline nodes.
    async fn broadcast_metadata_to_followers(&self, metadata: &FileMetadata, ttl: u8) {
        // Don't broadcast empty seq=0 creates — no chunk data, just noise on the wire.
        if metadata.write_seq == 0 && metadata.chunk_locations.is_empty() {
            return;
        }
        let cluster = self.cluster.clone();
        let client = self.client.clone();
        let local_id = self.cluster.local_node_id();
        let metadata_clone = metadata.clone();
        let sem = self.broadcast_semaphore.clone();
        tokio::spawn(async move {
            let _permit = sem.acquire().await.ok();
            let nodes = cluster.get_all_nodes().await;
            for node in &nodes {
                if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                    continue;
                }
                let req = Request::ReplicateMetadata { metadata: metadata_clone.clone(), ttl };
                let result = tokio::time::timeout(
                    tokio::time::Duration::from_secs(5),
                    client.send_message(node.addr, Message::Request(req)),
                ).await;
                if let Err(e) = result.map_err(|_| anyhow::anyhow!("timeout")).and_then(|r| r) {
                    warn!("broadcast_metadata_to_followers: failed to reach {}: {}", node.id, e);
                }
            }
        });
    }

    /// Enqueue a metadata update into the per-follower durable sled queue.
    /// Only the leader calls this. The dissemination loop drains the queue every 5s.
    /// Non-leaders skip enqueueing — they just store locally and let the leader
    /// handle dissemination to other followers.
    async fn enqueue_metadata_for_followers(&self, metadata: &FileMetadata) {
        if !self.cluster.is_leader().await {
            return;
        }

        // Don't queue seq=0 empty creates — they carry no chunk data and generate a
        // continuous replay storm from the dissemination loop that buries real writes.
        // The real metadata (with write_seq>0 and chunk_locations) will be enqueued
        // when the flush sends it.
        if metadata.write_seq == 0 && metadata.chunk_locations.is_empty() {
            return;
        }

        let local_id = self.cluster.local_node_id();
        let nodes = self.cluster.get_all_nodes().await;

        // Only enqueue for offline followers — online nodes receive the update
        // immediately via the pending_broadcasts flush loop. This check MUST happen
        // before next_meta_sequence(): that call does a synchronous sled write, and
        // under a write storm (rsync of 1000s of files) calling it 3500 times/sec
        // blocks Tokio worker threads even when there is nothing to enqueue.
        let offline_followers: Vec<_> = nodes.iter()
            .filter(|n| n.id != local_id && n.status != dfs_common::NodeStatus::Online)
            .collect();
        if offline_followers.is_empty() {
            return;
        }

        let seq = match self.metadata.next_meta_sequence() {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to increment meta sequence: {}", e);
                return;
            }
        };

        for node in &offline_followers {
            if let Err(e) = self.metadata.enqueue_meta_for_node(node.id, seq, metadata) {
                warn!("Failed to enqueue metadata for node {}: {}", node.id, e);
            }
        }
    }

    /// Record a metadata write in the short-term gossip ring.
    /// Evicts the oldest entry when the ring exceeds 512 items.
    fn record_recent_write(&self, metadata: FileMetadata) {
        // DashMap insert is O(1) and deduplicates by file_id automatically —
        // a newer write for the same file replaces the older one, which is exactly
        // what we want. No mutex, no O(n) retain, no contention under write storms.
        self.recent_writes.insert(metadata.id, (metadata, std::time::Instant::now()));
    }

    /// Short-term gossip loop: every 15s broadcast all writes from the last 30s
    /// to every known peer (including the leader) with TTL=0 (no re-broadcast).
    /// Fire-and-forget — a missed delivery is covered by the long-term reconciliation.
    pub fn start_metadata_gossip_loop(self: Arc<Self>) {
        let server = self.clone();
        tokio::spawn(async move {
            const GOSSIP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
            const GOSSIP_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);
            loop {
                tokio::time::sleep(GOSSIP_INTERVAL).await;

                // Collect writes from the last 30s, evict stale entries in the same pass.
                // DashMap.retain is lock-striped and doesn't block other inserts.
                let batch: Vec<FileMetadata> = {
                    server.recent_writes.retain(|_, (_, t)| t.elapsed() < GOSSIP_WINDOW * 2);
                    server.recent_writes.iter()
                        .filter(|e| e.value().1.elapsed() < GOSSIP_WINDOW)
                        .map(|e| e.value().0.clone())
                        .collect()
                };

                if batch.is_empty() {
                    continue;
                }

                let nodes = server.cluster.get_all_nodes().await;
                let local_id = server.cluster.local_node_id();
                let leader_addr = server.cluster.get_leader_addr().await;

                // Build target list: all online peers + the leader (even if also a peer).
                let mut targets: Vec<std::net::SocketAddr> = nodes.iter()
                    .filter(|n| n.id != local_id && n.status == dfs_common::NodeStatus::Online)
                    .map(|n| n.addr)
                    .collect();
                // Ensure the leader is included even if not yet in the node list.
                if let Some(la) = leader_addr {
                    if la != server.cluster.local_addr() && !targets.contains(&la) {
                        targets.push(la);
                    }
                }

                if targets.is_empty() {
                    continue;
                }

                debug!("[META GOSSIP] broadcasting {} recent writes to {} peers", batch.len(), targets.len());

                // Send all gossip items to each peer in a single DisseminateMetadata RPC
                // (up_to_sequence=0 sentinel so followers don't bump their sequence bookkeeping).
                // Previously each item was a separate spawn+connection — 512 items × 4 peers =
                // 2048 concurrent connections, filling followers' MAX_CONNECTIONS=128 semaphore
                // and causing health-check failures. One batch RPC per peer fixes this.
                for addr in targets {
                    let client = server.client.clone();
                    let batch_clone = batch.clone();
                    let sem = server.broadcast_semaphore.clone();
                    tokio::spawn(async move {
                        let _permit = sem.acquire().await.ok();
                        let req = dfs_common::Message::Request(dfs_common::Request::DisseminateMetadata {
                            items: batch_clone,
                            up_to_sequence: 0, // sentinel: don't advance follower sequence
                        });
                        let _ = tokio::time::timeout(
                            tokio::time::Duration::from_secs(5),
                            client.send_message(addr, req),
                        ).await;
                    });
                }
            }
        });
    }

    /// Metadata healer: drains pending_broadcasts every 5s and pushes authoritative
    /// metadata to all online followers. Unlike the old broadcast_flush_loop, this
    /// rebuilds chunk_locations from the leader's authoritative chunk_map before
    /// sending — client-originated chunk_locations in pending_broadcasts are ignored.
    /// A full-sweep every 5 minutes catches followers that missed dirty-file pushes.
    /// Periodic redb compaction.  Runs every 5 minutes normally; backs off to 2 minutes
    /// when the local disk is above 70% full so bloated COW pages are reclaimed before
    /// the disk hits critical levels.  Staggered by node address so all nodes never
    /// compact simultaneously.  Retries up to 3 times on "transaction in progress" errors
    /// (a transient race during startup) before giving up for the current cycle.
    pub fn start_compaction_loop(self: Arc<Self>) {
        let metadata = self.metadata.clone();
        let storage = self.storage.clone();
        let node_byte = self.cluster.local_node_id().as_bytes()[0] as u64;
        let cluster = self.cluster.clone();
        tokio::spawn(async move {
            // Wait for the cluster to establish before computing the stagger.
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            let node_count = cluster.get_all_nodes().await.len().max(1) as u64;
            // Stagger within a 5-minute window so nodes don't all compact at once.
            let interval_secs: u64 = 5 * 60;
            let stagger_secs = (node_byte % node_count) * (interval_secs / node_count);
            tokio::time::sleep(tokio::time::Duration::from_secs(stagger_secs)).await;
            // Tracks the DB size immediately after the last successful compact.
            // Zero means no baseline yet — compact unconditionally on first run.
            let mut last_compact_size: u64 = 0;
            let mut last_compact_time: Option<std::time::Instant> = None;
            loop {
                // The fragmentation gate makes this interval cheap (just a stat()
                // call on most iterations). Keep it at 60s so compaction triggers
                // promptly when fragmentation crosses the threshold.
                let sleep_secs = 60u64;

                let current_size = metadata.db_size();
                let frag_ratio = if last_compact_size > 0 {
                    current_size as f64 / last_compact_size as f64
                } else {
                    f64::INFINITY // first run: always compact
                };
                let secs_since_compact = last_compact_time
                    .map(|t| t.elapsed().as_secs())
                    .unwrap_or(u64::MAX);

                if last_compact_size > 0 && frag_ratio >= 2.0 {
                    warn!("redb fragmentation high: {:.1}MB (last compact baseline: {:.1}MB)",
                        current_size as f64 / 1_048_576.0,
                        last_compact_size as f64 / 1_048_576.0);
                }

                // Compact if fragmentation ≥ 20%, or 30 minutes have passed since the
                // last compact (catches latent free pages that redb only reclaims later).
                if frag_ratio < 1.20 && secs_since_compact < 30 * 60 {
                    tokio::time::sleep(tokio::time::Duration::from_secs(sleep_secs)).await;
                    continue;
                }

                // Retry up to 3 times on transient "transaction in progress" errors.
                let mut last_err = None;
                for attempt in 0..3u8 {
                    if attempt > 0 {
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    }
                    let m = metadata.clone();
                    let task = tokio::task::spawn_blocking(move || m.compact_db());
                    // compact_db() holds MetadataStore's exclusive write lock for its
                    // entire duration; with the periodic durability flush (see
                    // next_write_durability()) it should complete in milliseconds even
                    // for a large DB. If it ever takes minutes, the write lock is
                    // permanently wedged and every metadata read on this node will
                    // block forever too — there is no way to un-stick that lock from
                    // here. Restart so the other replicas (HA) keep serving while a
                    // fresh process gets a clean redb handle (compact on startup is
                    // fast on a freshly-opened handle even for the same file).
                    let result = tokio::time::timeout(std::time::Duration::from_secs(60), task).await;
                    let result = match result {
                        Ok(r) => r,
                        Err(_) => {
                            error!(
                                "redb compact_db() exceeded 60s — exclusive metadata write \
                                 lock is permanently wedged on this node. Restarting so HA \
                                 replicas can continue serving."
                            );
                            std::process::exit(1);
                        }
                    };
                    match result {
                        Ok(Ok((before, after))) => {
                            if before != after {
                                info!("redb compacted: {:.1}MB → {:.1}MB",
                                    before as f64 / 1_048_576.0,
                                    after  as f64 / 1_048_576.0);
                            }
                            last_compact_size = after; // update baseline only on success
                            last_compact_time = Some(std::time::Instant::now());
                            last_err = None;
                            break;
                        }
                        Ok(Err(e)) => {
                            let msg = e.to_string();
                            if msg.contains("transaction") || msg.contains("in progress") {
                                last_err = Some(msg);
                                // transient — retry after brief delay
                            } else {
                                warn!("redb periodic compact failed: {}", e);
                                last_err = None;
                                break;
                            }
                        }
                        Err(e) => {
                            warn!("redb periodic compact panicked: {}", e);
                            last_err = None;
                            break;
                        }
                    }
                }
                if let Some(e) = last_err {
                    warn!("redb periodic compact failed after retries: {}", e);
                }

                tokio::time::sleep(tokio::time::Duration::from_secs(sleep_secs)).await;
            }
        });
    }

    pub fn start_ops_tracker_loop(self: Arc<Self>) {
        let tracker = self.ops_tracker.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                tracker.tick();
            }
        });
    }

    pub fn start_metadata_healer_loop(self: Arc<Self>) {
        let server = self;
        tokio::spawn(async move {
            const DIRTY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
            const FULL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);
            const TOMBSTONE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
            let mut last_full = std::time::Instant::now();
            loop {
                tokio::time::sleep(DIRTY_INTERVAL).await;

                if !server.cluster.is_leader().await {
                    server.pending_broadcasts.clear();
                    continue;
                }

                // Dirty-file push: rebuild authoritative metadata for changed files.
                if !server.pending_broadcasts.is_empty() {
                    let dirty_entries: Vec<(dfs_common::FileId, dfs_common::FileMetadata)> = server
                        .pending_broadcasts
                        .iter()
                        .filter(|e| {
                            // Skip recently tombstoned files.
                            server.delete_tombstones.get(e.key())
                                .map(|t| t.value().elapsed() >= TOMBSTONE_TTL)
                                .unwrap_or(true)
                        })
                        .map(|e| (*e.key(), e.value().clone()))
                        .collect();
                    server.pending_broadcasts.clear();

                    if !dirty_entries.is_empty() {
                        // Rebuild chunk_locations from leader's authoritative chunk_map.
                        let batch: Vec<dfs_common::FileMetadata> = dirty_entries
                            .into_iter()
                            .map(|(file_id, mut meta)| {
                                if let Some(map_entry) = server.chunk_map.get(&file_id) {
                                    meta.chunk_locations = map_entry.value().0.clone();
                                }
                                meta
                            })
                            .collect();

                        let local_id = server.cluster.local_node_id();
                        let nodes = server.cluster.get_all_nodes().await;
                        for node in &nodes {
                            if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                                continue;
                            }
                            let req = dfs_common::Request::ReplicateMetadataBatch { items: batch.clone() };
                            let client = server.client.clone();
                            let addr = node.addr;
                            let sem = server.broadcast_semaphore.clone();
                            tokio::spawn(async move {
                                let _permit = sem.acquire().await.ok();
                                let _ = tokio::time::timeout(
                                    std::time::Duration::from_secs(5),
                                    client.send_message(addr, dfs_common::Message::Request(req)),
                                ).await;
                            });
                        }
                    }
                }

                // Full-sweep: push any file that an online follower is missing or behind on.
                if last_full.elapsed() >= FULL_INTERVAL {
                    last_full = std::time::Instant::now();
                    server.run_metadata_push_to_followers().await;
                }
            }
        });
    }

    /// Full-sweep push: for each online follower, find files where the follower's
    /// write_seq lags the leader's, and push the authoritative metadata for those files.
    /// Runs every 5 minutes from start_metadata_healer_loop. Guarantees ≥N copies of
    /// metadata across the cluster even after network partitions or node restarts.
    async fn run_metadata_push_to_followers(&self) {
        if !self.cluster.is_leader().await {
            return;
        }
        let local_id = self.cluster.local_node_id();
        let nodes = self.cluster.get_all_nodes().await;

        // Build the leader's authoritative inventory: FileId → write_seq.
        let my_inventory: std::collections::HashMap<dfs_common::FileId, u64> = {
            let meta = self.metadata.clone();
            match tokio::task::spawn_blocking(move || meta.get_file_inventory()).await {
                Ok(Ok(v)) => v.into_iter().collect(),
                Ok(Err(e)) => { warn!("metadata_healer full-sweep: local inventory failed: {}", e); return; }
                Err(e) => { warn!("metadata_healer full-sweep: spawn_blocking panic: {}", e); return; }
            }
        };
        if my_inventory.is_empty() { return; }

        for node in &nodes {
            if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                continue;
            }

            // Get the follower's inventory.
            let inv = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.client.send_message(node.addr, dfs_common::Message::Request(dfs_common::Request::GetFileInventory)),
            ).await;
            let follower_inv: Vec<(dfs_common::FileId, u64)> = match inv {
                Ok(Ok(env)) => match env.message {
                    dfs_common::Message::Response(dfs_common::Response::FileInventory { entries }) => entries,
                    _ => { warn!("metadata_healer: unexpected inventory response from {}", node.id); continue; }
                },
                Ok(Err(e)) => { warn!("metadata_healer: inventory fetch from {} failed: {}", node.id, e); continue; }
                Err(_) => { warn!("metadata_healer: inventory fetch from {} timed out", node.id); continue; }
            };
            let follower_map: std::collections::HashMap<dfs_common::FileId, u64> =
                follower_inv.into_iter().collect();

            // Find files where follower is behind or missing.
            let behind: Vec<dfs_common::FileId> = my_inventory.iter()
                .filter_map(|(id, &leader_seq)| {
                    match follower_map.get(id) {
                        None => Some(*id),
                        Some(&follower_seq) if follower_seq < leader_seq => Some(*id),
                        _ => None,
                    }
                })
                .collect();

            if behind.is_empty() { continue; }
            info!("metadata_healer: {} files to push to {}", behind.len(), node.id);

            for chunk in behind.chunks(100) {
                // Fetch full metadata from local sled.
                let batch_result = tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    {
                        let meta = self.metadata.clone();
                        let ids = chunk.to_vec();
                        tokio::task::spawn_blocking(move || {
                            ids.iter()
                                .filter_map(|id| meta.get_file(id).ok().flatten())
                                .collect::<Vec<_>>()
                        })
                    },
                ).await;
                let mut batch: Vec<dfs_common::FileMetadata> = match batch_result {
                    Ok(Ok(v)) => v,
                    _ => { warn!("metadata_healer: sled fetch failed for chunk"); continue; }
                };

                // Override chunk_locations with authoritative chunk_map entries.
                for meta in batch.iter_mut() {
                    if let Some(map_entry) = self.chunk_map.get(&meta.id) {
                        meta.chunk_locations = map_entry.value().0.clone();
                    }
                }

                let req = dfs_common::Request::ReplicateMetadataBatch { items: batch };
                let client = self.client.clone();
                let addr = node.addr;
                let sem = self.broadcast_semaphore.clone();
                tokio::spawn(async move {
                    let _permit = sem.acquire().await.ok();
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        client.send_message(addr, dfs_common::Message::Request(req)),
                    ).await;
                });
            }
        }
    }

    /// Periodic long-term reconciliation loop: every 5 minutes the leader collects
    /// its authoritative file inventory and sends ReconcileMetadata to each follower,
    /// waiting for each follower to ack before moving to the next.
    /// This is NOT fire-and-forget — failures are logged and retried next cycle.
    pub fn start_periodic_reconciliation_loop(self: Arc<Self>) {
        let server = self.clone();
        tokio::spawn(async move {
            const RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);
            let node_notify = server.cluster.node_recovered_notify.clone();
            // Stagger first run by 60s to avoid reconcile storm at cluster startup.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            loop {
                // Wake on either the periodic timer or a node recovery event.
                // Node recovery is the primary trigger for ghost-file resurrection:
                // a node that was offline during a delete still has the file in its DB,
                // and reconciliation is the only mechanism that purges it.
                tokio::select! {
                    _ = tokio::time::sleep(RECONCILE_INTERVAL) => {}
                    _ = node_notify.notified() => {
                        // Brief delay so the recovering node finishes its handshake
                        // and is marked Online before we send the ReconcileMetadata.
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }

                if !server.cluster.is_leader().await {
                    continue;
                }

                info!("[META RECONCILE] starting periodic reconciliation");

                // Collect authoritative file ID set from the leader's sled store.
                let metadata_ref = server.metadata.clone();
                let live_ids: Vec<dfs_common::FileId> = match tokio::task::spawn_blocking(move || {
                    let mut ids = Vec::new();
                    let _ = metadata_ref.scan_files(|file| {
                        ids.push(file.id);
                        Ok(())
                    });
                    ids
                }).await {
                    Ok(ids) => ids,
                    Err(e) => {
                        warn!("[META RECONCILE] failed to collect live IDs: {}", e);
                        continue;
                    }
                };

                info!("[META RECONCILE] {} live file IDs — sending to followers", live_ids.len());

                let nodes = server.cluster.get_all_nodes().await;
                let local_id = server.cluster.local_node_id();
                let mut any_failed = false;

                for node in &nodes {
                    if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                        continue;
                    }
                    let req = dfs_common::Request::ReconcileMetadata {
                        live_file_ids: live_ids.clone(),
                    };
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        server.client.send_message(node.addr, dfs_common::Message::Request(req)),
                    ).await {
                        Ok(Ok(_)) => {
                            info!("[META RECONCILE] node {} acked reconciliation", node.id);
                        }
                        Ok(Err(e)) => {
                            warn!("[META RECONCILE] node {} failed reconciliation: {}", node.id, e);
                            any_failed = true;
                        }
                        Err(_) => {
                            warn!("[META RECONCILE] node {} timed out during reconciliation", node.id);
                            any_failed = true;
                        }
                    }
                }

                if any_failed {
                    warn!("[META RECONCILE] reconciliation completed with failures — will retry next cycle");
                } else {
                    info!("[META RECONCILE] reconciliation complete");
                }
            }
        });
    }

    /// Handle put file metadata request.
    ///
    /// If this node is not the leader, return NotLeader so the client can redirect.
    /// The leader stores locally, enqueues for followers, and returns Ok.
    /// A non-leader that receives a direct write (e.g. quorum replica) stores locally
    /// only — the leader will disseminate to the remaining followers.
    async fn handle_put_file_metadata(&self, metadata: FileMetadata) -> Response {
        info!(
            "[META SERVER] put path={} id={} seq={} size={} is_leader={}",
            metadata.path, metadata.id, metadata.write_seq, metadata.size,
            self.cluster.is_leader().await
        );

        let is_leader = self.cluster.is_leader().await;

        if !is_leader {
            // Return the leader address so the client can redirect immediately.
            let leader_addr = self.cluster.get_leader_addr().await;
            return Response::NotLeader { leader_addr };
        }

        // Tombstone check: reject puts for recently-deleted files to prevent in-flight
        // seq=0 creates (from open() before write) from resurrecting deleted metadata.
        // TOMBSTONE_TTL is 30s — long enough for all in-flight disseminations to drain.
        const TOMBSTONE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
        if let Some(entry) = self.delete_tombstones.get(&metadata.id) {
            if entry.value().elapsed() < TOMBSTONE_TTL {
                info!(
                    "[META SERVER] tombstone-reject path={} id={} seq={} (deleted {:.1}s ago)",
                    metadata.path, metadata.id, metadata.write_seq,
                    entry.value().elapsed().as_secs_f64()
                );
                // Return Ok so the client doesn't retry — the file is intentionally gone.
                return Response::Ok { data: None };
            } else {
                // Tombstone expired — remove it and allow the write.
                drop(entry);
                self.delete_tombstones.remove(&metadata.id);
            }
        }

        // Update in-memory state immediately and respond to the client.
        // The sled write is handed to a dedicated single-threaded worker so it
        // never blocks the async runtime — concurrent puts used to cause a futex
        // pile-up as spawn_blocking threads all contended on sled's internal lock.
        //
        // Same stale-broadcast guard as handle_replicate_metadata: a concurrent release
        // may commit pre-patch state here before the patch's ReplicateChunkLocation
        // arrives to correct it. If our chunk_map already holds a freshly-patched
        // location, keep it — don't let the stale put regress it.
        //
        // Ordering uses write_seq (clock-agnostic) first, then written_at as a
        // secondary guard within the same session.
        //
        // write_seq is monotonically increasing per-file, managed by the client
        // (seeded from the server on open). If incoming.write_seq > stored_write_seq,
        // this is from a newer session — bypass the per-chunk guard entirely and
        // accept the incoming metadata as authoritative. This correctly handles
        // cross-session fresh overwrites where timestamps are unreliable (different
        // clocks, or T_now from a previous session stored in sled beats None/0).
        // Track the highest write_seq seen for session management.
        let stored_write_seq = self.file_write_seqs.get(&metadata.id).map(|v| *v).unwrap_or(0);
        let incoming_write_seq = metadata.write_seq;
        if incoming_write_seq > stored_write_seq {
            self.file_write_seqs.insert(metadata.id, incoming_write_seq);
        }
        // The leader's chunk_map is updated by ReplicateChunkLocation (RCL) after each
        // confirmed write. When RCL succeeds, chunk_map is authoritative and newer than
        // the client's metadata_cache snapshot. But if all 4 RCL attempts fail silently,
        // chunk_map retains the OLD chunk_id while the client's metadata holds the NEW one.
        // Blindly preferring chunk_map in that case would commit the stale chunk_id and
        // cause subsequent reads to return old data (observed as ext4 inode refcount errors
        // on VM disk images).
        //
        // Rule: prefer chunk_map only when it's provably newer — same written_at comparison
        // used by chunk_map_update's GHOST-reversion guard. Fresh writes (written_at=None)
        // from the client always win; a timestamped server entry only wins if its timestamp
        // is strictly greater than the client's.
        let metadata = {
            let mut m = metadata;
            // A regular metadata update (write/setattr/release) must never move a
            // file's path — only RenameFile does that. This request may have been
            // built before a concurrent rename completed, so it can carry the old
            // path. Snap to the current canonical path so it can't resurrect the
            // old path index entry or strand its fields (e.g. modified_at) under
            // a stale path key that get_file_by_path will never see again.
            if let Ok(Some(existing)) = self.metadata.get_file(&m.id) {
                if existing.path != m.path {
                    m.path = existing.path;
                }
            }
            if let Some(map_entry) = self.chunk_map.get(&m.id) {
                let (map_locs, _) = map_entry.value();
                for loc in m.chunk_locations.iter_mut() {
                    if let Some(file_offset) = loc.file_offset {
                        if let Some(map_loc) = map_locs.iter().find(|l| l.file_offset == Some(file_offset)) {
                            let should_use_server = if let Some(client_ts) = loc.written_at {
                                let server_ts = map_loc.written_at.unwrap_or(0);
                                server_ts > client_ts
                            } else {
                                // No timestamp: use client_write_seq to distinguish a stale
                                // replica broadcast (cws=None, server has cws=Some) from a
                                // genuine fresh write (cws=Some or neither has a seq).
                                match (loc.client_write_seq, map_loc.client_write_seq) {
                                    (None, Some(_)) => true,          // server has seq, incoming doesn't → stale
                                    (Some(inc), Some(ext)) => ext > inc, // server has higher seq → prefer server
                                    _ => false,                        // incoming has seq or neither has one → accept
                                }
                            };
                            if should_use_server {
                                *loc = map_loc.clone();
                            }
                        }
                    }
                }
                // Union: chunk_map grows incrementally as each chunk's RCL lands
                // (chunk_map_update_location_for_file), but concurrent per-chunk
                // flushes for the same file race to send their own non-cumulative
                // chunk_locations snapshot — a later write_seq can easily carry
                // FEWER entries than an earlier one. Append any chunk_map entry
                // whose file_offset isn't already in the incoming list, so the
                // persisted chunk_locations never regresses below what RCL has
                // already confirmed. Skip when the incoming list is empty — that's
                // an intentional truncate-to-zero, not an incomplete snapshot.
                if !m.chunk_locations.is_empty() {
                    for map_loc in map_locs.iter() {
                        if let Some(file_offset) = map_loc.file_offset {
                            if !m.chunk_locations.iter().any(|l| l.file_offset == Some(file_offset)) {
                                m.chunk_locations.push(map_loc.clone());
                            }
                        }
                    }
                    m.chunk_locations.sort_by_key(|l| l.file_offset.unwrap_or(u64::MAX));
                }
            }
            m
        };
        self.chunk_map_update(&metadata).await;
        self.record_recent_write(metadata.clone());
        // Mark file dirty for the metadata healer (drains every 5s).
        // DashMap deduplicates by FileId — write storms produce one healer push per file.
        if metadata.write_seq > 0 || !metadata.chunk_locations.is_empty() {
            self.pending_broadcasts.insert(metadata.id, metadata.clone());
        }
        self.enqueue_metadata_for_followers(&metadata).await;

        // Seq=0 creates (initial open(), no data yet) must be committed to sled
        // synchronously before we respond.  If we return first and a delete arrives
        // before the sled_write_tx worker commits, get_file_by_path returns None,
        // the delete returns NotFound without setting a tombstone, and the worker
        // later resurrects the file.  Seq>0 writes are safe: sled already has the
        // file record from the seq=0 commit, so the delete can always find it.
        if metadata.write_seq == 0 && metadata.chunk_locations.is_empty() {
            let meta_clone = metadata.clone();
            let meta_store = self.metadata.clone();
            let _ = tokio::task::spawn_blocking(move || meta_store.put_file(&meta_clone)).await;
        } else {
            if let Some(tx) = self.sled_write_tx.lock().unwrap().as_ref() {
                let _ = tx.send(metadata);
            }
        }
        Response::Ok { data: None }
    }

    /// Handle GetMetadataSequence — return this node's last received (follower) or
    /// last issued (leader) sequence number so a newly-elected leader can catch up.
    async fn handle_get_metadata_sequence(&self) -> Response {
        let seq = if self.cluster.is_leader().await {
            self.metadata.current_meta_sequence().unwrap_or(0)
        } else {
            self.metadata.get_follower_sequence().unwrap_or(0)
        };
        Response::MetadataSequence { sequence: seq }
    }

    /// Handle DisseminateMetadata — leader delivers a batch to this follower.
    /// Stores each item locally and records the sequence number.
    /// If any item is dropped as stale (follower has a newer version), the follower
    /// sends its newer version back to the leader to converge the cluster.
    async fn handle_disseminate_metadata(&self, items: Vec<FileMetadata>, up_to_sequence: u64) -> Response {
        debug!("Handling disseminate metadata: {} items up to seq={}", items.len(), up_to_sequence);

        // Pre-filter tombstoned items on the async thread (DashMap lookup is cheap).
        let tombstones = self.delete_tombstones.clone();
        let live_items_raw: Vec<FileMetadata> = items.into_iter().filter(|m| {
            if tombstones.contains_key(&m.id) {
                debug!("disseminate: tombstone-reject path={} id={}", m.path, m.id);
                false
            } else {
                true
            }
        }).collect();

        // Reconcile chunk_locations against the in-memory chunk_map before storing.
        // The chunk_map reflects live ReplicateChunkLocation updates (write_seq-free),
        // which may be newer than what the leader queued. If the chunk_map has a
        // different chunk_id for a given (file_id, file_offset), use it — this prevents
        // DisseminateMetadata from overwriting a newer chunk_id with a stale one and
        // causing perpetual ChunkStale loops on non-replica nodes.
        const CHUNK_SIZE_RECONCILE: u64 = 4 * 1024 * 1024;
        let live_items: Vec<FileMetadata> = live_items_raw.into_iter().map(|mut m| {
            if let Some(map_entry) = self.chunk_map.get(&m.id) {
                let (map_locs, _) = map_entry.value();
                for loc in m.chunk_locations.iter_mut() {
                    if let Some(file_offset) = loc.file_offset {
                        if let Some(map_loc) = map_locs.iter().find(|l| l.file_offset == Some(file_offset)) {
                            if map_loc.chunk_id != loc.chunk_id {
                                let chunk_idx = file_offset / CHUNK_SIZE_RECONCILE;
                                debug!("disseminate: reconcile file {} chunk {} {} -> {} (chunk_map newer)",
                                    m.id, chunk_idx, loc.chunk_id, map_loc.chunk_id);
                                *loc = map_loc.clone();
                            }
                        }
                    }
                }
            }
            m
        }).collect();

        // Run all sled writes in spawn_blocking so we never block the async runtime.
        // Under a write storm the 5000-item batch would otherwise stall the follower.
        let meta_store = self.metadata.clone();
        let block_result = tokio::task::spawn_blocking(move || {
            let mut stored: Vec<FileMetadata> = Vec::new();
            let mut corrections: Vec<FileMetadata> = Vec::new();
            let mut failed = 0usize;
            for metadata in &live_items {
                match meta_store.put_file(metadata) {
                    Ok(crate::metadata::PutFileResult::Stored) => {
                        stored.push(metadata.clone());
                    }
                    Ok(crate::metadata::PutFileResult::Stale(newer)) => {
                        debug!(
                            "disseminate: follower has newer write_seq={} for {}, will correct leader",
                            newer.write_seq, newer.path
                        );
                        corrections.push(newer);
                    }
                    Err(e) => {
                        warn!("disseminate: failed to store '{}': {}", metadata.path, e);
                        failed += 1;
                    }
                }
            }
            // Record follower sequence inside spawn_blocking to keep it off the async thread.
            (stored, corrections, failed)
        }).await;

        let (stored, corrections, failed) = match block_result {
            Ok(v) => v,
            Err(e) => {
                warn!("disseminate: spawn_blocking panicked: {}", e);
                return Response::Error {
                    message: "disseminate: internal error".to_string(),
                    code: ErrorCode::InternalError,
                };
            }
        };

        // Update in-memory chunk map for stored items (cheap, async-safe).
        for metadata in &stored {
            self.chunk_map_update(metadata).await;
        }

        // Record the highest sequence we've received.
        // up_to_sequence=0 is the gossip sentinel — don't overwrite the real sequence.
        if up_to_sequence > 0 {
            if let Err(e) = self.metadata.set_follower_sequence(up_to_sequence) {
                warn!("disseminate: failed to record follower sequence {}: {}", up_to_sequence, e);
            }
        }

        // Send corrections back to leader in the background — don't block the response.
        if !corrections.is_empty() {
            let client = self.network_client();
            let cluster = self.cluster.clone();
            tokio::spawn(async move {
                let leader_addr = match cluster.get_leader_addr().await {
                    Some(addr) => addr,
                    None => {
                        warn!("disseminate correction: no leader addr known, skipping {} corrections",
                              corrections.len());
                        return;
                    }
                };
                for newer in corrections {
                    let req = dfs_common::Request::PutFileMetadata { metadata: newer.clone() };
                    match client.send_message(leader_addr, dfs_common::Message::Request(req)).await {
                        Ok(_) => debug!("disseminate correction: sent write_seq={} for {} to leader",
                                        newer.write_seq, newer.path),
                        Err(e) => warn!("disseminate correction: failed to send {} to leader: {}",
                                        newer.path, e),
                    }
                }
            });
        }

        if failed > 0 {
            Response::Error {
                message: format!("disseminate: {} items failed to store", failed),
                code: ErrorCode::InternalError,
            }
        } else {
            Response::Ok { data: None }
        }
    }

    /// Return a compact file inventory: Vec<(FileId, modified_at)>.
    async fn handle_get_file_inventory(&self) -> Response {
        let metadata = self.metadata.clone();
        match tokio::task::spawn_blocking(move || metadata.get_file_inventory()).await {
            Ok(Ok(entries)) => Response::FileInventory { entries },
            Ok(Err(e)) => {
                warn!("get_file_inventory failed: {}", e);
                Response::Error { message: e.to_string(), code: ErrorCode::InternalError }
            }
            Err(e) => Response::Error { message: e.to_string(), code: ErrorCode::InternalError },
        }
    }

    /// Fetch full metadata for a batch of file IDs.
    async fn handle_get_file_metadata_batch(&self, file_ids: Vec<FileId>) -> Response {
        let metadata = self.metadata.clone();
        match tokio::task::spawn_blocking(move || metadata.get_files_batch(&file_ids)).await {
            Ok(Ok(items)) => Response::FileMetadataBatch { items },
            Ok(Err(e)) => {
                warn!("get_file_metadata_batch failed: {}", e);
                Response::Error { message: e.to_string(), code: ErrorCode::InternalError }
            }
            Err(e) => Response::Error { message: e.to_string(), code: ErrorCode::InternalError },
        }
    }

    /// Handle append file request.
    ///
    /// The server reads the partial last chunk (if the file is not chunk-aligned),
    /// prepends it to the new data, writes complete chunks + a new partial tail,
    /// updates FileMetadata atomically, and returns the updated metadata.
    ///
    /// `expected_offset` is a CAS guard: if file.size != expected_offset the server
    /// returns OffsetMismatch so the client can re-fetch and retry.
    async fn handle_append_file(
        &self,
        file_id: dfs_common::FileId,
        new_data: Vec<u8>,
        expected_offset: u64,
    ) -> Response {
        info!("AppendFile: file_id={} expected_offset={} data_len={}", file_id, expected_offset, new_data.len());

        // --- Step 1: Fetch current metadata ---
        let mut metadata = match self.metadata.get_file(&file_id) {
            Ok(Some(m)) => m,
            Ok(None) => return Response::Error {
                message: format!("File not found: {}", file_id),
                code: ErrorCode::NotFound,
            },
            Err(e) => return Response::Error {
                message: format!("Failed to read metadata: {}", e),
                code: ErrorCode::InternalError,
            },
        };

        // --- Step 2: CAS guard ---
        if metadata.size != expected_offset {
            return Response::Error {
                message: format!("Offset mismatch: expected {} but file is {} bytes", expected_offset, metadata.size),
                code: ErrorCode::OffsetMismatch,
            };
        }

        // --- Step 3: Read partial tail if file is not chunk-aligned ---
        let chunk_size = self.chunker.chunk_size() as u64;
        let partial_bytes = metadata.size % chunk_size;

        let (write_data, drop_last_chunk, actual_partial_bytes) = if partial_bytes > 0 {
            // File ends mid-chunk — read back the partial last chunk and prepend it
            let last_loc = match metadata.chunk_locations.last() {
                Some(loc) => loc.clone(),
                None => {
                    // chunk_locations empty but file has data — try legacy chunks
                    warn!("AppendFile: file {} has size {} but no chunk_locations, falling back", file_id, metadata.size);
                    return Response::Error {
                        message: "File metadata has no chunk locations".to_string(),
                        code: ErrorCode::InternalError,
                    };
                }
            };

            match self.read_chunk(&last_loc.chunk_id).await {
                Ok(tail_data) => {
                    // Use the actual on-disk chunk size as partial_bytes, not the
                    // metadata-derived value.  If the client's background flusher wrote
                    // more data than metadata recorded (async replication lag), the
                    // metadata-derived partial_bytes will be smaller than the real tail,
                    // causing the new size calculation to undercount and corrupting the
                    // file's recorded size on every subsequent AppendFile call.
                    let actual = tail_data.len() as u64;
                    if actual != partial_bytes {
                        info!("AppendFile: tail chunk on disk is {} bytes but metadata says {} (replication lag) — using disk size",
                              actual, partial_bytes);
                    }
                    let mut combined = tail_data;
                    combined.extend_from_slice(&new_data);
                    (combined, true, actual)
                }
                Err(e) => {
                    warn!("AppendFile: failed to read partial tail chunk {}: {}", last_loc.chunk_id, e);
                    return Response::Error {
                        message: format!("Failed to read partial tail chunk: {}", e),
                        code: ErrorCode::IOError,
                    };
                }
            }
        } else {
            (new_data, false, 0u64)
        };

        // --- Step 4+5: Chunk the combined data ---
        let chunks = self.chunker.chunk_data(&write_data, file_id);
        if chunks.is_empty() {
            // Nothing to write — return current metadata unchanged
            let remaining = chunk_size - (metadata.size % chunk_size);
            return Response::AppendFileResult { metadata, remaining_in_chunk: remaining };
        }

        // Base file offset: where the chunk-aligned region starts, using actual disk size
        let base_offset = metadata.size - actual_partial_bytes;

        // --- Step 6: Write each chunk with 2-replica guarantee ---
        let mut new_locations: Vec<ChunkLocation> = Vec::new();
        let mut current_offset = base_offset;

        for (chunk_id, chunk_data) in &chunks {
            let target_nodes = self.cluster
                .get_nodes_with_capacity_awareness(chunk_id, self.replication_factor)
                .await;

            if target_nodes.is_empty() {
                return Response::Error {
                    message: "No nodes available for chunk replication".to_string(),
                    code: ErrorCode::IOError,
                };
            }

            let immediate_replicas = if self.replication_factor >= 3 { 2 } else { self.replication_factor };

            // Fire all replica writes in parallel — same approach as the original client-side
            // dual-write. Both the local write and the remote ReplicateChunk are spawned
            // simultaneously; we wait for all quorum tasks together before ACKing.
            let quorum_nodes: Vec<dfs_common::NodeId> = target_nodes.iter()
                .take(immediate_replicas)
                .copied()
                .collect();

            let mut write_tasks = Vec::new();
            for node_id in &quorum_nodes {
                let node_id = *node_id;
                let chunk_id = *chunk_id;
                let chunk_data = chunk_data.clone();
                let storage = self.storage.clone();
                let cluster = self.cluster.clone();
                let client = self.client.clone();
                let local_id = self.cluster.local_node_id();

                write_tasks.push(tokio::spawn(async move {
                    if node_id == local_id {
                        let ok = storage.write_chunk(&chunk_id, &chunk_data).is_ok();
                        (node_id, ok)
                    } else {
                        let ok = match cluster.get_node(&node_id).await {
                            Some(node_info) => {
                                let request = Request::ReplicateChunk {
                                    chunk_id,
                                    data: chunk_data,
                                    checksum: chunk_id.hash,
                                    written_at: None,
                                };
                                matches!(
                                    client.send_message(node_info.addr, Message::Request(request)).await,
                                    Ok(dfs_common::protocol::MessageEnvelope {
                                        message: Message::Response(Response::Ok { .. }), ..
                                    })
                                )
                            }
                            None => false,
                        };
                        (node_id, ok)
                    }
                }));
            }

            let mut successful_nodes: Vec<dfs_common::NodeId> = Vec::new();
            for task in write_tasks {
                if let Ok((node_id, true)) = task.await {
                    successful_nodes.push(node_id);
                }
            }

            if successful_nodes.len() < immediate_replicas {
                return Response::Error {
                    message: format!("Failed to achieve quorum for chunk {} ({}/{})",
                                     chunk_id, successful_nodes.len(), immediate_replicas),
                    code: ErrorCode::IOError,
                };
            }

            let chunk_size_bytes = chunk_data.len();
            let location = ChunkLocation {
                chunk_id: *chunk_id,
                nodes: successful_nodes.clone(),
                size: chunk_size_bytes,
                checksum: chunk_id.hash,
                file_offset: Some(current_offset),
                written_at: None,
                client_write_seq: None,
                file_id: Some(file_id),
            };

            // Persist chunk location locally
            let _ = self.metadata.put_chunk_location(&location);

            // Broadcast chunk location to remaining nodes fire-and-forget
            {
                let all_nodes = self.cluster.get_all_nodes().await;
                let local_id = self.cluster.local_node_id();
                for node in all_nodes {
                    if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                        continue;
                    }
                    let client = self.client.clone();
                    let loc = location.clone();
                    tokio::spawn(async move {
                        let req = Request::ReplicateChunkLocation { location: loc, file_id: None };
                        let _ = client.send_message(node.addr, Message::Request(req)).await;
                    });
                }
            }

            current_offset += chunk_size_bytes as u64;
            new_locations.push(location);
        }

        // --- Step 7: Splice metadata ---
        if drop_last_chunk {
            metadata.chunk_locations.pop();
        }
        for loc in &new_locations {
            metadata.chunk_locations.push(loc.clone());
        }

        // --- Step 8: Update metadata size and timestamp ---
        // Use actual_partial_bytes (from disk) not partial_bytes (from metadata) so
        // the recorded size matches what's actually on disk.
        metadata.size = expected_offset + (write_data.len() as u64 - actual_partial_bytes);
        metadata.modified_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // --- Step 9: Persist metadata locally ---
        if let Err(e) = self.metadata.put_file(&metadata) {
            return Response::Error {
                message: format!("Failed to persist metadata: {}", e),
                code: ErrorCode::InternalError,
            };
        }

        // Update in-memory chunk map
        self.chunk_map_update(&metadata).await;

        // --- Step 10: Enqueue metadata for follower dissemination (leader-only). ---
        self.enqueue_metadata_for_followers(&metadata).await;

        // Tell the client how many bytes remain before this chunk seals.
        // When remaining_in_chunk == 0 the chunk is exactly full and the client
        // should rotate to a different primary for the next call.
        let remaining_in_chunk = {
            let partial = metadata.size % chunk_size;
            if partial == 0 { 0 } else { chunk_size - partial }
        };

        info!("AppendFile: complete for file_id={}, new size={}, remaining_in_chunk={}",
              file_id, metadata.size, remaining_in_chunk);
        Response::AppendFileResult { metadata, remaining_in_chunk }
    }

    /// Handle list directory request
    async fn handle_list_directory(&self, path: String) -> Response {
        debug!("Handling list directory: {}", path);

        // Offload synchronous RocksDB scan to blocking thread pool so we don't
        // starve the async executor (same pattern as heal scan).
        let metadata = self.metadata.clone();
        let result = tokio::task::spawn_blocking(move || metadata.list_directory(&path))
            .await;

        match result {
            Ok(Ok(entries)) => Response::DirectoryListing { entries },
            Ok(Err(e)) => {
                warn!("Failed to list directory: {}", e);
                Response::Error {
                    message: format!("Failed to list directory: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
            Err(e) => {
                warn!("spawn_blocking panicked in list_directory: {}", e);
                Response::Error {
                    message: "Internal error listing directory".to_string(),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle write file request (client writes entire file)
    async fn handle_write_file(&self, data: Vec<u8>, file_id: dfs_common::FileId) -> Response {
        debug!("Handling write file: {} bytes", data.len());

        match self.write_data(&data, file_id).await {
            Ok(chunk_ids_with_sizes_and_nodes) => {
                let chunk_ids: Vec<ChunkId> = chunk_ids_with_sizes_and_nodes.iter().map(|(id, _, _)| *id).collect();
                let chunk_sizes: Vec<u64> = chunk_ids_with_sizes_and_nodes.iter().map(|(_, size, _)| *size).collect();
                let replica_nodes_per_chunk: Vec<Vec<dfs_common::NodeId>> = chunk_ids_with_sizes_and_nodes.iter().map(|(_, _, nodes)| nodes.clone()).collect();
                Response::ChunkIds { chunk_ids, chunk_sizes, replica_nodes_per_chunk }
            }
            Err(e) => {
                warn!("Failed to write file: {}", e);
                Response::Error {
                    message: format!("Failed to write file: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle write file request (local only, no replication)
    /// Used for optimized RF=3+ writes where client sends to 2 servers in parallel
    async fn handle_write_file_local_only(&self, data: Vec<u8>, file_offset: u64, file_id: dfs_common::FileId) -> Response {
        debug!("Handling write file local only: {} bytes at offset {}", data.len(), file_offset);

        let local_node_id = self.cluster.local_node_id();
        match self.write_data_local_only_at(&data, file_offset, file_id).await {
            Ok(chunk_ids_with_sizes) => {
                let chunk_ids: Vec<ChunkId> = chunk_ids_with_sizes.iter().map(|(id, _)| *id).collect();
                let chunk_sizes: Vec<u64> = chunk_ids_with_sizes.iter().map(|(_, size)| *size).collect();
                // Local-only: this node holds the chunk (caller sends to 2 nodes in parallel)
                let replica_nodes_per_chunk: Vec<Vec<dfs_common::NodeId>> = chunk_ids.iter().map(|_| vec![local_node_id]).collect();
                info!("Wrote {} chunks locally (no replication)", chunk_ids.len());
                Response::ChunkIds { chunk_ids, chunk_sizes, replica_nodes_per_chunk }
            }
            Err(e) => {
                warn!("Failed to write file locally: {}", e);
                Response::Error {
                    message: format!("Failed to write file: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Apply a patch to an existing chunk without transferring the full chunk over the network.
    /// Reads the chunk locally, splices in patch bytes, recomputes position-aware Blake3,
    /// then atomically renames the chunk file to its new hash path and removes the old path.
    /// No data bytes are written to disk beyond the temp file — this is a read+patch+rename.
    async fn handle_patch_chunk(
        &self,
        chunk_id: ChunkId,
        file_id: dfs_common::FileId,
        chunk_idx: Option<u64>,
        chunk_file_offset: u64,
        intra_offset: usize,
        patch_data: Vec<u8>,
    ) -> Response {
        // Validate chunk_id against local chunk map when chunk_idx is provided.
        // If our record for (file_id, chunk_idx) differs, the client has a stale view —
        // return ChunkStale so the client can retry with the correct chunk_id.
        if let Some(cidx) = chunk_idx {
            if let Some(entry) = self.chunk_map.get(&file_id) {
                let (locations, _) = entry.value();
                const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
                if let Some(loc) = locations.iter().find(|l| l.file_offset.map(|o| o / CHUNK_SIZE) == Some(cidx)) {
                    if loc.chunk_id != chunk_id {
                        info!("PatchChunk: stale chunk_id from client — file {} chunk {} client={} server={}",
                            file_id, cidx, chunk_id, loc.chunk_id);
                        return Response::ChunkStale {
                            current_chunk_id: loc.chunk_id,
                            current_nodes: loc.nodes.clone(),
                        };
                    }
                }
            }
        }

        if let Some(healing) = self.healing.read().await.as_ref() {
            healing.evict_from_pending(&chunk_id).await;
        }

        let old_path = self.storage.get_chunk_path(&chunk_id);
        let storage = self.storage.clone();
        let metadata = self.metadata.clone();

        let result = tokio::task::spawn_blocking(move || {
            use std::fs;

            if !old_path.exists() {
                return Err(("Chunk not found".to_string(), ErrorCode::NotFound));
            }

            // Read the base chunk into memory and apply the patch there — never
            // mutate old_path in place. See handle_multi_patch for why: chunk_id is
            // content-addressed, so the same chunk_id can be the current base for
            // multiple (file, chunk_idx) slots at once.
            let mut buf = fs::read(&old_path)
                .map_err(|e| (format!("Failed to read chunk: {}", e), ErrorCode::InternalError))?;

            let patch_end = intra_offset + patch_data.len();
            if patch_end > buf.len() {
                buf.resize(patch_end, 0);
            }
            buf[intra_offset..patch_end].copy_from_slice(&patch_data);

            let final_size = buf.len();

            let new_chunk_id = ChunkId::from_hash(
                dfs_common::compute_chunk_hash_at(&buf, chunk_file_offset, file_id)
            );

            if new_chunk_id != chunk_id {
                let new_path = storage.get_chunk_path(&new_chunk_id);
                if let Some(parent) = new_path.parent() {
                    if let Err(e) = fs::create_dir_all(parent) {
                        return Err((format!("Failed to create chunk directory: {}", e), ErrorCode::InternalError));
                    }
                }
                if !new_path.exists() {
                    let tmp_path = new_path.with_extension(format!("tmp.{}.{:?}",
                        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos(),
                        std::thread::current().id()));
                    fs::write(&tmp_path, &buf)
                        .map_err(|e| (format!("Failed to write patched chunk: {}", e), ErrorCode::InternalError))?;
                    fs::File::open(&tmp_path)
                        .and_then(|f| f.sync_data())
                        .map_err(|e| (format!("Failed to sync patched chunk: {}", e), ErrorCode::InternalError))?;
                    fs::rename(&tmp_path, &new_path)
                        .map_err(|e| (format!("Failed to rename patched chunk: {}", e), ErrorCode::InternalError))?;
                }
                // Same as handle_multi_patch: pre-register new_chunk_id, and
                // deliberately leave the old chunk_id's routing entry & on-disk file
                // alone for the deep orphan-purge sweep — see comment there.
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                if let Ok(Some(old_loc)) = metadata.get_chunk_location(&chunk_id) {
                    let new_loc = ChunkLocation {
                        chunk_id: new_chunk_id,
                        nodes: old_loc.nodes,
                        size: final_size,
                        checksum: new_chunk_id.hash,
                        file_offset: old_loc.file_offset,
                        written_at: Some(now_secs),
                        client_write_seq: None,
                        file_id: Some(file_id),
                    };
                    if let Err(e) = metadata.put_chunk_location(&new_loc) {
                        warn!("PatchChunk: failed to pre-register {} in sled: {}", new_chunk_id, e);
                    }
                }
                storage.invalidate_cache(&new_chunk_id);
            }

            Ok((new_chunk_id, final_size, patch_data.len()))
        }).await;

        match result {
            Ok(Ok((new_chunk_id, final_size, patch_len))) => {
                info!("PatchChunk: {} -> {} ({} bytes at intra_offset={})", chunk_id, new_chunk_id, patch_len, intra_offset);

                // Update in-memory chunk_map. Sled was already updated inside
                // spawn_blocking before the rename — see comment there.
                if new_chunk_id != chunk_id {
                    const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
                    if let Some(cidx) = chunk_idx {
                        if let Some(mut entry) = self.chunk_map.get_mut(&file_id) {
                            let (locations, _) = entry.value_mut();
                            if let Some(loc) = locations.iter_mut().find(|l| {
                                l.file_offset.map(|o| o / CHUNK_SIZE) == Some(cidx)
                                    && l.chunk_id == chunk_id
                            }) {
                                loc.chunk_id = new_chunk_id;
                                loc.checksum = new_chunk_id.hash;
                                loc.size = final_size;
                            }
                        }
                    }
                }

                Response::PatchChunkResult { new_chunk_id, size: final_size }
            }
            Ok(Err((msg, code))) => {
                warn!("PatchChunk {}: {}", chunk_id, msg);
                Response::Error { message: msg, code }
            }
            Err(e) => {
                warn!("PatchChunk {}: spawn_blocking panicked: {}", chunk_id, e);
                Response::Error { message: "Internal error".to_string(), code: ErrorCode::InternalError }
            }
        }
    }

    async fn handle_multi_patch(
        &self,
        chunk_id: ChunkId,
        file_id: dfs_common::FileId,
        chunk_idx: Option<u64>,
        chunk_file_offset: u64,
        patches: Vec<(usize, Vec<u8>)>,
        _expected_new_chunk_id: Option<dfs_common::ChunkId>,
        client_write_seq: Option<u64>,
    ) -> Response {
        // Serialize all patches to the same (file_id, chunk_idx). Without this lock,
        // two concurrent patches against the same base (e.g. duplicate flush retries)
        // could both succeed independently and then race on the final chunk_map
        // update below, leaving (file_id, chunk_idx) pointing at whichever patch's
        // result lost the race — even though the OTHER patch's RPC response (and any
        // ReplicateChunkLocation derived from it) already told the client/leader
        // about its own new_chunk_id.
        let _chunk_patch_guard = if let Some(cidx) = chunk_idx {
            let lock = self.chunk_patch_locks
                .entry((file_id, cidx))
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone();
            Some(lock.lock_owned().await)
        } else {
            None
        };

        if let Some(cidx) = chunk_idx {
            if let Some(entry) = self.chunk_map.get(&file_id) {
                const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
                let (locations, _) = entry.value();
                if let Some(loc) = locations.iter().find(|l| l.file_offset.map(|o| o / CHUNK_SIZE) == Some(cidx)) {
                    if loc.chunk_id != chunk_id {
                        // Before returning ChunkStale, verify the "current" chunk exists on disk.
                        // A stale broadcast can revert chunk_map to an old hash whose file was
                        // already renamed away by a subsequent patch. Returning ChunkStale with
                        // that ghost hash sends the client into an infinite retry loop: the
                        // "corrected" hash is unreachable, so every retry fails. Instead, let
                        // the request proceed — spawn_blocking will open the file by the client's
                        // chunk_id; if it exists we patch correctly, if not it returns NotFound.
                        let current_path = self.storage.get_chunk_path(&loc.chunk_id);
                        if !current_path.exists() {
                            warn!("[GHOST-stale-check] chunk_map has {} for file {} chunk {} (written_at={:?}) but file NOT on disk — chunk_map is stale/reverted; proceeding with client's {} (file exists={})",
                                loc.chunk_id, file_id, cidx, loc.written_at, chunk_id,
                                self.storage.get_chunk_path(&chunk_id).exists());
                        } else {
                            info!("MultiPatch: stale chunk_id from client — file {} chunk {} client={} server={}",
                                file_id, cidx, chunk_id, loc.chunk_id);
                            return Response::ChunkStale {
                                current_chunk_id: loc.chunk_id,
                                current_nodes: loc.nodes.clone(),
                            };
                        }
                    }
                }
            }
        }

        if let Some(healing) = self.healing.read().await.as_ref() {
            healing.evict_from_pending(&chunk_id).await;
        }

        let old_path = self.storage.get_chunk_path(&chunk_id);
        let storage = self.storage.clone();
        let metadata = self.metadata.clone();

        let result = tokio::task::spawn_blocking(move || {
            use std::fs;

            // If the chunk file doesn't exist on disk, refuse the patch.
            // A ghost chunk_map entry (chunk_id in metadata but no file on disk)
            // can arise when ReplicateChunkLocation updated this node's map but the
            // actual data was written elsewhere. Creating an empty file and patching
            // it would produce corrupt data — return NotFound so the client excludes
            // this node and the healer can copy the real chunk here.
            if !old_path.exists() {
                return Err((format!("Failed to read chunk range: Failed to open chunk file: {:?}", old_path), ErrorCode::NotFound));
            }

            // Read the base chunk into memory and apply patches there — never mutate
            // old_path in place. chunk_id is content-addressed (blake3(offset||data)),
            // so the SAME chunk_id can be the "current" base for multiple (file,
            // chunk_idx) slots at once whenever their content+offset match (e.g.
            // duplicate files). Renaming/deleting the shared base out from under a
            // concurrent patch for another file made that file's MultiPatch fail with
            // "chunk data missing on this node", falling back to a fresh write of only
            // the dirty range and corrupting the un-patched bytes. Chunks are capped
            // at 4MB, so reading the whole base into memory is cheap.
            let mut buf = fs::read(&old_path)
                .map_err(|e| (format!("Failed to read chunk: {}", e), ErrorCode::InternalError))?;

            let needed_len = patches.iter()
                .map(|(off, d)| off + d.len())
                .max()
                .unwrap_or(0)
                .max(buf.len());
            buf.resize(needed_len, 0);

            for (intra_offset, patch_data) in &patches {
                buf[*intra_offset..*intra_offset + patch_data.len()].copy_from_slice(patch_data);
            }

            let final_size = buf.len();

            // Always compute the actual hash from the patched buffer so the leader
            // and client get ground truth. Trusting the client's pre-computed expected
            // hash is wrong: if the server's on-disk base differed (stale client
            // cache, healer update) the file would be named with the wrong hash,
            // silently corrupting the chunk.
            let new_chunk_id = ChunkId::from_hash(
                dfs_common::compute_chunk_hash_at(&buf, chunk_file_offset, file_id)
            );

            if new_chunk_id != chunk_id {
                let new_path = storage.get_chunk_path(&new_chunk_id);
                if let Some(parent) = new_path.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| (format!("Failed to create chunk directory: {}", e), ErrorCode::InternalError))?;
                }
                if !new_path.exists() {
                    let tmp_path = new_path.with_extension(format!("tmp.{}.{:?}",
                        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos(),
                        std::thread::current().id()));
                    fs::write(&tmp_path, &buf)
                        .map_err(|e| (format!("Failed to write patched chunk: {}", e), ErrorCode::InternalError))?;
                    fs::File::open(&tmp_path)
                        .and_then(|f| f.sync_data())
                        .map_err(|e| (format!("Failed to sync patched chunk: {}", e), ErrorCode::InternalError))?;
                    fs::rename(&tmp_path, &new_path)
                        .map_err(|e| (format!("Failed to rename patched chunk: {}", e), ErrorCode::InternalError))?;
                }
                // Register new_chunk_id in sled, reusing the old location's node list.
                // The OLD chunk_id's routing entry and on-disk file are deliberately
                // left in place — they may still be the current base for ANOTHER
                // (file, chunk_idx) slot with identical content+offset. The deep
                // orphan-purge sweep reclaims them once live_chunk_ids() (scanned from
                // file metadata) no longer references the old chunk_id, after its
                // grace period.
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                if let Ok(Some(old_loc)) = metadata.get_chunk_location(&chunk_id) {
                    let new_loc = ChunkLocation {
                        chunk_id: new_chunk_id,
                        nodes: old_loc.nodes,
                        size: final_size,
                        checksum: new_chunk_id.hash,
                        file_offset: old_loc.file_offset,
                        written_at: Some(now_secs),
                        client_write_seq: None,
                        file_id: Some(file_id),
                    };
                    if let Err(e) = metadata.put_chunk_location(&new_loc) {
                        warn!("MultiPatch: failed to pre-register {} in sled: {}", new_chunk_id, e);
                    }
                }
                storage.invalidate_cache(&new_chunk_id);
            }

            Ok::<_, (String, ErrorCode)>((new_chunk_id, final_size, patches))
        }).await;

        match result {
            Ok(Ok((new_chunk_id, final_size, patches))) => {
                let patch_summary: Vec<(usize, usize)> = patches.iter()
                    .map(|(off, d)| (*off, off + d.len()))
                    .collect();
                info!("MultiPatch: {} -> {} ({} patches, final size={}): {:?}",
                    chunk_id, new_chunk_id, patches.len(), final_size, patch_summary);

                // Update this node's local chunk_map and sled metadata to reflect the
                // new chunk_id atomically with the patch — before returning the response.
                //
                // Without this, the stale-base check at the top of this function uses the
                // chunk_map, which is only updated via async ReplicateChunkLocations from
                // the leader. That async gap means the next MultiPatch from the client
                // (using new_chunk_id as the base) would immediately see a false stale-base
                // here (chunk_map still says old chunk_id). The client then retries with the
                // old chunk_id, which no longer exists on disk (it was renamed), and the
                // server creates an empty file and patches it — producing corrupt garbage data.
                // Compute patch_ts once — used for both the chunk_map stamp and the
                // response. The client MUST use the returned patch_ts as written_at for
                // the new ChunkLocation so guard comparisons are in server time, not
                // client time. Clock skew (client ahead of server) otherwise defeats
                // the stale-broadcast guard: T0(client) > T1(server) → guard fails.
                let patch_ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                if new_chunk_id != chunk_id {
                    const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
                    if let Some(cidx) = chunk_idx {
                        // Targeted: O(1) lookup by file_id.
                        if let Some(mut entry) = self.chunk_map.get_mut(&file_id) {
                            let (locations, _) = entry.value_mut();
                            if let Some(loc) = locations.iter_mut().find(|l| {
                                l.file_offset.map(|o| o / CHUNK_SIZE) == Some(cidx)
                                    && l.chunk_id == chunk_id
                            }) {
                                loc.chunk_id = new_chunk_id;
                                loc.checksum = new_chunk_id.hash;
                                loc.size = final_size;
                                loc.written_at = Some(patch_ts);
                            }
                        }
                    } else {
                        // No file_id: fall back to scanning all files for the old chunk_id.
                        // Slower but correct — ensures chunk_map is updated even when the
                        // client didn't send file_id (e.g. legacy or fallback patch path).
                        for mut entry in self.chunk_map.iter_mut() {
                            let (locations, _) = entry.value_mut();
                            if let Some(loc) = locations.iter_mut().find(|l| l.chunk_id == chunk_id) {
                                loc.chunk_id = new_chunk_id;
                                loc.checksum = new_chunk_id.hash;
                                loc.size = final_size;
                                loc.written_at = Some(patch_ts);
                                break;
                            }
                        }
                    }
                }

                // Notify the leader of the actual computed hash before returning to the
                // client. Each replica sends its own node_id; the leader's merge logic
                // unions under-RF sets, so two replicas sending {A} and {B} accumulate
                // correctly to {A, B}. Fire-and-forget: if the leader is down the
                // chunk_location_sync loop and healer will reconcile.
                //
                // When this node IS the leader, update the chunk_map directly instead of
                // sending an RCL over the network. Without this, the chunk_map retains the
                // old chunk_id forever (no self-RCL), and handle_put_file_metadata's
                // "prefer chunk_map" merge logic later writes the stale id to sled —
                // causing EIO on cold reads after a client restart.
                if new_chunk_id != chunk_id {
                    let location = ChunkLocation {
                        chunk_id: new_chunk_id,
                        nodes: vec![self.cluster.local_node_id()],
                        size: final_size,
                        checksum: new_chunk_id.hash,
                        file_offset: Some(chunk_file_offset),
                        written_at: Some(patch_ts),
                        client_write_seq,
                        file_id: Some(file_id),
                    };
                    if let Some(leader_addr) = self.cluster.get_leader_addr().await {
                        if leader_addr != self.cluster.local_addr() {
                            let req = Request::ReplicateChunkLocation { location, file_id: Some(file_id) };
                            let client = self.client.clone();
                            tokio::spawn(async move {
                                if let Err(e) = client.send_message(leader_addr, Message::Request(req)).await {
                                    warn!("MultiPatch: failed to notify leader {} of new chunk {}: {}", leader_addr, new_chunk_id, e);
                                }
                            });
                        } else {
                            // We ARE the leader — update chunk_map directly, then sled.
                            // Reuse handle_replicate_chunk_location's merge + sled logic.
                            self.handle_replicate_chunk_location(location, Some(file_id)).await;
                        }
                    }
                }

                Response::MultiPatchResult {
                    new_chunk_id,
                    size: final_size,
                    patch_ts: if new_chunk_id != chunk_id { Some(patch_ts) } else { None },
                }
            }
            Ok(Err((msg, code))) => {
                warn!("MultiPatch {}: {}", chunk_id, msg);
                Response::Error { message: msg, code }
            }
            Err(e) => {
                warn!("MultiPatch {}: spawn_blocking panicked: {}", chunk_id, e);
                Response::Error { message: "Internal error".to_string(), code: ErrorCode::InternalError }
            }
        }
    }

    /// Handle delete file request.
    ///
    /// The client fans out DeleteFile to the leader + 2 other nodes simultaneously
    /// and waits for all 3 acks. Each recipient:
    ///   1. Looks up metadata → gets chunk list
    ///   2. Writes DeleteQueueEntry to sled (crash-safe, before any metadata removal)
    ///   3. Deletes local metadata (file record, path index, chunk locations)
    ///   4. Inserts tombstone + removes from chunk_map
    ///   5. Acks the client
    ///
    /// The leader's drain worker then handles all actual chunk deletion asynchronously.
    async fn handle_delete_file(&self, path: String) -> Response {
        debug!("Handling delete file: {}", path);

        let metadata = match self.metadata.get_file_by_path(&path) {
            Ok(Some(m)) => m,
            Ok(None) => {
                return Response::Error {
                    message: "File not found".to_string(),
                    code: ErrorCode::NotFound,
                };
            }
            Err(e) => {
                warn!("Failed to look up file {}: {}", path, e);
                return Response::Error {
                    message: format!("Failed to delete file: {}", e),
                    code: ErrorCode::InternalError,
                };
            }
        };

        let chunk_ids: Vec<ChunkId> = metadata.chunk_locations.iter()
            .map(|loc| loc.chunk_id)
            .collect();

        // Step 1: tombstone FIRST — matches follower path (handle_delete_metadata).
        // Any sled_write_tx worker that races with the steps below will see the
        // tombstone and discard the stale put_file, closing the resurrection window.
        self.delete_tombstones.insert(metadata.id, std::time::Instant::now());
        self.pending_broadcasts.remove(&metadata.id);

        // Step 2: persist chunk list to delete queue BEFORE removing metadata.
        let entry = dfs_common::DeleteQueueEntry {
            file_id: metadata.id,
            path: path.clone(),
            chunk_ids: chunk_ids.clone(),
        };
        if let Err(e) = self.metadata.enqueue_delete(&entry) {
            warn!("Failed to enqueue delete for {}: {}", path, e);
            return Response::Error {
                message: format!("Failed to enqueue delete: {}", e),
                code: ErrorCode::InternalError,
            };
        }

        // Step 3: remove metadata now that the chunk list is safely queued.
        if let Err(e) = self.metadata.delete_file(&metadata.id) {
            warn!("Failed to delete file metadata for {}: {}", path, e);
            // Queue entry is already written — drain worker will retry.
            // Still return error so client knows metadata removal may have failed.
            return Response::Error {
                message: format!("Failed to delete file: {}", e),
                code: ErrorCode::InternalError,
            };
        }
        if let Err(e) = self.metadata.delete_path_index(&path) {
            warn!("Failed to delete path index for {}: {}", path, e);
        }
        for chunk_id in &chunk_ids {
            if let Err(e) = self.metadata.delete_chunk_location(chunk_id) {
                warn!("Failed to delete chunk location {}: {}", chunk_id, e);
            }
        }

        // Step 4: in-memory chunk_map removal (tombstone already set in step 1).
        self.chunk_map_remove(&metadata.id).await;

        // Notify the drain worker that there's a new entry (leader only acts on it,
        // but the notify is harmless on followers).
        self.delete_drain_notify.notify_one();

        info!("DeleteFile: queued {} chunks for async deletion: {}", chunk_ids.len(), path);
        Response::Ok { data: None }
    }

    /// Handle DeleteChunksBatch — leader sends one of these per peer during drain.
    /// Recipient deletes all listed chunks from local storage + metadata, then acks.
    async fn handle_delete_chunks_batch(
        &self,
        file_id: FileId,
        path: String,
        chunk_ids: Vec<ChunkId>,
    ) -> Response {
        info!("DeleteChunksBatch: {} chunks for {} ({})", chunk_ids.len(), path, file_id);

        // Tombstone so in-flight replicates can't resurrect the file.
        self.delete_tombstones.insert(file_id, std::time::Instant::now());
        self.pending_broadcasts.remove(&file_id);

        // Wipe metadata (idempotent — already gone on quorum nodes).
        let _ = self.metadata.delete_file(&file_id);
        let _ = self.metadata.delete_path_index(&path);
        self.chunk_map_remove(&file_id).await;

        for chunk_id in &chunk_ids {
            self.chunk_tombstones.remove(chunk_id);
            let _ = self.metadata.delete_chunk_location(chunk_id);
            if let Err(e) = self.storage.delete_chunk(chunk_id) {
                // Not present locally — fine, log at debug.
                debug!("DeleteChunksBatch: chunk {} not local: {}", chunk_id, e);
            }
        }

        Response::Ok { data: None }
    }

    /// Handle ClearDeleteQueueEntry — leader broadcasts this after all nodes ack.
    async fn handle_clear_delete_queue_entry(&self, file_id: FileId) -> Response {
        if let Err(e) = self.metadata.dequeue_delete(&file_id) {
            warn!("ClearDeleteQueueEntry: failed to remove {} from queue: {}", file_id, e);
        }
        Response::Ok { data: None }
    }

    /// Handle GetDeleteQueue — leader polls all nodes on startup/election to merge queues.
    async fn handle_get_delete_queue(&self) -> Response {
        match self.metadata.get_all_pending_deletes() {
            Ok(entries) => Response::DeleteQueue { entries },
            Err(e) => {
                warn!("GetDeleteQueue: failed to read delete queue: {}", e);
                Response::Error {
                    message: format!("Failed to read delete queue: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Send a Request to a node and extract the Response from the envelope.
    async fn send_req(&self, addr: std::net::SocketAddr, req: Request) -> Result<Response> {
        let envelope = self.client.send_message(addr, Message::Request(req)).await?;
        match envelope.message {
            Message::Response(r) => Ok(r),
            other => anyhow::bail!("unexpected message type: {:?}", other),
        }
    }

    /// Leader-only background worker: drains the delete queue.
    ///
    /// On becoming leader, polls all nodes for their queues and merges them locally,
    /// then continuously drains: for each queued deletion, sends DeleteChunksBatch to
    /// every node that holds at least one chunk, waits for all acks, then broadcasts
    /// ClearDeleteQueueEntry to all nodes.
    pub fn start_delete_drain_loop(self: Arc<Self>) {
        let server = self.clone();
        tokio::spawn(async move {
            let mut was_leader = false;
            loop {
                // Wait for a notify (new delete enqueued) or periodic poll.
                tokio::select! {
                    _ = server.delete_drain_notify.notified() => {}
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(30)) => {}
                }

                let is_leader = server.cluster.is_leader().await;

                // On leadership acquisition: poll all nodes, merge their queues into ours.
                if is_leader && !was_leader {
                    info!("delete_drain: became leader — merging delete queues from all nodes");
                    let nodes = server.cluster.get_all_nodes().await;
                    let local_id = server.cluster.local_node_id();
                    for node in &nodes {
                        if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                            continue;
                        }
                        match tokio::time::timeout(
                            tokio::time::Duration::from_secs(10),
                            server.send_req(node.addr, Request::GetDeleteQueue),
                        ).await {
                            Ok(Ok(Response::DeleteQueue { entries })) => {
                                let existing = server.metadata.get_all_pending_deletes().unwrap_or_default();
                                for entry in entries {
                                    if !existing.iter().any(|e| e.file_id == entry.file_id) {
                                        if let Err(e) = server.metadata.enqueue_delete(&entry) {
                                            warn!("delete_drain: failed to merge entry from {}: {}", node.id, e);
                                        } else {
                                            info!("delete_drain: merged queued delete for {} from {}", entry.path, node.id);
                                        }
                                    }
                                }
                            }
                            Ok(Ok(_)) => warn!("delete_drain: unexpected response from {} for GetDeleteQueue", node.id),
                            Ok(Err(e)) => warn!("delete_drain: GetDeleteQueue from {} failed: {}", node.id, e),
                            Err(_) => warn!("delete_drain: GetDeleteQueue from {} timed out", node.id),
                        }
                    }
                }

                was_leader = is_leader;
                if !is_leader {
                    continue;
                }

                // Drain all pending entries.
                let entries = match server.metadata.get_all_pending_deletes() {
                    Ok(e) => e,
                    Err(e) => { warn!("delete_drain: failed to read queue: {}", e); continue; }
                };

                if entries.is_empty() {
                    continue;
                }

                info!("delete_drain: {} entries to process", entries.len());

                for entry in entries {
                    server.drain_one_delete(entry).await;
                }
            }
        });
    }

    /// Drain a single delete queue entry: send DeleteChunksBatch to all online nodes,
    /// wait for acks, then clear the entry from all queues.
    ///
    /// We broadcast to ALL online nodes (not just chunk holders) because catchup Phase 2
    /// pushes file metadata to every follower regardless of whether it holds chunks.
    /// A node that has metadata but no chunks would never be targeted by a chunk-holder-
    /// only broadcast, leaving zombie metadata that catchup would later resurrect.
    async fn drain_one_delete(&self, entry: dfs_common::DeleteQueueEntry) {
        let nodes = self.cluster.get_all_nodes().await;
        let local_id = self.cluster.local_node_id();

        // Delete locally first.  If the local metadata delete fails (e.g. disk full /
        // I/O error), bail out immediately rather than propagating the delete to
        // followers.  This prevents the "leader poisons followers" scenario where a
        // full-disk leader silently fails its own delete but still broadcasts
        // DeleteChunksBatch, causing followers to permanently lose the file while the
        // leader retains it.  The drain will retry on the next 30-second cycle.
        if let Err(e) = self.metadata.delete_file(&entry.file_id) {
            warn!("drain_one_delete: local metadata delete failed for {} — will retry: {}", entry.path, e);
            return;
        }
        if let Err(e) = self.metadata.delete_path_index(&entry.path) {
            warn!("drain_one_delete: local path index delete failed for {} — will retry: {}", entry.path, e);
        }
        for chunk_id in &entry.chunk_ids {
            let _ = self.metadata.delete_chunk_location(chunk_id);
            if let Err(e) = self.storage.delete_chunk(chunk_id) {
                debug!("drain_one_delete: local chunk {} not present: {}", chunk_id, e);
            }
        }
        self.chunk_map_remove(&entry.file_id).await;

        // Send DeleteChunksBatch to every online peer (not just chunk holders).
        let mut all_acked = true;
        for node in &nodes {
            if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                continue;
            }

            let req = Request::DeleteChunksBatch {
                file_id: entry.file_id,
                path: entry.path.clone(),
                chunk_ids: entry.chunk_ids.clone(),
            };
            match tokio::time::timeout(
                tokio::time::Duration::from_secs(30),
                self.send_req(node.addr, req),
            ).await {
                Ok(Ok(Response::Ok { .. })) => {
                    debug!("drain_one_delete: node {} acked delete of {}", node.id, entry.path);
                }
                Ok(Ok(_)) => {
                    warn!("drain_one_delete: unexpected response from {} for {}", node.id, entry.path);
                    all_acked = false;
                }
                Ok(Err(e)) => {
                    warn!("drain_one_delete: node {} failed for {}: {}", node.id, entry.path, e);
                    all_acked = false;
                }
                Err(_) => {
                    warn!("drain_one_delete: node {} timed out for {}", node.id, entry.path);
                    all_acked = false;
                }
            }
        }

        if !all_acked {
            // Leave in queue — will retry on next drain cycle.
            info!("drain_one_delete: some nodes failed for {} — will retry", entry.path);
            return;
        }

        // All chunk-holding nodes acked. Clear the queue entry from all nodes.
        info!("drain_one_delete: all nodes acked deletion of {} — clearing queue", entry.path);
        if let Err(e) = self.metadata.dequeue_delete(&entry.file_id) {
            warn!("drain_one_delete: failed to clear local queue entry for {}: {}", entry.path, e);
        }

        // Fire-and-forget ClearDeleteQueueEntry to all peers.
        for node in &nodes {
            if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                continue;
            }
            let req = Request::ClearDeleteQueueEntry { file_id: entry.file_id };
            let client = self.client.clone();
            let addr = node.addr;
            let path_clone = entry.path.clone();
            let node_id = node.id;
            tokio::spawn(async move {
                let msg = Message::Request(req);
                if let Err(e) = client.send_message(addr, msg).await {
                    warn!("drain_one_delete: ClearDeleteQueueEntry to {} failed for {}: {}", node_id, path_clone, e);
                }
            });
        }
    }

    /// Handle get cluster status request
    async fn handle_get_cluster_status(&self) -> Response {
        debug!("Handling get cluster status");

        let nodes = self.cluster.get_all_nodes().await;
        let healthy_nodes = nodes
            .iter()
            .filter(|n| n.status == dfs_common::NodeStatus::Online)
            .count();
        let total_nodes = nodes.len();
        let chunk_size_mb = self.chunker.chunk_size() / (1024 * 1024);
        let leader_node_id = nodes
            .iter()
            .filter(|n| n.status == dfs_common::NodeStatus::Online)
            .map(|n| n.id)
            .min();

        Response::ClusterStatus {
            nodes,
            total_nodes,
            healthy_nodes,
            chunk_size_mb,
            leader_node_id,
            replication_factor: self.replication_factor,
            local_node_id: Some(self.cluster.local_node_id()),
        }
    }

    /// Handle get storage stats request
    async fn handle_get_storage_stats(&self) -> Response {
        debug!("Handling get storage stats");

        const CACHE_TTL_SECS: u64 = 10;

        // Check cache first
        {
            let cache_read = self.storage_stats_cache.read().await;
            if let Some(cached) = cache_read.as_ref() {
                if cached.timestamp.elapsed().as_secs() < CACHE_TTL_SECS {
                    debug!("Storage stats cache HIT (age: {}s)", cached.timestamp.elapsed().as_secs());
                    let nodes_count = self.cluster.get_all_nodes().await.len();
                    let total_size = cached.total_space.saturating_sub(cached.available_space);

                    return Response::StorageStats {
                        total_chunks: cached.total_chunks,
                        total_size,
                        replication_factor: self.replication_factor,
                        nodes_count,
                        total_space: cached.total_space,
                        free_space: cached.free_space,
                        available_space: cached.available_space,
                    };
                }
            }
        }

        // Cache miss - calculate stats
        debug!("Storage stats cache MISS");

        let nodes_count = self.cluster.get_all_nodes().await.len();

        // Get local filesystem statistics and register this node's capacity.
        // This keeps the cluster's per-node capacity map fresh for placement decisions
        // even though we report aggregate (cluster-wide) stats to the client below.
        let (local_total, _local_free, local_available) = match self.storage.get_filesystem_stats() {
            Ok(stats) => stats,
            Err(e) => {
                warn!("Failed to get local storage stats: {}", e);
                return Response::Error {
                    message: format!("Failed to get storage stats: {}", e),
                    code: ErrorCode::InternalError,
                };
            }
        };
        self.cluster.update_node_capacity(
            self.cluster.local_node_id(),
            local_available,
            local_total,
        ).await;

        // Warn when this node's disk is getting dangerously full.
        let local_pct = if local_total > 0 { 100 * (local_total - local_available) / local_total } else { 0 };
        if local_pct >= 95 {
            warn!("DISK CRITICAL: local storage partition is {}% full ({:.1}GB free)", local_pct,
                local_available as f64 / 1_073_741_824.0);
        } else if local_pct >= 85 {
            warn!("DISK WARNING: local storage partition is {}% full ({:.1}GB free)", local_pct,
                local_available as f64 / 1_073_741_824.0);
        } else if local_pct >= 70 {
            info!("Disk usage: local storage partition is {}% full ({:.1}GB free)", local_pct,
                local_available as f64 / 1_073_741_824.0);
        }

        // Return raw local stats. The client queries all N nodes in parallel and
        // aggregates them, then divides by RF once.  Dividing here too would cause
        // double-RF division (total = 5 × local / RF² instead of 5 × local / RF).
        let total_space = local_total;
        let available_space = local_available;
        let free_space = local_available;

        // Calculate total_size as used logical space.
        let total_size = total_space.saturating_sub(available_space);

        // Estimate chunk count from used space (4MB chunks)
        // This avoids expensive list_chunks() call for statfs queries
        const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
        let total_chunks = (total_size / CHUNK_SIZE) as usize;

        // Update cache
        {
            let mut cache_write = self.storage_stats_cache.write().await;
            *cache_write = Some(StorageStatsCache {
                total_chunks,
                total_space,
                free_space,
                available_space,
                timestamp: std::time::Instant::now(),
            });
        }

        Response::StorageStats {
            total_chunks,
            total_size,
            replication_factor: self.replication_factor,
            nodes_count,
            total_space,
            free_space,
            available_space,
        }
    }

    /// Handle get healing status request
    async fn handle_get_healing_status(&self) -> Response {
        let healing_guard = self.healing.read().await;
        match healing_guard.as_ref() {
            Some(healing) => {
                let stats = healing.get_stats().await;
                Response::HealingStatus {
                    enabled: stats.auto_heal_enabled,
                    pending_count: stats.pending_healing,
                    in_flight_count: stats.in_flight_healing,
                    stalled_count: stats.stalled_healing,
                    last_check: 0,
                }
            }
            None => Response::HealingStatus {
                enabled: false,
                pending_count: 0,
                in_flight_count: 0,
                stalled_count: 0,
                last_check: 0,
            },
        }
    }

    /// Return the physical on-disk size of each requested chunk.
    /// Chunks not present on this node get size=0. Used by quorum-based metadata repair.
    async fn handle_query_chunk_sizes(&self, chunk_ids: Vec<dfs_common::ChunkId>) -> Response {
        let sizes: Vec<u64> = chunk_ids.iter()
            .map(|id| self.storage.get_chunk_size(id).unwrap_or(0))
            .collect();
        Response::ChunkSizes { sizes }
    }

    /// Handle trigger scrub request
    async fn handle_trigger_scrub(&self) -> Response {
        // Scrubber runs on its own interval loop; no immediate trigger implemented yet.
        Response::Ok { data: None }
    }

    /// Handle enable healing request
    async fn handle_enable_healing(&self) -> Response {
        // Auto-heal flag is set at startup from config; runtime toggling not yet supported.
        Response::Ok { data: None }
    }

    /// Handle disable healing request
    async fn handle_disable_healing(&self) -> Response {
        // Auto-heal flag is set at startup from config; runtime toggling not yet supported.
        Response::Ok { data: None }
    }

    /// Handle trigger healing request — runs an immediate heal cycle on the leader.
    async fn handle_trigger_healing(&self) -> Response {
        let healing_guard = self.healing.read().await;
        match healing_guard.as_ref() {
            Some(healing) => {
                let healing = healing.clone();
                drop(healing_guard);
                tokio::spawn(async move {
                    if let Err(e) = healing.trigger_heal_now().await {
                        warn!("Manual heal cycle error: {}", e);
                    }
                });
                Response::Ok { data: None }
            }
            None => Response::Error {
                message: "Healing manager not available".to_string(),
                code: dfs_common::ErrorCode::InternalError,
            },
        }
    }

    /// Handle on-demand metadata repair request.
    ///
    /// Runs locally first (path index repair, chunk map rebuild, chunk location
    /// rebuild), then collects the leader's authoritative file ID set and sends
    /// ReconcileMetadata to all followers so they remove stale records that
    /// accumulated from missed deletes. Returns immediately; work is backgrounded.
    async fn handle_trigger_metadata_repair(&self) -> Response {
        let metadata = self.metadata.clone();
        let chunk_map = self.chunk_map.clone();
        let cluster = self.cluster.clone();
        let client = self.network_client();
        let local_id = self.cluster.local_node_id();
        tokio::spawn(async move {
            // Step 1: local repair on this node (runs in blocking thread — sled scans).
            let (live_file_ids, files_to_check): (Vec<dfs_common::FileId>, Vec<dfs_common::FileMetadata>) =
                tokio::task::spawn_blocking({
                    let metadata = metadata.clone();
                    let chunk_map = chunk_map.clone();
                    move || -> anyhow::Result<(Vec<dfs_common::FileId>, Vec<dfs_common::FileMetadata>)> {
                        info!("Metadata repair: rebuilding path index");
                        if let Err(e) = metadata.repair_path_index() {
                            warn!("Metadata repair: path index repair failed: {}", e);
                        } else {
                            info!("Metadata repair: path index repair complete");
                        }

                        // Rebuild in-memory chunk map.
                        let mut built = 0usize;
                        let mut total = 0usize;
                        let _ = metadata.scan_files(|file| {
                            total += 1;
                            if !file.chunk_locations.is_empty() {
                                chunk_map.insert(file.id, (file.chunk_locations.clone(), file.write_seq));
                                built += 1;
                            }
                            Ok(())
                        });
                        info!("Metadata repair: chunk map rebuilt: {}/{} files", built, total);

                        // Rebuild missing chunk: routing table entries.
                        info!("Metadata repair: rebuilding missing chunk location records");
                        match metadata.rebuild_chunk_locations_from_files() {
                            Ok((written, _)) if written > 0 =>
                                info!("Metadata repair: restored {} missing chunk: records", written),
                            Ok(_) =>
                                info!("Metadata repair: chunk location records are complete (no gaps)"),
                            Err(e) =>
                                warn!("Metadata repair: chunk location rebuild failed: {}", e),
                        }

                        // Collect files needing size verification (deferred — quorum query
                        // happens after the blocking task, on the async side).
                        let files_to_check: Vec<dfs_common::FileMetadata> = {
                            let mut v = Vec::new();
                            let _ = metadata.scan_files(|file| {
                                if !file.chunk_locations.is_empty() {
                                    v.push(file.clone());
                                }
                                Ok(())
                            });
                            v
                        };

                        // Collect the authoritative live file ID set from this node's
                        // now-repaired file: records. Sent to followers for reconciliation.
                        let mut ids = Vec::new();
                        let _ = metadata.scan_files(|file| {
                            ids.push(file.id);
                            Ok(())
                        });
                        info!("Metadata repair: collected {} live file IDs for follower reconciliation", ids.len());
                        Ok((ids, files_to_check))
                    }
                })
                .await
                .unwrap_or_else(|_| Ok((Vec::new(), Vec::new())))
                .unwrap_or_default();

            if live_file_ids.is_empty() {
                warn!("Metadata repair: no live file IDs collected — skipping follower reconciliation");
                return;
            }

            // Only the leader runs reconciliation and quorum repair.
            if !cluster.is_leader().await {
                return;
            }
            let nodes = cluster.get_all_nodes().await;
            let online_nodes: Vec<_> = nodes.iter()
                .filter(|n| n.status == dfs_common::NodeStatus::Online)
                .collect();

            // Step 2: quorum-based file size repair.
            //
            // Strategy: per-chunk voting. For each chunk in a file, query all nodes
            // listed as holding it and ask for the physical on-disk byte count.
            // If ≥ majority of replica nodes agree on a size, that size is authoritative.
            // A node that disagrees has a corrupt/truncated copy — mark it for healing.
            // The file's authoritative size = max(chunk offset + authoritative size)
            // across all chunks — NOT a sum, since sparse files have gaps between
            // chunks and the chunks present don't tile the full logical extent.
            //
            // This is correct even when stored metadata sizes are corrupted: we read
            // from the physical layer and take the majority view.
            let mut repaired_files: Vec<dfs_common::FileMetadata> = Vec::new();
            let mut chunks_to_heal: Vec<dfs_common::ChunkId> = Vec::new();

            // Build a cache of QueryChunkSizes responses per node, per file, so we
            // issue at most one RPC per (node, file) pair.
            // node_id → { chunk_id → physical_size }
            // We'll populate this lazily as we process each file.

            for file in &files_to_check {
                // Collect all unique node IDs referenced by this file's chunks.
                let mut node_ids_for_file: std::collections::HashSet<dfs_common::NodeId> =
                    std::collections::HashSet::new();
                for loc in &file.chunk_locations {
                    for &nid in &loc.nodes {
                        node_ids_for_file.insert(nid);
                    }
                }

                let all_chunk_ids: Vec<dfs_common::ChunkId> =
                    file.chunk_locations.iter().map(|l| l.chunk_id).collect();

                // Query each node that holds chunks of this file once,
                // getting physical sizes for all chunks in the file in one RPC.
                // node_id → Vec<u64> parallel to all_chunk_ids
                let mut node_chunk_sizes: std::collections::HashMap<dfs_common::NodeId, Vec<u64>> =
                    std::collections::HashMap::new();

                for node_info in &online_nodes {
                    if !node_ids_for_file.contains(&node_info.id) {
                        continue;
                    }
                    let req = dfs_common::Request::QueryChunkSizes {
                        chunk_ids: all_chunk_ids.clone(),
                    };
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        client.send_message(node_info.addr, dfs_common::Message::Request(req)),
                    ).await {
                        Ok(Ok(envelope)) => {
                            if let dfs_common::Message::Response(
                                dfs_common::Response::ChunkSizes { sizes }
                            ) = envelope.message {
                                node_chunk_sizes.insert(node_info.id, sizes);
                            }
                        }
                        Ok(Err(e)) => warn!("Metadata repair: QueryChunkSizes to {} failed: {}", node_info.id, e),
                        Err(_) => warn!("Metadata repair: QueryChunkSizes to {} timed out", node_info.id),
                    }
                }

                if node_chunk_sizes.is_empty() {
                    debug!("Metadata repair: no nodes responded for file {} — skipping", file.path);
                    continue;
                }

                // Per-chunk voting: for each chunk, find the majority physical size
                // among nodes that are listed as holding it. The authoritative file
                // size is max(offset + majority_size) across chunks, not a sum:
                // sparse files have gaps between chunks, so summing physical chunk
                // sizes undercounts the true logical size (and previously corrupted
                // FileMetadata.size for sparse files like VM disk images down to the
                // sum of their populated chunks).
                const CHUNK_SIZE_FOR_REPAIR: u64 = 4 * 1024 * 1024;
                let mut authoritative_file_size: u64 = 0;
                let mut any_chunk_ambiguous = false;

                for (chunk_idx, loc) in file.chunk_locations.iter().enumerate() {
                    let chunk_offset = loc.file_offset.unwrap_or(chunk_idx as u64 * CHUNK_SIZE_FOR_REPAIR);

                    // Collect physical sizes from nodes listed as holding this chunk.
                    let mut size_votes: std::collections::HashMap<u64, Vec<dfs_common::NodeId>> =
                        std::collections::HashMap::new();
                    for &nid in &loc.nodes {
                        if let Some(sizes) = node_chunk_sizes.get(&nid) {
                            let phys = sizes.get(chunk_idx).copied().unwrap_or(0);
                            size_votes.entry(phys).or_default().push(nid);
                        }
                        // If node didn't respond, we simply don't count it.
                    }

                    if size_votes.is_empty() {
                        // No replica node responded — can't determine authoritative size.
                        authoritative_file_size = authoritative_file_size.max(chunk_offset + loc.size as u64);
                        any_chunk_ambiguous = true;
                        continue;
                    }

                    // Majority = group with the most votes; tie-break by largest size.
                    let (majority_size, majority_nodes) = size_votes.iter()
                        .max_by_key(|(size, nodes)| (nodes.len(), *size))
                        .map(|(&size, nodes)| (size, nodes.clone()))
                        .unwrap();

                    if majority_size == 0 {
                        // Majority says chunk is absent — file may still be recording.
                        // Don't trust this for size repair; use stored loc.size.
                        authoritative_file_size = authoritative_file_size.max(chunk_offset + loc.size as u64);
                        any_chunk_ambiguous = true;
                        continue;
                    }

                    authoritative_file_size = authoritative_file_size.max(chunk_offset + majority_size);

                    // Mark nodes whose physical size disagrees with the majority.
                    for (&reported_size, bad_nodes) in &size_votes {
                        if reported_size != majority_size {
                            for &bad_node in bad_nodes {
                                warn!(
                                    "Metadata repair: chunk {} on node {} has wrong physical size \
                                     (got {}, majority={}): queuing for re-healing",
                                    loc.chunk_id, bad_node, reported_size, majority_size
                                );
                                if !chunks_to_heal.contains(&loc.chunk_id) {
                                    chunks_to_heal.push(loc.chunk_id);
                                }
                            }
                        }
                    }

                    // If the stored chunk_location size is wrong, log it (we don't update
                    // chunk_location sizes here — the healer will fix via re-replication).
                    if loc.size as u64 != majority_size && majority_size > 0 {
                        debug!(
                            "Metadata repair: chunk {} location size {} ≠ physical majority {} for file {}",
                            loc.chunk_id, loc.size, majority_size, file.path
                        );
                    }
                    let _ = majority_nodes; // used for logging above
                }

                // If authoritative size differs from stored file.size, fix the metadata.
                if !any_chunk_ambiguous && authoritative_file_size != file.size && authoritative_file_size > 0 {
                    let mut fixed = file.clone();
                    fixed.size = authoritative_file_size;
                    fixed.modified_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    fixed.write_seq = fixed.write_seq.saturating_add(1);
                    warn!(
                        "Metadata repair: file {} size corrected by chunk quorum: metadata={} → physical={}",
                        file.path, file.size, authoritative_file_size
                    );
                    match metadata.put_file(&fixed) {
                        Ok(_) => repaired_files.push(fixed),
                        Err(e) => warn!("Metadata repair: failed to write corrected size for {}: {}", file.path, e),
                    }
                }
            }

            // Queue corrupt chunks for re-healing (bypasses the normal delay).
            if !chunks_to_heal.is_empty() {
                info!("Metadata repair: queuing {} corrupt chunks for immediate re-healing", chunks_to_heal.len());
                if let Some(healing) = {
                    // Can't hold the healing RwLock across await; just clone the Arc.
                    // The healing manager is available via a shared reference on the server,
                    // but here we're in a spawned task without self. Re-heal via HealFile
                    // admin message to the local node instead.
                    None::<()>
                } {
                    let _: () = healing; // unreachable, just for type inference
                }
                // Send HealFile for each affected file via local loopback.
                let local_addr = cluster.local_addr();
                let affected_files: std::collections::HashSet<dfs_common::FileId> = files_to_check.iter()
                    .filter(|f| f.chunk_locations.iter().any(|l| chunks_to_heal.contains(&l.chunk_id)))
                    .map(|f| f.id)
                    .collect();
                for fid in affected_files {
                    let req = dfs_common::Request::HealFile { path: fid.to_string() };
                    if let Err(e) = client.send_message(local_addr, dfs_common::Message::Request(req)).await {
                        warn!("Metadata repair: failed to trigger HealFile for {}: {}", fid, e);
                    }
                }
            }

            info!("Metadata repair: size repair complete ({} files corrected, {} corrupt chunks queued)",
                  repaired_files.len(), chunks_to_heal.len());

            // Step 3: send ReconcileMetadata to every online follower.
            for node in &nodes {
                if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                    continue;
                }
                let req = dfs_common::Request::ReconcileMetadata {
                    live_file_ids: live_file_ids.clone(),
                };
                match client.send_message(node.addr, dfs_common::Message::Request(req)).await {
                    Ok(_) => debug!("ReconcileMetadata sent to node {}", node.id),
                    Err(e) => warn!("Failed to send ReconcileMetadata to node {}: {}", node.id, e),
                }
            }
            info!("Metadata repair: follower reconciliation complete");

            // Step 4: push repaired metadata to followers so they get corrected sizes.
            if !repaired_files.is_empty() {
                info!("Metadata repair: broadcasting {} repaired files to followers", repaired_files.len());
                for node in &nodes {
                    if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                        continue;
                    }
                    for file in &repaired_files {
                        let req = dfs_common::Request::ReplicateMetadata {
                            metadata: file.clone(),
                            ttl: 0,
                        };
                        if let Err(e) = client.send_message(node.addr, dfs_common::Message::Request(req)).await {
                            warn!("Metadata repair: failed to push repaired {} to {}: {}", file.path, node.id, e);
                        }
                    }
                }
            }
        });
        Response::Ok { data: None }
    }

    /// Handle heal file request — queue all chunks of a specific file for immediate healing.
    /// Accepts either a file path or a UUID string. Only meaningful on the leader.
    async fn handle_heal_file(&self, path: String) -> Response {
        // Resolve file metadata — try UUID first, then path
        let file_meta = if let Ok(uuid) = uuid::Uuid::parse_str(&path) {
            let file_id = dfs_common::FileId::from_uuid(uuid);
            match self.metadata.get_file(&file_id) {
                Ok(Some(m)) => m,
                Ok(None) => return Response::Error {
                    message: format!("File not found: {}", path),
                    code: ErrorCode::NotFound,
                },
                Err(e) => return Response::Error {
                    message: format!("Failed to look up file: {}", e),
                    code: ErrorCode::InternalError,
                },
            }
        } else {
            match self.metadata.get_file_by_path(&path) {
                Ok(Some(m)) => m,
                Ok(None) => return Response::Error {
                    message: format!("File not found: {}", path),
                    code: ErrorCode::NotFound,
                },
                Err(e) => return Response::Error {
                    message: format!("Failed to look up file: {}", e),
                    code: ErrorCode::InternalError,
                },
            }
        };

        let healing_guard = self.healing.read().await;
        let healing = match healing_guard.as_ref() {
            Some(h) => h.clone(),
            None => return Response::Error {
                message: "Healing manager not available".to_string(),
                code: ErrorCode::InternalError,
            },
        };
        drop(healing_guard);

        let chunk_ids: Vec<ChunkId> = file_meta.chunk_locations.iter().map(|l| l.chunk_id).collect();
        let count = chunk_ids.len();
        healing.queue_chunks_immediate(chunk_ids).await;

        info!("HealFile: queued {} chunks for immediate healing (file: {})", count, file_meta.path);
        Response::Ok {
            data: Some(format!("Queued {} chunks for immediate healing", count).into_bytes()),
        }
    }

    /// Handle RepairFile: verify chunk hashes on all replicas, remove corrupt copies,
    /// and queue under/over-replicated chunks for immediate healing.
    /// Returns immediately — all work runs in the background, logged to server stdout.
    /// When force=true the post-election leadership grace period is bypassed.
    async fn handle_repair_file(&self, path: String, force: bool) -> Response {
        // Must be the leader to issue destructive operations.
        if !self.cluster.is_leader().await {
            return Response::Error {
                message: "This node is not the cluster leader — send RepairFile to the leader".to_string(),
                code: ErrorCode::NotFound,
            };
        }

        // Resolve file metadata up front so we can report errors immediately.
        let file_meta = if let Ok(uuid) = uuid::Uuid::parse_str(&path) {
            let file_id = dfs_common::FileId::from_uuid(uuid);
            match self.metadata.get_file(&file_id) {
                Ok(Some(m)) => m,
                Ok(None) => return Response::Error {
                    message: format!("File not found: {}", path),
                    code: ErrorCode::NotFound,
                },
                Err(e) => return Response::Error {
                    message: format!("Failed to look up file: {}", e),
                    code: ErrorCode::InternalError,
                },
            }
        } else {
            match self.metadata.get_file_by_path(&path) {
                Ok(Some(m)) => m,
                Ok(None) => return Response::Error {
                    message: format!("File not found: {}", path),
                    code: ErrorCode::NotFound,
                },
                Err(e) => return Response::Error {
                    message: format!("Failed to look up file: {}", e),
                    code: ErrorCode::InternalError,
                },
            }
        };

        let healing_guard = self.healing.read().await;
        let healing = match healing_guard.as_ref() {
            Some(h) => h.clone(),
            None => return Response::Error {
                message: "Healing manager not available".to_string(),
                code: ErrorCode::InternalError,
            },
        };
        drop(healing_guard);

        // Check cluster health — even with force=true we refuse destructive ops
        // when 2+ nodes are down (the surviving copy might be the last one).
        let all_nodes = self.cluster.get_all_nodes().await;
        let total = all_nodes.len();
        let online = all_nodes.iter().filter(|n| n.status == dfs_common::NodeStatus::Online).count();
        let nodes_down = total.saturating_sub(online);
        if nodes_down > 1 {
            return Response::Error {
                message: format!(
                    "Repair refused: {} node(s) down — destructive ops unsafe with 2+ nodes offline",
                    nodes_down
                ),
                code: ErrorCode::InternalError,
            };
        }

        let destructive_allowed = force || {
            let grace_elapsed = self.cluster.time_since_became_leader().await
                .map_or(true, |d| d.as_secs() >= crate::healing::LEADER_CHANGE_GRACE_SECS);
            grace_elapsed
        };

        let total_chunks = file_meta.chunk_locations.len();
        let file_path = file_meta.path.clone();

        // Clone everything the background task needs.
        let client = self.client.clone();
        let metadata = self.metadata.clone();
        let local_id = self.cluster.local_node_id();
        let rf = self.replication_factor;

        // Spawn the per-chunk work in the background. Process one chunk at a time so
        // we don't flood all replica nodes with hundreds of concurrent verify RPCs.
        tokio::spawn(async move {
            info!("RepairFile: starting background repair of {} ({} chunks)", file_path, total_chunks);
            let mut chunks_checked = 0usize;
            let mut corrupt_removed = 0usize;
            let mut heal_queued = 0usize;

            for chunk_loc in &file_meta.chunk_locations {
                let chunk_id = chunk_loc.chunk_id;
                let file_offset = match chunk_loc.file_offset {
                    Some(o) => o,
                    None => continue,
                };
                chunks_checked += 1;

                // Refresh the live location from sled (may have more nodes than inline).
                let live_loc = metadata.get_chunk_location(&chunk_id)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| chunk_loc.clone());

                // Resolve online replica addresses.
                let replica_addrs: Vec<(dfs_common::NodeId, std::net::SocketAddr)> = live_loc.nodes.iter()
                    .filter_map(|node_id| {
                        all_nodes.iter()
                            .find(|n| n.id == *node_id && n.status == dfs_common::NodeStatus::Online)
                            .map(|n| (*node_id, n.addr))
                    })
                    .collect();

                if replica_addrs.is_empty() {
                    continue;
                }

                // Verify all replicas of this chunk concurrently (N RPCs in parallel,
                // one per node), then await before moving on to the next chunk.
                // Carry (found, valid) so we can distinguish ghost from corrupt.
                let verify_req = dfs_common::Request::VerifyChunkIntegrity {
                    chunk_id,
                    file_offset,
                    file_id: live_loc.file_id.or(chunk_loc.file_id),
                };
                let mut verify_set: tokio::task::JoinSet<(dfs_common::NodeId, std::net::SocketAddr, bool, bool)> =
                    tokio::task::JoinSet::new();
                for &(node_id, addr) in &replica_addrs {
                    let c = client.clone();
                    let req = verify_req.clone();
                    verify_set.spawn(async move {
                        let (found, valid) = match c.send_message(addr, dfs_common::Message::Request(req)).await {
                            Ok(env) => match env.message {
                                dfs_common::Message::Response(
                                    dfs_common::Response::ChunkValid { found, valid }
                                ) => (found, valid),
                                _ => (false, false), // unexpected response → treat as not found
                            },
                            Err(_) => (false, false), // RPC failure → treat as not found
                        };
                        (node_id, addr, found, valid)
                    });
                }

                // found=true, valid=true  → healthy replica
                // found=true, valid=false → CORRUPT (file exists but hash mismatch) → delete
                // found=false             → ghost (file missing) → skip, healer handles it
                let mut valid_nodes: Vec<dfs_common::NodeId> = Vec::new();
                let mut corrupt_addrs: Vec<(dfs_common::NodeId, std::net::SocketAddr)> = Vec::new();
                while let Some(res) = verify_set.join_next().await {
                    match res {
                        Ok((nid, _addr, true, true)) => {
                            valid_nodes.push(nid);
                        }
                        Ok((nid, addr, true, false)) => {
                            warn!("RepairFile: chunk {} on node {} — file found but hash MISMATCH (corrupt)", chunk_id, nid);
                            corrupt_addrs.push((nid, addr));
                        }
                        Ok((nid, _addr, false, _)) => {
                            debug!("RepairFile: chunk {} on node {} — file not found (ghost replica, healer will prune)", chunk_id, nid);
                        }
                        Err(e) => warn!("RepairFile: verify task panicked for chunk {}: {}", chunk_id, e),
                    }
                }

                // Delete corrupt replicas.
                if destructive_allowed {
                    for (corrupt_id, corrupt_addr) in &corrupt_addrs {
                        let del = dfs_common::Request::DeleteChunkReplica { chunk_id, leader_id: local_id };
                        match client.send_message(*corrupt_addr, dfs_common::Message::Request(del)).await {
                            Ok(env) if matches!(env.message, dfs_common::Message::Response(dfs_common::Response::Ok { .. })) => {
                                info!("RepairFile: deleted corrupt replica of chunk {} from node {}", chunk_id, corrupt_id);
                                corrupt_removed += 1;
                            }
                            Ok(env) => warn!("RepairFile: node {} refused to delete corrupt chunk {}: {:?}",
                                             corrupt_id, chunk_id, env.message),
                            Err(e) => warn!("RepairFile: could not reach {} to delete corrupt chunk {}: {}",
                                            corrupt_id, chunk_id, e),
                        }
                    }

                    if !corrupt_addrs.is_empty() {
                        let corrupt_ids: Vec<dfs_common::NodeId> = corrupt_addrs.iter().map(|(id, _)| *id).collect();
                        let clean_nodes: Vec<dfs_common::NodeId> = live_loc.nodes.iter()
                            .filter(|n| !corrupt_ids.contains(n))
                            .copied()
                            .collect();
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;
                        let updated = dfs_common::ChunkLocation {
                            chunk_id,
                            nodes: clean_nodes,
                            size: live_loc.size,
                            checksum: live_loc.checksum,
                            file_offset: live_loc.file_offset,
                            written_at: Some(now_ms),
                            client_write_seq: None,
                            file_id: live_loc.file_id,
                        };
                        let _ = metadata.put_chunk_location(&updated);
                        // Broadcast to peers (fire-and-forget).
                        let bc_loc = updated.clone();
                        let bc_nodes = all_nodes.clone();
                        let bc_client = client.clone();
                        let bc_lid = local_id;
                        tokio::spawn(async move {
                            for node in &bc_nodes {
                                if node.id == bc_lid || node.status != dfs_common::NodeStatus::Online { continue; }
                                let req = dfs_common::Request::ReplicateChunkLocation {
                                    location: bc_loc.clone(), file_id: None,
                                };
                                let _ = bc_client.send_message(node.addr, dfs_common::Message::Request(req)).await;
                            }
                        });
                    }
                }

                // Do NOT touch the healer queue here. The normal healer cycle already
                // discovers and fixes under/over-replication. RepairFile's only job is
                // to remove genuinely corrupt replicas (hash mismatch on an existing file).
                // Adding heal queue calls here caused the repair to over-add replicas
                // by treating ghost replicas as corruption.
                let _ = (valid_nodes, rf, &healing, &mut heal_queued); // suppress unused warnings

                // Log progress every 50 chunks.
                if chunks_checked % 50 == 0 {
                    info!("RepairFile: {}/{} chunks checked ({} corrupt removed, {} queued for healing)",
                          chunks_checked, total_chunks, corrupt_removed, heal_queued);
                }
            }

            info!("RepairFile complete for {}: {} chunks checked, {} corrupt replicas removed, {} chunks queued for healing",
                  file_path, chunks_checked, corrupt_removed, heal_queued);
        });

        Response::Ok {
            data: Some(format!(
                "Repair started in background for {} ({} chunks). Progress logged to server.",
                file_meta.path, total_chunks
            ).into_bytes()),
        }
    }

    /// Handle get file info request
    async fn handle_get_file_info(&self, path: String) -> Response {
        debug!("Handling get file info: {}", path);

        match self.metadata.get_file_by_path(&path) {
            Ok(Some(metadata)) => {
                // Use the CHUNK_TABLE sled record as the authoritative node list —
                // it reflects healer updates (heal, trim, ghost prune) that never
                // touch the FileMetadata inline copy. Unioning the two sources
                // creates phantom extra nodes whenever the healer has replaced a
                // node in the CHUNK_TABLE but the inline still has the old one.
                let chunk_locations = metadata.chunk_locations.iter().map(|inline| {
                    if let Ok(Some(sled_loc)) = self.metadata.get_chunk_location(&inline.chunk_id) {
                        ChunkLocation { nodes: sled_loc.nodes, ..inline.clone() }
                    } else {
                        inline.clone()
                    }
                }).collect();

                Response::FileInfo {
                    metadata,
                    chunk_locations,
                }
            }
            Ok(None) => Response::Error {
                message: "File not found".to_string(),
                code: ErrorCode::NotFound,
            },
            Err(e) => {
                warn!("Failed to get file info: {}", e);
                Response::Error {
                    message: format!("Failed to get file info: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle get file info by ID request
    async fn handle_get_file_info_by_id(&self, file_id: dfs_common::FileId) -> Response {
        debug!("Handling get file info by id: {}", file_id);

        match self.metadata.get_file(&file_id) {
            Ok(Some(metadata)) => {
                let chunk_locations = metadata.chunk_locations.iter().map(|inline| {
                    if let Ok(Some(sled_loc)) = self.metadata.get_chunk_location(&inline.chunk_id) {
                        ChunkLocation { nodes: sled_loc.nodes, ..inline.clone() }
                    } else {
                        inline.clone()
                    }
                }).collect();

                Response::FileInfo {
                    metadata,
                    chunk_locations,
                }
            }
            Ok(None) => Response::Error {
                message: format!("File not found by ID: {}", file_id),
                code: ErrorCode::NotFound,
            },
            Err(e) => {
                warn!("Failed to get file info by id: {}", e);
                Response::Error {
                    message: format!("Failed to get file info by id: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle GetFileChunkMap — returns a windowed slice of the chunk location map.
    /// Served from the in-memory chunk map maintained by all nodes (leader-authoritative).
    /// Returns the sparse list as-is with total_chunks = max chunk index + 1 so the client
    /// can place each entry at its correct position using file_offset.
    async fn handle_get_file_chunk_map(&self, file_id: FileId, from_chunk: u32, count: u32) -> Response {
        let slice_response = |locations: &Vec<dfs_common::ChunkLocation>, write_seq: u64| {
            const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
            // total_chunks = max chunk index + 1 (not list length) so the client knows
            // the true density of the file and can size its engine map correctly.
            // total_chunks = max chunk index + 1 so the client engine map covers
            // the full sparse extent. For dense/sequential files this equals
            // locations.len(); for sparse files (e.g. VM disk images) it equals
            // the highest written chunk index + 1, which may be much larger.
            // The client window tracking uses this to know when it has fetched
            // the complete map — returning locations.len() would cause constant
            // re-fetches for sparse files where reads land beyond the last chunk.
            let max_chunk_idx = locations.iter()
                .filter_map(|l| l.file_offset.map(|o| (o / CHUNK_SIZE) as u32))
                .max()
                .unwrap_or(locations.len().saturating_sub(1) as u32);
            let total_chunks = max_chunk_idx + 1;

            // Return only entries whose chunk index falls within [from_chunk, from_chunk+count).
            // Override `nodes` from the CHUNK_TABLE sled record — same correction
            // handle_get_file_info applies. The healer (ghost pruning, re-replication,
            // over-replication trim) updates CHUNK_TABLE directly but never touches
            // this in-memory/inline chunk_locations copy, so without this clients can
            // be routed to a node the healer already moved the chunk away from.
            let window: Vec<dfs_common::ChunkLocation> = locations.iter()
                .filter(|l| {
                    let idx = l.file_offset.map(|o| (o / CHUNK_SIZE) as u32).unwrap_or(0);
                    idx >= from_chunk && idx < from_chunk.saturating_add(count)
                })
                .map(|l| match self.metadata.get_chunk_location(&l.chunk_id) {
                    Ok(Some(sled_loc)) => ChunkLocation { nodes: sled_loc.nodes, ..l.clone() },
                    _ => l.clone(),
                })
                .collect();

            Response::FileChunkMap {
                file_id,
                locations: window,
                from_chunk,
                total_chunks,
                write_seq,
            }
        };

        if let Some(entry) = self.chunk_map.get(&file_id) {
            let (locations, write_seq) = entry.value();
            return slice_response(locations, *write_seq);
        }

        // Cache miss — fall back to sled (chunk map may still be rebuilding after restart,
        // or the file was written via a path that skipped chunk_map_update).
        let metadata_store = self.metadata.clone();
        let result = tokio::task::spawn_blocking(move || metadata_store.get_file(&file_id)).await;
        match result {
            Ok(Ok(Some(metadata))) if !metadata.chunk_locations.is_empty() => {
                // Populate cache for future lookups.
                self.chunk_map.insert(file_id, (metadata.chunk_locations.clone(), metadata.write_seq));
                slice_response(&metadata.chunk_locations, metadata.write_seq)
            }
            _ => Response::Error {
                message: format!("No chunk map entry for file {}", file_id),
                code: ErrorCode::NotFound,
            },
        }
    }

    /// Handle list all files request
    async fn handle_list_all_files(&self) -> Response {
        debug!("Handling list all files");

        let metadata = self.metadata.clone();
        let result = tokio::task::spawn_blocking(move || metadata.list_files()).await;
        let list_result = match result {
            Ok(r) => r,
            Err(e) => {
                warn!("Failed to list files (spawn_blocking panic): {}", e);
                return Response::Error {
                    message: format!("Failed to list files: {}", e),
                    code: ErrorCode::InternalError,
                };
            }
        };
        match list_result {
            Ok(files) => {
                let total_count = files.len();
                // Strip chunk_locations before sending — the client startup warm only
                // needs scalar fields (path, size, id, mode, timestamps) for getattr
                // and dir cache. Sending full chunk arrays for hundreds of recordings
                // serializes gigabytes and blocks the leader's tokio workers.
                // Chunk locations are fetched lazily on open() via GetFileChunkMap.
                let files: Vec<_> = files.into_iter().map(|mut f| {
                    f.chunk_locations.clear();
                    f
                }).collect();
                Response::FileList { files, total_count }
            }
            Err(e) => {
                warn!("Failed to list files: {}", e);
                Response::Error {
                    message: format!("Failed to list files: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle purge file metadata request
    async fn handle_purge_file_metadata(&self, path: String) -> Response {
        info!("Handling purge file metadata: {}", path);

        // Get metadata to find file ID
        match self.metadata.get_file_by_path(&path) {
            Ok(Some(metadata)) => {
                let file_id = metadata.id;

                // Delete from local metadata store only (not chunks)
                match self.metadata.delete_file(&file_id) {
                    Ok(_) => {
                        info!("Purged local metadata for file: {}", path);

                        // CRITICAL: Replicate metadata deletion to all other nodes
                        // This ensures rename operations don't leave stale metadata on other servers
                        let cluster = self.cluster.clone();
                        let client = self.client.clone();
                        let path_clone = path.clone();

                        tokio::spawn(async move {
                            let nodes = cluster.get_all_nodes().await;
                            let local_id = cluster.local_node_id();

                            info!("Replicating metadata purge for file: {}", path_clone);

                            for node in &nodes {
                                // Skip self and offline nodes
                                if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                                    continue;
                                }

                                let request = Request::DeleteMetadata {
                                    file_id,
                                    path: path_clone.clone(),
                                    chunk_ids: Vec::new(), // purge = metadata only, chunks kept
                                    ttl: 1,
                                };

                                if let Err(e) = client.send_message(node.addr, Message::Request(request)).await {
                                    warn!("Failed to replicate metadata purge to node {}: {}", node.id, e);
                                }
                            }
                        });

                        Response::Ok { data: None }
                    }
                    Err(e) => {
                        warn!("Failed to purge file metadata: {}", e);
                        Response::Error {
                            message: format!("Failed to purge file metadata: {}", e),
                            code: ErrorCode::InternalError,
                        }
                    }
                }
            }
            Ok(None) => Response::Error {
                message: format!("File not found: {}", path),
                code: ErrorCode::NotFound,
            },
            Err(e) => {
                warn!("Failed to get file metadata: {}", e);
                Response::Error {
                    message: format!("Failed to get file metadata: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    async fn handle_purge_file_metadata_by_id(&self, file_id: FileId, propagate: bool) -> Response {
        info!("Handling purge file metadata by ID: {}", file_id);

        // Delete from local node first
        match self.metadata.delete_file(&file_id) {
            Ok(_) => {
                info!("Purged metadata for file ID {} from local node", file_id);

                // Only the originating node broadcasts — peer recipients must not
                // re-broadcast or every delete causes an exponential storm.
                if propagate {
                    let nodes = self.cluster.get_all_nodes().await;
                    let local_id = self.cluster.local_node_id();
                    let client = self.client.clone();
                    let sem = self.broadcast_semaphore.clone();

                    tokio::spawn(async move {
                        let _permit = sem.acquire().await.ok();
                        for node in nodes {
                            if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                                continue;
                            }
                            if let Err(e) = client.send_message(
                                node.addr,
                                Message::Request(Request::PurgeFileMetadataById {
                                    file_id: file_id.clone(),
                                    propagate: false,
                                })
                            ).await {
                                warn!("Error contacting node {} for purge: {}", node.id, e);
                            }
                        }
                    });
                }

                Response::Ok { data: None }
            }
            Err(e) => {
                warn!("Failed to purge file metadata by ID: {}", e);
                Response::Error {
                    message: format!("Failed to purge file metadata: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle atomic rename file request
    /// This is critical - must update metadata path AND delete old path atomically
    /// to prevent file from disappearing during rename
    async fn handle_rename_file(&self, old_path: String, new_path: String) -> Response {
        info!("Handling atomic rename: {} -> {}", old_path, new_path);

        // Get existing metadata
        match self.metadata.get_file_by_path(&old_path) {
            Ok(Some(mut metadata)) => {
                let file_id = metadata.id;

                // Update path. Per POSIX, rename(2) does not change the file's
                // mtime, so modified_at carries over unchanged from the source.
                metadata.path = new_path.clone();
                // Bump write_seq so put_file's stale-drop guard never rejects this.
                // The path: index entry may have write_seq=0 (loaded from sled via
                // get_file_by_path), while the file: entry has a higher write_seq
                // from the client's enqueue. Incrementing ensures we always win.
                metadata.write_seq = metadata.write_seq.saturating_add(1);

                // Store new metadata locally first
                match self.metadata.put_file(&metadata) {
                    Ok(_) => {
                        // Now replicate to all servers BEFORE deleting old path
                        // This ensures the new metadata exists everywhere before we delete the old
                        let nodes = self.cluster.get_all_nodes().await;
                        let local_id = self.cluster.local_node_id();
                        let client = self.client.clone();
                        let metadata_clone = metadata.clone();
                        let old_path_clone = old_path.clone();

                        // Replicate new metadata synchronously
                        let mut put_success = true;
                        for node in &nodes {
                            if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                                continue;
                            }

                            let put_request = Request::ReplicateMetadata {
                                metadata: metadata_clone.clone(),
                                ttl: 0, // already broadcasting to all nodes, no further forwarding needed
                            };

                            if let Err(e) = client.send_message(node.addr, Message::Request(put_request)).await {
                                warn!("Failed to replicate new metadata to {}: {}", node.addr, e);
                                put_success = false;
                            }
                        }

                        if !put_success {
                            warn!("Some replications failed for rename {} -> {}", old_path, new_path);
                        }

                        // Delete the OLD path index entry locally first, then replicate
                        // synchronously to all peers so no peer can resolve the old path.
                        if let Err(e) = self.metadata.delete_path_index(&old_path) {
                            warn!("Failed to delete old path index during rename: {}", e);
                        }

                        // Synchronously send DeletePathIndex to all peers so the old path
                        // is gone cluster-wide before we reply to the client. This prevents
                        // any node from returning the renamed file under its old name.
                        {
                            let nodes = self.cluster.get_all_nodes().await;
                            let local_id = self.cluster.local_node_id();
                            for node in &nodes {
                                if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                                    continue;
                                }
                                let req = Request::DeletePathIndex { path: old_path.clone() };
                                if let Err(e) = self.client.send_message(node.addr, Message::Request(req)).await {
                                    warn!("Failed to replicate path deletion to {}: {}", node.addr, e);
                                }
                            }
                        }

                        info!("Renamed {} -> {} (file_id: {})", old_path, new_path, file_id);
                        Response::Ok { data: None }
                    }
                    Err(e) => {
                        warn!("Failed to store new metadata during rename: {}", e);
                        Response::Error {
                            message: format!("Failed to rename file: {}", e),
                            code: ErrorCode::InternalError,
                        }
                    }
                }
            }
            Ok(None) => Response::Error {
                message: format!("File not found: {}", old_path),
                code: ErrorCode::NotFound,
            },
            Err(e) => {
                warn!("Failed to find file for rename: {}", e);
                Response::Error {
                    message: format!("Failed to rename file: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    async fn handle_remove_node(&self, node_id: NodeId) -> Response {
        info!("Handling remove node request: {}", node_id);

        // Check if node exists
        if self.cluster.get_node(&node_id).await.is_none() {
            return Response::Error {
                message: format!("Node {} not found in cluster", node_id),
                code: ErrorCode::NotFound,
            };
        }

        // Remove from cluster
        match self.cluster.remove_node(&node_id).await {
            Ok(_) => {
                info!("Successfully removed node {} from cluster", node_id);
                Response::Ok { data: None }
            }
            Err(e) => {
                warn!("Failed to remove node {}: {}", node_id, e);
                Response::Error {
                    message: format!("Failed to remove node: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dfs_common::hash::compute_chunk_hash;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_server_write_read_local() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();
        let temp_metadata_dir = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));

        let server = Server::new(storage, metadata, 4 * 1024 * 1024, cluster, 3, temp_metadata_dir.path().to_path_buf());

        // Write data
        let data = b"Hello, distributed filesystem!";
        let chunk_ids_with_sizes = server.write_data(data, dfs_common::FileId::new()).await.unwrap();

        assert!(!chunk_ids_with_sizes.is_empty());

        // Read data back
        let chunk_ids: Vec<ChunkId> = chunk_ids_with_sizes.iter().map(|(id, _, _)| *id).collect();
        let read_data = server.read_data(&chunk_ids).await.unwrap();
        assert_eq!(data.as_slice(), read_data.as_slice());
    }

    #[tokio::test]
    async fn test_handle_write_read_chunk() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();
        let temp_metadata_dir = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));

        let server = Server::new(storage, metadata, 4 * 1024 * 1024, cluster, 3, temp_metadata_dir.path().to_path_buf());

        // Test write
        let data = b"Test chunk data";
        let hash = compute_chunk_hash(data);
        let chunk_id = ChunkId::from_hash(hash);

        let response = server
            .handle_write_chunk(chunk_id, data.to_vec(), hash)
            .await;

        match response {
            Response::Ok { .. } => {}
            _ => panic!("Expected Ok response"),
        }

        // Test read
        let response = server.handle_read_chunk(chunk_id, None).await;

        match response {
            Response::ChunkData { data: read_data, .. } => {
                assert_eq!(data.as_slice(), read_data.as_slice());
            }
            _ => panic!("Expected ChunkData response"),
        }
    }

    #[tokio::test]
    async fn test_handle_has_chunk() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();
        let temp_metadata_dir = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));

        let server = Server::new(storage, metadata, 4 * 1024 * 1024, cluster, 3, temp_metadata_dir.path().to_path_buf());

        let data = b"Test";
        let hash = compute_chunk_hash(data);
        let chunk_id = ChunkId::from_hash(hash);

        // Should not exist yet
        let response = server.handle_has_chunk(chunk_id).await;
        match response {
            Response::Bool { value } => assert!(!value),
            _ => panic!("Expected Bool response"),
        }

        // Write chunk
        server
            .handle_write_chunk(chunk_id, data.to_vec(), hash)
            .await;

        // Should exist now
        let response = server.handle_has_chunk(chunk_id).await;
        match response {
            Response::Bool { value } => assert!(value),
            _ => panic!("Expected Bool response"),
        }
    }

    /// chunk_map_update_location_for_file must converge on the location with the
    /// highest client_write_seq for a given file_offset, regardless of the order in
    /// which ReplicateChunkLocation messages physically arrive at the leader.
    ///
    /// This is the invariant a burst of rapid same-chunk MultiPatch rotations relies
    /// on (e.g. qcow2 preallocation rewriting two adjacent 64KB clusters back-to-back
    /// many times before the next flush_metadata_sync). Each rotation fires its RCL
    /// via an independent tokio::spawn, so arrival order at the leader is not
    /// guaranteed to match send order. As long as each rotation carries a distinct,
    /// monotonically increasing client_write_seq (see Client::next_write_seq, called
    /// once per MultiPatch), the `inc >= ext` guard below correctly keeps the
    /// highest-seq (chronologically last) location even when it arrives before an
    /// earlier (lower-seq) rotation's RCL.
    ///
    /// If client_write_seq were instead shared across all rotations in one flush
    /// cycle (the pre-fix behavior), `inc >= ext` degenerates to "last arrival wins"
    /// for ties — letting an intermediate rotation overwrite the final one and
    /// leaving the leader's chunk_map pointing at stale chunk data.
    #[tokio::test]
    async fn test_chunk_map_update_converges_on_highest_write_seq_despite_reordering() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();
        let temp_metadata_dir = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));

        let server = Server::new(storage, metadata, 4 * 1024 * 1024, cluster, 3, temp_metadata_dir.path().to_path_buf());

        let file_id = dfs_common::FileId::new();
        let file_offset = 2 * 1024 * 1024u64; // chunk 512's file_offset

        // 8 rotations (V1..V7, then the final/converged G), each with a distinct
        // chunk_id and a distinct, monotonically increasing client_write_seq.
        let rotations: Vec<ChunkLocation> = (1u64..=8).map(|seq| {
            let hash = compute_chunk_hash(format!("rotation-{}", seq).as_bytes());
            ChunkLocation {
                chunk_id: ChunkId::from_hash(hash),
                nodes: vec![],
                size: 65536,
                checksum: hash,
                file_offset: Some(file_offset),
                written_at: Some(1000 + seq),
                client_write_seq: Some(seq),
                file_id: None,
            }
        }).collect();
        let final_chunk_id = rotations[7].chunk_id; // seq=8, the last rotation (G)

        // Deliver out of order: G (seq=8) arrives 6th, before V7 (seq=7) and V8... wait,
        // there is no V8 — arrival order below interleaves seq=8 ahead of seq=6,7.
        let arrival_order = [1usize, 3, 2, 5, 4, 8, 6, 7];
        for seq in arrival_order {
            let loc = &rotations[seq - 1];
            server.chunk_map_update_location_for_file(file_id, loc).await;
        }

        // The leader's chunk_map must reflect the highest-seq (last) rotation, even
        // though it arrived 6th out of 8.
        let entry = server.chunk_map.get(&file_id).expect("chunk_map entry must exist");
        let (locs, _) = entry.value();
        let loc = locs.iter().find(|l| l.file_offset == Some(file_offset))
            .expect("location for file_offset must exist");
        assert_eq!(loc.chunk_id, final_chunk_id,
            "chunk_map must converge on the highest client_write_seq location (G), \
             not whichever RCL happened to arrive last");
        assert_eq!(loc.client_write_seq, Some(8));
    }

    /// GetFileChunkMap must report CHUNK_TABLE's current node list for each chunk,
    /// not the in-memory chunk_map / inline FileMetadata.chunk_locations copy.
    ///
    /// The healer (ghost-node pruning, re-replication, over-replication trim) writes
    /// its results directly to CHUNK_TABLE via batch_update_chunk_locations, and
    /// broadcasts ReplicateChunkLocations to other nodes — but a node that was just
    /// restarted rebuilds chunk_map from the inline FileMetadata.chunk_locations
    /// (FILE_TABLE), which the healer never touches. If GetFileChunkMap serves that
    /// stale copy, clients are routed to a node the healer already moved the chunk
    /// away from, producing "Chunk ... not found on this node" on every read.
    #[tokio::test]
    async fn test_get_file_chunk_map_uses_chunk_table_authoritative_nodes() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();
        let temp_metadata_dir = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));

        let server = Server::new(storage, metadata, 4 * 1024 * 1024, cluster, 3, temp_metadata_dir.path().to_path_buf());

        let file_id = dfs_common::FileId::new();
        let hash = compute_chunk_hash(b"chunk-data");
        let chunk_id = ChunkId::from_hash(hash);

        let stale_node = NodeId::new();   // node the healer has already moved this chunk OFF of
        let current_node = NodeId::new(); // node CHUNK_TABLE says actually holds it now

        let loc = ChunkLocation {
            chunk_id,
            nodes: vec![stale_node],
            size: 65536,
            checksum: hash,
            file_offset: Some(0),
            written_at: Some(1000),
            client_write_seq: Some(1),
            file_id: None,
        };

        // Simulate the in-memory chunk_map (e.g. rebuilt from inline FileMetadata.chunk_locations
        // after a restart) — still pointing at the node the chunk used to live on.
        server.chunk_map.insert(file_id, (vec![loc.clone()], 1));

        // Simulate the healer having relocated the chunk to `current_node` and recorded
        // that in CHUNK_TABLE, without updating chunk_map/inline (the actual drift).
        server.metadata.put_chunk_location(&ChunkLocation { nodes: vec![current_node], ..loc.clone() }).unwrap();

        let response = server.handle_get_file_chunk_map(file_id, 0, u32::MAX).await;
        match response {
            Response::FileChunkMap { locations, .. } => {
                assert_eq!(locations.len(), 1);
                assert_eq!(locations[0].nodes, vec![current_node],
                    "GetFileChunkMap must report CHUNK_TABLE's current node list, \
                     not the stale chunk_map/inline copy the healer already moved the chunk away from");
            }
            other => panic!("expected FileChunkMap response, got {:?}", other),
        }
    }
}

/// Implement MessageHandler trait for Server
impl MessageHandler for Server {
    fn handle_request(
        &self,
        request: Request,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + '_>> {
        Box::pin(async move { self.handle_request(request).await })
    }

    fn handle_cluster_message(
        &self,
        message: ClusterMessage,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + '_>> {
        Box::pin(async move {
            // Handle cluster messages (heartbeat, join, leave, etc.)
            match message {
                ClusterMessage::Heartbeat { node_info, cluster_view } => {
                    debug!("Received heartbeat from {} with {} gossip entries",
                           node_info.id, cluster_view.len());

                    // Re-add the sender unconditionally. This handles nodes that were
                    // purged after a long failure — update_heartbeat silently no-ops when
                    // the node isn't in the map, causing permanent split-brain.
                    // add_node is idempotent: it updates existing entries and inserts new ones.
                    if let Err(e) = self.cluster.add_node(node_info).await {
                        warn!("Failed to add/update heartbeat node: {}", e);
                    }

                    // Merge cluster view gossip if present
                    if !cluster_view.is_empty() {
                        if let Err(e) = self.cluster.merge_cluster_gossip(cluster_view).await {
                            warn!("Failed to merge cluster gossip: {}", e);
                        }
                    }

                    Response::Ok { data: None }
                }
                ClusterMessage::Join { node_info } => {
                    info!("Node {} joining cluster", node_info.id);
                    if let Err(e) = self.cluster.add_node(node_info).await {
                        warn!("Failed to add node: {}", e);
                        Response::Error {
                            message: format!("Failed to add node: {}", e),
                            code: ErrorCode::InternalError,
                        }
                    } else {
                        Response::Ok { data: None }
                    }
                }
                ClusterMessage::Leave { node_id } => {
                    info!("Node {} leaving cluster", node_id);
                    if let Err(e) = self.cluster.remove_node(&node_id).await {
                        warn!("Failed to remove node: {}", e);
                    }
                    Response::Ok { data: None }
                }
                ClusterMessage::JoinRequest { node_info } => {
                    info!("Received join request from node {}", node_info.id);

                    // Add node to cluster
                    if let Err(e) = self.cluster.add_node(node_info.clone()).await {
                        warn!("Failed to add node: {}", e);
                        let response = ClusterMessage::JoinResponse {
                            accepted: false,
                            cluster_nodes: vec![],
                        };
                        return Response::Ok {
                            data: Some(bincode::serialize(&response).unwrap()),
                        };
                    }

                    // Get all cluster nodes
                    let cluster_nodes = self.cluster.get_all_nodes().await;

                    info!(
                        "Node {} joined cluster, now {} nodes total",
                        node_info.id,
                        cluster_nodes.len()
                    );

                    // Return success with cluster state
                    let response = ClusterMessage::JoinResponse {
                        accepted: true,
                        cluster_nodes,
                    };

                    Response::Ok {
                        data: Some(bincode::serialize(&response).unwrap()),
                    }
                }
                ClusterMessage::NodeJoined { node_info } => {
                    debug!("Node {} joined the cluster (broadcast)", node_info.id);

                    // Only add if not already known (prevents re-processing)
                    let already_known = self.cluster.get_node(&node_info.id).await.is_some();

                    if !already_known {
                        info!("New node {} joined the cluster", node_info.id);
                        if let Err(e) = self.cluster.add_node(node_info).await {
                            warn!("Failed to add node from broadcast: {}", e);
                        }

                        // Save updated peer list to disk
                        let peer_addrs = self.cluster.get_all_peer_addrs().await;
                        if let Err(e) = ClusterManager::save_persisted_peers(&peer_addrs, &self.metadata_dir).await {
                            warn!("Failed to save persisted peers after NodeJoined: {}", e);
                        }
                    } else {
                        debug!("Node {} already known, ignoring duplicate join", node_info.id);
                    }

                    // NO reciprocal announcements - prevents infinite loops
                    Response::Ok { data: None }
                }
                ClusterMessage::LeaderAnnouncement { node_id, addr } => {
                    let local_id = self.cluster.local_node_id();
                    // If the announcer has a lower NodeId (higher election priority) than us,
                    // ensure they are marked Online in our gossip view so is_leader() yields to them.
                    if node_id < local_id {
                        let node_info = dfs_common::NodeInfo::new(node_id, addr, None);
                        if let Err(e) = self.cluster.add_node(node_info).await {
                            warn!("LeaderAnnouncement: failed to update node {}: {}", node_id, e);
                        }
                        if self.cluster.is_leader().await {
                            // We just conceded — this shouldn't happen since node_id < local_id
                            // means is_leader() should now return false. Log for diagnostics.
                            warn!("LeaderAnnouncement from higher-priority {}: we should have conceded but still see ourselves as leader", node_id);
                        } else {
                            info!("LeaderAnnouncement from {}: conceding leadership (they have higher priority)", node_id);
                        }
                    } else {
                        info!("LeaderAnnouncement from {}: we have higher priority ({}), ignoring", node_id, local_id);
                    }
                    Response::Ok { data: None }
                }
                ClusterMessage::GracefulLeave { node_id, addr: _, reason } => {
                    info!("Node {} is leaving gracefully (reason: {:?})", node_id, reason);
                    self.cluster.set_leaving(node_id, reason).await;
                    Response::Ok { data: None }
                }
                _ => Response::Error {
                    message: "Cluster message not implemented".to_string(),
                    code: ErrorCode::InternalError,
                },
            }
        })
    }

}

/// Count TCP connections in CLOSE_WAIT state on the given local port by reading
/// /proc/net/tcp and /proc/net/tcp6.  Returns 0 if the files can't be read.
/// Used by the connection pressure watchdog to distinguish real load from leaks.
fn count_close_wait_connections(port: u16) -> usize {
    // CLOSE_WAIT = state 8 in Linux /proc/net/tcp
    const CLOSE_WAIT: &str = "08";
    let hex_port = format!("{:04X}", port);
    let mut count = 0usize;

    for path in &["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = std::fs::read_to_string(path) else { continue };
        for line in content.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 4 { continue; }
            // field 1 = local_address (hex IP:port), field 3 = state
            let local = fields[1];
            let state = fields[3];
            // Local port is after the last ':' in the address field
            if state == CLOSE_WAIT {
                if let Some(p) = local.rsplit(':').next() {
                    if p == hex_port {
                        count += 1;
                    }
                }
            }
        }
    }
    count
}

impl Server {
    /// Periodically remove tombstones for chunks that are no longer on disk.
    /// Handles the case where the fire-and-forget DeleteChunk from the client failed —
    /// the tombstone guard is no longer needed once the data is gone.
    pub fn start_chunk_tombstone_cleanup_loop(self: Arc<Self>) {
        let server = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(120)).await;
                let tombstoned: Vec<dfs_common::ChunkId> = server.chunk_tombstones
                    .iter().map(|e| *e).collect();
                let mut cleaned = 0usize;
                for chunk_id in tombstoned {
                    if !server.storage.has_chunk(&chunk_id) {
                        server.chunk_tombstones.remove(&chunk_id);
                        cleaned += 1;
                    }
                }
                if cleaned > 0 {
                    debug!("chunk_tombstone_cleanup: removed {} stale tombstones", cleaned);
                }
            }
        });
    }
}
