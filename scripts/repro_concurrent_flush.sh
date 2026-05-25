#!/bin/bash
# Reproduce the concurrent-flush race:
#   Two flush tasks for the same chunk both read old_location.chunk_id before
#   acquiring the chunk lock. The first task patches chunk A→B and renames it.
#   The second task then tries to patch with base A, server says "file not found",
#   client falls back to a fresh write that zeros unpatched regions → corruption.
#
# Method: write a 200MB file, then fire N parallel workers that each write a
# distinct pattern to a distinct 64KB region within the SAME 4MB chunk (chunk 0),
# followed immediately by fsync. The concurrent fsync calls race to flush the same
# chunk, triggering the bug.
#
# Expected: each region reads back its expected pattern. Zeros where a pattern
# should be means the fresh-write fallback fired → corruption confirmed.

set -e

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-repro-mount
LOG=/tmp/dfs-repro-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
TESTFILE="$MOUNT/repro_concurrent.img"

NUM_WORKERS=${1:-8}    # parallel writers, all hitting chunk 0
REPS=${2:-40}          # write+fsync cycles per worker
REGION_KB=64           # each worker owns a 64KB slice of chunk 0

echo "=== repro_concurrent_flush.sh ==="
echo "Workers: $NUM_WORKERS  Reps: $REPS  Region: ${REGION_KB}KB each"
echo ""

# ── Setup ────────────────────────────────────────────────────────────────────
pkill -f "dfs-server" 2>/dev/null || true
pkill -f "dfs-client" 2>/dev/null || true
sleep 0.5
fusermount -u "$MOUNT" 2>/dev/null || true
rm -rf "$BASE" "$MOUNT" "$LOG"
mkdir -p "$MOUNT" "$LOG"

echo "=== Starting 5-node cluster ==="
cd "$REPO"
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null
for i in 1 2 3 4 5; do
    RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        > "$LOG/server${i}.log" 2>&1 &
done
sleep 3

RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
CLIENT_PID=$!
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -20 "$LOG/client.log"; exit 1; }
echo "Mounted."
echo ""

# ── Write initial 200MB file ─────────────────────────────────────────────────
echo "=== Writing initial 200MB file (all zeros to establish chunk IDs) ==="
dd if=/dev/zero of="$TESTFILE" bs=1M count=200 2>/dev/null
sync "$MOUNT"
sleep 1
echo "Initial write committed."
echo ""

# ── Concurrent patch workers ─────────────────────────────────────────────────
# Each worker:
#   - owns a unique 64KB slice of chunk 0 (bytes [i*64K .. (i+1)*64K))
#   - writes its pattern (0x01, 0x02, ...) to that slice
#   - immediately fsyncs the file
#   - repeats REPS times
#
# All workers run in parallel. The concurrent fsyncs trigger concurrent flush
# tasks for chunk 0, racing on old_location.chunk_id.

echo "=== Firing $NUM_WORKERS concurrent workers, $REPS write+fsync each ==="

run_worker() {
    local id=$1
    local offset_bytes=$(( id * REGION_KB * 1024 ))
    local pattern=$(( (id % 255) + 1 ))   # 1..255, never 0 so zeros = corruption

    python3 - <<PYEOF
import os, sys

fd = os.open("$TESTFILE", os.O_RDWR)
buf = bytes([$pattern] * (${REGION_KB} * 1024))
for rep in range($REPS):
    os.pwrite(fd, buf, $offset_bytes)
    os.fsync(fd)
os.close(fd)
PYEOF
}

PIDS=()
for id in $(seq 0 $(( NUM_WORKERS - 1 ))); do
    run_worker "$id" &
    PIDS+=($!)
done

# Wait for all workers
WORKER_FAIL=0
for pid in "${PIDS[@]}"; do
    wait "$pid" || { echo "Worker $pid exited non-zero"; WORKER_FAIL=1; }
done
[ $WORKER_FAIL -eq 1 ] && echo "WARNING: one or more workers failed"

echo "All workers done. Final sync..."
sync "$MOUNT"
sleep 2
echo ""

# ── Verify ───────────────────────────────────────────────────────────────────
echo "=== Verifying each worker's region ==="

PASS=0; FAIL=0; CORRUPT_REGIONS=()

for id in $(seq 0 $(( NUM_WORKERS - 1 ))); do
    offset_bytes=$(( id * REGION_KB * 1024 ))
    expected=$(( (id % 255) + 1 ))

    result=$(python3 - <<PYEOF
import sys
try:
    with open("$TESTFILE", "rb") as f:
        f.seek($offset_bytes)
        data = f.read(${REGION_KB} * 1024)
    expected = $expected
    bad = [(i, b) for i, b in enumerate(data) if b != expected]
    if bad:
        first_i, first_b = bad[0]
        print(f"FAIL offset={$offset_bytes} expected=0x{expected:02x} got=0x{first_b:02x} at byte {first_i} ({len(bad)} bad bytes total)")
    else:
        print(f"PASS offset={$offset_bytes} pattern=0x{expected:02x}")
except Exception as e:
    print(f"FAIL offset={$offset_bytes} read error: {e}")
PYEOF
)
    echo "  $result"
    if [[ "$result" == PASS* ]]; then
        PASS=$(( PASS + 1 ))
    else
        FAIL=$(( FAIL + 1 ))
        CORRUPT_REGIONS+=($id)
    fi
done

echo ""
echo "=== Results: PASS=$PASS  FAIL=$FAIL / $NUM_WORKERS ==="

if [ ${#CORRUPT_REGIONS[@]} -gt 0 ]; then
    echo ""
    echo "CORRUPTION DETECTED in regions: ${CORRUPT_REGIONS[*]}"
    echo ""
    echo "--- Client log: zero-gap fresh-write fallbacks ---"
    grep -E "gap_prefix=[^0]|fresh.write|zero.gap|MultiPatch failed" "$LOG/client.log" | tail -30
    echo ""
    echo "--- Client log: post-lock refresh (if fix is active) ---"
    grep "post-lock server_chunk_id refresh" "$LOG/client.log" | tail -10
    echo ""
    echo "Full client log: $LOG/client.log"
else
    echo ""
    echo "No corruption detected."
    echo ""
    echo "--- Stale retries seen (expected): ---"
    grep "is stale" "$LOG/client.log" | wc -l
    echo "--- Failed to open chunk (unexpected): ---"
    grep "Failed to open chunk" "$LOG/client.log" | wc -l
    echo "--- Post-lock refreshes triggered: ---"
    grep "post-lock server_chunk_id refresh" "$LOG/client.log" | wc -l
fi

# ── Cleanup ──────────────────────────────────────────────────────────────────
echo ""
echo "=== Cleanup ==="
kill "$CLIENT_PID" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -f "dfs-server" 2>/dev/null || true
echo "Done. Logs in $LOG/"
