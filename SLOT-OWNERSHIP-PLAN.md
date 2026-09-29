# Slot Ownership Plan — one owner per chunk slot

> **Current status and the ordered to-do list live in [SLOT-OWNERSHIP-TODO.md](SLOT-OWNERSHIP-TODO.md).**

Status: PROPOSED (2026-09-28). Owner: Pete. Supersedes nothing yet; phases below retire
specific heuristics as they land.

## 1. Why

Most of our production incidents share one shape: **several nodes independently decide the
state of the same chunk slot `(file_id, chunk_idx)`, and nothing makes them agree.** Every
replica mints its own patch token, decides its own folds, garbage-collects on its own view,
and registers itself as a holder on its own say-so. We then reconcile after the fact with a
growing set of heuristics: ghost-chunk guard, RevalidateChunkSlot, location_supersedes
ranking, never-revert guard, RCL union-merge, fold_lock_grants / outbound_fold_claims, the
abandon path.

Recent examples, all the same root shape:
- 2026-09-27 VM-108 disk-1 chunk 9: one replica folded from RAM and registered a phantom
  holder. For the same write, g3/g4 kept token 9c40 and g1 minted 8195. g1 then abandoned
  a live patch. Result: a permanent EIO.
- 2026-09-11 fold identity ABA; 2026-09-10 location_supersedes ranking lost acked writes;
  2026-09-08 RCL-stale storm; 2026-09-05 ghost-guard livelock; 2026-08-30 fold-abandon
  destroyed a patch.

The fix is not a smarter heuristic. It is **one authority per slot at any moment, with fencing**,
so that disagreement becomes impossible for committed writes and immediately visible for
everything else.

## 2. Principles (constraints on the design)

1. **No leader on any write-critical path.** Leases, takeovers and ISR exclusions are all
   decided by majorities, never by the leader, so a dead or stalled leader costs no write
   availability. The leader keeps placement policy and auditing only. (gluster1 stalled 26 s on
   2026-09-24; its leader role must not be able to stall a guest.)
2. **No extra hop on the hot path by default.** Client write latency must stay within noise
   of today's.
3. **Clients never cause a promotion.** A failover happens only when a majority of nodes has
   voted the primary's lease expired. A client's view of the network is advisory.
4. **The client writes exactly 2 replicas; replicas 3..n are backfilled.** With RF = n > 2,
   every client write, full or patch, goes to exactly 2 nodes (primary + secondary). The
   remaining n−2 copies are filled afterwards by the primary (Phase 4; the healer until then),
   never by the client. RF = 1 writes 1, RF = 2 writes 2. Today's patch fan-out to every holder
   violates this and is removed in Phase 3.
5. **An acknowledged write exists on every in-sync replica.** This single rule is what makes
   any in-sync replica safe to promote.
6. **Deterministic, testable failure handling.** Partitions are tested by injected message
   filters, not wall-clock races (see feedback_timing_based_concurrency_tests_unreliable).
7. **Incremental.** Every phase is deployable on its own and earns its keep before the next.

## 3. Current state (verified 2026-09-28)

| Fact | Where |
|---|---|
| Leader = lowest online NodeId, only if it sees a strict majority. No term/epoch. | `dfs-server/src/cluster.rs:305` `is_leader` |
| Write durability floor is fixed at 2 when RF ≥ 2 | `dfs-common/src/types.rs:205` `write_quorum` |
| New chunks are written to 2 targets (dual_rf); the healer creates the 3rd | client `compute_required_replicas` |
| Patches fan out to **every** known holder (3 on staging), and each replica mints its own token | `MultiPatch … (3 replicas)` in client logs |
| Every replica folds independently; coordination is best-effort claims | `run_single_fold`, `coordinate_and_fold_slot` |
| Staging RF = 3 | `/mnt/gluster/dfs/config/*.toml` |

## 4. Roles and vocabulary

- **Leader**: existing role (lowest NodeId with majority view). Keeps placement policy and
  auditing; has **no role** in leases, takeovers or ISR changes.
- **Node lease**: "you may act as primary until T", held while a **majority** of nodes keeps
  acknowledging your renewals. **One lease per node** (5 on staging), not one per chunk.
- **Incarnation**: a per-node counter carried by its renewals. A majority that votes a node
  expired fences that incarnation; the node rejoins under a higher one.
- **Slot**: `(file_id, chunk_idx)`.
- **ISR (in-sync replica list)**: per slot, the *ordered* list of nodes guaranteed to hold
  every acknowledged write. Normally `[primary, secondary]`. Order matters, and it is a
  **recorded** order, never a sorted one (see "ISR order" below).
