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

/// Chunk map + parallel offsets, always updated together under one lock.
/// Bundling them prevents a reader from seeing new chunk_map with old chunk_offsets
/// (or vice versa) between two separate write-lock acquisitions.
struct ChunkMapSnapshot {
    map:     Arc<Vec<ChunkLocation>>,
    offsets: Arc<Vec<(usize, usize)>>,
}

/// Per-inode read engine. One instance lives as long as any fd has the file open.
///
/// `chunk_state` is a single RwLock covering both map and offsets so they are
/// always swapped atomically.  All other read-path fields use atomics or DashSet.
pub struct InodeReadEngine {
    pub inode: u64,

    /// Chunk locations + precomputed offsets, always consistent with each other.
    chunk_state: RwLock<ChunkMapSnapshot>,

    /// Cached file size at the time chunk_map was last built. Used to detect growth.
    pub known_size: AtomicUsize,

    /// When chunk_map was last fetched (ms since UNIX epoch).
    last_map_refresh_ms: AtomicU64,

    /// NodeId → SocketAddr mapping. Updated alongside chunk_state but separately
    /// (node map changes are rare and harmless if briefly inconsistent with the chunk map).
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

    /// Byte offset one past the end of the last completed read. Used to detect sequential
    /// access: if the current read starts here (or within a small tolerance), it's sequential
    /// and should use the full-chunk path rather than range-fetch.
    pub last_read_end: AtomicU64,
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
            chunk_state: RwLock::new(ChunkMapSnapshot {
                map:     Arc::new(Vec::new()),
                offsets: Arc::new(Vec::new()),
            }),
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
            last_read_end: AtomicU64::new(0),
        })
    }

    /// Force-expire the chunk map TTL so the next needs_refresh() returns true.
    pub fn expire_chunk_map(&self) {
        let stale_ms = now_ms().saturating_sub(60_000);
        self.last_map_refresh_ms.store(stale_ms, Ordering::Relaxed);
    }

    /// Async version — also clears the map data (for open() after recording finishes).
    pub async fn expire_chunk_map_async(&self) {
        {
            let mut s = self.chunk_state.write().unwrap();
            s.map     = Arc::new(Vec::new());
            s.offsets = Arc::new(Vec::new());
        }
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
        {
            let mut s = self.chunk_state.write().unwrap();
            s.map     = Arc::new(locations);
            s.offsets = Arc::new(offsets);
        }
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

        let mut new_map: Vec<ChunkLocation> = (*self.chunk_state.read().unwrap().map).clone();

        let nil_id = dfs_common::ChunkId::from_hash([0u8; 32]);
        if new_map.len() < total {
            new_map.resize(total, ChunkLocation {
                chunk_id: nil_id,
                nodes: Vec::new(),
                size: 0,
                checksum: [0u8; 32],
                file_offset: None,
                written_at: None,
                client_write_seq: None,
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
                // Guard: only overwrite if the incoming data is at least as new as what
                // the engine already has. After a local patch, the engine's client_write_seq
                // for that chunk is >= what the server's sled knows (RCL is async). Without
                // this guard, every merge after an unrelated flush overwrites the engine's
                // correct local hash with the server's slightly-stale one.
                let should_update = match (loc.client_write_seq, new_map[idx].client_write_seq) {
                    (Some(inc), Some(ext)) => inc >= ext,
                    (Some(_), None)        => true,
                    (None, Some(_))        => false,
                    (None, None)           => true,
                };
                if should_update {
                    new_map[idx] = loc;
                }
            }
        }
        while new_map.last().map(|l| l.chunk_id.hash == [0u8; 32]).unwrap_or(false) {
            new_map.pop();
        }

        let offsets = build_offsets(&new_map);
        let window_end = from_chunk + window_len;
        info!("Engine inode={}: chunk map window merged ({} chunks, {} bytes, window from={} end={})",
              self.inode, new_map.len(), file_size, from_chunk, window_end);

        {
            let mut s = self.chunk_state.write().unwrap();
            s.map     = Arc::new(new_map);
            s.offsets = Arc::new(offsets);
        }
        *self.node_id_to_addr.write().unwrap() = node_map;
        self.last_map_refresh_ms.store(now_ms(), Ordering::Relaxed);
        self.known_size.store(file_size as usize, Ordering::Relaxed);
        self.last_window_start.store(from_chunk, Ordering::Relaxed);
        self.last_window_end.store(window_end, Ordering::Relaxed);
    }

    /// Cheap snapshot: one read lock for (map, offsets) atomically, one for nim.
    /// Readers always see map and offsets from the same write — never a torn state.
    pub fn snapshot(&self) -> (Arc<Vec<ChunkLocation>>, Arc<Vec<(usize, usize)>>,
                               Arc<HashMap<dfs_common::NodeId, SocketAddr>>) {
        let s = self.chunk_state.read().unwrap();
        let cm  = Arc::clone(&s.map);
        let co  = Arc::clone(&s.offsets);
        drop(s);
        let nim = self.node_id_to_addr.read().unwrap().clone();
        (cm, co, nim)
    }

    /// Returns true if the chunk map is currently empty.
    pub fn is_chunk_map_empty(&self) -> bool {
        self.chunk_state.read().unwrap().map.is_empty()
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
        _fallback_nodes: &[SocketAddr],
        _selector: u64,
    ) -> Option<(SocketAddr, Vec<SocketAddr>)> {
        // Only try nodes that actually hold this chunk. Cluster-wide fallback
        // (leader + all other nodes) caused spurious "not found on this node"
        // failures and false health penalties on nodes that simply don't hold
        // the chunk. Leader priority is for metadata reads only, not data reads.
        // Sort deterministically by address so the same chunk always routes to
        // the same primary, enabling warm server-side caches.
        let mut addrs: Vec<SocketAddr> = loc.nodes.iter()
            .filter_map(|nid| nim.get(nid).copied())
            .collect();
        if addrs.is_empty() {
            return None;
        }
        addrs.sort_unstable();
        let primary = addrs[0];
        let fallbacks = addrs[1..].to_vec();
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
