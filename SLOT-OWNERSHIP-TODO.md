# Slot ownership — to-do and status (living document)

Pick-up point for SLOT-OWNERSHIP-PLAN.md. Update this file whenever a box is ticked.
Sizes are rough: **S** = hours, **M** = about a day, **L** = several days. Each phase still ends
with the gate in SLOT-OWNERSHIP-PLAN §8a (build, full timed suite, unit failure set vs baseline).

_Last updated: 2026-10-13, default-flip gates written down (see "Make ordering the default"); T81-T86 and chain replication_

**Resume here:** the default-flip gates under 3c ("Make ordering the default"). Ordering kept
(2026-10-03); ordering-on suites run 255-257 pass with 1-2 failures, but nearly every run still
turns up something real. The gates below say what "stable" means before the flag flips.

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
- [x] **Folds go through the primary's order** — REOPENED and DONE 2026-10-03 (ordered
      ForceFold, see the catch-up item below). The morning's "not needed" was wrong: it compared
      fold ids, not bytes. Under load the client's ForceFold reached the replicas at different
      points of the stream (T64a, 59 disagreements, ~1 run in 3 after T63).
      Morning note kept for history: ~~not needed: no failing case~~.
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
- **Finding (2026-10-03, T66 with byte-level replica compare): token ids are replica-local.**
      Two replicas applying the same write to the same bytes can mint different patch-token ids
      (the id hashes the replica's own accumulator: what it folded, merged, or got backfilled).
      So neither "secondary follows its own head" (diverges silently when it fell behind: after
      a stall the client writes around it) nor "secondary applies only onto the primary's base
      id" (tried; "behind" on nearly every write once ids drift, backfill churn, still diverged)
      is sound. Needed: continuity BY VERSION (secondary applied v-1 → apply v onto own head;
      never skip a gap) and catch-up BY THE PRIMARY when it can't continue (gap, or the primary
      wrote unordered around it) — the next item. Until then T66[secondary]b can fail (~1 in 2
      sequences); tooling to see it: `ReadSlotLocal` / `dfs-admin isr read` (each replica's own
      bytes for a slot, blake3).
