use anyhow::Result;
use dashmap::DashMap;
use libc;
use dfs_common::{ChunkId, ChunkLocation, FileMetadata, FileType};
use fuser::{
    FileAttr, FileType as FuseFileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEntry, ReplyStatfs, Request as FuseRequest,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::net::SocketAddr;
use std::path::Path;
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
///
/// Only WAL files benefit from buffering — they are append-only streams of frames
/// that accumulate correctly in-memory before a single flush. Buffering is correct
/// because WAL readers replay from the beginning so ordering within the buffer is
/// preserved when flushed.
///
/// Main .db files and rollback journals must NOT be buffered:
/// - Rollback journal mode requires strict per-fdatasync durability. The journal
///   must be fully durable before any db page is written. Buffering merges writes
///   across fdatasync boundaries, corrupting the rollback ordering.
/// All SQLite database files use the write buffer with sync_on_fsync=true so that:
/// 1. Sequential page writes are serialized (no concurrent-task metadata race).
/// 2. fdatasync flushes synchronously, preserving rollback-journal atomicity.
///
/// .db-shm is excluded separately — it is mmap'd MAP_SHARED.
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

/// SQLite paths that use FOPEN_DIRECT_IO — all except .db-shm (which needs mmap).
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

/// RAII guard that decrements per-(inode, chunk) write-task counters on drop.
/// Used by write() spawned tasks so release()/fsync() can wait for all pending
/// writes to land before flushing, and flush_one_chunk can wait for just the
/// one chunk it's about to snapshot rather than every write anywhere in the file.
struct WriteTaskGuard(Vec<Arc<std::sync::atomic::AtomicUsize>>);
impl Drop for WriteTaskGuard {
    fn drop(&mut self) {
        for c in &self.0 {
            c.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Chunk indices touched by a write of `len` bytes starting at `offset`.
/// Mirrors InodeWriteState::write_at's own chunk-boundary walk so the two stay
/// consistent about which chunks a given write spans.
fn chunk_indices_for_write(offset: u64, len: usize) -> Vec<u64> {
    if len == 0 {
        return vec![offset / CHUNK_SIZE as u64];
    }
    let start_idx = offset / CHUNK_SIZE as u64;
    let end_idx = (offset + len as u64 - 1) / CHUNK_SIZE as u64;
    (start_idx..=end_idx).collect()
}

/// Increment the per-(inode, chunk) in-flight counters for every chunk a write
/// touches, returning the Arc handles so the caller (typically a WriteTaskGuard)
/// can decrement the exact same set later.
fn inc_write_tasks_for_chunks(
    map: &dashmap::DashMap<(u64, u64), Arc<std::sync::atomic::AtomicUsize>>,
    ino: u64,
    chunk_indices: &[u64],
) -> Vec<Arc<std::sync::atomic::AtomicUsize>> {
    chunk_indices.iter().map(|&idx| {
        let counter = map.entry((ino, idx))
            .or_insert_with(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
            .clone();
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        counter
    }).collect()
}

/// Sum of in-flight write tasks across every chunk of a given inode. Used by
/// release()/fsync()/open() which need to know about writes anywhere in the
/// file, not just one chunk.
fn write_tasks_in_flight_for_inode(
    map: &dashmap::DashMap<(u64, u64), Arc<std::sync::atomic::AtomicUsize>>,
    ino: u64,
) -> usize {
    map.iter()
        .filter(|e| e.key().0 == ino)
        .map(|e| e.value().load(std::sync::atomic::Ordering::Relaxed))
        .sum()
}

/// Poll (with timeout) until all in-flight write tasks across every chunk of
/// this inode finish. Returns false on timeout (caller logs/handles as before).
async fn wait_for_inode_writes_done(
    map: &dashmap::DashMap<(u64, u64), Arc<std::sync::atomic::AtomicUsize>>,
    ino: u64,
    timeout: std::time::Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if write_tasks_in_flight_for_inode(map, ino) == 0 {
            return true;
        }
        if tokio::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

/// A single 4MB-aligned write buffer slot for one chunk.
/// Writes land in the slot at `file_offset % CHUNK_SIZE`.
/// The slot is flushed when it fills (exactly CHUNK_SIZE) or on fsync/release/timer.
/// Once flushed successfully, the slot is removed immediately — reads for committed
/// chunks go to the network, which holds the authoritative data.
///
/// Storage is SPARSE (2026-07-15): only app-written byte ranges are resident, as
/// `extents`. The previous representation was a single contiguous Vec indexed from
/// the chunk start, which materialized real zero-filled RAM for every gap — a lone
/// 4K random write at a 3MB intra-offset allocated ~3MB of zeros. Under QD32 4K
/// random writes that inflated resident memory ~20x over real dirty data (measured
/// live on server5 via BPFILLTIMING: 336MB resident from ~15MB dirty), pinning
/// `global_buffered_bytes` at the memory cap and throttling every write to the
/// 10ms back-pressure sleep — the root cause of the size-dependent kdiskmark
/// collapse (128MB fine / 256MB bimodal / 512MB+1GB floored at ~0.4MB/s).
/// The padded contiguous view still exists, but only transiently: `materialize()`
/// builds it at flush-snapshot time (bounded by flush concurrency, freed after the
/// network round) instead of it living in the buffer map for the slot's lifetime.
#[derive(Clone)]
struct ChunkSlot {
    /// App-written byte runs, sorted by intra-chunk start offset, non-overlapping,
    /// non-adjacent (write_extent merges exactly like mark_dirty merges
    /// dirty_ranges, so these runs mirror dirty_ranges' boundaries). Only these
    /// bytes are resident — gaps between/before them are NOT allocated.
    extents: Vec<(usize, Vec<u8>)>,
    /// Virtual end of the buffered region (what `data.len()` used to be): the
    /// furthest intra-chunk offset this slot's state extends to, counting gap-fill
    /// regions that are tracked but no longer materialized. Grows on writes and on
    /// gap-fill bookkeeping; never shrinks while the slot lives.
    span_end: usize,
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
    /// Exact byte ranges written by the application in this session, as (start, end) pairs.
    /// Maintained as a sorted, merged list. Used by the overwrite PatchChunk path to issue
    /// one patch per contiguous dirty range, skipping gap-fill zeros between them.
    /// Without this, a non-contiguous SQLite write (e.g. pages 1, 4, 5 but not 2-3) would
    /// patch a single range [0..real_data_end] that includes synthetic zeros at positions
    /// 2-3, silently overwriting correct server data with zeros.
    dirty_ranges: Vec<(usize, usize)>,
    /// Set to true by flush_one_chunk when it claims this slot for network I/O.
    /// Prevents a second concurrent flush task from picking the same slot.
    /// Cleared on failure (so it can be retried); on success the slot is removed.
    flushing: bool,
    /// Intra-chunk end offset of the most recent app write to this slot.
    /// Used to detect when an incoming write breaks the sequential pattern — a transition
    /// from sequential appends to a random offset signals that the existing buffered data
    /// should be flushed immediately rather than waiting for the timer.
    /// None until the first app write lands in this slot.
    last_sequential_end: Option<usize>,
    /// Consecutive patch failures for this slot. When this exceeds MAX_PATCH_FAILURES
    /// the flush falls back to a fresh write, preventing an infinite retry loop when
    /// clock skew or pre-existing corruption keeps the stale-broadcast guard from
    /// converging in a timely manner.
    consecutive_patch_failures: u32,
    /// The chunk_id last confirmed by the server for this slot — set from every
    /// successful MultiPatch or fresh-write response, never from metadata_cache or
    /// recent_chunk_writes. This is the authoritative base for the next patch: if set,
    /// it takes priority over all other sources so we never use a stale chunk_id.
    server_chunk_id: Option<ChunkId>,
    /// Total flush attempts that ended in the terminal "cannot safely fresh-write"
    /// failure (see reconstruct_or_abort_for_fresh_write), across ticks — unlike
    /// consecutive_patch_failures, this is NOT reset when that failure fires, since its
    /// only purpose is pacing retries of a chunk that's failed this specific way before.
    /// Used to compute `retry_backoff_until`.
    terminal_failure_count: u32,
    /// Set on a terminal flush failure to a short-in-the-future deadline; the periodic
    /// ticker skips this slot until it passes. Without this, a chunk that fails this way
    /// (e.g. its data no longer exists anywhere in the cluster) gets retried on every
    /// 50ms tick forever — see the 2026-07-03 staging incident, where a permanently
    /// under-replicated chunk's sole replica was wiped mid-repave: every retry failed
    /// identically, at ~20/sec, until the client was manually restarted. Backing off
    /// doesn't fix an unrecoverable chunk, but it stops the client from hammering the
    /// leader and spamming logs over something that isn't going to change on its own.
    retry_backoff_until: Option<std::time::Instant>,
    /// Set by write_at() when a write lands in a *different* chunk than the last one —
    /// an unambiguous, event-driven signal that this slot won't receive more data soon
    /// (as opposed to merely being idle for some duration, which can misfire for a
    /// continuous sequential stream — see STALE_FLUSH_MS's history: a blind 50ms idle
    /// timer once flushed a live DVR recording mid-write and corrupted it, commit
    /// 0bc3b09). Cleared the next time this slot is written to. The background ticker
    /// treats abandoned slots as immediately flush-eligible regardless of
    /// no_active_writers, since the file having other active writers elsewhere is
    /// irrelevant to whether *this specific* chunk is done — the DVR case that
    /// motivated no_active_writers is about a chunk still being actively appended to,
    /// which this flag structurally cannot signal for (only a real cross-chunk move
    /// sets it). Root cause of the 2026-07-05 kdiskmark RND4K throughput collapse:
    /// without this, abandoned chunks from random-write patterns fell through to the
    /// 2000ms STALE_FLUSH_MS fallback, forcing a multi-GB write-buffer cap just to
    /// avoid back-pressure on the resulting backlog.
    abandoned: bool,
}

impl ChunkSlot {
    fn new() -> Self {
        Self {
            extents: Vec::new(),
            span_end: 0,
            last_modified: SystemTime::now(),
            gap_filled_prefix: 0,
            real_data_end: 0,
            dirty_ranges: Vec::new(),
            consecutive_patch_failures: 0,
            flushing: false,
            last_sequential_end: None,
            server_chunk_id: None,
            terminal_failure_count: 0,
            retry_backoff_until: None,
            abandoned: false,
        }
    }

    /// Backoff after a terminal flush failure: 1s, 2s, 4s, ... capped at 30s.
    fn record_terminal_failure(&mut self) {
        self.terminal_failure_count = self.terminal_failure_count.saturating_add(1);
        let secs = 1u64 << self.terminal_failure_count.saturating_sub(1).min(5);
        self.retry_backoff_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(secs.min(30)));
    }

    /// True if a prior terminal failure's backoff hasn't elapsed yet.
    fn in_backoff(&self) -> bool {
        self.retry_backoff_until.is_some_and(|t| std::time::Instant::now() < t)
    }

    /// Record a write at [start, end) into dirty_ranges, merging with adjacent ranges.
    fn mark_dirty(&mut self, start: usize, end: usize) {
        if start >= end { return; }
        let mut new_start = start;
        let mut new_end = end;
        // Merge with any existing ranges that overlap or are adjacent.
        self.dirty_ranges.retain(|&(s, e)| {
            if e < new_start || s > new_end {
                true // no overlap, keep separate
            } else {
                new_start = new_start.min(s);
                new_end = new_end.max(e);
                false // absorbed into the new range
            }
        });
        self.dirty_ranges.push((new_start, new_end));
        self.dirty_ranges.sort_unstable();
    }

    fn is_full(&self) -> bool {
        if self.span_end < CHUNK_SIZE {
            return false;
        }
        // Overwrite slots can span the full existing 4MB chunk immediately
        // (span_end >= CHUNK_SIZE from gap-fill bookkeeping) — we can't use span
        // alone. Only dispatch immediately when the full span is dirty (total_dirty
        // == span_end). flush_buffer_async_one detects this as
        // is_full_replacement=true and takes the fresh-write path: one 4MB write to
        // a new content-addressed chunk, no patch/rename/re-hash. Equivalent to a
        // brand new write.
        //
        // Partial overwrites (random writes, small VM ops) don't dispatch here —
        // they're flushed by the write-pattern-change detector (sequential→random
        // transition triggers an immediate notify) or by fsync/release (urgent=true).
        let total_dirty: usize = self.dirty_ranges.iter().map(|&(s, e)| e - s).sum();
        total_dirty >= self.span_end
    }

    /// True if this slot has no buffered state at all — neither app-written bytes
    /// nor gap-fill bookkeeping. Replaces the old `data.is_empty()` checks (a slot
    /// created with a gap-fill span from flushed_sizes was non-empty under the old
    /// representation too, via its materialized zero prefix).
    fn is_empty(&self) -> bool {
        self.span_end == 0
    }

    /// Real bytes resident in this slot — only app-written extent bytes, since
    /// gaps are no longer materialized. This is what must count against the
    /// global memory cap.
    fn resident(&self) -> usize {
        self.extents.iter().map(|(_, d)| d.len()).sum()
    }

    /// Write `data` at intra-chunk offset `start`, merging with any overlapping or
    /// adjacent extents (same merge rule as mark_dirty, so extent boundaries stay
    /// mirror-identical to dirty_ranges'). Fast paths for the two hot patterns:
    /// pure sequential append (extends the last extent in place, O(1) amortized —
    /// DVR recordings) and fully-interior overwrite (copy_from_slice in place, no
    /// realloc — kdiskmark re-touching the same 4K block).
    fn write_extent(&mut self, start: usize, data: &[u8]) {
        if data.is_empty() { return; }
        let end = start + data.len();
        // Fast path 1: sequential append to the last extent.
        if let Some((last_start, last_data)) = self.extents.last_mut() {
            if *last_start + last_data.len() == start {
                last_data.extend_from_slice(data);
                return;
            }
            // Fast path 2: fully contained within the last extent (common: repeated
            // small overwrites near the append frontier).
            let last_end = *last_start + last_data.len();
            if start >= *last_start && end <= last_end {
                let off = start - *last_start;
                last_data[off..off + data.len()].copy_from_slice(data);
                return;
            }
        }
        // Fast path 2b: fully contained within ANY single extent.
        for (ex_start, ex_data) in self.extents.iter_mut() {
            let ex_end = *ex_start + ex_data.len();
            if start >= *ex_start && end <= ex_end {
                let off = start - *ex_start;
                ex_data[off..off + data.len()].copy_from_slice(data);
                return;
            }
        }
        // General path: collect every extent overlapping or adjacent to [start, end),
        // rebuild one merged extent covering them all plus the new data.
        let mut new_start = start;
        let mut new_end = end;
        let mut absorbed: Vec<(usize, Vec<u8>)> = Vec::new();
        self.extents.retain_mut(|(s, d)| {
            let e = *s + d.len();
            if e < new_start || *s > new_end {
                true // disjoint and non-adjacent — keep
            } else {
                new_start = new_start.min(*s);
                new_end = new_end.max(e);
                absorbed.push((*s, std::mem::take(d)));
                false
            }
        });
        let mut merged = vec![0u8; new_end - new_start];
        // Old extents first, new data last so the new write wins on overlap.
        for (s, d) in absorbed {
            merged[s - new_start..s - new_start + d.len()].copy_from_slice(&d);
        }
        merged[start - new_start..start - new_start + data.len()].copy_from_slice(data);
        let pos = self.extents.iter().position(|(s, _)| *s > new_start).unwrap_or(self.extents.len());
        self.extents.insert(pos, (new_start, merged));
    }

    /// Build the padded contiguous view (what `data` used to hold): span_end bytes,
    /// zeros everywhere the app didn't write, extents copied into place. Called at
    /// flush-snapshot and read-splice time only — the result is transient, bounded
    /// by flush/read concurrency, unlike the old always-resident representation.
    fn materialize(&self) -> Vec<u8> {
        let mut buf = vec![0u8; self.span_end];
        for (s, d) in &self.extents {
            let end = (s + d.len()).min(buf.len());
            if *s >= end { continue; }
            buf[*s..end].copy_from_slice(&d[..end - s]);
        }
        buf
    }

    /// Serve a read of up to `max` bytes starting at intra-chunk offset `intra`
    /// directly from the extent holding that offset (no materialization). Returns
    /// None if no extent covers `intra` — callers fall through to the network,
    /// which is the correct behavior for gap/synthetic regions anyway.
    fn read_at(&self, intra: usize, max: usize) -> Option<&[u8]> {
        for (s, d) in &self.extents {
            if *s <= intra && intra < s + d.len() {
                let off = intra - s;
                let n = max.min(d.len() - off);
                return Some(&d[off..off + n]);
            }
        }
        None
    }

    fn is_idle(&self) -> bool {
        self.last_modified.elapsed().unwrap_or_default() > std::time::Duration::from_millis(50)
    }

    fn dirty_bytes(&self) -> usize {
        self.dirty_ranges.iter().map(|&(s, e)| e - s).sum()
    }

    /// True if dirty_ranges has more than one distinct (non-adjacent) range — the
    /// signature of scattered/random writes landing at different offsets within the
    /// same chunk (e.g. repeated small patches to one chunk, never advancing past
    /// it). A genuine sequential fill-in-progress (e.g. a DVR recording) keeps
    /// merging each new write into a single contiguous range (mark_dirty() merges
    /// overlapping/adjacent ranges), so this stays false for it throughout — used to
    /// gate the dirty-byte flush threshold so it only fires for the scattered case
    /// it's meant for, not for an ordinary sequential chunk that's about to complete
    /// naturally via is_full() anyway. See SLOT_DIRTY_FLUSH_THRESHOLD_BYTES's history.
    fn is_fragmented(&self) -> bool {
        self.dirty_ranges.len() >= 2
    }
}

/// Tracks one (chunk_idx, node) miss streak for canonical_write_nodes' time-based
/// drop decision (see that field's doc comment). `first_missed_at` anchors the grace
/// period; `last_heal_request_at` throttles how often a targeted restore gets
/// re-requested for the same node while still within that window, so a hot chunk
/// missing the same node on every round for 2 seconds doesn't fire dozens of
/// redundant concurrent heal requests at it.
struct MissTracker {
    first_missed_at: std::time::Instant,
    last_heal_request_at: Option<std::time::Instant>,
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
    /// If true, every fsync() must flush immediately (O_SYNC / O_DSYNC was set on open).
    /// If false, fsyncs within the coalescing window are absorbed (DVR / streaming mode).
    sync_on_fsync: bool,
    /// True when the file was opened with O_TRUNC (full replacement). PatchChunk must not
    /// be used in this session — the caller is writing a new file, not patching an old one.
    is_truncated_session: bool,
    /// File ID set at create() or open() time. flush_buffer_async_one uses this to verify
    /// that metadata_cache still refers to the same file — if it changed (delete+recreate
    /// with inode reuse), existing_chunk_size from the old file must be ignored.
    expected_file_id: Option<dfs_common::FileId>,
    /// Per-chunk notification for in-flight flushes. When a flush for chunk N starts, it
    /// creates a Notify and stores it here. When the flush completes (metadata committed),
    /// it notifies all waiters and removes the entry. Before starting a new flush for chunk N,
    /// we wait on this notifier to ensure FIFO ordering for overlapping chunk flushes.
    chunk_flush_waiters: HashMap<u64, Arc<tokio::sync::Notify>>,
    /// Canonical write-pair for each chunk in this session. Set on the first successful
    /// patch of a chunk and held for the lifetime of the write session. All subsequent
    /// patches to the same chunk use these exact nodes — ignoring healer-added replicas
    /// in metadata_cache and recent_chunk_writes. This prevents the ChunkStale cascade
    /// where alternating between different node pairs creates divergent chunk versions.
    canonical_write_nodes: HashMap<u64, Vec<dfs_common::NodeId>>,
    /// Miss tracking per (chunk_idx, node) backing canonical_write_nodes's update
    /// site — see that call site's doc comment. A node absent from one MultiPatch
    /// round's result (e.g. it was mid-fold on the server and slower than its peer
    /// this one time) must not be silently and permanently dropped from future
    /// patch targeting; this bounds how long a node gets to catch back up before
    /// it's actually dropped.
    ///
    /// Time-based, not round-count-based (changed 2026-07-11 — see
    /// MISS_STREAK_DROP_GRACE's doc comment for the incident this fixes): a hot
    /// chunk under a rapid patch storm (e.g. qcow2 install traffic) can complete 5+
    /// rounds in under 100ms, so a fixed consecutive-round cap gave a genuinely
    /// healthy-but-momentarily-slower node no real time to catch up before being
    /// permanently dropped — collapsing the chunk to RF=1 exactly when the write
    /// load made recovering that redundancy matter most.
    canonical_node_miss_streak: HashMap<(u64, dfs_common::NodeId), MissTracker>,
    /// Chunk index of the most recent write_at call. Used to detect when the caller
    /// jumps to a different chunk, so the previous chunk's partial slot can be flushed
    /// immediately rather than waiting for the timer.
    last_written_chunk: Option<u64>,
}

impl InodeWriteState {
    fn new(sync_on_fsync: bool) -> Self {
        Self {
            slots: HashMap::new(),
            flushed_sizes: HashMap::new(),
            canonical_write_nodes: HashMap::new(),
            canonical_node_miss_streak: HashMap::new(),
            sync_on_fsync,
            is_truncated_session: false,
            expected_file_id: None,
            chunk_flush_waiters: HashMap::new(),
            last_written_chunk: None,
        }
    }

    /// Returns the chunk index and intra-chunk offset for a given file byte offset.
    fn chunk_index(file_offset: u64) -> u64 {
        file_offset / CHUNK_SIZE as u64
    }

    fn intra_offset(file_offset: u64) -> usize {
        (file_offset % CHUNK_SIZE as u64) as usize
    }

    /// Write bytes into the appropriate slot(s). Returns true if any write broke the
    /// sequential pattern of an existing slot (indicating the flush worker should be
    /// woken immediately to drain buffered data before it gets fragmented further).
    fn write_at(&mut self, file_offset: u64, data: &[u8]) -> bool {
        let mut pattern_changed = false;
        let mut remaining = data;
        let mut cur_offset = file_offset;

        while !remaining.is_empty() {
            let idx = Self::chunk_index(cur_offset);
            let intra = Self::intra_offset(cur_offset);

            // Cross-chunk pattern change: if this write lands in a different chunk than
            // the last one, the previous chunk's slot MAY have been abandoned mid-fill.
            // Signal an immediate flush so its buffered data doesn't wait for the timer.
            // Also mark the slot `abandoned` — a stronger, event-driven signal than
            // pattern_changed's notify_one() wake-up alone, which only wakes the ticker
            // without giving it a way to treat this slot as eligible regardless of
            // no_active_writers. See `abandoned`'s doc comment.
            //
            // Guarded by concurrent_streams: last_written_chunk is a single global
            // cursor over ALL write_at() calls for this inode, so a multi-threaded
            // writer (e.g. pbs-restore's 4 concurrent restore threads, or qemu-img
            // convert) that interleaves writes across several chunks at once makes
            // *every* call look like a "jump" — even though prev_idx's own writer
            // hasn't actually moved on and still has more real data coming. Root-caused
            // 2026-07-11 via a live Proxmox qcow2 restore: chunk 261 was abandoned and
            // flushed (zero-filling its still-unwritten first 458752 bytes as if they
            // were a legitimate sparse gap) ~1ms after an unrelated write landed in
            // chunk 262 from a different restore thread — then ~100ms later, real
            // (non-zero) data for that exact "gap" arrived, proving it was never sparse.
            // If any OTHER slot already has real unflushed data pending, that's a
            // concurrent multi-region writer, not a single sequential stream moving on —
            // fall back to the existing 50ms is_idle() ticker instead of flushing now.
            if let Some(prev_idx) = self.last_written_chunk {
                if prev_idx != idx {
                    let concurrent_streams = self.slots.iter().any(|(&other_idx, s)| {
                        other_idx != prev_idx
                            && (s.real_data_end > 0 || s.gap_filled_prefix > 0)
                            && !s.flushing
                    });
                    if !concurrent_streams {
                        if let Some(prev_slot) = self.slots.get_mut(&prev_idx) {
                            if (prev_slot.real_data_end > 0 || prev_slot.gap_filled_prefix > 0)
                                && !prev_slot.flushing
                            {
                                pattern_changed = true;
                                prev_slot.abandoned = true;
                            }
                        }
                    }
                }
            }
            self.last_written_chunk = Some(idx);

            let flushed = self.flushed_sizes.get(&idx).copied().unwrap_or(0);
            let slot = self.slots.entry(idx).or_insert_with(|| {
                let mut s = ChunkSlot::new();
                // Gap-fill bytes already on the server so the slot accurately represents
                // the full chunk state. Without this, is_append_extend PatchChunk would
                // send only the tail, missing the first flushed_sizes bytes.
                if flushed > 0 {
                    debug!("write_at: chunk={} created slot with gap_fill={} from flushed_sizes", idx, flushed);
                    s.span_end = flushed;
                    s.gap_filled_prefix = flushed;
                    // last_sequential_end left as None: the first write to this slot is
                    // always treated as sequential so that overwrite workloads (VM disk
                    // sequential writes to an existing image) can accumulate normally.
                    // Random writes are caught on the second write via gap_filled_prefix > 0.
                }
                s
            });
            // This slot is receiving data again — if it was previously marked abandoned
            // (a write moved away from it, then came back before the ticker flushed it),
            // that's no longer true.
            slot.abandoned = false;

            // Detect write-pattern change: if this write doesn't start exactly where
            // the last one ended, the caller has switched from sequential to random
            // I/O. Signal an immediate flush of whatever is already buffered so
            // partial-chunk data doesn't sit in the buffer indefinitely.
            // Also treat any write to a gap-filled slot as a pattern change — the slot
            // represents already-committed server data, so every write is a random
            // patch regardless of position, and should be flushed promptly.
            let expected = slot.last_sequential_end.unwrap_or(intra);
            if intra != expected && (slot.real_data_end > 0 || slot.gap_filled_prefix > 0) {
                pattern_changed = true;
            }

            // Extend the tracked span to cover intra_offset. The gap itself is NOT
            // materialized anymore (see ChunkSlot's doc comment) — only the span/
            // gap_filled_prefix bookkeeping happens here; materialize() re-creates
            // the zeros transiently at flush time for paths that need them.
            let pre_fill_len = slot.span_end;
            if slot.span_end < intra {
                // Only update gap_filled_prefix if the gap extends from the START of
                // the slot's state (gap_filled_prefix == pre_fill_len). Mid-slot gaps
                // (after real data) should NOT update gap_filled_prefix, as that
                // would incorrectly mark earlier real data as gap-filled.
                let fill_end = intra;
                if slot.gap_filled_prefix == pre_fill_len && slot.gap_filled_prefix < fill_end {
                    debug!("write_at: chunk={} gap_fill {} -> {} (sparse write at offset {})",
                           idx, slot.gap_filled_prefix, fill_end, file_offset);
                    slot.gap_filled_prefix = fill_end;
                } else if pre_fill_len > 0 {
                    debug!("write_at: chunk={} gap {} -> {} (mid-slot gap, gap_prefix stays {})",
                           idx, pre_fill_len, intra, slot.gap_filled_prefix);
                }
                slot.span_end = intra;
            }

            let space = CHUNK_SIZE - intra;
            let n = remaining.len().min(space);

            let write_start_offset = cur_offset; // Save for logging before incrementing

            // Write or overwrite: sparse extent insert/merge (fast-pathed for
            // sequential appends and in-place overwrites — see write_extent).
            slot.write_extent(intra, &remaining[..n]);
            slot.span_end = slot.span_end.max(intra + n);
            // If the app writes at or before the gap-filled prefix, real data now covers
            // that region — shrink gap_filled_prefix so PatchChunk sends the real bytes
            // rather than treating the entire gap-fill as already-on-server data.
            let old_gap_prefix = slot.gap_filled_prefix;
            if intra < slot.gap_filled_prefix {
                slot.gap_filled_prefix = intra;
                debug!("write_at: chunk={} gap_filled_prefix shrunk {} -> {} (overwrite at intra={})",
                       idx, old_gap_prefix, slot.gap_filled_prefix, intra);
            }
            // Track the furthest byte of real app-written data. Bytes beyond this in the
            // slot are synthetic gap-fill zeros (representing data already on the server)
            // and must not be sent as real patch data.
            let write_end = intra + n;
            if write_end > slot.real_data_end {
                slot.real_data_end = write_end;
            }
            // Record the exact dirty range so the overwrite PatchChunk path can issue
            // one patch per contiguous written region, skipping gaps of synthetic zeros.
            slot.mark_dirty(intra, write_end);
            slot.last_modified = SystemTime::now();
            // Advance sequential tracking regardless of whether this write was sequential.
            // The next write will compare its start against this end position.
            slot.last_sequential_end = Some(write_end);

            debug!("write_at: chunk={} file_offset={} intra={} write_end={} len={} | span_end={} resident={} gap_prefix={} real_end={} dirty_ranges={:?}",
                   idx, write_start_offset, intra, write_end, n, slot.span_end, slot.resident(), slot.gap_filled_prefix, slot.real_data_end,
                   slot.dirty_ranges);

            remaining = &remaining[n..];
            cur_offset += n as u64;
        }

        pattern_changed
    }

    /// How many dirty bytes are buffered across all slots.
    /// Counts only application-written ranges, not gap-fill prefix bytes.
    /// Gap-fill is a layout artifact (zeroes representing committed server data)
    /// and must not contribute to back-pressure — otherwise a single 4KB write
    /// to an existing 4MB chunk registers as 4MB of pressure, causing writes to
    /// stall after just 16 slots even though only ~64KB of real data is dirty.
    fn buffered_bytes(&self) -> usize {
        self.slots.values()
            .map(|s| s.dirty_ranges.iter().map(|&(start, end)| end - start).sum::<usize>())
            .sum()
    }

    /// Real memory resident in slot buffers. Since 2026-07-15's sparse-extent
    /// rework, gap-fill padding is no longer materialized, so this now equals
    /// buffered_bytes() plus negligible per-extent overhead — but it stays a
    /// separate function reading the extents' actual allocations (not
    /// dirty_ranges arithmetic) because it feeds the memory-safety cap and must
    /// track what's genuinely allocated, whatever the representation does in the
    /// future. History: under the old padded-Vec representation this diverged
    /// from buffered_bytes() by up to ~1000x (one 4K random write materialized
    /// ~4MB of zeros), which is BOTH why the cap had to count it (the 2026-07-05
    /// OOM: multi-GB RSS from padding, scripts/repro_write_deadlock.sh) AND why
    /// the cap then falsely saturated under QD32 4K random writes (2026-07-15:
    /// 336MB resident from ~15MB dirty pinned the cap and floored kdiskmark at
    /// ~0.4MB/s for any file ≥512MB — the size-dependent collapse's root cause).
    /// Removing the padding fixed both sides at once.
    fn resident_bytes(&self) -> usize {
        self.slots.values().map(|s| s.resident()).sum()
    }

    /// Slots that are full and not yet claimed by a flush task.
    fn full_slot_indices(&self) -> Vec<u64> {
        self.slots.iter()
            .filter(|(_, s)| s.is_full() && !s.flushing)
            .map(|(idx, _)| *idx)
            .collect()
    }

    /// True if any slot is ready for flush_one_chunk to pick up right now — full,
    /// abandoned, idle, or over the per-chunk dirty threshold. Used by the
    /// self-refilling ticker loop to decide whether to keep dispatching immediately
    /// rather than falling back to the next periodic tick; MUST exactly mirror
    /// flush_one_chunk's own (non-urgent) selection criteria, including
    /// !in_backoff() — omitting it caused a real self-refilling-loop hang (this
    /// method said "more work" forever for a backed-off slot while
    /// flush_one_chunk(ino, false) correctly skipped it and returned Ok(()) doing
    /// nothing, spinning forever and permanently pinning one of the
    /// PIPELINE_MAX_ITEMS concurrency slots) — caught during 2026-07-05 kdiskmark
    /// testing of the `abandoned` fix.
    fn has_flushable_slot(&self) -> bool {
        self.slots.values().any(|s| {
            !s.flushing && !s.is_empty() && !s.in_backoff() && (
                s.is_full() || s.is_idle() || s.abandoned ||
                (s.is_fragmented() && s.dirty_bytes() >= SLOT_DIRTY_FLUSH_THRESHOLD_BYTES)
            )
        })
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


/// Maximum number of chunk-flush tasks that may run concurrently per inode.
/// Allows small patches to pipeline efficiently (32 × 1KB = 32KB in flight) while
/// large/zero-padded slots (e.g. a random write into a chunk that hasn't been
/// touched yet this session — write_at zero-fills up to the write's offset, so a
/// single 4KB write near the end of a chunk can balloon the slot to nearly 4MB)
/// hit the byte limit (PIPELINE_MAX_BYTES) instead, regardless of item count.
/// Background ticker: gentle limit to avoid overwhelming servers during steady-state writes.
/// Raised 16 -> 24 -> 32 after RND4K Q32T1 write benchmarking showed item count was the
/// binding constraint on the background flush drain, not per-chunk durability latency.
/// The byte cap below was previously enforced only by flush_all_pipelined (fsync/release);
/// the ticker now enforces both, so raising this no longer raises worst-case memory —
/// only PIPELINE_MAX_BYTES does that.
const PIPELINE_MAX_ITEMS: usize = 32;
/// fsync/release flush: each wave in flush_all_pipelined's dispatch loop spawns up to
/// this many concurrent flush_one_chunk tasks and then awaits the ENTIRE wave before
/// computing the next one (a full barrier — see that function's loop). One slow task
/// (a retry, a replica timeout, a heal-on-demand) stalls every other task in its wave
/// and delays the next wave's dispatch by the same amount, even though the rest of the
/// wave's budget was free again almost immediately. Lowered from 64 (2026-07-14): under
/// staging's 512MB q32t1 write load this tail-latency-gates-the-batch effect produced
/// multi-second per-fsync stalls (a burst of patches completing in the first few
/// seconds, then a near-total stall for the rest of the write) — a large wave means a
/// large chance that at least one of its members is the slow tail. A continuous-refill
/// rewrite (dispatch fills the gap as each task completes, rather than waiting for the
/// whole batch) was tried and reverted the same day: T22b/T25e and others regressed
/// because it dispatches flush_one_chunk claims far more aggressively than this
/// wave-barrier design, catching a pre-existing gap where flush_one_chunk does not wait
/// for write_tasks_in_flight when gap_filled_prefix==0 (removed deliberately for
/// sequential/non-sparse writes — see flush_one_chunk's doc comment, RND4K Q32T1
/// collapsed ~20x with it) — a mid-write flush claim can tear a kernel-split large
/// write. Shrinking the wave size instead keeps this function's proven claim/dispatch
/// timing untouched and just bounds how many tasks any one straggler can hold hostage.
const FLUSH_ALL_MAX_ITEMS: usize = 8;

/// Maximum total bytes in-flight across concurrent flush tasks per inode.
/// Set to 8 × CHUNK_SIZE (32MB) — matches the typical in-flight footprint the old
/// PIPELINE_MAX_ITEMS=16 item-only cap produced for random-write workloads (each
/// flushed slot averages ~2MB for a single random touch into an untouched chunk,
/// so 16 × ~2MB ≈ 32MB). Used by both the background ticker and flush_all_pipelined,
/// so raising PIPELINE_MAX_ITEMS buys more concurrency for small patches without
/// raising the worst-case memory ceiling for large/zero-padded slots.
const PIPELINE_MAX_BYTES: usize = 8 * CHUNK_SIZE;

/// TEMP DIAGNOSTIC (remove once the write-buffer-cap theory is confirmed/killed):
/// process-wide rate limiter for BPFILLTIMING, logged from both write() back-pressure
/// loops (fast path and slow path) whenever a write pays ANY non-zero delay (fill_pct
/// >= 75%), not just the already-logged 100%-stuck case. Global (not per-inode) since
/// we want one legible time series of buffer pressure across the whole run, and QD32
/// writes to a single file would otherwise flood the log every single write once past
/// the 75% band. 100ms floor is tight enough to see a ramp/plateau shape but coarse
/// enough to stay readable across a 30s kdiskmark run.
static LAST_BPFILL_LOG_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn maybe_log_bpfill(path_tag: &str, ino: u64, fill_pct: usize, delay_ms: u64, current: usize, cap: usize) {
    if delay_ms == 0 { return; }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_BPFILL_LOG_MS.load(std::sync::atomic::Ordering::Relaxed);
    if now_ms.saturating_sub(last) < 100 { return; }
    if LAST_BPFILL_LOG_MS.compare_exchange(
        last, now_ms, std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed
    ).is_ok() {
        info!("BPFILLTIMING path={} ino={} fill_pct={} delay_ms={} buffered={} cap={}",
              path_tag, ino, fill_pct, delay_ms, current, cap);
    }
}

/// Per-chunk dirty-byte safety net: a slot whose dirty coverage crosses this
/// AND is fragmented (ChunkSlot::is_fragmented — dirty_ranges.len() >= 2) is
/// flush-eligible regardless of idle/abandoned status — for slots that keep
/// getting scattered/repeated touches at different offsets within the same
/// chunk (so never go idle, never get abandoned via a cross-chunk move, and
/// never hit is_full()), e.g. VM-disk-style repeated same-chunk patches
/// (local suite T22/T26/T27).
///
/// The fragmentation gate matters: this was first tried WITHOUT it, checking
/// dirty bytes alone. That worked for the scattered case but also fired on an
/// ordinary sequentially-filling chunk (e.g. a DVR recording) well before it
/// naturally reaches is_full() via a single contiguous dirty range — DVR's
/// sequential appends always keep mark_dirty() merging into one range, so
/// is_fragmented() reliably stays false for it throughout, while T22/T26's
/// same-chunk-different-offset pattern always produces multiple ranges.
/// Splitting single sequential chunk fills into extra round trips measurably
/// added cluster load that starved other clients sharing the same 5-node
/// cluster (rock5b's benchmark slowed down once nanopir3's DVR recording
/// started sending several times as many small ops) — simply raising the
/// byte threshold to avoid that (94% of a chunk) then regressed T22/T26/T27
/// back to their pre-abandoned-fix durations, since those tests' slots are
/// never abandoned (they never leave the chunk) and were relying on this
/// threshold specifically. The fragmentation gate is what lets both cases be
/// fixed at once: aggressive for genuinely scattered writes, hands-off for
/// sequential fills in progress.
///
/// One shared constant, not three duplicated locals — the ticker gate,
/// flush_one_chunk's selection, and has_flushable_slot() must all agree or
/// they can disagree about what's eligible (see has_flushable_slot's doc
/// comment for a real bug this already caused once).
const SLOT_DIRTY_FLUSH_THRESHOLD_BYTES: usize = CHUNK_SIZE / 4;

/// Historical note on the write-buffer cap (now computed at runtime in
/// new_with_runtime as global_write_buffer_cap_bytes, scaled to available
/// memory — see its computation for the full rationale): this was originally a
/// flat 256MB constant, which real-world kdiskmark testing on server5 showed
/// collapsed RND4K Q32T1/Q1T1 random-write throughput ~40x (3MB/s -> 0.07MB/s)
/// while sequential writes and reads improved. Root cause: resident_bytes() now
/// correctly charges the FULL gap-fill preload (up to CHUNK_SIZE) against this
/// cap for every distinct chunk touched (see its doc comment) — sequential
/// writes reuse the same/adjacent chunk many times before it needs to flush,
/// amortizing that cost, but *random* 4K writes across a multi-GB disk touch a
/// new chunk almost every write, each paying the full preload cost. At 256MB
/// (64 chunks) that exhausted almost immediately for kdiskmark's ~1GiB
/// random-write test region (256 chunks), collapsing concurrency to roughly one
/// admitted write per completed flush.

/// Cheaply-cloneable handle to the fields needed by flush_buffer_async.
/// Extracted so fsync() can clone it and spawn a background flush task without
/// holding a reference to DfsFilesystem (which is !Clone due to &mut self callbacks).
#[derive(Clone)]
struct FlushHandle {
    client: Arc<DfsClient>,
    write_buffers: Arc<DashMap<u64, Arc<Mutex<InodeWriteState>>>>,
    metadata_cache: Arc<DashMap<u64, FileMetadata>>,
    chunk_write_locks: Arc<DashMap<u64, Arc<DashMap<u64, Arc<tokio::sync::Mutex<()>>>>>>,
    /// Tracks how many chunk-flush tasks are currently in-flight per inode.
    /// Capped at PIPELINE_MAX_ITEMS by the background ticker.
    /// flush_all_pipelined (fsync/release) uses byte limit only — no item cap.
    flush_in_flight: Arc<RwLock<Option<Arc<DashMap<u64, usize>>>>>,
    last_metadata_update: Arc<DashMap<u64, std::time::Instant>>,
    /// Per-inode timestamp of the last full-metadata push made by a *background* (non-force)
    /// flush tick. Gates how often flush_buffer_async's background branch sends the complete
    /// FileMetadata (whole chunk_locations Vec) to the leader — see its use in
    /// flush_buffer_async for why this must be time-throttled rather than per-chunk.
    last_bg_metadata_push: Arc<DashMap<u64, std::time::Instant>>,
    dir_cache: Arc<DashMap<String, (Vec<FileMetadata>, std::time::Instant)>>,
    /// Tracks when each directory was last invalidated (create/mkdir/unlink/rename).
    /// readdir only inserts into dir_cache if the directory wasn't invalidated
    /// DURING the in-flight list_directory fetch — prevents stale empty results
    /// from being cached after a concurrent create() cleared the entry.
    dir_cache_invalidated_at: Arc<DashMap<String, std::time::Instant>>,
    path_to_inode: Arc<RwLock<HashMap<String, u64>>>,
    inode_to_path: Arc<RwLock<HashMap<u64, String>>>,
    /// Inodes that received a setattr(size=0) truncate while a flush was in progress.
    /// Prevents a racing flush from re-populating metadata with stale chunk locations.
    /// Cleared once fresh write data lands (first successful chunk update).
    truncated_inodes: Arc<dashmap::DashSet<u64>>,
    /// Inodes for which setattr just stamped an explicit mtime (utimes/utimensat)
    /// that hasn't yet been picked up by a flush. flush_buffer_async consumes
    /// (removes) this on its next run for the inode and skips its own
    /// modified_at=now() stamp, so a pending chunk flush can't clobber the
    /// explicit mtime the user just set (rsync -a temp-file restore pattern).
    explicit_mtime_pending: Arc<dashmap::DashSet<u64>>,
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
    write_tasks_in_flight: Arc<DashMap<(u64, u64), Arc<std::sync::atomic::AtomicUsize>>>,
    /// Per-inode mutex that serialises concurrent flush_all_pipelined calls for the same
    /// inode. Without this, two sync_release handlers closing in quick succession can both
    /// enter flush_all_pipelined concurrently, race on slot ownership, and produce
    /// out-of-order patches on the server (T22b / qcow2 header corruption).
    flush_pipeline_locks: Arc<DashMap<u64, Arc<tokio::sync::Mutex<()>>>>,
    /// When true, MultiPatch writes to only 2 replicas (leader-preferred, deterministic).
    /// flush_all_pipelined enables this for fsync/release flushes. Background ticker
    /// leaves this false.
    use_dual_rf: bool,
    /// Per-inode count of active write-mode open file descriptors. Used by
    /// flush_all_pipelined to skip flush_metadata_sync when a writer still has
    /// the file open — metadata_cache may only partially reflect the current
    /// session (e.g., mid-way through a 300MB write, only some chunks updated).
    write_open_counts: Arc<DashMap<u64, usize>>,
    /// Per-server prefetch hints for the current flush wave, populated ONCE per wave
    /// by flush_all_pipelined before tasks are spawned. Tasks read by cloning the inner
    /// Arc — one atomic increment, no HashMap clone, no Mutex contention under load.
    /// Always empty for background-ticker flushes (single chunk, nothing to hint about).
    patch_prefetch_hints: Arc<std::sync::Mutex<Arc<HashMap<SocketAddr, Vec<dfs_common::ChunkId>>>>>,
}

/// Cheaply-cloneable handle for graceful-shutdown buffer draining. Built from a
/// FlushHandle plus the couple of fields destroy() needs that FlushHandle doesn't
/// carry. Exists so main.rs can capture it before the filesystem is moved into
/// spawn_mount2, letting a signal handler drain buffers independently of whether
/// the FUSE unmount (and thus destroy()) ever actually runs.
#[derive(Clone)]
pub struct ShutdownHandle {
    flush_handle: FlushHandle,
    release_in_flight: Arc<DashMap<u64, Arc<std::sync::atomic::AtomicUsize>>>,
    written_inodes: Arc<dashmap::DashSet<u64>>,
}

impl ShutdownHandle {
    /// Drains all write buffers and commits pending metadata. Idempotent — safe to
    /// call even if destroy() already ran (e.g. unmount succeeded before a signal
    /// arrived); buffers are already empty so it's a fast no-op.
    pub async fn drain(&self) {
        let write_buffers = self.flush_handle.write_buffers.clone();
        let flush_in_flight = self.flush_handle.flush_in_flight.clone();
        let client = self.flush_handle.client.clone();
        let metadata_cache = self.flush_handle.metadata_cache.clone();
        let release_in_flight = self.release_in_flight.clone();
        let written_inodes = self.written_inodes.clone();
        let flush_handle = self.flush_handle.clone();

        // Step 0: Wait for any in-flight release() flush tasks to complete.
        // release() spawns async tasks that aren't tracked by flush_in_flight.
        // Without this wait, a release flush that started just before shutdown
        // may be interrupted mid-write, losing the final metadata commit.
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
        loop {
            let total: usize = release_in_flight.iter()
                .map(|entry| entry.value().load(std::sync::atomic::Ordering::Relaxed))
                .sum();
            if total == 0 { break; }
            if tokio::time::Instant::now() > deadline {
                warn!("shutdown drain: timed out waiting for release tasks");
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }

        // Step 1: Force-flush all dirty write buffers (catches buffers not yet
        // picked up by a release task, e.g. files open when shutdown began).
        //
        // Bounded the same way Steps 0/2 already are (added 2026-07-15): each
        // flush_all_pipelined() call does real network I/O to whichever nodes hold
        // this inode's chunks, with no timeout of its own. If a peer is genuinely
        // unreachable (e.g. mid-restart — precisely the scenario several
        // restart-heavy local-suite tests exercise), that .await never returns,
        // this loop never finishes, drain() never returns, and the SIGTERM handler
        // that's supposed to let this process exit promptly hangs forever instead —
        // confirmed live as dfs-client processes that ignored SIGTERM entirely and
        // needed SIGKILL, sitting well past when they should have exited. A 30s
        // cap here (same value as Steps 0/2) means a hung peer costs one bounded
        // wait instead of an indefinite hang; whatever didn't finish flushing is
        // simply not durable on exit, the same accepted risk an unreachable peer
        // already poses to Steps 0/2.
        let inodes: Vec<u64> = write_buffers.iter().map(|e| *e.key()).collect();
        if !inodes.is_empty() {
            info!("shutdown drain: force-flushing {} open write buffers", inodes.len());
            let handles: Vec<_> = inodes.into_iter().map(|ino| {
                let h = flush_handle.clone();
                let flush_rt = h.flush_runtime.clone();
                flush_rt.spawn(async move {
                    if let Err(e) = h.flush_all_pipelined(ino).await {
                        error!("shutdown drain: flush failed for inode {}: {}", ino, e);
                    }
                })
            }).collect();
            let joined = futures::future::join_all(handles);
            if tokio::time::timeout(tokio::time::Duration::from_secs(30), joined).await.is_err() {
                warn!("shutdown drain: timed out waiting for force-flushed write buffers");
            }
        }

        // Step 2: Wait for any background in-flight flushes to drain.
        let in_flight = flush_in_flight.read().unwrap().clone();
        if let Some(in_flight) = in_flight {
            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
            while !in_flight.is_empty() {
                if tokio::time::Instant::now() > deadline {
                    warn!("shutdown drain: timed out waiting for in-flight flushes");
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
            // Only flush metadata for inodes that were actively written this session.
            // Flushing read-only files whose metadata_cache was populated solely by
            // warmup would commit chunks with a higher write_seq, reinforcing any
            // DB corruption already present on the server.
            let to_commit: Vec<_> = metadata_cache.iter()
                .filter(|e| !e.chunk_locations.is_empty() && written_inodes.contains(e.key()))
                .map(|e| e.clone())
                .collect();
            if !to_commit.is_empty() {
                info!("shutdown drain: committing metadata for {} written inodes with chunks", to_commit.len());
                // Bounded the same way Steps 0/1/2 are (added 2026-07-15) — see
                // Step 1's comment for the hung-SIGTERM-handler incident this
                // closes. flush_metadata_sync does real network I/O per call with
                // no timeout of its own; a per-item cap plus an overall deadline
                // means one or even several unreachable peers cost a bounded wait
                // instead of blocking every other pending commit (and thus this
                // whole drain, and thus process exit) indefinitely.
                let overall_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
                for meta in to_commit {
                    if tokio::time::Instant::now() > overall_deadline {
                        warn!("shutdown drain: timed out committing remaining written-inode metadata");
                        break;
                    }
                    if tokio::time::timeout(tokio::time::Duration::from_secs(5), client.flush_metadata_sync(&meta)).await.is_err() {
                        warn!("shutdown drain: metadata commit for {} timed out", meta.path);
                    }
                }
            }
        }

        info!("shutdown drain: all buffers flushed and metadata committed");
    }
}

/// Splice a freshly-confirmed chunk location into a file's chunk_locations list,
/// replacing any existing entry for the same chunk_idx (file_offset / CHUNK_SIZE).
///
/// Must match by chunk_idx, not exact file_offset or "chunk_id present anywhere":
/// the same confirmed location can get spliced more than once (a client-side retry
/// whose first attempt actually succeeded server-side, or two flush paths racing on
/// the same inode) and a check that's too narrow lets the second splice insert a
/// byte-for-byte duplicate entry instead of recognizing it as the same slot — this
/// was observed live on staging as exact-duplicate rows in dfs-admin file info,
/// every field identical, immediately after a fresh chunk_idx RCL-matching fix
/// closed off the other half of this bug class on the server.
fn splice_chunk_location(
    chunk_locations: &mut Vec<dfs_common::ChunkLocation>,
    loc: dfs_common::ChunkLocation,
    client: &Arc<DfsClient>,
) {
    // chunk_locations is sorted by file_offset. Use binary search instead of linear
    // scan — O(log n) vs O(n) per flush.
    //
    // A file_offset:None `loc` carries no reliable position in the current
    // chunk_idx-keyed model (every real write path sets file_offset) and must be
    // dropped, not appended: appending would violate binary_search_by's
    // sortedness assumption below for every future splice on this list, and a
    // stray None-offset entry that happens to share a chunk_id with a real,
    // positioned entry elsewhere in the list can later collide with and clobber
    // it during a server-side merge (root-caused live via T48/T22's intermittent
    // chunk-count and patched-region corruption under full-suite concurrent
    // load — see merge_file_metadata's matching guard on the server side, fixed
    // the same way). Symmetric with update_chunk_map_window's read-side handling
    // of the identical case.
    let Some(offset) = loc.file_offset else { return };
    match chunk_locations.binary_search_by(|l| {
        l.file_offset.unwrap_or(u64::MAX).cmp(&offset)
    }) {
        Ok(pos) => {
            let old_cid = chunk_locations[pos].chunk_id;
            if old_cid != loc.chunk_id {
                let client = client.clone();
                tokio::spawn(async move {
                    let _ = client.chunk_cache.remove(&old_cid);
                });
            }
            chunk_locations[pos] = loc;
        }
        Err(insert_pos) => {
            chunk_locations.insert(insert_pos, loc);
        }
    }
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
        // Arc<Vec<u8>>, not Vec<u8>: each slot is up to CHUNK_SIZE (4MB) and gets cloned
        // again below to move into its tokio::spawn task — Arc makes that a refcount
        // bump instead of a second full-buffer copy.
        let mut slots_to_write: Vec<(u64, Arc<Vec<u8>>, u64)> = Vec::new(); // (chunk_idx, data, file_offset)
        let mut patch_metadata_dirty = false; // true if any PatchChunk succeeded (needs metadata flush)
        // Every chunk location that changed in this flush cycle — populated both by
        // successful PatchChunks below (which mutate metadata_cache in place, so their new
        // location is pushed here explicitly) and by fresh-write results further down (via
        // .extend(locations)). This is the single list fed to the read engine and spliced
        // into metadata_cache, so both patched and freshly-written chunks are reflected.
        let mut all_locations: Vec<dfs_common::ChunkLocation> = Vec::new();
        for chunk_idx in &indices_to_flush {
            let Some(state_lock) = self.write_buffers.get(&ino) else { continue };
            // Snapshot slot data and drop the mutex before any network I/O.
            // Holding a tokio Mutex across an .await blocks concurrent getattr/read/write
            // on the same inode, causing observable stalls when reading while recording.
            let (slot_data, file_offset) = {
                let state = state_lock.lock().await;
                match state.slots.get(chunk_idx) {
                    Some(slot) if !slot.is_empty() =>
                        (slot.materialize(), chunk_idx * CHUNK_SIZE as u64),
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
            let (gap_filled_prefix, real_data_end, dirty_ranges_snap) = {
                let state_lock = self.write_buffers.get(&ino);
                state_lock.and_then(|s| s.try_lock().ok()
                    .and_then(|st| st.slots.get(&chunk_idx).map(|sl| (sl.gap_filled_prefix, sl.real_data_end, sl.dirty_ranges.clone()))))
                    .unwrap_or((0, 0, Vec::new()))
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
                // For in-place overwrites, if dirty_ranges has multiple non-adjacent entries,
                // the gap bytes between them are zeros in the slot buffer but hold real data
                // on the server. A single-range patch (gap_filled_prefix..effective_write_end)
                // would include those zeros and corrupt the server (e.g. two writes to the
                // same chunk at offsets 0 and 1MB — the gap at 4KB..1MB gets zeroed).
                // mark_dirty() merges adjacent ranges, so len > 1 always implies real gaps.
                // Delegate to flush_buffer_async_one which uses MultiPatch for these cases.
                if is_overwrite && dirty_ranges_snap.len() > 1 {
                    info!("flush_buffer_async: slot {} is_overwrite with {} sparse dirty ranges — deferring to MultiPatch path",
                          chunk_idx, dirty_ranges_snap.len());
                    slots_to_write.push((*chunk_idx, Arc::new(slot_data), file_offset));
                    continue;
                }

                let (patch_intra, patch_bytes) = if is_append_extend {
                    // Send only the new appended bytes, starting at gap_filled_prefix (not
                    // existing_chunk_size). When gap_filled_prefix > existing_chunk_size (e.g.
                    // a prior flush wrote 446 bytes, but the next write starts at byte 1MB),
                    // using existing_chunk_size would include the zero-filled gap 446..1MB
                    // and overwrite real server data (GPT entries, partition headers, etc.).
                    // When gap_filled_prefix == existing_chunk_size (normal DVR append),
                    // behavior is unchanged.
                    (gap_filled_prefix, slot_data[gap_filled_prefix..].to_vec())
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
                    let file_id_legacy = meta.id;
                    // Check recent_chunk_writes before using metadata_cache location.
                    let old_location_opt = {
                        let cached_loc = meta.chunk_location_for_idx(*chunk_idx).cloned();
                        let recent = self.client.recent_chunk_writes.get(&(ino, *chunk_idx))
                            .filter(|r| {
                                let (_, fid, ts, _) = r.value();
                                *fid == meta.id && ts.elapsed().as_secs() < 120
                            })
                            .map(|r| {
                                let (cid, _, _, nodes) = r.value();
                                (*cid, nodes.clone())
                            });
                        match (cached_loc, recent) {
                            (Some(mut loc), Some((recent_id, recent_nodes))) if recent_id != loc.chunk_id => {
                                loc.chunk_id = recent_id;
                                loc.checksum = recent_id.hash;
                                if !recent_nodes.is_empty() {
                                    loc.nodes = recent_nodes;
                                }
                                Some(loc)
                            }
                            (loc, _) => loc,
                        }
                    };
                    // Enforce sorted-first-2: always patch the canonical pair of nodes (sorted
                    // by NodeId, lowest two). The 3rd replica is healer-owned — we never target
                    // it directly, so we can never get a stale-base from a healer-added node.
                    // After the patch, the broadcast-delete tombstones the old chunk on every
                    // node (including the 3rd) and the metadata update drops it from the map.
                    let old_location_opt = old_location_opt.map(|mut loc| {
                        if loc.nodes.len() > 2 {
                            loc.nodes.sort_unstable();
                            loc.nodes.truncate(2);
                        }
                        loc
                    });
                    if let Some(old_location) = old_location_opt {
                        // Note: the "stale-write guard" that used to live here (discard if
                        // chunk_id differed from an open()-time snapshot) is removed — see
                        // flush_buffer_async_one's equivalent comment for why: patch_bytes
                        // below is sliced purely from this session's own written bytes
                        // (gap_filled_prefix..effective_write_end), never server-read
                        // content, and old_location.chunk_id is already correctly resolved
                        // above. Genuine staleness is handled by the server's ChunkStale
                        // validation and the _verified retry path just below.
                        // Serialize this chunk's patch+metadata update with any concurrent
                        // write to the same (ino, chunk_idx) from any other path.
                        let _chunk_guard = DfsFilesystem::lock_chunk(&self.chunk_write_locks, ino, *chunk_idx).await;
                        // Pass file_id + chunk_idx so the server validates the chunk_id
                        // against its chunk map and returns ChunkStale if it's stale.
                        // patch_chunk_on_replicas_verified retries once automatically with
                        // the corrected chunk_id — no separate leader round-trip needed.
                        let patch_result = self.client.patch_chunk_on_replicas_verified(
                            old_location.chunk_id,
                            file_id_legacy,
                            *chunk_idx,
                            file_offset,
                            patch_intra,
                            patch_bytes.clone(),
                            &old_location,
                        ).await;

                        let patch_result = match patch_result {
                            Ok(loc) => Ok(loc),
                            Err(e) => {
                                // PatchChunk failed even after ChunkStale retry — fall back to
                                // fetching the single chunk location from the leader explicitly.
                                warn!("flush_buffer_async: PatchChunk failed for slot {} ({}), fetching single chunk location", chunk_idx, e);
                                match self.client.get_single_chunk_location(file_id_legacy, *chunk_idx).await {
                                    Ok(Some(mut fresh_loc)) if fresh_loc.chunk_id != old_location.chunk_id => {
                                        info!("flush_buffer_async: retrying PatchChunk slot {} with fresh location {} (was {})",
                                              chunk_idx, fresh_loc.chunk_id, old_location.chunk_id);
                                        if fresh_loc.nodes.len() > 2 { fresh_loc.nodes.sort_unstable(); fresh_loc.nodes.truncate(2); }
                                        if let Some(mut meta_entry) = self.metadata_cache.get_mut(&ino) {
                                            if let Some(existing) = meta_entry.chunk_location_for_idx_mut(*chunk_idx) {
                                                *existing = fresh_loc.clone();
                                            }
                                        }
                                        self.client.patch_chunk_on_replicas_verified(
                                            fresh_loc.chunk_id,
                                            file_id_legacy,
                                            *chunk_idx,
                                            file_offset,
                                            patch_intra,
                                            patch_bytes.clone(),
                                            &fresh_loc,
                                        ).await
                                    }
                                    Ok(Some(mut loc)) => {
                                        if loc.nodes.len() > 2 { loc.nodes.sort_unstable(); loc.nodes.truncate(2); }
                                        self.client.patch_chunk_on_replicas_verified(
                                            loc.chunk_id, file_id_legacy, *chunk_idx,
                                            file_offset, patch_intra, patch_bytes.clone(), &loc,
                                        ).await
                                    }
                                    Ok(None) | Err(_) => Err(e),
                                }
                            }
                        };

                        match patch_result {
                            Ok(new_location) => {
                                info!("flush_buffer_async: PatchChunk slot {} succeeded: {} -> {}",
                                      chunk_idx, old_location.chunk_id, new_location.chunk_id);
                                // Record the new chunk_id for fast lookup on the next write.
                                self.client.recent_chunk_writes.insert(
                                    (ino, *chunk_idx),
                                    (new_location.chunk_id, file_id_legacy, std::time::Instant::now(), new_location.nodes.clone()),
                                );
                                // Update slot.server_chunk_id so a concurrent flush_buffer_async_one
                                // for this chunk (background ticker) sees this patch's result as its
                                // base, instead of a stale metadata_cache/recent_chunk_writes snapshot
                                // taken before this patch completed.
                                if let Some(state_arc) = self.write_buffers.get(&ino) {
                                    let mut state = state_arc.lock().await;
                                    if let Some(slot) = state.slots.get_mut(chunk_idx) {
                                        slot.server_chunk_id = Some(new_location.chunk_id);
                                    }
                                }
                                // Evict the old chunk_id — the file at that hash path has been
                                // renamed away on the server. Any cached entry for it would cause
                                // an I/O error on the next read. The new chunk_id will be fetched
                                // fresh (or will be in cache if seeded by the release() pre-seed).
                                if old_location.chunk_id != new_location.chunk_id {
                                    let client = self.client.clone();
                                    let old_cid = old_location.chunk_id;
                                    tokio::spawn(async move {
                                        { let _ = client.chunk_cache.remove(&old_cid); };
                                    });
                                }
                                if let Some(mut meta_entry) = self.metadata_cache.get_mut(&ino) {
                                    if let Some(loc) = meta_entry.chunk_location_for_idx_mut(*chunk_idx) {
                                        *loc = new_location.clone();
                                    }
                                    if let Some(end) = new_location.file_offset.map(|o| o + new_location.size as u64) {
                                        meta_entry.size = meta_entry.size.max(end);
                                    }
                                }
                                all_locations.push(new_location.clone());
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
                        // chunk_location missing in local cache — fetch just this slot from the leader.
                        warn!("flush_buffer_async: no chunk_location for slot {} in local cache — fetching single chunk location", chunk_idx);
                        let mut refreshed = false;
                        match self.client.get_single_chunk_location(file_id_legacy, *chunk_idx).await {
                            Ok(Some(old_location)) => {
                                info!("flush_buffer_async: retrying PatchChunk slot {} after single-chunk fetch (loc={})", chunk_idx, old_location.chunk_id);
                                if let Some(mut meta_entry) = self.metadata_cache.get_mut(&ino) {
                                    if let Some(existing) = meta_entry.chunk_location_for_idx_mut(*chunk_idx) {
                                        *existing = old_location.clone();
                                    }
                                }
                                let retry = self.client.patch_chunk_on_replicas_verified(
                                    old_location.chunk_id,
                                    file_id_legacy,
                                    *chunk_idx,
                                    file_offset,
                                    patch_intra,
                                    patch_bytes.clone(),
                                    &old_location,
                                ).await;
                                match retry {
                                    Ok(new_location) => {
                                        info!("flush_buffer_async: PatchChunk slot {} succeeded after single-chunk fetch: {} -> {}",
                                              chunk_idx, old_location.chunk_id, new_location.chunk_id);
                                        self.client.recent_chunk_writes.insert(
                                            (ino, *chunk_idx),
                                            (new_location.chunk_id, file_id_legacy, std::time::Instant::now(), new_location.nodes.clone()),
                                        );
                                        if let Some(mut meta_entry) = self.metadata_cache.get_mut(&ino) {
                                            if let Some(loc) = meta_entry.chunk_location_for_idx_mut(*chunk_idx) {
                                                *loc = new_location.clone();
                                            }
                                        }
                                        all_locations.push(new_location.clone());
                                        patch_metadata_dirty = true;
                                        refreshed = true;
                                    }
                                    Err(e) => {
                                        warn!("flush_buffer_async: PatchChunk slot {} failed after single-chunk fetch: {}", chunk_idx, e);
                                    }
                                }
                            }
                            Ok(None) => {
                                warn!("flush_buffer_async: slot {} not found on leader — treating as new chunk", chunk_idx);
                            }
                            Err(e) => {
                                warn!("flush_buffer_async: single-chunk fetch failed for slot {}: {}", chunk_idx, e);
                            }
                        }
                        refreshed
                    }
                } else {
                    false
                };

                if !patched {
                    slots_to_write.push((*chunk_idx, Arc::new(slot_data), file_offset));
                }
            } else {
                slots_to_write.push((*chunk_idx, Arc::new(slot_data), file_offset));
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
        let file_id = self.metadata_cache.get(&ino).map(|m| m.id).unwrap_or_else(dfs_common::FileId::new);
        let handles: Vec<_> = slots_to_write.iter().map(|(chunk_idx, slot_data, file_offset)| {
            let client = self.client.clone();
            let data = Arc::clone(slot_data);
            let offset = *file_offset;
            let idx = *chunk_idx;
            tokio::spawn(async move {
                info!("flush_buffer_async: writing chunk {} ({} bytes at offset {})", idx, data.len(), offset);
                let result = client.write_data_with_cache(data.as_slice(), ino, offset, file_id, None).await;
                result.map(|(_, _, locs)| (idx, locs))
            })
        }).collect();

        let t_net_start = std::time::Instant::now();
        let results = futures::future::join_all(handles).await;
        let t_net = t_net_start.elapsed();

        // Process results: track flushed sizes and collect locations.
        // DO NOT remove slots yet - wait until read engine is updated to avoid race.
        // (all_locations itself is declared earlier, alongside patch_metadata_dirty, so
        // successful PatchChunks above can push their new location into the same list.)
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
            let path_opt = self.inode_to_path.read().unwrap().get(&ino).cloned();
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
                // File size = max of logical size (set by truncate) and physical chunk end.
                // A sparse file grown via truncate has a logical size larger than its written
                // chunks; clobbering with the physical end would shrink the reported size.
                // meta.size already tracks all prior chunk ends — we only need to extend it for
                // the chunks we're splicing in now, avoiding an O(n) scan over all chunks.
                for loc in &all_locations {
                    splice_chunk_location(Arc::make_mut(&mut meta.chunk_locations), loc.clone(), &self.client);
                    if let Some(end) = loc.file_offset.map(|o| o + loc.size as u64) {
                        meta.size = meta.size.max(end);
                    }
                }
                // Don't clobber an mtime the user explicitly just set via setattr
                // (utimes/utimensat) — e.g. rsync -a's temp-file restore can land
                // before this flush completes (T37).
                // Use contains() not remove(): two concurrent flush tasks for the same
                // inode both run this check, and remove() would let the second task see
                // None and stamp now(), clobbering the explicit mtime. The flag is
                // cleared only in write() when new data arrives.
                if !self.explicit_mtime_pending.contains(&ino) {
                    meta.modified_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                }
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
                    DfsFilesystem::invalidate_dir_cache(&self.dir_cache, &self.dir_cache_invalidated_at, &parent);
                }
            }

            if force {
                // release/fsync: commit metadata to leader, THEN update read engine.
                // Ordering matters: readers must not see chunk IDs before the leader
                // has them — otherwise a leader refresh overwrites our engine update
                // with stale data and readers get the wrong chunks.
                //
                // Send a network-lightweight clone: same scalar fields (path/size/mtime/
                // etc), but chunk_locations replaced with just this cycle's newly-flushed
                // locations (all_locations) instead of the full, ever-growing history.
                // handle_put_file_metadata's existing chunk_map-union reconcile already
                // treats the incoming list as potentially non-cumulative and fills in
                // every other chunk from its authoritative chunk_map (see its own comment:
                // "concurrent per-chunk flushes ... race to send their own non-cumulative
                // chunk_locations snapshot") — the full-history send here was pure
                // unnecessary cost: full clone, full bincode serialize, full network
                // transfer, and a full O(n) reconcile pass, repeated on every single
                // fsync/release regardless of how many chunks actually changed. Patching
                // a large file and fsyncing after each patch (confirmed via a dedicated
                // patch-timing test) showed this cost scaling directly with total file
                // size, uniformly regardless of where the patch landed.
                let mut meta_for_network = meta.clone();
                meta_for_network.chunk_locations = Arc::new(all_locations.clone());
                let t_meta_sync = std::time::Instant::now();
                debug!("[STATE-DIAG] ino={} metadata_cache_len={} write_buffers_len={} dir_cache_dirs={}",
                    ino, self.metadata_cache.len(), self.write_buffers.len(), self.dir_cache.len());
                self.client.flush_metadata_sync(&meta_for_network).await;
                debug!("[STATE-DIAG] ino={} flush_metadata_sync took {:?}", ino, t_meta_sync.elapsed());
                self.last_metadata_update.insert(ino, std::time::Instant::now());
                // Now the leader has the metadata — safe to populate the read engine.
                // Feed only the locations just flushed (all_locations), not the whole
                // (ever-growing) meta.chunk_locations — the read engine is incremental by
                // design (see feed_chunk_locations_to_read_engine's doc comment: callers
                // pass one location at a time), and passing the full list here made every
                // flush clone and rebuild a window over the entire file's chunk history:
                // O(n) work per flush, O(n^2) over a large sequential write.
                let current_size = meta.size;
                self.client.feed_chunk_locations_to_read_engine(
                    ino, &all_locations, current_size,
                ).await;
            } else {
                // Background tick: push directly into the queue (no back-pressure wait).
                // enqueue_metadata() may block waiting to rescue a stalled queue entry,
                // which would hold the in_flight slot and prevent new background flushes
                // from starting — starving the write pipeline. We stamp the seq and push
                // directly; the queue worker handles delivery and retries independently.
                //
                // Throttled to at most once per BG_METADATA_PUSH_INTERVAL, AND — like the
                // force/fsync branch above — sends only this cycle's newly-flushed locations
                // (all_locations), not the full, ever-growing meta.chunk_locations. This used
                // to send the *entire* Vec (every chunk written so far): the leader's chunk_map
                // is already kept current per-chunk by the cheap ReplicateChunkLocation(s) sent
                // during the chunk write itself (write_data_dual_replica), and
                // handle_put_file_metadata's reconcile step already unions a non-cumulative
                // incoming list against chunk_map before storing — so sending the full history
                // here was pure unnecessary cost (client-side clone/serialize, network transfer,
                // and server-side incoming-deserialize all scaling with total file size), on top
                // of the reconcile/chunk_map_update/put_file cost that scales with size
                // regardless. A real large-file workload (VM disk install) showed this specific
                // path's round-trip latency climbing from ~1.5ms to ~40-49ms as chunk_locations
                // grew past ~1300 entries, collapsing write throughput to a fraction of its
                // earlier rate for the rest of the session. all_locations is guaranteed
                // non-empty here — patch_metadata_dirty is only ever set alongside a matching
                // all_locations.push(), and this branch is unreachable unless the earlier
                // `all_locations.is_empty() && !patch_metadata_dirty` check passed — so this
                // can never be misread by the server as an intentional truncate-to-zero (empty
                // chunk_locations skips its chunk_map union entirely; see
                // handle_put_file_metadata's reconcile comment).
                const BG_METADATA_PUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
                let should_push = self.last_bg_metadata_push.get(&ino)
                    .map(|last| last.elapsed() >= BG_METADATA_PUSH_INTERVAL)
                    .unwrap_or(true);
                if should_push {
                    self.last_bg_metadata_push.insert(ino, std::time::Instant::now());
                    self.last_metadata_update.insert(ino, std::time::Instant::now());
                    // The leader flagged a genuine write_seq gap for this file
                    // (Response::ResyncMetadataRequested — see DfsClient::pending_resync's
                    // doc comment). Send the full cumulative metadata_cache snapshot
                    // instead of just this cycle's delta so the leader's chunk_locations
                    // converge — it already has everything this delta would have carried,
                    // plus whatever it was missing. Debounced independently of
                    // BG_METADATA_PUSH_INTERVAL — see last_resync_sent_at's doc comment:
                    // a full snapshot's cost scales with the file's total chunk count, not
                    // a delta's, so repeated false-positive gap flags must not each pay
                    // that cost. Doesn't clear pending_resync when debounced, so it's
                    // retried once the cooldown passes; the normal delta push below still
                    // goes out this tick either way, so writes keep progressing.
                    const RESYNC_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(30);
                    let now = std::time::Instant::now();
                    let resync_ready = self.client.pending_resync.contains(&meta.id)
                        && DfsClient::should_send_resync_snapshot(
                            self.client.last_resync_sent_at.get(&meta.id).map(|t| *t),
                            now, RESYNC_DEBOUNCE,
                        );
                    if resync_ready {
                        self.client.pending_resync.remove(&meta.id);
                        self.client.last_resync_sent_at.insert(meta.id, now);
                        let stamped = self.client.stamp_write_seq_pub(&meta);
                        self.client.metadata_queue.push_full_snapshot(stamped).await;
                    } else {
                        let mut meta_for_network = meta.clone();
                        meta_for_network.chunk_locations = Arc::new(all_locations.clone());
                        let stamped = self.client.stamp_write_seq_pub(&meta_for_network);
                        self.client.metadata_queue.push(stamped).await;
                    }
                }
                // For background flushes, update read engine immediately (not queued)
                // so reads see fresh chunk_map before slots are removed below. Always runs,
                // even when the metadata push above is throttled — this only updates this
                // client's own local read cache, not the network. Feed only all_locations
                // (this cycle's newly flushed chunks) — see the force branch's comment above
                // for why passing the full meta.chunk_locations here was an O(n) per flush.
                let current_size = meta.size;
                self.client.feed_chunk_locations_to_read_engine(
                    ino, &all_locations, current_size,
                ).await;
            }
        }

        // Now that read engine is updated, safe to remove flushed slots.
        // This ordering prevents race where reads fall through to network with stale chunk_map.
        if !flushed_chunks.is_empty() {
            if let Some(state_lock) = self.write_buffers.get(&ino) {
                let mut state = state_lock.lock().await;
                for (chunk_idx, _flushed_len) in flushed_chunks {
                    // Subtract the slot's actual resident size (including gap-fill padding),
                    // not flushed_len (dirty bytes only) — the counter is now maintained in
                    // resident-byte terms (see resident_bytes()' doc comment), so removing a
                    // slot must free its full allocation from the tally or the counter drifts
                    // upward forever and eventually wedges all writes under back-pressure.
                    let removed_size = state.slots.remove(&chunk_idx).map(|s| s.resident()).unwrap_or(0);
                    self.global_buffered_bytes.fetch_sub(
                        removed_size.min(self.global_buffered_bytes.load(std::sync::atomic::Ordering::Relaxed)),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
            }
        }

        info!("flush ino={} complete | preseed={:?} net={:?} meta={:?} total={:?}",
              ino, t_preseed, t_net, t_meta_start.elapsed(), t_flush_start.elapsed());
        Ok(())
    }

    /// Flush exactly one chunk slot for `ino`.
    ///
    /// `urgent=false` (background ticker): only full or idle slots — avoids dispatching
    /// tiny dirty ranges before they've had a chance to accumulate into larger batches.
    ///
    /// `urgent=true` (flush_all_pipelined / fsync / release): flush ANY non-empty slot.
    /// FAP holds flush_pipeline_locks, which prevents the ticker from running for this
    /// inode. Without the urgent fallback, FAP would spin forever dispatching tasks that
    /// return Ok(()) on no-full/no-idle slots while the pipeline lock blocks the ticker
    /// from ever draining them via the stale path.
    async fn flush_one_chunk(&self, ino: u64, urgent: bool) -> Result<()> {
        // Pick the lowest-index unclaimed full slot, falling back to the lowest idle slot,
        // and finally (urgent=true only) to any non-empty slot.
        // Atomically set flushing=true while holding the mutex so no second concurrent
        // task can claim the same slot.
        let (chunk_idx, mut slot_data, file_offset, gap_filled_prefix, real_data_end, dirty_ranges, last_modified_snap) = {
            let Some(state_arc) = self.write_buffers.get(&ino) else { return Ok(()); };
            // LOCKTIMING: write_buffers per-inode Mutex wait — see SCHEDTIMING's doc
            // comment for the broader instrumentation pass this belongs to.
            let lock_wait_start = std::time::Instant::now();
            let mut state = state_arc.lock().await;
            let lock_wait_ms = lock_wait_start.elapsed().as_secs_f64() * 1000.0;
            if lock_wait_ms > 5.0 {
                info!("LOCKTIMING write_buffers ino={} lock_wait_ms={:.1}", ino, lock_wait_ms);
            }

            // Full slots first (lowest index, not already claimed, not already on server).
            // Backoff only applies when !urgent — the ticker's opportunistic path should
            // skip a slot that just failed terminally rather than hammer it every tick,
            // but fsync/release (urgent=true) must still see and flush it: the caller is
            // waiting synchronously and needs the real outcome, not a silent no-op.
            let mut full: Vec<u64> = state.slots.iter()
                .filter(|(_, s)| s.is_full() && !s.flushing && (urgent || !s.in_backoff()))
                .map(|(idx, _)| *idx)
                .collect();
            full.sort_unstable();

            let idx = if let Some(i) = full.into_iter().next() {
                i
            } else {
                // No full unclaimed slot — try the oldest eligible partial slot. Eligible
                // means idle (quiet for a while), OR abandoned (a write moved to a
                // different chunk — an unambiguous "done with this one" event, safe
                // regardless of is_idle()'s timing — see `abandoned`'s doc comment), OR
                // over the per-chunk dirty-byte threshold (a slot that keeps getting
                // revisited so never goes idle or gets abandoned, but has accumulated
                // enough real dirty data to be worth flushing on its own).
                let mut idle: Vec<(u64, SystemTime)> = state.slots.iter()
                    .filter(|(_, s)| {
                        (s.is_idle() || s.abandoned
                            || (s.is_fragmented() && s.dirty_bytes() >= SLOT_DIRTY_FLUSH_THRESHOLD_BYTES))
                        && !s.is_empty() && !s.flushing && (urgent || !s.in_backoff())
                    })
                    .map(|(idx, s)| (*idx, s.last_modified))
                    .collect();
                idle.sort_by_key(|&(_, t)| t);
                match idle.into_iter().next() {
                    Some((i, _)) => i,
                    None => {
                        if urgent {
                            // Urgent path: flush any non-empty slot regardless of age or
                            // dirty coverage. Required during FAP (fsync/release) because
                            // the pipeline lock prevents the background ticker from running
                            // its own stale-flush path for this inode.
                            let mut any: Vec<u64> = state.slots.iter()
                                .filter(|(_, s)| !s.is_empty() && !s.flushing)
                                .map(|(idx, _)| *idx)
                                .collect();
                            any.sort_unstable();
                            match any.into_iter().next() {
                                Some(i) => i,
                                None => return Ok(()),
                            }
                        } else {
                            return Ok(());
                        }
                    }
                }
            };

            // FIFO ordering for overlapping chunks: wait for any in-flight flush for this chunk
            // to complete its metadata commit before starting a new flush. This prevents the
            // "stale chunk_id" race where two flushes both read the old chunk_id before either
            // completes, causing the second to fail with "chunk_id is stale, server has X".
            //
            // CRITICAL: Must loop! When a flush completes, it wakes ALL waiters. They race to
            // acquire the lock; the first one starts a new flush with a new waiter. The second
            // must re-check and wait on the NEW waiter, not just proceed. Without the loop,
            // multiple tasks can start flushing concurrently after a single notify_waiters().
            loop {
                let waiter = state.chunk_flush_waiters.get(&idx).cloned();
                if let Some(notify) = waiter {
                    drop(state); // Release lock while waiting
                    debug!("flush_one_chunk: ino={} chunk={} waiting for in-flight flush", ino, idx);

                    // Wait for notification with a 30-second timeout. If the previous flush is stuck
                    // (server hang, network timeout, slow patch), we must not wait forever or we'll
                    // deadlock all writes to this chunk. On timeout, force-remove the stale waiter
                    // and let this flush fail gracefully - the retry logic will handle it.
                    match tokio::time::timeout(tokio::time::Duration::from_secs(30), notify.notified()).await {
                        Ok(_) => {
                            debug!("flush_one_chunk: ino={} chunk={} wait complete, re-checking", ino, idx);
                            state = state_arc.lock().await; // Re-acquire and loop to check again
                        }
                        Err(_) => {
                            warn!("flush_one_chunk: ino={} chunk={} FIFO wait timeout after 30s - previous flush stuck", ino, idx);
                            // Force-remove the stale waiter and fail this flush
                            let mut state = state_arc.lock().await;
                            state.chunk_flush_waiters.remove(&idx);
                            // Also clear the flushing flag so the next attempt can proceed
                            if let Some(slot) = state.slots.get_mut(&idx) {
                                slot.flushing = false;
                            }
                            return Err(anyhow::anyhow!("FIFO wait timeout - previous flush stuck for >30s"));
                        }
                    }
                } else {
                    // No waiter → we can proceed
                    break;
                }
            }

            // Now holding lock with no waiter present - claim the slot
            // Check if slot is still valid AND not currently flushing before claiming.
            // The !flushing check is critical: after a flush completes, it removes the waiter
            // and notifies waiting tasks BEFORE setting flushing=false. Without this check,
            // a woken task could re-claim the same slot while the previous flush is still
            // updating metadata/removing the slot, causing concurrent patches with stale chunk_ids.
            if !state.slots.get(&idx).map(|s| !s.is_empty() && !s.flushing).unwrap_or(false) {
                return Ok(());
            }

            // Create a new notifier for this flush so subsequent flushes will wait
            let notify = Arc::new(tokio::sync::Notify::new());
            state.chunk_flush_waiters.insert(idx, notify);

            // Now claim the slot
            let slot = state.slots.get_mut(&idx).expect("slot disappeared");
            slot.flushing = true;

            let data = slot.materialize();
            let last_modified_snap = slot.last_modified;
            let gap_filled_prefix = slot.gap_filled_prefix;
            let real_data_end = slot.real_data_end;
            let dirty_ranges = slot.dirty_ranges.clone();
            let offset = idx * CHUNK_SIZE as u64;
            (idx, data, offset, gap_filled_prefix, real_data_end, dirty_ranges, last_modified_snap)
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
        //
        // Keyed by (ino, chunk_idx), not just ino: this only needs to wait for writers
        // touching THIS chunk. Waiting on a per-inode counter (any write anywhere in the
        // file) collapsed concurrent random-write throughput once writes were made to
        // actually run concurrently (KDiskMark RND4K write went from ~900-1200 ops/sec to
        // ~30-45 ops/sec) — at QD32 scattered across many chunks, the per-inode counter
        // almost never reached 0 during a sustained burst, so this wait kept timing out
        // at its full 5s deadline for nearly every flush attempt.
        if gap_filled_prefix > 0 {
        // Clone the Arc and drop the DashMap Ref guard immediately — holding a
        // DashMap shard's lock across the .await points below (up to 5 real
        // seconds via the sleep loop) blocks any other thread that needs the same
        // shard for an unrelated (ino, chunk_idx) key (e.g. inc_write_tasks_for_chunks
        // incrementing a different chunk's counter), with no timeout of its own
        // since that's a plain std lock, not tokio-aware. Root cause of a real
        // deadlock hit during 2026-07-05 testing (dramatically more likely to
        // trigger once the `abandoned` fix raised flush frequency/concurrency) —
        // quite possibly the same mechanism behind the still-unexplained
        // server5/VM100 production deadlock, which showed an identical
        // all-threads-parked-on-futex_wait_queue signature.
        if let Some(counter) = self.write_tasks_in_flight.get(&(ino, chunk_idx)).map(|r| r.clone()) {
            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
            while counter.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                if tokio::time::Instant::now() > deadline {
                    warn!("flush_one_chunk: timed out waiting for write tasks for ino={} chunk={}", ino, chunk_idx);
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
            }
            // Re-snapshot the slot now that all writes have landed.
            if let Some(state_arc) = self.write_buffers.get(&ino) {
                let mut state = state_arc.lock().await;
                if let Some(slot) = state.slots.get_mut(&chunk_idx) {
                    let data = slot.materialize();
                    let last_modified_snap = slot.last_modified;
                    let gap_filled_prefix = slot.gap_filled_prefix;
                    let real_data_end = slot.real_data_end;
                    let dirty_ranges = slot.dirty_ranges.clone();
                    let file_offset = chunk_idx * CHUNK_SIZE as u64;
                    drop(state);
                    return self.flush_buffer_async_one(ino, chunk_idx, data, file_offset, gap_filled_prefix, real_data_end, dirty_ranges, last_modified_snap).await;
                }
                info!("flush_one_chunk: ino={} chunk={} slot gone after wait — already flushed elsewhere", ino, chunk_idx);
            }
        } // write_tasks_in_flight guard
        } // gap_filled_prefix guard

        self.flush_buffer_async_one(ino, chunk_idx, slot_data, file_offset, gap_filled_prefix, real_data_end, dirty_ranges, last_modified_snap).await
    }

    /// Notify any waiters for this chunk and remove the notifier.
    /// Called after metadata commit completes (success or failure) to unblock subsequent flushes.
    ///
    /// IMPORTANT: This does NOT clear the flushing flag. The caller must do additional cleanup
    /// (update read engine, remove/update slot) BEFORE clearing flushing. If we cleared flushing
    /// here, a woken task could immediately claim the slot while we're still processing it.
    async fn notify_chunk_flush_complete(&self, ino: u64, chunk_idx: u64) {
        if let Some(state_arc) = self.write_buffers.get(&ino) {
            let mut state = state_arc.lock().await;
            if let Some(notify) = state.chunk_flush_waiters.remove(&chunk_idx) {
                drop(state); // Release lock before notifying
                notify.notify_waiters();
                debug!("notify_chunk_flush_complete: ino={} chunk={} notified waiters", ino, chunk_idx);
            }
        }
    }

    /// Resolve which nodes currently hold the data for `chunk_idx` of `ino`, using the same
    /// priority as the patch path: canonical_write_nodes (this session's last successful
    /// write/patch) > recent_chunk_writes (matching file_id, < 120s old) > metadata_cache.
    /// Used to target full-chunk-replacement writes at the chunk's existing replica holders
    /// instead of re-deriving placement via capacity-band randomization.
    async fn existing_chunk_nodes(&self, ino: u64, chunk_idx: u64) -> Option<Vec<dfs_common::NodeId>> {
        if let Some(state_arc) = self.write_buffers.get(&ino) {
            if let Ok(st) = state_arc.try_lock() {
                if let Some(nodes) = st.canonical_write_nodes.get(&chunk_idx) {
                    if !nodes.is_empty() {
                        return Some(nodes.clone());
                    }
                }
            }
        }
        let meta_id = self.metadata_cache.get(&ino).map(|m| m.id);
        if let Some(fid) = meta_id {
            if let Some(r) = self.client.recent_chunk_writes.get(&(ino, chunk_idx)) {
                let (_, rfid, ts, nodes) = r.value();
                if *rfid == fid && ts.elapsed().as_secs() < 120 && !nodes.is_empty() {
                    return Some(nodes.clone());
                }
            }
        }
        self.metadata_cache.get(&ino)
            .and_then(|m| m.chunk_location_for_idx(chunk_idx).map(|l| l.nodes.clone()))
            .filter(|n| !n.is_empty())
    }

    /// Make a fresh write safe after MultiPatch failed for a chunk that already had real
    /// data on the server not fully covered by `dirty_ranges` (anything except
    /// `is_full_replacement`). A naive fresh write sends `slot_data` as the complete chunk
    /// content, zero-filling every byte outside `dirty_ranges` — silently destroying real
    /// content (e.g. qcow2 metadata clusters) that this session never touched, regardless
    /// of whether the untouched bytes are a leading gap, a mid-chunk gap, or scattered
    /// random-write holes. Reconstructs the untouched bytes from chunk_cache when possible;
    /// otherwise returns an error so the caller aborts instead of corrupting the chunk —
    /// the slot stays dirty and the next fsync retries cleanly.
    async fn reconstruct_or_abort_for_fresh_write(
        &self,
        ino: u64,
        chunk_idx: u64,
        slot_data: &mut Vec<u8>,
        dirty_ranges: &[(usize, usize)],
        candidate_ids: &[dfs_common::ChunkId],
    ) -> Result<()> {
        let base = candidate_ids.iter().find_map(|id| self.client.chunk_cache.get(id));
        if let Some(base_arc) = base {
            let mut reconstructed = (*base_arc).clone();
            if reconstructed.len() < slot_data.len() {
                reconstructed.resize(slot_data.len(), 0);
            }
            for &(start, end) in dirty_ranges {
                let e = end.min(slot_data.len()).min(reconstructed.len());
                if start < e {
                    reconstructed[start..e].copy_from_slice(&slot_data[start..e]);
                }
            }
            info!("flush_buffer_async_one: ino={} chunk={} reconstructed from cache — untouched chunk regions preserved, writing fresh",
                ino, chunk_idx);
            *slot_data = reconstructed;
            Ok(())
        } else {
            let mut backoff_secs = 0u64;
            if let Some(state_arc) = self.write_buffers.get(&ino) {
                let mut state = state_arc.lock().await;
                if let Some(slot) = state.slots.get_mut(&chunk_idx) {
                    slot.flushing = false;
                    slot.consecutive_patch_failures = 0;
                    slot.record_terminal_failure();
                    backoff_secs = slot.retry_backoff_until
                        .map(|t| t.saturating_duration_since(std::time::Instant::now()).as_secs())
                        .unwrap_or(0);
                }
            }
            warn!("flush_buffer_async_one: ino={} chunk={} cache miss — aborting to prevent zeroing untouched chunk regions \
                   (returning EIO, backing off retries for this chunk {}s)",
                ino, chunk_idx, backoff_secs);
            self.notify_chunk_flush_complete(ino, chunk_idx).await;
            Err(anyhow::anyhow!(
                "chunk {} cache miss: cannot safely fresh-write without zeroing untouched real data", chunk_idx
            ))
        }
    }

    /// Internal: flush exactly the chunk at `chunk_idx` for `ino`.
    /// Slot data, file offset, gap_filled_prefix, real_data_end, and dirty_ranges are all
    /// pre-snapshotted by flush_one_chunk while holding the mutex. Reading these from the
    /// live slot after release is wrong — the slot may have been removed and recreated.
    async fn flush_buffer_async_one(&self, ino: u64, chunk_idx: u64, mut slot_data: Vec<u8>, file_offset: u64, gap_filled_prefix: usize, real_data_end: usize, dirty_ranges: Vec<(usize, usize)>, last_modified_snap: SystemTime) -> Result<()> {

        // Snapshot the file ID now. After the network write we'll verify it hasn't changed —
        // a delete+create can replace the file while the flush is in flight, and the new
        // file's metadata_cache must not be contaminated with old chunk locations.
        let file_id_at_flush_start = self.metadata_cache.get(&ino).map(|m| m.id);
        let buf_expected_id = self.write_buffers.get(&ino)
            .and_then(|s| s.try_lock().ok().and_then(|st| st.expected_file_id));

        // Check whether this slot needs PatchChunk or a fresh write.
        // Set inside the block below: true when flushed_sizes had no entry for this
        // chunk, i.e. this is the first flush of it in the current write session — the
        // only window where existing_chunk_size depends on (possibly stale/incomplete)
        // metadata_cache rather than this session's own authoritative flush history.
        let mut is_first_flush_this_session = true;
        let existing_chunk_size = {
            // Check if the buffer has a file_id expectation. If metadata_cache now refers
            // to a different file (inode reuse after delete+create), any chunk size from
            // metadata is stale and must be ignored to prevent PatchChunk on the wrong chunk.
            let expected_id = self.write_buffers.get(&ino)
                .and_then(|s| s.try_lock().ok().and_then(|st| st.expected_file_id));
            let meta_id_matches = expected_id.map(|eid|
                self.metadata_cache.get(&ino).map(|m| m.id == eid).unwrap_or(false)
            ).unwrap_or(true); // no expectation = trust metadata

            let from_flushed = self.write_buffers.get(&ino)
                .and_then(|s| s.try_lock().ok()
                    .and_then(|st| st.flushed_sizes.get(&chunk_idx).copied()));
            is_first_flush_this_session = from_flushed.is_none();
            if let Some(flushed) = from_flushed {
                flushed // flushed_sizes is always authoritative (same session)
            } else if meta_id_matches {
                self.metadata_cache.get(&ino)
                    .and_then(|m| m.chunk_location_for_idx(chunk_idx).map(|l| l.size))
                    .unwrap_or(0)
            } else {
                debug!("flush_buffer_async_one: ino={} chunk={} metadata file_id mismatch (expected={:?}) — treating as new file", ino, chunk_idx, expected_id);
                0
            }
        };
        let slot_len = slot_data.len();
        // Summarized at info (range count + total covered bytes) instead of the
        // full dirty_ranges array — a sparse write can carry hundreds of tuples
        // in one line (seen spanning most of a 4MB chunk under kdiskmark), and
        // the count/coverage is what's actually needed to follow the decision
        // logic below at a glance. Full tuple-by-tuple detail (needed to
        // investigate a race or a specific corrupted offset) stays available at
        // debug!, same as everywhere else in this codebase.
        let dirty_ranges_bytes: usize = dirty_ranges.iter().map(|&(s, e)| e - s).sum();
        info!("flush_buffer_async_one: ino={} chunk={} file_offset={} existing_chunk_size={} slot_len={} gap_prefix={} real_end={} dirty_ranges={} ranges ({} bytes) meta_file_id={:?} buf_expected_id={:?}",
            ino, chunk_idx, file_offset, existing_chunk_size, slot_len, gap_filled_prefix, real_data_end, dirty_ranges.len(), dirty_ranges_bytes, file_id_at_flush_start, buf_expected_id);
        debug!("flush_buffer_async_one: ino={} chunk={} dirty_ranges detail={:?}", ino, chunk_idx, dirty_ranges);
        let chunk_exists = existing_chunk_size > 0;
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
        // Mixed-extend: slot extends beyond the server chunk but gap_filled_prefix < existing_chunk_size.
        // This means the app wrote some new pages BEYOND the old end while also rewriting some earlier
        // pages. We can't do a fresh write (zeros in gaps would corrupt server pages 2-N), and
        // is_append_extend is false because gap_filled_prefix < existing_chunk_size. Treat as MultiPatch
        // using dirty_ranges — the server already has correct data for the gap bytes.
        let is_mixed_extend = chunk_exists
            && slot_len > existing_chunk_size
            && gap_filled_prefix < existing_chunk_size
            && !dirty_ranges.is_empty()
            && !is_truncated_session;
        // Sparse-within-chunk: the slot has gaps (non-contiguous dirty_ranges) that represent
        // unwritten regions. If we do a fresh write, the zero-filled gaps will overwrite data
        // that may already exist on the server (e.g., qcow2 metadata written at non-sequential
        // offsets). Force PATCH mode to send only the actually-written ranges.
        let has_sparse_gaps = !dirty_ranges.is_empty() && {
            let covered: usize = dirty_ranges.iter().map(|&(s, e)| e - s).sum();
            covered < slot_len && dirty_ranges.len() > 1
        };
        let is_sparse_write = has_sparse_gaps && !is_truncated_session;
        // Full-chunk replacement: all bytes newly written from offset 0, covering the entire
        // existing chunk, no gap-fill prefix. We have the complete new content in slot_data,
        // so we skip MultiPatch (which forces the server to read the old chunk) and use the
        // fresh write path instead — parallel dual-replica write, no server disk read.
        // The old chunk_id is queued for broadcast-delete cleanup afterward.
        let is_full_replacement = is_overwrite
            && gap_filled_prefix == 0
            && dirty_ranges.len() == 1
            && dirty_ranges[0].0 == 0
            && dirty_ranges[0].1 >= existing_chunk_size;

        // Capture old chunk_id before bypassing the patch path (needed for cleanup).
        let full_replacement_old_chunk_id: Option<dfs_common::ChunkId> = if is_full_replacement {
            self.metadata_cache.get(&ino)
                .and_then(|m| m.chunk_location_for_idx(chunk_idx).map(|l| l.chunk_id))
        } else {
            None
        };

        let needs_patch = (is_overwrite && !is_full_replacement)
            || is_append_extend || is_mixed_extend || is_sparse_write;

        info!("flush_buffer_async_one: ino={} chunk={} decision: chunk_exists={} is_overwrite={} is_append_extend={} is_mixed_extend={} is_sparse_write={} is_full_replacement={} needs_patch={} is_truncated_session={}",
              ino, chunk_idx, chunk_exists, is_overwrite, is_append_extend, is_mixed_extend, is_sparse_write, is_full_replacement, needs_patch, is_truncated_session);

        'try_patch: {
        if needs_patch {
            // Build the patch list:
            // Always use dirty_ranges when available — send only what the app actually
            // wrote, nothing more. For append_extend, dirty_ranges already contains only
            // the new bytes (all >= existing_chunk_size or crossing the boundary). The
            // legacy single-tail patch sent slot_data[existing_size..] which included
            // zero-filled gaps between the old end and the first real write, corrupting
            // server pages that were never touched by the application.
            let patches: Vec<(usize, Vec<u8>)> = if !dirty_ranges.is_empty() {
                dirty_ranges.iter()
                    .map(|&(s, e)| (s, slot_data[s..e.min(slot_data.len())].to_vec()))
                    .collect()
            } else {
                // Fallback: use the legacy single-range approach.
                let real_start = gap_filled_prefix;
                let real_end = effective_write_end;
                vec![(real_start, slot_data[real_start..real_end].to_vec())]
            };
            let meta = self.metadata_cache.get(&ino).map(|m| m.clone());
            if let Some(meta) = meta {
                // The file_id used to derive this chunk's file-scoped ChunkId. Prefer the
                // snapshot taken at flush start (matches the verified/non-verified branching
                // below); fall back to the freshly-read metadata's id if that snapshot was
                // unavailable (metadata_cache miss at flush start).
                let effective_file_id = file_id_at_flush_start.unwrap_or(meta.id);
                // Priority order for the base chunk_id:
                //   1. slot.server_chunk_id — confirmed by server in this session (highest)
                //   2. recent_chunk_writes  — last server-confirmed id (120s window)
                //   3. metadata_cache       — server metadata, may lag by a flush cycle
                //
                // server_chunk_id is set from every successful MultiPatch/write response
                // and is never overwritten by a cache refresh. It is the authoritative base
                // and eliminates the stale-base problem for any chunk that has been written
                // at least once in this session.
                let slot_server_id = self.write_buffers.get(&ino)
                    .and_then(|s| s.try_lock().ok()
                        .and_then(|st| st.slots.get(&chunk_idx)
                            .and_then(|sl| sl.server_chunk_id)));
                let mut recent_write_present = false;
                let old_location = {
                    let cached_loc = meta.chunk_location_for_idx(chunk_idx).cloned();
                    let recent = self.client.recent_chunk_writes.get(&(ino, chunk_idx))
                        .filter(|r| {
                            let (_, fid, ts, _) = r.value();
                            *fid == meta.id && ts.elapsed().as_secs() < 120
                        })
                        .map(|r| {
                            let (cid, _, _, nodes) = r.value();
                            (*cid, nodes.clone())
                        });
                    recent_write_present = recent.as_ref().is_some_and(|(_, nodes)| !nodes.is_empty());
                    match (cached_loc, recent) {
                        (Some(mut loc), Some((recent_id, recent_nodes))) => {
                            // Use recent_chunk_writes NODES — these are the 2 nodes we actually
                            // wrote to most recently, bypassing healer-added 3rd replicas in
                            // metadata_cache that don't have the latest patch and would cause
                            // ChunkStale. Do NOT override the chunk_id from metadata_cache:
                            // recent_chunk_writes is updated for both fresh writes and patches, so
                            // both should agree. If they differ, metadata_cache reflects the latest
                            // server-committed state (via flush_metadata_sync) and is authoritative.
                            // The old chunk_id override was causing T26 failures: the initial
                            // fresh-write's H0 in recent_chunk_writes poisoned every other patch
                            // by overriding H1, H2 etc. from metadata_cache with the stale H0.
                            if recent_id != loc.chunk_id {
                                // They can momentarily differ if the initial write and a subsequent
                                // patch race, but slot.server_chunk_id (checked next) resolves it.
                                debug!("flush_buffer_async_one: ino={} chunk={} recent_chunk_writes id {} differs from cache id={} — using cache id, recent nodes",
                                    ino, chunk_idx, recent_id, loc.chunk_id);
                            }
                            if !recent_nodes.is_empty() {
                                loc.nodes = recent_nodes;
                            }
                            Some(loc)
                        }
                        (loc, _) => loc,
                    }
                };
                // Override chunk_id with the server-confirmed value if we have one.
                // This is the highest-priority source and cannot be stale — it was set
                // directly from the last server response for this slot.
                let old_location = old_location.map(|mut loc| {
                    if let Some(confirmed_id) = slot_server_id {
                        if confirmed_id != loc.chunk_id {
                            info!("flush_buffer_async_one: ino={} chunk={} using server_chunk_id {} (cache/recent had {})",
                                ino, chunk_idx, confirmed_id, loc.chunk_id);
                            loc.chunk_id = confirmed_id;
                            loc.checksum = confirmed_id.hash;
                        }
                    }
                    loc
                });

                // Canonical write-pair: once we've successfully patched a chunk in this
                // session, always use the same 2 nodes for all subsequent patches.
                // canonical_write_nodes takes priority over everything — metadata_cache,
                // recent_chunk_writes, sorted-first-2 — because those can all be
                // contaminated by healer-added replicas that hold divergent chunk versions.
                // If no canonical pair exists yet, fall back to sorted-first-2.
                let canonical = self.write_buffers.get(&ino)
                    .and_then(|s| s.try_lock().ok()
                        .and_then(|st| st.canonical_write_nodes.get(&chunk_idx).cloned()));
                let old_location = old_location.map(|mut loc| {
                    if let Some(nodes) = canonical {
                        if !nodes.is_empty() {
                            loc.nodes = nodes;
                            return loc;
                        }
                    }
                    if loc.nodes.len() > 2 {
                        loc.nodes.sort_unstable();
                        loc.nodes.truncate(2);
                    }
                    loc
                });

                // Without a recent_chunk_writes or canonical_write_nodes override, the
                // node list above came straight from metadata_cache's raw, cached
                // FileMetadata.chunk_locations — the same frozen-at-write-time inline
                // copy that resolve_chunk_nodes corrects for reads (GetFileChunkMap),
                // but patches never went through that correction. A chunk untouched in
                // this session (no recent write, no canonical pair yet) can carry a
                // node list the leader's CHUNK_TABLE has long since pruned a ghost out
                // of, sending the very first patch of the session at a node that
                // doesn't hold the chunk. One extra round trip here, only on that first
                // touch, gets the leader's authoritative current node list before we
                // commit to a target — every subsequent patch in the session is
                // protected by recent_chunk_writes/canonical_write_nodes and skips this.
                let has_session_override = recent_write_present
                    || self.write_buffers.get(&ino)
                        .and_then(|s| s.try_lock().ok()
                            .and_then(|st| st.canonical_write_nodes.get(&chunk_idx).cloned()))
                        .is_some_and(|nodes| !nodes.is_empty());
                let old_location = if has_session_override {
                    old_location
                } else {
                    match old_location {
                        Some(loc) => {
                            match self.client.get_single_chunk_location(meta.id, chunk_idx).await {
                                Ok(Some(fresh)) if fresh.chunk_id == loc.chunk_id && fresh.nodes != loc.nodes => {
                                    // Trust the leader's fresh node list wholesale — it both
                                    // prunes ghosts (nodes no longer in fresh are gone from the
                                    // ring) and picks up nodes added since our session cache was
                                    // last refreshed.
                                    //
                                    // This used to intersect loc.nodes with fresh.nodes instead,
                                    // on the theory that a healer-added node might be registered
                                    // in the leader's metadata before its copy actually completes
                                    // — trusting it as a patch target could displace a confirmed
                                    // holder into the dual-RF skip slot, which then loses its
                                    // copy via tombstone, leaving too few replicas. Audited every
                                    // live path that writes a chunk's node list into the leader's
                                    // metadata (do_heal_chunk_shared verifies via HasChunks before
                                    // recording; the FUSE write path relies entirely on the healer
                                    // for the 3rd replica and never writes it speculatively itself;
                                    // MultiPatch reports only patched_node_ids — nodes the client
                                    // itself directly confirmed via RPC response) and found none
                                    // that registers an unconfirmed node — so "in fresh" already
                                    // means confirmed, and intersecting against a merely-stale
                                    // client-side cache only threw away good replicas.
                                    //
                                    // Real incident, 2026-07-03 (staging gluster3 repave): a
                                    // chunk's confirmed holders were healer-migrated out from
                                    // under a session whose cache still pointed at 2 old nodes.
                                    // Old nodes ∩ fresh nodes coincidentally overlapped in exactly
                                    // one node — the one being repaved — so intersection collapsed
                                    // the target set to that single, about-to-disappear node
                                    // instead of the 2 actually-good replicas fresh reported.
                                    // Every subsequent patch attempt failed identically until the
                                    // client was restarted, since the stale cache never healed
                                    // itself: this exact reconciliation is the only place a stale
                                    // session cache gets corrected before the intersection could
                                    // discard the correction.
                                    info!("flush_buffer_async_one: ino={} chunk={} no session override yet — \
                                           refreshing node list from leader before first patch this session ({:?} -> {:?})",
                                        ino, chunk_idx, loc.nodes, fresh.nodes);
                                    Some(ChunkLocation { nodes: fresh.nodes.clone(), ..loc })
                                }
                                _ => Some(loc),
                            }
                        }
                        None => None,
                    }
                };

                if let Some(old_location) = old_location {
                    // Note: there used to be a "stale-write guard" here that discarded the
                    // flush outright whenever the chunk's current id differed from a
                    // snapshot taken at open() time. Removed — patches here are always
                    // sliced from dirty_ranges (this session's own written bytes only,
                    // never server-read content, see the patches construction above), and
                    // old_location.chunk_id is already correctly resolved to the current
                    // chunk via the priority chain above (slot_server_id > recent_chunk_writes
                    // > canonical_write_nodes > metadata_cache). A mismatch against a stale
                    // open-time snapshot was never a real hazard — it's the expected case
                    // under concurrent writes to different byte ranges of the same chunk —
                    // and the old guard was silently dropping valid writes because of it.
                    // Genuine staleness is already handled correctly below: the server
                    // validates the patch base and the *_verified retry path corrects it.
                    //
                    // Acquire the per-chunk lock before the network call and hold it
                    // through the metadata_cache update at the end of this function.
                    let _chunk_guard = DfsFilesystem::lock_chunk(&self.chunk_write_locks, ino, chunk_idx).await;

                    // Re-read server_chunk_id and canonical_write_nodes after acquiring
                    // the lock. A concurrent flush that completed while we were waiting
                    // may have patched this chunk and updated both — our pre-lock snapshot
                    // of old_location is stale in that case and would cause a guaranteed
                    // MultiPatch failure (the base chunk was already renamed away).
                    let old_location = {
                        let mut loc = old_location;
                        if let Some(s) = self.write_buffers.get(&ino) {
                            if let Ok(st) = s.try_lock() {
                                if let Some(new_id) = st.slots.get(&chunk_idx).and_then(|sl| sl.server_chunk_id) {
                                    if new_id != loc.chunk_id {
                                        info!("flush_buffer_async_one: ino={} chunk={} post-lock server_chunk_id refresh {} -> {}",
                                            ino, chunk_idx, loc.chunk_id, new_id);
                                        loc.chunk_id = new_id;
                                        loc.checksum = new_id.hash;
                                    }
                                }
                                if let Some(nodes) = st.canonical_write_nodes.get(&chunk_idx).cloned() {
                                    if !nodes.is_empty() {
                                        loc.nodes = nodes;
                                    }
                                }
                            }
                        }
                        loc
                    };

                    // The server always recomputes the post-patch hash itself from its own
                    // on-disk base (it can never safely trust a client-supplied hash — a
                    // stale client cache could otherwise silently corrupt a chunk under the
                    // wrong name). So pre-hashing here client-side bought nothing but an
                    // extra full Blake3 pass over the same buffer the server was about to
                    // hash anyway, paid serially before the RPC even goes out. Don't compute
                    // it. We still apply the patch locally when the pre-patch chunk is warm
                    // in chunk_cache, so we can re-cache the patched buffer under the
                    // server's authoritative new chunk_id below — that's the only part of
                    // this that's actually worth keeping (avoids a cold read-back on the
                    // next access to this chunk).
                    let pending_cache_update = {
                        let cached = self.client.chunk_cache.get(&old_location.chunk_id);
                        cached.filter(|cached_arc| {
                            // A cache entry that's shorter than the chunk's real, known
                            // size isn't the whole chunk — e.g. only the byte range a
                            // prior partial/range-fetch read actually touched. Patching
                            // on top of it and re-caching the result under the new
                            // chunk_id below would silently poison the cache with a
                            // truncated buffer: every later read of a perfectly good,
                            // fully-present-on-disk chunk would then be served bytes cut
                            // off wherever this one patch happened to reach, instead of
                            // the correct content the server actually has (reproduced via
                            // T49 — a chunk warmed by a prior read, then patched, served
                            // truncated/wrong data on every read after, even though the
                            // server-side chunk was always complete and correct). Skip the
                            // optimization instead — the next read just pays a cold
                            // read-back, same as an ordinary cache miss.
                            cached_arc.len() as u64 == old_location.size as u64
                        }).map(|cached_arc| {
                            let mut patched = (*cached_arc).clone();
                            for (intra, data) in &patches {
                                let end = intra + data.len();
                                if end > patched.len() {
                                    patched.resize(end, 0u8);
                                }
                                patched[*intra..end].copy_from_slice(data);
                            }
                            patched
                        })
                    };

                    // Send all dirty ranges in a single MultiPatch RPC — one round trip,
                    // atomic server-side write+rename.
                    // Pass file_id + chunk_idx so the server validates chunk_id and returns
                    // ChunkStale if stale; client retries automatically with corrected id.
                    // Hints were computed once for the whole wave by flush_all_pipelined and
                    // written into patch_prefetch_hints before tasks were spawned. Read by
                    // cloning the inner Arc — one atomic increment, no HashMap copy, no
                    // Mutex contention under concurrent flush tasks.
                    let hints: Arc<HashMap<SocketAddr, Vec<dfs_common::ChunkId>>> = {
                        self.patch_prefetch_hints.lock().unwrap().clone()
                    };
                    let patch_result = if let Some(fid) = file_id_at_flush_start {
                        self.client.multi_patch_chunk_on_replicas_verified(
                            old_location.chunk_id, fid, chunk_idx,
                            file_offset, patches.clone(), &old_location, None,
                            self.use_dual_rf, hints.clone(),
                        ).await
                    } else {
                        self.client.multi_patch_chunk_on_replicas(
                            old_location.chunk_id, effective_file_id, file_offset, patches.clone(),
                            &old_location, None,
                            self.use_dual_rf, hints.clone(),
                        ).await
                    };
                    let (mut new_location, _skip_pairs) = match patch_result {
                        Ok((loc, sp)) => {
                            // Re-cache the patched buffer under the server's authoritative
                            // chunk_id (not a client-guessed one) so the next read of this
                            // chunk doesn't pay a cold read-back.
                            if let Some(patched) = pending_cache_update {
                                let _ = self.client.chunk_cache.remove(&old_location.chunk_id);
                                self.client.chunk_cache.insert(loc.chunk_id, std::sync::Arc::new(patched));
                            }
                            (loc, sp)
                        }
                        Err(e) => {
                            // MultiPatch failed — the chunk_id in our cache (or recent_chunk_writes)
                            // is stale. Rather than fetching the full file metadata, ask the leader
                            // for just this one chunk's current location. One in-memory map lookup,
                            // no full metadata scan, no Vec<ChunkLocation> allocation for a 2GB file.
                            warn!("flush_buffer_async_one: MultiPatch failed for ino={} chunk={} ({}), fetching single chunk location from leader", ino, chunk_idx, e);
                            let fresh_loc = if let Some(file_id) = file_id_at_flush_start {
                                match self.client.get_single_chunk_location(file_id, chunk_idx).await {
                                    Ok(Some(loc)) if loc.chunk_id != old_location.chunk_id => {
                                        info!("flush_buffer_async_one: got fresh chunk_id {} for ino={} chunk={} (was {})",
                                            loc.chunk_id, ino, chunk_idx, old_location.chunk_id);
                                        // Update metadata_cache so the next flush uses the correct id.
                                        if let Some(mut meta_entry) = self.metadata_cache.get_mut(&ino) {
                                            if let Some(existing) = meta_entry.chunk_location_for_idx_mut(chunk_idx) {
                                                *existing = loc.clone();
                                            }
                                        }
                                        Some(loc)
                                    }
                                    Ok(Some(loc)) => Some(loc),
                                    Ok(None) => None,
                                    Err(fetch_err) => {
                                        warn!("flush_buffer_async_one: single-chunk fetch failed for ino={} chunk={}: {}", ino, chunk_idx, fetch_err);
                                        None
                                    }
                                }
                            } else {
                                None
                            };
                            // Apply sorted-first-2 to the fresh location too.
                            let fresh_loc = fresh_loc.map(|mut loc| {
                                if loc.nodes.len() > 2 { loc.nodes.sort_unstable(); loc.nodes.truncate(2); }
                                loc
                            });

                            match fresh_loc {
                                Some(loc) => {
                                    // Retry the patch with the authoritative location.
                                    // Reuse hints from the initial attempt — any already-prefetched
                                    // chunks are no-ops on the server (contains_key guard).
                                    let retry_result = if let Some(fid) = file_id_at_flush_start {
                                        self.client.multi_patch_chunk_on_replicas_verified(
                                            loc.chunk_id, fid, chunk_idx, file_offset,
                                            patches.clone(), &loc, None, self.use_dual_rf, hints.clone(),
                                        ).await
                                    } else {
                                        self.client.multi_patch_chunk_on_replicas(
                                            loc.chunk_id, effective_file_id, file_offset, patches.clone(), &loc, None,
                                            self.use_dual_rf, hints.clone(),
                                        ).await
                                    };
                                    match retry_result {
                                        Ok((retry_loc, retry_skips)) => {
                                            info!("flush_buffer_async_one: retry MultiPatch succeeded for ino={} chunk={}: {} -> {}",
                                                ino, chunk_idx, loc.chunk_id, retry_loc.chunk_id);
                                            (retry_loc, retry_skips)
                                        }
                                        Err(retry_err) => {
                                            warn!("flush_buffer_async_one: retry MultiPatch failed for ino={} chunk={}: {}", ino, chunk_idx, retry_err);
                                            // If ALL replicas failed after the leader-refresh retry, the
                                            // chunk file is gone from every replica — unrecoverable.
                                            // This covers two cases:
                                            //   a) leader returned a different chunk_id (stale-corrected
                                            //      to X) and X is missing (healer deleted it as orphan)
                                            //   b) leader returned the SAME chunk_id as our base (leader
                                            //      is stale, never received the patch update) but replicas
                                            //      are ahead and their corrected chunk is also gone
                                            // In both cases gap regions are already lost; write slot_data
                                            // as a new chunk so the application write succeeds — but only
                                            // after reconstructing any untouched bytes from chunk_cache
                                            // (see reconstruct_or_abort_for_fresh_write), since slot_data
                                            // alone is a faithful full-chunk image only for full replacements.
                                            if retry_err.to_string().contains("all replicas failed") {
                                                warn!("flush_buffer_async_one: all replicas failed after leader-refresh retry for ino={} chunk={} (leader_id={} base_id={}) — falling back to fresh write",
                                                    ino, chunk_idx, loc.chunk_id, old_location.chunk_id);
                                                if !is_full_replacement {
                                                    self.reconstruct_or_abort_for_fresh_write(
                                                        ino, chunk_idx, &mut slot_data, &dirty_ranges,
                                                        &[old_location.chunk_id, loc.chunk_id],
                                                    ).await?;
                                                }
                                                break 'try_patch;
                                            }
                                            // Increment the consecutive failure counter. After
                                            // MAX_PATCH_FAILURES, fall back to a fresh write
                                            // regardless of the error type — this is the safety
                                            // valve against infinite loops when clock skew or
                                            // lingering corruption prevents the guard from
                                            // converging.
                                            const MAX_PATCH_FAILURES: u32 = 5;
                                            let failures = if let Some(state_arc) = self.write_buffers.get(&ino) {
                                                let mut state = state_arc.lock().await;
                                                if let Some(slot) = state.slots.get_mut(&chunk_idx) {
                                                    slot.consecutive_patch_failures += 1;
                                                    slot.consecutive_patch_failures
                                                } else { MAX_PATCH_FAILURES }
                                            } else { MAX_PATCH_FAILURES };
                                            if failures >= MAX_PATCH_FAILURES {
                                                warn!("flush_buffer_async_one: ino={} chunk={} exceeded {} consecutive patch failures — attempting cache reconstruction before fresh write",
                                                    ino, chunk_idx, MAX_PATCH_FAILURES);
                                                // Unless this is a full replacement, slot_data alone isn't
                                                // a faithful image of the chunk — reconstruct the untouched
                                                // bytes from chunk_cache or abort (see
                                                // reconstruct_or_abort_for_fresh_write).
                                                if !is_full_replacement {
                                                    self.reconstruct_or_abort_for_fresh_write(
                                                        ino, chunk_idx, &mut slot_data, &dirty_ranges,
                                                        &[old_location.chunk_id, loc.chunk_id],
                                                    ).await?;
                                                }
                                                break 'try_patch;
                                            }
                                            // Even though the retry failed, update metadata cache with
                                            // the fresh location so the next attempt uses the current
                                            // chunk ID instead of looping forever on the stale one.
                                            if let Some(mut meta_entry) = self.metadata_cache.get_mut(&ino) {
                                                if let Some(existing) = meta_entry.chunk_location_for_idx_mut(chunk_idx) {
                                                    *existing = loc.clone();
                                                }
                                            }
                                            self.client.recent_chunk_writes.insert(
                                                (ino, chunk_idx),
                                                (loc.chunk_id, file_id_at_flush_start.unwrap_or_default(), std::time::Instant::now(), loc.nodes.clone()),
                                            );
                                            if let Some(state_arc) = self.write_buffers.get(&ino) {
                                                let mut state = state_arc.lock().await;
                                                if let Some(slot) = state.slots.get_mut(&chunk_idx) {
                                                    slot.flushing = false;
                                                }
                                            }
                                            self.notify_chunk_flush_complete(ino, chunk_idx).await;
                                            return Err(retry_err);
                                        }
                                    }
                                }
                                None => {
                                    if let Some(state_arc) = self.write_buffers.get(&ino) {
                                        let mut state = state_arc.lock().await;
                                        if let Some(slot) = state.slots.get_mut(&chunk_idx) {
                                            slot.flushing = false;
                                        }
                                    }
                                    self.notify_chunk_flush_complete(ino, chunk_idx).await;
                                    return Err(e);
                                }
                            }
                        }
                    };

                    // Do NOT attempt to patch extra nodes when the primary returned < 2 replicas.
                    // A node not in the sorted-first-2 may hold a divergent version of the chunk
                    // (from a prior patch cycle that wrote to it but didn't track it in
                    // recent_chunk_writes), causing a REPLICA DISAGREEMENT that produces a
                    // second divergent chunk_id — making state worse, not better.
                    // The healer's freshness check correctly converges replica state without
                    // introducing further divergence.
                    //
                    // old_location.chunk_id is intentionally left untouched on disk and in the
                    // routing table: chunk_id is content-addressed (blake3(offset||data)), so
                    // the same chunk_id can simultaneously be the current base for OTHER
                    // (file_id, chunk_idx) slots with identical content+offset (e.g. duplicate
                    // files) that haven't patched yet. The leader's deep orphan-purge sweep
                    // reclaims it once live_chunk_ids() no longer references it cluster-wide.

                    // Record new chunk_id AND patched node list so the next flush uses
                    // the correct nodes — not the stale node list from metadata_cache,
                    // which may include healer-added nodes that never received this patch.
                    if let Some(file_id) = file_id_at_flush_start {
                        self.client.recent_chunk_writes.insert(
                            (ino, chunk_idx),
                            (new_location.chunk_id, file_id, std::time::Instant::now(), new_location.nodes.clone()),
                        );
                    }
                    // canonical_write_nodes is updated in the guaranteed lock().await below.
                    // Update recent_chunk_writes after every patch so that if write_buffers is
                    // cleared (safe_to_remove=true removes the slot + server_chunk_id), the
                    // next flush still has the correct base chunk_id rather than reverting to
                    // the initial fresh-write's chunk_id (H0), which would make it patch from
                    // a stale base and trigger ChunkStale on every other write cycle (T26).
                    if let Some(file_id) = file_id_at_flush_start {
                        let mut sorted_nodes = new_location.nodes.clone();
                        sorted_nodes.sort_unstable();
                        sorted_nodes.truncate(2);
                        self.client.recent_chunk_writes.insert(
                            (ino, chunk_idx),
                            (new_location.chunk_id, file_id, std::time::Instant::now(), sorted_nodes),
                        );
                    }
                    // Apply the patch to the cached base chunk so chunk_cache reflects the
                    // post-patch state immediately. This is the same recipe the server uses:
                    // read the old chunk, overlay the dirty ranges, write the new chunk.
                    // Reads then fall through byte_range_cache (miss, invalidated below) to
                    // chunk_cache (hit) — one authoritative source, no priority race.
                    // Guard: a cache entry shorter than the chunk's known size isn't the
                    // whole chunk (e.g. only a prior partial/range-fetch's coverage) —
                    // patching on top of it and re-caching under new_location.chunk_id
                    // would poison the cache with a truncated buffer for a chunk that's
                    // actually complete and correct on disk. See the matching guard in
                    // the MultiPatch cache-update above (T49) for the full incident.
                    if let Some(base) = self.client.chunk_cache.get(&old_location.chunk_id)
                        .filter(|base| base.len() as u64 == old_location.size as u64)
                    {
                        let mut patched = (*base).clone();
                        for &(s, e) in &dirty_ranges {
                            let e = e.min(slot_data.len());
                            if e > patched.len() { patched.resize(e, 0u8); }
                            if s < e { patched[s..e].copy_from_slice(&slot_data[s..e]); }
                        }
                        self.client.chunk_cache.insert(new_location.chunk_id, Arc::new(patched));
                    }
                    // Evict both byte_range_cache and zero_gap_table entries for this chunk
                    // so stale sub-range entries can't win over the freshly-patched chunk_cache.
                    self.client.invalidate_byte_range_cache_for_chunk(ino, file_offset, CHUNK_SIZE).await;
                    // Note: do NOT seed the zero gap table here. In the patch path the "gaps"
                    // between dirty ranges are existing server data (e.g. the gap_prefix for an
                    // append-extend is the original file content), not zero-filled holes. Seeding
                    // them as zeros would cause those bytes to be returned as 0x00 on the next
                    // read instead of the real data fetched from the server.

                    if let Some(mut meta_entry) = self.metadata_cache.get_mut(&ino) {
                        if file_id_at_flush_start.map(|id| id != meta_entry.id).unwrap_or(false) {
                            info!("flush_buffer_async_one: ino={} chunk={} file replaced during patch flush — discarding metadata update", ino, chunk_idx);
                            if let Some(state_arc) = self.write_buffers.get(&ino) {
                                let mut state = state_arc.lock().await;
                                let discarded_len = state.slots.get(&chunk_idx).map(|s| s.resident()).unwrap_or(0);
                                state.slots.remove(&chunk_idx);
                                if discarded_len > 0 {
                                    self.global_buffered_bytes.fetch_sub(
                                        discarded_len.min(self.global_buffered_bytes.load(std::sync::atomic::Ordering::Relaxed)),
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                }
                            }
                            return Ok(());
                        }
                        if let Some(loc) = meta_entry.chunk_location_for_idx_mut(chunk_idx) {
                            *loc = new_location.clone();
                        }
                        if let Some(end) = new_location.file_offset.map(|o| o + new_location.size as u64) {
                            meta_entry.size = meta_entry.size.max(end);
                        }
                    }
                    // Update read engine, THEN remove slot.
                    // Do NOT call flush_metadata_sync here — flush_all_pipelined does a single
                    // final sync after ALL chunks are done. Calling it per-chunk causes a race:
                    // concurrent patches for chunks 0/1/2 each read metadata_cache mid-flight
                    // and the highest-seq write wins in the queue dedup, potentially recording
                    // stale chunk_ids for chunks whose patch hadn't updated metadata_cache yet.
                    // Feed only new_location (this chunk's just-patched result), not the
                    // whole (ever-growing) meta.chunk_locations — see
                    // feed_chunk_locations_to_read_engine's doc comment / the fix applied
                    // at this function's other call sites (~line 1547/1597): passing the
                    // full list here made every single patch flush clone and rebuild a
                    // window over the entire file's chunk history, O(n) work per flush
                    // (O(n^2) over a large file under sustained random writes) — confirmed
                    // live 2026-07-12 via fio directly against the DFS mount: Q32T1 4K
                    // random write collapsed from 104MiB/s at 128 chunks to 184KiB/s at
                    // 1024 chunks, with no other variable changed.
                    let meta_size = self.metadata_cache.get(&ino).map(|m| m.size);
                    if let Some(meta_size) = meta_size {
                        self.client.feed_chunk_locations_to_read_engine(
                            ino, std::slice::from_ref(&new_location), meta_size,
                        ).await;
                    }
                    // Now that read engine is updated, safe to remove slot — unless new
                    // data arrived while the patch was in-flight (concurrent write() added
                    // bytes or dirty ranges beyond what we just patched). In that case,
                    // keep the slot with flushing=false so the next flush cycle picks it up.
                    // Without this check, writes that land during a patch are silently dropped:
                    // the slot is removed, taking with it any dirty_ranges within the patch window.
                    // We detect concurrent writes via BOTH:
                    //   - current_len > patched_len: an append extended the slot
                    //   - last_modified > last_modified_snap: an overwrite updated existing bytes
                    // The length check alone misses same-region overwrites (T26: 20 sequential
                    // patches at the same 1MB intra-chunk offset each leave slot len unchanged).
                    let patched_len = slot_data.len(); // snapshot length = what we actually sent
                    // The chunk's true known size must never shrink from an overwrite/patch of
                    // an existing chunk — slot_data here is only the patched sub-range (e.g. a
                    // 12KB header rewrite), not the whole chunk, so what gets recorded into
                    // flushed_sizes (read back as existing_chunk_size on the NEXT flush this
                    // session — see its lookup at the top of this function) must be the larger
                    // of the two, not patched_len alone. Storing patched_len alone corrupts
                    // every subsequent write to this chunk this session into believing the
                    // chunk shrank to just the patched region's length: the next write that
                    // reaches that length is misclassified as a full replacement and genuinely
                    // truncates the chunk on the server. Found via a rapid-repeated-patch
                    // repro (DVR app rewriting a recording's header on every open) that
                    // silently zeroed most of the chunk's real content.
                    let known_chunk_size = patched_len.max(existing_chunk_size);
                    if let Some(state_arc) = self.write_buffers.get(&ino) {
                        let mut state = state_arc.lock().await;
                        let buf_id = state.expected_file_id;
                        let id_ok = match (file_id_at_flush_start, buf_id) {
                            (Some(fid), Some(bid)) => fid == bid,
                            _ => true,
                        };
                        if id_ok {
                            // Guaranteed update for server_chunk_id and canonical_write_nodes.
                            // Must live here (not try_lock above) — QEMU write() holds this lock
                            // frequently, and a skipped try_lock leaves server_chunk_id stale,
                            // causing every subsequent flush to use the wrong base chunk_id.
                            // Covers both first-success and retry-success paths.
                            if let Some(slot) = state.slots.get_mut(&chunk_idx) {
                                slot.server_chunk_id = Some(new_location.chunk_id);
                                slot.consecutive_patch_failures = 0;
                            }
                            if !new_location.nodes.is_empty() {
                                // Union with the existing canonical set instead of replacing it
                                // outright. new_location.nodes is only the subset that answered
                                // THIS round (multi_patch_chunk_on_replicas_inner excludes any
                                // replica that failed/disagreed/was stale for this one call) — a
                                // node missing from it does not mean it stopped being a real
                                // replica, just that it didn't answer in time this round. The
                                // most common cause: it's mid-fold server-side (holding
                                // chunk_patch_locks for this exact slot while its peer, already
                                // folded, answers immediately) — a timing difference between two
                                // replicas' background fold schedules, not a data-loss event.
                                //
                                // Replacing wholesale here used to permanently narrow every
                                // future patch this session down to whichever node happened to
                                // answer fastest on ONE round: canonical_write_nodes is
                                // deliberately sticky (see its doc comment) and nothing else
                                // ever adds a dropped node back, so the excluded replica was
                                // never retried again for the rest of the session — silently
                                // running at RF=1 for a chunk that should have been RF=2,
                                // discovered 2026-07-10 while investigating a chunk that ended
                                // up registered with only 1-of-2 intended replicas holding data.
                                //
                                // Bounded by canonical_node_miss_streak so a node that's
                                // genuinely gone (not just momentarily slow) doesn't get retried
                                // — and pay a timeout — on every single patch for the rest of the
                                // session; after MISS_STREAK_DROP_GRACE elapses without it
                                // answering, drop it for real and let the healer's normal
                                // under-RF detection be the path back, same as before this fix.
                                //
                                // Time-based, not a fixed round count (changed 2026-07-11): a real
                                // incident on a hot qcow2-install chunk showed 5 consecutive rounds
                                // completing in 78ms — nowhere near enough time for a targeted
                                // restore's push+verify round-trip to land, let alone for a node
                                // that's merely a few dozen ms slower to fold under load to catch
                                // back up. A round-count cap punishes exactly the chunks under the
                                // heaviest write load hardest, dropping a healthy node's replica
                                // right when the RF actually mattered most, and the chunk_map's
                                // own RCL registration never gets a second contributor again for
                                // the rest of the session. A wall-clock grace period adapts
                                // naturally to chunk hotness instead.
                                const MISS_STREAK_DROP_GRACE: std::time::Duration = std::time::Duration::from_secs(3);
                                const HEAL_REQUEST_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
                                let now = std::time::Instant::now();
                                let previous = state.canonical_write_nodes.get(&chunk_idx).cloned().unwrap_or_default();
                                let mut merged = new_location.nodes.clone();
                                for node in &previous {
                                    if new_location.nodes.contains(node) {
                                        state.canonical_node_miss_streak.remove(&(chunk_idx, *node));
                                        continue;
                                    }
                                    let tracker = state.canonical_node_miss_streak
                                        .entry((chunk_idx, *node))
                                        .or_insert(MissTracker { first_missed_at: now, last_heal_request_at: None });
                                    let missing_for = now.duration_since(tracker.first_missed_at);
                                    if missing_for < MISS_STREAK_DROP_GRACE {
                                        merged.push(*node);
                                        // Ask the leader to restore this exact node's replica of
                                        // the CURRENT chunk_id now, rather than just hoping it
                                        // reappears on a future round or waiting for the healer's
                                        // own scan cycle (which can't keep pace with a hot chunk
                                        // that's superseded every few hundred ms — see the
                                        // RF=1-cascade investigation, 2026-07-11). Targets this
                                        // specific node (not a re-derived candidate) so the slot's
                                        // replica set actually converges instead of chasing a
                                        // different node on every generation. Fire-and-forget:
                                        // must not add latency to this patch round. Throttled to
                                        // at most one in flight per HEAL_REQUEST_MIN_INTERVAL —
                                        // a hot chunk can complete a dozen rounds within the grace
                                        // window, and firing a fresh heal request on every single
                                        // one just piles up redundant concurrent pushes racing each
                                        // other for the same target.
                                        let should_request = tracker.last_heal_request_at
                                            .map(|t| now.duration_since(t) >= HEAL_REQUEST_MIN_INTERVAL)
                                            .unwrap_or(true);
                                        if should_request {
                                            tracker.last_heal_request_at = Some(now);
                                            warn!("canonical_write_nodes: node {} missing for ino={} chunk={} (missing_for={:?}) — requesting targeted restore of {} on that node",
                                                node, ino, chunk_idx, missing_for, new_location.chunk_id);
                                            let heal_client = self.client.clone();
                                            let heal_node = *node;
                                            let heal_chunk_id = new_location.chunk_id;
                                            tokio::spawn(async move {
                                                heal_client.heal_chunk_to_node(heal_chunk_id, heal_node, Some(effective_file_id)).await;
                                            });
                                        }
                                    } else {
                                        info!("canonical_write_nodes: dropping node {} for ino={} chunk={} \
                                               after missing for {:?}",
                                            node, ino, chunk_idx, missing_for);
                                        state.canonical_node_miss_streak.remove(&(chunk_idx, *node));
                                    }
                                }
                                state.canonical_write_nodes.insert(chunk_idx, merged);
                            }
                            let new_data_arrived = state.slots.get(&chunk_idx).map(|s| {
                                s.span_end > patched_len || s.last_modified > last_modified_snap
                            }).unwrap_or(false);
                            if new_data_arrived {
                                // New data arrived during the patch — keep slot, update flushed_sizes
                                // so the next flush knows where the server's content ends.
                                state.flushed_sizes.insert(chunk_idx, known_chunk_size);
                                if let Some(slot) = state.slots.get_mut(&chunk_idx) {
                                    slot.flushing = false;
                                }
                                // No global_buffered_bytes subtraction here: the slot is KEPT
                                // with all its extents resident, so no memory was freed. The
                                // old code subtracted patched_len (the padded snapshot length)
                                // — already an over-subtract in the padded world (the slot's
                                // removal later subtracted its full length again), and under
                                // sparse extents patched_len (a materialized span) can exceed
                                // every dirty byte ever added, wiping the whole counter for
                                // memory still held. The eventual slot removal subtracts
                                // resident() and settles the account exactly.
                            } else {
                                if known_chunk_size > 0 {
                                    state.flushed_sizes.insert(chunk_idx, known_chunk_size);
                                }
                                // Subtract the slot's actual resident size (sum of its extents'
                                // allocations), not patched_len — mirrors what the add side
                                // counted. See resident_bytes()'s doc comment.
                                let removed_size = state.slots.remove(&chunk_idx).map(|s| s.resident()).unwrap_or(0);
                                if removed_size > 0 {
                                    self.global_buffered_bytes.fetch_sub(
                                        removed_size.min(self.global_buffered_bytes.load(std::sync::atomic::Ordering::Relaxed)),
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                }
                            }
                        }
                    }
                    // Metadata commit complete, cleanup done — notify any waiters so next flush can proceed.
                    // CRITICAL: Must notify AFTER cleanup (flushing=false or slot removed), otherwise woken
                    // tasks see flushing=true and bail, but flush_all_pipelined counts them as in-flight
                    // and waits forever.
                    self.notify_chunk_flush_complete(ino, chunk_idx).await;
                    return Ok(());
                }
            }
            // No metadata or location — fall through to normal write
        }
        } // end 'try_patch

        // Write path: send data to server, update metadata & read engine, THEN remove slot.
        // For sparse writes (non-contiguous dirty_ranges), we must NOT send the full slot with
        // zero-filled gaps, as this will overwrite existing data (e.g. qcow2 metadata clusters).
        // Instead, write each dirty range individually at its correct file offset.
        info!("flush_buffer_async_one: ino={} chunk={} FRESH WRITE PATH: chunk_exists={} slot_len={} dirty_ranges={:?}",
              ino, chunk_idx, chunk_exists, slot_len, dirty_ranges);

        let has_gaps = if !dirty_ranges.is_empty() {
            let covered: usize = dirty_ranges.iter().map(|&(s, e)| e - s).sum();
            covered < slot_len
        } else {
            false
        };

        if has_gaps && dirty_ranges.len() > 1 {
            // Sparse write: multiple non-contiguous ranges with gaps. We must NOT send the full
            // slot with zero-filled gaps, as those zeros will overwrite data written in earlier
            // operations within the same session (e.g., qcow2 metadata clusters).
            //
            // Strategy: Write the FULL slot (including zeros in gaps) as a fresh chunk write.
            // This ensures the chunk covers the entire range and reads from gap regions return
            // zeros instead of EIO. The gaps contain zeros because the application never wrote
            // to those offsets - it's sparse data, not missing data.
            info!("flush_buffer_async_one: ino={} chunk={} SPARSE WRITE: {} ranges covering {} of {} bytes - writing full slot with zero gaps",
                  ino, chunk_idx, dirty_ranges.len(),
                  dirty_ranges.iter().map(|&(s, e)| e - s).sum::<usize>(), slot_len);
            // Fall through to normal fresh write path which sends the full slot_data
        }

        // Last-line-of-defense safety check: about to fabricate gap_filled_prefix bytes
        // of zeros on the belief this chunk doesn't exist yet (chunk_exists=false). That
        // belief comes from existing_chunk_size, sourced from metadata_cache — which can
        // be stale specifically on a chunk's first flush in a session (see the open()
        // synchronous-refresh fix above). Real incident, 2026-07-03 (staging nanopir3):
        // dvr.conf's real 111 bytes were zeroed exactly this way, and reproduced even
        // after that fix — the precise staleness cause wasn't fully pinned down, so
        // rather than requiring perfect cache freshness everywhere, treat this as a
        // final, authoritative check instead. Scoped to first-flush-this-session only
        // (is_first_flush_this_session) so it doesn't add an RPC to every legitimate
        // sparse write to a genuinely-new chunk (e.g. VM disk image creation) — only the
        // narrow window where existing_chunk_size hasn't yet been confirmed by this
        // session's own flush history.
        if !chunk_exists && gap_filled_prefix > 0 && is_first_flush_this_session {
            let path_opt = self.inode_to_path.read().unwrap().get(&ino).cloned();
            let fresh_has_chunk = if let Some(path) = path_opt.clone() {
                match self.client.get_file_metadata(&path).await {
                    Ok(Some(fresh)) => {
                        let fresh_chunk0 = fresh.chunk_location_for_idx(chunk_idx).map(|l| l.size);
                        let has_chunk = fresh_chunk0.map(|s| s > 0).unwrap_or(false);
                        debug!("[SIZE TRACE] flush-safety-check ino={} chunk={} path={} fresh_chunks={} fresh_chunk0_size={:?} has_chunk={}",
                            ino, chunk_idx, path, fresh.chunk_locations.len(), fresh_chunk0, has_chunk);
                        if has_chunk {
                            self.metadata_cache.insert(ino, fresh);
                        }
                        has_chunk
                    }
                    Ok(None) => {
                        debug!("[SIZE TRACE] flush-safety-check ino={} chunk={} path={} server_says_not_found", ino, chunk_idx, path);
                        false
                    }
                    Err(e) => {
                        debug!("[SIZE TRACE] flush-safety-check ino={} chunk={} path={} lookup_error={}", ino, chunk_idx, path, e);
                        false
                    }
                }
            } else {
                debug!("[SIZE TRACE] flush-safety-check ino={} chunk={} no_path_known", ino, chunk_idx);
                false
            };
            if fresh_has_chunk {
                warn!("flush_buffer_async_one: ino={} chunk={} fresh-write safety check: server says this chunk actually has real data (metadata_cache was stale) — refusing to send gap-fill zeros over it; will retry with corrected metadata",
                    ino, chunk_idx);
                if let Some(state_arc) = self.write_buffers.get(&ino) {
                    if let Ok(mut state) = state_arc.try_lock() {
                        if let Some(slot) = state.slots.get_mut(&chunk_idx) {
                            slot.flushing = false;
                        }
                    }
                }
                self.notify_chunk_flush_complete(ino, chunk_idx).await;
                anyhow::bail!(
                    "chunk {} fresh-write safety check found real existing data — aborting this attempt to avoid corruption", chunk_idx
                );
            }
        }

        // Normal fresh write path (contiguous data, no gaps)
        // Acquire the per-chunk write lock for the fresh-write path too.
        let _chunk_guard = DfsFilesystem::lock_chunk(&self.chunk_write_locks, ino, chunk_idx).await;

        // Full-chunk replacements rewrite data that already lives somewhere — target the
        // existing replica nodes instead of re-deriving placement via capacity-band
        // randomization (which would migrate the chunk to a different pair on every
        // rewrite as relative capacity shifts cross band boundaries).
        let preferred_nodes: Option<Vec<SocketAddr>> = if is_full_replacement {
            match self.existing_chunk_nodes(ino, chunk_idx).await {
                Some(node_ids) => {
                    let mut addrs = self.client.resolve_node_addrs(&node_ids).await;
                    if addrs.len() >= 2 {
                        // Deterministic pair: same selection regardless of map iteration order,
                        // matching the sorted-first-2 convention used elsewhere for dual_rf.
                        addrs.sort_unstable();
                        addrs.truncate(2);
                        info!("flush_buffer_async_one: ino={} chunk={} full-replacement targeting existing replica nodes {:?}",
                              ino, chunk_idx, addrs);
                        Some(addrs)
                    } else {
                        None
                    }
                }
                None => None,
            }
        } else {
            None
        };

        info!("flush_buffer_async_one: ino={} chunk={} calling write_data_with_cache with {} bytes at file_offset={}",
              ino, chunk_idx, slot_data.len(), file_offset);
        let result = self.client.write_data_with_cache(&slot_data, ino, file_offset, file_id_at_flush_start.unwrap_or_else(dfs_common::FileId::new), preferred_nodes.as_deref()).await;
        match result {
            Ok((_, _, Some(locations))) => {
                let flushed_len = slot_data.len();

                // chunk_cache is already seeded by write_data_with_cache (both the
                // dual-replica and single-node-fallback paths populate it from the same
                // bytes) — no need to clone slot_data again here just to re-insert it.

                // Evict byte_range_cache and zero_gap_table entries for this chunk.
                // chunk_cache was just seeded with the full content above; invalidating
                // sub-range entries here ensures stale reads don't bypass it.
                self.client.invalidate_byte_range_cache_for_chunk(ino, file_offset, CHUNK_SIZE).await;

                // Seed the zero gap table with gaps between dirty ranges.
                // Use slot_data.len() (not CHUNK_SIZE) so we never claim bytes beyond
                // the actually-written content as zero. A trailing gap seeded all the
                // way to CHUNK_SIZE would mask subsequent patch writes to that region —
                // reads would return zeros instead of the patched data (T22b regression).
                // In-chunk gaps between dirty ranges (e.g. qcow2 metadata clusters) are
                // still seeded correctly: they are zero-filled and within slot_data.
                self.client.seed_zero_gap_table(ino, file_offset, slot_data.len(), &dirty_ranges).await;

                // Track flushed size but DON'T remove slot yet.
                // Also store the server-confirmed chunk_id so the next patch
                // uses it as the authoritative base rather than metadata_cache.
                if let Some(state_arc) = self.write_buffers.get(&ino) {
                    let mut state = state_arc.lock().await;
                    state.flushed_sizes.insert(chunk_idx, flushed_len);
                    if let (Some(loc), Some(slot)) = (locations.first(), state.slots.get_mut(&chunk_idx)) {
                        slot.server_chunk_id = Some(loc.chunk_id);
                    }
                    // Update canonical_write_nodes so the next patch targets the correct
                    // nodes. Without this, canonical_write_nodes retains the pre-fallback
                    // pair while server_chunk_id has the new hash (written to different
                    // nodes), causing "chunk data missing" on every subsequent patch.
                    // Unlike the MultiPatch success path above, a full replace (not a
                    // union) is correct here: this is a genuine identity change (fresh
                    // write to a fully new node set), not a same-identity patch where a
                    // replica simply missed one round — so any miss-streak state for the
                    // old node set is now meaningless and cleared with it.
                    if let Some(loc) = locations.first() {
                        if !loc.nodes.is_empty() {
                            state.canonical_node_miss_streak.retain(|(cidx, _), _| *cidx != chunk_idx);
                            state.canonical_write_nodes.insert(chunk_idx, loc.nodes.clone());
                        }
                    }
                }

                // Update recent_chunk_writes for ALL fresh writes so the next open/write
                // can find the correct server-confirmed chunk_id without relying on the
                // leader's metadata (which may be behind by up to one flush cycle when the
                // previous ReplicateChunkLocation arrived while the old guard was active).
                if let Some(loc) = locations.first() {
                    if let Some(file_id) = file_id_at_flush_start {
                        let mut sorted_nodes = loc.nodes.clone();
                        sorted_nodes.sort_unstable();
                        sorted_nodes.truncate(2);
                        self.client.recent_chunk_writes.insert(
                            (ino, chunk_idx),
                            (loc.chunk_id, file_id, std::time::Instant::now(), sorted_nodes),
                        );
                    }
                }

                // Fetch metadata if not cached
                if !self.metadata_cache.contains_key(&ino) {
                    let path_opt = self.inode_to_path.read().unwrap().get(&ino).cloned();
                    if let Some(path) = path_opt {
                        if let Ok(Some(fetched)) = self.client.get_file_metadata(&path).await {
                            self.client.seed_write_seq(fetched.id, fetched.write_seq);
                            self.metadata_cache.insert(ino, fetched);
                        }
                    }
                }

                if let Some(mut meta) = self.metadata_cache.get_mut(&ino) {
                    // If the file was deleted and recreated while the flush was in flight,
                    // the metadata_cache entry now belongs to the new file. Don't contaminate
                    // it with the old file's chunk locations.
                    if file_id_at_flush_start.map(|id| id != meta.id).unwrap_or(false) {
                        info!("flush_buffer_async_one: ino={} chunk={} file replaced during flush (old_id={:?} new_id={}) — discarding", ino, chunk_idx, file_id_at_flush_start, meta.id);
                        if let Some(state_arc) = self.write_buffers.get(&ino) {
                            let mut state = state_arc.lock().await;
                            let discarded_len = state.slots.get(&chunk_idx).map(|s| s.resident()).unwrap_or(0);
                            state.slots.remove(&chunk_idx);
                            if discarded_len > 0 {
                                self.global_buffered_bytes.fetch_sub(
                                    discarded_len.min(self.global_buffered_bytes.load(std::sync::atomic::Ordering::Relaxed)),
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                            }
                        }
                        return Ok(());
                    }
                    if self.truncated_inodes.contains(&ino) {
                        return Ok(());
                    }
                    for loc in &locations {
                        splice_chunk_location(Arc::make_mut(&mut meta.chunk_locations), loc.clone(), &self.client);
                        if let Some(end) = loc.file_offset.map(|o| o + loc.size as u64) {
                            meta.size = meta.size.max(end);
                        }
                    }
                    debug!("[SIZE TRACE] splice ino={} chunk={} spliced_locations={} meta_chunks_after={} meta_chunk0_size_after={:?}",
                        ino, chunk_idx, locations.len(), meta.chunk_locations.len(),
                        meta.chunk_locations.iter().find(|l| l.file_offset.unwrap_or(0) == 0).map(|l| l.size));
                    // Don't clobber an mtime the user explicitly just set via setattr
                    // (utimes/utimensat) — e.g. rsync -a's temp-file restore can land
                    // before this flush completes (T37).
                    // Use contains() not remove(): two concurrent flush tasks for the same
                    // inode both run this check, and remove() would let the second task see
                    // None and stamp now(), clobbering the explicit mtime. The flag is
                    // cleared only in write() when new data arrives.
                    if !self.explicit_mtime_pending.contains(&ino) {
                        meta.modified_at = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                    }
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
                            DfsFilesystem::invalidate_dir_cache(&self.dir_cache, &self.dir_cache_invalidated_at, &parent);
                        }
                    }
                    self.last_metadata_update.insert(ino, std::time::Instant::now());
                    // Do NOT call flush_metadata_sync here. This path runs for individual
                    // fresh-write chunks inside flush_all_pipelined (or background ticker),
                    // where OTHER chunks for the same inode may still be flushing concurrently
                    // (e.g., chunk_74 completes while chunk_0's patch is still in progress).
                    // Reading metadata_cache now captures a STALE snapshot of chunk_0 (pre-patch),
                    // and broadcasting it causes a ghost-reversion: chunk_map on all nodes reverts
                    // to the pre-patch hash whose file was already renamed, causing infinite
                    // ChunkStale retry loops. flush_all_pipelined (line 2275) and flush_buffer_async
                    // (line ~820) both call flush_metadata_sync after ALL chunks complete — that is
                    // the correct place where metadata_cache reflects the full current state.
                    // Update read engine for immediate read-after-write visibility.
                    // Feed only `locations` (this flush's freshly-written chunk(s)), not
                    // the whole (ever-growing) meta.chunk_locations — see the O(n)/O(n^2)
                    // per-flush cost this caused, described where the same fix was applied
                    // in flush_buffer_async_one's patch branch above.
                    let current_size = meta.size;
                    self.client.feed_chunk_locations_to_read_engine(
                        ino, &locations, current_size,
                    ).await;
                }
                // Now that read engine is updated, safe to remove slot — unless new data
                // arrived while the flush was in flight (concurrent writer added bytes).
                // In that case, keep the slot so the next flush cycle sends the new data.
                // Guard: if the buffer was replaced by a new create() while this flush was
                // in flight, don't write flushed_sizes into the new session's buffer.
                if let Some(state_arc) = self.write_buffers.get(&ino) {
                    let mut state = state_arc.lock().await;
                    let buf_id = state.expected_file_id;
                    let id_ok = match (file_id_at_flush_start, buf_id) {
                        (Some(fid), Some(bid)) => fid == bid,
                        _ => true,
                    };
                    if !id_ok {
                        debug!("flush_buffer_async_one: ino={} chunk={} buffer replaced during flush (flush_id={:?} buf_id={:?}) — skipping slot update", ino, chunk_idx, file_id_at_flush_start, buf_id);
                    } else {
                        // Detect concurrent writes via BOTH:
                        //   - data.len() > flushed_len: an append extended the slot
                        //   - last_modified > last_modified_snap: an in-place overwrite of
                        //     already-buffered bytes (slot length unchanged)
                        // The length check alone misses same-region overwrites that arrive
                        // while this fresh write is in flight — e.g. a 4KB rewrite to offset 0
                        // of a chunk that was just filled to CHUNK_SIZE and is being flushed.
                        // Without the last_modified check, that rewrite's dirty_ranges/data are
                        // discarded when the slot is removed below (silent data loss).
                        let new_data_arrived = state.slots.get(&chunk_idx).map(|s| {
                            s.span_end > flushed_len || s.last_modified > last_modified_snap
                        }).unwrap_or(false);
                        if !new_data_arrived {
                            // Subtract the slot's actual resident size (sum of its extents'
                            // allocations), not flushed_len — mirrors what the add side
                            // counted. See resident_bytes()'s doc comment.
                            let removed_size = state.slots.remove(&chunk_idx).map(|s| s.resident()).unwrap_or(0);
                            self.global_buffered_bytes.fetch_sub(
                                removed_size.min(self.global_buffered_bytes.load(std::sync::atomic::Ordering::Relaxed)),
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        } else {
                            state.flushed_sizes.insert(chunk_idx, flushed_len);
                            if let Some(slot) = state.slots.get_mut(&chunk_idx) {
                                slot.flushing = false;
                            }
                            // No global_buffered_bytes subtraction: the slot is KEPT with all
                            // its extents resident — no memory was freed. Subtracting
                            // flushed_len (the padded snapshot length) here could exceed
                            // every dirty byte ever added under sparse extents and wipe the
                            // counter for memory still held; the slot's eventual removal
                            // subtracts resident() and settles the account exactly. (Same
                            // reasoning as the patch path's kept-slot branch above.)
                        }
                    }
                }
                // Metadata commit complete, cleanup done — notify any waiters so next flush can proceed.
                // CRITICAL: Must notify AFTER cleanup (flushing=false or slot removed), otherwise woken
                // tasks see flushing=true and bail, but flush_all_pipelined counts them as in-flight
                // and waits forever.
                self.notify_chunk_flush_complete(ino, chunk_idx).await;
                Ok(())
            }
            Ok((_, _, None)) => {
                // Server accepted write but returned no locations — clear flushing so
                // the next ticker cycle can retry. Don't decrement global_buffered_bytes:
                // the slot is still in the buffer and will be retried.
                if let Some(state_arc) = self.write_buffers.get(&ino) {
                    let mut state = state_arc.lock().await;
                    if let Some(slot) = state.slots.get_mut(&chunk_idx) {
                        slot.flushing = false;
                    }
                }
                self.notify_chunk_flush_complete(ino, chunk_idx).await;
                Ok(())
            }
            Err(e) => {
                // Back off this slot the same way reconstruct_or_abort_for_fresh_write
                // does for its cache-miss case: without this, a replica write that fails
                // for any reason (e.g. ENOSPC — all replica targets rejecting the write)
                // gets retried on every ~100ms background tick at full speed forever, since
                // in_backoff() gates the ticker's slot selection but nothing was setting it
                // for this general failure path. That tight retry loop is what pegged the
                // CPU and fed the 2026-07-13 OOM once local disk filled during benchmarking.
                if let Some(state_arc) = self.write_buffers.get(&ino) {
                    let mut state = state_arc.lock().await;
                    if let Some(slot) = state.slots.get_mut(&chunk_idx) {
                        slot.flushing = false;
                        slot.record_terminal_failure();
                    }
                }
                self.notify_chunk_flush_complete(ino, chunk_idx).await;
                Err(e)
            }
        }
    }

    /// Compute per-server prefetch hints for a MultiPatch about to be dispatched.
    ///
    /// Snapshots all OTHER pending (non-flushing) chunk slots for this inode, resolves
    /// their existing locations, and groups their chunk_ids by server address. The caller
    /// includes the per-addr slice in each MultiPatch request so the server can start
    /// disk reads for those chunks while their patch payloads are still in transit.
    ///
    /// Called fresh at every MultiPatch dispatch point so each successive RPC carries
    /// only the chunks still outstanding — already-dispatched slots are marked flushing=true
    /// and naturally drop out of the snapshot.
    /// Build the per-server prefetch hint map for the upcoming flush wave.
    /// Called once per wave in flush_all_pipelined (not per task) and the result is
    /// written into the shared patch_prefetch_hints Mutex so spawned tasks can read
    /// it synchronously with no .await and no per-task metadata clone.
    async fn compute_wave_prefetch_hints(
        &self,
        ino: u64,
    ) -> HashMap<SocketAddr, Vec<dfs_common::ChunkId>> {
        if std::env::var("DFS_DISABLE_PATCH_PREFETCH").is_ok() {
            return HashMap::new();
        }
        // try_lock: a concurrent write() may hold this; skip hints rather than stall.
        let pending: Vec<u64> = {
            let Some(state_arc) = self.write_buffers.get(&ino) else { return HashMap::new() };
            let Ok(state) = state_arc.try_lock() else { return HashMap::new() };
            state.slots.iter()
                .filter(|(_, s)| !s.is_empty() && !s.flushing)
                .map(|(idx, _)| *idx)
                .collect()
        };
        if pending.is_empty() { return HashMap::new(); }
        let meta = match self.metadata_cache.get(&ino) {
            Some(m) => m.clone(),
            None => return HashMap::new(),
        };
        let node_id_to_addr = self.client.node_id_to_addr_snapshot().await;
        if node_id_to_addr.is_empty() { return HashMap::new(); }
        let mut by_server: HashMap<SocketAddr, Vec<dfs_common::ChunkId>> = HashMap::new();
        for idx in pending {
            if let Some(loc) = meta.chunk_location_for_idx(idx) {
                for &node_id in &loc.nodes {
                    if let Some(&addr) = node_id_to_addr.get(&node_id) {
                        by_server.entry(addr).or_default().push(loc.chunk_id);
                    }
                }
            }
        }
        by_server
    }

    /// Drain ALL dirty slots for `ino` (including partial tail) through a pipeline
    /// capped at FLUSH_ALL_MAX_ITEMS (64) and PIPELINE_MAX_BYTES (16MB).
    /// Used by release() and fsync(). 64 items >> background ticker's 16, so small
    /// patches (e.g. 200 × 1KB) take 4 rounds instead of 13, without flooding servers.
    async fn flush_all_pipelined(&self, ino: u64) -> Result<()> {
        // Serialize concurrent flush_all_pipelined calls for the same inode.
        // Without this, two sync_release handlers closing in quick succession both
        // enter this function, race on slot ownership, and can produce concurrent
        // flush_buffer_async_one calls for the same chunk. The per-inode mutex
        // ensures FIFO: the second caller waits, then on acquiring the lock checks
        // whether data remains and flushes it in order.
        let _pipeline_guard = {
            let lock = self.flush_pipeline_locks
                .entry(ino)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone();
            lock.lock_owned().await
        };

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

        // Sync-flush handle: dual-RF enabled so MultiPatch writes to 2 replicas and
        // returns skipped 3rd-replica addresses. Fresh skip-list accumulator so deferred
        // deletes from this flush session don't bleed into the background ticker's handle.
        let sync_handle = FlushHandle {
            use_dual_rf: true,
            // Fresh Arc per flush_all_pipelined call so concurrent calls for different
            // inodes don't share a hint map and overwrite each other's wave snapshots.
            patch_prefetch_hints: Arc::new(std::sync::Mutex::new(Arc::new(HashMap::new()))),
            ..self.clone()
        };

        let mut first_err: Option<anyhow::Error> = None;

        // Keep dispatching concurrent flush_one_chunk calls until no unclaimed slots remain.
        // Enforce byte limit only (PIPELINE_MAX_BYTES). The item count limit is for
        // the background ticker; fsync/release should drain all dirty slots as fast as
        // possible — small patches are negligible in bytes so the item cap was the
        // binding constraint and serialized what could be a single network round.
        loop {
            // Mark all pending slots as idle on every iteration so flush_one_chunk can claim
            // them. This must be inside the loop: write() calls arriving while a previous
            // iteration's flush_one_chunk tasks were in flight reset last_modified to now().
            // If the marking only happens once (before the loop), those new-write slots are
            // never idle and flush_one_chunk returns immediately without claiming them, causing
            // a busy-spin with pending > 0 but nothing to flush. Marking each iteration is
            // safe: flush_one_chunk waits for write_tasks_in_flight before snapshotting, so
            // in-flight write() data is never dropped.
            if let Some(state_arc) = self.write_buffers.get(&ino) {
                let mut state = state_arc.lock().await;
                let epoch = SystemTime::UNIX_EPOCH;
                for (_, slot) in state.slots.iter_mut() {
                    if !slot.is_empty() && !slot.flushing {
                        slot.last_modified = epoch;
                    }
                }
            }

            // Count how many slots are still pending (unclaimed and non-empty).
            let pending = self.write_buffers.get(&ino).map(|s| {
                s.try_lock().map(|st| {
                    st.slots.values().filter(|sl| !sl.is_empty() && !sl.flushing).count()
                }).unwrap_or(1) // if locked, assume work remains
            }).unwrap_or(0);

            if pending == 0 { break; }

            // Count currently in-flight slots for this inode (claimed, flushing=true).
            // Also sum their DIRTY byte sizes (not slot.data.len()) to enforce the byte
            // limit. slot.data.len() is the nominal zero-padded buffer length — a single
            // random write near the end of an untouched chunk can balloon that to ~4MB
            // while only a few KB is real dirty data. Counting padded length as if it were
            // real cost makes the byte budget bind far earlier than intended (same trap
            // buffered_bytes() already avoids for write back-pressure, see its doc comment).
            let (in_flight_count, in_flight_bytes) = self.write_buffers.get(&ino).map(|s| {
                s.try_lock().map(|st| {
                    let count = st.slots.values().filter(|sl| sl.flushing).count();
                    let bytes: usize = st.slots.values()
                        .filter(|sl| sl.flushing)
                        .map(|sl| sl.dirty_ranges.iter().map(|&(a, b)| b - a).sum::<usize>())
                        .sum();
                    (count, bytes)
                }).unwrap_or((0, 0))
            }).unwrap_or((0, 0));

            // Dispatch up to FLUSH_ALL_MAX_ITEMS (64) concurrent tasks and PIPELINE_MAX_BYTES
            // (16MB) of in-flight data. The item cap prevents saturating the XFS journal on
            // servers under a burst of many small patches; 64 >> 16 (background ticker) so
            // 200 small patches still take ceil(200/64)=4 rounds instead of ceil(200/16)=13.
            let items_available = FLUSH_ALL_MAX_ITEMS.saturating_sub(in_flight_count);
            let bytes_available = PIPELINE_MAX_BYTES.saturating_sub(in_flight_bytes);

            // Estimate how many more items we can dispatch based on both budgets.
            // Peek at pending slots' DIRTY byte sizes (not slot.data.len() — see the
            // in_flight_bytes comment above) to make a reasonable guess.
            let pending_slot_sizes: Vec<usize> = self.write_buffers.get(&ino)
                .and_then(|s| s.try_lock().ok().map(|st| {
                    let mut sizes: Vec<usize> = st.slots.values()
                        .filter(|sl| !sl.is_empty() && !sl.flushing)
                        .map(|sl| sl.dirty_ranges.iter().map(|&(a, b)| b - a).sum::<usize>())
                        .collect();
                    sizes.sort_unstable();
                    sizes
                }))
                .unwrap_or_default();

            let to_dispatch = if pending_slot_sizes.is_empty() {
                0
            } else {
                // Greedily fit as many pending slots as possible within both limits.
                let mut can_fit = 0;
                let mut bytes_so_far = 0;
                for &size in &pending_slot_sizes {
                    if can_fit >= items_available { break; }
                    if bytes_so_far + size > bytes_available { break; }
                    can_fit += 1;
                    bytes_so_far += size;
                }
                can_fit.min(pending)
            };

            if to_dispatch == 0 {
                // Item or byte budget exhausted — wait for in-flight slots to complete.
                tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
                continue;
            }

            // Back-pressure: if the metadata queue is stalling, pause before hammering
            // the server with another wave of MultiPatch RPCs. The server needs breathing
            // room to process PutFileMetadata — adding more chunk writes while it's already
            // saturated only makes the stall worse. Slow down rather than send EIO.
            {
                const META_STALL_THRESHOLD_SECS: u64 = 8;
                const META_BACKOFF_MS: u64 = 200;
                const META_BACKOFF_MAX_MS: u64 = 2000;
                let mut backoff_ms = META_BACKOFF_MS;
                while let Some(age) = self.client.metadata_queue.front_age().await {
                    if age.as_secs() < META_STALL_THRESHOLD_SECS { break; }
                    debug!(
                        "flush_all_pipelined: metadata queue stalled ({}s), throttling wave dispatch for {}ms",
                        age.as_secs(), backoff_ms
                    );
                    tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(META_BACKOFF_MAX_MS);
                }
            }

            // Compute per-server prefetch hints once for this wave and publish to the
            // shared Mutex before spawning tasks. Tasks read it synchronously (no .await),
            // so there is zero async overhead inside the per-task flush hot path.
            {
                let wave_hints = self.compute_wave_prefetch_hints(ino).await;
                *sync_handle.patch_prefetch_hints.lock().unwrap() = Arc::new(wave_hints);
            }

            let mut handles = Vec::new();
            for _ in 0..to_dispatch {
                let h = sync_handle.clone();
                handles.push(tokio::spawn(async move {
                    h.flush_one_chunk(ino, true).await  // urgent: flush any slot, not just full/idle
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

        // Re-feed the read engine so reads after fsync see the latest chunk_ids.
        // flush_buffer_async_one feeds it per-chunk, but a concurrent refresh_engine
        // can race and overwrite with stale server data. Feeding here corrects that.
        // flush_metadata_sync is NOT called from FAP — the release handler owns that
        // responsibility so metadata is only sent after ALL chunks are confirmed written.
        if let Some(meta) = self.metadata_cache.get(&ino).map(|m| m.clone()) {
            self.client.feed_chunk_locations_to_read_engine(
                ino, &meta.chunk_locations, meta.size,
            ).await;
        }

        // Old chunk_ids superseded by this flush's patches/replacements are intentionally
        // left on disk and in the routing table on their original nodes. Because chunk_id
        // is content-addressed (blake3(offset||data)), the same chunk_id can still be the
        // live base for another (file_id, chunk_idx) slot with identical content+offset
        // that hasn't been patched yet — eagerly broadcast-deleting it here could remove
        // data a sibling file's in-flight MultiPatch is about to read. The leader's deep
        // orphan-purge sweep (live_chunk_ids()-based, with grace period) reclaims these
        // once no file's metadata references them anymore.

        Ok(())
    }
}

/// FUSE filesystem implementation for DFS
pub struct DfsFilesystem {
    /// Client for communicating with DFS cluster
    client: Arc<DfsClient>,

    /// Metadata cache: inode -> FileMetadata
    metadata_cache: Arc<DashMap<u64, FileMetadata>>,

    /// Path to inode mapping (forward and reverse)
    path_to_inode: Arc<RwLock<HashMap<String, u64>>>,
    inode_to_path: Arc<RwLock<HashMap<u64, String>>>,

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

    /// Hard ceiling on total write-buffer resident memory across all inodes,
    /// scaled to available system memory at startup. See its computation in
    /// new() for the full rationale (nanopir3-safe, hypervisor-considerate,
    /// still large enough to avoid the RND4K throughput collapse).
    global_write_buffer_cap_bytes: usize,

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

    /// Tracks when each directory was last invalidated (create/mkdir/unlink/rename).
    /// readdir only inserts into dir_cache if the directory wasn't invalidated
    /// DURING the in-flight list_directory fetch.
    dir_cache_invalidated_at: Arc<DashMap<String, std::time::Instant>>,

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

    /// See FlushHandle::explicit_mtime_pending for details.
    explicit_mtime_pending: Arc<dashmap::DashSet<u64>>,

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

    /// Paths for which a create() is currently in flight. lookup() must not overwrite
    /// metadata_cache for these paths — the create()'s fresh metadata is authoritative.
    pending_creates: Arc<dashmap::DashSet<String>>,

    /// Inodes for which a background getattr refresh is already in flight.
    /// Prevents unbounded spawning of concurrent refresh tasks (one per inode max).
    refreshing_inodes: Arc<dashmap::DashSet<u64>>,

    /// Inodes that must use the direct (non-buffered) write path.
    /// Set at create()/open() time for .db and .db-journal files so the routing
    /// decision is available immediately when the first write() arrives — before
    /// path_to_inode or metadata_cache is populated by the async create task.
    direct_write_inodes: Arc<dashmap::DashSet<u64>>,

    /// Per-(inode, chunk_idx) mutex that serializes all write paths on the same chunk.
    /// Any path that does a direct network write (PatchChunk, WriteChunk) followed by
    /// a metadata_cache update must hold this lock for the duration. Writes to different
    /// chunks of the same inode proceed in parallel; writes to the same chunk are ordered
    /// by arrival. Entries are created on demand and removed when the inode is fully closed
    /// (write_buffers.remove guard in release()). The outer DashMap key is ino; the inner
    /// DashMap key is chunk_idx, so lock granularity is per chunk, not per file.
    chunk_write_locks: Arc<DashMap<u64, Arc<DashMap<u64, Arc<tokio::sync::Mutex<()>>>>>>,

    /// Per-inode count of release() flush tasks still in flight.
    /// Sequential writes wait for this to reach zero for the specific inode
    /// before opening, ensuring the previous release() has fully committed.
    /// destroy() waits for all to reach zero before exiting.
    release_in_flight: Arc<DashMap<u64, Arc<std::sync::atomic::AtomicUsize>>>,

    /// Per-inode count of write() tasks still running (spawned but not yet written into the slot).
    /// release() waits for this to reach zero before flush so we don't flush an incomplete slot.
    write_tasks_in_flight: Arc<DashMap<(u64, u64), Arc<std::sync::atomic::AtomicUsize>>>,


    /// Total bytes currently held across all per-inode write buffers.
    /// Incremented on every buffered write; decremented by flush_buffer_async on success.
    /// Shared with FlushHandle so both sides see the same counter.
    global_buffered_bytes: Arc<std::sync::atomic::AtomicUsize>,

    /// Notification channel to wake up flush workers immediately when chunks become full.
    /// Eliminates the 0-50ms polling delay from the ticker-based approach.
    /// Shared with FlushHandle so write operations can trigger immediate flushing.
    flush_notify: Arc<tokio::sync::Notify>,

    /// Inodes for which a write buffer was created this session.
    /// destroy() uses this to skip flushing metadata for read-only files whose
    /// metadata_cache was populated only by warmup — flushing those with a
    /// higher write_seq would reinforce any DB corruption already present.
    written_inodes: Arc<dashmap::DashSet<u64>>,

    /// Inodes unlinked while one or more fds were still open (POSIX deferred delete).
    /// Maps ino → path. Defers path_to_inode removal and server delete until the last
    /// fd closes so existing fds can continue writing without ENOENT.
    unlinked_while_open: Arc<DashMap<u64, String>>,
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

        // Scale the write-buffer cap to available memory, the same way chunk_cache
        // and byte_range_cache already do — a flat cap sized for a 16GB server would
        // be dangerous on a low-memory client like nanopir3 (2GB total, ~900MB
        // available). Kept deliberately conservative even on large boxes: dfs-client
        // often runs alongside the actual workload it exists to serve (e.g. server5
        // is a Proxmox hypervisor — its RAM is for VMs, not for us to help ourselves
        // to a quarter of it just because it happens to be free at startup). The max
        // tier is sized to comfortably cover kdiskmark's typical ~1GiB random-write
        // test region (256 chunks) — real-world testing on server5 showed the
        // previous 256MB cap collapsed RND4K random-write throughput ~40x (see
        // GLOBAL_WRITE_BUFFER_CAP_BYTES's history) because random access across a
        // multi-GB disk touches a new chunk almost every write, each paying the full
        // gap-fill cost (resident_bytes()) against the cap — but there's no reason
        // to go further than that working set just because more RAM is technically free.
        //
        // Also computed as target_pct of available memory MINUS what chunk_cache +
        // byte_range_cache already reserved from that same "available" snapshot
        // (client.reserved_cache_bytes) — sizing each cache as an independent
        // percentage of the same figure would let their worst cases silently compound.
        let available_bytes = dfs_common::get_available_memory().unwrap_or(1024 * 1024 * 1024);
        let available_mb = available_bytes / (1024 * 1024);
        let (wb_target_pct, wb_min_bytes, wb_max_bytes): (u8, usize, usize) =
            if available_mb < 256 {
                (4, 16 * 1024 * 1024, 32 * 1024 * 1024)
            } else if available_mb < 512 {
                (8, 32 * 1024 * 1024, 64 * 1024 * 1024)
            } else if available_mb < 1024 {
                (15, 64 * 1024 * 1024, 128 * 1024 * 1024)
            } else if available_mb < 2048 {
                (18, 96 * 1024 * 1024, 256 * 1024 * 1024)
            } else if available_mb < 4096 {
                (20, 128 * 1024 * 1024, 512 * 1024 * 1024)
            } else {
                (15, 256 * 1024 * 1024, 1024 * 1024 * 1024)
            };
        let target_bytes = (available_bytes as f64 * (wb_target_pct as f64 / 100.0)) as usize;
        let computed_cap_bytes = target_bytes
            .saturating_sub(client.reserved_cache_bytes)
            .max(wb_min_bytes)
            .min(wb_max_bytes);
        // DFS_WRITE_BUFFER_CAP_MB overrides the computed value entirely (no min/max
        // clamping applied to an explicit override) — for A/B testing cap sizing
        // against real workloads (e.g. kdiskmark RND4K) without a rebuild. Unset in
        // normal operation; the memory-scaled computation above is what actually ships.
        let env_override_mb = std::env::var("DFS_WRITE_BUFFER_CAP_MB").ok()
            .and_then(|s| s.parse::<usize>().ok());
        let global_write_buffer_cap_bytes = match env_override_mb {
            Some(mb) => {
                let bytes = mb * 1024 * 1024;
                info!("Write buffer cap sizing: OVERRIDDEN via DFS_WRITE_BUFFER_CAP_MB={} MB (computed value would have been {} MB: {} MB available, {}% target, {} MB reserved)",
                      mb, computed_cap_bytes / (1024 * 1024), available_mb, wb_target_pct,
                      client.reserved_cache_bytes / (1024 * 1024));
                bytes
            }
            None => {
                info!("Write buffer cap sizing: {} MB available, {}% target, {} MB reserved by other caches -> {} MB",
                      available_mb, wb_target_pct, client.reserved_cache_bytes / (1024 * 1024),
                      computed_cap_bytes / (1024 * 1024));
                computed_cap_bytes
            }
        };

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

        // Start background chunk-location batch drain worker.
        client.start_chunk_location_batch_worker(&runtime);

        // Start background sweeper evicting idle range_fetch_node_limit entries.
        client.start_range_fetch_limit_sweeper(&runtime);

        // Start background sweeper evicting stale read_write_seq_cache entries.
        client.start_read_write_seq_cache_sweeper(&runtime);

        // Start background sweeper pruning stale hot/cold fold-classification entries.
        client.start_hot_chunk_sweeper(&runtime);

        let metadata_cache = Arc::new(DashMap::<u64, FileMetadata>::new());
        let path_to_inode = Arc::new(RwLock::new(HashMap::<String, u64>::new()));
        let inode_to_path = Arc::new(RwLock::new(HashMap::<u64, String>::new()));
        let next_inode = Arc::new(RwLock::new(2)); // Start at 2, root is 1
        let write_buffers_for_cleanup = Arc::new(DashMap::<u64, Arc<Mutex<InodeWriteState>>>::new());
        let flush_in_flight_shared: Arc<RwLock<Option<Arc<DashMap<u64, usize>>>>> =
            Arc::new(RwLock::new(None));
        let truncated_inodes_shared: Arc<dashmap::DashSet<u64>> = Arc::new(dashmap::DashSet::new());
        let explicit_mtime_pending_shared: Arc<dashmap::DashSet<u64>> = Arc::new(dashmap::DashSet::new());
        let last_metadata_update_shared: Arc<DashMap<u64, std::time::Instant>> =
            Arc::new(DashMap::new());
        let last_bg_metadata_push_shared: Arc<DashMap<u64, std::time::Instant>> =
            Arc::new(DashMap::new());

        let write_open_counts: Arc<DashMap<u64, usize>> = Arc::new(DashMap::new());
        let open_counts: Arc<DashMap<u64, usize>> = Arc::new(DashMap::new());

        let dir_cache_shared: Arc<DashMap<String, (Vec<FileMetadata>, std::time::Instant)>> =
            Arc::new(DashMap::new());
        let dir_cache_invalidated_at_shared: Arc<DashMap<String, std::time::Instant>> =
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
        let write_tasks_in_flight_shared: Arc<DashMap<(u64, u64), Arc<std::sync::atomic::AtomicUsize>>> = Arc::new(DashMap::new());
        let chunk_write_locks_shared: Arc<DashMap<u64, Arc<DashMap<u64, Arc<tokio::sync::Mutex<()>>>>>> = Arc::new(DashMap::new());
        let flush_pipeline_locks_shared: Arc<DashMap<u64, Arc<tokio::sync::Mutex<()>>>> = Arc::new(DashMap::new());
        let release_in_flight_shared: Arc<DashMap<u64, Arc<std::sync::atomic::AtomicUsize>>> = Arc::new(DashMap::new());

        // Start background task to flush expired write buffers (if buffering enabled)
        if write_buffer_enabled {
            let write_buffers_clone = write_buffers_for_cleanup.clone();
            let client_for_cleanup = client.clone();
            let metadata_cache_for_cleanup = metadata_cache.clone();
            let write_open_counts_for_bg = write_open_counts.clone();
            let path_to_inode_for_bg = path_to_inode.clone();
            let inode_to_path_for_bg = inode_to_path.clone();
            let chunk_write_locks_for_bg = chunk_write_locks_shared.clone();
            let release_in_flight_for_bg = release_in_flight_shared.clone();
            // in_flight: per-inode count of chunk-flush tasks currently running.
            // The ticker keeps this at most PIPELINE_MAX_ITEMS per inode, so each
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
                last_bg_metadata_push: last_bg_metadata_push_shared.clone(),
                dir_cache: dir_cache_shared.clone(),
                dir_cache_invalidated_at: dir_cache_invalidated_at_shared.clone(),
                path_to_inode: path_to_inode_for_bg.clone(),
                inode_to_path: inode_to_path_for_bg.clone(),
                truncated_inodes: truncated_inodes_shared.clone(),
                explicit_mtime_pending: explicit_mtime_pending_shared.clone(),
                flush_runtime: flush_runtime.clone(),
                global_buffered_bytes: global_buffered_bytes.clone(),
                flush_notify: flush_notify.clone(),
                write_tasks_in_flight: write_tasks_in_flight_shared.clone(),
                chunk_write_locks: chunk_write_locks_for_bg.clone(),
                flush_pipeline_locks: flush_pipeline_locks_shared.clone(),
                use_dual_rf: false,
                write_open_counts: write_open_counts_for_bg.clone(),
                patch_prefetch_hints: Arc::new(std::sync::Mutex::new(Arc::new(HashMap::new()))),
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

                    // For each inode: if it has full slots and fewer than PIPELINE_MAX_ITEMS
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
                        if current_in_flight >= PIPELINE_MAX_ITEMS { continue; }

                        let state_arc = match write_buffers_clone.get(&ino) {
                            Some(a) => a.clone(),
                            None => continue,
                        };
                        let state = state_arc.lock().await;
                        // Dual cap: item count (PIPELINE_MAX_ITEMS) lets many small patches
                        // pipeline freely; the byte cap bounds the rarer case of a chunk that
                        // genuinely needs to send a lot of real data (e.g. is_full_replacement).
                        // Must count DIRTY bytes, not slot.data.len() — a slot touched for the
                        // first time this session can be zero-padded up to its write offset
                        // (see write_at), so a single random write near the end of a chunk
                        // makes data.len() ~4MB while real dirty content is a few KB. Counting
                        // padded length here made this gate bind far earlier than intended —
                        // worse than having no byte cap at all (regression caught by re-testing
                        // RND4K Q32T1 write after first adding this check).
                        let in_flight_bytes: usize = state.slots.values()
                            .filter(|s| s.flushing)
                            .map(|s| s.dirty_ranges.iter().map(|&(a, b)| b - a).sum::<usize>())
                            .sum();
                        if in_flight_bytes >= PIPELINE_MAX_BYTES { drop(state); continue; }
                        // Backoff only gates this opportunistic background dispatch, not
                        // full_slot_indices() itself — fsync/release/flush_all_pipelined
                        // callers elsewhere need an accurate answer immediately, not one
                        // silently suppressed by a terminal-failure backoff the caller
                        // doesn't know about.
                        let has_full = state.slots.iter()
                            .any(|(_, s)| s.is_full() && !s.flushing && !s.in_backoff());
                        let no_active_writers = write_open_counts_for_bg
                            .get(&ino).map(|c| *c == 0).unwrap_or(true);
                        // Flush idle slots with no new writes for >500ms, even with active
                        // writers. This drains partial slots from random-write workloads
                        // (VM disk patches) that never fill a full 4MB chunk. Without this,
                        // the buffer fills up and new writes hit the back-pressure timeout.
                        // Exception: skip if a release() flush task is already in flight for
                        // this inode — it will handle the slot, and racing it here would cause
                        // the stale-write guard to discard a legitimate second write (T7 race).
                        const STALE_FLUSH_MS: u128 = 2_000;
                        let release_inflight = release_in_flight_for_bg
                            .get(&ino).map(|c| c.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0);
                        // Skip if flush_all_pipelined is actively running for this inode.
                        // Without this, the ticker races with fsync/release: it patches a
                        // chunk just before fsync reads metadata_cache, so fsync uses the
                        // freshly-patched chunk_id as its base — but any replica that missed
                        // the ticker's patch returns "stale" and the fsync has to retry.
                        // These cascading retries slow down the pipeline and cause back-pressure.
                        let pipeline_busy = flush_handle_for_bg.flush_pipeline_locks
                            .get(&ino)
                            .map(|lock| lock.try_lock().is_err())
                            .unwrap_or(false);
                        if pipeline_busy { drop(state); continue; }
                        let has_stale = release_inflight == 0 && state.slots.iter().any(|(_, s)| {
                            !s.is_empty() && !s.flushing && !s.in_backoff() && s.is_idle() &&
                            s.last_modified.elapsed().map(|e| e.as_millis()).unwrap_or(0) >= STALE_FLUSH_MS
                        });
                        let has_idle = no_active_writers && state.slots.iter().any(|(_, s)| {
                            s.is_idle() && !s.is_empty() && !s.flushing && !s.in_backoff()
                        });
                        // Event-driven, no time delay and no no_active_writers requirement —
                        // an abandoned slot (write moved to a different chunk) is definitively
                        // done regardless of what else the file's writers are doing elsewhere.
                        // See `abandoned`'s doc comment for why this is safe for DVR too.
                        let has_abandoned = state.slots.iter()
                            .any(|(_, s)| s.abandoned && !s.is_empty() && !s.flushing && !s.in_backoff());
                        // Safety net for slots that keep getting revisited (so never go idle,
                        // never get abandoned via cross-chunk move, and never hit is_full())
                        // — flush once meaningfully dirty regardless of activity elsewhere.
                        let has_dirty_threshold = state.slots.iter().any(|(_, s)| {
                            !s.flushing && !s.in_backoff() && s.is_fragmented()
                                && s.dirty_bytes() >= SLOT_DIRTY_FLUSH_THRESHOLD_BYTES
                        });
                        drop(state);
                        if !has_full && !has_idle && !has_stale && !has_abandoned && !has_dirty_threshold { continue; }

                        // Fill the gap to PIPELINE_MAX_ITEMS in this tick instead of dispatching
                        // just one task. A single-task-per-tick dispatch rate can't keep the
                        // pipeline full once chunk dispersion is wide enough that a self-refilling
                        // task exhausts eligible work after only 1-2 flushes — confirmed via a
                        // local diagnostic (2026-07-14): under a 512MB/128-chunk wide-random-write
                        // workload, most self-refill loops exited at spin_count=1, and in_flight
                        // drained all the way to 0 far more often than it approached the cap.
                        // Ramping back up at only +1 task per 50ms tick left effective concurrency
                        // stuck in the single digits regardless of PIPELINE_MAX_ITEMS=32, capping
                        // throughput near the single-flush RPC latency instead of the pipeline's
                        // real capacity. Over-dispatching here is safe: each spawned task
                        // independently gates its own work via has_flushable_slot() before doing
                        // anything, so a task spawned with nothing left to do just exits
                        // immediately — cheap and self-correcting, not wrong.
                        let gap = PIPELINE_MAX_ITEMS.saturating_sub(current_in_flight);
                        for _ in 0..gap {
                        // Increment before spawning to prevent a second dispatch racing
                        // in the same tick before the task starts.
                        *in_flight.entry(ino).or_insert(0) += 1;

                        let handle = flush_handle_for_bg.clone();
                        let in_flight_task = in_flight.clone();
                        let flush_rt = handle.flush_runtime.clone();

                        // SCHEDTIMING: time from spawn() to the task actually starting to run —
                        // a direct measurement of flush_runtime worker-thread contention. If this
                        // grows as more tasks pile in, the 8-worker-thread pool (fuse_impl.rs's
                        // flush_runtime construction) is the bottleneck, not network/server time.
                        // Added 2026-07-14 alongside SFWTIMING/WRITETIMING for the same
                        // regression investigation.
                        let spawn_at = std::time::Instant::now();
                        flush_rt.spawn(async move {
                            let sched_delay_ms = spawn_at.elapsed().as_secs_f64() * 1000.0;
                            if sched_delay_ms > 5.0 {
                                info!("SCHEDTIMING flush_runtime ino={} sched_delay_ms={:.1}", ino, sched_delay_ms);
                            }
                            // Flush one chunk, then keep looping as long as:
                            //   - more full slots exist for this inode, AND
                            //   - we are the only task holding the in_flight slot
                            //     (i.e. we haven't been displaced by a concurrent task).
                            // This self-refilling loop is what keeps the pipeline truly full:
                            // each task fills its own pipeline slot back-to-back without
                            // waiting up to 100ms for the ticker to notice the vacancy.
                            //
                            // Safety valve: has_flushable_slot() must exactly mirror
                            // flush_one_chunk's own selection criteria, or this loop can spin
                            // forever finding "more work" that flush_one_chunk(ino, false)
                            // itself declines to touch (returns Ok(()) without flushing),
                            // permanently pinning one of the PIPELINE_MAX_ITEMS concurrency
                            // slots — exactly what happened during 2026-07-05 kdiskmark
                            // testing (has_flushable_slot was missing !in_backoff()). If this
                            // ever recurs for some other reason, cap the spin and surface it
                            // loudly rather than hanging silently forever.
                            let mut spin_count: u32 = 0;
                            const SPIN_WARN_THRESHOLD: u32 = 200;
                            loop {
                                spin_count += 1;
                                if spin_count == SPIN_WARN_THRESHOLD {
                                    tracing::error!(
                                        "Background flush self-refill loop for inode {} has spun {} times without exiting — \
                                         possible has_flushable_slot()/flush_one_chunk mismatch (see comment). Breaking to avoid \
                                         permanently pinning a pipeline slot; next tick will retry.",
                                        ino, spin_count
                                    );
                                    break;
                                }
                                if let Err(e) = handle.flush_one_chunk(ino, false).await {
                                    tracing::error!("Background flush failed for inode {}: {}", ino, e);
                                    break;
                                }
                                // Notify leader of current chunk locations after each successful
                                // flush so chunk_map stays populated throughout long writes
                                // (e.g., recordings where reads happen during the write session).
                                // MetadataQueue deduplicates by file_id so concurrent flushes
                                // only deliver one update; the final flush_metadata_sync at
                                // release always wins via write_seq ordering.
                                if let Some(meta) = handle.metadata_cache.get(&ino).map(|m| m.clone()) {
                                    if !meta.chunk_locations.is_empty() {
                                        handle.client.enqueue_metadata(&meta).await;
                                    }
                                }
                                // Check whether more flushable slots remain (full, abandoned,
                                // idle, or over the dirty threshold — not just full).
                                let has_more = handle.write_buffers.get(&ino).map(|s| {
                                    s.try_lock().map(|st| st.has_flushable_slot()).unwrap_or(false)
                                }).unwrap_or(false);
                                if !has_more { break; }
                                // Only continue if there's a spare pipeline slot for us.
                                // If another task already filled it (ticker dispatched a sibling),
                                // exit so we don't over-subscribe.
                                let current = in_flight_task.get(&ino).map(|v| *v).unwrap_or(0);
                                if current > PIPELINE_MAX_ITEMS { break; }
                                // Same dual cap as the dispatch site: stop refilling if other
                                // concurrent flushes for this inode are already holding
                                // PIPELINE_MAX_BYTES worth of real dirty data. Counts dirty_ranges,
                                // not slot.data.len() — see the dispatch-site comment above for why.
                                let bytes_now: usize = handle.write_buffers.get(&ino)
                                    .and_then(|s| s.try_lock().ok().map(|st| {
                                        st.slots.values().filter(|sl| sl.flushing)
                                            .map(|sl| sl.dirty_ranges.iter().map(|&(a, b)| b - a).sum::<usize>())
                                            .sum()
                                    }))
                                    .unwrap_or(0);
                                if bytes_now >= PIPELINE_MAX_BYTES { break; }
                            }
                            // Decrement; remove entry when it reaches zero.
                            let mut entry = in_flight_task.entry(ino).or_insert(0);
                            if *entry > 0 { *entry -= 1; }
                            if *entry == 0 { drop(entry); in_flight_task.remove(&ino); }
                        });
                        }
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
            chunk_locations: Arc::new(Vec::new()),
            write_seq: 0,
            symlink_target: None,
        };

        metadata_cache.insert(1, root_metadata);
        path_to_inode.write().unwrap().insert("/".to_string(), 1);
        inode_to_path.write().unwrap().insert(1, "/".to_string());

        // Build FlushHandle before moving fields into the struct
        let flush_handle = FlushHandle {
            client: client.clone(),
            write_buffers: write_buffers_for_cleanup.clone(),
            metadata_cache: metadata_cache.clone(),
            flush_in_flight: flush_in_flight_shared.clone(),
            last_metadata_update: last_metadata_update_shared.clone(),
            last_bg_metadata_push: last_bg_metadata_push_shared.clone(),
            dir_cache: dir_cache_shared.clone(),
            dir_cache_invalidated_at: dir_cache_invalidated_at_shared.clone(),
            path_to_inode: path_to_inode.clone(),
            inode_to_path: inode_to_path.clone(),
            truncated_inodes: truncated_inodes_shared.clone(),
            explicit_mtime_pending: explicit_mtime_pending_shared.clone(),
            flush_runtime: flush_runtime.clone(),
            global_buffered_bytes: global_buffered_bytes.clone(),
            flush_notify: flush_notify.clone(),
            write_tasks_in_flight: write_tasks_in_flight_shared.clone(),
            chunk_write_locks: chunk_write_locks_shared.clone(),
            flush_pipeline_locks: flush_pipeline_locks_shared.clone(),
            use_dual_rf: false,
            write_open_counts: write_open_counts.clone(),
            patch_prefetch_hints: Arc::new(std::sync::Mutex::new(Arc::new(HashMap::new()))),
        };

        Ok(Self {
            client,
            metadata_cache,
            path_to_inode,
            inode_to_path,
            next_inode,
            root_inode: 1,
            runtime,
            write_counters: Arc::new(RwLock::new(HashMap::new())),
            write_buffer_enabled,
            global_write_buffer_cap_bytes,
            write_buffers: write_buffers_for_cleanup,
            last_metadata_update: last_metadata_update_shared,
            last_chunk_cache: Arc::new(RwLock::new(None)),
            last_warm_offset: Arc::new(DashMap::new()),
            chunk_offset_cache: Arc::new(DashMap::new()),
            dir_cache: dir_cache_shared,
            dir_cache_invalidated_at: dir_cache_invalidated_at_shared,
            statfs_cache: Arc::new(RwLock::new(None)),
            lock_manager: Arc::new(LockManager::new()),
            buffer_flush_threshold,
            write_open_counts,
            open_counts,
            size_high_water: Arc::new(DashMap::new()),
            truncated_inodes: truncated_inodes_shared,
            explicit_mtime_pending: explicit_mtime_pending_shared,
            flush_in_flight: flush_in_flight_shared,
            flush_runtime,
            read_runtime,
            flush_handle,
            pending_deletes: Arc::new(dashmap::DashSet::new()),
            unlinked_while_open: Arc::new(DashMap::new()),
            pending_creates: Arc::new(dashmap::DashSet::new()),
            refreshing_inodes: Arc::new(dashmap::DashSet::new()),
            direct_write_inodes: Arc::new(dashmap::DashSet::new()),
            chunk_write_locks: chunk_write_locks_shared,
            release_in_flight: release_in_flight_shared,
            write_tasks_in_flight: write_tasks_in_flight_shared,
            global_buffered_bytes,
            flush_notify,
            written_inodes: Arc::new(dashmap::DashSet::new()),
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

    /// Cheaply-cloneable handle carrying everything needed to drain write buffers
    /// and commit pending metadata. Captured from outside (e.g. main.rs, before
    /// the filesystem is moved into spawn_mount2) so a SIGTERM/SIGINT handler can
    /// drain on shutdown even when fusermount -u never runs destroy() — e.g. when
    /// unmount fails with "device or resource busy" and systemd just kills the process.
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            flush_handle: self.flush_handle.clone(),
            release_in_flight: self.release_in_flight.clone(),
            written_inodes: self.written_inodes.clone(),
        }
    }

    /// Acquire the per-(ino, chunk_idx) write lock, creating it on demand.
    /// Returns the guard; dropping it releases the lock.
    async fn lock_chunk(chunk_write_locks: &Arc<DashMap<u64, Arc<DashMap<u64, Arc<tokio::sync::Mutex<()>>>>>>, ino: u64, chunk_idx: u64) -> tokio::sync::OwnedMutexGuard<()> {
        let inode_map = chunk_write_locks
            .entry(ino)
            .or_insert_with(|| Arc::new(DashMap::new()))
            .clone();
        let chunk_mutex = inode_map
            .entry(chunk_idx)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        chunk_mutex.lock_owned().await
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

    /// Flush all dirty chunks for `ino` using a hybrid-limited pipeline (16 items or 16MB).
    async fn flush_all_pipelined(&self, ino: u64) -> Result<()> {
        self.flush_handle.flush_all_pipelined(ino).await
    }

    /// Convert FileMetadata to FUSE FileAttr
    fn metadata_to_attr(&self, ino: u64, metadata: &FileMetadata) -> FileAttr {
        Self::metadata_to_attr_static(ino, metadata)
    }

    /// Invalidate the dir_cache entry for `dir_path` and record the invalidation
    /// timestamp. readdir async tasks check this timestamp after their in-flight
    /// list_directory() fetch returns; if the directory was invalidated during the
    /// fetch, the (potentially stale) response is discarded rather than cached.
    fn invalidate_dir_cache(
        dir_cache: &DashMap<String, (Vec<FileMetadata>, std::time::Instant)>,
        dir_cache_invalidated_at: &DashMap<String, std::time::Instant>,
        dir_path: &str,
    ) {
        dir_cache.remove(dir_path);
        dir_cache_invalidated_at.insert(dir_path.to_string(), std::time::Instant::now());
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

        // Fallback: check inode_to_path for the parent inode. This covers the case
        // where the kernel holds a directory inode from before a client restart but
        // the metadata_cache is empty (in-memory only, lost on restart).
        let parent_path = parent_path_opt.or_else(|| {
            self.inode_to_path.read().unwrap().get(&parent).cloned()
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
            let inode_to_path = self.inode_to_path.clone();
            let next_inode = self.next_inode.clone();
            let last_metadata_update = self.last_metadata_update.clone();
            let dir_cache = self.dir_cache.clone();
            let write_open_counts_warmup = self.write_open_counts.clone();
            // Signal channel so init() can block until warmup finishes. Per the FUSE protocol,
            // the kernel will not dispatch any operations (lookup, read, write, …) until it
            // receives our FUSE_INIT reply. Blocking here therefore prevents any filesystem
            // access until the cache is warm — the DVR and similar daemons are guaranteed to
            // see a fully populated directory/metadata cache on their very first operation.
            let (warmup_tx, warmup_rx) = tokio::sync::oneshot::channel::<usize>();
            self.runtime.spawn(async move {
                info!("Startup: warming metadata cache from leader");
                let files = match client.list_all_files().await {
                    Ok(f) => f,
                    Err(e) => { warn!("Startup warm: {}", e); let _ = warmup_tx.send(0); return; }
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
                                inode_to_path.write().unwrap().insert(v, file.path.clone());
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
                    // Don't overwrite metadata for an inode currently open for writing.
                    // The write path keeps in-memory chunk_ids authoritative between
                    // flush_buffer_async_one and flush_metadata_sync; a stale server
                    // snapshot here would clobber those ids and corrupt the committed state.
                    let is_open_for_write = write_open_counts_warmup.get(&ino).map(|c| *c > 0).unwrap_or(false);
                    if !is_open_for_write {
                        metadata_cache.insert(ino, file.clone());
                    }
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
                let _ = warmup_tx.send(count);
            });
            // Block until the warmup task completes. init() runs on a FUSE dispatch thread;
            // blocking here is safe because the kernel sends no further FUSE ops until it
            // receives our FUSE_INIT reply (which only happens when init() returns).
            let _ = warmup_rx.blocking_recv();
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
        let handle = self.shutdown_handle();
        self.block_on(handle.drain());
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
        let cached_write_seq = {
            let path_map = self.path_to_inode.read().unwrap();
            if let Some(&ino) = path_map.get(&path) {
                self.metadata_cache.get(&ino).map(|m| m.write_seq)
            } else {
                None
            }
        };

        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let inode_to_path = self.inode_to_path.clone();
        let next_inode = self.next_inode.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let write_open_counts = self.write_open_counts.clone();
        let pending_creates = self.pending_creates.clone();

        self.runtime.spawn(async move {
            let result = client.get_file_metadata_conditional(&path, cached_write_seq).await;

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
                            inode_to_path.write().unwrap().insert(ino, path.clone());
                            ino
                        }
                    };
                    client.seed_write_seq(metadata.id, metadata.write_seq);
                    // Don't overwrite metadata for an inode currently open for writing,
                    // or a path whose create() is still in flight. In both cases our local
                    // metadata is authoritative and the server may have stale chunk data.
                    let is_open_for_write = write_open_counts.get(&ino)
                        .map(|c| *c > 0).unwrap_or(false);
                    let create_in_flight = pending_creates.contains(&path);
                    if !is_open_for_write && !create_in_flight {
                        metadata_cache.insert(ino, metadata.clone());
                        last_metadata_update.insert(ino, std::time::Instant::now());
                    }

                    let attr = DfsFilesystem::metadata_to_attr_static(ino, &metadata);
                    reply.entry(&Duration::ZERO, &attr, 0);
                }
                Ok(None) => {
                    // Either 304 not-modified (cache still valid) OR 404 not-found.
                    if cached_write_seq.is_some() {
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
                                    inode_to_path.write().unwrap().insert(ino, path.clone());
                                    ino
                                }
                            };
                            client.seed_write_seq(metadata.id, metadata.write_seq);
                            let is_open_for_write = write_open_counts.get(&ino)
                                .map(|c| *c > 0).unwrap_or(false);
                            let create_in_flight = pending_creates.contains(&path);
                            if !is_open_for_write && !create_in_flight {
                                metadata_cache.insert(ino, metadata.clone());
                                last_metadata_update.insert(ino, std::time::Instant::now());
                            }
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
        info!("open: ino={} flags=0x{:x} release_in_flight={} write_tasks_in_flight={}",
              ino, flags, release_count,
              write_tasks_in_flight_for_inode(&self.write_tasks_in_flight, ino));

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
            // Mark inode as write-open so reads bypass the chunk cache for this session.
            // Synchronous — happens before open() returns, so the app's first read
            // after open always fetches fresh data from the server.
            self.client.write_open_inodes.insert(ino);

            let is_first_writer = self.write_open_counts.get(&ino).map(|c| *c == 1).unwrap_or(true);
            if is_first_writer {
                // O_TRUNC: skip — the writer is about to replace file content wholesale, so
                // stale chunk locations are irrelevant, and the O_TRUNC branch below removes
                // the metadata_cache entry outright to force a clean slate anyway.
                //
                // Non-O_TRUNC: MUST refresh synchronously (not backgrounded) before returning
                // from open(). Without this, a write session that partially rewrites an
                // existing file (not a full O_TRUNC replace) can find a stale or entirely
                // missing metadata_cache entry — most likely right after a client restart,
                // before this file has been touched again — and flush_buffer_async_one's
                // existing_chunk_size computation falls back to 0, misclassifying the write
                // as "chunk doesn't exist yet" (fresh-write path) instead of an append/patch.
                // The fresh-write path then sends the *whole* buffer, including synthetic
                // gap-fill zeros for any byte range the app didn't touch this session —
                // silently overwriting real existing content with zeros.
                //
                // Real incident, 2026-07-03 (staging nanopir3): dvr.conf's first 111 bytes
                // were zeroed exactly this way when the DVR container appended one line
                // (O_RDWR, no O_TRUNC) moments after a dfs-client restart.
                //
                // Only pay the RPC when the in-memory cache actually looks like it might be
                // missing real chunk data (absent entirely, or present with zero chunks) —
                // a cheap, no-network check. A hot-reopen workload (e.g. QEMU cycling a VM
                // disk's fd every few seconds via BLKFLSBUF) already has a populated,
                // non-empty entry from its own prior open in the same long-lived session,
                // so it never pays this cost; only a genuinely cold cache does.
                let cached_chunk0_size = self.metadata_cache.get(&ino)
                    .and_then(|m| m.chunk_locations.iter().find(|l| l.file_offset.unwrap_or(0) == 0).map(|l| l.size));
                let cache_looks_empty = self.metadata_cache.get(&ino)
                    .map(|m| m.chunk_locations.is_empty())
                    .unwrap_or(true);
                debug!("[SIZE TRACE] open-write-check ino={} is_trunc={} cache_present={} cache_looks_empty={} cached_chunk0_size={:?}",
                    ino, is_trunc, self.metadata_cache.get(&ino).is_some(), cache_looks_empty, cached_chunk0_size);
                if !is_trunc && cache_looks_empty {
                    let path_opt = self.inode_to_path.read().unwrap().get(&ino).cloned();
                    if let Some(path) = path_opt {
                        let client = self.client.clone();
                        match self.runtime.block_on(client.get_file_metadata(&path)) {
                            Ok(Some(fresh)) => {
                                let fresh_chunk0_size = fresh.chunk_locations.iter().find(|l| l.file_offset.unwrap_or(0) == 0).map(|l| l.size);
                                debug!("[SIZE TRACE] open-write-refresh ino={} path={} fresh_chunks={} fresh_chunk0_size={:?}",
                                    ino, path, fresh.chunk_locations.len(), fresh_chunk0_size);
                                self.client.seed_write_seq(fresh.id, fresh.write_seq);
                                self.metadata_cache.insert(ino, fresh);
                            }
                            Ok(None) => {
                                debug!("[SIZE TRACE] open-write-refresh ino={} path={} server_says_not_found", ino, path);
                            }
                            Err(e) => {
                                debug!("[SIZE TRACE] open-write-refresh ino={} path={} lookup_error={}", ino, path, e);
                            }
                        }
                    }
                }
            } else {
                // Subsequent writers on an already-open inode: the first writer's open
                // (above) already made metadata fresh for this session, so refresh in the
                // background rather than blocking this open() — the staleness window left
                // over is negligible.
                let path_opt = self.inode_to_path.read().unwrap().get(&ino).cloned();
                if let Some(path) = path_opt {
                    let client = self.client.clone();
                    let metadata_cache = self.metadata_cache.clone();
                    let write_open_counts = self.write_open_counts.clone();
                    self.runtime.spawn(async move {
                        if let Ok(Some(fresh)) = client.get_file_metadata(&path).await {
                            client.seed_write_seq(fresh.id, fresh.write_seq);
                            // Don't replace metadata_cache while any writer has the file open.
                            // In-memory chunk_ids are authoritative between flush_buffer_async_one
                            // and flush_metadata_sync; overwriting here clobbers them with stale
                            // server data, causing flush_metadata_sync to commit wrong ids.
                            let still_open = write_open_counts.get(&ino).map(|c| *c > 0).unwrap_or(false);
                            if !still_open {
                                metadata_cache.insert(ino, fresh);
                            }
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
            // Mark non-buffered SQLite inodes so write() fast-path can route correctly
            // without needing a path lookup (which may not be available yet on first write).
            let needs_direct = path_for_sync_check.as_deref()
                .map(|p| is_sqlite_path(p) && !is_sqlite_buffered(p))
                .unwrap_or(false);
            if needs_direct {
                self.direct_write_inodes.insert(ino);
                debug!("open: ino={} inserted into direct_write_inodes", ino);
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
                    // Only remove the write buffer if it has no active slot data.
                    // A slot is active if it has ANY data — whether flushing=false (pending
                    // send) OR flushing=true (network I/O in-flight). The flush task releases
                    // the write-buffer mutex BEFORE the network call, so try_lock() succeeds
                    // while the flush is running. Checking !flushing was a bug: it treated
                    // in-flight data as "safe to remove", silently discarding writes that
                    // arrived between the flush task's mutex release and its metadata commit.
                    let safe_to_remove = if let Some(state_arc) = self.write_buffers.get(&ino) {
                        if let Ok(st) = state_arc.try_lock() {
                            let has_unflushed = st.slots.values().any(|s| !s.is_empty());
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
                    // safe_to_remove only tells us it's safe — it doesn't mean removal is
                    // useful. The original purpose here was discarding a stale buffer left
                    // over from a session where release() never ran (client crash/restart),
                    // per this block's opening comment. But when safe_to_remove is true there
                    // is, by definition, no unflushed data sitting in the buffer for THIS
                    // open to protect against — so removing it has no protective effect. Its
                    // only real effect is discarding flushed_sizes / canonical_write_nodes /
                    // canonical_node_miss_streak, which is exactly the same load-bearing
                    // bookkeeping release() now preserves for the identical reason (see its
                    // "no unflushed data" cleanup comment). QEMU's close+reopen cycling means
                    // this open() runs moments after a clean release() far more often than
                    // after a crash, and blindly wiping here defeated that fix — confirmed
                    // live via [SLOT-TRACE]: flushed_sizes still read back as 0 on the next
                    // write_at() even after release() stopped removing the entry, because
                    // THIS site was doing it instead. Do NOT evict recent_chunk_writes here
                    // either. For VM disks, QEMU opens and closes the file every few seconds
                    // (BLKFLSBUF cycles). Evicting on every close wipes the chunk-ID cache,
                    // forcing every subsequent patch to use the stale metadata_cache (leader
                    // hasn't received async updates yet) and incur a stale-base retry. The
                    // 10-second TTL in recent_chunk_writes handles natural expiry; file_id
                    // filtering prevents cross-file pollution.
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
        let release_in_flight = self.release_in_flight.clone();
        let refreshing_inodes = self.refreshing_inodes.clone();
        let flush_in_flight = self.flush_in_flight.clone();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            let metadata = metadata_cache.get(&ino).map(|m| m.clone());

            if let Some(mut metadata) = metadata {
                if metadata.file_type == FileType::RegularFile {
                    // Wait for in-flight flushes if the file was very recently created and
                    // has active writers OR a release flush in progress. This prevents returning
                    // size=0 when qcow2-img (or similar tools) creates a disk and Proxmox/QEMU
                    // immediately hotplugs it to a running VM before the initial writes are
                    // flushed. Without this, the VM's getattr sees size=0, caches it, and the
                    // disk appears as 0 bytes even after creation completes.
                    //
                    // Checking release_in_flight handles the case where qemu-img has already
                    // closed the file (has_active_writer=false) but the release task is still
                    // flushing data in the background.
                    let has_active_writer = write_open_counts.get(&ino).map(|c| *c > 0).unwrap_or(false);
                    let has_release_in_flight = release_in_flight.get(&ino)
                        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed) > 0)
                        .unwrap_or(false);
                    let file_age = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                        .saturating_sub(metadata.created_at);
                    let is_very_recent = file_age < 60; // created in the last minute

                    if (has_active_writer || has_release_in_flight) && is_very_recent {
                        // File is actively being written to and very new.
                        // This is likely a create-in-progress. Wait up to 2 seconds for any
                        // in-flight flush to complete so we return a meaningful size.
                        let has_flush = {
                            let guard = flush_in_flight.read().unwrap();
                            guard.as_ref().map(|m| m.contains_key(&ino)).unwrap_or(false)
                        }; // Drop guard before await
                        if has_flush {
                            let start = std::time::Instant::now();
                            loop {
                                let still_flushing = {
                                    let guard = flush_in_flight.read().unwrap();
                                    guard.as_ref().map(|m| m.contains_key(&ino)).unwrap_or(false)
                                };
                                if !still_flushing || start.elapsed() >= std::time::Duration::from_secs(2) {
                                    break;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            }
                            // Re-fetch metadata after waiting — the flush should have updated it
                            if let Some(fresh) = metadata_cache.get(&ino).map(|m| m.clone()) {
                                metadata = fresh;
                            }
                        }
                    }
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
                        let current_write_seq = metadata.write_seq;
                        let current_size = metadata.size;
                        let current_chunks = metadata.chunk_locations.len();
                        tokio::spawn(async move {
                            let result = tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                client_bg.get_file_metadata(&path_bg),
                            ).await;
                            if let Ok(Ok(Some(fresh))) = result {
                                let server_is_newer = fresh.write_seq > current_write_seq
                                    || (fresh.write_seq == current_write_seq
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
                                    .map(|(idx, slot)| idx * CHUNK_SIZE as u64 + slot.span_end as u64)
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
                                    .map(|(idx, slot)| idx * CHUNK_SIZE as u64 + slot.span_end as u64)
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
            let (file_size, file_type, file_path, file_id, write_seq) = match metadata_cache.get(&ino) {
                Some(m) => (m.size, m.file_type, m.path.clone(), m.id, Some(m.write_seq)),
                None => { reply.error(libc::ENOENT); return; }
            };

            if file_type != FileType::RegularFile {
                reply.error(libc::EISDIR);
                return;
            }

            let offset = offset as usize;
            let size = size as usize;

            // Timeout for waiting on the local write-buffer lock below, instead of
            // falling through to a network read that risks serving stale data for
            // a slot that's mid-flush. Unified across all file types (2026-07-11,
            // superseding an earlier is_vm_disk-only carve-out) — two independent,
            // confirmed-real, *legitimate* (not-a-failure) sources of multi-second
            // delay exist in this system: redb compaction's exclusive Phase 3 lock
            // (observed consistently 2.8-4.1s across many real occurrences) and
            // resolve_chunk_content forcing a fold to complete before a read of a
            // not-yet-folded chunk returns (observed 8s+). Trying to pick a
            // shorter "probably safe" number for non-VM-disk files would need
            // confident margin over both, and their worst-case combination isn't
            // well bounded — not worth the risk given what a stale read here
            // actually costs (root-caused a live Proxmox qcow2 restore corruption:
            // "Preventing invalid write on metadata (overlaps with active L2
            // table)" — a stale read desynced the hypervisor's own allocation
            // tracking from what was actually persisted; the write it then
            // refused was never the bug, an earlier stale read was).
            const READ_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

            // Extend file_size to include buffered-but-uncommitted bytes so the EOF
            // check below doesn't gate out reads that are within the write buffer.
            // Use blocking lock (not try_lock) to wait for flush completion, ensuring
            // we see the final buffer state before falling through to network.
            let file_size = if write_buffer_enabled {
                if let Some(state_arc) = write_buffers.get(&ino).map(|r| r.clone()) {
                    // Bounded wait — see READ_LOCK_TIMEOUT's doc comment above for
                    // why this is 15s and not the original 500ms.
                    let buffered_end = match tokio::time::timeout(
                        READ_LOCK_TIMEOUT,
                        state_arc.lock(),
                    ).await {
                        Ok(state) => state.slots.iter()
                            .map(|(idx, slot)| idx * CHUNK_SIZE as u64 + slot.span_end as u64)
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
            // Bounded wait, not an immediate fallback — see READ_LOCK_TIMEOUT's doc
            // comment above. Only past 15s (genuinely stuck, not just a normal
            // compaction/fold delay) do we accept the stale-read risk of falling
            // through to the network rather than keep blocking this read.
            if write_buffer_enabled {
                if let Some(state_arc) = write_buffers.get(&ino).map(|r| r.clone()) {
                    let lock_result = tokio::time::timeout(
                        READ_LOCK_TIMEOUT,
                        state_arc.lock(),
                    ).await;
                    if lock_result.is_err() {
                        // Flush is stuck (likely slow node) — fall through to network
                        let result = client.read_file(
                            ino, file_size, file_id, &file_path, offset, size, false, write_seq,
                        ).await;
                        let elapsed = start.elapsed();
                        match result {
                            Ok(data) => {
                                let reply_data = if data.len() > size { &data[..size] } else { &data[..] };
                                debug!("FUSE read done: ino={}, {} bytes in {:?}", ino, reply_data.len(), elapsed);
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
                        // CRITICAL: serve buffered data for read-after-write consistency,
                        // but ONLY for bytes the app actually wrote this session
                        // (dirty_ranges). Bytes elsewhere in slot.data are synthetic
                        // gap-fill zeros (gap_filled_prefix prefix, or mid-slot gaps from
                        // non-sequential writes) — placeholders for data the server
                        // already has from a prior flush, not real content. Serving those
                        // directly would shadow real committed server data with zeros.
                        // Fall through to network for those, same as "beyond the buffered
                        // frontier" below.
                        if let Some(&(_, range_end)) = slot.dirty_ranges.iter()
                            .find(|&&(s, e)| s <= intra && intra < e)
                        {
                            // Buffer has real data at this offset — serve it, capped at
                            // the end of this dirty range so we never cross into a gap.
                            // read_at serves straight from the sparse extent (extent
                            // boundaries mirror dirty_ranges', so coverage is
                            // guaranteed here); the extra dirty-range cap is kept as
                            // belt-and-braces.
                            let avail = range_end - intra;
                            let n = avail.min(size);
                            if let Some(bytes) = slot.read_at(intra, n) {
                                reply.data(bytes);
                                return;
                            }
                            // Defensive: extent lookup missed despite a covering dirty
                            // range (should be impossible) — fall through to network
                            // rather than serving wrong bytes.
                            warn!("read: ino={} chunk={} intra={} inside dirty range but no extent covers it — falling through to network", ino, chunk_idx, intra);
                        }

                        // The read doesn't START inside a dirty range, but a dirty range
                        // may still start somewhere INSIDE the read window (e.g. an 8-byte
                        // field a few bytes past the read's start — VM108's mkfs.ext4
                        // hot-spot pattern, T35). Falling straight through to the network
                        // for the whole window would miss those just-written, not-yet-
                        // flushed bytes and return stale data. Splice: fetch the window
                        // from network/cache, then overlay each overlapping dirty
                        // sub-range with the slot's real bytes.
                        let read_end = intra + size;
                        let overlaps: Vec<(usize, usize)> = slot.dirty_ranges.iter()
                            .filter(|&&(s, e)| s < read_end && e > intra)
                            .copied()
                            .collect();
                        if !overlaps.is_empty() {
                            let slot_data = slot.materialize();
                            drop(state);
                            let net_result = client.read_file(
                                ino, file_size, file_id, &file_path, offset, size, false, write_seq,
                            ).await;
                            let mut buf = match net_result {
                                Ok(mut data) => { data.resize(size, 0); data }
                                Err(e) => {
                                    error!("read failed (slot-overlap splice): {}", e);
                                    reply.error(libc::EIO);
                                    return;
                                }
                            };
                            for (s, e) in overlaps {
                                let ov_start = s.max(intra);
                                let ov_end = e.min(read_end).min(slot_data.len());
                                if ov_end <= ov_start { continue; }
                                let buf_start = ov_start - intra;
                                let buf_end = ov_end - intra;
                                buf[buf_start..buf_end].copy_from_slice(&slot_data[ov_start..ov_end]);
                            }
                            reply.data(&buf);
                            return;
                        }

                        // Beyond this slot's buffered frontier, or within a gap-fill
                        // placeholder region. The server may have committed data here
                        // from a prior flush (e.g. mkfs.ext4 writes non-sequentially
                        // within a chunk). Fall through to the network
                        // unless we're past the committed metadata size (true live edge).
                        let committed_size = metadata_cache.get(&ino).map(|m| m.size as usize).unwrap_or(0);
                        if offset >= committed_size {
                            reply.data(&[]);
                            return;
                        }
                        // If this slot is currently being flushed (mid-PatchChunk), the
                        // old chunk ID is being deleted on the server right now. Falling
                        // through to the network would fetch a chunk that no longer exists.
                        // Drop the lock and wait briefly for the flush to complete, then
                        // the slot will be gone and we can safely fetch the new chunk ID.
                        if slot.flushing {
                            drop(state);
                            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
                            loop {
                                tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
                                let still_flushing = write_buffers.get(&ino).map(|a| {
                                    a.try_lock().map(|s| s.slots.get(&chunk_idx).map(|sl| sl.flushing).unwrap_or(false)).unwrap_or(true)
                                }).unwrap_or(false);
                                if !still_flushing || tokio::time::Instant::now() >= deadline {
                                    break;
                                }
                            }
                            // Fall through to network — flush is done, chunk ID is current.
                        }
                        // Fall through to network — server has data here.
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
                        // Do NOT replace metadata_cache with server data while the file is
                        // open for writing. Between flush_buffer_async_one updating chunk_ids
                        // in-memory and flush_metadata_sync committing them to the leader, the
                        // server's view is stale. Overwriting here clobbers the correct in-memory
                        // chunk_ids, which flush_metadata_sync then commits to the leader — leaving
                        // live chunks unreferenced and old (deleted) chunk_ids in the leader's DB.
                        let is_write_open = write_buffers.contains_key(&ino);
                        if offset >= fresh.size as usize {
                            client.seed_write_seq(fresh.id, fresh.write_seq);
                            if !is_write_open {
                                // Invalidate the read engine so next read picks up new chunk map.
                                client.invalidate_read_engine(ino);
                                metadata_cache.insert(ino, fresh);
                            }
                            reply.data(&[]);
                            return;
                        }
                        let new_size = fresh.size;
                        client.seed_write_seq(fresh.id, fresh.write_seq);
                        if is_write_open {
                            // File open for writing: in-memory chunk_ids are authoritative.
                            // Only advance the size; never overwrite chunk locations.
                            if let Some(mut m) = metadata_cache.get_mut(&ino) {
                                m.size = m.size.max(new_size);
                            }
                        } else {
                            // Invalidate the read engine so next read picks up new chunk map.
                            client.invalidate_read_engine(ino);
                            metadata_cache.insert(ino, fresh);
                        }
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
            debug!("FUSE read: ino={}, offset={}, size={}, file_size={}", ino, offset, size, effective_size);
            let result = client.read_file(
                ino, effective_size, file_id, &file_path, offset, size, has_active_writer, write_seq,
            ).await;

            let elapsed = start.elapsed();
            match result {
                Ok(data) => {
                    // FUSE rejects replies larger than the requested size with EINVAL.
                    let reply_data = if data.len() > size { &data[..size] } else { &data[..] };
                    debug!("FUSE read done: ino={}, {} bytes in {:?}", ino, reply_data.len(), elapsed);
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
        let dir_cache_invalidated_at = self.dir_cache_invalidated_at.clone();
        let metadata_cache = self.metadata_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let inode_to_path = self.inode_to_path.clone();
        let next_inode = self.next_inode.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let write_buffers = self.write_buffers.clone();
        let write_counters = self.write_counters.clone();

        // Snapshot the invalidation time BEFORE the fetch begins. After the fetch
        // returns, we only cache the result if this hasn't advanced — i.e., no
        // concurrent create/mkdir/unlink invalidated the directory during the fetch.
        let invalidation_at_fetch_start = dir_cache_invalidated_at
            .get(&path).map(|t| *t);

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
                        // Only cache if the directory wasn't invalidated during the fetch.
                        // A concurrent create/mkdir/unlink calls invalidate_dir_cache()
                        // which advances dir_cache_invalidated_at. If it advanced since
                        // we captured invalidation_at_fetch_start, our response is stale
                        // (it was fetched before the new file was on the server) — don't
                        // cache it, so the next readdir re-fetches and sees the new entry.
                        let invalidation_now = dir_cache_invalidated_at.get(&path).map(|t| *t);
                        if invalidation_now == invalidation_at_fetch_start {
                            dir_cache.insert(path.clone(), (entries.clone(), std::time::Instant::now()));
                        } else {
                            debug!("readdir: skipping dir_cache insert for {} — directory was invalidated during fetch", path);
                        }
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
                let inode_to_path = inode_to_path.clone();
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
                                        inode_to_path.write().unwrap().insert(v, entry.path.clone());
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

        // O_CREAT without O_EXCL: if the file already exists (concurrent create from another
        // thread, or a race between lookup and create), behave like open() rather than
        // creating a second inode. Without this check, two threads racing on the same path
        // both succeed at creating the file and write independent schemas — corrupting the db.
        //
        // Must NOT take this shortcut if the path is pending-delete: unlink() on a still-open
        // file defers clearing path_to_inode/metadata_cache/write_buffers until the old fd's
        // release() (see unlink()'s "still_open" branch) so in-flight writes on that fd can
        // still resolve — but that means this path's cached ino/metadata can be for a file
        // that's logically already gone. Reopening it here as "the same file" would hand a
        // brand new create() the old file's stale write_buffers/flushed_sizes, which
        // flush_buffer_async_one can misread as real existing chunk content belonging to the
        // new file (root cause of the chunk-0 header-loss corruption traced 2026-07-03).
        let o_excl = (_flags & libc::O_EXCL) != 0;
        let path_pending_delete = self.pending_deletes.contains(&path);
        if !o_excl && !path_pending_delete {
            let existing_ino = self.path_to_inode.read().unwrap().get(&path).copied();
            if let Some(ino) = existing_ino {
                if let Some(meta) = self.metadata_cache.get(&ino) {
                    let is_write = (_flags & libc::O_ACCMODE) != libc::O_RDONLY;
                    if is_write {
                        *self.write_open_counts.entry(ino).or_insert(0) += 1;
                        self.client.write_open_inodes.insert(ino);
                    }
                    *self.open_counts.entry(ino).or_insert(0) += 1;
                    let attr = DfsFilesystem::metadata_to_attr_static(ino, &meta);
                    let is_sqlite = is_sqlite_direct_io(&meta.path);
                    let open_flags = if is_sqlite { fuser::consts::FOPEN_DIRECT_IO } else { 0 };
                    drop(meta);
                    info!("create: path={} already exists (ino={}) — opening existing file", path, ino);
                    reply.created(&Duration::ZERO, &attr, 0, 0, open_flags);
                    return;
                }
            }
        }

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
            chunk_locations: Arc::new(Vec::new()),
            write_seq: 0,
            symlink_target: None,
        };

        // Store metadata on cluster — spawn so we never block_on the FUSE dispatch thread.
        let client = self.client.clone();
        let metadata_clone = metadata.clone();
        let metadata_cache = self.metadata_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let inode_to_path = self.inode_to_path.clone();
        let next_inode = self.next_inode.clone();
        let write_open_counts = self.write_open_counts.clone();
        let open_counts = self.open_counts.clone();
        let write_buffers = self.write_buffers.clone();
        let write_buffer_enabled = self.write_buffer_enabled;
        let direct_write_inodes = self.direct_write_inodes.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let pending_creates = self.pending_creates.clone();
        let dir_cache_invalidated_at = self.dir_cache_invalidated_at.clone();
        let written_inodes_for_create = self.written_inodes.clone();
        let is_write_open = (_flags & libc::O_ACCMODE) != libc::O_RDONLY;
        let pending_deletes = self.pending_deletes.clone();

        // Block lookups from overwriting metadata_cache for this path until the create
        // async task inserts the fresh metadata. Set on the FUSE dispatch thread (before
        // spawn) so it's visible to any lookup task that fires while we're in flight.
        pending_creates.insert(path.clone());

        self.runtime.spawn(async move {
            // O_CREAT without O_EXCL, local cache miss: the file may still genuinely exist
            // on the server — e.g. this client just restarted and hasn't warmed/looked up
            // this specific path yet, or the leader changed and this session never learned
            // about it. The check above only ever consulted our own in-memory path_to_inode,
            // never the server, so a cold cache here used to silently mint a brand new FileId
            // over a path that already had real content, orphaning it (staging incident,
            // 2026-07-03: hdhomerun dvr's dvr.conf got replaced with a near-empty file after
            // a full cluster + client redeploy — see create()'s local-cache-only comment
            // above for the full trace). Ask the server before assuming "new".
            //
            // Skip this reuse entirely if the path is pending-delete: the server hasn't
            // processed the delete yet either (unlink() defers it until the old fd's
            // release(), see the "still_open" branch), so it would answer with the SAME
            // stale, logically-superseded file we must not reuse — see the sync fast-path
            // guard above for the full explanation.
            if !o_excl && !pending_deletes.contains(&path) {
                if let Ok(Some(existing_metadata)) = client.get_file_metadata_conditional(&path, None).await {
                    let ino = {
                        let path_map = path_to_inode.read().unwrap();
                        if let Some(&existing) = path_map.get(&path) {
                            existing
                        } else {
                            drop(path_map);
                            let mut next = next_inode.write().unwrap();
                            let v = *next; *next += 1; drop(next);
                            path_to_inode.write().unwrap().insert(path.clone(), v);
                            inode_to_path.write().unwrap().insert(v, path.clone());
                            v
                        }
                    };
                    client.seed_write_seq(existing_metadata.id, existing_metadata.write_seq);
                    if is_write_open {
                        *write_open_counts.entry(ino).or_insert(0) += 1;
                        client.write_open_inodes.insert(ino);
                    }
                    *open_counts.entry(ino).or_insert(0) += 1;
                    metadata_cache.insert(ino, existing_metadata.clone());
                    last_metadata_update.insert(ino, std::time::Instant::now());
                    pending_creates.remove(&path);
                    let attr = DfsFilesystem::metadata_to_attr_static(ino, &existing_metadata);
                    let is_sqlite = is_sqlite_direct_io(&existing_metadata.path);
                    let open_flags = if is_sqlite { fuser::consts::FOPEN_DIRECT_IO } else { 0 };
                    info!("create: path={} found on server (ino={}, id={}) despite cold local cache — opening existing file instead of minting a new identity",
                        path, ino, existing_metadata.id);
                    reply.created(&Duration::ZERO, &attr, 0, 0, open_flags);
                    return;
                }
            }

            match client.put_file_metadata(&metadata_clone).await {
                Ok(_) => {
                    // Allocate inode. Don't reuse path_to_inode's existing entry if the path
                    // is pending-delete — that entry belongs to a file this same path is in
                    // the process of superseding (deferred cleanup, see the sync fast-path
                    // guard above), and handing its ino to this genuinely-new file would let
                    // it inherit that file's stale write_buffers/flushed_sizes. Always mint a
                    // fresh ino in that case; this naturally "steals" the path mapping away
                    // from the old, soon-to-be-cleaned-up inode, which is fine since it's
                    // already logically gone.
                    let ino = {
                        let path_map = path_to_inode.read().unwrap();
                        let reusable = path_map.get(&path).copied();
                        drop(path_map);
                        match reusable {
                            Some(existing) if !pending_deletes.contains(&path) => existing,
                            _ => {
                                let mut next = next_inode.write().unwrap();
                                let v = *next; *next += 1; drop(next);
                                path_to_inode.write().unwrap().insert(path.clone(), v);
                                inode_to_path.write().unwrap().insert(v, path.clone());
                                v
                            }
                        }
                    };

                    // Mark as open-for-write BEFORE inserting metadata so the lookup
                    // guard (which checks write_open_counts) is already set when we
                    // insert fresh metadata — preventing any concurrent lookup from
                    // overwriting it with stale server data.
                    *write_open_counts.entry(ino).or_insert(0) += 1;

                    // Cache metadata and stamp it as fresh.
                    // BUT: Only skip inserting if the cache already has data from THIS same
                    // file's own in-flight writes. Sparse writes (qcow2 creation) may have
                    // already updated the cache with the correct size while this async
                    // CREATE task was in flight. Without this check, we'd overwrite the
                    // sparse write's metadata (size=1GB) with stale creation metadata (size=0).
                    // We can't rely on modified_at because it only has 1-second granularity
                    // and the sparse write happens within the same second as create.
                    //
                    // Also always insert if the cached entry belongs to a DIFFERENT file id —
                    // a stale leftover at this ino from a logically-superseded predecessor
                    // must never be mistaken for "this session's own write already landed".
                    let should_insert = metadata_cache.get(&ino)
                        .map(|cached| cached.id != metadata.id || (cached.size == 0 && cached.chunk_locations.is_empty()))
                        .unwrap_or(true);
                    if should_insert {
                        metadata_cache.insert(ino, metadata.clone());
                        last_metadata_update.insert(ino, std::time::Instant::now());
                    }

                    // Clear any stale read engine for this inode. Inode numbers are reused
                    // within a mount session, so a previously-deleted file's chunk map may
                    // still be present. Without this, the first read of a newly-created file
                    // returns data from the old file — causing SQLITE_NOTADB.
                    if let Some(engine) = client.read_engines.get(ino) {
                        let engine_clone = engine.clone();
                        tokio::spawn(async move {
                            engine_clone.expire_chunk_map_async().await;
                        });
                    }

                    // Invalidate parent directory cache so 'ls' shows new file immediately.
                    // Also stamps dir_cache_invalidated_at so any in-flight readdir that
                    // fetched the parent listing BEFORE this create() completed will not
                    // re-populate dir_cache with a stale (empty) result.
                    let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                    let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                    DfsFilesystem::invalidate_dir_cache(&dir_cache, &dir_cache_invalidated_at, parent_path);

                    // Fresh metadata is now in cache — lift the lookup guard.
                    pending_creates.remove(&path);

                    // Mark inodes that must bypass the write buffer (set before reply.created()
                    // so the routing decision is available on the first write()).
                    if is_sqlite_path(&path) && !is_sqlite_buffered(&path) {
                        direct_write_inodes.insert(ino);
                        debug!("create: ino={} inserted into direct_write_inodes for path={}", ino, path);
                    }

                    // Pre-create the write buffer with sync_on_fsync=true for SQLite files
                    // so that the first fdatasync after the first write flushes synchronously.
                    // Without this, the InodeWriteState is created lazily on the first write()
                    // call, which races with path_to_inode insertion — the path may not be
                    // visible yet, is_sqlite_buffered returns false, and sync_on_fsync stays
                    // false, causing fdatasync to run in the background and reads after fsync
                    // to see stale data (SQLite corruption).
                    let is_sqlite_buf = is_sqlite_buffered(&path);
                    if write_buffer_enabled && is_sqlite_buf {
                        // Always replace — create() means a new file, so any existing buffer
                        // for this inode (from a deleted predecessor) has stale flushed_sizes
                        // that would cause flush_buffer_async_one to PatchChunk the wrong chunk.
                        let mut state = InodeWriteState::new(true);
                        state.expected_file_id = Some(metadata.id);
                        write_buffers.insert(ino, Arc::new(Mutex::new(state)));
                        written_inodes_for_create.insert(ino);
                        info!("create: ino={} pre-created SQLite write buffer (sync_on_fsync=true, file_id={})", ino, metadata.id);
                    }

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
                    pending_creates.remove(&path);
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

        // A new write invalidates any explicit mtime set by a prior utimes() call.
        // Flush tasks now use contains() (not remove()) so ALL concurrent flush tasks
        // for the same inode preserve the explicit mtime. Clearing here (on the FUSE
        // dispatch thread, before any async task is spawned) is the only place it's
        // removed — ensuring that after a write(), the next flush stamps mtime=now().
        self.explicit_mtime_pending.remove(&ino);

        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let write_counters = self.write_counters.clone();
        let write_buffers = self.write_buffers.clone();
        let write_buffer_enabled = self.write_buffer_enabled;
        let buffer_flush_threshold = self.buffer_flush_threshold;
        let last_metadata_update = self.last_metadata_update.clone();
        let path_to_inode = self.path_to_inode.clone();
        let inode_to_path = self.inode_to_path.clone();
        let flush_handle = self.flush_handle.clone();
        let size_high_water = self.size_high_water.clone();
        let global_buffered_bytes = self.global_buffered_bytes.clone();
        let global_write_buffer_cap_bytes = self.global_write_buffer_cap_bytes;
        let data_vec = data.to_vec();
        let req_uid = _req.uid();
        let req_gid = _req.gid();
        let direct_write_inodes = self.direct_write_inodes.clone();
        let chunk_write_locks = self.chunk_write_locks.clone();
        let written_inodes = self.written_inodes.clone();

        let write_tasks_in_flight = self.write_tasks_in_flight.clone();
        // Chunk(s) this write touches — almost always one for a sub-4MB write,
        // two if it straddles a chunk boundary. Computed once up front so the
        // fast and slow paths key their in-flight counters identically to how
        // flush_one_chunk will look them up (per (ino, chunk_idx), not per ino —
        // see chunk_indices_for_write's doc comment for why).
        let write_chunk_indices = chunk_indices_for_write(offset as u64, data.len());
        // NOTE: counters are incremented AFTER the back-pressure wait, just before
        // write_at(). Incrementing here (before back-pressure) would cause a
        // deadlock: flush_one_chunk waits for its chunk's counter to hit 0 before
        // snapshotting a gap-prefix slot, but a write stalled on back-pressure can't
        // call write_at() until the flush frees buffer space — neither can proceed.
        // After a 5-second timeout flush_one_chunk would snapshot incomplete data and
        // flush zeros to the server (silent data corruption).

        // Fast path: if metadata is cached, buffer is not full, and this is a
        // sequential (non-sparse) buffered write, handle it synchronously on the
        // FUSE dispatch thread via block_in_place. This avoids spawning a runtime
        // task entirely — critical because all 8 runtime threads can be occupied
        // by concurrent reads, causing write tasks to queue up and QEMU to deadlock
        // waiting for a reply that never comes.
        // Check direct_write_inodes first — set at create()/open() time before reply returns,
        // so this is race-free even when metadata_cache/path_to_inode aren't populated yet.
        let force_direct = direct_write_inodes.contains(&ino);

        // Fall back to path-based check for files not in direct_write_inodes.
        let path_for_sqlite_check = if force_direct { None } else {
            metadata_cache.get(&ino).map(|m| m.path.clone())
                .or_else(|| inode_to_path.read().unwrap().get(&ino).cloned())
        };
        let use_buffer = !force_direct && path_for_sqlite_check.as_deref()
            .map(|p| !is_sqlite_path(p) || is_sqlite_buffered(p))
            .unwrap_or(true);
        debug!("write routing: ino={} path={:?} force_direct={} use_buffer={}", ino, path_for_sqlite_check, force_direct, use_buffer);
        if write_buffer_enabled && use_buffer {
            if let Some(meta) = metadata_cache.get(&ino) {
                if meta.file_type == FileType::RegularFile {
                    let offset_usize = offset as usize;
                    let cache_size = meta.size as usize;
                    let current_file_id = meta.id;
                    drop(meta);
                    let hwm = size_high_water.get(&ino).map(|v| *v as usize).unwrap_or(0);
                    let current_size = hwm.max(cache_size);
                    let is_sequential = offset_usize <= current_size;
                    if is_sequential {
                        // Create a fresh buffer if none exists yet, or if the existing one was
                        // stamped for a different file identity — the same "Always replace"
                        // handling create()'s SQLite pre-create path already does, extended to
                        // this general lazy-creation path used by every other regular file
                        // (DVR recordings included). Without this, a write_buffers entry left
                        // over from a deleted-but-still-open predecessor at this inode carries
                        // stale flushed_sizes that flush_buffer_async_one can misread as real
                        // existing chunk content belonging to the new file — see
                        // expected_file_id's doc comment for the full mechanism.
                        let needs_fresh_buffer = match write_buffers.get(&ino) {
                            None => true,
                            Some(existing) => existing.try_lock().ok()
                                .map(|st| st.expected_file_id.is_some_and(|id| id != current_file_id))
                                .unwrap_or(false),
                        };
                        if needs_fresh_buffer {
                            let sync = path_for_sqlite_check.as_deref().map(is_sqlite_buffered).unwrap_or(false);
                            let mut state = InodeWriteState::new(sync);
                            state.expected_file_id = Some(current_file_id);
                            write_buffers.insert(ino, Arc::new(Mutex::new(state)));
                            written_inodes.insert(ino);
                        }
                        let state_arc = write_buffers.get(&ino).map(|e| e.clone());
                        if let Some(state_arc) = state_arc {
                        // Spawn immediately instead of running inline: fuser's session
                        // loop reads exactly one kernel request at a time and dispatches
                        // synchronously (see fuser session.rs — "this read-dispatch-loop
                        // is non-concurrent"). Anything done inline here blocks that one
                        // thread from picking up the NEXT request, regardless of how many
                        // the kernel has queued — which silently collapsed concurrent
                        // writes to effectively QD1 (confirmed: KDiskMark RND4K write
                        // throughput was flat from Q1 to Q32). Matches the shape read()
                        // already uses (read_runtime.spawn, fuse_impl.rs ~4171).
                        //
                        // Back-pressure semantics are unchanged — the same graduated
                        // sleep loop runs here, just inside the spawned task instead of
                        // on the dispatch thread. This still bounds memory: FUSE's own
                        // max_background cap limits how many writes the kernel will
                        // dispatch to us without a reply, so the number of these tasks
                        // that can exist at once is already bounded independent of this
                        // change — moving the sleep off the dispatch thread doesn't admit
                        // more outstanding writes than the kernel was already allowed to
                        // queue.
                        //
                        // CRITICAL: apply back-pressure BEFORE acquiring the lock.
                        // Holding the slot mutex while spinning would prevent the flush task
                        // from acquiring it to drain the buffer — permanent deadlock.
                        //
                        // Graduated back-pressure reading global_buffered_bytes directly.
                        // This avoids try_lock on the slot (which fails under high concurrency
                        // and falls back to cap, causing false pressure) without introducing
                        // CAS loops that cause spurious sleeps.
                        // The cap is soft: concurrent writers can overshoot by at most
                        // N×write_size before the next check catches it — acceptable given
                        // write_size ≤ 1MB and the flusher drains continuously.
                        self.runtime.spawn(async move {
                            {
                                let t_bp = std::time::Instant::now();
                                // Data integrity outranks availability: a full buffer means the
                                // flush pipeline is behind, not that the write is invalid. Block
                                // until it drains, however long that takes, instead of returning
                                // EIO and discarding bytes the kernel believes were written
                                // successfully. Log periodically so a genuine stall is visible
                                // instead of a silent multi-minute hang.
                                const STALL_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
                                let mut next_stall_log = STALL_LOG_INTERVAL;
                                let effective_cap = global_write_buffer_cap_bytes;
                                loop {
                                    let current = global_buffered_bytes.load(std::sync::atomic::Ordering::Relaxed);
                                    let fill_pct = current * 100 / effective_cap.max(1);
                                    let delay_ms: u64 = if fill_pct < 75 { 0 }
                                        else if fill_pct < 90 { 1 }
                                        else if fill_pct < 100 { 5 }
                                        else {
                                            let waited = t_bp.elapsed();
                                            if waited >= next_stall_log {
                                                warn!("write fast-path: ino={} blocked on backpressure for {:?} (global_buffered={}  cap={} active_inodes={}) — waiting for drain, not failing",
                                                      ino, waited, current, effective_cap, write_buffers.len());
                                                next_stall_log += STALL_LOG_INTERVAL;
                                            }
                                            // Buffer's full. If this write's target chunk(s) also
                                            // just failed to offload to every replica (in_backoff,
                                            // set by flush_buffer_async_one's Err arm), waiting
                                            // longer won't help — that's not a slow pipeline, it's
                                            // no pipeline. Report ENOSPC rather than blocking on a
                                            // drain that can't happen.
                                            let stuck = state_arc.try_lock()
                                                .map(|st| write_chunk_indices.iter()
                                                    .any(|idx| st.slots.get(idx).is_some_and(|s| s.in_backoff())))
                                                .unwrap_or(false);
                                            if stuck {
                                                error!("write fast-path: ino={} buffer full and chunk(s) {:?} cannot be offloaded to any replica — ENOSPC",
                                                       ino, write_chunk_indices);
                                                reply.error(libc::ENOSPC);
                                                return;
                                            }
                                            10
                                        };
                                    if delay_ms == 0 { break; }
                                    maybe_log_bpfill("fast", ino, fill_pct, delay_ms, current, effective_cap);
                                    // Under back-pressure, urgently wake the flush worker so it
                                    // drains stale partial slots (e.g. VM disk patches that never
                                    // fill a full 4MB chunk) instead of waiting for the 50ms tick.
                                    flush_handle.flush_notify.notify_one();
                                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                                    if fill_pct < 100 { break; }
                                }
                            }
                            // Increment AFTER back-pressure wait: counters now only reflect
                            // write_at() calls in progress, not writes stalled on buffer-full.
                            let _write_guard = WriteTaskGuard(
                                inc_write_tasks_for_chunks(&write_tasks_in_flight, ino, &write_chunk_indices)
                            );
                            let mut state = state_arc.lock().await;
                            {
                                let bytes_before = state.resident_bytes();
                                let pattern_changed = state.write_at(offset as u64, &data_vec);
                                let bytes_after = state.resident_bytes();
                                let has_full = !state.full_slot_indices().is_empty();
                                drop(state);
                                let new_end = (offset as u64) + data_vec.len() as u64;
                                {
                                    let mut hwm = size_high_water.entry(ino).or_insert(0);
                                    if new_end > *hwm { *hwm = new_end; }
                                }
                                // Invalidate zero_gap_table for chunks touched by this write.
                                // Same rationale as the slow path: gap entries must not shadow
                                // in-flight writes before the 50ms flush fires. Must be awaited
                                // inline here (not spawned onto flush_runtime) — spawning it
                                // detached meant flush_notify.notify_one() below could wake the
                                // background ticker, which could run flush_buffer_async_one and
                                // re-seed a gap over this chunk, before the spawned invalidation
                                // task ever ran. That stale gap then shadowed the real data this
                                // write just landed: a fresh reader with no live write-buffer
                                // slot (e.g. a new open() after the writer closed) would read
                                // through zero_gap_table and get zeros instead of the actual
                                // bytes. Root cause of the DVR-stream last-chunk stale-zero-read
                                // regression (chunk 7, page 0, in test_dvr_stream.sh).
                                {
                                    const GAP_CHUNK_SIZE: u64 = 4 * 1024 * 1024;
                                    let write_start = offset as u64;
                                    let write_end_gap = write_start + data_vec.len() as u64;
                                    let first_chunk_off = (write_start / GAP_CHUNK_SIZE) * GAP_CHUNK_SIZE;
                                    let mut chunk_off = first_chunk_off;
                                    while chunk_off < write_end_gap {
                                        client.invalidate_zero_gap_for_chunk(ino, chunk_off).await;
                                        chunk_off += GAP_CHUNK_SIZE;
                                    }
                                }
                                // Only count bytes actually added to the buffer, not the write
                                // size. Overlapping writes don't grow the slot, so adding
                                // data_vec.len() unconditionally causes the counter to drift up.
                                let added = bytes_after.saturating_sub(bytes_before);
                                if added > 0 {
                                    global_buffered_bytes.fetch_add(added, std::sync::atomic::Ordering::Relaxed);
                                }
                                {
                                    let mut counters = write_counters.write().unwrap();
                                    *counters.entry(ino).or_insert(0) += 1;
                                }
                                if has_full || pattern_changed {
                                    flush_handle.flush_notify.notify_one();
                                }
                                // Explicit drop (not end-of-scope) so the per-chunk counters
                                // decrement BEFORE the reply, same ordering as the explicit
                                // fetch_sub this replaces.
                                drop(_write_guard);
                                debug!("write fast-path: ino={} off={} len={}", ino, offset, data_vec.len());
                                reply.written(data_vec.len() as u32);
                            }
                        });
                        return;
                        } // if let Some(state_arc)
                    }
                } else {
                    drop(meta);
                }
            }
        }

        // Slow path: unlike the fast path, this can write at an offset other than
        // the original `offset` parameter (e.g. the sparse-gap branch below pads
        // from current_size, not from `offset`) — so each write_at call site below
        // computes and holds its own chunk-indexed guard from its actual
        // (offset, data) rather than reusing one computed up front.
        self.runtime.spawn(async move {
            let start = std::time::Instant::now();
            debug!("write: ino={}, offset={}, size={}", ino, offset, data_vec.len());

            let mut metadata = match metadata_cache.get(&ino) {
                Some(m) => m.clone(),
                None => {
                    // Metadata cache miss — fetch from server and populate.
                    let path_opt = inode_to_path.read().unwrap().get(&ino).cloned();
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
                                    chunk_locations: Arc::new(Vec::new()),
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
                                    symlink_target: None,
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
            // Buffer only non-SQLite files and .db-wal (is_sqlite_buffered).
            // Also check direct_write_inodes which is set race-free at create()/open() time.
            let use_write_buffer = !direct_write_inodes.contains(&ino)
                && (!is_sqlite || is_sqlite_buf);

            if write_buffer_enabled && use_write_buffer {
                let offset_usize = offset as usize;
                let current_size = {
                    let cache_size = metadata_cache.get(&ino)
                        .map(|m| m.size as usize)
                        .unwrap_or(metadata.size as usize);

                    if let Some(state_lock) = write_buffers.get(&ino) {
                        if let Ok(state) = state_lock.try_lock() {
                            let buffered_end = state.slots.iter()
                                .map(|(idx, slot)| (idx * CHUNK_SIZE as u64 + slot.span_end as u64) as usize)
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
                    if gap <= SMALL_GAP_THRESHOLD {
                        info!("Near-sequential write: offset={} current_size={} gap={} bytes — zero-filling into buffer",
                              offset_usize, current_size, gap);
                        let mut padded = vec![0u8; gap];
                        padded.extend_from_slice(&data_vec);
                        let gap_write_offset = current_size as u64;
                        let padded_len = padded.len();

                        // This writes at gap_write_offset (current_size), not the original
                        // `offset` — the chunk(s) touched can differ from write_chunk_indices
                        // computed up front from `offset`, so compute fresh here.
                        let gap_chunk_indices = chunk_indices_for_write(gap_write_offset, padded_len);
                        let _write_guard = WriteTaskGuard(
                            inc_write_tasks_for_chunks(&write_tasks_in_flight, ino, &gap_chunk_indices)
                        );

                        let state_arc = write_buffers
                            .entry(ino)
                            .or_insert_with(|| Arc::new(Mutex::new(InodeWriteState::new(is_sqlite_buf))))
                            .clone();
                        let mut state = state_arc.lock().await;
                        let bytes_before = state.resident_bytes();
                        let pattern_changed = state.write_at(gap_write_offset, &padded);
                        // Mark the gap bytes as synthetic so flush doesn't mistake them
                        // for real app data when deciding whether to PatchChunk.
                        let gap_chunk_idx = InodeWriteState::chunk_index(gap_write_offset);
                        let gap_intra = InodeWriteState::intra_offset(gap_write_offset);
                        if let Some(slot) = state.slots.get_mut(&gap_chunk_idx) {
                            slot.gap_filled_prefix = gap_intra + gap;
                        }
                        let added = state.resident_bytes().saturating_sub(bytes_before);

                        // Notify flush worker if chunks are now full or write pattern changed.
                        let has_full_chunks = !state.full_slot_indices().is_empty();
                        drop(state);
                        if added > 0 {
                            global_buffered_bytes.fetch_add(added, std::sync::atomic::Ordering::Relaxed);
                        }

                        if has_full_chunks || pattern_changed {
                            flush_handle.flush_notify.notify_one();
                        }

                        {
                            let mut counters = write_counters.write().unwrap();
                            *counters.entry(ino).or_insert(0) += 1;
                        }
                        reply.written(data_vec.len() as u32);
                        return;
                    }

                    // Large gap beyond current_size: fall through to the unified BUFFERED
                    // WRITE path below rather than special-casing it here. write_at() zero-
                    // fills only *within* the target chunk's own slot (bounded by CHUNK_SIZE),
                    // never the whole file-level gap — the earlier premise that a large gap
                    // required bypassing write_at() to avoid materializing a giant zero buffer
                    // was mistaken. flush_buffer_async_one already falls back to metadata_cache
                    // for existing_chunk_size on a chunk's first flush this session, so it
                    // correctly detects and patches an already-committed target chunk with no
                    // help needed here.
                    //
                    // Root-caused 2026-07-11 via a live qcow2 preallocation=metadata corruption
                    // investigation: this branch used to send the write directly
                    // (write_data_with_cache/PatchChunk), registering its own ChunkLocation
                    // completely outside write_buffers/write_at()'s tracking. QEMU's
                    // preallocation=metadata writes non-sequentially — a jump-ahead write here
                    // created a standalone, non-chunk-aligned ChunkLocation, and ~30ms later the
                    // normal sequential write reached the same byte range through write_at(),
                    // building a second, overlapping, chunk-aligned ChunkLocation with neither
                    // path aware of the other. Confirmed directly via dfs-admin file info: two
                    // permanent, overlapping registrations for the same bytes. Routing both
                    // through the same write_at()/write_buffers accumulator makes them coalesce
                    // into one chunk like any other pair of writes to the same region.
                    info!("Sparse write: offset {} > current_size {} (gap: {} bytes) — routing through write_at",
                           offset_usize, current_size, gap);
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

                    // Graduated back-pressure reading global_buffered_bytes directly.
                    // CRITICAL: do not use try_lock on the slot — under high concurrency all
                    // runtime threads can wait on lock().await simultaneously, starving the
                    // flush tasks needed to drain the buffer.
                    let t_bp_start = std::time::Instant::now();
                    // Data integrity outranks availability: block until the buffer drains
                    // rather than returning EIO (see matching comment on the fast-path
                    // write above). Log periodically so a genuine stall is visible.
                    const STALL_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
                    let mut next_stall_log = STALL_LOG_INTERVAL;
                    let effective_cap = global_write_buffer_cap_bytes;
                    loop {
                        let current = global_buffered_bytes.load(std::sync::atomic::Ordering::Relaxed);
                        let fill_pct = current * 100 / effective_cap.max(1);
                        let delay_ms: u64 = if fill_pct < 75 {
                            0
                        } else if fill_pct < 90 {
                            1
                        } else if fill_pct < 100 {
                            5
                        } else {
                            let waited = t_bp_start.elapsed();
                            if waited >= next_stall_log {
                                warn!("write: ino={} blocked on backpressure for {:?} (global_buffered={}  cap={} active_inodes={}) — waiting for drain, not failing",
                                      ino, waited, current, effective_cap, write_buffers.len());
                                next_stall_log += STALL_LOG_INTERVAL;
                            }
                            // See matching comment on the fast-path write above: a full buffer
                            // plus a target chunk already in_backoff (every replica just
                            // rejected it) means there's nothing to wait for — report ENOSPC.
                            let stuck = state_arc.try_lock()
                                .map(|st| chunk_indices_for_write(write_offset, data_vec.len()).iter()
                                    .any(|idx| st.slots.get(idx).is_some_and(|s| s.in_backoff())))
                                .unwrap_or(false);
                            if stuck {
                                error!("write: ino={} buffer full and target chunk(s) cannot be offloaded to any replica — ENOSPC", ino);
                                reply.error(libc::ENOSPC);
                                return;
                            }
                            10
                        };
                        if delay_ms == 0 { break; }
                        maybe_log_bpfill("slow", ino, fill_pct, delay_ms, current, effective_cap);
                        flush_handle.flush_notify.notify_one();
                        tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                        if fill_pct < 100 { break; }
                    }
                    let t_bp = t_bp_start.elapsed();

                    // write_offset == offset here, so write_chunk_indices (computed up
                    // front from the original offset/data) is still the right set.
                    let _write_guard = WriteTaskGuard(
                        inc_write_tasks_for_chunks(&write_tasks_in_flight, ino, &write_chunk_indices)
                    );

                    // t_buf: time to acquire the slot lock and copy bytes into the buffer.
                    let t_buf_start = std::time::Instant::now();
                    let mut state = state_arc.lock().await;
                    let bytes_before = state.resident_bytes();
                    let pattern_changed = state.write_at(write_offset, &data_vec);
                    let added = state.resident_bytes().saturating_sub(bytes_before);

                    // Notify the flush worker if a 4MB chunk is full or the write pattern
                    // changed from sequential to random (event-driven, no timer needed).
                    let has_full_chunks = !state.full_slot_indices().is_empty();
                    drop(state);

                    // Immediately evict zero_gap_table entries for every chunk touched by
                    // this write.  Without this, a gap seeded by a prior flush stays live
                    // and reads of the newly-written bytes return stale zeros until the
                    // next flush fires (up to 50 ms).  This is the root cause of ftruncate
                    // disk.img corruption: QEMU reads zeros in RMW cycles and writes the
                    // corruption back to disk.
                    {
                        const GAP_CHUNK_SIZE: u64 = 4 * 1024 * 1024;
                        let write_start = offset as u64;
                        let write_end = write_start + data_vec.len() as u64;
                        let first_chunk_off = (write_start / GAP_CHUNK_SIZE) * GAP_CHUNK_SIZE;
                        let mut chunk_off = first_chunk_off;
                        while chunk_off < write_end {
                            client.invalidate_zero_gap_for_chunk(ino, chunk_off).await;
                            chunk_off += GAP_CHUNK_SIZE;
                        }
                    }

                    if has_full_chunks || pattern_changed {
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
                            .map(|(idx, slot)| (idx * CHUNK_SIZE as u64 + slot.span_end as u64) as usize)
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

                        // Acquire per-chunk write locks for all affected chunks in index order
                        // (ascending) to prevent deadlock. Hold them for the entire read-modify-write.
                        let mut _chunk_guards = Vec::new();
                        for cidx in first_idx..=last_idx {
                            _chunk_guards.push(DfsFilesystem::lock_chunk(&chunk_write_locks, ino, cidx as u64).await);
                        }

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
                client.write_data_with_cache(&new_data, cache_inode, file_offset, metadata.id, None).await
            } else {
                let write_file_offset = if let Some((first_idx, _)) = affected_chunk_range {
                    metadata.chunk_locations[..first_idx].iter().map(|l| l.size as u64).sum::<u64>()
                } else {
                    0
                };
                client.write_data_with_cache(&new_data, cache_inode, write_file_offset, metadata.id, None).await
            };
            debug!("write_data took {:?}", write_start.elapsed());

            match result {
                Ok((_, _, chunk_locations_opt)) => {
                    if is_append {
                        if let Some(chunk_locations) = chunk_locations_opt {
                            Arc::make_mut(&mut metadata.chunk_locations).extend(chunk_locations);
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
                        metadata.chunk_locations = Arc::new(updated_locations);
                        let physical_size = metadata.chunk_locations.iter().map(|l| l.size as u64).sum();
                        metadata.size = metadata.size.max(physical_size);
                        info!("After splice: {} total chunks, {} total bytes",
                              metadata.chunk_locations.len(), metadata.size);
                        // The splice may not map 1:1 onto the old chunk indices (the new
                        // chunk count can differ from the old range size), so we can't assign
                        // correct per-index server_chunk_id values here. Clear server_chunk_id
                        // for every affected slot instead, so a concurrent flush_buffer_async_one
                        // falls back to the metadata_cache entry just written above (fresh)
                        // rather than trusting a chunk_id confirmed before this RMW replaced it.
                        if let Some(state_arc) = write_buffers.get(&ino) {
                            let mut state = state_arc.lock().await;
                            for cidx in first_idx..=last_idx {
                                if let Some(slot) = state.slots.get_mut(&(cidx as u64)) {
                                    slot.server_chunk_id = None;
                                }
                            }
                        }
                    } else {
                        warn!("Full file rewrite with {} bytes", new_data.len());
                        metadata.chunk_locations = Arc::new(chunk_locations_opt.unwrap_or_default());
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
            // quick-cache uses clock-based LRU, so no W-TinyLFU frequency pollution.
            // No explicit eviction needed; the engine drop lets LRU reclaim naturally.
            // We still remove the engine so the next open starts with a fresh pipeline.
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

        // If this inode was unlinked while fds were still open, and this is the last
        // fd closing, spawn a deferred cleanup task. The task waits for any in-flight
        // flush tasks (tracked by release_in_flight) to complete before issuing the
        // server delete — preventing resurrection of a deleted file by a concurrent flush.
        if is_last_open {
            if let Some((_, unlinked_path)) = self.unlinked_while_open.remove(&ino) {
                info!("release: ino={} was unlinked while open — triggering deferred server delete for {}", ino, unlinked_path);
                let def_client = self.client.clone();
                let def_path_to_inode = self.path_to_inode.clone();
                let def_inode_to_path = self.inode_to_path.clone();
                let def_pending_deletes = self.pending_deletes.clone();
                let def_metadata_cache = self.metadata_cache.clone();
                let def_write_counters = self.write_counters.clone();
                let def_last_warm_offset = self.last_warm_offset.clone();
                let def_chunk_offset_cache = self.chunk_offset_cache.clone();
                let def_release_in_flight = self.release_in_flight.clone();
                self.runtime.spawn(async move {
                    // Wait for any flush task for this inode to finish. The flush task
                    // (is_last_writer path) sees is_pending_delete=true and returns early
                    // after cleaning up write_buffers, then decrements release_in_flight.
                    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
                    loop {
                        let in_flight = def_release_in_flight.get(&ino)
                            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                            .unwrap_or(0);
                        if in_flight == 0 { break; }
                        if tokio::time::Instant::now() > deadline {
                            warn!("deferred unlink: timed out waiting for flush for ino={}", ino);
                            break;
                        }
                        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                    }
                    // Clean up remaining local state that was preserved for open fds.
                    def_metadata_cache.remove(&ino);
                    def_write_counters.write().unwrap().remove(&ino);
                    def_last_warm_offset.remove(&ino);
                    def_chunk_offset_cache.remove(&ino);
                    def_client.evict_recent_chunk_writes(ino);
                    // Remove from path_to_inode only if still our inode — a new file
                    // created at the same path takes precedence and must not be deleted.
                    let path_is_ours = def_path_to_inode.read().unwrap()
                        .get(&unlinked_path).copied() == Some(ino);
                    if path_is_ours {
                        def_path_to_inode.write().unwrap()
                            .remove_entry(&unlinked_path);
                        def_inode_to_path.write().unwrap().remove(&ino);
                        match def_client.delete_file(&unlinked_path).await {
                            Ok(_) => info!("deferred unlink: deleted {} (ino={})", unlinked_path, ino),
                            Err(e) => error!("deferred unlink: delete_file failed for {}: {}", unlinked_path, e),
                        }
                    } else {
                        info!("deferred unlink: ino={} path={} superseded by new file, orphaning chunks", ino, unlinked_path);
                    }
                    def_pending_deletes.remove(&unlinked_path);
                });
            }
        }

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
                let inode_to_path_for_release = self.inode_to_path.clone();
                let size_high_water_for_release = self.size_high_water.clone();
                let read_engines_for_release = self.client.read_engines.map.clone();
                let open_counts_for_release = self.open_counts.clone();
                let write_open_counts_for_release = self.write_open_counts.clone();
                let chunk_write_locks_for_release = self.chunk_write_locks.clone();
                // Increment per-inode release counter
                release_in_flight.entry(ino).or_insert_with(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                // NOTE: No pre-seeding of chunk_cache or read engine here.
                // Reads during the flush window are served directly from the write buffer
                // slot (see read() handler, write-buffer-read path). After the flush
                // completes, flush_buffer_async_one updates chunk_cache and calls
                // feed_chunk_locations_to_read_engine *before* removing the slot, so
                // there is no window where reads can fall through to a missing chunk.
                // Pre-seeding with block_on was blocking the FUSE dispatch thread under
                // heavy write load (many files), causing FUSE timeouts and spurious EIO.

                // Check if this file requires synchronous close. For correctness, we must wait
                // for the flush to complete before replying in these cases:
                // 1. Files opened with O_SYNC/O_DSYNC (sync_on_fsync=true)
                // 2. SQLite and database files
                // 3. qcow2/VM disk images accessed via NBD (detected by .qcow2 extension)
                //
                // Without this, mkfs.ext4 journal writes and database commits are lost when
                // the NBD/VM layer closes the file before the background flush completes.
                //
                // For streaming/DVR files, reply immediately to avoid blocking other FUSE ops.
                let path_for_sync_check = self.metadata_cache.get(&ino).map(|m| m.path.clone());
                let is_vm_disk = path_for_sync_check.as_deref()
                    .map(|p| p.ends_with(".qcow2") || p.ends_with(".raw") || p.ends_with(".img") || p.ends_with(".vmdk"))
                    .unwrap_or(false);
                let sync_on_fsync = self.write_buffers.get(&ino)
                    .and_then(|s| s.try_lock().ok().map(|state| state.sync_on_fsync))
                    .unwrap_or(false);
                let needs_sync_release = sync_on_fsync || is_vm_disk;

                if needs_sync_release {
                    info!("release: ino={} sync_release=true (sync_on_fsync={} is_vm_disk={}) — waiting for flush before replying",
                          ino, sync_on_fsync, is_vm_disk);
                    let reply_clone = reply;
                    let this_write_buffer_arc_sync = write_buffers.get(&ino).map(|e| std::sync::Arc::clone(e.value()));
                    flush_rt.spawn(async move {
                    // Wait for any concurrent write() tasks for this inode to finish writing
                    // into the slot before we flush. Without this, a close() that arrives
                    // while write() tasks are still queued flushes an incomplete slot.
                    if !wait_for_inode_writes_done(&write_tasks_in_flight, ino, std::time::Duration::from_secs(5)).await {
                        warn!("release: timed out waiting for write tasks for ino={}", ino);
                    }
                    // If the file was unlinked while this release task was queued, skip the
                    // flush — sending PutFileMetadata for a deleted file resurrects it on
                    // the server.
                    let release_path = inode_to_path_for_release.read().unwrap().get(&ino).cloned();
                    let is_pending_delete = release_path.as_deref()
                        .map_or(false, |p| pending_deletes_for_release.contains(p));
                    let path_gone = release_path.is_none();
                    if is_pending_delete || path_gone {
                        debug!("release: ino={} path={:?} deleted (pending={} path_gone={}) — skipping flush",
                               ino, release_path, is_pending_delete, path_gone);
                        write_buffers.remove(&ino);
                        flush_handle.client.evict_recent_chunk_writes(ino);
                        chunk_write_locks_for_release.remove(&ino);
                        if let Some(counter) = release_in_flight.get(&ino) {
                            counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        return;
                    }
                    // Wait for any concurrent flush (flushing=true slots from a prior
                    // release task) to complete before checking has_unflushed. Without this,
                    // a rapid write→close→write→close (T7 overwrite) can have:
                    //   release1: flushing=true on slot, writes "original"
                    //   write2:   updates slot data to "overwritten" while flushing=true
                    //   release2: sees flushing=true → has_unflushed=false → skips flush
                    //   release1: completes, keeps slot (len grew), but release2 already exited
                    // Result: "overwritten" data is never sent to the server.
                    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
                    loop {
                        let any_flushing = write_buffers.get(&ino)
                            .map(|s| s.try_lock().map(|st| st.slots.values().any(|sl| sl.flushing)).unwrap_or(true))
                            .unwrap_or(false);
                        if !any_flushing || tokio::time::Instant::now() >= deadline { break; }
                        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
                    }
                    let has_unflushed = write_buffers.get(&ino)
                        .map(|s| s.try_lock().map(|s| {
                            s.slots.values().any(|sl| !sl.is_empty() && !sl.flushing)
                        }).unwrap_or(true))
                        .unwrap_or(false);
                    if has_unflushed {
                        if let Err(e) = flush_handle.flush_all_pipelined(ino).await {
                            error!("release: flush failed for inode {}: {}", ino, e);
                        }
                        // FAP no longer calls flush_metadata_sync — it only flushes chunk data.
                        // Send metadata here, after ALL chunks are confirmed written, so the
                        // leader receives a complete and accurate snapshot. The server's
                        // handle_put_file_metadata uses its authoritative chunk_map to override
                        // any stale entries, so this is safe even if a new session has started.
                        // Re-check pending_delete: a delete can arrive while FAP was running.
                        {
                            let path_now = inode_to_path_for_release.read().unwrap().get(&ino).cloned();
                            let still_live = path_now.as_deref()
                                .map_or(false, |p| !pending_deletes_for_release.contains(p));
                            if still_live {
                                if let Some(meta) = flush_handle.metadata_cache.get(&ino).map(|m| m.clone()) {
                                    flush_handle.client.flush_metadata_sync(&meta).await;
                                }
                                flush_handle.last_metadata_update.insert(ino, std::time::Instant::now());
                            }
                        }
                    } else {
                        // Background ticker already flushed the data. Still need to guarantee
                        // the leader has current metadata before release_in_flight drops to
                        // zero — preventing a concurrent read open from getting stale chunk IDs.
                        // Re-check pending_delete: a delete can arrive while we waited above.
                        let path_now = inode_to_path_for_release.read().unwrap().get(&ino).cloned();
                        let still_live = path_now.as_deref()
                            .map_or(false, |p| !pending_deletes_for_release.contains(p));
                        if still_live {
                            let meta_to_persist = flush_handle.metadata_cache.get(&ino).map(|m| m.clone());
                            if let Some(meta) = meta_to_persist {
                                // Don't blindly re-send whatever metadata_cache currently holds —
                                // nothing was buffered this session (has_unflushed=false), so this
                                // cache entry's chunk_locations may never have been populated with
                                // real data (e.g. a scalar-only startup warm-up entry, right after a
                                // client restart). Sending it while empty on a file that isn't
                                // genuinely empty (size>0) claims "this file has no chunks" — a real
                                // incident, 2026-07-03 (staging nanopir3): the server trusted exactly
                                // this shape of commit as an intentional truncate and wiped its own
                                // correct chunk_map entry, and the next write zeroed real content
                                // over the gap. The server now refuses that specific case too
                                // (chunk_map_update requires size==0 to treat empty chunk_locations
                                // as a genuine truncate), but the client shouldn't construct and
                                // send a claim about file state it doesn't actually know in the
                                // first place — skip the redundant commit when there's nothing
                                // trustworthy to report.
                                let trustworthy = !meta.chunk_locations.is_empty() || meta.size == 0;
                                if trustworthy {
                                    flush_handle.client.flush_metadata_sync(&meta).await;
                                    flush_handle.last_metadata_update.insert(ino, std::time::Instant::now());
                                } else {
                                    warn!("release: ino={} skipping metadata re-commit — cached chunk_locations empty but size={} (untrustworthy snapshot, nothing changed this session)",
                                        ino, meta.size);
                                }
                            }
                        }
                    }
                    // Invalidate the read engine's chunk map so the next reader
                    // immediately picks up the newly flushed chunks — but only if no new
                    // writer has opened since this release task was spawned. A new O_TRUNC
                    // open pre-seeds the engine with the new session's chunk_id; expiring
                    // it here would wipe that out, causing the next cat to fall through to
                    // the server and read stale data (T7 race).
                    let has_new_writer = write_open_counts_for_release
                        .get(&ino).map(|c| *c > 0).unwrap_or(false);
                    if !has_new_writer {
                        if let Some(engine) = read_engines_for_release.get(&ino) {
                            engine.expire_chunk_map();
                        }
                        // No readers left either — free the engine entirely.
                        if !open_counts_for_release.get(&ino).map(|c| *c > 0).unwrap_or(false) {
                            read_engines_for_release.remove(&ino);
                        }
                    }
                    // Only remove the write buffer if no new writer has opened the file
                    // since this release task was spawned, AND the DashMap still holds the
                    // same Arc we were working with (Arc identity check). See async path for
                    // full explanation of the rapid write→close→write→close race.
                    let has_new_writer = write_open_counts_for_release
                        .get(&ino).map(|c| *c > 0).unwrap_or(false);
                    let is_our_buffer_sync = this_write_buffer_arc_sync.as_ref()
                        .map(|orig| write_buffers.get(&ino)
                            .map(|e| std::sync::Arc::ptr_eq(orig, e.value()))
                            .unwrap_or(false))
                        .unwrap_or(false);
                    let has_unflushed_data_sync = write_buffers.get(&ino)
                        .map(|s| s.try_lock().map(|st| st.slots.values().any(|sl| !sl.is_empty())).unwrap_or(true))
                        .unwrap_or(false);
                    if !has_new_writer && is_our_buffer_sync && !has_unflushed_data_sync {
                        // Do NOT remove write_buffers itself here. slots is already empty
                        // (has_unflushed_data_sync=false), so removing the entry serves no
                        // cleanup purpose — its only effect is discarding flushed_sizes /
                        // canonical_write_nodes / canonical_node_miss_streak, which are cheap
                        // (one HashMap entry per touched chunk) but load-bearing: they're what
                        // lets the next write_at() know a chunk already has content on the
                        // server. QEMU (qcow2 preallocation, mkfs.ext4, etc.) closes and
                        // reopens VM disk images multiple times per second — same pattern
                        // already documented below for recent_chunk_writes. Losing
                        // flushed_sizes across one of those reopens means the next fresh
                        // write to a chunk that already has content builds its slot with zero
                        // knowledge of the existing size, and a later patch decision that
                        // should target the same generation instead straddles two disconnected
                        // ones. Root-caused 2026-07-11 via a local qemu-img convert -O qcow2
                        // repro: confirmed live (flushed_sizes read as 0 immediately after a
                        // release, despite the chunk holding real server-confirmed content
                        // moments earlier) and correlated with a deterministic qcow2 "overlaps
                        // with active L2 table" corruption that a byte-identical raw-format
                        // conversion under the same write pattern does NOT reproduce — ruling
                        // out data loss and pointing at exactly this kind of session-boundary
                        // bookkeeping gap. Reused safely across reopen: same inode, same path,
                        // same file identity — unlike create()'s stale-buffer hazard (see its
                        // O_CREAT comment), nothing here changes across a plain close+reopen.
                        // Genuine identity changes (truncate, unlink, delete+recreate) already
                        // have their own explicit write_buffers.remove() call elsewhere.
                        size_high_water_for_release.remove(&ino);
                        // Do NOT evict recent_chunk_writes here — see open() is_first_writer
                        // comment. QEMU closes and reopens the disk every few seconds; evicting
                        // on close triggers the stale-base cascade on the very next open.
                        chunk_write_locks_for_release.remove(&ino);
                    }
                    if let Some(owner) = lock_owner {
                        if let Err(e) = lock_manager.release_all(ino, owner).await {
                            error!("release: lock release failed for inode {}: {}", ino, e);
                        }
                    }
                    if let Some(counter) = release_in_flight.get(&ino) {
                        counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    // For sync_on_fsync files, reply to FUSE only after flush completes.
                    // This was set earlier to reply_clone for the sync path.
                    reply_clone.ok();
                });
                } else {
                    // Async path: reply immediately for DVR/streaming files.
                    // Parking a main-runtime worker for the full flush duration (up to 10s
                    // for metadata delivery) starves all other FUSE ops (readdir, getattr,
                    // read) during DVR startup scans.
                    reply.ok();
                    // Capture the Arc identity of the write buffer we're about to flush.
                    // Used at cleanup to avoid removing a newer session's buffer: a rapid
                    // write→close→write→close (T22b pattern) can have:
                    //   write_i9 closes: spawns this task with write_buffers[ino] = Arc_i9
                    //   write_i10 opens+writes+closes: write_buffers[ino] = Arc_i10
                    //   this task reaches write_buffers.remove: count=0 (i10 already closed),
                    //     removes Arc_i10 instead of Arc_i9 → i10's data silently lost.
                    let this_write_buffer_arc = write_buffers.get(&ino).map(|e| std::sync::Arc::clone(e.value()));
                    flush_rt.spawn(async move {
                    // Wait for any concurrent write() tasks for this inode to finish writing
                    // into the slot before we flush. Without this, a close() that arrives
                    // while write() tasks are still queued flushes an incomplete slot.
                    if !wait_for_inode_writes_done(&write_tasks_in_flight, ino, std::time::Duration::from_secs(5)).await {
                        warn!("release: timed out waiting for write tasks for ino={}", ino);
                    }
                    // If the file was unlinked while this release task was queued, skip the
                    // flush — sending PutFileMetadata for a deleted file resurrects it on
                    // the server.
                    let release_path = inode_to_path_for_release.read().unwrap().get(&ino).cloned();
                    let is_pending_delete = release_path.as_deref()
                        .map_or(false, |p| pending_deletes_for_release.contains(p));
                    let path_gone = release_path.is_none();
                    if is_pending_delete || path_gone {
                        debug!("release: ino={} path={:?} deleted (pending={} path_gone={}) — skipping flush",
                               ino, release_path, is_pending_delete, path_gone);
                        write_buffers.remove(&ino);
                        flush_handle.client.evict_recent_chunk_writes(ino);
                        chunk_write_locks_for_release.remove(&ino);
                        if let Some(counter) = release_in_flight.get(&ino) {
                            counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        return;
                    }
                    // Wait for any concurrent flush (flushing=true slots from a prior
                    // release task) to complete before checking has_unflushed. Without this,
                    // a rapid write→close→write→close (T7 overwrite) can have:
                    //   release1: flushing=true on slot, writes "original"
                    //   write2:   updates slot data to "overwritten" while flushing=true
                    //   release2: sees flushing=true → has_unflushed=false → skips flush
                    //   release1: completes, keeps slot (len grew), but release2 already exited
                    // Result: "overwritten" data is never sent to the server.
                    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
                    loop {
                        let any_flushing = write_buffers.get(&ino)
                            .map(|s| s.try_lock().map(|st| st.slots.values().any(|sl| sl.flushing)).unwrap_or(true))
                            .unwrap_or(false);
                        if !any_flushing || tokio::time::Instant::now() >= deadline { break; }
                        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
                    }
                    let has_unflushed = write_buffers.get(&ino)
                        .map(|s| s.try_lock().map(|s| {
                            s.slots.values().any(|sl| !sl.is_empty() && !sl.flushing)
                        }).unwrap_or(true))
                        .unwrap_or(false);
                    if has_unflushed {
                        if let Err(e) = flush_handle.flush_all_pipelined(ino).await {
                            error!("release: flush failed for inode {}: {}", ino, e);
                        }
                        // FAP no longer calls flush_metadata_sync — it only flushes chunk data.
                        // Send metadata here, after ALL chunks are confirmed written, so the
                        // leader receives a complete and accurate snapshot. The server's
                        // handle_put_file_metadata uses its authoritative chunk_map to override
                        // any stale entries, so this is safe even if a new session has started.
                        // Re-check pending_delete: a delete can arrive while FAP was running.
                        {
                            let path_now = inode_to_path_for_release.read().unwrap().get(&ino).cloned();
                            let still_live = path_now.as_deref()
                                .map_or(false, |p| !pending_deletes_for_release.contains(p));
                            if still_live {
                                if let Some(meta) = flush_handle.metadata_cache.get(&ino).map(|m| m.clone()) {
                                    flush_handle.client.flush_metadata_sync(&meta).await;
                                }
                                flush_handle.last_metadata_update.insert(ino, std::time::Instant::now());
                            }
                        }
                    } else {
                        // Background ticker already flushed the data. Still need to guarantee
                        // the leader has current metadata before release_in_flight drops to
                        // zero — preventing a concurrent read open from getting stale chunk IDs.
                        // Re-check pending_delete: a delete can arrive while we waited above.
                        let path_now = inode_to_path_for_release.read().unwrap().get(&ino).cloned();
                        let still_live = path_now.as_deref()
                            .map_or(false, |p| !pending_deletes_for_release.contains(p));
                        if still_live {
                            let meta_to_persist = flush_handle.metadata_cache.get(&ino).map(|m| m.clone());
                            if let Some(meta) = meta_to_persist {
                                // Don't blindly re-send whatever metadata_cache currently holds —
                                // nothing was buffered this session (has_unflushed=false), so this
                                // cache entry's chunk_locations may never have been populated with
                                // real data (e.g. a scalar-only startup warm-up entry, right after a
                                // client restart). Sending it while empty on a file that isn't
                                // genuinely empty (size>0) claims "this file has no chunks" — a real
                                // incident, 2026-07-03 (staging nanopir3): the server trusted exactly
                                // this shape of commit as an intentional truncate and wiped its own
                                // correct chunk_map entry, and the next write zeroed real content
                                // over the gap. The server now refuses that specific case too
                                // (chunk_map_update requires size==0 to treat empty chunk_locations
                                // as a genuine truncate), but the client shouldn't construct and
                                // send a claim about file state it doesn't actually know in the
                                // first place — skip the redundant commit when there's nothing
                                // trustworthy to report.
                                let trustworthy = !meta.chunk_locations.is_empty() || meta.size == 0;
                                if trustworthy {
                                    flush_handle.client.flush_metadata_sync(&meta).await;
                                    flush_handle.last_metadata_update.insert(ino, std::time::Instant::now());
                                } else {
                                    warn!("release: ino={} skipping metadata re-commit — cached chunk_locations empty but size={} (untrustworthy snapshot, nothing changed this session)",
                                        ino, meta.size);
                                }
                            }
                        }
                    }
                    // Invalidate the read engine's chunk map so the next reader
                    // immediately picks up the newly flushed chunks — but only if no new
                    // writer has opened since this release task was spawned. A new O_TRUNC
                    // open pre-seeds the engine with the new session's chunk_id; expiring
                    // it here would wipe that out, causing the next cat to fall through to
                    // the server and read stale data (T7 race).
                    let has_new_writer = write_open_counts_for_release
                        .get(&ino).map(|c| *c > 0).unwrap_or(false);
                    if !has_new_writer {
                        if let Some(engine) = read_engines_for_release.get(&ino) {
                            engine.expire_chunk_map();
                        }
                        // No readers left either — free the engine entirely.
                        if !open_counts_for_release.get(&ino).map(|c| *c > 0).unwrap_or(false) {
                            read_engines_for_release.remove(&ino);
                        }
                    }
                    // Only remove the write buffer if no new writer has opened the file
                    // since this release task was spawned. A new O_TRUNC open races with
                    // this cleanup — if we remove here, we destroy the new session's buffer
                    // and its data is silently lost (T7 race).
                    //
                    // Also guard by Arc identity: a rapid write→close→write→close sequence
                    // can have write_i10 create a new buffer (Arc_i10) while this task is
                    // still running. If write_i10 closes before we reach here (count=0),
                    // has_new_writer=false, but the DashMap entry is Arc_i10 not our Arc_i9.
                    // Removing without the identity check silently destroys Arc_i10's data.
                    let has_new_writer = write_open_counts_for_release
                        .get(&ino).map(|c| *c > 0).unwrap_or(false);
                    let is_our_buffer = this_write_buffer_arc.as_ref()
                        .map(|orig| write_buffers.get(&ino)
                            .map(|e| std::sync::Arc::ptr_eq(orig, e.value()))
                            .unwrap_or(false))
                        .unwrap_or(false);
                    // Also guard against removing a buffer that received new data AFTER our
                    // flush completed. A write() arriving between slot-removal and cleanup can
                    // create a fresh slot in Arc_i1; if we remove Arc_i1 here, R2 (the next
                    // release task) finds no buffer and silently drops that data (T22b race).
                    let has_unflushed_data = write_buffers.get(&ino)
                        .map(|s| s.try_lock().map(|st| st.slots.values().any(|sl| !sl.is_empty())).unwrap_or(true))
                        .unwrap_or(false);
                    if !has_new_writer && is_our_buffer && !has_unflushed_data {
                        // Do NOT remove write_buffers itself — see the sync release path's
                        // identical guard for the full explanation (flushed_sizes/
                        // canonical_write_nodes continuity across a same-file reopen).
                        size_high_water_for_release.remove(&ino);
                        // Do NOT evict recent_chunk_writes — see sync release path comment.
                        chunk_write_locks_for_release.remove(&ino);
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
                }
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
                        s.slots.values().any(|sl| !sl.is_empty() && !sl.flushing)
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
                        flush_handle.client.evict_recent_chunk_writes(ino);
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
            chunk_locations: Arc::new(Vec::new()),
            write_seq: 0,
            symlink_target: None,
        };

        // Store metadata on cluster — spawn so we never block_on the FUSE dispatch thread.
        let client = self.client.clone();
        let metadata_clone = metadata.clone();
        let metadata_cache = self.metadata_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let dir_cache_invalidated_at = self.dir_cache_invalidated_at.clone();
        let path_to_inode = self.path_to_inode.clone();
        let inode_to_path = self.inode_to_path.clone();
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
                            inode_to_path.write().unwrap().insert(v, path.clone());
                            v
                        }
                    };
                    metadata_cache.insert(ino, metadata.clone());

                    let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                    let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                    DfsFilesystem::invalidate_dir_cache(&dir_cache, &dir_cache_invalidated_at, parent_path);

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

    fn symlink(
        &mut self,
        _req: &FuseRequest,
        parent: u64,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        debug!("symlink: parent={}, link_name={:?}, target={:?}", parent, link_name, target);

        let path = match self.get_path_from_parent(parent, link_name) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let target_str = match target.to_str() {
            Some(t) => t.to_string(),
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        // Symlink target is stored inline in metadata (see FileMetadata::symlink_target's
        // doc comment) rather than as chunk data — no chunk allocation/replication needed.
        // size mirrors the target string length, matching readlink()'s convention.
        let metadata = FileMetadata {
            id: dfs_common::FileId::new(),
            path: path.clone(),
            size: target_str.len() as u64,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            modified_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            mode: 0o777,
            uid: _req.uid(),
            gid: _req.gid(),
            file_type: FileType::Symlink,
            chunk_locations: Arc::new(Vec::new()),
            write_seq: 0,
            symlink_target: Some(target_str),
        };

        // Store metadata on cluster — spawn so we never block_on the FUSE dispatch thread.
        let client = self.client.clone();
        let metadata_clone = metadata.clone();
        let metadata_cache = self.metadata_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let dir_cache_invalidated_at = self.dir_cache_invalidated_at.clone();
        let path_to_inode = self.path_to_inode.clone();
        let inode_to_path = self.inode_to_path.clone();
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
                            inode_to_path.write().unwrap().insert(v, path.clone());
                            v
                        }
                    };
                    metadata_cache.insert(ino, metadata.clone());

                    let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                    let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                    DfsFilesystem::invalidate_dir_cache(&dir_cache, &dir_cache_invalidated_at, parent_path);

                    let attr = DfsFilesystem::metadata_to_attr_static(ino, &metadata);
                    reply.entry(&Duration::ZERO, &attr, 0);
                }
                Err(e) => {
                    error!("Failed to create symlink {}: {}", path, e);
                    reply.error(libc::EIO);
                }
            }
        });
    }

    fn readlink(&mut self, _req: &FuseRequest, ino: u64, reply: ReplyData) {
        debug!("readlink: ino={}", ino);

        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let inode_to_path = self.inode_to_path.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            let cached = metadata_cache.get(&ino).map(|m| m.clone());

            let metadata = match cached {
                Some(m) => Some(m),
                None => {
                    let path_opt = inode_to_path.read().unwrap().get(&ino).cloned();
                    match path_opt {
                        Some(path) => match client.get_file_metadata(&path).await {
                            Ok(Some(fetched)) => {
                                metadata_cache.insert(ino, fetched.clone());
                                last_metadata_update.insert(ino, std::time::Instant::now());
                                Some(fetched)
                            }
                            _ => None,
                        },
                        None => None,
                    }
                }
            };

            match metadata {
                Some(m) if m.file_type == FileType::Symlink => {
                    match m.symlink_target {
                        Some(target) => reply.data(target.as_bytes()),
                        None => {
                            error!("readlink: ino={} is a symlink with no stored target", ino);
                            reply.error(libc::EIO);
                        }
                    }
                }
                Some(_) => reply.error(libc::EINVAL),
                None => reply.error(libc::ENOENT),
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
        let dir_cache_invalidated_at = self.dir_cache_invalidated_at.clone();
        let path_to_inode = self.path_to_inode.clone();
        let inode_to_path = self.inode_to_path.clone();
        let write_buffers = self.write_buffers.clone();
        let write_counters = self.write_counters.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let last_bg_metadata_push = self.flush_handle.last_bg_metadata_push.clone();
        let last_warm_offset = self.last_warm_offset.clone();
        let chunk_offset_cache = self.chunk_offset_cache.clone();
        let pending_deletes = self.pending_deletes.clone();
        let open_counts = self.open_counts.clone();
        let unlinked_while_open = self.unlinked_while_open.clone();

        // Mark as pending-delete immediately so concurrent lookup() returns ENOENT
        // even while the server-side delete is still in flight.
        pending_deletes.insert(path.clone());
        debug!("unlink: inserted {:?} into pending_deletes (len={}, ptr={:p})", path, pending_deletes.len(), Arc::as_ptr(&pending_deletes));

        self.runtime.spawn(async move {
            let ino_opt = path_to_inode.read().unwrap().get(&path).copied();
            let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
            let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };

            // POSIX unlink-while-open: if any fd is still open, defer path removal and
            // server delete until the last fd closes. path_to_inode stays so in-flight
            // writes can resolve the path; pending_deletes prevents new lookups from
            // seeing the file. release() picks up the deferred cleanup via unlinked_while_open.
            let still_open = ino_opt
                .map(|ino| open_counts.get(&ino).map(|c| *c > 0).unwrap_or(false))
                .unwrap_or(false);
            if still_open {
                if let Some(ino) = ino_opt {
                    info!("unlink: ino={} path={} still open — deferring server delete", ino, path);
                    unlinked_while_open.insert(ino, path.clone());
                }
                DfsFilesystem::invalidate_dir_cache(&dir_cache, &dir_cache_invalidated_at, parent_path);
                reply.ok();
                return;
            }

            // No open fds — proceed with immediate cleanup.
            let file_id_opt = ino_opt.and_then(|ino| {
                metadata_cache.remove(&ino).map(|(_, m)| m.id)
            });
            if let Some(ino) = ino_opt {
                write_buffers.remove(&ino);
                client.evict_recent_chunk_writes(ino);
                write_counters.write().unwrap().remove(&ino);
                last_metadata_update.remove(&ino);
                last_bg_metadata_push.remove(&ino);
                last_warm_offset.remove(&ino);
                chunk_offset_cache.remove(&ino);
                inode_to_path.write().unwrap().remove(&ino);
            }
            path_to_inode.write().unwrap().remove(&path);
            DfsFilesystem::invalidate_dir_cache(&dir_cache, &dir_cache_invalidated_at, parent_path);

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
        let dir_cache_invalidated_at = self.dir_cache_invalidated_at.clone();
        let path_to_inode = self.path_to_inode.clone();
        let inode_to_path = self.inode_to_path.clone();

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
                                inode_to_path.write().unwrap().remove(&ino);
                            }
                            path_to_inode.write().unwrap().remove(&path);

                            let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                            let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                            DfsFilesystem::invalidate_dir_cache(&dir_cache, &dir_cache_invalidated_at, parent_path);

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
        let dir_cache_invalidated_at = self.dir_cache_invalidated_at.clone();
        let path_to_inode = self.path_to_inode.clone();
        let inode_to_path = self.inode_to_path.clone();
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
                client.evict_recent_chunk_writes(old_ino);
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
                    // Update local path→inode mapping (forward and reverse).
                    path_to_inode.write().unwrap().remove(&old_path);
                    path_to_inode.write().unwrap().insert(new_path.clone(), old_ino);
                    inode_to_path.write().unwrap().insert(old_ino, new_path.clone());

                    // Update local metadata cache with new path. Per POSIX, rename()
                    // does not change mtime (only ctime) — don't stamp modified_at=now()
                    // here, or it clobbers an explicit mtime set via setattr() just
                    // before the rename (T37: rsync -a's write -> utimes -> rename).
                    let mut new_metadata = metadata.clone();
                    new_metadata.path = new_path.clone();
                    metadata_cache.insert(old_ino, new_metadata);

                    // Invalidate directory caches.
                    let raw_old = old_path.rsplitn(2, '/').nth(1).unwrap_or("");
                    let old_parent = if raw_old.is_empty() { "/" } else { raw_old };
                    let raw_new = new_path.rsplitn(2, '/').nth(1).unwrap_or("");
                    let new_parent = if raw_new.is_empty() { "/" } else { raw_new };
                    DfsFilesystem::invalidate_dir_cache(&dir_cache, &dir_cache_invalidated_at, old_parent);
                    if old_parent != new_parent {
                        DfsFilesystem::invalidate_dir_cache(&dir_cache, &dir_cache_invalidated_at, new_parent);
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
        mtime: Option<fuser::TimeOrNow>,
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

        // No-op detection: skip the full metadata round-trip (and the downstream
        // server-side write) when nothing this call would actually change differs
        // from what's already stored. Added 2026-07-14: a touch-style pass over
        // many files (DVR indexing) was issuing a full put_file_metadata round
        // trip per file even when the mtime being set already matched what was
        // stored — no no-op check existed anywhere in this path, client or
        // server — generating enough write churn to keep triggering full-database
        // redb compaction during active benchmarks. Checked against the snapshot
        // just read above, before any of this function's own mutation logic runs,
        // so a genuinely different value from a concurrent flush is decided from
        // fields callers of setattr never touch anyway (concurrent flush tasks
        // only change size/chunk_locations, and the explicit_mtime_pending
        // mechanism below already exists specifically to arbitrate mtime races
        // with those) — TimeOrNow::Now is never treated as a no-op since "now"
        // essentially never coincides with a stored second-granularity timestamp.
        let mode_noop = mode.map_or(true, |v| v == metadata.mode);
        let uid_noop = uid.map_or(true, |v| v == metadata.uid);
        let gid_noop = gid.map_or(true, |v| v == metadata.gid);
        let size_noop = size.map_or(true, |v| v == metadata.size);
        let mtime_noop = match mtime {
            None => true,
            Some(fuser::TimeOrNow::SpecificTime(t)) => {
                t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() == metadata.modified_at
            }
            Some(fuser::TimeOrNow::Now) => false,
        };
        if mode_noop && uid_noop && gid_noop && size_noop && mtime_noop {
            let attr = self.metadata_to_attr(ino, &metadata);
            reply.attr(&Duration::from_secs(2), &attr);
            return;
        }

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
                    metadata.chunk_locations = Arc::new(Vec::new());
                    metadata.size = 0;
                    self.write_buffers.remove(&ino);
                    self.client.evict_recent_chunk_writes(ino);
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
                                client.write_data_with_cache(truncated_chunk, ino, chunk_offset, metadata.id, None).await
                            }) {
                                Ok((_, _, chunk_locations_opt)) => {
                                    let mut new_locs = metadata.chunk_locations[..last_chunk_idx].to_vec();
                                    if let Some(new_chunk_locs) = chunk_locations_opt {
                                        new_locs.extend(new_chunk_locs);
                                    }
                                    metadata.chunk_locations = Arc::new(new_locs);
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
                            Arc::make_mut(&mut metadata.chunk_locations).truncate(last_chunk_idx + 1);
                            metadata.size = new_size;
                        }
                    }
                }
            }
        }

        // Honor an explicit utimes()/utimensat() request (e.g. rsync -a restoring the
        // source mtime after a transfer) so the stored mtime round-trips correctly.
        // Without this, every setattr stamped modified_at=now(), so a client that
        // preserves mtimes would see its restored timestamp overwritten on the very
        // call that set it — making every subsequent quick-check comparison fail and
        // forcing re-transfers on every run.
        if let Some(mt) = mtime {
            metadata.modified_at = match mt {
                fuser::TimeOrNow::SpecificTime(t) => {
                    // Flag this inode so a chunk flush still in flight from an
                    // earlier write() doesn't stamp modified_at=now() and clobber
                    // the explicit mtime we're about to set (T37: rsync -a's
                    // write -> utimes -> chmod -> rename temp-file pattern).
                    self.explicit_mtime_pending.insert(ino);
                    t.duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                }
                fuser::TimeOrNow::Now => SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            };
        } else if size.is_some() {
            // No explicit mtime, but the size changed (truncate): POSIX bumps mtime to now.
            metadata.modified_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
        }
        // else: pure chmod/chown with no size/mtime change — leave modified_at as-is.

        // Store updated metadata — stamp write_seq so the server doesn't drop it as stale
        // (the last flush_metadata_sync incremented write_seq on the server, so sending the
        // cached pre-stamp value would be rejected; stamp here to always be monotonically newer).
        //
        // When size is not being explicitly set (no truncation), build the outgoing metadata
        // from the live cache entry rather than our initial snapshot. A concurrent flush task
        // on flush_rt may update metadata_cache (size, chunk_locations) via get_mut() at any
        // time between when we read the snapshot and now. If we stamp and send the stale
        // snapshot, the subsequent flush syncs (flush_all_pipelined final + release explicit)
        // will read the live cache and send size=correct — but if we later clobber the cache
        // with the stale entry, those syncs read size=0 and win with a higher write_seq.
        // Solution: always send max(snapshot, live) and update cache via get_mut() so we
        // never replace a concurrent in-place update with a stale full-entry insert.
        let send_meta = if size.is_none() {
            // Merge: take the live entry and apply only the setattr fields onto it.
            // This means the size/chunk_locations from any concurrent flush are preserved
            // both in what we send and in what we write back to the cache.
            let mut m = self.metadata_cache.get(&ino)
                .map(|e| e.clone())
                .unwrap_or_else(|| metadata.clone());
            if let Some(v) = mode { m.mode = v; }
            if let Some(v) = uid  { m.uid  = v; }
            if let Some(v) = gid  { m.gid  = v; }
            m.modified_at = metadata.modified_at;
            m
        } else {
            metadata.clone()
        };

        let client = self.client.clone();
        let mut send_stamped = client.stamp_write_seq_pub(&send_meta);
        let result = self.block_on(async {
            client.put_file_metadata(&send_stamped).await
        });

        match result {
            Ok(_) => {
                // Update cache via get_mut() so we patch only the setattr fields onto
                // whatever the live entry currently contains (which may have been updated
                // by a concurrent flush between our block_on call and now).
                // Never replace size/chunk_locations with our snapshot unless the caller
                // explicitly requested a truncation (size.is_some()).
                if size.is_none() {
                    if let Some(mut live) = self.metadata_cache.get_mut(&ino) {
                        if let Some(v) = mode { live.mode = v; }
                        if let Some(v) = uid  { live.uid  = v; }
                        if let Some(v) = gid  { live.gid  = v; }
                        live.modified_at = send_stamped.modified_at;
                        live.write_seq   = send_stamped.write_seq;
                        send_stamped = live.clone();
                    } else {
                        self.metadata_cache.insert(ino, send_stamped.clone());
                    }
                } else {
                    self.metadata_cache.insert(ino, send_stamped.clone());
                }

                // Convert to FUSE attr
                let attr = self.metadata_to_attr(ino, &send_stamped);
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
        // fsync on the root inode (ino==1) means `sync /mount` — flush everything.
        if ino == 1 {
            self.fsyncdir(_req, 1, _fh, datasync, reply);
            return;
        }

        let path = self.metadata_cache.get(&ino).map(|m| m.path.clone()).unwrap_or_default();
        let active_writers = self.write_open_counts.get(&ino).map(|c| *c).unwrap_or(0);
        let tasks_in_flight = write_tasks_in_flight_for_inode(&self.write_tasks_in_flight, ino);
        let buffered_slots = self.write_buffers.get(&ino)
            .and_then(|s| s.try_lock().ok().map(|st| st.slots.len()))
            .unwrap_or(0);
        info!("fsync: ino={} datasync={} path={:?} active_writers={} tasks_in_flight={} buffered_slots={}",
              ino, datasync, path, active_writers, tasks_in_flight, buffered_slots);

        // Direct-write inodes (SQLite .db / .db-journal) bypass the write buffer.
        // Their writes are async tasks — fsync must wait for all of them to land
        // before replying, otherwise reads after fdatasync see stale/zero data.
        if self.direct_write_inodes.contains(&ino) {
            let write_tasks_in_flight = self.write_tasks_in_flight.clone();
            self.runtime.spawn(async move {
                // Returns immediately if nothing is in flight for this inode, same
                // as the old else-branch's fast path when no counter entry existed.
                if !wait_for_inode_writes_done(&write_tasks_in_flight, ino, std::time::Duration::from_secs(10)).await {
                    error!("fsync: timed out waiting for direct write tasks for ino={}", ino);
                    reply.error(libc::EIO);
                    return;
                }
                info!("fsync: ino={} → direct-write path, done", ino);
                reply.ok();
            });
            return;
        }

        if self.write_buffer_enabled {
            // Check whether this inode needs synchronous fsyncs (SQLite or O_SYNC/O_DSYNC).
            // SQLite files always flush synchronously — check the path first (fast, no lock
            // needed) before falling back to the buffer state flag. This avoids the race where
            // try_lock() returns false because a write holds the buffer, causing fsync to go
            // async and reads after fdatasync to see stale data (SQLITE_CORRUPT).
            let is_sqlite_ino = self.metadata_cache.get(&ino)
                .map(|m| is_sqlite_buffered(&m.path))
                .unwrap_or(false);
            let sync_on_fsync = is_sqlite_ino || self.write_buffers.get(&ino)
                .map(|state_lock| {
                    state_lock.try_lock().map(|s| s.sync_on_fsync).unwrap_or(false)
                })
                .unwrap_or(false);

            // All three branches (O_SYNC/SQLite, no active writers, active writers) do the
            // same thing: flush and reply. Spawn so the FUSE dispatch thread is freed
            // immediately — the calling process's fsync() syscall blocks in the kernel
            // until the reply arrives, satisfying POSIX durability semantics without
            // monopolising the dispatch thread and starving other FUSE requests.
            let handle = self.flush_handle.clone();
            if sync_on_fsync {
                info!("fsync: ino={} path={:?} → sync flush (O_SYNC/SQLite)", ino, path);
            } else if active_writers == 0 {
                info!("fsync: ino={} path={:?} → sync flush (no active writers)", ino, path);
            } else {
                info!("fsync: ino={} path={:?} → sync flush (active_writers={})", ino, path, active_writers);
            }
            self.runtime.spawn(async move {
                match handle.flush_all_pipelined(ino).await {
                    Ok(_) => {
                        // Commit metadata to the leader after flushing chunk data.
                        // For long-lived file handles (VM disks, databases) that are never
                        // released between write sessions, fsync is the only durability
                        // boundary — metadata must be committed here so followers receive
                        // routing updates before the next read. The server's authoritative
                        // chunk_map guard makes mid-session metadata syncs ghost-free.
                        //
                        // Debounce: skip redundant PutFileMetadata RPCs on rapid fsyncs
                        // (e.g. VM-disk random writes). The per-chunk RCL fired by each
                        // replica already keeps the leader's in-memory chunk_map current
                        // for routing; a full PutFileMetadata is only needed periodically.
                        // release() always calls flush_metadata_sync unconditionally,
                        // guaranteeing a final durable commit on file close.
                        const METADATA_SYNC_DEBOUNCE_MS: u128 = 500;
                        if let Some(meta) = handle.metadata_cache.get(&ino).map(|m| m.clone()) {
                            let needs_sync = handle.last_metadata_update
                                .get(&ino)
                                .map(|t| t.elapsed().as_millis() >= METADATA_SYNC_DEBOUNCE_MS)
                                .unwrap_or(true);
                            // Same untrustworthy-snapshot guard as release() — fsync() can fire
                            // on a session that hasn't written this file's chunk data (e.g. a
                            // defensive/periodic fsync right after open), so metadata_cache may
                            // still hold a scalar-only entry with no real chunk_locations.
                            let trustworthy = !meta.chunk_locations.is_empty() || meta.size == 0;
                            if needs_sync && trustworthy {
                                handle.client.flush_metadata_sync(&meta).await;
                                handle.last_metadata_update.insert(ino, std::time::Instant::now());
                            }
                        }
                        reply.ok();
                    }
                    Err(e) => { error!("fsync failed for inode {}: {}", ino, e); reply.error(libc::EIO); }
                }
            });
        } else {
            // No write buffer, but we still need to flush any pending metadata updates
            // that were batched by the write() path to ensure data durability
            let client = self.client.clone();
            let metadata_cache = self.metadata_cache.clone();
            let write_counters = self.write_counters.clone();

            self.runtime.spawn(async move {
                let metadata_opt = metadata_cache.get(&ino).map(|m| m.clone());
                if let Some(metadata) = metadata_opt {
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
                reply.ok();
            });
        }
    }

    fn fsyncdir(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: fuser::ReplyEmpty,
    ) {
        // Only do a full flush when called on the root inode (i.e. `sync /mount`).
        // For other directories there is nothing to flush — return ok immediately.
        if ino != 1 {
            reply.ok();
            return;
        }

        info!("fsyncdir: root — flushing all write buffers and metadata queue");

        let write_buffers = self.write_buffers.clone();
        let flush_in_flight = self.flush_in_flight.clone();
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let release_in_flight = self.release_in_flight.clone();
        let written_inodes = self.written_inodes.clone();

        let flush_handle = FlushHandle {
            client: client.clone(),
            write_buffers: write_buffers.clone(),
            metadata_cache: metadata_cache.clone(),
            flush_in_flight: flush_in_flight.clone(),
            last_metadata_update: self.last_metadata_update.clone(),
            last_bg_metadata_push: self.flush_handle.last_bg_metadata_push.clone(),
            dir_cache: self.dir_cache.clone(),
            dir_cache_invalidated_at: self.dir_cache_invalidated_at.clone(),
            path_to_inode: self.path_to_inode.clone(),
            inode_to_path: self.inode_to_path.clone(),
            truncated_inodes: self.truncated_inodes.clone(),
            explicit_mtime_pending: self.explicit_mtime_pending.clone(),
            flush_runtime: self.flush_runtime.clone(),
            global_buffered_bytes: self.global_buffered_bytes.clone(),
            flush_notify: self.flush_notify.clone(),
            write_tasks_in_flight: self.write_tasks_in_flight.clone(),
            chunk_write_locks: self.chunk_write_locks.clone(),
            flush_pipeline_locks: self.flush_handle.flush_pipeline_locks.clone(),
            use_dual_rf: false,
            write_open_counts: self.write_open_counts.clone(),
            patch_prefetch_hints: Arc::new(std::sync::Mutex::new(Arc::new(HashMap::new()))),
        };

        // Spawn so the FUSE dispatch thread is freed immediately — same pattern as fsync().
        self.runtime.spawn(async move {
            // Wait for any in-flight release() flush tasks to complete.
            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
            loop {
                let total: usize = release_in_flight.iter()
                    .map(|e| e.value().load(std::sync::atomic::Ordering::Relaxed))
                    .sum();
                if total == 0 { break; }
                if tokio::time::Instant::now() > deadline {
                    warn!("fsyncdir: timed out waiting for release tasks");
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }

            // Flush all dirty write buffers.
            let inodes: Vec<u64> = write_buffers.iter().map(|e| *e.key()).collect();
            if !inodes.is_empty() {
                let handles: Vec<_> = inodes.into_iter().map(|i| {
                    let h = flush_handle.clone();
                    let rt = h.flush_runtime.clone();
                    rt.spawn(async move {
                        if let Err(e) = h.flush_all_pipelined(i).await {
                            error!("fsyncdir: flush failed for inode {}: {}", i, e);
                        }
                    })
                }).collect();
                for h in handles { let _ = h.await; }
            }

            // Wait for any background in-flight flushes to drain.
            // Clone the Arc before the await loop so the RwLockReadGuard is dropped
            // immediately — RwLockReadGuard is not Send and can't be held across await.
            let in_flight_opt = flush_in_flight.read().unwrap().as_ref().cloned();
            if let Some(in_flight) = in_flight_opt {
                let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
                while !in_flight.is_empty() {
                    if tokio::time::Instant::now() > deadline {
                        warn!("fsyncdir: timed out waiting for in-flight flushes");
                        break;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
            }

            // Commit metadata only for inodes actually written this session — not every
            // file this client has ever cached metadata for. Without the written_inodes
            // filter, this scanned and sequentially flushed the entire metadata_cache on
            // every `sync`, which scales with total cached-file count rather than dirty
            // files (the same shape as the shutdown-drain filter at line ~581). Spawn one
            // task per inode like the buffer-flush loop above, instead of a sequential
            // RPC-per-file loop.
            let to_commit: Vec<(u64, FileMetadata)> = metadata_cache.iter()
                .filter(|e| !e.chunk_locations.is_empty() && written_inodes.contains(e.key()))
                .map(|e| (*e.key(), e.clone()))
                .collect();
            if !to_commit.is_empty() {
                let handles: Vec<_> = to_commit.into_iter().map(|(commit_ino, meta)| {
                    let c = client.clone();
                    let written_inodes = written_inodes.clone();
                    tokio::spawn(async move {
                        c.flush_metadata_sync(&meta).await;
                        // Mark clean now that this snapshot is durably committed —
                        // written_inodes was previously insert-only (populated at every
                        // write/create, never removed), so every single `sync $MOUNT`
                        // re-committed full metadata (the complete, ever-growing
                        // chunk_locations list) for every file this client process had
                        // EVER written to, not just what's dirty since the last sync.
                        // A confirmed patch-timing test showed this scaling directly
                        // with total historical write volume: a freshly-restarted client
                        // (empty written_inodes) patched an 8GB file ~2.9x faster than
                        // the same client after a long session of unrelated writes to
                        // other files. flush_metadata_sync retries internally until the
                        // leader acks (see its own doc comment), so reaching this point
                        // means the commit genuinely succeeded. A write that races in
                        // concurrently re-inserts (insert is idempotent) and is simply
                        // picked up by the next sync — same best-effort semantics as
                        // sync() racing any concurrent write elsewhere.
                        written_inodes.remove(&commit_ino);
                    })
                }).collect();
                for h in handles { let _ = h.await; }
            }

            info!("fsyncdir: all buffers flushed and metadata committed");
            reply.ok();
        });
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

    fn poll(
        &mut self,
        _req: &FuseRequest,
        _ino: u64,
        _fh: u64,
        _ph: fuser::PollHandle,
        _events: u32,
        _flags: u32,
        reply: fuser::ReplyPoll,
    ) {
        // Regular files are always ready for both reads and writes.
        // Returning POLLIN|POLLOUT avoids ENOSYS, which breaks QEMU's async
        // I/O completion notification (FUSE_POLL_SCHEDULE_NOTIFY) and causes
        // it to fall back to periodic polling — introducing timing gaps that
        // manifest as transient errors after partition table writes.
        reply.poll(libc::POLLIN as u32 | libc::POLLOUT as u32);
    }

    fn ioctl(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        _flags: u32,
        cmd: u32,
        _in_data: &[u8],
        _out_size: u32,
        reply: fuser::ReplyIoctl,
    ) {
        // Block device ioctl commands that qemu-nbd and mkfs use
        const BLKFLSBUF: u32 = 0x1268;      // Flush buffer cache
        const BLKDISCARD: u32 = 0x80041272; // Discard/TRIM blocks
        const BLKROGET: u32 = 0x125e;       // Get read-only status
        const BLKGETSIZE64: u32 = 0x80081272; // Get device size in bytes
        const BLKZEROOUT: u32 = 0x800c581e; // Zero out a range (mkswap, parted)
        const BLKSECDISCARD: u32 = 0x801c581f; // Secure discard (mkswap, cryptsetup)

        match cmd {
            BLKFLSBUF => {
                // qemu-nbd/mkfs.ext4 calls BLKFLSBUF to flush pending writes.
                // Forward to fsync to ensure data reaches the server.
                info!("ioctl: BLKFLSBUF for ino={} — forwarding to fsync", ino);
                let runtime = self.runtime.clone();
                let flush_handle = self.flush_handle.clone();
                let metadata_cache = self.metadata_cache.clone();
                let client = self.client.clone();

                runtime.spawn(async move {
                    // Flush any buffered writes for this inode.
                    // This ensures all pending writes reach the server before returning.
                    // qemu-nbd and mkfs.ext4 rely on this for data durability.
                    // Use flush_all_pipelined (same path as release()/fsync) so chunk_cache,
                    // byte_range_cache and zero_gap_table stay coherent — the legacy
                    // flush_buffer_async path does not invalidate/reseed those caches,
                    // which left stale byte-range entries readable after a patch/rewrite.
                    if let Err(e) = flush_handle.flush_all_pipelined(ino).await {
                        error!("ioctl BLKFLSBUF: flush failed for ino={}: {}", ino, e);
                        reply.error(libc::EIO);
                        return;
                    }
                    if let Some(meta) = metadata_cache.get(&ino).map(|m| m.clone()) {
                        // Same untrustworthy-snapshot guard as release()/fsync() — see the
                        // 2026-07-03 dvr.conf incident comment there for why this matters.
                        if !meta.chunk_locations.is_empty() || meta.size == 0 {
                            client.flush_metadata_sync(&meta).await;
                        }
                    }

                    reply.ioctl(0, &[]);
                });
            }
            BLKDISCARD => {
                // TRIM/discard operation — qcow2 uses this for hole punching.
                // We don't support sparse holes yet, so just acknowledge success.
                let (from, len) = if _in_data.len() >= 16 {
                    let from = u64::from_ne_bytes(_in_data[0..8].try_into().unwrap());
                    let len  = u64::from_ne_bytes(_in_data[8..16].try_into().unwrap());
                    (from, len)
                } else {
                    (0u64, 0u64)
                };
                info!("ioctl: BLKDISCARD for ino={} from={} len={} (in_data={}B) — no-op (not implemented)", ino, from, len, _in_data.len());
                reply.ioctl(0, &[]);
            }
            BLKZEROOUT => {
                // Zero out a block range — used by parted/sgdisk before writing a new
                // partition table.
                //
                // This ioctl (0x800C581E) is direction=_IOC_READ, size=12: the caller
                // expects 12 bytes of output. We acknowledge success and return zeros.
                //
                // We deliberately do NOT write zeros into the write buffer or flush.
                // Previous implementations did this, but caused partition table loss:
                // Proxmox/QEMU calls BLKZEROOUT multiple times during disk management,
                // including AFTER fdisk has written the partition table. Writing zeros
                // to the buffer at that point erases committed user data. The callers
                // (parted, sgdisk, fdisk) write the actual partition table immediately
                // after BLKZEROOUT anyway, so zeroing the buffer is unnecessary — the
                // real write always follows and overwrites whatever was there.
                let (from, zero_len) = if _in_data.len() >= 16 {
                    let from = u64::from_ne_bytes(_in_data[0..8].try_into().unwrap());
                    let len  = u64::from_ne_bytes(_in_data[8..16].try_into().unwrap());
                    (from, len)
                } else if _in_data.len() >= 12 {
                    let from = u64::from_ne_bytes(_in_data[0..8].try_into().unwrap());
                    let len  = u32::from_ne_bytes(_in_data[8..12].try_into().unwrap()) as u64 * 512;
                    (from, len)
                } else {
                    (0u64, 0u64)
                };
                let has_write_handle = self.write_open_counts.get(&ino).map(|c| *c > 0).unwrap_or(false);
                info!("ioctl: BLKZEROOUT for ino={} from={} len={} (in_data={}B, write_handle={}) — acknowledged, no buffer write",
                      ino, from, zero_len, _in_data.len(), has_write_handle);
                let out = vec![0u8; _out_size as usize];
                reply.ioctl(0, &out);
            }
            BLKSECDISCARD => {
                // Secure discard — used by mkswap, cryptsetup to securely erase data.
                // Similar to BLKDISCARD but with security guarantees. For a distributed
                // filesystem, we don't support secure erase yet, so just acknowledge success.
                let (from, len) = if _in_data.len() >= 16 {
                    let from = u64::from_ne_bytes(_in_data[0..8].try_into().unwrap());
                    let len  = u64::from_ne_bytes(_in_data[8..16].try_into().unwrap());
                    (from, len)
                } else {
                    (0u64, 0u64)
                };
                info!("ioctl: BLKSECDISCARD for ino={} from={} len={} (in_data={}B) — no-op (not implemented)", ino, from, len, _in_data.len());
                reply.ioctl(0, &[]);
            }
            BLKROGET => {
                // Read-only status check — return 0 (read-write)
                debug!("ioctl: BLKROGET for ino={} — returning read-write (0)", ino);
                let data = 0u32.to_ne_bytes();
                reply.ioctl(0, &data);
            }
            BLKGETSIZE64 => {
                // Get device size in bytes — return file size
                if let Some(meta) = self.metadata_cache.get(&ino) {
                    let size = meta.size;
                    drop(meta);
                    info!("ioctl: BLKGETSIZE64 for ino={} — returning size={} (_out_size={})", ino, size, _out_size);
                    let data = size.to_ne_bytes();
                    reply.ioctl(0, &data);
                } else {
                    warn!("ioctl: BLKGETSIZE64 for ino={} — metadata not found", ino);
                    reply.error(libc::ENOENT);
                }
            }
            _ => {
                warn!("ioctl: unhandled cmd=0x{:x} for ino={}", cmd, ino);
                reply.error(libc::ENOTTY);
            }
        }
    }
}
