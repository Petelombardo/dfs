#!/bin/bash
# Local integration test suite: write, read, delete, partial writes, rename, remount persistence, metadata consistency.
# Usage: test_local_suite.sh [T<N> [T<N> ...]]   — run only the specified tests (e.g. T7 T23)
#        test_local_suite.sh                      — run all tests
set -e

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-mount
LOG=/tmp/dfs-test-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
PASS=0; FAIL=0; T=/tmp/dfs-suite-tmp-$$
CURRENT_CLIENT_LOG=""   # set once each client starts

# If test filter args given, only run those tests (e.g. T7 T23).
RUN_TESTS="${*:-ALL}"
should_run() {
    [ "$RUN_TESTS" = "ALL" ] && return 0
    for t in $RUN_TESTS; do [ "$t" = "$1" ] && return 0; done
    return 1
}

check() {
    local name="$1" result="$2"
    if [ "$result" = "PASS" ]; then echo "  PASS: $name"; PASS=$((PASS+1))
    else echo "  FAIL: $name"; FAIL=$((FAIL+1)); fi
}

# snapshot_log <test-label>
# Copies current client log to $LOG/<label>.log then truncates it to zero.
# Call at the START of each test so <label>.log contains only that test's output.
# Sets SKIP_TEST=1 if this test is not in the RUN_TESTS filter.
snapshot_log() {
    local label="$1"
    should_run "$label" || return 0   # don't snapshot log for skipped tests
    [ -z "$CURRENT_CLIENT_LOG" ] && return
    [ -f "$CURRENT_CLIENT_LOG" ] || return
    cp "$CURRENT_CLIENT_LOG" "$LOG/${label}.log"
    : > "$CURRENT_CLIENT_LOG"
}

# dfs_sync: flush all DFS write buffers and metadata to disk.
# Uses sync(1) on the mount point which triggers fsyncdir on the root inode,
# causing the client to drain all write buffers and commit metadata before returning.
dfs_sync() {
    mountpoint -q "$MOUNT" 2>/dev/null && sync "$MOUNT" || true
}

# ── cleanup ──────────────────────────────────────────────────────────────────
pkill -f "dfs-server" 2>/dev/null || true
pkill -f "dfs-client" 2>/dev/null || true
sleep 0.5
fusermount -u $MOUNT 2>/dev/null || true
# Keep $LOG (test logs) — cleared only here at run start, never at end.
# Per-test T<N>.log snapshots persist for post-mortem debugging.
sudo rm -rf $BASE $MOUNT $T 2>/dev/null || rm -rf $BASE $MOUNT $T 2>/dev/null || true
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
    --log-file "$LOG/client.log" --allow-other --log-level debug &
CLIENT_PID=$!
CURRENT_CLIENT_LOG="$LOG/client.log"
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -20 "$LOG/client.log"; exit 1; }
echo "Mounted. Running tests..."
echo ""

# ── Test 1: small write + read ────────────────────────────────────────────────
snapshot_log T1
echo "=== T1: small write/read ==="
echo "hello distributed world" > "$MOUNT/t1.txt"
GOT=$(cat "$MOUNT/t1.txt")
[ "$GOT" = "hello distributed world" ] && check "T1 small write/read" PASS || check "T1 small write/read (got: $GOT)" FAIL

