#!/bin/bash
# Targeted repro for the EXACT race the fix in dbdcb0e addresses (2026-08-01,
# VM-108 chunk-0 header zeroing): the release handler's read-only-close
# branch used to unconditionally wipe write_buffers/recent_chunk_writes for
# an inode whenever a read-only fd closed while the writer-fd count read
# zero. That requires two DIFFERENT fds on the SAME inode racing at close
# time -- something neither earlier repro script (same-file-across-restart,
# bystander-file-across-restart) actually created, since both only ever had
# one fd per file, opened and closed sequentially. This script creates the
# actual race: one process hammering open(O_WRONLY)/write/close on a file in
# a tight loop (mirrors QEMU's documented multiple-times-per-second write-fd
# cycling), while another process concurrently hammers
# open(O_RDONLY)/read/close on the SAME inode, for a fixed window -- trying
# to land a read-only release() exactly when the writer-count transitions to
# zero.
#
# Corruption signature under test: chunk 0's first megabyte ("header") holds
# real, previously-written, durable content. After the race window, ONE more
# write lands further into the same chunk (offset 1MB) -- mirroring a VM
# writing past its header. If write_buffers got wiped mid-race, the client
# may incorrectly believe the chunk doesn't exist and take the fresh-write
# path, fabricating zeros for the "gap" from offset 0 to the new write's
# offset -- even though that region has real, already-durable data. That
# would show up as the header reading back all-zero after a cold restart,
# while the later write itself is intact. This is the VM-108 signature
# exactly (chunk 0 header zeroed, other offsets in the same chunk held real
# data).
#
# Per CLAUDE.md: run this WITHOUT the fix first (should FAIL / reproduce),
# then WITH the fix (should PASS) -- see run_before_after.sh wrapper.
#
# Usage: bash scripts/repro_concurrent_rw_close_race.sh

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-race-mount
LOG=/tmp/dfs-race-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"
TARGET="$MOUNT/vm-race-target.img"
CHUNK_SIZE=$((4 * 1024 * 1024))
HEADER_SIZE=$((1 * 1024 * 1024))
RACE_SECONDS=${RACE_SECONDS:-20}

if pgrep -f "target/release/dfs-server" >/dev/null 2>&1 || pgrep -f "target/release/dfs-client" >/dev/null 2>&1; then
    echo "ABORT: dfs-server or dfs-client already running -- kill those first."
    pgrep -af "target/release/dfs-server|target/release/dfs-client"
    exit 2
fi
if ss -ltn 2>/dev/null | grep -qE ":(8900|8901|8902|8903|8904)\s"; then
    echo "ABORT: ports 8900-8904 already in use."
    exit 2
fi

cleanup_all() {
    pkill -9 -f "target/release/dfs-server" 2>/dev/null || true
    pkill -9 -f "target/release/dfs-client" 2>/dev/null || true
    pkill -9 -f "race_writer.py" 2>/dev/null || true
    pkill -9 -f "race_reader.py" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || fusermount -u -z "$MOUNT" 2>/dev/null || true
}

start_cluster() {
    for i in 1 2 3 4 5; do
        RUST_LOG=debug "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
            --log-level debug >> "$LOG/server${i}.log" 2>&1 &
    done
    sleep 3
}

