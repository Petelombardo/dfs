use anyhow::Result;
use dfs_common::{ChunkId, FileMetadata, FileType};
use fuser::{
    FileAttr, FileType as FuseFileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEntry, ReplyStatfs, Request as FuseRequest,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use crate::client::DfsClient;

/// Buffered write data for a single file
#[derive(Clone)]
struct WriteBuffer {
    /// Buffered data
    data: Vec<u8>,
    /// When this buffer was last modified
    last_modified: SystemTime,
}

/// FUSE filesystem implementation for DFS
pub struct DfsFilesystem {
    /// Client for communicating with DFS cluster
    client: Arc<DfsClient>,

    /// Metadata cache: inode -> FileMetadata
    metadata_cache: Arc<RwLock<HashMap<u64, FileMetadata>>>,

    /// Path to inode mapping
    path_to_inode: Arc<RwLock<HashMap<String, u64>>>,

    /// Next available inode number
    next_inode: Arc<RwLock<u64>>,

    /// Root inode is always 1 (FUSE convention)
    root_inode: u64,

    /// Tokio runtime handle for async operations
    runtime: tokio::runtime::Handle,

    /// Write counter per inode for batching metadata updates
    write_counters: Arc<RwLock<HashMap<u64, usize>>>,

    /// Enable write-behind buffering
    write_buffer_enabled: bool,

    /// Write buffers per inode (only used if write_buffer_enabled)
    write_buffers: Arc<Mutex<HashMap<u64, WriteBuffer>>>,

    /// Last read chunk cache: (ino, chunk_index, data)
    /// Prevents re-fetching same 4MB chunk for multiple 128KB FUSE reads
    last_chunk_cache: Arc<RwLock<Option<(u64, usize, Vec<u8>)>>>,
}

impl DfsFilesystem {
    /// Create a new DFS filesystem with an explicit runtime handle
    pub fn new_with_runtime(
        cluster_nodes: Vec<SocketAddr>,
        write_buffer_enabled: bool,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self> {
        let client = Arc::new(DfsClient::new(cluster_nodes)?);

        let metadata_cache = Arc::new(RwLock::new(HashMap::new()));
        let path_to_inode = Arc::new(RwLock::new(HashMap::new()));
        let next_inode = Arc::new(RwLock::new(2)); // Start at 2, root is 1

        // Create root directory metadata
        let root_metadata = FileMetadata {
            id: dfs_common::FileId::new(),
            path: "/".to_string(),
            size: 0,
            chunks: Vec::new(),
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            modified_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            mode: 0o755,
            uid: 0,
            gid: 0,
            file_type: FileType::Directory,
        };

        metadata_cache.write().unwrap().insert(1, root_metadata);
        path_to_inode.write().unwrap().insert("/".to_string(), 1);

        Ok(Self {
            client,
            metadata_cache,
            path_to_inode,
            next_inode,
            root_inode: 1,
            runtime,
            write_counters: Arc::new(RwLock::new(HashMap::new())),
            write_buffer_enabled,
            write_buffers: Arc::new(Mutex::new(HashMap::new())),
            last_chunk_cache: Arc::new(RwLock::new(None)),
        })
    }

    /// Create a new DFS filesystem (deprecated - use new_with_runtime)
    #[allow(dead_code)]
    pub fn new(cluster_nodes: Vec<SocketAddr>, write_buffer_enabled: bool) -> Result<Self> {
        // This version tries to get the current runtime handle
        // Only works if called from within a tokio runtime context
        let runtime = tokio::runtime::Handle::current();
        Self::new_with_runtime(cluster_nodes, write_buffer_enabled, runtime)
    }

    /// Execute an async operation in a blocking context
    /// Uses block_in_place to allow blocking within an async runtime
    fn block_on<F, T>(&self, future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        // NOTE: We can't use block_in_place because FUSE callbacks don't run on tokio worker threads
        // Just block_on directly using the runtime handle
        self.runtime.block_on(future)
    }

    /// Flush buffered writes for a specific inode to the cluster
    async fn flush_buffer_async(&self, ino: u64) -> Result<()> {
        // Get and remove buffer for this inode
        let buffer_opt = {
            let mut buffers = self.write_buffers.lock().await;
            buffers.remove(&ino)
        };

        if let Some(buffer) = buffer_opt {
            info!("Flushing {} bytes for inode {}", buffer.data.len(), ino);

            // Get current metadata to get existing chunks
            let mut metadata = {
                let cache = self.metadata_cache.read().unwrap();
                match cache.get(&ino) {
                    Some(m) => m.clone(),
                    None => {
                        anyhow::bail!("Metadata not found for inode {}", ino);
                    }
                }
            };

            // Write buffered data as new chunks (appending)
            let new_chunk_ids = self.client.write_data(&buffer.data).await?;

            // Append new chunks to existing chunks
            metadata.chunks.extend(new_chunk_ids);
            metadata.modified_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            // Store updated metadata
            self.client.put_file_metadata(&metadata).await?;

            // Update cache
            self.metadata_cache.write().unwrap().insert(ino, metadata);
        }

        Ok(())
    }

    /// Convert FileMetadata to FUSE FileAttr
    fn metadata_to_attr(&self, ino: u64, metadata: &FileMetadata) -> FileAttr {
        let kind = match metadata.file_type {
            FileType::RegularFile => FuseFileType::RegularFile,
            FileType::Directory => FuseFileType::Directory,
            FileType::Symlink => FuseFileType::Symlink,
        };

        FileAttr {
            ino,
            size: metadata.size,
            blocks: (metadata.size + 511) / 512, // 512-byte blocks
            atime: UNIX_EPOCH + Duration::from_secs(metadata.modified_at),
            mtime: UNIX_EPOCH + Duration::from_secs(metadata.modified_at),
            ctime: UNIX_EPOCH + Duration::from_secs(metadata.created_at),
            crtime: UNIX_EPOCH + Duration::from_secs(metadata.created_at),
            kind,
            perm: metadata.mode as u16,
            nlink: 1,
            uid: metadata.uid,
            gid: metadata.gid,
            rdev: 0,
            blksize: 4 * 1024 * 1024, // 4MB chunk size
            flags: 0,
        }
    }

    /// Get or allocate inode for a path
    fn get_or_create_inode(&self, path: &str) -> u64 {
        let path_map = self.path_to_inode.read().unwrap();
        if let Some(&ino) = path_map.get(path) {
            return ino;
        }
        drop(path_map);

        // Allocate new inode
        let mut next = self.next_inode.write().unwrap();
        let ino = *next;
        *next += 1;
        drop(next);

        self.path_to_inode
            .write()
            .unwrap()
            .insert(path.to_string(), ino);

        ino
    }

    /// Get path from parent inode and name
    fn get_path_from_parent(&self, parent: u64, name: &OsStr) -> Option<String> {
        let cache = self.metadata_cache.read().unwrap();
        let parent_metadata = cache.get(&parent)?;
        let name_str = name.to_str()?;

        let parent_path = &parent_metadata.path;
        let full_path = if parent_path == "/" {
            format!("/{}", name_str)
        } else {
            format!("{}/{}", parent_path, name_str)
        };

        Some(full_path)
    }
}

impl Filesystem for DfsFilesystem {
    fn lookup(&mut self, _req: &FuseRequest, parent: u64, name: &OsStr, reply: ReplyEntry) {
        debug!("lookup: parent={}, name={:?}", parent, name);

        let path = match self.get_path_from_parent(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Check cache first
        {
            let path_map = self.path_to_inode.read().unwrap();
            if let Some(&ino) = path_map.get(&path) {
                let cache = self.metadata_cache.read().unwrap();
                if let Some(metadata) = cache.get(&ino) {
                    let attr = self.metadata_to_attr(ino, metadata);
                    reply.entry(&Duration::from_secs(300), &attr, 0); // 5 minutes
                    return;
                }
            }
        }

        // Fetch from cluster
        let client = self.client.clone();
        let result = self.block_on(async {
            client.get_file_metadata(&path).await
        });

        match result {
            Ok(Some(metadata)) => {
                // Allocate inode
                let ino = self.get_or_create_inode(&path);

                // Cache metadata
                self.metadata_cache.write().unwrap().insert(ino, metadata.clone());

                // Convert to FUSE attr
                let attr = self.metadata_to_attr(ino, &metadata);
                reply.entry(&Duration::from_secs(3600), &attr, 0);
            }
            Ok(None) => {
                reply.error(libc::ENOENT);
            }
            Err(e) => {
                error!("Failed to lookup {}: {}", path, e);
                reply.error(libc::EIO);
            }
        }
    }

    fn open(&mut self, _req: &FuseRequest, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        info!("open: ino={}", ino);

        // Return success with file handle 0 and NO direct_io flag
        // This tells the kernel to use page cache for reads
        reply.opened(0, fuser::consts::FOPEN_KEEP_CACHE);
    }

    fn getattr(&mut self, _req: &FuseRequest, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        debug!("getattr: ino={}", ino);

        let cache = self.metadata_cache.read().unwrap();
        if let Some(metadata) = cache.get(&ino) {
            let attr = self.metadata_to_attr(ino, metadata);
            // 5 minute TTL for kernel page caching
            // Balances performance vs. freshness for multi-client scenarios
            reply.attr(&Duration::from_secs(300), &attr);
        } else {
            reply.error(libc::ENOENT);
        }
    }

    fn read(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let start = std::time::Instant::now();
        info!("FUSE read START: ino={}, offset={}, size={}", ino, offset, size);

        let metadata = {
            let cache = self.metadata_cache.read().unwrap();
            match cache.get(&ino) {
                Some(m) => m.clone(),
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        if metadata.file_type != FileType::RegularFile {
            reply.error(libc::EISDIR);
            return;
        }

        let offset = offset as usize;
        let size = size as usize;

        // Early return for out of bounds
        if offset >= metadata.size as usize {
            reply.data(&[]);
            return;
        }

        // Calculate which chunks we actually need
        const CHUNK_SIZE: usize = 4 * 1024 * 1024; // 4MB chunks
        let start_chunk = offset / CHUNK_SIZE;

        // Check cache first for single-chunk reads (most common case)
        let cache_hit = {
            let cache = self.last_chunk_cache.read().unwrap();
            if let Some((cached_ino, cached_idx, cached_data)) = cache.as_ref() {
                *cached_ino == ino && *cached_idx == start_chunk
            } else {
                false
            }
        };

        let chunk_data = if cache_hit {
            // Use cached chunk
            let cache = self.last_chunk_cache.read().unwrap();
            cache.as_ref().unwrap().2.clone()
        } else {
            // Fetch chunk from server
            let chunk_ids = vec![metadata.chunks[start_chunk]];
            let client = self.client.clone();

            let result = self.block_on(async {
                client.read_data(&chunk_ids).await
            });

            match result {
                Ok(data) => {
                    // Cache this chunk for next read
                    *self.last_chunk_cache.write().unwrap() = Some((ino, start_chunk, data.clone()));
                    data
                }
                Err(e) => {
                    error!("Failed to read data: {}", e);
                    reply.error(libc::EIO);
                    return;
                }
            }
        };

        // Calculate offset within the chunk and return requested slice
        let offset_in_chunk = offset % CHUNK_SIZE;
        let data_start = offset_in_chunk;
        let data_end = std::cmp::min(data_start + size, chunk_data.len());

        if data_start >= chunk_data.len() {
            reply.data(&[]);
        } else {
            reply.data(&chunk_data[data_start..data_end]);
        }

        let elapsed = start.elapsed();
        info!("FUSE read COMPLETE: ino={}, offset={}, size={}, took {:?}",
              ino, offset, size, elapsed);
    }

    fn readdir(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        debug!("readdir: ino={}, offset={}", ino, offset);

        let path = {
            let cache = self.metadata_cache.read().unwrap();
            match cache.get(&ino) {
                Some(metadata) => {
                    if metadata.file_type != FileType::Directory {
                        reply.error(libc::ENOTDIR);
                        return;
                    }
                    metadata.path.clone()
                }
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        let client = self.client.clone();
        let result = self.block_on(async {
            client.list_directory(&path).await
        });

        match result {
            Ok(entries) => {
                let mut entry_offset = 0i64;

                // Add . and ..
                if offset == 0 {
                    if reply.add(ino, 1, FuseFileType::Directory, ".") {
                        reply.ok();
                        return;
                    }
                    entry_offset += 1;
                }
                if offset <= 1 {
                    if reply.add(ino, 2, FuseFileType::Directory, "..") {
                        reply.ok();
                        return;
                    }
                    entry_offset += 1;
                }

                // Add actual entries
                let skip_count = if offset > 2 { (offset - 2) as usize } else { 0 };
                for (i, entry) in entries.iter().enumerate().skip(skip_count) {
                    let file_name = entry.path.rsplit('/').next().unwrap_or("");

                    // Skip entries with empty filenames (like the root directory "/")
                    if file_name.is_empty() {
                        debug!("Skipping entry with empty filename: path={}", entry.path);
                        continue;
                    }

                    let kind = match entry.file_type {
                        FileType::RegularFile => FuseFileType::RegularFile,
                        FileType::Directory => FuseFileType::Directory,
                        FileType::Symlink => FuseFileType::Symlink,
                    };

                    // Get or allocate inode
                    let entry_ino = self.get_or_create_inode(&entry.path);

                    // Cache metadata
                    self.metadata_cache
                        .write()
                        .unwrap()
                        .insert(entry_ino, entry.clone());

                    let next_offset = 3 + i as i64;  // 3 because . is 1, .. is 2, first file is 3
                    if reply.add(entry_ino, next_offset, kind, file_name) {
                        break; // Buffer full
                    }
                }

                reply.ok();
            }
            Err(e) => {
                error!("Failed to read directory {}: {}", path, e);
                reply.error(libc::EIO);
            }
        }
    }

    fn create(
        &mut self,
        _req: &FuseRequest,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        debug!("create: parent={}, name={:?}, mode={:o}", parent, name, mode);

        let path = match self.get_path_from_parent(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Create file metadata
        let metadata = FileMetadata {
            id: dfs_common::FileId::new(),
            path: path.clone(),
            size: 0,
            chunks: Vec::new(),
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            modified_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            mode,
            uid: _req.uid(),
            gid: _req.gid(),
            file_type: FileType::RegularFile,
        };

        // Store metadata on cluster
        let client = self.client.clone();
        let metadata_clone = metadata.clone();
        let result = self.block_on(async {
            client.put_file_metadata(&metadata_clone).await
        });

        match result {
            Ok(_) => {
                // Allocate inode
                let ino = self.get_or_create_inode(&path);

                // Cache metadata
                self.metadata_cache.write().unwrap().insert(ino, metadata.clone());

                // Convert to FUSE attr
                let attr = self.metadata_to_attr(ino, &metadata);
                // ReplyCreate expects: ttl, attr, generation, fh, flags
                reply.created(&Duration::from_secs(300), &attr, 0, 0, 0); // 5 minutes
            }
            Err(e) => {
                error!("Failed to create file {}: {}", path, e);
                reply.error(libc::EIO);
            }
        }
    }

    fn write(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyWrite,
    ) {
        let start = std::time::Instant::now();
        debug!("write: ino={}, offset={}, size={}", ino, offset, data.len());

        let mut metadata = {
            let cache = self.metadata_cache.read().unwrap();
            match cache.get(&ino) {
                Some(m) => m.clone(),
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        if metadata.file_type != FileType::RegularFile {
            reply.error(libc::EISDIR);
            return;
        }

        // Write-behind buffering: buffer sequential appends in memory
        if self.write_buffer_enabled {
            let offset_usize = offset as usize;
            let current_size = metadata.size as usize;

            // Only buffer sequential appends
            if offset_usize == current_size {
                // Buffer size threshold: 4MB (same as chunk size)
                const BUFFER_FLUSH_THRESHOLD: usize = 4 * 1024 * 1024;

                let write_buffers = self.write_buffers.clone();
                let should_flush = self.block_on(async move {
                    let mut buffers = write_buffers.lock().await;
                    let buffer = buffers.entry(ino).or_insert_with(|| WriteBuffer {
                        data: Vec::new(),
                        last_modified: SystemTime::now(),
                    });

                    // Append data to buffer
                    buffer.data.extend_from_slice(data);
                    buffer.last_modified = SystemTime::now();

                    // Check if buffer exceeds threshold
                    Ok::<bool, anyhow::Error>(buffer.data.len() >= BUFFER_FLUSH_THRESHOLD)
                });

                match should_flush {
                    Ok(flush_now) => {
                        // Update metadata size in cache (but don't persist yet)
                        metadata.size = (current_size + data.len()) as u64;
                        metadata.modified_at = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs();
                        self.metadata_cache.write().unwrap().insert(ino, metadata);

                        // Flush if buffer is too large
                        if flush_now {
                            debug!("Buffer threshold reached, flushing inode {}", ino);
                            if let Err(e) = self.block_on(self.flush_buffer_async(ino)) {
                                error!("Failed to auto-flush buffer: {}", e);
                                reply.error(libc::EIO);
                                return;
                            }
                        }

                        let total_elapsed = start.elapsed();
                        debug!("BUFFERED write() took {:?} for {} bytes ({:.2} MB/s)",
                            total_elapsed, data.len(),
                            (data.len() as f64 / 1024.0 / 1024.0) / total_elapsed.as_secs_f64());
                        reply.written(data.len() as u32);
                        return;
                    }
                    Err(e) => {
                        error!("Failed to buffer write: {}", e);
                        reply.error(libc::EIO);
                        return;
                    }
                }
            }
        }

        // Optimize for sequential writes (appends)
        let client = self.client.clone();
        let offset = offset as usize;
        let current_size = metadata.size as usize;

        let new_data = if offset == current_size {
            // Sequential write/append - just write new data
            // This is the fast path for DVR recordings, dd, etc.
            data.to_vec()
        } else if offset > current_size {
            // Writing past end of file - need to pad with zeros
            let mut padded = vec![0u8; offset - current_size];
            padded.extend_from_slice(data);
            padded
        } else {
            // Random write in middle of file - need read-modify-write
            // This is slow but necessary for correctness
            let existing_data = if !metadata.chunks.is_empty() {
                let chunk_ids = metadata.chunks.clone();
                match self.block_on(async {
                    client.read_data(&chunk_ids).await
                }) {
                    Ok(data) => data,
                    Err(e) => {
                        error!("Failed to read existing data: {}", e);
                        reply.error(libc::EIO);
                        return;
                    }
                }
            } else {
                Vec::new()
            };

            let mut merged = existing_data;
            if offset + data.len() > merged.len() {
                merged.resize(offset + data.len(), 0);
            }
            merged[offset..offset + data.len()].copy_from_slice(data);
            merged
        };

        // Write to cluster (only new/modified data for appends)
        let write_start = std::time::Instant::now();
        let result = if offset == current_size {
            // Append: write just the new data as new chunks
            self.block_on(async {
                client.write_data(&new_data).await
            })
        } else {
            // Rewrite: write entire file
            self.block_on(async {
                client.write_data(&new_data).await
            })
        };
        let write_elapsed = write_start.elapsed();
        debug!("write_data took {:?}", write_elapsed);

        match result {
            Ok(new_chunk_ids) => {
                // Update metadata
                if offset == current_size {
                    // Append: add new chunks to existing list
                    metadata.chunks.extend(new_chunk_ids);
                    metadata.size = current_size as u64 + new_data.len() as u64;
                } else {
                    // Rewrite: replace all chunks
                    metadata.chunks = new_chunk_ids;
                    metadata.size = new_data.len() as u64;
                }
                metadata.modified_at = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                // Batch metadata updates: only update every 10 writes
                let count = {
                    let mut counters = self.write_counters.write().unwrap();
                    let c = counters.entry(ino).or_insert(0);
                    *c += 1;
                    *c
                };
                let should_update = count % 10 == 0;

                if should_update {
                    // Store updated metadata
                    let metadata_start = std::time::Instant::now();
                    let metadata_clone = metadata.clone();
                    let update_result = self.block_on(async {
                        client.put_file_metadata(&metadata_clone).await
                    });
                    let metadata_elapsed = metadata_start.elapsed();
                    debug!("put_file_metadata took {:?} (batched at write #{})", metadata_elapsed, count);

                    match update_result {
                        Ok(_) => {
                            // Update cache
                            self.metadata_cache.write().unwrap().insert(ino, metadata);
                            let total_elapsed = start.elapsed();
                            debug!("TOTAL write() took {:?} for {} bytes ({:.2} MB/s)",
                                total_elapsed, data.len(),
                                (data.len() as f64 / 1024.0 / 1024.0) / total_elapsed.as_secs_f64());
                            reply.written(data.len() as u32);
                        }
                        Err(e) => {
                            error!("Failed to update metadata: {}", e);
                            reply.error(libc::EIO);
                        }
                    }
                } else {
                    // Skip metadata update for this write, just cache locally
                    self.metadata_cache.write().unwrap().insert(ino, metadata);
                    let total_elapsed = start.elapsed();
                    debug!("TOTAL write() took {:?} for {} bytes (metadata skipped) ({:.2} MB/s)",
                        total_elapsed, data.len(),
                        (data.len() as f64 / 1024.0 / 1024.0) / total_elapsed.as_secs_f64());
                    reply.written(data.len() as u32);
                }
            }
            Err(e) => {
                error!("Failed to write data: {}", e);
                reply.error(libc::EIO);
            }
        }
    }

    fn flush(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: fuser::ReplyEmpty,
    ) {
        debug!("flush: ino={}", ino);

        if self.write_buffer_enabled {
            // Flush any buffered writes
            let result = self.block_on(self.flush_buffer_async(ino));

            match result {
                Ok(_) => reply.ok(),
                Err(e) => {
                    error!("Failed to flush buffer for inode {}: {}", ino, e);
                    reply.error(libc::EIO);
                }
            }
        } else {
            reply.ok();
        }
    }

    fn release(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        debug!("release: ino={}", ino);

        if self.write_buffer_enabled {
            // Flush any buffered writes on file close
            let result = self.block_on(self.flush_buffer_async(ino));

            match result {
                Ok(_) => reply.ok(),
                Err(e) => {
                    error!("Failed to flush buffer on release for inode {}: {}", ino, e);
                    reply.error(libc::EIO);
                }
            }
        } else {
            reply.ok();
        }
    }

    fn mkdir(
        &mut self,
        _req: &FuseRequest,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        debug!("mkdir: parent={}, name={:?}, mode={:o}", parent, name, mode);

        let path = match self.get_path_from_parent(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Create directory metadata
        let metadata = FileMetadata {
            id: dfs_common::FileId::new(),
            path: path.clone(),
            size: 0,
            chunks: Vec::new(),
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            modified_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            mode,
            uid: _req.uid(),
            gid: _req.gid(),
            file_type: FileType::Directory,
        };

        // Store metadata on cluster
        let client = self.client.clone();
        let metadata_clone = metadata.clone();
        let result = self.block_on(async {
            client.put_file_metadata(&metadata_clone).await
        });

        match result {
            Ok(_) => {
                // Allocate inode
                let ino = self.get_or_create_inode(&path);

                // Cache metadata
                self.metadata_cache.write().unwrap().insert(ino, metadata.clone());

                // Convert to FUSE attr
                let attr = self.metadata_to_attr(ino, &metadata);
                reply.entry(&Duration::from_secs(3600), &attr, 0);
            }
            Err(e) => {
                error!("Failed to create directory {}: {}", path, e);
                reply.error(libc::EIO);
            }
        }
    }

    fn unlink(&mut self, _req: &FuseRequest, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
        debug!("unlink: parent={}, name={:?}", parent, name);

        let path = match self.get_path_from_parent(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Delete file from cluster
        let client = self.client.clone();
        let result = self.block_on(async {
            client.delete_file(&path).await
        });

        match result {
            Ok(_) => {
                // Remove from cache
                if let Some(&ino) = self.path_to_inode.read().unwrap().get(&path) {
                    self.metadata_cache.write().unwrap().remove(&ino);
                }
                self.path_to_inode.write().unwrap().remove(&path);

                reply.ok();
            }
            Err(e) => {
                error!("Failed to delete file {}: {}", path, e);
                reply.error(libc::EIO);
            }
        }
    }

    fn rmdir(&mut self, _req: &FuseRequest, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
        debug!("rmdir: parent={}, name={:?}", parent, name);

        let path = match self.get_path_from_parent(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Check if directory is empty
        let client = self.client.clone();
        let path_clone = path.clone();
        let result = self.block_on(async {
            client.list_directory(&path_clone).await
        });

        match result {
            Ok(entries) => {
                if !entries.is_empty() {
                    reply.error(libc::ENOTEMPTY);
                    return;
                }

                // Delete directory
                let delete_result = self.block_on(async {
                    client.delete_file(&path).await
                });

                match delete_result {
                    Ok(_) => {
                        // Remove from cache
                        if let Some(&ino) = self.path_to_inode.read().unwrap().get(&path) {
                            self.metadata_cache.write().unwrap().remove(&ino);
                        }
                        self.path_to_inode.write().unwrap().remove(&path);

                        reply.ok();
                    }
                    Err(e) => {
                        error!("Failed to delete directory {}: {}", path, e);
                        reply.error(libc::EIO);
                    }
                }
            }
            Err(e) => {
                error!("Failed to check directory {}: {}", path, e);
                reply.error(libc::EIO);
            }
        }
    }

    fn rename(
        &mut self,
        _req: &FuseRequest,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: fuser::ReplyEmpty,
    ) {
        debug!(
            "rename: parent={}, name={:?} -> newparent={}, newname={:?}",
            parent, name, newparent, newname
        );

        let old_path = match self.get_path_from_parent(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let new_path = match self.get_path_from_parent(newparent, newname) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Get existing metadata
        let client = self.client.clone();
        let old_path_clone = old_path.clone();
        let result = self.block_on(async {
            client.get_file_metadata(&old_path_clone).await
        });

        match result {
            Ok(Some(mut metadata)) => {
                // Update path
                metadata.path = new_path.clone();
                metadata.modified_at = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                // Put new metadata
                let metadata_clone = metadata.clone();
                let put_result = self.block_on(async {
                    client.put_file_metadata(&metadata_clone).await
                });

                match put_result {
                    Ok(_) => {
                        // Delete old metadata
                        let delete_result = self.block_on(async {
                            client.delete_file(&old_path).await
                        });

                        match delete_result {
                            Ok(_) => {
                                // Update cache
                                if let Some(&old_ino) = self.path_to_inode.read().unwrap().get(&old_path) {
                                    self.metadata_cache.write().unwrap().remove(&old_ino);
                                }
                                self.path_to_inode.write().unwrap().remove(&old_path);

                                let new_ino = self.get_or_create_inode(&new_path);
                                self.metadata_cache.write().unwrap().insert(new_ino, metadata);

                                reply.ok();
                            }
                            Err(e) => {
                                error!("Failed to delete old file {}: {}", old_path, e);
                                reply.error(libc::EIO);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to create new file {}: {}", new_path, e);
                        reply.error(libc::EIO);
                    }
                }
            }
            Ok(None) => {
                reply.error(libc::ENOENT);
            }
            Err(e) => {
                error!("Failed to get file metadata {}: {}", old_path, e);
                reply.error(libc::EIO);
            }
        }
    }

    fn setattr(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        debug!("setattr: ino={}, mode={:?}, uid={:?}, gid={:?}, size={:?}",
               ino, mode, uid, gid, size);

        let mut metadata = {
            let cache = self.metadata_cache.read().unwrap();
            match cache.get(&ino) {
                Some(m) => m.clone(),
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        // Update attributes
        if let Some(mode) = mode {
            metadata.mode = mode;
        }
        if let Some(uid) = uid {
            metadata.uid = uid;
        }
        if let Some(gid) = gid {
            metadata.gid = gid;
        }

        // Handle truncate
        if let Some(new_size) = size {
            if new_size != metadata.size {
                let client = self.client.clone();

                // Read existing data
                let existing_data = if !metadata.chunks.is_empty() {
                    let chunk_ids = metadata.chunks.clone();
                    match self.block_on(async {
                        client.read_data(&chunk_ids).await
                    }) {
                        Ok(data) => data,
                        Err(e) => {
                            error!("Failed to read existing data for truncate: {}", e);
                            reply.error(libc::EIO);
                            return;
                        }
                    }
                } else {
                    Vec::new()
                };

                // Resize data
                let mut new_data = existing_data;
                new_data.resize(new_size as usize, 0);

                // Write back
                let result = self.block_on(async {
                    client.write_data(&new_data).await
                });

                match result {
                    Ok(chunk_ids) => {
                        metadata.chunks = chunk_ids;
                        metadata.size = new_size;
                    }
                    Err(e) => {
                        error!("Failed to write truncated data: {}", e);
                        reply.error(libc::EIO);
                        return;
                    }
                }
            }
        }

        metadata.modified_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Store updated metadata
        let client = self.client.clone();
        let metadata_clone = metadata.clone();
        let result = self.block_on(async {
            client.put_file_metadata(&metadata_clone).await
        });

        match result {
            Ok(_) => {
                // Update cache
                self.metadata_cache.write().unwrap().insert(ino, metadata.clone());

                // Convert to FUSE attr
                let attr = self.metadata_to_attr(ino, &metadata);
                reply.attr(&Duration::from_secs(3600), &attr);
            }
            Err(e) => {
                error!("Failed to update attributes: {}", e);
                reply.error(libc::EIO);
            }
        }
    }

    fn statfs(&mut self, _req: &FuseRequest, _ino: u64, reply: ReplyStatfs) {
        debug!("statfs");

        // Query actual storage stats from cluster
        let client = self.client.clone();
        let result = self.block_on(async {
            client.get_storage_stats().await
        });

        const BLOCK_SIZE: u32 = 4096;

        let (total_blocks, free_blocks, avail_blocks) = match result {
            Ok((total_space, free_space, available_space, _replication_factor)) => {
                // Convert bytes to blocks
                let total = total_space / BLOCK_SIZE as u64;
                let free = free_space / BLOCK_SIZE as u64;
                let avail = available_space / BLOCK_SIZE as u64;
                (total, free, avail)
            }
            Err(e) => {
                error!("Failed to get storage stats: {}", e);
                // Return reasonable defaults on error
                (1_000_000_000, 500_000_000, 500_000_000)
            }
        };

        reply.statfs(
            total_blocks,  // blocks - total data blocks in filesystem
            free_blocks,   // bfree - free blocks in filesystem
            avail_blocks,  // bavail - free blocks available to non-privileged user
            0,             // files - total file nodes in filesystem (unlimited)
            0,             // ffree - free file nodes in filesystem (unlimited)
            BLOCK_SIZE,    // bsize - block size
            255,           // namelen - maximum filename length
            BLOCK_SIZE,    // frsize - fragment size
        );
    }

    fn access(&mut self, _req: &FuseRequest, ino: u64, mask: i32, reply: fuser::ReplyEmpty) {
        debug!("access: ino={}, mask={}", ino, mask);

        // Check if inode exists
        let cache = self.metadata_cache.read().unwrap();
        if cache.get(&ino).is_some() {
            // For simplicity, allow all access
            // A real implementation would check permissions based on mask
            reply.ok();
        } else {
            reply.error(libc::ENOENT);
        }
    }

    fn fsync(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        datasync: bool,
        reply: fuser::ReplyEmpty,
    ) {
        debug!("fsync: ino={}, datasync={}", ino, datasync);

        if self.write_buffer_enabled {
            // Flush any buffered writes
            let result = self.block_on(self.flush_buffer_async(ino));

            match result {
                Ok(_) => reply.ok(),
                Err(e) => {
                    error!("Failed to fsync inode {}: {}", ino, e);
                    reply.error(libc::EIO);
                }
            }
        } else {
            // No buffering, data is already synced
            reply.ok();
        }
    }

    fn getxattr(
        &mut self,
        _req: &FuseRequest,
        _ino: u64,
        name: &std::ffi::OsStr,
        size: u32,
        reply: fuser::ReplyXattr,
    ) {
        debug!("getxattr: name={:?}, size={}", name, size);
        // We don't support extended attributes
        reply.error(libc::ENODATA);
    }

    fn listxattr(&mut self, _req: &FuseRequest, _ino: u64, size: u32, reply: fuser::ReplyXattr) {
        debug!("listxattr: size={}", size);
        // We don't support extended attributes, return empty list
        if size == 0 {
            // Query size
            reply.size(0);
        } else {
            // Return empty list
            reply.data(&[]);
        }
    }
}