- [x] **Catch-up by version + anchors (2026-10-03, uncommitted at time of writing)** — replaces the
      prototype's "skip the gap". Token id = hash(accumulated delta), WITHOUT the base, so token
      ids prove nothing either way; real (content-hash) ids are trustworthy. So:
      - `head_version`: the secondary applies v only onto a head at v-1; never compares token ids.
      - Anchors: the primary starts every stream (and restarts after one of its own applies
        failed) from a real chunk, materialized from its head, or from its own slot content when
        the client's id isn't held here (`materialize_anchor`, slot-backstop fallback). A real
        base is adopted by the secondary (pulled hash-verified if missing). Anchors get a local
        ChunkLocation (a fresh accumulator requires its base's location; without it every write
        onto an anchor failed — T34).
      - Resync (`ResyncSlot`/`SlotResync`, appended): a secondary that can't continue gets the
        primary's head as a real chunk at the primary's current version; writes up to it are
        refused here and the client backfills them.
      - Ordered ForceFold: a version of the stream; both materialize at it; equal ids = identical
        content, nothing moves (Pete's hash-and-converge), else the secondary pulls the primary's.
      - An unordered patch to a slot with a stream holds the slot's order lock for its whole
        apply and clears the head (it was retiring the token an ordered write built on).
      - `stream` boot nonce on WriteOrder: a restarted primary's versions restart at 1.
      Gates met: T66 freeze sequence 6/6 (was ~1 in 2 diverging), T63+T64 6/6 (was ~1 in 3),
      T34, T64/T65. NOT yet: persisting versions across restarts (a restarted secondary waits
      one 3s turn timeout, then resyncs); ~~the primary-apply-fails-but-secondary-succeeds case~~
      — FIXED dca079f (T68: the client retries an ordered write its primary didn't apply,
      instead of backfill-and-ack; before, 3/3 runs lost an acked write).
- [x] **Phase 3b part 1 (2026-10-04):** (a) on-demand ISR seeding (`GetOrSeedSlotIsr`, client-only;
      verify with `DFS_SLOT_ISR_SEED_SECS=0 DFS_ORDERED_WRITES=1 ./scripts/test_local_suite.sh T64`:
      604 ordered / 0 unordered, 3/3); (b) the client patches exactly the ISR pair when both members
      hold the chunk and aren't penalized. T67b went from failing most runs to 4/5; the remaining
      failures are the outage-time unordered fallback → Phase 3d. Known flakes in full suites:
      T38b, T45i (RF 3→4 replica count; ~1 in 13 runs across builds, also in earlier sessions).
- [x] **Phase 3d step 1 (2026-10-04): replace a down ISR member; takeover.** New ReplaceIsrMember
      (appended). The client asks the healthy member when an ordered write's member fails at the
      TRANSPORT level (a penalty flag never fired: a dead node "recovers" after each failure). A
      primary excludes a secondary it can't ping (1s); a secondary takes over only from a primary
      a majority voted expired. Replacement = a holder first, epoch+1 via Paxos; it catches up by
      the 3c anchor/resync. Durability floor: after a replacement the client RETRIES on the new
      pair, and an ordered write never acks one copy (URGENT_SINGLE_REPLICA was 1-7/run). Unordered
      folds (background owner, RF-restore push, unordered ForceFold) wait while the slot's ordered
      stream is active (10s): a one-sided mid-stream fold parted the accumulators. A member refuses
      an ordered write/fold tagged with an older epoch than it knows (stale primary after a
      takeover can never get two copies). T67b/T67c REQUIRED: 5/5 each, 0 unordered, 0 single.
      T66 frozen primary → real takeovers (epoch 2, new primary). Known suite flakes: T38b, T45i,
      T57a (the delete-resurrection divergence; fix on unmerged fix/delete-resurrection).
      Not yet: the flap guard (a replaced node is simply not re-added, so flapping costs nothing
      today); the plan's failure-matrix rows as their own tests.
- [~] **Phase 3d failure-matrix test T69 (2026-10-04, pushed 2026-10-05):** rows #2/#6,
      #3, #5+#11, #10 with invariants I1 no acked write lost, I2 one primary per epoch, I3 no
      unclean promotion, I4 current ISR identical. 19/19 together; row #5 takeover 4/4. Fixes it
      drove: replace/takeover decided before the "no replica succeeded" bail-out; transport
      failures tracked structurally ("read len" was missed); a primary that finds a reported-failed
      secondary REACHABLE makes it resync (ResyncFromPrimary, appended) instead of leaving it stale
      in the ISR; a secondary fails fast once the primary is voted expired (each attempt cost a 6s
      timeout). T66-T68 15/15. PENDING: full suites (disk filled to 100% in the flag-off run;
      flag-on run hit the T13b rename flake → T14 aborts the suite). RESUME: add disk space, rerun
      both full suites (`{ time DFS_ORDERED_WRITES=1 ./scripts/test_local_suite.sh; } > ...`,
      then =0, `rm -rf /tmp/dfs-test` between), push if clean, update Legata (3d task).
      UPDATE: flag-on suite on fc37440 = 183/1, **T61d 9 disagreements** (storm start after T60
      chaos, with ghost-chunk-guard trips = unordered patches). Investigate before pushing:
      run `DFS_ORDERED_WRITES=1 ./scripts/test_local_suite.sh T60 T61` repeatedly, count
      unordered MultiPatch sends and GetOrSeedSlotIsr outcomes.
      2026-10-05: did NOT reproduce: T60+T61 4/4 pass, full suites flag-on 181/0 (17m53s),
      flag-off 148/1 (T64d, = baseline; 17m41s). The client now logs every unordered write
      (`[ORDER] client: UNORDERED MultiPatch`, with the cached ISR); 0 in T61 of the clean run.
      The failing run's ghost-guard trips prove some storm writes went unordered (the guard
      only runs for unordered patches); cause still open. Next time T61d fails, grep T61.log.
