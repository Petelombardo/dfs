#!/bin/bash
# Runs ON server3 (or server5) directly against the live staging DFS mount.
# Same shape as scripts/repro_fio_fsck_restart.sh (the local version) but
# against real staging hardware/network instead of localhost: format a raw
# disk image living on the DFS mount as ext4, loop-mount it, hammer it with
# fio (kdiskmark-style concurrent random I/O), cleanly unmount, wait, restart
# dfs-client, then fsck (read-only) to check for filesystem-level corruption.
#
# Deploy + run:
#   scp scripts/staging_fio_fsck_repro.sh root@server3:/root/staging_fio_fsck_repro.sh
#   ssh root@server3 "chmod +x /root/staging_fio_fsck_repro.sh"
#   ssh root@server3 "/root/staging_fio_fsck_repro.sh 2>&1 | tee /root/staging_fio_fsck_repro_\$(date +%s).out"
# (or background it and poll — see below)
#
# To run detached and poll for completion:
#   ssh root@server3 "nohup /root/staging_fio_fsck_repro.sh > /root/repro.out 2>&1 &"
#   ssh root@server3 "while ! grep -qE 'FSCK CLEAN|FSCK FOUND ERRORS|REMOUNT FAILED' /root/repro.out 2>/dev/null; do sleep 5; done"
#   ssh root@server3 "cat /root/repro.out"
#
# ── STATUS as of 2026-07-11 (see also memory: project_leader_sync_and_write_quiet_fixes.md) ──
#
# Six real chunk-loss bugs were found and fixed this session (commits
# dadc4a9..1bd8bbb on chunk-patch-overlay-consolidation, all locally verified
# via ./scripts/test_local_suite.sh and deployed to all 5 gluster nodes +
# this client). All shared the same disease: a node's local/cached view of
# chunk state trusted as cluster-wide truth, or a metadata push silently
# dropped instead of retried, letting a chunk_id become "current" somewhere
# with zero nodes that actually have its bytes. T51 in test_local_suite.sh
# is the local regression test for the specific leader-outage variant.
#
# OPEN ISSUE: after all 6 fixes were deployed, this repro started failing
# consistently with a NEW, different signature: `fio: ENOSPC ... No space
# left on device` during file layout (errno 28), runs ending after only
# ~10-20s instead of the full 180s. This is NOT the old "no registered
# location"/EIO signature from earlier in the session.
#
# Ruled out already (don't re-check these without new evidence):
#   - Physical disk space: all 5 gluster nodes 18-23% used, 670-716GB free.
#   - Physical inodes: 1% used on all 5 nodes (`df -i /mnt/gluster`).
#   - Per-node DFS capacity tracking: `dfs-admin cluster status` reports
#     correctly, no node near its configured limit.
#   - Client process staleness: the client already restarts fresh every
#     single repro cycle (this script's own "Restarting dfs-client" step),
#     so a long-lived-process leak isn't the differentiator between runs.
#   - `fallocate`/`WRITE_ZEROES` support: `dmesg -T | grep WRITE_ZEROES`
#     shows "operation not supported" on loop1 going back to 17:48 EDT that
#     day, including during periods with clean runs — longstanding and
#     benign, not new. dfs-client has no fallocate handler at all, but this
#     predates tonight's failures.
#   - Fix #6 (phantom-reconciliation/orphan-sweep write-quiet defer,
#     commit 4bd4d1e): did a real A/B test — reverted it, rebuilt, local
#     suite clean, redeployed (confirmed byte-identical binary to pre-fix-6),
#     re-ran this repro — STILL FAILED with the identical ENOSPC signature.
#     Conclusively not the cause. Re-reverted back to keep it (commit
#     1bd8bbb) since its own rationale is independently real and verified.
#   - grep confirms dfs-client's own source never returns ENOSPC anywhere
#     (only EIO/EAGAIN) — so this isn't DFS explicitly rejecting the write.
#
# Leading unconfirmed theory: the kernel's loop-device/page-cache layer may
# be synthesizing ENOSPC as a downstream symptom when the pre-existing
# WRITE_ZEROES-unsupported fallback path collides with some remaining
# intermittent chunk-write failure (of the same general class already fixed
# elsewhere this session), rather than propagating the original error
# faithfully. NOT verified. Next step: before concluding anything new, grep
# /var/log/dfs-client.log for the FIRST warning/error immediately preceding
# an ENOSPC-signature failure (not just search for "ENOSPC" itself, which
# won't be there) — find what DFS-level condition, if any, actually
# preceded it in time.
#
# Also worth checking fresh: has staging's total accumulated chunk count
# (`dfs-admin storage stats` if available, or count files under
# /mnt/gluster/dfs/data/chunks on each node) grown further since last
# night — staging has been used continuously for many hours of heavy
# testing with no cleanup pass, and a phantom-reconciliation pass logging
# "verifying presence for 80337 chunks" was the trigger for fix #6.
set -u

