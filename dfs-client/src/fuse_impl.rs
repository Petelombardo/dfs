use anyhow::Result;
use dashmap::DashMap;
use libc;
use dfs_common::{ChunkId, FileMetadata, FileType};
use fuser::{
    FileAttr, FileType as FuseFileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEntry, ReplyStatfs, Request as FuseRequest,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::client::DfsClient;
use crate::locks::LockManager;

/// Returns true if the path is a SQLite database or one of its sidecar/temp files.
/// Used to decide: direct I/O mode, write buffering bypass, immediate metadata flush.
/// Matches .db/.sqlite/.sqlite3 and their -wal/-journal/-shm sidecars,
/// plus _temp variants (e.g. gravity.db_temp used by pihole during gravity updates).
fn is_sqlite_path(path: &str) -> bool {
    path.ends_with(".db")
        || path.ends_with(".sqlite")
        || path.ends_with(".sqlite3")
        || path.ends_with(".db-wal")
        || path.ends_with(".db-journal")
        || path.ends_with(".db-shm")
        || path.ends_with(".db_temp")
        || path.ends_with(".sqlite_temp")
        || path.ends_with(".sqlite3_temp")
}

/// Same as is_sqlite_path but excludes .db-shm, which must NOT use FOPEN_DIRECT_IO
/// because SQLite mmaps it (MAP_SHARED) for WAL index coordination.
fn is_sqlite_direct_io(path: &str) -> bool {
    path.ends_with(".db")
        || path.ends_with(".sqlite")
        || path.ends_with(".sqlite3")
        || path.ends_with(".db-wal")
        || path.ends_with(".db-journal")
        || path.ends_with(".db_temp")
        || path.ends_with(".sqlite_temp")
        || path.ends_with(".sqlite3_temp")
}

const CHUNK_SIZE: usize = 4 * 1024 * 1024; // 4MB chunks

/// A single 4MB-aligned write buffer slot for one chunk.
/// Writes land in the slot at `file_offset % CHUNK_SIZE`.
/// The slot is flushed when it fills (exactly CHUNK_SIZE) or on fsync/release/timer.
#[derive(Clone)]
struct ChunkSlot {
    /// Buffered bytes for this chunk; capacity is at most CHUNK_SIZE
    data: Vec<u8>,
    /// When this slot was last written to
    last_modified: SystemTime,
}

impl ChunkSlot {
    fn new() -> Self {
        Self {
            data: Vec::with_capacity(CHUNK_SIZE),
            last_modified: SystemTime::now(),
        }
    }

    fn is_full(&self) -> bool {
        self.data.len() >= CHUNK_SIZE
    }

    fn is_idle(&self) -> bool {
        self.last_modified.elapsed().unwrap_or_default() > std::time::Duration::from_millis(500)
    }
}

/// Per-inode write state: a set of chunk slots keyed by chunk index (file_offset / CHUNK_SIZE).
/// This replaces the old linear append buffer, letting any offset land in the correct
/// pre-determined slot without requiring sequential ordering from the kernel.
struct InodeWriteState {
    /// Dirty chunk slots: chunk_index -> buffered bytes
    slots: HashMap<u64, ChunkSlot>,
    /// If true, every fsync() must flush immediately (O_SYNC / O_DSYNC was set on open).
    /// If false, fsyncs within the coalescing window are absorbed (DVR / streaming mode).
    sync_on_fsync: bool,
}

impl InodeWriteState {
    fn new(sync_on_fsync: bool) -> Self {
        Self {
            slots: HashMap::new(),
            sync_on_fsync,
        }
    }

    /// Returns the chunk index and intra-chunk offset for a given file byte offset.
    fn chunk_index(file_offset: u64) -> u64 {
        file_offset / CHUNK_SIZE as u64
    }

    fn intra_offset(file_offset: u64) -> usize {
        (file_offset % CHUNK_SIZE as u64) as usize
    }

    /// Write bytes into the appropriate slot(s).  Returns chunk indices that became full.
    fn write_at(&mut self, file_offset: u64, data: &[u8]) -> Vec<u64> {
        let mut full_slots = Vec::new();
        let mut remaining = data;
        let mut cur_offset = file_offset;

        while !remaining.is_empty() {
            let idx = Self::chunk_index(cur_offset);
            let intra = Self::intra_offset(cur_offset);
            let slot = self.slots.entry(idx).or_insert_with(ChunkSlot::new);

            // Grow slot to cover intra_offset (may need zero-fill for sparse-within-chunk)
            if slot.data.len() < intra {
                slot.data.resize(intra, 0u8);
            }

            let space = CHUNK_SIZE - intra;
            let n = remaining.len().min(space);

            // Write or overwrite within this slot
            if intra == slot.data.len() {
                slot.data.extend_from_slice(&remaining[..n]);
            } else {
                // Overwrite within already-buffered region
                let end = intra + n;
                if end > slot.data.len() {
                    slot.data.resize(end, 0u8);
                }
                slot.data[intra..intra + n].copy_from_slice(&remaining[..n]);
            }
            slot.last_modified = SystemTime::now();

            if slot.is_full() {
                full_slots.push(idx);
            }

            remaining = &remaining[n..];
            cur_offset += n as u64;
        }

        full_slots
    }

    /// How many dirty bytes are buffered across all slots
    fn buffered_bytes(&self) -> usize {
        self.slots.values().map(|s| s.data.len()).sum()
    }

    /// Slots that are full (ready for background flush without waiting for fsync)
    fn full_slot_indices(&self) -> Vec<u64> {
        self.slots.iter()
            .filter(|(_, s)| s.is_full())
            .map(|(idx, _)| *idx)
            .collect()
    }

    /// All dirty slot indices, sorted ascending (for ordered flush on fsync/release)
    fn all_slot_indices(&self) -> Vec<u64> {
        let mut indices: Vec<u64> = self.slots.keys().copied().collect();
        indices.sort_unstable();
        indices
    }
}


/// Cheaply-cloneable handle to the fields needed by flush_buffer_async.
/// Extracted so fsync() can clone it and spawn a background flush task without
/// holding a reference to DfsFilesystem (which is !Clone due to &mut self callbacks).
#[derive(Clone)]
struct FlushHandle {
    client: Arc<DfsClient>,
    write_buffers: Arc<DashMap<u64, Arc<Mutex<InodeWriteState>>>>,
    metadata_cache: Arc<DashMap<u64, FileMetadata>>,
    flush_in_flight: Arc<RwLock<Option<Arc<dashmap::DashSet<u64>>>>>,
    last_metadata_update: Arc<DashMap<u64, std::time::Instant>>,
}

impl FlushHandle {
    /// Flush dirty chunk slots for `ino`.
    ///
    /// `force = false` → flush only full slots (background ticker path)
    /// `force = true`  → flush all dirty slots including partial tail (fsync/release path)
    async fn flush_buffer_async(&self, ino: u64, force: bool) -> Result<()> {
        // If a background flush is in-flight for this inode, wait for it to finish
        // before proceeding.  Without this, a force-flush (release/fsync) and a
        // background tick flush can race: both snapshot the same slots, both try to
        // write the same chunk hash, and the second write fails with "already exists".
        if force {
            let in_flight_set = self.flush_in_flight.read().unwrap().as_ref().cloned();
            if let Some(in_flight_set) = in_flight_set {
                let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
                while in_flight_set.contains(&ino) {
                    if tokio::time::Instant::now() >= deadline {
                        break; // don't block forever; proceed and let the write deduplicate
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                }
            }
        }

        // Determine which chunk indices to flush
        let indices_to_flush: Vec<u64> = {
            match self.write_buffers.get(&ino) {
                Some(state_lock) => {
                    let state = state_lock.lock().await;
                    if force {
                        // fsync/release: flush everything including partial tail
                        state.all_slot_indices()
                    } else {
                        // Background tick: flush full slots + idle partial slots.
                        // A partial slot is only eligible for idle-flush if the file has
                        // already moved past it (a higher-indexed slot exists). This prevents
                        // prematurely flushing the first partial slot at recording startup
                        // before the DVR has written enough data to fill the first chunk —
                        // which produces a small first chunk and a visible artifact in the stream.
                        let max_slot_idx = state.slots.keys().copied().max();
                        let mut indices = state.full_slot_indices();
                        for (idx, slot) in &state.slots {
                            if !indices.contains(idx) && slot.is_idle() && !slot.data.is_empty() {
                                // Only flush partial/idle slot if file has progressed past it
                                let file_has_moved_on = max_slot_idx.map(|max| *idx < max).unwrap_or(false);
                                if file_has_moved_on {
                                    indices.push(*idx);
                                }
                            }
                        }
                        indices.sort_unstable();
                        indices
                    }
                }
                None => return Ok(()),
            }
        };

        if indices_to_flush.is_empty() {
            return Ok(());
        }

        // Snapshot all slot data before launching parallel writes.
        // For a partial slot where a higher-indexed slot has already been flushed to the server
        // (i.e. the file has grown past this slot's chunk boundary), we must read-modify-write:
        // fetch the existing chunk from the server, overlay our buffered bytes on top, and write
        // the full combined chunk. Without this, a DVR-style header write (small write to offset 0
        // at recording start, followed by the full stream body) produces a 12032-byte stub chunk
        // that replaces the correct 4MB first chunk when the file is closed.
        let max_flushed_idx: Option<u64> = {
            // The highest chunk index already committed to the server = max chunk in metadata
            // that is NOT currently in the write buffer (already flushed).
            let meta_chunk_count = self.metadata_cache.get(&ino)
                .map(|m| m.chunk_locations.len().max(m.chunks.len()) as u64)
                .unwrap_or(0);
            if meta_chunk_count > 0 { Some(meta_chunk_count - 1) } else { None }
        };

        let mut slots_to_write: Vec<(u64, Vec<u8>, u64)> = Vec::new(); // (chunk_idx, data, file_offset)
        for chunk_idx in &indices_to_flush {
            if let Some(state_lock) = self.write_buffers.get(&ino) {
                let state = state_lock.lock().await;
                if let Some(slot) = state.slots.get(chunk_idx) {
                    if !slot.data.is_empty() {
                        let file_offset = chunk_idx * CHUNK_SIZE as u64;
                        let slot_data = slot.data.clone();
                        let slot_len = slot_data.len();

                        // If this slot is partial AND higher chunks are already on the server,
                        // the slot holds an overlay (e.g. a header) written to a chunk that was
                        // previously flushed as a full 4MB chunk. Use PatchChunk to send only
                        // the changed bytes to each replica — no full chunk transfer needed.
                        let needs_patch = slot_len < CHUNK_SIZE
                            && max_flushed_idx.map(|max| *chunk_idx <= max).unwrap_or(false);

                        if needs_patch {
                            info!("flush_buffer_async: partial slot {} ({} bytes) — using PatchChunk",
                                  chunk_idx, slot_len);
                            let meta = self.metadata_cache.get(&ino).map(|m| m.clone());
                            let patched = if let Some(meta) = meta {
                                let chunk_idx_usize = *chunk_idx as usize;
                                let old_location_opt = meta.chunk_locations.get(chunk_idx_usize).cloned()
                                    .or_else(|| {
                                        // Legacy: no chunk_locations — can't patch without node list
                                        None
                                    });
                                if let Some(old_location) = old_location_opt {
                                    match self.client.patch_chunk_on_replicas(
                                        old_location.chunk_id,
                                        file_offset,  // chunk_file_offset = start of this chunk
                                        0,            // intra_offset: overlay always starts at byte 0 for DVR header
                                        slot_data.clone(),
                                        &old_location,
                                    ).await {
                                        Ok(new_location) => {
                                            info!("flush_buffer_async: PatchChunk slot {} succeeded: {} -> {}",
                                                  chunk_idx, old_location.chunk_id, new_location.chunk_id);
                                            // Update metadata cache with new chunk location in-place
                                            if let Some(mut meta_entry) = self.metadata_cache.get_mut(&ino) {
                                                if let Some(loc) = meta_entry.chunk_locations.get_mut(chunk_idx_usize) {
                                                    *loc = new_location.clone();
                                                }
                                                if let Some(id) = meta_entry.chunks.get_mut(chunk_idx_usize) {
                                                    *id = new_location.chunk_id;
                                                }
                                            }
                                            // Skip adding to slots_to_write — patch already committed
                                            true
                                        }
                                        Err(e) => {
                                            warn!("flush_buffer_async: PatchChunk failed for slot {}: {} — falling back to full write", chunk_idx, e);
                                            false
                                        }
                                    }
                                } else {
                                    warn!("flush_buffer_async: no chunk_location for slot {} — cannot patch, falling back to full write", chunk_idx);
                                    false
                                }
                            } else {
                                false
                            };

                            if !patched {
                                slots_to_write.push((*chunk_idx, slot_data, file_offset));
                            }
                        } else {
                            slots_to_write.push((*chunk_idx, slot_data, file_offset));
                        }
                    }
                }
            }
        }

        if slots_to_write.is_empty() {
            return Ok(());
        }

        info!("flush_buffer_async: flushing {} chunks in parallel for inode {}", slots_to_write.len(), ino);

        // Flush all slots in parallel — each chunk write is independent.
        // Results come back in arbitrary order; we sort by chunk_idx for metadata consistency.
        let handles: Vec<_> = slots_to_write.iter().map(|(chunk_idx, slot_data, file_offset)| {
            let client = self.client.clone();
            let data = slot_data.clone();
            let offset = *file_offset;
            let idx = *chunk_idx;
            tokio::spawn(async move {
                info!("flush_buffer_async: writing chunk {} ({} bytes at offset {})", idx, data.len(), offset);
                let result = client.write_data_with_cache(&data, ino, offset).await;
                result.map(|(_, _, locs)| (idx, locs))
            })
        }).collect();

        let results = futures::future::join_all(handles).await;

        // Process results: remove flushed slots, collect locations.
        let mut all_locations: Vec<dfs_common::ChunkLocation> = Vec::new();
        let mut first_err: Option<anyhow::Error> = None;

        for (join_result, (chunk_idx, _, _)) in results.into_iter().zip(slots_to_write.iter()) {
            match join_result {
                Ok(Ok((_, locations_opt))) => {
                    // Remove slot now that it's safely on disk.
                    if let Some(state_lock) = self.write_buffers.get(&ino) {
                        let mut state = state_lock.lock().await;
                        state.slots.remove(chunk_idx);
                    }
                    if let Some(locations) = locations_opt {
                        all_locations.extend(locations);
                    }
                }
                Ok(Err(e)) => {
                    if first_err.is_none() { first_err = Some(e); }
                }
                Err(e) => {
                    if first_err.is_none() { first_err = Some(anyhow::anyhow!("flush task panicked: {}", e)); }
                }
            }
        }

        if let Some(e) = first_err {
            return Err(e);
        }

        if all_locations.is_empty() {
            return Ok(());
        }

        // Update metadata cache: insert new chunk_locations in file-offset order,
        // deduplicate by file_offset (supersedes chunk_id dedup), and recalculate file size.
        {
            if let Some(mut meta) = self.metadata_cache.get_mut(&ino) {
                for loc in &all_locations {
                    if !meta.chunk_locations.iter().any(|l| l.chunk_id == loc.chunk_id) {
                        // Replace any existing entry at the same file_offset (from a previous
                        // partial flush of this slot). Content-addressed hashes differ when the
                        // same slot is flushed twice (partial then full), so chunk_id dedup alone
                        // misses this — we must also dedup by file_offset.
                        if let Some(offset) = loc.file_offset {
                            if let Some(pos) = meta.chunk_locations.iter().position(|l| l.file_offset == Some(offset)) {
                                let old_id = meta.chunk_locations[pos].chunk_id;
                                meta.chunk_locations[pos] = loc.clone();
                                // Keep legacy arrays consistent
                                if let Some(p) = meta.chunks.iter().position(|&id| id == old_id) {
                                    meta.chunks[p] = loc.chunk_id;
                                    meta.chunk_sizes[p] = loc.size as u64;
                                }
                                continue;
                            }
                        }
                        meta.chunks.push(loc.chunk_id);
                        meta.chunk_sizes.push(loc.size as u64);
                        meta.chunk_locations.push(loc.clone());
                    }
                }
                // File size = end of last chunk_location
                if let Some(last) = meta.chunk_locations.iter()
                    .filter_map(|l| l.file_offset.map(|o| o + l.size as u64))
                    .reduce(u64::max)
                {
                    meta.size = last;
                }
                meta.modified_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
            }
        }

        let meta_to_persist = self.metadata_cache.get(&ino).map(|m| m.clone());
        if let Some(meta) = meta_to_persist {
            if force {
                // release/fsync: wait for leader confirmation before returning.
                let _ = self.client.flush_metadata_sync(&meta).await;
            } else {
                // Background tick: only enqueue if enough time has passed since last
                // metadata update for this inode. The queue deduplicates by file_id so
                // only the latest snapshot is ever in-flight per file, but we still
                // rate-limit enqueues to avoid hammering the leader on every chunk flush.
                const METADATA_FLUSH_INTERVAL_SECS: u64 = 5;
                let should_enqueue = match self.last_metadata_update.get(&ino) {
                    None => true,
                    Some(last) => last.elapsed() >= std::time::Duration::from_secs(METADATA_FLUSH_INTERVAL_SECS),
                };
                if should_enqueue {
                    self.last_metadata_update.insert(ino, std::time::Instant::now());
                    self.client.enqueue_metadata(&meta).await;
                }
            }
        }

        Ok(())
    }
}

/// FUSE filesystem implementation for DFS
pub struct DfsFilesystem {
    /// Client for communicating with DFS cluster
    client: Arc<DfsClient>,

    /// Metadata cache: inode -> FileMetadata
    metadata_cache: Arc<DashMap<u64, FileMetadata>>,

    /// Path to inode mapping
    path_to_inode: Arc<RwLock<HashMap<String, u64>>>,

    /// Next available inode number
    next_inode: Arc<RwLock<u64>>,

    /// Root inode is always 1 (FUSE convention)
    root_inode: u64,

    /// Tokio runtime handle for async operations
    runtime: tokio::runtime::Handle,

    /// Write counter per inode for batching metadata updates
    write_counters: Arc<RwLock<HashMap<u64, usize>>>,

    /// Enable write-behind buffering
    write_buffer_enabled: bool,

    /// Per-inode write state: chunk slots keyed by chunk index, plus sync policy flag.
    /// DashMap provides lock-free reads and fine-grained locking per inode.
    write_buffers: Arc<DashMap<u64, Arc<Mutex<InodeWriteState>>>>,

    /// Last metadata update timestamp per inode for batching
    /// Prevents excessive metadata updates during continuous writes
    last_metadata_update: Arc<DashMap<u64, std::time::Instant>>,

    /// Last read chunk cache: (ino, chunk_index, data)
    /// Prevents re-fetching same 4MB chunk for multiple 128KB FUSE reads
    last_chunk_cache: Arc<RwLock<Option<(u64, usize, Vec<u8>)>>>,

    /// Track last warming offset per inode to throttle replica cache warming
    /// Prevents excessive warming overhead on files with many small chunks
    last_warm_offset: Arc<DashMap<u64, u64>>,

    /// Chunk offset map cache: inode -> Vec<(offset, size)>
    /// Prevents O(n) iteration through all chunks on every read
    /// Invalidated when file metadata changes (size/chunks)
    chunk_offset_cache: Arc<DashMap<u64, (u64, usize, Vec<(usize, usize)>)>>, // (file_size, chunk_count, offsets)

    /// Directory listing cache: path -> (entries, timestamp)
    /// Cache directory listings for 5 seconds to avoid repeated scans
    dir_cache: Arc<DashMap<String, (Vec<FileMetadata>, std::time::Instant)>>,

    /// Filesystem stats cache: (total, free, avail, timestamp)
    /// Cache statfs results for 30 seconds to avoid repeated expensive queries
    statfs_cache: Arc<RwLock<Option<(u64, u64, u64, std::time::Instant)>>>,

    /// Lock manager for byte-range locks
    lock_manager: Arc<LockManager>,

    /// Write buffer flush threshold in bytes (dynamic, queried from cluster)
    buffer_flush_threshold: usize,

    /// Count of write-mode open file handles per inode.
    /// Used to guard the write buffer in flush(): a flush() triggered by a read-only
    /// close must not touch the write buffer of a concurrently writing fd.
    write_open_counts: Arc<DashMap<u64, usize>>,

    /// High-water mark of reported file size per inode.
    /// Prevents getattr from reporting a smaller size during the window between a
    /// slot being flushed (removed from write_buffers) and the metadata being committed.
    /// Cleared on release() once the file is fully closed.
    size_high_water: Arc<DashMap<u64, u64>>,

    /// Shared reference to the background flusher's in-flight set.
    /// Set by the background flusher task after spawn; flush_buffer_async (fsync/close)
    /// waits for any in-flight background flush to complete before sending its own flush
    /// to avoid concurrent flushes that would produce OffsetMismatch.
    flush_in_flight: Arc<RwLock<Option<Arc<dashmap::DashSet<u64>>>>>,

    /// Cloneable handle used by fsync() to spawn background flush tasks.
    flush_handle: FlushHandle,
}

impl DfsFilesystem {
    /// Create a new DFS filesystem with an explicit runtime handle
    pub fn new_with_runtime(
        cluster_nodes: Vec<SocketAddr>,
        write_buffer_enabled: bool,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self> {
        let client = Arc::new(DfsClient::new(cluster_nodes)?);

        // Query cluster for chunk size configuration
        let chunk_size_mb = runtime.block_on(async {
            client.get_cluster_chunk_size().await
        }).unwrap_or_else(|e| {
            warn!("Failed to query cluster chunk size, using default 4MB: {}", e);
            4  // Default to 4MB if query fails
        });
        // Buffer flush threshold = ceil(32MB / chunk_size) × chunk_size.
        // This keeps ~32MB of data buffered in-flight regardless of chunk size.
        let chunk_size_bytes = chunk_size_mb * 1024 * 1024;
        let buffer_flush_threshold = chunk_size_bytes *
            ((32 * 1024 * 1024 + chunk_size_bytes - 1) / chunk_size_bytes);
        info!("Write buffer threshold: {}MB ({} chunks × {}MB)",
              buffer_flush_threshold / (1024 * 1024),
              buffer_flush_threshold / chunk_size_bytes,
              chunk_size_mb);

        // Populate addr_to_node_id immediately so the very first write gets real node IDs.
        if let Err(e) = runtime.block_on(client.refresh_cluster_nodes()) {
            tracing::warn!("Initial cluster node refresh failed: {}", e);
        }

        // Start background task to periodically refresh cluster nodes.
        // Uses exponential backoff on failure: 30s → 60s → 120s → 240s (cap),
        // resetting to 30s on the next success. This prevents hammering the network
        // when all nodes are temporarily unreachable (e.g. cluster restart).
        let client_clone = client.clone();
        runtime.spawn(async move {
            const BASE_INTERVAL: u64 = 30;
            const MAX_INTERVAL: u64 = 240;
            let mut interval_secs = BASE_INTERVAL;
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(interval_secs)).await;
                if let Err(e) = client_clone.refresh_cluster_nodes().await {
                    interval_secs = (interval_secs * 2).min(MAX_INTERVAL);
                    tracing::warn!("Failed to refresh cluster nodes (retrying in {}s): {}", interval_secs, e);
                } else {
                    interval_secs = BASE_INTERVAL;
                }
            }
        });

        // Start background metadata queue worker.
        client.start_metadata_queue_worker(&runtime);

        let metadata_cache = Arc::new(DashMap::<u64, FileMetadata>::new());
        let path_to_inode = Arc::new(RwLock::new(HashMap::<String, u64>::new()));
        let next_inode = Arc::new(RwLock::new(2)); // Start at 2, root is 1
        let write_buffers_for_cleanup = Arc::new(DashMap::<u64, Arc<Mutex<InodeWriteState>>>::new());
        let flush_in_flight_shared: Arc<RwLock<Option<Arc<dashmap::DashSet<u64>>>>> =
            Arc::new(RwLock::new(None));
        let last_metadata_update_shared: Arc<DashMap<u64, std::time::Instant>> =
            Arc::new(DashMap::new());

        // Start background task to flush expired write buffers (if buffering enabled)
        if write_buffer_enabled {
            let write_buffers_clone = write_buffers_for_cleanup.clone();
            let client_for_cleanup = client.clone();
            let metadata_cache_for_cleanup = metadata_cache.clone();
            // in_flight: tracks inodes with an active background flush task.
            // Prevents launching a second flush while the first is still in flight,
            // which would produce OffsetMismatch. Also used by flush_buffer_async
            // (fsync/close) to wait for any in-flight background flush to complete.
            let in_flight: Arc<dashmap::DashSet<u64>> = Arc::new(dashmap::DashSet::new());
            // Share the in_flight set with flush_buffer_async via the struct field
            *flush_in_flight_shared.write().unwrap() = Some(in_flight.clone());

            let flush_handle_for_bg = FlushHandle {
                client: client_for_cleanup.clone(),
                write_buffers: write_buffers_clone.clone(),
                metadata_cache: metadata_cache_for_cleanup.clone(),
                flush_in_flight: flush_in_flight_shared.clone(),
                last_metadata_update: last_metadata_update_shared.clone(),
            };
            runtime.spawn(async move {
                let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(100));
                loop {
                    interval.tick().await;

                    // Find inodes with full chunk slots ready for background flush.
                    // A full slot (exactly 4MB) is safe to flush without waiting for fsync.
                    // Partial slots are also flushed if they haven't been written to for 2s
                    // (file may have stopped growing — drain it rather than holding memory).
                    let flush_inodes: Vec<u64> = {
                        let mut ready = Vec::new();
                        for entry in write_buffers_clone.iter() {
                            let ino = *entry.key();
                            if in_flight.contains(&ino) { continue; }
                            let state = entry.value().lock().await;
                            let has_full = !state.full_slot_indices().is_empty();
                            // A partial/idle slot is only eligible if the file has moved past it
                            // (higher-indexed slot exists). See flush_buffer_async for rationale.
                            let max_slot = state.slots.keys().copied().max();
                            let has_idle = state.slots.iter().any(|(idx, s)| {
                                s.is_idle() && !s.data.is_empty()
                                    && max_slot.map(|max| *idx < max).unwrap_or(false)
                            });
                            if has_full || has_idle {
                                ready.push(ino);
                            }
                        }
                        ready
                    };

                    for ino in flush_inodes {
                        in_flight.insert(ino);
                        let handle = flush_handle_for_bg.clone();
                        let in_flight_task = in_flight.clone();

                        tokio::spawn(async move {
                            // force=false: flush full slots + idle partial slots
                            if let Err(e) = handle.flush_buffer_async(ino, false).await {
                                tracing::error!("Background flush failed for inode {}: {}", ino, e);
                            }
                            in_flight_task.remove(&ino);
                        });
                    }
                }
            });
        }

        // Create root directory metadata
        let root_metadata = FileMetadata {
            id: dfs_common::FileId::new(),
            path: "/".to_string(),
            size: 0,
            chunks: Vec::new(),
            chunk_sizes: Vec::new(),
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            modified_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            mode: 0o755,
            uid: 0,
            gid: 0,
            file_type: FileType::Directory,
            chunk_locations: Vec::new(),
            write_seq: 0,
        };

        metadata_cache.insert(1, root_metadata);
        path_to_inode.write().unwrap().insert("/".to_string(), 1);

        // Build FlushHandle before moving fields into the struct
        let flush_handle = FlushHandle {
            client: client.clone(),
            write_buffers: write_buffers_for_cleanup.clone(),
            metadata_cache: metadata_cache.clone(),
            flush_in_flight: flush_in_flight_shared.clone(),
            last_metadata_update: last_metadata_update_shared.clone(),
        };

        Ok(Self {
            client,
            metadata_cache,
            path_to_inode,
            next_inode,
            root_inode: 1,
            runtime,
            write_counters: Arc::new(RwLock::new(HashMap::new())),
            write_buffer_enabled,
            write_buffers: write_buffers_for_cleanup,
            last_metadata_update: last_metadata_update_shared,
            last_chunk_cache: Arc::new(RwLock::new(None)),
            last_warm_offset: Arc::new(DashMap::new()),
            chunk_offset_cache: Arc::new(DashMap::new()),
            dir_cache: Arc::new(DashMap::new()),
            statfs_cache: Arc::new(RwLock::new(None)),
            lock_manager: Arc::new(LockManager::new()),
            buffer_flush_threshold,
            write_open_counts: Arc::new(DashMap::new()),
            size_high_water: Arc::new(DashMap::new()),
            flush_in_flight: flush_in_flight_shared,
            flush_handle,
        })
    }

    /// Create a new DFS filesystem (deprecated - use new_with_runtime)
    #[allow(dead_code)]
    pub fn new(cluster_nodes: Vec<SocketAddr>, write_buffer_enabled: bool) -> Result<Self> {
        // This version tries to get the current runtime handle
        // Only works if called from within a tokio runtime context
        let runtime = tokio::runtime::Handle::current();
        Self::new_with_runtime(cluster_nodes, write_buffer_enabled, runtime)
    }

    /// Execute an async operation in a blocking context
    /// Uses block_in_place to allow blocking within an async runtime
    fn block_on<F, T>(&self, future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        // NOTE: We can't use block_in_place because FUSE callbacks don't run on tokio worker threads
        // Just block_on directly using the runtime handle
        self.runtime.block_on(future)
    }

    /// Safely update metadata cache, checking if there's an active write in progress
    /// Returns true if the metadata was updated, false if skipped due to active write
    fn safe_metadata_update(&self, ino: u64, metadata: FileMetadata) -> bool {
        // Check if there's an active write - either buffered or in-progress
        let (has_buffer, has_counter, current_size) = self.runtime.block_on(async {
            // Check for write buffer
            let has_buf = if self.write_buffer_enabled {
                self.write_buffers.contains_key(&ino)
            } else {
                false
            };

            // Check write counter (indicates recent writes)
            let counters = self.write_counters.read().unwrap();
            let has_cnt = counters.contains_key(&ino);

            // Get current cached size
            let cur_size = self.metadata_cache.get(&ino).map(|m| m.size).unwrap_or(0);

            (has_buf, has_cnt, cur_size)
        });

        let has_active_write = has_buffer || has_counter;

        if has_active_write {
            debug!("SKIPPING metadata update for ino={}: has_buffer={}, has_counter={}, cached_size={}, server_size={}",
                  ino, has_buffer, has_counter, current_size, metadata.size);
            false
        } else {
            debug!("UPDATING metadata for ino={}: cached_size={}, server_size={}",
                  ino, current_size, metadata.size);
            self.client.seed_write_seq(metadata.id, metadata.write_seq);
            self.metadata_cache.insert(ino, metadata);
            true
        }
    }

    /// Check if metadata should be updated based on time-based batching
    /// Returns true if:
    /// - force_update is true (e.g., on close/fsync), OR
    /// - More than 2 seconds have elapsed since last update, OR
    /// - This is the first update for this inode
    fn should_update_metadata(&self, ino: u64, force_update: bool) -> bool {
        if force_update {
            return true;
        }

        const METADATA_UPDATE_INTERVAL_SECS: u64 = 10;

        match self.last_metadata_update.get(&ino) {
            None => true,  // First update
            Some(last) => {
                let elapsed = last.elapsed();
                elapsed >= std::time::Duration::from_secs(METADATA_UPDATE_INTERVAL_SECS)
            }
        }
    }

    /// Update the last metadata update timestamp for an inode
    fn record_metadata_update(&self, ino: u64) {
        self.last_metadata_update.insert(ino, std::time::Instant::now());
    }

    /// Flush the write buffer for `ino`. Delegates to FlushHandle::flush_buffer_async.
    async fn flush_buffer_async(&self, ino: u64, force: bool) -> Result<()> {
        self.flush_handle.flush_buffer_async(ino, force).await
    }

    /// Convert FileMetadata to FUSE FileAttr
    fn metadata_to_attr(&self, ino: u64, metadata: &FileMetadata) -> FileAttr {
        Self::metadata_to_attr_static(ino, metadata)
    }

    /// Convert FileMetadata to FUSE FileAttr (static version for async contexts)
    fn metadata_to_attr_static(ino: u64, metadata: &FileMetadata) -> FileAttr {
        let kind = match metadata.file_type {
            FileType::RegularFile => FuseFileType::RegularFile,
            FileType::Directory => FuseFileType::Directory,
            FileType::Symlink => FuseFileType::Symlink,
        };

        FileAttr {
            ino,
            size: metadata.size,
            blocks: (metadata.size + 511) / 512, // 512-byte blocks
            atime: UNIX_EPOCH + Duration::from_secs(metadata.modified_at),
            mtime: UNIX_EPOCH + Duration::from_secs(metadata.modified_at),
            ctime: UNIX_EPOCH + Duration::from_secs(metadata.created_at),
            crtime: UNIX_EPOCH + Duration::from_secs(metadata.created_at),
            kind,
            perm: metadata.mode as u16,
            nlink: 1,
            uid: metadata.uid,
            gid: metadata.gid,
            rdev: 0,
            blksize: 4 * 1024 * 1024, // 4MB chunk size
            flags: 0,
        }
    }

    /// Get or allocate inode for a path
    fn get_or_create_inode(&self, path: &str) -> u64 {
        let path_map = self.path_to_inode.read().unwrap();
        if let Some(&ino) = path_map.get(path) {
            return ino;
        }
        drop(path_map);

        // Allocate new inode
        let mut next = self.next_inode.write().unwrap();
        let ino = *next;
        *next += 1;
        drop(next);

        self.path_to_inode
            .write()
            .unwrap()
            .insert(path.to_string(), ino);

        ino
    }

    /// Get path from parent inode and name
    fn get_path_from_parent(&self, parent: u64, name: &OsStr) -> Option<String> {
        let parent_metadata = self.metadata_cache.get(&parent)?;
        let name_str = name.to_str()?;

        let parent_path = &parent_metadata.path;
        let full_path = if parent_path == "/" {
            format!("/{}", name_str)
        } else {
            format!("{}/{}", parent_path, name_str)
        };

        Some(full_path)
    }
}

