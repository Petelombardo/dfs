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

/// Buffered write data for a single chunk-aligned slot within a file.
/// One ChunkBuffer covers exactly chunk_size bytes: [chunk_idx*chunk_size .. (chunk_idx+1)*chunk_size).
/// The buffer is zero-padded; bytes not yet written are zero.  On flush the full chunk_size
/// buffer is written to the cluster; the file's logical size (metadata.size) controls how
/// much is visible to readers — trailing zeros are never returned beyond the declared size.
struct ChunkBuffer {
    /// chunk_size bytes of buffered data, zero-initialized
    data: Vec<u8>,
    /// Needs to be written to the cluster
    dirty: bool,
    /// When this buffer was last written to
    last_modified: SystemTime,
    /// Logical end-of-data within this chunk (bytes actually written, not chunk_size-padded)
    logical_len: usize,
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

    /// Chunk size in bytes (queried from cluster, typically 4MB).
    /// All chunk buffers are exactly this size (zero-padded at end of file).
    chunk_size: usize,

    /// Per-chunk write buffers.  Key: (inode, chunk_idx) where chunk_idx = file_offset / chunk_size.
    /// Each buffer holds up to chunk_size bytes.  Writes merge into the buffer; on flush the full
    /// chunk_size buffer is sent to the cluster (zero-padded if the chunk is at EOF).
    /// Eliminates AppendFile and all OffsetMismatch races.
    write_chunk_buffers: Arc<DashMap<(u64, u64), Arc<Mutex<ChunkBuffer>>>>,

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

    /// Count of write-mode open file handles per inode.
    /// Used to guard the write buffer in flush(): a flush() triggered by a read-only
    /// close must not touch the write buffer of a concurrently writing fd.
    write_open_counts: Arc<DashMap<u64, usize>>,

    /// Per-inode write serialization lock.
    /// Prevents concurrent FUSE write callbacks from racing on the same inode.
    /// Uses std::sync::Mutex (not tokio) because FUSE callbacks run on OS threads.
    write_inode_locks: Arc<DashMap<u64, Arc<std::sync::Mutex<()>>>>,

    /// Per-inode semaphore limiting concurrent in-flight inline chunk flushes.
    /// Allows up to 2 chunks to be on the wire simultaneously so the application
    /// can fill the next chunk while the previous one is being sent (pipelining).
    /// Acquiring a permit blocks when both slots are full, providing back pressure.
    flush_semaphores: Arc<DashMap<u64, Arc<tokio::sync::Semaphore>>>,
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
        info!("Client configured with chunk_size={}MB for 4MB-aligned chunk writes", chunk_size_mb);

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

        let chunk_size = chunk_size_mb * 1024 * 1024;
        let metadata_cache = Arc::new(DashMap::<u64, FileMetadata>::new());
        let path_to_inode = Arc::new(RwLock::new(HashMap::<String, u64>::new()));
        let next_inode = Arc::new(RwLock::new(2)); // Start at 2, root is 1
        let write_chunk_buffers: Arc<DashMap<(u64, u64), Arc<Mutex<ChunkBuffer>>>> =
            Arc::new(DashMap::new());

        // No background flusher: all chunk flushes happen synchronously on fsync/release.
        // A background flusher that marks buffers clean before the network write completes
        // creates a race with release — release sees dirty=false, skips the chunk, removes
        // the buffer, and the data is silently lost if the background write then fails.
        // Correctness over throughput: flush on demand only.

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

