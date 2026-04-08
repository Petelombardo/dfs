use anyhow::{Context, Result};
use dfs_common::{ChunkLocation, FileId, FileMetadata, FileMetadataV0};
use sled::Db;
use std::path::PathBuf;
use tracing::{debug, info, warn};

/// Metadata storage using Sled embedded database
/// Optimized for SBC environments (memory-efficient, crash-safe)
pub struct MetadataStore {
    /// Sled database instance
    db: Db,
}

impl MetadataStore {
    /// Create a new metadata store
    pub fn new(metadata_dir: PathBuf) -> Result<Self> {
        // Configure Sled with memory limits to prevent unbounded cache growth
        // Default cache can grow to multiple GB even for small databases
        let cache_capacity_mb = std::env::var("DFS_METADATA_CACHE_MB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(128); // Default 128MB cache

        let cache_capacity_bytes = cache_capacity_mb * 1024 * 1024;

        let db = sled::Config::new()
            .path(&metadata_dir)
            .cache_capacity(cache_capacity_bytes)
            .flush_every_ms(Some(1000))  // Flush every second for durability
            .open()
            .with_context(|| format!("Failed to open metadata database at {:?}", metadata_dir))?;

        info!("Initialized metadata store at {:?} (cache: {}MB)", metadata_dir, cache_capacity_mb);

        Ok(Self { db })
    }

    /// Store file metadata
    pub fn put_file(&self, metadata: &FileMetadata) -> Result<()> {
        let key = self.file_key(&metadata.id);
        let value = bincode::serialize(metadata)
            .context("Failed to serialize file metadata")?;

        self.db
            .insert(key, value)
            .context("Failed to insert file metadata")?;

        // Also index by path for lookups
        let path_key = self.path_key(&metadata.path);
        let id_bytes = bincode::serialize(&metadata.id)?;
        self.db
            .insert(path_key, id_bytes)
            .context("Failed to insert path index")?;

        debug!("Stored metadata for file: {} ({})", metadata.path, metadata.id);

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
                        // If that fails, try old format (bincode can't handle extra fields)
                        match bincode::deserialize::<FileMetadataV0>(&value) {
                            Ok(v0_metadata) => {
                                // Successfully deserialized old format - convert to new
                                let mut metadata: FileMetadata = v0_metadata.into();

                                // CRITICAL: Populate chunk_locations from legacy chunks array
                                // Old files have empty chunk_locations, which causes slow seeks
                                // (client has to query metadata server for each chunk individually)
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

                                // Auto-migrate by writing back in new format
                                if let Err(e) = self.put_file(&metadata) {
                                    warn!("Failed to auto-migrate metadata for {}: {}", file_id, e);
                                }

                                Ok(Some(metadata))
                            }
                            Err(old_err) => {
                                // Failed both formats - report both errors
                                Err(anyhow::anyhow!(
                                    "Failed to deserialize file metadata (tried both formats). \
                                     New format error: {}. Old format error: {}",
                                    new_err, old_err
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

        match self.db.get(path_key)? {
            Some(id_bytes) => {
                let file_id: FileId = bincode::deserialize(&id_bytes)
                    .context("Failed to deserialize file ID")?;
                self.get_file(&file_id)
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

    /// List all files (iterator for memory efficiency on SBCs)
    pub fn list_files(&self) -> Result<Vec<FileMetadata>> {
        let mut files = Vec::new();
        let prefix = b"file:";

        for item in self.db.scan_prefix(prefix) {
            let (key, value) = item?;
            // Try new format first, then V0 fallback, skip entries that fail both.
            let metadata = match bincode::deserialize::<FileMetadata>(&value) {
                Ok(m) => m,
                Err(_) => match bincode::deserialize::<FileMetadataV0>(&value) {
                    Ok(v0) => v0.into(),
                    Err(e) => {
                        warn!("Skipping corrupt metadata entry (key={:?}): {}", key, e);
                        continue;
                    }
                },
            };
            files.push(metadata);
        }

        Ok(files)
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
                    // Deserialize the file ID and fetch the metadata
                    let file_id: FileId = bincode::deserialize(&value)
                        .context("Failed to deserialize file ID from path index")?;

                    if let Some(metadata) = self.get_file(&file_id)? {
                        files.push(metadata);
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

    /// Get chunk location information
    pub fn get_chunk_location(&self, chunk_id: &dfs_common::ChunkId) -> Result<Option<ChunkLocation>> {
        let key = self.chunk_key(chunk_id);

        match self.db.get(key)? {
            Some(value) => {
                let location: ChunkLocation = bincode::deserialize(&value)
                    .context("Failed to deserialize chunk location")?;
                Ok(Some(location))
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
