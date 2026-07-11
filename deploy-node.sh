#!/bin/bash
# deploy-node.sh — Provision a new DFS storage node
#
# Usage:
#   ./deploy-node.sh <node-ip> [seed-ip]
#
# Examples:
#   # First node in a new cluster (no seed)
#   ./deploy-node.sh 192.168.1.10
#
#   # Additional node joining an existing cluster
#   ./deploy-node.sh 192.168.1.12 192.168.1.10
#
# Prerequisites:
#   - SSH access as root to <node-ip>
#   - Release binaries already built: cargo build --release
#   - /mnt/dfs-data (or your preferred storage path) mounted on the target node

set -euo pipefail

NODE_IP="${1:?Usage: $0 <node-ip> [seed-ip]}"
SEED_IP="${2:-}"
PORT=8900
DATA_DIR="/mnt/dfs-data/dfs"
BIN_DIR="/usr/bin"
SYSTEMD_UNIT="/etc/systemd/system/dfs-server.service"

echo "======================================================================"
echo "  DFS Node Provisioner"
echo "======================================================================"
echo "  Node IP : $NODE_IP:$PORT"
echo "  Seed    : ${SEED_IP:-none (first node)}"
echo "  Data    : $DATA_DIR"
echo "======================================================================"
echo

# ── 1. Copy binaries ────────────────────────────────────────────────────────
echo "[1/5] Copying binaries..."
scp target/release/dfs-server target/release/dfs-admin root@"$NODE_IP":"$BIN_DIR/"
echo "      OK"

# ── 2. Create directory layout ───────────────────────────────────────────────
echo "[2/5] Creating directory layout..."
ssh root@"$NODE_IP" "mkdir -p $DATA_DIR/{data,metadata,config}"
echo "      OK"

# ── 3. Write config.toml ────────────────────────────────────────────────────
echo "[3/5] Writing config..."
if [ -n "$SEED_IP" ]; then
    SEED_NODES="[\"$SEED_IP:$PORT\"]"
else
    SEED_NODES="[]"
fi

ssh root@"$NODE_IP" "cat > $DATA_DIR/config/config.toml" <<EOF
[node]
listen_addr = "$NODE_IP:$PORT"

[storage]
data_dir = "$DATA_DIR/data"
metadata_dir = "$DATA_DIR/metadata"
chunk_size_mb = 4

[cluster]
seed_nodes = $SEED_NODES
heartbeat_interval_secs = 10
failure_timeout_secs = 30

[replication]
replication_factor = 3
healing_delay_secs = 300
auto_heal = true
scrub_interval_hours = 24
EOF
echo "      OK"

# ── 4. Install systemd unit ──────────────────────────────────────────────────
echo "[4/5] Installing systemd unit..."
ssh root@"$NODE_IP" "cat > $SYSTEMD_UNIT" <<EOF
[Unit]
Description=DFS Storage Node
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/dfs-server start --config $DATA_DIR/config/config.toml --log-level warn --log-file $DATA_DIR/logs/dfs-server.log
Restart=always
RestartSec=10
StandardOutput=journal
StandardError=journal
SyslogIdentifier=dfs-server
Environment="RUST_LOG=warn"
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
ssh root@"$NODE_IP" "systemctl daemon-reload && systemctl enable dfs-server"
echo "      OK"

# ── 5. Start service ─────────────────────────────────────────────────────────
echo "[5/5] Starting dfs-server..."
ssh root@"$NODE_IP" "systemctl restart dfs-server"
sleep 3
STATUS=$(ssh root@"$NODE_IP" "systemctl is-active dfs-server")

echo
echo "======================================================================"
if [ "$STATUS" = "active" ]; then
    echo "  Node $NODE_IP provisioned successfully!"
    echo
    echo "  Next steps:"
    if [ -z "$SEED_IP" ]; then
        echo "  - This is your first node. Add more nodes with:"
        echo "      ./deploy-node.sh <new-ip> $NODE_IP"
    else
        echo "  - Node is joining cluster via seed $SEED_IP"
        echo "  - Check status: ssh root@$NODE_IP journalctl -u dfs-server -f"
    fi
    echo "  - Mount a client: dfs-client mount /mnt/dfs --cluster $NODE_IP:$PORT"
else
    echo "  ERROR: dfs-server is not active (status: $STATUS)"
    echo "  Check logs: ssh root@$NODE_IP journalctl -u dfs-server -n 50"
    exit 1
fi
echo "======================================================================"
