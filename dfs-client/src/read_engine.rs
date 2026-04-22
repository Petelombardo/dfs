use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use dashmap::DashMap;
use dfs_common::{ChunkId, ChunkLocation};
use tokio::sync::Mutex;
use tracing::{debug, info};

/// Precomputed (file_byte_offset, chunk_size) for every chunk in the file.
/// Built once from chunk_locations and reused for all reads.
pub type ChunkOffsets = Arc<Vec<(usize, usize)>>;

/// Per-inode read engine.  One instance lives as long as any fd has the file open.
///
/// Write-independence guarantee:
///   - The engine never blocks writers.  Writers update `metadata_cache` (DashMap, per-key
///     Mutex) independently.  The engine's `chunk_map` is a snapshot; it ages out and
///     refreshes asynchronously so a stale snapshot is always safe to read (old data) and
///     never blocks a write.
///   - `pipeline_in_flight` uses its own Mutex that is never held across any await that
///     touches the write path.
pub struct InodeReadEngine {
    pub inode: u64,

    /// Current snapshot of chunk locations.  Wrapped in ArcSwap-style: readers take a
    /// cheap Arc clone; the refresh path swaps in a new Arc without blocking any reader.
    chunk_map: Mutex<Arc<Vec<ChunkLocation>>>,

    /// Precomputed (file_byte_offset, chunk_byte_size) parallel to chunk_map.
    chunk_offsets: Mutex<ChunkOffsets>,

    /// Cached file size at the time chunk_map was last built.  Used to detect growth.
    pub known_size: AtomicUsize,

    /// When chunk_map was last fetched from the leader.
    last_map_refresh: Mutex<std::time::Instant>,

    /// NodeId -> SocketAddr snapshot taken at engine-init / refresh time.
    node_id_to_addr: Mutex<Arc<std::collections::HashMap<dfs_common::NodeId, SocketAddr>>>,

    /// Chunks currently being fetched by this engine (prevents duplicate concurrent fetches).
    pub in_flight: Mutex<HashSet<ChunkId>>,

    /// Index of the next chunk the pipeline should speculatively fetch.
    /// Monotonically advances; never decremented (seeking backward just misses).
    pub pipeline_head: AtomicUsize,

    /// How many chunks to keep ahead of the read head.
    pub pipeline_depth: usize,

    /// Set while a refresh is in progress; prevents duplicate concurrent refreshes.
    pub refresh_in_progress: AtomicBool,

    /// Monotonic ms timestamp of the last refresh that returned no chunk map from the leader.
    /// Prevents hammering the leader when a file is being written and chunks aren't committed yet.
    pub last_failed_refresh_ms: AtomicU64,

    /// Chunk index range [window_start, window_end) covered by the last windowed fetch.
    /// Used to detect when a seek lands outside the cached window and force a re-fetch.
    pub last_window_start: AtomicU32,
    pub last_window_end: AtomicU32,
}

impl InodeReadEngine {
    pub fn new(inode: u64) -> Arc<Self> {
        Arc::new(Self {
            inode,
            chunk_map: Mutex::new(Arc::new(Vec::new())),
            chunk_offsets: Mutex::new(Arc::new(Vec::new())),
            known_size: AtomicUsize::new(0),
            last_map_refresh: Mutex::new(std::time::Instant::now()
                - std::time::Duration::from_secs(60)), // force immediate refresh
            node_id_to_addr: Mutex::new(Arc::new(std::collections::HashMap::new())),
            in_flight: Mutex::new(HashSet::new()),
            pipeline_head: AtomicUsize::new(0),
            pipeline_depth: 2,
            refresh_in_progress: AtomicBool::new(false),
            last_failed_refresh_ms: AtomicU64::new(0),
            last_window_start: AtomicU32::new(0),
            last_window_end: AtomicU32::new(0),
        })
    }

