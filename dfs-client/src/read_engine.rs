use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

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

    /// Fired every time a refresh finishes (success or failure) and clears
    /// refresh_in_progress. A caller that loses the refresh_in_progress race in
    /// Client::refresh_engine awaits this instead of returning immediately with
    /// a possibly-still-empty snapshot — see refresh_engine's doc comment for
    /// the T28 regression this fixes (2026-07-09): concurrent cold-cache reads
    /// racing on refresh_in_progress let the loser fall through read_file's
    /// "chunk map still empty" sparse-hole path and return zeros for a real,
    /// non-sparse file.
    pub refresh_done: tokio::sync::Notify,

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
            pipeline_depth: 4,
            refresh_in_progress: AtomicBool::new(false),
            refresh_done: tokio::sync::Notify::new(),
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

    /// Single-chunk write-path update — O(1) per call instead of O(n).
    ///
    /// Uses Arc::make_mut to update map and offsets in-place under the write lock.
    /// When the write path is the sole Arc holder (common during sequential writes with
    /// no concurrent reads), make_mut skips the clone entirely. If a concurrent reader
    /// holds a snapshot Arc, make_mut clones — but that reader continues from its own
    /// Arc unaffected. Either way, correctness is maintained.
    pub fn update_single_chunk(
        &self,
        loc: ChunkLocation,
        file_size: u64,
        node_map: Arc<HashMap<dfs_common::NodeId, SocketAddr>>,
    ) {
        const CHUNK_SIZE_U64: u64 = 4 * 1024 * 1024;
        let idx = loc.file_offset
            .map(|o| (o / CHUNK_SIZE_U64) as usize)
            .unwrap_or(0);
        let offset_entry = (loc.file_offset.unwrap_or(0) as usize, loc.size);
        let nil_loc = ChunkLocation {
            chunk_id: dfs_common::ChunkId::from_hash([0u8; 32]),
            nodes: Vec::new(), size: 0, checksum: [0u8; 32],
            file_offset: None, written_at: None, client_write_seq: None, file_id: None,
        };

        let map_len = {
            let mut s = self.chunk_state.write().unwrap();
            let map = Arc::make_mut(&mut s.map);
            if idx >= map.len() {
                map.resize(idx + 1, nil_loc);
            }
            map[idx] = loc;
            // Trim trailing nil entries (sequential writes never leave nils at the end)
            while map.last().map(|l| l.chunk_id.hash == [0u8; 32]).unwrap_or(false) {
                map.pop();
            }
            let map_len = map.len();
            let offsets = Arc::make_mut(&mut s.offsets);
            let old_len = offsets.len();
            if old_len < map_len {
                // Fill new nil slots with logical positions to keep the array
                // non-decreasing for partition_point in chunks_for_range.
                const CHUNK_SIZE: usize = 4 * 1024 * 1024;
                offsets.extend((old_len..map_len).map(|i| (i * CHUNK_SIZE, 0usize)));
            }
            if idx < offsets.len() {
                offsets[idx] = offset_entry;
            }
            offsets.truncate(map_len);
            map_len
        };

        *self.node_id_to_addr.write().unwrap() = node_map;
        self.last_map_refresh_ms.store(now_ms(), Ordering::Relaxed);
        self.known_size.store(file_size as usize, Ordering::Relaxed);
        self.last_window_end.store(map_len as u32, Ordering::Relaxed);
    }

    /// Merge a windowed chunk map response into the engine's full map.
    ///
    /// `from_write_path`: when true, write-path updates always win regardless of seq
    ///   (the client just received a confirmed result from the server — it's authoritative).
    ///   When false (server refresh), use strict `>` so an equal-seq server entry cannot
    ///   revert the engine to an older chunk_id that was already renamed on disk.
    pub fn update_chunk_map_window(
        &self,
        window: Vec<ChunkLocation>,
        from_chunk: u32,
        total_chunks: u32,
        node_map: Arc<HashMap<dfs_common::NodeId, SocketAddr>>,
        file_size: u64,
        from_write_path: bool,
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
                file_id: None,
            });
        }

        const CHUNK_SIZE_U64: u64 = 4 * 1024 * 1024;
        for loc in window.into_iter() {
            // A file_offset:None entry carries no reliable position — it's a stale/
            // orphaned chunk_locations record (see the T48-class chunk_locations-
            // hygiene gap), not a placeholder for "this window's start". Defaulting
            // it to `idx = from` used to silently claim whatever real chunk's slot
            // happened to sit at the window's start index (index 0 for a full
            // from-leader refresh) — clobbering that chunk's correct ChunkLocation
            // with an unrelated one and causing reads of a perfectly valid chunk to
            // fetch the wrong 4MB blob. Skip it: we have no chunk_idx to place it at,
            // and merging it anywhere is strictly worse than leaving that slot alone.
            let Some(offset) = loc.file_offset else { continue };
            let idx = (offset / CHUNK_SIZE_U64) as usize;
            if idx < new_map.len() {
                // Guard: for server-refresh calls, only update if the incoming seq is
                // strictly newer (prevents equal-seq server entries from reverting the
                // engine to an older chunk_id that has already been renamed on disk).
                // For write-path calls, always update — the client just received a
                // confirmed result; it is authoritative regardless of seq equality.
                let should_update = if from_write_path {
                    true
                } else {
                    match (loc.client_write_seq, new_map[idx].client_write_seq) {
                        (Some(inc), Some(ext)) => inc > ext,
                        (Some(_), None)        => true,
                        (None, Some(_))        => false,
                        (None, None)           => true,
                    }
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
        // window_end reflects the engine map's actual coverage [0, new_map.len()), not just
        // the (possibly sparse) entry count in this batch — otherwise needs_refresh() sees
        // current_chunk >= window_end for any chunk beyond the batch size and triggers a
        // redundant background leader refresh on every read into that region.
        let window_end = new_map.len() as u32;
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
        // Binary-search for the first entry whose chunk end is after `offset`.
        // build_offsets assigns nil/gap entries their logical position (idx*CHUNK_SIZE, 0),
        // keeping cs+cz non-decreasing across the whole array so partition_point is valid.
        // The `chunk_size > 0` guard in the scan loop excludes nil entries (size=0) that
        // fall inside the scan window but represent unwritten holes, not real data.
        let start_idx = offsets.partition_point(|&(chunk_start, chunk_size)| chunk_start + chunk_size <= offset);
        let mut result = Vec::new();
        for (idx, &(chunk_start, chunk_size)) in offsets[start_idx..].iter().enumerate() {
            if chunk_start >= end {
                break;
            }
            if chunk_size > 0 && chunk_start + chunk_size > offset {
                result.push((start_idx + idx, chunk_start, chunk_size));
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
        // Sort deterministically by address, then rotate by a hash of chunk_id
        // so the SAME chunk always picks the SAME primary (warm server-side
        // caches, deterministic routing) while DIFFERENT chunks that happen to
        // share the same replica set spread across all of them — avoiding a
        // single node absorbing 100% of read traffic for every chunk on a pair.
        let mut addrs: Vec<SocketAddr> = loc.nodes.iter()
            .filter_map(|nid| nim.get(nid).copied())
            .collect();
        if addrs.is_empty() {
            return None;
        }
        addrs.sort_unstable();
        let idx = (loc.chunk_id.hash[0] as usize) % addrs.len();
        addrs.rotate_left(idx);
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
            let loc = &chunk_map[la_idx];
            // Sparse-hole placeholder (chunk never written) — nothing to prefetch,
            // and no real node holds this all-zero chunk_id.
            if loc.nodes.is_empty() {
                continue;
            }
            let la_cid = loc.chunk_id;
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
    const CHUNK_SIZE: usize = 4 * 1024 * 1024;
    locations.iter().enumerate().map(|(idx, loc)| {
        match loc.file_offset {
            Some(offset) => (offset as usize, loc.size as usize),
            // Nil/gap placeholder: use logical position so the array stays
            // non-decreasing and partition_point in chunks_for_range stays valid.
            // chunk_size=0 ensures these entries are excluded by the scan guard.
            None => (idx * CHUNK_SIZE, 0),
        }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_id_with_hash0(b: u8) -> ChunkId {
        let mut hash = [0u8; 32];
        hash[0] = b;
        ChunkId::from_hash(hash)
    }

    fn loc_with_nodes(chunk_id: ChunkId, nodes: Vec<dfs_common::NodeId>) -> ChunkLocation {
        ChunkLocation {
            chunk_id,
            nodes,
            size: 4 * 1024 * 1024,
            checksum: chunk_id.hash,
            file_offset: Some(0),
            written_at: None,
            client_write_seq: None,
            file_id: None,
        }
    }

    /// Different chunks on the same 2-replica set must spread across both
    /// replicas (not always favor the lower-address node), while the SAME
    /// chunk always picks the SAME primary (cache warmth / determinism).
    #[test]
    fn test_resolve_primary_spreads_across_replicas_by_chunk_id() {
        let node_a = dfs_common::NodeId::new();
        let node_b = dfs_common::NodeId::new();
        let addr_a: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:8901".parse().unwrap();

        let mut nim = HashMap::new();
        nim.insert(node_a, addr_a);
        nim.insert(node_b, addr_b);

        let nodes = vec![node_a, node_b];

        // hash[0] = 0 -> rotate_left(0) -> primary = addr_a (sorted first)
        let loc0 = loc_with_nodes(chunk_id_with_hash0(0), nodes.clone());
        let (p0, f0) = InodeReadEngine::resolve_primary(&loc0, &nim, &[], 0).unwrap();
        assert_eq!(p0, addr_a);
        assert_eq!(f0, vec![addr_b]);

        // hash[0] = 1 -> rotate_left(1) -> primary = addr_b
        let loc1 = loc_with_nodes(chunk_id_with_hash0(1), nodes.clone());
        let (p1, f1) = InodeReadEngine::resolve_primary(&loc1, &nim, &[], 0).unwrap();
        assert_eq!(p1, addr_b);
        assert_eq!(f1, vec![addr_a]);

        // Repeated calls for the same chunk_id are stable.
        let (p0_again, _) = InodeReadEngine::resolve_primary(&loc0, &nim, &[], 0).unwrap();
        assert_eq!(p0, p0_again);
    }

    /// With RF=3, different chunks must spread primary selection across all
    /// 3 replicas (not just 2), and fallbacks always cover the remaining holders.
    #[test]
    fn test_resolve_primary_rf3_uses_all_replicas() {
        let nodes: Vec<dfs_common::NodeId> = (0..3).map(|_| dfs_common::NodeId::new()).collect();
        let addrs: Vec<SocketAddr> = (0..3)
            .map(|i| format!("127.0.0.1:{}", 8900 + i).parse().unwrap())
            .collect();

        let mut nim = HashMap::new();
        for (n, a) in nodes.iter().zip(addrs.iter()) {
            nim.insert(*n, *a);
        }

        let mut sorted_addrs = addrs.clone();
        sorted_addrs.sort_unstable();

        let mut seen = std::collections::HashSet::new();
        for b in 0u8..3 {
            let loc = loc_with_nodes(chunk_id_with_hash0(b), nodes.clone());
            let (primary, fallbacks) = InodeReadEngine::resolve_primary(&loc, &nim, &[], 0).unwrap();
            assert_eq!(sorted_addrs[b as usize % 3], primary);
            assert_eq!(fallbacks.len(), 2);
            seen.insert(primary);
        }
        assert_eq!(seen.len(), 3, "all 3 replicas should be used as primary across different chunks");
    }

    /// chunks_for_range must correctly find real chunks when nil/gap placeholders
    /// sit between them (sparse file with interior holes).  The bug: nil entries
    /// stored as (0, 0) make the predicate non-monotone and partition_point returns
    /// a wrong start_idx that skips all real entries before the gap.
    #[test]
    fn test_chunks_for_range_with_interior_nil_gaps() {
        const MB4: usize = 4 * 1024 * 1024;
        // Mimics T29: chunks 0-2 at offsets 0/4M/8M, gap at 3-4, chunk 5 at 20M.
        // build_offsets maps nil entries (file_offset=None) to (idx*4M, 0).
        let offsets: Vec<(usize, usize)> = vec![
            (0,    MB4),   // chunk 0
            (MB4,  MB4),   // chunk 1
            (2*MB4,MB4),   // chunk 2
            (3*MB4,  0),   // nil gap chunk 3
            (4*MB4,  0),   // nil gap chunk 4
            (5*MB4,MB4),   // chunk 5
        ];

        // Reading at start of chunk 0 must find chunk 0.
        let r = InodeReadEngine::chunks_for_range(&offsets, 0, 4096);
        assert_eq!(r, vec![(0, 0, MB4)], "chunk 0 start");

        // Reading inside chunk 1 must find chunk 1.
        let r = InodeReadEngine::chunks_for_range(&offsets, MB4 + 1024, 4096);
        assert_eq!(r, vec![(1, MB4, MB4)], "inside chunk 1");

        // Reading at start of gap must return empty (zeros for sparse hole).
        let r = InodeReadEngine::chunks_for_range(&offsets, 3*MB4, 4096);
        assert!(r.is_empty(), "gap start should be empty");

        // Reading in middle of gap must also be empty.
        let r = InodeReadEngine::chunks_for_range(&offsets, 4*MB4 - 4096, 4096);
        assert!(r.is_empty(), "gap middle should be empty");

        // Reading at chunk 5 must find it.
        let r = InodeReadEngine::chunks_for_range(&offsets, 5*MB4, 4096);
        assert_eq!(r, vec![(5, 5*MB4, MB4)], "chunk 5");

        // Reading spanning chunk 2 into the gap must only include chunk 2.
        let r = InodeReadEngine::chunks_for_range(&offsets, 3*MB4 - 1024, 4096);
        assert_eq!(r, vec![(2, 2*MB4, MB4)], "spanning chunk2→gap should not include nil");
    }

    /// resolve_primary must never invent a node outside loc.nodes ∩ nim — e.g.
    /// right after a patch (RF temporarily 2) it must restrict to those 2,
    /// even though `nim` may contain a 3rd cluster node that doesn't hold this chunk yet.
    #[test]
    fn test_resolve_primary_restricted_to_chunk_holders() {
        let node_a = dfs_common::NodeId::new();
        let node_b = dfs_common::NodeId::new();
        let node_c_not_a_holder = dfs_common::NodeId::new();
        let addr_a: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:8901".parse().unwrap();
        let addr_c: SocketAddr = "127.0.0.1:8902".parse().unwrap();

        let mut nim = HashMap::new();
        nim.insert(node_a, addr_a);
        nim.insert(node_b, addr_b);
        nim.insert(node_c_not_a_holder, addr_c);

        let loc = loc_with_nodes(chunk_id_with_hash0(0), vec![node_a, node_b]);
        for b in 0u8..=255 {
            let loc_b = ChunkLocation { chunk_id: chunk_id_with_hash0(b), ..loc.clone() };
            let (primary, fallbacks) = InodeReadEngine::resolve_primary(&loc_b, &nim, &[], 0).unwrap();
            assert!(primary == addr_a || primary == addr_b);
            assert_ne!(primary, addr_c);
            assert!(!fallbacks.contains(&addr_c));
        }
    }

    /// Real bug reproduced live via test_dvr_stream.sh: a stale `file_offset: None`
    /// chunk_locations entry (leftover from the T48-class chunk_locations-hygiene
    /// gap — confirmed via dfs-admin showing 10 chunk_locations for an 8-chunk
    /// file) causes update_chunk_map_window's None-offset fallback
    /// (`idx = from_chunk`, i.e. 0 for a full from-leader refresh) to collide with
    /// chunk 0's own real slot. Whichever entry processes last in the window Vec
    /// wins the should_update race, so chunk 0's real, correct ChunkLocation can be
    /// silently clobbered by the stray entry — even though the stray entry's own
    /// bytes are perfectly valid data for a DIFFERENT (unrelated) chunk. A read for
    /// chunk 0 then fetches the wrong 4MB blob instead of erroring, which is a much
    /// worse failure mode than the sparse-hole case this module already guards.
    #[test]
    fn test_stale_none_offset_entry_does_not_clobber_real_chunk_zero() {
        const MB4: usize = 4 * 1024 * 1024;
        let engine = InodeReadEngine::new(1);

        let real_chunk0 = loc_with_nodes(chunk_id_with_hash0(1), vec![dfs_common::NodeId::new()]);
        let real_chunk0 = ChunkLocation { file_offset: Some(0), size: MB4, ..real_chunk0 };
        let real_chunk1 = ChunkLocation {
            file_offset: Some(MB4 as u64),
            ..loc_with_nodes(chunk_id_with_hash0(2), vec![dfs_common::NodeId::new()])
        };
        // The stray entry: a real, valid chunk elsewhere in the cluster, but with no
        // file_offset recorded against THIS file — exactly what dfs-admin showed
        // ("Offset: ?") for the two extra entries beyond the file's real 8 chunks.
        let stray = ChunkLocation {
            file_offset: None,
            ..loc_with_nodes(chunk_id_with_hash0(99), vec![dfs_common::NodeId::new()])
        };

        // Window order matches what a real leader response looks like: real chunks
        // in order, stray/garbage entries trailing at the end of the Vec.
        let window = vec![real_chunk0.clone(), real_chunk1.clone(), stray.clone()];
        engine.update_chunk_map_window(window, 0, 3, Arc::new(HashMap::new()), (2 * MB4) as u64, false);

        let (map, offsets, _) = engine.snapshot();
        assert_eq!(map[0].chunk_id, real_chunk0.chunk_id,
            "chunk 0's real entry must survive — a stray file_offset:None record must never \
             overwrite a real chunk's slot in the map");

        let r = InodeReadEngine::chunks_for_range(&offsets, 0, 4096);
        assert_eq!(r.len(), 1, "reading chunk 0 must resolve to exactly one entry");
        assert_eq!(map[r[0].0].chunk_id, real_chunk0.chunk_id,
            "reading offset 0 must fetch chunk 0's real chunk_id, not the stray entry's");
    }
}
