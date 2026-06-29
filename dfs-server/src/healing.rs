use anyhow::{Context, Result};
use dashmap::DashMap;
use dfs_common::{ChunkId, ChunkLocation, FileId, Message, NodeId, Request, Response};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Semaphore};
use tokio::task::JoinSet;
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::cluster::ClusterManager;
use crate::metadata::MetadataStore;
use crate::network::NetworkClient;
use crate::storage::ChunkStorage;

/// Suspend all destructive healer operations (orphan purge, DATA LOSS declarations,
/// over-replication cleanup, disk orphan sweep) for this many seconds after a leader
/// election, to allow the cluster to settle and metadata to catch up before we start
/// deleting anything.
pub const LEADER_CHANGE_GRACE_SECS: u64 = 1200;

/// Token-bucket bandwidth limiter for heal traffic. Unlike a bytes-in-flight
/// semaphore (which only bounds concurrency — a transfer that completes instantly
/// just frees its permits for the next one to start at full speed), this paces
/// actual throughput to the configured rate by making callers wait for tokens to
/// refill over time. One of these lives per node and governs that node's outbound
/// heal-chunk reads+sends (see handle_push_chunk_to in server.rs), since that's
/// where the bytes actually move — the leader only orchestrates which node pushes
/// to which.
pub struct BandwidthLimiter {
    state: tokio::sync::Mutex<BandwidthLimiterState>,
}

struct BandwidthLimiterState {
    rate_bytes_per_sec: f64,
    burst_cap: f64,
    tokens: f64,
    last_refill: Instant,
}

