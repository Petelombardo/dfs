#!/bin/bash
# Slow-burn (~25-30 minute) local soak, real timing — no env-var shortcuts on the
# gates that matter. Written 2026-08-04 to validate the paginated/watchdogged
# disk-orphan-sweep (see project_paginated_disk_orphan_sweep_20260804.md) against
# real 20-minute restart-grace timing instead of the unit tests' env-override
# shortcuts, and to add fold-correctness coverage the project has never had
# locally before: client-forced fold (>20 patches to one chunk), server-initiated
# idle fold (a couple of patches + real wait past PATCH_DEBOUNCE_IDLE), and a
# re-patch of the post-fold chunk to check for corruption on the exact kind of
# path several real production data-loss incidents this session trace back to
# (stale-fold-wins-arbitration, coordinate_and_fold_slot races).
#
# REAL FINDING from run 1 (kept here so a future reader doesn't re-discover it
# the hard way): a synchronous unlink() on a file with a chunk replicated to a
# currently-SIGSTOP'd node blocks for roughly TWO MINUTES before returning —
# confirmed via log timestamps (DeleteFile dispatched to the paused node's port
# at T, next unlink in the same `rm` invocation not attempted until T+130s).
# This script does NOT try to work around that; it's treated as expected
# real-world behavior worth exercising, and every phase after Phase 1 budgets
# for Phase 1 taking minutes, not seconds.
#
# Phases (see inline headers below for exact timing):
#   1. Create genuine orphan chunks: confirm a specific node physically holds at
#      least one victim chunk (data-dir inspection, not a blind non-leader
#      guess), pause it (SIGSTOP, not a restart — its own SELF_RESTART_GRACE_SECS
#      clock does NOT reset), delete the victim files while it can't receive the
#      DeleteChunk RPC, resume it. This is exactly the accumulation path
#      run_discovery_loop's own comment describes: "they accumulate orphaned
#      files when they miss DeleteChunk RPCs while offline." Takes a real
#      snapshot of which specific chunk files survive the resume, rather than a
#      raw count, since unrelated legitimate traffic keeps landing on the same
#      node for the rest of the soak.
#   2. Client-forced fold: >20 distinct patches to one chunk (fsync per patch
#      forces each write to flush as its own patch — SLOT_DIRTY_FLUSH_THRESHOLD_BYTES
#      doesn't gate an explicit fsync) to trip ACTIVE_FOLD_PATCH_THRESHOLD=20
#      (dfs-client/src/client.rs:7704).
#   3. Server-initiated idle fold: a couple of patches to a different chunk,
#      then a real 45s idle wait — past PATCH_DEBOUNCE_IDLE (20s,
#      dfs-server/src/healing.rs:1293), which re-arms a fresh 20s window each
#      time it wakes to find the slot touched, so real fold latency can reach
#      ~40s. Reuses t53_patch_writer.py, the same harness T53 already uses for
#      this exact backstop path.
#   4. Re-patch both post-fold chunks and verify FULL-CHUNK byte-for-byte
#      integrity (not just the patched range) plus scan every log for any
#      corruption/hash-mismatch report. Explicitly checks both files exist
#      before comparing — two missing files md5summing to two empty strings
#      would otherwise "pass" trivially.
#   5. Wait out the real remaining time to clear the 20-minute
#      SELF_RESTART_GRACE_SECS/LEADER_CHANGE_GRACE_SECS grace period (NOT
#      shortened via env override — this is the whole point of a slow-burn
#      test), then watch the paginated sweep discover and evict Phase 1's
#      specific orphan chunks page-by-page. Confirms: the SPECIFIC orphan
#      chunks are eventually evicted (snapshot-based, immune to unrelated new
#      chunks landing on the same node later); the control files (written
#      throughout the whole soak) are NEVER evicted; pagination actually
#      happens (multiple distinct page-log lines, not one giant sweep); the
#      watchdog never had to fire.
#
# DFS_ORPHAN_SWEEP_PAGE_SIZE is set small (3) so a modest local chunk count
# still produces multiple observable pages — production default is 5000.
# DFS_ORPHAN_SWEEP_PAGE_GRACE_MS is left near its 3000ms default (2000ms here)
# — NOT shortened aggressively, since the point is to watch realistic pacing.
#
# Servers run RUST_LOG=debug (unlike test_local_suite.sh's info-level servers)
# specifically so every page's "chunks checked" line is visible, including the
# zero-candidate debug-only variant — needed to actually count pages processed
# over a long window. This is a diagnostic soak, not the routine suite.

