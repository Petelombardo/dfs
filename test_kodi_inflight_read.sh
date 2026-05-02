#!/bin/bash
# Reproduces the exact Kodi pattern from staging logs:
#
# 15:51:23.432  open O_RDWR → write 12032 → release (last writer)
# 15:51:23.466  open O_RDONLY (release_in_flight=1) ← reads DURING in-flight flush
# 15:51:23.470  PatchChunk 50da77 → 50da77 completes
# 15:51:23.499  open O_RDONLY (release_in_flight=1) ← another read during flush
# 15:51:23.532  open O_RDONLY (release_in_flight=0) ← flush done
#
# The hypothesis: the O_RDONLY opens at 15:51:23.466/.499 trigger expire_chunk_map
# and refresh, which races with the in-flight PatchChunk's metadata update.  The
# resulting chunk map may end up pointing to the OLD chunk_id, and subsequent
# reads fetch a chunk that's about to be (or has been) deleted by PatchChunk's
# unlink.

set -e
REPO=$(cd "$(dirname "$0")" && pwd)
BIN="$REPO/target/release"
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-inflight-mount
LOG=/tmp/dfs-inflight-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902"
CHUNK_SIZE=$((4 * 1024 * 1024))
T=/tmp/dfs-inflight-tmp-$$
PASS=0; FAIL=0

check() {
    local name="$1" result="$2"
    if [ "$result" = "PASS" ]; then
        echo "  PASS: $name"; PASS=$((PASS+1))
    else
        echo "  FAIL: $name ($3)"; FAIL=$((FAIL+1))
    fi
}

teardown() {
    fusermount -u "$MOUNT" 2>/dev/null || true
    sleep 0.3
    pkill -f "dfs-server" 2>/dev/null || true
    sleep 0.5
    rm -rf "$BASE" "$LOG" "$MOUNT" "$T"
}
trap teardown EXIT

echo "=== Kodi in-flight read pattern reproducer ==="
pkill -f "dfs-server" 2>/dev/null || true
fusermount -u "$MOUNT" 2>/dev/null || true
rm -rf "$BASE" "$LOG" "$MOUNT" "$T"
mkdir -p "$MOUNT" "$LOG" "$T"

cd "$REPO" && cargo build --release 2>&1 | tail -2

bash "$REPO/scripts/setup-cluster.sh" 3 2>/dev/null
for i in 1 2 3; do
    RUST_LOG=dfs_server=info "$BIN/dfs-server" start \
        --config "$BASE/node${i}/config.toml" > "$LOG/server${i}.log" 2>&1 &
done
sleep 2
RUST_LOG=dfs_client=info "$BIN/dfs-client" mount "$MOUNT" \
    --cluster "$CLUSTER" --log-file "$LOG/client.log" --allow-other --log-level info &
sleep 1
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -20 "$LOG/client.log"; exit 1; }

FILE="$MOUNT/recording.mpg"

echo ""
echo "=== Phase 1: Create recording (single chunk, 2.98MB like Full House) ==="
dd if=/dev/urandom of="$T/orig.bin" bs=1024 count=2914 2>/dev/null
dd if=/dev/zero bs=1 count=536 2>/dev/null >> "$T/orig.bin"
ls -l "$T/orig.bin"
cp "$T/orig.bin" "$FILE"
sync; sleep 0.5

# Generate three different headers — h0 (initial/orig), h1 (Kodi's seek), h2 (resume update)
dd if=/dev/urandom of="$T/h1.bin" bs=1 count=12032 2>/dev/null
cp "$T/h1.bin" "$T/h2.bin"
# h2 differs from h1 in byte 100 (simulating Resume field change)
printf '\xCC' | dd of="$T/h2.bin" bs=1 seek=100 count=1 conv=notrunc 2>/dev/null

echo ""
echo "=== Phase 2: Write h1 (PatchChunk d->h1) ==="
dd if="$T/h1.bin" of="$FILE" bs=1 seek=0 count=12032 conv=notrunc 2>/dev/null
sleep 0.3

# Verify
diff <(dd if="$FILE" bs=1 count=12032 2>/dev/null) "$T/h1.bin" >/dev/null 2>&1 \
    && check "Phase2: h1 written cleanly" PASS \
    || check "Phase2: h1 written cleanly" FAIL

echo ""
echo "=== Phase 3: Kodi pattern — h1 rewrite + immediate reads + h1 rewrite ==="
# This matches staging exactly: rapid O_RDWR sessions overlapping with O_RDONLY reads
python3 -c "
import os, threading, time

path = '$FILE'
h1 = open('$T/h1.bin', 'rb').read()