impl BandwidthLimiter {
    pub fn new(mb_per_sec: usize) -> Self {
        let rate_bytes_per_sec = (mb_per_sec * 1024 * 1024) as f64;
        // Burst cap = 2× the per-second rate so all heal_max_concurrent tasks
        // (default 8 × 4MB = 32MB) can start immediately at rates up to 16MB/s
        // without queuing. At higher rates the burst is even larger, so no task
        // ever has to wait on the first pass.
        let burst_cap = rate_bytes_per_sec * 2.0;
        Self {
            state: tokio::sync::Mutex::new(BandwidthLimiterState {
                rate_bytes_per_sec,
                burst_cap,
                tokens: burst_cap,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Current rate in MB/s (reads the live value from locked state).
    pub async fn current_rate_mb(&self) -> usize {
        let state = self.state.lock().await;
        (state.rate_bytes_per_sec / (1024.0 * 1024.0)) as usize
    }

    /// Update the token-bucket rate at runtime. Takes effect on the next acquire() call.
    /// Burst cap is always 2× the new rate. Existing token balance is clamped to the
    /// new burst cap to prevent a burst of accumulated tokens at the old rate.
    pub async fn set_rate_mb(&self, mb_per_sec: usize) {
        let rate = (mb_per_sec * 1024 * 1024) as f64;
        let burst = rate * 2.0;
        let mut state = self.state.lock().await;
        state.rate_bytes_per_sec = rate;
        state.burst_cap = burst;
        state.tokens = state.tokens.min(burst);
    }

    /// Block until `bytes` worth of bandwidth budget is available, refilling at
    /// the configured rate. Burst is capped to 2× the per-second rate so all
    /// concurrent heal tasks can start immediately and sustained throughput
    /// tracks the configured MB/s.
    pub async fn acquire(&self, bytes: usize) {
        let bytes = bytes as f64;
        loop {
            let wait = {
                let mut state = self.state.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(state.last_refill).as_secs_f64();
                state.tokens = (state.tokens + elapsed * state.rate_bytes_per_sec)
                    .min(state.burst_cap);
                state.last_refill = now;

                if state.tokens >= bytes {
                    state.tokens -= bytes;
                    None
                } else {
                    // Don't consume partial tokens on failure — leave them for the
                    // next attempt so the refill computed from elapsed time is not
                    // reset to zero, which would starve concurrent waiters.
                    let deficit = bytes - state.tokens;
                    Some(Duration::from_secs_f64(deficit / state.rate_bytes_per_sec))
                }
            };
            match wait {
                None => return,
                Some(d) => tokio::time::sleep(d).await,
            }
        }
    }
}

/// Healing manager - monitors and repairs chunk replication
/// Optimized for SBC environments (batched operations, configurable intervals)
pub struct HealingManager {
    /// Local storage
    storage: Arc<ChunkStorage>,

    /// Metadata store
    metadata: Arc<MetadataStore>,

    /// Leader-maintained in-memory chunk map, shared with Server (same Arc).
    /// Unlike MetadataStore::live_chunk_ids() (durable FILE_TABLE, only refreshed on
    /// a full PutFileMetadata), this is updated synchronously by every patch/replicate-
    /// location handler, so it never goes stale for actively-patched files. Used as an
    /// additional liveness source so the live-file orphan sweep doesn't mistake "FILE_TABLE
    /// hasn't caught up yet" for "patch-superseded" — see reconcile_live_file_candidates.
    chunk_map: Arc<DashMap<FileId, (Vec<ChunkLocation>, u64)>>,

    /// Cluster manager
    cluster: Arc<ClusterManager>,

    /// Network client for inter-node communication
    client: Arc<NetworkClient>,

    /// Target replication factor
    replication_factor: usize,

    /// Delay before starting healing after node failure (seconds)
    healing_delay_secs: u64,

    /// Scrubbing interval (hours)
    scrub_interval_hours: u64,

    /// Auto-healing enabled
    auto_heal: bool,

    /// Maximum number of chunks to process per drain cycle (queue depth)
    max_heal_per_cycle: usize,

    /// Bounds how many heal transfers the leader has concurrently outstanding
    /// (one permit per task), to avoid FD/task exhaustion from fanning out too many
    /// PushChunkTo RPCs at once. This is a concurrency cap only — actual byte
    /// throughput is paced separately by heal_bandwidth_limiter on the source node.
    heal_semaphore: Arc<Semaphore>,

    /// Max concurrent heal transfers (heal_semaphore's capacity), stored for the
    /// drain loop's task-fill bound.
    heal_max_concurrent: usize,

    /// Real bytes/sec pacing for this node's outbound heal-chunk reads+sends.
    /// Configured via DFS_HEAL_BANDWIDTH_MB.
    heal_bandwidth_limiter: Arc<BandwidthLimiter>,

    /// The configured heal bandwidth rate (MB/s), kept for logging.
    heal_bandwidth_mb: usize,

    /// Unix-epoch ms of the most recent client write seen on any node (updated via
    /// ReplicateChunkLocation broadcasts). Used by the adaptive bandwidth controller
    /// to detect active-write vs idle periods.
    last_cluster_write_ms: Arc<std::sync::atomic::AtomicU64>,

    /// Assumed node-to-node link bandwidth in MB/s. Used as the 100% baseline for
    /// the adaptive rate formula. Defaults to 100 (1Gbps). Future: auto-measured on startup.
    link_bandwidth_mb: usize,

    /// Chunks ready to heal: under-replicated, has ≥1 confirmed alive source node,
    /// and healing delay has passed. Maps chunk_id → first_detected_at so oldest-
    /// first scheduling works correctly.
    pending_healing: Arc<RwLock<HashMap<ChunkId, Instant>>>,

    /// Chunks currently being transferred — prevents double-dispatch across drain
    /// ticks. Inserted just before PushChunkTo; removed on completion or timeout.
    in_flight_healing: Arc<RwLock<HashSet<ChunkId>>>,

    /// Per-chunk confirmed-alive node list from the most recent discovery scan.
    /// Written by run_discovery_loop(); read by run_heal_loop().
    alive_nodes_cache: Arc<RwLock<HashMap<ChunkId, Vec<NodeId>>>>,

    /// Chunks with no viable source: under-replicated but ALL nodes that hold the
    /// data are currently offline. Managed entirely by discovery — moved here when
    /// 0 alive nodes are found, promoted back to pending when ≥1 source reappears.
    /// The heal loop never touches this set directly.
    stalled_healing: Arc<RwLock<HashSet<ChunkId>>>,

    /// Two-cycle orphan guard: chunk IDs that were absent from live_chunk_ids in the
    /// previous discovery pass. Only purged when absent in two consecutive passes.
    /// This prevents premature orphan-purge when the leader's metadata DB is temporarily
    /// stale (e.g. during initial metadata replication lag after a write). One extra
    /// 60s cycle of accumulation is acceptable vs. data loss from false-positive orphans.
    orphan_candidates: Arc<RwLock<HashSet<ChunkId>>>,

    /// Per-transfer timeout for a single PushChunkTo (seconds). On expiry the chunk
    /// stays in pending (retried next drain tick) and the semaphore slot is released.
    heal_transfer_timeout_secs: u64,

    /// Guards run_phantom_reconciliation_pass against overlapping itself — the
    /// periodic loop and a manual `dfs-admin healing reconcile` trigger can land
    /// close enough together to both be mid-scan at once. Each concurrent pass
    /// doubles the per-node HasChunks fan-out; piling up unbounded overlaps is
    /// exactly the kind of compounding load that turned a single slow node into
    /// the leader freeze on 2026-06-20. A second pass now logs and exits instead
    /// of running alongside the first.
    phantom_reconcile_in_progress: std::sync::atomic::AtomicBool,
}

impl HealingManager {
    /// Create a new healing manager
    pub fn new(
        storage: Arc<ChunkStorage>,
        metadata: Arc<MetadataStore>,
        cluster: Arc<ClusterManager>,
        client: Arc<NetworkClient>,
        replication_factor: usize,
        healing_delay_secs: u64,
        scrub_interval_hours: u64,
        auto_heal: bool,
        chunk_map: Arc<DashMap<FileId, (Vec<ChunkLocation>, u64)>>,
        last_cluster_write_ms: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        let max_heal_per_cycle = 200;

        // Real bytes/sec pacing for heal traffic, sized via DFS_HEAL_BANDWIDTH_MB
        // (default 32MB/s). This actually paces throughput over time instead of
        // just bounding how many bytes can be in flight at once.
        let heal_bw_mb = std::env::var("DFS_HEAL_BANDWIDTH_MB")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(32);
        let heal_bandwidth_limiter = Arc::new(BandwidthLimiter::new(heal_bw_mb));

        // Separate, independent concurrency cap on outstanding heal transfers
        // (FD/task safety) — no longer derived from the bandwidth number.
        let heal_max_concurrent = std::env::var("DFS_HEAL_MAX_CONCURRENT")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(8);
        let heal_semaphore = Arc::new(Semaphore::new(heal_max_concurrent));

        let heal_transfer_timeout_secs = std::env::var("DFS_HEAL_TRANSFER_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(120); // 2 minutes per transfer before timing out

        // Restore pending_healing first-detection times across process restarts.
        // Without this, every restart resets the whole backlog's debounce timer to
        // "just now", and a busy leader that restarts more often than
        // healing_delay_secs never heals anything.
        let mut pending_healing_map = HashMap::new();
        match metadata.get_pending_healing_inventory() {
            Ok(entries) => {
                let now_secs = dfs_common::types::current_timestamp();
                for (chunk_id, detected_at_secs) in entries {
                    let age = now_secs.saturating_sub(detected_at_secs);
                    let detected_at = Instant::now()
                        .checked_sub(Duration::from_secs(age))
                        .unwrap_or_else(Instant::now);
                    pending_healing_map.insert(chunk_id, detected_at);
                }
                if !pending_healing_map.is_empty() {
                    info!("Restored {} pending-healing entries from persisted state", pending_healing_map.len());
                }
            }
            Err(e) => {
                warn!("Failed to load persisted pending_healing inventory: {}", e);
            }
        }

        Self {
            storage,
            metadata,
            chunk_map,
            cluster,
            client,
            replication_factor,
            healing_delay_secs,
            scrub_interval_hours,
            auto_heal,
            max_heal_per_cycle,
            heal_semaphore,
            heal_max_concurrent,
            heal_bandwidth_limiter,
            heal_bandwidth_mb: heal_bw_mb,
            last_cluster_write_ms,
            link_bandwidth_mb: std::env::var("DFS_LINK_BANDWIDTH_MB")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(100),
            pending_healing: Arc::new(RwLock::new(pending_healing_map)),
            in_flight_healing: Arc::new(RwLock::new(HashSet::new())),
            alive_nodes_cache: Arc::new(RwLock::new(HashMap::new())),
            stalled_healing: Arc::new(RwLock::new(HashSet::new())),
            orphan_candidates: Arc::new(RwLock::new(HashSet::new())),
            heal_transfer_timeout_secs,
            phantom_reconcile_in_progress: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Start background healing tasks
    pub async fn start(self: Arc<Self>) {
        if !self.auto_heal {
            info!("Auto-healing is disabled");
            return;
        }

        info!(
            "Starting healing manager (delay: {}s, scrub: {}h, max_per_cycle: {}, bandwidth: {}MB/s, max_concurrent: {}, transfer_timeout: {}s)",
            self.healing_delay_secs, self.scrub_interval_hours, self.max_heal_per_cycle,
            self.heal_bandwidth_mb, self.heal_max_concurrent, self.heal_transfer_timeout_secs
        );

        // Discovery loop: scans all chunks, bulk-queries nodes, classifies under/over
        // replication, purges orphans. Runs every 60s and writes into pending_healing
        // and alive_nodes_cache.
        let discovery = self.clone();
        tokio::spawn(async move {
            discovery.run_discovery_loop().await;
        });

        // Heal loop: drains pending_healing using cached alive_nodes data. Runs every
        // 15s and starts immediately on the first tick — no waiting for discovery.
        let healer = self.clone();
        tokio::spawn(async move {
            healer.run_heal_loop().await;
        });

        // Scrubber (runs at configured interval)
        let scrubber = self.clone();
        tokio::spawn(async move {
            scrubber.run_scrubber().await;
        });

        // Phantom reconciliation: independent periodic verify-and-prune pass (see
        // run_phantom_reconciliation_pass doc comment for why this exists alongside
        // the discovery loop's own ghost-pruning).
        let reconciler = self.clone();
        tokio::spawn(async move {
            reconciler.run_phantom_reconciliation_loop().await;
        });

        // Adaptive bandwidth controller: re-evaluates heal rate every 2s based on
        // write activity and queue depth. Backs off to 10% during active writes,
        // ramps to 80% when idle. Overrides toward 80% as queue debt grows.
        let bw_controller = self.clone();
        tokio::spawn(async move {
            bw_controller.run_bandwidth_controller().await;
        });
    }

    async fn run_bandwidth_controller(&self) {
        // Tier boundaries (heal queue depth in chunks):
        //   < TIER1  → floor rate (trivially small, don't compete)
        //   TIER1..TIER2 → proportional scale, boosted by growth rate
        //   ≥ TIER2  → ceiling rate (queue is dangerously deep, must keep up)
        const TIER1: usize = 100;
        const TIER2: usize = 1_000;
        const LOW_PCT:  f64 = 0.10;
        const HIGH_PCT: f64 = 0.60; // logical ceiling: healer > writer → can never fall behind
        // A sustained growth rate of GROWTH_BOOST_RATE items/sec in the middle tier
        // contributes up to GROWTH_BOOST_SHARE of the remaining headroom, letting the
        // system react to a fast-growing queue before depth alone would force the rate up.
        const GROWTH_BOOST_RATE:  f64 = 10.0;  // items/sec = "growing fast"
        const GROWTH_BOOST_SHARE: f64 = 0.50;  // max extra fraction of headroom from growth
        const INTERVAL_SECS: f64 = 2.0;

        let mut prev_depth: usize = 0;

        loop {
            tokio::time::sleep(Duration::from_secs_f64(INTERVAL_SECS)).await;

            let queue_depth = self.pending_healing.read().await.len();

            // items/sec — positive means queue is growing (falling behind), negative means draining
            let growth_rate = (queue_depth as f64 - prev_depth as f64) / INTERVAL_SECS;
            prev_depth = queue_depth;

            let factor: f64 = if queue_depth < TIER1 {
                // Trivially small queue — stay at floor regardless of growth rate.
                // Even 0→100 in 2 s doesn't warrant going above the floor: depth
                // must be meaningful before we ramp up.
                0.0
            } else if queue_depth >= TIER2 {
                // Deep queue — use ceiling rate to match or exceed incoming rate.
                1.0
            } else {
                // Middle tier: linear depth scale, plus a growth-rate boost so a
                // fast-growing queue in this range pushes the rate up earlier.
                let depth_scale = (queue_depth - TIER1) as f64 / (TIER2 - TIER1) as f64;
                let growth_boost = if growth_rate > 0.0 {
                    (growth_rate / GROWTH_BOOST_RATE).clamp(0.0, 1.0) * GROWTH_BOOST_SHARE
                } else {
                    0.0
                };
                (depth_scale + growth_boost).clamp(0.0, 1.0)
            };

            let target_pct = LOW_PCT + (HIGH_PCT - LOW_PCT) * factor;
            let target_mb = ((self.link_bandwidth_mb as f64 * target_pct) as usize).max(1);

            self.heal_bandwidth_limiter.set_rate_mb(target_mb).await;

            if queue_depth > 0 || growth_rate.abs() > 0.5 {
                debug!(
                    "heal-bw: depth={} growth={:+.1}/s factor={:.2} rate={}MB/s",
                    queue_depth, growth_rate, factor, target_mb
                );
            }
        }
    }

    /// Discovery loop — runs every 60s on the cluster leader.
    ///
    /// Two-tier design:
    ///   Fast cycle (every 60s): only checks chunks that need attention — those in
    ///   pending_healing or with nodes.len() < RF in the routing table. Per-node
    ///   HasChunks payload is scoped to chunks that node is listed for.
    ///
    ///   Deep cycle (every healing_delay_secs): checks all routing table entries to
    ///   detect ghost nodes, orphans, and undocumented local copies. Interval matches
    ///   healing_delay_secs so new under-replicated chunks are discovered within one
    ///   delay window. Same scoped-per-node approach.
    async fn run_discovery_loop(&self) {
        let mut cycle_counter = 0u32;
        let mut cleanup_counter = 0u32;
        let mut disk_sweep_counter = 0u32;
        let mut was_leader = false;

        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;

            let is_leader = self.cluster.is_leader().await;

            if is_leader != was_leader {
                if is_leader {
                    info!("This node is now the cluster leader — taking over healing coordination");
                    // Reset cycle_counter so the first scan after a leadership change is
                    // always a deep scan. Without this, cycle_counter may be at an arbitrary
                    // value and the fast-path condition (% deep_every == 1) might not fire
                    // for many cycles, delaying discovery of under-replicated chunks.
                    cycle_counter = 0;
                } else {
                    info!("This node is no longer the cluster leader — yielding healing to new leader");
                }
                was_leader = is_leader;
            }

            // Disk orphan sweep: runs on EVERY node every 2 cycles (2 minutes).
            // First fire is at the 2-minute mark (120s startup grace from the 60s
            // per-cycle sleep × 2 cycles).  Must run on followers too — they accumulate
            // orphaned files when they miss DeleteChunk RPCs while offline, and also when
            // patch operations re-place a chunk on different nodes without the old node
            // receiving a DeleteChunk.  Intentionally before the is_leader gate.
            disk_sweep_counter += 1;
            if disk_sweep_counter >= 2 {
                disk_sweep_counter = 0;
                self.run_disk_orphan_sweep().await;
            }

            if !is_leader {
                continue;
            }

            // Deep scan every healing_delay_secs (rounded to whole 60s cycles, minimum 1).
            // This ensures new under-replicated chunks are discovered within one healing delay
            // window — the same responsiveness guarantee the delay provides for known chunks.
            let deep_every = ((self.healing_delay_secs + 59) / 60).max(1) as u32;
            cycle_counter += 1;
            let deep = cycle_counter % deep_every == 1; // first cycle after becoming leader is always deep
            if let Err(e) = self.run_discovery_pass(deep).await {
                warn!("Discovery pass error: {}", e);
            }

            cleanup_counter += 1;
            if cleanup_counter >= 10 {
                cleanup_counter = 0;
                if let Err(e) = self.cleanup_stale_pending().await {
                    warn!("Pending healing cleanup error: {}", e);
                }
            }
        }
    }

    /// Heal loop — runs every 15s on the cluster leader.
    ///
    /// Drains pending_healing for chunks that have waited healing_delay_secs,
    /// using alive_nodes_cache populated by the discovery loop. Starts immediately
    /// on first tick so chunks discovered in a previous cycle are healed right away
    /// without waiting another 60s for a fresh scan.
    async fn run_heal_loop(&self) {
        let mut was_leader = false;

        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;

            let is_leader = self.cluster.is_leader().await;

            if is_leader != was_leader {
                was_leader = is_leader;
                // Leadership transitions are logged by the discovery loop
            }

            if !is_leader {
                continue;
            }

            if let Err(e) = self.drain_heal_queue().await {
                warn!("Heal queue drain error: {}", e);
            }
        }
    }

    /// Insert `chunk_id` into pending_healing if not already present, recording the
    /// current time as its first-detection time, and persist that detection time so
    /// a process restart doesn't lose track of how long this chunk has been waiting.
    /// No-op (and no metadata write) if the chunk is already pending.
    async fn mark_pending(&self, chunk_id: ChunkId) {
        let mut pending = self.pending_healing.write().await;
        if let std::collections::hash_map::Entry::Vacant(e) = pending.entry(chunk_id) {
            e.insert(Instant::now());
            drop(pending);
            if let Err(err) = self.metadata.put_pending_healing_async(chunk_id, dfs_common::types::current_timestamp()).await {
                warn!("Failed to persist pending_healing entry for {}: {}", chunk_id, err);
            }
        }
    }

    /// Remove `chunk_id` from pending_healing (it reached RF, was purged, or is no
    /// longer relevant) and clear its persisted detection time.
    async fn clear_pending(&self, chunk_id: &ChunkId) {
        Self::clear_pending_static(&self.pending_healing, &self.metadata, chunk_id).await;
    }

    /// Remove a batch of chunks from pending_healing — called when files are deleted
    /// so their chunks don't inflate the pending count indefinitely.
    pub async fn clear_pending_for_deleted_chunks(&self, chunk_ids: &[ChunkId]) {
        let mut pending = self.pending_healing.write().await;
        let mut to_delete = Vec::new();
        for chunk_id in chunk_ids {
            if pending.remove(chunk_id).is_some() {
                to_delete.push(*chunk_id);
            }
        }
        drop(pending);
        for chunk_id in to_delete {
            let _ = self.metadata.delete_pending_healing_async(chunk_id).await;
        }
    }

    /// Free-function equivalent of `clear_pending` for static/spawned contexts
    /// (e.g. `do_heal_chunk_inner`) that don't have a `&self`.
    async fn clear_pending_static(
        pending_healing: &Arc<RwLock<HashMap<ChunkId, Instant>>>,
        metadata: &Arc<MetadataStore>,
        chunk_id: &ChunkId,
    ) {
        let existed = pending_healing.write().await.remove(chunk_id).is_some();
        if existed {
            if let Err(err) = metadata.delete_pending_healing_async(*chunk_id).await {
                warn!("Failed to delete pending_healing entry for {}: {}", chunk_id, err);
            }
        }
    }

    /// Cleanup stale entries from pending_healing map
    /// Removes chunks that no longer exist or have been pending for too long
    async fn cleanup_stale_pending(&self) -> Result<()> {
        let max_pending_time = Duration::from_secs(self.healing_delay_secs * 20); // 20x healing delay

        let to_remove: Vec<ChunkId> = {
            let pending = self.pending_healing.read().await;
            pending.iter()
                .filter_map(|(chunk_id, detected_at)| {
                    // Remove if pending for too long (likely deleted or unrecoverable)
                    if detected_at.elapsed() > max_pending_time {
                        debug!("Removing stale pending healing entry for chunk {} (pending for {}s)",
                               chunk_id, detected_at.elapsed().as_secs());
                        return Some(*chunk_id);
                    }

                    // Remove if the chunk: location record is gone — this means the file was
                    // deleted (or the chunk was legitimately purged as an orphan).  There is
                    // nothing left to heal regardless of whether raw chunk data still exists
                    // on disk (stale data will be cleaned up separately).
                    if self.metadata.get_chunk_location(chunk_id).ok().flatten().is_none() {
                        debug!("Removing pending healing entry for chunk {} — no location record", chunk_id);
                        return Some(*chunk_id);
                    }

                    None
                })
                .collect()
        };

        let removed_count = to_remove.len();
        for chunk_id in to_remove {
            self.clear_pending(&chunk_id).await;
        }

        if removed_count > 0 {
            info!("Cleaned up {} stale pending healing entries", removed_count);
        }

        // Prune stalled entries for chunks that have been deleted/purged — they won't
        // come back, and discovery won't clean them up since it only sees live chunks.
        {
            let mut stalled = self.stalled_healing.write().await;
            stalled.retain(|chunk_id| {
                self.metadata.get_chunk_location(chunk_id).ok().flatten().is_some()
            });
        }

        Ok(())
    }

    /// Run periodic scrubber
    /// Disk-level orphan sweep — runs on every node every 5 minutes (or immediately
    /// via TriggerOrphanCleanup).
    ///
    /// Two independent checks, each catching a different leak:
    ///  1. Routing-table membership: deletes chunk files whose ChunkLocation record
    ///     no longer lists this node — catches missed DeleteChunk RPCs while offline.
    ///  2. Live-file membership: deletes chunks still listed as ours in the routing
    ///     table, but no longer referenced by ANY live file — catches patch-superseded
    ///     chunks the routing table was deliberately left stale for (see
    ///     handle_patch_chunk/handle_multi_patch) that the inline fast-evict missed
    ///     (e.g. this node was offline when the patch happened).
    ///
    /// Category 2 is the more dangerous one to get wrong, since it relies on this
    /// node's own (eventually-consistent) metadata replica, which might be stale. So
    /// in addition to the existing grace period and a two-pass confirmation, it
    /// requires either explicit confirmation from the leader (non-leader nodes) or
    /// proof the whole cluster has been stable for several minutes (leader nodes,
    /// which have no one more authoritative to ask) — see
    /// authorize_live_file_orphan_deletes(). Any ambiguity (RPC failure, timeout,
    /// unreachable leader) defers deletion to the next cycle rather than proceeding.
    /// Snapshot of every chunk_id currently referenced by any file's in-memory
    /// chunk_map entry. See the `chunk_map` field doc comment for why this is a
    /// fresher liveness source than `MetadataStore::live_chunk_ids()`.
    fn live_chunk_ids_from_chunk_map(&self) -> HashSet<ChunkId> {
        let mut live = HashSet::new();
        for entry in self.chunk_map.iter() {
            let (locs, _) = entry.value();
            for loc in locs {
                live.insert(loc.chunk_id);
            }
        }
        live
    }

    /// Phantom-replica reconciliation: walks every live ChunkLocation in CHUNK_TABLE,
    /// verifies actual presence on every listed online node via a fresh HasChunks/
    /// has_chunk check, and immediately prunes any node confirmed not to hold the
    /// chunk — as long as at least one other listed node DOES hold it, so a chunk
    /// is never stranded at zero replicas. If pruning drops a chunk below RF, it's
    /// queued for immediate healing via queue_chunks_immediate.
    ///
    /// This runs independently of the discovery fast/deep cadence and the
    /// pending_healing interplay used by ghost-pruning in run_discovery_pass. In
    /// practice that existing path has been observed to leave large backlogs of
    /// confirmed-ghost entries unpruned for hours after an incident (e.g. a node
    /// losing its disk) without the root interaction ever being fully isolated.
    /// This pass acts directly on freshly-verified presence each time it runs,
    /// rather than depending on that pipeline, so it reconciles phantoms even when
    /// the other path stalls.
    pub async fn run_phantom_reconciliation_pass(&self) {
        use std::sync::atomic::Ordering;
        if self.phantom_reconcile_in_progress.compare_exchange(
            false, true, Ordering::AcqRel, Ordering::Relaxed,
        ).is_err() {
            warn!("Phantom reconciliation: previous pass is still running — skipping this trigger \
                   instead of running alongside it (overlapping passes compound RPC load on every node)");
            return;
        }

        // RAII guard: clears the in-progress flag on every exit path from the inner
        // pass, including early returns and a panic unwind, not just the happy path.
        struct ResetGuard<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for ResetGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        let _guard = ResetGuard(&self.phantom_reconcile_in_progress);

        self.run_phantom_reconciliation_pass_inner().await;
    }

    async fn run_phantom_reconciliation_pass_inner(&self) {
        if !self.cluster.is_leader().await {
            return;
        }

        let all_nodes = self.cluster.get_all_nodes().await;
        let total_nodes = all_nodes.len();
        let online_nodes: Vec<_> = all_nodes.iter()
            .filter(|n| n.status == dfs_common::NodeStatus::Online)
            .cloned()
            .collect();
        let nodes_down = total_nodes.saturating_sub(online_nodes.len());

        let grace_elapsed = self.cluster.time_since_became_leader().await
            .map_or(true, |d| d.as_secs() >= LEADER_CHANGE_GRACE_SECS);

        if nodes_down > 1 || !grace_elapsed {
            debug!("Phantom reconciliation: skipping — nodes_down={} grace_elapsed={}", nodes_down, grace_elapsed);
            return;
        }

        let local_id = self.cluster.local_node_id();

        // Full streaming scan of CHUNK_TABLE, restricted to chunks referenced by a
        // live file (same liveness source as the deep discovery scan) with more
        // than one listed node — a single-node record can't have a "phantom" to
        // prune without risking stranding the chunk at zero replicas.
        let metadata = self.metadata.clone();
        let scan_result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<ChunkLocation>> {
            let live = metadata.live_chunk_ids()?;
            let mut chunks = Vec::new();
            metadata.scan_chunk_locations(|loc| {
                if live.contains(&loc.chunk_id) && loc.nodes.len() > 1 {
                    chunks.push(loc);
                }
                true
            })?;
            Ok(chunks)
        }).await;

        let chunks_to_check = match scan_result {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => { warn!("Phantom reconciliation: scan error: {}", e); return; }
            Err(e) => { warn!("Phantom reconciliation: scan panicked: {}", e); return; }
        };

        if chunks_to_check.is_empty() {
            return;
        }

        info!("Phantom reconciliation: verifying presence for {} chunks", chunks_to_check.len());

        // Build per-node assignment, scoped like the discovery pass — each node is
        // only asked about chunks it's actually listed for.
        let mut node_assigned: HashMap<NodeId, Vec<ChunkId>> = HashMap::new();
        for loc in &chunks_to_check {
            for &node_id in &loc.nodes {
                node_assigned.entry(node_id).or_default().push(loc.chunk_id);
            }
        }

        let mut node_chunk_presence: HashMap<NodeId, HashSet<ChunkId>> = HashMap::new();

        // Local presence check: one recursive directory walk of the chunk store
        // (list_chunks(), the same bulk crawl run_disk_orphan_sweep already uses)
        // instead of tens of thousands of individual has_chunk() stat() calls — a
        // sequential directory read is far cheaper than that many scattered random
        // lookups, and it still must run in spawn_blocking since it's real disk I/O
        // (the inline version of the per-chunk loop froze the leader for ~2.5 hours
        // with zero log output — incident 2026-06-20).
        if node_assigned.contains_key(&local_id) {
            let storage = self.storage.clone();
            let local_set = tokio::task::spawn_blocking(move || {
                storage.list_chunks().map(|v| v.into_iter().collect::<HashSet<_>>())
            }).await;
            match local_set {
                Ok(Ok(set)) => { node_chunk_presence.insert(local_id, set); }
                Ok(Err(e)) => warn!("Phantom reconciliation: local chunk listing failed: {}", e),
                Err(e) => warn!("Phantom reconciliation: local chunk listing panicked: {}", e),
            }
        }

        for node_info in &online_nodes {
            if node_info.id == local_id {
                continue;
            }
            if !self.cluster.is_leader().await {
                debug!("Phantom reconciliation: leadership changed mid-scan — aborting");
                return;
            }
            let assigned = match node_assigned.get(&node_info.id) {
                Some(ids) if !ids.is_empty() => ids.clone(),
                _ => continue,
            };
            let request = Request::HasChunks { chunk_ids: assigned.clone() };
            // Bounded wait: a node whose own HasChunks handler is slow (e.g. a large
            // assignment, or it's mid-handling another bulk request) must not be able
            // to stall this entire pass. A timeout is treated the same as any other
            // RPC failure below — "unknown," not "missing."
            const HAS_CHUNKS_TIMEOUT: Duration = Duration::from_secs(30);
            match tokio::time::timeout(
                HAS_CHUNKS_TIMEOUT,
                self.client.send_message(node_info.addr, Message::Request(request)),
            ).await {
                Ok(Ok(envelope)) => {
                    if let Message::Response(Response::BoolVec { values }) = envelope.message {
                        let mut present = HashSet::new();
                        for (chunk_id, has) in assigned.iter().zip(values.iter()) {
                            if *has {
                                present.insert(*chunk_id);
                            }
                        }
                        node_chunk_presence.insert(node_info.id, present);
                    } else {
                        warn!("Phantom reconciliation: unexpected response to HasChunks from node {}", node_info.id);
                    }
                }
                Ok(Err(e)) => {
                    // RPC failure means "unknown," not "missing" — the node is simply
                    // excluded from node_chunk_presence and treated as unverified below.
                    debug!("Phantom reconciliation: HasChunks failed for node {} ({}): skipping node this pass", node_info.id, e);
                }
                Err(_) => {
                    debug!("Phantom reconciliation: HasChunks timed out for node {} after {:?}: skipping node this pass",
                        node_info.id, HAS_CHUNKS_TIMEOUT);
                }
            }
        }

        let mut to_heal: Vec<ChunkId> = Vec::new();
        let mut db_puts: Vec<ChunkLocation> = Vec::new();
        let mut broadcasts: Vec<ChunkLocation> = Vec::new();

        for loc in chunks_to_check {
            let mut confirmed_present: Vec<NodeId> = Vec::new();
            let mut confirmed_missing: Vec<NodeId> = Vec::new();

            for &node_id in &loc.nodes {
                // Only a verdict we actually obtained this pass counts — an offline
                // node or one whose HasChunks RPC failed is "unknown," never "missing."
                if let Some(present_set) = node_chunk_presence.get(&node_id) {
                    if present_set.contains(&loc.chunk_id) {
                        confirmed_present.push(node_id);
                    } else {
                        confirmed_missing.push(node_id);
                    }
                }
            }

            // Never strand a chunk at zero replicas — only prune when at least one
            // other listed node is confirmed to actually hold it.
            if confirmed_missing.is_empty() || confirmed_present.is_empty() {
                if !confirmed_missing.is_empty() && confirmed_present.is_empty() {
                    // We have absences but no confirmations either way for the
                    // remaining nodes — this is the case that's otherwise silently
                    // skipped. Surface it: either the safety guard is correctly
                    // protecting a genuinely at-risk chunk, or something upstream
                    // (RPC failure, an unverified node) is hiding a real holder.
                    let unverified: Vec<NodeId> = loc.nodes.iter()
                        .filter(|n| !confirmed_missing.contains(n))
                        .copied()
                        .collect();
                    warn!("Phantom reconciliation: chunk {} has {} confirmed-absent node(s) {:?} \
                           but no other listed node confirmed present (unverified: {:?}) — skipping to avoid stranding",
                        loc.chunk_id, confirmed_missing.len(), confirmed_missing, unverified);
                }
                continue;
            }

            let pruned_nodes: Vec<NodeId> = loc.nodes.iter()
                .filter(|n| !confirmed_missing.contains(n))
                .copied()
                .collect();

            warn!("Phantom reconciliation: chunk {} — pruning {} confirmed-absent node(s): {:?}",
                loc.chunk_id, confirmed_missing.len(), confirmed_missing);

            let updated = ChunkLocation {
                chunk_id: loc.chunk_id,
                nodes: pruned_nodes,
                size: loc.size,
                checksum: loc.checksum,
                file_offset: loc.file_offset,
                written_at: Some(Self::now_ms()),
                client_write_seq: None,
                file_id: loc.file_id,
            };

            if confirmed_present.len() < self.replication_factor {
                to_heal.push(loc.chunk_id);
            }

            db_puts.push(updated.clone());
            broadcasts.push(updated);
        }

        let pruned_count = db_puts.len();

        if !db_puts.is_empty() {
            let metadata = Arc::clone(&self.metadata);
            let puts = db_puts;
            let result = tokio::task::spawn_blocking(move || {
                metadata.batch_update_chunk_locations(&puts, &[])
            }).await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("Phantom reconciliation: batch metadata update failed: {}", e),
                Err(e) => warn!("Phantom reconciliation: batch update spawn_blocking panicked: {}", e),
            }
        }

        for loc in &broadcasts {
            Self::broadcast_chunk_location_shared(loc, &self.cluster, &self.client).await;
        }

        if !to_heal.is_empty() {
            info!("Phantom reconciliation: {} chunk(s) now under RF after pruning — queuing for immediate healing", to_heal.len());
            self.queue_chunks_immediate(to_heal).await;
        }

        if pruned_count > 0 {
            info!("Phantom reconciliation: pass complete — {} chunk(s) corrected", pruned_count);
        } else {
            debug!("Phantom reconciliation: pass complete — no phantoms found");
        }
    }

    /// Background loop for run_phantom_reconciliation_pass(). Runs on a fixed
    /// interval (default 10 minutes, DFS_PHANTOM_RECONCILE_INTERVAL_SECS override)
    /// regardless of the discovery loop's own cadence.
    async fn run_phantom_reconciliation_loop(&self) {
        let interval_secs = std::env::var("DFS_PHANTOM_RECONCILE_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(600);
        let mut timer = interval(Duration::from_secs(interval_secs));
        timer.tick().await; // skip immediate first tick — let the cluster settle on startup
        loop {
            timer.tick().await;
            self.run_phantom_reconciliation_pass().await;
        }
    }

    pub async fn run_disk_orphan_sweep(&self) {
        // Don't delete local chunks when the cluster is degraded — a copy on a
        // currently-offline node might be the only remaining replica.
        {
            let all_nodes = self.cluster.get_all_nodes().await;
            let total = all_nodes.len();
            let online = all_nodes.iter()
                .filter(|n| n.status == dfs_common::NodeStatus::Online)
                .count();
            let nodes_down = total.saturating_sub(online);

            let grace_elapsed = self.cluster.time_since_became_leader().await
                .map_or(true, |d| d.as_secs() >= LEADER_CHANGE_GRACE_SECS);

            if nodes_down > 1 {
                warn!("Skipping disk orphan sweep — {} node(s) down (max 1 allowed for destructive ops)", nodes_down);
                return;
            }
            if !grace_elapsed {
                let elapsed = self.cluster.time_since_became_leader().await
                    .map_or(0.0, |d| d.as_secs_f64());
                warn!("Skipping disk orphan sweep — within grace period after leader election ({:.0}s elapsed of {}s)",
                    elapsed, LEADER_CHANGE_GRACE_SECS);
                return;
            }
        }

        const GRACE_PERIOD_SECS: u64 = 300;

        // 2x the periodic full-reconciliation interval (server.rs RECONCILE_INTERVAL =
        // 300s) — that loop is the slowest *guaranteed* metadata-catchup path in the
        // system, so doubling it bounds how stale this node's live_chunk_ids() view
        // could legitimately still be for a routine (non-degraded) cluster.
        const LIVE_FILE_GRACE_SECS: u64 = 600;

        let storage = self.storage.clone();
        let metadata = self.metadata.clone();
        let local_node_id = self.cluster.local_node_id();
        // Union of the durable (FILE_TABLE) view and the in-memory chunk_map view.
        // FILE_TABLE only refreshes on a full PutFileMetadata, so a long-lived,
        // repeatedly in-place-patched file (e.g. a mounted qcow2 disk) can have a
        // current chunk_id that's missing from it for an unbounded amount of time —
        // chunk_map is kept synchronously fresh by every patch/replicate-location
        // handler and never has that gap. Taking the union means a chunk is only
        // ever treated as "not live" if BOTH sources agree it's gone.
        let live_from_chunk_map = self.live_chunk_ids_from_chunk_map();

        let result = tokio::task::spawn_blocking(move || {
            let mut live_chunks = metadata.live_chunk_ids()?;
            live_chunks.extend(live_from_chunk_map);
            let chunks = storage.list_chunks()?;
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let mut deleted = 0usize;
            let mut kept = 0usize;
            let mut too_recent = 0usize;
            // (chunk_id, age_secs) — still routed to us, but no live file references
            // it. Collected for the async leader-confirm/stability gate below; never
            // deleted inside this blocking closure.
            let mut live_file_candidates: Vec<(ChunkId, u64)> = Vec::new();

            for chunk_id in &chunks {
                // Determine whether this local file is still our responsibility.
                // The routing table is cluster-wide, so Ok(Some(loc)) only means
                // the chunk exists somewhere — we must also verify this node is
                // still listed.  A stale local copy where loc.nodes = [other, nodes]
                // is an orphan that DeleteChunk RPCs should have cleaned up but didn't.
                let loc_record = match metadata.get_chunk_location(chunk_id) {
                    Ok(v) => v,
                    Err(e) => {
                        debug!("Disk orphan sweep: routing table error for {}: {}", chunk_id, e);
                        continue;
                    }
                };

                // Ok(Some(loc)) that explicitly excludes us: the routing table is
                // authoritative and confidently says this chunk belongs elsewhere.
                // Safe to age-grace-delete locally without a cluster round trip.
                if let Some(loc) = &loc_record {
                    if !loc.nodes.contains(&local_node_id) {
                        let age_secs = storage.get_chunk_mtime(chunk_id)
                            .map(|mtime| now_secs.saturating_sub(mtime))
                            .unwrap_or(u64::MAX);

                        if age_secs > GRACE_PERIOD_SECS {
                            if let Err(e) = storage.delete_chunk(chunk_id) {
                                debug!("Disk orphan sweep: failed to delete {}: {}", chunk_id, e);
                            } else {
                                debug!("Disk orphan sweep: deleted local orphan {}", chunk_id);
                                deleted += 1;
                            }
                        } else {
                            too_recent += 1;
                        }
                        continue;
                    }
                }

                // Either still routed to us (Some(loc) containing us), or Ok(None) —
                // no location record at all. A bare `None` is NOT proof the chunk is
                // dead: it just as easily means this node's local metadata hasn't
                // caught up (e.g. after a leadership change or a metadata-replication
                // backlog) while the chunk is still legitimately live elsewhere.
                // Treating a bare absence as a confirmed orphan caused real data loss —
                // a node whose metadata fell behind cannibalized its entire chunk store
                // because every chunk looked unassigned locally. Route both cases
                // through the same leader-confirm + cluster-stability gate used below
                // for "no live file references this chunk", instead of deleting on
                // local age alone.
                if loc_record.is_some() && live_chunks.contains(chunk_id) {
                    kept += 1;
                    continue;
                }

                let age_secs = storage.get_chunk_mtime(chunk_id)
                    .map(|mtime| now_secs.saturating_sub(mtime))
                    .unwrap_or(u64::MAX);
                live_file_candidates.push((*chunk_id, age_secs));
            }

            Ok::<_, anyhow::Error>((deleted, kept, too_recent, chunks.len(), live_file_candidates))
        }).await;

        let (deleted, kept, too_recent, total, live_file_candidates) = match result {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => { warn!("Disk orphan sweep error: {}", e); return; }
            Err(e) => { warn!("Disk orphan sweep panicked: {}", e); return; }
        };

        if deleted > 0 || too_recent > 0 {
            info!("Disk orphan sweep: {} local chunks checked — {} orphans deleted, {} kept (legitimately ours), {} too recent",
                  total, deleted, kept, too_recent);
        } else {
            debug!("Disk orphan sweep: {} chunks checked, all accounted for", total);
        }

        self.reconcile_live_file_candidates(live_file_candidates, LIVE_FILE_GRACE_SECS).await;
    }

    /// Two-pass + age-gate + leader-confirm/stability check for category-2 candidates
    /// (routed to us, but absent from our own live_chunk_ids()). Reuses
    /// orphan_candidates as the per-node two-pass debounce set.
    async fn reconcile_live_file_candidates(&self, candidates: Vec<(ChunkId, u64)>, grace_secs: u64) {
        if candidates.is_empty() {
            self.orphan_candidates.write().await.clear();
            return;
        }

        let prev_candidates = self.orphan_candidates.read().await.clone();
        let mut new_candidates: HashSet<ChunkId> = HashSet::new();
        let mut ready_to_delete: Vec<ChunkId> = Vec::new();

        for (chunk_id, age_secs) in &candidates {
            if *age_secs < grace_secs {
                debug!("Live-file orphan grace: skipping {} (age={}s, grace={}s)", chunk_id, age_secs, grace_secs);
                continue;
            }
            if prev_candidates.contains(chunk_id) {
                ready_to_delete.push(*chunk_id);
            } else {
                new_candidates.insert(*chunk_id);
                debug!("Live-file orphan candidate (first sighting, will re-check next pass): {}", chunk_id);
            }
        }
        *self.orphan_candidates.write().await = new_candidates;

        if ready_to_delete.is_empty() {
            return;
        }

        let authorized = self.authorize_live_file_orphan_deletes(&ready_to_delete).await;
        if authorized.is_empty() {
            debug!("Live-file orphan sweep: {} candidate(s) confirmed-absent locally but not authorized for deletion this cycle",
                   ready_to_delete.len());
            return;
        }

        let mut evicted = 0usize;
        for chunk_id in &authorized {
            if let Err(e) = self.storage.delete_chunk(chunk_id) {
                debug!("Live-file orphan sweep: failed to delete {}: {}", chunk_id, e);
                continue;
            }
            let _ = self.metadata.delete_chunk_location_async(*chunk_id).await;
            evicted += 1;
        }
        if evicted > 0 {
            info!("Live-file orphan sweep: evicted {}/{} authorized chunk(s) (patch-superseded, missed by inline fast-evict)",
                  evicted, authorized.len());
        }
    }

    /// Decide which of `candidates` are safe to actually delete right now.
    ///
    /// - Not the leader: ask the leader via ConfirmChunksLive — it's normally the
    ///   most caught-up replica. Anything the leader confirms live is excluded. Any
    ///   RPC failure, timeout, or unexpected response authorizes NOTHING this cycle
    ///   (fail safe — retried next sweep, no data loss risk from being conservative).
    /// - Is the leader (no one more authoritative to ask): require every other known
    ///   node to be Online AND to report at least STABILITY_SECS of continuous
    ///   process uptime via GetNodeStats. A node that recently restarted may not have
    ///   finished catching up its own metadata replica yet — proceeding before that
    ///   settles is exactly the "split-brain mass delete" scenario this guards
    ///   against. Any node failing either check authorizes NOTHING this cycle.
    async fn authorize_live_file_orphan_deletes(&self, candidates: &[ChunkId]) -> Vec<ChunkId> {
        const STABILITY_SECS: u64 = 300;
        const RPC_TIMEOUT: Duration = Duration::from_secs(5);

        if self.cluster.is_leader().await {
            let nodes = self.cluster.get_all_nodes().await;
            let local_id = self.cluster.local_node_id();
            for node in &nodes {
                if node.id == local_id {
                    continue;
                }
                if node.status != dfs_common::NodeStatus::Online {
                    debug!("Live-file orphan sweep: deferring — node {} is not online", node.id);
                    return Vec::new();
                }
                let req = Request::GetNodeStats;
                match tokio::time::timeout(RPC_TIMEOUT, self.client.send_message(node.addr, Message::Request(req))).await {
                    Ok(Ok(envelope)) => match envelope.message {
                        Message::Response(Response::NodeStats { uptime_secs, .. }) => {
                            if uptime_secs < STABILITY_SECS {
                                debug!("Live-file orphan sweep: deferring — node {} uptime {}s < {}s stability requirement",
                                       node.id, uptime_secs, STABILITY_SECS);
                                return Vec::new();
                            }
                        }
                        _ => {
                            debug!("Live-file orphan sweep: deferring — unexpected response to GetNodeStats from {}", node.id);
                            return Vec::new();
                        }
                    },
                    _ => {
                        debug!("Live-file orphan sweep: deferring — GetNodeStats failed/timed out for node {}", node.id);
                        return Vec::new();
                    }
                }
            }
            // Every other node is online and has been stable for long enough —
            // this leader's own local view is authoritative.
            candidates.to_vec()
        } else {
            let leader_addr = match self.cluster.get_leader_addr().await {
                Some(addr) => addr,
                None => {
                    debug!("Live-file orphan sweep: deferring — no known leader to confirm with");
                    return Vec::new();
                }
            };
            let req = Request::ConfirmChunksLive { chunk_ids: candidates.to_vec() };
            match tokio::time::timeout(RPC_TIMEOUT, self.client.send_message(leader_addr, Message::Request(req))).await {
                Ok(Ok(envelope)) => match envelope.message {
                    Message::Response(Response::ChunkLiveness { live }) => {
                        let live_set: HashSet<ChunkId> = live.into_iter().collect();
                        candidates.iter().copied().filter(|id| !live_set.contains(id)).collect()
                    }
                    _ => {
                        debug!("Live-file orphan sweep: deferring — unexpected response to ConfirmChunksLive from leader");
                        Vec::new()
                    }
                },
                _ => {
                    debug!("Live-file orphan sweep: deferring — ConfirmChunksLive failed/timed out against leader {}", leader_addr);
                    Vec::new()
                }
            }
        }
    }

    async fn run_scrubber(&self) {
        let scrub_interval = Duration::from_secs(self.scrub_interval_hours * 3600);
        let mut timer = interval(scrub_interval);

        // Skip the immediate first tick - we don't want to scrub on startup
        timer.tick().await;

        loop {
            timer.tick().await;

            info!("Starting scrubbing pass");
            if let Err(e) = self.scrub_all_chunks().await {
                warn!("Scrubbing error: {}", e);
            }
        }
    }

    /// Discovery pass — scans all chunk IDs, bulk-queries node presence, classifies
    /// Classify chunks, update pending_healing, and update alive_nodes_cache.
    /// Called by run_discovery_loop() every 60s. Does NOT issue any PushChunkTo —
    /// actual healing is handled by drain_heal_queue().
    ///
    /// `deep`: when true, checks every routing table entry (detects ghost nodes,
    /// undocumented local copies, orphans). When false (fast cycle), only checks
    /// chunks that need attention: those in pending_healing or with nodes.len() < RF.
    /// The fast cycle is O(pending) instead of O(all_chunks), making it viable at
    /// 500K+ chunk scale without OOM or multi-minute stalls.
    ///
    /// In both modes, the HasChunks RPC payload sent to each node is scoped to only
    /// the chunk IDs where that node is listed as a replica — so message size scales
    /// with per-node replica count, not total cluster chunk count.
    async fn run_discovery_pass(&self, deep: bool) -> Result<()> {
        if deep {
            debug!("Running deep discovery pass (full routing table scan)");
        } else {
            debug!("Running fast discovery pass (pending/under-replicated chunks only)");
        }

        // Gate destructive operations (orphan purge, DATA LOSS declarations,
        // over-replication cleanup) on strict cluster health:
        //   1. At most 1 node down — if 2+ nodes are down we might be the only copy.
        //   2. Past the post-election grace period — cluster must be settled before
        //      we start deleting anything.
        // Under-replication healing (adding replicas) is always safe and is not gated.
        let all_nodes = self.cluster.get_all_nodes().await;
        let total_nodes = all_nodes.len();
        let online_nodes = all_nodes.iter()
            .filter(|n| n.status == dfs_common::NodeStatus::Online)
            .count();
        let nodes_down = total_nodes.saturating_sub(online_nodes);

        let grace_elapsed = self.cluster.time_since_became_leader().await
            .map_or(true, |d| d.as_secs() >= LEADER_CHANGE_GRACE_SECS);

        let destructive_allowed = grace_elapsed && nodes_down <= 1;

        if !destructive_allowed {
            if !grace_elapsed {
                let elapsed = self.cluster.time_since_became_leader().await
                    .map_or(0.0, |d| d.as_secs_f64());
                warn!("Skipping destructive healing operations — within grace period after leader election ({:.0}s elapsed of {}s)",
                    elapsed, LEADER_CHANGE_GRACE_SECS);
            } else {
                warn!("Skipping destructive healing operations — {} node(s) down (max 1 allowed)", nodes_down);
            }
        }

        let local_id = self.cluster.local_node_id();
        let online_nodes: Vec<_> = self.cluster.get_all_nodes().await
            .into_iter()
            .filter(|n| n.status == dfs_common::NodeStatus::Online)
            .collect();

        // All known node IDs (online + offline) — used to detect completely-removed ghost nodes.
        let all_known_node_ids: HashSet<NodeId> = self.cluster.get_all_nodes().await
            .into_iter()
            .map(|n| n.id)
            .collect();

        // Sled scan: stream through chunk location records, building only what we need.
        // Offloaded to a blocking thread to avoid stalling the async executor.
        //
        // We never materialise the full Vec<ChunkLocation> — instead the blocking task
        // produces the derived data structures directly:
        //   chunks_to_check   — locations that need network verification this cycle
        //   live_chunks       — chunk IDs referenced by live file records (deep only)
        //
        // This keeps peak memory proportional to what we actually act on, not the
        // entire routing table.
        let metadata_scan = self.metadata.clone();
        let pending_snapshot: HashSet<ChunkId> = self.pending_healing.read().await.keys().copied().collect();

        struct ScanResult {
            /// Locations to verify with HasChunks this cycle.
            chunks_to_check: Vec<ChunkLocation>,
        }

        // Fast path: no sled scan at all. We fetch locations only for chunks already
        // in pending_healing — O(pending) individual lookups, not O(all_chunks).
        // Deep path: full streaming sled scan for finding under-RF chunks that
        // haven't entered pending_healing yet. (Orphan detection used to live here
        // too — it now runs per-node in run_disk_orphan_sweep(), independent of
        // leadership; see that function's doc comment.)
        let scan_result = if !deep {
            // Fast: look up only pending chunks by ID.
            let mut chunks_to_check = Vec::with_capacity(pending_snapshot.len());
            for chunk_id in &pending_snapshot {
                match self.metadata.get_chunk_location(chunk_id) {
                    Ok(Some(loc)) => {
                        // If the chunk carries a file_id, verify the file still exists.
                        // Orphan routing table entries (file deleted but RCL entry
                        // re-added later) would otherwise cause the fast scan to keep
                        // trying to heal chunks that belong to deleted files.
                        let is_orphan = if let Some(file_id) = loc.file_id {
                            !self.metadata.file_exists_by_id(file_id).unwrap_or(true)
                        } else {
                            false // can't tell without file_id; defer to deep scan
                        };
                        if is_orphan {
                            let _ = self.metadata.delete_chunk_location_async(*chunk_id).await;
                            self.clear_pending(chunk_id).await;
                        } else {
                            chunks_to_check.push(loc);
                        }
                    }
                    // Chunk was deleted from routing table (file deleted) but its
                    // pending_healing entry was never cleaned up. Prune it now.
                    Ok(None) => { self.clear_pending(chunk_id).await; }
                    Err(_) => {}
                }
            }
            ScanResult { chunks_to_check }
        } else {
            tokio::task::spawn_blocking(move || {
                let live = metadata_scan.live_chunk_ids()?;

                let mut chunks_to_check = Vec::new();

                metadata_scan.scan_chunk_locations(|loc| {
                    if live.contains(&loc.chunk_id) {
                        chunks_to_check.push(loc);
                    }
                    true
                })?;

                Ok::<_, anyhow::Error>(ScanResult { chunks_to_check })
            })
            .await
            .context("spawn_blocking for chunk scan panicked")??
        };

        // Re-check leadership after the sled scan — on large clusters the scan can
        // take minutes, during which leadership may have transferred. No point sending
        // HasChunks RPCs or modifying state if we're no longer the leader.
        if !self.cluster.is_leader().await {
            debug!("Leadership changed during deep scan — aborting discovery pass");
            return Ok(());
        }

        let ScanResult { chunks_to_check } = scan_result;

        // work carries (chunk_id, status, confirmed_alive_node_ids) from the bulk scan.
        let mut work: Vec<(ChunkId, ReplicationStatus, Vec<NodeId>)> = Vec::new();
        let mut pending_count = 0;

        // Pending DB writes — applied in a single spawn_blocking after all classification.
        // Direct calls to metadata.put_chunk_location() / delete_chunk_location() call
        // redb's begin_write() which blocks the OS thread. Under a heal storm with many
        // concurrent async tasks all hitting begin_write(), every Tokio worker thread can
        // end up blocked on the mutex, freezing the entire async runtime.
        let mut db_puts: Vec<ChunkLocation> = Vec::new();
        let mut db_deletes: Vec<ChunkId> = Vec::new();

        // Orphan detection/purging used to live here (leader-only, gated on
        // destructive_allowed + a 30-minute age grace + two-pass confirmation). It now
        // runs on every node via run_disk_orphan_sweep()/reconcile_live_file_candidates(),
        // independent of leadership, with a leader-confirm-or-cluster-stability gate
        // replacing the old grace-period-only check — see authorize_live_file_orphan_deletes().

        // --- Build per-node chunk assignment maps ---
        // For each online node, collect only the chunk IDs where that node is listed
        // as a replica. This scopes the HasChunks payload to each node's actual
        // assignments rather than the entire cluster chunk universe.
        //
        // node_id → ordered Vec<ChunkId> that node is responsible for
        let mut node_assigned: HashMap<NodeId, Vec<ChunkId>> = HashMap::new();
        for loc in &chunks_to_check {
            for node_id in &loc.nodes {
                if all_known_node_ids.contains(node_id) {
                    node_assigned.entry(*node_id).or_default().push(loc.chunk_id);
                }
            }
        }

        // --- HasChunks RPCs: one per online node, payload = that node's assignments ---
        // node_id → HashSet<ChunkId> confirmed present
        let mut node_chunk_presence: HashMap<NodeId, HashSet<ChunkId>> = HashMap::new();
        // Nodes whose HasChunks RPC failed this cycle — skip in classification rather than
        // treating as "chunk missing". A timeout means we don't know the node's state;
        // penalizing it causes ghost pruning + over-replication on the next cycle.
        let mut rpc_failed_nodes: HashSet<NodeId> = HashSet::new();

        // Local node: check storage directly (no network hop).
        // One recursive directory walk (list_chunks()) instead of tens of thousands
        // of individual has_chunk() stat() calls on a deep cycle — far cheaper than
        // that many scattered random lookups, and still run via spawn_blocking since
        // it's real disk I/O (the inline per-chunk version of this loop froze the
        // leader for ~2.5 hours with zero log output — incident 2026-06-20).
        if node_assigned.contains_key(&local_id) {
            let storage = self.storage.clone();
            let local_set = tokio::task::spawn_blocking(move || {
                storage.list_chunks().map(|v| v.into_iter().collect::<HashSet<_>>())
            }).await;
            match local_set {
                Ok(Ok(set)) => { node_chunk_presence.insert(local_id, set); }
                Ok(Err(e)) => warn!("Discovery pass: local chunk listing failed: {}", e),
                Err(e) => warn!("Discovery pass: local chunk listing panicked: {}", e),
            }
        }

        // Remote nodes: one HasChunks RPC each, scoped to that node's assignments.
        for node_info in &online_nodes {
            if node_info.id == local_id {
                continue;
            }
            // Abort if we lost leadership mid-scan — don't send RPCs as non-leader.
            if !self.cluster.is_leader().await {
                debug!("Leadership changed during HasChunks RPCs — aborting discovery pass");
                return Ok(());
            }
            let assigned = match node_assigned.get(&node_info.id) {
                Some(ids) if !ids.is_empty() => ids.clone(),
                _ => continue, // node has no assigned chunks in this scan scope — skip
            };
            let request = Request::HasChunks { chunk_ids: assigned.clone() };
            match self.client.send_message(node_info.addr, Message::Request(request)).await {
                Ok(envelope) => {
                    if let Message::Response(Response::BoolVec { values }) = envelope.message {
                        let mut present = HashSet::new();
                        for (chunk_id, has) in assigned.iter().zip(values.iter()) {
                            if *has {
                                present.insert(*chunk_id);
                            }
                        }
                        debug!("Node {} confirmed {}/{} assigned chunks present",
                               node_info.id, present.len(), assigned.len());
                        node_chunk_presence.insert(node_info.id, present);
                    } else {
                        warn!("Unexpected response to HasChunks from node {}", node_info.id);
                    }
                }
                Err(e) => {
                    warn!("HasChunks RPC failed for node {} ({}): skipping node for this scan cycle", node_info.id, e);
                    rpc_failed_nodes.insert(node_info.id);
                }
            }
        }

        // Accumulate chunk location updates for batch broadcast at end of pass.
        let mut location_updates: Vec<ChunkLocation> = Vec::new();

        // Classify all chunks — no work cap here. Every chunk must be classified so
        // that pending_healing timestamps are updated for all under-replicated chunks,
        // not just the first max_heal_per_cycle. Without this, chunks past position 200
        // never enter pending_healing and never get healed until the queue drains.
        // The work batch is capped after classification, sorted oldest-first so long-
        // waiting chunks are not perpetually starved by newer ones.
        //
        // Note: chunks_to_check now carries full ChunkLocation records from the sled
        // scan, so we don't need a per-chunk DB lookup here.
        let mut classify_count = 0usize;
        for location in chunks_to_check {
            let chunk_id = location.chunk_id;
            classify_count += 1;
            if classify_count % 100 == 0 {
                tokio::task::yield_now().await;
            }

            let metadata_node_count = location.nodes.len();

            let mut actual_replicas = 0usize;
            let mut nodes_without_chunk: Vec<NodeId> = Vec::new();
            let mut confirmed_alive_nodes: Vec<NodeId> = Vec::new();
            let mut removed_node_ids: Vec<NodeId> = Vec::new();

            for node_id in &location.nodes {
                if !all_known_node_ids.contains(node_id) {
                    // Node is completely unknown to the cluster (removed/decommissioned).
                    // Prune it immediately — no delay needed, it will never come back.
                    removed_node_ids.push(*node_id);
                    continue;
                }
                // Only count online nodes — offline nodes are expected to be absent.
                if online_nodes.iter().any(|n| n.id == *node_id) {
                    if rpc_failed_nodes.contains(node_id) {
                        // RPC timed out — we don't know if chunk is present.
                        // Skip entirely: don't count as alive OR missing.
                        // Treating as missing causes ghost pruning + over-replication.
                        debug!("Chunk {} — node {} had RPC failure this cycle, skipping", chunk_id, node_id);
                        continue;
                    }
                    if node_chunk_presence.get(node_id).map_or(false, |s| s.contains(&chunk_id)) {
                        actual_replicas += 1;
                        confirmed_alive_nodes.push(*node_id);
                    } else {
                        nodes_without_chunk.push(*node_id);
                    }
                }
            }

            // Prune removed (completely unknown) nodes from metadata immediately.
            if !removed_node_ids.is_empty() && destructive_allowed {
                warn!(
                    "Chunk {} — pruning {} removed node(s) from metadata (not in cluster): {:?}",
                    chunk_id, removed_node_ids.len(), removed_node_ids
                );
                let pruned_nodes: Vec<NodeId> = location.nodes.iter()
                    .filter(|n| !removed_node_ids.contains(n))
                    .copied()
                    .collect();
                let updated_location = ChunkLocation {
                    chunk_id,
                    nodes: pruned_nodes,
                    size: location.size,
                    checksum: location.checksum,
                    file_offset: location.file_offset,
                    written_at: location.written_at,
                    client_write_seq: None,
                    file_id: location.file_id,
                };
                db_puts.push(updated_location.clone());
                location_updates.push(updated_location);
            }

            // Ghost node pruning: only remove a node from metadata after the healing
            // delay has passed for this chunk. A single HasChunks miss is not enough —
            // the node may have just restarted (connections drop, storage comes up within
            // seconds, but the next heal cycle is 60s away). We wait healing_delay_secs
            // before trusting the miss, same patience we apply to under-replication.
            if !nodes_without_chunk.is_empty() && actual_replicas > 0 {
                // Start the ghost-pruning timer the first time we see these nodes missing.
                // Without this, chunks at exactly RF alive nodes never enter pending_healing
                // (they're classified Ok below and evicted), so the delay can never expire.
                self.mark_pending(chunk_id).await;

                let pending = self.pending_healing.read().await;
                let delay_passed = pending.get(&chunk_id)
                    .map_or(false, |t| t.elapsed() >= Duration::from_secs(self.healing_delay_secs));
                drop(pending);

                if delay_passed {
                    warn!(
                        "Chunk {} — pruning {} ghost node(s) after delay (confirmed missing for {}s+): {:?}",
                        chunk_id, nodes_without_chunk.len(), self.healing_delay_secs, nodes_without_chunk
                    );
                    let pruned_nodes: Vec<NodeId> = location.nodes.iter()
                        .filter(|n| !nodes_without_chunk.contains(n))
                        .copied()
                        .collect();
                    let updated_location = ChunkLocation {
                        chunk_id,
                        nodes: pruned_nodes,
                        size: location.size,
                        checksum: location.checksum,
                        file_offset: location.file_offset,
                        written_at: Some(Self::now_ms()),
                        client_write_seq: None,
                        file_id: location.file_id,
                    };
                    db_puts.push(updated_location.clone());
                    location_updates.push(updated_location);
                } else {
                    debug!(
                        "Chunk {} — {} online node(s) didn't confirm holding it, waiting for delay before pruning: {:?}",
                        chunk_id, nodes_without_chunk.len(), nodes_without_chunk
                    );
                }
            }

            let replication_factor = self.replication_factor;

            // Detect unrecoverable chunks: actual_replicas == 0 and all metadata nodes
            // were reachable and confirmed they don't have it (or node list is empty).
            // Requirements before declaring unrecoverable:
            //  1. All metadata nodes must be reachable (online + responded to HasChunks) —
            //     an offline node might still hold the chunk.
            //  2. The healing delay must have passed — a node that just restarted may be
            //     online and responding but its storage could still be loading. We wait the
            //     same delay we use for under-replication before writing off the data.
            let all_metadata_nodes_reachable = location.nodes.iter()
                .all(|n| node_chunk_presence.contains_key(n));
            if actual_replicas == 0 && (location.nodes.is_empty() || all_metadata_nodes_reachable) {
                let pending = self.pending_healing.read().await;
                let delay_passed = if location.nodes.is_empty() {
                    // No nodes in the location record at all. This could mean:
                    //   (a) A genuinely incomplete write (nodes never populated), OR
                    //   (b) The chunk is very new and the location record hasn't been
                    //       updated with node IDs yet due to convergence lag.
                    // Use written_at to apply the same delay as normal under-replication —
                    // don't skip the delay just because nodes is empty.
                    let age_secs = location.written_at_secs();
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let written_age = now.saturating_sub(age_secs);
                    // Also check pending_healing as a fallback (written_at may be 0 for old records).
                    let pending_delay = pending.get(&chunk_id)
                        .map_or(false, |t| t.elapsed() >= Duration::from_secs(self.healing_delay_secs));
                    (age_secs > 0 && written_age >= self.healing_delay_secs) || pending_delay
                } else {
                    pending.get(&chunk_id)
                        .map_or(false, |t| t.elapsed() >= Duration::from_secs(self.healing_delay_secs))
                };
                drop(pending);

                if delay_passed {
                    if !destructive_allowed {
                        warn!(
                            "Chunk {} has 0 accessible replicas and delay passed, but skipping DATA LOSS purge — cluster degraded ({} node(s) down or in grace period)",
                            chunk_id, nodes_down
                        );
                    } else {
                        // Before declaring DATA LOSS, check whether this chunk_id was
                        // superseded by a newer write. PatchChunk renames old→new on disk,
                        // so the old chunk_id's file is gone from every node — but the data
                        // still exists under the new chunk_id. A superseded chunk appearing
                        // to have 0 replicas is expected cleanup, not data loss.
                        let is_superseded = if let (Some(file_id), Some(file_offset)) = (location.file_id, location.file_offset) {
                            match self.metadata.get_file(&file_id) {
                                Ok(Some(file_meta)) => {
                                    // If the active chunk at this file position is a different
                                    // chunk_id, we were patched away — this is not data loss.
                                    let current = file_meta.chunk_locations.iter()
                                        .find(|loc| loc.file_offset == Some(file_offset));
                                    match current {
                                        Some(cur) => cur.chunk_id != chunk_id,
                                        None => true, // position removed — chunk is orphaned
                                    }
                                }
                                Ok(None) => true, // file deleted — chunk is orphaned
                                Err(_) => false,  // metadata error — be conservative
                            }
                        } else {
                            false
                        };

                        if is_superseded {
                            debug!(
                                "Chunk {} has 0 replicas but was superseded by a newer write at the same file position — purging stale metadata (not DATA LOSS)",
                                chunk_id
                            );
                        } else {
                            warn!(
                                "DATA LOSS: Chunk {} is permanently unrecoverable ({} metadata nodes, all confirmed empty) — purging stale metadata",
                                chunk_id, location.nodes.len()
                            );
                        }
                        db_deletes.push(chunk_id);
                        self.clear_pending(&chunk_id).await;
                        continue;
                    }
                } else {
                    debug!(
                        "Chunk {} has 0 accessible replicas but delay not yet passed — waiting before declaring unrecoverable",
                        chunk_id
                    );
                }
            }

            // Reconcile routing table: if discovery found the chunk alive on nodes that
            // aren't in location.nodes, add them now before classifying. This prevents
            // the routing table from staying stale when the leader or other nodes have a
            // physical copy that wasn't recorded (e.g. from a previous local-fallback heal
            // that pushed data but didn't update metadata, or manual copy). Without this,
            // over-replication cleanup removes the "surprise" copies before the table is
            // updated, leaving the chunk under-replicated again on the next cycle.
            let extra_nodes: Vec<NodeId> = confirmed_alive_nodes.iter()
                .filter(|n| !location.nodes.contains(n))
                .copied()
                .collect();
            let (location, metadata_node_count) = if !extra_nodes.is_empty() {
                let mut updated_nodes = location.nodes.clone();
                updated_nodes.extend_from_slice(&extra_nodes);
                debug!(
                    "Chunk {} — reconciling routing table: adding {} newly-discovered node(s): {:?}",
                    chunk_id, extra_nodes.len(), extra_nodes
                );
                let updated_location = ChunkLocation {
                    chunk_id,
                    nodes: updated_nodes,
                    size: location.size,
                    checksum: location.checksum,
                    file_offset: location.file_offset,
                    written_at: location.written_at,
                    client_write_seq: None,
                    file_id: location.file_id,
                };
                db_puts.push(updated_location.clone());
                location_updates.push(updated_location.clone());
                let new_count = updated_location.nodes.len();
                (updated_location, new_count)
            } else {
                (location, metadata_node_count)
            };

            let status = if actual_replicas < replication_factor {
                ReplicationStatus::UnderReplicated
            } else if actual_replicas > replication_factor {
                ReplicationStatus::OverReplicated
            } else {
                ReplicationStatus::Ok
            };

            match status {
                ReplicationStatus::UnderReplicated => {
                    if confirmed_alive_nodes.is_empty() {
                        // No source available — move to stalled. Discovery will promote
                        // it back to pending as soon as a node that holds it comes online.
                        debug!("Chunk {} has 0 confirmed alive nodes — moving to stalled", chunk_id);
                        self.stalled_healing.write().await.insert(chunk_id);
                        // Keep first_detected timestamp in pending so we know how long
                        // it has been under-replicated, but mark it explicitly stalled.
                        self.mark_pending(chunk_id).await;
                        pending_count += 1;
                        continue;
                    }

                    // Source available — ensure not in stalled (promote if it was).
                    {
                        let mut stalled = self.stalled_healing.write().await;
                        if stalled.remove(&chunk_id) {
                            debug!("Chunk {} has a source again — promoted from stalled to pending", chunk_id);
                        }
                    }

                    // For chunks that were never fully replicated to RF nodes, apply a
                    // minimum delay of healing_delay_secs before healing. This prevents
                    // the healer from adding replicas to chunks that are still being
                    // actively written — the write pipeline emits chunks one at a time
                    // and the healer must wait for the file to be fully written before
                    // it can know the correct final replica set.
                    let never_fully_replicated = metadata_node_count < replication_factor;
                    if self.should_heal(&chunk_id).await {
                        work.push((chunk_id, ReplicationStatus::UnderReplicated, confirmed_alive_nodes.clone()));
                    } else {
                        if never_fully_replicated {
                            // Ensure pending_healing entry exists so should_heal() starts
                            // tracking the delay from first discovery.
                            self.mark_pending(chunk_id).await;
                        }
                        pending_count += 1;
                    }
                }
                ReplicationStatus::OverReplicated => {
                    if destructive_allowed {
                        // Add to pending_healing with the real detection time so the
                        // normal healing_delay_secs wait applies before trimming.
                        // This gives the VerifyChunkIntegrity pass (in do_cleanup_excess_shared)
                        // time to run after the cluster has settled.
                        self.mark_pending(chunk_id).await;
                        work.push((chunk_id, ReplicationStatus::OverReplicated, confirmed_alive_nodes.clone()));
                    } else {
                        debug!("Skipping over-replication cleanup for {} — cluster degraded", chunk_id);
                    }
                }
                ReplicationStatus::Ok => {
                    // Only evict from pending when there are no ghost nodes remaining.
                    // If nodes_without_chunk is non-empty we need the pending_healing
                    // entry to persist as the ghost-pruning delay timer.
                    if nodes_without_chunk.is_empty() {
                        self.clear_pending(&chunk_id).await;
                        self.stalled_healing.write().await.remove(&chunk_id);
                    }
                }
            }
        }

        // Apply all accumulated metadata writes in a single spawn_blocking call.
        // This is the key fix for the runtime deadlock: redb's begin_write() is a
        // synchronous exclusive lock. Calling it from async code during a healing
        // storm (many concurrent tasks) blocks all Tokio worker threads, freezing
        // the runtime. One batched spawn_blocking keeps the OS thread off the
        // async executor and reduces total lock contention to a single acquisition.
        if !db_puts.is_empty() || !db_deletes.is_empty() {
            let metadata = Arc::clone(&self.metadata);
            let result = tokio::task::spawn_blocking(move || {
                metadata.batch_update_chunk_locations(&db_puts, &db_deletes)?;
                Ok::<_, anyhow::Error>(())
            }).await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("Batch metadata update failed: {}", e),
                Err(e) => warn!("Batch metadata update spawn_blocking panicked: {}", e),
            }
        }

        // Update alive_nodes_cache so drain_heal_queue can read fresh presence data
        // without waiting for another scan. Under-replicated and over-replicated chunks
        // both need this; drain_heal_queue() reads the cache for all work it executes.
        {
            let mut cache = self.alive_nodes_cache.write().await;
            for (chunk_id, _status, confirmed_alive) in &work {
                cache.insert(*chunk_id, confirmed_alive.clone());
            }
            // Prune entries for chunks that are now Ok (pending removed above) or
            // have been purged as orphans, so the cache doesn't grow unboundedly.
            cache.retain(|chunk_id, _| {
                self.metadata.get_chunk_location(chunk_id)
                    .ok()
                    .flatten()
                    .is_some()
            });
        }

        // Batch-broadcast all chunk location updates accumulated during this pass.
        // One ReplicateChunkLocations per peer instead of one connection per chunk.
        if !location_updates.is_empty() {
            let cluster = self.cluster.clone();
            let client = self.client.clone();
            let local_id = cluster.local_node_id();
            let updates = location_updates;
            debug!("Batch-broadcasting {} chunk location updates to peers", updates.len());
            tokio::spawn(async move {
                let nodes = cluster.get_all_nodes().await;
                for node in &nodes {
                    if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                        continue;
                    }
                    let req = Request::ReplicateChunkLocations { locations: updates.clone() };
                    if let Err(e) = client.send_message(node.addr, Message::Request(req)).await {
                        debug!("Failed to batch-broadcast {} chunk location updates to node {}: {}",
                               updates.len(), node.id, e);
                    }
                }
            });
        }

        let under_count = work.iter().filter(|(_, s, _)| *s == ReplicationStatus::UnderReplicated).count();
        let over_count  = work.iter().filter(|(_, s, _)| *s == ReplicationStatus::OverReplicated).count();
        if under_count > 0 || over_count > 0 || pending_count > 0 {
            info!(
                "Discovery complete: under={}, over={}, pending_delay={}",
                under_count, over_count, pending_count
            );
        }

        Ok(())
    }

    /// Drain the heal queue — dispatches PushChunkTo / DeleteChunkReplica for all
    /// chunks in pending_healing that are ready (delay passed, source known, not
    /// in-flight, not stalled). Tasks are spawned-and-forgotten; in_flight_healing
    /// prevents double-dispatch across 15s ticks.
    async fn drain_heal_queue(&self) -> Result<()> {
        let destructive_allowed = {
            let all_nodes = self.cluster.get_all_nodes().await;
            let total = all_nodes.len();
            let online = all_nodes.iter()
                .filter(|n| n.status == dfs_common::NodeStatus::Online)
                .count();
            let nodes_down = total.saturating_sub(online);
            let grace_elapsed = self.cluster.time_since_became_leader().await
                .map_or(true, |d| d.as_secs() >= LEADER_CHANGE_GRACE_SECS);
            grace_elapsed && nodes_down <= 1
        };

        let work: Vec<(ChunkId, ReplicationStatus, Vec<NodeId>)> = {
            let pending   = self.pending_healing.read().await;
            let cache     = self.alive_nodes_cache.read().await;
            let in_flight = self.in_flight_healing.read().await;
            let stalled   = self.stalled_healing.read().await;

            let mut v = Vec::new();
            for (chunk_id, detected_at) in pending.iter() {
                if detected_at.elapsed() < Duration::from_secs(self.healing_delay_secs) {
                    continue;
                }
                // Skip already in-flight.
                if in_flight.contains(chunk_id) {
                    continue;
                }
                // Skip stalled — no source available; discovery promotes when a node returns.
                if stalled.contains(chunk_id) {
                    continue;
                }

                let confirmed_alive = match cache.get(chunk_id) {
                    Some(nodes) => nodes.clone(),
                    None => continue, // discovery hasn't run yet for this chunk
                };

                let status = if confirmed_alive.len() < self.replication_factor {
                    ReplicationStatus::UnderReplicated
                } else if confirmed_alive.len() > self.replication_factor {
                    if destructive_allowed { ReplicationStatus::OverReplicated } else { continue }
                } else {
                    continue; // at RF, discovery will clean up pending entry
                };

                v.push((*chunk_id, status, confirmed_alive));
            }
            drop(pending);
            drop(cache);
            drop(in_flight);
            drop(stalled);

            // Sort oldest-first, cap to max_heal_per_cycle.
            {
                let pending = self.pending_healing.read().await;
                v.sort_by_cached_key(|(chunk_id, _, _)| {
                    pending.get(chunk_id)
                        .map(|t| std::cmp::Reverse(t.elapsed()))
                        .unwrap_or(std::cmp::Reverse(Duration::ZERO))
                });
            }
            let skipped = v.len().saturating_sub(self.max_heal_per_cycle);
            if skipped > 0 {
                debug!("drain_heal_queue: deferring {} chunks past per-cycle cap", skipped);
            }
            v.truncate(self.max_heal_per_cycle);
            v
        };

        if work.is_empty() {
            return Ok(());
        }

        let total = work.len();

        // Bounds how many Tokio tasks exist simultaneously waiting for the semaphore.
        // Without this cap, all `work` tasks would be spawned at once, each holding OS
        // resources (stack, file descriptors from pending connects) while blocked on
        // acquire — causing "too many orphaned sockets" kernel warnings under load.
        // Actual byte throughput is paced separately by heal_bandwidth_limiter on
        // whichever node ends up being the transfer's source.
        let max_live = self.heal_max_concurrent.max(1);
        let mut set: JoinSet<()> = JoinSet::new();
        let mut iter = work.into_iter();

        loop {
            // Fill up to max_live concurrent tasks.
            while set.len() < max_live {
                let Some((chunk_id, status, confirmed_alive)) = iter.next() else { break };

                let storage = self.storage.clone();
                let metadata = self.metadata.clone();
                let cluster = self.cluster.clone();
                let client = self.client.clone();
                let pending_healing = self.pending_healing.clone();
                let in_flight_healing = self.in_flight_healing.clone();
                let stalled_healing = self.stalled_healing.clone();
                let heal_semaphore = self.heal_semaphore.clone();
                let replication_factor = self.replication_factor;
                let transfer_timeout = Duration::from_secs(self.heal_transfer_timeout_secs);
                let bandwidth_limiter = self.heal_bandwidth_limiter.clone();

                set.spawn(async move {
                    let _permit = heal_semaphore.acquire().await;

                    let result = tokio::time::timeout(transfer_timeout, async {
                        match status {
                            ReplicationStatus::UnderReplicated => {
                                HealingManager::do_heal_chunk_shared(
                                    &chunk_id, confirmed_alive, &storage, &metadata, &cluster, &client,
                                    &pending_healing, &in_flight_healing, replication_factor, &bandwidth_limiter,
                                ).await
                            }
                            ReplicationStatus::OverReplicated => {
                                HealingManager::do_cleanup_excess_shared(
                                    &chunk_id, confirmed_alive, &storage, &metadata, &cluster, &client, replication_factor,
                                ).await
                            }
                            ReplicationStatus::Ok => Ok(()),
                        }
                    }).await;

                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            warn!("Heal failed for chunk {}: {} — stalling until next discovery", chunk_id, e);
                            in_flight_healing.write().await.remove(&chunk_id);
                            stalled_healing.write().await.insert(chunk_id);
                        }
                        Err(_) => {
                            warn!("Heal timed out for chunk {} after {}s — stalling until next discovery", chunk_id, transfer_timeout.as_secs());
                            in_flight_healing.write().await.remove(&chunk_id);
                            stalled_healing.write().await.insert(chunk_id);
                        }
                    }
                    // _permit drops here, releasing semaphore budget
                });
            }

            if set.is_empty() {
                break;
            }

            // Wait for any one task to finish, then loop to fill the slot.
            set.join_next().await;
        }

        debug!("Heal queue drain: processed {} tasks (max_live={})", total, max_live);
        Ok(())
    }

