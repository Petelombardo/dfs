use anyhow::Result;
use dashmap::DashMap;
use libc;
use dfs_common::{ChunkLocation, FileMetadata, FileType};
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
/// Check if file should bypass chunk cache (includes all SQLite-related files).
pub fn is_sqlite_for_cache(path: &str) -> bool {
    is_sqlite_path(path)
}

/// SQLite files that go through the write buffer with sync_on_fsync=true.
/// Excludes .db-shm: it is mmap'd MAP_SHARED and must stay on the unbuffered path.
fn is_sqlite_buffered(path: &str) -> bool {
    path.ends_with(".db")
        || path.ends_with(".sqlite")
        || path.ends_with(".sqlite3")
        || path.ends_with(".db-wal")
        || path.ends_with(".db-journal")
        || path.ends_with(".db_temp")
        || path.ends_with(".sqlite_temp")
        || path.ends_with(".sqlite3_temp")
}

/// All SQLite-related paths — used for chunk-data cache bypass and FOPEN_DIRECT_IO.
/// Includes .db-shm (for cache bypass) but .db-shm must NOT get FOPEN_DIRECT_IO.
fn is_sqlite_path(path: &str) -> bool {
    is_sqlite_buffered(path) || path.ends_with(".db-shm")
}

/// Same as is_sqlite_path but excludes .db-shm, which must NOT use FOPEN_DIRECT_IO
/// because SQLite mmaps it (MAP_SHARED) for WAL index coordination.
fn is_sqlite_direct_io(path: &str) -> bool {
    is_sqlite_buffered(path)
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
/// Once flushed successfully, the slot is removed immediately — reads for committed
/// chunks go to the network, which holds the authoritative data.
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
    /// The highest byte offset (exclusive) in `data` that contains real app-written data.
    /// Bytes beyond this point are synthetic gap-fill zeros representing data already on
    /// the server. Used to bound PatchChunk so we don't send gap-fill zeros as real data
    /// when the slot is padded to CHUNK_SIZE but only a small region was actually written.
    real_data_end: usize,
    /// Set to true by flush_one_chunk when it claims this slot for network I/O.
    /// Prevents a second concurrent flush task from picking the same slot.
    /// Cleared on failure (so it can be retried); on success the slot is removed.
    flushing: bool,
}