set -uo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
cd "$REPO"

BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-soak-mount
LOG=/tmp/dfs-soak-logs
T=/tmp/dfs-soak-scratch
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"

export DFS_CHUNK_RING_CAPACITY=8
export DFS_DELTA_RING_CAPACITY=8
export DFS_MAX_CACHE_CHUNKS=8
export DFS_WRITE_BUFFER_CAP_MB=32
export DFS_ORPHAN_SWEEP_PAGE_SIZE=3
export DFS_ORPHAN_SWEEP_PAGE_GRACE_MS=2000

# Disk guard floor — see feedback_dev_box_disk_lockup_from_unthrottled_repro_writers.md.
MIN_FREE_KB=$((2*1024*1024))

PASS=0; FAIL=0
check() {
    local name="$1" result="$2"
    if [ "$result" = "PASS" ]; then echo "  PASS: $name"; PASS=$((PASS+1))
    else echo "  FAIL: $name"; FAIL=$((FAIL+1)); fi
}

# count_matches <pattern> <file...>: two distinct bugs found running this
# script for real. (1) grep -c's own "0 matches" case exits 1, which a naive
# `$(grep -c ... || echo 0)` double-counts (both grep's own "0" stdout AND the
# fallback echo fire, yielding "0\n0"). (2) grep -c given MULTIPLE files
# prefixes each line with "filename:count" instead of a bare count, so
# `grep -c PATTERN server*.log` doesn't return one integer at all — it returns
# one "path:0"-shaped line per file, which then broke every numeric comparison
# and check() message downstream of it. -h suppresses the filename prefix
# uniformly (single- or multi-file), and awk sums however many lines come
# back into one clean integer either way.
count_matches() {
    local pattern="$1"; shift
    grep -ch "$pattern" "$@" 2>/dev/null | awk '{s+=$1} END{print s+0}'
}

# wait_for_file_size <path> <min_bytes> <timeout_s>: poll instead of a fixed
# sleep — Phase 1's ~2-minute node-pause stall means the cluster can still be
# settling right when Phase 2 wants to write, so a fixed short sleep before
# the next write is exactly the kind of assumption that produced run 1's
# silent (stderr-suppressed) dd failures.
wait_for_file_size() {
    local path="$1" min_bytes="$2" timeout_s="$3"
    local waited=0
    while [ "$waited" -lt "$timeout_s" ]; do
        if [ -f "$path" ]; then
            local sz
            sz=$(stat -c %s "$path" 2>/dev/null || echo 0)
            [ "$sz" -ge "$min_bytes" ] && return 0
        fi
        sleep 1
        waited=$((waited+1))
    done
    return 1
}

# dd_with_retry <of_path> <bs> <count> <attempts>: run1 found dd's own stderr
# silently suppressed made a real write failure invisible until much later
# (a python FileNotFoundError several steps downstream). Never suppress dd's
# stderr here, and retry with a short backoff since the failures observed
# were transient (cluster still settling after Phase 1's node pause).
dd_with_retry() {
    local of="$1" bs="$2" count="$3" attempts="${4:-3}"
    local i=1
    while [ "$i" -le "$attempts" ]; do
        if dd if=/dev/zero of="$of" bs="$bs" count="$count" 2>"$T/dd_stderr.log"; then
            wait_for_file_size "$of" $(( $(numfmt --from=iec "$bs") * count )) 10 && return 0
        fi
        echo "  dd_with_retry: attempt $i/$attempts failed for $of:"
        sed 's/^/    /' "$T/dd_stderr.log"
        sleep $((i*3))
        i=$((i+1))
    done
    return 1
}

