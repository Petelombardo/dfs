use anyhow::{Context, Result};
use dfs_common::{ChunkId, ChunkLocation, FileId, FileMetadata, NodeId};
use redb::{Database, Durability, ReadableTable, TableDefinition};
// On Linux, Durability::Eventual calls fdatasync (same as Immediate). Only the macOS
// backend (F_BARRIERFSYNC) distinguishes them. Durability::None writes to the OS page
// cache without fdatasync — fast, immediately visible to reads, survives process crashes,
// only lost on kernel panic/power failure. Acceptable with 5-way replication.
use std::path::PathBuf;
use std::sync::RwLock;
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

// ---------------------------------------------------------------------------

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

/// Metadata storage using redb embedded database.
/// Replaces sled to eliminate the u8 fragment-count panic under heavy write loads.
pub struct MetadataStore {
    db: RwLock<Database>,
    db_path: PathBuf,
}

impl MetadataStore {
    /// Create a new metadata store (creates the redb file if it does not exist).
    pub fn new(metadata_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&metadata_dir)
            .with_context(|| format!("Failed to create metadata dir {:?}", metadata_dir))?;

        let db_path = metadata_dir.join("metadata.redb");

        // Cap redb's page cache to prevent OOM on low-RAM nodes (default is 1GB).
        // 256MB is plenty for our working set; the rest stays on disk.
        let mut db = Database::builder()
            .set_cache_size(256 * 1024 * 1024)
            .create(&db_path)
            .with_context(|| format!("Failed to open redb at {:?}", db_path))?;

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
            txn.commit()?;
        }

        info!("Initialized redb metadata store at {:?}", db_path);

        Ok(Self { db: RwLock::new(db), db_path })
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
            txn.set_durability(Durability::None);
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
            txn.set_durability(Durability::None);
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
        txn.set_durability(Durability::None);

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
            for loc in &mut cloned.chunk_locations {
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
        txn.set_durability(Durability::None);
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
        txn.set_durability(Durability::None);
        {
            let mut table = txn.open_table(PATH_TABLE)?;
            table.remove(path)?;
        }
        txn.commit()?;
        debug!("Deleted path index for: {}", path);
        Ok(())
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
            txn.set_durability(Durability::None);
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
            txn.set_durability(Durability::None);
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
        txn.set_durability(Durability::None);
        {
            let mut table = txn.open_table(CHUNK_TABLE)?;
            table.insert(key.as_str(), value.as_slice())?;
        }
        txn.commit()?;
        debug!("Stored location for chunk: {}", location.chunk_id);
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

    /// Delete chunk location.
    pub fn delete_chunk_location(&self, chunk_id: &ChunkId) -> Result<()> {
        let key = format!("{}", chunk_id);
        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        txn.set_durability(Durability::None);
        {
            let mut table = txn.open_table(CHUNK_TABLE)?;
            table.remove(key.as_str())?;
        }
        txn.commit()?;
        Ok(())
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
        txn.set_durability(Durability::None);
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
                for loc in &m.chunk_locations {
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
                for loc in &m.chunk_locations {
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
            txn.set_durability(Durability::None);
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

    /// Read current metadata sequence number.
    pub fn current_meta_sequence(&self) -> Result<u64> {
        let _db = self.db.read().unwrap();
        let txn = _db.begin_read()?;
        let table = txn.open_table(COUNTERS_TABLE)?;
        Ok(table.get("meta_seq")?.map(|v| v.value()).unwrap_or(0))
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
        txn.set_durability(Durability::None);
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
            txn.set_durability(Durability::None);
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
        txn.set_durability(Durability::None);
        {
            let mut table = txn.open_table(COUNTERS_TABLE)?;
            table.insert("follower_seq", seq)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Get the last sequence number received from the leader (follower-only).
    pub fn get_follower_sequence(&self) -> Result<u64> {
        let _db = self.db.read().unwrap();
        let txn = _db.begin_read()?;
        let table = txn.open_table(COUNTERS_TABLE)?;
        Ok(table.get("follower_seq")?.map(|v| v.value()).unwrap_or(0))
    }

    /// Return a compact inventory of all known files: Vec<(FileId, modified_at)>.
    pub fn get_file_inventory(&self) -> Result<Vec<(FileId, u64)>> {
        let _db = self.db.read().unwrap();
        let txn = _db.begin_read()?;
        let table = txn.open_table(FILE_TABLE)?;
        let mut out = Vec::new();
        for item in table.range::<&str>(..)? {
            let (_, v) = item?;
            if let Ok(m) = bincode::deserialize::<FileMetadata>(v.value()) {
                out.push((m.id, m.modified_at));
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

    /// Remove a completed deletion from the queue (called after all nodes ack).
    pub fn dequeue_delete(&self, file_id: &FileId) -> Result<()> {
        let key = format!("del:{}", file_id);
        let _db = self.db.read().unwrap();
        let mut txn = _db.begin_write()?;
        txn.set_durability(Durability::None);
        {
            let mut table = txn.open_table(DELETE_QUEUE_TABLE)?;
            table.remove(key.as_str())?;
        }
        txn.commit()?;
        Ok(())
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

    /// Compact the database, reclaiming dead pages from MultiPatch chunk-ID rotations.
    /// Returns (before_bytes, after_bytes). Runs in the caller's thread — use spawn_blocking
    /// from async code. Takes the Mutex exclusively, blocking all metadata I/O for the
    /// duration; compact time scales with live data size (not file size), so after the first
    /// run it is typically fast (seconds, not minutes).
    pub fn compact_db(&self) -> Result<(u64, u64)> {
        let size_before = std::fs::metadata(&self.db_path).map(|m| m.len()).unwrap_or(0);
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
        Ok((size_before, size_after))
    }

    /// No-op kept for call-site compatibility; compaction is handled by compact_db().
    pub fn flush(&self) -> Result<()> {
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
        };

        store.put_chunk_location(&location).unwrap();

        let retrieved = store.get_chunk_location(&chunk_id).unwrap().unwrap();
        assert_eq!(retrieved.nodes.len(), 2);
        assert_eq!(retrieved.size, 4096);
    }
}
