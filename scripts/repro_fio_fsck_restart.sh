#!/bin/bash
# Repro attempt: format a raw disk image (living on the DFS mount) as ext4,
# loop-mount it, hammer it with fio (kdiskmark-style concurrent random I/O),
# cleanly unmount the loop device, wait, restart dfs-client, then fsck the
# image to check for filesystem-level corruption.
#
# Mirrors the real staging incident's shape (qcow2 disk + kdiskmark + client
# restart + guest-visible errors) without needing a VM: ext4-on-a-file +
# loop device stands in for the qcow2/guest-filesystem layer, on the same
# machine.
#
# Usage: bash scripts/repro_fio_fsck_restart.sh
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-repro-fsck
MOUNT=/tmp/dfs-repro-fsck-mount
LOG=/tmp/dfs-repro-fsck-logs
LOOPMNT=/tmp/dfs-repro-fsck-loopmnt
CLUSTER="127.0.0.1:8940,127.0.0.1:8941,127.0.0.1:8942,127.0.0.1:8943,127.0.0.1:8944"
BIN="$REPO/target/release"
IMG_SIZE_MB=400
IMG="$MOUNT/testdisk.img"

cleanup_all() {
    umount "$LOOPMNT" 2>/dev/null || true
    for ld in $(losetup -j "$IMG" 2>/dev/null | cut -d: -f1); do
        losetup -d "$ld" 2>/dev/null || true
    done
    pkill -f "dfs-server.*dfs-repro-fsck" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$BASE" "$MOUNT" "$LOG" "$LOOPMNT"
mkdir -p "$MOUNT" "$LOG" "$LOOPMNT" "$BASE"

echo "=== Initializing 5-node cluster ==="
for i in 1 2 3 4 5; do
    NODE_DIR="$BASE/node${i}"
    PORT=$((8940 + i - 1))
    "$BIN/dfs-server" init --data-dir "$NODE_DIR/data" --meta-dir "$NODE_DIR/metadata" --config "$NODE_DIR/config.toml" >/dev/null
    sed -i "s/listen_addr = \"0.0.0.0:8900\"/listen_addr = \"127.0.0.1:${PORT}\"/" "$NODE_DIR/config.toml"
    if [ $i -gt 1 ]; then
        sed -i 's/seed_nodes = \[\]/seed_nodes = ["127.0.0.1:8940"]/' "$NODE_DIR/config.toml"
    fi
done
for i in 1 2 3 4 5; do
    RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" >> "$LOG/server${i}.log" 2>&1 &
done
sleep 3

start_client() {
    RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
        --log-file "$LOG/client.log" --allow-other --log-level info &
    sleep 2
    mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
}

echo "=== Mounting client ==="
start_client
echo "Mounted."

echo "=== Creating and formatting ${IMG_SIZE_MB}MB raw disk ==="
truncate -s ${IMG_SIZE_MB}M "$IMG"
mkfs.ext4 -F -q "$IMG"
sync "$MOUNT"

echo "=== Loop-mounting and running fio (kdiskmark-style, ~90s) ==="
LOOPDEV=$(losetup -f --show "$IMG")
mount "$LOOPDEV" "$LOOPMNT"
fio --name=stress --filename="$LOOPMNT/testfile" --size=280M --rw=randrw --bs=4k \
    --iodepth=32 --numjobs=4 --runtime=90 --time_based --direct=0 --group_reporting \
    --fsync=32 > "$LOG/fio.log" 2>&1
echo "fio done — see $LOG/fio.log"
tail -15 "$LOG/fio.log"

echo "=== Cleanly unmounting loop device (flush ext4) ==="
sync "$LOOPMNT"
umount "$LOOPMNT"
losetup -d "$LOOPDEV"
sync "$MOUNT"

echo "=== Waiting 15s (matching reported repro conditions) ==="
sleep 15

echo "=== Restarting dfs-client ==="
pkill -f "dfs-client mount $MOUNT"
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 1
start_client
echo "Remounted."
sync "$MOUNT"
sleep 2

echo "=== Re-loop-mounting image (read-only check, not mounting the fs itself) ==="
LOOPDEV=$(losetup -f --show "$IMG")
e2fsck -fn "$LOOPDEV" > "$LOG/fsck.log" 2>&1
FSCK_EXIT=$?
cat "$LOG/fsck.log"
losetup -d "$LOOPDEV"

echo ""
echo "════════════════════════════════════════════"
if [ "$FSCK_EXIT" -eq 0 ]; then
    echo "  FSCK CLEAN (exit 0) — no repro this run"
else
    echo "  FSCK FOUND ERRORS (exit $FSCK_EXIT) — see $LOG/fsck.log"
fi
echo "════════════════════════════════════════════"

echo ""
echo "=== Cleanup ==="
cleanup_all

exit "$FSCK_EXIT"
