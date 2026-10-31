#!/bin/bash
# G8 soak workloads (SLOT-OWNERSHIP-TODO G8), run ON the bench client (server4) against the
# :8950 bench mount. Started and stopped by scripts/bench_soak.sh; each worker loops until
# $SOAK/stop exists and appends one line per iteration to $SOAK/<worker>.log:
#   "<time> OK ..." or "<time> FAIL ...". Any FAIL is a G8 failure (guest EIO or a lost write).
#
#   bench_soak_workers.sh vm      fio random 4-64 KiB writes, fsynced, with fio's own checksum
#                                 verification (verify_backlog) over a 2 GiB disk image
#   bench_soak_workers.sh qcow2   qemu-io writes known patterns into a qcow2 image, a NEW qemu-io
#                                 process reads them back (a VM restart), then qemu-img check
#   bench_soak_workers.sh dvr     DVR-style recordings (512 MiB at ~8 MB/s, fsynced every ~5 s,
#                                 md5 recorded), plus a burst of shell appends to a log (T106)
#   bench_soak_workers.sh verify  mount a FRESH client, md5 every retained recording against its
#                                 recorded md5 and check the append log, unmount
MNT=/mnt/dfs-bench
CDIR=/root/dfs-bench
SOAK=$CDIR/soak
mkdir -p "$SOAK"
log() { echo "$(date '+%F %T') $*" >> "$SOAK/$1.log"; }
# A worker also stops when the bench mount is gone: after the client died (G8 10-31, a stack
# overflow) the workers kept writing into the bare directory on server4's root disk (13 GB).
stopping() {
    [ -f "$SOAK/stop" ] && return 0
    mountpoint -q "$MNT" && return 1
    log "$1" "FAIL $MNT is not mounted (the client is gone); stopping"
    return 0
}

case "$1" in
vm)
    mkdir -p "$MNT/vm"
    i=0
    while ! stopping vm; do
        i=$((i+1))
        out=$(cd "$MNT/vm" && fio --name=vm --filename=disk.raw --size=2g --rw=randwrite \
            --bsrange=4k-64k --ioengine=psync --fsync=8 --verify=crc32c --verify_backlog=64 \
            --verify_fatal=1 --runtime=600 --time_based --randrepeat=0 --output-format=terse 2>&1)
        rc=$?
        errs=$(echo "$out" | grep -ciE 'verify|input/output error|err=')
        if [ $rc = 0 ] && [ "$errs" = 0 ]; then log vm "OK iter $i"
        else log vm "FAIL iter $i rc=$rc: $(echo "$out" | grep -iE 'verify|error|err=' | head -3 | tr '\n' ' ')"; fi
    done
    ;;
qcow2)
    mkdir -p "$MNT/vm"
    Q=$MNT/vm/q.qcow2
    [ -f "$Q" ] || qemu-img create -q -f qcow2 "$Q" 4G
    i=0
    while ! stopping qcow2; do
        i=$((i+1)); p=$(( i % 250 + 1 ))
        # 200 random 64 KiB clusters, written with pattern p and flushed, then read back by a
        # separate qemu-io process (the image is closed and reopened in between).
        offs=$(python3 -c "import random; print(' '.join(str(random.randrange(65536)*65536) for _ in range(200)))")
        wargs=(); rargs=()
        for o in $offs; do wargs+=(-c "write -P $p $o 64k"); rargs+=(-c "read -P $p $o 64k"); done
        w=$(qemu-io -f qcow2 "${wargs[@]}" -c flush "$Q" 2>&1); wrc=$?
        r=$(qemu-io -f qcow2 -r "${rargs[@]}" "$Q" 2>&1); rrc=$?
        c=$(qemu-img check -q -f qcow2 "$Q" 2>&1); crc=$?
        bad=$(echo "$w$r" | grep -ciE 'error|fail|mismatch|pattern')
        if [ $wrc = 0 ] && [ $rrc = 0 ] && [ $crc = 0 ] && [ "$bad" = 0 ]; then log qcow2 "OK iter $i pattern $p"
        else log qcow2 "FAIL iter $i pattern $p wrc=$wrc rrc=$rrc check=$crc: $(echo "$w$r$c" | grep -iE 'error|fail|mismatch|pattern|corrupt|leak' | head -3 | tr '\n' ' ')"; fi
        sleep 20
    done
    ;;
