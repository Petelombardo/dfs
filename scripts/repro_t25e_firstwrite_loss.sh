#!/bin/bash
# Focused repro for the intermittent T25e-class loss: fresh file + 512B write +
# immediate fsync + cold remount reads back size 0 (leader serves size=0 despite
# logging seq=1/2 size=512 PutFileMetadata pushes — 2026-07-16 debug session).
# Loops the exact T25e sequence against a 5-node local cluster with SERVERS AT
# DEBUG level (the suite runs them at info, which hides [SIZE TRACE] and
# put_file's stale-guard decisions). Stops at first failure and leaves the
# cluster RUNNING so the persisted record can be queried with dfs-admin.
#
# Usage: repro_t25e_firstwrite_loss.sh [iterations=50]
set -u
ITER=${1:-50}
REPO=/builds/dfs
BIN=$REPO/target/release
LOG=/tmp/dfs-test-logs/repro-t25e
MOUNT=/tmp/dfs-mount
CLUSTER=127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904

cleanup_all() {
    pgrep -x dfs-server | xargs -r kill -9 2>/dev/null
    pgrep -x dfs-client | xargs -r kill -9 2>/dev/null
    pkill -9 -x fusermount3 2>/dev/null
    sleep 1
    fusermount -uz "$MOUNT" 2>/dev/null
}

remount_cold() {
    # Kill only the client; servers stay up (matches T25e).
    pgrep -x dfs-client | xargs -r kill 2>/dev/null
    sleep 0.5
    fusermount -uz "$MOUNT" 2>/dev/null
    sleep 0.5
    RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
        --log-file "$LOG/client-$1.log" --allow-other --log-level debug &
    sleep 2
    mountpoint -q "$MOUNT"
}

cleanup_all
rm -rf /tmp/dfs-test "$LOG"
mkdir -p "$LOG"

# Init + start 5 nodes at DEBUG level.
bash "$REPO/scripts/setup-cluster.sh" 5 > "$LOG/init.log" 2>&1 || { echo "init failed"; exit 1; }
for i in 1 2 3 4 5; do
    RUST_LOG=debug "$BIN/dfs-server" start --config /tmp/dfs-test/node$i/config.toml \
        > "$LOG/server$i.log" 2>&1 &
done
sleep 3
remount_cold boot || { echo "mount failed"; exit 1; }

for n in $(seq 1 "$ITER"); do
    F="$MOUNT/t25e_repro_$n.bin"
    python3 - "$F" <<'EOF'
import os, sys
data = bytes([0xEB, 0x63, 0x90] + [0xAA] * 509)
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(fd, data)
os.fsync(fd)
os.close(fd)
EOF
    remount_cold "$n" || { echo "iter $n: remount failed"; exit 1; }
    RESULT=$(python3 - "$F" <<'EOF'
import sys
expected = bytes([0xEB, 0x63, 0x90] + [0xAA] * 509)
try:
    actual = open(sys.argv[1], 'rb').read(512)
except Exception as e:
    print(f'READ_ERROR:{e}'); sys.exit(0)
print('OK' if actual == expected else f'MISMATCH:len={len(actual)}')
EOF
)
    if [ "$RESULT" != "OK" ]; then
        echo "iter $n: FAILED ($RESULT) — file=$F left in place, cluster left running"
        echo "query with: $BIN/dfs-admin --cluster $CLUSTER --format json file info /t25e_repro_$n.bin"
        exit 2
    fi
    echo "iter $n: OK"
    rm -f "$F"
done
echo "all $ITER iterations passed"
cleanup_all
