use anyhow::{Context, Result};
use dashmap::DashMap;
use dfs_common::{ChunkId, FileId};
use lru::LruCache;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Per-process counter for unique temp file names — avoids races when
/// two concurrent writes target the same chunk on the same node.
static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Increment and return the global write counter — used by callers (e.g. PatchChunk handler)
/// that need a unique temp file name in the same directory as a chunk.
pub fn next_write_seq() -> u64 {
    WRITE_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Group-commit for chunk-data durability, enabled by `DFS_DURABILITY_COALESCE`.
///
/// Background (2026-07-19, VM-111 install profiling): the storage NVMe on the
/// gluster nodes has a volatile write cache with no power-loss protection, so a
/// single `fdatasync`/`syncfs` costs ~21ms (a full drive cache-flush to NAND) —
/// measured identically on a busy replica and an idle node, i.e. it's the raw
/// device floor, ~48 durable-flushes/sec. Every chunk write did its *own*
/// `File::sync_all` (write_chunk) or `sync_data` (patch-delta append), so a
/// fsync-heavy workload (a Debian install issues ~45 guest fsync/sec, fully
/// serialized) plus the node's concurrent healing / fold / replica / metadata
/// durability all issued *independent* flushes and saturated that ~48/sec budget,
/// inflating client-observed flush latency 2-3x under load.
///
/// The physical fact this exploits: one `syncfs()` makes *all* dirty data on the
/// filesystem durable at once — so issuing more than one flush per ~21ms window
/// on the same device is pure waste. This coalescer routes every durability
/// consumer through a single per-device flush worker: a lone waiter flushes
/// immediately (no added latency vs. a direct fsync), and under load one `syncfs`
/// satisfies the whole accumulated batch. Durability is identical-or-stronger —
/// `syncfs` also persists the chunk `rename` (directory entry), which the old
/// per-file `sync_all`/`sync_data`-then-rename left in the page cache.
///
/// Mirrors the existing metadata group-committer (see metadata.rs
/// commit_worker_loop): a blocking mpsc of per-waiter reply channels, drained and
/// batched by one worker. Kept blocking (a dedicated OS thread, std mpsc) rather
/// than async because every call site is already inside `spawn_blocking` doing
/// synchronous file I/O.
///
/// Two durability classes (2026-07-19 refinement — see DurabilityClass). Both
/// block for a real durability confirmation (Ok/Err from the actual syncfs); the
/// class controls only *when* the worker flushes:
/// * `Foreground` (client-facing writes) flush *immediately* — a lone client
///   flush never waits for a car-pool, because the workload that produces them
///   (a serial guest install) issues write N+1 only after N is durable, so
///   there's never a second foreground waiter to batch with anyway.
/// * `Background` (healer repair writes) hold up to `linger` to gather a batch of
///   other background writers into one shared syncfs, but any foreground arrival
///   "calls the bus" and flushes everyone at once. The caller still blocks until
///   that syncfs confirms — so a failed flush is surfaced and the heal retried,
///   never silently dropped. This costs only a bounded background worker thread,
///   *not* a network connection: the healer's peer fetch completes and returns
///   its connection to the pool before the write begins (see pull_chunk_from_peers),
///   so blocking here holds nothing that could cause network congestion. Because
///   `syncfs` persists the whole filesystem, a foreground flush also makes pending
///   background writes durable for free — heal work car-pools onto client flushes
///   under load, and the linger only actually elapses when the cluster is idle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurabilityClass {
    /// Client-facing write: flush immediately, block until durable.
    Foreground,
    /// Background repair (healer): linger to batch, block until durable.
    Background,
}

/// One durability request handed to the coalescer worker. Both classes block on
/// `reply` for a real durability confirmation; `class` controls only *when* the
/// worker flushes (Foreground immediately, Background after a short linger to
/// batch with other background writers).
struct FlushReq {
    class: DurabilityClass,
    reply: mpsc::Sender<Result<(), String>>,
}

pub struct DurabilityCoalescer {
    tx: mpsc::Sender<FlushReq>,
}

