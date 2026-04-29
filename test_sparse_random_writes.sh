#!/bin/bash
# Test: sparse file with sub-chunk random writes, mirrored on local filesystem.
# Every operation (truncate, write, read) is applied identically to both a local
# reference file and the DFS file. At each checkpoint, MD5s are compared.
# If they differ, the local file is the ground truth — DFS is wrong.
#
# Spins up a local 3-node cluster, runs the test, tears everything down.

set -e

REPO=$(cd "$(dirname "$0")" && pwd)
BIN="$REPO/target/release"
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-sparse-mount
LOG=/tmp/dfs-sparse-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902"
CHUNK_SIZE=$((4 * 1024 * 1024))
FILE_SIZE=$((8 * 1024 * 1024 * 1024))  # 8GB sparse file
DFS_FILE="$MOUNT/sparse_rw_test.raw"
LOCAL_FILE="/tmp/dfs-sparse-tmp-$$/local_ref.raw"
T=/tmp/dfs-sparse-tmp-$$
PASS=0; FAIL=0

check() {
    local name="$1" result="$2"
    if [ "$result" = "PASS" ]; then
        echo "  PASS: $name"; PASS=$((PASS+1))
    else
        echo "  FAIL: $name"; FAIL=$((FAIL+1))
    fi
}

# Mirror a dd write to both DFS and local reference simultaneously
mirror_write() {
    local src="$1" offset="$2" length="$3"
    dd if="$src" of="$DFS_FILE"   bs=1 seek="$offset" count="$length" conv=notrunc 2>/dev/null
    dd if="$src" of="$LOCAL_FILE" bs=1 seek="$offset" count="$length" conv=notrunc 2>/dev/null
}

# Compare a byte range between DFS and local reference
compare_range() {
    local label="$1" offset="$2" length="$3"
    local dfs_tmp="$T/cmp_dfs.bin" loc_tmp="$T/cmp_loc.bin"
    dd if="$DFS_FILE"   of="$dfs_tmp" bs=1 skip="$offset" count="$length" 2>/dev/null
    dd if="$LOCAL_FILE" of="$loc_tmp" bs=1 skip="$offset" count="$length" 2>/dev/null
    local m1 m2
    m1=$(md5sum "$loc_tmp" | cut -d' ' -f1)
    m2=$(md5sum "$dfs_tmp" | cut -d' ' -f1)
    if [ "$m1" = "$m2" ]; then
        check "$label" PASS
    else
        check "$label (local=$m1 dfs=$m2)" FAIL
        # Show first difference
        python3 -c "
a=open('$loc_tmp','rb').read()
b=open('$dfs_tmp','rb').read()
for i,(x,y) in enumerate(zip(a,b)):
    if x!=y:
        print(f'  first diff at byte {i}: local=0x{x:02x} dfs=0x{y:02x}')
        break
" 2>/dev/null || true
    fi
    rm -f "$dfs_tmp" "$loc_tmp"
}

# Compare full file MD5
compare_full() {
    local label="$1"
    local m1 m2
    m1=$(md5sum "$LOCAL_FILE" | cut -d' ' -f1)
    m2=$(md5sum "$DFS_FILE"   | cut -d' ' -f1)
    if [ "$m1" = "$m2" ]; then
        check "$label" PASS
    else
        check "$label (local=$m1 dfs=$m2)" FAIL
    fi
}

# ── Teardown ──────────────────────────────────────────────────────────────────
teardown() {
    echo ""
    echo "=== Teardown ==="
    fusermount -u "$MOUNT" 2>/dev/null || true
    sleep 0.3
    pkill -f "dfs-server" 2>/dev/null || true
    sleep 0.5
    rm -rf "$BASE" "$LOG" "$MOUNT" "$T" 2>/dev/null || true
    echo "Done."
}
trap teardown EXIT

# ── Build + cluster setup ─────────────────────────────────────────────────────
echo "=== Sparse File Mirror Test ==="
echo ""

echo "=== Building ==="
cd "$REPO" && cargo build --release 2>&1 | tail -2
echo ""

pkill -f "dfs-server" 2>/dev/null || true
fusermount -u "$MOUNT" 2>/dev/null || true
rm -rf "$BASE" "$LOG" "$MOUNT" "$T"
mkdir -p "$MOUNT" "$LOG" "$T"

