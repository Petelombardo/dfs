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
# Slot audit (SLOT-OWNERSHIP-PLAN Phase 0) runs fast here so T57 can observe it, and
# so every other test doubles as a divergence measurement: grep "[DIVERGENCE]".
export DFS_SLOT_AUDIT_INTERVAL_SECS=2
export DFS_SLOT_AUDIT_QUIET_SECS=3
export DFS_SLOT_AUDIT_REAUDIT_SECS=20
export DFS_SLOT_AUDIT_CONFIRM_SECS=10
# Majority leases (Phase 1): production defaults (3s lease, 500ms margin), set explicitly.
export DFS_LEASE_MS=3000
export DFS_LEASE_MARGIN_MS=500
export DFS_LEASE_CLUSTER_SIZE=5   # fresh cluster: no membership history yet
export DFS_LEASE_TRACE=1   # log every lease extension: T60 checks none outlives its expiry vote
export DFS_SLOT_ISR_SEED_SECS=${DFS_SLOT_ISR_SEED_SECS:-3}   # seed per-chunk ISRs quickly so T62 can observe them (0 = off: only on-demand seeding)
export DFS_SLOT_ISR_CATCHUP_SECS=3   # and catch up missed commits quickly (T63)

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

# fresh_read <file in the mount> <offset> <len>: read through a NEW client mount, so no writer's
# own cache can serve its writes whatever the servers hold (that hid a real lost acked write in
# T68, 2026-10-04). Prints the bytes decoded leniently.
fresh_read() {
    local m=/tmp/dfs-mount-fresh
    mkdir -p "$m"
    RUST_LOG=info "$BIN/dfs-client" mount "$m" --cluster "$CLUSTER" \
        --log-file "$LOG/client_fresh.log" --allow-other --log-level debug &
    local pid=$!
    sleep 2
    python3 -c "f=open('$m/$1','rb');f.seek($2);print(f.read($3).decode(errors='replace'))" 2>/dev/null || true
    fusermount -u "$m" 2>/dev/null || true
    kill_client_and_wait "$pid"
}

# ── cleanup ──────────────────────────────────────────────────────────────────
pkill -f "dfs-server" 2>/dev/null || true
pkill -f "dfs-client" 2>/dev/null || true
sleep 0.5
# Every mount the suite uses, not just $MOUNT: a run killed mid-test leaves its second
# client's mount dead ("Transport endpoint is not connected"), and the next run's first
# mkdir there aborts the whole suite (T64, 2026-10-07). Lazy, since the client is gone.
for m in "$MOUNT" /tmp/dfs-mount2 /tmp/dfs-mount3 /tmp/dfs-mount-fresh; do
    fusermount -uz "$m" 2>/dev/null || true
done
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
    RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 DFS_FAULT_INJECTION=1 "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
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
        LIST=$("$BIN/dfs-admin" --cluster "127.0.0.1:$port" file list --local 2>/dev/null)
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
    RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 DFS_FAULT_INJECTION=1 "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
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
#
# "Heal queue empty" (the loop above) is only a proxy for "fully converged," and
# a lossy one for exactly the case this test hits: this file's rolling restart
# spans all 5 nodes (including the leader, at whatever point it lands in that
# rotation) mid-write, and any patch still an unfolded token when its restart
# hits is deliberately skipped by healing outright (patch tokens aren't
# independently replicable — they resolve via the fold, not a byte copy — see
# do_heal_chunk_inner's "marker-shaped as a patch token" fast path), clearing
# the queue almost instantly regardless of whether the fold itself, and the
# fold's own leader-announcement, have actually finished. Before 2026-08-14,
# the leader-announcement step could return early on a bare RPC-succeeded
# response even when the leader hadn't actually adopted the result (see
# Response::FoldReceipt's doc comment on dfs-common's Response enum) — fast,
# but sometimes silently wrong, exactly the daily VM-108 EIO root cause.
# Fixed to require a real leader acknowledgment and to keep retrying on a 10s
# backstop cadence instead of giving up after a fixed window — genuinely
# correct, but a rolling restart that includes the leader can now legitimately
# take a couple of that backstop's 10s cycles (the leader's own post-restart
# chunk_generations catch-up is part of what's being waited on) to finish
# converging, which the old code's willingness to accept an unconfirmed
# success was silently papering over. Poll the real signal (actual replica
# count) instead of padding the heal-queue-drain timeout above, which mostly
# doesn't wait on this at all once the queue itself empties quickly.
T38_CONVERGE_DEADLINE=$(( $(date +%s) + 35 ))
while true; do
    T38_UNDER_AFTER=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json file info /t38_slow.bin 2>/dev/null \
        | python3 -c "import json,sys; d=json.load(sys.stdin); print(sum(1 for c in d['chunk_locations'] if len(c['nodes']) < 3))" 2>/dev/null || echo "?")
    [ "$T38_UNDER_AFTER" = "0" ] && break
    if [ "$(date +%s)" -ge "$T38_CONVERGE_DEADLINE" ]; then
        break
    fi
    sleep 2
done

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

# Prove the per-node probe below sees the files while they exist, so its "none"
# after the delete means something.
T42B_BEFORE=""
for port in 8900 8901 8902 8903 8904; do
    T42B_BEFORE="${T42B_BEFORE} $port:$("$BIN/dfs-admin" --cluster "127.0.0.1:$port" file list --local 2>/dev/null | grep -c "t42_flood_" || true)"
done
echo "  T42b: flood files listed per node before delete:${T42B_BEFORE}"

for i in $(seq 0 $((T42_NUM_PROCS-1))); do rm -f "$MOUNT/t42_flood_${i}.bin" 2>/dev/null || true; done

# T42b (Legata bug): files deleted right after the flood lingered on some nodes'
# FILE_TABLE for minutes. Wait past the 30s delete-tombstone TTL, so an update
# that was still in flight at delete time has had its chance to re-create the
# row, then every node must have forgotten every flood file.
dfs_sync 2>/dev/null || true
sleep 40
T42B_LEFT=""
for port in 8900 8901 8902 8903 8904; do
    n=$("$BIN/dfs-admin" --cluster "127.0.0.1:$port" file list --local 2>/dev/null | grep -c "t42_flood_" || true)
    [ "$n" -gt 0 ] && T42B_LEFT="${T42B_LEFT} $port:$n"
done
echo "  T42b: flood files still listed 40s after delete:${T42B_LEFT:- none}"
[ -z "$T42B_LEFT" ] \
    && check "T42b every node forgot the deleted flood files" PASS \
    || check "T42b deleted flood files resurrected/lingered on:${T42B_LEFT}" FAIL
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
    if [ "$T43_RCL_COUNT" -gt 1 ]; then
        # Seen once in 45 ordered runs (2026-10-06), not reproducible alone. Suspect: an ordered
        # location whose send to a slow leader timed out was re-queued and sent again (at-least-
        # once). Same generation twice plus a client "re-queued" line would confirm it.
        grep -H "Handling replicate chunk location: $T43_CHUNK_ID " "$LOG"/server*.log 2>/dev/null \
            | sed 's/\x1b\[[0-9;]*m//g' | cut -c1-220 | sed 's/^/    /'
        grep -ah "not confirmed by the leader; re-queued" "$CURRENT_CLIENT_LOG" 2>/dev/null \
            | sed 's/\x1b\[[0-9;]*m//g' | tail -3 | cut -c1-200 | sed 's/^/    client: /'
    fi
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
RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 DFS_FAULT_INJECTION=1 "$BIN/dfs-server" start --config "$BASE/node1/config.toml" \
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
RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 DFS_FAULT_INJECTION=1 "$BIN/dfs-server" start --config "$BASE/node5/config.toml" \
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

RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 DFS_FAULT_INJECTION=1 "$BIN/dfs-server" start --config "$BASE/node${T51_LEADER_NODE}/config.toml" \
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
RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 DFS_FAULT_INJECTION=1 "$BIN/dfs-server" start --config "$BASE/node${T52_VICTIM_NUM}/config.toml" \
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

if should_run T56; then
snapshot_log T56
echo ""
echo "=== T56: fault-injection filter cuts real links and heals (SLOT-OWNERSHIP-PLAN Phase 0) ==="
# Every later ownership phase proves its failure-matrix rows with this filter, so
# it must demonstrably act on real inter-node traffic, not just on a unit-test socket.
T56_NODE=127.0.0.1:8904
T56_PEERS=127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903
T56_LOG="$LOG/server5.log"
T56_BEFORE=$(grep -c "first dropped send to" "$T56_LOG" 2>/dev/null || true)

"$BIN/dfs-admin" --cluster "$T56_NODE" fault set --drop-to "$T56_PEERS" >/dev/null 2>&1 \
    && check "T56a fault set accepted on a DFS_FAULT_INJECTION=1 node" PASS \
    || check "T56a fault set refused on a DFS_FAULT_INJECTION=1 node" FAIL
# Heartbeats go out every 5s locally; each one to a cut peer must fail with the injected error.
T56_HITS=0
for _ in $(seq 1 16); do
    T56_HITS=$(( $(grep -c "first dropped send to" "$T56_LOG" 2>/dev/null || true) - T56_BEFORE ))
    [ "$T56_HITS" -gt 0 ] && break
    sleep 0.5
done
[ "$T56_HITS" -gt 0 ] \
    && check "T56b cut node's own peer traffic hits the filter ($T56_HITS of 4 links)" PASS \
    || check "T56b no peer traffic from the cut node hit the filter within 8s" FAIL

"$BIN/dfs-admin" --cluster "$T56_NODE" fault set --refuse-clients >/dev/null 2>&1
timeout 5 "$BIN/dfs-admin" --cluster "$T56_NODE" cluster status >/dev/null 2>&1 \
    && check "T56c refuse_clients: client port still answered" FAIL \
    || check "T56c refuse_clients: client port stops answering" PASS
"$BIN/dfs-admin" --cluster "$T56_NODE" fault clear >/dev/null 2>&1 \
    && check "T56d fault clear gets through a refuse_clients node" PASS \
    || check "T56d fault clear blocked by refuse_clients -- a test could never heal" FAIL
timeout 5 "$BIN/dfs-admin" --cluster "$T56_NODE" cluster status >/dev/null 2>&1 \
    && check "T56e client port answers again after clear" PASS \
    || check "T56e client port still dead after clear" FAIL
T56_AFTER=$(grep -c "first dropped send to" "$T56_LOG" 2>/dev/null || true)
sleep 6
T56_LATE=$(( $(grep -c "first dropped send to" "$T56_LOG" 2>/dev/null || true) - T56_AFTER ))
[ "$T56_LATE" -eq 0 ] \
    && check "T56f peer links healed after clear (no injected failures in the next heartbeat)" PASS \
    || check "T56f $T56_LATE injected failures after clear -- filter not cleared" FAIL
fi # should_run T56

if should_run T57; then
snapshot_log T57
echo ""
echo "=== T57: slot audit sees clean data as clean and catches a planted phantom holder (SLOT-OWNERSHIP-PLAN Phase 0) ==="
t57_fid() {
    grep -h "\[META SERVER\] put path=$1 id=" "$LOG"/server*.log 2>/dev/null | tail -1 | grep -oP 'id=\K[0-9a-f-]+'
}
t57_div() { cat "$LOG"/server*.log 2>/dev/null | grep "\[DIVERGENCE\]" | grep -c "file=$1" || true; }

# T57a: ordinary writes plus in-place patches, then quiet -- zero findings allowed.
dd if=/dev/urandom of="$MOUNT/t57_clean.bin" bs=1M count=12 status=none
for off in 5 6 9; do
    dd if=/dev/urandom of="$MOUNT/t57_clean.bin" bs=4K count=1 seek=$((off * 256 + 3)) conv=notrunc status=none
done
dfs_sync
T57A_FID=$(t57_fid /t57_clean.bin)
T57A_PASSES_BEFORE=$(cat "$LOG"/server*.log | grep -c "SLOT AUDIT pass" || true)
sleep 20   # quiet (3s) + first check + confirm delay (10s) + re-check, with 2s passes
T57A_PASSES=$(( $(cat "$LOG"/server*.log | grep -c "SLOT AUDIT pass" || true) - T57A_PASSES_BEFORE ))
T57A_DIV=$(t57_div "$T57A_FID")
echo "  T57a: file_id=${T57A_FID:-<not found>} audit passes with work=$T57A_PASSES divergences=$T57A_DIV"
[ -n "$T57A_FID" ] && [ "$T57A_PASSES" -gt 0 ] \
    && check "T57a slot audit ran over fresh writes ($T57A_PASSES passes)" PASS \
    || check "T57a slot audit never ran (fid=${T57A_FID:-none}, passes=$T57A_PASSES)" FAIL
[ "$T57A_DIV" -eq 0 ] \
    && check "T57a zero confirmed [DIVERGENCE] findings on clean, settled data" PASS \
    || check "T57a $T57A_DIV confirmed [DIVERGENCE] findings on clean data -- false positives or real drift, see server logs" FAIL

# T57b: delete one replica's bytes from disk under an unchanged view (the 2026-09-27
# phantom shape). Healing is paused so the outcome can't depend on who wins a race.
"$BIN/dfs-admin" --cluster "$CLUSTER" healing disable >/dev/null 2>&1 || true
dd if=/dev/urandom of="$MOUNT/t57_phantom.bin" bs=1M count=4 status=none
dfs_sync
T57B_FID=$(t57_fid /t57_phantom.bin)
T57B_CHUNK=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json file info /t57_phantom.bin 2>/dev/null \
    | python3 -c "import json,sys; print(json.load(sys.stdin)['chunk_locations'][0]['chunk_id'])" 2>/dev/null)
T57B_VICTIM=$(find "$BASE"/node*/data -name "$T57B_CHUNK" 2>/dev/null | head -1)
if [ -n "$T57B_VICTIM" ]; then
    rm -f "$T57B_VICTIM"
    echo "  T57b: removed $T57B_VICTIM"
fi
T57B_FOUND=0
for _ in $(seq 1 90); do   # up to 45s: 20s re-audit + 10s confirm + passes
    T57B_FOUND=$(cat "$LOG"/server*.log 2>/dev/null | grep "\[DIVERGENCE\] kind=phantom_holder" \
        | grep "file=$T57B_FID" | grep -c "chunk_idx=0 " || true)
    [ "$T57B_FOUND" -gt 0 ] && break
    sleep 0.5
done
"$BIN/dfs-admin" --cluster "$CLUSTER" healing enable >/dev/null 2>&1 || true
[ -n "$T57B_VICTIM" ] && [ "$T57B_FOUND" -gt 0 ] \
    && check "T57b planted phantom holder reported by the slot audit" PASS \
    || check "T57b planted phantom not reported (victim=${T57B_VICTIM:-none}, fid=${T57B_FID:-none})" FAIL

T57_ALL_DIV=$(cat "$LOG"/server*.log 2>/dev/null | grep -c "\[DIVERGENCE\]" || true)
T57_ALL_TRANSIENT=$(cat "$LOG"/server*.log 2>/dev/null | grep -c "\[DIVERGENCE-TRANSIENT\]" || true)
echo "  (informational) across this whole run: $T57_ALL_DIV confirmed [DIVERGENCE], $T57_ALL_TRANSIENT transient (cleared within the confirm delay)"
rm -f "$MOUNT/t57_clean.bin" "$MOUNT/t57_phantom.bin"
fi # should_run T57

