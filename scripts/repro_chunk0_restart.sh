#!/bin/bash
# Standalone repro attempt for the staging chunk-0 header corruption bug.
#
# Hypothesis under test (per user): a *continuous* write session (no delete/recreate,
# no client restart) whose chunk-0 header write/flush happens close in time to a
# SERVER-side rolling restart of its replicas — exactly what `deploy-build.sh server`
# does while the DVR container / recordings keep running (that mode deliberately does
# NOT stop the client or containers first, unlike `all` mode). This mirrors tonight's
# actual incident: the healing/bandwidth fix was rolled out via a server-only rolling
# restart while 3-4 shows were actively recording.
#
# Not part of test_local_suite.sh (kept standalone per user's suggestion) since this
# is an exploratory repro attempt, run repeatedly with varied timing until it either
# reproduces or we've made a thorough case that it doesn't reproduce locally.
#
# Usage: bash scripts/repro_chunk0_restart.sh [num_trials]
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-repro
MOUNT=/tmp/dfs-repro-mount
LOG=/tmp/dfs-repro-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
TRIALS="${1:-5}"

cleanup_all() {
    pkill -f "dfs-server.*dfs-repro" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

echo "=== Cleaning up any previous run ==="
cleanup_all
sudo rm -rf "$BASE" "$MOUNT" "$LOG" 2>/dev/null || rm -rf "$BASE" "$MOUNT" "$LOG" 2>/dev/null || true
mkdir -p "$MOUNT" "$LOG"

echo "=== Building ==="
cd "$REPO" && cargo build --release 2>&1 | tail -3

echo "=== Initializing 5-node cluster at $BASE ==="
mkdir -p "$BASE"
for i in 1 2 3 4 5; do
    NODE_DIR="$BASE/node${i}"
    PORT=$((8900 + i - 1))
    "$BIN/dfs-server" init --data-dir "$NODE_DIR/data" --meta-dir "$NODE_DIR/metadata" --config "$NODE_DIR/config.toml" >/dev/null
    sed -i "s/listen_addr = \"0.0.0.0:8900\"/listen_addr = \"127.0.0.1:${PORT}\"/" "$NODE_DIR/config.toml"
    if [ $i -gt 1 ]; then
        sed -i 's/seed_nodes = \[\]/seed_nodes = ["127.0.0.1:8900"]/' "$NODE_DIR/config.toml"
    fi
    sed -i 's/heartbeat_interval_secs = .*/heartbeat_interval_secs = 3/' "$NODE_DIR/config.toml"
    sed -i 's/failure_timeout_secs = .*/failure_timeout_secs = 120/' "$NODE_DIR/config.toml"
    sed -i 's/healing_delay_secs = .*/healing_delay_secs = 5/' "$NODE_DIR/config.toml"
done

start_node() {
    local i="$1"
    RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        >> "$LOG/server${i}.log" 2>&1 &
    eval "SERVER_PID_${i}=$!"
}

echo "=== Starting all 5 nodes ==="
for i in 1 2 3 4 5; do start_node $i; done
sleep 3

echo "=== Mounting client ==="
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
CLIENT_PID=$!
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
echo "Mounted at $MOUNT."

dfs_sync() { mountpoint -q "$MOUNT" 2>/dev/null && sync "$MOUNT" || true; }

# wait_port_up <port>: poll until something is listening on 127.0.0.1:<port>,
# up to ~5s. Replaces a fixed sleep so a slow-to-rebind node doesn't get treated
# as "up" before it can actually accept connections (or worse, doesn't leave a
# stale bind in progress when we move to the next node).
wait_port_up() {
    local port="$1"
    for _ in $(seq 1 25); do
        (exec 3<>/dev/tcp/127.0.0.1/$port) 2>/dev/null && { exec 3<&- 3>&-; return 0; }
        sleep 0.2
    done
    return 1
}

# rolling_restart_servers: stop+start each of the 5 nodes in turn, mirroring
# deploy-build.sh's server-rolling-update loop but compressed since this is a
# local, near-idle cluster. Waits for each node to actually accept connections
# again before moving to the next, instead of a fixed sleep, so a slow rebind
# doesn't cascade into pkill hitting the wrong/next instance.
rolling_restart_servers() {
    for i in 1 2 3 4 5; do
        local port=$((8900 + i - 1))
        pkill -f "dfs-server start --config $BASE/node${i}/config.toml" 2>/dev/null || true
        sleep 0.5
        start_node $i
        if ! wait_port_up "$port"; then
            echo "  WARNING: node $i did not come back up on port $port within 5s"
        fi
        sleep 0.3
    done
}

PASS_COUNT=0
FAIL_COUNT=0

for trial in $(seq 1 "$TRIALS"); do
    echo ""
    echo "=== Trial $trial/$TRIALS ==="
    FILE="$MOUNT/repro_recording_${trial}.mpg"
    snapshot="$LOG/trial${trial}_client.log"
    cp "$LOG/client.log" "$snapshot" 2>/dev/null || true
    : > "$LOG/client.log" 2>/dev/null || true

    # Fire the rolling restart FIRST (backgrounded), then launch the writer with no
    # delay — we want the header write+fsync (which completes in low-single-digit
    # milliseconds on loopback) to actually land while nodes are mid-restart, not
    # 0.5s+ after they've already quietly recovered. Two rapid passes over all 5
    # nodes to maximize the chance some restart overlaps the header's own flush.
    ( rolling_restart_servers; rolling_restart_servers ) &
    RESTART_PID=$!

    # Writer: opens the file, writes a distinctive header first (mirrors HDHomeRun's
    # "StartTime" text header), then streams ~188KB every 30ms with an fsync after
    # nearly every write for ~10s — a single continuous open/write session (no
    # delete, no re-open, no client restart), front-loaded with fsyncs specifically
    # during the restart storm above.
    DFS_MOUNT="$MOUNT" DFS_TRIAL="$trial" python3 - <<'PYEOF' &
import os, time, sys
mount = os.environ['DFS_MOUNT']
trial = os.environ['DFS_TRIAL']
path = f"{mount}/repro_recording_{trial}.mpg"
header = f"StartTime: repro-trial-{trial}\n".encode()
chunk = 188 * 1024
total_duration = 10.0
interval = 0.03

f = open(path, 'wb')
f.write(header)
f.flush()
os.fsync(f.fileno())
print(f"[writer {trial}] header written and fsynced", flush=True)

data = os.urandom(chunk)
start = time.time()
n = 0
errors = 0
while time.time() - start < total_duration:
    try:
        f.write(data)
        n += 1
        f.flush()
        os.fsync(f.fileno())
    except OSError as e:
        errors += 1
        print(f"[writer {trial}] write error #{errors}: {e}", flush=True)
        time.sleep(0.2)
    time.sleep(interval)

try:
    f.flush()
    os.fsync(f.fileno())
except OSError as e:
    print(f"[writer {trial}] final fsync error: {e}", flush=True)
f.close()
print(f"[writer {trial}] done: {n} writes, {errors} errors", flush=True)
PYEOF
    WRITER_PID=$!

    wait "$WRITER_PID"
    wait "$RESTART_PID" 2>/dev/null || true
    dfs_sync
    sleep 1
    dfs_sync

    RESULT="FAIL"
    if dd if="$FILE" bs=1k count=12 2>/dev/null | strings | grep -qi "StartTime"; then
        RESULT="PASS"
        PASS_COUNT=$((PASS_COUNT+1))
    else
        FAIL_COUNT=$((FAIL_COUNT+1))
    fi
    echo "  Trial $trial chunk-0 header check: $RESULT"
    if [ "$RESULT" = "FAIL" ]; then
        echo "  --- first 200 bytes of $FILE (hex) ---"
        dd if="$FILE" bs=1 count=200 2>/dev/null | od -c | head -20
        cp "$LOG/client.log" "$LOG/trial${trial}_FAIL_client.log" 2>/dev/null || true
    fi
done

echo ""
echo "════════════════════════════════════════════"
echo "  Repro trials: $PASS_COUNT passed (header intact), $FAIL_COUNT FAILED (header lost/corrupted)"
echo "════════════════════════════════════════════"

echo ""
echo "=== Cleanup ==="
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -f "dfs-server.*$BASE" 2>/dev/null || true
pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true

[ "$FAIL_COUNT" -gt 0 ] && exit 1 || exit 0
