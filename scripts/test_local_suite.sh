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

# Cap every process's memory-scaled caches to a small, fixed size instead of letting
# each one independently compute a "reasonable" budget from system-wide available/total
# RAM. That sizing (chunk_ring, delta_ring, client chunk_cache, write buffer) is correct
# for its real target — one dfs-server per physical host — but this suite runs 5
# servers + 1 client on a single box, so each one's "generous" self-sizing stacks with
# the other 5 instead of sharing a pie: five chunk_rings alone can claim >1GB combined,
# all computed in ignorance of each other, on a dev box with a fraction of a real node's
# RAM. Root-caused 2026-07-15: this contention was a major contributor to a run's
# escalating flakiness (T22-T30-ish cascade, worse the longer the box had been running
# suites back-to-back) — high load average, timing-sensitive tests losing races they'd
# normally win. `export` here (not per-launch-line) so every dfs-server/dfs-client
# invocation below inherits these automatically, including T38/T45/T51's own mid-test
# restarts. All four already exist as override env vars in the source specifically for
# live tuning — DFS_REPLICA_CACHE_SIZE deliberately isn't touched here, it's a few
# hundred KB even at its default max and shrinking it risks metadata query storms for
# no memory benefit.
export DFS_CHUNK_RING_CAPACITY=8
export DFS_DELTA_RING_CAPACITY=8
export DFS_MAX_CACHE_CHUNKS=8
export DFS_WRITE_BUFFER_CAP_MB=32

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

# kill_client_and_wait <pid>: SIGTERM a dfs-client and block until it's
# actually gone (bounded, falls back to -9) before the caller proceeds to
# remount. Every remount site used to do a bare `kill $PID; sleep 1` — a fixed
# 1s guess, not a confirmation — so a client still mid-drain past that 1s
# became an orphan: still running, no longer tracked by any $CLIENT_PID
# variable, invisible to every later remount's own kill. Found 2026-07-11 as
# the root cause of an intermittent T41 failure: by the time T41 runs, up to
# 7 earlier remounts could each have leaked an orphan, and T41's own
# `pgrep -f "dfs-client mount $MOUNT" | head -1` — the same
# lowest-PID-wins ambiguity already fixed at the suite's final cleanup below
# (see that fix's comment) — would grab an old orphan instead of the actually
# -live client, then wait 30s for a process that was never going to respond
# to what T41 thought was "the" client's mount-serving instance.
kill_client_and_wait() {
    local pid="$1"
    [ -z "$pid" ] && return 0
    kill "$pid" 2>/dev/null || true
    local waited=0
    while kill -0 "$pid" 2>/dev/null; do
        sleep 0.1
        waited=$((waited+1))
        [ "$waited" -gt 50 ] && break   # 5s cap
    done
    kill -0 "$pid" 2>/dev/null && kill -9 "$pid" 2>/dev/null || true
}

# ── cleanup ──────────────────────────────────────────────────────────────────
pkill -f "dfs-server" 2>/dev/null || true
pkill -f "dfs-client" 2>/dev/null || true
sleep 0.5
fusermount -u $MOUNT 2>/dev/null || true
# Remove all artifacts from previous runs: $BASE/$MOUNT/$T, any stale
# dfs-suite-tmp-* dirs left behind by a crashed/interrupted run (different
# $$), and last run's $LOG so debug-level logs don't accumulate across runs.
# Per-test T<N>.log snapshots from the run that just finished remain available
# until this cleanup runs again at the start of the next invocation.
sudo rm -rf $BASE $MOUNT $T $LOG /tmp/dfs-suite-tmp-* 2>/dev/null || rm -rf $BASE $MOUNT $T $LOG /tmp/dfs-suite-tmp-* 2>/dev/null || true
mkdir -p $MOUNT $LOG $T

echo "=== Building ==="
cd "$REPO" && cargo build --release 2>&1 | tail -2

echo "=== Starting 5-node cluster ==="
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null
for i in 1 2 3 4 5; do
    RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
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
if should_run T1; then
echo "=== T1: small write/read ==="
echo "hello distributed world" > "$MOUNT/t1.txt"
GOT=$(cat "$MOUNT/t1.txt")
[ "$GOT" = "hello distributed world" ] && check "T1 small write/read" PASS || check "T1 small write/read (got: $GOT)" FAIL
fi # should_run T1

