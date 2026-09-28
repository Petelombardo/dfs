# Slot Ownership Plan — one owner per chunk slot

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

1. **Leader stays off the data path.** The leader's job is appointing owners and changing
   configuration, never per-write work. Its state must scale with *nodes*, not chunks or
   writes. (gluster1 stalled 26 s on 2026-09-24. Anything per-write on the leader would have
   stalled every guest.)
2. **No extra hop on the hot path by default.** Client write latency must stay within noise
   of today's.
3. **Clients never cause a promotion.** Only the leader decides a failover, based on its own
   reachability plus lease expiry. A client's view of the network is advisory.
4. **An acknowledged write exists on every in-sync replica.** This single rule is what makes
   any in-sync replica safe to promote.
5. **Deterministic, testable failure handling.** Partitions are tested by injected message
   filters, not wall-clock races (see feedback_timing_based_concurrency_tests_unreliable).
6. **Incremental.** Every phase is deployable on its own and earns its keep before the next.

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

- **Leader**: existing role (lowest NodeId with majority view). Gains a **term**: a
  monotonic number, persisted on a majority, bumped on each leadership change.
- **Node lease**: a time-bounded grant from the leader to a node: "you may act as primary
  until T". There is **one lease per node** (5 leases on staging), not one per chunk. Renewed
  by a cheap heartbeat.
- **Slot**: `(file_id, chunk_idx)`.
- **ISR (in-sync replica list)**: per slot, the *ordered* list of nodes guaranteed to hold
  every acknowledged write. Normally `[primary, secondary]`. Order matters.
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
- The leader only tracks **node leases** (N entries) and **node-pair exclusions** (see §6.3).
- When node P's lease lapses, *every* slot where P was primary fails over to its secondary
  at once. There is no per-slot leader work: each node derives the new primary locally.

Leader state is O(nodes + node pairs). Hot-path cost is zero leader RPCs.

## 5. Topologies

| Setup | Behavior |
|---|---|
| **RF = 1** (aggregation, no redundancy) | ISR = `[holder]`. Every holder is primary for its slots. The lease still fences a node that was declared dead from resurfacing and writing. Node down means its slots are unavailable (no copy exists). |
| **RF = 2** | ISR = `[P, S]` as chosen by the client at creation. Writes are acked only when both have them. P fails → S promoted (after lease expiry). S fails → P recruits a replacement secondary. |
| **RF ≥ 3** | The client still writes only P and S (quorum 2). P replicates asynchronously to followers. Only ISR members are promotable. A follower becomes secondary only after catch-up plus a leader-recorded ISR change. |

This matches the rule that the client writes 2 replicas. It also **narrows today's patch
fan-out from all holders to P+S** (a behavior change, Phase 3).

## 6. Mechanisms

### 6.1 Leader term (prerequisite)
- When a node becomes leader, it reads `max_term` from a majority, sets `term = max + 1`, and
  persists it to a majority before granting anything.
- Nodes reject lease grants and config changes carrying a term lower than the highest they've seen.
- This orders the two-leaders-with-overlapping-views case that the min-ID rule alone allows.

### 6.2 Node leases
- Duration `L` (start at 10 s; tune against observed stalls). The node renews every `L/3`.
- **Primary self-fence rule:** a node acts as primary only while
  `now_mono < renew_request_sent_at + L − margin`. It measures from when it *sent* the renewal,
  so its own clock is the conservative one. Once lapsed, it refuses both writes **and** reads
  for slots it's primary of, returning `NotPrimary { epoch, hint }`.
- **Leader re-appoint rule:** the leader treats a lease as expired only after `L + margin` since
  it last *granted* it. Because it waits longer than the holder can act, a lapsed primary has
  stopped before its replacement starts. Only bounded clock *drift* is assumed, not synchronized clocks.
- Renewal runs on a **dedicated lightweight task and connection**. It must not wait on the
  metadata committer, healer locks, or compaction. A long stall of the node's own storage
  stack should *correctly* cost it its lease, and a stall of unrelated subsystems must not.
- Optional (Phase 1b): relayed renewal. If P can't reach L but can reach S, S may forward P's
  renewal. This avoids a failover for a single broken P–L link.