impl DurabilityCoalescer {
    /// Open a long-lived fd on `data_dir` (any fd on the target filesystem works
    /// for `syncfs`) and spawn the flush worker.
    pub fn new(data_dir: &Path) -> std::io::Result<Arc<Self>> {
        let dir = fs::File::open(data_dir)?;
        let (tx, rx) = mpsc::channel::<FlushReq>();
        // How long a *background-only* batch waits to gather more writers before
        // flushing. Only ever applies when no foreground write is present (a
        // foreground arrival flushes immediately), and never delays the caller —
        // background writes are fire-and-forget. So this bounds only how stale a
        // healer write can be before its syncfs, when the cluster is otherwise
        // idle; under real load, foreground flushes sweep background work far
        // sooner. Tunable via DFS_DURABILITY_LINGER_MS.
        let linger = Duration::from_millis(
            std::env::var("DFS_DURABILITY_LINGER_MS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(1000),
        );
        std::thread::Builder::new()
            .name("durability-coalescer".into())
            .spawn(move || {
                let fd = dir.as_raw_fd(); // `dir` is kept alive by this closure for the fd's lifetime
                let mut stat_batches: u64 = 0;
                let mut stat_fg: u64 = 0;
                let mut stat_bg: u64 = 0;
                let mut stat_total_ms: f64 = 0.0;
                let mut stat_max_ms: f64 = 0.0;
                let mut last_log = Instant::now();
                loop {
                    // Block until at least one request arrives.
                    let first = match rx.recv() {
                        Ok(r) => r,
                        Err(_) => break, // all senders dropped — shutting down
                    };
                    let mut had_foreground = first.class == DurabilityClass::Foreground;
                    let mut batch = vec![first];

                    // Foreground "calls the bus" — flush immediately. A background-only
                    // batch instead lingers up to `linger` to gather a car-pool, but a
                    // foreground arrival during that window ends the wait at once (and
                    // rides the same syncfs). No caller is blocked by this wait: the
                    // foreground writer that would block hasn't arrived, and background
                    // writers are fire-and-forget.
                    if !had_foreground {
                        let deadline = Instant::now() + linger;
                        loop {
                            let now = Instant::now();
                            if now >= deadline {
                                break;
                            }
                            match rx.recv_timeout(deadline - now) {
                                Ok(r) => {
                                    let is_fg = r.class == DurabilityClass::Foreground;
                                    batch.push(r);
                                    if is_fg {
                                        had_foreground = true;
                                        break;
                                    }
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => break,
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                    }

                    // Sweep anything else already queued into the same flush.
                    while let Ok(r) = rx.try_recv() {
                        batch.push(r);
                    }

                    let flush_start = Instant::now();
                    let rc = unsafe { libc::syncfs(fd) };
                    let ms = flush_start.elapsed().as_secs_f64() * 1000.0;
                    let result = if rc == 0 {
                        Ok(())
                    } else {
                        Err(format!("syncfs failed: {}", std::io::Error::last_os_error()))
                    };
                    let (mut fg, mut bg) = (0u64, 0u64);
                    for req in batch {
                        match req.class {
                            DurabilityClass::Foreground => fg += 1,
                            DurabilityClass::Background => bg += 1,
                        }
                        let _ = req.reply.send(result.clone());
                    }
                    stat_batches += 1;
                    stat_fg += fg;
                    stat_bg += bg;
                    stat_total_ms += ms;
                    stat_max_ms = stat_max_ms.max(ms);
                    // Period-based summary (not per-batch — a saturated worker runs
                    // ~48 batches/sec), same discipline as [META COMMITTER]. fg/bg split
                    // shows whether coalescing is riding foreground flushes (fg-heavy) or
                    // batching idle heal work (bg-heavy).
                    if last_log.elapsed().as_secs() >= 5 {
                        let waiters = stat_fg + stat_bg;
                        info!(
                            "[DURABILITY] syncfs_batches={} waiters={} fg={} bg={} avg_batch={:.1} avg_syncfs_ms={:.1} max_syncfs_ms={:.1}",
                            stat_batches, waiters, stat_fg, stat_bg,
                            waiters as f64 / stat_batches as f64,
                            stat_total_ms / stat_batches as f64,
                            stat_max_ms,
                        );
                        stat_batches = 0;
                        stat_fg = 0;
                        stat_bg = 0;
                        stat_total_ms = 0.0;
                        stat_max_ms = 0.0;
                        last_log = Instant::now();
                    }
                }
            })?;
        Ok(Arc::new(Self { tx }))
    }

    /// Make the caller's already-written data durable on stable storage and block
    /// until the shared syncfs confirms it. Must be called *after* the file
    /// write/append (and rename) so the pages are dirty before the worker's syncfs
    /// runs. Both classes get a real Ok/Err — `class` only changes flush *timing*
    /// (see DurabilityClass): Foreground flushes now; Background lingers briefly to
    /// batch with peers, then both block on the same confirmed flush.
    pub fn sync_durable(&self, class: DurabilityClass) -> Result<(), String> {
        let (dtx, drx) = mpsc::channel();
        self.tx
            .send(FlushReq { class, reply: dtx })
            .map_err(|_| "durability coalescer worker gone".to_string())?;
        drx.recv()
            .map_err(|_| "durability coalescer dropped reply".to_string())?
    }
}

/// Local chunk storage manager
/// Optimized for SBC environments (limited CPU/RAM)
pub struct ChunkStorage {
    /// Root directory for chunk storage
    data_dir: PathBuf,

    /// LRU cache for frequently accessed chunks
    /// Sized based on available RAM (25-50% allocation)
    /// Cache stores Arc<Vec<u8>> to allow cheap cloning for concurrent readers
    cache: Arc<Mutex<LruCache<ChunkId, Arc<Vec<u8>>>>>,

    /// Cache configuration
    cache_capacity_chunks: usize,

    /// Live-maintained index for list_chunks() — see that method's doc comment.
    /// None until the first list_chunks() call populates it via a real directory
    /// walk; Some(set) thereafter, kept correct incrementally by write_chunk /
    /// delete_chunk rather than ever being invalidated wholesale.
    list_chunks_cache: Mutex<Option<std::collections::BTreeSet<ChunkId>>>,

    /// Present only when `DFS_DURABILITY_COALESCE` is set: routes every durable
    /// chunk write through one per-device syncfs worker instead of a per-write
    /// fsync. See DurabilityCoalescer. None = legacy per-file sync behavior.
    coalescer: Option<Arc<DurabilityCoalescer>>,

    /// Cumulative-since-startup delete_chunk() call counts by `reason` tag —
    /// added 2026-08-06 alongside RpcClassCounts (stats.rs) for the same
    /// operational-visibility ask, reusing the reason tags delete_chunk
    /// already carries rather than adding new instrumentation. In-memory
    /// only, not durable — see delete_chunk's own doc comment for why these
    /// tags exist in the first place.
    delete_reason_counts: DashMap<String, AtomicU64>,
}

impl ChunkStorage {
    /// Create a new chunk storage instance with auto-sized cache
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        // Create data directory if it doesn't exist
        fs::create_dir_all(&data_dir)
            .with_context(|| format!("Failed to create data directory: {:?}", data_dir))?;

        // Auto-size cache based on available RAM
        let cache_capacity_chunks = Self::calculate_cache_size();
        let cache = Arc::new(Mutex::new(
            LruCache::new(NonZeroUsize::new(cache_capacity_chunks).unwrap())
        ));

        info!(
            "Initialized chunk storage at {:?} with {}MB cache ({} chunks)",
            data_dir,
            cache_capacity_chunks * 4,  // 4MB per chunk
            cache_capacity_chunks
        );

        // Opt-in group-commit for chunk durability. Falls back to legacy per-file
        // sync if the flag is unset or the coalescer can't open the data dir.
        let coalescer = if std::env::var("DFS_DURABILITY_COALESCE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            match DurabilityCoalescer::new(&data_dir) {
                Ok(c) => {
                    info!("Durability coalescing ENABLED (per-device syncfs group-commit)");
                    Some(c)
                }
                Err(e) => {
                    warn!("DFS_DURABILITY_COALESCE set but coalescer init failed ({}); using per-file sync", e);
                    None
                }
            }
        } else {
            None
        };

        Ok(Self {
            data_dir,
            cache,
            cache_capacity_chunks,
            list_chunks_cache: Mutex::new(None),
            coalescer,
            delete_reason_counts: DashMap::new(),
        })
    }

    /// Snapshot of delete_chunk() call counts by reason tag, for
    /// Response::RpcClassCounts. See delete_reason_counts' doc comment.
    pub fn delete_reason_counts_snapshot(&self) -> Vec<(String, u64)> {
        self.delete_reason_counts
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect()
    }

    /// True when chunk writes route durability through the shared syncfs worker
    /// (see DurabilityCoalescer) rather than doing their own per-file fsync.
    pub fn coalescing_enabled(&self) -> bool {
        self.coalescer.is_some()
    }

    /// Make all pending chunk writes on this device durable via the shared syncfs
    /// worker. Callers that wrote+renamed a chunk file *outside* write_chunk (e.g.
    /// the patch-delta append path) call this in place of their own fsync. A no-op
    /// error-free return when coalescing is disabled — those callers keep doing
    /// their own sync in that case, so this should only be reached when enabled.
    pub fn sync_durable(&self, class: DurabilityClass) -> Result<()> {
        match &self.coalescer {
            Some(c) => c.sync_durable(class).map_err(|e| anyhow::anyhow!(e)),
            None => Ok(()),
        }
    }

    /// Calculate optimal cache size based on the shared server cache budget.
    ///
    /// The server-side chunk cache is used primarily for healing reads (a chunk
    /// that was just written is likely to be re-read by the healer shortly after).
    /// The client maintains its own much larger LRU for read serving, so the server
    /// cache does not need to be large.
    ///
    /// Takes 50% of `dfs_common::calculate_server_cache_budget_mb()` — chunk_ring
    /// and delta_ring (server.rs's `calculate_ring_capacity`) split the other 50%
    /// between them. See that budget function's doc comment for why this is now a
    /// shared pool rather than its own independent RAM tier: this cache, chunk_ring,
    /// and delta_ring used to each pick a tier off *total* RAM with nothing enforcing
    /// a combined ceiling, which is how a 3.8GB gluster node ended up committing
    /// ~1GB (27%) to caches before any real workload data existed (2026-07-19
    /// near-OOM investigation — gluster1 plateaued at ~77MB `MemAvailable`).
    ///
    /// `DFS_CHUNK_CACHE_CAPACITY_MB` overrides this cache's share directly (same
    /// pattern as `DFS_CHUNK_RING_CAPACITY`/`DFS_DELTA_RING_CAPACITY` for the other
    /// two — see their call sites in server.rs).
    fn calculate_cache_size() -> usize {
        const CHUNK_SIZE_MB: u64 = 4;

        let cache_mb: u64 = std::env::var("DFS_CHUNK_CACHE_CAPACITY_MB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or_else(|| dfs_common::calculate_server_cache_budget_mb() / 2);

        let final_cache = (cache_mb / CHUNK_SIZE_MB).max(1) as usize;

        info!(
            "Cache sizing: chunk_cache={}MB ({} chunks)",
            cache_mb, final_cache,
        );

        final_cache
    }

    /// Write a chunk to local storage. Foreground durability (client-facing):
    /// blocks until the write is durable. Background repair callers (the healer)
    /// use `write_chunk_prio(..., DurabilityClass::Background)` instead.
    pub fn write_chunk(&self, chunk_id: &ChunkId, data: &[u8]) -> Result<()> {
        self.write_chunk_prio(chunk_id, data, DurabilityClass::Foreground)
    }

    /// Write a chunk with an explicit durability class. See DurabilityClass: both
    /// block until the write is confirmed durable; `Background` first lingers to
    /// batch its syncfs with other background writers (and never holds a network
    /// connection while doing so — the healer's fetch is already done by then).
    pub fn write_chunk_prio(&self, chunk_id: &ChunkId, data: &[u8], class: DurabilityClass) -> Result<()> {
        let start = std::time::Instant::now();
        // Note: chunk_id.hash is now compute_chunk_hash_at(data, file_offset) — a
        // position-aware hash — so we cannot re-derive it here without knowing the
        // file offset. The hash serves as a unique key; integrity is guaranteed by
        // the fact that the client computes the ID from the same data it sends.
        let checksum_time = std::time::Duration::ZERO;

        let path = self.get_chunk_path(chunk_id);

        // Create parent directories
        let mkdir_start = std::time::Instant::now();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create chunk directory: {:?}", parent))?;
        }
        let mkdir_time = mkdir_start.elapsed();

        // Write data atomically using a unique temporary file.
        // Using a counter-suffixed name prevents two concurrent writes to the
        // same chunk (e.g. parallel dual-replica flush) from clobbering each
        // other's temp file before rename.
        let write_start = std::time::Instant::now();
        let seq = WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_name = format!("{}.{}.tmp", path.file_name().unwrap_or_default().to_string_lossy(), seq);
        let temp_path = path.parent().unwrap_or(path.as_path()).join(temp_name);
        let mut file = fs::File::create(&temp_path)
            .with_context(|| format!("Failed to create temporary file: {:?}", temp_path))?;

        file.write_all(data)
            .context("Failed to write chunk data")?;

        let sync_start = std::time::Instant::now();
        let (sync_time, rename_time) = match &self.coalescer {
            // Legacy path: sync this file's data, then rename. Unchanged behavior.
            None => {
                file.sync_all().context("Failed to sync chunk data")?;
                let sync_time = sync_start.elapsed();
                let rename_start = std::time::Instant::now();
                fs::rename(&temp_path, &path)
                    .with_context(|| format!("Failed to rename chunk file: {:?}", path))?;
                (sync_time, rename_start.elapsed())
            }
            // Coalesced path: rename first, then a single shared syncfs makes both
            // the data and the rename durable together (see DurabilityCoalescer).
            // We don't return to the caller as "durable" until that flush completes.
            Some(coalescer) => {
                drop(file);
                let rename_start = std::time::Instant::now();
                fs::rename(&temp_path, &path)
                    .with_context(|| format!("Failed to rename chunk file: {:?}", path))?;
                let rename_time = rename_start.elapsed();
                let flush_start = std::time::Instant::now();
                coalescer.sync_durable(class).map_err(|e| anyhow::anyhow!(e))
                    .context("Failed to sync chunk data (coalesced)")?;
                (flush_start.elapsed(), rename_time)
            }
        };

        let total_time = start.elapsed();
        debug!("Wrote chunk {} ({} bytes) in {:?} - checksum: {:?}, mkdir: {:?}, sync: {:?}, rename: {:?}",
               chunk_id, data.len(), total_time, checksum_time, mkdir_time, sync_time, rename_time);

        // Keep list_chunks()'s live index correct — see that method's doc comment.
        // A no-op if the index hasn't been built yet (None); once it exists, this
        // chunk must be visible to the very next list_chunks() call (e.g. a peer's
        // HasChunks check right after this write completes as part of a heal), so
        // it's inserted directly rather than invalidating the whole index (which,
        // under sustained write load, would defeat it almost as fast as it's built).
        if let Some(index) = self.list_chunks_cache.lock().unwrap().as_mut() {
            index.insert(*chunk_id);
        }

        Ok(())
    }

    /// Read a chunk, returning a shared Arc — zero copy on cache hit, one copy on miss.
    /// Use this instead of read_chunk() whenever the data will be sent to a network socket,
    /// since the Arc can be passed directly to write_all without an intermediate clone.
    pub fn read_chunk_arc(&self, chunk_id: &ChunkId) -> Result<Arc<Vec<u8>>> {
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(cached_data) = cache.get(chunk_id) {
                debug!("Cache HIT for chunk {} ({} bytes)", chunk_id, cached_data.len());
                return Ok(Arc::clone(cached_data));
            }
        }

        debug!("Cache MISS for chunk {}, reading from disk", chunk_id);
        let path = self.get_chunk_path(chunk_id);
        let mut file = fs::File::open(&path)
            .with_context(|| format!("Failed to open chunk file: {:?}", path))?;
        let mut data = Vec::new();
        file.read_to_end(&mut data).context("Failed to read chunk data")?;
        debug!("Read chunk {} from disk ({} bytes)", chunk_id, data.len());

        let data_arc = Arc::new(data);
        {
            let mut cache = self.cache.lock().unwrap();
            cache.put(*chunk_id, Arc::clone(&data_arc));
        }
        Ok(data_arc)
    }

    /// Read a chunk from local storage (cache-through)
    /// Checks cache first, then disk, then populates cache
    /// Does NOT verify checksum (for SBC performance) - use verify_chunk() for scrubbing
    ///
    /// CRITICAL: Never holds cache lock during disk I/O to prevent blocking other reads
    pub fn read_chunk(&self, chunk_id: &ChunkId) -> Result<Vec<u8>> {
        Ok(Arc::try_unwrap(self.read_chunk_arc(chunk_id)?).unwrap_or_else(|arc| (*arc).clone()))
    }

    /// Return the chunk from cache only — no disk read. Returns None on cache miss.
    /// Used by PatchChunk / MultiPatch: if the chunk is warm we RMW; if cold we
    /// start from zeros and let the patches carry all the real content.
    pub fn read_chunk_cached_only(&self, chunk_id: &ChunkId) -> Option<Vec<u8>> {
        let mut cache = self.cache.lock().unwrap();
        cache.get(chunk_id).map(|arc| (**arc).clone())
    }

    /// Read a byte range from a chunk — returns the cached Arc + clamped range so the
    /// caller can write the slice on the wire without ever cloning the bytes.
    /// Returns (arc, start, end) where start..end is the requested sub-range within the chunk.
    pub fn read_chunk_range_arc(&self, chunk_id: &ChunkId, offset: usize, length: usize)
        -> Result<(Arc<Vec<u8>>, usize, usize)>
    {
        let data = self.read_chunk_arc(chunk_id)?;
        if offset >= data.len() {
            return Err(anyhow::anyhow!("Offset {} beyond chunk size {}", offset, data.len()));
        }
        let end = (offset + length).min(data.len());
        Ok((data, offset, end))
    }

    /// Read only the requested byte range from a chunk without loading the full chunk
    /// into memory or the cache. On a cache hit the slice is copied from the warm Arc;
    /// on a cache miss only [offset, offset+length) is read from disk via seek+read_exact,
    /// avoiding 4MB of unnecessary disk I/O when the caller needs only a small region
    /// (e.g., a 32KB random read or one half of a striped sequential read).
    pub fn read_chunk_range_partial(&self, chunk_id: &ChunkId, offset: usize, length: usize)
        -> Result<Vec<u8>>
    {
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(arc) = cache.get(chunk_id) {
                if offset >= arc.len() {
                    return Err(anyhow::anyhow!("Offset {} beyond chunk size {}", offset, arc.len()));
                }
                let end = (offset + length).min(arc.len());
                return Ok(arc[offset..end].to_vec());
            }
        }
        let path = self.get_chunk_path(chunk_id);
        let mut file = fs::File::open(&path)
            .with_context(|| format!("Failed to open chunk file: {:?}", path))?;
        file.seek(SeekFrom::Start(offset as u64))
            .with_context(|| format!("Failed to seek chunk {} to offset {}", chunk_id, offset))?;
        let mut buf = vec![0u8; length];
        let mut total = 0;
        while total < length {
            match file.read(&mut buf[total..])? {
                0 => break,
                n => total += n,
            }
        }
        buf.truncate(total);
        Ok(buf)
    }

