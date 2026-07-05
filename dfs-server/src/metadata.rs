use anyhow::{Context, Result};
use dfs_common::{ChunkId, ChunkLocation, FileId, FileMetadata, NodeId};
use redb::{Database, Durability, ReadableTable, TableDefinition};
// On Linux, Durability::Eventual calls fdatasync (same as Immediate). Only the macOS
// backend (F_BARRIERFSYNC) distinguishes them. Durability::None writes to the OS page
// cache without fdatasync — fast, immediately visible to reads, survives process crashes,
// only lost on kernel panic/power failure. Acceptable with 5-way replication.
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
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

/// old_chunk_id hex string → bincode(PatchJournalEntry).
///
/// Write-ahead undo record for in-place chunk patching. Written (durably
/// committed) before any byte of the existing chunk file is touched, and
/// deleted only after the patch's rename + chunk_location update have both
/// committed. If a leftover entry is found at startup, the old chunk file's
/// presence tells us how far the in-flight patch got: still present means
/// the rename never happened (replay the undo bytes to restore it exactly);
/// absent means the rename already completed and the data under
/// new_chunk_id is intact (just discard the entry — same residual
/// metadata-propagation gap the orphan reconciliation sweep already covers,
/// not a new failure mode).
const CHUNK_PATCH_JOURNAL_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("chunk_patch_journal");

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

/// Write-ahead undo record for an in-place chunk patch. `patches` is recorded
/// in application order so recovery can unwind multiple sub-patches (from
/// MultiPatch) in reverse.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PatchJournalEntry {
    pub old_chunk_id: ChunkId,
    pub new_chunk_id: ChunkId,
    /// (offset within chunk, original bytes that were about to be overwritten)
    pub patches: Vec<(usize, Vec<u8>)>,
}

