#!/bin/bash
# Reproduce the ACTUAL reported field sequence more faithfully than either
# earlier repro script (2026-08-01, VM-108 on server4):
#
#   VM-108 was OFF (no fd open at all). VM-100 was ON with an ISO mounted
#   from the DFS. The dfs-client was restarted (redeploy) while VM-100's ISO
#   fd was open and VM-108 was untouched. VM-108 was then turned ON (first
#   open since the restart), used normally, then turned OFF (closed) --
#   then turned ON again, at which point it was found corrupt. It is NOT
#   confirmed whether a second dfs-client restart happened somewhere in that
#   window.
#
# Unlike repro_client_restart_while_busy_corruption.sh (different file,
# written only once, no close/reopen cycle) and
# repro_samefile_open_across_client_restart.sh (SAME file held open across
# the restart), this script:
#   1. Holds a BYSTANDER file open across the client restart (VM-100/ISO
#      stand-in) -- never touched again after that, just keeps the restart
#      "unclean" (busy mount -> forced kill), matching the real sequence.
#   2. Only AFTER the restart, opens the TARGET file (VM-108 stand-in) for
#      the first time, writes known bytes into it (also written to an
#      off-mount control file), fsyncs, and CLOSES it -- "turn VM-108 on,
#      then off".
#   3. Restarts the client a second time (covers the "not sure if I
#      restarted a second time" uncertainty in the field report).
#   4. Reopens the target file cold -- "turn VM-108 on again" -- and
#      compares its hash against the control.
#
# Usage: bash scripts/repro_bystander_open_target_offon.sh

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-bystander-mount
LOG=/tmp/dfs-bystander-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
BYSTANDER="$MOUNT/vm-100-iso.img"
TARGET="$MOUNT/vm-108-target.img"
CONTROL="$LOG/control.bin"
CONTROL_SIZE=$((12 * 1024 * 1024 + 333))   # deliberately not chunk-aligned

if pgrep -f "target/release/dfs-server" >/dev/null 2>&1 || pgrep -f "target/release/dfs-client" >/dev/null 2>&1; then
    echo "ABORT: dfs-server or dfs-client already running -- kill those first."
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
    pkill -9 -f "bystander_holder.py" 2>/dev/null || true
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
    # See repro_samefile_open_across_client_restart.sh for why this redirect
    # is required: without it, the backgrounded daemon inherits the $(...)
    # command substitution's pipe as fd 1 and never closes it, hanging the
    # calling shell forever even though the mount itself is fine.
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
        echo "  graceful unmount failed (mount busy, as expected) -- killing client process"
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

echo "=== Generating control bytes ($CONTROL_SIZE bytes) ==="
head -c "$CONTROL_SIZE" /dev/urandom > "$CONTROL"
CONTROL_HASH=$(sha256sum "$CONTROL" | awk '{print $1}')
echo "Control sha256: $CONTROL_HASH"

echo "=== Setting up + starting 5-node cluster ==="
cd "$REPO"
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null > /dev/null
start_cluster

CLIENT_PID=$(mount_client)
echo "Mounted. Client PID=$CLIENT_PID"

echo "=== Opening bystander file (VM-100/ISO stand-in), holding it open, VM-108 (target) untouched ==="
cat > "$LOG/bystander_holder.py" <<'PYEOF'
import os, sys, time
path, marker = sys.argv[1], sys.argv[2]
fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
os.write(fd, b"\x42" * (1024 * 1024))
os.fsync(fd)
print(f"BYSTANDER_OPEN fd={fd}", flush=True)
while not os.path.exists(marker):
    time.sleep(0.2)
try:
    os.close(fd)
    print("BYSTANDER_CLOSED ok", flush=True)
except OSError as e:
    print(f"BYSTANDER_CLOSE ERROR: {e}", flush=True)
PYEOF

BYSTANDER_MARKER="$LOG/bystander_close_now"
rm -f "$BYSTANDER_MARKER"
python3 "$LOG/bystander_holder.py" "$BYSTANDER" "$BYSTANDER_MARKER" > "$LOG/bystander.out" 2>&1 &
BYSTANDER_PID=$!

for i in $(seq 1 50); do
    grep -q "^BYSTANDER_OPEN" "$LOG/bystander.out" 2>/dev/null && break
    sleep 0.2
done
if ! grep -q "^BYSTANDER_OPEN" "$LOG/bystander.out" 2>/dev/null; then
    echo "Bystander never confirmed open -- aborting"; cat "$LOG/bystander.out"; cleanup_all; exit 1
fi
echo "Bystander fd open (VM-100/ISO stand-in). VM-108 stand-in ($TARGET) does not exist yet."

echo "=== Restarting dfs-client #1 (bystander fd still open, target untouched) ==="
CLIENT_PID2=$(restart_client "$CLIENT_PID")
echo "Fresh client mounted. PID=$CLIENT_PID2"
echo "  bystander process still alive: $(kill -0 "$BYSTANDER_PID" 2>/dev/null && echo yes || echo no)"

echo "=== 'Turning VM-108 on': first open+write of target file since the restart ==="
python3 -c "
with open('$CONTROL', 'rb') as f:
    data = f.read()
with open('$TARGET', 'wb') as f:
    f.write(data)
    f.flush()
    import os
    os.fsync(f.fileno())
print('target written')
"
echo "=== 'Turning VM-108 off': closed above (python 'with' closed the fd) ==="

echo "=== Restarting dfs-client #2 (covers the 'not sure if I restarted a second time' uncertainty) ==="
sleep 1
CLIENT_PID3=$(restart_client "$CLIENT_PID2")
echo "Second fresh client mounted. PID=$CLIENT_PID3"

echo "=== 'Turning VM-108 on again': reopen target cold, hash it ==="
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
    echo "=== RESULT: PASS -- target file intact through bystander-open restart + off/on cycle ==="
    RESULT=0
else
    echo "=== RESULT: FAIL -- target file corrupted/truncated (reproduced the reported bug) ==="
    RESULT=1
fi

touch "$BYSTANDER_MARKER"
sleep 1
kill "$CLIENT_PID3" 2>/dev/null || true
sleep 1
cleanup_all
echo "Done. Logs in $LOG/"
exit $RESULT
