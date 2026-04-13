use anyhow::{Context, Result};
use dfs_common::{ChunkId, ChunkLocation, Message, NodeId, Request, Response};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Semaphore};
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::cluster::ClusterManager;
use crate::metadata::MetadataStore;
use crate::network::NetworkClient;
use crate::storage::ChunkStorage;

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

    /// Chunks pending healing (chunk_id -> failure_detected_at)
    pending_healing: Arc<RwLock<HashMap<ChunkId, Instant>>>,

    /// Chunks currently being healed by this node — prevents double-healing when
    /// multiple nodes run the healer concurrently for the same chunk.
    /// A chunk is inserted before replication begins and removed on completion.
    in_flight_healing: Arc<RwLock<HashSet<ChunkId>>>,

    /// Per-chunk confirmed-alive node list from the most recent discovery scan.
    /// Written by run_discovery_loop(); read by run_heal_loop() so the heal loop
    /// can issue PushChunkTo without waiting for a new full scan each time.
    alive_nodes_cache: Arc<RwLock<HashMap<ChunkId, Vec<NodeId>>>>,
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
            pending_healing: Arc::new(RwLock::new(HashMap::new())),
            in_flight_healing: Arc::new(RwLock::new(HashSet::new())),
            alive_nodes_cache: Arc::new(RwLock::new(HashMap::new())),
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
            "Starting healing manager (delay: {}s, scrub: {}h, max_per_cycle: {}, bandwidth_budget: {}MB)",
            self.healing_delay_secs, self.scrub_interval_hours, self.max_heal_per_cycle, heal_bw_mb
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
    /// Performs the expensive full scan: sled enumeration, bulk HasChunks RPCs,
    /// orphan purge, ghost node pruning, and classification of under/over-replicated
    /// chunks into pending_healing. Writes confirmed-alive node sets into
    /// alive_nodes_cache so the heal loop can act without waiting for this scan.
    async fn run_discovery_loop(&self) {
        let mut cleanup_counter = 0u32;
        let mut was_leader = false;

        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;

            let is_leader = self.cluster.is_leader().await;

            if is_leader != was_leader {
                if is_leader {
                    info!("This node is now the cluster leader — taking over healing coordination");
                } else {
                    info!("This node is no longer the cluster leader — yielding healing to new leader");
                }
                was_leader = is_leader;
            }

            if !is_leader {
                // Followers do not run orphan purge. The leader purges orphaned
                // chunk: records and broadcasts PurgeChunkLocation to all followers,
                // so follower DBs drain via targeted deletes rather than a local scan.
                continue;
            }

            if let Err(e) = self.run_discovery_pass().await {
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

    /// Cleanup stale entries from pending_healing map
    /// Removes chunks that no longer exist or have been pending for too long
    async fn cleanup_stale_pending(&self) -> Result<()> {
        let mut pending = self.pending_healing.write().await;
        let max_pending_time = Duration::from_secs(self.healing_delay_secs * 20); // 20x healing delay

        let mut to_remove = Vec::new();

        for (chunk_id, detected_at) in pending.iter() {
            // Remove if pending for too long (likely deleted or unrecoverable)
            if detected_at.elapsed() > max_pending_time {
                debug!("Removing stale pending healing entry for chunk {} (pending for {}s)",
                       chunk_id, detected_at.elapsed().as_secs());
                to_remove.push(*chunk_id);
                continue;
            }

            // Remove if the chunk: location record is gone — this means the file was
            // deleted (or the chunk was legitimately purged as an orphan).  There is
            // nothing left to heal regardless of whether raw chunk data still exists
            // on disk (stale data will be cleaned up separately).
            if self.metadata.get_chunk_location(chunk_id).ok().flatten().is_none() {
                debug!("Removing pending healing entry for chunk {} — no location record", chunk_id);
                to_remove.push(*chunk_id);
            }
        }

        let removed_count = to_remove.len();
        for chunk_id in to_remove {
            pending.remove(&chunk_id);
        }

        if removed_count > 0 {
            info!("Cleaned up {} stale pending healing entries", removed_count);
        }

        Ok(())
    }

    /// Run periodic scrubber
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
    /// under/over-replicated chunks into pending_healing, and updates alive_nodes_cache.
    /// Called by run_discovery_loop() every 60s. Does NOT issue any PushChunkTo —
    /// actual healing is handled by drain_heal_queue().
    async fn run_discovery_pass(&self) -> Result<()> {
        debug!("Running healing check");

        // Gate destructive operations on quorum. Under-replication healing is always
        // safe (adding replicas can't lose data), but orphan purge and over-replication
        // cleanup are irreversible — we must not run them if we can only see a minority
        // of nodes, as the majority partition may still consider that data live.
        let quorum = self.cluster.has_quorum().await;
        if !quorum {
            warn!("Leader does not have quorum — skipping destructive healing operations (orphan purge, over-replication cleanup)");
        }

        // Kick off the sled scan concurrently with the HasChunks RPCs below.
        // list_all_chunk_ids + live_chunk_ids are blocking sled scans — offload them
        // so they don't hold the async executor while the bulk RPCs are in flight.
        let metadata_scan = self.metadata.clone();
        let scan_handle = tokio::task::spawn_blocking(move || {
            let all = metadata_scan.list_all_chunk_ids()?;
            let live = metadata_scan.live_chunk_ids()?;
            Ok::<_, anyhow::Error>((all, live))
        });

        // First pass: classify all chunks.
        //
        // Old approach: HasChunk per-chunk per-node → O(chunks × nodes) connections.
        // New approach: HasChunks (plural) per-node → O(nodes) connections total.
        //
        // We ask each online remote node "which of these chunk IDs do you hold?" in
        // one bulk RPC, then classify every chunk locally using the result maps.
        // This reduces scan-phase connections from potentially thousands to just one
        // per online node.
        // While the sled scan runs in the background, do the HasChunks bulk RPCs
        // concurrently — both are high-latency operations and neither depends on
        // the other's results yet.
        let local_id = self.cluster.local_node_id();
        let online_nodes: Vec<_> = self.cluster.get_all_nodes().await
            .into_iter()
            .filter(|n| n.status == dfs_common::NodeStatus::Online)
            .collect();

        // node_id → HashSet<ChunkId> of chunks confirmed present on that node.
        // We can't send HasChunks yet (don't have chunk list), but we CAN collect
        // the online node list and check local storage — done above.
        // HasChunks RPCs happen after we have the chunk list from the scan below.

        // Now await the scan — it's been running concurrently with the online_nodes fetch.
        let (all_chunks, live_chunks) = scan_handle
            .await
            .context("spawn_blocking for chunk scan panicked")??;

        // work carries (chunk_id, status, confirmed_alive_node_ids) from the bulk scan.
        // Passing confirmed-alive nodes avoids re-querying in the heal path, which would
        // race against transient RPC failures and misclassify good nodes as ghosts.
        let mut work: Vec<(ChunkId, ReplicationStatus, Vec<NodeId>)> = Vec::new();
        let mut pending_count = 0;
        let mut orphan_count = 0;

        // Separate orphan candidates (no network I/O) from live chunks to check.
        let mut chunks_to_check: Vec<ChunkId> = Vec::new();
        let mut purged_orphans: Vec<ChunkId> = Vec::new();

        for chunk_id in all_chunks {
            if !live_chunks.contains(&chunk_id) {
                if !quorum {
                    debug!("Skipping orphan purge for {} — no quorum", chunk_id);
                    self.pending_healing.write().await.remove(&chunk_id);
                    continue;
                }
                // Purge immediately — no two-cycle wait.  The live_chunk_ids scan already
                // cross-references every file record, so any chunk_id absent from that set
                // is definitively unreferenced.  The two-cycle approach was causing orphans
                // to survive indefinitely whenever the cluster leader changed (the new
                // leader's pending_healing map is empty, so every orphan resets to
                // "first sighting"), which let the sled DB grow to hundreds of MB and
                // ultimately OOM the nodes.
                debug!("Purging orphaned chunk location record: {}", chunk_id);
                if let Err(e) = self.metadata.delete_chunk_location(&chunk_id) {
                    warn!("Failed to purge orphaned chunk location {}: {}", chunk_id, e);
                } else {
                    purged_orphans.push(chunk_id);
                }
                self.pending_healing.write().await.remove(&chunk_id);
                orphan_count += 1;
                continue;
            }
            chunks_to_check.push(chunk_id);
        }

        // Broadcast orphan purges to all followers so their chunk: routing tables
        // drain in sync with the leader.  Followers never purge independently —
        // they could incorrectly delete records for live recordings whose file:
        // metadata hasn't been flushed yet (e.g. an open file being written to).
        if !purged_orphans.is_empty() {
            let cluster = self.cluster.clone();
            let client = self.client.clone();
            let local_id = cluster.local_node_id();
            let orphans_to_broadcast = purged_orphans.clone();
            tokio::spawn(async move {
                let nodes = cluster.get_all_nodes().await;
                for node in &nodes {
                    if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                        continue;
                    }
                    for &chunk_id in &orphans_to_broadcast {
                        let req = Request::PurgeChunkLocation { chunk_id };
                        if let Err(e) = client.send_message(node.addr, Message::Request(req)).await {
                            debug!("Failed to broadcast PurgeChunkLocation {} to node {}: {}", chunk_id, node.id, e);
                        }
                    }
                }
            });
        }

        // Bulk-query each online remote node: "which of these chunks do you hold?"
        // One HasChunks RPC per node replaces O(chunks) HasChunk RPCs per node.
        // node_id → HashSet<ChunkId> of chunks confirmed present on that node
        let mut node_chunk_presence: HashMap<NodeId, HashSet<ChunkId>> = HashMap::new();

        // Local node: check storage directly (no network).
        {
            let mut local_set = HashSet::new();
            for chunk_id in &chunks_to_check {
                if self.storage.has_chunk(chunk_id) {
                    local_set.insert(*chunk_id);
                }
            }
            node_chunk_presence.insert(local_id, local_set);
        }

        // Remote nodes: one HasChunks RPC each.
        for node_info in &online_nodes {
            if node_info.id == local_id {
                continue;
            }
            let request = Request::HasChunks { chunk_ids: chunks_to_check.clone() };
            match self.client.send_message(node_info.addr, Message::Request(request)).await {
                Ok(envelope) => {
                    if let Message::Response(Response::BoolVec { values }) = envelope.message {
                        let mut present = HashSet::new();
                        for (chunk_id, has) in chunks_to_check.iter().zip(values.iter()) {
                            if *has {
                                present.insert(*chunk_id);
                            }
                        }
                        debug!("Node {} holds {}/{} queried chunks", node_info.id, present.len(), chunks_to_check.len());
                        node_chunk_presence.insert(node_info.id, present);
                    } else {
                        warn!("Unexpected response to HasChunks from node {}", node_info.id);
                    }
                }
                Err(e) => {
                    // Don't insert anything for this node — we can't distinguish "node has
                    // no chunks" from "node is unreachable". Leaving it absent from
                    // node_chunk_presence means we won't count it as a confirmed replica,
                    // but we also won't declare chunks unrecoverable just because this RPC
                    // failed. The unrecoverable check requires all metadata nodes to be
                    // reachable before writing off a chunk as permanently lost.
                    warn!("HasChunks RPC failed for node {} ({}): skipping node for this scan cycle", node_info.id, e);
                }
            }
        }

        // Classify all chunks — no work cap here. Every chunk must be classified so
        // that pending_healing timestamps are updated for all under-replicated chunks,
        // not just the first max_heal_per_cycle. Without this, chunks past position 200
        // never enter pending_healing and never get healed until the queue drains.
        // The work batch is capped after classification, sorted oldest-first so long-
        // waiting chunks are not perpetually starved by newer ones.
        let mut classify_count = 0usize;
        for chunk_id in chunks_to_check {
            classify_count += 1;
            if classify_count % 100 == 0 {
                tokio::task::yield_now().await;
            }

            let location = match self.metadata.get_chunk_location(&chunk_id) {
                Ok(Some(loc)) => loc,
                _ => continue,
            };
            let metadata_node_count = location.nodes.len();

            let mut actual_replicas = 0usize;
            let mut nodes_without_chunk: Vec<NodeId> = Vec::new();
            let mut confirmed_alive_nodes: Vec<NodeId> = Vec::new();

            for node_id in &location.nodes {
                // Only count online nodes — offline nodes are expected to be absent.
                if online_nodes.iter().any(|n| n.id == *node_id) {
                    if node_chunk_presence.get(node_id).map_or(false, |s| s.contains(&chunk_id)) {
                        actual_replicas += 1;
                        confirmed_alive_nodes.push(*node_id);
                    } else {
                        nodes_without_chunk.push(*node_id);
                    }
                }
            }

            // Ghost node pruning: only remove a node from metadata after the healing
            // delay has passed for this chunk. A single HasChunks miss is not enough —
            // the node may have just restarted (connections drop, storage comes up within
            // seconds, but the next heal cycle is 60s away). We wait healing_delay_secs
            // before trusting the miss, same patience we apply to under-replication.
            if !nodes_without_chunk.is_empty() && actual_replicas > 0 {
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
                        written_at: location.written_at,
                    };
                    if let Err(e) = self.metadata.put_chunk_location(&updated_location) {
                        warn!("Failed to prune ghost nodes from chunk {} metadata: {}", chunk_id, e);
                    } else {
                        HealingManager::broadcast_chunk_location_shared(&updated_location, &self.cluster, &self.client).await;
                    }
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
                let delay_passed = location.nodes.is_empty() ||
                    pending.get(&chunk_id)
                        .map_or(false, |t| t.elapsed() >= Duration::from_secs(self.healing_delay_secs));
                drop(pending);

                if delay_passed {
                    warn!(
                        "DATA LOSS: Chunk {} is permanently unrecoverable ({} metadata nodes, all confirmed empty) — purging stale metadata",
                        chunk_id, location.nodes.len()
                    );
                    if let Err(e) = self.metadata.delete_chunk_location(&chunk_id) {
                        warn!("Failed to purge unrecoverable chunk {} metadata: {}", chunk_id, e);
                    }
                    self.pending_healing.write().await.remove(&chunk_id);
                    continue;
                } else {
                    debug!(
                        "Chunk {} has 0 accessible replicas but delay not yet passed — waiting before declaring unrecoverable",
                        chunk_id
                    );
                }
            }

            let status = if actual_replicas < replication_factor {
                ReplicationStatus::UnderReplicated
            } else if actual_replicas > replication_factor {
                ReplicationStatus::OverReplicated
            } else {
                ReplicationStatus::Ok
            };

            match status {
                ReplicationStatus::UnderReplicated => {
                    // Skip healing_delay if the chunk was never fully replicated to RF nodes —
                    // metadata always listed fewer than replication_factor nodes, so this was
                    // never a transient node failure but a chunk that missed its 3rd write.
                    let never_fully_replicated = metadata_node_count < replication_factor;
                    if never_fully_replicated || self.should_heal(&chunk_id).await {
                        if never_fully_replicated {
                            // Pre-insert into pending so should_heal won't re-delay it next cycle
                            self.pending_healing.write().await
                                .entry(chunk_id)
                                .or_insert_with(|| Instant::now() - Duration::from_secs(self.healing_delay_secs + 1));
                        }
                        work.push((chunk_id, ReplicationStatus::UnderReplicated, confirmed_alive_nodes.clone()));
                    } else {
                        pending_count += 1;
                    }
                }
                ReplicationStatus::OverReplicated => {
                    if quorum {
                        work.push((chunk_id, ReplicationStatus::OverReplicated, confirmed_alive_nodes.clone()));
                    } else {
                        debug!("Skipping over-replication cleanup for {} — no quorum", chunk_id);
                    }
                }
                ReplicationStatus::Ok => {
                    self.pending_healing.write().await.remove(&chunk_id);
                }
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
            // Flush sled after bulk orphan purge so the B-tree compacts and the OS
            // can reclaim page-cache memory from the now-smaller DB file.
            if let Err(e) = self.metadata.flush() {
                warn!("Failed to flush metadata after orphan purge: {}", e);
            }
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

    /// Drain the heal queue — issues PushChunkTo / DeleteChunkReplica for all chunks
    /// in pending_healing whose delay has elapsed. Uses alive_nodes_cache populated by
    /// run_discovery_pass() rather than re-scanning, so this can run frequently (15s)
    /// without hammering the cluster with HasChunks RPCs on every tick.
    async fn drain_heal_queue(&self) -> Result<()> {
        let quorum = self.cluster.has_quorum().await;

        // Build work list from pending_healing entries whose delay has passed.
        // Read pending + cache under a single read lock snapshot, then drop the lock
        // before issuing any network I/O.
        let work: Vec<(ChunkId, ReplicationStatus, Vec<NodeId>)> = {
            let pending = self.pending_healing.read().await;
            let cache   = self.alive_nodes_cache.read().await;

            let mut v = Vec::new();
            for (chunk_id, detected_at) in pending.iter() {
                if detected_at.elapsed() < Duration::from_secs(self.healing_delay_secs) {
                    continue;
                }

                // alive_nodes_cache tells us who confirmed holding this chunk. If
                // there is no cache entry yet (discovery hasn't run for this chunk),
                // skip — we'll pick it up after the first discovery pass runs.
                let confirmed_alive = match cache.get(chunk_id) {
                    Some(nodes) => nodes.clone(),
                    None => continue,
                };

                // Classify using cached alive count vs replication factor.
                let status = if confirmed_alive.len() < self.replication_factor {
                    ReplicationStatus::UnderReplicated
                } else if confirmed_alive.len() > self.replication_factor {
                    if quorum {
                        ReplicationStatus::OverReplicated
                    } else {
                        continue; // skip over-replication cleanup without quorum
                    }
                } else {
                    // Now at RF — remove from pending, will be pruned from cache
                    // on the next discovery pass.
                    continue;
                };

                v.push((*chunk_id, status, confirmed_alive));
            }
            drop(pending);
            drop(cache);

            // Sort oldest-first, cap to max_heal_per_cycle
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

        // Spawn all work immediately — the heal_semaphore gates how many bytes can be
        // in-flight at once. Each task acquires chunk_size permits before the transfer
        // and releases them on completion, so concurrency self-tunes to actual chunk
        // sizes with no fixed cap or inter-task sleeps needed.
        let total = work.len();
        let mut handles = Vec::with_capacity(total);

        for (chunk_id, status, confirmed_alive) in work {
            let storage = self.storage.clone();
            let metadata = self.metadata.clone();
            let cluster = self.cluster.clone();
            let client = self.client.clone();
            let pending_healing = self.pending_healing.clone();
            let in_flight_healing = self.in_flight_healing.clone();
            let heal_semaphore = self.heal_semaphore.clone();
            let heal_semaphore_capacity = self.heal_semaphore_capacity;
            let replication_factor = self.replication_factor;

            handles.push(tokio::spawn(async move {
                // Look up chunk size to size the semaphore acquisition.
                // Fall back to max chunk size (4MB) if unknown — conservative but safe.
                let chunk_size = metadata.get_chunk_location(&chunk_id)
                    .ok()
                    .flatten()
                    .map(|loc| loc.size)
                    .unwrap_or(4 * 1024 * 1024);

                // Acquire permits proportional to chunk size. Clamp to total capacity
                // so a single chunk larger than the budget can't deadlock — it just
                // acquires the full budget and runs alone.
                let permits = (chunk_size as u32).min(heal_semaphore_capacity as u32).max(1);
                let _permit = heal_semaphore.acquire_many(permits).await;

                match status {
                    ReplicationStatus::UnderReplicated => {
                        if let Err(e) = HealingManager::do_heal_chunk_shared(
                            &chunk_id, confirmed_alive, &storage, &metadata, &cluster, &client,
                            &pending_healing, &in_flight_healing, replication_factor,
                        ).await {
                            warn!("Failed to heal chunk {}: {}", chunk_id, e);
                        }
                    }
                    ReplicationStatus::OverReplicated => {
                        if let Err(e) = HealingManager::do_cleanup_excess_shared(
                            &chunk_id, confirmed_alive, &metadata, &cluster, &client, replication_factor,
                        ).await {
                            warn!("Failed to cleanup over-replicated chunk {}: {}", chunk_id, e);
                        }
                    }
                    ReplicationStatus::Ok => {}
                }
                // _permit drops here, releasing semaphore slots
            }));
        }

        // Wait for all spawned tasks to complete before returning, so the next
        // drain_heal_queue call doesn't double-queue chunks still in flight.
        let mut healed = 0usize;
        for handle in handles {
            let _ = handle.await;
            healed += 1;
        }

        if healed > 0 {
            info!("Heal queue drain complete: healed={}", healed);
        }

        Ok(())
    }

    /// Check if a chunk should be healed (delay has passed)
    async fn should_heal(&self, chunk_id: &ChunkId) -> bool {
        let mut pending = self.pending_healing.write().await;

        match pending.get(chunk_id) {
            Some(detected_at) => {
                // Check if delay has passed
                let elapsed = detected_at.elapsed();
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
                pending.insert(*chunk_id, Instant::now());
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
        if !alive.iter().any(|(id, _)| *id == local_id) && storage.has_chunk(chunk_id) {
            if let Some(info) = cluster.get_node(&local_id).await {
                warn!(
                    "Chunk {} found in local storage but missing from confirmed-alive list — adding as source",
                    chunk_id
                );
                alive.push((local_id, info.addr));
            }
        }

        if alive.is_empty() {
            anyhow::bail!("No alive nodes have chunk {}", chunk_id);
        }

        let needed = replication_factor.saturating_sub(alive.len());
        if needed == 0 {
            pending_healing.write().await.remove(chunk_id);
            return Ok(());
        }

        // Select target nodes: capacity-aware candidates that don't already hold the chunk
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

            let mut updated_nodes: Vec<NodeId> = alive.iter().map(|(id, _)| *id).collect();
            updated_nodes.extend(replicated);

            let updated_location = ChunkLocation {
                chunk_id: *chunk_id,
                nodes: updated_nodes,
                size: location.size,
                checksum: location.checksum,
                file_offset: location.file_offset,
                written_at: location.written_at,
            };

            if let Err(e) = metadata.put_chunk_location(&updated_location) {
                warn!("Failed to update chunk location after healing {}: {}", chunk_id, e);
            } else {
                Self::broadcast_chunk_location_shared(&updated_location, cluster, client).await;
            }

            pending_healing.write().await.remove(chunk_id);
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
            let request = Request::ReplicateChunkLocation { location: location.clone() };
            if let Err(e) = client.send_message(node.addr, Message::Request(request)).await {
                warn!("Failed to broadcast chunk location {} to node {}: {}", location.chunk_id, node.id, e);
            }
        }
    }

    /// Static cleanup implementation — callable from both instance methods and spawned tasks.
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

        // Use confirmed_alive_nodes from the bulk scan — these were verified via HasChunks.
        // Don't re-derive from location.nodes + online check, which could include ghost nodes
        // and lead to deleting a real replica while leaving a ghost in the metadata.
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

        // Remove the replica from the most-utilized node — this naturally rebalances
        // the cluster over time rather than always shedding from an arbitrary node.
        let alive_ids: Vec<NodeId> = alive.iter().map(|(id, _)| *id).collect();
        let excess_id = cluster.most_utilized_node(&alive_ids).await
            .unwrap_or_else(|| alive_ids[alive_ids.len() - 1]);
        let (_, excess_addr) = *alive.iter().find(|(id, _)| *id == excess_id).unwrap();

        info!(
            "Chunk {} over-replicated ({} alive, RF={}): leader instructing node {} to delete excess copy",
            chunk_id, alive.len(), replication_factor, excess_id
        );

        let local_id = cluster.local_node_id();
        let request = Request::DeleteChunkReplica { chunk_id: *chunk_id, leader_id: local_id };
        match client.send_message(excess_addr, Message::Request(request)).await {
            Ok(envelope) if matches!(envelope.message, Message::Response(Response::Ok { .. })) => {
                info!("Node {} deleted excess copy of chunk {}", excess_id, chunk_id);
            }
            Ok(envelope) => {
                warn!("Node {} failed to delete chunk {}: {:?}", excess_id, chunk_id, envelope.message);
                return Ok(()); // Don't update metadata if delete failed
            }
            Err(e) => {
                warn!("Failed to contact node {} for chunk {} deletion: {}", excess_id, chunk_id, e);
                return Ok(());
            }
        }

        // Update metadata: remove the excess node
        let updated_nodes: Vec<NodeId> = alive.iter()
            .map(|(id, _)| *id)
            .filter(|id| *id != excess_id)
            .collect();

        let updated_location = ChunkLocation {
            chunk_id: *chunk_id,
            nodes: updated_nodes,
            size: location.size,
            checksum: location.checksum,
            file_offset: location.file_offset,
            written_at: location.written_at,
        };

        if let Err(e) = metadata.put_chunk_location(&updated_location) {
            warn!("Failed to update chunk location after cleanup of {}: {}", chunk_id, e);
        } else {
            Self::broadcast_chunk_location_shared(&updated_location, cluster, client).await;
            info!("Excess replica cleanup complete for chunk {}", chunk_id);
        }

        Ok(())
    }

    /// Scrub all chunks (verify checksums)
    async fn scrub_all_chunks(&self) -> Result<()> {
        let chunks = self.storage.list_chunks()?;

        info!("Scrubbing {} chunks", chunks.len());

        let mut verified = 0;
        let mut errors = 0;

        for chunk_id in chunks {
            match self.storage.read_and_verify_chunk(&chunk_id) {
                Ok(_) => {
                    verified += 1;
                }
                Err(e) => {
                    warn!("Scrubbing error for chunk {}: {}", chunk_id, e);
                    errors += 1;

                    // Mark for healing
                    self.pending_healing
                        .write()
                        .await
                        .insert(chunk_id, Instant::now());
                }
            }
        }

        info!(
            "Scrubbing complete: verified={}, errors={}",
            verified, errors
        );

        Ok(())
    }

    /// Get healing statistics
    pub async fn get_stats(&self) -> HealingStats {
        let pending = self.pending_healing.read().await;

        HealingStats {
            pending_healing: pending.len(),
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
        // Run a fresh discovery pass first so alive_nodes_cache is current,
        // then immediately drain the queue — this mirrors what the periodic
        // loops do but back-to-back for the manual trigger case.
        self.run_discovery_pass().await?;
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
}