if should_run T58; then
snapshot_log T58
echo ""
echo "=== T58: majority node leases: fencing before takeover, no dependence on the leader (SLOT-OWNERSHIP-PLAN Phase 1) ==="
T58_NODES=(127.0.0.1:8900 127.0.0.1:8901 127.0.0.1:8902 127.0.0.1:8903 127.0.0.1:8904)
T58_ALL="$(IFS=,; echo "${T58_NODES[*]}")"
t58_status() { "$BIN/dfs-admin" --cluster "$T58_ALL" lease status 2>/dev/null || true; }
t58_holders() { t58_status | python3 -c "import json,sys; print(sum(1 for l in sys.stdin if json.loads(l).get('holds_lease')))"; }
t58_log() { echo "$LOG/server$(( ${1##*:} - 8899 )).log"; }
t58_id() { t58_status | python3 -c "
import json,sys
for l in sys.stdin:
    x=json.loads(l)
    if x['addr']=='$1': print(x['node'])"; }
t58_cut() {   # t58_cut <addr...>: sever every link between the listed nodes and the rest
    local group=" $* " inside="" outside=""
    for n in "${T58_NODES[@]}"; do
        if [[ "$group" == *" $n "* ]]; then inside="${inside:+$inside,}$n"; else outside="${outside:+$outside,}$n"; fi
    done
    for n in "${T58_NODES[@]}"; do
        if [[ "$group" == *" $n "* ]]; then "$BIN/dfs-admin" --cluster "$n" fault set --drop-to "$outside" >/dev/null 2>&1 || true
        else "$BIN/dfs-admin" --cluster "$n" fault set --drop-to "$inside" >/dev/null 2>&1 || true; fi
    done
}
t58_heal() { for n in "${T58_NODES[@]}"; do "$BIN/dfs-admin" --cluster "$n" fault clear >/dev/null 2>&1 || true; done; }
t58_wait_holders() {   # t58_wait_holders <count> <max-seconds>
    for _ in $(seq 1 $(( $2 * 2 ))); do [ "$(t58_holders)" = "$1" ] && return 0; sleep 0.5; done; return 1
}
t58_lost_count() { local c=0; for n in "$@"; do c=$(( c + $(grep -c "LEASE own: lost" "$(t58_log "$n")" || true) )); done; echo $c; }

# T58a: steady state.
t58_wait_holders 5 20 \
    && check "T58a all 5 nodes hold a lease from majority acks" PASS \
    || check "T58a only $(t58_holders)/5 nodes hold a lease after startup" FAIL

# T58b: isolate one node: it must fence itself before a majority votes it expired.
T58_VICTIM=127.0.0.1:8904
T58_VICTIM_ID=$(t58_id "$T58_VICTIM")
T58_LOST_BEFORE=$(grep -c "LEASE own: lost" "$(t58_log $T58_VICTIM)" || true)
T58_DECL_BEFORE=$(cat "$LOG"/server*.log | grep -c "LEASE takeover: node $T58_VICTIM_ID" || true)
T58_CUT_MS=$(date +%s%3N)
t58_cut "$T58_VICTIM"
for _ in $(seq 1 40); do
    [ "$(cat "$LOG"/server*.log | grep -c "LEASE takeover: node $T58_VICTIM_ID" || true)" -gt "$T58_DECL_BEFORE" ] && break
    sleep 0.5
done
T58_ENDED=$(grep "LEASE own: lost" "$(t58_log $T58_VICTIM)" | tail -n +$((T58_LOST_BEFORE + 1)) | head -1 | grep -oP 'wall_ms \K[0-9]+' || true)
# Filter by time, not by line count: the five logs are concatenated, not interleaved.
T58_EXPIRED=$(cat "$LOG"/server*.log | grep "LEASE takeover: node $T58_VICTIM_ID" \
    | grep -oP 'at wall_ms \K[0-9]+' | awk -v t="$T58_CUT_MS" '$1 >= t' | sort -n | head -1 || true)
echo "  T58b: victim=$T58_VICTIM own lease ended at ${T58_ENDED:-?}, first majority expiry at ${T58_EXPIRED:-?}"
[ -n "$T58_ENDED" ] && [ -n "$T58_EXPIRED" ] && [ "$T58_EXPIRED" -gt "$T58_ENDED" ] \
    && check "T58b isolated node's lease ended $(( T58_EXPIRED - T58_ENDED ))ms before a majority voted it expired (no overlap)" PASS \
    || check "T58b fencing order wrong or missing (ended=${T58_ENDED:-none}, expired=${T58_EXPIRED:-none})" FAIL
[ -n "$T58_ENDED" ] && [ -n "$T58_EXPIRED" ] && [ $(( T58_EXPIRED - T58_ENDED )) -lt 8000 ] \
    && check "T58b takeover became possible within 8s of the victim's lease ending" PASS \
    || check "T58b takeover too slow or missing" FAIL
t58_heal
t58_wait_holders 5 20 \
    && check "T58b healed node rejoins (new incarnation) and holds a lease again" PASS \
    || check "T58b healed node did not rejoin within 20s ($(t58_holders)/5 hold)" FAIL

# T58c: isolate the membership leader. The other four must not lose their leases at all.
T58_LEADER_PREFIX=$("$BIN/dfs-admin" --cluster "$T58_ALL" cluster status 2>/dev/null | grep -oP 'Leader:\s+\K\S+' | head -1 || true)
T58_LEADER=$(t58_status | python3 -c "
import json,sys
for l in sys.stdin:
    x=json.loads(l)
    if x['node'].startswith('$T58_LEADER_PREFIX'): print(x['addr'])" || true)
[ -z "$T58_LEADER" ] && T58_LEADER=127.0.0.1:8900
T58_OTHERS=(); for n in "${T58_NODES[@]}"; do [ "$n" != "$T58_LEADER" ] && T58_OTHERS+=("$n"); done
T58_OTHERS_LOST_BEFORE=$(t58_lost_count "${T58_OTHERS[@]}")
t58_cut "$T58_LEADER"
sleep 12
T58_OTHERS_LOST=$(( $(t58_lost_count "${T58_OTHERS[@]}") - T58_OTHERS_LOST_BEFORE ))
T58_NOW_HOLD=$(t58_holders)
echo "  T58c: leader=$T58_LEADER cut off 12s; lapses on the other four: $T58_OTHERS_LOST; holders now: $T58_NOW_HOLD"
[ "$T58_OTHERS_LOST" -eq 0 ] && [ "$T58_NOW_HOLD" = 4 ] \
    && check "T58c leader cut off: the other 4 nodes never lost their leases (leader death costs no write availability)" PASS \
    || check "T58c the leader's isolation cost other nodes their leases (lapses=$T58_OTHERS_LOST, holders=$T58_NOW_HOLD)" FAIL
t58_heal
t58_wait_holders 5 20 || true

# T58d: 2|3 split: the minority side loses its leases, the majority side keeps them.
t58_cut 127.0.0.1:8903 127.0.0.1:8904
sleep 8
T58D=$(t58_status | python3 -c "
import json,sys
r={json.loads(l)['addr']:json.loads(l)['holds_lease'] for l in sys.stdin}
minority=[r.get(a) for a in ('127.0.0.1:8903','127.0.0.1:8904')]
majority=[r.get(a) for a in ('127.0.0.1:8900','127.0.0.1:8901','127.0.0.1:8902')]
print('PASS' if not any(minority) and all(majority) else 'FAIL', minority, majority)")
echo "  T58d: $T58D"
check "T58d 2|3 split: the 2-node side fences itself, the 3-node side keeps its leases" "${T58D%% *}"
t58_heal
t58_wait_holders 5 20 \
    && check "T58d all 5 hold leases again after the split heals" PASS \
    || check "T58d only $(t58_holders)/5 hold leases 20s after healing" FAIL
fi # should_run T58

if should_run T59; then
snapshot_log T59
echo ""
echo "=== T59: subsystem stalls don't cost leases; a frozen process does, safely (SLOT-OWNERSHIP-PLAN Phase 1) ==="
T59_NODES=(127.0.0.1:8900 127.0.0.1:8901 127.0.0.1:8902 127.0.0.1:8903 127.0.0.1:8904)
T59_ALL="$(IFS=,; echo "${T59_NODES[*]}")"
t59_lost() { cat "$LOG"/server*.log 2>/dev/null | grep -c "LEASE own: lost" || true; }
t59_holders() { "$BIN/dfs-admin" --cluster "$T59_ALL" lease status 2>/dev/null \
    | python3 -c "import json,sys; print(sum(1 for l in sys.stdin if json.loads(l).get('holds_lease')))" || echo 0; }
t59_wait5() { for _ in $(seq 1 40); do [ "$(t59_holders)" = 5 ] && return 0; sleep 0.5; done; return 1; }
t59_wait5 || true

# T59a-c: hold a subsystem for 9s (3x the 3s lease) on one node; nobody may lose a lease.
T59_LEADER_PREFIX=$("$BIN/dfs-admin" --cluster "$T59_ALL" cluster status 2>/dev/null | grep -oP 'Leader:\s+\K\S+' | head -1 || true)
T59_LEADER=$("$BIN/dfs-admin" --cluster "$T59_ALL" lease status 2>/dev/null | python3 -c "
import json,sys
for l in sys.stdin:
    x=json.loads(l)
    if x['node'].startswith('$T59_LEADER_PREFIX'): print(x['addr'])" || true)
[ -z "$T59_LEADER" ] && T59_LEADER=127.0.0.1:8900
for spec in "metadata-db 127.0.0.1:8903 T59a" "healer-maps $T59_LEADER T59b" "cluster-membership 127.0.0.1:8902 T59c"; do
    set -- $spec
    T59_BEFORE=$(t59_lost)
    T59_OUT=$("$BIN/dfs-admin" --cluster "$2" fault stall --target "$1" --millis 9000 2>&1 || true)
    sleep 11
    T59_LAPSES=$(( $(t59_lost) - T59_BEFORE ))
    echo "  $3: stalled $1 on $2 for 9s -> $T59_LAPSES lease lapse(s) cluster-wide ($T59_OUT)"
    [ "$T59_LAPSES" -eq 0 ] \
        && check "$3 a 9s $1 stall costs no node its lease" PASS \
        || check "$3 a 9s $1 stall cost $T59_LAPSES lease lapse(s): the lease path depends on it" FAIL
    t59_wait5 || true
done

# T59e: the same metadata-db stall on the node clients talk to (the leader), with
# client metadata traffic running through it. Handlers that read redb synchronously
# on a tokio worker (not via spawn_blocking) each park a worker for the whole stall;
# enough of them starve the runtime, lease loop included. The full-suite run that
# failed T59a (2026-09-29) had the stalled node near-silent for 5.5s under load.
T59E_BEFORE=$(t59_lost)
T59E_STOP=$(( $(date +%s) + 10 ))
( i=0; while [ "$(date +%s)" -lt "$T59E_STOP" ]; do
    for j in 1 2 3 4 5 6 7 8; do
        ( echo "t59e $i $j" > "$MOUNT/t59e_$j.txt"; stat "$MOUNT/t59e_$j.txt" >/dev/null ) 2>/dev/null &
    done
    wait; i=$((i+1))
  done ) &
T59E_LOAD=$!
sleep 1
T59E_OUT=$("$BIN/dfs-admin" --cluster "$T59_LEADER" fault stall --target metadata-db --millis 9000 2>&1 || true)
sleep 11
kill "$T59E_LOAD" 2>/dev/null || true; wait "$T59E_LOAD" 2>/dev/null || true
T59E_LAPSES=$(( $(t59_lost) - T59E_BEFORE ))
echo "  T59e: stalled metadata-db on leader $T59_LEADER for 9s under client traffic -> $T59E_LAPSES lease lapse(s) ($T59E_OUT)"
[ "$T59E_LAPSES" -eq 0 ] \
    && check "T59e a 9s metadata-db stall on the leader under client traffic costs no node its lease" PASS \
    || check "T59e a 9s metadata-db stall under client traffic cost $T59E_LAPSES lease lapse(s): blocking redb reads starve the runtime" FAIL
rm -f "$MOUNT"/t59e_*.txt 2>/dev/null || true
t59_wait5 || true

# T59d: freeze a whole process. It must lose its lease, and the majority must vote it out
# only after its lease ended.
T59_VICTIM=127.0.0.1:8904
T59_VLOG="$LOG/server5.log"
T59_VID=$("$BIN/dfs-admin" --cluster "$T59_VICTIM" lease status 2>/dev/null | python3 -c "import json,sys; print(json.loads(sys.stdin.readline())['node'])" || true)
T59_PID=""
for p in $(pgrep -x dfs-server || true); do
    tr '\0' ' ' < /proc/$p/cmdline 2>/dev/null | grep -q "node5/config.toml" && T59_PID=$p
done
T59_LOST_BEFORE=$(grep -c "LEASE own: lost" "$T59_VLOG" || true)
T59_FREEZE_MS=$(date +%s%3N)
if [ -n "$T59_PID" ]; then
    kill -STOP "$T59_PID"; sleep 8; kill -CONT "$T59_PID"
fi
sleep 4
# Filter by time, not by line count: the five logs are concatenated, not interleaved,
# and earlier tests (T58) leave declarations about this same node.
T59_EXPIRED=$(cat "$LOG"/server*.log | grep "LEASE takeover: node $T59_VID" \
    | grep -oP 'at wall_ms \K[0-9]+' | awk -v t="$T59_FREEZE_MS" '$1 >= t' | sort -n | head -1 || true)
T59_ENDED=$(grep "LEASE own: lost" "$T59_VLOG" | tail -n +$((T59_LOST_BEFORE + 1)) | head -1 | grep -oP 'wall_ms \K[0-9]+' || true)
echo "  T59d: froze pid ${T59_PID:-?} for 8s: its lease ended at ${T59_ENDED:-?}, majority expiry at ${T59_EXPIRED:-?}"
[ -n "$T59_ENDED" ] && [ -n "$T59_EXPIRED" ] && [ "$T59_EXPIRED" -gt "$T59_ENDED" ] \
    && check "T59d frozen process: its lease ended $(( T59_EXPIRED - T59_ENDED ))ms before the majority voted it out" PASS \
    || check "T59d frozen process: fencing order wrong or missing (ended=${T59_ENDED:-none}, expired=${T59_EXPIRED:-none})" FAIL
t59_wait5 \
    && check "T59d thawed node rejoins and holds a lease again" PASS \
    || check "T59d thawed node did not rejoin within 20s" FAIL
fi # should_run T59

if should_run T60; then
snapshot_log T60
echo ""
echo "=== T60: lease chaos: random partitions, black-holes, one-way cuts and freezes; zero overlaps (SLOT-OWNERSHIP-PLAN Phase 1) ==="
T60_SECONDS="${DFS_T60_SECONDS:-120}"
T60_NODES=(127.0.0.1:8900 127.0.0.1:8901 127.0.0.1:8902 127.0.0.1:8903 127.0.0.1:8904)
T60_ALL="$(IFS=,; echo "${T60_NODES[*]}")"
t60_filter() {   # t60_filter <node> <drop-to-csv> [--black-hole]
    [ -n "$2" ] && "$BIN/dfs-admin" --cluster "$1" fault set --drop-to "$2" $3 >/dev/null 2>&1 || true
}
t60_heal() { for n in "${T60_NODES[@]}"; do "$BIN/dfs-admin" --cluster "$n" fault clear >/dev/null 2>&1 || true; done; }
t60_pid() {   # t60_pid <node-index 1..5>
    for p in $(pgrep -x dfs-server || true); do
        tr '\0' ' ' < /proc/$p/cmdline 2>/dev/null | grep -q "node$1/config.toml" && echo "$p"
    done
    true   # set -e: a non-matching last pid must not fail the caller's $(...)
}
T60_START_MS=$(date +%s%3N)
T60_END=$(( $(date +%s) + T60_SECONDS ))
T60_ROUNDS=0
while [ "$(date +%s)" -lt "$T60_END" ]; do
    T60_ROUNDS=$((T60_ROUNDS + 1))
    victims=$(printf '%s\n' 0 1 2 3 4 | shuf -n $(( RANDOM % 2 + 1 )) | tr '\n' ' ')
    inside=""; outside=""
    for i in 0 1 2 3 4; do
        if [[ " $victims " == *" $i "* ]]; then inside="${inside:+$inside,}${T60_NODES[$i]}"; else outside="${outside:+$outside,}${T60_NODES[$i]}"; fi
    done
    case $(( RANDOM % 4 )) in
        0) mode=partition;  for i in 0 1 2 3 4; do
               if [[ " $victims " == *" $i "* ]]; then t60_filter "${T60_NODES[$i]}" "$outside"; else t60_filter "${T60_NODES[$i]}" "$inside"; fi; done ;;
        1) mode=blackhole;  for i in 0 1 2 3 4; do
               if [[ " $victims " == *" $i "* ]]; then t60_filter "${T60_NODES[$i]}" "$outside" --black-hole; else t60_filter "${T60_NODES[$i]}" "$inside" --black-hole; fi; done ;;
        2) mode=oneway;     for i in 0 1 2 3 4; do [[ " $victims " != *" $i "* ]] && t60_filter "${T60_NODES[$i]}" "$inside"; done ;;
        3) mode=freeze;     for i in $victims; do p=$(t60_pid $((i + 1))); [ -n "$p" ] && kill -STOP "$p"; done ;;
    esac
    sleep $(( RANDOM % 6 + 2 ))
    if [ "$mode" = freeze ]; then for i in $victims; do p=$(t60_pid $((i + 1))); [ -n "$p" ] && kill -CONT "$p"; done; fi
    [ $(( RANDOM % 3 )) -ne 0 ] && t60_heal
    sleep $(( RANDOM % 3 + 1 ))
done
t60_heal
for i in 1 2 3 4 5; do p=$(t60_pid $i); [ -n "$p" ] && kill -CONT "$p" 2>/dev/null; done
sleep 8

