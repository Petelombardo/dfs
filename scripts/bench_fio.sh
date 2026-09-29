#!/bin/bash
# fio latency/throughput bench on a local 5-node cluster (SLOT-OWNERSHIP-PLAN 3c gate).
# Usage: scripts/bench_fio.sh [label]      e.g. DFS_ORDERED_WRITES=1 scripts/bench_fio.sh ordered
# Prints one line per job: iops, mean/p50/p99 completion latency (us), MB/s.
# Same cluster shape and cache caps as test_local_suite.sh; build release first.
set -e
LABEL="${1:-run}"
REPO=$(cd "$(dirname "$0")/.." && pwd)
BIN="$REPO/target/release"
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-mount
LOG=/tmp/dfs-bench-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
OUT="${BENCH_OUT:-/root/dfs-bench-$LABEL.txt}"
SIZE="${BENCH_SIZE:-64m}"      # small on purpose: cluster data amplifies ~13x (5 nodes, patches)
RUNTIME="${BENCH_RUNTIME:-30}"

export DFS_CHUNK_RING_CAPACITY=8 DFS_DELTA_RING_CAPACITY=8 DFS_MAX_CACHE_CHUNKS=8 DFS_WRITE_BUFFER_CAP_MB=32
export DFS_LEASE_MS=3000 DFS_LEASE_MARGIN_MS=500 DFS_LEASE_CLUSTER_SIZE=5
export DFS_ORDERED_WRITES="${DFS_ORDERED_WRITES:-0}"

cleanup() {
    fusermount -u "$MOUNT" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    pkill -x dfs-server 2>/dev/null || true
}
trap cleanup EXIT
cleanup; sleep 0.5
rm -rf "$LOG"; mkdir -p "$MOUNT" "$LOG"
cd "$REPO"
bash scripts/setup-cluster.sh 5 >/dev/null 2>&1
for i in 1 2 3 4 5; do
    RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        > "$LOG/server${i}.log" 2>&1 &
done
sleep 3
RUST_LOG=info "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" --log-file "$LOG/client.log" --allow-other &
sleep 3
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; exit 1; }
sleep 5   # leases, ISR seeding, leader settle

# O_DIRECT through FUSE if the mount allows it (what a VM with cache=none does); otherwise
# buffered with an fsync per write, which forces the same end-to-end round trip.
if fio --name=probe --filename="$MOUNT/probe.bin" --size=4k --bs=4k --rw=write --direct=1 \
        --ioengine=psync --output=/dev/null >/dev/null 2>&1; then
    SYNCMODE="--direct=1"
else
    SYNCMODE="--fsync=1"
fi
rm -f "$MOUNT/probe.bin"

run() {  # name, extra fio args...
    local name=$1; shift
    fio --name="$name" --filename="$MOUNT/bench.bin" --size="$SIZE" --ioengine=psync \
        --time_based --runtime="$RUNTIME" --group_reporting $SYNCMODE "$@" \
        --output-format=json --output="$LOG/$name.json" >/dev/null
    python3 - "$LOG/$name.json" "$name" <<'EOF'
import json, sys
j = json.load(open(sys.argv[1]))["jobs"][0]["write"]
c = j["clat_ns"]; p = c.get("percentile", {})
print(f"{sys.argv[2]:<14} iops={j['iops']:9.1f}  lat_us mean={c['mean']/1000:8.1f} "
      f"p50={p.get('50.000000',0)/1000:8.1f} p99={p.get('99.000000',0)/1000:9.1f}  MBps={j['bw']/1024:7.1f}")
EOF
}

{
    echo "== $LABEL  DFS_ORDERED_WRITES=$DFS_ORDERED_WRITES  $(date -u +%FT%TZ)  mode=$SYNCMODE size=$SIZE runtime=${RUNTIME}s"
    fio --name=layout --filename="$MOUNT/bench.bin" --size="$SIZE" --bs=1m --rw=write \
        --ioengine=psync --output=/dev/null >/dev/null   # lay the file out first
    run rand4k_qd1  --rw=randwrite --bs=4k --numjobs=1
    run rand4k_16w  --rw=randwrite --bs=4k --numjobs=16
    run seq1m       --rw=write --bs=1m --numjobs=1
} | tee "$OUT"