    /// Check if a chunk should be healed (delay has passed)
    async fn should_heal(&self, chunk_id: &ChunkId) -> bool {
        let elapsed = {
            let pending = self.pending_healing.read().await;
            pending.get(chunk_id).map(|detected_at| detected_at.elapsed())
        };

        match elapsed {
            Some(elapsed) => {
                // Check if delay has passed
                if elapsed >= Duration::from_secs(self.healing_delay_secs) {
                    true
                } else {
                    debug!(
                        "Chunk {} waiting for healing delay ({}/{}s)",
                        chunk_id,
                        elapsed.as_secs(),
                        self.healing_delay_secs
                    );
                    false
                }
            }
            None => {
                // First time detecting under-replication
                self.mark_pending(*chunk_id).await;
                debug!(
                    "Chunk {} marked for healing (delay: {}s)",
                    chunk_id, self.healing_delay_secs
                );
                false
            }
        }
    }


    /// Static heal implementation — callable from both instance methods and spawned tasks.
    async fn do_heal_chunk_shared(
        chunk_id: &ChunkId,
        confirmed_alive_nodes: Vec<NodeId>,
        storage: &Arc<ChunkStorage>,
        metadata: &Arc<MetadataStore>,
        cluster: &Arc<ClusterManager>,
        client: &Arc<NetworkClient>,
        pending_healing: &Arc<RwLock<HashMap<ChunkId, Instant>>>,
        in_flight_healing: &Arc<RwLock<HashSet<ChunkId>>>,
        replication_factor: usize,
        bandwidth_limiter: &Arc<BandwidthLimiter>,
    ) -> Result<()> {
        // In-flight guard: prevents two concurrent tasks healing the same chunk.
        {
            let mut in_flight = in_flight_healing.write().await;
            if in_flight.contains(chunk_id) {
                debug!("Chunk {} heal already in-flight, skipping", chunk_id);
                return Ok(());
            }
            in_flight.insert(*chunk_id);
        }

        let result = Self::do_heal_chunk_inner(
            chunk_id, confirmed_alive_nodes, storage, metadata, cluster, client, pending_healing, replication_factor, bandwidth_limiter,
        ).await;
        in_flight_healing.write().await.remove(chunk_id);
        result
    }

