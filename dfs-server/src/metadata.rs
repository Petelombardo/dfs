use anyhow::{Context, Result};
use dfs_common::{ChunkLocation, FileId, FileMetadata};
use sled::Db;
use std::path::PathBuf;
use tracing::{debug, info};

/// Metadata storage using Sled embedded database
/// Optimized for SBC environments (memory-efficient, crash-safe)
pub struct MetadataStore {
    /// Sled database instance
    db: Db,
}

impl MetadataStore {
    /// Create a new metadata store
    pub fn new(metadata_dir: PathBuf) -> Result<Self> {
        let db = sled::open(&metadata_dir)
            .with_context(|| format!("Failed to open metadata database at {:?}", metadata_dir))?;

        info!("Initialized metadata store at {:?}", metadata_dir);

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
    pub fn get_file(&self, file_id: &FileId) -> Result<Option<FileMetadata>> {
        let key = self.file_key(file_id);

        match self.db.get(key)? {
            Some(value) => {
                let metadata: FileMetadata = bincode::deserialize(&value)
                    .context("Failed to deserialize file metadata")?;
                Ok(Some(metadata))
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

    /// List all files (iterator for memory efficiency on SBCs)
    pub fn list_files(&self) -> Result<Vec<FileMetadata>> {
        let mut files = Vec::new();
        let prefix = b"file:";

        for item in self.db.scan_prefix(prefix) {
            let (_, value) = item?;
            let metadata: FileMetadata = bincode::deserialize(&value)
                .context("Failed to deserialize file metadata")?;
            files.push(metadata);
        }

        Ok(files)
    }

    /// List files in a directory (prefix scan)
    pub fn list_directory(&self, dir_path: &str) -> Result<Vec<FileMetadata>> {
        let mut files = Vec::new();
        let all_files = self.list_files()?;

        // Normalize directory path
        let dir_path = if dir_path.ends_with('/') {
            dir_path.to_string()
        } else {
            format!("{}/", dir_path)
        };

        for metadata in all_files {
            if metadata.path.starts_with(&dir_path) {
                // Only include direct children, not nested
                let relative = &metadata.path[dir_path.len()..];
                if !relative.contains('/') || relative.ends_with('/') {
                    files.push(metadata);
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
        };

        store.put_chunk_location(&location).unwrap();

        let retrieved = store.get_chunk_location(&chunk_id).unwrap().unwrap();
        assert_eq!(retrieved.nodes.len(), 2);
        assert_eq!(retrieved.size, 4096);
    }
}
