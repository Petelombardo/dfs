# DFS Deployment Guide for Restartos (Ephemeral OS)

## Overview

This guide covers deploying DFS on Restartos or similar ephemeral operating systems where everything except specific persistent storage gets wiped on reboot.

## Architecture for Restartos

### What Persists
- **External Storage Mount** (e.g., `/mnt/storage/`)
  - DFS data chunks
  - DFS metadata database
  - Configuration files
  - Compiled binaries (recommended)

### What Gets Rebuilt on Reboot
- System packages (unless using persistent overlays)
- Temporary files in `/tmp/`
- Service configurations (unless in persistent location)

## Prerequisites

1. **External persistent storage mounted** (e.g., `/mnt/storage/dfs/`)
2. **Rust toolchain** (if rebuilding on boot) OR **pre-compiled binaries** on persistent storage
3. **Network configuration** persisted across reboots
4. **FUSE support** enabled in kernel

## Deployment Options

### Option 1: Quick Single-Node Deployment

For testing or a single-node setup:

```bash
# Run interactive deployment
./scripts/deploy-node.sh
```

Answer the prompts:
- **Node IP**: Your server's IP or `0.0.0.0` for all interfaces
- **Node Port**: `8900` (or any available port)
- **Data Directory**: `/mnt/storage/dfs/data`
- **Metadata Directory**: `/mnt/storage/dfs/metadata`
- **Config Directory**: `/mnt/storage/dfs/config`
- **Is seed node?**: `yes` (for first node)
- **Replication Factor**: `3`
- **Chunk Size**: `4` MB

### Option 2: Multi-Node Cluster Deployment

#### Step 1: Deploy Seed Node (First Node)

On your first server (e.g., `192.168.1.10`):

```bash
./scripts/deploy-node.sh
```

Answers:
```
Node IP: 192.168.1.10
Node Port: 8900
Data Directory: /mnt/storage/dfs/data
Metadata Directory: /mnt/storage/dfs/metadata
Config Directory: /mnt/storage/dfs/config
Is seed node?: yes
Replication Factor: 3
Chunk Size: 4
```

#### Step 2: Deploy Additional Nodes

On each additional server (e.g., `192.168.1.11`, `192.168.1.12`, etc.):

```bash
./scripts/deploy-node.sh
```

Answers:
```
Node IP: 192.168.1.11  (this server's IP)
Node Port: 8900
Data Directory: /mnt/storage/dfs/data
Metadata Directory: /mnt/storage/dfs/metadata
Config Directory: /mnt/storage/dfs/config
Is seed node?: no
Seed node: 192.168.1.10:8900  (the seed node from Step 1)
Replication Factor: 3
Chunk Size: 4
```

#### Step 3: Start All Nodes

On each server:

```bash
# Using systemd (preferred)
sudo systemctl start dfs-server

# OR using manual start script
/mnt/storage/dfs/config/start-node.sh
```

#### Step 4: Verify Cluster

From any node:

```bash
./target/release/dfs-admin --cluster 192.168.1.10:8900 cluster status
```

Expected output:
```
Total Nodes:   3
Healthy Nodes: 3
```

## Persisting Binary Files

### Option A: Copy Binaries to Persistent Storage

```bash
# Create bin directory
mkdir -p /mnt/storage/dfs/bin

# Copy compiled binaries
cp target/release/dfs-server /mnt/storage/dfs/bin/
cp target/release/dfs-client /mnt/storage/dfs/bin/
cp target/release/dfs-admin /mnt/storage/dfs/bin/

# Update PATH or modify service to use persistent binaries
export PATH="/mnt/storage/dfs/bin:$PATH"
```

Update systemd service to use persistent binary:
```bash
sudo sed -i 's|ExecStart=.*|ExecStart=/mnt/storage/dfs/bin/dfs-server start --config /mnt/storage/dfs/config/config.toml|' /etc/systemd/system/dfs-server.service
sudo systemctl daemon-reload
```

### Option B: Build Script for Restartos Boot

Create `/mnt/storage/dfs/rebuild.sh`:

```bash
#!/bin/bash
# Rebuild DFS binaries on boot

cd /mnt/storage/dfs/source
cargo build --release

# Copy binaries
cp target/release/* /mnt/storage/dfs/bin/

# Start service
systemctl start dfs-server
```

## Restartos Persistent Configuration

### 1. Make Service Start on Boot

Create persistent service symlink:

```bash
# Ensure service file is on persistent storage
cp /etc/systemd/system/dfs-server.service /mnt/storage/dfs/config/dfs-server.service

# Create boot script to restore service
cat > /mnt/storage/dfs/restore-service.sh <<'EOF'
#!/bin/bash
# Restore DFS service on boot
cp /mnt/storage/dfs/config/dfs-server.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable dfs-server
systemctl start dfs-server
EOF

chmod +x /mnt/storage/dfs/restore-service.sh
```

### 2. Configure Restartos to Run Boot Script

Add to your Restartos boot configuration (location varies by setup):

```bash
# Example: Add to /etc/rc.local or systemd-tmpfiles
/mnt/storage/dfs/restore-service.sh
```

### 3. Persist Network Configuration

Ensure your network configuration (IP addresses, routes) persists across reboots.
This is typically handled by Restartos configuration files in persistent storage.

## Directory Structure

Recommended persistent storage layout:

```
/mnt/storage/dfs/
├── bin/                          # Compiled binaries
│   ├── dfs-server
│   ├── dfs-client
│   └── dfs-admin
├── config/                       # Configuration files
│   ├── config.toml
│   ├── start-node.sh
│   └── dfs-server.service
├── data/                         # Chunk storage (can be huge!)
│   └── [chunk files]
├── metadata/                     # Metadata database
│   └── [sled database files]
├── logs/                         # Optional: persistent logs
│   └── dfs-server.log
└── source/                       # Optional: source code for rebuilding
    └── [rust project]
```

