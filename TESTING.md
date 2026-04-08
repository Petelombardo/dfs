# Testing Your DFS Cluster

## Quick Start: 5-Node Cluster

### Setup and Start
```bash
# Setup cluster (initializes N nodes)
./scripts/setup-cluster.sh 5

# Start all nodes
./scripts/start-cluster.sh 5

# Check cluster status
./target/release/dfs-admin --cluster 127.0.0.1:8900 cluster status

# Mount filesystem
mkdir -p /tmp/dfs-mount
./target/release/dfs-client mount /tmp/dfs-mount --cluster 127.0.0.1:8900

# Stop cluster
./scripts/stop-cluster.sh
```

### Test Operations
```bash
# Write a file
echo "Testing 5-node cluster!" > /tmp/dfs-mount/test.txt

# Read the file
cat /tmp/dfs-mount/test.txt

# Check where chunks are stored
for i in 1 2 3 4 5; do
    echo "Node $i chunks:"
    ls -lh /tmp/dfs-test/node${i}/data/
done

# Test failover
# Kill node 2
pkill -f "node2/config.toml"

# Wait for failure detection (30+ seconds)
sleep 35

# File should still be readable
cat /tmp/dfs-mount/test.txt

# Check cluster status
./target/release/dfs-admin --cluster 127.0.0.1:8900 cluster status
```

## Frequently Asked Questions

### Q: Do all nodes need the same amount of storage space?

**No**, nodes can have different storage capacities. HOWEVER, the current implementation has limitations:

**Current Behavior:**
- Data placement uses **consistent hashing** based on chunk IDs
- **Nodes are treated equally** regardless of available disk space
- There is **NO space-aware placement** or rebalancing
- A small node can fill up before larger nodes, causing write failures

**What This Means:**
- ✅ You CAN run nodes with different disk sizes
- ❌ The system will NOT automatically use larger nodes more
- ❌ Small nodes may fill up first and cause problems
- ❌ No automatic rebalancing based on available space

**For Production:**
You should either:
1. Use nodes with similar storage capacity, OR
2. Implement space-aware placement (see "Future Enhancements" below)

### Q: Will the system automatically maximize available storage while making replicas?

**No**, not automatically. Here's how replication currently works:

**Current Implementation:**
- Fixed replication factor (default: 3 copies)
- Chunk placement determined by consistent hashing
- Replicas stored on next N nodes in the hash ring
- **NO consideration of available disk space**

**Example with 5 nodes (100GB, 100GB, 50GB, 100GB, 100GB):**
- System treats all nodes equally
- Node 3 (50GB) gets same amount of data as others
- Node 3 will fill up first
- Writes may fail even though 400GB total space remains

### Q: How does data get distributed across nodes?

**Consistent Hashing:**
1. Each chunk gets a unique ID (SHA-256 hash of content)
2. Nodes are placed on a hash ring
3. Chunk is assigned to next N nodes clockwise on the ring
4. This ensures:
   - Even distribution (statistically)
   - Minimal data movement when nodes join/leave
   - Same chunk always goes to same nodes

**Example:**
```
Hash Ring: [Node1] -> [Node3] -> [Node5] -> [Node2] -> [Node4] -> [Node1] ...
Chunk ABC hashes to position between Node3 and Node5
Replicas go to: Node5, Node2, Node4 (next 3 nodes clockwise)
```

### Q: What happens when a node fills up?

**Current Behavior:**
- Write operations to that node will fail
- May cause total write failure if replication can't be satisfied
- No automatic rebalancing to other nodes
- No graceful degradation

**Mitigation:**
- Monitor disk usage on all nodes
- Ensure nodes have adequate capacity
- Plan for 2-3x expected data size for replication overhead

## Storage Distribution Testing

### Test 1: Verify Chunk Distribution
```bash
# Write several files
for i in {1..10}; do
    echo "Test file $i" > /tmp/dfs-mount/file${i}.txt
done

# Count chunks per node
for i in 1 2 3 4 5; do
    count=$(ls /tmp/dfs-test/node${i}/data/ 2>/dev/null | wc -l)
    echo "Node $i: $count chunks"
done
```

Expected: Chunks distributed across multiple nodes (with 3x replication factor)

### Test 2: Verify Replication
```bash
# Write a file
echo "Replicated test" > /tmp/dfs-mount/replica-test.txt

# Check replication
echo "Checking replication across nodes..."
for i in 1 2 3 4 5; do
    chunks=$(ls /tmp/dfs-test/node${i}/data/ 2>/dev/null | wc -l)
    echo "Node $i: $chunks chunks"
done
```

