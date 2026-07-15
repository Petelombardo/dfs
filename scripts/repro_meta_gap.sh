#!/bin/bash
# Repro: [META GAP] warnings (lost file-level metadata pushes) under sustained
# wide random-write load, mimicking kdiskmark's Q32T1/Q1T1 pattern against a
# large file (many distinct chunks, not just one hot chunk).
#
# Root-caused 2026-07-12: staging overnight kdiskmark run showed 1330 [META
# GAP] events / 1592 missing write_seq values out of ~4378 total writes to
# one file — over a third of all file-level metadata pushes never durably
# reached the leader. fsck the next morning found real corruption; the
# client's own cache masked it immediately after the run (looked clean).
#
# Unlike scripts/repro_replica_disagreement.sh (single hot 4MB chunk), this
# spreads writes across a much larger file so many distinct chunk_idx values
# are touched — closer to kdiskmark's actual access pattern.
#
# Usage: bash scripts/repro_meta_gap.sh [duration_secs] [file_size_mb]
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-repro-metagap
MOUNT=/tmp/dfs-repro-metagap-mount
LOG=/tmp/dfs-repro-metagap-logs
CLUSTER="127.0.0.1:8970,127.0.0.1:8971,127.0.0.1:8972,127.0.0.1:8973,127.0.0.1:8974"
BIN="$REPO/target/release"
DURATION="${1:-45}"
FILE_SIZE_MB="${2:-48}"
LOG_LEVEL="${3:-info}"

cleanup_all() {
    pkill -f "dfs-server.*dfs-repro-metagap" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$BASE" "$MOUNT" "$LOG"
mkdir -p "$MOUNT" "$LOG" "$BASE"

echo "=== Initializing 5-node cluster ($LOG_LEVEL logging) ==="
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
    nohup "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" --log-level "$LOG_LEVEL" >> "$LOG/server${i}.log" 2>&1 &
done
sleep 3

nohup "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level "$LOG_LEVEL" &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
echo "Mounted."

IMG="$MOUNT/wide.img"
echo "=== Writing ${FILE_SIZE_MB}MB base (many chunks) ==="
dd if=/dev/urandom of="$IMG" bs=1M count="$FILE_SIZE_MB" 2>/dev/null
sync
sleep 1

echo "=== Starting wide random-write storm (fsync-per-write, ${FILE_SIZE_MB}MB region, many concurrent writers) for ${DURATION}s ==="
FILE_SIZE_BYTES=$((FILE_SIZE_MB * 1024 * 1024))
writer_pids=()
for w in 1 2 3 4; do
    python3 - "$IMG" "$DURATION" "$FILE_SIZE_BYTES" "$w" > "$LOG/writer${w}.log" 2>&1 <<'PYEOF' &
import os, sys, random, time
img, duration, file_size, wid = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
fd = os.open(img, os.O_WRONLY)
end = time.time() + duration
n = 0
while time.time() < end:
    length = random.choice([4096, 8192, 16384, 65536])
    off = random.randint(0, file_size - length)
    data = os.urandom(length)
    os.pwrite(fd, data, off)
    os.fsync(fd)
    n += 1
os.close(fd)
print(f"writer {wid} wrote {n} patches")
PYEOF
    writer_pids+=($!)
done
# Bare `wait` blocks on every backgrounded job in this shell, including the
# dfs-server/dfs-client processes started above (deliberately left running for
# post-run inspection) — wait on the writer PIDs specifically instead.
wait "${writer_pids[@]}"

echo "=== Writers done ==="
sync
sleep 2

echo ""
echo "=== Results ==="
GAP_COUNT=0
for i in 1 2 3 4 5; do
    c=$(grep -c "META GAP" "$LOG/server${i}.log" 2>/dev/null)
    c="${c:-0}"
    echo "server${i}: $c [META GAP] warnings"
    GAP_COUNT=$((GAP_COUNT + c))
done
echo "Total: $GAP_COUNT"

if [ "$GAP_COUNT" -gt 0 ]; then
    echo ""
    echo "=== First gap, with context ==="
    for i in 1 2 3 4 5; do
        grep -m1 -B5 "META GAP" "$LOG/server${i}.log" 2>/dev/null && echo "(from server${i}.log)" && break
    done
fi

echo ""
echo "Logs: $LOG/client.log, $LOG/server{1..5}.log"
echo "Cluster left running for inspection."
