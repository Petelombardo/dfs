#!/bin/bash
# Repro for the 2026-08-08/09 VM-108 restore corruption: qemu-img check found
# refcount errors in the qcow2 header/L1/refcount region (clusters 0/1/3) right
# after a Proxmox restore (preallocation=metadata, pbs-restore --skip-zero),
# but the SAME backup restored a second time and booted clean when the user
# waited longer than the client's active-fold interval before reading.
#
# This mirrors that A/B: write a qcow2 with preallocation=metadata (the exact
# jump-ahead L1/L2-table write pattern that makes DFS chunk 0 a "hot chunk"
# per client.rs's active-fold machinery, see ACTIVE_FOLD_MIN_INTERVAL's doc
# comment), close it, then `qemu-img check` it either IMMEDIATELY or after a
# delay well past the fold timer (jittered 6-10s + up to 5s cooldown, so 20s
# is a safe margin). Runs both variants many times to see whether "immediate"
# fails more often than "delayed" — that differential is the reproduction.
#
# Usage: ./scripts/repro_fold_timing_boot_after_restore.sh [iterations] [img_size_mb]

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-repro-foldboot
MOUNT=/tmp/dfs-repro-foldboot-mount
LOG=/tmp/dfs-repro-foldboot-logs
CLUSTER="127.0.0.1:8970,127.0.0.1:8971,127.0.0.1:8972,127.0.0.1:8973,127.0.0.1:8974"
BIN="$REPO/target/release"
IMG_SIZE_MB="${2:-512}"
ITERATIONS="${1:-5}"
SRC="$BASE/sparse_source.raw"
DELAY_SECS=20
# Dev box has no swap and a small root disk (feedback_dev_box_no_swap_repro_lockup /
# feedback_dev_box_disk_lockup_from_unthrottled_repro_writers) — abort rather than
# risk a lockup. RF=3 replication means each iteration's image costs ~3x its size in
# chunk storage across the 5 local nodes on top of the source file itself.
MIN_FREE_KB=$((3 * 1024 * 1024))  # 3GB

check_disk_guard() {
    local avail_kb
    avail_kb=$(df -k / | awk 'NR==2 {print $4}')
    if [ "$avail_kb" -lt "$MIN_FREE_KB" ]; then
        echo "DISK GUARD: free space ${avail_kb}KB < ${MIN_FREE_KB}KB — aborting early to avoid a lockup"
        cleanup_all
        exit 2
    fi
}

