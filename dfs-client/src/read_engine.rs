use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use moka::future::Cache as MokaCache;

use dashmap::DashSet;
use dashmap::DashMap;
use dfs_common::{ChunkId, ChunkLocation};
use tracing::{debug, info};

/// Precomputed (file_byte_offset, chunk_size) for every chunk in the file.
/// Built once from chunk_locations and reused for all reads.
pub type ChunkOffsets = Arc<Vec<(usize, usize)>>;

/// Per-inode read engine. One instance lives as long as any fd has the file open.
///
/// Lock-free design: all read-path fields use ArcSwap (atomic pointer swap) or atomics.
/// Writers swap in new Arcs; concurrent readers load without blocking or sleeping.
/// `in_flight` uses DashSet (sharded lock-free hash set) to avoid a single hot Mutex.
pub struct InodeReadEngine {
    pub inode: u64,

    /// Current chunk location snapshot. RwLock: many concurrent readers, rare writer.
    /// std::sync::RwLock (not tokio's) — readers acquire a shared read lock without
    /// incrementing any Arc refcount, avoiding cache-line bounce under high concurrency.
    chunk_map: RwLock<Arc<Vec<ChunkLocation>>>,

    /// Precomputed (file_byte_offset, chunk_byte_size) parallel to chunk_map.
    chunk_offsets: RwLock<Arc<Vec<(usize, usize)>>>,

    /// Cached file size at the time chunk_map was last built. Used to detect growth.
    pub known_size: AtomicUsize,

    /// When chunk_map was last fetched (ms since UNIX epoch).
    last_map_refresh_ms: AtomicU64,

    /// NodeId -> SocketAddr snapshot taken at engine-init / refresh time.
    node_id_to_addr: RwLock<Arc<HashMap<dfs_common::NodeId, SocketAddr>>>,

    /// Chunks currently being fetched by this engine (prevents duplicate concurrent fetches).
    /// DashSet is sharded — no single lock, scales with CPU count.
    pub in_flight: DashSet<ChunkId>,

    /// Index of the next chunk the pipeline should speculatively fetch.
    pub pipeline_head: AtomicUsize,

    /// How many chunks to keep ahead of the read head.
    pub pipeline_depth: usize,

    /// Set while a refresh is in progress; prevents duplicate concurrent refreshes.
    pub refresh_in_progress: AtomicBool,

    /// Monotonic ms timestamp of the last refresh that returned no chunk map from the leader.
    pub last_failed_refresh_ms: AtomicU64,

    /// Chunk index range [window_start, window_end) covered by the last windowed fetch.
    pub last_window_start: AtomicU32,
    pub last_window_end: AtomicU32,

