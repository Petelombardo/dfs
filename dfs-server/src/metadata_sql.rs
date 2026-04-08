//! SQL-based metadata storage for efficient sparse file support
//!
//! This module implements file and chunk metadata storage using SQLite,
//! providing O(log n) chunk lookups by file offset and proper sparse file support.

use anyhow::{Context, Result};
use dfs_common::{ChunkId, ChunkLocation, FileId, FileMetadata, FileType, NodeId};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use tracing::{debug, info};

/// SQL-based metadata store for files and chunks
pub struct SqlMetadataStore {
    conn: Connection,
}

impl SqlMetadataStore {
    /// Create a new SQL metadata store at the specified path
    pub fn new<P: AsRef<Path>>(db_path: P) -> Result<Self> {
        let conn = Connection::open(db_path.as_ref())
            .context("Failed to open SQLite database")?;

        // Enable WAL mode for better concurrency
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;"
        )?;

        let store = Self { conn };
        store.initialize_schema()?;
        Ok(store)
    }

    /// Initialize database schema
    fn initialize_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                id BLOB PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                size INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                modified_at INTEGER NOT NULL,
                mode INTEGER NOT NULL,
                uid INTEGER NOT NULL,
                gid INTEGER NOT NULL,
                file_type INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS chunks (
                file_id BLOB NOT NULL,
                chunk_id BLOB NOT NULL,
                file_offset INTEGER,
                size INTEGER NOT NULL,
                checksum BLOB NOT NULL,
                PRIMARY KEY (file_id, chunk_id),
                FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_chunks_file_offset
                ON chunks(file_id, file_offset)
                WHERE file_offset IS NOT NULL;

            CREATE TABLE IF NOT EXISTS chunk_replicas (
                chunk_id BLOB NOT NULL,
                node_id BLOB NOT NULL,
                PRIMARY KEY (chunk_id, node_id)
            );"
        ).context("Failed to initialize schema")?;

        info!("SQL metadata schema initialized");
        Ok(())
    }

    /// Store file metadata
    pub fn put_file_metadata(&self, metadata: &FileMetadata) -> Result<()> {
        let file_type_int = match metadata.file_type {
            FileType::RegularFile => 0,
            FileType::Directory => 1,
            FileType::Symlink => 2,
        };

        // Start transaction
        let tx = self.conn.unchecked_transaction()?;

        // Insert or replace file metadata
        tx.execute(
            "INSERT OR REPLACE INTO files (id, path, size, created_at, modified_at, mode, uid, gid, file_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                metadata.id.as_bytes(),
                &metadata.path,
                metadata.size as i64,
                metadata.created_at as i64,
                metadata.modified_at as i64,
                metadata.mode as i64,
                metadata.uid as i64,
                metadata.gid as i64,
                file_type_int,
            ],
        )?;

        // Delete existing chunks for this file
        tx.execute("DELETE FROM chunks WHERE file_id = ?1", params![metadata.id.as_bytes()])?;
        tx.execute("DELETE FROM chunk_replicas WHERE chunk_id IN (SELECT chunk_id FROM chunks WHERE file_id = ?1)",
                   params![metadata.id.as_bytes()])?;

        // Insert chunk locations
        for location in &metadata.chunk_locations {
            // Insert chunk
            tx.execute(
                "INSERT INTO chunks (file_id, chunk_id, file_offset, size, checksum)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    metadata.id.as_bytes(),
                    location.chunk_id.as_bytes(),
                    location.file_offset.map(|o| o as i64),
                    location.size as i64,
                    &location.checksum[..],
                ],
            )?;

            // Insert replicas
            for node_id in &location.nodes {
                tx.execute(
                    "INSERT OR IGNORE INTO chunk_replicas (chunk_id, node_id) VALUES (?1, ?2)",
                    params![location.chunk_id.as_bytes(), node_id.as_bytes()],
                )?;
            }
        }

        tx.commit()?;
        debug!("Stored metadata for file: {} ({} chunks)", metadata.path, metadata.chunk_locations.len());
        Ok(())
    }

    /// Get file metadata by ID
    pub fn get_file_metadata(&self, file_id: &FileId) -> Result<Option<FileMetadata>> {
        // Get file record
        let file = self.conn.query_row(
            "SELECT path, size, created_at, modified_at, mode, uid, gid, file_type
             FROM files WHERE id = ?1",
            params![file_id.as_bytes()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        ).optional()?;

        let Some((path, size, created_at, modified_at, mode, uid, gid, file_type_int)) = file else {
            return Ok(None);
        };

        let file_type = match file_type_int {
            0 => FileType::RegularFile,
            1 => FileType::Directory,
            2 => FileType::Symlink,
            _ => FileType::RegularFile,
        };

        // Get chunks first
        let mut stmt = self.conn.prepare(
            "SELECT chunk_id, file_offset, size, checksum
             FROM chunks
             WHERE file_id = ?1
             ORDER BY file_offset"
        )?;

        let chunk_data: Vec<(Vec<u8>, Option<i64>, i64, Vec<u8>)> = stmt.query_map(
            params![file_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        )?.collect::<Result<Vec<_>, _>>()?;

        // Now get replicas for each chunk and build ChunkLocations
        let mut chunk_locations = Vec::new();
        for (chunk_id_bytes, file_offset, size, checksum_bytes) in chunk_data {
            let mut chunk_id_array = [0u8; 32];
            chunk_id_array.copy_from_slice(&chunk_id_bytes);
            let chunk_id = ChunkId::from_hash(chunk_id_array);

            let mut checksum = [0u8; 32];
            checksum.copy_from_slice(&checksum_bytes);

            // Get node_ids for this chunk
            let mut node_stmt = self.conn.prepare(
                "SELECT node_id FROM chunk_replicas WHERE chunk_id = ?1"
            )?;
            let nodes: Vec<NodeId> = node_stmt.query_map(params![&chunk_id_bytes], |row| {
                let node_bytes: Vec<u8> = row.get(0)?;
                let mut node_array = [0u8; 16];
                node_array.copy_from_slice(&node_bytes);
                Ok(NodeId::from_bytes(node_array))
            })?.collect::<Result<Vec<_>, _>>()?;

            chunk_locations.push(ChunkLocation {
                chunk_id,
                nodes,
                size: size as usize,
                checksum,
                file_offset: file_offset.map(|o| o as u64),
            });
        }

        Ok(Some(FileMetadata {
            id: *file_id,
            path,
            size: size as u64,
            chunks: Vec::new(),  // Deprecated
            chunk_sizes: Vec::new(),  // Deprecated
            created_at: created_at as u64,
            modified_at: modified_at as u64,
            mode: mode as u32,
            uid: uid as u32,
            gid: gid as u32,
            file_type,
            chunk_locations,
        }))
    }

    /// Get file metadata by path
    pub fn get_file_metadata_by_path(&self, path: &str) -> Result<Option<FileMetadata>> {
        let file_id_bytes: Option<Vec<u8>> = self.conn.query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![path],
            |row| row.get(0),
        ).optional()?;

        if let Some(id_bytes) = file_id_bytes {
            let mut id_array = [0u8; 16];
            id_array.copy_from_slice(&id_bytes);
            let file_id = FileId::from_bytes(id_array);
            self.get_file_metadata(&file_id)
        } else {
            Ok(None)
        }
    }

    /// Find chunk at specific file offset (for sparse file reads)
    pub fn find_chunk_at_offset(&self, file_id: &FileId, offset: u64) -> Result<Option<ChunkLocation>> {
        // Find chunk where offset falls within [file_offset, file_offset + size)
        let chunk_data: Option<(Vec<u8>, Option<i64>, i64, Vec<u8>)> = self.conn.query_row(
            "SELECT chunk_id, file_offset, size, checksum
             FROM chunks
             WHERE file_id = ?1
               AND file_offset IS NOT NULL
               AND file_offset <= ?2
               AND file_offset + size > ?2
             LIMIT 1",
            params![file_id.as_bytes(), offset as i64],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        ).optional()?;

        if let Some((chunk_id_bytes, file_offset, size, checksum_bytes)) = chunk_data {
            let mut chunk_id_array = [0u8; 32];
            chunk_id_array.copy_from_slice(&chunk_id_bytes);
            let chunk_id = ChunkId::from_hash(chunk_id_array);

            let mut checksum = [0u8; 32];
            checksum.copy_from_slice(&checksum_bytes);

            // Get node_ids for this chunk
            let mut node_stmt = self.conn.prepare(
                "SELECT node_id FROM chunk_replicas WHERE chunk_id = ?1"
            )?;
            let nodes: Vec<NodeId> = node_stmt.query_map(params![&chunk_id_bytes], |row| {
                let node_bytes: Vec<u8> = row.get(0)?;
                let mut node_array = [0u8; 16];
                node_array.copy_from_slice(&node_bytes);
                Ok(NodeId::from_bytes(node_array))
            })?.collect::<Result<Vec<_>, _>>()?;

            Ok(Some(ChunkLocation {
                chunk_id,
                nodes,
                size: size as usize,
                checksum,
                file_offset: file_offset.map(|o| o as u64),
            }))
        } else {
            Ok(None)
        }
    }

    /// Delete file metadata
    pub fn delete_file_metadata(&self, file_id: &FileId) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;

        // Cascade delete will handle chunks and replicas
        tx.execute("DELETE FROM files WHERE id = ?1", params![file_id.as_bytes()])?;

        tx.commit()?;
        debug!("Deleted metadata for file: {:?}", file_id);
        Ok(())
    }

    /// List all files in a directory
    pub fn list_directory(&self, dir_path: &str) -> Result<Vec<FileMetadata>> {
        let pattern = format!("{}/%", dir_path.trim_end_matches('/'));

        let mut stmt = self.conn.prepare(
            "SELECT id FROM files WHERE path LIKE ?1 AND path NOT LIKE ?2"
        )?;

        let deep_pattern = format!("{}/%/%", dir_path.trim_end_matches('/'));
        let file_ids: Vec<Vec<u8>> = stmt.query_map(params![pattern, deep_pattern], |row| {
            row.get(0)
        })?.collect::<Result<Vec<_>, _>>()?;

        let mut files = Vec::new();
        for id_bytes in file_ids {
            let mut id_array = [0u8; 16];
            id_array.copy_from_slice(&id_bytes);
            let file_id = FileId::from_bytes(id_array);
            if let Some(metadata) = self.get_file_metadata(&file_id)? {
                files.push(metadata);
            }
        }

        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_basic_operations() {
        let temp_file = NamedTempFile::new().unwrap();
        let store = SqlMetadataStore::new(temp_file.path()).unwrap();

        // Create test metadata
        let mut metadata = FileMetadata::new("/test/file.txt".to_string(), FileType::RegularFile);
        metadata.size = 8192;

        let chunk_id = ChunkId::from_hash([1u8; 32]);
        metadata.chunk_locations.push(ChunkLocation {
            chunk_id,
            nodes: vec![NodeId::new(), NodeId::new()],
            size: 4096,
            checksum: [2u8; 32],
            file_offset: Some(0),
        });

        // Store and retrieve
        store.put_file_metadata(&metadata).unwrap();
        let retrieved = store.get_file_metadata(&metadata.id).unwrap().unwrap();

        assert_eq!(retrieved.path, metadata.path);
        assert_eq!(retrieved.size, metadata.size);
        assert_eq!(retrieved.chunk_locations.len(), 1);
        assert_eq!(retrieved.chunk_locations[0].chunk_id, chunk_id);
    }

    #[test]
    fn test_sparse_file_lookup() {
        let temp_file = NamedTempFile::new().unwrap();
        let store = SqlMetadataStore::new(temp_file.path()).unwrap();

        let mut metadata = FileMetadata::new("/sparse.dat".to_string(), FileType::RegularFile);
        metadata.size = 20480;

        // Add chunks with gaps (sparse file)
        let node1 = NodeId::new();
        metadata.chunk_locations.push(ChunkLocation {
            chunk_id: ChunkId::from_hash([1u8; 32]),
            nodes: vec![node1],
            size: 4096,
            checksum: [0u8; 32],
            file_offset: Some(0),  // Bytes 0-4095
        });

        // Gap: bytes 4096-8191 (no chunk = hole)

        metadata.chunk_locations.push(ChunkLocation {
            chunk_id: ChunkId::from_hash([2u8; 32]),
            nodes: vec![node1],
            size: 4096,
            checksum: [0u8; 32],
            file_offset: Some(8192),  // Bytes 8192-12287
        });

        store.put_file_metadata(&metadata).unwrap();

        // Test lookups
        let chunk_at_0 = store.find_chunk_at_offset(&metadata.id, 0).unwrap();
        assert!(chunk_at_0.is_some());
        assert_eq!(chunk_at_0.unwrap().file_offset, Some(0));

        let chunk_at_gap = store.find_chunk_at_offset(&metadata.id, 5000).unwrap();
        assert!(chunk_at_gap.is_none());  // Hole!

        let chunk_at_8192 = store.find_chunk_at_offset(&metadata.id, 8192).unwrap();
        assert!(chunk_at_8192.is_some());
        assert_eq!(chunk_at_8192.unwrap().file_offset, Some(8192));
    }
}
