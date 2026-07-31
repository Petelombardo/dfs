#!/bin/bash
# Repro: seed_chunk_seq (commit 0c3718b, 2026-07-28) seeds the per-slot patch
# counter (chunk_seq / CHUNK_SEQ_TABLE) from ChunkLocation.client_write_seq --
# which is the per-FILE write counter (dfs-client/src/client.rs:6823,
# self.write_seq.get(&file_id)), not a per-slot value. On any file with
# meaningfully more total writes than any single chunk's own patch count (i.e.
# any real multi-chunk file, e.g. a VM disk), this seeds chunk_seq far above
# the server's real per-slot CHUNK_SEQ_TABLE value, so the very first patch to
# that slot looks like a huge "chunk_seq gap" (new_seq > current_seq + 1) and
# forces handle_multi_patch's refresh_slot_from_leader rebase
# (dfs-server/src/server.rs:11510-11520) on every single patch instead of the
# rare real race it was written for.
#
# Confirmed live on gluster4 2026-07-28 during 3 real VM-111 install failures:
# 7184 "chunk_seq gap" events since the post-fix restart, 5288 (74%) with gap
# size > 100 (max 122,682) vs only 428 with gap <= 5 (the legitimate small
# concurrent-patch races this check was designed to catch).
#
# This script mimics a real VM install's write pattern end-to-end and checks
# backend data integrity at every stage:
#   1. Open file, write enough filler chunks elsewhere in the file to inflate
#      file-level write_seq well past any single chunk's real patch count.
#   2. Burst of small fsync'd patches to one hot chunk (>8s, matching
#      client.rs's own "~8s of continuous active patching" ForceFold trigger)
#      -> CLIENT-triggered fold.
#   3. Integrity check.
#   4. More patches to the same hot chunk, then go idle.
#   5. Wait out PATCH_DEBOUNCE_IDLE (20s, server.rs:1103) with no further
#      writes -> SERVER-triggered (debounce_fold_slot) fold, not client
#      ForceFold.
#   6. Integrity check.
#   7. More patches. Final integrity check.
#
# Integrity is checked by comparing a byte-exact Python-side shadow buffer
# against a fresh read of the same range through the mount at each
# checkpoint -- not just "no crash", actual content equality.
#
# Usage: bash scripts/repro_seed_chunk_seq_gap_corruption.sh

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-repro-seedgap
MOUNT=/tmp/dfs-repro-seedgap-mount
LOG=/tmp/dfs-repro-seedgap-logs
CLUSTER="127.0.0.1:8970,127.0.0.1:8971,127.0.0.1:8972,127.0.0.1:8973,127.0.0.1:8974"
BIN="$REPO/target/release"