    /// Last chunk fetch duration in milliseconds (EMA). Used for adaptive stagger delay.
    pub last_chunk_fetch_ms: AtomicU64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl InodeReadEngine {
    pub fn new(inode: u64) -> Arc<Self> {
        // Start last_map_refresh_ms 60s in the past to force immediate refresh.
        let stale_ms = now_ms().saturating_sub(60_000);
        Arc::new(Self {
            inode,
            chunk_map: RwLock::new(Arc::new(Vec::new())),
            chunk_offsets: RwLock::new(Arc::new(Vec::new())),
            known_size: AtomicUsize::new(0),
            last_map_refresh_ms: AtomicU64::new(stale_ms),
            node_id_to_addr: RwLock::new(Arc::new(HashMap::new())),
            in_flight: DashSet::new(),
            pipeline_head: AtomicUsize::new(0),
            pipeline_depth: 1,
            refresh_in_progress: AtomicBool::new(false),
            last_failed_refresh_ms: AtomicU64::new(0),
            last_window_start: AtomicU32::new(0),
            last_window_end: AtomicU32::new(0),
            last_chunk_fetch_ms: AtomicU64::new(50),
        })
    }

    /// Force-expire the chunk map TTL so the next needs_refresh() returns true.
    pub fn expire_chunk_map(&self) {
        let stale_ms = now_ms().saturating_sub(60_000);
        self.last_map_refresh_ms.store(stale_ms, Ordering::Relaxed);
    }

    /// Async version — also clears the map data (for open() after recording finishes).
    pub async fn expire_chunk_map_async(&self) {
        *self.chunk_map.write().unwrap() = Arc::new(Vec::new());
        *self.chunk_offsets.write().unwrap() = Arc::new(Vec::new());
        self.expire_chunk_map();
        self.last_window_start.store(u32::MAX, Ordering::Relaxed);
        self.last_window_end.store(0, Ordering::Relaxed);
        self.known_size.store(0, Ordering::Relaxed);
    }

    /// Returns true if the chunk map needs a refresh.
    pub fn needs_refresh(&self, current_size: u64, current_chunk: u32) -> bool {
        const FAILED_BACKOFF_MS: u64 = 1000;
        let last_fail = self.last_failed_refresh_ms.load(Ordering::Relaxed);
        if last_fail > 0 {
            if now_ms().saturating_sub(last_fail) < FAILED_BACKOFF_MS {
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
        let ws = self.last_window_start.load(Ordering::Relaxed);
        let we = self.last_window_end.load(Ordering::Relaxed);
        if current_chunk < ws || current_chunk >= we {
            return true;
        }
        let age_ms = now_ms().saturating_sub(self.last_map_refresh_ms.load(Ordering::Relaxed));
        age_ms > 5_000
    }

    pub fn record_failed_refresh(&self) {
        self.last_failed_refresh_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn clear_failed_refresh(&self) {
        self.last_failed_refresh_ms.store(0, Ordering::Relaxed);
    }

    /// Replace the chunk map snapshot with fresh data from the leader.
    pub fn update_chunk_map(
        &self,
        locations: Vec<ChunkLocation>,
        node_map: Arc<HashMap<dfs_common::NodeId, SocketAddr>>,
        file_size: u64,
    ) {
        let offsets = build_offsets(&locations);
        let loc_len = locations.len() as u32;
        *self.chunk_map.write().unwrap() = Arc::new(locations);
        *self.chunk_offsets.write().unwrap() = Arc::new(offsets);
        *self.node_id_to_addr.write().unwrap() = node_map;
        self.last_map_refresh_ms.store(now_ms(), Ordering::Relaxed);
        self.known_size.store(file_size as usize, Ordering::Relaxed);
        self.last_window_start.store(0, Ordering::Relaxed);
        self.last_window_end.store(loc_len, Ordering::Relaxed);
        info!("Engine inode={}: chunk map updated ({} chunks, {} bytes)", self.inode, loc_len, file_size);
    }

    /// Merge a windowed chunk map response into the engine's full map.
    pub fn update_chunk_map_window(
        &self,
        window: Vec<ChunkLocation>,
        from_chunk: u32,
        total_chunks: u32,
        node_map: Arc<HashMap<dfs_common::NodeId, SocketAddr>>,
        file_size: u64,
    ) {
        let from = from_chunk as usize;
        let total = total_chunks as usize;

        let mut new_map: Vec<ChunkLocation> = (**self.chunk_map.read().unwrap()).clone();

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

        const CHUNK_SIZE_U64: u64 = 4 * 1024 * 1024;
        let window_len = window.len() as u32;
        for loc in window.into_iter() {
            let idx = if let Some(offset) = loc.file_offset {
                (offset / CHUNK_SIZE_U64) as usize
            } else {
                from
            };
            if idx < new_map.len() {
                new_map[idx] = loc;
            }
        }
        while new_map.last().map(|l| l.chunk_id.hash == [0u8; 32]).unwrap_or(false) {
            new_map.pop();
        }

        let offsets = build_offsets(&new_map);
        let window_end = from_chunk + window_len;
        info!("Engine inode={}: chunk map window merged ({} chunks, {} bytes, window from={} end={})",
              self.inode, new_map.len(), file_size, from_chunk, window_end);

        *self.chunk_map.write().unwrap() = Arc::new(new_map);
        *self.chunk_offsets.write().unwrap() = Arc::new(offsets);
        *self.node_id_to_addr.write().unwrap() = node_map;
        self.last_map_refresh_ms.store(now_ms(), Ordering::Relaxed);
        self.known_size.store(file_size as usize, Ordering::Relaxed);
        self.last_window_start.store(from_chunk, Ordering::Relaxed);
        self.last_window_end.store(window_end, Ordering::Relaxed);
    }

    /// Cheap snapshot: acquires three read locks, clones three Arcs, releases locks.
    /// Read locks are shared — many concurrent readers proceed without blocking each other.
    /// Writers (refresh path) are rare and brief; they hold write locks only while swapping Arcs.
    pub fn snapshot(&self) -> (Arc<Vec<ChunkLocation>>, Arc<Vec<(usize, usize)>>,
                               Arc<HashMap<dfs_common::NodeId, SocketAddr>>) {
        let cm = self.chunk_map.read().unwrap().clone();
        let co = self.chunk_offsets.read().unwrap().clone();
        let nim = self.node_id_to_addr.read().unwrap().clone();
        (cm, co, nim)
    }

    /// Returns true if the chunk map is currently empty.
    pub fn is_chunk_map_empty(&self) -> bool {
        self.chunk_map.read().unwrap().is_empty()
    }

    /// Find all chunk indices that overlap [offset, offset+size).
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

    pub fn resolve_primary(
        loc: &ChunkLocation,
        nim: &HashMap<dfs_common::NodeId, SocketAddr>,
        fallback_nodes: &[SocketAddr],
        selector: u64,
    ) -> Option<(SocketAddr, Vec<SocketAddr>)> {
        let addrs: Vec<SocketAddr> = loc.nodes.iter()
            .filter_map(|nid| nim.get(nid).copied())
            .collect();
        if addrs.is_empty() {
            return None;
        }
        let primary_idx = selector as usize % addrs.len();
        let primary = addrs[primary_idx];
        let fallbacks = addrs.iter().copied()
            .chain(fallback_nodes.iter().copied())
            .filter(|&a| a != primary)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        Some((primary, fallbacks))
    }

    /// Determine lookahead chunks to speculatively fetch.
    /// Returns candidate lookahead (idx, cid) pairs that are not already in-flight.
    /// Callers must check chunk_cache themselves (async) before actually fetching.
    /// Does NOT insert into in_flight — caller does that after the cache check.
    pub fn pipeline_lookahead(
        &self,
        last_required_idx: usize,
        chunk_map_len: usize,
        chunk_map: &[ChunkLocation],
    ) -> Vec<(usize, ChunkId)> {
        let depth = self.pipeline_depth;
        let mut result = Vec::with_capacity(depth);
        for i in 1..=depth {
            let la_idx = last_required_idx + i;
            if la_idx >= chunk_map_len {
                break;
            }
            let la_cid = chunk_map[la_idx].chunk_id;
            if !self.in_flight.contains(&la_cid) {
                result.push((la_idx, la_cid));
            }
        }
        result
    }
}

/// Map of per-inode read engines. Lives for the client's lifetime.
#[derive(Clone)]
pub struct ReadEngineMap {
    pub map: DashMap<u64, Arc<InodeReadEngine>>,
}

impl ReadEngineMap {
    pub fn new() -> Self {
        Self { map: DashMap::new() }
    }

    pub fn get_or_create(&self, inode: u64) -> Arc<InodeReadEngine> {
        self.map
            .entry(inode)
            .or_insert_with(|| InodeReadEngine::new(inode))
            .clone()
    }

    pub fn get(&self, inode: u64) -> Option<Arc<InodeReadEngine>> {
        self.map.get(&inode).map(|e| Arc::clone(&*e))
    }

    pub fn remove(&self, inode: u64) {
        self.map.remove(&inode);
    }
}

fn build_offsets(locations: &[ChunkLocation]) -> Vec<(usize, usize)> {
    locations.iter().map(|loc| {
        let start = loc.file_offset.unwrap_or(0) as usize;
        let size = loc.size as usize;
        (start, size)
    }).collect()
}
