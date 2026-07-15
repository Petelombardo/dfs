#!/bin/bash
# Probe: does shrinking chunk_ring/delta_ring capacity at a SMALL file size (256MB)
# reproduce the size-dependent write collapse seen at 512MB/1GB (down to 0.38MB/s
# at 1GB despite an apparently-healthy cache hit rate)? Zero rebuild required.
#
# Fixes two problems in the older scripts/sweep_chunk_ring_capacity.sh:
#   1. That script's stat grep (`grep -oE '[0-9]+ hits'`) matches BOTH the
#      "chunk_ring stats..." and "delta_ring stats..." log lines and sums them
#      into one number — silently combining two independently-tracked pools
#      (chunk_ring = base chunks, delta_ring = patch chunks, split 2026-07-13,
#      each logged separately every 30s by start_chunk_ring_stats_loop in
#      dfs-server/src/server.rs). A healthy combined rate can hide one pool
#      thrashing near 0%. This script reports both separately, always.
#   2. That script only swept DFS_CHUNK_RING_CAPACITY; DFS_DELTA_RING_CAPACITY
#      stayed at its RAM-tiered default throughout. Under a 4K-random-write
#      workload (kdiskmark-like), MultiPatch traffic through delta_ring is
#      probably hotter than base-chunk traffic through chunk_ring — so the
#      thing that actually needed shrinking to test cache-pressure may never
#      have been varied. This script sets both env vars together per run.
#
# Uses real fio (numjobs=32/iodepth=1/ioengine=sync as the Q32T1-equivalent —
# this FUSE mount doesn't set FOPEN_DIRECT_IO for regular files, so O_DIRECT/
# libaio queueing isn't meaningful here; 32 blocking writers is the same
# approximation scripts/sweep_fold_concurrency.sh already used) instead of the
# old ad hoc python writers, and samples server RSS every 2s during the run so
# a throughput collapse can be correlated against memory growth, not just
# hit rate.
#
# Usage: bash scripts/probe_ring_capacity_scale.sh [duration_secs] [file_size_mb] [capacities...]
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-probe-ring
MOUNT=/tmp/dfs-probe-ring-mount
LOG=/tmp/dfs-probe-ring-logs
CLUSTER="127.0.0.1:8990,127.0.0.1:8991,127.0.0.1:8992,127.0.0.1:8993,127.0.0.1:8994"
BIN="$REPO/target/release"
DURATION="${1:-30}"
FILE_SIZE_MB="${2:-256}"
shift 2 2>/dev/null || shift $# 2>/dev/null || true
CAPACITIES=("$@")
if [ "${#CAPACITIES[@]}" -eq 0 ]; then
    CAPACITIES=(default 4)
fi