impl Filesystem for DfsFilesystem {
    fn init(
        &mut self,
        _req: &FuseRequest,
        config: &mut fuser::KernelConfig,
    ) -> Result<(), libc::c_int> {
        info!("Initializing DFS filesystem");

        // Enable aggressive kernel read-ahead for sequential reads (DVR streaming)
        // This tells the kernel to read ahead up to 16MB for sequential access patterns
        config.set_max_readahead(16 * 1024 * 1024);
        info!("Set max_readahead to 16MB for sequential streaming");

        // Enable POSIX file locking - tell kernel to use our setlk/getlk implementations
        // instead of handling locks in the kernel
        match config.add_capabilities(fuser::consts::FUSE_POSIX_LOCKS) {
            Ok(()) => {
                info!("FUSE_POSIX_LOCKS capability enabled");
                Ok(())
            }
            Err(_) => {
                error!("Failed to enable FUSE_POSIX_LOCKS capability");
                Err(libc::EIO)
            }
        }
    }

    fn destroy(&mut self) {
        info!("DFS filesystem destroy: flushing all write buffers and metadata queue");

        let write_buffers = self.write_buffers.clone();
        let flush_in_flight = self.flush_in_flight.clone();
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();

        let flush_handle = FlushHandle {
            client: client.clone(),
            write_buffers: write_buffers.clone(),
            metadata_cache: metadata_cache.clone(),
            flush_in_flight: flush_in_flight.clone(),
            last_metadata_update: self.last_metadata_update.clone(),
        };

        self.block_on(async move {
            // Step 1: Force-flush all dirty write buffers.
            let inodes: Vec<u64> = write_buffers.iter().map(|e| *e.key()).collect();
            if !inodes.is_empty() {
                info!("destroy: force-flushing {} open write buffers", inodes.len());
                let handles: Vec<_> = inodes.into_iter().map(|ino| {
                    let h = flush_handle.clone();
                    tokio::spawn(async move {
                        if let Err(e) = h.flush_buffer_async(ino, true).await {
                            error!("destroy: flush failed for inode {}: {}", ino, e);
                        }
                    })
                }).collect();
                for h in handles {
                    let _ = h.await;
                }
            }

            // Step 2: Wait for any background in-flight flushes to drain.
            if let Some(in_flight) = flush_in_flight.read().unwrap().as_ref() {
                let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
                while !in_flight.is_empty() {
                    if tokio::time::Instant::now() > deadline {
                        warn!("destroy: timed out waiting for in-flight flushes");
                        break;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
            }

            // Step 3: Wait for the metadata queue to drain completely.
            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
            loop {
                if client.metadata_queue.is_empty().await { break; }
                if tokio::time::Instant::now() > deadline {
                    warn!("destroy: timed out waiting for metadata queue to drain");
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            }

            info!("destroy: all buffers flushed and metadata queue drained");
        });
    }

    fn lookup(&mut self, _req: &FuseRequest, parent: u64, name: &OsStr, reply: ReplyEntry) {
        debug!("lookup: parent={}, name={:?}", parent, name);

        let path = match self.get_path_from_parent(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Check if we have fresh metadata from a recent readdir — if so, skip the
        // server round-trip entirely.  readdir populates both metadata_cache and
        // last_metadata_update; a fresh entry here means we just listed this directory
        // and can trust the cached metadata for the lookup TTL window.
        let cached = {
            let path_map = self.path_to_inode.read().unwrap();
            if let Some(&ino) = path_map.get(&path) {
                let fresh = self.last_metadata_update.get(&ino)
                    .map(|t| t.elapsed() < std::time::Duration::from_secs(5))
                    .unwrap_or(false);
                if fresh {
                    self.metadata_cache.get(&ino).map(|m| (ino, m.clone()))
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some((ino, metadata)) = cached {
            let attr = self.metadata_to_attr(ino, &metadata);
            reply.entry(&Duration::from_secs(1), &attr, 0);
            return;
        }

        // Cache miss or stale — fetch from cluster with conditional GET.
        // Use runtime.spawn (non-blocking) so we never block the FUSE dispatch
        // thread waiting for a runtime thread.  When all worker threads are busy
        // (e.g. 44 concurrent getattr tasks after a readdir) a block_on here would
        // deadlock — the FUSE thread parks waiting for a thread that will never free.
        let cached_modified_at = {
            let path_map = self.path_to_inode.read().unwrap();
            if let Some(&ino) = path_map.get(&path) {
                self.metadata_cache.get(&ino).map(|m| m.modified_at)
            } else {
                None
            }
        };

        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let next_inode = self.next_inode.clone();
        let last_metadata_update = self.last_metadata_update.clone();

        self.runtime.spawn(async move {
            let result = client.get_file_metadata_conditional(&path, cached_modified_at).await;

            match result {
                Ok(Some(metadata)) => {
                    // Metadata was modified or first fetch — update cache.
                    let ino = {
                        let path_map = path_to_inode.read().unwrap();
                        if let Some(&existing) = path_map.get(&path) {
                            existing
                        } else {
                            drop(path_map);
                            let mut next = next_inode.write().unwrap();
                            let ino = *next;
                            *next += 1;
                            drop(next);
                            path_to_inode.write().unwrap().insert(path.clone(), ino);
                            ino
                        }
                    };
                    client.seed_write_seq(metadata.id, metadata.write_seq);
                    metadata_cache.insert(ino, metadata.clone());
                    last_metadata_update.insert(ino, std::time::Instant::now());

                    let attr = DfsFilesystem::metadata_to_attr_static(ino, &metadata);
                    reply.entry(&Duration::from_secs(1), &attr, 0);
                }
                Ok(None) => {
                    // Either file not found OR metadata not modified (cache still valid).
                    if cached_modified_at.is_some() {
                        let path_map = path_to_inode.read().unwrap();
                        if let Some(&ino) = path_map.get(&path) {
                            if let Some(metadata) = metadata_cache.get(&ino) {
                                debug!("Using cached metadata for {} (not modified)", path);
                                let attr = DfsFilesystem::metadata_to_attr_static(ino, &*metadata);
                                reply.entry(&Duration::from_secs(1), &attr, 0);
                                return;
                            }
                        }
                    }
                    // File not found
                    reply.error(libc::ENOENT);
                }
                Err(e) => {
                    error!("Failed to lookup {}: {}", path, e);
                    reply.error(libc::EIO);
                }
            }
        });
    }

    fn open(&mut self, _req: &FuseRequest, ino: u64, flags: i32, reply: fuser::ReplyOpen) {
        info!("open: ino={}", ino);

        // Track write-mode opens so flush() can skip the write buffer for read-only closes.
        // O_RDONLY == 0; any flag with the low two bits set (O_WRONLY=1, O_RDWR=2) is a write open.
        let is_write = (flags & libc::O_ACCMODE) != libc::O_RDONLY;
        if is_write {
            *self.write_open_counts.entry(ino).or_insert(0) += 1;

            // Seed the write_seq counter from the server's stored value so that
            // writes after a client restart continue from the correct sequence,
            // not from 0 (which would be treated as stale by the server).
            if let Some(meta) = self.metadata_cache.get(&ino) {
                self.client.seed_write_seq(meta.id, meta.write_seq);
            }

            // O_SYNC / O_DSYNC: the caller wants every fsync() to be honored immediately.
            // SQLite, databases, and write-journaling apps use this. DVR/streaming apps don't.
            // We propagate this flag into the InodeWriteState so fsync() knows whether to
            // coalesce (DVR mode) or flush immediately (database mode).
            let sync_on_fsync = (flags & (libc::O_SYNC | libc::O_DSYNC)) != 0;
            if sync_on_fsync {
                info!("open: ino={} opened with O_SYNC/O_DSYNC — fsyncs will flush immediately", ino);
            }
            if self.write_buffer_enabled {
                // If this is the first writer (count was 0 before incrementing above),
                // discard any stale buffer left over from a previous session — e.g. after
                // a client restart where release() never ran. Without this, the background
                // flusher immediately flushes the stale data as a small first chunk.
                let is_first_writer = self.write_open_counts.get(&ino).map(|c| *c == 1).unwrap_or(true);
                if is_first_writer {
                    self.write_buffers.remove(&ino);
                }

                // Create or update the InodeWriteState for this inode.
                // If already exists (multiple writers), set sync_on_fsync if ANY fd requests it.
                let state_entry = self.write_buffers
                    .entry(ino)
                    .or_insert_with(|| Arc::new(Mutex::new(InodeWriteState::new(sync_on_fsync))));
                if sync_on_fsync {
                    // Upgrade existing state to sync mode
                    let state_arc = state_entry.clone();
                    let _ = self.runtime.block_on(async move {
                        state_arc.lock().await.sync_on_fsync = true;
                    });
                }
            }
        }

        // Check if this is a SQLite database file by looking up its path
        let is_sqlite = self.metadata_cache.get(&ino)
            .map(|m| is_sqlite_direct_io(&m.path))
            .unwrap_or(false);

        if is_sqlite {
            // For SQLite files: Use direct I/O to bypass page cache
            // This ensures lock consistency and prevents cache coherency issues
            info!("open: ino={} - SQLite database detected, using direct I/O", ino);
            reply.opened(0, fuser::consts::FOPEN_DIRECT_IO);
        } else {
            // For regular files: Use page cache for better performance
            reply.opened(0, fuser::consts::FOPEN_KEEP_CACHE);
        }
    }

    fn getattr(&mut self, _req: &FuseRequest, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        debug!("getattr: ino={}", ino);

        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let write_buffers = self.write_buffers.clone();
        let write_buffer_enabled = self.write_buffer_enabled;
        let size_high_water = self.size_high_water.clone();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            let metadata = metadata_cache.get(&ino).map(|m| m.clone());

            if let Some(mut metadata) = metadata {
                if metadata.file_type == FileType::RegularFile {
                    // Only hit the server if the cached metadata is more than 5 seconds old.
                    // getattr is called every 1s by the kernel for open files; querying the
                    // server on every call generates a connection storm under playback.
                    // 5s is fast enough to notice a file growing (live DVR) without flooding.
                    let should_refresh = match last_metadata_update.get(&ino) {
                        None => true,
                        Some(t) => t.elapsed() >= std::time::Duration::from_secs(5),
                    };

                    if should_refresh {
                        if let Ok(Some(fresh)) = client.get_file_metadata(&metadata.path).await {
                            let server_is_newer = fresh.modified_at > metadata.modified_at
                                || (fresh.modified_at == metadata.modified_at
                                    && (fresh.size > metadata.size
                                        || fresh.chunks.len() > metadata.chunks.len()));
                            if server_is_newer {
                                debug!("getattr: metadata updated: size {} -> {}, chunks {} -> {}",
                                       metadata.size, fresh.size,
                                       metadata.chunks.len(), fresh.chunks.len());
                                client.seed_write_seq(fresh.id, fresh.write_seq);
                                metadata_cache.insert(ino, fresh.clone());
                                metadata = fresh;
                            }
                        }
                        last_metadata_update.insert(ino, std::time::Instant::now());
                    }

                    // For files with an active write buffer, the true EOF is further ahead
                    // than the last committed flush position in metadata.size. Report the
                    // buffer's logical end so that Kodi/players seeking in a live recording
                    // see the correct (current) file size instead of a stale flushed size.
                    // Without this, a seek to the "end" of a live file may land beyond what
                    // getattr reports, causing the player to stall waiting for the file to
                    // appear to grow.
                    if write_buffer_enabled {
                        // Compute the logical end of any buffered-but-not-yet-flushed data
                        let buffered_end = if let Some(state_lock) = write_buffers.get(&ino) {
                            let state = state_lock.lock().await;
                            state.slots.iter()
                                .map(|(idx, slot)| idx * CHUNK_SIZE as u64 + slot.data.len() as u64)
                                .max()
                                .unwrap_or(0)
                        } else { 0 };

                        // Apply high-water mark: never report a size smaller than previously
                        // reported. This prevents the visible size oscillating down during the
                        // window between a slot being flushed and metadata being committed.
                        let hwm = size_high_water.get(&ino).map(|v| *v).unwrap_or(0);
                        let reported = metadata.size.max(buffered_end).max(hwm);
                        if reported > metadata.size {
                            metadata.size = reported;
                        }
                        // Update high-water mark
                        if reported > hwm {
                            size_high_water.insert(ino, reported);
                        }
                    }
                }

                let attr = DfsFilesystem::metadata_to_attr_static(ino, &metadata);
                // Use a short TTL for files with an active write buffer (live recordings)
                // so the kernel re-asks us promptly as the file grows. For static files,
                // 5s is fine — it matches our server refresh rate.
                let ttl = if write_buffer_enabled && write_buffers.contains_key(&ino) {
                    Duration::from_millis(500)
                } else {
                    Duration::from_secs(5)
                };
                reply.attr(&ttl, &attr);
            } else {
                reply.error(libc::ENOENT);
            }
        });
    }

    fn read(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        // Clone Arc-wrapped fields for async task
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let write_buffers = self.write_buffers.clone();
        let write_buffer_enabled = self.write_buffer_enabled;
        let buffer_flush_threshold = self.buffer_flush_threshold;
        let last_metadata_update = self.last_metadata_update.clone();
        let last_warm_offset = self.last_warm_offset.clone();
        let chunk_offset_cache = self.chunk_offset_cache.clone();
        let flush_in_flight = self.flush_in_flight.clone();

        // Spawn async read operation on tokio runtime
        self.runtime.spawn(async move {
            let start = std::time::Instant::now();
            info!("FUSE read START: ino={}, offset={}, size={}", ino, offset, size);

            let mut metadata = match metadata_cache.get(&ino) {
                Some(m) => m.clone(),
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            if metadata.file_type != FileType::RegularFile {
                reply.error(libc::EISDIR);
                return;
            }

            let offset = offset as usize;
            let size = size as usize;

            // Warm replica cache with sliding window - but only occasionally to avoid overhead
            // For files with many small chunks, warming on every read causes significant CPU overhead
            // Warm when: (1) first read, or (2) we've progressed significantly (every 50MB)
            // CRITICAL: Track warming per-inode to prevent cross-file interference during seeks
            let last_warm = last_warm_offset.get(&ino).map(|v| *v).unwrap_or(0);
            let should_warm = offset == 0 || offset.saturating_sub(last_warm as usize) >= 50 * 1024 * 1024;

            let has_locations = !metadata.chunk_locations.is_empty();
            let has_legacy = !metadata.chunks.is_empty() && !metadata.chunk_sizes.is_empty();
            if should_warm && (has_locations || has_legacy) {
                // Find which chunk index corresponds to this byte offset.
                let chunk_idx = if has_locations {
                    let mut cumulative = 0u64;
                    let mut idx = 0;
                    for (i, loc) in metadata.chunk_locations.iter().enumerate() {
                        if cumulative + loc.size as u64 > offset as u64 {
                            idx = i;
                            break;
                        }
                        cumulative += loc.size as u64;
                    }
                    idx
                } else {
                    let mut cumulative = 0u64;
                    let mut idx = 0;
                    for (i, &chunk_size) in metadata.chunk_sizes.iter().enumerate() {
                        if cumulative + chunk_size > offset as u64 {
                            idx = i;
                            break;
                        }
                        cumulative += chunk_size;
                    }
                    idx
                };

                // Prefer warming from real ChunkLocation data (has per-chunk node lists)
                // over the legacy all-nodes fake entries — eliminates mid-read metadata RPCs.
                if has_locations {
                    client.warm_replica_cache_from_locations(&metadata.chunk_locations, Some(chunk_idx)).await;
                } else {
                    client.warm_replica_cache_by_index(&metadata.chunks, Some(chunk_idx)).await;
                }

                last_warm_offset.insert(ino, offset as u64);
            }

            // Check write buffer first if write-behind buffering is enabled.
            // With slot-based buffering, we look up the specific chunk slot(s) that
            // cover the read range and serve directly from them if available.
            if write_buffer_enabled {
                if let Some(state_lock) = write_buffers.get(&ino) {
                    let state = state_lock.lock().await;
                    let read_end = offset + size;

                    // Check if the entire read is satisfied from buffered slots
                    let mut buf_data: Vec<u8> = Vec::with_capacity(size);
                    let mut fully_buffered = true;
                    let mut pos = offset;

                    while pos < read_end {
                        let chunk_idx = InodeWriteState::chunk_index(pos as u64);
                        let intra = InodeWriteState::intra_offset(pos as u64);
                        let chunk_file_start = (chunk_idx * CHUNK_SIZE as u64) as usize;
                        let need = (read_end - pos).min(CHUNK_SIZE - intra);

                        if let Some(slot) = state.slots.get(&chunk_idx) {
                            if intra + need <= slot.data.len() {
                                buf_data.extend_from_slice(&slot.data[intra..intra + need]);
                                pos += need;
                            } else if intra < slot.data.len() {
                                // Partial slot covers start of range but not all — serve what we have (live edge)
                                let avail = slot.data.len() - intra;
                                buf_data.extend_from_slice(&slot.data[intra..intra + avail]);
                                pos += avail;
                                fully_buffered = false; // will serve partial
                                break;
                            } else {
                                fully_buffered = false;
                                break;
                            }
                        } else {
                            fully_buffered = false;
                            break;
                        }
                    }

                    if !buf_data.is_empty() {
                        info!("FUSE read from write buffer slots: ino={}, offset={}, serving={} bytes (requested={})",
                              ino, offset, buf_data.len(), size);
                        reply.data(&buf_data);
                        return;
                    }
                    // If no buffered data covers this range, fall through to server read
                }
            }

            // Early return for out of bounds
            // But first, check if file might have grown by refreshing metadata
            if offset >= metadata.size as usize {
                // File might be actively growing (e.g., live recording in progress).
                // Rate-limit the metadata refresh so Kodi's frequent seek-past-EOF
                // polls don't hammer the server.  We allow one refresh per second per
                // inode; between refreshes we return empty immediately.
                let should_refresh = match last_metadata_update.get(&ino) {
                    None => true,
                    Some(last) => last.elapsed() >= std::time::Duration::from_secs(1),
                };

                if !should_refresh {
                    debug!("Read at offset {} >= size {}, rate-limiting EOF refresh, returning empty",
                           offset, metadata.size);
                    reply.data(&[]);
                    return;
                }

                info!("Read at offset {} >= cached size {}, refreshing metadata from server", offset, metadata.size);
                last_metadata_update.insert(ino, std::time::Instant::now());

                match client.get_file_metadata(&metadata.path).await {
                    Ok(Some(mut fresh_metadata)) => {
                        // Still past EOF even after refresh — return empty immediately,
                        // no need to fetch the chunk map (there's nothing new to read).
                        if offset >= fresh_metadata.size as usize {
                            info!("Still at EOF after refresh: offset {} >= size {}", offset, fresh_metadata.size);
                            client.seed_write_seq(fresh_metadata.id, fresh_metadata.write_seq);
                            metadata_cache.insert(ino, fresh_metadata);
                            reply.data(&[]);
                            return;
                        }

                        // File has grown past our read offset.  Now fetch the chunk map
                        // so we have accurate replica locations for the new chunks.
                        // Always refresh when size grew — modified_at may lag behind during
                        // active recording if the async metadata queue hasn't flushed yet.
                        let size_grew = fresh_metadata.size > metadata.size;
                        if size_grew || fresh_metadata.chunk_locations.is_empty() {
                            match client.get_file_chunk_map(fresh_metadata.id).await {
                                Ok((locations, _map_modified_at)) => {
                                    if !locations.is_empty() {
                                        info!("Refreshed chunk map from leader: {} locations for file {}",
                                              locations.len(), fresh_metadata.path);
                                        fresh_metadata.chunk_locations = locations;
                                        chunk_offset_cache.remove(&ino);
                                    }
                                }
                                Err(e) => {
                                    debug!("Could not refresh chunk map from leader at EOF ({}), using metadata chunk_locations", e);
                                }
                            }
                        }

                        // Update cache with fresh metadata
                        client.seed_write_seq(fresh_metadata.id, fresh_metadata.write_seq);
                        metadata_cache.insert(ino, fresh_metadata.clone());

                        // Warm replica cache for chunks ahead of the current read position
                        let chunk_idx = if !fresh_metadata.chunk_sizes.is_empty() {
                            let mut cumulative = 0u64;
                            let mut idx = 0;
                            for (i, &size) in fresh_metadata.chunk_sizes.iter().enumerate() {
                                if cumulative + size as u64 > offset as u64 {
                                    idx = i;
                                    break;
                                }
                                cumulative += size as u64;
                                idx = i + 1;
                            }
                            Some(idx)
                        } else {
                            None
                        };
                        client.warm_replica_cache_by_index(&fresh_metadata.chunks, chunk_idx).await;

                        info!("File grew from {} to {} bytes, continuing read", metadata.size, fresh_metadata.size);
                        metadata = fresh_metadata;
                    }
                    Ok(None) => {
                        info!("File not found when refreshing metadata");
                        reply.error(libc::ENOENT);
                        return;
                    }
                    Err(e) => {
                        // Server unreachable — return empty rather than blocking
                        info!("Failed to refresh metadata: {}, assuming EOF", e);
                        reply.data(&[]);
                        return;
                    }
                }
            }

            if metadata.chunks.is_empty() && metadata.chunk_locations.is_empty() {
                reply.data(&[]);
                return;
            }

            // Build or retrieve cached chunk offset map
            // CRITICAL: Cache this to avoid O(n) iteration through all chunks on every read
            // For 3GB files with 750+ chunks, this was a massive performance bottleneck
            //
            // SPARSE FILE SUPPORT: Use chunk_locations with file_offset if available,
            // otherwise fall back to sequential offset calculation for legacy files
            let chunk_offsets = {
                // Check cache without holding a ref across the potential insert
                let cached = chunk_offset_cache.get(&ino).and_then(|entry| {
                    let (cached_size, cached_chunk_count, ref cached_offsets) = *entry;
                    // Use chunk_locations.len() for modern files; fall back to chunks.len() for legacy.
                    let current_chunk_count = if !metadata.chunk_locations.is_empty() {
                        metadata.chunk_locations.len()
                    } else {
                        metadata.chunks.len()
                    };
                    if cached_size == metadata.size && cached_chunk_count == current_chunk_count {
                        Some(cached_offsets.clone())
                    } else {
                        None
                    }
                });

                if let Some(offsets) = cached {
                    offsets
                } else {
                    // Cache miss or invalidated - build and cache it
                    let mut offsets = Vec::with_capacity(metadata.chunks.len());

                    // Use chunk_locations when available — it's the authoritative source.
                    // The legacy chunks/chunk_sizes arrays may be empty when metadata was loaded
                    // from a server that only persisted chunk_locations (all new writes go here).
                    // The old requirement that chunk_locations.len() == chunks.len() is dropped:
                    // chunk_locations is self-sufficient and doesn't need chunks to be populated.
                    // Use chunk_locations with explicit offsets when ALL chunks have file_offset set.
                    // If any chunk has file_offset: None, fall back to sequential calculation —
                    // a partial mix (some Some, some None) would place None-chunks at offset 0,
                    // colliding with real chunk 0 and making subsequent chunks unreachable.
                    let all_have_offsets = !metadata.chunk_locations.is_empty()
                        && metadata.chunk_locations.iter().all(|l| l.file_offset.is_some());

                    if all_have_offsets {
                        // SPARSE FILE: Use explicit file_offset from chunk_locations
                        for location in &metadata.chunk_locations {
                            let chunk_offset = location.file_offset.unwrap();
                            offsets.push((chunk_offset as usize, location.size));
                        }
                    } else if !metadata.chunk_locations.is_empty() {
                        // chunk_locations present but missing offsets on some chunks —
                        // reconstruct sequentially from the sizes we do have.
                        let mut current_offset = 0usize;
                        for location in &metadata.chunk_locations {
                            offsets.push((current_offset, location.size));
                            current_offset += location.size;
                        }
                    } else {
                        // LEGACY FILE: fall back to legacy chunks/chunk_sizes arrays
                        let mut current_offset = 0usize;
                        for &chunk_size in metadata.chunk_sizes.iter() {
                            offsets.push((current_offset, chunk_size as usize));
                            current_offset += chunk_size as usize;
                        }
                    }

                    // Store in cache — use chunk_locations.len() for modern files so the
                    // cache invalidates correctly when new chunks are appended.
                    let chunk_count_key = if !metadata.chunk_locations.is_empty() {
                        metadata.chunk_locations.len()
                    } else {
                        metadata.chunks.len()
                    };
                    chunk_offset_cache.insert(ino, (metadata.size, chunk_count_key, offsets.clone()));

                    offsets
                }
            };

            // Find which chunks we need to read
            let end_offset = std::cmp::min(offset + size, metadata.size as usize);
            let mut chunks_to_read = Vec::new();
            let mut first_chunk_offset = 0usize;

            for (idx, &(chunk_start, chunk_size)) in chunk_offsets.iter().enumerate() {
                let chunk_end = chunk_start + chunk_size;

                // Check if this chunk overlaps with requested range
                if chunk_end > offset && chunk_start < end_offset {
                    chunks_to_read.push((idx, chunk_start, chunk_size));
                    if chunks_to_read.len() == 1 {
                        first_chunk_offset = chunk_start;
                    }
                }

                // Stop once we've found all needed chunks
                if chunk_start >= end_offset {
                    break;
                }
            }

            // SPARSE FILE SUPPORT: If no chunks found in this range, it's a hole!
            // Return zeros for the requested size (up to file size)
            if chunks_to_read.is_empty() {
                let bytes_to_read = end_offset.saturating_sub(offset);
                if bytes_to_read > 0 {
                    debug!("Reading from hole (unmapped region): offset {} size {} - returning zeros",
                           offset, bytes_to_read);
                    let zeros = vec![0u8; bytes_to_read];
                    reply.data(&zeros);
                } else {
                    reply.data(&[]);
                }
                return;
            }

            debug!("Reading {} chunks (indices {:?}) for offset {} size {}",
                   chunks_to_read.len(),
                   chunks_to_read.iter().map(|(idx, _, _)| idx).collect::<Vec<_>>(),
                   offset, size);

            // Build ChunkReadHints to tell the client how to read each chunk
            // For seeks (non-sequential reads), we can optimize by only fetching needed portions
            let read_hints: Vec<crate::client::ChunkReadHint> = chunks_to_read
                .iter()
                .map(|(idx, chunk_start, chunk_size)| {
                    let chunk_end = chunk_start + chunk_size;

                    // Calculate the overlap between requested range and this chunk
                    let read_start_in_file = offset.max(*chunk_start);
                    let read_end_in_file = end_offset.min(chunk_end);

                    // Calculate offset and length within the chunk
                    let offset_in_chunk = read_start_in_file.saturating_sub(*chunk_start);
                    let length_in_chunk = read_end_in_file.saturating_sub(read_start_in_file);

                    // Use partial reads when the request covers only a small fraction of the chunk.
                    // This is critical for Kodi seeks: it probes random offsets to detect duration,
                    // and fetching a full 4MB chunk over a 20Mbit link takes ~1.6s per probe.
                    // Only use partial reads when reading < 25% of the chunk to avoid overhead
                    // of multiple round trips for sequential streaming.
                    let full_chunk = length_in_chunk >= (*chunk_size as usize / 4)
                        || length_in_chunk == 0;

                    // Prefer chunk_locations as the authoritative source of chunk_id —
                    // metadata.chunks may be empty or mismatched when loaded from a server
                    // that only populated chunk_locations (common after a restart).
                    let chunk_id = if *idx < metadata.chunk_locations.len() {
                        metadata.chunk_locations[*idx].chunk_id
                    } else {
                        metadata.chunks[*idx]
                    };

                    crate::client::ChunkReadHint {
                        chunk_idx: *idx,
                        chunk_id,
                        full_chunk,
                        offset_in_chunk,
                        length: length_in_chunk,
                        file_offset: *chunk_start as u64,
                    }
                })
                .collect();

            let partial_reads = read_hints.iter().filter(|h| !h.full_chunk).count();
            if partial_reads > 0 {
                info!("Optimizing seek: {} partial chunk reads out of {}",
                      partial_reads, read_hints.len());
            }

            // For SQLite database files, disable caching by passing inode=0
            // This prevents stale cached data from causing corruption
            let cache_inode = {
                let path = &metadata.path;
                if is_sqlite_path(path) {
                    0 // Disable caching for SQLite files
                } else {
                    ino // Enable caching for other files
                }
            };

            let all_chunks = metadata.chunks.clone();
            // If metadata has no chunk_locations (legacy file or first read), fetch the full
            // chunk map from the leader in one round-trip instead of per-chunk fallback queries.
            let chunk_locations = if metadata.chunk_locations.is_empty() && !all_chunks.is_empty() {
                match client.get_file_chunk_map(metadata.id).await {
                    Ok((locations, _)) if !locations.is_empty() => {
                        info!("Fetched chunk map from leader: {} locations for {}", locations.len(), metadata.path);
                        // Cache the locations in metadata so subsequent reads skip this query
                        let mut updated = metadata.clone();
                        updated.chunk_locations = locations.clone();
                        metadata_cache.insert(ino, updated);
                        locations
                    }
                    Ok(_) => {
                        debug!("Leader returned empty chunk map for {}, using per-chunk fallback", metadata.path);
                        metadata.chunk_locations.clone()
                    }
                    Err(e) => {
                        debug!("Could not fetch chunk map from leader for {} ({}), using per-chunk fallback", metadata.path, e);
                        metadata.chunk_locations.clone()
                    }
                }
            } else {
                metadata.chunk_locations.clone()
            };
            let result = client.read_data(&read_hints, &all_chunks, cache_inode, &chunk_locations).await;

            let chunk_data = match result {
                Ok(data) => data,
                Err(e) => {
                    error!("Failed to read {} chunks: {}", read_hints.len(), e);
                    reply.error(libc::EIO);
                    return;
                }
            };

            // SPARSE FILE SUPPORT: Check if we need to handle holes
            // Holes can appear: (a) between chunks, (b) before the first chunk
            // (e.g. shm file truncated to N bytes then written at higher offsets)
            let has_holes = {
                // Check for leading hole: first chunk doesn't start at requested offset
                let leading_hole = if let Some(&(_, first_start, _)) = chunks_to_read.first() {
                    first_start > offset
                } else {
                    false
                };

                // Check for gaps between consecutive chunks
                let inter_chunk_gap = if chunks_to_read.len() > 1 {
                    let mut has_gap = false;
                    for i in 0..chunks_to_read.len() - 1 {
                        let (_, curr_start, curr_size) = chunks_to_read[i];
                        let (_, next_start, _) = chunks_to_read[i + 1];
                        if curr_start + curr_size < next_start {
                            has_gap = true;
                            break;
                        }
                    }
                    has_gap
                } else {
                    false
                };

                leading_hole || inter_chunk_gap
            };

            let final_data = if has_holes {
                // SPARSE FILE: Build result buffer with holes filled with zeros
                debug!("Sparse file read: filling holes with zeros");

                let bytes_needed = end_offset - offset;
                let mut result_buffer = vec![0u8; bytes_needed];

                let mut chunk_data_offset = 0usize;
                for (i, (_, chunk_start, chunk_size)) in chunks_to_read.iter().enumerate() {
                    // Determine how many bytes were actually returned for this chunk.
                    // Partial reads return only hint.length bytes, not the full chunk_size.
                    let actual_bytes_returned = if let Some(hint) = read_hints.get(i) {
                        if hint.full_chunk { *chunk_size } else { hint.length }
                    } else {
                        *chunk_size
                    };

                    // Calculate where this chunk's data should go in the result
                    let chunk_end = chunk_start + chunk_size;

                    // Find overlap between requested range [offset, end_offset) and chunk [chunk_start, chunk_end)
                    let overlap_start = (*chunk_start).max(offset);
                    let overlap_end = chunk_end.min(end_offset);

                    if overlap_start < overlap_end {
                        let result_offset = overlap_start - offset;
                        // For partial reads, data starts at byte 0 of what was returned (the hint's
                        // offset_in_chunk was already sent to the server as the range start).
                        let chunk_offset = if let Some(hint) = read_hints.get(i) {
                            if hint.full_chunk {
                                overlap_start - chunk_start
                            } else {
                                // Data returned starts at hint.offset_in_chunk in the logical chunk,
                                // but is at byte 0 in the returned buffer slice.
                                overlap_start.saturating_sub(chunk_start + hint.offset_in_chunk)
                            }
                        } else {
                            overlap_start - chunk_start
                        };
                        let overlap_size = overlap_end - overlap_start;

                        let src_start = chunk_data_offset + chunk_offset;
                        let src_end = src_start + overlap_size;
                        if src_end <= chunk_data.len() {
                            result_buffer[result_offset..result_offset + overlap_size]
                                .copy_from_slice(&chunk_data[src_start..src_end]);
                        } else {
                            warn!("Sparse read: chunk {} data out of bounds (src {}..{} > buf {}), skipping",
                                  i, src_start, src_end, chunk_data.len());
                        }
                    }

                    chunk_data_offset += actual_bytes_returned;
                }

                result_buffer
            } else {
                // NON-SPARSE FILE: Use simple offset calculation (existing logic)
                // For partial reads, data starts at hint.offset_in_chunk in the logical chunk
                // but at byte 0 in the returned buffer, so adjust accordingly.
                //
                // IMPORTANT: If the hint requested a partial read but the cache served a full
                // chunk (chunk_data.len() > hint.length), treat it as a full-chunk result so
                // we index from the chunk start rather than the partial-read offset.
                let first_hint_offset = read_hints.first()
                    .map(|h| {
                        if h.full_chunk {
                            0
                        } else if chunk_data.len() > h.length {
                            // Cache served a larger (full) chunk despite partial hint — index from chunk start
                            0
                        } else {
                            h.offset_in_chunk
                        }
                    })
                    .unwrap_or(0);
                let offset_in_data = offset.saturating_sub(first_chunk_offset + first_hint_offset);
                let data_end = std::cmp::min(offset_in_data + size, chunk_data.len());

                if offset_in_data >= chunk_data.len() {
                    debug!("Read offset {} beyond data length {}", offset_in_data, chunk_data.len());
                    Vec::new()
                } else {
                    chunk_data[offset_in_data..data_end].to_vec()
                }
            };

            if final_data.is_empty() {
                reply.data(&[]);
            } else {
                debug!("Returning {} bytes from offset {} (read {} chunks, has_holes: {})",
                       final_data.len(), offset, read_hints.len(), has_holes);
                reply.data(&final_data);
            }

            let elapsed = start.elapsed();
            info!("FUSE read COMPLETE: ino={}, offset={}, size={}, took {:?}",
                  ino, offset, size, elapsed);
        });
    }

    fn readdir(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        debug!("readdir: ino={}, offset={}", ino, offset);
        let start = std::time::Instant::now();

        let path = {
            match self.metadata_cache.get(&ino) {
                Some(metadata) => {
                    if metadata.file_type != FileType::Directory {
                        reply.error(libc::ENOTDIR);
                        return;
                    }
                    metadata.path.clone()
                }
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        // Check directory cache first (5-second TTL)
        let cached_entries = self.dir_cache.get(&path).and_then(|entry| {
            let (entries, timestamp) = &*entry;
            if timestamp.elapsed() < std::time::Duration::from_secs(5) {
                debug!("Directory cache HIT for {}", path);
                Some(entries.clone())
            } else {
                debug!("Directory cache EXPIRED for {}", path);
                None
            }
        });

        // Clone all Arc fields needed in the spawned task.
        let client = self.client.clone();
        let dir_cache = self.dir_cache.clone();
        let metadata_cache = self.metadata_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let next_inode = self.next_inode.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let write_buffers = self.write_buffers.clone();
        let write_counters = self.write_counters.clone();

        // Spawn the rest of readdir so we never block_on from the FUSE dispatch
        // thread.  If the runtime is saturated (e.g. concurrent recording writes)
        // a block_on here would deadlock — same root cause as the lookup() fix.
        // ReplyDirectory is Send so it moves into the task safely.
        self.runtime.spawn(async move {
            let entries = if let Some(entries) = cached_entries {
                entries
            } else {
                // Cache miss — fetch from server.
                debug!("Directory cache MISS for {}", path);
                match client.list_directory(&path).await {
                    Ok(entries) => {
                        dir_cache.insert(path.clone(), (entries.clone(), std::time::Instant::now()));
                        entries
                    }
                    Err(e) => {
                        error!("Failed to read directory {}: {}", path, e);
                        reply.error(libc::EIO);
                        return;
                    }
                }
            };

            // Add . and ..
            if offset == 0 {
                if reply.add(ino, 1, FuseFileType::Directory, ".") {
                    reply.ok();
                    return;
                }
            }
            if offset <= 1 {
                if reply.add(ino, 2, FuseFileType::Directory, "..") {
                    reply.ok();
                    return;
                }
            }

            // Add actual entries
            let skip_count = if offset > 2 { (offset - 2) as usize } else { 0 };
            for (i, entry) in entries.iter().enumerate().skip(skip_count) {
                let file_name = entry.path.rsplit('/').next().unwrap_or("");

                // Skip entries with empty filenames (like the root directory "/")
                if file_name.is_empty() {
                    debug!("Skipping entry with empty filename: path={}", entry.path);
                    continue;
                }

                let kind = match entry.file_type {
                    FileType::RegularFile => FuseFileType::RegularFile,
                    FileType::Directory => FuseFileType::Directory,
                    FileType::Symlink => FuseFileType::Symlink,
                };

                // Get or allocate inode
                let entry_ino = {
                    let path_map = path_to_inode.read().unwrap();
                    if let Some(&existing) = path_map.get(&entry.path) {
                        existing
                    } else {
                        drop(path_map);
                        let mut next = next_inode.write().unwrap();
                        let ino_val = *next;
                        *next += 1;
                        drop(next);
                        path_to_inode.write().unwrap().insert(entry.path.clone(), ino_val);
                        ino_val
                    }
                };

                // Cache metadata, but DON'T overwrite if there's an active write.
                let has_active_write = {
                    let has_buffer = write_buffers.contains_key(&entry_ino);
                    let has_counter = write_counters.read().unwrap().get(&entry_ino).map(|c| *c > 0).unwrap_or(false);
                    has_buffer || has_counter
                };
                if !has_active_write {
                    client.seed_write_seq(entry.id, entry.write_seq);
                    metadata_cache.insert(entry_ino, entry.clone());
                }

                // Mark metadata as just-refreshed so getattr skips the per-file server
                // round-trip on the immediately following `ls -alh`.
                last_metadata_update.insert(entry_ino, std::time::Instant::now());

                let next_offset = 3 + i as i64;  // 3 because . is 1, .. is 2, first file is 3
                if reply.add(entry_ino, next_offset, kind, file_name) {
                    break; // Buffer full
                }
            }

            let elapsed = start.elapsed();
            info!("readdir COMPLETE: {} with {} entries in {:?}", path, entries.len(), elapsed);
            reply.ok();

            // Prefetch subdirectory listings in the background so the next level of
            // readdir calls (e.g. DVR indexer walking show → episode directories) are
            // instant cache hits.  Fire-and-forget — we don't wait for these.
            let subdirs: Vec<String> = entries.iter()
                .filter(|e| e.file_type == FileType::Directory)
                .map(|e| e.path.clone())
                .collect();

            if !subdirs.is_empty() {
                let futures: Vec<_> = subdirs.into_iter().map(|subdir| {
                    let client = client.clone();
                    let dir_cache = dir_cache.clone();
                    let metadata_cache = metadata_cache.clone();
                    let path_to_inode = path_to_inode.clone();
                    let next_inode = next_inode.clone();
                    let last_metadata_update = last_metadata_update.clone();
                    async move {
                        // Skip if already cached and fresh
                        if let Some(entry) = dir_cache.get(&subdir) {
                            if entry.1.elapsed() < std::time::Duration::from_secs(4) {
                                return;
                            }
                        }
                        let fetch_start = std::time::Instant::now();
                        if let Ok(sub_entries) = client.list_directory(&subdir).await {
                            // Only cache if the directory hasn't been invalidated while we
                            // were fetching.
                            let still_valid = match dir_cache.get(&subdir) {
                                Some(entry) => entry.1 < fetch_start,
                                None => false,
                            };
                            if still_valid {
                                dir_cache.insert(
                                    subdir.clone(),
                                    (sub_entries.clone(), std::time::Instant::now()),
                                );
                            }
                            let now = std::time::Instant::now();
                            for entry in &sub_entries {
                                let ino_val = {
                                    let mut path_map = path_to_inode.write().unwrap();
                                    if let Some(&existing) = path_map.get(&entry.path) {
                                        existing
                                    } else {
                                        let mut next = next_inode.write().unwrap();
                                        let v = *next;
                                        *next += 1;
                                        path_map.insert(entry.path.clone(), v);
                                        v
                                    }
                                };
                                client.seed_write_seq(entry.id, entry.write_seq);
                                metadata_cache.insert(ino_val, entry.clone());
                                last_metadata_update.insert(ino_val, now);
                            }
                            debug!("Prefetched {} entries for {}", sub_entries.len(), subdir);
                        }
                    }
                }).collect();
                futures::future::join_all(futures).await;
            }
        });
    }

    fn create(
        &mut self,
        _req: &FuseRequest,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        debug!("create: parent={}, name={:?}, mode={:o}", parent, name, mode);

        let path = match self.get_path_from_parent(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Create file metadata
        let metadata = FileMetadata {
            id: dfs_common::FileId::new(),
            path: path.clone(),
            size: 0,
            chunks: Vec::new(),
            chunk_sizes: Vec::new(),
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            modified_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            mode,
            uid: _req.uid(),
            gid: _req.gid(),
            file_type: FileType::RegularFile,
            chunk_locations: Vec::new(),
            write_seq: 0,
        };

        // Store metadata on cluster — spawn so we never block_on the FUSE dispatch thread.
        let client = self.client.clone();
        let metadata_clone = metadata.clone();
        let metadata_cache = self.metadata_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let next_inode = self.next_inode.clone();
        let write_open_counts = self.write_open_counts.clone();

        self.runtime.spawn(async move {
            match client.put_file_metadata(&metadata_clone).await {
                Ok(_) => {
                    // Allocate inode
                    let ino = {
                        let path_map = path_to_inode.read().unwrap();
                        if let Some(&existing) = path_map.get(&path) {
                            existing
                        } else {
                            drop(path_map);
                            let mut next = next_inode.write().unwrap();
                            let v = *next; *next += 1; drop(next);
                            path_to_inode.write().unwrap().insert(path.clone(), v);
                            v
                        }
                    };

                    // Cache metadata
                    metadata_cache.insert(ino, metadata.clone());

                    // Invalidate parent directory cache so 'ls' shows new file immediately
                    let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                    let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                    dir_cache.remove(parent_path);

                    // create() always opens for writing — count it
                    *write_open_counts.entry(ino).or_insert(0) += 1;

                    let attr = DfsFilesystem::metadata_to_attr_static(ino, &metadata);
                    let open_flags = if is_sqlite_direct_io(&path) {
                        fuser::consts::FOPEN_DIRECT_IO
                    } else {
                        0
                    };
                    reply.created(&Duration::from_secs(300), &attr, 0, 0, open_flags);
                }
                Err(e) => {
                    error!("Failed to create file {}: {}", path, e);
                    reply.error(libc::EIO);
                }
            }
        });
    }

    fn write(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyWrite,
    ) {
        // Clone Arc-wrapped fields for thread pool
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let write_counters = self.write_counters.clone();
        let write_buffers = self.write_buffers.clone();
        let write_buffer_enabled = self.write_buffer_enabled;
        let buffer_flush_threshold = self.buffer_flush_threshold;
        let runtime = self.runtime.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let data_vec = data.to_vec(); // Copy data before moving

        // Execute write operation synchronously to preserve write order
        // This ensures proper sequencing and prevents corruption
        {
            let start = std::time::Instant::now();
            debug!("write: ino={}, offset={}, size={}", ino, offset, data_vec.len());

            let mut metadata = match metadata_cache.get(&ino) {
                Some(m) => m.clone(),
                None => {
                    // Metadata cache miss — the kernel may have sent open() with a cached
                    // inode without going through lookup() first (FOPEN_KEEP_CACHE survives
                    // client restarts).  Fetch from the server and populate the cache so
                    // the write path has a FileId and correct start_offset.
                    let path_opt = {
                        let map = self.path_to_inode.read().unwrap();
                        map.iter().find(|(_, &v)| v == ino).map(|(k, _)| k.clone())
                    };
                    if let Some(path) = path_opt {
                        match runtime.block_on(client.get_file_metadata(&path)) {
                            Ok(Some(fetched)) => {
                                client.seed_write_seq(fetched.id, fetched.write_seq);
                                metadata_cache.insert(ino, fetched.clone());
                                self.last_metadata_update.insert(ino, std::time::Instant::now());
                                fetched
                            }
                            Ok(None) => {
                                // File doesn't exist on server yet — new file, create minimal record
                                let new_meta = dfs_common::FileMetadata {
                                    id: dfs_common::FileId::new(),
                                    path: path.clone(),
                                    size: 0,
                                    chunks: Vec::new(),
                                    chunk_sizes: Vec::new(),
                                    chunk_locations: Vec::new(),
                                    created_at: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default().as_secs(),
                                    modified_at: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default().as_secs(),
                                    mode: 0o644,
                                    uid: _req.uid(),
                                    gid: _req.gid(),
                                    file_type: dfs_common::FileType::RegularFile,
                                    write_seq: 0,
                                };
                                info!("write: inode {} has no server metadata, creating new record for {}", ino, path);
                                metadata_cache.insert(ino, new_meta.clone());
                                new_meta
                            }
                            Err(e) => {
                                error!("write: failed to fetch metadata for inode {}: {}", ino, e);
                                reply.error(libc::EIO);
                                return;
                            }
                        }
                    } else {
                        error!("write: no path known for inode {}, returning ENOENT", ino);
                        reply.error(libc::ENOENT);
                        return;
                    }
                }
            };

            if metadata.file_type != FileType::RegularFile {
                reply.error(libc::EISDIR);
                return;
            }

            // For SQLite database files, disable caching AND buffering to prevent corruption
            // SQLite does read-your-own-writes and needs immediate consistency
            // .db-shm is excluded: it's mmapped by SQLite and managed via page cache
            let is_sqlite = is_sqlite_path(&metadata.path);
            let cache_inode = if is_sqlite { 0 } else { ino };

            // Write-behind buffering: buffer sequential appends in memory
            // EXCEPT for SQLite files which need immediate write-through for consistency
            if write_buffer_enabled && !is_sqlite {
                let offset_usize = offset as usize;
                // Calculate true current size including any buffered data.
                // With slot-based buffering, the buffered "end" is the highest
                // (chunk_idx * CHUNK_SIZE + slot.data.len()) across all dirty slots.
                let current_size = {
                    let cache_size = metadata_cache.get(&ino)
                        .map(|m| m.size as usize)
                        .unwrap_or(metadata.size as usize);

                    if let Some(state_lock) = write_buffers.get(&ino) {
                        let state = runtime.block_on(async { state_lock.lock().await });
                        let buffered_end = state.slots.iter()
                            .map(|(idx, slot)| (idx * CHUNK_SIZE as u64 + slot.data.len() as u64) as usize)
                            .max()
                            .unwrap_or(0);
                        buffered_end.max(cache_size)
                    } else {
                        cache_size
                    }
                };

                debug!("Buffered write check: offset={}, current_size={}, cache_size={}, buffer_present={}",
                       offset_usize, current_size,
                       metadata_cache.get(&ino).map(|m| m.size).unwrap_or(0),
                       write_buffers.contains_key(&ino));

                // Handle different write patterns:
                // 1. Sequential appends (offset == current_size) → use buffering for performance
                // 2. Overwrites (offset < current_size) → flush buffer first, then fall through to RMW
                // 3. Sparse writes (offset > current_size) → write directly with hole support
                let is_sequential_append = offset_usize == current_size;
                let is_overwrite = offset_usize < current_size;
                let is_sparse_write = offset_usize > current_size;

                // Only flush the append buffer before an overwrite if the write actually
                // overlaps with the buffered region. A Kodi-style metadata write at offset 0
                // while the DVR is recording at offset 200MB does NOT overlap — flushing in
                // that case produces a mis-aligned partial chunk and disrupts the recording.
                let overwrite_overlaps_buffer = if is_overwrite {
                    if let Some(state_lock) = write_buffers.get(&ino) {
                        let state = runtime.block_on(async { state_lock.lock().await });
                        // An overwrite overlaps a dirty slot if any slot covers the write range
                        let write_end = offset_usize + data.len();
                        state.slots.iter().any(|(idx, slot)| {
                            let slot_start = (idx * CHUNK_SIZE as u64) as usize;
                            let slot_end = slot_start + slot.data.len();
                            write_end > slot_start && offset_usize < slot_end
                        })
                    } else {
                        false
                    }
                } else {
                    false
                };

                if is_overwrite && write_buffers.contains_key(&ino) && overwrite_overlaps_buffer {
                    // OVERWRITE overlaps buffered region: flush the buffer first so the
                    // read-modify-write below sees the complete committed file. Without this,
                    // the splice recalculates metadata.size = sum(flushed chunks) which
                    // discards the buffered tail → file truncation.
                    info!("Overwrite at offset={} overlaps buffer (current_size={}): flushing buffer first",
                          offset_usize, current_size);
                    if let Err(e) = runtime.block_on(self.flush_buffer_async(ino, true)) {
                        error!("Failed to flush buffer before overwrite for inode {}: {}", ino, e);
                        reply.error(libc::EIO);
                        return;
                    }
                    // Fall through to the direct random-write path below (no buffering)
                }

                if is_sparse_write {
                    let gap = offset_usize - current_size;

                    // Small gaps (< 64KB) are DVR/MPEG-TS packet padding — absorb into the
                    // write buffer by zero-filling the gap so buffering stays intact.
                    // Large gaps are genuine sparse writes and bypass buffering as before.
                    const SMALL_GAP_THRESHOLD: usize = 64 * 1024;
                    if gap < SMALL_GAP_THRESHOLD {
                        info!("Near-sequential write: offset={} current_size={} gap={} bytes — zero-filling into buffer",
                              offset_usize, current_size, gap);
                        // Build padded data: zeros for the gap + the actual write data
                        let mut padded = vec![0u8; gap];
                        padded.extend_from_slice(&data_vec);
                        // Re-route through the sequential append path by adjusting data_vec
                        // and treating offset as current_size (now effectively sequential).
                        // We do this by appending to the write buffer directly.
                        let write_buffers_clone2 = write_buffers.clone();
                        let client_clone3 = client.clone();
                        let metadata_cache_clone4 = metadata_cache.clone();
                        let metadata_cache_clone5 = metadata_cache.clone();
                        let metadata_cache_clone6 = metadata_cache.clone();

                        let write_counters2 = write_counters.clone();
                        let padded_len = padded.len();
                        // Gap-fill write: route padded data into slot starting at current_size
                        let gap_write_offset = current_size as u64;

                        let gap_result = runtime.block_on(async move {
                            let state_arc = write_buffers_clone2
                                .entry(ino)
                                .or_insert_with(|| Arc::new(Mutex::new(InodeWriteState::new(false))))
                                .clone();
                            let mut state = state_arc.lock().await;
                            state.write_at(gap_write_offset, &padded);
                            Ok::<usize, anyhow::Error>(padded_len)
                        });

                        match gap_result {
                            Ok(_) => {
                                {
                                    let mut counters = write_counters2.write().unwrap();
                                    *counters.entry(ino).or_insert(0) += 1;
                                }
                                reply.written(data_vec.len() as u32);
                                return;
                            }
                            Err(e) => {
                                error!("Failed to buffer gap-fill write for inode {}: {}", ino, e);
                                reply.error(libc::EIO);
                                return;
                            }
                        }
                    }

                    // TRUE SPARSE WRITE: Large gap, write directly (hole support)
                    info!("Sparse write: offset {} > current_size {} (gap: {} bytes)",
                           offset_usize, current_size, gap);

                    // Write directly at the specified offset (no buffering for sparse writes)
                    let write_result = runtime.block_on(async {
                        // Write data with file_offset tracking (Phase 1 support)
                        client.write_data_with_cache(&data_vec, ino, offset as u64).await
                    });

                    match write_result {
                        Ok((new_chunk_ids, new_chunk_sizes, chunk_locations_opt)) => {
                            // Update file size to max(current, offset + len) for sparse files
                            let new_size = (offset_usize + data.len()).max(current_size);

                            // Update metadata with new chunks
                            let mut metadata = metadata_cache.get(&ino).map(|m| m.clone()).unwrap_or_else(|| metadata.clone());

                            if let Some(chunk_locations) = chunk_locations_opt {
                                metadata.chunk_locations.extend(chunk_locations);
                            }
                            metadata.chunks.extend(new_chunk_ids);
                            metadata.chunk_sizes.extend(new_chunk_sizes);
                            metadata.size = new_size as u64;
                            metadata.modified_at = SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_secs();

                            // Enqueue metadata update async — data is already safe on disk.
                            runtime.block_on(async {
                                client.enqueue_metadata(&metadata).await;
                            });

                            // Update cache
                            metadata_cache.insert(ino, metadata);

                            info!("Sparse write complete: ino={}, offset={}, len={}, new_size={}",
                                  ino, offset, data.len(), new_size);
                            reply.written(data.len() as u32);
                        }
                        Err(e) => {
                            error!("Sparse write failed for inode {}: {}", ino, e);
                            reply.error(libc::EIO);
                        }
                    }
                    return;
                }

                // BUFFERED WRITE (offset-based slot routing)
                // All write patterns — sequential, overwrite, sparse-within-chunk — are routed
                // into the correct per-chunk slot by file offset. No sequential ordering required.
                // Overwrites that don't overlap a dirty slot bypass buffering (rare random I/O).
                {
                    let write_buffers_clone = write_buffers.clone();
                    // Hard back-pressure cap = pipeline depth (ceil(32MB / chunk_size) × chunk_size).
                    // Background flusher drains full slots automatically; this cap prevents OOM
                    // on low-RAM clients (nanopir3: 1.9 GB) if the cluster is temporarily slow.
                    let max_buffered_bytes: usize = buffer_flush_threshold;
                    let data_slice = data_vec.clone();
                    let write_offset = offset as u64;

                    let buf_result = runtime.block_on(async move {
                        // Back-pressure: stall if too much unbuffered data
                        let stall_start = std::time::Instant::now();
                        loop {
                            let buffered = if let Some(entry) = write_buffers_clone.get(&ino) {
                                entry.lock().await.buffered_bytes()
                            } else { 0 };
                            if buffered < max_buffered_bytes { break; }
                            if stall_start.elapsed() > std::time::Duration::from_secs(10) {
                                return Err(anyhow::anyhow!("Write buffer stall timeout"));
                            }
                            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                        }

                        // Get or create InodeWriteState for this inode
                        let state_arc = write_buffers_clone
                            .entry(ino)
                            .or_insert_with(|| Arc::new(Mutex::new(InodeWriteState::new(false))))
                            .clone();

                        let mut state = state_arc.lock().await;
                        let full_slots = state.write_at(write_offset, &data_slice);
                        Ok::<Vec<u64>, anyhow::Error>(full_slots)
                    });

                    match buf_result {
                        Ok(_full_slots) => {
                            // Full slots will be drained by the background flusher automatically.
                            // No inline network I/O — write() returns immediately.
                            {
                                let mut counters = write_counters.write().unwrap();
                                *counters.entry(ino).or_insert(0) += 1;
                            }
                            let total_elapsed = start.elapsed();
                            debug!("BUFFERED write() took {:?} for {} bytes at offset {}",
                                   total_elapsed, data_vec.len(), offset);
                            reply.written(data_vec.len() as u32);
                            return;
                        }
                        Err(e) => {
                            error!("Failed to buffer write for inode {}: {}", ino, e);
                            reply.error(libc::EIO);
                            return;
                        }
                    }
                }
            }

            // Optimize for sequential writes (appends)
            let offset = offset as usize;
            // Calculate true current size including any buffered data
            // IMPORTANT: Re-read metadata from cache to get latest updates
            let current_size = if write_buffer_enabled {
                let cache_size = metadata_cache.get(&ino)
                    .map(|m| m.size as usize).unwrap_or(metadata.size as usize);

                let buffered_end = runtime.block_on(async {
                    if let Some(state_lock) = write_buffers.get(&ino) {
                        let state = state_lock.lock().await;
                        state.slots.iter()
                            .map(|(idx, slot)| (idx * CHUNK_SIZE as u64 + slot.data.len() as u64) as usize)
                            .max()
                            .unwrap_or(0)
                    } else {
                        0
                    }
                });
                buffered_end.max(cache_size)
            } else {
                metadata.size as usize
            };

            // Track affected chunk range for random writes (for metadata splice)
            let mut affected_chunk_range: Option<(usize, usize)> = None;

            let (new_data, is_append) = if offset == current_size {
                // Sequential write/append - just write new data
                // This is the fast path for DVR recordings, dd, etc.
                (data_vec.clone(), true)
            } else if offset > current_size {
                // Writing past end of file - need to pad with zeros
                let mut padded = vec![0u8; offset - current_size];
                padded.extend_from_slice(&data_vec);
                (padded, true)
            } else {
                // Random write in middle of file - need read-modify-write
                // CRITICAL FIX: Only read affected chunks, not entire file!
                // For a 10GB file with 1KB write, reading entire file causes OOM

                info!("Random write detected: offset={}, size={}, file_size={}",
                      offset, data_vec.len(), current_size);

                let write_end = offset + data_vec.len();

                // Calculate which chunks are affected by this write
                let chunk_ids = metadata.chunks.clone();
                let chunk_sizes = metadata.chunk_sizes.clone();

                if chunk_ids.is_empty() {
                    // Empty file - just write the data
                    (data_vec.clone(), false)
                } else {
                    let locations_match = !metadata.chunk_locations.is_empty()
                        && metadata.chunk_locations[0].file_offset.is_some();

                    // Find chunk range that overlaps with write range [offset, write_end)
                    let mut chunk_start_offset = 0u64;
                    let mut first_affected_chunk: Option<usize> = None;
                    let mut last_affected_chunk: Option<usize> = None;

                    for (idx, &chunk_size) in chunk_sizes.iter().enumerate() {
                        let chunk_end_offset = chunk_start_offset + chunk_size;

                        // Check if this chunk overlaps with write range
                        if chunk_end_offset > offset as u64 && chunk_start_offset < write_end as u64 {
                            if first_affected_chunk.is_none() {
                                first_affected_chunk = Some(idx);
                            }
                            last_affected_chunk = Some(idx);
                        }

                        chunk_start_offset = chunk_end_offset;
                    }

                    // Check if we found affected chunks
                    if first_affected_chunk.is_none() || last_affected_chunk.is_none() {
                        // Write is beyond EOF - treat as append
                        info!("Write beyond EOF, treating as append");
                        (data_vec.clone(), true)
                    } else {
                        let first_idx = first_affected_chunk.unwrap();
                        let last_idx = last_affected_chunk.unwrap();

                    info!("Random write affects chunks {}-{} (out of {} total)",
                          first_idx, last_idx, chunk_ids.len());

                    // Store affected range for metadata splice later
                    affected_chunk_range = Some((first_idx, last_idx));

                    // Read only the affected chunks
                    let affected_chunk_ids: Vec<_> = chunk_ids[first_idx..=last_idx].to_vec();
                    let affected_chunk_sizes: Vec<_> = chunk_sizes[first_idx..=last_idx].to_vec();

                    // Calculate file offset of first affected chunk
                    let first_chunk_file_offset: u64 = chunk_sizes[..first_idx].iter().sum();

                    // Build chunk offsets for affected chunks only
                    // Build read hints for affected chunks (full chunk reads for read-modify-write)
                    let mut read_hints = Vec::with_capacity(affected_chunk_ids.len());
                    let mut current_offset = first_chunk_file_offset;
                    for (i, &chunk_id) in affected_chunk_ids.iter().enumerate() {
                        let chunk_size = affected_chunk_sizes[i] as usize;
                        read_hints.push(crate::client::ChunkReadHint {
                            chunk_idx: first_idx + i,
                            chunk_id,
                            full_chunk: true,  // Always read full chunks for read-modify-write
                            offset_in_chunk: 0,
                            length: chunk_size,
                            file_offset: current_offset,
                        });
                        current_offset += chunk_size as u64;
                    }

                    let affected_data = match runtime.block_on(async {
                        client.read_data(&read_hints, &chunk_ids, cache_inode, &metadata.chunk_locations).await
                    }) {
                        Ok(data) => data,
                        Err(e) => {
                            error!("Failed to read affected chunks {}-{}: {}", first_idx, last_idx, e);
                            reply.error(libc::EIO);
                            return;
                        }
                    };

                    // Calculate offset within the affected chunk range
                    let write_offset_in_range = (offset as u64 - first_chunk_file_offset) as usize;
                    let affected_data_len = affected_data.len();

                    // Merge write data into affected chunks
                    let mut merged = affected_data;
                    if write_offset_in_range + data_vec.len() > merged.len() {
                        merged.resize(write_offset_in_range + data_vec.len(), 0);
                    }
                    merged[write_offset_in_range..write_offset_in_range + data_vec.len()]
                        .copy_from_slice(&data_vec);

                    info!("Random write: read {} bytes from {} chunks, merged to {} bytes",
                          affected_data_len, affected_chunk_ids.len(), merged.len());

                        // Return merged data and metadata update strategy
                        // We'll need to splice the new chunks into the metadata
                        (merged, false)
                    }
                }
            };

            // Write to cluster (only new/modified data for appends)
            // Use write_data_with_cache to populate byte-range cache for immediate read-back
            let write_start = std::time::Instant::now();
            let result = if is_append {
                // Append: write just the new data as new chunks
                runtime.block_on(async {
                    // Pass file offset for cache population (write-through caching)
                    client.write_data_with_cache(&new_data, cache_inode, current_size as u64).await
                })
            } else {
                // Random write: write only the affected chunks
                // Calculate file offset of first affected chunk for cache population
                let write_file_offset = if let Some((first_idx, _)) = affected_chunk_range {
                    metadata.chunk_sizes[..first_idx].iter().sum::<u64>()
                } else {
                    0
                };
                runtime.block_on(async {
                    client.write_data_with_cache(&new_data, cache_inode, write_file_offset).await
                })
            };
            let write_elapsed = write_start.elapsed();
            debug!("write_data took {:?}", write_elapsed);

            match result {
                Ok((new_chunk_ids, new_chunk_sizes, chunk_locations_opt)) => {
                    // Update metadata
                    if is_append {
                        // Append: add new chunks to existing list
                        if let Some(ref chunk_locations) = chunk_locations_opt {
                            metadata.chunk_locations.extend(chunk_locations.clone());
                        }
                        metadata.chunks.extend(new_chunk_ids);
                        metadata.chunk_sizes.extend(new_chunk_sizes);
                        metadata.size = current_size as u64 + new_data.len() as u64;
                    } else if let Some((first_idx, last_idx)) = affected_chunk_range {
                        // Random write: splice new chunks into affected range
                        // Keep chunks before affected range, insert new chunks, keep chunks after
                        info!("Splicing {} new chunks into range {}-{} (was {} chunks)",
                              new_chunk_ids.len(), first_idx, last_idx, last_idx - first_idx + 1);

                        let mut updated_chunks = Vec::new();
                        let mut updated_sizes = Vec::new();
                        let mut updated_locations = Vec::new();

                        // Keep chunks before affected range
                        updated_chunks.extend_from_slice(&metadata.chunks[..first_idx]);
                        updated_sizes.extend_from_slice(&metadata.chunk_sizes[..first_idx]);
                        if !metadata.chunk_locations.is_empty() && first_idx <= metadata.chunk_locations.len() {
                            updated_locations.extend_from_slice(&metadata.chunk_locations[..first_idx]);
                        }

                        // Insert new chunks
                        updated_chunks.extend(new_chunk_ids);
                        updated_sizes.extend(new_chunk_sizes);
                        if let Some(ref chunk_locations) = chunk_locations_opt {
                            updated_locations.extend(chunk_locations.clone());
                        }

                        // Keep chunks after affected range (if any)
                        if last_idx + 1 < metadata.chunks.len() {
                            updated_chunks.extend_from_slice(&metadata.chunks[last_idx + 1..]);
                            updated_sizes.extend_from_slice(&metadata.chunk_sizes[last_idx + 1..]);
                            if !metadata.chunk_locations.is_empty() && last_idx + 1 < metadata.chunk_locations.len() {
                                updated_locations.extend_from_slice(&metadata.chunk_locations[last_idx + 1..]);
                            }
                        }

                        metadata.chunks = updated_chunks;
                        metadata.chunk_sizes = updated_sizes;
                        metadata.chunk_locations = updated_locations;

                        // Recalculate total file size
                        metadata.size = metadata.chunk_sizes.iter().sum();

                        info!("After splice: {} total chunks, {} total bytes",
                              metadata.chunks.len(), metadata.size);
                    } else {
                        // Full rewrite (shouldn't happen with current logic, but keep as fallback)
                        warn!("Full file rewrite with {} bytes", new_data.len());
                        if let Some(chunk_locations) = chunk_locations_opt {
                            metadata.chunk_locations = chunk_locations;
                        } else {
                            metadata.chunk_locations.clear();
                        }
                        metadata.chunks = new_chunk_ids;
                        metadata.chunk_sizes = new_chunk_sizes;
                        metadata.size = new_data.len() as u64;
                    }
                    metadata.modified_at = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs();

                    // Enqueue metadata update async (sync for SQLite).
                    // The background worker drains to the leader; data is already safe on disk.
                    // Time-based batching is no longer needed: the queue deduplicates by file_id
                    // so only the latest metadata snapshot is ever in-flight per file.
                    let metadata_clone = metadata.clone();
                    runtime.block_on(async {
                        client.enqueue_metadata(&metadata_clone).await;
                    });

                    // Update local cache and reply.
                    metadata_cache.insert(ino, metadata);
                    let total_elapsed = start.elapsed();
                    debug!("TOTAL write() took {:?} for {} bytes ({:.2} MB/s)",
                        total_elapsed, data_vec.len(),
                        (data_vec.len() as f64 / 1024.0 / 1024.0) / total_elapsed.as_secs_f64());
                    reply.written(data_vec.len() as u32);
                }
                Err(e) => {
                    error!("Failed to write data: {}", e);
                    reply.error(libc::EIO);
                }
            }
        }
    }

    fn flush(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: fuser::ReplyEmpty,
    ) {
        // flush() is called for every close() — including read-only fds.
        // If there are no write-mode handles open for this inode, skip touching the
        // write buffer entirely. Otherwise a reader closing its fd would steal and
        // prematurely flush the DVR's write buffer, producing a mis-sized chunk and
        // resetting the write state mid-recording.
        let has_writers = self.write_open_counts.get(&ino).map(|c| *c > 0).unwrap_or(false);
        if !has_writers {
            debug!("flush: ino={} - no writers, skipping buffer flush for read-only close", ino);
            reply.ok();
            return;
        }

        // Clone Arc-wrapped fields for thread pool
        let write_buffer_enabled = self.write_buffer_enabled;
        let write_buffers = self.write_buffers.clone();
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let write_counters = self.write_counters.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let runtime = self.runtime.clone();

        // Spawn flush operation on tokio's blocking thread pool
        runtime.clone().spawn_blocking(move || {
            debug!("flush: ino={}", ino);

            if write_buffer_enabled {
                // When write-buffering is enabled, flush() must NOT drain the write buffer.
                //
                // flush() fires on every close() — including read-only fds (e.g. Kodi seeking
                // while the DVR is still recording) and the DVR's own file descriptor between
                // individual write calls. Draining the buffer here causes two problems:
                //   1. A partial (sub-4MB) buffer gets emitted as a tiny chunk, destroying
                //      4MB alignment and causing all subsequent chunks to be tiny.
                //   2. The re-alignment pre-seed (partial last-chunk read-back on DVR restart)
                //      is discarded before enough new data has accumulated to fill a full chunk.
                //
                // The write buffer is already flushed correctly by:
                //   - The periodic background flusher (every 2s, aligned 4MB chunks only)
                //   - fsync() (aligned chunks only, same as background flusher)
                //   - release() with a write-mode fd (force=true, flushes everything including
                //     the partial tail on genuine close)
                //
                // So flush() just needs to acknowledge without touching the buffer.
                debug!("flush: ino={} - write buffer active, deferring to background flusher / release", ino);
                reply.ok();
                return;
            } else {
                // No write buffer, but we still need to flush any pending metadata updates
                // that were batched by the write() path to ensure data durability
                let result = runtime.block_on(async {
                    // Get metadata from cache
                    let metadata_opt = metadata_cache.get(&ino).map(|m| m.clone());

                    if let Some(metadata) = metadata_opt {
                        // Check if there are pending writes
                        let has_pending = {
                            let counters = write_counters.read().unwrap();
                            counters.get(&ino).map(|c| *c > 0).unwrap_or(false)
                        };

                        if has_pending {
                            debug!("flush: enqueueing pending metadata async for ino={}", ino);
                            client.enqueue_metadata(&metadata).await;
                            write_counters.write().unwrap().insert(ino, 0);
                        }
                    }

                    Ok::<(), anyhow::Error>(())
                });

                match result {
                    Ok(_) => reply.ok(),
                    Err(e) => {
                        error!("Failed to flush metadata for inode {}: {}", ino, e);
                        reply.error(libc::EIO);
                    }
                }
            }
        });
    }

    fn release(
        &mut self,
        req: &FuseRequest,
        ino: u64,
        _fh: u64,
        flags: i32,
        lock_owner: Option<u64>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        let pid = req.pid();
        debug!("release: ino={}, owner={:?}, pid={}", ino, lock_owner, pid);

        // Decrement write-mode open count when a write-mode fd is closed
        let is_write = (flags & libc::O_ACCMODE) != libc::O_RDONLY;
        let is_last_writer = if is_write {
            let mut remove = false;
            let mut last = false;
            if let Some(mut count) = self.write_open_counts.get_mut(&ino) {
                if *count > 0 { *count -= 1; }
                if *count == 0 { remove = true; last = true; }
            }
            if remove {
                self.write_open_counts.remove(&ino);
                self.size_high_water.remove(&ino);
            }
            last
        } else {
            false
        };

        let lock_manager = self.lock_manager.clone();

        if self.write_buffer_enabled {
            // Only flush and remove the write buffer when ALL write-mode fds are closed
            // (is_last_writer). Intermediate write closes (e.g. DVR opening a file twice
            // then closing one fd while the other is still writing) must NOT flush or
            // remove the shared buffer — doing so discards buffered data still being
            // written by the remaining fd, producing a corrupt small first chunk.
            // Read-only releases must also never touch the write buffer.
            if is_last_writer {
                let result = self.block_on(async {
                    // First flush writes (force metadata update on close)
                    self.flush_buffer_async(ino, true).await?;

                    // Then release all locks held by this owner (if lock_owner is provided)
                    if let Some(owner) = lock_owner {
                        lock_manager.release_all(ino, owner).await?;
                    }

                    Ok::<(), anyhow::Error>(())
                });

                // Clean up write buffer entry to prevent memory leak
                // Even if flush failed, we should remove the buffer entry
                self.write_buffers.remove(&ino);

                match result {
                    Ok(_) => reply.ok(),
                    Err(e) => {
                        error!("Failed to flush/release for inode {}: {}", ino, e);
                        reply.error(libc::EIO);
                    }
                }
            } else if is_write {
                // Intermediate write close: release locks only, leave buffer intact.
                // The remaining writer(s) will flush and remove the buffer on their close.
                if let Some(owner) = lock_owner {
                    let result = self.block_on(lock_manager.release_all(ino, owner));
                    if let Err(e) = result {
                        error!("Failed to release locks for inode {}: {}", ino, e);
                    }
                }
                reply.ok();
            } else {
                // Read-only close: release locks if needed, leave write buffer untouched.
                if let Some(owner) = lock_owner {
                    let result = self.block_on(lock_manager.release_all(ino, owner));
                    if let Err(e) = result {
                        error!("Failed to release locks for inode {}: {}", ino, e);
                    }
                }
                reply.ok();
            }
        } else {
            // No write buffer, but flush any pending metadata updates before releasing locks
            let client = self.client.clone();
            let metadata_cache = self.metadata_cache.clone();
            let write_counters = self.write_counters.clone();

            let result = self.block_on(async {
                // Flush pending metadata if any
                let metadata_opt = metadata_cache.get(&ino).map(|m| m.clone());

                if let Some(metadata) = metadata_opt {
                    // release: send synchronously so close() confirms metadata on leader.
                    debug!("release: flushing metadata sync for ino={} ({} chunks)", ino, metadata.chunks.len());
                    client.flush_metadata_sync(&metadata).await;
                    write_counters.write().unwrap().insert(ino, 0);
                }

                // Release locks if lock_owner is provided
                if let Some(owner) = lock_owner {
                    lock_manager.release_all(ino, owner).await?;
                }
                Ok::<(), anyhow::Error>(())
            });

            match result {
                Ok(_) => reply.ok(),
                Err(e) => {
                    error!("Failed to flush/release locks for inode {}: {}", ino, e);
                    reply.error(libc::EIO);
                }
            }
        }
    }

    fn mkdir(
        &mut self,
        _req: &FuseRequest,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        debug!("mkdir: parent={}, name={:?}, mode={:o}", parent, name, mode);

        let path = match self.get_path_from_parent(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Create directory metadata
        let metadata = FileMetadata {
            id: dfs_common::FileId::new(),
            path: path.clone(),
            size: 0,
            chunks: Vec::new(),
            chunk_sizes: Vec::new(),
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            modified_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            mode,
            uid: _req.uid(),
            gid: _req.gid(),
            file_type: FileType::Directory,
            chunk_locations: Vec::new(),
            write_seq: 0,
        };

        // Store metadata on cluster — spawn so we never block_on the FUSE dispatch thread.
        let client = self.client.clone();
        let metadata_clone = metadata.clone();
        let metadata_cache = self.metadata_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let next_inode = self.next_inode.clone();

        self.runtime.spawn(async move {
            match client.put_file_metadata(&metadata_clone).await {
                Ok(_) => {
                    let ino = {
                        let path_map = path_to_inode.read().unwrap();
                        if let Some(&existing) = path_map.get(&path) {
                            existing
                        } else {
                            drop(path_map);
                            let mut next = next_inode.write().unwrap();
                            let v = *next; *next += 1; drop(next);
                            path_to_inode.write().unwrap().insert(path.clone(), v);
                            v
                        }
                    };
                    metadata_cache.insert(ino, metadata.clone());

                    let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                    let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                    dir_cache.remove(parent_path);

                    let attr = DfsFilesystem::metadata_to_attr_static(ino, &metadata);
                    reply.entry(&Duration::from_secs(1), &attr, 0);
                }
                Err(e) => {
                    error!("Failed to create directory {}: {}", path, e);
                    reply.error(libc::EIO);
                }
            }
        });
    }

    fn unlink(&mut self, _req: &FuseRequest, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
        debug!("unlink: parent={}, name={:?}", parent, name);

        let path = match self.get_path_from_parent(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Delete file from cluster — spawn so we never block_on the FUSE dispatch thread.
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let write_buffers = self.write_buffers.clone();
        let write_counters = self.write_counters.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let last_warm_offset = self.last_warm_offset.clone();
        let chunk_offset_cache = self.chunk_offset_cache.clone();

        self.runtime.spawn(async move {
            match client.delete_file(&path).await {
                Ok(_) => {
                    if let Some(&ino) = path_to_inode.read().unwrap().get(&path) {
                        metadata_cache.remove(&ino);
                        write_buffers.remove(&ino);
                        write_counters.write().unwrap().remove(&ino);
                        last_metadata_update.remove(&ino);
                        last_warm_offset.remove(&ino);
                        chunk_offset_cache.remove(&ino);
                    }
                    path_to_inode.write().unwrap().remove(&path);

                    let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                    let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                    dir_cache.remove(parent_path);

                    reply.ok();
                }
                Err(e) => {
                    error!("Failed to delete file {}: {}", path, e);
                    reply.error(libc::EIO);
                }
            }
        });
    }

    fn rmdir(&mut self, _req: &FuseRequest, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
        debug!("rmdir: parent={}, name={:?}", parent, name);

        let path = match self.get_path_from_parent(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Check if directory is empty then delete — spawn so we never block_on the FUSE dispatch thread.
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let path_to_inode = self.path_to_inode.clone();

        self.runtime.spawn(async move {
            match client.list_directory(&path).await {
                Ok(entries) => {
                    if !entries.is_empty() {
                        reply.error(libc::ENOTEMPTY);
                        return;
                    }
                    match client.delete_file(&path).await {
                        Ok(_) => {
                            if let Some(&ino) = path_to_inode.read().unwrap().get(&path) {
                                metadata_cache.remove(&ino);
                            }
                            path_to_inode.write().unwrap().remove(&path);

                            let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                            let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                            dir_cache.remove(parent_path);

                            reply.ok();
                        }
                        Err(e) => {
                            error!("Failed to delete directory {}: {}", path, e);
                            reply.error(libc::EIO);
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to check directory {}: {}", path, e);
                    reply.error(libc::EIO);
                }
            }
        });
    }

    fn rename(
        &mut self,
        _req: &FuseRequest,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: fuser::ReplyEmpty,
    ) {
        debug!(
            "rename: parent={}, name={:?} -> newparent={}, newname={:?}",
            parent, name, newparent, newname
        );

        let old_path = match self.get_path_from_parent(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let new_path = match self.get_path_from_parent(newparent, newname) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Get existing metadata then rename — spawn so we never block_on the FUSE dispatch thread.
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let next_inode = self.next_inode.clone();

        self.runtime.spawn(async move {
            match client.get_file_metadata(&old_path).await {
                Ok(Some(metadata)) => {
                    match client.rename_file(&old_path, &new_path).await {
                        Ok(_) => {
                            // Keep the same inode number — kernel still holds references to it.
                            let ino = {
                                let path_map = path_to_inode.read().unwrap();
                                if let Some(&existing) = path_map.get(&old_path) {
                                    existing
                                } else {
                                    drop(path_map);
                                    let mut next = next_inode.write().unwrap();
                                    let v = *next; *next += 1; drop(next);
                                    path_to_inode.write().unwrap().insert(old_path.clone(), v);
                                    v
                                }
                            };

                            path_to_inode.write().unwrap().remove(&old_path);
                            path_to_inode.write().unwrap().insert(new_path.clone(), ino);

                            let mut new_metadata = metadata.clone();
                            new_metadata.path = new_path.clone();
                            new_metadata.modified_at = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs();
                            metadata_cache.insert(ino, new_metadata);

                            let raw_old = old_path.rsplitn(2, '/').nth(1).unwrap_or("");
                            let old_parent = if raw_old.is_empty() { "/" } else { raw_old };
                            let raw_new = new_path.rsplitn(2, '/').nth(1).unwrap_or("");
                            let new_parent = if raw_new.is_empty() { "/" } else { raw_new };
                            dir_cache.remove(old_parent);
                            if old_parent != new_parent {
                                dir_cache.remove(new_parent);
                            }

                            info!("Renamed {} -> {} (inode {} preserved {} chunks)", old_path, new_path, ino, metadata.chunks.len());
                            reply.ok();
                        }
                        Err(e) => {
                            error!("Failed to rename {} -> {}: {}", old_path, new_path, e);
                            reply.error(libc::EIO);
                        }
                    }
                }
                Ok(None) => reply.error(libc::ENOENT),
                Err(e) => {
                    error!("Failed to get file metadata {}: {}", old_path, e);
                    reply.error(libc::EIO);
                }
            }
        });
    }

    fn setattr(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        debug!("setattr: ino={}, mode={:?}, uid={:?}, gid={:?}, size={:?}",
               ino, mode, uid, gid, size);

        let mut metadata = match self.metadata_cache.get(&ino) {
            Some(m) => m.clone(),
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Update attributes
        if let Some(mode) = mode {
            metadata.mode = mode;
        }
        if let Some(uid) = uid {
            metadata.uid = uid;
        }
        if let Some(gid) = gid {
            metadata.gid = gid;
        }

        // Handle truncate
        if let Some(new_size) = size {
            if new_size != metadata.size {
                let client = self.client.clone();

                // Optimization: truncate to zero doesn't need to read old data
                // This fixes overwrite scenarios (dd, DVR recording restart) where old chunks
                // may be unavailable due to garbage collection or incomplete replication
                if new_size == 0 {
                    // Just clear the metadata - no need to read old chunks
                    metadata.chunks = Vec::new();
                    metadata.chunk_sizes = Vec::new();
                    metadata.chunk_locations = Vec::new();
                    metadata.size = 0;
                } else if new_size > metadata.size {
                    // Growing file - just update metadata to extend with zeros
                    // No need to read existing data, just keep existing chunks
                    info!("Truncate growing: {} -> {} bytes (keeping {} chunks)",
                          metadata.size, new_size, metadata.chunks.len());
                    metadata.size = new_size;
                    // Note: The chunks stay the same, reads beyond will return zeros via FUSE
                } else {
                    // Shrinking file - only read chunks up to new_size
                    // CRITICAL FIX: Don't read entire file for truncate!
                    info!("Truncate shrinking: {} -> {} bytes", metadata.size, new_size);

                    if metadata.chunks.is_empty() {
                        metadata.size = new_size;
                    } else {
                        let chunk_sizes = metadata.chunk_sizes.clone();

                        // Find which chunks we need to keep (up to new_size)
                        let mut cumulative_size = 0u64;
                        let mut last_chunk_idx = 0;
                        let mut bytes_in_last_chunk = 0u64;

                        for (idx, &size) in chunk_sizes.iter().enumerate() {
                            if cumulative_size + size <= new_size {
                                // Entire chunk is kept
                                cumulative_size += size;
                                last_chunk_idx = idx;
                            } else if cumulative_size < new_size {
                                // This chunk is partially kept
                                bytes_in_last_chunk = new_size - cumulative_size;
                                last_chunk_idx = idx;
                                break;
                            } else {
                                break;
                            }
                        }

                        if bytes_in_last_chunk > 0 {
                            // Need to read and truncate the last partial chunk
                            info!("Truncate: keeping {} full chunks, truncating chunk {} to {} bytes",
                                  last_chunk_idx, last_chunk_idx, bytes_in_last_chunk);

                            let chunk_id = metadata.chunks[last_chunk_idx];
                            let chunk_offset: u64 = chunk_sizes[..last_chunk_idx].iter().sum();
                            let chunk_size = chunk_sizes[last_chunk_idx] as usize;

                            // Build read hint for the last partial chunk (full chunk read for truncate)
                            let read_hint = vec![crate::client::ChunkReadHint {
                                chunk_idx: last_chunk_idx,
                                chunk_id,
                                full_chunk: true,  // Read full chunk for truncate operation
                                offset_in_chunk: 0,
                                length: chunk_size,
                                file_offset: chunk_offset,
                            }];

                            // Read only the last partial chunk
                            let last_chunk_data = match self.block_on(async {
                                client.read_data(&read_hint, &metadata.chunks, ino, &metadata.chunk_locations).await
                            }) {
                                Ok(data) => data,
                                Err(e) => {
                                    error!("Failed to read last chunk for truncate: {}", e);
                                    reply.error(libc::EIO);
                                    return;
                                }
                            };

                            // Truncate the chunk data
                            let truncated_chunk = &last_chunk_data[..bytes_in_last_chunk as usize];

                            // Write back the truncated chunk
                            match self.block_on(async {
                                client.write_data_with_cache(truncated_chunk, ino, chunk_offset).await
                            }) {
                                Ok((new_chunk_ids, new_chunk_sizes, chunk_locations_opt)) => {
                                    // Keep chunks before last, add truncated chunk
                                    let mut new_all_chunks = metadata.chunks[..last_chunk_idx].to_vec();
                                    new_all_chunks.extend(new_chunk_ids);

                                    let mut new_all_sizes = metadata.chunk_sizes[..last_chunk_idx].to_vec();
                                    new_all_sizes.extend(new_chunk_sizes);

                                    // Update chunk_locations if provided
                                    if let Some(chunk_locations) = chunk_locations_opt {
                                        let mut new_all_locations = if last_chunk_idx < metadata.chunk_locations.len() {
                                            metadata.chunk_locations[..last_chunk_idx].to_vec()
                                        } else {
                                            Vec::new()
                                        };
                                        new_all_locations.extend(chunk_locations);
                                        metadata.chunk_locations = new_all_locations;
                                    }

                                    metadata.chunks = new_all_chunks;
                                    metadata.chunk_sizes = new_all_sizes;
                                    metadata.size = new_size;
                                }
                                Err(e) => {
                                    error!("Failed to write truncated chunk: {}", e);
                                    reply.error(libc::EIO);
                                    return;
                                }
                            }
                        } else {
                            // All chunks are complete, just drop the ones after
                            info!("Truncate: keeping {} full chunks, dropping rest", last_chunk_idx + 1);
                            metadata.chunks.truncate(last_chunk_idx + 1);
                            metadata.chunk_sizes.truncate(last_chunk_idx + 1);
                            metadata.chunk_locations.truncate(last_chunk_idx + 1);
                            metadata.size = new_size;
                        }
                    }
                }
            }
        }

        metadata.modified_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Store updated metadata
        let client = self.client.clone();
        let metadata_clone = metadata.clone();
        let result = self.block_on(async {
            client.put_file_metadata(&metadata_clone).await
        });

        match result {
            Ok(_) => {
                // Update cache
                self.metadata_cache.insert(ino, metadata.clone());

                // Convert to FUSE attr
                let attr = self.metadata_to_attr(ino, &metadata);
                // Short TTL (2s) for multi-client coherency
                reply.attr(&Duration::from_secs(2), &attr);
            }
            Err(e) => {
                error!("Failed to update attributes: {}", e);
                reply.error(libc::EIO);
            }
        }
    }

    fn statfs(&mut self, _req: &FuseRequest, _ino: u64, reply: ReplyStatfs) {
        debug!("statfs");

        const BLOCK_SIZE: u32 = 4096;
        const CACHE_TTL_SECS: u64 = 30; // Cache for 30 seconds

        // Check cache first
        let cached = {
            let cache = self.statfs_cache.read().unwrap();
            cache.as_ref().and_then(|(total, free, avail, timestamp)| {
                if timestamp.elapsed().as_secs() < CACHE_TTL_SECS {
                    debug!("statfs cache HIT (age: {}s)", timestamp.elapsed().as_secs());
                    Some((*total, *free, *avail))
                } else {
                    debug!("statfs cache EXPIRED");
                    None
                }
            })
        };

        if let Some((total, free, avail)) = cached {
            reply.statfs(total, free, avail, 0, 0, BLOCK_SIZE, 255, BLOCK_SIZE);
            return;
        }

        // Cache miss — spawn so we never block_on the FUSE dispatch thread.
        debug!("statfs cache MISS - querying cluster");
        let client = self.client.clone();
        let statfs_cache = self.statfs_cache.clone();

        self.runtime.spawn(async move {
            match client.get_storage_stats().await {
                Ok((total_space, free_space, available_space, _replication_factor)) => {
                    let total = total_space / BLOCK_SIZE as u64;
                    let free = free_space / BLOCK_SIZE as u64;
                    let avail = available_space / BLOCK_SIZE as u64;
                    *statfs_cache.write().unwrap() = Some((total, free, avail, std::time::Instant::now()));
                    reply.statfs(total, free, avail, 0, 0, BLOCK_SIZE, 255, BLOCK_SIZE);
                }
                Err(e) => {
                    error!("Failed to get storage stats: {}", e);
                    reply.statfs(1_000_000_000, 500_000_000, 500_000_000, 0, 0, BLOCK_SIZE, 255, BLOCK_SIZE);
                }
            }
        });
    }

    fn access(&mut self, _req: &FuseRequest, ino: u64, mask: i32, reply: fuser::ReplyEmpty) {
        debug!("access: ino={}, mask={}", ino, mask);

        // Check if inode exists
        if self.metadata_cache.contains_key(&ino) {
            // For simplicity, allow all access
            // A real implementation would check permissions based on mask
            reply.ok();
        } else {
            reply.error(libc::ENOENT);
        }
    }

    fn fsync(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        datasync: bool,
        reply: fuser::ReplyEmpty,
    ) {
        debug!("fsync: ino={}, datasync={}", ino, datasync);

        if self.write_buffer_enabled {
            // Check whether this inode was opened with O_SYNC/O_DSYNC.
            // If so, flush immediately (database/journaling mode).
            // If not, spawn the flush as a background task so the FUSE dispatch thread is
            // not blocked (DVR/streaming mode — coalesced fsyncs are fine).
            let sync_on_fsync = self.write_buffers.get(&ino)
                .map(|state_lock| {
                    // Try non-blocking read; if locked just default to async flush
                    state_lock.try_lock().map(|s| s.sync_on_fsync).unwrap_or(false)
                })
                .unwrap_or(false);

            let in_flight_opt = self.flush_in_flight.read().unwrap().clone();

            if sync_on_fsync {
                // O_SYNC / O_DSYNC: flush synchronously and wait for network ack
                // before returning reply.ok() to the caller.
                if let Some(ref in_flight) = in_flight_opt {
                    in_flight.insert(ino);
                }
                let handle = self.flush_handle.clone();
                let in_flight_for_sync = in_flight_opt.clone();
                let result = self.block_on(async move {
                    let r = handle.flush_buffer_async(ino, true).await;
                    if let Some(ref inf) = in_flight_for_sync { inf.remove(&ino); }
                    r
                });
                match result {
                    Ok(_) => reply.ok(),
                    Err(e) => { error!("fsync (O_SYNC) failed for inode {}: {}", ino, e); reply.error(libc::EIO); }
                }
            } else if let Some(in_flight) = in_flight_opt {
                // Async flush: don't block the FUSE thread
                in_flight.insert(ino);
                let handle = self.flush_handle.clone();
                let in_flight_clone = in_flight.clone();
                self.runtime.spawn(async move {
                    if let Err(e) = handle.flush_buffer_async(ino, true).await {
                        error!("fsync background flush failed for inode {}: {}", ino, e);
                    }
                    in_flight_clone.remove(&ino);
                });
                reply.ok();
            } else {
                // Background flusher not yet started — fall back to synchronous flush
                let result = self.block_on(self.flush_buffer_async(ino, true));
                match result {
                    Ok(_) => reply.ok(),
                    Err(e) => {
                        error!("Failed to fsync inode {}: {}", ino, e);
                        reply.error(libc::EIO);
                    }
                }
            }
        } else {
            // No write buffer, but we still need to flush any pending metadata updates
            // that were batched by the write() path to ensure data durability
            let client = self.client.clone();
            let metadata_cache = self.metadata_cache.clone();
            let write_counters = self.write_counters.clone();

            let result = self.block_on(async {
                // Get metadata from cache
                let metadata_opt = metadata_cache.get(&ino).map(|m| m.clone());

                if let Some(metadata) = metadata_opt {
                    // Check if there are pending writes
                    let has_pending = {
                        let counters = write_counters.read().unwrap();
                        counters.get(&ino).map(|c| *c > 0).unwrap_or(false)
                    };

                    if has_pending {
                        debug!("fsync: enqueueing pending metadata async for ino={}", ino);
                        client.enqueue_metadata(&metadata).await;
                        write_counters.write().unwrap().insert(ino, 0);
                    }
                }

                Ok::<(), anyhow::Error>(())
            });

            match result {
                Ok(_) => reply.ok(),
                Err(e) => {
                    error!("Failed to flush metadata on fsync for inode {}: {}", ino, e);
                    reply.error(libc::EIO);
                }
            }
        }
    }

    fn getxattr(
        &mut self,
        _req: &FuseRequest,
        _ino: u64,
        name: &std::ffi::OsStr,
        size: u32,
        reply: fuser::ReplyXattr,
    ) {
        debug!("getxattr: name={:?}, size={}", name, size);
        // We don't support extended attributes
        reply.error(libc::ENODATA);
    }

    fn listxattr(&mut self, _req: &FuseRequest, _ino: u64, size: u32, reply: fuser::ReplyXattr) {
        debug!("listxattr: size={}", size);
        // We don't support extended attributes, return empty list
        if size == 0 {
            // Query size
            reply.size(0);
        } else {
            // Return empty list
            reply.data(&[]);
        }
    }

    fn setlk(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        lock_owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        sleep: bool,
        reply: fuser::ReplyEmpty,
    ) {
        debug!(
            "setlk: ino={}, owner={}, pid={}, type={}, range=[{}, {}), sleep={}",
            ino, lock_owner, pid, typ, start, end, sleep
        );

        let lock_manager = self.lock_manager.clone();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            use crate::locks::LockType;

            // Convert FUSE range to our representation
            // end=u64::MAX means "to EOF" (len=0)
            let len = if end == u64::MAX { 0 } else { end.saturating_sub(start) };

            let result = match typ {
                libc::F_RDLCK => {
                    // Shared (read) lock
                    if sleep {
                        lock_manager.lock_wait(ino, lock_owner, pid, LockType::Shared, start, len).await
                    } else {
                        lock_manager.try_lock(ino, lock_owner, pid, LockType::Shared, start, len).await
                    }
                }
                libc::F_WRLCK => {
                    // Exclusive (write) lock
                    if sleep {
                        lock_manager.lock_wait(ino, lock_owner, pid, LockType::Exclusive, start, len).await
                    } else {
                        lock_manager.try_lock(ino, lock_owner, pid, LockType::Exclusive, start, len).await
                    }
                }
                libc::F_UNLCK => {
                    // Unlock
                    lock_manager.unlock(ino, lock_owner, start, len).await
                }
                _ => {
                    error!("Invalid lock type: {}", typ);
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            match result {
                Ok(()) => reply.ok(),
                Err(e) => {
                    if sleep {
                        // Blocking lock failed (should be rare)
                        error!("Lock acquisition failed: {}", e);
                        reply.error(libc::EIO);
                    } else {
                        // Non-blocking lock would block (conflict detected)
                        reply.error(libc::EAGAIN);
                    }
                }
            }
        });
    }

    fn getlk(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        lock_owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: fuser::ReplyLock,
    ) {
        debug!(
            "getlk: ino={}, owner={}, pid={}, type={}, range=[{}, {})",
            ino, lock_owner, pid, typ, start, end
        );

        let lock_manager = self.lock_manager.clone();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            use crate::locks::LockType;

            let lock_type = match typ {
                libc::F_RDLCK => LockType::Shared,
                libc::F_WRLCK => LockType::Exclusive,
                _ => {
                    error!("Invalid lock type for getlk: {}", typ);
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            let len = if end == u64::MAX { 0 } else { end.saturating_sub(start) };

            match lock_manager.get_conflict(ino, lock_owner, pid, lock_type, start, len).await {
                Some(conflict) => {
                    // Return conflicting lock details
                    let fuse_type = match conflict.lock_type {
                        LockType::Shared => libc::F_RDLCK,
                        LockType::Exclusive => libc::F_WRLCK,
                    };

                    let conflict_end = if conflict.len == 0 {
                        u64::MAX
                    } else {
                        conflict.start.saturating_add(conflict.len)
                    };

                    debug!(
                        "getlk: found conflict with owner={} pid={} type={} range=[{}, {})",
                        conflict.owner, conflict.pid, fuse_type, conflict.start, conflict_end
                    );

                    reply.locked(conflict.start, conflict_end, fuse_type, conflict.pid);
                }
                None => {
                    // No conflict - return F_UNLCK to indicate lock would succeed
                    debug!("getlk: no conflict found, lock would succeed");
                    reply.locked(start, end, libc::F_UNLCK, pid);
                }
            }
        });
    }
}
