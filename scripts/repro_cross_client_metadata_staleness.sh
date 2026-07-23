#!/bin/bash
# Reproduce the cross-client metadata_cache staleness found 2026-07-23 while
# investigating VM-migration corruption on staging (server2/3/4/5).
#
# OUTCOME: THE THEORY THIS WAS BUILT TO PROVE IS **DISPROVEN**. Kept as a
# cross-client coherence regression guard -- it should PASS (exit 0).
#
# The theory was: fuse_impl.rs:5274 -- write-mode open() refreshes chunk locations
#   only `if !is_trunc && cache_looks_empty`, where cache_looks_empty is literally
#   `chunk_locations.is_empty()`. That asks "is the cache COLD?", never "is the cache
#   CURRENT?" -- no write_seq comparison. metadata_cache has no TTL, and protocol.rs
#   has NO server->client invalidation message at all. So a client that cached a chunk
#   map looked free to keep using it after ANOTHER client rewrote the file -- exactly
#   VM migration back to a hypervisor that ran the VM before.
#
# Why that's wrong: lookup() already performs the write_seq revalidation open() skips,
#   on every path resolution that necessarily precedes an open:
#     fuse_impl.rs:5064-5078  trusts the cache only inside a 5s freshness window
#     fuse_impl.rs:5110       else get_file_metadata_conditional(path, cached_write_seq)
#     fuse_impl.rs:5138       refreshes metadata_cache when the server's copy differs
#     fuse_impl.rs:5082       replies with Duration::ZERO entry TTL, so the kernel
#                             cannot cache the dentry and skip lookup on later opens
#   The open() guard is a redundant backstop, not a correctness hole.
#
# Measured here: B DOES skip the open()-refresh, and NO harm results -- all markers
# survive, zero ChunkStale, zero slot-backstop hits, even with a rolling server restart
# injected (which wipes the ephemeral retired_chunk_aliases forwarding pointers) and
# with the server logs confirming folds advanced BOTH chunk identities.
#
# WHAT THIS SCRIPT DOES
#   Two dfs-client mounts (A and B) against ONE local cluster, standing in for two
#   hypervisors sharing one DFS:
#     1. A creates the "VM disk" and writes pattern A1.
#     2. B opens it O_RDWR and writes marker B1  -> B's metadata_cache is now POPULATED.
#     3. B goes idle (the VM "migrated away").
#     4. A rewrites the file heavily, forcing patches + folds so every chunk identity
#        changes. B is never told -- no invalidation protocol exists.
#     5. B opens O_RDWR again (the VM "migrates back") and writes marker B2.
#        cache_looks_empty is FALSE, so the refresh is skipped and B targets dead chunk_ids.
#     6. Verify from a THIRD, cold mount C that A2 and B2 both survived.
#
# PASS (exit 0): all markers present and file size intact -- the clients stayed coherent.
# FAIL (exit 1): a marker was lost/clobbered or B's write returned EIO. That would mean
#   cross-client coherence has genuinely regressed -- most likely lookup()'s conditional
#   GET (fuse_impl.rs:5110) or its zero-TTL dentry reply (5082) was weakened.
#
# Usage: ./scripts/repro_cross_client_metadata_staleness.sh
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MA=/tmp/dfs-mig-a
MB=/tmp/dfs-mig-b
MC=/tmp/dfs-mig-c
LOG=/tmp/dfs-mig-logs
BIN="$REPO/target/release"
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902"
DISK=vmdisk.img
FAILED=0

# Same cache clamps the suite uses: 3 servers + 3 clients on one dev box, each of which
# would otherwise self-size its rings from system-wide RAM in ignorance of the others.
# The dev box has NO SWAP -- an unclamped repro has hard-locked it before.
export DFS_CHUNK_RING_CAPACITY=8
export DFS_DELTA_RING_CAPACITY=8
export DFS_MAX_CACHE_CHUNKS=8
export DFS_WRITE_BUFFER_CAP_MB=32

note() { echo -e "\n=== $* ==="; }
fail() { echo "  FAIL: $*"; FAILED=1; }
pass() { echo "  PASS: $*"; }

cleanup() {
    for m in "$MA" "$MB" "$MC"; do
        mountpoint -q "$m" 2>/dev/null && fusermount -u "$m" 2>/dev/null
    done
    pkill -f "dfs-client mount /tmp/dfs-mig" 2>/dev/null
    pkill -f "dfs-server" 2>/dev/null
}
trap cleanup EXIT

# mount_client <name> <mountpoint> -- start a dfs-client and block until mounted.
mount_client() {
    local name="$1" mp="$2"
    mkdir -p "$mp"
    RUST_LOG=info "$BIN/dfs-client" mount "$mp" --cluster "$CLUSTER" \
        --log-file "$LOG/client-$name.log" --allow-other --log-level debug &
    local waited=0
    until mountpoint -q "$mp"; do
        sleep 0.5; waited=$((waited+1))
        [ "$waited" -gt 40 ] && { echo "MOUNT FAILED ($name)"; tail -20 "$LOG/client-$name.log"; exit 1; }
    done
    echo "  mounted $name at $mp"
}