mount_client() {
    env RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
        --log-file "$LOG/client.log" --allow-other --log-level debug \
        > /dev/null 2>&1 &
    sleep 2
    mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
    pgrep -f "dfs-client mount $MOUNT" | head -1
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$MOUNT" "$LOG" "$BASE" 2>/dev/null || true
mkdir -p "$MOUNT" "$LOG"

echo "=== Setting up + starting 5-node cluster ==="
cd "$REPO"
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null > /dev/null
start_cluster

CLIENT_PID=$(mount_client)
echo "Mounted. Client PID=$CLIENT_PID"

echo "=== Establishing real, durable content across all of chunk 0 ($CHUNK_SIZE bytes) ==="
python3 -c "
import os
header = bytes([(i * 73 + 5) % 256 for i in range($HEADER_SIZE)])
rest = bytes([(i * 31 + 17) % 256 for i in range($CHUNK_SIZE - $HEADER_SIZE)])
with open('$LOG/header_control.bin', 'wb') as f:
    f.write(header)
fd = os.open('$TARGET', os.O_WRONLY | os.O_CREAT, 0o644)
os.write(fd, header + rest)
os.fsync(fd)
os.close(fd)
print('chunk 0 fully written and fsynced')
"
HEADER_HASH=$(sha256sum "$LOG/header_control.bin" | awk '{print $1}')
echo "Header (offset 0..$HEADER_SIZE) control sha256: $HEADER_HASH"

echo "=== Racing: writer hammering open/write/close, reader hammering open/read/close, same inode, ${RACE_SECONDS}s ==="
cat > "$LOG/race_writer.py" <<'PYEOF'
import os, sys, time
path, seconds = sys.argv[1], float(sys.argv[2])
end = time.time() + seconds
n = 0
payload = b"\x99" * 4096
far_offset = 3 * 1024 * 1024
while time.time() < end:
    try:
        fd = os.open(path, os.O_WRONLY)
        os.pwrite(fd, payload, far_offset)
        os.close(fd)
        n += 1
    except OSError:
        pass
print(f"WRITER_DONE iterations={n}", flush=True)
PYEOF

cat > "$LOG/race_reader.py" <<'PYEOF'
import os, sys, time
path, seconds = sys.argv[1], float(sys.argv[2])
end = time.time() + seconds
n = 0
while time.time() < end:
    try:
        fd = os.open(path, os.O_RDONLY)
        os.read(fd, 4096)
        os.close(fd)
        n += 1
    except OSError:
        pass
print(f"READER_DONE iterations={n}", flush=True)
PYEOF

python3 "$LOG/race_writer.py" "$TARGET" "$RACE_SECONDS" > "$LOG/writer.out" 2>&1 &
WPID=$!
python3 "$LOG/race_reader.py" "$TARGET" "$RACE_SECONDS" > "$LOG/reader1.out" 2>&1 &
RPID1=$!
python3 "$LOG/race_reader.py" "$TARGET" "$RACE_SECONDS" > "$LOG/reader2.out" 2>&1 &
RPID2=$!
python3 "$LOG/race_reader.py" "$TARGET" "$RACE_SECONDS" > "$LOG/reader3.out" 2>&1 &
RPID3=$!
wait $WPID $RPID1 $RPID2 $RPID3
cat "$LOG/writer.out" "$LOG/reader1.out" "$LOG/reader2.out" "$LOG/reader3.out"

echo "=== Post-race: one more write further into the same chunk (offset 1MB), mirroring a VM writing past its header ==="
python3 -c "
import os
fd = os.open('$TARGET', os.O_WRONLY)
os.pwrite(fd, b'\xEE' * 4096, $HEADER_SIZE)
os.fsync(fd)
os.close(fd)
print('post-race write done')
"

echo "=== Restarting client cold (no cache masking) and reading chunk 0 back ==="
fusermount -u "$MOUNT" 2>/dev/null
if mountpoint -q "$MOUNT" 2>/dev/null; then
    kill -9 "$CLIENT_PID" 2>/dev/null
    sleep 1
    fusermount -u -z "$MOUNT" 2>/dev/null || true
fi
sleep 1
CLIENT_PID2=$(mount_client)
echo "Fresh client mounted. PID=$CLIENT_PID2"

python3 -c "
with open('$TARGET', 'rb') as f:
    data = f.read($HEADER_SIZE)
with open('$LOG/header_actual.bin', 'wb') as f:
    f.write(data)
zero_count = data.count(0)
print(f'header bytes read: {len(data)}, zero bytes: {zero_count}/{len(data)}')
"
ACTUAL_HEADER_HASH=$(sha256sum "$LOG/header_actual.bin" | awk '{print $1}')

echo ""
echo "Expected header sha256: $HEADER_HASH"
echo "Actual   header sha256: $ACTUAL_HEADER_HASH"

RESULT=1
if [ "$ACTUAL_HEADER_HASH" = "$HEADER_HASH" ]; then
    echo "=== RESULT: PASS -- chunk-0 header intact after the concurrent read/write close race ==="
    RESULT=0
else
    echo "=== RESULT: FAIL -- chunk-0 header corrupted (reproduced the VM-108 zeroing signature) ==="
    RESULT=1
fi

kill "$CLIENT_PID2" 2>/dev/null || true
sleep 1
cleanup_all
echo "Done. Logs in $LOG/"
exit $RESULT
