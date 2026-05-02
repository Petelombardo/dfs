#!/bin/bash
# Reproduces the Kodi seek bug: server-side LRU cache returns stale bytes
# after a same-id PatchChunk (50da77 → 50da77).
#
# The actual Kodi sequence from the staging log:
#   1. write 12032 bytes (header) → PatchChunk d9a8c3 → 50da77 (Kodi's seek table)
#   2. write 12032 bytes (same content) → PatchChunk 50da77 → 50da77 (HDHomeRun)
#      ↑ This rewrites the chunk file on disk but cache still has pre-patch bytes
#   3. Concurrent read during step 2 sees stale bytes from cache
#   4. Next write (Kodi's resume update) reads stale base, applies patch on top of OLD data
#   5. Result: Kodi's seek table edits are lost
#
# This test simulates the cache-staleness bug by:
#   - Writing initial header (h_v1)
#   - Doing a same-id PatchChunk (write h_v1 again — produces same hash)
#   - Modifying a byte that was changed in h_v1 to a NEW value (h_v2)
#   - Verifying the result is h_v2, not h_v1 (cache must be invalidated)

set -e
REPO=$(cd "$(dirname "$0")" && pwd)
BIN="$REPO/target/release"
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-cache-mount
LOG=/tmp/dfs-cache-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902"
CHUNK_SIZE=$((4 * 1024 * 1024))
T=/tmp/dfs-cache-tmp-$$
PASS=0; FAIL=0

check() {
    local name="$1" result="$2"
    if [ "$result" = "PASS" ]; then
        echo "  PASS: $name"; PASS=$((PASS+1))
    else
        echo "  FAIL: $name ($3)"; FAIL=$((FAIL+1))
    fi
}

teardown() {
    fusermount -u "$MOUNT" 2>/dev/null || true
    sleep 0.3
    pkill -f "dfs-server" 2>/dev/null || true
    sleep 0.5
    rm -rf "$BASE" "$LOG" "$MOUNT" "$T"
}
trap teardown EXIT

echo "=== Kodi cache staleness reproducer ==="
pkill -f "dfs-server" 2>/dev/null || true
fusermount -u "$MOUNT" 2>/dev/null || true
rm -rf "$BASE" "$LOG" "$MOUNT" "$T"
mkdir -p "$MOUNT" "$LOG" "$T"

echo "--- Build ---"
cd "$REPO" && cargo build --release 2>&1 | tail -2

echo "--- Start cluster ---"
bash "$REPO/scripts/setup-cluster.sh" 3 2>/dev/null
for i in 1 2 3; do
    RUST_LOG=dfs_server=info "$BIN/dfs-server" start \
        --config "$BASE/node${i}/config.toml" > "$LOG/server${i}.log" 2>&1 &
done
sleep 2

RUST_LOG=dfs_client=info "$BIN/dfs-client" mount "$MOUNT" \
    --cluster "$CLUSTER" --log-file "$LOG/client.log" --allow-other --log-level info &
sleep 1
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -20 "$LOG/client.log"; exit 1; }
echo "Mounted."

FILE="$MOUNT/recording.mpg"

# ── Phase 1: Create recording ─────────────────────────────────────────────────
echo ""
echo "=== Phase 1: Create initial recording ==="
dd if=/dev/urandom of="$T/orig.bin" bs="$CHUNK_SIZE" count=1 2>/dev/null
cp "$T/orig.bin" "$FILE"
sync; sleep 0.5

SIZE=$(stat -c%s "$FILE")
echo "  File size: $SIZE bytes"

# ── Phase 2: Write header v1 (creates a non-trivial chunk_id state) ──────────
echo ""
echo "=== Phase 2: Write header v1 ==="
dd if=/dev/urandom of="$T/h_v1.bin" bs=1 count=12032 2>/dev/null
cp "$T/orig.bin" "$T/ref.bin"
dd if="$T/h_v1.bin" of="$T/ref.bin" bs=1 seek=0 count=12032 conv=notrunc 2>/dev/null

dd if="$T/h_v1.bin" of="$FILE" bs=1 seek=0 count=12032 conv=notrunc 2>/dev/null
sleep 0.3

