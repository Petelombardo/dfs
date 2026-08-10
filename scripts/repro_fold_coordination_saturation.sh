#!/bin/bash
# Repro: fold_coordination_semaphore saturation forcing coordinate_and_fold_slot's
# uncoordinated fallback path (the pre-adf3faf mechanism its whole propose/lock
# protocol exists to replace), and checking for the resulting bad outcomes
# (REPLICA DISAGREEMENT, or worse, genuine chunk content corruption).
#
# Root-caused 2026-08-10 (staging VM-100, real incident): a much higher fold-
# attempt rate (the adaptive cold-start debounce, since reverted — see
# debounce_fold_slot's history) fully saturated fold_coordination_semaphore
# under real bulk-write load — "0/24 available", multi-second acquire waits —
# and forced repeated fold attempts through coordinate_and_fold_slot's
# uncoordinated fallback. Within seconds, a genuinely corrupt chunk (content
# didn't hash-match its own claimed identity) appeared on disk, unreplicated,
# for a live VM's qcow2 disk.
#
# This script reproduces the SATURATION mechanism using only what's on main
# today (the flat 20s PATCH_DEBOUNCE_IDLE debounce, unmodified — no adaptive
# debounce involved): write many DIFFERENT chunks of one big file in a tight
# burst, few patches each (VM-100's real shape — NOT repro_replica_disagreement.
# sh's single-hot-chunk shape). Each touched chunk spawns its own
# debounce_fold_slot task; since they're all written within the same short
# window, their 20s+jitter timers converge into the same synchronized wave the
# user originally observed with kdiskmark — which is exactly the shape that
# exhausted the semaphore on staging. DFS_FOLD_COORDINATION_CONCURRENCY is set
# very low here so this reproduces reliably with a modest chunk count instead
# of needing hundreds of real chunks like the staging incident did.
#
# Usage: bash scripts/repro_fold_coordination_saturation.sh [num_chunks] [semaphore_permits]
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-repro-foldsat
MOUNT=/tmp/dfs-repro-foldsat-mount
LOG=/tmp/dfs-repro-foldsat-logs
CLUSTER="127.0.0.1:8970,127.0.0.1:8971,127.0.0.1:8972,127.0.0.1:8973,127.0.0.1:8974"
BIN="$REPO/target/release"
CHUNK_SIZE=$((4 * 1024 * 1024))

if [ "${1:-}" = "cleanup" ]; then
    pkill -f "dfs-server.*dfs-repro-foldsat" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
    rm -rf "$BASE" "$MOUNT" "$LOG"
    echo "Cleaned up."
    exit 0
fi

NUM_CHUNKS="${1:-15}"
SEM_PERMITS="${2:-2}"