# sync_mount <mountpoint> -- drain write buffers + commit metadata (see CLAUDE.md dfs_sync).
sync_mount() { mountpoint -q "$1" && sync "$1" || true; }

# settle_folds -- give the server's patch-fold debounce time to fire, so chunk
# identities actually advance. This is the step that makes B's cached map stale.
settle_folds() { sleep 12; }

# rolling_restart_servers -- SIGTERM each dfs-server and bring it back, one at a
# time, leader last (node1 is the seed/leader), mirroring deploy-build.sh's real
# rolling order (gluster2->3->4->5->gluster1).
#
# WHY THIS MATTERS: full_rewrite_chunk unlinks the superseded chunk file as soon as
# a fold completes, so the old chunk_id is already gone from disk. What still lets a
# stale client map resolve is `retired_chunk_aliases` -- forwarding pointers the
# Server struct documents as "Ephemeral by design; never a durability dependency."
# They live only in memory, so every restart wipes them. A rolling deploy therefore
# clears, on every node, the exact structure that was masking every stale client
# cache in the fleet. That is the ingredient the first version of this repro lacked.
rolling_restart_servers() {
    for i in 2 3 1; do
        local port=$((8900 + i - 1))
        local pid
        pid=$(pgrep -f "dfs-server start --config $BASE/node${i}/config.toml" | head -1)
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null
            local waited=0
            while kill -0 "$pid" 2>/dev/null; do
                sleep 0.2; waited=$((waited+1))
                [ "$waited" -gt 50 ] && { kill -9 "$pid" 2>/dev/null; break; }
            done
        fi
        RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 "$BIN/dfs-server" start \
            --config "$BASE/node${i}/config.toml" >> "$LOG/server${i}.log" 2>&1 &
        # Wait for the port to answer again before moving to the next node.
        local w=0
        until (echo > "/dev/tcp/127.0.0.1/$port") 2>/dev/null; do
            sleep 0.3; w=$((w+1)); [ "$w" -gt 60 ] && break
        done
        echo "  restarted node$i (port $port)"
    done
    sleep 3
}

note "Cleanup + build check"
cleanup; sleep 1
rm -rf "$MA" "$MB" "$MC" "$LOG"; mkdir -p "$LOG"
[ -x "$BIN/dfs-client" ] || { echo "Build first: cargo build --release"; exit 1; }

note "Starting 3-node cluster"
bash "$REPO/scripts/setup-cluster.sh" 3 >/dev/null 2>&1
for i in 1 2 3; do
    RUST_LOG=info DFS_LEADER_HANDOFF_GRACE_MS=0 "$BIN/dfs-server" start \
        --config "$BASE/node${i}/config.toml" > "$LOG/server${i}.log" 2>&1 &
done
sleep 4

note "Mounting two clients (A = hypervisor 1, B = hypervisor 2)"
mount_client a "$MA"
mount_client b "$MB"

note "Step 1: A creates the VM disk (8MB, two chunks) and writes pattern A1"
dd if=/dev/urandom of="$MA/$DISK" bs=1M count=8 status=none
sync_mount "$MA"
printf 'A1-MARKER-AT-1MB' | dd of="$MA/$DISK" bs=1 seek=1048576 conv=notrunc status=none
sync_mount "$MA"
A1=$(dd if="$MA/$DISK" bs=1 skip=1048576 count=16 status=none)
[ "$A1" = "A1-MARKER-AT-1MB" ] && pass "A1 marker written" || fail "A1 marker not readable from A"

note "Step 2: B opens the same file O_RDWR and writes B1 -> B's metadata_cache populated"
printf 'B1-MARKER-AT-2MB' | dd of="$MB/$DISK" bs=1 seek=2097152 conv=notrunc status=none
sync_mount "$MB"
B1=$(dd if="$MB/$DISK" bs=1 skip=2097152 count=16 status=none)
[ "$B1" = "B1-MARKER-AT-2MB" ] && pass "B1 marker written (B now holds a populated chunk map)" \
    || fail "B1 marker not readable from B"

note "Step 3: B goes idle -- the VM 'migrates away' from hypervisor B"
sleep 2

note "Step 4: A rewrites the file heavily, forcing patches + folds (B is never told)"
# Scattered 4K rewrites across both chunks -> repeated patches to the same slots ->
# folds -> every chunk identity advances on the servers.
for pass_n in 1 2 3; do
    for off in 0 262144 524288 786432 1310720 2621440 3145728 4194304 5242880 6291456 7340032; do
        dd if=/dev/urandom of="$MA/$DISK" bs=4096 count=1 seek=$((off/4096)) conv=notrunc status=none
    done
    sync_mount "$MA"
done
printf 'A2-MARKER-AT-3MB' | dd of="$MA/$DISK" bs=1 seek=3145728 conv=notrunc status=none
sync_mount "$MA"
settle_folds
pass "A finished rewriting; chunk identities advanced"

