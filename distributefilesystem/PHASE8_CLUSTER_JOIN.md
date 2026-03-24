# Phase 8: Cluster Join Protocol Implementation

## Current Status

✅ **Completed:**
- Phase 7: Admin Tools (fully working)
- Critical bug fixes:
  - MessageEnvelope protocol (client/server communication)
  - FUSE runtime nested block issue
- Single-node deployment fully functional
- All file operations working through FUSE
- Admin CLI working perfectly

⚠️ **Current Limitation:**
- Multi-node clustering not automatic
- Nodes 2 and 3 start but don't join Node 1's cluster
- Cannot test replication or failover

## Goal: Implement Cluster Join Protocol

Enable automatic node discovery and cluster membership so multiple nodes can form a functional distributed cluster.

## What Needs Implementation

### 1. Node Registration Protocol

**Location:** `dfs-server/src/cluster.rs` and `dfs-server/src/network.rs`

Currently exists:
- `ClusterManager::add_node()` - adds node to cluster
- `ClusterManager::remove_node()` - removes node
- `ClusterManager::update_heartbeat()` - updates node heartbeat
- Heartbeat tracking and failure detection

Missing:
- Join request/response protocol
- Node discovery mechanism
- Cluster state synchronization

### 2. Protocol Messages

**Location:** `dfs-common/src/protocol.rs`

Need to add:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClusterMessage {
    // Existing
    Heartbeat { node_info: NodeInfo },

    // NEW - Need to add
    JoinRequest {
        node_info: NodeInfo,
    },
    JoinResponse {
        accepted: bool,
        cluster_nodes: Vec<NodeInfo>,
    },
    NodeJoined {
        node_info: NodeInfo,
    },
    NodeLeft {
        node_id: NodeId,
    },
}
```

### 3. Server Startup Join Logic

**Location:** `dfs-server/src/main.rs` in `start_server()`

Currently (line ~170):
```rust
// Start healing manager
let healing = std::sync::Arc::new(healing::HealingManager::new(...));
healing.clone().start().await;

// Start network server
let mut net_server = network::NetworkServer::new(config.node.listen_addr, server.clone());
```

Need to add after network server starts:
```rust
// Join cluster if seed nodes configured
if !config.cluster.seed_nodes.is_empty() {
    join_cluster(&server, &config.cluster.seed_nodes).await?;
}
```

### 4. Join Cluster Function

**Location:** New function in `dfs-server/src/main.rs`

```rust
async fn join_cluster(
    server: &Arc<Server>,
    seed_nodes: &[SocketAddr],
) -> Result<()> {
    info!("Attempting to join cluster via seed nodes: {:?}", seed_nodes);

    for seed_addr in seed_nodes {
        match send_join_request(seed_addr, server).await {
            Ok(_) => {
                info!("Successfully joined cluster via {}", seed_addr);
                return Ok(());
            }
            Err(e) => {
                warn!("Failed to join via {}: {}", seed_addr, e);
                continue;
            }
        }
    }

    anyhow::bail!("Failed to join cluster - all seed nodes unreachable")
}

async fn send_join_request(
    seed_addr: &SocketAddr,
    server: &Arc<Server>,
) -> Result<()> {
    let local_node_id = server.cluster().local_node_id();
    let local_addr = server.cluster().local_addr();

    let node_info = NodeInfo::new(local_node_id, local_addr, None);

    // Send join request
    let request = ClusterMessage::JoinRequest { node_info };
    let response = send_cluster_message(*seed_addr, request).await?;

    match response {
        ClusterMessage::JoinResponse { accepted, cluster_nodes } => {
            if !accepted {
                anyhow::bail!("Join request rejected");
            }

            // Add all cluster nodes
            for node in cluster_nodes {
                server.cluster().add_node(node).await?;
            }

            Ok(())
        }
        _ => anyhow::bail!("Unexpected response to join request"),
    }
}
```

### 5. Server Handler for Join Requests

**Location:** `dfs-server/src/server.rs` in `handle_cluster_message()`

Currently handles:
- `Heartbeat`

Need to add:
```rust
ClusterMessage::JoinRequest { node_info } => {
    info!("Received join request from node {}", node_info.id);

    // Add node to cluster
    if let Err(e) = self.cluster.add_node(node_info.clone()).await {
        warn!("Failed to add node: {}", e);
        return Response::Error {
            message: format!("Failed to add node: {}", e),
            code: ErrorCode::InternalError,
        };
    }

    // Get all cluster nodes
    let cluster_nodes = self.cluster.get_all_nodes().await;

    // Broadcast to other nodes that new node joined
    broadcast_node_joined(&node_info, &cluster_nodes, &self.cluster).await;

    // Return success with cluster state
    let response = ClusterMessage::JoinResponse {
        accepted: true,
        cluster_nodes,
    };

    Response::Ok { data: Some(bincode::serialize(&response).unwrap()) }
}