cleanup_all() {
    pkill -f "dfs-server.*dfs-repro-foldsat" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$BASE" "$MOUNT" "$LOG"
mkdir -p "$MOUNT" "$LOG" "$BASE"

# Disk guard (2026-08 incident precedent: an unthrottled repro writer filled a
# no-swap dev box's disk and forced a hard reboot — and a first version of
# THIS script's own guard underestimated real usage and hit the same thing:
# NUM_CHUNKS=80 with a *4 factor left only 2.8GB headroom looking "safe" on
# paper, but the real baseline write — replicated at RF=3 across all 5 local
# nodes' storage dirs on this shared filesystem, plus the patch/delta phase's
# own overhead, plus redb/log growth — actually consumed the entire remaining
# 2.8GB and drove /tmp to 0 free). *12 plus a hard absolute floor this time.
NEEDED_MB=$(( NUM_CHUNKS * 4 * 12 ))
MIN_ABSOLUTE_FREE_MB=1500
AVAIL_MB=$(df -m /tmp | awk 'NR==2 {print $4}')
if [ "$AVAIL_MB" -lt "$NEEDED_MB" ] || [ "$AVAIL_MB" -lt "$MIN_ABSOLUTE_FREE_MB" ]; then
    echo "REFUSING TO RUN: only ${AVAIL_MB}MB free on /tmp, need ~${NEEDED_MB}MB (NUM_CHUNKS=$NUM_CHUNKS) and at least ${MIN_ABSOLUTE_FREE_MB}MB absolute headroom. Lower NUM_CHUNKS or free space."
    exit 1
fi

echo "=== Initializing 5-node cluster (debug logging, DFS_FOLD_COORDINATION_CONCURRENCY=$SEM_PERMITS) ==="
export DFS_FOLD_COORDINATION_CONCURRENCY="$SEM_PERMITS"
declare -a SERVER_PIDS
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
    nohup "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" --log-level debug >> "$LOG/server${i}.log" 2>&1 &
    SERVER_PIDS[$i]=$!
done
sleep 3

nohup "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
echo "Mounted."

IMG="$MOUNT/bulkwrite.img"
FILE_SIZE=$((NUM_CHUNKS * CHUNK_SIZE))
echo "=== Establishing a real ${NUM_CHUNKS}-chunk baseline ($((FILE_SIZE / 1024 / 1024))MB) ==="
# A patch needs something EXISTING to patch — writing into never-touched sparse
# territory takes the direct full-chunk-write path and never touches
# debounce_fold_slot/coordinate_and_fold_slot at all (confirmed: an earlier
# version of this script that skipped this step produced zero fold activity
# across a 15+ minute observation window). Same reason
# repro_replica_disagreement.sh writes a real 4MB chunk with dd before its own
# patch storm. One shared random buffer reused for every chunk — content
# doesn't need to be unique, just real (non-sparse).
python3 - "$IMG" "$NUM_CHUNKS" "$CHUNK_SIZE" > "$LOG/baseline.log" 2>&1 <<'PYEOF'
import os, sys
img, num_chunks, chunk_size = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
buf = os.urandom(chunk_size)
fd = os.open(img, os.O_WRONLY | os.O_CREAT, 0o644)
for idx in range(num_chunks):
    os.pwrite(fd, buf, idx * chunk_size)
os.fsync(fd)
os.close(fd)
print(f"established baseline for {num_chunks} chunks")
PYEOF
echo "=== Baseline: $(cat "$LOG/baseline.log") ==="
sync
sleep 2

echo "=== Bulk write: one small patch per chunk across all $NUM_CHUNKS chunks, in a tight burst ==="
# fsync after every pwrite — matching repro_replica_disagreement.sh's proven
# technique — to force each write out as its own MultiPatch round trip instead
# of letting the client's local write-buffer coalesce all 80 into one flush at
# the final `sync`.
python3 - "$IMG" "$NUM_CHUNKS" "$CHUNK_SIZE" > "$LOG/writer.log" 2>&1 <<'PYEOF'
import os, sys, random
img, num_chunks, chunk_size = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
fd = os.open(img, os.O_WRONLY)
for idx in range(num_chunks):
    off = idx * chunk_size + random.randint(0, 4000)
    data = os.urandom(random.choice([4096, 8192]))
    os.pwrite(fd, data, off)
    os.fsync(fd)
os.close(fd)
print(f"wrote 1 patch to each of {num_chunks} distinct chunks")
PYEOF
echo "=== Writer done: $(cat "$LOG/writer.log") ==="

sync

echo "=== Waiting ~40s for the flat PATCH_DEBOUNCE_IDLE wave (20s base + up to 15s jitter) to hit the starved semaphore ==="
sleep 40

echo ""
echo "=== Results ==="
SATURATION_HITS=0
FALLBACK_HITS=0
CORRUPTION_HITS=0
for i in 1 2 3 4 5; do
    c=$(grep -c "fold_coordination_semaphore acquire took" "$LOG/server${i}.log" 2>/dev/null); c=${c:-0}
    SATURATION_HITS=$((SATURATION_HITS + c))
    c=$(grep -cE "local_fold_fingerprint unavailable|no reachable peer for file|peer unreachable/misbehaving for file" "$LOG/server${i}.log" 2>/dev/null); c=${c:-0}
    FALLBACK_HITS=$((FALLBACK_HITS + c))
    c=$(grep -c "disk corruption detected" "$LOG/server${i}.log" 2>/dev/null); c=${c:-0}
    CORRUPTION_HITS=$((CORRUPTION_HITS + c))
done
DISAGREEMENT_HITS=$(grep -c "REPLICA DISAGREEMENT" "$LOG/client.log" 2>/dev/null); DISAGREEMENT_HITS=${DISAGREEMENT_HITS:-0}

echo "fold_coordination_semaphore slow-acquire events (saturation signal): $SATURATION_HITS"
echo "coordinate_and_fold_slot uncoordinated-fallback events (choke point fired): $FALLBACK_HITS"
echo "\"disk corruption detected\" events (worst case — genuine content corruption): $CORRUPTION_HITS"
echo "REPLICA DISAGREEMENT events: $DISAGREEMENT_HITS"

if [ "$FALLBACK_HITS" -gt 0 ]; then
    echo ""
    echo "=== Sample fallback events, with context ==="
    grep -hE "local_fold_fingerprint unavailable|no reachable peer for file|peer unreachable/misbehaving for file|fold_coordination_semaphore acquire took" "$LOG"/server*.log | head -20
fi
if [ "$CORRUPTION_HITS" -gt 0 ]; then
    echo ""
    echo "=== Corruption events, with context ==="
    grep -hE "disk corruption detected|consolidation failed" "$LOG"/server*.log | head -20
fi

echo ""
if [ "$FALLBACK_HITS" -gt 0 ]; then
    echo "REPRODUCED: the uncoordinated fallback choke point fired under real bulk-write load."
else
    echo "NOT REPRODUCED this run: no fallback events observed — try raising NUM_CHUNKS or lowering SEM_PERMITS."
fi

echo ""
echo "Logs: $LOG/client.log, $LOG/server{1..5}.log"
echo "Cluster left running for inspection — use dfs-admin --cluster 127.0.0.1:8970 ... to poke at it."
echo "Run 'bash $0 cleanup' when done."
