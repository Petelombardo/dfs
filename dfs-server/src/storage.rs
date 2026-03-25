use anyhow::{Context, Result};
use dfs_common::{compute_chunk_hash, verify_chunk_hash, ChunkId};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Local chunk storage manager
/// Optimized for SBC environments (limited CPU/RAM)
pub struct ChunkStorage {
    /// Root directory for chunk storage
    data_dir: PathBuf,
}

impl ChunkStorage {
    /// Create a new chunk storage instance
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        // Create data directory if it doesn't exist
        fs::create_dir_all(&data_dir)
            .with_context(|| format!("Failed to create data directory: {:?}", data_dir))?;

        info!("Initialized chunk storage at {:?}", data_dir);

        Ok(Self { data_dir })
    }

    /// Write a chunk to local storage with checksum verification
    pub fn write_chunk(&self, chunk_id: &ChunkId, data: &[u8]) -> Result<()> {
        let start = std::time::Instant::now();

        // Verify checksum before writing
        let checksum_start = std::time::Instant::now();
        if !verify_chunk_hash(data, &chunk_id.hash) {
            anyhow::bail!("Checksum mismatch: data does not match chunk ID");
        }
        let checksum_time = checksum_start.elapsed();

        let path = self.get_chunk_path(chunk_id);

        // Create parent directories
        let mkdir_start = std::time::Instant::now();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create chunk directory: {:?}", parent))?;
        }
        let mkdir_time = mkdir_start.elapsed();

        // Write data atomically using temporary file
        let write_start = std::time::Instant::now();
        let temp_path = path.with_extension("tmp");
        let mut file = fs::File::create(&temp_path)
            .with_context(|| format!("Failed to create temporary file: {:?}", temp_path))?;

        file.write_all(data)
            .context("Failed to write chunk data")?;

        let sync_start = std::time::Instant::now();
        file.sync_all().context("Failed to sync chunk data")?;
        let sync_time = sync_start.elapsed();

        // Atomic rename
        let rename_start = std::time::Instant::now();
        fs::rename(&temp_path, &path)
            .with_context(|| format!("Failed to rename chunk file: {:?}", path))?;
        let rename_time = rename_start.elapsed();

        let total_time = start.elapsed();
        debug!("Wrote chunk {} ({} bytes) in {:?} - checksum: {:?}, mkdir: {:?}, sync: {:?}, rename: {:?}",
               chunk_id, data.len(), total_time, checksum_time, mkdir_time, sync_time, rename_time);

        Ok(())
    }

    /// Read a chunk from local storage
    /// Does NOT verify checksum (for SBC performance) - use verify_chunk() for scrubbing
    pub fn read_chunk(&self, chunk_id: &ChunkId) -> Result<Vec<u8>> {
        let path = self.get_chunk_path(chunk_id);

        let mut file = fs::File::open(&path)
            .with_context(|| format!("Failed to open chunk file: {:?}", path))?;

        let mut data = Vec::new();
        file.read_to_end(&mut data)
            .context("Failed to read chunk data")?;

        debug!("Read chunk {} ({} bytes)", chunk_id, data.len());

        Ok(data)
    }

    /// Read and verify a chunk (used during scrubbing or error recovery)
    pub fn read_and_verify_chunk(&self, chunk_id: &ChunkId) -> Result<Vec<u8>> {
        let data = self.read_chunk(chunk_id)?;

        // Verify checksum
        if !verify_chunk_hash(&data, &chunk_id.hash) {
            warn!("Checksum mismatch for chunk {}", chunk_id);
            anyhow::bail!("Stored chunk failed checksum verification");
        }

        Ok(data)
    }

    /// Check if a chunk exists in local storage
    pub fn has_chunk(&self, chunk_id: &ChunkId) -> bool {
        let path = self.get_chunk_path(chunk_id);
        path.exists()
    }

    /// Delete a chunk from local storage
    pub fn delete_chunk(&self, chunk_id: &ChunkId) -> Result<()> {
        let path = self.get_chunk_path(chunk_id);

        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("Failed to delete chunk file: {:?}", path))?;

            debug!("Deleted chunk {}", chunk_id);
        }

        Ok(())
    }

    /// Get the filesystem path for a chunk
    /// Uses 2-level directory sharding: chunks/XX/YY/hash
    fn get_chunk_path(&self, chunk_id: &ChunkId) -> PathBuf {
        let (dir1, dir2, filename) = chunk_id.storage_path_components();
        self.data_dir
            .join("chunks")
            .join(dir1)
            .join(dir2)
            .join(filename)
    }

    /// Get storage statistics (lightweight, doesn't verify checksums)
    pub fn get_stats(&self) -> Result<StorageStats> {
        let mut total_chunks = 0;
        let mut total_bytes = 0;

        let chunks_dir = self.data_dir.join("chunks");
        if chunks_dir.exists() {
            Self::count_chunks_recursive(&chunks_dir, &mut total_chunks, &mut total_bytes)?;
        }

        Ok(StorageStats {
            total_chunks,
            total_bytes,
        })
    }

    /// Recursively count chunks and bytes
    fn count_chunks_recursive(dir: &Path, chunks: &mut usize, bytes: &mut u64) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                Self::count_chunks_recursive(&path, chunks, bytes)?;
            } else if path.is_file() && !path.extension().map_or(false, |ext| ext == "tmp") {
                *chunks += 1;
                if let Ok(metadata) = fs::metadata(&path) {
                    *bytes += metadata.len();
                }
            }
        }
        Ok(())
    }

    /// List all chunk IDs in storage (useful for scrubbing/recovery)
    pub fn list_chunks(&self) -> Result<Vec<ChunkId>> {
        let mut chunk_ids = Vec::new();
        let chunks_dir = self.data_dir.join("chunks");

        if chunks_dir.exists() {
            Self::collect_chunks_recursive(&chunks_dir, &mut chunk_ids)?;
        }

        Ok(chunk_ids)
    }

    /// Get filesystem statistics for the data directory
    /// Returns (total_space, free_space, available_space) in bytes
    pub fn get_filesystem_stats(&self) -> Result<(u64, u64, u64)> {
        use std::os::unix::fs::MetadataExt;

        // Get filesystem stats using statvfs
        let metadata = fs::metadata(&self.data_dir)?;
        let dev = metadata.dev();

        // Use nix crate for statvfs
        use nix::sys::statvfs::statvfs;
        let stat = statvfs(&self.data_dir)?;

        let block_size = stat.block_size();
        let total_space = stat.blocks() * block_size;
        let free_space = stat.blocks_free() * block_size;
        let available_space = stat.blocks_available() * block_size;

        Ok((total_space, free_space, available_space))
    }

    /// Recursively collect chunk IDs
    fn collect_chunks_recursive(dir: &Path, chunk_ids: &mut Vec<ChunkId>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                Self::collect_chunks_recursive(&path, chunk_ids)?;
            } else if path.is_file() && !path.extension().map_or(false, |ext| ext == "tmp") {
                // Extract chunk ID from filename (64 hex chars)
                if let Some(filename) = path.file_name().and_then(|s| s.to_str()) {
                    if filename.len() == 64 {
                        if let Ok(hash_bytes) = hex_to_bytes(filename) {
                            if hash_bytes.len() == 32 {
                                let mut hash = [0u8; 32];
                                hash.copy_from_slice(&hash_bytes);
                                chunk_ids.push(ChunkId::from_hash(hash));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Storage statistics
#[derive(Debug, Clone)]
pub struct StorageStats {
    pub total_chunks: usize,
    pub total_bytes: u64,
}

/// Convert hex string to bytes (lightweight, no external deps)
fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .with_context(|| format!("Invalid hex string: {}", hex))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dfs_common::compute_chunk_hash;
    use tempfile::TempDir;

    #[test]
    fn test_write_and_read_chunk() {
        let temp_dir = TempDir::new().unwrap();
        let storage = ChunkStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let data = b"Hello, DFS!";
        let hash = compute_chunk_hash(data);
        let chunk_id = ChunkId::from_hash(hash);

        // Write chunk
        storage.write_chunk(&chunk_id, data).unwrap();

        // Verify it exists
        assert!(storage.has_chunk(&chunk_id));

        // Read chunk back (no verification)
        let read_data = storage.read_chunk(&chunk_id).unwrap();
        assert_eq!(data, read_data.as_slice());

        // Read with verification
        let verified_data = storage.read_and_verify_chunk(&chunk_id).unwrap();
        assert_eq!(data, verified_data.as_slice());
    }

    #[test]
    fn test_delete_chunk() {
        let temp_dir = TempDir::new().unwrap();
        let storage = ChunkStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let data = b"Test data";
        let hash = compute_chunk_hash(data);
        let chunk_id = ChunkId::from_hash(hash);

        storage.write_chunk(&chunk_id, data).unwrap();
        assert!(storage.has_chunk(&chunk_id));

        storage.delete_chunk(&chunk_id).unwrap();
        assert!(!storage.has_chunk(&chunk_id));
    }

    #[test]
    fn test_storage_stats() {
        let temp_dir = TempDir::new().unwrap();
        let storage = ChunkStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let data1 = b"First chunk";
        let data2 = b"Second chunk";

        let chunk1 = ChunkId::from_hash(compute_chunk_hash(data1));
        let chunk2 = ChunkId::from_hash(compute_chunk_hash(data2));

        storage.write_chunk(&chunk1, data1).unwrap();
        storage.write_chunk(&chunk2, data2).unwrap();

        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_chunks, 2);
        assert_eq!(stats.total_bytes, (data1.len() + data2.len()) as u64);
    }

    #[test]
    fn test_list_chunks() {
        let temp_dir = TempDir::new().unwrap();
        let storage = ChunkStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let data1 = b"Chunk 1";
        let data2 = b"Chunk 2";

        let chunk1 = ChunkId::from_hash(compute_chunk_hash(data1));
        let chunk2 = ChunkId::from_hash(compute_chunk_hash(data2));

        storage.write_chunk(&chunk1, data1).unwrap();
        storage.write_chunk(&chunk2, data2).unwrap();

        let chunks = storage.list_chunks().unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(chunks.contains(&chunk1));
        assert!(chunks.contains(&chunk2));
    }

    #[test]
    fn test_checksum_verification_on_write() {
        let temp_dir = TempDir::new().unwrap();
        let storage = ChunkStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let data = b"Test data";
        let wrong_hash = [0u8; 32]; // Wrong hash
        let chunk_id = ChunkId::from_hash(wrong_hash);

        // Should fail because checksum doesn't match
        let result = storage.write_chunk(&chunk_id, data);
        assert!(result.is_err());
    }
}
