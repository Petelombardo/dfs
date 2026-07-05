#!/bin/bash
# Reproduce the write-path deadlock observed on server5 (VM 100, 2026-07-05):
# under a heavy small-write / high-concurrency workload against a single large
# file (a qcow2 disk image being hammered by kdiskmark inside a VM), dfs-client
# went fully silent for 9+ minutes. Every worker thread (tokio-rt-worker,
# dfs-flush, dfs-read) was parked on futex_wait_queue with zero forward
# progress, and the kernel showed dozens of the writer's threads blocked in
# fuse_file_write_iter — i.e. a real deadlock, not backpressure.
#
# Method: create one large file, then fire many concurrent workers doing small
# (4KB) writes at effectively unbounded queue depth against overlapping chunk
# ranges (mimics kdiskmark's RND4K QDxx/T16-style access pattern, and qcow2's
# own tendency to repeatedly touch its L1/L2 tables near the start of the
# file). Watch for the client going silent / write latency spiking to "never
# returns" while the process is still alive (not crashed).
#
# Usage: bash scripts/repro_write_deadlock.sh [num_workers] [duration_sec]

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-deadlock-mount
LOG=/tmp/dfs-deadlock-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
TESTFILE="$MOUNT/deadlock_repro.img"

NUM_WORKERS=${1:-64}
DURATION=${2:-120}
FILE_SIZE_MB=${3:-64}   # small enough to force heavy chunk-lock contention