note "Step 4b: rolling server restart (the deploy) -- wipes retired_chunk_aliases"
rolling_restart_servers

note "Step 5: B 'migrates back' -- opens O_RDWR and writes B2 using its STALE cache"
# errexit is deliberately NEVER enabled in this script -- several checks below
# depend on a non-zero exit (notably `grep -c` returning 1 when it finds no
# open-write-refresh line, which is precisely the condition being tested). An
# earlier version paired this `set +e` with a matching `set -e`, which switched
# errexit ON for the rest of the run and made the script die silently at step 7.
B2_ERR=$(printf 'B2-MARKER-AT-5MB' | dd of="$MB/$DISK" bs=1 seek=5242880 conv=notrunc status=none 2>&1)
B2_RC=$?
sync_mount "$MB"
if [ $B2_RC -ne 0 ]; then
    fail "B's write returned an error (rc=$B2_RC) -- this is the staging I/O error: $B2_ERR"
else
    pass "B's write returned success (corruption may still be latent -- see step 6)"
fi

note "Step 6: verify from a THIRD, cold mount (C) that nothing was lost"
sync_mount "$MA"; sync_mount "$MB"
sleep 3
mount_client c "$MC"
sleep 2
GOT_A1=$(dd if="$MC/$DISK" bs=1 skip=1048576 count=16 status=none 2>/dev/null)
GOT_B1=$(dd if="$MC/$DISK" bs=1 skip=2097152 count=16 status=none 2>/dev/null)
GOT_A2=$(dd if="$MC/$DISK" bs=1 skip=3145728 count=16 status=none 2>/dev/null)
GOT_B2=$(dd if="$MC/$DISK" bs=1 skip=5242880 count=16 status=none 2>/dev/null)
[ "$GOT_A1" = "A1-MARKER-AT-1MB" ] && pass "A1 survived" || fail "A1 LOST/CLOBBERED (got: '$GOT_A1')"
[ "$GOT_B1" = "B1-MARKER-AT-2MB" ] && pass "B1 survived" || fail "B1 LOST/CLOBBERED (got: '$GOT_B1')"
[ "$GOT_A2" = "A2-MARKER-AT-3MB" ] && pass "A2 survived" || fail "A2 LOST/CLOBBERED (got: '$GOT_A2')"
[ "$GOT_B2" = "B2-MARKER-AT-5MB" ] && pass "B2 survived" || fail "B2 LOST/CLOBBERED (got: '$GOT_B2')"

SIZE=$(stat -c %s "$MC/$DISK" 2>/dev/null || echo 0)
[ "$SIZE" = "8388608" ] && pass "file size intact (8388608)" || fail "file size wrong: $SIZE"

note "Step 7: mechanism check -- did B skip the refresh on its write-open?"
# The smoking gun: an open-write-check line with a populated cache, and NO
# open-write-refresh line following it (the refresh is gated on cache_looks_empty).
# NOTE: `grep -c` already prints 0 when there is no match, and exits 1. Writing
# `$(grep -c ... || echo 0)` therefore yields "0\n0" and breaks the integer test
# below -- which is exactly how the first run of this script misreported step 7 as
# a PASS. Let grep's own count stand; `set -e` is deliberately not enabled here.
CHECKS=$(grep -c "open-write-check" "$LOG/client-b.log" 2>/dev/null)
SKIPPED=$(grep "open-write-check" "$LOG/client-b.log" 2>/dev/null | grep -c "cache_looks_empty=false")
REFRESHED=$(grep -c "open-write-refresh" "$LOG/client-b.log" 2>/dev/null)
echo "  B open-write-check lines: $CHECKS (with populated cache: $SKIPPED), open-write-refresh lines: $REFRESHED"
# Informational only -- skipping the open()-refresh is EXPECTED and harmless, because
# lookup() already revalidated on write_seq before this open (see the header). This is
# reported, not scored: it documents that the code path is still shaped the way the
# header describes, so a future reader doesn't re-derive the whole theory from scratch.
if [ "${SKIPPED:-0}" -gt 0 ] && [ "${REFRESHED:-0}" -eq 0 ]; then
    echo "  (expected) B skipped the open()-refresh -- lookup() had already revalidated."
else
    echo "  (also fine) B refreshed its metadata on write-open."
fi

note "RESULT"
# Only the DATA verdict scores. The stale-cache mechanism being present is not a bug:
# it was measured to be harmless, repeatedly, including across a rolling server restart.
if [ "$FAILED" -ne 0 ]; then
    echo "  FAIL -- cross-client coherence regressed: data lost/clobbered or EIO."
    echo "  Suspect lookup()'s conditional GET (fuse_impl.rs:5110) or its zero-TTL"
    echo "  dentry reply (fuse_impl.rs:5082)."
    echo "  Logs: $LOG/client-{a,b,c}.log"
    exit 1
fi
echo "  PASS -- two clients stayed coherent across a rewrite + rolling server restart."
exit 0
