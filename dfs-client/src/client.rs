use anyhow::{Context, Result};
use blake3;
use dashmap::{DashMap, DashSet};
use dfs_common::{ChunkId, ChunkLocation, ErrorCode, FileId, FileMetadata, Message, MessageEnvelope, NodeId, Request, RequestId, Response};
use lru::LruCache;
use quick_cache::sync::Cache as QuickCache;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify, RwLock};
use tracing::{debug, error, info, warn};

use crate::read_engine::{InodeReadEngine, ReadEngineMap};

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
    /// Last time a liveness probe (Ping) was launched for this node — single-flights
    /// the probe so concurrent failing requests don't each fire one.
    last_probe_at: Option<std::time::Instant>,
}

impl NodeHealth {
    fn new() -> Self {
        Self { consecutive_failures: 0, penalized_until: None, backoff_level: 0, last_probe_at: None }
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
///
/// A DashMap, not a single Mutex<HashMap<..>> (root-caused 2026-07-18, live on
/// staging): is_penalized() used to be called from only 2 sites; the same-day
/// circuit-breaker fix (send_request/send_split_frame_write_request checking it
/// up front on every RPC) made it a per-request check on the hot path. With a
/// single global mutex, that serialized is_penalized(node_A) against
/// record_failure(node_B) — two calls about completely unrelated nodes — behind
/// one lock, on every one of potentially dozens of concurrently in-flight
/// operations (kdiskmark QD32 = up to 32 outstanding at once). Measured
/// alongside this fix: random-write throughput sat a few percent under its
/// prior peak while sequential throughput exceeded its own peak — consistent
/// with a shared-lock tax that shows up more per-byte on many small concurrent
/// operations than on fewer, larger sequential ones. DashMap shards internally
/// (this codebase's established pattern everywhere else this shape of problem
/// occurs — chunk_map, chunk_to_file, dirty_patch_slots, etc.), so two calls
/// about different SocketAddrs no longer contend at all, and even same-address
/// calls only block other same-address calls, not the other 4 nodes' entries.
#[derive(Clone, Debug)]
pub struct NodeHealthTracker {
    inner: Arc<DashMap<SocketAddr, NodeHealth>>,
}

impl NodeHealthTracker {
    /// Number of consecutive failures before a node is penalized.
    const PENALTY_THRESHOLD: u32 = 5;

    /// How long a node proven dead (connection refused/reset) is skipped. Deliberately
    /// MUCH shorter than BASE_PROBE_SECS (30s), and it never escalates.
    ///
    /// The reason is the case where the only nodes holding a chunk are both offline.
    /// `send_request` hard-rejects a penalized address without attempting a connection,
    /// so a long shed on those two nodes converts "wait for them to come back" into
    /// "instant EIO" — and for a VM guest, waiting is enormously preferable to an I/O
    /// error, which can remount its filesystem read-only. A restarting node is typically
    /// back in seconds, so this window only needs to be long enough to stop the current
    /// retry ladder from re-paying an RPC timeout against a corpse, not long enough to
    /// exile it. Data availability outranks the latency win whenever they conflict.
    const DEAD_SHED_SECS: u64 = 5;
    /// Base probe interval (seconds) — doubles on each repeated failure.
    const BASE_PROBE_SECS: u64 = 30;
    /// Maximum probe interval (seconds).
    const MAX_PROBE_SECS: u64 = 120;

    fn new() -> Self {
        Self { inner: Arc::new(DashMap::new()) }
    }

    /// Record a successful response from `addr`.  Clears all penalty state.
    /// Still `async fn` (no `.await` inside — DashMap's API is synchronous) to
    /// avoid touching the 20+ existing call sites across the client.
    pub async fn record_success(&self, addr: SocketAddr) {
        if let Some(mut h) = self.inner.get_mut(&addr) {
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
        let mut h = self.inner.entry(addr).or_insert_with(NodeHealth::new);
        h.consecutive_failures += 1;

        if h.consecutive_failures >= Self::PENALTY_THRESHOLD {
            let secs = (Self::BASE_PROBE_SECS << h.backoff_level).min(Self::MAX_PROBE_SECS);
            h.penalized_until = Some(std::time::Instant::now() + Duration::from_secs(secs));
            // Increase backoff level for next penalty, capped so we don't overflow the shift.
            if h.backoff_level < 8 {
                h.backoff_level += 1;
            }
            let consecutive_failures = h.consecutive_failures;
            // Drop the shard guard before logging — tracing's own machinery
            // shouldn't run while holding a DashMap shard lock, even though
            // it's normally fast; no reason to extend the hold time for it.
            drop(h);
            warn!(
                "Node {} penalized for {}s after {} consecutive failures",
                addr, secs, consecutive_failures
            );
        }
    }

    /// Record an UNAMBIGUOUS death signal from `addr` — connection refused, reset,
    /// aborted, or host/network unreachable. Penalizes immediately rather than
    /// accumulating toward PENALTY_THRESHOLD.
    ///
    /// `UnexpectedEof` is deliberately NOT in that set: it is the ordinary signature of
    /// a pooled connection the server closed after its idle timeout, which
    /// `send_request` already retries transparently on a fresh connection precisely
    /// because the node itself is fine. Treating it as death would shed healthy nodes
    /// during normal idle churn.
    ///
    /// The distinction this draws is the whole point: a timeout says "no answer YET"
    /// (the node might merely be slow under load, and shedding it would be wrong),
    /// whereas a refused/reset connection is positive proof from the kernel that
    /// nothing is listening. Treating those identically is what made a single node
    /// restart cost tens of seconds: `record_failure` needed PENALTY_THRESHOLD (5)
    /// accumulations, and `claim_probe` needed PROBE_AFTER_FAILURES (2) before it
    /// would even spend a 1s liveness probe — so a corpse stayed in the rotation and
    /// every retry-ladder pass paid a full RPC timeout against it.
    ///
    /// Measured on staging 2026-07-22: a rolling restart during a live VM produced
    /// client stalls of 4.7s/12.8s/27.3s with ZERO failover events logged, because the
    /// ladder walked every node twice at 3s each and kept being redirected back to the
    /// dead leader. The guest's ~30s SCSI timeout expired first and the install took an
    /// I/O error. Failover detection must be far cheaper than the deadline that
    /// ultimately surfaces as EIO, and proof-of-death is the cheapest signal available.
    ///
    /// Deliberately does NOT touch `backoff_level`'s escalation the way hard_penalize
    /// does, and uses DEAD_SHED_SECS rather than BASE_PROBE_SECS — see that constant.
    pub async fn record_dead(&self, addr: SocketAddr) {
        let mut h = self.inner.entry(addr).or_insert_with(NodeHealth::new);
        let already_penalized = h.penalized_until.is_some();
        h.consecutive_failures = h.consecutive_failures.max(Self::PENALTY_THRESHOLD);
        h.penalized_until = Some(std::time::Instant::now() + Duration::from_secs(Self::DEAD_SHED_SECS));
        drop(h);
        if !already_penalized {
            warn!("Node {} shed immediately on proof of death (connection refused/reset/EOF) — skipping it for {}s instead of paying a full RPC timeout per retry",
                addr, Self::DEAD_SHED_SECS);
        }
    }

    /// True when an io::Error is positive proof the peer is gone, as opposed to
    /// merely slow. Used to choose between `record_dead` and `record_failure`.
    pub fn is_proof_of_death(kind: std::io::ErrorKind) -> bool {
        matches!(
            kind,
            std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::HostUnreachable
                | std::io::ErrorKind::NetworkUnreachable
        )
    }

    /// Consecutive failures that make a node "suspicious" enough to spend a
    /// liveness probe on. Deliberately above 1 so a single blip never probes, and
    /// well below PENALTY_THRESHOLD so a confirmed black hole is shed long before
    /// the 5-failure blind breaker would trip.
    const PROBE_AFTER_FAILURES: u32 = 2;

    /// Single-flight gate: returns true (and records the attempt) if `addr` is
    /// suspicious enough to warrant a liveness probe and none was launched
    /// recently. Marking `last_probe_at` here — under the shard lock — is what
    /// stops N concurrent failing requests from each firing their own probe.
    pub async fn claim_probe(&self, addr: SocketAddr) -> bool {
        let mut h = self.inner.entry(addr).or_insert_with(NodeHealth::new);
        if h.consecutive_failures < Self::PROBE_AFTER_FAILURES || h.is_penalized() {
            return false;
        }
        let now = std::time::Instant::now();
        if let Some(t) = h.last_probe_at {
            if now.duration_since(t) < Duration::from_secs(2) {
                return false;
            }
        }
        h.last_probe_at = Some(now);
        true
    }

    /// A liveness probe confirmed `addr` is unreachable (a black hole: TCP up but
    /// not answering even a Ping). Penalize immediately instead of waiting for
    /// PENALTY_THRESHOLD blind timeouts, so replica selection sheds it now and the
    /// write path's candidate-widening reroutes around it within seconds.
    pub async fn hard_penalize(&self, addr: SocketAddr) {
        let mut h = self.inner.entry(addr).or_insert_with(NodeHealth::new);
        h.consecutive_failures = h.consecutive_failures.max(Self::PENALTY_THRESHOLD);
        let secs = (Self::BASE_PROBE_SECS << h.backoff_level).min(Self::MAX_PROBE_SECS);
        h.penalized_until = Some(std::time::Instant::now() + Duration::from_secs(secs));
        if h.backoff_level < 8 {
            h.backoff_level += 1;
        }
        drop(h);
        warn!("Node {} hard-penalized for {}s (liveness probe failed — confirmed unreachable)", addr, secs);
    }

    /// Returns true if `addr` is currently in a penalty period.
    pub async fn is_penalized(&self, addr: SocketAddr) -> bool {
        if let Some(mut h) = self.inner.get_mut(&addr) {
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
        // Per-address shard lookups, not one lock held across the whole loop
        // (there was no async work inside the loop even before this, so the old
        // single-mutex version never blocked on I/O while holding it — but it did
        // serialize every OTHER concurrent NodeHealthTracker call, for any node,
        // behind this one call's full duration). Each iteration below now only
        // ever touches (and briefly locks) that one address's own DashMap shard.
        let mut healthy = Vec::new();
        let mut penalized = Vec::new();
        for &addr in addrs {
            let is_pen = if let Some(mut h) = self.inner.get_mut(&addr) {
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

/// Cache key for byte-range caching: (inode, file_byte_offset).
/// chunk_id is intentionally excluded: after a PatchChunk the chunk_id changes,
/// so including it caused every post-write read to be a cache miss even though
/// the flush path writes the fresh data into the cache immediately after the patch.
/// Staleness is prevented by the flush path overwriting the cache entry with
/// post-patch bytes on every successful flush, and by the 30s TTL as a backstop.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
struct ByteRangeCacheKey {
    inode: u64,
    file_offset: u64,
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

/// Key for zero-filled gap table (inode + chunk file offset)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ZeroGapKey {
    inode: u64,
    chunk_offset: u64,
}

/// Represents a zero-filled gap in a sparse file chunk.
/// Instead of caching actual zeros, we just track the range metadata.
#[derive(Debug, Clone)]
struct ZeroGap {
    /// File offset where this gap starts
    start: u64,
    /// File offset where this gap ends (exclusive)
    end: u64,
    /// When this gap was created (for TTL expiration)
    created_at: Instant,
}

impl ZeroGap {
    /// Check if this gap has expired (same TTL as byte cache: 30 seconds)
    fn is_expired(&self) -> bool {
        self.created_at.elapsed() > std::time::Duration::from_secs(30)
    }

    /// Check if a given file offset falls within this zero gap
    fn contains(&self, offset: u64) -> bool {
        offset >= self.start && offset < self.end
    }

    /// Check if this gap overlaps with the given range
    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.start < end && start < self.end
    }
}

/// Number of shards for byte_range_cache and zero_gap_table. Both are keyed by
/// (inode, ...), so sharding on inode gives perfect isolation between files —
/// concurrent random reads on different files no longer contend on one global
/// Mutex, and per-inode full scans (invalidation) only need to lock one shard
/// instead of the whole cache.
const CACHE_SHARD_COUNT: usize = 16;

fn shard_index(inode: u64) -> usize {
    (inode as usize) % CACHE_SHARD_COUNT
}

/// Sharded byte-range cache: each shard is an independently-locked LruCache.
struct ShardedByteRangeCache {
    shards: Vec<Mutex<LruCache<ByteRangeCacheKey, CachedChunk>>>,
}

impl ShardedByteRangeCache {
    fn new(total_capacity: NonZeroUsize) -> Self {
        let per_shard = NonZeroUsize::new((total_capacity.get() / CACHE_SHARD_COUNT).max(1)).unwrap();
        Self {
            shards: (0..CACHE_SHARD_COUNT).map(|_| Mutex::new(LruCache::new(per_shard))).collect(),
        }
    }

    fn shard(&self, inode: u64) -> &Mutex<LruCache<ByteRangeCacheKey, CachedChunk>> {
        &self.shards[shard_index(inode)]
    }
}

/// Sharded zero-gap table: each shard is an independently-locked HashMap.
struct ShardedZeroGapTable {
    shards: Vec<Mutex<HashMap<ZeroGapKey, Vec<ZeroGap>>>>,
}

impl ShardedZeroGapTable {
    fn new() -> Self {
        Self {
            shards: (0..CACHE_SHARD_COUNT).map(|_| Mutex::new(HashMap::new())).collect(),
        }
    }

    fn shard(&self, inode: u64) -> &Mutex<HashMap<ZeroGapKey, Vec<ZeroGap>>> {
        &self.shards[shard_index(inode)]
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
/// Per-server connection pool capacity. Must exceed PIPELINE_MAX_ITEMS (16) so that
/// all concurrent patch tasks can each return their connection without evicting others.
/// Evicted connections are dropped rather than shutdown, leaving the server in CLOSE_WAIT
/// and accumulating until file descriptor limits are hit under heavy write load.
const POOL_SIZE: usize = 20;

/// Toggle for striped reads (split a 4MB chunk across 2 replicas, fetch halves in parallel).
/// Striped reads halve transfer time on saturated links but cost an extra 4MB allocation +
/// memcpy per chunk to reassemble. On weak ARM CPUs (Cortex-A55) the reassembly cost can
/// exceed the bandwidth win on a 1Gbps LAN. Flip to `false` to use single-replica whole-chunk
/// reads instead — easy A/B test, easy to revert.
const STRIPED_READ_ENABLED: bool = false;

// Maximum concurrent background (prefetch/swarm) fetches allowed per server node.
// Spinning HDDs serve sequential requests faster than concurrent ones; capping at 1
// prevents seek contention and keeps server disk queues short across successive reads.
const MAX_BACKGROUND_PER_NODE: usize = 1;

// Hot/cold fold-path classification — see hot_chunk_slots's doc comment for the
// full scheme. A slot needs HOT_CLASSIFY_SAMPLES patches landing at more than
// HOT_RATE_THRESHOLD_PER_SEC to earn "hot" status (and therefore fold-trigger
// evaluation at all); it loses that status, and any in-progress classification
// window, after HOT_INACTIVITY_RESET of quiet. 2 patches/sec sits between the
// ~0.2/sec average measured locally for a wide-random-write workload's mostly-
// cold chunks and the ~4/sec cited for genuinely hot chunks elsewhere in this
// file (ACTIVE_FOLD_PATCH_THRESHOLD's doc comment) — enough margin to reject
// coincidental clustering from many concurrent writers sharing a small chunk
// space, while still comfortably catching real hot access before it does much
// unfolded accumulation.
const HOT_CLASSIFY_SAMPLES: u64 = 4;
const HOT_RATE_THRESHOLD_PER_SEC: f64 = 2.0;
const HOT_INACTIVITY_RESET: Duration = Duration::from_secs(2);

/// Get the cap on concurrent in-flight range-fetch requests a single file may have
/// outstanding to a single storage node. Without this, one file's random-read
/// workload (e.g. a benchmark with a high queue depth) can open unbounded
/// connections to a node and starve other files reading from the same node.
/// Can be overridden via DFS_RANGE_FETCH_MAX_PER_FILE_NODE for cap-tuning trials.
/// Default: 6 (raised from 2 after staging RND4K benchmarking showed 6 as the sweet
/// spot between per-file concurrency and starving other files on the same node).
fn range_fetch_max_per_file_node() -> usize {
    std::env::var("DFS_RANGE_FETCH_MAX_PER_FILE_NODE")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&v: &usize| v > 0)
        .unwrap_or(6)
}

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
    /// File IDs for which all future metadata writes are permanently blocked.
    /// Populated by cancel() when a file is deleted. Prevents a race where the
    /// release task's chunk write completes after the delete and tries to enqueue
    /// a stale metadata update — resurrecting the file on the server.
    /// Each FileId is a UUID: never reused, so no false-positive blocking.
    deleted_ids: DashSet<FileId>,

    /// Highest write_seq per file confirmed delivered (a leader returned
    /// Ok/ResyncMetadataRequested — either way, that push's own write_seq was
    /// genuinely persisted somewhere). Root-caused 2026-07-17 alongside the
    /// covers_from_write_seq accounting fix: that fix only derives coverage from
    /// data present in the CURRENT push (its own chunk_locations, or entries still
    /// sitting in this queue to coalesce with) — it has no memory that an earlier
    /// push for this file was already dequeued and successfully delivered moments
    /// ago. A rapid sequence of near-empty pushes (e.g. two closely-spaced
    /// open()/create() pushes) then looked like a gap purely because the earlier one
    /// had already left the queue by the time the later one's covers_from was
    /// computed. Folded into covers_from_write_seq (push_inner) as an additional
    /// floor: `delivered_write_seq + 1` is always safe to claim as covered, since we
    /// have concrete evidence a leader accepted it — this is orthogonal to (and must
    /// not be confused with) a NEW leader's own dissemination catch-up lag, which is
    /// a different, already-handled concern.
    delivered_write_seq: DashMap<FileId, u64>,
}

struct MetadataEntry {
    metadata: FileMetadata,
    enqueued_at: Instant,
    /// If Some, worker signals this channel after delivery (release/sync path).
    done_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// Lowest write_seq this entry's (possibly coalesced) chunk_locations fully
    /// accounts for. Starts equal to the entry's own write_seq; lowered by min()
    /// on every coalesce merge in push_inner, since coalescing unions in an older
    /// push's chunk_locations too. Sent to the server as covers_from_write_seq so
    /// it can distinguish a benign coalesced write_seq jump from a genuine gap.
    covers_from_write_seq: u64,
}

impl MetadataQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(VecDeque::new()),
            index: Mutex::new(HashMap::new()),
            notify: Notify::new(),
            max_age: Duration::from_secs(24),
            deleted_ids: DashSet::new(),
            delivered_write_seq: DashMap::new(),
        })
    }

    /// Record that `write_seq` for `file_id` was confirmed delivered to a leader —
    /// see delivered_write_seq's doc comment. Called by the queue worker after every
    /// successful PutFileMetadata response (Ok or ResyncMetadataRequested — both mean
    /// the push was persisted).
    fn mark_delivered(&self, file_id: FileId, write_seq: u64) {
        self.delivered_write_seq.entry(file_id)
            .and_modify(|v| *v = (*v).max(write_seq))
            .or_insert(write_seq);
    }

    /// Enqueue an async metadata update (fire-and-forget, no confirmation).
    /// Deduplicates by file_id — replaces existing entry in-place keeping original
    /// timestamp. Does NOT implement back-pressure directly — callers that need
    /// back-pressure (enqueue_metadata) check age and rescue before calling this.
    pub async fn push(&self, metadata: FileMetadata) {
        self.push_inner(metadata, None, false).await;
    }

    /// Enqueue a full authoritative metadata snapshot (complete chunk_locations, not
    /// just a delta) in response to Response::ResyncMetadataRequested — see
    /// DfsClient::pending_resync's doc comment. Forces covers_from_write_seq to 0 so
    /// detect_metadata_write_seq_gap never re-flags this push itself (0 can never
    /// exceed stored_write_seq + 1); relies on the existing union-only chunk_map merge
    /// (merge_file_metadata, 08a6201) to only ever fill in what the leader is missing.
    pub async fn push_full_snapshot(&self, metadata: FileMetadata) {
        self.push_inner(metadata, None, true).await;
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
        let path = metadata.path.clone();
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        self.push_inner(metadata, Some(tx), false).await;
        // Await confirmation from the worker. Log every 5s so stalls are visible;
        // never give up — the data is safely replicated, we just need metadata to land.
        let start = std::time::Instant::now();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), &mut rx).await {
                Ok(_) => break,
                Err(_) => {
                    warn!(
                        "flush_metadata_sync: waiting {}s for metadata delivery of {} — \
                         leader may be saturated, throttling writes",
                        start.elapsed().as_secs(), path
                    );
                }
            }
        }
    }

