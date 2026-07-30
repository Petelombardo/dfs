#!/bin/bash
# "Folding day" repro #1 of 3: debounce-fold-vs-live-write race.
#
# Theory (2026-07-30 VM-108/111 overnight I/O-error investigation): a slot's
# accumulator is tracked in dirty_patch_slots, keyed by (file_id, chunk_idx),
# and PATCH_DEBOUNCE_IDLE (20s) after the last patch, debounce_fold_slot folds
# it. apply_patch overwrites dirty_patch_slots with the fresh token on every
# merge, and fold_slot_now re-reads that entry fresh under chunk_patch_locks
# immediately before folding — so in theory a debounce fold firing shortly
# after a fresh client write should always fold the LATEST accumulator, never
# a stale one. Production logs showed a fold that consolidated a token whose
# client_write_seq was ~7000 writes behind the slot's actual current token,
# which shouldn't be possible if that reasoning holds.
#
# This reproduces the exact shape: patch A (fresh accumulator) followed ~19s
# later by patch B (merge onto the same accumulator, different byte range),
# then wait for the debounce backstop to fire (~20s after B) and verify BOTH
# patches' bytes survived the fold. If the fold used a stale/pre-B view of
# the accumulator, patch B's bytes will be missing or wrong post-fold — an
# externally observable correctness failure, not just a log curiosity.
#
# Usage: bash scripts/repro_fold_vs_write_race.sh

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-foldrace-mount
LOG=/tmp/dfs-foldrace-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
FILE="$MOUNT/foldrace.img"
CHUNK_SIZE=$((4 * 1024 * 1024))
PATCH_DEBOUNCE_IDLE=20

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
    RUST_LOG=debug "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        --log-level debug > "$LOG/server${i}.log" 2>&1 &
done
sleep 3

env RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
CLIENT_PID=$(pgrep -f "dfs-client mount $MOUNT" | head -1)
echo "Mounted. Client PID=$CLIENT_PID"

echo "=== Writing baseline chunk (0xAA, ${CHUNK_SIZE} bytes) ==="
python3 -c "
import sys
with open('$FILE', 'wb') as f:
    f.write(bytes([0xAA]) * $CHUNK_SIZE)
"
sync "$MOUNT"
sleep 1
LOGMARK=$(wc -l < "$LOG/client.log")

echo "=== Patch A: 4KB of 0xBB at offset 1,000,000 (fresh accumulator) ==="
python3 -c "
import os
fd = os.open('$FILE', os.O_RDWR)
os.pwrite(fd, bytes([0xBB]) * 4096, 1000000)
os.close(fd)
"
T_A=$(date +%s)
echo "patch A written at $(date -u +%H:%M:%S)"

SLEEP_BEFORE_B=$((PATCH_DEBOUNCE_IDLE - 1))
echo "=== Sleeping ${SLEEP_BEFORE_B}s (just under PATCH_DEBOUNCE_IDLE) ==="
sleep "$SLEEP_BEFORE_B"

echo "=== Patch B: 4KB of 0xCC at offset 2,000,000 (merge onto same accumulator) ==="
python3 -c "
import os
fd = os.open('$FILE', os.O_RDWR)
os.pwrite(fd, bytes([0xCC]) * 4096, 2000000)
os.close(fd)
"
T_B=$(date +%s)
echo "patch B written at $(date -u +%H:%M:%S)"

WAIT_FOR_FOLD=$((PATCH_DEBOUNCE_IDLE + 15))
echo "=== Waiting ${WAIT_FOR_FOLD}s for debounce backstop to fire on patch B ==="
sleep "$WAIT_FOR_FOLD"

sync "$MOUNT"
sleep 1

echo ""
echo "=== Server-side fold/patch log lines for this file since test start ==="
for i in 1 2 3 4 5; do
    echo "-- server${i} --"
    grep -E "MERGE-TRACE|Single fold|MultiPatch:" "$LOG/server${i}.log" | tail -10
done

echo ""
echo "=== Verifying final content ==="
python3 -c "
import os, sys
fd = os.open('$FILE', os.O_RDONLY)

def check(offset, length, expected_byte, label):
    data = os.pread(fd, length, offset)
    ok = all(b == expected_byte for b in data)
    print(f'{label}: {\"OK\" if ok else \"MISMATCH\"} (offset={offset}, expected=0x{expected_byte:02x}, got first byte=0x{data[0]:02x})')
    return ok

ok_a = check(1000000, 4096, 0xBB, 'Patch A bytes (0xBB @ 1,000,000)')
ok_b = check(2000000, 4096, 0xCC, 'Patch B bytes (0xCC @ 2,000,000)')
ok_base = check(0, 4096, 0xAA, 'Untouched baseline bytes (0xAA @ 0)')
os.close(fd)

if ok_a and ok_b and ok_base:
    print()
    print('PASS: all three regions correct — fold consolidated the full, current accumulator')
    sys.exit(0)
else:
    print()
    print('FAIL: fold lost or corrupted data — consistent with folding a stale/pre-B accumulator')
    sys.exit(1)
"
RESULT=$?

echo ""
if [ $RESULT -eq 0 ]; then
    echo "=== REPRO RESULT: PASS (no divergence reproduced this run) ==="
else
    echo "=== REPRO RESULT: FAIL (divergence reproduced) ==="
fi

rm -f "$FILE"
kill "$CLIENT_PID" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -f "dfs-server" 2>/dev/null || true
echo "Done. Logs in $LOG/"
exit $RESULT
