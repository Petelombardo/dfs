#!/bin/bash
# Deploy DFS Server Node
# For use on Restartos or other ephemeral OS systems
# Usage: ./deploy-node.sh

set -e

# ANSI colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${BLUE}==================================================${NC}"
echo -e "${BLUE}     DFS Server Node Deployment Setup${NC}"
echo -e "${BLUE}==================================================${NC}"
echo ""

# Function to prompt for input with default
prompt() {
    local var_name=$1
    local prompt_text=$2
    local default_value=$3

    echo -ne "${GREEN}${prompt_text}${NC}"
    if [ -n "$default_value" ]; then
        echo -ne " [${YELLOW}${default_value}${NC}]: "
    else
        echo -ne ": "
    fi
    read -r input

    if [ -z "$input" ] && [ -n "$default_value" ]; then
        eval "$var_name='$default_value'"
    else
        eval "$var_name='$input'"
    fi
}

# Gather configuration
echo -e "${YELLOW}Node Configuration:${NC}"
echo ""

# Get node address
while true; do
    prompt NODE_IP "Node IP address (not hostname)" "0.0.0.0"

    # Validate it's an IP address
    if [[ "$NODE_IP" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || [ "$NODE_IP" = "0.0.0.0" ]; then
        break
    else
        echo -e "${RED}Error: Please enter an IP address (e.g., 192.168.1.10 or 0.0.0.0), not a hostname${NC}"
        echo -e "${YELLOW}Tip: Use 'hostname -I' to see your IP addresses${NC}"
    fi
done

prompt NODE_PORT "Node port" "8900"
prompt DATA_DIR "Data storage directory (persistent)" "/mnt/storage/dfs/data"
prompt META_DIR "Metadata directory (persistent)" "/mnt/storage/dfs/metadata"
prompt CONFIG_DIR "Config directory (persistent)" "/mnt/storage/dfs/config"

echo ""
echo -e "${YELLOW}Cluster Configuration:${NC}"
echo ""

prompt IS_SEED "Is this the first/seed node? (yes/no)" "no"
if [ "$IS_SEED" != "yes" ]; then
    prompt SEED_NODE "Seed node address (IP:PORT)" "192.168.1.10:8900"
fi

prompt REPLICATION_FACTOR "Replication factor" "3"
prompt CHUNK_SIZE_MB "Chunk size in MB" "4"

echo ""
echo -e "${YELLOW}Advanced Configuration:${NC}"
echo ""

prompt HEARTBEAT_INTERVAL "Heartbeat interval (seconds)" "10"
prompt FAILURE_TIMEOUT "Failure timeout (seconds)" "30"
prompt AUTO_HEAL "Auto-heal missing replicas? (true/false)" "true"

# Summary
echo ""
echo -e "${BLUE}==================================================${NC}"
echo -e "${BLUE}     Configuration Summary${NC}"
echo -e "${BLUE}==================================================${NC}"
echo -e "Node Address:        ${GREEN}${NODE_IP}:${NODE_PORT}${NC}"
echo -e "Data Directory:      ${GREEN}${DATA_DIR}${NC}"
echo -e "Metadata Directory:  ${GREEN}${META_DIR}${NC}"
echo -e "Config Directory:    ${GREEN}${CONFIG_DIR}${NC}"
if [ "$IS_SEED" = "yes" ]; then
    echo -e "Node Type:           ${YELLOW}SEED NODE${NC}"
else
    echo -e "Seed Node:           ${GREEN}${SEED_NODE}${NC}"
fi
echo -e "Replication Factor:  ${GREEN}${REPLICATION_FACTOR}${NC}"
echo -e "Chunk Size:          ${GREEN}${CHUNK_SIZE_MB} MB${NC}"
echo ""

read -p "Proceed with deployment? (yes/no): " confirm
if [ "$confirm" != "yes" ]; then
    echo -e "${RED}Deployment cancelled${NC}"
    exit 1
fi

echo ""
echo -e "${BLUE}Deploying node...${NC}"
echo ""

# Create directories
echo -e "${GREEN}→${NC} Creating directories..."
mkdir -p "$DATA_DIR"
mkdir -p "$META_DIR"
mkdir -p "$CONFIG_DIR"

# Initialize node
echo -e "${GREEN}→${NC} Initializing DFS node..."
dfs-server init \
    --data-dir "$DATA_DIR" \
    --meta-dir "$META_DIR" \
    --config "$CONFIG_DIR/config.toml"

# Update configuration
echo -e "${GREEN}→${NC} Configuring node..."

# Set listen address
sed -i "s/listen_addr = \"0.0.0.0:8900\"/listen_addr = \"${NODE_IP}:${NODE_PORT}\"/" "$CONFIG_DIR/config.toml"

# Set chunk size
sed -i "s/chunk_size_mb = [0-9]\\+/chunk_size_mb = ${CHUNK_SIZE_MB}/" "$CONFIG_DIR/config.toml"

# Set replication factor
sed -i "s/replication_factor = [0-9]\\+/replication_factor = ${REPLICATION_FACTOR}/" "$CONFIG_DIR/config.toml"

# Set heartbeat interval
sed -i "s/heartbeat_interval_secs = [0-9]\\+/heartbeat_interval_secs = ${HEARTBEAT_INTERVAL}/" "$CONFIG_DIR/config.toml"

# Set failure timeout
sed -i "s/failure_timeout_secs = [0-9]\\+/failure_timeout_secs = ${FAILURE_TIMEOUT}/" "$CONFIG_DIR/config.toml"

# Set auto-heal
sed -i "s/auto_heal = [a-z]\\+/auto_heal = ${AUTO_HEAL}/" "$CONFIG_DIR/config.toml"

# Configure seed nodes
if [ "$IS_SEED" != "yes" ]; then
    sed -i "s/seed_nodes = \\[\\]/seed_nodes = [\"${SEED_NODE}\"]/" "$CONFIG_DIR/config.toml"
fi

# Create systemd service (if systemd available)
if command -v systemctl &> /dev/null; then
    echo -e "${GREEN}→${NC} Creating systemd service..."

    cat > /tmp/dfs-server.service <<EOF
[Unit]
Description=DFS Storage Node
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/dfs-server start --config ${CONFIG_DIR}/config.toml
Restart=always
RestartSec=10
StandardOutput=journal
StandardError=journal
SyslogIdentifier=dfs-server
Environment="RUST_LOG=info"

[Install]
WantedBy=multi-user.target
EOF

    sudo cp /tmp/dfs-server.service /etc/systemd/system/dfs-server.service
    sudo systemctl daemon-reload

    echo -e "${GREEN}✓${NC} Systemd service created"
    echo -e "  To enable on boot: ${YELLOW}sudo systemctl enable dfs-server${NC}"
    echo -e "  To start now:      ${YELLOW}sudo systemctl start dfs-server${NC}"
    echo -e "  To check status:   ${YELLOW}sudo systemctl status dfs-server${NC}"
    echo -e "  To view logs:      ${YELLOW}sudo journalctl -u dfs-server -f${NC}"
fi

# Create start script for manual use
echo -e "${GREEN}→${NC} Creating start script..."
cat > "$CONFIG_DIR/start-node.sh" <<EOF
#!/bin/bash
# Start DFS Server Node
RUST_LOG=info /usr/bin/dfs-server start --config ${CONFIG_DIR}/config.toml
EOF
chmod +x "$CONFIG_DIR/start-node.sh"

echo ""
echo -e "${BLUE}==================================================${NC}"
echo -e "${GREEN}     Deployment Complete!${NC}"
echo -e "${BLUE}==================================================${NC}"
echo ""
echo -e "${YELLOW}Configuration saved to:${NC} $CONFIG_DIR/config.toml"
echo ""
echo -e "${YELLOW}To start the node:${NC}"
if command -v systemctl &> /dev/null; then
    echo -e "  ${GREEN}sudo systemctl start dfs-server${NC} (systemd)"
    echo -e "  OR"
fi
echo -e "  ${GREEN}$CONFIG_DIR/start-node.sh${NC} (manual)"
echo ""
echo -e "${YELLOW}To check cluster status:${NC}"
echo -e "  ${GREEN}dfs-admin --cluster ${NODE_IP}:${NODE_PORT} cluster status${NC}"
echo ""

if [ "$IS_SEED" = "yes" ]; then
    echo -e "${YELLOW}⚠ This is a SEED NODE${NC}"
    echo -e "  Other nodes should use ${GREEN}${NODE_IP}:${NODE_PORT}${NC} as their seed address"
    echo ""
fi

echo -e "${YELLOW}Deployment Tips for Restartos:${NC}"
echo -e "  1. Ensure ${GREEN}$CONFIG_DIR${NC} is on persistent storage"
echo -e "  2. Ensure ${GREEN}$DATA_DIR${NC} is on persistent storage"
echo -e "  3. Ensure ${GREEN}$META_DIR${NC} is on persistent storage"
echo -e "  4. Copy binaries to persistent location or rebuild on boot"
echo -e "  5. Set up service to start on boot (systemd or init script)"
echo ""
