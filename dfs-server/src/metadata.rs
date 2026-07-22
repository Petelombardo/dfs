use anyhow::{Context, Result};
use dfs_common::{ChunkId, ChunkLocation, FileId, FileMetadata, NodeId};
use redb::{Database, Durability, ReadableTable, ReadableTableMetadata, TableDefinition};
// On Linux, Durability::Eventual calls fdatasync (same as Immediate). Only the macOS
// backend (F_BARRIERFSYNC) distinguishes them. Durability::None writes to the OS page
// cache without fdatasync — fast, immediately visible to reads, survives process crashes,
// only lost on kernel panic/power failure. Acceptable with 5-way replication.
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
// Fair (queue-ordered) RwLock for MetadataStore.db specifically — see its field doc
// comment for why std::sync::RwLock's pthread_rwlock-backed writer-starvation risk
// under sustained heavy reader load matters here. Not used for Mutex above — those
// guard small, short-held, single-owner-at-a-time state where starvation isn't a
// concern the way a reader-vs-writer race on the whole database handle is.
use parking_lot::RwLock;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Table definitions — each replaces one sled prefix or named tree.
// Keys carry no prefix since the table name is already the namespace.
// ---------------------------------------------------------------------------

/// file_id (hyphenated UUID string) → bincode(FileMetadata)
const FILE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("file");

/// full path string → bincode(FileMetadata)  (path index, same data as file table)
const PATH_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("path");

/// chunk_id hex string → bincode(ChunkLocation)
const CHUNK_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("chunk");

/// "{node_hex}:{seq:016x}" → bincode(FileMetadata)  (leader dissemination queue)
const META_QUEUE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta_queue");

/// "{node_hex}:{file_id_hex}" → seq as 8-byte big-endian u64  (dedup index)
const META_QUEUE_IDX: TableDefinition<&str, &[u8]> = TableDefinition::new("meta_queue_idx");

/// "meta_seq" → u64,  "follower_seq" → u64
const COUNTERS_TABLE: TableDefinition<&str, u64> = TableDefinition::new("counters");

/// "del:{file_id}" → bincode(DeleteQueueEntry)
const DELETE_QUEUE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("delete_queue");

/// chunk_id hex string → unix seconds when first detected as needing healing.
/// Persists HealingManager's per-chunk debounce timer across restarts so a
/// process restart doesn't reset the healing_delay_secs clock for the whole
/// backlog at once.
const PENDING_HEALING_TABLE: TableDefinition<&str, u64> = TableDefinition::new("pending_healing");

/// chunk_id hex string → reference count (u64).
///
/// Only ever populated for chunk_ids produced by PatchChunk/MultiPatch
/// (file+offset-scoped hashes — see compute_chunk_hash_at). Those hashes bake
/// in file_id and file_offset, so they can never alias another (file,
/// chunk_idx) slot; a tracked count hitting zero means the chunk is provably
/// dead, not just "absent from the last scan". Chunk_ids from the original
/// chunker (plain compute_chunk_hash, no file scoping) are never inserted
/// here — they can legitimately be shared across files (dedup), so they're
/// left for the deep orphan-purge sweep to handle as before. Absence of an
/// entry means "untracked", not "zero" — callers must never delete on a
/// missing entry.
const CHUNK_REFCOUNT_TABLE: TableDefinition<&str, u64> = TableDefinition::new("chunk_refcount");

/// public_token hex string → bincode(PatchState).
///
/// Deferred chunk-patch consolidation ("delayed write"): a small patch becomes a
/// cheap standalone delta chunk on disk (hashed only over its own bytes, not the
/// full ~4MB chunk), and the client is handed back a PUBLIC TOKEN chunk_id — never
/// a real, independently-readable chunk on disk, only ever resolved through this
/// table. This is deliberate: the delta's own raw chunk_id and the pre-patch base's
/// chunk_id must never be mistaken for directly-readable, standalone content by
/// anything that doesn't know to check here first (the healer replicating to a new
/// node, a stale peer's chunk_map, etc.) — routing everything through an opaque
/// token that only resolves via this table makes that mistake structurally
/// impossible, rather than relying on every caller to remember a special case.
///
/// At most one row is ever outstanding per (file_id, chunk_idx) — see
/// PATCH_STATE_SLOT_TABLE. Lazily created on first write, same as
/// CHUNK_REFCOUNT_TABLE — no migration needed, not in the startup table-open list.
const PATCH_STATE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("patch_state");

/// "{file_id}:{chunk_idx}" → public_token hex string (the PATCH_STATE_TABLE key
/// currently outstanding for this slot).
///
/// At most one patch_state is ever outstanding per (file_id, chunk_idx) at a time:
/// when a *new* patch lands on the same slot, that's proof a writer already learned
/// the previous public_token's resolution (it had to, to base this new patch on it)
/// — so the previous row can be retired immediately. This table exists purely so a
/// new patch can find the previous row's key in O(1) (PATCH_STATE_TABLE itself is
/// keyed by the token, not by slot, since that's what the read path needs) instead
/// of a reverse scan. Lazily created on first write.
const PATCH_STATE_SLOT_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("patch_state_slot");

/// "{file_id}:{chunk_idx}" → last-applied client-assigned per-slot sequence number.
///
/// Deliberately separate from FileMetadata.write_seq (the existing per-*file*
/// counter used by put_file's Rule 1/Rule 2 merge logic) and from
/// ChunkLocation.client_write_seq (the same per-file value, just carried
/// alongside a chunk_id) — this branch's history is chunk_id-identity staleness
/// bugs (see [[project_chunk_patch_overlay_consolidation]] items 7 and 8), and
/// every one of them came from treating chunk_id, which encodes a whole
/// fold/merge chain that's only disseminated cross-node via best-effort
/// broadcasts, as the thing that gates whether a patch may apply. A per-slot
/// sequence is a much smaller, simpler claim: "have I applied every patch up to
/// N for this exact (file, chunk_idx)?" — a plain integer comparison, no
/// content-hash chain to resolve. Since this DFS has exactly one active writer
/// per file, the client can assign these values authoritatively.
///
/// Only the "gap" direction is safe to act on unilaterally: a new_chunk_seq more
/// than one ahead of what's recorded means this replica is missing prior patches
/// to this exact slot, so handle_multi_patch proactively refreshes from the
/// leader before applying — see that function's comment. The mirror-image idea —
/// treating a new_chunk_seq at or behind what's recorded as a safe-to-skip
/// duplicate — was tried and reverted the same day (2026-07-10) after it caused
/// real data loss in the T28 patch-storm test: chunk_patch_locks only serializes
/// *processing* order, not *arrival* order, so two concurrent patches to the same
/// slot touching different byte ranges can have a higher-seq one win the lock and
/// apply first, making a genuinely-not-yet-applied lower-seq patch look like a
/// stale duplicate and get silently dropped. Detecting real duplicates safely
/// needs more than a plain integer compare (e.g. content-based dedup, or the
/// client guaranteeing strict per-slot in-flight ordering) — not implemented.
///
/// Lazily created on first write, same as CHUNK_REFCOUNT_TABLE — no migration
/// needed, not in the startup table-open list.
const CHUNK_SEQ_TABLE: TableDefinition<&str, u64> = TableDefinition::new("chunk_seq");

// ---------------------------------------------------------------------------

/// Decode a 64-character lowercase hex string (as produced by `ChunkId::to_hex`)
/// back into 32 raw bytes. Returns `None` on malformed input.
fn decode_hex_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Increment the last byte of a prefix to produce its exclusive upper bound.
/// All prefix strings used here end with ASCII characters far below 0xFF.
fn prefix_next(prefix: &str) -> String {
    let mut bytes = prefix.as_bytes().to_vec();
    if let Some(last) = bytes.last_mut() {
        *last += 1;
    }
    String::from_utf8(bytes).expect("prefix bytes are ASCII")
}

// ---------------------------------------------------------------------------

/// Result of a put_file call — distinguishes accepted writes from stale drops.
pub enum PutFileResult {
    /// Write was accepted and stored.
    Stored,
    /// Write was dropped because this node already has a newer version.
    /// The existing (newer) record is returned so the caller can propagate
    /// it back to whoever sent the stale write, converging the cluster.
    Stale(FileMetadata),
}

/// What a PATCH_STATE_TABLE public token currently resolves to. See
/// PATCH_STATE_TABLE's doc comment for why the token itself is never a real
/// on-disk chunk_id — every read of it goes through one of these two arms.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum PatchState {
    /// The background fold hasn't completed (or run) yet: resolve by reading
    /// `base_chunk_id` and applying `delta_chunk_id`'s own (intra_offset, bytes)
    /// patches on top. `delta_chunk_id`'s file holds bincode(Vec<(usize, Vec<u8>)>)
    /// — the same shape MultiPatch/PatchChunk already carry.
    Pending {
        base_chunk_id: ChunkId,
        delta_chunk_id: ChunkId,
        size: usize,
        written_at: u64,
        client_write_seq: Option<u64>,
    },
    /// The fold completed: redirect straight to the real, standalone,
    /// content-addressed result.
    Folded(ChunkId),
}

/// A fully-converged shadow database, ready for compact_db_finish's exclusive-locked
/// atomic swap. Produced by compact_db_prepare (Phases 1-2, shared-lock only) and
/// consumed by compact_db_finish (Phase 3, exclusive lock) — see their doc comments
/// for why they're split into separate calls.
pub(crate) struct CompactionPrep {
    shadow_db: Database,
    shadow_path: PathBuf,
    size_before: u64,
}

