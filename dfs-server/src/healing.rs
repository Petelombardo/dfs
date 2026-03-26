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

    /// Chunks pending healing (chunk_id -> failure_detected_at)
    pending_healing: Arc<RwLock<HashMap<ChunkId, Instant>>>,
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
        Self {
            storage,
            metadata,
            cluster,
            client,
            replication_factor,
            healing_delay_secs,
            scrub_interval_hours,
            auto_heal,
            pending_healing: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Start background healing tasks
    pub async fn start(self: Arc<Self>) {
        if !self.auto_heal {
            info!("Auto-healing is disabled");
            return;
        }

        info!(
            "Starting healing manager (delay: {}s, scrub: {}h)",
            self.healing_delay_secs, self.scrub_interval_hours
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

    /// Run periodic healing checker
    async fn run_healing_checker(&self) {
        let mut check_interval = interval(Duration::from_secs(60)); // Check every minute

        loop {
            check_interval.tick().await;

            if let Err(e) = self.check_and_heal().await {
                warn!("Healing check error: {}", e);
            }
        }
    }

    /// Run periodic scrubber
    async fn run_scrubber(&self) {
        let scrub_interval = Duration::from_secs(self.scrub_interval_hours * 3600);
        let mut timer = interval(scrub_interval);

        loop {
            timer.tick().await;

            info!("Starting scrubbing pass");
            if let Err(e) = self.scrub_all_chunks().await {
                warn!("Scrubbing error: {}", e);
            }
        }
    }

    /// Check all chunks and heal under-replicated ones
    async fn check_and_heal(&self) -> Result<()> {
        debug!("Running healing check");

        // Get all local chunks
        let local_chunks = self.storage.list_chunks()?;

        let mut healed_count = 0;
        let mut pending_count = 0;

        for chunk_id in local_chunks {
            match self.check_chunk_replication(&chunk_id).await {
                Ok(status) => match status {
                    ReplicationStatus::UnderReplicated => {
                        // Check if enough time has passed since failure detection
                        if self.should_heal(&chunk_id).await {
                            if let Err(e) = self.heal_chunk(&chunk_id).await {
                                warn!("Failed to heal chunk {}: {}", chunk_id, e);
                            } else {
                                healed_count += 1;
                            }
                        } else {
                            pending_count += 1;
                        }
                    }
                    ReplicationStatus::OverReplicated => {
                        if let Err(e) = self.cleanup_excess_replicas(&chunk_id).await {
                            warn!("Failed to cleanup chunk {}: {}", chunk_id, e);
                        }
                    }
                    ReplicationStatus::Ok => {
                        // Remove from pending if it was there
                        self.pending_healing.write().await.remove(&chunk_id);
                    }
                },
                Err(e) => {
                    debug!("Error checking chunk {}: {}", chunk_id, e);
                }
            }
        }

        if healed_count > 0 || pending_count > 0 {
            info!(
                "Healing check complete: healed={}, pending={}",
                healed_count, pending_count
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

    /// Heal an under-replicated chunk
    async fn heal_chunk(&self, chunk_id: &ChunkId) -> Result<()> {
        info!("Healing under-replicated chunk: {}", chunk_id);

        // Get current location
        let location = self
            .metadata
            .get_chunk_location(chunk_id)?
            .ok_or_else(|| anyhow::anyhow!("Chunk location not found"))?;

        // Find alive nodes
        let mut alive_nodes = Vec::new();
        for node_id in &location.nodes {
            if let Some(node_info) = self.cluster.get_node(node_id).await {
                if node_info.status == dfs_common::NodeStatus::Online {
                    alive_nodes.push(*node_id);
                }
            }
        }

        if alive_nodes.is_empty() {
            anyhow::bail!("No alive nodes have chunk {}", chunk_id);
        }

        // Read chunk data (from local or remote)
        let chunk_data = if self.storage.has_chunk(chunk_id) {
            self.storage.read_chunk(chunk_id)?
        } else {
            // Would need to fetch from remote - for now bail
            anyhow::bail!("Chunk not available locally for healing");
        };

        // Determine target nodes for additional replicas
        let needed_replicas = self.replication_factor.saturating_sub(alive_nodes.len());

        if needed_replicas == 0 {
            // Already have enough replicas
            self.pending_healing.write().await.remove(chunk_id);
            return Ok(());
        }

        // Get candidate nodes using capacity-aware placement
        // This matches how writes work, so healing won't fight against placement decisions
        let candidate_nodes = self
            .cluster
            .get_nodes_with_capacity_awareness(chunk_id, self.replication_factor + needed_replicas)
            .await;

        // Find nodes that don't have the chunk yet
        let alive_set: HashSet<_> = alive_nodes.iter().copied().collect();
        let new_targets: Vec<_> = candidate_nodes
            .into_iter()
            .filter(|n| !alive_set.contains(n))
            .take(needed_replicas)
            .collect();

        if new_targets.is_empty() {
            warn!("No suitable nodes found for re-replication of {}", chunk_id);
            return Ok(());
        }

        // Replicate to new nodes
        let mut replicated_count = 0;

        for node_id in &new_targets {
            if let Some(node_info) = self.cluster.get_node(node_id).await {
                info!(
                    "Replicating chunk {} to node {} ({})",
                    chunk_id, node_id, node_info.addr
                );

                // Send replication request
                let request = Request::ReplicateChunk {
                    chunk_id: *chunk_id,
                    data: chunk_data.clone(),
                    checksum: chunk_id.hash,
                };

                match self.client.send_message(node_info.addr, Message::Request(request)).await {
                    Ok(response) => {
                        if matches!(response.message, Message::Response(Response::Ok { .. })) {
                            replicated_count += 1;
                            info!("Successfully replicated chunk {} to node {}", chunk_id, node_id);
                        } else {
                            warn!("Failed to replicate chunk {} to node {}: unexpected response", chunk_id, node_id);
                        }
                    }
                    Err(e) => {
                        warn!("Failed to replicate chunk {} to node {}: {}", chunk_id, node_id, e);
                    }
                }
            }
        }

        if replicated_count > 0 {
            info!(
                "Healed chunk {}: added {} replicas",
                chunk_id, replicated_count
            );

            // Update metadata with new node list
            let mut updated_nodes = alive_nodes.clone();
            updated_nodes.extend(new_targets.iter().take(replicated_count));

            let updated_location = ChunkLocation {
                chunk_id: *chunk_id,
                nodes: updated_nodes,
                size: location.size,
                checksum: location.checksum,
            };

            if let Err(e) = self.metadata.put_chunk_location(&updated_location) {
                warn!("Failed to update chunk location metadata: {}", e);
            }

            // Remove from pending
            self.pending_healing.write().await.remove(chunk_id);
        }

        Ok(())
    }

    /// Cleanup excess replicas
    async fn cleanup_excess_replicas(&self, chunk_id: &ChunkId) -> Result<()> {
        debug!("Cleaning up over-replicated chunk: {}", chunk_id);

        let location = self
            .metadata
            .get_chunk_location(chunk_id)?
            .ok_or_else(|| anyhow::anyhow!("Chunk location not found"))?;

        // Count alive replicas
        let mut alive_nodes = Vec::new();
        for node_id in &location.nodes {
            if let Some(node_info) = self.cluster.get_node(node_id).await {
                if node_info.status == dfs_common::NodeStatus::Online {
                    alive_nodes.push(*node_id);
                }
            }
        }

        let excess = alive_nodes.len().saturating_sub(self.replication_factor);

        if excess > 0 {
            info!(
                "Chunk {} has {} excess replicas, cleaning up",
                chunk_id, excess
            );

            // Remove excess replicas (keep the first N)
            for node_id in alive_nodes.iter().skip(self.replication_factor) {
                debug!("Would remove chunk {} from node {}", chunk_id, node_id);
                // Actual deletion would use NetworkClient
            }
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
