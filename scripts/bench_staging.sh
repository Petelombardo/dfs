#!/bin/bash
# Separate benchmark DFS cluster on the staging hardware (SLOT-OWNERSHIP-PLAN 3c decision).
# Production is untouched: its dfs-server runs from /usr/bin under systemd on :8900 with data in
# /mnt/gluster/dfs; the bench cluster runs from /mnt/gluster/dfs-bench/bin as plain processes on
# :8950 with its own data, config and logs there. Nothing is persisted (no make-persistent), so a
# reboot also removes it. The client is server4 (x86_64), mounted at /mnt/dfs-bench; servers are gluster2-5.
#
#   scripts/bench_staging.sh check                  free disk/mem per node, fio on the client
#   scripts/bench_staging.sh deploy [0|1]           copy binaries, init, start servers, mount
#   scripts/bench_staging.sh restart 0|1            restart servers+client with DFS_ORDERED_WRITES
#   scripts/bench_staging.sh run <label>            fio jobs on the client -> /root/dfs-staging-bench-<label>.txt
#   scripts/bench_staging.sh teardown               unmount, stop, delete everything
set -e
NODES="gluster2 gluster3 gluster4 gluster5"   # not gluster1: production leader, least free memory
CLIENT=server4   # x86_64: client binary from dist/bench-x86_64 (build-x86.sh)
PORT=8950
DIR=/mnt/gluster/dfs-bench            # on each storage node (persistent disk, not /tmp = RAM)
CDIR=/root/dfs-bench                  # on the client
MNT=/mnt/dfs-bench
REPO=$(cd "$(dirname "$0")/.." && pwd)
BIN="$REPO/target/release"
MIN_FREE_MB=3000
# Same per-process cache caps as the local suite: this runs next to production.
SERVER_ENV="DFS_CHUNK_RING_CAPACITY=8 DFS_DELTA_RING_CAPACITY=8 DFS_MAX_CACHE_CHUNKS=8 DFS_LEASE_CLUSTER_SIZE=4 DFS_SLOT_ISR_SEED_SECS=3"

ip_of() { getent hosts "$1" | awk '{print $1; exit}'; }
SEED="$(ip_of gluster2):$PORT"

# Stop only processes whose executable IS the bench binary. Never pkill -f: the pattern would
# also match the ssh shell running it, and a loose pattern could reach production.
stop_bench_servers() {
    for n in $NODES; do
        ssh root@"$n" "for p in \$(pgrep -x dfs-server); do [ \"\$(readlink /proc/\$p/exe)\" = $DIR/bin/dfs-server ] && kill \$p; done; true"
    done
}
stop_bench_client() {
    ssh root@"$CLIENT" "fusermount -u $MNT 2>/dev/null || umount -l $MNT 2>/dev/null; for p in \$(pgrep -x dfs-client); do [ \"\$(readlink /proc/\$p/exe)\" = $CDIR/dfs-client ] && kill \$p; done; true"
}
start_servers() {  # $1 = DFS_ORDERED_WRITES
    for n in $NODES; do
        ssh root@"$n" "cd $DIR && $SERVER_ENV DFS_ORDERED_WRITES=$1 RUST_LOG=info nohup setsid $DIR/bin/dfs-server start --config $DIR/config/config.toml >> $DIR/server.log 2>&1 < /dev/null & echo started"
    done
    sleep 8
}
start_client() {  # $1 = DFS_ORDERED_WRITES
    local cluster
    cluster=$(for n in $NODES; do printf '%s:%s,' "$(ip_of "$n")" "$PORT"; done); cluster=${cluster%,}
    ssh root@"$CLIENT" "mkdir -p $MNT && DFS_ORDERED_WRITES=$1 RUST_LOG=info nohup setsid $CDIR/dfs-client mount $MNT --cluster $cluster --log-file $CDIR/client.log --allow-other > $CDIR/client.out 2>&1 < /dev/null & sleep 3; mountpoint -q $MNT && echo mounted || echo MOUNT-FAILED"
    sleep 10   # leases, ISR seeding (default 30s cadence: the first writes may go unordered)
}

case "$1" in
check)
    for n in $NODES; do
        echo "$n: $(ssh root@"$n" "df -Pm /mnt/gluster | awk 'NR==2{print \$4\" MB free on /mnt/gluster\"}'; awk '/MemAvailable/{print int(\$2/1024)\" MB mem avail\"}' /proc/meminfo; uname -m" | tr '\n' ' ')"
    done
    echo "$CLIENT: $(ssh root@"$CLIENT" "which fio || echo NO-FIO; uname -m; df -Pm /root | awk 'NR==2{print \$4\" MB free\"}'" | tr '\n' ' ')"
    ;;
