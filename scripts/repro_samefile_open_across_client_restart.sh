#!/bin/bash
# Reproduce a suspected corruption trigger reported live (2026-08-01, VM-108 on
# server4): a running VM's disk file is held open read-write while the
# dfs-client binary gets redeployed (systemd restart) underneath it. This
# script tests the SAME file that was open across the restart (unlike
# repro_client_restart_while_busy_corruption.sh, which tests a DIFFERENT file
# written only after the restart) -- i.e. does the restart itself corrupt the
# bytes of the file that was open when it happened.
#
# Sequence (as specified by the user):
#   1. Start a 5-node local cluster + one dfs-client, mounted.
#   2. Open a target file RW on the mount, copy bytes into it from a known
#      on-disk control file, fsync. Hash the control file now (the "control").
#   3. Keep the target file's fd open RW.
#   4. Restart the dfs-client process (same mountpoint) while that fd is still
#      open -- mirrors a binary redeploy while a VM is running.
#   5. Close the fd (best-effort -- the old fd may error against the now-dead
#      FUSE connection; that's recorded but not treated as failure on its own).
#   6. Restart the dfs-client a second time, purely to guarantee a cold read
#      with no client-side cache masking (same rationale as T51c / the other
#      repro script's cold-read step).
#   7. Read the target file back, hash it, compare against the control hash.
#
# Usage: bash scripts/repro_samefile_open_across_client_restart.sh

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-samefile-mount
LOG=/tmp/dfs-samefile-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
TARGET="$MOUNT/vm-target.img"
CONTROL="$LOG/control.bin"
CONTROL_SIZE=$((18 * 1024 * 1024 + 777))   # deliberately not chunk-aligned

# Guard: refuse to run if anything else is already using our ports/processes,
# since two earlier repro attempts were contaminated by exactly this.
if pgrep -f "target/release/dfs-server" >/dev/null 2>&1 || pgrep -f "target/release/dfs-client" >/dev/null 2>&1; then
    echo "ABORT: dfs-server or dfs-client already running -- kill those first (not doing it for you, might be someone else's run)."
    pgrep -af "target/release/dfs-server|target/release/dfs-client"
    exit 2
fi
if ss -ltn 2>/dev/null | grep -qE ":(8900|8901|8902|8903|8904)\s"; then
    echo "ABORT: ports 8900-8904 already in use."
    exit 2
fi

cleanup_all() {
    pkill -9 -f "target/release/dfs-server" 2>/dev/null || true
    pkill -9 -f "target/release/dfs-client" 2>/dev/null || true
    pkill -9 -f "hold_open_writer.py" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || fusermount -u -z "$MOUNT" 2>/dev/null || true
}

start_cluster() {
    for i in 1 2 3 4 5; do
        RUST_LOG=debug "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
            --log-level debug >> "$LOG/server${i}.log" 2>&1 &
    done
    sleep 3
}

mount_client() {
    # Redirect the client's own stdout/stderr to /dev/null explicitly -- it
    # already logs everything meaningful via --log-file. Without this, the
    # backgrounded long-running daemon inherits whatever fd 1 happens to be
    # at the call site (here, the pipe used by a $(...) command substitution)
    # and never closes it, so the substitution blocks forever waiting for an
    # EOF that will never come even though the mount itself is healthy.
    env RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
        --log-file "$LOG/client.log" --allow-other --log-level debug \
        > /dev/null 2>&1 &
    sleep 2
    mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
    pgrep -f "dfs-client mount $MOUNT" | head -1
}

