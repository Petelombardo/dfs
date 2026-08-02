#!/bin/bash
# Reproduce a suspected corruption trigger reported live (2026-08-01, VM-108/
# VM-100 on server4): whenever the dfs-client binary gets redeployed
# (systemd restart) while ANOTHER file elsewhere under the same mount is
# actively open and being written (a running VM's qcow2 disk), a DIFFERENT
# file that was untouched during the restart and gets opened/written for the
# first time AFTER the fresh client comes up ends up corrupted. Reported as
# reproducing "always" in the field. No error/EIO was found in the client log
# for the incident that prompted this repro, which is consistent with SILENT
# corruption (wrong bytes landing with no error path tripped) rather than the
# EIO-shaped bugs fixed earlier today (1f00d18, a7b5be0) -- this script exists
# to find out whether that's actually what's happening, in isolation, without
# needing real VMs.
#
# Sequence:
#   1. Start a 5-node local cluster + one dfs-client, mounted.
#   2. Open file A, start a background writer that holds A's fd open and
#      keeps issuing writes to it continuously (mirrors a running VM's qemu
#      process with aio=threads against its qcow2 file).
#   3. While A's writer is still running (fd held open, mid-write), restart
#      the dfs-client process at the same mountpoint -- same operational
#      action as redeploying the client binary.
#   4. Once the fresh client is up, create and write file B for the first
#      time (this VM was "offline" -- never touched -- during the restart).
#   5. Stop everything, restart a clean cluster+client, and read B back COLD
#      (no cache masking -- see T51c's pattern) to verify its bytes are
#      exactly what was written.
#
# Usage: bash scripts/repro_client_restart_while_busy_corruption.sh

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-restart-corruption-mount
LOG=/tmp/dfs-restart-corruption-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
FILE_A="$MOUNT/vm-a-busy.img"
FILE_B="$MOUNT/vm-b-fresh.img"
CHUNK_SIZE=$((4 * 1024 * 1024))
FILE_B_SIZE=$((16 * CHUNK_SIZE))   # 64MB, a handful of chunks

cleanup_all() {
    pkill -9 -f "target/release/dfs-server" 2>/dev/null || true
    pkill -9 -f "target/release/dfs-client" 2>/dev/null || true
    pkill -9 -f "busy_writer.py" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

start_cluster() {
    for i in 1 2 3 4 5; do
        RUST_LOG=debug "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
            --log-level debug >> "$LOG/server${i}.log" 2>&1 &
    done
    sleep 3
}

mount_client() {
    # Redirect explicitly -- without it, the backgrounded daemon inherits the
    # $(...) command substitution's pipe as fd 1 and never closes it, hanging
    # the calling shell forever even though the mount itself is fine. Hit
    # this as an intermittent hang in a later sibling repro script; it's
    # timing-dependent (whether dfs-client detaches from the inherited fd
    # before the 2s sleep elapses) which is why this script didn't always hang.
    env RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
        --log-file "$LOG/client.log" --allow-other --log-level debug \
        > /dev/null 2>&1 &
    sleep 2
    mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
    pgrep -f "dfs-client mount $MOUNT" | head -1
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$MOUNT" "$LOG" "$BASE" 2>/dev/null || true
mkdir -p "$MOUNT" "$LOG"

echo "=== Setting up + starting 5-node cluster ==="
cd "$REPO"
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null > /dev/null
start_cluster

CLIENT_PID=$(mount_client)
echo "Mounted. Client PID=$CLIENT_PID"

echo "=== Starting file A's busy writer (simulates a running VM, fd held open) ==="
cat > /tmp/dfs-restart-corruption-logs/busy_writer.py <<'PYEOF'
import os, sys, time, random
path = sys.argv[1]
size = 32 * 1024 * 1024
fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
os.ftruncate(fd, size)
random.seed(1234)
while True:
    off = random.randrange(0, size - 4096)
    os.pwrite(fd, bytes([0xAA]) * 4096, off)
    os.fsync(fd)
    time.sleep(0.05)
PYEOF
python3 /tmp/dfs-restart-corruption-logs/busy_writer.py "$FILE_A" &
WRITER_PID=$!
sleep 2
if ! kill -0 "$WRITER_PID" 2>/dev/null; then
    echo "Busy writer died immediately — aborting"; cleanup_all; exit 1
fi
echo "Busy writer running (PID=$WRITER_PID), file A fd held open and mid-write"

echo "=== Restarting dfs-client (busy writer keeps running throughout, fd stays open) ==="
# Mirror the real ExecStop/ExecStart sequence: try a graceful unmount first,
# fall back to killing the process if the mount is busy (exactly what "another
# file is open" would cause), then bring a fresh client up at the same path.
fusermount -u "$MOUNT" 2>/dev/null
if mountpoint -q "$MOUNT" 2>/dev/null; then
    echo "  graceful unmount failed (mount busy, as expected) — killing client process"
    kill "$CLIENT_PID" 2>/dev/null
    sleep 1
    kill -9 "$CLIENT_PID" 2>/dev/null
    fusermount -u -z "$MOUNT" 2>/dev/null || true
    sleep 1
fi
NEW_CLIENT_PID=$(mount_client)
echo "Fresh client mounted. New client PID=$NEW_CLIENT_PID"
echo "  busy writer still alive: $(kill -0 "$WRITER_PID" 2>/dev/null && echo yes || echo no)"

echo "=== Writing file B for the first time under the fresh client (was 'offline' during restart) ==="
python3 -c "
import hashlib
data = bytes([(i * 37 + 11) % 256 for i in range(4096)]) * ($FILE_B_SIZE // 4096)
with open('$FILE_B', 'wb') as f:
    f.write(data)
print('B written, sha256=' + hashlib.sha256(data).hexdigest())
"
EXPECTED_HASH=$(python3 -c "
data = bytes([(i * 37 + 11) % 256 for i in range(4096)]) * ($FILE_B_SIZE // 4096)
import hashlib
print(hashlib.sha256(data).hexdigest())
")
sync "$MOUNT" 2>/dev/null || true
sleep 1

echo "=== Stopping busy writer and both clients/servers ==="
kill -9 "$WRITER_PID" 2>/dev/null || true
kill "$NEW_CLIENT_PID" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -9 -f "target/release/dfs-server" 2>/dev/null || true
sleep 1

echo "=== Cold restart: fresh cluster processes (same data dirs) + fresh client, read B back ==="
start_cluster
CLIENT_PID2=$(mount_client)
ACTUAL_HASH=$(sha256sum "$FILE_B" | awk '{print $1}')
ACTUAL_SIZE=$(stat -c %s "$FILE_B")

echo ""
echo "Expected sha256: $EXPECTED_HASH"
echo "Actual   sha256: $ACTUAL_HASH"
echo "Expected size:   $FILE_B_SIZE"
echo "Actual   size:   $ACTUAL_SIZE"

RESULT=1
if [ "$ACTUAL_HASH" = "$EXPECTED_HASH" ] && [ "$ACTUAL_SIZE" = "$FILE_B_SIZE" ]; then
    echo "=== RESULT: PASS — file B intact after client restart while file A was busy ==="
    RESULT=0
else
    echo "=== RESULT: FAIL — file B corrupted/truncated (reproduced the reported bug) ==="
    RESULT=1
fi

kill "$CLIENT_PID2" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -9 -f "target/release/dfs-server" 2>/dev/null || true
echo "Done. Logs in $LOG/"
exit $RESULT
