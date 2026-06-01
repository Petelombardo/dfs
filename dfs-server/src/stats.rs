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