cleanup_all() {
    pkill -f "dfs-server" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$MOUNT" "$LOG" 2>/dev/null || true
mkdir -p "$MOUNT" "$LOG"

echo "=== Starting 5-node cluster ==="
cd "$REPO"
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null
for i in 1 2 3 4 5; do
    RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        > "$LOG/server${i}.log" 2>&1 &
done
sleep 3

# MALLOC_ARENA_MAX is intentionally NOT set here unless exported by the caller —
# leaving it unset preserves glibc's default (unbounded-ish) arena behavior so this
# script can be used to test the hypothesis that arena proliferation, not a real
# data leak, drives the RSS growth (export MALLOC_ARENA_MAX=2 before invoking to test).
env RUST_LOG=info ${MALLOC_ARENA_MAX:+MALLOC_ARENA_MAX="$MALLOC_ARENA_MAX"} \
    ${DFS_WRITE_BUFFER_CAP_MB:+DFS_WRITE_BUFFER_CAP_MB="$DFS_WRITE_BUFFER_CAP_MB"} \
    "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level info &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
# dfs-client daemonizes by default (forks to background), so $! captured the
# already-exited parent, not the real process — find the live PID via pgrep,
# same as test_local_suite.sh does.
CLIENT_PID=$(pgrep -f "dfs-client mount $MOUNT" | head -1)
echo "Mounted. Client PID=$CLIENT_PID"
echo ""

echo "=== Pre-allocating ${FILE_SIZE_MB}MB test file ==="
dd if=/dev/zero of="$TESTFILE" bs=1M count="$FILE_SIZE_MB" 2>/dev/null
sync "$MOUNT"
sleep 1
echo "Initial write committed."
echo ""

echo "=== Firing $NUM_WORKERS concurrent 4KB-write workers for ${DURATION}s ==="
echo "Each worker does small overlapping-chunk pwrite+occasional-fsync in a tight loop."

run_worker() {
    local id=$1
    python3 - <<PYEOF &
import os, random, time

fd = os.open("$TESTFILE", os.O_RDWR)
size = $FILE_SIZE_MB * 1024 * 1024
end = time.time() + $DURATION
buf = bytes([($id % 255) + 1] * 4096)
n = 0
# Throttle to ~QD1 per worker (one in-flight op, brief gap between) instead of
# a raw tight loop — real disk benchmarks (and QEMU's aio thread pool) bound
# outstanding I/O depth; an unthrottled Python loop doesn't and can spawn far
# more concurrent FUSE requests than any real workload would.
try:
    while time.time() < end:
        off = random.randrange(0, size - 4096, 4096)
        os.pwrite(fd, buf, off)
        n += 1
        if n % 37 == 0:
            os.fsync(fd)
        time.sleep(0.002)
finally:
    os.close(fd)
PYEOF
}

PIDS=()
for id in $(seq 0 $(( NUM_WORKERS - 1 ))); do
    run_worker "$id"
    PIDS+=($!)
done

echo "Workers launched: ${#PIDS[@]}. Monitoring client log for silence..."
echo ""

# ── RSS sampler ──────────────────────────────────────────────────────────────
# Track dfs-client's memory over time so we can tell a genuine lock deadlock
# (flat, modest RSS) apart from unbounded buffer growth (climbing RSS, likely
# heading for OOM) — these are different bugs with different fixes.
# Also: grab a LIVE gdb backtrace + task/thread count the moment RSS crosses
# RSS_TRAP_KB, well before the kernel OOM-kills the process. The deadlock
# detector's log-silence-based gdb dump runs too late for a fast OOM climb —
# by the time 20s of silence is confirmed, the process is often already dead.
RSS_TRAP_STEP_KB=$(( 512 * 1024 ))  # re-arm every 512MB so we see the growth progression
(
    next_trap=$RSS_TRAP_STEP_KB
    while kill -0 "$CLIENT_PID" 2>/dev/null; do
        RSS_KB=$(awk '/^VmRSS/{print $2}' /proc/$CLIENT_PID/status 2>/dev/null)
        echo "$(date +%s.%N) rss_kb=${RSS_KB:-0}" >> "$LOG/rss-samples.log"
        if [ -n "$RSS_KB" ] && [ "$RSS_KB" -ge "$next_trap" ]; then
            echo "$(date +%s.%N) RSS crossed ${next_trap}KB — capturing live snapshot" >> "$LOG/rss-trap.log"
            next_trap=$(( next_trap + RSS_TRAP_STEP_KB ))
            {
                echo "=== /proc/$CLIENT_PID/status ==="
                cat /proc/$CLIENT_PID/status 2>/dev/null
                echo "=== task count ==="
                ls /proc/$CLIENT_PID/task/ 2>/dev/null | wc -l
                echo "=== smaps_rollup (mapping-type breakdown) ==="
                cat /proc/$CLIENT_PID/smaps_rollup 2>/dev/null
                echo "=== top 15 largest individual mappings by Rss ==="
                awk '
                    /^[0-9a-f]+-[0-9a-f]+/ { addr=$0 }
                    /^Rss:/ { print $2, addr }
                ' /proc/$CLIENT_PID/smaps 2>/dev/null | sort -rn | head -15
                echo "=== mapping count by type (heap/stack/anon/other) ==="
                awk '
                    /^[0-9a-f]+-[0-9a-f]+/ {
                        if ($6=="[heap]") t="heap";
                        else if ($6 ~ /^\[stack/) t="stack";
                        else if (NF<6) t="anon";
                        else t="file";
                        cur=t
                    }
                    /^Rss:/ { sum[cur]+=$2 }
                    END { for (k in sum) print k, sum[k]"kB" }
                ' /proc/$CLIENT_PID/smaps 2>/dev/null
                echo "=== live gdb backtrace (all threads) ==="
                gdb -p "$CLIENT_PID" -batch -ex 'set pagination off' -ex 'thread apply all bt' 2>&1
            } >> "$LOG/rss-trap.log"
        fi
        sleep 1
    done
) &
RSS_SAMPLER_PID=$!

# ── Deadlock detector ───────────────────────────────────────────────────────
# Poll the client log's mtime. If it goes quiet for > STALL_SEC while workers
# are still supposed to be running, and the FUSE mount itself stops responding
# (a fresh 'stat' on the mountpoint hangs), declare a repro.
STALL_SEC=20
START=$(date +%s)
LAST_SIZE=-1
LAST_CHANGE=$(date +%s)

while true; do
    NOW=$(date +%s)
    ELAPSED=$(( NOW - START ))
    if [ "$ELAPSED" -gt $(( DURATION + 30 )) ]; then
        echo "Workers should be done and no stall observed. No repro this run."
        break
    fi

    CUR_SIZE=$(stat -c '%s' "$LOG/client.log" 2>/dev/null || echo -1)
    if [ "$CUR_SIZE" != "$LAST_SIZE" ]; then
        LAST_SIZE=$CUR_SIZE
        LAST_CHANGE=$NOW
    fi

    QUIET_FOR=$(( NOW - LAST_CHANGE ))
    if [ "$QUIET_FOR" -ge "$STALL_SEC" ]; then
        echo ""
        echo "=== POSSIBLE DEADLOCK: client log silent for ${QUIET_FOR}s at t=${ELAPSED}s ==="
        # Confirm the mount is actually unresponsive, not just quiet.
        if timeout 5 stat "$TESTFILE" > /dev/null 2>&1; then
            echo "Mount still responds to stat() — probably just idle, not deadlocked. Continuing to watch."
            LAST_CHANGE=$NOW
            sleep 2
            continue
        fi
        echo "CONFIRMED: stat() on the test file hung/timed out — mount is wedged."
        echo ""
        echo "--- Thread states, grouped by (comm, state, wchan) ---"
        for t in /proc/$CLIENT_PID/task/*/; do
            comm=$(cat "$t/comm" 2>/dev/null)
            stat_c=$(awk '{print $3}' "$t/stat" 2>/dev/null)
            wchan=$(cat "$t/wchan" 2>/dev/null)
            echo "comm=$comm state=$stat_c wchan=$wchan"
        done | sort | uniq -c | sort -rn | head -20
        echo ""
        echo "--- Full thread backtrace (gdb) ---"
        gdb -p "$CLIENT_PID" -batch -ex 'set pagination off' -ex 'thread apply all bt' \
            > "$LOG/deadlock-backtrace.txt" 2>&1
        echo "Backtrace saved to $LOG/deadlock-backtrace.txt"
        grep -B2 -A15 "InodeWriteState\|lock_chunk\|flush_one_chunk\|flush_buffer_async\|push_inner\|metadata_queue" "$LOG/deadlock-backtrace.txt" | head -200
        echo ""
        echo "--- RSS trend (last 20 samples, 1/sec) ---"
        tail -20 "$LOG/rss-samples.log"
        echo ""
        echo "REPRO CONFIRMED. Client PID $CLIENT_PID left running for further inspection."
        echo "Kill manually with: kill $CLIENT_PID; kill $RSS_SAMPLER_PID; fusermount -u $MOUNT; pkill -f dfs-server"
        kill "$RSS_SAMPLER_PID" 2>/dev/null || true
        exit 2
    fi
    sleep 2
done

echo ""
echo "--- RSS trend (last 20 samples, 1/sec) ---"
tail -20 "$LOG/rss-samples.log" 2>/dev/null
echo ""
echo "=== Cleanup ==="
kill "$RSS_SAMPLER_PID" 2>/dev/null || true
kill "$CLIENT_PID" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -f "dfs-server" 2>/dev/null || true
echo "Done. Logs in $LOG/"
