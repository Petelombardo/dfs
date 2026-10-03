# Slot ownership — to-do and status (living document)

Pick-up point for SLOT-OWNERSHIP-PLAN.md. Update this file whenever a box is ticked.
Sizes are rough: **S** = hours, **M** = about a day, **L** = several days. Each phase still ends
with the gate in SLOT-OWNERSHIP-PLAN §8a (build, full timed suite, unit failure set vs baseline).

_Last updated: 2026-10-03, staging fio bench done and torn down_

**Resume here:** 3c step 2 — next: order folds through the primary, then arbitration by version
at the leader. Ordering kept (2026-10-03). Replica-side version identity done (T64 5/5).

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
- [x] ~~T64b lost acked write~~ — **misdiagnosed** (corrected 2026-10-03): in every failing run a
      writer's fsync had returned **EIO** and the writer died there, so the "lost" writes were
      never acked. T64 hid writer errors. It now records each writer's last acked write, judges
      T64b against that, and T64d fails on any EIO. Real bug: two writers on one chunk get EIO,
      flag on AND off (2/3 runs each) — see step 2's replica-side item.
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
- [x] **Decided 2026-10-03: KEEP** (Pete)

### Step 2 — full 3c, if the numbers say keep it (L)
- [x] ~~Folds go through the primary's order~~ — **not needed: no failing case** (2026-10-03).
      Background folds already run as one coordinated fold per chunk (T61: ~74 folds/run, 0
      pulls), the client's ForceFold folds both replicas, and a fold on one replica alone is
      adopted by the other (T65, new: deterministic one-sided fold via `dfs-admin isr fold`, 0
      disagreements). T61d's old race after T60 chaos: flag off 1 run in 3, **flag on 0 in 6**
      → T61d is now required with the flag on. Revisit only if a test shows ids diverging.
- [x] **Replica side of version-as-identity** (2026-10-03): each replica applies version n onto its
      own result for n-1 (`SlotOrder::head`); the primary sends its base with `WriteOrder`. Ordered
      writes skip every check that substitutes the leader's/local chunk_map view (chunk_seq gap
      refresh, staleness rebase, leader-confirmation, ghost retry, chunk_map-reject ChunkStale);
      an ordered result always advances the replica's chunk_map (seq = max); the fold-abandon
      check never discards the slot's ordered head; an unordered patch clears the head.
      Root cause it fixes (T64d EIO): `update_chunk_map_after_patch` rejected the ordered result
      by comparing two clients' unrelated `client_write_seq`s (25 vs 27), the fold then saw the
      token as superseded and abandoned it, and every later write/read of the slot failed.
      T64 flag on: 3/5 fail → **5/5 pass** (0 disagreements, 0 EIO, last acked write held).
- Gate 2026-10-03: unit set = baseline; suite flag ON **147/0** (14m03s), flag OFF **146/1** (13m60s: T53c
      once under load — passes 3/3 alone, path unchanged with the flag off; watch it).
- [ ] Leader side: responses carry the version; the client compares versions, not chunk ids;
      the leader's location arbitration uses the version instead of per-client
      `client_write_seq` (not needed for T64 now; needed so a leader-side seq compare can't
      prefer an older occupant)
- [x] **Stall matrix T66** (2026-10-03, Pete: "a locked node during a dual-replica write" is where
      most past problems came from). Two writers on one block; one node SIGSTOPped 8s: chunk's
      ISR primary, secondary, or the leader when outside the ISR. Checks: no acked write lost,
      ISR replicas byte-identical (fold ids are content hashes), writes resume after SIGCONT.
      Flag off is far worse (12-54 writes in 16s vs ~300; a replica failed to fold after a
      primary freeze). Two ordered-path bugs found and fixed:
      - A resumed primary refuses until its lease is back (~0.9s) but left the secondary
        waiting out its turn timeout: every write in that window cost a 6s RPC timeout.
        Now the primary sends `WriteOrder { version: REFUSED }` and the secondary fails at once.
      - The client's transport resends a request whose reply timed out. The primary refused the
        stale copy (lease lapsed) and ORDERED the resend; the secondary had failed the write,
        got a raw backfill outside the stream, and the pair diverged (byte-different replicas).
        Now each replica decides each write once (`Decision`, bounded per slot): a resend gets
        the original answer. Frozen-primary case: 4/4 pass after the fix.
      Still open from T66: a frozen leader blocks writes for the whole freeze even when outside
      the ISR (fsync commits locations + metadata to the leader synchronously) → Phase 5's
      measurable target. The feared stalled-secondary skip-the-gap reorder hasn't shown in any
      run yet; durable versions + catch-up below remove the gap-skip regardless.
- [ ] Durable versions per slot; a lagging secondary NACKs and the primary sends what it missed
      (replaces the prototype's "skip the gap")
- [ ] Gates: T64 passes; the 2026-09-27 no-op-divergence repro ends with one version on both;
      lagging-secondary catch-up test; T60-style chaos with writes running and no acked write
      lost; T61d becomes required; **primary killed mid-storm**: no acked write lost, and say
      what the client does until 3d promotes the secondary (fails/retries, or falls back to
      unordered?) — Pete's 2026-10-03 question on the dual-stream durability guarantee
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
  - Two writers on one chunk get EIO on main today (flag off: T64d fails 2/3, plus 4-109
    replica disagreements). Fixed with the flag on; the flag-off path is the reason to make
    ordering the default.
- **Deploy note for the delete fix:** clusters upgraded after files were already deleted fold
  those leftover patch rows once.
