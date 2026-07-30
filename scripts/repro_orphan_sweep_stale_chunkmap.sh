#!/bin/bash
# "Folding day" repro #3 of 3: orphan sweep deletes a chunk a node was just told
# to hold, because a stale RESUMED fold clobbered that node's own chunk_map.
#
# Root cause chain confirmed by reading, not yet by running:
#   1. A node killed before its debounce fold fires leaves a genuinely-Pending
#      patch_state row that survives on disk (dirty_patch_slots is wiped on
#      restart, but PATCH_STATE_SLOT_TABLE/PATCH_STATE_TABLE are durable).
#   2. On restart, the resume sweep (metadata.rs:all_pending_patch_slots) makes
#      exactly ONE fold attempt from that OLD state — no live debounce watchdog,
#      confirmed via server.rs:9741 start_patch_fold_sweep_loop's own comment.
#   3. That fold's result gets applied to THIS node's own chunk_map via
#      update_chunk_map_after_patch (server.rs:620), which is explicitly
#      UNCONDITIONAL — matches only by (file_id, chunk_idx) position, no
#      write_seq/staleness comparison at all (its own comment argues this is
#      safe because chunk_patch_locks rules out CONCURRENT races — true, but
#      says nothing about a SEQUENTIALLY stale value being applied after the
#      real one already advanced). Confirmed in repro #2 that when this stale
#      result is broadcast to PEERS, they correctly reject it via write_seq
#      comparison (GHOST-reversion / RCL-stale-rejected) — but that guard only
#      exists on the *receiving* end for *incoming* updates, never for a node
#      applying its own locally-computed result to itself.
#   4. run_disk_orphan_sweep's candidacy check (healing.rs:1794) is
#      `loc_record.is_some() && live_chunks.contains(chunk_id)` — live_chunks
#      includes this node's OWN chunk_map. If the real final chunk got pushed
#      onto this node by the healer (to satisfy replication factor) but this
#      node's own chunk_map still (wrongly) names the stale chunk for that
#      slot, the real chunk is invisible to this node's own live_chunks set —
#      a legitimate eviction candidate, gated only by grace period + 2-pass
#      debounce + leader-confirm.
#
# Whether the leader-confirm gate (ConfirmChunksLive) catches this in practice
# is exactly what's unverified — theoretically it should (the leader's own
# chunk_map should still be correct), so this run is the actual test of that,
# not a foregone conclusion either way.
#
# Timing: SELF_RESTART_GRACE_SECS is hardcoded to 1200s (20 min) with no test
# override (checked: only DFS_LIVE_FILE_ORPHAN_GRACE_SECS is overridable via
# env). This script legitimately runs ~25-30 minutes.
#
# Usage: bash scripts/repro_orphan_sweep_stale_chunkmap.sh

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-orphansweep-mount
LOG=/tmp/dfs-orphansweep-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
FILE="$MOUNT/orphansweep.img"
CHUNK_SIZE=$((4 * 1024 * 1024))
DATA_DIR() { echo "$BASE/node$1/data"; }

declare -A SERVER_PID

