#!/bin/bash
# Upgrade test: build state on an OLD release, upgrade the cluster in place to this tree's
# release, and check that no file's content changed.
#
# Usage: test_upgrade.sh <old-repo-dir> [<new-bin-dir>]
#   <old-repo-dir>  a checkout of the release staging runs, already `cargo build --release`d
#                   (e.g. a worktree at c8637fb)
#   <new-bin-dir>   the binaries to upgrade to (default: this tree's target/release)
#
# Why: the suites always start a fresh cluster on one build, so they never see state an
# older build left behind. The 10-20 staging deploy (Legata ac7a02f0) passed both suites
# and still walked VM-108 disk-0 chunk 2830 back to older content: the new leader rebuilt
# slots from records the old build had written. This test writes and patches files on the
# old build (with folds and restarts, as staging does), upgrades node by node the way a
# deploy does, then forces a rebuild of every file's chunk map and compares every byte
# against a local reference copy.
#
# Uses the suite's ports (8900-8904) and directories: never run it while a suite runs.
set -e

OLD_REPO=$(cd "${1:?usage: test_upgrade.sh <old-repo-dir> [<new-bin-dir>]}" && pwd)
REPO=$(cd "$(dirname "$0")/.." && pwd)
OLD_BIN="$OLD_REPO/target/release"
NEW_BIN="${2:-$REPO/target/release}"
ADMIN="$REPO/target/release/dfs-admin"   # this tree's admin: `file revalidate`, and `cluster status` names the leader
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-mount
LOG=/tmp/dfs-test-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
REF=/tmp/dfs-upgrade-ref
PASS=0; FAIL=0
CLIENT_PID=""

# Same caps as test_local_suite.sh: 5 servers + 1 client share this box.
export DFS_CHUNK_RING_CAPACITY=8
export DFS_DELTA_RING_CAPACITY=8
export DFS_MAX_CACHE_CHUNKS=8
export DFS_WRITE_BUFFER_CAP_MB=32
export DFS_LEASE_CLUSTER_SIZE=5
# Staging runs ordering OFF; the upgrade keeps it OFF.
export DFS_ORDERED_WRITES=${DFS_ORDERED_WRITES:-0}

check() {
    if [ "$2" = "PASS" ]; then echo "  PASS: $1"; PASS=$((PASS+1))
    else echo "  FAIL: $1"; FAIL=$((FAIL+1)); fi
}

for b in "$OLD_BIN/dfs-server" "$OLD_BIN/dfs-client" "$NEW_BIN/dfs-server" "$NEW_BIN/dfs-client" "$ADMIN"; do
    [ -x "$b" ] || { echo "missing $b"; exit 1; }
done
echo "old: $OLD_REPO ($(git -C "$OLD_REPO" log -1 --format=%h 2>/dev/null)) $(md5sum < "$OLD_BIN/dfs-server" | cut -c1-8)"
echo "new: $NEW_BIN $(md5sum < "$NEW_BIN/dfs-server" | cut -c1-8)"

# ── cleanup (as test_local_suite.sh) ─────────────────────────────────────────
pkill -x dfs-server 2>/dev/null || true
pkill -x dfs-client 2>/dev/null || true
sleep 0.5
for m in "$MOUNT" /tmp/dfs-mount2 /tmp/dfs-mount3 /tmp/dfs-mount-fresh; do fusermount -uz "$m" 2>/dev/null || true; done
rm -rf $BASE $MOUNT $LOG $REF
mkdir -p $MOUNT $LOG $REF

