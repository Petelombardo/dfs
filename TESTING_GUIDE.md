# DFS Testing Guide - Single Host Testing

## Quick Answer: YES, you can test on a single host!

Run multiple server instances on different ports (8900, 8901, 8902, etc.)

---

## Single Host Test Setup (3 Nodes)

### Step 1: Create Test Directories

```bash
mkdir -p /tmp/dfs-test/{node1,node2,node3}/{data,metadata}
```

### Step 2: Create Config Files

**Node 1 Config** (`/tmp/dfs-test/node1/config.toml`):
```toml
[node]
listen_addr = "127.0.0.1:8900"

[storage]
data_dir = "/tmp/dfs-test/node1/data"
metadata_dir = "/tmp/dfs-test/node1/metadata"
chunk_size_mb = 4

[cluster]
seed_nodes = []  # First node, no seeds

[replication]
replication_factor = 3
healing_delay_secs = 300
auto_heal = true
scrub_interval_hours = 24
```

**Node 2 Config** (`/tmp/dfs-test/node2/config.toml`):
```toml
[node]
listen_addr = "127.0.0.1:8901"

[storage]
data_dir = "/tmp/dfs-test/node2/data"
metadata_dir = "/tmp/dfs-test/node2/metadata"
chunk_size_mb = 4

[cluster]
seed_nodes = ["127.0.0.1:8900"]  # Connect to node1

[replication]
replication_factor = 3
healing_delay_secs = 300
auto_heal = true
scrub_interval_hours = 24
```

**Node 3 Config** (`/tmp/dfs-test/node3/config.toml`):
```toml
[node]
listen_addr = "127.0.0.1:8902"

[storage]
data_dir = "/tmp/dfs-test/node3/data"
metadata_dir = "/tmp/dfs-test/node3/metadata"
chunk_size_mb = 4

[cluster]
seed_nodes = ["127.0.0.1:8900"]  # Connect to node1

[replication]
replication_factor = 3
healing_delay_secs = 300
auto_heal = true
scrub_interval_hours = 24
```

### Step 3: Initialize Nodes

```bash
# Node 1
./target/release/dfs-server init \
  --data-dir /tmp/dfs-test/node1/data \
  --meta-dir /tmp/dfs-test/node1/metadata \
  --config /tmp/dfs-test/node1/config.toml

# Node 2
./target/release/dfs-server init \
  --data-dir /tmp/dfs-test/node2/data \
  --meta-dir /tmp/dfs-test/node2/metadata \
  --config /tmp/dfs-test/node2/config.toml

# Node 3
./target/release/dfs-server init \
  --data-dir /tmp/dfs-test/node3/data \
  --meta-dir /tmp/dfs-test/node3/metadata \
  --config /tmp/dfs-test/node3/config.toml
```

### Step 4: Start Servers (3 separate terminals)

**Terminal 1 - Node 1:**
```bash
./target/release/dfs-server start --config /tmp/dfs-test/node1/config.toml
```

**Terminal 2 - Node 2:**
```bash
./target/release/dfs-server start --config /tmp/dfs-test/node2/config.toml
```

**Terminal 3 - Node 3:**
```bash
./target/release/dfs-server start --config /tmp/dfs-test/node3/config.toml
```

### Step 5: Test with Admin CLI

**Terminal 4 - Admin Commands:**
```bash
# Check cluster status
./target/release/dfs-admin --cluster 127.0.0.1:8900 cluster status

# Check storage stats
./target/release/dfs-admin --cluster 127.0.0.1:8900 storage stats

# Check healing status
./target/release/dfs-admin --cluster 127.0.0.1:8900 healing status
```

### Step 6: Mount and Use Filesystem

**Terminal 5 - Mount:**
```bash
mkdir -p /tmp/dfs-mount

./target/release/dfs-client mount /tmp/dfs-mount \
  --cluster 127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902 \
  --foreground
```

**Terminal 6 - Use the filesystem:**
```bash
# Create a file
echo "Hello DFS!" > /tmp/dfs-mount/test.txt

# Read it back
cat /tmp/dfs-mount/test.txt

# Create directory
mkdir /tmp/dfs-mount/mydir

# Copy files
cp /etc/hosts /tmp/dfs-mount/mydir/

# List files
ls -la /tmp/dfs-mount/mydir/

# Check file info with admin tool
./target/release/dfs-admin --cluster 127.0.0.1:8900 \
  file info /mydir/hosts

# Create a larger file (multi-chunk)
dd if=/dev/urandom of=/tmp/dfs-mount/bigfile bs=1M count=10

# Verify it
ls -lh /tmp/dfs-mount/bigfile
```

---

## Testing Scenarios

