#!/bin/bash
# Measure how much unflushed data the client is willing to hold in memory
# during a large sequential write, and the resulting throughput.
#
# Built to investigate: e9d61aa (2026-05-06) widened the graduated write-buffer
# back-pressure's unthrottled zone from 0-25% of cap to 0-75% of cap, to fix a
# "5MB/s regression and spurious FUSE dispatch thread stalls" that was caused by
# the back-pressure sleep running inline on FUSE's single-threaded dispatch
# loop. That root cause was fixed independently and later by 4cc0d930
# (2026-06-23), which moved write()'s back-pressure loop into a spawned tokio
# task so sleeping there no longer blocks dispatch. e9d61aa's threshold
# widening was never revisited afterward, so up to 75% of the write-buffer cap
# can now sit fully unthrottled in client RAM before any pushback at all.
#
# This script writes a single large sequential file well past the cap and
# reports the PEAK buffered/uncommitted bytes seen (via the WBSTATS debug log
# line) alongside overall throughput, so a before/after threshold change can
# be compared on both axes instead of throughput alone.
#
# Usage: bash scripts/repro_writebuffer_uncommitted.sh [size_mb]

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-wbuncommitted-mount
LOG=/tmp/dfs-wbuncommitted-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
TESTFILE="$MOUNT/wbuncommitted.img"

SIZE_MB=${1:-2048}

cleanup_all() {
    pkill -f "dfs-server" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$MOUNT" "$LOG" 2>/dev/null || true
mkdir -p "$MOUNT" "$LOG"

echo "=== Starting 5-node cluster ==="
cd "$REPO"
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null
for i in 1 2 3 4 5; do
    RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        > "$LOG/server${i}.log" 2>&1 &
done
sleep 3

# debug level: WBSTATS and BPFILLTIMING are both debug!() lines.
env RUST_LOG=debug ${DFS_WRITE_BUFFER_CAP_MB:+DFS_WRITE_BUFFER_CAP_MB="$DFS_WRITE_BUFFER_CAP_MB"} \
    "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
CLIENT_PID=$(pgrep -f "dfs-client mount $MOUNT" | head -1)
echo "Mounted. Client PID=$CLIENT_PID"

grep -m1 "Write buffer cap sizing" "$LOG/client.log" || true

echo "=== Writing ${SIZE_MB}MB sequentially ==="
DD_OUT=$(dd if=/dev/urandom of="$TESTFILE" bs=1M count="$SIZE_MB" 2>&1)
echo "$DD_OUT"
THROUGHPUT_LINE=$(echo "$DD_OUT" | tail -1)

echo "=== Draining: sync ==="
SYNC_START=$(date +%s.%N)
sync "$MOUNT"
SYNC_END=$(date +%s.%N)
echo "sync took $(echo "$SYNC_END - $SYNC_START" | bc)s"

echo ""
echo "=== Write-buffer cap ==="
CAP=$(grep -oP '(?<=cap=)\d+' "$LOG/client.log" | tail -1)
echo "cap (bytes) = ${CAP:-unknown}"

echo ""
echo "=== Peak buffered/uncommitted bytes observed (WBSTATS) ==="
PEAK=$(grep -oP '(?<=WBSTATS buffered=)\d+' "$LOG/client.log" | sort -n | tail -1)
PEAK_PCT=$(grep -oP '(?<=WBSTATS buffered=)\d+ cap=\d+ fill_pct=\d+' "$LOG/client.log" \
    | awk -F'fill_pct=' '{print $2}' | sort -n | tail -1)
if [ -n "${PEAK:-}" ]; then
    PEAK_MB=$(echo "scale=1; $PEAK / 1024 / 1024" | bc)
    echo "Peak buffered: ${PEAK} bytes (~${PEAK_MB} MB), peak fill_pct=${PEAK_PCT:-unknown}%"
else
    echo "No WBSTATS samples found — write likely completed faster than the 200ms sample interval."
fi

echo ""
echo "=== Time spent under back-pressure delay (BPFILLTIMING) ==="
BP_COUNT=$(grep -c "BPFILLTIMING" "$LOG/client.log" 2>/dev/null || echo 0)
echo "BPFILLTIMING samples (delay_ms>0): ${BP_COUNT}"

echo ""
echo "=== Summary ==="
echo "File size: ${SIZE_MB}MB"
echo "dd result: $THROUGHPUT_LINE"
echo "Cap: ${CAP:-unknown} bytes"
echo "Peak buffered (uncommitted): ${PEAK:-unknown} bytes (peak fill_pct=${PEAK_PCT:-unknown}%)"

echo ""
echo "=== Cleanup ==="
rm -f "$TESTFILE" 2>/dev/null
kill "$CLIENT_PID" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -f "dfs-server" 2>/dev/null || true
echo "Done. Logs in $LOG/"
