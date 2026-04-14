use anyhow::Result;
use dashmap::DashMap;
use libc;
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

/// Returns true if the path is a SQLite database or one of its sidecar/temp files.
/// Used to decide: direct I/O mode, write buffering bypass, immediate metadata flush.
/// Matches .db/.sqlite/.sqlite3 and their -wal/-journal/-shm sidecars,
/// plus _temp variants (e.g. gravity.db_temp used by pihole during gravity updates).
fn is_sqlite_path(path: &str) -> bool {
    path.ends_with(".db")
        || path.ends_with(".sqlite")
        || path.ends_with(".sqlite3")
        || path.ends_with(".db-wal")
        || path.ends_with(".db-journal")
        || path.ends_with(".db-shm")
        || path.ends_with(".db_temp")
        || path.ends_with(".sqlite_temp")
        || path.ends_with(".sqlite3_temp")
}

/// Same as is_sqlite_path but excludes .db-shm, which must NOT use FOPEN_DIRECT_IO
/// because SQLite mmaps it (MAP_SHARED) for WAL index coordination.
fn is_sqlite_direct_io(path: &str) -> bool {
    path.ends_with(".db")
        || path.ends_with(".sqlite")
        || path.ends_with(".sqlite3")
        || path.ends_with(".db-wal")
        || path.ends_with(".db-journal")
        || path.ends_with(".db_temp")
        || path.ends_with(".sqlite_temp")
        || path.ends_with(".sqlite3_temp")
}

/// Buffered write data for a single file
#[derive(Clone)]
struct WriteBuffer {
    /// Buffered data
    data: Vec<u8>,
    /// When this buffer was last modified
    last_modified: SystemTime,
    /// File offset where this buffer starts (= file size when buffer was created)
    start_offset: u64,
    /// When this buffer was created
    created_at: std::time::Instant,
    /// Preferred primary node for the next AppendFile call.
    /// Set on buffer creation, rotated when the server signals remaining_in_chunk == 0
    /// (chunk boundary crossed). None means pick by health on next flush.
    preferred_primary: Option<std::net::SocketAddr>,
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

    /// Check if this buffer has enough data for a background flush.
    /// Background flushes only send chunk-aligned data (multiples of 4MB) via the
    /// fast dual-parallel client write path. Partial tails are held until fsync/close,
    /// where AppendFile handles the final delta cheaply (server reads partial tail
    /// locally, no re-transmission from client).
    fn should_background_flush(&self) -> bool {
        const CHUNK_SIZE: usize = 4 * 1024 * 1024;
        self.data.len() >= CHUNK_SIZE
    }

    /// How many bytes of this buffer are chunk-aligned (safe for dual-parallel write).
    fn aligned_flush_len(&self) -> usize {
        const CHUNK_SIZE: usize = 4 * 1024 * 1024;
        (self.data.len() / CHUNK_SIZE) * CHUNK_SIZE
    }
}


/// Cheaply-cloneable handle to the fields needed by flush_buffer_async.
/// Extracted so fsync() can clone it and spawn a background flush task without
/// holding a reference to DfsFilesystem (which is !Clone due to &mut self callbacks).
#[derive(Clone)]
struct FlushHandle {
    client: Arc<DfsClient>,
    write_buffers: Arc<DashMap<u64, Arc<Mutex<WriteBuffer>>>>,
    metadata_cache: Arc<DashMap<u64, FileMetadata>>,
    flush_in_flight: Arc<RwLock<Option<Arc<dashmap::DashSet<u64>>>>>,
}

impl FlushHandle {
    async fn flush_buffer_async(&self, ino: u64, force: bool) -> Result<()> {
        const CHUNK_SIZE: usize = 4 * 1024 * 1024;

        let snapshot = {
            if let Some(buffer_lock) = self.write_buffers.get(&ino) {
                let buffer = buffer_lock.lock().await;
                if buffer.data.is_empty() {
                    return Ok(());
                }
                let aligned_len = buffer.aligned_flush_len();
                let tail_len = buffer.data.len() - aligned_len;
                if !force && aligned_len == 0 {
                    return Ok(());
                }
                Some((buffer.data.clone(), buffer.start_offset, aligned_len, tail_len))
            } else {
                None
            }
        };

        let (all_data, start_offset, aligned_len, tail_len) = match snapshot {
            Some(v) => v,
            None => return Ok(()),
        };

        let (file_id, path) = {
            match self.metadata_cache.get(&ino) {
                Some(m) => (m.id, m.path.clone()),
                None => anyhow::bail!("Metadata not found for inode {}", ino),
            }
        };

        // --- Step 1: Dual-parallel write for all complete 4MB chunks ---
        if aligned_len > 0 {
            let aligned_data = &all_data[..aligned_len];
            info!("FlushHandle::flush_buffer_async: dual-parallel write {} bytes ({} chunks) for inode {} at offset {}",
                  aligned_len, aligned_len / CHUNK_SIZE, ino, start_offset);

            let (chunk_ids, chunk_sizes, locations_opt) = self.client
                .write_data_with_cache(aligned_data, ino, start_offset)
                .await?;

            if let Some(buffer_lock) = self.write_buffers.get(&ino) {
                let mut buffer = buffer_lock.lock().await;
                if buffer.start_offset == start_offset {
                    let drain = aligned_len.min(buffer.data.len());
                    buffer.data.drain(..drain);
                    buffer.start_offset += drain as u64;
                    buffer.last_modified = SystemTime::now();
                }
            }

            if let Some(locations) = locations_opt {
                if let Some(mut meta) = self.metadata_cache.get_mut(&ino) {
                    for loc in &locations {
                        if !meta.chunks.contains(&loc.chunk_id) {
                            meta.chunks.push(loc.chunk_id);
                            meta.chunk_sizes.push(loc.size as u64);
                            meta.chunk_locations.push(loc.clone());
                        }
                    }
                    meta.size = start_offset + aligned_len as u64;
                    meta.modified_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                }
            } else {
                if let Some(mut meta) = self.metadata_cache.get_mut(&ino) {
                    meta.size = start_offset + aligned_len as u64;
                }
            }

            let meta_to_persist = self.metadata_cache.get(&ino).map(|m| m.clone());
            if let Some(meta) = meta_to_persist {
                let _ = self.client.put_file_metadata(&meta).await;
            }
        }

        // --- Step 2: AppendFile for the partial tail (only on forced flush) ---
        if force && tail_len > 0 {
            // Wait for any in-flight background flush to complete (not the fsync we just
            // spawned — that's us — but any concurrent background tick flush).
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                let in_flight_opt = self.flush_in_flight.read().unwrap().clone();
                // Only wait for background tick flushes, not fsync-spawned ones (ino is
                // already in in_flight for us; background flushes also use the same set).
                // We use a separate "background_in_flight" concept via the background flusher
                // inserting/removing independently — here we just yield briefly to let any
                // racing background tick drain before we send AppendFile.
                let _ = in_flight_opt; // no additional wait needed — we're the in_flight holder
                break;
            }

            let (tail_data, tail_offset) = {
                if let Some(buffer_lock) = self.write_buffers.get(&ino) {
                    let buffer = buffer_lock.lock().await;
                    if buffer.data.is_empty() {
                        return Ok(());
                    }
                    (buffer.data.clone(), buffer.start_offset)
                } else {
                    return Ok(());
                }
            };

            info!("FlushHandle::flush_buffer_async: AppendFile partial tail {} bytes for inode {} at offset {}",
                  tail_data.len(), ino, tail_offset);

            let (updated_metadata, _remaining, _primary) = match self.client
                .append_file(file_id, tail_data.clone(), tail_offset, None)
                .await
            {
                Ok(result) => result,
                Err(e) if e.to_string().contains("OffsetMismatch") => {
                    warn!("FlushHandle::flush_buffer_async: OffsetMismatch on tail flush for inode {}, re-fetching and retrying", ino);
                    let fresh = self.client.get_file_metadata(&path).await?
                        .ok_or_else(|| anyhow::anyhow!("File not found during tail flush retry: {}", path))?;
                    let new_offset = fresh.size;
                    self.metadata_cache.insert(ino, fresh);
                    if let Some(buffer_lock) = self.write_buffers.get(&ino) {
                        let mut buffer = buffer_lock.lock().await;
                        buffer.start_offset = new_offset;
                    }
                    self.client.append_file(file_id, tail_data.clone(), new_offset, None).await?
                }
                Err(e) => return Err(e),
            };

            if let Some(buffer_lock) = self.write_buffers.get(&ino) {
                let mut buffer = buffer_lock.lock().await;
                let drain = tail_data.len().min(buffer.data.len());
                buffer.data.drain(..drain);
                buffer.start_offset += drain as u64;
                buffer.last_modified = SystemTime::now();
            }

            self.metadata_cache.insert(ino, updated_metadata);
        }

        Ok(())
    }
}

/// FUSE filesystem implementation for DFS
pub struct DfsFilesystem {
    /// Client for communicating with DFS cluster
    client: Arc<DfsClient>,

    /// Metadata cache: inode -> FileMetadata
    metadata_cache: Arc<DashMap<u64, FileMetadata>>,

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

    /// Write buffers per inode with per-inode locking for concurrent access
    /// DashMap provides lock-free reads and fine-grained locking per inode
    write_buffers: Arc<DashMap<u64, Arc<Mutex<WriteBuffer>>>>,

    /// Last metadata update timestamp per inode for batching
    /// Prevents excessive metadata updates during continuous writes
    last_metadata_update: Arc<DashMap<u64, std::time::Instant>>,

    /// Last read chunk cache: (ino, chunk_index, data)
    /// Prevents re-fetching same 4MB chunk for multiple 128KB FUSE reads
    last_chunk_cache: Arc<RwLock<Option<(u64, usize, Vec<u8>)>>>,

    /// Track last warming offset per inode to throttle replica cache warming
    /// Prevents excessive warming overhead on files with many small chunks
    last_warm_offset: Arc<DashMap<u64, u64>>,

    /// Chunk offset map cache: inode -> Vec<(offset, size)>
    /// Prevents O(n) iteration through all chunks on every read
    /// Invalidated when file metadata changes (size/chunks)
    chunk_offset_cache: Arc<DashMap<u64, (u64, usize, Vec<(usize, usize)>)>>, // (file_size, chunk_count, offsets)

    /// Directory listing cache: path -> (entries, timestamp)
    /// Cache directory listings for 5 seconds to avoid repeated scans
    dir_cache: Arc<DashMap<String, (Vec<FileMetadata>, std::time::Instant)>>,

    /// Filesystem stats cache: (total, free, avail, timestamp)
    /// Cache statfs results for 30 seconds to avoid repeated expensive queries
    statfs_cache: Arc<RwLock<Option<(u64, u64, u64, std::time::Instant)>>>,

    /// Lock manager for byte-range locks
    lock_manager: Arc<LockManager>,

    /// Write buffer flush threshold in bytes (dynamic, queried from cluster)
    buffer_flush_threshold: usize,

    /// Count of write-mode open file handles per inode.
    /// Used to guard the write buffer in flush(): a flush() triggered by a read-only
    /// close must not touch the write buffer of a concurrently writing fd.
    write_open_counts: Arc<DashMap<u64, usize>>,

    /// Shared reference to the background flusher's in-flight set.
    /// Set by the background flusher task after spawn; flush_buffer_async (fsync/close)
    /// waits for any in-flight background flush to complete before sending its own flush
    /// to avoid concurrent flushes that would produce OffsetMismatch.
    flush_in_flight: Arc<RwLock<Option<Arc<dashmap::DashSet<u64>>>>>,

