#!/bin/bash
# Local integration test suite: write, read, delete, partial writes, rename, remount persistence, metadata consistency.
set -e

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-mount
LOG=/tmp/dfs-test-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
PASS=0; FAIL=0; T=/tmp/dfs-suite-tmp-$$

check() {
    local name="$1" result="$2"
    if [ "$result" = "PASS" ]; then echo "  PASS: $name"; PASS=$((PASS+1))
    else echo "  FAIL: $name"; FAIL=$((FAIL+1)); fi
}

# ── cleanup ──────────────────────────────────────────────────────────────────
pkill -f "dfs-server" 2>/dev/null || true
pkill -f "dfs-client" 2>/dev/null || true
sleep 0.5
fusermount -u $MOUNT 2>/dev/null || true
sudo rm -rf $BASE $LOG $MOUNT $T 2>/dev/null || rm -rf $BASE $LOG $MOUNT $T 2>/dev/null || true
mkdir -p $MOUNT $LOG $T

echo "=== Building ==="
cd "$REPO" && cargo build --release 2>&1 | tail -2

echo "=== Starting 5-node cluster ==="
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null
for i in 1 2 3 4 5; do
    RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        > "$LOG/server${i}.log" 2>&1 &
done
sleep 3

RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level info &
CLIENT_PID=$!
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -20 "$LOG/client.log"; exit 1; }
echo "Mounted. Running tests..."
echo ""

# ── Test 1: small write + read ────────────────────────────────────────────────
echo "=== T1: small write/read ==="
echo "hello distributed world" > "$MOUNT/t1.txt"
GOT=$(cat "$MOUNT/t1.txt")
[ "$GOT" = "hello distributed world" ] && check "T1 small write/read" PASS || check "T1 small write/read (got: $GOT)" FAIL