    /// Warm the cache with a chunk (prefetch hint handler)
    /// Reads chunk from disk into cache if not already present
    /// Returns true if chunk was loaded, false if already in cache or doesn't exist
    pub fn warm_cache(&self, chunk_id: &ChunkId) -> Result<bool> {
        // Check if already in cache
        {
            let cache = self.cache.lock().unwrap();
            if cache.peek(chunk_id).is_some() {
                debug!("Chunk {} already in cache, skipping warm", chunk_id);
                return Ok(false);
            }
        }

        // Check if chunk exists on disk
        if !self.has_chunk(chunk_id) {
            debug!("Chunk {} not found on disk, cannot warm cache", chunk_id);
            anyhow::bail!("Chunk not found");
        }

        // Read from disk and populate cache
        let path = self.get_chunk_path(chunk_id);
        let mut file = fs::File::open(&path)
            .with_context(|| format!("Failed to open chunk file: {:?}", path))?;

        let mut data = Vec::new();
        file.read_to_end(&mut data)
            .context("Failed to read chunk data")?;

        debug!("Warmed cache with chunk {} ({} bytes)", chunk_id, data.len());

        // Populate cache
        let data_arc = Arc::new(data);
        {
            let mut cache = self.cache.lock().unwrap();
            cache.put(*chunk_id, data_arc);
        }

        Ok(true)
    }

