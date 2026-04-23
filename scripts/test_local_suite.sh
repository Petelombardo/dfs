#!/bin/bash
# Local integration test suite: write, read, delete, partial writes, rename, remount persistence, metadata consistency.
set -e

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-mount
LOG=/tmp/dfs-test-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902"
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
rm -rf $BASE $LOG $MOUNT $T
mkdir -p $MOUNT $LOG $T

echo "=== Building ==="
cd "$REPO" && cargo build --release 2>&1 | tail -2

echo "=== Starting 3-node cluster ==="
bash "$REPO/scripts/setup-cluster.sh" 3 2>/dev/null
for i in 1 2 3; do
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
for port in 8900 8901 8902; do
    LIST=$("$BIN/dfs-admin" --cluster "127.0.0.1:$port" file list 2>/dev/null)
    echo "$LIST" | grep -q "t12_after.txt" || { OK=FAIL; echo "  Node $port missing t12_after.txt"; }
    echo "$LIST" | grep -q "t12_before.txt" && { OK=FAIL; echo "  Node $port still has t12_before.txt"; }
    echo "$LIST" | grep -q "t13_dst.bin"   || { OK=FAIL; echo "  Node $port missing t13_dst.bin"; }
    echo "$LIST" | grep -q "t13_src.bin"   && { OK=FAIL; echo "  Node $port still has t13_src.bin"; }
done
check "T14a rename paths propagated to all nodes" $OK

# Verify all nodes agree on the full file list (same set of files)
declare -A NODE_LISTS
for port in 8900 8901 8902; do
    NODE_LISTS[$port]=$("$BIN/dfs-admin" --cluster "127.0.0.1:$port" file list 2>/dev/null \
        | grep -E "^[0-9a-f]{8}" | awk '{print $1, $2, $3}' | sort)
done

if [ "${NODE_LISTS[8900]}" = "${NODE_LISTS[8901]}" ] && [ "${NODE_LISTS[8901]}" = "${NODE_LISTS[8902]}" ]; then
    check "T14b metadata identical on all 3 nodes" PASS
else
    check "T14b metadata consistency" FAIL
    echo "  Node 8900:"
    echo "${NODE_LISTS[8900]}" | sed 's/^/    /'
    echo "  Node 8901:"
    echo "${NODE_LISTS[8901]}" | sed 's/^/    /'
    echo "  Node 8902:"
    echo "${NODE_LISTS[8902]}" | sed 's/^/    /'
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
echo "=== T17: concurrent read while writing (deadlock regression) ==="
dd if=/dev/urandom of="$T/t17_seed.bin" bs=1M count=8 2>/dev/null
cp "$T/t17_seed.bin" "$MOUNT/t17_concurrent.bin"
sleep 0.5

# Generate writer chunks locally (no FUSE blocking), then cp each to mount.
# Using cp rather than dd-append avoids a stuck kernel write if FUSE deadlocks —
# cp can be killed cleanly, dd in append mode cannot (blocks in kernel).
dd if=/dev/urandom of="$T/t17_chunk.bin" bs=1M count=8 2>/dev/null
(
    for i in $(seq 1 4); do
        timeout 10 cp "$T/t17_chunk.bin" "$MOUNT/t17_write_$i.bin" 2>/dev/null || true
        sleep 0.2
    done
) &
WRITER_PID=$!

# Concurrently read from the file; must complete within 15s (not deadlock)
READ_OK=true
for i in $(seq 1 6); do
    if ! timeout 15 dd if="$MOUNT/t17_concurrent.bin" of=/dev/null bs=1M 2>/dev/null; then
        READ_OK=false
        break
    fi
    sleep 0.3
done
# Kill writer and any stuck subprocesses; wait won't hang since cp has timeout
kill $WRITER_PID 2>/dev/null
wait $WRITER_PID 2>/dev/null || true

$READ_OK && check "T17 concurrent read while writing (no deadlock)" PASS \
         || check "T17 concurrent read while writing (DEADLOCK or timeout)" FAIL

# ── cleanup ───────────────────────────────────────────────────────────────────
echo ""
echo "=== Cleanup ==="
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 0.3
kill $CLIENT_PID2 2>/dev/null || true
pkill -f "dfs-server" 2>/dev/null || true
rm -rf "$T"

echo ""
echo "══════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════"
[ $FAIL -eq 0 ] && exit 0 || exit 1
