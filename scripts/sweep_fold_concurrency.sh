#!/bin/bash
# Local sweep to empirically find the right fold_concurrency width, instead of
# guessing via slow live kdiskmark cycles on staging (server5).
#
# Background: staging kdiskmark Q32T1/Q1T1 4K random writes against a VM's
# qcow2 disk collapsed after the fold/semaphore work landed. Widening
# fold_concurrency from 16->40 (plus jittering the per-slot 8s fold timer)
# made Q1T1 slightly better but Q32T1 WORSE (0.6 -> ~0.5 MB/s), and the
# min-2-replica backfill count kept climbing across successive runs (61 -> 93
# -> 152) despite the width increase. gluster1-5 are 4-core NVMe boxes;
# chunk_patch_locks is per-(file_id,chunk_idx) so the server has no global
# fold lock, but the client-side fold_concurrency semaphore is global-not-
# per-node, so widening it just lets more concurrent full-4MB-chunk blocking
# operations (fold + backfill) pile onto whichever node is targeted, plausibly
# oversubscribing its 4 cores. This sweep drives a local 5-node cluster with a
# kdiskmark-like wide random-write pattern against a multi-chunk file at
# several DFS_FOLD_CONCURRENCY values and reports achieved throughput plus
# ForceFold/backfill counts for each, so the right width can be picked from
# real numbers instead of another guess.
#
# Usage: bash scripts/sweep_fold_concurrency.sh [duration_secs] [file_size_mb] [widths...]
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-sweep-fold
MOUNT=/tmp/dfs-sweep-fold-mount
LOG=/tmp/dfs-sweep-fold-logs
CLUSTER="127.0.0.1:8980,127.0.0.1:8981,127.0.0.1:8982,127.0.0.1:8983,127.0.0.1:8984"
BIN="$REPO/target/release"
DURATION="${1:-30}"
FILE_SIZE_MB="${2:-256}"
shift 2 2>/dev/null || shift $# 2>/dev/null || true
WIDTHS=("$@")
if [ "${#WIDTHS[@]}" -eq 0 ]; then
    WIDTHS=(4 8 16 24 40)
fi

cleanup_all() {
    pkill -f "dfs-server.*dfs-sweep-fold" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
    sleep 0.5
}

run_one_width() {
    local width="$1"
    local run_log="$LOG/width_${width}"
    mkdir -p "$run_log"

    cleanup_all
    rm -rf "$BASE" "$MOUNT"
    mkdir -p "$MOUNT" "$BASE"

    for i in 1 2 3 4 5; do
        NODE_DIR="$BASE/node${i}"
        PORT=$((8980 + i - 1))
        "$BIN/dfs-server" init --data-dir "$NODE_DIR/data" --meta-dir "$NODE_DIR/metadata" --config "$NODE_DIR/config.toml" >/dev/null
        sed -i "s/listen_addr = \"0.0.0.0:8900\"/listen_addr = \"127.0.0.1:${PORT}\"/" "$NODE_DIR/config.toml"
        if [ $i -gt 1 ]; then
            sed -i 's/seed_nodes = \[\]/seed_nodes = ["127.0.0.1:8980"]/' "$NODE_DIR/config.toml"
        fi
    done
    for i in 1 2 3 4 5; do
        nohup "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" --log-level info >> "$run_log/server${i}.log" 2>&1 &
    done
    sleep 3

    DFS_FOLD_CONCURRENCY="$width" nohup "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
        --log-file "$run_log/client.log" --allow-other --log-level info &
    sleep 2
    mountpoint -q "$MOUNT" || { echo "  MOUNT FAILED for width=$width"; tail -30 "$run_log/client.log"; return 1; }

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
    os.fsync(fd)
    n += 1
    total_bytes += length
os.close(fd)
print(f"{n} {total_bytes}")
PYEOF
        writer_pids+=($!)
    done
    wait "${writer_pids[@]}"
    local end_ts=$(date +%s.%N)
    sync
    sleep 1

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

    sed -r 's/\x1b\[[0-9;]*m//g' "$run_log/client.log" > "$run_log/client_clean.log"
    local fold_count=$(grep -c "INFO ForceFold" "$run_log/client_clean.log")
    local backfill_count=$(grep -c "landed on only" "$run_log/client_clean.log")
    local noleader_count=$(grep -c "no known leader" "$run_log/client_clean.log")

    printf "width=%-4s ops=%-6s %6.2f MB/s  %7.1f iops/s   ForceFold=%-5s backfill=%-5s no_leader=%s\n" \
        "$width" "$total_ops" "$mbps" "$iops" "$fold_count" "$backfill_count" "$noleader_count"

    cleanup_all
}

echo "=== fold_concurrency sweep: duration=${DURATION}s file=${FILE_SIZE_MB}MB widths=${WIDTHS[*]} ==="
mkdir -p "$LOG"
echo ""
for width in "${WIDTHS[@]}"; do
    run_one_width "$width"
done
echo ""
echo "Per-run logs: $LOG/width_<N>/"
