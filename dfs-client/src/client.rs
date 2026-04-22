use anyhow::{Context, Result};
use blake3;
use dashmap::DashMap;
use dfs_common::{ChunkId, ErrorCode, FileId, FileMetadata, Message, MessageEnvelope, NodeId, Request, RequestId, Response};
use lru::LruCache;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify, RwLock};
use tracing::{debug, info, warn};

use crate::read_engine::{InodeReadEngine, ReadEngineRegistry};

/// Per-node health state used by NodeHealthTracker.
///
/// A node is "penalized" after PENALTY_THRESHOLD consecutive timeouts/errors.
/// While penalized it is sorted to the back of candidate lists so healthy nodes
/// get first crack.  After PROBE_INTERVAL the penalty is lifted and the node is
/// tried normally again; a single success clears all state.
#[derive(Debug)]
struct NodeHealth {
    /// Consecutive timeout/error count since last success.
    consecutive_failures: u32,
    /// When the current penalty period expires and we try the node again normally.
    /// None means the node is not penalized.
    penalized_until: Option<std::time::Instant>,
    /// Exponential back-off level (capped).  Each new failure while already
    /// penalized doubles the probe interval up to MAX_PROBE_SECS.
    backoff_level: u32,
}

impl NodeHealth {
    fn new() -> Self {
        Self { consecutive_failures: 0, penalized_until: None, backoff_level: 0 }
    }

    fn is_penalized(&self) -> bool {
        self.penalized_until.map(|t| t > std::time::Instant::now()).unwrap_or(false)
    }

    /// If the penalty timer has expired, reset state so the node is treated as healthy
    /// again without needing an explicit success call.  This prevents the backoff level
    /// from compounding forever on nodes that are simply never tried.
    fn maybe_clear_expired_penalty(&mut self) {
        if let Some(until) = self.penalized_until {
            if until <= std::time::Instant::now() {
                self.penalized_until = None;
                self.consecutive_failures = 0;
                self.backoff_level = 0;
            }
        }
    }
}

/// Tracks per-node health across reads and writes.
///
/// Thread-safe; cheaply cloneable via Arc.
#[derive(Clone, Debug)]
pub struct NodeHealthTracker {
    inner: Arc<Mutex<HashMap<SocketAddr, NodeHealth>>>,
}

impl NodeHealthTracker {
    /// Number of consecutive failures before a node is penalized.
    const PENALTY_THRESHOLD: u32 = 5;
    /// Base probe interval (seconds) — doubles on each repeated failure.
    const BASE_PROBE_SECS: u64 = 30;
    /// Maximum probe interval (seconds).
    const MAX_PROBE_SECS: u64 = 120;

    fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Record a successful response from `addr`.  Clears all penalty state.
    pub async fn record_success(&self, addr: SocketAddr) {
        let mut map = self.inner.lock().await;
        if let Some(h) = map.get_mut(&addr) {
            if h.consecutive_failures > 0 || h.penalized_until.is_some() {
                info!("Node {} health recovered (was {} consecutive failures)", addr, h.consecutive_failures);
            }
            h.consecutive_failures = 0;
            h.penalized_until = None;
            h.backoff_level = 0;
        }
    }

    /// Record a timeout or connection error from `addr`.
    /// Penalizes the node when the failure count crosses the threshold.
    pub async fn record_failure(&self, addr: SocketAddr) {
        let mut map = self.inner.lock().await;
        let h = map.entry(addr).or_insert_with(NodeHealth::new);
        h.consecutive_failures += 1;

        if h.consecutive_failures >= Self::PENALTY_THRESHOLD {
            let secs = (Self::BASE_PROBE_SECS << h.backoff_level).min(Self::MAX_PROBE_SECS);
            h.penalized_until = Some(std::time::Instant::now() + Duration::from_secs(secs));
            // Increase backoff level for next penalty, capped so we don't overflow the shift.
            if h.backoff_level < 8 {
                h.backoff_level += 1;
            }
            warn!(
                "Node {} penalized for {}s after {} consecutive failures",
                addr, secs, h.consecutive_failures
            );
        }
    }

    /// Returns true if `addr` is currently in a penalty period.
    pub async fn is_penalized(&self, addr: SocketAddr) -> bool {
        let mut map = self.inner.lock().await;
        if let Some(h) = map.get_mut(&addr) {
            h.maybe_clear_expired_penalty();
            h.is_penalized()
        } else {
            false
        }
    }

    /// Sort a slice of addresses so healthy nodes come first, penalized nodes last.
    /// Within each group the original order (round-robin, warm-cache preference, etc.) is preserved.
    /// Also clears any penalties whose timer has expired, so nodes self-recover without needing
    /// an explicit success call after the probe interval passes.
    pub async fn sort_by_health(&self, addrs: &[SocketAddr]) -> Vec<SocketAddr> {
        let mut map = self.inner.lock().await;
        let mut healthy = Vec::new();
        let mut penalized = Vec::new();
        for &addr in addrs {
            let is_pen = if let Some(h) = map.get_mut(&addr) {
                h.maybe_clear_expired_penalty();
                h.is_penalized()
            } else {
                false
            };
            if is_pen {
                penalized.push(addr);
            } else {
                healthy.push(addr);
            }
        }
        healthy.extend(penalized);
        healthy
    }
}

/// Cache key for byte-range caching: (inode, file_byte_offset, chunk_id)
/// chunk_id is included to prevent stale hits when a file is deleted and recreated
/// at the same inode (same offset but different content).
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
struct ByteRangeCacheKey {
    inode: u64,
    file_offset: u64,
    chunk_id: ChunkId,
}

/// Cached chunk data with metadata and TTL
#[derive(Debug, Clone)]
struct CachedChunk {
    data: Arc<Vec<u8>>,
    chunk_size: usize,
    cached_at: std::time::Instant,
}

impl CachedChunk {
    /// Check if this cached chunk has expired (TTL: 30 seconds)
    fn is_expired(&self) -> bool {
        self.cached_at.elapsed() > std::time::Duration::from_secs(30)
    }
}

/// Hint for how to read a chunk - full or partial
/// Used to optimize seeks by only fetching needed portions of chunks
#[derive(Debug, Clone)]
pub struct ChunkReadHint {
    /// Index of chunk in the file's chunk array
    pub chunk_idx: usize,
    /// The chunk ID to read
    pub chunk_id: ChunkId,
    /// Whether to fetch the full chunk (true) or just a partial range (false)
    pub full_chunk: bool,
    /// If partial read: byte offset within the chunk to start reading from
    pub offset_in_chunk: usize,
    /// If partial read: number of bytes to read from the chunk
    pub length: usize,
    /// File offset where this chunk starts (for caching)
    pub file_offset: u64,
}

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Maximum number of recent SQLite file writes to track for read-after-write consistency
const SQLITE_WRITE_TRACKER_SIZE: usize = 256;

/// Get the SQLite consistency window duration in milliseconds
/// Can be overridden via DFS_SQLITE_CONSISTENCY_WINDOW_MS environment variable
/// Default: 500ms (conservative, allows time for async replication)
fn get_sqlite_consistency_window_ms() -> u64 {
    std::env::var("DFS_SQLITE_CONSISTENCY_WINDOW_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500)
}

/// Background metadata write queue.
///
/// Active writes enqueue metadata updates here instead of blocking the FUSE thread
/// on a synchronous leader RPC. The worker drains the queue continuously, retrying
/// indefinitely with leader redirect on every failure.
///
/// On file release (close), the caller enqueues with a oneshot completion channel.
/// The worker signals the channel after confirmed delivery. The release handler awaits
/// the channel — the FUSE thread is parked in block_on but the tokio worker threads
/// keep running, so no starvation. Release retries indefinitely just like active writes.
///
/// Back-pressure: if the oldest item is >10s old (leader unreachable), new async
/// pushes block until the front clears.
pub struct MetadataQueue {
    /// Queue entries. Oldest at front; deduped by file_id.
    inner: Mutex<VecDeque<MetadataEntry>>,
    /// file_id -> position in inner for O(1) dedup replace.
    index: Mutex<HashMap<FileId, usize>>,
    /// Wakes the worker when a new item is pushed.
    notify: Notify,
    /// How long the oldest async item may sit before new pushes block.
    max_age: Duration,
}

struct MetadataEntry {
    metadata: FileMetadata,
    enqueued_at: Instant,
    /// If Some, worker signals this channel after delivery (release/sync path).
    done_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl MetadataQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(VecDeque::new()),
            index: Mutex::new(HashMap::new()),
            notify: Notify::new(),
            max_age: Duration::from_secs(24),
        })
    }

    /// Enqueue an async metadata update (fire-and-forget, no confirmation).
    /// Deduplicates by file_id — replaces existing entry in-place keeping original
    /// timestamp. Does NOT implement back-pressure directly — callers that need
    /// back-pressure (enqueue_metadata) check age and rescue before calling this.
    pub async fn push(&self, metadata: FileMetadata) {
        self.push_inner(metadata, None).await;
    }

    /// Return the age of the front entry, if any.
    pub async fn front_age(&self) -> Option<Duration> {
        self.inner.lock().await.front().map(|e| e.enqueued_at.elapsed())
    }

    /// Pop the front entry only if it is older than max_age (the stalled entry
    /// blocking back-pressure). Returns None if queue is empty or front is young.
    pub async fn pop_stalled(&self) -> Option<MetadataEntry> {
        let mut q = self.inner.lock().await;
        match q.front() {
            Some(e) if e.enqueued_at.elapsed() > self.max_age => {}
            _ => return None,
        }
        let entry = q.pop_front().unwrap();
        let mut idx = self.index.lock().await;
        idx.remove(&entry.metadata.id);
        for (i, e) in q.iter().enumerate() {
            idx.insert(e.metadata.id, i);
        }
        Some(entry)
    }

    /// Enqueue a metadata update and wait for the worker to confirm delivery.
    /// Retries indefinitely — returns only when the leader acks. Used by release().
    pub async fn push_and_wait(&self, metadata: FileMetadata) {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        self.push_inner(metadata, Some(tx)).await;
        // Await confirmation from the worker. If the sender is dropped (shouldn't
        // happen — worker never drops without sending), treat as delivered.
        let _ = rx.await;
    }

    async fn push_inner(
        &self,
        metadata: FileMetadata,
        done_tx: Option<tokio::sync::oneshot::Sender<()>>,
    ) {
        let op = if done_tx.is_some() { "release" } else { "update" };
        let mut q = self.inner.lock().await;
        let mut idx = self.index.lock().await;

        if let Some(&pos) = idx.get(&metadata.id) {
            if let Some(entry) = q.get_mut(pos) {
                // Dedup replace: only replace if incoming write_seq >= existing.
                // This ensures newer metadata (higher sequence) always wins, even if
                // a stale entry somehow arrives after a newer one was already queued.
                if metadata.write_seq >= entry.metadata.write_seq {
                    info!(
                        "[META QUEUE] enqueue op={} path={} id={} seq={} size={} (replacing seq={})",
                        op, metadata.path, metadata.id, metadata.write_seq,
                        metadata.size, entry.metadata.write_seq
                    );
                    // If the existing entry had a done_tx (sync waiter), preserve it —
                    // the release caller is still waiting and must be notified on delivery.
                    // If the new push also has a done_tx, the new one wins (latest close wins).
                    if done_tx.is_some() {
                        entry.done_tx = done_tx;
                    }
                    entry.metadata = metadata;
                } else {
                    // Incoming is older — drop it, but transfer done_tx if present so
                    // a release() waiter still gets notified when the newer entry delivers.
                    info!(
                        "[META QUEUE] drop-stale op={} path={} id={} seq={} (queue has seq={})",
                        op, metadata.path, metadata.id, metadata.write_seq,
                        entry.metadata.write_seq
                    );
                    if done_tx.is_some() && entry.done_tx.is_none() {
                        entry.done_tx = done_tx;
                    }
                }
                drop(q);
                drop(idx);
                self.notify.notify_one();
                return;
            }
        }

        info!(
            "[META QUEUE] enqueue op={} path={} id={} seq={} size={} queue_len={}",
            op, metadata.path, metadata.id, metadata.write_seq, metadata.size, q.len() + 1
        );
        let pos = q.len();
        idx.insert(metadata.id, pos);
        q.push_back(MetadataEntry { metadata, enqueued_at: Instant::now(), done_tx });
        drop(q);
        drop(idx);
        self.notify.notify_one();
    }

    /// Returns true if there are no pending entries.
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.is_empty()
    }

    /// Re-insert a rescued entry at the front of the queue (all nodes unreachable).
    /// Preserves done_tx so any release() waiter is eventually notified.
    async fn push_inner_front(&self, entry: MetadataEntry) {
        let mut q = self.inner.lock().await;
        let mut idx = self.index.lock().await;
        // Rebuild index after prepend.
        idx.insert(entry.metadata.id, 0);
        q.push_front(entry);
        for (i, e) in q.iter().enumerate() {
            idx.insert(e.metadata.id, i);
        }
    }

    /// Cancel any pending queue entry for the given file_id.
    /// Called after a successful delete_file so the queue worker doesn't
    /// resurrect the file by delivering a stale metadata update.
    pub async fn cancel(&self, file_id: FileId) {
        let mut q = self.inner.lock().await;
        let mut idx = self.index.lock().await;
        if let Some(pos) = idx.remove(&file_id) {
            if let Some(removed) = q.remove(pos) {
                info!(
                    "[META QUEUE] cancel id={} path={} seq={} (delete pre-empt)",
                    file_id, removed.metadata.path, removed.metadata.write_seq
                );
            }
            // Rebuild index positions after removal.
            for (i, e) in q.iter().enumerate() {
                idx.insert(e.metadata.id, i);
            }
        }
    }

    /// Remove and return the front entry, if any.
    async fn pop(&self) -> Option<MetadataEntry> {
        let mut q = self.inner.lock().await;
        if let Some(entry) = q.pop_front() {
            let mut idx = self.index.lock().await;
            idx.remove(&entry.metadata.id);
            for (i, e) in q.iter().enumerate() {
                idx.insert(e.metadata.id, i);
            }
            Some(entry)
        } else {
            None
        }
    }
}

/// Client for communicating with DFS cluster
#[derive(Clone)]
pub struct DfsClient {
    /// List of cluster nodes (updated by refresh_cluster_nodes)
    cluster_nodes: Arc<RwLock<Vec<SocketAddr>>>,

    /// Original seed addresses provided at startup.
    /// Never mutated — used as a fallback when all cluster_nodes are unreachable
    /// so we can re-bootstrap cluster membership from scratch.
    seed_nodes: Vec<SocketAddr>,

    /// Current node index (for round-robin)
    current_node: Arc<RwLock<usize>>,

    /// LRU cache for chunks (ChunkId -> data)
    /// Cache up to 256 chunks (~1GB at 4MB/chunk)
    chunk_cache: Arc<tokio::sync::RwLock<LruCache<ChunkId, Arc<Vec<u8>>>>>,

    /// Byte-range cache for recently-accessed chunks (inode, offset) -> chunk data
    /// This solves the problem of content-addressed chunks changing during live DVR recording
    /// Even if chunk hashes change, we can still cache by file position
    byte_range_cache: Arc<Mutex<LruCache<ByteRangeCacheKey, CachedChunk>>>,

    /// TCP connection pool - maintains up to N idle connections per server
    /// VecDeque allows concurrent callers to each get their own connection.
    /// Arc<Mutex<...>> so the Arc can be cloned out of the DashMap before any .await,
    /// preventing the DashMap shard read-lock from being held across await points.
    connection_pool: Arc<DashMap<SocketAddr, Arc<Mutex<std::collections::VecDeque<TcpStream>>>>>,

    /// Track chunks currently being prefetched to avoid duplicates
    prefetch_in_flight: Arc<Mutex<HashSet<ChunkId>>>,

    /// Track recent read positions per file to detect sequential patterns
    /// Maps file_id (first chunk) -> VecDeque of last 4 read positions
    /// Limited to 256 entries to prevent unbounded growth during fast-forward/seeking
    read_history: Arc<tokio::sync::RwLock<LruCache<ChunkId, VecDeque<usize>>>>,

    /// Track last prefetched position per file to avoid duplicate prefetch from parallel reads
    /// Maps file_id -> last_chunk_idx that triggered prefetch
    /// Limited to 256 entries to prevent unbounded growth
    last_prefetch_position: Arc<Mutex<LruCache<ChunkId, usize>>>,

    /// Round-robin counter for replica selection (for load balancing)
    replica_selector: Arc<AtomicU64>,

    /// Replica location cache: ChunkId -> Vec<SocketAddr>
    /// Caches which nodes have which chunks to avoid metadata queries on every read
    /// Cache up to 128 entries (3x prefetch window of 32 = 256MB working set at 2MB/chunk)
    /// Small cache = faster lookups, less memory, better CPU cache utilization
    replica_cache: Arc<Mutex<LruCache<ChunkId, Arc<Vec<SocketAddr>>>>>,

    /// Track recent writes to SQLite files for read-after-write consistency
    /// Maps file path -> (write_node_addr, write_timestamp)
    /// Prevents reading stale metadata from non-write nodes before async replication completes
    sqlite_write_tracker: Arc<Mutex<LruCache<String, (SocketAddr, std::time::Instant)>>>,

    /// Address to NodeId mapping for chunk_locations metadata
    /// Maps SocketAddr -> NodeId to use real node IDs instead of synthetic ones
    addr_to_node_id: Arc<RwLock<HashMap<SocketAddr, dfs_common::NodeId>>>,

    /// Tracks which chunks have been prefetch-hinted to which server nodes
    /// When reading chunks, prefer these nodes to hit warm server caches
    /// Maps ChunkId -> (SocketAddr, timestamp) where timestamp is when hint was sent
    /// Expires after 60 seconds (assume cache eviction after that)
    warm_cache_map: Arc<Mutex<LruCache<ChunkId, (SocketAddr, std::time::Instant)>>>,

/// Address of the current cluster leader, used to route GetFileChunkMap requests.
    /// Updated during refresh_cluster_nodes(). Falls back to any node if unknown.
    leader_addr: Arc<RwLock<Option<SocketAddr>>>,

    /// Global semaphore capping total concurrent chunk fetches across ALL simultaneous
    /// read_data calls. Without this, a seek causes N parallel FUSE reads each spawning
    /// their own 20-slot semaphore, producing N*20 simultaneous connections and
    /// exhausting server file descriptors.
    fetch_semaphore: Arc<tokio::sync::Semaphore>,

    /// Per-node health tracker.  Penalizes nodes that time out repeatedly and
    /// automatically re-admits them after a back-off period.
    node_health: NodeHealthTracker,

    /// Replication factor fetched from cluster during refresh_cluster_nodes.
    /// Defaults to 2 until the first successful cluster status response.
    replication_factor: Arc<AtomicUsize>,

    /// Async metadata write queue. Active writes enqueue here; background worker
    /// drains to leader with redirect/retry. Release path bypasses this and sends
    /// synchronously via flush_metadata_sync().
    pub(crate) metadata_queue: Arc<MetadataQueue>,

    /// Per-file monotonic write sequence counter. Each metadata enqueue increments
    /// the counter for that file_id and stamps it on the metadata before queuing.
    /// Prevents out-of-order dissemination from overwriting newer records with stale ones.
    /// Seeded from the server's stored write_seq on first open-for-write.
    write_seq: Arc<DashMap<FileId, u64>>,

    /// Per-inode read engines.  Each open file gets one engine that holds the chunk map
    /// snapshot and pipeline state.  Writers never touch this; engines refresh lazily.
    pub read_engines: ReadEngineRegistry,
}