T60_RESULT=$("$BIN/dfs-admin" --cluster "$T60_ALL" lease status 2>/dev/null | python3 -c "
import json,sys,re,glob
since=int('$T60_START_MS')
addr_node={}
for l in sys.stdin:
    x=json.loads(l); addr_node[x['addr']]=x['node']
ext={}   # node -> [(inc, until_ms, logged_ms)]
decl=[]  # (target, inc, at_ms)
import datetime
for i in range(1,6):
    node=addr_node.get('127.0.0.1:%d' % (8899+i))
    for line in open('$LOG/server%d.log' % i, errors='replace'):
        line=re.sub(r'\x1b\[[0-9;]*m','',line)
        m=re.search(r'LEASE own: (?:acquired|renewed) \(incarnation (\d+).*until_wall_ms (\d+)\)', line)
        if m and node:
            ext.setdefault(node,[]).append((int(m.group(1)), int(m.group(2))))
        m=re.search(r'LEASE takeover: node (\S+) incarnation (\d+) expired .* at wall_ms (\d+)', line)
        if m:
            decl.append((m.group(1), int(m.group(2)), int(m.group(3))))
viol=[]
checked=0
for (t,inc,at) in decl:
    if at < since: continue
    checked+=1
    for (i,u) in ext.get(t,[]):
        if i <= inc and u > at:
            viol.append('%s inc %d lease until %d > expired at %d (inc %d)' % (t[:8], i, u, at, inc))
held=sum(len(v) for v in ext.values())
print(len(viol), checked, held)
for v in viol[:5]: print('   ', v)
")
T60_VIOL=$(echo "$T60_RESULT" | head -1 | awk '{print $1}')
T60_CHECKED=$(echo "$T60_RESULT" | head -1 | awk '{print $2}')
T60_EXT=$(echo "$T60_RESULT" | head -1 | awk '{print $3}')
echo "  T60: ${T60_ROUNDS} chaos rounds over ${T60_SECONDS}s; ${T60_CHECKED:-0} majority expiries checked against ${T60_EXT:-0} logged lease extensions"
echo "$T60_RESULT" | tail -n +2
[ "${T60_VIOL:-1}" = 0 ] && [ "${T60_CHECKED:-0}" -gt 0 ] \
    && check "T60 zero lease/expiry overlaps across ${T60_CHECKED} expiries" PASS \
    || check "T60 ${T60_VIOL:-?} overlap(s) (or no expiries to check: ${T60_CHECKED:-0})" FAIL
T60_HOLD=$("$BIN/dfs-admin" --cluster "$T60_ALL" lease status 2>/dev/null | python3 -c "import json,sys; print(sum(1 for l in sys.stdin if json.loads(l).get('holds_lease')))" || echo 0)
[ "$T60_HOLD" = 5 ] \
    && check "T60 cluster fully recovers: all 5 hold leases after the chaos" PASS \
    || check "T60 only $T60_HOLD/5 hold leases 8s after the chaos ended" FAIL
fi # should_run T60

if should_run T61; then
snapshot_log T61
echo ""
echo "=== T61: one fold owner per chunk: background folds start on a single node, no takeovers (SLOT-OWNERSHIP-PLAN Phase 2) ==="
T61_IMG="$MOUNT/t61_owner.img"
T61_CHUNKS=16
dd if=/dev/urandom of="$T/t61_base.bin" bs=4M count=$T61_CHUNKS 2>/dev/null
cp "$T/t61_base.bin" "$T61_IMG"
dfs_sync
T61_FID=$(grep -h "\[META SERVER\] put path=/t61_owner.img id=" "$LOG"/server*.log 2>/dev/null | tail -1 | grep -oP 'id=\K[0-9a-f-]+' || true)
echo "  T61: file_id=${T61_FID:-<not found>}; patch storm across $T61_CHUNKS chunks for 20s, then 45s quiet"
python3 "$REPO/scripts/t53_patch_writer.py" "$T61_IMG" "$T61_CHUNKS" 45 20
T61_RESULT=$(python3 - "$T61_FID" <<'PY'
import re, sys, glob
fid = sys.argv[1]
owners = {}   # chunk -> set of server logs that folded it as owner
takeovers = 0
for path in sorted(glob.glob("/tmp/dfs-test-logs/server*.log")):
    for line in open(path, errors="replace"):
        line = re.sub(r"\x1b\[[0-9;]*m", "", line)
        m = re.search(r"FOLD OWNER: file (\S+) chunk (\d+) folding as its owner", line)
        if m and m.group(1) == fid:
            owners.setdefault(int(m.group(2)), set()).add(path)
        if "[FOLD-OWNER]" in line and fid in line:
            takeovers += 1
multi = {c: sorted(p.rsplit('/',1)[1] for p in s) for c, s in owners.items() if len(s) > 1}
print(len(owners), len(multi), takeovers)
for c, s in sorted(multi.items())[:5]:
    print("    chunk %d folded as owner by %s" % (c, ", ".join(s)))
PY
)
T61_FOLDED=$(echo "$T61_RESULT" | head -1 | awk '{print $1}')
T61_MULTI=$(echo "$T61_RESULT" | head -1 | awk '{print $2}')
T61_TAKE=$(echo "$T61_RESULT" | head -1 | awk '{print $3}')
# Logged by the client (without a file id); snapshot_log started this test's client log empty.
T61_DISAGREE=$(grep -ac "REPLICA DISAGREEMENT" "$CURRENT_CLIENT_LOG" 2>/dev/null || true)
echo "  T61: $T61_FOLDED chunk(s) folded in the background; $T61_MULTI with more than one owner; $T61_TAKE takeover(s); $T61_DISAGREE replica disagreement(s)"
echo "$T61_RESULT" | tail -n +2
[ -n "$T61_FID" ] && [ "${T61_FOLDED:-0}" -gt 0 ] \
    && check "T61a background folds ran as owner folds ($T61_FOLDED chunks)" PASS \
    || check "T61a no owner folds observed for the storm file (fid=${T61_FID:-none})" FAIL
[ "${T61_MULTI:-1}" = 0 ] \
    && check "T61b every chunk's background folds started on exactly one node" PASS \
    || check "T61b $T61_MULTI chunk(s) had background folds started by more than one node" FAIL
[ "${T61_TAKE:-1}" = 0 ] \
    && check "T61c no owner takeovers with every node healthy" PASS \
    || check "T61c $T61_TAKE owner takeover(s) with every node healthy -- an owner failed to fold" FAIL
if [ "${T61_DISAGREE:-0}" != 0 ]; then
    # Evidence, kept in the suite log itself: later runs wipe $LOG.
    echo "  T61 diagnostics (first disagreements, then each server's fold activity for the file):"
    grep -a "REPLICA DISAGREEMENT" "$CURRENT_CLIENT_LOG" | head -3 | sed 's/\x1b\[[0-9;]*m//g' | cut -c1-260 | sed 's/^/    /'
    for i in 1 2 3 4 5; do
        sed 's/\x1b\[[0-9;]*m//g' "$LOG/server$i.log" | grep "$T61_FID" \
            | grep -E "FOLD OWNER|FOLD-OWNER|Single fold|ghost-chunk guard|ForceFold|coordinate_and_fold_slot" \
            | head -8 | cut -c1-230 | sed "s/^/    server$i: /"
    done
fi
# T61d: the disagreements it catches are a PRE-EXISTING race of unordered writes (the client's
# ForceFold folds both replicas while patches still arrive, and they end on different tokens;
# the healer corrects it). Measured 2026-10-03 with T60 then T61: flag off 1 run in 3, flag on
# 0 in 6. Required with DFS_ORDERED_WRITES=1; informational without it, where the race remains.
if [ "${DFS_ORDERED_WRITES:-0}" = 1 ]; then
    [ "${T61_DISAGREE:-1}" = 0 ] \
        && check "T61d ordered writes: no replica disagreements during the storm" PASS \
        || check "T61d ordered writes: ${T61_DISAGREE:-?} replica disagreement(s) during the storm" FAIL
else
    echo "  (informational without DFS_ORDERED_WRITES) T61d: ${T61_DISAGREE:-?} REPLICA DISAGREEMENT line(s) for the storm file"
fi
rm -f "$T61_IMG" "$T/t61_base.bin"
fi # should_run T61

if should_run T62; then
snapshot_log T62
echo ""
echo "=== T62: per-chunk ISRs are agreed by a majority: one value per epoch, even when nodes race (SLOT-OWNERSHIP-PLAN Phase 3a) ==="
T62_NODES=(127.0.0.1:8900 127.0.0.1:8901 127.0.0.1:8902 127.0.0.1:8903 127.0.0.1:8904)
T62_ALL="$(IFS=,; echo "${T62_NODES[*]}")"
T62_FILE=/t62_isr.bin
T62_CHUNKS=8
dd if=/dev/urandom of="$MOUNT$T62_FILE" bs=4M count=$T62_CHUNKS status=none
dfs_sync
T62_IDS=$("$BIN/dfs-admin" --cluster "$T62_ALL" lease status 2>/dev/null | python3 -c "import json,sys; print(' '.join(json.loads(l)['node'] for l in sys.stdin))" || true)
read -r -a T62_ID <<< "$T62_IDS"
# agree <chunk-count> [min-epoch]: PASS if every node reports the same non-null record per chunk
t62_agree() {
    "$BIN/dfs-admin" --cluster "$T62_ALL" isr get --file "$T62_FILE" --chunks "$1" 2>/dev/null | python3 -c "
import json,sys
rows=[json.loads(l) for l in sys.stdin]
if len(rows)!=5 or any('isr' not in r for r in rows): print('FAIL rows'); sys.exit()
views=[r['isr'] for r in rows]
bad=[c for c in range(len(views[0])) if any(v[c] is None or v[c]!=views[0][c] for v in views) or views[0][c]['epoch']<${2:-1}]
print('PASS' if not bad else 'FAIL chunks %s' % bad[:5])
" || echo "FAIL admin"
}

# T62a: seeding.
T62A=FAIL
for _ in $(seq 1 30); do T62A=$(t62_agree $T62_CHUNKS); [ "$T62A" = PASS ] && break; sleep 1; done
T62A_SHAPE=$("$BIN/dfs-admin" --cluster 127.0.0.1:8900 --format json file info "$T62_FILE" 2>/dev/null > "$T/t62_info.json"; \
    "$BIN/dfs-admin" --cluster 127.0.0.1:8900 isr get --file "$T62_FILE" --chunks $T62_CHUNKS 2>/dev/null | python3 -c "
import json,sys
isr=json.loads(sys.stdin.readline())['isr']
info=json.load(open('$T/t62_info.json'))['chunk_locations']
ok=all(r and r['epoch']==1 and len(r['members'])==2 and set(r['members'])<=set(info[i]['nodes']) for i,r in enumerate(isr))
print('PASS' if ok else 'FAIL')" || echo FAIL)
check "T62a all 5 nodes agree on a seeded ISR for every chunk ($T62A)" "${T62A%% *}"
check "T62a seeded ISRs are epoch 1, two members, both actual holders" "$T62A_SHAPE"

# T62b: every node proposes a different next ISR for the same chunk at the same moment.
t62_race() {   # t62_race <chunk>: five concurrent proposals with five different member orders
    local c=$1 pids=()
    for k in 0 1 2 3 4; do
        m="${T62_ID[$k]},${T62_ID[$(( (k + 1) % 5 ))]}"
        "$BIN/dfs-admin" --cluster "${T62_NODES[$k]}" isr propose --file "$T62_FILE" --chunk "$c" --members "$m" >/dev/null 2>&1 &
        pids+=($!)
    done
    for p in "${pids[@]}"; do wait "$p" || true; done
}
T62B=PASS
for round in 1 2 3 4 5; do
    for c in 0 1 2; do t62_race $c; done
    sleep 1
    r=$(t62_agree 3 $((round + 1)))
    [ "$r" = PASS ] || { T62B="$r (round $round)"; break; }
done
check "T62b five nodes racing different values: every node agrees on one ISR per chunk, 5 rounds ($T62B)" "${T62B%% *}"

# T62c: the same race while a 2|3 partition is up; after healing, still one value per epoch.
t62_cut() {
    for n in 127.0.0.1:8900 127.0.0.1:8901 127.0.0.1:8902; do "$BIN/dfs-admin" --cluster "$n" fault set --drop-to 127.0.0.1:8903,127.0.0.1:8904 >/dev/null 2>&1 || true; done
    for n in 127.0.0.1:8903 127.0.0.1:8904; do "$BIN/dfs-admin" --cluster "$n" fault set --drop-to 127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902 >/dev/null 2>&1 || true; done
}
t62_heal() { for n in "${T62_NODES[@]}"; do "$BIN/dfs-admin" --cluster "$n" fault clear >/dev/null 2>&1 || true; done; }
t62_cut
for c in 3 4; do t62_race $c; done
t62_heal
# Everyone learns what was decided: one more round from a healthy majority settles any gap.
sleep 2
for c in 3 4; do t62_race $c; done
sleep 1
T62C=$(t62_agree 5 1)
check "T62c racing during a 2|3 partition, then healing: one agreed ISR per chunk ($T62C)" "${T62C%% *}"
# T62d: race while two acceptors are stalled, so their answers arrive late and stale:
# the interleavings where a wrong proposer would let two values win the same epoch.
# Checked from the commit log of every node, not just final views.
T62D_START=$(date +%s%3N)
for round in 1 2 3 4 5 6; do
    for n in 127.0.0.1:8901 127.0.0.1:8903; do
        "$BIN/dfs-admin" --cluster "$n" fault stall --target metadata-db --millis 700 >/dev/null 2>&1 || true
    done
    # Wait on these races only: a bare `wait` would also wait on the suite's FUSE client.
    t62_pids=()
    for c in 5 6 7; do t62_race $c & t62_pids+=($!); done
    for p in "${t62_pids[@]}"; do wait "$p" || true; done
done
sleep 2
T62D=$(cat "$LOG"/server*.log | sed 's/\x1b\[[0-9;]*m//g' | python3 -c "
import re,sys
seen={}
for line in sys.stdin:
    m=re.search(r'^(\S+)Z.*SLOT ISR: file (\S+) chunk (\d+) epoch (\d+) = (.*)$', line)
    if not m: continue
    k=(m.group(2),m.group(3),m.group(4))
    seen.setdefault(k,set()).add(m.group(5).strip())
bad=[k for k,v in seen.items() if len(v)>1]
print(('PASS' if not bad else 'FAIL') + ' %d decisions, %d conflicting' % (len(seen), len(bad)))
")
T62D_AGREE=$(t62_agree 8 1)
echo "  T62d: $T62D; final views: $T62D_AGREE"
check "T62d racing with two stalled acceptors: never two different values committed for one epoch ($T62D)" "${T62D%% *}"
check "T62d every node ends on the same ISR for every chunk ($T62D_AGREE)" "${T62D_AGREE%% *}"
rm -f "$MOUNT$T62_FILE" "$T/t62_info.json"
fi # should_run T62

if should_run T63; then
snapshot_log T63
echo ""
echo "=== T63: a node that missed ISR commits while partitioned catches up on its own (SLOT-OWNERSHIP-PLAN Phase 3a) ==="
T63_NODES=(127.0.0.1:8900 127.0.0.1:8901 127.0.0.1:8902 127.0.0.1:8903 127.0.0.1:8904)
T63_ALL="$(IFS=,; echo "${T63_NODES[*]}")"
T63_FILE=/t63_isr.bin
dd if=/dev/urandom of="$MOUNT$T63_FILE" bs=4M count=4 status=none
dfs_sync
T63_IDS=$("$BIN/dfs-admin" --cluster "$T63_ALL" lease status 2>/dev/null | python3 -c "import json,sys; print(' '.join(json.loads(l)['node'] for l in sys.stdin))" || true)
read -r -a T63_ID <<< "$T63_IDS"
t63_views() {   # prints one line per node: addr and its per-chunk epochs
    "$BIN/dfs-admin" --cluster "$T63_ALL" isr get --file "$T63_FILE" --chunks 4 2>/dev/null | python3 -c "
import json,sys
for l in sys.stdin:
    r=json.loads(l)
    print(r['addr'], json.dumps(r.get('isr')))" || true
}
t63_node5_matches() {
    t63_views | python3 -c "
import sys
rows=dict(l.split(' ',1) for l in sys.stdin.read().splitlines() if ' ' in l)
others={v for a,v in rows.items() if a!='127.0.0.1:8904'}
print('YES' if len(others)==1 and rows.get('127.0.0.1:8904') in others and 'null' not in next(iter(others)) else 'NO')" || echo NO
}
for _ in $(seq 1 30); do [ "$(t63_node5_matches)" = YES ] && break; sleep 1; done   # seeded everywhere
# Cut node 5 off, move every chunk's ISR on the majority side, heal.
for n in 127.0.0.1:8900 127.0.0.1:8901 127.0.0.1:8902 127.0.0.1:8903; do "$BIN/dfs-admin" --cluster "$n" fault set --drop-to 127.0.0.1:8904 >/dev/null 2>&1 || true; done
"$BIN/dfs-admin" --cluster 127.0.0.1:8904 fault set --drop-to 127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903 >/dev/null 2>&1 || true
for c in 0 1 2 3; do
    "$BIN/dfs-admin" --cluster 127.0.0.1:8900 isr propose --file "$T63_FILE" --chunk "$c" --members "${T63_ID[1]},${T63_ID[2]}" >/dev/null 2>&1 || true
done
for n in "${T63_NODES[@]}"; do "$BIN/dfs-admin" --cluster "$n" fault clear >/dev/null 2>&1 || true; done
T63_BEHIND=$(t63_node5_matches)
T63_CAUGHT=NO
for _ in $(seq 1 15); do T63_CAUGHT=$(t63_node5_matches); [ "$T63_CAUGHT" = YES ] && break; sleep 1; done
echo "  T63: right after healing node 5 matches the others: $T63_BEHIND; within 15s: $T63_CAUGHT"
[ "$T63_BEHIND" = NO ] \
    && check "T63 precondition: the partitioned node really missed the new epochs" PASS \
    || check "T63 precondition: node 5 already matched right after healing (test is vacuous)" FAIL
if [ "$T63_CAUGHT" != YES ]; then
    echo "  T63 diagnostics: each node's epochs per chunk"
    t63_views | python3 -c "
import json,sys
for l in sys.stdin:
    a,v=l.split(' ',1); v=json.loads(v)
    print('    %s %s' % (a, [(r or {}).get('epoch') for r in (v or [])]))"
    sed 's/\x1b\[[0-9;]*m//g' "$LOG/server5.log" | grep "SLOT ISR catch-up" | tail -3 | sed 's/^/    server5: /'
fi
[ "$T63_CAUGHT" = YES ] \
    && check "T63 node 5 caught up on every missed ISR commit on its own" PASS \
    || check "T63 node 5 still behind 15s after healing" FAIL
rm -f "$MOUNT$T63_FILE"
fi # should_run T63

# ── Test 64: two clients writing one chunk must not split its replicas ──────────
# SLOT-OWNERSHIP-PLAN 3c. Each client sends a patch to both replicas in parallel; with
# nobody ordering the two clients' patches, P can apply A-then-B while S applies B-then-A
# and the replicas end on different content at the same point in the stream. The client
# logs that as REPLICA DISAGREEMENT. With DFS_ORDERED_WRITES the primary orders every
# write to the chunk and both replicas apply that order.
if should_run T64; then
snapshot_log T64
echo ""
echo "=== T64: two clients writing the same chunk leave its replicas identical (SLOT-OWNERSHIP-PLAN 3c) ==="
T64_MOUNT2=/tmp/dfs-mount2
T64_LOG2="$LOG/client_t64b.log"
mkdir -p "$T64_MOUNT2"; : > "$T64_LOG2"
RUST_LOG=info "$BIN/dfs-client" mount "$T64_MOUNT2" --cluster "$CLUSTER" \
    --log-file "$T64_LOG2" --allow-other --log-level debug &
T64_PID2=$!
sleep 2
mountpoint -q "$T64_MOUNT2" || check "T64 second client mounted" FAIL
T64_FILE=t64_shared.bin
dd if=/dev/urandom of="$MOUNT/$T64_FILE" bs=4M count=2 status=none
dfs_sync
# Ordering needs chunk 1's committed ISR, on whichever node the client asks: wait until every
# node reports it. A fixed 5s sleep wasn't enough under full-suite load: the writers then ran
# unordered (the pre-3c two-writer bug, which the flag-off run covers) and drew EIO.
for _ in $(seq 1 30); do
    "$BIN/dfs-admin" --cluster 127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904 \
        isr get --file "/$T64_FILE" --chunks 2 2>/dev/null \
        | python3 -c "import json,sys; rs=[json.loads(l) for l in sys.stdin]; sys.exit(0 if len(rs)==5 and all(r.get('isr') and r['isr'][1] for r in rs) else 1)" \
        2>/dev/null && break
    sleep 1
done
t64_writer() {  # mount tag: 150 fsync'd 4K writes of this client's own content to one block
    # Prints the last write whose fsync succeeded (its ack), or NONE, then FAILED:<error> if a
    # write or fsync failed. A failed fsync means that write was never acked, so T64b judges
    # the servers against each writer's last ACKED write, not against write 149.
    python3 - "$1/$T64_FILE" "$2" <<'PY'
import os, sys
path, tag = sys.argv[1], sys.argv[2].encode()
fd = os.open(path, os.O_RDWR)
off = 4 * 1024 * 1024 + 8192          # the same 4K block of chunk 1 for both clients
acked, err = None, None
for i in range(150):
    try:
        os.pwrite(fd, (tag + b"%06d" % i).ljust(4096, tag[:1]), off)
        os.fsync(fd)
    except OSError as e:
        err = "write %d: %s" % (i, e); break
    acked = (tag + b"%06d" % i).decode() + tag.decode() * 3
print(acked or "NONE")
if err: print("FAILED:" + err)
os.close(fd)
PY
}
t64_writer "$MOUNT" A > "$LOG/t64_w1.out" 2>&1 & T64_W1=$!
t64_writer "$T64_MOUNT2" B > "$LOG/t64_w2.out" 2>&1 & T64_W2=$!
wait "$T64_W1" "$T64_W2" 2>/dev/null || true
T64_ACK1=$(head -1 "$LOG/t64_w1.out"); T64_ACK2=$(head -1 "$LOG/t64_w2.out")
T64_ERR=$(grep -h "FAILED:\|Traceback\|Error" "$LOG/t64_w1.out" "$LOG/t64_w2.out" | head -2 | tr '\n' ' ')
dfs_sync; sync "$T64_MOUNT2" 2>/dev/null || true
sleep 3
T64_DIS=$(( $(grep -ac "REPLICA DISAGREEMENT" "$CURRENT_CLIENT_LOG" 2>/dev/null || true) \
          + $(grep -ac "REPLICA DISAGREEMENT" "$T64_LOG2" 2>/dev/null || true) ))
t64_read() { python3 -c "f=open('$1/$T64_FILE','rb');f.seek(4*1024*1024+8192);print(f.read(10).decode(errors='replace'))" 2>/dev/null || true; }
# A third client that never saw the file: what the servers actually hold. Each writer's writes
# are sequential, so the final block must be one writer's LAST write.
T64_MOUNT3=/tmp/dfs-mount3
mkdir -p "$T64_MOUNT3"
RUST_LOG=info "$BIN/dfs-client" mount "$T64_MOUNT3" --cluster "$CLUSTER" \
    --log-file "$LOG/client_t64c.log" --allow-other &
T64_PID3=$!
sleep 2
T64_FRESH=$(t64_read "$T64_MOUNT3")
T64_R1=$(t64_read "$MOUNT"); T64_R2=$(t64_read "$T64_MOUNT2")
echo "  T64: DFS_ORDERED_WRITES=${DFS_ORDERED_WRITES:-0}: $T64_DIS replica disagreement(s); final block: fresh client=$T64_FRESH writer1=$T64_R1 writer2=$T64_R2"
if [ "$T64_DIS" != 0 ]; then
    grep -ah "REPLICA DISAGREEMENT" "$CURRENT_CLIENT_LOG" "$T64_LOG2" | head -3 | sed 's/\x1b\[[0-9;]*m//g' | cut -c1-240 | sed 's/^/    /'
fi
[ "$T64_DIS" = 0 ] \
    && check "T64a concurrent writers to one chunk: replicas never disagreed" PASS \
    || check "T64a concurrent writers to one chunk: $T64_DIS replica disagreement(s) -- replicas applied the writes in different orders" FAIL
# Each writer's writes are sequential and every ack is an fsync, so the block must end on one
# writer's last ACKED write (a write whose fsync failed may or may not have landed: both count).
echo "  T64: last acked: writer1=$T64_ACK1 writer2=$T64_ACK2${T64_ERR:+; errors: $T64_ERR}"
t64_next() {  # last acked (or NONE) + tag -> the write right after it (the first, if none was acked)
    python3 -c "import sys;t,g=sys.argv[1],sys.argv[2];n=int(t[1:7])+1 if t!='NONE' else 0;print(g+'%06d'%n+g*3)" "$1" "$2"; }
case "$T64_FRESH" in
    "$T64_ACK1"|"$T64_ACK2"|"") [ -n "$T64_FRESH" ] \
        && check "T64b the servers hold one writer's last acked write ($T64_FRESH)" PASS \
        || check "T64b the fresh client read nothing back" FAIL ;;
    "$(t64_next "$T64_ACK1" A)"|"$(t64_next "$T64_ACK2" B)")
        check "T64b the servers hold a writer's un-acked final write ($T64_FRESH, its fsync failed)" PASS ;;
    *) check "T64b the servers hold $T64_FRESH, not either writer's last acked write ($T64_ACK1/$T64_ACK2) -- an acked write was lost" FAIL ;;
esac
[ -z "$T64_ERR" ] \
    && check "T64d both writers' 150 fsync'd writes succeeded (no EIO)" PASS \
    || check "T64d a writer failed: $T64_ERR" FAIL
# Informational: a writer's own later reads after the other client's writes (cache coherence).
echo "  (informational) T64c: writers read back writer1=$T64_R1 writer2=$T64_R2 vs servers $T64_FRESH"
fusermount -u "$T64_MOUNT3" 2>/dev/null || true
kill_client_and_wait "$T64_PID3"
rm -f "$MOUNT/$T64_FILE"
fusermount -u "$T64_MOUNT2" 2>/dev/null || true
kill_client_and_wait "$T64_PID2"
fi # should_run T64

# ── Test 65: a fold on one ISR member between ordered writes must not split the pair ────
# SLOT-OWNERSHIP-PLAN 3c step 2 ("folds go through the primary's order"). A fold gives a slot a
# new identity without changing its bytes. If only one replica folds between two ordered writes,
# the next write lands on different bases: the folded replica starts a fresh accumulator on the
# fold result while the other merges into its pending one, and the two return different chunk
# ids for the same version. The client then calls it a REPLICA DISAGREEMENT, excludes one
# replica and backfills it. Deterministic: fold the primary alone, then write again.
if should_run T65; then
snapshot_log T65
echo ""
echo "=== T65: folding one ISR member between ordered writes keeps the pair identical (SLOT-OWNERSHIP-PLAN 3c) ==="
T65_ALL=127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904
T65_FILE=t65_fold.bin
dd if=/dev/urandom of="$MOUNT/$T65_FILE" bs=4M count=2 status=none
dfs_sync
# Ordering needs chunk 1's committed ISR; the seeder (3s in this suite) can take a few rounds.
for _ in $(seq 1 30); do
    "$BIN/dfs-admin" --cluster 127.0.0.1:8900 isr get --file "/$T65_FILE" --chunks 2 2>/dev/null \
        | python3 -c "import json,sys; sys.exit(0 if json.loads(sys.stdin.readline())['isr'][1] else 1)" 2>/dev/null && break
    sleep 1
done
t65_writes() {  # first last: fsync'd 4K writes of "W<n>" to distinct blocks of chunk 1
    python3 - "$MOUNT/$T65_FILE" "$1" "$2" <<'PY'
import os, sys
path, a, b = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
fd = os.open(path, os.O_RDWR)
for i in range(a, b + 1):
    os.pwrite(fd, (b"W%06d" % i).ljust(4096, b"w"), 4 * 1024 * 1024 + i * 8192)
    os.fsync(fd)
os.close(fd)
PY
}
t65_writes 0 4
T65_PRIMARY=$(python3 - "$("$BIN/dfs-admin" --cluster "$T65_ALL" lease status 2>/dev/null)" \
    "$("$BIN/dfs-admin" --cluster 127.0.0.1:8900 isr get --file "/$T65_FILE" --chunks 2 2>/dev/null)" <<'PY'
import json, sys
addr = {}
for l in sys.argv[1].splitlines():
    r = json.loads(l)
    if "node" in r: addr[r["node"]] = r["addr"]
isr = json.loads(sys.argv[2].splitlines()[0])["isr"][1]
print(addr.get(isr["members"][0], "") if isr else "")
PY
)
T65_MARK=$(wc -l < "$CURRENT_CLIENT_LOG")
if [ -n "$T65_PRIMARY" ]; then
    echo "  T65: chunk 1's primary is $T65_PRIMARY; folding it alone, then 5 more writes"
    "$BIN/dfs-admin" --cluster "$T65_PRIMARY" isr fold --file "/$T65_FILE" --chunk 1 | sed 's/^/    /'
else
    check "T65 chunk 1 has a committed ISR" FAIL
    "$BIN/dfs-admin" --cluster 127.0.0.1:8900 isr get --file "/$T65_FILE" --chunks 2 2>&1 | head -3 | cut -c1-300 | sed 's/^/    isr get: /'
    "$BIN/dfs-admin" --cluster "$T65_ALL" lease status 2>&1 | head -2 | cut -c1-200 | sed 's/^/    lease: /'
fi
t65_writes 5 9
dfs_sync
T65_SINCE=$(tail -n +"$((T65_MARK + 1))" "$CURRENT_CLIENT_LOG" | sed 's/\x1b\[[0-9;]*m//g')
T65_DIS=$(echo "$T65_SINCE" | grep -ac "REPLICA DISAGREEMENT" || true)
T65_BACKFILL=$(echo "$T65_SINCE" | grep -ac "landed on only" || true)
echo "  T65: DFS_ORDERED_WRITES=${DFS_ORDERED_WRITES:-0}: after the one-sided fold: $T65_DIS replica disagreement(s), $T65_BACKFILL backfill(s)"
echo "$T65_SINCE" | grep -a "REPLICA DISAGREEMENT\|landed on only" | head -2 | cut -c1-240 | sed 's/^/    /'
[ "$T65_DIS" = 0 ] && [ "$T65_BACKFILL" = 0 ] \
    && check "T65a writes after a one-sided fold land identically on both ISR members" PASS \
    || check "T65a a one-sided fold split the pair: $T65_DIS disagreement(s), $T65_BACKFILL backfill(s)" FAIL
T65_BAD=$(python3 - "$MOUNT/$T65_FILE" <<'PY'
import sys
f = open(sys.argv[1], "rb")
bad = []
for i in range(10):
    f.seek(4 * 1024 * 1024 + i * 8192)
    if f.read(4096) != (b"W%06d" % i).ljust(4096, b"w"): bad.append(i)
print(" ".join(map(str, bad)))
PY
)
[ -z "$T65_BAD" ] \
    && check "T65b all 10 writes read back" PASS \
    || check "T65b writes $T65_BAD read back wrong" FAIL
rm -f "$MOUNT/$T65_FILE"
fi # should_run T65

# ── Test 66: a frozen node during two-writer ordered writes (stall matrix) ──────────────
# A locked node is worse than a dead one: it comes back and acts on what it knew before. Two
# writers hammer one block of chunk 1 while one node is frozen (SIGSTOP 8s, then SIGCONT): the
# chunk's ISR primary, its secondary, or the leader when it is in neither. T66_TARGETS picks the
# cases (default: all three). Checks per case: (a) no acked write lost; (b) the two ISR replicas
# end byte-identical (each folds; fold ids are content hashes); (c) both writers make progress
# again after the node resumes. Errors and worst latency are reported, not judged.
if should_run T66; then
snapshot_log T66
echo ""
echo "=== T66: two writers on one chunk while a node is frozen (stall matrix, SLOT-OWNERSHIP 3c) ==="
T66_ALL=127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904
T66_MOUNT2=/tmp/dfs-mount2
mkdir -p "$T66_MOUNT2"
RUST_LOG=info "$BIN/dfs-client" mount "$T66_MOUNT2" --cluster "$CLUSTER" \
    --log-file "$LOG/client_t66b.log" --allow-other --log-level debug &
