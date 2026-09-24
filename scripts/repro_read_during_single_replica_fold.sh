#!/bin/bash
# Repro for the 2026-09-24 VM-108 hung boot (errors on sdb, no EIO anywhere in the
# client log). On staging, a graceful guest shutdown wrote 4 KiB into chunk 0 of
# vm-108-disk-1.qcow2 (offset 1376256 — the qcow2 header/L1/L2 region every sdb
# read resolves through). ~16 s into the next boot, gluster1 ALONE folded that
# slot (the debounce backstop lets exactly one replica fold; peers only get a
# ReplicatePatchFold notice) and deleted its old base, while gluster2 still held
# base+patch. At the same time the leader stalled: requests from the hypervisor
# sat up to 26.5 s on gluster1 and all released together at "Discovery complete".
#
# Question this answers: does a guest-style reader of a just-patched chunk see a
# stall, an error, or wrong bytes while one replica folds it on its own —
#   MODE=fold               the fold alone
#   MODE=fold+folder-stall  the same fold, plus the node that performed it
#                           SIGSTOPped for STALL_SECS (default 27, matching
#                           gluster1) ~6 s after it fired
# Exactly one dimension differs between the two modes.
#
# READ_PATTERN (env) picks how "sdb" is read, independently of MODE:
#   random      (default) 4 KiB O_DIRECT reads, biased to the patched region —
#               served by the byte-range path (1 s per-replica timeout)
#   sequential  64 KiB O_DIRECT reads walking the whole image and wrapping —
#               the whole-chunk/pipeline path, whose per-replica waits are 10-30 s
#               (read_chunk_from_server 10 s/attempt; pipeline header 30 s)
#
# Why the folding node and not "the leader": on staging gluster1 was both — the
# leader whose discovery pass stalled it AND the sole holder of the fold output
# (it held a ReadChunkRange from server4 for 26.5 s). Locally the leader is
# rarely a replica of these chunks, and a first leader-stall run proved it: 0 of
# the ~3000 reads during the stall were sent to the stopped leader, so it tested
# nothing about reads. Stopping the folder reproduces the part that matters to a
# reader — the one node with the new identity's bytes stops answering.
#
# Pass/fail (per mode): every read must return the expected bytes, none may
# error, and none may take longer than GUEST_TIMEOUT_SECS (30 = Linux SCSI
# default; a read past that is what the guest reports as a disk error + hang).
# The run is INCONCLUSIVE (exit 3) unless the fold happened AND reads of sdb's
# chunk 0 specifically reached the servers after it (and, in the stall mode, the
# stalled node). Counted from the client's debug `READ TRACE` / `SLOW READ` lines,
# which carry each read's path, offset and network steps. Two earlier versions of
# this guard passed vacuously: first every read was a client cache hit; then the
# guard counted ALL server reads, and the "sda" traffic satisfied it while sdb's
# chunks sat in the cache the whole time.
#
# Usage: ./scripts/repro_read_during_single_replica_fold.sh [fold|fold+folder-stall] [read_secs]

set -u

MODE="${1:-fold}"
READ_SECS="${2:-75}"
STALL_SECS="${STALL_SECS:-27}"
READ_PATTERN="${READ_PATTERN:-random}"
case "$READ_PATTERN" in random|sequential) ;; *) echo "unknown READ_PATTERN: $READ_PATTERN"; exit 2 ;; esac
GUEST_TIMEOUT_SECS=30

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-repro-rdfold
MOUNT=/tmp/dfs-repro-rdfold-mount
LOG=/tmp/dfs-repro-rdfold-logs
PORT0=8980
CLUSTER="127.0.0.1:8980,127.0.0.1:8981,127.0.0.1:8982,127.0.0.1:8983,127.0.0.1:8984"
BIN="$REPO/target/release"
FILE="$MOUNT/vm-disk-1.img"     # "sdb" — chunk 0 gets the patch and the fold
FILE_MB=32   # 8 chunks > the 4-chunk client cache, so a sequential walk evicts chunk 0
OTHER="$MOUNT/vm-disk-0.img"    # "sda" — concurrent reads that keep evicting sdb's chunk 0
OTHER_MB=64
PATCH_OFFSET=1376256            # the exact offset the staging shutdown wrote

case "$MODE" in fold|fold+folder-stall) ;; *) echo "unknown mode: $MODE"; exit 2 ;; esac

