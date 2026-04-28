#!/bin/bash
# Test pihole SQLite database on DFS
# This uses the real pihole docker image as a real-world SQLite test case

set -e

# Configuration
MOUNT_POINT="/tmp/dfs-pihole-test"
PIHOLE_DIR="${MOUNT_POINT}/pihole"
CLIENT_LOG="/tmp/dfs-client-pihole.log"
CONTAINER_NAME="dfs-pihole-test"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log() {
    echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $1"
}

error() {
    echo -e "${RED}[$(date +%H:%M:%S)] ERROR:${NC} $1"
}

warn() {
    echo -e "${YELLOW}[$(date +%H:%M:%S)] WARNING:${NC} $1"
}

info() {
    echo -e "${BLUE}[$(date +%H:%M:%S)] INFO:${NC} $1"
}

cleanup() {
    log "Cleaning up..."

    # Stop and remove pihole container
    if sudo docker ps -a | grep -q "$CONTAINER_NAME"; then
        log "Stopping pihole container..."
        sudo docker stop "$CONTAINER_NAME" 2>/dev/null || true
        sudo docker rm "$CONTAINER_NAME" 2>/dev/null || true
    fi

    # Unmount DFS
    if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
        log "Unmounting DFS..."
        fusermount -u "$MOUNT_POINT" 2>/dev/null || true
        sleep 1
    fi

    # Kill client
    pkill -9 dfs-client 2>/dev/null || true

    # Stop cluster
    log "Stopping DFS cluster..."
    pkill -9 dfs-server 2>/dev/null || true

    # Clean up mount point
    if [ -d "$MOUNT_POINT" ]; then
        rm -rf "$MOUNT_POINT"
    fi

    log "Cleanup complete"
}

trap cleanup EXIT

echo "======================================================================"
echo "  Pi-hole SQLite Test on DFS"
echo "======================================================================"
echo ""
log "This test will:"
log "  1. Start a local 3-node DFS cluster"
log "  2. Mount DFS at $MOUNT_POINT"
log "  3. Copy pihole docker-compose.yml"
log "  4. Run pihole container with SQLite database on DFS"
log "  5. Monitor for SQLite errors"
echo ""

# Step 1: Setup and start local cluster
log "Step 1: Setting up local DFS cluster..."
bash scripts/setup-cluster.sh 3 || {
    error "Failed to setup DFS cluster"
    exit 1
}

log "Starting 3 DFS server nodes..."
BASE_DIR="/tmp/dfs-test"
for i in 1 2 3; do
    PORT=$((8899 + i))
    log "  Starting node $i on port $PORT..."
    target/release/dfs-server start \
        --config "${BASE_DIR}/node${i}/config.toml" \
        --log-level info \
        > "/tmp/dfs-server${i}.log" 2>&1 &
    sleep 0.5
done

sleep 3

# Verify servers started
RUNNING=$(ps aux | grep "[d]fs-server start" | wc -l)
if [ "$RUNNING" -ne 3 ]; then
    error "Only $RUNNING/3 servers started"
    exit 1
fi
log "✓ All 3 servers started"

# Step 2: Mount DFS (in debug mode for detailed logging)
log "Step 2: Mounting DFS in DEBUG mode..."
mkdir -p "$MOUNT_POINT"

log "Starting client with DEBUG logging..."
target/release/dfs-client mount \
    --cluster 127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902 \
    --foreground \
    --log-level debug \
    "$MOUNT_POINT" > "$CLIENT_LOG" 2>&1 &

CLIENT_PID=$!
sleep 3

if ! mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
    error "Failed to mount DFS"
    error "Check log: $CLIENT_LOG"
    tail -20 "$CLIENT_LOG"
    exit 1
fi

log "✓ DFS mounted successfully at $MOUNT_POINT"
log "  Client PID: $CLIENT_PID"
log "  Client log: $CLIENT_LOG"
echo ""

# Step 3: Set up pihole directory structure
log "Step 3: Setting up pihole directory structure..."
mkdir -p "${PIHOLE_DIR}/etc-pihole"
mkdir -p "${PIHOLE_DIR}/etc-dnsmasq.d"