def writer():
    # Session A: write h1
    fd = os.open(path, os.O_RDWR)
    os.write(fd, h1)
    os.close(fd)
    # Session B: write h1 again immediately (the same-content write)
    fd = os.open(path, os.O_RDWR)
    os.write(fd, h1)
    os.close(fd)

def reader(results):
    # 5 rapid reads — matching the rapid O_RDONLY opens during in-flight flush
    for i in range(5):
        try:
            with open(path, 'rb') as f:
                data = f.read(12032)
                results.append(data)
        except Exception as e:
            results.append(f'ERR: {e}')
        time.sleep(0.001)

results = []
t1 = threading.Thread(target=writer)
t2 = threading.Thread(target=reader, args=(results,))
t1.start()
t2.start()
t1.join()
t2.join()

# All reads should see h1 (since both writes are h1)
mismatches = sum(1 for r in results if r != h1)
print(f'  Read results: {len(results)} reads, {mismatches} mismatches')
exit(0 if mismatches == 0 else 1)
"
[ $? -eq 0 ] && check "Phase3: concurrent reads during rapid h1 rewrites all return h1" PASS \
             || check "Phase3: concurrent reads during rapid h1 rewrites all return h1" FAIL

echo ""
echo "=== Phase 4: Now write h2 (Kodi's resume update) ==="
dd if="$T/h2.bin" of="$FILE" bs=1 seek=0 count=12032 conv=notrunc 2>/dev/null
sleep 0.5

# Read it back — should be h2, not h1
diff <(dd if="$FILE" bs=1 count=12032 2>/dev/null) "$T/h2.bin" >/dev/null 2>&1 \
    && check "Phase4: file = h2 after Kodi's resume update" PASS \
    || {
        FIRST_DIFF=$(python3 -c "
a=open('$FILE','rb').read(12032); b=open('$T/h2.bin','rb').read()
for i,(x,y) in enumerate(zip(a,b)):
    if x!=y:
        print(f'byte {i}: got=0x{x:02x} expected=0x{y:02x}'); break
")
        check "Phase4: file = h2 after Kodi's resume update" FAIL "$FIRST_DIFF"
       }

echo ""
echo "=== Phase 5: Read byte 100 = 0xCC ==="
GOT=$(dd if="$FILE" bs=1 skip=100 count=1 2>/dev/null | xxd -p)
[ "$GOT" = "cc" ] \
    && check "Phase5: byte 100 = 0xCC (Kodi's update preserved)" PASS \
    || check "Phase5: byte 100 = 0xCC (Kodi's update preserved)" FAIL "got 0x$GOT"

echo ""
echo "=== Phase 6: Stress — interleave h1/h2 writes with concurrent reads ==="
python3 -c "
import os, threading, time, random

path = '$FILE'
h1 = open('$T/h1.bin', 'rb').read()
h2 = open('$T/h2.bin', 'rb').read()

# Reset
fd = os.open(path, os.O_RDWR); os.write(fd, h1); os.close(fd)
time.sleep(0.3)

errors = []
final_state = []

def writer(headers):
    for h in headers:
        try:
            fd = os.open(path, os.O_RDWR)
            os.write(fd, h)
            os.close(fd)
            time.sleep(random.uniform(0.001, 0.01))
        except Exception as e:
            errors.append(f'W: {e}')

def reader():
    for _ in range(20):
        try:
            with open(path, 'rb') as f:
                data = f.read(12032)
                # Either h1 or h2 is valid — anything else is corruption
                if data != h1 and data != h2:
                    errors.append(f'R: read returned neither h1 nor h2')
        except Exception as e:
            errors.append(f'R: {e}')
        time.sleep(0.002)

# Two writers alternating between h1 and h2, plus 4 concurrent readers
threads = [
    threading.Thread(target=writer, args=([h1, h2, h1, h2, h1],)),
    threading.Thread(target=writer, args=([h2, h1, h2, h1, h2],)),
] + [threading.Thread(target=reader) for _ in range(4)]
for t in threads: t.start()
for t in threads: t.join()

# Final state must be either h1 or h2, and reads of THAT state must be consistent
time.sleep(0.5)
with open(path, 'rb') as f:
    final = f.read(12032)

if final == h1: state = 'h1'
elif final == h2: state = 'h2'
else: state = 'CORRUPT'

print(f'  Final state: {state}, errors: {len(errors)}')
if errors:
    for e in errors[:5]: print(f'    {e}')
exit(0 if (not errors and state != 'CORRUPT') else 1)
"
[ $? -eq 0 ] && check "Phase6: stress — final state is consistent, no corruption" PASS \
             || check "Phase6: stress — final state is consistent, no corruption" FAIL

echo ""
echo "════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed"
echo "════════════════════════════════"
[ $FAIL -eq 0 ] && exit 0 || exit 1