strip_ansi() { sed "s/\x1b\[[0-9;]*m//g" "$@"; }

# Dev box has no swap and a small root disk; a 2026-09-24 run of this script
# filled it (cause not captured — the next run's cleanup deleted the evidence).
# Log free space at every phase and abort before a lockup.
MIN_FREE_KB=$((3 * 1024 * 1024))
disk_check() {  # <phase>
    local avail_kb
    avail_kb=$(df -k / | awk 'NR==2 {print $4}')
    echo "$(date -u +%H:%M:%S) disk free ${avail_kb}KB at: $1" >> "$LOG/disk.log"
    if [ "$avail_kb" -lt "$MIN_FREE_KB" ]; then
        echo "DISK GUARD: ${avail_kb}KB free at '$1' — aborting (largest under $BASE / $LOG):"
        du -ah "$BASE" "$LOG" 2>/dev/null | sort -h | tail -8
        return 2
    fi
}

cleanup_all() {
    # Resume a stopped leader first, or pkill's SIGTERM is never delivered.
    pkill -CONT -f "dfs-server.*dfs-repro-rdfold" 2>/dev/null || true
    pkill -f "dfs-server.*dfs-repro-rdfold" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    # Wait for the processes to actually exit (servers drain on SIGTERM). A 0.5 s
    # sleep here let a previous run's server keep its port and keep writing into
    # $BASE while the next run started: "Failed to bind to 127.0.0.1:8982" and
    # "rm: ... Directory not empty" on 2026-09-24.
    for _ in $(seq 1 60); do
        pgrep -f "dfs-server.*dfs-repro-rdfold|dfs-client mount $MOUNT" >/dev/null || break
        sleep 0.5
    done
    pkill -9 -f "dfs-server.*dfs-repro-rdfold" 2>/dev/null || true
    pkill -9 -f "dfs-client mount $MOUNT" 2>/dev/null || true
    fusermount -u "$MOUNT" 2>/dev/null || true
    for _ in $(seq 1 30); do
        ss -Htln | grep -qE "127\.0\.0\.1:1?898[0-4] " || break
        sleep 0.5
    done
}
trap cleanup_all EXIT

echo "=== [$MODE] Cleaning up any previous run ==="
cleanup_all
rm -rf "$BASE" "$MOUNT" "$LOG"
mkdir -p "$MOUNT" "$LOG" "$BASE"

echo "=== Initializing 5-node cluster on ports $PORT0-$((PORT0 + 4)) ==="
for i in 1 2 3 4 5; do
    NODE_DIR="$BASE/node${i}"
    PORT=$((PORT0 + i - 1))
    "$BIN/dfs-server" init --data-dir "$NODE_DIR/data" --meta-dir "$NODE_DIR/metadata" --config "$NODE_DIR/config.toml" >/dev/null
    sed -i "s/listen_addr = \"0.0.0.0:8900\"/listen_addr = \"127.0.0.1:${PORT}\"/" "$NODE_DIR/config.toml"
    if [ $i -gt 1 ]; then
        sed -i "s/seed_nodes = \[\]/seed_nodes = [\"127.0.0.1:${PORT0}\"]/" "$NODE_DIR/config.toml"
    fi
done
for i in 1 2 3 4 5; do
    RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" >> "$LOG/server${i}.log" 2>&1 &
done
sleep 3

# DFS_MAX_CACHE_CHUNKS=4: server4 serves many busy VM disks, so a guest's chunk is
# not pinned in the client's chunk cache. With the default cache every read after
# the guest write is served from client memory and the fold is never touched.
# debug: needed to count the read RPCs actually sent (INCONCLUSIVE check). The
# SLOW READ / READ PENDING reports under test are WARN, same as at staging's info.
DFS_MAX_CACHE_CHUNKS=4 "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "SETUP FAILED: mount"; tail -30 "$LOG/client.log"; exit 2; }
echo "Mounted."

write_image() {  # <path> <mb> <expected-copy>
    python3 -c '
import os, sys
path, mb, expected = sys.argv[1], int(sys.argv[2]), sys.argv[3]
data = os.urandom(mb * 1024 * 1024)
with open(path, "wb") as f:
    f.write(data); f.flush(); os.fsync(f.fileno())
open(expected, "wb").write(data)
' "$@"
}