    /// Read a chunk (used during scrubbing or error recovery)
    pub fn read_and_verify_chunk(&self, chunk_id: &ChunkId) -> Result<Vec<u8>> {
        // Note: chunk_id.hash is position-aware (compute_chunk_hash_at) so we cannot
        // re-verify it here without knowing the file offset. Just read and return.
        self.read_chunk(chunk_id)
    }

    /// Verify the on-disk chunk data matches its content-addressed ID.
    /// The hash is file-scoped and position-aware: Blake3(file_id || file_offset || data),
    /// so the caller must supply both the file_offset and file_id from ChunkLocation.
    /// If file_id is None (record predates file-scoped IDs, or was reconstructed
    /// without file context), verification is skipped and this returns true.
    /// Returns false if the file is missing, unreadable, or hash-mismatched.
    pub fn verify_chunk_at(&self, chunk_id: &ChunkId, file_offset: u64, file_id: Option<FileId>) -> bool {
        let file_id = match file_id {
            Some(id) => id,
            None => return true,
        };
        match self.read_chunk(chunk_id) {
            Ok(data) => {
                let expected = dfs_common::compute_chunk_hash_at(&data, file_offset, file_id);
                expected == chunk_id.hash
            }
            Err(_) => false,
        }
    }

    /// Check if a chunk exists in local storage
    pub fn has_chunk(&self, chunk_id: &ChunkId) -> bool {
        let path = self.get_chunk_path(chunk_id);
        path.exists()
    }