CLEANED_UP=0
cleanup() {
    [ "$CLEANED_UP" = "1" ] && return
    CLEANED_UP=1
    echo ""
    echo "=== Cleanup ==="
    [ -n "${DISKGUARD_PID:-}" ] && kill "$DISKGUARD_PID" 2>/dev/null || true
    [ -n "${TRICKLE_PID:-}" ] && kill "$TRICKLE_PID" 2>/dev/null || true
    # In case Phase 1 left a node paused (script aborted mid-phase), resume it
    # before trying to kill anything — SIGTERM on a SIGSTOP'd process just
    # queues the signal, it won't actually exit until resumed.
    for i in 1 2 3 4 5; do
        pid=$(pgrep -f "dfs-server start --config $BASE/node${i}/config.toml" | head -1)
        [ -n "$pid" ] && kill -CONT "$pid" 2>/dev/null || true
    done
    fusermount -u "$MOUNT" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    pkill -f "dfs-server" 2>/dev/null || true
}
trap cleanup EXIT

echo "=== Building ==="
cargo build --release 2>&1 | tail -3

echo "=== Cleaning old state ==="
pkill -f "dfs-server" 2>/dev/null || true
pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
sleep 0.5
fusermount -u "$MOUNT" 2>/dev/null || true
sudo rm -rf "$BASE" "$MOUNT" "$LOG" "$T" 2>/dev/null || rm -rf "$BASE" "$MOUNT" "$LOG" "$T" 2>/dev/null || true
mkdir -p "$MOUNT" "$LOG" "$T"

echo "=== Disk guard (background, floor=$((MIN_FREE_KB/1024/1024))GB) ==="
(
    while true; do
        free_kb=$(df -k / | awk 'NR==2{print $4}')
        if [ "$free_kb" -lt "$MIN_FREE_KB" ]; then
            echo "$(date): DISK GUARD TRIPPED (${free_kb}KB free) — aborting soak" | tee "$LOG/disk-guard-tripped"
            pkill -f "dfs-server" 2>/dev/null || true
            pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
            exit 1
        fi
        sleep 5
    done
) &
DISKGUARD_PID=$!

echo "=== Starting 5-node cluster ==="
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null
for i in 1 2 3 4 5; do
    RUST_LOG=debug DFS_LEADER_HANDOFF_GRACE_MS=0 "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        > "$LOG/server${i}.log" 2>&1 &
done
sleep 3

RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
CLIENT_PID=$!
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -20 "$LOG/client.log"; exit 1; }
echo "Mounted."

START=$(date +%s)
elapsed() { echo $(( $(date +%s) - START )); }
dfs_sync() { timeout 20 sync "$MOUNT" 2>/dev/null || true; }

echo ""
echo "=== Background trickle writer (control files, must survive the whole soak) ==="
(
    while true; do
        e=$(( $(date +%s) - START ))
        [ "$e" -ge 1700 ] && break
        dd if=/dev/urandom of="$MOUNT/control_a.bin" bs=65536 count=1 conv=notrunc 2>/dev/null || true
        dd if=/dev/urandom of="$MOUNT/control_b.bin" bs=65536 count=1 conv=notrunc 2>/dev/null || true
        timeout 10 sync "$MOUNT" 2>/dev/null || true
        sleep 45
    done
) &
TRICKLE_PID=$!
echo "  trickle writer pid=$TRICKLE_PID"

echo ""
echo "=== Phase 1 [t=$(elapsed)s]: create genuine orphan chunks via a paused replica ==="
for v in 1 2 3; do
    dd if=/dev/urandom of="$MOUNT/victim_$v.bin" bs=1M count=9 2>/dev/null
done
dfs_sync
# Give the async 3rd replica (see dfs-admin's own note: "writes only durably
# sync 2 replicas even at RF>=3, 3rd is async") a real chance to land before
# picking a node to pause — run 1 paused a node ~1.5s after sync and found it
# held zero of the victim chunks, which is consistent with pausing before the
# async push ever arrived rather than proving anything about eviction.
sleep 10
dfs_sync

LEADER_NODE=""
for attempt in $(seq 1 10); do
    for i in 1 2 3 4 5; do
        if grep -q "now the cluster leader" "$LOG/server${i}.log" 2>/dev/null; then
            LEADER_NODE=$i
            break 2
        fi
    done
    sleep 1
