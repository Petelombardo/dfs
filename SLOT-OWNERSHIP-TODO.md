# Slot ownership — to-do and status (living document)

Pick-up point for SLOT-OWNERSHIP-PLAN.md. Update this file whenever a box is ticked.
Sizes are rough: **S** = hours, **M** = about a day, **L** = several days. Each phase still ends
with the gate in SLOT-OWNERSHIP-PLAN §8a (build, full timed suite, unit failure set vs baseline).

_Last updated: 2026-09-29, paused by Pete before the staging deploy_

**Resume here:** `scripts/bench_staging.sh deploy 0` → `run off1` → `run off2` → `restart 1` →
`run on1` → `run on2` → `teardown`. Both binaries are already built: the aarch64 server from
this branch in `target/release`, and the x86 client in `dist/bench-x86_64/`. Rebuild both if the
branch changes.

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
- [ ] Deploy the bench cluster; production untouched
- [ ] fio flag off vs on, two runs each: 4k randwrite QD1, 4k randwrite 16 writers, 1M sequential
- [ ] Tear the bench cluster down; write up the numbers; **decide with Pete**

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
  - Under full-suite load a metadata-db stall still sometimes costs a lease (T59a/T59e). The
    blocking path hasn't been found; gdb's pause seems to hide it. `eu-stack` (elfutils) would
    help — install is Pete's call.
  - T64's lost acked write with two writers exists on main today; 3c step 2 fixes it.
- **Deploy note for the delete fix:** clusters upgraded after files were already deleted fold
  those leftover patch rows once.