cleanup_all() {
    pkill -f "dfs-server.*dfs-probe-ring" 2>/dev/null || true
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

    local envs=()
    if [ "$cap" != "default" ]; then
        envs=(env "DFS_CHUNK_RING_CAPACITY=$cap" "DFS_DELTA_RING_CAPACITY=$cap")
    else
        envs=(env)
    fi

    local server_pids=()
    for i in 1 2 3 4 5; do
        "${envs[@]}" nohup "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" --log-level info >> "$run_log/server${i}.log" 2>&1 &
        server_pids+=($!)
    done
    sleep 3

    nohup "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
        --log-file "$run_log/client.log" --allow-other --log-level info &
    sleep 2
    mountpoint -q "$MOUNT" || { echo "  MOUNT FAILED for capacity=$cap"; tail -30 "$run_log/client.log"; return 1; }

    IMG="$MOUNT/probe.img"
    dd if=/dev/zero of="$IMG" bs=1M count="$FILE_SIZE_MB" 2>/dev/null
    sync "$MOUNT"
    sleep 1

    # RSS sampler: every 2s, sum RSS (KB) across the 5 dfs-server procs for this run.
    (
        while true; do
            total_kb=0
            for pid in "${server_pids[@]}"; do
                rss=$(awk '/VmRSS/{print $2}' /proc/"$pid"/status 2>/dev/null)
                total_kb=$((total_kb + ${rss:-0}))
            done
            echo "$(date +%s.%N) ${total_kb}"
            sleep 2
        done
    ) > "$run_log/rss_samples.log" &
    local rss_sampler_pid=$!

    echo "  [capacity=$cap] running fio q32t1-equivalent randwrite, ${FILE_SIZE_MB}MB file, ${DURATION}s..."
    fio --name=probe --filename="$IMG" --rw=randwrite --bs=4k \
        --ioengine=sync --numjobs=32 --iodepth=1 --group_reporting \
        --time_based --runtime="${DURATION}" --size="${FILE_SIZE_MB}M" \
        --fsync=0 --direct=0 \
        --output="$run_log/fio.json" --output-format=json > "$run_log/fio.stdout" 2>&1

    kill "$rss_sampler_pid" 2>/dev/null
    sync "$MOUNT"
    sleep 2   # let the 30s stats-loop tick fire and flush before we grep it

    # Parse fio's aggregate bw/iops out of the json.
    local bw_kbs iops
    bw_kbs=$(python3 -c "import json; d=json.load(open('$run_log/fio.json')); j=d['jobs'][0]; print(j['write']['bw'])" 2>/dev/null || echo 0)
    iops=$(python3 -c "import json; d=json.load(open('$run_log/fio.json')); j=d['jobs'][0]; print(j['write']['iops'])" 2>/dev/null || echo 0)
    local mbps=$(echo "scale=3; $bw_kbs / 1024" | bc 2>/dev/null || echo "?")

    # chunk_ring and delta_ring stats — reported SEPARATELY, never combined.
    local cr_hits=0 cr_misses=0 dr_hits=0 dr_misses=0
    for i in 1 2 3 4 5; do
        sed -r 's/\x1b\[[0-9;]*m//g' "$run_log/server${i}.log" > "$run_log/server${i}_clean.log"
        local h m
        h=$(grep -oE '^.*chunk_ring stats.*' "$run_log/server${i}_clean.log" | grep -oE '[0-9]+ hits' | awk '{s+=$1} END{print s+0}')
        m=$(grep -oE '^.*chunk_ring stats.*' "$run_log/server${i}_clean.log" | grep -oE '[0-9]+ misses' | awk '{s+=$1} END{print s+0}')
        cr_hits=$((cr_hits + h)); cr_misses=$((cr_misses + m))
        h=$(grep -oE '^.*delta_ring stats.*' "$run_log/server${i}_clean.log" | grep -oE '[0-9]+ hits' | awk '{s+=$1} END{print s+0}')
        m=$(grep -oE '^.*delta_ring stats.*' "$run_log/server${i}_clean.log" | grep -oE '[0-9]+ misses' | awk '{s+=$1} END{print s+0}')
        dr_hits=$((dr_hits + h)); dr_misses=$((dr_misses + m))
    done
    local cr_total=$((cr_hits + cr_misses)) dr_total=$((dr_hits + dr_misses))
    local cr_pct="n/a" dr_pct="n/a"
    [ "$cr_total" -gt 0 ] && cr_pct=$(echo "scale=1; 100 * $cr_hits / $cr_total" | bc)
    [ "$dr_total" -gt 0 ] && dr_pct=$(echo "scale=1; 100 * $dr_hits / $dr_total" | bc)

    # Peak RSS across the run (sum of all 5 nodes), MB.
    local peak_rss_kb
    peak_rss_kb=$(awk '{print $2}' "$run_log/rss_samples.log" | sort -n | tail -1)
    local peak_rss_mb=$(echo "scale=1; ${peak_rss_kb:-0} / 1024" | bc)

    printf "capacity=%-8s %8.3f MB/s  %9.1f iops   chunk_ring: hits=%-6s misses=%-6s (%s%%)   delta_ring: hits=%-6s misses=%-6s (%s%%)   peak_RSS(5 nodes)=%s MB\n" \
        "$cap" "$mbps" "$iops" "$cr_hits" "$cr_misses" "$cr_pct" "$dr_hits" "$dr_misses" "$dr_pct" "$peak_rss_mb"

    cleanup_all
}

echo "=== ring capacity scale probe: duration=${DURATION}s file=${FILE_SIZE_MB}MB ($(( FILE_SIZE_MB / 4 )) chunks) capacities=${CAPACITIES[*]} ==="
mkdir -p "$LOG"
echo ""
for cap in "${CAPACITIES[@]}"; do
    run_one_capacity "$cap"
done
echo ""
echo "Per-run logs: $LOG/cap_<N>/  (fio.json, server*.log, rss_samples.log)"