### 6.3 ISR changes (configuration, via leader, O(node pairs))
Failures are node-granular, so ISR changes are expressed per node pair:
- `Exclude { primary: P, excluded: S, since_epoch }`, recorded by the leader under its term.
  Meaning: S is out of sync for slots where P is primary and S is secondary. S cannot be
  promoted for them until it catches up.
- Before acking any write without S, P must first get the exclusion recorded. That is exactly
  one leader RPC per (P, S) pair per failure, not per slot and not per write.
- P then recruits a replacement secondary per slot (placement chosen by P from the capacity
  view, or by leader policy). It copies the slot at a version, then appends the new node to the
  slot's ISR under a bumped slot epoch. Writes stall only for the catch-up window.
  **They are never acked with fewer than 2 copies when RF ≥ 2**; the existing durability floor is kept.
- Re-admitting S (after it heals): P catches S up, then asks the leader to clear the exclusion.

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
- A lapsed primary must not serve reads for its slots (§6.2). Otherwise, after failover, it
  would serve pre-failover data as current.
- Hedged reads (2854db0) stay. The hedge targets the other ISR member.

## 7. Failure decision tree

Actors: **P** (primary), **S** (secondary), **L** (leader), **C** (client). "✗" = link down.

| # | Situation | Decision |
|---|---|---|
| 1 | All links up | Normal. |
| 2 | P–S ✗; P–L ok; S–L ok | **No promotion.** P is leased and alive. P gets `Exclude{P,S}` recorded, recruits a replacement secondary, and catches it up. S is re-admitted after the link heals and it catches up. Promoting S here would create two primaries. |
| 3 | P–S ✗; S–L ✗; P–L ok | S is isolated. Same as #2. |
| 4 | P–L ✗; P–S ok; S–L ok | P can't renew. With relayed renewal (6.2 option), S forwards it and nothing changes. Without it, P self-fences at lease lapse, L waits `L+margin`, then S becomes primary (it is ISR, so it has every acked write). P rejoins as secondary after catch-up. |
| 5 | **P reachable only by C** (P–S ✗, P–L ✗, C–P ok). This was your open case. | P's lease lapses and **P fences itself**: it refuses writes and reads with `NotPrimary`. It can't have acked anything since then, because acks need S (rule 4). L waits `L+margin` and promotes S. C's retries against P get `NotPrimary`, so C refreshes and moves to S. No split-brain: the old primary stops before the new one starts. |
| 6 | S–P ✗; S–L ok; S–C ok (your promotion case) | Promote S **only if L also can't reach P and P's lease has expired.** If L can reach P, this is #2 and S is excluded instead. S's own view never triggers promotion. |
| 7 | C–P ✗, everything else ok | Not a failover. C sends via S, which relays to P, or C retries. Clients never cause promotion (principle 3). |
| 8 | L fails / L loses majority | Existing quorum gate: the old L stops being leader. The new leader bumps the term, learns the lease table from a majority, and **must not re-appoint any node until that node's last lease could have expired.** Primaries keep serving on their unexpired leases, so a leader change alone causes no write outage. |
| 9 | L was also P for some slots, and it fails | Your case: the new leader is elected (#8), waits out the old L's lease, then its slots derive S as primary. If the old L is alive but partitioned, it is #5 for its slots (it self-fences). |
| 10 | P and S both down | Slots are unavailable for writes. RF ≥ 3: followers are not promotable (they may lack acked writes). Wait for an ISR member. An operator-only "accept data loss" override (unclean promotion of the most-caught-up follower) exists, **off by default, never automatic.** |
| 11 | Old P returns after failover | Its epoch is stale, so all its slot messages are rejected. It rejoins as follower/secondary via catch-up from the new P. Anything it holds past the last acked version was never acked, so it is discarded. |
| 12 | Gray failure (P slow, not dead; e.g. the 26 s stalls) | Renewal on a dedicated path (6.2) means that a stall in unrelated subsystems doesn't cost the lease, and a real storage stall does. Clients hedge reads to S. Tune `L` against observed stall durations. |
| 13 | Flapping node / membership churn | Minimum dwell time between re-appointments of the same node, with backoff. The Exclude/re-admit cycle is rate-limited per node pair. |
| 14 | Asymmetric link (P→L works, L→P doesn't) | Renewal is a P-initiated request/response. The lease only counts if the *response* arrives. If it doesn't, this is #4. |
| 15 | Network partition splits the cluster (e.g. {g1,g2} \| {g3,g4,g5}) | The minority side has no leader (quorum gate). Its primaries self-fence at lease lapse. The majority leader re-appoints after expiry. Slots whose entire ISR sits in the minority stay unavailable (#10) and are not lost. |

## 8. Phases

Each phase has a verification gate. New tests are written first and shown failing, as usual.
Partition tests need a **fault-injection message filter** (test builds / local suite only):
`SetPeerFilter { drop_to: [NodeId], drop_from: [NodeId] }`. This makes #2–#15 deterministic
on the local 5-node cluster.

**Phase 0 — Observe only (no behavior change)**
- **Slot audit.** For each slot, the *audit owner* is the lowest NodeId among the slot's
  listed holders. It is deterministic and needs no coordination, so it stands in for the
  future primary. It tracks slots that changed since their last audit. Once a slot has been
  write-quiet for Q seconds, it sends each other listed holder one batched
  `AuditSlots { entries: [(file_id, chunk_idx, my_chunk_id)] }` per peer per period.
  The peer replies with mismatches only: a different current id, or bytes/patch state for the
  listed id absent (a **phantom holder**, the 2026-09-27 bug). Cost scales with changed slots,
  not total slots.
- Each mismatch is logged as `[DIVERGENCE]` at WARN with the slot and both views. Counters go
  into node stats, and `dfs-admin` shows them.
- **Fault-injection filter** in the network layer (`SetPeerFilter`, off unless explicitly set)
  plus a local-suite helper to partition node pairs. Phases 1–3 need it for the failure matrix.
- (Shadow slot versions moved to Phase 3. A version counter no node is authoritative for
  can't be compared across replicas, so it would measure nothing.)
- Gate: one week on staging. We get a measured divergence rate and a list of which paths
  produce it. That confirms (or refutes) that replica drift explains our incident load before
  we commit to Phase 3.

**Phase 1 — Terms and node leases**
- Leader term (§6.1), node leases (§6.2), and a lease table persisted on a majority.
- `NotPrimary` responses exist, but the write path doesn't consult them yet.
- Local tests: #4, #5, #8, #9, #11, #14, #15 via the filter, asserting who *would* be primary
  and that no two nodes ever both believe they hold the lease for the same interval.
- Gate: a chaos run on the local cluster (random filters, kill/restart) with zero overlapping
  leases. Staging soak with leases shadowed.

**Phase 2 — Primary-owned folds**
- §6.5. Only the derived primary folds. The others fold on `FoldAt` and verify hashes.
- Retire fold_lock_grants / outbound_fold_claims / uncoordinated fallback.
- Gate: T53-style storm with zero REPLICA DISAGREEMENT and zero divergence alarms. VM-108 soak.

**Phase 3 — Versioned writes, ISR, catch-up**
- §6.3 + §6.4: the client narrows patch fan-out to P+S with `(epoch, base_version)`, and
  primary-driven catch-up. This is the parallel fan-out vs primary relay decision, made with
  latency numbers (kdiskmark RND4K Q1T1/Q32T1 before and after).
- The failure matrix #2–#7, #10 enforced and tested.
- Gate: full suite, all failure-matrix tests, a kdiskmark regression within noise, and a
  multi-day VM-108 soak with zero divergence alarms.

**Phase 4 — Primary-driven replication and healing**
- §6.6. The leader healer becomes placement policy plus auditor.
- Gate: leader CPU and RPC rate measured before/after (expect a large drop). RF restore time
  after a node kill is no worse.

**Phase 5 — Delete what's now dead**
- Ghost-chunk guard, RevalidateChunkSlot, location_supersedes ranking, never-revert guard,
  RCL union-merge, abandon heuristics, content-id reference assumptions. Each removal gets
  its own commit, and TODO_DEAD_CODE.md is updated.

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
5. **Staging, only with explicit go-ahead each time:** deploy, then the phase's soak (at minimum
   VM-108 normal use, and kdiskmark RND4K Q1T1/Q32T1 before and after for any write-path phase).
   Watch the `[DIVERGENCE]` counters and guest EIOs.
6. **Rollback checked:** flip `ownership_mode` back one step on the local cluster and re-run the
   suite, proving the phase can be backed out live.

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