impl DfsClient {
    /// Create a new DFS client
    pub fn new(cluster_nodes: Vec<SocketAddr>) -> Result<Self> {
        if cluster_nodes.is_empty() {
            anyhow::bail!("No cluster nodes provided");
        }

        // Initialize LRU cache with CONSERVATIVE limits to prevent OOM on large sequential reads
        // CRITICAL: Both chunk_cache and byte_range_cache store the SAME data (doubled memory usage!)
        // Target: 2-3% of available RAM per cache (~128-256MB total at 4MB chunks)
        // min 8 chunks (~32MB), max 64 chunks (~256MB) PER CACHE
        let available_mb = dfs_common::get_available_memory()
            .map(|bytes| bytes / (1024 * 1024))
            .unwrap_or(1024);

        // Check for environment variable override
        // Default max based on available RAM. On write-heavy low-RAM clients (e.g. nanopir3
        // 1.9GB) the write buffer + in-flight pipeline data consumes 50-100MB during writes,
        // so cache sizes must leave headroom. Cap byte_range_cache tightly on < 2GB systems.
        let default_max_chunks = if available_mb < 512 {
            8    // Very low RAM: 32MB per cache
        } else if available_mb < 2048 {
            8    // Low-to-medium RAM (< 2GB): 32MB per cache — write buffer needs the headroom
        } else if available_mb < 4096 {
            64   // Good RAM (2-4GB): 256MB per cache
        } else {
            128  // High RAM (4GB+): 512MB per cache
        };

        let max_chunks = std::env::var("DFS_MAX_CACHE_CHUNKS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(default_max_chunks);

        let (chunk_target_pct, byte_target_pct, min_chunks) = if available_mb < 256 {
            // Extremely low memory systems (<256MB available): minimal cache
            // chunk: 4%, byte: 4%, min 2 chunks (~10MB + ~10MB = ~20MB total)
            // Just enough to avoid OOM on very constrained devices
            (4, 4, 2)
        } else if available_mb < 512 {
            // Very low memory systems (256-512MB available): conservative but usable
            // chunk: 8%, byte: 8%, min 4 chunks (~20MB + ~20MB = ~40MB total)
            // Enough for basic live streaming caching on ARM devices
            (8, 8, 4)
        } else if available_mb < 1024 {
            // Low memory systems (512MB-1GB available): moderate cache
            // chunk: 12%, byte: 12%, min 8 chunks (~60MB + ~60MB = ~120MB total)
            (12, 12, 8)
        } else if available_mb < 1536 {
            // Medium memory systems (1-1.5GB available): good cache
            // chunk: 15%, byte: 15%, min 12 chunks (~115MB + ~115MB = ~230MB total)
            (15, 15, 12)
        } else {
            // Normal systems (>1.5GB available): generous cache sizes
            // chunk: 18%, byte: 18%, min 16 chunks (~140MB + ~140MB = ~280MB total)
            // Larger cache prevents thrashing during high-speed sequential reads
            (18, 18, 16)
        };

        let cache_capacity = dfs_common::calculate_cache_capacity(
            4 * 1024 * 1024, // 4MB chunk size
            chunk_target_pct,
            min_chunks,
            max_chunks,
        )
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to calculate cache capacity: {}, using default of 32 chunks", e);
            NonZeroUsize::new(32).unwrap()
        });

        let cache = LruCache::new(cache_capacity);

        // Byte-range cache uses same conservative limits (both caches hold same data!)
        let byte_cache_capacity = dfs_common::calculate_cache_capacity(
            4 * 1024 * 1024, // 4MB chunk size
            byte_target_pct,
            min_chunks,
            max_chunks,
        )
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to calculate byte-range cache capacity: {}, using default", e);
            NonZeroUsize::new(16).unwrap()
        });

        let byte_range_cache = LruCache::new(byte_cache_capacity);

        // Replica location cache: MUST be large to avoid metadata query storms!
        // Each entry is just Arc<Vec<SocketAddr>> (~40-80 bytes), so even 2000 entries = ~160KB
        // CRITICAL: Should be much larger than chunk cache to cache replica locations
        // for sequential reads of large files (1000+ chunks)
        // A "replica storm" (100s of metadata queries) occurs when this is too small
        let replica_cache_capacity = std::env::var("DFS_REPLICA_CACHE_SIZE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .and_then(|v| NonZeroUsize::new(v))
            .unwrap_or_else(|| {
                // Default: 4x chunk cache capacity (min 512, max 2000)
                // This ensures we can cache locations for large sequential files
                let size = (cache_capacity.get() * 4).max(512).min(2000);
                NonZeroUsize::new(size).unwrap()
            });
        let replica_cache = LruCache::new(replica_cache_capacity);

        // SQLite write tracker: small LRU to prevent unbounded growth
        // Only tracks SQLite database files for read-after-write consistency
        let sqlite_write_tracker_capacity = NonZeroUsize::new(SQLITE_WRITE_TRACKER_SIZE)
            .expect("SQLITE_WRITE_TRACKER_SIZE must be > 0");
        let sqlite_write_tracker = LruCache::new(sqlite_write_tracker_capacity);

        // Track warm server caches - which chunks we've hinted to which nodes
        // Cache size matches prefetch window (up to 50 chunks ahead)
        let warm_cache_capacity = NonZeroUsize::new(128)
            .expect("warm_cache_capacity must be > 0");
        let warm_cache_map = LruCache::new(warm_cache_capacity);

        Ok(Self {
            cluster_nodes: Arc::new(RwLock::new(cluster_nodes.clone())),
            seed_nodes: cluster_nodes,
            current_node: Arc::new(RwLock::new(0)),
            chunk_cache: Arc::new(tokio::sync::RwLock::new(cache)),
            byte_range_cache: Arc::new(Mutex::new(byte_range_cache)),
            connection_pool: Arc::new(DashMap::new()),
            prefetch_in_flight: Arc::new(Mutex::new(HashSet::new())),
            read_history: Arc::new(tokio::sync::RwLock::new(LruCache::new(NonZeroUsize::new(256).unwrap()))),
            last_prefetch_position: Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(256).unwrap()))),
            replica_selector: Arc::new(AtomicU64::new(0)),
            replica_cache: Arc::new(Mutex::new(replica_cache)),
            sqlite_write_tracker: Arc::new(Mutex::new(sqlite_write_tracker)),
            addr_to_node_id: Arc::new(RwLock::new(HashMap::new())),
            warm_cache_map: Arc::new(Mutex::new(warm_cache_map)),