    /// Return the physical on-disk size of a chunk, or None if not present.
    pub fn get_chunk_size(&self, chunk_id: &ChunkId) -> Option<u64> {
        let path = self.get_chunk_path(chunk_id);
        fs::metadata(&path).ok().map(|m| m.len())
    }

    /// Return the mtime of a chunk file as Unix seconds, or None if not present.
    pub fn get_chunk_mtime(&self, chunk_id: &ChunkId) -> Option<u64> {
        let path = self.get_chunk_path(chunk_id);
        fs::metadata(&path).ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
    }

    /// Set the mtime of a chunk file to the given Unix timestamp.
    /// Used after replication to preserve the original write time across replicas.
    pub fn set_chunk_mtime(&self, chunk_id: &ChunkId, written_at_secs: u64) {
        let path = self.get_chunk_path(chunk_id);
        let target = std::time::UNIX_EPOCH + std::time::Duration::from_secs(written_at_secs);
        if let Err(e) = fs::File::open(&path).and_then(|f| f.set_modified(target)) {
            tracing::debug!("set_chunk_mtime: failed for {}: {}", chunk_id, e);
        }
    }

    /// Delete a chunk from local storage.
    ///
    /// `reason` identifies which of this codebase's ~9 deletion call sites is doing
    /// the deleting (e.g. "live_file_orphan_sweep", "fold_delta_cleanup") — every one
    /// of them previously funneled into this single function with no way to tell them
    /// apart afterward, since this is the ONLY place that logged the actual file
    /// removal, and only at debug! (invisible at this fleet's info level). Added
    /// 2026-08-04 after a real incident (VM-111 install, chunk baf9c254) took hours to
    /// reconstruct from indirect evidence because nothing recorded which mechanism
    /// physically deleted it.
    ///
    /// YOUNG_CHUNK_WARN_SECS: deleting a chunk this recently written is the specific
    /// blind-spot shape that incident had — a newly-minted, likely-still-needed chunk
    /// getting swept up as an apparent orphan — versus the overwhelmingly common case
    /// of cleaning up something genuinely old and settled. Logged at warn!, at info
    /// level, unconditionally (not gated on any deletion path's own verbosity), so a
    /// future recurrence is visible immediately instead of requiring another multi-
    /// hour reconstruction.
    pub fn delete_chunk(&self, chunk_id: &ChunkId, reason: &str) -> Result<()> {
        self.delete_reason_counts
            .entry(reason.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);

        let path = self.get_chunk_path(chunk_id);

        if path.exists() {
            const YOUNG_CHUNK_WARN_SECS: u64 = 300;
            if let Some(mtime) = self.get_chunk_mtime(chunk_id) {
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let age_secs = now_secs.saturating_sub(mtime);
                if age_secs < YOUNG_CHUNK_WARN_SECS {
                    tracing::warn!(
                        "delete_chunk: deleting YOUNG chunk {} (age={}s, reason={}) — recently-written \
                         data being physically removed; if this chunk was still the live/correct \
                         content for its slot, this is data loss",
                        chunk_id, age_secs, reason,
                    );
                }
            }
            fs::remove_file(&path)
                .with_context(|| format!("Failed to delete chunk file: {:?}", path))?;

            debug!("Deleted chunk {} (reason={})", chunk_id, reason);
        }
        // Invalidate cache regardless of file presence — a previous PatchChunk may have
        // left stale bytes here.
        self.invalidate_cache(chunk_id);
        // Keep list_chunks()'s live index correct — see write_chunk's matching
        // update and list_chunks' doc comment for why this is an incremental
        // remove, not a wholesale invalidation. Also a no-op if the index isn't
        // built yet, and harmless if chunk_id was never in it (path didn't exist).
        if let Some(index) = self.list_chunks_cache.lock().unwrap().as_mut() {
            index.remove(chunk_id);
        }

        Ok(())
    }

