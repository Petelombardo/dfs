#!/bin/bash
# G8 bench soak (SLOT-OWNERSHIP-TODO G8): the :8950 bench cluster (scripts/bench_staging.sh) with
# ordering on, a VM-108-like workload (scripts/bench_soak_workers.sh vm + qcow2) and DVR-style
# recordings (dvr), a fresh-client md5 pass every hour (verify), and a kill -9 of one random bench
# server about every 45 min (restarted 90-180 s later; never two down at once). Production on
# :8900 is untouched: only processes whose executable is the bench binary are ever killed.
#
#   scripts/bench_soak.sh start [hours]   deploy-time setup is bench_staging.sh deploy 1; this
#                                         starts the workers and runs the driver loop (default 48 h)
#   scripts/bench_soak.sh stop            ask the workers and the driver to stop
#   scripts/bench_soak.sh status          the summary so far
#
# Results: /root/g8-soak/summary.log here (one health line per 10 min, every kill/restart, a
# final verdict), worker logs on the client in /root/dfs-bench/soak/. Any worker FAIL line, a
# bench server panic, or a DATA LOSS line fails G8.
NODES="gluster2 gluster3 gluster4 gluster5"
CLIENT=${BENCH_CLIENT_HOST:-server4}
PORT=8950
DIR=/mnt/gluster/dfs-bench
CDIR=/root/dfs-bench
REPO=$(cd "$(dirname "$0")/.." && pwd)
OUT=/root/g8-soak
SERVER_ENV="DFS_CHUNK_RING_CAPACITY=8 DFS_DELTA_RING_CAPACITY=8 DFS_MAX_CACHE_CHUNKS=8 DFS_LEASE_CLUSTER_SIZE=4 DFS_SLOT_ISR_SEED_SECS=3"
mkdir -p "$OUT"
say() { echo "$(date '+%F %T') $*" | tee -a "$OUT/summary.log"; }
ip_of() { getent hosts "$1" | awk '{print $1; exit}'; }
bench_pid() { ssh -o BatchMode=yes root@"$1" "for p in \$(pgrep -x dfs-server); do [ \"\$(readlink /proc/\$p/exe)\" = $DIR/bin/dfs-server ] && echo \$p; done"; }
start_node() {
    ssh -o BatchMode=yes root@"$1" "cd $DIR || exit 1; $SERVER_ENV DFS_ORDERED_WRITES=1 DFS_METADATA_DURABLE=1 RUST_LOG=info nohup setsid $DIR/bin/dfs-server start --config $DIR/config/config.toml >> $DIR/server.log 2>&1 < /dev/null & echo started"
}
worker_counts() {  # "<worker>=<ok>/<fail> ..." from the client's worker logs
    ssh -o BatchMode=yes root@"$CLIENT" "cd $CDIR/soak 2>/dev/null && for w in vm qcow2 dvr verify; do printf '%s=%s/%s ' \$w \$(grep -c ' OK ' \$w.log 2>/dev/null || echo 0) \$(grep -c ' FAIL ' \$w.log 2>/dev/null || echo 0); done"
}
server_counts() {  # panics + DATA LOSS lines on each bench server since the soak started
    for n in $NODES; do
        o=$(awk -v n=$n '$1==n{print $2}' "$OUT/log-offsets")
        printf '%s:%s ' "$n" "$(ssh -o BatchMode=yes root@$n "tail -c +\$(( ${o:-0} + 1 )) $DIR/server.log | grep -a -c -E 'panicked|DATA LOSS:'")"
    done
}

case "$1" in
start)
    HOURS=${2:-48}
    rm -f "$OUT/stop"
    for n in $NODES; do echo "$n $(ssh root@$n "stat -c %s $DIR/server.log")"; done > "$OUT/log-offsets"
    cluster=$(for n in $NODES; do printf '%s:%s,' "$(ip_of "$n")" "$PORT"; done); cluster=${cluster%,}
    scp -q "$REPO/scripts/bench_soak_workers.sh" root@"$CLIENT":$CDIR/
    ssh root@"$CLIENT" "mkdir -p $CDIR/soak && rm -f $CDIR/soak/stop && echo '--cluster $cluster' > $CDIR/soak/cluster"
    for w in vm qcow2 dvr; do
        ssh root@"$CLIENT" "nohup setsid bash $CDIR/bench_soak_workers.sh $w > $CDIR/soak/$w.out 2>&1 < /dev/null & echo $w started"
    done
    say "G8 soak started: ${HOURS} h, nodes $NODES, client $CLIENT, ordering on"
    end=$(( $(date +%s) + HOURS * 3600 ))
    next_kill=$(( $(date +%s) + 1800 + RANDOM % 1800 ))
    next_verify=$(( $(date +%s) + 3600 ))
    next_health=0
    while [ "$(date +%s)" -lt "$end" ] && [ ! -f "$OUT/stop" ]; do
        now=$(date +%s)
        if [ "$now" -ge "$next_kill" ]; then
            set -- $NODES; shift $(( RANDOM % 4 )); victim=$1
            pid=$(bench_pid "$victim")
            if [ -n "$pid" ]; then
                ssh root@"$victim" "kill -9 $pid"; down=$(( 90 + RANDOM % 90 ))
                say "KILL -9 $victim (pid $pid); restarting in ${down}s"
                sleep "$down"
            else
                say "KILL skipped: $victim had no bench server running; restarting it"
            fi
            start_node "$victim" > /dev/null
            sleep 10
            [ -n "$(bench_pid "$victim")" ] && say "RESTARTED $victim" || say "FAIL $victim did not restart"
            next_kill=$(( $(date +%s) + 1800 + RANDOM % 1800 ))
        fi
        if [ "$now" -ge "$next_verify" ]; then
            ssh root@"$CLIENT" "bash $CDIR/bench_soak_workers.sh verify" < /dev/null
            next_verify=$(( $(date +%s) + 3600 ))
        fi
        if [ "$now" -ge "$next_health" ]; then
            up=""; for n in $NODES; do [ -n "$(bench_pid "$n")" ] && up="$up $n"; done
            say "HEALTH up:${up:- none} | workers $(worker_counts)| panics+dataloss $(server_counts)"
            next_health=$(( $(date +%s) + 600 ))
        fi
        sleep 30
    done
    ssh root@"$CLIENT" "touch $CDIR/soak/stop"
    say "G8 soak driver stopping; workers finish their current iteration"
    sleep 660
    ssh root@"$CLIENT" "bash $CDIR/bench_soak_workers.sh verify" < /dev/null
    fails=$(ssh root@"$CLIENT" "cat $CDIR/soak/*.log | grep -c ' FAIL '")
    sc=$(server_counts)
    say "FINAL workers $(worker_counts)| panics+dataloss $sc"
    if [ "$fails" = 0 ] && ! echo "$sc" | grep -qE ':[1-9]'; then say "G8 VERDICT: PASS"; else say "G8 VERDICT: FAIL ($fails worker FAIL lines; servers $sc)"; fi
    touch "$OUT/done"
    ;;
stop)
    touch "$OUT/stop"; ssh root@"$CLIENT" "touch $CDIR/soak/stop"; echo "stop requested"
    ;;
status)
    tail -n 20 "$OUT/summary.log"
    ;;
*) echo "usage: $0 start [hours] | stop | status"; exit 1 ;;
esac