leader_addr: Arc::new(RwLock::new(None)),
            fetch_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
            node_health: NodeHealthTracker::new(),
            replication_factor: Arc::new(AtomicUsize::new(2)),
            metadata_queue: MetadataQueue::new(),
            write_seq: Arc::new(DashMap::new()),
            read_engines: ReadEngineRegistry::new(),
        })
    }

    /// Check if a path represents a SQLite database file
    /// These files require special handling for read-after-write consistency.
    /// Matches .db, .sqlite, .sqlite3 and their WAL/journal/shm sidecars,
    /// plus SQLite temp files like gravity.db_temp (pihole pattern).
    fn is_sqlite_file(path: &str) -> bool {
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

    /// Get next node address (round-robin)
    async fn get_next_node(&self) -> SocketAddr {
        let nodes = self.cluster_nodes.read().await;
        let mut current = self.current_node.write().await;

        let addr = nodes[*current];
        *current = (*current + 1) % nodes.len();

        addr
    }

    /// Send a request to a cluster node with retry.
    ///
    /// Tries every known node in order. If all fail, waits briefly then re-bootstraps
    /// from the seed list and tries once more. A small inter-node delay (100ms) prevents
    /// hammering the network when nodes are refusing connections quickly.
    async fn send_request_with_retry(&self, request: Request) -> Result<Response> {
        let nodes = self.cluster_nodes.read().await.clone();
        let mut last_error = None;

        for (i, node_addr) in nodes.iter().enumerate() {
            if i > 0 {
                // Brief pause between node attempts — avoids a connection storm when
                // multiple nodes are down and each refuses immediately.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            match self.send_request(*node_addr, request.clone()).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    warn!("Failed to send request to {}: {}", node_addr, e);
                    last_error = Some(e);
                }
            }
        }

        // All known nodes failed — wait briefly, then re-bootstrap from seed list and retry once.
        warn!("All cluster nodes unreachable, re-bootstrapping from seed list");
        tokio::time::sleep(Duration::from_millis(500)).await;

        if let Err(e) = self.refresh_cluster_nodes().await {
            warn!("Re-bootstrap failed: {}", e);
        } else {
            let refreshed = self.cluster_nodes.read().await.clone();
            for (i, node_addr) in refreshed.iter().enumerate() {
                if i > 0 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                match self.send_request(*node_addr, request.clone()).await {
                    Ok(response) => return Ok(response),
                    Err(e) => {
                        warn!("Post-refresh: failed to send request to {}: {}", node_addr, e);
                        last_error = Some(e);
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All nodes failed")))
    }

    /// Send a request with retry, returning the successful node's address
    /// This is used for tracking which node handled a write operation
    async fn send_request_with_retry_tracking(&self, request: Request) -> Result<SocketAddr> {
        let nodes = self.cluster_nodes.read().await.clone();
        let mut last_error = None;

        // Try all nodes
        for node_addr in &nodes {
            match self.send_request(*node_addr, request.clone()).await {
                Ok(response) => {
                    // Check if response indicates success
                    match response {
                        Response::Ok { .. } => return Ok(*node_addr),
                        Response::Error { message, .. } => {
                            anyhow::bail!("Server returned error: {}", message);
                        }
                        _ => anyhow::bail!("Unexpected response type"),
                    }
                }
                Err(e) => {
                    warn!("Failed to send request to {}: {}", node_addr, e);
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All nodes failed")))
    }

    /// Send a request to a specific node, reusing a pooled connection when available.
    async fn send_request(&self, addr: SocketAddr, request: Request) -> Result<Response> {
        debug!("Sending request to {}: {:?}", addr, request);

        let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
        let envelope = MessageEnvelope::new(request_id, Message::Request(request));
        let encoded = envelope.to_bytes().context("Failed to serialize message")?;

        // Try pooled connection first; on failure (stale) fall back to a fresh one.
        // Clone the Arc out of DashMap before .await to release the shard read-lock immediately.
        let pooled = {
            let mutex_opt = self.connection_pool.get(&addr).map(|e| Arc::clone(&*e));
            if let Some(mutex) = mutex_opt {
                mutex.lock().await.pop_front()
            } else {
                None
            }
        };

        let mut stream = match pooled {
            Some(s) => {
                // Check if the server closed this connection while it was pooled.
                // A readable socket returning 0 bytes means the peer sent FIN —
                // reusing it would leave the server in CLOSE-WAIT indefinitely.
                let mut buf = [0u8; 1];
                let peer_closed = match s.try_read(&mut buf) {
                    Ok(0) => true,
                    Ok(_) => true,  // unexpected data — discard
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
                    Err(_) => true,
                };

                if peer_closed {
                    debug!("Pooled connection to {} closed by peer, reconnecting", addr);
                    let mut s = s;
                    let _ = s.shutdown().await;
                    let fresh = tokio::time::timeout(
                        tokio::time::Duration::from_millis(1000),
                        TcpStream::connect(addr),
                    ).await
                        .map_err(|_| anyhow::anyhow!("Connect timeout to {}", addr))?
                        .context("Failed to connect to node")?;
                    let _ = fresh.set_nodelay(true);
                    fresh
                } else {
                    s
                }
            }
            None => {
                let fresh = tokio::time::timeout(
                    tokio::time::Duration::from_secs(5),
                    TcpStream::connect(addr),
                ).await
                    .map_err(|_| anyhow::anyhow!("Failed to connect to node: connect timeout"))?
                    .context("Failed to connect to node")?;
                let _ = fresh.set_nodelay(true);
                fresh
            }
        };

        // Send and receive with a 30s timeout.  This must cover the full round-trip for large
        // write payloads (4MB+) on a slow HDD node, but should fail quickly enough that the
        // caller can fall back to a different node rather than blocking the write pipeline.
        let io_future = async {
            let len = encoded.len() as u32;
            stream.write_all(&len.to_be_bytes()).await?;
            stream.write_all(&encoded).await?;
            stream.flush().await?;

            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await?;
            let len = u32::from_be_bytes(len_buf) as usize;

            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await?;
            Ok::<Vec<u8>, std::io::Error>(buf)
        };

        let buf = match tokio::time::timeout(tokio::time::Duration::from_secs(3), io_future).await {
            Ok(Ok(buf)) => buf,
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe
                        || e.kind() == std::io::ErrorKind::ConnectionReset
                        || e.kind() == std::io::ErrorKind::ConnectionAborted
                        || e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Stale pooled connection — server closed it after idle timeout.
                // Retry transparently on a fresh connection; do NOT record a health
                // failure since the node itself is fine.
                // UnexpectedEof is the common case: server's 5s idle timeout fires
                // while a connection is sitting in the client pool.
                debug!("Stale pooled connection to {} ({}), retrying with fresh connection", addr, e);
                let mut fresh = match tokio::time::timeout(
                    tokio::time::Duration::from_secs(5),
                    TcpStream::connect(addr),
                ).await {
                    Ok(Ok(s)) => { let _ = s.set_nodelay(true); s }
                    Ok(Err(e)) => {
                        self.node_health.record_failure(addr).await;
                        return Err(anyhow::anyhow!("Failed to connect to node {}: {}", addr, e));
                    }
                    Err(_) => {
                        self.node_health.record_failure(addr).await;
                        return Err(anyhow::anyhow!("Failed to connect to node: connect timeout"));
                    }
                };
                let len = encoded.len() as u32;
                let retry_result = async {
                    fresh.write_all(&len.to_be_bytes()).await.context("write len")?;
                    fresh.write_all(&encoded).await.context("write body")?;
                    fresh.flush().await.context("flush")?;
                    let mut len_buf = [0u8; 4];
                    fresh.read_exact(&mut len_buf).await.context("read len")?;
                    let rlen = u32::from_be_bytes(len_buf) as usize;
                    let mut buf = vec![0u8; rlen];
                    fresh.read_exact(&mut buf).await.context("read body")?;
                    Ok::<Vec<u8>, anyhow::Error>(buf)
                };
                match tokio::time::timeout(tokio::time::Duration::from_secs(3), retry_result).await {
                    Ok(Ok(buf)) => {
                        stream = fresh;
                        buf
                    }
                    Ok(Err(e)) => {
                        self.node_health.record_failure(addr).await;
                        return Err(e);
                    }
                    Err(_) => {
                        self.node_health.record_failure(addr).await;
                        return Err(anyhow::anyhow!("Timeout reading chunk from {}", addr));
                    }
                }
            }
            Ok(Err(e)) => {
                self.node_health.record_failure(addr).await;
                return Err(anyhow::anyhow!("I/O error talking to {}: {}", addr, e));
            }
            Err(_) => {
                // Stale pooled connection or timeout — retry once with a fresh connection
                let mut fresh = match tokio::time::timeout(
                    tokio::time::Duration::from_secs(5),
                    TcpStream::connect(addr),
                ).await {
                    Ok(Ok(s)) => { let _ = s.set_nodelay(true); s }
                    Ok(Err(e)) => {
                        self.node_health.record_failure(addr).await;
                        return Err(anyhow::anyhow!("Failed to connect to node {}: {}", addr, e));
                    }
                    Err(_) => {
                        self.node_health.record_failure(addr).await;
                        return Err(anyhow::anyhow!("Failed to connect to node: connect timeout"));
                    }
                };

                // Reuse the same serialized envelope (idempotent for reads; acceptable for writes)
                let len = encoded.len() as u32;
                let retry_result = async {
                    fresh.write_all(&len.to_be_bytes()).await.context("write len")?;
                    fresh.write_all(&encoded).await.context("write body")?;
                    fresh.flush().await.context("flush")?;
                    let mut len_buf = [0u8; 4];
                    fresh.read_exact(&mut len_buf).await.context("read len")?;
                    let rlen = u32::from_be_bytes(len_buf) as usize;
                    let mut buf = vec![0u8; rlen];
                    fresh.read_exact(&mut buf).await.context("read body")?;
                    Ok::<Vec<u8>, anyhow::Error>(buf)
                };
                match tokio::time::timeout(tokio::time::Duration::from_secs(3), retry_result).await {
                    Ok(Ok(buf)) => {
                        stream = fresh;
                        buf
                    }
                    Ok(Err(e)) => {
                        self.node_health.record_failure(addr).await;
                        return Err(e);
                    }
                    Err(_) => {
                        self.node_health.record_failure(addr).await;
                        return Err(anyhow::anyhow!("Timeout reading chunk from {}", addr));
                    }
                }
            }
        };

        let response_envelope = MessageEnvelope::from_bytes(&buf)
            .context("Failed to deserialize response")?;

        let response = match response_envelope.message {
            Message::Response(Response::ChunkData { data, chunk_id, cache_stats }) => {
                // Split-frame: if data is empty, raw payload follows on the stream.
                let data = if data.is_empty() {
                    dfs_common::protocol::read_chunk_payload(&mut stream).await
                        .context("read split-frame chunk payload")?
                } else {
                    data
                };
                Response::ChunkData { chunk_id, data, cache_stats }
            }
            Message::Response(response) => response,
            _ => anyhow::bail!("Expected Response message"),
        };

        // Return connection to pool after all bytes are drained.
        // Clone Arc out before .await to avoid holding DashMap shard lock across await.
        {
            let mutex = {
                let entry = self.connection_pool
                    .entry(addr)
                    .or_insert_with(|| Arc::new(Mutex::new(std::collections::VecDeque::new())));
                Arc::clone(&*entry)
            };
            let mut queue = mutex.lock().await;
            if queue.len() < 8 {
                queue.push_back(stream);
            }
        }

        self.node_health.record_success(addr).await;
        Ok(response)
    }

    /// Get file metadata from cluster with optional conditional fetch
    /// Returns Ok(Some(metadata)) if found and modified, Ok(None) if not found, Err if error
    /// If if_modified_since is provided and metadata hasn't changed, returns Ok(None) with NotModified indicator
    pub async fn get_file_metadata_conditional(&self, path: &str, if_modified_since: Option<u64>) -> Result<Option<FileMetadata>> {
        let request = Request::GetFileMetadataByPath {
            path: path.to_string(),
            if_modified_since,
        };

        // Always query the leader — followers can have stale or missing metadata.
        // Use a short 1s timeout so a busy leader doesn't stall every lookup;
        // fall back to any node quickly rather than waiting for send_request's full 3s.
        let leader = { *self.leader_addr.read().await };
        let response = if let Some(leader_addr) = leader {
            match tokio::time::timeout(
                Duration::from_secs(1),
                self.send_request(leader_addr, request.clone()),
            ).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    warn!("get_file_metadata_conditional: leader {} failed ({}), retrying any node", leader_addr, e);
                    self.send_request_with_retry(request).await?
                }
                Err(_) => {
                    warn!("get_file_metadata_conditional: leader {} timed out, retrying any node", leader_addr);
                    self.send_request_with_retry(request).await?
                }
            }
        } else {
            self.send_request_with_retry(request).await?
        };

        match response {
            Response::FileMetadata { metadata } => Ok(Some(metadata)),
            Response::NotModified => {
                // Metadata hasn't changed, return None to signal cache is valid
                debug!("Metadata not modified for {}", path);
                Ok(None)
            }
            Response::Error { code, .. } if code == dfs_common::ErrorCode::NotFound => Ok(None),
            Response::Error { message, .. } => {
                anyhow::bail!("Server error: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Get file metadata from cluster (unconditional)
    pub async fn get_file_metadata(&self, path: &str) -> Result<Option<FileMetadata>> {
        // Check if this is a SQLite file with a recent write
        // If so, force read from the write node to ensure read-after-write consistency
        if Self::is_sqlite_file(path) {
            let write_info = {
                let mut tracker = self.sqlite_write_tracker.lock().await;
                tracker.get(path).copied()
            };

            if let Some((write_node, write_time)) = write_info {
                let age = write_time.elapsed();
                let window = std::time::Duration::from_millis(get_sqlite_consistency_window_ms());

                if age < window {
                    // Within consistency window - force read from write node
                    info!(
                        "SQLite read-after-write: forcing read from write node {} (age: {:?}, window: {:?})",
                        write_node, age, window
                    );
                    return self.get_file_metadata_from_node(path, write_node).await;
                } else {
                    debug!(
                        "SQLite consistency window expired for {} (age: {:?} > {:?})",
                        path, age, window
                    );
                }
            }
        }

        // Normal path: use retry logic (SQLite files outside window, or non-SQLite files)
        self.get_file_metadata_conditional(path, None).await
    }

    /// Get file metadata from a specific node with fallback to retry logic
    /// Used for SQLite read-after-write consistency to ensure we read from the write node
    async fn get_file_metadata_from_node(
        &self,
        path: &str,
        node: SocketAddr
    ) -> Result<Option<FileMetadata>> {
        let request = Request::GetFileMetadataByPath {
            path: path.to_string(),
            if_modified_since: None,
        };

        // Try the specified node first
        match self.send_request(node, request.clone()).await {
            Ok(response) => match response {
                Response::FileMetadata { metadata } => return Ok(Some(metadata)),
                Response::NotModified => return Ok(None),
                Response::Error { code, .. } if code == dfs_common::ErrorCode::NotFound => {
                    return Ok(None)
                }
                Response::Error { message, .. } => {
                    warn!("Error from write node {}, falling back: {}", node, message);
                }
                _ => {
                    warn!("Unexpected response from {}, falling back", node);
                }
            },
            Err(e) => {
                // Write node is down - fall back to normal retry logic
                warn!(
                    "Failed to read from write node {} ({}), falling back to retry logic",
                    node, e
                );
            }
        }

        // Fallback: use normal retry logic if write node failed
        info!("Falling back to normal retry for {}", path);
        self.get_file_metadata_conditional(path, None).await
    }

    /// Fetch all files from the leader for startup cache warming.
    pub async fn list_all_files(&self) -> Result<Vec<FileMetadata>> {
        let request = Request::ListAllFiles;
        let leader = { *self.leader_addr.read().await };
        let target = if let Some(addr) = leader {
            addr
        } else {
            let nodes = self.cluster_nodes.read().await.clone();
            *nodes.first().context("No cluster nodes available")?
        };
        let response = match tokio::time::timeout(
            Duration::from_secs(30),
            self.send_request(target, request.clone()),
        ).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                warn!("list_all_files: leader failed ({}), retrying any node", e);
                self.send_request_with_retry(request).await?
            }
            Err(_) => {
                warn!("list_all_files: leader timed out, retrying any node");
                self.send_request_with_retry(request).await?
            }
        };
        match response {
            Response::FileList { files, .. } => Ok(files),
            Response::Error { message, .. } => anyhow::bail!("Server error: {}", message),
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// List directory contents
    pub async fn list_directory(&self, path: &str) -> Result<Vec<FileMetadata>> {
        let request = Request::ListDirectory {
            path: path.to_string(),
        };

        // Always query the leader — followers can have stale metadata if async
        // replication hasn't completed yet. Fall back to any node if leader unknown.
        let target = {
            let leader = self.leader_addr.read().await;
            match *leader {
                Some(addr) => addr,
                None => {
                    let nodes = self.cluster_nodes.read().await;
                    *nodes.first().context("No cluster nodes available")?
                }
            }
        };

        let response = match self.send_request(target, request.clone()).await {
            Ok(r) => r,
            Err(e) => {
                warn!("list_directory to leader failed ({}), retrying any node", e);
                self.send_request_with_retry(request).await?
            }
        };

        match response {
            Response::DirectoryListing { entries } => Ok(entries),
            Response::Error { message, .. } => {
                anyhow::bail!("Server error: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Read data from cluster by chunk IDs - parallelized with caching
    /// Pipeline depth for sequential reads: how many chunks to keep in flight simultaneously.
    /// Formula mirrors the write pipeline: ceil(32MB / chunk_size), minimum 1.
    /// With 4MB chunks → 8 in flight, which at ~113 MB/s wire speed gives full saturation.
    fn pipeline_depth(chunk_size: usize) -> usize {
        // We want exactly 1 chunk of lookahead — enough to hide the connection RTT
        // (~10ms) behind the body transfer of the current chunk (~250ms on 1Gbps).
        // More than 2 in-flight at once wastes bandwidth and competes for NIC capacity.
        let _ = chunk_size; // reserved for future adaptive tuning
        2
    }

    // -------------------------------------------------------------------------
    // New per-inode read engine path
    // -------------------------------------------------------------------------

    /// Main read entry point used by the FUSE layer.
    ///
    /// `inode`      — kernel inode number
    /// `file_size`  — current size from metadata_cache (used to detect live-recording growth)
    /// `file_id`    — FileId for chunk-map RPCs
    /// `file_path`  — path, for SQLite cache-bypass detection
    /// `offset`     — byte offset within file
    /// `size`       — bytes requested
    ///
    /// Returns the raw bytes for [offset, offset+size) clipped to file_size.
    /// Never blocks the write path — engine refreshes are async and use their own locks.
    pub async fn read_file(
        &self,
        inode: u64,
        file_size: u64,
        file_id: FileId,
        file_path: &str,
        offset: usize,
        size: usize,
    ) -> Result<Vec<u8>> {
        if size == 0 || offset >= file_size as usize {
            return Ok(Vec::new());
        }

        let engine = self.read_engines.get_or_create(inode);

        // Refresh chunk map if stale or file grew — but do it in the background so the
        // current read is never blocked by a leader round-trip (~50-100ms). The stale
        // snapshot is safe to use: it has valid chunk locations; the next read after the
        // refresh completes will get the updated map. Only block when the map is empty
        // (handled below in the empty-map retry loop).
        // Compute the chunk index for the current read position — used to request only
        // the window of chunks we actually need from the leader.
        const CHUNK_SIZE_USIZE: usize = 4 * 1024 * 1024;
        let current_chunk = (offset / CHUNK_SIZE_USIZE) as u32;

        if engine.needs_refresh(file_size, current_chunk).await {
            let engine_clone = engine.clone();
            let client_clone = self.clone();
            tokio::spawn(async move {
                client_clone.refresh_engine(&engine_clone, file_id, file_size, current_chunk).await;
            });
        }

        let (mut chunk_map, mut chunk_offsets, mut nim) = engine.snapshot().await;

        // If the chunk map is empty but the file has a non-zero size, the leader may not
        // have received the first ReplicateChunkLocation yet (write in progress, first 4MB
        // chunk still accumulating in the write buffer).  Retry for up to 8s so that a
        // reader who opens a live-recording file during the first chunk's fill window
        // doesn't get a permanent empty read.
        if chunk_map.is_empty() && file_size > 0 {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            while std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                self.refresh_engine(&engine, file_id, file_size, 0).await;
                let snap = engine.snapshot().await;
                chunk_map = snap.0; chunk_offsets = snap.1; nim = snap.2;
                if !chunk_map.is_empty() { break; }
            }
        }

        if chunk_map.is_empty() {
            info!("read_file: inode={} chunk map empty after refresh, returning empty", inode);
            return Ok(Vec::new());
        }

        // SQLite files: bypass chunk_cache to prevent stale-read corruption.
        let bypass_cache = crate::fuse_impl::is_sqlite_for_cache(file_path);

        let end = offset + size;
        let needed = InodeReadEngine::chunks_for_range(&chunk_offsets, offset, size);

        if needed.is_empty() {
            // Hole (sparse file) — return zeros.
            let len = (file_size as usize).min(end).saturating_sub(offset);
            return Ok(vec![0u8; len]);
        }

        let nodes = self.cluster_nodes.read().await.clone();
        let selector = self.replica_selector.fetch_add(1, Ordering::Relaxed);

        // --- Cache check ---
        let mut result_chunks: Vec<(usize /*chunk_idx*/, Arc<Vec<u8>>)> = Vec::new();
        let mut to_fetch: Vec<(usize, ChunkId, SocketAddr, Vec<SocketAddr>)> = Vec::new();
        let mut to_wait: Vec<(usize, ChunkId)> = Vec::new();

        for (chunk_idx, _chunk_start, _chunk_size) in &needed {
            let idx = *chunk_idx;
            let loc = &chunk_map[idx];
            let cid = loc.chunk_id;

            // 1. Chunk cache (skip for SQLite).
            if !bypass_cache {
                let cache = self.chunk_cache.read().await;
                if let Some(data) = cache.peek(&cid) {
                    result_chunks.push((idx, Arc::clone(data)));
                    continue;
                }
            }

            // 2. Another request already fetching it?
            {
                let inf = engine.in_flight.lock().await;
                if inf.contains(&cid) {
                    to_wait.push((idx, cid));
                    continue;
                }
            }

            // 3. Need to fetch.
            let (primary, fallbacks) = match InodeReadEngine::resolve_primary(
                loc, &nim, &nodes, selector + idx as u64,
            ) {
                Some(pf) => pf,
                None => {
                    // No replicas known — fall back to any cluster node.
                    let p = nodes[selector as usize % nodes.len()];
                    (p, nodes.iter().filter(|&&a| a != p).copied().collect())
                }
            };

            engine.in_flight.lock().await.insert(cid);
            to_fetch.push((idx, cid, primary, fallbacks));
        }

        // --- Pipeline lookahead: speculatively fetch the next N chunks. ---
        // Fire-and-forget — their results go into chunk_cache; we don't await them here.
        if !to_fetch.is_empty() {
            let last_required_idx = needed.last().map(|(i, _, _)| *i).unwrap_or(0);
            let lookahead = engine.pipeline_lookahead(
                last_required_idx, chunk_map.len(),
                &self.chunk_cache, &chunk_map,
            ).await;

            for (la_idx, la_cid) in lookahead {
                let loc = &chunk_map[la_idx];
                let (primary, fallbacks) = match InodeReadEngine::resolve_primary(
                    loc, &nim, &nodes, selector + la_idx as u64,
                ) {
                    Some(pf) => pf,
                    None => {
                        engine.in_flight.lock().await.remove(&la_cid);
                        continue;
                    }
                };
                let client = self.clone();
                let eng = engine.clone();
                tokio::spawn(async move {
                    let result = client.fetch_chunk_with_fallback(la_cid, primary, &fallbacks).await;
                    match result {
                        Ok(data) => {
                            let arc = Arc::new(data);
                            client.chunk_cache.write().await.put(la_cid, arc);
                        }
                        Err(e) => debug!("Pipeline lookahead fetch failed for {}: {}", la_cid, e),
                    }
                    eng.in_flight.lock().await.remove(&la_cid);
                });
            }
        }

        // --- Fetch required chunks (sequential pipeline for full-chunk sequential reads) ---
        if !to_fetch.is_empty() {
            // Determine if this looks like a sequential full-chunk read.
            // All required chunks must be full (not seeking into the middle),
            // and they must be consecutive in the chunk map.
            let is_sequential_full = {
                let first_idx = needed.first().map(|(i,_,_)| *i).unwrap_or(0);
                needed.iter().enumerate().all(|(n, (ci, _co, cs))| {
                    *ci == first_idx + n && *cs == chunk_map[*ci].size
                })
            };

            let fetch_results: Vec<(usize, ChunkId, Result<Vec<u8>>)> =
            if is_sequential_full && to_fetch.len() > 0 {
                // Use the pipelined path: overlap connection setup with body drain.
                let pipeline_input: Vec<(ChunkId, SocketAddr)> = to_fetch.iter()
                    .map(|(_, cid, primary, _)| (*cid, *primary))
                    .collect();
                let results = self.sequential_pipeline_read(pipeline_input).await;
                to_fetch.iter().zip(results).map(|((idx, cid, _, _), res)| (*idx, *cid, res)).collect()
            } else {
                // Parallel path for random reads — fetch full chunks (cached for future reads).
                let tasks: Vec<_> = to_fetch.iter().map(|(idx, cid, primary, fallbacks)| {
                    let client = self.clone();
                    let idx = *idx;
                    let cid = *cid;
                    let primary = *primary;
                    let fallbacks = fallbacks.clone();
                    tokio::spawn(async move {
                        let data = client.fetch_chunk_with_fallback(cid, primary, &fallbacks).await;
                        (idx, cid, data)
                    })
                }).collect();
                futures::future::join_all(tasks).await.into_iter()
                    .map(|r| r.unwrap_or_else(|e| {
                        let dummy = ChunkId::from_hash([0u8; 32]);
                        (0usize, dummy, Err(anyhow::anyhow!("task panicked: {}", e)))
                    }))
                    .collect()
            };

            // Collect results; cache full chunks; remove from in-flight.
            for (idx, cid, res) in fetch_results {
                let data = res.with_context(|| format!("Failed to fetch chunk {}", cid))?;
                let arc = Arc::new(data);
                if !bypass_cache {
                    self.chunk_cache.write().await.put(cid, Arc::clone(&arc));
                }
                result_chunks.push((idx, arc));
                engine.in_flight.lock().await.remove(&cid);
            }
        }

        // --- Wait for in-flight chunks fetched by concurrent requests ---
        for (idx, cid) in to_wait {
            let data = self.wait_for_chunk_in_cache(cid, &engine).await?;
            result_chunks.push((idx, data));
        }

        // --- Assemble the response ---
        result_chunks.sort_by_key(|(i, _)| *i);

        let mut out = Vec::with_capacity(size);
        for (chunk_idx, data) in &result_chunks {
            let (chunk_start, chunk_size) = chunk_offsets[*chunk_idx];
            let read_start = offset.max(chunk_start);
            let read_end = (offset + size).min(chunk_start + chunk_size);
            if read_end <= read_start { continue; }
            let local_start = read_start - chunk_start;
            let local_end = read_end - chunk_start;
            if local_end > data.len() {
                // Partial/corrupt chunk — return what we have.
                if local_start < data.len() {
                    out.extend_from_slice(&data[local_start..]);
                }
            } else {
                out.extend_from_slice(&data[local_start..local_end]);
            }
        }

        Ok(out)
    }

    /// Fetch with primary then fallbacks sequentially.
    /// Connect timeout is 1s so a dead node fails fast without wasting bandwidth.
    async fn fetch_chunk_with_fallback(
        &self,
        cid: ChunkId,
        primary: SocketAddr,
        fallbacks: &[SocketAddr],
    ) -> Result<Vec<u8>> {
        for &addr in std::iter::once(&primary).chain(fallbacks.iter()) {
            match self.read_chunk_from_server(addr, cid).await {
                Ok(d) => {
                    self.node_health.record_success(addr).await;
                    return Ok(d);
                }
                Err(e) => {
                    let msg = e.to_string();
                    warn!("{} failed for chunk {}: {}", addr, cid, msg);
                    self.node_health.record_failure(addr).await;
                    if msg.contains("permanently missing") || msg.contains("blocklisted")
                        || msg.contains("location not found")
                    {
                        anyhow::bail!("Chunk {} is permanently missing", cid);
                    }
                }
            }
        }
        anyhow::bail!("All replicas failed for chunk {}", cid)
    }

    /// Poll chunk_cache for up to 1s waiting for a concurrent fetch to complete.
    /// Exits early if the in-flight entry disappears (fetch failed on the other side).
    async fn wait_for_chunk_in_cache(
        &self,
        cid: ChunkId,
        engine: &InodeReadEngine,
    ) -> Result<Arc<Vec<u8>>> {
        for _ in 0..20 {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            {
                let cache = self.chunk_cache.read().await;
                if let Some(data) = cache.peek(&cid) {
                    return Ok(Arc::clone(data));
                }
            }
            // If the other fetcher removed it from in-flight without caching, it failed.
            let inf = engine.in_flight.lock().await;
            if !inf.contains(&cid) {
                break;
            }
        }
        // Fall back — fetch ourselves.
        warn!("Timeout waiting for concurrent fetch of chunk {}, fetching directly", cid);
        let nodes = self.cluster_nodes.read().await.clone();
        let primary = nodes[0];
        let fallbacks: Vec<SocketAddr> = nodes[1..].to_vec();
        let data = self.fetch_chunk_with_fallback(cid, primary, &fallbacks).await?;
        Ok(Arc::new(data))
    }

    /// Refresh the engine's chunk map from the leader.
    /// `from_chunk` is the first chunk index the reader currently needs; the server
    /// returns a window of CHUNK_MAP_WINDOW chunks starting there so the response
    /// stays small even for multi-hour recordings.
    pub async fn refresh_engine(
        &self,
        engine: &InodeReadEngine,
        file_id: FileId,
        file_size: u64,
        from_chunk: u32,
    ) {
        use std::sync::atomic::Ordering;
        if engine.refresh_in_progress.compare_exchange(
            false, true, Ordering::AcqRel, Ordering::Relaxed,
        ).is_err() {
            return;
        }
        self.refresh_engine_flagged(engine, file_id, file_size, from_chunk).await;
    }

    /// Like `refresh_engine` but assumes the caller already set `refresh_in_progress = true`.
    /// Used by the open() prefetch which sets the flag synchronously before spawning.
    pub async fn refresh_engine_flagged(
        &self,
        engine: &InodeReadEngine,
        file_id: FileId,
        file_size: u64,
        from_chunk: u32,
    ) {
        use std::sync::atomic::Ordering;
        let nim: std::collections::HashMap<dfs_common::NodeId, SocketAddr> = {
            let addr_map = self.addr_to_node_id.read().await;
            addr_map.iter().map(|(&addr, &id)| (id, addr)).collect()
        };

        // Fetch all chunks from the current read position to EOF.
        // u32::MAX tells the server "give me everything from from_chunk onward".
        // This ensures any forward seek — even 10+ minutes ahead — has valid chunk
        // locations without a second round-trip. Backward seeks still trigger a
        // re-fetch (needs_refresh detects out-of-window), but those are rare.
        const CHUNK_MAP_WINDOW: u32 = u32::MAX;
        match self.get_file_chunk_map(file_id, from_chunk, CHUNK_MAP_WINDOW).await {
            Ok((locs, window_from, total_chunks, _)) if !locs.is_empty() => {
                info!("refresh_engine: inode={} got {} chunks (from={} total={}) from leader",
                      engine.inode, locs.len(), window_from, total_chunks);
                engine.clear_failed_refresh();
                engine.update_chunk_map_window(locs, window_from, total_chunks, Arc::new(nim), file_size).await;
            }
            Ok(_) | Err(_) => {
                info!("refresh_engine: inode={} no chunk map from leader", engine.inode);
                engine.record_failed_refresh();
            }
        }

        engine.refresh_in_progress.store(false, Ordering::Release);
    }

    /// Notify the read engine that a new chunk was appended by the write path.
    /// Called from fuse_impl after a successful WriteData completes.  Does not block
    /// writers — just bumps the engine's known_size so the next read triggers a refresh.
    pub fn invalidate_read_engine(&self, inode: u64) {
        if let Some(engine) = self.read_engines.engines.get(&inode) {
            // Set known_size to 0 so needs_refresh() returns true on the next read.
            engine.known_size.store(0, Ordering::Relaxed);
        }
    }

    /// all_file_chunks: Complete list of chunk IDs for the file (for prefetch - can be same as chunk_ids)
    /// start_chunk_idx: Index in all_file_chunks where chunk_ids[0] is located
    /// inode: File inode for byte-range caching (optional, 0 to disable)
    /// chunk_offsets: File byte offset for each chunk in chunk_ids (for byte-range caching)
    pub async fn read_data(
        &self,
        read_hints: &[ChunkReadHint],
        all_file_chunks: &[ChunkId],
        inode: u64,
        chunk_locations: &[dfs_common::ChunkLocation],
    ) -> Result<Vec<u8>> {
        if read_hints.is_empty() {
            return Ok(Vec::new());
        }

        // NOTE: We do NOT deduplicate reads at the byte-offset level here
        // FUSE issues multiple small reads (131KB) within the same 4MB chunk
        // Each needs to return its own data slice
        // Deduplication only happens for CHUNK-LEVEL tracking (prefetch/history)

        // Extract chunk_ids and offsets for compatibility with existing code
        let chunk_ids: Vec<ChunkId> = read_hints.iter().map(|h| h.chunk_id).collect();
        let chunk_offsets: Vec<u64> = read_hints.iter().map(|h| h.file_offset).collect();
        let start_chunk_idx = read_hints.first().map(|h| h.chunk_idx).unwrap_or(0);

        // Log the read request with byte offsets and chunk IDs for debugging
        if !chunk_offsets.is_empty() && chunk_offsets[0] > 0 {
            info!("READ: inode={} byte_offset={} chunk_count={} first_chunk={:?} partial_reads={}",
                  inode, chunk_offsets[0], chunk_ids.len(), chunk_ids.first(),
                  read_hints.iter().filter(|h| !h.full_chunk).count());
        }

        let start = std::time::Instant::now();
        let t0 = start;

        // Detect if we're in sequential access mode by checking read history
        // For sequential reads (DVR streaming), use single-node reads for best HDD performance
        // For random access, use striped reads for lower latency
        let is_sequential = if !all_file_chunks.is_empty() {
            let file_id = all_file_chunks[0];
            let history = self.read_history.read().await;
            if let Some(positions) = history.peek(&file_id) {
                if positions.len() >= 2 {
                    let mut sequential_count = 0;
                    for i in 1..positions.len() {
                        let prev = positions[i - 1];
                        let curr = positions[i];
                        if curr > prev && curr <= prev + 30 {
                            sequential_count += 1;
                        }
                    }
                    sequential_count >= 1
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        let t1 = start.elapsed(); // after sequential detection

        // Check byte-range cache first (for live DVR files), then chunk cache
        // Also track in-flight reads to prevent duplicate concurrent fetches
        // CRITICAL: Use separate lock acquisitions to reduce contention on fast CPUs
        let mut cached_chunks: Vec<(usize, Arc<Vec<u8>>)> = Vec::new();
        let mut chunks_to_fetch: Vec<(usize, ChunkId, u64, bool)> = Vec::new(); // (idx, chunk_id, file_offset, pipeline_only)
        let mut chunks_to_wait_for: Vec<(usize, ChunkId, u64)> = Vec::new(); // chunks being fetched by another request

        for (idx, chunk_id) in chunk_ids.iter().enumerate() {
            let mut found = false;

            // Check chunk cache first — it's the primary cache for pipeline reads.
            {
                let chunk_hit = {
                    let chunk_cache = self.chunk_cache.read().await;
                    chunk_cache.peek(chunk_id).map(|data| (idx, Arc::clone(data)))
                };
                if let Some(cached) = chunk_hit {
                    cached_chunks.push(cached);
                    found = true;
                }
            }

            // Only check byte-range cache (live DVR segments) if chunk cache missed.
            if !found && inode > 0 && idx < chunk_offsets.len() {
                let requested_offset = chunk_offsets[idx];
                let byte_hit = {
                    let mut byte_cache = self.byte_range_cache.lock().await;
                    let key = ByteRangeCacheKey {
                        inode,
                        file_offset: requested_offset,
                        chunk_id: *chunk_id,
                    };
                    if let Some(cached) = byte_cache.get(&key) {
                        if cached.is_expired() {
                            byte_cache.pop(&key);
                            None
                        } else {
                            info!("Byte-range cache HIT for inode={} offset={}", inode, requested_offset);
                            Some((idx, Arc::clone(&cached.data)))
                        }
                    } else {
                        None
                    }
                };
                if let Some(cached) = byte_hit {
                    cached_chunks.push(cached);
                    found = true;
                }
            }

            // Check if another request is already fetching this chunk - separate lock
            if !found {
                let is_in_flight = {
                    let in_flight = self.prefetch_in_flight.lock().await;
                    in_flight.contains(chunk_id)
                    // in_flight lock released here
                };

                if is_in_flight {
                    let file_offset = if idx < chunk_offsets.len() { chunk_offsets[idx] } else { 0 };
                    info!("Chunk {} already being fetched by another request - will wait", chunk_id);
                    chunks_to_wait_for.push((idx, *chunk_id, file_offset));
                    found = true;
                }
            }

            // Need to fetch - acquire lock only to mark in-flight
            if !found {
                let file_offset = if idx < chunk_offsets.len() { chunk_offsets[idx] } else { 0 };
                info!("Cache MISS for chunk {} (inode={}, offset={}) - will fetch", chunk_id, inode, file_offset);
                chunks_to_fetch.push((idx, *chunk_id, file_offset, false));

                // Mark as in-flight to prevent other concurrent requests from fetching
                {
                    let mut in_flight = self.prefetch_in_flight.lock().await;
                    in_flight.insert(*chunk_id);
                    // in_flight lock released here
                }
            }
        }

        // Pipeline lookahead: whenever we have a cache miss and a chunk map, speculatively
        // fetch the next depth-1 chunks alongside the required one.  This ensures chunk N+1
        // starts transferring while chunk N is being returned to FUSE — no sequential-
        // detection warmup delay.  For random/seek workloads the extra fetches land in
        // cache and are evicted harmlessly; the bandwidth waste is bounded (depth-1 chunks).
        if !chunks_to_fetch.is_empty() && !all_file_chunks.is_empty() {
            let chunk_size_hint = 4 * 1024 * 1024usize; // conservative; real size unknown here
            let depth = Self::pipeline_depth(chunk_size_hint);
            let last_required_file_idx = start_chunk_idx + chunk_ids.len().saturating_sub(1);
            let lookahead_needed = depth.saturating_sub(chunk_ids.len());

            if lookahead_needed > 0 {
                let mut pipeline_chunks: Vec<ChunkId> = Vec::with_capacity(lookahead_needed);
                {
                    let cache = self.chunk_cache.read().await;
                    let in_flight = self.prefetch_in_flight.lock().await;
                    let mut file_idx = last_required_file_idx + 1;
                    while pipeline_chunks.len() < lookahead_needed && file_idx < all_file_chunks.len() {
                        let cid = all_file_chunks[file_idx];
                        if cache.peek(&cid).is_none() && !in_flight.contains(&cid) {
                            pipeline_chunks.push(cid);
                        }
                        file_idx += 1;
                    }
                }
                // Mark pipeline chunks as in-flight and add to fetch list
                {
                    let mut in_flight = self.prefetch_in_flight.lock().await;
                    for cid in &pipeline_chunks {
                        in_flight.insert(*cid);
                    }
                }
                for cid in pipeline_chunks {
                    chunks_to_fetch.push((usize::MAX, cid, 0, true));
                }
            }
        }

        let t2 = start.elapsed(); // after cache lookup loop
        let cache_hits = cached_chunks.len();
        let cache_misses = chunks_to_fetch.iter().filter(|(_, _, _, po)| !po).count();

        info!("Reading {} chunks: {} cached, {} to fetch ({} pipeline lookahead) (chunk_ids: {:?})",
              chunk_ids.len(), cache_hits, cache_misses,
              chunks_to_fetch.iter().filter(|(_, _, _, po)| *po).count(),
              chunk_ids);

        // Fast path: all chunks were in cache, skip all fetch machinery.
        let mut fetched_chunks: Vec<(usize, Arc<Vec<u8>>)> = Vec::new();
        if !chunks_to_fetch.is_empty() {

        let nodes = self.cluster_nodes.read().await.clone();
        let chunk_loc_map: std::collections::HashMap<ChunkId, &dfs_common::ChunkLocation> =
            chunk_locations.iter().map(|loc| (loc.chunk_id, loc)).collect();

        // Create parallel fetch tasks with concurrency limit
        // CRITICAL: Use a SHARED semaphore (stored on self) so concurrent read_data calls
        // from parallel FUSE reads don't each get their own 20-slot budget. Without sharing,
        // a seek with N parallel FUSE reads opens N*20 simultaneous connections and exhausts
        // server file descriptors.
        let max_concurrent_fetches = self.fetch_semaphore.clone();

        // --- Step 1: resolve replicas and select primary node for each chunk. ---
        // Done upfront (sequentially, but all data is local after the first read)
        // so we can branch between pipelined-sequential and parallel-random paths.
        struct ResolvedFetch {
            idx: usize,
            chunk_id: ChunkId,
            file_offset: u64,
            pipeline_only: bool,
            use_partial_read: bool,
            primary: SocketAddr,
            fallbacks: Vec<SocketAddr>, // other replicas, excluding primary
        }

        let node_id_map = self.addr_to_node_id.read().await.clone();
        let mut resolved: Vec<ResolvedFetch> = Vec::with_capacity(chunks_to_fetch.len());

        for (idx, chunk_id, file_offset, pipeline_only) in &chunks_to_fetch {
            let idx = *idx;
            let chunk_id = *chunk_id;
            let file_offset = *file_offset;
            let pipeline_only = *pipeline_only;

            // Resolve replica list from chunk_locations (fast, no network).
            let mut replicas = if let Some(loc) = chunk_loc_map.get(&chunk_id) {
                let addrs: Vec<SocketAddr> = loc.nodes.iter()
                    .filter_map(|nid| node_id_map.iter()
                        .find(|(_, &id)| id == *nid)
                        .map(|(&addr, _)| addr))
                    .collect();
                if !addrs.is_empty() { addrs } else { Vec::new() }
            } else {
                Vec::new()
            };

            if replicas.is_empty() {
                // Fall back to replica cache or metadata query.
                let cached = { self.replica_cache.lock().await.get(&chunk_id).cloned() };
                replicas = if let Some(c) = cached {
                    (*c).clone()
                } else {
                    nodes.clone()
                };
            }

            // Check warm server cache.
            let warm_node = {
                self.warm_cache_map.lock().await.get(&chunk_id).and_then(|(addr, ts)| {
                    if ts.elapsed().as_secs() < 60 { Some(*addr) } else { None }
                })
            };

            // Select primary.
            let primary = if let Some(w) = warm_node {
                if replicas.contains(&w) { w } else {
                    self.select_replica(&replicas).await.unwrap_or(nodes[0])
                }
            } else {
                self.select_replica(&replicas).await.unwrap_or(nodes[0])
            };

            // Determine partial-read flag.
            let use_partial_read = if pipeline_only {
                false
            } else {
                read_hints.iter().find(|h| h.chunk_id == chunk_id)
                    .map(|h| !h.full_chunk && !is_sequential)
                    .unwrap_or(false)
            };

            let fallbacks: Vec<SocketAddr> = replicas.iter()
                .filter(|&&a| a != primary)
                .copied()
                .collect();

            resolved.push(ResolvedFetch { idx, chunk_id, file_offset, pipeline_only,
                                          use_partial_read, primary, fallbacks });
        }

        // --- Step 2: fetch chunks. ---
        // For sequential full-chunk reads: use sequential_pipeline_read so connection
        // setup for chunk N+1 overlaps with body transfer of chunk N.
        // For random / partial reads: fire parallel tasks (original behaviour).
        let has_partial = resolved.iter().any(|r| r.use_partial_read);

        let fetch_results: Vec<Result<(usize, ChunkId, u64, Arc<Vec<u8>>, bool, bool)>> =
        if !has_partial && !all_file_chunks.is_empty() {
            // Build ordered list for the pipeline (primary node per chunk).
            let pipeline_input: Vec<(ChunkId, SocketAddr)> = resolved.iter()
                .map(|r| (r.chunk_id, r.primary))
                .collect();

            let pipeline_results = self.sequential_pipeline_read(pipeline_input).await;

            // Map results back to the common tuple format, retrying fallbacks on failure.
            futures::future::join_all(pipeline_results.into_iter().zip(resolved.iter()).map(|(res, r)| {
                let client = self.clone();
                async move {
                    let data = match res {
                        Ok(d) => d,
                        Err(e) => {
                            warn!("Pipeline read failed for chunk {}, trying {} fallback(s): {}",
                                  r.chunk_id, r.fallbacks.len(), e);
                            let mut fallback_data = None;
                            let mut last_err = e;
                            for &fb_addr in &r.fallbacks {
                                match client.read_chunk_from_server(fb_addr, r.chunk_id).await {
                                    Ok(d) => { fallback_data = Some(d); break; }
                                    Err(e) => { last_err = e; }
                                }
                            }
                            match fallback_data {
                                Some(d) => d,
                                None => return Err(last_err.context(format!("pipeline read chunk {} (all replicas failed)", r.chunk_id))),
                            }
                        }
                    };
                    info!("✓ Chunk {} via pipeline ({} bytes)", r.chunk_id, data.len());
                    Ok((r.idx, r.chunk_id, r.file_offset, Arc::new(data), false, r.pipeline_only))
                }
            })).await
        } else {
            // Original parallel path.
            let tasks: Vec<_> = resolved.into_iter().map(|r| {
                let client = self.clone();
                let semaphore = max_concurrent_fetches.clone();
                let read_hint = read_hints.iter().find(|h| h.chunk_id == r.chunk_id).cloned();

                tokio::spawn(async move {
                    let _permit = semaphore.acquire().await.unwrap();
                    let all_nodes = std::iter::once(r.primary)
                        .chain(r.fallbacks.iter().copied());
                    let mut last_error = None;
                    let mut data = None;

                    for (i, node_addr) in all_nodes.enumerate() {
                        let read_start = std::time::Instant::now();
                        let result = if r.use_partial_read {
                            let hint = read_hint.as_ref().unwrap();
                            info!("PARTIAL READ: chunk {} offset={} length={}", r.chunk_id, hint.offset_in_chunk, hint.length);
                            client.read_chunk_range_from_server(node_addr, r.chunk_id,
                                hint.offset_in_chunk as u64, hint.length as u64).await
                        } else {
                            client.read_chunk_from_server(node_addr, r.chunk_id).await
                        };
                        match result {
                            Ok(d) => {
                                info!("✓ Chunk {} from {} ({}) in {:?} - {} bytes",
                                      r.chunk_id, node_addr,
                                      if i > 0 { "FALLBACK" } else { "PRIMARY" },
                                      read_start.elapsed(), d.len());
                                data = Some(d);
                                break;
                            }
                            Err(e) => { last_error = Some(e); }
                        }
                    }
                    let chunk_data = data.ok_or_else(||
                        last_error.unwrap_or_else(|| anyhow::anyhow!("All nodes failed")))?;
                    Ok::<_, anyhow::Error>((r.idx, r.chunk_id, r.file_offset,
                                           Arc::new(chunk_data), r.use_partial_read, r.pipeline_only))
                })
            }).collect();

            // Wait for all parallel fetches to complete.
            futures::future::join_all(tasks).await
                .into_iter()
                .map(|r| r.context("Fetch task panicked").and_then(|x| x))
                .collect()
        };

        // Process results and update both caches
        for result in fetch_results {
            let (idx, chunk_id, file_offset, data_arc, was_partial, is_pipeline_only) = result
                .context("Failed to fetch chunk")?;

            // Only store FULL chunks in the chunk cache keyed by chunk_id.
            // A partial read (ReadChunkRange) fetches only a byte slice of the chunk.
            // Caching that slice under the full chunk ID would corrupt any subsequent
            // read that expects the complete chunk (e.g. read-modify-write splice).
            // Partial results are still stored in the byte-range cache below, which
            // is keyed by (inode, offset) and is safe for partial use.
            if !was_partial {
                let mut chunk_cache = self.chunk_cache.write().await;
                chunk_cache.put(chunk_id, Arc::clone(&data_arc));
                debug!("Cached chunk {} ({} bytes)", chunk_id, data_arc.len());
            }

            // Add to byte-range cache if we have inode (skip pipeline-only — no valid file_offset).
            // Note: file_offset == 0 is valid (first chunk of file) and should be cached.
            if inode > 0 && !is_pipeline_only {
                let mut byte_cache = self.byte_range_cache.lock().await;
                let key = ByteRangeCacheKey {
                    inode,
                    file_offset,
                    chunk_id,
                };
                let cached = CachedChunk {
                    data: Arc::clone(&data_arc),
                    chunk_size: data_arc.len(),
                    cached_at: std::time::Instant::now(),
                };
                byte_cache.put(key, cached);
                info!("Byte-range cached: inode={} offset={} ({} bytes)", inode, file_offset, data_arc.len());
            }

            // Pipeline-only chunks are cached but not returned to the caller.
            if !is_pipeline_only {
                fetched_chunks.push((idx, data_arc));
            }
        }

        // Remove from in-flight now that fetches are complete.
        {
            let mut in_flight = self.prefetch_in_flight.lock().await;
            for (_, chunk_id, _, _) in &chunks_to_fetch {
                in_flight.remove(chunk_id);
            }
        }

        } // end if !chunks_to_fetch.is_empty()

        // Wait for chunks that were already being fetched by other requests
        // Poll the cache until they appear (they should be there very soon)
        if !chunks_to_wait_for.is_empty() {
            info!("Waiting for {} chunks already being fetched by other requests", chunks_to_wait_for.len());

            for (idx, chunk_id, file_offset) in chunks_to_wait_for {
                let wait_start = std::time::Instant::now();
                let mut data_found = false;

                // Poll for up to 3s (60 attempts @ 50ms each).
                // SBC spinning disks can take 300-500ms for a cold read; 200ms was
                // too short and caused spurious timeouts that then failed the fallback
                // fetch as well (both requests racing for the same replica).
                for attempt in 0..60 {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

                    // Check chunk cache
                    let chunk_cache = self.chunk_cache.read().await;
                    if let Some(data) = chunk_cache.peek(&chunk_id) {
                        debug!("Waited chunk {} now available after {:?}", chunk_id, wait_start.elapsed());
                        fetched_chunks.push((idx, Arc::clone(data)));
                        data_found = true;
                        break;
                    }
                    drop(chunk_cache);

                    if attempt % 20 == 0 && attempt > 0 {
                        debug!("Still waiting for chunk {} ({}ms elapsed)", chunk_id, attempt * 50);
                    }
                }

                if !data_found {
                    // This shouldn't happen - another request said it was fetching
                    // But if it does, fall back to fetching ourselves
                    warn!("Timeout waiting for chunk {} being fetched by another request, fetching ourselves", chunk_id);

                    // Try to fetch it ourselves, trying multiple replicas if needed
                    let replicas = self.cluster_nodes.read().await.clone();

                    let selected_replica = self.select_replica(&replicas).await
                        .context("No replicas available for fallback fetch")?;

                    // Try selected replica first, then fall back to others
                    let mut fetch_succeeded = false;
                    for (i, node_addr) in std::iter::once(&selected_replica)
                        .chain(replicas.iter().filter(|&n| n != &selected_replica))
                        .enumerate()
                    {
                        match self.read_chunk_from_server(*node_addr, chunk_id).await {
                            Ok(data) => {
                                if i > 0 {
                                    debug!("Fetched chunk {} from fallback replica {} after timeout", chunk_id, node_addr);
                                }
                                let data_arc = Arc::new(data);

                                // Cache it
                                let mut chunk_cache = self.chunk_cache.write().await;
                                chunk_cache.put(chunk_id, Arc::clone(&data_arc));

                                fetched_chunks.push((idx, data_arc));
                                fetch_succeeded = true;
                                break;
                            }
                            Err(e) => {
                                debug!("Failed to fetch chunk {} from {} after timeout: {}", chunk_id, node_addr, e);
                                continue;
                            }
                        }
                    }

                    if !fetch_succeeded {
                        anyhow::bail!("Failed to fetch chunk {} from any replica after timeout", chunk_id);
                    }
                }
            }
        }

        // Combine cached and fetched chunks
        let mut all_chunks: Vec<(usize, Arc<Vec<u8>>)> = Vec::new();
        all_chunks.extend(cached_chunks);
        all_chunks.extend(fetched_chunks);

        // Sort by index to maintain chunk order
        all_chunks.sort_by_key(|(idx, _)| *idx);

        // Find the highest index we accessed (within the local array)
        let last_local_idx = all_chunks.iter().map(|(idx, _)| *idx).max().unwrap_or(0);

        // Concatenate all chunks
        let mut all_data = Vec::new();
        for (_, data) in all_chunks {
            all_data.extend_from_slice(&data);
        }

        let elapsed = start.elapsed();
        let throughput = (all_data.len() as f64 / 1024.0 / 1024.0) / elapsed.as_secs_f64();
        info!("Read complete: {} bytes in {:?} ({:.2} MB/s) t1={:?} t2={:?} fetch={:?}",
              all_data.len(), elapsed, throughput, t1, t2 - t1, elapsed - t2);

        // Detect sequential access patterns and prefetch aggressively
        // Prefetch on every read to keep the server cache warm ahead of our position
        if !chunk_ids.is_empty() && !all_file_chunks.is_empty() {
            let last_file_chunk_idx = start_chunk_idx + last_local_idx;
            let file_id = all_file_chunks[0]; // Use first chunk as file identifier

            // Detect sequential access patterns for prefetch decisions
            // read_history has its own Mutex, no outer lock needed
            let is_sequential = {
                // Track read history and detect sequential patterns
                let mut history = self.read_history.write().await;
                // LRU cache: get existing or create new entry
                if !history.contains(&file_id) {
                    history.put(file_id, VecDeque::with_capacity(4));
                }
                let read_positions = history.get_mut(&file_id).unwrap();

                // Add current read position only if it differs from the last recorded one.
                // FUSE issues many 128KB reads within a single 4MB chunk — without this
                // dedup the history fills with identical positions and sequential detection
                // never fires until we've already crossed into the second chunk.
                if read_positions.back() != Some(&last_file_chunk_idx) {
                    read_positions.push_back(last_file_chunk_idx);
                    if read_positions.len() > 4 {
                        read_positions.pop_front();
                    }
                }

                // Detect if we have sequential momentum (2+ consecutive sequential reads)
                let result = if read_positions.len() >= 2 {
                    let mut sequential_count = 0;
                    for i in 1..read_positions.len() {
                        let prev = read_positions[i - 1];
                        let curr = read_positions[i];
                        // Consider sequential if moving forward within 30 chunks
                        // With DIRECT_IO and large chunks (4MB), FUSE may skip ahead in 128KB increments
                        // resulting in gaps of 10-20 chunks during sequential playback
                        if curr > prev && curr <= prev + 30 {
                            sequential_count += 1;
                        }
                    }
                    let is_seq = sequential_count >= 1; // Need at least 1 sequential step

                    // Log detection result with chunk indices
                    debug!("Sequential detection: chunk_idx={} history={:?} sequential={}",
                           last_file_chunk_idx, read_positions, is_seq);

                    is_seq
                } else {
                    false // Not enough history yet
                };

                drop(history); // Release history lock
                // read_guard released here automatically
                result
            };

        }

        Ok(all_data)
    }

    /// Query the leader for the full chunk location map of a file.
    /// Returns (locations, modified_at). Falls back to any node if leader is unknown.
    pub async fn get_file_chunk_map(&self, file_id: FileId, from_chunk: u32, count: u32) -> Result<(Vec<dfs_common::ChunkLocation>, u32, u32, u64)> {
        let target = {
            let leader = self.leader_addr.read().await;
            match *leader {
                Some(addr) => addr,
                None => {
                    let nodes = self.cluster_nodes.read().await;
                    *nodes.first().context("No cluster nodes available")?
                }
            }
        };

        let request = Request::GetFileChunkMap { file_id, from_chunk, count };
        let response = self.send_request(target, request).await;

        let response = match response {
            Ok(r) => r,
            Err(e) => {
                warn!("GetFileChunkMap to leader failed ({}), retrying any node", e);
                let nodes = self.cluster_nodes.read().await.clone();
                let mut last_err = e;
                let mut found = None;
                for addr in &nodes {
                    if *addr == target { continue; }
                    match self.send_request(*addr, Request::GetFileChunkMap { file_id, from_chunk, count }).await {
                        Ok(r) => { found = Some(r); break; }
                        Err(e) => { last_err = e; }
                    }
                }
                found.ok_or(last_err)?
            }
        };

        match response {
            Response::FileChunkMap { locations, from_chunk, total_chunks, modified_at, .. } => {
                Ok((locations, from_chunk, total_chunks, modified_at))
            }
            Response::Error { message, .. } => anyhow::bail!("GetFileChunkMap error: {}", message),
            _ => anyhow::bail!("Unexpected response to GetFileChunkMap"),
        }
    }

    /// Select one replica from a list using round-robin for load balancing.
    /// Penalized nodes are moved to the back so healthy nodes are preferred.
    async fn select_replica(&self, replicas: &[SocketAddr]) -> Option<SocketAddr> {
        if replicas.is_empty() {
            return None;
        }
        let ordered = self.node_health.sort_by_health(replicas).await;
        let idx = self.replica_selector.fetch_add(1, Ordering::Relaxed) as usize % ordered.len();
        Some(ordered[idx])
    }

    /// Pre-populate replica cache with chunk locations for upcoming reads
    /// This is called when reading file metadata to warm the cache for sequential reads
    /// For now, we use a simple heuristic: all nodes have all chunks (true for RF=2 with 5 nodes)
    /// In the future, this could query the metadata server for actual locations
    ///
    /// Parameters:
    /// - chunk_ids: All chunks in the file
    /// - current_chunk_idx: Current chunk index (optional, for smart warming)
    pub async fn warm_replica_cache_by_index(&self, chunk_ids: &[ChunkId], current_chunk_idx: Option<usize>) {
        if chunk_ids.is_empty() {
            return;
        }

        // Determine which chunks to warm
        let (start_idx, end_idx) = if let Some(idx) = current_chunk_idx {
            // Smart warming: sliding window ahead of current read position
            // Warm next 1000 chunks (~600MB for typical DVR chunks)
            // This creates a sliding window that prevents metadata query storms
            // for large sequential files while keeping memory usage low (<100KB)
            let start = idx.min(chunk_ids.len());
            let end = (idx + 1000).min(chunk_ids.len());
            (start, end)
        } else {
            // No offset provided, warm first 1000 chunks (for new file opens)
            (0, 1000.min(chunk_ids.len()))
        };

        if start_idx >= end_idx {
            return;
        }

        let chunks_to_warm = &chunk_ids[start_idx..end_idx];
        let nodes = self.cluster_nodes.read().await.clone();
        let nodes_arc = Arc::new(nodes);

        let mut cache = self.replica_cache.lock().await;
        let mut warmed = 0;
        for chunk_id in chunks_to_warm {
            // Only add if not already in cache
            if !cache.contains(chunk_id) {
                cache.put(*chunk_id, Arc::clone(&nodes_arc));
                warmed += 1;
            }
        }

        info!("Warmed replica cache: {} new entries (range {}-{} of {} total chunks)",
              warmed, start_idx, end_idx, chunk_ids.len());
    }

    /// Pre-populate replica cache with chunk locations for upcoming reads (byte offset version)
    /// Parameters:
    /// - chunk_ids: All chunks in the file
    /// - current_offset: Current read position in bytes (optional, for smart warming)
    /// - chunk_size: Size of each chunk in bytes (for calculating chunk index from offset)
    pub async fn warm_replica_cache_range(&self, chunk_ids: &[ChunkId], current_offset: Option<u64>, chunk_size: u64) {
        let current_chunk_idx = current_offset.map(|offset| (offset / chunk_size) as usize);
        self.warm_replica_cache_by_index(chunk_ids, current_chunk_idx).await;
    }

    /// Legacy wrapper for warming cache without offset info
    pub async fn warm_replica_cache(&self, chunk_ids: &[ChunkId]) {
        // Assume 2MB chunks for legacy calls
        self.warm_replica_cache_range(chunk_ids, None, 2 * 1024 * 1024).await;
    }

    /// Warm replica cache from actual ChunkLocation data — uses real per-chunk node
    /// lists instead of the fake "all nodes" entries that warm_replica_cache_by_index
    /// produces.  This eliminates mid-read get_chunk_replicas RPCs on saturated links.
    pub async fn warm_replica_cache_from_locations(
        &self,
        locations: &[dfs_common::ChunkLocation],
        current_chunk_idx: Option<usize>,
    ) {
        if locations.is_empty() {
            return;
        }

        let start = current_chunk_idx.unwrap_or(0).min(locations.len());
        let end = (start + 1000).min(locations.len());

        // Build NodeId -> SocketAddr from the cluster node list directly.
        // This is authoritative and doesn't depend on addr_to_node_id being populated yet.
        let node_id_to_addr: std::collections::HashMap<dfs_common::NodeId, SocketAddr> = {
            let nodes = self.cluster_nodes.read().await;
            let addr_map = self.addr_to_node_id.read().await;
            // Primary: invert addr_to_node_id (populated by refresh_cluster_nodes).
            // Fallback: if addr_to_node_id is empty (first read before first refresh),
            // use cluster_nodes directly paired with GetClusterStatus NodeIds if available.
            if !addr_map.is_empty() {
                addr_map.iter().map(|(&addr, &id)| (id, addr)).collect()
            } else {
                // addr_to_node_id not yet populated — can't map NodeIds to addrs.
                // Return empty; replica_cache will miss and fall back to all-nodes.
                std::collections::HashMap::new()
            }
        };

        // If we have no mapping yet, fall back to all-nodes warmup so we at least
        // have something rather than empty cache entries.
        if node_id_to_addr.is_empty() {
            let chunk_ids: Vec<ChunkId> = locations[start..end].iter()
                .map(|l| l.chunk_id)
                .collect();
            self.warm_replica_cache_by_index(&chunk_ids, Some(0)).await;
            return;
        }

        let mut cache = self.replica_cache.lock().await;
        let mut warmed = 0usize;
        let mut with_real_nodes = 0usize;
        let nodes_fallback = {
            let nodes = self.cluster_nodes.read().await;
            Arc::new(nodes.clone())
        };

        for loc in &locations[start..end] {
            if cache.contains(&loc.chunk_id) {
                continue;
            }
            let addrs: Vec<SocketAddr> = loc.nodes.iter()
                .filter_map(|nid| node_id_to_addr.get(nid).copied())
                .collect();
            if !addrs.is_empty() {
                cache.put(loc.chunk_id, Arc::new(addrs));
                with_real_nodes += 1;
            } else {
                // NodeId not in map (node removed, or stale location) — use all nodes.
                cache.put(loc.chunk_id, Arc::clone(&nodes_fallback));
            }
            warmed += 1;
        }
        if warmed > 0 {
            info!("Warmed replica cache: {} new entries (range {}-{} of {} total chunks, {} with real node mapping)",
                  warmed, start, end, locations.len(), with_real_nodes);
        }
    }

    /// Read a single chunk from a specific server using connection pooling
    async fn read_chunk_from_server(&self, server_addr: SocketAddr, chunk_id: ChunkId) -> Result<Vec<u8>> {
        let request = Request::ReadChunk {
            chunk_id,
            sequential_hint: None, // TODO: Pass sequential hint when available
        };

        // Try using pooled connection first, with fallback to new connection
        let mut attempt = 0;
        loop {
            attempt += 1;

            // Get or create connection (pop from per-server VecDeque)
            // Clone Arc out before .await to avoid holding DashMap shard lock across await.
            let stream = {
                let mutex_opt = self.connection_pool.get(&server_addr).map(|e| Arc::clone(&*e));
                if let Some(mutex) = mutex_opt {
                    mutex.lock().await.pop_front()
                } else {
                    None
                }
            };

            let mut stream = match stream {
                Some(s) => {
                    let mut buf = [0u8; 1];
                    let peer_closed = match s.try_read(&mut buf) {
                        Ok(0) => true,
                        Ok(_) => true,
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
                        Err(_) => true,
                    };
                    if peer_closed {
                        debug!("Pooled connection to {} closed by peer, reconnecting", server_addr);
                        let mut s = s;
                        let _ = s.shutdown().await;
                        None // fall through to create new connection
                    } else {
                        debug!("Reusing pooled connection to {}", server_addr);
                        Some(s)
                    }
                }
                None => None,
            };

            let mut stream = match stream {
                Some(s) => s,
                None => {
                    debug!("Creating new connection to {}", server_addr);
                    tokio::time::timeout(
                        tokio::time::Duration::from_secs(5),
                        TcpStream::connect(server_addr),
                    ).await
                        .map_err(|_| anyhow::anyhow!("Connect timeout to {}", server_addr))?
                        .context("Failed to connect to server")?
                }
            };

            let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
            let envelope = MessageEnvelope::new(request_id, Message::Request(request.clone()));
            let encoded = envelope.to_bytes().context("Failed to serialize message")?;

            // Send request and read response with 3-second timeout
            let io_future = async {
                // Send request
                let len = encoded.len() as u32;
                stream.write_all(&len.to_be_bytes()).await?;
                stream.write_all(&encoded).await?;
                stream.flush().await?;

                // Read response
                let mut len_buf = [0u8; 4];
                stream.read_exact(&mut len_buf).await?;
                let len = u32::from_be_bytes(len_buf) as usize;

                let mut buf = vec![0u8; len];
                stream.read_exact(&mut buf).await?;

                Ok::<(TcpStream, Vec<u8>), std::io::Error>((stream, buf))
            };

            let result = tokio::time::timeout(
                tokio::time::Duration::from_secs(1),
                io_future
            ).await;

            let result = match result {
                Ok(r) => r,
                Err(_) => {
                    // Timeout occurred
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("Timeout reading chunk from {}", server_addr)
                    ))
                }
            };

            match result {
                Ok((mut stream, buf)) => {
                    // Deserialize envelope first (before returning stream to pool).
                    let response_envelope = MessageEnvelope::from_bytes(&buf)
                        .context("Failed to deserialize response")?;

                    match response_envelope.message {
                        Message::Response(Response::ChunkData { data, cache_stats, .. }) => {
                            // Split-frame: read raw payload before returning stream to pool.
                            let data = if data.is_empty() {
                                dfs_common::protocol::read_chunk_payload(&mut stream).await
                                    .context("read split-frame chunk payload")?
                            } else {
                                data
                            };

                            // Return connection to pool now that we've drained all bytes.
                            // Clone Arc out before .await to avoid holding DashMap shard lock.
                            {
                                let mutex = {
                                    let entry = self.connection_pool
                                        .entry(server_addr)
                                        .or_insert_with(|| Arc::new(Mutex::new(std::collections::VecDeque::new())));
                                    Arc::clone(&*entry)
                                };
                                let mut queue = mutex.lock().await;
                                if queue.len() < 8 {
                                    queue.push_back(stream);
                                }
                            }

                            // Flow control: Check server cache pressure and throttle if needed
                            if let Some((_, capacity, size)) = cache_stats {
                                let utilization = (size as f64 / capacity as f64) * 100.0;
                                if utilization > 90.0 {
                                    let sleep_ms = ((utilization - 90.0) * 2.0) as u64;
                                    debug!("Server {} cache pressure: {:.1}% ({}/{}), throttling {}ms",
                                           server_addr, utilization, size, capacity, sleep_ms);
                                    tokio::time::sleep(tokio::time::Duration::from_millis(sleep_ms)).await;
                                }
                            }
                            return Ok(data);
                        },
                        Message::Response(Response::Error { message, .. }) => {
                            anyhow::bail!("Server error: {}", message);
                        }
                        _ => anyhow::bail!("Unexpected response type"),
                    }
                }
                Err(e) => {
                    // Connection failed - don't return to pool
                    warn!("Connection to {} failed (attempt {}): {}", server_addr, attempt, e);

                    // Retry once with new connection if this was a pooled connection
                    if attempt == 1 {
                        debug!("Retrying with new connection to {}", server_addr);
                        continue;
                    } else {
                        return Err(e).context("Failed to read chunk after retry");
                    }
                }
            }
        }
    }

    /// Phase 1 of pipelined sequential reads: open connection, send ReadChunk request,
    /// and read the 4-byte response-length prefix.  Returns the open stream plus the
    /// declared response body length so the caller can drain the body separately.
    ///
    /// By running Phase 1 for chunk N+1 concurrently with draining chunk N we hide
    /// TCP connection setup + server processing latency behind the data transfer.
    async fn open_chunk_request(
        &self,
        server_addr: SocketAddr,
        chunk_id: ChunkId,
    ) -> Result<(TcpStream, usize)> {
        let request = Request::ReadChunk { chunk_id, sequential_hint: None };
        let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
        let envelope = MessageEnvelope::new(request_id, Message::Request(request));
        let encoded = envelope.to_bytes().context("serialize")?;

        // Prefer a pooled connection; fall back to a new one.
        // Clone Arc out before .await to avoid holding DashMap shard lock across await.
        let pooled = {
            let mutex_opt = self.connection_pool.get(&server_addr).map(|e| Arc::clone(&*e));
            if let Some(mutex) = mutex_opt {
                mutex.lock().await.pop_front()
            } else {
                None
            }
        };

        let mut stream = match pooled {
            Some(s) => {
                let mut buf = [0u8; 1];
                let peer_closed = !matches!(s.try_read(&mut buf), Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock);
                if peer_closed {
                    tokio::time::timeout(
                        tokio::time::Duration::from_secs(5),
                        TcpStream::connect(server_addr),
                    ).await.map_err(|_| anyhow::anyhow!("connect timeout"))??
                } else {
                    s
                }
            }
            None => tokio::time::timeout(
                tokio::time::Duration::from_secs(5),
                TcpStream::connect(server_addr),
            ).await.map_err(|_| anyhow::anyhow!("connect timeout"))??,
        };

        // Send request + read 4-byte length prefix (tiny, fast).
        let len = encoded.len() as u32;
        let write_result = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            async {
                stream.write_all(&len.to_be_bytes()).await?;
                stream.write_all(&encoded).await?;
                stream.flush().await
            },
        ).await.map_err(|_| anyhow::anyhow!("Timeout sending request to {}", server_addr))?;
        write_result?;

        let mut len_buf = [0u8; 4];
        tokio::time::timeout(
            tokio::time::Duration::from_secs(30),
            stream.read_exact(&mut len_buf),
        ).await
            .map_err(|_| anyhow::anyhow!("Timeout reading length prefix from {}", server_addr))??;
        let body_len = u32::from_be_bytes(len_buf) as usize;

        Ok((stream, body_len))
    }

    /// Phase 2 of pipelined sequential reads: drain the response body from a stream
    /// that has already completed Phase 1 (open_chunk_request).  Returns the chunk
    /// data and hands the stream back to the connection pool.
    async fn drain_chunk_response(
        &self,
        server_addr: SocketAddr,
        mut stream: TcpStream,
        body_len: usize,
    ) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; body_len];
        // 30s per chunk body — enough for a 4MB chunk on a slow link, not forever on a hung node.
        tokio::time::timeout(
            tokio::time::Duration::from_secs(30),
            stream.read_exact(&mut buf),
        ).await
            .map_err(|_| anyhow::anyhow!("Timeout draining chunk body from {}", server_addr))??;

        let response_envelope = MessageEnvelope::from_bytes(&buf).context("deserialize")?;
        let data = match response_envelope.message {
            Message::Response(Response::ChunkData { data, .. }) => {
                if data.is_empty() {
                    // Split-frame: raw payload follows on the stream — read before pooling.
                    tokio::time::timeout(
                        tokio::time::Duration::from_secs(30),
                        dfs_common::protocol::read_chunk_payload(&mut stream),
                    ).await
                        .map_err(|_| anyhow::anyhow!("Timeout reading split-frame payload from {}", server_addr))?
                        .context("read split-frame chunk payload")?
                } else {
                    data
                }
            }
            Message::Response(Response::Error { message, .. }) => {
                anyhow::bail!("Server error: {}", message)
            }
            _ => anyhow::bail!("Unexpected response type"),
        };

        // Return connection to pool after all bytes are drained.
        // Clone Arc out before .await to avoid holding DashMap shard lock across await.
        {
            let mutex = {
                let entry = self.connection_pool
                    .entry(server_addr)
                    .or_insert_with(|| Arc::new(Mutex::new(std::collections::VecDeque::new())));
                Arc::clone(&*entry)
            };
            let mut queue = mutex.lock().await;
            if queue.len() < 8 {
                queue.push_back(stream);
            }
        }

        Ok(data)
    }

    /// Sequential pipeline read: fetch `chunks` one at a time but with the *next*
    /// connection already established and request already sent before the current
    /// chunk's body has finished transferring.  Eliminates per-chunk TCP + RTT latency
    /// from the critical path.
    ///
    /// Returns chunk data in the same order as `chunks`.  On any Phase-1 error falls
    /// back to the normal `read_chunk_from_server` path for that chunk.
    pub async fn sequential_pipeline_read(
        &self,
        chunks: Vec<(ChunkId, SocketAddr)>,
    ) -> Vec<Result<Vec<u8>>> {
        if chunks.is_empty() {
            return Vec::new();
        }

        let mut results: Vec<Result<Vec<u8>>> = Vec::with_capacity(chunks.len());

        type P1Handle = tokio::task::JoinHandle<Result<(TcpStream, usize)>>;

        // Kick off Phase 1 for the first chunk immediately.
        let mut pending: Option<(SocketAddr, P1Handle)> = {
            let (cid, addr) = chunks[0]; // ChunkId and SocketAddr are Copy
            let client = self.clone();
            Some((addr, tokio::spawn(async move {
                client.open_chunk_request(addr, cid).await
            })))
        };

        for i in 0..chunks.len() {
            let (cid, addr) = chunks[i];

            let (p1_addr, p1_handle) = match pending.take() {
                Some(p) => p,
                None => {
                    results.push(self.read_chunk_from_server(addr, cid).await);
                    continue;
                }
            };

            // Concurrently start Phase 1 for the next chunk while we await drain of this one.
            let next_pending: Option<(SocketAddr, P1Handle)> = if i + 1 < chunks.len() {
                let (next_cid, next_addr) = chunks[i + 1];
                let client = self.clone();
                Some((next_addr, tokio::spawn(async move {
                    client.open_chunk_request(next_addr, next_cid).await
                })))
            } else {
                None
            };

            // Await Phase 1 completion then drain the body.
            let chunk_result = match p1_handle.await {
                Ok(Ok((stream, body_len))) => {
                    self.drain_chunk_response(p1_addr, stream, body_len).await
                }
                Ok(Err(e)) => {
                    warn!("Pipeline Phase-1 failed for chunk {:?} on {}: {}", cid, p1_addr, e);
                    self.read_chunk_from_server(addr, cid).await
                }
                Err(e) => Err(anyhow::anyhow!("Phase-1 task panicked: {}", e)),
            };

            results.push(chunk_result);
            pending = next_pending;
        }

        results
    }

    /// Send prefetch hint to server (fire-and-forget, non-blocking)
    /// Server will warm these chunks into its page cache
    /// Read a byte range from a specific server (for striped multi-replica reads)
    async fn read_chunk_range_from_server(
        &self,
        server_addr: SocketAddr,
        chunk_id: ChunkId,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>> {
        let request = Request::ReadChunkRange { chunk_id, offset, length };
        let response = tokio::time::timeout(
            tokio::time::Duration::from_secs(1),
            self.send_request(server_addr, request),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Timeout reading chunk range from {}", server_addr))??;

        match response {
            Response::ChunkData { data, .. } => Ok(data),
            Response::Error { message, .. } => {
                anyhow::bail!("Server {} returned error for byte range: {}", server_addr, message)
            }
            _ => anyhow::bail!("Unexpected response from server {}", server_addr),
        }
    }

    /// Read chunk using striped multi-replica approach (parallel byte ranges from multiple nodes)
    async fn read_chunk_striped(
        &self,
        chunk_id: ChunkId,
        location: &dfs_common::ChunkLocation,
        file_offset: u64,
    ) -> Result<Vec<u8>> {
        let chunk_size = location.size;

        // Map NodeIds to SocketAddrs
        let node_id_map = self.addr_to_node_id.read().await;
        let node_addrs: Vec<SocketAddr> = location.nodes.iter()
            .filter_map(|node_id| {
                node_id_map.iter()
                    .find(|(_, &id)| id == *node_id)
                    .map(|(&addr, _)| addr)
            })
            .take(2)  // Only use first 2 replicas for striping
            .collect();
        drop(node_id_map);

        if node_addrs.is_empty() {
            // None of the chunk_locations nodes exist in current cluster
            // Fall back to normal replica discovery path
            warn!("Striped read requested but none of the chunk_location nodes are in current cluster, falling back to all nodes");

            let replicas = self.cluster_nodes.read().await.clone();

            for node_addr in &replicas {
                match self.read_chunk_from_server(*node_addr, chunk_id).await {
                    Ok(data) => return Ok(data),
                    Err(e) => {
                        debug!("Failed to read chunk from {}: {}", node_addr, e);
                        continue;
                    }
                }
            }

            anyhow::bail!("Failed to read chunk {} from any node", chunk_id);
        }

        if node_addrs.len() < 2 {
            // Only 1 address available, use single-node read
            warn!("Striped read requested but only 1 address available, falling back to single-node");
            return self.read_chunk_from_server(node_addrs[0], chunk_id).await;
        }

        let node1 = node_addrs[0];
        let node2 = node_addrs[1];

        // Split chunk in half
        let mid_point = chunk_size / 2;
        let first_half_size = mid_point;
        let second_half_size = chunk_size - mid_point;

        debug!("Striped read: chunk {} ({} bytes) from node1={} (0-{}) + node2={} ({}-{})",
               chunk_id, chunk_size, node1, first_half_size, node2, mid_point, chunk_size);

        // Fetch both halves in parallel
        let client1 = self.clone();
        let client2 = self.clone();

        let task1 = tokio::spawn(async move {
            client1.read_chunk_range_from_server(node1, chunk_id, 0, first_half_size as u64).await
        });

        let task2 = tokio::spawn(async move {
            client2.read_chunk_range_from_server(node2, chunk_id, mid_point as u64, second_half_size as u64).await
        });

        let (result1, result2) = tokio::join!(task1, task2);

        // Handle results
        let first_half = result1
            .context("Task1 panicked")??;
        let second_half = result2
            .context("Task2 panicked")??;

        // Reassemble data
        let mut combined = Vec::with_capacity(chunk_size);
        combined.extend_from_slice(&first_half);
        combined.extend_from_slice(&second_half);

        debug!("Striped read complete: chunk {} ({} + {} = {} bytes)",
               chunk_id, first_half.len(), second_half.len(), combined.len());

        Ok(combined)
    }

    /// Read a single chunk by ID, resolving node IDs to addresses.
    /// Used to re-read the partial last chunk when re-aligning a write buffer after an interrupted append.
    pub async fn read_chunk_by_id(&self, chunk_id: ChunkId, node_ids: &[dfs_common::NodeId]) -> Result<Vec<u8>> {
        // Resolve NodeIds to SocketAddrs
        let node_id_map = self.addr_to_node_id.read().await;
        let mut node_addrs: Vec<SocketAddr> = node_ids.iter()
            .filter_map(|node_id| {
                node_id_map.iter()
                    .find(|(_, &id)| id == *node_id)
                    .map(|(&addr, _)| addr)
            })
            .collect();
        drop(node_id_map);

        if node_addrs.is_empty() {
            node_addrs = self.cluster_nodes.read().await.clone();
        }

        for addr in &node_addrs {
            match self.read_chunk_from_server(*addr, chunk_id).await {
                Ok(data) => return Ok(data),
                Err(e) => debug!("read_chunk_by_id: failed from {}: {}", addr, e),
            }
        }

        anyhow::bail!("read_chunk_by_id: failed to read chunk {} from any node", chunk_id)
    }

    /// Broadcast a ChunkLocation to every cluster node.
    /// The leader gets reliable delivery with exponential-backoff retries (up to ~30s).
    /// Followers get fire-and-forget — they learn about the chunk from the server-side
    /// ReplicateChunkLocation handler anyway, and the healer reconciles any gaps.
    fn broadcast_chunk_location(&self, location: dfs_common::ChunkLocation, all_nodes: Vec<SocketAddr>) {
        let leader_addr = {
            // Snapshot leader addr synchronously; we're not in an async context here.
            // If the RwLock is uncontended this is instant; worst case we skip retry for
            // this one call — the healer will catch up.
            self.leader_addr.try_read().ok().and_then(|g| *g)
        };

        for &addr in &all_nodes {
            let client = self.clone();
            let loc = location.clone();
            let is_leader = Some(addr) == leader_addr;
            tokio::spawn(async move {
                let req = Request::ReplicateChunkLocation { location: loc };
                if is_leader {
                    // Retry to the leader with exponential backoff so the chunk map stays
                    // current even if the leader is momentarily slow.
                    let mut backoff_ms = 500u64;
                    for attempt in 1u32..=6 {
                        match tokio::time::timeout(
                            Duration::from_secs(3),
                            client.send_request(addr, req.clone()),
                        ).await {
                            Ok(Ok(_)) => return,
                            Ok(Err(e)) => warn!(
                                "ReplicateChunkLocation to leader {} failed (attempt {}): {}",
                                addr, attempt, e
                            ),
                            Err(_) => warn!(
                                "ReplicateChunkLocation to leader {} timed out (attempt {})",
                                addr, attempt
                            ),
                        }
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms * 2).min(30_000);
                    }
                    warn!("ReplicateChunkLocation to leader {} gave up after 6 attempts", addr);
                } else {
                    if let Err(e) = client.send_request(addr, req).await {
                        debug!("Failed to replicate chunk location to {}: {}", addr, e);
                    }
                }
            });
        }
    }

    /// Write data with synchronous dual-replica replication
    /// NEW: Writes each chunk to 2 nodes synchronously (not striped)
    /// Returns chunk_locations with replica tracking
    pub async fn write_data_dual_replica(&self, data: &[u8], inode: u64, file_offset: u64) -> Result<Vec<dfs_common::ChunkLocation>> {
        let nodes = self.cluster_nodes.read().await.clone();
        if nodes.len() < 2 {
            anyhow::bail!("Need at least 2 nodes for writes (only {} available)", nodes.len());
        }

        // Rotate the preferred pair by chunk index so each 4MB chunk lands on a different
        // node pair. Without this, all chunks of a file go to nodes[0]+nodes[1].
        let chunk_idx = (file_offset / (4 * 1024 * 1024)) as usize;
        let preferred1 = nodes[chunk_idx % nodes.len()];
        let preferred2 = nodes[(chunk_idx + 1) % nodes.len()];

        info!("Writing {} bytes with synchronous dual-replica (preferred: {}, {})",
              data.len(), preferred1, preferred2);

        let chunk_locations = self.write_chunk_to_replicas(data, preferred1, preferred2, inode, file_offset, &nodes).await?;

        // Log which nodes were actually used
        if let Some(loc) = chunk_locations.first() {
            let node_id_map = self.addr_to_node_id.read().await;
            let rev: std::collections::HashMap<_, _> = node_id_map.iter().map(|(a, id)| (id, a)).collect();
            let n1 = loc.nodes.first().and_then(|id| rev.get(id)).map(|a| a.to_string()).unwrap_or_default();
            let n2 = loc.nodes.get(1).and_then(|id| rev.get(id)).map(|a| a.to_string()).unwrap_or_default();
            info!("Dual-replica write complete: {} chunks stored on {} and {}", chunk_locations.len(), n1, n2);
            drop(node_id_map);
        }

        // write_chunk_to_replicas already delivered ChunkLocation to leader (sync) and
        // followers (async). No second broadcast needed here.

        // Populate byte-range cache for immediate read-back
        if inode > 0 {
            let mut byte_cache = self.byte_range_cache.lock().await;
            let mut current_offset = file_offset;

            for (idx, location) in chunk_locations.iter().enumerate() {
                let chunk_start = if idx == 0 { 0 } else {
                    chunk_locations[..idx].iter().map(|l| l.size as u64).sum::<u64>() as usize
                };
                let chunk_end = chunk_start + location.size;
                let chunk_data = data[chunk_start..chunk_end].to_vec();

                let key = ByteRangeCacheKey {
                    inode,
                    file_offset: current_offset,
                    chunk_id: location.chunk_id,
                };
                let cached = CachedChunk {
                    data: Arc::new(chunk_data),
                    chunk_size: location.size,
                    cached_at: std::time::Instant::now(),
                };
                byte_cache.put(key, cached);

                current_offset += location.size as u64;
            }
        }

        Ok(chunk_locations)
    }


    /// Write a single chunk to 2 replica nodes, with fallback to other nodes if either fails.
    /// The server healer is responsible for lazily replicating to additional nodes up to RF.
    async fn write_chunk_to_replicas(
        &self,
        data: &[u8],
        replica1: SocketAddr,
        replica2: SocketAddr,
        inode: u64,
        file_offset: u64,
        all_nodes: &[SocketAddr],
    ) -> Result<Vec<dfs_common::ChunkLocation>> {
        const WRITE_TIMEOUT_SECS: u64 = 30;

        // Build the ordered list of candidates: preferred pair first, then others as fallbacks.
        // Sort so penalized nodes are tried last — healthy nodes get first crack at quorum.
        //
        // IMPORTANT: We must not write to a penalized node AND its healthy replacement in the same
        // parallel round, or we end up with 3+ replicas on disk (the penalized write completes late).
        // Fix: promote healthy rest nodes ahead of penalized preferred nodes in the candidate list,
        // so the parallel pair is always 2 healthy nodes when healthy alternatives exist.
        let preferred: Vec<SocketAddr> = vec![replica1, replica2];
        let mut rest: Vec<SocketAddr> = all_nodes.iter().copied()
            .filter(|&n| n != replica1 && n != replica2)
            .collect();

        let sorted_preferred = self.node_health.sort_by_health(&preferred).await;
        let sorted_rest = self.node_health.sort_by_health(&rest).await;
        rest = sorted_rest;

        // Count how many of the preferred pair are healthy (not penalized)
        let mut healthy_preferred: Vec<SocketAddr> = Vec::new();
        let mut penalized_preferred: Vec<SocketAddr> = Vec::new();
        for &n in &sorted_preferred {
            if self.node_health.is_penalized(n).await {
                penalized_preferred.push(n);
            } else {
                healthy_preferred.push(n);
            }
        }

        // Build candidates: healthy preferred first, then healthy rest (to fill any gaps from
        // penalized preferred), then penalized preferred last (only used if no other choice).
        let mut candidates: Vec<SocketAddr> = Vec::new();
        candidates.extend_from_slice(&healthy_preferred);
        // Fill up to 2 with healthy rest nodes before falling back to penalized preferred
        for &n in &rest {
            if candidates.len() >= 2 { break; }
            if !penalized_preferred.contains(&n) {
                candidates.push(n);
            }
        }
        // Append penalized preferred (fallback only)
        candidates.extend_from_slice(&penalized_preferred);
        // Append remaining rest nodes not yet in candidates
        for &n in &rest {
            if !candidates.contains(&n) {
                candidates.push(n);
            }
        }

        // Try the preferred pair in parallel first — halves write latency on the hot path.
        // If either fails, fall back to serial retries from the remaining candidates.
        let mut successful: Vec<(SocketAddr, Response)> = Vec::new();

        if candidates.len() >= 2 {
            let n1 = candidates[0];
            let n2 = candidates[1];
            let req1 = Request::WriteFileLocalOnly { data: data.to_vec(), file_offset };
            let req2 = Request::WriteFileLocalOnly { data: data.to_vec(), file_offset };
            let t1 = tokio::time::timeout(
                tokio::time::Duration::from_secs(WRITE_TIMEOUT_SECS),
                self.send_request(n1, req1),
            );
            let t2 = tokio::time::timeout(
                tokio::time::Duration::from_secs(WRITE_TIMEOUT_SECS),
                self.send_request(n2, req2),
            );
            let (r1, r2) = tokio::join!(t1, t2);
            match r1 {
                Ok(Ok(resp)) => { debug!("Parallel replica write succeeded to {}", n1); successful.push((n1, resp)); }
                Ok(Err(e))   => { warn!("Parallel replica write failed: {}: {}, will retry serially", n1, e); }
                Err(_)       => { warn!("Parallel replica write failed: {}: timeout after {}s, will retry serially", n1, WRITE_TIMEOUT_SECS); }
            }
            match r2 {
                Ok(Ok(resp)) => { debug!("Parallel replica write succeeded to {}", n2); successful.push((n2, resp)); }
                Ok(Err(e))   => { warn!("Parallel replica write failed: {}: {}, will retry serially", n2, e); }
                Err(_)       => { warn!("Parallel replica write failed: {}: timeout after {}s, will retry serially", n2, WRITE_TIMEOUT_SECS); }
            }
        }

        // Serial fallback for any missing replicas
        let mut candidate_iter = candidates.iter().skip(if successful.len() == 2 { candidates.len() } else { 2 });
        while successful.len() < 2 {
            let node = match candidate_iter.next() {
                Some(n) => *n,
                None => anyhow::bail!(
                    "Chunk write failed: could not get 2 replicas after trying all {} nodes",
                    candidates.len()
                ),
            };

            let request = Request::WriteFileLocalOnly { data: data.to_vec(), file_offset };
            let result = tokio::time::timeout(
                tokio::time::Duration::from_secs(WRITE_TIMEOUT_SECS),
                self.send_request(node, request),
            ).await;

            match result {
                Ok(Ok(response)) => {
                    debug!("Chunk replica write succeeded to {}", node);
                    successful.push((node, response));
                }
                Ok(Err(e)) => {
                    warn!("Chunk replica write to {} failed: {}, trying next node", node, e);
                }
                Err(_) => {
                    warn!("Chunk replica write to {} timed out after {}s, trying next node",
                          node, WRITE_TIMEOUT_SECS);
                }
            }
        }

        let (addr1, response1) = successful.remove(0);
        let (addr2, response2) = successful.remove(0);

        let (chunk_ids_1, chunk_sizes_1) = match response1 {
            Response::ChunkIds { chunk_ids, chunk_sizes, .. } => (chunk_ids, chunk_sizes),
            Response::Error { message, .. } => anyhow::bail!("Replica 1 ({}) failed: {}", addr1, message),
            _ => anyhow::bail!("Unexpected response from replica 1 ({})", addr1),
        };

        let (chunk_ids_2, _) = match response2 {
            Response::ChunkIds { chunk_ids, chunk_sizes, .. } => (chunk_ids, chunk_sizes),
            Response::Error { message, .. } => anyhow::bail!("Replica 2 ({}) failed: {}", addr2, message),
            _ => anyhow::bail!("Unexpected response from replica 2 ({})", addr2),
        };

        if chunk_ids_1.len() != chunk_ids_2.len() {
            anyhow::bail!("Replica mismatch: {} chunks vs {} chunks", chunk_ids_1.len(), chunk_ids_2.len());
        }

        // Create ChunkLocation entries with the 2 nodes that received the data.
        // The server healer will lazily replicate to additional nodes up to RF.
        let node_id_map = self.addr_to_node_id.read().await;
        let mut chunk_locations = Vec::new();

        let mut current_offset = file_offset;
        for (idx, chunk_id) in chunk_ids_1.iter().enumerate() {
            if chunk_id != &chunk_ids_2[idx] {
                warn!("Chunk ID mismatch at index {}: {} vs {}", idx, chunk_id, chunk_ids_2[idx]);
            }

            let node1_id = Self::resolve_node_id(&node_id_map, addr1);
            let node2_id = Self::resolve_node_id(&node_id_map, addr2);

            let chunk_size = chunk_sizes_1[idx] as usize;
            let location = dfs_common::ChunkLocation {
                chunk_id: *chunk_id,
                nodes: vec![node1_id, node2_id],
                size: chunk_size,
                checksum: chunk_id.hash,
                file_offset: Some(current_offset),
                written_at: None,
            };

            chunk_locations.push(location);
            current_offset += chunk_size as u64;
        }
        drop(node_id_map);

        // Deliver ChunkLocation to the leader synchronously before returning to the caller.
        // This guarantees the leader has the chunk map entry before the client's write ACK
        // lands — so any subsequent read (even from another node that queries the leader)
        // sees the chunk. Non-leader nodes get it fire-and-forget — a slow/unresponsive
        // follower must not stall the write pipeline; the leader's dissemination covers misses.
        let all_nodes_snapshot = all_nodes.to_vec();
        let leader_addr = *self.leader_addr.read().await;
        for location in chunk_locations.iter().cloned() {
            let mut leader_task = None;
            for &addr in &all_nodes_snapshot {
                let client = self.clone();
                let loc = location.clone();
                let is_leader = Some(addr) == leader_addr;
                let task = tokio::spawn(async move {
                    let req = Request::ReplicateChunkLocation { location: loc.clone() };
                    match tokio::time::timeout(Duration::from_secs(2), client.send_request(addr, req.clone())).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) if is_leader => {
                            // One retry for the leader, also bounded, so a flaky leader
                            // can't hold up the write pipeline indefinitely.
                            warn!("ChunkLocation to leader {} failed: {}, retrying once", addr, e);
                            if let Err(e2) = tokio::time::timeout(
                                Duration::from_secs(2),
                                client.send_request(addr, req),
                            ).await.unwrap_or_else(|_| Err(anyhow::anyhow!("timeout"))) {
                                warn!("ChunkLocation to leader {} retry also failed: {}", addr, e2);
                            }
                        }
                        Ok(Err(e)) => debug!("ChunkLocation to {} failed (leader will catch up): {}", addr, e),
                        Err(_) => warn!("ChunkLocation to {} timed out", addr),
                    }
                });
                if is_leader {
                    leader_task = Some(task);
                }
                // Non-leader tasks are detached — fire-and-forget.
            }
            // Only await the leader; followers run in background.
            if let Some(t) = leader_task {
                let _ = t.await;
            }
        }

        // Populate byte-range cache for immediate read-back
        if inode > 0 {
            let mut byte_cache = self.byte_range_cache.lock().await;
            let mut current_offset = file_offset;

            for (idx, location) in chunk_locations.iter().enumerate() {
                let chunk_start = if idx == 0 { 0 } else {
                    chunk_locations[..idx].iter().map(|l| l.size as u64).sum::<u64>() as usize
                };
                let chunk_end = chunk_start + location.size;
                let chunk_data = data[chunk_start..chunk_end].to_vec();

                let key = ByteRangeCacheKey {
                    inode,
                    file_offset: current_offset,
                    chunk_id: location.chunk_id,
                };
                let cached = CachedChunk {
                    data: Arc::new(chunk_data),
                    chunk_size: location.size,
                    cached_at: std::time::Instant::now(),
                };
                byte_cache.put(key, cached);

                current_offset += location.size as u64;
            }
        }

        Ok(chunk_locations)
    }

    /// Resolve a SocketAddr to a NodeId using the addr→id map.
    /// The map is populated by refresh_cluster_nodes() using the peer_addr each server
    /// advertises (non-wildcard), so exact matches should always succeed in a healthy
    /// cluster. The fallback logs a warning so we notice if something goes wrong.
    fn resolve_node_id(
        node_id_map: &HashMap<SocketAddr, dfs_common::NodeId>,
        addr: SocketAddr,
    ) -> dfs_common::NodeId {
        if let Some(&id) = node_id_map.get(&addr) {
            return id;
        }
        warn!("addr_to_node_id: no entry for {} — falling back to hash-derived NodeId", addr);
        Self::node_id_from_addr(addr)
    }

    /// Helper to create a NodeId from SocketAddr
    /// For now, we create a deterministic UUID from the address
    /// TODO: Store actual NodeId mappings from cluster discovery
    fn node_id_from_addr(addr: SocketAddr) -> dfs_common::NodeId {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        // Create deterministic hash from address
        let mut hasher = DefaultHasher::new();
        addr.to_string().hash(&mut hasher);
        let hash = hasher.finish();

        // Convert to UUID bytes (simple approach for now)
        let uuid_bytes = [
            (hash >> 56) as u8,
            (hash >> 48) as u8,
            (hash >> 40) as u8,
            (hash >> 32) as u8,
            (hash >> 24) as u8,
            (hash >> 16) as u8,
            (hash >> 8) as u8,
            hash as u8,
            0, 0, 0, 0, 0, 0, 0, 0, // Pad to 16 bytes
        ];

        let uuid = uuid::Uuid::from_bytes(uuid_bytes);
        dfs_common::NodeId::from_uuid(uuid)
    }

    /// Append data to a file using the server-side AppendFile RPC.
    /// The server handles chunk alignment: it reads back the partial last chunk if
    /// needed, writes complete chunks + new partial tail, and returns updated metadata.
    ///
    /// `preferred_primary`: if Some, try this node first (for write load distribution —
    /// caller rotates the primary when remaining_in_chunk hits 0).
    ///
    /// Returns (updated_metadata, remaining_in_chunk). When remaining_in_chunk == 0
    /// the chunk boundary was just crossed — caller should pick a new primary.
    pub async fn append_file(
        &self,
        file_id: dfs_common::FileId,
        data: Vec<u8>,
        expected_offset: u64,
        preferred_primary: Option<SocketAddr>,
    ) -> Result<(dfs_common::FileMetadata, u64, SocketAddr)> {
        use dfs_common::protocol::{ErrorCode, Request, Response};

        let nodes = self.cluster_nodes.read().await.clone();
        let sorted = self.node_health.sort_by_health(&nodes).await;

        // Build candidate list: preferred primary first (if healthy), then rest in health order.
        let mut candidates: Vec<SocketAddr> = Vec::new();
        if let Some(preferred) = preferred_primary {
            if sorted.iter().take(2).any(|n| *n == preferred) {
                candidates.push(preferred);
            }
        }
        for n in &sorted {
            if !candidates.contains(n) {
                candidates.push(*n);
            }
        }
        if candidates.is_empty() {
            anyhow::bail!("No cluster nodes available for AppendFile");
        }

        let mut last_err = anyhow::anyhow!("AppendFile: no candidates tried");

        for primary in candidates {
            let request = Request::AppendFile { file_id, data: data.clone(), expected_offset };

            let response = match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.send_request(primary, request),
            ).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    self.node_health.record_failure(primary).await;
                    last_err = anyhow::anyhow!("AppendFile send failed on {}: {}", primary, e);
                    warn!("AppendFile: node {} failed (send error), trying next: {}", primary, last_err);
                    continue;
                }
                Err(_) => {
                    self.node_health.record_failure(primary).await;
                    last_err = anyhow::anyhow!("AppendFile timeout on {}", primary);
                    warn!("AppendFile: node {} timed out, trying next", primary);
                    continue;
                }
            };

            match response {
                Response::AppendFileResult { metadata, remaining_in_chunk } => {
                    self.node_health.record_success(primary).await;
                    return Ok((metadata, remaining_in_chunk, primary));
                }
                Response::Error { message, code: ErrorCode::OffsetMismatch } => {
                    // CAS mismatch — no point retrying other nodes, caller must re-fetch
                    anyhow::bail!("OffsetMismatch: {}", message);
                }
                Response::Error { message, .. } => {
                    self.node_health.record_failure(primary).await;
                    last_err = anyhow::anyhow!("AppendFile server error on {}: {}", primary, message);
                    warn!("AppendFile: node {} returned error, trying next: {}", primary, last_err);
                    // continue to next candidate
                }
                other => anyhow::bail!("Unexpected response from AppendFile: {:?}", other),
            }
        }

        Err(last_err)
    }

    /// Write data and populate byte-range cache for immediate read-back
    /// This enables zero-latency reads of just-written data (DVR use case)
    /// Returns (chunk_ids, chunk_sizes, chunk_locations) - locations include full replica node tracking
    pub async fn write_data_with_cache(&self, data: &[u8], inode: u64, file_offset: u64) -> Result<(Vec<ChunkId>, Vec<u64>, Option<Vec<dfs_common::ChunkLocation>>)> {
        let nodes = self.cluster_nodes.read().await.clone();
        if nodes.len() < 2 {
            // Single-node cluster: fall back to server-side replication
            let (chunk_ids, chunk_sizes, replica_nodes_per_chunk) = self.write_data_single_chunk_tracked(data).await?;
            let locations = Self::build_chunk_locations_from_ids(&chunk_ids, &chunk_sizes, file_offset, replica_nodes_per_chunk);
            return Ok((chunk_ids, chunk_sizes, Some(locations)));
        }

        let chunk_locations = self.write_data_dual_replica(data, inode, file_offset).await?;

        // Extract chunk IDs and sizes for backward compatibility
        let chunk_ids: Vec<ChunkId> = chunk_locations.iter().map(|loc| loc.chunk_id).collect();
        let chunk_sizes: Vec<u64> = chunk_locations.iter().map(|loc| loc.size as u64).collect();

        Ok((chunk_ids, chunk_sizes, Some(chunk_locations)))
    }

    /// Write a chunk to a specific server
    async fn write_chunk_to_server(server_addr: SocketAddr, data: Vec<u8>) -> Result<(Vec<ChunkId>, Vec<u64>)> {
        let total_start = std::time::Instant::now();
        let data_len = data.len();

        let request = Request::WriteFile { data };

        // Create connection
        let connect_start = std::time::Instant::now();
        let mut stream = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            TcpStream::connect(server_addr),
        ).await
            .map_err(|_| anyhow::anyhow!("Connect timeout to {}", server_addr))?
            .context("Failed to connect to server")?;
        let connect_time = connect_start.elapsed();

        let serialize_start = std::time::Instant::now();
        let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
        let envelope = MessageEnvelope::new(request_id, Message::Request(request));
        let encoded = envelope.to_bytes().context("Failed to serialize message")?;
        let serialize_time = serialize_start.elapsed();

        // Send request
        let send_start = std::time::Instant::now();
        let len = encoded.len() as u32;
        stream.write_all(&len.to_be_bytes()).await?;
        stream.write_all(&encoded).await?;
        stream.flush().await?;
        let send_time = send_start.elapsed();

        // Read response
        let recv_start = std::time::Instant::now();
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await?;
        let recv_time = recv_start.elapsed();

        let deserialize_start = std::time::Instant::now();
        let response_envelope = MessageEnvelope::from_bytes(&buf)
            .context("Failed to deserialize response")?;
        let deserialize_time = deserialize_start.elapsed();

        let total_time = total_start.elapsed();
        let throughput = (data_len as f64 / 1024.0 / 1024.0) / total_time.as_secs_f64();
        info!("Client write to {}: {} bytes in {:?} ({:.2} MB/s) - connect: {:?}, serialize: {:?}, send: {:?}, recv: {:?}, deserialize: {:?}",
              server_addr, data_len, total_time, throughput, connect_time, serialize_time, send_time, recv_time, deserialize_time);

        match response_envelope.message {
            Message::Response(Response::ChunkIds { chunk_ids, chunk_sizes, .. }) => Ok((chunk_ids, chunk_sizes)),
            Message::Response(Response::Error { message, .. }) => {
                anyhow::bail!("Server error: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Write a chunk to a specific server (local only, no replication)
    /// Used for optimized RF=3+ writes
    async fn write_chunk_to_server_local_only(server_addr: SocketAddr, data: Vec<u8>, file_offset: u64) -> Result<(Vec<ChunkId>, Vec<u64>)> {
        let total_start = std::time::Instant::now();
        let data_len = data.len();

        let request = Request::WriteFileLocalOnly { data, file_offset };

        // Create connection
        let mut stream = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            TcpStream::connect(server_addr),
        ).await
            .map_err(|_| anyhow::anyhow!("Connect timeout to {}", server_addr))?
            .context("Failed to connect to server")?;

        let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
        let envelope = MessageEnvelope::new(request_id, Message::Request(request));
        let encoded = envelope.to_bytes().context("Failed to serialize message")?;

        // Send request
        let len = encoded.len() as u32;
        stream.write_all(&len.to_be_bytes()).await?;
        stream.write_all(&encoded).await?;
        stream.flush().await?;

        // Read response
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await?;

        let response_envelope = MessageEnvelope::from_bytes(&buf)
            .context("Failed to deserialize response")?;

        let total_time = total_start.elapsed();
        let throughput = (data_len as f64 / 1024.0 / 1024.0) / total_time.as_secs_f64();
        info!("Client LOCAL write to {}: {} bytes in {:?} ({:.2} MB/s)",
              server_addr, data_len, total_time, throughput);

        match response_envelope.message {
            Message::Response(Response::ChunkIds { chunk_ids, chunk_sizes, .. }) => Ok((chunk_ids, chunk_sizes)),
            Message::Response(Response::Error { message, .. }) => {
                anyhow::bail!("Server error: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Write small data via single server (old path)
    pub async fn write_data_single_chunk(&self, data: &[u8]) -> Result<(Vec<ChunkId>, Vec<u64>)> {
        let (chunk_ids, chunk_sizes, _) = self.write_data_single_chunk_tracked(data).await?;
        Ok((chunk_ids, chunk_sizes))
    }

    /// Like write_data_single_chunk but also returns per-chunk replica node lists.
    /// The server includes all replica NodeIds in the ChunkIds response.
    async fn write_data_single_chunk_tracked(&self, data: &[u8]) -> Result<(Vec<ChunkId>, Vec<u64>, Vec<Vec<NodeId>>)> {
        let request = Request::WriteFile {
            data: data.to_vec(),
        };

        let nodes = self.cluster_nodes.read().await.clone();
        let mut last_error = None;

        for (i, node_addr) in nodes.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            match self.send_request(*node_addr, request.clone()).await {
                Ok(Response::ChunkIds { chunk_ids, chunk_sizes, replica_nodes_per_chunk }) => {
                    return Ok((chunk_ids, chunk_sizes, replica_nodes_per_chunk));
                }
                Ok(Response::Error { message, .. }) => {
                    anyhow::bail!("Failed to write data: {}", message);
                }
                Ok(_) => anyhow::bail!("Unexpected response type"),
                Err(e) => {
                    warn!("Failed to write to {}: {}", node_addr, e);
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All nodes failed for single-chunk write")))
    }

    /// Apply a small patch to an existing chunk on all replicas without transferring
    /// the full chunk over the network.
    ///
    /// Sends PatchChunk to all-but-last replicas in parallel (fast path, caller blocks).
    /// The last replica is patched in a background task whose JoinHandle is returned so
    /// the caller can await it at a convenient point (next flush or release) without
    /// holding up the write path.
    ///
    /// Protocol:
    /// 1. Server evicts old_chunk_id from pending_healing on receipt of PatchChunk,
    ///    preventing the healer from replicating the old file during the rename window
    /// 2. Send PatchChunk to all known replicas in parallel, get new_chunk_id
    /// 3. Broadcast ReplicateChunkLocation with new_chunk_id to all nodes
    /// 4. Fire-and-forget DeleteChunk (old_chunk_id) to any cluster nodes NOT in the replica set
    ///    — removes stale old chunk from healer-replicated copies on non-replica nodes
    /// 5. Healer sees new_chunk_id under-replicated and copies it to remaining nodes
    ///
    /// Returns the new ChunkLocation.
    pub async fn patch_chunk_on_replicas(
        &self,
        old_chunk_id: ChunkId,
        chunk_file_offset: u64,
        intra_offset: usize,
        patch_data: Vec<u8>,
        old_location: &dfs_common::ChunkLocation,
    ) -> Result<dfs_common::ChunkLocation> {
        // Resolve NodeId -> SocketAddr for the replica nodes
        let node_id_to_addr: HashMap<dfs_common::NodeId, SocketAddr> = {
            let addr_map = self.addr_to_node_id.read().await;
            addr_map.iter().map(|(&addr, &id)| (id, addr)).collect()
        };
        let all_cluster_nodes = self.cluster_nodes.read().await.clone();

        let replica_addrs: Vec<SocketAddr> = if old_location.nodes.is_empty() {
            all_cluster_nodes.clone()
        } else {
            old_location.nodes.iter()
                .filter_map(|nid| node_id_to_addr.get(nid).copied())
                .collect()
        };

        if replica_addrs.is_empty() {
            anyhow::bail!("PatchChunk: no replica addresses resolved for chunk {}", old_chunk_id);
        }

        let addr_to_node_id_snap = self.addr_to_node_id.read().await.clone();

        // Step 2: PatchChunk to all known replicas in parallel
        let patch_req = Request::PatchChunk {
            chunk_id: old_chunk_id,
            chunk_file_offset,
            intra_offset,
            data: patch_data,
        };

        let futures: Vec<_> = replica_addrs.iter().map(|&addr| {
            let client = self.clone();
            let req = patch_req.clone();
            async move { (addr, client.send_request(addr, req).await) }
        }).collect();

        let results = futures::future::join_all(futures).await;

        let mut new_chunk_id: Option<ChunkId> = None;
        let mut new_size: usize = old_location.size;
        let mut patched_node_ids: Vec<dfs_common::NodeId> = Vec::new();

        for (addr, result) in results {
            match result {
                Ok(Response::PatchChunkResult { new_chunk_id: ncid, size }) => {
                    new_chunk_id = Some(ncid);
                    new_size = size;
                    if let Some(&nid) = addr_to_node_id_snap.get(&addr) {
                        patched_node_ids.push(nid);
                    }
                }
                Ok(Response::Error { message, .. }) => {
                    warn!("PatchChunk replica {} error: {}", addr, message);
                }
                Err(e) => {
                    warn!("PatchChunk replica {} failed: {}", addr, e);
                }
                _ => {}
            }
        }

        let new_chunk_id = match new_chunk_id {
            Some(id) => id,
            None => anyhow::bail!("PatchChunk: all replicas failed for chunk {}", old_chunk_id),
        };

        // Step 3: Broadcast new ChunkLocation to all nodes
        let new_location = dfs_common::ChunkLocation {
            chunk_id: new_chunk_id,
            nodes: patched_node_ids.clone(),
            size: new_size,
            checksum: new_chunk_id.hash,
            file_offset: old_location.file_offset,
            written_at: None,
        };
        // Leader gets new ChunkLocation synchronously — guarantees the leader's chunk map
        // is current before we return. Followers get it async via leader's own broadcast.
        let leader_addr = *self.leader_addr.read().await;
        if let Some(leader) = leader_addr {
            let req = Request::ReplicateChunkLocation { location: new_location.clone() };
            let mut backoff_ms = 250u64;
            for attempt in 1u32..=4 {
                match tokio::time::timeout(
                    Duration::from_secs(3),
                    self.send_request(leader, req.clone()),
                ).await {
                    Ok(Ok(_)) => break,
                    Ok(Err(e)) => warn!("PatchChunk: ChunkLocation to leader {} failed (attempt {}): {}", leader, attempt, e),
                    Err(_)    => warn!("PatchChunk: ChunkLocation to leader {} timed out (attempt {})", leader, attempt),
                }
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(4_000);
            }
        }
        for &addr in &all_cluster_nodes {
            if Some(addr) == leader_addr { continue; }
            let client = self.clone();
            let req = Request::ReplicateChunkLocation { location: new_location.clone() };
            tokio::spawn(async move {
                if let Err(e) = client.send_request(addr, req).await {
                    debug!("PatchChunk: location to follower {} failed (leader will catch up): {}", addr, e);
                }
            });
        }

        // Step 4: Delete old chunk from any cluster nodes that are NOT in the replica set.
        // These are nodes where the healer previously copied the old chunk. Deleting it
        // prevents the healer from spreading the stale version; the healer will then see
        // new_chunk_id is under-replicated and copy it to those nodes.
        let replica_addr_set: std::collections::HashSet<SocketAddr> = replica_addrs.into_iter().collect();
        for addr in all_cluster_nodes.iter().filter(|a| !replica_addr_set.contains(a)) {
            let client = self.clone();
            let req = Request::DeleteChunk { chunk_id: old_chunk_id };
            let addr = *addr;
            tokio::spawn(async move {
                if let Err(e) = client.send_request(addr, req).await {
                    debug!("PatchChunk: delete old chunk on {} failed (ok if not present): {}", addr, e);
                }
            });
        }

        info!("PatchChunk: {} -> {} ({} replicas patched)", old_chunk_id, new_chunk_id, patched_node_ids.len());
        Ok(new_location)
    }

    /// Build ChunkLocation entries from chunk_ids/sizes with file_offset tracking.
    /// replica_nodes_per_chunk provides the actual node list for each chunk (from server response).
    /// If empty or mismatched, falls back to empty node list (healing will repair later).
    fn build_chunk_locations_from_ids(
        chunk_ids: &[ChunkId],
        chunk_sizes: &[u64],
        file_offset: u64,
        replica_nodes_per_chunk: Vec<Vec<NodeId>>,
    ) -> Vec<dfs_common::ChunkLocation> {
        let mut locations = Vec::with_capacity(chunk_ids.len());
        let mut current_offset = file_offset;
        for (idx, (chunk_id, &size)) in chunk_ids.iter().zip(chunk_sizes.iter()).enumerate() {
            let nodes = replica_nodes_per_chunk.get(idx).cloned().unwrap_or_default();
            locations.push(dfs_common::ChunkLocation {
                chunk_id: *chunk_id,
                nodes,
                size: size as usize,
                checksum: chunk_id.hash,
                file_offset: Some(current_offset),
                written_at: None,
            });
            current_offset += size;
        }
        locations
    }

    /// Write file metadata to the leader (with redirect on NotLeader) plus one
    /// additional node for durability.  The leader owns dissemination to all other
    /// followers via its sled-backed queue, so we no longer fan-out to all nodes.
    ///
    /// Retry strategy:
    ///   1. Send to the known leader (or any node if leader unknown).
    ///   2. If the response is NotLeader{leader_addr}, update our cached leader and retry.
    ///   3. After getting leader ack, send to one non-leader for durability (fire-and-forget).
    ///   4. Up to 4 retries total; on exhaustion, fall back to single-node write.
    pub async fn put_file_metadata_with_quorum(
        &self,
        metadata: &FileMetadata,
        _replica_nodes: Option<(SocketAddr, SocketAddr)>
    ) -> Result<()> {
        let nodes = self.cluster_nodes.read().await.clone();
        if nodes.is_empty() {
            anyhow::bail!("No cluster nodes available for metadata write");
        }

        // --- Step 1: Send to leader, retrying on NotLeader redirect. ---
        let mut leader_addr = *self.leader_addr.read().await;
        let mut leader_acked_node: Option<SocketAddr> = None;
        let mut last_err = String::new();

        for attempt in 0..4u32 {
            // Pick target: known leader, else first node.
            let target = leader_addr.unwrap_or_else(|| nodes[0]);

            let req = Request::PutFileMetadata { metadata: metadata.clone() };
            match self.send_request(target, req).await {
                Ok(Response::Ok { .. }) => {
                    // Leader accepted.
                    *self.leader_addr.write().await = Some(target);
                    leader_acked_node = Some(target);
                    break;
                }
                Ok(Response::NotLeader { leader_addr: redirect }) => {
                    debug!("PutFileMetadata: {} said NotLeader, redirecting to {:?}", target, redirect);
                    if let Some(addr) = redirect {
                        *self.leader_addr.write().await = Some(addr);
                        leader_addr = Some(addr);
                    } else {
                        // Leader unknown — try next node.
                        let idx = nodes.iter().position(|&n| n == target).unwrap_or(0);
                        leader_addr = Some(nodes[(idx + 1) % nodes.len()]);
                    }
                    if attempt < 3 {
                        tokio::time::sleep(tokio::time::Duration::from_millis(100 << attempt)).await;
                    }
                }
                Ok(other) => {
                    last_err = format!("unexpected response from {}: {:?}", target, other);
                    warn!("PutFileMetadata: {}", last_err);
                    // Try next node.
                    let idx = nodes.iter().position(|&n| n == target).unwrap_or(0);
                    leader_addr = Some(nodes[(idx + 1) % nodes.len()]);
                }
                Err(e) => {
                    last_err = format!("{}: {}", target, e);
                    warn!("PutFileMetadata to {} failed (attempt {}): {}", target, attempt + 1, e);
                    // Mark leader unknown; try next node.
                    leader_addr = None;
                    *self.leader_addr.write().await = None;
                    let idx = nodes.iter().position(|&n| n == target).unwrap_or(0);
                    leader_addr = Some(nodes[(idx + 1) % nodes.len()]);
                    if attempt < 3 {
                        tokio::time::sleep(tokio::time::Duration::from_millis(100 << attempt)).await;
                    }
                }
            }
        }

        if leader_acked_node.is_none() {
            anyhow::bail!("Metadata write to leader failed after retries: {}", last_err);
        }

        // Step 2: Leader already broadcasts to all followers via broadcast_metadata_to_followers
        // and the durability catch-up queue. No client-side replica needed — sending
        // ReplicateMetadata directly to a follower caused resurrection of deleted files
        // via the follower's leader_forward_queue.
        let leader = leader_acked_node.unwrap();

        // Track SQLite writes for read-after-write consistency.
        if Self::is_sqlite_file(&metadata.path) {
            let mut tracker = self.sqlite_write_tracker.lock().await;
            tracker.put(metadata.path.clone(), (leader, std::time::Instant::now()));
            debug!("SQLite write tracked: path={}, node={}", metadata.path, leader);
        }

        Ok(())
    }

    /// Create or update file metadata (single node write)
    async fn put_file_metadata_single(&self, metadata: &FileMetadata) -> Result<()> {
        let request = Request::PutFileMetadata {
            metadata: metadata.clone(),
        };

        // Track which node handles the write for read-after-write consistency
        let write_node = self.send_request_with_retry_tracking(request).await?;

        // If this is a SQLite file, track the write to ensure reads go to this node
        // within the consistency window (before async replication completes)
        if Self::is_sqlite_file(&metadata.path) {
            let mut tracker = self.sqlite_write_tracker.lock().await;
            tracker.put(metadata.path.clone(), (write_node, std::time::Instant::now()));

            info!(
                "SQLite write tracked: path={}, node={}, consistency_window={}ms",
                metadata.path, write_node, get_sqlite_consistency_window_ms()
            );
        }

        Ok(())
    }

    /// Create or update file metadata
    /// Automatically uses quorum writes when enough nodes are available
    pub async fn put_file_metadata(&self, metadata: &FileMetadata) -> Result<()> {
        // Use quorum writes by default (will fall back internally if not enough nodes)
        self.put_file_metadata_with_quorum(metadata, None).await
    }

    /// Seed the per-file write sequence counter from an existing server record.
    /// Called on open-for-write so that writes after a client restart continue
    /// from where the server left off, not from 0 (which would be dropped by the
    /// server's stale-write guard if the file already has a higher sequence).
    pub fn seed_write_seq(&self, file_id: FileId, server_seq: u64) {
        // Only seed if we don't already have a counter for this file, or if the
        // server has a higher value (e.g. after a client restart).
        let mut entry = self.write_seq.entry(file_id).or_insert(0);
        if server_seq >= *entry {
            *entry = server_seq;
        }
    }

    /// Increment and return the next write sequence number for a file.
    fn next_write_seq(&self, file_id: FileId) -> u64 {
        let mut entry = self.write_seq.entry(file_id).or_insert(0);
        *entry += 1;
        *entry
    }

    /// Stamp the next write_seq onto a metadata clone and return it.
    fn stamp_write_seq(&self, metadata: &FileMetadata) -> FileMetadata {
        let mut m = metadata.clone();
        m.write_seq = self.next_write_seq(m.id);
        m
    }

    /// Public alias for stamp_write_seq — used by fuse_impl's FlushHandle.
    pub fn stamp_write_seq_pub(&self, metadata: &FileMetadata) -> FileMetadata {
        self.stamp_write_seq(metadata)
    }

    /// Enqueue a metadata update for async delivery to the leader.
    /// Returns immediately in the normal case — the background worker handles retries.
    ///
    /// Back-pressure + self-rescue: if the queue front is older than max_age the
    /// worker is stalled. Rather than sleeping passively we:
    ///   1. Pop the stalled entry and attempt immediate delivery (2s timeout).
    ///   2. If that fails, fan out to ALL known nodes in parallel and take the
    ///      first success — this bypasses the cached leader and finds whoever is up.
    ///   3. Signal any release() waiter on the rescued entry.
    ///   4. Re-enqueue the entry if all nodes are unreachable (genuine cluster down).
    ///   5. Repeat until the queue front is young enough to proceed normally.
    ///
    /// SQLite files are sent synchronously regardless (they require immediate consistency).
    pub async fn enqueue_metadata(&self, metadata: &FileMetadata) {
        if Self::is_sqlite_file(&metadata.path) {
            // SQLite needs immediate consistency — bypass the queue.
            if let Err(e) = self.put_file_metadata(metadata).await {
                warn!("SQLite metadata sync failed ({}): {}", metadata.path, e);
            }
            return;
        }

        // Back-pressure: if the front is stalled, attempt ONE rescue then return.
        // We must not loop here — if the cluster is unreachable this would block
        // the calling task indefinitely, starving all FUSE threads on the main runtime.
        // The background metadata_queue worker handles persistent retries; our job is
        // only to do a single fast rescue attempt to unblock a transiently-stalled queue.
        if let Some(age) = self.metadata_queue.front_age().await {
            if age > self.metadata_queue.max_age {
                if let Some(stalled) = self.metadata_queue.pop_stalled().await {
                    warn!(
                        "metadata_queue: back-pressure rescue for {} (age {}ms)",
                        stalled.metadata.path,
                        stalled.enqueued_at.elapsed().as_millis()
                    );

                    // Attempt 1: normal quorum write with 2s timeout.
                    let delivered = tokio::time::timeout(
                        Duration::from_secs(2),
                        self.put_file_metadata_with_quorum(&stalled.metadata, None),
                    ).await.ok().and_then(|r| r.ok()).is_some();

                    if delivered {
                        debug!("metadata_queue: rescue delivered {} via quorum", stalled.metadata.path);
                        if let Some(tx) = stalled.done_tx { let _ = tx.send(()); }
                    } else {
                        // Attempt 2: fan out to all nodes in parallel, take first success.
                        *self.leader_addr.write().await = None;
                        let nodes = self.cluster_nodes.read().await.clone();
                        let meta = stalled.metadata.clone();
                        let rescued = if !nodes.is_empty() {
                            let futs: Vec<_> = nodes.iter().map(|&addr| {
                                let client = self.clone();
                                let m = meta.clone();
                                async move {
                                    let req = Request::PutFileMetadata { metadata: m };
                                    match tokio::time::timeout(
                                        Duration::from_secs(2),
                                        client.send_request(addr, req),
                                    ).await {
                                        Ok(Ok(Response::Ok { .. })) => {
                                            *client.leader_addr.write().await = Some(addr);
                                            true
                                        }
                                        _ => false,
                                    }
                                }
                            }).collect();
                            let results = futures::future::join_all(futs).await;
                            results.into_iter().any(|ok| ok)
                        } else {
                            false
                        };

                        if rescued {
                            warn!("metadata_queue: rescue delivered {} via fan-out", stalled.metadata.path);
                            if let Some(tx) = stalled.done_tx { let _ = tx.send(()); }
                        } else {
                            // All nodes unreachable — put it back; background worker will keep trying.
                            warn!("metadata_queue: rescue failed for {}, all nodes unreachable — requeueing",
                                  stalled.metadata.path);
                            self.metadata_queue.push_inner_front(stalled).await;
                        }
                    }
                }
            }
        }

        let stamped = self.stamp_write_seq(metadata);
        self.metadata_queue.push(stamped).await;
    }

    /// Enqueue metadata for release (close). Waits until the background worker
    /// confirms delivery to the leader — retries indefinitely, no timeout.
    /// The FUSE thread is parked in block_on but tokio worker threads keep running,
    /// so the metadata queue worker proceeds without starvation.
    pub async fn flush_metadata_sync(&self, metadata: &FileMetadata) {
        let stamped = self.stamp_write_seq(metadata);
        self.metadata_queue.push_and_wait(stamped).await;
    }

    /// Spawn the background metadata queue worker onto the given runtime.
    /// Must be called once after construction. The worker runs for the lifetime
    /// of the process, retrying each item until the leader confirms receipt.
    pub fn start_metadata_queue_worker(&self, runtime: &tokio::runtime::Handle) {
        let client = self.clone();
        runtime.spawn(async move {
            loop {
                // Wait for something to appear in the queue.
                // Use enable()+notified() so a notify_one() that fires while we are
                // inside the drain loop is not lost — the permit is stored and the
                // next notified().await returns immediately.
                let notified = client.metadata_queue.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();  // arm the permit before we check the queue

                // Drain all available items before waiting again.
                loop {
                    let entry = match client.metadata_queue.pop().await {
                        Some(e) => e,
                        None => break,
                    };

                    // Retry until the leader confirms. Each failure re-identifies the leader.
                    // Each attempt is capped at 5s so a saturated/slow leader can't block
                    // this worker (and any release() waiter) for 30s+ per attempt.
                    let mut attempts = 0u32;
                    loop {
                        let result = tokio::time::timeout(
                            Duration::from_secs(2),
                            client.put_file_metadata_with_quorum(&entry.metadata, None),
                        ).await;
                        match result {
                            Ok(Ok(())) => {
                                info!(
                                    "[META QUEUE] delivered path={} id={} seq={} size={}",
                                    entry.metadata.path, entry.metadata.id,
                                    entry.metadata.write_seq, entry.metadata.size
                                );
                                // Signal the release waiter if present.
                                if let Some(tx) = entry.done_tx {
                                    let _ = tx.send(());
                                }
                                break;
                            }
                            Ok(Err(e)) => {
                                attempts += 1;
                                let backoff_ms = (200u64 * attempts as u64).min(5000);
                                warn!(
                                    "metadata_queue: delivery failed for {} (attempt {}), \
                                     retrying in {}ms: {}",
                                    entry.metadata.path, attempts, backoff_ms, e
                                );
                                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                            }
                            Err(_) => {
                                // 5s deadline exceeded — leader is saturated or unreachable.
                                // Clear cached leader so next attempt rediscovers via any node.
                                *client.leader_addr.write().await = None;
                                attempts += 1;
                                let backoff_ms = (200u64 * attempts as u64).min(5000);
                                warn!(
                                    "metadata_queue: timed out delivering {} (attempt {}), \
                                     retrying in {}ms",
                                    entry.metadata.path, attempts, backoff_ms
                                );
                                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                            }
                        }
                    }
                }
                // Queue is empty — wait for the next notification.  The permit was
                // armed before the drain loop, so any notify_one() that fired during
                // delivery is captured here and won't cause a missed wakeup.
                notified.await;
            }
        });
    }

    /// Cancel any pending metadata queue entry for the given file_id.
    /// Must be called after a successful delete so the queue worker can't resurrect the file.
    pub async fn cancel_metadata(&self, file_id: dfs_common::FileId) {
        self.metadata_queue.cancel(file_id).await;
    }

    /// Delete file
    pub async fn delete_file(&self, path: &str) -> Result<()> {
        let request = Request::DeleteFile {
            path: path.to_string(),
        };

        // Always send deletes to the leader. If sent to a follower that doesn't hold
        // the file, the follower broadcasts to all peers — with a glob delete of
        // hundreds of files this fans out to thousands of concurrent connections and
        // exhausts file descriptors on every node. The leader always has the authoritative
        // metadata and handles the broadcast itself under the broadcast_semaphore.
        let response = {
            let leader = self.leader_addr.read().await;
            match *leader {
                Some(addr) => match self.send_request(addr, request.clone()).await {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("delete_file to leader failed ({}), retrying any node", e);
                        self.send_request_with_retry(request).await?
                    }
                },
                None => self.send_request_with_retry(request).await?,
            }
        };

        match response {
            Response::Ok { .. } => Ok(()),
            Response::Error { code: ErrorCode::NotFound, .. } => {
                // File already gone — treat as success (idempotent delete).
                // This can happen when a glob delete races: the first rm succeeds,
                // the kernel dentry cache still has the inode, and the second rm
                // fires before the kernel notices it's gone.
                Ok(())
            }
            Response::Error { message, .. } => {
                anyhow::bail!("Failed to delete file: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Purge file metadata without deleting chunks (for rename operations)
    /// This only removes the metadata entry, preserving chunk data
    pub async fn purge_file_metadata(&self, path: &str) -> Result<()> {
        let request = Request::PurgeFileMetadata {
            path: path.to_string(),
        };

        let response = self.send_request_with_retry(request).await?;

        match response {
            Response::Ok { .. } => Ok(()),
            Response::Error { message, .. } => {
                anyhow::bail!("Failed to purge file metadata: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Rename file atomically (server-side atomic operation)
    /// This is safer than separate put + purge operations as it prevents
    /// race conditions where the file disappears during rename
    pub async fn rename_file(&self, old_path: &str, new_path: &str) -> Result<()> {
        let request = Request::RenameFile {
            old_path: old_path.to_string(),
            new_path: new_path.to_string(),
        };

        let response = self.send_request_with_retry(request).await?;

        match response {
            Response::Ok { .. } => Ok(()),
            Response::Error { message, .. } => {
                anyhow::bail!("Failed to rename file: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Refresh cluster node list by querying GetClusterStatus.
    /// Tries the known leader first (most authoritative source of cluster membership),
    /// then all currently-known nodes, then the original seed addresses as a last resort.
    pub async fn refresh_cluster_nodes(&self) -> Result<()> {
        // Build a deduplicated candidate list: leader first, then current nodes, then seeds.
        let mut candidates: Vec<SocketAddr> = Vec::new();
        if let Some(leader) = *self.leader_addr.read().await {
            candidates.push(leader);
        }
        let current = self.cluster_nodes.read().await.clone();
        for addr in &current {
            if !candidates.contains(addr) {
                candidates.push(*addr);
            }
        }
        for seed in &self.seed_nodes {
            if !candidates.contains(seed) {
                candidates.push(*seed);
            }
        }

        for node_addr in &candidates {
            let request = Request::GetClusterStatus;

            match self.send_request(*node_addr, request).await {
                Ok(Response::ClusterStatus { nodes: cluster_nodes, leader_node_id, replication_factor, .. }) => {
                    let new_addrs: Vec<SocketAddr> = cluster_nodes
                        .iter()
                        .filter(|n| n.status == dfs_common::NodeStatus::Online)
                        .map(|n| n.addr)
                        .collect();

                    if !new_addrs.is_empty() {
                        {
                            let mut mapping = self.addr_to_node_id.write().await;
                            for node in &cluster_nodes {
                                if node.status == dfs_common::NodeStatus::Online {
                                    mapping.insert(node.addr, node.id);
                                }
                            }
                        }

                        if let Some(leader_id) = leader_node_id {
                            let leader = cluster_nodes.iter()
                                .find(|n| n.id == leader_id && n.status == dfs_common::NodeStatus::Online)
                                .map(|n| n.addr);
                            *self.leader_addr.write().await = leader;
                            if let Some(addr) = leader {
                                info!("Leader node: {} ({})", leader_id, addr);
                            }
                        }

                        if replication_factor > 0 {
                            self.replication_factor.store(replication_factor, Ordering::Relaxed);
                        }

                        let mut nodes_lock = self.cluster_nodes.write().await;
                        *nodes_lock = new_addrs;
                        info!("Refreshed cluster nodes: {} nodes, RF={} (via {})", nodes_lock.len(), replication_factor, node_addr);
                        return Ok(());
                    }
                }
                _ => continue,
            }
        }

        Err(anyhow::anyhow!("Failed to refresh cluster nodes from any server (tried {} candidates)", candidates.len()))
    }

    /// Get storage statistics from all nodes and aggregate them
    /// Returns (total_space, free_space, available_space, replication_factor)
    pub async fn get_storage_stats(&self) -> Result<(u64, u64, u64, usize)> {
        let nodes = self.cluster_nodes.read().await.clone();
        let request = Request::GetStorageStats;

        // Query all nodes IN PARALLEL for speed
        let mut tasks: Vec<tokio::task::JoinHandle<Result<Option<(u64, u64, u64, usize)>, Box<dyn std::error::Error + Send + Sync>>>> = Vec::new();

        for node_addr in nodes {
            let request = request.clone();
            let task = tokio::spawn(async move {
                // Wrap entire query with 10s timeout to avoid hanging on offline nodes
                // (ARM servers need generous timeout for reliable stats)
                let query_future = async {
                    // Create a temporary client for this request with 5s connect timeout
                    let mut stream = tokio::time::timeout(
                        std::time::Duration::from_millis(5000),
                        tokio::net::TcpStream::connect(node_addr)
                    ).await
                        .map_err(|_| Box::new(std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout")) as Box<dyn std::error::Error + Send + Sync>)?
                        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

                let request_id = dfs_common::RequestId::new(
                    std::sync::atomic::AtomicU64::new(1).fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                );
                let envelope = dfs_common::MessageEnvelope::new(
                    request_id,
                    dfs_common::Message::Request(request)
                );
                let encoded = envelope.to_bytes().map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

                use tokio::io::{AsyncWriteExt, AsyncReadExt};
                stream.write_u32(encoded.len() as u32).await.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
                stream.write_all(&encoded).await.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
                stream.flush().await.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

                let mut len_buf = [0u8; 4];
                stream.read_exact(&mut len_buf).await.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
                let len = u32::from_be_bytes(len_buf) as usize;

                let mut buf = vec![0u8; len];
                stream.read_exact(&mut buf).await.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

                    let response_envelope = dfs_common::MessageEnvelope::from_bytes(&buf).map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

                    match response_envelope.message {
                        dfs_common::Message::Response(dfs_common::Response::StorageStats {
                            total_space,
                            free_space,
                            available_space,
                            replication_factor,
                            ..
                        }) => Ok(Some((total_space, free_space, available_space, replication_factor))),
                        _ => Ok(None),
                    }
                };

                // Apply overall 10s timeout to entire query
                tokio::time::timeout(std::time::Duration::from_millis(10000), query_future)
                    .await
                    .map_err(|_| Box::new(std::io::Error::new(std::io::ErrorKind::TimedOut, "query timeout")) as Box<dyn std::error::Error + Send + Sync>)?
            });

            tasks.push(task);
        }

        // Wait for ALL queries to complete in parallel (not sequentially!)
        // Use join_all to await all futures concurrently
        let results = futures::future::join_all(tasks).await;

        let mut total_raw_space = 0u64;
        let mut node_capacities: Vec<(u64, u64)> = Vec::new(); // (total, available) per node
        let mut replication_factor = None;

        for result in results {
            if let Ok(Ok(Some((total, _free, avail, rf)))) = result {
                total_raw_space += total;
                node_capacities.push((total, avail));
                if replication_factor.is_none() {
                    replication_factor = Some(rf);
                }
            }
        }

        // If we didn't get any valid stats, return reasonable defaults
        // This prevents df from hanging when servers are temporarily slow
        if node_capacities.is_empty() {
            warn!("Failed to get storage stats from any node, using defaults");
            return Ok((0, 0, 0, replication_factor.unwrap_or(2)));
        }

        let replication_factor = replication_factor.unwrap_or(2);

        // Calculate usable capacity using greedy algorithm:
        // Iteratively select the best RF nodes and add their bottleneck to total capacity
        //
        // This correctly handles heterogeneous clusters where smart replica set selection
        // can dramatically increase usable capacity.
        //
        // Example: RF=3, nodes (100G, 100G, 100G, 10G)
        //   Iteration 1: Pick top 3 (100,100,100), min=100G, total=100G
        //   Iteration 2: Pick top 3 (0,0,0), min=0G, done
        //   → Total = 100G (NOT 13G from naive formula!)
        let usable_total = total_raw_space / replication_factor as u64;
        let usable_available = calculate_usable_capacity(
            &node_capacities.iter().map(|(_, avail)| *avail).collect::<Vec<_>>(),
            replication_factor
        );

        info!("Storage stats: {} nodes, usable_total={}, usable_avail={} (RF={})",
              node_capacities.len(), usable_total, usable_available, replication_factor);

        // Calculate usable_free as the complement of used space
        // (usable_total - usable_available gives used space on a per-replica basis)
        let usable_free = usable_available;

        Ok((usable_total, usable_free, usable_available, replication_factor))
    }

    /// Get cluster status including chunk size configuration
    pub async fn get_cluster_chunk_size(&self) -> Result<usize> {
        let request = Request::GetClusterStatus;
        let response = self.send_request_with_retry(request).await?;

        match response {
            Response::ClusterStatus { chunk_size_mb, .. } => {
                Ok(chunk_size_mb)
            }
            Response::Error { message, .. } => {
                anyhow::bail!("Failed to get cluster status: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }
}

/// Calculate usable capacity using greedy algorithm for smart replica set selection
///
/// This algorithm correctly handles heterogeneous clusters by iteratively selecting
/// the best replica sets (top RF nodes by capacity) and accounting for their bottleneck.
///
/// Example: RF=3, nodes (100G, 100G, 100G, 10G)
///   - Iteration 1: Pick top 3 (100,100,100), min=100G, add 100G to total
///   - Iteration 2: Pick top 3 (0,0,0), min=0G, done
///   - Result: 100G (NOT 13G from naive min×nodes/RF formula)
///
/// This matches the bash algorithm provided by the user and works for any RF value.
fn calculate_usable_capacity(node_capacities: &[u64], replication_factor: usize) -> u64 {
    if node_capacities.is_empty() || replication_factor == 0 {
        return 0;
    }

    let mut capacities = node_capacities.to_vec();
    let mut total = 0u64;

    loop {
        // Filter out zeros and sort descending
        let mut non_zero: Vec<u64> = capacities.iter()
            .copied()
            .filter(|&c| c > 0)
            .collect();

        // Check if we have at least RF nodes with capacity > 0
        if non_zero.len() < replication_factor {
            break;
        }

        // Sort descending
        non_zero.sort_by(|a, b| b.cmp(a));

        // The decrement is the minimum of the top RF nodes (the RF-th largest value)
        let decrement = non_zero[replication_factor - 1];
        total += decrement;

        // Subtract decrement ONLY from the top RF nodes
        let mut decremented_count = 0;
        for val in &non_zero[0..replication_factor] {
            // Find this value in the original capacities array and decrement it
            for capacity in &mut capacities {
                if *capacity == *val && decremented_count < replication_factor {
                    *capacity = capacity.saturating_sub(decrement);
                    decremented_count += 1;
                    break; // Move to next value in the top RF
                }
            }
        }
    }

    total
}