    /// Cloneable handle used by fsync() to spawn background flush tasks.
    flush_handle: FlushHandle,
}

impl DfsFilesystem {
    /// Create a new DFS filesystem with an explicit runtime handle
    pub fn new_with_runtime(
        cluster_nodes: Vec<SocketAddr>,
        write_buffer_enabled: bool,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self> {
        let client = Arc::new(DfsClient::new(cluster_nodes)?);

        // Query cluster for chunk size configuration
        let chunk_size_mb = runtime.block_on(async {
            client.get_cluster_chunk_size().await
        }).unwrap_or_else(|e| {
            warn!("Failed to query cluster chunk size, using default 4MB: {}", e);
            4  // Default to 4MB if query fails
        });
        // Use 3x chunk size for write buffer threshold to enable pipelined writes
        // With 4MB chunks, this gives us 12MB buffer = 3 chunks that can be written in parallel
        let buffer_flush_threshold = chunk_size_mb * 1024 * 1024 * 3;
        info!("Client configured with buffer_flush_threshold={} bytes ({}MB, 3x cluster chunk size) for pipelined writes",
              buffer_flush_threshold, buffer_flush_threshold / (1024 * 1024));

        // Populate addr_to_node_id immediately so the very first write gets real node IDs.
        if let Err(e) = runtime.block_on(client.refresh_cluster_nodes()) {
            tracing::warn!("Initial cluster node refresh failed: {}", e);
        }

        // Start background task to periodically refresh cluster nodes.
        // Uses exponential backoff on failure: 30s → 60s → 120s → 240s (cap),
        // resetting to 30s on the next success. This prevents hammering the network
        // when all nodes are temporarily unreachable (e.g. cluster restart).
        let client_clone = client.clone();
        runtime.spawn(async move {
            const BASE_INTERVAL: u64 = 30;
            const MAX_INTERVAL: u64 = 240;
            let mut interval_secs = BASE_INTERVAL;
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(interval_secs)).await;
                if let Err(e) = client_clone.refresh_cluster_nodes().await {
                    interval_secs = (interval_secs * 2).min(MAX_INTERVAL);
                    tracing::warn!("Failed to refresh cluster nodes (retrying in {}s): {}", interval_secs, e);
                } else {
                    interval_secs = BASE_INTERVAL;
                }
            }
        });

        let metadata_cache = Arc::new(DashMap::<u64, FileMetadata>::new());
        let path_to_inode = Arc::new(RwLock::new(HashMap::<String, u64>::new()));
        let next_inode = Arc::new(RwLock::new(2)); // Start at 2, root is 1
        let write_buffers_for_cleanup = Arc::new(DashMap::<u64, Arc<Mutex<WriteBuffer>>>::new());
        let flush_in_flight_shared: Arc<RwLock<Option<Arc<dashmap::DashSet<u64>>>>> =
            Arc::new(RwLock::new(None));

