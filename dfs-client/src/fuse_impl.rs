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
use crate::locks::LockManager;

/// Buffered write data for a single file
#[derive(Clone)]
struct WriteBuffer {
    /// Buffered data
    data: Vec<u8>,
    /// When this buffer was last modified
    last_modified: SystemTime,
    /// File offset where this buffer starts
    start_offset: u64,
    /// When this buffer was created
    created_at: std::time::Instant,
}

impl WriteBuffer {
    /// Check if this buffer has been idle (no writes) for 5 seconds
    /// Active buffers (continuous DVR recording) should NOT be flushed
    fn is_idle(&self) -> bool {
        // Check time since LAST write, not creation time
        // This prevents flushing active DVR recordings that write continuously
        if let Ok(elapsed) = self.last_modified.elapsed() {
            elapsed > std::time::Duration::from_secs(5)
        } else {
            false // If we can't get elapsed time, don't consider it idle
        }
    }
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

    /// Filesystem stats cache: (total, free, avail, timestamp)
    /// Cache statfs results for 30 seconds to avoid repeated expensive queries
    statfs_cache: Arc<RwLock<Option<(u64, u64, u64, std::time::Instant)>>>,

    /// Lock manager for byte-range locks
    lock_manager: Arc<LockManager>,
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

        let metadata_cache = Arc::new(RwLock::new(HashMap::<u64, FileMetadata>::new()));
        let path_to_inode = Arc::new(RwLock::new(HashMap::<String, u64>::new()));
        let next_inode = Arc::new(RwLock::new(2)); // Start at 2, root is 1
        let write_buffers_for_cleanup = Arc::new(Mutex::new(HashMap::<u64, WriteBuffer>::new()));

        // Start background task to flush expired write buffers (if buffering enabled)
        if write_buffer_enabled {
            let write_buffers_clone = write_buffers_for_cleanup.clone();
            let client_for_cleanup = client.clone();
            let metadata_cache_for_cleanup = metadata_cache.clone();
            runtime.spawn(async move {
                let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(2));
                loop {
                    interval.tick().await;

                    // Find idle buffers (no writes for 5+ seconds)
                    let idle_inodes: Vec<u64> = {
                        let buffers = write_buffers_clone.lock().await;
                        buffers.iter()
                            .filter(|(_, buf)| buf.is_idle())
                            .map(|(ino, _)| *ino)
                            .collect()
                    };

                    // Flush each idle buffer
                    for ino in idle_inodes {
                        let buffer_opt = {
                            let mut buffers = write_buffers_clone.lock().await;
                            buffers.remove(&ino)
                        };

                        if let Some(buffer) = buffer_opt {
                            let idle_time = buffer.last_modified.elapsed().unwrap_or(std::time::Duration::from_secs(0));
                            info!("Background flush: idle write buffer for inode {} ({} bytes, idle: {:?})",
                                  ino, buffer.data.len(), idle_time);

                            // Get metadata
                            let mut metadata = {
                                let cache = metadata_cache_for_cleanup.read().unwrap();
                                match cache.get(&ino) {
                                    Some(m) => m.clone(),
                                    None => {
                                        tracing::warn!("Metadata not found for inode {} during background flush", ino);
                                        continue;
                                    }
                                }
                            };

                            // Write buffered data
                            match client_for_cleanup.write_data_with_cache(&buffer.data, ino, buffer.start_offset).await {
                                Ok((new_chunk_ids, new_chunk_sizes)) => {
                                    let new_size = buffer.start_offset + buffer.data.len() as u64;
                                    metadata.chunks.extend(new_chunk_ids);
                                    metadata.chunk_sizes.extend(new_chunk_sizes);
                                    metadata.size = new_size;
                                    metadata.modified_at = SystemTime::now()
                                        .duration_since(UNIX_EPOCH)
                                        .unwrap()
                                        .as_secs();

                                    // Store updated metadata
                                    if let Err(e) = client_for_cleanup.put_file_metadata(&metadata).await {
                                        tracing::error!("Failed to store metadata during background flush for inode {}: {}", ino, e);
                                    } else {
                                        metadata_cache_for_cleanup.write().unwrap().insert(ino, metadata);
                                        info!("Background flush complete for inode {}", ino);
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("Failed to write data during background flush for inode {}: {}", ino, e);
                                }
                            }
                        }
                    }
                }
            });
        }