    async fn do_heal_chunk_inner(
        chunk_id: &ChunkId,
        confirmed_alive_nodes: Vec<NodeId>,
        storage: &Arc<ChunkStorage>,
        metadata: &Arc<MetadataStore>,
        cluster: &Arc<ClusterManager>,
        client: &Arc<NetworkClient>,
        pending_healing: &Arc<RwLock<HashMap<ChunkId, Instant>>>,
        replication_factor: usize,
        bandwidth_limiter: &Arc<BandwidthLimiter>,
    ) -> Result<()> {
        info!("Leader healing under-replicated chunk: {}", chunk_id);

        let location = metadata
            .get_chunk_location(chunk_id)?
            .ok_or_else(|| anyhow::anyhow!("Chunk location not found"))?;

        // Fast orphan guard: if no file claims this chunk, don't waste a heal slot on
        // it. The deep-scan orphan sweep runs every ~6 min and will delete the routing
        // table entry; we just don't want to block a 120s heal transfer on a ghost chunk.
        let is_orphan = match location.file_id {
            Some(file_id) => !metadata.file_exists_by_id(file_id).unwrap_or(true),
            None => {
                // No file_id recorded — we can't verify quickly. Stall for now and let
                // the deep scan handle it rather than burning 120s on a speculative heal.
                warn!("Chunk {} has no file_id in routing table — deferring to orphan sweep", chunk_id);
                Self::clear_pending_static(pending_healing, metadata, chunk_id).await;
                return Ok(());
            }
        };
        if is_orphan {
            warn!("Chunk {}: file {:?} no longer exists — removing orphan chunk from routing table", chunk_id, location.file_id);
            let _ = metadata.delete_chunk_location_async(*chunk_id).await;
            Self::clear_pending_static(pending_healing, metadata, chunk_id).await;
            return Ok(());
        }

        // Build alive list from confirmed_alive_nodes passed in from the bulk scan.
        // These were verified by the HasChunks bulk RPC in the classification phase —
        // no per-chunk re-querying here, which avoids misclassifying healthy nodes as
        // ghosts due to transient RPC failures.
        let local_id = cluster.local_node_id();
        let mut alive: Vec<(NodeId, std::net::SocketAddr)> = Vec::new();
        for node_id in confirmed_alive_nodes {
            if let Some(info) = cluster.get_node(&node_id).await {
                alive.push((node_id, info.addr));
            }
        }

        // Also check local storage — the chunk may exist here but not be in metadata
        // (e.g. node ID was incorrectly pruned by a previous healer bug).
        let mut local_added_via_fallback = false;
        if !alive.iter().any(|(id, _)| *id == local_id) && storage.has_chunk(chunk_id) {
            if let Some(info) = cluster.get_node(&local_id).await {
                warn!(
                    "Chunk {} found in local storage but missing from confirmed-alive list — adding as source",
                    chunk_id
                );
                alive.push((local_id, info.addr));
                local_added_via_fallback = true;
            }
        }

        if alive.is_empty() {
            anyhow::bail!("No alive nodes have chunk {}", chunk_id);
        }

        // `alive` (including any verified local-fallback copy) is exactly the node set
        // that will end up holding this chunk once healing completes, so count all of
        // it toward replica_count. Chunk IDs are content hashes, so a fallback copy
        // confirmed via storage.has_chunk() is guaranteed correct, not stale — treating
        // it as "just a source" (prior behavior) under-counted replica_count by one
        // while still including it in the final node list, over-replicating to RF+1.
        let replica_count = alive.len();
        let needed = replication_factor.saturating_sub(replica_count);
        let base_nodes: Vec<NodeId> = alive.iter().map(|(id, _)| *id).collect();
        if needed == 0 {
            if local_added_via_fallback {
                // Reconcile routing table: this node holds a verified copy the routing
                // table doesn't know about. Record it now so discovery stops reporting
                // this chunk as under-replicated and the orphan sweep doesn't delete
                // this now-needed replica.
                let updated_location = ChunkLocation {
                    chunk_id: *chunk_id,
                    nodes: base_nodes,
                    size: location.size,
                    checksum: location.checksum,
                    file_offset: location.file_offset,
                    written_at: Some(Self::now_ms()),
                    client_write_seq: None,
                    file_id: location.file_id,
                };
                let meta = Arc::clone(metadata);
                let loc = updated_location.clone();
                match tokio::task::spawn_blocking(move || meta.put_chunk_location(&loc)).await {
                    Ok(Ok(())) => {
                        Self::broadcast_chunk_location_shared(&updated_location, cluster, client).await;
                    }
                    Ok(Err(e)) => warn!("Failed to reconcile chunk location for {}: {}", chunk_id, e),
                    Err(e) => warn!("put_chunk_location spawn_blocking panicked for {}: {}", chunk_id, e),
                }
            }
            Self::clear_pending_static(pending_healing, metadata, chunk_id).await;
            return Ok(());
        }

        // Select target nodes: capacity-aware candidates that don't already hold the chunk.
        // get_nodes_with_capacity_awareness returns nodes in capacity-priority order
        // (most-available first, seeded by chunk hash for determinism). Do NOT re-sort
        // by NodeId — that would throw away the capacity ordering and always pick the
        // lowest-NodeId node regardless of how full it is.
        let alive_ids: HashSet<NodeId> = alive.iter().map(|(id, _)| *id).collect();
        let candidates = cluster
            .get_nodes_with_capacity_awareness(chunk_id, replication_factor + needed)
            .await;
        let targets: Vec<NodeId> = candidates
            .into_iter()
            .filter(|n| !alive_ids.contains(n))
            .take(needed)
            .collect();

        if targets.is_empty() {
            warn!(
                "No suitable target nodes for healing chunk {} (alive={:?} replica_count={} needed={})",
                chunk_id, alive_ids, replica_count, needed
            );
            return Ok(());
        }
        debug!(
            "Healing chunk {}: alive={:?} replica_count={} needed={} targets={:?}",
            chunk_id, alive_ids, replica_count, needed, targets
        );

        // Prefer a remote source to avoid loopback TCP (leader→leader PushChunkTo hangs
        // under Tokio scheduling pressure). Fall back to local only when no remote has it.
        let source = if alive.len() > 1 {
            alive.iter().find(|(id, _)| *id != local_id).copied()
        } else {
            alive.first().copied()
        };
        let (source_id, source_addr) = source.ok_or_else(|| anyhow::anyhow!("No source node"))?;

        let mut replicated = Vec::new();

        {
            for target_id in &targets {
                if let Some(target_info) = cluster.get_node(target_id).await {
                    info!(
                        "Healing chunk {}: instructing node {} to push to node {} ({})",
                        chunk_id, source_id, target_id, target_info.addr
                    );

                    let request = Request::PushChunkTo {
                        chunk_id: *chunk_id,
                        target_addr: target_info.addr,
                        leader_id: local_id,
                    };

                    // PushChunkTo response time = source disk read + ReplicateChunk RTT
                    // to target (5s connect + 30s write + 30s read). Use 90s so the
                    // leader doesn't time out before the source handler finishes.
                    let target_addr = target_info.addr;
                    match client.send_message_timeout(
                        source_addr,
                        Message::Request(request),
                        std::time::Duration::from_secs(90),
                    ).await {
                        Ok(envelope) if matches!(envelope.message, Message::Response(Response::Ok { .. })) => {
                            // Verify the target actually received and can confirm the chunk.
                            // PushChunkTo's Ok only means the source completed the send —
                            // a target with a corrupted filesystem may write OK but fail
                            // reads immediately after. A quick HasChunks round-trip to the
                            // target catches this before we commit it to routing metadata.
                            let verify_req = dfs_common::Request::HasChunks { chunk_ids: vec![*chunk_id] };
                            let target_confirmed = match client.send_message(
                                target_addr,
                                dfs_common::Message::Request(verify_req),
                            ).await {
                                Ok(env) => matches!(
                                    env.message,
                                    dfs_common::Message::Response(dfs_common::Response::BoolVec { ref values })
                                    if values.first().copied().unwrap_or(false)
                                ),
                                Err(e) => {
                                    warn!("Chunk {} post-push verification on target {} failed: {}", chunk_id, target_addr, e);
                                    false
                                }
                            };
                            if target_confirmed {
                                info!("Chunk {} successfully pushed from {} to {} (target confirmed)", chunk_id, source_id, target_id);
                                replicated.push(*target_id);
                            } else {
                                warn!("Chunk {} push to {} reported success but target {} does not confirm having it — not recording replica", chunk_id, source_id, target_id);
                            }
                        }
                        Ok(envelope) if matches!(envelope.message, Message::Response(Response::Error { code: dfs_common::ErrorCode::NotFound, .. })) => {
                            warn!("Chunk {} not found on source {} — removing from heal queue", chunk_id, source_id);
                            let _ = metadata.delete_chunk_location_async(*chunk_id).await;
                            Self::clear_pending_static(pending_healing, metadata, chunk_id).await;
                            return Ok(());
                        }
                        Ok(envelope) => {
                            warn!("Chunk {} push from {} to {} failed: {:?}", chunk_id, source_id, target_id, envelope.message);
                        }
                        Err(e) => {
                            warn!("Chunk {} push from {} to {} error: {}", chunk_id, source_id, target_id, e);
                        }
                    }
                } else {
                    warn!("Chunk {}: target node {} not found in cluster registry — skipping", chunk_id, target_id);
                }
            }
        }

        if !replicated.is_empty() {
            info!("Healed chunk {}: added {} replicas", chunk_id, replicated.len());

            // CAS-style freshness check: verify the source node still holds the chunk
            // before broadcasting the healed replica into the chunk map. Between our scan
            // and now, a client write may have patched the source: it renames chunk_id
            // to a new hash, so HasChunks(chunk_id) returns false on the source. If the
            // source lost the chunk, our healed copy on the target is a stale version —
            // don't add it to metadata. The orphan sweep will clean up the target's copy,
            // and the healer will heal the correct (newer) chunk_id on the next cycle.
            // This keeps the chunk map free of stale replicas and prevents the client's
            // sorted-first-2 algorithm from targeting a node with an old base chunk.
            let req = dfs_common::Request::HasChunks { chunk_ids: vec![*chunk_id] };
            let source_still_valid = match client.send_message(source_addr, dfs_common::Message::Request(req)).await {
                Ok(env) => match env.message {
                    dfs_common::Message::Response(dfs_common::Response::BoolVec { ref values }) => {
                        values.first().copied().unwrap_or(false)
                    }
                    _ => {
                        warn!("Healer: unexpected response verifying chunk {} on source {}", chunk_id, source_addr);
                        false
                    }
                },
                Err(e) => {
                    warn!("Healer: could not verify chunk {} on source {}: {}", chunk_id, source_addr, e);
                    false
                }
            };

            if !source_still_valid {
                info!("Healer: chunk {} no longer on source {} (superseded by a newer write) — discarding healed replica", chunk_id, source_addr);
                Self::clear_pending_static(pending_healing, metadata, chunk_id).await;
                return Ok(());
            }

            let mut updated_nodes = base_nodes;
            updated_nodes.extend(replicated);

            let updated_location = ChunkLocation {
                chunk_id: *chunk_id,
                nodes: updated_nodes,
                size: location.size,
                checksum: location.checksum,
                file_offset: location.file_offset,
                written_at: Some(Self::now_ms()),
                client_write_seq: None,
                file_id: location.file_id,
            };

            let meta = Arc::clone(metadata);
            let loc = updated_location.clone();
            let store_result = tokio::task::spawn_blocking(move || meta.put_chunk_location(&loc)).await;
            match store_result {
                Ok(Ok(())) => {
                    Self::broadcast_chunk_location_shared(&updated_location, cluster, client).await;
                }
                Ok(Err(e)) => warn!("Failed to update chunk location after healing {}: {}", chunk_id, e),
                Err(e) => warn!("put_chunk_location spawn_blocking panicked for {}: {}", chunk_id, e),
            }

            Self::clear_pending_static(pending_healing, metadata, chunk_id).await;
        }

        Ok(())
    }

