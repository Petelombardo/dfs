#!/bin/bash
# Isolated local before/after harness for the Q32T1-style throughput question
# (2026-08-02): does coordinate_and_fold_slot's ProposeFold/FoldLockGrant
# protocol (adf3faf, 2026-07-29) add measurable overhead to concurrent
# random-write throughput, even though it's confirmed NOT on the synchronous
# write-response path (only the backstop debounce-idle fold path)?
#
# Sets up a fresh local 5-node cluster + client, runs raw_disk_bench.sh
# (T7 = concurrent QD=32 random 4K write, mirrors KDiskMark RND4K Q32T1)
# against whatever binary is currently built, and appends to the CSV.
# Run once against the current (with-coordination) binary, once against a
# binary with coordinate_and_fold_slot short-circuited to the old
# uncoordinated path, using the SAME label convention so results are directly
# comparable in /tmp/dfs-bench-results.csv.
#
# Usage: LABEL=with-coordination bash scripts/bench_fold_coordination_compare.sh

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-bench-mount
LOG=/tmp/dfs-bench-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
LABEL="${LABEL:-$(date +%Y%m%d-%H%M%S)}"

if pgrep -f "target/release/dfs-server" >/dev/null 2>&1 || pgrep -f "target/release/dfs-client" >/dev/null 2>&1; then
    echo "ABORT: dfs-server or dfs-client already running -- kill those first."
    pgrep -af "target/release/dfs-server|target/release/dfs-client"
    exit 2
fi

cleanup_all() {
    pkill -9 -f "target/release/dfs-server" 2>/dev/null || true
    pkill -9 -f "target/release/dfs-client" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || fusermount -u -z "$MOUNT" 2>/dev/null || true
}

start_cluster() {
    for i in 1 2 3 4 5; do
        RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
            --log-level info >> "$LOG/server${i}.log" 2>&1 &
    done
    sleep 3
}

mount_client() {
    env RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
        --log-file "$LOG/client.log" --allow-other --log-level info \
        > /dev/null 2>&1 &
    sleep 2
    mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$MOUNT" "$LOG" "$BASE" 2>/dev/null || true
mkdir -p "$MOUNT" "$LOG"

echo "=== Setting up + starting 5-node cluster ==="
cd "$REPO"
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null > /dev/null
start_cluster
mount_client
echo "Mounted at $MOUNT."

echo "=== Running raw_disk_bench.sh (label=$LABEL) ==="
bash "$REPO/scripts/raw_disk_bench.sh" "$MOUNT" "$LABEL"
RESULT=$?

cleanup_all
echo "Done. CSV: /tmp/dfs-bench-results.csv"
exit $RESULT