cleanup_all() {
    pkill -f "dfs-server.*dfs-repro-seedgap" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$BASE" "$MOUNT" "$LOG"
mkdir -p "$MOUNT" "$LOG" "$BASE"

echo "=== Initializing 5-node cluster (debug logging) ==="
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
done
sleep 3

nohup "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
echo "Mounted."

IMG="$MOUNT/vmdisk.img"

echo ""
echo "=== Phase 0: filler writes (32 x 4MB fresh chunks = 128MB) to inflate file-level write_seq ==="
dd if=/dev/urandom of="$IMG" bs=4M count=32 2>/dev/null
sync "$MOUNT"
sleep 1
WRITE_SEQ_MARK=$(grep -o "flush_buffer_async_one.*calling write_data_with_cache" "$LOG/client.log" | wc -l)
echo "Foreground flush_buffer_async_one calls so far (proxy for file-level write_seq climb): $WRITE_SEQ_MARK"

echo ""
echo "=== Pre-phase: interleave a few real patches to the hot chunk (idx 0) among patches to  ==="
echo "=== OTHER chunks (idx 1-9) in the SAME file. This is what actually inflates the hot     ==="
echo "=== chunk's recorded client_write_seq past its true per-slot patch count -- touching    ==="
echo "=== only the hot chunk keeps the two counters in lockstep and never reproduces the bug. ==="
python3 - "$IMG" > "$LOG/prephase.log" 2>&1 <<'PYEOF'
import os, sys, random
img = sys.argv[1]
CHUNK = 4 * 1024 * 1024
HOT_OFF = 0
OTHER_CHUNK_IDXS = list(range(1, 10))
fd = os.open(img, os.O_RDWR)
for round_i in range(6):
    length = random.choice([512, 1024, 2048])
    off_in_chunk = random.randint(0, CHUNK - length)
    os.pwrite(fd, os.urandom(length), HOT_OFF + off_in_chunk)
    os.fsync(fd)
    for cidx in OTHER_CHUNK_IDXS:
        ooff = cidx * CHUNK + random.randint(0, CHUNK - 1024)
        os.pwrite(fd, os.urandom(1024), ooff)
        os.fsync(fd)
os.close(fd)
print("pre-phase done: 6 real patches to hot chunk, interleaved with 54 patches to other chunks")
PYEOF
cat "$LOG/prephase.log"
sync "$MOUNT"
sleep 1

echo ""
echo "=== Remounting client (simulates a fresh VM/qemu session opening a pre-existing disk image -- ==="
echo "=== this is the real trigger: refresh_engine_flagged seeds chunk_seq from every location in  ==="
echo "=== the first chunk-map window it fetches for a newly-opened file, unconditionally)           ==="
pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 1
nohup "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "REMOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
echo "Remounted with a fresh client process (empty chunk_seq/engine state)."

python3 - "$IMG" "$LOG" > "$LOG/driver.log" 2>&1 <<'PYEOF'
import os, sys, hashlib, time, random

img, logdir = sys.argv[1], sys.argv[2]
CHUNK = 4 * 1024 * 1024
HOT_CHUNK_IDX = 0
HOT_OFF = HOT_CHUNK_IDX * CHUNK

fd = os.open(img, os.O_RDWR)

# Shadow buffer for the hot chunk only -- read it fresh from disk right now
# (post pre-phase, post-remount) so it exactly matches what's actually stored
# before any of the bursts below.
shadow = bytearray(os.pread(fd, CHUNK, HOT_OFF))
assert len(shadow) == CHUNK, f"expected {CHUNK} bytes at offset {HOT_OFF}, got {len(shadow)}"

def integrity_check(label):
    os.fsync(fd)
    actual = os.pread(fd, CHUNK, HOT_OFF)
    ok = (bytes(shadow) == actual)
    shadow_h = hashlib.sha256(shadow).hexdigest()[:16]
    actual_h = hashlib.sha256(actual).hexdigest()[:16]
    print(f"[{label}] integrity: {'PASS' if ok else '*** FAIL ***'} "
          f"(shadow={shadow_h} actual={actual_h} len_shadow={len(shadow)} len_actual={len(actual)})")
    return ok

def patch_burst(duration_s, label):
    n = 0
    end = time.time() + duration_s
    while time.time() < end:
        length = random.choice([512, 1024, 4096, 8192])
        off_in_chunk = random.randint(0, CHUNK - length)
        data = os.urandom(length)
        os.pwrite(fd, data, HOT_OFF + off_in_chunk)
        shadow[off_in_chunk:off_in_chunk + length] = data
        os.fsync(fd)
        n += 1
    print(f"[{label}] wrote {n} patches over {duration_s}s")

overall_ok = True

print("=== Phase 1: patch burst on hot chunk (chunk_idx=0), >8s -> expect CLIENT ForceFold ===")
patch_burst(10, "phase1-burst")
time.sleep(2)  # let an in-flight ForceFold land
overall_ok &= integrity_check("after-phase1-client-fold")

print("=== Phase 2: a few more patches on hot chunk, then go idle ===")
patch_burst(3, "phase2-burst")
last_patch_at = time.time()
overall_ok &= integrity_check("after-phase2-before-idle")

print("=== Phase 3: idle for 25s (> PATCH_DEBOUNCE_IDLE=20s) -> expect SERVER (debounce_fold_slot) fold, no client ForceFold ===")
time.sleep(25)
overall_ok &= integrity_check("after-phase3-server-fold")

print("=== Phase 4: more patches after server fold ===")
patch_burst(5, "phase4-burst")
time.sleep(2)
overall_ok &= integrity_check("after-phase4-final")

os.close(fd)
print(f"OVERALL: {'PASS' if overall_ok else 'FAIL'}")
sys.exit(0 if overall_ok else 1)
PYEOF
DRIVER_RC=$?

cat "$LOG/driver.log"
sync "$MOUNT"

echo ""
echo "=== Server-side chunk_seq gap stats (all 5 nodes) ==="
TOTAL_GAPS=0
TOTAL_BIG_GAPS=0
for i in 1 2 3 4 5; do
    F="$BASE/node${i}/metadata/../../node${i}"
    SLOG="$LOG/server${i}.log"
    GAPS=$(grep -c "chunk_seq gap" "$SLOG" 2>/dev/null || echo 0)
    BIG=$(grep "chunk_seq gap" "$SLOG" 2>/dev/null | sed -E 's/.*chunk_seq gap \(([0-9]+) vs expected ([0-9]+)\).*/\1 \2/' | awk '{d=$1-$2; if(d>50) c++} END{print c+0}')
    echo "node${i}: chunk_seq gap events=$GAPS (>50 magnitude: $BIG)"
    TOTAL_GAPS=$((TOTAL_GAPS + GAPS))
    TOTAL_BIG_GAPS=$((TOTAL_BIG_GAPS + BIG))
done
echo "TOTAL: gap events=$TOTAL_GAPS, large (>50) gap events=$TOTAL_BIG_GAPS"

echo ""
echo "=== Fold activity (client- vs server-triggered) ==="
grep "ForceFold:.*folded" "$LOG/client.log" | tail -5
echo "--- server-side fold completions (any trigger) ---"
grep "Single fold: file" "$LOG"/server*.log | tail -10

echo ""
echo "=== RESULT ==="
if [ "$DRIVER_RC" -eq 0 ]; then
    echo "Data integrity: PASS throughout all phases"
else
    echo "Data integrity: *** FAIL *** -- shadow buffer diverged from actual mount content at some checkpoint (see above)"
fi
if [ "$TOTAL_BIG_GAPS" -gt 0 ]; then
    echo "seed_chunk_seq bug: REPRODUCED ($TOTAL_BIG_GAPS large false-positive chunk_seq gap events, matching the staging incident)"
else
    echo "seed_chunk_seq bug: NOT reproduced this run (no large gap events -- may need more filler chunks/longer run)"
fi

echo ""
echo "=== Cleaning up ==="
cleanup_all

exit $DRIVER_RC
