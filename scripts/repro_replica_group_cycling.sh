#!/bin/bash
# "Folding day" repro #2 of 3: replica-group cycling / stale-node reconvergence.
#
# Theory (2026-07-30 VM-108/111 investigation, user's proposal): node A held a
# chunk's old state indefinitely, node B likewise but a different vintage; a
# later patch got routed back to node A, which resolved against ITS OWN old
# local view instead of the cluster's current one. Two confirmed code-level
# gaps make this plausible: (1) start_patch_fold_rebroadcast_loop only retries
# a fold announcement to an unreachable peer for 120s (PATCH_FOLD_REBROADCAST_TTL)
# before giving up permanently — a peer down/partitioned longer than that never
# learns the fold happened at all; (2) a slot resumed after a restart gets
# exactly one fold attempt, not a live debounce watchdog.
#
# This reproduces it directly: group1={node1,node2}, group2={node3,node4},
# node5 stays up throughout as the cluster majority/arbiter so the cluster
# keeps functioning.
#   Round 1: group2 down. Write patch 1 (0xBB) — lands on group1(+node5).
#   Round 2: group1 down, group2 up. Write patch 2 (0xCC) — lands on
#            group2(+node5). Hold group1 down > PATCH_FOLD_REBROADCAST_TTL
#            (120s) so any fold-announcement to them is fully abandoned.
#   Round 3: group1 back up, group2 down again. Write patch 3 (0xDD) — this
#            SHOULD route back through group1, the nodes that missed round 2
#            entirely.
# Then verify final content has all three patches correctly merged, and watch
# group1's server logs during round 3 for ghost/stale/ghost-chunk-guard/
# ChunkStale activity — the question is whether it correctly detects it's
# behind and self-heals/fails-loud, or silently resolves against its stale
# local view.
#
# Usage: bash scripts/repro_replica_group_cycling.sh

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-cycling-mount
LOG=/tmp/dfs-cycling-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
FILE="$MOUNT/cycling.img"
CHUNK_SIZE=$((4 * 1024 * 1024))
REBROADCAST_TTL=120

declare -A SERVER_PID

