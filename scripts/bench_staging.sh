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
#                                                   (DFS_METADATA_DURABLE=0 in the env: old commit cadence)
#   scripts/bench_staging.sh run <label>            fio jobs on the client -> /root/dfs-staging-bench-<label>.txt
#   scripts/bench_staging.sh upgrade-node <node> <dir> [0|1]  replace one node's server binary, keep its data
#   scripts/bench_staging.sh swap-client <path> [0|1]  replace just the client binary and remount
#   scripts/bench_staging.sh dd <label> [N]         N x `dd 500 MB conv=fsync` on the client (the rock5b
#                                                   10-13 EIO) -> /root/dfs-staging-dd-<label>.txt
#   scripts/bench_staging.sh meta <label> [N]       N x (300 fsynced 4 KiB creates + 500 fsynced 4 KiB
#                                                   appends): metadata-commit latency -> /root/dfs-staging-meta-<label>.txt
#   scripts/bench_staging.sh lat <label> [N]        N x 500 4 KiB O_DSYNC writes on the client: per-write
#                                                   latency -> /root/dfs-staging-lat-<label>.txt
#   scripts/bench_staging.sh read <label> [N] [MB]  write one MB-sized file (default 2048: more than the
#                                                   servers can cache), then N x remount + sequential read
#                                                   piped through md5 -> /root/dfs-staging-read-<label>.txt
#   scripts/bench_staging.sh teardown               unmount, stop, delete everything
#
# BENCH_JOBS="seq1m_fsync ..." runs only those fio jobs (default: all).
# BENCH_SERVER_EXTRA_ENV="K=V ..." adds environment for the servers (e.g. DFS_APTIMING_INFO_MS=0).
# BENCH_CLIENT_EXTRA_ENV="K=V ..." adds environment for the client (e.g. DFS_CHAIN_WRITES=1).
# BENCH_LEADER_ORDER="server5 server4 ..." gives those nodes fixed node ids in that order, lowest
#   first: the lowest-id fit node leads, so this picks the leader and who follows it (a slow disk
#   as leader made every metadata commit wait on it, G8 1029). Unlisted nodes keep random ids.
# BENCH_SERVER_BIN_DIR / BENCH_CLIENT_BIN deploy other binaries (e.g. production's own, copied
# read-only, for a prod-vs-new comparison). A redeploy over a running bench needs teardown first.
# BENCH_NODES="server2 server3 ..." / BENCH_DIR=/mnt/dfsbench/srv run the servers on other hosts and
# disks (the first node seeds the rest); BENCH_CLIENT_HOST picks the client. Default: gluster2-5.
set -e
NODES=${BENCH_NODES:-"gluster2 gluster3 gluster4 gluster5"}   # not gluster1: production leader, least free memory
CLIENT=${BENCH_CLIENT_HOST:-server4}   # server4: x86_64 client from dist/bench-x86_64 (build-x86.sh);
                                       # rock5b (aarch64, 1 GbE): BENCH_CLIENT_BIN=target/release/dfs-client
PORT=8950
DIR=${BENCH_DIR:-/mnt/gluster/dfs-bench}   # on each storage node (persistent disk, not /tmp = RAM)
CDIR=/root/dfs-bench                  # on the client
MNT=/mnt/dfs-bench
REPO=$(cd "$(dirname "$0")/.." && pwd)
BIN="${BENCH_SERVER_BIN_DIR:-$REPO/target/release}"           # dfs-server + dfs-admin (aarch64)
CLIENT_BIN="${BENCH_CLIENT_BIN:-$REPO/dist/bench-x86_64/dfs-client}"  # x86_64
MIN_FREE_MB=3000
# Same per-process cache caps as the local suite: this runs next to production.
SERVER_ENV="DFS_CHUNK_RING_CAPACITY=8 DFS_DELTA_RING_CAPACITY=8 DFS_MAX_CACHE_CHUNKS=8 DFS_LEASE_CLUSTER_SIZE=4 DFS_SLOT_ISR_SEED_SECS=3"