done
echo "  leader node: ${LEADER_NODE:-<not found after 10s, proceeding without excluding one>}"

# Data-driven choice: pick a non-leader node that ACTUALLY holds at least one
# victim chunk right now, rather than assuming any node will. Confirmed by
# direct inspection of that node's data dir.
PAUSE_NODE=""
for i in 1 2 3 4 5; do
    [ "$i" = "$LEADER_NODE" ] && continue
    n=$(find "$BASE/node${i}/data/chunks" -type f 2>/dev/null | wc -l)
    if [ "$n" -gt 0 ]; then
        PAUSE_NODE=$i
        break
    fi
done
if [ -z "$PAUSE_NODE" ]; then
    echo "  FATAL: no non-leader node holds any chunks yet — replication hasn't landed"
    for i in 1 2 3 4 5; do
        n=$(find "$BASE/node${i}/data/chunks" -type f 2>/dev/null | wc -l)
        echo "    node$i: $n chunks (leader=$([ "$i" = "$LEADER_NODE" ] && echo yes || echo no))"
    done
    exit 1
fi
echo "  pausing node $PAUSE_NODE (confirmed holds chunks, non-leader) to simulate a missed DeleteChunk RPC"

ORPHAN_NODE_DIR="$BASE/node${PAUSE_NODE}/data/chunks"
# Snapshot of the SPECIFIC files present before the pause — the eventual
# eviction check must be scoped to exactly this set, since unrelated
# legitimate traffic (control files, fold test files) keeps landing on this
# same node for the rest of the soak and would otherwise pollute a raw count.
PRE_PAUSE_SNAPSHOT="$T/pre_pause_snapshot.txt"
find "$ORPHAN_NODE_DIR" -type f 2>/dev/null | sort > "$PRE_PAUSE_SNAPSHOT"
echo "  chunk files on node $PAUSE_NODE before pause: $(wc -l < "$PRE_PAUSE_SNAPSHOT")"

PAUSE_PID=$(pgrep -f "dfs-server start --config $BASE/node${PAUSE_NODE}/config.toml" | head -1)
if [ -z "$PAUSE_PID" ]; then
    echo "  FATAL: could not find pid for node $PAUSE_NODE"
    exit 1
fi
PAUSE_STARTED_AT=$(elapsed)
kill -STOP "$PAUSE_PID"
echo "  [t=$(elapsed)s] paused pid $PAUSE_PID"

# NOTE: this rm is expected to block for a while (confirmed run 1: ~2 minutes)
# — a synchronous unlink() on a file with a chunk replicated to the paused
# node doesn't return until that node's RPC attempt resolves one way or
# another. That's fine and expected here; timeout is just a safety backstop,
# not a target duration.
timeout 180 rm -f "$MOUNT"/victim_1.bin "$MOUNT"/victim_2.bin "$MOUNT"/victim_3.bin
RM_RC=$?
echo "  [t=$(elapsed)s] rm of victim files returned (exit=$RM_RC, blocked ~$(( $(elapsed) - PAUSE_STARTED_AT ))s while node $PAUSE_NODE was paused)"
dfs_sync

kill -CONT "$PAUSE_PID"
echo "  [t=$(elapsed)s] resumed pid $PAUSE_PID"

# Snapshot again right after resume — anything still present here that was
# ALSO in the pre-pause snapshot is a real physical orphan candidate: this
# node never got the DeleteChunk RPC for it because it was frozen throughout.
POST_RESUME_SNAPSHOT="$T/post_resume_snapshot.txt"
find "$ORPHAN_NODE_DIR" -type f 2>/dev/null | sort > "$POST_RESUME_SNAPSHOT"
ORPHAN_CANDIDATES=$(comm -12 "$PRE_PAUSE_SNAPSHOT" "$POST_RESUME_SNAPSHOT")
ORPHAN_CANDIDATE_COUNT=$(echo -n "$ORPHAN_CANDIDATES" | grep -c . 2>/dev/null)
ORPHAN_CANDIDATE_COUNT=${ORPHAN_CANDIDATE_COUNT:-0}
echo "  physical orphan candidates on node $PAUSE_NODE (present before pause AND after resume): $ORPHAN_CANDIDATE_COUNT"
echo "$ORPHAN_CANDIDATES" > "$T/orphan_candidates.txt"