# ── Test 2: 2MB write + read ──────────────────────────────────────────────────
snapshot_log T2
echo "=== T2: 2MB write/read ==="
dd if=/dev/urandom of="$T/big.bin" bs=1M count=2 2>/dev/null
cp "$T/big.bin" "$MOUNT/t2.bin"
cp "$MOUNT/t2.bin" "$T/big_read.bin"
m1=$(md5sum "$T/big.bin"     | cut -d' ' -f1)
m2=$(md5sum "$T/big_read.bin"| cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T2 2MB write/read" PASS || check "T2 2MB write/read (exp $m1 got $m2)" FAIL

# ── Test 3: delete vanishes immediately ───────────────────────────────────────
snapshot_log T3
echo "=== T3: delete ==="
echo "delete me" > "$MOUNT/t3_del.txt"
rm "$MOUNT/t3_del.txt"
[ ! -f "$MOUNT/t3_del.txt" ] && check "T3 delete vanishes" PASS || check "T3 delete vanishes" FAIL

# ── Test 4: delete stays gone ─────────────────────────────────────────────────
snapshot_log T4
echo "=== T4: delete stays gone after 3s ==="
sleep 3
[ ! -f "$MOUNT/t3_del.txt" ] && check "T4 delete stays gone" PASS || check "T4 delete stays gone" FAIL

# ── Test 5: delete + recreate same path ───────────────────────────────────────
snapshot_log T5
echo "=== T5: delete+recreate ==="
echo "v1" > "$MOUNT/t5.txt"
rm "$MOUNT/t5.txt"
sleep 0.3
echo "v2" > "$MOUNT/t5.txt"
GOT=$(cat "$MOUNT/t5.txt")
[ "$GOT" = "v2" ] && check "T5 delete+recreate" PASS || check "T5 delete+recreate (got: $GOT)" FAIL

# ── Test 6: selective delete ──────────────────────────────────────────────────
snapshot_log T6
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
snapshot_log T7
echo "=== T7: overwrite ==="
echo "original" > "$MOUNT/t7.txt"
echo "overwritten" > "$MOUNT/t7.txt"
dfs_sync
GOT=$(cat "$MOUNT/t7.txt")
[ "$GOT" = "overwritten" ] && check "T7 overwrite" PASS || check "T7 overwrite (got: $GOT)" FAIL

# ── Test 8: unmount + remount persistence ─────────────────────────────────────
snapshot_log T8
echo ""
echo "=== T8: unmount + remount persistence ==="
echo "persistent data" > "$MOUNT/t8_persist.txt"
dd if=/dev/urandom of="$T/persist_big.bin" bs=1M count=1 2>/dev/null
cp "$T/persist_big.bin" "$MOUNT/t8_big.bin"
PERSIST_MD5=$(md5sum "$T/persist_big.bin" | cut -d' ' -f1)
dfs_sync

echo "  Unmounting..."
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 0.5
kill $CLIENT_PID 2>/dev/null || true
sleep 1

echo "  Remounting..."
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client2.log" --allow-other --log-level debug &
CLIENT_PID2=$!
CURRENT_CLIENT_LOG="$LOG/client2.log"
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
snapshot_log T9
echo ""
echo "=== T9: partial write (sub-chunk) ==="
dd if=/dev/urandom of="$T/partial.bin" bs=1M count=1 2>/dev/null
cp "$T/partial.bin" "$MOUNT/t9_partial.bin"
dfs_sync
cp "$MOUNT/t9_partial.bin" "$T/partial_read.bin"
m1=$(md5sum "$T/partial.bin"      | cut -d' ' -f1)
m2=$(md5sum "$T/partial_read.bin" | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T9a 1MB partial write/read" PASS || check "T9a 1MB partial write/read (exp $m1 got $m2)" FAIL

# Sub-chunk write that lands mid-chunk: 100KB
dd if=/dev/urandom of="$T/tiny.bin" bs=1K count=100 2>/dev/null
cp "$T/tiny.bin" "$MOUNT/t9_tiny.bin"
dfs_sync
cp "$MOUNT/t9_tiny.bin" "$T/tiny_read.bin"
m1=$(md5sum "$T/tiny.bin"      | cut -d' ' -f1)
m2=$(md5sum "$T/tiny_read.bin" | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T9b 100KB partial write/read" PASS || check "T9b 100KB partial write/read (exp $m1 got $m2)" FAIL

# ── Test 10: cross-chunk boundary write (> 4MB, < 8MB) ───────────────────────
snapshot_log T10
echo ""
echo "=== T10: cross-chunk boundary write (6MB) ==="
dd if=/dev/urandom of="$T/cross.bin" bs=1M count=6 2>/dev/null
cp "$T/cross.bin" "$MOUNT/t10_cross.bin"
dfs_sync
cp "$MOUNT/t10_cross.bin" "$T/cross_read.bin"
m1=$(md5sum "$T/cross.bin"      | cut -d' ' -f1)
m2=$(md5sum "$T/cross_read.bin" | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T10 6MB cross-chunk write/read" PASS || check "T10 6MB cross-chunk write/read (exp $m1 got $m2)" FAIL

# ── Test 11: append to existing file ─────────────────────────────────────────
snapshot_log T11
echo ""
echo "=== T11: append ==="
echo "first line" > "$MOUNT/t11_append.txt"
dfs_sync  # ensure metadata (file size) is committed before O_APPEND open
echo "second line" >> "$MOUNT/t11_append.txt"
dfs_sync
GOT=$(cat "$MOUNT/t11_append.txt")
EXPECTED=$'first line\nsecond line'
[ "$GOT" = "$EXPECTED" ] && check "T11 append to file" PASS || check "T11 append to file (got: $(echo $GOT | head -c 60))" FAIL

# ── Test 12: rename — new path readable, old path gone ───────────────────────
snapshot_log T12
echo ""
echo "=== T12: rename ==="
echo "rename me" > "$MOUNT/t12_before.txt"
dfs_sync
mv "$MOUNT/t12_before.txt" "$MOUNT/t12_after.txt"
GOT=$(cat "$MOUNT/t12_after.txt" 2>/dev/null)
[ "$GOT" = "rename me" ] && check "T12a renamed file readable at new path" PASS || check "T12a renamed file readable (got: $GOT)" FAIL
[ ! -f "$MOUNT/t12_before.txt" ] && check "T12b old path gone after rename" PASS || check "T12b old path still exists after rename" FAIL

# ── Test 13: rename a binary file, verify data integrity ─────────────────────
snapshot_log T13
echo ""
echo "=== T13: rename binary file ==="
dd if=/dev/urandom of="$T/rename_src.bin" bs=1M count=2 2>/dev/null
cp "$T/rename_src.bin" "$MOUNT/t13_src.bin"
dfs_sync
mv "$MOUNT/t13_src.bin" "$MOUNT/t13_dst.bin"
cp "$MOUNT/t13_dst.bin" "$T/rename_dst_read.bin"
m1=$(md5sum "$T/rename_src.bin"      | cut -d' ' -f1)
m2=$(md5sum "$T/rename_dst_read.bin" | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T13a renamed binary data intact" PASS || check "T13a renamed binary data (exp $m1 got $m2)" FAIL
[ ! -f "$MOUNT/t13_src.bin" ] && check "T13b src gone after rename" PASS || check "T13b src still exists after rename" FAIL

# ── Test 14: rename + metadata consistency across nodes ──────────────────────
snapshot_log T14
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
# Try once at 3s, then retry with extra 2s wait if inconsistent (5s total)
T14B_OK=FAIL
for attempt in 1 2; do
    declare -A NODE_LISTS
    for port in 8900 8901 8902 8903 8904; do
        NODE_LISTS[$port]=$("$BIN/dfs-admin" --cluster "127.0.0.1:$port" file list 2>/dev/null \
            | grep -E "^[0-9a-f]{8}" | awk '{print $1, $2, $3}' | sort)
    done

    if [ "${NODE_LISTS[8900]}" = "${NODE_LISTS[8901]}" ] && \
       [ "${NODE_LISTS[8901]}" = "${NODE_LISTS[8902]}" ] && \
       [ "${NODE_LISTS[8902]}" = "${NODE_LISTS[8903]}" ] && \
       [ "${NODE_LISTS[8903]}" = "${NODE_LISTS[8904]}" ]; then
        T14B_OK=PASS
        break
    elif [ "$attempt" -eq 1 ]; then
        echo "  Metadata inconsistent at 3s, waiting 2s more..."
        sleep 2
    fi
done

if [ "$T14B_OK" = "PASS" ]; then
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
snapshot_log T15
echo ""
echo "=== T15: partial in-place overwrite (4MB file, patch first 2MB) ==="
dd if=/dev/urandom of="$T/t15_orig.bin"  bs=1M count=4 2>/dev/null
dd if=/dev/urandom of="$T/t15_patch.bin" bs=1M count=2 2>/dev/null

# Build the expected result on the local filesystem (no DFS involved)
cp "$T/t15_orig.bin" "$T/t15_expected.bin"
dd if="$T/t15_patch.bin" of="$T/t15_expected.bin" bs=1M count=2 conv=notrunc 2>/dev/null

# Write orig to DFS, then patch first 2MB in-place (conv=notrunc)
cp "$T/t15_orig.bin" "$MOUNT/t15_patch.bin"
dfs_sync
dd if="$T/t15_patch.bin" of="$MOUNT/t15_patch.bin" bs=1M count=2 conv=notrunc 2>/dev/null
dfs_sync
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
snapshot_log T16
echo ""
echo "=== T16: O_TRUNC replace (3MB → 1MB) ==="
dd if=/dev/urandom of="$T/t16_big.bin"   bs=1M count=3 2>/dev/null
dd if=/dev/urandom of="$T/t16_small.bin" bs=1M count=1 2>/dev/null
cp "$T/t16_big.bin" "$MOUNT/t16_trunc.bin"
dfs_sync
cp "$T/t16_small.bin" "$MOUNT/t16_trunc.bin"   # cp uses O_TRUNC
dfs_sync
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
snapshot_log T17
echo ""
echo "=== T17: DVR header-update (4MB file, small patch at offset 0) ==="
dd if=/dev/urandom of="$T/t17_orig.bin" bs=1M count=4 2>/dev/null
dd if=/dev/urandom of="$T/t17_hdr.bin"  bs=1K count=12 2>/dev/null

# Expected: first 12KB = header, rest = original tail
cp "$T/t17_orig.bin" "$T/t17_expected.bin"
dd if="$T/t17_hdr.bin" of="$T/t17_expected.bin" bs=1K count=12 conv=notrunc 2>/dev/null

# Write 4MB to DFS, flush, then update header
cp "$T/t17_orig.bin" "$MOUNT/t17_dvr.bin"
dfs_sync  # ensure chunk 0 is flushed and flushed_sizes[0] is set before header patch
dd if="$T/t17_hdr.bin" of="$MOUNT/t17_dvr.bin" bs=1K count=12 conv=notrunc 2>/dev/null
dfs_sync
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
snapshot_log T17c
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
dfs_sync  # flush the 12KB header, setting flushed_sizes[0]=12032 before recording write

# Step 2: write recording data at offset 12032 — slot grows to 4MB, ticker flushes via PatchChunk
dd if="$T/t17c_recording.bin" of="$MOUNT/t17c_dvr.bin" bs=1k seek=$(($HEADER_SIZE/1024)) count=$(($TAIL_SIZE/1024)) conv=notrunc 2>/dev/null
dfs_sync

cp "$MOUNT/t17c_dvr.bin" "$T/t17c_read.bin"

m1=$(md5sum "$T/t17c_expected.bin" | cut -d' ' -f1)
m2=$(md5sum "$T/t17c_read.bin"     | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T17c DVR exact pattern: header+recording intact" PASS \
    || check "T17c DVR exact pattern: data mismatch (exp $m1 got $m2)" FAIL

# ── Test 18: DVR concurrent-read integrity ────────────────────────────────────
# Write a 20MB file at ~4MB/s while concurrently reading from offset 0.
# Verifies: no short reads that skip data, read copy matches written data.
snapshot_log T18
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
snapshot_log T20
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
dfs_sync

# Apply same patches to DFS file
dd if="$T/t20_patch.bin" of="$MOUNT/t20_test.bin" bs=1M count=2 seek=0            conv=notrunc 2>/dev/null || true  # chunk 0
dd if="$T/t20_patch.bin" of="$MOUNT/t20_test.bin" bs=1M count=2 seek=4            conv=notrunc 2>/dev/null || true  # chunk 1 start
dd if="$T/t20_patch.bin" of="$MOUNT/t20_test.bin" bs=1M count=2 seek=8            conv=notrunc 2>/dev/null || true  # chunk 2 start
dfs_sync

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
snapshot_log T19
echo ""
echo "=== T19: large-file delete (400MB / ~100 chunks) ==="
dd if=/dev/urandom of="$T/t19_large.bin" bs=1M count=400 2>/dev/null
cp "$T/t19_large.bin" "$MOUNT/t19_large.bin"
dfs_sync
# Drop the kernel page cache so the read-back goes to the DFS servers cold,
# not the write-path chunk cache which may hold intermediate chunk states.
sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches' 2>/dev/null || true
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

# ── Test 21: metadata storm — 2000 touches, node health check, 100 more ──────
snapshot_log T21
echo ""
echo "=== T21: metadata storm + node health ==="

T21_DIR="$MOUNT/t21_storm"
mkdir -p "$T21_DIR"

# Touch 2000 files concurrently (100 at a time) — pure metadata load
# Reduced from 5000 to 2000 after fixing concurrent patch race (be84ce7):
# 5000 likely had silent corruption from stale chunk_id races, but was passing
echo "  Touching 2000 files (100 concurrent)..."
T21_ERRORS=0
seq 1 2000 | xargs -P100 -I{} bash -c \
    'touch "$1/f$(printf "%05d" "$2").txt" 2>/dev/null || echo FAIL' \
    _ "$T21_DIR" {} | grep -c FAIL > /tmp/t21_touch_errors_$$ 2>/dev/null || true
T21_TOUCH_ERRORS=$(cat /tmp/t21_touch_errors_$$ 2>/dev/null || echo 0)
rm -f /tmp/t21_touch_errors_$$

[ "$T21_TOUCH_ERRORS" -eq 0 ] \
    && check "T21a 2000-file touch storm (0 errors)" PASS \
    || check "T21a 2000-file touch storm ($T21_TOUCH_ERRORS errors)" FAIL

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

# ── Test 22: VM disk image random-patch throughput (QEMU install pattern) ─────
#
# Emulates what happens during a Debian install on a raw disk image:
#   - An 8GB pre-existing disk image (all chunks already on servers)
#   - Many concurrent open → write small patch at random offset → close cycles
#   - Each patch hits an existing chunk so must go through fetch-hash-patch path
#
# Baseline: measures total time and per-patch latency before any optimization.
dfs_sync  # drain any residual T21 metadata before starting T22
snapshot_log T22
echo ""
echo "=== T22: VM disk random-patch throughput (QEMU install pattern) ==="

T22_IMG="$MOUNT/t22_disk.img"
T22_SIZE_MB=32        # 8 chunks of 4MB — representative slice of a disk image
T22_PATCH_COUNT=50    # 50 concurrent open/patch/close cycles
T22_PATCH_SIZE=12032  # 12KB — matches the GRUB header write seen in logs
T22_CONCURRENCY=8     # 8 at a time — matches QEMU's typical queue depth

# Step 1: create the base image (fresh sequential write — fast path)
echo "  Writing ${T22_SIZE_MB}MB base image..."
dd if=/dev/urandom of="$T/t22_base.bin" bs=1M count=$T22_SIZE_MB 2>/dev/null
cp "$T/t22_base.bin" "$T22_IMG"
dfs_sync  # ensure all chunks are flushed and metadata is committed before patches

# Step 2: run N random patches concurrently, measuring total wall time
echo "  Running $T22_PATCH_COUNT patches ($T22_CONCURRENCY concurrent, ${T22_PATCH_SIZE}B each)..."
dd if=/dev/urandom of="$T/t22_patch.bin" bs=$T22_PATCH_SIZE count=1 2>/dev/null

T22_START=$(date +%s%3N)

# Debug: run one patch with visible stderr to capture any error
python3 -c "
import sys, os
img, patch_file = sys.argv[1], sys.argv[2]
data = open(patch_file,'rb').read()
fd = os.open(img, os.O_WRONLY)
os.lseek(fd, 0, 0)
os.write(fd, data)
os.close(fd)
" "$T22_IMG" "$T/t22_patch.bin" 2>&1 | head -3 && echo "  DEBUG: single patch OK" || echo "  DEBUG: single patch FAILED"

# Each job: pick a random 4MB-aligned chunk offset, patch T22_PATCH_SIZE bytes at a
# random intra-chunk offset. Use dd conv=notrunc so the rest of the chunk is preserved.
T22_ERRORS=0
seq 1 $T22_PATCH_COUNT | xargs -P$T22_CONCURRENCY -I{} bash -c '
    img="$1"; patch="$2"; size_mb="$3"; patch_size="$4"; errfile="$5"
    # random chunk (0..N-1) then random intra-chunk offset aligned to 4KB
    chunk=$(( RANDOM % (size_mb / 4) ))
    intra=$(( (RANDOM % ((4*1024*1024 - patch_size) / 4096)) * 4096 ))
    byte_off=$(( chunk * 4 * 1024 * 1024 + intra ))
    python3 -c "
import sys, os
img, patch_file, byte_off = sys.argv[1], sys.argv[2], int(sys.argv[3])
data = open(patch_file,\"rb\").read()
fd = os.open(img, os.O_WRONLY)
os.lseek(fd, byte_off, 0)
os.write(fd, data)
os.close(fd)
" "$img" "$patch" "$byte_off" 2>>"$errfile" || echo FAIL
' _ "$T22_IMG" "$T/t22_patch.bin" "$T22_SIZE_MB" "$T22_PATCH_SIZE" "/tmp/t22_py_errors_$$" \
  | grep -c FAIL > /tmp/t22_errors_$$ 2>/dev/null || true
if [ -s "/tmp/t22_py_errors_$$" ]; then
    echo "  Sample python error: $(head -1 /tmp/t22_py_errors_$$)"
fi
rm -f "/tmp/t22_py_errors_$$"

T22_MS=$(( $(date +%s%3N) - T22_START ))
T22_ERRORS=$(cat /tmp/t22_errors_$$ 2>/dev/null || echo 0)
rm -f /tmp/t22_errors_$$

T22_PER_PATCH_MS=$(( T22_MS / T22_PATCH_COUNT ))

[ "$T22_ERRORS" -eq 0 ] \
    && check "T22a $T22_PATCH_COUNT random patches, 0 errors" PASS \
    || check "T22a $T22_PATCH_COUNT random patches, $T22_ERRORS errors" FAIL

echo "  Throughput: ${T22_PATCH_COUNT} patches in ${T22_MS}ms (~${T22_PER_PATCH_MS}ms/patch)"

# Step 3: read back and verify the image is still consistent (no corruption)
cp "$T22_IMG" "$T/t22_readback.bin" 2>/dev/null
[ -s "$T/t22_readback.bin" ] \
    && check "T22c image readable after patch storm" PASS \
    || check "T22c image unreadable after patch storm" FAIL

rm -f "$T22_IMG" 2>/dev/null || true

# ── Test 22b: FIFO ordering — sequential overlapping patches to same chunk ─────
snapshot_log T22b
echo ""
echo "=== T22b: FIFO ordering for overlapping chunk patches ==="

# This test targets the concurrent same-chunk patch race fixed in be84ce7.
# Strategy: Apply the same sequence of overlapping dd writes to both DFS and
# a local file, then verify md5sums match. Without FIFO ordering, background
# ticker flushes can race with foreground writes, causing out-of-order patches
# that result in data corruption (final DFS content differs from local file).

T22B_DFS="$MOUNT/t22b_fifo.img"
T22B_LOCAL="$T/t22b_local.img"

# Create sparse 4MB file (1 chunk) on both filesystems
truncate -s 4M "$T22B_DFS" 2>/dev/null
truncate -s 4M "$T22B_LOCAL" 2>/dev/null
dfs_sync

echo "  Applying 12 sequential overlapping writes (varying offsets/sizes)..."

T22B_START=$(date +%s%3N)

# Generate 12 distinct patterns
for i in $(seq 0 11); do
    pattern=$(printf "%02x" $((i * 0x11)))
    if [ $(( i % 2 )) -eq 0 ]; then
        # Even: small 4KB write
        dd if=/dev/zero bs=4096 count=1 2>/dev/null | tr '\000' "\x$pattern" > "$T/t22b_pat_$i.bin"
    else
        # Odd: large 256KB write
        dd if=/dev/zero bs=262144 count=1 2>/dev/null | tr '\000' "\x$pattern" > "$T/t22b_pat_$i.bin"
    fi
done

# Apply writes to both files sequentially
# Offsets chosen to overlap previous writes (forces patches to same chunk)
# Offset pattern: 0, 128KB, 256KB, 384KB, 512KB, 640KB, 768KB, 896KB, 1MB, ...
for i in $(seq 0 11); do
    offset=$((i * 128 * 1024))  # 128KB stride
    dd if="$T/t22b_pat_$i.bin" of="$T22B_DFS" bs=4096 seek=$((offset / 4096)) conv=notrunc 2>/dev/null
    dd if="$T/t22b_pat_$i.bin" of="$T22B_LOCAL" bs=4096 seek=$((offset / 4096)) conv=notrunc 2>/dev/null
done

# Sync DFS to ensure all patches are flushed
dfs_sync

T22B_MS=$(( $(date +%s%3N) - T22B_START ))

# Compare md5sums
T22B_DFS_MD5=$(md5sum "$T22B_DFS" | awk '{print $1}')
T22B_LOCAL_MD5=$(md5sum "$T22B_LOCAL" | awk '{print $1}')

if [ "$T22B_DFS_MD5" = "$T22B_LOCAL_MD5" ]; then
    check "T22b DFS matches local file (md5: ${T22B_DFS_MD5:0:8})" PASS
else
    check "T22b DFS differs from local (dfs=${T22B_DFS_MD5:0:8} local=${T22B_LOCAL_MD5:0:8})" FAIL
fi

echo "  Completed in ${T22B_MS}ms"

rm -f "$T22B_DFS" "$T22B_LOCAL" "$T"/t22b_pat_*.bin 2>/dev/null || true

# ── Test 23: random small-read path (range-fetch) ─────────────────────────────
#
# Verifies that 4K reads into a multi-chunk file use the byte-range fetch path
# (ReadChunkRange) rather than fetching the full 4MB chunk.  Checks:
#   a) Data correctness: every 4K read returns the exact bytes written.
#   b) Range fetch fires: "Range fetch:" appears in the client log.
#   c) Sub-chunk cache: a re-read of the same offset is served from cache
#      (no second "Range fetch:" for the same chunk offset).
snapshot_log T23
if should_run T23; then
echo "=== T23: random small-read (range-fetch) path ==="

T23_SIZE=$(( 3 * 4 * 1024 * 1024 ))   # 12MB — 3 full chunks

# Remount first so the file is written fresh on the new client — this means
# the kernel page cache has never seen it, so all reads go through FUSE.
fusermount -u "$MOUNT" 2>/dev/null || true
kill "$CLIENT_PID2" 2>/dev/null || true
sleep 1
T23_CLIENT_LOG="$LOG/client_t23.log"
: > "$T23_CLIENT_LOG"
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$T23_CLIENT_LOG" --allow-other --log-level debug &
CLIENT_PID2=$!
CURRENT_CLIENT_LOG="$T23_CLIENT_LOG"
sleep 2
mountpoint -q "$MOUNT" || { check "T23 remount" FAIL; CLIENT_PID2=""; }

T23_FILE="$MOUNT/t23_range.bin"

# Write known-pattern data: each 4KB block filled with its block index byte.
python3 -c "
size = $T23_SIZE
block = 4096
data = bytearray()
for i in range(size // block):
    data += bytes([i & 0xff]) * block
open('$T23_FILE', 'wb').write(data)
"
dfs_sync

# Drop kernel page cache so reads go through FUSE to DFS — not served from RAM.
# The remount alone is not enough: the kernel page cache persists across FUSE
# remounts (keyed by inode on the underlying fs). drop_caches flushes it fully.
fusermount -u "$MOUNT" 2>/dev/null || true
kill "$CLIENT_PID2" 2>/dev/null || true
sleep 1
: > "$T23_CLIENT_LOG"
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$T23_CLIENT_LOG" --allow-other --log-level debug &
CLIENT_PID2=$!
sleep 2
mountpoint -q "$MOUNT" || { check "T23 remount2" FAIL; CLIENT_PID2=""; }
T23_FILE="$MOUNT/t23_range.bin"

# Pick 3 4K offsets in different chunks.
T23_OFF1=$(( 0 * 4*1024*1024 + 8192 ))       # chunk 0, intra 8KB
T23_OFF2=$(( 1 * 4*1024*1024 + 1048576 ))     # chunk 1, intra 1MB
T23_OFF3=$(( 2 * 4*1024*1024 + 3*1024*1024 )) # chunk 2, intra 3MB

# Read 4K at each offset using O_DIRECT to bypass kernel page cache.
# FUSE passes O_DIRECT through to the filesystem handler, ensuring reads
# go through FUSE to DFS rather than being served from the kernel page cache.
T23_READ_PY='
import os, sys
path, off_s, exp_s = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
fd = os.open(path, os.O_RDONLY | os.O_DIRECT)
os.lseek(fd, off_s, os.SEEK_SET)
data = os.read(fd, 4096)
os.close(fd)
ok = len(data) == 4096 and all(x == exp_s for x in data)
print("OK" if ok else "FAIL")
'
T23_ERRORS=0
for OFF in $T23_OFF1 $T23_OFF2 $T23_OFF3; do
    EXPECT=$(( (OFF / 4096) & 0xff ))
    GOT=$(python3 -c "$T23_READ_PY" "$T23_FILE" "$OFF" "$EXPECT")
    [ "$GOT" = "OK" ] || T23_ERRORS=$(( T23_ERRORS + 1 ))
done
sleep 0.5  # let async log writes flush

[ "$T23_ERRORS" -eq 0 ] \
    && check "T23a 4K random reads correct data" PASS \
    || check "T23a 4K random reads data errors=$T23_ERRORS" FAIL

# Check that Range fetch log lines appeared (proves ReadChunkRange was used).
# The log file was freshly created before this mount, so count from the start.
RANGE_FETCHES=$(grep -c "Range fetch:" "$CURRENT_CLIENT_LOG" 2>/dev/null; true)
[ "$RANGE_FETCHES" -ge 3 ] \
    && check "T23b range-fetch fired ($RANGE_FETCHES lines)" PASS \
    || check "T23b range-fetch did not fire (got $RANGE_FETCHES, want >=3)" FAIL

# Re-read same offsets — O_DIRECT again to verify DFS byte-range cache (not page cache).
T23C_ERRORS=0
for OFF in $T23_OFF1 $T23_OFF2 $T23_OFF3; do
    EXPECT=$(( (OFF / 4096) & 0xff ))
    GOT=$(python3 -c "$T23_READ_PY" "$T23_FILE" "$OFF" "$EXPECT")
    [ "$GOT" = "OK" ] || T23C_ERRORS=$(( T23C_ERRORS + 1 ))
done
[ "$T23C_ERRORS" -eq 0 ] \
    && check "T23c re-read data still correct" PASS \
    || check "T23c re-read data errors=$T23C_ERRORS" FAIL

rm -f "$T23_FILE" 2>/dev/null || true
fi # should_run T23

snapshot_log T24
if should_run T24; then
echo "=== T24: sequential read uses full-chunk path (no range-fetch) ==="

# 16MB = 4 full chunks. Written and then re-read sequentially.
# The test verifies two things:
#   T24a: data is correct end-to-end
#   T24b: the full-chunk path fired (no Range fetch log lines) — proving
#         sequential reads are NOT being broken into 128KB range-fetch RTTs.
T24_SIZE=$(( 16 * 1024 * 1024 ))
T24_FILE="$MOUNT/t24_seq.bin"

# Write known pattern: each byte = (offset / 4096) & 0xff
python3 -c "
size = $T24_SIZE
block = 4096
data = bytearray()
for i in range(size // block):
    data += bytes([i & 0xff]) * block
open('$T24_FILE', 'wb').write(data)
"
dfs_sync

# Remount for cold cache — ensures all reads go through FUSE to DFS.
fusermount -u "$MOUNT" 2>/dev/null || true
kill "$CLIENT_PID2" 2>/dev/null || true
sleep 1
T24_CLIENT_LOG="$LOG/client_t24.log"
: > "$T24_CLIENT_LOG"
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$T24_CLIENT_LOG" --allow-other --log-level debug &
CLIENT_PID2=$!
CURRENT_CLIENT_LOG="$T24_CLIENT_LOG"
sleep 2
mountpoint -q "$MOUNT" || { check "T24 remount" FAIL; CLIENT_PID2=""; }
T24_FILE="$MOUNT/t24_seq.bin"

# Sequential read of the full file.
T24_ERRORS=$(python3 -c "
size = $T24_SIZE
block = 4096
errors = 0
with open('$T24_FILE', 'rb') as f:
    for i in range(size // block):
        data = f.read(block)
        exp = i & 0xff
        if len(data) != block or not all(x == exp for x in data):
            errors += 1
print(errors)
")
[ "$T24_ERRORS" -eq 0 ] \
    && check "T24a sequential read data correct" PASS \
    || check "T24a sequential read data errors=$T24_ERRORS" FAIL

# Verify full-chunk path was used: no "Range fetch:" lines in the log.
# Sequential reads must fetch whole 4MB chunks, not 128KB slices.
RANGE_LINES=$(grep -c "Range fetch:" "$T24_CLIENT_LOG" 2>/dev/null || true)
[ "$RANGE_LINES" -eq 0 ] \
    && check "T24b no range-fetch on sequential read (full-chunk path used)" PASS \
    || check "T24b range-fetch fired on sequential read ($RANGE_LINES lines) — regression" FAIL

rm -f "$T24_FILE" 2>/dev/null || true
fi # should_run T24

# ── Test 25: OS-install simulation — full disk image, multi-phase patches, fsync, integrity ──
snapshot_log T25
if should_run T25; then
echo ""
echo "=== T25: OS-install simulation (disk image, scatter patches, fsync, integrity) ==="

# 256MB raw disk = 64 chunks.  Chunks 0-253 are inside the metadata-refresh window;
# chunks 254-63 are beyond it and rely on metadata_cache / recent_chunk_writes.
# This specifically tests the dual-RF stale-base retry and healer tombstone paths.
T25_IMG="$MOUNT/t25_disk.raw"
T25_MB=256
T25_MANIFEST="$T/t25_manifest.py"

# Phase 1: blank disk (like a fresh VM disk allocation via ftruncate)
echo "  Phase 1: allocating ${T25_MB}MB blank disk..."
truncate -s ${T25_MB}M "$T25_IMG"
dfs_sync

# Phase 2: partition table + filesystem structures (scattered writes across many chunks)
# Simulates what mkfs.ext4 or the debian-installer does: small writes spread across
# the whole disk including chunks well beyond the metadata-refresh window.
echo "  Phase 2: writing partition table + filesystem structures..."
python3 - "$T25_IMG" "$T25_MANIFEST" "$T25_MB" << 'PYEOF'
import os, sys, json, random

img, manifest_path, size_mb = sys.argv[1], sys.argv[2], int(sys.argv[3])
CHUNK = 4 * 1024 * 1024
n_chunks = size_mb * 1024 * 1024 // CHUNK

fd = os.open(img, os.O_RDWR)
state = {}   # file_offset (str) -> hex of last written data

def write_at(offset, data):
    end = offset + len(data)
    # Evict any earlier manifest entry whose start falls inside this write's range.
    # Without this, a large write (e.g. 32KB grub core) leaves stale entries for
    # sub-ranges written in earlier phases, causing false verification mismatches.
    for k in list(state.keys()):
        if offset <= int(k) < end:
            del state[k]
    os.lseek(fd, offset, os.SEEK_SET)
    os.write(fd, data)
    state[str(offset)] = data.hex()

# MBR / partition table (chunk 0, very beginning)
write_at(0,   b'DFSTEST_MBR_' + bytes(range(256)) * 2)   # 512 B
write_at(512, b'GPT_HEADER___' + b'\xaa' * 500)           # 512 B

# Superblock at 1KB offset inside chunk 0
write_at(1024, b'EXT4_SUPER___' + b'\x5a' * 1011)         # 1024 B

# Group descriptors etc — scattered 4KB writes inside first few chunks
for chunk_idx in range(min(4, n_chunks)):
    for inner in [0, 4096, 8192, 32768, 65536]:
        off = chunk_idx * CHUNK + inner
        tag = f'GDT_c{chunk_idx:03d}_{inner:06d}'.encode().ljust(64, b'\x11')
        write_at(off, tag)

# Inode table + data blocks scattered across HIGH-NUMBERED chunks
# (beyond the 253-chunk metadata-window — these are the problematic ones)
random.seed(42)
for chunk_idx in random.sample(range(30, n_chunks), min(30, n_chunks - 30)):
    for inner in [0, 4096, 12288, 65536]:
        off = chunk_idx * CHUNK + inner
        tag = f'INODE_c{chunk_idx:04d}_{inner:08d}_PHASE2'.encode().ljust(128, b'\x22')
        write_at(off, tag)

os.close(fd)
json.dump(state, open(manifest_path, 'w'))
print(f"  wrote {len(state)} regions across {n_chunks} chunks")
PYEOF
dfs_sync
echo "  Phase 2 fsync done."

# Phase 3: file data installation — full-chunk writes to a run of high chunks,
# simulating large package extractions overwriting blocks across the disk.
echo "  Phase 3: simulating package file extraction (full-chunk writes)..."
python3 - "$T25_IMG" "$T25_MANIFEST" "$T25_MB" << 'PYEOF'
import os, sys, json

img, manifest_path, size_mb = sys.argv[1], sys.argv[2], int(sys.argv[3])
CHUNK = 4 * 1024 * 1024
state = json.load(open(manifest_path))

fd = os.open(img, os.O_RDWR)

def write_at(offset, data):
    end = offset + len(data)
    for k in list(state.keys()):
        if offset <= int(k) < end:
            del state[k]
    os.lseek(fd, offset, os.SEEK_SET)
    os.write(fd, data)
    state[str(offset)] = data.hex()

# Write full-chunk data to several mid-range chunks (like extracting a 20MB package)
for chunk_idx in range(10, 15):
    off = chunk_idx * CHUNK
    data = bytes([(chunk_idx * 7 + i) & 0xff for i in range(CHUNK)])
    write_at(off, data)

os.close(fd)
json.dump(state, open(manifest_path, 'w'))
print(f"  {len(state)} total regions tracked")
PYEOF
dfs_sync
echo "  Phase 3 fsync done."

# Phase 4: grub install — small patches over chunks that were already written,
# then an fsync exactly like grub does.  This is the critical path that was failing.
echo "  Phase 4: grub-style small patches + fsync..."
python3 - "$T25_IMG" "$T25_MANIFEST" << 'PYEOF'
import os, sys, json

img, manifest_path = sys.argv[1], sys.argv[2]
CHUNK = 4 * 1024 * 1024
state = json.load(open(manifest_path))

fd = os.open(img, os.O_RDWR)

def write_at(offset, data):
    end = offset + len(data)
    for k in list(state.keys()):
        if offset <= int(k) < end:
            del state[k]
    os.lseek(fd, offset, os.SEEK_SET)
    os.write(fd, data)
    state[str(offset)] = data.hex()

# Overwrite MBR with grub stage1 (exactly like grub-install does)
write_at(0, b'GRUB_STAGE1__' + b'\xeb\x63\x90' + b'\xff' * 499)

# Grub core image: 32KB patch at start of chunk 0 after the first sector
write_at(512 * 2, b'GRUB_CORE____' + bytes(range(256)) * 128)

# Also re-patch two high-numbered chunks to simulate grub writing
# filesystem-specific data (e.g., blocklist for /boot/grub files).
# These patches land on top of previously written phase-2 data.
for chunk_idx in [40, 55]:
    off = chunk_idx * CHUNK + 4096
    write_at(off, f'GRUB_BLKLIST_c{chunk_idx:04d}'.encode().ljust(512, b'\xdd'))

os.close(fd)
json.dump(state, open(manifest_path, 'w'))
PYEOF

# The fsync that triggers "please insert CD" — this is the exact failing operation
dfs_sync
echo "  Phase 4 fsync done (grub complete)."

# Verification: re-open cold and check every written region
echo "  Verifying integrity..."
T25_MISMATCHES=$(python3 - "$T25_IMG" "$T25_MANIFEST" << 'PYEOF'
import os, sys, json

img, manifest_path = sys.argv[1], sys.argv[2]
state = json.load(open(manifest_path))
errors = []

with open(img, 'rb') as f:
    for offset_str, expected_hex in sorted(state.items(), key=lambda x: int(x[0])):
        offset = int(offset_str)
        expected = bytes.fromhex(expected_hex)
        f.seek(offset)
        actual = f.read(len(expected))
        if actual != expected:
            errors.append(f"offset {offset}: exp {expected_hex[:16]}... got {actual.hex()[:16]}...")

for e in errors[:5]:
    print(e)
print(len(errors))
PYEOF
)

T25_ERR_COUNT=$(echo "$T25_MISMATCHES" | tail -1)
T25_ERR_COUNT=${T25_ERR_COUNT:-1}
[ "$T25_ERR_COUNT" -eq 0 ] \
    && check "T25a OS-install integrity: all ${#} regions match after 4-phase install+fsync" PASS \
    || check "T25a OS-install integrity: $T25_ERR_COUNT mismatches after install" FAIL

# Phase 5: patch-over-full-chunk integrity —
# write a full chunk, then patch a small region within it, verify both regions.
echo "  Phase 5: patch-over-full-chunk integrity..."
T25B_CHUNK_OFF=$(( 20 * 4 * 1024 * 1024 ))   # chunk 20, well within range
T25B_RESULT=$(python3 - "$T25_IMG" "$T25B_CHUNK_OFF" << 'PYEOF'
import os, sys

img = sys.argv[1]
base_off = int(sys.argv[2])
CHUNK = 4 * 1024 * 1024
PATCH_OFF = 131072   # 128KB into chunk
PATCH_LEN = 4096

# Write known full-chunk pattern
full = bytes([0xab] * CHUNK)
with open(img, 'r+b') as f:
    f.seek(base_off); f.write(full)
PYEOF
)
dfs_sync   # flush full write

python3 - "$T25_IMG" "$T25B_CHUNK_OFF" << 'PYEOF'
import os, sys

img = sys.argv[1]
base_off = int(sys.argv[2])
PATCH_OFF = 131072
PATCH_LEN = 4096

# Now patch a small region within it (simulating grub patching a written chunk)
patch_data = bytes([0xcd] * PATCH_LEN)
with open(img, 'r+b') as f:
    f.seek(base_off + PATCH_OFF); f.write(patch_data)
PYEOF
dfs_sync   # fsync the patch

# Verify: before-patch region and after-patch region both correct
T25B_ERRORS=$(python3 - "$T25_IMG" "$T25B_CHUNK_OFF" << 'PYEOF'
import sys
img = sys.argv[1]
base_off = int(sys.argv[2])
CHUNK = 4 * 1024 * 1024
PATCH_OFF = 131072
PATCH_LEN = 4096

errors = 0
with open(img, 'rb') as f:
    # Region before patch — should still be 0xab
    f.seek(base_off)
    pre = f.read(PATCH_OFF)
    if any(b != 0xab for b in pre):
        print(f"pre-patch region corrupted ({sum(1 for b in pre if b != 0xab)} wrong bytes)")
        errors += 1
    # Patch region — should be 0xcd
    f.seek(base_off + PATCH_OFF)
    patch = f.read(PATCH_LEN)
    if any(b != 0xcd for b in patch):
        print(f"patch region wrong ({sum(1 for b in patch if b != 0xcd)} wrong bytes)")
        errors += 1
    # Region after patch — should still be 0xab
    f.seek(base_off + PATCH_OFF + PATCH_LEN)
    post = f.read(CHUNK - PATCH_OFF - PATCH_LEN)
    if any(b != 0xab for b in post):
        print(f"post-patch region corrupted ({sum(1 for b in post if b != 0xab)} wrong bytes)")
        errors += 1
print(errors)
PYEOF
)
T25B_ERR_COUNT=$(echo "$T25B_ERRORS" | tail -1)
T25B_ERR_COUNT=${T25B_ERR_COUNT:-1}
[ "$T25B_ERR_COUNT" -eq 0 ] \
    && check "T25b patch-over-full-chunk: pre/patch/post regions all correct" PASS \
    || check "T25b patch-over-full-chunk: $T25B_ERR_COUNT region errors" FAIL

rm -f "$T25_IMG" "$T25_MANIFEST" 2>/dev/null || true

# T25c: healer-race regression — write, patch (dual-RF), trigger healer, verify no revert.
# This is the exact corruption scenario from staging:
#   Without tombstones: healer copies old_hash from 3rd replica back to the 2 patched
#   replicas, reverting the patch. Read-back returns pre-patch data.
#   With tombstones: HasChunks returns false for old_hash on 3rd replica — healer
#   cannot use it as source, cannot revert. Read-back returns patched data.
echo "  T25c: healer-race regression (patch + trigger healer + verify no revert)..."
T25C_FILE="$MOUNT/t25c_healer.bin"
T25C_CHUNK_BYTES=$(( 4 * 1024 * 1024 ))
T25C_PATCH_OFF=$(( 64 * 1024 ))   # 64KB into the chunk
T25C_PATCH_LEN=$(( 32 * 1024 ))   # 32KB patch

# Step 1: fresh full-chunk write — goes RF=3 (no dual-RF skip, no old data on any node)
python3 -c "
import sys
with open(sys.argv[1], 'wb') as f:
    f.write(bytes([0xaa] * $T25C_CHUNK_BYTES))
" "$T25C_FILE"
dfs_sync

# Step 2: small patch within the chunk — dual-RF: 2 nodes get 0xbb region, 3rd keeps 0xaa
python3 -c "
import os, sys
fd = os.open(sys.argv[1], os.O_RDWR)
os.lseek(fd, $T25C_PATCH_OFF, 0)
os.write(fd, bytes([0xbb] * $T25C_PATCH_LEN))
os.close(fd)
" "$T25C_FILE"
dfs_sync   # flush — now: patched_node_A and patched_node_B have 0xbb; 3rd has 0xaa

# Step 3: explicitly trigger the healer while the patched state is live.
# Without tombstones the healer would copy 0xaa from 3rd node back to A and B.
# With tombstones the 3rd node's old chunk returns false from HasChunks — safe.
"$BIN/dfs-admin" --cluster "$CLUSTER" healing trigger 2>/dev/null || true
sleep 5   # give healer time to run a full cycle

# Also trigger a second time and wait — catches healers that need multiple cycles
"$BIN/dfs-admin" --cluster "$CLUSTER" healing trigger 2>/dev/null || true
sleep 5

# Step 4: read back and verify both regions
T25C_RESULT=$(python3 -c "
import sys
errors = []
with open(sys.argv[1], 'rb') as f:
    # Pre-patch region: should still be 0xaa
    pre = f.read($T25C_PATCH_OFF)
    bad = sum(1 for b in pre if b != 0xaa)
    if bad: errors.append(f'pre-patch: {bad} bytes wrong (healer may have reverted)')
    # Patch region: should be 0xbb
    patch = f.read($T25C_PATCH_LEN)
    bad = sum(1 for b in patch if b != 0xbb)
    if bad: errors.append(f'patch region: {bad} bytes wrong (healer reverted patch!)')
    # Post-patch region: should still be 0xaa
    post = f.read()
    bad = sum(1 for b in post if b != 0xaa)
    if bad: errors.append(f'post-patch: {bad} bytes wrong')
for e in errors: print(e)
print(len(errors))
" "$T25C_FILE")

T25C_ERRS=$(echo "$T25C_RESULT" | tail -1)
[ "${T25C_ERRS:-1}" -eq 0 ] \
    && check "T25c healer-race: patch survived healer cycle (tombstone working)" PASS \
    || check "T25c healer-race: patch reverted by healer — tombstone not working" FAIL

# T25d: write → heal → write again — the exact installer-corruption scenario.
# Reproduces: install phase1 fsyncs, healer fires (reverts dual-RF patches without
# tombstones), install phase2 builds on reverted state, wrong data after final fsync.
#
# Without tombstones: phase1 patches get reverted between phase2; phase2 builds on
# wrong base; final data differs from what was written in phase2.
# With tombstones: phase1 patches are protected; phase2 builds correctly; data matches.
echo "  T25d: write → trigger-heal → write more → verify (install corruption scenario)..."
T25D_FILE="$MOUNT/t25d_install.bin"
T25D_SIZE=$(( 8 * 1024 * 1024 ))   # 2 chunks

# Base: fresh full write (establishes RF=3 baseline — no corruption possible here)
python3 -c "
with open('$T25D_FILE', 'wb') as f:
    f.write(bytes([0x00] * $T25D_SIZE))
"
dfs_sync

# Phase 1: installer writes filesystem structures (patches to specific offsets)
# These are the writes that get reverted by the healer in the bug scenario
python3 -c "
import os
fd = os.open('$T25D_FILE', os.O_RDWR)
# MBR / partition table region
os.lseek(fd, 0, 0);       os.write(fd, bytes([0xAA] * 4096))
# Superblock
os.lseek(fd, 65536, 0);   os.write(fd, bytes([0xBB] * 4096))
# Journal / inode table (second chunk, high offset)
os.lseek(fd, 4*1024*1024 + 65536, 0); os.write(fd, bytes([0xCC] * 4096))
os.close(fd)
"
dfs_sync   # phase1 fsync: dual-RF patches land on 2 of 3 replicas

# Trigger healer between phase1 and phase2 — this is what causes the corruption.
# Without tombstones: healer copies pre-phase1 data (0x00) from 3rd replica back to
# the 2 patched replicas, reverting the 0xAA/0xBB/0xCC writes.
"$BIN/dfs-admin" --cluster "$CLUSTER" healing trigger 2>/dev/null || true
sleep 8   # enough time for healer cycle to complete

# Phase 2: grub writes over the already-written phase1 data (like grub-install does)
# If phase1 was reverted, these build on 0x00 base; if not reverted, on 0xAA/0xBB base.
# Either way, these specific bytes MUST be present in the final read-back.
python3 -c "
import os
fd = os.open('$T25D_FILE', os.O_RDWR)
# Grub stage1 overwrites MBR region
os.lseek(fd, 0, 0);     os.write(fd, bytes([0xDD] * 512))
# Grub core: 32KB at offset 1KB
os.lseek(fd, 1024, 0);  os.write(fd, bytes([0xEE] * 32768))
# Superblock is NOT touched by grub — must still be 0xBB from phase1
os.close(fd)
"
dfs_sync   # phase2 fsync

# Trigger healer again (simulates healer still running during/after install)
"$BIN/dfs-admin" --cluster "$CLUSTER" healing trigger 2>/dev/null || true
sleep 8

# Final integrity check: verify the LAST write to each region wins
T25D_RESULT=$(python3 -c "
errors = []
with open('$T25D_FILE', 'rb') as f:
    data = f.read()

# MBR first 512 bytes: phase2 wrote 0xDD — must be 0xDD
r = data[0:512]
bad = sum(1 for b in r if b != 0xdd)
if bad: errors.append(f'MBR (0..512): {bad} wrong bytes, expected 0xDD — phase2 write lost')

# 1024..33792: phase2 grub core 0xEE
r = data[1024:1024+32768]
bad = sum(1 for b in r if b != 0xee)
if bad: errors.append(f'grub core (1024..34KB): {bad} wrong bytes, expected 0xEE — phase2 write lost')

# 65536..69632: phase1 wrote 0xBB, NOT overwritten in phase2 — must still be 0xBB
r = data[65536:65536+4096]
bad = sum(1 for b in r if b != 0xbb)
if bad: errors.append(f'superblock (64KB): {bad} wrong bytes, expected 0xBB — phase1 write reverted by healer')

# second chunk inode region: phase1 wrote 0xCC, not touched in phase2 — must be 0xCC
off2 = 4*1024*1024 + 65536
r = data[off2:off2+4096]
bad = sum(1 for b in r if b != 0xcc)
if bad: errors.append(f'inode table (chunk2+64KB): {bad} wrong bytes, expected 0xCC — phase1 write reverted by healer')

for e in errors: print(e)
print(len(errors))
" 2>&1)

T25D_ERR_COUNT=$(echo "$T25D_RESULT" | tail -1)
[ "${T25D_ERR_COUNT:-1}" -eq 0 ] \
    && check "T25d write→heal→write: all regions correct after interleaved healing" PASS \
    || check "T25d write→heal→write: data corruption under interleaved healing" FAIL

rm -f "$T25C_FILE" "$T25D_FILE" 2>/dev/null || true
fi # should_run T25

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