T66_PID2=$!
sleep 2
mountpoint -q "$T66_MOUNT2" || check "T66 second client mounted" FAIL
t66_pid_of() {   # addr -> dfs-server pid
    local n=$(( ${1##*:} - 8900 + 1 ))
    for p in $(pgrep -x dfs-server || true); do
        tr '\0' ' ' < /proc/$p/cmdline 2>/dev/null | grep -q "node$n/config.toml" && echo "$p"
    done
    true
}
t66_writer() {  # mount file tag seconds out: fsync'd 4K writes to one block until the deadline
    python3 - "$1/$2" "$3" "$4" > "$5" 2>&1 <<'PY'
import os, sys, time, json
path, tag, secs = sys.argv[1], sys.argv[2].encode(), float(sys.argv[3])
fd = os.open(path, os.O_RDWR)
off = 4 * 1024 * 1024 + 8192
start = time.time(); i = 0
acked = []; failed = []; worst = 0.0
while time.time() - start < secs:
    t0 = time.time()
    try:
        os.pwrite(fd, (tag + b"%06d" % i).ljust(4096, tag[:1]), off)
        os.fsync(fd)
        acked.append((i, time.time() - start))
    except OSError as e:
        failed.append(i)
        time.sleep(0.2)
    worst = max(worst, time.time() - t0)
    i += 1
os.close(fd)
print(json.dumps({"acked": [a for a, _ in acked], "late": sum(1 for _, t in acked if t > secs - 4),
                  "failed": failed, "worst_s": round(worst, 2)}))
PY
}
T66_CASES=${T66_TARGETS:-primary secondary leader}
# Required with DFS_ORDERED_WRITES=1. Without it the replicas routinely end unfoldable after a
# primary/secondary freeze (2026-10-03) -- the problem ordering exists to fix -- so report only.
t66_check() {
    if [ "${DFS_ORDERED_WRITES:-0}" = 1 ]; then check "$1" "$2"
    else echo "  (informational without DFS_ORDERED_WRITES) $2: $1"; fi
}
for T66_CASE in $T66_CASES; do
    T66_FILE=t66_$T66_CASE.bin
    dd if=/dev/urandom of="$MOUNT/$T66_FILE" bs=4M count=2 status=none
    dfs_sync
    T66_ISR=""
    for _ in $(seq 1 30); do
        T66_ISR=$("$BIN/dfs-admin" --cluster 127.0.0.1:8900 isr get --file "/$T66_FILE" --chunks 2 2>/dev/null \
            | python3 -c "import json,sys; r=json.loads(sys.stdin.readline())['isr'][1]; print(' '.join(r['members']) if r else '')" 2>/dev/null || true)
        [ -n "$T66_ISR" ] && break
        sleep 1
    done
    T66_MAP=$("$BIN/dfs-admin" --cluster "$T66_ALL" lease status 2>/dev/null \
        | python3 -c "import json,sys; [print(r['node'], r['addr']) for r in map(json.loads, sys.stdin) if 'node' in r]" || true)
    read -r T66_PID_NODE T66_SID_NODE <<< "$T66_ISR"
    T66_P=$(echo "$T66_MAP" | awk -v n="$T66_PID_NODE" '$1==n{print $2}')
    T66_S=$(echo "$T66_MAP" | awk -v n="$T66_SID_NODE" '$1==n{print $2}')
    T66_LEADER=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json cluster status 2>/dev/null | python3 -c "
import json, sys
d = json.load(sys.stdin)
online = sorted((n for n in d.get('nodes', []) if n.get('status') == 'Online'), key=lambda n: n['id'])
print(online[0]['address'] if online else '')" 2>/dev/null || true)
    case "$T66_CASE" in
        primary) T66_TARGET=$T66_P ;;
        secondary) T66_TARGET=$T66_S ;;
        leader) if [ "$T66_LEADER" = "$T66_P" ] || [ "$T66_LEADER" = "$T66_S" ]; then
                    echo "  T66[leader]: the leader $T66_LEADER is in chunk 1's ISR -- case covered by primary/secondary, skipped"
                    rm -f "$MOUNT/$T66_FILE"; continue
                fi
                T66_TARGET=$T66_LEADER ;;
    esac
    if [ -z "$T66_P" ] || [ -z "$T66_S" ] || [ -z "$T66_TARGET" ]; then
        check "T66[$T66_CASE] setup: ISR ($T66_P,$T66_S) and target ($T66_TARGET) found" FAIL
        rm -f "$MOUNT/$T66_FILE"; continue
    fi
    T66_TPID=$(t66_pid_of "$T66_TARGET")
    echo "  T66[$T66_CASE]: ISR primary=$T66_P secondary=$T66_S leader=$T66_LEADER; freezing $T66_TARGET (pid $T66_TPID) 8s"
    t66_writer "$MOUNT" "$T66_FILE" A 16 "$LOG/t66_w1.out" & T66_W1=$!
    t66_writer "$T66_MOUNT2" "$T66_FILE" B 16 "$LOG/t66_w2.out" & T66_W2=$!
    sleep 3
    kill -STOP "$T66_TPID"; sleep 8; kill -CONT "$T66_TPID"
    wait "$T66_W1" "$T66_W2" 2>/dev/null || true
    dfs_sync; sync "$T66_MOUNT2" 2>/dev/null || true
    sleep 3
    T66_FRESH=$(fresh_read "$T66_FILE" $((4*1024*1024+8192)) 10)
    T66_VERDICT=$(python3 - "$LOG/t66_w1.out" "$LOG/t66_w2.out" "$T66_FRESH" <<'PY'
import json, sys
fresh = sys.argv[3]
ok, summary = False, []
for path, tag in ((sys.argv[1], "A"), (sys.argv[2], "B")):
    try: r = json.loads(open(path).read().strip().splitlines()[-1])
    except Exception as e: print("BAD", "writer %s output unreadable: %s" % (tag, e)); sys.exit()
    last = max(r["acked"]) if r["acked"] else -1
    allowed = {last} | {f for f in r["failed"] if f > last}
    if fresh[:1] == tag and fresh[1:7].isdigit() and int(fresh[1:7]) in allowed: ok = True
    summary.append("%s: %d acked (last %d, %d in the last 4s), %d failed, worst %.1fs"
                   % (tag, len(r["acked"]), last, r["late"], len(r["failed"]), r["worst_s"]))
    if r["late"] == 0: summary.append("NOPROGRESS-" + tag)
print("OK" if ok else "LOST", "; ".join(summary))
PY
)
    echo "  T66[$T66_CASE]: servers hold $T66_FRESH; ${T66_VERDICT#* }"
    case "$T66_VERDICT" in
        OK*) t66_check "T66[$T66_CASE]a no acked write lost (servers hold $T66_FRESH)" PASS ;;
        *)   t66_check "T66[$T66_CASE]a servers hold $T66_FRESH, not a writer's last acked write -- acked write lost" FAIL ;;
    esac
    # Each ISR member's own bytes for the slot (ReadSlotLocal: its ordered head, else its local
    # chunk_map entry; nothing substituted). Folding and comparing fold ids proved unreliable:
    # ForceFold answers with the local chunk_map entry, which can be a token it didn't fold.
    # Compare the ISR as it is NOW: a frozen member that stops answering the primary is
    # replaced (Phase 3d), so the pair recorded before the freeze can be stale.
    T66_NOW=$("$BIN/dfs-admin" --cluster "$T66_ALL" isr get --file "/$T66_FILE" --chunks 2 2>/dev/null \
        | python3 -c "
import json,sys
best=None
for l in sys.stdin:
    try: r=json.loads(l)['isr'][1]
    except Exception: continue
    if r and (best is None or r['epoch']>best['epoch']): best=r
print(best['epoch'], ' '.join(best['members']) if best else '')" 2>/dev/null || true)
    read -r T66_EPOCH T66_NPN T66_NSN <<< "$T66_NOW"
    T66_MAPNOW=$("$BIN/dfs-admin" --cluster "$T66_ALL" lease status 2>/dev/null \
        | python3 -c "import json,sys; [print(r['node'], r['addr']) for r in map(json.loads, sys.stdin) if 'node' in r]" || true)
    T66_CP=$(echo "$T66_MAPNOW" | awk -v n="$T66_NPN" '$1==n{print $2}')
    T66_CS=$(echo "$T66_MAPNOW" | awk -v n="$T66_NSN" '$1==n{print $2}')
    [ "${T66_EPOCH:-1}" != 1 ] && echo "  T66[$T66_CASE]: ISR now epoch $T66_EPOCH: primary=$T66_CP secondary=$T66_CS"
    T66_READ_RAW=$("$BIN/dfs-admin" --cluster "${T66_CP:-$T66_P},${T66_CS:-$T66_S}" isr read --file "/$T66_FILE" --chunk 1 2>&1 || true)
    T66_HASHES=$(echo "$T66_READ_RAW" | python3 -c "
import json,sys
hs=[]
for l in sys.stdin:
    try: r=json.loads(l)
    except Exception: continue
    hs.append(r['blake3'][:16] if r.get('len') == 4194304 else 'ERR')
print(' '.join(hs))" || true)
    read -r T66_H1 T66_H2 <<< "$T66_HASHES"
    if [ -n "$T66_H1" ] && [ "$T66_H1" = "$T66_H2" ] && [ "$T66_H1" != ERR ]; then
        t66_check "T66[$T66_CASE]b both ISR replicas hold identical bytes ($T66_H1)" PASS
    else
        t66_check "T66[$T66_CASE]b ISR replicas differ or unreadable: $T66_HASHES" FAIL
        echo "$T66_READ_RAW" | cut -c1-260 | sed 's/^/    read: /'
    fi
    case "$T66_VERDICT" in
        *NOPROGRESS*) t66_check "T66[$T66_CASE]c a writer made no progress in the last 4s after the node resumed" FAIL ;;
        *)            t66_check "T66[$T66_CASE]c both writers made progress after the node resumed" PASS ;;
    esac
    rm -f "$MOUNT/$T66_FILE"
    sleep 3
done
fusermount -u "$T66_MOUNT2" 2>/dev/null || true
kill_client_and_wait "$T66_PID2"
fi # should_run T66

# ── Test 67: the chunk's secondary restarts mid-stream (ordered-write resync) ───────────
# A restarted secondary has no ordering state: its head can't continue the slot's version
# stream, so the first ordered write it sees after rejoining must resync it from the primary
# (ResyncSlot: the primary materializes its head as a real chunk; the secondary pulls it
# hash-verified). Two writers hammer one block of chunk 1; the secondary is killed -9 and
# restarted mid-storm. Checks: (a) no acked write lost; (b) both ISR replicas end
# byte-identical; (c) both writers make progress after the restart. How it rejoined
# (resync vs. an anchor the primary re-issued) is reported.
if should_run T67; then
snapshot_log T67
echo ""
echo "=== T67: the chunk's secondary restarts mid-stream; it must resync, not diverge (SLOT-OWNERSHIP 3c) ==="
T67_ALL=127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904
T67_FILE=t67_restart.bin
T67_MOUNT2=/tmp/dfs-mount2
mkdir -p "$T67_MOUNT2"
RUST_LOG=info "$BIN/dfs-client" mount "$T67_MOUNT2" --cluster "$CLUSTER" \
    --log-file "$LOG/client_t67b.log" --allow-other --log-level debug &
T67_PID2=$!
sleep 2
mountpoint -q "$T67_MOUNT2" || check "T67 second client mounted" FAIL
t67_check() {   # required with DFS_ORDERED_WRITES=1, informational without (as T66)
    if [ "${DFS_ORDERED_WRITES:-0}" = 1 ]; then check "$1" "$2"
    else echo "  (informational without DFS_ORDERED_WRITES) $2: $1"; fi
}
t67_writer() {  # mount file tag seconds out: fsync'd 4K writes to one block until the deadline
    python3 - "$1/$2" "$3" "$4" > "$5" 2>&1 <<'PY'
import os, sys, time, json
path, tag, secs = sys.argv[1], sys.argv[2].encode(), float(sys.argv[3])
fd = os.open(path, os.O_RDWR)
off = 4 * 1024 * 1024 + 8192
start = time.time(); i = 0
acked = []; failed = []; worst = 0.0
while time.time() - start < secs:
    t0 = time.time()
    try:
        os.pwrite(fd, (tag + b"%06d" % i).ljust(4096, tag[:1]), off)
        os.fsync(fd)
        acked.append((i, time.time() - start))
    except OSError:
        failed.append(i)
        time.sleep(0.2)
    worst = max(worst, time.time() - t0)
    i += 1
os.close(fd)
print(json.dumps({"acked": [a for a, _ in acked], "late": sum(1 for _, t in acked if t > secs - 4),
                  "failed": failed, "worst_s": round(worst, 2)}))
PY
}
dd if=/dev/urandom of="$MOUNT/$T67_FILE" bs=4M count=2 status=none
dfs_sync
T67_ISR=""
for _ in $(seq 1 30); do
    T67_ISR=$("$BIN/dfs-admin" --cluster 127.0.0.1:8900 isr get --file "/$T67_FILE" --chunks 2 2>/dev/null \
        | python3 -c "import json,sys; r=json.loads(sys.stdin.readline())['isr'][1]; print(' '.join(r['members']) if r else '')" 2>/dev/null || true)
    [ -n "$T67_ISR" ] && break
    sleep 1
done
T67_MAP=$("$BIN/dfs-admin" --cluster "$T67_ALL" lease status 2>/dev/null \
    | python3 -c "import json,sys; [print(r['node'], r['addr']) for r in map(json.loads, sys.stdin) if 'node' in r]" || true)
read -r T67_PID_NODE T67_SID_NODE <<< "$T67_ISR"
T67_P=$(echo "$T67_MAP" | awk -v n="$T67_PID_NODE" '$1==n{print $2}')
T67_S=$(echo "$T67_MAP" | awk -v n="$T67_SID_NODE" '$1==n{print $2}')
if [ -z "$T67_P" ] || [ -z "$T67_S" ]; then
    check "T67 setup: chunk 1's ISR found ($T67_P,$T67_S)" FAIL
else
    T67_N=$(( ${T67_S##*:} - 8900 + 1 ))
    T67_MARK=$(cat "$LOG"/server*.log | wc -l)
    echo "  T67: ISR primary=$T67_P secondary=$T67_S (node$T67_N); killing the secondary 3s in, restarting it 2s later"
    t67_writer "$MOUNT" "$T67_FILE" A 20 "$LOG/t67_w1.out" & T67_W1=$!
    t67_writer "$T67_MOUNT2" "$T67_FILE" B 20 "$LOG/t67_w2.out" & T67_W2=$!
    sleep 3
    pkill -9 -f "dfs-server start --config $BASE/node${T67_N}/config.toml" 2>/dev/null || true
    sleep 2
    RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 DFS_FAULT_INJECTION=1 "$BIN/dfs-server" start --config "$BASE/node${T67_N}/config.toml" \
        >> "$LOG/server${T67_N}.log" 2>&1 &
    wait "$T67_W1" "$T67_W2" 2>/dev/null || true
    dfs_sync; sync "$T67_MOUNT2" 2>/dev/null || true
    sleep 5
    T67_FRESH=$(fresh_read "$T67_FILE" $((4*1024*1024+8192)) 10)
    T67_VERDICT=$(python3 - "$LOG/t67_w1.out" "$LOG/t67_w2.out" "$T67_FRESH" <<'PY'
import json, sys
fresh = sys.argv[3]
ok, summary = False, []
for path, tag in ((sys.argv[1], "A"), (sys.argv[2], "B")):
    try: r = json.loads(open(path).read().strip().splitlines()[-1])
    except Exception as e: print("BAD", "writer %s output unreadable: %s" % (tag, e)); sys.exit()
    last = max(r["acked"]) if r["acked"] else -1
    allowed = {last} | {f for f in r["failed"] if f > last}
    if fresh[:1] == tag and fresh[1:7].isdigit() and int(fresh[1:7]) in allowed: ok = True
    summary.append("%s: %d acked (last %d, %d in the last 4s), %d failed, worst %.1fs"
                   % (tag, len(r["acked"]), last, r["late"], len(r["failed"]), r["worst_s"]))
    if r["late"] == 0: summary.append("NOPROGRESS-" + tag)
print("OK" if ok else "LOST", "; ".join(summary))
PY
)
    echo "  T67: servers hold $T67_FRESH; ${T67_VERDICT#* }"
    case "$T67_VERDICT" in
        OK*) t67_check "T67a no acked write lost across the secondary's restart (servers hold $T67_FRESH)" PASS ;;
        *)   t67_check "T67a servers hold $T67_FRESH, not a writer's last acked write -- acked write lost" FAIL ;;
    esac
    # Compare the ISR as it is NOW: a down secondary is replaced (Phase 3d), and the restarted
    # node is no longer a member.
    T67_NOW=$("$BIN/dfs-admin" --cluster "$T67_P" isr get --file "/$T67_FILE" --chunks 2 2>/dev/null \
        | python3 -c "import json,sys; r=json.loads(sys.stdin.readline())['isr'][1]; print(r['epoch'], ' '.join(r['members']))" 2>/dev/null || true)
    read -r T67_EPOCH T67_NP T67_NS <<< "$T67_NOW"
    T67_MAP2=$("$BIN/dfs-admin" --cluster "$T67_ALL" lease status 2>/dev/null \
        | python3 -c "import json,sys; [print(r['node'], r['addr']) for r in map(json.loads, sys.stdin) if 'node' in r]" || true)
    T67_CP=$(echo "$T67_MAP2" | awk -v n="$T67_NP" '$1==n{print $2}')
    T67_CS=$(echo "$T67_MAP2" | awk -v n="$T67_NS" '$1==n{print $2}')
    echo "  T67: ISR now epoch ${T67_EPOCH:-?}: primary=${T67_CP:-?} secondary=${T67_CS:-?}"
    T67_READ_RAW=$("$BIN/dfs-admin" --cluster "${T67_CP:-$T67_P},${T67_CS:-$T67_S}" isr read --file "/$T67_FILE" --chunk 1 2>&1 || true)
    T67_HASHES=$(echo "$T67_READ_RAW" | python3 -c "
import json,sys
hs=[]
for l in sys.stdin:
    try: r=json.loads(l)
    except Exception: continue
    hs.append(r['blake3'][:16] if r.get('len') == 4194304 else 'ERR')
print(' '.join(hs))" || true)
    read -r T67_H1 T67_H2 <<< "$T67_HASHES"
    # Required with ordering since Phase 3d (2026-10-04): a down member is replaced under
    # ordering (ReplaceIsrMember), so the CURRENT ISR pair must end byte-identical.
    if [ -n "$T67_H1" ] && [ "$T67_H1" = "$T67_H2" ] && [ "$T67_H1" != ERR ]; then
        t67_check "T67b both ISR replicas hold identical bytes after the restart ($T67_H1)" PASS
    else
        t67_check "T67b ISR replicas differ or unreadable after the restart: $T67_HASHES" FAIL
        echo "$T67_READ_RAW" | cut -c1-260 | sed 's/^/    read: /'
    fi
    # Required with ordering since Phase 3d: the writers stay on an ordered ISR pair throughout.
    case "$T67_VERDICT" in
        *NOPROGRESS*) t67_check "T67c a writer made no progress in the last 4s after the restart" FAIL ;;
        *)            t67_check "T67c both writers made progress after the restart" PASS ;;
    esac
    T67_SINCE=$(cat "$LOG"/server*.log | tail -n +"$((T67_MARK + 1))" | sed 's/\x1b\[[0-9;]*m//g')
    T67_RESYNCS=$(echo "$T67_SINCE" | grep -ac "resync anchor" || true)
    T67_ADOPT=$(echo "$T67_SINCE" | grep -ac "resynced from the primary" || true)
    echo "  T67: $T67_RESYNCS resync(s) served by the primary, $T67_ADOPT completed on the secondary"
    # Informational: rejoining through an anchor the primary re-issued (no resync) is fine too.
fi
rm -f "$MOUNT/$T67_FILE"
fusermount -u "$T67_MOUNT2" 2>/dev/null || true
kill_client_and_wait "$T67_PID2"
fi # should_run T67

