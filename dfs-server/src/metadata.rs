use anyhow::{Context, Result};
use dfs_common::{ChunkId, ChunkLocation, ChunkLocationV0, ChunkLocationV1, FileId, FileMetadata, FileMetadataV0, FileMetadataV1, NodeId};
use sled::Db;
use std::path::PathBuf;
use tracing::{debug, info, warn};

/// Metadata storage using Sled embedded database
/// Optimized for SBC environments (memory-efficient, crash-safe)
pub struct MetadataStore {
    /// Sled database instance
    db: Db,

    /// Durable per-follower dissemination queue tree (leader-only).
    /// Key: `{node_id_hex}:{seq:016x}` — Value: bincode-serialized FileMetadata.
    /// The leader writes here before acking the client; a background loop drains
    /// entries to each follower and removes them on confirmed ack.
    pub meta_queue: sled::Tree,

    /// Monotonic sequence counter for this node's metadata writes (leader-only).
    /// Stored in sled so it survives restarts. Key: b"meta_seq".
    pub meta_seq_tree: sled::Tree,

    /// Last sequence number received from the leader (follower-only).
    /// Key: b"follower_seq". Used by new leaders for catch-up calculation.
    pub follower_seq_tree: sled::Tree,
}

impl MetadataStore {
    /// Create a new metadata store
    pub fn new(metadata_dir: PathBuf) -> Result<Self> {
        // Configure Sled with memory limits to prevent unbounded cache growth
        // Default cache can grow to multiple GB even for small databases
        let cache_capacity_mb = std::env::var("DFS_METADATA_CACHE_MB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(16); // 16MB — keeps the hot working set (file: and path: keys) warm
                            // without mapping the full chunk: index into RAM.  The chunk:
                            // keyspace is large (100k+ records) but only scanned by the
                            // leader healer once per minute; 16MB is enough for file lookups
                            // and write-path hot keys.

        let cache_capacity_bytes = cache_capacity_mb * 1024 * 1024;

        let db = sled::Config::new()
            .path(&metadata_dir)
            .cache_capacity(cache_capacity_bytes)
            .flush_every_ms(Some(250))  // Flush every 250ms — reduces metadata write stalls for sequential workloads
            .open()
            .with_context(|| format!("Failed to open metadata database at {:?}", metadata_dir))?;

        let meta_queue = db.open_tree(b"meta_queue")
            .context("Failed to open meta_queue tree")?;
        let meta_seq_tree = db.open_tree(b"meta_seq")
            .context("Failed to open meta_seq tree")?;
        let follower_seq_tree = db.open_tree(b"follower_seq")
            .context("Failed to open follower_seq tree")?;

        info!("Initialized metadata store at {:?} (cache: {}MB)", metadata_dir, cache_capacity_mb);

        Ok(Self { db, meta_queue, meta_seq_tree, follower_seq_tree })
    }

    /// Scan all `file:` records and re-create any missing `path:` index entries.
    ///
    /// A crash between the two sled inserts in `put_file` (file: written, path: not yet)
    /// leaves the DB with a file record that is invisible to `list_directory`.  This runs
    /// at startup and is cheap: it only inserts when the path: key is absent.
    pub fn repair_path_index(&self) -> Result<()> {
        let mut repaired = 0usize;
        // Pass 1: ensure every file: record has a corresponding path: entry.
        for item in self.db.scan_prefix(b"file:") {
            let (_, value) = item?;
            let metadata = if let Ok(m) = bincode::deserialize::<FileMetadata>(&value) {
                m
            } else if let Ok(v1) = bincode::deserialize::<FileMetadataV1>(&value) {
                v1.into()
            } else if let Ok(v0) = bincode::deserialize::<FileMetadataV0>(&value) {
                v0.into()
            } else {
                continue;
            };

            let path_key = self.path_key(&metadata.path);
            if self.db.get(&path_key)?.is_none() {
                self.db.insert(path_key, value)?;
                warn!("Repaired missing path index for: {}", metadata.path);
                repaired += 1;
            }
        }

        // Pass 2: remove path: entries whose file: record no longer exists.
        // These accumulate when a delete removes the file: record but the path:
        // entry isn't cleaned up (crash, replication gap, etc.) — causing deleted
        // files to reappear in readdir on every remount.
        let mut stale_keys: Vec<sled::IVec> = Vec::new();
        for item in self.db.scan_prefix(b"path:") {
            let (key, value) = item?;
            let file_id = if let Ok(m) = bincode::deserialize::<FileMetadata>(&value) {
                m.id
            } else if let Ok(v1) = bincode::deserialize::<FileMetadataV1>(&value) {
                v1.id
            } else if let Ok(v0) = bincode::deserialize::<FileMetadataV0>(&value) {
                v0.id
            } else if let Ok(id) = bincode::deserialize::<FileId>(&value) {
                id
            } else {
                // Can't parse — leave it alone
                continue;
            };

            let file_key = self.file_key(&file_id);
            if self.db.get(&file_key)?.is_none() {
                stale_keys.push(key);
            }
        }

        let stale_count = stale_keys.len();
        for key in stale_keys {
            if let Ok(key_str) = std::str::from_utf8(&key) {
                warn!("Removing stale path index entry: {}", key_str);
            }
            self.db.remove(key)?;
        }

        if repaired > 0 || stale_count > 0 {
            info!("Path index repair: {} entries rebuilt, {} stale entries removed", repaired, stale_count);
        }
        Ok(())
    }

    /// Store file metadata
    pub fn put_file(&self, metadata: &FileMetadata) -> Result<()> {
        // If a different file ID already exists at this path, remove the old file: record
        // before writing the new one. Without this, every create() on an existing path
        // allocates a new FileId UUID and leaves the old file: entry orphaned in the DB —
        // causing unbounded metadata growth and a permanent healer backlog.
        let path_key = self.path_key(&metadata.path);
        if let Ok(Some(existing_bytes)) = self.db.get(&path_key) {
            // Try all FileMetadata formats, then legacy FileId-only format
            let existing_id = if let Ok(existing) = bincode::deserialize::<FileMetadata>(&existing_bytes) {
                Some(existing.id)
            } else if let Ok(existing) = bincode::deserialize::<FileMetadataV1>(&existing_bytes) {
                Some(existing.id)
            } else if let Ok(existing) = bincode::deserialize::<FileMetadataV0>(&existing_bytes) {
                Some(existing.id)
            } else if let Ok(id) = bincode::deserialize::<FileId>(&existing_bytes) {
                Some(id)
            } else {
                None
            };

            if let Some(old_id) = existing_id {
                if old_id != metadata.id {
                    // Different ID at same path — purge the old file: record
                    let old_key = self.file_key(&old_id);
                    if let Err(e) = self.db.remove(old_key) {
                        warn!("Failed to remove stale file record {} for path {}: {}", old_id, metadata.path, e);
                    } else {
                        debug!("Removed stale file record {} superseded by {} at path {}", old_id, metadata.id, metadata.path);
                    }
                }
            }
        }

        // Merge chunk_locations: if the same file already exists with more replica nodes
        // for any chunk, preserve those nodes. This prevents stale PutFileMetadata/
        // ReplicateMetadata broadcasts (which carry only the 2 nodes written by the client)
        // from overwriting healed 3-node state that was added via ReplicateChunkLocation.
        //
        // Also guards against stale-metadata resurrection: if the existing record is
        // strictly newer (higher modified_at) AND has chunks while the incoming one
        // does not, silently drop the incoming write. This prevents the metadata retry
        // queue from clobbering a fully-written file with its zero-size create entry.
        let merged_metadata;
        let metadata_to_store = {
            let file_key = self.file_key(&metadata.id);
            if let Ok(Some(existing_bytes)) = self.db.get(&file_key) {
                let existing_opt = if let Ok(m) = bincode::deserialize::<FileMetadata>(&existing_bytes) {
                    Some(m)
                } else if let Ok(v1) = bincode::deserialize::<FileMetadataV1>(&existing_bytes) {
                    Some(v1.into())
                } else if let Ok(v0) = bincode::deserialize::<FileMetadataV0>(&existing_bytes) {
                    Some(v0.into())
                } else {
                    None
                };

                if let Some(existing) = existing_opt {
                    // Drop the incoming write if it is stale: both records have a
                    // write_seq > 0 and the existing is strictly newer.
                    // write_seq is assigned by the client in strictly increasing order
                    // before enqueueing, so higher == newer. Sequence 0 means legacy
                    // record (no sequencing) — never drop those on sequence alone.
                    if existing.write_seq > 0
                        && metadata.write_seq > 0
                        && existing.write_seq > metadata.write_seq
                    {
                        debug!(
                            "Dropping stale metadata for {} (existing write_seq={} > incoming={})",
                            metadata.path, existing.write_seq, metadata.write_seq
                        );
                        return Ok(());
                    }

                    // Merge chunk nodes: preserve any replica nodes from the existing record
                    // that the incoming record doesn't know about yet.
                    let existing_nodes: std::collections::HashMap<ChunkId, Vec<NodeId>> =
                        existing.chunk_locations.iter()
                            .map(|loc| (loc.chunk_id, loc.nodes.clone()))
                            .collect();

                    let mut cloned = metadata.clone();
                    for loc in &mut cloned.chunk_locations {
                        if let Some(known_nodes) = existing_nodes.get(&loc.chunk_id) {
                            for node in known_nodes {
                                if !loc.nodes.contains(node) {
                                    loc.nodes.push(*node);
                                }
                            }
                        }
                    }
                    merged_metadata = cloned;
                    &merged_metadata
                } else {
                    metadata
                }
            } else {
                metadata
            }
        };

        let key = self.file_key(&metadata_to_store.id);
        let value = bincode::serialize(metadata_to_store)
            .context("Failed to serialize file metadata")?;

        self.db
            .insert(key, value.clone())
            .context("Failed to insert file metadata")?;

        // Also index by path for lookups — store full metadata so list_directory
        // is a single prefix scan with no per-entry secondary lookups.
        self.db
            .insert(path_key, value)
            .context("Failed to insert path index")?;

        debug!("Stored metadata for file: {} ({})", metadata_to_store.path, metadata_to_store.id);

        Ok(())
    }

    /// Get file metadata by ID
    ///
    /// BACKWARD COMPATIBILITY: This method handles both old (FileMetadataV0 with ChunkLocationV0)
    /// and new (FileMetadata with ChunkLocation) formats for seamless upgrades.
    pub fn get_file(&self, file_id: &FileId) -> Result<Option<FileMetadata>> {
        let key = self.file_key(file_id);

        match self.db.get(key)? {
            Some(value) => {
                // Try to deserialize as new format first
                match bincode::deserialize::<FileMetadata>(&value) {
                    Ok(mut metadata) => {
                        // MIGRATION FIXUP: Even for new format, populate chunk_locations if empty
                        // This handles files that were migrated before we added chunk_locations population
                        if metadata.chunk_locations.is_empty() && !metadata.chunks.is_empty() {
                            info!("Populating chunk_locations for file {} ({} chunks)",
                                  file_id, metadata.chunks.len());

                            for (idx, chunk_id) in metadata.chunks.iter().enumerate() {
                                if let Ok(Some(location)) = self.get_chunk_location(chunk_id) {
                                    metadata.chunk_locations.push(location);
                                } else {
                                    warn!("Failed to find chunk location for chunk {} (idx {}) during fixup",
                                          chunk_id, idx);
                                }
                            }

                            // Write back updated metadata
                            if let Err(e) = self.put_file(&metadata) {
                                warn!("Failed to update metadata for {}: {}", file_id, e);
                            }
                        }

                        Ok(Some(metadata))
                    }
                    Err(new_err) => {
                        // Try V1 format (ChunkLocationV1 — has file_offset but no written_at)
                        let v1_result = bincode::deserialize::<FileMetadataV1>(&value).ok().map(FileMetadata::from);
                        // Then try V0 format (ChunkLocationV0 — no file_offset or written_at)
                        let legacy_metadata = v1_result.or_else(|| {
                            bincode::deserialize::<FileMetadataV0>(&value).ok().map(FileMetadata::from)
                        });

                        match legacy_metadata {
                            Some(mut metadata) => {
                                // CRITICAL: Populate chunk_locations from legacy chunks array
                                if metadata.chunk_locations.is_empty() && !metadata.chunks.is_empty() {
                                    info!("Migrating {} legacy chunks to chunk_locations for file {}",
                                          metadata.chunks.len(), file_id);

                                    for (idx, chunk_id) in metadata.chunks.iter().enumerate() {
                                        if let Ok(Some(location)) = self.get_chunk_location(chunk_id) {
                                            metadata.chunk_locations.push(location);
                                        } else {
                                            warn!("Failed to find chunk location for chunk {} (idx {}) during migration",
                                                  chunk_id, idx);
                                        }
                                    }
                                }

                                // Auto-migrate by writing back in current format
                                if let Err(e) = self.put_file(&metadata) {
                                    warn!("Failed to auto-migrate metadata for {}: {}", file_id, e);
                                }

                                Ok(Some(metadata))
                            }
                            None => {
                                Err(anyhow::anyhow!(
                                    "Failed to deserialize file metadata (tried all formats). \
                                     New format error: {}",
                                    new_err
                                ))
                            }
                        }
                    }
                }
            }
            None => Ok(None),
        }
    }

    /// Get file metadata by path
    pub fn get_file_by_path(&self, path: &str) -> Result<Option<FileMetadata>> {
        let path_key = self.path_key(path);

        match self.db.get(&path_key)? {
            Some(bytes) => {
                // Try current FileMetadata format first
                if let Ok(metadata) = bincode::deserialize::<FileMetadata>(&bytes) {
                    return Ok(Some(metadata));
                }
                // Try V1 format (ChunkLocationV1 — file_offset but no written_at)
                if let Ok(v1) = bincode::deserialize::<FileMetadataV1>(&bytes) {
                    let metadata: FileMetadata = v1.into();
                    let _ = self.db.insert(&path_key, bincode::serialize(&metadata)?);
                    return Ok(Some(metadata));
                }
                // Try V0 format (ChunkLocationV0 — no file_offset or written_at)
                if let Ok(v0) = bincode::deserialize::<FileMetadataV0>(&bytes) {
                    let metadata: FileMetadata = v0.into();
                    let _ = self.db.insert(&path_key, bincode::serialize(&metadata)?);
                    return Ok(Some(metadata));
                }
                // Legacy format: FileId stored, requires second lookup
                let file_id: FileId = bincode::deserialize(&bytes)
                    .context("Failed to deserialize file ID")?;
                let result = self.get_file(&file_id)?;
                // Rewrite in new format so next access is fast
                if let Some(ref metadata) = result {
                    let _ = self.db.insert(path_key, bincode::serialize(metadata)?);
                }
                Ok(result)
            }
            None => Ok(None),
        }
    }

    /// Delete file metadata
    pub fn delete_file(&self, file_id: &FileId) -> Result<()> {
        // Get metadata first to remove path index
        if let Some(metadata) = self.get_file(file_id)? {
            let path_key = self.path_key(&metadata.path);
            self.db.remove(path_key)?;
        }

        let key = self.file_key(file_id);
        self.db
            .remove(key)
            .context("Failed to delete file metadata")?;

        debug!("Deleted metadata for file: {}", file_id);

        Ok(())
    }

    /// Delete only the path index entry for a specific path
    /// Used during rename to remove the old path without touching the file metadata
    pub fn delete_path_index(&self, path: &str) -> Result<()> {
        let path_key = self.path_key(path);
        self.db.remove(path_key)
            .context("Failed to delete path index")?;
        debug!("Deleted path index for: {}", path);
        Ok(())
    }

    /// List all files — returns a Vec, loading all records into memory.
    /// Only use for admin/diagnostic paths where the full list is needed at once.
    /// For startup scans use `scan_files` to avoid loading all metadata into RAM.
    pub fn list_files(&self) -> Result<Vec<FileMetadata>> {
        let mut files = Vec::new();
        self.scan_files(|m| { files.push(m); Ok(()) })?;
        Ok(files)
    }

    /// Stream all file metadata records, calling `f` for each one without
    /// materialising the full collection in memory.  Used at startup for the
    /// chunk-map build so that 535 MB of on-disk sled data doesn't become 2 GB
    /// of in-RAM Vec<FileMetadata>.
    pub fn scan_files<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(FileMetadata) -> Result<()>,
    {
        for item in self.db.scan_prefix(b"file:") {
            let (key, value) = item?;
            let metadata = if let Ok(m) = bincode::deserialize::<FileMetadata>(&value) {
                m
            } else if let Ok(v1) = bincode::deserialize::<FileMetadataV1>(&value) {
                v1.into()
            } else if let Ok(v0) = bincode::deserialize::<FileMetadataV0>(&value) {
                v0.into()
            } else {
                warn!("Skipping corrupt metadata entry (key={:?})", key);
                continue;
            };
            f(metadata)?;
        }
        Ok(())
    }

    /// Remove file: and path: records whose file ID is not in `live_ids`.
    ///
    /// Called on followers after the leader sends a ReconcileMetadata message.
    /// The leader's file: keyspace is authoritative — any ID the leader doesn't
    /// have is a stale entry from a missed delete. Chunk data is never touched.
    ///
    /// Returns the number of (file:, path:) record pairs removed.
    pub fn remove_unlisted_files(
        &self,
        live_ids: &std::collections::HashSet<FileId>,
    ) -> Result<usize> {
        let mut removed = 0usize;

        // Collect stale file: records.
        let mut stale_file_keys: Vec<sled::IVec> = Vec::new();
        let mut stale_paths: Vec<String> = Vec::new();

        for item in self.db.scan_prefix(b"file:") {
            let (key, value) = item?;
            let file_id = if let Ok(m) = bincode::deserialize::<FileMetadata>(&value) {
                // Also collect the path so we can remove the path: entry directly.
                stale_paths.push(m.path.clone());
                m.id
            } else if let Ok(v1) = bincode::deserialize::<FileMetadataV1>(&value) {
                stale_paths.push(v1.path.clone());
                v1.id
            } else if let Ok(v0) = bincode::deserialize::<FileMetadataV0>(&value) {
                stale_paths.push(v0.path.clone());
                v0.id
            } else {
                continue; // can't parse — leave it alone
            };

            if !live_ids.contains(&file_id) {
                stale_file_keys.push(key);
            } else {
                stale_paths.pop(); // not stale, remove the path we just pushed
            }
        }

        // Remove stale file: records and their path: index entries.
        for (key, path) in stale_file_keys.iter().zip(stale_paths.iter()) {
            if let Ok(key_str) = std::str::from_utf8(key) {
                warn!("ReconcileMetadata: removing stale file record: {} (path: {})", key_str, path);
            }
            self.db.remove(key)?;
            let path_key = self.path_key(path);
            self.db.remove(path_key)?;
            removed += 1;
        }

        // Also sweep path: entries independently — a path: record can exist without
        // a file: record if the file: record was removed out-of-order.
        let mut stale_path_keys: Vec<sled::IVec> = Vec::new();
        for item in self.db.scan_prefix(b"path:") {
            let (key, value) = item?;
            let file_id = if let Ok(m) = bincode::deserialize::<FileMetadata>(&value) {
                m.id
            } else if let Ok(v1) = bincode::deserialize::<FileMetadataV1>(&value) {
                v1.id
            } else if let Ok(v0) = bincode::deserialize::<FileMetadataV0>(&value) {
                v0.id
            } else if let Ok(id) = bincode::deserialize::<FileId>(&value) {
                id
            } else {
                continue;
            };
            if !live_ids.contains(&file_id) {
                stale_path_keys.push(key);
            }
        }
        for key in stale_path_keys {
            if let Ok(key_str) = std::str::from_utf8(&key) {
                warn!("ReconcileMetadata: removing stale path index entry: {}", key_str);
            }
            self.db.remove(key)?;
            removed += 1;
        }

        if removed > 0 {
            info!("ReconcileMetadata: removed {} stale metadata records", removed);
        }
        Ok(removed)
    }

    /// List files in a directory (optimized with path prefix scan)
    pub fn list_directory(&self, dir_path: &str) -> Result<Vec<FileMetadata>> {
        let mut files = Vec::new();

        // Normalize directory path
        let dir_path = if dir_path.ends_with('/') {
            dir_path.to_string()
        } else {
            format!("{}/", dir_path)
        };

        // Use path index prefix scan instead of full table scan
        // This scans only paths starting with "path:/dir/" instead of all files
        let prefix = format!("path:{}", dir_path);

        for item in self.db.scan_prefix(prefix.as_bytes()) {
            let (key, value) = item?;

            // Extract the path from the key
            let key_str = String::from_utf8_lossy(&key);
            if let Some(path) = key_str.strip_prefix("path:") {
                // Check if this is a direct child (not nested subdirectory)
                let relative = &path[dir_path.len()..];
                if !relative.is_empty() && (!relative.contains('/') || relative.ends_with('/')) {
                    // Try current FileMetadata format first (no secondary lookup)
                    if let Ok(metadata) = bincode::deserialize::<FileMetadata>(&value) {
                        files.push(metadata);
                    } else if let Ok(v1) = bincode::deserialize::<FileMetadataV1>(&value) {
                        // Path index entry written before written_at was added to ChunkLocation
                        let metadata: FileMetadata = v1.into();
                        let _ = self.db.insert(key, bincode::serialize(&metadata)?);
                        files.push(metadata);
                    } else if let Ok(v0) = bincode::deserialize::<FileMetadataV0>(&value) {
                        // Path index entry written before file_offset was added to ChunkLocation
                        let metadata: FileMetadata = v0.into();
                        let _ = self.db.insert(key, bincode::serialize(&metadata)?);
                        files.push(metadata);
                    } else if let Ok(file_id) = bincode::deserialize::<FileId>(&value) {
                        // Legacy entry — fetch metadata and rewrite index in new format
                        if let Some(metadata) = self.get_file(&file_id)? {
                            let _ = self.db.insert(key, bincode::serialize(&metadata)?);
                            files.push(metadata);
                        }
                    } else {
                        warn!("list_directory: could not deserialize path index entry for {}", path);
                    }
                }
            }
        }

        Ok(files)
    }

    /// Store chunk location information
    pub fn put_chunk_location(&self, location: &ChunkLocation) -> Result<()> {
        let key = self.chunk_key(&location.chunk_id);
        let value = bincode::serialize(location)
            .context("Failed to serialize chunk location")?;

        self.db
            .insert(key, value)
            .context("Failed to insert chunk location")?;

        debug!("Stored location for chunk: {}", location.chunk_id);

        Ok(())
    }

    // -------------------------------------------------------------------------
    // Leader metadata sequence & dissemination queue
    // -------------------------------------------------------------------------

    /// Increment and return the next metadata sequence number (leader-only).
    /// Persisted in sled so it survives leader restarts.
    pub fn next_meta_sequence(&self) -> Result<u64> {
        // Atomic fetch-and-increment via sled's compare-and-swap loop.
        loop {
            let current_bytes = self.meta_seq_tree.get(b"meta_seq")?;
            let current: u64 = current_bytes.as_ref()
                .and_then(|b| b.as_ref().try_into().ok())
                .map(u64::from_be_bytes)
                .unwrap_or(0);
            let next = current + 1;
            let next_bytes = next.to_be_bytes();
            match self.meta_seq_tree.compare_and_swap(
                b"meta_seq",
                current_bytes.as_deref(),
                Some(&next_bytes),
            )? {
                Ok(_) => return Ok(next),
                Err(_) => continue, // lost the race, retry
            }
        }
    }

    /// Read current metadata sequence (leader: last issued; follower: last received).
    pub fn current_meta_sequence(&self) -> Result<u64> {
        Ok(self.meta_seq_tree.get(b"meta_seq")?
            .as_ref()
            .and_then(|b| b.as_ref().try_into().ok())
            .map(u64::from_be_bytes)
            .unwrap_or(0))
    }

    /// Format node_id as a fixed-length hex string for use as a sled key prefix.
    fn node_id_hex(node_id: NodeId) -> String {
        node_id.as_bytes().iter().map(|b| format!("{:02x}", b)).collect()
    }

    /// Enqueue a metadata update destined for `node_id` at `sequence`.
    /// Key format: `{node_id_hex}:{seq:016x}` — lexicographic order == sequence order.
    pub fn enqueue_meta_for_node(&self, node_id: NodeId, sequence: u64, metadata: &FileMetadata) -> Result<()> {
        let key = format!("{}:{:016x}", Self::node_id_hex(node_id), sequence);
        let value = bincode::serialize(metadata).context("Failed to serialize metadata for queue")?;
        self.meta_queue.insert(key.as_bytes(), value)?;
        Ok(())
    }

    /// Return all queued metadata entries for `node_id`, in sequence order.
    /// Returns Vec<(sequence, FileMetadata)>.
    pub fn drain_meta_queue_for_node(&self, node_id: NodeId) -> Result<Vec<(u64, FileMetadata)>> {
        let prefix = format!("{}:", Self::node_id_hex(node_id));
        let mut items = Vec::new();
        for item in self.meta_queue.scan_prefix(prefix.as_bytes()) {
            let (key, value) = item?;
            let key_str = std::str::from_utf8(&key).unwrap_or("");
            let seq_hex = key_str.split(':').nth(1).unwrap_or("0");
            let seq = u64::from_str_radix(seq_hex, 16).unwrap_or(0);
            match bincode::deserialize::<FileMetadata>(&value) {
                Ok(m) => items.push((seq, m)),
                Err(e) => warn!("meta_queue: failed to deserialize entry {}: {}", key_str, e),
            }
        }
        Ok(items)
    }

    /// Remove all queue entries for `node_id` up to and including `up_to_sequence`.
    pub fn ack_meta_queue_for_node(&self, node_id: NodeId, up_to_sequence: u64) -> Result<()> {
        let prefix = format!("{}:", Self::node_id_hex(node_id));
        let up_to_key = format!("{}:{:016x}", Self::node_id_hex(node_id), up_to_sequence);
        let mut to_remove = Vec::new();
        for item in self.meta_queue.scan_prefix(prefix.as_bytes()) {
            let (key, _) = item?;
            if key.as_ref() <= up_to_key.as_bytes() {
                to_remove.push(key);
            }
        }
        for key in to_remove {
            self.meta_queue.remove(key)?;
        }
        Ok(())
    }

    /// Deduplication pass: for each node prefix, retain only the last entry per FileId.
    /// Called before draining to avoid sending redundant updates for files written many times.
    pub fn compact_meta_queue_for_node(&self, node_id: NodeId) -> Result<()> {
        let prefix = format!("{}:", Self::node_id_hex(node_id));
        // Build map: file_id -> (key, seq) — keep highest seq per file_id.
        let mut seen: std::collections::HashMap<FileId, (sled::IVec, u64)> = std::collections::HashMap::new();
        for item in self.meta_queue.scan_prefix(prefix.as_bytes()) {
            let (key, value) = item?;
            let key_str = std::str::from_utf8(&key).unwrap_or("");
            let seq_hex = key_str.split(':').nth(1).unwrap_or("0");
            let seq = u64::from_str_radix(seq_hex, 16).unwrap_or(0);
            if let Ok(m) = bincode::deserialize::<FileMetadata>(&value) {
                let entry = seen.entry(m.id).or_insert((key.clone(), seq));
                if seq > entry.1 {
                    self.meta_queue.remove(&entry.0)?;
                    *entry = (key, seq);
                } else if seq < entry.1 {
                    self.meta_queue.remove(&key)?;
                }
            }
        }
        Ok(())
    }

    /// Record the last sequence number received from the leader (follower-only).
    pub fn set_follower_sequence(&self, seq: u64) -> Result<()> {
        self.follower_seq_tree.insert(b"follower_seq", &seq.to_be_bytes())?;
        Ok(())
    }

    /// Get the last sequence number received from the leader (follower-only).
    pub fn get_follower_sequence(&self) -> Result<u64> {
        Ok(self.follower_seq_tree.get(b"follower_seq")?
            .as_ref()
            .and_then(|b| b.as_ref().try_into().ok())
            .map(u64::from_be_bytes)
            .unwrap_or(0))
    }

    /// Return a compact inventory of all known files: Vec<(FileId, modified_at)>.
    /// Used by a newly-elected leader to diff against follower inventories.
    pub fn get_file_inventory(&self) -> Result<Vec<(FileId, u64)>> {
        let mut out = Vec::new();
        for item in self.db.scan_prefix(b"file:") {
            let (_, value) = item?;
            let (id, modified_at) = if let Ok(m) = bincode::deserialize::<FileMetadata>(&value) {
                (m.id, m.modified_at)
            } else if let Ok(v1) = bincode::deserialize::<FileMetadataV1>(&value) {
                let m: FileMetadata = v1.into();
                (m.id, m.modified_at)
            } else if let Ok(v0) = bincode::deserialize::<FileMetadataV0>(&value) {
                let m: FileMetadata = v0.into();
                (m.id, m.modified_at)
            } else {
                continue;
            };
            out.push((id, modified_at));
        }
        Ok(out)
    }

    /// Fetch a batch of file records by ID. Missing IDs are silently skipped.
    pub fn get_files_batch(&self, ids: &[FileId]) -> Result<Vec<FileMetadata>> {
        let mut out = Vec::new();
        for id in ids {
            if let Ok(Some(m)) = self.get_file(id) {
                out.push(m);
            }
        }
        Ok(out)
    }

    /// Scan all file records and call `f` for each deserialized FileMetadata.
    /// Used by the catch-up pass on leader election — avoids exposing `db` directly.
    pub fn scan_all_files<F>(&self, mut f: F) -> Result<usize>
    where
        F: FnMut(FileMetadata) -> Result<()>,
    {
        let mut count = 0usize;
        for item in self.db.scan_prefix(b"file:") {
            let (_, value) = item?;
            let meta: FileMetadata = if let Ok(m) = bincode::deserialize(&value) { m }
                else if let Ok(v1) = bincode::deserialize::<FileMetadataV1>(&value) { v1.into() }
                else if let Ok(v0) = bincode::deserialize::<FileMetadataV0>(&value) { v0.into() }
                else { continue; };
            f(meta)?;
            count += 1;
        }
        Ok(count)
    }

    // -------------------------------------------------------------------------

    /// List all chunk IDs known in metadata (for leader-coordinated healing).
    /// Returns every chunk ID that has a location record, regardless of which
    /// node holds it locally.
    pub fn list_all_chunk_ids(&self) -> Result<Vec<dfs_common::ChunkId>> {
        let prefix = b"chunk:";
        let mut ids = Vec::new();
        for item in self.db.scan_prefix(prefix) {
            let (_, value) = item?;
            if let Ok(location) = bincode::deserialize::<ChunkLocation>(&value) {
                ids.push(location.chunk_id);
            }
        }
        Ok(ids)
    }

    /// Return all chunk location records in one sled scan.
    /// Used by the discovery pass to build per-node chunk assignment maps without
    /// a second pass over the DB.
    pub fn list_all_chunk_locations(&self) -> Result<Vec<ChunkLocation>> {
        let prefix = b"chunk:";
        let mut locations = Vec::new();
        for item in self.db.scan_prefix(prefix) {
            let (_, value) = item?;
            if let Ok(loc) = bincode::deserialize::<ChunkLocation>(&value) {
                locations.push(loc);
            }
        }
        Ok(locations)
    }

    /// Stream chunk location records, calling `f` for each one.
    /// Return `false` from `f` to stop iteration early.
    /// More memory-efficient than list_all_chunk_locations when only a subset is needed.
    pub fn scan_chunk_locations<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(ChunkLocation) -> bool,
    {
        let prefix = b"chunk:";
        for item in self.db.scan_prefix(prefix) {
            let (_, value) = item?;
            if let Ok(loc) = bincode::deserialize::<ChunkLocation>(&value) {
                if !f(loc) {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Build the set of chunk IDs referenced by any live file in metadata.
    /// Used by the healer to identify orphaned chunk: records (chunks whose
    /// file metadata was deleted but whose chunk: record was not cleaned up).
    /// Scanning file records is cheaper than a reverse index for our file counts.
    pub fn live_chunk_ids(&self) -> Result<std::collections::HashSet<dfs_common::ChunkId>> {
        let mut live = std::collections::HashSet::new();
        let prefix = b"file:";
        for item in self.db.scan_prefix(prefix) {
            let (_, value) = item?;
            // Try current format, then V1, then V0.
            let metadata_opt: Option<FileMetadata> = if let Ok(m) = bincode::deserialize::<FileMetadata>(&value) {
                Some(m)
            } else if let Ok(v1) = bincode::deserialize::<FileMetadataV1>(&value) {
                Some(v1.into())
            } else if let Ok(v0) = bincode::deserialize::<FileMetadataV0>(&value) {
                Some(v0.into())
            } else {
                None
            };
            if let Some(metadata) = metadata_opt {
                for loc in &metadata.chunk_locations {
                    live.insert(loc.chunk_id);
                }
                for &chunk_id in &metadata.chunks {
                    live.insert(chunk_id);
                }
            }
        }
        Ok(live)
    }

    /// Rebuild chunk: routing table entries from file metadata.
    ///
    /// If an earlier healer bug (aggressive orphan purge, crash mid-write, etc.) deleted
    /// chunk: records while the file: record still references those chunks, the healer
    /// can't discover them via its normal sled scan and healing stalls permanently.
    ///
    /// This repair pass reads every file: record, extracts each ChunkLocation embedded in
    /// chunk_locations, and writes a chunk: entry if one doesn't already exist.  Existing
    /// entries are left untouched — this only fills gaps.
    ///
    /// Returns (written, skipped) counts.
    pub fn rebuild_chunk_locations_from_files(&self) -> Result<(usize, usize)> {
        let mut written = 0usize;
        let mut skipped = 0usize;

        for item in self.db.scan_prefix(b"file:") {
            let (_, value) = item?;
            let metadata: FileMetadata = if let Ok(m) = bincode::deserialize::<FileMetadata>(&value) {
                m
            } else if let Ok(v1) = bincode::deserialize::<FileMetadataV1>(&value) {
                v1.into()
            } else if let Ok(v0) = bincode::deserialize::<FileMetadataV0>(&value) {
                v0.into()
            } else {
                continue;
            };

            for loc in &metadata.chunk_locations {
                let key = self.chunk_key(&loc.chunk_id);
                if self.db.get(&key)?.is_none() {
                    let bytes = bincode::serialize(loc)
                        .context("Failed to serialize ChunkLocation during rebuild")?;
                    self.db.insert(key, bytes)?;
                    warn!(
                        "Rebuilt missing chunk: record for {} (file: {})",
                        loc.chunk_id, metadata.path
                    );
                    written += 1;
                } else {
                    skipped += 1;
                }
            }
        }

        if written > 0 {
            info!(
                "Chunk location rebuild: {} missing records restored, {} already present",
                written, skipped
            );
        }
        Ok((written, skipped))
    }

    /// Get chunk location information
    pub fn get_chunk_location(&self, chunk_id: &dfs_common::ChunkId) -> Result<Option<ChunkLocation>> {
        let key = self.chunk_key(chunk_id);

        match self.db.get(&key)? {
            Some(value) => {
                // Try current format (6 fields), then V1 (5 fields, no written_at),
                // then V0 (4 fields, no file_offset or written_at).
                match bincode::deserialize::<ChunkLocation>(&value) {
                    Ok(location) => Ok(Some(location)),
                    Err(_) => match bincode::deserialize::<ChunkLocationV1>(&value) {
                        Ok(v1) => {
                            let location = ChunkLocation::from(v1);
                            if let Ok(encoded) = bincode::serialize(&location) {
                                let _ = self.db.insert(&key, encoded);
                            }
                            Ok(Some(location))
                        }
                        Err(_) => match bincode::deserialize::<ChunkLocationV0>(&value) {
                            Ok(v0) => {
                                let location = ChunkLocation::from(v0);
                                // Migrate in place so we don't hit this path again
                                if let Ok(encoded) = bincode::serialize(&location) {
                                    let _ = self.db.insert(&key, encoded);
                                }
                                Ok(Some(location))
                            }
                            Err(e) => Err(anyhow::anyhow!("Failed to deserialize chunk location (tried all formats): {}", e)),
                        },
                    },
                }
            }
            None => Ok(None),
        }
    }

    /// Delete chunk location
    pub fn delete_chunk_location(&self, chunk_id: &dfs_common::ChunkId) -> Result<()> {
        let key = self.chunk_key(chunk_id);
        self.db.remove(key)?;
        Ok(())
    }

    /// Flush all pending writes to disk (for durability)
    pub fn flush(&self) -> Result<()> {
        self.db.flush()?;
        Ok(())
    }

    /// Get database statistics
    pub fn get_stats(&self) -> Result<MetadataStats> {
        let file_count = self.list_files()?.len();
        let size_on_disk = self.db.size_on_disk()?;

        Ok(MetadataStats {
            file_count,
            size_on_disk,
        })
    }

    /// Key for file metadata
    fn file_key(&self, file_id: &FileId) -> Vec<u8> {
        format!("file:{}", file_id).into_bytes()
    }

    /// Key for path index
    fn path_key(&self, path: &str) -> Vec<u8> {
        format!("path:{}", path).into_bytes()
    }

    /// Key for chunk location
    fn chunk_key(&self, chunk_id: &dfs_common::ChunkId) -> Vec<u8> {
        format!("chunk:{}", chunk_id).into_bytes()
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
            file_offset: None,  // Test data doesn't need file offsets
        };

        store.put_chunk_location(&location).unwrap();

        let retrieved = store.get_chunk_location(&chunk_id).unwrap().unwrap();
        assert_eq!(retrieved.nodes.len(), 2);
        assert_eq!(retrieved.size, 4096);
    }
}