        Ok(Self {
            client,
            metadata_cache,
            path_to_inode,
            next_inode,
            root_inode: 1,
            runtime,
            write_counters: Arc::new(RwLock::new(HashMap::new())),
            write_buffer_enabled,
            chunk_size,
            write_chunk_buffers,
            last_metadata_update: Arc::new(DashMap::new()),
            last_chunk_cache: Arc::new(RwLock::new(None)),
            last_warm_offset: Arc::new(DashMap::new()),
            chunk_offset_cache: Arc::new(DashMap::new()),
            dir_cache: Arc::new(DashMap::new()),
            statfs_cache: Arc::new(RwLock::new(None)),
            lock_manager: Arc::new(LockManager::new()),
            write_open_counts: Arc::new(DashMap::new()),
            write_inode_locks: Arc::new(DashMap::new()),
            flush_semaphores: Arc::new(DashMap::new()),
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
                self.write_chunk_buffers.iter().any(|e| e.key().0 == ino && e.value().try_lock().map(|b| b.dirty).unwrap_or(false))
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

    /// Merge chunk locations from a write into the metadata cache.
    /// Called after write_data_with_cache() returns; updates chunk list and file size.
    /// `preserve_size`: if true, only grow size (never shrink) — used for overwrites.
    fn update_metadata_with_chunk(
        metadata_cache: &Arc<DashMap<u64, FileMetadata>>,
        ino: u64,
        locations: &[dfs_common::ChunkLocation],
        file_offset: u64,
        chunk_size: usize,
        preserve_size: bool,
    ) {
        if let Some(mut meta) = metadata_cache.get_mut(&ino) {
            for loc in locations {
                // Replace existing chunk at this file_offset if present, else append.
                if let Some(pos) = meta.chunk_locations.iter().position(|l| l.file_offset == loc.file_offset) {
                    meta.chunks[pos] = loc.chunk_id;
                    meta.chunk_sizes[pos] = loc.size as u64;
                    meta.chunk_locations[pos] = loc.clone();
                } else {
                    meta.chunks.push(loc.chunk_id);
                    meta.chunk_sizes.push(loc.size as u64);
                    meta.chunk_locations.push(loc.clone());
                }
            }
            // Sort chunk_locations by file_offset to keep them ordered
            let mut combined: Vec<(dfs_common::ChunkLocation, ChunkId, u64)> = meta.chunk_locations.iter().cloned()
                .zip(meta.chunks.iter().cloned())
                .zip(meta.chunk_sizes.iter().cloned())
                .map(|((loc, id), sz)| (loc, id, sz))
                .collect();
            combined.sort_by_key(|(loc, _, _)| loc.file_offset.unwrap_or(0));
            meta.chunk_locations = combined.iter().map(|(loc, _, _)| loc.clone()).collect();
            meta.chunks = combined.iter().map(|(_, id, _)| *id).collect();
            meta.chunk_sizes = combined.iter().map(|(_, _, sz)| *sz).collect();

            // Update logical size: end of last location
            let written_end = file_offset + locations.iter().map(|l| l.size as u64).sum::<u64>();
            if preserve_size {
                meta.size = meta.size.max(written_end);
            } else {
                meta.size = meta.size.max(written_end);
            }
            meta.modified_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
        }
    }

    /// Flush all dirty chunk buffers for `ino`.
    ///
    /// If `force=true` (fsync/release): flushes all dirty buffers including partial tail chunks
    /// (zero-padded to chunk_size before writing; file's logical size limits reads).
    /// If `force=false` (background): only flushes full (logical_len == chunk_size) buffers.
    ///
    /// No AppendFile, no OffsetMismatch.  Each chunk is written independently at its
    /// fixed file offset; metadata is updated once per chunk, then persisted.
    async fn flush_chunks_for_inode(&self, ino: u64, force: bool) -> Result<()> {
        let chunk_size = self.chunk_size;

        // Wait for any in-flight inline flushes to complete before we inspect the
        // chunk buffers.  We do this by acquiring all semaphore permits — once we hold
        // both, no background flush task is running for this inode.
        if let Some(sem) = self.flush_semaphores.get(&ino) {
            let _p1 = sem.acquire().await.ok();
            let _p2 = sem.acquire().await.ok();
            // Permits are released when _p1/_p2 drop at end of this block, which is
            // fine — all in-flight tasks have completed by the time we hold both.
        }

        // Collect all (chunk_idx, data, file_offset) to flush, releasing locks quickly.
        let mut to_flush: Vec<(u64, Vec<u8>)> = Vec::new();
        {
            let keys: Vec<(u64, u64)> = self.write_chunk_buffers.iter()
                .filter(|e| e.key().0 == ino)
                .map(|e| *e.key())
                .collect();

            for key in keys {
                let (_, chunk_idx) = key;
                if let Some(lock) = self.write_chunk_buffers.get(&key) {
                    let mut buf = lock.lock().await;
                    if !buf.dirty {
                        continue;
                    }
                    if !force && buf.logical_len < chunk_size {
                        continue; // leave partial tail for forced flush
                    }
                    // Zero-pad to chunk_size
                    if buf.data.len() < chunk_size {
                        buf.data.resize(chunk_size, 0);
                    }
                    to_flush.push((chunk_idx, buf.data.clone()));
                    buf.dirty = false; // optimistic: mark clean; revert on error below
                }
            }
        }

        // Sort by chunk_idx for predictable ordering
        to_flush.sort_by_key(|(idx, _)| *idx);

        let mut any_error: Option<anyhow::Error> = None;

        for (chunk_idx, data) in &to_flush {
            let file_offset = chunk_idx * chunk_size as u64;
            info!("flush_chunks_for_inode: ino={} chunk_idx={} offset={} size={}",
                  ino, chunk_idx, file_offset, data.len());

            // Retry up to 3 times on transient errors before giving up.
            let mut last_err: Option<anyhow::Error> = None;
            for attempt in 0..3u32 {
                if attempt > 0 {
                    tokio::time::sleep(tokio::time::Duration::from_millis(200 * attempt as u64)).await;
                    warn!("flush_chunks_for_inode: retrying ino={} chunk_idx={} attempt={}", ino, chunk_idx, attempt + 1);
                }
                match self.client.write_data_with_cache(data, ino, file_offset).await {
                    Ok((_, _, locations_opt)) => {
                        if let Some(locs) = locations_opt {
                            Self::update_metadata_with_chunk(
                                &self.metadata_cache, ino, &locs, file_offset, chunk_size, true,
                            );
                        }
                        last_err = None;
                        break;
                    }
                    Err(e) => {
                        error!("flush_chunks_for_inode: ino={} chunk_idx={} attempt={} failed: {}", ino, chunk_idx, attempt + 1, e);
                        last_err = Some(e);
                    }
                }
            }
            if let Some(e) = last_err {
                // Re-mark dirty so a future flush can retry
                if let Some(lock) = self.write_chunk_buffers.get(&(ino, *chunk_idx)) {
                    let mut buf = lock.lock().await;
                    buf.dirty = true;
                }
                any_error = Some(e);
            }
        }

        // Persist metadata after all chunks are written.
        // Always persist on a forced flush (fsync/release) — inline-flush background tasks
        // may have already sent all the chunk data without persisting metadata, so we must
        // do it here even when to_flush was empty.
        if any_error.is_none() && force {
            let meta_snap = self.metadata_cache.get(&ino).map(|m| m.clone());
            if let Some(meta) = meta_snap {
                if let Err(e) = self.client.put_file_metadata(&meta).await {
                    error!("flush_chunks_for_inode: metadata persist failed for ino={}: {}", ino, e);
                    return Err(e);
                }
                self.chunk_offset_cache.remove(&ino);
            }
        }

        if let Some(e) = any_error {
            return Err(e);
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

        // Cache miss or stale — fetch from cluster with conditional GET.
        // Use runtime.spawn (non-blocking) so we never block the FUSE dispatch
        // thread waiting for a runtime thread.  When all worker threads are busy
        // (e.g. 44 concurrent getattr tasks after a readdir) a block_on here would
        // deadlock — the FUSE thread parks waiting for a thread that will never free.
        let cached_modified_at = {
            let path_map = self.path_to_inode.read().unwrap();
            if let Some(&ino) = path_map.get(&path) {
                self.metadata_cache.get(&ino).map(|m| m.modified_at)
            } else {
                None
            }
        };

        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let next_inode = self.next_inode.clone();
        let last_metadata_update = self.last_metadata_update.clone();

        self.runtime.spawn(async move {
            let result = client.get_file_metadata_conditional(&path, cached_modified_at).await;

            match result {
                Ok(Some(metadata)) => {
                    // Metadata was modified or first fetch — update cache.
                    let ino = {
                        let path_map = path_to_inode.read().unwrap();
                        if let Some(&existing) = path_map.get(&path) {
                            existing
                        } else {
                            drop(path_map);
                            let mut next = next_inode.write().unwrap();
                            let ino = *next;
                            *next += 1;
                            drop(next);
                            path_to_inode.write().unwrap().insert(path.clone(), ino);
                            ino
                        }
                    };
                    metadata_cache.insert(ino, metadata.clone());
                    last_metadata_update.insert(ino, std::time::Instant::now());

                    let attr = DfsFilesystem::metadata_to_attr_static(ino, &metadata);
                    reply.entry(&Duration::from_secs(1), &attr, 0);
                }
                Ok(None) => {
                    // Either file not found OR metadata not modified (cache still valid).
                    if cached_modified_at.is_some() {
                        let path_map = path_to_inode.read().unwrap();
                        if let Some(&ino) = path_map.get(&path) {
                            if let Some(metadata) = metadata_cache.get(&ino) {
                                debug!("Using cached metadata for {} (not modified)", path);
                                let attr = DfsFilesystem::metadata_to_attr_static(ino, &*metadata);
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
        });
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
        let write_chunk_buffers = self.write_chunk_buffers.clone();
        let write_buffer_enabled = self.write_buffer_enabled;
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            let metadata = metadata_cache.get(&ino).map(|m| m.clone());

            if let Some(mut metadata) = metadata {
                if metadata.file_type == FileType::RegularFile {
                    // Only hit the server if the cached metadata is more than 5 seconds old.
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

                    // The metadata.size is kept up-to-date by the write handler's in-memory
                    // update, so no need to scan chunk buffers here.
                }

                let attr = DfsFilesystem::metadata_to_attr_static(ino, &metadata);
                // Use a short TTL for files with active chunk buffers (live recordings)
                let has_dirty_buf = write_buffer_enabled
                    && write_chunk_buffers.iter().any(|e| e.key().0 == ino);
                let ttl = if has_dirty_buf {
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
        let write_chunk_buffers = self.write_chunk_buffers.clone();
        let write_buffer_enabled = self.write_buffer_enabled;
        let chunk_size = self.chunk_size;
        let last_metadata_update = self.last_metadata_update.clone();
        let last_warm_offset = self.last_warm_offset.clone();
        let chunk_offset_cache = self.chunk_offset_cache.clone();

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

            // Check chunk write buffers for data not yet flushed to the cluster.
            // With 4MB-aligned chunk buffers, we can serve reads directly from any
            // dirty chunk buffer overlapping the requested byte range.
            // This enables "live rewind" — reading while writing without waiting.
            if write_buffer_enabled {
                let read_end = offset + size;
                let first_chunk_idx = (offset / chunk_size) as u64;
                let last_chunk_idx = ((read_end.saturating_sub(1)) / chunk_size) as u64;

                // Build a result buffer spanning the entire read range, overlay chunk buffers.
                // We only take this path if all bytes are available in the chunk buffers;
                // otherwise fall through to the server read path.
                let mut buf_result: Vec<u8> = vec![0u8; size];
                let mut covered = 0usize;

                for chunk_idx in first_chunk_idx..=last_chunk_idx {
                    let key = (ino, chunk_idx);
                    if let Some(lock) = write_chunk_buffers.get(&key) {
                        let buf = lock.lock().await;
                        let chunk_file_start = chunk_idx as usize * chunk_size;
                        let chunk_file_end = chunk_file_start + buf.logical_len;

                        // Intersection of read range with this chunk's data range
                        let overlap_start = offset.max(chunk_file_start);
                        let overlap_end = read_end.min(chunk_file_end);
                        if overlap_start >= overlap_end { continue; }

                        let src_off = overlap_start - chunk_file_start;
                        let dst_off = overlap_start - offset;
                        let len = overlap_end - overlap_start;
                        if src_off + len <= buf.data.len() {
                            buf_result[dst_off..dst_off + len]
                                .copy_from_slice(&buf.data[src_off..src_off + len]);
                            covered += len;
                        }
                    }
                }

                // If chunk buffers cover the entire requested range (and it's within the
                // logical file size), serve it directly without hitting the cluster.
                // Clamp to logical file size to avoid returning trailing zeros.
                let logical_eof = metadata.size as usize;
                let effective_end = read_end.min(logical_eof);
                let expected_covered = effective_end.saturating_sub(offset);

                if covered >= expected_covered && expected_covered > 0 {
                    let serve_len = expected_covered;
                    info!("FUSE read from chunk buffers: ino={}, offset={}, covered={}/{} bytes",
                          ino, offset, covered, size);
                    let elapsed = start.elapsed();
                    info!("FUSE read COMPLETE (chunk buffer): ino={}, offset={}, size={}, took {:?}",
                          ino, offset, serve_len, elapsed);
                    reply.data(&buf_result[..serve_len]);
                    return;
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

        // Clone all Arc fields needed in the spawned task.
        let client = self.client.clone();
        let dir_cache = self.dir_cache.clone();
        let metadata_cache = self.metadata_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let next_inode = self.next_inode.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let write_chunk_buffers_rd = self.write_chunk_buffers.clone();
        let write_counters = self.write_counters.clone();

        // Spawn the rest of readdir so we never block_on from the FUSE dispatch
        // thread.  If the runtime is saturated (e.g. concurrent recording writes)
        // a block_on here would deadlock — same root cause as the lookup() fix.
        // ReplyDirectory is Send so it moves into the task safely.
        self.runtime.spawn(async move {
            let entries = if let Some(entries) = cached_entries {
                entries
            } else {
                // Cache miss — fetch from server.
                debug!("Directory cache MISS for {}", path);
                match client.list_directory(&path).await {
                    Ok(entries) => {
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
                let entry_ino = {
                    let path_map = path_to_inode.read().unwrap();
                    if let Some(&existing) = path_map.get(&entry.path) {
                        existing
                    } else {
                        drop(path_map);
                        let mut next = next_inode.write().unwrap();
                        let ino_val = *next;
                        *next += 1;
                        drop(next);
                        path_to_inode.write().unwrap().insert(entry.path.clone(), ino_val);
                        ino_val
                    }
                };

                // Cache metadata, but DON'T overwrite if there's an active write.
                let has_active_write = {
                    let has_buffer = write_chunk_buffers_rd.iter().any(|e| e.key().0 == entry_ino);
                    let has_counter = write_counters.read().unwrap().get(&entry_ino).map(|c| *c > 0).unwrap_or(false);
                    has_buffer || has_counter
                };
                if !has_active_write {
                    metadata_cache.insert(entry_ino, entry.clone());
                }

                // Mark metadata as just-refreshed so getattr skips the per-file server
                // round-trip on the immediately following `ls -alh`.
                last_metadata_update.insert(entry_ino, std::time::Instant::now());

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
                let futures: Vec<_> = subdirs.into_iter().map(|subdir| {
                    let client = client.clone();
                    let dir_cache = dir_cache.clone();
                    let metadata_cache = metadata_cache.clone();
                    let path_to_inode = path_to_inode.clone();
                    let next_inode = next_inode.clone();
                    let last_metadata_update = last_metadata_update.clone();
                    async move {
                        // Skip if already cached and fresh
                        if let Some(entry) = dir_cache.get(&subdir) {
                            if entry.1.elapsed() < std::time::Duration::from_secs(29) {
                                return;
                            }
                        }
                        let fetch_start = std::time::Instant::now();
                        if let Ok(sub_entries) = client.list_directory(&subdir).await {
                            // Only cache if the directory hasn't been invalidated while we
                            // were fetching.
                            let still_valid = match dir_cache.get(&subdir) {
                                Some(entry) => entry.1 < fetch_start,
                                None => false,
                            };
                            if still_valid {
                                dir_cache.insert(
                                    subdir.clone(),
                                    (sub_entries.clone(), std::time::Instant::now()),
                                );
                            }
                            let now = std::time::Instant::now();
                            for entry in &sub_entries {
                                let ino_val = {
                                    let mut path_map = path_to_inode.write().unwrap();
                                    if let Some(&existing) = path_map.get(&entry.path) {
                                        existing
                                    } else {
                                        let mut next = next_inode.write().unwrap();
                                        let v = *next;
                                        *next += 1;
                                        path_map.insert(entry.path.clone(), v);
                                        v
                                    }
                                };
                                metadata_cache.insert(ino_val, entry.clone());
                                last_metadata_update.insert(ino_val, now);
                            }
                            debug!("Prefetched {} entries for {}", sub_entries.len(), subdir);
                        }
                    }
                }).collect();
                futures::future::join_all(futures).await;
            }
        });
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

        // Store metadata on cluster — spawn so we never block_on the FUSE dispatch thread.
        let client = self.client.clone();
        let metadata_clone = metadata.clone();
        let metadata_cache = self.metadata_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let next_inode = self.next_inode.clone();
        let write_open_counts = self.write_open_counts.clone();

        self.runtime.spawn(async move {
            match client.put_file_metadata(&metadata_clone).await {
                Ok(_) => {
                    // Allocate inode
                    let ino = {
                        let path_map = path_to_inode.read().unwrap();
                        if let Some(&existing) = path_map.get(&path) {
                            existing
                        } else {
                            drop(path_map);
                            let mut next = next_inode.write().unwrap();
                            let v = *next; *next += 1; drop(next);
                            path_to_inode.write().unwrap().insert(path.clone(), v);
                            v
                        }
                    };

                    // Cache metadata
                    metadata_cache.insert(ino, metadata.clone());

                    // Invalidate parent directory cache so 'ls' shows new file immediately
                    let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                    let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                    dir_cache.remove(parent_path);

                    // create() always opens for writing — count it
                    *write_open_counts.entry(ino).or_insert(0) += 1;

                    let attr = DfsFilesystem::metadata_to_attr_static(ino, &metadata);
                    let open_flags = if is_sqlite_direct_io(&path) {
                        fuser::consts::FOPEN_DIRECT_IO
                    } else {
                        0
                    };
                    reply.created(&Duration::from_secs(300), &attr, 0, 0, open_flags);
                }
                Err(e) => {
                    error!("Failed to create file {}: {}", path, e);
                    reply.error(libc::EIO);
                }
            }
        });
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
        let data_len = data.len();
        let data_vec = data.to_vec();
        let offset = offset as u64;
        let metadata_cache = self.metadata_cache.clone();
        let write_chunk_buffers = self.write_chunk_buffers.clone();
        let write_buffer_enabled = self.write_buffer_enabled;
        let chunk_size = self.chunk_size;
        let chunk_offset_cache = self.chunk_offset_cache.clone();
        let client = self.client.clone();
        let runtime = self.runtime.clone();
        let flush_semaphores = self.flush_semaphores.clone();

        // Acquire per-inode write lock to serialize concurrent writes to the same inode.
        // qemu-nbd issues parallel write requests; without this they'd race on chunk buffers.
        // std::sync::Mutex because FUSE callbacks run on OS threads.
        let inode_lock = self.write_inode_locks
            .entry(ino)
            .or_insert_with(|| Arc::new(std::sync::Mutex::new(())))
            .clone();
        let _inode_guard = inode_lock.lock().unwrap_or_else(|p| p.into_inner());

        // --- 4MB-aligned chunk write path ---
        // Every write goes into a per-(ino, chunk_idx) buffer.
        // chunk_idx = file_offset / chunk_size.  The buffer holds exactly chunk_size bytes
        // (zero-padded); writes merge at the correct intra-chunk offset.  On fsync/release,
        // each dirty buffer is written as a complete fixed-size chunk (no AppendFile, no races).
        {
            let start = std::time::Instant::now();
            debug!("write: ino={}, offset={}, size={}", ino, offset, data_len);


            // Ensure metadata exists for this inode
            if metadata_cache.get(&ino).is_none() {
                let path_opt = {
                    let map = self.path_to_inode.read().unwrap();
                    map.iter().find(|(_, &v)| v == ino).map(|(k, _)| k.clone())
                };
                if let Some(path) = path_opt {
                    match runtime.block_on(client.get_file_metadata(&path)) {
                        Ok(Some(fetched)) => {
                            metadata_cache.insert(ino, fetched);
                        }
                        Ok(None) => {
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
                            metadata_cache.insert(ino, new_meta);
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

            // Verify it's a regular file
            {
                let meta = metadata_cache.get(&ino).unwrap();
                if meta.file_type != FileType::RegularFile {
                    reply.error(libc::EISDIR);
                    return;
                }
            }

            // SQLite files bypass chunk buffering — write through immediately for consistency.
            let is_sqlite = {
                let meta = metadata_cache.get(&ino).unwrap();
                is_sqlite_path(&meta.path)
            };
            let cache_inode = if is_sqlite { 0 } else { ino };

            if write_buffer_enabled && !is_sqlite {
                // --- Chunk-buffered write path ---
                // Split the write across 4MB-aligned chunk boundaries.
                // Each chunk buffer covers [chunk_idx*chunk_size .. (chunk_idx+1)*chunk_size).
                // Writes are merged at the correct intra-chunk position; the buffer is
                // zero-padded to chunk_size. On fsync/release, flush_chunks_for_inode()
                // writes each full chunk_size buffer to the cluster.
                let mut pos = 0usize;
                while pos < data_len {
                    let file_off = offset + pos as u64;
                    let chunk_idx = file_off / chunk_size as u64;
                    let chunk_file_start = chunk_idx * chunk_size as u64;
                    let intra_off = (file_off - chunk_file_start) as usize;
                    let space_in_chunk = chunk_size - intra_off;
                    let write_len = (data_len - pos).min(space_in_chunk);

                    let slice = &data_vec[pos..pos + write_len];
                    let key = (ino, chunk_idx);

                    // RMW-on-buffer-init: if no in-memory buffer exists for this chunk but
                    // the server already has committed data at this file_offset, seed the
                    // buffer by fetching the existing chunk before applying the new write.
                    // Without this, a zero-initialized buffer would overwrite prior data.
                    if !write_chunk_buffers.contains_key(&key) {
                        let file_offset = chunk_file_start;
                        debug!("write new buffer: ino={} chunk_idx={} file_offset={}", ino, chunk_idx, file_offset);

                        // Check if the cluster already has a chunk at this file_offset.
                        let server_chunk: Option<(usize, ChunkId, Vec<dfs_common::ChunkLocation>)> = {
                            metadata_cache.get(&ino).and_then(|m| {
                                let locs = m.chunk_locations.clone();
                                // Find the chunk whose file_offset matches this chunk slot.
                                // chunk_locations and chunks are parallel arrays.
                                locs.iter().enumerate().find_map(|(i, loc)| {
                                    if loc.file_offset == Some(file_offset) {
                                        // Get the corresponding chunk ID from m.chunks
                                        m.chunks.get(i).map(|&cid| (i, cid, locs.clone()))
                                    } else {
                                        None
                                    }
                                })
                            })
                        };

                        debug!("write RMW check: ino={} chunk_idx={} server_chunk found={}", ino, chunk_idx, server_chunk.is_some());
                        let (buf_data, logical_len) = if let Some((chunk_arr_idx, chunk_id, chunk_locs)) = server_chunk {
                            let all_chunks = {
                                metadata_cache.get(&ino).map(|m| m.chunks.clone()).unwrap_or_default()
                            };
                            let hint = crate::client::ChunkReadHint {
                                chunk_idx: chunk_arr_idx,
                                chunk_id,
                                full_chunk: true,
                                offset_in_chunk: 0,
                                length: chunk_size,
                                file_offset,
                            };
                            match runtime.block_on(client.read_data(&[hint], &all_chunks, ino, &chunk_locs)) {
                                Ok(data) => {
                                    let copy_len = data.len().min(chunk_size);
                                    let mut buf = vec![0u8; chunk_size];
                                    buf[..copy_len].copy_from_slice(&data[..copy_len]);
                                    let llen = copy_len;
                                    debug!("write RMW seed: ino={} chunk_idx={} loaded {} bytes from server", ino, chunk_idx, copy_len);
                                    (buf, llen)
                                }
                                Err(e) => {
                                    error!("write RMW seed: failed to read chunk ino={} offset={}: {}", ino, file_offset, e);
                                    (vec![0u8; chunk_size], 0)
                                }
                            }
                        } else {
                            (vec![0u8; chunk_size], 0)
                        };

                        write_chunk_buffers.entry(key).or_insert_with(|| {
                            Arc::new(Mutex::new(ChunkBuffer {
                                data: buf_data,
                                dirty: false,
                                last_modified: SystemTime::now(),
                                logical_len,
                            }))
                        });
                    }

                    let buf_lock = write_chunk_buffers.get(&key).unwrap().clone();

                    runtime.block_on(async {
                        let mut buf = buf_lock.lock().await;
                        buf.data[intra_off..intra_off + write_len].copy_from_slice(slice);
                        buf.dirty = true;
                        buf.last_modified = SystemTime::now();
                        let new_logical = (intra_off + write_len).max(buf.logical_len);
                        buf.logical_len = new_logical;
                    });

                    pos += write_len;
                }

                // Update logical file size in metadata cache (no cluster I/O here).
                let new_end = offset + data_len as u64;
                if let Some(mut meta) = metadata_cache.get_mut(&ino) {
                    if new_end > meta.size {
                        meta.size = new_end;
                        meta.modified_at = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default().as_secs();
                    }
                }
                chunk_offset_cache.remove(&ino);

                // Inline flush-on-full: when a chunk just became completely full, pipeline
                // it to the cluster while the app continues writing the next chunk.
                // Uses a per-inode semaphore (2 permits) so at most 2 chunks are in-flight
                // simultaneously.  Acquiring a permit blocks when both are taken, providing
                // back pressure that caps write speed to network throughput.
                // Only full chunks (logical_len == chunk_size) are flushed here; partial
                // tail chunks wait for fsync/release.
                let full_dirty_keys: Vec<u64> = {
                    write_chunk_buffers.iter()
                        .filter_map(|entry| {
                            let &(ino_key, cidx) = entry.key();
                            if ino_key != ino { return None; }
                            if let Ok(buf) = entry.value().try_lock() {
                                if buf.dirty && buf.logical_len == chunk_size {
                                    return Some(cidx);
                                }
                            }
                            None
                        })
                        .collect()
                };
                for cidx in full_dirty_keys {
                    let key = (ino, cidx);
                    let flush_data = write_chunk_buffers.get(&key).and_then(|lock| {
                        lock.try_lock().ok().and_then(|mut buf| {
                            if buf.dirty && buf.logical_len == chunk_size {
                                let data = buf.data.clone();
                                buf.dirty = false;
                                Some(data)
                            } else {
                                None
                            }
                        })
                    });
                    if let Some(data) = flush_data {
                        let file_offset = cidx * chunk_size as u64;

                        // Acquire a semaphore permit — blocks if 2 flushes are already
                        // in-flight, providing back pressure to the writer.
                        let sem = flush_semaphores
                            .entry(ino)
                            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(2)))
                            .clone();
                        let permit = runtime.block_on(sem.acquire_owned())
                            .expect("semaphore closed");

                        let client_t = client.clone();
                        let metadata_t = metadata_cache.clone();
                        let buffers_t = write_chunk_buffers.clone();

                        debug!("inline flush spawn: ino={} chunk_idx={} offset={}", ino, cidx, file_offset);
                        runtime.spawn(async move {
                            match client_t.write_data_with_cache(&data, ino, file_offset).await {
                                Ok((_, _, locations_opt)) => {
                                    if let Some(locs) = locations_opt {
                                        Self::update_metadata_with_chunk(
                                            &metadata_t, ino, &locs, file_offset, chunk_size, true,
                                        );
                                    }
                                    // Persist metadata after each inline flush so dfs-admin
                                    // and other clients see the current file size during
                                    // long-running open writes (e.g. live DVR recordings).
                                    if let Some(meta) = metadata_t.get(&ino).map(|m| m.clone()) {
                                        if let Err(e) = client_t.put_file_metadata(&meta).await {
                                            error!("inline flush: metadata persist failed ino={}: {}", ino, e);
                                        }
                                    }
                                    debug!("inline flush done: ino={} chunk_idx={}", ino, cidx);
                                }
                                Err(e) => {
                                    // Restore dirty so release/fsync retries
                                    if let Some(lock) = buffers_t.get(&key) {
                                        if let Ok(mut buf) = lock.try_lock() {
                                            buf.dirty = true;
                                        }
                                    }
                                    error!("inline flush failed: ino={} chunk_idx={}: {}", ino, cidx, e);
                                }
                            }
                            drop(permit); // release back-pressure slot
                        });
                    }
                }

                let elapsed = start.elapsed();
                debug!("BUFFERED write() ino={} offset={} size={} took {:?}", ino, offset, data_len, elapsed);
                reply.written(data_len as u32);
                return;
            }

            // --- Direct write path (SQLite / buffering disabled) ---
            // Read-modify-write for overwrites, direct write for appends.
            let current_size = metadata_cache.get(&ino).map(|m| m.size as usize).unwrap_or(0);
            let offset_usize = offset as usize;

            let (new_data, write_offset) = if offset_usize == current_size || offset_usize > current_size {
                // Append or sparse write
                let mut d = if offset_usize > current_size {
                    let mut pad = vec![0u8; offset_usize - current_size];
                    pad.extend_from_slice(&data_vec);
                    pad
                } else {
                    data_vec.clone()
                };
                (d, current_size as u64)
            } else {
                // Overwrite: read affected chunks, merge, write back
                let metadata = metadata_cache.get(&ino).unwrap().clone();
                let write_end = offset_usize + data_len;
                let chunk_ids = metadata.chunks.clone();
                let chunk_sizes = metadata.chunk_sizes.clone();

                if chunk_ids.is_empty() {
                    (data_vec.clone(), offset as u64)
                } else {
                    let locations_match = !metadata.chunk_locations.is_empty()
                        && metadata.chunk_locations.len() == metadata.chunks.len()
                        && metadata.chunk_locations[0].file_offset.is_some();

                    let mut first_idx: Option<usize> = None;
                    let mut last_idx: Option<usize> = None;
                    let mut cumulative = 0u64;
                    for (i, &csz) in chunk_sizes.iter().enumerate() {
                        let cs = if locations_match {
                            metadata.chunk_locations[i].file_offset.unwrap_or(cumulative)
                        } else { cumulative };
                        let ce = cs + csz;
                        if ce > offset as u64 && cs < write_end as u64 {
                            if first_idx.is_none() { first_idx = Some(i); }
                            last_idx = Some(i);
                        }
                        cumulative += csz;
                    }

                    if let (Some(fi), Some(li)) = (first_idx, last_idx) {
                        let first_file_off: u64 = if locations_match {
                            metadata.chunk_locations[fi].file_offset.unwrap_or_else(|| chunk_sizes[..fi].iter().sum())
                        } else { chunk_sizes[..fi].iter().sum() };

                        let affected_ids: Vec<_> = chunk_ids[fi..=li].to_vec();
                        let affected_sizes: Vec<_> = chunk_sizes[fi..=li].to_vec();

                        let mut hints = Vec::new();
                        let mut co = first_file_off;
                        for (i, &cid) in affected_ids.iter().enumerate() {
                            hints.push(crate::client::ChunkReadHint {
                                chunk_idx: fi + i, chunk_id: cid, full_chunk: true,
                                offset_in_chunk: 0, length: affected_sizes[i] as usize,
                                file_offset: co,
                            });
                            co += affected_sizes[i];
                        }

                        let affected_data = match runtime.block_on(async {
                            client.read_data(&hints, &chunk_ids, cache_inode, &metadata.chunk_locations).await
                        }) {
                            Ok(d) => d,
                            Err(e) => {
                                error!("write RMW read failed for ino={}: {}", ino, e);
                                reply.error(libc::EIO);
                                return;
                            }
                        };

                        let rel_off = (offset_usize as u64 - first_file_off) as usize;
                        let mut merged = affected_data;
                        if rel_off + data_len > merged.len() {
                            merged.resize(rel_off + data_len, 0);
                        }
                        merged[rel_off..rel_off + data_len].copy_from_slice(&data_vec);
                        (merged, first_file_off)
                    } else {
                        (data_vec.clone(), offset as u64)
                    }
                }
            };

            let result = runtime.block_on(async {
                client.write_data_with_cache(&new_data, cache_inode, write_offset).await
            });

            match result {
                Ok((new_ids, new_szs, locs_opt)) => {
                    let mut meta = metadata_cache.get(&ino).unwrap().clone();
                    if let Some(locs) = locs_opt {
                        meta.chunk_locations.extend(locs);
                    }
                    meta.chunks.extend(new_ids);
                    meta.chunk_sizes.extend(new_szs);
                    meta.size = (write_offset + new_data.len() as u64).max(meta.size);
                    meta.modified_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

                    if let Err(e) = runtime.block_on(client.put_file_metadata(&meta)) {
                        error!("write: failed to persist metadata for ino={}: {}", ino, e);
                        reply.error(libc::EIO);
                        return;
                    }
                    chunk_offset_cache.remove(&ino);
                    metadata_cache.insert(ino, meta);
                    reply.written(data_len as u32);
                }
                Err(e) => {
                    error!("write: cluster write failed for ino={}: {}", ino, e);
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
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let write_counters = self.write_counters.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let runtime = self.runtime.clone();

        // Spawn flush operation on tokio's blocking thread pool
        runtime.clone().spawn_blocking(move || {
            debug!("flush: ino={}", ino);

            if write_buffer_enabled {
                // Chunk buffers are flushed by fsync() and release().
                // flush() fires on every close() (including read-only) so we just ack.
                debug!("flush: ino={} - deferring to fsync/release", ino);
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

        if self.write_buffer_enabled && is_write {
            // Only flush chunk buffers when a write-mode fd is being closed.
            // Read-only closes must NOT flush — doing so would write zero-padded partial
            // chunks to the cluster and corrupt data being written by concurrent writers.
            let result = self.block_on(async {
                self.flush_chunks_for_inode(ino, true).await?;
                if let Some(owner) = lock_owner {
                    lock_manager.release_all(ino, owner).await?;
                }
                Ok::<(), anyhow::Error>(())
            });

            match result {
                Ok(_) => {
                    // Only remove chunk buffers after a confirmed successful flush.
                    // If we removed them on error, dirty data would be silently lost.
                    let keys_to_remove: Vec<(u64, u64)> = self.write_chunk_buffers.iter()
                        .filter(|e| e.key().0 == ino)
                        .map(|e| *e.key())
                        .collect();
                    for k in keys_to_remove {
                        self.write_chunk_buffers.remove(&k);
                    }
                    reply.ok();
                }
                Err(e) => {
                    error!("release: flush failed for inode {}: {}", ino, e);
                    // Leave buffers dirty so fsync can retry — do NOT remove them.
                    reply.error(libc::EIO);
                }
            }
        } else {
            // Read-only close or buffering disabled: release locks, no flush.
            if is_write {
                // Buffering disabled: persist metadata
                let client = self.client.clone();
                let metadata_cache = self.metadata_cache.clone();
                let result = self.block_on(async {
                    if let Some(meta) = metadata_cache.get(&ino).map(|m| m.clone()) {
                        client.put_file_metadata(&meta).await?;
                    }
                    if let Some(owner) = lock_owner {
                        lock_manager.release_all(ino, owner).await?;
                    }
                    Ok::<(), anyhow::Error>(())
                });
                match result {
                    Ok(_) => reply.ok(),
                    Err(e) => {
                        error!("release: metadata persist failed for inode {}: {}", ino, e);
                        reply.error(libc::EIO);
                    }
                }
            } else {
                if let Some(owner) = lock_owner {
                    let _ = self.block_on(lock_manager.release_all(ino, owner));
                }
                reply.ok();
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

        // Store metadata on cluster — spawn so we never block_on the FUSE dispatch thread.
        let client = self.client.clone();
        let metadata_clone = metadata.clone();
        let metadata_cache = self.metadata_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let next_inode = self.next_inode.clone();

        self.runtime.spawn(async move {
            match client.put_file_metadata(&metadata_clone).await {
                Ok(_) => {
                    let ino = {
                        let path_map = path_to_inode.read().unwrap();
                        if let Some(&existing) = path_map.get(&path) {
                            existing
                        } else {
                            drop(path_map);
                            let mut next = next_inode.write().unwrap();
                            let v = *next; *next += 1; drop(next);
                            path_to_inode.write().unwrap().insert(path.clone(), v);
                            v
                        }
                    };
                    metadata_cache.insert(ino, metadata.clone());

                    let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                    let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                    dir_cache.remove(parent_path);

                    let attr = DfsFilesystem::metadata_to_attr_static(ino, &metadata);
                    reply.entry(&Duration::from_secs(1), &attr, 0);
                }
                Err(e) => {
                    error!("Failed to create directory {}: {}", path, e);
                    reply.error(libc::EIO);
                }
            }
        });
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

        // Delete file from cluster — spawn so we never block_on the FUSE dispatch thread.
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let write_chunk_buffers_ul = self.write_chunk_buffers.clone();
        let write_counters = self.write_counters.clone();
        let last_metadata_update = self.last_metadata_update.clone();
        let last_warm_offset = self.last_warm_offset.clone();
        let chunk_offset_cache = self.chunk_offset_cache.clone();

        self.runtime.spawn(async move {
            match client.delete_file(&path).await {
                Ok(_) => {
                    if let Some(&ino) = path_to_inode.read().unwrap().get(&path) {
                        metadata_cache.remove(&ino);
                        // Remove all chunk buffers for this inode
                        let keys: Vec<_> = write_chunk_buffers_ul.iter()
                            .filter(|e| e.key().0 == ino)
                            .map(|e| *e.key())
                            .collect();
                        for k in keys { write_chunk_buffers_ul.remove(&k); }
                        write_counters.write().unwrap().remove(&ino);
                        last_metadata_update.remove(&ino);
                        last_warm_offset.remove(&ino);
                        chunk_offset_cache.remove(&ino);
                    }
                    path_to_inode.write().unwrap().remove(&path);

                    let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                    let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                    dir_cache.remove(parent_path);

                    reply.ok();
                }
                Err(e) => {
                    error!("Failed to delete file {}: {}", path, e);
                    reply.error(libc::EIO);
                }
            }
        });
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

        // Check if directory is empty then delete — spawn so we never block_on the FUSE dispatch thread.
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let path_to_inode = self.path_to_inode.clone();

        self.runtime.spawn(async move {
            match client.list_directory(&path).await {
                Ok(entries) => {
                    if !entries.is_empty() {
                        reply.error(libc::ENOTEMPTY);
                        return;
                    }
                    match client.delete_file(&path).await {
                        Ok(_) => {
                            if let Some(&ino) = path_to_inode.read().unwrap().get(&path) {
                                metadata_cache.remove(&ino);
                            }
                            path_to_inode.write().unwrap().remove(&path);

                            let raw_parent = path.rsplitn(2, '/').nth(1).unwrap_or("");
                            let parent_path = if raw_parent.is_empty() { "/" } else { raw_parent };
                            dir_cache.remove(parent_path);

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
        });
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

        // Get existing metadata then rename — spawn so we never block_on the FUSE dispatch thread.
        let client = self.client.clone();
        let metadata_cache = self.metadata_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let path_to_inode = self.path_to_inode.clone();
        let next_inode = self.next_inode.clone();

        self.runtime.spawn(async move {
            match client.get_file_metadata(&old_path).await {
                Ok(Some(metadata)) => {
                    match client.rename_file(&old_path, &new_path).await {
                        Ok(_) => {
                            // Keep the same inode number — kernel still holds references to it.
                            let ino = {
                                let path_map = path_to_inode.read().unwrap();
                                if let Some(&existing) = path_map.get(&old_path) {
                                    existing
                                } else {
                                    drop(path_map);
                                    let mut next = next_inode.write().unwrap();
                                    let v = *next; *next += 1; drop(next);
                                    path_to_inode.write().unwrap().insert(old_path.clone(), v);
                                    v
                                }
                            };

                            path_to_inode.write().unwrap().remove(&old_path);
                            path_to_inode.write().unwrap().insert(new_path.clone(), ino);

                            let mut new_metadata = metadata.clone();
                            new_metadata.path = new_path.clone();
                            new_metadata.modified_at = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs();
                            metadata_cache.insert(ino, new_metadata);

                            let raw_old = old_path.rsplitn(2, '/').nth(1).unwrap_or("");
                            let old_parent = if raw_old.is_empty() { "/" } else { raw_old };
                            let raw_new = new_path.rsplitn(2, '/').nth(1).unwrap_or("");
                            let new_parent = if raw_new.is_empty() { "/" } else { raw_new };
                            dir_cache.remove(old_parent);
                            if old_parent != new_parent {
                                dir_cache.remove(new_parent);
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
                Ok(None) => reply.error(libc::ENOENT),
                Err(e) => {
                    error!("Failed to get file metadata {}: {}", old_path, e);
                    reply.error(libc::EIO);
                }
            }
        });
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

        if let Some((total, free, avail)) = cached {
            reply.statfs(total, free, avail, 0, 0, BLOCK_SIZE, 255, BLOCK_SIZE);
            return;
        }

        // Cache miss — spawn so we never block_on the FUSE dispatch thread.
        debug!("statfs cache MISS - querying cluster");
        let client = self.client.clone();
        let statfs_cache = self.statfs_cache.clone();

        self.runtime.spawn(async move {
            match client.get_storage_stats().await {
                Ok((total_space, free_space, available_space, _replication_factor)) => {
                    let total = total_space / BLOCK_SIZE as u64;
                    let free = free_space / BLOCK_SIZE as u64;
                    let avail = available_space / BLOCK_SIZE as u64;
                    *statfs_cache.write().unwrap() = Some((total, free, avail, std::time::Instant::now()));
                    reply.statfs(total, free, avail, 0, 0, BLOCK_SIZE, 255, BLOCK_SIZE);
                }
                Err(e) => {
                    error!("Failed to get storage stats: {}", e);
                    reply.statfs(1_000_000_000, 500_000_000, 500_000_000, 0, 0, BLOCK_SIZE, 255, BLOCK_SIZE);
                }
            }
        });
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
            // Spawn flush on the tokio runtime — must NOT block the FUSE dispatch thread.
            // flush_chunks_for_inode flushes all dirty chunk buffers (force=true includes
            // partial tail chunks).  The per-chunk write_data_with_cache calls are
            // fully independent and race-free — no AppendFile, no OffsetMismatch.
            let result = self.block_on(self.flush_chunks_for_inode(ino, true));
            match result {
                Ok(_) => reply.ok(),
                Err(e) => {
                    error!("fsync: flush failed for inode {}: {}", ino, e);
                    reply.error(libc::EIO);
                }
            }
        } else {
            // Buffering disabled: persist any cached metadata updates.
            let client = self.client.clone();
            let metadata_cache = self.metadata_cache.clone();
            let write_counters = self.write_counters.clone();
            let result = self.block_on(async {
                if let Some(metadata) = metadata_cache.get(&ino).map(|m| m.clone()) {
                    let has_pending = write_counters.read().unwrap()
                        .get(&ino).map(|c| *c > 0).unwrap_or(false);
                    if has_pending {
                        client.put_file_metadata(&metadata).await?;
                        write_counters.write().unwrap().insert(ino, 0);
                    }
                }
                Ok::<(), anyhow::Error>(())
            });
            match result {
                Ok(_) => reply.ok(),
                Err(e) => {
                    error!("fsync: metadata persist failed for inode {}: {}", ino, e);
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