/// One queued write for the group-commit committer thread — see
/// MetadataStore::committer_tx. Each variant carries exactly the arguments of the
/// single-record write function it replaces, plus a oneshot reply the committer
/// resolves once the batch containing this op has committed (or failed). The op
/// set is deliberately only the *hot single-record* paths: per-chunk-write and
/// per-heal record updates measured as the dominant transaction sites in the
/// 2026-07-15 DB-growth baselines (put/delete_chunk_location, put_chunk_seq, and
/// the patch-state trio each fired >1000 single-record commits/min under RND4K
/// load). Bulk paths (put_files_batch, batch_update_chunk_locations, meta queue)
/// already amortize their own transactions and stay on their direct path.
enum MetaWriteOp {
    PutChunkLocation {
        location: ChunkLocation,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    DeleteChunkLocation {
        chunk_id: ChunkId,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    PutChunkSeq {
        file_id: FileId,
        chunk_idx: u64,
        seq: u64,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    PutPatchStatePending {
        file_id: FileId,
        chunk_idx: u64,
        public_token: ChunkId,
        base_chunk_id: ChunkId,
        delta_chunk_id: ChunkId,
        size: usize,
        written_at: u64,
        client_write_seq: Option<u64>,
        reply: tokio::sync::oneshot::Sender<Result<Option<ChunkId>>>,
    },
    UpdatePatchStateFolded {
        public_token: ChunkId,
        new_chunk_id: ChunkId,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    DeletePatchStateAbandoned {
        public_token: ChunkId,
        file_id: FileId,
        chunk_idx: u64,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    PutPendingHealing {
        chunk_id: ChunkId,
        detected_at_secs: u64,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    DeletePendingHealing {
        chunk_id: ChunkId,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    /// Multi-record CHUNK_TABLE/PENDING_HEALING update (the batch_update /
    /// put_chunk_locations_batch async paths). These need grouping every bit as
    /// much as the single-record ops: the live write path calls
    /// put_chunk_locations_batch_async with a single-chunk "batch" per confirmed
    /// write, and queue_chunks_immediate does the same for pending-healing marks —
    /// measured 2026-07-16 (post-Phase-1 heal repro): +500 batch-of-1
    /// put_chunk_locations_batch txns and +504 batch-of-1 put_pending_healing_batch
    /// txns per minute on the leader during ingest, ballooning it to 56.5MB with
    /// 0.6MB live exactly like the single-record storms this file already fixed.
    UpdateChunkLocationsBatch {
        puts: Vec<ChunkLocation>,
        deletes: Vec<ChunkId>,
        pending_healing_deletes: Vec<ChunkId>,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
    PutPendingHealingBatch {
        entries: Vec<(ChunkId, u64)>,
        reply: tokio::sync::oneshot::Sender<Result<()>>,
    },
}

impl MetaWriteOp {
    /// Logical write count for durability accounting — a batch op carries as many
    /// writes as it has records, and next_write_durability_n counts writes (not
    /// commits, not queue items) so the durable-flush cadence is load-proportional.
    fn weight(&self) -> u64 {
        match self {
            MetaWriteOp::UpdateChunkLocationsBatch { puts, deletes, pending_healing_deletes, .. } => {
                (puts.len() + deletes.len() + pending_healing_deletes.len()).max(1) as u64
            }
            MetaWriteOp::PutPendingHealingBatch { entries, .. } => entries.len().max(1) as u64,
            _ => 1,
        }
    }
}

/// A reply the committer owes once the current batch's commit outcome is known —
/// the per-op apply result is held here until commit() succeeds (send it as-is)
/// or fails (replace tentative Ok with the commit error; a per-op apply error is
/// authoritative either way, that op wrote nothing).
enum PendingReply {
    Unit(tokio::sync::oneshot::Sender<Result<()>>, Result<()>),
    RetiredToken(tokio::sync::oneshot::Sender<Result<Option<ChunkId>>>, Result<Option<ChunkId>>),
}

/// Metadata storage using redb embedded database.
/// Replaces sled to eliminate the u8 fragment-count panic under heavy write loads.
pub struct MetadataStore {
    db: RwLock<Database>,
    db_path: PathBuf,
    /// Counts Durability::None commits since the last Durability::Immediate one.
    /// See next_write_durability() for why this exists.
    non_durable_commits: AtomicU64,
    /// FILE_TABLE keys (file_id strings) written or removed since the dirty set was
    /// last drained. Populated by every FILE_TABLE mutator (put_file, delete_file,
    /// remove_unlisted_files) immediately after a successful commit, while that
    /// mutator still holds `db.read()` — see compact_db_with_budget's doc comment on
    /// diff_all_tables_tracked for why that ordering makes this race-free against
    /// Phase 3's exclusive lock. Drained (not just read) at the start of each
    /// compaction catch-up pass so repeated passes only see genuinely new writes.
    dirty_files: Mutex<std::collections::HashSet<String>>,
    /// PATH_TABLE keys (path strings), same tracking discipline as dirty_files.
    dirty_paths: Mutex<std::collections::HashSet<String>>,
    /// Per-call-site write-transaction attribution: site (function name) -> (txn_count,
    /// payload_bytes). Populated by note_txn() immediately after every successful
    /// write-transaction commit in this file, to answer "which call site is actually
    /// responsible for DB growth" (see the Phase 0 attribution work this exists for).
    /// Never reset by production code — server.rs's periodic [META TXN] logger keeps
    /// its own previous snapshot and computes deltas itself. Same Mutex-guarded-small-
    /// state discipline as dirty_files/dirty_paths above.
    txn_stats: Mutex<std::collections::HashMap<&'static str, (u64, u64)>>,
    /// Group-commit queue for the hot single-record write paths — lazily started
    /// on first use (needs an Arc<Self>, which new() doesn't have). See
    /// committer_tx()/commit_worker_loop() for the design and the invariant that
    /// nothing may submit an op and wait on its reply while holding db.write().
    committer: OnceLock<tokio::sync::mpsc::Sender<MetaWriteOp>>,
    /// Accumulated group-commit queue-depth/commit-duration stats since the last
    /// [META COMMITTER] log line — see CommitterStats and commit_worker_loop.
    /// Added 2026-07-19 while chasing a write-latency trend (client-observed
    /// per-write latency climbing 2-3x over a single sustained-write session)
    /// that couldn't be explained by any in-memory structure's size (chunk_map,
    /// pending_patch_ids etc. are all O(1) DashMap point access; PATCH_STATE
    /// tables are O(log n) redb B-tree ops) — the one place left that could show
    /// genuine queueing-delay growth under sustained concurrent load (this
    /// install + healing + fold completions all funnel through one committer
    /// thread) had no visibility at all. This makes that visible directly instead
    /// of inferring it from client-side write timing.
    committer_stats: Mutex<CommitterStats>,
}

/// See MetadataStore::committer_stats' doc comment. Reset to defaults every time
/// commit_worker_loop flushes a [META COMMITTER] log line (period-based, not
/// count-based, so the log stays useful under both light and heavy load).
#[derive(Default)]
struct CommitterStats {
    batches: u64,
    ops: u64,
    max_queue_depth: usize,
    total_commit_ms: f64,
    max_commit_ms: f64,
    last_log: Option<std::time::Instant>,
}

impl MetadataStore {
    /// Create a new metadata store (creates the redb file if it does not exist).
    pub fn new(metadata_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&metadata_dir)
            .with_context(|| format!("Failed to create metadata dir {:?}", metadata_dir))?;

        let db_path = metadata_dir.join("metadata.redb");

        // Discard any leftover shadow file from a compact_db() that crashed mid-flight
        // (see compact_db()). Always safe: the live db_path file is never touched until
        // the final atomic rename, so a stale shadow file never holds anything live.
        let shadow_path = db_path.with_extension("redb.shadow");
        if shadow_path.exists() {
            warn!("Discarding stale compaction shadow file from prior crash: {:?}", shadow_path);
            let _ = std::fs::remove_file(&shadow_path);
        }

        // Cap redb's page cache to prevent OOM on low-RAM nodes (default is 1GB).
        // 256MB is plenty for our working set; the rest stays on disk.
        //
        // Retry briefly on DatabaseAlreadyOpen: a just-killed previous instance's
        // graceful-shutdown flush can still hold the file lock for a moment after
        // the process exits, which would otherwise turn a fast restart (SIGTERM
        // immediately followed by respawn) into a permanent crash loop.
        let mut db = {
            let mut attempt = 0;
            loop {
                match Database::builder()
                    .set_cache_size(256 * 1024 * 1024)
                    .create(&db_path)
                {
                    Ok(db) => break db,
                    Err(redb::DatabaseError::DatabaseAlreadyOpen) if attempt < 10 => {
                        attempt += 1;
                        warn!("redb at {:?} still locked by previous instance, retrying ({}/10)...", db_path, attempt);
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                    Err(e) => {
                        return Err(e).with_context(|| format!("Failed to open redb at {:?}", db_path));
                    }
                }
            }
        };

        // Check structural integrity on every startup. redb's check_integrity()
        // validates B-tree page allocations and repairs any inconsistency left by
        // an unclean shutdown (power loss, kill -9, OOM). Returns true if the DB
        // was already clean, false if repair was needed. An unrecoverable error
        // here means the file is truly corrupt — surface it so the admin can
        // restore from a peer rather than silently serving bad data.
        match db.check_integrity() {
            Ok(true)  => info!("redb integrity check passed (clean)"),
            Ok(false) => warn!("redb integrity check: repaired inconsistency — DB was unclean at last shutdown"),
            Err(e)    => {
                // Log but don't abort — a corrupt but partially-readable database
                // is better than a crash loop. The error is visible in journalctl.
                warn!("redb integrity check FAILED (possible corruption): {}", e);
            }
        }

        // Compact on every startup to reclaim dead pages left by previous write
        // sessions. Under heavy MultiPatch load (VM disk patching) each chunk-ID
        // rotation writes a new page and marks the old one free — without compaction
        // the file grows unboundedly. We have exclusive access here (before any Arc
        // wrapping) so &mut is safe and no Mutex is needed.
        let size_before = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
        match db.compact() {
            Ok(true)  => {
                let size_after = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
                info!("redb compacted: {:.1}MB → {:.1}MB",
                    size_before as f64 / 1_048_576.0,
                    size_after  as f64 / 1_048_576.0);
            }
            Ok(false) => info!("redb compact: nothing to reclaim ({:.1}MB)",
                size_before as f64 / 1_048_576.0),
            Err(e)    => warn!("redb compact failed (non-fatal): {}", e),
        }

        // Ensure all tables exist on first open.
        {
            let txn = db.begin_write()?;
            txn.open_table(FILE_TABLE)?;
            txn.open_table(PATH_TABLE)?;
            txn.open_table(CHUNK_TABLE)?;
            txn.open_table(META_QUEUE_TABLE)?;
            txn.open_table(META_QUEUE_IDX)?;
            txn.open_table(COUNTERS_TABLE)?;
            txn.open_table(DELETE_QUEUE_TABLE)?;
            txn.open_table(PENDING_HEALING_TABLE)?;
            txn.commit()?;
        }

        info!("Initialized redb metadata store at {:?}", db_path);

        Ok(Self {
            db: RwLock::new(db),
            db_path,
            non_durable_commits: AtomicU64::new(0),
            dirty_files: Mutex::new(std::collections::HashSet::new()),
            dirty_paths: Mutex::new(std::collections::HashSet::new()),
            txn_stats: Mutex::new(std::collections::HashMap::new()),
            committer: OnceLock::new(),
            committer_stats: Mutex::new(CommitterStats::default()),
        })
    }

    /// Record one committed write transaction attributed to `site` (the function
    /// name — pass a string literal so it's `&'static str`, no allocation per call).
    /// `payload_bytes` is the serialized value size actually written where that's
    /// cheaply available (already computed for the insert), 0 otherwise (pure
    /// deletes, counter bumps, or multi-row txns where tracking an exact sum isn't
    /// worth the bookkeeping). Call immediately after a successful `txn.commit()?` —
    /// never before, so a failed commit is never counted as growth. Cheap: one
    /// Mutex lock + HashMap entry bump, no behavior change to the write path.
    fn note_txn(&self, site: &'static str, payload_bytes: usize) {
        let mut stats = self.txn_stats.lock().unwrap();
        let entry = stats.entry(site).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += payload_bytes as u64;
    }

    /// Snapshot of (site, txn_count, payload_bytes) for every site note_txn has
    /// recorded so far, sorted by txn_count descending. Does NOT reset the
    /// underlying counters — callers (the [META TXN] periodic logger in server.rs)
    /// keep their own previous snapshot and compute deltas themselves, so this can
    /// be called concurrently (e.g. from an admin RPC) without perturbing anything.
    pub fn txn_stats_snapshot(&self) -> Vec<(String, u64, u64)> {
        let stats = self.txn_stats.lock().unwrap();
        let mut out: Vec<(String, u64, u64)> = stats.iter()
            .map(|(site, (count, bytes))| (site.to_string(), *count, *bytes))
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1));
        out
    }

    /// Upper bound on ops folded into one group-commit transaction. Bounds the
    /// redb write-lock hold per batch (an unbounded batch under sustained load
    /// would hold the single writer slot indefinitely — same failure shape as the
    /// unbounded Phase 1-2 compaction that hung a restart on 2026-07-11).
    const GROUP_COMMIT_MAX_OPS: usize = 256;

    /// Queue capacity for the group committer. Full queue = callers await in
    /// send() — natural backpressure, bounded memory.
    const GROUP_COMMIT_QUEUE: usize = 4096;

    /// Emergency/bisection kill switch: DFS_DISABLE_GROUP_COMMIT=1 makes every
    /// *_async wrapper fall back to its pre-2026-07-15 spawn_blocking
    /// one-transaction-per-call path instead of the group committer. Read once —
    /// flipping it requires a restart, deliberately (a mid-flight mix of both
    /// paths is exactly the kind of ordering ambiguity this switch exists to
    /// rule out when debugging).
    fn group_commit_enabled() -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| {
            let disabled = std::env::var_os("DFS_DISABLE_GROUP_COMMIT").is_some();
            if disabled {
                warn!("group commit DISABLED via DFS_DISABLE_GROUP_COMMIT — using one transaction per write op");
            }
            !disabled
        })
    }

    /// Get (lazily starting) the group-commit queue.
    ///
    /// Design: all hot single-record write paths (see MetaWriteOp) submit ops to
    /// one dedicated committer thread, which drains whatever is queued (up to
    /// GROUP_COMMIT_MAX_OPS) and applies it in ONE redb write transaction, then
    /// resolves every op's oneshot with its result. Callers therefore keep
    /// exactly the semantics the old spawn_blocking wrappers had — the await
    /// returns only after the write is committed (read-your-writes preserved, no
    /// write-behind visibility window, durability cadence preserved via
    /// next_write_durability_n) — but a burst of concurrent single-record writes
    /// now costs ~1 transaction per batch instead of 1 per record. Under light
    /// load a batch naturally contains a single op, identical to the old path;
    /// batching kicks in exactly when contention does (classic group commit).
    /// Measured motivation: RND4K baseline 2026-07-15 showed ~5,800 single-record
    /// commits/min/node from these paths ballooning the DB file 10-50x its live
    /// content.
    ///
    /// INVARIANT: nothing may submit an op and block on its reply while holding
    /// db.write() (the compaction Phase-3 exclusive lock) — the committer needs
    /// db.read() to make progress, so that would deadlock. Compaction and every
    /// other exclusive-lock holder use direct transactions, never these wrappers.
    fn committer_tx(self: &Arc<Self>) -> tokio::sync::mpsc::Sender<MetaWriteOp> {
        self.committer.get_or_init(|| {
            let (tx, rx) = tokio::sync::mpsc::channel(Self::GROUP_COMMIT_QUEUE);
            let weak = Arc::downgrade(self);
            std::thread::Builder::new()
                .name("meta-committer".into())
                .spawn(move || Self::commit_worker_loop(weak, rx))
                .expect("failed to spawn metadata group-commit thread");
            tx
        }).clone()
    }

    /// Committer thread body. Holds only a Weak ref between batches so dropping
    /// the store (tests) tears everything down: senders drop with the store →
    /// blocking_recv returns None → thread exits.
    fn commit_worker_loop(store: Weak<MetadataStore>, mut rx: tokio::sync::mpsc::Receiver<MetaWriteOp>) {
        loop {
            let first = match rx.blocking_recv() {
                Some(op) => op,
                None => return, // all senders gone — store dropped
            };
            let mut ops = vec![first];
            while ops.len() < Self::GROUP_COMMIT_MAX_OPS {
                match rx.try_recv() {
                    Ok(op) => ops.push(op),
                    Err(_) => break,
                }
            }
            // Depth of what's still queued behind this batch, captured before
            // apply_ops_group runs — see committer_stats' doc comment for why.
            let queue_depth = rx.len();
            let batch_size = ops.len();
            let Some(store) = store.upgrade() else { return };
            let commit_start = std::time::Instant::now();
            store.apply_ops_group(ops);
            let commit_ms = commit_start.elapsed().as_secs_f64() * 1000.0;
            store.record_committer_stats(batch_size, queue_depth, commit_ms);
        }
    }

    /// Accumulate one batch's stats and flush a [META COMMITTER] summary line
    /// every ~5s (period-based, not per-batch — a busy committer can process
    /// hundreds of batches/sec, and per-batch logging is exactly the "log file
    /// grew to 1GB in 3 hours" mistake this project already made once with
    /// per-op PUT logging). See committer_stats' doc comment for why this exists.
    fn record_committer_stats(&self, batch_size: usize, queue_depth: usize, commit_ms: f64) {
        const LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
        let mut stats = self.committer_stats.lock().unwrap();
        stats.batches += 1;
        stats.ops += batch_size as u64;
        stats.max_queue_depth = stats.max_queue_depth.max(queue_depth);
        stats.total_commit_ms += commit_ms;
        stats.max_commit_ms = stats.max_commit_ms.max(commit_ms);

        let now = std::time::Instant::now();
        let last = *stats.last_log.get_or_insert(now);
        if now.duration_since(last) >= LOG_INTERVAL {
            let avg_batch = stats.ops as f64 / stats.batches as f64;
            let avg_commit_ms = stats.total_commit_ms / stats.batches as f64;
            info!(
                "[META COMMITTER] batches={} ops={} avg_batch_size={:.1} max_queue_depth={} avg_commit_ms={:.2} max_commit_ms={:.2}",
                stats.batches, stats.ops, avg_batch, stats.max_queue_depth, avg_commit_ms, stats.max_commit_ms,
            );
            *stats = CommitterStats { last_log: Some(now), ..Default::default() };
        }
    }

    /// Apply one batch of queued write ops in a single transaction and resolve
    /// every op's reply. A per-op apply error (serialization, table open) fails
    /// only that op; a commit error fails every op whose apply had succeeded —
    /// the same all-or-nothing outcome each op's own single-record transaction
    /// would have reported for itself.
    fn apply_ops_group(&self, ops: Vec<MetaWriteOp>) {
        let op_count: u64 = ops.iter().map(|op| op.weight()).sum();

        // Any failure to even open the transaction fails the whole batch.
        let _db = self.db.read();
        let mut txn = match _db.begin_write() {
            Ok(txn) => txn,
            Err(e) => {
                let msg = format!("group commit begin_write failed: {}", e);
                warn!("{}", msg);
                for op in ops {
                    Self::fail_op(op, &msg);
                }
                return;
            }
        };
        txn.set_durability(self.next_write_durability_n(op_count));

        let mut replies: Vec<PendingReply> = Vec::with_capacity(ops.len());
        let mut payload_bytes: usize = 0;
        for op in ops {
            match op {
                MetaWriteOp::PutChunkLocation { location, reply } => {
                    let result = (|| -> Result<()> {
                        let key = format!("{}", location.chunk_id);
                        let value = bincode::serialize(&location)
                            .context("Failed to serialize chunk location")?;
                        payload_bytes += value.len();
                        let mut table = txn.open_table(CHUNK_TABLE)?;
                        table.insert(key.as_str(), value.as_slice())?;
                        Ok(())
                    })();
                    self.note_txn("op:put_chunk_location", 0);
                    replies.push(PendingReply::Unit(reply, result));
                }
                MetaWriteOp::DeleteChunkLocation { chunk_id, reply } => {
                    let result = (|| -> Result<()> {
                        let key = format!("{}", chunk_id);
                        let mut table = txn.open_table(CHUNK_TABLE)?;
                        table.remove(key.as_str())?;
                        Ok(())
                    })();
                    self.note_txn("op:delete_chunk_location", 0);
                    replies.push(PendingReply::Unit(reply, result));
                }
                MetaWriteOp::PutChunkSeq { file_id, chunk_idx, seq, reply } => {
                    let result = (|| -> Result<()> {
                        let key = format!("{}:{}", file_id, chunk_idx);
                        let mut table = txn.open_table(CHUNK_SEQ_TABLE)?;
                        // Monotonic (L3, 2026-07-19): never roll the per-slot generation
                        // backward. A stale/out-of-order push must not lower chunk_seq —
                        // that reopens the "gap" that made the server treat a current chunk
                        // as old and let a ghost win (VM-111 install EIO). Guards the
                        // METADATA counter only; drops no patch data (a seq-based data drop
                        // caused the T28 regression — see handle_multi_patch).
                        let current = table.get(key.as_str())?.map(|v| v.value()).unwrap_or(0);
                        if seq > current {
                            table.insert(key.as_str(), seq)?;
                        }
                        Ok(())
                    })();
                    self.note_txn("op:put_chunk_seq", 0);
                    replies.push(PendingReply::Unit(reply, result));
                }
                MetaWriteOp::PutPatchStatePending {
                    file_id, chunk_idx, public_token, base_chunk_id, delta_chunk_id,
                    size, written_at, client_write_seq, reply,
                } => {
                    let result = Self::put_patch_state_pending_in_txn(
                        &txn, file_id, chunk_idx, &public_token, base_chunk_id,
                        delta_chunk_id, size, written_at, client_write_seq,
                    ).map(|(retired, bytes)| {
                        payload_bytes += bytes;
                        retired
                    });
                    self.note_txn("op:put_patch_state_pending", 0);
                    replies.push(PendingReply::RetiredToken(reply, result));
                }
                MetaWriteOp::UpdatePatchStateFolded { public_token, new_chunk_id, reply } => {
                    let result = (|| -> Result<()> {
                        let token_key = format!("{}", public_token);
                        let value = bincode::serialize(&PatchState::Folded(new_chunk_id))
                            .context("Failed to serialize patch state")?;
                        payload_bytes += value.len();
                        let mut table = txn.open_table(PATCH_STATE_TABLE)?;
                        table.insert(token_key.as_str(), value.as_slice())?;
                        Ok(())
                    })();
                    self.note_txn("op:update_patch_state_folded", 0);
                    replies.push(PendingReply::Unit(reply, result));
                }
                MetaWriteOp::DeletePatchStateAbandoned { public_token, file_id, chunk_idx, reply } => {
                    let result = Self::delete_patch_state_abandoned_in_txn(&txn, &public_token, file_id, chunk_idx);
                    self.note_txn("op:delete_patch_state_abandoned", 0);
                    replies.push(PendingReply::Unit(reply, result));
                }
                MetaWriteOp::PutPendingHealing { chunk_id, detected_at_secs, reply } => {
                    let result = (|| -> Result<()> {
                        let key = format!("{}", chunk_id);
                        let mut table = txn.open_table(PENDING_HEALING_TABLE)?;
                        table.insert(key.as_str(), detected_at_secs)?;
                        Ok(())
                    })();
                    self.note_txn("op:put_pending_healing", 0);
                    replies.push(PendingReply::Unit(reply, result));
                }
                MetaWriteOp::DeletePendingHealing { chunk_id, reply } => {
                    let result = (|| -> Result<()> {
                        let key = format!("{}", chunk_id);
                        let mut table = txn.open_table(PENDING_HEALING_TABLE)?;
                        table.remove(key.as_str())?;
                        Ok(())
                    })();
                    self.note_txn("op:delete_pending_healing", 0);
                    replies.push(PendingReply::Unit(reply, result));
                }
                MetaWriteOp::UpdateChunkLocationsBatch { puts, deletes, pending_healing_deletes, reply } => {
                    let result = Self::apply_chunk_location_updates_in_txn(
                        &txn, &puts, &deletes, &pending_healing_deletes,
                    ).map(|bytes| { payload_bytes += bytes; });
                    self.note_txn("op:update_chunk_locations_batch", 0);
                    replies.push(PendingReply::Unit(reply, result));
                }
                MetaWriteOp::PutPendingHealingBatch { entries, reply } => {
                    let result = Self::put_pending_healing_batch_in_txn(&txn, &entries);
                    self.note_txn("op:put_pending_healing_batch", 0);
                    replies.push(PendingReply::Unit(reply, result));
                }
            }
        }

        let commit_error: Option<String> = match txn.commit() {
            Ok(()) => {
                self.note_txn("group_commit", payload_bytes);
                None
            }
            Err(e) => Some(format!("group commit of {} ops failed: {}", op_count, e)),
        };
        if let Some(msg) = &commit_error {
            warn!("{}", msg);
        }

        for pending in replies {
            match pending {
                PendingReply::Unit(reply, result) => {
                    let final_result = match (&commit_error, result) {
                        // Apply errors are authoritative — that op wrote nothing either way.
                        (_, Err(e)) => Err(e),
                        (Some(msg), Ok(())) => Err(anyhow::anyhow!("{}", msg)),
                        (None, Ok(())) => Ok(()),
                    };
                    let _ = reply.send(final_result);
                }
                PendingReply::RetiredToken(reply, result) => {
                    let final_result = match (&commit_error, result) {
                        (_, Err(e)) => Err(e),
                        (Some(msg), Ok(_)) => Err(anyhow::anyhow!("{}", msg)),
                        (None, Ok(retired)) => Ok(retired),
                    };
                    let _ = reply.send(final_result);
                }
            }
        }
    }

    /// Resolve an op's reply with an error without applying it (used when the
    /// batch's transaction couldn't even be opened).
    fn fail_op(op: MetaWriteOp, msg: &str) {
        match op {
            MetaWriteOp::PutChunkLocation { reply, .. }
            | MetaWriteOp::DeleteChunkLocation { reply, .. }
            | MetaWriteOp::PutChunkSeq { reply, .. }
            | MetaWriteOp::UpdatePatchStateFolded { reply, .. }
            | MetaWriteOp::DeletePatchStateAbandoned { reply, .. }
            | MetaWriteOp::PutPendingHealing { reply, .. }
            | MetaWriteOp::DeletePendingHealing { reply, .. }
            | MetaWriteOp::UpdateChunkLocationsBatch { reply, .. }
            | MetaWriteOp::PutPendingHealingBatch { reply, .. } => {
                let _ = reply.send(Err(anyhow::anyhow!("{}", msg)));
            }
            MetaWriteOp::PutPatchStatePending { reply, .. } => {
                let _ = reply.send(Err(anyhow::anyhow!("{}", msg)));
            }
        }
    }

    /// Every Nth Durability::None write is instead committed with
    /// Durability::Immediate, which as a side effect drains redb's
    /// pending_non_durable_commits list (see compact_db()'s pre-flush comment).
    ///
    /// Without this, a sustained burst of insert+delete churn on the same table
    /// (e.g. MultiPatch chunk-ID rotation while qemu-img rewrites a qcow2 image's
    /// L2/refcount tables) lets that list grow unboundedly: the file balloons far
    /// beyond its live-data size, and compact()'s cost scales with the churn
    /// history rather than live data — turning a "should be milliseconds"
    /// compaction into one that can run for tens of minutes while holding
    /// the exclusive metadata write lock, freezing the whole node. A periodic
    /// durable flush (measured: every 200 commits) keeps the file size and
    /// compact() time proportional to live data instead.
    const DURABILITY_FLUSH_INTERVAL: u64 = 200;

    fn next_write_durability(&self) -> Durability {
        self.next_write_durability_n(1)
    }

    /// next_write_durability for a transaction that carries `ops` logical writes
    /// (a group commit). Counting ops instead of commits keeps the durable-flush
    /// cadence — and therefore both the crash-loss window and the
    /// pending_non_durable_commits drain frequency — expressed in the same unit
    /// the single-op path always used. Counting group commits as 1 would stretch
    /// "every 200" to "every 200 batches" (up to ~256x more unflushed writes).
    fn next_write_durability_n(&self, ops: u64) -> Durability {
        let start = self.non_durable_commits.fetch_add(ops, Ordering::Relaxed);
        let end = start + ops;
        if start / Self::DURABILITY_FLUSH_INTERVAL != end / Self::DURABILITY_FLUSH_INTERVAL {
            Durability::Immediate
        } else {
            Durability::None
        }
    }

    // -------------------------------------------------------------------------
    // Startup repair (retained as a sanity check; crash window no longer exists
    // because put_file now writes file: and path: atomically in one transaction).
    // -------------------------------------------------------------------------

    /// Scan all file records and re-create any missing path index entries.
    pub fn repair_path_index(&self) -> Result<()> {
        // Pass 1: find file records whose path index entry is missing.
        // (path, file_id_str) — see resolve_path_entry_file_id for why the
        // repaired entry is just the id, not the full blob.
        let to_repair: Vec<(String, String)> = {
            let _db = self.db.read();
            let txn = _db.begin_read()?;
            let file_table = txn.open_table(FILE_TABLE)?;
            let path_table = txn.open_table(PATH_TABLE)?;
            let mut repairs = Vec::new();
            for item in file_table.range::<&str>(..)? {
                let (k, v) = item?;
                let m = match dfs_common::deserialize_file_metadata(v.value()) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if path_table.get(m.path.as_str())?.is_none() {
                    repairs.push((m.path.clone(), k.value().to_string()));
                }
            }
            repairs
        };

        let repaired = to_repair.len();
        if !to_repair.is_empty() {
            let _db = self.db.read();
            let mut txn = _db.begin_write()?;
            txn.set_durability(self.next_write_durability());
            {
                let mut path_table = txn.open_table(PATH_TABLE)?;
                for (path, file_id_str) in &to_repair {
                    warn!("Repaired missing path index for: {}", path);
                    path_table.insert(path.as_str(), file_id_str.as_bytes())?;
                }
            }
            txn.commit()?;
            let payload_bytes: usize = to_repair.iter().map(|(_, id)| id.len()).sum();
            self.note_txn("repair_path_index", payload_bytes);
        }

        // Pass 2: find path index entries whose file record no longer exists.
        let stale_paths: Vec<String> = {
            let _db = self.db.read();
            let txn = _db.begin_read()?;
            let file_table = txn.open_table(FILE_TABLE)?;
            let path_table = txn.open_table(PATH_TABLE)?;
            let mut stale = Vec::new();
            for item in path_table.range::<&str>(..)? {
                let (k, v) = item?;
                if let Ok(file_id) = Self::resolve_path_entry_file_id(v.value()) {
                    let fid_str = format!("{}", file_id);
                    if file_table.get(fid_str.as_str())?.is_none() {
                        stale.push(k.value().to_string());
                    }
                }
            }
            stale
        };

        let stale_count = stale_paths.len();
        if !stale_paths.is_empty() {
            let _db = self.db.read();
            let mut txn = _db.begin_write()?;
            txn.set_durability(self.next_write_durability());
            {
                let mut path_table = txn.open_table(PATH_TABLE)?;
                for path in &stale_paths {
                    warn!("Removing stale path index entry: {}", path);
                    path_table.remove(path.as_str())?;
                }
            }
            txn.commit()?;
            self.note_txn("repair_path_index", 0);
        }

        if repaired > 0 || stale_count > 0 {
            info!(
                "Path index repair: {} entries rebuilt, {} stale entries removed",
                repaired, stale_count
            );
        }
        Ok(())
    }

    // -------------------------------------------------------------------------
    // File metadata — core CRUD
    // -------------------------------------------------------------------------

    /// Core of put_file, parameterized over already-open table handles so callers
    /// can share ONE transaction across many records (see put_files_batch) instead
    /// of paying a full commit per record.
    ///
    /// A "stale" write (existing.write_seq > incoming.write_seq) does NOT mean
    /// "ignore incoming entirely" — incoming's chunk_locations are still unioned
    /// into existing's before storing, only incoming's scalar fields (size, mtime,
    /// write_seq, ...) are discarded in favor of existing's. This always writes to
    /// file_table/path_table, stale or not; the caller must always commit and mark
    /// the result dirty (see put_file/put_files_batch). This is deliberate: under
    /// the per-cycle-delta push model (background-tick sends only chunks touched
    /// since the last push, not the full cumulative history — see fuse_impl.rs),
    /// two pushes for the same file can arrive at the leader out of write_seq
    /// order while each still carrying a genuinely new, disjoint chunk. Treating
    /// "stale" as "discard the whole payload" (the old behavior) silently dropped
    /// that chunk forever. Real repro: T48's background-tick sustained-write test
    /// losing exactly one chunk under full-suite timing pressure, even after the
    /// dissemination/queue-coalescing/existing-base-union fixes elsewhere in this
    /// file — this was the fourth and deepest layer of the same underlying issue.
    ///
    /// Returns the stored/stale result (the persisted, merged record — not the raw
    /// pre-merge existing) plus any old file_id removed due to a path collision —
    /// the caller must mark both dirty after commit (see put_file's dirty-marking
    /// comment).
    /// Pure per-write reconciliation: merges `incoming` against an optional
    /// `existing` record with no I/O. Contains every rule `put_file_in_txn` used
    /// to apply inline (staleness check, chunk_locations union, scalar-field
    /// selection) — extracted so the batch-fold step in the sled_write_tx worker
    /// (see server.rs) can fold several pending writes for the same file_id
    /// in-memory, one call per fold step, using the exact same logic as the
    /// redb-backed path instead of a second hand-written copy. Two independently
    /// maintained copies of this merge is exactly how the historical four-layer
    /// chunk_locations drop bug arose — do not duplicate this logic elsewhere.
    ///
    /// Returns (metadata_to_store, is_stale). is_stale means incoming's *scalar*
    /// fields (size, mtime, write_seq, ...) lost to existing's — but
    /// chunk_locations is always the union of both regardless; see below for why
    /// (T48 background-tick chunk-drop history).
    pub(crate) fn merge_file_metadata(existing: Option<&FileMetadata>, incoming: &FileMetadata) -> (FileMetadata, bool) {
        // If existing is strictly newer, incoming's *scalar* fields (size, mtime,
        // write_seq, ...) are stale and must not overwrite existing's — but incoming's
        // chunk_locations still need to be unioned in below. Under the per-cycle-delta
        // push model (background-tick sends only chunks touched since the last push,
        // not the full cumulative history), a push can arrive out of order with a
        // LOWER write_seq while still carrying a chunk offset `existing` has never
        // seen — e.g. two background-tick pushes for the same file racing on the
        // network, or a queue-coalesce (MetadataQueue::push_inner) resolving in
        // seq order but delivery completing out of order. Previously this branch
        // returned immediately without merging, silently discarding that chunk —
        // real repro: T48 background-tick sustained-write test losing exactly one
        // chunk (7/8 persisted) under full-suite timing pressure, even after the
        // dissemination/queue/put_file union fixes above — root-caused to this
        // early bail-out never running the merge at all.
        let is_stale = existing.is_some_and(|existing| {
            existing.write_seq > 0 && incoming.write_seq > 0 && existing.write_seq > incoming.write_seq
        });

        let Some(existing) = existing else {
            return (incoming.clone(), false);
        };

        if is_stale {
            debug!(
                "Merging (not dropping) stale-scalar metadata for {} (existing write_seq={} > incoming={}) — chunk_locations still unioned",
                incoming.path, existing.write_seq, incoming.write_seq
            );
        }

        // Merge chunk_locations as a TRUE UNION, starting from EXISTING (the
        // persisted record) rather than the incoming payload. This matters
        // because incoming may be only a partial, non-cumulative delta (routine
        // writes now send just the chunks touched this cycle, not the full
        // history — see fuse_impl.rs's flush_buffer_async). Starting from
        // incoming (the old approach) silently dropped any chunk recorded in
        // existing but absent from incoming — safe only as long as some upstream
        // step (e.g. handle_put_file_metadata's chunk_map reconcile) had already
        // expanded incoming to a full list, which isn't guaranteed under
        // concurrent/rapid pushes where chunk_map itself can lag behind the
        // delta currently arriving. Starting from existing means anything not
        // mentioned by incoming simply survives untouched, regardless of how
        // partial incoming is or how stale chunk_map was upstream.
        //
        // Per-entry reconciliation rules are unchanged from before: Rule 1
        // (same chunk_id already present: union node lists, keep incoming's
        // other fields) and Rule 2 (different chunk_id at the same offset: keep
        // whichever is newer by client_write_seq/written_at, falling back to
        // file-level write_seq). Matched by chunk_id first, then by offset —
        // same priority as before, since a chunk_id is a content hash of
        // (file_id, offset, data) and in practice never appears at two
        // different offsets for the same file.
        let mut merged_locs: Vec<ChunkLocation> = existing.chunk_locations.as_ref().clone();
        let mut id_index: std::collections::HashMap<ChunkId, usize> = merged_locs.iter()
            .enumerate().map(|(i, l)| (l.chunk_id, i)).collect();
        let mut offset_index: std::collections::HashMap<u64, usize> = merged_locs.iter()
            .enumerate().filter_map(|(i, l)| l.file_offset.map(|o| (o, i))).collect();

        for incoming_loc in incoming.chunk_locations.iter() {
            // A file_offset:None entry carries no reliable position in the current
            // chunk_idx-keyed file model — every real write path sets file_offset
            // (see this loop's own doc comment above and update_chunk_map_window's
            // matching guard on the read side, which already treats a None-offset
            // entry as "stale/orphaned... merging it anywhere is strictly worse
            // than leaving that slot alone"). Applying that same rule here too is
            // the fix: without it, Rule 1 below (same chunk_id already present)
            // blindly took incoming's fields — including a missing file_offset —
            // clobbering a perfectly valid, correctly-positioned entry and losing
            // its coordinate. Once merged in, that corrupted entry then persists
            // and self-propagates through every subsequent merge and metadata
            // fetch — root-caused live via T48/T22's intermittent chunk-count and
            // patched-region corruption under full-suite concurrent load. Drop it
            // instead of merging it in, symmetric with the read side.
            if incoming_loc.file_offset.is_none() {
                continue;
            }
            if let Some(&idx) = id_index.get(&incoming_loc.chunk_id) {
                // Rule 1: same chunk_id already present — union node lists,
                // otherwise take incoming's fields (mirrors the pre-union
                // behavior of enriching a copy of incoming with existing's nodes).
                let mut new_entry = incoming_loc.clone();
                for node in &merged_locs[idx].nodes {
                    if !new_entry.nodes.contains(node) {
                        new_entry.nodes.push(*node);
                    }
                }
                if let Some(old_offset) = merged_locs[idx].file_offset {
                    if Some(old_offset) != new_entry.file_offset {
                        offset_index.remove(&old_offset);
                    }
                }
                if let Some(new_offset) = new_entry.file_offset {
                    offset_index.insert(new_offset, idx);
                }
                merged_locs[idx] = new_entry;
                continue;
            }
            if let Some(file_offset) = incoming_loc.file_offset {
                if let Some(&idx) = offset_index.get(&file_offset) {
                    // Rule 2: different chunk_id at the same offset — keep the newer one.
                    let existing_loc = &merged_locs[idx];
                    // If the incoming file-level write_seq is strictly higher, the incoming
                    // chunk is definitively from a later write session and always wins,
                    // regardless of per-chunk client_write_seq. This covers the case where
                    // the incoming chunk has client_write_seq=None (e.g. fresh overwrite after
                    // a patch that did carry a client_write_seq).
                    let incoming_file_seq_wins = incoming.write_seq > 0
                        && existing.write_seq > 0
                        && incoming.write_seq > existing.write_seq;
                    let keep_existing = if incoming_file_seq_wins {
                        // File-level seq wins for fresh overwrites, but a chunk with a
                        // higher existing client_write_seq is still authoritative —
                        // prevents a stale broadcast (lower cws) from winning just
                        // because the accompanying file write_seq happened to be higher.
                        matches!(
                            (incoming_loc.client_write_seq, existing_loc.client_write_seq),
                            (Some(inc), Some(ext)) if ext > inc
                        )
                    } else {
                        match (incoming_loc.client_write_seq, existing_loc.client_write_seq) {
                            (Some(inc), Some(ext)) => ext > inc,
                            (Some(_), None)        => false,
                            (None, Some(_))        => true,
                            (None, None)           => {
                                existing_loc.written_at.unwrap_or(0) > incoming_loc.written_at.unwrap_or(0)
                            }
                        }
                    };
                    if !keep_existing {
                        id_index.remove(&merged_locs[idx].chunk_id);
                        id_index.insert(incoming_loc.chunk_id, idx);
                        merged_locs[idx] = incoming_loc.clone();
                    }
                    continue;
                }
            }
            // Genuinely new slot — append. Position is fixed up by the sort below;
            // id_index/offset_index only need to resolve to *some* valid index for
            // the rest of this merge call, not the final sorted one.
            let new_idx = merged_locs.len();
            id_index.insert(incoming_loc.chunk_id, new_idx);
            if let Some(offset) = incoming_loc.file_offset {
                offset_index.insert(offset, new_idx);
            }
            merged_locs.push(incoming_loc.clone());
        }

        // Restore offset order. Entries land here in whatever order distinct
        // offsets were first seen across this file's entire push history — under
        // buffered/background-tick writes the tail chunk routinely flushes before
        // the head chunk, so "genuinely new slot" appends above do NOT produce an
        // offset-sorted array on their own, and in-place Rule 1/2 updates preserve
        // whatever position an entry already has. Multiple consumers assume sorted
        // order for O(log n) binary search (handle_get_file_chunk_map's max-chunk-
        // index scan, chunk_map_update_location_for_file's partition_point, and
        // the client-side lookups in fuse_impl.rs/client.rs) — leaving this array
        // unsorted makes those silently return wrong results instead of erroring,
        // which is how a two-chunk file can end up served as if it were one chunk
        // (root-caused via a compat/upgrade test: t_patched.bin's tail chunk
        // flushed first, landed at index 0 forever, and handle_get_file_chunk_map's
        // sorted-from-the-end scan then computed total_chunks=1). None-offset
        // entries sort last, matching update_chunk_map_window's convention.
        merged_locs.sort_by_key(|l| l.file_offset.unwrap_or(u64::MAX));

        // Scalar fields (size, mtime, write_seq, ...) come from whichever side is
        // authoritative by write_seq — existing if incoming is stale, else incoming
        // (unchanged from before this fix). chunk_locations is always the union.
        let mut cloned = if is_stale { existing.clone() } else { incoming.clone() };
        cloned.chunk_locations = Arc::new(merged_locs);
        (cloned, is_stale)
    }

    /// PATH_TABLE's value format changed 2026-07-16 from a full serialized
    /// FileMetadata blob (an exact duplicate of what FILE_TABLE stores for the
    /// same record, rewritten on every single put_file/put_files_batch call) to
    /// just the pointed-to file_id string. FILE_TABLE remains the single source
    /// of truth for file content; PATH_TABLE is purely a path -> file_id index.
    /// Measured as the dominant metadata-DB write-volume driver during sustained
    /// patch writes (kdiskmark RND4K: ~27MB/min of blob payload, doubled again
    /// by this duplication) — see scratchpad_db_growth_baseline.md.
    ///
    /// Never migrated in bulk: existing entries stay in the old (blob) format
    /// until the next write to that path converts them — same reasoning as
    /// dfs_common::deserialize_file_metadata's FileMetadataLegacyV0 fallback for
    /// FileMetadata's own struct evolution. Safe to deploy over a live database
    /// with no migration pass and no window where a path is unreadable; the
    /// write-volume win phases in as records get touched rather than requiring
    /// staging to sit through a bulk rewrite. Every PATH_TABLE reader must go
    /// through this resolver instead of deserializing the value directly.
    fn resolve_path_entry_file_id(bytes: &[u8]) -> Result<FileId> {
        if let Ok(s) = std::str::from_utf8(bytes) {
            if let Ok(uuid) = uuid::Uuid::parse_str(s) {
                return Ok(FileId::from_uuid(uuid));
            }
        }
        // Legacy format: bytes are a full FileMetadata blob — pull the id out.
        dfs_common::deserialize_file_metadata(bytes)
            .map(|m| m.id)
            .context("path index entry is neither a file_id nor a legacy FileMetadata blob")
    }

    /// Third tuple element is the serialized payload byte count actually written
    /// to FILE_TABLE (feeds put_file/put_files_batch's note_txn() call, see
    /// MetadataStore::note_txn) — PATH_TABLE's write is now just a short file_id
    /// string, not worth tracking separately (see resolve_path_entry_file_id's
    /// doc comment for why the two tables no longer share one payload size).
    fn put_file_in_txn(
        file_table: &mut redb::Table<&str, &[u8]>,
        path_table: &mut redb::Table<&str, &[u8]>,
        metadata: &FileMetadata,
    ) -> Result<(PutFileResult, Option<String>, usize)> {
        let file_id_str = format!("{}", metadata.id);
        let path_str = metadata.path.as_str();

        // TEMP PROFILING (2026-07-07): timing instrumentation to find the per-push
        // server-side cost under sustained concurrent writes (32-way pipeline) — see
        // project memory on the vm-111 install bottleneck investigation. Remove once
        // the bottleneck is characterized.
        let t_merge_start = std::time::Instant::now();

        // Merge chunk_locations with any existing same-ID record.
        let existing_opt: Option<FileMetadata> = match file_table.get(file_id_str.as_str())? {
            Some(v) => dfs_common::deserialize_file_metadata(v.value()).ok(),
            None => None,
        };
        let existing_chunk_count = existing_opt.as_ref().map(|m| m.chunk_locations.len()).unwrap_or(0);
        let incoming_chunk_count = metadata.chunk_locations.len();

        let (metadata_to_store, is_stale) = Self::merge_file_metadata(existing_opt.as_ref(), metadata);

        // Never persist the full chunk_locations array — CHUNK_TABLE (via put_chunk_location)
        // is the sole authoritative per-chunk store, updated O(1) per touch. Before this fix,
        // every single push here re-serialized and rewrote the ENTIRE array regardless of delta
        // size — O(chunks in the file) per push, confirmed live as the dominant metadata-DB
        // growth driver for large/growing files (a DVR recording at write_seq=5126, ~161 chunks
        // and climbing, was generating enough genuine fragmentation to trigger compaction every
        // 60-90s without ever shrinking db_size). merge_file_metadata's union above still runs
        // (needed to correctly resolve every OTHER field), its result is just never written for
        // this one field. Every reader that needs a file's full chunk list now derives it fresh
        // from CHUNK_TABLE/chunk_map instead (see handle_append_file, rebuild_chunk_map_from_metadata,
        // handle_get_file_chunk_map's cache-miss fallback, handle_get_file_info,
        // push_held_file_metadata_to) — all of which were already either cold paths or, in
        // handle_append_file's case, fixed alongside this change to stop depending on it.
        // Fully backward compatible: old records already on disk keep their embedded array
        // (nothing ever reads it again after this deploys), no migration needed.
        //
        // Strip only the copy that gets serialized — `metadata_to_store` itself keeps the
        // real merged chunk_locations for the PutFileResult::Stale(_) return value below.
        // handle_disseminate_metadata forwards that value's chunk_locations to the leader
        // as a correction; emptying it there too would silently drop real chunk info from
        // that correction instead of just skipping a redundant disk write (cheap: Arc-clone
        // of every other field, not a deep copy).
        let for_disk = FileMetadata {
            chunk_locations: std::sync::Arc::new(Vec::new()),
            ..metadata_to_store.clone()
        };

        let merge_elapsed = t_merge_start.elapsed();
        if merge_elapsed.as_millis() > 5 {
            debug!(
                "[TIMING] put_file_in_txn merge: path={} existing_chunks={} incoming_chunks={} took={:?}",
                metadata.path, existing_chunk_count, incoming_chunk_count, merge_elapsed
            );
        }

        // If a different file ID already exists at this path, remove the stale file
        // record. Done only now (after the stale-check above can no longer bail
        // out) — see this function's doc comment for why the ordering matters.
        let old_id_str: Option<String> = match path_table.get(path_str)? {
            Some(v) => Self::resolve_path_entry_file_id(v.value())
                .ok()
                .filter(|id| *id != metadata.id)
                .map(|id| format!("{}", id)),
            None => None,
        };
        if let Some(old_id) = &old_id_str {
            if let Err(e) = file_table.remove(old_id.as_str()) {
                warn!("Failed to remove stale file record {} for path {}: {}", old_id, metadata.path, e);
            } else {
                debug!("Removed stale file record {} superseded by {} at path {}", old_id, metadata.id, metadata.path);
            }
        }

        let value = bincode::serialize(&for_disk)
            .context("Failed to serialize file metadata")?;

        file_table.insert(file_id_str.as_str(), value.as_slice())
            .context("Failed to insert file metadata")?;
        // Just the file_id, not the blob — see resolve_path_entry_file_id.
        path_table.insert(path_str, file_id_str.as_bytes())
            .context("Failed to insert path index")?;

        let payload_bytes = value.len();
        if is_stale {
            // Still report Stale so the caller knows incoming's scalar fields lost
            // and can converge whoever sent it — but the persisted record (returned
            // here) now includes the union, not just existing's original chunks.
            Ok((PutFileResult::Stale(metadata_to_store), old_id_str, payload_bytes))
        } else {
            Ok((PutFileResult::Stored, old_id_str, payload_bytes))
        }
    }

    /// Store file metadata.
    pub fn put_file(&self, metadata: &FileMetadata) -> Result<PutFileResult> {
        // TEMP PROFILING (2026-07-07): see put_file_in_txn's matching comment.
        let t_put_start = std::time::Instant::now();
        let _db = self.db.read();
        let t_lock_acquired = t_put_start.elapsed();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        let t_txn_begun = t_put_start.elapsed();

        let (result, old_id_str, payload_bytes) = {
            let mut file_table = txn.open_table(FILE_TABLE)?;
            let mut path_table = txn.open_table(PATH_TABLE)?;
            Self::put_file_in_txn(&mut file_table, &mut path_table, metadata)?
        };
        let t_txn_body_done = t_put_start.elapsed();

        // A Stale result no longer means "nothing was mutated" — put_file_in_txn still
        // unions incoming's chunk_locations into existing's before returning Stale, so
        // this transaction must always commit or that merge is silently rolled back.
        txn.commit()?;
        self.note_txn("put_file", payload_bytes);
        let t_committed = t_put_start.elapsed();
        if t_committed.as_millis() > 5 {
            debug!(
                "[TIMING] put_file: path={} lock={:?} begin_txn={:?} body={:?} commit={:?} total={:?}",
                metadata.path, t_lock_acquired, t_txn_begun - t_lock_acquired,
                t_txn_body_done - t_txn_begun, t_committed - t_txn_body_done, t_committed
            );
        }

        // Mark touched keys dirty for compact_db_with_budget's incremental catch-up —
        // must happen while `_db` (the read guard) is still held, so Phase 3's
        // exclusive db.write() lock acts as a barrier: it cannot succeed until this
        // line has already run for every write that started before it.
        self.dirty_files.lock().unwrap().insert(format!("{}", metadata.id));
        self.dirty_paths.lock().unwrap().insert(metadata.path.clone());
        if let Some(old_id) = &old_id_str {
            self.dirty_files.lock().unwrap().insert(old_id.clone());
        }

        debug!("Stored metadata for file: {} ({})", metadata.path, metadata.id);
        Ok(result)
    }

    /// Batched version of put_file: applies every item within ONE shared
    /// transaction (one commit for the whole batch) instead of one transaction per
    /// item. This is the dissemination catch-up path's hot loop
    /// (handle_disseminate_metadata in server.rs) — a follower that's been offline
    /// accumulates one queued entry per write it missed, and applying thousands of
    /// them via individual put_file calls (one redb commit each) is what made a
    /// real deployment's rejoin catch-up take well over a minute for ~10,000 queued
    /// records, even though almost all of that time was pure per-transaction
    /// overhead rather than actual work. Batching cuts that to a single commit.
    ///
    /// Returns one PutFileResult per input item, in the same order. Unlike calling
    /// put_file in a loop, a hard error (e.g. genuine I/O failure) aborts the whole
    /// batch rather than skipping just the offending item — acceptable here since
    /// such errors are effectively never caused by a single item's content, and the
    /// caller (handle_disseminate_metadata) already retries the whole batch on any
    /// failure (the leader won't ack_meta_queue_for_node until it succeeds).
    pub fn put_files_batch(&self, items: &[FileMetadata]) -> Result<Vec<PutFileResult>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }

        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());

        let mut results = Vec::with_capacity(items.len());
        let mut touched_file_ids: Vec<String> = Vec::new();
        let mut touched_paths: Vec<String> = Vec::new();
        let mut payload_bytes: usize = 0;
        {
            let mut file_table = txn.open_table(FILE_TABLE)?;
            let mut path_table = txn.open_table(PATH_TABLE)?;
            for metadata in items {
                let (result, old_id_str, item_bytes) = Self::put_file_in_txn(&mut file_table, &mut path_table, metadata)?;
                // Stale results still mutate (chunk_locations union — see put_file_in_txn),
                // so they must be marked dirty too, same as Stored.
                touched_file_ids.push(format!("{}", metadata.id));
                touched_paths.push(metadata.path.clone());
                if let Some(old_id) = old_id_str {
                    touched_file_ids.push(old_id);
                }
                payload_bytes += item_bytes;
                results.push(result);
            }
        }
        txn.commit()?;
        self.note_txn("put_files_batch", payload_bytes);

        // See put_file's matching comment: must happen while `_db` is still held.
        self.dirty_files.lock().unwrap().extend(touched_file_ids);
        self.dirty_paths.lock().unwrap().extend(touched_paths);

        Ok(results)
    }

    /// Get file metadata by ID.
    /// Cheap existence check — avoids deserializing the full FileMetadata.
    pub fn file_exists_by_id(&self, file_id: FileId) -> Result<bool> {
        let key = format!("{}", file_id);
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(FILE_TABLE)?;
        Ok(table.get(key.as_str())?.is_some())
    }

    /// Async wrapper for file_exists_by_id — see get_chunk_location_async. Must be
    /// used from any code running on a tokio worker thread (e.g. the concurrently-
    /// spawned heal tasks in drain_heal_queue's JoinSet): the sync variant blocks
    /// the worker on the parking_lot `db` read lock, and many concurrent blocking
    /// reads starve the runtime — the second half of the 2026-07-17 gluster1 wedge.
    pub async fn file_exists_by_id_async(self: &Arc<Self>, file_id: FileId) -> Result<bool> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.file_exists_by_id(file_id))
            .await
            .context("spawn_blocking panicked in file_exists_by_id_async")?
    }

    pub fn get_file(&self, file_id: &FileId) -> Result<Option<FileMetadata>> {
        let key = format!("{}", file_id);
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(FILE_TABLE)?;
        match table.get(key.as_str())? {
            Some(v) => Ok(Some(dfs_common::deserialize_file_metadata(v.value())
                .with_context(|| format!("Failed to deserialize metadata for {}", file_id))?)),
            None => Ok(None),
        }
    }

    /// Test-only: write unparseable bytes directly under a file_id key, bypassing
    /// put_file's normal serialization. Used to reproduce a genuine get_file() Err
    /// (as opposed to Ok(None)), which no amount of legitimate FileMetadata data can
    /// trigger — see the 2026-07-06 healing.rs regression test this supports.
    #[cfg(test)]
    pub(crate) fn put_raw_file_bytes(&self, file_id: &FileId, bytes: &[u8]) -> Result<()> {
        let key = format!("{}", file_id);
        let _db = self.db.read();
        let txn = _db.begin_write()?;
        {
            let mut file_table = txn.open_table(FILE_TABLE)?;
            file_table.insert(key.as_str(), bytes)?;
        }
        txn.commit()?;
        self.note_txn("put_raw_file_bytes", bytes.len());
        Ok(())
    }

    /// Test-only: write PATH_TABLE's value for `path` directly, bypassing
    /// put_file_in_txn's normal file_id-only encoding. Used to simulate a
    /// pre-2026-07-16 legacy entry (a full serialized FileMetadata blob) so the
    /// resolve_path_entry_file_id fallback can be exercised without needing an
    /// actual old binary — see that function's doc comment for the upgrade-path
    /// contract this proves.
    #[cfg(test)]
    pub(crate) fn put_raw_path_entry(&self, path: &str, bytes: &[u8]) -> Result<()> {
        let _db = self.db.read();
        let txn = _db.begin_write()?;
        {
            let mut path_table = txn.open_table(PATH_TABLE)?;
            path_table.insert(path, bytes)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Async wrapper for get_file — see get_chunk_location_async for why the sync
    /// version must never be called directly from async request-handling code.
    pub async fn get_file_async(self: &Arc<Self>, file_id: FileId) -> Result<Option<FileMetadata>> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.get_file(&file_id))
            .await
            .context("spawn_blocking panicked in get_file_async")?
    }

    /// Get file metadata by path.
    pub fn get_file_by_path(&self, path: &str) -> Result<Option<FileMetadata>> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let path_table = txn.open_table(PATH_TABLE)?;
        let file_id = match path_table.get(path)? {
            Some(v) => Self::resolve_path_entry_file_id(v.value())
                .with_context(|| format!("Failed to resolve path index entry for {}", path))?,
            None => return Ok(None),
        };
        let file_table = txn.open_table(FILE_TABLE)?;
        let key = format!("{}", file_id);
        match file_table.get(key.as_str())? {
            Some(v) => Ok(Some(dfs_common::deserialize_file_metadata(v.value())
                .with_context(|| format!("Failed to deserialize metadata for path {}", path))?)),
            None => Ok(None),
        }
    }

    /// Async wrapper for get_file_by_path — see get_file_async for why the sync
    /// version must never be called directly from async request-handling code.
    pub async fn get_file_by_path_async(self: &Arc<Self>, path: String) -> Result<Option<FileMetadata>> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.get_file_by_path(&path))
            .await
            .context("spawn_blocking panicked in get_file_by_path_async")?
    }

    /// Delete file metadata (removes both file and path index entries).
    pub fn delete_file(&self, file_id: &FileId) -> Result<()> {
        let file_id_str = format!("{}", file_id);
        let mut removed_path: Option<String> = None;
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut file_table = txn.open_table(FILE_TABLE)?;
            let mut path_table = txn.open_table(PATH_TABLE)?;

            // Get path from file record so we can remove the path index entry.
            if let Some(v) = file_table.get(file_id_str.as_str())? {
                if let Ok(m) = dfs_common::deserialize_file_metadata(v.value()) {
                    path_table.remove(m.path.as_str())?;
                    removed_path = Some(m.path);
                }
            }
            file_table.remove(file_id_str.as_str())?;
        }
        txn.commit()?;
        self.note_txn("delete_file", 0);

        // See put_file's matching comment: must happen while `_db` is still held.
        self.dirty_files.lock().unwrap().insert(file_id_str.clone());
        if let Some(path) = &removed_path {
            self.dirty_paths.lock().unwrap().insert(path.clone());
        }

        debug!("Deleted metadata for file: {}", file_id);
        Ok(())
    }

    /// Delete only the path index entry for a specific path (used during rename).
    pub fn delete_path_index(&self, path: &str) -> Result<()> {
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(PATH_TABLE)?;
            table.remove(path)?;
        }
        txn.commit()?;
        self.note_txn("delete_path_index", 0);

        // See put_file's matching comment: must happen while `_db` is still held.
        self.dirty_paths.lock().unwrap().insert(path.to_string());

        debug!("Deleted path index for: {}", path);
        Ok(())
    }

    /// Async wrapper for put_file — offloads the blocking redb commit to a dedicated
    /// blocking thread so it never blocks a Tokio worker thread. Call sites that
    /// already route through the dedicated sled_write_tx writer thread (see
    /// server.rs) don't need this; use it anywhere else put_file is called directly
    /// from async code. See put_chunk_location_async for why this matters.
    pub async fn put_file_async(self: &Arc<Self>, metadata: FileMetadata) -> Result<PutFileResult> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.put_file(&metadata))
            .await
            .context("spawn_blocking panicked in put_file_async")?
    }

    /// Async wrapper for delete_file — see put_file_async.
    pub async fn delete_file_async(self: &Arc<Self>, file_id: FileId) -> Result<()> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.delete_file(&file_id))
            .await
            .context("spawn_blocking panicked in delete_file_async")?
    }

    /// Async wrapper for delete_path_index — see put_file_async.
    pub async fn delete_path_index_async(self: &Arc<Self>, path: String) -> Result<()> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.delete_path_index(&path))
            .await
            .context("spawn_blocking panicked in delete_path_index_async")?
    }

    /// List all files — loads all records into memory. Use scan_files for streaming.
    pub fn list_files(&self) -> Result<Vec<FileMetadata>> {
        let mut files = Vec::new();
        self.scan_files(|m| { files.push(m); Ok(()) })?;
        Ok(files)
    }

    /// Stream all file metadata records, calling `f` for each without materialising all in RAM.
    pub fn scan_files<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(FileMetadata) -> Result<()>,
    {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(FILE_TABLE)?;
        for item in table.range::<&str>(..)? {
            let (k, v) = item?;
            match dfs_common::deserialize_file_metadata(v.value()) {
                Ok(m) => f(m)?,
                Err(_) => warn!("Skipping corrupt metadata entry (key={:?})", k.value()),
            }
        }
        Ok(())
    }

    /// Remove file and path records whose file ID is not in `live_ids`.
    /// Called on followers after ReconcileMetadata.  Returns records removed.
    pub fn remove_unlisted_files(
        &self,
        live_ids: &std::collections::HashSet<FileId>,
    ) -> Result<usize> {
        // Collect stale file entries.
        let (stale_file_ids, stale_file_paths): (Vec<String>, Vec<String>) = {
            let _db = self.db.read();
            let txn = _db.begin_read()?;
            let table = txn.open_table(FILE_TABLE)?;
            let mut ids = Vec::new();
            let mut paths = Vec::new();
            for item in table.range::<&str>(..)? {
                let (k, v) = item?;
                let m = match dfs_common::deserialize_file_metadata(v.value()) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if !live_ids.contains(&m.id) {
                    ids.push(k.value().to_string());
                    paths.push(m.path.clone());
                }
            }
            (ids, paths)
        };

        let mut removed = 0usize;
        if !stale_file_ids.is_empty() {
            let _db = self.db.read();
            let mut txn = _db.begin_write()?;
            txn.set_durability(self.next_write_durability());
            {
                let mut file_table = txn.open_table(FILE_TABLE)?;
                let mut path_table = txn.open_table(PATH_TABLE)?;
                for (id_str, path) in stale_file_ids.iter().zip(stale_file_paths.iter()) {
                    warn!("ReconcileMetadata: removing stale file record: {} (path: {})", id_str, path);
                    file_table.remove(id_str.as_str())?;
                    path_table.remove(path.as_str())?;
                    removed += 1;
                }
            }
            txn.commit()?;
            self.note_txn("remove_unlisted_files", 0);

            // See put_file's matching comment: must happen while `_db` is still held.
            {
                let mut dirty_files = self.dirty_files.lock().unwrap();
                dirty_files.extend(stale_file_ids.iter().cloned());
            }
            {
                let mut dirty_paths = self.dirty_paths.lock().unwrap();
                dirty_paths.extend(stale_file_paths.iter().cloned());
            }
        }

        // Also sweep path entries independently — a path entry can exist without
        // a file entry if the file entry was removed out-of-order.
        let stale_path_keys: Vec<String> = {
            let _db = self.db.read();
            let txn = _db.begin_read()?;
            let table = txn.open_table(PATH_TABLE)?;
            let mut stale = Vec::new();
            for item in table.range::<&str>(..)? {
                let (k, v) = item?;
                if let Ok(file_id) = Self::resolve_path_entry_file_id(v.value()) {
                    if !live_ids.contains(&file_id) {
                        stale.push(k.value().to_string());
                    }
                }
            }
            stale
        };

        if !stale_path_keys.is_empty() {
            let _db = self.db.read();
            let mut txn = _db.begin_write()?;
            txn.set_durability(self.next_write_durability());
            {
                let mut table = txn.open_table(PATH_TABLE)?;
                for path in &stale_path_keys {
                    warn!("ReconcileMetadata: removing stale path index entry: {}", path);
                    table.remove(path.as_str())?;
                    removed += 1;
                }
            }
            txn.commit()?;
            self.note_txn("remove_unlisted_files", 0);

            // See put_file's matching comment: must happen while `_db` is still held.
            let mut dirty_paths = self.dirty_paths.lock().unwrap();
            dirty_paths.extend(stale_path_keys.iter().cloned());
        }

        if removed > 0 {
            info!("ReconcileMetadata: removed {} stale metadata records", removed);
        }
        Ok(removed)
    }

    /// List direct children of `dir_path`.
    pub fn list_directory(&self, dir_path: &str) -> Result<Vec<FileMetadata>> {
        let dir_path = if dir_path.ends_with('/') {
            dir_path.to_string()
        } else {
            format!("{}/", dir_path)
        };

        // Range scan: all paths that start with dir_path.
        // Upper bound = increment last char of dir_path ("/" → "0").
        let end = prefix_next(&dir_path);

        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let path_table = txn.open_table(PATH_TABLE)?;
        let file_table = txn.open_table(FILE_TABLE)?;
        let mut files = Vec::new();
        for item in path_table.range(dir_path.as_str()..end.as_str())? {
            let (k, v) = item?;
            let path = k.value();
            let relative = &path[dir_path.len()..];
            // Direct child: no slash, or a lone trailing slash (directory entry).
            if !relative.is_empty() && (!relative.contains('/') || relative.ends_with('/')) {
                let file_id = match Self::resolve_path_entry_file_id(v.value()) {
                    Ok(id) => id,
                    Err(_) => {
                        warn!("list_directory: could not resolve path index entry for {}", path);
                        continue;
                    }
                };
                let key = format!("{}", file_id);
                match file_table.get(key.as_str()) {
                    Ok(Some(fv)) => match dfs_common::deserialize_file_metadata(fv.value()) {
                        Ok(m) => files.push(m),
                        Err(_) => warn!("list_directory: could not deserialize file record {} for path {}", file_id, path),
                    },
                    Ok(None) => warn!("list_directory: path index points at missing file record {} for path {}", file_id, path),
                    Err(e) => warn!("list_directory: file_table lookup failed for {} (path {}): {}", file_id, path, e),
                }
            }
        }
        Ok(files)
    }

    // -------------------------------------------------------------------------
    // Chunk location
    // -------------------------------------------------------------------------

    /// Store chunk location information.
    pub fn put_chunk_location(&self, location: &ChunkLocation) -> Result<()> {
        let key = format!("{}", location.chunk_id);
        let value = bincode::serialize(location)
            .context("Failed to serialize chunk location")?;
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(CHUNK_TABLE)?;
            table.insert(key.as_str(), value.as_slice())?;
        }
        txn.commit()?;
        self.note_txn("put_chunk_location", value.len());
        debug!("Stored location for chunk: {}", location.chunk_id);
        Ok(())
    }

    /// Store multiple chunk locations in a single write transaction.
    ///
    /// Callers that already have several locations to persist at once (e.g. the
    /// ReplicateChunkLocations batch RPC handler) should use this instead of looping
    /// put_chunk_location — committing N locations as N separate transactions costs N
    /// separate B-tree page allocations and pending_non_durable_commits entries, the
    /// same per-transaction overhead this function exists to amortize across the whole
    /// batch in one commit.
    pub fn put_chunk_locations_batch(&self, locations: &[ChunkLocation]) -> Result<()> {
        if locations.is_empty() {
            return Ok(());
        }
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        let mut payload_bytes: usize = 0;
        {
            let mut table = txn.open_table(CHUNK_TABLE)?;
            for location in locations {
                let key = format!("{}", location.chunk_id);
                let value = bincode::serialize(location)
                    .context("Failed to serialize chunk location")?;
                payload_bytes += value.len();
                table.insert(key.as_str(), value.as_slice())?;
            }
        }
        txn.commit()?;
        self.note_txn("put_chunk_locations_batch", payload_bytes);
        debug!("Stored {} chunk locations in one batch transaction", locations.len());
        Ok(())
    }

    /// Get chunk location information.
    pub fn get_chunk_location(&self, chunk_id: &ChunkId) -> Result<Option<ChunkLocation>> {
        let key = format!("{}", chunk_id);
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(CHUNK_TABLE)?;
        match table.get(key.as_str())? {
            Some(v) => Ok(Some(bincode::deserialize::<ChunkLocation>(v.value())
                .with_context(|| format!("Failed to deserialize chunk location {}", chunk_id))?)),
            None => Ok(None),
        }
    }

    /// Async wrapper for get_chunk_location. Calling the sync version directly from
    /// request-handling code runs a redb read transaction (real disk/mmap I/O) inline
    /// on the tokio executor thread — see put_chunk_location_async for why that's
    /// dangerous under load. This is on the hottest per-write path there is
    /// (handle_replicate_chunk_location fires once per confirmed chunk write
    /// cluster-wide), so always call this instead from async handlers.
    pub async fn get_chunk_location_async(self: &Arc<Self>, chunk_id: ChunkId) -> Result<Option<ChunkLocation>> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.get_chunk_location(&chunk_id))
            .await
            .context("spawn_blocking panicked in get_chunk_location_async")?
    }

    /// Delete chunk location.
    pub fn delete_chunk_location(&self, chunk_id: &ChunkId) -> Result<()> {
        let key = format!("{}", chunk_id);
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(CHUNK_TABLE)?;
            table.remove(key.as_str())?;
        }
        txn.commit()?;
        self.note_txn("delete_chunk_location", 0);
        Ok(())
    }

    /// Async wrapper for put_chunk_location. This is THE hot path: it's called
    /// once per confirmed chunk write via ReplicateChunkLocation, at whatever rate
    /// clients are writing/healing. Calling put_chunk_location directly from async
    /// code (no .await inside it) lets a burst of concurrent callers monopolize
    /// every Tokio worker thread simultaneously inside redb's write-transaction
    /// lock, starving the whole runtime — this is what froze gluster1 in staging
    /// on 2026-06-19 (see metadata::tests::test_put_chunk_location_does_not_starve_runtime_under_concurrency).
    /// Always call this instead of put_chunk_location from request-handling code.
    ///
    /// Since 2026-07-15 this submits to the group committer instead of running its
    /// own single-record transaction via spawn_blocking — same commit-before-return
    /// semantics, ~1 transaction per concurrent burst instead of 1 per record. See
    /// committer_tx.
    pub async fn put_chunk_location_async(self: &Arc<Self>, location: ChunkLocation) -> Result<()> {
        if !Self::group_commit_enabled() {
            let store = Arc::clone(self);
            return tokio::task::spawn_blocking(move || store.put_chunk_location(&location))
                .await
                .context("spawn_blocking panicked in put_chunk_location_async")?;
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.committer_tx().send(MetaWriteOp::PutChunkLocation { location, reply }).await
            .map_err(|_| anyhow::anyhow!("metadata group-commit thread is gone"))?;
        rx.await.context("metadata group-commit thread dropped its reply")?
    }

    /// Async wrapper for put_chunk_locations_batch — see put_chunk_location_async.
    /// Group-committed since 2026-07-15: the live write path sends single-chunk
    /// "batches" through here once per confirmed write, so concurrent callers need
    /// coalescing exactly like the single-record ops (see
    /// MetaWriteOp::UpdateChunkLocationsBatch for the measurement).
    pub async fn put_chunk_locations_batch_async(self: &Arc<Self>, locations: Vec<ChunkLocation>) -> Result<()> {
        if locations.is_empty() {
            return Ok(());
        }
        if !Self::group_commit_enabled() {
            let store = Arc::clone(self);
            return tokio::task::spawn_blocking(move || store.put_chunk_locations_batch(&locations))
                .await
                .context("spawn_blocking panicked in put_chunk_locations_batch_async")?;
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.committer_tx().send(MetaWriteOp::UpdateChunkLocationsBatch {
            puts: locations, deletes: Vec::new(), pending_healing_deletes: Vec::new(), reply,
        }).await.map_err(|_| anyhow::anyhow!("metadata group-commit thread is gone"))?;
        rx.await.context("metadata group-commit thread dropped its reply")?
    }

    /// Async wrapper for delete_chunk_location — see put_chunk_location_async
    /// (group-committed since 2026-07-15; as hot as puts under patch load, the
    /// chunk-ID rotation deletes the old identity on every fold).
    pub async fn delete_chunk_location_async(self: &Arc<Self>, chunk_id: ChunkId) -> Result<()> {
        if !Self::group_commit_enabled() {
            let store = Arc::clone(self);
            return tokio::task::spawn_blocking(move || store.delete_chunk_location(&chunk_id))
                .await
                .context("spawn_blocking panicked in delete_chunk_location_async")?;
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.committer_tx().send(MetaWriteOp::DeleteChunkLocation { chunk_id, reply }).await
            .map_err(|_| anyhow::anyhow!("metadata group-commit thread is gone"))?;
        rx.await.context("metadata group-commit thread dropped its reply")?
    }

    // -------------------------------------------------------------------------
    // Chunk refcounts (fast-path eviction for patch-generated chunk_ids)
    // -------------------------------------------------------------------------

    /// Mark `chunk_id` as live for one (file, chunk_idx) slot. Call exactly
    /// once per chunk_id, when it becomes the new value produced by a patch.
    pub fn incr_chunk_refcount(&self, chunk_id: &ChunkId) -> Result<u64> {
        let key = format!("{}", chunk_id);
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        let new_count;
        {
            let mut table = txn.open_table(CHUNK_REFCOUNT_TABLE)?;
            let current = table.get(key.as_str())?.map(|v| v.value()).unwrap_or(0);
            new_count = current + 1;
            table.insert(key.as_str(), new_count)?;
        }
        txn.commit()?;
        self.note_txn("incr_chunk_refcount", 0);
        Ok(new_count)
    }

    /// Decrement the refcount for `chunk_id`. Returns:
    /// - `Some(n)` — chunk_id was tracked; `n` is the count after decrementing
    ///   (0 means no other slot references it under this scheme — safe to
    ///   delete immediately; the table entry is removed in that case).
    /// - `None` — chunk_id was never tracked (e.g. an original chunker-hash
    ///   chunk, potentially dedup-shared with another file). Callers MUST NOT
    ///   delete the chunk in this case; leave it for the deep sweep.
    pub fn decr_chunk_refcount(&self, chunk_id: &ChunkId) -> Result<Option<u64>> {
        let key = format!("{}", chunk_id);
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        let result;
        {
            let mut table = txn.open_table(CHUNK_REFCOUNT_TABLE)?;
            let existing: Option<u64> = table.get(key.as_str())?.map(|v| v.value());
            match existing {
                None => result = None,
                Some(current) => {
                    let new_count = current.saturating_sub(1);
                    if new_count == 0 {
                        table.remove(key.as_str())?;
                    } else {
                        table.insert(key.as_str(), new_count)?;
                    }
                    result = Some(new_count);
                }
            }
        }
        txn.commit()?;
        self.note_txn("decr_chunk_refcount", 0);
        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Per-slot chunk sequence (see CHUNK_SEQ_TABLE's doc comment)
    // -------------------------------------------------------------------------

    /// Last-applied client-assigned sequence number for (file_id, chunk_idx),
    /// or None if this slot has never recorded one (pre-dates this table, or
    /// simply never patched).
    pub fn get_chunk_seq(&self, file_id: FileId, chunk_idx: u64) -> Result<Option<u64>> {
        let key = format!("{}:{}", file_id, chunk_idx);
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        // Lazily created (like CHUNK_REFCOUNT_TABLE) — a store that has never
        // recorded a chunk_seq yet errors on the read-side open_table.
        let table = match txn.open_table(CHUNK_SEQ_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(table.get(key.as_str())?.map(|v| v.value()))
    }

    /// Async wrapper for get_chunk_seq — see put_chunk_location_async.
    pub async fn get_chunk_seq_async(self: &Arc<Self>, file_id: FileId, chunk_idx: u64) -> Result<Option<u64>> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.get_chunk_seq(file_id, chunk_idx))
            .await
            .context("spawn_blocking panicked in get_chunk_seq_async")?
    }

    /// Record `seq` as the last-applied sequence number for (file_id, chunk_idx).
    /// Currently unconditional (record-only, no CAS/rejection) — see
    /// CHUNK_SEQ_TABLE's doc comment for the follow-up that makes this
    /// load-bearing in apply_patch/handle_multi_patch.
    pub fn put_chunk_seq(&self, file_id: FileId, chunk_idx: u64, seq: u64) -> Result<()> {
        let key = format!("{}:{}", file_id, chunk_idx);
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(CHUNK_SEQ_TABLE)?;
            // Monotonic (L3, 2026-07-19) — see the committer path's PutChunkSeq arm for
            // why: a stale/out-of-order push must never lower the per-slot generation.
            let current = table.get(key.as_str())?.map(|v| v.value()).unwrap_or(0);
            if seq > current {
                table.insert(key.as_str(), seq)?;
            }
        }
        txn.commit()?;
        self.note_txn("put_chunk_seq", 0);
        Ok(())
    }

    /// Async wrapper for put_chunk_seq — see put_chunk_location_async
    /// (group-committed since 2026-07-15; fires once per client chunk write).
    pub async fn put_chunk_seq_async(self: &Arc<Self>, file_id: FileId, chunk_idx: u64, seq: u64) -> Result<()> {
        if !Self::group_commit_enabled() {
            let store = Arc::clone(self);
            return tokio::task::spawn_blocking(move || store.put_chunk_seq(file_id, chunk_idx, seq))
                .await
                .context("spawn_blocking panicked in put_chunk_seq_async")?;
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.committer_tx().send(MetaWriteOp::PutChunkSeq { file_id, chunk_idx, seq, reply }).await
            .map_err(|_| anyhow::anyhow!("metadata group-commit thread is gone"))?;
        rx.await.context("metadata group-commit thread dropped its reply")?
    }

    // -------------------------------------------------------------------------
    // Patch state (deferred single-patch consolidation — see PATCH_STATE_TABLE)
    // -------------------------------------------------------------------------

    /// Register a new Pending patch_state row for `public_token`, retiring
    /// whatever row was previously outstanding for this (file_id, chunk_idx) slot
    /// (if any) in the same transaction. Returns the retired token, if there was
    /// one, so the caller can also drop it from any in-memory fast-path index.
    pub fn put_patch_state_pending(
        &self, file_id: FileId, chunk_idx: u64, public_token: &ChunkId,
        base_chunk_id: ChunkId, delta_chunk_id: ChunkId, size: usize,
        written_at: u64, client_write_seq: Option<u64>,
    ) -> Result<Option<ChunkId>> {
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        let (retired, payload_bytes) = Self::put_patch_state_pending_in_txn(
            &txn, file_id, chunk_idx, public_token, base_chunk_id, delta_chunk_id,
            size, written_at, client_write_seq,
        )?;
        txn.commit()?;
        self.note_txn("put_patch_state_pending", payload_bytes);
        Ok(retired)
    }

    /// Body of put_patch_state_pending against an already-open write transaction —
    /// shared by the standalone function above and the group-commit committer
    /// (apply_ops_group) so the slot-retirement logic can never drift between the
    /// two paths. Returns (retired token, serialized payload bytes).
    #[allow(clippy::too_many_arguments)]
    fn put_patch_state_pending_in_txn(
        txn: &redb::WriteTransaction, file_id: FileId, chunk_idx: u64, public_token: &ChunkId,
        base_chunk_id: ChunkId, delta_chunk_id: ChunkId, size: usize,
        written_at: u64, client_write_seq: Option<u64>,
    ) -> Result<(Option<ChunkId>, usize)> {
        let slot_key = format!("{}:{}", file_id, chunk_idx);
        let token_key = format!("{}", public_token);
        let state = PatchState::Pending { base_chunk_id, delta_chunk_id, size, written_at, client_write_seq };
        let value = bincode::serialize(&state).context("Failed to serialize patch state")?;

        let retired = {
            let mut slot_table = txn.open_table(PATCH_STATE_SLOT_TABLE)?;
            let retired = slot_table.get(slot_key.as_str())?
                .map(|v| v.value().to_vec())
                .map(String::from_utf8)
                .transpose()
                .context("corrupt patch state slot entry (not utf8)")?;
            slot_table.insert(slot_key.as_str(), token_key.as_bytes())?;
            retired
        };
        {
            let mut state_table = txn.open_table(PATCH_STATE_TABLE)?;
            if let Some(retired) = &retired {
                state_table.remove(retired.as_str())?;
            }
            state_table.insert(token_key.as_str(), value.as_slice())?;
        }
        let payload_bytes = value.len();
        Ok((retired.and_then(|hex| decode_hex_32(&hex)).map(ChunkId::from_hash), payload_bytes))
    }

    /// Async wrapper for put_patch_state_pending — patches are a normal-volume
    /// client write path; see put_chunk_location_async for why this matters
    /// (group-committed since 2026-07-15; fires once per 4K patch under RND4K load).
    #[allow(clippy::too_many_arguments)]
    pub async fn put_patch_state_pending_async(
        self: &Arc<Self>, file_id: FileId, chunk_idx: u64, public_token: ChunkId,
        base_chunk_id: ChunkId, delta_chunk_id: ChunkId, size: usize,
        written_at: u64, client_write_seq: Option<u64>,
    ) -> Result<Option<ChunkId>> {
        if !Self::group_commit_enabled() {
            let store = Arc::clone(self);
            return tokio::task::spawn_blocking(move || store.put_patch_state_pending(
                file_id, chunk_idx, &public_token, base_chunk_id, delta_chunk_id, size, written_at, client_write_seq,
            )).await.context("spawn_blocking panicked in put_patch_state_pending_async")?;
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.committer_tx().send(MetaWriteOp::PutPatchStatePending {
            file_id, chunk_idx, public_token, base_chunk_id, delta_chunk_id,
            size, written_at, client_write_seq, reply,
        }).await.map_err(|_| anyhow::anyhow!("metadata group-commit thread is gone"))?;
        rx.await.context("metadata group-commit thread dropped its reply")?
    }

    /// Flip an existing patch_state row from Pending to Folded once the
    /// background fold completes. Same key (`public_token`), no slot-table
    /// change — the slot's outstanding token doesn't change, only what it
    /// resolves to.
    pub fn update_patch_state_folded(&self, public_token: &ChunkId, new_chunk_id: ChunkId) -> Result<()> {
        let token_key = format!("{}", public_token);
        let value = bincode::serialize(&PatchState::Folded(new_chunk_id))
            .context("Failed to serialize patch state")?;
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(PATCH_STATE_TABLE)?;
            table.insert(token_key.as_str(), value.as_slice())?;
        }
        txn.commit()?;
        self.note_txn("update_patch_state_folded", value.len());
        Ok(())
    }

    /// Remove a Pending PATCH_STATE_TABLE row directly, along with its
    /// PATCH_STATE_SLOT_TABLE pointer if that slot still points at this exact
    /// token. For abandoning a patch whose base and/or delta chunk is
    /// confirmed gone cluster-wide (no CHUNK_TABLE record anywhere, not just
    /// missing on this node) — see run_single_fold's call site for the full
    /// rationale and the safety check that gates calling this at all. Distinct
    /// from update_patch_state_folded, which is the *normal* (successful) way
    /// a Pending row stops being Pending; this is the abandon-on-failure path.
    pub fn delete_patch_state_abandoned(&self, public_token: &ChunkId, file_id: FileId, chunk_idx: u64) -> Result<()> {
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        Self::delete_patch_state_abandoned_in_txn(&txn, public_token, file_id, chunk_idx)?;
        txn.commit()?;
        self.note_txn("delete_patch_state_abandoned", 0);
        Ok(())
    }

    /// Body of delete_patch_state_abandoned against an already-open write
    /// transaction — shared with the group-commit committer, same rationale as
    /// put_patch_state_pending_in_txn.
    fn delete_patch_state_abandoned_in_txn(
        txn: &redb::WriteTransaction, public_token: &ChunkId, file_id: FileId, chunk_idx: u64,
    ) -> Result<()> {
        let token_key = format!("{}", public_token);
        let slot_key = format!("{}:{}", file_id, chunk_idx);
        {
            let mut state_table = txn.open_table(PATCH_STATE_TABLE)?;
            state_table.remove(token_key.as_str())?;
        }
        {
            let mut slot_table = txn.open_table(PATCH_STATE_SLOT_TABLE)?;
            let still_current = slot_table.get(slot_key.as_str())?
                .map(|v| v.value() == token_key.as_bytes())
                .unwrap_or(false);
            if still_current {
                slot_table.remove(slot_key.as_str())?;
            }
        }
        Ok(())
    }

    /// Async wrapper for delete_patch_state_abandoned — see put_chunk_location_async
    /// (group-committed since 2026-07-15).
    pub async fn delete_patch_state_abandoned_async(self: &Arc<Self>, public_token: ChunkId, file_id: FileId, chunk_idx: u64) -> Result<()> {
        if !Self::group_commit_enabled() {
            let store = Arc::clone(self);
            return tokio::task::spawn_blocking(move || store.delete_patch_state_abandoned(&public_token, file_id, chunk_idx))
                .await
                .context("spawn_blocking panicked in delete_patch_state_abandoned_async")?;
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.committer_tx().send(MetaWriteOp::DeletePatchStateAbandoned { public_token, file_id, chunk_idx, reply }).await
            .map_err(|_| anyhow::anyhow!("metadata group-commit thread is gone"))?;
        rx.await.context("metadata group-commit thread dropped its reply")?
    }

    /// Async wrapper for update_patch_state_folded — see put_chunk_location_async
    /// (group-committed since 2026-07-15; fires once per completed fold).
    pub async fn update_patch_state_folded_async(self: &Arc<Self>, public_token: ChunkId, new_chunk_id: ChunkId) -> Result<()> {
        if !Self::group_commit_enabled() {
            let store = Arc::clone(self);
            return tokio::task::spawn_blocking(move || store.update_patch_state_folded(&public_token, new_chunk_id))
                .await
                .context("spawn_blocking panicked in update_patch_state_folded_async")?;
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.committer_tx().send(MetaWriteOp::UpdatePatchStateFolded { public_token, new_chunk_id, reply }).await
            .map_err(|_| anyhow::anyhow!("metadata group-commit thread is gone"))?;
        rx.await.context("metadata group-commit thread dropped its reply")?
    }

    /// Look up a patch_state row by its public token. `None` means `chunk_id` is
    /// not a currently-outstanding patch token at all — callers should treat it
    /// as ordinary, directly-readable chunk content.
    pub fn get_patch_state(&self, public_token: &ChunkId) -> Result<Option<PatchState>> {
        let key = format!("{}", public_token);
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        // Not pre-created at startup (lazy, like CHUNK_REFCOUNT_TABLE) — a store
        // that has never had a patch yet errors on the read-side open_table
        // (unlike the write side, which auto-creates). Treat as "no such state".
        let table = match txn.open_table(PATCH_STATE_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match table.get(key.as_str())? {
            Some(v) => Ok(Some(bincode::deserialize::<PatchState>(v.value())
                .with_context(|| format!("Failed to deserialize patch state {}", public_token))?)),
            None => Ok(None),
        }
    }

    /// Async wrapper for get_patch_state — see get_chunk_location_async for why
    /// this must go through spawn_blocking from request-handling code.
    pub async fn get_patch_state_async(self: &Arc<Self>, public_token: ChunkId) -> Result<Option<PatchState>> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.get_patch_state(&public_token))
            .await
            .context("spawn_blocking panicked in get_patch_state_async")?
    }

    /// Every `base_chunk_id` and `delta_chunk_id` referenced by a currently-Pending
    /// patch_state row, cluster-wide on this node. Used by handle_confirm_chunks_live
    /// and the healing discovery pass to keep both files alive while their fold is
    /// in flight — neither has a normal ChunkLocation/chunk_map/FILE_TABLE reference
    /// while Pending (see PATCH_STATE_TABLE's doc comment), so without this they'd
    /// look exactly like orphaned disk files to the usual liveness scans. Folded
    /// rows are excluded on purpose: their target already has its own, normally-
    /// registered ChunkLocation from the fold itself, so it's already protected by
    /// the ordinary liveness rules with no special-casing needed.
    pub fn all_pending_patch_chunk_ids(&self) -> Result<std::collections::HashSet<ChunkId>> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = match txn.open_table(PATCH_STATE_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(std::collections::HashSet::new()),
            Err(e) => return Err(e.into()),
        };
        let mut ids = std::collections::HashSet::new();
        for item in table.range::<&str>(..)? {
            let (_, v) = item?;
            let state = bincode::deserialize::<PatchState>(v.value())
                .context("Failed to deserialize patch state")?;
            if let PatchState::Pending { base_chunk_id, delta_chunk_id, .. } = state {
                ids.insert(base_chunk_id);
                ids.insert(delta_chunk_id);
            }
        }
        Ok(ids)
    }

    /// Async wrapper for all_pending_patch_chunk_ids — see get_chunk_location_async.
    pub async fn all_pending_patch_chunk_ids_async(self: &Arc<Self>) -> Result<std::collections::HashSet<ChunkId>> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.all_pending_patch_chunk_ids())
            .await
            .context("spawn_blocking panicked in all_pending_patch_chunk_ids_async")?
    }

    /// Every (file_id, chunk_idx, public_token) whose PATCH_STATE_TABLE row is
    /// still Pending — i.e. every slot with a fold that hasn't happened yet,
    /// found by walking PATCH_STATE_SLOT_TABLE (the only place the slot ->
    /// token mapping exists; PatchState::Pending itself carries no file_id/
    /// chunk_idx) and checking each token's current state.
    ///
    /// For server.rs's startup resume sweep: dirty_patch_slots (the in-memory
    /// map debounce_fold_slot and its retry loop key off of) is wiped on every
    /// process restart, so any Pending row that existed at the moment of a
    /// restart has zero live task tracking it afterward — orphaned
    /// indefinitely, since nothing else re-discovers a Pending row from the
    /// persisted table alone. Confirmed live (2026-07-11): 3 patches stuck
    /// Pending on a fully idle cluster with no client connected.
    pub fn all_pending_patch_slots(&self) -> Result<Vec<(FileId, u64, ChunkId)>> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let slot_table = match txn.open_table(PATCH_STATE_SLOT_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let state_table = match txn.open_table(PATCH_STATE_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };

        let mut out = Vec::new();
        for item in slot_table.range::<&str>(..)? {
            let (slot_key, token_bytes) = item?;
            let Some((file_id_str, chunk_idx_str)) = slot_key.value().rsplit_once(':') else { continue };
            let Ok(file_uuid) = uuid::Uuid::parse_str(file_id_str) else { continue };
            let Ok(chunk_idx) = chunk_idx_str.parse::<u64>() else { continue };
            let Ok(token_str) = std::str::from_utf8(token_bytes.value()) else { continue };
            let Some(token_hash) = decode_hex_32(token_str) else { continue };
            let token = ChunkId::from_hash(token_hash);

            if let Some(v) = state_table.get(token_str)? {
                if let Ok(PatchState::Pending { .. }) = bincode::deserialize::<PatchState>(v.value()) {
                    out.push((FileId::from_uuid(file_uuid), chunk_idx, token));
                }
            }
        }
        Ok(out)
    }

    /// Async wrapper for all_pending_patch_slots — see get_chunk_location_async.
    pub async fn all_pending_patch_slots_async(self: &Arc<Self>) -> Result<Vec<(FileId, u64, ChunkId)>> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.all_pending_patch_slots())
            .await
            .context("spawn_blocking panicked in all_pending_patch_slots_async")?
    }

    /// Every currently-outstanding public token — every PATCH_STATE_TABLE key,
    /// Pending or Folded. Used by the healing discovery pass to exclude tokens
    /// from its own "is this chunk under-replicated" classification entirely: a
    /// token never names a real file (Pending) or is a permanent alias to one
    /// living at a different identity (Folded) — either way, treating its own
    /// ChunkLocation entry as something to actively heal to full RF is
    /// meaningless. The table is tiny and short-lived-ish (bounded by distinct
    /// (file, chunk_idx) slots ever patched), so a full scan here — once per
    /// discovery pass, not per chunk — is cheap.
    pub fn all_patch_token_ids(&self) -> Result<std::collections::HashSet<ChunkId>> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = match txn.open_table(PATCH_STATE_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(std::collections::HashSet::new()),
            Err(e) => return Err(e.into()),
        };
        let mut ids = std::collections::HashSet::new();
        for item in table.range::<&str>(..)? {
            let (k, _) = item?;
            if let Some(hash) = decode_hex_32(k.value()) {
                ids.insert(ChunkId::from_hash(hash));
            }
        }
        Ok(ids)
    }

    /// Async wrapper for all_patch_token_ids — see get_chunk_location_async.
    pub async fn all_patch_token_ids_async(self: &Arc<Self>) -> Result<std::collections::HashSet<ChunkId>> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.all_patch_token_ids())
            .await
            .context("spawn_blocking panicked in all_patch_token_ids_async")?
    }

    /// Prune Folded PATCH_STATE_TABLE rows whose target chunk is either gone
    /// (already superseded/deleted — the token is useless regardless of age)
    /// or old enough that no realistic client cache could still reference the
    /// retired token. NEVER touches Pending rows — those are actively in use
    /// and are only ever removed by the normal retire-on-next-pending path in
    /// put_patch_state_pending.
    ///
    /// Root cause this closes (2026-07-11): once a (file, chunk_idx) slot goes
    /// quiet after its last patch folds, nothing ever removes its
    /// PATCH_STATE_TABLE row — the existing retire cleanup only fires when the
    /// *same* slot is patched again. Confirmed live: PATCH_STATE_TABLE growth
    /// accounted for a large share of a metadata store running 5x larger than
    /// expected after one heavy VM-install session (a 17GB file alone touches
    /// ~4300 chunk_idx slots, and most go quiet — never patched again — well
    /// before the file itself is deleted, if it ever is).
    ///
    /// Uses the target chunk's own ChunkLocation.written_at as the age proxy
    /// rather than adding a timestamp to PatchState::Folded itself — that would
    /// be a bincode wire-format change to an enum variant, riskier to roll out
    /// safely than reusing a timestamp that's already there.
    pub fn prune_stale_folded_patch_states(&self, min_age: std::time::Duration) -> Result<usize> {
        let now = std::time::SystemTime::now();
        let candidates: Vec<(String, ChunkId)> = {
            let _db = self.db.read();
            let txn = _db.begin_read()?;
            let table = match txn.open_table(PATCH_STATE_TABLE) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
                Err(e) => return Err(e.into()),
            };
            let mut out = Vec::new();
            for item in table.range::<&str>(..)? {
                let (k, v) = item?;
                if let Ok(PatchState::Folded(target)) = bincode::deserialize::<PatchState>(v.value()) {
                    out.push((k.value().to_string(), target));
                }
            }
            out
        };

        let mut to_remove = Vec::new();
        for (key, target) in candidates {
            let safe_to_remove = match self.get_chunk_location(&target)? {
                // Target already gone (superseded/deleted) — nothing can resolve
                // through this token correctly anymore regardless of age.
                None => true,
                Some(loc) => {
                    let file_gone = match loc.file_id {
                        Some(fid) => !self.file_exists_by_id(fid).unwrap_or(true),
                        None => false,
                    };
                    if file_gone {
                        true
                    } else {
                        match loc.written_at {
                            Some(ts_ms) => {
                                let written = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ts_ms);
                                now.duration_since(written).unwrap_or_default() >= min_age
                            }
                            // No timestamp to establish age from — leave it alone
                            // rather than guess.
                            None => false,
                        }
                    }
                }
            };
            if safe_to_remove {
                to_remove.push(key);
            }
        }

        if to_remove.is_empty() {
            return Ok(0);
        }

        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(PATCH_STATE_TABLE)?;
            for key in &to_remove {
                table.remove(key.as_str())?;
            }
        }
        txn.commit()?;
        self.note_txn("prune_stale_folded_patch_states", 0);
        Ok(to_remove.len())
    }

    /// Async wrapper for prune_stale_folded_patch_states — see get_chunk_location_async.
    pub async fn prune_stale_folded_patch_states_async(self: &Arc<Self>, min_age: std::time::Duration) -> Result<usize> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.prune_stale_folded_patch_states(min_age))
            .await
            .context("spawn_blocking panicked in prune_stale_folded_patch_states_async")?
    }

    /// Every chunk_id that is currently the durable *result* of a background
    /// fold — i.e. every `PatchState::Folded(target)` row's target, scanned
    /// straight from PATCH_STATE_TABLE. Same scan shape as
    /// `prune_stale_folded_patch_states` (just collecting targets instead of
    /// candidates for removal), used at startup to rebuild
    /// `Server::fold_result_chunk_ids` — the in-memory set that lets
    /// `location_supersedes` derive a chunk's fold-vs-client origin without a
    /// `ChunkLocation` wire field (see that field's doc comment for why).
    pub fn all_folded_chunk_ids(&self) -> Result<Vec<ChunkId>> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = match txn.open_table(PATCH_STATE_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for item in table.range::<&str>(..)? {
            let (_k, v) = item?;
            if let Ok(PatchState::Folded(target)) = bincode::deserialize::<PatchState>(v.value()) {
                out.push(target);
            }
        }
        Ok(out)
    }

    /// Total PATCH_STATE_TABLE row count still in the Pending state (not yet
    /// folded) — used for Response::HealingStatus's outstanding-patches gauge.
    pub fn count_pending_patch_entries(&self) -> Result<usize> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = match txn.open_table(PATCH_STATE_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut count = 0usize;
        for item in table.range::<&str>(..)? {
            let (_, v) = item?;
            let state = bincode::deserialize::<PatchState>(v.value())
                .context("Failed to deserialize patch state")?;
            if matches!(state, PatchState::Pending { .. }) {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Async wrapper for count_pending_patch_entries — see get_chunk_location_async.
    pub async fn count_pending_patch_entries_async(self: &Arc<Self>) -> Result<usize> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.count_pending_patch_entries())
            .await
            .context("spawn_blocking panicked in count_pending_patch_entries_async")?
    }

    // -------------------------------------------------------------------------
    // Pending healing (per-chunk debounce timer, survives process restart)
    // -------------------------------------------------------------------------

    /// Record that `chunk_id` was first observed as needing healing at
    /// `detected_at_secs` (unix seconds). Idempotent — callers should only
    /// write this once per detection (on first insert into the in-memory map).
    pub fn put_pending_healing(&self, chunk_id: &ChunkId, detected_at_secs: u64) -> Result<()> {
        let key = format!("{}", chunk_id);
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(PENDING_HEALING_TABLE)?;
            table.insert(key.as_str(), detected_at_secs)?;
        }
        txn.commit()?;
        self.note_txn("put_pending_healing", 0);
        Ok(())
    }

    /// Clear the persisted debounce timer for `chunk_id` (chunk reached RF, or
    /// was purged).
    pub fn delete_pending_healing(&self, chunk_id: &ChunkId) -> Result<()> {
        let key = format!("{}", chunk_id);
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(PENDING_HEALING_TABLE)?;
            table.remove(key.as_str())?;
        }
        txn.commit()?;
        self.note_txn("delete_pending_healing", 0);
        Ok(())
    }

    /// Async wrapper for put_pending_healing — fires once per chunk at the start
    /// of a heal storm, exactly the burst scenario that starved gluster1; see
    /// put_chunk_location_async.
    pub async fn put_pending_healing_async(self: &Arc<Self>, chunk_id: ChunkId, detected_at_secs: u64) -> Result<()> {
        if !Self::group_commit_enabled() {
            let store = Arc::clone(self);
            return tokio::task::spawn_blocking(move || store.put_pending_healing(&chunk_id, detected_at_secs))
                .await
                .context("spawn_blocking panicked in put_pending_healing_async")?;
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.committer_tx().send(MetaWriteOp::PutPendingHealing { chunk_id, detected_at_secs, reply }).await
            .map_err(|_| anyhow::anyhow!("metadata group-commit thread is gone"))?;
        rx.await.context("metadata group-commit thread dropped its reply")?
    }

    /// Record first-detection times for multiple chunks in one write transaction.
    /// Same idempotency contract as `put_pending_healing` per entry — callers should
    /// only include (chunk_id, detected_at_secs) pairs they intend to write, this
    /// function doesn't check for existing entries. Use this instead of looping
    /// `put_pending_healing`/`put_pending_healing_async` any time more than one
    /// chunk needs marking at once (discovery-pass classification, immediate-heal
    /// backdating) — see `put_chunk_locations_batch`'s doc comment for why N
    /// single-record transactions cost far more than one N-record transaction.
    pub fn put_pending_healing_batch(&self, entries: &[(ChunkId, u64)]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        Self::put_pending_healing_batch_in_txn(&txn, entries)?;
        txn.commit()?;
        self.note_txn("put_pending_healing_batch", 0);
        Ok(())
    }

    /// Body of put_pending_healing_batch against an already-open write
    /// transaction — shared with the group-commit committer.
    fn put_pending_healing_batch_in_txn(txn: &redb::WriteTransaction, entries: &[(ChunkId, u64)]) -> Result<()> {
        let mut table = txn.open_table(PENDING_HEALING_TABLE)?;
        for (chunk_id, detected_at_secs) in entries {
            let key = format!("{}", chunk_id);
            table.insert(key.as_str(), *detected_at_secs)?;
        }
        Ok(())
    }

    /// Async wrapper for put_pending_healing_batch — see put_chunk_location_async
    /// (group-committed since 2026-07-15: queue_chunks_immediate submits
    /// single-entry batches once per below-RF write, so these coalesce too).
    pub async fn put_pending_healing_batch_async(self: &Arc<Self>, entries: Vec<(ChunkId, u64)>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        if !Self::group_commit_enabled() {
            let store = Arc::clone(self);
            return tokio::task::spawn_blocking(move || store.put_pending_healing_batch(&entries))
                .await
                .context("spawn_blocking panicked in put_pending_healing_batch_async")?;
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.committer_tx().send(MetaWriteOp::PutPendingHealingBatch { entries, reply }).await
            .map_err(|_| anyhow::anyhow!("metadata group-commit thread is gone"))?;
        rx.await.context("metadata group-commit thread dropped its reply")?
    }

    /// Async wrapper for delete_pending_healing — see put_chunk_location_async.
    pub async fn delete_pending_healing_async(self: &Arc<Self>, chunk_id: ChunkId) -> Result<()> {
        if !Self::group_commit_enabled() {
            let store = Arc::clone(self);
            return tokio::task::spawn_blocking(move || store.delete_pending_healing(&chunk_id))
                .await
                .context("spawn_blocking panicked in delete_pending_healing_async")?;
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.committer_tx().send(MetaWriteOp::DeletePendingHealing { chunk_id, reply }).await
            .map_err(|_| anyhow::anyhow!("metadata group-commit thread is gone"))?;
        rx.await.context("metadata group-commit thread dropped its reply")?
    }

    /// Read all persisted (chunk_id, first_detected_at_secs) entries. Used at
    /// HealingManager startup to seed the in-memory pending_healing map so the
    /// healing_delay_secs debounce reflects time elapsed before this process
    /// started, not just since this process started.
    pub fn get_pending_healing_inventory(&self) -> Result<Vec<(ChunkId, u64)>> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(PENDING_HEALING_TABLE)?;
        let mut out = Vec::new();
        for item in table.range::<&str>(..)? {
            let (k, v) = item?;
            if let Some(hash) = decode_hex_32(k.value()) {
                out.push((ChunkId::from_hash(hash), v.value()));
            }
        }
        Ok(out)
    }

    /// Batch apply chunk location updates — all puts, deletes, AND pending-healing
    /// clears in one write transaction. Use this from async code via `spawn_blocking`
    /// to avoid blocking Tokio worker threads on redb's exclusive write lock.
    ///
    /// `pending_healing_deletes` folds in what used to be a separate per-chunk
    /// `delete_pending_healing` transaction (called from `clear_pending_static` once
    /// per healed chunk) into this same commit — heal completion, the routing-table
    /// update, and the debounce-timer clear are one atomic unit of work, not three.
    /// Same key format as `delete_pending_healing`/`put_pending_healing`
    /// (`format!("{}", chunk_id)`), same table (PENDING_HEALING_TABLE).
    pub fn batch_update_chunk_locations(
        &self,
        puts: &[ChunkLocation],
        deletes: &[ChunkId],
        pending_healing_deletes: &[ChunkId],
    ) -> Result<()> {
        if puts.is_empty() && deletes.is_empty() && pending_healing_deletes.is_empty() {
            return Ok(());
        }
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        let payload_bytes = Self::apply_chunk_location_updates_in_txn(&txn, puts, deletes, pending_healing_deletes)?;
        txn.commit()?;
        self.note_txn("batch_update_chunk_locations", payload_bytes);
        Ok(())
    }

    /// Body of batch_update_chunk_locations against an already-open write
    /// transaction — shared by the standalone functions and the group-commit
    /// committer (apply_ops_group). Returns serialized payload bytes written.
    fn apply_chunk_location_updates_in_txn(
        txn: &redb::WriteTransaction,
        puts: &[ChunkLocation],
        deletes: &[ChunkId],
        pending_healing_deletes: &[ChunkId],
    ) -> Result<usize> {
        let mut payload_bytes: usize = 0;
        if !puts.is_empty() || !deletes.is_empty() {
            let mut table = txn.open_table(CHUNK_TABLE)?;
            for location in puts {
                let key = format!("{}", location.chunk_id);
                let value = bincode::serialize(location)
                    .context("Failed to serialize chunk location")?;
                payload_bytes += value.len();
                table.insert(key.as_str(), value.as_slice())?;
            }
            for chunk_id in deletes {
                let key = format!("{}", chunk_id);
                table.remove(key.as_str())?;
            }
        }
        if !pending_healing_deletes.is_empty() {
            let mut table = txn.open_table(PENDING_HEALING_TABLE)?;
            for chunk_id in pending_healing_deletes {
                let key = format!("{}", chunk_id);
                table.remove(key.as_str())?;
            }
        }
        Ok(payload_bytes)
    }

    /// Async wrapper for batch_update_chunk_locations — see put_chunk_location_async
    /// (group-committed since 2026-07-15, same coalescing rationale as
    /// put_chunk_locations_batch_async).
    pub async fn batch_update_chunk_locations_async(
        self: &Arc<Self>,
        puts: Vec<ChunkLocation>,
        deletes: Vec<ChunkId>,
        pending_healing_deletes: Vec<ChunkId>,
    ) -> Result<()> {
        if puts.is_empty() && deletes.is_empty() && pending_healing_deletes.is_empty() {
            return Ok(());
        }
        if !Self::group_commit_enabled() {
            let store = Arc::clone(self);
            return tokio::task::spawn_blocking(move || {
                store.batch_update_chunk_locations(&puts, &deletes, &pending_healing_deletes)
            })
                .await
                .context("spawn_blocking panicked in batch_update_chunk_locations_async")?;
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.committer_tx().send(MetaWriteOp::UpdateChunkLocationsBatch {
            puts, deletes, pending_healing_deletes, reply,
        }).await.map_err(|_| anyhow::anyhow!("metadata group-commit thread is gone"))?;
        rx.await.context("metadata group-commit thread dropped its reply")?
    }

    /// List all chunk IDs known in metadata.
    pub fn list_all_chunk_ids(&self) -> Result<Vec<ChunkId>> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(CHUNK_TABLE)?;
        let mut ids = Vec::new();
        for item in table.range::<&str>(..)? {
            let (_, v) = item?;
            if let Ok(loc) = bincode::deserialize::<ChunkLocation>(v.value()) {
                ids.push(loc.chunk_id);
            }
        }
        Ok(ids)
    }

    /// Return all chunk location records in one scan.
    pub fn list_all_chunk_locations(&self) -> Result<Vec<ChunkLocation>> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(CHUNK_TABLE)?;
        let mut locations = Vec::new();
        for item in table.range::<&str>(..)? {
            let (_, v) = item?;
            if let Ok(loc) = bincode::deserialize::<ChunkLocation>(v.value()) {
                locations.push(loc);
            }
        }
        Ok(locations)
    }

    /// Stream chunk location records, calling `f` for each. Return `false` to stop early.
    pub fn scan_chunk_locations<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(ChunkLocation) -> bool,
    {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(CHUNK_TABLE)?;
        for item in table.range::<&str>(..)? {
            let (_, v) = item?;
            if let Ok(loc) = bincode::deserialize::<ChunkLocation>(v.value()) {
                if !f(loc) {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Build the set of chunk IDs referenced by any live file in metadata.
    ///
    /// Used by orphan/heal-scan callers (healing.rs) to decide whether a chunk is
    /// still needed — a chunk this wrongly omits looks orphaned and can be purged,
    /// so this must never silently under-report. Derives from CHUNK_TABLE (each
    /// ChunkLocation's own `file_id` field) cross-referenced against the set of
    /// currently-existing file_ids, NOT FileMetadata.chunk_locations — that's never
    /// populated on disk anymore (see put_file_in_txn's doc comment); reading it
    /// here would make every chunk in the cluster look orphaned regardless of
    /// whether its file is actually live. Cross-referencing against live file_ids
    /// (rather than "any chunk with a file_id tag") matters because CHUNK_TABLE
    /// isn't pruned the instant a file is deleted — that's exactly what the orphan
    /// sweep this feeds is for; a stale file_id tag on an otherwise-orphaned chunk
    /// must not count as "live".
    pub fn live_chunk_ids(&self) -> Result<std::collections::HashSet<ChunkId>> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let mut live_file_ids: std::collections::HashSet<FileId> = std::collections::HashSet::new();
        {
            let table = txn.open_table(FILE_TABLE)?;
            for item in table.range::<&str>(..)? {
                let (_, v) = item?;
                if let Ok(m) = dfs_common::deserialize_file_metadata(v.value()) {
                    live_file_ids.insert(m.id);
                }
            }
        }
        let mut live = std::collections::HashSet::new();
        {
            let table = txn.open_table(CHUNK_TABLE)?;
            for item in table.range::<&str>(..)? {
                let (_, v) = item?;
                if let Ok(loc) = bincode::deserialize::<ChunkLocation>(v.value()) {
                    if loc.file_id.is_some_and(|fid| live_file_ids.contains(&fid)) {
                        live.insert(loc.chunk_id);
                    }
                }
            }
        }
        Ok(live)
    }

    /// Rebuild missing chunk: routing table entries from file metadata.
    pub fn rebuild_chunk_locations_from_files(&self) -> Result<(usize, usize)> {
        // Collect missing chunk records (read phase).
        let to_write: Vec<(String, Vec<u8>)> = {
            let _db = self.db.read();
            let txn = _db.begin_read()?;
            let file_table = txn.open_table(FILE_TABLE)?;
            let chunk_table = txn.open_table(CHUNK_TABLE)?;
            let mut missing = Vec::new();
            for item in file_table.range::<&str>(..)? {
                let (_, v) = item?;
                let m = match dfs_common::deserialize_file_metadata(v.value()) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                for loc in m.chunk_locations.iter() {
                    let key = format!("{}", loc.chunk_id);
                    if chunk_table.get(key.as_str())?.is_none() {
                        let bytes = bincode::serialize(loc)
                            .context("Failed to serialize ChunkLocation during rebuild")?;
                        missing.push((key, bytes));
                    }
                }
            }
            missing
        };

        let written = to_write.len();
        if !to_write.is_empty() {
            let _db = self.db.read();
            let mut txn = _db.begin_write()?;
            txn.set_durability(self.next_write_durability());
            {
                let mut table = txn.open_table(CHUNK_TABLE)?;
                for (key, bytes) in &to_write {
                    warn!("Rebuilt missing chunk record for {}", key);
                    table.insert(key.as_str(), bytes.as_slice())?;
                }
            }
            txn.commit()?;
            let payload_bytes: usize = to_write.iter().map(|(_, b)| b.len()).sum();
            self.note_txn("rebuild_chunk_locations_from_files", payload_bytes);
        }

        // Count already-present entries (skipped).
        let skipped = {
            let _db = self.db.read();
            let txn = _db.begin_read()?;
            let table = txn.open_table(CHUNK_TABLE)?;
            let total: usize = table.range::<&str>(..)?.count();
            total.saturating_sub(written)
        };

        if written > 0 {
            info!(
                "Chunk location rebuild: {} missing records restored, {} already present",
                written, skipped
            );
        }
        Ok((written, skipped))
    }

    // -------------------------------------------------------------------------
    // Leader metadata sequence & dissemination queue
    // -------------------------------------------------------------------------

    /// Increment and return the next metadata sequence number (leader-only).
    pub fn next_meta_sequence(&self) -> Result<u64> {
        let _db = self.db.read();
        let txn = _db.begin_write()?;
        let next = {
            let mut table = txn.open_table(COUNTERS_TABLE)?;
            let current = table.get("meta_seq")?.map(|v| v.value()).unwrap_or(0);
            let next = current + 1;
            table.insert("meta_seq", next)?;
            next
        };
        txn.commit()?;
        self.note_txn("next_meta_sequence", 0);
        Ok(next)
    }

    /// Async wrapper for next_meta_sequence — see put_chunk_location_async. There's
    /// a known case (enqueue_metadata_for_followers in server.rs) where calling this
    /// directly was already observed to block worker threads under a write storm
    /// (3500 calls/sec during an rsync of thousands of files); it was patched around
    /// by skipping the call when there are no offline followers, but the direct call
    /// remains unwrapped for the case where there IS at least one offline follower —
    /// exactly when healing/dissemination load is also highest.
    pub async fn next_meta_sequence_async(self: &Arc<Self>) -> Result<u64> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.next_meta_sequence())
            .await
            .context("spawn_blocking panicked in next_meta_sequence_async")?
    }

    /// Read current metadata sequence number.
    pub fn current_meta_sequence(&self) -> Result<u64> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(COUNTERS_TABLE)?;
        Ok(table.get("meta_seq")?.map(|v| v.value()).unwrap_or(0))
    }

    /// Persist which node most recently became cluster leader, and the unix
    /// epoch second its current leadership episode began. On the next startup,
    /// if this node becomes leader again and matches the persisted NodeId, the
    /// grace period (LEADER_CHANGE_GRACE_SECS) carries over from `since_secs`
    /// instead of restarting — see `cluster::resolve_became_leader_epoch`.
    pub fn put_leader_state(&self, node_id: NodeId, since_secs: u64) -> Result<()> {
        let bytes = node_id.as_bytes();
        let hi = u64::from_be_bytes(bytes[0..8].try_into().unwrap());
        let lo = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        {
            let mut table = txn.open_table(COUNTERS_TABLE)?;
            table.insert("leader_state_hi", hi)?;
            table.insert("leader_state_lo", lo)?;
            table.insert("leader_state_since_secs", since_secs)?;
        }
        txn.commit()?;
        self.note_txn("put_leader_state", 0);
        Ok(())
    }

    /// Read back the last-persisted leader NodeId and its leadership-episode
    /// start time (unix seconds), if any has ever been recorded.
    pub fn get_leader_state(&self) -> Result<(Option<NodeId>, Option<u64>)> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(COUNTERS_TABLE)?;
        let hi = table.get("leader_state_hi")?.map(|v| v.value());
        let lo = table.get("leader_state_lo")?.map(|v| v.value());
        let since = table.get("leader_state_since_secs")?.map(|v| v.value());
        let node_id = match (hi, lo) {
            (Some(h), Some(l)) => {
                let mut bytes = [0u8; 16];
                bytes[0..8].copy_from_slice(&h.to_be_bytes());
                bytes[8..16].copy_from_slice(&l.to_be_bytes());
                Some(NodeId::from_bytes(bytes))
            }
            _ => None,
        };
        Ok((node_id, since))
    }

    /// Format node_id as a fixed-length hex string for use as a key prefix.
    fn node_id_hex(node_id: NodeId) -> String {
        node_id.as_bytes().iter().map(|b| format!("{:02x}", b)).collect()
    }

    /// Enqueue a metadata update destined for `node_id` at `sequence`.
    pub fn enqueue_meta_for_node(
        &self,
        node_id: NodeId,
        sequence: u64,
        metadata: &FileMetadata,
    ) -> Result<()> {
        let node_hex = Self::node_id_hex(node_id);
        let file_id_hex = metadata.id.0.as_simple().to_string();
        let idx_key = format!("{}:{}", node_hex, file_id_hex);

        let _db = self.db.read();
        let txn = _db.begin_write()?;
        let payload_bytes;
        {
            let mut queue_table = txn.open_table(META_QUEUE_TABLE)?;
            let mut idx_table = txn.open_table(META_QUEUE_IDX)?;

            // Remove any existing queue entry for this (node, file) pair.
            if let Some(old_seq_bytes) = idx_table.get(idx_key.as_str())? {
                if old_seq_bytes.value().len() == 8 {
                    let old_seq = u64::from_be_bytes(
                        old_seq_bytes.value().try_into().unwrap()
                    );
                    let old_key = format!("{}:{:016x}", node_hex, old_seq);
                    queue_table.remove(old_key.as_str())?;
                }
            }

            let queue_key = format!("{}:{:016x}", node_hex, sequence);
            let value = bincode::serialize(metadata)
                .context("Failed to serialize metadata for queue")?;
            payload_bytes = value.len();
            queue_table.insert(queue_key.as_str(), value.as_slice())?;
            idx_table.insert(idx_key.as_str(), sequence.to_be_bytes().as_slice())?;
        }
        txn.commit()?;
        self.note_txn("enqueue_meta_for_node", payload_bytes);
        Ok(())
    }

    /// Async wrapper for enqueue_meta_for_node — see put_chunk_location_async.
    pub async fn enqueue_meta_for_node_async(
        self: &Arc<Self>,
        node_id: NodeId,
        sequence: u64,
        metadata: FileMetadata,
    ) -> Result<()> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.enqueue_meta_for_node(node_id, sequence, &metadata))
            .await
            .context("spawn_blocking panicked in enqueue_meta_for_node_async")?
    }

    /// Return all queued metadata entries for `node_id`, in sequence order.
    pub fn drain_meta_queue_for_node(
        &self,
        node_id: NodeId,
    ) -> Result<Vec<(u64, FileMetadata)>> {
        let node_hex = Self::node_id_hex(node_id);
        let prefix = format!("{}:", node_hex);
        let prefix_end = prefix_next(&prefix);

        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(META_QUEUE_TABLE)?;
        let mut items = Vec::new();
        for item in table.range(prefix.as_str()..prefix_end.as_str())? {
            let (k, v) = item?;
            let key_str = k.value();
            let seq_hex = key_str.split(':').nth(1).unwrap_or("0");
            let seq = u64::from_str_radix(seq_hex, 16).unwrap_or(0);
            match dfs_common::deserialize_file_metadata(v.value()) {
                Ok(m) => items.push((seq, m)),
                Err(e) => warn!("meta_queue: failed to deserialize entry {}: {}", key_str, e),
            }
        }
        Ok(items)
    }

    /// Remove all queue entries for `node_id` up to and including `up_to_sequence`.
    pub fn ack_meta_queue_for_node(&self, node_id: NodeId, up_to_sequence: u64) -> Result<()> {
        let node_hex = Self::node_id_hex(node_id);
        let prefix = format!("{}:", node_hex);
        let prefix_end = prefix_next(&prefix);
        let up_to_key = format!("{}:{:016x}", node_hex, up_to_sequence);

        // Collect keys to remove (read phase — no mutation during iteration).
        let to_remove: Vec<(String, Option<String>)> = {
            let _db = self.db.read();
            let txn = _db.begin_read()?;
            let table = txn.open_table(META_QUEUE_TABLE)?;
            let mut items = Vec::new();
            for item in table.range(prefix.as_str()..prefix_end.as_str())? {
                let (k, v) = item?;
                let key_str = k.value();
                if key_str > up_to_key.as_str() {
                    break;
                }
                let idx_key = dfs_common::deserialize_file_metadata(v.value())
                    .ok()
                    .map(|m| format!("{}:{}", node_hex, m.id.0.as_simple()));
                items.push((key_str.to_string(), idx_key));
            }
            items
        };

        if to_remove.is_empty() {
            return Ok(());
        }

        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut queue_table = txn.open_table(META_QUEUE_TABLE)?;
            let mut idx_table = txn.open_table(META_QUEUE_IDX)?;

            for (queue_key, idx_key_opt) in &to_remove {
                if let Some(idx_key) = idx_key_opt {
                    // Extract idx_seq before any mutable borrow of idx_table.
                    let idx_seq_opt: Option<u64> = match idx_table.get(idx_key.as_str())? {
                        Some(v) if v.value().len() == 8 => {
                            Some(u64::from_be_bytes(v.value().try_into().unwrap()))
                        }
                        _ => None,
                    };
                    if let Some(idx_seq) = idx_seq_opt {
                        let seq_hex = queue_key.split(':').nth(1).unwrap_or("0");
                        let seq = u64::from_str_radix(seq_hex, 16).unwrap_or(0);
                        if idx_seq == seq {
                            idx_table.remove(idx_key.as_str())?;
                        }
                    }
                }
                queue_table.remove(queue_key.as_str())?;
            }
        }
        txn.commit()?;
        self.note_txn("ack_meta_queue_for_node", 0);
        Ok(())
    }

    /// Deduplication pass: retain only the last entry per FileId for `node_id`.
    pub fn compact_meta_queue_for_node(&self, node_id: NodeId) -> Result<()> {
        let node_hex = Self::node_id_hex(node_id);
        let prefix = format!("{}:", node_hex);
        let prefix_end = prefix_next(&prefix);

        // Read all entries first.
        let entries: Vec<(String, u64, FileId)> = {
            let _db = self.db.read();
            let txn = _db.begin_read()?;
            let table = txn.open_table(META_QUEUE_TABLE)?;
            let mut result = Vec::new();
            for item in table.range(prefix.as_str()..prefix_end.as_str())? {
                let (k, v) = item?;
                let key_str = k.value();
                let seq_hex = key_str.split(':').nth(1).unwrap_or("0");
                let seq = u64::from_str_radix(seq_hex, 16).unwrap_or(0);
                if let Ok(m) = dfs_common::deserialize_file_metadata(v.value()) {
                    result.push((key_str.to_string(), seq, m.id));
                }
            }
            result
        };

        // Determine which keys to remove (keep highest seq per FileId).
        let mut seen: std::collections::HashMap<FileId, (String, u64)> =
            std::collections::HashMap::new();
        let mut to_remove: Vec<String> = Vec::new();
        for (key, seq, file_id) in entries {
            let entry = seen.entry(file_id).or_insert((key.clone(), seq));
            if seq > entry.1 {
                to_remove.push(entry.0.clone());
                *entry = (key, seq);
            } else if seq < entry.1 {
                to_remove.push(key);
            }
        }

        if !to_remove.is_empty() {
            let _db = self.db.read();
            let mut txn = _db.begin_write()?;
            txn.set_durability(self.next_write_durability());
            {
                let mut table = txn.open_table(META_QUEUE_TABLE)?;
                for key in &to_remove {
                    table.remove(key.as_str())?;
                }
            }
            txn.commit()?;
            self.note_txn("compact_meta_queue_for_node", 0);
        }
        Ok(())
    }

    /// Record the last sequence number received from the leader (follower-only).
    pub fn set_follower_sequence(&self, seq: u64) -> Result<()> {
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(COUNTERS_TABLE)?;
            table.insert("follower_seq", seq)?;
        }
        txn.commit()?;
        self.note_txn("set_follower_sequence", 0);
        Ok(())
    }

    /// Async wrapper for set_follower_sequence — called once per incoming
    /// dissemination batch on every follower; see put_chunk_location_async.
    pub async fn set_follower_sequence_async(self: &Arc<Self>, seq: u64) -> Result<()> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.set_follower_sequence(seq))
            .await
            .context("spawn_blocking panicked in set_follower_sequence_async")?
    }

    /// Get the last sequence number received from the leader (follower-only).
    pub fn get_follower_sequence(&self) -> Result<u64> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(COUNTERS_TABLE)?;
        Ok(table.get("follower_seq")?.map(|v| v.value()).unwrap_or(0))
    }

    /// Return a compact inventory of all known files: Vec<(FileId, write_seq)>.
    /// write_seq (not modified_at) so catchup/healing comparisons are clock-agnostic —
    /// modified_at is user-settable (setattr/utimes) and not safe for ordering.
    pub fn get_file_inventory(&self) -> Result<Vec<(FileId, u64)>> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(FILE_TABLE)?;
        let mut out = Vec::new();
        for item in table.range::<&str>(..)? {
            let (_, v) = item?;
            if let Ok(m) = dfs_common::deserialize_file_metadata(v.value()) {
                out.push((m.id, m.write_seq));
            }
        }
        Ok(out)
    }

    /// Fetch a batch of file records by ID. Missing IDs are silently skipped.
    pub fn get_files_batch(&self, ids: &[FileId]) -> Result<Vec<FileMetadata>> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(FILE_TABLE)?;
        let mut out = Vec::new();
        for id in ids {
            let key = format!("{}", id);
            if let Some(v) = table.get(key.as_str())? {
                if let Ok(m) = dfs_common::deserialize_file_metadata(v.value()) {
                    out.push(m);
                }
            }
        }
        Ok(out)
    }

    /// Scan all file records, calling `f` for each deserialized FileMetadata.
    pub fn scan_all_files<F>(&self, mut f: F) -> Result<usize>
    where
        F: FnMut(FileMetadata) -> Result<()>,
    {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(FILE_TABLE)?;
        let mut count = 0usize;
        for item in table.range::<&str>(..)? {
            let (_, v) = item?;
            if let Ok(m) = dfs_common::deserialize_file_metadata(v.value()) {
                f(m)?;
                count += 1;
            }
        }
        Ok(count)
    }

    // -------------------------------------------------------------------------
    // Delete queue
    // -------------------------------------------------------------------------

    /// Enqueue a pending deletion. Written BEFORE metadata is removed so the
    /// chunk list is never lost even on a crash mid-delete.
    pub fn enqueue_delete(&self, entry: &dfs_common::DeleteQueueEntry) -> Result<()> {
        let key = format!("del:{}", entry.file_id);
        let value = bincode::serialize(entry)
            .context("Failed to serialize DeleteQueueEntry")?;
        let _db = self.db.read();
        let txn = _db.begin_write()?;
        {
            let mut table = txn.open_table(DELETE_QUEUE_TABLE)?;
            table.insert(key.as_str(), value.as_slice())?;
        }
        txn.commit()?;
        self.note_txn("enqueue_delete", value.len());
        Ok(())
    }

    /// Async wrapper for enqueue_delete — see put_chunk_location_async.
    pub async fn enqueue_delete_async(self: &Arc<Self>, entry: dfs_common::DeleteQueueEntry) -> Result<()> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.enqueue_delete(&entry))
            .await
            .context("spawn_blocking panicked in enqueue_delete_async")?
    }

    /// Remove a completed deletion from the queue (called after all nodes ack).
    pub fn dequeue_delete(&self, file_id: &FileId) -> Result<()> {
        let key = format!("del:{}", file_id);
        let _db = self.db.read();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(DELETE_QUEUE_TABLE)?;
            table.remove(key.as_str())?;
        }
        txn.commit()?;
        self.note_txn("dequeue_delete", 0);
        Ok(())
    }

    /// Async wrapper for dequeue_delete — see put_chunk_location_async.
    pub async fn dequeue_delete_async(self: &Arc<Self>, file_id: FileId) -> Result<()> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.dequeue_delete(&file_id))
            .await
            .context("spawn_blocking panicked in dequeue_delete_async")?
    }

    /// Return all pending delete queue entries.
    pub fn get_all_pending_deletes(&self) -> Result<Vec<dfs_common::DeleteQueueEntry>> {
        let _db = self.db.read();
        let txn = _db.begin_read()?;
        let table = txn.open_table(DELETE_QUEUE_TABLE)?;
        let mut entries = Vec::new();
        for item in table.range::<&str>(..)? {
            let (_, v) = item?;
            match bincode::deserialize::<dfs_common::DeleteQueueEntry>(v.value()) {
                Ok(e) => entries.push(e),
                Err(e) => warn!("Skipping corrupt delete queue entry: {}", e),
            }
        }
        Ok(entries)
    }

    // -------------------------------------------------------------------------
    // Misc
    // -------------------------------------------------------------------------

    /// Bytes-valued tables copied/diffed as a unit by compact_db()'s shadow-copy pass.
    const BYTES_TABLES: [TableDefinition<'static, &'static str, &'static [u8]>; 8] = [
        FILE_TABLE, PATH_TABLE, CHUNK_TABLE, META_QUEUE_TABLE, META_QUEUE_IDX,
        DELETE_QUEUE_TABLE, PATCH_STATE_TABLE, PATCH_STATE_SLOT_TABLE,
    ];

    /// u64-valued tables copied/diffed as a unit by compact_db()'s shadow-copy pass.
    const U64_TABLES: [TableDefinition<'static, &'static str, u64>; 3] =
        [COUNTERS_TABLE, PENDING_HEALING_TABLE, CHUNK_REFCOUNT_TABLE];

    /// BYTES_TABLES minus FILE_TABLE/PATH_TABLE — the tables diff_all_tables_tracked
    /// still diffs via a full scan, since only FILE_TABLE/PATH_TABLE have dirty-key
    /// tracking (see dirty_files/dirty_paths). Individually much smaller per-row than
    /// full serialized FileMetadata blobs, so their O(size) cost isn't the bottleneck.
    const OTHER_BYTES_TABLES: [TableDefinition<'static, &'static str, &'static [u8]>; 6] = [
        CHUNK_TABLE, META_QUEUE_TABLE, META_QUEUE_IDX, DELETE_QUEUE_TABLE,
        PATCH_STATE_TABLE, PATCH_STATE_SLOT_TABLE,
    ];

    /// Copy every row of `def` from `src` into `dst`, overwriting whatever's there.
    /// Used for compact_db()'s initial full snapshot copy.
    /// How many rows between deadline checks in copy_bytes_table/copy_u64_table — see
    /// copy_all_tables' doc comment for why a per-table-only check isn't enough. Small
    /// enough that even a slow-disk row still checks the deadline within a fraction of
    /// a second of it passing; large enough that Instant::now() isn't called on every
    /// single row of a multi-million-row table.
    const COPY_DEADLINE_CHECK_INTERVAL: usize = 2048;

    /// Returns `false` (and leaves `dst_table` partially populated — safe because the
    /// caller's whole dst_txn is dropped, never committed, on that outcome) if `deadline`
    /// passes before every row is copied. See copy_all_tables' doc comment for why this
    /// intra-table check exists on top of copy_all_tables' own between-tables one.
    fn copy_bytes_table(
        src: &redb::ReadTransaction,
        dst: &redb::WriteTransaction,
        def: TableDefinition<&str, &[u8]>,
        deadline: std::time::Instant,
    ) -> Result<bool> {
        // Some tables (e.g. chunk_refcount) aren't pre-created at startup and only come
        // into existence on their first real write — a fresh/lightly-used store may
        // never have touched one. redb's read-side open_table errors on a table that
        // was never created (unlike the write side, which auto-creates); treat that as
        // "nothing to copy" rather than a real failure.
        let src_table = match src.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(true),
            Err(e) => return Err(e.into()),
        };
        let mut dst_table = dst.open_table(def)?;
        for (i, item) in src_table.range::<&str>(..)?.enumerate() {
            if i % Self::COPY_DEADLINE_CHECK_INTERVAL == 0 && std::time::Instant::now() >= deadline {
                return Ok(false);
            }
            let (k, v) = item?;
            dst_table.insert(k.value(), v.value())?;
        }
        Ok(true)
    }

    fn copy_u64_table(
        src: &redb::ReadTransaction,
        dst: &redb::WriteTransaction,
        def: TableDefinition<&str, u64>,
        deadline: std::time::Instant,
    ) -> Result<bool> {
        let src_table = match src.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(true),
            Err(e) => return Err(e.into()),
        };
        let mut dst_table = dst.open_table(def)?;
        for (i, item) in src_table.range::<&str>(..)?.enumerate() {
            if i % Self::COPY_DEADLINE_CHECK_INTERVAL == 0 && std::time::Instant::now() >= deadline {
                return Ok(false);
            }
            let (k, v) = item?;
            dst_table.insert(k.value(), v.value())?;
        }
        Ok(true)
    }

    /// Reconcile `dst`'s copy of `def` against `src`'s current contents: update any key
    /// whose value differs, insert any key missing from `dst`, and remove any key in
    /// `dst` that's no longer present in `src` (covers updates, inserts, and deletes in
    /// one pass). Returns the number of rows changed. Used by compact_db()'s catch-up
    /// passes to bring the shadow copy current with writes that landed during/after the
    /// initial snapshot copy — schema-agnostic by design (compares raw bytes, never
    /// needs to know what a table's values mean), so a future table needs no special
    /// handling here beyond being added to BYTES_TABLES/U64_TABLES.
    fn diff_bytes_table(
        src: &redb::ReadTransaction,
        dst: &redb::WriteTransaction,
        def: TableDefinition<&str, &[u8]>,
    ) -> Result<usize> {
        let src_table = match src.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut dst_table = dst.open_table(def)?;
        let mut changed = 0usize;
        let mut live_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        for item in src_table.range::<&str>(..)? {
            let (k, v) = item?;
            let key = k.value().to_string();
            live_keys.insert(key.clone());
            let needs_update = match dst_table.get(key.as_str())? {
                Some(existing) => existing.value() != v.value(),
                None => true,
            };
            if needs_update {
                dst_table.insert(key.as_str(), v.value())?;
                changed += 1;
            }
        }
        let stale_keys: Vec<String> = dst_table.range::<&str>(..)?
            .map(|item| item.map(|(k, _)| k.value().to_string()))
            .collect::<std::result::Result<_, _>>()?;
        for key in stale_keys {
            if !live_keys.contains(&key) {
                dst_table.remove(key.as_str())?;
                changed += 1;
            }
        }
        Ok(changed)
    }

    fn diff_u64_table(
        src: &redb::ReadTransaction,
        dst: &redb::WriteTransaction,
        def: TableDefinition<&str, u64>,
    ) -> Result<usize> {
        let src_table = match src.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut dst_table = dst.open_table(def)?;
        let mut changed = 0usize;
        let mut live_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        for item in src_table.range::<&str>(..)? {
            let (k, v) = item?;
            let key = k.value().to_string();
            live_keys.insert(key.clone());
            let needs_update = match dst_table.get(key.as_str())? {
                Some(existing) => existing.value() != v.value(),
                None => true,
            };
            if needs_update {
                dst_table.insert(key.as_str(), v.value())?;
                changed += 1;
            }
        }
        let stale_keys: Vec<String> = dst_table.range::<&str>(..)?
            .map(|item| item.map(|(k, _)| k.value().to_string()))
            .collect::<std::result::Result<_, _>>()?;
        for key in stale_keys {
            if !live_keys.contains(&key) {
                dst_table.remove(key.as_str())?;
                changed += 1;
            }
        }
        Ok(changed)
    }

    /// Copies every table, checking `deadline` both between tables and every
    /// COPY_DEADLINE_CHECK_INTERVAL rows within one (see copy_bytes_table/
    /// copy_u64_table). Returns `false` (nothing committed by the caller's dst_txn
    /// beyond what was already inserted) if the deadline hit before all tables were
    /// copied — see compact_db_prepare's Phase 1 budget for why this matters.
    ///
    /// Root-caused 2026-07-15 (gluster1 incident): the between-tables-only check this
    /// used to do left a single large table's own copy entirely unbounded — under
    /// sustained heavy write churn competing for disk I/O, one table's copy blew past
    /// not just this budget but server.rs's outer 60s wedge-detection timeout (which
    /// can't cancel an in-flight spawn_blocking call), forcing a node restart. The
    /// budget existed but couldn't actually bound wall-clock time. Checking within a
    /// table's own copy (not just between tables) closes that gap: Phase 1 now reliably
    /// bails and defers within its stated budget regardless of table size or
    /// contention, the same "try again next cycle" contract Phase 2 already honors.
    fn copy_all_tables(src: &redb::ReadTransaction, dst: &redb::WriteTransaction, deadline: std::time::Instant) -> Result<bool> {
        for def in Self::BYTES_TABLES {
            if std::time::Instant::now() >= deadline { return Ok(false); }
            if !Self::copy_bytes_table(src, dst, def, deadline)? { return Ok(false); }
        }
        for def in Self::U64_TABLES {
            if std::time::Instant::now() >= deadline { return Ok(false); }
            if !Self::copy_u64_table(src, dst, def, deadline)? { return Ok(false); }
        }
        Ok(true)
    }

    /// Reconcile `dst`'s copy of `def` against `src`'s current contents, but only for
    /// the given `keys` — the incremental counterpart to diff_bytes_table, used for
    /// FILE_TABLE/PATH_TABLE whose dirty keys are tracked as writes happen (see
    /// dirty_files/dirty_paths) instead of requiring a full-table scan to find what
    /// changed. A key present in `keys` but missing from `src` means it was deleted
    /// since the last pass — removed from `dst` too. Returns the number of rows
    /// actually changed (not just checked).
    fn diff_bytes_table_by_keys(
        src: &redb::ReadTransaction,
        dst: &redb::WriteTransaction,
        def: TableDefinition<&str, &[u8]>,
        keys: &std::collections::HashSet<String>,
    ) -> Result<usize> {
        if keys.is_empty() {
            return Ok(0);
        }
        let src_table = match src.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut dst_table = dst.open_table(def)?;
        let mut changed = 0usize;
        for key in keys {
            match src_table.get(key.as_str())? {
                Some(v) => {
                    let needs_update = match dst_table.get(key.as_str())? {
                        Some(existing) => existing.value() != v.value(),
                        None => true,
                    };
                    if needs_update {
                        dst_table.insert(key.as_str(), v.value())?;
                        changed += 1;
                    }
                }
                None => {
                    // No longer in the live table (deleted) — remove from shadow too.
                    if dst_table.get(key.as_str())?.is_some() {
                        dst_table.remove(key.as_str())?;
                        changed += 1;
                    }
                }
            }
        }
        Ok(changed)
    }

    /// Same as the old diff_all_tables, but FILE_TABLE/PATH_TABLE go through the
    /// dirty-key-tracked incremental path (O(recent writes), not O(table size)) while
    /// every other table still uses the full-scan diff_bytes_table/diff_u64_table —
    /// see OTHER_BYTES_TABLES's doc comment for why that's an acceptable tradeoff.
    ///
    /// Fixes the 2026-07-06 T44 finding: diff_bytes_table's full-table scan meant
    /// Phase 3's exclusive lock was held for however long it took to re-scan the
    /// *entire* live table on every compaction, regardless of how few rows had
    /// actually changed — "convergence" tracked the change *count*, never the scan
    /// *cost*, so a 50MB table cost ~1.1s under the exclusive lock even when nearly
    /// nothing had changed since the last pass. FILE_TABLE/PATH_TABLE are the
    /// dominant contributors to that size (full serialized FileMetadata blobs,
    /// duplicated across both tables), so tracking just those two turns their part of
    /// Phase 3's cost into O(writes since the last pass).
    ///
    /// Drains (not just reads) the dirty sets each call — see dirty_files's field doc
    /// comment for why doing this while holding `db`'s exclusive lock (Phase 3) or a
    /// shared lock started before this snapshot (Phase 2) is race-free against
    /// concurrent writers: every writer marks its keys dirty before releasing its own
    /// `db.read()` guard, so nothing committed-but-unmarked can exist by the time this
    /// runs under Phase 3's exclusive `db.write()`.
    fn diff_all_tables_tracked(&self, src: &redb::ReadTransaction, dst: &redb::WriteTransaction) -> Result<usize> {
        let dirty_file_keys = std::mem::take(&mut *self.dirty_files.lock().unwrap());
        let dirty_path_keys = std::mem::take(&mut *self.dirty_paths.lock().unwrap());

        let mut changed = 0usize;
        changed += Self::diff_bytes_table_by_keys(src, dst, FILE_TABLE, &dirty_file_keys)?;
        changed += Self::diff_bytes_table_by_keys(src, dst, PATH_TABLE, &dirty_path_keys)?;
        for def in Self::OTHER_BYTES_TABLES { changed += Self::diff_bytes_table(src, dst, def)?; }
        for def in Self::U64_TABLES { changed += Self::diff_u64_table(src, dst, def)?; }
        Ok(changed)
    }

    /// Compact the database without blocking concurrent metadata I/O.
    ///
    /// redb's own `Database::compact()` requires exclusive `&mut Database` access with
    /// no other open transactions — there's no "online compaction" mode in redb to turn
    /// on. Calling it directly (the old approach) meant holding `self.db`'s exclusive
    /// lock — which every other metadata operation takes the shared form of — for the
    /// entire compaction, which is CPU/IO-bound and scales with live data size. Under
    /// sustained write load that froze every other write on the node for the duration.
    ///
    /// Instead: build a fresh replacement file on the side (which starts out close to
    /// optimal, since it's populated by a single insert pass rather than years of
    /// update/delete churn — no further compact() of it is needed), and only take the
    /// exclusive lock for the brief final handoff.
    ///
    ///  1. Copy every table into a new shadow db, holding only the same shared lock
    ///     every ordinary read already uses — live reads and writes proceed normally
    ///     throughout this (expensive, O(live data size)) pass.
    ///  2. Iteratively diff the shadow copy against the live db's current contents and
    ///     apply just the differences, still unlocked. Each pass should be cheaper than
    ///     the last, since the window of "what changed since the last pass" shrinks as
    ///     long as our diff throughput beats the live write rate.
    ///  3. Take the exclusive lock once, for a final diff pass (bounded by whatever
    ///     didn't converge away in step 2 — should be tiny) plus the atomic swap: close
    ///     the shadow handle, rename it over the live file, and reopen a fresh handle on
    ///     the renamed file. This is the only part that blocks other metadata ops, and
    ///     it's now bounded by recent write volume instead of total live data size.
    ///
    /// Returns (before_bytes, after_bytes). Runs in the caller's thread — use
    /// spawn_blocking from async code.
    pub fn compact_db(&self) -> Result<(u64, u64)> {
        let prep = self.compact_db_prepare(std::time::Duration::from_secs(30), std::time::Duration::from_secs(5), 64)?;
        self.compact_db_finish(prep)
    }

    /// Same as `compact_db()`, parameterized by Phase 1's copy time budget and Phase 2's
    /// catch-up time budget / convergence threshold (row-changes-per-pass below which we
    /// consider it settled). Split out so tests can exercise the non-convergence/defer
    /// path deterministically with a tiny budget/threshold, instead of needing a huge
    /// dataset and a multi-second wait to reliably outrun the production defaults.
    fn compact_db_with_budget(&self, phase1_budget: std::time::Duration, catchup_budget: std::time::Duration, convergence_threshold: usize) -> Result<(u64, u64)> {
        let prep = self.compact_db_prepare(phase1_budget, catchup_budget, convergence_threshold)?;
        self.compact_db_finish(prep)
    }

    /// Phases 1-2: full copy + iterative catch-up, both held under only the *shared*
    /// read lock — can legitimately take a while in wall-clock terms (Phase 1 scales
    /// with DB size; Phase 2 has its own catchup_budget) without blocking anything else
    /// on this node. Split out from the exclusive-locked Phase 3 (see
    /// `compact_db_finish`) so server.rs can wrap ONLY Phase 3 in a tight wedge-
    /// detection timeout — wrapping the whole compact_db() call in one timeout (as this
    /// used to do) meant Phase 1/2 legitimately running long under heavy write churn
    /// could trip the same "permanently wedged, restart the node" handling meant for a
    /// truly stuck exclusive lock, which should complete in well under a second.
    ///
    /// `phase1_budget` bounds Phase 1's full-table copy the same way `catchup_budget`
    /// already bounds Phase 2 — added 2026-07-12 after a real incident: under a sustained
    /// heavy-write benchmark, Phase 1's copy (ordinary disk I/O competing with a flood of
    /// concurrent write commits for the same disk) took long enough to blow past
    /// server.rs's outer 60s wedge-detection timeout, which has no way to cancel an
    /// in-flight spawn_blocking call and so restarted the node — exactly the "relocate the
    /// blocking problem" failure Phase 2's own budget was already written to avoid, just
    /// one phase earlier. Phase 1 has no equivalent self-defense before this. Checked
    /// between whole tables (see copy_all_tables), not intra-table, mirroring Phase 2's
    /// own between-passes (not intra-pass) granularity.
    pub(crate) fn compact_db_prepare(&self, phase1_budget: std::time::Duration, catchup_budget: std::time::Duration, convergence_threshold: usize) -> Result<CompactionPrep> {
        let size_before = std::fs::metadata(&self.db_path).map(|m| m.len()).unwrap_or(0);
        info!("redb compaction starting ({:.1}MB live)", size_before as f64 / 1_048_576.0);

        let shadow_path = self.db_path.with_extension("redb.shadow");
        let _ = std::fs::remove_file(&shadow_path);
        let shadow_db = Database::builder()
            .set_cache_size(256 * 1024 * 1024)
            .create(&shadow_path)
            .map_err(|e| anyhow::anyhow!("compact: failed to create shadow db: {}", e))?;

        // Establish the dirty-tracking baseline before Phase 1's snapshot: any write
        // that commits after this point marks itself dirty (see dirty_files's field
        // doc comment), so Phase 2/3's incremental catch-up will see it regardless of
        // whether it landed before or after Phase 1's own read snapshot — being marked
        // dirty for a key Phase 1 already copied is harmless (just a redundant re-copy
        // next pass), it's only a miss in the other direction that would lose data.
        self.dirty_files.lock().unwrap().clear();
        self.dirty_paths.lock().unwrap().clear();

        // Phase 1: full copy, holding only the shared lock. Bailing here (rather than
        // ever entering Phase 2) is the same "try again next cycle" contract Phase 2
        // already gives the caller — see this function's doc comment.
        let phase1_deadline = std::time::Instant::now() + phase1_budget;
        {
            let live = self.db.read();
            let src_txn = live.begin_read().map_err(|e| anyhow::anyhow!("compact phase1 begin_read: {}", e))?;
            let dst_txn = shadow_db.begin_write().map_err(|e| anyhow::anyhow!("compact phase1 begin_write: {}", e))?;
            let finished = Self::copy_all_tables(&src_txn, &dst_txn, phase1_deadline)?;
            if !finished {
                drop(dst_txn);
                drop(shadow_db);
                let _ = std::fs::remove_file(&shadow_path);
                anyhow::bail!(
                    "compaction deferred: Phase 1's full-table copy exceeded its {:?} budget \
                     (live db under sustained I/O contention) — will retry on the next cycle",
                    phase1_budget
                );
            }
            dst_txn.commit().map_err(|e| anyhow::anyhow!("compact phase1 commit: {}", e))?;
            // Payload = size_before: this phase's commit rewrites the whole live DB into
            // the shadow file, not a proportional per-record write — attributing
            // size_before bytes here (rather than 0) keeps it from being mistaken for a
            // "free" transaction when comparing to real per-record growth sites, while
            // clearly flagging it (via the site name) as a bulk full-copy, not organic growth.
            self.note_txn("compact_db_prepare_phase1", size_before as usize);
        }

        // Phase 2: iterative catch-up, still unlocked. Bounded by a time budget, not a
        // fixed pass count — under sustained heavy write churn (e.g. a bulk-delete
        // storm), the live db can keep changing faster than we can scan it, so no fixed
        // number of passes is guaranteed to converge. If we don't converge within the
        // budget, abort cleanly rather than handing Phase 3 a large diff to apply while
        // holding the exclusive lock — that would just relocate the original blocking
        // problem instead of fixing it. The caller (server.rs's compaction loop) treats
        // a plain Err here as "try again next cycle", which is exactly what we want:
        // skip compacting while the node is busy, retry once things quiet down.
        let catchup_deadline = std::time::Instant::now() + catchup_budget;
        let mut converged = false;
        loop {
            let live = self.db.read();
            let src_txn = live.begin_read().map_err(|e| anyhow::anyhow!("compact phase2 begin_read: {}", e))?;
            let dst_txn = shadow_db.begin_write().map_err(|e| anyhow::anyhow!("compact phase2 begin_write: {}", e))?;
            let changed = self.diff_all_tables_tracked(&src_txn, &dst_txn)?;
            dst_txn.commit().map_err(|e| anyhow::anyhow!("compact phase2 commit: {}", e))?;
            self.note_txn("compact_db_prepare_phase2", 0);
            if changed <= convergence_threshold {
                converged = true;
                break;
            }
            if std::time::Instant::now() >= catchup_deadline {
                break;
            }
        }
        if !converged {
            drop(shadow_db);
            let _ = std::fs::remove_file(&shadow_path);
            anyhow::bail!(
                "compaction deferred: live db is under sustained write churn and catch-up \
                 didn't settle within budget — will retry on the next cycle"
            );
        }

        Ok(CompactionPrep { shadow_db, shadow_path, size_before })
    }

    /// Phase 3 alone: final catch-up diff + atomic swap, under the *exclusive* write
    /// lock — the only part of compaction that blocks other metadata operations on this
    /// node. The diff here is small by construction (Phase 2 already converged it below
    /// convergence_threshold), so this should complete in well under a second even for
    /// a large DB. server.rs wraps only this call in a tight wedge-detection timeout —
    /// if it doesn't return promptly, the exclusive lock is genuinely wedged.
    pub(crate) fn compact_db_finish(&self, prep: CompactionPrep) -> Result<(u64, u64)> {
        let CompactionPrep { shadow_db, shadow_path, size_before } = prep;
        info!("redb compaction phase3 lock acquiring");
        {
            let mut live = self.db.write();
            {
                let src_txn = live.begin_read().map_err(|e| anyhow::anyhow!("compact phase3 begin_read: {}", e))?;
                let dst_txn = shadow_db.begin_write().map_err(|e| anyhow::anyhow!("compact phase3 begin_write: {}", e))?;
                self.diff_all_tables_tracked(&src_txn, &dst_txn)?;
                dst_txn.commit().map_err(|e| anyhow::anyhow!("compact phase3 commit: {}", e))?;
                self.note_txn("compact_db_finish_phase3", 0);
            }
            drop(shadow_db);
            std::fs::rename(&shadow_path, &self.db_path)
                .map_err(|e| anyhow::anyhow!("compact: failed to swap in shadow db: {}", e))?;
            let new_db = Database::builder()
                .set_cache_size(256 * 1024 * 1024)
                .open(&self.db_path)
                .map_err(|e| anyhow::anyhow!("compact: failed to reopen post-swap db: {}", e))?;
            *live = new_db;
        }

        let size_after = std::fs::metadata(&self.db_path).map(|m| m.len()).unwrap_or(0);
        // Unconditional completion marker, distinct from the caller's (server.rs)
        // size-delta log — that one only fires when before != after, so it can't be
        // relied on alone to always close a compaction window.
        info!("redb compaction finished");
        Ok((size_before, size_after))
    }

    /// Last-resort fallback: compact the live db directly, in place, blocking all other
    /// metadata I/O on this node for the duration.
    ///
    /// compact_db() can defer indefinitely under sustained write churn — by design, it
    /// never hands Phase 3 a large diff to apply while exclusively locked. That's the
    /// right tradeoff for a transient burst (the next 60s cycle just tries again), but
    /// under truly sustained churn (hours of continuous heavy writes) it could mean
    /// fragmentation never gets reclaimed at all, and the file grows unboundedly. The
    /// caller (server.rs's compaction loop) is responsible for escalating to this method
    /// after compact_db() has deferred repeatedly for too long *and* fragmentation is
    /// still bad enough to matter — accepting one bounded blocking hit is better than
    /// unbounded disk growth.
    pub fn compact_db_blocking(&self) -> Result<(u64, u64)> {
        let size_before = std::fs::metadata(&self.db_path).map(|m| m.len()).unwrap_or(0);
        info!("redb compaction starting (blocking fallback, {:.1}MB live)", size_before as f64 / 1_048_576.0);
        let mut db = self.db.write();

        // redb's compact() fails if any pending_non_durable_commits exist — it counts
        // them as live_read_transactions. Durability::None commits (used on every write
        // for performance) accumulate in this list and are only cleared by a durable
        // commit. A single empty durable commit drains the entire accumulated list so
        // compact() can proceed.
        {
            let txn = db.begin_write()
                .map_err(|e| anyhow::anyhow!("compact pre-flush begin: {}", e))?;
            txn.commit()
                .map_err(|e| anyhow::anyhow!("compact pre-flush commit: {}", e))?;
            self.note_txn("compact_db_blocking", 0);
        }

        db.compact().map_err(|e| anyhow::anyhow!("redb compact: {}", e))?;
        let size_after = std::fs::metadata(&self.db_path).map(|m| m.len()).unwrap_or(0);
        info!("redb compaction finished (blocking fallback)");
        Ok((size_before, size_after))
    }

    /// No-op kept for call-site compatibility; compaction is handled by compact_db().
    pub fn flush(&self) -> Result<()> {
        Ok(())
    }

    /// Force all pending Durability::None commits to physical disk via a single fdatasync.
    /// Same trick used by compact_db() — one empty Durability::Immediate commit promotes
    /// the entire accumulated pending_non_durable_commits list atomically.
    ///
    /// Takes the SHARED (`db.read()`) lock, NOT `db.write()`. This only opens an
    /// empty redb write transaction and commits it — the identical operation
    /// apply_ops_group performs under `db.read()` (redb serializes its own single
    /// writer internally; the parking_lot exclusive lock is ONLY needed by
    /// compaction, which swaps the Database handle via `&mut`). Using `db.write()`
    /// here was a latent node-wide-wedge bug: it set parking_lot's writer-intent,
    /// which (anti-starvation) blocks EVERY subsequent metadata read on the node.
    /// Because flush_durable is called synchronously from the offline-compaction /
    /// shutdown drain (drain_sled_writes) on a runtime thread — inside a `timeout()`
    /// that cannot interrupt a synchronous blocking lock — a single slow reader was
    /// enough to hang the whole node behind wait_for_readers(). Confirmed live
    /// 2026-07-17 (gluster1, gdb: ~90 threads parked in lock_shared_slow behind this
    /// one lock_exclusive_slow). The shared lock coexists with all readers and can
    /// never set writer-intent, so that wedge vector is gone.
    pub fn flush_durable(&self) -> Result<()> {
        let db = self.db.read();
        let txn = db.begin_write()
            .map_err(|e| anyhow::anyhow!("flush_durable begin: {}", e))?;
        txn.commit()
            .map_err(|e| anyhow::anyhow!("flush_durable commit: {}", e))?;
        self.note_txn("flush_durable", 0);
        Ok(())
    }

    /// Get database statistics.
    pub fn get_stats(&self) -> Result<MetadataStats> {
        let file_count = self.list_files()?.len();
        let size_on_disk = std::fs::metadata(&self.db_path)
            .map(|m| m.len())
            .unwrap_or(0);
        Ok(MetadataStats { file_count, size_on_disk })
    }

    pub fn db_size(&self) -> u64 {
        std::fs::metadata(&self.db_path).map(|m| m.len()).unwrap_or(0)
    }

    /// Real, in-use bytes (stored data + B-tree indexing overhead), as opposed to
    /// db_size()'s raw OS file size.
    ///
    /// Root-caused 2026-07-15 (server5/VM111 live incident, self-sustaining
    /// compaction loop): the compaction trigger used db_size() as a stand-in for
    /// fragmentation, comparing raw file size against the size right after the
    /// last compact. But redb grows its backing file geometrically (observed live:
    /// a *tight, freshly-compacted* 257.5MB file jumped to 514.5MB — almost exactly
    /// 2x — after only ~273 small chunk_location writes in under two minutes,
    /// nowhere near enough real data to explain 257MB of genuine growth). db_size()
    /// can't tell "the file grew because redb pre-allocated headroom for future
    /// writes" apart from "the file grew because of genuine fragmented waste" —
    /// they look identical from the outside, but only the second one is a reason
    /// to pay for another compaction. redb's own WriteTransaction::stats() (there is
    /// no read-only equivalent in this redb version) reports stored_bytes/
    /// metadata_bytes/fragmented_bytes directly from its B-tree accounting, which
    /// distinguishes them. Opens a write transaction only to read stats and
    /// explicitly aborts it (no commit, no data touched) — same cadence as the
    /// existing 60s compaction-check poll, not on any write's hot path.
    pub fn redb_fragmentation_stats(&self) -> Result<(u64, u64)> {
        let _db = self.db.read();
        let txn = _db.begin_write()?;
        let stats = txn.stats()?;
        let live_bytes = stats.stored_bytes() + stats.metadata_bytes();
        let fragmented_bytes = stats.fragmented_bytes();
        txn.abort()?;
        Ok((live_bytes, fragmented_bytes))
    }
}

