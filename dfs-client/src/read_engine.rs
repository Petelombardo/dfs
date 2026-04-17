use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
        })
    }

    /// Returns true if the chunk map needs a refresh (too old or file grew).
    pub async fn needs_refresh(&self, current_size: u64) -> bool {
        let known = self.known_size.load(Ordering::Relaxed) as u64;
        if current_size > known {
            return true;
        }
        let last = self.last_map_refresh.lock().await;
        last.elapsed() > std::time::Duration::from_secs(5)
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
        *cm = Arc::new(locations);
        *co = Arc::new(offsets);
        *nim = node_map;
        *lr = std::time::Instant::now();
        self.known_size.store(file_size as usize, Ordering::Relaxed);
        info!("Engine inode={}: chunk map updated ({} chunks, {} bytes)",
              self.inode, cm.len(), file_size);
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
        // Only advance forward
        let start = old.max(target);
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