Expected: With replication factor 3, each chunk should appear on 3 nodes

### Test 3: Test Failover with Multiple Nodes
```bash
# Start with 5 nodes, kill 2
pkill -f "node2/config.toml"
pkill -f "node4/config.toml"

# Wait for failure detection
sleep 35

# Files should still be readable (with 3x replication)
cat /tmp/dfs-mount/replica-test.txt

# Check cluster
./target/release/dfs-admin --cluster 127.0.0.1:8900 cluster status
```

Expected: 3 healthy nodes, 2 failed, files still accessible

## Future Enhancements

### Space-Aware Placement
To properly handle different storage capacities, you would need:

1. **Storage Reporting:**
   - Nodes report available disk space in heartbeats
   - Cluster manager tracks capacity per node

2. **Weighted Consistent Hashing:**
   - Larger nodes get more virtual nodes on hash ring
   - Proportionally distributes load based on capacity
   - Example: 100GB node gets 10 vnodes, 200GB gets 20 vnodes

3. **Placement Policies:**
   - Check available space before placing chunks
   - Skip nodes that are >90% full
   - Rebalance when nodes become unbalanced

4. **Rebalancing:**
   - Background process to move chunks from full nodes
   - Respect replication constraints during moves
   - Minimize data movement

### Example Space-Aware Implementation:
```rust
// In server.rs
async fn select_nodes_for_chunk(&self, chunk_id: &ChunkId) -> Vec<NodeId> {
    let nodes = self.cluster.get_all_nodes().await;

    // Filter out nodes that are too full
    let available_nodes: Vec<_> = nodes.into_iter()
        .filter(|n| n.disk_usage_pct() < 90.0)
        .collect();

    // Use weighted consistent hashing based on available space
    self.cluster.get_nodes_weighted(chunk_id, self.replication_factor, &available_nodes).await
}
```

## Architecture Notes

### Current Limitations:
1. ❌ No space-aware placement
2. ❌ No automatic rebalancing
3. ❌ No quotas or limits per node
4. ❌ No degraded mode (write with reduced replication)
5. ❌ No chunk migration tools

### What Works Well:
1. ✅ Automatic cluster formation
2. ✅ Failure detection and recovery
3. ✅ Consistent hashing for even distribution
4. ✅ 3x replication for fault tolerance
5. ✅ Automatic replica healing
6. ✅ FUSE filesystem interface

## Node Configuration

### Ports:
- Node 1: 8900 (seed node)
- Node 2: 8901
- Node 3: 8902
- Node 4: 8903
- Node 5: 8904

### Data Locations:
- Data: `/tmp/dfs-test/node{N}/data/`
- Metadata: `/tmp/dfs-test/node{N}/metadata/`
- Config: `/tmp/dfs-test/node{N}/config.toml`

### Configuration Parameters:
```toml
[storage]
chunk_size_mb = 4                    # Size of data chunks

[cluster]
heartbeat_interval_secs = 10         # How often to send heartbeats
failure_timeout_secs = 30            # When to mark node as failed

[replication]
replication_factor = 3               # Number of copies per chunk
healing_delay_secs = 300             # Wait before healing missing replicas
auto_heal = true                     # Automatically heal missing replicas
scrub_interval_hours = 24            # How often to verify checksums
```

## Monitoring

### Check Cluster Health:
```bash
./target/release/dfs-admin --cluster 127.0.0.1:8900 cluster status
```

### Check Storage Usage:
```bash
for i in 1 2 3 4 5; do
    echo "=== Node $i ==="
    du -sh /tmp/dfs-test/node${i}/data
    ls /tmp/dfs-test/node${i}/data | wc -l | xargs echo "Chunks:"
done
```

### Check Node Logs:
```bash
# Watch node 1 logs
tail -f /tmp/dfs-test/node1/server.log  # (if using start-cluster.sh with logging)

# Or check journal if running as systemd service
journalctl -u dfs-server@node1 -f
```

## Summary

- ✅ **Easy cluster setup** with provided scripts
- ✅ **Automatic cluster formation** via seed nodes
- ✅ **Fault tolerance** with 3x replication
- ⚠️ **No automatic space balancing** - use similar-sized nodes
- ⚠️ **No space-aware placement** - system treats all nodes equally
- 💡 **For heterogeneous storage**, implement weighted consistent hashing
