#!/bin/bash
# Local sweep to test whether chunk_ring's capacity (dfs-server/src/server.rs,
# a flat 32-chunk constant since it was added 2026-07-11) is the actual root
# cause of the size-dependent write throughput cliff chased on staging
# 2026-07-13 (server5, real kdiskmark: 512MB fine after the count-based fold
# trigger fix, 1GB still collapsed to 0.31MB/s).
#
# 32 chunks = 128MB — exactly where things stop being fast. Past that, every
# fold's base-chunk read misses the ring and falls back to a real disk read.
# This sweep drives a local 5-node cluster with a wide random-write storm
# against a file sized past 32 active chunks, at several DFS_CHUNK_RING_CAPACITY
# values, and reports the new chunk_ring hit/miss stats (added alongside the
# env var override) plus achieved throughput for each, so the fix can be
# validated with real numbers instead of another guess.
#
# Usage: bash scripts/sweep_chunk_ring_capacity.sh [duration_secs] [file_size_mb] [capacities...]
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-sweep-ring
MOUNT=/tmp/dfs-sweep-ring-mount
LOG=/tmp/dfs-sweep-ring-logs
CLUSTER="127.0.0.1:8990,127.0.0.1:8991,127.0.0.1:8992,127.0.0.1:8993,127.0.0.1:8994"
BIN="$REPO/target/release"
DURATION="${1:-40}"
FILE_SIZE_MB="${2:-256}"
shift 2 2>/dev/null || shift $# 2>/dev/null || true
CAPACITIES=("$@")
if [ "${#CAPACITIES[@]}" -eq 0 ]; then
    CAPACITIES=(16 32 64 128 256)
fi

cleanup_all() {
    pkill -f "dfs-server.*dfs-sweep-ring" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
    sleep 0.5
}

run_one_capacity() {
    local cap="$1"
    local run_log="$LOG/cap_${cap}"
    mkdir -p "$run_log"

    cleanup_all
    rm -rf "$BASE" "$MOUNT"
    mkdir -p "$MOUNT" "$BASE"

    for i in 1 2 3 4 5; do
        NODE_DIR="$BASE/node${i}"
        PORT=$((8990 + i - 1))
        "$BIN/dfs-server" init --data-dir "$NODE_DIR/data" --meta-dir "$NODE_DIR/metadata" --config "$NODE_DIR/config.toml" >/dev/null
        sed -i "s/listen_addr = \"0.0.0.0:8900\"/listen_addr = \"127.0.0.1:${PORT}\"/" "$NODE_DIR/config.toml"
        if [ $i -gt 1 ]; then
            sed -i 's/seed_nodes = \[\]/seed_nodes = ["127.0.0.1:8990"]/' "$NODE_DIR/config.toml"
        fi
    done
    for i in 1 2 3 4 5; do
        DFS_CHUNK_RING_CAPACITY="$cap" nohup "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" --log-level info >> "$run_log/server${i}.log" 2>&1 &
    done
    sleep 3

    nohup "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
        --log-file "$run_log/client.log" --allow-other --log-level info &
    sleep 2
    mountpoint -q "$MOUNT" || { echo "  MOUNT FAILED for capacity=$cap"; tail -30 "$run_log/client.log"; return 1; }

    IMG="$MOUNT/wide.img"
    dd if=/dev/zero of="$IMG" bs=1M count="$FILE_SIZE_MB" 2>/dev/null
    sync
    sleep 1

    FILE_SIZE_BYTES=$((FILE_SIZE_MB * 1024 * 1024))
    local start_ts=$(date +%s.%N)
    writer_pids=()
    for w in $(seq 1 32); do
        python3 - "$IMG" "$DURATION" "$FILE_SIZE_BYTES" "$w" > "$run_log/writer${w}.log" 2>&1 <<'PYEOF' &
import os, sys, random, time
img, duration, file_size, wid = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
fd = os.open(img, os.O_WRONLY)
end = time.time() + duration
n = 0
total_bytes = 0
while time.time() < end:
    length = 4096
    off = random.randint(0, file_size - length)
    data = os.urandom(length)
    os.pwrite(fd, data, off)
    n += 1
    total_bytes += length
os.fsync(fd)
os.close(fd)
print(f"{n} {total_bytes}")
PYEOF
        writer_pids+=($!)
    done
    wait "${writer_pids[@]}"
    local end_ts=$(date +%s.%N)
    sync
    sleep 2   # let the 30s stats-loop tick fire and flush before we grep it

    local total_ops=0
    local total_bytes=0
    for w in $(seq 1 32); do
        read -r ops bytes < "$run_log/writer${w}.log"
        total_ops=$((total_ops + ops))
        total_bytes=$((total_bytes + bytes))
    done
    local wall=$(echo "$end_ts - $start_ts" | bc)
    local mbps=$(echo "scale=3; $total_bytes / 1048576 / $wall" | bc)
    local iops=$(echo "scale=1; $total_ops / $wall" | bc)

    # Aggregate chunk_ring stats across all 5 nodes for this run.
    local total_hits=0
    local total_misses=0
    for i in 1 2 3 4 5; do
        sed -r 's/\x1b\[[0-9;]*m//g' "$run_log/server${i}.log" > "$run_log/server${i}_clean.log"
        local h=$(grep -oE '[0-9]+ hits' "$run_log/server${i}_clean.log" | awk '{s+=$1} END{print s+0}')
        local m=$(grep -oE '[0-9]+ misses' "$run_log/server${i}_clean.log" | awk '{s+=$1} END{print s+0}')
        total_hits=$((total_hits + h))
        total_misses=$((total_misses + m))
    done
    local ring_total=$((total_hits + total_misses))
    local hit_pct="n/a"
    if [ "$ring_total" -gt 0 ]; then
        hit_pct=$(echo "scale=1; 100 * $total_hits / $ring_total" | bc)
    fi

    printf "capacity=%-4s ops=%-7s %7.2f MB/s  %7.1f iops/s   ring_hits=%-6s ring_misses=%-6s hit_rate=%s%%\n" \
        "$cap" "$total_ops" "$mbps" "$iops" "$total_hits" "$total_misses" "$hit_pct"

    cleanup_all
}

echo "=== chunk_ring capacity sweep: duration=${DURATION}s file=${FILE_SIZE_MB}MB ($(( FILE_SIZE_MB / 4 )) chunks) capacities=${CAPACITIES[*]} ==="
mkdir -p "$LOG"
echo ""
for cap in "${CAPACITIES[@]}"; do
    run_one_capacity "$cap"
done
echo ""
echo "Per-run logs: $LOG/cap_<N>/"