deploy)
    ORDERED=${2:-0}
    for n in $NODES; do
        free=$(ssh root@"$n" "df -Pm /mnt/gluster | awk 'NR==2{print \$4}'")
        [ "$free" -ge "$MIN_FREE_MB" ] || { echo "$n: only ${free}MB free on /mnt/gluster (< ${MIN_FREE_MB}) — aborting"; exit 1; }
    done
    for n in $NODES; do
        ip=$(ip_of "$n")
        ssh root@"$n" "mkdir -p $DIR/bin $DIR/data $DIR/metadata $DIR/config"
        scp -q "$BIN/dfs-server" "$BIN/dfs-admin" root@"$n":$DIR/bin/
        ssh root@"$n" "cd $DIR && [ -f config/config.toml ] || $DIR/bin/dfs-server init --data-dir $DIR/data --meta-dir $DIR/metadata --config $DIR/config/config.toml >/dev/null 2>&1
            sed -i 's|^listen_addr = .*|listen_addr = \"$ip:$PORT\"|' $DIR/config/config.toml
            if [ $n != gluster2 ]; then sed -i 's|^seed_nodes = .*|seed_nodes = [\"$SEED\"]|' $DIR/config/config.toml; fi
            grep -E '^(listen_addr|seed_nodes)' $DIR/config/config.toml"
    done
    ssh root@"$CLIENT" "mkdir -p $CDIR"
    scp -q "$REPO/dist/bench-x86_64/dfs-client" root@"$CLIENT":$CDIR/
    start_servers "$ORDERED"
    start_client "$ORDERED"
    ;;
restart)
    [ "$2" = 0 ] || [ "$2" = 1 ] || { echo "restart 0|1"; exit 1; }
    stop_bench_client; stop_bench_servers; sleep 3
    start_servers "$2"; start_client "$2"
    ;;
run)
    LABEL=${2:?label}; OUT=/root/dfs-staging-bench-$LABEL.txt
    ssh root@"$CLIENT" "cd $MNT && rm -f bench.bin && \
        fio --name=layout --filename=bench.bin --size=64m --bs=1m --rw=write --ioengine=psync --output=/dev/null && \
        sleep 10 && \
        for job in 'rand4k_qd1 --rw=randwrite --bs=4k --numjobs=1' 'rand4k_16w --rw=randwrite --bs=4k --numjobs=16' 'seq1m --rw=write --bs=1m --numjobs=1'; do
            set -- \$job; name=\$1; shift
            fio --name=\$name --filename=bench.bin --size=64m --ioengine=psync --direct=1 --time_based --runtime=30 --group_reporting \"\$@\" --output-format=json --output=$CDIR/\$name.json >/dev/null 2>&1 \
              || fio --name=\$name --filename=bench.bin --size=64m --ioengine=psync --fsync=1 --time_based --runtime=30 --group_reporting \"\$@\" --output-format=json --output=$CDIR/\$name.json >/dev/null
            cat $CDIR/\$name.json
            echo '@@END@@'
        done" > /root/dfs-staging-bench-$LABEL.raw
    python3 - "/root/dfs-staging-bench-$LABEL.raw" "$LABEL" <<'EOF' | tee "$OUT"
import json, sys
raw = open(sys.argv[1]).read().split("@@END@@")
print(f"== {sys.argv[2]}")
for part in raw:
    part = part.strip()
    if not part: continue
    j = json.loads(part[part.index("{"):]); job = j["jobs"][0]; w = job["write"]
    c = w["clat_ns"]; p = c.get("percentile", {})
    print(f"{job['jobname']:<12} iops={w['iops']:9.1f}  lat_us mean={c['mean']/1000:8.1f} "
          f"p50={p.get('50.000000',0)/1000:8.1f} p99={p.get('99.000000',0)/1000:9.1f}  MBps={w['bw']/1024:7.1f}")
EOF
    ;;
teardown)
    stop_bench_client; stop_bench_servers; sleep 2
    ssh root@"$CLIENT" "rm -rf $CDIR; rmdir $MNT 2>/dev/null; true"
    for n in $NODES; do ssh root@"$n" "rm -rf $DIR"; done
    echo "bench cluster removed"
    ;;
*) sed -n 2,13p "$0"; exit 1 ;;
esac
