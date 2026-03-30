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
use tracing::{debug, error, info, warn};

use crate::client::DfsClient;

/// Buffered write data for a single file
#[derive(Clone)]
struct WriteBuffer {
    /// Buffered data
    data: Vec<u8>,
    /// When this buffer was last modified
    last_modified: SystemTime,
    /// File offset where this buffer starts
    start_offset: u64,
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

    /// Directory listing cache: path -> (entries, timestamp)
    /// Cache directory listings for 5 seconds to avoid repeated scans
    dir_cache: Arc<RwLock<HashMap<String, (Vec<FileMetadata>, std::time::Instant)>>>,
}

impl DfsFilesystem {
    /// Create a new DFS filesystem with an explicit runtime handle
    pub fn new_with_runtime(
        cluster_nodes: Vec<SocketAddr>,
        write_buffer_enabled: bool,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self> {
        let client = Arc::new(DfsClient::new(cluster_nodes)?);

        // Start background task to periodically refresh cluster nodes
        let client_clone = client.clone();
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                if let Err(e) = client_clone.refresh_cluster_nodes().await {
                    tracing::debug!("Failed to refresh cluster nodes: {}", e);
                }
            }
        });

        let metadata_cache = Arc::new(RwLock::new(HashMap::new()));
        let path_to_inode = Arc::new(RwLock::new(HashMap::new()));
        let next_inode = Arc::new(RwLock::new(2)); // Start at 2, root is 1

        // Create root directory metadata
        let root_metadata = FileMetadata {
            id: dfs_common::FileId::new(),
            path: "/".to_string(),
            size: 0,
            chunks: Vec::new(),
            chunk_sizes: Vec::new(),
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
            dir_cache: Arc::new(RwLock::new(HashMap::new())),
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

    /// Safely update metadata cache, checking if there's an active write in progress
    /// Returns true if the metadata was updated, false if skipped due to active write
    fn safe_metadata_update(&self, ino: u64, metadata: FileMetadata) -> bool {
        // Check if there's an active write - either buffered or in-progress
        let (has_buffer, has_counter, current_size) = self.runtime.block_on(async {
            // Check for write buffer
            let has_buf = if self.write_buffer_enabled {
                let buffers = self.write_buffers.lock().await;
                buffers.contains_key(&ino)
            } else {
                false
            };

            // Check write counter (indicates recent writes)
            let counters = self.write_counters.read().unwrap();
            let has_cnt = counters.contains_key(&ino);

            // Get current cached size
            let cache = self.metadata_cache.read().unwrap();
            let cur_size = cache.get(&ino).map(|m| m.size).unwrap_or(0);

            (has_buf, has_cnt, cur_size)
        });

        let has_active_write = has_buffer || has_counter;

        if has_active_write {
            debug!("SKIPPING metadata update for ino={}: has_buffer={}, has_counter={}, cached_size={}, server_size={}",
                  ino, has_buffer, has_counter, current_size, metadata.size);
            false
        } else {
            debug!("UPDATING metadata for ino={}: cached_size={}, server_size={}",
                  ino, current_size, metadata.size);
            self.metadata_cache.write().unwrap().insert(ino, metadata);
            true
        }
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

            // Get current metadata from cache
            // NOTE: metadata.size has already been updated by buffered writes
            // We only need to add the chunks for the buffered data
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
            // Use the buffer's recorded start offset
            let buffer_start_offset = buffer.start_offset;

            // Use write-through caching to populate byte-range cache for immediate read-back
            let (new_chunk_ids, new_chunk_sizes) = self.client
                .write_data_with_cache(&buffer.data, ino, buffer_start_offset)
                .await?;

            // Append new chunks to existing chunks and update size
            let num_chunks = new_chunk_ids.len();
            let new_size = buffer_start_offset + buffer.data.len() as u64;

            metadata.chunks.extend(new_chunk_ids);
            metadata.chunk_sizes.extend(new_chunk_sizes);
            metadata.size = new_size;
            metadata.modified_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            info!("Flush complete: {} chunks added at offset {}, total file size {}",
                  num_chunks, buffer_start_offset, new_size);

            // Store updated metadata
            self.client.put_file_metadata(&metadata).await?;

            // Update cache
            self.metadata_cache.write().unwrap().insert(ino, metadata);
        }

        Ok(())
    }

    /// Convert FileMetadata to FUSE FileAttr
    fn metadata_to_attr(&self, ino: u64, metadata: &FileMetadata) -> FileAttr {
        Self::metadata_to_attr_static(ino, metadata)
    }

    /// Convert FileMetadata to FUSE FileAttr (static version for async contexts)
    fn metadata_to_attr_static(ino: u64, metadata: &FileMetadata) -> FileAttr {
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

        // Check cache first and validate freshness
        let cached_modified_at = {
            let path_map = self.path_to_inode.read().unwrap();
            if let Some(&ino) = path_map.get(&path) {
                let cache = self.metadata_cache.read().unwrap();
                cache.get(&ino).map(|m| m.modified_at)
            } else {
                None
            }
        };

        // Fetch from cluster with conditional GET if we have cached metadata
        let client = self.client.clone();
        let result = self.block_on(async {
            client.get_file_metadata_conditional(&path, cached_modified_at).await
        });

        match result {
            Ok(Some(metadata)) => {
                // Metadata was modified or first fetch - update cache
                let ino = self.get_or_create_inode(&path);
                self.safe_metadata_update(ino, metadata.clone());

                let attr = self.metadata_to_attr(ino, &metadata);
                reply.entry(&Duration::from_secs(3600), &attr, 0);
            }
            Ok(None) => {
                // Either file not found OR metadata not modified (cache still valid)
                if cached_modified_at.is_some() {
                    // Cache is valid, use cached metadata
                    let path_map = self.path_to_inode.read().unwrap();
                    if let Some(&ino) = path_map.get(&path) {
                        let cache = self.metadata_cache.read().unwrap();
                        if let Some(metadata) = cache.get(&ino) {
                            debug!("Using cached metadata for {} (not modified)", path);
                            let attr = self.metadata_to_attr(ino, metadata);
                            reply.entry(&Duration::from_secs(3600), &attr, 0);
                            return;
                        }
                    }
                }
                // File not found
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

        // Clone what we need for async operation
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let runtime = self.runtime.clone();

        // Spawn async task to potentially refresh metadata
        runtime.spawn(async move {
            let metadata = {
                let cache = metadata_cache.read().unwrap();
                cache.get(&ino).cloned()
            };

            if let Some(mut metadata) = metadata {
                // For regular files, try to refresh metadata if it might be stale
                // This allows players to see files growing in real-time
                if metadata.file_type == FileType::RegularFile {
                    // Try to get fresh metadata from server
                    if let Ok(Some(fresh)) = client.get_file_metadata(&metadata.path).await {
                        if fresh.size != metadata.size {
                            debug!("getattr: file grew from {} to {} bytes", metadata.size, fresh.size);
                            metadata_cache.write().unwrap().insert(ino, fresh.clone());

                            // Don't warm replica cache here - getattr() is called frequently (every 1s)
                            // and we don't know the read position. Let read() warm the cache when needed.

                            metadata = fresh;
                        }
                    }
                }

                let attr = DfsFilesystem::metadata_to_attr_static(ino, &metadata);
                // Use very short TTL (1 second) so kernel asks us frequently for growing files
                reply.attr(&Duration::from_secs(1), &attr);
            } else {
                reply.error(libc::ENOENT);
            }
        });
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
        // Clone Arc-wrapped fields for async task
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let write_buffers = self.write_buffers.clone();
        let write_buffer_enabled = self.write_buffer_enabled;

        // Spawn async read operation on tokio runtime
        self.runtime.spawn(async move {
            let start = std::time::Instant::now();
            info!("FUSE read START: ino={}, offset={}, size={}", ino, offset, size);

            let mut metadata = {
                let cache = metadata_cache.read().unwrap();
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

            // Check write buffer first if write-behind buffering is enabled
            // This enables "live rewind" - reading while writing without waiting for backend
            if write_buffer_enabled {
                let write_buffers_lock = write_buffers.lock().await;
                if let Some(buffer) = write_buffers_lock.get(&ino) {
                    let buffer_start = buffer.start_offset as usize;
                    let buffer_end = buffer_start + buffer.data.len();

                    // Check if read is entirely within the write buffer range
                    if offset >= buffer_start && offset < buffer_end {
                        let buffer_relative_offset = offset - buffer_start;
                        let end_offset = std::cmp::min(buffer_relative_offset + size, buffer.data.len());
                        let data = buffer.data[buffer_relative_offset..end_offset].to_vec();

                        info!("FUSE read from write buffer: ino={}, offset={}, size={}, buffer_hit={} bytes, buffer_range=[{}, {})",
                              ino, offset, size, data.len(), buffer_start, buffer_end);

                        let elapsed = start.elapsed();
                        info!("FUSE read COMPLETE (write buffer): ino={}, offset={}, size={}, took {:?}",
                              ino, offset, size, elapsed);

                        reply.data(&data);
                        return;
                    }
                }
            }

            // Early return for out of bounds
            // But first, check if file might have grown by refreshing metadata
            if offset >= metadata.size as usize {
                // File might be actively growing (e.g., recording in progress)
                // Refresh metadata from server to see if more data is available
                info!("Read at offset {} >= cached size {}, refreshing metadata from server", offset, metadata.size);

                match client.get_file_metadata(&metadata.path).await {
                    Ok(Some(fresh_metadata)) => {
                        // Update cache with fresh metadata
                        metadata_cache.write().unwrap().insert(ino, fresh_metadata.clone());

                        // Warm replica cache with chunk locations for upcoming reads
                        // Only warm chunks ahead of current read position (smart warming)
                        client.warm_replica_cache_range(&fresh_metadata.chunks, Some(offset as u64), 2 * 1024 * 1024).await;

                        // If file has grown, continue with the read using fresh metadata
                        if offset < fresh_metadata.size as usize {
                            info!("File grew from {} to {} bytes, continuing read", metadata.size, fresh_metadata.size);
                            metadata = fresh_metadata;
                        } else {
                            // Still at EOF even after refresh
                            info!("Still at EOF after refresh: offset {} >= size {}", offset, fresh_metadata.size);
                            reply.data(&[]);
                            return;
                        }
                    }
                    Ok(None) => {
                        // File was deleted
                        info!("File not found when refreshing metadata");
                        reply.error(libc::ENOENT);
                        return;
                    }
                    Err(e) => {
                        // Couldn't refresh, assume EOF
                        info!("Failed to refresh metadata: {}, assuming EOF", e);
                        reply.data(&[]);
                        return;
                    }
                }
            }

            if metadata.chunks.is_empty() {
                reply.data(&[]);
                return;
            }

            // Build chunk offset map for efficient lookups using chunk_sizes
            let mut chunk_offsets = Vec::with_capacity(metadata.chunks.len());
            let mut current_offset = 0usize;

            for (idx, &chunk_size) in metadata.chunk_sizes.iter().enumerate() {
                chunk_offsets.push((current_offset, chunk_size as usize));
                current_offset += chunk_size as usize;
            }

            // Find which chunks we need to read
            let end_offset = std::cmp::min(offset + size, metadata.size as usize);
            let mut chunks_to_read = Vec::new();
            let mut first_chunk_offset = 0usize;

            for (idx, &(chunk_start, chunk_size)) in chunk_offsets.iter().enumerate() {
                let chunk_end = chunk_start + chunk_size;

                // Check if this chunk overlaps with requested range
                if chunk_end > offset && chunk_start < end_offset {
                    chunks_to_read.push((idx, chunk_start, chunk_size));
                    if chunks_to_read.len() == 1 {
                        first_chunk_offset = chunk_start;
                    }
                }

                // Stop once we've found all needed chunks
                if chunk_start >= end_offset {
                    break;
                }
            }

            if chunks_to_read.is_empty() {
                reply.data(&[]);
                return;
            }

            debug!("Reading {} chunks (indices {:?}) for offset {} size {}",
                   chunks_to_read.len(),
                   chunks_to_read.iter().map(|(idx, _, _)| idx).collect::<Vec<_>>(),
                   offset, size);

            // Read only the needed chunks in one batch
            let chunk_ids: Vec<ChunkId> = chunks_to_read
                .iter()
                .map(|(idx, _, _)| metadata.chunks[*idx])
                .collect();

            // Get the file chunk index of the first chunk we're reading (for prefetch tracking)
            let start_chunk_idx = chunks_to_read.first().map(|(idx, _, _)| *idx).unwrap_or(0);

            // Build chunk file offsets for byte-range caching
            let chunk_file_offsets: Vec<u64> = chunks_to_read
                .iter()
                .map(|(_, chunk_start, _)| *chunk_start as u64)
                .collect();

            let all_chunks = metadata.chunks.clone();
            let result = client.read_data(&chunk_ids, &all_chunks, start_chunk_idx, ino, &chunk_file_offsets).await;

            let all_data = match result {
                Ok(data) => data,
                Err(e) => {
                    error!("Failed to read {} chunks: {}", chunk_ids.len(), e);
                    reply.error(libc::EIO);
                    return;
                }
            };

            // Calculate offset within the read data
            let offset_in_data = offset.saturating_sub(first_chunk_offset);
            let data_end = std::cmp::min(offset_in_data + size, all_data.len());

            if offset_in_data >= all_data.len() {
                debug!("Read offset {} beyond data length {}", offset_in_data, all_data.len());
                reply.data(&[]);
            } else {
                debug!("Returning {} bytes from offset {} (read {} chunks, total {} bytes)",
                       data_end - offset_in_data, offset, chunk_ids.len(), all_data.len());
                reply.data(&all_data[offset_in_data..data_end]);
            }

            let elapsed = start.elapsed();
            info!("FUSE read COMPLETE: ino={}, offset={}, size={}, took {:?}",
                  ino, offset, size, elapsed);
        });
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
        let start = std::time::Instant::now();

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

        // Check directory cache first (5-second TTL)
        let cached_entries = {
            let cache = self.dir_cache.read().unwrap();
            cache.get(&path).and_then(|(entries, timestamp)| {
                if timestamp.elapsed() < std::time::Duration::from_secs(5) {
                    debug!("Directory cache HIT for {}", path);
                    Some(entries.clone())
                } else {
                    debug!("Directory cache EXPIRED for {}", path);
                    None
                }
            })
        };

        let entries = if let Some(entries) = cached_entries {
            entries
        } else {
            // Cache miss - fetch from server
            debug!("Directory cache MISS for {}", path);
            let client = self.client.clone();
            let dir_cache = self.dir_cache.clone();
            let path_clone = path.clone();

            let result = self.block_on(async {
                client.list_directory(&path_clone).await
            });

            match result {
                Ok(entries) => {
                    // Update cache
                    dir_cache.write().unwrap().insert(path.clone(), (entries.clone(), std::time::Instant::now()));
                    entries
                }
                Err(e) => {
                    error!("Failed to read directory {}: {}", path, e);
                    reply.error(libc::EIO);
                    return;
                }
            }
        };

        // Add . and ..
        if offset == 0 {
            if reply.add(ino, 1, FuseFileType::Directory, ".") {
                reply.ok();
                return;
            }
        }
        if offset <= 1 {
            if reply.add(ino, 2, FuseFileType::Directory, "..") {
                reply.ok();
                return;
            }
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

            // For directory listings, always use fresh server data without lock contention
            // safe_metadata_update() acquires write_buffers lock for EVERY file, causing
            // massive contention during writes. Directory listing should be fast and non-blocking.
            // The metadata comes directly from the server, so it's already authoritative.
            self.metadata_cache.write().unwrap().insert(entry_ino, entry.clone());

            let next_offset = 3 + i as i64;  // 3 because . is 1, .. is 2, first file is 3
            if reply.add(entry_ino, next_offset, kind, file_name) {
                break; // Buffer full
            }
        }

        let elapsed = start.elapsed();
        info!("readdir COMPLETE: {} with {} entries in {:?}", path, entries.len(), elapsed);
        reply.ok();
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
            chunk_sizes: Vec::new(),
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
        // Clone Arc-wrapped fields for thread pool
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let write_counters = self.write_counters.clone();
        let write_buffers = self.write_buffers.clone();
        let write_buffer_enabled = self.write_buffer_enabled;
        let runtime = self.runtime.clone();
        let data_vec = data.to_vec(); // Copy data before moving to thread

        // Spawn write operation on tokio's blocking thread pool
        runtime.clone().spawn_blocking(move || {
            let start = std::time::Instant::now();
            debug!("write: ino={}, offset={}, size={}", ino, offset, data_vec.len());

            let mut metadata = {
                let cache = metadata_cache.read().unwrap();
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
            if write_buffer_enabled {
                let offset_usize = offset as usize;
                // Calculate true current size including any buffered data
                // IMPORTANT: Re-read metadata from cache to get latest updates from concurrent writes
                let current_size = {
                    let cache_size = {
                        let cache = metadata_cache.read().unwrap();
                        cache.get(&ino).map(|m| m.size as usize).unwrap_or(metadata.size as usize)
                    };

                    let buffers_guard = runtime.block_on(async {
                        write_buffers.lock().await
                    });
                    if let Some(buffer) = buffers_guard.get(&ino) {
                        ((buffer.start_offset + buffer.data.len() as u64) as usize)
                            .max(cache_size)
                    } else {
                        cache_size
                    }
                };

                info!("Buffered write check: offset={}, current_size={}, cache_size={}, buffer_present={}",
                       offset_usize, current_size,
                       {let cache = metadata_cache.read().unwrap(); cache.get(&ino).map(|m| m.size).unwrap_or(0)},
                       {let bg = runtime.block_on(async { write_buffers.lock().await }); bg.get(&ino).is_some()});

                // Only buffer sequential appends
                if offset_usize != current_size {
                    info!("Buffered write skipped: offset {} != current_size {} (diff: {})",
                           offset_usize, current_size,
                           if offset_usize > current_size {
                               offset_usize - current_size
                           } else {
                               current_size - offset_usize
                           });
                }
                if offset_usize == current_size {
                    // Buffer size threshold: 4MB (same as chunk size)
                    const BUFFER_FLUSH_THRESHOLD: usize = 4 * 1024 * 1024;

                    let write_buffers_clone = write_buffers.clone();
                    let client_clone = client.clone();
                    let metadata_cache_clone = metadata_cache.clone();
                    let metadata_cache_clone2 = metadata_cache.clone();
                    let metadata_cache_clone3 = metadata_cache.clone();
                    let runtime_clone = runtime.clone();
                    let data_slice = &data_vec[..];

                    let should_flush = runtime.block_on(async move {
                        let mut buffers = write_buffers_clone.lock().await;

                        // Recalculate current_size while holding the buffer lock to avoid races
                        let actual_current_size = {
                            let cache_size = {
                                let cache = metadata_cache_clone3.read().unwrap();
                                cache.get(&ino).map(|m| m.size as u64).unwrap_or(current_size as u64)
                            };

                            if let Some(existing_buffer) = buffers.get(&ino) {
                                (existing_buffer.start_offset + existing_buffer.data.len() as u64).max(cache_size)
                            } else {
                                cache_size
                            }
                        };

                        let buffer = buffers.entry(ino).or_insert_with(|| WriteBuffer {
                            data: Vec::new(),
                            last_modified: SystemTime::now(),
                            start_offset: actual_current_size,
                        });

                        // Safety check: if buffer is already way too large, something is wrong
                        // This can happen if writes backed up during lock contention
                        const MAX_BUFFER_SIZE: usize = BUFFER_FLUSH_THRESHOLD * 3; // 12MB absolute max
                        if buffer.data.len() > MAX_BUFFER_SIZE {
                            error!("Buffer overflow detected for inode {}: {} bytes (max {}), refusing to buffer more data",
                                   ino, buffer.data.len(), MAX_BUFFER_SIZE);
                            return Err(anyhow::anyhow!("Buffer overflow"));
                        }

                        // Append data to buffer
                        buffer.data.extend_from_slice(data_slice);
                        buffer.last_modified = SystemTime::now();

                        // Check if buffer exceeds threshold
                        Ok::<bool, anyhow::Error>(buffer.data.len() >= BUFFER_FLUSH_THRESHOLD)
                    });

                    match should_flush {
                        Ok(flush_now) => {
                            // Don't update metadata.size here - it will be updated during flush
                            // The buffer already contains the data, so current_size calculation
                            // will account for it via buffer.start_offset + buffer.data.len()

                            // Update write counter to protect metadata from stale server fetches
                            {
                                let mut counters = write_counters.write().unwrap();
                                let c = counters.entry(ino).or_insert(0);
                                *c += 1;
                            }

                            // Flush if buffer is too large (BLOCKING to avoid gaps)
                            if flush_now {
                                debug!("Buffer threshold reached, flushing inode {}", ino);

                                // Blocking flush
                                let flush_result = runtime_clone.block_on(async {
                                    // Clone the buffer data for flushing, but keep buffer for reads
                                    // This prevents read gaps during the flush operation
                                    let buffer_data: Option<(Vec<u8>, u64)> = {
                                        let mut buffers = write_buffers.lock().await;
                                        if let Some(buffer) = buffers.get_mut(&ino) {
                                            let data: Vec<u8> = buffer.data.clone();
                                            let start: u64 = buffer.start_offset;
                                            // Clear buffer for new writes, update start offset
                                            let new_start = start + data.len() as u64;
                                            buffer.data.clear();
                                            buffer.start_offset = new_start;
                                            buffer.last_modified = SystemTime::now();
                                            Some((data, start))
                                        } else {
                                            None
                                        }
                                    };

                                    if let Some((data, buffer_start_offset)) = buffer_data {
                                        info!("Flushing {} bytes for inode {}", data.len(), ino);

                                        // Get current metadata from cache
                                        let mut flush_metadata = {
                                            let cache = metadata_cache_clone.read().unwrap();
                                            match cache.get(&ino) {
                                                Some(m) => m.clone(),
                                                None => {
                                                    return Err(anyhow::anyhow!("Metadata not found for inode {}", ino));
                                                }
                                            }
                                        };

                                        // Write buffered data as new chunks with caching
                                        let (new_chunk_ids, new_chunk_sizes) = client_clone
                                            .write_data_with_cache(&data, ino, buffer_start_offset)
                                            .await?;

                                        // Calculate new size before moving chunk data
                                        let new_size = buffer_start_offset + data.len() as u64;
                                        let num_chunks = new_chunk_ids.len();

                                        // Append new chunks to existing chunks and update size
                                        flush_metadata.chunks.extend(new_chunk_ids);
                                        flush_metadata.chunk_sizes.extend(new_chunk_sizes);
                                        flush_metadata.size = new_size;
                                        flush_metadata.modified_at = SystemTime::now()
                                            .duration_since(UNIX_EPOCH)
                                            .unwrap()
                                            .as_secs();

                                        info!("Flush complete: {} chunks added at offset {}, total file size {}",
                                              num_chunks, buffer_start_offset, new_size);

                                        // Store updated metadata
                                        client_clone.put_file_metadata(&flush_metadata).await?;

                                        // Update cache
                                        metadata_cache_clone.write().unwrap().insert(ino, flush_metadata);
                                    }

                                    Ok::<(), anyhow::Error>(())
                                });

                                if let Err(e) = flush_result {
                                    error!("Failed to auto-flush buffer: {}", e);
                                    reply.error(libc::EIO);
                                    return;
                                }
                            }

                            let total_elapsed = start.elapsed();
                            debug!("BUFFERED write() took {:?} for {} bytes ({:.2} MB/s)",
                                total_elapsed, data_vec.len(),
                                (data_vec.len() as f64 / 1024.0 / 1024.0) / total_elapsed.as_secs_f64());
                            reply.written(data_vec.len() as u32);
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
            let offset = offset as usize;
            // Calculate true current size including any buffered data
            // IMPORTANT: Re-read metadata from cache to get latest updates
            let current_size = if write_buffer_enabled {
                let cache_size = {
                    let cache = metadata_cache.read().unwrap();
                    cache.get(&ino).map(|m| m.size as usize).unwrap_or(metadata.size as usize)
                };

                let buffer_end = runtime.block_on(async {
                    let buffers = write_buffers.lock().await;
                    if let Some(buffer) = buffers.get(&ino) {
                        (buffer.start_offset + buffer.data.len() as u64) as usize
                    } else {
                        cache_size
                    }
                });
                buffer_end.max(cache_size)
            } else {
                metadata.size as usize
            };

            let (new_data, is_append) = if offset == current_size {
                // Sequential write/append - just write new data
                // This is the fast path for DVR recordings, dd, etc.
                (data_vec.clone(), true)
            } else if offset > current_size {
                // Writing past end of file - need to pad with zeros
                let mut padded = vec![0u8; offset - current_size];
                padded.extend_from_slice(&data_vec);
                (padded, true)
            } else if offset + data_vec.len() >= current_size {
                // Writing near end of file (overlaps with current end)
                // Treat as append to avoid expensive read-modify-write
                // This handles DVR recordings and other streaming writes where
                // small timing variations might cause writes slightly behind the end
                debug!("Write at offset {} overlaps with end at {}, treating as append",
                       offset, current_size);
                (data_vec.clone(), true)
            } else {
                // Random write in middle of file - need read-modify-write
                // This is slow but necessary for correctness
                // True random write - need full read-modify-write
                let existing_data = if !metadata.chunks.is_empty() {
                        let chunk_ids = metadata.chunks.clone();
                        let chunk_sizes = metadata.chunk_sizes.clone();

                        // Build chunk offsets for byte-range caching
                        let mut chunk_offsets = Vec::with_capacity(chunk_ids.len());
                        let mut current_offset = 0u64;
                        for &size in &chunk_sizes {
                            chunk_offsets.push(current_offset);
                            current_offset += size;
                        }

                        match runtime.block_on(async {
                            // Reading entire file, so start_chunk_idx=0
                            // Pass actual inode and offsets for proper byte-range caching
                            client.read_data(&chunk_ids, &chunk_ids, 0, ino, &chunk_offsets).await
                        }) {
                            Ok(data) => data,
                            Err(e) => {
                                error!("Failed to read existing data for random write at offset {} (file size {}): {}",
                                       offset, current_size, e);
                                reply.error(libc::EIO);
                                return;
                            }
                        }
                    } else {
                        Vec::new()
                    };

                let mut merged = existing_data;
                if offset + data_vec.len() > merged.len() {
                    merged.resize(offset + data_vec.len(), 0);
                }
                merged[offset..offset + data_vec.len()].copy_from_slice(&data_vec);
                (merged, false)
            };

            // Write to cluster (only new/modified data for appends)
            // Use write_data_with_cache to populate byte-range cache for immediate read-back
            let write_start = std::time::Instant::now();
            let result = if is_append {
                // Append: write just the new data as new chunks
                runtime.block_on(async {
                    // Pass file offset for cache population (write-through caching)
                    client.write_data_with_cache(&new_data, ino, current_size as u64).await
                })
            } else {
                // Rewrite: write entire file starting at offset 0
                runtime.block_on(async {
                    client.write_data_with_cache(&new_data, ino, 0).await
                })
            };
            let write_elapsed = write_start.elapsed();
            debug!("write_data took {:?}", write_elapsed);

            match result {
                Ok((new_chunk_ids, new_chunk_sizes)) => {
                    // Update metadata
                    if is_append {
                        // Append: add new chunks to existing list
                        metadata.chunks.extend(new_chunk_ids);
                        metadata.chunk_sizes.extend(new_chunk_sizes);
                        metadata.size = current_size as u64 + new_data.len() as u64;
                    } else {
                        // Rewrite: replace all chunks
                        metadata.chunks = new_chunk_ids;
                        metadata.chunk_sizes = new_chunk_sizes;
                        metadata.size = new_data.len() as u64;
                    }
                    metadata.modified_at = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs();

                    // Batch metadata updates: only update every 10 writes
                    let count = {
                        let mut counters = write_counters.write().unwrap();
                        let c = counters.entry(ino).or_insert(0);
                        *c += 1;
                        *c
                    };
                    let should_update = count % 10 == 0;

                    if should_update {
                        // Store updated metadata
                        let metadata_start = std::time::Instant::now();
                        let metadata_clone = metadata.clone();
                        let update_result = runtime.block_on(async {
                            client.put_file_metadata(&metadata_clone).await
                        });
                        let metadata_elapsed = metadata_start.elapsed();
                        debug!("put_file_metadata took {:?} (batched at write #{})", metadata_elapsed, count);

                        match update_result {
                            Ok(_) => {
                                // Update cache
                                metadata_cache.write().unwrap().insert(ino, metadata);
                                let total_elapsed = start.elapsed();
                                debug!("TOTAL write() took {:?} for {} bytes ({:.2} MB/s)",
                                    total_elapsed, data_vec.len(),
                                    (data_vec.len() as f64 / 1024.0 / 1024.0) / total_elapsed.as_secs_f64());
                                reply.written(data_vec.len() as u32);
                            }
                            Err(e) => {
                                error!("Failed to update metadata: {}", e);
                                reply.error(libc::EIO);
                            }
                        }
                    } else {
                        // Skip metadata update for this write, just cache locally
                        metadata_cache.write().unwrap().insert(ino, metadata);
                        let total_elapsed = start.elapsed();
                        debug!("TOTAL write() took {:?} for {} bytes (metadata skipped) ({:.2} MB/s)",
                            total_elapsed, data_vec.len(),
                            (data_vec.len() as f64 / 1024.0 / 1024.0) / total_elapsed.as_secs_f64());
                        reply.written(data_vec.len() as u32);
                    }
                }
                Err(e) => {
                    error!("Failed to write data: {}", e);
                    reply.error(libc::EIO);
                }
            }
        });
    }

    fn flush(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: fuser::ReplyEmpty,
    ) {
        // Clone Arc-wrapped fields for thread pool
        let write_buffer_enabled = self.write_buffer_enabled;
        let write_buffers = self.write_buffers.clone();
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let runtime = self.runtime.clone();

        // Spawn flush operation on tokio's blocking thread pool
        runtime.clone().spawn_blocking(move || {
            debug!("flush: ino={}", ino);

            if write_buffer_enabled {
                // Inline flush_buffer_async logic
                let result = runtime.block_on(async {
                    // Get and remove buffer for this inode
                    let buffer_opt = {
                        let mut buffers = write_buffers.lock().await;
                        buffers.remove(&ino)
                    };

                    if let Some(buffer) = buffer_opt {
                        info!("Flushing {} bytes for inode {}", buffer.data.len(), ino);

                        // Get current metadata from cache
                        let mut flush_metadata = {
                            let cache = metadata_cache.read().unwrap();
                            match cache.get(&ino) {
                                Some(m) => m.clone(),
                                None => {
                                    return Err(anyhow::anyhow!("Metadata not found for inode {}", ino));
                                }
                            }
                        };

                        // Write buffered data as new chunks with caching
                        // Use the buffer's recorded start offset
                        let buffer_start_offset = buffer.start_offset;
                        let (new_chunk_ids, new_chunk_sizes) = client
                            .write_data_with_cache(&buffer.data, ino, buffer_start_offset)
                            .await?;

                        // Calculate new size and save chunk count before moving
                        let num_chunks = new_chunk_ids.len();
                        let new_size = buffer_start_offset + buffer.data.len() as u64;

                        // Append new chunks to existing chunks and update size
                        flush_metadata.chunks.extend(new_chunk_ids);
                        flush_metadata.chunk_sizes.extend(new_chunk_sizes);
                        flush_metadata.size = new_size;
                        flush_metadata.modified_at = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs();

                        info!("Flush complete: {} chunks added at offset {}, total file size {}",
                              num_chunks, buffer_start_offset, new_size);

                        // Store updated metadata
                        client.put_file_metadata(&flush_metadata).await?;

                        // Update cache
                        metadata_cache.write().unwrap().insert(ino, flush_metadata);
                    }

                    Ok::<(), anyhow::Error>(())
                });

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
        });
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
            chunk_sizes: Vec::new(),
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
                // CRITICAL: Rename must preserve chunks - only update metadata path
                // The old delete_file() call was DELETING ALL CHUNKS - major data loss bug!

                // Update path and timestamp
                metadata.path = new_path.clone();
                metadata.modified_at = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                // Put new metadata (this creates new path index entry)
                let metadata_clone = metadata.clone();
                let file_id = metadata.id;
                let put_result = self.block_on(async {
                    client.put_file_metadata(&metadata_clone).await
                });

                match put_result {
                    Ok(_) => {
                        // Delete ONLY the old metadata entry (purge), NOT the chunks
                        // Use purge_file_metadata which only removes metadata, not chunks
                        let delete_result = self.block_on(async {
                            client.purge_file_metadata(&old_path).await
                        });

                        match delete_result {
                            Ok(_) => {
                                // Update local cache
                                if let Some(&old_ino) = self.path_to_inode.read().unwrap().get(&old_path) {
                                    self.metadata_cache.write().unwrap().remove(&old_ino);
                                }
                                self.path_to_inode.write().unwrap().remove(&old_path);

                                let new_ino = self.get_or_create_inode(&new_path);
                                self.metadata_cache.write().unwrap().insert(new_ino, metadata);

                                // Invalidate directory cache for both old and new parent directories
                                let old_parent = old_path.rsplitn(2, '/').nth(1).unwrap_or("/");
                                let new_parent = new_path.rsplitn(2, '/').nth(1).unwrap_or("/");
                                self.dir_cache.write().unwrap().remove(old_parent);
                                if old_parent != new_parent {
                                    self.dir_cache.write().unwrap().remove(new_parent);
                                }

                                info!("Renamed {} -> {} (preserved {} chunks)", old_path, new_path, metadata_clone.chunks.len());
                                reply.ok();
                            }
                            Err(e) => {
                                error!("Failed to purge old metadata for {}: {}", old_path, e);
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

                // Optimization: truncate to zero doesn't need to read old data
                // This fixes overwrite scenarios (dd, DVR recording restart) where old chunks
                // may be unavailable due to garbage collection or incomplete replication
                if new_size == 0 {
                    // Just clear the metadata - no need to read old chunks
                    metadata.chunks = Vec::new();
                    metadata.chunk_sizes = Vec::new();
                    metadata.size = 0;
                } else {
                    // Read existing data for partial truncate
                    let existing_data = if !metadata.chunks.is_empty() {
                        let chunk_ids = metadata.chunks.clone();
                        let chunk_sizes = metadata.chunk_sizes.clone();

                        // Build chunk offsets for byte-range caching
                        let mut chunk_offsets = Vec::with_capacity(chunk_ids.len());
                        let mut current_offset = 0u64;
                        for &size in &chunk_sizes {
                            chunk_offsets.push(current_offset);
                            current_offset += size;
                        }

                        match self.block_on(async {
                            // Reading entire file for truncate, start_chunk_idx=0
                            // Pass actual inode and offsets for proper byte-range caching
                            client.read_data(&chunk_ids, &chunk_ids, 0, ino, &chunk_offsets).await
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
                        Ok((chunk_ids, chunk_sizes)) => {
                            metadata.chunks = chunk_ids;
                            metadata.chunk_sizes = chunk_sizes;
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