disk_check "mounted" || exit 2
echo "=== Writing images (sdb ${FILE_MB}MB, sda ${OTHER_MB}MB) and letting them settle ==="
write_image "$FILE" "$FILE_MB" "$BASE/expected.bin" || { echo "SETUP FAILED: writing sdb"; exit 2; }
write_image "$OTHER" "$OTHER_MB" "$BASE/expected_other.bin" || { echo "SETUP FAILED: writing sda"; exit 2; }
sync "$MOUNT"
disk_check "images written" || exit 2
sleep 35
disk_check "images settled" || exit 2

echo "=== Guest 'shutdown' write: 4 KiB at offset $PATCH_OFFSET of sdb chunk 0, fsync, close ==="
python3 -c '
import os, sys
path, off, expected = sys.argv[1], int(sys.argv[2]), sys.argv[3]
patch = os.urandom(4096)
fd = os.open(path, os.O_RDWR)
os.pwrite(fd, patch, off); os.fsync(fd); os.close(fd)
with open(expected, "r+b") as f:
    f.seek(off); f.write(patch)
' "$FILE" "$PATCH_OFFSET" "$BASE/expected.bin" || { echo "SETUP FAILED: patch write"; exit 2; }
PATCH_AT=$(date +%s.%N)

LEADER_ADDR=$(strip_ansi "$LOG/client.log" | grep -a "Leader node:" | tail -1 | sed -E 's/.*\((127\.0\.0\.1:[0-9]+)\).*/\1/')
LEADER_NODE=$(( ${LEADER_ADDR##*:} - PORT0 + 1 ))
LEADER_PID=$(pgrep -f "dfs-server start --config $BASE/node${LEADER_NODE}/config.toml" | head -1)
echo "Leader: $LEADER_ADDR (node$LEADER_NODE, pid $LEADER_PID)"

if [ "$MODE" = "fold+folder-stall" ]; then
    # Stall the node that folded, matching the staging order: the debounce fold
    # went at +16 s and gluster1's stall started ~6 s after it.
    (
        FOLDER=""
        for _ in $(seq 1 120); do
            FOLDER=$(grep -la "Single fold: file .* chunk_idx 0 " "$LOG"/server*.log 2>/dev/null | head -1 | sed -E 's/.*server([0-9])\.log/\1/')
            [ -n "$FOLDER" ] && break
            sleep 0.5
        done
        [ -n "$FOLDER" ] || { echo "no fold seen — not stalling" >> "$LOG/stall.log"; exit 0; }
        FOLDER_PID=$(pgrep -f "dfs-server start --config $BASE/node${FOLDER}/config.toml" | head -1)
        sleep 6
        echo "$(date -u +%Y-%m-%dT%H:%M:%S.%N) STOP folder node$FOLDER pid=$FOLDER_PID (leader is node$LEADER_NODE)" >> "$LOG/stall.log"
        kill -STOP "$FOLDER_PID"
        sleep "$STALL_SECS"
        kill -CONT "$FOLDER_PID"
        echo "$(date -u +%Y-%m-%dT%H:%M:%S.%N) CONT folder node$FOLDER pid=$FOLDER_PID" >> "$LOG/stall.log"
    ) &
fi

# Out of space mid-read: tear the cluster down (stops the writers), then let the
# main flow's "reads done" check abort the run.
( while sleep 5; do disk_check "during reads" || { cleanup_all; break; }; done ) &
DISK_SAMPLER=$!
echo "=== Reading sdb ($READ_PATTERN) + sda traffic for ${READ_SECS}s through the fold (O_DIRECT) ==="
python3 -c '
import mmap, os, random, sys, time, threading
path, expected_path, secs, patch_at, guest_to, other_path, other_expected_path, pattern = sys.argv[1:9]
secs, patch_at, guest_to = float(secs), float(patch_at), float(guest_to)
expected = open(expected_path, "rb").read()
other_expected = open(other_expected_path, "rb").read()
CHUNK, BS = 4 * 1024 * 1024, 4096
fd = os.open(path, os.O_RDONLY | os.O_DIRECT)
ofd = os.open(other_path, os.O_RDONLY | os.O_DIRECT)
SEQ_BS = 64 * 1024
buf = mmap.mmap(-1, SEQ_BS)   # page-aligned, as O_DIRECT requires
random.seed(7)
# Guest-shaped: mostly the patched (qcow2 metadata) region, some elsewhere in chunk 0.
hot = [(1376256 // BS + d) * BS for d in range(-8, 9)]
stats = dict(reads=0, errors=0, mismatches=0, over_1s=0, over_guest=0, max_lat=0.0)
inflight = {"since": None, "what": ""}

def watchdog():
    # Reports a read that is stuck NOW — a hung read never returns to be timed.
    while True:
        time.sleep(1)
        s = inflight["since"]
        w = inflight["what"]
        if s is not None and time.time() - s > guest_to:
            print(f"t=+{time.time()-patch_at:.1f}s READ STUCK {w} for {time.time()-s:.1f}s (> guest timeout)", flush=True)
threading.Thread(target=watchdog, daemon=True).start()

def one(disk, f, off, exp, size=BS):
    t0 = time.time()
    inflight["since"], inflight["what"] = t0, f"{disk} off={off}"
    try:
        n = os.preadv(f, [memoryview(buf)[:size]], off)
        got = bytes(buf[:n])
    except OSError as e:
        inflight["since"] = None
        stats["errors"] += 1
        print(f"t=+{t0-patch_at:.1f}s {disk} off={off} ERROR {e}", flush=True)
        return
    inflight["since"] = None
    lat = time.time() - t0
    stats["reads"] += 1
    stats["max_lat"] = max(stats["max_lat"], lat)
    if lat > 1: stats["over_1s"] += 1
    if lat > guest_to: stats["over_guest"] += 1
    if got != exp[off:off + size]:
        stats["mismatches"] += 1
        print(f"t=+{t0-patch_at:.1f}s {disk} off={off} MISMATCH lat={lat*1000:.0f}ms", flush=True)
    elif lat > 1:
        print(f"t=+{t0-patch_at:.1f}s {disk} off={off} slow read {lat*1000:.0f}ms", flush=True)

end = time.time() + secs
seq_pos = 0
sda_pos = 0
while time.time() < end:
    if pattern == "sequential":
        one("sdb", fd, seq_pos, expected, SEQ_BS)
        seq_pos = (seq_pos + SEQ_BS) % len(expected)
    else:
        off = random.choice(hot) if random.random() < 0.7 else random.randrange(0, CHUNK // BS) * BS
        one("sdb", fd, off, expected)
    # "sda" traffic: a sequential 64 KiB walk, like a booting root disk. It must be
    # sequential: that is the whole-chunk path, which shares the client chunk cache
    # with sdb and so evicts sdb chunk 0. Random 4 KiB sda reads go through the
    # separate byte-range cache and never evict it — with them, the client fetched
    # sdb chunk 0 whole ~7 s after the patch and served every later read from
    # memory, so the fold was never exercised (2026-09-24, runs 1-2).
    for _ in range(3):
        one("sda", ofd, sda_pos, other_expected, SEQ_BS)
        sda_pos = (sda_pos + SEQ_BS) % len(other_expected)
    time.sleep(0.02)
s = stats
print("RESULT reads={} errors={} mismatches={} over_1s={} over_guest_timeout={} max_latency_ms={:.0f}".format(
    s["reads"], s["errors"], s["mismatches"], s["over_1s"], s["over_guest"], s["max_lat"] * 1000), flush=True)
' "$FILE" "$BASE/expected.bin" "$READ_SECS" "$PATCH_AT" "$GUEST_TIMEOUT_SECS" "$OTHER" "$BASE/expected_other.bin" "$READ_PATTERN" > "$LOG/reader.log" 2>&1

kill "$DISK_SAMPLER" 2>/dev/null
disk_check "reads done" || exit 2
echo "=== Evidence ==="
echo "--- folds of sdb chunk 0 (which node(s), when):"
for i in 1 2 3 4 5; do
    strip_ansi "$LOG/server${i}.log" | grep -a "Single fold: file .* chunk_idx 0 " | cut -c1-27 | sed "s/^/  node$i /"
done
echo "--- server-side reads that found a fold link without the bytes:"
grep -ac "Failed to read chunk range\|Failed to open chunk file" "$LOG"/server*.log | sed 's/^/  /'
[ -f "$LOG/stall.log" ] && { echo "--- leader stall:"; sed 's/^/  /' "$LOG/stall.log"; }
echo "--- client SLOW READ / READ PENDING / EIO:"
strip_ansi "$LOG/client.log" | grep -aE "SLOW READ|READ PENDING|FUSE read error" | cut -c1-400 | head -20 | sed 's/^/  /'
echo "--- reader:"
sed 's/^/  /' "$LOG/reader.log" | tail -25

RESULT=$(grep "^RESULT" "$LOG/reader.log")
FOLDS=$(grep -ah "Single fold: file .* chunk_idx 0 " "$LOG"/server*.log | wc -l)
FOLD_TS=$(grep -ah "Single fold: file .* chunk_idx 0 " "$LOG"/server*.log | strip_ansi | cut -c1-27 | sort | head -1)
# Read RPCs the client actually sent after the fold (debug lines): pooled
# ReadChunk connections plus ReadChunkRange sends.
SERVER_READS=$(strip_ansi "$LOG/client.log" | awk -v t="$FOLD_TS" '$1 > t' \
    | grep -acE "Reusing pooled connection to|Creating new connection to|Sending request to .*: ReadChunkRange")
# Reads of sdb chunk 0 (path vm-disk-1.img, offset < 4 MiB) that made a network
# read step — the only reads that exercise the fold. Prints matching lines.
sdb_chunk0_net_reads() {  # <from-ts> [to-ts]
    strip_ansi "$LOG/client.log" \
        | awk -v s="$1" -v e="${2:-9999}" '$1 > s && $1 <= e' \
        | grep -aE "(READ TRACE|SLOW READ|READ PENDING) .*path=/vm-disk-1\.img .*ReadChunk" \
        | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^offset=/) { split($i, a, "="); if (a[2] + 0 < 4194304) print; break } }'
}
SDB_READS=$(sdb_chunk0_net_reads "$FOLD_TS" | wc -l)
STALLED_READS=""
if [ "$MODE" = "fold+folder-stall" ] && grep -q "STOP folder" "$LOG/stall.log" 2>/dev/null; then
    S_T=$(grep "STOP folder" "$LOG/stall.log" | cut -d' ' -f1 | cut -c1-26)
    E_T=$(grep "CONT folder" "$LOG/stall.log" | cut -d' ' -f1 | cut -c1-26)
    S_PORT=$((PORT0 + $(grep "STOP folder" "$LOG/stall.log" | sed -E 's/.*node([0-9]).*/\1/') - 1))
    # A read is reported when it completes, so allow reads that finished up to
    # 60 s after the stall ended to count.
    E_T_LATE=$(date -u -d "$(echo "$E_T" | tr T ' ') 60 seconds" +%Y-%m-%dT%H:%M:%S)
    STALLED_READS=$(sdb_chunk0_net_reads "$S_T" "$E_T_LATE" | grep -c "@127\.0\.0\.1:${S_PORT} ")
fi
echo
echo "[$MODE/$READ_PATTERN] $RESULT"
echo "[$MODE] sdb chunk-0 reads that reached a server after the fold: $SDB_READS"
[ -n "$STALLED_READS" ] && echo "[$MODE] ... of which sent to the stalled node: $STALLED_READS"
echo "[$MODE] folds of sdb chunk 0: $FOLDS (first at ${FOLD_TS:-never}); read RPCs sent to servers after it: $SERVER_READS"
echo "Logs: $LOG"
if [ -z "$RESULT" ] || [ "$FOLDS" -lt 1 ] || [ "$SDB_READS" -lt 5 ]; then
    echo "[$MODE] INCONCLUSIVE — no fold during the read window, or sdb chunk 0 was never read from a server after it"
    exit 3
fi
if [ "$MODE" = "fold+folder-stall" ] && [ "${STALLED_READS:-0}" -lt 1 ]; then
    echo "[$MODE] INCONCLUSIVE — no sdb chunk-0 read was sent to the stalled node, so the stall tested nothing"
    exit 3
fi
if echo "$RESULT" | grep -q "errors=0 mismatches=0 .*over_guest_timeout=0 " && ! grep -q "READ STUCK" "$LOG/reader.log"; then
    echo "[$MODE] PASS — no error, no wrong bytes, no read past the ${GUEST_TIMEOUT_SECS}s guest timeout"
    exit 0
fi
echo "[$MODE] FAIL — reproduced a guest-visible read failure"
exit 1
