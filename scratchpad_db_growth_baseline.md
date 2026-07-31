# Metadata DB growth baselines (Phase 0, 2026-07-15, pre-fix)

> **UPDATE 2026-07-16 — post-fix results (Phases 1+2: healing batching + group commit):**
> - Heal repro, drain-phase leader windows: 56.5MB file / ~4,500 single-record txns/min
>   → **7.5MB file**, heal commits collapsed into ~20 batch txns + group commits, and the
>   put_pending_healing/delete_pending_healing single-record storms are gone entirely.
> - RND4K: the single-record quartet (put/delete_chunk_location, put_chunk_seq,
>   put_patch_state_pending) now flows through `group_commit` (coalescing factor ~1.3x at
>   local 616 IOPS; scales with load by construction). Remaining dominant site:
>   **put_files_batch +27.4MB/min of FileMetadata blob rewrites (~12/sec during fio)** —
>   also +19.5MB/min during heal-repro ingest. **Phase 3 (FILE_TABLE blob churn) is
>   confirmed triggered and is the gating follow-up for the kdiskmark scenario.**
> - Full local suite: initial runs failed intermittently with empty/missing persisted
>   FILE records. Root cause (found via isolated repro + gdb, 2026-07-16): TWO
>   PRE-EXISTING HEAD bugs the batching work exposed by changing churn patterns —
>   (1) run_planned_offline_compaction drained the metadata persist worker and resumed
>   serving without respawning it (silent no-persist until restart; ReconcileMetadata
>   then propagated the stale inventory as cluster-wide deletions of live files);
>   (2) should_compact's scale-free fragmentation ratio made near-empty DBs escalate
>   into availability-pausing offline compactions every 1-3min. Both fixed
>   (Server::restart_sled_writes + COMPACT_MIN_RECLAIMABLE_BYTES floor). Also fixed a
>   real regression of this work's own: heal outcomes originally flushed only at
>   >=32-or-end-of-drain (T38b convergence miss) — now flush after every completion with
>   a try_join_next scoop. Final: repro 25/25 clean, suite 104 PASS / 0 FAIL in 5m19
>   (faster than pre-fix runs — the misfiring compactions were eating suite time).

Instrumentation: per-call-site write-txn counters (`MetadataStore::note_txn`) + 60s `[META TXN]`
delta log in the compaction-check loop. Local 5-node cluster (ports 8900-8904), release build.

## Scenario 1: healing backlog (`scripts/repro_db_growth_heal.sh`)

500 x 4MB files written, one node killed + data-wiped + rejoined, backlog drained.

Per-node metadata DB file size (first / peak / last — "last" is post-compaction):

| node | first | peak | last |
|---|---|---|---|
| node1 | 1.4MB | 15.5MB | 3.5MB |
| node2 | 14.5MB | 14.5MB | 3.5MB |
| node3 | 14.5MB | 14.5MB | 6.5MB |
| node4 | 1.9MB | 12.0MB | 3.5MB |
| node5 (leader) | 56.5MB | **56.5MB** | 3.5MB |

Leader at peak: `db=56.5MB live=0.7MB frag=48.9MB` → **~98.6% of the file is reclaimable garbage**,
confirming per-txn COW churn (NOT geometric pre-allocation headroom) dominates at this scale.

Dominant sites on the leader during the drain (one 60s window):
- `put_pending_healing=+1032tx` — txn-count leader
- `put_chunk_location=+880tx` — heal completions + RCL singles
- `delete_pending_healing=+508tx` — one per healed chunk (clear_pending)
- `put_chunk_locations_batch=+500tx` — 500 calls ≈ batch-of-1 usage (batching defeated upstream)
- `put_files_batch=+462tx/+19.5MB` — **payload-bytes leader**: full FileMetadata blob rewrites

## Scenario 2: RND4K writes (`scripts/repro_db_growth_rnd4k.sh`)

1GB file, fio randwrite 4K, 120s. Result: 616 IOPS, 289MiB written.

| node | first | peak/last |
|---|---|---|
| node1 | 28.5MB | 28.5MB |
| node2 | 7.5MB | 24.5MB |
| node3 | 1.2MB | 12.5MB |
| node4 | 7.5MB | 14.5MB |
| node5 | 7.5MB | 14.5MB |

Write-target node during fio (60s window): `put_chunk_location=+1634tx delete_chunk_location=+1455tx
put_chunk_seq=+1377tx put_patch_state_pending=+1371tx` — ~5800 single-record txns/min with ~0.4MB
of actual payload; file at `db=24.5MB live=0.8MB frag=13.7MB`.

## Conclusions / gates

1. Per-record transaction churn is confirmed as the dominant growth mechanism in BOTH scenarios
   (live bytes stay <1MB while the file grows 10-50x that). Phases 1-2 target the right sites.
2. `delete_chunk_location` is as hot as `put_chunk_location` under patch load (chunk-ID rotation) —
   must be included in the group-commit op set (it is, per plan).
3. Phase 3 gate: `put_files_batch` at +19.5MB/min of blob payload during healing IS significant —
   FILE_TABLE blob churn deserves a follow-up look after Phases 1-2 land (separate change).
4. Local absolute numbers are smaller than staging incidents (shorter runs, smaller trees, and the
   offline compactor reclaims aggressively here) — compare rates/attribution, not absolutes.

## Appendix: growth rates (max growth over any 60s window, from db_growth.csv)

| node | heal repro | rnd4k repro |
|---|---|---|
| node1 | **+14.1MB/60s** | +0.0MB (leader; 28.5MB was pre-accumulated during the 1GB create, then internal free pages absorbed fio churn) |
| node2 | +9.0MB/60s | **+17.0MB/60s** |
| node3 | +10.5MB/60s | +10.8MB/60s |
| node4 | +10.1MB/60s | +7.0MB/60s |
| node5 | +0.0 (peaked during write phase at 56.5MB) | +7.0MB/60s |

Heal-repro drain ran at only ~4 heals/s; staging's 257MB/min incident is the same mechanism at
production heal concurrency and FILE_TABLE size.

## Appendix: pre-existing unit-test failure (NOT from the Phase 0 change)

`metadata::tests::test_redb_fragmentation_stats_reports_low_fragmentation_after_many_small_writes`
fails identically on the unmodified baseline (verified via `git stash` A/B on 2026-07-15): 500
sequential `put_chunk_location` commits on a fresh DB show **91.7%** fragmented, vs the test's <20%
expectation. The test encodes the repo narrative that per-record commits produce little genuine
waste; both repro baselines above contradict that (live <1MB inside 10-56MB files). Phases 1-2
should flip this test to passing by construction (far fewer commits), resolving the plan's "open
contradiction". All other 19 metadata unit tests pass with the instrumentation in place.