- **Primary of a slot**: the **first node in the slot's ISR that holds a valid node lease**.
  It is derived, not assigned per slot. Everyone computes the same answer from the same inputs.
- **Secondary**: the other ISR member.
- **Follower** (RF ≥ 3 only): holds a copy but is not in the ISR. It is filled asynchronously
  by the primary and is never promotable until caught up and added to the ISR.
- **Slot version**: a monotonic u64 per slot, advanced only by the primary. It replaces the
  content hash as the ordering key. The content hash stays, but only as an integrity check.
- **Slot epoch**: bumped when the slot's ISR or primary changes. Every slot-level message
  carries `(epoch, version)`, and stale epochs are rejected.

### Key design decision: node-level leases + derived per-slot primary

Per-slot or per-group leases would make the leader's state scale with data, and moving to
placement groups would force a data migration. Instead:

- Placement stays **per chunk**, as today. The client picks 2 nodes when a slot is created,
  and that ordered pair becomes the slot's initial ISR: `[P, S]`.
- Lease state is O(nodes) per node (each node's ack and fence records), and exclusions are
  O(node pairs) (see §6.3). Nothing scales with chunks or writes.
- When a majority votes node P expired, *every* slot where P was primary fails over to its
  secondary at once. Nothing is done per slot: each node derives the new primary locally.

Hot-path cost is zero extra RPCs; renewals are a few small messages per node per second.

### ISR order: recorded, versioned, never derived from the holder list

"First node in the ISR" only works if every node sees the same order. So:

- The ISR is a **new stored field per slot**, separate from today's `ChunkLocation.nodes`.
  That list is merged from many sources (union merges, per-node registrations, healer
  additions) and has no reliable order, so two nodes can list the same holders differently.
- **Not sorted by NodeId.** Sorting would make the lowest-id node primary for every slot it
  holds (gluster1, which is also the leader), and the highest-id node never primary except
  on failover, whatever was designated.
- It starts as `[P, S]` in the order the client chose at creation. The client's placement
  already spreads chunks evenly, so primaries spread the same way, and no node is
  structurally excluded from being primary.
- Only the primary changes it (replace a secondary, re-admit one, add a caught-up follower),
  and every change bumps the slot epoch. Every slot-level message carries that epoch, so a
  node acting on a stale order is rejected and refreshes instead of disagreeing.
- Anything that needs a node choice *without* an ISR (the Phase 0 audit, before ISRs exist)
  uses rendezvous hashing over the holder set: order-independent and balanced.

## 5. Topologies

| Setup | Behavior |
|---|---|
| **RF = 1** (aggregation, no redundancy) | ISR = `[holder]`. Every holder is primary for its slots. The lease still fences a node that was declared dead from resurfacing and writing. Node down means its slots are unavailable (no copy exists). |
| **RF = 2** | ISR = `[P, S]` as chosen by the client at creation. Writes are acked only when both have them. P fails → S promoted (after lease expiry). S fails → P recruits a replacement secondary. |
| **RF ≥ 3** | Principle 4: the client writes only P and S (quorum 2), for full writes and patches alike. P replicates asynchronously to followers. Only ISR members are promotable. A follower becomes secondary only after catch-up plus a leader-recorded ISR change. |

This matches the rule that the client writes 2 replicas. It also **narrows today's patch
fan-out from all holders to P+S** (a behavior change, Phase 3).

## 6. Mechanisms

### 6.1 Majority leases (built: Phase 1, `dfs-server/src/lease.rs`)
Guarantee: **once a majority has voted node P expired at incarnation i, P holds no lease at
any incarnation <= i.**
- **Renewal:** every `L/3`, P asks every peer in parallel. Each voter records the ack at its
  receive time. P holds its lease until `sent + L − margin` if a majority (P included) acked a
  renewal sent at `sent`. Counting from the send and ending a margin early errs toward P
  stopping sooner. Defaults: `L` = 3 s, margin = 500 ms (`DFS_LEASE_MS`, `DFS_LEASE_MARGIN_MS`).
- **Expiry vote:** a voter votes P expired only after `L + margin` without acking P. It then
  refuses P's renewals at or below that incarnation, persisting the fence before answering.
  Any majority that kept P's lease alive and any majority that voted P out share a voter, and
  that voter can't do both inside the forbidden window.
- **Stale-incarnation guard:** each vote answer carries the highest incarnation that voter has
  seen from P. If any is newer than the one being voted, the vote fails.
- **Restarts:** fences are durable; a restarted voter casts no expiry vote for `L + margin`
  (it may have acked P just before crashing).
- **Majority size:** counted over a persisted membership high-water mark, never over the
  membership a node happens to know (a just-started node knows only itself; on the first run
  of the leader-lease version, all five nodes each counted "1 of 1").
- **Isolation:** the lease runtime has its own locks and network client, and never touches the
  metadata DB, healer, chunk_map or membership lock, so a stall in those can't cost a lease.
- **Self-fence rule for Phase 3:** once lapsed, a primary refuses both writes **and** reads for
  its slots with `NotPrimary { epoch, hint }`.
- **Failover time:** about `L − margin` for the lease to lapse, plus `L + margin` of voter
  silence, which overlaps it: about 4 s locally, measured (T58b). Well inside the guest's
  30 s SCSI timeout.
- (Superseded design: leader-granted leases with leader terms, commit e77f956. Leader death
  meant 30–50 s with no writes, and a guest can hit an I/O error well before that.)

### 6.3 ISR changes (configuration, recorded on a majority, O(node pairs))
Failures are node-granular, so ISR changes are expressed per node pair:
- `Exclude { primary: P, excluded: S, since_epoch }`, recorded on a **majority** of nodes
  (durably, like lease fences). Meaning: S is out of sync for slots where P is primary and S is
  secondary. S cannot be promoted for them until it catches up.
- Before acking any write without S, P must get the exclusion onto a majority: one round per
  (P, S) pair per failure, not per slot and not per write, and no leader involved.
- Before taking over P's slots, S reads exclusions from a majority. The two majorities
  intersect, so S always sees an exclusion of itself and won't promote itself past it.
- P then recruits a replacement secondary per slot (placement chosen by P from the capacity
  view, or by leader policy). It copies the slot at a version, then appends the new node to the
  slot's ISR under a bumped slot epoch. Writes stall only for the catch-up window.
  **They are never acked with fewer than 2 copies when RF ≥ 2**; the existing durability floor is kept.
- Re-admitting S (after it heals): P catches S up, then clears the exclusion on a majority.

### 6.4 Write path (patches and full writes)
Default design (keeps today's parallel fan-out):
1. The client sends the patch to P and S **in parallel**, tagged `(slot_epoch, base_version,
   client_write_seq)`.
2. Each replica applies it only if its slot is at `base_version` under `slot_epoch`. The result
   is `version = base_version + 1`, identical everywhere, regardless of no-op detection or
   content. (This removes the 9c40-vs-8195 split outright.)
3. S also confirms to P. P acks the client only when both have `version`. P is the commit authority.
4. If S is behind (its version < base), it NACKs with its version. P pushes the missing
   deltas/base and S applies and acks. Only on persistent failure does P start §6.3.
5. If P is not primary (lease lapsed, stale epoch), it NACKs with `NotPrimary { hint }`.
   The client refreshes the slot's primary from any node and retries.

Concurrent writers to one slot (rare; VMs are single-writer): S will see a base-version
mismatch. It defers to P's ordering, and P serializes and pushes. Correctness never depends
on arrival order at S.

Fallback option, if Phase 3 measurement shows parallel fan-out complexity isn't worth it:
primary relay (client → P → S). That costs one LAN hop and is simplest to reason about.
This is a Phase 3 decision point with numbers.

### 6.5 Folds
- Only P decides to fold a slot, at a specific `version`. It sends `FoldAt { epoch, version,
  result_hash }` to the ISR. Each member folds its own base+deltas to that version and verifies
  `result_hash`. A mismatch is a divergence alarm, and P pushes its copy.
- A fold never changes `version` (content is unchanged). This makes "output id == base id"
  irrelevant: nothing keys off content identity.
- GC of old bases/deltas is P's call, after all ISR members confirm the fold at that version.
  This closes the fold-identity ABA class (no content-id reference counting needed).
- Retires: fold_lock_grants, outbound_fold_claims, coordinate_and_fold_slot fallbacks,
  debounce/backstop dual paths, and the abandon heuristics.

### 6.6 Replication and healing (moves load off the leader)
- The leader keeps **placement policy** (target RF, which nodes are candidates, capacity).
- **P executes** replication for its slots: filling followers, replacing secondaries, restoring
  RF after node loss. The leader stops walking chunks, and healing parallelizes across all
  primaries.
- The leader's healer becomes an auditor. It samples digests (§6.7) and nudges primaries,
  and no longer does a per-chunk discovery pass.

### 6.7 Divergence detection (cheap, continuous)
- Each primary periodically exchanges a per-slot `(epoch, version, content_hash)` digest with
  its ISR and followers: a Merkle tree over its primary slots, bucketed so unchanged ranges cost
  one hash.
- Any mismatch at equal `(epoch, version)` is a bug by definition. Log it loudly, then P repairs.
  This catches "two replicas moved from the same base to different content" within one digest period.

### 6.8 Reads
- Reads can go to any ISR member that is at the slot's current version. The client knows the
  version from its last write or open.
- A lapsed primary must not serve reads for its slots (§6.1). Otherwise, after failover, it
  would serve pre-failover data as current.
- Hedged reads (2854db0) stay. The hedge targets the other ISR member.

## 7. Failure decision tree

Actors: **P** (primary), **S** (secondary), **L** (leader), **C** (client), **M** (a majority of
nodes). "✗" = link down. The leader appears only to show it doesn't matter.

| # | Situation | Decision |
|---|---|---|
| 1 | All links up | Normal. |
| 2 | P–S ✗; P and S each reach M | **No promotion.** P still holds its lease (M acks it), so no majority can vote it out. P gets `Exclude{P,S}` onto M, recruits a replacement secondary, and catches it up. S is re-admitted after the link heals and it catches up. |
| 3 | S isolated from everyone | Same as #2 from P's side. S's own lease lapses, so S acts as primary for nothing. |
| 4 | P–L ✗ only | **Nothing happens.** The leader plays no part in leases. (Tested: T58c.) |
| 5 | **P reachable only by C** (P cut off from M) | P can't renew, so its lease lapses and **P fences itself**: it refuses writes and reads with `NotPrimary`. It acked nothing after that, because acks need S (principle 5). M's voters have been silent about P for `L + margin`, so they vote it expired; S takes over. C's retries get `NotPrimary` and move to S. The old primary stops before the new one starts. (Tested: T58b, the lease ended 1.5 s before the vote.) |
| 6 | S–P ✗; S reaches M and C | S may take over **only after M votes P expired**. If M still hears P (it's #2), the vote fails and S is excluded instead. S's own view never triggers promotion. |
| 7 | C–P ✗, everything else ok | Not a failover. C sends via S, which relays to P, or C retries. Clients never cause promotion (principle 3). |
| 8 | L dies, stalls, or loses its majority | **No effect on leases or writes.** Membership elects a new leader in its own time; only placement policy and auditing wait for it. (Tested: T58c, the other 4 nodes had zero lease lapses.) |
| 9 | L was also P for some slots, and it dies | It's #5 for its slots (a primary dying). That it was leader is irrelevant. |
| 10 | P and S both down | Slots are unavailable for writes. RF ≥ 3: followers are not promotable (they may lack acked writes). Wait for an ISR member. An operator-only "accept data loss" override (unclean promotion of the most-caught-up follower) exists, **off by default, never automatic.** |
| 11 | Old P returns after failover | Its renewals are refused at the fenced incarnation, so it rejoins under a new one, holding no slots it lost. Its slot epoch is stale, so its slot messages are rejected. It rejoins as follower/secondary via catch-up. Anything it holds past the last acked version was never acked, so it's discarded. |
| 12 | Gray failure (P slow, not dead; e.g. the 26 s stalls) | Renewal on its own path means a stall in unrelated subsystems doesn't cost the lease. A whole-process or network stall longer than `L − margin` does, and then S takes over within about 4 s instead of the guest waiting out the stall. Clients hedge reads to S. |
| 13 | Flapping node / membership churn | Minimum dwell time before a node that just rejoined can be primary again, with backoff. The Exclude/re-admit cycle is rate-limited per node pair. |
| 14 | Asymmetric link (P→M works, M→P doesn't) | Renewal is P-initiated request/response; an ack counts only if the *response* arrives. P loses its lease, and M's voters, having received renewals, don't vote it out until they go silent too. The system is safe: P fenced itself. Takeover waits until M stops hearing P, so availability suffers, not safety. |
| 15 | Partition, e.g. {g1,g2} \| {g3,g4,g5} | The 2-node side can't reach a majority, so its nodes' leases lapse and they fence. The 3-node side keeps its leases, votes the other side out, and takes over their slots where the ISR partner is on its side. Slots whose entire ISR sits in the minority stay unavailable (#10) and are not lost. (Tested: T58d.) |

## 8. Phases

Each phase has a verification gate. New tests are written first and shown failing, as usual.
Partition tests use the **fault-injection link filter** (built in Phase 0, local suite only):
`SetPeerFilter { drop_to, refuse_clients, mode: Refuse | BlackHole }`, driven by
`dfs-admin fault set|clear`. It is outbound-only per node: set both sides for a partition,
one side for an asymmetric link. This makes #2–#15 deterministic on the local 5-node cluster.

**Phase 0 — Observe only (no behavior change)**
- **Slot audit.** For each slot, the *audit owner* is chosen by rendezvous hash over the
  slot's listed holders (the highest hash(slot, node) wins). It is deterministic,
  order-independent, balanced across nodes, and needs no coordination, so it stands in for
  the future primary. It tracks slots that changed since their last audit. Once a slot has been
  write-quiet for Q seconds, it sends each other listed holder one batched
  `AuditSlots { entries: [(file_id, chunk_idx, my_chunk_id)] }` per peer per period.
  The peer replies with mismatches only: a different current id, or bytes/patch state for the
  listed id absent (a **phantom holder**, the 2026-09-27 bug). Cost scales with changed slots,
  not total slots, and at most a fixed number of slots go out per pass.
- **Confirmation:** a first sighting is only a suspect. It is re-checked after a confirm delay
  and reported as `[DIVERGENCE]` (WARN, with how long it persisted) only if still there;
  otherwise it's counted as `[DIVERGENCE-TRANSIENT]` with how long it took to clear. One
  observation can't tell propagation lag from drift. The lag measurements feed the Phase 3
  fan-out decision.
- Settled slots are **re-audited** periodically (bytes can vanish under an unchanged view), and
  slots of files deleted from the owner's file table are skipped.
- Per-pass summary line `SLOT AUDIT pass: …` with running totals. (Findings live in the logs;
  no dfs-admin display.)
- **Fault-injection filter** in the network layer (`SetPeerFilter`, refused unless the server
  runs with `DFS_FAULT_INJECTION=1`). Phases 1–3 need it for the failure matrix.
- **Stall measurement:** each node logs, once a minute, the worst heartbeat round trip to each
  peer. A week of this gives the partial-stall distribution that Phase 1 sizes `L` against.
- (Shadow slot versions moved to Phase 3. A version counter no node is authoritative for
  can't be compared across replicas, so it would measure nothing.)
- Gate: one week on staging. We get a measured divergence rate and a list of which paths
  produce it. That confirms (or refutes) that replica drift explains our incident load before
  we commit to Phase 3.

**Phase 1 — Majority node leases** (built, shadow only; local gate passed)
- §6.1. Leases are held, lost, fenced and voted on, but nothing consults them yet.
- State machines with explicit clocks, plus a seeded randomized simulation: 5 nodes, random
  delays and partitions, the invariant checked at every step. Mutating any voter rule
  (fence check, silence window, self-vote) breaks it within the first seed.
- Suite, real 5-process cluster:
  - T58: an isolated node fences before the majority vote (#5); cutting the leader off costs
    the others nothing (#4/#8); a 2|3 split fences exactly the minority (#15); heal and rejoin (#11).
  - T59: holding the metadata DB lock, the healer maps, or the membership lock for 3×`L`
    costs no lease anywhere. A frozen process (SIGSTOP) loses its lease and is voted out
    only after its lease ended: the gray-failure takeover of #12.
  - T60 chaos: random partitions, black-holes, one-way cuts (#14) and freezes; every majority
    expiry is checked against the target's logged lease extensions. 0 overlaps across 141
    expiries in 300 s.
- Bugs these tests caught before any enforcement existed: majority counted over current
  membership (startup split-brain); "never heard from" tested as incarnation 0 (no votes ever
  ran); the lease loop blocking on the membership lock; **a node voting itself expired**
  (40 overlaps in the first chaos run: the simulation had modeled the rule but the runtime
  didn't have it; the rule now lives in the voter state machine, and the simulation asks the
  target too).
- Moved to Phase 3: the **flap guard (#13).** A rejoining node never takes its old slots back,
  so flapping costs nothing until takeovers start rewriting ISRs.
- `L` stays at the local default (3 s, margin 500 ms). It is confirmed against real
  `PEER RTT` numbers at the branch's final staging validation (see "Staging policy" below).

**Phase 2 — One fold owner per chunk** (built; local gate below)
- A chunk's **fold owner** is the first of its holders, in rendezvous order, that isn't known
  to be majority-expired; this node counts only while it holds its own lease. Every node
  computes it locally; there is no negotiation.
- Background folds (debounce, backstop sweep, post-restart resume) start only on the owner, as
  a wave (fold, then push the bytes to peers; >= 2 matching copies). A non-owner waits; if the
  owner's result has already arrived it adopts it; after `FOLD_OWNER_PATIENCE` (60 s dirty,
  3x the debounce) it takes over, logged `[FOLD-OWNER]` and counted.
- Retired from use: the `ProposeFold`/`ReleaseFoldLock` negotiation, fold-lock grants, and its
  "no peer" / "hard failure" solo-fold fallbacks (listed in TODO_DEAD_CODE.md, deleted in Phase 5).
- **Deliberately unchanged: client ForceFold.** The client already folds on every replica it
  wrote and cross-checks the results. Forwarding it to the owner would leave non-owners without
  the folded bytes until healing (the phantom-holder shape), and pushing them from the owner
  would put a 4 MB transfer in the client's flush path. It moves to Phase 3, with the write path.
- Known limit until Phase 3's stored ISR: the owner is computed from each node's holder list, so
  nodes whose lists differ can each see themselves as owner. Safe (the wave's >= 2 rule), and no
  worse than the negotiation it replaces.
- Tests: `fold_role` unit test; non-owner defers / owner folds without negotiating / takeover
  after patience, each shown failing against the old negotiation body; adopt-instead-of-takeover
  guard. Suite T61: a 16-chunk patch storm, then quiet; every chunk's background folds start on
  one node, no takeovers, no replica disagreement. A planted "every holder owns" fault fails
  T61 on all 16 chunks.

**Phase 3 — Stored ISR, 2-replica writes, versions, catch-up** (DESIGN, awaiting approval)

Where the write path stands today (read 2026-09-29):
- Full writes already go to an ordered pair (`write_data_dual_replica`, `select_write_pair`).
- Patches go to **every** holder, except the fsync/release path (`use_dual_rf: true`,
  fuse_impl.rs ~3977), which picks two with the **leader first**, then by address.
- Each replica mints its own token; ordering is the per-file `client_write_seq`.
- The client already cross-checks ForceFold results across replicas.

Four sub-phases, each gated on its own (§8a):

**3a — The ISR as a majority-agreed record per chunk (server-only).** (built)
- `SlotIsr { epoch, members }` per `(file_id, chunk_idx)`. Each epoch's value is chosen by
  **single-decree Paxos** among all nodes (dfs-server/src/slot_isr.rs). The first design
  said "compare-and-set on a majority"; that is unsafe when two nodes propose at once (each
  can win a different minority), so it's Paxos: prepare/promise, then accept, and a proposer
  adopts the highest-ballot value already accepted.
- Acceptor state lives in a `slot_isr` table written with Durability::Immediate (an acceptor
  must not answer ahead of its disk); prepare/accept/commit/get are batched RPCs, so one pass
  costs one fsync per node, not one per chunk. A committed-record cache serves hot paths.
- **Seeding:** every 30 s (`DFS_SLOT_ISR_SEED_SECS`), at most 500 chunks per pass, each
  seeded by its first rendezvous holder: epoch 1 = the first 2 holders in rendezvous order.
  Deleted files are skipped.
- **Fold owner** now follows the stored ISR when a chunk has one (Phase 2's known limit is gone
  for seeded chunks), falling back to rendezvous over holders otherwise.
- **How it's tested, stated plainly:**
  - Safety (never two values chosen for one epoch) is proven by a seeded randomized
    simulation (5 acceptors, 3 competing proposers, loss/duplication/reordering, restarts),
    which fails within one seed when a proposer ignores accepted values or an acceptor accepts
    below its promise. Its first run caught a real bug: counting a duplicated reply twice
    faked a majority; replies are now counted per acceptor.
  - Suite T62 proves the wiring on the real cluster: seeding converges on all 5 nodes; 5 nodes
    racing different values (5 rounds, during a 2|3 partition, and with two acceptors stalled)
    always end on one value per epoch, checked from every node's commit log. T62 alone would
    NOT catch a broken proposer: a planted "skip phase 1" fault passed it, because the conflict
    window is ~1 ms on a local cluster. The simulation is the safety proof.
- Still to do before 3b relies on it: **catch-up** for a node that missed a commit broadcast
  (today it's stale until the chunk's next epoch; safe for folds, not for clients).
- Gate 2026-09-29: suite 142/1 at 13m15s. The one failure, T61d (replica disagreement during a
  patch storm), was shown to be **pre-existing**, not caused by 3a: T60-then-T61 reproduces it on
  the committed Phase 2 build (1 run in 4, 10 disagreements) and on 3a (1 in 2). The cause is the
  client ForceFold folding both replicas at once mid-storm. T61d is informational until 3c, which
  removes the race, and becomes required again there.

**Order change (2026-09-29, from the repo's history): 3c before 3b.** Narrowing writes to 2
replicas before chunks have versions reopens the hazard from commit ef0afac (2026-05-15): the
third replica keeps a stale copy and the healer can copy it back over the fresh two (tombstones
were added to contain that). With versions, a stale copy is visibly older and is never copied over
a newer one, so versions go first. Next after 3a: catch-up for missed commits, then 3c, then 3b.

**3b — Client writes to exactly the ISR pair (client + server).** Principle 4.
- Server answers a new `GetSlotIsr` (batched); the client caches ISRs by epoch.
- Patches and full writes go to the ISR's first two members only; replicas 3..n are left to the
  primary (healer until Phase 4). The all-holder fan-out and the leader-first ordering go away.
- Any request carrying a stale epoch gets `NotPrimary`/`StaleIsr { epoch, isr }`; the client
  refreshes and retries. An old client keeps working (servers still accept the old requests).
- **ForceFold goes to the primary**, which folds as a wave and pushes the bytes to S, so S is
  never left without them (the reason it stayed unchanged in Phase 2).
- Test gate: kdiskmark RND4K Q1T1/Q32T1 locally before and after, within noise (fewer targets
  should help); a patch storm shows 2 targets per write; T53/T61 unchanged.

**3c — Slot versions (server + client).**
- Each write carries `(epoch, base_version)`; a replica applies it only at `base_version`, giving
  `version = base_version + 1` everywhere, whatever each node's no-op detection decides (this
  removes the 9c40-vs-8195 split outright).
- The primary is the commit authority: it acks when S has the version. S behind: it NACKs with
  its version, and P pushes the missing deltas/base.
- **Ordering (decided 2026-09-29): the primary orders, the data still fans out.** Parallel
  fan-out alone (the client orders) lets two writers to one chunk land in different orders on P
  and S at the same version: silent divergence. A naive store-and-forward relay orders correctly
  but serializes P's and S's hash+write and sends every payload twice. So split data from
  ordering: the client sends the bytes to P and S in parallel as today; P, under the slot lock,
  assigns `version = n` to the write (microseconds, before any hashing or disk) and sends S a
  small "version n = write X" message; each replica hashes and writes in parallel, S applying
  strictly in version order (holding a payload whose version hasn't arrived yet). The client is
  acked when both hold version n. Expected cost over today: one small-message hop (~0.2 ms).
- Behind a flag (`DFS_ORDERED_WRITES`), measured before it becomes the default: fio 4k randwrite
  QD1 and 32-way, plus a large sequential write, flag off vs on, same build (`scripts/bench_fio.sh`).
  Keep it if the difference is within run-to-run noise; if 4k QD1 regresses beyond noise, add a
  fast path for single-writer chunks (VM disks) rather than drop the ordering.
- Test gate: the 2026-09-27 no-op-divergence repro (identical rewrite on a base one replica
  can't compose) must end with one version on both; lagging-secondary catch-up; T60-style chaos
  with writes running, no acked write lost (verified by reading back every acked range).

**3d — Exclusions and replacement (server).**
- S unreachable: P gets `Exclude{P,S}` onto a majority before acking without S, recruits a
  replacement secondary, catches it up, then writes resume with 2 copies (the durability floor
  is never lowered).
- A takeover (majority expired P) makes S primary: ISR epoch+1 via the same compare-and-set.
- **Flap guard** (from Phase 1): a rejoined node is re-admitted only after a dwell time.
- Test gate: failure matrix rows #2, #3, #5, #6, #10, #11, #13 on the local cluster, each asserting
  no acked write is lost and no two primaries accept writes at the same epoch.

Order and risk: 3a is server-only and invisible to clients (low risk). 3b changes what the client
sends. 3c changes what a write means. 3d is where failure handling becomes real. Each stays on
this branch and off staging until the whole branch is proven.

**Phase 4 — Primary-driven replication and healing**
- §6.6. The leader healer becomes placement policy plus auditor.
- Gate: leader CPU and RPC rate measured before/after (expect a large drop). RF restore time
  after a node kill is no worse.

**Phase 5 — Delete what's now dead, and take per-write traffic off the leader**
- Ghost-chunk guard, RevalidateChunkSlot, location_supersedes ranking, never-revert guard,
  RCL union-merge, abandon heuristics, content-id reference assumptions. Each removal gets
  its own commit, and TODO_DEAD_CODE.md is updated.
- **Stop sending every write's chunk location to the leader.** Today each client write also
  sends a location update (ReplicateChunkLocation*) that the leader arbitrates. That's the
  traffic behind 573k declined updates in 25 minutes (2026-09-08) and the leader's
  chunk_map lagging clients for days. Once primaries are the authority for their slots:
  - The primary holds the slot's `(epoch, version, ISR)`. Clients learn locations from the
    primary (or any ISR member), with the leader's map as a hint.
  - The leader's chunk_map becomes a *cache*, refreshed in bulk from primaries' digests
    (§6.7) rather than per write. Its job is placement policy and answering "who owns this
    slot", not tracking every version.
  - Location updates to the leader happen only on ISR changes (rare), not on writes.
  - Gate: leader RPCs per client write, measured before and after on staging under
    kdiskmark and VM-108 load. Target: near zero on the steady-state write path. Also,
    client read latency after a cold cache is no worse (the lookup moves from leader to
    primary).

## 8a. Test protocol (every phase, no exceptions)

A phase is not "done" until every step below passes, in order. Its successor doesn't start
until then.

1. **New tests first, shown failing.** Each phase's behaviors get unit tests (real multi-Server
   networking where peers matter) and local-suite tests. Each is run against the pre-phase code
   and shown to fail, or, for pure additions, shown to detect a planted fault.
2. **Unit tests:** the `dfs-server` failure set must equal the baseline
   (project_20260905_preexisting_dfs_server_test_failures), compared as a set.
3. **Local suite, timed and build-free** (`{ time ./scripts/test_local_suite.sh; }`): pass count
   no worse than the last clean run (110), and elapsed time within noise of the previous run.
   The known rename flake (T13b/T14a/T14b together) is re-run in isolation before being dismissed.
4. **Phase-specific local tests.** The failure-matrix rows the phase claims, run via the
   fault-injection filter, deterministically (no wall-clock races).
5. **Staging: not per phase** (see "Staging policy"). Deferred to one final validation of the
   whole branch, with explicit go-ahead then: deploy, then the phase's soak (at minimum
   VM-108 normal use, and kdiskmark RND4K Q1T1/Q32T1 before and after for any write-path phase).
   Watch the `[DIVERGENCE]` counters and guest EIOs.
6. **Rollback checked:** flip `ownership_mode` back one step on the local cluster and re-run the
   suite, proving the phase can be backed out live.

### Staging policy (2026-09-28)
The branch stays **off staging** until it is proven stable as a whole, observe-only phases
included. Each phase is proven locally: unit tests, the seeded simulation, the suite, and the
chaos gate. Staging is used once, for a final validation of the stable branch, and only with
explicit go-ahead then. Parameters meant to be tuned from staging data (lease length from
`PEER RTT`, divergence rates, lag distribution) keep their local defaults until then.

## 9. Compatibility and rollout
- New `Request`/`Response` variants are **appended at the end** of the bincode enums
  (feedback_bincode_enum_variants_must_append_at_end). No new fields on existing variants
  (feedback_bincode_field_addition_not_backward_compatible). New structs are used instead.
- A cluster-wide `ownership_mode` flag moves `off → shadow → enforce`, and is only allowed to
  advance when every node and every client reports support. Deploy order per phase: servers
  first, then clients (both arm64 deploy-build.sh hosts **and** the x86 hypervisors via
  dist/x86_64).
- Rollback: `enforce → shadow` is always safe because tokens keep working until Phase 5.

## 10. Risks
- **Spurious failovers from short leases.** A 3 s lease means a node whose process or network
  stalls for more than about 2.5 s gives up its slots. Failing over beats a hung guest, but
  flapping is a risk: the Phase 1 stall and flap work and Phase 0's staging measurements decide `L`.
- **Lease tuning vs stalls:** too short means spurious failovers during the known stalls;
  too long means slow failover. Mitigated by the dedicated renewal path, the measured stall
  distribution, and relayed renewal.
- **Phase 3 write-path change:** the largest blast radius. Kept behind `ownership_mode` with
  shadow validation first.
- **Disk-full cluster (94–98%):** catch-up and secondary replacement need headroom. Phase 3
  must refuse to start replacements that can't complete, and surface the error instead of
  thrashing.
- **Mixed-version windows:** handled by the flag gating, never by inference.

## 11. Open questions
1. Initial `L` and margin: pick from Phase 0 stall measurements (renewal-latency histogram).
2. Parallel fan-out vs primary relay: decided in Phase 3 with numbers.
3. Should a follower (RF ≥ 3) serve reads? Only if at the current version. Needs the version
   hint from the client.
4. Digest period and bucket size: pick for < 1% network overhead at staging write rates.
5. Multi-writer files (shared, non-VM): confirm the serialization cost at P is acceptable.