ip_of() { getent hosts "$1" | awk '{print $1; exit}'; }
SEED_NODE=${NODES%% *}   # the first node seeds the others
SEED="$(ip_of "$SEED_NODE"):$PORT"

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
# Each remote start must background ONE simple command: `x && y &` backgrounds a subshell that
# keeps ssh's stdout open, so ssh never returns.
start_servers() {  # $1 = DFS_ORDERED_WRITES; DFS_METADATA_DURABLE from the caller's env (default 1)
    for n in $NODES; do
        ssh root@"$n" "cd $DIR || exit 1; $SERVER_ENV $BENCH_SERVER_EXTRA_ENV DFS_ORDERED_WRITES=$1 DFS_METADATA_DURABLE=${DFS_METADATA_DURABLE:-1} RUST_LOG=info nohup setsid $DIR/bin/dfs-server start --config $DIR/config/config.toml >> $DIR/server.log 2>&1 < /dev/null & echo started"
    done
    sleep 8
}
start_client() {  # $1 = DFS_ORDERED_WRITES
    local cluster
    cluster=$(for n in $NODES; do printf '%s:%s,' "$(ip_of "$n")" "$PORT"; done); cluster=${cluster%,}
    ssh root@"$CLIENT" "mkdir -p $MNT || exit 1; cd $CDIR && ulimit -c unlimited; $BENCH_CLIENT_EXTRA_ENV DFS_ORDERED_WRITES=$1 RUST_LOG=info nohup setsid $CDIR/dfs-client mount $MNT --cluster $cluster --log-file $CDIR/client.log --allow-other > $CDIR/client.out 2>&1 < /dev/null & sleep 3; mountpoint -q $MNT && echo mounted || echo MOUNT-FAILED"
    sleep 10   # leases, ISR seeding (default 30s cadence: the first writes may go unordered)
}

case "$1" in
check)
    for n in $NODES; do
        echo "$n: $(ssh root@"$n" "df -Pm $(dirname $DIR) | awk 'NR==2{print \$4\" MB free on $(dirname $DIR)\"}'; awk '/MemAvailable/{print int(\$2/1024)\" MB mem avail\"}' /proc/meminfo; uname -m" | tr '\n' ' ')"
    done
    echo "$CLIENT: $(ssh root@"$CLIENT" "which fio || echo NO-FIO; uname -m; df -Pm /root | awk 'NR==2{print \$4\" MB free\"}'" | tr '\n' ' ')"
    ;;
deploy)
    ORDERED=${2:-0}
    for n in $NODES; do
        free=$(ssh root@"$n" "df -Pm $(dirname $DIR) | awk 'NR==2{print \$4}'")
        [ "$free" -ge "$MIN_FREE_MB" ] || { echo "$n: only ${free}MB free on $(dirname $DIR) (< ${MIN_FREE_MB}) — aborting"; exit 1; }
    done
    for n in $NODES; do
        ip=$(ip_of "$n")
        ssh root@"$n" "mkdir -p $DIR/bin $DIR/data $DIR/metadata $DIR/config"
        scp -q "$BIN/dfs-server" "$BIN/dfs-admin" root@"$n":$DIR/bin/
        ssh root@"$n" "cd $DIR && [ -f config/config.toml ] || $DIR/bin/dfs-server init --data-dir $DIR/data --meta-dir $DIR/metadata --config $DIR/config/config.toml >/dev/null 2>&1
            sed -i 's|^listen_addr = .*|listen_addr = \"$ip:$PORT\"|' $DIR/config/config.toml
            if [ $n != $SEED_NODE ]; then sed -i 's|^seed_nodes = .*|seed_nodes = [\"$SEED\"]|' $DIR/config/config.toml; fi
            grep -E '^(listen_addr|seed_nodes)' $DIR/config/config.toml"
        rank=0; k=0
        for m in ${BENCH_LEADER_ORDER:-}; do k=$((k + 1)); [ "$m" = "$n" ] && rank=$k; done
        if [ "$rank" != 0 ]; then
            id=$(printf '00000000-0000-4000-8000-%012d' "$rank")
            ssh root@"$n" "f=$DIR/config/config.toml; if grep -q '^node_id' \$f; then sed -i 's|^node_id = .*|node_id = \"$id\"|' \$f; else sed -i '/^\[node\]/a node_id = \"$id\"' \$f; fi; grep '^node_id' \$f"
        fi
    done
    ssh root@"$CLIENT" "mkdir -p $CDIR"
    scp -q "$CLIENT_BIN" root@"$CLIENT":$CDIR/dfs-client
    start_servers "$ORDERED"
    start_client "$ORDERED"
    ;;