MOUNT=/mnt/dfs
LOOPMNT=/mnt/staging-fsck-test
IMG="$MOUNT/staging_fsck_test.img"
IMG_SIZE_MB=8192
LOG=/root/staging-fsck-repro

cleanup() {
    umount "$LOOPMNT" 2>/dev/null || true
    for ld in $(losetup -j "$IMG" 2>/dev/null | cut -d: -f1); do
        losetup -d "$ld" 2>/dev/null || true
    done
}

echo "=== Cleanup any previous run ==="
cleanup
rm -rf "$LOG"
mkdir -p "$LOG" "$LOOPMNT"

echo "=== Creating and formatting ${IMG_SIZE_MB}MB raw disk on $MOUNT ==="
# rm (not just truncate) so each run gets a genuinely fresh DFS file_id —
# chunk hashing is file-scoped (see feedback_chunk_hashing_file_scoped), so
# reusing the same path via truncate alone can leave old chunk state behind
# for a later run to collide with.
rm -f "$IMG"
truncate -s ${IMG_SIZE_MB}M "$IMG"
mkfs.ext4 -F -q "$IMG"
sync "$MOUNT"

echo "=== Loop-mounting and running fio (kdiskmark-style) ==="
LOOPDEV=$(losetup -f --show "$IMG")
mount "$LOOPDEV" "$LOOPMNT"
fio --name=stress --directory="$LOOPMNT" --size=6000M --rw=randrw --bs=4k \
    --iodepth=32 --numjobs=4 --runtime=180 --time_based --direct=0 --group_reporting \
    --fsync=32 > "$LOG/fio.log" 2>&1
echo "fio done — see $LOG/fio.log"
tail -15 "$LOG/fio.log"

echo "=== Cleanly unmounting loop device ==="
sync "$LOOPMNT"
umount "$LOOPMNT"
losetup -d "$LOOPDEV"
sync "$MOUNT"

echo "=== Waiting 30s ==="
sleep 30

echo "=== Restarting dfs-client ==="
systemctl restart dfs-client
sleep 3
mountpoint -q "$MOUNT" || { echo "REMOUNT FAILED"; exit 1; }
echo "Remounted."
sync "$MOUNT"
sleep 2

echo "=== Re-loop-mounting image for read-only fsck ==="
LOOPDEV=$(losetup -f --show "$IMG")
e2fsck -fn "$LOOPDEV" > "$LOG/fsck.log" 2>&1
FSCK_EXIT=$?
cat "$LOG/fsck.log"
losetup -d "$LOOPDEV"

echo ""
echo "════════════════════════════════════════════"
if [ "$FSCK_EXIT" -eq 0 ]; then
    echo "  FSCK CLEAN (exit 0) — no repro this run"
else
    echo "  FSCK FOUND ERRORS (exit $FSCK_EXIT) — see $LOG/fsck.log"
fi
echo "════════════════════════════════════════════"

exit "$FSCK_EXIT"
