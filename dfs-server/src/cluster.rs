use anyhow::Result;
use dfs_common::{ConsistentHashRing, NodeId, NodeInfo, NodeStatus};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Notify, RwLock};
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

/// Persisted peer list for cluster recovery
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedPeers {
    peers: Vec<SocketAddr>,
    last_updated: u64, // Unix timestamp
}

/// Node capacity information for placement decisions
#[derive(Debug, Clone)]
struct NodeCapacity {
    available: u64,
    total: u64,
    last_updated: u64, // Unix timestamp
}

/// Cluster membership manager
/// Tracks all nodes in the cluster and their status
pub struct ClusterManager {
    /// This node's ID
    local_node_id: NodeId,

    /// This node's address
    local_addr: SocketAddr,

    /// All nodes in the cluster (NodeId -> NodeInfo)
    nodes: Arc<RwLock<HashMap<NodeId, NodeInfo>>>,

    /// Consistent hash ring for data placement
    hash_ring: Arc<RwLock<ConsistentHashRing>>,

    /// Node capacity information for capacity-aware placement
    node_capacities: Arc<RwLock<HashMap<NodeId, NodeCapacity>>>,

    /// Heartbeat interval in seconds
    heartbeat_interval: u64,

    /// Node failure timeout in seconds
    failure_timeout: u64,

    /// Fired whenever any peer node transitions to Online (recovered or newly joined).
    /// Listeners use this to trigger proactive sync without polling.
    pub node_recovered_notify: Arc<Notify>,

    /// Timestamp of the most recent leader promotion on this node.
    /// Set by the server when it detects !was_leader && is_leader.
    /// Used by the healer to enforce a post-election grace period before
    /// allowing destructive operations (orphan purge, DATA LOSS declarations).
    became_leader_at: Arc<RwLock<Option<std::time::Instant>>>,
}

impl ClusterManager {
    /// Create a new cluster manager
    pub fn new(
        local_node_id: NodeId,
        local_addr: SocketAddr,
        heartbeat_interval: u64,
        failure_timeout: u64,
    ) -> Self {
        let mut nodes = HashMap::new();
        let node_info = NodeInfo::new(local_node_id, local_addr, None);
        nodes.insert(local_node_id, node_info);

        let mut hash_ring = ConsistentHashRing::new(100); // 100 virtual nodes
        hash_ring.add_node(local_node_id);

        Self {
            local_node_id,
            local_addr,
            nodes: Arc::new(RwLock::new(nodes)),
            hash_ring: Arc::new(RwLock::new(hash_ring)),
            node_capacities: Arc::new(RwLock::new(HashMap::new())),
            heartbeat_interval,
            failure_timeout,
            node_recovered_notify: Arc::new(Notify::new()),
            became_leader_at: Arc::new(RwLock::new(None)),
        }
    }

    /// Get this node's ID
    pub fn local_node_id(&self) -> NodeId {
        self.local_node_id
    }

    /// Get this node's address
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Add a new node to the cluster
    pub async fn add_node(&self, node_info: NodeInfo) -> Result<()> {
        let mut nodes = self.nodes.write().await;
        let mut ring = self.hash_ring.write().await;

        let is_new = !nodes.contains_key(&node_info.id);
        if is_new {
            info!("Adding new node {} to cluster", node_info.id);
            ring.add_node(node_info.id);
        } else {
            debug!("Node {} already exists, updating info", node_info.id);
        }

        nodes.insert(node_info.id, node_info);
        drop(nodes);
        drop(ring);

        if is_new {
            self.node_recovered_notify.notify_waiters();
        }

        Ok(())
    }

    /// Remove a node from the cluster
    pub async fn remove_node(&self, node_id: &NodeId) -> Result<()> {
        let mut nodes = self.nodes.write().await;
        let mut ring = self.hash_ring.write().await;

        if nodes.remove(node_id).is_some() {
            info!("Removed node {} from cluster", node_id);
            ring.remove_node(node_id);
        }

        Ok(())
    }

