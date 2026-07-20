#!/usr/bin/env python3
"""T53 helper: run a sustained random-write patch storm across N chunks, then
stop dead and hold the file open and idle.

The stop is the point of the whole thing. While the storm runs, the client's
own active-fold timer issues ForceFold to both replicas and everything stays
healthy. What is left behind at the end is a set of slots whose newest
generation was never folded by anyone — and those are picked up by
dfs-server's per-node debounce_fold_slot backstop.

That backstop re-sleeps a whole fresh PATCH_DEBOUNCE_IDLE (20s) whenever it
wakes to find the slot was touched inside the window, so a sub-second
difference in when a generation started on each replica becomes a ~20s
difference in when each one fires. Confirmed on staging 2026-07-20 (VM-111
install, file d159a6c7…, chunk_idx 1791): gluster1 fired at 10:52:20 and
folded alone; gluster4 never logged that slot again until healing three
minutes later. Whichever node fires first broadcasts ReplicatePatchFold, and
the peer's fold_slot_now then sees PatchState::Folded and returns without
folding — leaving exactly one node holding the new chunk identity.

A single synchronized burst does NOT reproduce this: both replicas land on the
same side of the 20s boundary, both fold to the same deterministic chunk id,
and the locations union to 2.
"""
import os
import random
import sys
import time

CHUNK_SIZE = 4 * 1024 * 1024
PATCH_SIZE = 4096


def main():
    path = sys.argv[1]
    nchunks = int(sys.argv[2])
    quiet = int(sys.argv[3])
    storm = int(sys.argv[4]) if len(sys.argv) > 4 else 30

    rng = random.Random(5353)
    fd = os.open(path, os.O_RDWR)
    try:
        deadline = time.time() + storm
        n = 0
        while time.time() < deadline:
            idx = rng.randrange(nchunks)
            # Vary the intra-chunk offset so successive patches to one slot
            # accumulate as separate delta records rather than overwriting.
            off = idx * CHUNK_SIZE + rng.randrange(0, CHUNK_SIZE - PATCH_SIZE, PATCH_SIZE)
            os.pwrite(fd, bytes([0x50 + (n % 32)]) * PATCH_SIZE, off)
            n += 1
            if n % 8 == 0:
                os.fsync(fd)
            time.sleep(0.02)
        os.fsync(fd)
        print(f"t53: {n} patches across {nchunks} chunks in {storm}s", flush=True)
        # Dead stop. Hold the file open, idle, while the server-side debounce
        # windows elapse on whatever generations were left unfolded.
        time.sleep(quiet)
    finally:
        os.close(fd)


if __name__ == "__main__":
    main()
