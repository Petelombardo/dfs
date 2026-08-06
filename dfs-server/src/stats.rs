use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

const RING_LEN: usize = 3600; // 1-hour history at 1-second resolution

struct RingBuffer {
    reads: [u64; RING_LEN],
    writes: [u64; RING_LEN],
    meta: [u64; RING_LEN],
    head: usize,   // next slot to write
    filled: usize, // valid slots filled so far (0..=RING_LEN)
}

impl RingBuffer {
    fn new() -> Self {
        Self {
            reads: [0u64; RING_LEN],
            writes: [0u64; RING_LEN],
            meta: [0u64; RING_LEN],
            head: 0,
            filled: 0,
        }
    }
}

pub struct OpsTracker {
    // Current-second accumulation buckets (swapped to ring every tick).
    reads: AtomicU64,
    writes: AtomicU64,
    meta: AtomicU64,
    started_at: Instant,
    ring: Mutex<RingBuffer>,
}

pub struct NodeStatsSnapshot {
    pub reads_live: u64,
    pub writes_live: u64,
    pub meta_live: u64,
    pub reads_peak_1h: u64,
    pub writes_peak_1h: u64,
    pub meta_peak_1h: u64,
    pub total_peak_1h: u64,
    pub reads_avg_1h: u64,
    pub writes_avg_1h: u64,
    pub meta_avg_1h: u64,
    pub uptime_secs: u64,
    pub active_connections: u64,
    pub max_connections: u64,
}

impl OpsTracker {
    pub fn new() -> Self {
        Self {
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            meta: AtomicU64::new(0),
            started_at: Instant::now(),
            ring: Mutex::new(RingBuffer::new()),
        }
    }

