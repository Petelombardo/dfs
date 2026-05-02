#!/bin/bash
# Reproducer for the Kodi/HDHomeRun seek corruption bug.
#
# Key insight: the bug manifests on the SECOND file played, not the first.
# Pattern observed in staging:
#   1. HDHomeRun startup scan: opens EVERY recording O_RDWR, reads 32KB, closes
#      (no writes — but this creates write session state for each file)
#   2. Kodi plays file A: seeks → O_RDWR write of seek table → PatchChunk ✓
#   3. Kodi plays file B: seeks → O_RDWR write of seek table, then ANOTHER
#      O_RDWR open with no writes fires a spurious PatchChunk with stale data
#
# Also tests EOF probe: Kodi reads near the end of file before seeking.

set -e
REPO=$(cd "$(dirname "$0")" && pwd)
BIN="$REPO/target/release"
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-seek-mount
LOG=/tmp/dfs-seek-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902"
CHUNK_SIZE=$((4 * 1024 * 1024))
T=/tmp/dfs-seek-tmp-$$
PASS=0; FAIL=0

check() {
    local name="$1" result="$2"
    if [ "$result" = "PASS" ]; then
        echo "  PASS: $name"; PASS=$((PASS+1))
    else
        echo "  FAIL: $name ($3)"; FAIL=$((FAIL+1))
    fi
}

compare_range() {
    local label="$1" offset="$2" length="$3" file="$4" ref="$5"
    local dfs_tmp="$T/cmp_dfs.bin" loc_tmp="$T/cmp_loc.bin"
    dd if="$file" of="$dfs_tmp" bs=1 skip="$offset" count="$length" 2>/dev/null
    dd if="$ref"  of="$loc_tmp" bs=1 skip="$offset" count="$length" 2>/dev/null
    local m1 m2
    m1=$(md5sum "$loc_tmp" | cut -d' ' -f1)
    m2=$(md5sum "$dfs_tmp" | cut -d' ' -f1)
    if [ "$m1" = "$m2" ]; then
        check "$label" PASS
    else
        local detail
        detail=$(python3 -c "
a=open('$loc_tmp','rb').read(); b=open('$dfs_tmp','rb').read()
diffs=[(i,x,y) for i,(x,y) in enumerate(zip(a,b)) if x!=y]
if diffs:
    i,x,y=diffs[0]
    print(f'first diff byte {i}: expected=0x{x:02x} got=0x{y:02x} ({len(diffs)} total diffs)')
else:
    print(f'len mismatch: expected={len(a)} got={len(b)}')
" 2>/dev/null)
        check "$label" FAIL "$detail"
    fi
    rm -f "$dfs_tmp" "$loc_tmp"
}

teardown() {
    fusermount -u "$MOUNT" 2>/dev/null || true
    sleep 0.3
    pkill -f "dfs-server" 2>/dev/null || true
    sleep 0.5
    rm -rf "$BASE" "$LOG" "$MOUNT" "$T"
}
trap teardown EXIT

echo "=== DVR seek corruption reproducer (two-file pattern) ==="
pkill -f "dfs-server" 2>/dev/null || true
fusermount -u "$MOUNT" 2>/dev/null || true
rm -rf "$BASE" "$LOG" "$MOUNT" "$T"
mkdir -p "$MOUNT" "$LOG" "$T"

echo "--- Build ---"
cd "$REPO" && cargo build --release 2>&1 | tail -2

echo "--- Start cluster ---"
bash "$REPO/scripts/setup-cluster.sh" 3 2>/dev/null
for i in 1 2 3; do
    RUST_LOG=dfs_server=warn "$BIN/dfs-server" start \
        --config "$BASE/node${i}/config.toml" > "$LOG/server${i}.log" 2>&1 &
done
sleep 2
RUST_LOG=dfs_client=info "$BIN/dfs-client" mount "$MOUNT" \
    --cluster "$CLUSTER" --log-file "$LOG/client.log" --allow-other --log-level info &
sleep 1
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -20 "$LOG/client.log"; exit 1; }
echo "Mounted."

