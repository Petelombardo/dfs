use anyhow::{Context, Result};
use dfs_common::{ChunkId, ChunkLocation, Message, NodeId, Request, Response};
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

/// Healing manager - monitors and repairs chunk replication
/// Optimized for SBC environments (batched operations, configurable intervals)
pub struct HealingManager {
    /// Local storage
    storage: Arc<ChunkStorage>,

    /// Metadata store
    metadata: Arc<MetadataStore>,

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

    /// Byte-budget semaphore — limits total bytes in-flight across all concurrent
    /// heal transfers. Each task acquires chunk_size permits before sending and
    /// releases them on completion, naturally throttling without fixed concurrency
    /// caps or inter-batch sleeps.
    heal_semaphore: Arc<Semaphore>,

    /// Total permit capacity of heal_semaphore (bytes). Stored separately so tasks
    /// can clamp their acquisition to the full budget without racing on available_permits().
    heal_semaphore_capacity: usize,

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
    ) -> Self {
        let max_heal_per_cycle = 200;

        // Byte-budget semaphore: cap total bytes in-flight across all concurrent
        // heal transfers. Sized via DFS_HEAL_BANDWIDTH_MB (default 32MB).
        // At 4MB chunks this allows up to 8 concurrent transfers; smaller end-of-segment
        // chunks let more through automatically. No fixed concurrency cap needed —
        // the semaphore self-tunes to actual chunk sizes.
        let heal_bw_mb = std::env::var("DFS_HEAL_BANDWIDTH_MB")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(32);
        let heal_semaphore_capacity = heal_bw_mb * 1024 * 1024;
        let heal_semaphore = Arc::new(Semaphore::new(heal_semaphore_capacity));

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
            cluster,
            client,
            replication_factor,
            healing_delay_secs,
            scrub_interval_hours,
            auto_heal,
            max_heal_per_cycle,
            heal_semaphore,
            heal_semaphore_capacity,
            pending_healing: Arc::new(RwLock::new(pending_healing_map)),
            in_flight_healing: Arc::new(RwLock::new(HashSet::new())),
            alive_nodes_cache: Arc::new(RwLock::new(HashMap::new())),
            stalled_healing: Arc::new(RwLock::new(HashSet::new())),
            orphan_candidates: Arc::new(RwLock::new(HashSet::new())),
            heal_transfer_timeout_secs,
        }
    }

    /// Start background healing tasks
    pub async fn start(self: Arc<Self>) {
        if !self.auto_heal {
            info!("Auto-healing is disabled");
            return;
        }

        let heal_bw_mb = self.heal_semaphore_capacity / (1024 * 1024);
        info!(
            "Starting healing manager (delay: {}s, scrub: {}h, max_per_cycle: {}, bandwidth_budget: {}MB, transfer_timeout: {}s)",
            self.healing_delay_secs, self.scrub_interval_hours, self.max_heal_per_cycle,
            heal_bw_mb, self.heal_transfer_timeout_secs
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
            if let Err(err) = self.metadata.put_pending_healing(&chunk_id, dfs_common::types::current_timestamp()) {
                warn!("Failed to persist pending_healing entry for {}: {}", chunk_id, err);
            }
        }
    }

    /// Remove `chunk_id` from pending_healing (it reached RF, was purged, or is no
    /// longer relevant) and clear its persisted detection time.
    async fn clear_pending(&self, chunk_id: &ChunkId) {
        Self::clear_pending_static(&self.pending_healing, &self.metadata, chunk_id).await;
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
            if let Err(err) = metadata.delete_pending_healing(chunk_id) {
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
    /// Disk-level orphan sweep — runs on every node every 5 minutes.
    ///
    /// Walks the local chunk directory and deletes any chunk file whose chunk_id is
    /// absent from the local routing table. This catches the case where a node missed
    /// DeleteChunk RPCs while offline: the routing table entries are gone (cleaned up
    /// by the leader while the node was absent), but the physical files remain.
    ///
    /// Uses a 5-minute grace period to avoid deleting chunks that were just written
    /// but whose routing table entries haven't been committed yet.
    async fn run_disk_orphan_sweep(&self) {
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

        let storage = self.storage.clone();
        let metadata = self.metadata.clone();
        let local_node_id = self.cluster.local_node_id();

        let result = tokio::task::spawn_blocking(move || {
            let chunks = storage.list_chunks()?;
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let mut deleted = 0usize;
            let mut kept = 0usize;
            let mut too_recent = 0usize;

            for chunk_id in &chunks {
                // Determine whether this local file is still our responsibility.
                // The routing table is cluster-wide, so Ok(Some(loc)) only means
                // the chunk exists somewhere — we must also verify this node is
                // still listed.  A stale local copy where loc.nodes = [other, nodes]
                // is an orphan that DeleteChunk RPCs should have cleaned up but didn't.
                let is_ours = match metadata.get_chunk_location(chunk_id) {
                    Ok(Some(loc)) => loc.nodes.contains(&local_node_id),
                    Ok(None) => false,
                    Err(e) => {
                        debug!("Disk orphan sweep: routing table error for {}: {}", chunk_id, e);
                        continue;
                    }
                };

                if is_ours {
                    kept += 1;
                    continue;
                }

                // Not our chunk (missing entry or our node not listed).
                // Apply grace period so we don't race with in-flight writes that
                // haven't had their routing entry committed yet.
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
            }

            Ok::<_, anyhow::Error>((deleted, kept, too_recent, chunks.len()))
        }).await;

        match result {
            Ok(Ok((deleted, kept, too_recent, total))) => {
                if deleted > 0 || too_recent > 0 {
                    info!("Disk orphan sweep: {} local chunks checked — {} orphans deleted, {} kept (legitimately ours), {} too recent",
                          total, deleted, kept, too_recent);
                } else {
                    debug!("Disk orphan sweep: {} chunks checked, all accounted for", total);
                }
            }
            Ok(Err(e)) => warn!("Disk orphan sweep error: {}", e),
            Err(e) => warn!("Disk orphan sweep panicked: {}", e),
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
            /// live_chunk_ids from file records (deep scan only, else None).
            live_chunks: Option<HashSet<ChunkId>>,
            /// All chunk IDs from routing table (deep scan only, for orphan diff).
            /// Paired with written_at (ms since epoch) for the grace-period check.
            all_chunk_ids: Option<Vec<(ChunkId, Option<u64>)>>,
        }

        // Fast path: no sled scan at all. We fetch locations only for chunks already
        // in pending_healing — O(pending) individual lookups, not O(all_chunks).
        // Deep path: full streaming sled scan for orphan detection and finding under-RF
        // chunks that haven't entered pending_healing yet.
        let scan_result = if !deep {
            // Fast: look up only pending chunks by ID.
            let mut chunks_to_check = Vec::with_capacity(pending_snapshot.len());
            for chunk_id in &pending_snapshot {
                if let Ok(Some(loc)) = self.metadata.get_chunk_location(chunk_id) {
                    chunks_to_check.push(loc);
                }
            }
            ScanResult { chunks_to_check, live_chunks: None, all_chunk_ids: None }
        } else {
            tokio::task::spawn_blocking(move || {
                let live = Some(metadata_scan.live_chunk_ids()?);

                let mut chunks_to_check = Vec::new();
                let mut all_chunk_ids_for_orphan_check: Vec<(ChunkId, Option<u64>)> = Vec::new();

                metadata_scan.scan_chunk_locations(|loc| {
                    all_chunk_ids_for_orphan_check.push((loc.chunk_id, loc.written_at));
                    // Deep: include all live chunks for HasChunks verification.
                    if let Some(ref live_set) = live {
                        if live_set.contains(&loc.chunk_id) {
                            chunks_to_check.push(loc);
                        }
                    }
                    true
                })?;

                Ok::<_, anyhow::Error>(ScanResult {
                    chunks_to_check,
                    live_chunks: live,
                    all_chunk_ids: Some(all_chunk_ids_for_orphan_check),
                })
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

        let ScanResult {
            chunks_to_check: mut chunks_to_check,
            live_chunks: live_chunks_opt,
            all_chunk_ids: all_chunk_ids_opt,
        } = scan_result;

        // work carries (chunk_id, status, confirmed_alive_node_ids) from the bulk scan.
        let mut work: Vec<(ChunkId, ReplicationStatus, Vec<NodeId>)> = Vec::new();
        let mut pending_count = 0;
        let mut orphan_count = 0;
        let mut purged_orphans: Vec<ChunkId> = Vec::new();
        let mut new_candidates: HashSet<ChunkId> = HashSet::new();

        // Pending DB writes — applied in a single spawn_blocking after all classification.
        // Direct calls to metadata.put_chunk_location() / delete_chunk_location() call
        // redb's begin_write() which blocks the OS thread. Under a heal storm with many
        // concurrent async tasks all hitting begin_write(), every Tokio worker thread can
        // end up blocked on the mutex, freezing the entire async runtime.
        let mut db_puts: Vec<ChunkLocation> = Vec::new();
        let mut db_deletes: Vec<ChunkId> = Vec::new();
        // Physical file deletions for orphaned chunks — applied in the same spawn_blocking.
        let mut disk_deletes: Vec<ChunkId> = Vec::new();

        // --- Orphan detection (deep scan only) ---
        if let (Some(ref live_chunks), Some(all_chunk_ids)) = (live_chunks_opt.as_ref(), all_chunk_ids_opt) {
            // Grace period: chunks written within this window are never touched by the
            // orphan purge, even if they're absent from live file metadata.  The META
            // QUEUE can lag many minutes behind chunk writes on large files, so a short
            // two-pass guard (2 × 5 min) is not sufficient.  30 minutes comfortably
            // covers any realistic META QUEUE backlog.
            const ORPHAN_GRACE_SECS: u64 = 1800;
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;

            // Snapshot prev_candidates and immediately drop the read lock so we can
            // take the write lock at the end of this block without deadlocking.
            let prev_candidates: HashSet<ChunkId> = self.orphan_candidates.read().await.clone();
            for (chunk_id, written_at_ms) in all_chunk_ids {
                if !live_chunks.contains(&chunk_id) {
                    // Skip chunks that were recently registered in the routing table —
                    // their file metadata may still be in transit through the META QUEUE.
                    let age_secs = written_at_ms
                        .map(|ts| now_ms.saturating_sub(ts) / 1000)
                        .unwrap_or(u64::MAX);
                    if age_secs < ORPHAN_GRACE_SECS {
                        debug!("Orphan grace: skipping {} (written {}s ago, grace={}s)", chunk_id, age_secs, ORPHAN_GRACE_SECS);
                        continue;
                    }

                    if !destructive_allowed {
                        debug!("Skipping orphan purge for {} — cluster degraded", chunk_id);
                        self.clear_pending(&chunk_id).await;
                        continue;
                    }
                    if prev_candidates.contains(&chunk_id) {
                        debug!("Purging orphaned chunk location record and physical file: {}", chunk_id);
                        // Deferred: collected into db_deletes/disk_deletes and applied in
                        // spawn_blocking below. Optimistically add to purged_orphans for
                        // broadcasting — if the db delete fails it logs a warning and
                        // the purge is retried next cycle.
                        db_deletes.push(chunk_id);
                        disk_deletes.push(chunk_id);
                        purged_orphans.push(chunk_id);
                        self.clear_pending(&chunk_id).await;
                        orphan_count += 1;
                    } else {
                        new_candidates.insert(chunk_id);
                        debug!("Orphan candidate (first sighting, will purge next pass if still absent): {}", chunk_id);
                    }
                }
            }
            *self.orphan_candidates.write().await = new_candidates;
            // Remove purged chunks from chunks_to_check.
            if !purged_orphans.is_empty() {
                let purged_set: HashSet<ChunkId> = purged_orphans.iter().copied().collect();
                chunks_to_check.retain(|loc| !purged_set.contains(&loc.chunk_id));
            }
        }

        // Broadcast orphan purges to followers (deep scan only, but only if any occurred).
        if !purged_orphans.is_empty() {
            let cluster = self.cluster.clone();
            let client = self.client.clone();
            let orphans_to_broadcast = purged_orphans.clone();
            tokio::spawn(async move {
                let nodes = cluster.get_all_nodes().await;
                for node in &nodes {
                    if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                        continue;
                    }
                    let req = Request::PurgeChunkLocations { chunk_ids: orphans_to_broadcast.clone() };
                    if let Err(e) = client.send_message(node.addr, Message::Request(req)).await {
                        debug!("Failed to batch-broadcast PurgeChunkLocations ({} chunks) to node {}: {}",
                               orphans_to_broadcast.len(), node.id, e);
                    }
                }
            });
        }

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
        if let Some(assigned) = node_assigned.get(&local_id) {
            let mut local_set = HashSet::new();
            for chunk_id in assigned {
                if self.storage.has_chunk(chunk_id) {
                    local_set.insert(*chunk_id);
                }
            }
            node_chunk_presence.insert(local_id, local_set);
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
                        warn!(
                            "DATA LOSS: Chunk {} is permanently unrecoverable ({} metadata nodes, all confirmed empty) — purging stale metadata",
                            chunk_id, location.nodes.len()
                        );
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
        if !db_puts.is_empty() || !db_deletes.is_empty() || !disk_deletes.is_empty() {
            let metadata = Arc::clone(&self.metadata);
            let storage = Arc::clone(&self.storage);
            let has_orphans = !disk_deletes.is_empty();
            let result = tokio::task::spawn_blocking(move || {
                metadata.batch_update_chunk_locations(&db_puts, &db_deletes)?;
                for chunk_id in &disk_deletes {
                    if let Err(e) = storage.delete_chunk(chunk_id) {
                        debug!("Orphan {} not on local disk (ok): {}", chunk_id, e);
                    }
                }
                if has_orphans {
                    // Compact redb after bulk orphan purge so the OS can reclaim page-cache.
                    metadata.flush()?;
                }
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

        if orphan_count > 0 {
            info!("Purged {} orphaned chunk location records", orphan_count);
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
                "Discovery complete: under={}, over={}, pending_delay={}, orphans={}",
                under_count, over_count, pending_count, orphan_count
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
                v.sort_by_key(|(chunk_id, _, _)| {
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

        // Max live tasks = semaphore_capacity / max_chunk_size (4MB).
        // This bounds how many Tokio tasks exist simultaneously waiting for the semaphore.
        // Without this cap, all `work` tasks would be spawned at once, each holding OS
        // resources (stack, file descriptors from pending connects) while blocked on
        // acquire_many — causing "too many orphaned sockets" kernel warnings under load.
        let max_live = (self.heal_semaphore_capacity / (4 * 1024 * 1024)).max(1);
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
                let heal_semaphore_capacity = self.heal_semaphore_capacity;
                let replication_factor = self.replication_factor;
                let transfer_timeout = Duration::from_secs(self.heal_transfer_timeout_secs);

                set.spawn(async move {
                    let chunk_size = metadata.get_chunk_location(&chunk_id)
                        .ok()
                        .flatten()
                        .map(|loc| loc.size)
                        .unwrap_or(4 * 1024 * 1024);

                    let permits = (chunk_size as u32).min(heal_semaphore_capacity as u32).max(1);
                    let _permit = heal_semaphore.acquire_many(permits).await;

                    let result = tokio::time::timeout(transfer_timeout, async {
                        match status {
                            ReplicationStatus::UnderReplicated => {
                                HealingManager::do_heal_chunk_shared(
                                    &chunk_id, confirmed_alive, &storage, &metadata, &cluster, &client,
                                    &pending_healing, &in_flight_healing, replication_factor,
                                ).await
                            }
                            ReplicationStatus::OverReplicated => {
                                HealingManager::do_cleanup_excess_shared(
                                    &chunk_id, confirmed_alive, &metadata, &cluster, &client, replication_factor,
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
            chunk_id, confirmed_alive_nodes, storage, metadata, cluster, client, pending_healing, replication_factor,
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
    ) -> Result<()> {
        info!("Leader healing under-replicated chunk: {}", chunk_id);

        let location = metadata
            .get_chunk_location(chunk_id)?
            .ok_or_else(|| anyhow::anyhow!("Chunk location not found"))?;

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
        // NOTE: we track whether the local node was added via fallback so we don't
        // count it toward replica_count — the routing table is authoritative for that.
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

        // Use confirmed_alive count (excluding the local fallback) for replica accounting.
        // The routing table is authoritative for how many replicas exist; the local fallback
        // is a source-of-data only and shouldn't inflate the replica count.
        let replica_count = if local_added_via_fallback {
            alive.len().saturating_sub(1)
        } else {
            alive.len()
        };
        let needed = replication_factor.saturating_sub(replica_count);
        if needed == 0 {
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
            warn!("No suitable target nodes for healing chunk {}", chunk_id);
            return Ok(());
        }

        // Pick the source node: prefer local node (no network hop for the read),
        // otherwise use the first alive node.
        let source = if alive_ids.contains(&local_id) {
            alive.iter().find(|(id, _)| *id == local_id).copied()
        } else {
            alive.first().copied()
        };
        let (source_id, source_addr) = source.ok_or_else(|| anyhow::anyhow!("No source node"))?;

        let mut replicated = Vec::new();

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

                match client.send_message(source_addr, Message::Request(request)).await {
                    Ok(envelope) if matches!(envelope.message, Message::Response(Response::Ok { .. })) => {
                        info!("Chunk {} successfully pushed from {} to {}", chunk_id, source_id, target_id);
                        replicated.push(*target_id);
                    }
                    Ok(envelope) => {
                        warn!("Chunk {} push from {} to {} failed: {:?}", chunk_id, source_id, target_id, envelope.message);
                    }
                    Err(e) => {
                        warn!("Chunk {} push from {} to {} error: {}", chunk_id, source_id, target_id, e);
                    }
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
            let source_still_valid = {
                let req = dfs_common::Request::HasChunks { chunk_ids: vec![*chunk_id] };
                match client.send_message(source_addr, dfs_common::Message::Request(req)).await {
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
                }
            };

            if !source_still_valid {
                info!("Healer: chunk {} no longer on source {} (superseded by a newer write) — discarding healed replica", chunk_id, source_addr);
                Self::clear_pending_static(pending_healing, metadata, chunk_id).await;
                return Ok(());
            }

            let mut updated_nodes: Vec<NodeId> = alive.iter().map(|(id, _)| *id).collect();
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
            let request = Request::ReplicateChunkLocation { location: location.clone(), file_id: None };
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

    pub async fn queue_chunks_immediate(&self, chunk_ids: Vec<ChunkId>) {
        let backdated = Instant::now() - Duration::from_secs(self.healing_delay_secs + 1);
        let backdated_secs = dfs_common::types::current_timestamp()
            .saturating_sub(self.healing_delay_secs + 1);
        let mut pending = self.pending_healing.write().await;
        let mut cache  = self.alive_nodes_cache.write().await;
        for chunk_id in chunk_ids {
            pending.insert(chunk_id, backdated);
            if let Err(err) = self.metadata.put_pending_healing(&chunk_id, backdated_secs) {
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

        let healing = HealingManager::new(storage, metadata, cluster, client, 3, 300, 24, true);

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

        let healing = HealingManager::new(storage, metadata, cluster, client, 3, 2, 24, true); // 2s delay

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
        let healing = HealingManager::new(storage, metadata, cluster, client, 3, 10, 24, true);

        // 8s < 10s — not ready yet, but the persisted age must have been restored
        // (not reset to 0s on restart).
        assert!(!healing.should_heal(&chunk_id).await);

        // 8s (carried over) + 3s (elapsed since restart) = 11s >= 10s.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(healing.should_heal(&chunk_id).await);
    }
}
