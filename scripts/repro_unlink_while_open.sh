#!/bin/bash
# Surgical repro for the specific vulnerable path confirmed by direct code reading:
#
#   1. unlink() while a file descriptor is still open elsewhere skips clearing
#      path_to_inode/metadata_cache/write_buffers (fuse_impl.rs:6932-6947,
#      "POSIX unlink-while-open" — deferred to release() on the old fd's close).
#   2. create() on that same path (O_CREAT, no O_EXCL) while those caches still
#      have a valid entry takes a SYNCHRONOUS fast path (fuse_impl.rs:5111-5130)
#      that just reopens the existing ino as-is — no server round-trip, no
#      fresh-identity handling, no check for "was this path logically replaced".
#
# Both happen within a single client process's lifetime — no restart needed.
# This is much faster to iterate than the server-restart trials.
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-repro2
MOUNT=/tmp/dfs-repro2-mount
LOG=/tmp/dfs-repro2-logs
CLUSTER="127.0.0.1:8910,127.0.0.1:8911,127.0.0.1:8912"
BIN="$REPO/target/release"
TRIALS="${1:-5}"

pkill -f "dfs-server.*dfs-repro2" 2>/dev/null || true
pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
sleep 0.5
fusermount -u "$MOUNT" 2>/dev/null || true
sudo rm -rf "$BASE" "$MOUNT" "$LOG" 2>/dev/null || rm -rf "$BASE" "$MOUNT" "$LOG" 2>/dev/null || true
mkdir -p "$MOUNT" "$LOG"

echo "=== Initializing 3-node cluster ==="
mkdir -p "$BASE"
for i in 1 2 3; do
    NODE_DIR="$BASE/node${i}"
    PORT=$((8910 + i - 1))
    "$BIN/dfs-server" init --data-dir "$NODE_DIR/data" --meta-dir "$NODE_DIR/metadata" --config "$NODE_DIR/config.toml" >/dev/null
    sed -i "s/listen_addr = \"0.0.0.0:8900\"/listen_addr = \"127.0.0.1:${PORT}\"/" "$NODE_DIR/config.toml"
    if [ $i -gt 1 ]; then
        sed -i 's/seed_nodes = \[\]/seed_nodes = ["127.0.0.1:8910"]/' "$NODE_DIR/config.toml"
    fi
    RUST_LOG=info "$BIN/dfs-server" start --config "$NODE_DIR/config.toml" >> "$LOG/server${i}.log" 2>&1 &
done
sleep 2

echo "=== Mounting client ==="
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
echo "Mounted."

PASS_COUNT=0
FAIL_COUNT=0

for trial in $(seq 1 "$TRIALS"); do
    FILE="$MOUNT/repro2_${trial}.mpg"
    : > "$LOG/client.log"

    DFS_MOUNT="$MOUNT" DFS_TRIAL="$trial" python3 - <<'PYEOF'
import os, sys
mount = os.environ['DFS_MOUNT']
trial = os.environ['DFS_TRIAL']
path = f"{mount}/repro2_{trial}.mpg"

# Step 1: open, write substantial OLD content, fsync — real, durably-committed
# chunk 0 with a real (large) size.
old_fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(old_fd, b'OLDFILE_MARKER_DO_NOT_KEEP\n')
os.write(old_fd, os.urandom(3 * 1024 * 1024))
os.fsync(old_fd)
print(f"[trial {trial}] old content written+fsynced (fd still open)", flush=True)

# Step 2: unlink while old_fd is STILL open — hits the deferred-cleanup branch.
os.unlink(path)
print(f"[trial {trial}] unlinked while open", flush=True)

# Step 3: immediately recreate the SAME path while old_fd is still open (not yet
# closed/released) — this is the race window.
new_fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(new_fd, b'NEWFILE_HEADER_MARKER_XYZ\n')
os.fsync(new_fd)
os.write(new_fd, os.urandom(1024 * 1024))
os.fsync(new_fd)
os.close(new_fd)
print(f"[trial {trial}] new file created+written+closed while old fd still open", flush=True)

# Step 4: now close the old fd, triggering release()'s deferred cleanup.
os.close(old_fd)
print(f"[trial {trial}] old fd closed", flush=True)
PYEOF

    sync "$MOUNT" 2>/dev/null || true
    sleep 0.3
    sync "$MOUNT" 2>/dev/null || true

    RESULT="FAIL"
    if dd if="$FILE" bs=1k count=12 2>/dev/null | strings | grep -q "NEWFILE_HEADER_MARKER_XYZ"; then
        RESULT="PASS"
        PASS_COUNT=$((PASS_COUNT+1))
    else
        FAIL_COUNT=$((FAIL_COUNT+1))
        cp "$LOG/client.log" "$LOG/trial${trial}_FAIL_client.log"
    fi
    OLD_LEAKED="no"
    dd if="$FILE" bs=1k count=12 2>/dev/null | strings | grep -q "OLDFILE_MARKER_DO_NOT_KEEP" && OLD_LEAKED="YES-CONTAMINATED"
    echo "Trial $trial: header_survives=$RESULT old_content_leaked=$OLD_LEAKED"
done

echo ""
echo "════════════════════════════════════════════"
echo "  unlink-while-open trials: $PASS_COUNT passed, $FAIL_COUNT FAILED"
echo "════════════════════════════════════════════"

fusermount -u "$MOUNT" 2>/dev/null || true
pkill -f "dfs-server.*dfs-repro2" 2>/dev/null || true
pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
[ "$FAIL_COUNT" -gt 0 ] && exit 1 || exit 0