FILE_A="$MOUNT/recording_a.mpg"
FILE_B="$MOUNT/recording_b.mpg"
REF_A="$T/ref_a.bin"
REF_B="$T/ref_b.bin"

# ── Phase 1: Create two recordings with full 4MB chunk 0 ─────────────────────
echo ""
echo "=== Phase 1: Create two recordings ==="
dd if=/dev/urandom of="$T/orig_a.bin" bs="$CHUNK_SIZE" count=1 2>/dev/null
dd if=/dev/urandom of="$T/orig_b.bin" bs="$CHUNK_SIZE" count=1 2>/dev/null

cp "$T/orig_a.bin" "$FILE_A"
cp "$T/orig_b.bin" "$FILE_B"
sync; sleep 0.5

SIZE_A=$(stat -c%s "$FILE_A")
SIZE_B=$(stat -c%s "$FILE_B")
echo "  File A: $SIZE_A bytes, File B: $SIZE_B bytes"
compare_range "Phase1: File A chunk 0 intact" 0 "$CHUNK_SIZE" "$FILE_A" "$T/orig_a.bin"
compare_range "Phase1: File B chunk 0 intact" 0 "$CHUNK_SIZE" "$FILE_B" "$T/orig_b.bin"

# ── Phase 2: HDHomeRun startup scan — O_RDWR opens all files, reads, no write ─
echo ""
echo "=== Phase 2: Simulate HDHomeRun startup scan (O_RDWR read-only on both) ==="
python3 -c "
import os
for path in ['$FILE_A', '$FILE_B']:
    fd = os.open(path, os.O_RDWR)
    data = os.read(fd, 32768)
    os.close(fd)
    print(f'  Scanned {path.split(\"/\")[-1]}: read {len(data)} bytes, closed without writing')
"
sleep 0.5

# ── Phase 3: Play File A — seek writes header, check EOF, rapid re-open ───────
echo ""
echo "=== Phase 3: Play File A (first file — should always work) ==="

# Kodi EOF probe: open, seek near end, read, close
echo "  EOF probe on File A..."
python3 -c "
import os
fd = os.open('$FILE_A', os.O_RDONLY)
size = os.lseek(fd, 0, 2)  # seek to end
near_end = max(0, size - 262144)
os.lseek(fd, near_end, 0)
data = os.read(fd, 262144)
os.close(fd)
print(f'  File A EOF probe: size={size}, read {len(data)} bytes near end')
"

# Write seek header v1 to File A
dd if=/dev/urandom of="$T/seek_a_v1.bin" bs=1 count=12032 2>/dev/null
cp "$T/orig_a.bin" "$REF_A"
dd if="$T/seek_a_v1.bin" of="$REF_A" bs=1 seek=0 count=12032 conv=notrunc 2>/dev/null

dd if="$T/seek_a_v1.bin" of="$FILE_A" bs=1 seek=0 count=12032 conv=notrunc 2>/dev/null

# Rapid re-open (like Kodi does after writing seek table)
python3 -c "
import os, time
time.sleep(0.05)
fd = os.open('$FILE_A', os.O_RDWR)
os.close(fd)
print('  File A: rapid re-open/close done')
"

echo "  Waiting for flushes..."
sleep 0.5

compare_range "Phase3: File A header = seek_a_v1" 0 12032 "$FILE_A" "$REF_A"
compare_range "Phase3: File A data beyond header intact" 12032 $((CHUNK_SIZE - 12032)) "$FILE_A" "$REF_A"

# ── Phase 4: Play File B — second file (the one that fails in staging) ────────
echo ""
echo "=== Phase 4: Play File B (second file — bug target) ==="