/// Metadata storage using redb embedded database.
/// Replaces sled to eliminate the u8 fragment-count panic under heavy write loads.
pub struct MetadataStore {
    db: RwLock<Database>,
    db_path: PathBuf,
    /// Counts Durability::None commits since the last Durability::Immediate one.
    /// See next_write_durability() for why this exists.
    non_durable_commits: AtomicU64,
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
            txn.open_table(CHUNK_PATCH_JOURNAL_TABLE)?;
            txn.commit()?;
        }

        info!("Initialized redb metadata store at {:?}", db_path);

        Ok(Self { db: RwLock::new(db), db_path, non_durable_commits: AtomicU64::new(0) })
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
        let n = self.non_durable_commits.fetch_add(1, Ordering::Relaxed) + 1;
        if n % Self::DURABILITY_FLUSH_INTERVAL == 0 {
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
        let to_repair: Vec<(String, Vec<u8>)> = {
            let _db = self.db.read().unwrap();
            let txn = _db.begin_read()?;
            let file_table = txn.open_table(FILE_TABLE)?;
            let path_table = txn.open_table(PATH_TABLE)?;
            let mut repairs = Vec::new();
            for item in file_table.range::<&str>(..)? {
                let (_, v) = item?;
                let m = match bincode::deserialize::<FileMetadata>(v.value()) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if path_table.get(m.path.as_str())?.is_none() {
                    repairs.push((m.path.clone(), v.value().to_vec()));
                }
            }
            repairs
        };

        let repaired = to_repair.len();
        if !to_repair.is_empty() {
            let _db = self.db.read().unwrap();
            let mut txn = _db.begin_write()?;
            txn.set_durability(self.next_write_durability());
            {
                let mut path_table = txn.open_table(PATH_TABLE)?;
                for (path, bytes) in &to_repair {
                    warn!("Repaired missing path index for: {}", path);
                    path_table.insert(path.as_str(), bytes.as_slice())?;
                }
            }
            txn.commit()?;
        }

        // Pass 2: find path index entries whose file record no longer exists.
        let stale_paths: Vec<String> = {
            let _db = self.db.read().unwrap();
            let txn = _db.begin_read()?;
            let file_table = txn.open_table(FILE_TABLE)?;
            let path_table = txn.open_table(PATH_TABLE)?;
            let mut stale = Vec::new();
            for item in path_table.range::<&str>(..)? {
                let (k, v) = item?;
                if let Ok(m) = bincode::deserialize::<FileMetadata>(v.value()) {
                    let fid_str = format!("{}", m.id);
                    if file_table.get(fid_str.as_str())?.is_none() {
                        stale.push(k.value().to_string());
                    }
                }
            }
            stale
        };

        let stale_count = stale_paths.len();
        if !stale_paths.is_empty() {
            let _db = self.db.read().unwrap();
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

    /// Store file metadata.
    pub fn put_file(&self, metadata: &FileMetadata) -> Result<PutFileResult> {
        let file_id_str = format!("{}", metadata.id);
        let path_str = metadata.path.as_str();

        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());

        // Read-before-write: open both tables once and keep them alive for the
        // duration of the transaction so all reads and writes are atomic.
        let mut file_table = txn.open_table(FILE_TABLE)?;
        let mut path_table = txn.open_table(PATH_TABLE)?;

        // If a different file ID already exists at this path, remove the stale file record.
        {
            let old_id_str: Option<String> = match path_table.get(path_str)? {
                Some(v) => bincode::deserialize::<FileMetadata>(v.value())
                    .ok()
                    .filter(|m| m.id != metadata.id)
                    .map(|m| format!("{}", m.id)),
                None => None,
            };
            if let Some(old_id) = old_id_str {
                if let Err(e) = file_table.remove(old_id.as_str()) {
                    warn!("Failed to remove stale file record {} for path {}: {}", old_id, metadata.path, e);
                } else {
                    debug!("Removed stale file record {} superseded by {} at path {}", old_id, metadata.id, metadata.path);
                }
            }
        }

        // Merge chunk_locations with any existing same-ID record.
        let existing_opt: Option<FileMetadata> = match file_table.get(file_id_str.as_str())? {
            Some(v) => bincode::deserialize::<FileMetadata>(v.value()).ok(),
            None => None,
        };

        let merged_metadata: Option<FileMetadata>;
        let metadata_to_store: &FileMetadata = if let Some(existing) = existing_opt {
            // Drop stale incoming write if existing is strictly newer.
            if existing.write_seq > 0
                && metadata.write_seq > 0
                && existing.write_seq > metadata.write_seq
            {
                debug!(
                    "Dropping stale metadata for {} (existing write_seq={} > incoming={})",
                    metadata.path, existing.write_seq, metadata.write_seq
                );
                return Ok(PutFileResult::Stale(existing));
            }

            // Merge chunk locations — Rule 1 (same chunk_id: merge node lists),
            // Rule 2 (same offset, different chunk_id: keep newer by client_write_seq).
            let existing_by_id: std::collections::HashMap<ChunkId, &dfs_common::ChunkLocation> =
                existing.chunk_locations.iter().map(|loc| (loc.chunk_id, loc)).collect();
            let existing_by_offset: std::collections::HashMap<u64, &dfs_common::ChunkLocation> =
                existing.chunk_locations.iter()
                    .filter_map(|loc| loc.file_offset.map(|o| (o, loc)))
                    .collect();

            let mut cloned = metadata.clone();
            for loc in Arc::make_mut(&mut cloned.chunk_locations).iter_mut() {
                if let Some(existing_loc) = existing_by_id.get(&loc.chunk_id) {
                    // Rule 1: same chunk_id — merge node lists.
                    for node in &existing_loc.nodes {
                        if !loc.nodes.contains(node) {
                            loc.nodes.push(*node);
                        }
                    }
                } else if let Some(file_offset) = loc.file_offset {
                    // Rule 2: different chunk_id at same offset — keep the newer one.
                    if let Some(existing_loc) = existing_by_offset.get(&file_offset) {
                        // If the incoming file-level write_seq is strictly higher, the incoming
                        // chunk is definitively from a later write session and always wins,
                        // regardless of per-chunk client_write_seq. This covers the case where
                        // the incoming chunk has client_write_seq=None (e.g. fresh overwrite after
                        // a patch that did carry a client_write_seq).
                        let incoming_file_seq_wins = metadata.write_seq > 0
                            && existing.write_seq > 0
                            && metadata.write_seq > existing.write_seq;
                        let keep_existing = if incoming_file_seq_wins {
                            // File-level seq wins for fresh overwrites, but a chunk with a
                            // higher existing client_write_seq is still authoritative —
                            // prevents a stale broadcast (lower cws) from winning just
                            // because the accompanying file write_seq happened to be higher.
                            matches!(
                                (loc.client_write_seq, existing_loc.client_write_seq),
                                (Some(inc), Some(ext)) if ext > inc
                            )
                        } else {
                            match (loc.client_write_seq, existing_loc.client_write_seq) {
                                (Some(inc), Some(ext)) => ext > inc,
                                (Some(_), None)        => false,
                                (None, Some(_))        => true,
                                (None, None)           => {
                                    existing_loc.written_at.unwrap_or(0) > loc.written_at.unwrap_or(0)
                                }
                            }
                        };
                        if keep_existing {
                            *loc = (*existing_loc).clone();
                        }
                    }
                }
            }
            merged_metadata = Some(cloned);
            merged_metadata.as_ref().unwrap()
        } else {
            metadata
        };

        let value = bincode::serialize(metadata_to_store)
            .context("Failed to serialize file metadata")?;

        file_table.insert(file_id_str.as_str(), value.as_slice())
            .context("Failed to insert file metadata")?;
        path_table.insert(path_str, value.as_slice())
            .context("Failed to insert path index")?;

        drop(file_table);
        drop(path_table);
        txn.commit()?;

        debug!("Stored metadata for file: {} ({})", metadata_to_store.path, metadata_to_store.id);
        Ok(PutFileResult::Stored)
    }

    /// Get file metadata by ID.
    /// Cheap existence check — avoids deserializing the full FileMetadata.
    pub fn file_exists_by_id(&self, file_id: FileId) -> Result<bool> {
        let key = format!("{}", file_id);
        let _db = self.db.read().unwrap();
        let txn = _db.begin_read()?;
        let table = txn.open_table(FILE_TABLE)?;
        Ok(table.get(key.as_str())?.is_some())
    }

    pub fn get_file(&self, file_id: &FileId) -> Result<Option<FileMetadata>> {
        let key = format!("{}", file_id);
        let _db = self.db.read().unwrap();
        let txn = _db.begin_read()?;
        let table = txn.open_table(FILE_TABLE)?;
        match table.get(key.as_str())? {
            Some(v) => Ok(Some(bincode::deserialize::<FileMetadata>(v.value())
                .with_context(|| format!("Failed to deserialize metadata for {}", file_id))?)),
            None => Ok(None),
        }
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
        let _db = self.db.read().unwrap();
        let txn = _db.begin_read()?;
        let table = txn.open_table(PATH_TABLE)?;
        match table.get(path)? {
            Some(v) => Ok(Some(bincode::deserialize::<FileMetadata>(v.value())
                .with_context(|| format!("Failed to deserialize metadata for path {}", path))?)),
            None => Ok(None),
        }
    }

    /// Delete file metadata (removes both file and path index entries).
    pub fn delete_file(&self, file_id: &FileId) -> Result<()> {
        let file_id_str = format!("{}", file_id);
        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut file_table = txn.open_table(FILE_TABLE)?;
            let mut path_table = txn.open_table(PATH_TABLE)?;

            // Get path from file record so we can remove the path index entry.
            if let Some(v) = file_table.get(file_id_str.as_str())? {
                if let Ok(m) = bincode::deserialize::<FileMetadata>(v.value()) {
                    path_table.remove(m.path.as_str())?;
                }
            }
            file_table.remove(file_id_str.as_str())?;
        }
        txn.commit()?;
        debug!("Deleted metadata for file: {}", file_id);
        Ok(())
    }

    /// Delete only the path index entry for a specific path (used during rename).
    pub fn delete_path_index(&self, path: &str) -> Result<()> {
        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(PATH_TABLE)?;
            table.remove(path)?;
        }
        txn.commit()?;
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
        let _db = self.db.read().unwrap();
        let txn = _db.begin_read()?;
        let table = txn.open_table(FILE_TABLE)?;
        for item in table.range::<&str>(..)? {
            let (k, v) = item?;
            match bincode::deserialize::<FileMetadata>(v.value()) {
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
            let _db = self.db.read().unwrap();
            let txn = _db.begin_read()?;
            let table = txn.open_table(FILE_TABLE)?;
            let mut ids = Vec::new();
            let mut paths = Vec::new();
            for item in table.range::<&str>(..)? {
                let (k, v) = item?;
                let m = match bincode::deserialize::<FileMetadata>(v.value()) {
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
            let _db = self.db.read().unwrap();
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
        }

        // Also sweep path entries independently — a path entry can exist without
        // a file entry if the file entry was removed out-of-order.
        let stale_path_keys: Vec<String> = {
            let _db = self.db.read().unwrap();
            let txn = _db.begin_read()?;
            let table = txn.open_table(PATH_TABLE)?;
            let mut stale = Vec::new();
            for item in table.range::<&str>(..)? {
                let (k, v) = item?;
                if let Ok(m) = bincode::deserialize::<FileMetadata>(v.value()) {
                    if !live_ids.contains(&m.id) {
                        stale.push(k.value().to_string());
                    }
                }
            }
            stale
        };

        if !stale_path_keys.is_empty() {
            let _db = self.db.read().unwrap();
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

        let _db = self.db.read().unwrap();
        let txn = _db.begin_read()?;
        let table = txn.open_table(PATH_TABLE)?;
        let mut files = Vec::new();
        for item in table.range(dir_path.as_str()..end.as_str())? {
            let (k, v) = item?;
            let path = k.value();
            let relative = &path[dir_path.len()..];
            // Direct child: no slash, or a lone trailing slash (directory entry).
            if !relative.is_empty() && (!relative.contains('/') || relative.ends_with('/')) {
                match bincode::deserialize::<FileMetadata>(v.value()) {
                    Ok(m) => files.push(m),
                    Err(_) => warn!("list_directory: could not deserialize path index for {}", path),
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
        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(CHUNK_TABLE)?;
            table.insert(key.as_str(), value.as_slice())?;
        }
        txn.commit()?;
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
        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(CHUNK_TABLE)?;
            for location in locations {
                let key = format!("{}", location.chunk_id);
                let value = bincode::serialize(location)
                    .context("Failed to serialize chunk location")?;
                table.insert(key.as_str(), value.as_slice())?;
            }
        }
        txn.commit()?;
        debug!("Stored {} chunk locations in one batch transaction", locations.len());
        Ok(())
    }

    /// Get chunk location information.
    pub fn get_chunk_location(&self, chunk_id: &ChunkId) -> Result<Option<ChunkLocation>> {
        let key = format!("{}", chunk_id);
        let _db = self.db.read().unwrap();
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
        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(CHUNK_TABLE)?;
            table.remove(key.as_str())?;
        }
        txn.commit()?;
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
    pub async fn put_chunk_location_async(self: &Arc<Self>, location: ChunkLocation) -> Result<()> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.put_chunk_location(&location))
            .await
            .context("spawn_blocking panicked in put_chunk_location_async")?
    }

    /// Async wrapper for put_chunk_locations_batch — see put_chunk_location_async for
    /// why this must go through spawn_blocking rather than calling the sync method
    /// directly from request-handling code.
    pub async fn put_chunk_locations_batch_async(self: &Arc<Self>, locations: Vec<ChunkLocation>) -> Result<()> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.put_chunk_locations_batch(&locations))
            .await
            .context("spawn_blocking panicked in put_chunk_locations_batch_async")?
    }

    /// Async wrapper for delete_chunk_location — see put_chunk_location_async.
    pub async fn delete_chunk_location_async(self: &Arc<Self>, chunk_id: ChunkId) -> Result<()> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.delete_chunk_location(&chunk_id))
            .await
            .context("spawn_blocking panicked in delete_chunk_location_async")?
    }

    // -------------------------------------------------------------------------
    // Chunk refcounts (fast-path eviction for patch-generated chunk_ids)
    // -------------------------------------------------------------------------

    /// Mark `chunk_id` as live for one (file, chunk_idx) slot. Call exactly
    /// once per chunk_id, when it becomes the new value produced by a patch.
    pub fn incr_chunk_refcount(&self, chunk_id: &ChunkId) -> Result<u64> {
        let key = format!("{}", chunk_id);
        let _db = self.db.read().unwrap();
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
        let _db = self.db.read().unwrap();
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
        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Chunk patch journal (write-ahead undo for in-place patching)
    // -------------------------------------------------------------------------

    /// Durably record the undo info for an in-flight in-place patch. Must be
    /// committed before the patcher writes a single byte to the existing
    /// chunk file.
    pub fn put_patch_journal(&self, entry: &PatchJournalEntry) -> Result<()> {
        let key = format!("{}", entry.old_chunk_id);
        let value = bincode::serialize(entry)
            .context("Failed to serialize patch journal entry")?;
        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(CHUNK_PATCH_JOURNAL_TABLE)?;
            table.insert(key.as_str(), value.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Remove the journal entry for `old_chunk_id`. Call only after the
    /// rename and the new chunk_location have both committed.
    pub fn delete_patch_journal(&self, old_chunk_id: &ChunkId) -> Result<()> {
        let key = format!("{}", old_chunk_id);
        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(CHUNK_PATCH_JOURNAL_TABLE)?;
            table.remove(key.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Async wrapper for put_patch_journal — patches are a normal-volume client
    /// write path; see put_chunk_location_async for why this matters.
    pub async fn put_patch_journal_async(self: &Arc<Self>, entry: PatchJournalEntry) -> Result<()> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.put_patch_journal(&entry))
            .await
            .context("spawn_blocking panicked in put_patch_journal_async")?
    }

    /// Async wrapper for delete_patch_journal — see put_chunk_location_async.
    pub async fn delete_patch_journal_async(self: &Arc<Self>, old_chunk_id: ChunkId) -> Result<()> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.delete_patch_journal(&old_chunk_id))
            .await
            .context("spawn_blocking panicked in delete_patch_journal_async")?
    }

    /// Read every leftover journal entry. Used once at startup to recover
    /// from a crash that interrupted an in-place patch.
    pub fn scan_patch_journal(&self) -> Result<Vec<PatchJournalEntry>> {
        let _db = self.db.read().unwrap();
        let txn = _db.begin_read()?;
        let table = txn.open_table(CHUNK_PATCH_JOURNAL_TABLE)?;
        let mut out = Vec::new();
        for item in table.range::<&str>(..)? {
            let (_, v) = item?;
            out.push(bincode::deserialize::<PatchJournalEntry>(v.value())
                .context("Failed to deserialize patch journal entry")?);
        }
        Ok(out)
    }

    // -------------------------------------------------------------------------
    // Pending healing (per-chunk debounce timer, survives process restart)
    // -------------------------------------------------------------------------

    /// Record that `chunk_id` was first observed as needing healing at
    /// `detected_at_secs` (unix seconds). Idempotent — callers should only
    /// write this once per detection (on first insert into the in-memory map).
    pub fn put_pending_healing(&self, chunk_id: &ChunkId, detected_at_secs: u64) -> Result<()> {
        let key = format!("{}", chunk_id);
        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(PENDING_HEALING_TABLE)?;
            table.insert(key.as_str(), detected_at_secs)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Clear the persisted debounce timer for `chunk_id` (chunk reached RF, or
    /// was purged).
    pub fn delete_pending_healing(&self, chunk_id: &ChunkId) -> Result<()> {
        let key = format!("{}", chunk_id);
        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(PENDING_HEALING_TABLE)?;
            table.remove(key.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Async wrapper for put_pending_healing — fires once per chunk at the start
    /// of a heal storm, exactly the burst scenario that starved gluster1; see
    /// put_chunk_location_async.
    pub async fn put_pending_healing_async(self: &Arc<Self>, chunk_id: ChunkId, detected_at_secs: u64) -> Result<()> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.put_pending_healing(&chunk_id, detected_at_secs))
            .await
            .context("spawn_blocking panicked in put_pending_healing_async")?
    }

    /// Async wrapper for delete_pending_healing — see put_chunk_location_async.
    pub async fn delete_pending_healing_async(self: &Arc<Self>, chunk_id: ChunkId) -> Result<()> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.delete_pending_healing(&chunk_id))
            .await
            .context("spawn_blocking panicked in delete_pending_healing_async")?
    }

    /// Read all persisted (chunk_id, first_detected_at_secs) entries. Used at
    /// HealingManager startup to seed the in-memory pending_healing map so the
    /// healing_delay_secs debounce reflects time elapsed before this process
    /// started, not just since this process started.
    pub fn get_pending_healing_inventory(&self) -> Result<Vec<(ChunkId, u64)>> {
        let _db = self.db.read().unwrap();
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

    /// Batch apply chunk location updates — all puts and deletes in one write transaction.
    /// Use this from async code via `spawn_blocking` to avoid blocking Tokio worker threads
    /// on redb's exclusive write lock.
    pub fn batch_update_chunk_locations(
        &self,
        puts: &[ChunkLocation],
        deletes: &[ChunkId],
    ) -> Result<()> {
        if puts.is_empty() && deletes.is_empty() {
            return Ok(());
        }
        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(CHUNK_TABLE)?;
            for location in puts {
                let key = format!("{}", location.chunk_id);
                let value = bincode::serialize(location)
                    .context("Failed to serialize chunk location")?;
                table.insert(key.as_str(), value.as_slice())?;
            }
            for chunk_id in deletes {
                let key = format!("{}", chunk_id);
                table.remove(key.as_str())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// List all chunk IDs known in metadata.
    pub fn list_all_chunk_ids(&self) -> Result<Vec<ChunkId>> {
        let _db = self.db.read().unwrap();
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
        let _db = self.db.read().unwrap();
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
        let _db = self.db.read().unwrap();
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
    pub fn live_chunk_ids(&self) -> Result<std::collections::HashSet<ChunkId>> {
        let _db = self.db.read().unwrap();
        let txn = _db.begin_read()?;
        let table = txn.open_table(FILE_TABLE)?;
        let mut live = std::collections::HashSet::new();
        for item in table.range::<&str>(..)? {
            let (_, v) = item?;
            if let Ok(m) = bincode::deserialize::<FileMetadata>(v.value()) {
                for loc in m.chunk_locations.iter() {
                    live.insert(loc.chunk_id);
                }
            }
        }
        Ok(live)
    }

    /// Rebuild missing chunk: routing table entries from file metadata.
    pub fn rebuild_chunk_locations_from_files(&self) -> Result<(usize, usize)> {
        // Collect missing chunk records (read phase).
        let to_write: Vec<(String, Vec<u8>)> = {
            let _db = self.db.read().unwrap();
            let txn = _db.begin_read()?;
            let file_table = txn.open_table(FILE_TABLE)?;
            let chunk_table = txn.open_table(CHUNK_TABLE)?;
            let mut missing = Vec::new();
            for item in file_table.range::<&str>(..)? {
                let (_, v) = item?;
                let m = match bincode::deserialize::<FileMetadata>(v.value()) {
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
            let _db = self.db.read().unwrap();
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
        }

        // Count already-present entries (skipped).
        let skipped = {
            let _db = self.db.read().unwrap();
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
        let _db = self.db.read().unwrap();
        let txn = _db.begin_write()?;
        let next = {
            let mut table = txn.open_table(COUNTERS_TABLE)?;
            let current = table.get("meta_seq")?.map(|v| v.value()).unwrap_or(0);
            let next = current + 1;
            table.insert("meta_seq", next)?;
            next
        };
        txn.commit()?;
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
        let _db = self.db.read().unwrap();
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
        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        {
            let mut table = txn.open_table(COUNTERS_TABLE)?;
            table.insert("leader_state_hi", hi)?;
            table.insert("leader_state_lo", lo)?;
            table.insert("leader_state_since_secs", since_secs)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Read back the last-persisted leader NodeId and its leadership-episode
    /// start time (unix seconds), if any has ever been recorded.
    pub fn get_leader_state(&self) -> Result<(Option<NodeId>, Option<u64>)> {
        let _db = self.db.read().unwrap();
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

        let _db = self.db.read().unwrap();
        let txn = _db.begin_write()?;
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
            queue_table.insert(queue_key.as_str(), value.as_slice())?;
            idx_table.insert(idx_key.as_str(), sequence.to_be_bytes().as_slice())?;
        }
        txn.commit()?;
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

        let _db = self.db.read().unwrap();
        let txn = _db.begin_read()?;
        let table = txn.open_table(META_QUEUE_TABLE)?;
        let mut items = Vec::new();
        for item in table.range(prefix.as_str()..prefix_end.as_str())? {
            let (k, v) = item?;
            let key_str = k.value();
            let seq_hex = key_str.split(':').nth(1).unwrap_or("0");
            let seq = u64::from_str_radix(seq_hex, 16).unwrap_or(0);
            match bincode::deserialize::<FileMetadata>(v.value()) {
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
            let _db = self.db.read().unwrap();
            let txn = _db.begin_read()?;
            let table = txn.open_table(META_QUEUE_TABLE)?;
            let mut items = Vec::new();
            for item in table.range(prefix.as_str()..prefix_end.as_str())? {
                let (k, v) = item?;
                let key_str = k.value();
                if key_str > up_to_key.as_str() {
                    break;
                }
                let idx_key = bincode::deserialize::<FileMetadata>(v.value())
                    .ok()
                    .map(|m| format!("{}:{}", node_hex, m.id.0.as_simple()));
                items.push((key_str.to_string(), idx_key));
            }
            items
        };

        if to_remove.is_empty() {
            return Ok(());
        }

        let _db = self.db.read().unwrap();
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
        Ok(())
    }

    /// Deduplication pass: retain only the last entry per FileId for `node_id`.
    pub fn compact_meta_queue_for_node(&self, node_id: NodeId) -> Result<()> {
        let node_hex = Self::node_id_hex(node_id);
        let prefix = format!("{}:", node_hex);
        let prefix_end = prefix_next(&prefix);

        // Read all entries first.
        let entries: Vec<(String, u64, FileId)> = {
            let _db = self.db.read().unwrap();
            let txn = _db.begin_read()?;
            let table = txn.open_table(META_QUEUE_TABLE)?;
            let mut result = Vec::new();
            for item in table.range(prefix.as_str()..prefix_end.as_str())? {
                let (k, v) = item?;
                let key_str = k.value();
                let seq_hex = key_str.split(':').nth(1).unwrap_or("0");
                let seq = u64::from_str_radix(seq_hex, 16).unwrap_or(0);
                if let Ok(m) = bincode::deserialize::<FileMetadata>(v.value()) {
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
            let _db = self.db.read().unwrap();
            let mut txn = _db.begin_write()?;
            txn.set_durability(self.next_write_durability());
            {
                let mut table = txn.open_table(META_QUEUE_TABLE)?;
                for key in &to_remove {
                    table.remove(key.as_str())?;
                }
            }
            txn.commit()?;
        }
        Ok(())
    }

    /// Record the last sequence number received from the leader (follower-only).
    pub fn set_follower_sequence(&self, seq: u64) -> Result<()> {
        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(COUNTERS_TABLE)?;
            table.insert("follower_seq", seq)?;
        }
        txn.commit()?;
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
        let _db = self.db.read().unwrap();
        let txn = _db.begin_read()?;
        let table = txn.open_table(COUNTERS_TABLE)?;
        Ok(table.get("follower_seq")?.map(|v| v.value()).unwrap_or(0))
    }

    /// Return a compact inventory of all known files: Vec<(FileId, write_seq)>.
    /// write_seq (not modified_at) so catchup/healing comparisons are clock-agnostic —
    /// modified_at is user-settable (setattr/utimes) and not safe for ordering.
    pub fn get_file_inventory(&self) -> Result<Vec<(FileId, u64)>> {
        let _db = self.db.read().unwrap();
        let txn = _db.begin_read()?;
        let table = txn.open_table(FILE_TABLE)?;
        let mut out = Vec::new();
        for item in table.range::<&str>(..)? {
            let (_, v) = item?;
            if let Ok(m) = bincode::deserialize::<FileMetadata>(v.value()) {
                out.push((m.id, m.write_seq));
            }
        }
        Ok(out)
    }

    /// Fetch a batch of file records by ID. Missing IDs are silently skipped.
    pub fn get_files_batch(&self, ids: &[FileId]) -> Result<Vec<FileMetadata>> {
        let _db = self.db.read().unwrap();
        let txn = _db.begin_read()?;
        let table = txn.open_table(FILE_TABLE)?;
        let mut out = Vec::new();
        for id in ids {
            let key = format!("{}", id);
            if let Some(v) = table.get(key.as_str())? {
                if let Ok(m) = bincode::deserialize::<FileMetadata>(v.value()) {
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
        let _db = self.db.read().unwrap();
        let txn = _db.begin_read()?;
        let table = txn.open_table(FILE_TABLE)?;
        let mut count = 0usize;
        for item in table.range::<&str>(..)? {
            let (_, v) = item?;
            if let Ok(m) = bincode::deserialize::<FileMetadata>(v.value()) {
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
        let _db = self.db.read().unwrap();
        let txn = _db.begin_write()?;
        {
            let mut table = txn.open_table(DELETE_QUEUE_TABLE)?;
            table.insert(key.as_str(), value.as_slice())?;
        }
        txn.commit()?;
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
        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        txn.set_durability(self.next_write_durability());
        {
            let mut table = txn.open_table(DELETE_QUEUE_TABLE)?;
            table.remove(key.as_str())?;
        }
        txn.commit()?;
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
        let _db = self.db.read().unwrap();
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
    const BYTES_TABLES: [TableDefinition<'static, &'static str, &'static [u8]>; 7] = [
        FILE_TABLE, PATH_TABLE, CHUNK_TABLE, META_QUEUE_TABLE, META_QUEUE_IDX,
        DELETE_QUEUE_TABLE, CHUNK_PATCH_JOURNAL_TABLE,
    ];

    /// u64-valued tables copied/diffed as a unit by compact_db()'s shadow-copy pass.
    const U64_TABLES: [TableDefinition<'static, &'static str, u64>; 3] =
        [COUNTERS_TABLE, PENDING_HEALING_TABLE, CHUNK_REFCOUNT_TABLE];

    /// Copy every row of `def` from `src` into `dst`, overwriting whatever's there.
    /// Used for compact_db()'s initial full snapshot copy.
    fn copy_bytes_table(
        src: &redb::ReadTransaction,
        dst: &redb::WriteTransaction,
        def: TableDefinition<&str, &[u8]>,
    ) -> Result<()> {
        // Some tables (e.g. chunk_refcount) aren't pre-created at startup and only come
        // into existence on their first real write — a fresh/lightly-used store may
        // never have touched one. redb's read-side open_table errors on a table that
        // was never created (unlike the write side, which auto-creates); treat that as
        // "nothing to copy" rather than a real failure.
        let src_table = match src.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let mut dst_table = dst.open_table(def)?;
        for item in src_table.range::<&str>(..)? {
            let (k, v) = item?;
            dst_table.insert(k.value(), v.value())?;
        }
        Ok(())
    }

    fn copy_u64_table(
        src: &redb::ReadTransaction,
        dst: &redb::WriteTransaction,
        def: TableDefinition<&str, u64>,
    ) -> Result<()> {
        let src_table = match src.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let mut dst_table = dst.open_table(def)?;
        for item in src_table.range::<&str>(..)? {
            let (k, v) = item?;
            dst_table.insert(k.value(), v.value())?;
        }
        Ok(())
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

    fn copy_all_tables(src: &redb::ReadTransaction, dst: &redb::WriteTransaction) -> Result<()> {
        for def in Self::BYTES_TABLES { Self::copy_bytes_table(src, dst, def)?; }
        for def in Self::U64_TABLES { Self::copy_u64_table(src, dst, def)?; }
        Ok(())
    }

    fn diff_all_tables(src: &redb::ReadTransaction, dst: &redb::WriteTransaction) -> Result<usize> {
        let mut changed = 0usize;
        for def in Self::BYTES_TABLES { changed += Self::diff_bytes_table(src, dst, def)?; }
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
        self.compact_db_with_budget(std::time::Duration::from_secs(5), 64)
    }

    /// Same as `compact_db()`, parameterized by the Phase 2 catch-up time budget and
    /// convergence threshold (row-changes-per-pass below which we consider it settled).
    /// Split out so tests can exercise the non-convergence/defer path deterministically
    /// with a tiny budget/threshold, instead of needing a huge dataset and a multi-
    /// second wait to reliably outrun the production defaults.
    fn compact_db_with_budget(&self, catchup_budget: std::time::Duration, convergence_threshold: usize) -> Result<(u64, u64)> {
        let size_before = std::fs::metadata(&self.db_path).map(|m| m.len()).unwrap_or(0);
        info!("redb compaction starting ({:.1}MB live)", size_before as f64 / 1_048_576.0);

        let shadow_path = self.db_path.with_extension("redb.shadow");
        let _ = std::fs::remove_file(&shadow_path);
        let shadow_db = Database::builder()
            .set_cache_size(256 * 1024 * 1024)
            .create(&shadow_path)
            .map_err(|e| anyhow::anyhow!("compact: failed to create shadow db: {}", e))?;

        // Phase 1: full copy, holding only the shared lock.
        {
            let live = self.db.read().unwrap();
            let src_txn = live.begin_read().map_err(|e| anyhow::anyhow!("compact phase1 begin_read: {}", e))?;
            let dst_txn = shadow_db.begin_write().map_err(|e| anyhow::anyhow!("compact phase1 begin_write: {}", e))?;
            Self::copy_all_tables(&src_txn, &dst_txn)?;
            dst_txn.commit().map_err(|e| anyhow::anyhow!("compact phase1 commit: {}", e))?;
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
            let live = self.db.read().unwrap();
            let src_txn = live.begin_read().map_err(|e| anyhow::anyhow!("compact phase2 begin_read: {}", e))?;
            let dst_txn = shadow_db.begin_write().map_err(|e| anyhow::anyhow!("compact phase2 begin_write: {}", e))?;
            let changed = Self::diff_all_tables(&src_txn, &dst_txn)?;
            dst_txn.commit().map_err(|e| anyhow::anyhow!("compact phase2 commit: {}", e))?;
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

        // Phase 3: final catch-up + atomic swap, exclusively locked — the only part
        // that blocks other metadata operations on this node. Logged separately from
        // "compaction starting" so the actually-locked duration can be measured on its
        // own — Phases 1-2 above can legitimately take a while in wall-clock terms
        // without that being a problem, since they never hold this lock.
        info!("redb compaction phase3 lock acquiring");
        {
            let mut live = self.db.write().unwrap();
            {
                let src_txn = live.begin_read().map_err(|e| anyhow::anyhow!("compact phase3 begin_read: {}", e))?;
                let dst_txn = shadow_db.begin_write().map_err(|e| anyhow::anyhow!("compact phase3 begin_write: {}", e))?;
                Self::diff_all_tables(&src_txn, &dst_txn)?;
                dst_txn.commit().map_err(|e| anyhow::anyhow!("compact phase3 commit: {}", e))?;
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
        let mut db = self.db.write().unwrap();

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
    pub fn flush_durable(&self) -> Result<()> {
        let mut db = self.db.write().unwrap();
        let txn = db.begin_write()
            .map_err(|e| anyhow::anyhow!("flush_durable begin: {}", e))?;
        txn.commit()
            .map_err(|e| anyhow::anyhow!("flush_durable commit: {}", e))?;
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

        let result = store.compact_db_with_budget(std::time::Duration::from_millis(5), 1);
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
}