cleanup_all() {
    pkill -f "dfs-server" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

start_node() {
    local i=$1
    DFS_LIVE_FILE_ORPHAN_GRACE_SECS=3 RUST_LOG=debug "$BIN/dfs-server" start \
        --config "$BASE/node${i}/config.toml" --log-level debug >> "$LOG/server${i}.log" 2>&1 &
    SERVER_PID[$i]=$!
    echo "  [$(date -u +%H:%M:%S)] node${i} started, pid=${SERVER_PID[$i]}"
}

stop_node() {
    local i=$1
    if [ -n "${SERVER_PID[$i]:-}" ]; then
        kill -9 "${SERVER_PID[$i]}" 2>/dev/null || true
        wait "${SERVER_PID[$i]}" 2>/dev/null || true
        unset SERVER_PID[$i]
        echo "  [$(date -u +%H:%M:%S)] node${i} stopped (kill -9, no graceful shutdown — simulating a real crash/OOM)"
    fi
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$MOUNT" "$LOG" 2>/dev/null || true
mkdir -p "$MOUNT" "$LOG"

echo "=== Initializing 5-node cluster ==="
cd "$REPO"
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null > /dev/null

echo "=== Starting all 5 nodes (DFS_LIVE_FILE_ORPHAN_GRACE_SECS=3 for fast grace-period testing) ==="
for i in 1 2 3 4 5; do start_node "$i"; done
sleep 3

env RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
CLIENT_PID=$(pgrep -f "dfs-client mount $MOUNT" | head -1)
echo "Mounted. Client PID=$CLIENT_PID"

echo ""
echo "=== Writing baseline chunk (0xAA, ${CHUNK_SIZE} bytes), all 5 up ==="
python3 -c "
with open('$FILE', 'wb') as f:
    f.write(bytes([0xAA]) * $CHUNK_SIZE)
"
sync "$MOUNT"
sleep 1

echo ""
echo "=== Patch 1: 0xBB @ offset 1,000,000 (fresh accumulator) ==="
python3 -c "
import os
fd = os.open('$FILE', os.O_RDWR)
os.pwrite(fd, bytes([0xBB]) * 4096, 1000000)
os.close(fd)
"

echo "=== Determining which node(s) actually hold this chunk (RF-selected replicas only —"
echo "    NOT every node gets patch data; most only get metadata reconciliation) ==="
echo "    Confirmed via two earlier aborted runs: a leader-avoidance heuristic picked a node"
echo "    that never received MultiPatch/MERGE-TRACE at all — routing table membership,"
echo "    not leadership, decides who gets the actual write."
REPLICA_NODES=()
for i in $(seq 1 50); do
    REPLICA_NODES=()
    for n in 1 2 3 4 5; do
        if grep -qE "MultiPatch:|MERGE-TRACE" "$LOG/server${n}.log" 2>/dev/null; then
            REPLICA_NODES+=("$n")
        fi
    done
    if [ "${#REPLICA_NODES[@]}" -ge 2 ]; then
        break
    fi
    sleep 0.1
done
echo "  Nodes confirmed holding this chunk: ${REPLICA_NODES[*]:-none}"
if [ "${#REPLICA_NODES[@]}" -lt 2 ]; then
    echo "  Fewer than 2 replica-holders detected within 5s — aborting, can't safely pick a stale node"
    cat "$LOG"/server*.log | grep -E "Processing request: MultiPatch|MERGE-TRACE" | tail -20
    kill "$CLIENT_PID" 2>/dev/null; fusermount -u "$MOUNT" 2>/dev/null; pkill -9 -f "target/release/dfs-server" 2>/dev/null
    exit 2
fi
STALE_NODE="${REPLICA_NODES[0]}"
echo "  Using node${STALE_NODE} as the stale/restarting node (a confirmed chunk holder)"

echo "=== Waiting 3s for the new durability-flush-timer backstop (fires every 2s server-side,"
echo "    metadata.rs:start_durability_flush_timer) to get at least one full cycle before we kill"
echo "    it — the write+detect+sync sequence above completes in well under 1s, faster than the"
echo "    timer's own interval, so killing immediately would test nothing (confirmed: two runs"
echo "    before this fix, and one run AFTER it with no wait here, all showed dirty_patch_slots=0"
echo "    on restart — the last one is inconclusive on the fix specifically, not evidence it failed) ==="
sleep 3

echo "=== Forcing a durability barrier before killing it too (client-side sync — confirmed this"
echo "    alone is NOT sufficient, it only guarantees the RPC was acknowledged, not that this"
echo "    node's own redb committed with Durability::Immediate) ==="
sync "$MOUNT" 2>&1 | tail -5

echo "  Killing it now"
stop_node "$STALE_NODE"

echo ""
echo "=== Patch 2: 0xCC @ offset 2,000,000 (merge, only reaches the 4 remaining nodes) ==="
python3 -c "
import os
fd = os.open('$FILE', os.O_RDWR)
os.pwrite(fd, bytes([0xCC]) * 4096, 2000000)
os.close(fd)
"
sync "$MOUNT" 2>&1 | tail -5

echo ""
echo "=== Waiting 40s for the remaining 4 nodes' debounce backstop to actually fold patch 1+2 ==="
sleep 40
sync "$MOUNT" 2>&1 | tail -5

echo ""
echo "=== Extracting the real FINAL chunk_id for this slot — searching all non-stale nodes' logs ==="
echo "    (replica selection isn't simply \"everyone but the down node\" — RF likely picked a"
echo "    specific subset, so we scan every candidate rather than assume which one has it)"
FINAL_CHUNK=""
FINAL_CHUNK_NODE=""
for i in 1 2 3 4 5; do
    [ "$i" = "$STALE_NODE" ] && continue
    c=$(grep "Single fold:" "$LOG/server${i}.log" 2>/dev/null | tail -1 | grep -oE '\-> [0-9a-f]{64}' | awk '{print $2}')
    if [ -z "$c" ]; then
        c=$(grep "MultiPatch:" "$LOG/server${i}.log" 2>/dev/null | tail -1 | grep -oE '\-> [0-9a-f]{64}' | awk '{print $2}')
    fi
    if [ -n "$c" ]; then
        FINAL_CHUNK="$c"
        FINAL_CHUNK_NODE="$i"
        break
    fi
done
echo "  FINAL_CHUNK (real, cluster-correct current chunk for this slot) = $FINAL_CHUNK (seen on node${FINAL_CHUNK_NODE})"
if [ -z "$FINAL_CHUNK" ]; then
    echo "  Could not determine FINAL_CHUNK from any node's logs — aborting"
    for i in 1 2 3 4 5; do
        echo "-- server${i} tail --"
        tail -15 "$LOG/server${i}.log"
    done
    kill "$CLIENT_PID" 2>/dev/null; fusermount -u "$MOUNT" 2>/dev/null; pkill -f dfs-server 2>/dev/null
    exit 2
fi
P1=${FINAL_CHUNK:0:2}; P2=${FINAL_CHUNK:2:2}
FINAL_PATH_STALE_NODE="$(DATA_DIR $STALE_NODE)/chunks/$P1/$P2/$FINAL_CHUNK"

echo ""
echo "=== Sanity read-check (baseline + patch1 + patch2) before reintroducing node${STALE_NODE} ==="
python3 -c "
import os, sys
fd = os.open('$FILE', os.O_RDONLY)
def check(offset, length, expected_byte, label):
    data = os.pread(fd, length, offset)
    ok = all(b == expected_byte for b in data)
    print(f'{label}: {\"OK\" if ok else \"MISMATCH\"}')
    return ok
ok1 = check(1000000, 4096, 0xBB, 'Patch 1 (0xBB)')
ok2 = check(2000000, 4096, 0xCC, 'Patch 2 (0xCC)')
os.close(fd)
sys.exit(0 if (ok1 and ok2) else 1)
"
if [ $? -ne 0 ]; then
    echo "  Pre-restart sanity check failed — aborting, this isn't the scenario we want to test"
    kill "$CLIENT_PID" 2>/dev/null; fusermount -u "$MOUNT" 2>/dev/null; pkill -f dfs-server 2>/dev/null
    exit 2
fi

echo ""
echo "=== Restarting node${STALE_NODE} — its resume sweep will find patch1's genuinely-stuck Pending row ==="
T_RESTART=$(date +%s)
LOGMARK=$(wc -l < "$LOG/server${STALE_NODE}.log" 2>/dev/null || echo 0)
start_node "$STALE_NODE"
sleep 5

echo ""
echo "=== node${STALE_NODE}'s resume-sweep / fold activity right after restart ==="
tail -n +"$LOGMARK" "$LOG/server${STALE_NODE}.log" | grep -iE "resume sweep|Single fold|MultiPatch:|chunk_map|GHOST" | head -30

echo ""
echo "=== Continuous timeline: does FINAL_CHUNK exist on node${STALE_NODE}'s disk, checked every 20s"
echo "    for the full ~25 minute window (SELF_RESTART_GRACE_SECS=1200s + 2 sweep cycles + buffer)?"
echo "    NOT forcing any further writes to this slot — that would mint a new current chunk and"
echo "    invalidate the FINAL_CHUNK we're tracking (confirmed via an earlier aborted run). If it"
echo "    never appears at all, that's an honest finding about healing latency, not a fabricated"
echo "    eviction signal. ==="
TOTAL_WAIT=$(( 1200 + 300 ))
POLL_INTERVAL=20
ELAPSED_POLL=0
LAST_STATE=""
APPEARED_AT=""
DISAPPEARED_AT=""
while [ "$ELAPSED_POLL" -lt "$TOTAL_WAIT" ]; do
    if [ -f "$FINAL_PATH_STALE_NODE" ]; then
        STATE="present"
    else
        STATE="absent"
    fi
    if [ "$STATE" != "$LAST_STATE" ]; then
        NOW_ELAPSED=$(( $(date +%s) - T_RESTART ))
        echo "  [+${NOW_ELAPSED}s since restart] state change: ${LAST_STATE:-<initial>} -> ${STATE}"
        if [ "$STATE" = "present" ] && [ -z "$APPEARED_AT" ]; then APPEARED_AT="$NOW_ELAPSED"; fi
        if [ "$STATE" = "absent" ] && [ -n "$APPEARED_AT" ] && [ -z "$DISAPPEARED_AT" ]; then DISAPPEARED_AT="$NOW_ELAPSED"; fi
        LAST_STATE="$STATE"
    fi
    sleep "$POLL_INTERVAL"
    ELAPSED_POLL=$(( ELAPSED_POLL + POLL_INTERVAL ))
done

echo ""
echo "=== Timeline summary ==="
echo "  Appeared at: ${APPEARED_AT:-never}"
echo "  Disappeared at (after appearing): ${DISAPPEARED_AT:-never (or never appeared)}"

if [ -f "$FINAL_PATH_STALE_NODE" ]; then
    echo "  FINAL state: STILL PRESENT: $FINAL_PATH_STALE_NODE"
    if [ -n "$APPEARED_AT" ]; then
        RESULT=0
    else
        echo "  (never confirmed absent-then-appeared during the window — inconclusive on the eviction question,"
        echo "   but it was present from before restart onward, meaning node${STALE_NODE} may have simply never"
        echo "   lost its original copy, e.g. if this slot's replica set didn't actually exclude it)"
        RESULT=3
    fi
elif [ -n "$APPEARED_AT" ]; then
    echo "  FINAL state: MISSING, having appeared at +${APPEARED_AT}s and disappeared at +${DISAPPEARED_AT:-?}s"
    echo "  (WRONGLY EVICTED — this is the target reproduction)"
    RESULT=1
else
    echo "  FINAL state: MISSING, and it never appeared at all during the window"
    echo "  (inconclusive — healing never gave node${STALE_NODE} this chunk in the first place,"
    echo "   so the orphan-sweep-eviction mechanism was never actually exercised)"
    RESULT=2
fi

echo ""
echo "=== node${STALE_NODE} orphan-sweep / ConfirmChunksLive log activity (full run) ==="
grep -iE "orphan sweep|Live-file orphan|Phantom reconciliation|ConfirmChunksLive|ghost-chunk guard|GHOST-" \
    "$LOG/server${STALE_NODE}.log" | grep -i "$FINAL_CHUNK"
echo "-- broader orphan-sweep summary lines --"
grep -iE "Disk orphan sweep:|Live-file orphan sweep:" "$LOG/server${STALE_NODE}.log" | tail -20

echo ""
echo "=== Final end-to-end read-back via the client (does the FS still serve correct data?) ==="
sync "$MOUNT" 2>&1 | tail -5
python3 -c "
import os, sys
fd = os.open('$FILE', os.O_RDONLY)
def check(offset, length, expected_byte, label):
    data = os.pread(fd, length, offset)
    ok = all(b == expected_byte for b in data)
    print(f'{label}: {\"OK\" if ok else \"MISMATCH/EIO\"}')
    return ok
try:
    ok1 = check(1000000, 4096, 0xBB, 'Patch 1 (0xBB)')
    ok2 = check(2000000, 4096, 0xCC, 'Patch 2 (0xCC)')
except OSError as e:
    print(f'READ FAILED: {e}')
os.close(fd)
"

echo ""
case "$RESULT" in
    0) echo "=== REPRO RESULT: PASS (appeared via healing, survived the full window — leader-confirm gate held) ===" ;;
    1) echo "=== REPRO RESULT: FAIL (orphan sweep wrongly evicted a chunk it had just been told to hold) ===" ;;
    2) echo "=== REPRO RESULT: INCONCLUSIVE (healing never gave node${STALE_NODE} this chunk during the window — mechanism not exercised, try a longer window or a different node pairing) ===" ;;
    3) echo "=== REPRO RESULT: INCONCLUSIVE (chunk was present throughout — this node may not have actually lost its original copy) ===" ;;
esac

rm -f "$FILE"
kill "$CLIENT_PID" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -f "dfs-server" 2>/dev/null || true
echo "Done. Logs in $LOG/"
exit $RESULT
