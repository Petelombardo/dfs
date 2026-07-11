#!/bin/bash
# Tight repro: mkfs a raw disk image living on the DFS mount, then fsck it
# IMMEDIATELY (no fio hammering, no client restart, no artificial delay
# beyond what mkfs/sync themselves take) — mirrors the user's simplest
# real-world complaint: attach a fresh disk to a VM, partition+format+mount,
# and an fsck run right after sometimes finds errors. Described as random/
# speed-dependent, not deterministic — run in a loop to catch it.
#
# Usage: bash scripts/repro_fsck_immediately_after_format.sh [iterations]
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-repro-fastfsck
MOUNT=/tmp/dfs-repro-fastfsck-mount
LOG=/tmp/dfs-repro-fastfsck-logs
LOOPMNT_UNUSED=/tmp/dfs-repro-fastfsck-loopmnt
CLUSTER="127.0.0.1:8950,127.0.0.1:8951,127.0.0.1:8952,127.0.0.1:8953,127.0.0.1:8954"
BIN="$REPO/target/release"
IMG_SIZE_MB=200
IMG="$MOUNT/fastfsck.img"
ITERATIONS="${1:-30}"

cleanup_all() {
    for ld in $(losetup -j "$IMG" 2>/dev/null | cut -d: -f1); do
        losetup -d "$ld" 2>/dev/null || true
    done
    pkill -f "dfs-server.*dfs-repro-fastfsck" 2>/dev/null || true
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
    PORT=$((8950 + i - 1))
    "$BIN/dfs-server" init --data-dir "$NODE_DIR/data" --meta-dir "$NODE_DIR/metadata" --config "$NODE_DIR/config.toml" >/dev/null
    sed -i "s/listen_addr = \"0.0.0.0:8900\"/listen_addr = \"127.0.0.1:${PORT}\"/" "$NODE_DIR/config.toml"
    if [ $i -gt 1 ]; then
        sed -i 's/seed_nodes = \[\]/seed_nodes = ["127.0.0.1:8950"]/' "$NODE_DIR/config.toml"
    fi
done
for i in 1 2 3 4 5; do
    RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" >> "$LOG/server${i}.log" 2>&1 &
done
sleep 3

RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level info &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
echo "Mounted."

FAIL_COUNT=0
for i in $(seq 1 "$ITERATIONS"); do
    rm -f "$IMG"
    truncate -s ${IMG_SIZE_MB}M "$IMG"

    # Format, then check as fast as possible — no unmount/remount, no
    # artificial delay, mirroring "format then immediately fsck".
    T0=$(date +%s.%N)
    mkfs.ext4 -F -q "$IMG"
    T1=$(date +%s.%N)

    LOOPDEV=$(losetup -f --show "$IMG")
    e2fsck -fn "$LOOPDEV" > "$LOG/fsck_iter${i}.log" 2>&1
    FSCK_EXIT=$?
    T2=$(date +%s.%N)
    losetup -d "$LOOPDEV"

    MKFS_MS=$(echo "($T1 - $T0) * 1000" | bc)
    FSCK_MS=$(echo "($T2 - $T1) * 1000" | bc)

    if [ "$FSCK_EXIT" -eq 0 ]; then
        echo "  [$i/$ITERATIONS] clean (mkfs ${MKFS_MS%.*}ms, fsck ${FSCK_MS%.*}ms)"
    else
        FAIL_COUNT=$((FAIL_COUNT+1))
        echo "  [$i/$ITERATIONS] *** FSCK FOUND ERRORS *** (exit $FSCK_EXIT, mkfs ${MKFS_MS%.*}ms, fsck ${FSCK_MS%.*}ms) — see $LOG/fsck_iter${i}.log"
        cat "$LOG/fsck_iter${i}.log"
    fi
done

echo ""
echo "════════════════════════════════════════════"
echo "  $FAIL_COUNT / $ITERATIONS iterations found fsck errors"
echo "════════════════════════════════════════════"

cleanup_all
[ "$FAIL_COUNT" -eq 0 ] && exit 0 || exit 1