- [x] **T73 primary killed mid-storm (2026-10-05):** new suite
      test (moved to run before T70-T72: T71 leaves the cluster leaderless). Run 1 on 4047393 found
      two bugs: (1) EIO ~3s after the kill: the secondary declines a takeover until a majority
      votes the primary expired, the client treated that as final, flush ladder -> fresh-write
      fallback -> EIO; (2) the background flusher (dual_rf=false) never picked the ISR pair, so
      after a takeover it wrote UNORDERED to the stale location (likely T61d's cause). Client fix
      (client.rs): dual_rf forced on with ordering + chunk_idx; a "not expired by a majority"
      decline is polled every 300ms within CONNECT_RETRY_BUDGET; an accepted replace resends in
      place (`continue 'retry`, was `return Err`). T73 3/3 pass (0 EIO, 0 unordered; 3-6s stall).
      **BLOCKER:** full suite flag on 196/3: T69[row3] I1 LOST acked writes A134/A135 (servers
      held A133; secondary replaced, epoch 2). Passed in both suites before this change, so
      suspect the change. Hypothesis: a background flush of A133 held in the new poll/resend
      loop while fsync flushes acked A134/A135, then landed last. NEXT: `DFS_ORDERED_WRITES=1
      T69_CASES=row3 ./scripts/test_local_suite.sh T69` x5 with the fix; keep logs on failure;
      check for two concurrent flushes of chunk 1 for ino A. Then decide: serialize flushes
      per chunk across the retry, or drop the in-place resend and only keep the takeover poll.
      Flag off 158/6: T59a (known flake), T64a/b/d (unordered path, known), T73 x2 (ordering;
      fixed by the move).
      10-05 resume: did NOT reproduce. T69 row3 alone 8/8, full T69 6/6 (19/0 each; logs confirm
      the secondary replace + in-place resend ran), flag-on full suite 203/0 in 21m58s
      (/root/dfs-suite-t73fix-on2.log; T73 0 EIO, worst stall 6.2s). The one loss is unexplained
      (logs were wiped) — the concurrent-flush overlap needs a flush >30s (FIFO/pipeline wait
      timeouts), not seen. Flag-off full suite 162/0 (21m36s, /root/dfs-suite-t73fix-off2.log).
      Committed + pushed. Watch T69[row3] for a recurrence: if it fails, keep the logs. Logs: /root/dfs-suite-t73fix-{on,off}.log, /root/dfs-t73-run{1..4}.log.
- [x] **T74 chaos with writes running (2026-10-05):** T60's chaos for 60s while two clients
      fsync-write their own block in chunks 1-4; required with ordering: no acked write lost
      (T74a), every chunk's ISR pair byte-identical (T74b), both writers progress after the heal
      (T74c). On f171417: 2 acked writes lost, 19 EIO, 2082 unordered sends. Found and fixed:
      1. Client fell back to UNORDERED when the pair's primary wasn't in the (stale) location,
         a member was penalized, or the one holder asked didn't know the ISR: one client's
         unordered patch rolled back the other's ordered stream. Now the agreed pair wins and
         every holder is asked.
      2. The leader arbitrated a chunk's locations by client_write_seq (per-client counters):
         the pair stored the last acked write, readers got an older one (stale pointer, also
         seen as zero reads). The primary now stamps a slot generation in stream order
         ((epoch<<40|version) floored at stored+1); the client sends ordered locations with it
         (own coalesced queue, pending_ordered_locations); the leader's metadata path
         (chunk_map_update) arbitrates by generation when both sides have one; a received
         location keeps its stated generation (it was promoted to the slot max on receipt,
         making late stale updates tie with the head).
      3. An ordered write was acked on the primary + a RAW BACKFILL onto a non-member when the
         client skipped a penalized secondary; a later takeover lost it. Now ISR members are
         never skipped on client penalty, and the write is resent on the pair until the
         secondary takes it or is replaced (CONNECT_RETRY_BUDGET), never backfilled off-pair.
      4. A node outside a chunk's ISR folded its leftover accumulator and announced old bytes;
         background folds now skip slots whose ISR excludes this node.
      5. On-demand ISR seeding answered "none" for a brand-new file (holder's chunk map and
         metadata lag the leader): the first writes went unordered. Seeding now falls back to
         the leader's location.
      After: T74 6/6 + 6/6 + 4/4, 0 unordered sends. Final suites: flag on 205/1 (T38b, the
      long-standing heal-timing flake; 23m34s), flag off 161/1 (T64d baseline; 22m47s).
      Background folds skip only on a node that has been in the slot's ordered stream: ISRs
      are seeded with ordering off too. Ordering off, T74 is bad before AND after this work
      (f171417 baseline: 29-111 EIOs, acked writes lost 2/3 runs) — the unordered path does
      not survive chaos, which is the case for flipping the default. Diagnostics kept: T74 prints stored bytes on
      loss (`dfs-admin isr read --range`), [GEN-PARTIAL] log when the leader arbitrates a
      slot by client_write_seq with only one side's generation known.
      FOLLOW-UPS: (a) 0-10 writer EIOs per T74 run (outage budget exhausted while both
      members unreachable? not yet analysed); (b) ISR epoch flapping under chaos (up to 107
      epochs in 60s) = the Phase 3d flap guard; (c) non-member leftover accumulators are now
      never folded: trim them (healer/GC); (d) T38b is a long-standing heal-timing flake.
- [ ] **T67 secondary restart:** T67a required (no acked write lost); T67b/T67c informational
      until 3b — after the restart the client drifts to a pair without the ISR primary and writes
      unordered (the pre-3c two-writer bug). Also found by T64: a brand-new chunk has no ISR for
      ~3s+, so its first writes are unordered → assign the ISR at chunk creation (3b).
- [x] **T75 the 2026-09-27 no-op-divergence repro (2026-10-06):** write, let the stream go
      quiet (10s), fold ONE ISR member, rewrite the same bytes, write, fold both, rewrite and
      write again. Ordering on: the no-op rewrites never split the pair (same head id on both,
      0 disagreements). The 09-27 split is structurally gone. But it found a real gap: a member
      that ADOPTS another replica's fold (ForceFold, the read path's Local fold) didn't hold the
      result. The broadcast flipped its head token to Folded(result), so it couldn't read its own
      head for ~30s, and the leader's under-replicated heal placed copies by ring placement, not
      on that member (3 of 5 ordering-on runs pre-fix). Fix: adopt_fold_result pulls the result from the
      folder (hash-verified), moves the ordered head onto it, and registers this node as a
      holder with the leader; the leader heal is only the fallback. DeleteChunkReplica now
      refuses to delete a node's ordered head (the excess trim is not ISR-aware). T75 3/3 on,
      2/2 off; pairs converge in 1s. FOLLOW-UPS: (a) the excess-replica trim's "lowest two
      NodeIds" pair ignores the slot ISR; (b) Local/ForceFold folds don't push their result
      to the slot peer the way Wave folds do (the adopter now pulls instead).