start_server() {   # start_server <bin-dir> <node#> <log-suffix>
    RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 "$1/dfs-server" start --config "$BASE/node$2/config.toml" \
        --log-level info >> "$LOG/server$2-$3.log" 2>&1 &
}
stop_server() {    # stop_server <node#>: SIGTERM, as systemd stop does
    local pat="dfs-server start --config $BASE/node$1/config.toml"
    pkill -TERM -f "$pat" 2>/dev/null || true
    for _ in $(seq 1 300); do pgrep -f "$pat" >/dev/null || return 0; sleep 0.1; done
    echo "  node$1 ignored SIGTERM for 30 s; SIGKILL"
    pkill -9 -f "$pat" 2>/dev/null || true
    sleep 0.5
}
start_client() {   # start_client <bin-dir> <log-name>
    RUST_LOG=info "$1/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
        --log-file "$LOG/$2.log" --allow-other --log-level debug &
    CLIENT_PID=$!
    for _ in $(seq 1 50); do mountpoint -q "$MOUNT" && return 0; sleep 0.2; done
    echo "MOUNT FAILED"; tail -20 "$LOG/$2.log"; exit 1
}
stop_client() {
    sync "$MOUNT" 2>/dev/null || true
    fusermount -u "$MOUNT" 2>/dev/null || fusermount -uz "$MOUNT" 2>/dev/null || true
    [ -n "$CLIENT_PID" ] && for _ in $(seq 1 100); do kill -0 "$CLIENT_PID" 2>/dev/null || break; sleep 0.1; done
    CLIENT_PID=""
}
leader() {
    "$ADMIN" --cluster "$CLUSTER" --format json cluster status 2>/dev/null \
        | python3 -c "import json,sys; print(json.load(sys.stdin).get('leader') or '')" 2>/dev/null
}
wait_leader() {
    local l=""
    for _ in $(seq 1 60); do l=$(leader) || true; [ -n "$l" ] && { echo "$l"; return 0; }; sleep 1; done
    return 1
}

# compare <label>: every reference file against the mount, 4 MB chunk by chunk.
# Prints each bad chunk and returns the count of bad files.
compare() {
    python3 - "$REF" "$MOUNT" "$1" <<'PY'
import os, sys, hashlib
ref, mnt, label = sys.argv[1:4]
CH = 4 << 20
bad = 0
for name in sorted(os.listdir(ref)):
    want = open(os.path.join(ref, name), "rb").read()
    try:
        got = open(os.path.join(mnt, name), "rb").read()
    except OSError as e:
        print(f"  {label}: {name}: read failed: {e}"); bad += 1; continue
    if got == want:
        continue
    bad += 1
    if len(got) != len(want):
        print(f"  {label}: {name}: size {len(got)} != {len(want)}")
    for i in range(0, max(len(got), len(want)), CH):
        g, w = got[i:i+CH], want[i:i+CH]
        if g != w:
            print(f"  {label}: {name} chunk {i // CH}: got {hashlib.md5(g).hexdigest()[:12]} want {hashlib.md5(w).hexdigest()[:12]}")
# Files that should be gone (deleted on the old build) must stay gone.
for name in sorted(os.listdir(mnt)):
    if name.startswith("u") and not os.path.exists(os.path.join(ref, name)):
        print(f"  {label}: {name}: present on the mount but deleted/renamed away on the old build"); bad += 1
sys.exit(min(bad, 100))
PY
}