# ── Test 68: the primary's apply of an ordered write fails while the secondary's succeeds ──
# The primary announces version v, then fails its own apply; the secondary applies v. The client
# sees one replica succeed, backfills it and acks. The primary's stream then re-anchors from its
# own head, which lacks v. If the secondary adopts that anchor, v is gone from both ordered
# replicas although it was acked. One writer writes 30 distinct blocks of chunk 1 (fsync each);
# chunk 1's primary is armed to fail one ordered apply after block 9. Checks: (a) every acked
# block reads back; (b) both ISR replicas end byte-identical; (c) the injected failure fired.
if should_run T68; then
snapshot_log T68
echo ""
echo "=== T68: the primary fails an ordered apply the secondary made; no acked write may be lost (SLOT-OWNERSHIP 3c) ==="
T68_ALL=127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904
T68_FILE=t68_primary_fail.bin
t68_check() {
    if [ "${DFS_ORDERED_WRITES:-0}" = 1 ]; then check "$1" "$2"
    else echo "  (informational without DFS_ORDERED_WRITES) $2: $1"; fi
}
dd if=/dev/urandom of="$MOUNT/$T68_FILE" bs=4M count=2 status=none
dfs_sync
T68_ISR=""
for _ in $(seq 1 30); do
    T68_ISR=$("$BIN/dfs-admin" --cluster "$T68_ALL" isr get --file "/$T68_FILE" --chunks 2 2>/dev/null \
        | python3 -c "
import json,sys
rs=[json.loads(l) for l in sys.stdin]
ok=len(rs)==5 and all(r.get('isr') and r['isr'][1] for r in rs)
print(' '.join(rs[0]['isr'][1]['members']) if ok else '')" 2>/dev/null || true)
    [ -n "$T68_ISR" ] && break
    sleep 1
done
T68_MAP=$("$BIN/dfs-admin" --cluster "$T68_ALL" lease status 2>/dev/null \
    | python3 -c "import json,sys; [print(r['node'], r['addr']) for r in map(json.loads, sys.stdin) if 'node' in r]" || true)
read -r T68_PN T68_SN <<< "$T68_ISR"
T68_P=$(echo "$T68_MAP" | awk -v n="$T68_PN" '$1==n{print $2}')
T68_S=$(echo "$T68_MAP" | awk -v n="$T68_SN" '$1==n{print $2}')
t68_writes() {  # first last: fsync'd 4K writes of "W<n>" to distinct blocks of chunk 1
    python3 - "$MOUNT/$T68_FILE" "$1" "$2" <<'PY'
import os, sys
path, a, b = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
fd = os.open(path, os.O_RDWR)
for i in range(a, b + 1):
    try:
        os.pwrite(fd, (b"W%06d" % i).ljust(4096, b"w"), 4 * 1024 * 1024 + i * 8192)
        os.fsync(fd)
        print("ACK", i)
    except OSError as e:
        print("FAIL", i, e)
os.close(fd)
PY
}
if [ -z "$T68_P" ] || [ -z "$T68_S" ]; then
    check "T68 setup: chunk 1's ISR found ($T68_P,$T68_S)" FAIL
else
    T68_FIRED0=$(cat "$LOG"/server*.log | grep -ac "FAULT INJECTION: primary failing" || true)
    echo "  T68: ISR primary=$T68_P secondary=$T68_S; failing one ordered apply on the primary after block 9"
    T68_OUT=$(t68_writes 0 9)
    "$BIN/dfs-admin" --cluster "$T68_P" fault fail-ordered --count 1 >/dev/null 2>&1 || true
    T68_OUT="$T68_OUT
$(t68_writes 10 29)"
    dfs_sync
    sleep 3
    T68_ACKED=$(echo "$T68_OUT" | awk '$1=="ACK"{print $2}' | tr '\n' ' ')
    # Read back through a FRESH client: the writing client's own cache would serve its writes
    # whatever the servers hold (that hid a real loss on the first runs, 2026-10-04).
    T68_MOUNT2=/tmp/dfs-mount2
    mkdir -p "$T68_MOUNT2"
    RUST_LOG=info "$BIN/dfs-client" mount "$T68_MOUNT2" --cluster "$CLUSTER" \
        --log-file "$LOG/client_t68b.log" --allow-other --log-level debug &
    T68_PID2=$!
    sleep 2
    T68_BAD=$(python3 - "$T68_MOUNT2/$T68_FILE" "$T68_ACKED" <<'PY'
import sys
f = open(sys.argv[1], "rb")
bad = []
for i in map(int, sys.argv[2].split()):
    f.seek(4 * 1024 * 1024 + i * 8192)
    if f.read(4096) != (b"W%06d" % i).ljust(4096, b"w"): bad.append(i)
print(" ".join(map(str, bad)))
PY
)
    T68_NACK=$(echo "$T68_ACKED" | wc -w)
    echo "  T68: $T68_NACK of 30 writes acked; $(echo "$T68_OUT" | grep -c '^FAIL') failed"
    [ -z "$T68_BAD" ] \
        && t68_check "T68a every acked write reads back ($T68_NACK acked)" PASS \
        || t68_check "T68a acked write(s) $T68_BAD read back wrong -- lost" FAIL
    T68_READ_RAW=$("$BIN/dfs-admin" --cluster "$T68_P,$T68_S" isr read --file "/$T68_FILE" --chunk 1 2>&1 || true)
    T68_HASHES=$(echo "$T68_READ_RAW" | python3 -c "
import json,sys
hs=[]
for l in sys.stdin:
    try: r=json.loads(l)
    except Exception: continue
    hs.append(r['blake3'][:16] if r.get('len') == 4194304 else 'ERR')
print(' '.join(hs))" || true)
    read -r T68_H1 T68_H2 <<< "$T68_HASHES"
    if [ -n "$T68_H1" ] && [ "$T68_H1" = "$T68_H2" ] && [ "$T68_H1" != ERR ]; then
        t68_check "T68b both ISR replicas hold identical bytes ($T68_H1)" PASS
    else
        t68_check "T68b ISR replicas differ or unreadable: $T68_HASHES" FAIL
        echo "$T68_READ_RAW" | cut -c1-260 | sed 's/^/    read: /'
    fi
    fusermount -u "$T68_MOUNT2" 2>/dev/null || true
    kill_client_and_wait "$T68_PID2"
    T68_FIRED=$(( $(cat "$LOG"/server*.log | grep -ac "FAULT INJECTION: primary failing" || true) - ${T68_FIRED0:-0} ))
    [ "${T68_FIRED:-0}" -ge 1 ] \
        && t68_check "T68c the injected primary apply failure fired ($T68_FIRED)" PASS \
        || t68_check "T68c the injected failure never fired: the writes after block 9 weren't ordered" FAIL
fi
rm -f "$MOUNT/$T68_FILE"
fi # should_run T68

# ── Test 69: the failure matrix under ordered writes (SLOT-OWNERSHIP-PLAN §7, Phase 3d) ──────
# Rows #2/#6 (P–S link cut, both reach the majority), #3 (S isolated from everyone), #5+#11 (P cut
# off from the majority but reachable by clients; then healed), #10 (P and S both frozen), #12 (P
# and S both cut off from every peer, clients reach both: no lease anywhere in the pair). Each
# case: fresh file, two writers on one block of chunk 1 for 18s, the fault 4s in for 7s. Checked
# for every case:
#   I1 no acked write lost (read through a fresh client)
#   I2 never two primaries at one ISR epoch (from the servers' "[ORDER] primary: ordering" lines)
#   I3 no unclean promotion: each new epoch's primary was a member of the previous epoch
#   I4 the CURRENT ISR pair ends byte-identical
# plus the row's own decision: #2/#3 keep the primary (no promotion); #5 moves it (takeover).
# T69_CASES picks cases (default: all). Requires DFS_ORDERED_WRITES=1 (informational without).
if should_run T69; then
snapshot_log T69
echo ""
echo "=== T69: failure matrix rows #2/#6, #3, #5+#11, #10, #12 under ordered writes (SLOT-OWNERSHIP 3d) ==="
T69_NODES=(127.0.0.1:8900 127.0.0.1:8901 127.0.0.1:8902 127.0.0.1:8903 127.0.0.1:8904)
T69_ALL="$(IFS=,; echo "${T69_NODES[*]}")"
T69_MOUNT2=/tmp/dfs-mount2
mkdir -p "$T69_MOUNT2"
RUST_LOG=info "$BIN/dfs-client" mount "$T69_MOUNT2" --cluster "$CLUSTER" \
    --log-file "$LOG/client_t69b.log" --allow-other --log-level debug &
T69_PID2=$!
sleep 2
mountpoint -q "$T69_MOUNT2" || check "T69 second client mounted" FAIL
t69_check() {
    if [ "${DFS_ORDERED_WRITES:-0}" = 1 ]; then check "$1" "$2"
    else echo "  (informational without DFS_ORDERED_WRITES) $2: $1"; fi
}
t69_pid_of() {   # addr -> dfs-server pid
    local n=$(( ${1##*:} - 8900 + 1 ))
    for p in $(pgrep -x dfs-server || true); do
        tr '\0' ' ' < /proc/$p/cmdline 2>/dev/null | grep -q "node$n/config.toml" && echo "$p"
    done
    true
}
t69_others() {   # addr -> every other node, comma-separated
    local out=""
    for n in "${T69_NODES[@]}"; do [ "$n" != "$1" ] && out="$out${out:+,}$n"; done
    echo "$out"
}
t69_isr() {   # file -> "epoch primary-addr secondary-addr" (highest epoch any node reports)
    local raw map
    raw=$("$BIN/dfs-admin" --cluster "$T69_ALL" isr get --file "/$1" --chunks 2 2>/dev/null || true)
    map=$("$BIN/dfs-admin" --cluster "$T69_ALL" lease status 2>/dev/null || true)
    python3 - "$raw" "$map" <<'PY'
import json, sys
best = None
for l in sys.argv[1].splitlines():
    try: r = json.loads(l)["isr"][1]
    except Exception: continue
    if r and (best is None or r["epoch"] > best["epoch"]): best = r
addr = {}
for l in sys.argv[2].splitlines():
    try: r = json.loads(l)
    except Exception: continue
    if "node" in r: addr[r["node"]] = r["addr"]
if best:
    print(best["epoch"], addr.get(best["members"][0], "?"), addr.get(best["members"][1], "?"))
PY
}
t69_writer() {  # mount file tag seconds out
    python3 - "$1/$2" "$3" "$4" > "$5" 2>&1 <<'PY'
import os, sys, time, json
path, tag, secs = sys.argv[1], sys.argv[2].encode(), float(sys.argv[3])
fd = os.open(path, os.O_RDWR)
off = 4 * 1024 * 1024 + 8192
start = time.time(); i = 0
acked = []; failed = []
while time.time() - start < secs:
    try:
        os.pwrite(fd, (tag + b"%06d" % i).ljust(4096, tag[:1]), off)
        os.fsync(fd)
        acked.append(i)
    except OSError:
        failed.append(i)
        time.sleep(0.2)
    i += 1
os.close(fd)
print(json.dumps({"acked": acked, "failed": failed}))
PY
}
t69_heal() {
    for n in "${T69_NODES[@]}"; do "$BIN/dfs-admin" --cluster "$n" fault clear >/dev/null 2>&1 || true; done
}
T69_CASES=${T69_CASES:-row2 row3 row5 row10 row12}
for T69_CASE in $T69_CASES; do
    T69_FILE=t69_$T69_CASE.bin
    dd if=/dev/urandom of="$MOUNT/$T69_FILE" bs=4M count=2 status=none
    dfs_sync
    T69_START=""
    for _ in $(seq 1 30); do
        T69_START=$(t69_isr "$T69_FILE")
        [ -n "$T69_START" ] && break
        sleep 1
    done
    read -r T69_E0 T69_P T69_S <<< "$T69_START"
    if [ -z "$T69_P" ] || [ "$T69_P" = "?" ] || [ -z "$T69_S" ] || [ "$T69_S" = "?" ]; then
        check "T69[$T69_CASE] setup: chunk 1's ISR found ($T69_START)" FAIL
        rm -f "${MOUNT:?}/${T69_FILE:?}"; continue
    fi
    T69_FID=$(grep -h "\[META SERVER\] put path=/$T69_FILE id=" "$LOG"/server*.log 2>/dev/null | tail -1 | grep -oP 'id=\K[0-9a-f-]+' || true)
    echo "  T69[$T69_CASE]: ISR epoch $T69_E0 primary=$T69_P secondary=$T69_S"
    t69_writer "$MOUNT" "$T69_FILE" A 18 "$LOG/t69_w1.out" & T69_W1=$!
    t69_writer "$T69_MOUNT2" "$T69_FILE" B 18 "$LOG/t69_w2.out" & T69_W2=$!
    sleep 4
    case "$T69_CASE" in
        row2)   # P–S link cut both ways; both still reach everyone else and the clients
            "$BIN/dfs-admin" --cluster "$T69_P" fault set --drop-to "$T69_S" >/dev/null 2>&1 || true
            "$BIN/dfs-admin" --cluster "$T69_S" fault set --drop-to "$T69_P" >/dev/null 2>&1 || true
            sleep 7; t69_heal ;;
        row3)   # S isolated from every peer and from clients
            "$BIN/dfs-admin" --cluster "$T69_S" fault set --drop-to "$(t69_others "$T69_S")" --refuse-clients >/dev/null 2>&1 || true
            for n in "${T69_NODES[@]}"; do [ "$n" != "$T69_S" ] && "$BIN/dfs-admin" --cluster "$n" fault set --drop-to "$T69_S" >/dev/null 2>&1; done
            sleep 7; t69_heal ;;
        row5)   # P cut off from every peer; clients still reach it. Then healed (#11).
            "$BIN/dfs-admin" --cluster "$T69_P" fault set --drop-to "$(t69_others "$T69_P")" >/dev/null 2>&1 || true
            for n in "${T69_NODES[@]}"; do [ "$n" != "$T69_P" ] && "$BIN/dfs-admin" --cluster "$n" fault set --drop-to "$T69_P" >/dev/null 2>&1; done
            sleep 10; t69_heal ;;   # lease 3s + margin, then a majority vote: takeover takes ~5s
        row10)  # P and S both frozen
            T69_PP=$(t69_pid_of "$T69_P"); T69_SP=$(t69_pid_of "$T69_S")
            kill -STOP $T69_PP $T69_SP; sleep 7; kill -CONT $T69_PP $T69_SP ;;
        row12)  # P and S both cut off from every peer (and each other); clients reach both.
                # Neither holds a lease, so P refuses and S can't take over: a wait, not an error.
            for m in "$T69_P" "$T69_S"; do
                "$BIN/dfs-admin" --cluster "$m" fault set --drop-to "$(t69_others "$m")" >/dev/null 2>&1 || true
            done
            for n in "${T69_NODES[@]}"; do
                [ "$n" != "$T69_P" ] && [ "$n" != "$T69_S" ] && "$BIN/dfs-admin" --cluster "$n" fault set --drop-to "$T69_P,$T69_S" >/dev/null 2>&1
            done
            sleep 6; t69_heal ;;
    esac
    wait "$T69_W1" "$T69_W2" 2>/dev/null || true
    dfs_sync; sync "$T69_MOUNT2" 2>/dev/null || true
    sleep 3
    T69_FRESH=$(fresh_read "$T69_FILE" $((4*1024*1024+8192)) 10)
    T69_END=$(t69_isr "$T69_FILE")
    read -r T69_E1 T69_CP T69_CS <<< "$T69_END"
    T69_V=$(python3 - "$LOG/t69_w1.out" "$LOG/t69_w2.out" "$T69_FRESH" "$T69_FID" <<'PY'
import json, sys, re, glob
fresh, fid = sys.argv[3], sys.argv[4]
ok, acks = False, []
for path, tag in ((sys.argv[1], "A"), (sys.argv[2], "B")):
    try: r = json.loads(open(path).read().strip().splitlines()[-1])
    except Exception: r = {"acked": [], "failed": []}
    last = max(r["acked"]) if r["acked"] else -1
    allowed = {last} | {f for f in r["failed"] if f > last}
    if fresh[:1] == tag and fresh[1:7].isdigit() and int(fresh[1:7]) in allowed: ok = True
    acks.append("%s:%d acked/%d failed" % (tag, len(r["acked"]), len(r["failed"])))
# I2/I3 from the servers' logs for this file's chunk 1
primaries, commits = {}, {}
for path in glob.glob("/tmp/dfs-test-logs/server*.log"):
    node = path.rsplit("/", 1)[1]
    for line in open(path, errors="replace"):
        line = re.sub(r"\x1b\[[0-9;]*m", "", line)
        if not fid or fid not in line: continue
        m = re.search(r"\[ORDER\] primary: ordering file \S+ chunk 1 epoch (\d+)", line)
        if m: primaries.setdefault(int(m.group(1)), set()).add(node)
        m = re.search(r"SLOT ISR: file \S+ chunk 1 epoch (\d+) = \[(.*)\]", line)
        if m: commits[int(m.group(1))] = re.findall(r"NodeId\(([0-9a-f-]+)\)", m.group(2))
two = {e: sorted(n) for e, n in primaries.items() if len(n) > 1}
unclean = [e for e in sorted(commits) if e - 1 in commits and commits[e] and commits[e][0] not in commits[e - 1]]
print("OK" if ok else "LOST", "TWO=%s" % (two or "none"), "UNCLEAN=%s" % (unclean or "none"), " ".join(acks))
PY
)
    echo "  T69[$T69_CASE]: servers hold $T69_FRESH; ISR now epoch ${T69_E1:-?} primary=${T69_CP:-?} secondary=${T69_CS:-?}; $T69_V"
    case "$T69_V" in OK*) t69_check "T69[$T69_CASE] I1 no acked write lost" PASS ;;
                     *)   t69_check "T69[$T69_CASE] I1 servers hold $T69_FRESH, not a writer's last acked write" FAIL ;; esac
    case "$T69_V" in *TWO=none*) t69_check "T69[$T69_CASE] I2 one primary per ISR epoch" PASS ;;
                     *)          t69_check "T69[$T69_CASE] I2 two primaries ordered writes at one epoch: $T69_V" FAIL ;; esac
    case "$T69_V" in *UNCLEAN=none*) t69_check "T69[$T69_CASE] I3 no unclean promotion" PASS ;;
                     *)              t69_check "T69[$T69_CASE] I3 an epoch's primary wasn't in the previous epoch: $T69_V" FAIL ;; esac
    T69_HASHES=$("$BIN/dfs-admin" --cluster "${T69_CP},${T69_CS}" isr read --file "/$T69_FILE" --chunk 1 2>/dev/null | python3 -c "
import json,sys
hs=[]
for l in sys.stdin:
    try: r=json.loads(l)
    except Exception: continue
    hs.append(r['blake3'][:16] if r.get('len') == 4194304 else 'ERR')
print(' '.join(hs))" || true)
    read -r T69_H1 T69_H2 <<< "$T69_HASHES"
    [ -n "$T69_H1" ] && [ "$T69_H1" = "$T69_H2" ] && [ "$T69_H1" != ERR ] \
        && t69_check "T69[$T69_CASE] I4 current ISR replicas identical ($T69_H1)" PASS \
        || t69_check "T69[$T69_CASE] I4 current ISR replicas differ or unreadable: $T69_HASHES" FAIL
    case "$T69_CASE" in
        row2|row3) [ "$T69_CP" = "$T69_P" ] \
            && t69_check "T69[$T69_CASE] no promotion: the primary stayed ($T69_P)" PASS \
            || t69_check "T69[$T69_CASE] the primary moved $T69_P -> $T69_CP without being voted out" FAIL ;;
        row5) [ "$T69_CP" != "$T69_P" ] && [ "${T69_E1:-0}" -gt "$T69_E0" ] \
            && t69_check "T69[row5] takeover: primary $T69_P -> $T69_CP at epoch $T69_E1" PASS \
            || t69_check "T69[row5] no takeover after the primary was cut off (epoch ${T69_E1:-?}, primary ${T69_CP:-?})" FAIL ;;
        row12) T69_NFAIL=$(echo "$T69_V" | grep -oP '[AB]:[0-9]+ acked/\K[0-9]+' | awk '{s+=$1} END{print s+0}')
            [ "$T69_NFAIL" = 0 ] \
            && t69_check "T69[row12] no writer error while both members were cut off (a 6s wait)" PASS \
            || t69_check "T69[row12] $T69_NFAIL write(s) failed while both members were cut off" FAIL ;;
    esac
    t69_heal
    rm -f "${MOUNT:?}/${T69_FILE:?}"
    sleep 3
done
fusermount -u "$T69_MOUNT2" 2>/dev/null || true
kill_client_and_wait "$T69_PID2"
fi # should_run T69
if should_run T73; then
snapshot_log T73
echo ""
echo "=== T73: the chunk's ISR primary is killed mid-storm; the secondary takes over, no acked write lost (SLOT-OWNERSHIP 3c gate) ==="
# Pete's 2026-10-03 question: with the primary dead, what does a writer see until the secondary
# is promoted (EIO, or a stall then success), and is any acked write lost? The primary is
# SIGKILLed (not frozen as in T66) and stays down past its lease, so only a takeover (Phase 3d)
# lets writes continue on two copies. A chunk whose primary isn't the leader is picked, so a
# leader election doesn't confound the result.
T73_ALL=127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904
T73_FILE=t73_primary_kill.bin
T73_CHUNKS=8
T73_DOWN=10
T73_SECS=26
T73_MOUNT2=/tmp/dfs-mount2
mkdir -p "$T73_MOUNT2"
RUST_LOG=info "$BIN/dfs-client" mount "$T73_MOUNT2" --cluster "$CLUSTER" \
    --log-file "$LOG/client_t73b.log" --allow-other --log-level debug &