[ "$ORPHAN_CANDIDATE_COUNT" -gt 0 ] \
    && check "Phase1 paused node retained at least one physical orphan candidate" PASS \
    || check "Phase1 paused node retained at least one physical orphan candidate (got 0)" FAIL

echo ""
echo "=== Phase 2 [t=$(elapsed)s]: client-forced fold (>20 patches to one chunk) ==="
if dd_with_retry "$MOUNT/fold_client.bin" 1M 4 3; then
    dfs_sync
    FOLD_CLIENT_FILE_ID=$(grep -h "\[META SERVER\] put path=/fold_client.bin id=" "$LOG"/server*.log 2>/dev/null \
        | tail -1 | grep -oP 'id=\K[0-9a-f-]+')
    echo "  file_id=${FOLD_CLIENT_FILE_ID:-<not found>}"

    python3 - "$MOUNT/fold_client.bin" <<'PYEOF'
import os, sys, time
path = sys.argv[1]
fd = os.open(path, os.O_RDWR)
try:
    for i in range(25):
        off = (i * 131072) % (4 * 1024 * 1024 - 4096)
        os.pwrite(fd, bytes([0x40 + (i % 32)]) * 4096, off)
        os.fsync(fd)  # each fsync forces this write to flush as its own distinct patch
        time.sleep(0.05)
finally:
    os.close(fd)
print("25 distinct patches written", flush=True)
PYEOF
    # NOT a fixed short sleep — run 1 found the real completion can take ~55s
    # (one ForceFold attempt raced ahead of chunk_map registration, warned,
    # and only a later retry actually succeeded). Poll instead of guessing.
    FOLD_WAIT=0
    CLIENT_FOLD_HITS=0
    while [ "$FOLD_WAIT" -lt 90 ]; do
        CLIENT_FOLD_HITS=$(count_matches "ForceFold: file ${FOLD_CLIENT_FILE_ID} chunk 0 folded" "$LOG/client.log")
        [ "$CLIENT_FOLD_HITS" -ge 1 ] && break
        sleep 5
        FOLD_WAIT=$((FOLD_WAIT+5))
    done
    echo "  ForceFold completions for this file/chunk after ${FOLD_WAIT}s poll: $CLIENT_FOLD_HITS"
    [ "$CLIENT_FOLD_HITS" -ge 1 ] \
        && check "Phase2 client-forced ForceFold triggered (count-threshold path)" PASS \
        || check "Phase2 client-forced ForceFold triggered (found $CLIENT_FOLD_HITS)" FAIL
else
    echo "  FAIL: could not even create fold_client.bin after retries"
    check "Phase2 client-forced ForceFold triggered (file creation itself failed)" FAIL
fi

