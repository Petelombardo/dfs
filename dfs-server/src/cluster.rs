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

/// Decide the epoch (unix secs) this leadership episode "started" at.
///
/// If the same node was already the leader before (persisted), carry over its
/// original start time so the post-election grace period
/// (`healing::LEADER_CHANGE_GRACE_SECS`) doesn't restart on a simple process
/// restart of the same perpetual leader. Otherwise — a different node was
/// previously leader, or there's no prior record — this is a genuine new
/// election, so start the clock now.
pub fn resolve_became_leader_epoch(
    prev_leader: Option<NodeId>,
    prev_since_secs: Option<u64>,
    local_id: NodeId,
    now_secs: u64,
) -> u64 {
    if prev_leader == Some(local_id) {
        prev_since_secs.unwrap_or(now_secs)
    } else {
        now_secs
    }
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
        let capacities = self.node_capacities.read().await;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        nodes.values().map(|n| {
            let mut info = n.clone();
            if let Some(cap) = capacities.get(&n.id) {
                if cap.total > 0 && now.saturating_sub(cap.last_updated) < 120 {
                    info.available_bytes = cap.available;
                    info.total_bytes = cap.total;
                }
            }
            info
        }).collect()
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

    /// Set `became_leader_at` so that `time_since_became_leader()` reflects
    /// `became_leader_at_secs` (a unix-epoch second), not just "now". Used on a
    /// leader-election transition together with `resolve_became_leader_epoch` so
    /// that the LEADER_CHANGE_GRACE_SECS countdown carries over across a restart
    /// of the same perpetual leader instead of restarting from zero.
    pub async fn set_became_leader_epoch(&self, became_leader_at_secs: u64, now_secs: u64) {
        let age = now_secs.saturating_sub(became_leader_at_secs);
        let became_leader_at = std::time::Instant::now()
            .checked_sub(Duration::from_secs(age))
            .unwrap_or_else(std::time::Instant::now);
        *self.became_leader_at.write().await = Some(became_leader_at);
    }

    /// Grace period after a GracefulLeave before healing treats the node as Failed.
    /// 60s is enough time for a normal restart (stop ~5s + start ~5s + margin).
    pub const LEAVING_GRACE_SECS: u64 = 60;

    /// Mark a remote node as Leaving immediately, recording when it left.
    /// Called when we receive a GracefulLeave broadcast from that node.
    pub async fn set_leaving(&self, node_id: NodeId, reason: dfs_common::LeaveReason) {
        let now = dfs_common::types::current_timestamp();
        let mut nodes = self.nodes.write().await;
        if let Some(info) = nodes.get_mut(&node_id) {
            info.status = NodeStatus::Leaving;
            info.leaving_at = now;
            info.leave_reason = Some(reason);
            // Set last_heartbeat to 0 so the failure detector's is_timed_out check
            // is immediately true.  Without this, the stale fresh heartbeat timestamp
            // triggers the "!is_timed_out → rejoined" branch on the very next tick,
            // resetting the node back to Online before it's actually gone.
            info.last_heartbeat = 0;
        }
        // Remove from hash ring so no new chunks are placed on a leaving node.
        let mut ring = self.hash_ring.write().await;
        ring.remove_node(&node_id);
    }

    /// Mark THIS node as Leaving and broadcast GracefulLeave to all peers.
    /// Leadership re-elects immediately on the receiving side.
    pub async fn announce_leaving(&self, reason: dfs_common::LeaveReason) {
        use dfs_common::protocol::{ClusterMessage, Message, MessageEnvelope, RequestId};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        // Mark ourselves Leaving locally first.
        {
            let now = dfs_common::types::current_timestamp();
            let mut nodes = self.nodes.write().await;
            if let Some(info) = nodes.get_mut(&self.local_node_id) {
                info.status = NodeStatus::Leaving;
                info.leaving_at = now;
                info.leave_reason = Some(reason);
            }
        }

        let msg = Message::Cluster(ClusterMessage::GracefulLeave {
            node_id: self.local_node_id,
            addr: self.local_addr,
            reason,
        });
        let envelope = MessageEnvelope::new(
            RequestId::new(0),
            msg,
        );
        let encoded = match envelope.to_bytes() {
            Ok(b) => b,
            Err(e) => { warn!("announce_leaving: failed to encode message: {}", e); return; }
        };

        let peers: Vec<SocketAddr> = {
            let nodes = self.nodes.read().await;
            nodes.values()
                .filter(|n| n.id != self.local_node_id && n.status != NodeStatus::Failed)
                .map(|n| n.addr)
                .collect()
        };

        let encoded = Arc::new(encoded);
        let mut tasks = Vec::new();
        for addr in peers {
            let encoded = encoded.clone();
            tasks.push(tokio::spawn(async move {
                let timeout = tokio::time::Duration::from_millis(500);
                if let Ok(Ok(mut stream)) = tokio::time::timeout(
                    timeout,
                    TcpStream::connect(addr),
                ).await {
                    let len = (encoded.len() as u32).to_be_bytes();
                    let _ = stream.write_all(&len).await;
                    let _ = stream.write_all(&encoded).await;
                    // Read and discard the response so the server-side socket closes cleanly.
                    let mut buf = [0u8; 4];
                    let _ = tokio::time::timeout(timeout, stream.read_exact(&mut buf)).await;
                }
            }));
        }
        for t in tasks { let _ = t.await; }
    }

    /// Mark THIS node as Online again after connection pressure has resolved.
    /// The next heartbeat will propagate the recovery to peers.
    pub async fn announce_recovery(&self) {
        let mut nodes = self.nodes.write().await;
        if let Some(info) = nodes.get_mut(&self.local_node_id) {
            info.status = NodeStatus::Online;
            info.leaving_at = 0;
            info.leave_reason = None;
        }
        // Re-add to hash ring so chunks can be placed here again.
        let mut ring = self.hash_ring.write().await;
        ring.add_node(self.local_node_id);
        info!("Connection pressure resolved — rejoining cluster as Online");
        self.node_recovered_notify.notify_waiters();
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

    /// Get nodes for chunk with capacity-banded placement.
    ///
    /// # Why banded, not sorted
    ///
    /// Sorting by exact available bytes makes capacity the total ordering key: a node
    /// with even 1 byte more space always beats another, so the hash-based distribution
    /// only fires when capacities are byte-for-byte identical (never in practice).
    /// Worse, the rotation `(i + seed) % n` only produces CONSECUTIVE pairs in ring
    /// order — for n=5 RF=2, only 5 of 10 possible pairs are ever selected regardless
    /// of how uniformly the seed is distributed.
    ///
    /// # Algorithm
    ///
    /// 1. Exclude nodes with less than 20 GB free — hard veto against ENOSPC; banding
    ///    handles preference among eligible nodes, so this is a last-resort guard only.
    /// 2. Bucket remaining nodes into 10 equal-width capacity bands (band 0 = most
    ///    available, band 9 = least available).  Nodes within the same band are treated
    ///    as equal for placement purposes.
    /// 3. Use deterministic Fisher-Yates (seeded from the chunk hash) to shuffle
    ///    nodes within each band; concatenate bands best-first to form a priority list.
    /// 4. Take the first `count` nodes.
    ///
    /// This lets the hash distribute chunks uniformly across all C(n, count) pairs when
    /// nodes are similarly loaded, while still steering away from nodes that are
    /// significantly fuller than their peers.
    pub async fn get_nodes_with_capacity_awareness(
        &self,
        chunk_id: &dfs_common::ChunkId,
        count: usize,
    ) -> Vec<NodeId> {
        let ring = self.hash_ring.read().await;
        let all_nodes: Vec<NodeId> = ring.nodes().to_vec();
        drop(ring);

        let seed = u64::from_le_bytes(chunk_id.hash[..8].try_into().unwrap_or([0u8; 8]));

        if all_nodes.len() <= count {
            let n = all_nodes.len();
            let offset = (seed % n as u64) as usize;
            let mut rotated = all_nodes[offset..].to_vec();
            rotated.extend_from_slice(&all_nodes[..offset]);
            return rotated;
        }

        let capacities = self.node_capacities.read().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Build (node_id, available, total) for each node.
        // Staleness windows:
        //   < 300s — use real values. This ensures a nearly-full node stays vetoed for
        //            up to 5 minutes after a heartbeat gap (e.g. post-rolling-restart),
        //            rather than the old 60s window that caused the healer to assume 50%
        //            free and flood the node during the post-restart heartbeat gap.
        //   > 300s — very stale / never seen: assume 50% free (new node joining or
        //            extended connectivity gap — give benefit of the doubt).
        let node_caps: Vec<(NodeId, u64, u64)> = all_nodes.iter().map(|node_id| {
            let (available, total) = if let Some(cap) = capacities.get(node_id) {
                if cap.total > 0 && now.saturating_sub(cap.last_updated) < 300 {
                    (cap.available, cap.total)
                } else {
                    (cap.total / 2, cap.total)
                }
            } else {
                (1_000_000_000_000u64, 2_000_000_000_000u64)
            };
            (*node_id, available, total)
        }).collect();
        drop(capacities);

        // Hard veto: skip nodes with less than 20 GB free (absolute, not percentage).
        // Percentage-based thresholds are wrong for large disks — 10% of 932 GB = 93 GB
        // reserved, far more than needed to avoid ENOSPC.  20 GB gives ~5 000 × 4 MB chunks
        // of headroom as a last-resort guard; banding handles preference among eligible nodes.
        const MIN_FREE_BYTES: u64 = 20 * 1024 * 1024 * 1024;
        let eligible: Vec<(NodeId, u64, u64)> = node_caps.iter()
            .filter(|(_, avail, total)| *total == 0 || *avail >= MIN_FREE_BYTES)
            .cloned()
            .collect();

        // If the veto left fewer than `count` nodes, fall back to all nodes sorted by
        // most available — we need to place somewhere even if everything is nearly full.
        let mut candidates: Vec<(NodeId, u64, u64)> = if eligible.len() >= count {
            eligible
        } else {
            let mut all = node_caps;
            all.sort_by(|a, b| b.1.cmp(&a.1));
            all
        };

        // Compute capacity bands: 10 equal-width buckets spanning [min_avail, max_avail].
        // Nodes in band 0 are the most available, band 9 the least.
        let max_avail = candidates.iter().map(|(_, a, _)| *a).max().unwrap_or(1).max(1);
        let min_avail = candidates.iter().map(|(_, a, _)| *a).min().unwrap_or(0);
        let band_width = (max_avail - min_avail) / 10 + 1;

        // Group nodes by band (0 = best).
        let mut bands: Vec<Vec<NodeId>> = vec![Vec::new(); 10];
        for (node_id, avail, _) in &candidates {
            let band = (max_avail - avail) / band_width;
            bands[band.min(9) as usize].push(*node_id);
        }

        // Deterministic Fisher-Yates shuffle within each band using the chunk seed.
        // A different multiplier per band so adjacent bands don't correlate.
        let mut result: Vec<NodeId> = Vec::with_capacity(count);
        for (band_idx, band) in bands.iter_mut().enumerate() {
            if band.is_empty() { continue; }
            let band_seed = seed.wrapping_add(band_idx as u64 * 0x9e3779b97f4a7c15);
            // Knuth shuffle: for i from len-1 down to 1, swap i with j = hash(i,seed)%i+1
            let n = band.len();
            for i in (1..n).rev() {
                let mix = band_seed.wrapping_mul(i as u64 + 1).wrapping_add(0x517cc1b727220a95);
                let j = (mix >> 33) as usize % (i + 1);
                band.swap(i, j);
            }
            for node_id in band.iter() {
                result.push(*node_id);
                if result.len() == count { return result; }
            }
        }

        // Fallback: shouldn't be reached if candidates.len() >= count.
        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        candidates.into_iter().take(count).map(|(id, _, _)| id).collect()
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
                    // After the grace period, treat as Failed so healing can kick in.
                    // Before that, the node may still come back (rolling restart / recovery).
                    let grace_expired = node_info.leaving_at > 0
                        && now.saturating_sub(node_info.leaving_at) > Self::LEAVING_GRACE_SECS;
                    if grace_expired {
                        warn!("Node {} leaving grace period expired — treating as Failed", node_id);
                        node_info.status = NodeStatus::Failed;
                        failed_nodes.push(*node_id);
                    } else if !is_timed_out {
                        // Node sent a heartbeat — it came back before grace expired.
                        info!("Node {} rejoined after graceful leave", node_id);
                        node_info.status = NodeStatus::Online;
                        node_info.leaving_at = 0;
                        node_info.leave_reason = None;
                        recovered_nodes.push(*node_id);
                    }
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

    /// resolve_became_leader_epoch: same node regaining leadership carries over its
    /// original start time; a different (or no) prior leader gets a fresh clock.
    #[test]
    fn test_resolve_became_leader_epoch() {
        let node_x = NodeId::new();
        let node_y = NodeId::new();
        let now = 1_000_000u64;

        // Same node was leader before — carry over the original start time.
        assert_eq!(
            resolve_became_leader_epoch(Some(node_x), Some(now - 1000), node_x, now),
            now - 1000
        );

        // A different node was leader before — fresh clock for the new leader.
        assert_eq!(
            resolve_became_leader_epoch(Some(node_y), Some(now - 1000), node_x, now),
            now
        );

        // No prior record at all — fresh clock.
        assert_eq!(resolve_became_leader_epoch(None, None, node_x, now), now);
    }

    /// set_became_leader_epoch: time_since_became_leader() reflects the carried-over
    /// start time, not just "now".
    #[tokio::test]
    async fn test_set_became_leader_epoch_carries_over() {
        let local_id = NodeId::new();
        let local_addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let manager = ClusterManager::new(local_id, local_addr, 10, 30);

        let now = dfs_common::types::current_timestamp();
        manager.set_became_leader_epoch(now - 1000, now).await;

        let elapsed = manager.time_since_became_leader().await.unwrap();
        assert!(elapsed.as_secs() >= 1000, "expected >=1000s elapsed, got {}s", elapsed.as_secs());
    }
}