    /// Force-expire the chunk map TTL so the next needs_refresh() call returns true.
    /// Called on open() to guarantee a fresh fetch after recording finishes.
    pub fn expire_chunk_map(&self) {
        if let Ok(mut last) = self.last_map_refresh.try_lock() {
            *last = std::time::Instant::now() - std::time::Duration::from_secs(60);
        }
        // Also reset window bounds so needs_refresh triggers on chunk position too.
        self.last_window_start.store(u32::MAX, Ordering::Relaxed);
        self.last_window_end.store(0, Ordering::Relaxed);
    }

    /// Returns true if the chunk map needs a refresh.
    /// Triggers on: file grew by a chunk, TTL expired, or current read position is outside
    /// the last-fetched window (e.g. seek backward past the window start).
    pub async fn needs_refresh(&self, current_size: u64, current_chunk: u32) -> bool {
        // If the last refresh returned nothing from the leader, back off for 1 second before
        // trying again. Without this, concurrent reads on a live-recording file (where the
        // leader has no committed chunk map yet) spawn dozens of refresh RPCs per second.
        const FAILED_BACKOFF_MS: u64 = 1000;
        let last_fail = self.last_failed_refresh_ms.load(Ordering::Relaxed);
        if last_fail > 0 {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            if now_ms.saturating_sub(last_fail) < FAILED_BACKOFF_MS {
                return false;
            }
        }

        let known = self.known_size.load(Ordering::Relaxed) as u64;
        const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
        let new_chunk_count = (current_size + CHUNK_SIZE - 1) / CHUNK_SIZE;
        let known_chunk_count = (known + CHUNK_SIZE - 1) / CHUNK_SIZE;
        if new_chunk_count > known_chunk_count {
            return true;
        }
        // Seek landed outside the cached window — need to fetch the new region.
        let ws = self.last_window_start.load(Ordering::Relaxed);
        let we = self.last_window_end.load(Ordering::Relaxed);
        if current_chunk < ws || current_chunk >= we {
            return true;
        }
        let last = self.last_map_refresh.lock().await;
        last.elapsed() > std::time::Duration::from_secs(5)
    }