# Copy docker-compose.yml and modify it for testing
log "Copying and modifying docker-compose.yml..."
cat > "${PIHOLE_DIR}/docker-compose.yml" <<'EOF'
services:
  pihole:
    container_name: dfs-pihole-test
    image: pihole/pihole:latest
    dns:
      - 8.8.8.8
      - 8.8.4.4
    shm_size: '256mb'
    ports:
      - "15353:53/tcp"
      - "15353:53/udp"
      - "18800:80/tcp"
    environment:
      TZ: 'America/New_York'
      DNS1: 8.8.8.8
      DNS2: 8.8.4.4
      DNSMASQ_LISTENING: local
    volumes:
      - './etc-pihole/:/etc/pihole/'
      - './etc-dnsmasq.d/:/etc/dnsmasq.d/'
    cap_add:
      - NET_ADMIN
    deploy:
      resources:
        limits:
          memory: 500m
        reservations:
          memory: 100m
EOF

log "✓ Directory structure created"
log "  Pihole data: ${PIHOLE_DIR}/etc-pihole/"
log "  Dnsmasq config: ${PIHOLE_DIR}/etc-dnsmasq.d/"
echo ""

# Step 4: Start pihole container
log "Step 4: Starting pihole container..."
log "This will create SQLite databases on DFS..."
echo ""

cd "${PIHOLE_DIR}"

warn "Starting pihole container - watch for SQLite errors..."
echo ""

# Start container and tail logs
# Use docker-compose (v1) or docker compose (v2) depending on what's available
if command -v docker-compose &> /dev/null; then
    sudo docker-compose up -d
else
    sudo docker compose up -d
fi

sleep 2

log "Container started. Monitoring logs..."
log "Press Ctrl+C to stop monitoring (container will continue running)"
echo ""
echo "======================================================================"
echo "  Pihole Container Logs"
echo "======================================================================"
echo ""

# Monitor logs for 30 seconds or until error
timeout 30 sudo docker logs -f "$CONTAINER_NAME" 2>&1 || true

echo ""
echo "======================================================================"
log "Checking container status..."
if sudo docker ps | grep -q "$CONTAINER_NAME"; then
    log "✓ Container is still running"
else
    error "✗ Container has stopped or crashed"
    log "Full container logs:"
    sudo docker logs "$CONTAINER_NAME" 2>&1
fi

echo ""
log "Checking SQLite databases created on DFS..."
ls -lh "${PIHOLE_DIR}/etc-pihole/" || true

echo ""
log "Checking for SQLite database files..."
find "${PIHOLE_DIR}/etc-pihole/" -name "*.db*" -ls || true

echo ""
log "Testing SQLite database integrity..."
for db in "${PIHOLE_DIR}/etc-pihole/"*.db; do
    if [ -f "$db" ]; then
        info "Checking: $db"
        sqlite3 "$db" "PRAGMA integrity_check;" || {
            error "Integrity check failed for $db"
        }
        sqlite3 "$db" "PRAGMA quick_check;" || {
            error "Quick check failed for $db"
        }
    fi
done

echo ""
log "Checking DFS client debug log for errors..."
grep -i "error\|warn\|fail" "$CLIENT_LOG" | tail -20 || log "No obvious errors in client log"

echo ""
echo "======================================================================"
echo "  Test Summary"
echo "======================================================================"
log "Container name: $CONTAINER_NAME"
log "Mount point: $MOUNT_POINT"
log "Pihole data: ${PIHOLE_DIR}/etc-pihole/"
log "Client log: $CLIENT_LOG"
echo ""
log "To interact with pihole:"
log "  sudo docker exec -it $CONTAINER_NAME bash"
log ""
log "To check SQLite databases:"
log "  sqlite3 ${PIHOLE_DIR}/etc-pihole/gravity.db '.tables'"
log ""
log "To view client logs:"
log "  tail -f $CLIENT_LOG"
echo ""
warn "Container will keep running. Press Enter to stop and cleanup..."
read

log "Stopping test..."