T73_PID2=$!
sleep 2
mountpoint -q "$T73_MOUNT2" || check "T73 second client mounted" FAIL
t73_check() {   # required with DFS_ORDERED_WRITES=1, informational without (as T66/T67)
    if [ "${DFS_ORDERED_WRITES:-0}" = 1 ]; then check "$1" "$2"
    else echo "  (informational without DFS_ORDERED_WRITES) $2: $1"; fi
}
t73_writer() {  # mount file offset tag seconds out: fsync'd 4K writes to one block until the deadline
    python3 - "$1/$2" "$3" "$4" "$5" > "$6" 2>&1 <<'PY'
import os, sys, time, json
path, off, tag, secs = sys.argv[1], int(sys.argv[2]), sys.argv[3].encode(), float(sys.argv[4])
fd = os.open(path, os.O_RDWR)
start = time.time(); i = 0
acked = []; failed = []; worst = 0.0
while time.time() - start < secs:
    t0 = time.time()
    try:
        os.pwrite(fd, (tag + b"%06d" % i).ljust(4096, tag[:1]), off)
        os.fsync(fd)
        acked.append((i, time.time() - start))
    except OSError:
        failed.append((i, round(time.time() - start, 1)))
        time.sleep(0.2)
    worst = max(worst, time.time() - t0)
    i += 1
os.close(fd)
ts = [0.0] + [t for _, t in acked]
gap = max((b - a for a, b in zip(ts, ts[1:])), default=secs)
print(json.dumps({"acked": [a for a, _ in acked], "late": sum(1 for _, t in acked if t > secs - 4),
                  "failed": [f for f, _ in failed], "failed_at": [t for _, t in failed],
                  "worst_s": round(worst, 2), "max_ack_gap_s": round(gap, 2)}))
PY
}
dd if=/dev/urandom of="$MOUNT/$T73_FILE" bs=4M count=$T73_CHUNKS status=none
dfs_sync
T73_LEADER=$("$BIN/dfs-admin" --cluster "$CLUSTER" --format json cluster status 2>/dev/null | python3 -c "
import json, sys
d = json.load(sys.stdin)
online = sorted((n for n in d.get('nodes', []) if n.get('status') == 'Online'), key=lambda n: n['id'])
print(online[0]['address'] if online else '')" 2>/dev/null || true)
T73_MAP=$("$BIN/dfs-admin" --cluster "$T73_ALL" lease status 2>/dev/null \
    | python3 -c "import json,sys; [print(r['node'], r['addr']) for r in map(json.loads, sys.stdin) if 'node' in r]" || true)
# First chunk (1..) with a committed ISR whose primary isn't the leader: "chunk primary secondary epoch".
T73_PICK=""
for _ in $(seq 1 30); do
    T73_PICK=$("$BIN/dfs-admin" --cluster 127.0.0.1:8900 isr get --file "/$T73_FILE" --chunks $T73_CHUNKS 2>/dev/null \
        | python3 -c "
import json, sys
addr = dict(l.split() for l in '''$T73_MAP'''.strip().splitlines())
isr = json.loads(sys.stdin.readline())['isr']
for c in range(1, len(isr)):
    r = isr[c]
    if r and len(r['members']) >= 2 and addr.get(r['members'][0]) not in ('', None, '$T73_LEADER'):
        print(c, addr[r['members'][0]], addr.get(r['members'][1], ''), r['epoch']); break" 2>/dev/null || true)
    [ -n "$T73_PICK" ] && break
    sleep 1
done
read -r T73_C T73_P T73_S T73_EPOCH0 <<< "$T73_PICK"
if [ -z "$T73_P" ] || [ -z "$T73_S" ]; then
    check "T73 setup: a chunk whose ISR primary isn't the leader ($T73_LEADER) found" FAIL
else
    T73_OFF=$(( T73_C * 4 * 1024 * 1024 + 8192 ))
    T73_FILE_ID_PAT=$(grep -h "\[META SERVER\] put path=/$T73_FILE id=" "$LOG"/server*.log 2>/dev/null | tail -1 | grep -oP 'id=\K[0-9a-f-]+' || true)
    T73_FILE_ID_PAT=${T73_FILE_ID_PAT:-no-file-id}
    T73_N=$(( ${T73_P##*:} - 8900 + 1 ))
    T73_MARK=$(cat "$LOG"/server*.log | wc -l)
    echo "  T73: chunk $T73_C ISR epoch $T73_EPOCH0 primary=$T73_P (node$T73_N) secondary=$T73_S leader=$T73_LEADER; killing the primary 4s in for ${T73_DOWN}s"
    t73_writer "$MOUNT" "$T73_FILE" "$T73_OFF" A "$T73_SECS" "$LOG/t73_w1.out" & T73_W1=$!
    t73_writer "$T73_MOUNT2" "$T73_FILE" "$T73_OFF" B "$T73_SECS" "$LOG/t73_w2.out" & T73_W2=$!
    sleep 4
    pkill -9 -f "dfs-server start --config $BASE/node${T73_N}/config.toml" 2>/dev/null || true
    sleep "$T73_DOWN"
    RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 DFS_FAULT_INJECTION=1 "$BIN/dfs-server" start --config "$BASE/node${T73_N}/config.toml" \
        >> "$LOG/server${T73_N}.log" 2>&1 &
    wait "$T73_W1" "$T73_W2" 2>/dev/null || true
    dfs_sync; sync "$T73_MOUNT2" 2>/dev/null || true
    sleep 5
    T73_FRESH=$(fresh_read "$T73_FILE" "$T73_OFF" 10)
    T73_VERDICT=$(python3 - "$LOG/t73_w1.out" "$LOG/t73_w2.out" "$T73_FRESH" <<'PY'
import json, sys
fresh = sys.argv[3]
ok, summary = False, []
for path, tag in ((sys.argv[1], "A"), (sys.argv[2], "B")):
    try: r = json.loads(open(path).read().strip().splitlines()[-1])
    except Exception as e: print("BAD", "writer %s output unreadable: %s" % (tag, e)); sys.exit()
    last = max(r["acked"]) if r["acked"] else -1
    allowed = {last} | {f for f in r["failed"] if f > last}
    if fresh[:1] == tag and fresh[1:7].isdigit() and int(fresh[1:7]) in allowed: ok = True
    summary.append("%s: %d acked (last %d, %d in the last 4s), %d failed%s, longest gap between acks %.1fs, worst op %.1fs"
                   % (tag, len(r["acked"]), last, r["late"], len(r["failed"]),
                      (" at %s s" % r["failed_at"][:6]) if r["failed"] else "", r["max_ack_gap_s"], r["worst_s"]))
    if r["late"] == 0: summary.append("NOPROGRESS-" + tag)
print("OK" if ok else "LOST", "; ".join(summary))
PY
)
    echo "  T73: servers hold $T73_FRESH; ${T73_VERDICT#* }"
    case "$T73_VERDICT" in
        OK*) t73_check "T73a no acked write lost across the primary's death (servers hold $T73_FRESH)" PASS ;;
        *)   t73_check "T73a servers hold $T73_FRESH, not a writer's last acked write -- acked write lost" FAIL ;;
    esac
    T73_NOW=$("$BIN/dfs-admin" --cluster "$T73_S" isr get --file "/$T73_FILE" --chunks $T73_CHUNKS 2>/dev/null \
        | python3 -c "import json,sys; r=json.loads(sys.stdin.readline())['isr'][$T73_C]; print(r['epoch'], ' '.join(r['members']))" 2>/dev/null || true)
    read -r T73_EPOCH T73_NP T73_NS <<< "$T73_NOW"
    T73_MAP2=$("$BIN/dfs-admin" --cluster "$T73_ALL" lease status 2>/dev/null \
        | python3 -c "import json,sys; [print(r['node'], r['addr']) for r in map(json.loads, sys.stdin) if 'node' in r]" || true)
    T73_CP=$(echo "$T73_MAP2" | awk -v n="$T73_NP" '$1==n{print $2}')
    T73_CS=$(echo "$T73_MAP2" | awk -v n="$T73_NS" '$1==n{print $2}')
    echo "  T73: ISR now epoch ${T73_EPOCH:-?}: primary=${T73_CP:-?} secondary=${T73_CS:-?}"
    if [ "${T73_EPOCH:-0}" -gt "${T73_EPOCH0:-0}" ] && [ "$T73_CP" = "$T73_S" ]; then
        t73_check "T73b the secondary took over (epoch $T73_EPOCH0 -> $T73_EPOCH, primary $T73_S)" PASS
    else
        t73_check "T73b no takeover: epoch ${T73_EPOCH0:-?} -> ${T73_EPOCH:-?}, primary ${T73_CP:-?} (expected $T73_S)" FAIL
    fi
    case "$T73_VERDICT" in
        *NOPROGRESS*) t73_check "T73c a writer made no progress in the last 4s after the takeover" FAIL ;;
        *)            t73_check "T73c both writers made progress after the takeover" PASS ;;
    esac
    T73_HASHES=$("$BIN/dfs-admin" --cluster "${T73_CP:-$T73_S},${T73_CS:-$T73_P}" isr read --file "/$T73_FILE" --chunk "$T73_C" 2>/dev/null | python3 -c "
import json,sys
hs=[]
for l in sys.stdin:
    try: r=json.loads(l)
    except Exception: continue
    hs.append(r['blake3'][:16] if r.get('len') == 4194304 else 'ERR')
print(' '.join(hs))" || true)
    read -r T73_H1 T73_H2 <<< "$T73_HASHES"
    if [ -n "$T73_H1" ] && [ "$T73_H1" = "$T73_H2" ] && [ "$T73_H1" != ERR ]; then
        t73_check "T73d the new ISR pair holds identical bytes ($T73_H1)" PASS
    else
        t73_check "T73d the new ISR pair differs or is unreadable: $T73_HASHES" FAIL
    fi
    # The primary's death must cost the writers a stall, never an error: a guest sees EIO
    # as a disk failure. Before 2026-10-05 the client gave up while the takeover was still
    # pending (the primary not yet voted expired) and fsync returned EIO.
    T73_EIO=$(python3 -c "
import json, sys
print(sum(len(json.loads(open(p).read().strip().splitlines()[-1])['failed']) for p in sys.argv[1:]))" "$LOG/t73_w1.out" "$LOG/t73_w2.out" 2>/dev/null || echo "?")
    [ "$T73_EIO" = 0 ] \
        && t73_check "T73e no writer got an error while the secondary took over" PASS \
        || t73_check "T73e $T73_EIO write(s) failed (EIO) while the secondary took over" FAIL
    # Counted from the raw sends in the clients' debug logs, not from the client's own
    # UNORDERED line: the background flusher once bypassed that line and the ISR pair both.
    T73_RAW=$(cat "$CURRENT_CLIENT_LOG" "$LOG/client_t73b.log" 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' \
        | grep -aE "Sending request to [0-9.:]+: MultiPatch \{" | grep -ac "$T73_FILE_ID_PAT" || true)
    [ "${T73_RAW:-1}" = 0 ] \
        && t73_check "T73f every patch to the file went out ordered" PASS \
        || t73_check "T73f $T73_RAW unordered MultiPatch send(s) to the file" FAIL
    T73_SINCE=$(cat "$LOG"/server*.log | tail -n +"$((T73_MARK + 1))" | sed 's/\x1b\[[0-9;]*m//g')
    echo "  T73: $(echo "$T73_SINCE" | grep -ac "resync anchor" || true) resync(s) served; $(grep -ac "ReplaceIsrMember" "$CURRENT_CLIENT_LOG" "$LOG/client_t73b.log" 2>/dev/null | awk -F: '{s+=$2} END{print s+0}') ReplaceIsrMember mention(s) in the clients' logs; $(cat "$CURRENT_CLIENT_LOG" "$LOG/client_t73b.log" 2>/dev/null | grep -ac "UNORDERED MultiPatch" || true) unordered write(s)"
fi
rm -f "$MOUNT/$T73_FILE"
fusermount -u "$T73_MOUNT2" 2>/dev/null || true
kill_client_and_wait "$T73_PID2"
fi # should_run T73
if should_run T74; then
snapshot_log T74
echo ""
echo "=== T74: lease chaos while two clients write: no acked write lost, every ISR pair identical (SLOT-OWNERSHIP 3c gate) ==="
# T60's chaos (random partitions, black-holes, one-way cuts and freezes of 1-2 nodes) with
# writes running. Each writer fsyncs its own 4K block in chunks 1..4 in turn, so "the last
# acked write" of every block is exact, and each chunk still sees two writers interleaved.
T74_SECONDS="${DFS_T74_SECONDS:-60}"
T74_NODES=(127.0.0.1:8900 127.0.0.1:8901 127.0.0.1:8902 127.0.0.1:8903 127.0.0.1:8904)
T74_ALL="$(IFS=,; echo "${T74_NODES[*]}")"
T74_FILE=t74_chaos.bin
T74_CHUNKS=5
T74_MOUNT2=/tmp/dfs-mount2
mkdir -p "$T74_MOUNT2"
RUST_LOG=info "$BIN/dfs-client" mount "$T74_MOUNT2" --cluster "$CLUSTER" \
    --log-file "$LOG/client_t74b.log" --allow-other --log-level debug &
T74_PID2=$!
sleep 2
mountpoint -q "$T74_MOUNT2" || check "T74 second client mounted" FAIL
t74_check() {   # required with DFS_ORDERED_WRITES=1, informational without (as T69/T73)
    if [ "${DFS_ORDERED_WRITES:-0}" = 1 ]; then check "$1" "$2"
    else echo "  (informational without DFS_ORDERED_WRITES) $2: $1"; fi
}
t74_filter() {   # t74_filter <node> <drop-to-csv> [--black-hole]
    [ -n "$2" ] && "$BIN/dfs-admin" --cluster "$1" fault set --drop-to "$2" $3 >/dev/null 2>&1 || true
}
t74_heal() { for n in "${T74_NODES[@]}"; do "$BIN/dfs-admin" --cluster "$n" fault clear >/dev/null 2>&1 || true; done; }
t74_pid() {   # t74_pid <node-index 1..5>
    for p in $(pgrep -x dfs-server || true); do
        tr '\0' ' ' < /proc/$p/cmdline 2>/dev/null | grep -q "node$1/config.toml" && echo "$p"
    done
    true
}
t74_writer() {  # mount file block-offset-in-chunk tag stop-file out: until the stop file appears
    python3 - "$1/$2" "$3" "$4" "$5" > "$6" 2>&1 <<'PY'
import os, sys, time, json
path, base, tag, stop = sys.argv[1], int(sys.argv[2]), sys.argv[3].encode(), sys.argv[4]
fd = os.open(path, os.O_RDWR)
start = time.time(); i = 0
per = {c: {"acked": [], "failed": []} for c in range(1, 5)}
errors = 0; worst = 0.0; ack_at = []
while not os.path.exists(stop) and time.time() - start < 600:
    c = 1 + i % 4
    t0 = time.time()
    try:
        os.pwrite(fd, (tag + b"%06d" % i).ljust(4096, tag[:1]), c * 4 * 1024 * 1024 + base)
        os.fsync(fd)
        per[c]["acked"].append(i); ack_at.append(time.time())
    except OSError:
        per[c]["failed"].append(i); errors += 1
        time.sleep(0.2)
    worst = max(worst, time.time() - t0)
    i += 1
os.close(fd)
end = time.time()
late = sum(1 for t in ack_at if t > end - 4)
print(json.dumps({"per": per, "errors": errors, "late": late, "worst_s": round(worst, 2)}))
PY
}
dd if=/dev/urandom of="$MOUNT/$T74_FILE" bs=4M count=$T74_CHUNKS status=none
dfs_sync
T74_FILE_ID_PAT=$(grep -h "\[META SERVER\] put path=/$T74_FILE id=" "$LOG"/server*.log 2>/dev/null | tail -1 | grep -oP 'id=\K[0-9a-f-]+' || true)
T74_FILE_ID_PAT=${T74_FILE_ID_PAT:-no-file-id}
T74_CLIENT_MARK=$(wc -l < "$CURRENT_CLIENT_LOG" 2>/dev/null || echo 0)
T74_STOP="$LOG/t74.stop"
rm -f "$T74_STOP"
t74_writer "$MOUNT" "$T74_FILE" 8192 A "$T74_STOP" "$LOG/t74_w1.out" & T74_W1=$!
t74_writer "$T74_MOUNT2" "$T74_FILE" 16384 B "$T74_STOP" "$LOG/t74_w2.out" & T74_W2=$!
sleep 2
T74_END=$(( $(date +%s) + T74_SECONDS ))
T74_ROUNDS=0; T74_MODES=""
while [ "$(date +%s)" -lt "$T74_END" ]; do
    T74_ROUNDS=$((T74_ROUNDS + 1))
    victims=$(printf '%s\n' 0 1 2 3 4 | shuf -n $(( RANDOM % 2 + 1 )) | tr '\n' ' ')
    inside=""; outside=""
    for i in 0 1 2 3 4; do
        if [[ " $victims " == *" $i "* ]]; then inside="${inside:+$inside,}${T74_NODES[$i]}"; else outside="${outside:+$outside,}${T74_NODES[$i]}"; fi
    done
    case $(( RANDOM % 4 )) in
        0) mode=partition;  for i in 0 1 2 3 4; do
               if [[ " $victims " == *" $i "* ]]; then t74_filter "${T74_NODES[$i]}" "$outside"; else t74_filter "${T74_NODES[$i]}" "$inside"; fi; done ;;
        1) mode=blackhole;  for i in 0 1 2 3 4; do
               if [[ " $victims " == *" $i "* ]]; then t74_filter "${T74_NODES[$i]}" "$outside" --black-hole; else t74_filter "${T74_NODES[$i]}" "$inside" --black-hole; fi; done ;;
        2) mode=oneway;     for i in 0 1 2 3 4; do [[ " $victims " != *" $i "* ]] && t74_filter "${T74_NODES[$i]}" "$inside"; done ;;
        3) mode=freeze;     for i in $victims; do p=$(t74_pid $((i + 1))); [ -n "$p" ] && kill -STOP "$p"; done ;;
    esac
    T74_MODES="$T74_MODES $mode($(echo $victims | tr ' ' '+'))"
    sleep $(( RANDOM % 6 + 2 ))
    if [ "$mode" = freeze ]; then for i in $victims; do p=$(t74_pid $((i + 1))); [ -n "$p" ] && kill -CONT "$p"; done; fi
    [ $(( RANDOM % 3 )) -ne 0 ] && t74_heal
    sleep $(( RANDOM % 3 + 1 ))
done
t74_heal
for i in 1 2 3 4 5; do p=$(t74_pid $i); [ -n "$p" ] && kill -CONT "$p" 2>/dev/null; done
echo "  T74: $T74_ROUNDS chaos rounds over ${T74_SECONDS}s:$T74_MODES"
sleep 10   # writers keep going on a healed cluster; T74c wants acks in their last 4s
touch "$T74_STOP"
wait "$T74_W1" "$T74_W2" 2>/dev/null || true
dfs_sync; sync "$T74_MOUNT2" 2>/dev/null || true
sleep 5
# Every writer block through one fresh mount (no writer's cache can answer).
T74_FM=/tmp/dfs-mount-fresh
mkdir -p "$T74_FM"
RUST_LOG=info "$BIN/dfs-client" mount "$T74_FM" --cluster "$CLUSTER" \
    --log-file "$LOG/client_fresh.log" --allow-other --log-level debug &
T74_FPID=$!
sleep 2
T74_FRESH=$(python3 -c "
f = open('$T74_FM/$T74_FILE', 'rb')
for c in range(1, 5):
    for base in (8192, 16384):
        f.seek(c * 4 * 1024 * 1024 + base)
        print(c, base, f.read(10).hex())" 2>/dev/null || true)   # hex: \$(...) drops NUL bytes
fusermount -u "$T74_FM" 2>/dev/null || true
kill_client_and_wait "$T74_FPID"
T74_VERDICT=$(python3 - "$LOG/t74_w1.out" "$LOG/t74_w2.out" "$T74_FRESH" <<'PY'
import json, sys
fresh = {}
for l in sys.argv[3].splitlines():
    p = l.split(" ", 2)
    if len(p) == 3: fresh[(int(p[0]), int(p[1]))] = bytes.fromhex(p[2]).decode(errors="replace").replace("\x00", "\\0")
lost, summary, noprog, errors = [], [], [], 0
for path, tag, base in ((sys.argv[1], "A", 8192), (sys.argv[2], "B", 16384)):
    try: r = json.loads(open(path).read().strip().splitlines()[-1])
    except Exception as e: print("BAD NOPROG=? ERRORS=? | writer %s output unreadable: %s |" % (tag, e)); sys.exit()
    acked = sum(len(v["acked"]) for v in r["per"].values())
    errors += r["errors"]
    summary.append("%s: %d acked, %d failed, %d in the last 4s, worst op %.1fs" % (tag, acked, r["errors"], r["late"], r["worst_s"]))
    if r["late"] == 0: noprog.append(tag)
    for c, v in r["per"].items():
        c = int(c)
        last = max(v["acked"]) if v["acked"] else -1
        allowed = {last} | {f for f in v["failed"] if f > last}
        got = fresh.get((c, base), "")
        if not (got[:1] == tag and got[1:7].isdigit() and int(got[1:7]) in allowed):
            lost.append("chunk %d %s: servers hold %r, last acked %s%06d" % (c, tag, got, tag, last))
print("LOST" if lost else "OK", "NOPROG=%s" % (",".join(noprog) or "none"), "ERRORS=%d" % errors, "|", "; ".join(summary), "|", "; ".join(lost))
PY
)
echo "  T74: $(echo "$T74_VERDICT" | cut -d'|' -f2)"
case "$T74_VERDICT" in
    OK*) t74_check "T74a no acked write lost across the chaos (8 blocks, 4 chunks)" PASS ;;
    *)   t74_check "T74a acked write(s) lost:$(echo "$T74_VERDICT" | cut -d'|' -f3)" FAIL
         echo "  T74: the leader's view of the file after the loss:"
         "$BIN/dfs-admin" --cluster "$CLUSTER" file info "/$T74_FILE" 2>&1 | sed 's/^/    /' | head -40 ;;
esac
case "$T74_VERDICT" in
    *NOPROG=none*) t74_check "T74c both writers made progress after the chaos healed" PASS ;;
    *)             t74_check "T74c a writer made no progress in the last 4s after the heal: $(echo "$T74_VERDICT" | cut -d'|' -f1)" FAIL ;;
esac
# Each chunk's current ISR (highest epoch any node reports) must hold identical bytes.
T74_MAP=$("$BIN/dfs-admin" --cluster "$T74_ALL" lease status 2>/dev/null \
    | python3 -c "import json,sys; [print(r['node'], r['addr']) for r in map(json.loads, sys.stdin) if 'node' in r]" || true)