    /// Record that a refresh attempt returned no chunk map from the leader.
    /// Causes needs_refresh() to suppress retries for 1 second.
    pub fn record_failed_refresh(&self) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_failed_refresh_ms.store(now_ms, Ordering::Relaxed);
    }

    /// Clear the failed-refresh backoff (called when a refresh succeeds).
    pub fn clear_failed_refresh(&self) {
        self.last_failed_refresh_ms.store(0, Ordering::Relaxed);
    }

    /// Replace the chunk map snapshot with fresh data from the leader.
    /// Called only when `needs_refresh` returns true; concurrent readers on the old
    /// Arc are unaffected.
    pub async fn update_chunk_map(
        &self,
        locations: Vec<ChunkLocation>,
        node_map: Arc<std::collections::HashMap<dfs_common::NodeId, SocketAddr>>,
        file_size: u64,
    ) {
        let offsets = build_offsets(&locations);
        let mut cm = self.chunk_map.lock().await;
        let mut co = self.chunk_offsets.lock().await;
        let mut nim = self.node_id_to_addr.lock().await;
        let mut lr = self.last_map_refresh.lock().await;
        let loc_len = locations.len() as u32;
        *cm = Arc::new(locations);
        *co = Arc::new(offsets);
        *nim = node_map;
        *lr = std::time::Instant::now();
        self.known_size.store(file_size as usize, Ordering::Relaxed);
        self.last_window_start.store(0, Ordering::Relaxed);
        self.last_window_end.store(loc_len, Ordering::Relaxed);
        info!("Engine inode={}: chunk map updated ({} chunks, {} bytes)",
              self.inode, cm.len(), file_size);
    }

    /// Merge a windowed chunk map response into the engine's full map.
    /// Slots [from_chunk .. from_chunk+window.len()) are updated in place;
    /// slots outside the window are preserved from the previous snapshot.
    /// If `total_chunks` exceeds the current map length, the map is extended.
    pub async fn update_chunk_map_window(
        &self,
        window: Vec<ChunkLocation>,
        from_chunk: u32,
        total_chunks: u32,
        node_map: Arc<std::collections::HashMap<dfs_common::NodeId, SocketAddr>>,
        file_size: u64,
    ) {
        let from = from_chunk as usize;
        let total = total_chunks as usize;

        let mut cm = self.chunk_map.lock().await;
        let mut co = self.chunk_offsets.lock().await;
        let mut nim = self.node_id_to_addr.lock().await;
        let mut lr = self.last_map_refresh.lock().await;

        // Build updated map: start from existing, resize to total_chunks if needed.
        let mut new_map = (*cm).as_ref().clone();
        let nil_id = dfs_common::ChunkId::from_hash([0u8; 32]);
        if new_map.len() < total {
            new_map.resize(total, ChunkLocation {
                chunk_id: nil_id,
                nodes: Vec::new(),
                size: 0,
                checksum: [0u8; 32],
                file_offset: None,
                written_at: None,
            });
        }
        // Overwrite the refreshed window.
        let window_len = window.len() as u32;
        for (i, loc) in window.into_iter().enumerate() {
            let idx = from + i;
            if idx < new_map.len() {
                new_map[idx] = loc;
            }
        }
        // Drop placeholder slots at the tail that were never filled.
        while new_map.last().map(|l| l.chunk_id.hash == [0u8; 32]).unwrap_or(false) {
            new_map.pop();
        }

        let offsets = build_offsets(&new_map);
        let window_end = from_chunk + window_len;
        info!("Engine inode={}: chunk map window merged ({} chunks, {} bytes, window from={} end={})",
              self.inode, new_map.len(), file_size, from_chunk, window_end);
        *cm = Arc::new(new_map);
        *co = Arc::new(offsets);
        *nim = node_map;
        *lr = std::time::Instant::now();
        self.known_size.store(file_size as usize, Ordering::Relaxed);
        self.last_window_start.store(from_chunk, Ordering::Relaxed);
        self.last_window_end.store(window_end, Ordering::Relaxed);
    }

    /// Cheap snapshot — does not block writers.
    pub async fn snapshot(&self) -> (Arc<Vec<ChunkLocation>>, ChunkOffsets,
                                    Arc<std::collections::HashMap<dfs_common::NodeId, SocketAddr>>) {
        let cm = self.chunk_map.lock().await.clone();
        let co = self.chunk_offsets.lock().await.clone();
        let nim = self.node_id_to_addr.lock().await.clone();
        (cm, co, nim)
    }

    /// Find all chunk indices that overlap [offset, offset+size).
    /// Returns (chunk_idx, chunk_file_offset, chunk_size).
    pub fn chunks_for_range(
        offsets: &[(usize, usize)],
        offset: usize,
        size: usize,
    ) -> Vec<(usize, usize, usize)> {
        let end = offset + size;
        let mut result = Vec::new();
        for (idx, &(chunk_start, chunk_size)) in offsets.iter().enumerate() {
            let chunk_end = chunk_start + chunk_size;
            if chunk_end > offset && chunk_start < end {
                result.push((idx, chunk_start, chunk_size));
            }
            if chunk_start >= end {
                break;
            }
        }
        result
    }

    /// Resolve the primary SocketAddr for a chunk given a node_id_to_addr map.
    /// Falls back to round-robin across `fallback_nodes` when mapping is absent.
    pub fn resolve_primary(
        loc: &ChunkLocation,
        nim: &std::collections::HashMap<dfs_common::NodeId, SocketAddr>,
        fallback_nodes: &[SocketAddr],
        selector: u64,
    ) -> Option<(SocketAddr, Vec<SocketAddr>)> {
        let addrs: Vec<SocketAddr> = loc.nodes.iter()
            .filter_map(|nid| nim.get(nid).copied())
            .collect();

        if addrs.is_empty() {
            if fallback_nodes.is_empty() {
                return None;
            }
            let primary = fallback_nodes[(selector as usize) % fallback_nodes.len()];
            let fallbacks: Vec<SocketAddr> = fallback_nodes.iter()
                .filter(|&&a| a != primary).copied().collect();
            return Some((primary, fallbacks));
        }

        let primary = addrs[(selector as usize) % addrs.len()];
        let fallbacks: Vec<SocketAddr> = addrs.iter()
            .filter(|&&a| a != primary).copied().collect();
        Some((primary, fallbacks))
    }

    /// Advance the pipeline head and return chunk indices [old_head, old_head+depth)
    /// that are not yet in-flight or in `chunk_cache`.
    /// Caller is responsible for marking returned indices as in-flight.
    pub async fn pipeline_lookahead(
        &self,
        current_idx: usize,
        chunk_map_len: usize,
        chunk_cache: &tokio::sync::RwLock<lru::LruCache<ChunkId, Arc<Vec<u8>>>>,
        chunk_map: &[ChunkLocation],
    ) -> Vec<(usize, ChunkId)> {
        let target = current_idx + 1;
        let old = self.pipeline_head.load(Ordering::Relaxed);
        // Reset pipeline head on backward seek or large forward jump (e.g. seek to live edge).
        // A jump of more than pipeline_depth*4 chunks means we're somewhere new; reset so
        // prefetch resumes from the actual read position instead of the old position.
        let start = if old > target + self.pipeline_depth * 4 || target > old + self.pipeline_depth * 4 {
            self.pipeline_head.store(target, Ordering::Relaxed);
            debug!("Engine inode={}: pipeline head reset {} → {} (seek detected)",
                   self.inode, old, target);
            target
        } else {
            old.max(target)
        };
        let end = (start + self.pipeline_depth).min(chunk_map_len);

        if start >= end {
            return Vec::new();
        }

        let cache = chunk_cache.read().await;
        let mut in_flight = self.in_flight.lock().await;
        let mut result = Vec::new();

        for idx in start..end {
            let cid = chunk_map[idx].chunk_id;
            if cache.peek(&cid).is_none() && !in_flight.contains(&cid) {
                in_flight.insert(cid);
                result.push((idx, cid));
            }
        }

        if !result.is_empty() {
            self.pipeline_head.store(end, Ordering::Relaxed);
            debug!("Engine inode={}: pipeline lookahead {} chunks (head→{})",
                   self.inode, result.len(), end);
        }

        result
    }
}

