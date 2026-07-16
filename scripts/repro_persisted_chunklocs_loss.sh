#!/bin/bash
# Isolated repro for the 2026-07-16 empty-persisted-chunk_locations regression
# (T45d/T48c/T28a class): write a file, dfs_sync, then poll every node's
# persisted FileMetadata (dfs-admin file info) once per second. Distinguishes
# "chunk_locations never appear" from "appear then vanish", per node, with
# servers at DEBUG level. Stops (cluster left running) the first time a node
# that had a non-zero count reports 0/missing, or if the leader still reports
# 0 after GRACE seconds.
set -u
ITER=${1:-20}
GRACE=20
REPO=/builds/dfs
BIN=$REPO/target/release
LOG=/tmp/dfs-test-logs/repro-chunklocs
MOUNT=/tmp/dfs-mount
CLUSTER=127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904

/root/dfs-clean.sh
rm -rf /tmp/dfs-test "$LOG"
mkdir -p "$LOG"

bash "$REPO/scripts/setup-cluster.sh" 5 > "$LOG/init.log" 2>&1 || { echo "init failed"; exit 1; }
for i in 1 2 3 4 5; do
    RUST_LOG=debug "$BIN/dfs-server" start --config /tmp/dfs-test/node$i/config.toml \
        > "$LOG/server$i.log" 2>&1 &
done
sleep 3
RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "mount failed"; exit 1; }

count_on() { # port path -> chunk count | "miss" | "err"
    "$BIN/dfs-admin" --cluster 127.0.0.1:$1 --format json file info "$2" 2>/dev/null \
        | python3 -c "
import json,sys
try:
    d = json.load(sys.stdin)
    print(len(d.get('chunk_locations', [])))
except Exception:
    print('err')" 2>/dev/null || echo err
}

for n in $(seq 1 "$ITER"); do
    F="/t_cl_$n.bin"
    dd if=/dev/urandom of="$MOUNT$F" bs=1M count=1 2>/dev/null
    sync "$MOUNT"
    echo "=== iter $n: written+synced $F, polling ==="
    seen_nonzero=""
    for s in $(seq 1 "$GRACE"); do
        line="t+${s}s:"
        vanish=""
        for p in 8900 8901 8902 8903 8904; do
            c=$(count_on $p "$F")
            line="$line $p=$c"
            case "$c" in
                ''|err) c=err ;;
            esac
            if [ "$c" != "err" ] && [ "$c" != "0" ]; then
                seen_nonzero="$seen_nonzero $p"
            fi
            # a node that previously reported nonzero now reports 0 or err —
            # re-check a few times before declaring loss: a transient "err" is
            # routine (each node takes a brief serving pause for its one-time
            # first-run offline compaction), and only a PERSISTENT 0/err means
            # the record was actually lost.
            if [ "$c" = "0" ] || [ "$c" = "err" ]; then
                case " $seen_nonzero " in
                    *" $p "*)
                        for _retry in 1 2 3 4 5; do
                            sleep 2
                            c=$(count_on $p "$F")
                            [ "$c" != "0" ] && [ "$c" != "err" ] && [ -n "$c" ] && break
                        done
                        if [ "$c" = "0" ] || [ "$c" = "err" ] || [ -z "$c" ]; then
                            vanish="$p"
                        fi
                        ;;
                esac
            fi
        done
        echo "  $line"
        if [ -n "$vanish" ]; then
            echo "!!! iter $n: node $vanish HAD chunk_locations and now reports 0/err — VANISHED. Cluster left running; logs in $LOG"
            exit 2
        fi
        sleep 1
    done
    if [ -z "$seen_nonzero" ]; then
        echo "!!! iter $n: NO node ever reported non-zero chunk_locations within ${GRACE}s — NEVER-APPEARED. Cluster left running; logs in $LOG"
        exit 3
    fi
    rm -f "$MOUNT$F"
done
echo "all $ITER iterations clean"
/root/dfs-clean.sh