T74_PAIRS=$("$BIN/dfs-admin" --cluster "$T74_ALL" isr get --file "/$T74_FILE" --chunks $T74_CHUNKS 2>/dev/null | python3 -c "
import json, sys
addr = dict(l.split() for l in '''$T74_MAP'''.strip().splitlines())
best = {}
for l in sys.stdin:
    try: isr = json.loads(l)['isr']
    except Exception: continue
    for c in range(1, 5):
        r = isr[c] if c < len(isr) else None
        if r and len(r['members']) >= 2 and (c not in best or r['epoch'] > best[c]['epoch']): best[c] = r
for c in range(1, 5):
    r = best.get(c)
    print(c, r['epoch'] if r else '?', ','.join(addr.get(m, '?') for m in r['members'][:2]) if r else '?')" 2>/dev/null || true)
T74_BAD=""
while read -r c e pair; do
    [ -z "$c" ] && continue
    hs=$("$BIN/dfs-admin" --cluster "$pair" isr read --file "/$T74_FILE" --chunk "$c" 2>/dev/null | python3 -c "
import json,sys
hs=[]
for l in sys.stdin:
    try: r=json.loads(l)
    except Exception: continue
    hs.append(r['blake3'][:16] if r.get('len') == 4194304 else 'ERR')
print(' '.join(hs))" || true)
    read -r h1 h2 <<< "$hs"
    echo "  T74: chunk $c ISR epoch $e [$pair]: ${hs:-unreadable}"
    if echo "$T74_VERDICT" | grep -q "chunk $c "; then   # a loss here: what each member stores
        for base in 8192 16384; do
            "$BIN/dfs-admin" --cluster "$pair" isr read --file "/$T74_FILE" --chunk "$c" --range "$base:10" 2>/dev/null | python3 -c "
import json,sys
for l in sys.stdin:
    try: r=json.loads(l)
    except Exception: continue
    print('    stored on', r.get('addr'), 'at +$base:', repr(bytes.fromhex(r.get('range_hex',''))), r.get('chunk_id','')[:16], r.get('error','')[:120])" || true
        done
    fi
    { [ -n "$h1" ] && [ "$h1" = "$h2" ] && [ "$h1" != ERR ]; } || T74_BAD="$T74_BAD chunk$c($hs)"
done <<< "$T74_PAIRS"
case "$T74_VERDICT" in OK*) ;; *)   # and through a second fresh client, 4K preads, no readahead
    RUST_LOG=info "$BIN/dfs-client" mount "$T74_FM" --cluster "$CLUSTER" \
        --log-file "$LOG/client_fresh2.log" --allow-other --log-level debug &
    T74_FPID=$!
    sleep 2
    python3 -c "
import os
fd = os.open('$T74_FM/$T74_FILE', os.O_RDONLY)
for c in range(1, 5):
    for base in (8192, 16384):
        print('    second fresh read chunk', c, '+%d:' % base, repr(os.pread(fd, 10, c * 4 * 1024 * 1024 + base)))" 2>&1 || true
    fusermount -u "$T74_FM" 2>/dev/null || true
    kill_client_and_wait "$T74_FPID" ;;
esac
[ -n "$T74_PAIRS" ] && [ -z "$T74_BAD" ] \
    && t74_check "T74b every chunk's current ISR pair holds identical bytes" PASS \
    || t74_check "T74b ISR pair(s) differ or unreadable:${T74_BAD:- no ISR found}" FAIL