dvr)
    mkdir -p "$MNT/dvr"
    i=$(ls "$SOAK"/rec_*.md5 2>/dev/null | wc -l)
    while ! stopping dvr; do
        i=$((i+1)); f=$MNT/dvr/rec_$i.ts
        # ~8 MB/s in 188-byte-aligned blocks, fsync about every 5 s, md5 of exactly what was written.
        res=$(python3 - "$f" <<'PY'
import hashlib, os, sys, time
path = sys.argv[1]; h = hashlib.md5(); blk = 188 * 1024; total = 512 * 1024 * 1024
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644); n = 0; t0 = last = time.time()
try:
    while n < total:
        b = os.urandom(min(blk, total - n)); os.write(fd, b); h.update(b); n += len(b)
        if time.time() - last >= 5: os.fsync(fd); last = time.time()
        ahead = n / 8e6 - (time.time() - t0)
        if ahead > 0: time.sleep(ahead)
    os.fsync(fd); os.close(fd); print("OK", h.hexdigest())
except OSError as e:
    print("FAIL", e)
PY
)
        set -- $res
        if [ "$1" = OK ]; then echo "$2" > "$SOAK/rec_$i.md5"; log dvr "OK rec_$i written md5 $2"
        else log dvr "FAIL rec_$i: $res"; fi
        # Keep the newest 8 recordings (4 GiB): delete older ones and their md5s.
        for old in $(ls "$SOAK"/rec_*.md5 2>/dev/null | sed 's/.*rec_\([0-9]*\).md5/\1/' | sort -n | head -n -8); do
            rm -f "$MNT/dvr/rec_$old.ts" "$SOAK/rec_$old.md5"
        done
        # The DVR startup-log pattern (Legata 1e111d42): 40 shell appends, then read back.
        : > "$MNT/dvr/app.log"; : > "$SOAK/app.exp"
        for k in $(seq 1 40); do L="$(date +%T) rec $i startup line $k: padding to look like a log line"
            echo "$L" >> "$MNT/dvr/app.log"; echo "$L" >> "$SOAK/app.exp"; done
        if cmp -s "$SOAK/app.exp" "$MNT/dvr/app.log"; then log dvr "OK appends after rec_$i"
        else log dvr "FAIL appends after rec_$i: $(diff "$SOAK/app.exp" "$MNT/dvr/app.log" | grep -c '^[<>]') line(s) differ"; fi
    done
    ;;
verify)
    V=/mnt/dfs-bench-verify; mkdir -p "$V"
    cluster=$(sed -n 's/.*--cluster \([^ ]*\).*/\1/p' "$SOAK/cluster" 2>/dev/null)
    RUST_LOG=info nohup setsid "$CDIR/dfs-client" mount "$V" --cluster "$cluster" \
        --log-file "$CDIR/client-verify.log" --allow-other > /dev/null 2>&1 < /dev/null &
    pid=$!
    for _ in $(seq 1 60); do mountpoint -q "$V" && break; sleep 1; done
    if ! mountpoint -q "$V"; then log verify "FAIL fresh client did not mount"; kill $pid 2>/dev/null; exit 0; fi
    sleep 3
    ok=0; bad=0; badlist=""
    # The newest recording may still be being written: verify only those with a recorded md5.
    for m in $(ls "$SOAK"/rec_*.md5 2>/dev/null); do
        n=$(basename "$m" .md5)
        want=$(cat "$m" 2>/dev/null)
        got=$(md5sum < "$V/dvr/$n.ts" 2>&1 | cut -c1-32)
        [ -f "$m" ] || continue   # the dvr worker retired it mid-read
        if [ "$got" = "$want" ]; then ok=$((ok+1)); continue; fi
        if [ "$got" = "$(cat "$m")" ]; then ok=$((ok+1)); else bad=$((bad+1)); badlist="$badlist $n($got)"; fi
    done
    fusermount -u "$V" 2>/dev/null || umount -l "$V"; kill $pid 2>/dev/null
    if [ $bad = 0 ]; then log verify "OK $ok recording(s) match through a fresh client"
    else log verify "FAIL $bad of $((ok+bad)) recording(s) differ through a fresh client:$badlist"; fi
    ;;
*) echo "usage: $0 vm|qcow2|dvr|verify"; exit 1 ;;
esac