upgrade-node)  # upgrade-node <node> <bin dir> [0|1]: replace ONE node's server binary in place, keep its data
    n=$2; [ -n "$n" ] && [ -f "$3/dfs-server" ] || { echo "upgrade-node <node> <bin dir> [0|1]"; exit 1; }
    ssh root@"$n" "for p in \$(pgrep -x dfs-server); do [ \"\$(readlink /proc/\$p/exe)\" = $DIR/bin/dfs-server ] && kill \$p; done; sleep 2; true"
    scp -q "$3/dfs-server" "$3/dfs-admin" root@"$n":$DIR/bin/
    ssh root@"$n" "cd $DIR || exit 1; $SERVER_ENV $BENCH_SERVER_EXTRA_ENV DFS_ORDERED_WRITES=${4:-0} DFS_METADATA_DURABLE=${DFS_METADATA_DURABLE:-1} RUST_LOG=info nohup setsid $DIR/bin/dfs-server start --config $DIR/config/config.toml >> $DIR/server.log 2>&1 < /dev/null & echo started $n"
    ;;
swap-client)  # swap-client <dfs-client path> [0|1]: replace only the client binary, remount
    [ -f "$2" ] || { echo "swap-client <dfs-client path> [0|1]"; exit 1; }
    stop_bench_client; sleep 1
    scp -q "$2" root@"$CLIENT":$CDIR/dfs-client
    start_client "${3:-0}"
    ;;
restart)
    [ "$2" = 0 ] || [ "$2" = 1 ] || { echo "restart 0|1"; exit 1; }
    stop_bench_client; stop_bench_servers; sleep 3
    start_servers "$2"; start_client "$2"
    ;;
run)
    LABEL=${2:?label}; OUT=/root/dfs-staging-bench-$LABEL.txt
    # Every job fsyncs each write: through FUSE, O_DIRECT alone lands in the client's
    # write buffer, so unsynced numbers measure buffering and swing ~15x run to run
    # (2026-10-03: 770 vs 18000 4k IOPS, same build and flag). Only acked writes count.
    ssh root@"$CLIENT" "cd $MNT && rm -f bench.bin && \
        fio --name=layout --filename=bench.bin --size=64m --bs=1m --rw=write --ioengine=psync --output=/dev/null && \
        sleep 10 && \
        for job in 'rand4k_fsync --rw=randwrite --bs=4k --numjobs=1' 'rand4k_16w_fsync --rw=randwrite --bs=4k --numjobs=16' 'seq1m_fsync --rw=write --bs=1m --numjobs=1' 'rand4k_read --rw=randread --bs=4k --numjobs=1' 'rand4k_16r_read --rw=randread --bs=4k --numjobs=16' 'seq1m_read --rw=read --bs=1m --numjobs=1'; do
            set -- \$job; name=\$1; shift
            case \" $BENCH_JOBS \" in \"  \"|*\" \$name \"*) ;; *) continue ;; esac
            fio --name=\$name --filename=bench.bin --size=64m --ioengine=psync --direct=1 --fsync=1 --time_based --runtime=30 --group_reporting \"\$@\" --output-format=json --output=$CDIR/\$name.json >/dev/null 2>&1 \
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
    j = json.loads(part[part.index("{"):]); job = j["jobs"][0]
    w = job["read"] if job["read"]["io_bytes"] > 0 else job["write"]
    c = w["clat_ns"]; p = c.get("percentile", {})
    print(f"{job['jobname']:<12} iops={w['iops']:9.1f}  lat_us mean={c['mean']/1000:8.1f} "
          f"p50={p.get('50.000000',0)/1000:8.1f} p99={p.get('99.000000',0)/1000:9.1f}  MBps={w['bw']/1024:7.1f}"
          + (f"  fsync_us mean={sy['lat_ns']['mean']/1000:.0f} p50={sy['lat_ns'].get('percentile',{}).get('50.000000',0)/1000:.0f} "
             f"p99={sy['lat_ns'].get('percentile',{}).get('99.000000',0)/1000:.0f}"
             if (sy := job.get("sync", {})) and sy.get("total_ios") else ""))
EOF
    ;;
