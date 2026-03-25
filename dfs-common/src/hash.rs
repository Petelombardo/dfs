use crate::types::{ChunkId, NodeId};
use blake3::Hasher;
use std::collections::BTreeMap;

/// Consistent hash ring for data placement
#[derive(Debug, Clone)]
pub struct ConsistentHashRing {
    /// Virtual nodes on the ring (hash -> NodeId)
    /// Using BTreeMap for sorted keys
    ring: BTreeMap<u64, NodeId>,

    /// Number of virtual nodes per physical node
    virtual_nodes: usize,

    /// Active nodes in the cluster
    nodes: Vec<NodeId>,
}

impl ConsistentHashRing {
    /// Create a new hash ring
    pub fn new(virtual_nodes: usize) -> Self {
        Self {
            ring: BTreeMap::new(),
            virtual_nodes,
            nodes: Vec::new(),
        }
    }

    /// Add a node to the ring
    pub fn add_node(&mut self, node_id: NodeId) {
        if !self.nodes.contains(&node_id) {
            self.nodes.push(node_id);
        }

        // Add virtual nodes
        for i in 0..self.virtual_nodes {
            let hash = self.hash_node(&node_id, i);
            self.ring.insert(hash, node_id);
        }
    }

    /// Remove a node from the ring
    pub fn remove_node(&mut self, node_id: &NodeId) {
        self.nodes.retain(|n| n != node_id);

        // Remove all virtual nodes
        for i in 0..self.virtual_nodes {
            let hash = self.hash_node(node_id, i);
            self.ring.remove(&hash);
        }
    }

    /// Get N nodes responsible for a chunk (for replication)
    pub fn get_nodes(&self, chunk_id: &ChunkId, n: usize) -> Vec<NodeId> {
        if self.ring.is_empty() {
            return Vec::new();
        }

        let hash = self.hash_chunk(chunk_id);
        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // Find first node >= hash
        let iter = self.ring.range(hash..).chain(self.ring.range(..));

        for (_, node_id) in iter {
            if !seen.contains(node_id) {
                seen.insert(*node_id);
                result.push(*node_id);

                if result.len() >= n {
                    break;
                }
            }

            // Prevent infinite loop if we don't have enough unique nodes
            if seen.len() >= self.nodes.len() {
                break;
            }
        }

        result
    }

    /// Get primary node for a chunk (first node in replication list)
    pub fn get_primary_node(&self, chunk_id: &ChunkId) -> Option<NodeId> {
        self.get_nodes(chunk_id, 1).into_iter().next()
    }

    /// Get all nodes in the ring
    pub fn nodes(&self) -> &[NodeId] {
        &self.nodes
    }

    /// Get number of nodes
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Hash a node with virtual node index
    fn hash_node(&self, node_id: &NodeId, virtual_index: usize) -> u64 {
        let mut hasher = Hasher::new();
        hasher.update(node_id.0.as_bytes());
        hasher.update(&virtual_index.to_le_bytes());
        let hash = hasher.finalize();
        u64::from_le_bytes(hash.as_bytes()[0..8].try_into().unwrap())
    }

    /// Hash a chunk to find its position on the ring
    fn hash_chunk(&self, chunk_id: &ChunkId) -> u64 {
        let bytes = &chunk_id.hash[0..8];
        u64::from_le_bytes(bytes.try_into().unwrap())
    }
}

/// Compute Blake3 hash of data
pub fn compute_chunk_hash(data: &[u8]) -> [u8; 32] {
    let hash = blake3::hash(data);
    *hash.as_bytes()
}

/// Verify chunk data against expected hash
pub fn verify_chunk_hash(data: &[u8], expected: &[u8; 32]) -> bool {
    let actual = compute_chunk_hash(data);
    &actual == expected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeId;

    #[test]
    fn test_hash_ring_basic() {
        let mut ring = ConsistentHashRing::new(100);

        let node1 = NodeId::new();
        let node2 = NodeId::new();
        let node3 = NodeId::new();

        ring.add_node(node1);
        ring.add_node(node2);
        ring.add_node(node3);

        assert_eq!(ring.node_count(), 3);
        assert_eq!(ring.ring.len(), 300); // 3 nodes * 100 virtual nodes
    }

    #[test]
    fn test_get_nodes() {
        let mut ring = ConsistentHashRing::new(100);

        let node1 = NodeId::new();
        let node2 = NodeId::new();
        let node3 = NodeId::new();

        ring.add_node(node1);
        ring.add_node(node2);
        ring.add_node(node3);

        let chunk_id = ChunkId::from_hash([0u8; 32]);
        let nodes = ring.get_nodes(&chunk_id, 3);

        assert_eq!(nodes.len(), 3);
        // All nodes should be unique
        assert_ne!(nodes[0], nodes[1]);
        assert_ne!(nodes[1], nodes[2]);
        assert_ne!(nodes[0], nodes[2]);
    }

    #[test]
    fn test_remove_node() {
        let mut ring = ConsistentHashRing::new(100);

        let node1 = NodeId::new();
        let node2 = NodeId::new();

        ring.add_node(node1);
        ring.add_node(node2);

        assert_eq!(ring.node_count(), 2);

        ring.remove_node(&node1);

        assert_eq!(ring.node_count(), 1);
        assert_eq!(ring.ring.len(), 100); // Only node2's virtual nodes
    }

    #[test]
    fn test_chunk_hash_verification() {
        let data = b"Hello, DFS!";
        let hash = compute_chunk_hash(data);

        assert!(verify_chunk_hash(data, &hash));
        assert!(!verify_chunk_hash(b"Different data", &hash));
    }
}