impl ChunkSlot {
    fn new() -> Self {
        Self {
            data: Vec::with_capacity(CHUNK_SIZE),
            last_modified: SystemTime::now(),
            gap_filled_prefix: 0,
            real_data_end: 0,
            flushing: false,
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
    /// Tracks how many bytes were flushed for each chunk index. Used by the PatchChunk
    /// logic to detect append-extend: if a slot was partially flushed, flushed_sizes[idx]
    /// tells us how many bytes the server already has for that chunk.
    flushed_sizes: HashMap<u64, usize>,
    /// Chunk IDs that were current when this write session opened the file.
    /// Used by PatchChunk to detect a stale-write race: if another session already
    /// patched chunk N between our open() and flush, chunk_ids_at_open[N] will no
    /// longer match metadata_cache — discard this flush rather than reverting.
    chunk_ids_at_open: HashMap<u64, dfs_common::ChunkId>,
    /// If true, every fsync() must flush immediately (O_SYNC / O_DSYNC was set on open).
    /// If false, fsyncs within the coalescing window are absorbed (DVR / streaming mode).
    sync_on_fsync: bool,
    /// True when the file was opened with O_TRUNC (full replacement). PatchChunk must not
    /// be used in this session — the caller is writing a new file, not patching an old one.
    is_truncated_session: bool,
}

impl InodeWriteState {
    fn new(sync_on_fsync: bool) -> Self {
        Self {
            slots: HashMap::new(),
            flushed_sizes: HashMap::new(),
            chunk_ids_at_open: HashMap::new(),
            sync_on_fsync,
            is_truncated_session: false,
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
            let slot = self.slots.entry(idx).or_insert_with(|| {
                let mut s = ChunkSlot::new();
                // Gap-fill bytes already on the server so the slot accurately represents
                // the full chunk state. Without this, is_append_extend PatchChunk would
                // send only the tail, missing the first flushed_sizes bytes.
                if flushed > 0 {
                    s.data.resize(flushed, 0u8);
                    s.gap_filled_prefix = flushed;
                }
                s
            });

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
            // If the app writes at or before the gap-filled prefix, real data now covers
            // that region — shrink gap_filled_prefix so PatchChunk sends the real bytes
            // rather than treating the entire gap-fill as already-on-server data.
            if intra < slot.gap_filled_prefix {
                slot.gap_filled_prefix = intra;
            }
            // Track the furthest byte of real app-written data. Bytes beyond this in the
            // slot are synthetic gap-fill zeros (representing data already on the server)
            // and must not be sent as real patch data.
            let write_end = intra + n;
            if write_end > slot.real_data_end {
                slot.real_data_end = write_end;
            }
            slot.last_modified = SystemTime::now();
            // A real write() supersedes the open-time snapshot for this chunk.
            // The stale-write guard uses chunk_ids_at_open to detect sessions that
            // buffered content at open time and would revert a competing write — but
            // once an actual write() has landed here, the slot has newer data than
            // anything on the server and must not be discarded.
            self.chunk_ids_at_open.remove(&idx);

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

    /// Slots that are full and not yet claimed by a flush task.
    fn full_slot_indices(&self) -> Vec<u64> {
        self.slots.iter()
            .filter(|(_, s)| s.is_full() && !s.flushing)
            .map(|(idx, _)| *idx)
            .collect()
    }

    /// All slot indices sorted ascending. Used by fsync/release.
    fn all_slot_indices(&self) -> Vec<u64> {
        let mut indices: Vec<u64> = self.slots.iter()
            .filter(|(_, s)| !s.flushing)
            .map(|(idx, _)| *idx)
            .collect();
        indices.sort_unstable();
        indices
    }
}


/// Number of chunk-flush tasks that may run concurrently per inode.
/// 2 means 2 × 2 replica connections = 4 simultaneous node connections,
/// which fits a 5-node cluster without thrashing.
const PIPELINE_CHUNKS: usize = 2;

/// Number of full chunk slots the writer may buffer ahead of the pipeline.
/// With PIPELINE_CHUNKS=2 flushing and BUFFER_CHUNKS=4 in the buffer, the
/// writer is always filling the next slots while the current ones are in-flight.
const BUFFER_CHUNKS: usize = 4;

/// Cheaply-cloneable handle to the fields needed by flush_buffer_async.
/// Extracted so fsync() can clone it and spawn a background flush task without
/// holding a reference to DfsFilesystem (which is !Clone due to &mut self callbacks).
#[derive(Clone)]
struct FlushHandle {
    client: Arc<DfsClient>,
    write_buffers: Arc<DashMap<u64, Arc<Mutex<InodeWriteState>>>>,
    metadata_cache: Arc<DashMap<u64, FileMetadata>>,
    /// Tracks how many chunk-flush tasks are currently in-flight per inode.
    /// Capped at PIPELINE_CHUNKS by the background ticker.
    flush_in_flight: Arc<RwLock<Option<Arc<DashMap<u64, usize>>>>>,
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
    /// Global write buffer byte counter — decremented when slots are flushed and removed.
    global_buffered_bytes: Arc<std::sync::atomic::AtomicUsize>,
    /// Notification channel to wake up flush workers immediately when chunks become full.
    /// This eliminates the 0-50ms polling delay from the ticker-based approach.
    flush_notify: Arc<tokio::sync::Notify>,
    /// Per-inode count of in-flight FUSE write() tasks. Used by flush_one_chunk to wait
    /// for all writes to land before snapshotting a full slot.
    write_tasks_in_flight: Arc<DashMap<u64, Arc<std::sync::atomic::AtomicUsize>>>,
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
            let in_flight_map = self.flush_in_flight.read().unwrap().as_ref().cloned();
            if let Some(in_flight_map) = in_flight_map {
                let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
                while in_flight_map.get(&ino).map(|v| *v).unwrap_or(0) > 0 {
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
                        .and_then(|m| m.chunk_location_for_idx(*chunk_idx).map(|l| l.size))
                        .unwrap_or(0)
                })
            };
            let chunk_exists = existing_chunk_size > 0;
            // Detect append: the slot was gap-zero-filled up to existing_chunk_size, then
            // real data was appended beyond. Use gap_filled_prefix (set explicitly when we
            // zero-fill) rather than checking for all-zeros — the all-zeros heuristic was
            // wrong when the server's real chunk 0 data happened to start with zeros.
            let (gap_filled_prefix, real_data_end) = {
                let state_lock = self.write_buffers.get(&ino);
                state_lock.and_then(|s| s.try_lock().ok()
                    .and_then(|st| st.slots.get(&chunk_idx).map(|sl| (sl.gap_filled_prefix, sl.real_data_end))))
                    .unwrap_or((0, 0))
            };
            // is_append_extend also covers the full-chunk case: if the slot grew to exactly
            // 4MB but started with a gap-filled prefix (bytes already on server), we must
            // still PatchChunk rather than doing a fresh WriteData. A fresh write would
            // send the gap-fill zeros as real data, overwriting the server's existing bytes
            // (e.g. the 12KB JSON header) with zeros. This is the root cause of broken
            // recordings: the release flush writes a full 4MB slot containing gap-fill
            // zeros at the start, replacing the real header content on the server.
            let is_append_extend = chunk_exists
                && slot_len > existing_chunk_size
                && gap_filled_prefix >= existing_chunk_size;
            // Use PatchChunk for a genuine partial in-place edit (conv=notrunc style), but NOT
            // when the session was opened with O_TRUNC — that is a full replacement and must
            // emit a fresh WriteChunk so the old tail bytes are not left on the server.
            let is_truncated_session = self.write_buffers.get(&ino)
                .and_then(|s| s.try_lock().ok().map(|st| st.is_truncated_session))
                .unwrap_or(false);
            // Use real_data_end to detect a small overwrite into a full-sized slot.
            // A slot padded to CHUNK_SIZE with gap-fill zeros must not be sent as a fresh
            // write — only the real written bytes should be patched onto the server.
            let effective_write_end = if real_data_end > 0 { real_data_end } else { slot_len };
            let is_overwrite = chunk_exists
                && effective_write_end <= existing_chunk_size
                && !is_truncated_session;
            let needs_patch = is_overwrite || is_append_extend;

            if needs_patch {
                let (patch_intra, patch_bytes) = if is_append_extend {
                    // Send only the new appended bytes, starting at the old chunk boundary.
                    (existing_chunk_size, slot_data[existing_chunk_size..].to_vec())
                } else {
                    // In-place overwrite: send only the real app-written bytes.
                    // Use real_data_end to bound the patch so gap-fill zeros beyond the
                    // last real write are not sent (they would overwrite real server data).
                    let real_start = gap_filled_prefix;
                    let real_end = effective_write_end;
                    (real_start, slot_data[real_start..real_end].to_vec())
                };
                info!("flush_buffer_async: slot {} ({} bytes) — PatchChunk intra={} patch_len={}",
                      chunk_idx, slot_len, patch_intra, patch_bytes.len());
                let meta = self.metadata_cache.get(&ino).map(|m| m.clone());
                let patched = if let Some(meta) = meta {
                    let old_location_opt = meta.chunk_location_for_idx(*chunk_idx).cloned();
                    if let Some(old_location) = old_location_opt {
                        // Stale-write guard: discard if another session patched this chunk after our open().
                        let id_at_open = self.write_buffers.get(&ino)
                            .and_then(|s| s.try_lock().ok()
                                .and_then(|st| st.chunk_ids_at_open.get(chunk_idx).copied()));
                        if let Some(open_id) = id_at_open {
                            if open_id != old_location.chunk_id {
                                info!("flush_buffer_async: ino={} chunk={} chunk_id changed since open — discarding stale write", ino, chunk_idx);
                                continue;
                            }
                        }
                        let patch_result = self.client.patch_chunk_on_replicas(
                            old_location.chunk_id,
                            file_offset,
                            patch_intra,
                            patch_bytes.clone(),
                            &old_location,
                        ).await;

                        let patch_result = match patch_result {
                            Ok(loc) => Ok(loc),
                            Err(e) => {
                                // PatchChunk failed — the replica locations in our cache may be
                                // stale (chunk was healed to different nodes since we last fetched
                                // metadata). Re-fetch from the leader and retry once before giving
                                // up. This prevents the catastrophic fallback where we write a new
                                // 12KB standalone chunk that overwrites the chunk map entry for an
                                // existing multi-GB file.
                                warn!("flush_buffer_async: PatchChunk failed for slot {} ({}), re-fetching metadata and retrying", chunk_idx, e);
                                let path_opt = self.path_to_inode.read().unwrap()
                                    .iter().find(|(_, &v)| v == ino).map(|(k, _)| k.clone());
                                if let Some(path) = path_opt {
                                    match self.client.get_file_metadata(&path).await {
                                        Ok(Some(fresh_meta)) => {
                                            self.client.seed_write_seq(fresh_meta.id, fresh_meta.write_seq);
                                            if let Some(fresh_loc) = fresh_meta.chunk_location_for_idx(*chunk_idx).cloned() {
                                                info!("flush_buffer_async: retrying PatchChunk slot {} with fresh location {} (was {})",
                                                      chunk_idx, fresh_loc.chunk_id, old_location.chunk_id);
                                                let retry = self.client.patch_chunk_on_replicas(
                                                    fresh_loc.chunk_id,
                                                    file_offset,
                                                    patch_intra,
                                                    patch_bytes.clone(),
                                                    &fresh_loc,
                                                ).await;
                                                // Update cache with fresh metadata regardless of retry outcome
                                                self.metadata_cache.insert(ino, fresh_meta);
                                                retry
                                            } else {
                                                self.metadata_cache.insert(ino, fresh_meta);
                                                Err(e)
                                            }
                                        }
                                        Ok(None) | Err(_) => Err(e),
                                    }
                                } else {
                                    Err(e)
                                }
                            }
                        };

                        match patch_result {
                            Ok(new_location) => {
                                info!("flush_buffer_async: PatchChunk slot {} succeeded: {} -> {}",
                                      chunk_idx, old_location.chunk_id, new_location.chunk_id);
                                if let Some(mut meta_entry) = self.metadata_cache.get_mut(&ino) {
                                    if let Some(loc) = meta_entry.chunk_location_for_idx_mut(*chunk_idx) {
                                        *loc = new_location.clone();
                                    }
                                    if let Some(new_size) = meta_entry.chunk_locations.iter()
                                        .filter_map(|l| l.file_offset.map(|o| o + l.size as u64))
                                        .reduce(u64::max)
                                    {
                                        meta_entry.size = meta_entry.size.max(new_size);
                                    }
                                }
                                patch_metadata_dirty = true;
                                true
                            }
                            Err(e) => {
                                // PatchChunk failed even after metadata refresh. The chunk exists
                                // on disk (it's part of a committed file) but we cannot reach it.
                                // Falling back to a fresh write here would be catastrophic: it
                                // would create a new standalone chunk at this file_offset and
                                // overwrite the chunk map entry, silently discarding all other
                                // chunks of the file. Return the error instead — the caller
                                // (release/fsync) will propagate EIO and the user can retry.
                                warn!("flush_buffer_async: PatchChunk failed for slot {} after metadata refresh — cannot safely fall back for existing chunk: {}", chunk_idx, e);
                                return Err(e);
                            }
                        }
                    } else {
                        // chunk_locations[chunk_idx] is missing — our local cache is stale.
                        // This happens during long live recordings when the server has committed
                        // more chunks than the client's metadata_cache reflects. Fetching fresh
                        // metadata here prevents a ghost chunk accumulation: without this, we
                        // fall through to a fresh write that appends a duplicate tail entry
                        // instead of patching the existing one, producing hundreds of spurious
                        // partial chunk_location records that break Kodi seekability.
                        warn!("flush_buffer_async: no chunk_location for slot {} in local cache — fetching fresh metadata before deciding", chunk_idx);
                        let path_opt = self.path_to_inode.read().unwrap()
                            .iter().find(|(_, &v)| v == ino).map(|(k, _)| k.clone());
                        let mut refreshed = false;
                        if let Some(path) = path_opt {
                            if let Ok(Some(fresh_meta)) = self.client.get_file_metadata(&path).await {
                                self.client.seed_write_seq(fresh_meta.id, fresh_meta.write_seq);
                                let fresh_loc = fresh_meta.chunk_location_for_idx(*chunk_idx).cloned();
                                self.metadata_cache.insert(ino, fresh_meta);
                                if let Some(old_location) = fresh_loc {
                                    info!("flush_buffer_async: retrying PatchChunk slot {} after cache refresh (loc={})", chunk_idx, old_location.chunk_id);
                                    let retry = self.client.patch_chunk_on_replicas(
                                        old_location.chunk_id,
                                        file_offset,
                                        patch_intra,
                                        patch_bytes.clone(),
                                        &old_location,
                                    ).await;
                                    match retry {
                                        Ok(new_location) => {
                                            info!("flush_buffer_async: PatchChunk slot {} succeeded after cache refresh: {} -> {}",
                                                  chunk_idx, old_location.chunk_id, new_location.chunk_id);
                                            if let Some(mut meta_entry) = self.metadata_cache.get_mut(&ino) {
                                                if let Some(loc) = meta_entry.chunk_location_for_idx_mut(*chunk_idx) {
                                                    *loc = new_location.clone();
                                                }
                                            }
                                            patch_metadata_dirty = true;
                                            refreshed = true;
                                        }
                                        Err(e) => {
                                            warn!("flush_buffer_async: PatchChunk slot {} failed after cache refresh: {}", chunk_idx, e);
                                        }
                                    }
                                }
                            }
                        }
                        refreshed
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
        let t_flush_start = std::time::Instant::now();
        let t_preseed = t_flush_start.elapsed();

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

        let t_net_start = std::time::Instant::now();
        let results = futures::future::join_all(handles).await;
        let t_net = t_net_start.elapsed();

        // Process results: track flushed sizes and collect locations.
        // DO NOT remove slots yet - wait until read engine is updated to avoid race.
        let mut all_locations: Vec<dfs_common::ChunkLocation> = Vec::new();
        let mut first_err: Option<anyhow::Error> = None;
        let mut failed_chunk_indices: Vec<usize> = Vec::new();
        let mut flushed_chunks: Vec<(u64, usize)> = Vec::new(); // (chunk_idx, flushed_len)

        for (join_result, (chunk_idx, slot_data_snap, _)) in results.into_iter().zip(slots_to_write.iter()) {
            match join_result {
                Ok(Ok((_, locations_opt))) => {
                    // Mark slot as flushed but DON'T remove yet. Slots will be removed
                    // after read engine is updated to prevent race where reads fall through
                    // to network with stale chunk_map.
                    let flushed_len = slot_data_snap.len();
                    if let Some(state_lock) = self.write_buffers.get(&ino) {
                        let mut state = state_lock.lock().await;
                        state.flushed_sizes.insert(*chunk_idx, flushed_len);
                    }
                    flushed_chunks.push((*chunk_idx, flushed_len));
                    if let Some(locations) = locations_opt {
                        all_locations.extend(locations);
                    }
                }
                Ok(Err(e)) => {
                    failed_chunk_indices.push(*chunk_idx as usize);
                    if first_err.is_none() { first_err = Some(e); }
                }
                Err(e) => {
                    failed_chunk_indices.push(*chunk_idx as usize);
                    if first_err.is_none() { first_err = Some(anyhow::anyhow!("flush task panicked: {}", e)); }
                }
            }
        }


        if let Some(e) = first_err {
            return Err(e);
        }

        let t_meta_start = std::time::Instant::now();
        info!("flush ino={} chunks={} | preseed={:?} net={:?} (so far {:?})",
              ino, slots_to_write.len(), t_preseed, t_net, t_flush_start.elapsed());
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
                                let old_cid = meta.chunk_locations[pos].chunk_id;
                                if old_cid != loc.chunk_id {
                                    let client = self.client.clone();
                                    tokio::spawn(async move {
                                        client.chunk_cache.invalidate(&old_cid).await;
                                    });
                                }
                                meta.chunk_locations[pos] = loc.clone();
                                continue;
                            }
                        }
                        meta.chunk_locations.push(loc.clone());
                    }
                }
                // File size = max of logical size (set by truncate) and physical chunk end.
                // A sparse file grown via truncate has a logical size larger than its written
                // chunks; clobbering with the physical end would shrink the reported size.
                if let Some(last) = meta.chunk_locations.iter()
                    .filter_map(|l| l.file_offset.map(|o| o + l.size as u64))
                    .reduce(u64::max)
                {
                    meta.size = meta.size.max(last);
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
                // release/fsync: commit metadata to leader, THEN update read engine.
                // Ordering matters: readers must not see chunk IDs before the leader
                // has them — otherwise a leader refresh overwrites our engine update
                // with stale data and readers get the wrong chunks.
                self.client.flush_metadata_sync(&meta).await;
                self.last_metadata_update.insert(ino, std::time::Instant::now());
                // Now the leader has the metadata — safe to populate the read engine.
                let current_size = meta.size;
                self.client.feed_chunk_locations_to_read_engine(
                    ino, &meta.chunk_locations, current_size,
                ).await;
            } else {
                // Background tick: push directly into the queue (no back-pressure wait).
                // enqueue_metadata() may block waiting to rescue a stalled queue entry,
                // which would hold the in_flight slot and prevent new background flushes
                // from starting — starving the write pipeline. We stamp the seq and push
                // directly; the queue worker handles delivery and retries independently.
                self.last_metadata_update.insert(ino, std::time::Instant::now());
                let stamped = self.client.stamp_write_seq_pub(&meta);
                self.client.metadata_queue.push(stamped).await;
                // For background flushes, update read engine immediately (not queued)
                // so reads see fresh chunk_map before slots are removed below.
                let current_size = meta.size;
                self.client.feed_chunk_locations_to_read_engine(
                    ino, &meta.chunk_locations, current_size,
                ).await;
            }
        }

        // Now that read engine is updated, safe to remove flushed slots.
        // This ordering prevents race where reads fall through to network with stale chunk_map.
        if !flushed_chunks.is_empty() {
            if let Some(state_lock) = self.write_buffers.get(&ino) {
                let mut state = state_lock.lock().await;
                for (chunk_idx, flushed_len) in flushed_chunks {
                    state.slots.remove(&chunk_idx);
                    self.global_buffered_bytes.fetch_sub(
                        flushed_len.min(self.global_buffered_bytes.load(std::sync::atomic::Ordering::Relaxed)),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
            }
        }

        info!("flush ino={} complete | preseed={:?} net={:?} meta={:?} total={:?}",
              ino, t_preseed, t_net, t_meta_start.elapsed(), t_flush_start.elapsed());
        Ok(())
    }

    /// Flush exactly one full (or idle) chunk slot for `ino`.
    /// Called by the background ticker; each call handles one chunk so the pipeline
    /// drains one slot at a time rather than batch-flushing all full slots at once.
    async fn flush_one_chunk(&self, ino: u64) -> Result<()> {
        // Pick the lowest-index unclaimed full slot, falling back to the lowest idle slot.
        // Atomically set flushing=true while holding the mutex so no second concurrent
        // task can claim the same slot.
        let (chunk_idx, slot_data, file_offset, gap_filled_prefix, real_data_end) = {
            let Some(state_arc) = self.write_buffers.get(&ino) else { return Ok(()); };
            let mut state = state_arc.lock().await;

            // Full slots first (lowest index, not already claimed, not already on server)
            let mut full: Vec<u64> = state.slots.iter()
                .filter(|(_, s)| s.is_full() && !s.flushing)
                .map(|(idx, _)| *idx)
                .collect();
            full.sort_unstable();

            let idx = if let Some(i) = full.into_iter().next() {
                i
            } else {
                // No full unclaimed slot — try the oldest idle partial slot.
                let mut idle: Vec<(u64, SystemTime)> = state.slots.iter()
                    .filter(|(_, s)| s.is_idle() && !s.data.is_empty() && !s.flushing)
                    .map(|(idx, s)| (*idx, s.last_modified))
                    .collect();
                idle.sort_by_key(|&(_, t)| t);
                match idle.into_iter().next() {
                    Some((i, _)) => i,
                    None => return Ok(()),
                }
            };

            // Claim the slot and snapshot its data — all while holding the lock.
            let slot = match state.slots.get_mut(&idx) {
                Some(s) if !s.data.is_empty() => s,
                _ => return Ok(()),
            };
            slot.flushing = true;
            let data = slot.data.clone();
            let gap_filled_prefix = slot.gap_filled_prefix;
            let real_data_end = slot.real_data_end;
            let offset = idx * CHUNK_SIZE as u64;
            (idx, data, offset, gap_filled_prefix, real_data_end)
            // mutex released here
        };

        // Wait for any in-flight write() tasks to finish before flushing — but ONLY when
        // the slot has a gap-filled prefix. This is the sparse-file case: a write at a
        // high intra-offset zero-fills the preceding region, making the slot full on the
        // first byte. The remaining bytes of that write are still queued. Without waiting,
        // the snapshot sends zeros for the unfinished tail.
        //
        // Sequential appends (DVR) have gap_filled_prefix=0 — they fill slots incrementally
        // and the flusher fires after the slot is complete. No wait needed, and waiting
        // would stall on write_tasks_in_flight>0 (continuous incoming writes), breaking
        // throughput completely.
        if gap_filled_prefix > 0 {
        if let Some(counter) = self.write_tasks_in_flight.get(&ino) {
            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
            while counter.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                if tokio::time::Instant::now() > deadline {
                    warn!("flush_one_chunk: timed out waiting for write tasks for ino={}", ino);
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
            }
            // Re-snapshot the slot now that all writes have landed.
            if let Some(state_arc) = self.write_buffers.get(&ino) {
                let mut state = state_arc.lock().await;
                if let Some(slot) = state.slots.get_mut(&chunk_idx) {
                    let data = slot.data.clone();
                    let gap_filled_prefix = slot.gap_filled_prefix;
                    let real_data_end = slot.real_data_end;
                    let file_offset = chunk_idx * CHUNK_SIZE as u64;
                    drop(state);
                    return self.flush_buffer_async_one(ino, chunk_idx, data, file_offset, gap_filled_prefix, real_data_end).await;
                }
                info!("flush_one_chunk: ino={} chunk={} slot gone after wait — already flushed elsewhere", ino, chunk_idx);
            }
        } // write_tasks_in_flight guard
        } // gap_filled_prefix guard

        self.flush_buffer_async_one(ino, chunk_idx, slot_data, file_offset, gap_filled_prefix, real_data_end).await
    }

    /// Internal: flush exactly the chunk at `chunk_idx` for `ino`.
    /// Slot data, file offset, gap_filled_prefix, and real_data_end are all pre-snapshotted
    /// by flush_one_chunk while holding the mutex. Reading these from the live slot after
    /// release is wrong — the slot may have been removed and recreated by a concurrent writer.
    async fn flush_buffer_async_one(&self, ino: u64, chunk_idx: u64, slot_data: Vec<u8>, file_offset: u64, gap_filled_prefix: usize, real_data_end: usize) -> Result<()> {

        // Check whether this slot needs PatchChunk or a fresh write.
        let existing_chunk_size = {
            let from_flushed = self.write_buffers.get(&ino)
                .and_then(|s| s.try_lock().ok()
                    .and_then(|st| st.flushed_sizes.get(&chunk_idx).copied()));
            from_flushed.unwrap_or_else(|| {
                self.metadata_cache.get(&ino)
                    .and_then(|m| m.chunk_location_for_idx(chunk_idx).map(|l| l.size))
                    .unwrap_or(0)
            })
        };
        let chunk_exists = existing_chunk_size > 0;
        let slot_len = slot_data.len();
        let is_append_extend = chunk_exists
            && slot_len > existing_chunk_size
            && gap_filled_prefix >= existing_chunk_size;
        let is_truncated_session = self.write_buffers.get(&ino)
            .and_then(|s| s.try_lock().ok().map(|st| st.is_truncated_session))
            .unwrap_or(false);
        // A slot is an overwrite if: chunk exists on server, not a truncated session, and
        // the real written data doesn't exceed what's already there. We use real_data_end
        // (not slot_len) because a full-sized slot may be padded with gap-fill zeros that
        // represent data already on the server — those must NOT be sent as patch data.
        let effective_write_end = if real_data_end > 0 { real_data_end } else { slot_len };
        let is_overwrite = chunk_exists
            && effective_write_end <= existing_chunk_size
            && !is_truncated_session;
        let needs_patch = is_overwrite || is_append_extend;

        if needs_patch {
            let (patch_intra, patch_bytes) = if is_append_extend {
                (existing_chunk_size, slot_data[existing_chunk_size..].to_vec())
            } else {
                let real_start = gap_filled_prefix;
                let real_end = effective_write_end;
                (real_start, slot_data[real_start..real_end].to_vec())
            };
            let meta = self.metadata_cache.get(&ino).map(|m| m.clone());
            if let Some(meta) = meta {
                if let Some(old_location) = meta.chunk_location_for_idx(chunk_idx).cloned() {
                    // Stale-write guard: if another session already patched this chunk
                    // between our open() and now, the current chunk_id will differ from
                    // what we snapshotted at open time. Our write buffer contains bytes
                    // read at open time — applying them now would revert the newer write.
                    let id_at_open = self.write_buffers.get(&ino)
                        .and_then(|s| s.try_lock().ok()
                            .and_then(|st| st.chunk_ids_at_open.get(&chunk_idx).copied()));
                    info!("flush_buffer_async_one: ino={} chunk={} id_at_open={:?} current_id={}",
                        ino, chunk_idx, id_at_open, old_location.chunk_id);
                    if let Some(open_id) = id_at_open {
                        if open_id != old_location.chunk_id {
                            info!("flush_buffer_async_one: ino={} chunk={} chunk_id changed since open ({} -> {}) — discarding stale write",
                                ino, chunk_idx, open_id, old_location.chunk_id);
                            if let Some(state_arc) = self.write_buffers.get(&ino) {
                                if let Ok(mut state) = state_arc.try_lock() {
                                    state.slots.remove(&chunk_idx);
                                }
                            }
                            return Ok(());
                        }
                    }
                    let patch_result = self.client.patch_chunk_on_replicas(
                        old_location.chunk_id,
                        file_offset,
                        patch_intra,
                        patch_bytes,
                        &old_location,
                    ).await;
                    match patch_result {
                        Ok(new_location) => {
                            if let Some(mut meta_entry) = self.metadata_cache.get_mut(&ino) {
                                if let Some(loc) = meta_entry.chunk_location_for_idx_mut(chunk_idx) {
                                    *loc = new_location.clone();
                                }
                                if let Some(new_size) = meta_entry.chunk_locations.iter()
                                    .filter_map(|l| l.file_offset.map(|o| o + l.size as u64))
                                    .reduce(u64::max)
                                {
                                    meta_entry.size = meta_entry.size.max(new_size);
                                }
                            }
                            // Commit metadata to leader FIRST, then update read engine, THEN remove slot.
                            // This ordering prevents race where reads fall through to network with stale chunk_map.
                            let (flushed_len, meta_to_persist) = {
                                if let Some(state_arc) = self.write_buffers.get(&ino) {
                                    let state = state_arc.lock().await;
                                    let len = state.slots.get(&chunk_idx).map(|s| s.data.len()).unwrap_or(0);
                                    (len, self.metadata_cache.get(&ino).map(|m| m.clone()))
                                } else {
                                    (0, None)
                                }
                            };
                            if let Some(meta) = meta_to_persist {
                                self.client.flush_metadata_sync(&meta).await;
                                self.last_metadata_update.insert(ino, std::time::Instant::now());
                                self.client.feed_chunk_locations_to_read_engine(
                                    ino, &meta.chunk_locations, meta.size,
                                ).await;
                            }
                            // Now that read engine is updated, safe to remove slot.
                            if let Some(state_arc) = self.write_buffers.get(&ino) {
                                let mut state = state_arc.lock().await;
                                if flushed_len > 0 {
                                    state.flushed_sizes.insert(chunk_idx, flushed_len);
                                    self.global_buffered_bytes.fetch_sub(
                                        flushed_len.min(self.global_buffered_bytes.load(std::sync::atomic::Ordering::Relaxed)),
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                }
                                state.slots.remove(&chunk_idx);
                            }
                            return Ok(());
                        }
                        Err(e) => {
                            warn!("flush_one_chunk: PatchChunk failed for slot {} — {}", chunk_idx, e);
                            if let Some(state_arc) = self.write_buffers.get(&ino) {
                                if let Ok(mut state) = state_arc.try_lock() {
                                    if let Some(slot) = state.slots.get_mut(&chunk_idx) {
                                        slot.flushing = false;
                                    }
                                }
                            }
                            return Err(e);
                        }
                    }
                }
            }
            // No metadata or location — fall through to normal write
        }

        // Write path: send data to server, update metadata & read engine, THEN remove slot.
        let result = self.client.write_data_with_cache(&slot_data, ino, file_offset).await;
        match result {
            Ok((_, _, Some(locations))) => {
                let flushed_len = slot_data.len();

                // Track flushed size but DON'T remove slot yet.
                if let Some(state_arc) = self.write_buffers.get(&ino) {
                    let mut state = state_arc.lock().await;
                    state.flushed_sizes.insert(chunk_idx, flushed_len);
                }

                // Fetch metadata if not cached
                if !self.metadata_cache.contains_key(&ino) {
                    let path_opt = self.path_to_inode.read().unwrap()
                        .iter().find(|(_, &v)| v == ino).map(|(k, _)| k.clone());
                    if let Some(path) = path_opt {
                        if let Ok(Some(fetched)) = self.client.get_file_metadata(&path).await {
                            self.client.seed_write_seq(fetched.id, fetched.write_seq);
                            self.metadata_cache.insert(ino, fetched);
                        }
                    }
                }

                if let Some(mut meta) = self.metadata_cache.get_mut(&ino) {
                    if self.truncated_inodes.contains(&ino) {
                        return Ok(());
                    }
                    for loc in &locations {
                        if !meta.chunk_locations.iter().any(|l| l.chunk_id == loc.chunk_id) {
                            if let Some(offset) = loc.file_offset {
                                if let Some(pos) = meta.chunk_locations.iter().position(|l| l.file_offset == Some(offset)) {
                                    // Evict the old chunk_id from the read cache so stale partial
                                    // data can't be served after the slot is replaced with a larger
                                    // (or different) flush of the same chunk offset.
                                    let old_cid = meta.chunk_locations[pos].chunk_id;
                                    if old_cid != loc.chunk_id {
                                        let client = self.client.clone();
                                        tokio::spawn(async move {
                                            client.chunk_cache.invalidate(&old_cid).await;
                                        });
                                    }
                                    meta.chunk_locations[pos] = loc.clone();
                                    continue;
                                }
                                // Insert at the correct position based on file_offset to keep
                                // chunk_locations sorted. This prevents out-of-order chunks when
                                // concurrent flush tasks complete in non-sequential order.
                                let insert_pos = meta.chunk_locations.iter()
                                    .position(|l| l.file_offset.map(|o| o > offset).unwrap_or(false))
                                    .unwrap_or(meta.chunk_locations.len());
                                meta.chunk_locations.insert(insert_pos, loc.clone());
                            } else {
                                // No file_offset — append to end (shouldn't happen in normal operation)
                                meta.chunk_locations.push(loc.clone());
                            }
                        }
                    }
                    if let Some(last) = meta.chunk_locations.iter()
                        .filter_map(|l| l.file_offset.map(|o| o + l.size as u64))
                        .reduce(u64::max)
                    {
                        meta.size = meta.size.max(last);
                    }
                    meta.modified_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                }

                let meta_to_persist = self.metadata_cache.get(&ino).map(|m| m.clone());
                if let Some(meta) = meta_to_persist {
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
                        } else {
                            self.dir_cache.remove(&parent);
                        }
                    }
                    self.last_metadata_update.insert(ino, std::time::Instant::now());
                    // Commit metadata to leader first, then update the read engine.
                    // This ensures leader refreshes never overwrite our engine update
                    // with stale data — the leader has the chunk map before we expose it.
                    self.client.flush_metadata_sync(&meta).await;
                    // Update read engine after leader has the metadata.
                    let current_size = meta.size;
                    self.client.feed_chunk_locations_to_read_engine(
                        ino, &meta.chunk_locations, current_size,
                    ).await;
                }
                // Now that read engine is updated, safe to remove slot — unless new data
                // arrived while the flush was in flight (concurrent writer added bytes).
                // In that case, keep the slot so the next flush cycle sends the new data.
                if let Some(state_arc) = self.write_buffers.get(&ino) {
                    let mut state = state_arc.lock().await;
                    let current_len = state.slots.get(&chunk_idx).map(|s| s.data.len()).unwrap_or(0);
                    if current_len <= flushed_len {
                        state.slots.remove(&chunk_idx);
                        self.global_buffered_bytes.fetch_sub(
                            flushed_len.min(self.global_buffered_bytes.load(std::sync::atomic::Ordering::Relaxed)),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    } else {
                        // New data arrived during flush — clear flushing flag and record the
                        // committed size so the next flush knows what's already on the server.
                        state.flushed_sizes.insert(chunk_idx, flushed_len);
                        if let Some(slot) = state.slots.get_mut(&chunk_idx) {
                            slot.flushing = false;
                        }
                        self.global_buffered_bytes.fetch_sub(
                            flushed_len.min(self.global_buffered_bytes.load(std::sync::atomic::Ordering::Relaxed)),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }
                }
                Ok(())
            }
            Ok((_, _, None)) => {
                Ok(())
            }
            Err(e) => {
                if let Some(state_arc) = self.write_buffers.get(&ino) {
                    if let Ok(mut state) = state_arc.try_lock() {
                        if let Some(slot) = state.slots.get_mut(&chunk_idx) {
                            slot.flushing = false;
                        }
                    }
                }
                Err(e)
            }
        }
    }

    /// Drain ALL dirty slots for `ino` (including partial tail) through the
    /// same PIPELINE_CHUNKS-capped pipeline used by the background ticker.
    /// Used by release() and fsync() so close/sync never exceeds 2 concurrent
    /// writes — keeping total cluster connections at 2×2=4 on a 5-node cluster.
    async fn flush_all_pipelined(&self, ino: u64) -> Result<()> {
        // Wait for any background pipeline tasks to finish first so we don't
        // race with them on slot ownership.
        let in_flight_map = self.flush_in_flight.read().unwrap().as_ref().cloned();
        if let Some(ref map) = in_flight_map {
            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
            while map.get(&ino).map(|v| *v).unwrap_or(0) > 0 {
                if tokio::time::Instant::now() >= deadline { break; }
                tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
            }
        }

        // Make partial slots eligible for flush_one_chunk by temporarily marking
        // them as idle (last_modified = old enough). They're already no longer
        // being written to (release/fsync caller guarantees this).
        if let Some(state_arc) = self.write_buffers.get(&ino) {
            let mut state = state_arc.lock().await;
            let epoch = SystemTime::UNIX_EPOCH; // far in the past → is_idle() = true
            for (_, slot) in state.slots.iter_mut() {
                if !slot.data.is_empty() {
                    slot.last_modified = epoch;
                }
            }
        }

        let mut first_err: Option<anyhow::Error> = None;

        // Keep dispatching up to PIPELINE_CHUNKS concurrent flush_one_chunk calls
        // until no unclaimed slots remain.
        loop {
            // Count how many slots are still pending (unclaimed and non-empty).
            let pending = self.write_buffers.get(&ino).map(|s| {
                s.try_lock().map(|st| {
                    st.slots.values().filter(|sl| !sl.data.is_empty() && !sl.flushing).count()
                }).unwrap_or(1) // if locked, assume work remains
            }).unwrap_or(0);

            if pending == 0 { break; }

            // Count currently in-flight slots for this inode (claimed, flushing=true).
            let in_flight_count = self.write_buffers.get(&ino).map(|s| {
                s.try_lock().map(|st| st.slots.values().filter(|sl| sl.flushing).count()).unwrap_or(0)
            }).unwrap_or(0);

            // Dispatch up to PIPELINE_CHUNKS tasks total.
            let to_dispatch = PIPELINE_CHUNKS.saturating_sub(in_flight_count).min(pending);
            if to_dispatch == 0 {
                // Pipeline is full — wait a bit for a slot to complete.
                tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
                continue;
            }

            let mut handles = Vec::new();
            for _ in 0..to_dispatch {
                let handle = self.clone();
                handles.push(tokio::spawn(async move {
                    handle.flush_one_chunk(ino).await
                }));
            }

            for h in handles {
                match h.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => { if first_err.is_none() { first_err = Some(e); } }
                    Err(e) => { if first_err.is_none() {
                        first_err = Some(anyhow::anyhow!("flush task panicked: {}", e));
                    }}
                }
            }

            if first_err.is_some() { break; }
        }

        if let Some(e) = first_err { return Err(e); }

        // Final metadata sync — flush_one_chunk enqueues to the metadata queue,
        // but release/fsync need a synchronous commit so the file survives restart.
        let meta_to_persist = self.metadata_cache.get(&ino).map(|m| m.clone());
        if let Some(meta) = meta_to_persist {
            self.client.flush_metadata_sync(&meta).await;
            self.last_metadata_update.insert(ino, std::time::Instant::now());
        }

        // Slots are removed immediately on flush success, so nothing to clean up here.
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

    /// Tokio runtime handle for async operations (writes, metadata, misc FUSE ops)
    runtime: tokio::runtime::Handle,

    /// Dedicated runtime for read operations — isolated from writes so a burst
    /// of slow reads (network cache misses) can't starve write reply tasks and
    /// vice versa, preventing FUSE request deadlocks under concurrent I/O.
    read_runtime: Arc<tokio::runtime::Runtime>,

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

    /// Shared reference to the background flusher's in-flight count map.
    /// Set by the background flusher task after spawn; flush_buffer_async (fsync/close)
    /// waits until the count reaches zero before sending its own flush
    /// to avoid concurrent flushes that would produce OffsetMismatch.
    flush_in_flight: Arc<RwLock<Option<Arc<DashMap<u64, usize>>>>>,

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

    /// Per-inode count of release() flush tasks still in flight.
    /// Sequential writes wait for this to reach zero for the specific inode
    /// before opening, ensuring the previous release() has fully committed.
    /// destroy() waits for all to reach zero before exiting.
    release_in_flight: Arc<DashMap<u64, Arc<std::sync::atomic::AtomicUsize>>>,

    /// Per-inode count of write() tasks still running (spawned but not yet written into the slot).
    /// release() waits for this to reach zero before flush so we don't flush an incomplete slot.
    write_tasks_in_flight: Arc<DashMap<u64, Arc<std::sync::atomic::AtomicUsize>>>,


    /// Total bytes currently held across all per-inode write buffers.
    /// Incremented on every buffered write; decremented by flush_buffer_async on success.
    /// Shared with FlushHandle so both sides see the same counter.
    global_buffered_bytes: Arc<std::sync::atomic::AtomicUsize>,

    /// Notification channel to wake up flush workers immediately when chunks become full.
    /// Eliminates the 0-50ms polling delay from the ticker-based approach.
    /// Shared with FlushHandle so write operations can trigger immediate flushing.
    flush_notify: Arc<tokio::sync::Notify>,

    /// Hard cap on total write buffer bytes (~30% of available RAM, min 64MB).
    /// The write task delays reply.written() while this is exceeded, throttling the kernel.
    global_write_buffer_cap: usize,
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

        // Global write buffer cap: BUFFER_CHUNKS × CHUNK_SIZE.
        // The writer may buffer up to BUFFER_CHUNKS full slots per inode before blocking.
        // With PIPELINE_CHUNKS=2 flushing concurrently, BUFFER_CHUNKS=4 slots in the buffer,
        // the writer always has room to fill the next slot while prior ones are in-flight.
        let global_write_buffer_cap = BUFFER_CHUNKS * CHUNK_SIZE;
        info!("Global write buffer cap: {}MB ({} buffer chunks × {}MB)",
              global_write_buffer_cap / (1024 * 1024), BUFFER_CHUNKS, CHUNK_SIZE / (1024 * 1024));

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
        let flush_in_flight_shared: Arc<RwLock<Option<Arc<DashMap<u64, usize>>>>> =
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

        // Dedicated runtime for FUSE read operations.
        // Isolated from writes so a burst of slow reads (network cache misses, ~40ms
        // each) can't fill the write runtime and cause write reply tasks to queue up,
        // which would deadlock QEMU waiting for write acknowledgements.
        let read_runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(32)
                .enable_all()
                .thread_name("dfs-read")
                .build()
                .expect("Failed to build read runtime")
        );

        let global_buffered_bytes: Arc<std::sync::atomic::AtomicUsize> =
            Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // Notification channel for immediate flush triggering when chunks become full
        let flush_notify: Arc<tokio::sync::Notify> = Arc::new(tokio::sync::Notify::new());

        // Shared write_tasks_in_flight — used by flush_one_chunk to wait for in-flight
        // write() tasks before snapshotting a full slot.
        let write_tasks_in_flight_shared: Arc<DashMap<u64, Arc<std::sync::atomic::AtomicUsize>>> = Arc::new(DashMap::new());

        // Start background task to flush expired write buffers (if buffering enabled)
        if write_buffer_enabled {
            let write_buffers_clone = write_buffers_for_cleanup.clone();
            let client_for_cleanup = client.clone();
            let metadata_cache_for_cleanup = metadata_cache.clone();
            let write_open_counts_for_bg = write_open_counts.clone();
            let path_to_inode_for_bg = path_to_inode.clone();
            // in_flight: per-inode count of chunk-flush tasks currently running.
            // The ticker keeps this at most PIPELINE_CHUNKS per inode, so each
            // flush task handles exactly one chunk and decrements on completion.
            // flush_buffer_async (fsync/close) waits for the count to reach zero.
            let in_flight: Arc<DashMap<u64, usize>> = Arc::new(DashMap::new());
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
                global_buffered_bytes: global_buffered_bytes.clone(),
                flush_notify: flush_notify.clone(),
                write_tasks_in_flight: write_tasks_in_flight_shared.clone(),
            };
            runtime.spawn(async move {
                let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(50));
                let notify = flush_handle_for_bg.flush_notify.clone();
                loop {
                    // Dual-mode wake: immediate notification when chunks become full (fast path)
                    // or periodic tick for idle chunks (slow path). This eliminates the 0-50ms
                    // polling delay for full chunks while still handling idle/partial chunks.
                    tokio::select! {
                        _ = notify.notified() => {
                            tracing::debug!("Flush triggered by notification (chunk full)");
                        }
                        _ = interval.tick() => {
                            tracing::debug!("Flush triggered by ticker (periodic check)");
                        }
                    }

                    // For each inode: if it has full slots and fewer than PIPELINE_CHUNKS
                    // tasks in-flight, dispatch one more chunk-flush task.  Each task flushes
                    // exactly one chunk (max_chunks=1) and decrements the counter on completion.
                    // This gives continuous single-chunk pipelining: as soon as one chunk
                    // finishes, the slot is freed (unblocking the writer) and the next tick
                    // can dispatch a replacement — no need to wait for the whole batch.
                    let inodes: Vec<u64> = write_buffers_clone.iter()
                        .map(|e| *e.key())
                        .collect();

                    for ino in inodes {
                        let current_in_flight = in_flight.get(&ino).map(|v| *v).unwrap_or(0);
                        if current_in_flight >= PIPELINE_CHUNKS { continue; }

                        let state_arc = match write_buffers_clone.get(&ino) {
                            Some(a) => a.clone(),
                            None => continue,
                        };
                        let state = state_arc.lock().await;
                        let has_full = !state.full_slot_indices().is_empty();
                        let no_active_writers = write_open_counts_for_bg
                            .get(&ino).map(|c| *c == 0).unwrap_or(true);
                        let has_idle = no_active_writers && state.slots.iter().any(|(_, s)| {
                            s.is_idle() && !s.data.is_empty() && !s.flushing
                        });
                        drop(state);
                        if !has_full && !has_idle { continue; }

                        // Increment before spawning to prevent a second dispatch racing
                        // in the same tick before the task starts.
                        *in_flight.entry(ino).or_insert(0) += 1;

                        let handle = flush_handle_for_bg.clone();
                        let in_flight_task = in_flight.clone();
                        let flush_rt = handle.flush_runtime.clone();

                        flush_rt.spawn(async move {
                            // Flush one chunk, then keep looping as long as:
                            //   - more full slots exist for this inode, AND
                            //   - we are the only task holding the in_flight slot
                            //     (i.e. we haven't been displaced by a concurrent task).
                            // This self-refilling loop is what keeps the pipeline truly full:
                            // each task fills its own pipeline slot back-to-back without
                            // waiting up to 100ms for the ticker to notice the vacancy.
                            loop {
                                if let Err(e) = handle.flush_one_chunk(ino).await {
                                    tracing::error!("Background flush failed for inode {}: {}", ino, e);
                                    break;
                                }
                                // Check whether more full slots remain.
                                let has_more = handle.write_buffers.get(&ino).map(|s| {
                                    s.try_lock().map(|st| !st.full_slot_indices().is_empty()).unwrap_or(false)
                                }).unwrap_or(false);
                                if !has_more { break; }
                                // Only continue if there's a spare pipeline slot for us.
                                // If another task already filled it (ticker dispatched a sibling),
                                // exit so we don't over-subscribe.
                                let current = in_flight_task.get(&ino).map(|v| *v).unwrap_or(0);
                                if current > PIPELINE_CHUNKS { break; }
                            }
                            // Decrement; remove entry when it reaches zero.
                            let mut entry = in_flight_task.entry(ino).or_insert(0);
                            if *entry > 0 { *entry -= 1; }
                            if *entry == 0 { drop(entry); in_flight_task.remove(&ino); }
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
            global_buffered_bytes: global_buffered_bytes.clone(),
            flush_notify: flush_notify.clone(),
            write_tasks_in_flight: write_tasks_in_flight_shared.clone(),
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
            read_runtime,
            flush_handle,
            pending_deletes: Arc::new(dashmap::DashSet::new()),
            refreshing_inodes: Arc::new(dashmap::DashSet::new()),
            release_in_flight: Arc::new(DashMap::new()),
            write_tasks_in_flight: write_tasks_in_flight_shared,
            global_buffered_bytes,
            flush_notify,
            global_write_buffer_cap,
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

    /// Flush all dirty chunks for `ino` using a pipelined approach capped at PIPELINE_CHUNKS.
    async fn flush_all_pipelined(&self, ino: u64) -> Result<()> {
        self.flush_handle.flush_all_pipelined(ino).await
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

        // Allow generous kernel readahead so dd-style sequential workloads can issue
        // many parallel FUSE reads against the same chunk.  The userspace path
        // deduplicates concurrent fetches via chunk_cache + in_flight, and waiters
        // wake on a Notify the moment the chunk lands — so extra FUSE reads
        // coalesce on a single network fetch instead of racing the pipeline.
        // The kernel only fires readahead on sequential patterns, so random-read
        // workloads (SQLite, seek-heavy) are unaffected.
        let _ = config.set_max_readahead(4 * 1024 * 1024);
        // Raise max_background so reads are never starved by concurrent release/write ops.
        // Default is 16 with congestion threshold at 12; under heavy write load (4 releases +
        // 2 pipeline writes in-flight) the kernel stops dispatching reads. 64 gives reads
        // headroom without risking memory pressure (each slot is just a request descriptor).
        let _ = config.set_max_background(64);
        let _ = config.set_congestion_threshold(48);

        // Warm metadata and directory caches from the leader on startup so that the
        // first ls/find/DVR index scan sees all files immediately without round-trips.
        // Run as a background task — block_on here would stall the FUSE dispatch thread
        // on staging with many files, hanging the first write until warm-up completes.
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
                let mut dir_entries: std::collections::HashMap<String, Vec<dfs_common::FileMetadata>> =
                    std::collections::HashMap::new();
                for file in files {
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
                    // Preserve any larger in-memory size (e.g. truncate-grown sparse file)
                    // so that a stale server record doesn't clobber the logical size.
                    let mut file = file;
                    if let Some(cached) = metadata_cache.get(&ino) {
                        if cached.size > file.size {
                            file.size = cached.size;
                        }
                    }
                    metadata_cache.insert(ino, file.clone());
                    last_metadata_update.insert(ino, now);

                    if let Some(slash) = file.path.rfind('/') {
                        let parent = if slash == 0 { "/".to_string() } else { file.path[..slash].to_string() };
                        dir_entries.entry(parent).or_default().push(file);
                    }
                }
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
            global_buffered_bytes: self.global_buffered_bytes.clone(),
            flush_notify: self.flush_notify.clone(),
            write_tasks_in_flight: self.write_tasks_in_flight.clone(),
        };

        self.block_on(async move {
            // Step 0: Wait for any in-flight release() flush tasks to complete.
            // release() spawns async tasks that aren't tracked by flush_in_flight.
            // Without this wait, a release flush that started just before unmount
            // may be interrupted mid-write, losing the final metadata commit.
            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
            loop {
                let total: usize = release_in_flight.iter()
                    .map(|entry| entry.value().load(std::sync::atomic::Ordering::Relaxed))
                    .sum();
                if total == 0 { break; }
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
        let release_count = self.release_in_flight.get(&ino)
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0);
        info!("open: ino={} flags=0x{:x} release_in_flight={} write_tasks_in_flight={:?}",
              ino, flags, release_count,
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
        let is_trunc = (flags & libc::O_TRUNC) != 0;
        if is_write {
            *self.write_open_counts.entry(ino).or_insert(0) += 1;

            // Seed the write_seq counter from the server's stored value so that
            // writes after a client restart continue from the correct sequence,
            // not from 0 (which would be treated as stale by the server).
            if let Some(meta) = self.metadata_cache.get(&ino) {
                self.client.seed_write_seq(meta.id, meta.write_seq);
            }

            // Refresh metadata from the leader so chunk locations reflect any healer
            // rebalancing since the last fetch. Without this, a write open on a long-lived
            // file (e.g. a multi-hour DVR recording) uses chunk locations from hours ago,
            // causing PatchChunk to target nodes that no longer hold the chunk.
            // Skip for first-writer opens: the writer is about to replace file content, so
            // stale chunk locations are irrelevant.  A background refresh that completes
            // after the write session flushes will overwrite the freshly-committed metadata
            // with old server data, causing the file to revert to its pre-write size.
            // For subsequent writers on the same inode (write_open_count > 1), the metadata
            // is already fresh from the first writer's open, so skip there too.
            // Mark inode as write-open so reads bypass the chunk cache for this session.
            // Synchronous — happens before open() returns, so the app's first read
            // after open always fetches fresh data from the server.
            self.client.write_open_inodes.insert(ino);

            let is_first_writer = self.write_open_counts.get(&ino).map(|c| *c == 1).unwrap_or(true);
            if !is_first_writer {
                let path_opt = self.path_to_inode.read().unwrap()
                    .iter().find(|(_, &v)| v == ino).map(|(k, _)| k.clone());
                if let Some(path) = path_opt {
                    let client = self.client.clone();
                    let metadata_cache = self.metadata_cache.clone();
                    self.runtime.spawn(async move {
                        if let Ok(Some(fresh)) = client.get_file_metadata(&path).await {
                            client.seed_write_seq(fresh.id, fresh.write_seq);
                            metadata_cache.insert(ino, fresh);
                        }
                    });
                }
            }

            // O_SYNC / O_DSYNC: the caller wants every fsync() to be honored immediately.
            // SQLite, databases, and write-journaling apps use this. DVR/streaming apps don't.
            // We propagate this flag into the InodeWriteState so fsync() knows whether to
            // coalesce (DVR mode) or flush immediately (database mode).
            // Also force sync_on_fsync for SQLite files regardless of open flags — SQLite
            // doesn't set O_SYNC but its fdatasync() calls must flush immediately so that
            // WAL checkpoint reads see the committed data.
            let path_for_sync_check = self.metadata_cache.get(&ino).map(|m| m.path.clone());
            let is_sqlite_buf = path_for_sync_check.as_deref().map(is_sqlite_buffered).unwrap_or(false);
            let sync_on_fsync = (flags & (libc::O_SYNC | libc::O_DSYNC)) != 0 || is_sqlite_buf;
            if sync_on_fsync {
                info!("open: ino={} sync_on_fsync=true (O_SYNC/O_DSYNC or SQLite)", ino);
            }
            if self.write_buffer_enabled {
                // If this is the first writer (count was 0 before incrementing above),
                // discard any stale buffer left over from a previous session — e.g. after
                // a client restart where release() never ran. Without this, the background
                // flusher immediately flushes the stale data as a small first chunk.
                if is_first_writer {
                    // For O_TRUNC opens, clear metadata cache and read engine so the file
                    // starts fresh. The general open() wait for write_buffers (lines 2265-2275)
                    // already ensures any previous write session has completed.
                    if is_trunc {
                        // Remove entire metadata cache entry (not just clear fields).
                        // Removing forces fresh metadata fetch with new chunk locations.
                        self.metadata_cache.remove(&ino);

                        // Invalidate read engine so subsequent reads get new file content.
                        if let Some(engine) = self.client.read_engines.get(ino) {
                            let engine_clone = engine.clone();
                            self.runtime.block_on(async move {
                                engine_clone.expire_chunk_map_async().await;
                            });
                        }
                    }
                    // Only remove the write buffer if it has no unflushed data.
                    // If a slot with real data exists (flushing=false, non-empty), a
                    // concurrent flush task wrote data that hasn't been sent to the server yet.
                    // Removing the buffer here would silently discard that data.
                    // If flushing=true, a flush is in progress — leave it; that task will
                    // clean up the slot after the network call completes.
                    let safe_to_remove = if let Some(state_arc) = self.write_buffers.get(&ino) {
                        if let Ok(st) = state_arc.try_lock() {
                            let has_unflushed = st.slots.values().any(|s| !s.data.is_empty() && !s.flushing);
                            if has_unflushed {
                                info!("open: ino={} is_first_writer — NOT removing write_buffers, has unflushed data", ino);
                            }
                            !has_unflushed
                        } else {
                            // Lock held by flush task — don't remove
                            false
                        }
                    } else {
                        true // doesn't exist, nothing to remove
                    };
                    if safe_to_remove {
                        self.write_buffers.remove(&ino);
                    }
                    // Clear any pending truncate flag — new write session starts clean.
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
                if is_trunc {
                    if let Ok(mut st) = state_entry.try_lock() { st.is_truncated_session = true; }
                }
                // Snapshot chunk IDs at open time for stale-write detection.
                // If another session patches chunk N between our open() and flush(),
                // the chunk_id in metadata_cache will no longer match chunk_ids_at_open[N]
                // and we skip the PatchChunk rather than reverting the newer write.
                if let Some(meta) = self.metadata_cache.get(&ino) {
                    if let Ok(mut st) = state_entry.try_lock() {
                        for loc in &meta.chunk_locations {
                            if let Some(offset) = loc.file_offset {
                                let idx = offset / CHUNK_SIZE as u64;
                                let prev = st.chunk_ids_at_open.entry(idx).or_insert(loc.chunk_id);
                                info!("open: ino={} chunk_ids_at_open[{}] = {} (prev={})", ino, idx, loc.chunk_id, prev);
                            }
                        }
                    }
                }
            }
        }

        // Check if this is a SQLite database file by looking up its path
        let is_sqlite = self.metadata_cache.get(&ino)
            .map(|m| is_sqlite_direct_io(&m.path))
            .unwrap_or(false);

        // For read-only opens of finished files, fetch the full chunk map synchronously
        // before returning the file handle. This guarantees the map covers the entire
        // file from byte 0 so any seek position works immediately without a second
        // round-trip. Cost is one leader RPC (~5ms).
        // Live recordings (has an active writer) use the async path — their chunk map
        // is changing continuously and blocking open() on them would deadlock with
        // in-flight write tasks on the same runtime.
        let is_read_open = (flags & libc::O_ACCMODE) == libc::O_RDONLY;
        let has_active_writer = self.write_open_counts.get(&ino).map(|v| *v > 0).unwrap_or(false);
        let has_inflight_flush = self.flush_in_flight.read().unwrap()
            .as_ref().map(|m| m.get(&ino).map(|v| *v > 0).unwrap_or(false)).unwrap_or(false);
        // CRITICAL: Wait for ALL in-flight release() tasks to complete before proceeding.
        // release() increments release_in_flight before reply.ok(), so a counter > 0 means
        // a prior session's flush and metadata commit is still running. We must wait for it
        // so metadata_cache reflects the prior session's chunks before any subsequent open
        // (read or write) fetches the chunk map. Without this, a read-open immediately
        // after a write session (e.g. `cp` right after a DVR stream write) may get a stale
        // chunk map that only has the first chunk committed, causing all later chunks to
        // read back as zeros.
        // We check release_in_flight only (not write_buffers) because when a new writer
        // registers, write_buffers is intentionally kept alive and would deadlock the wait.
        let had_inflight = !has_active_writer &&
            self.release_in_flight.get(&ino).map(|c| c.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0) > 0;
        if had_inflight {
            let release_in_flight = self.release_in_flight.clone();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            tokio::task::block_in_place(|| {
                self.runtime.block_on(async {
                    while release_in_flight.get(&ino).map(|c| c.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0) > 0 {
                        if std::time::Instant::now() >= deadline {
                            warn!("open: timed out waiting for release_in_flight on ino={}", ino);
                            break;
                        }
                        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
                    }
                });
            });
        }
        if is_read_open && !has_active_writer && !has_inflight_flush {
            if let Some(meta) = self.metadata_cache.get(&ino) {
                let file_id = meta.id;
                let file_size = meta.size;
                drop(meta);
                let client = self.client.clone();
                let engine = client.read_engines.get_or_create(ino);
                engine.expire_chunk_map();
                if engine.refresh_in_progress
                    .compare_exchange(false, true,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Relaxed).is_ok()
                {
                    // Fire-and-forget: the read path handles a stale/empty engine gracefully
                    // by refreshing on first read. Blocking here stalls the FUSE dispatch
                    // thread and can deadlock under concurrent opens (e.g. kdiskmark prep).
                    self.runtime.spawn(async move {
                        client.refresh_engine_flagged(&engine, file_id, file_size, 0).await;
                    });
                }
            }
        } else if is_read_open {
            // Live recording — kick off a full chunk map fetch from chunk 0 in the
            // background so backward seeks have the complete history available.
            if let Some(meta) = self.metadata_cache.get(&ino) {
                let file_id = meta.id;
                let file_size = meta.size;
                drop(meta);
                let client = self.client.clone();
                let engine = client.read_engines.get_or_create(ino);
                engine.expire_chunk_map();
                if engine.refresh_in_progress
                    .compare_exchange(false, true,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Relaxed).is_ok()
                {
                    self.runtime.spawn(async move {
                        client.refresh_engine_flagged(&engine, file_id, file_size, 0).await;
                    });
                }
            }
        }

        // Use direct I/O for SQLite database files (required for correctness).
        // With KEEP_CACHE the kernel page cache fills gaps with zeros when FUSE returns
        // a short/empty read — e.g. when the write buffer doesn't have the data yet.
        // Direct I/O bypasses the cache so short reads are passed through as-is without
        // being cached as zeros.
        // Application-level chunk cache (Moka) is more efficient than kernel page cache
        // for our 4MB chunk workload: 56 MB/s (direct) vs 27 MB/s (page cache).
        //
        // EXCEPTION: SQLite .db-shm files need kernel page cache enabled to support mmap.
        // The .db-shm file is SQLite's shared memory coordination file for WAL mode.
        // It must be mmap'd with MAP_SHARED for inter-process coordination. If we use
        // FOPEN_DIRECT_IO, mmap fails with ENODEV and SQLite falls back to sparse writes
        // at high offsets (e.g., 309GB), causing allocation errors.
        if is_sqlite {
            info!("open: ino={} - SQLite database detected, using direct I/O", ino);
            reply.opened(0, fuser::consts::FOPEN_DIRECT_IO);
        } else {
            // .db-shm and other files use kernel page cache to enable mmap support
            reply.opened(0, 0);
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

        self.read_runtime.spawn(async move {
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

            // Extend file_size to include buffered-but-uncommitted bytes so the EOF
            // check below doesn't gate out reads that are within the write buffer.
            // Use blocking lock (not try_lock) to wait for flush completion, ensuring
            // we see the final buffer state before falling through to network.
            let file_size = if write_buffer_enabled {
                if let Some(state_arc) = write_buffers.get(&ino).map(|r| r.clone()) {
                    // Use a timeout on the lock so a slow flush (e.g. node failure causing
                    // TCP timeout) doesn't freeze all reads at the live edge for 17+ seconds.
                    let buffered_end = match tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        state_arc.lock(),
                    ).await {
                        Ok(state) => state.slots.iter()
                            .map(|(idx, slot)| idx * CHUNK_SIZE as u64 + slot.data.len() as u64)
                            .max().unwrap_or(0),
                        Err(_) => 0, // flush taking too long — fall through to network
                    };
                    file_size.max(buffered_end)
                } else { file_size }
            } else { file_size };

            // --- Write buffer: serve uncommitted data without a network round-trip. ---
            // Slots are present only while data is uncommitted (removed immediately on flush).
            // Slot exists → serve from buffer (uncommitted live data).
            // Slot absent → fall through to network (committed data on server).
            // This prevents 0-byte returns during the pre-commit window that cause
            // concurrent dd readers to get offset-shifted data in READ_COPY.
            // Use a 500ms timeout on the lock so a slow flush (node failure, TCP timeout)
            // doesn't freeze reads at the live edge. If we can't acquire within 500ms,
            // fall through to the network — slightly stale chunk_id risk is acceptable
            // compared to a 17-second player freeze.
            if write_buffer_enabled {
                if let Some(state_arc) = write_buffers.get(&ino).map(|r| r.clone()) {
                    let lock_result = tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        state_arc.lock(),
                    ).await;
                    if lock_result.is_err() {
                        // Flush is stuck (likely slow node) — fall through to network
                        let result = client.read_file(
                            ino, file_size, file_id, &file_path, offset, size, false,
                        ).await;
                        let elapsed = start.elapsed();
                        match result {
                            Ok(data) => {
                                let reply_data = if data.len() > size { &data[..size] } else { &data[..] };
                                info!("FUSE read done: ino={}, {} bytes in {:?}", ino, reply_data.len(), elapsed);
                                reply.data(reply_data);
                            }
                            Err(e) => { error!("read failed: {}", e); reply.error(libc::EIO); }
                        }
                        return;
                    }
                    let state = lock_result.unwrap();
                    let chunk_idx = InodeWriteState::chunk_index(offset as u64);
                    let intra = InodeWriteState::intra_offset(offset as u64);
                    if let Some(slot) = state.slots.get(&chunk_idx) {
                        // Slot present — data is buffered and not yet fully committed.
                        // The gap_filled_prefix range (0..gap_filled_prefix) is a synthetic
                        // zero-fill for bytes already on the server from a prior partial
                        // flush. The real new data starts at gap_filled_prefix.
                        // For reads within 0..gap_filled_prefix: the server has this data,
                        // fall through to the network.
                        // For reads within gap_filled_prefix..data.len(): serve from buffer.
                        // For reads at or beyond data.len(): write edge, return empty.
                        if intra >= slot.data.len() {
                            // Beyond this slot's buffered frontier. The server may have
                            // committed data here from a prior flush (e.g. mkfs.ext4 writes
                            // non-sequentially within a chunk). Fall through to the network
                            // unless we're past the committed metadata size (true live edge).
                            let committed_size = metadata_cache.get(&ino).map(|m| m.size as usize).unwrap_or(0);
                            if offset >= committed_size {
                                reply.data(&[]);
                                return;
                            }
                            // Fall through to network — server has data here.
                        } else if intra >= slot.gap_filled_prefix {
                            // Real buffered data — serve it.
                            let avail = slot.data.len() - intra;
                            let n = avail.min(size);
                            reply.data(&slot.data[intra..intra + n]);
                            return;
                        }
                        // intra < gap_filled_prefix: server has this range, fall through.
                    } else {
                        // No slot — check if we're at the live write edge.
                        let committed_size = metadata_cache.get(&ino).map(|m| m.size as usize).unwrap_or(0);
                        if !state.slots.is_empty() && offset >= committed_size {
                            reply.data(&[]);
                            return;
                        }
                        // Fall through to network for committed data.
                    }
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
                    Ok(Some(mut fresh)) => {
                        // Never let a stale server size shrink the cached logical size.
                        if let Some(cached) = metadata_cache.get(&ino) {
                            if cached.size > fresh.size { fresh.size = cached.size; }
                        }
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
            // True only when there are dirty (unflushed) slots in the write buffer.
            // A stale empty-buffer key must not suppress the synchronous chunk map refresh —
            // that would cause reads on a just-closed file to return empty instead of data.
            // True only when there are dirty (unflushed) slots in the write buffer.
            // A stale empty-buffer key must not suppress the synchronous chunk map refresh —
            // that would cause reads on a just-closed file to return empty instead of data.
            let has_active_writer = write_buffer_enabled && {
                let has_slots = write_buffers.get(&ino)
                    .and_then(|arc| arc.try_lock().ok().map(|s| !s.slots.is_empty()))
                    .unwrap_or(false);
                has_slots
            };
            info!("FUSE read: ino={}, offset={}, size={}, file_size={}", ino, offset, size, effective_size);
            let result = client.read_file(
                ino, effective_size, file_id, &file_path, offset, size, has_active_writer,
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
                    // Use direct I/O for SQLite database files, but NOT for .db-shm (needs mmap)
                    let is_sqlite = is_sqlite_direct_io(&path);
                    if is_sqlite {
                        reply.created(&Duration::ZERO, &attr, 0, 0, fuser::consts::FOPEN_DIRECT_IO);
                    } else {
                        reply.created(&Duration::ZERO, &attr, 0, 0, 0);
                    }
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
        let global_buffered_bytes = self.global_buffered_bytes.clone();
        let global_write_buffer_cap = self.global_write_buffer_cap;
        let data_vec = data.to_vec();
        let req_uid = _req.uid();
        let req_gid = _req.gid();

        let write_task_counter = self.write_tasks_in_flight
            .entry(ino)
            .or_insert_with(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
            .clone();
        write_task_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Fast path: if metadata is cached, buffer is not full, and this is a
        // sequential (non-sparse) buffered write, handle it synchronously on the
        // FUSE dispatch thread via block_in_place. This avoids spawning a runtime
        // task entirely — critical because all 8 runtime threads can be occupied
        // by concurrent reads, causing write tasks to queue up and QEMU to deadlock
        // waiting for a reply that never comes.
        let path_for_sqlite_check = path_to_inode.read().unwrap()
            .iter().find(|(_, &v)| v == ino).map(|(k, _)| k.clone());
        // .db-shm is mmap'd MAP_SHARED — keep it on the unbuffered path.
        // All other SQLite files (.db, .db-wal, .db-journal, etc.) now go through
        // the write buffer with sync_on_fsync=true, giving coherent chunk accumulation
        // and correct PatchChunk behaviour on fdatasync.
        let is_shm_only = path_for_sqlite_check.as_deref().map(|p| p.ends_with(".db-shm")).unwrap_or(false);
        if write_buffer_enabled && !is_shm_only {
            if let Some(meta) = metadata_cache.get(&ino) {
                if meta.file_type == FileType::RegularFile {
                    let offset_usize = offset as usize;
                    let cache_size = meta.size as usize;
                    drop(meta);
                    let hwm = size_high_water.get(&ino).map(|v| *v as usize).unwrap_or(0);
                    let current_size = hwm.max(cache_size);
                    let is_sequential = offset_usize <= current_size;
                    if is_sequential {
                        if !write_buffers.contains_key(&ino) {
                            let sync = path_for_sqlite_check.as_deref().map(is_sqlite_buffered).unwrap_or(false);
                            write_buffers.insert(ino, Arc::new(Mutex::new(InodeWriteState::new(sync))));
                        }
                        let state_arc = write_buffers.get(&ino).map(|e| e.clone());
                        if let Some(state_arc) = state_arc {
                        // Use blocking_lock() — if the flush thread holds the mutex we wait
                        // on the FUSE dispatch thread rather than falling through to spawn an
                        // async task. Blocking the FUSE thread here is correct and desirable:
                        // it throttles the writer naturally without consuming a runtime slot.
                        //
                        // CRITICAL: apply back-pressure BEFORE acquiring the lock.
                        // Holding the slot mutex while spinning would prevent the flush task
                        // from acquiring it to drain the buffer — permanent deadlock.
                        {
                            let t_bp = std::time::Instant::now();
                            const BP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
                            loop {
                                // Use per-inode buffered bytes. If the lock is held by the
                                // flush task, fall back to global_buffered_bytes (not cap) —
                                // returning cap would cause 10ms sleeps during every flush,
                                // turning a 130ms flush into 260ms and halving throughput.
                                let current = state_arc.try_lock()
                                    .map(|s| s.buffered_bytes())
                                    .unwrap_or_else(|_| global_buffered_bytes.load(std::sync::atomic::Ordering::Relaxed));
                                let fill_pct = current * 100 / global_write_buffer_cap.max(1);
                                let delay_ms: u64 = if fill_pct < 25 { 0 }
                                    else if fill_pct < 50 { 1 }
                                    else if fill_pct < 75 { 5 }
                                    else if fill_pct < 100 { 20 }
                                    else {
                                        if t_bp.elapsed() >= BP_TIMEOUT {
                                            error!("write fast-path: ino={} bp timeout — EIO (global_buffered={}  cap={})", ino, current, global_write_buffer_cap);
                                            write_task_counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                                            reply.error(libc::EIO);
                                            return;
                                        }
                                        10
                                    };
                                if delay_ms == 0 { break; }
                                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                                if fill_pct < 100 { break; }
                            }
                        }
                        let mut state = state_arc.blocking_lock();
                        {
                            let bytes_before = state.buffered_bytes();
                            state.write_at(offset as u64, &data_vec);
                            let bytes_after = state.buffered_bytes();
                            let has_full = !state.full_slot_indices().is_empty();
                            drop(state);
                            let new_end = (offset as u64) + data_vec.len() as u64;
                            {
                                let mut hwm = size_high_water.entry(ino).or_insert(0);
                                if new_end > *hwm { *hwm = new_end; }
                            }
                            // Only count bytes actually added to the buffer, not the write
                            // size. Overlapping writes don't grow the slot, so adding
                            // data_vec.len() unconditionally causes the counter to drift up
                            // and never come back down, triggering false back-pressure.
                            let added = bytes_after.saturating_sub(bytes_before);
                            if added > 0 {
                                global_buffered_bytes.fetch_add(added, std::sync::atomic::Ordering::Relaxed);
                            }
                            {
                                let mut counters = write_counters.write().unwrap();
                                *counters.entry(ino).or_insert(0) += 1;
                            }
                            if has_full {
                                flush_handle.flush_notify.notify_one();
                            }
                            write_task_counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                            debug!("write fast-path: ino={} off={} len={}", ino, offset, data_vec.len());
                            reply.written(data_vec.len() as u32);
                            return;
                        } // blocking_lock scope
                        } // if let Some(state_arc)
                    }
                } else {
                    drop(meta);
                }
            }
        }

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
            let is_sqlite_buf = is_sqlite_buffered(&metadata.path);
            let cache_inode = if is_sqlite { 0 } else { ino };
            // .db-shm stays unbuffered (mmap'd MAP_SHARED); all other SQLite files use the
            // write buffer with sync_on_fsync=true for coherent chunk accumulation.
            let is_shm = metadata.path.ends_with(".db-shm");

            if write_buffer_enabled && !is_shm {
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
                            .or_insert_with(|| Arc::new(Mutex::new(InodeWriteState::new(is_sqlite_buf))))
                            .clone();
                        let mut state = state_arc.lock().await;
                        let bytes_before = state.buffered_bytes();
                        state.write_at(gap_write_offset, &padded);
                        // Mark the gap bytes as synthetic so flush doesn't mistake them
                        // for real app data when deciding whether to PatchChunk.
                        let gap_chunk_idx = InodeWriteState::chunk_index(gap_write_offset);
                        let gap_intra = InodeWriteState::intra_offset(gap_write_offset);
                        if let Some(slot) = state.slots.get_mut(&gap_chunk_idx) {
                            slot.gap_filled_prefix = gap_intra + gap;
                        }
                        let added = state.buffered_bytes().saturating_sub(bytes_before);

                        // Notify flush worker if chunks are now full (event-driven flush)
                        let has_full_chunks = !state.full_slot_indices().is_empty();
                        drop(state);
                        if added > 0 {
                            global_buffered_bytes.fetch_add(added, std::sync::atomic::Ordering::Relaxed);
                        }

                        if has_full_chunks {
                            flush_handle.flush_notify.notify_one();
                        }

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
                        .and_then(|m| m.chunk_location_for_idx(target_chunk_idx).cloned());

                    if let Some(old_loc) = existing_loc {
                        // Target offset is within an existing chunk — patch it in-place.
                        info!("Sparse write at offset={} lands in existing chunk {} (size={}) — using PatchChunk",
                              offset_usize, target_chunk_idx, old_loc.size);
                        match client.patch_chunk_on_replicas(
                            old_loc.chunk_id, offset as u64, target_intra, data_vec.clone(), &old_loc,
                        ).await {
                            Ok(new_loc) => {
                                let mut meta = meta_snap.unwrap();
                                let new_size = (offset_usize + data_vec.len()).max(current_size).max(meta.size as usize);
                                if let Some(loc) = meta.chunk_location_for_idx_mut(target_chunk_idx) {
                                    *loc = new_loc;
                                }
                                meta.size = new_size as u64;
                                meta.modified_at = SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                                client.enqueue_metadata(&meta).await;
                                // Update read engine immediately for SQLite read-after-write consistency
                                client.feed_chunk_locations_to_read_engine(
                                    ino, &meta.chunk_locations, meta.size,
                                ).await;
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
                                let mut metadata = meta_snap.unwrap_or_else(|| metadata.clone());
                                let new_size = (offset_usize + data_vec.len()).max(current_size).max(metadata.size as usize);
                                if let Some(chunk_locations) = chunk_locations_opt {
                                    metadata.chunk_locations.extend(chunk_locations);
                                }
                                metadata.size = new_size as u64;
                                metadata.modified_at = SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                                client.enqueue_metadata(&metadata).await;
                                // Update read engine immediately for SQLite read-after-write consistency
                                client.feed_chunk_locations_to_read_engine(
                                    ino, &metadata.chunk_locations, metadata.size,
                                ).await;
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

                // BUFFERED WRITE — wait for room before buffering, then reply.
                // Backpressure is per-inode: each inode gets its own 2×pipeline budget so
                // concurrent writers (e.g. multiple DVR recordings) don't compete for a
                // shared cap.  We check before writing — acquire the lock, peek the count,
                // drop, sleep if over cap, then re-acquire to write.  Delaying reply.written()
                // blocks the kernel from issuing the next write(), which is the throttle.
                // The write task runs on self.runtime (separate from flush_runtime), so
                // sleeping here does not starve the flush workers.
                {
                    let write_offset = offset as u64;

                    let state_arc = write_buffers
                        .entry(ino)
                        .or_insert_with(|| Arc::new(Mutex::new(InodeWriteState::new(is_sqlite_buf))))
                        .clone();

                    // t_sched: time from FUSE write() call to reaching the backpressure gate
                    // (runtime scheduling + metadata cache lookup overhead).
                    let t_sched = start.elapsed();

                    // Graduated back-pressure: slow writes proportionally as the buffer
                    // fills so the pipeline never actually hits the cap under normal load.
                    // Early pressure keeps the buffer low and flush latency spikes from
                    // causing a full stall. Hard cap is still enforced with a 30s timeout
                    // as a safety net against a permanently stuck flush pipeline.
                    //
                    // CRITICAL: use try_lock() rather than lock().await to read the buffer
                    // level. Under high write concurrency all 8 runtime threads can end up
                    // waiting on state_arc.lock().await simultaneously, starving the runtime
                    // and deadlocking the flush tasks that need to run to drain the buffer.
                    // If try_lock fails (flush task holds the lock), fall back to the
                    // size_high_water mark as a conservative estimate — it's always >= real
                    // buffered bytes so back-pressure is applied safely.
                    let t_bp_start = std::time::Instant::now();
                    const BP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
                    loop {
                        let current = state_arc.try_lock()
                            .map(|s| s.buffered_bytes())
                            .unwrap_or_else(|_| global_write_buffer_cap);
                        let fill_pct = current * 100 / global_write_buffer_cap.max(1);
                        let delay_ms: u64 = if fill_pct < 25 {
                            0
                        } else if fill_pct < 50 {
                            1
                        } else if fill_pct < 75 {
                            5
                        } else if fill_pct < 100 {
                            20
                        } else {
                            // At cap — check timeout before blocking.
                            if t_bp_start.elapsed() >= BP_TIMEOUT {
                                error!("write: ino={} back-pressure timeout after {:?} — flush pipeline stuck, returning EIO",
                                       ino, t_bp_start.elapsed());
                                reply.error(libc::EIO);
                                return;
                            }
                            10
                        };
                        if delay_ms == 0 { break; }
                        tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                        if fill_pct < 100 { break; }
                    }
                    let t_bp = t_bp_start.elapsed();

                    // t_buf: time to acquire the slot lock and copy bytes into the buffer.
                    let t_buf_start = std::time::Instant::now();
                    let mut state = state_arc.lock().await;
                    let bytes_before = state.buffered_bytes();
                    state.write_at(write_offset, &data_vec);
                    let added = state.buffered_bytes().saturating_sub(bytes_before);

                    // Check if this write completed any 4MB chunks. If so, notify the flush
                    // worker immediately (event-driven) instead of waiting for the 50ms ticker.
                    let has_full_chunks = !state.full_slot_indices().is_empty();
                    drop(state);

                    if has_full_chunks {
                        flush_handle.flush_notify.notify_one();
                    }

                    if added > 0 {
                        global_buffered_bytes.fetch_add(added, std::sync::atomic::Ordering::Relaxed);
                    }
                    let t_buf = t_buf_start.elapsed();

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

                    debug!("write ino={} off={} len={} | sched={:?} bp_wait={:?} buf={:?} total={:?}",
                          ino, offset, data_vec.len(),
                          t_sched, t_bp, t_buf, start.elapsed());
                    reply.written(data_vec.len() as u32);
                    return;
                }
            }

            // Non-buffered write path (write_buffer_enabled=false, or .db-shm).
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

            // write_file_offset: the byte offset passed to write_data_with_cache.
            // For appends and gap writes this is the actual write position (not current_size).
            let mut write_file_offset_override: Option<u64> = None;

            let (new_data, is_append) = if offset == current_size {
                (data_vec.clone(), true)
            } else if offset > current_size {
                // Write starts past EOF — send only the real data at its actual offset.
                // The gap is implicit zero space; zero-padding it would write content that
                // was never written by the application and creates unnecessary chunks.
                write_file_offset_override = Some(offset as u64);
                (data_vec.clone(), true)
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
                let file_offset = write_file_offset_override.unwrap_or(current_size as u64);
                client.write_data_with_cache(&new_data, cache_inode, file_offset).await
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
                        // For gap writes, the actual end is offset + len, not current_size + len.
                        let write_end = write_file_offset_override
                            .unwrap_or(current_size as u64) + new_data.len() as u64;
                        metadata.size = metadata.size.max(write_end);
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
                        let physical_size = metadata.chunk_locations.iter().map(|l| l.size as u64).sum();
                        metadata.size = metadata.size.max(physical_size);
                        info!("After splice: {} total chunks, {} total bytes",
                              metadata.chunk_locations.len(), metadata.size);
                    } else {
                        warn!("Full file rewrite with {} bytes", new_data.len());
                        metadata.chunk_locations = chunk_locations_opt.unwrap_or_default();
                        metadata.size = metadata.size.max(new_data.len() as u64);
                    }
                    metadata.modified_at = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs();

                    client.enqueue_metadata(&metadata).await;
                    // Update read engine immediately for SQLite read-after-write consistency
                    client.feed_chunk_locations_to_read_engine(
                        ino, &metadata.chunk_locations, metadata.size,
                    ).await;
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
        let flush_handle = self.flush_handle.clone();
        // For SQLite-buffered files, flush() must drain synchronously — SQLite's WAL
        // checkpoint reads data back immediately after flush and must see committed chunks.
        let is_sqlite_flush = self.metadata_cache.get(&ino)
            .map(|m| is_sqlite_buffered(&m.path))
            .unwrap_or(false);

        // Spawn flush operation on tokio's blocking thread pool
        runtime.clone().spawn_blocking(move || {
            debug!("flush: ino={}", ino);

            if write_buffer_enabled {
                if is_sqlite_flush {
                    // SQLite: flush everything now so the fdatasync ordering guarantee holds.
                    debug!("flush: ino={} - SQLite file, flushing buffer synchronously", ino);
                    let result = flush_handle.flush_runtime.block_on(
                        flush_handle.flush_all_pipelined(ino)
                    );
                    match result {
                        Ok(_) => reply.ok(),
                        Err(e) => { error!("flush (SQLite) failed for inode {}: {}", ino, e); reply.error(libc::EIO); }
                    }
                    return;
                }
                // When write-buffering is enabled, flush() must NOT drain the write buffer
                // for DVR/streaming files.
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
                            // Update read engine for SQLite read-after-write consistency
                            client.feed_chunk_locations_to_read_engine(
                                ino, &metadata.chunk_locations, metadata.size,
                            ).await;
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
        // Keep the read engine alive while a writer is still active — it holds pre-seeded
        // chunk locations for in-flight chunks.  Destroying it when the last reader closes
        // (but the writer is still open) drops those entries; the next reader's open() creates
        // a fresh engine from the leader which may only know about committed chunks, causing
        // holes for the range currently being written.
        let has_active_writer = self.write_open_counts.get(&ino).map(|v| *v > 0).unwrap_or(false);
        if is_last_open && !has_active_writer {
            self.client.read_engines.remove(ino);
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
                self.client.write_open_inodes.remove(&ino);
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
                let read_engines_for_release = self.client.read_engines.map.clone();
                let open_counts_for_release = self.open_counts.clone();
                let write_open_counts_for_release = self.write_open_counts.clone();
                // Increment per-inode release counter
                release_in_flight.entry(ino).or_insert_with(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                        if let Some(counter) = release_in_flight.get(&ino) {
                            counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        return;
                    }
                    // Only flush if this session actually has slots with real data.
                    // A write-mode open that reads but never writes (HDHomeRun scan,
                    // Kodi seek-probe) has no slots — calling flush_all_pipelined would
                    // pick up a flushing=true slot from a concurrent session, wait for it
                    // to finish, then dispatch a second PatchChunk with stale data.
                    let has_unflushed = write_buffers.get(&ino)
                        .map(|s| s.try_lock().map(|s| {
                            s.slots.values().any(|sl| !sl.data.is_empty() && !sl.flushing)
                        }).unwrap_or(true))
                        .unwrap_or(false);
                    if has_unflushed {
                        if let Err(e) = flush_handle.flush_all_pipelined(ino).await {
                            error!("release: flush failed for inode {}: {}", ino, e);
                        }
                    } else {
                        // All chunks were already flushed to the servers by the background
                        // tick, but the tick's 5-second metadata throttle may not have
                        // committed the latest chunk map to the leader yet. Do a final
                        // synchronous metadata sync so the leader knows about all chunks
                        // before any reader can open the file.
                        debug!("release: ino={} last writer — no unflushed data, syncing metadata", ino);
                        let meta_to_persist = flush_handle.metadata_cache.get(&ino).map(|m| m.clone());
                        if let Some(meta) = meta_to_persist {
                            flush_handle.client.flush_metadata_sync(&meta).await;
                            flush_handle.last_metadata_update.insert(ino, std::time::Instant::now());
                        }
                    }
                    // Invalidate the read engine's chunk map so the next reader
                    // immediately picks up the newly flushed chunks.
                    // If no fds remain open, drop the engine entirely — it holds
                    // a Vec<ChunkLocation> that grows with the file and is never
                    // needed again once the file is closed for writing.
                    if let Some(engine) = read_engines_for_release.get(&ino) {
                        engine.expire_chunk_map();
                    }
                    // Check open_counts: if no readers are still open, free the engine.
                    // open_counts was decremented synchronously in release() before this
                    // task was spawned, so 0 here means truly no fds remain.
                    if !open_counts_for_release.get(&ino).map(|c| *c > 0).unwrap_or(false) {
                        read_engines_for_release.remove(&ino);
                    }
                    // Only remove the write buffer if no new writer has opened the file
                    // since this release task was spawned. A new O_TRUNC open races with
                    // this cleanup — if we remove here, we destroy the new session's buffer
                    // and its data is silently lost (T7 race).
                    let has_new_writer = write_open_counts_for_release
                        .get(&ino).map(|c| *c > 0).unwrap_or(false);
                    if !has_new_writer {
                        write_buffers.remove(&ino);
                        size_high_water_for_release.remove(&ino);
                    }
                    if let Some(owner) = lock_owner {
                        if let Err(e) = lock_manager.release_all(ino, owner).await {
                            error!("release: lock release failed for inode {}: {}", ino, e);
                        }
                    }
                    if let Some(counter) = release_in_flight.get(&ino) {
                        counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    }
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
                    .map(|s| s.try_lock().map(|s| {
                        s.slots.values().any(|sl| !sl.data.is_empty() && !sl.flushing)
                    }).unwrap_or(false))
                    .unwrap_or(false);
                let flush_handle = self.flush_handle.clone();
                let flush_rt = flush_handle.flush_runtime.clone();
                let write_buffers = self.write_buffers.clone();
                reply.ok();
                flush_rt.spawn(async move {
                    if has_buffer {
                        debug!("release: read-only close for ino={} has buffered data — flushing", ino);
                        if let Err(e) = flush_handle.flush_all_pipelined(ino).await {
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
                loop {
                    let total: usize = release_in_flight.iter()
                        .map(|entry| entry.value().load(std::sync::atomic::Ordering::Relaxed))
                        .sum();
                    if total == 0 { break; }
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
                if let Err(e) = flush_handle.flush_all_pipelined(old_ino).await {
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
                    metadata.chunk_locations = Vec::new();
                    metadata.size = 0;
                    self.write_buffers.remove(&ino);
                    self.size_high_water.remove(&ino);
                    // Only set the truncated flag when there are no active writers.
                    // For O_TRUNC opens, FUSE sends open() first then setattr(size=0) — by then
                    // write_open_count is already ≥1, so this is the current session truncating
                    // itself (not a race from an old session). Setting the flag in that case
                    // would poison the current session's flush.
                    // Only set it when write_open_count==0: a concurrent truncate racing a
                    // previous session's still-in-flight flush — the original intended use.
                    let active_writers = self.write_open_counts.get(&ino).map(|c| *c).unwrap_or(0);
                    if active_writers == 0 {
                        self.truncated_inodes.insert(ino);
                    }
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
                    handle.flush_all_pipelined(ino).await
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
                    if let Err(e) = handle.flush_all_pipelined(ino).await {
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