        // Start background task to flush expired write buffers (if buffering enabled)
        if write_buffer_enabled {
            let write_buffers_clone = write_buffers_for_cleanup.clone();
            let client_for_cleanup = client.clone();
            let metadata_cache_for_cleanup = metadata_cache.clone();
            // in_flight: tracks inodes with an active background flush task.
            // Prevents launching a second flush while the first is still in flight,
            // which would produce OffsetMismatch. Also used by flush_buffer_async
            // (fsync/close) to wait for any in-flight background flush to complete.
            let in_flight: Arc<dashmap::DashSet<u64>> = Arc::new(dashmap::DashSet::new());
            // Share the in_flight set with flush_buffer_async via the struct field
            *flush_in_flight_shared.write().unwrap() = Some(in_flight.clone());

            let flush_threshold_for_task = buffer_flush_threshold;
            runtime.spawn(async move {
                const CHUNK_SIZE: usize = 4 * 1024 * 1024;
                let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(100));
                loop {
                    interval.tick().await;

                    // Find inodes ready for a background flush.
                    // Fire when buffer has >= flush_threshold (12MB = 3 chunks) of aligned data
                    // so the pipelined write path is used (3 chunks in-flight = ~30 MB/s).
                    // Also fire when buffer has >= 1 chunk AND data is > 2s old, to drain
                    // buffers that stopped accumulating (file closed without fsync, etc.).
                    let flush_inodes: Vec<u64> = {
                        let mut ready = Vec::new();
                        for entry in write_buffers_clone.iter() {
                            let ino = *entry.key();
                            if in_flight.contains(&ino) { continue; }
                            let buffer = entry.value().lock().await;
                            let aligned = buffer.aligned_flush_len();
                            if aligned == 0 { continue; }
                            let age_ms = buffer.last_modified.elapsed()
                                .unwrap_or_default().as_millis();
                            let enough_for_pipeline = aligned >= flush_threshold_for_task;
                            let aging_out = aligned >= CHUNK_SIZE && age_ms >= 2000;
                            if enough_for_pipeline || aging_out {
                                ready.push(ino);
                            }
                        }
                        ready
                    };

                    for ino in flush_inodes {
                        // Snapshot the aligned portion only
                        let flush_opt = {
                            if let Some(buf_lock) = write_buffers_clone.get(&ino) {
                                let buffer = buf_lock.lock().await;
                                let aligned = buffer.aligned_flush_len();
                                if aligned == 0 { continue; }
                                let file_offset = buffer.start_offset;
                                Some((buffer.data[..aligned].to_vec(), file_offset))
                            } else { None }
                        };
                        let (flush_data, file_offset) = match flush_opt {
                            Some(v) => v,
                            None => continue,
                        };

                        info!("Background flush: {} bytes ({} chunks) for inode {} at offset {}",
                              flush_data.len(), flush_data.len() / CHUNK_SIZE, ino, file_offset);

                        in_flight.insert(ino);
                        let client_task = client_for_cleanup.clone();
                        let write_buffers_task = write_buffers_clone.clone();
                        let metadata_cache_task = metadata_cache_for_cleanup.clone();
                        let in_flight_task = in_flight.clone();

                        tokio::spawn(async move {
                            let flushed = flush_data.len();
                            match client_task.write_data_with_cache(&flush_data, ino, file_offset).await {
                                Ok((_chunk_ids, _chunk_sizes, locations_opt)) => {
                                    // Drain flushed bytes from buffer
                                    if let Some(buf_lock) = write_buffers_task.get(&ino) {
                                        let mut buffer = buf_lock.lock().await;
                                        if buffer.start_offset == file_offset {
                                            let drain = flushed.min(buffer.data.len());
                                            buffer.data.drain(..drain);
                                            buffer.start_offset += drain as u64;
                                            buffer.last_modified = SystemTime::now();
                                        }
                                    }
                                    // Update metadata cache with new chunk locations + size,
                                    // then persist. Drop the write guard before the await.
                                    if let Some(locations) = locations_opt {
                                        {
                                            if let Some(mut meta) = metadata_cache_task.get_mut(&ino) {
                                                for loc in &locations {
                                                    if !meta.chunks.contains(&loc.chunk_id) {
                                                        meta.chunks.push(loc.chunk_id);
                                                        meta.chunk_sizes.push(loc.size as u64);
                                                        meta.chunk_locations.push(loc.clone());
                                                    }
                                                }
                                                meta.size = file_offset + flushed as u64;
                                                meta.modified_at = std::time::SystemTime::now()
                                                    .duration_since(std::time::UNIX_EPOCH)
                                                    .unwrap_or_default().as_secs();
                                            }
                                        }
                                        let meta_to_persist = metadata_cache_task.get(&ino).map(|m| m.clone());
                                        if let Some(meta) = meta_to_persist {
                                            let _ = tokio::time::timeout(
                                                tokio::time::Duration::from_secs(5),
                                                client_task.put_file_metadata(&meta),
                                            ).await;
                                        }
                                    }
                                    info!("Background flush complete for inode {}: {} bytes at offset {}",
                                          ino, flushed, file_offset);
                                }
                                Err(e) => {
                                    tracing::error!("Background flush failed for inode {}: {}", ino, e);
                                }
                            }
                            in_flight_task.remove(&ino);
                        });
                    }
                }
            });
        }

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
            chunk_locations: Vec::new(),
        };

        metadata_cache.insert(1, root_metadata);
        path_to_inode.write().unwrap().insert("/".to_string(), 1);

        // Build FlushHandle before moving fields into the struct
        let flush_handle = FlushHandle {
            client: client.clone(),
            write_buffers: write_buffers_for_cleanup.clone(),
            metadata_cache: metadata_cache.clone(),
            flush_in_flight: flush_in_flight_shared.clone(),
        };

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
            last_metadata_update: Arc::new(DashMap::new()),
            last_chunk_cache: Arc::new(RwLock::new(None)),
            last_warm_offset: Arc::new(DashMap::new()),
            chunk_offset_cache: Arc::new(DashMap::new()),
            dir_cache: Arc::new(DashMap::new()),
            statfs_cache: Arc::new(RwLock::new(None)),
            lock_manager: Arc::new(LockManager::new()),
            buffer_flush_threshold,
            write_open_counts: Arc::new(DashMap::new()),
            flush_in_flight: flush_in_flight_shared,
            flush_handle,
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
                self.write_buffers.contains_key(&ino)
            } else {
                false
            };

            // Check write counter (indicates recent writes)
            let counters = self.write_counters.read().unwrap();
            let has_cnt = counters.contains_key(&ino);

            // Get current cached size
            let cur_size = self.metadata_cache.get(&ino).map(|m| m.size).unwrap_or(0);

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
            self.metadata_cache.insert(ino, metadata);
            true
        }
    }

    /// Check if metadata should be updated based on time-based batching
    /// Returns true if:
    /// - force_update is true (e.g., on close/fsync), OR
    /// - More than 2 seconds have elapsed since last update, OR
    /// - This is the first update for this inode
    fn should_update_metadata(&self, ino: u64, force_update: bool) -> bool {
        if force_update {
            return true;
        }

        const METADATA_UPDATE_INTERVAL_SECS: u64 = 2;

        match self.last_metadata_update.get(&ino) {
            None => true,  // First update
            Some(last) => {
                let elapsed = last.elapsed();
                elapsed >= std::time::Duration::from_secs(METADATA_UPDATE_INTERVAL_SECS)
            }
        }
    }

    /// Update the last metadata update timestamp for an inode
    fn record_metadata_update(&self, ino: u64) {
        self.last_metadata_update.insert(ino, std::time::Instant::now());
    }

    /// Flush the write buffer for `ino`.
    ///
    /// Hybrid strategy:
    ///   - Chunk-aligned data (multiples of 4MB): dual-parallel client write to 2 nodes
    ///     via write_data_with_cache() — maximum throughput, no extra hop.
    ///   - Partial tail (< 4MB remainder) on force=true (fsync/close): AppendFile RPC
    ///     sends only the delta bytes; server reads its local partial tail, prepends,
    ///     writes aligned chunks. Client never re-transmits existing data.
    ///   - force=false (background): only flushes aligned chunks, leaves partial tail
    ///     buffered until the next 4MB boundary or fsync/close.
    async fn flush_buffer_async(&self, ino: u64, force: bool) -> Result<()> {
        const CHUNK_SIZE: usize = 4 * 1024 * 1024;

        // Snapshot aligned portion (and optionally the full buffer including tail)
        let snapshot = {
            if let Some(buffer_lock) = self.write_buffers.get(&ino) {
                let buffer = buffer_lock.lock().await;
                if buffer.data.is_empty() {
                    return Ok(());
                }
                let aligned_len = buffer.aligned_flush_len();
                let tail_len = buffer.data.len() - aligned_len;
                // Background flush: only if we have >= 1 full chunk
                // Forced flush: always proceed (even if only tail remains)
                if !force && aligned_len == 0 {
                    return Ok(());
                }
                Some((
                    buffer.data.clone(),
                    buffer.start_offset,
                    aligned_len,
                    tail_len,
                ))
            } else {
                None
            }
        };

        let (all_data, start_offset, aligned_len, tail_len) = match snapshot {
            Some(v) => v,
            None => return Ok(()),
        };

        let (file_id, path) = {
            match self.metadata_cache.get(&ino) {
                Some(m) => (m.id, m.path.clone()),
                None => anyhow::bail!("Metadata not found for inode {}", ino),
            }
        };

        // --- Step 1: Dual-parallel write for all complete 4MB chunks ---
        if aligned_len > 0 {
            let aligned_data = &all_data[..aligned_len];
            info!("flush_buffer_async: dual-parallel write {} bytes ({} chunks) for inode {} at offset {}",
                  aligned_len, aligned_len / CHUNK_SIZE, ino, start_offset);

            let (chunk_ids, chunk_sizes, locations_opt) = self.client
                .write_data_with_cache(aligned_data, ino, start_offset)
                .await?;

            // Splice chunk metadata into cache
            if let Some(buffer_lock) = self.write_buffers.get(&ino) {
                let mut buffer = buffer_lock.lock().await;
                if buffer.start_offset == start_offset {
                    let drain = aligned_len.min(buffer.data.len());
                    buffer.data.drain(..drain);
                    buffer.start_offset += drain as u64;
                    buffer.last_modified = SystemTime::now();
                }
            }

            // Update metadata cache with new chunk locations
            if let Some(locations) = locations_opt {
                if let Some(mut meta) = self.metadata_cache.get_mut(&ino) {
                    for loc in &locations {
                        if !meta.chunks.contains(&loc.chunk_id) {
                            meta.chunks.push(loc.chunk_id);
                            meta.chunk_sizes.push(loc.size as u64);
                            meta.chunk_locations.push(loc.clone());
                        }
                    }
                    meta.size = start_offset + aligned_len as u64;
                    meta.modified_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                }
            } else {
                // Fallback: update size only
                if let Some(mut meta) = self.metadata_cache.get_mut(&ino) {
                    meta.size = start_offset + aligned_len as u64;
                }
            }

            // Persist metadata to server
            let meta_to_persist = self.metadata_cache.get(&ino).map(|m| m.clone());
            if let Some(meta) = meta_to_persist {
                let _ = self.client.put_file_metadata(&meta).await;
            }
        }

        // --- Step 2: AppendFile for the partial tail (only on forced flush) ---
        if force && tail_len > 0 {
            // Wait for any in-flight background flush to complete first — a background
            // task may still be writing the aligned portion we just submitted above,
            // and its drain hasn't happened yet. Racing would cause OffsetMismatch.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                let in_flight_opt = self.flush_in_flight.read().unwrap().clone();
                let still_running = in_flight_opt.map(|s| s.contains(&ino)).unwrap_or(false);
                if !still_running { break; }
                if std::time::Instant::now() >= deadline {
                    warn!("flush_buffer_async: timed out waiting for background flush on inode {}", ino);
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }

            // Re-read current buffer state — aligned flush may have drained some data
            let (tail_data, tail_offset) = {
                if let Some(buffer_lock) = self.write_buffers.get(&ino) {
                    let buffer = buffer_lock.lock().await;
                    if buffer.data.is_empty() {
                        return Ok(());
                    }
                    (buffer.data.clone(), buffer.start_offset)
                } else {
                    return Ok(());
                }
            };

            info!("flush_buffer_async: AppendFile partial tail {} bytes for inode {} at offset {}",
                  tail_data.len(), ino, tail_offset);

            let (updated_metadata, _remaining, _primary) = match self.client
                .append_file(file_id, tail_data.clone(), tail_offset, None)
                .await
            {
                Ok(result) => result,
                Err(e) if e.to_string().contains("OffsetMismatch") => {
                    warn!("flush_buffer_async: OffsetMismatch on tail flush for inode {}, re-fetching and retrying", ino);
                    let fresh = self.client.get_file_metadata(&path).await?
                        .ok_or_else(|| anyhow::anyhow!("File not found during tail flush retry: {}", path))?;
                    let new_offset = fresh.size;
                    self.metadata_cache.insert(ino, fresh);
                    if let Some(buffer_lock) = self.write_buffers.get(&ino) {
                        let mut buffer = buffer_lock.lock().await;
                        buffer.start_offset = new_offset;
                    }
                    self.client.append_file(file_id, tail_data.clone(), new_offset, None).await?
                }
                Err(e) => return Err(e),
            };

            // Drain tail from buffer
            if let Some(buffer_lock) = self.write_buffers.get(&ino) {
                let mut buffer = buffer_lock.lock().await;
                let drain = tail_data.len().min(buffer.data.len());
                buffer.data.drain(..drain);
                buffer.start_offset += drain as u64;
                buffer.last_modified = SystemTime::now();
            }

            self.metadata_cache.insert(ino, updated_metadata);
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
        let parent_metadata = self.metadata_cache.get(&parent)?;
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

        // Enable aggressive kernel read-ahead for sequential reads (DVR streaming)
        // This tells the kernel to read ahead up to 16MB for sequential access patterns
        config.set_max_readahead(16 * 1024 * 1024);
        info!("Set max_readahead to 16MB for sequential streaming");

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

        // Check if we have fresh metadata from a recent readdir — if so, skip the
        // server round-trip entirely.  readdir populates both metadata_cache and
        // last_metadata_update; a fresh entry here means we just listed this directory
        // and can trust the cached metadata for the lookup TTL window.
        let cached = {
            let path_map = self.path_to_inode.read().unwrap();
            if let Some(&ino) = path_map.get(&path) {
                let fresh = self.last_metadata_update.get(&ino)
                    .map(|t| t.elapsed() < std::time::Duration::from_secs(30))
                    .unwrap_or(false);
                if fresh {
                    self.metadata_cache.get(&ino).map(|m| (ino, m.clone()))
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some((ino, metadata)) = cached {
            let attr = self.metadata_to_attr(ino, &metadata);
            reply.entry(&Duration::from_secs(1), &attr, 0);
            return;
        }

        // Cache miss or stale — fetch from cluster with conditional GET
        let cached_modified_at = {
            let path_map = self.path_to_inode.read().unwrap();
            if let Some(&ino) = path_map.get(&path) {
                self.metadata_cache.get(&ino).map(|m| m.modified_at)
            } else {
                None
            }
        };

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
                reply.entry(&Duration::from_secs(1), &attr, 0);
            }
            Ok(None) => {
                // Either file not found OR metadata not modified (cache still valid)
                if cached_modified_at.is_some() {
                    // Cache is valid, use cached metadata
                    let path_map = self.path_to_inode.read().unwrap();
                    if let Some(&ino) = path_map.get(&path) {
                        if let Some(metadata) = self.metadata_cache.get(&ino) {
                            debug!("Using cached metadata for {} (not modified)", path);
                            let attr = self.metadata_to_attr(ino, &*metadata);
                            reply.entry(&Duration::from_secs(1), &attr, 0);
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

    fn open(&mut self, _req: &FuseRequest, ino: u64, flags: i32, reply: fuser::ReplyOpen) {
        info!("open: ino={}", ino);

        // Track write-mode opens so flush() can skip the write buffer for read-only closes.
        // O_RDONLY == 0; any flag with the low two bits set (O_WRONLY=1, O_RDWR=2) is a write open.
        let is_write = (flags & libc::O_ACCMODE) != libc::O_RDONLY;
        if is_write {
            *self.write_open_counts.entry(ino).or_insert(0) += 1;
        }

        // Check if this is a SQLite database file by looking up its path
        let is_sqlite = self.metadata_cache.get(&ino)
            .map(|m| is_sqlite_direct_io(&m.path))
            .unwrap_or(false);

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

        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let write_buffers = self.write_buffers.clone();
        let write_buffer_enabled = self.write_buffer_enabled;
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            let metadata = metadata_cache.get(&ino).map(|m| m.clone());

            if let Some(mut metadata) = metadata {
                if metadata.file_type == FileType::RegularFile {
                    // Only hit the server if the cached metadata is more than 5 seconds old.
                    // getattr is called every 1s by the kernel for open files; querying the
                    // server on every call generates a connection storm under playback.
                    // 5s is fast enough to notice a file growing (live DVR) without flooding.
                    let should_refresh = match last_metadata_update.get(&ino) {
                        None => true,
                        Some(t) => t.elapsed() >= std::time::Duration::from_secs(5),
                    };

                    if should_refresh {
                        if let Ok(Some(fresh)) = client.get_file_metadata(&metadata.path).await {
                            let server_is_newer = fresh.modified_at > metadata.modified_at
                                || (fresh.modified_at == metadata.modified_at
                                    && (fresh.size > metadata.size
                                        || fresh.chunks.len() > metadata.chunks.len()));
                            if server_is_newer {
                                debug!("getattr: metadata updated: size {} -> {}, chunks {} -> {}",
                                       metadata.size, fresh.size,
                                       metadata.chunks.len(), fresh.chunks.len());
                                metadata_cache.insert(ino, fresh.clone());
                                metadata = fresh;
                            }
                        }
                        last_metadata_update.insert(ino, std::time::Instant::now());
                    }

                    // For files with an active write buffer, the true EOF is further ahead
                    // than the last committed flush position in metadata.size. Report the
                    // buffer's logical end so that Kodi/players seeking in a live recording
                    // see the correct (current) file size instead of a stale flushed size.
                    // Without this, a seek to the "end" of a live file may land beyond what
                    // getattr reports, causing the player to stall waiting for the file to
                    // appear to grow.
                    if write_buffer_enabled {
                        if let Some(buffer_lock) = write_buffers.get(&ino) {
                            let buffer = buffer_lock.lock().await;
                            let buffer_end = buffer.start_offset + buffer.data.len() as u64;
                            if buffer_end > metadata.size {
                                metadata.size = buffer_end;
                            }
                        }
                    }
                }

                let attr = DfsFilesystem::metadata_to_attr_static(ino, &metadata);
                // Use a short TTL for files with an active write buffer (live recordings)
                // so the kernel re-asks us promptly as the file grows. For static files,
                // 5s is fine — it matches our server refresh rate.
                let ttl = if write_buffer_enabled && write_buffers.contains_key(&ino) {
                    Duration::from_millis(500)
                } else {
                    Duration::from_secs(5)
                };
                reply.attr(&ttl, &attr);
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
        let buffer_flush_threshold = self.buffer_flush_threshold;
        let last_metadata_update = self.last_metadata_update.clone();
        let last_warm_offset = self.last_warm_offset.clone();
        let chunk_offset_cache = self.chunk_offset_cache.clone();
        let flush_in_flight = self.flush_in_flight.clone();

        // Spawn async read operation on tokio runtime
        self.runtime.spawn(async move {
            let start = std::time::Instant::now();
            info!("FUSE read START: ino={}, offset={}, size={}", ino, offset, size);

            let mut metadata = match metadata_cache.get(&ino) {
                Some(m) => m.clone(),
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            if metadata.file_type != FileType::RegularFile {
                reply.error(libc::EISDIR);
                return;
            }

            let offset = offset as usize;
            let size = size as usize;

            // Warm replica cache with sliding window - but only occasionally to avoid overhead
            // For files with many small chunks, warming on every read causes significant CPU overhead
            // Warm when: (1) first read, or (2) we've progressed significantly (every 50MB)
            // CRITICAL: Track warming per-inode to prevent cross-file interference during seeks
            let last_warm = last_warm_offset.get(&ino).map(|v| *v).unwrap_or(0);
            let should_warm = offset == 0 || offset.saturating_sub(last_warm as usize) >= 50 * 1024 * 1024;

            if should_warm && !metadata.chunks.is_empty() && !metadata.chunk_sizes.is_empty() {
                // Find which chunk index corresponds to this byte offset
                let mut cumulative = 0u64;
                let mut chunk_idx = 0;
                for (idx, &chunk_size) in metadata.chunk_sizes.iter().enumerate() {
                    if cumulative + chunk_size > offset as u64 {
                        chunk_idx = idx;
                        break;
                    }
                    cumulative += chunk_size;
                }

                // Warm 1000 chunks ahead of current position using actual chunk index
                client.warm_replica_cache_by_index(&metadata.chunks, Some(chunk_idx)).await;

                // Update per-inode warming tracker
                last_warm_offset.insert(ino, offset as u64);
            }

            // Check write buffer first if write-behind buffering is enabled
            // This enables "live rewind" - reading while writing without waiting for backend
            if write_buffer_enabled {
                // Check if read overlaps with write buffer
                let should_flush_buffer = {
                    if let Some(buffer_lock) = write_buffers.get(&ino) {
                        let buffer = buffer_lock.lock().await;
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

                        // If read extends beyond buffer, either flush or serve partial
                        if offset >= buffer_start && offset < buffer_end && read_end > buffer_end {
                            let distance_to_buffer_end = buffer_end - offset;
                            let buffer_size = buffer.data.len();

                            // Calculate current EOF (max of metadata size and buffer end)
                            let current_eof = (buffer_start + buffer_size).max(metadata.size as usize);
                            let is_at_eof = buffer_end >= current_eof;

                            // If we're reading at EOF (live edge), serve what's in buffer without flushing
                            // This prevents hiccups during live TV playback
                            if is_at_eof {
                                let buffer_relative_offset = offset - buffer_start;
                                let available = buffer_end - offset;
                                let data = buffer.data[buffer_relative_offset..buffer_relative_offset + available].to_vec();

                                info!("FUSE read at EOF (live edge): ino={}, offset={}, requested={}, serving={}, buffer=[{}, {}), eof={}",
                                      ino, offset, size, available, buffer_start, buffer_end, current_eof);

                                let elapsed = start.elapsed();
                                info!("FUSE read COMPLETE (EOF): ino={}, offset={}, size={}, took {:?}",
                                      ino, offset, available, elapsed);

                                reply.data(&data);
                                return;
                            }

                            // If read is very close to buffer end AND buffer is reasonably full, flush it
                            // Use cluster-configured chunk size as threshold
                            if distance_to_buffer_end < 65536 && buffer_size >= buffer_flush_threshold {
                                info!("FUSE read near buffer end: ino={}, offset={}, distance={} bytes, buffer_size={} bytes (threshold={}), flushing",
                                      ino, offset, distance_to_buffer_end, buffer_size, buffer_flush_threshold);
                                true
                            } else if distance_to_buffer_end < 65536 {
                                // Buffer too small to flush - serve partial read to avoid tiny chunks
                                let buffer_relative_offset = offset - buffer_start;
                                let available = buffer_end - offset;
                                let data = buffer.data[buffer_relative_offset..buffer_relative_offset + available].to_vec();

                                info!("FUSE read partial (buffer too small to flush): ino={}, offset={}, requested={}, serving={}, buffer_size={}",
                                      ino, offset, size, available, buffer_size);

                                let elapsed = start.elapsed();
                                info!("FUSE read COMPLETE (partial): ino={}, offset={}, size={}, took {:?}",
                                      ino, offset, available, elapsed);

                                reply.data(&data);
                                return;
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

                // Note: we intentionally do NOT wait for an in-flight fsync flush here.
                // Waiting (spin-polling) blocks the single FUSE thread and freezes the
                // entire mount — no reads, no getattrs, nothing — for up to 5 seconds
                // while a slow AppendFile completes on a loaded node.
                // The data being read is almost always already on the server (the player
                // reads behind the write head during live recordings). If a chunk genuinely
                // isn't there yet, the normal server read retry path handles it.

                // Spawn background flush if needed (don't block read operation)
                if should_flush_buffer {
                    info!("Spawning background flush for ino={} due to boundary read (non-blocking)", ino);
                    let buffer_opt = write_buffers.remove(&ino).map(|(_, v)| v);

                    if let Some(buffer_lock) = buffer_opt {
                        let buffer = buffer_lock.lock().await.clone();
                        let client_clone = client.clone();
                        let metadata_cache_clone = metadata_cache.clone();
                        let last_metadata_update_clone = last_metadata_update.clone();

                        // Spawn flush in background, don't wait for it
                        tokio::spawn(async move {
                            let buffer_start = buffer.start_offset;
                            let buffer_size = buffer.data.len();

                            // Write buffered data
                            match client_clone.write_data_with_cache(&buffer.data, ino, buffer_start).await {
                                Ok((new_chunk_ids, new_chunk_sizes, chunk_locations_opt)) => {
                                    let new_size = buffer_start + buffer_size as u64;

                                    // Update metadata
                                    let mut meta = metadata_cache_clone.get(&ino).map(|m| m.clone());
                                    if let Some(mut m) = meta {
                                        if let Some(chunk_locations) = chunk_locations_opt {
                                            m.chunk_locations.extend(chunk_locations);
                                        }
                                        m.chunks.extend(new_chunk_ids);
                                        m.chunk_sizes.extend(new_chunk_sizes);
                                        m.size = new_size;
                                        m.modified_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

                                        // Boundary flush during read: use time-based batching
                                        let should_update_meta = match last_metadata_update_clone.get(&ino) {
                                            None => true,
                                            Some(last) => last.elapsed() >= std::time::Duration::from_secs(2)
                                        };

                                        if should_update_meta {
                                            if let Err(e) = client_clone.put_file_metadata(&m).await {
                                                error!("Failed to update metadata after background flush: {}", e);
                                            } else {
                                                last_metadata_update_clone.insert(ino, std::time::Instant::now());
                                                metadata_cache_clone.insert(ino, m.clone());
                                                info!("Background flush complete: {} bytes at offset {}, new size {} (metadata updated)", buffer_size, buffer_start, new_size);
                                            }
                                        } else {
                                            // Skip metadata update but still update cache
                                            metadata_cache_clone.insert(ino, m.clone());
                                            info!("Background flush complete: {} bytes at offset {}, new size {} (metadata batched)", buffer_size, buffer_start, new_size);
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to flush buffer in background: {}", e);
                                }
                            }
                        });
                        info!("Background flush spawned for ino={}, continuing with read immediately", ino);
                    }
                }
            }

            // Early return for out of bounds
            // But first, check if file might have grown by refreshing metadata
            if offset >= metadata.size as usize {
                // File might be actively growing (e.g., live recording in progress).
                // Rate-limit the metadata refresh so Kodi's frequent seek-past-EOF
                // polls don't hammer the server.  We allow one refresh per second per
                // inode; between refreshes we return empty immediately.
                let should_refresh = match last_metadata_update.get(&ino) {
                    None => true,
                    Some(last) => last.elapsed() >= std::time::Duration::from_secs(1),
                };

                if !should_refresh {
                    debug!("Read at offset {} >= size {}, rate-limiting EOF refresh, returning empty",
                           offset, metadata.size);
                    reply.data(&[]);
                    return;
                }

                info!("Read at offset {} >= cached size {}, refreshing metadata from server", offset, metadata.size);
                last_metadata_update.insert(ino, std::time::Instant::now());

                match client.get_file_metadata(&metadata.path).await {
                    Ok(Some(mut fresh_metadata)) => {
                        // Still past EOF even after refresh — return empty immediately,
                        // no need to fetch the chunk map (there's nothing new to read).
                        if offset >= fresh_metadata.size as usize {
                            info!("Still at EOF after refresh: offset {} >= size {}", offset, fresh_metadata.size);
                            metadata_cache.insert(ino, fresh_metadata);
                            reply.data(&[]);
                            return;
                        }

                        // File has grown past our read offset.  Now fetch the chunk map
                        // so we have accurate replica locations for the new chunks.
                        let prev_modified_at = metadata.modified_at;
                        if fresh_metadata.modified_at != prev_modified_at || fresh_metadata.chunk_locations.is_empty() {
                            match client.get_file_chunk_map(fresh_metadata.id).await {
                                Ok((locations, _map_modified_at)) => {
                                    if !locations.is_empty() {
                                        info!("Refreshed chunk map from leader: {} locations for file {}",
                                              locations.len(), fresh_metadata.path);
                                        fresh_metadata.chunk_locations = locations;
                                        chunk_offset_cache.remove(&ino);
                                    }
                                }
                                Err(e) => {
                                    debug!("Could not refresh chunk map from leader at EOF ({}), using metadata chunk_locations", e);
                                }
                            }
                        }

                        // Update cache with fresh metadata
                        metadata_cache.insert(ino, fresh_metadata.clone());

                        // Warm replica cache for chunks ahead of the current read position
                        let chunk_idx = if !fresh_metadata.chunk_sizes.is_empty() {
                            let mut cumulative = 0u64;
                            let mut idx = 0;
                            for (i, &size) in fresh_metadata.chunk_sizes.iter().enumerate() {
                                if cumulative + size as u64 > offset as u64 {
                                    idx = i;
                                    break;
                                }
                                cumulative += size as u64;
                                idx = i + 1;
                            }
                            Some(idx)
                        } else {
                            None
                        };
                        client.warm_replica_cache_by_index(&fresh_metadata.chunks, chunk_idx).await;

                        info!("File grew from {} to {} bytes, continuing read", metadata.size, fresh_metadata.size);
                        metadata = fresh_metadata;
                    }
                    Ok(None) => {
                        info!("File not found when refreshing metadata");
                        reply.error(libc::ENOENT);
                        return;
                    }
                    Err(e) => {
                        // Server unreachable — return empty rather than blocking
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

            // Build or retrieve cached chunk offset map
            // CRITICAL: Cache this to avoid O(n) iteration through all chunks on every read
            // For 3GB files with 750+ chunks, this was a massive performance bottleneck
            //
            // SPARSE FILE SUPPORT: Use chunk_locations with file_offset if available,
            // otherwise fall back to sequential offset calculation for legacy files
            let chunk_offsets = {
                // Check cache without holding a ref across the potential insert
                let cached = chunk_offset_cache.get(&ino).and_then(|entry| {
                    let (cached_size, cached_chunk_count, ref cached_offsets) = *entry;
                    if cached_size == metadata.size && cached_chunk_count == metadata.chunks.len() {
                        Some(cached_offsets.clone())
                    } else {
                        None
                    }
                });

                if let Some(offsets) = cached {
                    offsets
                } else {
                    // Cache miss or invalidated - build and cache it
                    let mut offsets = Vec::with_capacity(metadata.chunks.len());

                    // Check if we have chunk_locations with file_offset (sparse file support)
                    // IMPORTANT: chunk_locations count must match chunks count, otherwise offsets
                    // would be misaligned (e.g., if a small chunk was written without chunk_locations
                    // being populated). Fall back to sequential calculation in that case.
                    let locations_match = !metadata.chunk_locations.is_empty()
                        && metadata.chunk_locations.len() == metadata.chunks.len()
                        && metadata.chunk_locations[0].file_offset.is_some();

                    if locations_match {
                        // SPARSE FILE: Use explicit file_offset from chunk_locations
                        for location in &metadata.chunk_locations {
                            let chunk_offset = location.file_offset.unwrap_or(0);
                            offsets.push((chunk_offset as usize, location.size));
                        }
                    } else {
                        // LEGACY FILE or mismatched chunk_locations: Sequential chunks, calculate offsets
                        if !metadata.chunk_locations.is_empty()
                            && metadata.chunk_locations.len() != metadata.chunks.len()
                        {
                            warn!("chunk_locations count ({}) != chunks count ({}) for ino={}, using sequential offsets",
                                  metadata.chunk_locations.len(), metadata.chunks.len(), ino);
                        }
                        let mut current_offset = 0usize;
                        for &chunk_size in metadata.chunk_sizes.iter() {
                            offsets.push((current_offset, chunk_size as usize));
                            current_offset += chunk_size as usize;
                        }
                    }

                    // Store in cache
                    chunk_offset_cache.insert(ino, (metadata.size, metadata.chunks.len(), offsets.clone()));

                    offsets
                }
            };

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

            // SPARSE FILE SUPPORT: If no chunks found in this range, it's a hole!
            // Return zeros for the requested size (up to file size)
            if chunks_to_read.is_empty() {
                let bytes_to_read = end_offset.saturating_sub(offset);
                if bytes_to_read > 0 {
                    debug!("Reading from hole (unmapped region): offset {} size {} - returning zeros",
                           offset, bytes_to_read);
                    let zeros = vec![0u8; bytes_to_read];
                    reply.data(&zeros);
                } else {
                    reply.data(&[]);
                }
                return;
            }

            debug!("Reading {} chunks (indices {:?}) for offset {} size {}",
                   chunks_to_read.len(),
                   chunks_to_read.iter().map(|(idx, _, _)| idx).collect::<Vec<_>>(),
                   offset, size);

            // Build ChunkReadHints to tell the client how to read each chunk
            // For seeks (non-sequential reads), we can optimize by only fetching needed portions
            let read_hints: Vec<crate::client::ChunkReadHint> = chunks_to_read
                .iter()
                .map(|(idx, chunk_start, chunk_size)| {
                    let chunk_end = chunk_start + chunk_size;

                    // Calculate the overlap between requested range and this chunk
                    let read_start_in_file = offset.max(*chunk_start);
                    let read_end_in_file = end_offset.min(chunk_end);

                    // Calculate offset and length within the chunk
                    let offset_in_chunk = read_start_in_file.saturating_sub(*chunk_start);
                    let length_in_chunk = read_end_in_file.saturating_sub(read_start_in_file);

                    // Use partial reads when the request covers only a small fraction of the chunk.
                    // This is critical for Kodi seeks: it probes random offsets to detect duration,
                    // and fetching a full 4MB chunk over a 20Mbit link takes ~1.6s per probe.
                    // Only use partial reads when reading < 25% of the chunk to avoid overhead
                    // of multiple round trips for sequential streaming.
                    let full_chunk = length_in_chunk >= (*chunk_size as usize / 4)
                        || length_in_chunk == 0;

                    crate::client::ChunkReadHint {
                        chunk_idx: *idx,
                        chunk_id: metadata.chunks[*idx],
                        full_chunk,
                        offset_in_chunk,
                        length: length_in_chunk,
                        file_offset: *chunk_start as u64,
                    }
                })
                .collect();

            let partial_reads = read_hints.iter().filter(|h| !h.full_chunk).count();
            if partial_reads > 0 {
                info!("Optimizing seek: {} partial chunk reads out of {}",
                      partial_reads, read_hints.len());
            }

            // For SQLite database files, disable caching by passing inode=0
            // This prevents stale cached data from causing corruption
            let cache_inode = {
                let path = &metadata.path;
                if is_sqlite_path(path) {
                    0 // Disable caching for SQLite files
                } else {
                    ino // Enable caching for other files
                }
            };

            let all_chunks = metadata.chunks.clone();
            // If metadata has no chunk_locations (legacy file or first read), fetch the full
            // chunk map from the leader in one round-trip instead of per-chunk fallback queries.
            let chunk_locations = if metadata.chunk_locations.is_empty() && !all_chunks.is_empty() {
                match client.get_file_chunk_map(metadata.id).await {
                    Ok((locations, _)) if !locations.is_empty() => {
                        info!("Fetched chunk map from leader: {} locations for {}", locations.len(), metadata.path);
                        // Cache the locations in metadata so subsequent reads skip this query
                        let mut updated = metadata.clone();
                        updated.chunk_locations = locations.clone();
                        metadata_cache.insert(ino, updated);
                        locations
                    }
                    Ok(_) => {
                        debug!("Leader returned empty chunk map for {}, using per-chunk fallback", metadata.path);
                        metadata.chunk_locations.clone()
                    }
                    Err(e) => {
                        debug!("Could not fetch chunk map from leader for {} ({}), using per-chunk fallback", metadata.path, e);
                        metadata.chunk_locations.clone()
                    }
                }
            } else {
                metadata.chunk_locations.clone()
            };
            let result = client.read_data(&read_hints, &all_chunks, cache_inode, &chunk_locations).await;

            let chunk_data = match result {
                Ok(data) => data,
                Err(e) => {
                    error!("Failed to read {} chunks: {}", read_hints.len(), e);
                    reply.error(libc::EIO);
                    return;
                }
            };

            // SPARSE FILE SUPPORT: Check if we need to handle holes
            // Holes can appear: (a) between chunks, (b) before the first chunk
            // (e.g. shm file truncated to N bytes then written at higher offsets)
            let has_holes = {
                // Check for leading hole: first chunk doesn't start at requested offset
                let leading_hole = if let Some(&(_, first_start, _)) = chunks_to_read.first() {
                    first_start > offset
                } else {
                    false
                };

                // Check for gaps between consecutive chunks
                let inter_chunk_gap = if chunks_to_read.len() > 1 {
                    let mut has_gap = false;
                    for i in 0..chunks_to_read.len() - 1 {
                        let (_, curr_start, curr_size) = chunks_to_read[i];
                        let (_, next_start, _) = chunks_to_read[i + 1];
                        if curr_start + curr_size < next_start {
                            has_gap = true;
                            break;
                        }
                    }
                    has_gap
                } else {
                    false
                };

                leading_hole || inter_chunk_gap
            };

            let final_data = if has_holes {
                // SPARSE FILE: Build result buffer with holes filled with zeros
                debug!("Sparse file read: filling holes with zeros");

                let bytes_needed = end_offset - offset;
                let mut result_buffer = vec![0u8; bytes_needed];

                let mut chunk_data_offset = 0usize;
                for (i, (_, chunk_start, chunk_size)) in chunks_to_read.iter().enumerate() {
                    // Determine how many bytes were actually returned for this chunk.
                    // Partial reads return only hint.length bytes, not the full chunk_size.
                    let actual_bytes_returned = if let Some(hint) = read_hints.get(i) {
                        if hint.full_chunk { *chunk_size } else { hint.length }
                    } else {
                        *chunk_size
                    };

                    // Calculate where this chunk's data should go in the result
                    let chunk_end = chunk_start + chunk_size;

                    // Find overlap between requested range [offset, end_offset) and chunk [chunk_start, chunk_end)
                    let overlap_start = (*chunk_start).max(offset);
                    let overlap_end = chunk_end.min(end_offset);

                    if overlap_start < overlap_end {
                        let result_offset = overlap_start - offset;
                        // For partial reads, data starts at byte 0 of what was returned (the hint's
                        // offset_in_chunk was already sent to the server as the range start).
                        let chunk_offset = if let Some(hint) = read_hints.get(i) {
                            if hint.full_chunk {
                                overlap_start - chunk_start
                            } else {
                                // Data returned starts at hint.offset_in_chunk in the logical chunk,
                                // but is at byte 0 in the returned buffer slice.
                                overlap_start.saturating_sub(chunk_start + hint.offset_in_chunk)
                            }
                        } else {
                            overlap_start - chunk_start
                        };
                        let overlap_size = overlap_end - overlap_start;

                        let src_start = chunk_data_offset + chunk_offset;
                        let src_end = src_start + overlap_size;
                        if src_end <= chunk_data.len() {
                            result_buffer[result_offset..result_offset + overlap_size]
                                .copy_from_slice(&chunk_data[src_start..src_end]);
                        } else {
                            warn!("Sparse read: chunk {} data out of bounds (src {}..{} > buf {}), skipping",
                                  i, src_start, src_end, chunk_data.len());
                        }
                    }

                    chunk_data_offset += actual_bytes_returned;
                }

                result_buffer
            } else {
                // NON-SPARSE FILE: Use simple offset calculation (existing logic)
                // For partial reads, data starts at hint.offset_in_chunk in the logical chunk
                // but at byte 0 in the returned buffer, so adjust accordingly.
                //
                // IMPORTANT: If the hint requested a partial read but the cache served a full
                // chunk (chunk_data.len() > hint.length), treat it as a full-chunk result so
                // we index from the chunk start rather than the partial-read offset.
                let first_hint_offset = read_hints.first()
                    .map(|h| {
                        if h.full_chunk {
                            0
                        } else if chunk_data.len() > h.length {
                            // Cache served a larger (full) chunk despite partial hint — index from chunk start
                            0
                        } else {
                            h.offset_in_chunk
                        }
                    })
                    .unwrap_or(0);
                let offset_in_data = offset.saturating_sub(first_chunk_offset + first_hint_offset);
                let data_end = std::cmp::min(offset_in_data + size, chunk_data.len());

                if offset_in_data >= chunk_data.len() {
                    debug!("Read offset {} beyond data length {}", offset_in_data, chunk_data.len());
                    Vec::new()
                } else {
                    chunk_data[offset_in_data..data_end].to_vec()
                }
            };

            if final_data.is_empty() {
                reply.data(&[]);
            } else {
                debug!("Returning {} bytes from offset {} (read {} chunks, has_holes: {})",
                       final_data.len(), offset, read_hints.len(), has_holes);
                reply.data(&final_data);
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
            match self.metadata_cache.get(&ino) {
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

        // Check directory cache first (30-second TTL)
        let cached_entries = self.dir_cache.get(&path).and_then(|entry| {
            let (entries, timestamp) = &*entry;
            if timestamp.elapsed() < std::time::Duration::from_secs(30) {
                debug!("Directory cache HIT for {}", path);
                Some(entries.clone())
            } else {
                debug!("Directory cache EXPIRED for {}", path);
                None
            }
        });

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
                    dir_cache.insert(path.clone(), (entries.clone(), std::time::Instant::now()));
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

            // Mark metadata as just-refreshed so getattr skips the per-file server
            // round-trip on the immediately following `ls -alh`. Without this, each
            // of the 25 getattr calls would hit the server serially (~130ms each).
            self.last_metadata_update.insert(entry_ino, std::time::Instant::now());

            let next_offset = 3 + i as i64;  // 3 because . is 1, .. is 2, first file is 3
            if reply.add(entry_ino, next_offset, kind, file_name) {
                break; // Buffer full
            }
        }

        let elapsed = start.elapsed();
        info!("readdir COMPLETE: {} with {} entries in {:?}", path, entries.len(), elapsed);
        reply.ok();

        // Prefetch subdirectory listings in the background so the next level of
        // readdir calls (e.g. DVR indexer walking show → episode directories) are
        // instant cache hits.  Fire-and-forget — we don't wait for these.
        let subdirs: Vec<String> = entries.iter()
            .filter(|e| e.file_type == FileType::Directory)
            .map(|e| e.path.clone())
            .collect();

        if !subdirs.is_empty() {
            let client = self.client.clone();
            let dir_cache = self.dir_cache.clone();
            let metadata_cache = self.metadata_cache.clone();
            let path_to_inode = self.path_to_inode.clone();
            let next_inode = self.next_inode.clone();
            let last_metadata_update = self.last_metadata_update.clone();
            self.runtime.spawn(async move {
                // Fetch all subdirs concurrently
                let futures: Vec<_> = subdirs.into_iter().map(|subdir| {
                    let client = client.clone();
                    let dir_cache = dir_cache.clone();
                    let metadata_cache = metadata_cache.clone();
                    let path_to_inode = path_to_inode.clone();
                    let next_inode = next_inode.clone();
                    let last_metadata_update = last_metadata_update.clone();
                    async move {
                        // Skip if already cached and fresh
                        {
                            if let Some(entry) = dir_cache.get(&subdir) {
                                if entry.1.elapsed() < std::time::Duration::from_secs(29) {
                                    return;
                                }
                            }
                        }
                        let fetch_start = std::time::Instant::now();
                        if let Ok(sub_entries) = client.list_directory(&subdir).await {
                            // Only cache if the directory hasn't been invalidated while we
                            // were fetching.  An invalidation removes the entry from the cache
                            // (e.g. a concurrent create/unlink in that directory); reinserting
                            // stale prefetch results would hide the new file from the next
                            // readdir until the 30-second TTL expires.
                            let still_valid = match dir_cache.get(&subdir) {
                                Some(entry) => entry.1 < fetch_start, // entry predates our fetch
                                None => false, // was invalidated — do not reinsert
                            };
                            if still_valid {
                                dir_cache.insert(
                                    subdir.clone(),
                                    (sub_entries.clone(), std::time::Instant::now()),
                                );
                            }
                            // Cache metadata for each entry so getattr is instant too
                            let now = std::time::Instant::now();
                            for entry in &sub_entries {
                                let ino = {
                                    let mut path_map = path_to_inode.write().unwrap();
                                    if let Some(&existing) = path_map.get(&entry.path) {
                                        existing
                                    } else {
                                        let mut next = next_inode.write().unwrap();
                                        let ino = *next;
                                        *next += 1;
                                        path_map.insert(entry.path.clone(), ino);
                                        ino
                                    }
                                };
                                metadata_cache.insert(ino, entry.clone());
                                last_metadata_update.insert(ino, now);
                            }
                            debug!("Prefetched {} entries for {}", sub_entries.len(), subdir);
                        }
                    }
                }).collect();
                futures::future::join_all(futures).await;
            });
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
            chunk_locations: Vec::new(),
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
                self.metadata_cache.insert(ino, metadata.clone());

                // CRITICAL: Invalidate parent directory cache so 'ls' shows new file immediately
                let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                self.dir_cache.remove(parent_path);
                debug!("Invalidated directory cache for parent: {}", parent_path);

                // create() always opens for writing — count it
                *self.write_open_counts.entry(ino).or_insert(0) += 1;

                // Convert to FUSE attr
                let attr = self.metadata_to_attr(ino, &metadata);

                // For SQLite files: use direct I/O to bypass page cache.
                // This ensures reads see fresh data (no stale page cache).
                // EXCEPTION: .db-shm must NOT use direct I/O — SQLite mmaps it
                // (MAP_SHARED) for WAL index coordination. FOPEN_DIRECT_IO would
                // cause mmap to return ENODEV → SQLITE_IOERR.
                // SQLite pre-initializes the shm with sparse writes before mmap,
                // so the page cache will have valid data when mmap is called.
                let open_flags = if is_sqlite_direct_io(&path) {
                    fuser::consts::FOPEN_DIRECT_IO
                } else {
                    0
                };

                // ReplyCreate expects: ttl, attr, generation, fh, flags
                reply.created(&Duration::from_secs(300), &attr, 0, 0, open_flags);
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
        let last_metadata_update = self.last_metadata_update.clone();
        let data_vec = data.to_vec(); // Copy data before moving

        // Execute write operation synchronously to preserve write order
        // This ensures proper sequencing and prevents corruption
        {
            let start = std::time::Instant::now();
            debug!("write: ino={}, offset={}, size={}", ino, offset, data_vec.len());

            let mut metadata = match metadata_cache.get(&ino) {
                Some(m) => m.clone(),
                None => {
                    // Metadata cache miss — the kernel may have sent open() with a cached
                    // inode without going through lookup() first (FOPEN_KEEP_CACHE survives
                    // client restarts).  Fetch from the server and populate the cache so
                    // the write path has a FileId and correct start_offset.
                    let path_opt = {
                        let map = self.path_to_inode.read().unwrap();
                        map.iter().find(|(_, &v)| v == ino).map(|(k, _)| k.clone())
                    };
                    if let Some(path) = path_opt {
                        match runtime.block_on(client.get_file_metadata(&path)) {
                            Ok(Some(fetched)) => {
                                metadata_cache.insert(ino, fetched.clone());
                                self.last_metadata_update.insert(ino, std::time::Instant::now());
                                fetched
                            }
                            Ok(None) => {
                                // File doesn't exist on server yet — new file, create minimal record
                                let new_meta = dfs_common::FileMetadata {
                                    id: dfs_common::FileId::new(),
                                    path: path.clone(),
                                    size: 0,
                                    chunks: Vec::new(),
                                    chunk_sizes: Vec::new(),
                                    chunk_locations: Vec::new(),
                                    created_at: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default().as_secs(),
                                    modified_at: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default().as_secs(),
                                    mode: 0o644,
                                    uid: _req.uid(),
                                    gid: _req.gid(),
                                    file_type: dfs_common::FileType::RegularFile,
                                };
                                info!("write: inode {} has no server metadata, creating new record for {}", ino, path);
                                metadata_cache.insert(ino, new_meta.clone());
                                new_meta
                            }
                            Err(e) => {
                                error!("write: failed to fetch metadata for inode {}: {}", ino, e);
                                reply.error(libc::EIO);
                                return;
                            }
                        }
                    } else {
                        error!("write: no path known for inode {}, returning ENOENT", ino);
                        reply.error(libc::ENOENT);
                        return;
                    }
                }
            };

            if metadata.file_type != FileType::RegularFile {
                reply.error(libc::EISDIR);
                return;
            }

            // For SQLite database files, disable caching AND buffering to prevent corruption
            // SQLite does read-your-own-writes and needs immediate consistency
            // .db-shm is excluded: it's mmapped by SQLite and managed via page cache
            let is_sqlite = is_sqlite_path(&metadata.path);
            let cache_inode = if is_sqlite { 0 } else { ino };

            // Write-behind buffering: buffer sequential appends in memory
            // EXCEPT for SQLite files which need immediate write-through for consistency
            if write_buffer_enabled && !is_sqlite {
                let offset_usize = offset as usize;
                // Calculate true current size including any buffered data
                // IMPORTANT: Re-read metadata from cache to get latest updates from concurrent writes
                let current_size = {
                    let cache_size = metadata_cache.get(&ino)
                        .map(|m| m.size as usize)
                        .unwrap_or(metadata.size as usize);

                    if let Some(buffer_lock) = write_buffers.get(&ino) {
                        let buffer = runtime.block_on(async { buffer_lock.lock().await });
                        ((buffer.start_offset + buffer.data.len() as u64) as usize)
                            .max(cache_size)
                    } else {
                        cache_size
                    }
                };

                debug!("Buffered write check: offset={}, current_size={}, cache_size={}, buffer_present={}",
                       offset_usize, current_size,
                       metadata_cache.get(&ino).map(|m| m.size).unwrap_or(0),
                       write_buffers.contains_key(&ino));

                // Handle different write patterns:
                // 1. Sequential appends (offset == current_size) → use buffering for performance
                // 2. Overwrites (offset < current_size) → flush buffer first, then fall through to RMW
                // 3. Sparse writes (offset > current_size) → write directly with hole support
                let is_sequential_append = offset_usize == current_size;
                let is_overwrite = offset_usize < current_size;
                let is_sparse_write = offset_usize > current_size;

                // Only flush the append buffer before an overwrite if the write actually
                // overlaps with the buffered region. A Kodi-style metadata write at offset 0
                // while the DVR is recording at offset 200MB does NOT overlap — flushing in
                // that case produces a mis-aligned partial chunk and disrupts the recording.
                let overwrite_overlaps_buffer = if is_overwrite {
                    if let Some(buffer_lock) = write_buffers.get(&ino) {
                        let buffer = runtime.block_on(async { buffer_lock.lock().await });
                        let write_end = offset_usize + data.len();
                        write_end > buffer.start_offset as usize
                    } else {
                        false
                    }
                } else {
                    false
                };

                if is_overwrite && write_buffers.contains_key(&ino) && overwrite_overlaps_buffer {
                    // OVERWRITE overlaps buffered region: flush the buffer first so the
                    // read-modify-write below sees the complete committed file. Without this,
                    // the splice recalculates metadata.size = sum(flushed chunks) which
                    // discards the buffered tail → file truncation.
                    info!("Overwrite at offset={} overlaps buffer (current_size={}): flushing buffer first",
                          offset_usize, current_size);
                    if let Err(e) = runtime.block_on(self.flush_buffer_async(ino, true)) {
                        error!("Failed to flush buffer before overwrite for inode {}: {}", ino, e);
                        reply.error(libc::EIO);
                        return;
                    }
                    // Fall through to the direct random-write path below (no buffering)
                }

                if is_sparse_write {
                    let gap = offset_usize - current_size;

                    // Small gaps (< 64KB) are DVR/MPEG-TS packet padding — absorb into the
                    // write buffer by zero-filling the gap so buffering stays intact.
                    // Large gaps are genuine sparse writes and bypass buffering as before.
                    const SMALL_GAP_THRESHOLD: usize = 64 * 1024;
                    if gap < SMALL_GAP_THRESHOLD {
                        info!("Near-sequential write: offset={} current_size={} gap={} bytes — zero-filling into buffer",
                              offset_usize, current_size, gap);
                        // Build padded data: zeros for the gap + the actual write data
                        let mut padded = vec![0u8; gap];
                        padded.extend_from_slice(&data_vec);
                        // Re-route through the sequential append path by adjusting data_vec
                        // and treating offset as current_size (now effectively sequential).
                        // We do this by appending to the write buffer directly.
                        let write_buffers_clone2 = write_buffers.clone();
                        let client_clone3 = client.clone();
                        let metadata_cache_clone4 = metadata_cache.clone();
                        let metadata_cache_clone5 = metadata_cache.clone();
                        let metadata_cache_clone6 = metadata_cache.clone();

                        let write_counters2 = write_counters.clone();

                        let padded_len = padded.len();
                        let buffer_flush_threshold2 = self.buffer_flush_threshold;

                        let gap_result = runtime.block_on(async move {
                            let buffer_lock = write_buffers_clone2.entry(ino).or_insert_with(|| {
                                let cache_size = {
                                    metadata_cache_clone6.get(&ino).map(|m| m.size as u64).unwrap_or(current_size as u64)
                                };
                                Arc::new(Mutex::new(WriteBuffer {
                                    data: Vec::new(),
                                    last_modified: SystemTime::now(),
                                    start_offset: cache_size,
                                    created_at: std::time::Instant::now(),
                                    preferred_primary: None,
                                }))
                            }).clone();

                            let mut buffer = buffer_lock.lock().await;
                            // Server-side AppendFile handles chunk alignment — no client re-alignment needed.

                            buffer.data.extend_from_slice(&padded);
                            buffer.last_modified = SystemTime::now();
                            let should_flush = buffer.data.len() >= buffer_flush_threshold2;
                            Ok::<(bool, usize), anyhow::Error>((should_flush, padded_len))
                        });

                        match gap_result {
                            Ok((_flush_now, _)) => {
                                {
                                    let mut counters = write_counters2.write().unwrap();
                                    let c = counters.entry(ino).or_insert(0);
                                    *c += 1;
                                }
                                // Background flusher handles draining — no inline network I/O.
                                reply.written(data_vec.len() as u32);
                                return;
                            }
                            Err(e) => {
                                error!("Failed to buffer gap-fill write for inode {}: {}", ino, e);
                                reply.error(libc::EIO);
                                return;
                            }
                        }
                    }

                    // TRUE SPARSE WRITE: Large gap, write directly (hole support)
                    info!("Sparse write: offset {} > current_size {} (gap: {} bytes)",
                           offset_usize, current_size, gap);

                    // Write directly at the specified offset (no buffering for sparse writes)
                    let write_result = runtime.block_on(async {
                        // Write data with file_offset tracking (Phase 1 support)
                        client.write_data_with_cache(&data_vec, ino, offset as u64).await
                    });

                    match write_result {
                        Ok((new_chunk_ids, new_chunk_sizes, chunk_locations_opt)) => {
                            // Update file size to max(current, offset + len) for sparse files
                            let new_size = (offset_usize + data.len()).max(current_size);

                            // Update metadata with new chunks
                            let mut metadata = metadata_cache.get(&ino).map(|m| m.clone()).unwrap_or_else(|| metadata.clone());

                            if let Some(chunk_locations) = chunk_locations_opt {
                                metadata.chunk_locations.extend(chunk_locations);
                            }
                            metadata.chunks.extend(new_chunk_ids);
                            metadata.chunk_sizes.extend(new_chunk_sizes);
                            metadata.size = new_size as u64;
                            metadata.modified_at = SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_secs();

                            // Store updated metadata
                            if let Err(e) = runtime.block_on(async {
                                client.put_file_metadata(&metadata).await
                            }) {
                                error!("Failed to store metadata after sparse write: {}", e);
                                reply.error(libc::EIO);
                                return;
                            }

                            // Update cache
                            metadata_cache.insert(ino, metadata);

                            info!("Sparse write complete: ino={}, offset={}, len={}, new_size={}",
                                  ino, offset, data.len(), new_size);
                            reply.written(data.len() as u32);
                        }
                        Err(e) => {
                            error!("Sparse write failed for inode {}: {}", ino, e);
                            reply.error(libc::EIO);
                        }
                    }
                    return;
                }

                // SEQUENTIAL APPEND (buffered for performance)
                if is_sequential_append {
                    // Buffer size threshold: 4MB to match server chunk_size for optimal performance
                    // With environment variable override support
                    // Use buffer flush threshold from cluster config (or env var override)
                    let buffer_flush_threshold: usize = std::env::var("DFS_WRITE_BUFFER_SIZE")
                        .ok()
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(self.buffer_flush_threshold); // Use cluster-configured chunk size

                    let write_buffers_clone = write_buffers.clone();
                    let client_clone = client.clone();
                    let metadata_cache_clone2 = metadata_cache.clone();
                    let metadata_cache_clone3 = metadata_cache.clone();
                    let data_slice = &data_vec[..];

                    let should_flush = runtime.block_on(async move {
                        // Back-pressure: stall only if the buffer is over the RAM cap.
                        // fsync flushes run concurrently in the background; OffsetMismatch
                        // retries handle any ordering race — no need to stall writes here.
                        // Hard cap: 1.5× the flush threshold (18MB for 4MB chunks).
                        // The flusher triggers at buffer_flush_threshold (12MB = 3 chunks).
                        // The cap sits above that to absorb one slow-node stall without EIO,
                        // but stays tight enough to avoid OOM on low-RAM clients (nanopir3: 1.9GB).
                        let max_buffer_size: usize = buffer_flush_threshold + buffer_flush_threshold / 2;
                        let stall_start = std::time::Instant::now();
                        loop {
                            let buf_len = if let Some(entry) = write_buffers_clone.get(&ino) {
                                entry.lock().await.data.len()
                            } else {
                                0
                            };
                            if buf_len < max_buffer_size {
                                break;
                            }
                            if stall_start.elapsed() > std::time::Duration::from_secs(10) {
                                error!("Write buffer stall timeout for inode {}: {} bytes in buffer, cluster not draining", ino, buf_len);
                                return Err(anyhow::anyhow!("Write buffer stall timeout"));
                            }
                            debug!("Back-pressure stall: inode {} buffer {} bytes (cap {}), waiting for flusher",
                                   ino, buf_len, max_buffer_size);
                            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                        }

                        // Get or create buffer lock for this inode
                        let buffer_lock = write_buffers_clone.entry(ino).or_insert_with(|| {
                            // Calculate initial size
                            let cache_size = metadata_cache_clone3.get(&ino)
                                .map(|m| m.size as u64).unwrap_or(current_size as u64);

                            Arc::new(Mutex::new(WriteBuffer {
                                data: Vec::new(),
                                last_modified: SystemTime::now(),
                                start_offset: cache_size,
                                created_at: std::time::Instant::now(),
                                preferred_primary: None,
                            }))
                        }).clone();

                        let mut buffer = buffer_lock.lock().await;
                        // Server-side AppendFile handles chunk alignment — no client re-alignment needed.
                        // This ensures data isn't lost

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

                            // Data is now in the write buffer. The background flusher
                            // (running every 500ms) will drain it to the cluster as soon
                            // as a full 4MB chunk is ready. write() does no network I/O.
                            let _ = flush_now; // flushing is handled by background flusher
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
                let cache_size = metadata_cache.get(&ino)
                    .map(|m| m.size as usize).unwrap_or(metadata.size as usize);

                let buffer_end = runtime.block_on(async {
                    if let Some(buffer_lock) = write_buffers.get(&ino) {
                        let buffer = buffer_lock.lock().await;
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
                    // Build read hints for affected chunks (full chunk reads for read-modify-write)
                    let mut read_hints = Vec::with_capacity(affected_chunk_ids.len());
                    let mut current_offset = first_chunk_file_offset;
                    for (i, &chunk_id) in affected_chunk_ids.iter().enumerate() {
                        let chunk_size = affected_chunk_sizes[i] as usize;
                        read_hints.push(crate::client::ChunkReadHint {
                            chunk_idx: first_idx + i,
                            chunk_id,
                            full_chunk: true,  // Always read full chunks for read-modify-write
                            offset_in_chunk: 0,
                            length: chunk_size,
                            file_offset: current_offset,
                        });
                        current_offset += chunk_size as u64;
                    }

                    let affected_data = match runtime.block_on(async {
                        client.read_data(&read_hints, &chunk_ids, cache_inode, &metadata.chunk_locations).await
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
                Ok((new_chunk_ids, new_chunk_sizes, chunk_locations_opt)) => {
                    // Update metadata
                    if is_append {
                        // Append: add new chunks to existing list
                        if let Some(ref chunk_locations) = chunk_locations_opt {
                            metadata.chunk_locations.extend(chunk_locations.clone());
                        }
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
                        let mut updated_locations = Vec::new();

                        // Keep chunks before affected range
                        updated_chunks.extend_from_slice(&metadata.chunks[..first_idx]);
                        updated_sizes.extend_from_slice(&metadata.chunk_sizes[..first_idx]);
                        if !metadata.chunk_locations.is_empty() && first_idx <= metadata.chunk_locations.len() {
                            updated_locations.extend_from_slice(&metadata.chunk_locations[..first_idx]);
                        }

                        // Insert new chunks
                        updated_chunks.extend(new_chunk_ids);
                        updated_sizes.extend(new_chunk_sizes);
                        if let Some(ref chunk_locations) = chunk_locations_opt {
                            updated_locations.extend(chunk_locations.clone());
                        }

                        // Keep chunks after affected range (if any)
                        if last_idx + 1 < metadata.chunks.len() {
                            updated_chunks.extend_from_slice(&metadata.chunks[last_idx + 1..]);
                            updated_sizes.extend_from_slice(&metadata.chunk_sizes[last_idx + 1..]);
                            if !metadata.chunk_locations.is_empty() && last_idx + 1 < metadata.chunk_locations.len() {
                                updated_locations.extend_from_slice(&metadata.chunk_locations[last_idx + 1..]);
                            }
                        }

                        metadata.chunks = updated_chunks;
                        metadata.chunk_sizes = updated_sizes;
                        metadata.chunk_locations = updated_locations;

                        // Recalculate total file size
                        metadata.size = metadata.chunk_sizes.iter().sum();

                        info!("After splice: {} total chunks, {} total bytes",
                              metadata.chunks.len(), metadata.size);
                    } else {
                        // Full rewrite (shouldn't happen with current logic, but keep as fallback)
                        warn!("Full file rewrite with {} bytes", new_data.len());
                        if let Some(chunk_locations) = chunk_locations_opt {
                            metadata.chunk_locations = chunk_locations;
                        } else {
                            metadata.chunk_locations.clear();
                        }
                        metadata.chunks = new_chunk_ids;
                        metadata.chunk_sizes = new_chunk_sizes;
                        metadata.size = new_data.len() as u64;
                    }
                    metadata.modified_at = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs();

                    // Check if this is a SQLite database file - they need immediate metadata updates
                    let is_sqlite_db = is_sqlite_path(&metadata.path);

                    // Batch metadata updates for non-SQLite files: time-based (every 2 seconds)
                    // For SQLite files: ALWAYS update immediately to prevent corruption
                    let should_update = if is_sqlite_db {
                        debug!("SQLite database detected - forcing immediate metadata update for ino={}", ino);
                        true // Always update for SQLite
                    } else {
                        // Use time-based batching for regular files
                        match last_metadata_update.get(&ino) {
                            None => true,
                            Some(last) => last.elapsed() >= std::time::Duration::from_secs(2)
                        }
                    };

                    if should_update {
                        // Store updated metadata
                        let metadata_start = std::time::Instant::now();
                        let metadata_clone = metadata.clone();
                        let update_result = runtime.block_on(async {
                            client.put_file_metadata(&metadata_clone).await
                        });
                        let metadata_elapsed = metadata_start.elapsed();

                        // Record metadata update time for batching
                        last_metadata_update.insert(ino, std::time::Instant::now());

                        if is_sqlite_db {
                            debug!("put_file_metadata took {:?} (SQLite immediate update)", metadata_elapsed);
                        } else {
                            debug!("put_file_metadata took {:?} (time-based batching)", metadata_elapsed);
                        }

                        match update_result {
                            Ok(_) => {
                                // Update cache
                                metadata_cache.insert(ino, metadata);
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
                        metadata_cache.insert(ino, metadata);
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
        // flush() is called for every close() — including read-only fds.
        // If there are no write-mode handles open for this inode, skip touching the
        // write buffer entirely. Otherwise a reader closing its fd would steal and
        // prematurely flush the DVR's write buffer, producing a mis-sized chunk and
        // resetting the write state mid-recording.
        let has_writers = self.write_open_counts.get(&ino).map(|c| *c > 0).unwrap_or(false);
        if !has_writers {
            debug!("flush: ino={} - no writers, skipping buffer flush for read-only close", ino);
            reply.ok();
            return;
        }

        // Clone Arc-wrapped fields for thread pool
        let write_buffer_enabled = self.write_buffer_enabled;
        let write_buffers = self.write_buffers.clone();
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let write_counters = self.write_counters.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let runtime = self.runtime.clone();

        // Spawn flush operation on tokio's blocking thread pool
        runtime.clone().spawn_blocking(move || {
            debug!("flush: ino={}", ino);

            if write_buffer_enabled {
                // When write-buffering is enabled, flush() must NOT drain the write buffer.
                //
                // flush() fires on every close() — including read-only fds (e.g. Kodi seeking
                // while the DVR is still recording) and the DVR's own file descriptor between
                // individual write calls. Draining the buffer here causes two problems:
                //   1. A partial (sub-4MB) buffer gets emitted as a tiny chunk, destroying
                //      4MB alignment and causing all subsequent chunks to be tiny.
                //   2. The re-alignment pre-seed (partial last-chunk read-back on DVR restart)
                //      is discarded before enough new data has accumulated to fill a full chunk.
                //
                // The write buffer is already flushed correctly by:
                //   - The periodic background flusher (every 2s, aligned 4MB chunks only)
                //   - fsync() (aligned chunks only, same as background flusher)
                //   - release() with a write-mode fd (force=true, flushes everything including
                //     the partial tail on genuine close)
                //
                // So flush() just needs to acknowledge without touching the buffer.
                debug!("flush: ino={} - write buffer active, deferring to background flusher / release", ino);
                reply.ok();
                return;
            } else {
                // No write buffer, but we still need to flush any pending metadata updates
                // that were batched by the write() path to ensure data durability
                let result = runtime.block_on(async {
                    // Get metadata from cache
                    let metadata_opt = metadata_cache.get(&ino).map(|m| m.clone());

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
        flags: i32,
        lock_owner: Option<u64>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        let pid = req.pid();
        debug!("release: ino={}, owner={:?}, pid={}", ino, lock_owner, pid);

        // Decrement write-mode open count when a write-mode fd is closed
        let is_write = (flags & libc::O_ACCMODE) != libc::O_RDONLY;
        if is_write {
            let mut remove = false;
            if let Some(mut count) = self.write_open_counts.get_mut(&ino) {
                if *count > 0 { *count -= 1; }
                if *count == 0 { remove = true; }
            }
            if remove { self.write_open_counts.remove(&ino); }
        }

        let lock_manager = self.lock_manager.clone();

        if self.write_buffer_enabled {
            // Only flush the write buffer when a write-mode fd is being closed.
            // Read-only releases (e.g. Kodi closing a playback fd before a seek) must NOT
            // touch the write buffer — doing so causes a synchronous multi-MB cluster write
            // that stalls the FUSE thread for ~1.4 seconds and disrupts live-recording reads.
            // The write buffer is owned by the writer; readers can serve from it but should
            // not flush or remove it when they close.
            if is_write {
                let result = self.block_on(async {
                    // First flush writes (force metadata update on close)
                    self.flush_buffer_async(ino, true).await?;

                    // Then release all locks held by this owner (if lock_owner is provided)
                    if let Some(owner) = lock_owner {
                        lock_manager.release_all(ino, owner).await?;
                    }

                    Ok::<(), anyhow::Error>(())
                });

                // Clean up write buffer entry to prevent memory leak
                // Even if flush failed, we should remove the buffer entry
                self.write_buffers.remove(&ino);

                match result {
                    Ok(_) => reply.ok(),
                    Err(e) => {
                        error!("Failed to flush/release for inode {}: {}", ino, e);
                        reply.error(libc::EIO);
                    }
                }
            } else {
                // Read-only close: release locks if needed, leave write buffer untouched.
                if let Some(owner) = lock_owner {
                    let result = self.block_on(lock_manager.release_all(ino, owner));
                    if let Err(e) = result {
                        error!("Failed to release locks for inode {}: {}", ino, e);
                    }
                }
                reply.ok();
            }
        } else {
            // No write buffer, but flush any pending metadata updates before releasing locks
            let client = self.client.clone();
            let metadata_cache = self.metadata_cache.clone();
            let write_counters = self.write_counters.clone();

            let result = self.block_on(async {
                // Flush pending metadata if any
                let metadata_opt = metadata_cache.get(&ino).map(|m| m.clone());

                if let Some(metadata) = metadata_opt {
                    // Always flush metadata on release for non-buffered writes — the 2-second
                    // batch window means the final chunks may not have been committed yet.
                    debug!("release: flushing metadata for ino={} ({} chunks)", ino, metadata.chunks.len());
                    client.put_file_metadata(&metadata).await?;
                    write_counters.write().unwrap().insert(ino, 0);
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
            chunk_locations: Vec::new(),
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
                self.metadata_cache.insert(ino, metadata.clone());

                // CRITICAL: Invalidate parent directory cache so 'ls' shows new directory immediately
                let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                self.dir_cache.remove(parent_path);
                debug!("Invalidated directory cache for parent: {}", parent_path);

                // Convert to FUSE attr
                let attr = self.metadata_to_attr(ino, &metadata);
                reply.entry(&Duration::from_secs(1), &attr, 0);
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
                // Remove from all caches to prevent memory leaks
                if let Some(&ino) = self.path_to_inode.read().unwrap().get(&path) {
                    self.metadata_cache.remove(&ino);
                    self.write_buffers.remove(&ino); // Clean up write buffer
                    self.write_counters.write().unwrap().remove(&ino); // Clean up write counter
                    self.last_metadata_update.remove(&ino); // Clean up metadata update tracker
                    self.last_warm_offset.remove(&ino); // Clean up warm offset tracker
                    self.chunk_offset_cache.remove(&ino); // Clean up chunk offset cache
                }
                self.path_to_inode.write().unwrap().remove(&path);

                // CRITICAL: Invalidate parent directory cache so 'ls' shows deletion immediately
                let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                self.dir_cache.remove(parent_path);
                debug!("Invalidated directory cache for parent: {}", parent_path);

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
                            self.metadata_cache.remove(&ino);
                        }
                        self.path_to_inode.write().unwrap().remove(&path);

                        // CRITICAL: Invalidate parent directory cache so 'ls' shows deletion immediately
                        let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                        let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                        self.dir_cache.remove(parent_path);
                        debug!("Invalidated directory cache for parent: {}", parent_path);

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

                        self.metadata_cache.insert(ino, new_metadata);

                        // Invalidate directory cache for both old and new parent directories
                        let raw_old = old_path.rsplitn(2, '/').nth(1).unwrap_or("");
                        let old_parent = if raw_old.is_empty() { "/" } else { raw_old };
                        let raw_new = new_path.rsplitn(2, '/').nth(1).unwrap_or("");
                        let new_parent = if raw_new.is_empty() { "/" } else { raw_new };
                        self.dir_cache.remove(old_parent);
                        if old_parent != new_parent {
                            self.dir_cache.remove(new_parent);
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

        let mut metadata = match self.metadata_cache.get(&ino) {
            Some(m) => m.clone(),
            None => {
                reply.error(libc::ENOENT);
                return;
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
                            let chunk_size = chunk_sizes[last_chunk_idx] as usize;

                            // Build read hint for the last partial chunk (full chunk read for truncate)
                            let read_hint = vec![crate::client::ChunkReadHint {
                                chunk_idx: last_chunk_idx,
                                chunk_id,
                                full_chunk: true,  // Read full chunk for truncate operation
                                offset_in_chunk: 0,
                                length: chunk_size,
                                file_offset: chunk_offset,
                            }];

                            // Read only the last partial chunk
                            let last_chunk_data = match self.block_on(async {
                                client.read_data(&read_hint, &metadata.chunks, ino, &metadata.chunk_locations).await
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
                                Ok((new_chunk_ids, new_chunk_sizes, chunk_locations_opt)) => {
                                    // Keep chunks before last, add truncated chunk
                                    let mut new_all_chunks = metadata.chunks[..last_chunk_idx].to_vec();
                                    new_all_chunks.extend(new_chunk_ids);

                                    let mut new_all_sizes = metadata.chunk_sizes[..last_chunk_idx].to_vec();
                                    new_all_sizes.extend(new_chunk_sizes);

                                    // Update chunk_locations if provided
                                    if let Some(chunk_locations) = chunk_locations_opt {
                                        let mut new_all_locations = if last_chunk_idx < metadata.chunk_locations.len() {
                                            metadata.chunk_locations[..last_chunk_idx].to_vec()
                                        } else {
                                            Vec::new()
                                        };
                                        new_all_locations.extend(chunk_locations);
                                        metadata.chunk_locations = new_all_locations;
                                    }

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
                self.metadata_cache.insert(ino, metadata.clone());

                // Convert to FUSE attr
                let attr = self.metadata_to_attr(ino, &metadata);
                // Short TTL (2s) for multi-client coherency
                reply.attr(&Duration::from_secs(2), &attr);
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
        if self.metadata_cache.contains_key(&ino) {
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
            // Spawn the flush as a background task so the FUSE dispatch thread is not
            // blocked — reads from any file and writes to other inodes remain unaffected.
            // Per-inode ordering is preserved: write() stalls while this inode is in
            // in_flight, so subsequent writes to the same file cannot proceed until the
            // flush completes.  The kernel gets reply.ok() once the data is in-flight on
            // the network (two nodes, parallel); the <200ms gap before disk ack is
            // acceptable for a DVR workload and far better than the old "don't flush tail"
            // approach.
            let in_flight_opt = self.flush_in_flight.read().unwrap().clone();
            if let Some(in_flight) = in_flight_opt {
                // Mark this inode as flush-in-flight before spawning so write() sees it
                // immediately (before the spawn even starts running).
                in_flight.insert(ino);

                let handle = self.flush_handle.clone();
                let in_flight_clone = in_flight.clone();
                self.runtime.spawn(async move {
                    if let Err(e) = handle.flush_buffer_async(ino, true).await {
                        error!("fsync background flush failed for inode {}: {}", ino, e);
                    }
                    in_flight_clone.remove(&ino);
                });

                reply.ok();
            } else {
                // Background flusher not yet started — fall back to synchronous flush
                let result = self.block_on(self.flush_buffer_async(ino, true));
                match result {
                    Ok(_) => reply.ok(),
                    Err(e) => {
                        error!("Failed to fsync inode {}: {}", ino, e);
                        reply.error(libc::EIO);
                    }
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
                let metadata_opt = metadata_cache.get(&ino).map(|m| m.clone());

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