ClusterMessage::NodeJoined { node_info } => {
    info!("Node {} joined the cluster", node_info.id);
    self.cluster.add_node(node_info).await.ok();
    Response::Ok { data: None }
}
```

### 6. Background Heartbeat Sender

**Location:** New task in `dfs-server/src/cluster.rs`

```rust
impl ClusterManager {
    pub async fn start_heartbeat_sender(self: Arc<Self>) {
        let mut interval = tokio::time::interval(Duration::from_secs(self.heartbeat_interval));

        tokio::spawn(async move {
            loop {
                interval.tick().await;

                let nodes = self.nodes.read().await.clone();
                let local_id = self.local_node_id;

                for (node_id, node_info) in nodes {
                    if node_id == local_id {
                        continue; // Don't send to self
                    }

                    // Send heartbeat
                    let heartbeat = ClusterMessage::Heartbeat {
                        node_info: NodeInfo::new(
                            local_id,
                            self.local_addr,
                            None,
                        ),
                    };

                    // Send async (don't wait)
                    tokio::spawn(async move {
                        if let Err(e) = send_cluster_message(node_info.addr, heartbeat).await {
                            debug!("Failed to send heartbeat to {}: {}", node_id, e);
                        }
                    });
                }
            }
        });
    }
}
```

## Implementation Steps

1. ✅ **Review current code** - Already done, see locations above
2. **Add protocol messages** - Update ClusterMessage enum
3. **Implement join request handler** - Server-side
4. **Implement join client** - Client-side startup logic
5. **Add heartbeat sender** - Background task
6. **Add node joined broadcast** - Notify all nodes of new member
7. **Test multi-node cluster** - Run 3-node test
8. **Test replication** - Verify chunks replicate
9. **Test failover** - Kill a node, verify system continues

## Test Plan

### Test 1: 3-Node Cluster Formation
```bash
# Start node 1 (seed)
./target/release/dfs-server start --config /tmp/dfs-test/node1/config.toml

# Start node 2 (should auto-join)
./target/release/dfs-server start --config /tmp/dfs-test/node2/config.toml

# Start node 3 (should auto-join)
./target/release/dfs-server start --config /tmp/dfs-test/node3/config.toml

# Verify cluster
./target/release/dfs-admin --cluster 127.0.0.1:8900 cluster status
# Should show 3 nodes
```

### Test 2: Replication
```bash
# Mount filesystem
./target/release/dfs-client mount /tmp/dfs-mount --cluster 127.0.0.1:8900

# Create file
echo "replicated data" > /tmp/dfs-mount/test.txt

# Check file info
./target/release/dfs-admin --cluster 127.0.0.1:8900 file info /test.txt
# Should show chunks on 3 different nodes
```

### Test 3: Failover
```bash
# Create file with 3-node cluster
echo "failover test" > /tmp/dfs-mount/failover.txt

# Kill node 2
pkill -f "node2/config.toml"

# Verify file still readable
cat /tmp/dfs-mount/failover.txt
# Should still work (read from node 1 or 3)

# Check cluster status
./target/release/dfs-admin --cluster 127.0.0.1:8900 cluster status
# Should show 2 healthy nodes, 1 failed
```

## Files to Modify

1. `dfs-common/src/protocol.rs` - Add JoinRequest/Response messages
2. `dfs-server/src/server.rs` - Add join request handler
3. `dfs-server/src/cluster.rs` - Add heartbeat sender, helper methods
4. `dfs-server/src/main.rs` - Add join_cluster() function and startup logic
5. `dfs-server/src/network.rs` - May need helper for sending cluster messages

## Estimated Complexity

- **Protocol messages:** Easy (5 minutes)
- **Join request handler:** Medium (15 minutes)
- **Join client logic:** Medium (15 minutes)
- **Heartbeat sender:** Easy (10 minutes)
- **Testing & debugging:** 30-60 minutes

**Total:** ~1.5-2 hours

## Success Criteria

✅ Three nodes form a cluster automatically
✅ All nodes aware of each other
✅ Heartbeats flowing between nodes
✅ Files replicate across multiple nodes
✅ System survives single node failure

## Current File State

All files are committed:
- Latest commit: e792899 "Add comprehensive test report"
- No uncommitted changes
- Ready to start implementation

## Next Steps

1. Clear context (start fresh)
2. Begin with protocol messages in dfs-common/src/protocol.rs
3. Work through implementation steps above
4. Test with 3-node cluster
5. Document results

---

**Ready to implement!** 🚀