restart_client() {
    local old_pid="$1"
    fusermount -u "$MOUNT" 2>/dev/null
    if mountpoint -q "$MOUNT" 2>/dev/null; then
        echo "  graceful unmount failed (mount busy, as expected -- target fd still open) -- killing client process"
        kill "$old_pid" 2>/dev/null
        sleep 1
        kill -9 "$old_pid" 2>/dev/null
        fusermount -u -z "$MOUNT" 2>/dev/null || true
        sleep 1
    fi
    mount_client
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$MOUNT" "$LOG" "$BASE" 2>/dev/null || true
mkdir -p "$MOUNT" "$LOG"

echo "=== Generating control file ($CONTROL_SIZE bytes) ==="
head -c "$CONTROL_SIZE" /dev/urandom > "$CONTROL"
CONTROL_HASH=$(sha256sum "$CONTROL" | awk '{print $1}')
echo "Control sha256: $CONTROL_HASH"

echo "=== Setting up + starting 5-node cluster ==="
cd "$REPO"
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null > /dev/null
start_cluster

CLIENT_PID=$(mount_client)
echo "Mounted. Client PID=$CLIENT_PID"

echo "=== Opening target file RW, copying control bytes in, fsync, holding fd open ==="
cat > "$LOG/hold_open_writer.py" <<'PYEOF'
import os, sys, time

target, control, marker = sys.argv[1], sys.argv[2], sys.argv[3]

fd = os.open(target, os.O_RDWR | os.O_CREAT, 0o644)
with open(control, 'rb') as f:
    data = f.read()
os.write(fd, data)
os.fsync(fd)
print(f"WRITTEN fd={fd} bytes={len(data)}", flush=True)

# Hold the fd open until told to close.
while not os.path.exists(marker):
    time.sleep(0.2)

try:
    os.close(fd)
    print("CLOSED ok", flush=True)
except OSError as e:
    print(f"CLOSE ERROR: {e}", flush=True)
PYEOF

MARKER="$LOG/close_now"
rm -f "$MARKER"
python3 "$LOG/hold_open_writer.py" "$TARGET" "$CONTROL" "$MARKER" > "$LOG/writer.out" 2>&1 &
WRITER_PID=$!

# Wait for the writer to confirm the write landed before we restart under it.
for i in $(seq 1 50); do
    grep -q "^WRITTEN" "$LOG/writer.out" 2>/dev/null && break
    sleep 0.2
done
if ! grep -q "^WRITTEN" "$LOG/writer.out" 2>/dev/null; then
    echo "Writer never confirmed write -- aborting"; cat "$LOG/writer.out"; cleanup_all; exit 1
fi
cat "$LOG/writer.out"
echo "Writer PID=$WRITER_PID, fd held open RW."

echo "=== Restarting dfs-client while target fd is still open ==="
NEW_CLIENT_PID=$(restart_client "$CLIENT_PID")
echo "Fresh client mounted. New client PID=$NEW_CLIENT_PID"
echo "  writer process still alive: $(kill -0 "$WRITER_PID" 2>/dev/null && echo yes || echo no)"

echo "=== Closing the target fd (best-effort against the old, now-dead FUSE session) ==="
touch "$MARKER"
for i in $(seq 1 25); do
    kill -0 "$WRITER_PID" 2>/dev/null || break
    sleep 0.2
done
if kill -0 "$WRITER_PID" 2>/dev/null; then
    echo "  writer did not exit on its own after close attempt -- killing it"
    kill -9 "$WRITER_PID" 2>/dev/null
fi
cat "$LOG/writer.out"

echo "=== Restarting dfs-client a second time (force cold state, no cache masking) ==="
sleep 1
NEW_CLIENT_PID2=$(restart_client "$NEW_CLIENT_PID")
echo "Second fresh client mounted. PID=$NEW_CLIENT_PID2"

echo "=== Reading target back cold and comparing hashes ==="
sync "$MOUNT" 2>/dev/null || true
ACTUAL_HASH=$(sha256sum "$TARGET" 2>/dev/null | awk '{print $1}')
ACTUAL_SIZE=$(stat -c %s "$TARGET" 2>/dev/null)

echo ""
echo "Expected sha256: $CONTROL_HASH"
echo "Actual   sha256: $ACTUAL_HASH"
echo "Expected size:   $CONTROL_SIZE"
echo "Actual   size:   $ACTUAL_SIZE"

RESULT=1
if [ "$ACTUAL_HASH" = "$CONTROL_HASH" ] && [ "$ACTUAL_SIZE" = "$CONTROL_SIZE" ]; then
    echo "=== RESULT: PASS -- target file intact after being held open across a client restart ==="
    RESULT=0
else
    echo "=== RESULT: FAIL -- target file corrupted/truncated by the restart-while-open sequence ==="
    RESULT=1
fi

kill "$NEW_CLIENT_PID2" 2>/dev/null || true
sleep 1
cleanup_all
echo "Done. Logs in $LOG/"
exit $RESULT
