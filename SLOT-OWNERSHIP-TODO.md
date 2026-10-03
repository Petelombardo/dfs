# Slot ownership — to-do and status (living document)

Pick-up point for SLOT-OWNERSHIP-PLAN.md. Update this file whenever a box is ticked.
Sizes are rough: **S** = hours, **M** = about a day, **L** = several days. Each phase still ends
with the gate in SLOT-OWNERSHIP-PLAN §8a (build, full timed suite, unit failure set vs baseline).

_Last updated: 2026-10-03, staging fio bench done and torn down_

**Resume here:** Pete decides keep/drop from the 2026-10-03 numbers below (recommendation: keep,
since it costs nothing measurable). If kept: 3c step 2, starting with version as identity (T64b).

## Where things stand

| Phase | What | Status |
|---|---|---|
| 0 | Slot audit | done (97218b9) |
| 1 | Majority node leases | done (1598cf0) |
| 2 | One fold owner per chunk | done (11f9d35) |
| 3a | Per-chunk ISR, Paxos-agreed, + catch-up | done (e4ce1b6 and before) |
| **3c** | **Slot versions / ordered writes** | **in progress — step 1 (prototype for the fio decision)** |
| 3b | Client writes exactly the ISR pair | not started |
| 3d | Exclusions, replacement, flap guard | not started |
| 4 | Primary-driven replication and healing | not started |
| 5 | Delete dead code, take per-write traffic off the leader | not started |

**Remaining, roughly:** 3c step 1 ≈ S–M · 3c step 2 ≈ L · 3b ≈ M · 3d ≈ L · 4 ≈ L · 5 ≈ M.
In total several weeks of focused work, not days. Every phase needs its own gate, and the
integration and staging checks come on top.

## 3c — ordered writes

Decided 2026-09-29: the primary orders, and the data still fans out to both replicas (see
SLOT-OWNERSHIP-PLAN 3c). Behind `DFS_ORDERED_WRITES`; kept or dropped on fio numbers from staging.

### Step 1 — prototype for the measurement (S–M)
- [x] T64 gate: two clients writing one chunk. Fails today: 3–47 replica disagreements per run,
      and one run lost acked writes (d468d3e)
- [x] `scripts/bench_fio.sh` (local harness) (d468d3e)
- [x] Protocol: `Request::Ordered { tag, request }`, `Request::WriteOrder`, both appended
- [x] Server: `write_order.rs` (per-slot version state) + `handle_ordered` (primary: check still
      primary, assign version, send `WriteOrder`, apply; secondary: wait for its turn, apply)
- [x] Network: the split-frame decoder sees through `Ordered`
- [x] Client: flag, ISR lookup and cache (`GetSlotIsr`), wrap the request for the ISR pair,
      drop the cached ISR on "not the primary"
- [x] Unit tests (`write_order`, 3 tests) + full unit set = baseline (9 on this branch)
- [x] T64 locally with the flag on: **0 replica disagreements** (flag off: 3–47 per run). This took
      one more fix: an ordered write applies onto the replica's own current state for the slot,
      not the client's possibly stale chunk id. Otherwise one replica rejected a write as stale
      while the other rebased it, and the pair split with the order intact.
- [ ] T64b (lost acked write) still fails with the flag on: the servers end on e.g. B000002 after
      both writers were acked through write 149. The ordering is fine; the loss is in
      location/metadata arbitration by per-client `client_write_seq` → step 2
- [x] Full suite, flag on: **145/1** (only T64b, the step 2 gate; 0 replica disagreements).
      Flag off: 143/3 (known failures only)
- [x] Committed on slot-ownership (not pushed)
- [x] Full suite, flag off: 143/3. All three failures are known (T38b flake; T59a, which the
      async-reads branch fixes; T64b, the step 2 gate). No regression from the prototype.
- [x] `scripts/bench_staging.sh` (check/deploy/restart/run/teardown). Placement agreed with Pete:
      **gluster2–5 only** (gluster1 is the production leader with 470 MB free; a capped bench
      server uses 170–280 MB), port 8950, `/mnt/gluster/dfs-bench/…`, plain processes. **Client
      server4** (x86_64: binary from `dist/bench-x86_64/`, production `dist/x86_64/` untouched).
      fio installed on server4 with Pete's OK. Disk: 31–71 GB free per node.
- [x] Deploy the bench cluster; production untouched (2026-10-03, gluster2-5 :8950, client server4)
- [x] fio flag off vs on, two clean runs each (restart before every run), every write fsynced:

      | run  | 4k QD1 IOPS (fsync p50) | 4k 16 writers IOPS | 1M seq MB/s |
      |------|-------------------------|--------------------|-------------|
      | off4 | 34.0 (19.3 ms)          | 43.5               | 12.5        |
      | off5 | 45.6 (16.6 ms)          | 49.7               | 15.2        |
      | on2  | 45.1 (16.2 ms)          | 43.6               | 13.8        |
      | on3  | 42.3 (16.1 ms)          | 50.9               | 15.6        |

      **Ordering costs nothing measurable**: on is within the off-off spread on all three jobs.
      `[ORDER]` primary/secondary lines on all four nodes, 0 gaps, 0 WriteOrder failures.
      Unsynced jobs were dropped: through FUSE, O_DIRECT lands in the client write buffer, and
      the same build and flag measured 770 and 18000 4k IOPS on different restarts (off1 vs off3).
      Raw output: `/root/dfs-staging-bench-*.{raw,txt}` on the dev box.
- [x] Tear the bench cluster down (verified: no bench dirs or processes, production active)
- [ ] **Decide with Pete** (keep / drop)

### Step 2 — full 3c, if the numbers say keep it (L)
- [ ] Folds go through the primary's order (ForceFold and the background wave), so both
      replicas' chunk identities match, not just their bytes
- [ ] Version as identity: responses carry the version; the client compares versions, not chunk
      ids; location/metadata arbitration uses the version instead of per-client
      `client_write_seq` (the likely cause of T64's lost acked write)
- [ ] Durable versions per slot; a lagging secondary NACKs and the primary sends what it missed
      (replaces the prototype's "skip the gap")
- [ ] Gates: T64 passes; the 2026-09-27 no-op-divergence repro ends with one version on both;
      lagging-secondary catch-up test; T60-style chaos with writes running and no acked write
      lost; T61d becomes required
- [ ] Make the flag the default (or drop it)

## Outside the phases — waiting on Pete

- **Six fix branches off main, ready for review/merge** (nothing merged or deployed):
  `fix/dir-rename-subtree`, `fix/is-leader-startup`, `fix/delete-resurrection`,
  `fix/rename-lost-pending-write`, `fix/unit-test-fixtures`, `fix/async-metadata-reads`.
  Combined on main they pass 122/0 (`scratch-integrate-main`); merging them into this branch
  comes after that.
- **Open bugs:**
  - Fsync doesn't scale with writers: 16 fsyncing writers get ~45 IOPS total, the same as one
    (fsync p50 ~16 ms alone, ~165 ms with 16). Something on the fsync path serializes across
    files or writers. Same with the flag on or off, so it's not 3c. Worth its own investigation.
  - Under full-suite load a metadata-db stall still sometimes costs a lease (T59a/T59e). The
    blocking path hasn't been found; gdb's pause seems to hide it. `eu-stack` (elfutils) would
    help — install is Pete's call.
  - T64's lost acked write with two writers exists on main today; 3c step 2 fixes it.
- **Deploy note for the delete fix:** clusters upgraded after files were already deleted fold
  those leftover patch rows once.