        // Create root directory metadata
        let root_metadata = FileMetadata {
            id: dfs_common::FileId::new(),
            path: "/".to_string(),
            size: 0,
            chunk_locations: Vec::new(),
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
            write_buffers: write_buffers_for_cleanup,
            last_chunk_cache: Arc::new(RwLock::new(None)),
            dir_cache: Arc::new(RwLock::new(HashMap::new())),
            statfs_cache: Arc::new(RwLock::new(None)),
            lock_manager: Arc::new(LockManager::new()),
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
    fn init(
        &mut self,
        _req: &FuseRequest,
        config: &mut fuser::KernelConfig,
    ) -> Result<(), libc::c_int> {
        info!("Initializing DFS filesystem");

        // Enable POSIX file locking - tell kernel to use our setlk/getlk implementations
        // instead of handling locks in the kernel
        match config.add_capabilities(fuser::consts::FUSE_POSIX_LOCKS) {
            Ok(()) => {
                info!("FUSE_POSIX_LOCKS capability enabled");
                Ok(())
            }
            Err(_) => {
                error!("Failed to enable FUSE_POSIX_LOCKS capability");
                Err(libc::EIO)
            }
        }
    }

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

        // Check if this is a SQLite database file by looking up its path
        let is_sqlite = {
            let cache = self.metadata_cache.read().unwrap();
            if let Some(metadata) = cache.get(&ino) {
                let path = &metadata.path;
                // SQLite database files need direct I/O to avoid cache coherency issues
                path.ends_with(".db")
                    || path.ends_with(".sqlite")
                    || path.ends_with(".sqlite3")
                    || path.ends_with(".db-wal")
                    || path.ends_with(".db-journal")
                    || path.ends_with(".db-shm")
            } else {
                false
            }
        };

        if is_sqlite {
            // For SQLite files: Use direct I/O to bypass page cache
            // This ensures lock consistency and prevents cache coherency issues
            info!("open: ino={} - SQLite database detected, using direct I/O", ino);
            reply.opened(0, fuser::consts::FOPEN_DIRECT_IO);
        } else {
            // For regular files: Use page cache for better performance
            reply.opened(0, fuser::consts::FOPEN_KEEP_CACHE);
        }
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
                // Check if read overlaps with write buffer
                let should_flush_buffer = {
                    let buffers_lock = write_buffers.lock().await;
                    if let Some(buffer) = buffers_lock.get(&ino) {
                        let buffer_start = buffer.start_offset as usize;
                        let buffer_end = buffer_start + buffer.data.len();
                        let read_end = offset + size;

                        // If read is entirely within buffer, serve it directly
                        if offset >= buffer_start && read_end <= buffer_end {
                            let buffer_relative_offset = offset - buffer_start;
                            let data = buffer.data[buffer_relative_offset..buffer_relative_offset + size].to_vec();

                            info!("FUSE read from write buffer: ino={}, offset={}, size={}, buffer_range=[{}, {})",
                                  ino, offset, size, buffer_start, buffer_end);

                            let elapsed = start.elapsed();
                            info!("FUSE read COMPLETE (write buffer): ino={}, offset={}, size={}, took {:?}",
                                  ino, offset, size, elapsed);

                            reply.data(&data);
                            return;
                        }

                        // If read is CLOSE to end of buffer (within 64KB), flush to avoid short reads
                        // This prevents garbled video from partial reads when watching live TV
                        if offset >= buffer_start && offset < buffer_end {
                            let distance_to_buffer_end = buffer_end - offset;
                            if distance_to_buffer_end < 65536 {  // Within 64KB of write position
                                info!("FUSE read near buffer end: ino={}, offset={}, distance={} bytes, buffer_range=[{}, {}), flushing buffer",
                                      ino, offset, distance_to_buffer_end, buffer_start, buffer_end);
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };

                // Flush buffer if needed (outside the lock to avoid deadlock)
                if should_flush_buffer {
                    info!("Flushing write buffer for ino={} due to boundary read", ino);
                    let buffer_opt = {
                        let mut buffers = write_buffers.lock().await;
                        buffers.remove(&ino)
                    };

                    if let Some(buffer) = buffer_opt {
                        let buffer_start = buffer.start_offset;
                        let buffer_size = buffer.data.len();

                        // Write buffered data
                        match client.write_data_with_cache(&buffer.data, ino, buffer_start).await {
                            Ok((new_chunk_ids, new_chunk_sizes)) => {
                                let new_size = buffer_start + buffer_size as u64;

                                // Update metadata
                                let mut meta = metadata_cache.read().unwrap().get(&ino).cloned();
                                if let Some(mut m) = meta {
                                    m.chunks.extend(new_chunk_ids);
                                    m.chunk_sizes.extend(new_chunk_sizes);
                                    m.size = new_size;
                                    m.modified_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

                                    if let Err(e) = client.put_file_metadata(&m).await {
                                        error!("Failed to update metadata after boundary flush: {}", e);
                                    } else {
                                        metadata_cache.write().unwrap().insert(ino, m.clone());
                                        metadata = m;  // Use updated metadata for read
                                        info!("Flushed {} bytes at offset {}, new size {}", buffer_size, buffer_start, new_size);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Failed to flush buffer for boundary read: {}", e);
                            }
                        }
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

            // For SQLite database files, disable caching by passing inode=0
            // This prevents stale cached data from causing corruption
            let cache_inode = {
                let path = &metadata.path;
                let is_sqlite = path.ends_with(".db")
                    || path.ends_with(".sqlite")
                    || path.ends_with(".sqlite3")
                    || path.ends_with(".db-wal")
                    || path.ends_with(".db-journal")
                    || path.ends_with(".db-shm");

                if is_sqlite {
                    0 // Disable caching for SQLite files
                } else {
                    ino // Enable caching for other files
                }
            };

            let all_chunks = metadata.chunks.clone();
            let result = client.read_data(&chunk_ids, &all_chunks, start_chunk_idx, cache_inode, &chunk_file_offsets).await;

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

            // Cache metadata, but DON'T overwrite if there's an active write
            // Use safe_metadata_update to check both buffers and write counters
            self.safe_metadata_update(entry_ino, entry.clone());

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
            chunk_locations: Vec::new(),
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
        let data_vec = data.to_vec(); // Copy data before moving

        // Execute write operation synchronously to preserve write order
        // Using spawn_blocking causes parallel execution which corrupts SQLite databases
        {
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

            // For SQLite database files, disable caching to prevent corruption
            let is_sqlite = {
                let path = &metadata.path;
                path.ends_with(".db") || path.ends_with(".sqlite") ||
                    path.ends_with(".sqlite3") || path.ends_with(".db-wal") ||
                    path.ends_with(".db-journal") || path.ends_with(".db-shm")
            };
            let cache_inode = if is_sqlite { 0 } else { ino };

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
                    // Buffer size threshold: REDUCED from 4MB to 1MB to prevent OOM
                    // With environment variable override support
                    let buffer_flush_threshold: usize = std::env::var("DFS_WRITE_BUFFER_SIZE")
                        .ok()
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(1 * 1024 * 1024); // Conservative default: 1MB

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
                            created_at: std::time::Instant::now(),
                        });

                        // No need to check idle here - we'll append data first, then check
                        // This ensures data isn't lost

                        // Safety check: if buffer is already way too large, something is wrong
                        // This can happen if writes backed up during lock contention
                        // REDUCED from 12MB to 3MB (3x the new 1MB threshold)
                        let max_buffer_size: usize = buffer_flush_threshold * 3;
                        if buffer.data.len() > max_buffer_size {
                            error!("Buffer overflow detected for inode {}: {} bytes (max {}), refusing to buffer more data",
                                   ino, buffer.data.len(), max_buffer_size);
                            return Err(anyhow::anyhow!("Buffer overflow"));
                        }

                        // Append data to buffer
                        buffer.data.extend_from_slice(data_slice);
                        buffer.last_modified = SystemTime::now();

                        // Check if buffer exceeds threshold
                        // Note: Don't check idle here - we just updated last_modified!
                        Ok::<bool, anyhow::Error>(buffer.data.len() >= buffer_flush_threshold)
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
                                            // Use mem::take to move data without cloning (prevents memory leak)
                                            let data: Vec<u8> = std::mem::take(&mut buffer.data);
                                            let start: u64 = buffer.start_offset;
                                            // Update start offset for new writes
                                            let new_start = start + data.len() as u64;
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
                                            .write_data_with_cache(&data, cache_inode, buffer_start_offset)
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

            // Track affected chunk range for random writes (for metadata splice)
            let mut affected_chunk_range: Option<(usize, usize)> = None;

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
                // CRITICAL FIX: Only read affected chunks, not entire file!
                // For a 10GB file with 1KB write, reading entire file causes OOM

                info!("Random write detected: offset={}, size={}, file_size={}",
                      offset, data_vec.len(), current_size);

                let write_end = offset + data_vec.len();

                // Calculate which chunks are affected by this write
                let chunk_ids = metadata.chunks.clone();
                let chunk_sizes = metadata.chunk_sizes.clone();

                if chunk_ids.is_empty() {
                    // Empty file - just write the data
                    (data_vec.clone(), false)
                } else {
                    // Find chunk range that overlaps with write range [offset, write_end)
                    let mut chunk_start_offset = 0u64;
                    let mut first_affected_chunk: Option<usize> = None;
                    let mut last_affected_chunk: Option<usize> = None;

                    for (idx, &chunk_size) in chunk_sizes.iter().enumerate() {
                        let chunk_end_offset = chunk_start_offset + chunk_size;

                        // Check if this chunk overlaps with write range
                        if chunk_end_offset > offset as u64 && chunk_start_offset < write_end as u64 {
                            if first_affected_chunk.is_none() {
                                first_affected_chunk = Some(idx);
                            }
                            last_affected_chunk = Some(idx);
                        }

                        chunk_start_offset = chunk_end_offset;
                    }

                    // Check if we found affected chunks
                    if first_affected_chunk.is_none() || last_affected_chunk.is_none() {
                        // Write is beyond EOF - treat as append
                        info!("Write beyond EOF, treating as append");
                        (data_vec.clone(), true)
                    } else {
                        let first_idx = first_affected_chunk.unwrap();
                        let last_idx = last_affected_chunk.unwrap();

                    info!("Random write affects chunks {}-{} (out of {} total)",
                          first_idx, last_idx, chunk_ids.len());

                    // Store affected range for metadata splice later
                    affected_chunk_range = Some((first_idx, last_idx));

                    // Read only the affected chunks
                    let affected_chunk_ids: Vec<_> = chunk_ids[first_idx..=last_idx].to_vec();
                    let affected_chunk_sizes: Vec<_> = chunk_sizes[first_idx..=last_idx].to_vec();

                    // Calculate file offset of first affected chunk
                    let first_chunk_file_offset: u64 = chunk_sizes[..first_idx].iter().sum();

                    // Build chunk offsets for affected chunks only
                    let mut chunk_offsets = Vec::with_capacity(affected_chunk_ids.len());
                    let mut current_offset = first_chunk_file_offset;
                    for &size in &affected_chunk_sizes {
                        chunk_offsets.push(current_offset);
                        current_offset += size;
                    }

                    let affected_data = match runtime.block_on(async {
                        client.read_data(&affected_chunk_ids, &chunk_ids, first_idx, cache_inode, &chunk_offsets).await
                    }) {
                        Ok(data) => data,
                        Err(e) => {
                            error!("Failed to read affected chunks {}-{}: {}", first_idx, last_idx, e);
                            reply.error(libc::EIO);
                            return;
                        }
                    };

                    // Calculate offset within the affected chunk range
                    let write_offset_in_range = (offset as u64 - first_chunk_file_offset) as usize;
                    let affected_data_len = affected_data.len();

                    // Merge write data into affected chunks
                    let mut merged = affected_data;
                    if write_offset_in_range + data_vec.len() > merged.len() {
                        merged.resize(write_offset_in_range + data_vec.len(), 0);
                    }
                    merged[write_offset_in_range..write_offset_in_range + data_vec.len()]
                        .copy_from_slice(&data_vec);

                    info!("Random write: read {} bytes from {} chunks, merged to {} bytes",
                          affected_data_len, affected_chunk_ids.len(), merged.len());

                        // Return merged data and metadata update strategy
                        // We'll need to splice the new chunks into the metadata
                        (merged, false)
                    }
                }
            };

            // Write to cluster (only new/modified data for appends)
            // Use write_data_with_cache to populate byte-range cache for immediate read-back
            let write_start = std::time::Instant::now();
            let result = if is_append {
                // Append: write just the new data as new chunks
                runtime.block_on(async {
                    // Pass file offset for cache population (write-through caching)
                    client.write_data_with_cache(&new_data, cache_inode, current_size as u64).await
                })
            } else {
                // Random write: write only the affected chunks
                // Calculate file offset of first affected chunk for cache population
                let write_file_offset = if let Some((first_idx, _)) = affected_chunk_range {
                    metadata.chunk_sizes[..first_idx].iter().sum::<u64>()
                } else {
                    0
                };
                runtime.block_on(async {
                    client.write_data_with_cache(&new_data, cache_inode, write_file_offset).await
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
                    } else if let Some((first_idx, last_idx)) = affected_chunk_range {
                        // Random write: splice new chunks into affected range
                        // Keep chunks before affected range, insert new chunks, keep chunks after
                        info!("Splicing {} new chunks into range {}-{} (was {} chunks)",
                              new_chunk_ids.len(), first_idx, last_idx, last_idx - first_idx + 1);

                        let mut updated_chunks = Vec::new();
                        let mut updated_sizes = Vec::new();

                        // Keep chunks before affected range
                        updated_chunks.extend_from_slice(&metadata.chunks[..first_idx]);
                        updated_sizes.extend_from_slice(&metadata.chunk_sizes[..first_idx]);

                        // Insert new chunks
                        updated_chunks.extend(new_chunk_ids);
                        updated_sizes.extend(new_chunk_sizes);

                        // Keep chunks after affected range (if any)
                        if last_idx + 1 < metadata.chunks.len() {
                            updated_chunks.extend_from_slice(&metadata.chunks[last_idx + 1..]);
                            updated_sizes.extend_from_slice(&metadata.chunk_sizes[last_idx + 1..]);
                        }

                        metadata.chunks = updated_chunks;
                        metadata.chunk_sizes = updated_sizes;

                        // Recalculate total file size
                        metadata.size = metadata.chunk_sizes.iter().sum();

                        info!("After splice: {} total chunks, {} total bytes",
                              metadata.chunks.len(), metadata.size);
                    } else {
                        // Full rewrite (shouldn't happen with current logic, but keep as fallback)
                        warn!("Full file rewrite with {} bytes", new_data.len());
                        metadata.chunks = new_chunk_ids;
                        metadata.chunk_sizes = new_chunk_sizes;
                        metadata.size = new_data.len() as u64;
                    }
                    metadata.modified_at = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs();

                    // Check if this is a SQLite database file - they need immediate metadata updates
                    let is_sqlite_db = {
                        let path = &metadata.path;
                        path.ends_with(".db") || path.ends_with(".sqlite") ||
                            path.ends_with(".sqlite3") || path.ends_with(".db-wal") ||
                            path.ends_with(".db-journal") || path.ends_with(".db-shm")
                    };

                    // Batch metadata updates for non-SQLite files: only update every 10 writes
                    // For SQLite files: ALWAYS update immediately to prevent corruption
                    let (should_update, count) = if is_sqlite_db {
                        debug!("SQLite database detected - forcing immediate metadata update for ino={}", ino);
                        (true, 0) // Always update, count doesn't matter
                    } else {
                        let count = {
                            let mut counters = write_counters.write().unwrap();
                            let c = counters.entry(ino).or_insert(0);
                            *c += 1;
                            *c
                        };
                        (count % 10 == 0, count)
                    };

                    if should_update {
                        // Store updated metadata
                        let metadata_start = std::time::Instant::now();
                        let metadata_clone = metadata.clone();
                        let update_result = runtime.block_on(async {
                            client.put_file_metadata(&metadata_clone).await
                        });
                        let metadata_elapsed = metadata_start.elapsed();
                        if is_sqlite_db {
                            debug!("put_file_metadata took {:?} (SQLite immediate update)", metadata_elapsed);
                        } else {
                            debug!("put_file_metadata took {:?} (batched at write #{})", metadata_elapsed, count);
                        }

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
        // Clone Arc-wrapped fields for thread pool
        let write_buffer_enabled = self.write_buffer_enabled;
        let write_buffers = self.write_buffers.clone();
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let write_counters = self.write_counters.clone();
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
                // No write buffer, but we still need to flush any pending metadata updates
                // that were batched by the write() path to ensure data durability
                let result = runtime.block_on(async {
                    // Get metadata from cache
                    let metadata_opt = {
                        let cache = metadata_cache.read().unwrap();
                        cache.get(&ino).cloned()
                    };

                    if let Some(metadata) = metadata_opt {
                        // Check if there are pending writes
                        let has_pending = {
                            let counters = write_counters.read().unwrap();
                            counters.get(&ino).map(|c| *c > 0).unwrap_or(false)
                        };

                        if has_pending {
                            debug!("flush: flushing pending metadata for ino={}", ino);
                            client.put_file_metadata(&metadata).await?;
                            // Reset write counter after successful metadata flush
                            write_counters.write().unwrap().insert(ino, 0);
                        }
                    }

                    Ok::<(), anyhow::Error>(())
                });

                match result {
                    Ok(_) => reply.ok(),
                    Err(e) => {
                        error!("Failed to flush metadata for inode {}: {}", ino, e);
                        reply.error(libc::EIO);
                    }
                }
            }
        });
    }

    fn release(
        &mut self,
        req: &FuseRequest,
        ino: u64,
        _fh: u64,
        _flags: i32,
        lock_owner: Option<u64>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        let pid = req.pid();
        debug!("release: ino={}, owner={:?}, pid={}", ino, lock_owner, pid);

        let lock_manager = self.lock_manager.clone();

        if self.write_buffer_enabled {
            // Flush any buffered writes on file close, then release locks
            let result = self.block_on(async {
                // First flush writes
                self.flush_buffer_async(ino).await?;

                // Then release all locks held by this owner (if lock_owner is provided)
                // lock_owner is only provided if the process held locks
                if let Some(owner) = lock_owner {
                    lock_manager.release_all(ino, owner).await?;
                }

                Ok::<(), anyhow::Error>(())
            });

            match result {
                Ok(_) => reply.ok(),
                Err(e) => {
                    error!("Failed to flush/release for inode {}: {}", ino, e);
                    reply.error(libc::EIO);
                }
            }
        } else {
            // No write buffer, but flush any pending metadata updates before releasing locks
            let client = self.client.clone();
            let metadata_cache = self.metadata_cache.clone();
            let write_counters = self.write_counters.clone();

            let result = self.block_on(async {
                // Flush pending metadata if any
                let metadata_opt = {
                    let cache = metadata_cache.read().unwrap();
                    cache.get(&ino).cloned()
                };

                if let Some(metadata) = metadata_opt {
                    let has_pending = {
                        let counters = write_counters.read().unwrap();
                        counters.get(&ino).map(|c| *c > 0).unwrap_or(false)
                    };

                    if has_pending {
                        debug!("release: flushing pending metadata for ino={}", ino);
                        client.put_file_metadata(&metadata).await?;
                        write_counters.write().unwrap().insert(ino, 0);
                    }
                }

                // Release locks if lock_owner is provided
                if let Some(owner) = lock_owner {
                    lock_manager.release_all(ino, owner).await?;
                }
                Ok::<(), anyhow::Error>(())
            });

            match result {
                Ok(_) => reply.ok(),
                Err(e) => {
                    error!("Failed to flush/release locks for inode {}: {}", ino, e);
                    reply.error(libc::EIO);
                }
            }
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
            chunk_locations: Vec::new(),
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
            Ok(Some(metadata)) => {
                // Use atomic rename operation - server handles the entire rename atomically
                // This prevents race conditions where the file disappears during rename
                let rename_result = self.block_on(async {
                    client.rename_file(&old_path, &new_path).await
                });

                match rename_result {
                    Ok(_) => {
                        // CRITICAL: Keep the same inode number!
                        // The kernel's FUSE layer still has references to the old inode
                        // If we create a new inode, the old one becomes orphaned
                        let ino = self.path_to_inode.read().unwrap().get(&old_path).copied()
                            .unwrap_or_else(|| self.get_or_create_inode(&old_path));

                        // Remove old path mapping
                        self.path_to_inode.write().unwrap().remove(&old_path);

                        // Add new path mapping with SAME inode
                        self.path_to_inode.write().unwrap().insert(new_path.clone(), ino);

                        // Update metadata in cache with new path (same inode)
                        let mut new_metadata = metadata.clone();
                        new_metadata.path = new_path.clone();
                        new_metadata.modified_at = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs();

                        self.metadata_cache.write().unwrap().insert(ino, new_metadata);

                        // Invalidate directory cache for both old and new parent directories
                        let old_parent = old_path.rsplitn(2, '/').nth(1).unwrap_or("/");
                        let new_parent = new_path.rsplitn(2, '/').nth(1).unwrap_or("/");
                        self.dir_cache.write().unwrap().remove(old_parent);
                        if old_parent != new_parent {
                            self.dir_cache.write().unwrap().remove(new_parent);
                        }

                        info!("Renamed {} -> {} (inode {} preserved {} chunks)", old_path, new_path, ino, metadata.chunks.len());
                        reply.ok();
                    }
                    Err(e) => {
                        error!("Failed to rename {} -> {}: {}", old_path, new_path, e);
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
                } else if new_size > metadata.size {
                    // Growing file - just update metadata to extend with zeros
                    // No need to read existing data, just keep existing chunks
                    info!("Truncate growing: {} -> {} bytes (keeping {} chunks)",
                          metadata.size, new_size, metadata.chunks.len());
                    metadata.size = new_size;
                    // Note: The chunks stay the same, reads beyond will return zeros via FUSE
                } else {
                    // Shrinking file - only read chunks up to new_size
                    // CRITICAL FIX: Don't read entire file for truncate!
                    info!("Truncate shrinking: {} -> {} bytes", metadata.size, new_size);

                    if metadata.chunks.is_empty() {
                        metadata.size = new_size;
                    } else {
                        let chunk_sizes = metadata.chunk_sizes.clone();

                        // Find which chunks we need to keep (up to new_size)
                        let mut cumulative_size = 0u64;
                        let mut last_chunk_idx = 0;
                        let mut bytes_in_last_chunk = 0u64;

                        for (idx, &size) in chunk_sizes.iter().enumerate() {
                            if cumulative_size + size <= new_size {
                                // Entire chunk is kept
                                cumulative_size += size;
                                last_chunk_idx = idx;
                            } else if cumulative_size < new_size {
                                // This chunk is partially kept
                                bytes_in_last_chunk = new_size - cumulative_size;
                                last_chunk_idx = idx;
                                break;
                            } else {
                                break;
                            }
                        }

                        if bytes_in_last_chunk > 0 {
                            // Need to read and truncate the last partial chunk
                            info!("Truncate: keeping {} full chunks, truncating chunk {} to {} bytes",
                                  last_chunk_idx, last_chunk_idx, bytes_in_last_chunk);

                            let chunk_id = metadata.chunks[last_chunk_idx];
                            let chunk_offset: u64 = chunk_sizes[..last_chunk_idx].iter().sum();

                            // Read only the last partial chunk
                            let last_chunk_data = match self.block_on(async {
                                client.read_data(&[chunk_id], &metadata.chunks, last_chunk_idx, ino, &[chunk_offset]).await
                            }) {
                                Ok(data) => data,
                                Err(e) => {
                                    error!("Failed to read last chunk for truncate: {}", e);
                                    reply.error(libc::EIO);
                                    return;
                                }
                            };

                            // Truncate the chunk data
                            let truncated_chunk = &last_chunk_data[..bytes_in_last_chunk as usize];

                            // Write back the truncated chunk
                            match self.block_on(async {
                                client.write_data_with_cache(truncated_chunk, ino, chunk_offset).await
                            }) {
                                Ok((new_chunk_ids, new_chunk_sizes)) => {
                                    // Keep chunks before last, add truncated chunk
                                    let mut new_all_chunks = metadata.chunks[..last_chunk_idx].to_vec();
                                    new_all_chunks.extend(new_chunk_ids);

                                    let mut new_all_sizes = metadata.chunk_sizes[..last_chunk_idx].to_vec();
                                    new_all_sizes.extend(new_chunk_sizes);

                                    metadata.chunks = new_all_chunks;
                                    metadata.chunk_sizes = new_all_sizes;
                                    metadata.size = new_size;
                                }
                                Err(e) => {
                                    error!("Failed to write truncated chunk: {}", e);
                                    reply.error(libc::EIO);
                                    return;
                                }
                            }
                        } else {
                            // All chunks are complete, just drop the ones after
                            info!("Truncate: keeping {} full chunks, dropping rest", last_chunk_idx + 1);
                            metadata.chunks.truncate(last_chunk_idx + 1);
                            metadata.chunk_sizes.truncate(last_chunk_idx + 1);
                            metadata.size = new_size;
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

        const BLOCK_SIZE: u32 = 4096;
        const CACHE_TTL_SECS: u64 = 30; // Cache for 30 seconds

        // Check cache first
        let cached = {
            let cache = self.statfs_cache.read().unwrap();
            cache.as_ref().and_then(|(total, free, avail, timestamp)| {
                if timestamp.elapsed().as_secs() < CACHE_TTL_SECS {
                    debug!("statfs cache HIT (age: {}s)", timestamp.elapsed().as_secs());
                    Some((*total, *free, *avail))
                } else {
                    debug!("statfs cache EXPIRED");
                    None
                }
            })
        };

        let (total_blocks, free_blocks, avail_blocks) = if let Some((total, free, avail)) = cached {
            (total, free, avail)
        } else {
            // Cache miss - query cluster (this is SLOW!)
            debug!("statfs cache MISS - querying cluster");
            let client = self.client.clone();
            let statfs_cache = self.statfs_cache.clone();

            let result = self.block_on(async {
                client.get_storage_stats().await
            });

            match result {
                Ok((total_space, free_space, available_space, _replication_factor)) => {
                    // Convert bytes to blocks
                    let total = total_space / BLOCK_SIZE as u64;
                    let free = free_space / BLOCK_SIZE as u64;
                    let avail = available_space / BLOCK_SIZE as u64;

                    // Update cache
                    *statfs_cache.write().unwrap() = Some((total, free, avail, std::time::Instant::now()));

                    (total, free, avail)
                }
                Err(e) => {
                    error!("Failed to get storage stats: {}", e);
                    // Return reasonable defaults on error
                    (1_000_000_000, 500_000_000, 500_000_000)
                }
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
            // No write buffer, but we still need to flush any pending metadata updates
            // that were batched by the write() path to ensure data durability
            let client = self.client.clone();
            let metadata_cache = self.metadata_cache.clone();
            let write_counters = self.write_counters.clone();

            let result = self.block_on(async {
                // Get metadata from cache
                let metadata_opt = {
                    let cache = metadata_cache.read().unwrap();
                    cache.get(&ino).cloned()
                };

                if let Some(metadata) = metadata_opt {
                    // Check if there are pending writes
                    let has_pending = {
                        let counters = write_counters.read().unwrap();
                        counters.get(&ino).map(|c| *c > 0).unwrap_or(false)
                    };

                    if has_pending {
                        debug!("fsync: flushing pending metadata for ino={}", ino);
                        client.put_file_metadata(&metadata).await?;
                        // Reset write counter after successful metadata flush
                        write_counters.write().unwrap().insert(ino, 0);
                    }
                }

                Ok::<(), anyhow::Error>(())
            });

            match result {
                Ok(_) => reply.ok(),
                Err(e) => {
                    error!("Failed to flush metadata on fsync for inode {}: {}", ino, e);
                    reply.error(libc::EIO);
                }
            }
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

    fn setlk(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        lock_owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        sleep: bool,
        reply: fuser::ReplyEmpty,
    ) {
        debug!(
            "setlk: ino={}, owner={}, pid={}, type={}, range=[{}, {}), sleep={}",
            ino, lock_owner, pid, typ, start, end, sleep
        );

        let lock_manager = self.lock_manager.clone();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            use crate::locks::LockType;

            // Convert FUSE range to our representation
            // end=u64::MAX means "to EOF" (len=0)
            let len = if end == u64::MAX { 0 } else { end.saturating_sub(start) };

            let result = match typ {
                libc::F_RDLCK => {
                    // Shared (read) lock
                    if sleep {
                        lock_manager.lock_wait(ino, lock_owner, pid, LockType::Shared, start, len).await
                    } else {
                        lock_manager.try_lock(ino, lock_owner, pid, LockType::Shared, start, len).await
                    }
                }
                libc::F_WRLCK => {
                    // Exclusive (write) lock
                    if sleep {
                        lock_manager.lock_wait(ino, lock_owner, pid, LockType::Exclusive, start, len).await
                    } else {
                        lock_manager.try_lock(ino, lock_owner, pid, LockType::Exclusive, start, len).await
                    }
                }
                libc::F_UNLCK => {
                    // Unlock
                    lock_manager.unlock(ino, lock_owner, start, len).await
                }
                _ => {
                    error!("Invalid lock type: {}", typ);
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            match result {
                Ok(()) => reply.ok(),
                Err(e) => {
                    if sleep {
                        // Blocking lock failed (should be rare)
                        error!("Lock acquisition failed: {}", e);
                        reply.error(libc::EIO);
                    } else {
                        // Non-blocking lock would block (conflict detected)
                        reply.error(libc::EAGAIN);
                    }
                }
            }
        });
    }

    fn getlk(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        lock_owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: fuser::ReplyLock,
    ) {
        debug!(
            "getlk: ino={}, owner={}, pid={}, type={}, range=[{}, {})",
            ino, lock_owner, pid, typ, start, end
        );

        let lock_manager = self.lock_manager.clone();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            use crate::locks::LockType;

            let lock_type = match typ {
                libc::F_RDLCK => LockType::Shared,
                libc::F_WRLCK => LockType::Exclusive,
                _ => {
                    error!("Invalid lock type for getlk: {}", typ);
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            let len = if end == u64::MAX { 0 } else { end.saturating_sub(start) };

            match lock_manager.get_conflict(ino, lock_owner, pid, lock_type, start, len).await {
                Some(conflict) => {
                    // Return conflicting lock details
                    let fuse_type = match conflict.lock_type {
                        LockType::Shared => libc::F_RDLCK,
                        LockType::Exclusive => libc::F_WRLCK,
                    };

                    let conflict_end = if conflict.len == 0 {
                        u64::MAX
                    } else {
                        conflict.start.saturating_add(conflict.len)
                    };

                    debug!(
                        "getlk: found conflict with owner={} pid={} type={} range=[{}, {})",
                        conflict.owner, conflict.pid, fuse_type, conflict.start, conflict_end
                    );

                    reply.locked(conflict.start, conflict_end, fuse_type, conflict.pid);
                }
                None => {
                    // No conflict - return F_UNLCK to indicate lock would succeed
                    debug!("getlk: no conflict found, lock would succeed");
                    reply.locked(start, end, libc::F_UNLCK, pid);
                }
            }
        });
    }
}