# ── Test 2: 2MB write + read ──────────────────────────────────────────────────
echo "=== T2: 2MB write/read ==="
dd if=/dev/urandom of="$T/big.bin" bs=1M count=2 2>/dev/null
cp "$T/big.bin" "$MOUNT/t2.bin"
cp "$MOUNT/t2.bin" "$T/big_read.bin"
m1=$(md5sum "$T/big.bin"     | cut -d' ' -f1)
m2=$(md5sum "$T/big_read.bin"| cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T2 2MB write/read" PASS || check "T2 2MB write/read (exp $m1 got $m2)" FAIL

# ── Test 3: delete vanishes immediately ───────────────────────────────────────
echo "=== T3: delete ==="
echo "delete me" > "$MOUNT/t3_del.txt"
rm "$MOUNT/t3_del.txt"
[ ! -f "$MOUNT/t3_del.txt" ] && check "T3 delete vanishes" PASS || check "T3 delete vanishes" FAIL

# ── Test 4: delete stays gone ─────────────────────────────────────────────────
echo "=== T4: delete stays gone after 3s ==="
sleep 3
[ ! -f "$MOUNT/t3_del.txt" ] && check "T4 delete stays gone" PASS || check "T4 delete stays gone" FAIL

# ── Test 5: delete + recreate same path ───────────────────────────────────────
echo "=== T5: delete+recreate ==="
echo "v1" > "$MOUNT/t5.txt"
rm "$MOUNT/t5.txt"
sleep 0.3
echo "v2" > "$MOUNT/t5.txt"
GOT=$(cat "$MOUNT/t5.txt")
[ "$GOT" = "v2" ] && check "T5 delete+recreate" PASS || check "T5 delete+recreate (got: $GOT)" FAIL

# ── Test 6: selective delete ──────────────────────────────────────────────────
echo "=== T6: selective delete ==="
for i in 1 2 3 4 5; do echo "file$i" > "$MOUNT/t6_$i.txt"; done
rm "$MOUNT/t6_2.txt" "$MOUNT/t6_4.txt"
sleep 0.5
OK=PASS
[ -f "$MOUNT/t6_1.txt" ] || OK=FAIL
[ ! -f "$MOUNT/t6_2.txt" ] || OK=FAIL
[ -f "$MOUNT/t6_3.txt" ] || OK=FAIL
[ ! -f "$MOUNT/t6_4.txt" ] || OK=FAIL
[ -f "$MOUNT/t6_5.txt" ] || OK=FAIL
check "T6 selective delete" $OK

# ── Test 7: overwrite ─────────────────────────────────────────────────────────
echo "=== T7: overwrite ==="
echo "original" > "$MOUNT/t7.txt"
echo "overwritten" > "$MOUNT/t7.txt"
GOT=$(cat "$MOUNT/t7.txt")
[ "$GOT" = "overwritten" ] && check "T7 overwrite" PASS || check "T7 overwrite (got: $GOT)" FAIL

# ── Test 8: unmount + remount persistence ─────────────────────────────────────
echo ""
echo "=== T8: unmount + remount persistence ==="
echo "persistent data" > "$MOUNT/t8_persist.txt"
dd if=/dev/urandom of="$T/persist_big.bin" bs=1M count=1 2>/dev/null
cp "$T/persist_big.bin" "$MOUNT/t8_big.bin"
PERSIST_MD5=$(md5sum "$T/persist_big.bin" | cut -d' ' -f1)
sync

echo "  Unmounting..."
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 0.5
kill $CLIENT_PID 2>/dev/null || true
sleep 1

echo "  Remounting..."
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client2.log" --allow-other --log-level info &
CLIENT_PID2=$!
sleep 2
mountpoint -q "$MOUNT" || { echo "REMOUNT FAILED"; tail -20 "$LOG/client2.log"; exit 1; }

GOT=$(cat "$MOUNT/t8_persist.txt" 2>/dev/null)
[ "$GOT" = "persistent data" ] && check "T8a text persists after remount" PASS || check "T8a text persists (got: $GOT)" FAIL

cp "$MOUNT/t8_big.bin" "$T/persist_big_read.bin"
m1=$(md5sum "$T/persist_big.bin"      | cut -d' ' -f1)
m2=$(md5sum "$T/persist_big_read.bin" | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T8b 1MB persists after remount" PASS || check "T8b 1MB persists (exp $m1 got $m2)" FAIL

[ ! -f "$MOUNT/t3_del.txt" ] && check "T8c deleted file still gone after remount" PASS || check "T8c deleted file reappeared after remount" FAIL

# ── Test 9: partial write — sub-chunk (< 4MB) write + read ───────────────────
echo ""
echo "=== T9: partial write (sub-chunk) ==="
dd if=/dev/urandom of="$T/partial.bin" bs=1M count=1 2>/dev/null
cp "$T/partial.bin" "$MOUNT/t9_partial.bin"
sleep 0.2
cp "$MOUNT/t9_partial.bin" "$T/partial_read.bin"
m1=$(md5sum "$T/partial.bin"      | cut -d' ' -f1)
m2=$(md5sum "$T/partial_read.bin" | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T9a 1MB partial write/read" PASS || check "T9a 1MB partial write/read (exp $m1 got $m2)" FAIL

# Sub-chunk write that lands mid-chunk: 100KB
dd if=/dev/urandom of="$T/tiny.bin" bs=1K count=100 2>/dev/null
cp "$T/tiny.bin" "$MOUNT/t9_tiny.bin"
sleep 0.2
cp "$MOUNT/t9_tiny.bin" "$T/tiny_read.bin"
m1=$(md5sum "$T/tiny.bin"      | cut -d' ' -f1)
m2=$(md5sum "$T/tiny_read.bin" | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T9b 100KB partial write/read" PASS || check "T9b 100KB partial write/read (exp $m1 got $m2)" FAIL

# ── Test 10: cross-chunk boundary write (> 4MB, < 8MB) ───────────────────────
echo ""
echo "=== T10: cross-chunk boundary write (6MB) ==="
dd if=/dev/urandom of="$T/cross.bin" bs=1M count=6 2>/dev/null
cp "$T/cross.bin" "$MOUNT/t10_cross.bin"
sleep 0.3
cp "$MOUNT/t10_cross.bin" "$T/cross_read.bin"
m1=$(md5sum "$T/cross.bin"      | cut -d' ' -f1)
m2=$(md5sum "$T/cross_read.bin" | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T10 6MB cross-chunk write/read" PASS || check "T10 6MB cross-chunk write/read (exp $m1 got $m2)" FAIL

# ── Test 11: append to existing file ─────────────────────────────────────────
echo ""
echo "=== T11: append ==="
echo "first line" > "$MOUNT/t11_append.txt"
sleep 1  # wait for flush + metadata commit so kernel gets correct file size for O_APPEND
echo "second line" >> "$MOUNT/t11_append.txt"
sleep 0.5
GOT=$(cat "$MOUNT/t11_append.txt")
EXPECTED=$'first line\nsecond line'
[ "$GOT" = "$EXPECTED" ] && check "T11 append to file" PASS || check "T11 append to file (got: $(echo $GOT | head -c 60))" FAIL

# ── Test 12: rename — new path readable, old path gone ───────────────────────
echo ""
echo "=== T12: rename ==="
echo "rename me" > "$MOUNT/t12_before.txt"
sleep 1
mv "$MOUNT/t12_before.txt" "$MOUNT/t12_after.txt"
sleep 0.5
GOT=$(cat "$MOUNT/t12_after.txt" 2>/dev/null)
[ "$GOT" = "rename me" ] && check "T12a renamed file readable at new path" PASS || check "T12a renamed file readable (got: $GOT)" FAIL
[ ! -f "$MOUNT/t12_before.txt" ] && check "T12b old path gone after rename" PASS || check "T12b old path still exists after rename" FAIL

# ── Test 13: rename a binary file, verify data integrity ─────────────────────
echo ""
echo "=== T13: rename binary file ==="
dd if=/dev/urandom of="$T/rename_src.bin" bs=1M count=2 2>/dev/null
cp "$T/rename_src.bin" "$MOUNT/t13_src.bin"
sleep 1
mv "$MOUNT/t13_src.bin" "$MOUNT/t13_dst.bin"
sleep 0.5
cp "$MOUNT/t13_dst.bin" "$T/rename_dst_read.bin"
m1=$(md5sum "$T/rename_src.bin"      | cut -d' ' -f1)
m2=$(md5sum "$T/rename_dst_read.bin" | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T13a renamed binary data intact" PASS || check "T13a renamed binary data (exp $m1 got $m2)" FAIL
[ ! -f "$MOUNT/t13_src.bin" ] && check "T13b src gone after rename" PASS || check "T13b src still exists after rename" FAIL

# ── Test 14: rename + metadata consistency across nodes ──────────────────────
echo ""
echo "=== T14: metadata consistency after renames ==="
sleep 3  # let dissemination propagate

# Verify t12_after.txt appears on all nodes with correct path
OK=PASS
for port in 8900 8901 8902 8903 8904; do
    LIST=$("$BIN/dfs-admin" --cluster "127.0.0.1:$port" file list 2>/dev/null)
    echo "$LIST" | grep -q "t12_after.txt" || { OK=FAIL; echo "  Node $port missing t12_after.txt"; }
    echo "$LIST" | grep -q "t12_before.txt" && { OK=FAIL; echo "  Node $port still has t12_before.txt"; }
    echo "$LIST" | grep -q "t13_dst.bin"   || { OK=FAIL; echo "  Node $port missing t13_dst.bin"; }
    echo "$LIST" | grep -q "t13_src.bin"   && { OK=FAIL; echo "  Node $port still has t13_src.bin"; }
done
check "T14a rename paths propagated to all nodes" $OK

# Verify all nodes agree on the full file list (same set of files)
declare -A NODE_LISTS
for port in 8900 8901 8902 8903 8904; do
    NODE_LISTS[$port]=$("$BIN/dfs-admin" --cluster "127.0.0.1:$port" file list 2>/dev/null \
        | grep -E "^[0-9a-f]{8}" | awk '{print $1, $2, $3}' | sort)
done

if [ "${NODE_LISTS[8900]}" = "${NODE_LISTS[8901]}" ] && \
   [ "${NODE_LISTS[8901]}" = "${NODE_LISTS[8902]}" ] && \
   [ "${NODE_LISTS[8902]}" = "${NODE_LISTS[8903]}" ] && \
   [ "${NODE_LISTS[8903]}" = "${NODE_LISTS[8904]}" ]; then
    check "T14b metadata identical on all 5 nodes" PASS
else
    check "T14b metadata identical on all 5 nodes" FAIL
    for port in 8900 8901 8902 8903 8904; do
        echo "  Node $port:"
        echo "${NODE_LISTS[$port]}" | sed 's/^/    /'
    done
fi

echo ""
echo "  Current file list (from node 8900):"
"$BIN/dfs-admin" --cluster "127.0.0.1:8900" file list 2>/dev/null | grep -E "^[0-9a-f]|Total" | sed 's/^/    /'

# ── Test 15: partial in-place overwrite (DVR header-update pattern) ──────────
# Create a 4MB file. Overwrite the first 2MB with new data (no truncation).
# Result must still be 4MB, and the final content must match the same op on
# the local filesystem (first 2MB = patch, last 2MB = original tail).
echo ""
echo "=== T15: partial in-place overwrite (4MB file, patch first 2MB) ==="
dd if=/dev/urandom of="$T/t15_orig.bin"  bs=1M count=4 2>/dev/null
dd if=/dev/urandom of="$T/t15_patch.bin" bs=1M count=2 2>/dev/null

# Build the expected result on the local filesystem (no DFS involved)
cp "$T/t15_orig.bin" "$T/t15_expected.bin"
dd if="$T/t15_patch.bin" of="$T/t15_expected.bin" bs=1M count=2 conv=notrunc 2>/dev/null

# Write orig to DFS, then patch first 2MB in-place (conv=notrunc)
cp "$T/t15_orig.bin" "$MOUNT/t15_patch.bin"
sleep 1   # wait for flush + metadata commit
dd if="$T/t15_patch.bin" of="$MOUNT/t15_patch.bin" bs=1M count=2 conv=notrunc 2>/dev/null
sleep 0.5
cp "$MOUNT/t15_patch.bin" "$T/t15_read.bin"

READ_SIZE=$(stat -c%s "$T/t15_read.bin")
EXP_SIZE=$(stat -c%s "$T/t15_expected.bin")
[ "$READ_SIZE" = "$EXP_SIZE" ] && check "T15a partial overwrite size correct (4MB)" PASS \
    || check "T15a partial overwrite size (got $READ_SIZE, exp $EXP_SIZE)" FAIL

m1=$(md5sum "$T/t15_expected.bin" | cut -d' ' -f1)
m2=$(md5sum "$T/t15_read.bin"     | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T15b partial overwrite data intact" PASS \
    || check "T15b partial overwrite data (exp $m1 got $m2)" FAIL

# ── Test 16: full replace via O_TRUNC (cp smaller file over larger) ───────────
echo ""
echo "=== T16: O_TRUNC replace (3MB → 1MB) ==="
dd if=/dev/urandom of="$T/t16_big.bin"   bs=1M count=3 2>/dev/null
dd if=/dev/urandom of="$T/t16_small.bin" bs=1M count=1 2>/dev/null
cp "$T/t16_big.bin" "$MOUNT/t16_trunc.bin"
sleep 1
cp "$T/t16_small.bin" "$MOUNT/t16_trunc.bin"   # cp uses O_TRUNC
sleep 0.5
cp "$MOUNT/t16_trunc.bin" "$T/t16_read.bin"

READ_SIZE=$(stat -c%s "$T/t16_read.bin")
EXP_SIZE=$(stat -c%s "$T/t16_small.bin")
[ "$READ_SIZE" = "$EXP_SIZE" ] && check "T16a O_TRUNC replace size correct (1MB)" PASS \
    || check "T16a O_TRUNC replace size (got $READ_SIZE, exp $EXP_SIZE)" FAIL

m1=$(md5sum "$T/t16_small.bin" | cut -d' ' -f1)
m2=$(md5sum "$T/t16_read.bin"  | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T16b O_TRUNC replace data intact" PASS \
    || check "T16b O_TRUNC replace data (exp $m1 got $m2)" FAIL

# ── Test 17: concurrent read while writing (deadlock regression) ──────────────
# Simulates DVR: write a large file while concurrently reading from it (same client).
# Before the fix, holding the write-buffer mutex across PatchChunk network I/O
# caused concurrent reads/getattrs on the same inode to stall indefinitely.
#echo "=== T17: concurrent read while writing (deadlock regression) ==="
#dd if=/dev/urandom of="$T/t17_seed.bin" bs=1M count=8
#cp -v "$T/t17_seed.bin" "$MOUNT/t17_concurrent.bin"
#sleep 0.5

# Generate writer chunks locally (no FUSE blocking), then cp each to mount.
# Using cp rather than dd-append avoids a stuck kernel write if FUSE deadlocks —
# cp can be killed cleanly, dd in append mode cannot (blocks in kernel).
#dd if=/dev/urandom of="$T/t17_chunk.bin" bs=1M count=8 2>/dev/null
#    for i in $(seq 1 4); do
#        timeout 10 cp "$T/t17_chunk.bin" "$MOUNT/t17_write_$i.bin" 2>/dev/null || true
#        sleep 0.2
#    done
#WRITER_PID=$!

# Concurrently read from the file; must complete within 15s (not deadlock)
#READ_OK=true
#for i in $(seq 1 6); do
#    if ! timeout 15 dd if="$MOUNT/t17_concurrent.bin" of=/dev/null bs=1M 2>/dev/null; then
#        READ_OK=false
#        break
#    fi
#    sleep 0.3
#done
# Kill writer and any stuck subprocesses; wait won't hang since cp has timeout
#kill $WRITER_PID 2>/dev/null
#wait $WRITER_PID 2>/dev/null || true

#$READ_OK && check "T17 concurrent read while writing (no deadlock)" PASS \
#         || check "T17 concurrent read while writing (DEADLOCK or timeout)" FAIL

# ── Test 17: DVR header-update pattern (full-chunk gap-fill corruption) ───────
# Write exactly 4MB (one full chunk). Then do a small header update at offset 0
# (conv=notrunc). The tail of the file must not be zeroed out.
# This catches the gap_filled_prefix bug: when the slot fills to CHUNK_SIZE,
# needs_patch was false and the full slot (with gap-fill zeros) was sent as a
# fresh WriteData, overwriting real server data with zeros.
echo ""
echo "=== T17: DVR header-update (4MB file, small patch at offset 0) ==="
dd if=/dev/urandom of="$T/t17_orig.bin" bs=1M count=4 2>/dev/null
dd if=/dev/urandom of="$T/t17_hdr.bin"  bs=1K count=12 2>/dev/null

# Expected: first 12KB = header, rest = original tail
cp "$T/t17_orig.bin" "$T/t17_expected.bin"
dd if="$T/t17_hdr.bin" of="$T/t17_expected.bin" bs=1K count=12 conv=notrunc 2>/dev/null

# Write 4MB to DFS, flush, then update header
cp "$T/t17_orig.bin" "$MOUNT/t17_dvr.bin"
sleep 2  # ensure chunk 0 is flushed and flushed_sizes[0] is set
dd if="$T/t17_hdr.bin" of="$MOUNT/t17_dvr.bin" bs=1K count=12 conv=notrunc 2>/dev/null
sleep 1
cp "$MOUNT/t17_dvr.bin" "$T/t17_read.bin"

READ_SIZE=$(stat -c%s "$T/t17_read.bin")
EXP_SIZE=$(stat -c%s "$T/t17_expected.bin")
[ "$READ_SIZE" = "$EXP_SIZE" ] && check "T17a DVR header-update size correct (4MB)" PASS \
    || check "T17a DVR header-update size (got $READ_SIZE, exp $EXP_SIZE)" FAIL

m1=$(md5sum "$T/t17_expected.bin" | cut -d' ' -f1)
m2=$(md5sum "$T/t17_read.bin"     | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T17b DVR header-update data intact (tail not zeroed)" PASS \
    || check "T17b DVR header-update data (exp $m1 got $m2)" FAIL

# ── Test 17c: DVR exact write pattern (12KB header then fill to 4MB) ──────────
# Simulates exact HDHomeRun DVR sequence: write 12KB header first (fresh chunk),
# then write recording data that fills chunk 0 to exactly 4MB via background ticker.
# Verifies the tail is not zeroed when the slot fills to CHUNK_SIZE with gap-fill.
echo ""
echo "=== T17c: DVR exact pattern (12KB header + fill to 4MB via background ticker) ==="
HEADER_SIZE=12032
CHUNK_BYTES=$((4*1024*1024))
TAIL_SIZE=$((CHUNK_BYTES - HEADER_SIZE))

dd if=/dev/urandom of="$T/t17c_header.bin"    bs=1k count=$(($HEADER_SIZE/1024)) 2>/dev/null
dd if=/dev/urandom of="$T/t17c_recording.bin" bs=1k count=$((TAIL_SIZE/1024))   2>/dev/null
cat "$T/t17c_header.bin" "$T/t17c_recording.bin" > "$T/t17c_expected.bin"

# Step 1: write 12KB header — creates fresh 12032-byte chunk on server
dd if="$T/t17c_header.bin" of="$MOUNT/t17c_dvr.bin" bs=1k count=$(($HEADER_SIZE/1024)) 2>/dev/null
sleep 1  # let background ticker flush the 12KB, setting flushed_sizes[0]=12032

# Step 2: write recording data at offset 12032 — slot grows to 4MB, ticker flushes via PatchChunk
dd if="$T/t17c_recording.bin" of="$MOUNT/t17c_dvr.bin" bs=1k seek=$(($HEADER_SIZE/1024)) count=$(($TAIL_SIZE/1024)) conv=notrunc 2>/dev/null
sleep 2  # let background ticker flush the extended slot

sync
cp "$MOUNT/t17c_dvr.bin" "$T/t17c_read.bin"

m1=$(md5sum "$T/t17c_expected.bin" | cut -d' ' -f1)
m2=$(md5sum "$T/t17c_read.bin"     | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T17c DVR exact pattern: header+recording intact" PASS \
    || check "T17c DVR exact pattern: data mismatch (exp $m1 got $m2)" FAIL

# ── Test 18: DVR concurrent-read integrity ────────────────────────────────────
# Write a 20MB file at ~4MB/s while concurrently reading from offset 0.
# Verifies: no short reads that skip data, read copy matches written data.
sleep 2
echo "=== T18: DVR concurrent-read integrity ==="
WRITE_SIZE_MB=16
CHUNK_SIZE_BYTES=$((4 * 1024 * 1024))
T18_SRC="$T/t18_src.bin"
T18_DST="$MOUNT/t18_dvr.bin"
T18_REF="$T/t18_ref.bin"
T18_COPY="$T/t18_copy.bin"

# Generate source data locally
dd if=/dev/urandom of="$T18_SRC" bs=1M count="$WRITE_SIZE_MB" 2>/dev/null
cp "$T18_SRC" "$T18_REF"

# Writer: copy source to DFS using dd in 128KB blocks at ~4MB/s
(
    dd if="$T18_SRC" of="$T18_DST" bs=131072 2>/dev/null
) &
T18_WRITER=$!

# Reader: start after 1s, read sequentially tracking actual bytes received
sleep 1
(
    BYTES_READ=0
    TOTAL=$(( WRITE_SIZE_MB * 1024 * 1024 ))
    DEADLINE=$(( $(date +%s) + 30 ))
    while [ "$BYTES_READ" -lt "$TOTAL" ] && [ "$(date +%s)" -lt "$DEADLINE" ]; do
        DFS_SIZE=$(stat -c%s "$T18_DST" 2>/dev/null || echo 0)
        AVAIL=$(( DFS_SIZE - BYTES_READ ))
        if [ "$AVAIL" -ge 4096 ]; then
            PAGES=$(( AVAIL / 4096 ))
            BEFORE=$(stat -c%s "$T18_COPY" 2>/dev/null || echo 0)
            dd if="$T18_DST" bs=4096 skip=$(( BYTES_READ / 4096 )) count="$PAGES" \
               2>/dev/null >> "$T18_COPY"
            AFTER=$(stat -c%s "$T18_COPY" 2>/dev/null || echo 0)
            BYTES_READ=$(( BYTES_READ + AFTER - BEFORE ))
        else
            sleep 0.05
        fi
    done
    # drain tail after writer
    sleep 0.5
    DFS_SIZE=$(stat -c%s "$T18_DST" 2>/dev/null || echo 0)
    REMAINING=$(( DFS_SIZE - BYTES_READ ))
    if [ "$REMAINING" -gt 0 ]; then
        dd if="$T18_DST" bs=4096 skip=$(( BYTES_READ / 4096 )) \
           count=$(( (REMAINING + 4095) / 4096 )) 2>/dev/null >> "$T18_COPY"
    fi
) &
T18_READER=$!

wait "$T18_WRITER"
wait "$T18_READER"

# Compare first N complete chunks of the reference vs read copy
REF_SIZE=$(stat -c%s "$T18_REF" 2>/dev/null || echo 0)
COPY_SIZE=$(stat -c%s "$T18_COPY" 2>/dev/null || echo 0)
CMP_BYTES=$(( (COPY_SIZE / CHUNK_SIZE_BYTES) * CHUNK_SIZE_BYTES ))

if [ "$CMP_BYTES" -eq 0 ]; then
    check "T18 DVR concurrent-read (read copy empty)" FAIL
else
    T18_MD5_REF=$(dd if="$T18_REF"  bs="$CHUNK_SIZE_BYTES" count=$(( CMP_BYTES / CHUNK_SIZE_BYTES )) 2>/dev/null | md5sum | cut -d' ' -f1)
    T18_MD5_CPY=$(dd if="$T18_COPY" bs="$CHUNK_SIZE_BYTES" count=$(( CMP_BYTES / CHUNK_SIZE_BYTES )) 2>/dev/null | md5sum | cut -d' ' -f1)
    # size check: copy should be within one chunk of reference
    SIZE_OK=false
    [ "$COPY_SIZE" -ge $(( REF_SIZE - CHUNK_SIZE_BYTES )) ] && SIZE_OK=true
    if [ "$T18_MD5_REF" = "$T18_MD5_CPY" ] && $SIZE_OK; then
        check "T18 DVR concurrent-read integrity" PASS
    else
        check "T18 DVR concurrent-read integrity (ref_size=$REF_SIZE copy_size=$COPY_SIZE cmp_bytes=$CMP_BYTES)" FAIL
    fi
fi
rm -f "$T18_DST"

./scripts/test_dvr_stream.sh && check "DVR stream integrity (write+live read)" PASS \
    || check "DVR stream integrity (write+live read)" FAIL

# ── Test 20: partial overwrite integrity — first, middle, and last chunk ───────
# Write a 12MB file (3 chunks). Write a 2MB patch file.
# Apply the 2MB patch to: first 2MB of chunk 0, first 2MB of chunk 1, first 2MB of chunk 2.
# Mirror every operation on the local filesystem, then compare MD5s chunk-by-chunk.
echo ""
echo "=== T20: partial overwrite — start, middle, end chunk ==="
CHUNK=$((4*1024*1024))
PATCH_SIZE=$((2*1024*1024))

dd if=/dev/urandom of="$T/t20_orig.bin"  bs=1M count=12 2>/dev/null
dd if=/dev/urandom of="$T/t20_patch.bin" bs=1M count=2  2>/dev/null

# Build expected result locally
cp "$T/t20_orig.bin" "$T/t20_expected.bin"
dd if="$T/t20_patch.bin" of="$T/t20_expected.bin" bs=1M count=2 seek=0            conv=notrunc 2>/dev/null  # chunk 0
dd if="$T/t20_patch.bin" of="$T/t20_expected.bin" bs=1M count=2 seek=4            conv=notrunc 2>/dev/null  # chunk 1 start
dd if="$T/t20_patch.bin" of="$T/t20_expected.bin" bs=1M count=2 seek=8            conv=notrunc 2>/dev/null  # chunk 2 start

# Write original to DFS
cp "$T/t20_orig.bin" "$MOUNT/t20_test.bin" || true
sleep 1

# Apply same patches to DFS file
dd if="$T/t20_patch.bin" of="$MOUNT/t20_test.bin" bs=1M count=2 seek=0            conv=notrunc 2>/dev/null || true  # chunk 0
dd if="$T/t20_patch.bin" of="$MOUNT/t20_test.bin" bs=1M count=2 seek=4            conv=notrunc 2>/dev/null || true  # chunk 1 start
dd if="$T/t20_patch.bin" of="$MOUNT/t20_test.bin" bs=1M count=2 seek=8            conv=notrunc 2>/dev/null || true  # chunk 2 start
sleep 1

cp "$MOUNT/t20_test.bin" "$T/t20_read.bin" || true

m1=$(md5sum "$T/t20_expected.bin" | cut -d' ' -f1)
m2=$(md5sum "$T/t20_read.bin"     | cut -d' ' -f1)
if [ "$m1" = "$m2" ]; then
    check "T20 partial overwrite: start/middle/end chunks intact" PASS
else
    check "T20 partial overwrite: mismatch — checking per-chunk" FAIL
    for chunk in 0 1 2; do
        off=$(( chunk * 4 ))
        e=$(dd if="$T/t20_expected.bin" bs=1M skip=$off count=4 2>/dev/null | md5sum | cut -d' ' -f1)
        g=$(dd if="$T/t20_read.bin"     bs=1M skip=$off count=4 2>/dev/null | md5sum | cut -d' ' -f1)
        [ "$e" = "$g" ] && echo "  chunk $chunk: OK" || echo "  chunk $chunk: MISMATCH (exp $e got $g)"
    done
fi

# ── Test 19: large-file delete — non-blocking rm + async chunk cleanup ────────
echo ""
echo "=== T19: large-file delete (400MB / ~100 chunks) ==="
dd if=/dev/urandom of="$T/t19_large.bin" bs=1M count=400 2>/dev/null
cp "$T/t19_large.bin" "$MOUNT/t19_large.bin"
sync
sleep 3
# Drop the kernel page cache so the read-back goes to the DFS servers cold,
# not the write-path chunk cache which may hold intermediate chunk states.
echo 3 > /proc/sys/vm/drop_caches 2>/dev/null || true
T19_MD5_LOCAL=$(md5sum "$T/t19_large.bin" | cut -d' ' -f1)
T19_MD5_DFS=$(md5sum "$MOUNT/t19_large.bin" | cut -d' ' -f1)
[ "$T19_MD5_LOCAL" = "$T19_MD5_DFS" ] && check "T19a 400MB write+read integrity" PASS \
    || check "T19a 400MB write+read integrity (exp $T19_MD5_LOCAL got $T19_MD5_DFS)" FAIL

CHUNKS_BEFORE=$(find /tmp/dfs-test/node{1,2,3,4,5}/data/chunks -type f 2>/dev/null | wc -l)

T19_START=$(date +%s%3N)
rm "$MOUNT/t19_large.bin"
T19_MS=$(( $(date +%s%3N) - T19_START ))

[ ! -f "$MOUNT/t19_large.bin" ] && check "T19b file gone from namespace immediately" PASS \
    || check "T19b file still visible after rm" FAIL

[ "$T19_MS" -lt 5000 ] && check "T19c rm non-blocking (${T19_MS}ms)" PASS \
    || check "T19c rm blocked too long (${T19_MS}ms, expected <5000ms)" FAIL

# Wait up to 60s for drain worker to delete chunks from disk
T19_WAITED=0
while [ $T19_WAITED -lt 60 ]; do
    sleep 2; T19_WAITED=$((T19_WAITED + 2))
    CHUNKS_AFTER=$(find /tmp/dfs-test/node{1,2,3,4,5}/data/chunks -type f 2>/dev/null | wc -l)
    [ "$CHUNKS_AFTER" -lt "$CHUNKS_BEFORE" ] && break
done
[ "$CHUNKS_AFTER" -lt "$CHUNKS_BEFORE" ] \
    && check "T19d chunks deleted from disk within ${T19_WAITED}s (${CHUNKS_BEFORE}→${CHUNKS_AFTER})" PASS \
    || check "T19d chunks not deleted after ${T19_WAITED}s (before=$CHUNKS_BEFORE after=$CHUNKS_AFTER)" FAIL

# ── Test 21: metadata storm — 5000 touches, node health check, 100 more ──────
echo ""
echo "=== T21: metadata storm + node health ==="

T21_DIR="$MOUNT/t21_storm"
mkdir -p "$T21_DIR"

# Touch 5000 files concurrently (100 at a time) — pure metadata load
echo "  Touching 5000 files (100 concurrent)..."
T21_ERRORS=0
seq 1 5000 | xargs -P100 -I{} bash -c \
    'touch "$1/f$(printf "%05d" "$2").txt" 2>/dev/null || echo FAIL' \
    _ "$T21_DIR" {} | grep -c FAIL > /tmp/t21_touch_errors_$$ 2>/dev/null || true
T21_TOUCH_ERRORS=$(cat /tmp/t21_touch_errors_$$ 2>/dev/null || echo 0)
rm -f /tmp/t21_touch_errors_$$

[ "$T21_TOUCH_ERRORS" -eq 0 ] \
    && check "T21a 5000-file touch storm (0 errors)" PASS \
    || check "T21a 5000-file touch storm ($T21_TOUCH_ERRORS errors)" FAIL

# Wait 10 seconds for any deferred work (sled writes, broadcast flush, dissemination)
echo "  Waiting 10s for cluster to settle..."
sleep 10

# Check every node directly — timeout 5s each; a hang means deadlock
echo "  Checking health of all 5 nodes..."
T21_HEALTH=PASS
for port in 8900 8901 8902 8903 8904; do
    STATUS=$(timeout 5 "$BIN/dfs-admin" --cluster "127.0.0.1:$port" cluster status 2>/dev/null \
        | grep -c "Online" 2>/dev/null) || STATUS=0
    STATUS=$(echo "$STATUS" | tr -d '[:space:]')
    if [ "${STATUS:-0}" -ge 1 ] 2>/dev/null; then
        echo "  Node $port: OK (${STATUS} online)"
    else
        echo "  Node $port: DEADLOCK or unresponsive"
        T21_HEALTH=FAIL
    fi
done
check "T21b all nodes responsive after storm" $T21_HEALTH

# Touch 100 more files and check for any I/O errors
echo "  Touching 100 more files post-storm..."
T21_POST_ERRORS=0
seq 5001 5100 | xargs -P20 -I{} bash -c \
    'touch "$1/f$(printf "%05d" "$2").txt" 2>/dev/null || echo FAIL' \
    _ "$T21_DIR" {} | grep -c FAIL > /tmp/t21_post_errors_$$ 2>/dev/null || true
T21_POST_ERRORS=$(cat /tmp/t21_post_errors_$$ 2>/dev/null || echo 0)
rm -f /tmp/t21_post_errors_$$

[ "$T21_POST_ERRORS" -eq 0 ] \
    && check "T21c 100-file post-storm touch (0 I/O errors)" PASS \
    || check "T21c 100-file post-storm touch ($T21_POST_ERRORS I/O errors)" FAIL

rm -rf "$T21_DIR" 2>/dev/null || true

# ── cleanup ───────────────────────────────────────────────────────────────────
echo ""
echo "=== Cleanup ==="
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 0.3
kill $CLIENT_PID2 2>/dev/null || true
pkill -f "dfs-server" 2>/dev/null || true
rm -rf "$T"

echo ""
echo "════════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed"
echo "════════════════════════════════════════════"
[ $FAIL -eq 0 ] && exit 0 || exit 1