echo "=== Starting 3-node cluster ==="
bash "$REPO/scripts/setup-cluster.sh" 3 2>/dev/null

for i in 1 2 3; do
    RUST_LOG=warn "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        > "$LOG/server${i}.log" 2>&1 &
done
sleep 3

RUST_LOG=warn "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level warn &
CLIENT_PID=$!
sleep 2

mountpoint -q "$MOUNT" || {
    echo "MOUNT FAILED"; tail -20 "$LOG/client.log"; exit 1
}
echo "Cluster running, DFS mounted at $MOUNT"
echo ""

# ── T1: Create sparse file ────────────────────────────────────────────────────
echo "=== T1: Create ${FILE_SIZE}-byte sparse file (mirrored) ==="
truncate -s "$FILE_SIZE" "$DFS_FILE"
truncate -s "$FILE_SIZE" "$LOCAL_FILE"

DFS_SIZE=$(stat -c%s "$DFS_FILE")
LOC_SIZE=$(stat -c%s "$LOCAL_FILE")
if [ "$DFS_SIZE" -eq "$FILE_SIZE" ] && [ "$LOC_SIZE" -eq "$FILE_SIZE" ]; then
    check "T1a sparse file size correct on DFS and local" PASS
else
    check "T1a size mismatch (dfs=$DFS_SIZE local=$LOC_SIZE want=$FILE_SIZE)" FAIL
fi
echo ""

# ── T2: Sub-chunk writes scattered across the file ───────────────────────────
echo "=== T2: Sub-chunk writes (mirrored) ==="

# Generate random data blobs for each write
BLOBS=()
BLOB_SIZES=(512 4096 8192 16384 512 4096 8192 4096 4096 512 4096 8192 65536 4096)
for sz in "${BLOB_SIZES[@]}"; do
    f="$T/blob_${sz}_$RANDOM.bin"
    dd if=/dev/urandom of="$f" bs=1 count="$sz" 2>/dev/null
    BLOBS+=("$f")
done

declare -a OFFSETS LABELS
OFFSETS=(
    512                           # MBR/GPT header — chunk 0
    4096                          # superblock area — chunk 0
    $((32 * 1024))                # 32KB in — chunk 0
    $((1024 * 1024))              # 1MB in — chunk 0
    $((CHUNK_SIZE - 512))         # last 512B of chunk 0
    $CHUNK_SIZE                   # chunk 1 start
    $((CHUNK_SIZE + 4096))        # 4KB into chunk 1
    $((CHUNK_SIZE * 2 + 65536))   # chunk 2, 64KB in
    $((FILE_SIZE - 4096))         # near EOF
    $((FILE_SIZE - 512))          # very last 512B
    8192                          # 2nd write to chunk 0 (PatchChunk chain)
    $((CHUNK_SIZE + 131072))      # 2nd write to chunk 1
    $((CHUNK_SIZE * 5 + 1024))    # chunk 5
    $((CHUNK_SIZE * 10))          # chunk 10 start
)
LABELS=(
    "MBR/GPT header (chunk 0)"
    "superblock area (chunk 0)"
    "32KB in chunk 0"
    "1MB mid-chunk 0"
    "last 512B chunk 0"
    "chunk 1 start"
    "4KB into chunk 1"
    "chunk 2, 64KB in"
    "near EOF"
    "very last 512B"
    "2nd write chunk 0 (PatchChunk chain)"
    "2nd write chunk 1"
    "chunk 5"
    "chunk 10 start"
)