- [ ] Gates: T64 passes; ~~the 2026-09-27 no-op-divergence repro ends with one version on both~~ (T75);
      lagging-secondary catch-up test; T60-style chaos with writes running and no acked write
      lost; T61d becomes required; **primary killed mid-storm**: no acked write lost, and say
      what the client does until 3d promotes the secondary (fails/retries, or falls back to
      unordered?) — Pete's 2026-10-03 question on the dual-stream durability guarantee
- [ ] **Make ordering the default** (DFS_ORDERED_WRITES=1). Gates, agreed 2026-10-13 (Pete:
      "the ever elusive stable"). Also on the Legata roadmap shelf wall. Phases 3d/4/5 are NOT
      required for the flip.
      State 2026-10-13: ordering-on suites 1-2 failures per run (off: 4-5), but 6 of the last 9
      runs had a non-flake failure that turned out real or test-side: T57a views diverged,
      T74a acked loss, T74d writer EIO (a regression from the T74a fix), T76a acked loss on
      10-12 (not since; maybe closed by T74a), T85a cleanup didn't run on one node, T86c test
      killed the leader. All fixed or understood except T85a and T76a.
  - [ ] **G1 Clean streak:** 5 consecutive ordering-on full suites whose only failures are the
        mode-independent T38b heal-timing flake; each new failure root-caused before the
        streak restarts.
        10-14: 1014a/b clean, 1014c broke it (T85a; its T81b was an untested G6 build);
        1014d (G3+G6) 267/0 = 1/5.
        10-16/17: 1016i 288/0, then T85a / T59a broke it; 1017a T66[secondary]a (fixed 87286ad);
        1017b 288/1 (T38b only) = 1/5.
        1018a 287/1 (T38b) = 2/5; 1018c broke it with T83d (root-caused, fixed b01fb4e + the
        delivered record); 1018d 288/0 = 1/5.
        10-15: 1015a (T91 fix) ON 271/3 = T85a (G7) + T59a/T59e (one lease lapse in a 9 s
        db stall; 3/3 alone, logs lost to the OFF run) -> streak back to 0/5.
  - [x] **G2 Chaos loops:** T74, T69, T73, T76, T77 and T86 looped 20x each with ordering on, 0
        acked writes lost and 0 writer EIOs. Writer EIO counts as a failure (T74d).
        10-15 loops (ae6a40f): T69/T73/T76/T77/T86 20/20, T74 18/20. Both T74 fails = a
        secondary's ResyncSlot request (under the slot's order lock) lost to a blackhole
        waited the 30 s RPC timeout, past the client's 20 s budget -> fsync EIO. Fixed
        (5 s resync timeout); T91 fails first (worst op 39.9 s), 3/3 fixed. Re-loop T74 to tick.
        Re-loop (79502b5): T91 20/20, T74 19/20. The fail = the owner's idle background fold
        checked stream_active once, then folded without the order lock; a write landing
        mid-fold applied onto different accumulators on the pair -> client EIO. Fixed: the
        fold holds the slot's order lock throughout (T92 fails first 2/2, fixed 4/4; fio A/B
        no measurable cost). Suites 1015b ON 273/2 (T38b, T79f), OFF 206/3 (known).
        DONE 10-15 (68b21dc): T74 20/20, T92 20/20.
  - [x] **G3 Leader crash failover** (Legata f2708108): a SIGKILLed leader stalls creates
        2 x failure_timeout (~60 s on staging). Leadership must follow the majority-voted
        lease expiry (~3-4 s); suite test: kill -9 the leader, an fsynced create succeeds
        within the client's 15 s budget.
        DONE f489f20 (10-14): leader_among skips peers a majority voted lease-expired; T88
        0/3 on the old build (EIO at 15.0 s), 3/3 fixed (create in 6.3-6.7 s).
  - [ ] **G4 New-leader chunk maps** (Legata c742039c, T79c/f): a new leader must not serve
        GetFileChunkMap before holders' locations land (fresh readers get zeros meanwhile).
        10-15: T79f failed once in 1015a OFF (patched file read wrong md5; logs lost);
        3/3 alone. Watch for it before ticking.
        1015b ON: T79f failed again with ordering on (logs /root/dfs-suite-1015b-on-logs.tgz).
        Root cause: a restarted leader that missed a patch rebuilds its chunk map from its own
        CHUNK_TABLE (pre-patch version), and the holders' SyncHeldLocations push skipped
        unfolded patch tokens (no file under the token's id). FIXED: push_locations_to also sends
        the Pending tokens this node holds; T93 fails first 5/5, fixed 5/5. Suites 1015c ON
        276/1 (T75[secondary]a: admin ForceFold on the secondary returns before the primary
        adopts; a no-op rewrite in that ~100 ms gap splits ids, bytes identical), OFF 204/5:
        T79f failed AGAIN with ordering off, a different shape (node3 pulled locations fine;
        holders pushed pre-patch ids; node3 never refused a write). 3/3 alone. Open before ticking.
        10-16: T94 (patch, then the leader is killed for good: settled / immediate / bounce)
        found 4 bugs, all fixed fail-first: (1) resolve_by_slot served its slot occupant for
        any requested id -> silent stale read; it now serves only for a known-older id.
        (2) the client kept sending RCL to the dead leader; it rediscovers (500 ms rate
        limit); the first cut held the leader_addr read guard across an if-let -> deadlock
        (97-min fsync hang), fixed. (3) record_dead re-armed the 5 s shed on every refusal,
        so a restarted node stayed skipped past fsync's ladder -> EIO; one probe/s now.
        (4) fsync acked before a slot the background flusher had already claimed was written
        (T90c OFF data loss); fsync waits for claimed slots. Suites 1016a ON 281/2 (T59a,
        T59e), OFF 205/10 (all seen before: T83d T85a T38b T59a T64a/b/d T65a T77a/b).
        T79f (OFF, follower never persists placement rows) still has no deterministic repro.
        10-16: T75[secondary]a FIXED: a ForceFold on one ISR member now runs as the pair's
        ordered fold and returns once both members answered (unit test fails first). T75 6/6
        alone; suites 1016b ON 282/1 (T85a), OFF 211/4 (T83d T38b T77a/b), 46m each.
        10-16: T95 (the leader refuses every location registration for 2 s, then a fsynced
        patch; [up] leader stays, [dies] leader SIGKILLed right after the fsync). The client
        took a batched ReplicateChunkLocations answered with Response::Error as success and
        dropped the batch -> OFF read the wrong md5 after the leader died. FIXED: the batch is
        retried and re-queued. Old client fails T95[dies] OFF; fixed 6/6. Measured, then
        DROPPED: making fsync wait for the leader to register (bench QD1 -17%); the agreed
        direction is holders durably own the slot's version, the leader only knows who
        (README on the roadmap wall, steps 1-4). Suites 1016i ON 288/0 (43m11s), OFF 212/5
        (T79a T83d T64a/d T77b, all seen before; 56m41s). Unit: client 83/0, server 346/0.
        10-16 step 1 (holders own the slot's version): T95[orphan] (the leader AND the writing
        client die right after the fsync; holders are the only source) failed 5/6 on the old
        code. Fixed, each fail-first: (1) holders pushed locations before the file's metadata,
        so a leader that never heard of the file dropped them as a deleted file's and served it
        with 0 chunks, and the next writer's patch went as a FRESH chunk over the 4 MB one
        (Legata 1995942e); metadata now goes first, and ReplicateMetadataBatch answers only once
        committed. (2) a lone surviving ISR member's 1-node token lost to the 2-node pre-patch
        row on replica count: new Request::ReplicateIsrHeads (appended); the leader installs a
        member's head when its committed ISR names it at that epoch, by the usual causal order
        minus the replica count. (3) a holder that answered a pull while it still led itself
        skipped its later push as "already pulled". (4) heal transfers panicked on followers for
        ~2 s after every election (0 MB/s heartbeat target; Legata 0b184276), and a transfer
        bigger than the bucket waited forever at the 1 MB/s floor. T95[dies]/[orphan] are
        informational with ordering off (no ISR to prove quorum). Suites 1016l ON 287/2 (T85a,
        T59a), OFF 211/5 (T38b T64b/d T77a/b); T95+T45 3/3. Unit: server 351/0, client 83/0.
        10-17 step 1c DONE a25bff3: the generation commits in the same transaction as the
        patch state. Suites on it found two more bugs, fixed: a refused flush left its slot
        claimed and sync hung 11 h (83b519a, OFF T66[leader]); an ordered fold's result row had
        no file offset and RF-restore wrote back a stale row, so the leader lost the slot
        (87286ad, ON T66[secondary]a; isolated 2/2 FAIL -> 10/10). Suites 1017b: ON 288/1
        (T38b), OFF 210/6 (known). Step 1 done; next step 2 (reads resolve through the pair).
        10-18 steps 2+3 DONE (together: step 2 alone had no failing case, since every fsync still
        put the file's chunk locations to the leader). Step 2: GetSlotHeads; a fresh reader takes
        each slot's version from its leased primary, the leader's map is a hint. Step 3 (ordering
        on): an in-place patch's fsync doesn't wait on the leader when it already has the file's
        size and slots; reports still flow in the background (dropping them waits for step 4:
        healer/GC/sweep read the leader's map). T96 3/3 FAIL -> PASS; T66[leader]d worst fsync
        7.1 s -> 0.2-0.3 s. :8950 A/B (DFS_LEADER_OFF_PATH, 3 rounds): no measurable cost with a
        healthy leader (QD1 58.3 vs 56.7, 16w 49.5 vs 50.4, reads +/-1%). Suites 1018d ON 288/0,
        OFF 215/2 (T83d, T38b known). Found on the way: a delete that missed a moved leader
        (b01fb4e). Not measured yet: leader RPCs per write.
  - [x] **G5 No unordered window at chunk creation** (3b, the part the flip needs): a new
        chunk has no ISR for ~3 s, so its first writes go unordered. Assign the ISR at
        creation; T67b/T67c become required.
        DONE 10-15: the on-demand ISR seeding (slot_isr_for -> leader) already closed it.
        T90 (background-flushed new file, another client's fresh file, appended chunk,
        each patched 20-500 ms later) logs 0 UNORDERED sends and reads back exactly.
  - [x] **G6 No EIO for a retryable refusal** (Legata 7b87b67e): the client's fresh-write
        safety-check refusal is retried within the flush deadline, not returned from fsync.
        10-14: fix written (FlushAttemptRefused, 30 s budget; not when every replica refused
        the patch for its own storage, T81); T89 (DFS_FAULT_FAIL_PATCHES=2) EIO before, 3/3
        after. DONE (10-14) with suites 1014d: on 267/0, off 201/5 (known T38b T64b T64d T77a/b).
  - [ ] **G7 Open ordering-on failures closed:** T85a (cleanup ran 0 passes on one node, both
        modes, 10-13) and T76a (acked loss 10-12; Legata cf286166) root-caused or shown fixed.
  - [ ] **G8 Bench soak:** the :8950 bench cluster with ordering on, 48 h of a VM-108-like
        workload (qemu disk I/O + DVR-style sequential writes) plus periodic node kills: 0 guest
        EIO, 0 acked loss (md5 / fsck checks), latency and throughput no worse than off.
  - [ ] **G9 Staging soak:** Pete's go-ahead; ordering on for staging (servers, then clients;
        outside 19:00-22:00 ET), 1 week of normal use with the DVR and VMs, then flip the
        default in code. Rollback: DFS_ORDERED_WRITES=0 restart.

## Outside the phases

- **Failure-scenario test coverage (2026-10-05, Pete: "test every scenario"):** tracked on the
  Legata roadmap as "Failure-scenario test coverage" (external_ref testing/*), one subtask per
  gap. Quick wins: whole-cluster crash, client SIGKILL mid-flush, bit-flip corruption,
  ENOSPC on one node then all. High value: power loss (LazyFS / dm-flakey: unsynced writes
  really dropped), mixed-version rolling upgrade. Also: network latency/loss, slow disk,
  clock skew, hours-long soak with random faults. Each fails first, then joins the suite
  (both flag settings, every change).
  - [x] **Whole-cluster crash, T76 (2026-10-06):** failed first: acked writes lost, a file read
        as zeros. Fixed on fix/durable-acks (off fix/patch-after-delete, merged c9a3ac3):
        metadata commits durable before ack (bench 2026-10-07: no measurable cost; opt-out
        DFS_METADATA_DURABLE=0), chunk_map_update stale/partial-record wipes, fold/delete race.
        T76 required with ordering on; informational off (the unordered path loses writes).
  - [x] **Client SIGKILL, T77 (2026-10-06):** passes with ordering on; with ordering off a killed
        client's fsync'd writes are lost (Legata bug, open). Watch: the killed client sometimes
        finishes no fsync in its first 5s (2 of 10 runs).
  - [x] **Bit flip, T78 (2026-10-07):** failed first: a fresh client read flipped bytes with no
        error; the scrub couldn't see rot; the healer deleted the routing record of a chunk whose
        source lacked it. Fixed a0179af (verify on first disk read + quarantine, real scrub,
        heal drops only the missing holder). Cold 4K random reads -8..-14% (first touch).
  - [x] **ENOSPC one node, T79 (2026-10-08):** failed first: one full node turned fresh writes
        into EIO. Fixed 5f3d067 (fall back to another node on the replica's own storage error).
        Ordering on: a full ISR member was never replaced. Fixed 36c31f6 (ResignIsrMember: the
        failing member resigns to its pair peer, epoch+1); T79a/T79e required in both modes.
  - [x] **ENOSPC on the leader, T80 (2026-10-08):** failed first: 3 fsyncs acked, 3 files lost.
        Fixed a809751 + 99b672e (ack PutFileMetadata after commit, redb reopen after I/O error,
        fsync syncs a grown size, bounded 60s wait -> EIO). Bench: no measurable cost.
        Leader step-down done 36c31f6 (LeaderUnfit; catch-up before leading again; T80c).
        Found on the way, fixed 36c31f6 (all pre-existing): redb panics on commit/abort after
        ENOSPC killed the meta-committer; merge_self_heal dropped slots (zeros); a late
        write_seq-0 create reset a file to 0 B. T78b flake fixed ea6fe11 (healer drained from a
        stale alive_nodes_cache entry naming the quarantined holder).
  - [x] **Mixed versions (2026-10-07):** rolling upgrade from production's build on the bench
        cluster (bb11734 upgrade-node): 0 writer errors, every file intact; old nodes log
        "Failed to deserialize" for new request types until upgraded.
  - [x] **ENOSPC on every node, T81 (2026-10-08; DONE 2026-10-12, 496fa3a, on staging):** all 5 nodes on full loop
        filesystems. Found and fixed (local commit, suites NOT yet run):
        (1) a new file's write() hung on backpressure for 600s behind other files' undrainable
        buffer: client cluster_storage_full mark (every node refused for its own storage) ->
        write() gets ENOSPC; (2) redb 2.6.3 race: begin_write checks for I/O errors before
        waiting for the writer slot, so a commit queued behind a failed file grow PANICKED
        (assert !needs_recovery): all commits go through metadata::commit_txn, which turns it
        into an I/O error (fail-first unit test); (3) the client's fresh-write safety check
        treated a failed metadata lookup as "no data" and could zero-fill real bytes.
        OPEN: T81c ~1 in 3: with every node unfit, a LAGGING follower (node4, still at the
        file's create record) briefly became leader and served size 0 to a fresh client.
        Next: a node that hasn't caught up must not serve as leader (or the client must not
        trust a leader that isn't caught up). Logs: /root/t81fail (dev box).
  - [x] **2026-10-12/13, T82-T86:** T82 rolling restart of the leader (heartbeat after
        GracefulLeave, fixed); T83 fault kills (T83d off-only, not fixed per Pete); T84 client
        upload congestion -> EIO (6b59781); T85 superseded-version cleanup (2813c2f); T86 chain
        replication (a9e56a1, default on, DFS_CHAIN_WRITES=0 disables: rock5b 500 MB fsync dd
        52 -> ~90 MB/s). T38a acked loss (client adopted a leaderless one-node view) e06d02e.
  - [ ] Power loss (LazyFS / dm-flakey), and the rest of the Legata list.

- **Six fix branches MERGED 2026-10-05** (fix/dir-rename-subtree, fix/is-leader-startup,
  fix/delete-resurrection, fix/rename-lost-pending-write, fix/unit-test-fixtures,
  fix/async-metadata-reads): main 7931eef (suite 122/0, unit tests all green); into this branch
  05b4deb + T59e bbac84b + c87e287 (suite flag on 194/0 20m11s, flag off 161/1 = T64d baseline
  20m27s). Not deployed to staging.
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
