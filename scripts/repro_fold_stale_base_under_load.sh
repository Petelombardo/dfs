#!/bin/bash
# Reproduce the 2026-08-04 VM-111 install data-loss incident: a background fold used
# a stale pre-patch base 20 seconds after a client patch had already committed a
# newer, complete chunk on the same slot. The stale fold's no-op result won
# arbitration via location_supersedes' fold-priority tiebreak, and the correct chunk
# was later evicted as an apparent orphan.
#
# Real incident load profile: an OS install writing one VM disk, ~3 simultaneous DVR
# recordings, and a read (playback) — 4-5 files being actively serviced by the same
# 5-node cluster at once. Static-analysis review of the locking (chunk_patch_locks
# held for the whole apply_patch merge AND the whole fold_slot_now execution, both
# re-reading PATCH_STATE_TABLE fresh) did not find the exact gap — see session notes.
# Two real, confirmed bugs were found and fixed along the way (handle_propose_fold's
# unlocked peer-side fingerprint read, notify_leader_of_fold's best-effort leader
# notification) but neither was proven to be THE trigger for this specific incident.
# This script exists to catch the remaining mechanism directly instead of continuing
# to guess from logs.
#
# WHAT THIS DOES
#   Client A: writes a monotonically increasing sequence number into a small, fixed
#     byte range of ONE chunk (RACE_CHUNK) of a target file, in a tight loop — small,
#     distinct-offset patches, matching an install's real write pattern (not one big
#     overwrite, which wouldn't build a MultiPatch accumulator at all).
#   Client B: a SEPARATE client (separate process, separate connection — no client-
#     local write-cache shortcut) concurrently reads that same byte range in a tight
#     loop, forcing the server to resolve (and potentially force-fold) the same slot
#     A is actively patching.
#   Clients C/D/E: three background noise writers, each continuously patching a
#     DIFFERENT large file — the "3 DVR recordings" concurrent-load ingredient the
#     incident's own load profile depended on.
#
#   INVARIANT: the highest sequence number B has ever observed must never regress.
#   A regression means the persisted content reverted to an older value — the exact
#   externally-observable shape of the incident (data silently reverting to a stale
#   pre-patch state).
#
# PASS (exit 0): no regression observed for the full run duration.
# FAIL (exit 1): a regression was observed — reproduced. Full debug logs are in $LOG.
#
# OUTCOME (2026-08-04): DID NOT REPRODUCE, on any version of the code tested — four
# clean runs (30s, 120s, 600s on current HEAD, and a 600s run against commit 1f748e8,
# the EXACT code that was actually live in production when the real incident
# happened) all passed with zero regressions. This is a genuine negative result, not
# a tooling gap that was fixed and then retried into a pass — see the session's own
# notes for the full disk-usage debugging detour (an earlier unthrottled version of
# the noise generators filled the dev box's disk and locked up the machine; the
# throttled, sequential-append version here is safe but was never the blocker on
# reproduction).
#
# Leading theory for why: this script sustains pressure on ONE (file, chunk_idx)
# slot. The real incident was an OS install, which touches THOUSANDS of distinct
# chunk_idx positions in a tight window — a fundamentally different concurrency
# shape (many slots each independently racing patch-vs-fold at once) that a
# single-hot-slot pattern doesn't recreate. A next attempt should scatter writer A's
# patches across many chunk_idx positions (closer to a real install/extraction
# pattern) instead of hammering one — untried here for time/disk-budget reasons, see
# session notes for the explicit A-vs-B tradeoff discussion.
#
# Three real, independently-confirmed bugs were found and fixed during this
# investigation regardless of not reproducing the incident directly:
# handle_propose_fold's unlocked peer-side fingerprint read, notify_leader_of_fold's
# best-effort (no-retry) leader notification, and two disk deletes missing
# spawn_blocking. Plus: storage.rs's delete_chunk now takes a `reason` tag and warns
# on deleting a chunk under 5 minutes old — the diagnostic gap that made the original
# incident take hours to reconstruct from indirect evidence is now closed regardless
# of whether this repro ever catches the live mechanism.
#
# Usage: ./scripts/repro_fold_stale_base_under_load.sh [duration_secs]

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MA=/tmp/dfs-foldload-a
MB=/tmp/dfs-foldload-b
MC=/tmp/dfs-foldload-c
MD=/tmp/dfs-foldload-d
ME=/tmp/dfs-foldload-e
LOG=/tmp/dfs-foldload-logs
BIN="$REPO/target/release"
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
DURATION="${1:-180}"
RACE_CHUNK=4096
CHUNK_SIZE=$((4 * 1024 * 1024))
RACE_OFFSET=$((RACE_CHUNK * CHUNK_SIZE + 1000000))