# ── Phase 3: Same-content rewrite (the cache staleness trigger) ───────────────
echo ""
echo "=== Phase 3: Same-content rewrite (PatchChunk same→same) ==="
# Write h_v1 AGAIN — same bytes, same resulting chunk_id
dd if="$T/h_v1.bin" of="$FILE" bs=1 seek=0 count=12032 conv=notrunc 2>/dev/null
sleep 0.3

# Read back the file at this point — should match h_v1
diff <(dd if="$FILE" bs=1 count=12032 2>/dev/null) "$T/h_v1.bin" >/dev/null 2>&1 \
    && check "Phase3: file content matches h_v1 after same-id rewrite" PASS \
    || check "Phase3: file content matches h_v1 after same-id rewrite" FAIL "content mismatch"

# ── Phase 4: Patch ONE BYTE in the header — simulates Kodi's resume update ───
echo ""
echo "=== Phase 4: Patch single byte (simulates Kodi resume update) ==="
# Change byte 100 to a known new value
NEW_BYTE=$'\xAB'
printf "$NEW_BYTE" | dd of="$FILE" bs=1 seek=100 count=1 conv=notrunc 2>/dev/null
sleep 0.5

# Read it back
GOT_BYTE=$(dd if="$FILE" bs=1 skip=100 count=1 2>/dev/null | xxd -p)
EXPECTED="ab"
if [ "$GOT_BYTE" = "$EXPECTED" ]; then
    check "Phase4: byte 100 = 0xAB after single-byte patch" PASS
else
    check "Phase4: byte 100 = 0xAB after single-byte patch" FAIL "got 0x$GOT_BYTE, expected 0x$EXPECTED"
fi

# ── Phase 5: Verify the rest of the header unchanged ──────────────────────────
echo ""
echo "=== Phase 5: Verify rest of header unchanged ==="
# Build the expected file: h_v1 with byte 100 = 0xAB
cp "$T/h_v1.bin" "$T/h_v2_expected.bin"
printf "$NEW_BYTE" | dd of="$T/h_v2_expected.bin" bs=1 seek=100 count=1 conv=notrunc 2>/dev/null

diff <(dd if="$FILE" bs=1 count=12032 2>/dev/null) "$T/h_v2_expected.bin" >/dev/null 2>&1 \
    && check "Phase5: header bytes 0-12032 = h_v1 with byte 100 changed" PASS \
    || check "Phase5: header bytes 0-12032 = h_v1 with byte 100 changed" FAIL "header content mismatch"

# ── Phase 6: Concurrent reads during rapid same-content rewrites ─────────────
echo ""
echo "=== Phase 6: Concurrent reads during same-content rewrites ==="
# This simulates the Kodi pattern: rapid O_RDONLY opens during in-flight write flush
python3 -c "
import os, threading, time

path = '$FILE'
h_v1 = open('$T/h_v1.bin', 'rb').read()

# Reset to h_v1
fd = os.open(path, os.O_RDWR)
os.write(fd, h_v1)
os.close(fd)
time.sleep(0.3)

errors = []
read_results = []

def reader():
    for _ in range(10):
        try:
            with open(path, 'rb') as f:
                data = f.read(12032)
                read_results.append(data == h_v1)
        except Exception as e:
            errors.append(str(e))
        time.sleep(0.005)

# Start concurrent readers, then do same-content writes
readers = [threading.Thread(target=reader) for _ in range(4)]
for r in readers: r.start()

for i in range(3):
    fd = os.open(path, os.O_RDWR)
    os.write(fd, h_v1)
    os.close(fd)
    time.sleep(0.01)

for r in readers: r.join()

print(f'  Reads succeeded: {sum(read_results)}/{len(read_results)}')
if errors:
    print(f'  Errors: {errors[:3]}')
    exit(1)
if not all(read_results):
    print('  FAIL: some reads returned wrong content during concurrent same-content writes')
    exit(1)
print('  PASS: all concurrent reads returned correct content')
"
if [ $? -eq 0 ]; then
    PASS=$((PASS+1))
else
    FAIL=$((FAIL+1))
fi

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed"
echo "════════════════════════════════"
[ $FAIL -eq 0 ] && exit 0 || exit 1
