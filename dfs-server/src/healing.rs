use anyhow::{Context, Result};
use dfs_common::{ChunkId, ChunkLocation, Message, NodeId, Request, Response};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
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

    /// Maximum number of chunks to process per check cycle (queue depth)
    max_heal_per_cycle: usize,

    /// Maximum number of heal/cleanup ops to run concurrently within a cycle
    max_concurrent_heals: usize,

    /// Chunks pending healing (chunk_id -> failure_detected_at)
    pending_healing: Arc<RwLock<HashMap<ChunkId, Instant>>>,

    /// Chunks currently being healed by this node — prevents double-healing when
    /// multiple nodes run the healer concurrently for the same chunk.
    /// A chunk is inserted before replication begins and removed on completion.
    in_flight_healing: Arc<RwLock<HashSet<ChunkId>>>,
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
        // Process up to 1000 chunks per cycle (queue depth). 2 concurrent heals
        // with a 50ms cooldown keeps healing gentle on gigabit — at most 2 connections
        // competing with client I/O at any moment, yielding every 50ms.
        let max_heal_per_cycle = 1000;
        let max_concurrent_heals = 2;

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
            max_concurrent_heals,
            pending_healing: Arc::new(RwLock::new(HashMap::new())),
            in_flight_healing: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Start background healing tasks
    pub async fn start(self: Arc<Self>) {
        if !self.auto_heal {
            info!("Auto-healing is disabled");
            return;
        }

        info!(
            "Starting healing manager (delay: {}s, scrub: {}h, max_per_cycle: {}, concurrency: {})",
            self.healing_delay_secs, self.scrub_interval_hours, self.max_heal_per_cycle, self.max_concurrent_heals
        );

        // Start healing checker (runs every minute)
        let healing_checker = self.clone();
        tokio::spawn(async move {
            healing_checker.run_healing_checker().await;
        });

        // Start scrubber (runs at configured interval)
        let scrubber = self.clone();
        tokio::spawn(async move {
            scrubber.run_scrubber().await;
        });
    }

    /// Run periodic healing checker — only executes on the cluster leader.
    ///
    /// Leadership is derived from the gossip cluster view: the online node with
    /// the minimum NodeId is the leader. All nodes compute this identically so
    /// no election protocol is needed. When the leader goes offline the next
    /// lowest-ID node takes over on its next interval tick.
    async fn run_healing_checker(&self) {
        let mut cleanup_counter = 0;
        let mut was_leader = false;

        loop {
            // Sleep first so startup doesn't immediately trigger a heal cycle.
            // Using sleep-after-completion rather than a fixed interval ensures
            // we never queue a second cycle before the first one finishes.
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
                continue;
            }

            if let Err(e) = self.check_and_heal().await {
                warn!("Healing check error: {}", e);
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

            // Remove if chunk no longer exists in local storage or metadata
            if !self.storage.has_chunk(chunk_id) {
                if self.metadata.get_chunk_location(chunk_id).ok().flatten().is_none() {
                    debug!("Removing pending healing entry for non-existent chunk {}", chunk_id);
                    to_remove.push(*chunk_id);
                }
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

    /// Check all chunks and heal under-replicated ones.
    /// As leader, iterates over ALL chunk IDs known in metadata — not just local
    /// ones — so it can coordinate healing regardless of where the data lives.
    ///
    /// Up to `max_heal_per_cycle` chunks are queued per cycle; up to
    /// `max_concurrent_heals` ops run in parallel, each throttled by 200ms after
    /// completion to avoid connection storms.
    async fn check_and_heal(&self) -> Result<()> {
        debug!("Running healing check");

        let all_chunks = self.metadata.list_all_chunk_ids()?;

        // First pass: collect the work to do this cycle (respects queue cap)
        let mut work: Vec<(ChunkId, ReplicationStatus)> = Vec::new();
        let mut pending_count = 0;
        let mut skipped_count = 0;

        for chunk_id in all_chunks {
            if work.len() >= self.max_heal_per_cycle {
                skipped_count += 1;
                continue;
            }

            match self.check_chunk_replication(&chunk_id).await {
                Ok(ReplicationStatus::UnderReplicated) => {
                    if self.should_heal(&chunk_id).await {
                        work.push((chunk_id, ReplicationStatus::UnderReplicated));
                    } else {
                        pending_count += 1;
                    }
                }
                Ok(ReplicationStatus::OverReplicated) => {
                    work.push((chunk_id, ReplicationStatus::OverReplicated));
                }
                Ok(ReplicationStatus::Ok) => {
                    self.pending_healing.write().await.remove(&chunk_id);
                }
                Err(e) => {
                    debug!("Error checking chunk {}: {}", chunk_id, e);
                }
            }
        }

        if work.is_empty() {
            if pending_count > 0 {
                debug!("Healing check: {} chunks pending delay", pending_count);
            }
            return Ok(());
        }

        // Second pass: execute with bounded concurrency.
        // We need an Arc<Self> to share across tasks; callers already hold one
        // (start() takes Arc<Self>), but check_and_heal takes &self. We reconstruct
        // the Arc from the fields we already Arc-wrapped — storage/metadata/cluster
        // are all Arc'd, so we just need a way to share self. The cleanest approach
        // is to pass the fields directly rather than the whole struct.
        let semaphore = Arc::new(tokio::sync::Semaphore::new(self.max_concurrent_heals));
        let healed_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = Vec::new();

        for (chunk_id, status) in work {
            let sem = semaphore.clone();
            let healed = healed_count.clone();
            // Clone the Arc fields needed per-op so tasks are Send + 'static
            let storage = self.storage.clone();
            let metadata = self.metadata.clone();
            let cluster = self.cluster.clone();
            let client = self.client.clone();
            let pending_healing = self.pending_healing.clone();
            let in_flight_healing = self.in_flight_healing.clone();
            let replication_factor = self.replication_factor;

            let handle = tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                match status {
                    ReplicationStatus::UnderReplicated => {
                        if let Err(e) = HealingManager::do_heal_chunk_shared(
                            &chunk_id, &storage, &metadata, &cluster, &client,
                            &pending_healing, &in_flight_healing, replication_factor,
                        ).await {
                            warn!("Failed to heal chunk {}: {}", chunk_id, e);
                        }
                    }
                    ReplicationStatus::OverReplicated => {
                        if let Err(e) = HealingManager::do_cleanup_excess_shared(
                            &chunk_id, &metadata, &cluster, &client, replication_factor,
                        ).await {
                            warn!("Failed to cleanup over-replicated chunk {}: {}", chunk_id, e);
                        }
                    }
                    ReplicationStatus::Ok => {}
                }
                healed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Throttle after each op — 50ms on gigabit yields frequently
                // without significantly slowing recovery (4MB transfer ~33ms).
                tokio::time::sleep(Duration::from_millis(50)).await;
            });
            handles.push(handle);
        }

        for handle in handles {
            let _ = handle.await;
        }

        let healed = healed_count.load(std::sync::atomic::Ordering::Relaxed);
        if healed > 0 || pending_count > 0 || skipped_count > 0 {
            info!(
                "Healing check complete: healed={}, pending={}, deferred={}",
                healed, pending_count, skipped_count
            );
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

    /// Check replication status of a chunk
    async fn check_chunk_replication(&self, chunk_id: &ChunkId) -> Result<ReplicationStatus> {
        // Get chunk location from metadata
        let location = self
            .metadata
            .get_chunk_location(chunk_id)?
            .ok_or_else(|| anyhow::anyhow!("Chunk location not found"))?;

        // Count how many nodes actually have the chunk
        // Note: For RF=3 optimization, chunks are initially written to 2 nodes,
        // then the 3rd replica is created in the background by the healing manager
        let mut actual_replicas = 0;

        for node_id in &location.nodes {
            // Check if node is online AND has the chunk
            if let Some(node_info) = self.cluster.get_node(node_id).await {
                if node_info.status == dfs_common::NodeStatus::Online {
                    // Check if this node actually has the chunk
                    let has_chunk = if *node_id == self.cluster.local_node_id() {
                        // Local check
                        self.storage.has_chunk(chunk_id)
                    } else {
                        // Remote check - would need to query the node
                        // For now, assume metadata is accurate for remote nodes
                        // (proper implementation would use HasChunk request)
                        true
                    };

                    if has_chunk {
                        actual_replicas += 1;
                    }
                }
            }
        }

        if actual_replicas < self.replication_factor {
            Ok(ReplicationStatus::UnderReplicated)
        } else if actual_replicas > self.replication_factor {
            Ok(ReplicationStatus::OverReplicated)
        } else {
            Ok(ReplicationStatus::Ok)
        }
    }

    /// Heal an under-replicated chunk (instance method — delegates to shared static).
    async fn heal_chunk(&self, chunk_id: &ChunkId) -> Result<()> {
        Self::do_heal_chunk_shared(
            chunk_id, &self.storage, &self.metadata, &self.cluster, &self.client,
            &self.pending_healing, &self.in_flight_healing, self.replication_factor,
        ).await
    }

    /// Static heal implementation — callable from both instance methods and spawned tasks.
    async fn do_heal_chunk_shared(
        chunk_id: &ChunkId,
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
            chunk_id, storage, metadata, cluster, client, pending_healing, replication_factor,
        ).await;
        in_flight_healing.write().await.remove(chunk_id);
        result
    }

    async fn do_heal_chunk_inner(
        chunk_id: &ChunkId,
        _storage: &Arc<ChunkStorage>,
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

        // Build alive node list with their addresses
        let mut alive: Vec<(NodeId, std::net::SocketAddr)> = Vec::new();
        for node_id in &location.nodes {
            if let Some(info) = cluster.get_node(node_id).await {
                if info.status == dfs_common::NodeStatus::Online {
                    alive.push((*node_id, info.addr));
                }
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
        let local_id = cluster.local_node_id();
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

    /// Remove one excess replica (instance method — delegates to shared static).
    async fn cleanup_excess_replicas(&self, chunk_id: &ChunkId) -> Result<()> {
        Self::do_cleanup_excess_shared(
            chunk_id, &self.metadata, &self.cluster, &self.client, self.replication_factor,
        ).await
    }

    /// Static cleanup implementation — callable from both instance methods and spawned tasks.
    async fn do_cleanup_excess_shared(
        chunk_id: &ChunkId,
        metadata: &Arc<MetadataStore>,
        cluster: &Arc<ClusterManager>,
        client: &Arc<NetworkClient>,
        replication_factor: usize,
    ) -> Result<()> {
        let location = metadata
            .get_chunk_location(chunk_id)?
            .ok_or_else(|| anyhow::anyhow!("Chunk location not found"))?;

        // Collect alive nodes in stable (location.nodes) order
        let mut alive: Vec<(NodeId, std::net::SocketAddr)> = Vec::new();
        for node_id in &location.nodes {
            if let Some(info) = cluster.get_node(node_id).await {
                if info.status == dfs_common::NodeStatus::Online {
                    alive.push((*node_id, info.addr));
                }
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