cleanup_all() {
    for m in "$MA" "$MB" "$MC" "$MD" "$ME"; do
        mountpoint -q "$m" 2>/dev/null && fusermount -u "$m" 2>/dev/null
    done
    pkill -f "dfs-client mount /tmp/dfs-foldload" 2>/dev/null || true
    pkill -f "dfs-server" 2>/dev/null || true
    sleep 0.5
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$MA" "$MB" "$MC" "$MD" "$ME" "$LOG" 2>/dev/null || true
mkdir -p "$MA" "$MB" "$MC" "$MD" "$ME" "$LOG"

echo "=== Starting 5-node cluster ==="
cd "$REPO"
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null
for i in 1 2 3 4 5; do
    RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        --log-level info > "$LOG/server${i}.log" 2>&1 &
done
sleep 3

mount_client() {
    local name="$1" mp="$2" level="${3:-debug}"
    RUST_LOG="$level" "$BIN/dfs-client" mount "$mp" --cluster "$CLUSTER" \
        --log-file "$LOG/client-$name.log" --allow-other --log-level "$level" > /dev/null 2>&1 &
    local waited=0
    until mountpoint -q "$mp"; do
        sleep 0.5; waited=$((waited+1))
        [ "$waited" -gt 40 ] && { echo "MOUNT FAILED ($name)"; tail -30 "$LOG/client-$name.log"; exit 1; }
    done
    echo "  mounted $name at $mp (log level: $level)"
}

echo "=== Mounting 5 clients (A=writer, B=reader, C/D/E=background noise) ==="
mount_client a "$MA" debug
mount_client b "$MB" debug
# Noise clients: info only — we don't need their per-op detail, just the load,
# and debug-level logging from 3 more clients was the dominant disk-usage cost
# of a longer run (a 30s sanity run alone produced ~244MB across 5 debug clients).
mount_client c "$MC" info
mount_client d "$MD" info
mount_client e "$ME" info

TARGET="$MA/target.img"
echo "=== Writing baseline (32MB, so chunk_idx $RACE_CHUNK exists) ==="
python3 -c "
import os
with open('$TARGET', 'wb') as f:
    f.seek($((( RACE_CHUNK + 8 ) * CHUNK_SIZE)) - 1)
    f.write(b'\\0')
"
sync "$MA"
sleep 1

echo "=== Launching background noise writers (C/D/E, 3 separate large files) ==="
# Paced, mostly-sequential append — like a real DVR recording stream, not an
# unthrottled random-write flood. The first version of this script used
# unthrottled random writes across a wide range and consumed ~7GB in well
# under 300s (many distinct chunk_idx positions, each its own accumulator/
# fold cycle, at a rate no real recording produces) — that's what caused the
# disk-full lockup. ~2MB/s per stream (three streams ~= 6MB/s total) is a
# generous stand-in for a real recording's bitrate.
for m in "$MC" "$MD" "$ME"; do
    python3 -c "
import os, time
path = '$m/noise.img'
fd = os.open(path, os.O_CREAT | os.O_RDWR, 0o644)
end = time.time() + $DURATION + 10
offset = 0
chunk = os.urandom(65536)
while time.time() < end:
    os.pwrite(fd, chunk, offset)
    offset += 65536
    time.sleep(0.064)  # ~1MB/s — halved after the 2MB/s x 3 streams x RF replication
                        # ate 6.2GB in ~550s on this dev box's limited disk headroom
os.close(fd)
" > "$LOG/noise-$(basename "$m").log" 2>&1 &
done

echo "=== Launching reader B (forces server-side resolve/fold on the race slot) ==="
python3 -c "
import os, time, sys
path = '$MB/target.img'
end = time.time() + $DURATION + 5
highest = -1
regressions = 0
last_err_report = 0
while time.time() < end:
    try:
        fd = os.open(path, os.O_RDONLY)
        data = os.pread(fd, 8, $RACE_OFFSET)
        os.close(fd)
        if len(data) == 8:
            seq = int.from_bytes(data, 'little')
            if seq != 0:
                if seq < highest:
                    regressions += 1
                    print(f'REGRESSION: observed seq={seq} after already having seen seq={highest}', flush=True)
                highest = max(highest, seq)
    except OSError as e:
        now = time.time()
        if now - last_err_report > 2:
            print(f'read error (transient, expected under load): {e}', flush=True)
            last_err_report = now
    time.sleep(0.05)
print(f'READER DONE: highest={highest} regressions={regressions}', flush=True)
sys.exit(1 if regressions > 0 else 0)
" > "$LOG/reader.log" 2>&1 &
READER_PID=$!

echo "=== Launching writer A (sequence-numbered small patches to the race slot) ==="
python3 -c "
import os, time, sys
path = '$TARGET'
fd = os.open(path, os.O_RDWR)
end = time.time() + $DURATION
seq = 0
while time.time() < end:
    seq += 1
    os.pwrite(fd, seq.to_bytes(8, 'little'), $RACE_OFFSET)
    # Small delay: real install writes aren't a tight spin, and this gives the
    # server's debounce/fold-on-read machinery room to actually fire between
    # writes instead of every write landing mid-accumulator forever.
    time.sleep(0.1)
os.close(fd)
print(f'WRITER DONE: final seq={seq}', flush=True)
" > "$LOG/writer.log" 2>&1 &
WRITER_PID=$!

# Safety net after the disk-full lockup this repro caused once already: abort
# everything (not just this script) the moment free space gets tight, rather
# than trusting the run to finish first. Checked every 5s; MIN_FREE_KB leaves
# real margin since redb/ext4 both misbehave badly at 0 free, not just slowly.
MIN_FREE_KB=$((2 * 1024 * 1024))  # 2GB
(
    while kill -0 "$WRITER_PID" 2>/dev/null; do
        avail_kb=$(df -k / | awk 'NR==2 {print $4}')
        if [ "$avail_kb" -lt "$MIN_FREE_KB" ]; then
            echo "DISK GUARD: free space ${avail_kb}KB < ${MIN_FREE_KB}KB — aborting run early to avoid another lockup" | tee "$LOG/disk-guard-tripped"
            kill "$WRITER_PID" "$READER_PID" 2>/dev/null
            pkill -f "dfs-server" 2>/dev/null
            pkill -f "dfs-client mount /tmp/dfs-foldload" 2>/dev/null
            break
        fi
        sleep 5
    done
) &
DISK_GUARD_PID=$!

echo "=== Running for ${DURATION}s (disk guard active, min free ${MIN_FREE_KB}KB) ==="
wait "$WRITER_PID"
wait "$READER_PID"
READER_RESULT=$?
kill "$DISK_GUARD_PID" 2>/dev/null

if [ -f "$LOG/disk-guard-tripped" ]; then
    echo ""
    echo "=== REPRO RESULT: ABORTED (disk guard tripped) — not a valid pass/fail, re-run after freeing space ==="
    df -h /
    exit 2
fi

echo ""
echo "=== Writer output ==="
tail -5 "$LOG/writer.log"
echo "=== Reader output ==="
tail -20 "$LOG/reader.log"

echo ""
echo "=== Final verification from a FRESH read (post-settle) ==="
sync "$MA"
sleep 2
FINAL_SEQ=$(python3 -c "
import os
fd = os.open('$MA/target.img', os.O_RDONLY)
data = os.pread(fd, 8, $RACE_OFFSET)
os.close(fd)
print(int.from_bytes(data, 'little'))
")
WRITER_FINAL=$(grep -oE 'final seq=[0-9]+' "$LOG/writer.log" | grep -oE '[0-9]+')
echo "Writer's last-written seq: $WRITER_FINAL"
echo "Final on-disk seq (fresh read): $FINAL_SEQ"

RESULT=0
if [ "$READER_RESULT" -ne 0 ]; then
    echo "=== REPRO RESULT: FAIL (reader observed a sequence regression during the run) ==="
    RESULT=1
fi
if [ -n "$WRITER_FINAL" ] && [ "$FINAL_SEQ" != "$WRITER_FINAL" ]; then
    echo "=== REPRO RESULT: FAIL (final on-disk value $FINAL_SEQ != writer's last write $WRITER_FINAL) ==="
    RESULT=1
fi
if [ "$RESULT" -eq 0 ]; then
    echo "=== REPRO RESULT: PASS (no divergence reproduced this run) ==="
fi

echo ""
echo "Logs preserved in $LOG/ — server*.log, client-*.log (debug level), reader.log, writer.log"
echo "(not cleaning up automatically — inspect logs, then run: pkill -f dfs-server; pkill -f 'dfs-client mount /tmp/dfs-foldload'; for m in $MA $MB $MC $MD $ME; do fusermount -u \$m; done)"
exit $RESULT