# churn <rounds> <pause>: random fsynced patches into the files, mirrored into $REF. Each
# write is 4K-64K at a 4K-aligned offset; about one in eight straddles a chunk boundary.
churn() {
    python3 - "$REF" "$MOUNT" "$1" "$2" <<'PY'
import os, sys, random, time
ref, mnt, rounds, pause = sys.argv[1], sys.argv[2], int(sys.argv[3]), float(sys.argv[4])
CH = 4 << 20
names = sorted(n for n in os.listdir(ref))
for r in range(rounds):
    for name in random.sample(names, k=min(3, len(names))):
        size = os.path.getsize(os.path.join(ref, name))
        n = random.choice([4096, 16384, 65536])
        if random.random() < 0.125 and size > CH:
            off = (random.randrange(1, size // CH) * CH) - 8192
        else:
            off = random.randrange(0, (size - n) // 4096) * 4096
        data = os.urandom(n)
        fd = os.open(os.path.join(mnt, name), os.O_RDWR)
        os.pwrite(fd, data, off); os.fsync(fd); os.close(fd)
        with open(os.path.join(ref, name), "r+b") as f:
            f.seek(off); f.write(data)
    time.sleep(pause)
PY
}

# ── Phase 1: the old release ─────────────────────────────────────────────────
echo "=== Phase 1: build state on the old release ==="
# What scripts/setup-cluster.sh does, minus its `pkill -f dfs-server`, which also kills any
# shell whose command line mentions dfs-server (this test's own caller, once).
for i in 1 2 3 4 5; do
    d="$BASE/node$i"
    "$OLD_BIN/dfs-server" init --data-dir "$d/data" --meta-dir "$d/metadata" --config "$d/config.toml" >/dev/null
    sed -i "s/listen_addr = \"0.0.0.0:8900\"/listen_addr = \"127.0.0.1:$((8899 + i))\"/" "$d/config.toml"
    [ $i -gt 1 ] && sed -i 's/seed_nodes = \[\]/seed_nodes = ["127.0.0.1:8900"]/' "$d/config.toml"
    sed -i -e 's/heartbeat_interval_secs = .*/heartbeat_interval_secs = 5/' \
           -e 's/failure_timeout_secs = .*/failure_timeout_secs = 120/' \
           -e 's/healing_delay_secs = .*/healing_delay_secs = 10/' "$d/config.toml"
done
for i in 1 2 3 4 5; do start_server "$OLD_BIN" $i old; done
sleep 3
start_client "$OLD_BIN" client-old
for i in 0 1 2 3 4 5; do
    dd if=/dev/urandom of="$REF/u$i.bin" bs=1M count=$(( i < 4 ? 16 : 32 )) status=none
    cp "$REF/u$i.bin" "$MOUNT/u$i.bin"
done
dd if=/dev/urandom of="$REF/u6.bin" bs=1M count=12 status=none
cp "$REF/u6.bin" "$MOUNT/u6.bin"
dd if=/dev/urandom of="$MOUNT/u7.bin" bs=1M count=8 status=none
sync "$MOUNT"
echo "  wrote 8 files; patching with folds in between"
churn 30 1.5          # paced: the client folds a slot ~6 s after its last patch
sleep 10
L=$(wait_leader); LN=$(( ${L##*:} - 8899 ))
echo "  restarting the leader (node$LN) on the old release"
stop_server $LN; start_server "$OLD_BIN" $LN old
sleep 8
churn 20 1.5
echo "  restarting every node on the old release, one at a time"
for i in 1 2 3 4 5; do stop_server $i; start_server "$OLD_BIN" $i old; sleep 5; done
wait_leader >/dev/null
churn 20 1.5
rm -f "$MOUNT/u7.bin"
mv "$MOUNT/u6.bin" "$MOUNT/u6r.bin"; mv "$REF/u6.bin" "$REF/u6r.bin"
churn 10 0            # the last writes stay unfolded patch tokens across the upgrade
sync "$MOUNT"
sleep 5
OLD_BAD=0; compare "old" || OLD_BAD=$?
[ "$OLD_BAD" = 0 ] && check "U0 the old release itself reads back every byte it acked" PASS \
    || check "U0 the old release reads back $OLD_BAD file(s) wrong BEFORE the upgrade (an old-release bug, not the upgrade's)" FAIL
# A file the old release already lost would fail every later phase too and hide whatever the
# new release does. Re-baseline it to what the old release serves, so U1-U4 judge the upgrade
# and the new release only.
if [ "$OLD_BAD" != 0 ]; then
    for f in "$REF"/*; do
        n=$(basename "$f")
        cmp -s "$f" "$MOUNT/$n" || { cp "$MOUNT/$n" "$f" && echo "  re-baselined $n to the old release's content"; }
    done
fi
stop_client

# ── Phase 2: upgrade node by node ────────────────────────────────────────────
echo "=== Phase 2: upgrade, one node at a time ==="
UPGRADE_TS=$(date -u +%Y-%m-%dT%H:%M:%S)
for i in 1 2 3 4 5; do
    stop_server $i
    start_server "$NEW_BIN" $i new
    sleep 5
done
L=$(wait_leader) || { echo "no leader after the upgrade"; exit 1; }
echo "  leader after the upgrade: $L"
sleep 10
start_client "$NEW_BIN" client-new

BAD=0; compare "U1" || BAD=$?
[ "$BAD" = 0 ] && check "U1 after the upgrade every file reads back as acked" PASS \
    || check "U1 $BAD file(s) changed across the upgrade" FAIL

# ── Phase 3: rebuild every file's chunk map on the new leader ────────────────
echo "=== Phase 3: rebuild every file's chunk map ==="
REVAL_ERR=0
for f in $(ls "$REF"); do
    first=$("$ADMIN" --cluster "$L" --format json file info "/$f" 2>/dev/null \
        | python3 -c "import json,sys; print(json.load(sys.stdin)['chunk_locations'][0]['chunk_id'])" 2>/dev/null) || first=""
    fid=$("$ADMIN" --cluster "$L" --format json file raw-location "$first" 2>/dev/null \
        | python3 -c "import json,sys; print(json.loads(sys.stdin.readline())['location']['file_id'])" 2>/dev/null) || fid=""
    if [ -z "$fid" ]; then echo "  $f: no file id (first chunk ${first:0:12})"; REVAL_ERR=$((REVAL_ERR+1)); continue; fi
    "$ADMIN" --cluster "$L" file revalidate --file-id "$fid" --chunk 0 --distrust "$first" >>"$LOG/revalidate.log" 2>&1 \
        || { echo "  $f: revalidate failed"; REVAL_ERR=$((REVAL_ERR+1)); }
done
[ "$REVAL_ERR" = 0 ] && check "U2 the leader rebuilt every file's chunk map" PASS \
    || check "U2 $REVAL_ERR file(s) could not be rebuilt" FAIL
stop_client; start_client "$NEW_BIN" client-new2
BAD=0; compare "U3" || BAD=$?
[ "$BAD" = 0 ] && check "U3 after a rebuild of every slot, a fresh client reads every file as acked" PASS \
    || check "U3 $BAD file(s) changed when the new leader rebuilt their chunk maps" FAIL

# ── Phase 4: the one-time cleanup (B), then new writes on the new release ────
echo "=== Phase 4: reconcile, then write on the new release ==="
"$ADMIN" --cluster "$L" healing reconcile >>"$LOG/reconcile.log" 2>&1 || true
sleep 20
churn 20 1.5
sync "$MOUNT"
stop_client; start_client "$NEW_BIN" client-new3
BAD=0; compare "U4" || BAD=$?
[ "$BAD" = 0 ] && check "U4 after reconcile and new patches, every file reads back as acked" PASS \
    || check "U4 $BAD file(s) wrong after reconcile + new writes" FAIL

# ── Logs ─────────────────────────────────────────────────────────────────────
DL=$(cat $LOG/server*-new.log | grep -c "DATA LOSS" || true)
[ "$DL" = 0 ] && check "U5 no server logged DATA LOSS after the upgrade" PASS \
    || check "U5 $DL DATA LOSS line(s) after the upgrade (grep 'DATA LOSS' $LOG/server*-new.log)" FAIL
PANIC=$(cat $LOG/server*-new.log | grep -c "panicked" || true)
[ "$PANIC" = 0 ] && check "U6 no new-release server panicked" PASS || check "U6 $PANIC panic(s) in the new servers' logs" FAIL
echo "  info: slot rebuild corrections: $(cat $LOG/server*-new.log | grep -c 'Slot rebuild:' || true)," \
     "revalidate corrections: $(cat $LOG/server*-new.log | grep -c 'RevalidateChunkSlot: .* corrected' || true)," \
     "client EIOs: $(cat $LOG/client-new*.log | grep -ci 'EIO' || true)"

stop_client
pkill -x dfs-server 2>/dev/null || true
echo ""
echo "  Results: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