## Storage Space Planning

### Calculating Required Space

For a cluster with:
- N nodes
- R replication factor
- D total data to store

Each node needs approximately: `(D * R) / N` storage space

**Example:**
- 5 nodes
- 3x replication
- 1TB total data

Each node needs: `(1TB * 3) / 5 = 600GB`

### Recommendations:
- Add 20-30% overhead for metadata and temporary files
- Monitor disk usage regularly
- Plan for growth

## Monitoring and Maintenance

### Check Node Status

```bash
# Check service status
systemctl status dfs-server

# Check logs
journalctl -u dfs-server -f

# OR if using manual start
tail -f /mnt/storage/dfs/logs/dfs-server.log
```

### Check Cluster Health

```bash
# Cluster status
./target/release/dfs-admin --cluster <SEED_IP>:8900 cluster status

# Storage statistics
./target/release/dfs-admin --cluster <SEED_IP>:8900 storage stats

# Healing status
./target/release/dfs-admin --cluster <SEED_IP>:8900 healing status
```

### Disk Space Monitoring

```bash
# Check node storage usage
du -sh /mnt/storage/dfs/data
du -sh /mnt/storage/dfs/metadata

# Count chunks
ls /mnt/storage/dfs/data | wc -l
```

## Client Usage

### Mount DFS Filesystem

```bash
# Create mount point
mkdir -p /mnt/dfs

# Mount
./target/release/dfs-client mount /mnt/dfs --cluster <SEED_IP>:8900

# Verify
df -h /mnt/dfs
ls -la /mnt/dfs
```

### Using DFS

```bash
# Write files
cp /path/to/file /mnt/dfs/

# Read files
cat /mnt/dfs/file

# Delete files
rm /mnt/dfs/file

# List files
ls -lah /mnt/dfs/
```

## Troubleshooting

### Node Won't Join Cluster

1. **Check network connectivity:**
   ```bash
   ping <SEED_IP>
   telnet <SEED_IP> 8900
   ```

2. **Check firewall:**
   ```bash
   # Allow DFS port
   sudo firewall-cmd --add-port=8900/tcp --permanent
   sudo firewall-cmd --reload
   ```

3. **Check seed node is running:**
   ```bash
   ./target/release/dfs-admin --cluster <SEED_IP>:8900 cluster status
   ```

4. **Check configuration:**
   ```bash
   cat /mnt/storage/dfs/config/config.toml | grep seed_nodes
   ```

### Node Marked as Failed

Wait 30 seconds (failure timeout) for detection, then:

1. **Restart node:**
   ```bash
   systemctl restart dfs-server
   ```

2. **Node will automatically rejoin** with new UUID

3. **Old failed node entry will remain** in cluster list but marked as failed

### Storage Full

1. **Check disk space:**
   ```bash
   df -h /mnt/storage
   ```

2. **If full, either:**
   - Add more storage
   - Add more nodes to distribute load
   - Reduce replication factor (not recommended)

### After Reboot

1. **Verify persistent storage mounted:**
   ```bash
   ls /mnt/storage/dfs
   ```

2. **Verify service started:**
   ```bash
   systemctl status dfs-server
   ```

3. **Check cluster status:**
   ```bash
   ./target/release/dfs-admin --cluster <LOCAL_IP>:8900 cluster status
   ```

## Production Recommendations

1. **Minimum 3 nodes** for fault tolerance
2. **Replication factor of 3** for data safety
3. **Monitor disk usage** (set alerts at 80% full)
4. **Regular backups** of metadata directory
5. **Network redundancy** (bonded interfaces)
6. **UPS or battery backup** for data integrity
7. **Regular testing** of failover scenarios

## Quick Reference Commands

```bash
# Deploy node
./scripts/deploy-node.sh

# Start service
systemctl start dfs-server
systemctl enable dfs-server

# Check cluster
./target/release/dfs-admin --cluster <IP>:8900 cluster status

# Mount filesystem
./target/release/dfs-client mount /mnt/dfs --cluster <IP>:8900

# Unmount
fusermount -u /mnt/dfs

# View logs
journalctl -u dfs-server -f

# Stop service
systemctl stop dfs-server
```

## Example: 3-Node Production Cluster

### Node 1 (Seed): 192.168.1.10
```bash
./scripts/deploy-node.sh
# Answer: seed=yes, ip=192.168.1.10
systemctl start dfs-server
```

### Node 2: 192.168.1.11
```bash
./scripts/deploy-node.sh
# Answer: seed=no, ip=192.168.1.11, seed_node=192.168.1.10:8900
systemctl start dfs-server
```

### Node 3: 192.168.1.12
```bash
./scripts/deploy-node.sh
# Answer: seed=no, ip=192.168.1.12, seed_node=192.168.1.10:8900
systemctl start dfs-server
```

### Verify
```bash
./target/release/dfs-admin --cluster 192.168.1.10:8900 cluster status
# Expected: Total Nodes: 3, Healthy Nodes: 3
```

### Client
```bash
./target/release/dfs-client mount /mnt/dfs --cluster 192.168.1.10:8900
echo "Hello DFS!" > /mnt/dfs/test.txt
cat /mnt/dfs/test.txt
```

## Summary

✅ **DFS works well on Restartos** with proper persistent storage configuration
✅ **All critical data** (chunks, metadata, config) on persistent storage
✅ **Binaries** can be persisted or rebuilt on boot
✅ **Automatic cluster formation** via seed nodes
✅ **Fault tolerant** with 3x replication
✅ **Self-healing** replicas after node failures

The key is ensuring `/mnt/storage/dfs/` (or your persistent mount point) contains everything needed to restore service on reboot.