/// Metadata storage statistics
#[derive(Debug, Clone)]
pub struct MetadataStats {
    pub file_count: usize,
    pub size_on_disk: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use dfs_common::{ChunkId, FileType, NodeId};
    use tempfile::TempDir;

    /// redb's `fragmented_bytes` is NOT reclaimable waste, and must never be used as
    /// a compaction trigger. Measured 2026-07-16 with this exact workload:
    ///
    ///   after churn            file=14.51MB live=1.22MB frag= 7.32MB  frag%=50.4%
    ///   shadow-copy compact -> file= 4.53MB live=1.42MB frag= 2.59MB  frag%=57.1%
    ///   redb compact()      -> file= 3.52MB live=0.91MB frag= 2.10MB  frag%=59.5%
    ///   redb compact() again-> file= 3.52MB (no-op, converged)         frag%=59.5%
    ///
    /// The file shrank 76% overall, and frag% went UP the whole way. At redb's own
    /// converged floor — compact() twice, the second a proven no-op — it still reports
    /// ~60% fragmented. There is no state any compaction can reach where this drops
    /// under 20%, so a `frag_pct >= 0.20` gate is a constant `true`, not a threshold.
    /// That gate ran on staging every 60s for ~20s a cycle reclaiming zero bytes.
    ///
    /// This replaces test_redb_fragmentation_stats_reports_low_fragmentation_after_
    /// many_small_writes, which asserted the opposite (<20% on a fresh DB), failed
    /// against reality at 91.7%, and was deleted in 84b6f8e instead of being believed.
    /// Compaction triggers must key off FILE GROWTH past the post-compaction baseline
    /// — the only quantity here that actually tracks reclaimable space.
    /// Build a realistically-churned DB: many small per-record commits, plus
    /// chunk-ID rotation (write then delete) to leave real dead pages behind.
    #[cfg(test)]
    fn churn_for_compaction_test(store: &MetadataStore) {
        let node = NodeId::new();
        for i in 0..8000u64 {
            let hash = dfs_common::hash::compute_chunk_hash(format!("chunk-{}", i).as_bytes());
            let chunk_id = ChunkId::from_hash(hash);
            let loc = ChunkLocation {
                chunk_id,
                nodes: vec![node],
                size: 4 * 1024 * 1024,
                checksum: hash,
                file_offset: Some((i % 256) * 4 * 1024 * 1024),
                written_at: Some(1000 + i),
                client_write_seq: Some(i),
                file_id: Some(FileId::new()),
            };
            store.put_chunk_location(&loc).unwrap();
            if i % 2 == 1 {
                let old = ChunkId::from_hash(dfs_common::hash::compute_chunk_hash(format!("chunk-{}", i - 1).as_bytes()));
                store.delete_chunk_location(&old).unwrap();
            }
        }
    }

