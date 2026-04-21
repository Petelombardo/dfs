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
pub fn is_sqlite_for_cache(path: &str) -> bool {
    is_sqlite_path(path)
}

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

/// RAII guard that decrements a per-inode write-task counter on drop.
/// Used by write() spawned tasks so release() can wait for all pending writes
/// to land in the slot before flushing.
struct WriteTaskGuard(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for WriteTaskGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A single 4MB-aligned write buffer slot for one chunk.
/// Writes land in the slot at `file_offset % CHUNK_SIZE`.
/// The slot is flushed when it fills (exactly CHUNK_SIZE) or on fsync/release/timer.
#[derive(Clone)]
struct ChunkSlot {
    /// Buffered bytes for this chunk; capacity is at most CHUNK_SIZE
    data: Vec<u8>,
    /// When this slot was last written to
    last_modified: SystemTime,
    /// Bytes at the front of `data` that were zero-filled by our gap logic, not written
    /// by the application. Used to distinguish a real all-zero write from a synthetic gap
    /// prefix — prevents PatchChunk from overwriting real server data with our zeros.
    gap_filled_prefix: usize,
    /// How many bytes were already flushed to the server for this chunk before this slot
    /// was created (i.e. the slot was recreated after a flush). Reads within 0..server_prefix
    /// should be served from the server, not from our zero-filled slot data.
    server_prefix: usize,
}

impl ChunkSlot {
    fn new() -> Self {
        Self {
            data: Vec::with_capacity(CHUNK_SIZE),
            last_modified: SystemTime::now(),
            gap_filled_prefix: 0,
            server_prefix: 0,
        }
    }

    fn new_post_flush(server_bytes: usize) -> Self {
        Self {
            data: Vec::with_capacity(CHUNK_SIZE),
            last_modified: SystemTime::now(),
            // Pre-seed gap_filled_prefix so that is_append_extend correctly identifies
            // new bytes written into this slot as an extension of the server's committed
            // data, rather than treating them as a fresh WriteData.
            gap_filled_prefix: server_bytes,
            server_prefix: server_bytes,
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
    /// Tracks how many bytes were flushed for each chunk index. When a slot is removed after
    /// a successful flush and later re-created (e.g. DVR appending into a partially-flushed
    /// chunk), write_at uses this to set server_prefix so the read path can fall through to
    /// the network for bytes already on the server.
    flushed_sizes: HashMap<u64, usize>,
    /// If true, every fsync() must flush immediately (O_SYNC / O_DSYNC was set on open).
    /// If false, fsyncs within the coalescing window are absorbed (DVR / streaming mode).
    sync_on_fsync: bool,
}

impl InodeWriteState {
    fn new(sync_on_fsync: bool) -> Self {
        Self {
            slots: HashMap::new(),
            flushed_sizes: HashMap::new(),
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
            let flushed = self.flushed_sizes.get(&idx).copied().unwrap_or(0);
            let slot = self.slots.entry(idx).or_insert_with(|| ChunkSlot::new_post_flush(flushed));

            // Grow slot to cover intra_offset (may need zero-fill for sparse-within-chunk)
            if slot.data.len() < intra {
                // Track how far the zero-fill extends so the read path can fall through
                // to the server for this range rather than serving synthetic zeros.
                let fill_end = intra;
                if slot.gap_filled_prefix < fill_end {
                    slot.gap_filled_prefix = fill_end;
                }
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
    dir_cache: Arc<DashMap<String, (Vec<FileMetadata>, std::time::Instant)>>,
    path_to_inode: Arc<RwLock<HashMap<String, u64>>>,
    /// Inodes that received a setattr(size=0) truncate while a flush was in progress.
    /// Prevents a racing flush from re-populating metadata with stale chunk locations.
    /// Cleared once fresh write data lands (first successful chunk update).
    truncated_inodes: Arc<dashmap::DashSet<u64>>,
    /// Dedicated runtime for chunk network I/O. Isolated from the main runtime so
    /// flush sub-tasks (which do blocking network writes) never starve write reply
    /// tasks, which must run to unblock the kernel's FUSE write queue.
    flush_runtime: Arc<tokio::runtime::Runtime>,
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
                        // Background tick: flush full (4MB) slots only.
                        // Partial slots must not be flushed here — doing so creates small stub
                        // chunks on the server while the DVR is still writing into them. When
                        // the client later reads back those positions it only gets the stub bytes,
                        // causing the player to stall or exit. Partial slots are flushed on
                        // fsync/release (force=true) only.
                        let mut indices = state.full_slot_indices();
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
                .map(|m| m.chunk_locations.len() as u64)
                .unwrap_or(0);
            if meta_chunk_count > 0 { Some(meta_chunk_count - 1) } else { None }
        };

        let mut slots_to_write: Vec<(u64, Vec<u8>, u64)> = Vec::new(); // (chunk_idx, data, file_offset)
        let mut patch_metadata_dirty = false; // true if any PatchChunk succeeded (needs metadata flush)
        for chunk_idx in &indices_to_flush {
            let Some(state_lock) = self.write_buffers.get(&ino) else { continue };
            // Snapshot slot data and drop the mutex before any network I/O.
            // Holding a tokio Mutex across an .await blocks concurrent getattr/read/write
            // on the same inode, causing observable stalls when reading while recording.
            let (slot_data, file_offset) = {
                let state = state_lock.lock().await;
                match state.slots.get(chunk_idx) {
                    Some(slot) if !slot.data.is_empty() =>
                        (slot.data.clone(), chunk_idx * CHUNK_SIZE as u64),
                    _ => continue,
                }
            }; // mutex released here

            let slot_len = slot_data.len();

            // Use PatchChunk for two cases:
            //   1. In-place overwrite: slot_len <= existing_chunk_size (overlay at intra=0)
            //   2. Append/extend: slot starts with existing data at intra=0..existing_size,
            //      and new bytes extend the chunk. We send only the NEW bytes at intra=existing_size.
            // Both require the chunk to already exist on the server and the slot to be partial (<4MB).
            // Prefer flushed_sizes (authoritative for this session) over metadata_cache
            // (which may lag by a flush cycle). This prevents a stale metadata_cache from
            // causing chunk_exists=false and triggering a full WriteData that overwrites
            // the server's real header bytes with our zero-prefixed slot.
            let existing_chunk_size = {
                let from_flushed = self.write_buffers.get(&ino)
                    .and_then(|s| s.try_lock().ok()
                        .and_then(|st| st.flushed_sizes.get(chunk_idx).copied()));
                from_flushed.unwrap_or_else(|| {
                    self.metadata_cache.get(&ino)
                        .and_then(|m| m.chunk_locations.get(*chunk_idx as usize).map(|l| l.size))
                        .unwrap_or(0)
                })
            };
            let chunk_exists = existing_chunk_size > 0
                || max_flushed_idx.map(|max| *chunk_idx <= max).unwrap_or(false);
            // Detect append: the slot was gap-zero-filled up to existing_chunk_size, then
            // real data was appended beyond. Use gap_filled_prefix (set explicitly when we
            // zero-fill) rather than checking for all-zeros — the all-zeros heuristic was
            // wrong when the server's real chunk 0 data happened to start with zeros.
            let gap_filled_prefix = {
                let state_lock = self.write_buffers.get(&ino);
                state_lock.and_then(|s| s.try_lock().ok()
                    .and_then(|st| st.slots.get(&chunk_idx).map(|sl| sl.gap_filled_prefix)))
                    .unwrap_or(0)
            };
            let is_append_extend = chunk_exists
                && slot_len < CHUNK_SIZE
                && slot_len > existing_chunk_size
                && gap_filled_prefix >= existing_chunk_size;
            let is_overwrite = chunk_exists && slot_len < CHUNK_SIZE && slot_len <= existing_chunk_size;
            let needs_patch = is_overwrite || is_append_extend;

            if needs_patch {
                let (patch_intra, patch_bytes) = if is_append_extend {
                    // Send only the new appended bytes, starting at the old chunk boundary.
                    (existing_chunk_size, slot_data[existing_chunk_size..].to_vec())
                } else {
                    // Full overlay from byte 0 (DVR header / in-place overwrite).
                    (0, slot_data.clone())
                };
                info!("flush_buffer_async: slot {} ({} bytes) — PatchChunk intra={} patch_len={}",
                      chunk_idx, slot_len, patch_intra, patch_bytes.len());
                let meta = self.metadata_cache.get(&ino).map(|m| m.clone());
                let patched = if let Some(meta) = meta {
                    let chunk_idx_usize = *chunk_idx as usize;
                    let old_location_opt = meta.chunk_locations.get(chunk_idx_usize).cloned();
                    if let Some(old_location) = old_location_opt {
                        match self.client.patch_chunk_on_replicas(
                            old_location.chunk_id,
                            file_offset,
                            patch_intra,
                            patch_bytes,
                            &old_location,
                        ).await {
                            Ok(new_location) => {
                                info!("flush_buffer_async: PatchChunk slot {} succeeded: {} -> {}",
                                      chunk_idx, old_location.chunk_id, new_location.chunk_id);
                                if let Some(mut meta_entry) = self.metadata_cache.get_mut(&ino) {
                                    if let Some(loc) = meta_entry.chunk_locations.get_mut(chunk_idx_usize) {
                                        *loc = new_location.clone();
                                    }
                                    if let Some(new_size) = meta_entry.chunk_locations.iter()
                                        .filter_map(|l| l.file_offset.map(|o| o + l.size as u64))
                                        .reduce(u64::max)
                                    {
                                        meta_entry.size = new_size;
                                    }
                                }
                                patch_metadata_dirty = true;
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

        if slots_to_write.is_empty() {
            // Nothing to write normally, but if PatchChunk updated metadata we still need to persist it.
            if patch_metadata_dirty {
                let meta_to_persist = self.metadata_cache.get(&ino).map(|m| m.clone());
                if let Some(meta) = meta_to_persist {
                    if force {
                        self.client.flush_metadata_sync(&meta).await;
                    } else {
                        self.client.enqueue_metadata(&meta).await;
                    }
                }
            }
            return Ok(());
        }

        info!("flush_buffer_async: flushing {} chunks in parallel for inode {}", slots_to_write.len(), ino);

        // Flush all slots in parallel. When flush_buffer_async runs on the flush_runtime
        // (as dispatched by the background flusher and release/fsync callers), these
        // tokio::spawn calls land on flush_runtime workers — isolated from the main runtime.
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
                        if let Some(removed) = state.slots.remove(chunk_idx) {
                            // Remember how many bytes this chunk has on the server so that
                            // if the slot is recreated (e.g. DVR appending into a partial
                            // chunk), the read path can bypass our zero-fill prefix.
                            state.flushed_sizes.insert(*chunk_idx, removed.data.len());
                        }
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

        info!("flush_buffer_async: got {} chunk locations from {} slot writes", all_locations.len(), slots_to_write.len());
        if all_locations.is_empty() && !patch_metadata_dirty {
            return Ok(());
        }

        // Update metadata cache: insert new chunk_locations in file-offset order,
        // deduplicate by file_offset (supersedes chunk_id dedup), and recalculate file size.
        // If the entry isn't cached yet (create()'s async task may still be in flight),
        // fetch from server so we have a valid base to merge chunk locations into.
        if !self.metadata_cache.contains_key(&ino) {
            let path_opt = self.path_to_inode.read().unwrap()
                .iter().find(|(_, &v)| v == ino).map(|(k, _)| k.clone());
            match path_opt {
                None => {
                    // No path → file was deleted; skip metadata commit.
                    debug!("flush_buffer_async: ino={} not in path_to_inode (deleted) — skipping metadata", ino);
                    return Ok(());
                }
                Some(path) => {
                    match self.client.get_file_metadata(&path).await {
                        Ok(Some(fetched)) => {
                            self.client.seed_write_seq(fetched.id, fetched.write_seq);
                            self.metadata_cache.insert(ino, fetched);
                        }
                        Ok(None) | Err(_) => {
                            // File not on server yet or fetch failed — create() still in flight.
                            // destroy() sweep will commit once create() finishes.
                            warn!("flush_buffer_async: ino={} ({}) metadata not available — deferring to destroy()", ino, path);
                            return Ok(());
                        }
                    }
                }
            }
        }

        {
            if let Some(mut meta) = self.metadata_cache.get_mut(&ino) {
                // If setattr(size=0) raced with this flush, discard stale locations.
                // We use an explicit flag (set by setattr, cleared here on first fresh write)
                // rather than checking meta.size==0, which is also true for brand-new files.
                if self.truncated_inodes.contains(&ino) {
                    info!("flush_buffer_async: ino={} was truncated to zero during flush — discarding stale chunk locations", ino);
                    return Ok(());
                }
                for loc in &all_locations {
                    if !meta.chunk_locations.iter().any(|l| l.chunk_id == loc.chunk_id) {
                        // Replace any existing entry at the same file_offset (from a previous
                        // partial flush of this slot). Content-addressed hashes differ when the
                        // same slot is flushed twice (partial then full), so chunk_id dedup alone
                        // misses this — we must also dedup by file_offset.
                        if let Some(offset) = loc.file_offset {
                            if let Some(pos) = meta.chunk_locations.iter().position(|l| l.file_offset == Some(offset)) {
                                meta.chunk_locations[pos] = loc.clone();
                                continue;
                            }
                        }
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
            // Update the parent directory cache with our current metadata so readdir
            // reflects writes the client itself made without waiting for a server round-trip.
            // The client already knows what it wrote — no need to ask the server.
            let parent_path = meta.path.rfind('/').map(|i| {
                let p = &meta.path[..i];
                if p.is_empty() { "/".to_string() } else { p.to_string() }
            });
            if let Some(parent) = parent_path {
                if let Some(mut dir_entry) = self.dir_cache.get_mut(&parent) {
                    let (entries, _) = &mut *dir_entry;
                    if let Some(pos) = entries.iter().position(|e| e.id == meta.id) {
                        entries[pos] = meta.clone();
                    } else {
                        entries.push(meta.clone());
                    }
                }
                // If not in dir cache yet, just invalidate so next readdir fetches fresh.
                else {
                    self.dir_cache.remove(&parent);
                }
            }

            if force {
                // release/fsync: flush metadata synchronously so new chunk IDs survive restart.
                self.client.flush_metadata_sync(&meta).await;
                // Record the update time so getattr returns TTL=0 for the post-close window
                // (O_APPEND openers need the current size before their open() call).
                self.last_metadata_update.insert(ino, std::time::Instant::now());
            } else {
                // Background tick: push directly into the queue (no back-pressure wait).
                // enqueue_metadata() may block waiting to rescue a stalled queue entry,
                // which would hold the in_flight slot and prevent new background flushes
                // from starting — starving the write pipeline. We stamp the seq and push
                // directly; the queue worker handles delivery and retries independently.
                const METADATA_FLUSH_INTERVAL_SECS: u64 = 5;
                let should_enqueue = match self.last_metadata_update.get(&ino) {
                    None => true,
                    Some(last) => last.elapsed() >= std::time::Duration::from_secs(METADATA_FLUSH_INTERVAL_SECS),
                };
                if should_enqueue {
                    self.last_metadata_update.insert(ino, std::time::Instant::now());
                    let stamped = self.client.stamp_write_seq_pub(&meta);
                    self.client.metadata_queue.push(stamped).await;
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
    /// Cache directory listings for 30 seconds to avoid repeated scans
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

    /// Total open file handle count per inode (read + write).
    /// When this reaches zero on release(), the read engine is dropped to free memory.
    open_counts: Arc<DashMap<u64, usize>>,

    /// High-water mark of reported file size per inode.
    /// Prevents getattr from reporting a smaller size during the window between a
    /// slot being flushed (removed from write_buffers) and the metadata being committed.
    /// Cleared on release() once the file is fully closed.
    size_high_water: Arc<DashMap<u64, u64>>,

    /// Inodes that received setattr(size=0) while a flush was already in progress.
    /// See FlushHandle::truncated_inodes for details.
    truncated_inodes: Arc<dashmap::DashSet<u64>>,

    /// Shared reference to the background flusher's in-flight set.
    /// Set by the background flusher task after spawn; flush_buffer_async (fsync/close)
    /// waits for any in-flight background flush to complete before sending its own flush
    /// to avoid concurrent flushes that would produce OffsetMismatch.
    flush_in_flight: Arc<RwLock<Option<Arc<dashmap::DashSet<u64>>>>>,

    /// Dedicated runtime for chunk network I/O. Isolated from the main runtime.
    flush_runtime: Arc<tokio::runtime::Runtime>,

    /// Cloneable handle used by fsync() to spawn background flush tasks.
    flush_handle: FlushHandle,

    /// Paths for which an unlink is in flight to the server.
    /// lookup() returns ENOENT for these paths so that a concurrent create/open
    /// does not race with the still-pending server-side delete.
    pending_deletes: Arc<dashmap::DashSet<String>>,

    /// Inodes for which a background getattr refresh is already in flight.
    /// Prevents unbounded spawning of concurrent refresh tasks (one per inode max).
    refreshing_inodes: Arc<dashmap::DashSet<u64>>,

    /// Count of release() flush tasks still in flight.
    /// destroy() waits for this to reach zero before exiting so no in-progress
    /// release flush is interrupted mid-write by process shutdown.
    release_in_flight: Arc<std::sync::atomic::AtomicUsize>,

    /// Per-inode count of write() tasks still running (spawned but not yet written into the slot).
    /// release() waits for this to reach zero before flush so we don't flush an incomplete slot.
    write_tasks_in_flight: Arc<DashMap<u64, Arc<std::sync::atomic::AtomicUsize>>>,

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
        let truncated_inodes_shared: Arc<dashmap::DashSet<u64>> = Arc::new(dashmap::DashSet::new());
        let last_metadata_update_shared: Arc<DashMap<u64, std::time::Instant>> =
            Arc::new(DashMap::new());

        let write_open_counts: Arc<DashMap<u64, usize>> = Arc::new(DashMap::new());
        let open_counts: Arc<DashMap<u64, usize>> = Arc::new(DashMap::new());

        let dir_cache_shared: Arc<DashMap<String, (Vec<FileMetadata>, std::time::Instant)>> =
            Arc::new(DashMap::new());


        // Dedicated runtime for chunk network I/O during flush.
        // Isolated from the main runtime so flush sub-tasks (network writes) never
        // saturate the main pool and starve write reply tasks.
        let flush_runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(8)
                .enable_all()
                .thread_name("dfs-flush")
                .build()
                .expect("Failed to build flush runtime")
        );

        // Start background task to flush expired write buffers (if buffering enabled)
        if write_buffer_enabled {
            let write_buffers_clone = write_buffers_for_cleanup.clone();
            let client_for_cleanup = client.clone();
            let metadata_cache_for_cleanup = metadata_cache.clone();
            let write_open_counts_for_bg = write_open_counts.clone();
            let path_to_inode_for_bg = path_to_inode.clone();
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
                dir_cache: dir_cache_shared.clone(),
                path_to_inode: path_to_inode_for_bg.clone(),
                truncated_inodes: truncated_inodes_shared.clone(),
                flush_runtime: flush_runtime.clone(),
            };
            runtime.spawn(async move {
                let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(100));
                loop {
                    interval.tick().await;

                    // Find inodes with full chunk slots ready for background flush.
                    // A full slot (exactly 4MB) is safe to flush without waiting for fsync.
                    // Partial slots are also flushed if they haven't been written to for 2s
                    // (file may have stopped growing — drain it rather than holding memory).
                    // Collect keys first (brief DashMap read lock, no await held).
                    // Holding a DashMap shard lock across an .await is a deadlock:
                    // write() calls .entry().or_insert_with() (needs DashMap write lock)
                    // while holding the buffer mutex — if the ticker holds the shard read
                    // lock while awaiting the buffer mutex, both tasks block each other.
                    let inodes: Vec<u64> = write_buffers_clone.iter()
                        .map(|e| *e.key())
                        .collect();

                    let mut flush_inodes: Vec<u64> = Vec::new();
                    for ino in inodes {
                        if in_flight.contains(&ino) { continue; }
                        let state_arc = match write_buffers_clone.get(&ino) {
                            Some(a) => a.clone(),
                            None => continue,
                        };
                        let state = state_arc.lock().await;
                        let has_full = !state.full_slot_indices().is_empty();
                        // A partial/idle slot is safe to flush only when there are no
                        // active write-mode fds — i.e. the writer has already closed the
                        // file but the buffer wasn't flushed in release() for some reason
                        // (e.g. rename-based save). DVR always has an open fd, so it is
                        // never flushed prematurely here.
                        let no_active_writers = write_open_counts_for_bg
                            .get(&ino).map(|c| *c == 0).unwrap_or(true);
                        let has_idle = no_active_writers && state.slots.iter().any(|(_, s)| {
                            s.is_idle() && !s.data.is_empty()
                        });
                        if has_full || has_idle {
                            flush_inodes.push(ino);
                        }
                    }

                    for ino in flush_inodes {
                        in_flight.insert(ino);
                        let handle = flush_handle_for_bg.clone();
                        let in_flight_task = in_flight.clone();
                        let flush_rt = handle.flush_runtime.clone();

                        // Spawn on flush_runtime so chunk network I/O runs isolated from
                        // the main runtime, preventing worker starvation of write reply tasks.
                        flush_rt.spawn(async move {
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
            dir_cache: dir_cache_shared.clone(),
            path_to_inode: path_to_inode.clone(),
            truncated_inodes: truncated_inodes_shared.clone(),
            flush_runtime: flush_runtime.clone(),
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
            dir_cache: dir_cache_shared,
            statfs_cache: Arc::new(RwLock::new(None)),
            lock_manager: Arc::new(LockManager::new()),
            buffer_flush_threshold,
            write_open_counts,
            open_counts,
            size_high_water: Arc::new(DashMap::new()),
            truncated_inodes: truncated_inodes_shared,
            flush_in_flight: flush_in_flight_shared,
            flush_runtime,
            flush_handle,
            pending_deletes: Arc::new(dashmap::DashSet::new()),
            refreshing_inodes: Arc::new(dashmap::DashSet::new()),
            release_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            write_tasks_in_flight: Arc::new(DashMap::new()),
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
        let name_str = name.to_str()?;

        // Fast path: parent is in metadata_cache (normal case after lookup/readdir).
        let parent_path_opt = self.metadata_cache.get(&parent).map(|m| m.path.clone());

        // Fallback: scan path_to_inode for the parent inode. This covers the case
        // where the kernel holds a directory inode from before a client restart but
        // the metadata_cache is empty (in-memory only, lost on restart).
        let parent_path = parent_path_opt.or_else(|| {
            let map = self.path_to_inode.read().unwrap();
            map.iter().find(|(_, &v)| v == parent).map(|(k, _)| k.clone())
        })?;

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

        // Disable kernel readahead — our pipeline (depth=2) handles lookahead explicitly.
        // Kernel readahead would race our pipeline with extra concurrent fetches.
        let _ = config.set_max_readahead(0);

        // Warm metadata and directory caches from the leader on startup so that the
        // first ls/find/DVR index scan sees all files immediately without round-trips.
        {
            let client = self.client.clone();
            let metadata_cache = self.metadata_cache.clone();
            let path_to_inode = self.path_to_inode.clone();
            let next_inode = self.next_inode.clone();
            let last_metadata_update = self.last_metadata_update.clone();
            let dir_cache = self.dir_cache.clone();
            self.runtime.spawn(async move {
                info!("Startup: warming metadata cache from leader");
                let files = match client.list_all_files().await {
                    Ok(f) => f,
                    Err(e) => { warn!("Startup warm: {}", e); return; }
                };
                let now = std::time::Instant::now();
                let count = files.len();
                // Group into dir-cache entries as we go.
                let mut dir_entries: std::collections::HashMap<String, Vec<dfs_common::FileMetadata>> =
                    std::collections::HashMap::new();
                for file in files {
                    // Allocate inode
                    let ino = {
                        let path_map = path_to_inode.read().unwrap();
                        if let Some(&existing) = path_map.get(&file.path) {
                            existing
                        } else {
                            drop(path_map);
                            let mut path_map = path_to_inode.write().unwrap();
                            if let Some(&existing) = path_map.get(&file.path) {
                                existing
                            } else {
                                let mut next = next_inode.write().unwrap();
                                let v = *next;
                                *next += 1;
                                drop(next);
                                path_map.insert(file.path.clone(), v);
                                v
                            }
                        }
                    };
                    client.seed_write_seq(file.id, file.write_seq);
                    metadata_cache.insert(ino, file.clone());
                    last_metadata_update.insert(ino, now);

                    // Bucket into parent dir for dir_cache population.
                    if let Some(slash) = file.path.rfind('/') {
                        let parent = if slash == 0 { "/".to_string() } else { file.path[..slash].to_string() };
                        dir_entries.entry(parent).or_default().push(file);
                    }
                }
                // Populate dir_cache for every parent directory seen.
                for (dir, entries) in dir_entries {
                    dir_cache.insert(dir, (entries, now));
                }
                info!("Startup: warmed {} files into metadata/dir cache", count);
            });
        }

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
        let release_in_flight = self.release_in_flight.clone();

        let flush_handle = FlushHandle {
            client: client.clone(),
            write_buffers: write_buffers.clone(),
            metadata_cache: metadata_cache.clone(),
            flush_in_flight: flush_in_flight.clone(),
            last_metadata_update: self.last_metadata_update.clone(),
            dir_cache: self.dir_cache.clone(),
            path_to_inode: self.path_to_inode.clone(),
            truncated_inodes: self.truncated_inodes.clone(),
            flush_runtime: self.flush_runtime.clone(),
        };

        self.block_on(async move {
            // Step 0: Wait for any in-flight release() flush tasks to complete.
            // release() spawns async tasks that aren't tracked by flush_in_flight.
            // Without this wait, a release flush that started just before unmount
            // may be interrupted mid-write, losing the final metadata commit.
            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
            loop {
                if release_in_flight.load(std::sync::atomic::Ordering::Relaxed) == 0 { break; }
                if tokio::time::Instant::now() > deadline {
                    warn!("destroy: timed out waiting for release tasks");
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }

            // Step 1: Force-flush all dirty write buffers (catches buffers not yet
            // picked up by a release task, e.g. files open when unmount was called).
            let inodes: Vec<u64> = write_buffers.iter().map(|e| *e.key()).collect();
            if !inodes.is_empty() {
                info!("destroy: force-flushing {} open write buffers", inodes.len());
                let handles: Vec<_> = inodes.into_iter().map(|ino| {
                    let h = flush_handle.clone();
                    let flush_rt = h.flush_runtime.clone();
                    flush_rt.spawn(async move {
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

            // Step 3: Commit metadata for any inode that has chunk_locations in cache.
            // Catches files where flush_buffer_async ran but metadata_cache was missing
            // (race with create()'s async task) or where the buffer was flushed by the
            // background flusher but the metadata commit was skipped due to cache miss.
            {
                let to_commit: Vec<_> = metadata_cache.iter()
                    .filter(|e| !e.chunk_locations.is_empty())
                    .map(|e| e.clone())
                    .collect();
                if !to_commit.is_empty() {
                    info!("destroy: committing metadata for {} inodes with chunks", to_commit.len());
                    for meta in to_commit {
                        client.flush_metadata_sync(&meta).await;
                    }
                }
            }

            info!("destroy: all buffers flushed and metadata committed");
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

        // If a concurrent unlink is in flight for this path, report it as gone immediately
        // so creates/opens don't race with the still-pending server-side delete.
        if self.pending_deletes.contains(&path) {
            debug!("lookup: {} is pending delete, returning ENOENT", path);
            reply.error(libc::ENOENT);
            return;
        }
        debug!("lookup: pending_deletes ptr={:p} len={} checking {:?}", Arc::as_ptr(&self.pending_deletes), self.pending_deletes.len(), path);

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
            reply.entry(&Duration::ZERO, &attr, 0);
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
                    reply.entry(&Duration::ZERO, &attr, 0);
                }
                Ok(None) => {
                    // Either 304 not-modified (cache still valid) OR 404 not-found.
                    if cached_modified_at.is_some() {
                        // We sent a conditional GET — None means "not modified", use cache.
                        let path_map = path_to_inode.read().unwrap();
                        if let Some(&ino) = path_map.get(&path) {
                            if let Some(metadata) = metadata_cache.get(&ino) {
                                debug!("Using cached metadata for {} (not modified)", path);
                                let attr = DfsFilesystem::metadata_to_attr_static(ino, &*metadata);
                                reply.entry(&Duration::ZERO, &attr, 0);
                                return;
                            }
                        }
                    }
                    // No cached entry and server returned not-found. Do one unconditional
                    // retry — transient leader misses can cause false not-found on first lookup.
                    match client.get_file_metadata(&path).await {
                        Ok(Some(metadata)) => {
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
                            reply.entry(&Duration::ZERO, &attr, 0);
                        }
                        _ => reply.error(libc::ENOENT),
                    }
                }
                Err(e) => {
                    error!("Failed to lookup {}: {}", path, e);
                    reply.error(libc::EIO);
                }
            }
        });
    }

    fn open(&mut self, _req: &FuseRequest, ino: u64, flags: i32, reply: fuser::ReplyOpen) {
        info!("open: ino={} flags=0x{:x} release_in_flight={} write_tasks_in_flight={:?}",
              ino, flags,
              self.release_in_flight.load(std::sync::atomic::Ordering::Relaxed),
              self.write_tasks_in_flight.get(&ino).map(|c| c.load(std::sync::atomic::Ordering::Relaxed)));

        // NOTE: We intentionally do NOT block here waiting for write_tasks_in_flight or
        // release_in_flight. Both used block_on() on main-runtime workers, which deadlocks
        // when all workers are occupied (e.g. concurrent reads during active writes in T17).
        // The read engine refreshes chunk locations lazily on first read, so a read that
        // opens slightly before the flush commits will simply fetch fresh metadata from the
        // leader — correct behavior with no blocking required.

        // Track all opens for read-engine lifecycle management.
        *self.open_counts.entry(ino).or_insert(0) += 1;

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
                    // Clear any pending truncate flag — new write session starts clean.
                    // The flag was set by setattr(size=0) to block the old session's
                    // in-flight flush; now that open() has started a fresh session, allow
                    // this session's flushes through.
                    self.truncated_inodes.remove(&ino);
                }

                // Create or update the InodeWriteState for this inode.
                // If already exists (multiple writers), set sync_on_fsync if ANY fd requests it.
                let state_entry = self.write_buffers
                    .entry(ino)
                    .or_insert_with(|| Arc::new(Mutex::new(InodeWriteState::new(sync_on_fsync))));
                if sync_on_fsync {
                    if let Ok(mut st) = state_entry.try_lock() { st.sync_on_fsync = true; }
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

        // Pre-warm the read engine so the first read() hits the chunk map immediately.
        // For read-mode opens after waiting for in-flight flush tasks, always refresh
        // so we pick up the newly committed chunk locations (not the stale size=0 cache).
        if let Some(meta) = self.metadata_cache.get(&ino) {
            let file_id = meta.id;
            let file_size = meta.size;
            drop(meta);
            let client = self.client.clone();
            let engine = client.read_engines.get_or_create(ino);
            let is_read_open = (flags & libc::O_ACCMODE) == libc::O_RDONLY;
            let needs_refresh = is_read_open  // always refresh on read-open after flush drain
                || engine.known_size.load(std::sync::atomic::Ordering::Relaxed) == 0;
            if needs_refresh && engine.refresh_in_progress
                .compare_exchange(false, true,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Relaxed).is_ok()
            {
                self.runtime.spawn(async move {
                    client.refresh_engine_flagged(&engine, file_id, file_size).await;
                });
            }
        }
        info!("open: ino={} DONE — reply sent", ino);
    }

    fn getattr(&mut self, _req: &FuseRequest, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        debug!("getattr: ino={}", ino);

        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let write_buffers = self.write_buffers.clone();
        let write_buffer_enabled = self.write_buffer_enabled;
        let size_high_water = self.size_high_water.clone();
        let write_open_counts = self.write_open_counts.clone();
        let refreshing_inodes = self.refreshing_inodes.clone();
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

                    // Gate: only one background refresh per inode at a time.
                    // Without this, a slow node causes unbounded task accumulation that
                    // exhausts the connection pool and freezes the entire tokio runtime.
                    if should_refresh && refreshing_inodes.insert(ino) {
                        let client_bg = client.clone();
                        let metadata_cache_bg = metadata_cache.clone();
                        let last_metadata_update_bg = last_metadata_update.clone();
                        let refreshing_inodes_bg = refreshing_inodes.clone();
                        let path_bg = metadata.path.clone();
                        let current_modified_at = metadata.modified_at;
                        let current_size = metadata.size;
                        let current_chunks = metadata.chunk_locations.len();
                        tokio::spawn(async move {
                            let result = tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                client_bg.get_file_metadata(&path_bg),
                            ).await;
                            if let Ok(Ok(Some(fresh))) = result {
                                let server_is_newer = fresh.modified_at > current_modified_at
                                    || (fresh.modified_at == current_modified_at
                                        && (fresh.size > current_size
                                            || fresh.chunk_locations.len() > current_chunks));
                                if server_is_newer {
                                    client_bg.seed_write_seq(fresh.id, fresh.write_seq);
                                    metadata_cache_bg.insert(ino, fresh);
                                }
                            }
                            last_metadata_update_bg.insert(ino, std::time::Instant::now());
                            refreshing_inodes_bg.remove(&ino);
                        });
                    }

                    // For files with an active write buffer, the true EOF is further ahead
                    // than the last committed flush position in metadata.size. Report the
                    // buffer's logical end so that Kodi/players seeking in a live recording
                    // see the correct (current) file size instead of a stale flushed size.
                    // WITHOUT this, a seek to the "end" of a live file may land beyond what
                    // getattr reports, causing the player to stall waiting for the file to grow.
                    //
                    // BUT: only inflate the size if a writer currently has the file open.
                    // If only readers are open, the buffered bytes aren't accessible to them
                    // and reporting an inflated size causes seeks into the unflushed gap, which
                    // returns short/zero data and makes players jump back to the start.
                    let has_active_writer = write_open_counts.get(&ino).map(|c| *c > 0).unwrap_or(false);
                    if write_buffer_enabled && has_active_writer {
                        // Use try_lock() — never block a main-runtime thread on a flush-runtime mutex.
                        // If locked, fall back to the high-water mark from a previous successful read.
                        let buffered_end = if let Some(state_lock) = write_buffers.get(&ino) {
                            if let Ok(state) = state_lock.try_lock() {
                                state.slots.iter()
                                    .map(|(idx, slot)| idx * CHUNK_SIZE as u64 + slot.data.len() as u64)
                                    .max()
                                    .unwrap_or(0)
                            } else { 0 } // flush in progress; hwm covers us
                        } else { 0 };

                        // High-water mark: never report a size smaller than previously seen.
                        let hwm = size_high_water.get(&ino).map(|v| *v).unwrap_or(0);
                        let reported = metadata.size.max(buffered_end).max(hwm);
                        if reported > metadata.size {
                            metadata.size = reported;
                        }
                        if reported > hwm {
                            size_high_water.insert(ino, reported);
                        }
                    } else if write_buffer_enabled {
                        // No active writer, but the release flush task may still be running.
                        let buffered_end = if let Some(state_lock) = write_buffers.get(&ino) {
                            if let Ok(state) = state_lock.try_lock() {
                                state.slots.iter()
                                    .map(|(idx, slot)| idx * CHUNK_SIZE as u64 + slot.data.len() as u64)
                                    .max()
                                    .unwrap_or(0)
                            } else { 0 }
                        } else { 0 };
                        let hwm = size_high_water.get(&ino).map(|v| *v).unwrap_or(0);
                        let reported = metadata.size.max(buffered_end).max(hwm);
                        if reported > metadata.size {
                            metadata.size = reported;
                        }
                    }
                }

                let attr = DfsFilesystem::metadata_to_attr_static(ino, &metadata);
                // TTL=0 forces the kernel to re-validate on every stat — critical for
                // files that just closed (O_APPEND openers need the current size) and
                // for renamed files (old name must not be served from stale dentry).
                // Active write buffers also use TTL=0 so DVR players see the growing size.
                // Static files (no recent write, no buffer) use 5s to match server refresh.
                let recently_written = last_metadata_update.get(&ino)
                    .map(|t| t.elapsed() < Duration::from_secs(2))
                    .unwrap_or(false);
                let ttl = if write_buffer_enabled && (write_buffers.contains_key(&ino) || recently_written) {
                    Duration::ZERO
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
        let last_metadata_update = self.last_metadata_update.clone();
        let size_high_water = self.size_high_water.clone();

        self.runtime.spawn(async move {
            let start = std::time::Instant::now();
            debug!("FUSE read: ino={}, offset={}, size={}", ino, offset, size);

            // --- Metadata: read only scalars we need, release DashMap ref fast. ---
            let (file_size, file_type, file_path, file_id) = match metadata_cache.get(&ino) {
                Some(m) => (m.size, m.file_type, m.path.clone(), m.id),
                None => { reply.error(libc::ENOENT); return; }
            };

            if file_type != FileType::RegularFile {
                reply.error(libc::EISDIR);
                return;
            }

            let offset = offset as usize;
            let size = size as usize;

            // Include buffered-but-not-yet-flushed bytes in the effective file size.
            // Use try_lock() — never block a main-runtime thread waiting for the flush
            // runtime to release the mutex. If the buffer is locked, fall through to the
            // server-committed size; the reader will fetch from the network instead.
            let file_size = if write_buffer_enabled {
                if let Some(state_lock) = write_buffers.get(&ino) {
                    if let Ok(state) = state_lock.try_lock() {
                        let buffered_end = state.slots.iter()
                            .map(|(idx, slot)| idx * CHUNK_SIZE as u64 + slot.data.len() as u64)
                            .max()
                            .unwrap_or(0);
                        file_size.max(buffered_end)
                    } else {
                        file_size // flush in progress; use server-committed size
                    }
                } else {
                    file_size
                }
            } else {
                file_size
            };

            // --- Write buffer: serve from dirty slots without any network I/O. ---
            // Use try_lock() to avoid blocking main runtime threads on flush-runtime mutex.
            if write_buffer_enabled {
                if let Some(state_lock) = write_buffers.get(&ino) {
                    if let Ok(state) = state_lock.try_lock() {
                        let read_end = offset + size;
                        let mut buf_data: Vec<u8> = Vec::with_capacity(size);
                        let mut pos = offset;

                        while pos < read_end {
                            let chunk_idx = InodeWriteState::chunk_index(pos as u64);
                            let intra = InodeWriteState::intra_offset(pos as u64);
                            let need = (read_end - pos).min(CHUNK_SIZE - intra);

                            if let Some(slot) = state.slots.get(&chunk_idx) {
                                // If the requested range falls within bytes already committed to
                                // the server (server_prefix) or within a synthetic gap-fill prefix,
                                // fall through to the network so the server's real data is returned.
                                let bypass_prefix = slot.server_prefix.max(slot.gap_filled_prefix);
                                if intra + need <= bypass_prefix {
                                    break;
                                }
                                if intra + need <= slot.data.len() {
                                    buf_data.extend_from_slice(&slot.data[intra..intra + need]);
                                    pos += need;
                                } else if intra < slot.data.len() {
                                    let avail = slot.data.len() - intra;
                                    buf_data.extend_from_slice(&slot.data[intra..intra + avail]);
                                    pos += avail;
                                    break;
                                } else {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }

                        if !buf_data.is_empty() {
                            debug!("FUSE read from write buffer: ino={}, {} bytes", ino, buf_data.len());
                            reply.data(&buf_data);
                            return;
                        }
                    }
                    // If try_lock failed (flush in progress), fall through to network read.
                }
            }

            // --- EOF check: refresh metadata if file may have grown. ---
            let effective_size = if offset >= file_size as usize {
                // If the file is actively being written, use the size_high_water mark before
                // hitting the leader. This avoids a network round-trip on every read near the
                // write head — the DVR player reads are much slower than they need to be
                // because getattr advances the HWM but the read path ignores it.
                let hwm = size_high_water.get(&ino).map(|v| *v).unwrap_or(0);
                if write_buffer_enabled && write_buffers.contains_key(&ino) && offset < hwm as usize {
                    hwm
                } else {
                let should_refresh = match last_metadata_update.get(&ino) {
                    None => true,
                    Some(last) => last.elapsed() >= std::time::Duration::from_secs(1),
                };
                if !should_refresh {
                    reply.data(&[]);
                    return;
                }
                last_metadata_update.insert(ino, std::time::Instant::now());

                match client.get_file_metadata(&file_path).await {
                    Ok(Some(fresh)) => {
                        if offset >= fresh.size as usize {
                            client.seed_write_seq(fresh.id, fresh.write_seq);
                            // Invalidate the read engine so next read picks up new chunk map.
                            client.invalidate_read_engine(ino);
                            metadata_cache.insert(ino, fresh);
                            reply.data(&[]);
                            return;
                        }
                        let new_size = fresh.size;
                        client.seed_write_seq(fresh.id, fresh.write_seq);
                        client.invalidate_read_engine(ino);
                        metadata_cache.insert(ino, fresh);
                        info!("File grew to {} bytes (ino={})", new_size, ino);
                        new_size
                    }
                    Ok(None) => { reply.error(libc::ENOENT); return; }
                    Err(_)   => { reply.data(&[]); return; }
                }
                } // end hwm-miss else
            } else {
                file_size
            };

            // --- Delegate to the read engine (chunk map + pipeline + cache). ---
            info!("FUSE read: ino={}, offset={}, size={}, file_size={}", ino, offset, size, effective_size);
            let result = client.read_file(
                ino, effective_size, file_id, &file_path, offset, size,
            ).await;

            let elapsed = start.elapsed();
            match result {
                Ok(data) => {
                    // FUSE rejects replies larger than the requested size with EINVAL.
                    let reply_data = if data.len() > size { &data[..size] } else { &data[..] };
                    info!("FUSE read done: ino={}, {} bytes in {:?}", ino, reply_data.len(), elapsed);
                    reply.data(reply_data);
                }
                Err(e) => {
                    tracing::error!("FUSE read error: ino={}, offset={}: {}", ino, offset, e);
                    reply.error(libc::EIO);
                }
            }
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
            // instant cache hits.  Each subdir gets its own independent task so the
            // readdir reply is not delayed waiting for network I/O.
            for entry in entries.iter().filter(|e| e.file_type == FileType::Directory) {
                let subdir = entry.path.clone();
                // Skip if already cached and fresh
                if let Some(cached) = dir_cache.get(&subdir) {
                    if cached.1.elapsed() < std::time::Duration::from_secs(5) {
                        continue;
                    }
                }
                let client = client.clone();
                let dir_cache = dir_cache.clone();
                let metadata_cache = metadata_cache.clone();
                let path_to_inode = path_to_inode.clone();
                let next_inode = next_inode.clone();
                let last_metadata_update = last_metadata_update.clone();
                tokio::spawn(async move {
                    let fetch_start = std::time::Instant::now();
                    if let Ok(sub_entries) = client.list_directory(&subdir).await {
                        // Only cache if the directory hasn't been invalidated while fetching.
                        let still_valid = match dir_cache.get(&subdir) {
                            Some(entry) => entry.1 < fetch_start,
                            None => true,
                        };
                        if still_valid {
                            dir_cache.insert(subdir.clone(), (sub_entries.clone(), std::time::Instant::now()));
                        }
                        let now = std::time::Instant::now();
                        for entry in &sub_entries {
                            let ino_val = {
                                let path_map = path_to_inode.read().unwrap();
                                if let Some(&existing) = path_map.get(&entry.path) {
                                    existing
                                } else {
                                    drop(path_map);
                                    let mut path_map = path_to_inode.write().unwrap();
                                    if let Some(&existing) = path_map.get(&entry.path) {
                                        existing
                                    } else {
                                        let mut next = next_inode.write().unwrap();
                                        let v = *next;
                                        *next += 1;
                                        drop(next);
                                        path_map.insert(entry.path.clone(), v);
                                        v
                                    }
                                }
                            };
                            client.seed_write_seq(entry.id, entry.write_seq);
                            metadata_cache.insert(ino_val, entry.clone());
                            last_metadata_update.insert(ino_val, now);
                        }
                        debug!("Prefetched {} entries for {}", sub_entries.len(), subdir);
                    }
                });
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
        debug!("write FUSE: ino={} offset={} len={}", ino, offset, data.len());

        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let write_counters = self.write_counters.clone();
        let write_buffers = self.write_buffers.clone();
        let write_buffer_enabled = self.write_buffer_enabled;
        let buffer_flush_threshold = self.buffer_flush_threshold;
        let last_metadata_update = self.last_metadata_update.clone();
        let path_to_inode = self.path_to_inode.clone();
        let flush_handle = self.flush_handle.clone();
        let size_high_water = self.size_high_water.clone();
        let data_vec = data.to_vec();
        let req_uid = _req.uid();
        let req_gid = _req.gid();

        let write_task_counter = self.write_tasks_in_flight
            .entry(ino)
            .or_insert_with(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
            .clone();
        write_task_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        self.runtime.spawn(async move {
            let _write_guard = WriteTaskGuard(write_task_counter);
            let start = std::time::Instant::now();
            debug!("write: ino={}, offset={}, size={}", ino, offset, data_vec.len());

            let mut metadata = match metadata_cache.get(&ino) {
                Some(m) => m.clone(),
                None => {
                    // Metadata cache miss — fetch from server and populate.
                    let path_opt = {
                        let map = path_to_inode.read().unwrap();
                        map.iter().find(|(_, &v)| v == ino).map(|(k, _)| k.clone())
                    };
                    if let Some(path) = path_opt {
                        match client.get_file_metadata(&path).await {
                            Ok(Some(fetched)) => {
                                client.seed_write_seq(fetched.id, fetched.write_seq);
                                metadata_cache.insert(ino, fetched.clone());
                                last_metadata_update.insert(ino, std::time::Instant::now());
                                fetched
                            }
                            Ok(None) => {
                                let new_meta = dfs_common::FileMetadata {
                                    id: dfs_common::FileId::new(),
                                    path: path.clone(),
                                    size: 0,
                                    chunk_locations: Vec::new(),
                                    created_at: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default().as_secs(),
                                    modified_at: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default().as_secs(),
                                    mode: 0o644,
                                    uid: req_uid,
                                    gid: req_gid,
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

            let is_sqlite = is_sqlite_path(&metadata.path);
            let cache_inode = if is_sqlite { 0 } else { ino };

            if write_buffer_enabled && !is_sqlite {
                let offset_usize = offset as usize;
                let current_size = {
                    let cache_size = metadata_cache.get(&ino)
                        .map(|m| m.size as usize)
                        .unwrap_or(metadata.size as usize);

                    if let Some(state_lock) = write_buffers.get(&ino) {
                        if let Ok(state) = state_lock.try_lock() {
                            let buffered_end = state.slots.iter()
                                .map(|(idx, slot)| (idx * CHUNK_SIZE as u64 + slot.data.len() as u64) as usize)
                                .max()
                                .unwrap_or(0);
                            buffered_end.max(cache_size)
                        } else {
                            // Flush in progress — use the high-water mark so that an incoming
                            // write just past the currently-flushing boundary isn't misclassified
                            // as a sparse write (which would create a tiny chunk on the server).
                            let hwm = size_high_water.get(&ino).map(|v| *v as usize).unwrap_or(0);
                            hwm.max(cache_size)
                        }
                    } else {
                        cache_size
                    }
                };

                debug!("Buffered write check: offset={}, current_size={}, cache_size={}, buffer_present={}",
                       offset_usize, current_size,
                       metadata_cache.get(&ino).map(|m| m.size).unwrap_or(0),
                       write_buffers.contains_key(&ino));

                let is_overwrite = offset_usize < current_size;
                let is_sparse_write = offset_usize > current_size;


                if is_sparse_write {
                    let gap = offset_usize - current_size;

                    const SMALL_GAP_THRESHOLD: usize = 64 * 1024;
                    if gap < SMALL_GAP_THRESHOLD {
                        info!("Near-sequential write: offset={} current_size={} gap={} bytes — zero-filling into buffer",
                              offset_usize, current_size, gap);
                        let mut padded = vec![0u8; gap];
                        padded.extend_from_slice(&data_vec);
                        let gap_write_offset = current_size as u64;
                        let padded_len = padded.len();

                        let state_arc = write_buffers
                            .entry(ino)
                            .or_insert_with(|| Arc::new(Mutex::new(InodeWriteState::new(false))))
                            .clone();
                        let mut state = state_arc.lock().await;
                        state.write_at(gap_write_offset, &padded);
                        // Mark the gap bytes as synthetic so flush doesn't mistake them
                        // for real app data when deciding whether to PatchChunk.
                        let gap_chunk_idx = InodeWriteState::chunk_index(gap_write_offset);
                        let gap_intra = InodeWriteState::intra_offset(gap_write_offset);
                        if let Some(slot) = state.slots.get_mut(&gap_chunk_idx) {
                            slot.gap_filled_prefix = gap_intra + gap;
                        }
                        drop(state);

                        {
                            let mut counters = write_counters.write().unwrap();
                            *counters.entry(ino).or_insert(0) += 1;
                        }
                        reply.written(data_vec.len() as u32);
                        return;
                    }

                    // TRUE SPARSE WRITE: large gap.
                    // If the target offset falls within an already-committed chunk (e.g. a DVR
                    // metadata update jumping back into the video stream), use PatchChunk so we
                    // update the existing chunk in-place rather than creating a new tiny chunk
                    // that corrupts the chunk map's contiguous layout.
                    info!("Sparse write: offset {} > current_size {} (gap: {} bytes)",
                           offset_usize, current_size, gap);

                    let target_chunk_idx = InodeWriteState::chunk_index(offset as u64);
                    let target_intra = InodeWriteState::intra_offset(offset as u64);
                    let meta_snap = metadata_cache.get(&ino).map(|m| m.clone());
                    let existing_loc = meta_snap.as_ref()
                        .and_then(|m| m.chunk_locations.get(target_chunk_idx as usize).cloned());

                    if let Some(old_loc) = existing_loc {
                        // Target offset is within an existing chunk — patch it in-place.
                        info!("Sparse write at offset={} lands in existing chunk {} (size={}) — using PatchChunk",
                              offset_usize, target_chunk_idx, old_loc.size);
                        match client.patch_chunk_on_replicas(
                            old_loc.chunk_id, offset as u64, target_intra, data_vec.clone(), &old_loc,
                        ).await {
                            Ok(new_loc) => {
                                let new_size = (offset_usize + data_vec.len()).max(current_size);
                                let mut meta = meta_snap.unwrap();
                                if let Some(loc) = meta.chunk_locations.get_mut(target_chunk_idx as usize) {
                                    *loc = new_loc;
                                }
                                meta.size = new_size as u64;
                                meta.modified_at = SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                                client.enqueue_metadata(&meta).await;
                                metadata_cache.insert(ino, meta);
                                info!("Sparse write complete (patch): ino={}, offset={}, len={}, new_size={}",
                                      ino, offset, data_vec.len(), new_size);
                                reply.written(data_vec.len() as u32);
                            }
                            Err(e) => {
                                error!("Sparse write PatchChunk failed for inode {}: {}", ino, e);
                                reply.error(libc::EIO);
                            }
                        }
                    } else {
                        // Target is beyond all committed chunks — new chunk, write directly.
                        match client.write_data_with_cache(&data_vec, ino, offset as u64).await {
                            Ok((_, _, chunk_locations_opt)) => {
                                let new_size = (offset_usize + data_vec.len()).max(current_size);
                                let mut metadata = meta_snap.unwrap_or_else(|| metadata.clone());
                                if let Some(chunk_locations) = chunk_locations_opt {
                                    metadata.chunk_locations.extend(chunk_locations);
                                }
                                metadata.size = new_size as u64;
                                metadata.modified_at = SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                                client.enqueue_metadata(&metadata).await;
                                metadata_cache.insert(ino, metadata);
                                info!("Sparse write complete: ino={}, offset={}, len={}, new_size={}",
                                      ino, offset, data_vec.len(), new_size);
                                reply.written(data_vec.len() as u32);
                            }
                            Err(e) => {
                                error!("Sparse write failed for inode {}: {}", ino, e);
                                reply.error(libc::EIO);
                            }
                        }
                    }
                    return;
                }

                // BUFFERED WRITE — write into the slot and return immediately.
                // No back-pressure: the background flusher drains full slots continuously
                // and is always schedulable (write tasks never block waiting for it).
                // Memory is bounded in practice: the flusher runs every 100ms and each
                // 4MB slot takes ~50ms to write to two replicas, so at most ~2 batches
                // accumulate before the flusher catches up.
                {
                    let write_offset = offset as u64;

                    let state_arc = write_buffers
                        .entry(ino)
                        .or_insert_with(|| Arc::new(Mutex::new(InodeWriteState::new(false))))
                        .clone();
                    let mut state = state_arc.lock().await;
                    state.write_at(write_offset, &data_vec);
                    drop(state);

                    // Update size_high_water immediately so concurrent writes that hit
                    // try_lock() failure see the correct write position and don't misclassify
                    // this buffered range as a sparse gap.
                    let new_end = (offset as u64) + data_vec.len() as u64;
                    {
                        let mut hwm = size_high_water.entry(ino).or_insert(0);
                        if new_end > *hwm { *hwm = new_end; }
                    }

                    {
                        let mut counters = write_counters.write().unwrap();
                        *counters.entry(ino).or_insert(0) += 1;
                    }
                    let total_elapsed = start.elapsed();
                    debug!("write REPLY: ino={} offset={} len={} took={:?}",
                           ino, offset, data_vec.len(), total_elapsed);
                    reply.written(data_vec.len() as u32);
                    return;
                }
            }

            // Non-buffered write path (SQLite or write_buffer_enabled=false).
            let offset = offset as usize;
            let current_size = if write_buffer_enabled {
                let cache_size = metadata_cache.get(&ino)
                    .map(|m| m.size as usize).unwrap_or(metadata.size as usize);
                let buffered_end = if let Some(state_lock) = write_buffers.get(&ino) {
                    if let Ok(state) = state_lock.try_lock() {
                        state.slots.iter()
                            .map(|(idx, slot)| (idx * CHUNK_SIZE as u64 + slot.data.len() as u64) as usize)
                            .max()
                            .unwrap_or(0)
                    } else { 0 }
                } else { 0 };
                buffered_end.max(cache_size)
            } else {
                metadata.size as usize
            };

            let mut affected_chunk_range: Option<(usize, usize)> = None;

            let (new_data, is_append) = if offset == current_size {
                (data_vec.clone(), true)
            } else if offset > current_size {
                let mut padded = vec![0u8; offset - current_size];
                padded.extend_from_slice(&data_vec);
                (padded, true)
            } else {
                info!("Random write detected: offset={}, size={}, file_size={}",
                      offset, data_vec.len(), current_size);

                let write_end = offset + data_vec.len();

                if metadata.chunk_locations.is_empty() {
                    (data_vec.clone(), false)
                } else {
                    let mut chunk_start_offset = 0u64;
                    let mut first_affected_chunk: Option<usize> = None;
                    let mut last_affected_chunk: Option<usize> = None;

                    for (idx, loc) in metadata.chunk_locations.iter().enumerate() {
                        let chunk_size = loc.size as u64;
                        let chunk_end_offset = chunk_start_offset + chunk_size;
                        if chunk_end_offset > offset as u64 && chunk_start_offset < write_end as u64 {
                            if first_affected_chunk.is_none() { first_affected_chunk = Some(idx); }
                            last_affected_chunk = Some(idx);
                        }
                        chunk_start_offset = chunk_end_offset;
                    }

                    if first_affected_chunk.is_none() || last_affected_chunk.is_none() {
                        info!("Write beyond EOF, treating as append");
                        (data_vec.clone(), true)
                    } else {
                        let first_idx = first_affected_chunk.unwrap();
                        let last_idx = last_affected_chunk.unwrap();

                        info!("Random write affects chunks {}-{} (out of {} total)",
                              first_idx, last_idx, metadata.chunk_locations.len());
                        affected_chunk_range = Some((first_idx, last_idx));

                        let affected_locs = &metadata.chunk_locations[first_idx..=last_idx];
                        let first_chunk_file_offset: u64 = metadata.chunk_locations[..first_idx]
                            .iter().map(|l| l.size as u64).sum();
                        let all_chunk_ids: Vec<_> = metadata.chunk_locations.iter().map(|l| l.chunk_id).collect();
                        let mut read_hints = Vec::with_capacity(affected_locs.len());
                        let mut current_offset = first_chunk_file_offset;
                        for (i, loc) in affected_locs.iter().enumerate() {
                            let chunk_size = loc.size as usize;
                            read_hints.push(crate::client::ChunkReadHint {
                                chunk_idx: first_idx + i,
                                chunk_id: loc.chunk_id,
                                full_chunk: true,
                                offset_in_chunk: 0,
                                length: chunk_size,
                                file_offset: current_offset,
                            });
                            current_offset += chunk_size as u64;
                        }

                        let affected_data = match client.read_data(&read_hints, &all_chunk_ids, cache_inode, &metadata.chunk_locations).await {
                            Ok(data) => data,
                            Err(e) => {
                                error!("Failed to read affected chunks {}-{}: {}", first_idx, last_idx, e);
                                reply.error(libc::EIO);
                                return;
                            }
                        };

                        let write_offset_in_range = (offset as u64 - first_chunk_file_offset) as usize;
                        let affected_data_len = affected_data.len();
                        let mut merged = affected_data;
                        if write_offset_in_range + data_vec.len() > merged.len() {
                            merged.resize(write_offset_in_range + data_vec.len(), 0);
                        }
                        merged[write_offset_in_range..write_offset_in_range + data_vec.len()]
                            .copy_from_slice(&data_vec);
                        info!("Random write: read {} bytes from {} chunks, merged to {} bytes",
                              affected_data_len, affected_locs.len(), merged.len());
                        (merged, false)
                    }
                }
            };

            let write_start = std::time::Instant::now();
            let result = if is_append {
                client.write_data_with_cache(&new_data, cache_inode, current_size as u64).await
            } else {
                let write_file_offset = if let Some((first_idx, _)) = affected_chunk_range {
                    metadata.chunk_locations[..first_idx].iter().map(|l| l.size as u64).sum::<u64>()
                } else {
                    0
                };
                client.write_data_with_cache(&new_data, cache_inode, write_file_offset).await
            };
            debug!("write_data took {:?}", write_start.elapsed());

            match result {
                Ok((_, _, chunk_locations_opt)) => {
                    if is_append {
                        if let Some(chunk_locations) = chunk_locations_opt {
                            metadata.chunk_locations.extend(chunk_locations);
                        }
                        metadata.size = current_size as u64 + new_data.len() as u64;
                    } else if let Some((first_idx, last_idx)) = affected_chunk_range {
                        let new_locations = chunk_locations_opt.unwrap_or_default();
                        info!("Splicing {} new chunks into range {}-{} (was {} chunks)",
                              new_locations.len(), first_idx, last_idx, last_idx - first_idx + 1);
                        let mut updated_locations = Vec::new();
                        updated_locations.extend_from_slice(&metadata.chunk_locations[..first_idx]);
                        updated_locations.extend(new_locations);
                        if last_idx + 1 < metadata.chunk_locations.len() {
                            updated_locations.extend_from_slice(&metadata.chunk_locations[last_idx + 1..]);
                        }
                        metadata.chunk_locations = updated_locations;
                        metadata.size = metadata.chunk_locations.iter().map(|l| l.size as u64).sum();
                        info!("After splice: {} total chunks, {} total bytes",
                              metadata.chunk_locations.len(), metadata.size);
                    } else {
                        warn!("Full file rewrite with {} bytes", new_data.len());
                        metadata.chunk_locations = chunk_locations_opt.unwrap_or_default();
                        metadata.size = new_data.len() as u64;
                    }
                    metadata.modified_at = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs();

                    client.enqueue_metadata(&metadata).await;
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
        });
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
                            debug!("flush: enqueueing pending metadata for ino={}", ino);
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
        let is_write_flag = (flags & libc::O_ACCMODE) != libc::O_RDONLY;
        info!("release: ino={}, pid={}, is_write={}", ino, pid, is_write_flag);

        // Decrement total open count; drop read engine when last fd closes.
        let is_last_open = {
            let mut remove = false;
            let mut last = false;
            if let Some(mut count) = self.open_counts.get_mut(&ino) {
                if *count > 0 { *count -= 1; }
                if *count == 0 { remove = true; last = true; }
            }
            if remove { self.open_counts.remove(&ino); }
            last
        };
        if is_last_open {
            self.client.read_engines.engines.remove(&ino);
        }

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
                // size_high_water is cleared after the flush completes (in the release task)
                // so that getattr during the flush window still reports the correct size.
            }
            last
        } else {
            false
        };

        let lock_manager = self.lock_manager.clone();

        // All release paths spawn async tasks and return immediately.
        // Never use block_on here — it monopolizes the tokio scheduler and
        // stalls all other FUSE operations (reads, writes, lookups) for the
        // entire duration of the flush.

        if self.write_buffer_enabled {
            if is_last_writer {
                info!("release: ino={} last writer — flushing buffer", ino);
                let flush_handle = self.flush_handle.clone();
                let flush_rt = flush_handle.flush_runtime.clone();
                let write_buffers = self.write_buffers.clone();
                let release_in_flight = self.release_in_flight.clone();
                let write_tasks_in_flight = self.write_tasks_in_flight.clone();
                let pending_deletes_for_release = self.pending_deletes.clone();
                let path_to_inode_for_release = self.path_to_inode.clone();
                let size_high_water_for_release = self.size_high_water.clone();
                release_in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Reply to FUSE immediately — release() errors are informational only
                // and the kernel ignores them. Parking a main-runtime worker for the
                // full flush duration (up to 10s for metadata delivery) starves all
                // other FUSE ops (readdir, getattr, read) during DVR startup scans.
                reply.ok();
                flush_rt.spawn(async move {
                    // Wait for any concurrent write() tasks for this inode to finish writing
                    // into the slot before we flush. Without this, a close() that arrives
                    // while write() tasks are still queued flushes an incomplete slot.
                    if let Some(counter) = write_tasks_in_flight.get(&ino) {
                        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
                        while counter.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                            if tokio::time::Instant::now() > deadline {
                                warn!("release: timed out waiting for write tasks for ino={}", ino);
                                break;
                            }
                            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                        }
                    }
                    // If the file was unlinked while this release task was queued, skip the
                    // flush — sending PutFileMetadata for a deleted file resurrects it on
                    // the server.
                    let release_path = path_to_inode_for_release.read().unwrap()
                        .iter().find(|(_, &v)| v == ino).map(|(k, _)| k.clone());
                    let is_pending_delete = release_path.as_deref()
                        .map_or(false, |p| pending_deletes_for_release.contains(p));
                    let path_gone = release_path.is_none();
                    if is_pending_delete || path_gone {
                        debug!("release: ino={} path={:?} deleted (pending={} path_gone={}) — skipping flush",
                               ino, release_path, is_pending_delete, path_gone);
                        write_buffers.remove(&ino);
                        release_in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        return;
                    }
                    if let Err(e) = flush_handle.flush_buffer_async(ino, true).await {
                        error!("release: flush failed for inode {}: {}", ino, e);
                    }
                    write_buffers.remove(&ino);
                    size_high_water_for_release.remove(&ino);
                    if let Some(owner) = lock_owner {
                        if let Err(e) = lock_manager.release_all(ino, owner).await {
                            error!("release: lock release failed for inode {}: {}", ino, e);
                        }
                    }
                    release_in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                });
            } else if is_write {
                // Intermediate write close: release locks only, leave buffer intact.
                self.runtime.spawn(async move {
                    if let Some(owner) = lock_owner {
                        if let Err(e) = lock_manager.release_all(ino, owner).await {
                            error!("release: lock release failed for inode {}: {}", ino, e);
                        }
                    }
                    reply.ok();
                });
            } else {
                // Read-only close: only flush if there are no active writers still holding
                // the file open. If a writer is still recording, do NOT touch its buffer —
                // flushing a partial slot here creates a tiny chunk mid-recording (the
                // "rename-based save" fallback only applies when there are no writers).
                let has_writers = self.write_open_counts.get(&ino).map(|c| *c > 0).unwrap_or(false);
                let has_buffer = !has_writers && self.write_buffers.get(&ino)
                    .map(|s| s.try_lock().map(|s| !s.slots.is_empty()).unwrap_or(false))
                    .unwrap_or(false);
                let flush_handle = self.flush_handle.clone();
                let flush_rt = flush_handle.flush_runtime.clone();
                let write_buffers = self.write_buffers.clone();
                reply.ok();
                flush_rt.spawn(async move {
                    if has_buffer {
                        debug!("release: read-only close for ino={} has buffered data — flushing", ino);
                        if let Err(e) = flush_handle.flush_buffer_async(ino, true).await {
                            error!("release: flush failed for inode {}: {}", ino, e);
                        }
                        write_buffers.remove(&ino);
                    }
                    if let Some(owner) = lock_owner {
                        if let Err(e) = lock_manager.release_all(ino, owner).await {
                            error!("release: lock release failed for inode {}: {}", ino, e);
                        }
                    }
                });
            }
        } else {
            // No write buffer: enqueue metadata async and release locks.
            let client = self.client.clone();
            let metadata_cache = self.metadata_cache.clone();
            let write_counters = self.write_counters.clone();
            self.runtime.spawn(async move {
                let metadata_opt = metadata_cache.get(&ino).map(|m| m.clone());
                if let Some(metadata) = metadata_opt {
                    debug!("release: enqueueing metadata for ino={} ({} chunks)", ino, metadata.chunk_locations.len());
                    client.enqueue_metadata(&metadata).await;
                    write_counters.write().unwrap().insert(ino, 0);
                }
                if let Some(owner) = lock_owner {
                    if let Err(e) = lock_manager.release_all(ino, owner).await {
                        error!("release: lock release failed for inode {}: {}", ino, e);
                    }
                }
                reply.ok();
            });
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
                    reply.entry(&Duration::ZERO, &attr, 0);
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
        let pending_deletes = self.pending_deletes.clone();

        // Mark as pending-delete immediately so concurrent lookup() returns ENOENT
        // even while the server-side delete is still in flight.
        pending_deletes.insert(path.clone());
        debug!("unlink: inserted {:?} into pending_deletes (len={}, ptr={:p})", path, pending_deletes.len(), Arc::as_ptr(&pending_deletes));

        self.runtime.spawn(async move {
            // Always clean local cache first — stale entries block future creates even
            // when the server delete times out. The server/healer will clean up its side.
            let ino_opt = path_to_inode.read().unwrap().get(&path).copied();
            let file_id_opt = ino_opt.and_then(|ino| {
                metadata_cache.remove(&ino).map(|(_, m)| m.id)
            });
            if let Some(ino) = ino_opt {
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

            // Cancel any queued metadata flush for this file before sending the delete.
            // This prevents the queue worker from resurrecting the file after deletion.
            if let Some(file_id) = file_id_opt {
                client.cancel_metadata(file_id).await;
            }

            match client.delete_file(&path).await {
                Ok(_) => {
                    pending_deletes.remove(&path);
                    reply.ok();
                }
                Err(e) => {
                    pending_deletes.remove(&path);
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

        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let flush_handle = self.flush_handle.clone();
        let write_buffers = self.write_buffers.clone();
        let release_in_flight = self.release_in_flight.clone();
        // Look up old inode from local cache — the file may have been written
        // but not yet persisted to the server (write buffer still in flight).
        let old_ino = path_to_inode.read().unwrap().get(&old_path).copied();
        let old_ino = match old_ino {
            Some(i) => i,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Look up destination inode if it already exists (overwrite case).
        let new_ino = path_to_inode.read().unwrap().get(&new_path).copied();

        self.runtime.spawn(async move {
            // Wait for any in-flight release flush to complete. A release task commits
            // put_file_metadata to the server; if rename fires before that delivery,
            // the server returns NotFound for old_path and the rename fails.
            {
                let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
                while release_in_flight.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                    if tokio::time::Instant::now() > deadline {
                        warn!("rename: timed out waiting for release flush for ino={}", old_ino);
                        break;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
                }
            }

            // Flush the source file's write buffer to the server before renaming,
            // so the server has the current metadata for old_path.
            if write_buffers.contains_key(&old_ino) {
                if let Err(e) = flush_handle.flush_buffer_async(old_ino, true).await {
                    error!("rename: flush failed for ino={}: {}", old_ino, e);
                    reply.error(libc::EIO);
                    return;
                }
                write_buffers.remove(&old_ino);
            }

            // If destination exists, delete it first so the server doesn't have
            // two metadata entries for the same path after rename.
            if let Some(new_ino_val) = new_ino {
                if let Some(existing_meta) = metadata_cache.get(&new_ino_val).map(|m| m.clone()) {
                    if let Err(e) = client.delete_file(&existing_meta.path).await {
                        warn!("rename: failed to delete destination {}: {}", existing_meta.path, e);
                        // Non-fatal: proceed with rename
                    }
                    metadata_cache.remove(&new_ino_val);
                }
            }

            // Get source metadata from local cache (already flushed above).
            let metadata = match metadata_cache.get(&old_ino).map(|m| m.clone()) {
                Some(m) => m,
                None => {
                    // Cache miss after flush — try server.
                    match client.get_file_metadata(&old_path).await {
                        Ok(Some(m)) => m,
                        Ok(None) => { reply.error(libc::ENOENT); return; }
                        Err(e) => {
                            error!("rename: get metadata failed for {}: {}", old_path, e);
                            reply.error(libc::EIO);
                            return;
                        }
                    }
                }
            };

            match client.rename_file(&old_path, &new_path).await {
                Ok(_) => {
                    // Update local path→inode mapping.
                    path_to_inode.write().unwrap().remove(&old_path);
                    path_to_inode.write().unwrap().insert(new_path.clone(), old_ino);

                    // Update local metadata cache with new path.
                    let mut new_metadata = metadata.clone();
                    new_metadata.path = new_path.clone();
                    new_metadata.modified_at = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    metadata_cache.insert(old_ino, new_metadata);

                    // Invalidate directory caches.
                    let raw_old = old_path.rsplitn(2, '/').nth(1).unwrap_or("");
                    let old_parent = if raw_old.is_empty() { "/" } else { raw_old };
                    let raw_new = new_path.rsplitn(2, '/').nth(1).unwrap_or("");
                    let new_parent = if raw_new.is_empty() { "/" } else { raw_new };
                    dir_cache.remove(old_parent);
                    if old_parent != new_parent {
                        dir_cache.remove(new_parent);
                    }

                    info!("Renamed {} -> {} (ino={}, {} chunks)", old_path, new_path, old_ino, metadata.chunk_locations.len());
                    reply.ok();
                }
                Err(e) => {
                    error!("rename: server rename failed {} -> {}: {}", old_path, new_path, e);
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
                    // Truncate to zero: clear metadata and discard any buffered write data.
                    // FUSE converts O_TRUNC into a setattr(size=0) before open(), so this
                    // is the canonical place to reset the write buffer for overwrite scenarios.
                    metadata.chunk_locations = Vec::new();
                    metadata.size = 0;
                    self.write_buffers.remove(&ino);
                    self.size_high_water.remove(&ino);
                    // Mark inode truncated so any in-flight flush discards its stale locations.
                    self.truncated_inodes.insert(ino);
                } else if new_size > metadata.size {
                    // Growing file - just update metadata to extend with zeros
                    info!("Truncate growing: {} -> {} bytes (keeping {} chunks)",
                          metadata.size, new_size, metadata.chunk_locations.len());
                    metadata.size = new_size;
                } else {
                    // Shrinking file - only read chunks up to new_size
                    info!("Truncate shrinking: {} -> {} bytes", metadata.size, new_size);

                    if metadata.chunk_locations.is_empty() {
                        metadata.size = new_size;
                    } else {
                        // Find which chunks we need to keep (up to new_size)
                        let mut cumulative_size = 0u64;
                        let mut last_chunk_idx = 0;
                        let mut bytes_in_last_chunk = 0u64;

                        for (idx, loc) in metadata.chunk_locations.iter().enumerate() {
                            let size = loc.size as u64;
                            if cumulative_size + size <= new_size {
                                cumulative_size += size;
                                last_chunk_idx = idx;
                            } else if cumulative_size < new_size {
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

                            let loc = &metadata.chunk_locations[last_chunk_idx];
                            let chunk_id = loc.chunk_id;
                            let chunk_offset: u64 = metadata.chunk_locations[..last_chunk_idx]
                                .iter().map(|l| l.size as u64).sum();
                            let chunk_size = loc.size as usize;

                            let all_chunk_ids: Vec<_> = metadata.chunk_locations.iter().map(|l| l.chunk_id).collect();
                            let read_hint = vec![crate::client::ChunkReadHint {
                                chunk_idx: last_chunk_idx,
                                chunk_id,
                                full_chunk: true,
                                offset_in_chunk: 0,
                                length: chunk_size,
                                file_offset: chunk_offset,
                            }];

                            let last_chunk_data = match self.block_on(async {
                                client.read_data(&read_hint, &all_chunk_ids, ino, &metadata.chunk_locations).await
                            }) {
                                Ok(data) => data,
                                Err(e) => {
                                    error!("Failed to read last chunk for truncate: {}", e);
                                    reply.error(libc::EIO);
                                    return;
                                }
                            };

                            let truncated_chunk = &last_chunk_data[..bytes_in_last_chunk as usize];

                            match self.block_on(async {
                                client.write_data_with_cache(truncated_chunk, ino, chunk_offset).await
                            }) {
                                Ok((_, _, chunk_locations_opt)) => {
                                    let mut new_locs = metadata.chunk_locations[..last_chunk_idx].to_vec();
                                    if let Some(new_chunk_locs) = chunk_locations_opt {
                                        new_locs.extend(new_chunk_locs);
                                    }
                                    metadata.chunk_locations = new_locs;
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

        // Store updated metadata — stamp write_seq so the server doesn't drop it as stale
        // (the last flush_metadata_sync incremented write_seq on the server, so sending the
        // cached pre-stamp value would be rejected; stamp here to always be monotonically newer).
        let client = self.client.clone();
        let metadata_clone = client.stamp_write_seq_pub(&metadata);
        let result = self.block_on(async {
            client.put_file_metadata(&metadata_clone).await
        });
        // Update the cache with the stamped write_seq so future operations stay consistent.
        metadata.write_seq = metadata_clone.write_seq;

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

            if sync_on_fsync {
                // O_SYNC / O_DSYNC: flush synchronously and wait for network ack.
                let handle = self.flush_handle.clone();
                let result = self.block_on(async move {
                    handle.flush_buffer_async(ino, true).await
                });
                match result {
                    Ok(_) => reply.ok(),
                    Err(e) => { error!("fsync (O_SYNC) failed for inode {}: {}", ino, e); reply.error(libc::EIO); }
                }
            } else {
                // Async flush: spawn and reply immediately. Do NOT insert into in_flight —
                // that set is only for background-ticker vs force-flush races. If we insert
                // here, a concurrent release flush will wait 30s for this task to start,
                // causing the well-known nano close hang.
                let handle = self.flush_handle.clone();
                self.runtime.spawn(async move {
                    if let Err(e) = handle.flush_buffer_async(ino, true).await {
                        error!("fsync background flush failed for inode {}: {}", ino, e);
                    }
                });
                reply.ok();
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
                        debug!("fsync: enqueueing pending metadata for ino={}", ino);
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