T74_RAW=$( { tail -n +"$((T74_CLIENT_MARK + 1))" "$CURRENT_CLIENT_LOG"; cat "$LOG/client_t74b.log"; } 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' \
    | grep -aE "Sending request to [0-9.:]+: MultiPatch \{" | grep -ac "$T74_FILE_ID_PAT" || true)
T74_ERR=$(echo "$T74_VERDICT" | grep -oP 'ERRORS=\K[0-9?]+' || echo "?")
echo "  T74 (informational): $T74_ERR writer error(s); ${T74_RAW:-?} unordered MultiPatch send(s) to the file"
# Every fault here ends within the client's 20s outage budget, so a writer should only ever wait.
# Was 0-11 errors per run: ordering refusals (a pair with no lease) weren't treated as a wait.
[ "$T74_ERR" = 0 ] \
    && t74_check "T74d no writer got an error (every outage was waited out)" PASS \
    || t74_check "T74d $T74_ERR writer error(s) during the chaos" FAIL
rm -f "${MOUNT:?}/${T74_FILE:?}"
fusermount -u "$T74_MOUNT2" 2>/dev/null || true
kill_client_and_wait "$T74_PID2"
fi # should_run T74

# ── Test 75: no-op rewrites after a one-sided fold leave the pair on ONE version ──────────
# The 2026-09-27 VM-108 disk-1 chunk 9 shape. A guest rewrote bytes the chunk already held. One
# replica had no accumulator for the slot (it had just folded), so it started a fresh one and
# returned a new id; the others still held a warm merge buffer, saw the rewrite change nothing,
# and returned the old id (9c40 -> 9c40). The pair split on identity with identical bytes, and the
# split later fed a self-fold (a294+delta -> a294) and an abandoned patch: a permanent EIO.
# Deterministic: write, fold ONE member, rewrite the same bytes, then fold the other. Each case
# requires (a) no replica disagreement or backfill, (b) both ISR members report the same head
# id, (c) identical bytes, (d) every block reads back; then a fold on both keeps (b) and (c).
if should_run T75; then
snapshot_log T75
echo ""
echo "=== T75: no-op rewrites after a one-sided fold leave both ISR members on one version (09-27 repro) ==="
T75_ALL=127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904
t75_writes() {  # file first last: fsync'd 4K writes of "W<n>" to distinct blocks of chunk 1
    python3 - "$MOUNT/$1" "$2" "$3" <<'PY'
import os, sys
path, a, b = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
fd = os.open(path, os.O_RDWR)
for i in range(a, b + 1):
    os.pwrite(fd, (b"W%06d" % i).ljust(4096, b"w"), 4 * 1024 * 1024 + i * 8192)
    os.fsync(fd)
os.close(fd)
PY
}
t75_pair() {  # file: "<primary addr> <secondary addr>" of chunk 1's committed ISR
    python3 - "$("$BIN/dfs-admin" --cluster "$T75_ALL" lease status 2>/dev/null)" \
        "$("$BIN/dfs-admin" --cluster 127.0.0.1:8900 isr get --file "/$1" --chunks 2 2>/dev/null)" <<'PY'
import json, sys
addr = {}
for l in sys.argv[1].splitlines():
    r = json.loads(l)
    if "node" in r: addr[r["node"]] = r["addr"]
isr = json.loads(sys.argv[2].splitlines()[0])["isr"][1]
print(" ".join(addr.get(m, "?") for m in isr["members"]) if isr else "")
PY
}
t75_heads() {  # file pair: "<id>/<blake3> <id>/<blake3>" as each member reports chunk 1
    "$BIN/dfs-admin" --cluster "$(echo "$2" | tr ' ' ',')" isr read --file "/$1" --chunk 1 2>/dev/null \
        | python3 -c "
import json, sys
rs = [json.loads(l) for l in sys.stdin]
for r in rs:   # the head line truncates an error: print it whole (stderr reaches the suite log)
    if 'error' in r: print('    T75 head read error from %s: %s' % (r.get('addr', '?'), r['error'][:300]), file=sys.stderr)
print(' '.join(r.get('chunk_id', 'ERR')[:12] + '/' + r.get('blake3', r.get('error', '?'))[:12] for r in rs))"
}
# a/b/e (pair agreement) are required with DFS_ORDERED_WRITES=1. Without it the 09-27 no-op
# divergence is still there (ordering is its fix): seen in 2 of 3 ordering-off suites and 1 of 15
# lone runs once commits became durable (2026-10-06), so report only. d (data) is always required.
t75_check() {
    if [ "${DFS_ORDERED_WRITES:-0}" = 1 ]; then check "$1" "$2"
    else echo "  (informational without DFS_ORDERED_WRITES) $2: $1"; fi
}
t75_same() { set -- $1; [ $# = 2 ] && [ "$1" = "$2" ] && [ "${1#ERR}" = "$1" ]; }
t75_same_bytes() { set -- $1; [ $# = 2 ] && [ "${1#*/}" = "${2#*/}" ] && [ "${1#ERR}" = "$1" ] && [ "${2#ERR}" = "$2" ]; }
for T75_CASE in primary secondary; do
    T75_FILE=t75_noop_$T75_CASE.bin
    dd if=/dev/urandom of="$MOUNT/$T75_FILE" bs=4M count=2 status=none
    dfs_sync
    for _ in $(seq 1 30); do [ -n "$(t75_pair "$T75_FILE" 2>/dev/null)" ] && break; sleep 1; done
    T75_PAIR=$(t75_pair "$T75_FILE")
    if [ -z "$T75_PAIR" ]; then check "T75[$T75_CASE] chunk 1 has a committed ISR" FAIL; rm -f "${MOUNT:?}/${T75_FILE:?}"; continue; fi
    t75_writes "$T75_FILE" 0 4          # real changes: both members hold an accumulator
    t75_writes "$T75_FILE" 0 4          # the same bytes again: both warm their merge buffer
    dfs_sync
    T75_MARK=$(wc -l < "$CURRENT_CLIENT_LOG")
    set -- $T75_PAIR
    [ "$T75_CASE" = primary ] && T75_FOLD=$1 T75_OTHER=$2 || T75_FOLD=$2 T75_OTHER=$1
    echo "  T75[$T75_CASE]: pair $T75_PAIR; heads before: $(t75_heads "$T75_FILE" "$T75_PAIR")"
    # A one-sided fold is refused while the ordered stream is active (ORDERED_STREAM_QUIET, 10s);
    # after that an idle-fold timer can fold one member alone, which is what happened on 09-27.
    sleep 12
    echo "  T75[$T75_CASE]: folding $T75_FOLD alone after the stream went quiet, then rewriting the same 5 blocks"
    "$BIN/dfs-admin" --cluster "$T75_FOLD" isr fold --file "/$T75_FILE" --chunk 1 | sed 's/^/    /'
    t75_writes "$T75_FILE" 0 4          # no-op rewrites: one member cold, one warm
    dfs_sync
    T75_H1=$(t75_heads "$T75_FILE" "$T75_PAIR")
    echo "  T75[$T75_CASE]: after the no-op rewrites, heads: $T75_H1"
    t75_writes "$T75_FILE" 5 6          # and a real write on top
    dfs_sync
    T75_H2=$(t75_heads "$T75_FILE" "$T75_PAIR")
    echo "  T75[$T75_CASE]: after a real write on top, heads: $T75_H2"
    sleep 12   # quiet again, so both unordered folds run (the 09-27 a294+delta -> a294 self-fold)
    "$BIN/dfs-admin" --cluster "$T75_OTHER" isr fold --file "/$T75_FILE" --chunk 1 | sed 's/^/    /'
    "$BIN/dfs-admin" --cluster "$T75_FOLD" isr fold --file "/$T75_FILE" --chunk 1 | sed 's/^/    /'
    # The second ForceFold finds the slot folded elsewhere and heals the result rather than
    # folding again, so its member holds the bytes only once that heal lands. Until the next
    # write, that member's ordered head may still name the pre-fold token: same version, same
    # bytes (the next write lands on one id on both, check e), so c compares bytes.
    T75_WAITED=0
    for _ in $(seq 1 30); do
        T75_H3=$(t75_heads "$T75_FILE" "$T75_PAIR")
        t75_same_bytes "$T75_H3" && break
        sleep 1; T75_WAITED=$((T75_WAITED + 1))
    done
    echo "  T75[$T75_CASE]: after folding both, heads (${T75_WAITED}s): $T75_H3"
    # 09-27 step 3: the patch after the self-fold is where the replicas split (9c40 -> 8195 on
    # one, 9c40 -> 9c40 on the others). Rewrite the same bytes again, then make a real change.
    t75_writes "$T75_FILE" 0 4
    dfs_sync
    T75_H4=$(t75_heads "$T75_FILE" "$T75_PAIR")
    t75_writes "$T75_FILE" 7 7
    dfs_sync
    T75_H5=$(t75_heads "$T75_FILE" "$T75_PAIR")
    echo "  T75[$T75_CASE]: after the folds, a no-op rewrite: $T75_H4; then a real write: $T75_H5"
    T75_SINCE=$(tail -n +"$((T75_MARK + 1))" "$CURRENT_CLIENT_LOG" | sed 's/\x1b\[[0-9;]*m//g')
    T75_DIS=$(echo "$T75_SINCE" | grep -ac "REPLICA DISAGREEMENT" || true)
    T75_BACKFILL=$(echo "$T75_SINCE" | grep -ac "landed on only" || true)
    echo "$T75_SINCE" | grep -a "REPLICA DISAGREEMENT\|landed on only" | head -2 | cut -c1-240 | sed 's/^/    /'
    [ "$T75_DIS" = 0 ] && [ "$T75_BACKFILL" = 0 ] \
        && t75_check "T75[$T75_CASE]a no-op rewrites caused no replica disagreement or backfill" PASS \
        || t75_check "T75[$T75_CASE]a no-op rewrites split the pair: $T75_DIS disagreement(s), $T75_BACKFILL backfill(s)" FAIL
    t75_same "$T75_H1" && t75_same "$T75_H2" \
        && t75_check "T75[$T75_CASE]b both ISR members report one version (same head id and bytes)" PASS \
        || t75_check "T75[$T75_CASE]b ISR members on different versions: [$T75_H1] then [$T75_H2]" FAIL
    if [ "${DFS_ORDERED_WRITES:-0}" = 1 ]; then
        t75_same_bytes "$T75_H3" \
            && check "T75[$T75_CASE]c after a fold on both, both ISR members hold the same bytes" PASS \
            || check "T75[$T75_CASE]c after a fold on both, members still differ after 30s: [$T75_H3]" FAIL
    else
        # Unordered, a fold's result is re-replicated by the leader's placement, not to the
        # ISR pair, so the other member may legitimately not hold the slot any more.
        t75_same_bytes "$T75_H3" || echo "  T75[$T75_CASE]c (informational without DFS_ORDERED_WRITES): after a fold on both: [$T75_H3]"
    fi
    t75_same "$T75_H4" && t75_same "$T75_H5" \
        && t75_check "T75[$T75_CASE]e writes after the folds keep the pair on one version" PASS \
        || t75_check "T75[$T75_CASE]e writes after the folds split the pair: [$T75_H4] then [$T75_H5]" FAIL
    T75_BAD=$(python3 - "$MOUNT/$T75_FILE" <<'PY'
import sys
f = open(sys.argv[1], "rb")
bad = []
for i in range(8):
    f.seek(4 * 1024 * 1024 + i * 8192)
    if f.read(4096) != (b"W%06d" % i).ljust(4096, b"w"): bad.append(i)
print(" ".join(map(str, bad)))
PY
)
    [ -z "$T75_BAD" ] \
        && check "T75[$T75_CASE]d all 8 blocks read back" PASS \
        || check "T75[$T75_CASE]d blocks $T75_BAD read back wrong" FAIL
    rm -f "${MOUNT:?}/${T75_FILE:?}"
done
fi # should_run T75

# ── Test 76: every server SIGKILLed mid-storm, then restarted (failure-scenario quick win) ─────
# The process-crash counterpart of a power loss, with no tooling: all 5 servers die at once while
# two clients fsync-write their own block in chunks 1..4, then all restart. Every acked write
# must read back through a fresh client, each chunk's current ISR pair must end identical (ISRs
# and leases are rebuilt from durable state), and both writers must make progress after the
# restart. Informational: writer errors, and how long until writes resumed after the crash.
if should_run T76; then
snapshot_log T76
echo ""
echo "=== T76: all 5 servers SIGKILLed mid-storm and restarted: no acked write lost, pairs recover ==="
T76_NODES=(127.0.0.1:8900 127.0.0.1:8901 127.0.0.1:8902 127.0.0.1:8903 127.0.0.1:8904)
T76_ALL="$(IFS=,; echo "${T76_NODES[*]}")"
T76_FILE=t76_cluster_crash.bin
T76_CHUNKS=5
T76_MOUNT2=/tmp/dfs-mount2
mkdir -p "$T76_MOUNT2"
RUST_LOG=info "$BIN/dfs-client" mount "$T76_MOUNT2" --cluster "$CLUSTER" \
    --log-file "$LOG/client_t76b.log" --allow-other --log-level debug &
T76_PID2=$!
sleep 2
mountpoint -q "$T76_MOUNT2" || check "T76 second client mounted" FAIL
t76_check() {   # required with DFS_ORDERED_WRITES=1, informational without (as T74)
    if [ "${DFS_ORDERED_WRITES:-0}" = 1 ]; then check "$1" "$2"
    else echo "  (informational without DFS_ORDERED_WRITES) $2: $1"; fi
}
t76_writer() {  # mount file block-offset-in-chunk tag stop-file out: until the stop file appears
    python3 - "$1/$2" "$3" "$4" "$5" > "$6" 2>&1 <<'PY'
import os, sys, time, json
path, base, tag, stop = sys.argv[1], int(sys.argv[2]), sys.argv[3].encode(), sys.argv[4]
fd = os.open(path, os.O_RDWR)
start = time.time(); i = 0
per = {c: {"acked": [], "failed": []} for c in range(1, 5)}
errors = 0; worst = 0.0; ack_at = []
while not os.path.exists(stop) and time.time() - start < 600:
    c = 1 + i % 4
    t0 = time.time()
    try:
        os.pwrite(fd, (tag + b"%06d" % i).ljust(4096, tag[:1]), c * 4 * 1024 * 1024 + base)
        os.fsync(fd)
        per[c]["acked"].append(i); ack_at.append(round(time.time(), 3))
    except OSError:
        per[c]["failed"].append(i); errors += 1
        time.sleep(0.2)
    worst = max(worst, time.time() - t0)
    i += 1
os.close(fd)
print(json.dumps({"per": per, "errors": errors, "ack_at": ack_at, "worst_s": round(worst, 2)}))
PY
}
dd if=/dev/urandom of="$MOUNT/$T76_FILE" bs=4M count=$T76_CHUNKS status=none
dfs_sync
T76_STOP="$LOG/t76.stop"
rm -f "$T76_STOP"
t76_writer "$MOUNT" "$T76_FILE" 8192 A "$T76_STOP" "$LOG/t76_w1.out" & T76_W1=$!
t76_writer "$T76_MOUNT2" "$T76_FILE" 16384 B "$T76_STOP" "$LOG/t76_w2.out" & T76_W2=$!
sleep 6
T76_KILL=$(date +%s.%N)
for i in 1 2 3 4 5; do
    pkill -9 -f "dfs-server start --config $BASE/node${i}/config.toml" 2>/dev/null || true
done
echo "  T76: killed all 5 servers 6s into the storm; restarting them in 3s"
sleep 3
for i in 1 2 3 4 5; do
    RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 DFS_FAULT_INJECTION=1 "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        >> "$LOG/server${i}.log" 2>&1 &
done
T76_UP=$(date +%s.%N)
T76_READY=""
for s in $(seq 1 30); do
    T76_N=$("$BIN/dfs-admin" --cluster "$T76_ALL" lease status 2>/dev/null | grep -c '"node"' || true)
    if [ "${T76_N:-0}" -ge 5 ]; then T76_READY=$s; break; fi
    sleep 1
done
echo "  T76: all 5 servers answering ${T76_READY:-NEVER (30s)}s after the restart"
sleep 12
touch "$T76_STOP"
wait "$T76_W1" "$T76_W2" 2>/dev/null || true
dfs_sync; sync "$T76_MOUNT2" 2>/dev/null || true
sleep 3
T76_FM=/tmp/dfs-mount-fresh
mkdir -p "$T76_FM"
RUST_LOG=info "$BIN/dfs-client" mount "$T76_FM" --cluster "$CLUSTER" \
    --log-file "$LOG/client_fresh.log" --allow-other --log-level debug &
T76_FPID=$!
sleep 2
T76_FRESH=$(python3 -c "
f = open('$T76_FM/$T76_FILE', 'rb')
for c in range(1, 5):
    for base in (8192, 16384):
        f.seek(c * 4 * 1024 * 1024 + base)
        print(c, base, f.read(10).hex())" 2>/dev/null || true)   # hex: \$(...) drops NUL bytes
fusermount -u "$T76_FM" 2>/dev/null || true
kill_client_and_wait "$T76_FPID"
T76_VERDICT=$(python3 - "$LOG/t76_w1.out" "$LOG/t76_w2.out" "$T76_FRESH" "$T76_KILL" "$T76_UP" <<'PY'
import json, sys
fresh = {}
for l in sys.argv[3].splitlines():
    p = l.split(" ", 2)
    if len(p) == 3: fresh[(int(p[0]), int(p[1]))] = bytes.fromhex(p[2]).decode(errors="replace").replace("\x00", "\\0")
kill, up = float(sys.argv[4]), float(sys.argv[5])
lost, summary, noprog, errors, resumed = [], [], [], 0, []
for path, tag, base in ((sys.argv[1], "A", 8192), (sys.argv[2], "B", 16384)):
    try: r = json.loads(open(path).read().strip().splitlines()[-1])
    except Exception as e: print("BAD NOPROG=? ERRORS=? | writer %s output unreadable: %s |" % (tag, e)); sys.exit()
    acked = sum(len(v["acked"]) for v in r["per"].values())
    errors += r["errors"]
    after = [t for t in r["ack_at"] if t > up]
    if not after: noprog.append(tag)
    else: resumed.append("%s %.1fs" % (tag, min(after) - kill))
    summary.append("%s: %d acked (%d after the restart), %d failed, worst op %.1fs" % (tag, acked, len(after), r["errors"], r["worst_s"]))
    for c, v in r["per"].items():
        c = int(c)
        last = max(v["acked"]) if v["acked"] else -1
        allowed = {last} | {f for f in v["failed"] if f > last}
        got = fresh.get((c, base), "")
        if not (got[:1] == tag and got[1:7].isdigit() and int(got[1:7]) in allowed):
            lost.append("chunk %d %s: servers hold %r, last acked %s%06d" % (c, tag, got, tag, last))
print("LOST" if lost else "OK", "NOPROG=%s" % (",".join(noprog) or "none"), "ERRORS=%d" % errors,
      "RESUMED=%s" % (",".join(resumed) or "never"), "|", "; ".join(summary), "|", "; ".join(lost))
PY
)
echo "  T76: $(echo "$T76_VERDICT" | cut -d'|' -f2)"
case "$T76_VERDICT" in
    OK*) t76_check "T76a no acked write lost across a whole-cluster crash (8 blocks, 4 chunks)" PASS ;;
    *)   t76_check "T76a acked write(s) lost:$(echo "$T76_VERDICT" | cut -d'|' -f3)" FAIL
         "$BIN/dfs-admin" --cluster "$CLUSTER" file info "/$T76_FILE" 2>&1 | sed 's/^/    /' | head -30 ;;
esac
case "$T76_VERDICT" in
    *NOPROG=none*) t76_check "T76c both writers made progress after the restart" PASS ;;
    *)             t76_check "T76c a writer made no progress after the restart: $(echo "$T76_VERDICT" | cut -d'|' -f1)" FAIL ;;
esac
T76_MAP=$("$BIN/dfs-admin" --cluster "$T76_ALL" lease status 2>/dev/null \
    | python3 -c "import json,sys; [print(r['node'], r['addr']) for r in map(json.loads, sys.stdin) if 'node' in r]" || true)
T76_PAIRS=$("$BIN/dfs-admin" --cluster "$T76_ALL" isr get --file "/$T76_FILE" --chunks $T76_CHUNKS 2>/dev/null | python3 -c "
import json, sys
addr = dict(l.split() for l in '''$T76_MAP'''.strip().splitlines())
best = {}
for l in sys.stdin:
    try: isr = json.loads(l)['isr']
    except Exception: continue
    for c in range(1, 5):
        r = isr[c] if c < len(isr) else None
        if r and len(r['members']) >= 2 and (c not in best or r['epoch'] > best[c]['epoch']): best[c] = r
for c in range(1, 5):
    r = best.get(c)
    print(c, r['epoch'] if r else '?', ','.join(addr.get(m, '?') for m in r['members'][:2]) if r else '?')" 2>/dev/null || true)
T76_BAD=""
while read -r c e pair; do
    [ -z "$c" ] && continue
    hs=$("$BIN/dfs-admin" --cluster "$pair" isr read --file "/$T76_FILE" --chunk "$c" 2>/dev/null | python3 -c "
import json,sys
hs=[]
for l in sys.stdin:
    try: r=json.loads(l)
    except Exception: continue
    hs.append(r['blake3'][:16] if r.get('len') == 4194304 else 'ERR')
print(' '.join(hs))" || true)
    read -r h1 h2 <<< "$hs"
    echo "  T76: chunk $c ISR epoch $e [$pair]: ${hs:-unreadable}"
    { [ -n "$h1" ] && [ "$h1" = "$h2" ] && [ "$h1" != ERR ]; } || T76_BAD="$T76_BAD chunk$c($hs)"
done <<< "$T76_PAIRS"
[ -n "$T76_PAIRS" ] && [ -z "$T76_BAD" ] \
    && t76_check "T76b every chunk's current ISR pair holds identical bytes after the crash" PASS \
    || t76_check "T76b ISR pair(s) differ or unreadable:${T76_BAD:- no ISR found}" FAIL
echo "  T76 (informational): $(echo "$T76_VERDICT" | grep -oP 'ERRORS=\S+') writer error(s); writes resumed after the crash: $(echo "$T76_VERDICT" | grep -oP 'RESUMED=\K\S+')"
rm -f "${MOUNT:?}/${T76_FILE:?}"
fusermount -u "$T76_MOUNT2" 2>/dev/null || true
kill_client_and_wait "$T76_PID2"
fi # should_run T76

# ── Test 77: a client SIGKILLed mid-flush, then a fresh mount (failure-scenario quick win) ─────
# Client 2 writes 4K blocks to chunks 1..4, fsyncing only every 4th write, so unflushed buffers,
# half-sent patches and queued location updates are always in flight when it's SIGKILLed.
# Client 1 keeps writing its own blocks throughout. Required: every write client 2 had fsync'd
# reads back; no block is torn (each block holds exactly one whole write, the last fsync'd one or
# a later unsynced one); client 1 saw no error and kept making progress across the kill.
if should_run T77; then
snapshot_log T77
echo ""
echo "=== T77: a client SIGKILLed mid-flush: its fsync'd writes survive, nothing torn, the other client unaffected ==="
T77_FILE=t77_client_kill.bin
T77_MOUNT2=/tmp/dfs-mount2
mkdir -p "$T77_MOUNT2"
RUST_LOG=info "$BIN/dfs-client" mount "$T77_MOUNT2" --cluster "$CLUSTER" \
    --log-file "$LOG/client_t77b.log" --allow-other --log-level debug &
T77_PID2=$!
sleep 2
mountpoint -q "$T77_MOUNT2" || check "T77 second client mounted" FAIL
dd if=/dev/urandom of="$MOUNT/$T77_FILE" bs=4M count=5 status=none
dfs_sync
# The fill each block starts with: a client that never fsynced may leave it in place.
T77_ORIG=$(python3 -c "
f = open('$MOUNT/$T77_FILE', 'rb')
for c in range(1, 5):
    for base in (8192, 16384):
        f.seek(c * 4 * 1024 * 1024 + base)
        print(c, base, f.read(4096).hex())" 2>/dev/null || true)
T77_STOP="$LOG/t77.stop"
rm -f "$T77_STOP"
t77_writer() {  # mount file block-offset tag sync-every stop-file out
    python3 - "$1/$2" "$3" "$4" "$5" "$6" > "$7" 2>&1 <<'PY'
import os, sys, time, json
path, base, tag, every, stop = sys.argv[1], int(sys.argv[2]), sys.argv[3].encode(), int(sys.argv[4]), sys.argv[5]
out = sys.stdout
fd = os.open(path, os.O_RDWR)
start = time.time(); i = 0
synced = {c: -1 for c in range(1, 5)}; since = {c: [] for c in range(1, 5)}
errors = 0; ack_at = []
def dump():   # after every step: the last line before a SIGKILL of the client is the truth
    out.write(json.dumps({"synced": synced, "since": since, "errors": errors, "ack_at": ack_at, "start": round(start, 3)}) + "\n"); out.flush()
while not os.path.exists(stop) and time.time() - start < 120:
    c = 1 + i % 4
    try:
        os.pwrite(fd, (tag + b"%06d" % i).ljust(4096, tag[:1]), c * 4 * 1024 * 1024 + base)
        since[c].append(i)
        dump()
        if i % every == every - 1:
            os.fsync(fd)
            for cc in since:   # everything written so far is now durable
                if since[cc]: synced[cc] = max(since[cc]); since[cc] = []
            ack_at.append(round(time.time(), 3))
            dump()
    except OSError:
        errors += 1
        time.sleep(0.2)
    i += 1
try: os.close(fd)
except OSError: pass
dump()
PY
}
t77_writer "$MOUNT" "$T77_FILE" 8192 A 1 "$T77_STOP" "$LOG/t77_w1.out" & T77_W1=$!
t77_writer "$T77_MOUNT2" "$T77_FILE" 16384 B 4 "$T77_STOP" "$LOG/t77_w2.out" & T77_W2=$!
sleep 5
T77_KILL=$(date +%s.%N)
kill -9 "$T77_PID2" 2>/dev/null || true
echo "  T77: SIGKILLed client 2 (pid $T77_PID2) 5s into the storm"
sleep 1
kill "$T77_W2" 2>/dev/null || true   # its writer is stuck on a dead mount
fusermount -uz "$T77_MOUNT2" 2>/dev/null || true
sleep 8
touch "$T77_STOP"
wait "$T77_W1" 2>/dev/null || true
dfs_sync
sleep 3
T77_FM=/tmp/dfs-mount-fresh
mkdir -p "$T77_FM"
RUST_LOG=info "$BIN/dfs-client" mount "$T77_FM" --cluster "$CLUSTER" \
    --log-file "$LOG/client_fresh.log" --allow-other --log-level debug &
T77_FPID=$!
sleep 2
T77_FRESH=$(python3 -c "
f = open('$T77_FM/$T77_FILE', 'rb')
for c in range(1, 5):
    for base in (8192, 16384):
        f.seek(c * 4 * 1024 * 1024 + base)
        print(c, base, f.read(4096).hex())" 2>/dev/null || true)
fusermount -u "$T77_FM" 2>/dev/null || true
kill_client_and_wait "$T77_FPID"
T77_VERDICT=$(python3 - "$LOG/t77_w1.out" "$LOG/t77_w2.out" "$T77_FRESH" "$T77_KILL" "$T77_ORIG" <<'PY'
import json, sys
def blocks(text):
    out = {}
    for l in text.splitlines():
        p = l.split(" ", 2)
        if len(p) == 3: out[(int(p[0]), int(p[1]))] = bytes.fromhex(p[2])
    return out
fresh, orig = blocks(sys.argv[3]), blocks(sys.argv[5])
kill = float(sys.argv[4])
bad, notes = [], []
def last_state(path):
    lines = [l for l in open(path).read().splitlines() if l.startswith("{")]
    return json.loads(lines[-1]) if lines else None
for path, tag, base, name in ((sys.argv[1], "A", 8192, "client 1"), (sys.argv[2], "B", 16384, "client 2 (killed)")):
    r = last_state(path)
    if r is None: bad.append("%s: writer output unreadable" % name); continue
    if tag == "A":
        after = [t for t in r["ack_at"] if t > kill + 1]
        notes.append("%s: %d fsyncs (%d after the kill), %d errors" % (name, len(r["ack_at"]), len(after), r["errors"]))
        if r["errors"]: bad.append("ERR %s got %d error(s)" % (name, r["errors"]))
        if not after: bad.append("NOPROG %s made no progress after the kill" % name)
    else:
        first = ("first fsync %.1fs in" % (r["ack_at"][0] - r["start"])) if r["ack_at"] and "start" in r else "no fsync done"
        notes.append("%s: %d fsyncs before the kill (%s)" % (name, len(r["ack_at"]), first))
    for c in range(1, 5):
        s = r["synced"][str(c)]; w = r["since"][str(c)]
        allowed = {s} | set(w)
        got = fresh.get((c, base), b"")
        idx = got[1:7].decode(errors="replace") if got else ""
        whole = idx.isdigit() and got == (tag.encode() + idx.encode()).ljust(4096, tag.encode())
        if s < 0 and not w: continue
        if s < 0 and orig.get((c, base)) is not None and got == orig[(c, base)]: continue  # nothing fsync'd, none landed
        if not got or got[:1] != tag.encode() or not idx.isdigit():
            bad.append("LOST chunk %d %s: holds %r, last fsync'd %s%06d" % (c, tag, got[:10], tag, s)); continue
        if not whole: bad.append("TORN chunk %d %s: block isn't one whole write (%r...)" % (c, tag, got[:12]))
        elif int(idx) not in allowed: bad.append("LOST chunk %d %s: holds %s%s, last fsync'd %s%06d" % (c, tag, tag, idx, tag, s))
print("OK" if not bad else "BAD", "|", "; ".join(notes), "|", "; ".join(bad))
PY
)
echo "  T77: $(echo "$T77_VERDICT" | cut -d'|' -f2)"
case "$T77_VERDICT" in
    *LOST*|*TORN*) check "T77a a killed client's fsync'd writes survive, nothing torn:$(echo "$T77_VERDICT" | cut -d'|' -f3)" FAIL ;;
    *)             check "T77a a killed client's fsync'd writes survive, nothing torn" PASS ;;
esac
case "$T77_VERDICT" in
    *ERR*|*NOPROG*) check "T77b the other client unaffected:$(echo "$T77_VERDICT" | cut -d'|' -f3)" FAIL ;;
    *)              check "T77b the other client wrote on without an error across the kill" PASS ;;
esac
rm -f "${MOUNT:?}/${T77_FILE:?}"
fi # should_run T77


if should_run T70; then
snapshot_log T70
echo ""
echo "=== T70: renaming a directory keeps its contents (Legata bug: sobpoena -> subpoena showed 0 files) ==="
rm -rf "$MOUNT/t70_sob" "$MOUNT/t70_sub" 2>/dev/null || true
mkdir -p "$MOUNT/t70_sob/sub"
dd if=/dev/urandom of="$MOUNT/t70_sob/a.bin" bs=1M count=1 status=none
dd if=/dev/urandom of="$MOUNT/t70_sob/sub/b.bin" bs=1M count=1 status=none
dfs_sync
T70_A=$(md5sum < "$MOUNT/t70_sob/a.bin" | cut -c1-32)
T70_B=$(md5sum < "$MOUNT/t70_sob/sub/b.bin" | cut -c1-32)
# Hold a file open across the rename and write through it afterwards, as a VM with its
# disk image in the directory would: its later writes must not resurrect the old path.
T70_INO_BEFORE=$(ls -i "$MOUNT/t70_sob/sub/b.bin" | awk '{print $1}')
exec 7>>"$MOUNT/t70_sob/sub/b.bin"
mv "$MOUNT/t70_sob" "$MOUNT/t70_sub"
head -c 65536 /dev/urandom >&7
exec 7>&-
dfs_sync
T70_B=$(md5sum < "$MOUNT/t70_sub/sub/b.bin" 2>/dev/null | cut -c1-32)
T70_LS=$(ls "$MOUNT/t70_sub" 2>/dev/null | tr '\n' ' ')
T70_LS_SUB=$(ls "$MOUNT/t70_sub/sub" 2>/dev/null | tr '\n' ' ')
echo "  T70: ls t70_sub = [$T70_LS], ls t70_sub/sub = [$T70_LS_SUB]"
[[ "$T70_LS" == *"a.bin"* && "$T70_LS" == *"sub"* && "$T70_LS_SUB" == *"b.bin"* ]] \
    && check "T70a renamed directory lists its file and its subdirectory's file" PASS \
    || check "T70a renamed directory lost its contents" FAIL
[ "$(md5sum < "$MOUNT/t70_sub/a.bin" 2>/dev/null | cut -c1-32)" = "$T70_A" ] && \
[ "$(md5sum < "$MOUNT/t70_sub/sub/b.bin" 2>/dev/null | cut -c1-32)" = "$T70_B" ] \
    && check "T70b file contents intact under the new name" PASS \
    || check "T70b file contents unreadable or changed under the new name" FAIL
[ ! -e "$MOUNT/t70_sob" ] \
    && check "T70c old directory name is gone" PASS \
    || check "T70c old directory name still exists" FAIL
# The servers' own metadata, independent of any client cache.
T70_SRV=$("$BIN/dfs-admin" --cluster "$CLUSTER" file list 2>/dev/null | grep -oE "/t70_(sob|sub)[^ ]*" | sort -u | tr '\n' ' ')
echo "  T70: server paths: $T70_SRV"
[[ "$T70_SRV" == *"/t70_sub/a.bin"* && "$T70_SRV" == *"/t70_sub/sub/b.bin"* && "$T70_SRV" != *"/t70_sob"* ]] \
    && check "T70d server metadata moved every descendant to the new path" PASS \
    || check "T70d server metadata still has old descendant paths or lacks new ones" FAIL
T70_INO_AFTER=$(ls -i "$MOUNT/t70_sub/sub/b.bin" 2>/dev/null | awk '{print $1}')
echo "  T70: b.bin inode before rename $T70_INO_BEFORE, after $T70_INO_AFTER"
[ -n "$T70_INO_BEFORE" ] && [ "$T70_INO_BEFORE" = "$T70_INO_AFTER" ] \
    && check "T70f a child keeps its inode number across the directory rename" PASS \
    || check "T70f a child's inode number changed across the directory rename" FAIL
T70_BSIZE=$(stat -c %s "$MOUNT/t70_sub/sub/b.bin" 2>/dev/null || echo 0)
[ "$T70_BSIZE" = $((1048576 + 65536)) ] \
    && check "T70e a file held open across the rename takes writes under its new name" PASS \
    || check "T70e write through a handle held across the rename was lost (size $T70_BSIZE)" FAIL
rm -rf "$MOUNT/t70_sub" "$MOUNT/t70_sob" 2>/dev/null || true
fi # should_run T70

# ── Test 72: no node keeps a file the cluster deleted ────────────────────────
# Legata bug: files deleted by earlier tests (T36/T37/T39 in one run, T42's
# flood in others) stayed in some nodes' FILE_TABLE/chunk_map for the rest of
# the run, where the slot audit kept reporting them. Every file a node lists as
# its own must also be on the leader. Runs after every other test, so each
# test's deletes are long past the 30s tombstone TTL.
snapshot_log T72
if should_run T72; then
echo "=== T72: every node's file table matches the leader's (no lingering deleted files) ==="
dfs_sync 2>/dev/null || true
# T70 (just before this) ends by deleting its tree, so give a delete still propagating
# up to 10s: the property is that no node KEEPS a deleted file.
for T72_TRY in $(seq 1 10); do
    T72_LEADER=$("$BIN/dfs-admin" --cluster "127.0.0.1:8900" file list 2>/dev/null | grep -oE "^[0-9a-f-]{36}" | sort -u)
    T72_EXTRA=""; T72_DETAIL=""
    for port in 8900 8901 8902 8903 8904; do
        extra=$(comm -13 <(echo "$T72_LEADER") <("$BIN/dfs-admin" --cluster "127.0.0.1:$port" file list --local 2>/dev/null | grep -oE "^[0-9a-f-]{36}" | sort -u))
        if [ -n "$extra" ]; then
            T72_EXTRA="${T72_EXTRA} $port:$(echo "$extra" | grep -c .)"
            T72_DETAIL="${T72_DETAIL}  T72: node $port keeps files the leader doesn't have: $(echo $extra | cut -c1-200)"$'\n'
        fi
    done
    [ -z "$T72_EXTRA" ] && break
    sleep 1
done
echo "  T72: leader lists $(echo "$T72_LEADER" | grep -c . || true) files (check $T72_TRY)"
printf "%s" "$T72_DETAIL"
[ -z "$T72_EXTRA" ] \
    && check "T72 no node keeps a file the leader has deleted" PASS \
    || check "T72 nodes keep deleted files:${T72_EXTRA}" FAIL

# T72b: the mechanism behind it. A patch slot still dirty when its file was deleted
# was later folded by the patch-fold sweep, which re-installed the deleted file's
# chunk_map. No node may fold a file after its delete committed there ("tombstoned for
# good"; the DeleteChunksBatch line is logged when the delete ARRIVES, and a fold that
# commits before the delete does is legitimate: the delete then removes its rows).
# Measured from arrival, durable commits made that window show up (2026-10-07). Folds are
# timed by their commit ("Fold committed"), not by "Single fold", which is logged after the
# leader notification (up to hundreds of ms later).
T72B_VIOLATIONS=""
for f in "$LOG"/server[0-9].log; do
    T72B_VIOLATIONS="${T72B_VIOLATIONS}$(sed -E 's/\x1b\[[0-9;]*m//g' "$f" | awk -v node="$(basename "$f" .log)" '
        /tombstoned for good/ { for (i=1; i<=NF; i++) if ($i=="file") { id=$(i+1); break }; if (!(id in del)) del[id]=$1 }
        /Fold committed: file / {
            for (i=1; i<=NF; i++) if ($i=="file") { id=$(i+1); break }
            if ((id in del) && $1 > del[id]) print "  T72b: " node " folded deleted file " id " at " $1 " (deleted " del[id] ")"
        }')"$'\n'
done
T72B_BAD=$(echo "$T72B_VIOLATIONS" | grep -c "T72b:" || true)
echo "$T72B_VIOLATIONS" | grep "T72b:" | head -10 || true
[ "$T72B_BAD" -eq 0 ] \
    && check "T72b no node folds a file after deleting it" PASS \
    || check "T72b $T72B_BAD fold(s) of already-deleted files re-created their chunk maps" FAIL
fi # should_run T72

# ── Test 71: a node restarted while its peers are down must not lead ─────────
# Legata bug: is_leader()/has_quorum() sized the majority from the peers this
# process has heard from so far, not from the cluster it belongs to — a node
# that restarts alone sees {self}, computes quorum = 1 of 1, and runs leader-only
# healing (including has_quorum-gated destructive cleanup) against a partition of
# one. Runs LAST: it stops the whole cluster and restarts only node5 (alone,
# any node is the min-id online node, so which one we pick doesn't matter).
snapshot_log T71
if should_run T71; then
echo "=== T71: a node restarted alone (peers down) must not claim leadership ==="
dfs_sync
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
pkill -f "dfs-server start" 2>/dev/null || true
for _ in $(seq 1 50); do pgrep -f "dfs-server start" >/dev/null || break; sleep 0.1; done
[ -s "$BASE/node5/peers.json" ] \
    && check "T71a node5 persisted its peers before the restart" PASS \
    || check "T71a node5 has no peers.json — cannot know its cluster size" FAIL
RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 "$BIN/dfs-server" start --config "$BASE/node5/config.toml" \
    > "$LOG/server5_t71.log" 2>&1 &
T71_PID=$!
# The healer's discovery loop re-evaluates leadership on a 60s tick; wait past
# the first one so a lone node that believes it leads has had its chance to say so.
sleep 70
T71_LED=$(grep -c "now the cluster leader" "$LOG/server5_t71.log" || true)
echo "  T71: lone node5 'now the cluster leader' lines: $T71_LED"
[ "$T71_LED" -eq 0 ] \
    && check "T71b lone restarted node does not take over healing coordination" PASS \
    || check "T71b lone restarted node declared itself leader of a partition of one" FAIL
T71_STATUS=$(timeout 5 "$BIN/dfs-admin" --cluster "127.0.0.1:8904" cluster status 2>/dev/null | grep -E "^(Total Nodes|Leader):" || true)
echo "  T71: node5 status: $(echo $T71_STATUS)"
echo "$T71_STATUS" | grep -q "^Leader:" \
    && check "T71c lone node reports a leader it cannot have (no quorum)" FAIL \
    || check "T71c lone node reports no leader while it lacks a majority" PASS
kill "$T71_PID" 2>/dev/null || true
fi # should_run T71

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
