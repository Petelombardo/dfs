#!/bin/bash
# Probe: how much of the 256MB write collapse is fold-volume-driven (many
# lightly-patched, mostly-cold chunks each paying a full 4MB read+write+fsync
# fold) vs. real write-path cost? Compares DFS_ACTIVE_FOLD_BASE_SECS=6
# (current default — client.rs's jittered active_fold_interval, 6-10s since
# first patch on a slot) against a base long enough that the time-based
# trigger effectively never fires within the test window, so only whatever
# size/count triggers would naturally fire still do.
#
# Also works around a stats-reporting gap found while building this: server's
# start_chunk_ring_stats_loop ticks every 30s from SERVER STARTUP and swap()s
# (resets) the counters on each tick — a burst that lands between the last
# tick and script-driven shutdown is silently dropped, never reported. This
# script sleeps 32s after fio finishes (not the old script's 2s) to guarantee
# one more full tick fires and captures any end-of-run burst before cleanup.
#
# Usage: bash scripts/probe_fold_interval.sh [duration_secs] [file_size_mb] [base_secs...]
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-probe-fold
MOUNT=/tmp/dfs-probe-fold-mount
LOG=/tmp/dfs-probe-fold-logs
CLUSTER="127.0.0.1:8995,127.0.0.1:8996,127.0.0.1:8997,127.0.0.1:8998,127.0.0.1:8999"
BIN="$REPO/target/release"
DURATION="${1:-30}"
FILE_SIZE_MB="${2:-256}"
shift 2 2>/dev/null || shift $# 2>/dev/null || true
BASE_SECS_LIST=("$@")
if [ "${#BASE_SECS_LIST[@]}" -eq 0 ]; then
    BASE_SECS_LIST=(6 90)
fi

cleanup_all() {
    pkill -f "dfs-server.*dfs-probe-fold" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
    sleep 0.5
}

run_one() {
    local base_secs="$1"
    local run_log="$LOG/base_${base_secs}"
    rm -rf "$run_log"   # servers append (>>) to these logs — stale content from a
                        # prior run at a different file size would silently pollute
                        # this run's fold/ring-stats counts otherwise.
    mkdir -p "$run_log"

    cleanup_all
    rm -rf "$BASE" "$MOUNT"
    mkdir -p "$MOUNT" "$BASE"

    for i in 1 2 3 4 5; do
        NODE_DIR="$BASE/node${i}"
        PORT=$((8995 + i - 1))
        "$BIN/dfs-server" init --data-dir "$NODE_DIR/data" --meta-dir "$NODE_DIR/metadata" --config "$NODE_DIR/config.toml" >/dev/null
        sed -i "s/listen_addr = \"0.0.0.0:8900\"/listen_addr = \"127.0.0.1:${PORT}\"/" "$NODE_DIR/config.toml"
        if [ $i -gt 1 ]; then
            sed -i 's/seed_nodes = \[\]/seed_nodes = ["127.0.0.1:8995"]/' "$NODE_DIR/config.toml"
        fi
    done

    local server_pids=()
    for i in 1 2 3 4 5; do
        nohup "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" --log-level info >> "$run_log/server${i}.log" 2>&1 &
        server_pids+=($!)
    done
    sleep 3

    DFS_ACTIVE_FOLD_BASE_SECS="$base_secs" nohup "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
        --log-file "$run_log/client.log" --allow-other --log-level info &
    sleep 2
    mountpoint -q "$MOUNT" || { echo "  MOUNT FAILED for base_secs=$base_secs"; tail -30 "$run_log/client.log"; return 1; }

    IMG="$MOUNT/probe.img"
    dd if=/dev/zero of="$IMG" bs=1M count="$FILE_SIZE_MB" 2>/dev/null
    sync "$MOUNT"
    sleep 1

    echo "  [base_secs=$base_secs] running fio q32t1-equivalent randwrite, ${FILE_SIZE_MB}MB file, ${DURATION}s..."
    fio --name=probe --filename="$IMG" --rw=randwrite --bs=4k \
        --ioengine=sync --numjobs=32 --iodepth=1 --group_reporting \
        --time_based --runtime="${DURATION}" --size="${FILE_SIZE_MB}M" \
        --fsync=0 --direct=0 \
        --output="$run_log/fio.json" --output-format=json > "$run_log/fio.stdout" 2>&1

    sync "$MOUNT"
    sleep 32   # guarantee a full 30s stats tick fires and captures any end-of-run fold burst

    local bw_kbs iops
    bw_kbs=$(python3 -c "import json; d=json.load(open('$run_log/fio.json')); j=d['jobs'][0]; print(j['write']['bw'])" 2>/dev/null || echo 0)
    iops=$(python3 -c "import json; d=json.load(open('$run_log/fio.json')); j=d['jobs'][0]; print(j['write']['iops'])" 2>/dev/null || echo 0)
    local mbps=$(echo "scale=3; $bw_kbs / 1024" | bc 2>/dev/null || echo "?")

    local cr_hits=0 cr_misses=0 dr_hits=0 dr_misses=0 fold_count=0
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
    sed -r 's/\x1b\[[0-9;]*m//g' "$run_log/client.log" > "$run_log/client_clean.log"
    fold_count=$(grep -c "ForceFold:.*folded" "$run_log/client_clean.log")

    local cr_total=$((cr_hits + cr_misses)) dr_total=$((dr_hits + dr_misses))
    local cr_pct="n/a" dr_pct="n/a"
    [ "$cr_total" -gt 0 ] && cr_pct=$(echo "scale=1; 100 * $cr_hits / $cr_total" | bc)
    [ "$dr_total" -gt 0 ] && dr_pct=$(echo "scale=1; 100 * $dr_hits / $dr_total" | bc)

    printf "base_secs=%-4s %8.3f MB/s  %9.1f iops   folds=%-5s  chunk_ring: hits=%-5s misses=%-5s (%s%%)   delta_ring: hits=%-5s misses=%-5s (%s%%)\n" \
        "$base_secs" "$mbps" "$iops" "$fold_count" "$cr_hits" "$cr_misses" "$cr_pct" "$dr_hits" "$dr_misses" "$dr_pct"

    cleanup_all
}

echo "=== fold-interval probe: duration=${DURATION}s file=${FILE_SIZE_MB}MB ($(( FILE_SIZE_MB / 4 )) chunks) base_secs=${BASE_SECS_LIST[*]} ==="
mkdir -p "$LOG"
echo ""
for b in "${BASE_SECS_LIST[@]}"; do
    run_one "$b"
done
echo ""
echo "Per-run logs: $LOG/base_<N>/  (fio.json, server*.log, client.log)"