    /// HEAD-TO-HEAD from the SAME churned starting point — the comparison the first
    /// version of this experiment failed to make (it ran redb's compact() on the
    /// already-shadow-compacted file, so it could only ever show redb's *incremental*
    /// gain, and was misread as "redb alone beats shadow alone").
    ///
    /// Measured 2026-07-16 (8000 churned records):
    ///   churned start          : 14.51MB
    ///   A: shadow-copy only    :  4.53MB
    ///   B: redb compact() only :  2.74MB   <- best here
    ///   C: shadow, then redb   :  3.52MB
    ///
    /// DO NOT read B as "always use redb's compact()". Staging, on a real ~500MB DB,
    /// went the other way: redb's compact() alone reached 514.5MB->351.5MB where the
    /// shadow copy had always reached 257.5MB. The reconciling fact (per the user, from
    /// operating this system): redb's compact() is ITERATIVE — repeated passes reclaim
    /// more — and we deliberately call it ONCE. At this test's scale one pass already
    /// converges; at staging's scale it does not.
    ///
    /// That single pass is a deliberate speed-and-value trade, not an oversight: take
    /// the cheap majority and let the 30-minute periodic pick up the rest, rather than
    /// sit in an exclusive lock chasing the last megabytes. This test therefore asserts
    /// only that all three methods substantially shrink the file — NOT a ranking between
    /// them, because the ranking is scale-dependent and this fixture cannot see it.
    #[test]
    fn compare_compaction_methods_from_the_same_starting_point() {
        let size_of = |store: &MetadataStore| store.db_size() as f64 / 1_048_576.0;

        // Arm A: shadow-copy rebuild only (the online path).
        let dir_a = TempDir::new().unwrap();
        let store_a = MetadataStore::new(dir_a.path().to_path_buf()).unwrap();
        churn_for_compaction_test(&store_a);
        let churned = size_of(&store_a);
        store_a.compact_db().unwrap();
        let shadow_only = size_of(&store_a);

        // Arm B: redb's own compact() only (what the offline path now does).
        let dir_b = TempDir::new().unwrap();
        let store_b = MetadataStore::new(dir_b.path().to_path_buf()).unwrap();
        churn_for_compaction_test(&store_b);
        store_b.compact_db_blocking().unwrap();
        let redb_only = size_of(&store_b);

        // Arm C: shadow copy, then redb compact() — what the original experiment
        // actually measured end-to-end.
        let dir_c = TempDir::new().unwrap();
        let store_c = MetadataStore::new(dir_c.path().to_path_buf()).unwrap();
        churn_for_compaction_test(&store_c);
        store_c.compact_db().unwrap();
        store_c.compact_db_blocking().unwrap();
        let both = size_of(&store_c);

        println!();
        println!("  churned start          : {:>7.2}MB", churned);
        println!("  A: shadow-copy only    : {:>7.2}MB", shadow_only);
        println!("  B: redb compact() only : {:>7.2}MB", redb_only);
        println!("  C: shadow, then redb   : {:>7.2}MB", both);
        println!();

        // Deliberately NOT a ranking assertion — see the doc comment. Only that each
        // method does substantial work, so a regression that silently stops reclaiming
        // still fails here.
        for (label, after) in [("shadow-only", shadow_only), ("redb-only", redb_only), ("both", both)] {
            assert!(after < churned / 2.0,
                "{} should reclaim at least half the churned file ({:.2}MB -> {:.2}MB)",
                label, churned, after);
        }
    }