    /// Broadcast an updated chunk location to all online peers.
    async fn broadcast_chunk_location(&self, location: &ChunkLocation) {
        Self::broadcast_chunk_location_shared(location, &self.cluster, &self.client).await;
    }

    async fn broadcast_chunk_location_shared(
        location: &ChunkLocation,
        cluster: &Arc<ClusterManager>,
        client: &Arc<NetworkClient>,
    ) {
        let nodes = cluster.get_all_nodes().await;
        let local_id = cluster.local_node_id();
        for node in nodes {
            if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                continue;
            }
            // Include file_id so the leader routes through chunk_map_update_location_for_file
            // (with write_seq guard) instead of the scan-all fallback that only does an
            // exact chunk_id match and can't enforce ordering between a stale heal RCL
            // and a newer client patch RCL that already updated the same chunk slot.
            let request = Request::ReplicateChunkLocation { location: location.clone(), file_id: location.file_id };
            if let Err(e) = client.send_message(node.addr, Message::Request(request)).await {
                warn!("Failed to broadcast chunk location {} to node {}: {}", location.chunk_id, node.id, e);
            }
        }
    }

    /// Over-replication cleanup with deterministic pair preference.
    ///
    /// Trims one excess replica per cycle. Prefers to remove nodes outside the
    /// deterministic pair (lowest 2 NodeIds among confirmed-alive nodes), using
    /// disk utilization as a tiebreaker within each group.
    ///
    /// Hash integrity verification is NOT done here — that is too expensive for
    /// background bulk trimming (reads 4MB from every node per chunk, floods I/O).
    /// Use `dfs-admin file repair` for explicit integrity checking.
    async fn do_cleanup_excess_shared(
        chunk_id: &ChunkId,
        confirmed_alive_nodes: Vec<NodeId>,
        storage: &Arc<ChunkStorage>,
        metadata: &Arc<MetadataStore>,
        cluster: &Arc<ClusterManager>,
        client: &Arc<NetworkClient>,
        replication_factor: usize,
    ) -> Result<()> {
        let location = metadata
            .get_chunk_location(chunk_id)?
            .ok_or_else(|| anyhow::anyhow!("Chunk location not found"))?;

        // Resolve confirmed-alive nodes to addresses.
        let mut alive: Vec<(NodeId, std::net::SocketAddr)> = Vec::new();
        for node_id in &confirmed_alive_nodes {
            if let Some(info) = cluster.get_node(node_id).await {
                alive.push((*node_id, info.addr));
            }
        }

        let excess = alive.len().saturating_sub(replication_factor);
        if excess == 0 {
            return Ok(());
        }

        // Deterministic pair: lowest 2 NodeIds among confirmed-alive nodes.
        // Prefer trimming non-pair nodes (highest utilization first).
        // Fall back to pair nodes if all excess are pair nodes.
        let local_id = cluster.local_node_id();
        let mut sorted_alive: Vec<NodeId> = alive.iter().map(|(id, _)| *id).collect();
        sorted_alive.sort_unstable();
        let pair: std::collections::HashSet<NodeId> = sorted_alive.iter().take(2).copied().collect();

        let non_pair_ids: Vec<NodeId> = alive.iter()
            .map(|(id, _)| *id)
            .filter(|id| !pair.contains(id))
            .collect();

        let trim_id = if !non_pair_ids.is_empty() {
            cluster.most_utilized_node(&non_pair_ids).await
                .unwrap_or(non_pair_ids[non_pair_ids.len() - 1])
        } else {
            let all_ids: Vec<NodeId> = alive.iter().map(|(id, _)| *id).collect();
            cluster.most_utilized_node(&all_ids).await
                .unwrap_or(all_ids[all_ids.len() - 1])
        };

        let (_, trim_addr) = match alive.iter().find(|(id, _)| *id == trim_id) {
            Some(v) => *v,
            None => return Ok(()),
        };

        // The trim target can be the leader itself (it's just another alive replica
        // holder) — delete locally instead of opening a loopback TCP connection to
        // our own listener to tell ourselves to delete a chunk.
        if trim_id == local_id {
            let storage = storage.clone();
            let chunk_id_owned = *chunk_id;
            let delete_result = tokio::task::spawn_blocking(move || storage.delete_chunk(&chunk_id_owned)).await;
            match delete_result {
                Ok(Ok(_)) => {
                    info!("Chunk {} over-replicated ({} alive, RF={}): trimmed local node {} (pair={:?})",
                          chunk_id, alive.len(), replication_factor, trim_id, pair);
                }
                Ok(Err(e)) => {
                    warn!("Failed to locally trim chunk {}: {}", chunk_id, e);
                    return Ok(());
                }
                Err(e) => {
                    warn!("Local trim of chunk {} panicked: {}", chunk_id, e);
                    return Ok(());
                }
            }
        } else {
            let req = Request::DeleteChunkReplica { chunk_id: *chunk_id, leader_id: local_id };
            match client.send_message(trim_addr, Message::Request(req)).await {
                Ok(env) if matches!(env.message, Message::Response(Response::Ok { .. })) => {
                    info!("Chunk {} over-replicated ({} alive, RF={}): trimmed node {} (pair={:?})",
                          chunk_id, alive.len(), replication_factor, trim_id, pair);
                }
                Ok(env) => {
                    warn!("Node {} refused to trim chunk {}: {:?}", trim_id, chunk_id, env.message);
                    return Ok(());
                }
                Err(e) => {
                    warn!("Failed to reach node {} for chunk {} trim: {}", trim_id, chunk_id, e);
                    return Ok(());
                }
            }
        }

        let removed_nodes = vec![trim_id];

        // Update metadata to remove all deleted nodes (corrupt + trim).
        if !removed_nodes.is_empty() {
            let updated_nodes: Vec<NodeId> = location.nodes.iter()
                .filter(|n| !removed_nodes.contains(n))
                .copied()
                .collect();
            let heal_ts = Self::now_ms();
            let updated_location = ChunkLocation {
                chunk_id: *chunk_id,
                nodes: updated_nodes,
                size: location.size,
                checksum: location.checksum,
                file_offset: location.file_offset,
                written_at: Some(heal_ts),
                client_write_seq: None,
                file_id: location.file_id,
            };
            let meta = Arc::clone(metadata);
            let loc = updated_location.clone();
            let store_result = tokio::task::spawn_blocking(move || meta.put_chunk_location(&loc)).await;
            match store_result {
                Ok(Ok(())) => {
                    Self::broadcast_chunk_location_shared(&updated_location, cluster, client).await;
                    info!("Excess replica cleanup complete for chunk {} ({} node(s) removed)",
                          chunk_id, removed_nodes.len());
                }
                Ok(Err(e)) => warn!("Failed to update chunk location after cleanup of {}: {}", chunk_id, e),
                Err(e) => warn!("put_chunk_location spawn_blocking panicked for {}: {}", chunk_id, e),
            }
        }

        Ok(())
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// Scrub all chunks (verify checksums)
    async fn scrub_all_chunks(&self) -> Result<()> {
        // Chunks whose on-disk mtime is more than MTIME_TOLERANCE_SECS newer than
        // ChunkLocation.written_at were likely overwritten after the original write
        // (e.g. a crash during PatchChunk). Only verify hash on those suspicious chunks,
        // avoiding a full re-hash of every chunk on every scrub cycle.
        const MTIME_TOLERANCE_SECS: u64 = 300;

        let chunks = self.storage.list_chunks()?;
        info!("Scrubbing {} chunks (mtime heuristic)", chunks.len());

        let mut skipped = 0usize;
        let mut verified = 0usize;
        let mut errors = 0usize;

        for chunk_id in chunks {
            let mtime = self.storage.get_chunk_mtime(&chunk_id);
            let written_at = self.metadata
                .get_chunk_location(&chunk_id)
                .ok()
                .flatten()
                .and_then(|loc| loc.written_at);

            let suspicious = match (mtime, written_at) {
                (Some(mt), Some(wa)) => mt > wa.saturating_add(MTIME_TOLERANCE_SECS),
                // No written_at recorded (old chunk) — always verify
                (Some(_), None) => true,
                // Can't stat the file — verify so we detect missing chunks
                (None, _) => true,
            };

            if !suspicious {
                skipped += 1;
                continue;
            }

            match self.storage.read_and_verify_chunk(&chunk_id) {
                Ok(_) => {
                    verified += 1;
                }
                Err(e) => {
                    warn!("Scrub hash mismatch for chunk {} (mtime={:?} written_at={:?}): {}",
                        chunk_id, mtime, written_at, e);
                    errors += 1;
                    self.mark_pending(chunk_id).await;
                }
            }
        }

        info!(
            "Scrubbing complete: skipped={}, verified={}, errors={}",
            skipped, verified, errors
        );

        Ok(())
    }

    /// Queue a specific set of chunks for immediate healing, bypassing the normal delay.
    /// Used by HealFile to force-heal all chunks of a single file for targeted testing.
    /// Remove a chunk from the healer's pending queue. Used by PatchChunk to prevent
    /// the healer from replicating the old chunk file during the rename window.
    pub async fn evict_from_pending(&self, chunk_id: &ChunkId) {
        self.clear_pending(chunk_id).await;
        self.stalled_healing.write().await.remove(chunk_id);
    }

    /// This node's outbound heal-bandwidth limiter — used by Server::handle_push_chunk_to
    /// to pace the actual disk-read+network-send of healed chunk data.
    pub fn heal_bandwidth_limiter(&self) -> &Arc<BandwidthLimiter> {
        &self.heal_bandwidth_limiter
    }

    pub async fn queue_chunks_immediate(&self, chunk_ids: Vec<ChunkId>) {
        let backdated = Instant::now() - Duration::from_secs(self.healing_delay_secs + 1);
        let backdated_secs = dfs_common::types::current_timestamp()
            .saturating_sub(self.healing_delay_secs + 1);
        let mut pending = self.pending_healing.write().await;
        let mut cache  = self.alive_nodes_cache.write().await;
        for chunk_id in chunk_ids {
            pending.insert(chunk_id, backdated);
            if let Err(err) = self.metadata.put_pending_healing_async(chunk_id, backdated_secs).await {
                warn!("Failed to persist backdated pending_healing entry for {}: {}", chunk_id, err);
            }
            // Invalidate any stale cache entry for this chunk so drain_heal_queue
            // doesn't classify a healthy RF=3 chunk as under-replicated based on
            // old alive-node data from a previous healing cycle. The next discovery
            // pass will repopulate the cache with a fresh HasChunks scan.
            cache.remove(&chunk_id);
        }
    }

    /// Get healing statistics
    pub async fn get_stats(&self) -> HealingStats {
        let pending   = self.pending_healing.read().await;
        let in_flight = self.in_flight_healing.read().await;
        let stalled   = self.stalled_healing.read().await;

        // pending count excludes stalled (they're a subset of pending_healing keys
        // but have no source — not actionable by the heal loop right now).
        let stalled_count = stalled.len();
        let pending_count = pending.len().saturating_sub(stalled_count);

        HealingStats {
            pending_healing: pending_count,
            in_flight_healing: in_flight.len(),
            stalled_healing: stalled_count,
            auto_heal_enabled: self.auto_heal,
            healing_delay_secs: self.healing_delay_secs,
            current_bandwidth_mb: self.heal_bandwidth_limiter.current_rate_mb().await,
        }
    }

    /// Trigger an immediate heal cycle, bypassing the 60s interval.
    /// Runs check_and_heal directly on the calling task. Only has effect on the leader;
    /// non-leaders log and return immediately (same behaviour as the periodic loop).
    pub async fn trigger_heal_now(&self) -> Result<()> {
        if !self.cluster.is_leader().await {
            info!("TriggerHealing received on non-leader node — ignoring");
            return Ok(());
        }
        info!("Manual heal cycle triggered");
        // Use a deep scan if there's nothing in pending_healing — fast scan would
        // find nothing since it only checks already-known chunks. Deep scan finds
        // all under-replicated chunks in the routing table regardless of whether
        // they've been seen before. Fast scan is used when pending is non-empty.
        let pending_count = self.pending_healing.read().await.len();
        let deep = pending_count == 0;
        if deep {
            info!("Manual heal cycle: pending queue empty — running deep scan to discover new under-replicated chunks");
        }
        self.run_discovery_pass(deep).await?;
        self.drain_heal_queue().await
    }
}

