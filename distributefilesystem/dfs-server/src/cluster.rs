use anyhow::Result;
use dfs_common::{ConsistentHashRing, NodeId, NodeInfo, NodeStatus};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

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

    /// Heartbeat interval in seconds
    heartbeat_interval: u64,

    /// Node failure timeout in seconds
    failure_timeout: u64,
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
            heartbeat_interval,
            failure_timeout,
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

        if nodes.contains_key(&node_info.id) {
            debug!("Node {} already exists, updating info", node_info.id);
        } else {
            info!("Adding new node {} to cluster", node_info.id);
            ring.add_node(node_info.id);
        }

        nodes.insert(node_info.id, node_info);

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

    /// Start background task to check for failed nodes
    pub async fn start_failure_detector(self: Arc<Self>) {
        let mut check_interval = interval(Duration::from_secs(self.heartbeat_interval));

        tokio::spawn(async move {
            loop {
                check_interval.tick().await;

                if let Err(e) = self.check_failed_nodes().await {
                    warn!("Error checking for failed nodes: {}", e);
                }
            }
        });
    }

    /// Check for nodes that have failed (no heartbeat within timeout)
    async fn check_failed_nodes(&self) -> Result<()> {
        let mut nodes = self.nodes.write().await;
        let mut failed_nodes = Vec::new();

        for (node_id, node_info) in nodes.iter_mut() {
            // Skip local node
            if node_id == &self.local_node_id {
                continue;
            }

            if node_info.is_failed(self.failure_timeout) {
                if node_info.status != NodeStatus::Failed {
                    warn!("Node {} has failed (no heartbeat)", node_id);
                    node_info.status = NodeStatus::Failed;
                    failed_nodes.push(*node_id);
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
}

/// Cluster statistics
#[derive(Debug, Clone)]
pub struct ClusterStats {
    pub total_nodes: usize,
    pub online_nodes: usize,
    pub failed_nodes: usize,
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