    #[test]
    fn fragmented_bytes_stays_high_even_at_redbs_own_compaction_floor() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();
        let node = NodeId::new();

        // Build a realistically-churned DB: many small per-record commits, plus
        // chunk-ID rotation (write then delete) to leave real dead pages behind.
        for i in 0..8000u64 {
            let hash = dfs_common::hash::compute_chunk_hash(format!("chunk-{}", i).as_bytes());
            let chunk_id = ChunkId::from_hash(hash);
            let loc = ChunkLocation {
                chunk_id,
                nodes: vec![node],
                size: 4 * 1024 * 1024,
                checksum: hash,
                file_offset: Some((i % 256) * 4 * 1024 * 1024),
                written_at: Some(1000 + i),
                client_write_seq: Some(i),
                file_id: Some(FileId::new()),
            };
            store.put_chunk_location(&loc).unwrap();
            // Rotate: every other chunk is superseded and removed, like real patching.
            if i % 2 == 1 {
                let old = ChunkId::from_hash(dfs_common::hash::compute_chunk_hash(format!("chunk-{}", i - 1).as_bytes()));
                store.delete_chunk_location(&old).unwrap();
            }
        }

        let report = |label: &str, store: &MetadataStore| {
            let size = store.db_size();
            let (live, frag) = store.redb_fragmentation_stats().unwrap();
            println!(
                "{:<28} file={:>8.2}MB  live={:>8.2}MB  frag={:>8.2}MB  frag%={:>5.1}%  file/live={:>4.2}x",
                label,
                size as f64 / 1_048_576.0,
                live as f64 / 1_048_576.0,
                frag as f64 / 1_048_576.0,
                100.0 * frag as f64 / size.max(1) as f64,
                size as f64 / live.max(1) as f64,
            );
        };