### Test 1: Basic Operations
```bash
# Write
echo "test data" > /tmp/dfs-mount/file1.txt

# Read
cat /tmp/dfs-mount/file1.txt

# Modify
echo "more data" >> /tmp/dfs-mount/file1.txt

# Delete
rm /tmp/dfs-mount/file1.txt
```

### Test 2: Multi-Chunk Files
```bash
# Create 20MB file (5 chunks @ 4MB each)
dd if=/dev/zero of=/tmp/dfs-mount/large.dat bs=1M count=20

# Check chunks
./target/release/dfs-admin --cluster 127.0.0.1:8900 \
  file info /large.dat
```

### Test 3: Replication Verification
```bash
# Create a file
echo "replicated data" > /tmp/dfs-mount/replicated.txt

# Check where it's stored
./target/release/dfs-admin --cluster 127.0.0.1:8900 \
  file info /replicated.txt

# Should see chunks on multiple nodes!
```

### Test 4: Node Failure (Failover)
```bash
# Create file
echo "test failover" > /tmp/dfs-mount/failover.txt

# Kill node 2 (Ctrl+C in terminal 2)

# Try to read (should still work!)
cat /tmp/dfs-mount/failover.txt

# Check cluster status
./target/release/dfs-admin --cluster 127.0.0.1:8900 cluster status

# Restart node 2
./target/release/dfs-server start --config /tmp/dfs-test/node2/config.toml
```

### Test 5: Self-Healing
```bash
# Create file
echo "healing test" > /tmp/dfs-mount/heal.txt

# Check replication
./target/release/dfs-admin --cluster 127.0.0.1:8900 \
  file info /heal.txt

# Kill a node, wait 300+ seconds, healing should kick in
# (Check healing status during this time)
./target/release/dfs-admin --cluster 127.0.0.1:8900 healing status
```

---

## Cleanup

```bash
# Unmount
fusermount -u /tmp/dfs-mount

# Stop all servers (Ctrl+C in each terminal)

# Clean up
rm -rf /tmp/dfs-test /tmp/dfs-mount
```

---

## Expected Output Examples

### Cluster Status:
```
DFS Cluster Status
==================
Total Nodes:   3
Healthy Nodes: 3

Nodes:
ID                                       Address
------------------------------------------------------------
a1b2c3d4-e5f6-7890-abcd-ef1234567890    127.0.0.1:8900
b2c3d4e5-f678-90ab-cdef-123456789012    127.0.0.1:8901
c3d4e5f6-7890-abcd-ef12-3456789012ab    127.0.0.1:8902
```

### Storage Stats:
```
DFS Storage Statistics
======================
Total Chunks:       5
Total Size:         20 MB
Replication Factor: 3
Nodes Count:        3
```

### File Info:
```
File Information: /large.dat
==================
Path:       /large.dat
Size:       20971520 bytes
Chunks:     5
Created:    1732456789
Modified:   1732456789
Mode:       644
UID:        1000
GID:        1000
Type:       RegularFile

Chunk Locations:
Chunk ID             Size       Nodes
----------------------------------------------------------------------
a1b2c3d4e5f67890    4194304    a1b2c3d4, b2c3d4e5, c3d4e5f6
b2c3d4e5f6789012    4194304    b2c3d4e5, c3d4e5f6, a1b2c3d4
...
```

---

## Troubleshooting

### Issue: "Failed to connect"
- Check all servers are running: `ps aux | grep dfs-server`
- Check ports are listening: `netstat -tlnp | grep "8900\|8901\|8902"`

### Issue: "No nodes available"
- Check cluster status: `dfs-admin cluster status`
- Verify seed_nodes in configs point to node 1

### Issue: "Permission denied" on mount
- Try with sudo: `sudo ./target/release/dfs-client mount ...`
- Or use a directory you own

### Issue: "Chunk not found"
- Wait for replication to complete (it's async)
- Check healing status: `dfs-admin healing status`

---

## Multi-Host Testing (Optional)

If you want to test on multiple machines:

1. Use actual IPs instead of 127.0.0.1
2. Ensure ports 8900-8902 are open in firewall
3. Copy binaries to each host
4. Update configs with real IPs

**Node 1 (192.168.1.10):**
```toml
listen_addr = "192.168.1.10:8900"
seed_nodes = []
```

**Node 2 (192.168.1.11):**
```toml
listen_addr = "192.168.1.11:8900"
seed_nodes = ["192.168.1.10:8900"]
```

**Node 3 (192.168.1.12):**
```toml
listen_addr = "192.168.1.12:8900"
seed_nodes = ["192.168.1.10:8900"]
```

---

## Summary

**YES - Single host testing works perfectly!**

- Run 3 servers on ports 8900, 8901, 8902
- All on 127.0.0.1 (localhost)
- Full cluster functionality
- Complete replication and healing
- Perfect for development and testing

**No need for multiple machines to test the system!** 🚀