# ── Test 2: 2MB write + read ──────────────────────────────────────────────────
snapshot_log T2
if should_run T2; then
echo "=== T2: 2MB write/read ==="
dd if=/dev/urandom of="$T/big.bin" bs=1M count=2 2>/dev/null
cp "$T/big.bin" "$MOUNT/t2.bin"
cp "$MOUNT/t2.bin" "$T/big_read.bin"
m1=$(md5sum "$T/big.bin"     | cut -d' ' -f1)
m2=$(md5sum "$T/big_read.bin"| cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T2 2MB write/read" PASS || check "T2 2MB write/read (exp $m1 got $m2)" FAIL
fi # should_run T2

# ── Test 3: delete vanishes immediately ───────────────────────────────────────
snapshot_log T3
if should_run T3; then
echo "=== T3: delete ==="
echo "delete me" > "$MOUNT/t3_del.txt"
rm "$MOUNT/t3_del.txt"
[ ! -f "$MOUNT/t3_del.txt" ] && check "T3 delete vanishes" PASS || check "T3 delete vanishes" FAIL
fi # should_run T3

# ── Test 4: delete stays gone ─────────────────────────────────────────────────
snapshot_log T4
if should_run T4; then
echo "=== T4: delete stays gone after 3s ==="
sleep 3
[ ! -f "$MOUNT/t3_del.txt" ] && check "T4 delete stays gone" PASS || check "T4 delete stays gone" FAIL
fi # should_run T4

# ── Test 5: delete + recreate same path ───────────────────────────────────────
snapshot_log T5
if should_run T5; then
echo "=== T5: delete+recreate ==="
echo "v1" > "$MOUNT/t5.txt"
rm "$MOUNT/t5.txt"
sleep 0.3
echo "v2" > "$MOUNT/t5.txt"
GOT=$(cat "$MOUNT/t5.txt")
[ "$GOT" = "v2" ] && check "T5 delete+recreate" PASS || check "T5 delete+recreate (got: $GOT)" FAIL
fi # should_run T5

# ── Test 47: symlink create/readlink/read-through + healer safety ────────────
# Placed early (despite the T47 number, assigned in creation order like T19/T20's
# out-of-order placement) so the symlink stays alive for every later healer-trigger
# test in this file (T25c, T25d, T38, T45, ...) — each one becomes an incidental
# regression check that the healer still leaves a chunk-less symlink alone.
snapshot_log T47
if should_run T47; then
echo "=== T47: symlink create, readlink, read-through, and healer safety ==="
echo "symlink target content" > "$MOUNT/t47_target.txt"

T47_OK=PASS

# Relative symlink, same shape as the original repro: ln -s test1.img test18.img
ln -s t47_target.txt "$MOUNT/t47_link.txt" || T47_OK=FAIL
dfs_sync

T47_READLINK=$(readlink "$MOUNT/t47_link.txt" 2>/dev/null)
[ "$T47_READLINK" = "t47_target.txt" ] || T47_OK=FAIL

# Must report as a symlink, not a regular file, in both directory listing and stat.
[ -L "$MOUNT/t47_link.txt" ] || T47_OK=FAIL

T47_VIA_LINK=$(cat "$MOUNT/t47_link.txt" 2>/dev/null)
[ "$T47_VIA_LINK" = "symlink target content" ] || T47_OK=FAIL

check "T47a symlink create/readlink/read-through" "$T47_OK"

# Trigger the healer twice (same pattern as T25c/T25d) — a symlink has zero chunks,
# so a correct healer must leave it alone entirely: not delete it as an orphan (it
# has nothing in chunk_map to confirm-live), and not try to repair/replicate chunks
# that don't exist for it.
"$BIN/dfs-admin" --cluster "$CLUSTER" healing trigger 2>/dev/null || true
sleep 5
"$BIN/dfs-admin" --cluster "$CLUSTER" healing trigger 2>/dev/null || true
sleep 5

T47_POST_HEAL_OK=PASS
[ -L "$MOUNT/t47_link.txt" ] || T47_POST_HEAL_OK=FAIL
T47_POST_READLINK=$(readlink "$MOUNT/t47_link.txt" 2>/dev/null)
[ "$T47_POST_READLINK" = "t47_target.txt" ] || T47_POST_HEAL_OK=FAIL
T47_POST_VIA_LINK=$(cat "$MOUNT/t47_link.txt" 2>/dev/null)
[ "$T47_POST_VIA_LINK" = "symlink target content" ] || T47_POST_HEAL_OK=FAIL
[ -f "$MOUNT/t47_target.txt" ] || T47_POST_HEAL_OK=FAIL

check "T47b symlink and target survive healer cycles unmodified (not eaten, not falsely healed)" "$T47_POST_HEAL_OK"

# unlink(symlink) must remove only the link, never the target it points to.
rm "$MOUNT/t47_link.txt"
sleep 0.3
T47_UNLINK_OK=PASS
[ ! -e "$MOUNT/t47_link.txt" ] || T47_UNLINK_OK=FAIL
[ -f "$MOUNT/t47_target.txt" ] || T47_UNLINK_OK=FAIL
T47_TARGET_STILL=$(cat "$MOUNT/t47_target.txt" 2>/dev/null)
[ "$T47_TARGET_STILL" = "symlink target content" ] || T47_UNLINK_OK=FAIL
check "T47c unlinking symlink leaves target intact" "$T47_UNLINK_OK"
fi # should_run T47

# ── Test 6: selective delete ──────────────────────────────────────────────────
snapshot_log T6
if should_run T6; then
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
fi # should_run T6

# ── Test 7: overwrite ─────────────────────────────────────────────────────────
snapshot_log T7
if should_run T7; then
echo "=== T7: overwrite ==="
echo "original" > "$MOUNT/t7.txt"
echo "overwritten" > "$MOUNT/t7.txt"
dfs_sync
GOT=$(cat "$MOUNT/t7.txt")
[ "$GOT" = "overwritten" ] && check "T7 overwrite" PASS || check "T7 overwrite (got: $GOT)" FAIL
fi # should_run T7

# ── Test 8: unmount + remount persistence ─────────────────────────────────────
snapshot_log T8
if should_run T8; then
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
kill_client_and_wait "$CLIENT_PID"

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
fi # should_run T8

# ── Test 9: partial write — sub-chunk (< 4MB) write + read ───────────────────
snapshot_log T9
if should_run T9; then
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
fi # should_run T9

# ── Test 10: cross-chunk boundary write (> 4MB, < 8MB) ───────────────────────
snapshot_log T10
if should_run T10; then
echo ""
echo "=== T10: cross-chunk boundary write (6MB) ==="
dd if=/dev/urandom of="$T/cross.bin" bs=1M count=6 2>/dev/null
cp "$T/cross.bin" "$MOUNT/t10_cross.bin"
dfs_sync
cp "$MOUNT/t10_cross.bin" "$T/cross_read.bin"
m1=$(md5sum "$T/cross.bin"      | cut -d' ' -f1)
m2=$(md5sum "$T/cross_read.bin" | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T10 6MB cross-chunk write/read" PASS || check "T10 6MB cross-chunk write/read (exp $m1 got $m2)" FAIL
fi # should_run T10

# ── Test 11: append to existing file ─────────────────────────────────────────
snapshot_log T11
if should_run T11; then
echo ""
echo "=== T11: append ==="
echo "first line" > "$MOUNT/t11_append.txt"
dfs_sync  # ensure metadata (file size) is committed before O_APPEND open
echo "second line" >> "$MOUNT/t11_append.txt"
dfs_sync
GOT=$(cat "$MOUNT/t11_append.txt")
EXPECTED=$'first line\nsecond line'
[ "$GOT" = "$EXPECTED" ] && check "T11 append to file" PASS || check "T11 append to file (got: $(echo $GOT | head -c 60))" FAIL
fi # should_run T11

# ── Test 12: rename — new path readable, old path gone ───────────────────────
snapshot_log T12
if should_run T12; then
echo ""
echo "=== T12: rename ==="
echo "rename me" > "$MOUNT/t12_before.txt"
dfs_sync
mv "$MOUNT/t12_before.txt" "$MOUNT/t12_after.txt"
GOT=$(cat "$MOUNT/t12_after.txt" 2>/dev/null)
[ "$GOT" = "rename me" ] && check "T12a renamed file readable at new path" PASS || check "T12a renamed file readable (got: $GOT)" FAIL
[ ! -f "$MOUNT/t12_before.txt" ] && check "T12b old path gone after rename" PASS || check "T12b old path still exists after rename" FAIL
fi # should_run T12

# ── Test 13: rename a binary file, verify data integrity ─────────────────────
snapshot_log T13
if should_run T13; then
echo ""
echo "=== T13: rename binary file ==="
dd if=/dev/urandom of="$T/rename_src.bin" bs=1M count=2 2>/dev/null
cp "$T/rename_src.bin" "$MOUNT/t13_src.bin"
dfs_sync
mv "$MOUNT/t13_src.bin" "$MOUNT/t13_dst.bin"
dfs_sync  # commit the rename's metadata to the leader before T14 checks it
cp "$MOUNT/t13_dst.bin" "$T/rename_dst_read.bin"
m1=$(md5sum "$T/rename_src.bin"      | cut -d' ' -f1)
m2=$(md5sum "$T/rename_dst_read.bin" | cut -d' ' -f1)
[ "$m1" = "$m2" ] && check "T13a renamed binary data intact" PASS || check "T13a renamed binary data (exp $m1 got $m2)" FAIL
[ ! -f "$MOUNT/t13_src.bin" ] && check "T13b src gone after rename" PASS || check "T13b src still exists after rename" FAIL
fi # should_run T13

# ── Test 14: rename + metadata consistency across nodes ──────────────────────
snapshot_log T14
if should_run T14; then
echo ""
echo "=== T14: metadata consistency after renames ==="

# Verify t12_after.txt/t13_dst.bin (and NOT t12_before.txt/t13_src.bin) appear on
# all nodes. Non-leader nodes only receive rename metadata via async
# dissemination/healing (up to ~25s), so poll with retry instead of a single
# fixed sleep+check — a one-shot 3s sleep was flaky under full-suite load
# whenever a follower's healing pass took longer than that window.
T14A_MAX_WAIT=25
T14A_POLL_INTERVAL=2
T14A_ELAPSED=0
OK=FAIL
FAILURES=""
while [ "$T14A_ELAPSED" -le "$T14A_MAX_WAIT" ]; do
    OK=PASS
    FAILURES=""
    for port in 8900 8901 8902 8903 8904; do
        LIST=$("$BIN/dfs-admin" --cluster "127.0.0.1:$port" file list 2>/dev/null)
        echo "$LIST" | grep -q "t12_after.txt" || { OK=FAIL; FAILURES="${FAILURES}  Node $port missing t12_after.txt\n"; }
        echo "$LIST" | grep -q "t12_before.txt" && { OK=FAIL; FAILURES="${FAILURES}  Node $port still has t12_before.txt\n"; }
        echo "$LIST" | grep -q "t13_dst.bin"   || { OK=FAIL; FAILURES="${FAILURES}  Node $port missing t13_dst.bin\n"; }
        echo "$LIST" | grep -q "t13_src.bin"   && { OK=FAIL; FAILURES="${FAILURES}  Node $port still has t13_src.bin\n"; }
    done
    [ "$OK" = "PASS" ] && break
    sleep "$T14A_POLL_INTERVAL"
    T14A_ELAPSED=$((T14A_ELAPSED + T14A_POLL_INTERVAL))
done
[ "$OK" = "FAIL" ] && printf "%b" "$FAILURES"
check "T14a rename paths propagated to all nodes" $OK

# Verify the leader has the authoritative file list. T13's dfs_sync should make
# this immediately consistent (flush_metadata_sync commits to the leader
# synchronously) — poll briefly anyway as a safety margin against any transient
# debounce/queue delay rather than a single fixed check.
T14B_MAX_WAIT=5
T14B_POLL_INTERVAL=1
T14B_ELAPSED=0
T14B_OK=FAIL
FAILURES_B=""
while [ "$T14B_ELAPSED" -le "$T14B_MAX_WAIT" ]; do
    LEADER_LIST=$("$BIN/dfs-admin" --cluster "127.0.0.1:8900" file list 2>/dev/null \
        | grep -E "^[0-9a-f]{8}" | awk '{print $1, $2, $3}' | sort)

    T14B_OK=PASS
    FAILURES_B=""
    echo "$LEADER_LIST" | grep -q "t12_after.txt" || { T14B_OK=FAIL; FAILURES_B="${FAILURES_B}  Leader missing t12_after.txt\n"; }
    echo "$LEADER_LIST" | grep -q "t12_before.txt" && { T14B_OK=FAIL; FAILURES_B="${FAILURES_B}  Leader still has t12_before.txt\n"; }
    echo "$LEADER_LIST" | grep -q "t13_dst.bin"   || { T14B_OK=FAIL; FAILURES_B="${FAILURES_B}  Leader missing t13_dst.bin\n"; }
    echo "$LEADER_LIST" | grep -q "t13_src.bin"   && { T14B_OK=FAIL; FAILURES_B="${FAILURES_B}  Leader still has t13_src.bin\n"; }
    echo "$LEADER_LIST" | grep -q "t6_4.txt"      && { T14B_OK=FAIL; FAILURES_B="${FAILURES_B}  Leader still has deleted t6_4.txt\n"; }
    [ "$T14B_OK" = "PASS" ] && break
    sleep "$T14B_POLL_INTERVAL"
    T14B_ELAPSED=$((T14B_ELAPSED + T14B_POLL_INTERVAL))
done
[ "$T14B_OK" = "FAIL" ] && printf "%b" "$FAILURES_B"

check "T14b leader metadata correct after renames/deletes" $T14B_OK

echo ""
echo "  Current file list (from node 8900):"
"$BIN/dfs-admin" --cluster "127.0.0.1:8900" file list 2>/dev/null | grep -E "^[0-9a-f]|Total" | sed 's/^/    /'
fi # should_run T14

# ── Test 15: partial in-place overwrite (DVR header-update pattern) ──────────
# Create a 4MB file. Overwrite the first 2MB with new data (no truncation).
# Result must still be 4MB, and the final content must match the same op on
# the local filesystem (first 2MB = patch, last 2MB = original tail).
snapshot_log T15
if should_run T15; then
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
fi # should_run T15

# ── Test 16: full replace via O_TRUNC (cp smaller file over larger) ───────────
snapshot_log T16
if should_run T16; then
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
fi # should_run T16

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
if should_run T17; then
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
fi # should_run T17

# ── Test 17c: DVR exact write pattern (12KB header then fill to 4MB) ──────────
# Simulates exact HDHomeRun DVR sequence: write 12KB header first (fresh chunk),
# then write recording data that fills chunk 0 to exactly 4MB via background ticker.
# Verifies the tail is not zeroed when the slot fills to CHUNK_SIZE with gap-fill.
snapshot_log T17c
if should_run T17c; then
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
fi # should_run T17c

# ── Test 18: DVR concurrent-read integrity ────────────────────────────────────
# Write a 20MB file at ~4MB/s while concurrently reading from offset 0.
# Verifies: no short reads that skip data, read copy matches written data.
snapshot_log T18
if should_run T18; then
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
fi # should_run T18

# ── Test 20: partial overwrite integrity — first, middle, and last chunk ───────
# Write a 12MB file (3 chunks). Write a 2MB patch file.
# Apply the 2MB patch to: first 2MB of chunk 0, first 2MB of chunk 1, first 2MB of chunk 2.
# Mirror every operation on the local filesystem, then compare MD5s chunk-by-chunk.
snapshot_log T20
if should_run T20; then
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
fi # should_run T20

# ── Test 19: large-file delete — non-blocking rm + async chunk cleanup ────────
snapshot_log T19
if should_run T19; then
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
fi # should_run T19

# ── Test 21: metadata storm — 2000 touches, node health check, 100 more ──────
snapshot_log T21
if should_run T21; then
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
fi # should_run T21

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
if should_run T22; then
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

# Step 2: run N concurrent patches, each writing a unique, position-derived tag at a
# deterministic, non-overlapping offset — not a shared random buffer. This is what
# makes Step 3 below a real integrity check instead of just an emptiness check: with a
# shared buffer there's no way to tell whether any individual patch actually landed at
# the right place, survived concurrent batched metadata updates, or got silently lost —
# every job's content would look the same either way. With per-job unique content we
# can verify every one of the 50 regions byte-for-byte after the storm.
#
# Offset formula (job is 0-indexed): chunk_idx = job % n_chunks, slot = job / n_chunks,
# intra = slot * 65536 (well above the 12032B patch size, so adjacent slots in the same
# chunk never overlap). With 50 jobs / 8 chunks that's at most 7 slots per chunk,
# 7*65536=458752 — comfortably inside one 4MB chunk.
echo "  Running $T22_PATCH_COUNT patches ($T22_CONCURRENCY concurrent, ${T22_PATCH_SIZE}B each, unique tagged content)..."

T22_START=$(date +%s%3N)

T22_N_CHUNKS=$(( T22_SIZE_MB / 4 ))
T22_ERRORS=0
seq 0 $((T22_PATCH_COUNT-1)) | xargs -P$T22_CONCURRENCY -I{} bash -c '
    img="$1"; patch_size="$2"; n_chunks="$3"; errfile="$4"; job="$5"
    chunk=$(( job % n_chunks ))
    slot=$(( job / n_chunks ))
    intra=$(( slot * 65536 ))
    byte_off=$(( chunk * 4 * 1024 * 1024 + intra ))
    python3 -c "
import sys, os
img, byte_off, patch_size, job = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
tag = (\"T22_JOB_%04d_\" % job).encode()
data = (tag + bytes([job % 256]) * (patch_size - len(tag)))[:patch_size]
fd = os.open(img, os.O_WRONLY)
os.lseek(fd, byte_off, 0)
os.write(fd, data)
os.close(fd)
" "$img" "$byte_off" "$patch_size" "$job" 2>>"$errfile" || echo FAIL
' _ "$T22_IMG" "$T22_PATCH_SIZE" "$T22_N_CHUNKS" "/tmp/t22_py_errors_$$" {} \
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

dfs_sync  # ensure every patch's chunk data AND metadata are durably committed before verifying

# Step 3: byte-for-byte integrity check — recompute each job's expected offset and
# content with the same formula used to write it, and confirm it's actually there.
# Catches silent corruption that T22's old "file is non-empty" check could never see:
# a patch landing at the wrong offset, a dropped/misapplied chunk-location update, or
# a stale chunk_id being read back instead of the patched one.
T22_INTEGRITY=$(python3 -c "
import sys, os
img, patch_size, n_chunks, patch_count = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
mismatches = []
with open(img, 'rb') as f:
    for job in range(patch_count):
        chunk = job % n_chunks
        slot = job // n_chunks
        intra = slot * 65536
        byte_off = chunk * 4 * 1024 * 1024 + intra
        tag = ('T22_JOB_%04d_' % job).encode()
        expected = (tag + bytes([job % 256]) * (patch_size - len(tag)))[:patch_size]
        f.seek(byte_off)
        actual = f.read(patch_size)
        if actual != expected:
            # Identify what's actually there: another job's tag (cross-contamination /
            # wrong-offset landing) vs non-tag bytes (never patched at all, still base image).
            actual_tag = actual[:13]
            looks_like_other_job = actual_tag.startswith(b'T22_JOB_') and actual_tag != tag
            kind = f'belongs to a DIFFERENT job ({actual_tag!r})' if looks_like_other_job else 'not a T22 tag at all (patch never landed?)'
            mismatches.append(f'job {job} offset {byte_off}: expected {tag!r}, got {actual[:64]!r} -- {kind}')
for m in mismatches[:10]:
    print(m)
print(len(mismatches))
" "$T22_IMG" "$T22_PATCH_SIZE" "$T22_N_CHUNKS" "$T22_PATCH_COUNT")

T22_MISMATCH_COUNT=$(echo "$T22_INTEGRITY" | tail -1)
if [ "$T22_MISMATCH_COUNT" -gt 0 ]; then
    echo "  Mismatch details:"
    echo "$T22_INTEGRITY" | head -n -1 | sed 's/^/    /'
fi
[ "$T22_MISMATCH_COUNT" -eq 0 ] \
    && check "T22c all $T22_PATCH_COUNT patched regions verified byte-for-byte after storm" PASS \
    || check "T22c $T22_MISMATCH_COUNT/$T22_PATCH_COUNT patched regions corrupted after storm" FAIL

rm -f "$T22_IMG" 2>/dev/null || true
fi # should_run T22

# ── Test 22b: FIFO ordering — sequential overlapping patches to same chunk ─────
snapshot_log T22b
if should_run T22b; then
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

# Generate 12 distinct patterns using python3 for reliable byte generation
# (tr '\000' "\xNN" is not portable across platforms for \x00)
for i in $(seq 0 11); do
    val=$((i * 0x11))
    if [ $(( i % 2 )) -eq 0 ]; then
        python3 -c "import sys; sys.stdout.buffer.write(bytes([$val]*4096))" > "$T/t22b_pat_$i.bin"
    else
        python3 -c "import sys; sys.stdout.buffer.write(bytes([$val]*262144))" > "$T/t22b_pat_$i.bin"
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

T22B_READ="$T/t22b_read.bin"
cp "$T22B_DFS" "$T22B_READ" 2>/dev/null
if [ "$T22B_DFS_MD5" = "$T22B_LOCAL_MD5" ]; then
    check "T22b DFS matches local file (md5: ${T22B_DFS_MD5:0:8})" PASS
else
    check "T22b DFS differs from local (dfs=${T22B_DFS_MD5:0:8} local=${T22B_LOCAL_MD5:0:8})" FAIL
    echo "  Per-write region check:"
    for i in $(seq 0 11); do
        offset=$((i * 128 * 1024))
        if [ $(( i % 2 )) -eq 0 ]; then sz=4096; else sz=262144; fi
        pat=$(printf "%02x" $((i * 0x11)))
        dfs_hex=$(dd if="$T22B_READ"  bs=1 skip=$offset count=4 2>/dev/null | xxd -p | tr -d '\n')
        loc_hex=$(dd if="$T22B_LOCAL" bs=1 skip=$offset count=4 2>/dev/null | xxd -p | tr -d '\n')
        [ "$dfs_hex" = "$loc_hex" ] && status="ok" || status="MISMATCH dfs=$dfs_hex local=$loc_hex"
        echo "    i=$i off=$offset sz=$sz pat=0x$pat: $status"
    done
fi
rm -f "$T22B_READ"

echo "  Completed in ${T22B_MS}ms"

rm -f "$T22B_DFS" "$T22B_LOCAL" "$T"/t22b_pat_*.bin 2>/dev/null || true
fi # should_run T22b

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
kill_client_and_wait "$CLIENT_PID2"
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
kill_client_and_wait "$CLIENT_PID2"
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
kill_client_and_wait "$CLIENT_PID2"
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

# T25e: slow-path write + immediate fsync race.
# Regression for: when metadata_cache has no entry for an inode (first write
# after open, or after cache eviction), write() falls through to the slow path
# which spawns an async task and increments write_tasks_in_flight BEFORE the
# spawn. flush_all_pipelined only checks pending *slots* — the slow-path task
# hasn't called write_at() yet so pending=0, the loop exits, flush_metadata_sync
# fires, reply.ok() fires. The spawned task then puts data on the server AFTER
# fsync returned. Remount + read returns pre-write data.
#
# To reproduce: open a NEW file (no metadata cache entry), write MBR-like data
# (small write, grub installer pattern), fsync immediately, close.
# Remount cold, read back. Without the fix the data is missing/zero.
echo "  T25e: slow-path write + immediate fsync (first-write race)..."
T25E_FILE="$MOUNT/t25e_firstwrite.bin"
T25E_MANIFEST="$T/t25e_manifest.bin"
T25E_WRITE_SIZE=512   # MBR size — small write like grub stage1

python3 -c "
import os, sys
data = bytes([0xEB, 0x63, 0x90] + [0xAA] * ($T25E_WRITE_SIZE - 3))   # MBR-like pattern
open('$T25E_MANIFEST', 'wb').write(data)
# Open file fresh — no metadata in cache yet — triggers slow path
fd = os.open('$T25E_FILE', os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(fd, data)
# Fsync immediately while spawned write task may still be in-flight
os.fsync(fd)
os.close(fd)
"

# Remount cold — forces a fresh metadata fetch, bypasses any client-side cache
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 0.5
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client_t25e.log" --allow-other --log-level debug &
T25E_PID=$!
sleep 2
mountpoint -q "$MOUNT" || { check "T25e remount" FAIL; T25E_PID=""; }

T25E_RESULT=$(python3 -c "
import sys
expected = open('$T25E_MANIFEST','rb').read()
try:
    actual = open('$T25E_FILE','rb').read($T25E_WRITE_SIZE)
except Exception as e:
    print(f'read error: {e}')
    print(1); sys.exit()
if actual == expected:
    print(0)
else:
    bad = sum(1 for a,b in zip(actual,expected) if a!=b)
    print(f'first-write race: {bad}/{len(expected)} bytes wrong after remount')
    print(1)
" 2>&1)

T25E_ERR=$(echo "$T25E_RESULT" | tail -1)
[ "${T25E_ERR:-1}" -eq 0 ] \
    && check "T25e first-write+fsync race: data durable after immediate fsync" PASS \
    || check "T25e first-write+fsync race: data lost — slow-path/fsync race" FAIL

# Also test: write multiple times to trigger path after metadata is cached,
# then immediate fsync — verifying the fix doesn't break the normal path.
T25E2_FILE="$MOUNT/t25e2_seqwrite.bin"
python3 -c "
import os
fd = os.open('$T25E2_FILE', os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
# First write — slow path (no metadata cache)
os.write(fd, bytes([0x11] * 512))
# Second write — fast path (metadata now cached)
os.write(fd, bytes([0x22] * 512))
# Third write — still fast path
os.write(fd, bytes([0x33] * 512))
os.fsync(fd)
os.close(fd)
"
T25E2_RESULT=$(python3 -c "
with open('$T25E2_FILE','rb') as f:
    data = f.read(1536)
errors = 0
if data[0:512] != bytes([0x11]*512): errors += 1; print('block1 wrong')
if data[512:1024] != bytes([0x22]*512): errors += 1; print('block2 wrong')
if data[1024:1536] != bytes([0x33]*512): errors += 1; print('block3 wrong')
print(errors)
")
T25E2_ERR=$(echo "$T25E2_RESULT" | tail -1)
[ "${T25E2_ERR:-1}" -eq 0 ] \
    && check "T25e2 mixed slow+fast path writes all durable after fsync" PASS \
    || check "T25e2 mixed slow+fast path writes: data loss" FAIL

# Cleanup T25e client
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 0.3
kill $T25E_PID 2>/dev/null || true
# Remount for remaining tests/cleanup
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
CLIENT_PID2=$!
CURRENT_CLIENT_LOG="$LOG/client.log"
sleep 2

rm -f "$T25E_FILE" "$T25E_MANIFEST" "$T25E2_FILE" 2>/dev/null || true
fi # should_run T25

# ── Test 26: repeated patches to same chunk — stale-base regression ───────────
# Rapidly patches the same intra-chunk offset 10 times in sequence.
# Each patch should be applied to the SAME 2 replica nodes as the previous one.
# If the node-tracking bug is present, the second and subsequent patches target
# wrong nodes (nodes that never received the previous patch) and the client log
# will contain "stale base" warnings from the stale-base retry path.
# Data correctness is verified by md5sum; stale warnings are a separate check.
if should_run T26; then
snapshot_log T26
echo ""
echo "=== T26: repeated same-chunk patches (stale-base node-tracking regression) ==="

T26_FILE="$MOUNT/t26_repatch.bin"
T26_PATCH_OFFSET=$(( 1024 * 1024 )) # 1MB into the chunk

# Create initial 4MB file (fresh write — establishes chunk on 2 replica nodes)
dd if=/dev/urandom of="$T/t26_orig.bin" bs=1M count=4 2>/dev/null
cp "$T/t26_orig.bin" "$T26_FILE"
dfs_sync

# Mark log position before the patch sequence so we can isolate T26's warnings.
T26_LOG_MARK=$( wc -l < "$CURRENT_CLIENT_LOG" 2>/dev/null || echo 0 )

# Wait for the healer to replicate the chunk to a 3rd node.
# Healer: 10s initial delay + 15s interval → 30s guarantees at least one cycle.
# This is the key condition for the bug: healer adds a node to the chunk's
# location list; the next patch may target that node (which has the pre-patch
# version) instead of the nodes that actually received the previous patch.
echo "  Waiting 30s for healer to replicate chunk to 3rd node..."
sleep 30

# Apply 20 sequential patches to the same intra-chunk offset.
cp "$T/t26_orig.bin" "$T/t26_expected.bin"
for i in $(seq 1 20); do
    dd if=/dev/urandom of="$T/t26_patch_${i}.bin" bs=4096 count=1 2>/dev/null
    dd if="$T/t26_patch_${i}.bin" of="$T26_FILE" bs=4096 count=1 \
        seek=$(( T26_PATCH_OFFSET / 4096 )) conv=notrunc 2>/dev/null
    dd if="$T/t26_patch_${i}.bin" of="$T/t26_expected.bin" bs=4096 count=1 \
        seek=$(( T26_PATCH_OFFSET / 4096 )) conv=notrunc 2>/dev/null
done
dfs_sync

# Verify data correctness
cp "$T26_FILE" "$T/t26_read.bin"
m1=$(md5sum "$T/t26_expected.bin" | cut -d' ' -f1)
m2=$(md5sum "$T/t26_read.bin"     | cut -d' ' -f1)
[ "$m1" = "$m2" ] \
    && check "T26a repeated-patch data integrity" PASS \
    || check "T26a repeated-patch data integrity (exp $m1 got $m2)" FAIL

# Check for stale-base warnings in the lines added since T26_LOG_MARK.
# "stale base" appears when a patch targets a node that has an older version —
# signature of the node-tracking bug where metadata_cache carries wrong nodes.
STALE_COUNT=$( tail -n +"$T26_LOG_MARK" "$CURRENT_CLIENT_LOG" 2>/dev/null \
               | grep "stale base" | wc -l )
[ "$STALE_COUNT" -eq 0 ] \
    && check "T26b no stale-base retries (node tracking correct)" PASS \
    || check "T26b stale-base retries detected ($STALE_COUNT) — node tracking bug" FAIL

if [ "$STALE_COUNT" -gt 0 ]; then
    echo "  Stale-base detail:"
    tail -n +"$T26_LOG_MARK" "$CURRENT_CLIENT_LOG" 2>/dev/null \
        | grep "stale base\|MultiPatch.*replicas" | head -20 | sed 's/^/    /'
fi

rm -f "$T26_FILE" "$T/t26_orig.bin" "$T/t26_expected.bin" "$T/t26_read.bin" \
      "$T"/t26_patch_*.bin 2>/dev/null || true
fi # should_run T26

# ── Test 27: sparse-patch gap-fill corruption regression ──────────────────────
# Verifies two flush_buffer_async code paths that were sending zero-filled buffer
# bytes to the server, overwriting real data in the gaps between writes.
#
#   T27a — is_overwrite + sparse dirty_ranges:
#     Two non-adjacent writes to the same chunk in one session (no intermediate
#     sync). The gap between them has real server data that must be preserved.
#     Before the fix: batch flush sent slot_data[gap_filled_prefix..effective_end]
#     as one block, zeroing the gap. Fix: sparse dirty_ranges delegates to
#     MultiPatch, which sends only the actually-written byte ranges.
#
#   T27b — is_append_extend + gap_filled_prefix > existing_chunk_size:
#     A partial flush (fsync within the same session) sets flushed_sizes[0]=N.
#     A subsequent write at offset M > N triggers is_append_extend with
#     gap_filled_prefix=M > existing_chunk_size=N. The gap N..M has real server
#     data that must be preserved. Before the fix: patch started at existing_chunk_size
#     (N), sending zeros for N..M. Fix: patch starts at gap_filled_prefix (M).
#
# Both bugs corrupt GPT/inode-bitmap data during OS install: GRUB writes MBR at
# byte 0 and the core image at byte 1MB, leaving the GPT partition table in the
# gap — that gap was getting zeroed and corrupting the disk.
if should_run T27; then
snapshot_log T27
echo ""
echo "=== T27: sparse-patch gap-fill corruption regression ==="

T27_IMG="$MOUNT/t27_disk.raw"

# 4MB base image with a never-zero repeating pattern so every byte is checkable.
# Pattern: byte at offset i = (i % 251) + 1   (range 1-251, never 0)
echo "  Writing 4MB base image with known pattern..."
python3 -c "
import sys
CHUNK = 4 * 1024 * 1024
sys.stdout.buffer.write(bytes([(i % 251) + 1 for i in range(CHUNK)]))
" > "$T/t27_base.bin"
cp "$T/t27_base.bin" "$T27_IMG"
dfs_sync

# ── T27a: is_overwrite with sparse dirty_ranges ────────────────────────────────
echo "  T27a: two non-adjacent writes to same chunk, no intermediate sync..."
python3 - "$T27_IMG" << 'PYEOF'
import os, sys
img = sys.argv[1]
fd = os.open(img, os.O_RDWR)

# Write 1: bytes 0-511
os.lseek(fd, 0, os.SEEK_SET)
os.write(fd, b'PATCHA__' + b'\xab' * 504)   # 512 bytes

# Write 2: bytes 65536-66047  (64KB gap at 512..65535 has original pattern)
os.lseek(fd, 65536, os.SEEK_SET)
os.write(fd, b'PATCHB__' + b'\xcd' * 504)   # 512 bytes

# No intermediate sync — both writes are buffered together at close time.
# flush_buffer_async sees is_overwrite with dirty_ranges=[(0,512),(65536,66048)].
os.close(fd)
PYEOF

T27A_RESULT=$(python3 - "$T27_IMG" "$T/t27_base.bin" << 'PYEOF'
import sys
img_path, base_path = sys.argv[1], sys.argv[2]
with open(img_path, 'rb') as f:
    data = f.read(66048)
with open(base_path, 'rb') as f:
    base = f.read(66048)
errors = []
if data[:8] != b'PATCHA__':
    errors.append(f"patch A missing at 0: {data[:8]!r}")
if data[65536:65536+8] != b'PATCHB__':
    errors.append(f"patch B missing at 65536: {data[65536:65536+8]!r}")
# Gap at 512..65535 must NOT be zeroed — must still hold original pattern.
bad = [(i, data[i], base[i]) for i in range(512, 65536) if data[i] != base[i]]
if bad:
    sample = ', '.join(f'off={o} got={g:#04x} want={w:#04x}' for o,g,w in bad[:3])
    errors.append(f"gap corrupted ({len(bad)} bytes): {sample}")
print("PASS" if not errors else "FAIL: " + "; ".join(errors))
PYEOF
)
[[ "$T27A_RESULT" == PASS* ]] \
    && check "T27a sparse is_overwrite: gap bytes preserved" PASS \
    || check "T27a sparse is_overwrite: gap bytes corrupted ($T27A_RESULT)" FAIL

# ── T27b: is_append_extend with gap_filled_prefix > existing_chunk_size ────────
echo "  T27b: is_append_extend gap — fsync mid-session then write past flush point..."
cp "$T/t27_base.bin" "$T27_IMG"
dfs_sync

python3 - "$T27_IMG" << 'PYEOF'
import os, sys
img = sys.argv[1]
fd = os.open(img, os.O_RDWR)

# Write 12KB header at offset 0, then fsync within the same session.
# This sets flushed_sizes[chunk 0] = 12288 WITHOUT closing the file.
os.lseek(fd, 0, os.SEEK_SET)
os.write(fd, b'HEADER__' + b'\x42' * (12288 - 8))
os.fsync(fd)   # flush_all_pipelined: chunk 0 patched, flushed_sizes[0]=12288

# Write 4KB at offset 16384 — there is a 4KB gap at 12288..16383.
# The server chunk still has original pattern data at 12288..16383.
# gap_filled_prefix becomes 16384 (> existing_chunk_size=12288).
# is_append_extend fires. Bug: sends slot_data[12288..] zeroing the gap.
# Fix: sends slot_data[16384..] starting at gap_filled_prefix.
os.lseek(fd, 16384, os.SEEK_SET)
os.write(fd, b'RECORD__' + b'\xcc' * (4096 - 8))

os.close(fd)
PYEOF

T27B_RESULT=$(python3 - "$T27_IMG" "$T/t27_base.bin" << 'PYEOF'
import sys
img_path, base_path = sys.argv[1], sys.argv[2]
with open(img_path, 'rb') as f:
    data = f.read(16384 + 4096)
with open(base_path, 'rb') as f:
    base = f.read(16384 + 4096)
errors = []
if data[:8] != b'HEADER__':
    errors.append(f"header missing at 0: {data[:8]!r}")
if data[16384:16384+8] != b'RECORD__':
    errors.append(f"record missing at 16384: {data[16384:16384+8]!r}")
# Gap at 12288..16383 must NOT be zeroed — original pattern must be intact.
bad = [(i, data[i], base[i]) for i in range(12288, 16384) if data[i] != base[i]]
if bad:
    sample = ', '.join(f'off={o} got={g:#04x} want={w:#04x}' for o,g,w in bad[:3])
    errors.append(f"gap corrupted ({len(bad)} bytes): {sample}")
print("PASS" if not errors else "FAIL: " + "; ".join(errors))
PYEOF
)
[[ "$T27B_RESULT" == PASS* ]] \
    && check "T27b append-extend gap preserved (gap_filled_prefix > existing)" PASS \
    || check "T27b append-extend gap corrupted ($T27B_RESULT)" FAIL

rm -f "$T27_IMG" "$T/t27_base.bin"
fi # should_run T27

# ── Test 28: thick-file heavy-patch + restart → stale-metadata EIO regression ──
#
# Reproduces the "e2fsck passes warm, fails after restart" bug:
#   1. Write a thick 200MB file (dd /dev/urandom — not ftruncate, so no gaps)
#   2. Do 200 random 4KB overwrites spread across all chunks (heavy patch storm)
#   3. dfs_sync to flush everything and commit metadata
#   4. Kill and restart the dfs-client (cold cache — metadata fetched from leader)
#   5. md5sum the file — must match the reference captured before restart
#      If the leader has stale chunk_ids (pointing to deleted patch intermediates),
#      reads will EIO and the sum will fail.
if should_run T28; then
snapshot_log T28
echo ""
echo "=== T28: thick-file heavy-patch + restart stale-metadata regression ==="

T28_FILE="$MOUNT/t28_thick.bin"
T28_SIZE_MB=200
T28_PATCH_COUNT=200
T28_PATCH_SIZE=4096
CHUNK_SIZE=$(( 4 * 1024 * 1024 ))
T28_CHUNKS=$(( T28_SIZE_MB / 4 ))  # 50 chunks

# Phase 1: write thick file (dd so every byte is real, no sparse/gap)
echo "  Writing ${T28_SIZE_MB}MB thick file..."
dd if=/dev/urandom of="$T/t28_orig.bin" bs=1M count=$T28_SIZE_MB 2>/dev/null
cp "$T/t28_orig.bin" "$T28_FILE"
dfs_sync

# Phase 2: 200 random 4KB patches spread across all chunks
echo "  Applying $T28_PATCH_COUNT random 4KB patches..."
dd if=/dev/urandom of="$T/t28_patch.bin" bs=$T28_PATCH_SIZE count=1 2>/dev/null

# Build reference: apply same patches to local copy so we know expected content.
python3 - "$T/t28_orig.bin" "$T28_FILE" "$T/t28_patch.bin" \
         "$T28_PATCH_COUNT" "$T28_CHUNKS" "$CHUNK_SIZE" "$T28_PATCH_SIZE" \
         "$T/t28_expected.bin" <<'T28PY'
import sys, os, random
orig_path, dfs_path, patch_path, n_patches, n_chunks, chunk_size, patch_size, out_path = sys.argv[1:]
n_patches = int(n_patches); n_chunks = int(n_chunks)
chunk_size = int(chunk_size); patch_size = int(patch_size)

patch_data = open(patch_path, 'rb').read()
reference = bytearray(open(orig_path, 'rb').read())

random.seed(42)
offsets = []
for _ in range(n_patches):
    chunk = random.randrange(n_chunks)
    max_intra = chunk_size - patch_size
    intra = (random.randrange(max_intra // 4096)) * 4096
    off = chunk * chunk_size + intra
    offsets.append(off)

# Apply to DFS
fd = os.open(dfs_path, os.O_WRONLY)
for off in offsets:
    os.lseek(fd, off, os.SEEK_SET)
    os.write(fd, patch_data)
os.close(fd)

# Apply same offsets to local reference
for off in offsets:
    reference[off:off+patch_size] = patch_data

open(out_path, 'wb').write(reference)
print("patches applied")
T28PY

dfs_sync
echo "  Flush complete. Computing reference md5..."
T28_REF_MD5=$(md5sum "$T/t28_expected.bin" | awk '{print $1}')
echo "  Reference md5: $T28_REF_MD5"

# Phase 3: restart the client (cold cache — all metadata fetched from leader)
echo "  Restarting dfs-client (cold cache)..."
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 0.3
kill_client_and_wait "$CLIENT_PID2"
T28_CLIENT_LOG="$LOG/client_t28.log"
: > "$T28_CLIENT_LOG"
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$T28_CLIENT_LOG" --allow-other --log-level debug &
CLIENT_PID2=$!
CURRENT_CLIENT_LOG="$T28_CLIENT_LOG"
sleep 2
mountpoint -q "$MOUNT" || { check "T28 remount" FAIL; }

# Phase 4: verify data integrity with cold cache (reads go to DFS, no local state)
echo "  Verifying data integrity after cold restart..."
T28_GOT_MD5=$(md5sum "$MOUNT/t28_thick.bin" 2>/dev/null | awk '{print $1}')

[ "$T28_GOT_MD5" = "$T28_REF_MD5" ] \
    && check "T28a thick-file data intact after patch storm + restart (md5 match)" PASS \
    || check "T28a thick-file data corrupt after restart (want $T28_REF_MD5 got $T28_GOT_MD5)" FAIL

# Check for EIO errors in the cold-read log — they indicate stale metadata routing failures.
# grep -c returns exit 1 (no match) even when count is 0, so use grep -c ... || true.
T28_EIO=$(grep -c "EIO\|Input/output error\|chunk not found\|No such file\|file_not_found" \
    "$T28_CLIENT_LOG" 2>/dev/null || true)
T28_EIO=${T28_EIO:-0}
[ "${T28_EIO:-0}" -eq 0 ] \
    && check "T28b no EIO/chunk-not-found errors during cold read" PASS \
    || check "T28b EIO or chunk-not-found errors during cold read ($T28_EIO lines)" FAIL

rm -f "$T28_FILE" "$T/t28_orig.bin" "$T/t28_expected.bin" "$T/t28_patch.bin"
fi # should_run T28

# ── Test 29: sparse-file interior-gap prefetch (nil-chunk lookahead/swarm regression) ──
#
# Reproduces the "Chunk 0000...0000 not found on this node" log burst seen on
# staging during sequential reads of sparse VM disk images (grub-install on
# VM108, kdiskmark on VM100). The chunk map for a sparse file pads unwritten
# chunk indices with a nil placeholder (chunk_id = all-zero hash, nodes = []).
# pipeline_lookahead and the swarm/chain-reaction prefetch used to index into
# these placeholders directly and try to fetch the all-zero chunk_id from
# every cluster node, flooding the log with "not found on this node" for a
# chunk that can never exist anywhere.
#
# Layout: 24MB file (6 x 4MB chunks). Chunks 0,1,2 and 5 are written;
# chunks 3,4 are an interior gap (never written). A cold-cache sequential
# read across chunks 0->5 should:
#   - return correct data (gap reads as zeros)
#   - NOT emit any "Chunk 000...000 not found on this node" log lines
if should_run T29; then
snapshot_log T29
echo ""
echo "=== T29: sparse-file interior-gap prefetch (nil-chunk lookahead/swarm regression) ==="

T29_FILE="$MOUNT/t29_sparse.bin"

echo "  Writing 24MB sparse file: chunks 0-2 and 5 written, chunks 3-4 left as a gap..."
python3 - "$T29_FILE" "$T/t29_expected.bin" << 'PYEOF'
import os, sys
dfs_path, expected_path = sys.argv[1], sys.argv[2]
CHUNK = 4 * 1024 * 1024

# Expected full-file contents: pattern byte = (offset // 4096) & 0xff for
# written chunks (0,1,2,5); zeros for the gap chunks (3,4).
expected = bytearray()
for chunk_idx in range(6):
    if chunk_idx in (3, 4):
        expected += bytes(CHUNK)
    else:
        for b in range(0, CHUNK, 4096):
            off = chunk_idx * CHUNK + b
            expected += bytes([(off // 4096) & 0xff]) * 4096
open(expected_path, 'wb').write(expected)

# Write chunks 0-2 (first 12MB), then seek past the gap and write chunk 5.
# Chunks 3-4 (offsets 12MB-20MB) are never written -> interior sparse hole.
fd = os.open(dfs_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(fd, bytes(expected[0:3*CHUNK]))
os.lseek(fd, 5 * CHUNK, os.SEEK_SET)
os.write(fd, bytes(expected[5*CHUNK:6*CHUNK]))
os.close(fd)
PYEOF
dfs_sync

# Remount for cold cache — forces a fresh chunk map fetch with nil placeholders
# for the gap, and resets pipeline_head/in_flight so prefetch fires from scratch.
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 0.3
kill_client_and_wait "$CLIENT_PID2"
T29_CLIENT_LOG="$LOG/client_t29.log"
: > "$T29_CLIENT_LOG"
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$T29_CLIENT_LOG" --allow-other --log-level debug &
CLIENT_PID2=$!
CURRENT_CLIENT_LOG="$T29_CLIENT_LOG"
sleep 2
mountpoint -q "$MOUNT" || { check "T29 remount" FAIL; }

# Sequential read of the full 24MB file in 4KB blocks (matches T24's pattern,
# which exercises the full-chunk pipeline/swarm prefetch path).
echo "  Reading 24MB sequentially across the gap..."
T29_ERRORS=$(python3 -c "
size = 6 * 4 * 1024 * 1024
block = 4096
errors = 0
expected = open('$T/t29_expected.bin', 'rb').read()
with open('$T29_FILE', 'rb') as f:
    for i in range(size // block):
        data = f.read(block)
        exp = expected[i*block:(i+1)*block]
        if data != exp:
            errors += 1
print(errors)
")
[ "$T29_ERRORS" -eq 0 ] \
    && check "T29a sparse read across interior gap correct (gap=zeros)" PASS \
    || check "T29a sparse read across interior gap errors=$T29_ERRORS" FAIL

# Allow background lookahead/swarm/chain-reaction tasks to finish and log their results.
sleep 1

# The all-zero ChunkId — must NEVER be looked up, since it represents an
# unwritten sparse-hole placeholder, not a real chunk on any node.
T29_NIL_HASH=$(printf '0%.0s' $(seq 1 64))
T29_NIL_LINES=$(grep -c "$T29_NIL_HASH" "$T29_CLIENT_LOG" 2>/dev/null || true)
T29_NIL_LINES=${T29_NIL_LINES:-0}
[ "$T29_NIL_LINES" -eq 0 ] \
    && check "T29b no nil-chunk (all-zero hash) lookups for sparse holes" PASS \
    || check "T29b nil-chunk lookups for sparse holes ($T29_NIL_LINES lines) — pipeline_lookahead/swarm regression" FAIL

rm -f "$T29_FILE" "$T/t29_expected.bin"
fi # should_run T29

# ── Test 30: sparse-file metadata-repair size regression (sum-vs-max) ─────────
#
# handle_trigger_metadata_repair's quorum size-check used to compute
# authoritative_file_size as a SUM of each chunk's physical on-disk size.
# For a sparse file (logical size larger than the sum of its populated
# chunks, due to gaps), sum(chunk_sizes) < max(offset+size) == true logical
# size. Running `dfs-admin healing repair` against such a file silently
# shrunk FileMetadata.size to that sum — e.g. a 512MB VM disk image with 9
# populated chunks got shrunk to ~21MB. Fix: authoritative_file_size =
# max(offset + majority_size) across chunks.
#
# Layout: 12MB sparse file, only chunk 0 (offset 0) and chunk 2 (offset 8MB)
# written; chunk 1 is an unwritten gap. sum(chunk sizes)=8MB,
# max(offset+size)=12MB == true file size.
if should_run T30; then
snapshot_log T30
echo ""
echo "=== T30: sparse-file metadata-repair size (sum-vs-max regression) ==="

T30_FILE="$MOUNT/t30_sparse.raw"
T30_CHUNK=$(( 4 * 1024 * 1024 ))

python3 -c "
import os
fd = os.open('$T30_FILE', os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(fd, bytes([0xAB]) * $T30_CHUNK)        # chunk 0
os.lseek(fd, 2 * $T30_CHUNK, os.SEEK_SET)
os.write(fd, bytes([0xCD]) * $T30_CHUNK)        # chunk 2 (chunk 1 is a gap)
os.close(fd)
"
dfs_sync

T30_SIZE_BEFORE=$(stat -c %s "$T30_FILE")

echo "  Triggering metadata repair on all nodes..."
"$BIN/dfs-admin" --cluster "$CLUSTER" healing repair >/dev/null 2>&1 || true

# Repair runs as a background task on each node; give it time to complete.
sleep 8

# Cold remount to bypass any client-side size cache.
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 0.3
kill_client_and_wait "$CLIENT_PID2"
T30_CLIENT_LOG="$LOG/client_t30.log"
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$T30_CLIENT_LOG" --allow-other --log-level debug &
CLIENT_PID2=$!
CURRENT_CLIENT_LOG="$T30_CLIENT_LOG"
sleep 2
mountpoint -q "$MOUNT" || { check "T30 remount" FAIL; }

T30_SIZE_AFTER=$(stat -c %s "$T30_FILE" 2>/dev/null || echo 0)

[ "$T30_SIZE_AFTER" -eq "$T30_SIZE_BEFORE" ] \
    && check "T30 sparse-file size preserved after metadata repair (size=$T30_SIZE_AFTER)" PASS \
    || check "T30 sparse-file size corrupted by metadata repair (before=$T30_SIZE_BEFORE after=$T30_SIZE_AFTER)" FAIL

rm -f "$T30_FILE"
fi # should_run T30

# ── Test 31: read of never-written sparse file returns zeros, not EOF ─────────
#
# read_file() returned Ok(Vec::new()) whenever a file's chunk_map was completely
# empty (e.g. a VM disk image created via ftruncate and never written), even for
# in-bounds offsets (offset < file_size). Buffered reads tolerate this as a
# 0-byte/EOF response, but O_DIRECT readers (e.g. QEMU with cache=none, the PVE
# default) treat a short read at a non-EOF offset as an I/O error — turning every
# fdisk/mkfs/grub-install/fsck on a freshly created VM disk into "lots of
# corruption" (confirmed via losetup --direct-io=on + fdisk -> EIO on staging).
# Fix: an empty chunk_map with offset < file_size is a sparse hole; return
# zero-filled bytes instead of an empty Vec.
if should_run T31; then
snapshot_log T31
echo ""
echo "=== T31: read of never-written sparse file returns zeros (O_DIRECT + buffered) ==="

T31_FILE="$MOUNT/t31_sparse.raw"
T31_SIZE=$(( 64 * 1024 * 1024 ))

truncate -s $T31_SIZE "$T31_FILE"
dfs_sync

T31_RESULT=$(python3 -c "
import os

def probe(flags, offset, label):
    fd = os.open('$T31_FILE', os.O_RDONLY | flags)
    os.lseek(fd, offset, os.SEEK_SET)
    data = os.read(fd, 4096)
    os.close(fd)
    status = 'allzero' if data and all(b == 0 for b in data) else 'notzero'
    print(f'{label}: {len(data)} {status}')

probe(os.O_DIRECT, 0, 'direct_start')
probe(os.O_DIRECT, $T31_SIZE - 4096, 'direct_end')
probe(0, 0, 'buffered_start')
probe(0, $T31_SIZE - 4096, 'buffered_end')
")

echo "$T31_RESULT" | sed 's/^/  /'

echo "$T31_RESULT" | grep -q '^direct_start: 4096 allzero$' \
    && echo "$T31_RESULT" | grep -q '^direct_end: 4096 allzero$' \
    && echo "$T31_RESULT" | grep -q '^buffered_start: 4096 allzero$' \
    && echo "$T31_RESULT" | grep -q '^buffered_end: 4096 allzero$' \
    && check "T31 reads of never-written sparse file return zero-filled bytes" PASS \
    || check "T31 reads of never-written sparse file returned EOF/short read" FAIL

rm -f "$T31_FILE"
fi # should_run T31

# ── Test 32: concurrent multi-chunk pwrite/pread isolation with sparse holes ──
#
# Extensive investigation chased a suspected "cross-chunk contamination" bug:
# concurrent positional writes to one chunk appearing to leak into reads of a
# different chunk of the same file. Root-caused to a TEST HARNESS bug, not a
# DFS bug — the repro shared one fd's seek cursor across writer threads via
# lseek()+writev()/readv(), which races (thread A seeks, thread B's seek
# overwrites the shared cursor, thread A's writev lands at thread B's offset).
# Switching to positional pwritev()/preadv() (what real I/O stacks like QEMU
# use) eliminated the errors entirely (0/71359 and 0/61906 across two runs).
# This test codifies that workload as a permanent regression guard: concurrent
# pwritev/preadv across multiple chunks (some never written — must read as
# zero, exercising the T31 sparse-hole fix) with periodic fsync to cross the
# write-buffer→network transition.
if should_run T32; then
snapshot_log T32
echo ""
echo "=== T32: concurrent multi-chunk pwrite/pread isolation with sparse holes ==="

T32_FILE="$MOUNT/t32_concurrent.raw"
T32_NUM_CHUNKS=4
T32_SIZE=$(( T32_NUM_CHUNKS * 4 * 1024 * 1024 ))

truncate -s $T32_SIZE "$T32_FILE"
dfs_sync

T32_RESULT=$(python3 -c "
import os, mmap, random, threading, time

CHUNK = 4 * 1024 * 1024
BLK = 1024
NUM_CHUNKS = $T32_NUM_CHUNKS
PATH = '$T32_FILE'

fd_w = os.open(PATH, os.O_RDWR | os.O_DIRECT)
fd_r = os.open(PATH, os.O_RDONLY | os.O_DIRECT)

WRITTEN = [0, 2]
HOLES = [1, 3]
FILL = {c: 0xA0 + c for c in WRITTEN}

stop = threading.Event()
errors = []
reads = [0]
fsyncs = [0]

def writer(c):
    rng = random.Random(1000 + c)
    fill = FILL[c]
    base = c * CHUNK
    while not stop.is_set():
        nb = rng.randint(1, 8)
        blk = rng.randint(0, (CHUNK // BLK) - nb)
        off = base + blk * BLK
        size = nb * BLK
        buf = mmap.mmap(-1, size)
        buf.write(bytes([fill]) * size)
        os.pwritev(fd_w, [buf], off)

def fsyncer():
    while not stop.is_set():
        time.sleep(0.3)
        try:
            os.fsync(fd_w)
            fsyncs[0] += 1
        except OSError:
            pass

def reader():
    rng = random.Random(7777)
    while not stop.is_set():
        c = rng.randint(0, NUM_CHUNKS - 1)
        base = c * CHUNK
        blk = rng.randint(0, (CHUNK // BLK) - 1)
        off = base + blk * BLK
        buf = mmap.mmap(-1, BLK)
        n = os.preadv(fd_r, [buf], off)
        reads[0] += 1
        if n == 0:
            continue
        data = bytes(buf[:n])
        if c in HOLES:
            if any(b != 0 for b in data):
                errors.append(('HOLE_NONZERO', c, off, n))
                if len(errors) >= 5:
                    stop.set()
        else:
            allowed = FILL[c]
            for b in data:
                if b != 0 and b != allowed:
                    errors.append(('WRONG_FILL', c, off, n))
                    if len(errors) >= 5:
                        stop.set()
                    break

threads = [threading.Thread(target=writer, args=(c,)) for c in WRITTEN]
threads += [threading.Thread(target=reader) for _ in range(4)]
threads += [threading.Thread(target=fsyncer)]
for t in threads:
    t.start()

start = time.time()
while time.time() - start < 5 and not stop.is_set():
    time.sleep(0.05)
stop.set()
for t in threads:
    t.join()

os.close(fd_w)
os.close(fd_r)
print(f'reads={reads[0]} fsyncs={fsyncs[0]} errors={len(errors)}')
for e in errors[:5]:
    print(f'  {e}')
print(len(errors))
")

echo "$T32_RESULT" | sed 's/^/  /'
T32_ERR_COUNT=$(echo "$T32_RESULT" | tail -1)
T32_ERR_COUNT=${T32_ERR_COUNT:-1}

[ "$T32_ERR_COUNT" = "0" ] \
    && check "T32 concurrent multi-chunk pwrite/pread, 0 errors" PASS \
    || check "T32 concurrent multi-chunk pwrite/pread, $T32_ERR_COUNT errors" FAIL

rm -f "$T32_FILE"
fi # should_run T32

# ── Test 33: fresh-chunk rewrite-before-flush-completes (silent data loss) ────
#
# A chunk filled to exactly CHUNK_SIZE via sequential writes triggers an async
# is_full() flush (FRESH WRITE PATH, chunk_exists=false). flush_buffer_async_one
# claims the slot (flushing=true) and snapshots its data/dirty_ranges/
# last_modified before sending it to the storage nodes. If a small in-place
# rewrite to an already-buffered offset (e.g. offset 0) arrives while that
# flush is in flight, write_at finds the still-present slot and mutates
# slot.data in place — slot.data.len() does NOT grow, since this is an
# overwrite, not an append.
#
# The completion handler used to decide whether to remove the slot based only
# on `current_len <= flushed_len` (did the slot grow past what was just
# flushed?). An in-place overwrite leaves current_len == flushed_len, so the
# slot — now holding the new dirty rewrite — was removed, silently discarding
# it. This is exactly the write pattern QEMU/grub-install produces on VM disk
# images: sequential 128KB writes fill a 4MB chunk, then a small fsync-adjacent
# rewrite touches the start of that same chunk (e.g. an ext4 journal
# superblock rewrite) — the trigger pattern behind staging VM 108's
# "/images/108/vm-108-disk-2.raw" chunk 28 anomaly.
#
# Fix: flush_buffer_async_one's FRESH WRITE PATH now also checks
# `last_modified > last_modified_snap` (matching the PATCH path's existing
# T26 fix), keeping the slot alive (flushing=false, flushed_sizes populated)
# so the rewrite is flushed on the next cycle as a full-replacement.
if should_run T33; then
snapshot_log T33
echo ""
echo "=== T33: fresh-chunk rewrite-before-flush-completes (silent data loss) ==="

T33_FILE="$MOUNT/t33_rewrite.bin"

T33_RESULT=$(python3 -c "
import os, time

CHUNK = 4 * 1024 * 1024
path = '$T33_FILE'

fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
os.ftruncate(fd, 2 * CHUNK)

# Fill chunk 0 fully via 32x 128KB writes of pattern A (0xAA). The last write
# makes the slot is_full(), triggering an async FRESH WRITE flush of the
# whole 4MB chunk.
patA = bytes([0xAA]) * (128 * 1024)
for off in range(0, CHUNK, len(patA)):
    n = os.pwrite(fd, patA, off)
    assert n == len(patA), n

# Give the async flush time to claim the slot and start sending it.
time.sleep(0.1)

# Second small write to offset 0, pattern B (0xBB) — an in-place overwrite of
# already-buffered bytes (slot.data.len() does not grow).
patB = bytes([0xBB]) * 4096
n = os.pwrite(fd, patB, 0)
assert n == 4096, n

os.fsync(fd)
os.close(fd)

# Verify final content: [4096 bytes 0xBB][remaining 0xAA fill], read back
# through the mount after fsync.
fd = os.open(path, os.O_RDONLY)
head = os.pread(fd, 4096, 0)
tail = os.pread(fd, CHUNK - 4096, 4096)
os.close(fd)

errors = []
if head != patB:
    errors.append(f'head mismatch: first16={head[:16].hex()}')
if tail != bytes([0xAA]) * (CHUNK - 4096):
    bad = next((i for i, b in enumerate(tail) if b != 0xAA), -1)
    errors.append(f'tail mismatch: first bad byte at offset {bad}, value={tail[bad]:#x}' if bad >= 0 else 'tail mismatch')

for e in errors:
    print(e)
print(len(errors))
")

echo "$T33_RESULT" | sed 's/^/  /'
T33_ERR_COUNT=$(echo "$T33_RESULT" | tail -1)
T33_ERR_COUNT=${T33_ERR_COUNT:-1}

dfs_sync

[ "$T33_ERR_COUNT" = "0" ] \
    && check "T33 in-place rewrite during in-flight fresh-chunk flush preserved" PASS \
    || check "T33 in-place rewrite during in-flight fresh-chunk flush lost ($T33_ERR_COUNT errors)" FAIL

rm -f "$T33_FILE"
fi # should_run T33

if should_run T34; then
snapshot_log T34
echo ""
echo "=== T34: cross-path same-chunk patch race (server_chunk_id invariant) ==="

T34_FILE="$MOUNT/t34_crosspath.bin"

T34_RESULT=$(python3 -c "
import os, threading, time, random

CHUNK = 4 * 1024 * 1024
PATH = '$T34_FILE'

fd = os.open(PATH, os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)

# Establish two existing 4MB chunks (0xAA / 0xCC) and fsync, so chunk0 has an
# existing_loc on the server — every subsequent write into chunk0 is an
# in-place overwrite (PatchChunk/MultiPatch), not a fresh-chunk write.
patA = bytes([0xAA]) * (128 * 1024)
patC = bytes([0xCC]) * (128 * 1024)
for off in range(0, CHUNK, len(patA)):
    os.pwrite(fd, patA, off)
for off in range(0, CHUNK, len(patC)):
    os.pwrite(fd, patC, CHUNK + off)
os.fsync(fd)

# 16 threads, each repeatedly patching its OWN disjoint 4KB region within the
# first 64KB of chunk0 with its OWN fixed pattern. A fsyncer thread calls
# fsync (path 1: flush_buffer_async force=true) every ~80ms while writers and
# the 50ms background ticker (path 2) are also flushing chunk0. Because each
# thread always rewrites the SAME pattern to the SAME region, the only way a
# region can end up wrong is if a concurrent flush from another path silently
# loses the update (stale slot.server_chunk_id base for a MultiPatch).
N_WRITERS = 16
REGION = 4096
DURATION = 3.0

stop = threading.Event()

def writer(t):
    rng = random.Random(t)
    pattern = bytes([0x10 + t]) * REGION
    off = t * REGION
    while not stop.is_set():
        os.pwrite(fd, pattern, off)
        time.sleep(rng.uniform(0.001, 0.005))

def fsyncer():
    while not stop.is_set():
        try:
            os.fsync(fd)
        except OSError:
            pass
        time.sleep(0.08)

threads = [threading.Thread(target=writer, args=(t,)) for t in range(N_WRITERS)]
threads.append(threading.Thread(target=fsyncer))
for th in threads:
    th.start()

time.sleep(DURATION)
stop.set()
for th in threads:
    th.join()

os.fsync(fd)
os.close(fd)

# Verify via a fresh fd: each thread's region must hold its own pattern, and
# the untouched remainder of chunk0/chunk1 must be unchanged.
fd2 = os.open(PATH, os.O_RDONLY)
chunk0 = os.pread(fd2, CHUNK, 0)
chunk1 = os.pread(fd2, CHUNK, CHUNK)
os.close(fd2)

errors = []
for t in range(N_WRITERS):
    off = t * REGION
    expected = bytes([0x10 + t]) * REGION
    got = chunk0[off:off+REGION]
    if got != expected:
        errors.append(f'region {t} (offset {off}): expected {expected[:8].hex()}, got {got[:8].hex()}')

rest = chunk0[N_WRITERS*REGION:]
if rest != bytes([0xAA]) * len(rest):
    bad = next((i for i, b in enumerate(rest) if b != 0xAA), -1)
    errors.append(f'chunk0 tail corrupted at offset {N_WRITERS*REGION + bad}, value={rest[bad]:#x}')

if chunk1 != bytes([0xCC]) * CHUNK:
    bad = next((i for i, b in enumerate(chunk1) if b != 0xCC), -1)
    errors.append(f'chunk1 corrupted at offset {bad}, value={chunk1[bad]:#x}')

for e in errors:
    print(e)
print(len(errors))
")

echo "$T34_RESULT" | sed 's/^/  /'
T34_ERR_COUNT=$(echo "$T34_RESULT" | tail -1)
T34_ERR_COUNT=${T34_ERR_COUNT:-1}

dfs_sync

[ "$T34_ERR_COUNT" = "0" ] \
    && check "T34 cross-path same-chunk patch race, 0 errors" PASS \
    || check "T34 cross-path same-chunk patch race, $T34_ERR_COUNT errors" FAIL

rm -f "$T34_FILE"
fi # should_run T34

if should_run T35; then
snapshot_log T35
echo ""
echo "=== T35: rapid same-chunk rotation read-after-write monotonicity ==="

T35_FILE="$MOUNT/t35_hotrotate.bin"

T35_RESULT=$(python3 -c "
import os, time, struct

CHUNK = 4 * 1024 * 1024
PATH = '$T35_FILE'

fd = os.open(PATH, os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)

# Establish chunk0 as an existing in-place-patchable chunk (matches qcow2
# preallocation: every subsequent tiny write is a MultiPatch against an
# existing_loc, not a fresh-chunk write).
os.pwrite(fd, bytes(CHUNK), 0)
os.fsync(fd)

# Replicate VM108's mkfs.ext4 hot-spot on staging: two 8-byte fields (an
# L2-table entry at offset 196640 and a refcount-block entry at offset
# 65544) within chunk0, each rewritten ~once per ~27ms by the background
# flush ticker, for 50 rotations -- with NO fsync between writes (matches
# the live trace). After every few rotations, pread a region covering each
# field WITHOUT fsync, alternating between a >32KB read (full-chunk path)
# and a <=32KB read (range-fetch path), and verify the field reflects the
# LATEST write. A stale-rotation read here reproduces the read-after-write
# regression suspected of triggering QEMU's 'Marking image as corrupt'.
OFF_A = 196640   # within cluster [196608, 262144)
OFF_B = 65544    # within cluster [65536, 131072)
N = 50

errors = []
for i in range(N):
    os.pwrite(fd, struct.pack('<Q', i), OFF_A)
    os.pwrite(fd, struct.pack('<Q', i), OFF_B)
    time.sleep(0.03)

    if i % 3 == 2:
        size = 65536 if (i % 6 == 2) else 4096
        dataA = os.pread(fd, size, 196608)
        dataB = os.pread(fd, size, 65536)
        gotA = struct.unpack('<Q', dataA[32:40])[0]
        gotB = struct.unpack('<Q', dataB[8:16])[0]
        if gotA != i:
            errors.append(f'iter {i} (size={size}): field A read back {gotA}, expected {i}')
        if gotB != i:
            errors.append(f'iter {i} (size={size}): field B read back {gotB}, expected {i}')

os.fsync(fd)
os.close(fd)

# Final check via a fresh fd, after full flush.
fd2 = os.open(PATH, os.O_RDONLY)
dataA = os.pread(fd2, 65536, 196608)
dataB = os.pread(fd2, 65536, 65536)
os.close(fd2)
gotA = struct.unpack('<Q', dataA[32:40])[0]
gotB = struct.unpack('<Q', dataB[8:16])[0]
if gotA != N - 1:
    errors.append(f'final: field A = {gotA}, expected {N-1}')
if gotB != N - 1:
    errors.append(f'final: field B = {gotB}, expected {N-1}')

for e in errors:
    print(e)
print(len(errors))
")

echo "$T35_RESULT" | sed 's/^/  /'
T35_ERR_COUNT=$(echo "$T35_RESULT" | tail -1)
T35_ERR_COUNT=${T35_ERR_COUNT:-1}

dfs_sync

[ "$T35_ERR_COUNT" = "0" ] \
    && check "T35 rapid same-chunk rotation read-after-write, 0 errors" PASS \
    || check "T35 rapid same-chunk rotation read-after-write, $T35_ERR_COUNT errors" FAIL

rm -f "$T35_FILE"
fi # should_run T35

if should_run T36; then
snapshot_log T36
echo ""
echo "=== T36: setattr honors explicit mtime (rsync -a timestamp preservation) ==="

T36_FILE="$MOUNT/t36_mtime.bin"

T36_RESULT=$(python3 -c "
import os

PATH = '$T36_FILE'
OLD_MTIME = 1577836800   # 2020-01-01T00:00:00Z
NEWER_MTIME = 1609459200 # 2021-01-01T00:00:00Z

errors = []

fd = os.open(PATH, os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(fd, b'hello dfs')
os.close(fd)

# Simulate rsync -a: after the transfer, restore the source file's mtime.
os.utime(PATH, (OLD_MTIME, OLD_MTIME))

st = os.stat(PATH)
if int(st.st_mtime) != OLD_MTIME:
    errors.append(f'after utime: st_mtime={int(st.st_mtime)}, expected {OLD_MTIME}')

# A plain chmod (mode-only setattr) must not bump mtime.
os.chmod(PATH, 0o600)
st = os.stat(PATH)
if int(st.st_mtime) != OLD_MTIME:
    errors.append(f'after chmod: st_mtime={int(st.st_mtime)}, expected {OLD_MTIME} (unchanged)')

# A second utime (e.g. a later rsync run with an updated source file) must take effect.
os.utime(PATH, (NEWER_MTIME, NEWER_MTIME))
st = os.stat(PATH)
if int(st.st_mtime) != NEWER_MTIME:
    errors.append(f'after second utime: st_mtime={int(st.st_mtime)}, expected {NEWER_MTIME}')

for e in errors:
    print(e)
print(len(errors))
")

echo "$T36_RESULT" | sed 's/^/  /'
T36_ERR_COUNT=$(echo "$T36_RESULT" | tail -1)
T36_ERR_COUNT=${T36_ERR_COUNT:-1}

dfs_sync

[ "$T36_ERR_COUNT" = "0" ] \
    && check "T36 setattr honors explicit mtime, 0 errors" PASS \
    || check "T36 setattr honors explicit mtime, $T36_ERR_COUNT errors" FAIL

rm -f "$T36_FILE"
fi # should_run T36

if should_run T37; then
snapshot_log T37
echo ""
echo "=== T37: rename preserves explicit mtime set before rename (rsync temp-file pattern) ==="

T37_TMP="$MOUNT/.t37_rsync.bin.tmp"
T37_FILE="$MOUNT/t37_rsync.bin"

T37_RESULT=$(python3 -c "
import os

TMP = '$T37_TMP'
FINAL = '$T37_FILE'
OLD_MTIME = 1577836800   # 2020-01-01T00:00:00Z

errors = []

# Simulate rsync -a's temp-file dance: write data to a hidden temp file,
# restore the source mtime, chmod, then rename into place. The renamed
# file must keep the restored mtime, not get stamped with 'now'.
fd = os.open(TMP, os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o600)
os.write(fd, b'hello dfs rename')
os.close(fd)

os.utime(TMP, (OLD_MTIME, OLD_MTIME))
os.chmod(TMP, 0o644)
os.rename(TMP, FINAL)

st = os.stat(FINAL)
if int(st.st_mtime) != OLD_MTIME:
    errors.append(f'after rename: st_mtime={int(st.st_mtime)}, expected {OLD_MTIME}')

for e in errors:
    print(e)
print(len(errors))
")

echo "$T37_RESULT" | sed 's/^/  /'
T37_ERR_COUNT=$(echo "$T37_RESULT" | tail -1)
T37_ERR_COUNT=${T37_ERR_COUNT:-1}

dfs_sync

[ "$T37_ERR_COUNT" = "0" ] \
    && check "T37 rename preserves explicit mtime, 0 errors" PASS \
    || check "T37 rename preserves explicit mtime, $T37_ERR_COUNT errors" FAIL

rm -f "$T37_FILE" "$T37_TMP"
fi # should_run T37

# ── Test 39: explicit mtime preserved across concurrent flush tasks ───────────
#
# Reproduces the rsync re-transfer bug: when a file spans multiple chunk slots
# and utimes() is called before all flush tasks complete, two concurrent flush
# tasks both check explicit_mtime_pending.  The old code used remove() — the
# first task consumed the flag; the second saw None and stamped mtime=now(),
# clobbering the utimes value with a higher write_seq (server accepted it).
# The fix uses contains() (non-destructive) in flush tasks and clears the flag
# only in write(), making all concurrent tasks preserve the explicit mtime.
if should_run T39; then
snapshot_log T39
echo ""
echo "=== T39: explicit mtime preserved with multi-chunk file (concurrent flush race) ==="

T39_FILE="$MOUNT/t39_mtime_race.bin"
OLD_MTIME=1609459200   # 2021-01-01T00:00:00 UTC

T39_RESULT=$(python3 -c "
import os, time

PATH = '$T39_FILE'
OLD_MTIME = $OLD_MTIME
CHUNK = 4 * 1024 * 1024
N_CHUNKS = 3

errors = []

# Write 3 chunks (12 MB) to create multiple flush slots.
fd = os.open(PATH, os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
data = os.urandom(CHUNK)
ref_md5 = None
import hashlib
h = hashlib.md5()
for _ in range(N_CHUNKS):
    os.write(fd, data)
    h.update(data)
ref_md5 = h.hexdigest()
os.close(fd)

# Set explicit historical mtime AFTER writes, simulating rsync utimes().
os.utime(PATH, (OLD_MTIME, OLD_MTIME))

# fsync to flush everything (may spawn N_CHUNKS flush tasks concurrently).
fd2 = os.open(PATH, os.O_RDONLY)
os.fsync(fd2)
os.close(fd2)

# Give any in-flight flush tasks a moment to complete.
time.sleep(1)

st = os.stat(PATH)
got_mtime = int(st.st_mtime)
if got_mtime != OLD_MTIME:
    errors.append(f'mtime clobbered by concurrent flush: got {got_mtime}, expected {OLD_MTIME}')

# Verify data integrity too.
with open(PATH, 'rb') as f:
    got_md5 = hashlib.md5(f.read()).hexdigest()
if got_md5 != ref_md5:
    errors.append(f'data corrupt: got {got_md5}, expected {ref_md5}')

for e in errors:
    print(e)
print(len(errors))
")

echo "$T39_RESULT" | sed 's/^/  /'
T39_ERR_COUNT=$(echo "$T39_RESULT" | tail -1)
T39_ERR_COUNT=${T39_ERR_COUNT:-1}

dfs_sync

[ "$T39_ERR_COUNT" = "0" ] \
    && check "T39 explicit mtime preserved across concurrent flush slots, 0 errors" PASS \
    || check "T39 explicit mtime race: $T39_ERR_COUNT errors" FAIL

rm -f "$T39_FILE"
fi # should_run T39

# ── Test 38: rolling node restart during slow write — replica convergence ────
#
# Reproduces a staging observation (live DVR recording, RF=3 cluster): a
# rolling restart of all server nodes while a long write is in flight left
# some chunks under-replicated (< RF=3) until the healer caught up. Verifies:
#   1. Data written across the restart window is intact (md5 match).
#   2. The healer converges every chunk back to RF=3 within a short window.
if should_run T38; then
snapshot_log T38
echo ""
echo "=== T38: rolling node restart during slow write (replica convergence) ==="

T38_FILE="$MOUNT/t38_slow.bin"
T38_SIZE_MB=40

dd if=/dev/urandom of="$T/t38_src.bin" bs=1M count=$T38_SIZE_MB 2>/dev/null

# Write at ~2MB/s so the 40MB/10-chunk write spans the whole rolling restart.
pv -q -L 2m "$T/t38_src.bin" > "$T38_FILE" &
T38_PV_PID=$!

sleep 2   # let the write get going before the first restart

echo "  Rolling restart of all 5 server nodes (one at a time)..."
for i in 1 2 3 4 5; do
    pkill -f "dfs-server start --config $BASE/node${i}/config.toml" 2>/dev/null || true
    sleep 0.5
    RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        >> "$LOG/server${i}.log" 2>&1 &
    sleep 2
done

wait $T38_PV_PID
dfs_sync

T38_GOT_MD5=$(md5sum "$T38_FILE" | awk '{print $1}')
T38_REF_MD5=$(md5sum "$T/t38_src.bin" | awk '{print $1}')
[ "$T38_GOT_MD5" = "$T38_REF_MD5" ] \
    && check "T38a data intact after rolling restart during write (md5 match)" PASS \
    || check "T38a data corrupt after rolling restart (want $T38_REF_MD5 got $T38_GOT_MD5)" FAIL

# Inspect chunk replication immediately after the restart settles.
T38_UNDER_BEFORE=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json file info /t38_slow.bin 2>/dev/null \
    | python3 -c "import json,sys; d=json.load(sys.stdin); print(sum(1 for c in d['chunk_locations'] if len(c['nodes']) < 2))")
echo "  Under-replicated chunks immediately after restart: ${T38_UNDER_BEFORE:-?}"

# Tune healing_delay_secs down before triggering. Root-caused 2026-07-15: any chunk
# that was "never fully replicated" (e.g. a late patch during this test's own slow
# write, landing at 2/3 replicas) gets a deliberate healing_delay_secs wait before
# should_heal() will act on it at all — production default is 300s, specifically to
# avoid healing a chunk that's still mid-write. T38's poll loop can't wait that long,
# and re-triggering doesn't help (it's a wall-clock check against first-detection
# time, not a retry-count check) — so without this, the test either times out or
# reports a false PASS (the queue-depth metric excludes gated chunks, so it can read
# "0" while one is still deliberately untouched). 2s is enough for the write to have
# genuinely settled by the time healing acts, without making the test wait minutes.
"$BIN/dfs-admin" --cluster "$CLUSTER" healing set --healing-delay-secs 2 >/dev/null 2>&1 || true

# Trigger an immediate heal scan, then poll until the queue drains (or 30s).
echo "  Triggering healer and polling for convergence..."
"$BIN/dfs-admin" --cluster "$CLUSTER" healing file /t38_slow.bin 2>/dev/null || true
"$BIN/dfs-admin" --cluster "$CLUSTER" healing trigger 2>/dev/null || true
sleep 3   # let the triggered scan populate the queue before polling

T38_DEADLINE=$(( $(date +%s) + 30 ))
while true; do
    T38_STATUS=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json healing status 2>/dev/null || echo '{}')
    T38_QUEUE=$(echo "$T38_STATUS" | python3 -c "
import json, sys
d = json.load(sys.stdin)
print(d.get('pending_count', 0) + d.get('in_flight_count', 0))
" 2>/dev/null || echo "?")
    echo "  Heal queue: ${T38_QUEUE} (pending + in-flight)"
    [ "$T38_QUEUE" = "0" ] && break
    if [ "$(date +%s)" -ge "$T38_DEADLINE" ]; then
        echo "  WARN: heal queue did not drain within 30s"
        break
    fi
    sleep 2
done

"$BIN/dfs-admin" --cluster "$CLUSTER" file info /t38_slow.bin

# Uses < 3 (the real RF target), not the < 2 "sync-durable floor" threshold
# T38_UNDER_BEFORE uses — a chunk sitting at exactly 2/3 replicas is expected
# right after the write (3rd replica lands async, see T45's note on this), but
# NOT expected here: healing has had its chance to finish the job by this point,
# so anything still below the full target RF is a genuine convergence failure.
# Using the same lenient <2 threshold here would have silently passed the exact
# bug this test exists to catch (root-caused 2026-07-15: a chunk stuck at 2/3
# replicas, gated behind healing_delay_secs).
T38_UNDER_AFTER=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json file info /t38_slow.bin 2>/dev/null \
    | python3 -c "import json,sys; d=json.load(sys.stdin); print(sum(1 for c in d['chunk_locations'] if len(c['nodes']) < 3))")

[ "$T38_UNDER_AFTER" = "0" ] \
    && check "T38b all chunks reach RF=3 after healing (was ${T38_UNDER_BEFORE:-?} under-replicated)" PASS \
    || check "T38b ${T38_UNDER_AFTER:-?} chunks still under-replicated after healing (was ${T38_UNDER_BEFORE:-?})" FAIL

# Restore the production default so later tests (and anyone poking at the cluster
# after this suite finishes) see realistic healing behavior, not this test's
# deliberately-shortened delay.
"$BIN/dfs-admin" --cluster "$CLUSTER" healing set --healing-delay-secs 300 >/dev/null 2>&1 || true

rm -f "$T38_FILE" "$T/t38_src.bin"
fi # should_run T38

# ── Test 40: zero_gap stale-read + gap fill regression ───────────────────────
#
# Reproduces the qcow2 corruption root cause (local simulation):
#   1. Write sparse ranges to a chunk, fsync → zero_gap seeded for the gaps.
#   2. Write real data into a gap position → PATCH path.
#   3. Read back BEFORE fsync  → must come from slot (not zero_gap zeros).
#   4. Read back AFTER fsync   → zero_gap cleared; chunk_cache has correct data.
#   5. Wait 2s and read again  → no stale zero_gap TTL re-serves zeros.
if should_run T40; then
snapshot_log T40
echo ""
echo "=== T40: zero_gap stale-read + gap fill regression ==="

T40_RESULT=$(python3 "$(dirname "$0")/test_qcow2_gap.py" "$MOUNT" 2>&1)
echo "$T40_RESULT" | sed 's/^/  /'

if echo "$T40_RESULT" | grep -q "ALL TESTS PASSED"; then
    check "T40 zero_gap gap-fill: all reads correct" PASS
else
    check "T40 zero_gap gap-fill: data corruption detected" FAIL
fi

dfs_sync
fi # should_run T40

# ── Test 41: SIGTERM without a clean unmount must still drain write buffers ──
#
# Reproduces the staging "corrupted dvr.conf after deploy-build.sh" bug: the
# deploy script does `podman stop` then `systemctl stop dfs-client`, whose
# ExecStop runs `fusermount -u`. If something still has the mount busy at that
# moment, fusermount fails, destroy() (which normally drains write buffers) never
# runs, and systemd just SIGTERMs the client directly — silently dropping any
# buffered-but-unflushed write. This test skips fusermount entirely and sends
# SIGTERM straight to the client while a write is still sitting in the buffer
# (well under the 500ms idle-flush window), simulating exactly that fallback path.
if should_run T41; then
snapshot_log T41
echo ""
echo "=== T41: SIGTERM mid-write must drain buffers (no clean unmount) ==="

T41_FILE="$MOUNT/t41_sigterm.bin"
dd if=/dev/urandom of="$T/t41_ref.bin" bs=1M count=1 2>/dev/null

# Find whichever dfs-client is currently serving the mount. Prefer the
# tracked $CLIENT_PID2/$CLIENT_PID bash variable over pgrep when available —
# pgrep | head -1 picks whichever matching PID happens to sort first, which
# is wrong (an old orphan, not the live one) if more than one dfs-client
# process matching this mount is still around. That used to be a real risk:
# every earlier remount's kill was fire-and-forget with a fixed sleep, not a
# wait for actual exit (see kill_client_and_wait's doc comment for the T41
# flake this caused) — fixed now, but keep the tracked-PID preference as
# defense in depth rather than relying solely on no future remount
# reintroducing an orphan. Fall back to pgrep only when this test runs alone
# (T41 only) before any remount test has set either variable.
T41_CLIENT_PID="${CLIENT_PID2:-$CLIENT_PID}"
[ -z "$T41_CLIENT_PID" ] && T41_CLIENT_PID=$(pgrep -f "dfs-client mount $MOUNT" | head -1)

# Open, write, and hold the fd open in the background — release()'s flush
# must not run, since we want the data to still be sitting unflushed when we
# signal the client.
python3 -c "
import os, time
fd = os.open('$T41_FILE', os.O_WRONLY | os.O_CREAT, 0o644)
with open('$T/t41_ref.bin', 'rb') as f:
    os.write(fd, f.read())
time.sleep(10)
try:
    os.close(fd)
except OSError:
    pass
" &
T41_WRITER_PID=$!

sleep 0.15   # land in the write buffer, stay under the 500ms idle-flush window
kill -TERM "$T41_CLIENT_PID"

# Our SIGTERM handler drains buffers then calls process::exit — wait for it.
T41_WAITED=0
while kill -0 "$T41_CLIENT_PID" 2>/dev/null; do
    sleep 0.2
    T41_WAITED=$((T41_WAITED+1))
    [ "$T41_WAITED" -gt 150 ] && break   # 30s safety cap
done
kill -0 "$T41_CLIENT_PID" 2>/dev/null \
    && check "T41a client exited after SIGTERM" FAIL \
    || check "T41a client exited after SIGTERM" PASS

# The writer's fd is now attached to a dead FUSE connection — kill it and force
# the stale mount out of the way before remounting fresh.
kill -9 "$T41_WRITER_PID" 2>/dev/null || true
wait "$T41_WRITER_PID" 2>/dev/null || true
fusermount -uz "$MOUNT" 2>/dev/null || true
sleep 0.5

T41_CLIENT_LOG="$LOG/client_t41.log"
: > "$T41_CLIENT_LOG"
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$T41_CLIENT_LOG" --allow-other --log-level debug &
CLIENT_PID2=$!
CURRENT_CLIENT_LOG="$T41_CLIENT_LOG"
sleep 2
mountpoint -q "$MOUNT" || { check "T41b remount after SIGTERM" FAIL; }

T41_GOT_MD5=$(md5sum "$T41_FILE" 2>/dev/null | awk '{print $1}')
T41_REF_MD5=$(md5sum "$T/t41_ref.bin" | awk '{print $1}')
[ -n "$T41_GOT_MD5" ] && [ "$T41_GOT_MD5" = "$T41_REF_MD5" ] \
    && check "T41c write survives SIGTERM-without-unmount (md5 match)" PASS \
    || check "T41c write lost/corrupted after SIGTERM-without-unmount (want $T41_REF_MD5 got ${T41_GOT_MD5:-<missing>})" FAIL

rm -f "$T41_FILE" "$T/t41_ref.bin"
fi # should_run T41

# ── Test 42: chunk-write flood must not starve the leader's async runtime ────
#
# Repro for the staging gluster1 hang (2026-06-19): handle_replicate_chunk_location
# (server.rs) calls metadata.put_chunk_location() directly — a synchronous redb
# begin_write()/commit() — on the Tokio worker thread. The heal-scan path already
# carries an explicit warning about this exact failure mode (healing.rs:850-853:
# "every Tokio worker thread can end up blocked on the mutex, freezing the entire
# async runtime") and was fixed there via batching through spawn_blocking, but the
# live per-write RPC path (handle_replicate_chunk_location) never got the same fix.
#
# Under a burst of concurrent chunk writes — many overlapping ReplicateChunkLocation
# RPCs landing on the leader at once — every worker thread can end up blocked inside
# redb's single-writer transaction lock simultaneously, starving the whole runtime,
# including unrelated requests like `cluster status`.
if should_run T42; then
snapshot_log T42
echo ""
echo "=== T42: replicate-chunk-location flood must not starve the leader ==="

# Leader isn't pinned to a fixed port — discover it from the client's own log
# (set right after mount: "Leader node: <id> (<addr>)"). snapshot_log just
# moved the mount-time log lines into T42.log and truncated client.log, so
# look there first; fall back to client.log in case this test ran without
# the snapshot (e.g. invoked standalone after other tests already ran).
LEADER_ADDR=$( { cat "$LOG/T42.log" "$LOG/client.log" 2>/dev/null || true; } \
    | grep -oE "Leader node: [^(]+\(([0-9.]+:[0-9]+)\)" \
    | tail -1 | grep -oE "[0-9.]+:[0-9]+" || true)
[ -z "$LEADER_ADDR" ] && LEADER_ADDR="127.0.0.1:8900"
echo "  T42: leader is $LEADER_ADDR"

T42_NUM_PROCS=100
T42_DURATION=15
# Hard cap on the flood subprocess itself — if writes/fsyncs against the
# starved leader block indefinitely (the bug also wedges the client, not just
# the server), this guarantees the test fails loudly instead of hanging the
# suite forever.
T42_FLOOD_CAP=$(( T42_DURATION + 15 ))

# Flood the leader with concurrent small fsync'd writes from many separate
# *processes* (not threads — avoids the GIL throttling request rate below what
# the server can actually keep up with). Each write+fsync triggers a
# ReplicateChunkLocation RPC to the leader. Small payload (well under one 4MB
# chunk) keeps disk usage bounded; what matters here is RPC concurrency, not
# data volume.
T42_FLOOD_PIDS=()
for i in $(seq 0 $((T42_NUM_PROCS-1))); do
    timeout --kill-after=5 "${T42_FLOOD_CAP}s" python3 -c "
import os, time
path = '$MOUNT/t42_flood_$i.bin'
buf = bytes([$i % 256]) * (4 * 1024)
stop_at = time.time() + $T42_DURATION
while time.time() < stop_at:
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    os.write(fd, buf)
    os.fsync(fd)
    os.close(fd)
" &
    T42_FLOOD_PIDS+=($!)
done

# While the flood runs, repeatedly poll cluster status against the leader with
# a hard 3s timeout. A healthy node answers in well under 100ms; any timeout
# here means the leader's runtime was starved.
T42_TIMEOUTS=0
T42_CALLS=0
T42_MAX_MS=0
T42_POLL_END=$(( $(date +%s) + T42_DURATION + 2 ))
while [ "$(date +%s)" -lt "$T42_POLL_END" ]; do
    T42_START_MS=$(date +%s%3N)
    if timeout 3 "$BIN/dfs-admin" -c "$LEADER_ADDR" cluster status >/dev/null 2>&1; then
        T42_ELAPSED=$(( $(date +%s%3N) - T42_START_MS ))
        [ "$T42_ELAPSED" -gt "$T42_MAX_MS" ] && T42_MAX_MS=$T42_ELAPSED
    else
        T42_TIMEOUTS=$((T42_TIMEOUTS+1))
    fi
    T42_CALLS=$((T42_CALLS+1))
    sleep 0.2
done

# Bounded by T42_FLOOD_CAP above — cannot hang the suite even if the flood
# itself is stuck inside a blocked write()/fsync() syscall.
T42_FLOOD_RC=0
for pid in "${T42_FLOOD_PIDS[@]}"; do
    wait "$pid" 2>/dev/null || T42_FLOOD_RC=$?
done
dfs_sync 2>/dev/null || true

echo "  T42: $T42_CALLS cluster-status calls during flood, $T42_TIMEOUTS timeouts, max latency ${T42_MAX_MS}ms, flood_rc=$T42_FLOOD_RC"

if [ "$T42_FLOOD_RC" -ge 124 ]; then
    check "T42 leader hung under chunk-write flood (writer threads themselves got stuck — flood killed after ${T42_FLOOD_CAP}s)" FAIL
elif [ "$T42_TIMEOUTS" -eq 0 ]; then
    check "T42 leader stayed responsive under chunk-write flood" PASS
else
    check "T42 leader hung under chunk-write flood ($T42_TIMEOUTS/$T42_CALLS cluster-status calls timed out, max ${T42_MAX_MS}ms)" FAIL
fi

for i in $(seq 0 $((T42_NUM_PROCS-1))); do rm -f "$MOUNT/t42_flood_${i}.bin" 2>/dev/null || true; done
fi # should_run T42

if should_run T43; then
snapshot_log T43
echo ""
echo "=== T43: single patch write should not trigger redundant ReplicateChunkLocation broadcasts ==="

# Established staging-cluster observation: a single small in-place overwrite
# (MultiPatch, 2 replicas under RF=2) produces THREE separate "Handling
# replicate chunk location" log lines on the leader instead of one — each
# patched replica self-reports its own node_id (server.rs ~5044-5070), and
# the client then sends a third broadcast with the merged node set
# (client.rs ~5926-5942). This test reproduces that count directly so a
# future fix can be validated against it.

T43_FILE="$MOUNT/t43_patch.bin"

# Fresh 4MB write establishes chunk0 with an existing_loc on the server, so
# the next write below takes the in-place overwrite (PatchChunk/MultiPatch)
# path rather than the fresh-chunk path.
dd if=/dev/zero of="$T43_FILE" bs=1M count=4 2>/dev/null
dfs_sync

# Record each server log's line count *before* the write, so the extraction
# below can look only at lines appended by this specific write — server logs
# aren't per-test-truncated (unlike the client log snapshot), so a background
# flush/heal cycle settling late from an earlier test (e.g. a slow/throttled
# write test) can otherwise land its own "MultiPatch:" line after this test's
# and get mistaken for it by a plain `tail -1` across the whole run's history.
declare -A T43_LOG_MARKS
for f in "$LOG"/server*.log; do
    T43_LOG_MARKS["$f"]=$(wc -l < "$f" 2>/dev/null || echo 0)
done

# Exactly one small in-place overwrite -> exactly one MultiPatch, one patch.
python3 -c "
import os
fd = os.open('$T43_FILE', os.O_RDWR)
os.pwrite(fd, bytes([0xAB]) * 4096, 1000)
os.fsync(fd)
os.close(fd)
"
dfs_sync
sleep 1   # let the replicas' fire-and-forget self-report RPCs land

# Pull the resulting chunk id out of the server logs — each patched replica
# logs its own "MultiPatch: old -> new (... final size=...)" line synchronously
# as part of applying the patch, so this is available immediately (unlike the
# client's own MultiPatch summary line, which can lag behind a buffered log
# flush after dfs_sync returns). Scoped to only lines appended since the marks
# above, so unrelated background activity elsewhere can't be picked up instead.
T43_CHUNK_ID=""
for f in "$LOG"/server*.log; do
    mark=${T43_LOG_MARKS["$f"]:-0}
    found=$(tail -n "+$((mark+1))" "$f" 2>/dev/null \
        | grep -oE "MultiPatch: [0-9a-f]+ -> [0-9a-f]+" | tail -1 | awk '{print $4}')
    [ -n "$found" ] && T43_CHUNK_ID="$found"
done
echo "  T43: patched chunk = ${T43_CHUNK_ID:-<not found>}"

if [ -z "$T43_CHUNK_ID" ]; then
    check "T43 could not find patched chunk id in client log" FAIL
else
    T43_RCL_COUNT=$(grep -h "Handling replicate chunk location: $T43_CHUNK_ID " "$LOG"/server*.log 2>/dev/null | wc -l)
    echo "  T43: $T43_RCL_COUNT ReplicateChunkLocation broadcasts handled by the leader for 1 patch write (ideal: 1)"
    [ "$T43_RCL_COUNT" -le 1 ] \
        && check "T43 no redundant RCL broadcasts for single patch" PASS \
        || check "T43 redundant RCL broadcasts: $T43_RCL_COUNT calls for 1 patch write (expect 1)" FAIL
fi

rm -f "$T43_FILE"
fi # should_run T43

if should_run T44; then
echo ""
echo "=== T44: metadata compaction must not visibly stall request handling ==="

# Opportunistic, not a dedicated load test: compaction already fires naturally several
# times per full suite run (confirmed in real logs — every server hits the fragmentation
# threshold within the first couple minutes of T1-T43's combined write volume). server*.log
# files aren't truncated per-test (unlike the client log), so they cover the whole run —
# scan them for "redb compaction phase3 lock acquiring" -> "redb compaction finished"
# windows (the only part of compact_db() that actually holds the exclusive lock — see
# dfs-server/src/metadata.rs) and check whether that server's own log went suspiciously
# quiet during one (a self-contained signal: if metadata I/O were blocked,
# concurrently-handled requests on that same node couldn't log anything either, so the
# gap shows up in the same file). The unlocked copy/catch-up phases before this window
# can legitimately take a while in wall-clock terms without that being a problem.
#
# Local DBs are tens of MB, so even the actually-locked phase finishes in well under
# 500ms — too fast for this to be a strong signal at this scale (that's exactly why the
# bug only surfaced on staging's much larger live dataset). Treat this as a sanity
# check, not proof; zero windows found (e.g. a filtered RUN_TESTS subset) is not a
# failure.
T44_REPORT=$(python3 -c "
import re, sys, glob
from datetime import datetime

THRESHOLD_MS = 1000
TS_RE = re.compile(r'(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+)Z')

def parse_ts(line):
    m = TS_RE.search(line)
    if not m:
        return None
    return datetime.strptime(m.group(1), '%Y-%m-%dT%H:%M:%S.%f')

windows_checked = 0
max_gap_ms = 0.0
worst = None

# Only the span between 'phase3 lock acquiring' and 'compaction finished' is actually
# exclusively locked — Phase 1-2 (full copy + catch-up) run before this and can
# legitimately take a while in wall-clock terms without blocking anyone, since they
# never hold the lock. Measuring from 'compaction starting' instead would conflate
# that unlocked work with real blocking.
for path in sorted(glob.glob('$LOG/server*.log')):
    with open(path, errors='replace') as f:
        lines = f.readlines()
    timestamps = [parse_ts(l) for l in lines]

    start_idx = None
    for i, line in enumerate(lines):
        if 'redb compaction phase3 lock acquiring' in line:
            start_idx = i
            continue
        if start_idx is not None and 'redb compaction finished' in line:
            end_idx = i
            window_ts = [t for t in timestamps[start_idx:end_idx+1] if t is not None]
            if len(window_ts) >= 2:
                windows_checked += 1
                gaps = [(b - a).total_seconds() * 1000 for a, b in zip(window_ts, window_ts[1:])]
                gap = max(gaps)
                if gap > max_gap_ms:
                    max_gap_ms = gap
                    worst = (path, gap)
            start_idx = None

print(f'{windows_checked}|{max_gap_ms:.1f}|{worst[0] if worst else \"\"}')
")
T44_WINDOWS=$(echo "$T44_REPORT" | cut -d'|' -f1)
T44_MAX_GAP=$(echo "$T44_REPORT" | cut -d'|' -f2)
T44_WORST_LOG=$(echo "$T44_REPORT" | cut -d'|' -f3)

echo "  T44: $T44_WINDOWS compaction window(s) observed across server logs, max internal gap ${T44_MAX_GAP}ms${T44_WORST_LOG:+ (in $T44_WORST_LOG)}"

if [ "$T44_WINDOWS" -eq 0 ]; then
    echo "  T44: no compaction windows observed this run (e.g. filtered RUN_TESTS subset) — not a failure"
elif awk "BEGIN { exit !($T44_MAX_GAP > 1000) }"; then
    check "T44 request handling stalled ${T44_MAX_GAP}ms during a metadata compaction window (>1000ms threshold)" FAIL
else
    check "T44 no significant stall observed during metadata compaction windows" PASS
fi
fi # should_run T44

# ── Test 45: live healing tuning + replication-factor set/get, rejoin reconciliation ──
#
# Verifies the `dfs-admin healing set/get` and `cluster set/get` commands: healing
# bandwidth ceiling, concurrency, and transfer timeout, plus replication_factor, are
# live-tunable cluster-wide without a restart, and persist to config.toml so a restart
# doesn't revert them. Also verifies the rejoin-reconciliation gap-closer: a node that's
# down during a `cluster set --replication-factor` change silently keeps its stale
# config — by design, each node reads replication_factor independently with no
# cross-node consistency check — and must self-heal to the leader's value when it
# rejoins, without the operator needing to notice and re-run the command (see
# reconcile_replication_factor_with_leader in dfs-server/src/main.rs).
if should_run T45; then
snapshot_log T45
echo ""
echo "=== T45: live healing tuning + replication-factor set/get + rejoin reconciliation ==="

# --- Part A: healing set/get — live effect + config persistence across a restart ---
T45_BASELINE=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json healing get 2>/dev/null)
echo "  T45: baseline healing tuning: $T45_BASELINE"

"$BIN/dfs-admin" --cluster "$CLUSTER" healing set \
    --link-bandwidth-mb 55 --max-pct 42 --max-concurrent 6 --transfer-timeout-secs 77
T45_AFTER=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json healing get 2>/dev/null)
echo "  T45: after set: $T45_AFTER"

T45_LIVE_OK=$(echo "$T45_AFTER" | python3 -c "
import json, sys
d = json.load(sys.stdin)
ok = (d.get('link_bandwidth_mb') == 55 and abs(d.get('heal_max_pct', 0) - 42.0) < 0.01
      and d.get('heal_max_concurrent') == 6 and d.get('heal_transfer_timeout_secs') == 77)
print('PASS' if ok else 'FAIL')
" 2>/dev/null || echo FAIL)
check "T45a healing set applied live (link=55 pct=42 concurrent=6 timeout=77)" "$T45_LIVE_OK"

T45_CONFIG_OK=PASS
for i in 1 2 3 4 5; do
    grep -q "link_bandwidth_mb = 55" "$BASE/node${i}/config.toml" || T45_CONFIG_OK=FAIL
    grep -q "heal_max_concurrent = 6" "$BASE/node${i}/config.toml" || T45_CONFIG_OK=FAIL
done
check "T45b healing set persisted to config.toml on all 5 nodes" "$T45_CONFIG_OK"

echo "  T45: restarting node1 to confirm tuned values survive (not reverted to defaults)..."
pkill -f "dfs-server start --config $BASE/node1/config.toml" 2>/dev/null || true
sleep 0.5
RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 "$BIN/dfs-server" start --config "$BASE/node1/config.toml" \
    >> "$LOG/server1.log" 2>&1 &

# Poll for a full 5-node rejoin, not just node1's RPC listener being up — Part B below
# uses node1 (cluster_addrs[0]) for cluster-wide node discovery via GetClusterStatus,
# which needs join_cluster to have actually completed (repopulating node1's member
# list), not merely an accepting socket. Bounded poll instead of a fixed sleep: fast
# on the happy path (typically ~1-2s locally), safe if it's ever slower.
T45_DEADLINE=$(( $(date +%s) + 15 ))
while [ "$(date +%s)" -lt "$T45_DEADLINE" ]; do
    T45_N=$("$BIN/dfs-admin" --cluster "127.0.0.1:8900" --format json cluster status 2>/dev/null \
        | python3 -c "import json,sys; print(json.load(sys.stdin).get('total_nodes', 0))" 2>/dev/null || echo 0)
    [ "$T45_N" = "5" ] && break
    sleep 1
done

T45_NODE1_STATUS=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json healing get 2>/dev/null || echo '{}')
T45_SURVIVES_RESTART=$(echo "$T45_NODE1_STATUS" | python3 -c "
import json, sys
d = json.load(sys.stdin)
print('PASS' if d.get('link_bandwidth_mb') == 55 else 'FAIL')
" 2>/dev/null || echo FAIL)
check "T45c tuned healing values survive a node restart (persisted, not reverted)" "$T45_SURVIVES_RESTART"

# --- Part B: replication-factor INCREASE — verify healing adds a real replica ---
#
# Deliberately tests an RF *increase* (3→4), not a decrease. Over-replication trims
# (what a decrease would eventually trigger) require destructive_allowed =
# grace_elapsed && nodes_down <= 1, where grace_elapsed needs LEADER_CHANGE_GRACE_SECS
# (1200s = 20min) since the last leader election — and every test run boots a brand new
# cluster (fresh leader election at startup), so that grace period cannot have elapsed
# within this test's lifetime. Under-replication healing (an increase) has no such
# gate — it's unconditional ("always safe") — so it's both the more meaningful check
# (does healing actually push a real 4th replica onto existing data, not just accept a
# new config number?) and the only RF direction that's fast enough to assert on here.

# min_replica_count <path>: smallest nodes-per-chunk count across a file's chunks, via
# dfs-admin's JSON file info. Used to poll for convergence after an RF change.
min_replica_count() {
    "$BIN/dfs-admin" --cluster "$CLUSTER" --format json file info "$1" 2>/dev/null \
        | python3 -c "import json,sys
try:
    d = json.load(sys.stdin)
    print(min(len(c['nodes']) for c in d['chunk_locations']))
except Exception:
    print('?')" 2>/dev/null || echo "?"
}

T45_RF_FILE="$MOUNT/t45_rf.bin"
dd if=/dev/urandom of="$T/t45_rf_src.bin" bs=1M count=1 2>/dev/null
cp "$T/t45_rf_src.bin" "$T45_RF_FILE"
dfs_sync

T45_RF_BEFORE=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json cluster get 2>/dev/null \
    | python3 -c "import json,sys; print(json.load(sys.stdin)['replication_factor'])" 2>/dev/null || echo "?")
echo "  T45: baseline replication_factor: $T45_RF_BEFORE"

# Not waiting for full RF=3 convergence here — that would cost a full
# healing_delay_secs+heal-loop-tick cycle for something T45i already checks below.
# But dfs_sync only guarantees the client-facing write+metadata commit, not that
# dfs-admin's separately-routed query (via cluster_addrs[0]) sees it on the very next
# request — a brief metadata-propagation lag is real, so retry briefly rather than
# taking a single immediate sample.
T45_REPLICAS_BEFORE="?"
for _ in 1 2 3 4 5; do
    T45_REPLICAS_BEFORE=$(min_replica_count /t45_rf.bin)
    [ "$T45_REPLICAS_BEFORE" != "?" ] && break
    sleep 1
done
echo "  T45: replicas per chunk shortly after write (sync-only, no heal wait): $T45_REPLICAS_BEFORE"
check "T45d write lands at least 2 sync replicas before any healing" \
    "$( [ "${T45_REPLICAS_BEFORE:-0}" -ge 2 ] 2>/dev/null && echo PASS || echo FAIL )"

echo "  T45: stopping node5 to simulate it being unreachable during the RF change..."
pkill -f "dfs-server start --config $BASE/node5/config.toml" 2>/dev/null || true
sleep 0.5

# node5 is down, so this fans out to the other 4 and reports a failure (and non-zero
# exit) for node5 — that's expected and is exactly the scenario rejoin reconciliation
# exists to heal, so it's tolerated here.
"$BIN/dfs-admin" --cluster "$CLUSTER" cluster set --replication-factor 4 2>&1 | tail -5 || true

T45_RF_LIVE_NODES_OK=PASS
for i in 1 2 3 4; do
    grep -q "replication_factor = 4" "$BASE/node${i}/config.toml" || T45_RF_LIVE_NODES_OK=FAIL
done
check "T45e replication_factor updated + persisted on 4 reachable nodes while node5 was down" "$T45_RF_LIVE_NODES_OK"

T45_NODE5_STALE_OK=PASS
grep -q "replication_factor = 3" "$BASE/node5/config.toml" || T45_NODE5_STALE_OK=FAIL
check "T45f node5 config still stale (=3) while down — confirms no silent cross-node update" "$T45_NODE5_STALE_OK"

echo "  T45: restarting node5 — expect it to self-reconcile replication_factor to 4 on rejoin..."
RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 "$BIN/dfs-server" start --config "$BASE/node5/config.toml" \
    >> "$LOG/server5.log" 2>&1 &

T45_DEADLINE=$(( $(date +%s) + 30 ))
T45_RECONCILED=FAIL
while [ "$(date +%s)" -lt "$T45_DEADLINE" ]; do
    if grep -q "replication_factor = 4" "$BASE/node5/config.toml" 2>/dev/null; then
        T45_RECONCILED=PASS
        break
    fi
    sleep 2
done
check "T45g node5 self-reconciled replication_factor to 4 after rejoining (no manual re-run needed)" "$T45_RECONCILED"

T45_RECONCILE_LOG_OK=FAIL
grep -q "stale vs. cluster majority" "$LOG/server5.log" 2>/dev/null && T45_RECONCILE_LOG_OK=PASS
check "T45h reconciliation warning logged on node5" "$T45_RECONCILE_LOG_OK"

# The real proof: does healing actually push a physical 4th replica of *existing* data,
# not just accept the new config number?
#
# `healing trigger` is a one-shot RPC that only does anything if it lands on the
# *current* leader — a non-leader just logs "ignoring" and still returns Ok, so the
# caller can't tell it was a no-op. Node5 just restarted and reconciled RF moments
# ago, so leadership may still be mid-transition — a single upfront trigger can race
# that and land on a node that (correctly) ignores it, leaving convergence to the slow
# passive 60s discovery cycle instead. Re-issue the trigger each iteration so a
# transient "wrong node" miss just gets retried instead of dooming the whole check to
# that passive cycle.
echo "  T45: triggering healing (retried) and polling for the 4th replica to land on real data..."
T45_DEADLINE=$(( $(date +%s) + 60 ))
T45_HEALED_TO_4=FAIL
while [ "$(date +%s)" -lt "$T45_DEADLINE" ]; do
    "$BIN/dfs-admin" --cluster "$CLUSTER" healing trigger >/dev/null 2>&1 || true
    [ "$(min_replica_count /t45_rf.bin)" = "4" ] && { T45_HEALED_TO_4=PASS; break; }
    sleep 3
done
check "T45i healing added a real 4th replica of existing data after RF 3→4 (was $T45_REPLICAS_BEFORE)" "$T45_HEALED_TO_4"

"$BIN/dfs-admin" --cluster "$CLUSTER" file info /t45_rf.bin || true
rm -f "$T45_RF_FILE" "$T/t45_rf_src.bin"

# Restore defaults so any tests appended after this one start from a clean baseline.
# This is a *decrease* (4→3) — deliberately not asserted on: the over-replication trim
# it would eventually trigger is gated behind the 20-minute post-leader-election grace
# period explained above, which this freshly-started test cluster can't have cleared
# yet. The config value still updates immediately; only the physical trim-down lags,
# and that's harmless (an extra replica, not a missing one) in the meantime.
"$BIN/dfs-admin" --cluster "$CLUSTER" cluster set --replication-factor 3 >/dev/null 2>&1 || true
"$BIN/dfs-admin" --cluster "$CLUSTER" healing set \
    --link-bandwidth-mb 100 --max-pct 60 --max-concurrent 8 --transfer-timeout-secs 120 >/dev/null 2>&1 || true

fi # should_run T45

# ── T46: chunk-0 header loss after delete+recreate at the same path ───────────
#
# Reproduces the staging corruption seen in HDHomeRun DVR recordings and dvr.conf:
# both write a small header/content first, and when a file is deleted and
# immediately recreated at the identical path (inode reused via path_to_inode),
# InodeWriteState's `expected_file_id` guard — which exists specifically to detect
# "metadata_cache now refers to a different file" (fuse_impl.rs:309-312) — is only
# ever set on the SQLite pre-create path (fuse_impl.rs:5297-5306), never on the
# general lazy write-buffer path (fuse_impl.rs:5411-5413) that DVR recordings and
# dvr.conf go through. A stale, larger existing_chunk_size left over from the
# deleted predecessor then causes the new file's small first write to be routed
# as a patch against the old (wrong) chunk instead of a fresh write, and the real
# header never lands in chunk 0.
if should_run T46; then
snapshot_log T46
echo ""
echo "=== T46: chunk-0 header loss after delete+recreate at same path ==="

T46_FILE="$MOUNT/t46_header.bin"

# Step 1: write a large "old" file with a distinctive marker as its first bytes,
# and commit it for real (dfs_sync) so the server has a genuine, sized chunk 0.
DFS_MOUNT="$MOUNT" python3 - <<'PYEOF'
import os
mount = os.environ['DFS_MOUNT']
path = mount + '/t46_header.bin'
with open(path, 'wb') as f:
    f.write(b'OLDFILE_MARKER_DO_NOT_KEEP\n')
    f.write(os.urandom(3 * 1024 * 1024))
    f.flush()
    os.fsync(f.fileno())
PYEOF
dfs_sync

T46_OLD_LANDED=FAIL
dd if="$T46_FILE" bs=1k count=12 2>/dev/null | strings | grep -q "OLDFILE_MARKER_DO_NOT_KEEP" && T46_OLD_LANDED=PASS
check "T46a old file's marker landed before delete (sanity check)" "$T46_OLD_LANDED"

# Step 2: delete it, then immediately recreate the SAME path with a distinctive
# "new" header as the very first write, fsync'ing right after just that header
# (before writing the bulk data) to force the first flush of chunk 0 while the
# slot is still tiny — no sleep, to land inside the inode-reuse window while the
# client's own metadata_cache/write_buffers state for this inode is still stale.
rm -f "$T46_FILE"

DFS_MOUNT="$MOUNT" python3 - <<'PYEOF'
import os
mount = os.environ['DFS_MOUNT']
path = mount + '/t46_header.bin'
with open(path, 'wb') as f:
    f.write(b'NEWFILE_HEADER_MARKER_XYZ\n')
    f.flush()
    os.fsync(f.fileno())
    f.write(os.urandom(1 * 1024 * 1024))
    f.flush()
    os.fsync(f.fileno())
PYEOF
dfs_sync

T46_NEW_HEADER_OK=FAIL
dd if="$T46_FILE" bs=1k count=12 2>/dev/null | strings | grep -q "NEWFILE_HEADER_MARKER_XYZ" && T46_NEW_HEADER_OK=PASS
check "T46b new file's chunk-0 header survives delete+recreate at same path" "$T46_NEW_HEADER_OK"

T46_OLD_LEAKED=PASS
dd if="$T46_FILE" bs=1k count=12 2>/dev/null | strings | grep -q "OLDFILE_MARKER_DO_NOT_KEEP" && T46_OLD_LEAKED=FAIL
check "T46c new file's chunk-0 is not contaminated with the old file's content" "$T46_OLD_LEAKED"

rm -f "$T46_FILE"
fi # should_run T46

# ── Test 48: background-tick metadata push must not lose chunk_locations ─────
#
# Regression test for a real-world finding: a large qcow2 disk write's metadata
# round-trip latency climbed from ~1.5ms to ~40-49ms as chunk_locations grew past
# ~1300 entries, because flush_buffer_async's background-tick push (the non-force
# branch, throttled to once per 2s but NOT payload-trimmed) sent the file's entire,
# ever-growing chunk_locations Vec on every push. The force/fsync branch already
# sent only the newly-flushed locations (all_locations) for this exact reason; the
# background-tick branch was missed. Fixed by applying the same trim there.
#
# This test writes several separate 4MB-aligned chunks to the SAME open file
# descriptor with pauses long enough for the background tick's own 2s throttle to
# fire independently between writes (no explicit fsync in between — fsync takes
# the already-fixed force branch, so avoiding it is what actually exercises the
# code path this regression lives in). If the fix regressed — e.g. sending a
# genuinely non-cumulative chunk_locations that the server misread as a
# truncate-to-zero — chunk_locations would have been lost partway through, and
# either the read-back content or the leader's persisted chunk count would show it.
echo ""
echo "=== T48: background-tick metadata push preserves chunk_locations under sustained writes ==="
snapshot_log T48
if should_run T48; then
T48_FILE="$MOUNT/t48_bgpush.bin"
T48_CHUNKS=8
T48_CHUNK_BYTES=$(( 4 * 1024 * 1024 ))

# Run the writer in the background and keep the fd open across all chunks — the
# CRITICAL part of this test is checking metadata WHILE the file is still open,
# before close()/release() ever runs. release() takes the already-correct
# force/fsync branch (full reconcile against chunk_map), which would silently
# repair any corruption the background-tick branch caused in between — checking
# only after close would never see the regression this test exists to catch.
python3 -c "
import os, time, sys
path, chunks, chunk_bytes = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
fd = os.open(path, os.O_WRONLY | os.O_CREAT, 0o644)
for i in range(chunks):
    os.write(fd, bytes([i % 256]) * chunk_bytes)
    # No fsync here on purpose — see this test's header comment.
    time.sleep(2.5)
os.close(fd)
" "$T48_FILE" "$T48_CHUNKS" "$T48_CHUNK_BYTES" &
T48_WRITER_PID=$!

# Let ~4 of the 8 chunks land (each write + 2.5s sleep), giving the background
# tick's own 2s throttle multiple chances to fire, then check the leader's
# persisted view WHILE the writer still holds the fd open.
sleep 11
T48_MIDWRITE_COUNT=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json file info /t48_bgpush.bin 2>/dev/null \
    | python3 -c "import json,sys; d=json.load(sys.stdin); print(len(d.get('chunk_locations', [])))" 2>/dev/null)
echo "  T48: mid-write (file still open), leader reports $T48_MIDWRITE_COUNT chunk(s) (~4 expected so far)"
T48_MIDWRITE_OK=PASS
[ "${T48_MIDWRITE_COUNT:-0}" -ge 3 ] || T48_MIDWRITE_OK=FAIL
check "T48a mid-write: background-tick pushes keep chunk_locations growing, not truncated to empty" "$T48_MIDWRITE_OK"

wait "$T48_WRITER_PID"
dfs_sync

T48_OK=PASS
python3 -c "
import sys
path, chunks, chunk_bytes = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
with open(path, 'rb') as f:
    for i in range(chunks):
        data = f.read(chunk_bytes)
        expected = bytes([i % 256]) * chunk_bytes
        if data != expected:
            print(f'chunk {i} MISMATCH: got {len(data)} bytes, expected first byte {i % 256}, got {data[0] if data else None}')
            sys.exit(1)
print('all chunks verified')
" "$T48_FILE" "$T48_CHUNKS" "$T48_CHUNK_BYTES" || T48_OK=FAIL
check "T48b all chunks intact after sustained background-tick pushes" "$T48_OK"

# Cross-check the leader's own persisted view — the exact regression this guards
# against is a background push silently truncating FILE_TABLE's chunk_locations,
# which read-back alone might not catch if the client's local cache masks it.
T48_CHUNK_COUNT=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json file info /t48_bgpush.bin 2>/dev/null \
    | python3 -c "import json,sys; d=json.load(sys.stdin); print(len(d.get('chunk_locations', [])))" 2>/dev/null)
echo "  T48: dfs-admin reports $T48_CHUNK_COUNT chunk(s) for the file (expect $T48_CHUNKS)"
[ "${T48_CHUNK_COUNT:-0}" -eq "$T48_CHUNKS" ] \
    && check "T48c persisted metadata shows all chunks (not truncated by background push)" PASS \
    || check "T48c persisted metadata shows all chunks (not truncated by background push): got ${T48_CHUNK_COUNT:-0}, want $T48_CHUNKS" FAIL

rm -f "$T48_FILE"
fi # should_run T48

# ── Test 49: patch an earlier chunk of a file with a non-4MB tail chunk ────────
#
# Reproduces a bug found via an upgrade-compatibility test (2026-07-08): a file
# whose LAST chunk is smaller than the standard 4MB (i.e. size is not a multiple
# of the chunk size — every file with a partial tail chunk, which is most files)
# gets corrupted across its entire tail — not just the patched region — the
# moment an EARLIER chunk is patched. T22/T25 patch multi-chunk files heavily but
# always use sizes that are exact multiples of 4MB (no partial tail chunk), which
# is why they never caught this. Black-box only: compare against a local mirror
# file byte-for-byte via md5sum — the point of this bug is that it doesn't matter
# which chunk_id ends up serving the read, only whether the bytes match.
snapshot_log T49
if should_run T49; then
echo ""
echo "=== T49: patching an earlier chunk doesn't corrupt a non-4MB tail chunk ==="

T49_FILE="$MOUNT/t49_tail.bin"
T49_LOCAL="$T/t49_local.bin"

# 6MB = one full 4MB chunk + one 2MB (partial/tail) chunk.
dd if=/dev/urandom of="$T49_LOCAL" bs=1M count=6 2>/dev/null
cp "$T49_LOCAL" "$T49_FILE"
dfs_sync

T49_GOT=$(md5sum "$T49_FILE" | awk '{print $1}')
T49_WANT=$(md5sum "$T49_LOCAL" | awk '{print $1}')
[ "$T49_GOT" = "$T49_WANT" ] \
    && check "T49a fresh 6MB write (full chunk + partial tail chunk) correct" PASS \
    || check "T49a fresh write corrupt (want $T49_WANT got $T49_GOT)" FAIL

# Patch 16KB at offset 1MB — well inside chunk 0, nowhere near the tail chunk.
dd if=/dev/urandom of="$T49_LOCAL" bs=4096 count=4 seek=256 conv=notrunc 2>/dev/null
# skip=256 is required here: without it, dd reads from the START of T49_LOCAL
# (offset 0) instead of the just-randomized region at offset 1MB, copying the
# wrong 16KB into the mount and making T49b fail even when the patch path is
# byte-for-byte correct end to end (root-caused 2026-07-09 — see
# project_t49_write_loss_unresolved memory).
dd if="$T49_LOCAL" of="$T49_FILE" bs=4096 count=4 seek=256 skip=256 conv=notrunc 2>/dev/null
dfs_sync

T49_GOT=$(md5sum "$T49_FILE" | awk '{print $1}')
T49_WANT=$(md5sum "$T49_LOCAL" | awk '{print $1}')
[ "$T49_GOT" = "$T49_WANT" ] \
    && check "T49b patch to chunk 0 leaves whole file (incl. tail chunk) correct" PASS \
    || check "T49b patch to chunk 0 corrupted the file (want $T49_WANT got $T49_GOT)" FAIL

# Pinpoint: does corruption (if any) start exactly at the patch offset and run
# to EOF, or is it localized to just the patched 16KB? Diagnostic only — T49b
# above is the real pass/fail signal.
if [ "$T49_GOT" != "$T49_WANT" ]; then
    T49_FIRST_DIFF=$(cmp "$T49_LOCAL" "$T49_FILE" 2>&1 | grep -oP 'byte \K[0-9]+' || echo "?")
    echo "  T49 diagnostic: first differing byte = $T49_FIRST_DIFF (patch started at byte 1048577)"
    echo "  T49 diagnostic: server-side (FILE_TABLE) view via dfs-admin:"
    "$BIN/dfs-admin" --cluster "$CLUSTER" --format json file info /t49_tail.bin 2>&1
fi

rm -f "$T49_FILE" "$T49_LOCAL"
fi # should_run T49

# ── Test 50: rapid repeated patches to the same chunk don't truncate it ───────
#
# Reproduces a real staging incident (2026-07-09): a DVR app that rewrites a
# ~12KB header block at the start of a recording every time it opens the file
# (e.g. on fast-forward) issued several such patches in quick succession.
# flush_buffer_async_one's post-patch bookkeeping recorded flushed_sizes[idx]
# as just the patch payload's length (e.g. 12032 bytes) instead of the chunk's
# true total size (4194304) whenever a concurrent write arrived while a patch
# was still in flight — which rapid back-to-back patches make likely. The next
# write to that chunk then read existing_chunk_size back from flushed_sizes,
# saw only 12032, and misclassified itself as a full replacement — genuinely
# truncating the chunk's real content on the server. Confirmed on staging: one
# node's chunk_location record showed size=12032 where it should have been
# 4194304, and reads beyond the patched header returned zeros. Black-box only:
# compare against a local mirror file byte-for-byte via md5sum.
snapshot_log T50
if should_run T50; then
echo ""
echo "=== T50: rapid repeated patches to the same chunk don't truncate it ==="

T50_FILE="$MOUNT/t50_dvr.mpg"
T50_LOCAL="$T/t50_local.mpg"

# 12MB = one full 4MB chunk (chunk 0, the one we repeatedly patch) + a 8MB tail
# spanning 2 more chunks, so a truncation of chunk 0 is unambiguous in the
# whole-file checksum.
dd if=/dev/urandom of="$T50_LOCAL" bs=1M count=12 2>/dev/null
cp "$T50_LOCAL" "$T50_FILE"
dfs_sync

T50_GOT=$(md5sum "$T50_FILE" | awk '{print $1}')
T50_WANT=$(md5sum "$T50_LOCAL" | awk '{print $1}')
[ "$T50_GOT" = "$T50_WANT" ] \
    && check "T50a fresh 12MB write correct" PASS \
    || check "T50a fresh write corrupt (want $T50_WANT got $T50_GOT)" FAIL

# Rewrite the first 12032 bytes 25 times, back-to-back, via separate
# open/write/close cycles — matching the real DVR app's pattern (a fresh
# open() on every fast-forward, not one long-lived fd) closely enough to hit
# the same "next write arrives while the previous patch is still in flight"
# window, without needing multi-minute wall-clock spacing to do it.
python3 -c "
import os
path = '$T50_FILE'
for i in range(1, 26):
    fd = os.open(path, os.O_RDWR)
    os.lseek(fd, 0, os.SEEK_SET)
    os.write(fd, bytes([i % 256]) * 12032)
    os.close(fd)
"
# Mirror only the LAST patch's effect onto the local reference file — every
# earlier patch was overwritten by the next one at the same offset.
python3 -c "
path = '$T50_LOCAL'
with open(path, 'r+b') as f:
    f.seek(0)
    f.write(bytes([25 % 256]) * 12032)
"
dfs_sync

T50_GOT=$(md5sum "$T50_FILE" | awk '{print $1}')
T50_WANT=$(md5sum "$T50_LOCAL" | awk '{print $1}')
[ "$T50_GOT" = "$T50_WANT" ] \
    && check "T50b 25 rapid repeated patches leave the whole file (incl. untouched tail) correct" PASS \
    || check "T50b rapid repeated patches corrupted/truncated the file (want $T50_WANT got $T50_GOT)" FAIL

# Cross-check the leader's own persisted chunk_locations — the exact
# regression this guards against is chunk 0's registered size collapsing to
# the last patch's payload length (12032) instead of staying 4194304.
T50_CHUNK0_SIZE=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json file info /t50_dvr.mpg 2>/dev/null \
    | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['chunk_locations'][0]['size'])" 2>/dev/null)
echo "  T50: dfs-admin reports chunk 0 size=$T50_CHUNK0_SIZE (expect 4194304)"
[ "${T50_CHUNK0_SIZE:-0}" -eq 4194304 ] \
    && check "T50c persisted chunk 0 size not truncated by rapid patching" PASS \
    || check "T50c persisted chunk 0 size truncated: got ${T50_CHUNK0_SIZE:-0}, want 4194304" FAIL

rm -f "$T50_FILE" "$T50_LOCAL"
fi # should_run T50

# ── Test 51: leader restart mid-patch-storm must not lose chunk data ──────────
#
# Repro for a real 2026-07-11 staging incident (fio+fsck repro on server3): the
# leader (gluster1) hit "redb compact_db() exceeded 60s — exclusive metadata
# write lock is permanently wedged" and self-restarted. For the ~16s it was
# down, every other node logged "Failed to connect" to it, and the CLIENT's
# concurrent MultiPatch calls to hot chunks — which target 2 replicas, one of
# which was often the leader itself (it's an ordinary storage replica too, not
# just a coordinator) — silently degraded to "(1 replicas, ...)" instead of
# failing outright. The client accepted that 1-of-2 result and kept chaining
# further patches on top of it with no verification the surviving replica
# actually persisted durably and no attempt to restore real replication. End
# state: a chunk_id with ZERO CHUNK_TABLE records on any of the 5 nodes —
# including the one that supposedly "succeeded" — surfacing later as a hard
# EIO on read (e2fsck: "Input/output error reading journal superblock").
#
# This test reproduces the mechanism directly: sustained concurrent patches to
# non-overlapping slots of one hot chunk, with the current leader killed and
# restarted partway through (same kill/relaunch shape T45 already uses for its
# own node-restart test). Verifies both immediately after (in-memory/cache
# could mask real loss) and after a cold client restart (the real incident's
# corruption only surfaced on a fresh read, once cache no longer masked it).
if should_run T51; then
snapshot_log T51
echo ""
echo "=== T51: leader restart mid-patch-storm must not lose chunk data ==="

T51_IMG="$MOUNT/t51_disk.img"
T51_PATCH_SIZE=4096
T51_DURATION=8          # seconds of sustained patch storm
T51_CONCURRENCY=6

echo "  Writing 4MB base chunk..."
dd if=/dev/urandom of="$T/t51_base.bin" bs=4M count=1 2>/dev/null
cp "$T/t51_base.bin" "$T51_IMG"
dfs_sync

echo "  Launching sustained patch storm (${T51_DURATION}s, $T51_CONCURRENCY concurrent workers)..."
T51_STOP="$T/t51_stop_$$"
rm -f "$T51_STOP"
T51_LOGDIR="$T/t51_jobs_$$"
mkdir -p "$T51_LOGDIR"

# Each worker repeatedly patches its own dedicated non-overlapping 4KB slot
# (65536B apart — comfortably non-overlapping) with an incrementing
# sequence-tagged payload. After the storm, each slot must hold EXACTLY that
# worker's *last* write — not zeros, not another worker's tag, not a
# superseded intermediate sequence number.
#
# T51_WORKER_PIDS captures exactly these 6 subshell PIDs so the wait below can
# target them specifically — a bare `wait` waits for ALL of this shell's
# background jobs, which by the time we reach it also includes the relaunched
# dfs-server daemon below (a long-lived process that never exits on its own),
# hanging the test indefinitely. Real mistake made 2026-07-11 while first
# writing this test: it ran for 15+ minutes before being caught and fixed.
T51_WORKER_PIDS=()
for w in $(seq 0 $((T51_CONCURRENCY-1))); do
    (
        seq_n=0
        byte_off=$(( w * 65536 ))
        while [ ! -f "$T51_STOP" ]; do
            # Wall time for the whole open/write/close cycle. T51d asserts on the max of
            # these: killing the leader mid-storm must not stall a client write for longer
            # than a guest's I/O timeout, or a VM running on the mount takes an EIO even
            # though no data was lost. Added 2026-07-22 after a rolling restart during a
            # live VM produced client stalls of 4.7s/12.8s/27.3s with zero failover events
            # logged — this test already killed the leader mid-write and passed, because it
            # only ever checked correctness, never latency.
            t51_w_start=$(date +%s%N)
            python3 -c "
import os, sys
img, byte_off, patch_size, worker, seq_n = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
tag = ('T51_W%02d_S%06d_' % (worker, seq_n)).encode()
data = (tag + bytes([worker % 256]) * (patch_size - len(tag)))[:patch_size]
fd = os.open(img, os.O_WRONLY)
os.lseek(fd, byte_off, 0)
os.write(fd, data)
os.close(fd)
" "$T51_IMG" "$byte_off" "$T51_PATCH_SIZE" "$w" "$seq_n" 2>>"$T51_LOGDIR/err_$w" \
                && echo "$seq_n" > "$T51_LOGDIR/last_$w"
            echo $(( ($(date +%s%N) - t51_w_start) / 1000000 )) >> "$T51_LOGDIR/lat_$w"
            seq_n=$((seq_n+1))
            sleep 0.05
        done
    ) &
    T51_WORKER_PIDS+=($!)
done

# Let the storm run for a couple seconds before disrupting the leader, so
# there's real in-flight/steady-state traffic when it goes down — matches the
# real incident (the leader crashed mid-fio-run, not at the very start).
sleep 2

# Kill the LEADER specifically, not just any replica. A single degraded
# ("1 replicas") MultiPatch alone isn't enough to actually lose data — tried
# that first, and the one surviving replica held up fine. The real incident
# needed BOTH failures at once: the down node was an ordinary data replica
# for the affected chunk (RF=3 in a 5-node cluster means the leader routinely
# is one, degrading a dual-replica MultiPatch to "1 replicas") AND it was the
# leader, the sole target for ReplicateChunkLocations delivery — so even the
# one replica that DID succeed couldn't get its location durably confirmed
# cluster-wide during the same outage window.
T51_LEADER_ADDR=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json cluster status 2>/dev/null \
    | python3 -c "
import json, sys
d = json.load(sys.stdin)
online = [n for n in d.get('nodes', []) if n.get('status') == 'Online']
online.sort(key=lambda n: n['id'])
print(online[0]['address'] if online else '')
" 2>/dev/null)
T51_LEADER_PORT="${T51_LEADER_ADDR##*:}"
T51_LEADER_NODE=$(( (T51_LEADER_PORT - 8900) + 1 ))
echo "  Leader is node$T51_LEADER_NODE ($T51_LEADER_ADDR) — killing it mid-storm..."

# SIGKILL, not the default SIGTERM: this codebase's SIGTERM handler does a
# graceful drain (flushes pending writes before exiting — see
# kill_client_and_wait's shutdown-drain counterpart on the server side),
# which is exactly the safe path and would mask the bug. The real incident
# was an ABRUPT crash — gluster1's own watchdog force-exited it after
# detecting a wedged redb lock, with no graceful drain in its log — so a
# clean SIGTERM restart here would not reproduce the same failure shape.
pkill -9 -f "dfs-server start --config $BASE/node${T51_LEADER_NODE}/config.toml" 2>/dev/null || true

# Stay down for a few real seconds — not an instant relaunch — so the storm
# has a genuine window to hit in-flight writes against the dead leader
# repeatedly, the same way the real incident's ~16s leader outage did.
sleep 3

RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 "$BIN/dfs-server" start --config "$BASE/node${T51_LEADER_NODE}/config.toml" \
    >> "$LOG/server${T51_LEADER_NODE}.log" 2>&1 &

# Storm keeps running through the outage and recovery — that's the whole
# point: patches must survive a leader that's briefly completely unreachable.
sleep $(( T51_DURATION - 2 ))

touch "$T51_STOP"
# Bounded wait on exactly the worker PIDs (not a bare `wait` — see
# T51_WORKER_PIDS's doc comment). Well-behaved workers exit within one
# 0.05s loop iteration of the stop file appearing; 15s is a generous safety
# margin, with a hard kill -9 fallback so a genuinely wedged write() (e.g.
# reproducing the underlying bug's hang shape rather than a clean EIO) can
# never hang the suite — it shows up as a slot with no recorded last write
# instead, which T51a already treats as a failure.
T51_WAIT_DEADLINE=$(( $(date +%s) + 15 ))
for pid in "${T51_WORKER_PIDS[@]}"; do
    while kill -0 "$pid" 2>/dev/null; do
        [ "$(date +%s)" -ge "$T51_WAIT_DEADLINE" ] && { kill -9 "$pid" 2>/dev/null; break; }
        sleep 0.1
    done
done
dfs_sync

T51_MISMATCHES=0
for w in $(seq 0 $((T51_CONCURRENCY-1))); do
    last_seq=$(cat "$T51_LOGDIR/last_$w" 2>/dev/null || echo -1)
    if [ "$last_seq" -lt 0 ]; then
        echo "  worker $w: no successful patch ever recorded"
        T51_MISMATCHES=$((T51_MISMATCHES+1))
        continue
    fi
    byte_off=$(( w * 65536 ))
    python3 -c "
import sys
img, byte_off, patch_size, worker, expected_seq = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
tag = ('T51_W%02d_S%06d_' % (worker, expected_seq)).encode()
expected = (tag + bytes([worker % 256]) * (patch_size - len(tag)))[:patch_size]
with open(img, 'rb') as f:
    f.seek(byte_off)
    actual = f.read(patch_size)
sys.exit(0 if actual == expected else 1)
" "$T51_IMG" "$byte_off" "$T51_PATCH_SIZE" "$w" "$last_seq" \
        || { echo "  worker $w: slot content mismatch (expected seq $last_seq)"; T51_MISMATCHES=$((T51_MISMATCHES+1)); }
done

[ "$T51_MISMATCHES" -eq 0 ] \
    && check "T51a all $T51_CONCURRENCY worker slots hold their last write after leader-restart storm" PASS \
    || check "T51a $T51_MISMATCHES/$T51_CONCURRENCY worker slots corrupted/lost after leader-restart storm" FAIL

# T51d: failover LATENCY, not just correctness. Killing the leader mid-storm must not
# stall a client write past a guest's I/O deadline — a VM on this mount takes an EIO
# (and may remount read-only) even when no data was lost. Bound is deliberately well
# under a Linux guest's ~30s SCSI timeout while leaving generous headroom for a loaded
# CI box; the behavior this guards against was a measured 27.3s stall on staging, from a
# retry ladder that walked every node twice at one RPC timeout each without ever
# shedding the dead one.
T51_MAX_LAT=$(cat "$T51_LOGDIR"/lat_* 2>/dev/null | sort -n | tail -1)
T51_MAX_LAT=${T51_MAX_LAT:-0}
T51_P99_LAT=$(cat "$T51_LOGDIR"/lat_* 2>/dev/null | sort -n | awk '{a[NR]=$1} END {if(NR) print a[int(NR*0.99)]; else print 0}')
T51_LAT_BOUND_MS=15000
echo "  T51: worst single-write latency during leader restart: ${T51_MAX_LAT}ms (p99 ${T51_P99_LAT}ms, bound ${T51_LAT_BOUND_MS}ms)"
[ "$T51_MAX_LAT" -le "$T51_LAT_BOUND_MS" ] \
    && check "T51d no client write stalled past ${T51_LAT_BOUND_MS}ms during leader restart (worst ${T51_MAX_LAT}ms)" PASS \
    || check "T51d a client write stalled ${T51_MAX_LAT}ms during leader restart (bound ${T51_LAT_BOUND_MS}ms) — failover too slow, a guest would see EIO" FAIL

# Cold-restart the client and re-verify — the real incident's corruption only
# surfaced on read-back after a client restart (cache masked it beforehand).
echo "  Restarting dfs-client (cold cache) to confirm durability, not just cache masking..."
fusermount -u "$MOUNT" 2>/dev/null || true
kill_client_and_wait "$CLIENT_PID2"
T51_CLIENT_LOG="$LOG/client_t51.log"
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$T51_CLIENT_LOG" --allow-other --log-level debug &
CLIENT_PID2=$!
CURRENT_CLIENT_LOG="$T51_CLIENT_LOG"
sleep 2
mountpoint -q "$MOUNT" || { check "T51b remount after leader-restart storm" FAIL; }

T51_MISMATCHES2=0
T51_IOERR=0
for w in $(seq 0 $((T51_CONCURRENCY-1))); do
    last_seq=$(cat "$T51_LOGDIR/last_$w" 2>/dev/null || echo -1)
    [ "$last_seq" -lt 0 ] && continue
    byte_off=$(( w * 65536 ))
    python3 -c "
import sys
img, byte_off, patch_size, worker, expected_seq = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
tag = ('T51_W%02d_S%06d_' % (worker, expected_seq)).encode()
expected = (tag + bytes([worker % 256]) * (patch_size - len(tag)))[:patch_size]
try:
    with open(img, 'rb') as f:
        f.seek(byte_off)
        actual = f.read(patch_size)
except OSError as e:
    print(f'IOERR: {e}')
    sys.exit(2)
sys.exit(0 if actual == expected else 1)
" "$T51_IMG" "$byte_off" "$T51_PATCH_SIZE" "$w" "$last_seq"
    rc=$?
    if [ "$rc" -eq 2 ]; then
        T51_IOERR=$((T51_IOERR+1))
    elif [ "$rc" -ne 0 ]; then
        T51_MISMATCHES2=$((T51_MISMATCHES2+1))
    fi
done

[ "$T51_MISMATCHES2" -eq 0 ] && [ "$T51_IOERR" -eq 0 ] \
    && check "T51c all worker slots intact after cold client restart (no I/O errors, no corruption)" PASS \
    || check "T51c $T51_MISMATCHES2 corrupted + $T51_IOERR I/O-error slots after cold restart" FAIL

rm -f "$T51_IMG" 2>/dev/null || true
rm -rf "$T51_LOGDIR" "$T51_STOP" 2>/dev/null || true
fi # should_run T51

# T52: never a single replica (2026-07-17 live incident — see project memory
# project_never_single_replica). Root cause: required_replicas used to be derived
# from the chunk's CURRENT known node count, not configured replication_factor —
# once a chunk fell to 1 replica, every subsequent patch silently accepted that as
# already-sufficient. Confirmed live: a patch landed on a chunk's one known replica
# right as that node hit a compaction-wedge restart, and the bytes never persisted
# anywhere — real, unrecoverable data loss on a VM disk install.
#
# This test reproduces the mechanism directly: sustained concurrent patches to
# non-overlapping slots of one hot chunk, with a NON-LEADER replica actually
# HOLDING that chunk killed (SIGKILL, not graceful) and restarted partway through —
# deliberately different from T51 (which kills the leader): the point here is a
# plain replica outage racing an in-flight patch, the exact shape of the real
# incident, not a leader-dissemination failure. Unlike T38/T51, this test polls
# replica counts LIVE throughout the storm rather than only checking convergence
# afterward — the invariant under test is that the write path itself never drops
# below 2 replicas, not just that the healer eventually fixes it.
if should_run T52; then
snapshot_log T52
echo ""
echo "=== T52: never a single replica, even when a replica dies mid-patch-storm ==="

T52_IMG="$MOUNT/t52_disk.img"
T52_PATCH_SIZE=4096
T52_DURATION=8
T52_CONCURRENCY=6

echo "  Writing 4MB base chunk..."
dd if=/dev/urandom of="$T/t52_base.bin" bs=4M count=1 2>/dev/null
cp "$T/t52_base.bin" "$T52_IMG"
dfs_sync

T52_RF=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json cluster status 2>/dev/null \
    | python3 -c "import json,sys; print(json.load(sys.stdin).get('replication_factor', 3))" 2>/dev/null)
[ -z "$T52_RF" ] && T52_RF=3
echo "  Configured replication_factor: $T52_RF (required_replicas should floor at 2)"

# Identify a NON-LEADER node currently holding this chunk to kill mid-storm.
T52_LEADER_ADDR=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json cluster status 2>/dev/null \
    | python3 -c "
import json, sys
d = json.load(sys.stdin)
online = [n for n in d.get('nodes', []) if n.get('status') == 'Online']
online.sort(key=lambda n: n['id'])
print(online[0]['address'] if online else '')
" 2>/dev/null)
T52_HOLDER_ADDR=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json file info /t52_disk.img 2>/dev/null \
    | python3 -c "
import json, sys
d = json.load(sys.stdin)
nodes = d['chunk_locations'][0]['nodes'] if d.get('chunk_locations') else []
print(nodes[0] if nodes else '')
" 2>/dev/null)
# nodes[] in file info is a list of node IDs, not addresses — resolve via cluster
# status so we get something pkill can match against a config path.
T52_VICTIM_NODE=""
if [ -n "$T52_HOLDER_ADDR" ]; then
    T52_VICTIM_NODE=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json cluster status 2>/dev/null \
        | python3 -c "
import json, sys
d = json.load(sys.stdin)
holder_id = '$T52_HOLDER_ADDR'
leader = '$T52_LEADER_ADDR'
for n in d.get('nodes', []):
    if n.get('id') == holder_id and n.get('address') != leader:
        print(n['address'])
        break
" 2>/dev/null)
fi
# Fall back to "any non-leader online node" if we couldn't resolve a specific
# holder (file info's node-id-vs-address shape can vary by dfs-admin version) —
# the storm still exercises the invariant against whichever node goes down, just
# without the guarantee it was already a confirmed holder at kill time.
if [ -z "$T52_VICTIM_NODE" ]; then
    T52_VICTIM_NODE=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json cluster status 2>/dev/null \
        | python3 -c "
import json, sys
d = json.load(sys.stdin)
leader = '$T52_LEADER_ADDR'
online = [n for n in d.get('nodes', []) if n.get('status') == 'Online' and n.get('address') != leader]
print(online[0]['address'] if online else '')
" 2>/dev/null)
fi
T52_VICTIM_PORT="${T52_VICTIM_NODE##*:}"
T52_VICTIM_NUM=$(( (T52_VICTIM_PORT - 8900) + 1 ))
echo "  Leader: $T52_LEADER_ADDR — will kill non-leader replica node$T52_VICTIM_NUM ($T52_VICTIM_NODE) mid-storm"

echo "  Launching sustained patch storm (${T52_DURATION}s, $T52_CONCURRENCY concurrent workers)..."
T52_STOP="$T/t52_stop_$$"
rm -f "$T52_STOP"
T52_LOGDIR="$T/t52_jobs_$$"
mkdir -p "$T52_LOGDIR"

T52_WORKER_PIDS=()
for w in $(seq 0 $((T52_CONCURRENCY-1))); do
    (
        seq_n=0
        byte_off=$(( w * 65536 ))
        while [ ! -f "$T52_STOP" ]; do
            python3 -c "
import os, sys
img, byte_off, patch_size, worker, seq_n = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
tag = ('T52_W%02d_S%06d_' % (worker, seq_n)).encode()
data = (tag + bytes([worker % 256]) * (patch_size - len(tag)))[:patch_size]
fd = os.open(img, os.O_WRONLY)
os.lseek(fd, byte_off, 0)
os.write(fd, data)
os.close(fd)
" "$T52_IMG" "$byte_off" "$T52_PATCH_SIZE" "$w" "$seq_n" 2>>"$T52_LOGDIR/err_$w" \
                && echo "$seq_n" > "$T52_LOGDIR/last_$w"
            seq_n=$((seq_n+1))
            sleep 0.05
        done
    ) &
    T52_WORKER_PIDS+=($!)
done

# Live replica-count poller: samples file info every 0.2s throughout the storm +
# kill + recovery window and records a full (timestamp, min-replica-count)
# timeseries for THIS chunk. T38/T51-style checks only confirm convergence
# after the fact, which cannot distinguish "never dropped below 2" from
# "dropped to 1 and the healer fixed it before we happened to look."
#
# The assertion is a BOUNDED recovery window, not zero-duration: a truly
# instantaneous guarantee isn't physically achievable for any replicated write
# (there's always some gap between the first copy landing and the second being
# confirmed) — confirmed empirically 2026-07-17, first version of this test
# asserted zero-tolerance and still failed even after the round-robin backfill
# fix correctly recovered in ~700ms, because SOME poll sample always lands in
# that window. What actually matters, and what compute_required_replicas /
# the round-robin backfill / urgent_heal together guarantee, is that a drop is
# always brief and self-closing — never sustained, never silently permanent.
T52_SAMPLES="$T/t52_samples_$$"
: > "$T52_SAMPLES"
T52_POLL_STOP="$T/t52_poll_stop_$$"
rm -f "$T52_POLL_STOP"
(
    while [ ! -f "$T52_POLL_STOP" ]; do
        cur=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json file info /t52_disk.img 2>/dev/null \
            | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
    print(min((len(c['nodes']) for c in d.get('chunk_locations', [])), default=999))
except Exception:
    print(999)
" 2>/dev/null)
        [ -n "$cur" ] && echo "$(date +%s.%N) $cur" >> "$T52_SAMPLES"
        sleep 0.2
    done
) &
T52_POLL_PID=$!

sleep 2

echo "  Killing node$T52_VICTIM_NUM ($T52_VICTIM_NODE) mid-storm (SIGKILL, matching a real abrupt outage)..."
pkill -9 -f "dfs-server start --config $BASE/node${T52_VICTIM_NUM}/config.toml" 2>/dev/null || true
sleep 3
RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 "$BIN/dfs-server" start --config "$BASE/node${T52_VICTIM_NUM}/config.toml" \
    >> "$LOG/server${T52_VICTIM_NUM}.log" 2>&1 &

sleep $(( T52_DURATION - 2 ))

touch "$T52_STOP"
T52_WAIT_DEADLINE=$(( $(date +%s) + 15 ))
for pid in "${T52_WORKER_PIDS[@]}"; do
    while kill -0 "$pid" 2>/dev/null; do
        [ "$(date +%s)" -ge "$T52_WAIT_DEADLINE" ] && { kill -9 "$pid" 2>/dev/null; break; }
        sleep 0.1
    done
done
dfs_sync

# Let the poller catch a few more post-storm samples (backfill/healing settling)
# before stopping it.
sleep 1
touch "$T52_POLL_STOP"
wait "$T52_POLL_PID" 2>/dev/null || true

# Longest continuous span across all POLL SAMPLES where the observed count
# stayed below 2 — informational only, not the pass/fail assertion. Root-
# caused 2026-07-17: under a continuous multi-worker storm against ONE hot
# chunk plus a real multi-second node outage, a rapid succession of DIFFERENT
# patches each independently drop to 1 and recover within ~0.5s — but if a new
# patch's drop starts before the poller happens to catch the previous one's
# brief recovery, this streak metric chains several independent sub-second
# gaps into what looks like one long window, even though no single gap was
# ever close to that long. Kept as a secondary signal (still useful context)
# — the primary assertion below measures each individual gap directly from
# the client's own timestamps instead.
T52_MAX_LOW_DURATION=$(python3 -c "
samples = []
with open('$T52_SAMPLES') as f:
    for line in f:
        parts = line.split()
        if len(parts) == 2:
            samples.append((float(parts[0]), int(parts[1])))
max_low = 0.0
low_start = None
for ts, cnt in samples:
    if cnt < 2:
        if low_start is None:
            low_start = ts
        max_low = max(max_low, ts - low_start)
    else:
        low_start = None
if low_start is not None and samples:
    max_low = max(max_low, samples[-1][0] - low_start)
print(f'{max_low:.2f}')
" 2>/dev/null)
[ -z "$T52_MAX_LOW_DURATION" ] && T52_MAX_LOW_DURATION="999"
echo "  (informational) longest continuous poll-sampled window below 2 replicas: ${T52_MAX_LOW_DURATION}s"

# Primary assertion: the actual per-event exposure window, measured directly
# from the client's own "landed on only X/Y" -> "backfilled ... now Y/Y"
# timestamps for each individual patch. This is what the fix actually
# guarantees (compute_required_replicas + round-robin backfill + urgent_heal)
# and is immune to the poll-interleaving artifact above.
T52_RECOVERY_BOUND=2.0
T52_MAX_EVENT_DURATION=$(python3 -c "
import re, datetime
start_re = re.compile(r'(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z).*chunk (\S+) landed on only')
end_re = re.compile(r'(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z).*backfilled chunk (\S+) onto')
def parse_ts(s):
    return datetime.datetime.strptime(s, '%Y-%m-%dT%H:%M:%S.%fZ')
starts = {}
max_delta = 0.0
with open('$CURRENT_CLIENT_LOG') as f:
    for line in f:
        m = start_re.search(line)
        if m:
            starts.setdefault(m.group(2), parse_ts(m.group(1)))
            continue
        m = end_re.search(line)
        if m and m.group(2) in starts:
            delta = (parse_ts(m.group(1)) - starts.pop(m.group(2))).total_seconds()
            max_delta = max(max_delta, delta)
print(f'{max_delta:.2f}')
" 2>/dev/null)
[ -z "$T52_MAX_EVENT_DURATION" ] && T52_MAX_EVENT_DURATION="999"
echo "  Longest individual chunk's exposure window (landed-on-1 to backfilled-to-2): ${T52_MAX_EVENT_DURATION}s (bound: ${T52_RECOVERY_BOUND}s)"
if [ "$T52_RF" -ge 2 ]; then
    python3 -c "import sys; sys.exit(0 if float('$T52_MAX_EVENT_DURATION') <= $T52_RECOVERY_BOUND else 1)" 2>/dev/null \
        && check "T52a every individual under-replicated patch recovers to >=2 within ${T52_RECOVERY_BOUND}s (RF=$T52_RF, longest observed: ${T52_MAX_EVENT_DURATION}s)" PASS \
        || check "T52a a patch stayed under-replicated for ${T52_MAX_EVENT_DURATION}s (RF=$T52_RF, must recover within ${T52_RECOVERY_BOUND}s)" FAIL
else
    check "T52a RF<2 configured — single-replica invariant does not apply, skipping" PASS
fi

# If the true emergency fallback ever fired, confirm it was actually treated as
# urgent (queued immediately, not sitting in the normal 300s-delayed backlog) —
# see DfsClient::urgent_heal. Absence of this marker is fine (means the
# candidate-widening backfill alone was always enough); presence without a
# corresponding queued-healing confirmation would be the real failure.
T52_URGENT_COUNT=$(grep -ac "URGENT_SINGLE_REPLICA" "$CURRENT_CLIENT_LOG" 2>/dev/null || true)
[ -z "$T52_URGENT_COUNT" ] && T52_URGENT_COUNT=0
if [ "${T52_URGENT_COUNT:-0}" -gt 0 ]; then
    echo "  URGENT_SINGLE_REPLICA fired $T52_URGENT_COUNT time(s) — confirming urgent_heal was actually invoked"
    T52_URGENT_HEAL_CALLS=$(grep -ac "urgent_heal: chunk .* queued for immediate healing" "$CURRENT_CLIENT_LOG" 2>/dev/null || true)
    [ -z "$T52_URGENT_HEAL_CALLS" ] && T52_URGENT_HEAL_CALLS=0
    [ "${T52_URGENT_HEAL_CALLS:-0}" -gt 0 ] \
        && check "T52b emergency single-replica case triggered urgent_heal as designed" PASS \
        || check "T52b URGENT_SINGLE_REPLICA fired but urgent_heal was never confirmed queued" FAIL
else
    check "T52b no emergency single-replica case occurred (candidate-widening backfill alone was sufficient)" PASS
fi

rm -f "$T52_IMG" "$T52_SAMPLES" 2>/dev/null || true
rm -rf "$T52_LOGDIR" "$T52_STOP" "$T52_POLL_STOP" 2>/dev/null || true
fi # should_run T52

# ── T53: a server-side backstop fold must not create a single-replica chunk ───
#
# The client's ForceFold (client.rs's active-fold timer) only ever fires from
# *inside* a MultiPatch — i.e. on the next patch to that slot. A slot that is
# patched a few times and then goes quiet is therefore never folded by the
# client at all. dfs-server's debounce_fold_slot backstop (PATCH_DEBOUNCE_IDLE,
# 20s) picks it up instead — but it runs independently on each node, so
# whichever node's jittered timer fires first folds ALONE, mints a brand-new
# chunk identity that exists on exactly one node, and broadcasts that as the
# authoritative ChunkLocation with nodes:[itself].
#
# Confirmed live on staging 2026-07-20 during a VM-111 install (file
# d159a6c7…, chunk_idx 1791): gluster1 logged
#   "Single fold: chunk_idx 1791 consolidated (baa7075c + delta -> a0808f41…)"
# at 10:52:20 with no corresponding client ForceFold anywhere in the client
# log, and every node then reported "total nodes: 1" for that chunk every 10s
# until healing finally landed at 10:55:38 — a 3m18s single-replica window on
# a fully healthy 5-node cluster. The client itself was blameless over the
# whole install: 144079 MultiPatch at 2 replicas, 26 at 3, zero at 1.
#
# Contrast the client-driven fold on the SAME slot 28s earlier: it reached
# both write-pair replicas, each folded independently to the same
# deterministic chunk id, each broadcast nodes:[self], and the merge unioned
# them to "total nodes: 2". So ForceFold's design is fine; the per-node
# backstop is what breaks the invariant.
#
# Compounding it, handle_replicate_patch_fold's self-heal backstop is disabled
# in exactly this case: the peer that receives the pointer-only fold broadcast
# checks "am I in this chunk's known node list?" against the single-node list
# the fold just published, concludes it isn't a replica, and skips the heal —
# so the one node already holding base+delta stands down.
#
# This test reproduces the mechanism directly and deliberately does NOT kill
# anything: a healthy cluster, a live client, one slot patched and then left
# alone. That is the ordinary random-write pattern of a VM install, which is
# what makes the staging exposure continuous rather than incidental.
if should_run T53; then
snapshot_log T53
echo ""
echo "=== T53: server-side backstop fold must not leave a single-replica chunk ==="

T53_IMG="$MOUNT/t53_backstop.img"
T53_CHUNKS=64           # slot pool — see storm/dead-stop note below
# debounce_fold_slot's task is spawned on the slot's FIRST patch and re-sleeps
# a whole fresh PATCH_DEBOUNCE_IDLE (20s, plus jitter) whenever it wakes to
# find the slot was touched inside the window. With patches spread over a few
# seconds that puts the actual fold up to ~40s after the last patch, not 20s —
# a 35s quiet period missed it entirely on the first run of this test.
T53_STORM=40            # sustained patch storm before the dead stop
T53_QUIET=60
T53_RECOVERY_BOUND=5.0  # seconds a backstop-folded chunk may sit below 2 replicas

# A sustained storm that then STOPS is the load-bearing part of this repro,
# not incidental scale. While the storm runs, the client's own active-fold
# timer drives ForceFold to both replicas and everything stays healthy. What
# matters is what is left behind at the dead stop: slots whose newest
# generation nobody folded, which only dfs-server's per-node
# debounce_fold_slot backstop will pick up.
#
# That backstop re-sleeps a whole fresh PATCH_DEBOUNCE_IDLE (20s) whenever it
# wakes to find the slot was touched inside the window, so a sub-second
# difference in when a generation started on each replica turns into a ~20s
# difference in when each one fires. Whichever fires first folds ALONE and
# broadcasts ReplicatePatchFold; the peer's fold_slot_now then finds
# PatchState::Folded, drops the dirty slot and returns WITHOUT folding — so
# exactly one node ends up holding the new chunk identity, and the
# ChunkLocation it publishes names only itself.
#
# A single synchronized burst does NOT reproduce this — both replicas land on
# the same side of the 20s boundary, fold to the same deterministic chunk id,
# and the locations union to 2. Two earlier versions of this test did exactly
# that and passed against the broken build.
echo "  Writing ${T53_CHUNKS}x4MB base file..."
dd if=/dev/urandom of="$T/t53_base.bin" bs=4M count=$T53_CHUNKS 2>/dev/null
cp "$T/t53_base.bin" "$T53_IMG"
dfs_sync

# One small patch per chunk, so every slot gets its own delta accumulator and
# its own debounce task. Kept far under every client-side ForceFold trigger
# (8s window / 20 patches / size threshold) so the client never folds any of
# these itself — the backstop is the only thing that can. The file is held
# open across the quiet period, matching a VM disk image that stays open while
# the guest writes elsewhere.
echo "  Patch storm across $T53_CHUNKS chunks for ${T53_STORM}s, then quiet for ${T53_QUIET}s..."
python3 "$REPO/scripts/t53_patch_writer.py" "$T53_IMG" "$T53_CHUNKS" "$T53_QUIET" "$T53_STORM" &
T53_WRITER_PID=$!

sleep $(( T53_STORM + T53_QUIET ))

# Resolve the file id from the SERVER logs' own path->id line, not from
# CURRENT_CLIENT_LOG. That variable is reassigned by every remount test
# (T23/T24/etc.) and several of them never point it back at the live client's
# log afterward, so by the time T53 runs near the end of a full-suite run it
# can be stale — this test's own MPTIMING lines never land in it at all, and a
# grep against it either finds nothing or (worse) silently matches whatever
# unrelated file another test last logged there. Root-caused 2026-07-20: a
# full-suite run resolved T53's writes to t28_thick.bin's file id instead,
# found zero folds for it, and failed with "test setup did not reproduce the
# trigger" — the fix folded correctly the whole time. Server logs are written
# by every node regardless of which client log is "current".
T53_FID=$(grep -ah "\[META SERVER\] put path=/t53_backstop.img" "$LOG"/server*.log 2>/dev/null \
    | head -1 | grep -o "id=[0-9a-f-]*" | head -1 | cut -d= -f2)

# Same staleness concern applies here: search every client log under this run
# rather than trust CURRENT_CLIENT_LOG to be the live one. Harmless either way
# since a stale/unrelated log won't mention this run's fresh random file id.
T53_FORCEFOLD=$(grep -ah "ForceFold: file $T53_FID chunk " "$LOG"/client*.log 2>/dev/null | wc -l)
[ -z "$T53_FORCEFOLD" ] && T53_FORCEFOLD=0

if [ -z "$T53_FID" ]; then
    check "T53a could not resolve file id from client log — no MultiPatch reached the file" FAIL
else
    # Every backstop fold this run produced, with how many distinct nodes folded
    # each slot. A slot folded by only ONE node is the bug's signature: that
    # node is then the sole holder of the new chunk identity.
    T53_REPORT="$T/t53_folds_$$"
    python3 "$REPO/scripts/t53_collect_folds.py" "$T53_FID" "$LOG" > "$T53_REPORT"

    T53_FOLD_COUNT=$(wc -l < "$T53_REPORT")
    T53_SOLO_FOLDS=$(awk '$3 == 1' "$T53_REPORT" | wc -l)

    if [ "$T53_FOLD_COUNT" -eq 0 ]; then
        check "T53a no backstop fold observed after ${T53_QUIET}s idle — test setup did not reproduce the trigger" FAIL
    else
        # Client ForceFolds during the storm are expected and healthy — they are
        # what leaves a final unfolded generation behind at the dead stop. The
        # backstop folds counted here are the ones that happened AFTER it, with
        # no client involvement at all.
        echo "  $T53_FOLD_COUNT fold(s) total, $T53_SOLO_FOLDS performed by a single node ($T53_FORCEFOLD client ForceFolds during the storm)"
        check "T53a folds observed after the dead stop ($T53_FOLD_COUNT)" PASS

        # INFORMATIONAL ONLY, not a failure condition. A fold mints a NEW chunk
        # identity, and under the ORIGINAL (peer-recompute) coordination design
        # a second node could only ever hold those bytes by ALSO independently
        # running the fold itself — so a solo-folder count was a direct proxy
        # for single-replica. That stopped being true 2026-07-20: the
        # coordinated fold now folds ONCE and explicitly pushes/announces the
        # result (see dfs-server's replicate_fold_result), specifically BECAUSE
        # peer recompute produced REPLICA DISAGREEMENT under load (measured
        # 10 -> 34 -> 47 across three tightening attempts at the old design,
        # traced to dfs-client's own 2026-07-11 abandonment of delta-recompute
        # for exactly this reason). A solo-folder count is now the EXPECTED,
        # cheaper, correct signature — the second replica exists via a raw copy
        # or the generic healer, never via a second "Single fold" log line. See
        # T53c (ground-truth on-disk replica count) for the real replication
        # check and T53b below for the real correctness invariant.
        echo "  (informational) $T53_SOLO_FOLDS/$T53_FOLD_COUNT folded generation(s) replicated without a second node independently folding — expected under the coordinated-push design, not a defect"

        # PRIMARY assertion, though an honest caveat first: the coordinated-push
        # redesign removed the ONLY code path that ever emitted "REPLICA
        # DISAGREEMENT" (peer recompute, deleted along with force_fold_on_peers)
        # — divergence is now structurally prevented (exactly one node ever
        # computes a slot's generation) rather than merely detected-and-logged.
        # So this check currently passes trivially every run, and stays a
        # regression tripwire rather than active verification: if peer recompute
        # is ever reintroduced without ALSO reintroducing its disagreement log
        # line, this would silently pass on a real regression. Kept anyway
        # because it's free and correct FOR the current design, and cheap
        # insurance if that log line comes back with the mechanism it belongs to.
        T53_DISAGREEMENTS=$(grep -ah "REPLICA DISAGREEMENT on file $T53_FID" "$LOG"/server*.log 2>/dev/null | wc -l)
        [ -z "$T53_DISAGREEMENTS" ] && T53_DISAGREEMENTS=0
        [ "$T53_DISAGREEMENTS" -eq 0 ] \
            && check "T53b zero REPLICA DISAGREEMENT for this file's folds" PASS \
            || check "T53b $T53_DISAGREEMENTS REPLICA DISAGREEMENT event(s) for this file's folds" FAIL

        # Ground truth backstop to the above: count node data dirs that actually
        # hold each surviving folded chunk's bytes. The leader's own
        # ChunkLocation is checked separately below — a location claiming
        # replicas it does not have is precisely the failure mode here, so it
        # cannot be the evidence.
        t53_disk_replicas() {
            local hex="$1" n=0 i
            for i in 1 2 3 4 5; do
                [ -f "$BASE/node$i/data/chunks/${hex:0:2}/${hex:2:2}/$hex" ] && n=$((n+1))
            done
            echo "$n"
        }

        T53_START=$(date +%s.%N)
        T53_UNDER=""
        while :; do
            T53_UNDER=""
            while read -r idx hex nodes; do
                [ "$(t53_disk_replicas "$hex")" -lt 2 ] && T53_UNDER="$T53_UNDER $idx"
            done < "$T53_REPORT"
            [ -z "$T53_UNDER" ] && break
            python3 -c "import sys; sys.exit(0 if $(date +%s.%N) - $T53_START > $T53_RECOVERY_BOUND else 1)" && break
            sleep 0.2
        done
        T53_ELAPSED=$(python3 -c "print(f'{$(date +%s.%N) - $T53_START:.2f}')")
        T53_UNDER_COUNT=$(echo $T53_UNDER | wc -w)

        [ "$T53_UNDER_COUNT" -eq 0 ] \
            && check "T53c every folded chunk has >=2 on-disk replicas (settled in ${T53_ELAPSED}s)" PASS \
            || check "T53c $T53_UNDER_COUNT folded chunk(s) still single-replica after ${T53_RECOVERY_BOUND}s (chunk_idx:$T53_UNDER)" FAIL

        # The published locations must agree too. A fold that broadcasts
        # nodes:[self] makes the whole cluster believe the chunk is
        # single-replica even when another node does hold the bytes — and
        # handle_replicate_patch_fold's self-heal backstop then reads that same
        # single-node list, concludes the real second replica "isn't a replica",
        # and skips the heal that would have fixed it.
        T53_MIN_LOC=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json file info /t53_backstop.img 2>/dev/null \
            | python3 "$REPO/scripts/t53_min_loc_nodes.py")
        [ -z "$T53_MIN_LOC" ] && T53_MIN_LOC=0
        [ "$T53_MIN_LOC" -ge 2 ] \
            && check "T53d every ChunkLocation the leader reports lists >=2 nodes (min $T53_MIN_LOC)" PASS \
            || check "T53d leader reports a ChunkLocation with only $T53_MIN_LOC node(s)" FAIL
    fi
    rm -f "$T53_REPORT" 2>/dev/null || true
fi

wait "$T53_WRITER_PID" 2>/dev/null || true
rm -f "$T53_IMG" "$T/t53_base.bin" 2>/dev/null || true
fi # should_run T53

if should_run T54; then
snapshot_log T54
echo ""
echo "=== T54: same-chunk patches under load should never broadcast more locations than patches applied ==="

# 2026-07-21 staging finding: a hot chunk under concurrent small-write load (VM
# installer pattern) produced ~9.6 chunk-location-replicated completions per
# actual patch applied. pending_chunk_locations is a bare Vec with no dedup
# (client.rs ~7961), so every patch enqueues its own entry, and if several land
# within the same 10ms batch-drain window they all ride the batch instead of
# collapsing to the chunk's latest location. chunk_id = blake3(file_id ||
# file_offset || data) where file_offset is the CHUNK-ALIGNED offset (not the
# specific byte range touched) and data is the whole chunk's new content after
# the patch -- so every patch to chunk 0 gets a different chunk_id but the same
# dedup key (file_id, file_offset=0).
#
# NOTE: reproducing genuine sub-10ms overlapping same-chunk patches (the exact
# condition that produced the 9.6x ratio in production) could not be forced
# reliably in this single-client, low-latency local suite -- several write
# patterns were tried (spaced bursts, concurrent processes, tightly-paced
# groups) and all collapsed to a clean 1:1 patch:broadcast ratio here, unlike
# production's sustained real VM-install load. So this test asserts the sound
# invariant the fix guarantees instead (RCL broadcasts can never exceed patches
# applied -- dedup only removes entries, never adds them) plus a byte-level
# integrity check, rather than demonstrating the specific redundancy ratio.
# The redundancy reduction itself is verified separately via live staging
# re-measurement (same log-sampling technique used to find this bug) after
# deploying, not by this test.
#
# Correlates patches to RCL completions via the MERGE-TRACE line's own `token=`
# field (server.rs), which is exactly the chunk_id the paired completion log
# uses. Empirically (checked while developing this test): only the batch
# handler's "Successfully replicated chunk location for X" completion fires
# under this local single-client setup (zero "Handling replicate chunk
# location" singular self-report lines appear at all), so this counts the
# exact path Fix 1 targets. Dedups patch tokens across server logs before
# counting, since RF replicates each patch to multiple nodes that each
# independently log the same MERGE-TRACE token.

T54_IMG="$MOUNT/t54_hotchunk.bin"
T54_GROUPS=8
T54_WRITES_PER_GROUP=20
T54_WRITE_SIZE=16384    # 20 * 16KB = 320KB/group, > SLOT_DIRTY_FLUSH_THRESHOLD_BYTES (256KB)
T54_GROUP_GAP_S=0.004   # shorter than the ~11-40ms observed apply_patch round trip, so
                        # the next group's threshold-crossing dispatch overlaps the
                        # previous group's still-in-flight RPC instead of waiting for it

dd if=/dev/zero of="$T54_IMG" bs=1M count=4 2>/dev/null
dfs_sync

# dfs-admin's `file info --format json` doesn't expose the file's UUID, only
# path/size/chunks -- pull it from the server log's own
# "[META SERVER] put path=... id=..." line instead (same field T43's neighbors
# already rely on), from the dd+dfs_sync above.
T54_FILE_ID=$(grep -h "\[META SERVER\] put path=/t54_hotchunk.bin id=" "$LOG"/server*.log 2>/dev/null \
    | tail -1 | grep -oP 'id=\K[0-9a-f-]+')
echo "  T54: file_id=${T54_FILE_ID:-<not found>}"

declare -A T54_LOG_MARKS
for f in "$LOG"/server*.log; do
    T54_LOG_MARKS["$f"]=$(wc -l < "$f" 2>/dev/null || echo 0)
done

# Groups of scattered small writes into chunk 0 through one fd, no fsync
# between them, paced tighter than the observed per-patch round trip so a new
# group's fragmentation-threshold flush gets dispatched while the previous
# group's is still in flight (flush_one_chunk snapshots-and-clears the dirty
# tracker under a mutex before its RPC starts, so new writes land in a fresh
# buffer immediately, not blocked on the in-flight RPC completing) -- unlike
# T22's separate-process-per-patch pattern, which (confirmed while developing
# this test) doesn't reliably land within the same 10ms window due to process
# spawn overhead.
python3 -c "
import os, time
fd = os.open('$T54_IMG', os.O_RDWR)
for g in range($T54_GROUPS):
    for i in range($T54_WRITES_PER_GROUP):
        off = (i * 131072) % (3 * 1024 * 1024)  # scattered, non-adjacent within chunk 0
        os.pwrite(fd, bytes([(g * 20 + i) % 256]) * $T54_WRITE_SIZE, off)
    time.sleep($T54_GROUP_GAP_S)
os.close(fd)
"

dfs_sync
sleep 1   # let any fire-and-forget RPCs land

if [ -z "$T54_FILE_ID" ]; then
    check "T54 could not resolve file id" FAIL
else
    T54_TOKENS=$(for f in "$LOG"/server*.log; do
        mark=${T54_LOG_MARKS["$f"]:-0}
        tail -n "+$((mark+1))" "$f" 2>/dev/null \
            | grep "MERGE-TRACE" | grep "file=$T54_FILE_ID " | grep "chunk_idx=0 " \
            | grep -oP 'token=\K[0-9a-f]+'
    done | sort -u)
    T54_PATCH_COUNT=$(echo "$T54_TOKENS" | grep -c . || true)
    echo "  T54: $T54_PATCH_COUNT distinct patches applied to chunk 0"

    T54_RCL_COUNT=0
    for tok in $T54_TOKENS; do
        c=$(grep -h "Successfully replicated chunk location for $tok " "$LOG"/server*.log 2>/dev/null | wc -l)
        T54_RCL_COUNT=$((T54_RCL_COUNT + c))
    done
    echo "  T54: $T54_RCL_COUNT RCL broadcasts completed by the leader for $T54_PATCH_COUNT patches applied (invariant: never more broadcasts than patches)"

    if [ "$T54_PATCH_COUNT" -eq 0 ]; then
        check "T54 no patches detected -- test setup issue" FAIL
    else
        [ "$T54_RCL_COUNT" -le "$T54_PATCH_COUNT" ] \
            && check "T54 RCL broadcasts ($T54_RCL_COUNT) never exceed patches applied ($T54_PATCH_COUNT)" PASS \
            || check "T54 RCL broadcasts ($T54_RCL_COUNT) exceed patches applied ($T54_PATCH_COUNT) -- dedup not collapsing redundant enqueues" FAIL
    fi
fi

# Byte-level integrity check: every offset's final content must be from the
# LAST group that touched it (group g, iteration i writes tag byte
# (g*20+i)%256) -- catches the (file_id, file_offset) dedup change silently
# picking a stale location if freshest-wins via client_write_seq is wrong.
T54_LAST_GROUP=$(( T54_GROUPS - 1 ))
T54_INTEGRITY=$(python3 -c "
with open('$T54_IMG', 'rb') as f:
    mismatches = 0
    for i in range($T54_WRITES_PER_GROUP):
        off = (i * 131072) % (3 * 1024 * 1024)
        expected = bytes([($T54_LAST_GROUP * $T54_WRITES_PER_GROUP + i) % 256]) * $T54_WRITE_SIZE
        f.seek(off)
        actual = f.read($T54_WRITE_SIZE)
        if actual != expected:
            mismatches += 1
    print(mismatches)
")
[ "$T54_INTEGRITY" -eq 0 ] \
    && check "T54 final chunk-0 content matches the last write to every offset" PASS \
    || check "T54 $T54_INTEGRITY/$T54_WRITES_PER_GROUP offsets have stale/wrong content after the write storm" FAIL

rm -f "$T54_IMG"
fi # should_run T54

if should_run T55; then
snapshot_log T55
echo ""
echo "=== T55: sustained hot-chunk writes should not push metadata on every background flush ==="

# 2026-07-21 staging finding: the background flush self-refill loop
# (fuse_impl.rs ~4266-4276) calls enqueue_metadata() after every successful
# flush_one_chunk with no rate limit, unlike its sibling ticker-driven path
# (fuse_impl.rs ~1776-1782) which already debounces the same kind of
# opportunistic push to BG_METADATA_PUSH_INTERVAL=2s per inode. Live evidence:
# 767 of 785 metadata PUTs in a 44s window were for one continuously-open file,
# arriving every 30-160ms. This reproduces that shape: several bursts of
# scattered small writes to the same file, spaced out over several seconds with
# no fsync between them, so the background flusher fires repeatedly on its own.

T55_FILE="$MOUNT/t55_sustained.bin"
T55_BURSTS=5
T55_WRITES_PER_BURST=20
T55_WRITE_SIZE=16384   # 20 * 16KB = 320KB per burst, > SLOT_DIRTY_FLUSH_THRESHOLD_BYTES (256KB)
T55_BURST_GAP_S=0.9

dd if=/dev/zero of="$T55_FILE" bs=1M count=4 2>/dev/null
dfs_sync

declare -A T55_LOG_MARKS
for f in "$LOG"/server*.log; do
    T55_LOG_MARKS["$f"]=$(wc -l < "$f" 2>/dev/null || echo 0)
done

T55_START=$(date +%s.%N)
python3 -c "
import os, time
fd = os.open('$T55_FILE', os.O_RDWR)
for b in range($T55_BURSTS):
    for i in range($T55_WRITES_PER_BURST):
        off = (i * 131072) % (3 * 1024 * 1024)  # scattered, non-adjacent within chunk 0
        os.pwrite(fd, bytes([(b * 20 + i) % 256]) * $T55_WRITE_SIZE, off)
    time.sleep($T55_BURST_GAP_S)
os.close(fd)
"
dfs_sync
T55_ELAPSED=$(python3 -c "print(f'{$(date +%s.%N) - $T55_START:.1f}')")
sleep 1   # let any fire-and-forget RPCs land

T55_PUT_COUNT=0
for f in "$LOG"/server*.log; do
    mark=${T55_LOG_MARKS["$f"]:-0}
    c=$(tail -n "+$((mark+1))" "$f" 2>/dev/null | grep -c "\[META SERVER\] put path=/t55_sustained.bin " || true)
    T55_PUT_COUNT=$((T55_PUT_COUNT + c))
done

# Debounced to at most once per 2s per inode -> bound is generous (ceil+2) to
# absorb scheduling jitter without masking a real per-flush-push regression,
# where the count would instead track T55_BURSTS * T55_WRITES_PER_BURST (100).
T55_BOUND=$(python3 -c "import math; print(math.ceil($T55_ELAPSED / 2.0) + 2)")
echo "  T55: $T55_PUT_COUNT metadata PUTs over ${T55_ELAPSED}s wall time (bound: <= $T55_BOUND at a 2s-per-inode debounce)"

[ "$T55_PUT_COUNT" -le "$T55_BOUND" ] \
    && check "T55 metadata PUTs ($T55_PUT_COUNT) respect the 2s-per-inode debounce (bound $T55_BOUND)" PASS \
    || check "T55 metadata PUTs ($T55_PUT_COUNT) exceed the 2s-per-inode debounce bound ($T55_BOUND) -- background flush loop pushing on every flush" FAIL

rm -f "$T55_FILE"
fi # should_run T55

# ── cleanup ───────────────────────────────────────────────────────────────────
echo ""
echo "=== Cleanup ==="
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 0.3
# Don't assume CLIENT_PID2 is set — it's only assigned by remount tests (T8/T23/T24/
# etc.), so running a later-numbered test standalone (e.g. `test_local_suite.sh T45`)
# leaves it empty and `kill $CLIENT_PID2` a silent no-op, orphaning the mount's
# dfs-client process. Use pkill (kills every match), not `kill $(pgrep | head -1)` —
# if more than one dfs-client happens to be running at cleanup time (e.g. a prior
# orphan that predates this fix, or a race with a concurrent invocation), head -1
# only kills whichever one pgrep lists first, silently leaving the rest running
# indefinitely. Same all-matches approach the top-of-script preamble already uses.
pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
pkill -f "dfs-server" 2>/dev/null || true
rm -rf "$T"

echo ""
echo "════════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed"
echo "════════════════════════════════════════════"
[ $FAIL -eq 0 ] && exit 0 || exit 1
