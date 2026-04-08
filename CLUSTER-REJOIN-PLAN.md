# Cluster Rejoin Implementation Plan
**Goal:** Enable any node (including seed) to reboot and have the cluster self-heal

## Current State
- Nodes have `seed_nodes` in config
- On startup, nodes try to join via seed nodes only
- If seed node is down, cluster fragments
- No persistent peer discovery

## Problem Scenarios
1. **Seed node reboots** → Other nodes can't find cluster
2. **Non-seed node reboots** → Needs to rejoin, but only knows seed
3. **All nodes reboot** → Cluster doesn't reform

## Solution: Persistent Peer List

### Phase 1: Persist Discovered Peers ✓ (Implement First)
**Goal:** Nodes remember all peers they've ever seen

**Implementation:**
1. Add `peers.json` file in metadata directory
2. When node joins cluster and receives `ClusterInfo`, save all peers
3. When node receives `NodeJoined` broadcast, add to peers list
4. On startup, load peers from disk

**Files to modify:**
- `dfs-server/src/cluster.rs` - Add peer persistence methods
- `dfs-server/src/main.rs` - Load peers on startup, try all known peers

**New structure:**
```rust
// In cluster.rs
struct PersistedPeers {
    peers: Vec<SocketAddr>,
    last_updated: SystemTime,
}

impl ClusterManager {
    pub async fn load_persisted_peers(&self, path: &Path) -> Result<Vec<SocketAddr>>;
    pub async fn save_peer(&self, addr: SocketAddr, path: &Path) -> Result<()>;
    pub async fn handle_node_joined(&self, node_info: NodeInfo, persist_path: &Path);
}
```

**Startup logic change:**
```rust
// main.rs
let mut all_seed_nodes = config.cluster.seed_nodes.clone();

// Load persisted peers
if let Ok(persisted) = load_persisted_peers(&config.storage.metadata_dir).await {
    info!("Loaded {} persisted peers", persisted.len());
    all_seed_nodes.extend(persisted);
}

// Try ALL known peers (config + persisted)
if !all_seed_nodes.is_empty() {
    join_cluster(server.clone(), &all_seed_nodes).await?;
}
```

### Phase 2: Gossip Protocol (Future Enhancement)
**Goal:** Nodes actively share peer information

- Nodes periodically broadcast their peer list
- Nodes merge received peer lists with their own
- Enables discovery of nodes that joined while this node was down

### Phase 3: Background Rejoin Attempts (Future Enhancement)
**Goal:** Retry joining if initial attempt fails

- If join fails on startup, retry every 30 seconds
- Allows node to join when seed comes back online
- Stops retrying once successfully joined

## Implementation Steps

### Step 1: Add Peer Persistence Structure
```rust
// Add to dfs-server/Cargo.toml if needed
serde_json = "1.0"

// Add to dfs-server/src/cluster.rs
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedPeers {
    peers: Vec<SocketAddr>,
    last_updated: u64, // Unix timestamp
}
```

### Step 2: Implement Load/Save Methods
```rust
impl ClusterManager {
    pub async fn load_persisted_peers(path: &Path) -> Result<Vec<SocketAddr>> {
        let peers_file = path.join("peers.json");
        if !peers_file.exists() {
            return Ok(Vec::new());
        }

        let data = tokio::fs::read_to_string(&peers_file).await?;
        let persisted: PersistedPeers = serde_json::from_str(&data)?;

        info!("Loaded {} persisted peers", persisted.peers.len());
        Ok(persisted.peers)
    }

    pub async fn save_persisted_peers(&self, peers: &[SocketAddr], path: &Path) -> Result<()> {
        let persisted = PersistedPeers {
            peers: peers.to_vec(),
            last_updated: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };

        let peers_file = path.join("peers.json");
        let data = serde_json::to_string_pretty(&persisted)?;
        tokio::fs::write(&peers_file, data).await?;

        debug!("Saved {} peers to {}", peers.len(), peers_file.display());
        Ok(())
    }
}
```

### Step 3: Update on Cluster Info Received
```rust
// In main.rs, after successful join:
match join_cluster(server.clone(), &all_seed_nodes).await {
    Ok(cluster_nodes) => {
        info!("✓ Successfully joined cluster");

        // Extract peer addresses
        let peer_addrs: Vec<SocketAddr> = cluster_nodes
            .iter()
            .map(|n| n.address)
            .collect();

        // Save to disk
        let metadata_dir = PathBuf::from(&config.storage.metadata_dir);
        if let Err(e) = ClusterManager::save_persisted_peers(&peer_addrs, &metadata_dir).await {
            warn!("Failed to save persisted peers: {}", e);
        }
    }
    Err(e) => warn!("Failed to join cluster: {}", e),
}
```

### Step 4: Handle NodeJoined Broadcasts
```rust
// In server.rs, when handling ClusterMessage::NodeJoined:
ClusterMessage::NodeJoined { node_info } => {
    info!("Node joined: {}", node_info.id);

    // Add to cluster
    self.cluster_manager.add_node(node_info.clone()).await?;

    // Save updated peer list
    let nodes = self.cluster_manager.get_all_nodes().await;
    let peer_addrs: Vec<SocketAddr> = nodes.iter().map(|n| n.address).collect();

    let metadata_dir = PathBuf::from(&self.config.storage.metadata_dir);
    ClusterManager::save_persisted_peers(&peer_addrs, &metadata_dir).await?;
}
```

## Testing Plan

### Test 1: Seed Node Reboot
1. Start all 3 nodes (gluster1, 2, 3)
2. Verify cluster is healthy
3. `systemctl restart dfs-server` on gluster1 (seed)
4. Wait 30 seconds
5. Verify:
   - gluster2 and gluster3 still see each other
   - gluster1 rejoins and is discovered

### Test 2: Non-Seed Reboot
1. Start all 3 nodes
2. `systemctl restart dfs-server` on gluster2
3. Verify gluster2 rejoins via persisted peers

### Test 3: All Nodes Reboot (Rolling)
1. `systemctl restart dfs-server` on gluster1
2. Wait for it to start
3. `systemctl restart dfs-server` on gluster2
4. Wait for it to start (should find gluster1)
5. `systemctl restart dfs-server` on gluster3
6. Verify all 3 rejoin

### Test 4: Simultaneous Reboot
1. `systemctl restart dfs-server` on all nodes at once
2. First node up becomes effective seed
3. Others find it via persisted peers
4. Cluster reforms

## Success Criteria
- ✅ Any single node can reboot and rejoin
- ✅ Seed node can reboot without breaking cluster
- ✅ peers.json is created and updated
- ✅ Logs show "Loaded X persisted peers" on startup
- ✅ Cluster reforms after rolling reboots

## Files to Create/Modify
- `dfs-server/src/cluster.rs` - Add persistence methods
- `dfs-server/src/main.rs` - Load peers, save after join
- `dfs-server/src/server.rs` - Handle NodeJoined, save peers
- `CLUSTER-REJOIN-PLAN.md` - This file
