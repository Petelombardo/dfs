#!/bin/bash
# Probe: what happens when the client-facing connection cap (network::MAX_CONNECTIONS,
# normally 512) is slammed down to something absurd like 2, via the new
# DFS_MAX_CONNECTIONS env override, and then hit with fio concurrency well above
# that cap?
#
# This isn't chasing a known bug — it's a deliberate stress probe. The
# connection-pressure watchdog (Server::start_conn_pressure_watchdog) and the
# accept-loop busy-rejection path (network.rs) are both designed to handle
# exhaustion gracefully (ServerBusy replies, WARN logs, leadership step-down
# after 30s, restart after 5min) but that logic normally only exercises under
# a real ~512-connection peak. Forcing exhaustion at cap=2 lets us hit those
# same code paths on a 3-node cluster with a small, cheap fio run instead of
# waiting for a real load spike, and check: does the client retry/back off
# cleanly, or does something (a stuck write, a torn read) turn connection
# pressure into actual data corruption? fio's own verify=crc32c catches the
# latter directly.
#
# Usage: bash scripts/probe_low_connection_cap.sh [runtime_secs] [max_connections]
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-probe-lowconn
MOUNT=/tmp/dfs-probe-lowconn-mount
LOG=/tmp/dfs-probe-lowconn-logs
CLUSTER="127.0.0.1:8950,127.0.0.1:8951,127.0.0.1:8952"
BIN="$REPO/target/release"
RUNTIME="${1:-90}"
CAP="${2:-2}"

cleanup_all() {
    pkill -f "dfs-server.*dfs-probe-lowconn" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$BASE" "$MOUNT" "$LOG"
mkdir -p "$MOUNT" "$LOG" "$BASE"

echo "=== Initializing 3-node cluster ==="
for i in 1 2 3; do
    NODE_DIR="$BASE/node${i}"
    PORT=$((8950 + i - 1))
    "$BIN/dfs-server" init --data-dir "$NODE_DIR/data" --meta-dir "$NODE_DIR/metadata" --config "$NODE_DIR/config.toml" >/dev/null
    sed -i "s/listen_addr = \"0.0.0.0:8900\"/listen_addr = \"127.0.0.1:${PORT}\"/" "$NODE_DIR/config.toml"
    if [ $i -gt 1 ]; then
        sed -i 's/seed_nodes = \[\]/seed_nodes = ["127.0.0.1:8950"]/' "$NODE_DIR/config.toml"
    fi
done

echo "=== Starting nodes with DFS_MAX_CONNECTIONS=${CAP} (client-facing cap only — peer pool stays at its normal size so fold/heal traffic isn't the thing under test) ==="
for i in 1 2 3; do
    DFS_MAX_CONNECTIONS="$CAP" RUST_LOG=debug "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" >> "$LOG/server${i}.log" 2>&1 &
done
sleep 3

echo "=== Mounting client (debug level, matching test-suite convention) ==="
RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
echo "Mounted."

echo "=== Confirming the cap actually took effect (dfs-admin stats) ==="
"$BIN/dfs-admin" --cluster "$CLUSTER" stats 2>&1 | tee "$LOG/stats_before.log"

echo "=== Polling connection stats in the background during the run ==="
(
    while true; do
        echo "--- $(date +%H:%M:%S) ---"
        "$BIN/dfs-admin" --cluster "$CLUSTER" stats 2>&1
        sleep 5
    done
) > "$LOG/stats_poll.log" 2>&1 &
POLL_PID=$!

echo "=== Running fio: 8 jobs (>> cap of ${CAP}) for ${RUNTIME}s, randwrite + crc32c verify ==="
fio --name=lowconn --directory="$MOUNT" --size=24m --numjobs=8 --rw=randwrite --bs=4k \
    --ioengine=psync --direct=0 --runtime="$RUNTIME" --time_based \
    --verify=crc32c --verify_fatal=1 --group_reporting \
    > "$LOG/fio.log" 2>&1
FIO_EXIT=$?
kill "$POLL_PID" 2>/dev/null || true
echo "fio exit=$FIO_EXIT — see $LOG/fio.log"
tail -20 "$LOG/fio.log"

echo "=== sync + unmount/remount to force a cold re-read (catch anything cache is hiding) ==="
sync "$MOUNT"
pkill -f "dfs-client mount $MOUNT"
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 1
RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client_remount.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "REMOUNT FAILED"; tail -30 "$LOG/client_remount.log"; exit 1; }

echo "=== Re-verify pass against the cold remount ==="
fio --name=lowconn --directory="$MOUNT" --size=24m --numjobs=8 --bs=4k \
    --ioengine=psync --direct=0 --verify=crc32c --verify_fatal=1 --verify_only \
    --group_reporting > "$LOG/fio_reverify.log" 2>&1
REVERIFY_EXIT=$?
tail -20 "$LOG/fio_reverify.log"

echo ""
echo "=== Signal summary (watchdog / backpressure behavior) ==="
grep -h "Connection limit reached\|Connection pressure\|ServerBusy\|stepping down from leadership\|announce_recovery\|Connection slots exhausted" "$LOG"/server*.log | sort | uniq -c | sort -rn | tee "$LOG/watchdog_signals.log"
echo "--- client-side retry/error signals ---"
grep -h "ServerBusy\|retry\|Retrying\|connection refused\|Connection refused" "$LOG/client.log" | wc -l | xargs echo "client retry/busy log lines:"

echo ""
echo "════════════════════════════════════════════"
if [ "$FIO_EXIT" -ne 0 ] || [ "$REVERIFY_EXIT" -ne 0 ]; then
    echo "  CORRUPTION DETECTED — fio verify failed (write pass exit=$FIO_EXIT, reverify exit=$REVERIFY_EXIT)"
    echo "  See $LOG/fio.log and $LOG/fio_reverify.log"
    RESULT=1
else
    echo "  NO CORRUPTION — fio verify clean on both the live-mount pass and the cold remount pass"
    RESULT=0
fi
echo "════════════════════════════════════════════"
echo "Logs: $LOG"

echo ""
echo "=== Cleanup ==="
cleanup_all

exit "$RESULT"