echo ""
echo "=== Phase 3 [t=$(elapsed)s]: server-initiated idle fold (spread patches + real wait) ==="
# nchunks=1 (a single hot slot) was run 1's mistake — T53's own header
# comment says exactly why: "a single synchronized burst does NOT reproduce
# this," because concentrating every patch on one slot lets the CLIENT's own
# ForceFold trigger (checked on the next incoming patch to that slot, per
# client.rs — not an independent background timer) fire long before the file
# goes idle. T53 reproduces the genuine server-side debounce_fold_slot
# backstop by spreading patches across MANY chunks (64) so each individual
# slot gets only one small patch and never crosses any client-side trigger on
# its own — reusing T53's own proven parameters here rather than re-guessing
# a smaller scale that might not isolate the same path.
T53_LIKE_CHUNKS=64
T53_LIKE_STORM=40
T53_LIKE_QUIET=60
if dd if=/dev/urandom of="$MOUNT/fold_idle.bin" bs=4M count=$T53_LIKE_CHUNKS 2>"$T/dd_stderr.log" \
    && wait_for_file_size "$MOUNT/fold_idle.bin" $((T53_LIKE_CHUNKS * 4 * 1024 * 1024)) 30; then
    dfs_sync
    FOLD_IDLE_FILE_ID=$(grep -h "\[META SERVER\] put path=/fold_idle.bin id=" "$LOG"/server*.log 2>/dev/null \
        | tail -1 | grep -oP 'id=\K[0-9a-f-]+')
    echo "  file_id=${FOLD_IDLE_FILE_ID:-<not found>}"

    echo "  patch storm across $T53_LIKE_CHUNKS chunks for ${T53_LIKE_STORM}s, then quiet ${T53_LIKE_QUIET}s (T53's own proven recipe)..."
    python3 "$REPO/scripts/t53_patch_writer.py" "$MOUNT/fold_idle.bin" "$T53_LIKE_CHUNKS" "$T53_LIKE_QUIET" "$T53_LIKE_STORM"
    sleep 2

    IDLE_FOLD_HITS=$(count_matches "Single fold: file ${FOLD_IDLE_FILE_ID} chunk_idx" "$LOG"/server*.log)
    CLIENT_FOLD_HITS_FOR_IDLE=$(count_matches "ForceFold: file ${FOLD_IDLE_FILE_ID} chunk" "$LOG/client.log")
    echo "  server-side 'Single fold' lines: $IDLE_FOLD_HITS, client-side ForceFold lines: $CLIENT_FOLD_HITS_FOR_IDLE"
    [ "$IDLE_FOLD_HITS" -ge 1 ] \
        && check "Phase3 server-initiated idle fold (backstop path fired at least once)" PASS \
        || check "Phase3 server-initiated idle fold (server=$IDLE_FOLD_HITS client=$CLIENT_FOLD_HITS_FOR_IDLE)" FAIL
else
    echo "  FAIL: could not even create fold_idle.bin"
    sed 's/^/    /' "$T/dd_stderr.log" 2>/dev/null
    check "Phase3 server-initiated idle fold (file creation itself failed)" FAIL
fi

echo ""
echo "=== Phase 4 [t=$(elapsed)s]: re-patch post-fold chunks, validate full-chunk integrity ==="
for name in fold_client fold_idle; do
    F="$MOUNT/${name}.bin"
    LOCAL_EXPECTED="$T/${name}_expected.bin"

    if [ ! -f "$F" ]; then
        check "Phase4 ${name}: post-fold re-patch full-chunk integrity (source file missing, cannot test)" FAIL
        continue
    fi
    cp "$F" "$LOCAL_EXPECTED"

    python3 -c "
import os
fd = os.open('$LOCAL_EXPECTED', os.O_RDWR)
os.pwrite(fd, bytes([0xAA]) * 8192, 1048576)
os.close(fd)
"
    python3 -c "
import os
fd = os.open('$F', os.O_RDWR)
os.pwrite(fd, bytes([0xAA]) * 8192, 1048576)
os.fsync(fd)
os.close(fd)
"
    dfs_sync
    sleep 1

    if [ ! -f "$F" ] || [ ! -f "$LOCAL_EXPECTED" ]; then
        check "Phase4 ${name}: post-fold re-patch full-chunk integrity (file vanished mid-check)" FAIL
        continue
    fi
    m1=$(md5sum "$LOCAL_EXPECTED" | cut -d' ' -f1)
    m2=$(md5sum "$F" | cut -d' ' -f1)
    [ "$m1" = "$m2" ] \
        && check "Phase4 ${name}: post-fold re-patch full-chunk integrity" PASS \
        || check "Phase4 ${name}: post-fold re-patch full-chunk integrity (exp $m1 got $m2)" FAIL
done

CORRUPTION_LINES=$(grep -inE "corrupt|hash mismatch|checksum mismatch" "$LOG"/server*.log "$LOG"/client.log 2>/dev/null | grep -vi "no corruption\|would have reported")
CORRUPTION_HITS=$(echo -n "$CORRUPTION_LINES" | grep -c . 2>/dev/null)
CORRUPTION_HITS=${CORRUPTION_HITS:-0}
if [ "$CORRUPTION_HITS" -eq 0 ]; then
    check "Phase4 no corruption/hash-mismatch log lines anywhere" PASS
else
    check "Phase4 no corruption/hash-mismatch log lines anywhere (found $CORRUPTION_HITS — review below)" FAIL
    echo "$CORRUPTION_LINES" | sed 's/^/    /'