N=${#OFFSETS[@]}
for i in $(seq 0 $((N-1))); do
    OFF=${OFFSETS[$i]}
    BLOB="${BLOBS[$i]}"
    LEN=$(stat -c%s "$BLOB")
    CHUNK=$((OFF / CHUNK_SIZE))
    printf "    [%2d] offset=%12d len=%6d chunk=%d  %s\n" \
           "$i" "$OFF" "$LEN" "$CHUNK" "${LABELS[$i]}"
    mirror_write "$BLOB" "$OFF" "$LEN"
done
echo ""

# ── T3: Sync and verify each write individually ───────────────────────────────
echo "=== T3: Per-write read-back verification ==="
sync
sleep 2

for i in $(seq 0 $((N-1))); do
    OFF=${OFFSETS[$i]}
    LEN=$(stat -c%s "${BLOBS[$i]}")
    compare_range "T3[$i] ${LABELS[$i]}" "$OFF" "$LEN"
done
echo ""

# ── T4: Full file MD5 ─────────────────────────────────────────────────────────
# Note: for 8GB this is slow; we compare only the written regions and holes.
echo "=== T4: Spot-check sparse holes return zeros ==="
# Holes: regions we never wrote to — should be zero on both local and DFS
HOLE_OFFSETS=(
    $((2 * 1024 * 1024))
    $((CHUNK_SIZE * 3))
    $((100 * 1024 * 1024))
    $((1024 * 1024 * 1024))
)
HOLE_LABELS=(
    "2MB (unwritten, within chunk 0)"
    "chunk 3 start (unwritten)"
    "100MB (deep sparse)"
    "1GB (deep sparse)"
)
for i in "${!HOLE_OFFSETS[@]}"; do
    compare_range "T4[$i] hole: ${HOLE_LABELS[$i]}" "${HOLE_OFFSETS[$i]}" 4096
done
echo ""

# ── T5: Overwrite existing written regions (second PatchChunk layer) ──────────
echo "=== T5: Overwrite previously-written regions ==="
OW_OFFSETS=(
    512                        # overwrite chunk 0 MBR area again
    $((4096 + 512))            # overwrite into superblock area
    $CHUNK_SIZE                # overwrite chunk 1 start again
)
OW_LABELS=(
    "re-overwrite MBR area (chunk 0)"
    "re-overwrite superblock+512 (chunk 0)"
    "re-overwrite chunk 1 start"
)
OW_BLOBS=()
for j in 0 1 2; do
    f="$T/ow_blob_$j.bin"
    dd if=/dev/urandom of="$f" bs=1 count=512 2>/dev/null
    OW_BLOBS+=("$f")
done

for j in "${!OW_OFFSETS[@]}"; do
    OFF=${OW_OFFSETS[$j]}
    BLOB="${OW_BLOBS[$j]}"
    LEN=$(stat -c%s "$BLOB")
    printf "    [%d] offset=%12d len=%6d  %s\n" "$j" "$OFF" "$LEN" "${OW_LABELS[$j]}"
    mirror_write "$BLOB" "$OFF" "$LEN"
done

sync
sleep 2

for j in "${!OW_OFFSETS[@]}"; do
    OFF=${OW_OFFSETS[$j]}
    LEN=$(stat -c%s "${OW_BLOBS[$j]}")
    compare_range "T5[$j] ${OW_LABELS[$j]}" "$OFF" "$LEN"
done

# Also re-verify original writes weren't disturbed by the overwrites
echo "  Re-checking adjacent original writes..."
compare_range "T5 chunk 0 superblock still intact" 4096 4096
compare_range "T5 chunk 1 4KB region still intact" $((CHUNK_SIZE + 4096)) 8192
echo ""

# ── T6: Remount and re-verify persistence ────────────────────────────────────
echo "=== T6: Remount + persistence ==="
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 0.5
kill $CLIENT_PID 2>/dev/null || true
sleep 1

RUST_LOG=warn "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client2.log" --allow-other --log-level warn &
sleep 2
mountpoint -q "$MOUNT" || { echo "REMOUNT FAILED"; tail -20 "$LOG/client2.log"; exit 1; }
echo "  Remounted."

DFS_SIZE=$(stat -c%s "$DFS_FILE")
[ "$DFS_SIZE" -eq "$FILE_SIZE" ] \
    && check "T6a size persists after remount" PASS \
    || check "T6a size after remount (got $DFS_SIZE, want $FILE_SIZE)" FAIL

# Spot-check a sample of writes after remount
for i in 0 1 4 5 10 13; do
    OFF=${OFFSETS[$i]}
    LEN=$(stat -c%s "${BLOBS[$i]}")
    compare_range "T6b[$i] ${LABELS[$i]} persists" "$OFF" "$LEN"
done

# Spot-check a hole after remount
compare_range "T6c sparse hole still zero after remount" $((100 * 1024 * 1024)) 4096
echo ""

# ── Summary ───────────────────────────────────────────────────────────────────
echo "════════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed"
echo "════════════════════════════════════════════"
[ $FAIL -eq 0 ] && exit 0 || exit 1