cleanup_all() {
    pkill -f "dfs-server.*dfs-repro-foldboot" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$BASE" "$MOUNT" "$LOG"
mkdir -p "$MOUNT" "$LOG" "$BASE"

echo "=== Initializing 5-node cluster ==="
for i in 1 2 3 4 5; do
    NODE_DIR="$BASE/node${i}"
    PORT=$((8970 + i - 1))
    "$BIN/dfs-server" init --data-dir "$NODE_DIR/data" --meta-dir "$NODE_DIR/metadata" --config "$NODE_DIR/config.toml" >/dev/null
    sed -i "s/listen_addr = \"0.0.0.0:8900\"/listen_addr = \"127.0.0.1:${PORT}\"/" "$NODE_DIR/config.toml"
    if [ $i -gt 1 ]; then
        sed -i 's/seed_nodes = \[\]/seed_nodes = ["127.0.0.1:8970"]/' "$NODE_DIR/config.toml"
    fi
done
for i in 1 2 3 4 5; do
    RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" >> "$LOG/server${i}.log" 2>&1 &
done
sleep 3

RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
echo "Mounted."

echo "=== Building a synthetic sparse source (~45% non-zero, scattered bursts) ==="
python3 -c "
import os, random
random.seed(42)
size = ${IMG_SIZE_MB} * 1024 * 1024
with open('$SRC', 'wb') as f:
    f.truncate(size)
    f.seek(0)
burst = 64 * 1024
pos = 0
with open('$SRC', 'r+b') as f:
    while pos < size:
        if random.random() < 0.45:
            f.seek(pos)
            f.write(os.urandom(burst))
        pos += burst
"
echo "Source built: $(du -h "$SRC" | cut -f1) apparent, $(du -h --apparent-size "$SRC" | cut -f1) size"

# Background contention, like the real incident's concurrent-file load profile
# (repro_fold_stale_base_under_load.sh's rationale) — two small, throttled,
# bounded-size writers to SEPARATE files, contending for the same server-side
# fold_concurrency permits / chunk_patch_locks / list_chunks paths that the
# main convert is also using, to widen whatever race window exists instead of
# relying purely on this dev box happening to be slow at the right moment.
NOISE_PIDS=()
start_noise() {
    local n="$1"
    for tag in noiseA noiseB; do
        python3 -c "
import os, time
path = '$MOUNT/${tag}_${n}.dat'
fd = os.open(path, os.O_CREAT | os.O_RDWR, 0o644)
chunk = os.urandom(65536)
end = time.time() + 60
written = 0
cap = 40 * 1024 * 1024
while time.time() < end and written < cap:
    os.pwrite(fd, chunk, written)
    written += len(chunk)
    time.sleep(0.02)
os.close(fd)
" > "$LOG/${tag}_iter${n}.log" 2>&1 &
        NOISE_PIDS+=($!)
    done
}
stop_noise() {
    # NOTE: must wait on each noise PID individually — a bare `wait` blocks on
    # EVERY background job of this script, including the long-running
    # dfs-server/dfs-client processes, not just these two noise writers.
    for p in "${NOISE_PIDS[@]}"; do
        kill "$p" 2>/dev/null
        wait "$p" 2>/dev/null
    done
    NOISE_PIDS=()
}

IMMEDIATE_FAIL=0
DELAYED_FAIL=0

for i in $(seq 1 "$ITERATIONS"); do
    check_disk_guard
    IMG="$MOUNT/foldboot_${i}.qcow2"
    rm -f "$IMG"
    start_noise "$i"

    T0=$(date +%s.%N)
    qemu-img convert -p -m 4 -O qcow2 -o preallocation=metadata,cluster_size=65536 "$SRC" "$IMG" > "$LOG/convert_iter${i}.log" 2>&1
    CONVERT_RC=$?
    T1=$(date +%s.%N)
    if [ "$CONVERT_RC" -ne 0 ]; then
        echo "  [$i/$ITERATIONS] qemu-img convert FAILED (rc=$CONVERT_RC) — see $LOG/convert_iter${i}.log"
        cat "$LOG/convert_iter${i}.log"
        stop_noise
        rm -f "$MOUNT"/noise*_"${i}".dat
        continue
    fi
    CONVERT_MS=$(echo "($T1 - $T0) * 1000" | bc)

    # IMMEDIATE check — no delay beyond what convert's own close() does,
    # mirroring "qm start right after restore finished".
    qemu-img check "$IMG" > "$LOG/check_immediate_iter${i}.log" 2>&1
    IMM_RC=$?
    T2=$(date +%s.%N)
    stop_noise
    rm -f "$MOUNT"/noise*_"${i}".dat

    if [ "$IMM_RC" -ne 0 ]; then
        IMMEDIATE_FAIL=$((IMMEDIATE_FAIL+1))
        echo "  [$i/$ITERATIONS] convert ${CONVERT_MS%.*}ms — *** IMMEDIATE CHECK FAILED *** (see $LOG/check_immediate_iter${i}.log)"
        grep -E "ERROR|Image end offset" "$LOG/check_immediate_iter${i}.log"
    else
        echo "  [$i/$ITERATIONS] convert ${CONVERT_MS%.*}ms — immediate check: clean"
    fi

    # DELAYED check, same file, after waiting past the active-fold timer.
    sleep "$DELAY_SECS"
    qemu-img check "$IMG" > "$LOG/check_delayed_iter${i}.log" 2>&1
    DEL_RC=$?

    if [ "$DEL_RC" -ne 0 ]; then
        DELAYED_FAIL=$((DELAYED_FAIL+1))
        echo "               delayed (+${DELAY_SECS}s) check: *** FAILED *** (see $LOG/check_delayed_iter${i}.log)"
        grep -E "ERROR|Image end offset" "$LOG/check_delayed_iter${i}.log"
    else
        echo "               delayed (+${DELAY_SECS}s) check: clean"
    fi

    rm -f "$IMG"
done

echo ""
echo "════════════════════════════════════════════════════════════════"
echo "  Immediate-check failures: $IMMEDIATE_FAIL / $ITERATIONS"
echo "  Delayed-check failures:   $DELAYED_FAIL / $ITERATIONS"
echo "════════════════════════════════════════════════════════════════"

cleanup_all
[ "$IMMEDIATE_FAIL" -eq 0 ] && [ "$DELAYED_FAIL" -eq 0 ] && exit 0 || exit 1