    /// Insert already-known content into the cache directly, without touching disk.
    /// Used to cache the result of composing an overlay chain (see
    /// `Server::resolve_chunk_content`) so a subsequent read of the same head
    /// chunk_id doesn't repeat the composition.
    pub fn cache_put(&self, chunk_id: ChunkId, data: Arc<Vec<u8>>) {
        let mut cache = self.cache.lock().unwrap();
        cache.put(chunk_id, data);
    }

    /// Remove a chunk from the in-memory cache. Must be called whenever the on-disk
    /// content for a chunk_id is rewritten (PatchChunk same→same, repair, etc.) or the
    /// chunk is deleted. Without this, read_chunk_arc returns stale bytes and subsequent
    /// PatchChunks read-modify-write the wrong base data.
    pub fn invalidate_cache(&self, chunk_id: &ChunkId) {
        let mut cache = self.cache.lock().unwrap();
        cache.pop(chunk_id);
    }

    /// Get the filesystem path for a chunk
    /// Uses 2-level directory sharding: chunks/XX/YY/hash
    pub fn get_chunk_path(&self, chunk_id: &ChunkId) -> PathBuf {
        let (dir1, dir2, filename) = chunk_id.storage_path_components();
        self.data_dir
            .join("chunks")
            .join(dir1)
            .join(dir2)
            .join(filename)
    }

    /// Get storage statistics (lightweight, doesn't verify checksums)
    pub fn get_stats(&self) -> Result<StorageStats> {
        let mut total_chunks = 0;
        let mut total_bytes = 0;

        let chunks_dir = self.data_dir.join("chunks");
        if chunks_dir.exists() {
            Self::count_chunks_recursive(&chunks_dir, &mut total_chunks, &mut total_bytes)?;
        }

        Ok(StorageStats {
            total_chunks,
            total_bytes,
        })
    }

