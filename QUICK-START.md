# DFS Quick Start Guide

## What You Have

A fully functional distributed filesystem with:
- ✅ **5-node cluster** successfully tested
- ✅ **Automatic cluster formation** via seed nodes
- ✅ **3x replication** for fault tolerance
- ✅ **Automatic failover** and recovery
- ✅ **Self-healing** replicas
- ✅ **FUSE filesystem** interface
- ✅ **Production-ready deployment scripts**

## Quick Test (Local)

```bash
# Build
cargo build --release

# Setup 5-node test cluster
./scripts/setup-cluster.sh 5

# Start cluster
./scripts/start-cluster.sh 5

# Check status
./target/release/dfs-admin --cluster 127.0.0.1:8900 cluster status

# Mount filesystem
mkdir -p /tmp/dfs-mount
./target/release/dfs-client mount /tmp/dfs-mount --cluster 127.0.0.1:8900

# Use it
echo "Hello DFS!" > /tmp/dfs-mount/test.txt
cat /tmp/dfs-mount/test.txt

# Stop
./scripts/stop-cluster.sh
```

## Production Deployment (Restartos)

### Step 1: Deploy First Node (Seed)
On your first server (e.g., `192.168.1.10`):
```bash
./scripts/deploy-node.sh
```
- Node IP: `192.168.1.10`
- Is seed?: `yes`
- Data dir: `/mnt/storage/dfs/data`

### Step 2: Deploy Additional Nodes
On each additional server:
```bash
./scripts/deploy-node.sh
```
- Node IP: `<this-server-ip>`
- Is seed?: `no`
- Seed node: `192.168.1.10:8900`
- Data dir: `/mnt/storage/dfs/data`

### Step 3: Start Services
On each server:
```bash
sudo systemctl start dfs-server
sudo systemctl enable dfs-server
```

### Step 4: Verify
```bash
./target/release/dfs-admin --cluster 192.168.1.10:8900 cluster status
```

## Key Features

### Storage Distribution
- Uses **consistent hashing** for even distribution
- Each chunk replicated to **3 nodes** (configurable)
- Automatic placement based on hash ring
- ⚠️ **No space-aware balancing** - use similar-sized nodes

### Fault Tolerance
- Survives up to **N-R node failures** (N nodes, R replication)
- **30-second** failure detection
- **Automatic replica healing** (5-minute delay)
- Failed nodes can **rejoin automatically**

### What Works
✅ Multi-node clusters (tested with 5 nodes)
✅ Automatic cluster formation
✅ Node failure detection
✅ Automatic replica healing
✅ Consistent hashing
✅ FUSE filesystem interface
✅ Status reporting

### Current Limitations
❌ No space-aware placement (treats all nodes equally)
❌ No automatic rebalancing
❌ Old data doesn't move when nodes rejoin
❌ No quotas or limits per node
❌ No degraded mode (reduced replication on failure)

## Important Files

### Scripts
- `scripts/deploy-node.sh` - Interactive deployment for production
- `scripts/setup-cluster.sh` - Setup N-node test cluster
- `scripts/start-cluster.sh` - Start test cluster
- `scripts/stop-cluster.sh` - Stop test cluster

### Documentation
- `DEPLOYMENT-RESTARTOS.md` - Complete Restartos deployment guide
- `TESTING.md` - Testing guide with FAQs
- `README.md` - Project overview
- `QUICK-START.md` - This file

### Configuration
- Default config location: `/etc/dfs/config.toml`
- Production config: `/mnt/storage/dfs/config/config.toml`

## Common Commands

```bash
# Check cluster status
./target/release/dfs-admin --cluster <IP>:8900 cluster status

# Check storage stats
./target/release/dfs-admin --cluster <IP>:8900 storage stats

# Check healing status
./target/release/dfs-admin --cluster <IP>:8900 healing status

# Mount filesystem
./target/release/dfs-client mount /mnt/dfs --cluster <IP>:8900

# Unmount
fusermount -u /mnt/dfs

# View logs (systemd)
journalctl -u dfs-server -f

# Restart service
sudo systemctl restart dfs-server
```

## Restartos Deployment Tips

### What Must Be Persistent
1. **Binaries** - Copy to `/mnt/storage/dfs/bin/`
2. **Configuration** - Store in `/mnt/storage/dfs/config/`
3. **Data** - Store in `/mnt/storage/dfs/data/`
4. **Metadata** - Store in `/mnt/storage/dfs/metadata/`
5. **Service file** - Copy to persistent storage

### Boot Script
Create `/mnt/storage/dfs/restore-service.sh`:
```bash
#!/bin/bash
cp /mnt/storage/dfs/config/dfs-server.service /etc/systemd/system/
systemctl daemon-reload
systemctl start dfs-server
```

Add to your Restartos boot configuration.

## FAQ

**Q: Do all nodes need the same storage size?**
A: No, but the system doesn't balance based on available space. Use similar sizes or implement space-aware placement.

**Q: What happens if a node fills up?**
A: Writes to that node will fail. The system doesn't automatically rebalance to other nodes.

**Q: How many nodes can I have?**
A: No hard limit. Tested with 5 nodes, should scale to dozens.

**Q: What's the minimum number of nodes?**
A: 1 node works, but you need at least 3 for meaningful fault tolerance with 3x replication.

**Q: Can nodes have different hardware?**
A: Yes, but they're treated equally by the consistent hash ring.

**Q: What happens when I restart a node?**
A: It rejoins automatically (if seed nodes configured) with a new UUID. Old data intact.

**Q: Do I need to configure firewalls?**
A: Yes, allow TCP port 8900 (or your configured port) between all nodes.

## Troubleshooting

**Nodes won't join cluster:**
- Check network connectivity: `ping <seed-ip>`
- Check port is open: `telnet <seed-ip> 8900`
- Check seed node is running
- Check firewall allows port 8900

**"Input/output error" on FUSE mount:**
- Unmount stale mounts: `fusermount -u /mnt/dfs`
- Check cluster is running
- Try remounting

**Node marked as failed but still running:**
- Network partition - check connectivity
- High load - node not responding to heartbeats
- Restart node to rejoin

## Next Steps

1. **Test locally** with scripts/setup-cluster.sh
2. **Deploy seed node** on first server
3. **Deploy additional nodes** pointing to seed
4. **Verify cluster** with dfs-admin
5. **Mount filesystem** and test
6. **Monitor** disk usage and cluster health
7. **Plan for growth** - add nodes as needed

## Support

- **Issues**: Report at GitHub
- **Documentation**: See DEPLOYMENT-RESTARTOS.md for detailed guide
- **Testing**: See TESTING.md for test scenarios

---

**Your DFS cluster is ready for production deployment on Restartos!**