cleanup_all() {
    pkill -f "dfs-server" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

start_node() {
    local i=$1
    RUST_LOG=debug "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        --log-level debug >> "$LOG/server${i}.log" 2>&1 &
    SERVER_PID[$i]=$!
    echo "  node${i} started, pid=${SERVER_PID[$i]}"
}

stop_node() {
    local i=$1
    if [ -n "${SERVER_PID[$i]:-}" ]; then
        kill "${SERVER_PID[$i]}" 2>/dev/null || true
        wait "${SERVER_PID[$i]}" 2>/dev/null || true
        unset SERVER_PID[$i]
        echo "  node${i} stopped"
    fi
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$MOUNT" "$LOG" 2>/dev/null || true
mkdir -p "$MOUNT" "$LOG"

echo "=== Initializing 5-node cluster (not starting yet) ==="
cd "$REPO"
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null > /dev/null

echo "=== Starting all 5 nodes ==="
for i in 1 2 3 4 5; do start_node "$i"; done
sleep 3

env RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
CLIENT_PID=$(pgrep -f "dfs-client mount $MOUNT" | head -1)
echo "Mounted. Client PID=$CLIENT_PID"

echo "=== Writing baseline chunk (0xAA, ${CHUNK_SIZE} bytes) with all 5 up ==="
python3 -c "
with open('$FILE', 'wb') as f:
    f.write(bytes([0xAA]) * $CHUNK_SIZE)
"
sync "$MOUNT"
sleep 2

echo ""
echo "=== ROUND 1: group2 (node3,node4) DOWN — write patch 1 (0xBB @ 1,000,000) ==="
stop_node 3
stop_node 4
sleep 2
python3 -c "
import os
fd = os.open('$FILE', os.O_RDWR)
os.pwrite(fd, bytes([0xBB]) * 4096, 1000000)
os.close(fd)
"
sync "$MOUNT" 2>&1 | tail -5
sleep 2

echo ""
echo "=== ROUND 2: group1 (node1,node2) DOWN, group2 back UP — write patch 2 (0xCC @ 2,000,000) ==="
stop_node 1
stop_node 2
start_node 3
start_node 4
sleep 3
python3 -c "
import os
fd = os.open('$FILE', os.O_RDWR)
os.pwrite(fd, bytes([0xCC]) * 4096, 2000000)
os.close(fd)
"
sync "$MOUNT" 2>&1 | tail -5

echo ""
echo "=== Holding group1 down for ${REBROADCAST_TTL}s + margin, to cross PATCH_FOLD_REBROADCAST_TTL ==="
sleep $((REBROADCAST_TTL + 15))

echo ""
echo "=== ROUND 3: group1 back UP, group2 DOWN again — write patch 3 (0xDD @ 3,000,000) ==="
LOGMARK1=$(wc -l < "$LOG/server1.log" 2>/dev/null || echo 0)
LOGMARK2=$(wc -l < "$LOG/server2.log" 2>/dev/null || echo 0)
stop_node 3
stop_node 4
start_node 1
start_node 2
sleep 3
python3 -c "
import os
fd = os.open('$FILE', os.O_RDWR)
os.pwrite(fd, bytes([0xDD]) * 4096, 3000000)
os.close(fd)
"
sync "$MOUNT" 2>&1 | tail -5
sleep 3

echo ""
echo "=== group1 (node1,node2) log activity during/after round 3 (ghost/stale/refresh signals) ==="
for i in 1 2; do
    echo "-- server${i} (since round 3 started) --"
    tail -n +"$((i == 1 ? LOGMARK1 : LOGMARK2))" "$LOG/server${i}.log" 2>/dev/null | \
        grep -iE "ghost|stale|refresh_slot_from_leader|ChunkStale|pull_chunk_from_peers|superseded|MultiPatch|MERGE-TRACE|Single fold" | tail -20
done

echo ""
echo "=== Bringing all 5 nodes up for final verification ==="
start_node 3
start_node 4
sleep 3
sync "$MOUNT" 2>&1 | tail -5
sleep 2

echo ""
echo "=== Verifying final content ==="
python3 -c "
import os, sys
fd = os.open('$FILE', os.O_RDONLY)

def check(offset, length, expected_byte, label):
    data = os.pread(fd, length, offset)
    ok = all(b == expected_byte for b in data)
    print(f'{label}: {\"OK\" if ok else \"MISMATCH\"} (offset={offset}, expected=0x{expected_byte:02x}, got first byte=0x{data[0]:02x})')
    return ok

ok1 = check(1000000, 4096, 0xBB, 'Patch 1 bytes (0xBB @ 1,000,000, written while group2 was down)')
ok2 = check(2000000, 4096, 0xCC, 'Patch 2 bytes (0xCC @ 2,000,000, written while group1 was down)')
ok3 = check(3000000, 4096, 0xDD, 'Patch 3 bytes (0xDD @ 3,000,000, written after group1 recovered)')
okb = check(0, 4096, 0xAA, 'Untouched baseline bytes (0xAA @ 0)')
os.close(fd)

if ok1 and ok2 and ok3 and okb:
    print()
    print('PASS: all four regions correct — no reconvergence corruption')
    sys.exit(0)
else:
    print()
    print('FAIL: stale-replica reconvergence corrupted or lost data')
    sys.exit(1)
"
RESULT=$?

echo ""
if [ $RESULT -eq 0 ]; then
    echo "=== REPRO RESULT: PASS (no divergence reproduced this run) ==="
else
    echo "=== REPRO RESULT: FAIL (divergence reproduced) ==="
fi

rm -f "$FILE"
kill "$CLIENT_PID" 2>/dev/null || true
sleep 1
fusermount -u "$MOUNT" 2>/dev/null || true
pkill -f "dfs-server" 2>/dev/null || true
echo "Done. Logs in $LOG/"
exit $RESULT