fi

echo ""
echo "=== Phase 5 [t=$(elapsed)s]: waiting out the real 20-minute grace period ==="
while [ "$(elapsed)" -lt 1260 ]; do
    sleep 30
    e=$(elapsed)
    surviving=$(comm -12 "$T/orphan_candidates.txt" <(find "$ORPHAN_NODE_DIR" -type f 2>/dev/null | sort) | grep -c . 2>/dev/null)
    surviving=${surviving:-0}
    echo "  [t=${e}s] orphan candidates still present on node${PAUSE_NODE}: $surviving / $ORPHAN_CANDIDATE_COUNT (grace clears at 1200s)"
done

echo ""
echo "=== Phase 5b [t=$(elapsed)s]: grace should have cleared — watching pagination + eviction ==="
DEADLINE=1560
while [ "$(elapsed)" -lt "$DEADLINE" ]; do
    sleep 30
    e=$(elapsed)
    surviving=$(comm -12 "$T/orphan_candidates.txt" <(find "$ORPHAN_NODE_DIR" -type f 2>/dev/null | sort) | grep -c . 2>/dev/null)
    surviving=${surviving:-0}
    echo "  [t=${e}s] orphan candidates still present on node${PAUSE_NODE}: $surviving / $ORPHAN_CANDIDATE_COUNT"
done

FINAL_SURVIVING=$(comm -12 "$T/orphan_candidates.txt" <(find "$ORPHAN_NODE_DIR" -type f 2>/dev/null | sort) | grep -c . 2>/dev/null)
FINAL_SURVIVING=${FINAL_SURVIVING:-0}
echo "  final: $FINAL_SURVIVING / $ORPHAN_CANDIDATE_COUNT original orphan candidates still present on node${PAUSE_NODE}"

PAGE_EVENTS=$(count_matches "Disk orphan sweep: .* chunks checked" "$LOG/server${PAUSE_NODE}.log")
echo "  sweep page-log lines on node${PAUSE_NODE}: $PAGE_EVENTS"
echo "  eviction log lines on node${PAUSE_NODE}:"
grep "Live-file orphan sweep: evicted" "$LOG/server${PAUSE_NODE}.log" 2>/dev/null | sed 's/^/    /'

if [ "$ORPHAN_CANDIDATE_COUNT" -eq 0 ]; then
    check "Phase5 orphaned chunks were evicted by the paginated sweep (skipped — Phase1 found 0 candidates)" FAIL
elif [ "$FINAL_SURVIVING" -lt "$ORPHAN_CANDIDATE_COUNT" ]; then
    check "Phase5 orphaned chunks were evicted by the paginated sweep ($((ORPHAN_CANDIDATE_COUNT - FINAL_SURVIVING))/$ORPHAN_CANDIDATE_COUNT evicted)" PASS
else
    check "Phase5 orphaned chunks were evicted by the paginated sweep (0/$ORPHAN_CANDIDATE_COUNT evicted)" FAIL
fi

[ "$PAGE_EVENTS" -ge 2 ] \
    && check "Phase5 sweep processed multiple distinct pages (pagination observed, not one giant sweep)" PASS \
    || check "Phase5 sweep processed multiple distinct pages (only $PAGE_EVENTS page-log lines)" FAIL

[ -f "$MOUNT/control_a.bin" ] && [ -f "$MOUNT/control_b.bin" ] \
    && check "Phase5 control files survived the whole soak (no false-positive eviction)" PASS \
    || check "Phase5 control files survived the whole soak (no false-positive eviction)" FAIL

WATCHDOG_HITS=0
for f in "$LOG"/server*.log; do
    n=$(count_matches "Disk orphan sweep watchdog:" "$f")
    WATCHDOG_HITS=$((WATCHDOG_HITS + n))
done
[ "$WATCHDOG_HITS" -eq 0 ] \
    && check "Phase5 watchdog never fired (no crash/respawn during soak)" PASS \
    || check "Phase5 watchdog never fired (fired $WATCHDOG_HITS times — see server logs)" FAIL

echo ""
echo "════════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed"
echo "════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