        println!();
        report("0. after churn", &store);

        let (_, after_shadow) = store.compact_db().unwrap();
        report("1. after shadow-copy", &store);

        // redb's own compact() on the ALREADY shadow-compacted db — whatever it
        // reclaims here is space the shadow copy left behind.
        let (_, after_redb) = store.compact_db_blocking().unwrap();
        report("2. after redb compact()", &store);
        assert!(
            after_redb < after_shadow,
            "redb's compact() must reclaim what the shadow-copy rebuild leaves behind \
             (shadow left {}B, redb got it to {}B) — this is why the OFFLINE path, which \
             already has exclusive access, should not pay for the shadow copy",
            after_shadow, after_redb
        );

        // Idempotent: a second pass has nothing left to do. This is the floor.
        let (_, after_redb2) = store.compact_db_blocking().unwrap();
        report("3. after redb compact x2", &store);
        assert_eq!(after_redb2, after_redb, "redb compact() should converge, not oscillate");

        // THE POINT: at that floor, fragmentation is still way over any sane trigger.
        let (live, frag) = store.redb_fragmentation_stats().unwrap();
        let frag_pct = frag as f64 / after_redb2.max(1) as f64;
        assert!(
            frag_pct >= 0.20,
            "expected fragmented_bytes to remain >=20% even at redb's own compaction \
             floor (got {:.1}%, live={}B frag={}B file={}B). If this ever fails, redb's \
             accounting changed and Server::should_compact could legitimately use frag% \
             again — until then it must gate on file growth instead.",
            frag_pct * 100.0, live, frag, after_redb2
        );
        println!();
    }

    #[test]
    fn test_store_and_retrieve_file() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();

        let mut metadata = FileMetadata::new("/test.txt".to_string(), FileType::RegularFile);
        metadata.size = 1024;

        store.put_file(&metadata).unwrap();

        let retrieved = store.get_file(&metadata.id).unwrap().unwrap();
        assert_eq!(retrieved.path, "/test.txt");
        assert_eq!(retrieved.size, 1024);
    }

    /// Regression for the 2026-07-16 PATH_TABLE dedup: put_file_in_txn used to
    /// write the ENTIRE serialized FileMetadata blob into PATH_TABLE (an exact
    /// duplicate of what FILE_TABLE already stores for the same record) — the
    /// dominant metadata-DB write-volume driver measured under sustained patch
    /// writes. Confirms the new write actually is short (a file_id string), not
    /// the old multi-hundred-byte blob.
    #[test]
    fn test_path_table_stores_file_id_not_blob() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();

        let mut metadata = FileMetadata::new("/dedup_test.bin".to_string(), FileType::RegularFile);
        let node = NodeId::new();
        metadata.chunk_locations = std::sync::Arc::new(vec![dfs_common::ChunkLocation {
            chunk_id: ChunkId::from_hash(dfs_common::hash::compute_chunk_hash(b"x")),
            nodes: vec![node],
            size: 4096,
            checksum: [0u8; 32],
            file_offset: Some(0),
            written_at: None,
            client_write_seq: Some(1),
            file_id: None,
        }]);
        store.put_file(&metadata).unwrap();

        // Read PATH_TABLE's raw value directly and confirm it round-trips through
        // resolve_path_entry_file_id AS a plain uuid string — a full FileMetadata
        // blob for a record with a chunk_locations entry would be well over 100
        // bytes; a hyphenated UUID string is exactly 36.
        let raw = {
            let _db = store.db.read();
            let txn = _db.begin_read().unwrap();
            let path_table = txn.open_table(PATH_TABLE).unwrap();
            path_table.get("/dedup_test.bin").unwrap().unwrap().value().to_vec()
        };
        assert_eq!(raw.len(), 36, "PATH_TABLE value should be a bare UUID string, not a FileMetadata blob (got {} bytes)", raw.len());
        assert_eq!(MetadataStore::resolve_path_entry_file_id(&raw).unwrap(), metadata.id);

        // And the normal read path still resolves correctly through the new format.
        // chunk_locations is empty on the raw MetadataStore-level read — put_file_in_txn
        // never persists it (see its doc comment); CHUNK_TABLE is the authoritative
        // source, enriched by Server::chunk_locations_for_info for callers that need it.
        let retrieved = store.get_file_by_path("/dedup_test.bin").unwrap().unwrap();
        assert_eq!(retrieved.id, metadata.id);
        assert_eq!(retrieved.chunk_locations.len(), 0);
    }

    /// Upgrade-path regression: a PATH_TABLE entry written by pre-2026-07-16 code
    /// (the full legacy blob format) must still resolve correctly under the new
    /// code — no migration pass, no window where an old path is unreadable. This
    /// is the scenario a real staging upgrade exercises: existing on-disk data in
    /// the old format, new binary reading it.
    #[test]
    fn test_get_file_by_path_resolves_legacy_blob_entry() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();

        let mut metadata = FileMetadata::new("/legacy_path.bin".to_string(), FileType::RegularFile);
        metadata.size = 777;
        // Populate FILE_TABLE the normal way...
        store.put_file(&metadata).unwrap();
        // ...then overwrite PATH_TABLE's entry with the OLD full-blob format,
        // simulating data written before the dedup change.
        let legacy_blob = bincode::serialize(&metadata).unwrap();
        store.put_raw_path_entry("/legacy_path.bin", &legacy_blob).unwrap();

        let retrieved = store.get_file_by_path("/legacy_path.bin").unwrap().unwrap();
        assert_eq!(retrieved.id, metadata.id);
        assert_eq!(retrieved.size, 777);
    }

    /// Same upgrade-path guarantee for list_directory and remove_unlisted_files —
    /// both must handle a directory/reconciliation scan that mixes legacy-blob
    /// and new-format path entries (the realistic post-upgrade state: only paths
    /// touched by a write since the upgrade have converted).
    #[test]
    fn test_list_directory_and_reconcile_handle_mixed_path_formats() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();

        let new_fmt = FileMetadata::new("/dir/new_format.bin".to_string(), FileType::RegularFile);
        store.put_file(&new_fmt).unwrap();

        let legacy_fmt = FileMetadata::new("/dir/legacy_format.bin".to_string(), FileType::RegularFile);
        store.put_file(&legacy_fmt).unwrap();
        let legacy_blob = bincode::serialize(&legacy_fmt).unwrap();
        store.put_raw_path_entry("/dir/legacy_format.bin", &legacy_blob).unwrap();

        let mut listed = store.list_directory("/dir").unwrap();
        listed.sort_by_key(|m| m.path.clone());
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].path, "/dir/legacy_format.bin");
        assert_eq!(listed[1].path, "/dir/new_format.bin");

        // remove_unlisted_files' path-table sweep must also resolve the legacy
        // entry correctly to decide liveness, not just skip/mis-clear it.
        let live_ids: std::collections::HashSet<FileId> = [new_fmt.id, legacy_fmt.id].into_iter().collect();
        let removed = store.remove_unlisted_files(&live_ids).unwrap();
        assert_eq!(removed, 0, "both entries are live and must survive reconciliation regardless of on-disk format");

        let live_ids_minus_legacy: std::collections::HashSet<FileId> = [new_fmt.id].into_iter().collect();
        let removed = store.remove_unlisted_files(&live_ids_minus_legacy).unwrap();
        assert!(removed > 0, "the now-unlisted legacy-format entry must still be recognized and removed");
        assert!(store.get_file_by_path("/dir/legacy_format.bin").unwrap().is_none());
        assert!(store.get_file_by_path("/dir/new_format.bin").unwrap().is_some());
    }

    /// Regression for the PATH_TABLE dedup fix's actual on-disk effect: repeated
    /// pushes of a file with a realistic (kdiskmark-sized, 1GB/4MB-chunk)
    /// chunk_locations array must grow the database by roughly ONE blob's worth
    /// of bytes per push, not two (FILE_TABLE plus a duplicate PATH_TABLE copy —
    /// the pre-fix behavior). Live fio repros are too noisy (compaction timing)
    /// to isolate this one variable cleanly, so this measures it directly
    /// against a computed old-shape estimate instead of relying on a live A/B.
    #[test]
    fn test_path_table_dedup_reduces_total_disk_growth() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();
        let node = NodeId::new();

        let chunk_locations: Vec<_> = (0..256u32).map(|i| {
            let hash = dfs_common::hash::compute_chunk_hash(format!("c{}", i).as_bytes());
            dfs_common::ChunkLocation {
                chunk_id: ChunkId::from_hash(hash),
                nodes: vec![node],
                size: 4 * 1024 * 1024,
                checksum: hash,
                file_offset: Some(i as u64 * 4 * 1024 * 1024),
                written_at: Some(1000),
                client_write_seq: Some(0),
                file_id: None,
            }
        }).collect();

        let mut metadata = FileMetadata::new("/big.bin".to_string(), FileType::RegularFile);
        metadata.chunk_locations = std::sync::Arc::new(chunk_locations);
        store.put_file(&metadata).unwrap();
        let one_blob_size = bincode::serialize(&metadata).unwrap().len() as u64;

        store.compact_db().unwrap();
        let (live_before, _) = store.redb_fragmentation_stats().unwrap();

        const PUSHES: u64 = 100;
        for _ in 0..PUSHES {
            // Re-push with a bumped write_seq/modified_at each time — same shape
            // as a real repeated patch/background-tick push. Content doesn't
            // need to change for this measurement, only that each is a genuine
            // new commit (put_file's is_stale guard would otherwise skip work).
            metadata.write_seq += 1;
            metadata.modified_at += 1;
            store.put_file(&metadata).unwrap();
        }

        let (live_after, fragmented_after) = store.redb_fragmentation_stats().unwrap();
        let total_growth = (live_after + fragmented_after).saturating_sub(live_before);

        // Pre-fix, every push wrote one_blob_size bytes to FILE_TABLE AND an
        // identical one_blob_size to PATH_TABLE — roughly 2x per push. Post-fix,
        // PATH_TABLE's write shrinks to a 36-byte file_id, so growth should be
        // close to 1x. Assert well under the old-shape estimate rather than
        // near-exactly 1x, since B-tree/table overhead isn't purely linear.
        let old_shape_estimate = 2 * one_blob_size * PUSHES;
        assert!(
            total_growth < old_shape_estimate * 2 / 3,
            "expected roughly half the pre-dedup growth (old-shape estimate={}B, actual={}B)",
            old_shape_estimate, total_growth
        );
    }

    /// repair_path_index's rebuild pass must write the NEW (short file_id) format,
    /// not resurrect the old blob format it's ostensibly "repairing" — otherwise a
    /// repair after this change silently reintroduces the write-volume regression
    /// this whole change exists to fix.
    #[test]
    fn test_repair_path_index_rebuilds_in_new_format() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();

        let metadata = FileMetadata::new("/needs_repair.bin".to_string(), FileType::RegularFile);
        store.put_file(&metadata).unwrap();
        store.delete_path_index("/needs_repair.bin").unwrap();
        assert!(store.get_file_by_path("/needs_repair.bin").unwrap().is_none());

        store.repair_path_index().unwrap();

        let raw = {
            let _db = store.db.read();
            let txn = _db.begin_read().unwrap();
            let path_table = txn.open_table(PATH_TABLE).unwrap();
            path_table.get("/needs_repair.bin").unwrap().unwrap().value().to_vec()
        };
        assert_eq!(raw.len(), 36, "repair must recreate the short file_id format, not the legacy blob");
        assert_eq!(store.get_file_by_path("/needs_repair.bin").unwrap().unwrap().id, metadata.id);
    }

    /// History: this test originally asserted that 500 per-record commits produce
    /// LOW genuine fragmentation, under the 2026-07-15 theory that the observed
    /// 257.5MB→514.5MB live jump was redb's geometric file pre-allocation, not real
    /// waste. That assertion was wrong and the test failed from day one: 500
    /// sequential single-record commits measure ~90% fragmented_bytes even on a
    /// fresh DB — every commit COW-rewrites the touched leaf and its ancestors, and
    /// with Durability::None those freed pages aren't reusable until the next
    /// durable flush. That per-record-commit churn IS the growth mechanism (both
    /// 2026-07-15 baselines: live <1MB inside 10-56MB files), and the group
    /// committer exists to fix it.
    ///
    /// So the meaningful regression assertion is comparative: the same 500 records
    /// written as one batch transaction must fragment FAR less than 500 per-record
    /// transactions. If this ever fails, batching stopped amortizing the COW churn
    /// — the whole point of the 2026-07-15/16 DB-growth fixes.
    #[test]
    fn test_batched_writes_fragment_far_less_than_per_record_commits() {
        let node = NodeId::new();
        let make_loc = |i: u32| {
            let hash = dfs_common::hash::compute_chunk_hash(format!("chunk-{}", i).as_bytes());
            dfs_common::ChunkLocation {
                chunk_id: ChunkId::from_hash(hash),
                nodes: vec![node],
                size: 4096,
                checksum: hash,
                file_offset: Some(0),
                written_at: Some(1000 + i as u64),
                client_write_seq: Some(i as u64),
                file_id: None,
            }
        };

        // Store A: 500 single-record transactions (the pre-fix hot-path shape).
        let temp_a = TempDir::new().unwrap();
        let store_a = MetadataStore::new(temp_a.path().to_path_buf()).unwrap();
        for i in 0..500u32 {
            store_a.put_chunk_location(&make_loc(i)).unwrap();
        }
        let (live_a, frag_a) = store_a.redb_fragmentation_stats().unwrap();
        assert!(live_a > 0);
        let frag_pct_a = frag_a as f64 / (live_a + frag_a) as f64;

        // Store B: the same 500 records in one batch transaction (what the group
        // committer produces for a concurrent burst).
        let temp_b = TempDir::new().unwrap();
        let store_b = MetadataStore::new(temp_b.path().to_path_buf()).unwrap();
        let locations: Vec<_> = (0..500u32).map(make_loc).collect();
        store_b.put_chunk_locations_batch(&locations).unwrap();
        let (live_b, frag_b) = store_b.redb_fragmentation_stats().unwrap();
        assert!(live_b > 0);
        let frag_pct_b = frag_b as f64 / (live_b + frag_b) as f64;

        // Compare absolute fragmented BYTES, not ratios: both stores hold the same
        // live records, but on a nearly-empty DB the ratio is dominated by fixed
        // table-creation overhead (measured: one 500-record txn still reads ~51%
        // by ratio while being ~10x smaller in bytes than the per-record store's
        // 91.7%). Bytes isolate the per-commit COW churn this test is about.
        assert!(
            frag_b * 3 < frag_a,
            "batched writes should fragment far less than per-record commits \
             (per-record: {}B / {:.1}%, batched: {}B / {:.1}%)",
            frag_a, frag_pct_a * 100.0, frag_b, frag_pct_b * 100.0
        );
    }

    /// Group-commit regression test (2026-07-15/16 DB-growth fix): N concurrent
    /// single-record *_async calls must coalesce into far fewer transactions than
    /// N, while every record is still individually readable once its own await
    /// returns (commit-before-return semantics preserved — no write-behind window).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_group_commit_coalesces_concurrent_single_record_writes() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(MetadataStore::new(temp_dir.path().to_path_buf()).unwrap());
        let node = NodeId::new();

        const N: usize = 200;
        let mut handles = Vec::new();
        for i in 0..N {
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                let hash = dfs_common::hash::compute_chunk_hash(format!("gc-{}", i).as_bytes());
                let loc = dfs_common::ChunkLocation {
                    chunk_id: ChunkId::from_hash(hash),
                    nodes: vec![node],
                    size: 4096,
                    checksum: hash,
                    file_offset: Some(0),
                    written_at: Some(1000 + i as u64),
                    client_write_seq: Some(i as u64),
                    file_id: None,
                };
                store.put_chunk_location_async(loc.clone()).await.unwrap();
                // Read-your-writes: our await returned, so our record is committed.
                let read_back = store.get_chunk_location(&loc.chunk_id).unwrap();
                assert_eq!(read_back.map(|l| l.chunk_id), Some(loc.chunk_id));
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }

        let stats = store.txn_stats_snapshot();
        let count_of = |site: &str| stats.iter()
            .find(|(s, _, _)| s == site)
            .map(|(_, count, _)| *count)
            .unwrap_or(0);
        assert_eq!(count_of("op:put_chunk_location"), N as u64, "every op must be applied exactly once");
        let commits = count_of("group_commit");
        assert!(commits >= 1, "at least one commit must have happened");
        assert!(
            commits < (N as u64) * 3 / 4,
            "{} concurrent single-record writes should group-commit into fewer \
             transactions ({} observed) — if this is ~N, coalescing is broken",
            N, commits
        );
    }

    /// Regression test for a real bug found via T48 under full local-suite load
    /// (2026-07-07): put_file's chunk_locations merge must be a true union starting
    /// from the PERSISTED record, not from the incoming payload. Routine writes now
    /// send only the chunks touched this cycle (a partial, non-cumulative delta —
    /// see fuse_impl.rs's flush_buffer_async), relying on this merge to preserve
    /// whatever the incoming update doesn't mention. Starting from incoming (the old
    /// behavior) silently dropped any chunk recorded in the existing record but
    /// absent from incoming — this was masked in most cases by handle_put_file_metadata's
    /// upstream chunk_map reconcile pre-expanding incoming to a full list, but that
    /// reconcile is only as fresh as chunk_map itself, which can lag behind concurrent
    /// pushes — put_file must not depend on it.
    #[test]
    fn test_put_file_preserves_existing_chunks_when_incoming_is_partial() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();
        let node = NodeId::new();

        let mut metadata = FileMetadata::new("/partial_merge.bin".to_string(), FileType::RegularFile);
        metadata.write_seq = 1;
        let loc0 = dfs_common::ChunkLocation {
            chunk_id: ChunkId::from_hash([1u8; 32]),
            nodes: vec![node],
            size: 4 * 1024 * 1024,
            checksum: [1u8; 32],
            file_offset: Some(0),
            written_at: None,
            client_write_seq: Some(1),
            file_id: Some(metadata.id),
        };
        metadata.chunk_locations = Arc::new(vec![loc0.clone()]);
        store.put_file(&metadata).unwrap();

        // Second push describes ONLY a new chunk at a different offset — a partial,
        // non-cumulative delta, exactly what the routine background-tick/force-flush
        // paths send since 2026-07-07. Must not lose loc0.
        let mut update = metadata.clone();
        update.write_seq = 2;
        let loc1 = dfs_common::ChunkLocation {
            chunk_id: ChunkId::from_hash([2u8; 32]),
            nodes: vec![node],
            size: 4 * 1024 * 1024,
            checksum: [2u8; 32],
            file_offset: Some(4 * 1024 * 1024),
            written_at: None,
            client_write_seq: Some(1),
            file_id: Some(metadata.id),
        };
        update.chunk_locations = Arc::new(vec![loc1.clone()]);

        // Exercise merge_file_metadata directly against the in-memory `metadata` (which
        // still carries loc0) rather than a round-trip through store.get_file() — the
        // persisted record never carries chunk_locations anymore (see put_file_in_txn's
        // doc comment), so a re-fetch would hand merge_file_metadata an empty "existing"
        // and defeat the point of this test. merge_file_metadata's union logic is still a
        // real, correct pure function (put_file_in_txn still calls it, and its Stale(_)
        // return value carries the merged result to callers like handle_disseminate_metadata's
        // correction-forwarding) — it's just that put_file_in_txn's own on-disk existing_opt
        // is always empty in production now, so the actual chunk-preservation work happens
        // one layer up, via each caller's own reconcile-against-chunk_map step.
        let (merged, _is_stale) = MetadataStore::merge_file_metadata(Some(&metadata), &update);
        store.put_file(&update).unwrap();

        let offsets: std::collections::HashSet<Option<u64>> = merged.chunk_locations
            .iter().map(|l| l.file_offset).collect();
        assert!(offsets.contains(&loc0.file_offset), "chunk from the earlier push must survive a later partial update");
        assert!(offsets.contains(&loc1.file_offset), "chunk from the later partial update must be present");
        assert_eq!(merged.chunk_locations.len(), 2, "must be a union, not just the incoming payload");
    }

    /// Regression test for the fourth and deepest layer of the T48 background-tick
    /// chunk-loss bug (2026-07-07): a push that arrives OUT OF ORDER with a lower
    /// write_seq than what's already persisted must not lose either write's chunk
    /// data. Updated 2026-07-16 for Phase 4: put_file_in_txn's own merge can no
    /// longer recover a chunk from an EARLIER separate put_file call — its on-disk
    /// `existing_opt` read is always chunk_locations-empty now (see put_file_in_txn's
    /// doc comment), so a merge fed only the stale push's own incoming chunk can only
    /// ever echo that one chunk back, not reunite it with an earlier call's chunk.
    /// That's expected, not a regression: every real chunk write ALSO calls
    /// put_chunk_location independently of the metadata push (see e.g.
    /// handle_put_file_metadata), so CHUNK_TABLE — not this FILE_TABLE merge — is
    /// what actually guarantees no chunk from either write is lost. This test now
    /// verifies that guarantee where it actually lives.
    #[test]
    fn test_put_file_unions_chunks_from_an_out_of_order_stale_push() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();
        let node = NodeId::new();

        let mut metadata = FileMetadata::new("/stale_union.bin".to_string(), FileType::RegularFile);
        metadata.write_seq = 2;
        let loc1 = dfs_common::ChunkLocation {
            chunk_id: ChunkId::from_hash([4u8; 32]),
            nodes: vec![node],
            size: 4 * 1024 * 1024,
            checksum: [4u8; 32],
            file_offset: Some(4 * 1024 * 1024),
            written_at: None,
            client_write_seq: Some(2),
            file_id: Some(metadata.id),
        };
        metadata.chunk_locations = Arc::new(vec![loc1.clone()]);
        store.put_file(&metadata).unwrap();
        store.put_chunk_location(&loc1).unwrap();

        // A delayed push for the SAME file arrives after, but describes an EARLIER
        // write (write_seq=1 < stored 2) — e.g. two background-tick pushes raced on
        // the network. It carries a chunk at a different offset that the stored
        // record has never seen.
        let mut stale_push = metadata.clone();
        stale_push.write_seq = 1;
        let loc0 = dfs_common::ChunkLocation {
            chunk_id: ChunkId::from_hash([3u8; 32]),
            nodes: vec![node],
            size: 4 * 1024 * 1024,
            checksum: [3u8; 32],
            file_offset: Some(0),
            written_at: None,
            client_write_seq: Some(1),
            file_id: Some(metadata.id),
        };
        stale_push.chunk_locations = Arc::new(vec![loc0.clone()]);
        let result = store.put_file(&stale_push).unwrap();
        store.put_chunk_location(&loc0).unwrap();
        assert!(matches!(result, PutFileResult::Stale(_)), "lower write_seq than stored must still report Stale");
        if let PutFileResult::Stale(merged) = result {
            assert_eq!(merged.write_seq, 2, "scalar fields must come from the newer (existing) side, not the stale push");
        }

        // Both chunks must be discoverable via CHUNK_TABLE regardless of which side
        // "won" the metadata race — this is the real, current safety net.
        let mut found_offsets: std::collections::HashSet<Option<u64>> = std::collections::HashSet::new();
        store.scan_chunk_locations(|loc| {
            if loc.file_id == Some(metadata.id) {
                found_offsets.insert(loc.file_offset);
            }
            true
        }).unwrap();
        assert!(found_offsets.contains(&loc0.file_offset), "chunk from the out-of-order stale push must survive, not be silently dropped");
        assert!(found_offsets.contains(&loc1.file_offset), "chunk from the newer record must still be present");
    }

    /// Regression test for a real bug found live via T48/T22 under full local-suite
    /// load (2026-07-08): a file_offset:None chunk_locations entry carries no
    /// reliable position in the current chunk_idx-keyed file model (every real
    /// write path sets file_offset — the read side already treats a None-offset
    /// entry as stale/orphaned and skips it, see update_chunk_map_window in
    /// read_engine.rs). merge_file_metadata's Rule 1 (same chunk_id already
    /// present) used to blindly take the incoming entry's fields when a chunk_id
    /// matched — including a missing file_offset — clobbering a perfectly valid,
    /// correctly-positioned entry and losing its coordinate. Once merged in, the
    /// corrupted entry then self-propagated through every subsequent merge and
    /// metadata fetch: observed live as dfs-admin reporting one fewer chunk than
    /// was actually written (T48c) and, more severely, patched regions reading
    /// back as all-zero (T22c) because the position that should have resolved to
    /// real content had been overwritten by an unpositioned duplicate.
    #[test]
    fn test_put_file_ignores_none_offset_entry_sharing_a_positioned_chunk_id() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();
        let node = NodeId::new();

        let shared_chunk_id = ChunkId::from_hash([9u8; 32]);

        let mut metadata = FileMetadata::new("/none_offset_clobber.bin".to_string(), FileType::RegularFile);
        metadata.write_seq = 1;
        let real_chunk0 = dfs_common::ChunkLocation {
            chunk_id: shared_chunk_id,
            nodes: vec![node],
            size: 4 * 1024 * 1024,
            checksum: shared_chunk_id.hash,
            file_offset: Some(0),
            written_at: None,
            client_write_seq: Some(1),
            file_id: Some(metadata.id),
        };
        metadata.chunk_locations = Arc::new(vec![real_chunk0.clone()]);
        store.put_file(&metadata).unwrap();

        // A second push carries the SAME chunk_id (e.g. a stray/legacy record — see
        // this test's doc comment) but with file_offset stripped. This must not be
        // allowed to overwrite chunk 0's real, positioned entry.
        let mut stray_push = metadata.clone();
        stray_push.write_seq = 2;
        let stray_entry = dfs_common::ChunkLocation {
            file_offset: None,
            client_write_seq: Some(2),
            ..real_chunk0.clone()
        };
        stray_push.chunk_locations = Arc::new(vec![stray_entry]);

        // Exercise merge_file_metadata directly against the in-memory `metadata` (which
        // still carries real_chunk0) — stray_push.write_seq=2 makes this a Stored result,
        // which carries no data, and the persisted record never carries chunk_locations
        // anymore (see put_file_in_txn's doc comment), so neither the return value nor a
        // get_file() round-trip can observe the merge result for this case.
        let (merged, _is_stale) = MetadataStore::merge_file_metadata(Some(&metadata), &stray_push);
        store.put_file(&stray_push).unwrap();

        assert_eq!(merged.chunk_locations.len(), 1,
            "the None-offset duplicate must be dropped, not appended as a second entry");
        assert_eq!(merged.chunk_locations[0].file_offset, Some(0),
            "chunk 0's real position must survive — the None-offset entry must never clobber it");
        assert_eq!(merged.chunk_locations[0].chunk_id, shared_chunk_id);
    }

    #[test]
    fn test_get_file_by_path() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();

        let metadata = FileMetadata::new("/data/file.dat".to_string(), FileType::RegularFile);
        store.put_file(&metadata).unwrap();

        let retrieved = store.get_file_by_path("/data/file.dat").unwrap().unwrap();
        assert_eq!(retrieved.id, metadata.id);
    }

    #[test]
    fn test_delete_file() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();

        let metadata = FileMetadata::new("/temp.txt".to_string(), FileType::RegularFile);
        store.put_file(&metadata).unwrap();

        assert!(store.get_file(&metadata.id).unwrap().is_some());

        store.delete_file(&metadata.id).unwrap();

        assert!(store.get_file(&metadata.id).unwrap().is_none());
        assert!(store.get_file_by_path("/temp.txt").unwrap().is_none());
    }

    #[test]
    fn test_list_directory() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();

        let file1 = FileMetadata::new("/dir/file1.txt".to_string(), FileType::RegularFile);
        let file2 = FileMetadata::new("/dir/file2.txt".to_string(), FileType::RegularFile);
        let file3 = FileMetadata::new("/other/file3.txt".to_string(), FileType::RegularFile);

        store.put_file(&file1).unwrap();
        store.put_file(&file2).unwrap();
        store.put_file(&file3).unwrap();

        let dir_contents = store.list_directory("/dir").unwrap();
        assert_eq!(dir_contents.len(), 2);
    }

    #[test]
    fn test_chunk_location() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();

        let chunk_id = ChunkId::from_hash([1u8; 32]);
        let location = ChunkLocation {
            chunk_id,
            nodes: vec![NodeId::new(), NodeId::new()],
            size: 4096,
            checksum: [2u8; 32],
            file_offset: None,
            written_at: None,
            client_write_seq: None,
            file_id: None,
        };

        store.put_chunk_location(&location).unwrap();

        let retrieved = store.get_chunk_location(&chunk_id).unwrap().unwrap();
        assert_eq!(retrieved.nodes.len(), 2);
        assert_eq!(retrieved.size, 4096);
    }

    /// Regression test for the staging gluster1 hang (2026-06-19):
    /// handle_replicate_chunk_location (server.rs) used to call put_chunk_location()
    /// directly on the Tokio worker thread — a synchronous redb begin_write()/commit()
    /// with no .await inside. A spawned async task with no yield points runs to
    /// completion in a single poll, monopolizing whichever worker thread picks it up
    /// for the task's entire duration. Under concurrent load (many such tasks queued
    /// against a small worker pool — exactly what a chunk-write/heal-storm burst
    /// produces on the leader), that starved every *other* task on the runtime,
    /// including unrelated requests like `cluster status`.
    ///
    /// Every production call site now goes through put_chunk_location_async (which
    /// offloads to spawn_blocking) instead of calling put_chunk_location directly.
    /// This test proves the wrapper actually fixes the starvation, using the same
    /// concurrent-flood shape that reproduced the bug against the raw sync method.
    ///
    /// This reproduces the mechanism directly against the metadata layer, independent
    /// of disk speed — the E2E version of this test (test_local_suite.sh T42) passed
    /// even under heavy concurrency on fast local storage, because individual redb
    /// commits complete in the low single-digit milliseconds here; staging's hang took
    /// over an hour to surface because slower storage plus sustained load (a 16MB/s+
    /// VM disk write competing for the same physical disk) widened the same starvation
    /// window from milliseconds to effectively forever.
    ///
    /// A "heartbeat" task with a real yield point (tokio::time::sleep) should never be
    /// delayed by more than its own sleep duration plus scheduling jitter — unrelated
    /// async work must not be able to block it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_put_chunk_location_async_does_not_starve_runtime_under_concurrency() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(MetadataStore::new(temp_dir.path().to_path_buf()).unwrap());

        let max_gap_ms = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let heartbeat = {
            let max_gap_ms = max_gap_ms.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                let mut last = std::time::Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    let now = std::time::Instant::now();
                    let gap = now.duration_since(last).as_millis() as u64;
                    max_gap_ms.fetch_max(gap, Ordering::Relaxed);
                    last = now;
                }
            })
        };

        // More concurrent flooders than worker threads, each issuing a burst of
        // synchronous chunk-location writes with no yield points in between —
        // mirrors a burst of concurrent ReplicateChunkLocation RPCs landing on the
        // leader during a write/heal storm.
        let mut handles = Vec::new();
        for i in 0..32u8 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                for j in 0..40u8 {
                    let mut hash = [0u8; 32];
                    hash[0] = i;
                    hash[1] = j;
                    let location = ChunkLocation {
                        chunk_id: ChunkId::from_hash(hash),
                        nodes: vec![NodeId::new(), NodeId::new()],
                        size: 4096,
                        checksum: [0u8; 32],
                        file_offset: None,
                        written_at: None,
                        client_write_seq: None,
                        file_id: None,
                    };
                    store.put_chunk_location_async(location).await.unwrap();
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        stop.store(true, Ordering::Relaxed);
        heartbeat.await.unwrap();

        let gap = max_gap_ms.load(Ordering::Relaxed);
        assert!(
            gap < 150,
            "heartbeat task starved for {}ms — put_chunk_location_async should offload \
             to spawn_blocking and never block a Tokio worker thread for this long",
            gap
        );
    }

    #[test]
    fn test_pending_healing_round_trip() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();

        let chunk_a = ChunkId::from_hash([3u8; 32]);
        let chunk_b = ChunkId::from_hash([4u8; 32]);

        store.put_pending_healing(&chunk_a, 1_000).unwrap();
        store.put_pending_healing(&chunk_b, 2_000).unwrap();

        let mut inventory = store.get_pending_healing_inventory().unwrap();
        inventory.sort_by_key(|(_, secs)| *secs);
        assert_eq!(inventory, vec![(chunk_a, 1_000), (chunk_b, 2_000)]);

        store.delete_pending_healing(&chunk_a).unwrap();
        let inventory = store.get_pending_healing_inventory().unwrap();
        assert_eq!(inventory, vec![(chunk_b, 2_000)]);
    }

    #[test]
    fn test_chunk_seq_round_trip() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();
        let file_id = FileId::new();

        // Lazily-created table: no chunk_seq recorded yet reads as None, not an error.
        assert_eq!(store.get_chunk_seq(file_id, 5).unwrap(), None);

        store.put_chunk_seq(file_id, 5, 1).unwrap();
        assert_eq!(store.get_chunk_seq(file_id, 5).unwrap(), Some(1));

        // A different chunk_idx on the same file is a distinct slot.
        assert_eq!(store.get_chunk_seq(file_id, 6).unwrap(), None);

        // Overwriting advances the recorded value.
        store.put_chunk_seq(file_id, 5, 2).unwrap();
        assert_eq!(store.get_chunk_seq(file_id, 5).unwrap(), Some(2));
    }

    #[test]
    fn test_leader_state_round_trip() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();

        // No leader recorded yet.
        assert_eq!(store.get_leader_state().unwrap(), (None, None));

        let node_id = NodeId::new();
        store.put_leader_state(node_id, 12_345).unwrap();

        let (leader, since) = store.get_leader_state().unwrap();
        assert_eq!(leader, Some(node_id));
        assert_eq!(since, Some(12_345));

        // Overwrite with a different node/time.
        let node_id2 = NodeId::new();
        store.put_leader_state(node_id2, 67_890).unwrap();
        let (leader, since) = store.get_leader_state().unwrap();
        assert_eq!(leader, Some(node_id2));
        assert_eq!(since, Some(67_890));
    }

    #[test]
    fn test_compact_db_preserves_existing_data() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();

        for i in 0..200 {
            let mut m = FileMetadata::new(format!("/seed_{}", i), FileType::RegularFile);
            m.size = i as u64;
            store.put_file(&m).unwrap();
        }
        let chunk_id = ChunkId::from_hash([7u8; 32]);
        let location = ChunkLocation {
            chunk_id,
            nodes: vec![NodeId::new(), NodeId::new()],
            size: 4096,
            checksum: [9u8; 32],
            file_offset: None,
            written_at: None,
            client_write_seq: None,
            file_id: None,
        };
        store.put_chunk_location(&location).unwrap();

        let (before, after) = store.compact_db().unwrap();
        assert!(before > 0, "size_before should reflect the live file before compaction");
        assert!(after > 0, "size_after should reflect the new file after compaction");

        for i in 0..200 {
            let path = format!("/seed_{}", i);
            let retrieved = store.get_file_by_path(&path).unwrap()
                .unwrap_or_else(|| panic!("lost file {} across compact_db()", path));
            assert_eq!(retrieved.size, i as u64);
        }
        let retrieved = store.get_chunk_location(&chunk_id).unwrap().unwrap();
        assert_eq!(retrieved.nodes.len(), 2);
        assert_eq!(retrieved.size, 4096);
    }

    /// Regression test for the 2026-07-06 T44 finding: diff_bytes_table's full-table
    /// scan meant every compaction catch-up pass cost O(live table size), not O(what
    /// actually changed) — "convergence" tracked the change count, never the scan
    /// cost, so a real run's ~50MB table held Phase 3's exclusive lock for ~1.1s even
    /// though almost nothing had changed since the prior pass. diff_all_tables_tracked
    /// fixes this for FILE_TABLE/PATH_TABLE via dirty-key tracking: seed a table large
    /// enough that a full scan would be measurably slow, mark only a handful of keys
    /// dirty, and confirm the tracked diff pass stays fast and reports only those keys
    /// as changed (not all seeded rows).
    #[test]
    fn test_diff_all_tables_tracked_is_fast_regardless_of_table_size() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();

        const SEED_COUNT: usize = 5_000;
        for i in 0..SEED_COUNT {
            let mut m = FileMetadata::new(format!("/seed_{}", i), FileType::RegularFile);
            m.size = i as u64;
            store.put_file(&m).unwrap();
        }

        let shadow_dir = TempDir::new().unwrap();
        let shadow_db = Database::builder()
            .create(shadow_dir.path().join("shadow.redb"))
            .unwrap();

        // Mirror Phase 1: a full copy so the shadow starts in sync with everything
        // seeded above — otherwise a diff pass against an empty shadow reports every
        // row as "changed" regardless of code path, which would hide the actual bug
        // (full-table-scan *cost* even when the *count* of real changes is tiny).
        {
            let live = store.db.read();
            let src_txn = live.begin_read().unwrap();
            let dst_txn = shadow_db.begin_write().unwrap();
            let finished = MetadataStore::copy_all_tables(&src_txn, &dst_txn, std::time::Instant::now() + std::time::Duration::from_secs(60)).unwrap();
            assert!(finished, "copy_all_tables should complete well within a 60s budget for this small test dataset");
            dst_txn.commit().unwrap();
        }

        // Simulate "Phase 2 has already converged, this is one more catch-up pass" —
        // only the writes below should register as dirty for it.
        store.dirty_files.lock().unwrap().clear();
        store.dirty_paths.lock().unwrap().clear();

        const NEW_COUNT: usize = 5;
        for i in 0..NEW_COUNT {
            let mut m = FileMetadata::new(format!("/fresh_{}", i), FileType::RegularFile);
            m.size = 999;
            store.put_file(&m).unwrap();
        }

        let live = store.db.read();
        let src_txn = live.begin_read().unwrap();
        let dst_txn = shadow_db.begin_write().unwrap();

        let start = std::time::Instant::now();
        let changed = store.diff_all_tables_tracked(&src_txn, &dst_txn).unwrap();
        let elapsed = start.elapsed();

        dst_txn.commit().unwrap();

        // Each fresh file touches both FILE_TABLE and PATH_TABLE.
        assert_eq!(
            changed, NEW_COUNT * 2,
            "only the {} fresh writes (x2 tables) should be reported as changed, not all {} seeded rows",
            NEW_COUNT, SEED_COUNT
        );
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "diff_all_tables_tracked took {:?} for {} dirty keys against a {}-row table \
             — should be near-instant, not scale with table size",
            elapsed, NEW_COUNT, SEED_COUNT
        );
    }

    /// Regression test for a real deployment finding: a rejoining follower (gluster2)
    /// was still short 1 file after ~90s of catch-up out of ~10,638 queued records.
    /// handle_disseminate_metadata applied each queued item via an individual
    /// put_file call — one full redb transaction + commit per record — which is
    /// almost entirely per-transaction overhead at that scale, not real work.
    /// put_files_batch shares ONE transaction across the whole batch. Confirms both
    /// that per-item semantics are unchanged (stale rejection inside a shared batch
    /// transaction must not affect other items) and that it's meaningfully faster.
    #[test]
    fn test_put_files_batch_matches_individual_puts_and_is_faster() {
        let temp_dir = TempDir::new().unwrap();
        let store = MetadataStore::new(temp_dir.path().to_path_buf()).unwrap();

        // --- Correctness: new record, updated record, and a stale resend, all in
        // one shared-transaction batch. ---
        let mut existing = FileMetadata::new("/existing.txt".to_string(), FileType::RegularFile);
        existing.write_seq = 5;
        store.put_file(&existing).unwrap();

        let new_file = FileMetadata::new("/new.txt".to_string(), FileType::RegularFile);

        let mut updated_existing = existing.clone();
        updated_existing.write_seq = 6;
        updated_existing.size = 4096;

        let mut stale_resend = existing.clone();
        stale_resend.write_seq = 1; // < stored write_seq=5 — must be rejected as Stale

        let batch = vec![new_file.clone(), updated_existing.clone(), stale_resend.clone()];
        let results = store.put_files_batch(&batch).unwrap();
        assert!(matches!(results[0], PutFileResult::Stored), "brand-new file must store");
        assert!(matches!(results[1], PutFileResult::Stored), "newer write_seq must store");
        assert!(matches!(results[2], PutFileResult::Stale(_)), "older write_seq must be rejected as stale");

        assert!(store.get_file(&new_file.id).unwrap().is_some(), "new file lost in batch");
        let stored_existing = store.get_file(&existing.id).unwrap().unwrap();
        assert_eq!(stored_existing.write_seq, 6, "stale resend must not have clobbered the real update");
        assert_eq!(stored_existing.size, 4096, "update from the same batch must have been applied");

        // --- Performance: individual put_file calls vs. one put_files_batch call
        // over an equivalent number of brand-new records. ---
        const COUNT: usize = 2000;
        let individual_dir = TempDir::new().unwrap();
        let individual_store = MetadataStore::new(individual_dir.path().to_path_buf()).unwrap();
        let individual_items: Vec<FileMetadata> = (0..COUNT)
            .map(|i| FileMetadata::new(format!("/individual_{}", i), FileType::RegularFile))
            .collect();
        let start_individual = std::time::Instant::now();
        for m in &individual_items {
            individual_store.put_file(m).unwrap();
        }
        let individual_elapsed = start_individual.elapsed();

        let batch_dir = TempDir::new().unwrap();
        let batch_store = MetadataStore::new(batch_dir.path().to_path_buf()).unwrap();
        let batch_items: Vec<FileMetadata> = (0..COUNT)
            .map(|i| FileMetadata::new(format!("/batched_{}", i), FileType::RegularFile))
            .collect();
        let start_batch = std::time::Instant::now();
        let batch_results = batch_store.put_files_batch(&batch_items).unwrap();
        let batch_elapsed = start_batch.elapsed();

        assert_eq!(batch_results.len(), COUNT);
        assert!(batch_results.iter().all(|r| matches!(r, PutFileResult::Stored)));
        for m in &batch_items {
            assert!(batch_store.get_file(&m.id).unwrap().is_some(), "lost {} in batched put", m.path);
        }

        assert!(
            batch_elapsed * 3 < individual_elapsed,
            "put_files_batch ({:?} for {} records) should be at least 3x faster than \
             {} individual put_file calls ({:?}) — the whole point is amortizing the \
             per-transaction commit cost across the batch",
            batch_elapsed, COUNT, COUNT, individual_elapsed
        );
    }

    #[test]
    fn test_compact_db_preserves_concurrent_writes() {
        let temp_dir = TempDir::new().unwrap();
        let store = std::sync::Arc::new(MetadataStore::new(temp_dir.path().to_path_buf()).unwrap());

        // Seed enough rows that Phase 1's full-table copy takes measurable time,
        // giving the concurrent writer thread below a real chance to land writes
        // while compact_db() is in its unlocked copy/catch-up phases rather than
        // only before or after.
        for i in 0..3000 {
            let mut m = FileMetadata::new(format!("/seed_{}", i), FileType::RegularFile);
            m.size = i as u64;
            store.put_file(&m).unwrap();
        }

        let store2 = std::sync::Arc::clone(&store);
        let writer = std::thread::spawn(move || {
            for i in 0..500 {
                let mut m = FileMetadata::new(format!("/concurrent_{}", i), FileType::RegularFile);
                m.size = 100_000 + i as u64;
                store2.put_file(&m).unwrap();
            }
        });

        store.compact_db().unwrap();
        writer.join().unwrap();

        // Every concurrent write must have survived, regardless of whether it landed
        // before, during, or after any particular compaction phase — this is the
        // correctness property the catch-up passes (diff_all_tables) exist to provide.
        for i in 0..500 {
            let path = format!("/concurrent_{}", i);
            assert!(
                store.get_file_by_path(&path).unwrap().is_some(),
                "lost concurrent write to {} during compact_db()", path
            );
        }
        for i in 0..3000 {
            let path = format!("/seed_{}", i);
            assert!(
                store.get_file_by_path(&path).unwrap().is_some(),
                "lost seeded file {} during compact_db()", path
            );
        }
    }

    #[test]
    fn test_compact_db_defers_under_sustained_churn() {
        // Repro for a real bug found via the local suite's T44 check: a "storm" test
        // (T21, thousands of rapid-fire deletes) kept the live db churning continuously
        // through Phase 1/2, so the catch-up passes never dropped below the convergence
        // threshold — Phase 3 inherited a large diff and held the exclusive lock for
        // ~458ms, relocating the original blocking problem instead of fixing it.
        // compact_db() must detect non-convergence and abort cleanly (Err) rather than
        // ever handing Phase 3 a large diff.
        //
        // Uses compact_db_with_budget() with a tiny budget/threshold rather than the
        // production defaults (5s / 64 rows) — reproducing non-convergence against the
        // real defaults needs a huge dataset and several seconds of sustained writes to
        // reliably outrun them, which is correct for production but far too slow for a
        // unit test. A 5ms budget and a threshold of 1 row exercises the exact same
        // code path deterministically and fast.
        let temp_dir = TempDir::new().unwrap();
        let store = std::sync::Arc::new(MetadataStore::new(temp_dir.path().to_path_buf()).unwrap());

        const SEED_COUNT: usize = 200;
        for i in 0..SEED_COUNT {
            let mut m = FileMetadata::new(format!("/seed_{}", i), FileType::RegularFile);
            m.size = i as u64;
            store.put_file(&m).unwrap();
        }

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let store2 = std::sync::Arc::clone(&store);
        let stop2 = std::sync::Arc::clone(&stop);
        let storm = std::thread::spawn(move || {
            let mut i = 0u64;
            while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                let mut m = FileMetadata::new(format!("/storm_{}", i), FileType::RegularFile);
                m.size = i;
                store2.put_file(&m).unwrap();
                i += 1;
            }
        });

        let result = store.compact_db_with_budget(std::time::Duration::from_secs(30), std::time::Duration::from_millis(5), 1);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        storm.join().unwrap();

        assert!(result.is_err(), "compact_db() should defer (Err) under sustained churn, not block on a large Phase 3 diff");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("deferred"), "unexpected error: {}", msg);

        // Nothing should have been lost or corrupted by the aborted attempt — the live
        // db must be untouched (no swap happened).
        for i in 0..SEED_COUNT {
            let path = format!("/seed_{}", i);
            assert!(store.get_file_by_path(&path).unwrap().is_some(), "lost seeded file {} after deferred compaction", path);
        }

        // A subsequent compaction, once churn has stopped, must succeed normally.
        let (before, after) = store.compact_db().unwrap();
        assert!(before > 0 && after > 0);
        for i in 0..SEED_COUNT {
            let path = format!("/seed_{}", i);
            assert!(store.get_file_by_path(&path).unwrap().is_some(), "lost seeded file {} after successful follow-up compaction", path);
        }
    }

    #[test]
    fn test_compact_db_defers_when_phase1_exceeds_budget() {
        // Repro for the 2026-07-12 gluster1 incident: Phase 1's full-table copy has no
        // time budget of its own, unlike Phase 2's catchup_budget. Under a sustained
        // heavy-write benchmark, Phase 1 competed with a flood of concurrent write
        // commits for disk I/O and ran long enough to blow past server.rs's outer 60s
        // wedge-detection timeout — which can't cancel an in-flight spawn_blocking call,
        // so the node self-restarted. Phase 1 must instead defer cleanly (Err) the same
        // way Phase 2 already does, well before that outer timeout.
        //
        // A near-zero phase1_budget deterministically exercises the bail-before-Phase-2
        // path without needing real I/O contention or a large dataset — same rationale as
        // test_compact_db_defers_under_sustained_churn's tiny catchup_budget/threshold.
        let temp_dir = TempDir::new().unwrap();
        let store = std::sync::Arc::new(MetadataStore::new(temp_dir.path().to_path_buf()).unwrap());

        const SEED_COUNT: usize = 200;
        for i in 0..SEED_COUNT {
            let mut m = FileMetadata::new(format!("/seed_{}", i), FileType::RegularFile);
            m.size = i as u64;
            store.put_file(&m).unwrap();
        }

        let result = store.compact_db_with_budget(
            std::time::Duration::from_nanos(1), std::time::Duration::from_secs(5), 64,
        );

        assert!(result.is_err(), "compact_db() should defer (Err) when Phase 1 exceeds its own budget, not run unbounded");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("deferred") && msg.contains("Phase 1"), "unexpected error: {}", msg);

        // No shadow file left behind, and the live db must be untouched (no swap
        // happened) — a deferred Phase 1 must clean up after itself same as Phase 2.
        let shadow_path = temp_dir.path().join("metadata.redb.shadow");
        assert!(!shadow_path.exists(), "deferred Phase 1 left a shadow db file behind");
        for i in 0..SEED_COUNT {
            let path = format!("/seed_{}", i);
            assert!(store.get_file_by_path(&path).unwrap().is_some(), "lost seeded file {} after Phase-1-deferred compaction", path);
        }

        // A subsequent compaction with a normal budget must succeed.
        let (before, after) = store.compact_db().unwrap();
        assert!(before > 0 && after > 0);
        for i in 0..SEED_COUNT {
            let path = format!("/seed_{}", i);
            assert!(store.get_file_by_path(&path).unwrap().is_some(), "lost seeded file {} after successful follow-up compaction", path);
        }
    }

    #[test]
    fn test_copy_bytes_table_bails_mid_table_on_deadline() {
        // Root-caused 2026-07-15 (gluster1 incident): copy_all_tables used to check its
        // Phase 1 budget only *between* whole tables — a single large table's own
        // copy_bytes_table call was entirely unbounded once started. Under sustained
        // disk contention, one table's copy blew past not just the stated phase1_budget
        // but server.rs's outer 60s wedge-detection timeout (which can't cancel an
        // in-flight spawn_blocking call), forcing a node restart — the budget existed
        // but never actually bounded wall-clock time.
        //
        // test_compact_db_defers_when_phase1_exceeds_budget already covers the
        // near-zero-budget case, but that's caught by copy_all_tables' pre-existing
        // between-tables check *before* copy_bytes_table is ever called — it never
        // exercises the new intra-table check at all. This test proves the fix directly:
        // a single table large enough that a full copy provably takes longer than the
        // budget must still leave the copy partial (not 0, not all) when the deadline
        // lands mid-copy — i.e. the bail happened during the table, not before or after.
        let temp_dir = TempDir::new().unwrap();
        let src_db = Database::create(temp_dir.path().join("src.redb")).unwrap();
        const ROW_COUNT: usize = 300_000;
        {
            let txn = src_db.begin_write().unwrap();
            {
                let mut table = txn.open_table(FILE_TABLE).unwrap();
                for i in 0..ROW_COUNT {
                    let key = format!("/row_{:08}", i);
                    table.insert(key.as_str(), &b"x"[..]).unwrap();
                }
            }
            txn.commit().unwrap();
        }

        let dst_db = Database::create(temp_dir.path().join("dst.redb")).unwrap();
        let src_txn = src_db.begin_read().unwrap();
        let dst_txn = dst_db.begin_write().unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(30);
        let finished = MetadataStore::copy_bytes_table(&src_txn, &dst_txn, FILE_TABLE, deadline).unwrap();
        assert!(!finished, "copy_bytes_table should report unfinished when its deadline lands mid-copy");

        let copied = {
            let table = dst_txn.open_table(FILE_TABLE).unwrap();
            table.len().unwrap() as usize
        };
        assert!(copied > 0, "copy_bytes_table bailed before copying anything — deadline check fired too early to be a mid-table bail");
        assert!(copied < ROW_COUNT, "copy_bytes_table copied every row despite reporting unfinished — the deadline check isn't actually bounding the loop");
    }
}