    /// Update heartbeat for a node
    pub async fn update_heartbeat(&self, node_id: &NodeId) -> Result<()> {
        let mut nodes = self.nodes.write().await;

        if let Some(node) = nodes.get_mut(node_id) {
            node.update_heartbeat();
            node.status = NodeStatus::Online;
            debug!("Updated heartbeat for node {}", node_id);
        }

        Ok(())
    }

    /// Merge received cluster view gossip with local view
    /// Conflict resolution: Most recent information wins
    /// Optimistic bias: If ANY node thinks another is online, mark it online
    pub async fn merge_cluster_gossip(&self, gossip: Vec<dfs_common::NodeHealthGossip>) -> Result<()> {
        let mut nodes = self.nodes.write().await;

        for gossip_entry in gossip {
            // Skip self
            if gossip_entry.node_id == self.local_node_id {
                continue;
            }

            if let Some(local_node) = nodes.get_mut(&gossip_entry.node_id) {
                // CONFLICT RESOLUTION: Most recent info wins
                if gossip_entry.last_seen > local_node.last_heartbeat {
                    debug!(
                        "Gossip: Updating node {} from gossip (local: {}s ago, gossip: {}s ago)",
                        gossip_entry.node_id,
                        dfs_common::types::current_timestamp().saturating_sub(local_node.last_heartbeat),
                        dfs_common::types::current_timestamp().saturating_sub(gossip_entry.last_seen)
                    );

                    local_node.last_heartbeat = gossip_entry.last_seen;

                    // OPTIMISTIC BIAS: If ANY node thinks it's online, mark it online
                    // This prevents false positives from network partitions
                    if gossip_entry.status == NodeStatus::Online {
                        // If we thought it was failed, resurrect it
                        if local_node.status == NodeStatus::Failed {
                            info!("Gossip: Node {} recovered (via gossip)", gossip_entry.node_id);
                            local_node.status = NodeStatus::Online;

                            // Add back to hash ring
                            let mut ring = self.hash_ring.write().await;
                            ring.add_node(gossip_entry.node_id);
                        } else if local_node.status == NodeStatus::Suspected {
                            debug!("Gossip: Node {} no longer suspected (via gossip)", gossip_entry.node_id);
                            local_node.status = NodeStatus::Online;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Get information about a specific node
    pub async fn get_node(&self, node_id: &NodeId) -> Option<NodeInfo> {
        let nodes = self.nodes.read().await;
        nodes.get(node_id).cloned()
    }

    /// Get all nodes in the cluster
    pub async fn get_all_nodes(&self) -> Vec<NodeInfo> {
        let nodes = self.nodes.read().await;
        nodes.values().cloned().collect()
    }

    /// Returns true if this node is the current cluster leader.
    ///
    /// Leader = the online node with the minimum NodeId, BUT only if this node
    /// can see a strict majority of the cluster as online (quorum). Without the
    /// quorum gate, a node that just restarted and only sees itself will declare
    /// itself leader and run heal/catchup against the real leader's partition.
    ///
    /// Leadership transfers automatically when the current leader goes offline
    /// and a majority of remaining nodes agree on the new minimum-ID leader.
    pub async fn is_leader(&self) -> bool {
        let nodes = self.nodes.read().await;
        let total = nodes.len();
        let online_ids: Vec<NodeId> = nodes
            .values()
            .filter(|n| n.status == NodeStatus::Online)
            .map(|n| n.id)
            .collect();
        let quorum = total / 2 + 1;
        if online_ids.len() < quorum {
            return false;
        }
        online_ids.iter().min() == Some(&self.local_node_id)
    }

    /// Returns the SocketAddr of the current leader, if known.
    pub async fn get_leader_addr(&self) -> Option<std::net::SocketAddr> {
        let nodes = self.nodes.read().await;
        nodes.values()
            .filter(|n| n.status == NodeStatus::Online)
            .min_by_key(|n| n.id)
            .map(|n| n.addr)
    }

    /// Returns true if the given node_id is the current leader per this node's gossip view.
    /// Used to validate incoming healing instructions — if the sender claims to be leader
    /// but our view disagrees, we reject the instruction to prevent split-brain execution.
    pub async fn is_leader_id(&self, node_id: NodeId) -> bool {
        let nodes = self.nodes.read().await;
        let leader_id = nodes
            .values()
            .filter(|n| n.status == NodeStatus::Online)
            .map(|n| n.id)
            .min();
        leader_id == Some(node_id)
    }

    /// Returns true if a strict majority of known nodes are online.
    ///
    /// Quorum = floor(total / 2) + 1, e.g. 3-of-5, 2-of-3.
    /// The leader must have quorum before taking any destructive / irreversible
    /// action (orphan purge, over-replication cleanup). This prevents a partitioned
    /// leader from deleting data that the majority partition still considers live.
    /// Non-destructive healing (adding replicas) is NOT gated on quorum — it is
    /// always safe to create more copies.
    pub async fn has_quorum(&self) -> bool {
        let nodes = self.nodes.read().await;
        let total = nodes.len();
        let online = nodes
            .values()
            .filter(|n| n.status == NodeStatus::Online)
            .count();
        let quorum = total / 2 + 1;
        online >= quorum
    }

    /// Record the moment this node became leader. Call on every leader transition.
    pub async fn notify_became_leader(&self) {
        *self.became_leader_at.write().await = Some(std::time::Instant::now());
    }

    /// How long ago this node became leader, or None if it has never been leader.
    pub async fn time_since_became_leader(&self) -> Option<std::time::Duration> {
        self.became_leader_at.read().await.map(|t| t.elapsed())
    }

    /// Get online nodes count
    pub async fn online_node_count(&self) -> usize {
        let nodes = self.nodes.read().await;
        nodes
            .values()
            .filter(|n| n.status == NodeStatus::Online)
            .count()
    }

    /// Get nodes responsible for a chunk (using consistent hashing)
    pub async fn get_nodes_for_chunk(
        &self,
        chunk_id: &dfs_common::ChunkId,
        count: usize,
    ) -> Vec<NodeId> {
        let ring = self.hash_ring.read().await;
        ring.get_nodes(chunk_id, count)
    }

    /// Get primary node for a chunk
    pub async fn get_primary_node(
        &self,
        chunk_id: &dfs_common::ChunkId,
    ) -> Option<NodeId> {
        let ring = self.hash_ring.read().await;
        ring.get_primary_node(chunk_id)
    }

    /// Get nodes for chunk with smart replica set selection
    /// Picks the replica set with highest minimum capacity to prevent small nodes from bottlenecking
    /// Works for any replication factor (not hardcoded to triplets)
    pub async fn get_nodes_with_capacity_awareness(
        &self,
        chunk_id: &dfs_common::ChunkId,
        count: usize,
    ) -> Vec<NodeId> {
        let ring = self.hash_ring.read().await;
        let all_nodes: Vec<NodeId> = ring.nodes().to_vec();
        drop(ring);

        // Use first 8 bytes of chunk_id hash as a per-chunk rotation seed.
        // This ensures that when we take the top 'count' nodes, different chunks
        // land on different node pairs — distributing write load across the cluster.
        let seed = u64::from_le_bytes(chunk_id.hash[..8].try_into().unwrap_or([0u8; 8]));

        if all_nodes.len() <= count {
            // All nodes are included — but rotate the list so that taking the first
            // 'immediate_replicas' subset picks a different pair per chunk.
            let n = all_nodes.len();
            let offset = (seed % n as u64) as usize;
            let mut rotated = all_nodes[offset..].to_vec();
            rotated.extend_from_slice(&all_nodes[..offset]);
            return rotated;
        }

        // Get capacity information
        let capacities = self.node_capacities.read().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Build list of (node_id, available_capacity)
        let mut node_capacities_vec: Vec<(NodeId, u64)> = Vec::new();
        for node_id in &all_nodes {
            let available = if let Some(cap) = capacities.get(node_id) {
                if cap.total > 0 && now - cap.last_updated < 60 {
                    cap.available
                } else {
                    // Stale data, assume moderate capacity
                    cap.total / 2
                }
            } else {
                // No capacity data, assume moderate capacity (1TB)
                1_000_000_000_000
            };
            node_capacities_vec.push((*node_id, available));
        }

        // SMART REPLICA SET SELECTION (Greedy Algorithm with tiebreak by chunk hash):
        // Sort nodes by available capacity descending and take top 'count' nodes.
        // When capacities are equal, use the chunk_id hash to break ties differently
        // per chunk — this distributes chunks evenly across nodes without sacrificing
        // capacity-awareness.
        //
        // Example: RF=2, 3 nodes all equal (100G each)
        //   chunk hash rotates the starting position → chunks spread across all 3 nodes
        //
        // Example: RF=3, nodes (100G, 100G, 100G, 10G)
        //   capacity sort still excludes the 10G node; hash only breaks ties within
        //   the top-capacity group → min=100G is still guaranteed

        let n = node_capacities_vec.len() as u64;

        // Assign each node a stable index (by current position), then compute its
        // rotated rank using the chunk seed.
        let mut indexed: Vec<(usize, NodeId, u64)> = node_capacities_vec
            .into_iter()
            .enumerate()
            .map(|(i, (id, cap))| (i, id, cap))
            .collect();

        indexed.sort_by_key(|(i, _, cap)| {
            // Primary: higher capacity is better (sort ascending by negated capacity).
            // Secondary: rotate node index by seed so equal-capacity nodes appear in
            //            a different order for each chunk.
            let rotated_rank = (*i as u64 + seed) % n;
            // Pack into a u128: high bits = inverse capacity (so more cap = lower key),
            // low bits = rotated rank.
            let inv_cap = u64::MAX - cap;
            (inv_cap as u128) << 64 | rotated_rank as u128
        });

        indexed
            .into_iter()
            .take(count)
            .map(|(_, node_id, _)| node_id)
            .collect()
    }

    /// From a given set of node IDs, return the one with the least available space
    /// (i.e. most utilized). Used by the leader to pick which excess replica to remove
    /// during over-replication cleanup — shedding from the fullest node naturally
    /// rebalances the cluster over time.
    ///
    /// Falls back to the last node in the input slice if capacity data is unavailable.
    pub async fn most_utilized_node(&self, node_ids: &[NodeId]) -> Option<NodeId> {
        if node_ids.is_empty() {
            return None;
        }

        let capacities = self.node_capacities.read().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Pick the node with the smallest available space (most utilized).
        // Nodes with stale or missing capacity data are treated as moderately utilized.
        node_ids.iter().copied().min_by_key(|node_id| {
            if let Some(cap) = capacities.get(node_id) {
                if cap.total > 0 && now - cap.last_updated < 60 {
                    return cap.available;
                }
            }
            // No fresh data — assume moderate (sort to middle)
            u64::MAX / 2
        })
    }

    /// Update capacity information for a node
    /// Return aggregate (total_space, available_space) across all nodes with recent capacity
    /// data, divided by replication_factor so the result reflects logical (user-visible)
    /// capacity rather than raw physical capacity.  Nodes whose data is stale (> 5 min old)
    /// are excluded so a dead node doesn't permanently deflate reported free space.
    pub async fn get_aggregate_capacity(&self, replication_factor: u64) -> (u64, u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        const STALE_SECS: u64 = 300;
        let rf = replication_factor.max(1);

        let capacities = self.node_capacities.read().await;
        let mut total_sum: u64 = 0;
        let mut available_sum: u64 = 0;
        let mut count: u64 = 0;
        for cap in capacities.values() {
            if now.saturating_sub(cap.last_updated) <= STALE_SECS {
                total_sum = total_sum.saturating_add(cap.total);
                available_sum = available_sum.saturating_add(cap.available);
                count += 1;
            }
        }
        if count == 0 {
            return (0, 0);
        }
        (total_sum / rf, available_sum / rf)
    }

    pub async fn update_node_capacity(&self, node_id: NodeId, available: u64, total: u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut capacities = self.node_capacities.write().await;
        capacities.insert(
            node_id,
            NodeCapacity {
                available,
                total,
                last_updated: now,
            },
        );
    }

    /// Start background task to check for failed nodes
    pub async fn start_failure_detector(self: Arc<Self>) {
        let mut check_interval = interval(Duration::from_secs(self.heartbeat_interval));
        let mut cleanup_counter = 0;

        tokio::spawn(async move {
            loop {
                check_interval.tick().await;

                if let Err(e) = self.check_failed_nodes().await {
                    warn!("Error checking for failed nodes: {}", e);
                }

                // Periodic cleanup of stale capacity data (every 10 checks = ~100 seconds)
                cleanup_counter += 1;
                if cleanup_counter >= 10 {
                    cleanup_counter = 0;
                    if let Err(e) = self.cleanup_stale_capacities().await {
                        warn!("Error cleaning up stale capacities: {}", e);
                    }
                }
            }
        });
    }

    /// Start background task to send heartbeats to all nodes
    pub async fn start_heartbeat_sender(self: Arc<Self>) {
        let mut heartbeat_interval = interval(Duration::from_secs(self.heartbeat_interval));
        let mut probe_counter = 0u32;

        tokio::spawn(async move {
            loop {
                heartbeat_interval.tick().await;
                probe_counter += 1;

                // Probe failed nodes every 6 intervals (~60s at default 10s heartbeat).
                // This breaks the mutual-silence deadlock that occurs when both sides
                // mark each other Failed and stop sending heartbeats — probing lets a
                // recovered node announce itself without requiring a manual restart.
                let probe_failed = probe_counter % 6 == 0;

                if let Err(e) = self.send_heartbeats(probe_failed).await {
                    warn!("Error sending heartbeats: {}", e);
                }
            }
        });
    }

    /// Send heartbeats to all nodes in the cluster.
    ///
    /// When `probe_failed` is true, also sends to Failed nodes so they can
    /// recover after a reboot or network partition without manual intervention.
    async fn send_heartbeats(&self, probe_failed: bool) -> Result<()> {
        use dfs_common::protocol::{ClusterMessage, Message, MessageEnvelope, RequestId};
        use dfs_common::{NodeHealthGossip, NodeInfo};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let nodes = self.nodes.read().await.clone();
        let local_node_id = self.local_node_id;
        let local_addr = self.local_addr;

        // Build cluster view for gossiping - include our view of all nodes
        let cluster_view: Vec<NodeHealthGossip> = nodes
            .values()
            .map(|node| NodeHealthGossip {
                node_id: node.id,
                last_seen: node.last_heartbeat,
                status: node.status,
            })
            .collect();

        for (node_id, node_info) in nodes {
            // Skip self
            if node_id == local_node_id {
                continue;
            }

            // Skip failed nodes unless this is a recovery probe cycle
            if node_info.status == NodeStatus::Failed && !probe_failed {
                continue;
            }

            let local_node_info = NodeInfo::new(local_node_id, local_addr, None);
            let heartbeat = ClusterMessage::Heartbeat {
                node_info: local_node_info,
                cluster_view: cluster_view.clone(),
            };

            // Send heartbeat asynchronously (don't wait for response)
            let target_addr = node_info.addr;
            let is_probe = node_info.status == NodeStatus::Failed;
            tokio::spawn(async move {
                if let Err(e) = send_heartbeat_message(target_addr, heartbeat).await {
                    if is_probe {
                        debug!("Recovery probe to failed node {} unreachable: {}", target_addr, e);
                    } else {
                        debug!("Failed to send heartbeat to {}: {}", target_addr, e);
                    }
                } else if is_probe {
                    debug!("Recovery probe sent to failed node {}", target_addr);
                }
            });
        }

        Ok(())
    }

    /// Check for nodes that have failed (no heartbeat within timeout)
    /// State machine: Online → Suspected → Failed
    /// This gives gossip a chance to correct false positives before marking nodes as Failed
    async fn check_failed_nodes(&self) -> Result<()> {
        let mut nodes = self.nodes.write().await;
        let mut failed_nodes = Vec::new();
        let mut recovered_nodes = Vec::new();
        let mut nodes_to_purge = Vec::new();

        let now = dfs_common::types::current_timestamp();

        // Threshold for purging: failed for 24 hours
        let purge_threshold = self.failure_timeout * 24 * 3600 / self.failure_timeout;

        for (node_id, node_info) in nodes.iter_mut() {
            // Skip local node
            if node_id == &self.local_node_id {
                continue;
            }

            let time_since_heartbeat = now.saturating_sub(node_info.last_heartbeat);
            let is_timed_out = time_since_heartbeat > self.failure_timeout;

            // State machine: Online -> Suspected -> Failed
            match node_info.status {
                NodeStatus::Online => {
                    if is_timed_out {
                        // FIRST timeout: Mark as Suspected (not Failed)
                        // Gives gossip a chance to fix false positives
                        warn!(
                            "Node {} suspected failed ({}s since heartbeat, threshold={}s)",
                            node_id, time_since_heartbeat, self.failure_timeout
                        );
                        node_info.status = NodeStatus::Suspected;
                    }
                }
                NodeStatus::Suspected => {
                    if is_timed_out {
                        // SECOND timeout: Now mark as Failed (requires 2x threshold total)
                        warn!(
                            "Node {} confirmed failed ({}s since heartbeat)",
                            node_id, time_since_heartbeat
                        );
                        node_info.status = NodeStatus::Failed;
                        failed_nodes.push(*node_id);
                    } else {
                        // Recovered via gossip!
                        info!(
                            "Node {} recovered from suspected state ({}s since heartbeat)",
                            node_id, time_since_heartbeat
                        );
                        node_info.status = NodeStatus::Online;
                        recovered_nodes.push(*node_id);
                    }
                }
                NodeStatus::Failed => {
                    if !is_timed_out {
                        // Recovered from failure via gossip
                        info!(
                            "Node {} recovered from failed state ({}s since heartbeat)",
                            node_id, time_since_heartbeat
                        );
                        node_info.status = NodeStatus::Online;
                        recovered_nodes.push(*node_id);
                    } else {
                        // Still failed - check if we should purge it (failed for too long)
                        if node_info.is_failed(purge_threshold) {
                            nodes_to_purge.push(*node_id);
                        }
                    }
                }
                NodeStatus::Leaving => {
                    // Graceful shutdown - leave alone
                }
            }
        }

        // Remove failed nodes from hash ring
        if !failed_nodes.is_empty() {
            let mut ring = self.hash_ring.write().await;
            for node_id in failed_nodes {
                info!("Removing failed node {} from hash ring", node_id);
                ring.remove_node(&node_id);
            }
        }

        // Add recovered nodes back to hash ring
        if !recovered_nodes.is_empty() {
            let mut ring = self.hash_ring.write().await;
            for node_id in recovered_nodes {
                info!("Adding recovered node {} back to hash ring", node_id);
                ring.add_node(node_id);
            }
            self.node_recovered_notify.notify_waiters();
        }

        // Purge long-failed nodes from nodes HashMap to prevent memory leak
        if !nodes_to_purge.is_empty() {
            info!("Purging {} long-failed nodes from memory", nodes_to_purge.len());
            for node_id in nodes_to_purge {
                nodes.remove(&node_id);
                debug!("Purged node {} (failed for >24h)", node_id);
            }
        }

        Ok(())
    }

    /// Cleanup stale node capacity data
    /// Called periodically to remove capacity info for nodes that no longer exist
    async fn cleanup_stale_capacities(&self) -> Result<()> {
        let nodes = self.nodes.read().await;
        let mut capacities = self.node_capacities.write().await;

        let valid_node_ids: std::collections::HashSet<_> = nodes.keys().cloned().collect();
        let capacity_node_ids: Vec<_> = capacities.keys().cloned().collect();

        let mut removed = 0;
        for node_id in capacity_node_ids {
            if !valid_node_ids.contains(&node_id) {
                capacities.remove(&node_id);
                removed += 1;
            }
        }

        if removed > 0 {
            info!("Cleaned up capacity data for {} removed nodes", removed);
        }

        Ok(())
    }

    /// Mark a node as recovered
    pub async fn mark_node_recovered(&self, node_id: &NodeId) -> Result<()> {
        let mut nodes = self.nodes.write().await;

        if let Some(node) = nodes.get_mut(node_id) {
            if node.status == NodeStatus::Failed {
                info!("Node {} has recovered", node_id);
                node.status = NodeStatus::Online;
                node.update_heartbeat();

                // Add back to hash ring
                let mut ring = self.hash_ring.write().await;
                ring.add_node(*node_id);
            }
        }

        Ok(())
    }

    /// Get cluster statistics
    pub async fn get_stats(&self) -> ClusterStats {
        let nodes = self.nodes.read().await;

        let total_nodes = nodes.len();
        let online_nodes = nodes
            .values()
            .filter(|n| n.status == NodeStatus::Online)
            .count();
        let failed_nodes = nodes
            .values()
            .filter(|n| n.status == NodeStatus::Failed)
            .count();

        ClusterStats {
            total_nodes,
            online_nodes,
            failed_nodes,
        }
    }

    /// Check if a node is healthy
    pub fn is_node_healthy(&self, node_id: &NodeId) -> bool {
        if node_id == &self.local_node_id {
            return true;
        }

        // For remote nodes, we need async access, so just return true for now
        // In a real implementation, this should check the node status
        true
    }

    /// Load persisted peer list from disk
    pub async fn load_persisted_peers(metadata_dir: &Path) -> Result<Vec<SocketAddr>> {
        let peers_file = metadata_dir.join("peers.json");

        if !peers_file.exists() {
            debug!("No persisted peers file found at {}", peers_file.display());
            return Ok(Vec::new());
        }

        let data = tokio::fs::read_to_string(&peers_file).await?;
        let persisted: PersistedPeers = serde_json::from_str(&data)?;

        info!("✓ Loaded {} persisted peers from {}", persisted.peers.len(), peers_file.display());
        Ok(persisted.peers)
    }

    /// Save peer list to disk for cluster recovery
    pub async fn save_persisted_peers(peers: &[SocketAddr], metadata_dir: &Path) -> Result<()> {
        let persisted = PersistedPeers {
            peers: peers.to_vec(),
            last_updated: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };

        let peers_file = metadata_dir.join("peers.json");
        let data = serde_json::to_string_pretty(&persisted)?;
        tokio::fs::write(&peers_file, data).await?;

        debug!("✓ Saved {} peers to {}", peers.len(), peers_file.display());
        Ok(())
    }

    /// Get all peer addresses (excluding self) for persistence
    pub async fn get_all_peer_addrs(&self) -> Vec<SocketAddr> {
        let nodes = self.nodes.read().await;
        nodes
            .values()
            .filter(|n| n.id != self.local_node_id) // Exclude self
            .map(|n| n.addr)
            .collect()
    }
}

/// Cluster statistics
#[derive(Debug, Clone)]
pub struct ClusterStats {
    pub total_nodes: usize,
    pub online_nodes: usize,
    pub failed_nodes: usize,
}

/// Helper function to send heartbeat message to a node
async fn send_heartbeat_message(
    target_addr: SocketAddr,
    heartbeat: dfs_common::protocol::ClusterMessage,
) -> Result<()> {
    use dfs_common::protocol::{Message, MessageEnvelope, RequestId};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    // Connect to target node (5s timeout to prevent fd leaks when peers are overloaded)
    let mut stream = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        TcpStream::connect(target_addr),
    ).await
        .map_err(|_| anyhow::anyhow!("Heartbeat connect timeout to {}", target_addr))??;

    // Create message envelope
    let request_id = RequestId::new(0); // Heartbeats don't need tracking
    let envelope = MessageEnvelope::new(request_id, Message::Cluster(heartbeat));
    let encoded = envelope.to_bytes()?;

    // Send length prefix + message
    stream.write_u32(encoded.len() as u32).await?;
    stream.write_all(&encoded).await?;
    stream.flush().await?;

    // Shut down the write half so the server sees EOF immediately and exits
    // handle_connection without waiting for the idle timeout. We don't need
    // the response — heartbeats are fire-and-forget.
    let _ = stream.shutdown().await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_add_remove_node() {
        let local_id = NodeId::new();
        let local_addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let manager = ClusterManager::new(local_id, local_addr, 10, 30);

        // Add a node
        let node_id = NodeId::new();
        let node_addr: SocketAddr = "127.0.0.1:8901".parse().unwrap();
        let node_info = NodeInfo::new(node_id, node_addr, None);

        manager.add_node(node_info).await.unwrap();

        // Should have 2 nodes now (local + added)
        assert_eq!(manager.get_all_nodes().await.len(), 2);

        // Remove node
        manager.remove_node(&node_id).await.unwrap();

        // Should have 1 node (just local)
        assert_eq!(manager.get_all_nodes().await.len(), 1);
    }

    #[tokio::test]
    async fn test_heartbeat() {
        let local_id = NodeId::new();
        let local_addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let manager = ClusterManager::new(local_id, local_addr, 10, 30);

        let node_id = NodeId::new();
        let node_addr: SocketAddr = "127.0.0.1:8901".parse().unwrap();
        let node_info = NodeInfo::new(node_id, node_addr, None);

        manager.add_node(node_info).await.unwrap();

        // Update heartbeat
        manager.update_heartbeat(&node_id).await.unwrap();

        let node = manager.get_node(&node_id).await.unwrap();
        assert_eq!(node.status, NodeStatus::Online);
    }

    #[tokio::test]
    async fn test_consistent_hashing() {
        let local_id = NodeId::new();
        let local_addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let manager = ClusterManager::new(local_id, local_addr, 10, 30);

        // Add two more nodes
        let node2 = NodeId::new();
        let node3 = NodeId::new();

        manager
            .add_node(NodeInfo::new(
                node2,
                "127.0.0.1:8901".parse().unwrap(),
                None,
            ))
            .await
            .unwrap();

        manager
            .add_node(NodeInfo::new(
                node3,
                "127.0.0.1:8902".parse().unwrap(),
                None,
            ))
            .await
            .unwrap();

        // Get nodes for a chunk
        let chunk_id = dfs_common::ChunkId::from_hash([0u8; 32]);
        let nodes = manager.get_nodes_for_chunk(&chunk_id, 3).await;

        assert_eq!(nodes.len(), 3);
        assert_ne!(nodes[0], nodes[1]);
        assert_ne!(nodes[1], nodes[2]);
    }

    #[tokio::test]
    async fn test_cluster_stats() {
        let local_id = NodeId::new();
        let local_addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let manager = ClusterManager::new(local_id, local_addr, 10, 30);

        let stats = manager.get_stats().await;
        assert_eq!(stats.total_nodes, 1);
        assert_eq!(stats.online_nodes, 1);
        assert_eq!(stats.failed_nodes, 0);
    }
}
