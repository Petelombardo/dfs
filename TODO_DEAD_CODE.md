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

## Found 2026-07-22 (per-chunk write-buffer sharding refactor)

- **`dfs-client/src/fuse_impl.rs` — `FlushHandle::flush_buffer_async` and
  `DfsFilesystem::flush_buffer_async`/`flush_all_pipelined` thin wrapper** (~line 4671/4676
  area). Compiler-flagged unused (`flush_buffer_async`, `flush_all_pipelined`,
  `should_update_metadata`, `record_metadata_update`, `safe_metadata_update`,
  `get_or_create_inode` all in the same dead-code warning block). Pre-existing, not
  introduced by the sharding refactor — confirmed via grep that `flush_buffer_async` has
  exactly one caller (its own `DfsFilesystem` wrapper) and that wrapper itself has zero
  callers. Live flush paths are `flush_all_pipelined` (the `FlushHandle` one, called from
  release/fsync) and the background ticker calling `flush_one_chunk` directly — this
  whole `FlushHandle::flush_buffer_async` function (with its own internal PatchChunk logic,
  duplicating much of `flush_buffer_async_one`) looks superseded and never wired back in.
  Was carried forward faithfully during the sharding refactor since removing/behavior-
  changing dead code was out of scope for that task.

- **`dfs-client/src/fuse_impl.rs` — `InodeWriteBuffer::buffered_bytes`** (~line 741, added
  during the sharding refactor as a direct port of the pre-existing
  `InodeWriteState::buffered_bytes`). Confirmed pre-existing dead code, not new: the original
  had the same doc-comment note that `resident_bytes()` is what back-pressure actually uses
  since the 2026-07-15 sparse-extent rework made the two nearly equivalent. No call site uses
  `buffered_bytes()` in either the old or new design.

- **`dfs-client/src/fuse_impl.rs` — `InodeWriteBuffer::resident_bytes`** (~line 850). Became
  dead 2026-07-22 when write back-pressure stopped deriving "bytes this write added" from the
  delta of two `resident_bytes()` samples and `write_at()` started returning the exact
  per-slot growth instead. **Prefer deleting this one over reviving it.** It sums the WHOLE
  buffer and silently SKIPS any shard whose `try_lock` fails, so it is only ever an
  approximation — safe for a coarse occupancy gauge, actively wrong as a term in a
  difference. Using it in a subtraction is precisely what wedged the write buffer at its
  cap on server4/server5 (adds inflated by other chunks' bytes, `saturating_sub` clamping
  the opposite error to zero, so the counter could only ratchet up until every write paid
  back-pressure forever and only a client restart recovered). Left in place here rather than
  deleted inline to keep that fix reviewable on its own; if it stays, it needs a
  "never use this in a delta" warning on the function itself.

- **`dfs-client/src/fuse_impl.rs` — `InodeWriteBuffer::all_slot_indices`** (~line 891).
  Compiler-flagged unused alongside `buffered_bytes`/`resident_bytes`. Its doc comment still
  claims "Used by fsync/release", which is no longer true — worth confirming whether the
  fsync/release path lost a call it should still be making, or whether the comment is simply
  stale, before deleting.

## Found 2026-07-28 (VM-108 dangling-pointer incident, fold-generation + chunk_seq fixes)

- **`dfs-client/src/client.rs` — `broadcast_chunk_location`** (~line 5760). Zero callers
  (confirmed via grep across `dfs-client/src/*.rs`). Its own doc comment ("The leader gets
  reliable delivery with exponential-backoff retries... Followers get fire-and-forget") reads
  as though it's the live per-patch chunk-location notification path, and initially misled
  this investigation into thinking ordinary writes' leader-notification was a bounded,
  no-backstop, one-shot RPC (unlike folds' `pending_patch_fold_broadcasts`). It is not — the
  real live path is `pending_chunk_locations` + `enqueue_chunk_location` (called from both
  `PatchChunk` and `MultiPatch` response handling) drained by
  `start_chunk_location_batch_worker`, which retries every 10ms **forever, no TTL**, re-queuing
  on any failure rather than dropping — actually *more* durable than folds' 120s-TTL backstop,
  not less. Superseded by that mechanism; candidate for deletion, but its misleading doc
  comment is the more urgent problem if it's kept around — at minimum mark it
  `#[allow(dead_code)]` with a pointer to the real mechanism, or just delete it.