    async fn push_inner(
        &self,
        metadata: FileMetadata,
        done_tx: Option<tokio::sync::oneshot::Sender<()>>,
        force_full_snapshot: bool,
    ) {
        // Block metadata writes for deleted files. cancel() adds the file_id here to
        // prevent a race where the release task's chunk write completes after the unlink
        // delete and enqueues stale metadata — resurrecting the file on the server.
        if self.deleted_ids.contains(&metadata.id) {
            debug!("[META QUEUE] blocked: file {} ({}) is deleted, skipping enqueue",
                   metadata.id, metadata.path);
            // Signal any done_tx waiter so the release path doesn't hang.
            if let Some(tx) = done_tx { let _ = tx.send(()); }
            return;
        }
        let op = if done_tx.is_some() { "release" } else { "update" };
        let mut q = self.inner.lock().await;
        let mut idx = self.index.lock().await;

        if let Some(&pos) = idx.get(&metadata.id) {
            if let Some(entry) = q.get_mut(pos) {
                // Dedup replace: only replace if incoming write_seq >= existing.
                // This ensures newer metadata (higher sequence) always wins, even if
                // a stale entry somehow arrives after a newer one was already queued.
                //
                // Union chunk_locations from BOTH sides first, regardless of which one
                // wins the scalar-field comparison below. Since 2026-07-07,
                // flush_buffer_async sends only each cycle's newly-flushed locations
                // (all_locations), not the full cumulative history — two coalesced
                // pushes for the same file describe two DIFFERENT, not overlapping-nor-
                // superset, deltas. Blindly keeping only the "winning" side's
                // chunk_locations (the old behavior, safe back when every push carried
                // the complete history) silently drops whichever chunk(s) only the
                // losing side described — caught by T48 as an intermittent off-by-one
                // in the persisted chunk count under concurrent/rapid flush cycles.
                // Dedup by file_offset: a genuine conflict (same offset, both sides
                // recorded a write to it) prefers the scalar-comparison winner's version.
                // This coalesce is absorbing incoming's write_seq into entry — entry's
                // covers_from must widen to include it, regardless of which side wins
                // the scalar comparison below. Also widen against incoming's own
                // chunk_locations' client_write_seq — see the "new entry" branch below
                // for why (patch-only write_seq numbers never separately touch this
                // queue, but their resulting locations do end up in this delta).
                entry.covers_from_write_seq = if force_full_snapshot {
                    0
                } else {
                    let derived = metadata.chunk_locations.iter()
                        .filter_map(|l| l.client_write_seq)
                        .fold(entry.covers_from_write_seq.min(metadata.write_seq), u64::min);
                    match self.delivered_write_seq.get(&metadata.id) {
                        Some(w) => derived.min(*w + 1),
                        None => derived,
                    }
                };

                let winner_is_incoming = metadata.write_seq >= entry.metadata.write_seq;
                let (base_locs, other_locs) = if winner_is_incoming {
                    (&metadata.chunk_locations, &entry.metadata.chunk_locations)
                } else {
                    (&entry.metadata.chunk_locations, &metadata.chunk_locations)
                };
                let merged_locs: Vec<dfs_common::ChunkLocation> = if other_locs.is_empty() {
                    base_locs.as_ref().clone()
                } else {
                    let mut merged = base_locs.as_ref().clone();
                    let existing_offsets: std::collections::HashSet<Option<u64>> =
                        merged.iter().map(|l| l.file_offset).collect();
                    for loc in other_locs.iter() {
                        if !existing_offsets.contains(&loc.file_offset) {
                            merged.push(loc.clone());
                        }
                    }
                    merged
                };

                if winner_is_incoming {
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
                    let mut merged_metadata = metadata;
                    merged_metadata.chunk_locations = std::sync::Arc::new(merged_locs);
                    entry.metadata = merged_metadata;
                } else {
                    // Incoming is older — drop its scalar fields, but transfer done_tx if
                    // present so a release() waiter still gets notified when the newer
                    // entry delivers, and keep the union of chunk_locations either way.
                    info!(
                        "[META QUEUE] drop-stale op={} path={} id={} seq={} (queue has seq={})",
                        op, metadata.path, metadata.id, metadata.write_seq,
                        entry.metadata.write_seq
                    );
                    if done_tx.is_some() && entry.done_tx.is_none() {
                        entry.done_tx = done_tx;
                    }
                    entry.metadata.chunk_locations = std::sync::Arc::new(merged_locs);
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
        // write_seq is a single per-file counter shared by metadata-queue pushes AND
        // per-patch RCL ordering (next_write_seq, consumed by every MultiPatch rotation
        // — e.g. qcow2 preallocation bursts). A patch never separately enqueues a
        // metadata push, but its resulting ChunkLocation (carrying that patch's own
        // client_write_seq) DOES end up in the very push that follows. Defaulting
        // covers_from_write_seq to just this push's own write_seq stamp ignored those
        // earlier patch-only write_seq numbers even though they're already fully
        // represented here — a structural false positive on the leader's
        // detect_metadata_write_seq_gap (root-caused 2026-07-17: thousands of
        // false-positive [META GAP] warnings during the VM 111 install, one for
        // essentially every push that followed a patch burst). Fold in the minimum
        // client_write_seq actually present in this push's chunk_locations.
        let covers_from_write_seq = if force_full_snapshot {
            0
        } else {
            let derived = metadata.chunk_locations.iter()
                .filter_map(|l| l.client_write_seq)
                .fold(metadata.write_seq, u64::min);
            // Also fold in delivered_write_seq's watermark — see its doc comment.
            // Closes a second false-positive class: an earlier push for this file may
            // have already been dequeued and delivered before this one was built, so
            // there's nothing left in this queue to coalesce with, yet nothing was
            // actually lost.
            match self.delivered_write_seq.get(&metadata.id) {
                Some(w) => derived.min(*w + 1),
                None => derived,
            }
        };
        q.push_back(MetadataEntry { metadata, enqueued_at: Instant::now(), done_tx, covers_from_write_seq });
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

    /// Cancel any pending queue entry for the given file_id AND block all future
    /// enqueues for it. Called when a file is deleted — prevents a race where the
    /// release task's chunk write completes after the delete and enqueues stale
    /// metadata, resurrecting the file on the server.
    pub async fn cancel(&self, file_id: FileId) {
        // Mark as deleted first so any concurrent push_inner sees it immediately.
        self.deleted_ids.insert(file_id);
        // This file's write_seq bookkeeping is now moot — it's a UUID, never reused,
        // so nothing will ever need this entry again. Same lifecycle event as
        // deleted_ids; see delivered_write_seq's doc comment for what it tracks.
        self.delivered_write_seq.remove(&file_id);

        let mut q = self.inner.lock().await;
        let mut idx = self.index.lock().await;
        if let Some(pos) = idx.remove(&file_id) {
            if let Some(removed) = q.remove(pos) {
                info!(
                    "[META QUEUE] cancel id={} path={} seq={} (delete pre-empt)",
                    file_id, removed.metadata.path, removed.metadata.write_seq
                );
                // Signal any done_tx waiter so push_and_wait doesn't hang.
                if let Some(tx) = removed.done_tx { let _ = tx.send(()); }
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

/// Dedup key for pending_chunk_locations: identifies a chunk SLOT (file_id +
/// chunk-aligned file_offset), not a specific chunk_id — see that field's doc
/// comment for why chunk_id itself can't be the key.
type ChunkLocationSlotKey = (Option<dfs_common::FileId>, Option<u64>);

/// Upsert `locations` into `pending` keyed by chunk slot, freshest-wins via
/// client_write_seq. Shared by every pending_chunk_locations writer (enqueue,
/// the two failed-send re-queue paths, and the two direct-extend call sites) so
/// they can't disagree about the merge rule. When either side's write_seq is
/// unknown (None — legacy/fresh-write records), falls back to last-write-wins
/// by call order, matching the old Vec's behavior for those records.
fn upsert_chunk_location(
    pending: &mut HashMap<ChunkLocationSlotKey, dfs_common::ChunkLocation>,
    location: dfs_common::ChunkLocation,
) {
    let key = (location.file_id, location.file_offset);
    let keep_new = match pending.get(&key) {
        Some(existing) => match (existing.client_write_seq, location.client_write_seq) {
            (Some(old_seq), Some(new_seq)) => new_seq >= old_seq,
            _ => true,
        },
        None => true,
    };
    if keep_new {
        pending.insert(key, location);
    }
}

/// Client for communicating with DFS cluster
#[derive(Clone)]
pub struct DfsClient {
    /// List of cluster nodes (updated by refresh_cluster_nodes)
    pub cluster_nodes: Arc<RwLock<Vec<SocketAddr>>>,

    /// Original seed addresses provided at startup.
    /// Never mutated — used as a fallback when all cluster_nodes are unreachable
    /// so we can re-bootstrap cluster membership from scratch.
    seed_nodes: Vec<SocketAddr>,

    /// Current node index (for round-robin)
    current_node: Arc<RwLock<usize>>,

    /// Concurrent LRU cache for chunks (ChunkId -> data).
    /// Uses quick-cache (clock-based LRU approximation) — no global write lock,
    /// no W-TinyLFU frequency sketch, so cross-file scan pollution cannot occur.
    pub chunk_cache: Arc<QuickCache<ChunkId, Arc<Vec<u8>>>>,

    /// Byte-range cache for recently-accessed chunks (inode, offset) -> chunk data
    /// This solves the problem of content-addressed chunks changing during live DVR recording
    /// Even if chunk hashes change, we can still cache by file position
    byte_range_cache: Arc<ShardedByteRangeCache>,

    /// Combined worst-case byte size of chunk_cache + byte_range_cache, as actually
    /// computed at startup (not re-derived independently elsewhere). Callers sizing
    /// their own memory budget off the same `available_mb` snapshot (e.g.
    /// fuse_impl.rs's write-buffer cap) must subtract this first — each cache being
    /// sized as an independent percentage of the same "available" figure would let
    /// their worst cases silently compound past what's actually safe.
    pub reserved_cache_bytes: usize,

    /// Per-target "last op completed at" timestamp, keyed by server address —
    /// used only to compute the idle gap since the previous chunk-write op to
    /// that same target, logged alongside each op's own duration in WRITETIMING.
    /// Duration alone tells you how long one transaction took; the gap tells you
    /// how much time is being lost *between* transactions (client-side queuing/
    /// backpressure/scheduling) rather than inside them (server/network) — sum of
    /// both, divided into bytes sent, gives real achieved throughput per target.
    /// Added 2026-07-14 for the offline-compaction regression investigation.
    write_target_last_op_at: Arc<DashMap<SocketAddr, std::time::Instant>>,

    /// Zero-filled gap table: tracks ranges that contain zeros in sparse files.
    /// Key: (inode, chunk_offset), Value: Vec of gap ranges within that chunk.
    /// This avoids caching megabytes of zeros for qcow2 sparse writes.
    /// Gaps expire with same TTL as byte_range_cache (30s).
    zero_gap_table: Arc<ShardedZeroGapTable>,

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

    /// Inodes currently open for writing. Reads on these inodes bypass the chunk cache
    /// so the writer always sees fresh server-side content (e.g. HDHomeRun reading chunk 0
    /// to update seek offsets must not get a stale cached version of the previous patch).
    pub write_open_inodes: Arc<dashmap::DashSet<u64>>,

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

    /// Per-node capacity (available_bytes, total_bytes), keyed by SocketAddr.
    /// Updated during refresh_cluster_nodes() from NodeInfo fields.
    /// Used for capacity-banded write placement (same algorithm as server-side).
    node_capacities: Arc<DashMap<SocketAddr, (u64, u64)>>,

    /// Global semaphore capping total concurrent chunk fetches across ALL simultaneous
    /// read_data calls. Without this, a seek causes N parallel FUSE reads each spawning
    /// their own 20-slot semaphore, producing N*20 simultaneous connections and
    /// exhausting server file descriptors.
    fetch_semaphore: Arc<tokio::sync::Semaphore>,

    /// In-flight fetch count per server node, used to spread load across replicas.
    /// Incremented before a fetch task starts, decremented when it completes.
    node_inflight: Arc<DashMap<SocketAddr, Arc<AtomicUsize>>>,

    /// Per-(inode, node) semaphore bounding concurrent random-read range-fetch
    /// requests one file may have outstanding to one storage node. Created lazily;
    /// caps a single file's benchmark-style high queue depth from monopolizing a
    /// node's connections and starving other files' reads to the same node.
    range_fetch_node_limit: Arc<DashMap<(u64, SocketAddr), Arc<tokio::sync::Semaphore>>>,

    /// Single broadcast notify woken every time a chunk lands in chunk_cache.
    /// Lets waiters in `wait_for_chunk_in_cache` resume immediately rather than
    /// polling on a 50 ms timer — the polling delay was the dominant source of
    /// dead air at chunk boundaries on the sequential read path.
    chunk_landed: Arc<Notify>,

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

    /// Files the leader has flagged via Response::ResyncMetadataRequested — a genuine
    /// write_seq gap (detect_metadata_write_seq_gap) that this leader can't self-heal.
    /// flush_buffer_async's background-tick branch checks this on every tick for the
    /// inode it's about to push and, if present, sends a full metadata_cache snapshot
    /// instead of the usual delta, then clears the entry. Not persisted: a file with no
    /// open inode simply waits here until next opened for write, same as today's
    /// do-nothing outcome for that case, just self-healing once writes resume.
    pub(crate) pending_resync: Arc<dashmap::DashSet<FileId>>,

    /// Per-file cooldown backstop on push_full_snapshot, independent of how accurate
    /// covers_from_write_seq accounting turns out to be. A full snapshot's cost scales
    /// with the file's TOTAL chunk count, not a delta's — the exact cost profile that
    /// caused a real regression on large files (see flush_buffer_async's background-tick
    /// branch doc comment, 2026-07-07: ~1.5ms to ~40-49ms per push past ~1300 chunk
    /// entries). Root-caused 2026-07-17: even after two rounds of closing
    /// covers_from_write_seq false-positive classes, 81 resync requests still fired
    /// during a single local suite run — bounding the worst case here means a third,
    /// undiscovered false-positive source can only cost one full snapshot per file per
    /// RESYNC_DEBOUNCE, not one per background-tick (2s).
    pub(crate) last_resync_sent_at: Arc<DashMap<FileId, std::time::Instant>>,

    /// Counts every time multi_patch_chunk_on_replicas_inner's backfill had to reach
    /// beyond the chunk's originally-targeted nodes to a genuinely new cluster
    /// candidate (compute_required_replicas' doc comment) — i.e. an original target was
    /// unreachable and a different node had to take over as the second replica. Not
    /// itself a problem (this is exactly the robustness fix for a compaction pause or
    /// restart racing a patch), but a sustained rise means it's worth checking why
    /// (see single_replica_emergency_count for the more serious tier) — full-chunk
    /// re-replication to a new node isn't free.
    pub(crate) backfill_new_candidate_count: Arc<std::sync::atomic::AtomicU64>,

    /// Counts every URGENT_SINGLE_REPLICA event — a patch write that still couldn't
    /// reach required_replicas after exhausting every reachable candidate (original
    /// targets AND health-sorted cluster fallbacks). This should stay at zero in normal
    /// operation; any nonzero value means the cluster was, even briefly, unable to
    /// place a second replica anywhere.
    pub(crate) single_replica_emergency_count: Arc<std::sync::atomic::AtomicU64>,

    /// Counts every time the bounded follow-up watchdog (spawned alongside
    /// urgent_heal on a single_replica_emergency_count event) ran its full
    /// SINGLE_REPLICA_FOLLOWUP_DEADLINE without the chunk ever reaching
    /// required_replicas — see multi_patch_chunk_on_replicas_inner's emergency
    /// fallback. This should stay at zero even more strictly than
    /// single_replica_emergency_count itself: reaching this means the cluster
    /// couldn't achieve durability for a specific chunk even with the healer's own
    /// priority queue actively working on it for the full deadline — a "dead disk"
    /// -grade signal, not routine degradation.
    pub(crate) single_replica_followup_exhausted_count: Arc<std::sync::atomic::AtomicU64>,

    /// Pending per-chunk ReplicateChunkLocation notifications, coalesced into batched
    /// ReplicateChunkLocations RPCs by a background drain task instead of each patch
    /// sending its own individual RPC. Safe to batch/delay: no caller depends on this
    /// landing before it proceeds (each just logs+retries today, never branching
    /// control flow on the outcome), and flush_metadata_sync delivers the file's
    /// complete, authoritative chunk_locations state at the end of every flush cycle
    /// regardless — see send_chunk_locations_batched's doc comment.
    ///
    /// Keyed on (file_id, file_offset) — the chunk-SLOT identity — not on
    /// ChunkLocation::chunk_id. chunk_id is blake3(file_id || file_offset || data),
    /// so it changes on every patch to a slot; keying on it would never collapse a
    /// hot slot's repeated patches. Keying on the slot instead means N patches to the
    /// same chunk landing in one batch window collapse to that slot's single latest
    /// location before the RPC/server loop ever sees the redundant N-1. Freshest-wins
    /// via ChunkLocation::client_write_seq (see upsert_chunk_location) rather than
    /// plain insertion order, so a delayed re-queue can't clobber a fresher arrival.
    /// 2026-07-21 staging finding: a hot chunk under concurrent small-write load
    /// produced ~9.6 chunk-location-replicated completions per actual patch applied
    /// before this dedup existed.
    pending_chunk_locations: Arc<tokio::sync::Mutex<HashMap<ChunkLocationSlotKey, dfs_common::ChunkLocation>>>,

    /// Per-file monotonic write sequence counter. Each metadata enqueue increments
    /// the counter for that file_id and stamps it on the metadata before queuing.
    /// Prevents out-of-order dissemination from overwriting newer records with stale ones.
    /// Seeded from the server's stored write_seq on first open-for-write.
    write_seq: Arc<DashMap<FileId, u64>>,

    /// Per-(file, chunk_idx) monotonic sequence counter, deliberately separate from
    /// `write_seq` above (that one gates file-level metadata merge Rule 1/Rule 2 —
    /// not touched by this). Incremented once per patch to a specific chunk slot
    /// and sent as `new_chunk_seq` on PatchChunk/MultiPatch — see CHUNK_SEQ_TABLE's
    /// doc comment in dfs-server/src/metadata.rs for the rationale (a plain integer
    /// compare instead of chasing chunk_id identity through a fold/merge chain).
    /// Currently only ever incremented and sent; nothing consumes the server's
    /// returned chunk_seq yet — see that doc comment for the follow-up.
    chunk_seq: Arc<DashMap<(FileId, u64), u64>>,

    /// Cache of chunk_id -> (write_seq, inserted_at) for read operations.
    /// Populated in read_file() from the file's metadata and looked up by read_chunk_from_server()
    /// to enable client-driven metadata staleness detection. Entries are content-addressed
    /// (a rewrite mints a new ChunkId) so they're never overwritten — swept on a timer by
    /// start_read_write_seq_cache_sweeper() instead, to bound long-mount-lifetime growth.
    read_write_seq_cache: Arc<DashMap<ChunkId, (u64, Instant)>>,

    /// Per-inode read engines.  Each open file gets one engine that holds the chunk map
    /// snapshot and pipeline state.  Writers never touch this; engines refresh lazily.
    pub read_engines: ReadEngineMap,

    /// Recently-patched chunk IDs keyed by (inode, chunk_idx).
    /// Written after every successful PatchChunk/MultiPatch so the next write to the same
    /// slot can bypass a full GetFileMetadata round-trip and go straight to a single-chunk
    /// GetFileChunkMap on failure — or use the cached id directly on the happy path.
    /// TTL is enforced by comparing the stored Instant against a 10s window at read time.
    pub recent_chunk_writes: Arc<DashMap<(u64, u64), (ChunkId, FileId, Instant, Vec<dfs_common::NodeId>)>>,

    /// When the currently-accumulating Pending generation for (file_id, chunk_idx)
    /// started, from this client's own point of view. Drives
    /// multi_patch_chunk_on_replicas_inner's active-fold timer — see
    /// Request::ForceFold's doc comment for why the client, not each server
    /// independently, now owns the fold-timing decision. Reset whenever a
    /// ForceFold succeeds (a new generation starts accumulating from there).
    /// A stale leftover entry from a since-superseded file is harmless: worst
    /// case it makes the very next patch to a reused slot trigger one spurious
    /// (but safe — fold_slot_now treats "nothing pending" as trivial success)
    /// early ForceFold.
    active_fold_started_at: Arc<DashMap<(FileId, u64), Instant>>,

    /// Patches accumulated since the current generation's timer was seeded
    /// (paired 1:1 with active_fold_started_at, same reset points). Lets the
    /// fold trigger fire on patch volume, not just wall-clock time — see the
    /// ACTIVE_FOLD_PATCH_THRESHOLD doc comment at its use site. Added
    /// 2026-07-13, independently of and complementary to active_fold_bytes
    /// below (same slot-generation reset points, different dimension: patch
    /// count vs. patch payload size).
    active_fold_patch_count: Arc<DashMap<(FileId, u64), u64>>,

    /// Sum of patch payload bytes this client has sent to (file_id, chunk_idx)'s
    /// current accumulating generation since the last fold — the size-based
    /// counterpart to active_fold_started_at's time-based trigger. Added
    /// 2026-07-12 after a local repro found the *only* fold trigger was the
    /// ~6-10s timer: a hot slot under heavy concurrent write pressure (many
    /// small patches landing faster than the timer fires) can accumulate an
    /// unbounded server-side delta file between fold cycles — reproduced
    /// locally as a single 73MB on-disk accumulator for one chunk of a 50MB
    /// test file within ~15s, and separately as a sustained-write run driving
    /// a small test disk to 100% full. This must be a client-side trigger
    /// (not a server-side one) for the same REPLICA DISAGREEMENT reason
    /// active_fold_started_at's doc comment explains: only a single,
    /// externally-driven decision delivered identically to every replica is
    /// safe — an independent per-replica size check would have the exact same
    /// race PATCH_FOLD_COUNT_THRESHOLD was already measured to make worse.
    /// Reset alongside active_fold_started_at wherever that resets.
    active_fold_bytes: Arc<DashMap<(FileId, u64), usize>>,

    /// Chunks currently classified "hot" — only these consult the fold-trigger
    /// logic below (active_fold_interval/bytes/count); everything else applies
    /// patches directly and skips fold-trigger evaluation entirely, since most
    /// chunks under a wide random-write workload (kdiskmark-style) are touched
    /// only a couple of times and never benefit from having been folded. A
    /// chunk earns hot status by sustaining >2 patches/sec across
    /// HOT_CLASSIFY_SAMPLES consecutive patches (see its use site), and loses
    /// it after HOT_INACTIVITY_RESET of quiet. Expected to hold far fewer
    /// entries than active_fold_started_at/active_fold_patch_count (which
    /// still track every recently-touched slot pre-classification) — see
    /// start_hot_chunk_sweeper for why those two don't grow unbounded either.
    hot_chunk_slots: Arc<DashSet<(FileId, u64)>>,

    /// (last_failure_at, consecutive_failure_count) per slot whose most recent
    /// ForceFold attempt failed (a replica errored/timed out, or two replicas
    /// disagreed on the result). Added 2026-07-13 after a live kdiskmark run
    /// under real CPU pressure showed a retry storm: on failure, the code
    /// deliberately leaves active_fold_started_at unchanged so "the very next
    /// patch to this slot retries the fold immediately instead of waiting out
    /// another full interval" — correct under normal conditions, but under
    /// sustained write pressure to a hot chunk, *every single subsequent
    /// patch* re-attempts the full permit-acquire + dual-replica-RPC dance,
    /// even though the last attempt just failed for the same reason (an
    /// overloaded peer) moments ago. Confirmed live: fold_concurrency permit
    /// waits stretching past 6 minutes for one chunk while dozens of other
    /// chunks' patches kept re-triggering fresh attempts. This doesn't change
    /// *whether* a failed fold retries (it still does, on the next patch) —
    /// only adds a short per-slot cooldown so a struggling chunk isn't
    /// hammering the shared permit pool on every single write while things
    /// are bad. Cleared on any successful fold for that slot.
    active_fold_failure_backoff: Arc<DashMap<(FileId, u64), (Instant, u32)>>,

    /// Bounds how many ForceFold operations this client has actively in flight
    /// (RPC round-trip in progress, blocking the triggering patch) at once,
    /// across every file/slot. Root-caused 2026-07-11 night: a wide, dense
    /// random-write pattern (kdiskmark Q32T1/Q1T1) touches many different
    /// (file_id, chunk_idx) slots at a similar rate, so their independent 8s
    /// active-fold timers mature in overlapping windows — observed up to 51
    /// ForceFold calls (each a real 4MB read+write+fsync on the server)
    /// within a single one-second window on server5's client log, saturating
    /// disk I/O.
    ///
    /// Deliberately WIDE, not narrow: a same-day attempt at a tight cap (2)
    /// measured an ~80x Q32T1 throughput collapse (20 -> 0.25) — because the
    /// permit is acquired *inline in the triggering write's own completion
    /// path* (required — see the ForceFold call site's doc comment for why
    /// that blocking is load-bearing for correctness, not optional), a narrow
    /// global pool couples every unrelated chunk's write latency together
    /// through it (convoy/head-of-line blocking). This must stay comfortably
    /// above realistic burst concurrency so the common case never actually
    /// waits on it — it's a backstop against a genuinely pathological burst,
    /// not a routine gate.
    ///
    /// Widened again 2026-07-12 (16 -> 40): a real kdiskmark run against
    /// server5's VM100 (spanning ~150 distinct hot chunks of the qcow2 disk)
    /// measured up to 37 ForceFold maturities landing in a single one-second
    /// window even after the active-fold interval was jittered per-slot (see
    /// its call site) — with that many hot slots each maturing roughly every
    /// 6-10s, steady-state average demand alone (~150/8 ≈ 19/s) already
    /// exceeded the old 16-permit pool, not just the transient bursts. This
    /// same semaphore also now bounds MultiPatch's min-2-replica backfill
    /// (see that call site) — another full-chunk-copy operation of the same
    /// cost class, sharing the pool rather than getting an uncoordinated
    /// second one.
    fold_concurrency: Arc<tokio::sync::Semaphore>,
}

/// True if a replica write/patch failure is a transient node-level connectivity or
/// latency problem (unreachable, OR reachable-but-slow / black-holing) rather than a
/// data/protocol error. The MultiPatch retry loop rides these out within
/// CONNECT_RETRY_BUDGET instead of failing the whole patch to the lossy "all replicas
/// failed" fallback.
///
/// 2026-07-21: originally this only matched "Failed to connect", so a merely-overloaded
/// leader (which returns "Timeout reading chunk" / "Timeout writing to", not a connect
/// error) failed the patch instantly with no retry — falling through to a fallback that
/// drops the write and leaves a gap in chunk_locations (root cause of qcow2 VM-disk
/// corruption). A slow-but-alive node is exactly the case we most want to wait out, so
/// its timeout errors must count as transient too. Matched case-insensitively against
/// the error strings produced by send_request / send_split_frame_write_request.
fn is_transient_write_failure(err_msg: &str) -> bool {
    let s = err_msg.to_lowercase();
    // All distinctive client-side network-wrapper phrases (see send_request /
    // send_split_frame_write_request). Deliberately does NOT match server-side data
    // errors ("chunk data missing", "unexpected response") or ChunkStale — those are
    // real and must fail/retry-with-correction, not spin as a connectivity blip.
    s.contains("failed to connect")                    // connect refused/timeout
        || s.contains("connect timeout")               // pooled-reconnect connect timeout
        || s.contains("timeout reading")               // slow/black-hole response read
        || s.contains("timeout writing")               // slow/black-hole request write
        || s.contains("i/o error talking to")          // connection dropped mid-write
        || s.contains("i/o error reading write response") // connection dropped mid-read
        || s.contains("i/o error on retry")            // connection dropped on write-retry
}

impl DfsClient {
    /// Create a new DFS client
    pub fn new(cluster_nodes: Vec<SocketAddr>) -> Result<Self> {
        if cluster_nodes.is_empty() {
            anyhow::bail!("No cluster nodes provided");
        }

        // The chunk_cache is a single shared LRU keyed by ChunkId — every inode hits
        // the same pool, so a working set spanning multiple files competes fairly for
        // slots.  Sizing strategy: target a fraction of available RAM, but cap so the
        // write buffer + in-flight pipeline always have headroom.
        //
        // The byte_range_cache is a smaller secondary cache keyed by (inode, offset)
        // used by the legacy read_data path for partial-chunk DVR seeks.  It can hold
        // the same data as chunk_cache, so we keep its budget significantly smaller
        // than chunk_cache to avoid duplication eating most of RAM.
        let available_mb = dfs_common::get_available_memory()
            .map(|bytes| bytes / (1024 * 1024))
            .unwrap_or(1024);

        // chunk_cache target: target_pct of available RAM, bounded by [min, max].
        // Sub-1GB clients still cap aggressively; 1-2GB clients now get a real cache
        // (was previously stuck at 32MB regardless of how much RAM was available).
        let (chunk_target_pct, min_chunks, default_max_chunks) = if available_mb < 256 {
            // Extremely low memory: minimum viable cache (~8MB).
            (4, 2, 4)
        } else if available_mb < 512 {
            // Very low: ~32MB max so the write path has headroom.
            (8, 4, 8)
        } else if available_mb < 1024 {
            // Low (512MB-1GB): aim for ~120MB so a sequential read working set fits.
            // Bumping this tier was a key fix — the previous max of 8 chunks (32MB)
            // caused thrash on the nanopir3 (2GB total, ~900MB available) for any
            // sequential read of a file larger than the cache.
            (12, 8, 32)
        } else if available_mb < 2048 {
            // 1-2GB: aim for ~150MB.
            (12, 12, 48)
        } else if available_mb < 4096 {
            (15, 16, 96)
        } else {
            (18, 24, 128)
        };

        // byte_range_cache: a quarter of chunk_cache target, since the new
        // read_file path doesn't touch it and it largely duplicates chunk_cache
        // for live-DVR partial reads.
        let byte_target_pct = (chunk_target_pct / 4).max(2);

        let max_chunks = std::env::var("DFS_MAX_CACHE_CHUNKS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(default_max_chunks);

        let byte_max_chunks = (max_chunks / 4).max(2);

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

        let cache = Arc::new(QuickCache::new(cache_capacity.get()));

        // Byte-range cache: smaller secondary cache for the legacy partial-read path.
        // Sized at a fraction of chunk_cache so it doesn't duplicate the working set.
        let byte_cache_capacity = dfs_common::calculate_cache_capacity(
            4 * 1024 * 1024, // 4MB chunk size
            byte_target_pct,
            (min_chunks / 4).max(2),
            byte_max_chunks,
        )
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to calculate byte-range cache capacity: {}, using default", e);
            NonZeroUsize::new(4).unwrap()
        });

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

        let reserved_cache_bytes =
            (cache_capacity.get() + byte_cache_capacity.get()) * 4 * 1024 * 1024;

        Ok(Self {
            cluster_nodes: Arc::new(RwLock::new(cluster_nodes.clone())),
            seed_nodes: cluster_nodes,
            current_node: Arc::new(RwLock::new(0)),
            chunk_cache: cache,
            byte_range_cache: Arc::new(ShardedByteRangeCache::new(byte_cache_capacity)),
            reserved_cache_bytes,
            zero_gap_table: Arc::new(ShardedZeroGapTable::new()),
            write_target_last_op_at: Arc::new(DashMap::new()),
            connection_pool: Arc::new(DashMap::new()),
            prefetch_in_flight: Arc::new(Mutex::new(HashSet::new())),
            read_history: Arc::new(tokio::sync::RwLock::new(LruCache::new(NonZeroUsize::new(256).unwrap()))),
            last_prefetch_position: Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(256).unwrap()))),
            write_open_inodes: Arc::new(dashmap::DashSet::new()),
            replica_selector: Arc::new(AtomicU64::new(0)),
            replica_cache: Arc::new(Mutex::new(replica_cache)),
            sqlite_write_tracker: Arc::new(Mutex::new(sqlite_write_tracker)),
            addr_to_node_id: Arc::new(RwLock::new(HashMap::new())),
            warm_cache_map: Arc::new(Mutex::new(warm_cache_map)),
leader_addr: Arc::new(RwLock::new(None)),
            node_capacities: Arc::new(DashMap::new()),
            fetch_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
            node_inflight: Arc::new(DashMap::new()),
            range_fetch_node_limit: Arc::new(DashMap::new()),
            chunk_landed: Arc::new(Notify::new()),
            node_health: NodeHealthTracker::new(),
            replication_factor: Arc::new(AtomicUsize::new(2)),
            metadata_queue: MetadataQueue::new(),
            pending_resync: Arc::new(dashmap::DashSet::new()),
            last_resync_sent_at: Arc::new(DashMap::new()),
            backfill_new_candidate_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            single_replica_emergency_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            single_replica_followup_exhausted_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            pending_chunk_locations: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            write_seq: Arc::new(DashMap::new()),
            chunk_seq: Arc::new(DashMap::new()),
            read_write_seq_cache: Arc::new(DashMap::new()),
            read_engines: ReadEngineMap::new(),
            recent_chunk_writes: Arc::new(DashMap::new()),
            active_fold_started_at: Arc::new(DashMap::new()),
            active_fold_patch_count: Arc::new(DashMap::new()),
            active_fold_bytes: Arc::new(DashMap::new()),
            hot_chunk_slots: Arc::new(DashSet::new()),
            active_fold_failure_backoff: Arc::new(DashMap::new()),
            fold_concurrency: Arc::new(tokio::sync::Semaphore::new(
                std::env::var("DFS_FOLD_CONCURRENCY")
                    .ok()
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(40),
            )),
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
    /// Send a request to the current leader, retrying for up to LEADER_OP_TIMEOUT_SECS on
    /// transient failures (network errors, NodeLeaving, NotLeader redirect).
    ///
    /// Returns Ok(Response) — the caller is responsible for interpreting server-side
    /// errors (NotFound, etc.) as permanent or not.  Only network-level failures
    /// (Err) trigger the retry loop; a Response::Error from the server is returned
    /// immediately without retrying.
    async fn send_to_leader_with_retry(&self, request: Request) -> Result<Response> {
        const TIMEOUT_SECS: u64 = 15;
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(TIMEOUT_SECS);
        let mut backoff = tokio::time::Duration::from_millis(150);
        let mut attempts = 0u32;

        loop {
            let nodes = self.cluster_nodes.read().await.clone();
            if nodes.is_empty() {
                anyhow::bail!("no cluster nodes available");
            }
            // When leader is known, use it directly.  When unknown (cleared after a
            // failure), rotate through all known nodes so we don't keep hammering a
            // dead node — the new leader will be at one of the other addresses.
            let leader_opt = *self.leader_addr.read().await;
            let target = leader_opt.unwrap_or_else(|| nodes[attempts as usize % nodes.len()]);

            match self.send_request(target, request.clone()).await {
                Ok(Response::NotLeader { leader_addr: Some(new_leader) }) => {
                    // Redirect — update cache and retry immediately (no backoff).
                    *self.leader_addr.write().await = Some(new_leader);
                    continue;
                }
                Ok(Response::NotLeader { leader_addr: None }) => {
                    // Leader unknown — fall through to backoff + retry.
                    *self.leader_addr.write().await = None;
                }
                Ok(resp) => return Ok(resp), // success or server-side error
                Err(e) => {
                    // Network / connection error — transient.
                    *self.leader_addr.write().await = None;
                    attempts += 1;
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                        return Err(e.context(format!(
                            "leader unreachable after {} attempts ({:.0}s)",
                            attempts, TIMEOUT_SECS
                        )));
                    }
                    let remaining = deadline - now;
                    let wait = backoff.min(remaining);
                    warn!("leader RPC attempt {}: {} — retrying in {:?}", attempts, e, wait);
                    // Actively try to discover the new leader from surviving nodes
                    // before sleeping — this lets a freshly mounted client find the
                    // new leader in the first retry rather than after the full backoff.
                    let _ = self.refresh_cluster_nodes().await;
                    tokio::time::sleep(wait).await;
                    backoff = (backoff * 2).min(tokio::time::Duration::from_secs(2));
                    continue;
                }
            }

            // NotLeader without redirect: refresh and retry.
            attempts += 1;
            let now = tokio::time::Instant::now();
            if now >= deadline {
                anyhow::bail!("no leader found after {} attempts ({:.0}s)", attempts, TIMEOUT_SECS);
            }
            let remaining = deadline - now;
            let wait = backoff.min(remaining);
            warn!("leader election in progress (attempt {}), retrying in {:?}", attempts, wait);
            let _ = self.refresh_cluster_nodes().await;
            tokio::time::sleep(wait).await;
            backoff = (backoff * 2).min(tokio::time::Duration::from_secs(2));
        }
    }

    /// from the seed list and tries once more. A small inter-node delay (100ms) prevents
    /// hammering the network when nodes are refusing connections quickly.
    async fn send_request_with_retry(&self, request: Request) -> Result<Response> {
        let nodes = self.cluster_nodes.read().await.clone();
        let mut last_error = None;
        // Addresses already proven dead in THIS ladder. The second (post-rebootstrap)
        // round skips them instead of paying another full RPC timeout each.
        //
        // Cost model this exists to fix (measured on staging 2026-07-22): the ladder was
        // `all nodes -> 500ms -> re-bootstrap -> all nodes AGAIN`, at up to one RPC
        // timeout per node per round. On a 5-node cluster that is 2 * 5 * 3s + 500ms ~=
        // 31s worst case -- longer than the ~30s SCSI timeout of the guest whose write
        // is waiting on it, so the VM reported an I/O error before the client had even
        // finished deciding where to send. Worse, the cost is O(nodes * timeout * rounds),
        // so it degrades as the cluster grows.
        //
        // Note this is deliberately NOT a deadline on the whole operation. Capping total
        // time would abort legitimately-slow-but-succeeding writes (a 4MB patch to a busy
        // node) and turn "slow" into "failed", generating more retry load exactly when the
        // cluster is already struggling. The goal is to fail OVER fast, not to fail fast:
        // remove dead paths from the ladder, never shorten the deadline for a node that is
        // actually working.
        let mut proven_dead: std::collections::HashSet<SocketAddr> = std::collections::HashSet::new();

        for (i, node_addr) in nodes.iter().enumerate() {
            if i > 0 {
                // Brief pause between node attempts — avoids a connection storm when
                // multiple nodes are down and each refuses immediately.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            match self.send_request(*node_addr, request.clone()).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    if Self::error_is_proof_of_death(&e) {
                        proven_dead.insert(*node_addr);
                        self.node_health.record_dead(*node_addr).await;
                    }
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
                // Skip anything the kernel already proved is gone this pass. Re-bootstrap
                // can legitimately hand back the same list, and a node that refused the
                // connection 500ms ago will refuse it again — there is nothing to learn
                // from a second timeout against it, only time to lose.
                if proven_dead.contains(node_addr) {
                    debug!("Post-refresh: skipping {} — already proven dead this pass", node_addr);
                    continue;
                }
                if i > 0 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                match self.send_request(*node_addr, request.clone()).await {
                    Ok(response) => return Ok(response),
                    Err(e) => {
                        if Self::error_is_proof_of_death(&e) {
                            self.node_health.record_dead(*node_addr).await;
                        }
                        warn!("Post-refresh: failed to send request to {}: {}", node_addr, e);
                        last_error = Some(e);
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All nodes failed")))
    }

    /// True when an error that came back up the stack is positive proof the peer is
    /// gone rather than merely slow. Walks the anyhow chain because the io::Error is
    /// usually wrapped in context by the time it reaches the retry ladder.
    fn error_is_proof_of_death(err: &anyhow::Error) -> bool {
        for cause in err.chain() {
            if let Some(io_err) = cause.downcast_ref::<std::io::Error>() {
                if NodeHealthTracker::is_proof_of_death(io_err.kind()) {
                    return true;
                }
            }
        }
        false
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
    /// No concurrency limiting happens here — callers needing a per-node or
    /// per-(file, node) cap (e.g. range_fetch_permit_for) must acquire it themselves
    /// before calling this.
    /// Single-flight liveness probe: if `addr` is suspicious (>= PROBE_AFTER_FAILURES
    /// consecutive failures) and none was launched recently, open a FRESH connection
    /// and send a Ping with a tight budget. On failure, hard-penalize immediately so
    /// the node is shed at once instead of after PENALTY_THRESHOLD blind timeouts;
    /// on success, leave the soft failure state alone (the node is slow, not hung).
    ///
    /// A fresh connection on purpose — the pooled one may be stuck behind the very
    /// request that just timed out. Handshake, not a duration guess: a healthy-but-
    /// loaded node still answers Pong in ~1s, a wedged node (all workers parked, port
    /// still LISTENing — the 2026-07-19 gluster3 black hole) cannot.
    async fn confirm_liveness_or_penalize(&self, addr: SocketAddr) {
        if !self.node_health.claim_probe(addr).await {
            return;
        }
        let alive = {
            let probe = async {
                let mut s = TcpStream::connect(addr).await.ok()?;
                let _ = s.set_nodelay(true);
                let env = MessageEnvelope::new(
                    RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst)),
                    Message::Request(Request::Ping),
                );
                let encoded = env.to_bytes().ok()?;
                let len = encoded.len() as u32;
                s.write_all(&len.to_be_bytes()).await.ok()?;
                s.write_all(&encoded).await.ok()?;
                s.flush().await.ok()?;
                let mut len_buf = [0u8; 4];
                s.read_exact(&mut len_buf).await.ok()?;
                let rlen = u32::from_be_bytes(len_buf) as usize;
                let mut buf = vec![0u8; rlen];
                s.read_exact(&mut buf).await.ok()?;
                let resp = MessageEnvelope::from_bytes(&buf).ok()?;
                Some(matches!(resp.message, Message::Response(Response::Pong)))
            };
            matches!(tokio::time::timeout(Duration::from_secs(1), probe).await, Ok(Some(true)))
        };
        if alive {
            debug!("Liveness probe to {} OK — slow but alive, not penalizing", addr);
        } else {
            self.node_health.hard_penalize(addr).await;
        }
    }

    async fn send_request(&self, addr: SocketAddr, request: Request) -> Result<Response> {
        // Fail instantly against a node already known to be bad, instead of paying a
        // full connect-attempt + retry-connect-attempt + timeout cycle on every single
        // request to it. NodeHealthTracker already records every failure from this
        // function (record_failure below, and its many other call sites) and computes
        // is_penalized() from that — a real circuit breaker already existed, it just
        // wasn't being read here. Root-caused 2026-07-18 live on staging: gluster5 had
        // ~2960 kernel-level ghost CLOSE_WAIT sockets (a real OS-level issue, not an
        // application bug) and every write that happened to target it as a replica paid
        // the full retry-then-3s-timeout tax on EVERY call, serialized behind the
        // dispatch's join_all — one bad node was measured stalling a single MultiPatch
        // chunk write over 12s (6s primary + 6.6s sequential backfill) instead of
        // failing over to the two healthy replicas quickly. is_penalized() only trips
        // after PENALTY_THRESHOLD (5) consecutive failures with real backoff (30-120s),
        // so this can't fire on a single blip — by the time it's true, the node has
        // already proven itself bad multiple times in a row.
        if self.node_health.is_penalized(addr).await {
            return Err(anyhow::anyhow!(
                "Node {} is penalized (recent repeated failures) — skipping without attempting a connection",
                addr
            ));
        }

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
                    // 5s, matching the other connect paths in this file (truly-fresh,
                    // and mid-request-failure retry) — this used to be a tighter 1s,
                    // which is backwards: a peer we already know was reachable (we had
                    // it pooled) shouldn't get LESS patience reconnecting than a total
                    // stranger. This is also the single most common connect path for a
                    // GetFileChunkMap refresh to the leader specifically — that
                    // connection is used rarely enough (each file refreshes only every
                    // ~5s of active reading, or on open) that the server's own 5s idle
                    // close routinely beats us to it, so nearly every "cold cache" first
                    // refresh lands here. Confirmed live on server4's dfs-client.log
                    // (2026-07-27): 6 real refresh failures, all landing at ~1.0-1.4s —
                    // exactly the old timeout — for inodes later confirmed to be real,
                    // populated files. A momentarily busy leader (e.g. many files
                    // cold-opening at once) had exactly enough margin to trip this one
                    // artificially-tight timeout.
                    let fresh = tokio::time::timeout(
                        tokio::time::Duration::from_secs(5),
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
        // CLIENTNET timing added 2026-07-12 alongside NETTIMING (server) and SPTIMING
        // (handle_multi_patch) to find where a slow round trip (233ms-1.5s+ measured at
        // the multi_patch_chunk_on_replicas_inner level, while server-side handling was
        // consistently 5-25ms) is actually going: client write, network+server, or
        // client read. write/read here are just the syscall-level client-side legs;
        // the gap between write completing and read completing that ISN'T server-side
        // dispatch (per NETTIMING) is genuine network transit / TCP-level delay.
        let io_future = async {
            // Coalesce the length prefix and body into one buffer so the write hits the
            // wire as a single packet instead of two — halves the packet count (and
            // associated kernel/NIC/ACK overhead) on this RTT-bound RPC path.
            let len = encoded.len() as u32;
            let mut framed = Vec::with_capacity(4 + encoded.len());
            framed.extend_from_slice(&len.to_be_bytes());
            framed.extend_from_slice(&encoded);
            let write_start = std::time::Instant::now();
            stream.write_all(&framed).await?;
            stream.flush().await?;
            let write_elapsed = write_start.elapsed();

            let read_start = std::time::Instant::now();
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await?;
            let len = u32::from_be_bytes(len_buf) as usize;

            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await?;
            let read_elapsed = read_start.elapsed();
            if write_elapsed.as_millis() >= 5 || read_elapsed.as_millis() >= 5 {
                info!("CLIENTNET addr={} write={:?} read={:?}", addr, write_elapsed, read_elapsed);
            }
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
                // UnexpectedEof is the common case: the server's idle-connection
                // timeout (network.rs IDLE_TIMEOUT, 300s/5min — NOT 5s, corrected
                // 2026-07-27 after this comment's wrong number fed a mistaken root-
                // cause theory) fires while a connection is sitting in the client
                // pool, or the peer closed it for some other reason (restart, RST).
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
                        // Read timed out after the connection succeeded — the exact
                        // black-hole signature. Confirm with a Ping and shed the node
                        // fast if it's genuinely hung (single-flight, only if suspicious).
                        self.confirm_liveness_or_penalize(addr).await;
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
                        // Read timed out after the connection succeeded — the exact
                        // black-hole signature. Confirm with a Ping and shed the node
                        // fast if it's genuinely hung (single-flight, only if suspicious).
                        self.confirm_liveness_or_penalize(addr).await;
                        return Err(anyhow::anyhow!("Timeout reading chunk from {}", addr));
                    }
                }
            }
        };

        let response_envelope = MessageEnvelope::from_bytes(&buf)
            .context("Failed to deserialize response")?;

        let response = match response_envelope.message {
            Message::Response(Response::ChunkData { data, chunk_id, cache_stats, .. }) => {
                // Split-frame: if data is empty, raw payload follows on the stream.
                let data = if data.is_empty() {
                    dfs_common::protocol::read_chunk_payload(&mut stream).await
                        .context("read split-frame chunk payload")?
                } else {
                    data
                };
                Response::ChunkData { chunk_id, data, cache_stats, arc_data: None, arc_range: None }
            }
            Message::Response(response) => response,
            _ => anyhow::bail!("Expected Response message"),
        };

        // Return connection to pool after all bytes are drained.
        // Clone Arc out before .await to avoid holding DashMap shard lock across await.
        // Pool size matches PIPELINE_MAX_ITEMS so concurrent patches each have a slot.
        // When the pool is full, explicitly shutdown() instead of dropping: dropping sends
        // a FIN but the kernel may not complete the TCP close sequence before the server
        // handles it, leaving the server stuck in CLOSE_WAIT. Explicit shutdown() lets the
        // server progress through CLOSE_WAIT → LAST_ACK → CLOSED immediately.
        {
            let mutex = {
                let entry = self.connection_pool
                    .entry(addr)
                    .or_insert_with(|| Arc::new(Mutex::new(std::collections::VecDeque::new())));
                Arc::clone(&*entry)
            };
            let mut queue = mutex.lock().await;
            if queue.len() < POOL_SIZE {
                queue.push_back(stream);
            } else {
                // Pool full — close gracefully so server doesn't accumulate CLOSE_WAIT.
                tokio::spawn(async move { let _ = stream.shutdown().await; });
            }
        }

        // ServerBusy: treat as a transient error so send_request_with_retry backs
        // off and tries another node rather than propagating as EIO.
        if let Response::Error { code: dfs_common::ErrorCode::ServerBusy, .. } = &response {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            return Err(anyhow::anyhow!("ServerBusy"));
        }

        // NodeLeaving: the node has announced a graceful departure. Clear the cached
        // leader so the next request triggers a cluster refresh, then return an error
        // so the caller retries against another node immediately.
        if let Response::Error { code: dfs_common::ErrorCode::NodeLeaving, .. } = &response {
            debug!("Node {} is leaving — clearing cached leader to force refresh", addr);
            *self.leader_addr.write().await = None;
            return Err(anyhow::anyhow!("NodeLeaving"));
        }

        self.node_health.record_success(addr).await;
        Ok(response)
    }

    /// Send pre-serialized request bytes to a specific address
    /// This is an optimization for cases where the same request needs to be sent to multiple servers
    /// (e.g., dual-replica writes) - serialize once, send multiple times.
    async fn send_encoded_request(&self, addr: SocketAddr, encoded: &[u8]) -> Result<Response> {
        debug!("Sending pre-serialized request to {} ({} bytes)", addr, encoded.len());

        // Coalesce the length prefix and body once up front (reused across the primary
        // send and both stale-connection retries below) so each attempt is a single
        // write_all instead of two — fewer packets on this latency-bound RPC path.
        let framed = {
            let len = encoded.len() as u32;
            let mut f = Vec::with_capacity(4 + encoded.len());
            f.extend_from_slice(&len.to_be_bytes());
            f.extend_from_slice(encoded);
            f
        };

        // Try pooled connection first; on failure (stale) fall back to a fresh one.
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
                let mut buf = [0u8; 1];
                let peer_closed = match s.try_read(&mut buf) {
                    Ok(0) => true,
                    Ok(_) => true,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
                    Err(_) => true,
                };

                if peer_closed {
                    debug!("Pooled connection to {} closed by peer, reconnecting", addr);
                    let mut s = s;
                    let _ = s.shutdown().await;
                    // 5s, matching the other connect paths in this file (truly-fresh,
                    // and mid-request-failure retry) — this used to be a tighter 1s,
                    // which is backwards: a peer we already know was reachable (we had
                    // it pooled) shouldn't get LESS patience reconnecting than a total
                    // stranger. This is also the single most common connect path for a
                    // GetFileChunkMap refresh to the leader specifically — that
                    // connection is used rarely enough (each file refreshes only every
                    // ~5s of active reading, or on open) that the server's own 5s idle
                    // close routinely beats us to it, so nearly every "cold cache" first
                    // refresh lands here. Confirmed live on server4's dfs-client.log
                    // (2026-07-27): 6 real refresh failures, all landing at ~1.0-1.4s —
                    // exactly the old timeout — for inodes later confirmed to be real,
                    // populated files. A momentarily busy leader (e.g. many files
                    // cold-opening at once) had exactly enough margin to trip this one
                    // artificially-tight timeout.
                    let fresh = tokio::time::timeout(
                        tokio::time::Duration::from_secs(5),
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

        // Send and receive with a 3s timeout
        let io_future = async {
            stream.write_all(&framed).await?;
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
                let retry_result = async {
                    fresh.write_all(&framed).await.context("write request")?;
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
                        return Err(anyhow::anyhow!("Timeout reading from {}", addr));
                    }
                }
            }
            Ok(Err(e)) => {
                self.node_health.record_failure(addr).await;
                return Err(anyhow::anyhow!("I/O error talking to {}: {}", addr, e));
            }
            Err(_) => {
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

                let retry_result = async {
                    fresh.write_all(&framed).await.context("write request")?;
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
                        return Err(anyhow::anyhow!("Timeout reading from {}", addr));
                    }
                }
            }
        };

        let response_envelope = MessageEnvelope::from_bytes(&buf)
            .context("Failed to deserialize response")?;

        let response = match response_envelope.message {
            Message::Response(Response::ChunkData { data, chunk_id, cache_stats, .. }) => {
                // Split-frame: if data is empty, raw payload follows on the stream.
                let data = if data.is_empty() {
                    dfs_common::protocol::read_chunk_payload(&mut stream).await
                        .context("read split-frame chunk payload")?
                } else {
                    data
                };
                Response::ChunkData { chunk_id, data, cache_stats, arc_data: None, arc_range: None }
            }
            Message::Response(response) => response,
            _ => anyhow::bail!("Expected Response message"),
        };

        // Return connection to pool after all bytes are drained.
        {
            let mutex = {
                let entry = self.connection_pool
                    .entry(addr)
                    .or_insert_with(|| Arc::new(Mutex::new(std::collections::VecDeque::new())));
                Arc::clone(&*entry)
            };
            let mut queue = mutex.lock().await;
            if queue.len() < POOL_SIZE {
                queue.push_back(stream);
            } else {
                tokio::spawn(async move { let _ = stream.shutdown().await; });
            }
        }

        self.node_health.record_success(addr).await;
        Ok(response)
    }

    /// Send a write request using split-frame encoding to avoid bincode serialization overhead.
    /// The envelope contains an empty data field; raw bytes are sent separately.
    async fn send_split_frame_write_request(&self, addr: SocketAddr, encoded_envelope: &[u8], raw_data: &[u8]) -> Result<Response> {
        // See send_request's matching check for why: fail instantly against a node
        // already known bad instead of paying a full connect+retry+timeout cycle on
        // every call to it while the multi-replica dispatch (join_all) waits on it.
        if self.node_health.is_penalized(addr).await {
            return Err(anyhow::anyhow!(
                "Node {} is penalized (recent repeated failures) — skipping without attempting a connection",
                addr
            ));
        }

        debug!("Sending split-frame write request to {} ({} bytes data)", addr, raw_data.len());

        // SFWTIMING: splits this call into pool-acquire / connect / write / read-response
        // phases so a growing WRITETIMING duration (client.rs's per-target data-phase log)
        // can be attributed to a specific sub-step instead of staying an opaque total —
        // e.g. distinguishing "waiting for a pool lock or a free flush_runtime worker
        // thread" from "the actual network transfer got slower". Added 2026-07-14.
        let sfw_start = std::time::Instant::now();

        // Try pooled connection first
        let pool_acquire_start = std::time::Instant::now();
        let pooled = {
            let mutex_opt = self.connection_pool.get(&addr).map(|e| Arc::clone(&*e));
            if let Some(mutex) = mutex_opt {
                mutex.lock().await.pop_front()
            } else {
                None
            }
        };
        let pool_acquire_ms = pool_acquire_start.elapsed().as_secs_f64() * 1000.0;
        let pool_hit = pooled.is_some();

        let mut stream = match pooled {
            Some(s) => {
                let mut buf = [0u8; 1];
                let peer_closed = match s.try_read(&mut buf) {
                    Ok(0) => true,
                    Ok(_) => true,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
                    Err(_) => true,
                };

                if peer_closed {
                    debug!("Pooled connection to {} closed by peer, reconnecting", addr);
                    let mut s = s;
                    let _ = s.shutdown().await;
                    // 5s, matching the other connect paths in this file (truly-fresh,
                    // and mid-request-failure retry) — this used to be a tighter 1s,
                    // which is backwards: a peer we already know was reachable (we had
                    // it pooled) shouldn't get LESS patience reconnecting than a total
                    // stranger. This is also the single most common connect path for a
                    // GetFileChunkMap refresh to the leader specifically — that
                    // connection is used rarely enough (each file refreshes only every
                    // ~5s of active reading, or on open) that the server's own 5s idle
                    // close routinely beats us to it, so nearly every "cold cache" first
                    // refresh lands here. Confirmed live on server4's dfs-client.log
                    // (2026-07-27): 6 real refresh failures, all landing at ~1.0-1.4s —
                    // exactly the old timeout — for inodes later confirmed to be real,
                    // populated files. A momentarily busy leader (e.g. many files
                    // cold-opening at once) had exactly enough margin to trip this one
                    // artificially-tight timeout.
                    let fresh = tokio::time::timeout(
                        tokio::time::Duration::from_secs(5),
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

        // SFWTIMING: everything before this point was pool-acquire + (if pool miss or
        // stale) connect — everything from here to recv_start below is the actual write.
        let connect_done_at = std::time::Instant::now();

        // Send using split-frame encoding (envelope with empty data + raw bytes)
        let io_future = dfs_common::protocol::write_split_frame_request(&mut stream, encoded_envelope, raw_data);

        match tokio::time::timeout(tokio::time::Duration::from_secs(3), io_future).await {
            Ok(Ok(())) => {},
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe
                        || e.kind() == std::io::ErrorKind::ConnectionReset
                        || e.kind() == std::io::ErrorKind::ConnectionAborted
                        || e.kind() == std::io::ErrorKind::UnexpectedEof => {
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
                match tokio::time::timeout(
                    tokio::time::Duration::from_secs(3),
                    dfs_common::protocol::write_split_frame_request(&mut fresh, encoded_envelope, raw_data)
                ).await {
                    Ok(Ok(())) => { stream = fresh; }
                    Ok(Err(e)) => {
                        self.node_health.record_failure(addr).await;
                        return Err(anyhow::anyhow!("I/O error on retry: {}", e));
                    }
                    Err(_) => {
                        self.node_health.record_failure(addr).await;
                        return Err(anyhow::anyhow!("Timeout writing to {}", addr));
                    }
                }
            }
            Ok(Err(e)) => {
                self.node_health.record_failure(addr).await;
                return Err(anyhow::anyhow!("I/O error talking to {}: {}", addr, e));
            }
            Err(_) => {
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

                match tokio::time::timeout(
                    tokio::time::Duration::from_secs(3),
                    dfs_common::protocol::write_split_frame_request(&mut fresh, encoded_envelope, raw_data)
                ).await {
                    Ok(Ok(())) => { stream = fresh; }
                    Ok(Err(e)) => {
                        self.node_health.record_failure(addr).await;
                        return Err(e.into());
                    }
                    Err(_) => {
                        self.node_health.record_failure(addr).await;
                        return Err(anyhow::anyhow!("Timeout writing to {}", addr));
                    }
                }
            }
        }

        // Read response — BOUNDED. This used to be two bare read_exact().await calls
        // with no timeout: a server that received the request but was slow or never
        // answered (an overloaded leader, or a black-holing node — TCP up, never
        // replies) hung this flush task forever, holding its global_flush_semaphore
        // permit and the per-chunk lock. That cascaded into every other inode's flushes
        // (permit-pool exhaustion, seen live as "FIFO wait timeout - previous flush
        // stuck for >30s"), and on VM shutdown / client restart the drain's own bounded
        // flush abandoned the still-hung write — silently losing the chunk's bytes and
        // leaving a gap in chunk_locations. Root cause of qcow2 VM-disk corruption,
        // 2026-07-21. The small-payload sibling (send_request) already bounds its read
        // exactly this way; this large-payload (split-frame) path was missing it. 30s
        // matches the fresh-write path's WRITE_TIMEOUT_SECS — long enough for a
        // genuinely loaded-but-alive leader to answer, short enough that a true black
        // hole is shed (via confirm_liveness_or_penalize) instead of hanging forever.
        const WRITE_RESPONSE_READ_TIMEOUT: Duration = Duration::from_secs(30);
        let recv_start = std::time::Instant::now();
        let read_response = async {
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await?;
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await?;
            Ok::<Vec<u8>, std::io::Error>(buf)
        };
        let buf = match tokio::time::timeout(WRITE_RESPONSE_READ_TIMEOUT, read_response).await {
            Ok(Ok(buf)) => buf,
            Ok(Err(e)) => {
                self.node_health.record_failure(addr).await;
                return Err(anyhow::anyhow!("I/O error reading write response from {}: {}", addr, e));
            }
            Err(_) => {
                self.node_health.record_failure(addr).await;
                // Response never arrived after the request was sent — the exact
                // black-hole signature. Confirm with a Ping and shed the node fast if
                // it's genuinely hung (same handling as send_request's read timeout).
                // Returns "Timeout reading chunk" so the MultiPatch retry loop's
                // is_transient_write_failure check rides it out rather than failing.
                self.confirm_liveness_or_penalize(addr).await;
                return Err(anyhow::anyhow!("Timeout reading chunk from {}", addr));
            }
        };
        let recv_time = recv_start.elapsed();
        debug!("Split-frame write response received in {:?}", recv_time);

        // SFWTIMING: see this function's entry comment. connect_ms covers pool-miss/stale
        // reconnect + the actual write (everything between pool-acquire finishing and
        // the response read starting) — not split further since which of those two ran
        // is already visible from pool_hit.
        let connect_and_write_ms = connect_done_at.elapsed().as_secs_f64() * 1000.0 - recv_time.as_secs_f64() * 1000.0;
        let read_ms = recv_time.as_secs_f64() * 1000.0;
        let total_ms = sfw_start.elapsed().as_secs_f64() * 1000.0;
        info!("SFWTIMING addr={} pool_hit={} pool_acquire_ms={:.1} connect_and_write_ms={:.1} read_ms={:.1} total_ms={:.1}",
            addr, pool_hit, pool_acquire_ms, connect_and_write_ms, read_ms, total_ms);

        let response_envelope = MessageEnvelope::from_bytes(&buf)
            .context("Failed to deserialize response")?;

        let response = match response_envelope.message {
            Message::Response(response) => response,
            _ => anyhow::bail!("Expected Response message"),
        };

        // Return connection to pool
        {
            let mutex = {
                let entry = self.connection_pool
                    .entry(addr)
                    .or_insert_with(|| Arc::new(Mutex::new(std::collections::VecDeque::new())));
                Arc::clone(&*entry)
            };
            let mut queue = mutex.lock().await;
            if queue.len() < POOL_SIZE {
                queue.push_back(stream);
            } else {
                tokio::spawn(async move { let _ = stream.shutdown().await; });
            }
        }

        // ServerBusy on write path: retry with backoff rather than EIO.
        if let Response::Error { code: dfs_common::ErrorCode::ServerBusy, .. } = &response {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            return Box::pin(self.send_split_frame_write_request(addr, encoded_envelope, raw_data)).await;
        }

        self.node_health.record_success(addr).await;
        Ok(response)
    }

    /// Get file metadata from cluster with optional conditional fetch
    /// Returns Ok(Some(metadata)) if found and modified, Ok(None) if not found, Err if error
    /// If if_modified_since is provided and metadata hasn't changed, returns Ok(None) with NotModified indicator
    /// Ask the leader to restore a specific chunk's replica on a specific node,
    /// bypassing the normal healer's capacity-aware (content-hash-seeded) target
    /// selection — see HealChunkToNode's doc comment for why that matters here.
    /// Fire-and-forget: callers spawn this so a slow/failed heal request never
    /// adds latency to the write path it was triggered from. Best-effort only —
    /// if this particular request is lost, the node stays in canonical_write_nodes'
    /// miss-streak tracking and gets retried on the slot's next patch round anyway.
    pub async fn heal_chunk_to_node(&self, chunk_id: ChunkId, target_node: dfs_common::NodeId, file_id: Option<FileId>) {
        let request = Request::HealChunkToNode { chunk_id, target_node, file_id };
        let leader = { *self.leader_addr.read().await };
        let Some(leader_addr) = leader else {
            warn!("heal_chunk_to_node: no known leader — cannot request restore of {} on {}", chunk_id, target_node);
            return;
        };
        if let Err(e) = self.send_request(leader_addr, request).await {
            warn!("heal_chunk_to_node: request to leader {} for chunk {} -> {} failed: {}", leader_addr, chunk_id, target_node, e);
        }
    }

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
        // Testing with 4 chunks in flight (3 chunks of lookahead) to see impact on
        // sequential read throughput. May help hide network latency on slower links.
        // Previous value was 2 (1 chunk lookahead).
        let _ = chunk_size; // reserved for future adaptive tuning
        4
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
        has_active_writer: bool,
        client_write_seq: Option<u64>,
    ) -> Result<Vec<u8>> {
        if size == 0 || offset >= file_size as usize {
            // TEMP DIAGNOSTIC (2026-07-14): tracing a fast (<1ms) empty-read report on
            // rock5b right after a client restart — need to know whether file_size is
            // genuinely 0/stale here (a metadata convergence gap distinct from the
            // chunk_locations-stripped-on-warmup one) or whether this path isn't even
            // being hit and the fast return is happening somewhere else entirely.
            if size != 0 {
                info!("read_file: inode={} fast-empty-return offset={} size={} file_size={} (offset >= file_size)",
                    inode, offset, size, file_size);
            }
            return Ok(Vec::new());
        }

        // Store write_seq in a context that read operations can access.
        // Use the provided write_seq, or fall back to our internal counter.
        let write_seq = client_write_seq.or_else(|| self.write_seq.get(&file_id).map(|e| *e));

        let engine = self.read_engines.get_or_create(inode);

        // Refresh chunk map if stale or file grew — always in the background so reads are
        // never blocked by a leader round-trip. The stale snapshot is safe: it has valid
        // chunk locations; the next read after refresh completes will pick up the new map.
        const CHUNK_SIZE_USIZE: usize = 4 * 1024 * 1024;
        let current_chunk = (offset / CHUNK_SIZE_USIZE) as u32;

        let (mut chunk_map, mut chunk_offsets, mut nim) = engine.snapshot();

        // Pins down which of the three states (cold/synchronous-refresh,
        // non-empty-but-stale/background-refresh, or non-empty-and-fresh/served
        // straight from snapshot) a read actually observes. A non-empty chunk_map on
        // what should be a brand-new engine (get_or_create just ran) is itself a
        // finding — the constructor starts genuinely empty.
        //
        // debug!, not info!: this fires on EVERY read. Beyond the log volume, the
        // needs_refresh() call below exists only to build this message — at info level
        // tracing skips evaluating the fields entirely, so it costs nothing on the hot
        // read path unless someone is actually running the client at debug.
        debug!("read_file: inode={} pre-refresh snapshot: chunk_map.len()={} needs_refresh={}",
            inode, chunk_map.len(), engine.needs_refresh(file_size, current_chunk));

        if chunk_map.is_empty() {
            // Engine is cold. Check if this read is beyond the committed file size.
            // - If offset < file_size: data is committed on server, safe to refresh
            // - If offset >= file_size: data might be in write buffer only, return empty
            if has_active_writer && offset >= file_size as usize {
                // Read is beyond committed size with active writer — data is in write buffer.
                // The FUSE write-buffer path should have served this, but didn't (no slot).
                // Return empty; the flush path will feed the engine once chunk is committed.
                return Ok(Vec::new());
            }
            // Either no active writer, or read is within committed size — do synchronous refresh.
            // Do NOT force-clear refresh_in_progress here (removed 2026-07-14 — see the
            // convergence bug this caused): open() may have already spawned a background
            // prefetch that's still in flight. Blindly clearing the flag to guarantee this
            // caller wins the exchange raced that prefetch — two concurrent fetches both
            // writing engine.chunk_state and both signaling the same refresh_done Notify, so
            // a waiter could be woken by the *other* fetch's completion and snapshot before
            // its own fetch's data was actually written, observing an empty chunk map despite
            // "synchronous refresh" having just run. refresh_engine() below already handles
            // "someone else is already refreshing" correctly (waits on notified(), which by
            // construction only fires after that refresher's own update_chunk_map_window call
            // — see refresh_engine_flagged) — just let it do that instead of racing it.
            let sync_start = std::time::Instant::now();
            self.refresh_engine(&engine, file_id, file_size, current_chunk).await;
            info!("read_file: inode={} synchronous chunk map refresh took {:?}", inode, sync_start.elapsed());
            let snap = engine.snapshot();
            chunk_map = snap.0; chunk_offsets = snap.1; nim = snap.2;
        } else if engine.needs_refresh(file_size, current_chunk) {
            // Non-empty but stale — refresh in background, serve this read from current snapshot.
            let engine_clone = engine.clone();
            let client_clone = self.clone();
            tokio::spawn(async move {
                client_clone.refresh_engine(&engine_clone, file_id, file_size, current_chunk).await;
            });
        }

        if chunk_map.is_empty() {
            // Still empty after a synchronous refresh (our own, or one we waited on via
            // refresh_engine's refresh_in_progress guard). Only trust this as "the file
            // genuinely has zero chunks" (e.g. a VM disk image created via ftruncate and
            // never written — fully sparse) if the server has actually CONFIRMED that at
            // least once — never on an unconfirmed empty map. Without this check, a merely
            // SLOW cold-cache fetch (a big/sparse VM disk's first-ever chunk map fetch,
            // or one delayed behind a known legitimate multi-second server-side stall) that
            // outlasts refresh_engine's 10s concurrent-waiter timeout left every OTHER
            // concurrent reader on this inode falling through to here with a snapshot that
            // was never actually populated — silently returning zero-filled bytes for a
            // real, non-sparse file instead of erroring or waiting longer. That's a bigger
            // hazard than a returned error: those zeros can get written back by the VM's
            // own qcow2 layer as if they were the real data, turning a slow leader into
            // silent on-disk corruption (suspected root cause of the VM-108/111 boot
            // corruption incidents, 2026-07 — the "restart client fixes it" signature
            // matches a poisoned engine snapshot from exactly this race).
            //
            // offset < file_size was already checked at function entry, so a CONFIRMED
            // empty map here is a hole within the file's logical extent: return zero-filled
            // bytes, same as the "Hole (sparse file)" case below. Returning an empty Vec
            // signals EOF to FUSE/the kernel; O_DIRECT readers (e.g. QEMU with cache=none)
            // treat a short read at a non-EOF offset as an I/O error — which is exactly what
            // turns fdisk/mkfs/fsck on a freshly created VM disk into "lots of corruption",
            // so this path must stay zero-fill for the genuinely-confirmed-sparse case.
            if engine.confirmed_at_least_once.load(std::sync::atomic::Ordering::Relaxed) {
                let len = (file_size as usize).min(offset + size).saturating_sub(offset);
                return Ok(vec![0u8; len]);
            }
            anyhow::bail!(
                "inode={} chunk map unavailable (leader refresh never confirmed) — refusing to fabricate zero-filled data",
                inode
            );
        }

        // Chunks are content-addressed (ChunkId = hash of data), so there is no
        // staleness risk from caching: a modified page gets a new ChunkId and the
        // old cache entry is simply never requested again.  SQLite files previously
        // bypassed the cache, which also forced every small (4KB) page read through
        // the full-chunk path — fetching 4MB per 4KB read with no caching.  Now all
        // files, including SQLite, use the cache and the range-fetch path for small reads.
        let bypass_cache = false;

        let end = offset + size;
        let needed = InodeReadEngine::chunks_for_range(&chunk_offsets, offset, size);

        // Populate write_seq cache for only the chunks this read actually touches, so
        // read_chunk_from_server can include it in read requests for client-driven
        // staleness detection. Previously this inserted every chunk in the file's
        // chunk_map on every read — O(file's chunk count) per read instead of O(chunks
        // touched), and since chunk_ids are content-addressed (a new id per rewrite),
        // entries for old chunk_ids were never evicted — an unbounded leak over a long
        // mount lifetime. start_read_write_seq_cache_sweeper() (background sweeper)
        // bounds the leak from the other end.
        if let Some(ws) = write_seq {
            for (idx, _, _) in &needed {
                if let Some(loc) = chunk_map.get(*idx) {
                    self.read_write_seq_cache.insert(loc.chunk_id, (ws, Instant::now()));
                }
            }
        }

        if needed.is_empty() {
            // Hole (sparse file) — return zeros.
            let len = (file_size as usize).min(end).saturating_sub(offset);
            return Ok(vec![0u8; len]);
        }

        let nodes = self.cluster_nodes.read().await.clone();
        let selector = self.replica_selector.fetch_add(1, Ordering::Relaxed);

        const CHUNK_SIZE_BYTES: usize = 4 * 1024 * 1024;
        // Sequential access detection: current read continues from where the last ended,
        // within the same chunk. Sequential reads use the full-chunk path so pipeline
        // prefetch fires and each 4MB chunk is fetched once, not as N × 128KB RTTs.
        let last_end = engine.last_read_end.load(Ordering::Relaxed) as usize;
        let is_sequential = last_end > 0
            && offset <= last_end + size
            && offset + size > last_end
            && (offset / CHUNK_SIZE_BYTES) == (last_end.saturating_sub(1) / CHUNK_SIZE_BYTES);

        // Broad sequential: reading from offset 0 (start of file, assumed sequential) or
        // continuing exactly from where the last read ended (cross-chunk sequential).
        // Sequential reads always take the full-chunk path so the pipeline can prefetch
        // ahead. is_sequential (above) is kept for the within-chunk striped-read path.
        let is_broad_sequential = offset == 0 || (last_end > 0 && offset == last_end);

        // Range-fetch for random reads: fetches only the requested bytes rather than a
        // full 4MB chunk. Threshold raised to 1MB so that medium-sized random reads
        // (QEMU 64KB cluster reads at non-sequential offsets, our benchmark 32KB–512KB
        // random ops) avoid the 4MB amplification. Sequential reads are fully protected
        // by is_broad_sequential and never take this path regardless of size.
        const RANGE_FETCH_MAX: usize = 1024 * 1024;
        let use_range_fetch = !bypass_cache && !is_broad_sequential && size <= RANGE_FETCH_MAX && inode > 0;

        if use_range_fetch {
            let mut result_chunks: Vec<(usize, Arc<Vec<u8>>)> = Vec::new();

            // Gather per-chunk range tasks: (chunk_idx, chunk_start, range_offset_in_chunk, range_len, cid, primary, fallbacks)
            struct RangeFetch {
                idx: usize,
                chunk_start: usize,
                offset_in_chunk: usize,
                len_in_chunk: usize,
                cid: ChunkId,
                primary: SocketAddr,
                fallbacks: Vec<SocketAddr>,
            }
            let mut range_fetches: Vec<RangeFetch> = Vec::new();

            for (chunk_idx, chunk_start, chunk_size) in &needed {
                let idx = *chunk_idx;
                let chunk_start = *chunk_start;
                let chunk_size = *chunk_size;
                let loc = &chunk_map[idx];
                // Prefer the chunk_id confirmed by our own write path over the engine's
                // FILE_TABLE entry, which may lag behind the meta_queue drain. The client
                // is authoritative for chunks it has written.
                let cid = self.recent_chunk_writes
                    .get(&(inode, idx as u64))
                    .map(|e| e.0)
                    .unwrap_or(loc.chunk_id);

                let read_start = offset.max(chunk_start);
                let read_end = (offset + size).min(chunk_start + chunk_size);
                let offset_in_chunk = read_start - chunk_start;
                let len_in_chunk = read_end - read_start;

                // Check sub-chunk cache first. Key on read_start (the exact file byte offset
                // of the fetched data) so the lookup and store use the same coordinate.
                let cache_key = ByteRangeCacheKey { inode, file_offset: read_start as u64 };
                let cached = {
                    let mut byte_cache = self.byte_range_cache.shard(inode).lock().await;
                    if let Some(entry) = byte_cache.get(&cache_key) {
                        if entry.is_expired() {
                            byte_cache.pop(&cache_key);
                            None
                        } else if len_in_chunk <= entry.data.len() {
                            Some(Arc::clone(&entry.data))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };

                if let Some(cached_data) = cached {
                    // Cache stores exactly the fetched bytes starting at read_start.
                    // Trim to len_in_chunk in case a prior fetch was larger.
                    let slice = cached_data[..len_in_chunk.min(cached_data.len())].to_vec();
                    debug!("Sub-chunk cache HIT inode={} file_offset={} len={}",
                           inode, read_start, len_in_chunk);
                    result_chunks.push((idx, Arc::new(slice)));
                    continue;
                }

                // Check zero gap table for sparse file gaps.
                // This handles qcow2 sparse writes without caching megabytes of zeros.
                {
                    let gap_key = ZeroGapKey {
                        inode,
                        chunk_offset: chunk_start as u64,
                    };
                    let mut gap_table = self.zero_gap_table.shard(inode).lock().await;
                    if let Some(gaps) = gap_table.get_mut(&gap_key) {
                        // Check if requested range overlaps any gap
                        let mut found_gap = false;
                        gaps.retain(|gap| !gap.is_expired());

                        for gap in gaps.iter() {
                            if gap.start <= read_start as u64
                                && (read_start as u64 + len_in_chunk as u64) <= gap.end
                            {
                                // Entire requested range is within this gap - return zeros
                                let zeros = vec![0u8; len_in_chunk];
                                debug!("Zero gap HIT inode={} file_offset={} len={} gap={}..{}",
                                       inode, read_start, len_in_chunk, gap.start, gap.end);
                                result_chunks.push((idx, Arc::new(zeros)));
                                found_gap = true;
                                break;
                            }
                        }
                        if found_gap {
                            continue;
                        }
                    }
                }

                // Also try the full chunk_cache (another path may have loaded the full chunk).
                // Slice to [offset_in_chunk..offset_in_chunk+len_in_chunk] so the assembly
                // (which expects data starting at offset_in_chunk) gets correctly positioned bytes.
                if let Some(data) = self.chunk_cache.get(&cid) {
                    if offset_in_chunk + len_in_chunk <= data.len() {
                        let slice = Arc::new(data[offset_in_chunk..offset_in_chunk + len_in_chunk].to_vec());
                        result_chunks.push((idx, slice));
                        continue;
                    }
                }

                // Load-aware pick (same logic the prefetch/swarm paths use): prefers the
                // replica with fewer in-flight requests over a busier one, with chunk-hash
                // rotation breaking ties so chunks still fan out across replicas.
                let (primary, fallbacks) = self.pick_replica_by_load(loc, &nim, &nodes);
                self.node_inflight_inc(primary);

                range_fetches.push(RangeFetch { idx, chunk_start, offset_in_chunk, len_in_chunk, cid, primary, fallbacks });
            }

            // Fetch all missing byte ranges in parallel.
            if !range_fetches.is_empty() {
                let tasks: Vec<_> = range_fetches.iter().map(|rf| {
                    let client = self.clone();
                    let idx = rf.idx;
                    let chunk_start = rf.chunk_start;
                    let offset_in_chunk = rf.offset_in_chunk;
                    let len_in_chunk = rf.len_in_chunk;
                    let cid = rf.cid;
                    let primary = rf.primary;
                    let fallbacks = rf.fallbacks.clone();
                    let ws = write_seq; // Capture for async block
                    let inode = inode; // Capture for async block
                    let file_id = file_id; // Capture for the (file_id, chunk_idx) slot backstop
                    tokio::spawn(async move {
                        // Try primary then fallbacks.
                        let mut last_err = None;
                        let mut all_not_found = true;
                        for &addr in std::iter::once(&primary).chain(fallbacks.iter()) {
                            // Bound concurrent range-fetch requests this file has outstanding
                            // to this node so one high-queue-depth file can't monopolize a
                            // node's connections and starve other files reading from it.
                            // Waits (rather than skipping) once the cap is reached.
                            let permit_sem = client.range_fetch_permit_for(inode, addr);
                            let _permit = permit_sem.acquire_owned().await;
                            match client.read_chunk_range_from_server(
                                addr, cid, offset_in_chunk as u64, len_in_chunk as u64, ws,
                                Some((file_id, idx as u64)),
                            ).await {
                                Ok(data) => {
                                    // Per-range-read trace — fires on every single successful
                                    // fetch (tens of thousands under a random-read workload like
                                    // kdiskmark), dominating the log by volume with no decision
                                    // or state-transition content. debug! only.
                                    debug!("Range fetch: chunk {} off={} len={} → {} bytes",
                                          cid, offset_in_chunk, len_in_chunk, data.len());
                                    client.node_inflight_dec(primary);
                                    return Ok((idx, chunk_start, offset_in_chunk, data));
                                }
                                Err(e) => {
                                    let msg = e.to_string();
                                    if !msg.contains("Failed to open chunk file")
                                        && !msg.contains("Failed to read chunk range")
                                    {
                                        all_not_found = false;
                                    }
                                    last_err = Some(e);
                                }
                            }
                        }
                        client.node_inflight_dec(primary);
                        if all_not_found && last_err.is_some() {
                            Err(anyhow::anyhow!(
                                "Range chunk {} missing on all replicas — metadata may be stale", cid
                            ))
                        } else {
                            Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no replicas")))
                        }
                    })
                }).collect();

                let fetch_results = futures::future::join_all(tasks).await;

                // Collect results; queue stale-metadata failures for one metadata-refresh retry.
                let mut stale_range_retries: Vec<(usize, usize, usize, usize)> = Vec::new(); // (idx, chunk_start, offset_in_chunk, len_in_chunk)
                for (rf, res) in range_fetches.iter().zip(fetch_results) {
                    match res.context("Range fetch task panicked").and_then(|r| r) {
                        Ok((idx, chunk_start, offset_in_chunk, data)) => {
                            // Guard against a race where a concurrent flush rotated this
                            // chunk to a new chunk_id WHILE this fetch was in flight (T35:
                            // rapid same-chunk MultiPatch rotation). The fetch was issued
                            // against rf.cid, captured before the request went out; if our
                            // own write path has since confirmed a newer id for this
                            // (inode, idx), these bytes are from the stale base and must not
                            // be cached or returned under the (inode, file_offset) key — a
                            // later read at the same key would get a stale HIT. Retry against
                            // the fresh id instead, same as a missing-chunk stale response.
                            let is_stale = self.recent_chunk_writes
                                .get(&(inode, idx as u64))
                                .map(|e| e.0 != rf.cid)
                                .unwrap_or(false);
                            if is_stale {
                                warn!("Range fetch for chunk {} (idx={}) completed after a newer write rotated the chunk — discarding stale bytes and retrying", rf.cid, idx);
                                stale_range_retries.push((rf.idx, rf.chunk_start, rf.offset_in_chunk, rf.len_in_chunk));
                                continue;
                            }
                            let arc = Arc::new(data);
                            {
                                let cache_key = ByteRangeCacheKey {
                                    inode,
                                    file_offset: (chunk_start + offset_in_chunk) as u64,
                                };
                                let cached_entry = CachedChunk {
                                    data: Arc::clone(&arc),
                                    chunk_size: arc.len(),
                                    cached_at: std::time::Instant::now(),
                                };
                                self.byte_range_cache.shard(inode).lock().await.put(cache_key, cached_entry);
                            }
                            result_chunks.push((idx, arc));
                        }
                        Err(e) if e.to_string().contains("metadata may be stale") => {
                            error!("Range chunk {} missing on all replicas — will refresh metadata and retry", rf.cid);
                            stale_range_retries.push((rf.idx, rf.chunk_start, rf.offset_in_chunk, rf.len_in_chunk));
                        }
                        Err(e) => return Err(e),
                    }
                }

                // Retry stale-metadata range chunks. Prefer chunk_id from our own write
                // path (recent_chunk_writes) over the leader's FILE_TABLE, which may lag
                // behind the meta_queue drain. The client is authoritative for what it wrote.
                if !stale_range_retries.is_empty() {
                    use std::sync::atomic::Ordering;
                    engine.refresh_in_progress.store(false, Ordering::Release);
                    self.refresh_engine(&engine, file_id, file_size, 0).await;
                    let snap = engine.snapshot();
                    let fresh_map = snap.0;
                    let fresh_nim = snap.2;
                    let fresh_nodes = self.cluster_nodes.read().await.clone();
                    for (idx, chunk_start, offset_in_chunk, len_in_chunk) in stale_range_retries {
                        // recent_chunk_writes holds the last chunk_id confirmed by our write
                        // path. Prefer it over the leader's FILE_TABLE for this chunk.
                        let fresh_cid = self.recent_chunk_writes
                            .get(&(inode, idx as u64))
                            .map(|e| e.0)
                            .or_else(|| fresh_map.get(idx).map(|l| l.chunk_id))
                            .ok_or_else(|| anyhow::anyhow!("Chunk at index {} missing from fresh metadata", idx))?;

                        let (fp, ffb) = match fresh_map.get(idx).and_then(|loc| InodeReadEngine::resolve_primary(
                            loc, &fresh_nim, &fresh_nodes, selector + idx as u64,
                        )) {
                            Some(pf) => pf,
                            None => {
                                let p = fresh_nodes[selector as usize % fresh_nodes.len()];
                                (p, fresh_nodes.iter().filter(|&&a| a != p).copied().collect())
                            }
                        };
                        // Try primary then all fallbacks with the preferred chunk_id.
                        let mut retry_data: Option<Vec<u8>> = None;
                        for &addr in std::iter::once(&fp).chain(ffb.iter()) {
                            if let Ok(data) = self.read_chunk_range_from_server(
                                addr, fresh_cid, offset_in_chunk as u64, len_in_chunk as u64, None,
                                Some((file_id, idx as u64)),
                            ).await {
                                retry_data = Some(data);
                                break;
                            }
                        }
                        // Fall back to the leader's own fresh chunk_id if the preferred one
                        // (recent_chunk_writes) is unreachable everywhere. "The client is
                        // authoritative for what it wrote" no longer holds unconditionally:
                        // a server can now change a chunk's identity after acking our write
                        // without a further client-visible RPC (deferred chunk-patch
                        // consolidation folds a stacked overlay chain in the background) —
                        // recent_chunk_writes can legitimately point at an identity that's
                        // been superseded on every replica by the time we read it back.
                        if retry_data.is_none() {
                            if let Some(fresh_loc) = fresh_map.get(idx) {
                                if fresh_loc.chunk_id != fresh_cid {
                                    let (fp2, ffb2) = InodeReadEngine::resolve_primary(
                                        fresh_loc, &fresh_nim, &fresh_nodes, selector + idx as u64,
                                    ).unwrap_or_else(|| {
                                        let p = fresh_nodes[selector as usize % fresh_nodes.len()];
                                        (p, fresh_nodes.iter().filter(|&&a| a != p).copied().collect())
                                    });
                                    for &addr in std::iter::once(&fp2).chain(ffb2.iter()) {
                                        if let Ok(data) = self.read_chunk_range_from_server(
                                            addr, fresh_loc.chunk_id, offset_in_chunk as u64, len_in_chunk as u64, None,
                                            Some((file_id, idx as u64)),
                                        ).await {
                                            retry_data = Some(data);
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        // The single refresh+retry above is a one-shot: it raced a
                        // metadata-convergence window where every replica — and the
                        // leader we just refreshed from — was momentarily behind the
                        // write that retired this chunk_id. That's transient (replication
                        // converges in a few ms), so back off briefly and try again a few
                        // rounds, re-refreshing each time so a later round picks up the
                        // now-committed state, rather than surfacing an EIO. An EIO on a
                        // VM disk read flips the guest read-only, so a rare few hundred ms
                        // of latency is vastly the better trade. Bounded, so a genuinely
                        // lost chunk still fails instead of hanging the read forever.
                        //
                        // This is the last 0.02% the (file_id, chunk_idx) slot backstop
                        // couldn't reach: the backstop needs *some* node to already know
                        // the slot's new occupant; this loop waits for that to become true.
                        let mut data = retry_data;
                        if data.is_none() {
                            const RANGE_RETRY_ROUNDS: usize = 5;
                            const RANGE_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);
                            for round in 0..RANGE_RETRY_ROUNDS {
                                tokio::time::sleep(RANGE_RETRY_BACKOFF).await;
                                engine.refresh_in_progress.store(false, Ordering::Release);
                                self.refresh_engine(&engine, file_id, file_size, 0).await;
                                let rsnap = engine.snapshot();
                                let rmap = rsnap.0;
                                let rnim = rsnap.2;
                                let rnodes = self.cluster_nodes.read().await.clone();
                                let rcid = self.recent_chunk_writes
                                    .get(&(inode, idx as u64))
                                    .map(|e| e.0)
                                    .or_else(|| rmap.get(idx).map(|l| l.chunk_id));
                                let Some(rcid) = rcid else { continue };
                                let (rfp, rffb) = match rmap.get(idx).and_then(|loc| InodeReadEngine::resolve_primary(
                                    loc, &rnim, &rnodes, selector + idx as u64,
                                )) {
                                    Some(pf) => pf,
                                    None => {
                                        let p = rnodes[selector as usize % rnodes.len()];
                                        (p, rnodes.iter().filter(|&&a| a != p).copied().collect())
                                    }
                                };
                                for &addr in std::iter::once(&rfp).chain(rffb.iter()) {
                                    if let Ok(d) = self.read_chunk_range_from_server(
                                        addr, rcid, offset_in_chunk as u64, len_in_chunk as u64, None,
                                        Some((file_id, idx as u64)),
                                    ).await {
                                        data = Some(d);
                                        break;
                                    }
                                }
                                if data.is_some() {
                                    info!("Range fetch for (inode={}, idx={}) recovered after {} backoff round(s)", inode, idx, round + 1);
                                    break;
                                }
                            }
                        }
                        let data = data.ok_or_else(|| anyhow::anyhow!(
                            "Failed to fetch range for chunk {} after metadata refresh + backoff retries", fresh_cid
                        ))?;
                        let arc = Arc::new(data);
                        {
                            let cache_key = ByteRangeCacheKey {
                                inode,
                                file_offset: (chunk_start + offset_in_chunk) as u64,
                            };
                            self.byte_range_cache.shard(inode).lock().await.put(cache_key, CachedChunk {
                                data: Arc::clone(&arc),
                                chunk_size: arc.len(),
                                cached_at: std::time::Instant::now(),
                            });
                        }
                        result_chunks.push((idx, arc));
                    }
                }
            }

            // Assemble response.
            // Range-fetched data starts at offset_in_chunk (not chunk_start), so we
            // copy data[0..len] directly to out[out_start..out_end] — no local_start offset.
            result_chunks.sort_by_key(|(i, _)| *i);
            let clamped_size = size.min((file_size as usize).saturating_sub(offset));
            let mut out = vec![0u8; clamped_size];
            for (chunk_idx, data) in &result_chunks {
                let (chunk_start, chunk_size) = chunk_offsets[*chunk_idx];
                let read_start = offset.max(chunk_start);
                let read_end = (offset + size).min(chunk_start + chunk_size);
                if read_end <= read_start { continue; }
                let out_start = read_start - offset;
                // Clamp out_end to the output buffer size. loc.size may be larger than
                // the actual file content (e.g. chunk rounded to a block boundary) but
                // the output buffer is bounded by clamped_size = file_size - offset.
                let out_end = (read_end - offset).min(clamped_size);
                if out_end <= out_start { continue; }
                let copy_len = (out_end - out_start).min(data.len());
                out[out_start..out_start + copy_len].copy_from_slice(&data[..copy_len]);
            }
            engine.last_read_end.store((offset + clamped_size) as u64, Ordering::Relaxed);
            return Ok(out);
        }

        // --- Full-chunk path (sequential reads, large reads, SQLite) ---

        // --- Cache check ---
        let mut result_chunks: Vec<(usize /*chunk_idx*/, Arc<Vec<u8>>)> = Vec::new();
        let mut to_fetch: Vec<(usize, ChunkId, SocketAddr, Vec<SocketAddr>)> = Vec::new();
        let mut to_wait: Vec<(usize, ChunkId, ChunkLocation)> = Vec::new();

        for (chunk_idx, _chunk_start, _chunk_size) in &needed {
            let idx = *chunk_idx;
            let loc = &chunk_map[idx];
            // Prefer the chunk_id confirmed by our own write path over chunk_map, which is
            // fed by the leader's RCL-merged view and can lag behind (or, if multiple rapid
            // same-chunk patches raced with equal client_write_seq, regress past) our most
            // recent write. The client is authoritative for chunks it has written — same
            // reasoning as the range-fetch path above.
            let cid = self.recent_chunk_writes
                .get(&(inode, idx as u64))
                .map(|e| e.0)
                .unwrap_or(loc.chunk_id);

            // 1. Chunk cache (skip for SQLite).
            if !bypass_cache {
                if let Some(data) = self.chunk_cache.get(&cid) {
                    result_chunks.push((idx, data));
                    continue;
                }
            }

            // 2. Atomically claim this chunk for fetching (DashSet::insert returns true iff
            //    newly inserted). This is a single locked operation — no TOCTOU race between
            //    concurrent FUSE reads that both see "not in-flight" and both start a fetch.
            if !engine.in_flight.insert(cid) {
                to_wait.push((idx, cid, loc.clone()));
                continue;
            }

            // 3. We own the fetch — pick replica and queue it.
            let (primary, fallbacks) = self.pick_replica_by_load(loc, &nim, &nodes);
            self.node_inflight_inc(primary);
            to_fetch.push((idx, cid, primary, fallbacks));
        }

        // --- Pipeline lookahead: speculatively fetch the next N chunks. ---
        // Fire on every sequential read, not only on misses. Without this, cache-hit reads
        // produce no new prefetch — leaving the chunk two ahead unfetched and causing the
        // miss→hit→miss→hit alternating stall pattern.
        // Fire-and-forget — their results go into chunk_cache; we don't await them here.
        if !needed.is_empty() && !bypass_cache {
            let last_required_idx = needed.last().map(|(i, _, _)| *i).unwrap_or(0);
            let lookahead_candidates = engine.pipeline_lookahead(
                last_required_idx, chunk_map.len(), &chunk_map,
            );

            for (la_idx, la_cid) in lookahead_candidates {
                // Skip if already cached — no need to fetch or mark in-flight.
                if self.chunk_cache.get(&la_cid).is_some() {
                    continue;
                }

                let loc = &chunk_map[la_idx];
                let (primary, fallbacks) = self.pick_replica_by_load(loc, &nim, &nodes);

                // Cap background fetches per node: spinning HDDs can't efficiently
                // serve multiple concurrent requests. If the least-loaded replica is
                // already handling a background fetch, skip — main reads go through
                // regardless; the next read will re-evaluate after a task completes.
                let current_load = self.node_inflight.get(&primary)
                    .map(|e| e.load(Ordering::Relaxed))
                    .unwrap_or(0);
                if current_load >= MAX_BACKGROUND_PER_NODE {
                    continue;
                }

                // Atomically claim — if another task already inserted (concurrent FUSE read
                // also firing lookahead), skip rather than double-fetching.
                if !engine.in_flight.insert(la_cid) {
                    continue;
                }
                self.node_inflight_inc(primary);
                let client = self.clone();
                let eng = engine.clone();
                tokio::spawn(async move {
                    let result = client.fetch_chunk_with_fallback(la_cid, primary, &fallbacks, None).await;
                    client.node_inflight_dec(primary);
                    match result {
                        Ok(data) => {
                            let arc = Arc::new(data);
                            client.chunk_cache.insert(la_cid, arc);
                            client.chunk_landed.notify_waiters();
                        }
                        Err(e) => debug!("Pipeline lookahead fetch failed for {}: {}", la_cid, e),
                    }
                    eng.in_flight.remove(&la_cid);
                    client.chunk_landed.notify_waiters();
                });
            }
        }

        // --- Fetch required chunks (sequential pipeline for full-chunk sequential reads) ---
        if !to_fetch.is_empty() {
            // Measure fetch time for chunk 0 to adaptively set stagger delay
            let start_time = std::time::Instant::now();
            let first_idx_in_batch = to_fetch.first().map(|(idx, _, _, _)| *idx);

            let fetch_results: Vec<(usize, ChunkId, Result<Vec<u8>>)> =
            if is_sequential && to_fetch.len() > 0 {
                // Use striped reads for full-chunk sequential fetches: split each chunk across
                // both replicas and fetch the halves in parallel. This doubles effective read
                // bandwidth — each replica only transfers 2MB instead of 4MB, but both transfer
                // simultaneously, keeping both links busy.
                let tasks: Vec<_> = to_fetch.iter().map(|(idx, cid, primary, fallbacks)| {
                    let client = self.clone();
                    let idx = *idx;
                    let cid = *cid;
                    let primary = *primary;
                    let fallbacks = fallbacks.clone();
                    let loc = chunk_map.get(idx).cloned();
                    tokio::spawn(async move {
                        let data = if STRIPED_READ_ENABLED {
                            if let Some(loc) = loc {
                                if loc.nodes.len() >= 2 && loc.size == 4 * 1024 * 1024 {
                                    let file_offset = loc.file_offset.unwrap_or(0);
                                    client.read_chunk_striped(cid, &loc, file_offset).await
                                } else {
                                    client.fetch_chunk_with_fallback(cid, primary, &fallbacks, None).await
                                }
                            } else {
                                client.fetch_chunk_with_fallback(cid, primary, &fallbacks, None).await
                            }
                        } else {
                            let _ = loc;
                            client.fetch_chunk_with_fallback(cid, primary, &fallbacks, None).await
                        };
                        client.node_inflight_dec(primary);
                        (idx, cid, data)
                    })
                }).collect();
                futures::future::join_all(tasks).await.into_iter()
                    .map(|r| r.unwrap_or_else(|e| {
                        let dummy = ChunkId::from_hash([0u8; 32]);
                        (0usize, dummy, Err(anyhow::anyhow!("task panicked: {}", e)))
                    }))
                    .collect()
            } else {
                // Parallel path for random reads — fetch full chunks (cached for future reads).
                let tasks: Vec<_> = to_fetch.iter().map(|(idx, cid, primary, fallbacks)| {
                    let client = self.clone();
                    let idx = *idx;
                    let cid = *cid;
                    let primary = *primary;
                    let fallbacks = fallbacks.clone();
                    tokio::spawn(async move {
                        let data = client.fetch_chunk_with_fallback(cid, primary, &fallbacks, None).await;
                        client.node_inflight_dec(primary);
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

            // Update fetch timing for adaptive pipeline gating.
            // Exponential moving average so the estimate tracks network conditions.
            if !fetch_results.is_empty() {
                let elapsed_ms = (start_time.elapsed().as_millis() as u64)
                    .max(1) / to_fetch.len().max(1) as u64; // per-chunk avg
                let prev = engine.last_chunk_fetch_ms.load(Ordering::Relaxed);
                let smoothed = (prev * 7 + elapsed_ms) / 8; // EMA α=0.125
                engine.last_chunk_fetch_ms.store(smoothed, Ordering::Relaxed);
            }

            // Collect results; cache full chunks; remove from in-flight.
            // Always remove from in_flight before propagating errors — a leaked entry
            // causes every subsequent read for that chunk to wait 1s for a timeout.
            let mut stale_retries: Vec<(usize, ChunkId)> = Vec::new();
            for (idx, cid, res) in fetch_results {
                engine.in_flight.remove(&cid);
                // Wake any waiter regardless of success — a failure also unblocks
                // the waiter (which falls back to fetching directly).
                self.chunk_landed.notify_waiters();
                match res {
                    Ok(data) => {
                        let arc = Arc::new(data);
                        if !bypass_cache {
                            self.chunk_cache.insert(cid, Arc::clone(&arc));
                            self.chunk_landed.notify_waiters();
                        }
                        result_chunks.push((idx, arc));
                    }
                    Err(e) => {
                        // Every replica failed for this chunk_id — whether because they all
                        // said "not found" (metadata is stale: chunk was patched/folded since
                        // we last fetched the map) or some other failure (timeout, transient
                        // overload, a slow node during a known legitimate multi-second stall —
                        // see read_file's REGRESSION comment history). Both shapes get the same
                        // bounded refresh-and-retry treatment below rather than surfacing EIO
                        // immediately: an EIO on a VM disk read flips the guest read-only, so a
                        // bounded few seconds of latency is vastly the better trade, and this
                        // was previously only extended to the "metadata may be stale" message —
                        // a real fetch failure with a different message (e.g. a plain
                        // connectivity error) went straight to EIO with no retry at all, even
                        // though the correct current chunk_id was fully healthy on every
                        // replica the whole time (confirmed live on staging 2026-07-27: VM-111
                        // dd EIO on a chunk verified present + hash-correct on all 3 replicas).
                        // Bounded by STALE_RETRY_DELAYS_MS below — not unconditional retry.
                        warn!("Chunk {} failed on all replicas ({}) — will refresh metadata and retry", cid, e);
                        stale_retries.push((idx, cid));
                    }
                }
            }

            // Retry any stale-metadata chunks with a fresh chunk_map from the leader.
            //
            // Strategy (in order of preference):
            //   1. recent_chunk_writes: session-local record of the last chunk_id and nodes
            //      we wrote to. Available immediately, no leader needed — covers the common
            //      case where the chunk was rewritten in this session and the read engine
            //      hasn't caught up yet, or the leader is briefly down after an election.
            //   2. Leader chunk_map refresh: for chunks written by other sessions or when
            //      recent_chunk_writes doesn't have a fresher id. Retried with exponential
            //      backoff (up to ~7s) so a leader election has time to complete.
            if !stale_retries.is_empty() {
                use std::sync::atomic::Ordering;
                const STALE_RETRY_DELAYS_MS: &[u64] = &[0, 200, 500, 1000, 2000, 3000];
                let inode = engine.inode;
                let mut resolved: Vec<(usize, Arc<Vec<u8>>)> = Vec::new();
                let mut remaining = stale_retries;
                let mut last_err: Option<anyhow::Error> = None;
                'stale: for (attempt, &delay_ms) in STALE_RETRY_DELAYS_MS.iter().enumerate() {
                    if delay_ms > 0 {
                        tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                    }
                    // Always try to refresh from the leader on each attempt.
                    // This is async and cheap — it gives us updated locations for chunks
                    // NOT in recent_chunk_writes (other-session writes) and keeps the
                    // engine up to date after a newly elected leader comes online.
                    engine.refresh_in_progress.store(false, Ordering::Release);
                    self.refresh_engine(&engine, file_id, file_size, 0).await;
                    let snap = engine.snapshot();
                    let fresh_map = snap.0;
                    let fresh_nim = snap.2;
                    let fresh_nodes = self.cluster_nodes.read().await.clone();
                    let mut still_stale: Vec<(usize, ChunkId)> = Vec::new();
                    for (idx, stale_cid) in remaining {
                        // --- Source 1: session-local recent_chunk_writes ---
                        // If we wrote this chunk in this session we know exactly where it
                        // is and what its current hash is — no leader round-trip needed.
                        let local_loc = self.recent_chunk_writes
                            .get(&(inode, idx as u64))
                            .filter(|r| {
                                let (cid, fid, _, _) = r.value();
                                *fid == file_id && *cid != stale_cid
                            })
                            .map(|r| {
                                let (cid, _, _, nodes) = r.value();
                                (*cid, nodes.clone())
                            });

                        if let Some((local_cid, local_nodes)) = local_loc {
                            let fallbacks: Vec<SocketAddr> = local_nodes.iter()
                                .filter_map(|nid| fresh_nim.get(nid).copied())
                                .collect();
                            let primary = fallbacks.first().copied()
                                .unwrap_or_else(|| fresh_nodes[selector as usize % fresh_nodes.len()]);
                            let fallback_rest: Vec<SocketAddr> = fallbacks.iter()
                                .skip(1).copied().collect();
                            match self.fetch_chunk_with_fallback(local_cid, primary, &fallback_rest, None).await {
                                Ok(data) => {
                                    let arc = Arc::new(data);
                                    if !bypass_cache {
                                        self.chunk_cache.insert(local_cid, Arc::clone(&arc));
                                        self.chunk_landed.notify_waiters();
                                    }
                                    resolved.push((idx, arc));
                                    continue; // chunk resolved — move to next
                                }
                                Err(_) => {
                                    // Local nodes don't have it either; fall through to
                                    // leader-refresh path below.
                                    //
                                    // L4 (2026-07-22 fold-mapping data-loss incident):
                                    // local_cid is now confirmed missing on every node this
                                    // session recorded for it — purge this (inode, idx) entry
                                    // so it can't keep re-pinning a dead chunk_id (e.g. one a
                                    // background fold replaced) on every future read of this
                                    // slot. Only remove if it still points at exactly the
                                    // chunk_id just confirmed missing — a concurrent write
                                    // may have already replaced this entry with a newer one.
                                    self.recent_chunk_writes.remove_if(&(inode, idx as u64), |_, v| v.0 == local_cid);
                                }
                            }
                        }

                        // --- Source 2: leader chunk_map refresh ---
                        if let Some(fresh_loc) = fresh_map.get(idx) {
                            let fresh_cid = fresh_loc.chunk_id;
                            // Same chunk_id as the one that failed — leader hasn't
                            // updated yet; queue for the next backoff round.
                            if fresh_cid == stale_cid && attempt + 1 < STALE_RETRY_DELAYS_MS.len() {
                                still_stale.push((idx, stale_cid));
                                continue;
                            }
                            let (fp, ffb) = match InodeReadEngine::resolve_primary(
                                fresh_loc, &fresh_nim, &fresh_nodes, selector + idx as u64,
                            ) {
                                Some(pf) => pf,
                                None => {
                                    let p = fresh_nodes[selector as usize % fresh_nodes.len()];
                                    (p, fresh_nodes.iter().filter(|&&a| a != p).copied().collect())
                                }
                            };
                            match self.fetch_chunk_with_fallback(fresh_cid, fp, &ffb, None).await {
                                Ok(data) => {
                                    let arc = Arc::new(data);
                                    if !bypass_cache {
                                        self.chunk_cache.insert(fresh_cid, Arc::clone(&arc));
                                        self.chunk_landed.notify_waiters();
                                    }
                                    resolved.push((idx, arc));
                                }
                                Err(e) => {
                                    // Same broadened treatment as the initial-failure classification
                                    // above (not gated on "metadata may be stale" specifically) —
                                    // still bounded by STALE_RETRY_DELAYS_MS, so this can't loop
                                    // past the fixed schedule regardless of error shape.
                                    if attempt + 1 < STALE_RETRY_DELAYS_MS.len() {
                                        still_stale.push((idx, fresh_cid));
                                    } else {
                                        last_err = Some(e);
                                        break 'stale;
                                    }
                                }
                            }
                        } else if attempt + 1 < STALE_RETRY_DELAYS_MS.len() {
                            // Chunk not in leader map yet (election in progress); retry.
                            still_stale.push((idx, stale_cid));
                        } else {
                            last_err = Some(anyhow::anyhow!("Chunk at index {} missing from leader map after {} retries", idx, attempt + 1));
                            break 'stale;
                        }
                    }
                    remaining = still_stale;
                    if remaining.is_empty() {
                        break 'stale;
                    }
                }
                if let Some(e) = last_err {
                    return Err(e);
                }
                result_chunks.extend(resolved);
            }
        }

        // --- Wait for in-flight chunks fetched by concurrent requests ---
        for (idx, cid, loc) in to_wait {
            let data = self.wait_for_chunk_in_cache(cid, &engine, &loc).await?;
            result_chunks.push((idx, data));
        }

        // Advance pipeline_head to one past the last chunk we just consumed.
        // The swarming chain reaction gates itself on pipeline_head + MAX_AHEAD, so
        // this must be updated before swarming fires, otherwise pipeline_head stays 0
        // forever and chains only run for the first 4 chunks of any sequential read.
        if let Some((last_idx, _, _)) = needed.last() {
            engine.pipeline_head.fetch_max(last_idx + 1, Ordering::Relaxed);
        }

        // --- Adaptive staggered 2-chunk swarming for sequential reads ---
        // If we just fetched chunks and they look sequential, proactively fetch the next
        // 2 chunks with an adaptive stagger (based on chunk 0's fetch time / 2) to avoid
        // connection/disk/network contention while auto-adapting to any network speed.
        // This keeps the pipeline full without the overhead of continuous prefetching.
        // Don't start swarming on the very first chunk to keep initial latency minimal.
        if !to_fetch.is_empty() && !bypass_cache && needed.len() > 0 {
            let last_fetched_idx = needed.last().map(|(i, _, _)| *i).unwrap_or(0);
            let is_sequential = needed.len() == 1 || needed.windows(2).all(|w| w[1].0 == w[0].0 + 1);

            if is_sequential && last_fetched_idx > 0 && last_fetched_idx + 1 < chunk_map.len() {
                // Spawn staggered fetches for next 2 chunks
                // Skip sparse-hole placeholders (nodes.is_empty()) — there's no real
                // chunk to prefetch and no node holds the all-zero chunk_id.
                let swarm_indices = vec![last_fetched_idx + 1]
                    .into_iter()
                    .filter(|&idx| idx < chunk_map.len() && !chunk_map[idx].nodes.is_empty())
                    .collect::<Vec<_>>();

                for (swarm_offset, swarm_idx) in swarm_indices.iter().enumerate() {
                    let swarm_cid = chunk_map[*swarm_idx].chunk_id;

                    // Only fetch if not already cached or in-flight
                    let should_swarm = self.chunk_cache.get(&swarm_cid).is_none()
                        && !engine.in_flight.contains(&swarm_cid);

                    if should_swarm {
                        let swarm_loc = &chunk_map[*swarm_idx];
                        let (primary, fallbacks) = self.pick_replica_by_load(swarm_loc, &nim, &nodes);

                        // Skip if the least-loaded replica is already handling a background
                        // fetch. Spinning HDDs are fastest with one sequential request at a
                        // time; piling up concurrent requests causes seek contention and
                        // degrades throughput across successive file reads.
                        let current_load = self.node_inflight.get(&primary)
                            .map(|e| e.load(Ordering::Relaxed))
                            .unwrap_or(0);
                        if current_load >= MAX_BACKGROUND_PER_NODE {
                            continue;
                        }

                        if !engine.in_flight.insert(swarm_cid) {
                            continue;
                        }
                        self.node_inflight_inc(primary);

                        // Adaptive stagger: use half of chunk 0's fetch time to ensure chunk N+2
                        // starts when chunk N+1 is ~50% complete. This auto-adapts to any network
                        // speed (1G, 10G, etc.) without manual tuning.
                        let base_stagger_ms = engine.last_chunk_fetch_ms.load(Ordering::Relaxed) / 2;
                        let stagger_ms = swarm_offset as u64 * base_stagger_ms;

                        let client = self.clone();
                        let eng = engine.clone();
                        let idx_copy = *swarm_idx;
                        tokio::spawn(async move {
                            if stagger_ms > 0 {
                                tokio::time::sleep(tokio::time::Duration::from_millis(stagger_ms)).await;
                            }
                            let swarm_result = client.fetch_chunk_with_fallback(swarm_cid, primary, &fallbacks, None).await;
                            client.node_inflight_dec(primary);
                            match swarm_result {
                                Ok(data) => {
                                    client.chunk_cache.insert(swarm_cid, Arc::new(data));
                                    client.chunk_landed.notify_waiters();
                                    debug!("Swarming: fetched chunk {} (stagger {}ms)", idx_copy, stagger_ms);

                                    // Chain reaction: spawn the next chunk in sequence to maintain pipeline
                                    // But limit to MAX_AHEAD chunks beyond the pipeline_head to avoid runaway prefetch
                                    const MAX_AHEAD: usize = 4;
                                    let next_idx = idx_copy + 2;
                                    let pipeline_pos = eng.pipeline_head.load(Ordering::Relaxed);

                                    // Only chain if we're not too far ahead of the read position
                                    if next_idx < pipeline_pos + MAX_AHEAD {
                                        if let Some(e) = client.read_engines.get(eng.inode) {
                                            let (cm, _co, nim) = e.snapshot();
                                            // Skip sparse-hole placeholders — no real chunk/node to chase.
                                            if next_idx < cm.len() && !cm[next_idx].nodes.is_empty() {
                                                let next_cid = cm[next_idx].chunk_id;
                                                if client.chunk_cache.get(&next_cid).is_none() {
                                                    let chain_nodes = client.cluster_nodes.read().await.clone();
                                                    let (next_primary, next_fallbacks) = client.pick_replica_by_load(&cm[next_idx], &nim, &chain_nodes);
                                                    // Same per-node cap for chain fetches.
                                                    let chain_load = client.node_inflight.get(&next_primary)
                                                        .map(|e| e.load(Ordering::Relaxed))
                                                        .unwrap_or(0);
                                                    if chain_load < MAX_BACKGROUND_PER_NODE && eng.in_flight.insert(next_cid) {
                                                        client.node_inflight_inc(next_primary);
                                                        let chain_client = client.clone();
                                                        let chain_eng = eng.clone();
                                                        tokio::spawn(async move {
                                                            let chain_result = chain_client.fetch_chunk_with_fallback(next_cid, next_primary, &next_fallbacks, None).await;
                                                            chain_client.node_inflight_dec(next_primary);
                                                            match chain_result {
                                                                Ok(chain_data) => {
                                                                    chain_client.chunk_cache.insert(next_cid, Arc::new(chain_data));
                                                                    chain_client.chunk_landed.notify_waiters();
                                                                    debug!("Swarming: chained chunk {}", next_idx);
                                                                }
                                                                Err(e) => {
                                                                    debug!("Swarming: chain failed for chunk {}: {}", next_idx, e);
                                                                }
                                                            }
                                                            chain_eng.in_flight.remove(&next_cid);
                                                        });
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    debug!("Swarming failed for chunk {}: {}", idx_copy, e);
                                }
                            }
                            eng.in_flight.remove(&swarm_cid);
                        });
                    }
                }
            }
        }

        // --- Assemble the response ---
        result_chunks.sort_by_key(|(i, _)| *i);

        // Pre-fill with zeros so sparse gaps within the read range are correct.
        // Without this, reads that span a physical chunk boundary into a sparse
        // hole return a short buffer — FUSE interprets that as EOF.
        let clamped_size = size.min((file_size as usize).saturating_sub(offset));
        let mut out = vec![0u8; clamped_size];
        for (chunk_idx, data) in &result_chunks {
            let (chunk_start, chunk_size) = chunk_offsets[*chunk_idx];
            let read_start = offset.max(chunk_start);
            let read_end = (offset + size).min(chunk_start + chunk_size);
            if read_end <= read_start { continue; }
            let local_start = read_start - chunk_start;
            let local_end = read_end - chunk_start;
            let out_start = read_start - offset;
            // Clamp out_end to the output buffer — loc.size may exceed actual file content.
            let out_end = (read_end - offset).min(clamped_size);
            if out_end <= out_start { continue; }
            if local_end > data.len() {
                if local_start < data.len() {
                    let copy_len = (data.len() - local_start).min(out_end - out_start);
                    out[out_start..out_start + copy_len].copy_from_slice(&data[local_start..local_start + copy_len]);
                }
            } else {
                let clamped_local_end = local_end.min(local_start + (out_end - out_start));
                out[out_start..out_end].copy_from_slice(&data[local_start..clamped_local_end]);
            }
        }

        // Record where this read ended so the next read can detect sequential access.
        engine.last_read_end.store((offset + clamped_size) as u64, Ordering::Relaxed);

        Ok(out)
    }

    /// Pick the replica with the fewest in-flight fetches.
    /// Ties (the common case — all replicas equally loaded) break by a hash of
    /// chunk_id rather than by address, so different chunks that share the same
    /// replica set fan out across all of them instead of always favoring the
    /// lowest-address node. Returns (primary, remaining_in_load_order).
    fn pick_replica_by_load(
        &self,
        loc: &ChunkLocation,
        nim: &HashMap<dfs_common::NodeId, SocketAddr>,
        cluster_nodes: &[SocketAddr],
    ) -> (SocketAddr, Vec<SocketAddr>) {
        let mut addrs: Vec<SocketAddr> = loc.nodes.iter()
            .filter_map(|nid| nim.get(nid).copied())
            .collect();

        if addrs.is_empty() {
            let p = cluster_nodes.iter()
                .min_by_key(|&&a| self.node_inflight.get(&a)
                    .map(|e| e.load(Ordering::Relaxed))
                    .unwrap_or(0))
                .copied()
                .unwrap_or(cluster_nodes[0]);
            let fallbacks = cluster_nodes.iter().filter(|&&a| a != p).copied().collect();
            return (p, fallbacks);
        }

        // Sort deterministically by address, then rotate by a hash of chunk_id —
        // gives a stable per-chunk starting point that varies across chunks.
        addrs.sort_unstable();
        let idx = (loc.chunk_id.hash[0] as usize) % addrs.len();
        addrs.rotate_left(idx);

        // Stable sort by in-flight load: ties preserve the hash-rotated order
        // (the chunk-hash pick wins when all replicas are equally loaded), but
        // a genuinely busier replica is still passed over.
        let mut scored: Vec<(SocketAddr, usize)> = addrs.iter().map(|&addr| {
            let n = self.node_inflight.get(&addr).map(|e| e.load(Ordering::Relaxed)).unwrap_or(0);
            (addr, n)
        }).collect();
        scored.sort_by_key(|&(_, n)| n);

        let primary = scored[0].0;
        let fallbacks = scored[1..].iter().map(|&(a, _)| a).collect();
        (primary, fallbacks)
    }

    fn node_inflight_inc(&self, addr: SocketAddr) {
        self.node_inflight
            .entry(addr)
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .fetch_add(1, Ordering::Relaxed);
    }

    fn node_inflight_dec(&self, addr: SocketAddr) {
        if let Some(e) = self.node_inflight.get(&addr) {
            // Saturating prevents usize::MAX underflow if dec is called without a matching inc.
            let prev = e.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(v.saturating_sub(1)));
            debug_assert!(prev.unwrap_or(0) > 0, "node_inflight underflow for {addr}");
        }
    }

    /// Get (or lazily create) the semaphore bounding concurrent range-fetch requests
    /// for this (inode, node) pair. The cap is fixed at first creation per pair from
    /// range_fetch_max_per_file_node() (DFS_RANGE_FETCH_MAX_PER_FILE_NODE override).
    fn range_fetch_permit_for(&self, inode: u64, addr: SocketAddr) -> Arc<tokio::sync::Semaphore> {
        self.range_fetch_node_limit
            .entry((inode, addr))
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(range_fetch_max_per_file_node())))
            .clone()
    }

    /// Spawn the background sweeper that evicts idle `range_fetch_node_limit` entries.
    /// Must be called once after construction. Without this, every distinct (inode, node)
    /// pair that has ever done a range fetch keeps a permanent semaphore entry — an
    /// unbounded leak that grows with total files-ever-opened, not concurrent files
    /// (e.g. a DVR cycling through thousands of recordings over weeks, or QEMU
    /// repeatedly reopening images). An entry is idle when nothing holds a permit
    /// (`available_permits() == max`) and no other Arc clone is outstanding
    /// (`strong_count == 1`, i.e. only this map's own reference remains) — an
    /// in-flight `acquire_owned()` guard holds its own Arc clone, so it can't be
    /// evicted out from under an active fetch.
    pub fn start_range_fetch_limit_sweeper(&self, runtime: &tokio::runtime::Handle) {
        let client = self.clone();
        let max_permits = range_fetch_max_per_file_node();
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                client.range_fetch_node_limit.retain(|_, sem| {
                    !(Arc::strong_count(sem) == 1 && sem.available_permits() == max_permits)
                });
            }
        });
    }

    /// Spawn the background sweeper that evicts stale `read_write_seq_cache` entries.
    /// Must be called once after construction. Chunk ids are content-addressed (a rewrite
    /// mints a new id), so entries are never overwritten — without this sweeper the map
    /// only grows for the life of the mount. A 5-minute TTL is generous: this cache only
    /// serves an optional staleness-detection hint on reads, so an evicted-too-early entry
    /// just means that one read omits client_write_seq, not a correctness issue.
    pub fn start_read_write_seq_cache_sweeper(&self, runtime: &tokio::runtime::Handle) {
        let client = self.clone();
        const TTL: Duration = Duration::from_secs(300);
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                client.read_write_seq_cache.retain(|_, (_, inserted_at)| inserted_at.elapsed() < TTL);
            }
        });
    }

    /// Spawn the background sweeper that prunes stale pre-hot-classification
    /// entries from active_fold_started_at/active_fold_patch_count — see
    /// hot_chunk_slots's doc comment for the classification scheme. A slot
    /// that accumulates 1-3 patches within HOT_INACTIVITY_RESET and then goes
    /// permanently quiet (the common case — most chunks under a wide
    /// random-write workload are touched a couple of times and never again)
    /// would otherwise leave its entry in those two maps forever, since
    /// nothing else removes it except a *future* patch to that exact slot.
    /// Same TTL-retain pattern as start_read_write_seq_cache_sweeper, tighter
    /// interval since the TTL itself is only 2s here (vs. 5 minutes there).
    /// Never touches hot_chunk_slots or a slot that's actually hot — a hot
    /// slot's active_fold_started_at is its real fold-trigger generation
    /// timer, not classification state, and must only reset on an actual
    /// fold (existing behavior, unchanged).
    pub fn start_hot_chunk_sweeper(&self, runtime: &tokio::runtime::Handle) {
        let client = self.clone();
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                client.active_fold_started_at.retain(|key, started_at| {
                    client.hot_chunk_slots.contains(key) || started_at.elapsed() < HOT_INACTIVITY_RESET
                });
                // active_fold_patch_count is keyed identically and reset in lockstep
                // with active_fold_started_at everywhere else — prune anything that
                // no longer has a started_at entry (either just evicted above, or a
                // hot slot that hasn't been touched by this sweep's first branch
                // since it's protected by the hot_chunk_slots check there too).
                client.active_fold_patch_count.retain(|key, _| client.active_fold_started_at.contains_key(key));
            }
        });
    }

    /// Fetch with primary then fallbacks sequentially.
    /// Connect timeout is 1s so a dead node fails fast without wasting bandwidth.
    async fn fetch_chunk_with_fallback(
        &self,
        cid: ChunkId,
        primary: SocketAddr,
        fallbacks: &[SocketAddr],
        client_write_seq: Option<u64>,
    ) -> Result<Vec<u8>> {
        let mut all_not_found = true;
        for &addr in std::iter::once(&primary).chain(fallbacks.iter()) {
            match self.read_chunk_from_server(addr, cid, client_write_seq).await {
                Ok(d) => {
                    self.node_health.record_success(addr).await;
                    return Ok(d);
                }
                Err(e) => {
                    let msg = e.to_string();
                    warn!("{} failed for chunk {}: {}", addr, cid, msg);
                    // "Chunk not found on this node" means the node is up but doesn't hold
                    // this specific chunk — a metadata/routing issue, not a health issue.
                    // Only count actual connectivity failures against node health so we don't
                    // penalise a healthy node and skew future routing decisions.
                    if !msg.contains("not found on this node") {
                        all_not_found = false;
                        self.node_health.record_failure(addr).await;
                    }
                    if msg.contains("permanently missing") || msg.contains("location not found") {
                        anyhow::bail!("Chunk {} is permanently missing", cid);
                    }
                    // "blocklisted" or "temporarily unavailable" — try next replica.
                }
            }
        }
        // Distinguish "chunk missing everywhere" (stale metadata) from connectivity failures
        // so callers can refresh metadata and retry instead of surfacing EIO immediately.
        if all_not_found {
            anyhow::bail!("Chunk {} missing on all replicas — metadata may be stale", cid)
        } else {
            anyhow::bail!("All replicas failed for chunk {}", cid)
        }
    }

    /// Poll chunk_cache for up to 1s waiting for a concurrent fetch to complete.
    /// Exits early if the in-flight entry disappears (fetch failed on the other side).
    async fn wait_for_chunk_in_cache(
        &self,
        cid: ChunkId,
        engine: &InodeReadEngine,
        loc: &ChunkLocation,
    ) -> Result<Arc<Vec<u8>>> {
        // Arm the notified() future BEFORE checking the cache so we never miss
        // a notify_waiters() that fires between the check and the wait.
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(1);
        loop {
            let notified = self.chunk_landed.notified();
            tokio::pin!(notified);
            // Enable so any subsequent notify_waiters() will wake us.
            notified.as_mut().enable();

            if let Some(data) = self.chunk_cache.get(&cid) {
                return Ok(data);
            }
            if !engine.in_flight.contains(&cid) {
                // Other fetcher dropped in_flight without caching → it failed.
                break;
            }

            // Wait for either a chunk-landed notification or the overall deadline.
            // notified() is a single-shot future; we re-arm at the top of each loop.
            let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
            if timeout.is_zero() {
                break;
            }
            if tokio::time::timeout(timeout, notified).await.is_err() {
                // Hit the 1s deadline without any chunk landing — give up and
                // fall through to fetching directly.
                break;
            }
        }

        // Fall back — fetch ourselves using only the chunk's actual holders.
        warn!("Timeout waiting for concurrent fetch of chunk {}, fetching directly", cid);
        let nim = {
            let m = self.addr_to_node_id.read().await;
            m.iter().map(|(&a, &id)| (id, a)).collect::<std::collections::HashMap<_, _>>()
        };
        let nodes = self.cluster_nodes.read().await.clone();
        let (primary, fallbacks) =
            InodeReadEngine::resolve_primary(loc, &nim, &nodes, 0)
                .unwrap_or_else(|| {
                    let p = nodes[0];
                    (p, nodes[1..].to_vec())
                });
        let data = Arc::new(self.fetch_chunk_with_fallback(cid, primary, &fallbacks, None).await?);
        // Cache the result so other concurrent waiters don't need their own direct fetch.
        // Without this, every waiter that timed out does a separate network fetch for the
        // same chunk — a thundering herd when the primary fetch is slow (e.g. server throttling).
        self.chunk_cache.insert(cid, Arc::clone(&data));
        self.chunk_landed.notify_waiters();
        Ok(data)
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
        // Arm the notification before attempting the exchange — enable() must
        // happen first so a concurrent refresh that finishes between our failed
        // exchange below and our await can't be missed (same enable-before-check
        // pattern the metadata queue worker uses for the same reason).
        let notified = engine.refresh_done.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        if engine.refresh_in_progress.compare_exchange(
            false, true, Ordering::AcqRel, Ordering::Relaxed,
        ).is_err() {
            // Someone else is already refreshing this engine — wait for them to
            // finish instead of returning immediately. A caller that returned
            // here with the engine's snapshot still empty used to fall through
            // read_file's "chunk map still empty after refresh" sparse-hole
            // check and serve zeros for a real, non-sparse file (2026-07-09,
            // found via T28: concurrent cold-cache reads on a freshly restarted
            // client all race this exchange on the first read of a file).
            if tokio::time::timeout(std::time::Duration::from_secs(10), notified).await.is_err() {
                warn!("refresh_engine: inode={} timed out waiting for a concurrent refresh to finish", engine.inode);
            }
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

        // Always fetch from chunk 0 with u32::MAX window to get the complete map
        // in one RPC. Fetching from from_chunk causes constant re-fetches for sparse
        // files (e.g. VM disk images) where reads jump to high chunk indices that fall
        // outside the previously fetched window. One full fetch covers all positions.
        const CHUNK_MAP_WINDOW: u32 = u32::MAX;
        let rpc_start = std::time::Instant::now();
        match self.get_file_chunk_map(file_id, 0, CHUNK_MAP_WINDOW).await {
            // A real answer from the server — including a legitimate "zero chunks"
            // (total_chunks == 0) response — is a CONFIRMED result, not a failure.
            // Previously this arm required `!locs.is_empty()`, so a genuinely empty
            // file's confirmed answer fell into the same bucket as a network error
            // (see confirmed_at_least_once's doc comment) — both left the engine
            // indistinguishable from "we don't actually know yet".
            Ok((locs, window_from, total_chunks, _)) => {
                info!("refresh_engine: inode={} got {} chunks (from={} total={}) from leader in {:?}",
                      engine.inode, locs.len(), window_from, total_chunks, rpc_start.elapsed());
                engine.clear_failed_refresh();
                engine.update_chunk_map_window(locs, window_from, total_chunks, Arc::new(nim), file_size, false);
                engine.confirmed_at_least_once.store(true, Ordering::Relaxed);
            }
            Err(_) => {
                info!("refresh_engine: inode={} no chunk map from leader (took {:?})",
                      engine.inode, rpc_start.elapsed());
                engine.record_failed_refresh();
            }
        }

        engine.refresh_in_progress.store(false, Ordering::Release);
        engine.refresh_done.notify_waiters();
    }

    /// Notify the read engine that a new chunk was appended by the write path.
    /// Called from fuse_impl after a successful WriteData completes.  Does not block
    /// writers — just bumps the engine's known_size so the next read triggers a refresh.
    pub fn invalidate_read_engine(&self, inode: u64) {
        if let Some(engine) = self.read_engines.get(inode) {
            // Set known_size to 0 so needs_refresh() returns true on the next read.
            engine.known_size.store(0, Ordering::Relaxed);
        }
    }

    /// Feed freshly-written chunk locations directly into the read engine for `inode`,
    /// bypassing the leader.  Called from the write flush path so concurrent readers on
    /// the same client see new chunks immediately without a leader round-trip.
    pub async fn feed_chunk_locations_to_read_engine(
        &self,
        inode: u64,
        locations: &[dfs_common::ChunkLocation],
        file_size: u64,
    ) {
        if locations.is_empty() {
            return;
        }
        let engine = self.read_engines.get_or_create(inode);

        // Derive from_chunk from the file_offset of the first location so that a single
        // chunk N stub is placed at slot N, not slot 0.  Callers from the flush path pass
        // one location at a time (file_offset = chunk_idx * CHUNK_SIZE); using from_chunk=0
        // was overwriting lower-indexed committed chunks with the new stub.
        const CHUNK_SIZE_U64: u64 = 4 * 1024 * 1024;
        let from_chunk = locations.first()
            .and_then(|l| l.file_offset)
            .map(|o| (o / CHUNK_SIZE_U64) as u32)
            .unwrap_or(0);
        // chunk_locations is a sparse list sorted by file_offset, NOT indexed by chunk_idx —
        // for large sparse files (e.g. VM disk images) locations.len() can be far smaller
        // than the highest chunk_idx present. update_chunk_map_window places each entry at
        // new_map[chunk_idx] and only does so if chunk_idx < total_chunks, so total_chunks
        // must cover the highest chunk_idx referenced here (matching the server's
        // handle_get_file_chunk_map: total_chunks = max_chunk_idx + 1). Otherwise the
        // highest-indexed chunk in this batch is silently dropped from the engine's map:
        // chunks_for_range then finds no entry covering that file range and treats it as a
        // sparse hole, returning zeros for a chunk that was actually written and replicated.
        let max_chunk_idx = locations.iter()
            .filter_map(|l| l.file_offset.map(|o| (o / CHUNK_SIZE_U64) as u32))
            .max()
            .unwrap_or(from_chunk);
        let total_chunks = (from_chunk + locations.len() as u32).max(max_chunk_idx + 1);

        // Snapshot old chunk IDs for the slots we're about to update so we can evict
        // them from the chunk cache. Without this, a reader near the write edge can get
        // a cache hit on the old (shorter) chunk ID and return stale partial data.
        let old_chunk_ids: Vec<dfs_common::ChunkId> = {
            let (old_map, _, _) = engine.snapshot();
            let base = from_chunk as usize;
            (0..locations.len())
                .filter_map(|i| old_map.get(base + i).map(|l| l.chunk_id))
                .collect()
        };

        let nim: std::collections::HashMap<dfs_common::NodeId, std::net::SocketAddr> = {
            let addr_map = self.addr_to_node_id.read().await;
            addr_map.iter().map(|(&addr, &id)| (id, addr)).collect()
        };
        let node_map = Arc::new(nim);

        if locations.len() == 1 {
            // Single-chunk write-path: update in-place with Arc::make_mut — O(1) vs O(n) clone.
            engine.update_single_chunk(locations[0].clone(), file_size, node_map);
        } else {
            engine.update_chunk_map_window(
                locations.to_vec(),
                from_chunk,
                total_chunks,
                node_map,
                file_size,
                true,
            );
        }
        engine.clear_failed_refresh();

        // Evict stale chunk IDs from the cache. Any reader that already holds an Arc to
        // the old data is unaffected; only future cache lookups miss and fetch fresh data.
        {
            let new_ids: std::collections::HashSet<dfs_common::ChunkId> =
                locations.iter().map(|l| l.chunk_id).collect();
            for old_id in old_chunk_ids {
                if !new_ids.contains(&old_id) {
                    { let _ = self.chunk_cache.remove(&old_id); };
                }
            }
        }

        // Also evict byte_range_cache entries for the written chunk slots. The
        // byte_range_cache is keyed by (inode, file_offset), not chunk_id, so we
        // invalidate by range. Without this, a range-fetch read immediately after a
        // same-chunk overwrite can return stale cached bytes.
        for i in 0..locations.len() {
            let chunk_file_offset = (from_chunk as u64 + i as u64) * CHUNK_SIZE_U64;
            self.invalidate_byte_range_cache_for_chunk(inode, chunk_file_offset, CHUNK_SIZE_U64 as usize).await;
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
        let hints_map: std::collections::HashMap<ChunkId, &ChunkReadHint> =
            read_hints.iter().map(|h| (h.chunk_id, h)).collect();

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
            if let Some(data) = self.chunk_cache.get(chunk_id) {
                cached_chunks.push((idx, data));
                found = true;
            }

            // Only check byte-range cache (live DVR segments) if chunk cache missed.
            if !found && inode > 0 && idx < chunk_offsets.len() {
                let requested_offset = chunk_offsets[idx];
                let byte_hit = {
                    let mut byte_cache = self.byte_range_cache.shard(inode).lock().await;
                    let key = ByteRangeCacheKey {
                        inode,
                        file_offset: requested_offset,
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
                    let in_flight = self.prefetch_in_flight.lock().await;
                    let mut file_idx = last_required_file_idx + 1;
                    while pipeline_chunks.len() < lookahead_needed && file_idx < all_file_chunks.len() {
                        let cid = all_file_chunks[file_idx];
                        if self.chunk_cache.get(&cid).is_none() && !in_flight.contains(&cid) {
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

        let node_id_to_addr: HashMap<dfs_common::NodeId, SocketAddr> = {
            let m = self.addr_to_node_id.read().await;
            m.iter().map(|(&addr, &id)| (id, addr)).collect()
        };
        let mut resolved: Vec<ResolvedFetch> = Vec::with_capacity(chunks_to_fetch.len());

        for (idx, chunk_id, file_offset, pipeline_only) in &chunks_to_fetch {
            let idx = *idx;
            let chunk_id = *chunk_id;
            let file_offset = *file_offset;
            let pipeline_only = *pipeline_only;

            // Resolve replica list from chunk_locations (fast, no network).
            let mut replicas = if let Some(loc) = chunk_loc_map.get(&chunk_id) {
                let addrs: Vec<SocketAddr> = loc.nodes.iter()
                    .filter_map(|nid| node_id_to_addr.get(nid).copied())
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
            self.node_inflight_inc(primary);

            // Determine partial-read flag.
            let use_partial_read = if pipeline_only {
                false
            } else {
                hints_map.get(&chunk_id).copied()
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
                                match client.read_chunk_from_server(fb_addr, r.chunk_id, None).await {
                                    Ok(d) => { fallback_data = Some(d); break; }
                                    Err(e) => { last_err = e; }
                                }
                            }
                            match fallback_data {
                                Some(d) => d,
                                None => {
                                    client.node_inflight_dec(r.primary);
                                    return Err(last_err.context(format!("pipeline read chunk {} (all replicas failed)", r.chunk_id)));
                                }
                            }
                        }
                    };
                    client.node_inflight_dec(r.primary);
                    info!("✓ Chunk {} via pipeline ({} bytes)", r.chunk_id, data.len());
                    Ok((r.idx, r.chunk_id, r.file_offset, Arc::new(data), false, r.pipeline_only))
                }
            })).await
        } else {
            // Original parallel path.
            let tasks: Vec<_> = resolved.into_iter().map(|r| {
                let client = self.clone();
                let semaphore = max_concurrent_fetches.clone();
                let read_hint = hints_map.get(&r.chunk_id).copied().cloned();

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
                                hint.offset_in_chunk as u64, hint.length as u64, None,
                                // read_data (sequential/full-chunk path) does not thread
                                // file_id; this partial-read branch is not the one that errors
                                // under mixed load. TODO: carry file_id via
                                // ResolvedFetch to give this the slot backstop too.
                                None).await
                        } else {
                            client.read_chunk_from_server(node_addr, r.chunk_id, None).await
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
                    client.node_inflight_dec(r.primary);
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
                self.chunk_cache.insert(chunk_id, Arc::clone(&data_arc));
                self.chunk_landed.notify_waiters();
                debug!("Cached chunk {} ({} bytes)", chunk_id, data_arc.len());
            }

            // Add to byte-range cache if we have inode (skip pipeline-only — no valid file_offset).
            // Note: file_offset == 0 is valid (first chunk of file) and should be cached.
            if inode > 0 && !is_pipeline_only {
                let mut byte_cache = self.byte_range_cache.shard(inode).lock().await;
                let key = ByteRangeCacheKey {
                    inode,
                    file_offset,
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
                    if let Some(data) = self.chunk_cache.get(&chunk_id) {
                        debug!("Waited chunk {} now available after {:?}", chunk_id, wait_start.elapsed());
                        fetched_chunks.push((idx, data));
                        data_found = true;
                        break;
                    }

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
                    self.node_inflight_inc(selected_replica);

                    // Try selected replica first, then fall back to others
                    let mut fetch_succeeded = false;
                    for (i, node_addr) in std::iter::once(&selected_replica)
                        .chain(replicas.iter().filter(|&n| n != &selected_replica))
                        .enumerate()
                    {
                        match self.read_chunk_from_server(*node_addr, chunk_id, None).await {
                            Ok(data) => {
                                if i > 0 {
                                    debug!("Fetched chunk {} from fallback replica {} after timeout", chunk_id, node_addr);
                                }
                                let data_arc = Arc::new(data);

                                // Cache it
                                self.chunk_cache.insert(chunk_id, Arc::clone(&data_arc));
                                self.chunk_landed.notify_waiters();

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
                    self.node_inflight_dec(selected_replica);

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
    /// Returns (locations, from_chunk, total_chunks, write_seq). Falls back to any node if leader is unknown.
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

        // Followers only have the chunks written to them — they cannot answer
        // GetFileChunkMap authoritatively. Never fall back to a non-leader node:
        // a partial follower response would give the client stale chunk_ids for
        // chunks it doesn't hold, causing "chunk not found" on all replicas.
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                // Leader unreachable: refresh our leader address and retry once on
                // the newly discovered leader before giving up entirely.
                warn!("GetFileChunkMap to leader {} failed ({}), refreshing leader and retrying", target, e);
                let _ = self.refresh_cluster_nodes().await;
                let new_target = {
                    let leader = self.leader_addr.read().await;
                    *leader
                };
                match new_target {
                    Some(addr) if addr != target => {
                        self.send_request(addr, Request::GetFileChunkMap { file_id, from_chunk, count }).await
                            .map_err(|e2| anyhow::anyhow!("GetFileChunkMap: leader {} also failed: {}", addr, e2))?
                    }
                    _ => return Err(anyhow::anyhow!("GetFileChunkMap: leader unreachable: {}", e)),
                }
            }
        };

        match response {
            Response::FileChunkMap { locations, from_chunk, total_chunks, write_seq, .. } => {
                Ok((locations, from_chunk, total_chunks, write_seq))
            }
            Response::Error { message, .. } => anyhow::bail!("GetFileChunkMap error: {}", message),
            _ => anyhow::bail!("Unexpected response to GetFileChunkMap"),
        }
    }

    /// Remove all recent_chunk_writes entries for an inode.
    /// Call this wherever write_buffers is removed (release, unlink, rename, truncate).
    pub fn evict_recent_chunk_writes(&self, ino: u64) {
        self.recent_chunk_writes.retain(|k, _| k.0 != ino);
    }

    /// Fetch a single ChunkLocation from the leader by (file_id, chunk_idx).
    /// Uses GetFileChunkMap with count=1 — one in-memory map lookup on the server,
    /// no full metadata scan. Call this on patch failure instead of get_file_metadata.
    pub async fn get_single_chunk_location(&self, file_id: FileId, chunk_idx: u64) -> Result<Option<dfs_common::ChunkLocation>> {
        let (locations, _, _, _) = self.get_file_chunk_map(file_id, chunk_idx as u32, 1).await?;
        Ok(locations.into_iter().next())
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
    /// Seed the byte-range cache with freshly-written bytes for each dirty range.
    /// Called after a successful PatchChunk or fresh WriteChunk so that subsequent
    /// reads at those offsets hit the cache instead of going to the network.
    /// dirty_ranges: (intra_chunk_start, intra_chunk_end) pairs from the slot.
    pub async fn seed_byte_range_cache(
        &self,
        inode: u64,
        chunk_file_offset: u64,
        slot_data: &[u8],
        dirty_ranges: &[(usize, usize)],
    ) {
        if inode == 0 || dirty_ranges.is_empty() {
            return;
        }
        let mut byte_cache = self.byte_range_cache.shard(inode).lock().await;
        for &(range_start, range_end) in dirty_ranges {
            if range_end > range_start && range_end <= slot_data.len() {
                let key = ByteRangeCacheKey {
                    inode,
                    file_offset: chunk_file_offset + range_start as u64,
                };
                let cached = CachedChunk {
                    data: Arc::new(slot_data[range_start..range_end].to_vec()),
                    chunk_size: range_end - range_start,
                    cached_at: std::time::Instant::now(),
                };
                byte_cache.put(key, cached);
            }
        }
    }

    /// Seed the zero gap table with metadata about zero-filled regions between dirty ranges.
    /// This allows us to serve zeros for sparse file gaps without caching megabytes of zeros.
    /// Called after a successful flush with sparse writes (e.g., qcow2 header + L1 table).
    pub async fn seed_zero_gap_table(
        &self,
        inode: u64,
        chunk_file_offset: u64,
        slot_len: usize,
        dirty_ranges: &[(usize, usize)],
    ) {
        if inode == 0 || dirty_ranges.is_empty() || slot_len == 0 {
            return;
        }

        // Identify gaps between dirty ranges
        let mut gaps = Vec::new();
        let mut sorted_ranges = dirty_ranges.to_vec();
        sorted_ranges.sort_by_key(|r| r.0);

        // Check for gap before first dirty range
        if sorted_ranges[0].0 > 0 {
            gaps.push((0, sorted_ranges[0].0));
        }

        // Check for gaps between consecutive dirty ranges
        for i in 0..sorted_ranges.len() - 1 {
            let end_of_current = sorted_ranges[i].1;
            let start_of_next = sorted_ranges[i + 1].0;
            if start_of_next > end_of_current {
                gaps.push((end_of_current, start_of_next));
            }
        }

        // Check for gap after last dirty range
        let last_end = sorted_ranges.last().unwrap().1;
        if last_end < slot_len {
            gaps.push((last_end, slot_len));
        }

        // Add gaps to the gap table
        if !gaps.is_empty() {
            let key = ZeroGapKey {
                inode,
                chunk_offset: chunk_file_offset,
            };
            let mut gap_table = self.zero_gap_table.shard(inode).lock().await;
            let gap_entries: Vec<ZeroGap> = gaps
                .into_iter()
                .map(|(start, end)| ZeroGap {
                    start: chunk_file_offset + start as u64,
                    end: chunk_file_offset + end as u64,
                    created_at: std::time::Instant::now(),
                })
                .collect();

            debug!(
                "seed_zero_gap_table: ino={} chunk_offset={} added {} gaps",
                inode, chunk_file_offset, gap_entries.len()
            );
            gap_table.insert(key, gap_entries);
        }
    }

    /// Invalidate all byte-range cache entries for a chunk.
    /// Called before seeding patched data to prevent stale cache hits.
    /// Example: qcow2 writes full header at offset 0, then patches offset 36.
    /// Without invalidation, reads at offset 0 hit the old cached header.
    pub async fn invalidate_byte_range_cache_for_chunk(
        &self,
        inode: u64,
        chunk_file_offset: u64,
        chunk_len: usize,
    ) {
        if inode == 0 || chunk_len == 0 {
            return;
        }
        let mut byte_cache = self.byte_range_cache.shard(inode).lock().await;
        // Invalidate all keys in the range [chunk_file_offset, chunk_file_offset + chunk_len).
        // LruCache doesn't have range removal, so we scan all entries. This is acceptable
        // because byte_range_cache is small (~100 entries) and invalidation is rare (patches).
        let keys_to_remove: Vec<ByteRangeCacheKey> = byte_cache.iter()
            .filter_map(|(k, _)| {
                if k.inode == inode
                    && k.file_offset >= chunk_file_offset
                    && k.file_offset < chunk_file_offset + chunk_len as u64
                {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect();
        for key in keys_to_remove {
            byte_cache.pop(&key);
        }

        // Also invalidate zero gaps for this chunk.
        // When we invalidate the byte cache, we should also invalidate gap metadata
        // since the chunk content may have changed.
        let gap_key = ZeroGapKey {
            inode,
            chunk_offset: chunk_file_offset,
        };
        let mut gap_table = self.zero_gap_table.shard(inode).lock().await;
        gap_table.remove(&gap_key);
    }

    /// Invalidate only the zero_gap_table entry for a specific chunk.
    /// Called on every write so that gap entries never shadow real in-flight data.
    pub async fn invalidate_zero_gap_for_chunk(&self, inode: u64, chunk_file_offset: u64) {
        let gap_key = ZeroGapKey { inode, chunk_offset: chunk_file_offset };
        let mut gap_table = self.zero_gap_table.shard(inode).lock().await;
        gap_table.remove(&gap_key);
    }

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
    async fn read_chunk_from_server(&self, server_addr: SocketAddr, chunk_id: ChunkId, client_write_seq: Option<u64>) -> Result<Vec<u8>> {
        // Look up write_seq from cache if not explicitly provided
        let ws = client_write_seq.or_else(|| self.read_write_seq_cache.get(&chunk_id).map(|e| e.0));

        let request = Request::ReadChunk {
            chunk_id,
            sequential_hint: None, // TODO: Pass sequential hint when available
            client_write_seq: ws,
            // Full-chunk reads don't currently carry the slot backstop; the mixed
            // read/write collapse manifests on the range path. TODO: thread file_id
            // here too if the full-chunk path shows the same stale-id EIOs.
            file_id: None,
            chunk_idx: None,
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

            // Send request and read full response (envelope + split-frame payload) under
            // one deadline. Previously the timeout only covered the envelope header, leaving
            // the split-frame 4MB payload read unbounded — causing indefinite hangs when the
            // server was slow under concurrent write load (T19 regression).
            let io_future = async {
                // Send request — coalesced into one write to cut packet/RTT overhead.
                let len = encoded.len() as u32;
                let mut framed = Vec::with_capacity(4 + encoded.len());
                framed.extend_from_slice(&len.to_be_bytes());
                framed.extend_from_slice(&encoded);
                stream.write_all(&framed).await?;
                stream.flush().await?;

                // Read envelope
                let mut len_buf = [0u8; 4];
                stream.read_exact(&mut len_buf).await?;
                let len = u32::from_be_bytes(len_buf) as usize;
                let mut buf = vec![0u8; len];
                stream.read_exact(&mut buf).await?;

                // Deserialize and read split-frame payload inside the deadline.
                let response_envelope = MessageEnvelope::from_bytes(&buf)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                let (data, cache_stats) = match response_envelope.message {
                    Message::Response(Response::ChunkData { data, cache_stats, .. }) => {
                        let data = if data.is_empty() {
                            dfs_common::protocol::read_chunk_payload(&mut stream).await?
                        } else {
                            data
                        };
                        (data, cache_stats)
                    }
                    Message::Response(Response::Error { message, .. }) => {
                        return Err(std::io::Error::new(std::io::ErrorKind::Other, message));
                    }
                    _ => {
                        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "unexpected response"));
                    }
                };

                Ok::<(TcpStream, Vec<u8>, _), std::io::Error>((stream, data, cache_stats))
            };

            let result = tokio::time::timeout(
                tokio::time::Duration::from_secs(10),
                io_future
            ).await;

            let result = match result {
                Ok(r) => r,
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("Timeout reading chunk from {}", server_addr)
                )),
            };

            match result {
                Ok((mut stream, data, cache_stats)) => {
                    // Return connection to pool now that we've drained all bytes.
                    {
                        let mutex = {
                            let entry = self.connection_pool
                                .entry(server_addr)
                                .or_insert_with(|| Arc::new(Mutex::new(std::collections::VecDeque::new())));
                            Arc::clone(&*entry)
                        };
                        let mut queue = mutex.lock().await;
                        if queue.len() < POOL_SIZE {
                            queue.push_back(stream);
                        } else {
                            tokio::spawn(async move { let _ = stream.shutdown().await; });
                        }
                    }

                    // Flow control: throttle if server cache is under pressure.
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
                }
                Err(e) => {
                    // Connection failed - don't return to pool
                    warn!("Connection to {} failed (attempt {}): {}", server_addr, attempt, e);

                    // Retry once with new connection if this was a pooled connection
                    if attempt == 1 {
                        debug!("Retrying with new connection to {}", server_addr);
                        continue;
                    } else {
                        // Preserve the original error message so callers can inspect it
                        // (e.g. "not found on this node" drives stale-metadata retry in
                        // fetch_chunk_with_fallback — wrapping loses that signal).
                        return Err(anyhow::Error::from(e));
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
        client_write_seq: Option<u64>,
    ) -> Result<(TcpStream, usize)> {
        // Look up write_seq from cache if not explicitly provided
        let ws = client_write_seq.or_else(|| self.read_write_seq_cache.get(&chunk_id).map(|e| e.0));

        let request = Request::ReadChunk { chunk_id, sequential_hint: None, client_write_seq: ws, file_id: None, chunk_idx: None };
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

        // Send request (coalesced into one write) + read 4-byte length prefix (tiny, fast).
        let len = encoded.len() as u32;
        let mut framed = Vec::with_capacity(4 + encoded.len());
        framed.extend_from_slice(&len.to_be_bytes());
        framed.extend_from_slice(&encoded);
        let write_result = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            async {
                stream.write_all(&framed).await?;
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
            if queue.len() < POOL_SIZE {
                queue.push_back(stream);
            } else {
                tokio::spawn(async move { let _ = stream.shutdown().await; });
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
                client.open_chunk_request(addr, cid, None).await
            })))
        };

        for i in 0..chunks.len() {
            let (cid, addr) = chunks[i];

            let (p1_addr, p1_handle) = match pending.take() {
                Some(p) => p,
                None => {
                    results.push(self.read_chunk_from_server(addr, cid, None).await);
                    continue;
                }
            };

            // Concurrently start Phase 1 for the next chunk while we await drain of this one.
            let next_pending: Option<(SocketAddr, P1Handle)> = if i + 1 < chunks.len() {
                let (next_cid, next_addr) = chunks[i + 1];
                let client = self.clone();
                Some((next_addr, tokio::spawn(async move {
                    client.open_chunk_request(next_addr, next_cid, None).await
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
                    self.read_chunk_from_server(addr, cid, None).await
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
        client_write_seq: Option<u64>,
        // The logical (file_id, chunk_idx) this read is for, when the caller knows
        // it. Lets the server fall back to the current occupant of that slot if
        // chunk_id has been retired — see ReadChunkRange's doc in protocol.rs.
        slot: Option<(FileId, u64)>,
    ) -> Result<Vec<u8>> {
        // Look up write_seq from cache if not explicitly provided
        let ws = client_write_seq.or_else(|| self.read_write_seq_cache.get(&chunk_id).map(|e| e.0));

        let (file_id, chunk_idx) = match slot {
            Some((f, c)) => (Some(f), Some(c)),
            None => (None, None),
        };
        let request = Request::ReadChunkRange { chunk_id, offset, length, client_write_seq: ws, file_id, chunk_idx };
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
        let _ = file_offset;
        let chunk_size = location.size;

        // Map ALL replica NodeIds to SocketAddrs (not just first 2) so we have
        // real fallback candidates if either striped half-fetch fails.
        let all_replica_addrs: Vec<SocketAddr> = {
            let addr_map = self.addr_to_node_id.read().await;
            let node_id_to_addr: HashMap<dfs_common::NodeId, SocketAddr> =
                addr_map.iter().map(|(&addr, &id)| (id, addr)).collect();
            location.nodes.iter()
                .filter_map(|node_id| node_id_to_addr.get(node_id).copied())
                .collect()
        };

        // Helper: full-chunk read trying every available replica in order, then
        // any other cluster node as a last resort (covers ghost-record / drift).
        let whole_chunk_fallback = |replicas: Vec<SocketAddr>| {
            let client = self.clone();
            async move {
                let cluster_nodes = client.cluster_nodes.read().await.clone();
                let mut tried = std::collections::HashSet::<SocketAddr>::new();
                let mut last_err: Option<anyhow::Error> = None;
                for addr in replicas.iter().copied().chain(cluster_nodes.iter().copied()) {
                    if !tried.insert(addr) { continue; }
                    match client.read_chunk_from_server(addr, chunk_id, None).await {
                        Ok(data) => return Ok(data),
                        Err(e) => {
                            debug!("Whole-chunk fallback: {} failed for chunk {}: {}", addr, chunk_id, e);
                            last_err = Some(e);
                        }
                    }
                }
                Err(last_err.unwrap_or_else(|| anyhow::anyhow!(
                    "No replicas available for chunk {}", chunk_id)))
            }
        };

        if all_replica_addrs.is_empty() {
            // None of the chunk_locations nodes resolve in the current cluster —
            // fall back to whole-chunk reads against every cluster node.
            warn!("Striped read: chunk {} has no resolvable replicas, falling back to cluster-wide whole-chunk fetch", chunk_id);
            return whole_chunk_fallback(Vec::new()).await;
        }

        if all_replica_addrs.len() < 2 {
            // Only 1 address available, single-node read with cluster-wide fallback.
            return whole_chunk_fallback(all_replica_addrs).await;
        }

        let node1 = all_replica_addrs[0];
        let node2 = all_replica_addrs[1];

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
            client1.read_chunk_range_from_server(node1, chunk_id, 0, first_half_size as u64, None, None).await
        });

        let task2 = tokio::spawn(async move {
            client2.read_chunk_range_from_server(node2, chunk_id, mid_point as u64, second_half_size as u64, None, None).await
        });

        let (result1, result2) = tokio::join!(task1, task2);

        // Unwrap join errors first; treat panicked tasks the same as a failed half.
        let half1 = result1.unwrap_or_else(|e| Err(anyhow::anyhow!("striped task1 panicked: {}", e)));
        let half2 = result2.unwrap_or_else(|e| Err(anyhow::anyhow!("striped task2 panicked: {}", e)));

        match (half1, half2) {
            (Ok(first_half), Ok(second_half)) => {
                let mut combined = Vec::with_capacity(chunk_size);
                combined.extend_from_slice(&first_half);
                combined.extend_from_slice(&second_half);
                debug!("Striped read complete: chunk {} ({} + {} = {} bytes)",
                       chunk_id, first_half.len(), second_half.len(), combined.len());
                Ok(combined)
            }
            (half1_res, half2_res) => {
                // At least one half failed.  The failing node may be a ghost replica
                // (metadata says it has the chunk, but it doesn't), so don't EIO —
                // fall back to whole-chunk reads against every replica + cluster node.
                if let Err(e) = &half1_res {
                    warn!("Striped read: half1 from {} failed for chunk {}: {}", node1, chunk_id, e);
                }
                if let Err(e) = &half2_res {
                    warn!("Striped read: half2 from {} failed for chunk {}: {}", node2, chunk_id, e);
                }
                whole_chunk_fallback(all_replica_addrs).await
            }
        }
    }

    /// Read a single chunk by ID, resolving node IDs to addresses.
    /// Used to re-read the partial last chunk when re-aligning a write buffer after an interrupted append.
    pub async fn read_chunk_by_id(&self, chunk_id: ChunkId, node_ids: &[dfs_common::NodeId]) -> Result<Vec<u8>> {
        // Resolve NodeIds to SocketAddrs
        let mut node_addrs: Vec<SocketAddr> = {
            let addr_map = self.addr_to_node_id.read().await;
            let node_id_to_addr: HashMap<dfs_common::NodeId, SocketAddr> =
                addr_map.iter().map(|(&addr, &id)| (id, addr)).collect();
            node_ids.iter()
                .filter_map(|node_id| node_id_to_addr.get(node_id).copied())
                .collect()
        };

        if node_addrs.is_empty() {
            node_addrs = self.cluster_nodes.read().await.clone();
        }

        for addr in &node_addrs {
            match self.read_chunk_from_server(*addr, chunk_id, None).await {
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
                let req = Request::ReplicateChunkLocation { location: loc, file_id: None, generation: None };
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

    /// Resolve NodeIds to their current SocketAddrs using the addr→id map, dropping any
    /// NodeIds that don't resolve (stale/unknown).
    pub async fn resolve_node_addrs(&self, node_ids: &[dfs_common::NodeId]) -> Vec<SocketAddr> {
        let addr_map = self.addr_to_node_id.read().await;
        let id_to_addr: HashMap<dfs_common::NodeId, SocketAddr> = addr_map.iter().map(|(&addr, &id)| (id, addr)).collect();
        node_ids.iter().filter_map(|nid| id_to_addr.get(nid).copied()).collect()
    }

    /// Select two write-replica nodes from `nodes` using capacity-banded Fisher-Yates.
    /// Nodes are bucketed into 10 equal-width bands by available disk space (band 0 = most free).
    /// Within each band, nodes are shuffled deterministically by `seed`.  The first two nodes
    /// from the resulting priority list are returned as (preferred1, preferred2).
    ///
    /// Falls back to consecutive-pair rotation when capacity data is unavailable for all nodes.
    fn select_write_pair(&self, nodes: &[SocketAddr], seed: u64) -> (SocketAddr, SocketAddr) {
        if nodes.len() < 2 {
            return (nodes[0], nodes[0]);
        }

        // Build (addr, available, total) for each node from cached capacity data.
        let caps: Vec<(SocketAddr, u64, u64)> = nodes.iter().map(|&addr| {
            let (avail, total) = self.node_capacities.get(&addr)
                .map(|e| *e)
                .unwrap_or((0u64, 0u64));
            (addr, avail, total)
        }).collect();

        // If no capacity data yet, fall back to seeded rotation across all pairs.
        let has_capacity = caps.iter().any(|(_, _, total)| *total > 0);
        if !has_capacity {
            let i = (seed as usize) % nodes.len();
            let j = (i + 1) % nodes.len();
            return (nodes[i], nodes[j]);
        }

        // Hard veto: skip nodes with less than 20 GB free (absolute, not percentage).
        const MIN_FREE_BYTES: u64 = 20 * 1024 * 1024 * 1024;
        let eligible: Vec<(SocketAddr, u64, u64)> = caps.iter()
            .filter(|(_, avail, total)| *total == 0 || *avail >= MIN_FREE_BYTES)
            .cloned()
            .collect();
        let candidates = if eligible.len() >= 2 { eligible } else {
            let mut all = caps.clone();
            all.sort_by(|a, b| b.1.cmp(&a.1));
            all
        };

        // Weighted-random priority order: probability of ranking first is proportional
        // to available space (Efraimidis-Spirakis weighted sampling without replacement).
        // Mirrors the server's get_nodes_with_capacity_awareness in dfs-server/src/cluster.rs —
        // equal-width banding degrades badly when one node's free space vastly exceeds the
        // rest (the band width is dominated by the outlier, collapsing the others into the
        // same bottom band regardless of how differently full they actually are).
        let mut keyed: Vec<(f64, SocketAddr)> = candidates.iter().map(|(addr, avail, _)| {
            let weight_gb = (*avail as f64 / 1_000_000_000.0).max(0.01);
            let u = Self::seeded_unit_interval(seed, *addr);
            (u.powf(1.0 / weight_gb), *addr)
        }).collect();
        keyed.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        (keyed[0].1, keyed[1].1)
    }

    /// Deterministic pseudo-random value in (0, 1), seeded from a placement seed and a
    /// node address. See dfs-server/src/cluster.rs::seeded_unit_interval — same algorithm,
    /// kept independent since the client has no dependency on dfs-server internals.
    fn seeded_unit_interval(seed: u64, addr: SocketAddr) -> f64 {
        let addr_bits = match addr.ip() {
            std::net::IpAddr::V4(v4) => u32::from(v4) as u64 ^ ((addr.port() as u64) << 32),
            std::net::IpAddr::V6(v6) => {
                let octets = v6.octets();
                u64::from_le_bytes(octets[..8].try_into().unwrap()) ^ (addr.port() as u64)
            }
        };
        let mut x = seed ^ addr_bits.wrapping_mul(0x9e3779b97f4a7c15);
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51afd7ed558ccd);
        x ^= x >> 33;
        x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
        x ^= x >> 33;
        ((x >> 11) as f64 / (1u64 << 53) as f64).max(1e-12)
    }

    /// Write data with synchronous dual-replica replication
    /// NEW: Writes each chunk to 2 nodes synchronously (not striped)
    /// Returns chunk_locations with replica tracking
    pub async fn write_data_dual_replica(&self, data: &[u8], inode: u64, file_offset: u64, file_id: dfs_common::FileId, preferred_nodes: Option<&[SocketAddr]>) -> Result<Vec<dfs_common::ChunkLocation>> {
        let nodes = self.cluster_nodes.read().await.clone();
        if nodes.len() < 2 {
            anyhow::bail!("Need at least 2 nodes for writes (only {} available)", nodes.len());
        }

        // Rewrites of existing chunk data: target the nodes that already hold it rather
        // than re-deriving placement via capacity-band randomization. Banding in
        // select_write_pair is for *new* chunk placement — applying it to a full-chunk
        // rewrite would migrate the chunk to a different pair whenever relative capacity
        // shifts cross a band boundary, causing constant cross-node churn for chunks that
        // get rewritten repeatedly (e.g. VM disk blocks).
        let (preferred1, preferred2) = match preferred_nodes {
            Some(existing) if existing.len() >= 2 => (existing[0], existing[1]),
            _ => {
                // Capacity-banded placement: prefer nodes with more available disk space, using
                // the same banded Fisher-Yates algorithm as the server's get_nodes_with_capacity_awareness.
                // Seed from chunk offset so different chunks of the same file land on different pairs.
                let chunk_idx = (file_offset / (4 * 1024 * 1024)) as usize;
                let seed = chunk_idx as u64 ^ (inode.wrapping_mul(0x9e3779b97f4a7c15));
                self.select_write_pair(&nodes, seed)
            }
        };

        info!("Writing {} bytes with synchronous dual-replica (preferred: {}, {})",
              data.len(), preferred1, preferred2);

        let chunk_locations = self.write_chunk_to_replicas(data, preferred1, preferred2, inode, file_offset, &nodes, file_id).await?;

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

        // Populate caches for immediate read-back of freshly-written data.
        // chunk_cache (keyed by ChunkId) enables full-chunk reads to hit cache.
        // byte_range_cache (keyed by inode+offset) enables sub-chunk range reads to hit cache.
        {
            let mut byte_cache = if inode > 0 { Some(self.byte_range_cache.shard(inode).lock().await) } else { None };
            let mut current_offset = file_offset;
            let mut any_inserted = false;

            for (idx, location) in chunk_locations.iter().enumerate() {
                let chunk_start = if idx == 0 { 0 } else {
                    chunk_locations[..idx].iter().map(|l| l.size as u64).sum::<u64>() as usize
                };
                let chunk_end = chunk_start + location.size;
                let arc = Arc::new(data[chunk_start..chunk_end].to_vec());

                self.chunk_cache.insert(location.chunk_id, Arc::clone(&arc));
                any_inserted = true;

                if let Some(ref mut bc) = byte_cache {
                    let key = ByteRangeCacheKey { inode, file_offset: current_offset };
                    bc.put(key, CachedChunk {
                        data: Arc::clone(&arc),
                        chunk_size: location.size,
                        cached_at: std::time::Instant::now(),
                    });
                }

                current_offset += location.size as u64;
            }

            if any_inserted {
                self.chunk_landed.notify_waiters();
            }
        }

        Ok(chunk_locations)
    }

    /// Send a batch of chunk locations to the leader in one RPC (ReplicateChunkLocations,
    /// the plural wire message), with the same 4-attempt exponential-backoff retry shape
    /// every per-location call site already used individually. Used both directly (where a
    /// call site already has a Vec<ChunkLocation> in hand) and as the sink for the queued
    /// single-location call sites (see pending_chunk_locations / its drain task) — either
    /// way, callers never depend on this succeeding before proceeding: flush_metadata_sync
    /// delivers the file's complete, authoritative chunk_locations state at the end of every
    /// flush cycle regardless, so this is strictly a latency optimization, not a correctness
    /// requirement. A no-op for an empty batch.
    async fn send_chunk_locations_batched(&self, leader: SocketAddr, locations: Vec<dfs_common::ChunkLocation>) -> Result<()> {
        if locations.is_empty() {
            return Ok(());
        }
        let req = Request::ReplicateChunkLocations { locations };
        let mut backoff_ms = 250u64;
        for attempt in 1u32..=4 {
            match tokio::time::timeout(Duration::from_secs(3), self.send_request(leader, req.clone())).await {
                Ok(Ok(_)) => return Ok(()),
                Ok(Err(e)) => warn!("ReplicateChunkLocations to leader {} failed (attempt {}): {}", leader, attempt, e),
                Err(_)    => warn!("ReplicateChunkLocations to leader {} timed out (attempt {})", leader, attempt),
            }
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(4_000);
        }
        anyhow::bail!("ReplicateChunkLocations to leader {} failed after 4 attempts", leader)
    }

    /// Return a snapshot of the NodeId→SocketAddr reverse map for use by callers that
    /// need to resolve chunk location node-ids to addresses (e.g. prefetch hint building).
    pub async fn node_id_to_addr_snapshot(&self) -> HashMap<NodeId, SocketAddr> {
        self.addr_to_node_id.read().await
            .iter().map(|(&addr, &id)| (id, addr)).collect()
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
        file_id: dfs_common::FileId,
    ) -> Result<Vec<dfs_common::ChunkLocation>> {
        const WRITE_TIMEOUT_SECS: u64 = 30;

        // Build the ordered list of candidates: preferred pair ALWAYS first, then rest
        // nodes sorted by health as fallback.
        //
        // We do NOT demote penalized preferred nodes behind healthy rest nodes.
        // Rationale: the preferred pair is where the chunk lives (or should live for
        // consistent routing). Writing to a different pair because the preferred nodes
        // are slow causes canonical drift — the next patch must go to the canonical pair,
        // and if we moved the data elsewhere, that patch will fail with "chunk not found."
        // A slow preferred node is always better than a fast node that doesn't have the chunk.
        // If a preferred node is truly unreachable it will time out and we fall back to rest.
        let rest: Vec<SocketAddr> = self.node_health.sort_by_health(
            &all_nodes.iter().copied()
                .filter(|&n| n != replica1 && n != replica2)
                .collect::<Vec<_>>()
        ).await;

        let mut candidates: Vec<SocketAddr> = vec![replica1, replica2];
        candidates.extend_from_slice(&rest);

        // Try the preferred pair in parallel first — halves write latency on the hot path.
        // If either fails, fall back to serial retries from the remaining candidates.
        let mut successful: Vec<(SocketAddr, Response)> = Vec::new();

        if candidates.len() >= 2 {
            let n1 = candidates[0];
            let n2 = candidates[1];

            // Optimize: Use split-frame encoding to avoid bincode serialization of 4MB payload.
            // Serialize a small envelope (data=empty) once, send to both replicas with raw bytes.
            // This eliminates bincode overhead (~20-40ms) plus one 4MB copy (~25ms) = ~45-65ms savings.
            let request = Request::WriteFileLocalOnly {
                data: Vec::new(),  // Empty = split-frame indicator
                file_offset,
                file_id,
            };
            let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
            let envelope = MessageEnvelope::new(request_id, Message::Request(request));
            let encoded = envelope.to_bytes().context("Failed to serialize write request")?;

            // WRITETIMING: per-target latency for the data phase, measured from the same
            // start point so a stall on one specific replica (e.g. a node mid-planned-
            // compaction-pause) is directly visible instead of hidden inside an aggregate
            // "dual-replica write complete" number. Added 2026-07-14 to spot anomalies
            // rather than infer them from throughput alone — see this session's offline-
            // compaction regression investigation.
            let replica_write_start = std::time::Instant::now();
            let t1 = tokio::time::timeout(
                tokio::time::Duration::from_secs(WRITE_TIMEOUT_SECS),
                self.send_split_frame_write_request(n1, &encoded, data),
            );
            let t2 = tokio::time::timeout(
                tokio::time::Duration::from_secs(WRITE_TIMEOUT_SECS),
                self.send_split_frame_write_request(n2, &encoded, data),
            );
            let (r1, r2) = tokio::join!(t1, t2);
            let n1_ms = replica_write_start.elapsed().as_secs_f64() * 1000.0;
            match r1 {
                Ok(Ok(resp)) => { debug!("Parallel replica write succeeded to {}", n1); successful.push((n1, resp)); }
                Ok(Err(e))   => { warn!("Parallel replica write failed: {}: {}, will retry serially", n1, e); }
                Err(_)       => { warn!("Parallel replica write failed: {}: timeout after {}s, will retry serially", n1, WRITE_TIMEOUT_SECS); }
            }
            let n2_ms = replica_write_start.elapsed().as_secs_f64() * 1000.0;
            match r2 {
                Ok(Ok(resp)) => { debug!("Parallel replica write succeeded to {}", n2); successful.push((n2, resp)); }
                Ok(Err(e))   => { warn!("Parallel replica write failed: {}: {}, will retry serially", n2, e); }
                Err(_)       => { warn!("Parallel replica write failed: {}: timeout after {}s, will retry serially", n2, WRITE_TIMEOUT_SECS); }
            }
            // Gap since this same target's previous op — see write_target_last_op_at's
            // field doc comment. None (first-ever op to this target) prints as -1 so it's
            // unambiguous in logs/scripts rather than looking like a real zero-gap.
            let now = std::time::Instant::now();
            let n1_gap_ms = self.write_target_last_op_at.insert(n1, now)
                .map(|prev| now.duration_since(prev).as_secs_f64() * 1000.0).unwrap_or(-1.0);
            let n2_gap_ms = self.write_target_last_op_at.insert(n2, now)
                .map(|prev| now.duration_since(prev).as_secs_f64() * 1000.0).unwrap_or(-1.0);
            info!("WRITETIMING data-phase inode={} offset={} replica1={} replica1_ms={:.1} replica1_gap_ms={:.1} replica2={} replica2_ms={:.1} replica2_gap_ms={:.1}",
                inode, file_offset, n1, n1_ms, n1_gap_ms, n2, n2_ms, n2_gap_ms);
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

            // Use split-frame encoding for serial fallback too
            let request = Request::WriteFileLocalOnly {
                data: Vec::new(),  // Empty = split-frame indicator
                file_offset,
                file_id,
            };
            let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
            let envelope = MessageEnvelope::new(request_id, Message::Request(request));
            let encoded = envelope.to_bytes().context("Failed to serialize write request")?;

            let result = tokio::time::timeout(
                tokio::time::Duration::from_secs(WRITE_TIMEOUT_SECS),
                self.send_split_frame_write_request(node, &encoded, data),
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
                written_at: None, // fresh writes use None — see build_chunk_locations_from_ids
                client_write_seq: self.write_seq.get(&file_id).map(|e| *e),
                file_id: Some(file_id),
            };

            chunk_locations.push(location);
            current_offset += chunk_size as u64;
        }
        drop(node_id_map);

        // Deliver ChunkLocation only to the leader. Replica nodes already hold the data
        // and don't need a chunk_map notification — they're not the stale-check gatekeeper.
        // Non-leader followers get the full authoritative state via flush_metadata_sync
        // (PutFileMetadata with write_seq ordering) at the end of the flush cycle.
        // Sending to replica nodes or other followers creates stale broadcast races.
        //
        // One batched ReplicateChunkLocations RPC for the whole multi-chunk write
        // instead of one ReplicateChunkLocation per chunk (each previously with its own
        // sequentially-awaited 4-attempt retry — N chunks could mean N round trips,
        // sequentially). Each location already carries file_id (set at construction
        // above), which the leader's handle_replicate_chunk_locations uses the same way
        // the singular handler's top-level file_id parameter used to (chunk_map update
        // by file_offset, and the file-record patch so DisseminateMetadata broadcasts
        // the correct chunk_id) — see metadata::live_file_ids and
        // handle_replicate_chunk_locations's orphan-gate/parity fix for why a per-item
        // embedded file_id is required here, not optional.
        // Try immediate delivery first (lower latency than waiting for the
        // background worker's 10ms tick on the common, leader-reachable path),
        // but re-queue on failure instead of dropping — see
        // start_chunk_location_batch_worker's and flush_metadata_sync's
        // identical fix for the full rationale (the chunk's data write already
        // completed by the time it lands here; dropping the location
        // registration orphans real, already-durable bytes with nothing in
        // metadata ever pointing to them again). This was the one remaining
        // unprotected call site: MultiPatch's own registration already goes
        // through enqueue_chunk_location (the queue), but this fresh-write path
        // delivered inline with no re-queue fallback at all — including
        // silently dropping with no warning whatsoever when leader_addr was
        // simply unknown at call time. Real 2026-07-10/11 incident (staging
        // fio+fsck repro): a fresh chunk write's location never reached the
        // leader during a ~16s leader outage, leaving the leader with zero
        // record of a chunk that later patches kept chaining on top of.
        let leader_addr = *self.leader_addr.read().await;
        match leader_addr {
            Some(leader) => {
                // Bound this to a short outer timeout, not send_chunk_locations_batched's
                // own up-to-~15s internal 4-attempt retry (3s timeout x4 + backoff) — this
                // is a latency optimization the caller never depends on (see that
                // function's doc comment: flush_metadata_sync delivers the authoritative
                // state regardless, and the re-queue fallback right below already exists
                // for exactly this failure case). Awaiting the full retry here blocked
                // this one fresh write's completion for up to ~15s whenever the leader
                // was merely busy, not down — confirmed live 2026-07-12 via fio directly
                // against the DFS mount: clat tail latencies up to 20s during a 4K random
                // write test, tracing back to exactly this call.
                let leader_write_start = std::time::Instant::now();
                let result = tokio::time::timeout(
                    Duration::from_secs(1),
                    self.send_chunk_locations_batched(leader, chunk_locations.clone()),
                ).await;
                let leader_ms = leader_write_start.elapsed().as_secs_f64() * 1000.0;
                let failed = match result {
                    Ok(Ok(())) => false,
                    Ok(Err(e)) => {
                        warn!("WriteChunk: batched ChunkLocations to leader {} failed: {} — re-queuing for the background worker", leader, e);
                        true
                    }
                    Err(_) => {
                        warn!("WriteChunk: batched ChunkLocations to leader {} took >1s — re-queuing for the background worker instead of blocking this write", leader);
                        true
                    }
                };
                // WRITETIMING metadata phase — see the data-phase log above's doc comment.
                let now = std::time::Instant::now();
                let leader_gap_ms = self.write_target_last_op_at.insert(leader, now)
                    .map(|prev| now.duration_since(prev).as_secs_f64() * 1000.0).unwrap_or(-1.0);
                info!("WRITETIMING metadata-phase inode={} offset={} leader={} leader_ms={:.1} leader_gap_ms={:.1} failed={}",
                    inode, file_offset, leader, leader_ms, leader_gap_ms, failed);
                if failed {
                    let mut pending = self.pending_chunk_locations.lock().await;
                    for loc in chunk_locations.clone() {
                        upsert_chunk_location(&mut pending, loc);
                    }
                }
            }
            None => {
                warn!("WriteChunk: no known leader — re-queuing {} chunk location(s) for the background worker", chunk_locations.len());
                let mut pending = self.pending_chunk_locations.lock().await;
                for loc in chunk_locations.clone() {
                    upsert_chunk_location(&mut pending, loc);
                }
            }
        }

        // Populate caches for immediate read-back of freshly-written data.
        // chunk_cache (keyed by ChunkId) enables full-chunk reads to hit cache.
        // byte_range_cache (keyed by inode+offset) enables sub-chunk range reads to hit cache.
        {
            let mut byte_cache = if inode > 0 { Some(self.byte_range_cache.shard(inode).lock().await) } else { None };
            let mut current_offset = file_offset;
            let mut any_inserted = false;

            for (idx, location) in chunk_locations.iter().enumerate() {
                let chunk_start = if idx == 0 { 0 } else {
                    chunk_locations[..idx].iter().map(|l| l.size as u64).sum::<u64>() as usize
                };
                let chunk_end = chunk_start + location.size;
                let arc = Arc::new(data[chunk_start..chunk_end].to_vec());

                self.chunk_cache.insert(location.chunk_id, Arc::clone(&arc));
                any_inserted = true;

                if let Some(ref mut bc) = byte_cache {
                    let key = ByteRangeCacheKey { inode, file_offset: current_offset };
                    bc.put(key, CachedChunk {
                        data: Arc::clone(&arc),
                        chunk_size: location.size,
                        cached_at: std::time::Instant::now(),
                    });
                }

                current_offset += location.size as u64;
            }

            if any_inserted {
                self.chunk_landed.notify_waiters();
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
    pub async fn write_data_with_cache(&self, data: &[u8], inode: u64, file_offset: u64, file_id: dfs_common::FileId, preferred_nodes: Option<&[SocketAddr]>) -> Result<(Vec<ChunkId>, Vec<u64>, Option<Vec<dfs_common::ChunkLocation>>)> {
        let nodes = self.cluster_nodes.read().await.clone();
        if nodes.len() < 2 {
            // Single-node cluster: fall back to server-side replication
            let (chunk_ids, chunk_sizes, replica_nodes_per_chunk) = self.write_data_single_chunk_tracked(data, file_id).await?;
            let cws = self.write_seq.get(&file_id).map(|e| *e);
            let locations = Self::build_chunk_locations_from_ids(&chunk_ids, &chunk_sizes, file_offset, replica_nodes_per_chunk, cws, file_id);

            // Seed chunk_cache here so callers never need to re-clone `data` themselves
            // to do it — mirrors write_data_dual_replica's seeding below for the
            // multi-node case, which is the dominant path in production.
            let mut chunk_start = 0usize;
            for loc in &locations {
                let chunk_end = chunk_start + loc.size;
                self.chunk_cache.insert(loc.chunk_id, Arc::new(data[chunk_start..chunk_end].to_vec()));
                chunk_start = chunk_end;
            }

            return Ok((chunk_ids, chunk_sizes, Some(locations)));
        }

        let chunk_locations = self.write_data_dual_replica(data, inode, file_offset, file_id, preferred_nodes).await?;

        // Extract chunk IDs and sizes for backward compatibility
        let chunk_ids: Vec<ChunkId> = chunk_locations.iter().map(|loc| loc.chunk_id).collect();
        let chunk_sizes: Vec<u64> = chunk_locations.iter().map(|loc| loc.size as u64).collect();

        Ok((chunk_ids, chunk_sizes, Some(chunk_locations)))
    }

    /// Write a chunk to a specific server
    async fn write_chunk_to_server(server_addr: SocketAddr, data: Vec<u8>, file_id: dfs_common::FileId) -> Result<(Vec<ChunkId>, Vec<u64>)> {
        let total_start = std::time::Instant::now();
        let data_len = data.len();

        let request = Request::WriteFile { data, file_id };

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

        // Send request — coalesced into one write to cut packet/RTT overhead.
        let send_start = std::time::Instant::now();
        let len = encoded.len() as u32;
        let mut framed = Vec::with_capacity(4 + encoded.len());
        framed.extend_from_slice(&len.to_be_bytes());
        framed.extend_from_slice(&encoded);
        stream.write_all(&framed).await?;
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
    async fn write_chunk_to_server_local_only(server_addr: SocketAddr, data: Vec<u8>, file_offset: u64, file_id: dfs_common::FileId) -> Result<(Vec<ChunkId>, Vec<u64>)> {
        let total_start = std::time::Instant::now();
        let data_len = data.len();

        let request = Request::WriteFileLocalOnly { data, file_offset, file_id };

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

        // Send request — coalesced into one write to cut packet/RTT overhead.
        let len = encoded.len() as u32;
        let mut framed = Vec::with_capacity(4 + encoded.len());
        framed.extend_from_slice(&len.to_be_bytes());
        framed.extend_from_slice(&encoded);
        stream.write_all(&framed).await?;
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
    pub async fn write_data_single_chunk(&self, data: &[u8], file_id: dfs_common::FileId) -> Result<(Vec<ChunkId>, Vec<u64>)> {
        let (chunk_ids, chunk_sizes, _) = self.write_data_single_chunk_tracked(data, file_id).await?;
        Ok((chunk_ids, chunk_sizes))
    }

    /// Like write_data_single_chunk but also returns per-chunk replica node lists.
    /// The server includes all replica NodeIds in the ChunkIds response.
    async fn write_data_single_chunk_tracked(&self, data: &[u8], file_id: dfs_common::FileId) -> Result<(Vec<ChunkId>, Vec<u64>, Vec<Vec<NodeId>>)> {
        let request = Request::WriteFile {
            data: data.to_vec(),
            file_id,
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
        file_id: FileId,
        chunk_file_offset: u64,
        intra_offset: usize,
        patch_data: Vec<u8>,
        old_location: &dfs_common::ChunkLocation,
    ) -> Result<dfs_common::ChunkLocation> {
        self.patch_chunk_on_replicas_inner(old_chunk_id, file_id, None, chunk_file_offset, intra_offset, patch_data, old_location).await
    }

    pub async fn patch_chunk_on_replicas_verified(
        &self,
        old_chunk_id: ChunkId,
        file_id: FileId,
        chunk_idx: u64,
        chunk_file_offset: u64,
        intra_offset: usize,
        patch_data: Vec<u8>,
        old_location: &dfs_common::ChunkLocation,
    ) -> Result<dfs_common::ChunkLocation> {
        self.patch_chunk_on_replicas_inner(old_chunk_id, file_id, Some(chunk_idx), chunk_file_offset, intra_offset, patch_data, old_location).await
    }

    async fn patch_chunk_on_replicas_inner(
        &self,
        mut old_chunk_id: ChunkId,
        file_id: FileId,
        chunk_idx: Option<u64>,
        chunk_file_offset: u64,
        intra_offset: usize,
        patch_data: Vec<u8>,
        old_location: &dfs_common::ChunkLocation,
    ) -> Result<dfs_common::ChunkLocation> {
        // On ChunkStale, the server tells us the current chunk_id — retry once with it.
        let mut current_location = old_location.clone();
        // Computed once outside the retry loop below: a ChunkStale retry is the same
        // logical operation, not a new one, so it must reuse the same target sequence.
        let new_chunk_seq = chunk_idx.map(|cidx| self.next_chunk_seq(file_id, cidx));
        for attempt in 0u8..2 {
        // Resolve NodeId -> SocketAddr for the replica nodes
        let node_id_to_addr: HashMap<dfs_common::NodeId, SocketAddr> = {
            let addr_map = self.addr_to_node_id.read().await;
            addr_map.iter().map(|(&addr, &id)| (id, addr)).collect()
        };
        let all_cluster_nodes = self.cluster_nodes.read().await.clone();

        let replica_addrs: Vec<SocketAddr> = if current_location.nodes.is_empty() {
            all_cluster_nodes.clone()
        } else {
            current_location.nodes.iter()
                .filter_map(|nid| node_id_to_addr.get(nid).copied())
                .collect()
        };

        if replica_addrs.is_empty() {
            anyhow::bail!("PatchChunk: no replica addresses resolved for chunk {}", old_chunk_id);
        }

        let addr_to_node_id_snap = self.addr_to_node_id.read().await.clone();

        let patch_req = Request::PatchChunk {
            chunk_id: old_chunk_id,
            file_id,
            chunk_idx,
            chunk_file_offset,
            intra_offset,
            data: patch_data.clone(),
            new_chunk_seq,
        };

        let futures: Vec<_> = replica_addrs.iter().map(|&addr| {
            let client = self.clone();
            let req = patch_req.clone();
            async move { (addr, client.send_request(addr, req).await) }
        }).collect();

        let results = futures::future::join_all(futures).await;

        let mut new_chunk_id: Option<ChunkId> = None;
        let mut new_size: usize = current_location.size;
        let mut patched_node_ids: Vec<dfs_common::NodeId> = Vec::new();
        let mut stale_response: Option<(ChunkId, Vec<dfs_common::NodeId>)> = None;

        for (addr, result) in results {
            match result {
                Ok(Response::PatchChunkResult { new_chunk_id: ncid, size, chunk_seq: _ }) => {
                    // A non-empty patch that returns the same chunk_id means the node read stale
                    // base data and the patch landed on wrong content. Skip this result so
                    // the stale node doesn't contaminate the consensus hash.
                    if ncid == old_chunk_id {
                        warn!("PatchChunk replica {} returned unchanged chunk_id {} after patch — stale base, skipping this replica", addr, ncid);
                        continue;
                    }
                    if let Some(existing) = new_chunk_id {
                        if existing != ncid {
                            warn!("PatchChunk REPLICA DISAGREEMENT: {} returned {} but previous returned {} — stale base chunk on one replica",
                                addr, ncid, existing);
                            // Don't overwrite new_chunk_id — keep the first (leader-preferred) value.
                            continue;
                        }
                    }
                    new_chunk_id = Some(ncid);
                    new_size = size;
                    if let Some(&nid) = addr_to_node_id_snap.get(&addr) {
                        patched_node_ids.push(nid);
                    }
                }
                Ok(Response::ChunkStale { current_chunk_id, current_nodes }) => {
                    // Server says our chunk_id is stale — it didn't apply the patch.
                    // Use the server's current chunk_id for the retry.
                    if stale_response.is_none() {
                        stale_response = Some((current_chunk_id, current_nodes));
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

        // If any replica said stale and none succeeded, retry with corrected chunk_id.
        if new_chunk_id.is_none() {
            if let Some((corrected_id, corrected_nodes)) = stale_response {
                if attempt == 0 {
                    warn!("PatchChunk: client chunk_id {} is stale, retrying with server's {} (attempt {})",
                        old_chunk_id, corrected_id, attempt + 1);
                    old_chunk_id = corrected_id;
                    current_location = dfs_common::ChunkLocation {
                        chunk_id: corrected_id,
                        nodes: corrected_nodes,
                        size: current_location.size,
                        checksum: corrected_id.hash,
                        file_offset: current_location.file_offset,
                        written_at: None,
                        client_write_seq: self.write_seq.get(&file_id).map(|e| *e),
                        file_id: Some(file_id),
                    };
                    continue;
                }
            }
        }

        let new_chunk_id = match new_chunk_id {
            Some(id) => id,
            None => anyhow::bail!("PatchChunk: all replicas failed for chunk {}", old_chunk_id),
        };

        // Step 3: Send new ChunkLocation synchronously to leader + replica nodes.
        // The leader re-broadcasts to remaining followers async. Non-replica nodes must
        // NOT receive direct ReplicateChunkLocation: they'd update their chunk_map to the
        // new chunk_id without holding the data, causing them to return stale ChunkStale
        // corrections on the next patch — a ghost reference that can't be resolved.
        let patch_written_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let new_location = dfs_common::ChunkLocation {
            chunk_id: new_chunk_id,
            nodes: patched_node_ids.clone(),
            size: new_size,
            checksum: new_chunk_id.hash,
            file_offset: current_location.file_offset,
            written_at: Some(patch_written_at),
            client_write_seq: self.write_seq.get(&file_id).map(|e| *e),
            file_id: Some(file_id),
        };
        // Hand off to the background batch worker instead of sending+retrying inline —
        // see pending_chunk_locations's doc comment for why this is safe. new_location
        // already carries file_id (set above), which is all the batched leader-side
        // handler needs (no separate top-level parameter the way the old per-item
        // request shape had one).
        self.enqueue_chunk_location(new_location.clone()).await;

        // Step 4 (removed): We no longer eagerly delete old_chunk_id from non-replica nodes.
        // Under concurrent patches (A→B→C), the async delete of A would race with B being
        // spread by the healer, and the delete of B (from patch 2) could fire before patch 2's
        // ReplicateChunkLocation broadcast reaches all nodes — causing C to be unfindable.
        // The healer already handles cleanup of over-replicated old chunks safely.

        info!("PatchChunk: {} -> {} ({} replicas patched)", old_chunk_id, new_chunk_id, patched_node_ids.len());
        return Ok(new_location);
        } // end retry loop
        anyhow::bail!("PatchChunk: exhausted retries for chunk {}", old_chunk_id)
    }

    /// Patch a chunk at `chunk_idx` within `file_path`, with leader validation.
    ///
    /// Before patching, asks the leader for the current chunk location for that index.
    /// If the leader returns a different chunk ID than `expected_chunk_id`, the caller's
    /// view is stale — we use the leader's authoritative ID instead. This prevents the
    /// concurrent-overwrite race where two writers both snapshot the same old chunk ID,
    /// one patches it, and the second tries to patch the now-deleted chunk.
    ///
    /// Returns (new_location, fresh_metadata) so the caller can update metadata_cache
    /// with the complete fresh picture from the leader (not just the patched chunk).
    pub async fn patch_chunk_with_leader_verify(
        &self,
        file_path: &str,
        chunk_idx: u64,
        expected_chunk_id: ChunkId,
        chunk_file_offset: u64,
        intra_offset: usize,
        patch_data: Vec<u8>,
    ) -> Result<(dfs_common::ChunkLocation, FileMetadata)> {
        // Fetch authoritative metadata from leader.
        let fresh_meta = self.get_file_metadata(file_path).await?
            .ok_or_else(|| anyhow::anyhow!("patch_chunk_with_leader_verify: file not found: {}", file_path))?;

        let current_loc = fresh_meta.chunk_location_for_idx(chunk_idx)
            .ok_or_else(|| anyhow::anyhow!("patch_chunk_with_leader_verify: chunk {} not in leader metadata for {}", chunk_idx, file_path))?
            .clone();

        if current_loc.chunk_id != expected_chunk_id {
            info!("patch_chunk_with_leader_verify: chunk {} stale — expected {} leader has {}, using leader ID",
                chunk_idx, expected_chunk_id, current_loc.chunk_id);
        }

        let new_loc = self.patch_chunk_on_replicas_verified(
            current_loc.chunk_id,
            fresh_meta.id,
            chunk_idx,
            chunk_file_offset,
            intra_offset,
            patch_data,
            &current_loc,
        ).await?;

        Ok((new_loc, fresh_meta))
    }

    /// Apply multiple non-contiguous byte-range patches to a chunk in a single RPC.
    /// Equivalent to patch_chunk_on_replicas but sends all dirty ranges in one request,
    /// so the server applies them atomically without serial round-trips or gap zero-fills.
    pub async fn multi_patch_chunk_on_replicas(
        &self,
        old_chunk_id: ChunkId,
        file_id: FileId,
        chunk_file_offset: u64,
        patches: Vec<(usize, Vec<u8>)>,
        old_location: &dfs_common::ChunkLocation,
        expected_new_chunk_id: Option<ChunkId>,
        dual_rf: bool,
        per_server_hints: Arc<HashMap<SocketAddr, Vec<ChunkId>>>,
    ) -> Result<(dfs_common::ChunkLocation, Vec<(SocketAddr, ChunkId)>)> {
        self.multi_patch_chunk_on_replicas_inner(old_chunk_id, file_id, None, chunk_file_offset, patches, old_location, expected_new_chunk_id, dual_rf, per_server_hints).await
    }

    pub async fn multi_patch_chunk_on_replicas_verified(
        &self,
        old_chunk_id: ChunkId,
        file_id: FileId,
        chunk_idx: u64,
        chunk_file_offset: u64,
        patches: Vec<(usize, Vec<u8>)>,
        old_location: &dfs_common::ChunkLocation,
        expected_new_chunk_id: Option<ChunkId>,
        dual_rf: bool,
        per_server_hints: Arc<HashMap<SocketAddr, Vec<ChunkId>>>,
    ) -> Result<(dfs_common::ChunkLocation, Vec<(SocketAddr, ChunkId)>)> {
        self.multi_patch_chunk_on_replicas_inner(old_chunk_id, file_id, Some(chunk_idx), chunk_file_offset, patches, old_location, expected_new_chunk_id, dual_rf, per_server_hints).await
    }

    async fn multi_patch_chunk_on_replicas_inner(
        &self,
        mut old_chunk_id: ChunkId,
        file_id: FileId,
        chunk_idx: Option<u64>,
        chunk_file_offset: u64,
        patches: Vec<(usize, Vec<u8>)>,
        old_location: &dfs_common::ChunkLocation,
        expected_new_chunk_id: Option<ChunkId>,
        dual_rf: bool,
        per_server_hints: Arc<HashMap<SocketAddr, Vec<ChunkId>>>,
    ) -> Result<(dfs_common::ChunkLocation, Vec<(SocketAddr, ChunkId)>)> {
        // Timing instrumentation added 2026-07-12 to find the actual dominant cost in
        // this function under real kdiskmark-style load — logged unconditionally (not
        // just on a slow threshold) so a full run's timing distribution can be pulled
        // straight from the log rather than guessed at. TIMING lines are intentionally
        // greppable as one token.
        let fn_start = std::time::Instant::now();
        let mut timing_rpc = std::time::Duration::ZERO;
        let mut timing_backfill = std::time::Duration::ZERO;
        let mut timing_fold = std::time::Duration::ZERO;
        let mut timing_fold_permit_wait = std::time::Duration::ZERO;
        let original_old_chunk_id = old_chunk_id;
        let mut current_location = old_location.clone();
        let mut skip_addrs: Vec<SocketAddr> = vec![];
        // Computed once for the whole call, unlike patch_client_write_seq below: a
        // ChunkStale retry within the loop is the same logical operation resent with
        // a corrected base, not a new one, so it must target the same sequence value.
        let new_chunk_seq = chunk_idx.map(|cidx| self.next_chunk_seq(file_id, cidx));
        // How long to keep retrying when every replica failed via a pure connection
        // failure (not a data error, not a ChunkStale response) — see the retry site
        // below for the incident this closes. A compaction-triggered self-restart
        // (redb exclusive-lock wedge -> process exit -> systemd restart) resolves in
        // ~8-15s; 20s gives real margin without hanging a write indefinitely on a
        // genuinely dead node (permanent failures still surface, just after riding out
        // a transient one first).
        const CONNECT_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
        const CONNECT_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(750);
        // Original behavior preserved: a stale-base response got one immediate retry
        // (attempt 0 -> 1) before falling through to "all replicas failed" — matches
        // the original `0u8..2` loop bound's effective stale-retry allowance.
        const MAX_STALE_RETRY_ATTEMPTS: u8 = 1;
        let mut stale_retry_attempts: u8 = 0;
        let retry_started = std::time::Instant::now();
        // Attempt count is now just a hard safety backstop against a genuine infinite
        // loop bug — the real gate on connection-failure retries is CONNECT_RETRY_BUDGET
        // (wall-clock), and each such retry consumes one attempt here.
        let mut timing_setup = std::time::Duration::ZERO;
        'retry: for _attempt in 0u32..1000 {
        let iteration_start = std::time::Instant::now();
        let node_id_to_addr: HashMap<dfs_common::NodeId, SocketAddr> = {
            let addr_map = self.addr_to_node_id.read().await;
            addr_map.iter().map(|(&addr, &id)| (id, addr)).collect()
        };
        let all_cluster_nodes = self.cluster_nodes.read().await.clone();

        let mut replica_addrs: Vec<SocketAddr> = if current_location.nodes.is_empty() {
            all_cluster_nodes.clone()
        } else {
            current_location.nodes.iter()
                .filter_map(|nid| node_id_to_addr.get(nid).copied())
                .collect()
        };

        if replica_addrs.is_empty() {
            // NodeIds in the chunk location don't resolve — addr↔NodeId map is stale.
            // Step 1: refresh cluster membership to rebuild the map, then retry resolution.
            // Step 2: if still unresolvable, query the leader for the current authoritative
            //         chunk location (may list different NodeIds after healer moved the chunk).
            // Step 3: last resort — broadcast to all cluster nodes and let stale-base
            //         retry find the correct holder.
            warn!("MultiPatch: no replica addresses resolved for chunk {} ({} location node(s)) — refreshing cluster nodes",
                  old_chunk_id, current_location.nodes.len());
            let _ = self.refresh_cluster_nodes().await;

            let refreshed_node_id_to_addr: HashMap<dfs_common::NodeId, SocketAddr> = {
                let addr_map = self.addr_to_node_id.read().await;
                addr_map.iter().map(|(&addr, &id)| (id, addr)).collect()
            };
            replica_addrs = current_location.nodes.iter()
                .filter_map(|nid| refreshed_node_id_to_addr.get(nid).copied())
                .collect();

            if replica_addrs.is_empty() {
                warn!("MultiPatch: chunk {} NodeIds still unresolved after cluster refresh — querying leader for current location",
                      old_chunk_id);
                if let Some(cidx) = chunk_idx {
                    if let Ok(Some(fresh_loc)) = self.get_single_chunk_location(file_id, cidx).await {
                        let fresh_addrs: Vec<SocketAddr> = fresh_loc.nodes.iter()
                            .filter_map(|nid| refreshed_node_id_to_addr.get(nid).copied())
                            .collect();
                        if !fresh_addrs.is_empty() {
                            info!("MultiPatch: resolved {} replica(s) for chunk {} via leader query",
                                  fresh_addrs.len(), old_chunk_id);
                            replica_addrs = fresh_addrs;
                            current_location = fresh_loc;
                        }
                    }
                }
            }

            if replica_addrs.is_empty() {
                warn!("MultiPatch: falling back to all {} cluster nodes for chunk {}",
                      all_cluster_nodes.len(), old_chunk_id);
                replica_addrs = all_cluster_nodes.clone();
            }
        }

        if replica_addrs.is_empty() {
            anyhow::bail!("MultiPatch: no cluster nodes available for chunk {}", old_chunk_id);
        }

        let leader_addr = *self.leader_addr.read().await;

        // Sort deterministically: leader first, remaining by address.
        // Consistent ordering means the same nodes are always primary targets, which
        // makes stale-base retries predictable (the stale response points back to the
        // nodes we always write to).
        if let Some(la) = leader_addr {
            if let Some(pos) = replica_addrs.iter().position(|&a| a == la) {
                replica_addrs.swap(0, pos);
            }
        }
        if replica_addrs.len() > 1 {
            replica_addrs[1..].sort();
        }

        // Dual-RF: patch the first 2 replicas; the 3rd is tombstoned (synchronously, below)
        // so the healer cannot use it as a source before metadata commits.
        let patch_addrs: Vec<SocketAddr> = if dual_rf && replica_addrs.len() > 2 {
            skip_addrs = replica_addrs[2..].to_vec();
            replica_addrs[..2].to_vec()
        } else {
            skip_addrs = vec![];
            replica_addrs.clone()
        };

        let addr_to_node_id_snap = self.addr_to_node_id.read().await.clone();

        // Consume a fresh write_seq for this patch so the leader can use it to order
        // concurrent RCL notifications from the same file without relying on wall clocks.
        // MUST increment (not just read): a single flush cycle can issue several rapid
        // MultiPatch rotations for the same chunk (e.g. qcow2 preallocation patching
        // adjacent 64KB clusters back-to-back) before the next flush_metadata_sync bumps
        // write_seq. If they all carried the same seq, the leader's chunk_map merge
        // (`inc >= ext`) accepts whichever RCL physically arrives last — which can be an
        // intermediate rotation, not the final one — leaving chunk_map pointing at stale
        // chunk data that a later full-chunk read serves to the application.
        let patch_client_write_seq = Some(self.next_write_seq(file_id));

        // Split-frame MultiPatch: when total patch data is large enough that bincode
        // serialization overhead matters (>= 32KB), serialize the envelope once with
        // empty patch data as a signal, then send raw patch bytes separately.
        // Same optimization as the fresh write path — eliminates ~20-40ms bincode overhead
        // per node (40-80ms total for dual-replica) on large patch payloads.
        const SPLIT_FRAME_THRESHOLD: usize = 32 * 1024;
        let total_patch_data: usize = patches.iter().map(|(_, d)| d.len()).sum();

        let rpc_start = std::time::Instant::now();
        timing_setup += rpc_start.duration_since(iteration_start);
        let results = if total_patch_data >= SPLIT_FRAME_THRESHOLD {
            // Raw payload shared across all replicas: [4B len0][data0][4B len1][data1]...
            let mut raw_payload = Vec::with_capacity(
                patches.iter().map(|(_, d)| 4 + d.len()).sum::<usize>()
            );
            for (_, data) in &patches {
                raw_payload.extend_from_slice(&(data.len() as u32).to_be_bytes());
                raw_payload.extend_from_slice(data);
            }
            let raw_payload = Arc::new(raw_payload);

            // Each addr gets its own envelope: hints differ per server and the envelope
            // is tiny (metadata only — patch data travels in the shared raw_payload arc).
            let futures: Vec<_> = patch_addrs.iter().map(|&addr| {
                let client = self.clone();
                let raw = Arc::clone(&raw_payload);
                let empty_patches: Vec<(usize, Vec<u8>)> = patches.iter()
                    .map(|(off, _)| (*off, Vec::new()))
                    .collect();
                let hints = per_server_hints.get(&addr).cloned();
                let patch_req_split = Request::MultiPatch {
                    chunk_id: old_chunk_id,
                    file_id,
                    chunk_idx,
                    chunk_file_offset,
                    patches: empty_patches,
                    expected_new_chunk_id,
                    client_write_seq: patch_client_write_seq,
                    prefetch_hints: hints,
                    new_chunk_seq,
                };
                async move {
                    let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
                    let envelope = MessageEnvelope::new(request_id, Message::Request(patch_req_split));
                    let encoded = match envelope.to_bytes().context("Failed to serialize MultiPatch envelope") {
                        Ok(b) => b,
                        Err(e) => return (addr, Err(e)),
                    };
                    (addr, client.send_split_frame_write_request(addr, &encoded, &raw).await)
                }
            }).collect();
            futures::future::join_all(futures).await
        } else {
            let futures: Vec<_> = patch_addrs.iter().map(|&addr| {
                let client = self.clone();
                let hints = per_server_hints.get(&addr).cloned();
                let req = Request::MultiPatch {
                    chunk_id: old_chunk_id,
                    file_id,
                    chunk_idx,
                    chunk_file_offset,
                    patches: patches.clone(),
                    expected_new_chunk_id,
                    client_write_seq: patch_client_write_seq,
                    prefetch_hints: hints,
                    new_chunk_seq,
                };
                async move { (addr, client.send_request(addr, req).await) }
            }).collect();
            futures::future::join_all(futures).await
        };
        timing_rpc += rpc_start.elapsed();

        // Collect per-replica results before any disagreement logic.
        // (addr, Ok(ncid, size)) for success, (addr, Err) for failure.
        // (addr, Ok((chunk_id, size, patch_ts_from_server)))
        let mut replica_results: Vec<(SocketAddr, Result<(ChunkId, usize, Option<u64>)>)> = Vec::new();
        // If any replica reports a stale base, record its corrected location for a
        // potential full retry. We do NOT continue 'retry immediately upon the first
        // stale response — doing so discards all successful results from other replicas
        // and re-sends to everyone with the stale replica's hash, causing them to reject
        // it too ("all replicas failed"). Instead, finish collecting all results first,
        // then only retry if no replica succeeded (i.e. our base is wrong for everyone).
        // Replicas that did succeed are handled via the re-push mechanism below.
        let mut stale_retry: Option<(ChunkId, dfs_common::ChunkLocation)> = None;
        // Replicas that returned ChunkStale, keyed by addr → chunk_id they reported.
        // We defer their verdict: if their reported chunk_id matches the authoritative
        // patch result, the healer already brought them current and they count as patched.
        let mut stale_ahead: Vec<(SocketAddr, ChunkId)> = Vec::new();
        for (addr, result) in results {
            match result {
                Ok(Response::MultiPatchResult { new_chunk_id: ncid, size, patch_ts, chunk_seq: _ }) => {
                    // ncid == old_chunk_id means the patch produced no content change (hash
                    // unchanged). This is always a legitimate no-op — the server applied the
                    // patch bytes and got the same hash back. A stale base is signalled by the
                    // server via ChunkStale, not by an unchanged hash. Accept it as success.
                    replica_results.push((addr, Ok((ncid, size, patch_ts))));
                }
                Ok(Response::ChunkStale { current_chunk_id, current_nodes }) => {
                    warn!("MultiPatch replica {}: chunk_id {} is stale, server has {} — retrying",
                        addr, old_chunk_id, current_chunk_id);
                    stale_ahead.push((addr, current_chunk_id));
                    if stale_retry.is_none() {
                        // Save corrected location; retry only if no replica succeeded.
                        // Was gated on `attempt == 0`, but connection-failure retries
                        // (see CONNECT_RETRY_BUDGET below) can now consume several
                        // attempts before a stale response ever arrives — gate on
                        // "haven't captured one yet" instead so a stale response is
                        // captured whichever attempt it shows up on.
                        stale_retry = Some((current_chunk_id, dfs_common::ChunkLocation {
                            chunk_id: current_chunk_id,
                            nodes: current_nodes,
                            size: current_location.size,
                            checksum: current_chunk_id.hash,
                            file_offset: current_location.file_offset,
                            written_at: None,
                            client_write_seq: self.write_seq.get(&file_id).map(|e| *e),
                            file_id: Some(file_id),
                        }));
                    }
                    replica_results.push((addr, Err(anyhow::anyhow!("chunk stale"))));
                }
                Ok(Response::Error { message, .. }) => {
                    if message.contains("Failed to open chunk file")
                        || message.contains("Failed to read chunk range")
                    {
                        error!("MultiPatch replica {} error: {} (chunk data missing on this node)", addr, message);
                    } else {
                        warn!("MultiPatch replica {} error: {}", addr, message);
                    }
                    replica_results.push((addr, Err(anyhow::anyhow!("{}", message))));
                }
                Err(e) => {
                    warn!("MultiPatch replica {} failed: {}", addr, e);
                    replica_results.push((addr, Err(e)));
                }
                _ => {
                    replica_results.push((addr, Err(anyhow::anyhow!("unexpected response"))));
                }
            }
        }

        // If no replica succeeded and we have a corrected location from a stale response,
        // retry with that location. This is the only case where a full retry makes sense:
        // our assumed base was wrong for ALL replicas, so we need to patch the real base.
        let has_any_success = replica_results.iter().any(|(_, r)| r.is_ok());
        if !has_any_success {
            if let Some((fresh_id, fresh_loc)) = stale_retry {
                // Bounded independently of CONNECT_RETRY_BUDGET's much larger attempt
                // cap (1000, gated by wall-clock instead of count) — a stale response
                // recurring indefinitely would indicate a genuinely broken invariant
                // elsewhere, not something worth hammering on with no backoff.
                stale_retry_attempts += 1;
                if stale_retry_attempts <= MAX_STALE_RETRY_ATTEMPTS {
                    // Use current_nodes from the stale response — the server told us exactly
                    // which nodes hold the correct base hash. Broadcasting to all cluster
                    // nodes wastes bandwidth and throws away that knowledge. The deterministic
                    // sort+dual-RF selection runs again on this updated node list, so we
                    // still hit the same predictable 2 nodes.
                    old_chunk_id = fresh_id;
                    current_location = fresh_loc;
                    continue 'retry;
                }
                warn!("MultiPatch: chunk {} still stale after {} retries — giving up",
                    old_chunk_id, stale_retry_attempts);
            }
        }

        // Determine authoritative new_chunk_id: prefer the leader's result.
        // If the leader wasn't in replica_addrs or failed, fall back to any agreeing majority.
        let leader_result = leader_addr.and_then(|la| {
            replica_results.iter().find(|(a, _)| *a == la)
                .and_then(|(_, r)| r.as_ref().ok().copied())
        });

        let authoritative: Option<(ChunkId, usize, Option<u64>)> = if let Some(lr) = leader_result {
            Some(lr)
        } else {
            // No leader result — use the first successful result (they should all agree).
            replica_results.iter().find_map(|(_, r)| r.as_ref().ok().copied())
        };

        let (authoritative_chunk_id, authoritative_size, authoritative_patch_ts) = match authoritative {
            Some(x) => x,
            None => {
                // All replicas failed. If every single failure was transient — a node
                // that's unreachable OR reachable-but-slow/black-holing (NOT a
                // data/protocol error, NOT a ChunkStale response — those are handled
                // separately above) — this looks like a passing node outage or a
                // temporarily-overloaded leader rather than a permanent problem. Retry
                // with a bounded backoff instead of failing the write outright.
                // Root-caused 2026-07-11: a VM install hit guest-visible EIO during
                // exactly an ~8s gluster4 self-restart window (redb exclusive-lock wedge
                // -> process exit -> systemd restart) because this bailed on the very
                // first attempt. Broadened 2026-07-21 (see is_transient_write_failure):
                // it previously matched ONLY "Failed to connect", so a slow-but-alive
                // leader (which returns a *timeout* error, not a connect error) skipped
                // this retry entirely and fell through to the lossy "all replicas
                // failed" fallback below — dropping the write and leaving a gap. A slow
                // leader is precisely the case to wait out. A genuinely dead node still
                // surfaces the error, just after CONNECT_RETRY_BUDGET instead of
                // instantly.
                let all_transient_failures = !replica_results.is_empty()
                    && replica_results.iter().all(|(_, r)| {
                        r.as_ref().err()
                            .map(|e| is_transient_write_failure(&e.to_string()))
                            .unwrap_or(false)
                    });
                if all_transient_failures && retry_started.elapsed() < CONNECT_RETRY_BUDGET {
                    warn!("MultiPatch: all {} replica(s) unavailable for chunk {} (transient \
                           connect/timeout failure, elapsed {:?}/{:?}) — retrying in {:?}",
                        replica_results.len(), old_chunk_id, retry_started.elapsed(),
                        CONNECT_RETRY_BUDGET, CONNECT_RETRY_BACKOFF);
                    tokio::time::sleep(CONNECT_RETRY_BACKOFF).await;
                    continue 'retry;
                }
                anyhow::bail!("MultiPatch: all replicas failed for chunk {}", old_chunk_id)
            }
        };

        // Record only replicas that successfully applied the patch with the authoritative
        // chunk_id. Stale replicas (healer-copied old versions) are simply excluded from
        // the new location — the healer will replicate the new chunk to them on the next
        // cycle. Re-pushing is not our job: we already have our dual-RF write guarantee
        // from the nodes that succeeded, and re-pushing risks pushing incorrect data to
        // nodes whose state we don't fully know.
        let new_size: usize = authoritative_size;
        let mut patched_node_ids: Vec<dfs_common::NodeId> = Vec::new();

        for (addr, result) in &replica_results {
            match result {
                Ok((ncid, _, _)) if *ncid == authoritative_chunk_id => {
                    if let Some(&nid) = addr_to_node_id_snap.get(addr) {
                        patched_node_ids.push(nid);
                    }
                }
                Ok((ncid, _, _)) => {
                    warn!("MultiPatch REPLICA DISAGREEMENT: {} returned {} but leader/majority returned {} — excluding, healer will correct",
                        addr, ncid, authoritative_chunk_id);
                }
                Err(e) if e.to_string().contains("chunk stale") => {
                    // Check if this replica already had the target chunk (healer beat us to it).
                    // If so it holds the correct data and should be counted as patched.
                    let already_current = stale_ahead.iter()
                        .any(|(a, cid)| a == addr && *cid == authoritative_chunk_id);
                    if already_current {
                        info!("MultiPatch replica {}: already at target {} (healer concurrent) — counting as patched",
                            addr, authoritative_chunk_id);
                        if let Some(&nid) = addr_to_node_id_snap.get(addr) {
                            patched_node_ids.push(nid);
                        }
                    } else {
                        warn!("MultiPatch replica {}: stale base — excluding from location, healer will bring current",
                            addr);
                    }
                }
                Err(_) => {} // connection/other failure — already logged
            }
        }

        // Require at least 2 replicas whenever configured replication_factor >= 2 — a
        // chunk that lands on exactly 1 node is a single point of failure: if that one
        // node dies before the healer catches up, the chunk is unrecoverably lost.
        // Root-caused 2026-07-11: this exact sequence (a patch landed on only gluster1,
        // which self-restarted ~15s later from an unrelated compaction wedge)
        // permanently destroyed a real chunk of a VM disk. That incident's fix pinned
        // required_replicas to patch_addrs.len().min(2) — but patch_addrs is built from
        // the chunk's CURRENT known node list, so once a chunk was already down to 1
        // replica, required_replicas silently shrank to 1 too and was trivially
        // satisfied — no backfill even attempted on every subsequent patch. Confirmed
        // live 2026-07-17: this self-reinforcing trap caused a second, real,
        // unrecoverable data loss (chunk af931c98..., VM 111 disk install) — a patch
        // landed on the chunk's one known replica right as that node hit another
        // compaction-wedge restart, and neither listed node ended up with the bytes.
        // compute_required_replicas pins the target to configured RF instead (see its
        // doc comment), independent of how many nodes the chunk currently happens to
        // be on — a genuine RF<2 single-node cluster, or dual_rf's intentional 2-target
        // design, both still resolve to the right floor (1 or 2 respectively).
        let configured_rf = self.replication_factor.load(Ordering::Relaxed);
        let required_replicas = Self::compute_required_replicas(configured_rf);
        if patched_node_ids.len() < required_replicas {
            let mut missing_addrs: Vec<SocketAddr> = patch_addrs.iter().copied()
                .filter(|a| !addr_to_node_id_snap.get(a)
                    .map(|nid| patched_node_ids.contains(nid))
                    .unwrap_or(false))
                .collect();
            // Always queue at least one genuinely new candidate alongside the
            // originally-targeted node(s) — not just when patch_addrs itself was too
            // small (the af931c98 case), but on every backfill. Root-caused
            // 2026-07-17 via T52 (SIGKILL a replica mid-patch-storm, poll replica
            // count live): with only the originally-targeted node in missing_addrs,
            // the retry loop below has nothing else to try if that ONE node is
            // genuinely down — it just waits out the full BACKFILL_RETRY_BUDGET
            // hoping the same node comes back, exactly the "wait for it" behavior the
            // user explicitly said not to do ("skip that node and patch a different
            // node with the full bytes that they need"). A fresh candidate costs
            // nothing extra to have queued if it turns out not to be needed (the
          // round-robin loop below only uses as many as required_replicas actually
            // needs) — health-sorted, excluding anyone already targeted, reusing the
            // same ordering write_chunk_to_replicas uses for its own candidate
            // fallback (client.rs:5457).
            {
                let exclude: std::collections::HashSet<SocketAddr> =
                    patch_addrs.iter().copied().chain(missing_addrs.iter().copied()).collect();
                let needed = required_replicas.saturating_sub(patched_node_ids.len()).max(1);
                let extra_candidates = self.node_health.sort_by_health(
                    &all_cluster_nodes.iter().copied()
                        .filter(|a| !exclude.contains(a))
                        .collect::<Vec<_>>()
                ).await;
                missing_addrs.extend(extra_candidates.into_iter().take(needed));
            }
            warn!("MultiPatch: chunk {} landed on only {}/{} required replica(s) — backfilling the \
                   missing {} with a direct raw copy before accepting the write",
                authoritative_chunk_id, patched_node_ids.len(), required_replicas, missing_addrs.len());

            // Fetch the actual final bytes once and push them directly (WriteChunk, not
            // MultiPatch) to each missing node — NOT a re-send of the original `patches`
            // against `old_chunk_id`. On a hot chunk (exactly the VM-install workload this
            // exists for) a missing node's real on-disk state has very often already moved
            // past old_chunk_id by the time a retry lands (a newer patch from the same
            // write burst got there first), so re-applying the same delta against a base
            // that's no longer current produces a hash that legitimately disagrees with
            // authoritative_chunk_id — confirmed live 2026-07-11: this was the ORIGINAL
            // backfill design here, and it silently no-op'd (logged "REPLICA DISAGREEMENT"-
            // style and gave up) on almost every hot-chunk write during a live VM install,
            // so single-replica writes kept accumulating exactly like before this fix
            // existed. A raw copy of the already-computed final bytes has no base to agree
            // or disagree on — it's simply the same content everywhere.
            const BACKFILL_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
            const BACKFILL_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(300);
            // Share fold_concurrency's permit pool: a backfill is the same class of
            // expensive operation as ForceFold (a full 4MB chunk read plus one 4MB
            // write per missing replica), so it needs the same bound. Without this,
            // backfills under sustained hot-chunk contention have no cap at all and
            // can compound — each backfill's own writes add load to the same nodes
            // that are already missing acks, producing more under-replicated patches
            // next. Measured live 2026-07-12: backfill frequency escalated from
            // isolated singles to 5-7/sec in the last ~18s of a kdiskmark RND4K run.
            let permit_wait_start = std::time::Instant::now();
            let _fold_permit = self.fold_concurrency.acquire().await.unwrap();
            timing_fold_permit_wait += permit_wait_start.elapsed();
            let backfill_started = std::time::Instant::now();
            let source_bytes = self.read_chunk_by_id(authoritative_chunk_id, &patched_node_ids).await;
            match source_bytes {
                Ok(data) => {
                    let data = std::sync::Arc::new(data);
                    // Round-robin across ALL remaining candidates each pass, instead of
                    // exhausting the whole retry budget on one address before ever
                    // trying the next — a genuinely dead node must never starve a live
                    // candidate of a chance. See this block's doc comment above
                    // (2026-07-17, T52) for the incident this fixes: a killed replica
                    // happened to recover within the old budget and masked this, but a
                    // node that stayed down would have exhausted the full 5s against it
                    // alone with a live candidate sitting untried.
                    let mut remaining = missing_addrs;
                    while patched_node_ids.len() < required_replicas && !remaining.is_empty() {
                        let mut still_missing = Vec::new();
                        for addr in remaining {
                            if patched_node_ids.len() >= required_replicas {
                                still_missing.push(addr);
                                continue;
                            }
                            let req = Request::WriteChunk {
                                chunk_id: authoritative_chunk_id,
                                data: (*data).clone(),
                                checksum: authoritative_chunk_id.hash,
                            };
                            match self.send_request(addr, req).await {
                                Ok(Response::Ok { .. }) => {
                                    if let Some(&nid) = addr_to_node_id_snap.get(&addr) {
                                        if !patch_addrs.contains(&addr) {
                                            self.backfill_new_candidate_count.fetch_add(1, Ordering::Relaxed);
                                        }
                                        patched_node_ids.push(nid);
                                        info!("MultiPatch: backfilled chunk {} onto {} via raw copy — now {}/{} replicas",
                                            authoritative_chunk_id, addr, patched_node_ids.len(), required_replicas);
                                    }
                                }
                                Ok(other) => {
                                    warn!("MultiPatch: backfill copy to {} for chunk {} got unexpected result: {:?} — will not retry this node",
                                        addr, authoritative_chunk_id, other);
                                }
                                Err(e) => {
                                    debug!("MultiPatch: backfill copy to {} for chunk {} failed ({}) — trying other candidates before retrying it",
                                        addr, authoritative_chunk_id, e);
                                    still_missing.push(addr);
                                }
                            }
                        }
                        remaining = still_missing;
                        if !remaining.is_empty() && patched_node_ids.len() < required_replicas {
                            if backfill_started.elapsed() >= BACKFILL_RETRY_BUDGET {
                                warn!("MultiPatch: backfill for chunk {} exhausted retry budget with {} candidate(s) still unreachable: {:?}",
                                    authoritative_chunk_id, remaining.len(), remaining);
                                break;
                            }
                            tokio::time::sleep(BACKFILL_RETRY_BACKOFF).await;
                        }
                    }
                }
                Err(e) => {
                    warn!("MultiPatch: could not read back chunk {} from any of {:?} for backfill — {}",
                        authoritative_chunk_id, patched_node_ids, e);
                }
            }
            if patched_node_ids.len() < required_replicas {
                // Reaching here means every originally-targeted node AND every
                // health-sorted cluster candidate was tried and still came up short —
                // a genuine cluster-health emergency (fewer than required_replicas
                // nodes reachable at all), not a routine occurrence now that the
                // candidate pool above isn't limited to the chunk's shrunk node list.
                // Still accept the write (refusing would strand the client with no way
                // to persist durable data during a real outage) but make this loud and
                // immediately actionable rather than silently routine.
                self.single_replica_emergency_count.fetch_add(1, Ordering::Relaxed);
                error!("URGENT_SINGLE_REPLICA: chunk {} landed on only {}/{} replica(s) after \
                        exhausting all reachable candidates — cluster degraded, queuing urgent heal",
                    authoritative_chunk_id, patched_node_ids.len(), required_replicas);
                self.urgent_heal(authoritative_chunk_id).await;
                // urgent_heal alone is fire-and-forget with no confirmation it actually
                // lands — spawn a bounded background watchdog (does not delay this
                // write's own return) that keeps checking/nudging until the chunk
                // actually reaches required_replicas or a "declare it dead" deadline
                // passes. See spawn_single_replica_followup's doc comment for why 30s.
                self.spawn_single_replica_followup(authoritative_chunk_id, required_replicas);
            }
            timing_backfill += backfill_started.elapsed();
        }

        let mut new_chunk_id = authoritative_chunk_id;
        let mut new_size = new_size;

        // Active-fold timer: this client (not each server independently) now owns
        // the decision to fold a hot slot's accumulated patches into real content.
        // See Request::ForceFold's doc comment for the full root cause this closes
        // (REPLICA DISAGREEMENT from two replicas' independent per-node fold timers
        // picking different logical cut-points in an identical patch stream — fully
        // reproduced and confirmed via scripts/repro_replica_disagreement.sh).
        // Every ~8s of continuous active patching on a slot, send ONE ForceFold to
        // every replica that's currently confirmed caught-up (patched_node_ids) and
        // block here until it resolves — matches the same "wait for both before
        // proceeding" invariant the min-2-replica gate above already enforces for
        // ordinary patches, just applied to the fold decision itself.
        //
        // Tried making this a non-blocking background dispatch (2026-07-12,
        // same day): measured 7 REPLICA DISAGREEMENT instances in a 2395-patch
        // repro run, versus 0 for the synchronous version. Root cause: the two
        // ForceFold RPCs (one per replica) are sent sequentially, not in
        // parallel, so there's a real window where one replica has already
        // folded and the other hasn't received the request yet. Blocking here
        // is what keeps the write-buffer's per-slot `flushing` gate held for
        // that whole window, which is what prevents a sibling patch to this
        // exact slot from being dispatched into that window in the first
        // place — load-bearing, not just a courtesy. The actual throughput
        // regression this was chasing (see fold_concurrency's doc comment) was
        // never about blocking-vs-not; it was the semaphore being too narrow
        // and shared across *unrelated* slots.
        if let Some(cidx) = chunk_idx {
            // Hot/cold classification gate — see hot_chunk_slots's doc comment.
            // Reuses active_fold_started_at/active_fold_patch_count as the
            // classification window before a slot earns hot status (start_hot_chunk_sweeper
            // prunes these if the window ages out without reaching hot), then hands the
            // same two maps off to the existing fold-trigger logic below once it does —
            // one generation-timer pair serving both purposes, reset at the hand-off.
            if !self.hot_chunk_slots.contains(&(file_id, cidx)) {
                let now = std::time::Instant::now();
                let expired = self.active_fold_started_at.get(&(file_id, cidx))
                    .map(|t| now.duration_since(*t) > HOT_INACTIVITY_RESET)
                    .unwrap_or(false);
                if expired {
                    // Window aged out without reaching HOT_CLASSIFY_SAMPLES — either a
                    // genuine gap (nothing since the last patch) or just a slow trickle
                    // (patches kept coming, too slowly to prove "hot"). Either way, drop
                    // it and let this patch start a fresh window below.
                    self.active_fold_started_at.remove(&(file_id, cidx));
                    self.active_fold_patch_count.remove(&(file_id, cidx));
                }
                let window_start = *self.active_fold_started_at.entry((file_id, cidx)).or_insert(now);
                let window_count = {
                    let mut e = self.active_fold_patch_count.entry((file_id, cidx)).or_insert(0);
                    *e += 1;
                    *e
                };
                if window_count >= HOT_CLASSIFY_SAMPLES {
                    let elapsed_secs = now.duration_since(window_start).as_secs_f64().max(0.001);
                    if (window_count as f64 / elapsed_secs) > HOT_RATE_THRESHOLD_PER_SEC {
                        self.hot_chunk_slots.insert((file_id, cidx));
                        // Hand-off: fold-trigger generation starts fresh from here, not
                        // from whenever the classification window happened to begin.
                        self.active_fold_started_at.insert((file_id, cidx), now);
                        self.active_fold_patch_count.insert((file_id, cidx), 0);
                        self.active_fold_bytes.insert((file_id, cidx), 0);
                    } else {
                        // Didn't clear the bar — keep sampling from a fresh window rather
                        // than judging this slot forever by its first HOT_CLASSIFY_SAMPLES.
                        self.active_fold_started_at.insert((file_id, cidx), now);
                        self.active_fold_patch_count.insert((file_id, cidx), 0);
                    }
                }
            }
        }
        if let Some(cidx) = chunk_idx {
          if self.hot_chunk_slots.contains(&(file_id, cidx)) {
            // Jittered per-slot interval, not a fixed 8s: a kdiskmark-style workload
            // starts writing many different chunks within the same brief window, so
            // a fixed interval means every one of them matures in lockstep waves
            // every ~8s, forever (each generation resets its own timer to "now",
            // re-synchronizing the next wave too). Measured live 2026-07-12: up to
            // 37 ForceFold maturities landing in a single one-second window against
            // fold_concurrency's 16-permit pool — real queueing, not just isolated
            // hot-chunk cost. A deterministic per-(file_id, chunk_idx) jitter spreads
            // different slots' maturity times across a 6-10s window instead of a
            // single instant, breaking the resonance instead of just widening the
            // pool to chase whatever burst size this workload happens to produce.
            let active_fold_interval = {
                use std::collections::hash_map::DefaultHasher;
                use std::hash::{Hash, Hasher};
                let mut hasher = DefaultHasher::new();
                (file_id, cidx).hash(&mut hasher);
                let jitter_ms = (hasher.finish() % 4000) as u64; // 0..4000ms
                // DFS_ACTIVE_FOLD_BASE_SECS: override for local experiments only —
                // measuring how much of the size-dependent write collapse is
                // fold-volume-driven (many lightly-patched, mostly-cold chunks
                // each paying a full fold) vs. real write-path cost, by testing
                // with this raised high enough that the time trigger effectively
                // never fires within a short test window. Default (6) is
                // unchanged production behavior.
                static BASE_SECS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
                let base_secs = *BASE_SECS.get_or_init(|| {
                    std::env::var("DFS_ACTIVE_FOLD_BASE_SECS")
                        .ok()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(6)
                });
                std::time::Duration::from_secs(base_secs) + std::time::Duration::from_millis(jitter_ms)
            };
            // Size-based early trigger, alongside the time-based one above — see
            // active_fold_bytes's doc comment for why an unbounded accumulator is a
            // real problem this closes, and why it's safe to add here (same single
            // client-side decision-maker, same identical-to-every-replica delivery).
            // 8MB = 2x the real 4MB chunk size: generous enough that normal write
            // patterns still coalesce many small patches per fold (the whole point
            // of debouncing), tight enough to bound worst-case accumulator growth to
            // a small constant multiple of a real chunk instead of unbounded.
            const ACTIVE_FOLD_SIZE_THRESHOLD: usize = 8 * 1024 * 1024;
            let this_patch_bytes: usize = patches.iter().map(|(_, d)| d.len()).sum();
            let accumulated_bytes = {
                let mut entry = self.active_fold_bytes.entry((file_id, cidx)).or_insert(0);
                *entry += this_patch_bytes;
                *entry
            };
            // Patch-count trigger, alongside time and bytes above — added
            // 2026-07-13, independently of active_fold_bytes, ties fold
            // frequency to actual work done (patch count) rather than only
            // time or only payload size; each dimension catches a workload
            // shape the others miss (many tiny patches vs. few large ones vs.
            // a slow trickle). Same single-decision-maker safety as the other
            // two — no REPLICA DISAGREEMENT risk. Sized against real observed
            // per-chunk touch rate (~4/s): at 4/s, ~24-40 patches accumulate
            // within the jittered 6-10s time window, so a threshold well
            // below that (20) lets the count trigger occasionally preempt
            // time/bytes during busier moments while still comfortably
            // clearing the "no more than ~1 fold/sec" floor.
            const ACTIVE_FOLD_PATCH_THRESHOLD: u64 = 20; // was 30, then 50, 100, 500, 1000

            let elapsed_since_start = self.active_fold_started_at.get(&(file_id, cidx)).map(|e| e.elapsed());
            let patch_count = {
                let mut entry = self.active_fold_patch_count.entry((file_id, cidx)).or_insert(0);
                *entry += 1;
                *entry
            };
            match elapsed_since_start {
                None => {
                    // First patch this client has observed on this slot's current
                    // accumulator — seed the timer, nothing to fold yet.
                    self.active_fold_started_at.insert((file_id, cidx), std::time::Instant::now());
                }
                Some(elapsed) if elapsed >= active_fold_interval
                    || accumulated_bytes >= ACTIVE_FOLD_SIZE_THRESHOLD
                    || patch_count >= ACTIVE_FOLD_PATCH_THRESHOLD => {
                    // Backoff after a recent failed attempt on this exact slot — see
                    // active_fold_failure_backoff's doc comment for the retry storm
                    // this closes. Skip this round entirely (no permit acquired, no
                    // RPCs sent) while within the backoff window; a later patch to
                    // this slot will retry once it's elapsed. Base 2s doubling per
                    // consecutive failure, capped at 30s — short relative to
                    // heal_push_failure's healing-scale backoff (minutes), since a
                    // fold retry needs to stay responsive once things recover, not
                    // just stop hot-looping while they're bad.
                    const FOLD_FAILURE_BACKOFF_BASE: std::time::Duration = std::time::Duration::from_secs(2);
                    const FOLD_FAILURE_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(30);
                    let in_backoff = self.active_fold_failure_backoff.get(&(file_id, cidx))
                        .map(|entry| {
                            let (last_failure, count) = *entry;
                            let backoff = FOLD_FAILURE_BACKOFF_BASE
                                .saturating_mul(1u32 << count.min(4))
                                .min(FOLD_FAILURE_BACKOFF_CAP);
                            last_failure.elapsed() < backoff
                        })
                        .unwrap_or(false);
                    if !in_backoff {
                    // Cap (not eliminate) fold concurrency across every slot on
                    // this client — see fold_concurrency's doc comment. Wide on
                    // purpose: this must stay well above kdiskmark-style burst
                    // concurrency (Q32T1 = 32) or unrelated chunks' writes start
                    // queuing behind each other through this shared permit pool.
                    let permit_wait_start = std::time::Instant::now();
                    let _fold_permit = self.fold_concurrency.acquire().await.unwrap();
                    timing_fold_permit_wait += permit_wait_start.elapsed();
                    let fold_rpc_start = std::time::Instant::now();
                    let fold_targets: Vec<SocketAddr> = patch_addrs.iter().copied()
                        .filter(|a| addr_to_node_id_snap.get(a)
                            .map(|nid| patched_node_ids.contains(nid))
                            .unwrap_or(false))
                        .collect();
                    let mut folded: Option<(ChunkId, usize)> = None;
                    let mut fold_ok = !fold_targets.is_empty();
                    // Dispatch to every target concurrently, not one-at-a-time: a
                    // sequential loop leaves a real window where one replica has
                    // already advanced to the new generation (and deleted its own
                    // copy of the old chunk as part of that) while another replica
                    // hasn't even received the request yet — a read or a later
                    // patch landing on the lagging replica during that window can
                    // resolve to a chunk_id that's already gone on the replica that
                    // raced ahead. Firing all requests together shrinks that window
                    // to however long the RPCs actually take to process, not however
                    // long they take to be dispatched one after another. Still
                    // blocks here until every target resolves — same overall
                    // synchronous-fold invariant as before, only the fan-out itself
                    // is parallel.
                    let fold_requests: Vec<_> = fold_targets.iter().map(|&addr| {
                        let req = Request::ForceFold { file_id, chunk_idx: cidx };
                        async move { (addr, self.send_request(addr, req).await) }
                    }).collect();
                    let fold_responses = futures::future::join_all(fold_requests).await;
                    for (addr, response) in fold_responses {
                        match response {
                            Ok(Response::ForceFoldResult { real_chunk_id, size }) => {
                                match folded {
                                    None => folded = Some((real_chunk_id, size)),
                                    Some((prior_id, _)) if prior_id != real_chunk_id => {
                                        warn!("ForceFold: {} disagrees with prior target on result for file {} chunk {} ({} vs {}) — skipping this fold, next patch will retry",
                                            addr, file_id, cidx, real_chunk_id, prior_id);
                                        fold_ok = false;
                                    }
                                    Some(_) => {}
                                }
                            }
                            Ok(other) => {
                                warn!("ForceFold: {} for file {} chunk {} got unexpected result: {:?}",
                                    addr, file_id, cidx, other);
                                fold_ok = false;
                            }
                            Err(e) => {
                                warn!("ForceFold: {} for file {} chunk {} failed: {}", addr, file_id, cidx, e);
                                fold_ok = false;
                            }
                        }
                    }
                    if fold_ok {
                        if let Some((real_chunk_id, size)) = folded {
                            let trigger = if patch_count >= ACTIVE_FOLD_PATCH_THRESHOLD { "count" }
                                else if accumulated_bytes >= ACTIVE_FOLD_SIZE_THRESHOLD { "bytes" }
                                else { "time" };
                            info!("ForceFold: file {} chunk {} folded {} -> {} after {:?} / {} patches / {} accumulated bytes of active patching (trigger={})",
                                file_id, cidx, new_chunk_id, real_chunk_id, elapsed, patch_count, accumulated_bytes, trigger);
                            new_chunk_id = real_chunk_id;
                            new_size = size;
                        }
                        // New generation starts accumulating from here.
                        self.active_fold_started_at.insert((file_id, cidx), std::time::Instant::now());
                        self.active_fold_patch_count.insert((file_id, cidx), 0);
                        self.active_fold_bytes.insert((file_id, cidx), 0);
                        self.active_fold_failure_backoff.remove(&(file_id, cidx));
                    } else {
                        let mut entry = self.active_fold_failure_backoff.entry((file_id, cidx)).or_insert((std::time::Instant::now(), 0));
                        entry.0 = std::time::Instant::now();
                        entry.1 = entry.1.saturating_add(1);
                    }
                    // Else: leave the timer, patch count, and byte count where they
                    // are (don't reset any to now/0) so the very next patch to this
                    // slot retries the fold immediately instead of waiting out
                    // another full interval or threshold.
                    timing_fold += fold_rpc_start.elapsed();
                    }
                }
                Some(_) => {} // not due yet
            }
          }
        }

        // Use server's patch_ts if available — this ensures written_at is in server
        // time, making guard comparisons clock-agnostic. Falls back to client time
        // only when the server doesn't return a timestamp (no-op patch or old server).
        let now_secs = authoritative_patch_ts.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64
        });
        let new_location = dfs_common::ChunkLocation {
            chunk_id: new_chunk_id,
            nodes: patched_node_ids.clone(),
            size: new_size,
            checksum: new_chunk_id.hash,
            file_offset: old_location.file_offset,
            written_at: Some(now_secs),
            // Carry the file's current write_seq so the server's put_file Rule 2 merge
            // correctly identifies this patched chunk as newer than the old chunk it replaced.
            // Without this, Rule 2 sees (None, Some(old_seq)) and keeps the pre-patch chunk_id.
            client_write_seq: patch_client_write_seq,
            file_id: Some(file_id),
        };

        // Hand off to the background batch worker instead of sending+retrying inline —
        // see pending_chunk_locations's doc comment for why this is safe. The worker
        // resolves the leader itself at flush time; if it's unknown there, the batch is
        // dropped for that round, same as this code's old "skip entirely if leader_addr
        // is None right now" behavior — not a new gap, flush_metadata_sync is still the
        // authoritative backstop regardless.
        let enqueue_start = std::time::Instant::now();
        self.enqueue_chunk_location(new_location.clone()).await;
        let timing_enqueue = enqueue_start.elapsed();

        // Collect skip pairs for deferred tombstone+delete after metadata commits.
        // Tombstone is NOT sent here — sending it before metadata sync creates a read
        // blackout window: PatchChunk renames old→new on patched nodes (old gone), and
        // a premature tombstone makes the skip node return HasChunks=false too, so any
        // read during the metadata-sync window hits all nodes and fails. By deferring
        // tombstone to after flush_metadata_sync, the skip node still serves old_chunk
        // during the commit window; after metadata is updated the deferred task
        // tombstones then deletes old_chunk from the skip node cleanly.
        let skip_pairs: Vec<(SocketAddr, ChunkId)> = skip_addrs.iter()
            .map(|&addr| (addr, original_old_chunk_id))
            .collect();

        let n_patches = patches.len();
        // MPTIMING: unconditional per-op timing breakdown, added 2026-07-12 to find the
        // dominant cost under real kdiskmark-style load without guessing. `total` is
        // wall-clock for this whole call (including any stale-base retries this
        // iteration went through — rpc/backfill/fold accumulate across those too, via
        // +=). `other` is total minus every phase we explicitly measured — a large
        // `other` means time is going somewhere NOT instrumented here yet (scheduler
        // delay, an un-timed lock, CPU-bound work in the results loop, etc.) rather than
        // in the RPC/backfill/fold/enqueue calls themselves. Deliberately unconditional
        // (not gated on a slow threshold) so the full distribution can be pulled from
        // the log, per-op, not just outliers.
        let total = fn_start.elapsed();
        let measured = timing_setup + timing_rpc + timing_backfill + timing_fold_permit_wait + timing_fold + timing_enqueue;
        let other = total.saturating_sub(measured);
        info!("MPTIMING file={} chunk={:?} targets={:?} total={:?} setup={:?} rpc={:?} backfill={:?} fold_permit_wait={:?} fold_rpc={:?} enqueue={:?} other={:?}",
            file_id, chunk_idx, patch_addrs, total, timing_setup, timing_rpc, timing_backfill, timing_fold_permit_wait, timing_fold, timing_enqueue, other);
        info!("MultiPatch: {} -> {} ({} replicas, {} patches, {} skipped)",
            old_chunk_id, new_chunk_id, patched_node_ids.len(), n_patches, skip_pairs.len());
        return Ok((new_location, skip_pairs));
        } // end retry loop
        anyhow::bail!("MultiPatch: exhausted retries for chunk {}", old_chunk_id)
    }

    /// Build ChunkLocation entries from chunk_ids/sizes with file_offset tracking.
    /// replica_nodes_per_chunk provides the actual node list for each chunk (from server response).
    /// If empty or mismatched, falls back to empty node list (healing will repair later).
    fn build_chunk_locations_from_ids(
        chunk_ids: &[ChunkId],
        chunk_sizes: &[u64],
        file_offset: u64,
        replica_nodes_per_chunk: Vec<Vec<NodeId>>,
        client_write_seq: Option<u64>,
        file_id: dfs_common::FileId,
    ) -> Vec<dfs_common::ChunkLocation> {
        // written_at intentionally None for fresh writes. Using the client clock here
        // creates a timestamp that can exceed any server-side patch_ts (client ahead of
        // server). The broadcast guard (existing_ts > incoming_ts) then incorrectly
        // treats a subsequent patch as "stale" and reverts the chunk_map. None (=0)
        // ensures every server-timestamped patch result beats a fresh-write broadcast.
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
                client_write_seq,
                file_id: Some(file_id),
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
    ///
    /// `covers_from_write_seq`: pass the queue entry's coalesced lower bound when
    /// called from the metadata-queue worker/rescue paths; None for any standalone
    /// send (defaults to metadata.write_seq — i.e. "this push only accounts for
    /// itself"). See MetadataEntry::covers_from_write_seq and
    /// handle_put_file_metadata's gap check.
    pub async fn put_file_metadata_with_quorum(
        &self,
        metadata: &FileMetadata,
        _replica_nodes: Option<(SocketAddr, SocketAddr)>,
        covers_from_write_seq: Option<u64>,
    ) -> Result<()> {
        let nodes = self.cluster_nodes.read().await.clone();
        if nodes.is_empty() {
            anyhow::bail!("No cluster nodes available for metadata write");
        }

        // --- Step 1: Send to leader with time-based retry. ---
        // Uses send_to_leader_with_retry which retries for up to 15s on network failures,
        // following NotLeader redirects immediately.
        let req = Request::PutFileMetadata {
            metadata: metadata.clone(),
            covers_from_write_seq: covers_from_write_seq.unwrap_or(metadata.write_seq),
        };
        match self.send_to_leader_with_retry(req).await? {
            Response::Ok { .. } => {}
            // The push itself was accepted and persisted — this is purely an
            // additional signal that the leader is also missing earlier state for
            // this file. Mark it; flush_buffer_async's background tick sends a full
            // snapshot on its next cycle for this file. See pending_resync's doc
            // comment and Response::ResyncMetadataRequested's.
            Response::ResyncMetadataRequested { file_id } => {
                self.pending_resync.insert(file_id);
            }
            Response::Error { message, .. } => anyhow::bail!("PutFileMetadata: {}", message),
            other => anyhow::bail!("PutFileMetadata: unexpected response: {:?}", other),
        }
        let leader = self.leader_addr.read().await.unwrap_or(nodes[0]);

        // Step 2: Leader already broadcasts to all followers via broadcast_metadata_to_followers
        // and the durability catch-up queue. No client-side replica needed — sending
        // ReplicateMetadata directly to a follower caused resurrection of deleted files
        // via the follower's leader_forward_queue.

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
            covers_from_write_seq: metadata.write_seq,
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
        self.put_file_metadata_with_quorum(metadata, None, None).await
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

    /// Increment and return the next per-slot sequence number for (file_id, chunk_idx).
    /// See the `chunk_seq` field's doc comment.
    fn next_chunk_seq(&self, file_id: FileId, chunk_idx: u64) -> u64 {
        let mut entry = self.chunk_seq.entry((file_id, chunk_idx)).or_insert(0);
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

    /// Per-file cooldown on push_full_snapshot — see last_resync_sent_at's doc comment.
    /// Pure decision, no I/O: `last_sent` is this file's last_resync_sent_at entry (if
    /// any), `now` and `debounce` are passed in for testability rather than read
    /// directly from Instant::now()/a hardcoded constant.
    pub(crate) fn should_send_resync_snapshot(
        last_sent: Option<std::time::Instant>, now: std::time::Instant, debounce: Duration,
    ) -> bool {
        match last_sent {
            Some(t) => now.duration_since(t) >= debounce,
            None => true,
        }
    }

    /// The minimum replica count a patch write must reach before being accepted, given
    /// the cluster's CONFIGURED replication_factor — never derived from how many
    /// replicas a chunk currently happens to have. Root-caused 2026-07-17: the previous
    /// `patch_addrs.len().min(2)` used the chunk's CURRENT (possibly already-shrunk)
    /// known node count as the ceiling, so a chunk already down to 1 replica silently
    /// satisfied "required_replicas=1" on every subsequent patch — a self-reinforcing
    /// trap where a chunk that lost a replica never got it back. This directly caused
    /// real, confirmed, unrecoverable data loss on a live VM disk install (chunk
    /// af931c98..., 2026-07-17): a patch landed on the chunk's one known replica right
    /// as that node hit a compaction-wedge restart, and neither listed node ended up
    /// with the bytes on disk. Pinning the target to configured RF (floored at 2, unless
    /// RF itself is below 2) means every patch always tries to reach 2 replicas
    /// regardless of the chunk's current state.
    fn compute_required_replicas(configured_rf: usize) -> usize {
        if configured_rf >= 2 { 2 } else { configured_rf.max(1) }
    }

    /// Ask the leader to heal `chunk_id` immediately, bypassing healing_delay_secs —
    /// the backstop for when a patch write genuinely can't reach required_replicas
    /// even after exhausting every reachable candidate (see
    /// multi_patch_chunk_on_replicas_inner's "still under-replicated" fallback). Reuses
    /// the existing Request::QueueChunksForHealing -> HealingManager::queue_chunks_immediate
    /// path (already used for this exact purpose elsewhere) — no new server-side code.
    /// Fire-and-forget: never blocks or fails the caller's write on this, since the
    /// write itself already has to proceed (refusing to acknowledge a durable patch
    /// during a genuine cluster-health emergency would strand the client entirely).
    async fn urgent_heal(&self, chunk_id: ChunkId) {
        const URGENT_HEAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
        let target = { *self.leader_addr.read().await }
            .or_else(|| self.cluster_nodes.try_read().ok().and_then(|n| n.first().copied()));
        let Some(target) = target else {
            warn!("urgent_heal: no known node to forward chunk {} to — will only be caught by the normal (delayed) discovery pass", chunk_id);
            return;
        };
        let req = Request::QueueChunksForHealing { chunk_ids: vec![chunk_id] };
        match tokio::time::timeout(URGENT_HEAL_TIMEOUT, self.send_request(target, req)).await {
            Ok(Ok(Response::Ok { .. })) => {
                info!("urgent_heal: chunk {} queued for immediate healing via {}", chunk_id, target);
            }
            Ok(Ok(other)) => {
                warn!("urgent_heal: unexpected response queuing chunk {} via {}: {:?}", chunk_id, target, other);
            }
            Ok(Err(e)) => {
                warn!("urgent_heal: failed to queue chunk {} via {}: {}", chunk_id, target, e);
            }
            Err(_) => {
                warn!("urgent_heal: timed out queuing chunk {} via {} after {:?}", chunk_id, target, URGENT_HEAL_TIMEOUT);
            }
        }
    }

    /// Ground-truth current replica count for `chunk_id`, queried directly from the
    /// leader's CHUNK_TABLE (DebugGetRawChunkLocation — the same query dfs-admin's
    /// `file raw-location` uses) rather than a merged/cached view, since this is
    /// specifically checking whether a real backfill actually landed. Returns 0 (not
    /// an error) if the leader is unreachable or has no record — callers are polling
    /// in a loop and should just treat that as "not yet", not distinguish it from a
    /// genuine zero.
    async fn current_replica_count(&self, chunk_id: ChunkId) -> usize {
        const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
        let Some(target) = *self.leader_addr.read().await else { return 0; };
        let req = Request::DebugGetRawChunkLocation { chunk_id };
        match tokio::time::timeout(QUERY_TIMEOUT, self.send_request(target, req)).await {
            Ok(Ok(Response::DebugRawChunkLocation { location: Some(loc) })) => loc.nodes.len(),
            _ => 0,
        }
    }

    /// Bounded end-to-end follow-up for the true emergency case (multi_patch's
    /// backfill exhausted every reachable candidate within its own 5s
    /// BACKFILL_RETRY_BUDGET and still came up short of required_replicas).
    /// urgent_heal alone is fire-and-forget — it hands the chunk to the leader's
    /// (now priority-sorted, per drain_heal_queue's severity ordering) healer with no
    /// confirmation it actually lands. Spawned as a background task so it never
    /// blocks or delays the write's own return; the write already had to proceed
    /// with whatever replica count it achieved (see this fallback's call site).
    ///
    /// SINGLE_REPLICA_FOLLOWUP_DEADLINE=30s deliberately mirrors the classic OS/
    /// kernel block-layer "declare a disk dead" command-timeout convention (e.g.
    /// the Linux SCSI layer's default) — user's explicit framing when this was
    /// added 2026-07-17: this is exactly the same class of question ("how long
    /// before we stop hoping and call it actually broken"), so borrowing an
    /// already-battle-tested value beats picking an arbitrary one.
    fn spawn_single_replica_followup(&self, chunk_id: ChunkId, required_replicas: usize) {
        const FOLLOWUP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
        const FOLLOWUP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);
        let client = self.clone();
        tokio::spawn(async move {
            let start = std::time::Instant::now();
            loop {
                tokio::time::sleep(FOLLOWUP_POLL_INTERVAL).await;
                let current = client.current_replica_count(chunk_id).await;
                if current >= required_replicas {
                    info!("urgent_heal: chunk {} reached {}/{} replicas via background healing after {:.1}s follow-up",
                        chunk_id, current, required_replicas, start.elapsed().as_secs_f64());
                    return;
                }
                if start.elapsed() >= FOLLOWUP_DEADLINE {
                    client.single_replica_followup_exhausted_count.fetch_add(1, Ordering::Relaxed);
                    error!("URGENT_SINGLE_REPLICA_FOLLOWUP_EXHAUSTED: chunk {} still only {}/{} replicas after \
                            the full {:.0}s follow-up window — cluster-health emergency did not self-resolve, \
                            needs operator attention",
                        chunk_id, current, required_replicas, FOLLOWUP_DEADLINE.as_secs_f64());
                    return;
                }
                // Nudge again in case the first request never landed (leader change,
                // transient network failure) — queue_chunks_immediate is idempotent.
                client.urgent_heal(chunk_id).await;
            }
        });
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
                        self.put_file_metadata_with_quorum(
                            &stalled.metadata, None, Some(stalled.covers_from_write_seq),
                        ),
                    ).await.ok().and_then(|r| r.ok()).is_some();

                    if delivered {
                        debug!("metadata_queue: rescue delivered {} via quorum", stalled.metadata.path);
                        if let Some(tx) = stalled.done_tx { let _ = tx.send(()); }
                    } else {
                        // Attempt 2: fan out to all nodes in parallel, take first success.
                        *self.leader_addr.write().await = None;
                        let nodes = self.cluster_nodes.read().await.clone();
                        let meta = stalled.metadata.clone();
                        let covers_from_write_seq = stalled.covers_from_write_seq;
                        let rescued = if !nodes.is_empty() {
                            let futs: Vec<_> = nodes.iter().map(|&addr| {
                                let client = self.clone();
                                let m = meta.clone();
                                async move {
                                    let req = Request::PutFileMetadata { metadata: m, covers_from_write_seq };
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
        {
            let chunk0_size = metadata.chunk_locations.iter().find(|l| l.file_offset.unwrap_or(0) == 0).map(|l| l.size);
            debug!("[SIZE TRACE] flush_metadata_sync path={} id={} seq={} chunks={} chunk0_size={:?} pending_chunk_locations_for_this_file={}",
                metadata.path, metadata.id, metadata.write_seq, metadata.chunk_locations.len(), chunk0_size,
                self.pending_chunk_locations.lock().await.keys().filter(|(fid, _)| *fid == Some(metadata.id)).count());
        }
        // Drain and synchronously send this file's own pending chunk-location
        // notifications first. Without this, fsync/release could return
        // (metadata_queue's push_and_wait below confirmed) while some chunk-location
        // updates from this same flush cycle are still sitting in
        // pending_chunk_locations, not yet sent — the background batch worker only
        // flushes on its own ~10ms tick. The underlying chunk data is already safely
        // replicated regardless of this notification's timing, but a read shortly
        // after this call returns can still race ahead of the background worker and
        // see the leader's pre-patch chunk_map/CHUNK_TABLE state if that read needs to
        // consult the leader rather than relying entirely on already-patched local
        // state. This closes that window at the one point every flush path already
        // calls to confirm metadata is fully committed, instead of needing a drain
        // call at each of flush_metadata_sync's ~10 call sites individually.
        //
        // Scoped to this file_id specifically — NOT the whole queue. flush_all_pipelined
        // (called just before this, at the end of every flush cycle) already waits for
        // all of this file's own concurrent patches to land, so waiting on their
        // location notifications too is bounded by work this caller is already doing.
        // Draining indiscriminately would instead make a fast caller (e.g. closing a
        // small file) wait on unrelated notifications queued by a completely different
        // file's concurrent flush — real head-of-line blocking introduced by batching,
        // not present in the old one-RPC-per-patch design. Anything left over (other
        // files' pending locations) stays queued for the background worker's next tick.
        let batch: Vec<dfs_common::ChunkLocation> = {
            let mut pending = self.pending_chunk_locations.lock().await;
            let taken = std::mem::take(&mut *pending);
            let (mine, rest): (HashMap<_, _>, HashMap<_, _>) = taken
                .into_iter()
                .partition(|(k, _)| k.0 == Some(metadata.id));
            *pending = rest;
            mine.into_values().collect()
        };
        if !batch.is_empty() {
            let leader_addr = *self.leader_addr.read().await;
            match leader_addr {
                Some(leader) => {
                    let count = batch.len();
                    // Same short-timeout treatment as WriteChunk's fresh-write call site
                    // above — this drain is a latency optimization, not a correctness
                    // requirement (flush_metadata_sync's own PutFileMetadata call right
                    // after this delivers the authoritative state regardless), so don't
                    // block this flush for send_chunk_locations_batched's up-to-~15s
                    // internal retry when the re-queue fallback already handles failure.
                    let result = tokio::time::timeout(
                        Duration::from_secs(1),
                        self.send_chunk_locations_batched(leader, batch.clone()),
                    ).await;
                    let failed = match result {
                        Ok(Ok(())) => false,
                        Ok(Err(e)) => {
                            warn!("flush_metadata_sync: draining {} pending chunk locations failed: {} — re-queuing for the background worker", count, e);
                            true
                        }
                        Err(_) => {
                            warn!("flush_metadata_sync: draining {} pending chunk locations took >1s — re-queuing for the background worker instead of blocking this flush", count);
                            true
                        }
                    };
                    if failed {
                        // Re-queue rather than drop — see start_chunk_location_batch_worker's
                        // identical fix for the full rationale (a chunk's data write already
                        // completed by the time it lands here; dropping the location
                        // registration orphans real, already-durable bytes).
                        let mut pending = self.pending_chunk_locations.lock().await;
                        for loc in batch {
                            upsert_chunk_location(&mut pending, loc);
                        }
                    }
                }
                None => {
                    warn!("flush_metadata_sync: no known leader — re-queuing {} pending chunk locations for the background worker", batch.len());
                    let mut pending = self.pending_chunk_locations.lock().await;
                    for loc in batch {
                        upsert_chunk_location(&mut pending, loc);
                    }
                }
            }
        }

        let stamped = self.stamp_write_seq(metadata);
        self.metadata_queue.push_and_wait(stamped).await;
    }

    /// Spawn the background chunk-location batch drain task onto the given runtime.
    /// Must be called once after construction. Wakes on a short fixed interval,
    /// drains whatever has accumulated in pending_chunk_locations, and sends it as one
    /// batched ReplicateChunkLocations RPC to the current leader. 10ms is short enough
    /// not to add meaningfully to per-chunk latency, long enough to coalesce the
    /// concurrent burst flush_all_pipelined's dispatched flush_one_chunk tasks produce
    /// (that's the actual VM-fsync-driven case this exists for).
    pub fn start_chunk_location_batch_worker(&self, runtime: &tokio::runtime::Handle) {
        let client = self.clone();
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(10));
            loop {
                interval.tick().await;
                let mut batch: Vec<dfs_common::ChunkLocation> = {
                    let mut pending = client.pending_chunk_locations.lock().await;
                    if pending.is_empty() {
                        continue;
                    }
                    std::mem::take(&mut *pending).into_values().collect()
                };
                // Resolved fresh here, not at enqueue time — uses the most current
                // leader knowledge for whatever accumulated since the last tick.
                let leader_addr = *client.leader_addr.read().await;
                match leader_addr {
                    Some(leader) => {
                        let count = batch.len();
                        if let Err(e) = client.send_chunk_locations_batched(leader, batch.clone()).await {
                            warn!("chunk_location_batch_worker: batched send of {} locations to leader {} failed: {} — re-queuing for next tick", count, leader, e);
                            // Re-queue rather than drop: a chunk's data write already
                            // completed by the time it lands here (this batch only ever
                            // carries post-write location registrations) — dropping this
                            // silently orphans real, already-durable bytes with nothing
                            // in metadata ever pointing to them again. Spliced ahead of
                            // (not appended after) whatever accumulated during this send,
                            // O(n) via a single swap rather than O(n^2) per-element
                            // inserts, so this generation drains before newer ones pile
                            // up behind it. Mirrors metadata_queue's retry-until-delivered
                            // posture (see start_metadata_queue_worker) instead of this
                            // worker's previous silent-drop-on-failure, confirmed as a
                            // real cause of unreachable chunks in a 2026-07-10 incident
                            // (a ~10s leader restart during active writes).
                            //
                            // Merge batch (this failed, now-stale attempt) as the base, then
                            // pending (arrived more recently, while the send was in flight) on
                            // top, so upsert_chunk_location's write_seq comparison sees pending's
                            // entries as the later call for any slot key both share — preserving
                            // "newer arrivals win over a stale failed-send retry" for the
                            // no-write-seq fallback case too, not just when write_seq differs.
                            let mut pending = client.pending_chunk_locations.lock().await;
                            let mut merged = HashMap::new();
                            for loc in batch.drain(..) {
                                upsert_chunk_location(&mut merged, loc);
                            }
                            for (_, loc) in std::mem::take(&mut *pending) {
                                upsert_chunk_location(&mut merged, loc);
                            }
                            *pending = merged;
                        }
                    }
                    None => {
                        warn!("chunk_location_batch_worker: no known leader — re-queuing {} locations for next tick", batch.len());
                        let mut pending = client.pending_chunk_locations.lock().await;
                        let mut merged = HashMap::new();
                        for loc in batch.drain(..) {
                            upsert_chunk_location(&mut merged, loc);
                        }
                        for (_, loc) in std::mem::take(&mut *pending) {
                            upsert_chunk_location(&mut merged, loc);
                        }
                        *pending = merged;
                    }
                }
            }
        });
    }

    /// Enqueue a single chunk location for the background batch worker to send,
    /// instead of sending it immediately inline. See pending_chunk_locations's doc
    /// comment for why this is safe for every current caller.
    async fn enqueue_chunk_location(&self, location: dfs_common::ChunkLocation) {
        let mut pending = self.pending_chunk_locations.lock().await;
        upsert_chunk_location(&mut pending, location);
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

                    // Second deleted_ids check: the file may have been deleted while
                    // this entry was already popped and in the worker's hands — cancel()
                    // can only remove from the queue, not from an already-popped entry.
                    if client.metadata_queue.deleted_ids.contains(&entry.metadata.id) {
                        debug!("[META QUEUE] skip-deleted (post-pop) id={} path={}",
                            entry.metadata.id, entry.metadata.path);
                        if let Some(tx) = entry.done_tx { let _ = tx.send(()); }
                        continue;
                    }

                    // Retry until the leader confirms. Each failure re-identifies the leader.
                    // Each attempt is capped at 5s so a saturated/slow leader can't block
                    // this worker (and any release() waiter) for 30s+ per attempt.
                    let mut attempts = 0u32;
                    loop {
                        let result = tokio::time::timeout(
                            Duration::from_secs(5),
                            client.put_file_metadata_with_quorum(
                                &entry.metadata, None, Some(entry.covers_from_write_seq),
                            ),
                        ).await;
                        match result {
                            Ok(Ok(())) => {
                                info!(
                                    "[META QUEUE] delivered path={} id={} seq={} size={}",
                                    entry.metadata.path, entry.metadata.id,
                                    entry.metadata.write_seq, entry.metadata.size
                                );
                                client.metadata_queue.mark_delivered(entry.metadata.id, entry.metadata.write_seq);
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
                                // 5s deadline exceeded without the inner call itself reporting
                                // an error — put_file_metadata_with_quorum's own
                                // send_to_leader_with_retry already has a 15s budget and already
                                // clears/rediscovers leader_addr on genuine network errors or
                                // NotLeader redirects, so hitting this branch means the leader
                                // was just slow (busy under concurrent load), not gone.
                                // Deliberately do NOT clear leader_addr here: that field is
                                // shared by every concurrent write path (WriteChunk,
                                // flush_metadata_sync, chunk_location_batch_worker, ...), and
                                // clearing it on a single worker's impatience — with no evidence
                                // the leader actually changed — forced every other in-flight
                                // caller into a redundant "no known leader" rediscovery storm.
                                // Measured live 2026-07-12: one such timeout produced 2428 "no
                                // known leader" warnings over the next 23s during a kdiskmark
                                // run, while the leader's own server log showed it never
                                // stopped processing puts. Just retry; if the leader really is
                                // down, the next attempt's send_to_leader_with_retry will hit a
                                // real error and clear the cache itself.
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
        // Same lifecycle event as metadata_queue's own delivered_write_seq cleanup —
        // this file_id is a UUID, never reused, so these entries are now dead weight.
        self.pending_resync.remove(&file_id);
        self.last_resync_sent_at.remove(&file_id);
    }

    /// Delete file
    pub async fn delete_file(&self, path: &str) -> Result<()> {
        // Fan out to quorum (leader + 2 other online nodes) simultaneously.
        // Each recipient enqueues the deletion durably to sled and wipes local
        // metadata before acking — so the file disappears from the namespace
        // on all 3 nodes before this call returns. Chunk cleanup happens
        // asynchronously via the leader's drain worker; it never blocks the client.
        //
        // If the leader is down, pick any 3 online nodes. The new leader will
        // poll all nodes for their delete queues on election and resume the drain.
        let request = Request::DeleteFile { path: path.to_string() };

        let all_nodes = self.cluster_nodes.read().await.clone();
        let leader = *self.leader_addr.read().await;

        // Build quorum: leader first, then up to 2 healthy others.
        // Skip penalized nodes so a dead node doesn't force a 10s timeout per delete.
        let mut quorum: Vec<SocketAddr> = Vec::with_capacity(3);
        if let Some(leader_addr) = leader {
            if !self.node_health.is_penalized(leader_addr).await {
                quorum.push(leader_addr);
            }
        }
        for &addr in &all_nodes {
            if quorum.len() >= 3 { break; }
            if !quorum.contains(&addr) && !self.node_health.is_penalized(addr).await {
                quorum.push(addr);
            }
        }
        // Fallback: if all known nodes are penalized, try anyway (better than failing fast).
        if quorum.is_empty() {
            if let Some(leader_addr) = leader { quorum.push(leader_addr); }
            for &addr in &all_nodes {
                if quorum.len() >= 3 { break; }
                if !quorum.contains(&addr) { quorum.push(addr); }
            }
        }
        if quorum.is_empty() {
            anyhow::bail!("No nodes available for delete");
        }

        // Fire all quorum RPCs concurrently via independent tasks.
        // Each task gets its own Arc clone so no lifetime issues.
        // The leader handle is always first (quorum[0]). We wait for it specifically
        // because the leader is the one that durably enqueues the delete.  The other
        // two handles run in the background for redundancy but we don't block on them:
        // if the leader acks, the delete is durable regardless of the others.
        let leader_addr = quorum[0];
        let mut handles: Vec<_> = quorum.iter().map(|&addr| {
            let req = request.clone();
            let this = self.clone();
            tokio::spawn(async move {
                (addr, tokio::time::timeout(
                    tokio::time::Duration::from_secs(10),
                    this.send_request(addr, req),
                ).await)
            })
        }).collect();

        let mut not_found_count = 0usize;
        let mut success_count = 0usize;
        let quorum_len = quorum.len();
        let mut leader_not_found = false;
        for h in &mut handles {
            let join_result = h.await;
            let (addr, timeout_result) = match join_result {
                Ok(v) => v,
                Err(e) => { warn!("delete_file: task panicked for {}: {}", path, e); continue; }
            };
            let is_leader = addr == leader_addr;
            match timeout_result {
                Ok(Ok(Response::Ok { .. })) => {
                    success_count += 1;
                    // Leader acked — delete is durably queued. Return without
                    // waiting for the background follower RPCs.
                    if is_leader { return Ok(()); }
                }
                Ok(Ok(Response::Error { code: ErrorCode::NotFound, .. })) => {
                    not_found_count += 1;
                    success_count += 1;
                    if is_leader { leader_not_found = true; }
                }
                Ok(Ok(Response::Error { message, .. })) => {
                    warn!("delete_file: node returned error for {}: {}", path, message);
                }
                Ok(Ok(_)) => { warn!("delete_file: unexpected response for {}", path); }
                Ok(Err(e)) => { warn!("delete_file: RPC failed for {}: {}", path, e); }
                Err(_) => { warn!("delete_file: RPC timed out for {}", path); }
            }
        }

        if leader_not_found || not_found_count == quorum_len {
            // Every node said NotFound — file was already gone.
            return Ok(());
        }
        if success_count == 0 {
            anyhow::bail!("delete_file: all quorum nodes failed for {}", path);
        }

        Ok(())
    }

    /// Purge file metadata without deleting chunks (for rename operations)
    /// This only removes the metadata entry, preserving chunk data
    pub async fn purge_file_metadata(&self, path: &str) -> Result<()> {
        let req = Request::PurgeFileMetadata { path: path.to_string() };
        match self.send_to_leader_with_retry(req).await? {
            Response::Ok { .. } => Ok(()),
            Response::Error { message, .. } => anyhow::bail!("purge_file_metadata: {}", message),
            _ => anyhow::bail!("purge_file_metadata: unexpected response"),
        }
    }

    /// Rename file atomically (server-side atomic operation)
    /// This is safer than separate put + purge operations as it prevents
    /// race conditions where the file disappears during rename
    pub async fn rename_file(&self, old_path: &str, new_path: &str) -> Result<()> {
        // Route to the leader — followers may not have received the file metadata
        // yet if the broadcast flush window hasn't fired since the last PutFileMetadata.
        let req = Request::RenameFile {
            old_path: old_path.to_string(),
            new_path: new_path.to_string(),
        };
        match self.send_to_leader_with_retry(req).await? {
            Response::Ok { .. } => return Ok(()),
            Response::Error { message, .. } => anyhow::bail!("rename_file: {}", message),
            _ => anyhow::bail!("rename_file: unexpected response"),
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
                                    if node.total_bytes > 0 {
                                        self.node_capacities.insert(node.addr, (node.available_bytes, node.total_bytes));
                                    }
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
        let usable_available = dfs_common::calculate_usable_capacity(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_id_with_hash0(b: u8) -> ChunkId {
        let mut hash = [0u8; 32];
        hash[0] = b;
        ChunkId::from_hash(hash)
    }

    fn loc_with_nodes(chunk_id: ChunkId, nodes: Vec<dfs_common::NodeId>) -> ChunkLocation {
        ChunkLocation {
            chunk_id,
            nodes,
            size: 4 * 1024 * 1024,
            checksum: chunk_id.hash,
            file_offset: Some(0),
            written_at: None,
            client_write_seq: None,
            file_id: None,
        }
    }

    fn loc_at(chunk_id: ChunkId, file_id: dfs_common::FileId, file_offset: u64, write_seq: Option<u64>) -> ChunkLocation {
        ChunkLocation {
            chunk_id,
            nodes: vec![],
            size: 4 * 1024 * 1024,
            checksum: chunk_id.hash,
            file_offset: Some(file_offset),
            written_at: None,
            client_write_seq: write_seq,
            file_id: Some(file_id),
        }
    }

    /// An empty batch must be a no-op — no network call, no error — since callers
    /// (e.g. write_chunk_to_replicas with zero chunks, or a queue drain that found
    /// nothing pending) shouldn't need to special-case this themselves.
    #[tokio::test]
    async fn test_send_chunk_locations_batched_empty_is_noop() {
        // An unreachable address: if this were attempted as a real send, the test
        // would hang/timeout instead of returning immediately.
        let unreachable: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let client = DfsClient::new(vec![unreachable]).unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            client.send_chunk_locations_batched(unreachable, vec![]),
        ).await;
        assert!(result.is_ok(), "empty batch must return immediately, not attempt a send");
        assert!(result.unwrap().is_ok(), "empty batch must return Ok(())");
    }

    /// Concurrent callers (mirroring the concurrent flush_one_chunk tasks one
    /// flush_all_pipelined call dispatches) targeting 20 DISTINCT chunk slots must all
    /// land in the shared queue before any send happens, rather than each firing its
    /// own RPC immediately or losing entries under concurrent locking — that
    /// accumulation is the actual coalescing opportunity the background drain worker
    /// (start_chunk_location_batch_worker) acts on. Doesn't start that worker, so this
    /// deterministically captures the queue's behavior in isolation, with no race
    /// against an active drain. Each location gets its own file_offset specifically so
    /// this test isn't also exercising upsert_chunk_location's same-slot dedup — see
    /// test_enqueue_chunk_location_dedups_same_slot for that.
    #[tokio::test]
    async fn test_enqueue_chunk_location_coalesces_concurrent_callers() {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let client = DfsClient::new(vec![addr]).unwrap();
        let file_id = dfs_common::FileId::new();

        let mut handles = Vec::new();
        for i in 0..20u8 {
            let client = client.clone();
            let chunk_id = chunk_id_with_hash0(i);
            handles.push(tokio::spawn(async move {
                client.enqueue_chunk_location(loc_at(chunk_id, file_id, (i as u64) * 4 * 1024 * 1024, None)).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let pending = client.pending_chunk_locations.lock().await;
        assert_eq!(pending.len(), 20, "20 concurrent enqueues to 20 distinct chunk slots should all land in the shared queue");
    }

    /// Direct regression test for the 2026-07-21 staging finding: N patches to the
    /// SAME chunk slot (same file_id + file_offset, different chunk_id per patch since
    /// chunk_id = blake3(file_id||file_offset||data)) landing before the batch worker's
    /// next drain must collapse to exactly one entry — the freshest one, by
    /// client_write_seq — not ride the batch as N separate redundant entries.
    #[tokio::test]
    async fn test_enqueue_chunk_location_dedups_same_slot() {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let client = DfsClient::new(vec![addr]).unwrap();
        let file_id = dfs_common::FileId::new();

        // Enqueue 5 patches to the same slot, out of write_seq order, to prove the
        // dedup picks the highest write_seq rather than just the last call.
        let seqs = [3u64, 1, 5, 2, 4];
        let mut expected_winner = None;
        for &seq in &seqs {
            let chunk_id = chunk_id_with_hash0(seq as u8);
            if expected_winner.is_none() || seq == 5 {
                expected_winner = Some(chunk_id);
            }
            client.enqueue_chunk_location(loc_at(chunk_id, file_id, 0, Some(seq))).await;
        }

        let pending = client.pending_chunk_locations.lock().await;
        assert_eq!(pending.len(), 1, "5 patches to the same slot must collapse to 1 entry, got {}", pending.len());
        let surviving = pending.values().next().unwrap();
        assert_eq!(surviving.client_write_seq, Some(5), "the highest write_seq (5) must win regardless of arrival order");
        assert_eq!(surviving.chunk_id, expected_winner.unwrap(), "the surviving entry must be the one carrying write_seq 5");
    }

    /// Classification guard for the 2026-07-21 write-corruption fix. A slow-but-alive
    /// leader (the corruption trigger) returns a *timeout* error, not a connect error;
    /// before the fix the MultiPatch retry loop only treated "Failed to connect" as
    /// transient, so a merely-overloaded leader failed the whole patch instantly and
    /// fell through to the lossy "all replicas failed" fallback that drops the write.
    /// Timeouts and connection-drops must count as transient (ride out via
    /// CONNECT_RETRY_BUDGET); genuine server-side data errors must NOT.
    #[test]
    fn is_transient_write_failure_matches_timeouts_not_data_errors() {
        // Slow / black-holing leader — the exact case that was silently dropping writes:
        assert!(is_transient_write_failure("Timeout reading chunk from 10.0.0.1:8900"));
        assert!(is_transient_write_failure("Timeout writing to 10.0.0.1:8900"));
        // Node restart / connection drop (CONNECT_RETRY_BUDGET's documented purpose):
        assert!(is_transient_write_failure("Failed to connect to node: connect timeout"));
        assert!(is_transient_write_failure("Failed to connect to node 10.0.0.1:8900: connection refused"));
        assert!(is_transient_write_failure("Connect timeout to 10.0.0.1:8900"));
        assert!(is_transient_write_failure("I/O error talking to 10.0.0.1:8900: broken pipe"));
        assert!(is_transient_write_failure("I/O error reading write response from 10.0.0.1:8900: connection reset"));
        // Real errors that must NOT be treated as a retriable connectivity blip:
        assert!(!is_transient_write_failure("chunk data missing on this node"));
        assert!(!is_transient_write_failure("unexpected response"));
        assert!(!is_transient_write_failure("chunk 224 fresh-write safety check found real existing data"));
    }

    /// Direct regression for the 2026-07-21 corruption root cause: the split-frame
    /// write path's response read was a bare read_exact().await with no timeout, so a
    /// server that received the request but never replied (overloaded leader, or a
    /// black-holing node — TCP up, never answers) hung the flush task forever, holding
    /// a global_flush_semaphore permit + chunk lock and ultimately losing the write on
    /// drain. This stands up a black-hole server (accepts, reads the request, then
    /// never responds) and asserts the call returns a bounded timeout error instead of
    /// hanging. Uses paused virtual time so the 30s bound is exercised instantly; the
    /// drive loop advances the clock in steps rather than assuming exactly when the
    /// task parks on the read. Without the fix this test hangs (the task never returns).
    #[tokio::test(start_paused = true)]
    async fn split_frame_write_response_read_is_bounded_not_infinite() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                // Consume the incoming request, then hang forever without replying.
                let mut buf = vec![0u8; 4096];
                let _ = sock.read(&mut buf).await;
                std::future::pending::<()>().await;
            }
        });

        let client = DfsClient::new(vec![addr]).unwrap();
        let env = MessageEnvelope::new(RequestId::new(1), Message::Request(Request::Ping));
        let encoded = env.to_bytes().unwrap();
        let raw = vec![0u8; 128];

        let handle = tokio::spawn(async move {
            client.send_split_frame_write_request(addr, &encoded, &raw).await
        });

        // Let real loopback I/O (connect + write) complete and the task park on the
        // response read, then advance virtual time past the 30s read bound. Stepping +
        // yielding is robust to exactly when the read-timeout's Sleep gets armed.
        for _ in 0..120 {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(1)).await;
            if handle.is_finished() {
                break;
            }
        }

        assert!(handle.is_finished(), "the call must not hang on a non-responding server");
        let result = handle.await.unwrap();
        assert!(result.is_err(), "a black-holing server must yield a bounded error, not a value");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Timeout reading chunk"),
            "expected a bounded read-timeout error, got: {}", msg);
    }

    /// Regression test for a real bug found via T48 (background-tick metadata push
    /// under full-suite load, 2026-07-07): coalescing two queued pushes for the same
    /// file must union their chunk_locations, not let the write_seq-winning side's
    /// replace silently discard whatever only the losing side described.
    ///
    /// Since 2026-07-07, flush_buffer_async sends only each cycle's newly-flushed
    /// locations (all_locations), not the full cumulative chunk history — so two
    /// pushes for the same file describe two DIFFERENT, non-overlapping deltas, not
    /// one being a superset of the other. If a background-tick push for chunk N is
    /// still queued when a later push (e.g. from release()) arrives and doesn't
    /// happen to describe chunk N, a naive replace loses chunk N forever — the
    /// server never sees a delta that was discarded client-side before ever being
    /// sent over the network.
    #[tokio::test]
    async fn test_metadata_queue_coalesce_unions_chunk_locations_not_replaces() {
        let queue = MetadataQueue::new();
        let file_id = FileId::new();

        let mut first = FileMetadata::new("/coalesce_test.bin".to_string(), dfs_common::FileType::RegularFile);
        first.id = file_id;
        first.write_seq = 1;
        first.size = 4 * 1024 * 1024;
        let loc_a = loc_with_nodes(chunk_id_with_hash0(1), vec![]);
        first.chunk_locations = Arc::new(vec![loc_a.clone()]);

        let mut second = FileMetadata::new("/coalesce_test.bin".to_string(), dfs_common::FileType::RegularFile);
        second.id = file_id;
        second.write_seq = 2;
        second.size = 8 * 1024 * 1024;
        let mut loc_b = loc_with_nodes(chunk_id_with_hash0(2), vec![]);
        loc_b.file_offset = Some(4 * 1024 * 1024); // a different offset than loc_a
        second.chunk_locations = Arc::new(vec![loc_b.clone()]);

        // Both pushed before anything drains — exactly the coalescing scenario: a
        // background-tick push (first) still queued when a later push (second,
        // e.g. release()) arrives for the same file.
        queue.push(first).await;
        queue.push(second).await;

        let entry = queue.pop().await.expect("coalesced entry must still be present");
        let offsets: std::collections::HashSet<Option<u64>> = entry.metadata.chunk_locations
            .iter().map(|l| l.file_offset).collect();
        assert!(offsets.contains(&loc_a.file_offset), "chunk from the coalesced-away first push must survive");
        assert!(offsets.contains(&loc_b.file_offset), "chunk from the winning second push must be present");
        assert_eq!(entry.metadata.chunk_locations.len(), 2, "must be a union, not just the winner's own locations");
        assert_eq!(entry.metadata.size, 8 * 1024 * 1024, "scalar fields must come from the write_seq-winning push");
    }

    /// Root-caused 2026-07-17 (staging: thousands of false-positive [META GAP] warnings
    /// on the leader during the VM 111 install). write_seq is a single per-file counter
    /// shared by two unrelated consumers: metadata-queue pushes (stamp_write_seq) and
    /// per-patch RCL ordering (patch_client_write_seq, consumed by every MultiPatch
    /// rotation — e.g. qcow2 preallocation bursts). Patches never separately enqueue a
    /// metadata push, but their resulting ChunkLocations (each carrying client_write_seq)
    /// DO end up in the very push that follows. Before this fix, covers_from_write_seq
    /// only ever reflected the push's own freshly-stamped write_seq — ignoring earlier
    /// patch-only write_seq numbers already represented in its own chunk_locations — so
    /// the leader's detect_metadata_write_seq_gap flagged a "gap" for every patch burst
    /// between two pushes, even though nothing was actually lost.
    #[tokio::test]
    async fn test_metadata_queue_covers_from_write_seq_accounts_for_patch_only_seqs() {
        let queue = MetadataQueue::new();
        let file_id = FileId::new();

        let mut meta = FileMetadata::new("/covers_from_test.bin".to_string(), dfs_common::FileType::RegularFile);
        meta.id = file_id;
        meta.write_seq = 16; // this push's own freshly-stamped write_seq
        meta.size = 4 * 1024 * 1024;
        // Chunk patched at write_seq 11 (consumed via next_write_seq for RCL ordering,
        // never separately pushed to the metadata queue) is represented in this delta.
        let mut loc = loc_with_nodes(chunk_id_with_hash0(1), vec![]);
        loc.client_write_seq = Some(11);
        meta.chunk_locations = Arc::new(vec![loc]);

        queue.push(meta).await;
        let entry = queue.pop().await.expect("entry must be present");
        assert_eq!(entry.covers_from_write_seq, 11,
            "covers_from_write_seq must account for the earliest client_write_seq actually \
             represented in this push's chunk_locations, not just the push's own write_seq stamp");
    }

    /// Same false positive, but through the coalesce path (push_inner's "existing entry"
    /// branch, line ~488) rather than the "new entry" branch — coalescing two queued
    /// pushes must widen covers_from_write_seq against BOTH sides' chunk_locations'
    /// client_write_seq, not just each push's own write_seq stamp.
    #[tokio::test]
    async fn test_metadata_queue_coalesce_covers_from_write_seq_accounts_for_patch_only_seqs() {
        let queue = MetadataQueue::new();
        let file_id = FileId::new();

        let mut first = FileMetadata::new("/coalesce_covers_from.bin".to_string(), dfs_common::FileType::RegularFile);
        first.id = file_id;
        first.write_seq = 16;
        first.size = 4 * 1024 * 1024;
        let mut loc_a = loc_with_nodes(chunk_id_with_hash0(1), vec![]);
        loc_a.client_write_seq = Some(11);
        first.chunk_locations = Arc::new(vec![loc_a]);

        let mut second = FileMetadata::new("/coalesce_covers_from.bin".to_string(), dfs_common::FileType::RegularFile);
        second.id = file_id;
        second.write_seq = 41;
        second.size = 8 * 1024 * 1024;
        let mut loc_b = loc_with_nodes(chunk_id_with_hash0(2), vec![]);
        loc_b.file_offset = Some(4 * 1024 * 1024);
        loc_b.client_write_seq = Some(17);
        second.chunk_locations = Arc::new(vec![loc_b]);

        queue.push(first).await;
        queue.push(second).await;

        let entry = queue.pop().await.expect("coalesced entry must be present");
        assert_eq!(entry.covers_from_write_seq, 11,
            "coalesced covers_from_write_seq must still reflect the earliest client_write_seq \
             across BOTH sides' chunk_locations, not just each push's own write_seq stamp");
    }

    /// Root-caused 2026-07-17, second false-positive class found after the first fix
    /// above: a live suite run still logged 81 [META GAP] false positives (down from
    /// thousands/day), concentrated in rapid-write scenarios (T22b, T42 flood, T51). A
    /// concrete example: two near-empty pushes for the same file in quick succession
    /// (e.g. create() then an immediate update) — by the time the second push's
    /// covers_from_write_seq is computed, the first has ALREADY been dequeued and
    /// delivered, so there's nothing left in this queue to coalesce with (the fix
    /// above only ever looks at what's still queued or in the current push's own
    /// chunk_locations). Nothing was actually lost — the first push really was
    /// delivered — but the detector had no way to know that. mark_delivered / the
    /// delivered_write_seq watermark closes this: any push whose write_seq is at or
    /// below a confirmed-delivered watermark for this file never needs independent
    /// evidence to be considered covered.
    #[tokio::test]
    async fn test_metadata_queue_covers_from_write_seq_accounts_for_prior_delivered_push() {
        let queue = MetadataQueue::new();
        let file_id = FileId::new();

        // First push (e.g. a file create) — write_seq=1, no chunk_locations.
        let mut first = FileMetadata::new("/delivered_watermark_test.bin".to_string(), dfs_common::FileType::RegularFile);
        first.id = file_id;
        first.write_seq = 1;
        queue.push(first).await;
        let entry = queue.pop().await.expect("first entry must be present");
        assert_eq!(entry.covers_from_write_seq, 1);
        // Simulate the queue worker confirming delivery of this push.
        queue.mark_delivered(file_id, entry.metadata.write_seq);

        // Second push — write_seq=2, arrives after the first was already dequeued
        // (nothing left in the queue to coalesce with), also with no chunk_locations
        // of its own to derive coverage from.
        let mut second = FileMetadata::new("/delivered_watermark_test.bin".to_string(), dfs_common::FileType::RegularFile);
        second.id = file_id;
        second.write_seq = 2;
        queue.push(second).await;
        let entry2 = queue.pop().await.expect("second entry must be present");
        assert_eq!(entry2.covers_from_write_seq, 2,
            "must account for write_seq=1 already being confirmed delivered by a prior, \
             already-dequeued push — not just this push's own write_seq stamp");
    }

    /// Backstop independent of covers_from_write_seq accuracy — see
    /// DfsClient::last_resync_sent_at's doc comment: a full snapshot's cost scales with
    /// total chunk count, so repeated false-positive resync requests for the same file
    /// must be bounded regardless of how many gap-detection classes remain unfixed.
    #[test]
    fn test_should_send_resync_snapshot_respects_debounce() {
        let now = std::time::Instant::now();
        let debounce = Duration::from_secs(30);
        assert!(DfsClient::should_send_resync_snapshot(None, now, debounce),
            "no prior resync send for this file — must send");
        let recent = now - Duration::from_secs(5);
        assert!(!DfsClient::should_send_resync_snapshot(Some(recent), now, debounce),
            "within the cooldown window — must wait, not resend");
        let old = now - Duration::from_secs(31);
        assert!(DfsClient::should_send_resync_snapshot(Some(old), now, debounce),
            "cooldown fully elapsed — must send again");
    }

    /// Root-caused 2026-07-17: the previous required_replicas = patch_addrs.len().min(2)
    /// derived the target from the chunk's CURRENT (possibly already-shrunk) known node
    /// count, so a chunk already down to 1 replica silently satisfied
    /// required_replicas=1 forever after — self-reinforcing, never recovered. This
    /// caused two confirmed real data-loss incidents (2026-07-11, 2026-07-17).
    /// compute_required_replicas must always target 2 once configured RF is 2 or more,
    /// independent of any chunk's current state.
    #[test]
    fn test_compute_required_replicas_floors_at_two_when_rf_at_least_two() {
        assert_eq!(DfsClient::compute_required_replicas(2), 2);
        assert_eq!(DfsClient::compute_required_replicas(3), 2);
        assert_eq!(DfsClient::compute_required_replicas(5), 2);
    }

    /// "Not unless replica < 2 in the config" — a genuine RF<2 cluster (or a
    /// single-node deployment) must not be forced to find a second replica that
    /// can't exist.
    #[test]
    fn test_compute_required_replicas_respects_rf_below_two() {
        assert_eq!(DfsClient::compute_required_replicas(1), 1);
        assert_eq!(DfsClient::compute_required_replicas(0), 1, "RF=0 is degenerate — floor at 1, not 0");
    }

    /// A deleted file's write_seq bookkeeping must not linger forever — file_ids are
    /// UUIDs, never reused, so a long-running client that creates and deletes many
    /// files (e.g. DVR recording rotation) would otherwise grow delivered_write_seq
    /// unboundedly. cancel() is the natural lifecycle hook: same event that already
    /// cleans up deleted_ids.
    #[tokio::test]
    async fn test_metadata_queue_cancel_clears_delivered_write_seq() {
        let queue = MetadataQueue::new();
        let file_id = FileId::new();
        queue.mark_delivered(file_id, 5);
        assert!(queue.delivered_write_seq.contains_key(&file_id));

        queue.cancel(file_id).await;
        assert!(!queue.delivered_write_seq.contains_key(&file_id),
            "delivered_write_seq must be cleared on delete, not retained forever");
    }

    /// Same lifecycle cleanup, at the DfsClient level — pending_resync and
    /// last_resync_sent_at must not linger for a deleted file either.
    #[tokio::test]
    async fn test_cancel_metadata_clears_resync_bookkeeping() {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let client = DfsClient::new(vec![addr]).unwrap();
        let file_id = FileId::new();

        client.pending_resync.insert(file_id);
        client.last_resync_sent_at.insert(file_id, std::time::Instant::now());

        client.cancel_metadata(file_id).await;

        assert!(!client.pending_resync.contains(&file_id),
            "pending_resync must be cleared on delete");
        assert!(!client.last_resync_sent_at.contains_key(&file_id),
            "last_resync_sent_at must be cleared on delete");
    }

    /// With equal load (the common case), different chunks on the same 2-replica
    /// set must spread across both replicas by chunk_id hash, not always favor
    /// the lower-address node.
    #[test]
    fn test_pick_replica_by_load_spreads_on_equal_load() {
        let addr_a: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:8901".parse().unwrap();
        let client = DfsClient::new(vec![addr_a, addr_b]).unwrap();

        let node_a = dfs_common::NodeId::new();
        let node_b = dfs_common::NodeId::new();
        let mut nim = HashMap::new();
        nim.insert(node_a, addr_a);
        nim.insert(node_b, addr_b);

        let nodes = vec![node_a, node_b];
        let cluster = [addr_a, addr_b];

        let loc0 = loc_with_nodes(chunk_id_with_hash0(0), nodes.clone());
        let (p0, _) = client.pick_replica_by_load(&loc0, &nim, &cluster);
        assert_eq!(p0, addr_a);

        let loc1 = loc_with_nodes(chunk_id_with_hash0(1), nodes.clone());
        let (p1, _) = client.pick_replica_by_load(&loc1, &nim, &cluster);
        assert_eq!(p1, addr_b);
    }

    /// A busier replica must be passed over even if the chunk-hash points at it —
    /// load is the primary signal, chunk-hash only breaks ties.
    #[test]
    fn test_pick_replica_by_load_prefers_least_loaded() {
        let addr_a: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:8901".parse().unwrap();
        let client = DfsClient::new(vec![addr_a, addr_b]).unwrap();

        let node_a = dfs_common::NodeId::new();
        let node_b = dfs_common::NodeId::new();
        let mut nim = HashMap::new();
        nim.insert(node_a, addr_a);
        nim.insert(node_b, addr_b);

        let nodes = vec![node_a, node_b];
        let cluster = [addr_a, addr_b];

        // hash[0]=0 would normally pick addr_a (see test above) — load it up
        // heavily so addr_b (idle) wins despite the hash.
        client.node_inflight_inc(addr_a);
        client.node_inflight_inc(addr_a);
        client.node_inflight_inc(addr_a);

        let loc0 = loc_with_nodes(chunk_id_with_hash0(0), nodes.clone());
        let (primary, fallbacks) = client.pick_replica_by_load(&loc0, &nim, &cluster);
        assert_eq!(primary, addr_b);
        assert_eq!(fallbacks, vec![addr_a]);
    }
}