# Kodi EOF probe on File B
echo "  EOF probe on File B..."
python3 -c "
import os
fd = os.open('$FILE_B', os.O_RDONLY)
size = os.lseek(fd, 0, 2)
near_end = max(0, size - 262144)
os.lseek(fd, near_end, 0)
data = os.read(fd, 262144)
os.close(fd)
print(f'  File B EOF probe: size={size}, read {len(data)} bytes near end')
"

# Write seek header v1 to File B
dd if=/dev/urandom of="$T/seek_b_v1.bin" bs=1 count=12032 2>/dev/null
cp "$T/orig_b.bin" "$REF_B"
dd if="$T/seek_b_v1.bin" of="$REF_B" bs=1 seek=0 count=12032 conv=notrunc 2>/dev/null

# Use python to do write + immediate re-open atomically without any sleep
# This ensures the re-open happens before the flush completes
python3 -c "
import os, time

# Session 1: write seek header
fd = os.open('$FILE_B', os.O_RDWR)
with open('$T/seek_b_v1.bin', 'rb') as f:
    data = f.read()
os.write(fd, data)
os.close(fd)  # triggers release task + flush

# Session 2: immediate re-open with NO writes (the bug trigger)
# No sleep — races with the flush task
fd2 = os.open('$FILE_B', os.O_RDWR)
os.close(fd2)

# Session 3: another immediate re-open
fd3 = os.open('$FILE_B', os.O_RDWR)
os.close(fd3)

print('  File B: write + 2x immediate re-open/close done (no sleep)')
"

echo "  Waiting for all flushes..."
sleep 1

echo ""
echo "=== Phase 4 checks: File B must have seek_b_v1 header ==="
compare_range "Phase4: File B header = seek_b_v1 (not corrupted)" 0 12032 "$FILE_B" "$REF_B"
compare_range "Phase4: File B data beyond header intact" 12032 $((CHUNK_SIZE - 12032)) "$FILE_B" "$REF_B"

# Also re-verify File A hasn't been disturbed
compare_range "Phase4: File A still correct (not cross-contaminated)" 0 "$CHUNK_SIZE" "$FILE_A" "$REF_A"

# ── Phase 5: Second seek on File B — write new header ─────────────────────────
echo ""
echo "=== Phase 5: Second seek on File B (write seek_b_v2) ==="
dd if=/dev/urandom of="$T/seek_b_v2.bin" bs=1 count=12032 2>/dev/null
cp "$T/orig_b.bin" "$REF_B"
dd if="$T/seek_b_v2.bin" of="$REF_B" bs=1 seek=0 count=12032 conv=notrunc 2>/dev/null

python3 -c "
import os

# Session 1: write seek header v2
fd = os.open('$FILE_B', os.O_RDWR)
with open('$T/seek_b_v2.bin', 'rb') as f:
    data = f.read()
os.write(fd, data)
os.close(fd)

# Sessions 2, 3, 4: immediate re-opens with no writes
for i in range(3):
    fd = os.open('$FILE_B', os.O_RDWR)
    os.close(fd)

print('  File B: write + 3x immediate re-open/close done (no sleep)')
"

echo "  Waiting for flushes..."
sleep 1

compare_range "Phase5: File B has seek_b_v2 (last write wins)" 0 12032 "$FILE_B" "$REF_B"
compare_range "Phase5: File B data beyond header intact" 12032 $((CHUNK_SIZE - 12032)) "$FILE_B" "$REF_B"

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed"
echo "════════════════════════════════"
echo ""
if [ $FAIL -gt 0 ]; then
    echo "--- Client log (write/flush activity) ---"
    grep -E "PatchChunk|flush_buffer|open.*removing|PATCH slot|ino=3|ino=4" \
        "$LOG/client.log" 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' | tail -60
    # Save log for post-mortem
    cp "$LOG/client.log" /tmp/dfs-seek-client-last-fail.log 2>/dev/null && \
        echo "(full log saved to /tmp/dfs-seek-client-last-fail.log)"
fi
[ $FAIL -eq 0 ] && exit 0 || exit 1