/// Replication status of a chunk
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplicationStatus {
    Ok,
    UnderReplicated,
    OverReplicated,
}

/// Healing statistics
#[derive(Debug, Clone)]
pub struct HealingStats {
    pub pending_healing: usize,
    pub in_flight_healing: usize,
    pub stalled_healing: usize,
    pub auto_heal_enabled: bool,
    pub healing_delay_secs: u64,
    pub current_bandwidth_mb: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::ClusterManager;
    use crate::metadata::MetadataStore;
    use crate::storage::ChunkStorage;
    use dfs_common::compute_chunk_hash;
    use std::net::SocketAddr;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_healing_manager_creation() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));
        let client = Arc::new(NetworkClient::new());

        let healing = HealingManager::new(storage, metadata, cluster, client, 3, 300, 24, true, Arc::new(DashMap::new()));

        let stats = healing.get_stats().await;
        assert_eq!(stats.pending_healing, 0);
        assert!(stats.auto_heal_enabled);
        assert_eq!(stats.healing_delay_secs, 300);
    }

    #[tokio::test]
    async fn test_should_heal_with_delay() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));
        let client = Arc::new(NetworkClient::new());

        let healing = HealingManager::new(storage, metadata, cluster, client, 3, 2, 24, true, Arc::new(DashMap::new())); // 2s delay

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"test"));

        // First check - should return false and mark for healing
        assert!(!healing.should_heal(&chunk_id).await);

        // Still within delay
        assert!(!healing.should_heal(&chunk_id).await);

        // Wait for delay
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Now should heal
        assert!(healing.should_heal(&chunk_id).await);
    }

    /// A chunk's pending-healing "first detected" time must survive a process
    /// restart: HealingManager::new seeds pending_healing from the persisted
    /// PENDING_HEALING_TABLE, so the healing_delay_secs debounce reflects total
    /// elapsed time (pre- and post-restart), not just time-since-this-process-started.
    #[tokio::test]
    async fn test_pending_healing_survives_restart() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));
        let client = Arc::new(NetworkClient::new());

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"restart-test"));

        // Simulate a chunk a *prior process* detected as needing healing 8s ago.
        let detected_at_secs = dfs_common::types::current_timestamp() - 8;
        metadata.put_pending_healing(&chunk_id, detected_at_secs).unwrap();

        // Construct a fresh HealingManager against the same MetadataStore —
        // simulates a process restart. healing_delay_secs = 10, so the 8s that
        // already elapsed before restart must carry over.
        let healing = HealingManager::new(storage, metadata, cluster, client, 3, 10, 24, true, Arc::new(DashMap::new()));

        // 8s < 10s — not ready yet, but the persisted age must have been restored
        // (not reset to 0s on restart).
        assert!(!healing.should_heal(&chunk_id).await);

        // 8s (carried over) + 3s (elapsed since restart) = 11s >= 10s.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(healing.should_heal(&chunk_id).await);
    }

    fn make_healing(node_id: NodeId, addr: SocketAddr) -> (Arc<ChunkStorage>, Arc<MetadataStore>, HealingManager, TempDir, TempDir) {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();
        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));
        let client = Arc::new(NetworkClient::new());
        let healing = HealingManager::new(storage.clone(), metadata.clone(), cluster, client, 3, 300, 24, true, Arc::new(DashMap::new()));
        (storage, metadata, healing, temp_storage, temp_metadata)
    }

    /// Single-node cluster: this node is trivially its own leader with no peers to
    /// check, so the stability gate has nothing to fail on — every candidate must be
    /// authorized.
    #[tokio::test]
    async fn test_authorize_live_file_orphan_deletes_leader_no_peers_authorizes_all() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (_storage, _metadata, healing, _t1, _t2) = make_healing(node_id, addr);

        let candidates = vec![
            ChunkId::from_hash(compute_chunk_hash(b"a")),
            ChunkId::from_hash(compute_chunk_hash(b"b")),
        ];
        let authorized = healing.authorize_live_file_orphan_deletes(&candidates).await;
        assert_eq!(authorized.len(), 2, "no peers to fail the stability check — must authorize everything");
    }

    /// Leader with an unreachable peer: the stability check cannot confirm the peer
    /// has been up long enough, so nothing may be deleted this cycle. This is the
    /// core split-brain guard — when in doubt, defer instead of deleting.
    #[tokio::test]
    async fn test_authorize_live_file_orphan_deletes_leader_unreachable_peer_defers() {
        let (id_a, id_b) = {
            let a = NodeId::new();
            let b = NodeId::new();
            if a < b { (a, b) } else { (b, a) }
        };
        let local_addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (_storage, _metadata, healing, _t1, _t2) = make_healing(id_a, local_addr);

        // id_a < id_b, so id_a (local) is the leader by construction (min NodeId).
        // Peer address is unreachable — GetNodeStats must fail/timeout.
        let peer_addr: SocketAddr = "127.0.0.1:19999".parse().unwrap();
        healing.cluster.add_node(dfs_common::NodeInfo::new(id_b, peer_addr, None)).await.unwrap();
        assert!(healing.cluster.is_leader().await, "local node (min id) must be leader");

        let candidates = vec![ChunkId::from_hash(compute_chunk_hash(b"c"))];
        let authorized = healing.authorize_live_file_orphan_deletes(&candidates).await;
        assert!(authorized.is_empty(), "unreachable peer must defer the whole batch, not authorize anything");
    }

    /// Non-leader node with an unreachable leader: ConfirmChunksLive cannot be
    /// answered, so nothing may be deleted this cycle.
    #[tokio::test]
    async fn test_authorize_live_file_orphan_deletes_follower_unreachable_leader_defers() {
        let (id_a, id_b) = {
            let a = NodeId::new();
            let b = NodeId::new();
            if a < b { (a, b) } else { (b, a) }
        };
        // Local node is id_b (NOT the minimum) — id_a is the leader.
        let local_addr: SocketAddr = "127.0.0.1:8901".parse().unwrap();
        let (_storage, _metadata, healing, _t1, _t2) = make_healing(id_b, local_addr);

        let leader_addr: SocketAddr = "127.0.0.1:19998".parse().unwrap(); // unreachable
        healing.cluster.add_node(dfs_common::NodeInfo::new(id_a, leader_addr, None)).await.unwrap();
        assert!(!healing.cluster.is_leader().await, "local node (not min id) must not be leader");

        let candidates = vec![ChunkId::from_hash(compute_chunk_hash(b"d"))];
        let authorized = healing.authorize_live_file_orphan_deletes(&candidates).await;
        assert!(authorized.is_empty(), "unreachable leader must defer, never authorize blindly");
    }

    /// End-to-end: a chunk still routed to us in the local CHUNK_TABLE, but with no
    /// live file referencing it (a patch-superseded chunk the inline fast-evict
    /// missed), must survive the first sighting (two-pass guard) and only be evicted
    /// on a second pass once it's both old enough and authorized.
    #[tokio::test]
    async fn test_disk_orphan_sweep_evicts_live_file_orphan_after_two_pass_when_authorized() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (storage, metadata, healing, _t1, _t2) = make_healing(node_id, addr);

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"live-file-orphan-test"));
        storage.write_chunk(&chunk_id, b"some data").unwrap();
        let old_ts = dfs_common::types::current_timestamp().saturating_sub(700);
        storage.set_chunk_mtime(&chunk_id, old_ts);

        // Still routed to us, but no file metadata references it at all.
        metadata.put_chunk_location(&ChunkLocation {
            chunk_id,
            nodes: vec![node_id],
            size: 9,
            checksum: chunk_id.hash,
            file_offset: Some(0),
            written_at: Some(old_ts * 1000),
            client_write_seq: None,
            file_id: None,
        }).unwrap();

        healing.run_disk_orphan_sweep().await;
        assert!(storage.get_chunk_path(&chunk_id).exists(), "must survive first sighting (two-pass guard)");

        healing.run_disk_orphan_sweep().await;
        assert!(!storage.get_chunk_path(&chunk_id).exists(), "must be evicted on second pass once old enough and authorized");
    }

    /// A live-file-orphan candidate younger than LIVE_FILE_GRACE_SECS must never be
    /// deleted, even across many sweep passes — age is checked every pass, not just
    /// before the first sighting.
    #[tokio::test]
    async fn test_disk_orphan_sweep_never_evicts_recent_live_file_orphan() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (storage, metadata, healing, _t1, _t2) = make_healing(node_id, addr);

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"too-recent-orphan"));
        storage.write_chunk(&chunk_id, b"some data").unwrap();
        // mtime left at "now" — well within the 600s grace window.

        metadata.put_chunk_location(&ChunkLocation {
            chunk_id,
            nodes: vec![node_id],
            size: 9,
            checksum: chunk_id.hash,
            file_offset: Some(0),
            written_at: Some(dfs_common::types::current_timestamp() * 1000),
            client_write_seq: None,
            file_id: None,
        }).unwrap();

        for _ in 0..3 {
            healing.run_disk_orphan_sweep().await;
        }
        assert!(storage.get_chunk_path(&chunk_id).exists(), "must never evict a candidate still inside the age grace");
    }
}