dd)
    # The 10-13 rock5b report: a 500 MB burst with fsync on a 1 GbE client.
    out=/root/dfs-staging-dd-$2.txt; : > "$out"
    for r in $(seq 1 "${3:-3}"); do
        # timeouts/skipped count this run's client-log lines only (the log spans every run).
        res=$(ssh root@"$CLIENT" "cd $MNT && rm -f dd.img; L=$CDIR/client.log; t0=\$(grep -c 'Timeout writing' \$L); k0=\$(grep -c 'skipping without attempting' \$L); s=\$(date +%s%N); dd if=/dev/urandom of=dd.img bs=1M count=500 conv=fsync status=none 2>&1; rc=\$?; e=\$(date +%s%N); echo rc=\$rc ms=\$(( (e - s) / 1000000 )) MBps=\$(( 500 * 1000000000 / (e - s) )) timeouts=\$((\$(grep -c 'Timeout writing' \$L) - t0)) skipped=\$((\$(grep -c 'skipping without attempting' \$L) - k0)); rm -f dd.img")
        echo "run $r: $(echo $res | tr '\n' ' ')" | tee -a "$out"
        sleep 20
    done
    ;;
meta)
    # Puts that change a file's size or slots (each one is a leader metadata commit, acked on a
    # majority with DFS_METADATA_MAJORITY_ACK): fsynced 4 KB creates, then fsynced 4 KB appends.
    out=/root/dfs-staging-meta-$2.txt; : > "$out"
    for r in $(seq 1 "${3:-3}"); do
        res=$(ssh root@"$CLIENT" "cd $MNT && rm -rf meta && mkdir meta && python3 - <<'PY'
import os, time
t0 = time.time()
for i in range(300):
    fd = os.open('meta/f%d' % i, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    os.write(fd, os.urandom(4096)); os.fsync(fd); os.close(fd)
t1 = time.time()
fd = os.open('meta/append.log', os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
for i in range(500):
    os.write(fd, os.urandom(4096)); os.fsync(fd)
os.close(fd)
t2 = time.time()
print('create_ms_per_file=%.2f append_ms_per_fsync=%.2f' % ((t1 - t0) * 1000 / 300, (t2 - t1) * 1000 / 500))
PY
rm -rf meta")
        echo "run $r: $res" | tee -a "$out"
        sleep 5
    done
    ;;
lat)
    out=/root/dfs-staging-lat-$2.txt; : > "$out"
    for r in $(seq 1 "${3:-3}"); do
        res=$(ssh root@"$CLIENT" "cd $MNT && rm -f lat.img && dd if=/dev/zero of=lat.img bs=1M count=8 conv=fsync status=none && s=\$(date +%s%N); dd if=/dev/urandom of=lat.img bs=4k count=500 oflag=dsync conv=notrunc status=none; rc=\$?; e=\$(date +%s%N); echo rc=\$rc us_per_write=\$(( (e - s) / 500000 )); rm -f lat.img")
        echo "run $r: $res" | tee -a "$out"
        sleep 5
    done
    ;;
read)
    out=/root/dfs-staging-read-$2.txt; : > "$out"
    mb=${4:-2048}
    w=$(ssh root@"$CLIENT" "cd $MNT && rm -f rd.img; s=\$(date +%s%N); dd if=/dev/urandom bs=1M count=$mb status=none | tee rd.img | md5sum | cut -d' ' -f1 > $CDIR/rd.md5; sync rd.img; e=\$(date +%s%N); echo write_MBps=\$(( $mb * 1000000000 / (e - s) ))")
    echo "wrote $mb MB: $w" | tee -a "$out"
    for r in $(seq 1 "${3:-3}"); do
        # A fresh mount each run: nothing comes from the client's caches or rock5b's page cache.
        stop_bench_client; sleep 1; start_client "${READ_ORDERED:-1}" >/dev/null
        res=$(ssh root@"$CLIENT" "cd $MNT && s=\$(date +%s%N); m=\$(dd if=rd.img bs=1M status=none | md5sum | cut -d' ' -f1); e=\$(date +%s%N); [ \"\$m\" = \"\$(cat $CDIR/rd.md5)\" ] && ok=match || ok=MISMATCH; echo ms=\$(( (e - s) / 1000000 )) MBps=\$(( $mb * 1000000000 / (e - s) )) md5=\$ok")
        echo "run $r: $res" | tee -a "$out"
    done
    ssh root@"$CLIENT" "rm -f $MNT/rd.img"
    ;;
teardown)
    stop_bench_client; stop_bench_servers; sleep 2
    ssh root@"$CLIENT" "rm -rf $CDIR; rmdir $MNT 2>/dev/null; true"
    for n in $NODES; do ssh root@"$n" "rm -rf $DIR"; done
    echo "bench cluster removed"
    ;;
*) sed -n 2,13p "$0"; exit 1 ;;
esac