pub fn build_offsets(locations: &[ChunkLocation]) -> Vec<(usize, usize)> {
    // Prefer explicit file_offset when all chunks have it (sparse support).
    let all_have = !locations.is_empty() && locations.iter().all(|l| l.file_offset.is_some());
    if all_have {
        locations.iter()
            .map(|l| (l.file_offset.unwrap() as usize, l.size))
            .collect()
    } else {
        let mut cur = 0usize;
        locations.iter().map(|l| {
            let start = cur;
            cur += l.size;
            (start, l.size)
        }).collect()
    }
}

/// Registry of per-inode read engines.  Stored on DfsClient so it survives
/// multiple FUSE open() calls on the same inode.
#[derive(Clone)]
pub struct ReadEngineRegistry {
    pub engines: Arc<DashMap<u64, Arc<InodeReadEngine>>>,
}

impl ReadEngineRegistry {
    pub fn new() -> Self {
        Self { engines: Arc::new(DashMap::new()) }
    }

    /// Get or create an engine for `inode`.
    pub fn get_or_create(&self, inode: u64) -> Arc<InodeReadEngine> {
        if let Some(e) = self.engines.get(&inode) {
            return e.clone();
        }
        let engine = InodeReadEngine::new(inode);
        self.engines.insert(inode, engine.clone());
        engine
    }

    /// Remove an engine when no fd has the file open any more.
    pub fn remove(&self, inode: u64) {
        self.engines.remove(&inode);
    }
}
