# Dead code / cleanup tracker

Running list of dead code, unused functions, and known-broken-but-unreachable paths found
during development. Not urgent — tracked here so we can batch a cleanup pass later instead
of losing track of them. Add an entry whenever you find one; remove the entry when cleaned up.

## Found 2026-07-16 (Phase 4: chunk_locations decoupling work)

- **`dfs-server/src/metadata_sql.rs`** — entire file/module (`SqlMetadataStore`). Zero
  references to `SqlMetadataStore` anywhere outside its own defining file — never
  instantiated. Looks like an abandoned alternate SQLite-backed metadata store, superseded
  by the redb-based `MetadataStore`. Candidate for full deletion (confirm nothing external
  depends on it first).

- **`dfs-server/src/server.rs` — `broadcast_metadata_to_followers`** (~line 6266). Zero
  callers (confirmed via grep). Flagged by the compiler as unused
  (`warning: methods ... broadcast_metadata_to_followers ... are never used`). Superseded by
  `enqueue_metadata_for_followers` (the durable per-follower queue path).

- **`dfs-server/src/server.rs` — `write_data_local_only`** (~line 5866) and **`read_data`**
  (~line 6001). Flagged by the compiler as unused, same warning block as
  `broadcast_metadata_to_followers` above.

- **`dfs-server/src/storage.rs` — `next_write_seq`** (~line 18). Compiler-flagged unused.

- **`dfs-server/src/storage.rs` — `read_chunk_cached_only`** (~line 208) and
  **`read_chunk_range_arc`** (~line 216). Compiler-flagged unused.

- **`dfs-server/src/stats.rs` — `NodeStatsSnapshot::active_connections` /
  `::max_connections`** (~line 48-49). Fields are written but never read anywhere.

- **`dfs-server/src/server.rs` — `CHUNK_SIZE_REBUILD`** constant inside
  `rebuild_chunk_map_from_metadata` (~line 2147). Compiler-flagged unused — looks like a
  leftover from a refactor (the scan it was meant to gate no longer needs it).

- **`dfs-server/src/server.rs` — `pull_metadata_from_leader`** (~line 2542). Not dead code
  exactly, but permanently broken: matches its RPC response against `Response::FileMetadata`,
  but the request it sends (`Request::GetFileInfoById`) is actually handled by
  `handle_get_file_info_by_id`, which returns `Response::FileInfo { metadata, chunk_locations }`
  — a variant this match never accounts for. Every call falls through to the `_ =>` arm and
  returns `Err("Unexpected response from leader")`. Pre-existing (not introduced by Phase 4),
  found in passing while auditing `chunk_map_update` call sites. This "self-heal when stale
  metadata is detected" path has likely never actually worked.

- **Leftover temporary instrumentation shipped to production.** `read_file`'s
  `TEMP DIAGNOSTIC (2026-07-14)` in `dfs-client/src/client.rs` was left at `info!` and fired
  on *every read* — demoted to `debug!` 2026-07-16. Others of the same shape are still in
  place and worth a sweep: `dfs-server/src/metadata.rs` `put_file_in_txn` carries
  `TEMP PROFILING (2026-07-07)` timing scaffolding (`t_merge_start`/`t_put_start` etc., whose
  own comment says "Remove once the bottleneck is characterized" — it has been), and there
  are `[SIZE TRACE]` / `[CHUNK_MAP dup]` diagnostic blocks in `server.rs` from past
  investigations. Grep for `TEMP DIAGNOSTIC`, `TEMP PROFILING`, and `DIAGNOSTIC (` to find
  them. Two things to check per site, not just log volume: (1) is it at `info!` when it
  should be `debug!`, and (2) does building the message do real work (a function call, a
  clone, an `.iter().find()`)? `tracing` skips field evaluation when the level is disabled,
  so demoting to `debug!` fixes both at once — but a diagnostic that's genuinely finished
  should just be deleted.

- **`dfs-admin/src/main.rs` — `file list` / `file find-chunk` Chunks column** (~lines
  1070/1120/1141). Always reports 0 chunks: both read `FileMetadata.chunk_locations`, which
  `handle_list_all_files` (server.rs) unconditionally strips to empty before responding — a
  pre-existing, unrelated-to-Phase-4 design choice (client startup warm-up only needs scalar
  fields; chunk locations are meant to be fetched lazily via `GetFileChunkMap`). `file info`
  and `file repack` were fixed this session (they now correctly read the separate
  `chunk_locations` field off `Response::FileInfo`); `list`/`find-chunk` were not, since they
  go through a different response type (`FileList`) that never carried per-file chunk info to
  begin with.

## Found 2026-07-18 (triple-node compaction-wedge fix)

- **`dfs-server/src/server.rs` — `nothing_to_reclaim` branch in `start_compaction_loop`**
  (~line 6825 onward, including the `should_skip_periodic_compaction_under_load` call site
  and its skip-log). `should_compact` no longer has a time-based fallback (removed same
  session — see its doc comment): every path that reaches this point now implies
  `current_size` is at least `COMPACT_MIN_RECLAIMABLE_BYTES` past `last_compact_size`, so
  `nothing_to_reclaim` (`current_size <= last_compact_size`) should be structurally
  unreachable except a same-tick race between the `current_size` read and a since-last-tick
  compaction elsewhere — not proven impossible, so left in place as a defensive check rather
  than deleted. `should_skip_periodic_compaction_under_load` itself and its unit tests
  (`periodic_compaction_load_gate` module) are still meaningful as a defense-in-depth guard
  should `nothing_to_reclaim` ever actually fire, so kept rather than removed too. Candidate
  for a follow-up pass: confirm via a live counter (add a debug! or metric if this branch
  ever actually executes) that it's truly dead, then delete it and the load-gate scaffolding
  together.