    #[inline]
    pub fn inc_read(&self) {
        self.reads.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_write(&self) {
        self.writes.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_meta(&self) {
        self.meta.fetch_add(1, Ordering::Relaxed);
    }

    /// Called every second by the background tick task.
    /// Snapshots the current-second atomics into the ring buffer.
    pub fn tick(&self) {
        let r = self.reads.swap(0, Ordering::Relaxed);
        let w = self.writes.swap(0, Ordering::Relaxed);
        let m = self.meta.swap(0, Ordering::Relaxed);
        let mut ring = self.ring.lock().unwrap();
        let head = ring.head;
        ring.reads[head] = r;
        ring.writes[head] = w;
        ring.meta[head] = m;
        ring.head = (head + 1) % RING_LEN;
        if ring.filled < RING_LEN {
            ring.filled += 1;
        }
    }

    pub fn get_stats(&self) -> NodeStatsSnapshot {
        let ring = self.ring.lock().unwrap();
        let filled = ring.filled;

        if filled == 0 {
            // No completed second yet — report in-flight atomics as approximation.
            return NodeStatsSnapshot {
                reads_live: self.reads.load(Ordering::Relaxed),
                writes_live: self.writes.load(Ordering::Relaxed),
                meta_live: self.meta.load(Ordering::Relaxed),
                reads_peak_1h: 0,
                writes_peak_1h: 0,
                meta_peak_1h: 0,
                total_peak_1h: 0,
                reads_avg_1h: 0,
                writes_avg_1h: 0,
                meta_avg_1h: 0,
                uptime_secs: self.started_at.elapsed().as_secs(),
                active_connections: 0,
                max_connections: 0,
            };
        }

        // "Live" = most recently completed 1-second window.
        let live_idx = (ring.head + RING_LEN - 1) % RING_LEN;
        let r_live = ring.reads[live_idx];
        let w_live = ring.writes[live_idx];
        let m_live = ring.meta[live_idx];

        let mut r_sum = 0u64;
        let mut w_sum = 0u64;
        let mut m_sum = 0u64;
        let mut r_peak = 0u64;
        let mut w_peak = 0u64;
        let mut m_peak = 0u64;
        let mut total_peak = 0u64;

        for i in 0..filled {
            // Walk backwards from most-recent to oldest.
            // (head + RING_LEN - 1 - i) % RING_LEN is always non-negative because
            // i < filled <= RING_LEN, so the subtrahend is at most RING_LEN - 1.
            let idx = (ring.head + RING_LEN - 1 - i) % RING_LEN;
            let r = ring.reads[idx];
            let w = ring.writes[idx];
            let m = ring.meta[idx];
            r_sum += r;
            w_sum += w;
            m_sum += m;
            r_peak = r_peak.max(r);
            w_peak = w_peak.max(w);
            m_peak = m_peak.max(m);
            total_peak = total_peak.max(r + w + m);
        }

        let count = filled as u64;
        NodeStatsSnapshot {
            reads_live: r_live,
            writes_live: w_live,
            meta_live: m_live,
            reads_peak_1h: r_peak,
            writes_peak_1h: w_peak,
            meta_peak_1h: m_peak,
            total_peak_1h: total_peak,
            reads_avg_1h: r_sum / count,
            writes_avg_1h: w_sum / count,
            meta_avg_1h: m_sum / count,
            uptime_secs: self.started_at.elapsed().as_secs(),
            // Filled in by Server::handle_request(GetNodeStats) which has access to the semaphore.
            active_connections: 0,
            max_connections: 0,
        }
    }
}

/// Which bucket an RPC (Request or ClusterMessage) belongs to, for
/// RpcClassCounts. See classify_request/classify_cluster_message in
/// server.rs for the actual variant -> class mapping — this type is
/// deliberately just the bucket set, not tied to the wire enums, so
/// stats.rs doesn't need to depend on dfs_common::protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcClass {
    PeerHealing,
    PeerDeleteOps,
    PeerFold,
    PeerGossip,
    PeerOther,
    ClientFullPatch,
    ClientMultiPatch,
    ClientFold,
    ClientOther,
    Admin,
}

/// Cumulative-since-startup RPC counts by class. Added 2026-08-06 after a
/// real overnight soak raised the operational question "were most of our
/// RPCs peer or client, and within that, what kind" with no way to answer it
/// — OpsTracker above only tracks coarse read/write/meta, missing healing/
/// fold/delete/admin entirely. Deliberately simple (plain AtomicU64 counters,
/// no ring buffer/rate tracking like OpsTracker) since the ask was an
/// operational proportion ("what fraction"), not a rate — rough and in-memory
/// is explicitly fine, this is not a durability-critical metric.
pub struct RpcClassCounts {
    peer_healing: AtomicU64,
    peer_delete_ops: AtomicU64,
    peer_fold: AtomicU64,
    peer_gossip: AtomicU64,
    peer_other: AtomicU64,
    client_full_patch: AtomicU64,
    client_multi_patch: AtomicU64,
    client_fold: AtomicU64,
    client_other: AtomicU64,
    admin: AtomicU64,
}

/// Plain-u64 snapshot of RpcClassCounts, for building Response::RpcClassCounts.
#[derive(Debug, Clone, Copy, Default)]
pub struct RpcClassSnapshot {
    pub peer_healing: u64,
    pub peer_delete_ops: u64,
    pub peer_fold: u64,
    pub peer_gossip: u64,
    pub peer_other: u64,
    pub client_full_patch: u64,
    pub client_multi_patch: u64,
    pub client_fold: u64,
    pub client_other: u64,
    pub admin: u64,
}

impl RpcClassCounts {
    pub fn new() -> Self {
        Self {
            peer_healing: AtomicU64::new(0),
            peer_delete_ops: AtomicU64::new(0),
            peer_fold: AtomicU64::new(0),
            peer_gossip: AtomicU64::new(0),
            peer_other: AtomicU64::new(0),
            client_full_patch: AtomicU64::new(0),
            client_multi_patch: AtomicU64::new(0),
            client_fold: AtomicU64::new(0),
            client_other: AtomicU64::new(0),
            admin: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn record(&self, class: RpcClass) {
        let counter = match class {
            RpcClass::PeerHealing => &self.peer_healing,
            RpcClass::PeerDeleteOps => &self.peer_delete_ops,
            RpcClass::PeerFold => &self.peer_fold,
            RpcClass::PeerGossip => &self.peer_gossip,
            RpcClass::PeerOther => &self.peer_other,
            RpcClass::ClientFullPatch => &self.client_full_patch,
            RpcClass::ClientMultiPatch => &self.client_multi_patch,
            RpcClass::ClientFold => &self.client_fold,
            RpcClass::ClientOther => &self.client_other,
            RpcClass::Admin => &self.admin,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RpcClassSnapshot {
        RpcClassSnapshot {
            peer_healing: self.peer_healing.load(Ordering::Relaxed),
            peer_delete_ops: self.peer_delete_ops.load(Ordering::Relaxed),
            peer_fold: self.peer_fold.load(Ordering::Relaxed),
            peer_gossip: self.peer_gossip.load(Ordering::Relaxed),
            peer_other: self.peer_other.load(Ordering::Relaxed),
            client_full_patch: self.client_full_patch.load(Ordering::Relaxed),
            client_multi_patch: self.client_multi_patch.load(Ordering::Relaxed),
            client_fold: self.client_fold.load(Ordering::Relaxed),
            client_other: self.client_other.load(Ordering::Relaxed),
            admin: self.admin.load(Ordering::Relaxed),
        }
    }
}