    /// Recursively count chunks and bytes
    fn count_chunks_recursive(dir: &Path, chunks: &mut usize, bytes: &mut u64) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                Self::count_chunks_recursive(&path, chunks, bytes)?;
            } else if path.is_file() && !path.extension().map_or(false, |ext| ext == "tmp") {
                *chunks += 1;
                if let Ok(metadata) = fs::metadata(&path) {
                    *bytes += metadata.len();
                }
            }
        }
        Ok(())
    }

    /// List all chunk IDs in storage (useful for scrubbing/recovery).
    ///
    /// Root-caused 2026-07-18 via a live gdb thread dump on staging: this used to be a
    /// full recursive directory walk over every locally-stored chunk (60-70K+ files on
    /// a loaded gluster-class node) on EVERY call, and every one of its 6 call sites
    /// (handle_has_chunks — peer-triggered, no rate limit — plus 4 periodic scans in
    /// healing.rs) ran it with no coordination between callers. Under load, that let
    /// 24 of 38 threads on one node pile up simultaneously in getdents64, all
    /// independently re-walking the SAME directory tree at once — a real, measured
    /// disk-I/O storm (the same node that later showed ~99% disk utilization
    /// dominated by reads), not just wasted CPU.
    ///
    /// A first attempt at this fix (same day) cached the result with a short TTL,
    /// invalidated wholesale on every write_chunk/delete_chunk. That was correct but
    /// self-defeating: under sustained write load (a VM restore) writes land far more
    /// often than the TTL, so the cache was invalidated almost as fast as it was
    /// populated — never surviving long enough to help during exactly the load that
    /// motivated the fix.
    ///
    /// Now a live-maintained index instead of a cache: scanned from disk once (lazily,
    /// on first call after startup — this function still blocks concurrent callers
    /// behind that one scan the same way the TTL version did, via the same
    /// std::sync::Mutex), then kept correct incrementally by write_chunk/delete_chunk
    /// inserting/removing exactly the one chunk_id that changed. No TTL, so it's never
    /// stale; no periodic rescans after the first, so none of the 6 call sites ever
    /// pays the full directory-walk cost again for the life of the process.
    pub fn list_chunks(&self) -> Result<Vec<ChunkId>> {
        let mut guard = self.list_chunks_cache.lock().unwrap();
        if let Some(index) = guard.as_ref() {
            return Ok(index.iter().copied().collect());
        }

        let mut chunk_ids = Vec::new();
        let chunks_dir = self.data_dir.join("chunks");
        if chunks_dir.exists() {
            Self::collect_chunks_recursive(&chunks_dir, &mut chunk_ids)?;
        }

        let index: std::collections::BTreeSet<ChunkId> = chunk_ids.iter().copied().collect();
        let result = chunk_ids;
        *guard = Some(index);
        Ok(result)
    }

    /// Return up to `limit` chunk ids strictly greater than `after` (or the first
    /// `limit` if `after` is None), in ascending order, using the same live-maintained
    /// index as list_chunks(). Builds the index via a directory walk on first call,
    /// same as list_chunks(). An empty result means the cursor has reached the end of
    /// the set — callers rotate by passing None again on the next call.
    pub fn list_chunks_page(&self, after: Option<ChunkId>, limit: usize) -> Result<Vec<ChunkId>> {
        let mut guard = self.list_chunks_cache.lock().unwrap();
        if guard.is_none() {
            drop(guard);
            self.list_chunks()?;
            guard = self.list_chunks_cache.lock().unwrap();
        }
        let index = guard.as_ref().expect("index populated above");
        let iter = match after {
            Some(cursor) => index.range((std::ops::Bound::Excluded(cursor), std::ops::Bound::Unbounded)),
            None => index.range(..),
        };
        Ok(iter.take(limit).copied().collect())
    }

    /// Get cache statistics for flow control
    /// Returns (cache_capacity, cache_current_size)
    pub fn get_cache_stats(&self) -> (usize, usize) {
        let cache = self.cache.lock().unwrap();
        (self.cache_capacity_chunks, cache.len())
    }

    /// Get filesystem statistics for the data directory
    /// Returns (total_space, free_space, available_space) in bytes
    pub fn get_filesystem_stats(&self) -> Result<(u64, u64, u64)> {
        use std::os::unix::fs::MetadataExt;

        // Get filesystem stats using statvfs
        let metadata = fs::metadata(&self.data_dir)?;
        let dev = metadata.dev();

        // Use nix crate for statvfs
        use nix::sys::statvfs::statvfs;
        let stat = statvfs(&self.data_dir)?;

        let block_size = stat.block_size();
        let total_space = stat.blocks() * block_size;
        let free_space = stat.blocks_free() * block_size;
        let available_space = stat.blocks_available() * block_size;

        Ok((total_space, free_space, available_space))
    }

    /// Recursively collect chunk IDs
    fn collect_chunks_recursive(dir: &Path, chunk_ids: &mut Vec<ChunkId>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                Self::collect_chunks_recursive(&path, chunk_ids)?;
            } else if path.is_file() && !path.extension().map_or(false, |ext| ext == "tmp") {
                // Extract chunk ID from filename (64 hex chars)
                if let Some(filename) = path.file_name().and_then(|s| s.to_str()) {
                    if filename.len() == 64 {
                        if let Ok(hash_bytes) = hex_to_bytes(filename) {
                            if hash_bytes.len() == 32 {
                                let mut hash = [0u8; 32];
                                hash.copy_from_slice(&hash_bytes);
                                chunk_ids.push(ChunkId::from_hash(hash));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Storage statistics
#[derive(Debug, Clone)]
pub struct StorageStats {
    pub total_chunks: usize,
    pub total_bytes: u64,
}

/// Convert hex string to bytes (lightweight, no external deps)
fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .with_context(|| format!("Invalid hex string: {}", hex))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dfs_common::compute_chunk_hash;
    use tempfile::TempDir;

    #[test]
    fn test_write_and_read_chunk() {
        let temp_dir = TempDir::new().unwrap();
        let storage = ChunkStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let data = b"Hello, DFS!";
        let hash = compute_chunk_hash(data);
        let chunk_id = ChunkId::from_hash(hash);

        // Write chunk
        storage.write_chunk(&chunk_id, data).unwrap();

        // Verify it exists
        assert!(storage.has_chunk(&chunk_id));

        // Read chunk back (no verification)
        let read_data = storage.read_chunk(&chunk_id).unwrap();
        assert_eq!(data, read_data.as_slice());

        // Read with verification
        let verified_data = storage.read_and_verify_chunk(&chunk_id).unwrap();
        assert_eq!(data, verified_data.as_slice());
    }

    #[test]
    fn test_delete_chunk() {
        let temp_dir = TempDir::new().unwrap();
        let storage = ChunkStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let data = b"Test data";
        let hash = compute_chunk_hash(data);
        let chunk_id = ChunkId::from_hash(hash);

        storage.write_chunk(&chunk_id, data).unwrap();
        assert!(storage.has_chunk(&chunk_id));

        storage.delete_chunk(&chunk_id, "test").unwrap();
        assert!(!storage.has_chunk(&chunk_id));
    }

    #[test]
    fn test_storage_stats() {
        let temp_dir = TempDir::new().unwrap();
        let storage = ChunkStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let data1 = b"First chunk";
        let data2 = b"Second chunk";

        let chunk1 = ChunkId::from_hash(compute_chunk_hash(data1));
        let chunk2 = ChunkId::from_hash(compute_chunk_hash(data2));

        storage.write_chunk(&chunk1, data1).unwrap();
        storage.write_chunk(&chunk2, data2).unwrap();

        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_chunks, 2);
        assert_eq!(stats.total_bytes, (data1.len() + data2.len()) as u64);
    }

    #[test]
    fn test_list_chunks() {
        let temp_dir = TempDir::new().unwrap();
        let storage = ChunkStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let data1 = b"Chunk 1";
        let data2 = b"Chunk 2";

        let chunk1 = ChunkId::from_hash(compute_chunk_hash(data1));
        let chunk2 = ChunkId::from_hash(compute_chunk_hash(data2));

        storage.write_chunk(&chunk1, data1).unwrap();
        storage.write_chunk(&chunk2, data2).unwrap();

        let chunks = storage.list_chunks().unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(chunks.contains(&chunk1));
        assert!(chunks.contains(&chunk2));
    }

    #[test]
    fn test_list_chunks_page_full_rotation_no_dupes_no_gaps() {
        let temp_dir = TempDir::new().unwrap();
        let storage = ChunkStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let mut expected = std::collections::BTreeSet::new();
        for i in 0..23u32 {
            let data = format!("chunk-{}", i).into_bytes();
            let chunk_id = ChunkId::from_hash(compute_chunk_hash(&data));
            storage.write_chunk(&chunk_id, &data).unwrap();
            expected.insert(chunk_id);
        }

        // Walk the whole set in pages of 5 (23 chunks -> 4 full pages + 1 partial).
        let mut collected = Vec::new();
        let mut cursor = None;
        loop {
            let page = storage.list_chunks_page(cursor, 5).unwrap();
            if page.is_empty() {
                break;
            }
            cursor = Some(*page.last().unwrap());
            collected.extend(page);
        }

        assert_eq!(collected.len(), expected.len(), "page walk should visit every chunk exactly once");
        let collected_set: std::collections::BTreeSet<_> = collected.iter().copied().collect();
        assert_eq!(collected_set.len(), collected.len(), "no chunk should be paged twice in one rotation");
        assert_eq!(collected_set, expected, "page walk should cover exactly the written set, no gaps");

        // Ascending order across page boundaries, not just within a page.
        for w in collected.windows(2) {
            assert!(w[0] < w[1], "pages must be strictly ascending across the whole rotation");
        }
    }

    #[test]
    fn test_list_chunks_page_mid_rotation_insert_picked_up_next_rotation() {
        let temp_dir = TempDir::new().unwrap();
        let storage = ChunkStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let mut chunk_ids = Vec::new();
        for i in 0..4u32 {
            let data = format!("initial-{}", i).into_bytes();
            let chunk_id = ChunkId::from_hash(compute_chunk_hash(&data));
            storage.write_chunk(&chunk_id, &data).unwrap();
            chunk_ids.push(chunk_id);
        }
        chunk_ids.sort();

        // Page halfway through the first rotation.
        let first_page = storage.list_chunks_page(None, 2).unwrap();
        assert_eq!(first_page.len(), 2);

        // Insert a new chunk mid-rotation (after the index has already been built).
        let new_data = b"inserted-mid-rotation";
        let new_chunk = ChunkId::from_hash(compute_chunk_hash(new_data));
        storage.write_chunk(&new_chunk, new_data).unwrap();

        // Finish out this rotation: the new chunk must NOT be skipped forever, but it's
        // fine if it doesn't appear until the cursor wraps back around.
        let mut cursor = Some(*first_page.last().unwrap());
        let mut rest_of_rotation = Vec::new();
        loop {
            let page = storage.list_chunks_page(cursor, 2).unwrap();
            if page.is_empty() {
                break;
            }
            cursor = Some(*page.last().unwrap());
            rest_of_rotation.extend(page);
        }

        // Next rotation (cursor reset to None) must include the mid-rotation insert.
        let mut next_rotation = Vec::new();
        let mut cursor = None;
        loop {
            let page = storage.list_chunks_page(cursor, 2).unwrap();
            if page.is_empty() {
                break;
            }
            cursor = Some(*page.last().unwrap());
            next_rotation.extend(page);
        }
        assert!(next_rotation.contains(&new_chunk), "chunk inserted mid-rotation must appear on the next rotation");
        assert_eq!(next_rotation.len(), 5, "next rotation should see all 5 chunks now on disk");
    }

    #[test]
    fn test_checksum_verification_on_write() {
        let temp_dir = TempDir::new().unwrap();
        let storage = ChunkStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let data = b"Test data";
        let wrong_hash = [0u8; 32]; // Wrong hash
        let chunk_id = ChunkId::from_hash(wrong_hash);

        // Should fail because checksum doesn't match
        let result = storage.write_chunk(&chunk_id, data);
        assert!(result.is_err());
    }
}
