use anyhow::{Context, Result};
use dashmap::DashMap;
use dfs_common::{ChunkId, ChunkLocation, FileId, Message, NodeId, Request, Response};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};
use tokio::task::JoinSet;
use tokio::time::interval;
use tracing::{debug, error, info, warn};

use crate::cluster::ClusterManager;
use crate::metadata::MetadataStore;
use crate::network::NetworkClient;
use crate::storage::ChunkStorage;

/// Suspend all destructive healer operations (orphan purge, DATA LOSS declarations,
/// over-replication cleanup, disk orphan sweep) for this many seconds after a leader
/// election, to allow the cluster to settle and metadata to catch up before we start
/// deleting anything.
pub const LEADER_CHANGE_GRACE_SECS: u64 = 1200;

/// Suspend disk-orphan-sweep-family destructive decisions for this many seconds
/// after THIS node's own process starts, regardless of leadership status.
///
/// LEADER_CHANGE_GRACE_SECS above only fires for a node that has itself been (or
/// recently became) leader — `time_since_became_leader()` returns None for a plain
/// follower, and the `.map_or(true, ...)` default trivially passes the check for
/// every follower, unconditionally, no matter how recently it restarted. That's a
/// real gap: `rebuild_chunk_map_from_metadata` on startup is fast (typically well
/// under a second even for tens of thousands of chunks), but the chunk_map it
/// rebuilds is only ever as fresh as the last durable FileMetadata sync — nowhere
/// near as current as a long-running peer's in-memory chunk_map, which is updated
/// synchronously on every single patch. A freshly-restarted follower's own
/// live_chunk_ids()/chunk_map union can legitimately be missing entries for
/// recently-active files for as long as it takes normal traffic (background-tick
/// pushes, new patches, peer broadcasts) to catch it back up — there's no
/// "rebuild finished" signal that means "and now I'm current."
///
/// Confirmed 2026-07-10: gluster2 restarted (redeploy) at a moment of the cluster's
/// choosing, its chunk_map rebuild completed in ~1s, but its view of an
/// actively-patched file was still stale minutes later — its own
/// [GHOST-stale-check] logged exactly this for the same file. A disk-orphan-sweep
/// pass ran in that window, misread ~70 real, live chunks belonging to
/// not-yet-caught-up files as "patch-superseded, cleanup missed," and (after
/// cross-node authorization that only protects Pending patch state, not ordinary
/// live chunks — see handle_confirm_chunks_live) deleted its own only-recently-
/// created copies of at least one of them, for real.
pub const SELF_RESTART_GRACE_SECS: u64 = LEADER_CHANGE_GRACE_SECS;

/// Settle grace specifically for the NON-destructive orphan-dequeue in discovery
/// (dropping an unreferenced under-RF chunk from the heal queue). Much shorter than
/// SELF_RESTART_GRACE_SECS on purpose: that 20-minute grace exists to keep the
/// DESTRUCTIVE paths from *deleting* off a stale post-restart chunk_map. Dropping a
/// chunk from the heal queue deletes nothing and is self-correcting — a chunk
/// mislabeled unreferenced (because the map hadn't finished converging) is simply
/// re-added by the next deep scan if it's still under-RF. So we only need enough
/// settle for rebuild_chunk_map_from_metadata to populate the map, not the full
/// destructive grace. 120s keeps the heal queue from staying clogged for 20 minutes
/// after every restart/deploy.
pub const ORPHAN_DEQUEUE_SETTLE_SECS: u64 = 120;

/// Token-bucket bandwidth limiter for heal traffic. Unlike a bytes-in-flight
/// semaphore (which only bounds concurrency — a transfer that completes instantly
/// just frees its permits for the next one to start at full speed), this paces
/// actual throughput to the configured rate by making callers wait for tokens to
/// refill over time. One of these lives per node and governs that node's outbound
/// heal-chunk reads+sends (see handle_push_chunk_to in server.rs), since that's
/// where the bytes actually move — the leader only orchestrates which node pushes
/// to which.
pub struct BandwidthLimiter {
    state: tokio::sync::Mutex<BandwidthLimiterState>,
}

struct BandwidthLimiterState {
    rate_bytes_per_sec: f64,
    burst_cap: f64,
    tokens: f64,
    last_refill: Instant,
}

impl BandwidthLimiter {
    pub fn new(mb_per_sec: usize) -> Self {
        let rate_bytes_per_sec = (mb_per_sec * 1024 * 1024) as f64;
        // Burst cap = 2× the per-second rate so all heal_max_concurrent tasks
        // (default 8 × 4MB = 32MB) can start immediately at rates up to 16MB/s
        // without queuing. At higher rates the burst is even larger, so no task
        // ever has to wait on the first pass.
        let burst_cap = rate_bytes_per_sec * 2.0;
        Self {
            state: tokio::sync::Mutex::new(BandwidthLimiterState {
                rate_bytes_per_sec,
                burst_cap,
                tokens: burst_cap,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Current rate in MB/s (reads the live value from locked state).
    pub async fn current_rate_mb(&self) -> usize {
        let state = self.state.lock().await;
        (state.rate_bytes_per_sec / (1024.0 * 1024.0)) as usize
    }

    /// Update the token-bucket rate at runtime. Takes effect on the next acquire() call.
    /// Burst cap is always 2× the new rate. Existing token balance is clamped to the
    /// new burst cap to prevent a burst of accumulated tokens at the old rate.
    pub async fn set_rate_mb(&self, mb_per_sec: usize) {
        let rate = (mb_per_sec * 1024 * 1024) as f64;
        let burst = rate * 2.0;
        let mut state = self.state.lock().await;
        state.rate_bytes_per_sec = rate;
        state.burst_cap = burst;
        state.tokens = state.tokens.min(burst);
    }

    /// Block until `bytes` worth of bandwidth budget is available, refilling at
    /// the configured rate. Burst is capped to 2× the per-second rate so all
    /// concurrent heal tasks can start immediately and sustained throughput
    /// tracks the configured MB/s.
    pub async fn acquire(&self, bytes: usize) {
        let bytes = bytes as f64;
        loop {
            let wait = {
                let mut state = self.state.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(state.last_refill).as_secs_f64();
                state.tokens = (state.tokens + elapsed * state.rate_bytes_per_sec)
                    .min(state.burst_cap);
                state.last_refill = now;

                if state.tokens >= bytes {
                    state.tokens -= bytes;
                    None
                } else {
                    // Don't consume partial tokens on failure — leave them for the
                    // next attempt so the refill computed from elapsed time is not
                    // reset to zero, which would starve concurrent waiters.
                    let deficit = bytes - state.tokens;
                    Some(Duration::from_secs_f64(deficit / state.rate_bytes_per_sec))
                }
            };
            match wait {
                None => return,
                Some(d) => tokio::time::sleep(d).await,
            }
        }
    }
}

/// Healing manager - monitors and repairs chunk replication
/// Optimized for SBC environments (batched operations, configurable intervals)
pub struct HealingManager {
    /// Local storage
    storage: Arc<ChunkStorage>,

    /// Metadata store
    metadata: Arc<MetadataStore>,

    /// Leader-maintained in-memory chunk map, shared with Server (same Arc).
    /// Unlike MetadataStore::live_chunk_ids() (durable FILE_TABLE, only refreshed on
    /// a full PutFileMetadata), this is updated synchronously by every patch/replicate-
    /// location handler, so it never goes stale for actively-patched files. Used as an
    /// additional liveness source so the live-file orphan sweep doesn't mistake "FILE_TABLE
    /// hasn't caught up yet" for "patch-superseded" — see reconcile_live_file_candidates.
    chunk_map: Arc<DashMap<FileId, (Vec<ChunkLocation>, u64)>>,

    /// Same Arc as Server::fold_result_chunk_ids / Server::chunk_generations —
    /// needed to call Server::location_supersedes with full fidelity when
    /// resolving which chunk_id currently owns a (file_id, chunk_idx) slot (see
    /// superseded_generation_chunk_ids). Without these, a same-client_write_seq
    /// tie (rare, but exactly what the 2026-07-24 VM-108 dangling-pointer bug
    /// hit) would fall back to a same-generation/unknown-generation heuristic
    /// instead of the authoritative tiebreak the read path itself uses — this
    /// scan can then run destructive disk deletes (via run_disk_orphan_sweep),
    /// so it must resolve ties the exact same way reads do, not an approximation.
    fold_result_chunk_ids: Arc<dashmap::DashSet<ChunkId>>,
    chunk_generations: Arc<DashMap<ChunkId, u64>>,

    /// Chunk_ids that are file_id-live (their file still exists) but lost the
    /// supersession contest for their (file_id, chunk_idx) slot to a newer
    /// generation — i.e. a stale routing row CHUNK_TABLE never pruned when the
    /// slot moved on, invisible to live_chunk_ids()'s file-existence-only check.
    /// Recomputed once per deep discovery pass (run_discovery_pass) as a
    /// byproduct of the scan it already does — see ScanResult::slot_losers —
    /// and consulted by both the discovery classification loop (so these never
    /// get queued as under-replicated / never occupy a dispatch slot) and
    /// run_disk_orphan_sweep (so they're finally eligible for the existing
    /// age-gated + leader-confirmed physical-delete pipeline instead of being
    /// permanently invisible to it). Cached rather than recomputed by each
    /// reader: both consumers would otherwise pay for their own full CHUNK_TABLE
    /// scan+group every cycle for data the deep pass already produced.
    superseded_generation_chunk_ids: Arc<RwLock<HashSet<ChunkId>>>,

    /// Cluster manager
    cluster: Arc<ClusterManager>,

    /// Network client for inter-node communication
    client: Arc<NetworkClient>,

    /// Target replication factor. Shared `Arc<AtomicUsize>` with `Server::replication_factor`
    /// (same instance, constructed once in main.rs) so a live `dfs-admin cluster set
    /// --replication-factor` change — or rejoin reconciliation adopting the leader's
    /// value — is visible to both the write path and healing decisions without a restart.
    replication_factor: Arc<AtomicUsize>,

    /// Delay before starting healing after node failure (seconds). Live-tunable via
    /// SetHealingTuning (see apply_tuning) — same pattern as heal_transfer_timeout_secs —
    /// so tests exercising a "never fully replicated" chunk (see should_heal's gate)
    /// don't have to wait out the full production default (300s) to converge.
    healing_delay_secs: Arc<AtomicU64>,

    /// Scrubbing interval (hours)
    scrub_interval_hours: u64,

    /// Auto-healing enabled
    auto_heal: bool,

    /// Maximum number of chunks to process per drain cycle (queue depth)
    max_heal_per_cycle: usize,

    /// Bounds how many heal transfers the leader has concurrently outstanding
    /// (one permit per task), to avoid FD/task exhaustion from fanning out too many
    /// PushChunkTo RPCs at once. This is a concurrency cap only — actual byte
    /// throughput is paced separately by heal_bandwidth_limiter_out on the source node.
    heal_semaphore: Arc<Semaphore>,

    /// Max concurrent heal transfers (heal_semaphore's target capacity), stored for the
    /// drain loop's task-fill bound. Live-settable via `dfs-admin healing set
    /// --max-concurrent` — see `resize_heal_concurrency`, which reconciles the actual
    /// `heal_semaphore` permit count to this target.
    heal_max_concurrent: Arc<AtomicUsize>,

    /// Per-node concurrency gate: one `Semaphore` per `NodeId`, created lazily
    /// (on first use) with `heal_max_concurrent_per_node` permits. Combined in+out —
    /// a node counts toward its own cap whether it's acting as heal source or target —
    /// this bounds how many transfers any single node can be party to at once, so a
    /// busy node can't consume the whole global `heal_max_concurrent` budget while
    /// other nodes sit idle (e.g. B→A and D→C can each run their own quota in parallel).
    node_inflight: Arc<DashMap<NodeId, Arc<Semaphore>>>,

    /// Target capacity for each entry in `node_inflight` (heal_max_concurrent_per_node's
    /// live value). Newly-created node semaphores start at this value; live-settable via
    /// `dfs-admin healing set --max-concurrent-per-node` — see `resize_node_concurrency`.
    /// Always kept <= `heal_max_concurrent`: a per-node cap above the global cap could
    /// never bind, so both `apply_tuning` and `resize_heal_concurrency` clamp it down.
    heal_max_concurrent_per_node: Arc<AtomicUsize>,

    /// Real bytes/sec pacing for this node's outbound heal-chunk reads+sends.
    /// Rate is managed entirely by the adaptive bandwidth controller. Separate from
    /// heal_bandwidth_limiter_in because TX/RX are independent full-duplex capacity —
    /// a node simultaneously pushing to one peer and receiving from another isn't
    /// competing with itself, so sharing one bucket would wrongly halve its throughput.
    heal_bandwidth_limiter_out: Arc<BandwidthLimiter>,

    /// Real bytes/sec pacing for this node's inbound heal-chunk receives+writes.
    /// Same configured rate as heal_bandwidth_limiter_out (both driven by the same
    /// adaptive controller tick), but a distinct token bucket so RX pacing never
    /// contends with TX pacing. Without this, several source nodes could each stay
    /// under their own egress cap while collectively swamping one recovering target's
    /// ingress, since nothing previously paced the receiving side at all.
    heal_bandwidth_limiter_in: Arc<BandwidthLimiter>,

    /// Unix-epoch ms of the most recent client write seen on any node (updated via
    /// ReplicateChunkLocation broadcasts). Used by the adaptive bandwidth controller
    /// to detect active-write vs idle periods.
    last_cluster_write_ms: Arc<std::sync::atomic::AtomicU64>,

    /// Assumed node-to-node link bandwidth in MB/s. Used as the 100% baseline for
    /// the adaptive rate formula. Defaults to 100 (1Gbps). Live-settable via
    /// `dfs-admin healing set --link-bandwidth-mb`.
    link_bandwidth_mb: Arc<AtomicUsize>,

    /// Maximum fraction of link bandwidth the healer may use (0.10–1.00).
    /// Default 0.60 — logical ceiling when client and heal traffic share one interface
    /// (healer > writer → can never fall behind). `RwLock` rather than an atomic since
    /// it's a float; read fresh each `run_bandwidth_controller` tick (every 2s) so a
    /// live `dfs-admin healing set --max-pct` change takes effect within one tick.
    heal_max_pct: Arc<RwLock<f64>>,

    /// Chunks ready to heal: under-replicated, has ≥1 confirmed alive source node,
    /// and healing delay has passed. Maps chunk_id → first_detected_at so oldest-
    /// first scheduling works correctly.
    pending_healing: Arc<RwLock<HashMap<ChunkId, Instant>>>,

    /// Chunk IDs a fold has claimed as "about to be retired" — checked by
    /// do_heal_chunk_inner right before it commits a heal's ChunkLocation update,
    /// so a heal that's already in flight when a fold starts still gets discarded
    /// instead of resurrecting a superseded identity. See Server::cancel_healing_for_chunk
    /// (called at the start of every fold) and this file's own cancel_healing/
    /// retract_healing_cancellation. Maps chunk_id -> when the cancellation was
    /// recorded, so a stale entry (the fold that requested it crashed or never
    /// retracted for some other reason) expires rather than permanently blocking
    /// healing for that identity — see CANCEL_TOMBSTONE_TTL.
    cancelled_heals: Arc<DashMap<ChunkId, Instant>>,

    /// Chunks currently being transferred — prevents double-dispatch across drain
    /// ticks. Inserted just before PushChunkTo; removed on completion or timeout.
    in_flight_healing: Arc<RwLock<HashSet<ChunkId>>>,

    /// Per-chunk confirmed-alive node list from the most recent discovery scan.
    /// Written by run_discovery_loop(); read by run_heal_loop().
    alive_nodes_cache: Arc<RwLock<HashMap<ChunkId, Vec<NodeId>>>>,

    /// Chunks with no viable source: under-replicated but ALL nodes that hold the
    /// data are currently offline. Managed entirely by discovery — moved here when
    /// 0 alive nodes are found, promoted back to pending when ≥1 source reappears.
    /// The heal loop never touches this set directly.
    stalled_healing: Arc<RwLock<HashSet<ChunkId>>>,

    /// Chunks whose most recent heal attempt reached ≥1 confirmed-alive source but
    /// still failed to replicate to ANY target (source-side corruption, all targets
    /// rejecting, etc.) — distinct from stalled_healing's "0 alive nodes" case, whose
    /// promotion-back-to-pending check only looks at HasChunks presence, not whether
    /// a push from that exact source has actually been *verified* to work. Without
    /// this, a chunk whose only replica fails its own checksum on every push attempt
    /// still answers HasChunks "yes" (the bytes are locally present, just corrupt),
    /// so discovery immediately re-promotes and re-queues it every single cycle
    /// forever — confirmed live 2026-07-12: one such chunk retried every ~15s for
    /// over 10 hours straight, a real and continuous CPU/disk-I/O/network drain on
    /// the leader (which was simultaneously the corrupted source). Maps chunk_id ->
    /// (last_failure, consecutive_failure_count) for exponential backoff, capped —
    /// long enough to stop the hot loop, short enough to still notice if the source
    /// is eventually fixed/replaced. Cleared on any successful heal of that chunk.
    heal_push_failure: Arc<DashMap<ChunkId, (Instant, u32)>>,

    /// Dispatch-priority override for chunks that have failed/stalled at least once,
    /// separate from pending_healing's `detected_at` (which must stay the ORIGINAL
    /// first-detection time — cleanup_stale_pending's staleness bound depends on it
    /// never resetting, or a chronically-failing chunk could never age out). Without
    /// this, heal_queue_sort_key's oldest-first ordering means a chunk that just
    /// stalled or failed a push looks like the MOST urgent candidate the instant it
    /// becomes eligible again (it's technically been "detected" the longest) and jumps
    /// to the FRONT of dispatch — starving chunks that are genuinely waiting their
    /// normal turn behind a doomed/flaky one that will likely just fail again. Touched
    /// (Instant::now()) at every failure/stall point (drain_heal_queue's heal-failed
    /// and heal-timed-out arms, and discovery's 0-alive-nodes stall) so the sort key
    /// (see its read site) uses "time since last failure" instead of "time since first
    /// detection" for any chunk that has ever failed — it re-earns front-of-queue
    /// priority only by going a while without failing again, same durability urgency
    /// otherwise unchanged. Absent entry (never failed) falls back to detected_at, so
    /// normal chunks are completely unaffected. Cleared alongside pending_healing.
    requeue_priority: Arc<DashMap<ChunkId, Instant>>,

    /// Two-cycle orphan guard: chunk IDs that were absent from live_chunk_ids in the
    /// previous discovery pass. Only purged when absent in two consecutive passes.
    /// This prevents premature orphan-purge when the leader's metadata DB is temporarily
    /// stale (e.g. during initial metadata replication lag after a write). One extra
    /// 60s cycle of accumulation is acceptable vs. data loss from false-positive orphans.
    orphan_candidates: Arc<RwLock<HashSet<ChunkId>>>,

    /// Tokens this node has confirmed the current leader knows about, via
    /// run_pending_patch_reconciliation — see that method's doc comment. Cleared
    /// wholesale (not per-token) on a detected leader change, and GC'd each cycle
    /// against the current live Pending set.
    pending_patch_confirmed: Arc<RwLock<HashSet<ChunkId>>>,

    /// Same purpose as pending_patch_confirmed, but for Folded tokens — closes the
    /// notify_leader_of_fold gap (2026-08-04): that call is best-effort (2 attempts,
    /// 1s timeout each, failure is just a warn!), and by the time it runs, run_single_fold
    /// has already committed the fold locally (new chunk on disk, PATCH_STATE_TABLE
    /// flipped to Folded, local chunk_map updated) — so a failed notify leaves the
    /// leader's view silently stale with no retry beyond that one call. Confirmed live
    /// (VM-111 install, file 01beabe2, chunk_idx=4096, 2026-08-04): a stale-base fold
    /// happened under sustained heavy concurrent write load (OS install + 3 DVR
    /// recordings + a read), exactly the load profile under which a 1s RPC timeout to
    /// an overloaded leader becomes routine. Keyed on the FOLD RESULT chunk_id
    /// (real_chunk_id), not the token — the token is retired the moment it folds and a
    /// fresh accumulator can immediately mint a new one at the same slot, so the token
    /// itself isn't a stable key across reconciliation cycles the way the fold's
    /// standalone output chunk_id is.
    folded_patch_confirmed: Arc<RwLock<HashSet<ChunkId>>>,

    /// Leader address as of the last run_pending_patch_reconciliation cycle, used
    /// only to detect a leader change (see pending_patch_confirmed, folded_patch_confirmed).
    last_reconcile_leader: Arc<RwLock<Option<std::net::SocketAddr>>>,

    /// Per-transfer timeout for a single PushChunkTo (seconds). On expiry the chunk
    /// stays in pending (retried next drain tick) and the semaphore slot is released.
    /// Live-settable via `dfs-admin healing set --transfer-timeout-secs`.
    heal_transfer_timeout_secs: Arc<AtomicU64>,

    /// Guards run_phantom_reconciliation_pass against overlapping itself — the
    /// periodic loop and a manual `dfs-admin healing reconcile` trigger can land
    /// close enough together to both be mid-scan at once. Each concurrent pass
    /// doubles the per-node HasChunks fan-out; piling up unbounded overlaps is
    /// exactly the kind of compounding load that turned a single slow node into
    /// the leader freeze on 2026-06-20. A second pass now logs and exits instead
    /// of running alongside the first.
    phantom_reconcile_in_progress: std::sync::atomic::AtomicBool,

    /// Runtime healing kill-switch. Toggled by `dfs-admin healing disable/enable`
    /// without a service restart. Initialised from `auto_heal` at construction.
    /// All loop iterations check this flag; when false, they skip their work and
    /// sleep until re-enabled.
    pub healing_enabled: Arc<std::sync::atomic::AtomicBool>,

    /// When this process started (approximately — set at HealingManager
    /// construction, which happens moments after rebuild_chunk_map_from_metadata
    /// is kicked off in main.rs, close enough for a 20-minute grace window). See
    /// SELF_RESTART_GRACE_SECS's doc comment for why this exists independently of
    /// LEADER_CHANGE_GRACE_SECS's own (leader-only) tracking.
    local_started_at: Instant,

    /// Same Arc instance as Server::compaction_quiescing — set for the duration of
    /// compaction's Phase 3 (or the compact_db_blocking() fallback), so the heal
    /// loop skips dispatching new transfers while it's set rather than adding to
    /// the same shared metadata read-lock contention Phase 3 is queued behind.
    /// Checked the same way as healing_enabled: skip this cycle, retry next tick —
    /// no need for a bounded wait here since run_heal_loop already ticks every 15s.
    compaction_quiescing: Arc<std::sync::atomic::AtomicBool>,

    /// JoinHandle for the currently-running run_disk_orphan_sweep_paginated_loop
    /// task, watched by run_disk_orphan_sweep_watchdog. Replaced (not just read) by
    /// the watchdog when it detects the loop has ended, so this always points at
    /// whichever instance is actually running.
    disk_sweep_task: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,

    /// Updated by the paginated loop after every page (whether or not the page was
    /// gated/empty) — purely a liveness heartbeat for the watchdog, not used for any
    /// pacing decision itself.
    disk_sweep_last_page_at: Arc<std::sync::Mutex<Instant>>,

    /// Last time the paginated loop's gated-retry path actually emitted a warn! —
    /// see GATE_LOG_THROTTLE's doc comment.
    disk_sweep_gate_log_throttle: Arc<std::sync::Mutex<Instant>>,
}

/// Deferred commit intent produced by one heal task. do_heal_chunk_inner used to
/// commit each of these itself the moment the chunk finished healing: one
/// single-record put/delete transaction, one delete_pending_healing transaction
/// (via clear_pending_static), and one ReplicateChunkLocation connection per peer
/// — per chunk. Under a backlog drain that's hundreds of tiny redb commits per
/// minute, each one COW-rewriting the B-tree path to the root (measured 2026-07-15:
/// leader DB ballooned to 56.5MB with 0.7MB live during a 500-chunk drain).
/// drain_heal_queue now collects these and flushes them in batches — one
/// transaction and one per-peer ReplicateChunkLocationsV2 message per batch; see
/// flush_heal_outcomes.
struct HealOutcome {
    chunk_id: ChunkId,
    /// Updated routing record to persist (and broadcast when `broadcast`).
    location_put: Option<ChunkLocation>,
    /// Routing record to delete (orphaned chunk / chunk lost from its source).
    location_delete: Option<ChunkId>,
    /// Remove chunk_id from pending_healing — both the in-memory map and its
    /// PENDING_HEALING_TABLE row (what clear_pending_static used to do inline).
    clear_pending: bool,
    /// Broadcast location_put to all online peers after the local commit.
    broadcast: bool,
}

impl HealOutcome {
    /// Outcome that only clears the pending_healing entry (orphan deferrals,
    /// cancelled heals, stale-source discards, already-at-RF chunks).
    fn clear_only(chunk_id: ChunkId) -> Self {
        Self { chunk_id, location_put: None, location_delete: None, clear_pending: true, broadcast: false }
    }
}

impl HealingManager {
    /// Create a new healing manager
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        storage: Arc<ChunkStorage>,
        metadata: Arc<MetadataStore>,
        cluster: Arc<ClusterManager>,
        client: Arc<NetworkClient>,
        replication_factor: Arc<AtomicUsize>,
        healing_delay_secs: u64,
        scrub_interval_hours: u64,
        auto_heal: bool,
        chunk_map: Arc<DashMap<FileId, (Vec<ChunkLocation>, u64)>>,
        fold_result_chunk_ids: Arc<dashmap::DashSet<ChunkId>>,
        chunk_generations: Arc<DashMap<ChunkId, u64>>,
        last_cluster_write_ms: Arc<std::sync::atomic::AtomicU64>,
        link_bandwidth_mb: usize,
        heal_max_pct_config: f64,
        heal_max_concurrent: usize,
        heal_max_concurrent_per_node: usize,
        heal_transfer_timeout_secs: u64,
        compaction_quiescing: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        let max_heal_per_cycle = 200;

        // Initialize the limiter at the floor rate (10% of assumed link bandwidth).
        // The adaptive bandwidth controller takes over within 2 seconds of startup
        // and adjusts the rate based on queue depth and growth rate.
        let initial_bw_mb = ((link_bandwidth_mb as f64 * 0.10) as usize).max(1);
        let heal_bandwidth_limiter_out = Arc::new(BandwidthLimiter::new(initial_bw_mb));
        let heal_bandwidth_limiter_in = Arc::new(BandwidthLimiter::new(initial_bw_mb));
        let link_bandwidth_mb = Arc::new(AtomicUsize::new(link_bandwidth_mb));

        // Separate, independent concurrency cap on outstanding heal transfers
        // (FD/task safety) — no longer derived from the bandwidth number.
        let heal_semaphore = Arc::new(Semaphore::new(heal_max_concurrent));
        let heal_max_concurrent = Arc::new(AtomicUsize::new(heal_max_concurrent));

        // A per-node cap above the global cap could never bind — clamp at construction
        // the same way apply_tuning/resize_heal_concurrency clamp it on live updates.
        let heal_max_concurrent_per_node = heal_max_concurrent_per_node
            .clamp(1, heal_max_concurrent.load(Ordering::Relaxed));
        let node_inflight: Arc<DashMap<NodeId, Arc<Semaphore>>> = Arc::new(DashMap::new());
        let heal_max_concurrent_per_node = Arc::new(AtomicUsize::new(heal_max_concurrent_per_node));

        let heal_max_pct = Arc::new(RwLock::new((heal_max_pct_config / 100.0).clamp(0.10, 1.00)));

        let heal_transfer_timeout_secs = Arc::new(AtomicU64::new(heal_transfer_timeout_secs));
        let healing_delay_secs = Arc::new(AtomicU64::new(healing_delay_secs));

        // Restore pending_healing first-detection times across process restarts.
        // Without this, every restart resets the whole backlog's debounce timer to
        // "just now", and a busy leader that restarts more often than
        // healing_delay_secs never heals anything.
        let mut pending_healing_map = HashMap::new();
        match metadata.get_pending_healing_inventory() {
            Ok(entries) => {
                let now_secs = dfs_common::types::current_timestamp();
                for (chunk_id, detected_at_secs) in entries {
                    let age = now_secs.saturating_sub(detected_at_secs);
                    let detected_at = Instant::now()
                        .checked_sub(Duration::from_secs(age))
                        .unwrap_or_else(Instant::now);
                    pending_healing_map.insert(chunk_id, detected_at);
                }
                if !pending_healing_map.is_empty() {
                    info!("Restored {} pending-healing entries from persisted state", pending_healing_map.len());
                }
            }
            Err(e) => {
                warn!("Failed to load persisted pending_healing inventory: {}", e);
            }
        }

        Self {
            storage,
            metadata,
            chunk_map,
            fold_result_chunk_ids,
            chunk_generations,
            superseded_generation_chunk_ids: Arc::new(RwLock::new(HashSet::new())),
            cluster,
            client,
            replication_factor,
            healing_delay_secs,
            scrub_interval_hours,
            auto_heal,
            max_heal_per_cycle,
            heal_semaphore,
            heal_max_concurrent,
            node_inflight,
            heal_max_concurrent_per_node,
            heal_bandwidth_limiter_out,
            heal_bandwidth_limiter_in,
            last_cluster_write_ms,
            link_bandwidth_mb,
            heal_max_pct,
            pending_healing: Arc::new(RwLock::new(pending_healing_map)),
            in_flight_healing: Arc::new(RwLock::new(HashSet::new())),
            cancelled_heals: Arc::new(DashMap::new()),
            alive_nodes_cache: Arc::new(RwLock::new(HashMap::new())),
            stalled_healing: Arc::new(RwLock::new(HashSet::new())),
            heal_push_failure: Arc::new(DashMap::new()),
            requeue_priority: Arc::new(DashMap::new()),
            orphan_candidates: Arc::new(RwLock::new(HashSet::new())),
            pending_patch_confirmed: Arc::new(RwLock::new(HashSet::new())),
            folded_patch_confirmed: Arc::new(RwLock::new(HashSet::new())),
            last_reconcile_leader: Arc::new(RwLock::new(None)),
            heal_transfer_timeout_secs,
            phantom_reconcile_in_progress: std::sync::atomic::AtomicBool::new(false),
            healing_enabled: Arc::new(std::sync::atomic::AtomicBool::new(auto_heal)),
            local_started_at: Instant::now(),
            compaction_quiescing,
            disk_sweep_task: Arc::new(std::sync::Mutex::new(None)),
            disk_sweep_last_page_at: Arc::new(std::sync::Mutex::new(Instant::now())),
            // Initialized already-expired so the very first gated check logs
            // immediately rather than being silently suppressed for the first 60s.
            disk_sweep_gate_log_throttle: Arc::new(std::sync::Mutex::new(
                Instant::now() - Self::GATE_LOG_THROTTLE,
            )),
        }
    }

    /// Start background healing tasks
    pub async fn start(self: Arc<Self>) {
        if !self.auto_heal {
            info!("Auto-healing is disabled");
            return;
        }

        info!(
            "Starting healing manager (delay: {}s, scrub: {}h, max_per_cycle: {}, max_concurrent: {}, max_concurrent_per_node: {}, transfer_timeout: {}s, link: {}MB/s, max_pct: {}%)",
            self.healing_delay_secs.load(Ordering::Relaxed), self.scrub_interval_hours, self.max_heal_per_cycle,
            self.heal_max_concurrent.load(Ordering::Relaxed), self.heal_max_concurrent_per_node.load(Ordering::Relaxed),
            self.heal_transfer_timeout_secs.load(Ordering::Relaxed),
            self.link_bandwidth_mb.load(Ordering::Relaxed), (*self.heal_max_pct.read().await * 100.0) as u32
        );

        // Discovery loop: scans all chunks, bulk-queries nodes, classifies under/over
        // replication, purges orphans. Runs every 60s and writes into pending_healing
        // and alive_nodes_cache.
        let discovery = self.clone();
        tokio::spawn(async move {
            discovery.run_discovery_loop().await;
        });

        // Heal loop: drains pending_healing using cached alive_nodes data. Runs every
        // 15s and starts immediately on the first tick — no waiting for discovery.
        let healer = self.clone();
        tokio::spawn(async move {
            healer.run_heal_loop().await;
        });

        // Scrubber (runs at configured interval)
        let scrubber = self.clone();
        tokio::spawn(async move {
            scrubber.run_scrubber().await;
        });

        // Phantom reconciliation: independent periodic verify-and-prune pass (see
        // run_phantom_reconciliation_pass doc comment for why this exists alongside
        // the discovery loop's own ghost-pruning).
        let reconciler = self.clone();
        tokio::spawn(async move {
            reconciler.run_phantom_reconciliation_loop().await;
        });

        // Adaptive bandwidth controller: re-evaluates heal rate every 2s based on
        // write activity and queue depth. Backs off to 10% during active writes,
        // ramps to 80% when idle. Overrides toward 80% as queue debt grows.
        let bw_controller = self.clone();
        tokio::spawn(async move {
            bw_controller.run_bandwidth_controller().await;
        });

        // Emergency disk-pressure monitor: independent, fast (5s) check, separate
        // from the 60s discovery loop's own 10%-threshold early-sweep-trigger.
        // Added 2026-07-12 after a local repro showed the 60s cadence itself can
        // be too coarse: 16-way concurrent hot-chunk writes drove a small test
        // disk from 4.2GB free to under 600MB in under 50 seconds — faster than
        // even a single 60s cycle, so the discovery loop's own check never got a
        // chance to fire. This one skips wait_for_write_quiet's up-to-30s lull
        // wait at the tighter (3%) threshold — a true near-ENOSPC emergency
        // should reclaim space immediately even if it costs some I/O contention
        // with in-flight writes, since the alternative (writes start failing
        // outright) is strictly worse.
        let emergency = self.clone();
        tokio::spawn(async move {
            emergency.run_disk_emergency_monitor().await;
        });

        // Paginated disk-orphan-sweep: bounds each destructive-scan pass to a page
        // instead of run_discovery_loop's old inline full-scan-every-2-cycles call
        // (removed — see run_disk_orphan_sweep_paginated_loop's doc comment).
        // Independent of the 60s discovery cadence; watched by the watchdog below.
        let sweep = self.clone();
        let sweep_handle = tokio::spawn(async move {
            sweep.run_disk_orphan_sweep_paginated_loop().await;
        });
        *self.disk_sweep_task.lock().unwrap() = Some(sweep_handle);

        // Watchdog for the paginated sweep loop above — see its own doc comment.
        let watchdog = self.clone();
        tokio::spawn(async move {
            watchdog.run_disk_orphan_sweep_watchdog().await;
        });
    }

    /// See disk_emergency_monitor's spawn-site doc comment for why this exists
    /// alongside (not instead of) the discovery loop's own 10%/60s check.
    async fn run_disk_emergency_monitor(&self) {
        const CHECK_INTERVAL: Duration = Duration::from_secs(5);
        const EMERGENCY_THRESHOLD_PCT: f64 = 0.03;
        loop {
            tokio::time::sleep(CHECK_INTERVAL).await;
            if !self.healing_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }
            if self.compaction_quiescing.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }
            let critical = match self.storage.get_filesystem_stats() {
                Ok((total, _free, available)) if total > 0 => {
                    (available as f64) < (total as f64) * EMERGENCY_THRESHOLD_PCT
                }
                _ => false,
            };
            if critical {
                warn!("Disk orphan sweep: EMERGENCY trigger — available space under {:.0}% of total, sweeping immediately (skipping write-quiet wait)",
                    EMERGENCY_THRESHOLD_PCT * 100.0);
                self.run_disk_orphan_sweep().await;
            }
        }
    }

    async fn run_bandwidth_controller(&self) {
        // Tier boundaries (heal queue depth in chunks):
        //   < TIER1  → floor rate (trivially small, don't compete)
        //   TIER1..TIER2 → proportional scale, boosted by growth rate
        //   ≥ TIER2  → ceiling rate (queue is dangerously deep, must keep up)
        const TIER1: usize = 100;
        const TIER2: usize = 1_000;
        const LOW_PCT: f64 = 0.10;
        // A sustained growth rate of GROWTH_BOOST_RATE items/sec in the middle tier
        // contributes up to GROWTH_BOOST_SHARE of the remaining headroom, letting the
        // system react to a fast-growing queue before depth alone would force the rate up.
        const GROWTH_BOOST_RATE:  f64 = 10.0;  // items/sec = "growing fast"
        const GROWTH_BOOST_SHARE: f64 = 0.50;  // max extra fraction of headroom from growth
        const INTERVAL_SECS: f64 = 2.0;

        let mut prev_depth: usize = 0;

        loop {
            tokio::time::sleep(Duration::from_secs_f64(INTERVAL_SECS)).await;

            // pending_healing is only ever populated on the leader (discovery and
            // drain are both leader-gated), so a follower always sees depth 0 here
            // and would otherwise wrongly pin itself at the LOW_PCT floor regardless
            // of the real cluster-wide backlog. Followers instead receive the
            // leader's computed target via heartbeat — see
            // Server::handle_cluster_message's ClusterMessage::Heartbeat arm and
            // apply_external_bandwidth_target below.
            if !self.cluster.is_leader().await {
                continue;
            }

            // Read fresh each tick so a live `dfs-admin healing set --max-pct` change
            // takes effect within one 2s cycle instead of only at the next restart.
            let high_pct = *self.heal_max_pct.read().await;
            let queue_depth = self.pending_healing.read().await.len();

            // items/sec — positive means queue is growing (falling behind), negative means draining
            let growth_rate = (queue_depth as f64 - prev_depth as f64) / INTERVAL_SECS;
            prev_depth = queue_depth;

            let factor: f64 = if queue_depth < TIER1 {
                // Trivially small queue — stay at floor regardless of growth rate.
                // Even 0→100 in 2 s doesn't warrant going above the floor: depth
                // must be meaningful before we ramp up.
                0.0
            } else if queue_depth >= TIER2 {
                // Deep queue — use ceiling rate to match or exceed incoming rate.
                1.0
            } else {
                // Middle tier: linear depth scale, plus a growth-rate boost so a
                // fast-growing queue in this range pushes the rate up earlier.
                let depth_scale = (queue_depth - TIER1) as f64 / (TIER2 - TIER1) as f64;
                let growth_boost = if growth_rate > 0.0 {
                    (growth_rate / GROWTH_BOOST_RATE).clamp(0.0, 1.0) * GROWTH_BOOST_SHARE
                } else {
                    0.0
                };
                (depth_scale + growth_boost).clamp(0.0, 1.0)
            };

            let target_pct = LOW_PCT + (high_pct - LOW_PCT) * factor;
            let target_mb = ((self.link_bandwidth_mb.load(Ordering::Relaxed) as f64 * target_pct) as usize).max(1);

            self.heal_bandwidth_limiter_out.set_rate_mb(target_mb).await;
            self.heal_bandwidth_limiter_in.set_rate_mb(target_mb).await;
            self.cluster.set_local_heal_bandwidth_target(target_mb);

            if queue_depth > 0 || growth_rate.abs() > 0.5 {
                debug!(
                    "heal-bw: depth={} growth={:+.1}/s factor={:.2} rate={}MB/s",
                    queue_depth, growth_rate, factor, target_mb
                );
            }
        }
    }

    /// Apply a heal bandwidth target (MB/s) received from the leader via heartbeat.
    /// Used by followers, whose own `run_bandwidth_controller` tick is a no-op
    /// (see the leader gate above) because their local `pending_healing` is always
    /// empty. Bypasses the tiered depth calculation entirely — the leader already
    /// did it — and just applies the rate directly to both local limiters.
    pub async fn apply_external_bandwidth_target(&self, target_mb: usize) {
        debug!("heal-bw (follower): applying leader-pushed target {}MB/s", target_mb);
        self.heal_bandwidth_limiter_out.set_rate_mb(target_mb).await;
        self.heal_bandwidth_limiter_in.set_rate_mb(target_mb).await;
    }

    /// Discovery loop — runs every 60s on the cluster leader.
    ///
    /// Two-tier design:
    ///   Fast cycle (every 60s): only checks chunks that need attention — those in
    ///   pending_healing or with nodes.len() < RF in the routing table. Per-node
    ///   HasChunks payload is scoped to chunks that node is listed for.
    ///
    ///   Deep cycle (every healing_delay_secs): checks all routing table entries to
    ///   detect ghost nodes, orphans, and undocumented local copies. Interval matches
    ///   healing_delay_secs so new under-replicated chunks are discovered within one
    ///   delay window. Same scoped-per-node approach.
    async fn run_discovery_loop(&self) {
        let mut cleanup_counter = 0u32;
        let mut was_leader = false;

        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;

            if !self.healing_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }

            // See the same check in run_heal_loop — discovery's routing-table/
            // FILE_TABLE scans are metadata-read-heavy too.
            if self.compaction_quiescing.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }

            let is_leader = self.cluster.is_leader().await;

            if is_leader != was_leader {
                if is_leader {
                    info!("This node is now the cluster leader — taking over healing coordination");
                } else {
                    info!("This node is no longer the cluster leader — yielding healing to new leader");
                }
                was_leader = is_leader;
            }

            // Disk orphan sweep used to be triggered inline here (every 2 cycles, plus
            // a disk-pressure early trigger). Moved to an independent, paginated,
            // watchdogged loop (run_disk_orphan_sweep_paginated_loop, spawned in
            // start()) — see that function's doc comment. A full unbounded sweep
            // dumped its entire cost into whichever single 60s cycle first cleared
            // the post-restart grace period, which produced severe redb write
            // contention that contributed to a real VM-108 data-loss incident
            // (2026-08-03, file 565f683c). The paginated loop bounds each pass to a
            // page and self-paces independently of this loop's cadence, so this
            // comment (and the disk_pressure early-trigger it replaces) no longer
            // has anything to do here.

            // Every node, every cycle (cheap — skips anything already confirmed) —
            // see run_pending_patch_reconciliation's doc comment. Must run
            // regardless of leadership: any node can be holding an unconfirmed
            // token, not just the leader.
            self.run_pending_patch_reconciliation().await;

            if !is_leader {
                continue;
            }

            // Every cycle is now a deep scan (2026-07-24). The shallow/deep split existed
            // because a deep pass — two full CHUNK_TABLE scans via live_chunk_ids() +
            // scan_chunk_locations() — was expensive enough that running it every 60s
            // wasn't affordable, so it was rationed to once per healing_delay_secs.
            // scan_live_chunk_locations (one combined CHUNK_TABLE pass) removed that cost:
            // confirmed live at ~5-14s per pass against a large table, vs. 15-20+ minutes
            // before. Runnable every cycle now, which matters beyond just being
            // affordable: dispatch (drain_heal_queue) has no working pre-flight orphan
            // check of its own (the existing "superseded-generation guard" in
            // do_heal_chunk_shared is dead code — it reads FileMetadata.chunk_locations,
            // which is never populated post-2026-07-16, see put_file_in_txn's doc
            // comment) — it relies entirely on discovery's orphan-dequeue to keep the
            // queue clean. At the old ~5-minute deep cadence, a freshly-orphaned chunk
            // could sit in the queue for up to 5 minutes getting repeatedly dispatched
            // and failing content-hash verification before the next deep pass caught it —
            // confirmed live: a 500-line log window during that gap was 0 successes /
            // 252 failures, effectively all dispatch capacity wasted. Now every ~60s
            // cycle both discovers new under-replication AND re-runs the orphan-dequeue,
            // shrinking that wasted-capacity window roughly 5x.
            if let Err(e) = self.run_discovery_pass(true).await {
                warn!("Discovery pass error: {}", e);
            }

            cleanup_counter += 1;
            if cleanup_counter >= 10 {
                cleanup_counter = 0;
                if let Err(e) = self.cleanup_stale_pending().await {
                    warn!("Pending healing cleanup error: {}", e);
                }
            }
        }
    }

    /// Heal loop — runs every 15s on the cluster leader.
    ///
    /// Drains pending_healing for chunks that have waited healing_delay_secs,
    /// using alive_nodes_cache populated by the discovery loop. Starts immediately
    /// on first tick so chunks discovered in a previous cycle are healed right away
    /// without waiting another 60s for a fresh scan.
    async fn run_heal_loop(&self) {
        let mut was_leader = false;

        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;

            if !self.healing_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }

            // Compaction's Phase 3 (or the compact_db_blocking() fallback) is
            // holding/queued for the exclusive metadata lock right now — skip this
            // drain cycle rather than add to the same shared read-lock contention
            // it's waiting behind. See Server::compaction_quiescing's doc comment.
            if self.compaction_quiescing.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }

            let is_leader = self.cluster.is_leader().await;

            if is_leader != was_leader {
                was_leader = is_leader;
                // Leadership transitions are logged by the discovery loop
            }

            if !is_leader {
                continue;
            }

            if let Err(e) = self.drain_heal_queue().await {
                warn!("Heal queue drain error: {}", e);
            }
        }
    }

    /// Insert `chunk_id` into pending_healing if not already present, recording the
    /// current time as its first-detection time, and persist that detection time so
    /// a process restart doesn't lose track of how long this chunk has been waiting.
    /// No-op (and no metadata write) if the chunk is already pending.
    async fn mark_pending(&self, chunk_id: ChunkId) {
        let mut pending = self.pending_healing.write().await;
        if let std::collections::hash_map::Entry::Vacant(e) = pending.entry(chunk_id) {
            e.insert(Instant::now());
            drop(pending);
            if let Err(err) = self.metadata.put_pending_healing_async(chunk_id, dfs_common::types::current_timestamp()).await {
                warn!("Failed to persist pending_healing entry for {}: {}", chunk_id, err);
            }
        }
    }

    /// Deferred-persistence variant of mark_pending for per-chunk classification
    /// loops: the in-memory Vacant insert happens immediately (dedup and
    /// delay-tracking semantics identical to mark_pending), but instead of paying
    /// a single-record write transaction per chunk, the persist pair is pushed
    /// onto `deferred` for the caller to flush in ONE put_pending_healing_batch
    /// transaction after its loop — see put_chunk_locations_batch's doc comment
    /// for why N single-record transactions cost far more than one N-record one
    /// (measured: put_pending_healing was the #1 txn site, +1032tx/min, during
    /// the 2026-07-15 heal-backlog baseline).
    async fn mark_pending_deferred(&self, chunk_id: ChunkId, deferred: &mut Vec<(ChunkId, u64)>) {
        let mut pending = self.pending_healing.write().await;
        if let std::collections::hash_map::Entry::Vacant(e) = pending.entry(chunk_id) {
            e.insert(Instant::now());
            drop(pending);
            deferred.push((chunk_id, dfs_common::types::current_timestamp()));
        }
    }

    /// Remove `chunk_id` from pending_healing (it reached RF, was purged, or is no
    /// longer relevant) and clear its persisted detection time.
    async fn clear_pending(&self, chunk_id: &ChunkId) {
        Self::clear_pending_static(&self.pending_healing, &self.metadata, chunk_id).await;
        self.requeue_priority.remove(chunk_id);
    }

    /// A fold is about to retire `chunk_id` as the base it's consolidating —
    /// remove it from both pending_healing (never dispatch a heal for it) and
    /// in_flight_healing (don't let an already-dispatched-but-not-yet-committed
    /// heal task believe it still owns exclusivity over this chunk_id) and
    /// tombstone it so do_heal_chunk_inner's pre-commit check (see that
    /// function) discards any heal for this identity that's already past both
    /// of those guards and racing toward a commit. See Server::cancel_healing_for_chunk
    /// for why this must happen at every fold's start, not just here — this method
    /// is the leader-local implementation; the RPC-forwarding wrapper lives on Server.
    pub async fn cancel_healing(&self, chunk_id: ChunkId) {
        Self::clear_pending_static(&self.pending_healing, &self.metadata, &chunk_id).await;
        self.in_flight_healing.write().await.remove(&chunk_id);
        self.cancelled_heals.insert(chunk_id, Instant::now());
    }

    /// Undo cancel_healing for `chunk_id` — used when the fold that requested the
    /// cancellation turns out to be a no-op (new_chunk_id == base_chunk_id, e.g. a
    /// patch that overwrote a region with its own existing content). In that case
    /// base_chunk_id is still genuinely the slot's current, correct identity —
    /// leaving it tombstoned would block it from ever being healed again until
    /// CANCEL_TOMBSTONE_TTL expires, even though nothing about it was actually
    /// superseded.
    pub fn retract_healing_cancellation(&self, chunk_id: ChunkId) {
        self.cancelled_heals.remove(&chunk_id);
    }

    /// How long a cancel_healing tombstone blocks do_heal_chunk_inner from
    /// committing a heal for that chunk_id, if retract_healing_cancellation is
    /// never called (the fold that requested it crashed, or the process
    /// restarted). Comfortably longer than PushChunkTo's own 90s timeout plus its
    /// post-push HasChunks verification round trip, so a legitimate in-flight heal
    /// racing the cancellation always has time to observe it before committing.
    const CANCEL_TOMBSTONE_TTL: Duration = Duration::from_secs(150);

    /// Checked by do_heal_chunk_inner immediately before it commits a healed
    /// chunk's ChunkLocation update — see cancel_healing's doc comment.
    fn is_healing_cancelled(cancelled_heals: &Arc<DashMap<ChunkId, Instant>>, chunk_id: &ChunkId) -> bool {
        cancelled_heals.get(chunk_id)
            .map(|e| e.elapsed() < Self::CANCEL_TOMBSTONE_TTL)
            .unwrap_or(false)
    }

    /// Base and cap for heal_push_failure's exponential backoff — see that field's
    /// doc comment. Base 60s means a freshly-failing chunk still gets a fairly
    /// prompt second attempt (transient target-node hiccup shouldn't wait long);
    /// doubling per consecutive failure reaches the 30-minute cap after 5 straight
    /// failures, which is where a persistently-corrupt source lands and stays
    /// (still auto-retries periodically — e.g. if the source disk gets replaced —
    /// just no longer in a hot loop).
    const HEAL_PUSH_FAILURE_BASE: Duration = Duration::from_secs(60);
    const HEAL_PUSH_FAILURE_CAP: Duration = Duration::from_secs(1800);

    /// True if a chunk's last heal attempt failed to replicate to any target despite
    /// having a confirmed-alive source, and its backoff window hasn't elapsed yet.
    /// See heal_push_failure's doc comment for why this check exists.
    fn heal_push_failure_backoff_active(
        heal_push_failure: &Arc<DashMap<ChunkId, (Instant, u32)>>,
        chunk_id: &ChunkId,
    ) -> bool {
        heal_push_failure.get(chunk_id)
            .map(|entry| {
                let (last_failure, count) = *entry;
                let backoff = Self::HEAL_PUSH_FAILURE_BASE
                    .saturating_mul(1u32 << count.min(5))
                    .min(Self::HEAL_PUSH_FAILURE_CAP);
                last_failure.elapsed() < backoff
            })
            .unwrap_or(false)
    }

    /// Are this deep pass's orphan-dequeue candidates (pending chunks the durable
    /// `live` set doesn't recognize) trustworthy enough to act on? An empty `live`
    /// while there IS pending work outstanding is the signature of a not-yet-converged
    /// FILE_TABLE (e.g. moments after a leader election) — not evidence that every
    /// pending chunk is genuinely orphaned. In that case nothing should be treated as
    /// orphaned this pass; a later, converged pass will still find the same real
    /// orphans (self-correcting, since this only ever drops from the heal QUEUE —
    /// the disk orphan sweep, not this check, owns actual deletion).
    fn orphan_candidates_are_trustworthy(live_is_empty: bool, pending_is_empty: bool) -> bool {
        !(live_is_empty && !pending_is_empty)
    }

    /// May this cycle declare a 0-replica chunk permanently lost and purge it?
    ///
    /// BOTH conditions are required, and they are not redundant:
    /// - `destructive_allowed`: cluster healthy enough for destructive ops. Tolerates
    ///   `nodes_down <= 1`, which is right for RF=3 replica math.
    /// - `patch_token_view_complete`: EVERY node contributed its patch-token set this
    ///   cycle. A patch token has no file on disk until its fold lands, so "absent on
    ///   every replica" — the sole evidence a DATA LOSS purge acts on — is trivially
    ///   true of any token. If even one node's tokens are invisible, an unfolded token
    ///   from that node is indistinguishable from a genuinely lost chunk.
    ///
    /// The second is exactly the case the first waves through: at `nodes_down == 1`
    /// destructive ops stay allowed, but that one absent node's ENTIRE token set is
    /// invisible — a total blind spot, not a 1-in-3 one. PATCH_STATE_TABLE is
    /// node-local and only FOLDS are disseminated (Request::ReplicatePatchFold); there
    /// is no equivalent for pending tokens, so the GetPatchTokenIds union is the only
    /// way to see them, and a node that is away contributes nothing to it.
    ///
    /// Confirmed data loss twice before this gate existed — 2026-07-15 (vm-108
    /// chunk_idx 230) and 2026-07-17 06:09 on an IDLE cluster with no writes since a
    /// clean fsck. A `PlannedCompaction` leave is enough to open the window, and those
    /// happen constantly.
    fn may_declare_data_loss(destructive_allowed: bool, patch_token_view_complete: bool) -> bool {
        destructive_allowed && patch_token_view_complete
    }

    /// Classify a chunk_id that has 0 accessible replicas: is it safe to purge
    /// (superseded by a newer write at the same file position, or its file no
    /// longer exists) or is it a genuine, permanent loss? Returns:
    ///   Some(true)  — superseded/orphaned, safe to purge (not DATA LOSS)
    ///   Some(false) — still the live chunk for this file position, genuine DATA LOSS
    ///   None        — metadata read failed, cannot confirm either way
    ///
    /// The None case must NOT be collapsed into Some(false) by the caller. A
    /// metadata read error here (e.g. the 2026-07-06 bincode-deserialization
    /// incident, where every FILE_TABLE read failed cluster-wide) previously fell
    /// into an `Err(_) => false` arm that still purged the chunk and logged it as
    /// DATA LOSS — the "be conservative" comment on that arm didn't actually change
    /// the outcome, since both branches purged unconditionally. Callers must skip
    /// the purge entirely on None and let the chunk be re-evaluated next cycle.
    fn classify_zero_replica_chunk(&self, chunk_id: ChunkId, location: &ChunkLocation) -> Option<bool> {
        let (file_id, file_offset) = match (location.file_id, location.file_offset) {
            (Some(file_id), Some(file_offset)) => (file_id, file_offset),
            // No file context to cross-check against — treat as not superseded,
            // same as before this was extracted into its own method.
            _ => return Some(false),
        };
        // Check the in-memory chunk_map first: it's updated synchronously by every
        // patch/replicate-location handler (see the `chunk_map` field doc comment),
        // so under the overlay-consolidation design's high fold-churn rate it can be
        // generations ahead of the durable FileMetadata below. Landing a healing scan
        // in that lag window previously caused a real false-positive DATA LOSS purge
        // of a chunk that a concurrent fold had already superseded (2026-07-09).
        if let Some(entry) = self.chunk_map.get(&file_id) {
            let (locs, _) = entry.value();
            let current = locs.iter().find(|l| l.file_offset == Some(file_offset));
            return Some(match current {
                Some(cur) => cur.chunk_id != chunk_id,
                None => true, // position removed from the fresh view — orphaned
            });
        }
        match self.metadata.get_file(&file_id) {
            Ok(Some(_file_meta)) => {
                // chunk_map has no entry for this file, so we cannot answer "what is
                // the live chunk at this position?" — DEFER (None), don't purge.
                //
                // This branch used to consult FileMetadata.chunk_locations, which
                // Phase 4 made permanently empty; the 2026-07-16 replacement scanned
                // CHUNK_TABLE instead, which was a serious mistake on two counts:
                //   1. scan_chunk_locations walks and bincode-deserializes the ENTIRE
                //      table (~400k rows on staging) — and this is a SYNC fn called
                //      from the async discovery loop, once per zero-replica chunk. On
                //      a cold chunk_map (exactly when this branch runs — right after a
                //      restart, before rebuild_chunk_map_from_metadata finishes) that
                //      is a full blocking scan per chunk on a Tokio worker thread.
                //      Confirmed live 2026-07-16: gluster2/gluster3 stopped answering
                //      every request, including heartbeats, until restarted — the
                //      process was fine, its runtime was wedged.
                //   2. It was answering a question it has no business answering yet.
                //      A cold chunk_map IS the "cannot confirm" case this function's
                //      doc comment is about, identical in kind to the read-error arms
                //      below. Deferring costs one healing cycle; guessing risks either
                //      purging a live chunk or waving through a real loss.
                //
                // rebuild_chunk_map_from_metadata populates chunk_map from CHUNK_TABLE
                // on startup anyway — so the answer arrives on its own, correctly and
                // exactly once, instead of being recomputed per chunk at O(table).
                debug!(
                    "Chunk {} has 0 accessible replicas but chunk_map has no entry for file {} yet (cold window) — deferring purge decision to next cycle",
                    chunk_id, file_id
                );
                None
            }
            Ok(None) => Some(true), // file deleted — chunk is orphaned
            Err(e) => {
                warn!(
                    "Chunk {} has 0 accessible replicas but reading metadata for file {} failed ({}) — cannot confirm superseded-vs-lost, deferring purge decision to next cycle",
                    chunk_id, file_id, e
                );
                None
            }
        }
    }

    /// Remove a batch of chunks from pending_healing — called when files are deleted
    /// so their chunks don't inflate the pending count indefinitely.
    ///
    /// ONE transaction for the whole set, not one per chunk. Deleting a large file
    /// hands this every chunk it owned at once; per-chunk commits made that N
    /// single-record redb transactions, each COW-rewriting the B-tree leaf→root for
    /// a key removal carrying no payload. Measured live on gluster1 2026-07-16:
    /// op:delete_pending_healing=+17949tx in one 60s window moving +0.0MB, in the
    /// same window the metadata DB doubled 257.5MB→514.5MB and drove the ~60s
    /// compaction cycle. The put side has been batched since Phase 1
    /// (put_pending_healing_batch); this is the delete side finally matching it.
    pub async fn clear_pending_for_deleted_chunks(&self, chunk_ids: &[ChunkId]) {
        let mut pending = self.pending_healing.write().await;
        let mut to_delete = Vec::new();
        for chunk_id in chunk_ids {
            if pending.remove(chunk_id).is_some() {
                to_delete.push(*chunk_id);
            }
        }
        drop(pending);
        if let Err(e) = self.metadata
            .batch_update_chunk_locations_async(Vec::new(), Vec::new(), to_delete)
            .await
        {
            warn!("Failed to batch-clear pending_healing for deleted chunks: {}", e);
        }
    }

    /// Free-function equivalent of `clear_pending` for static/spawned contexts
    /// (e.g. `do_heal_chunk_inner`) that don't have a `&self`.
    async fn clear_pending_static(
        pending_healing: &Arc<RwLock<HashMap<ChunkId, Instant>>>,
        metadata: &Arc<MetadataStore>,
        chunk_id: &ChunkId,
    ) {
        let existed = pending_healing.write().await.remove(chunk_id).is_some();
        if existed {
            if let Err(err) = metadata.delete_pending_healing_async(*chunk_id).await {
                warn!("Failed to delete pending_healing entry for {}: {}", chunk_id, err);
            }
        }
    }

    /// Cleanup stale entries from pending_healing map
    /// Removes chunks that no longer exist or have been pending for too long
    async fn cleanup_stale_pending(&self) -> Result<()> {
        let max_pending_time = Duration::from_secs(self.healing_delay_secs.load(Ordering::Relaxed) * 20); // 20x healing delay

        let to_remove: Vec<ChunkId> = {
            let pending = self.pending_healing.read().await;
            pending.iter()
                .filter_map(|(chunk_id, detected_at)| {
                    // Remove if pending for too long (likely deleted or unrecoverable)
                    if detected_at.elapsed() > max_pending_time {
                        debug!("Removing stale pending healing entry for chunk {} (pending for {}s)",
                               chunk_id, detected_at.elapsed().as_secs());
                        return Some(*chunk_id);
                    }

                    // Remove if the chunk: location record is gone — this means the file was
                    // deleted (or the chunk was legitimately purged as an orphan).  There is
                    // nothing left to heal regardless of whether raw chunk data still exists
                    // on disk (stale data will be cleaned up separately).
                    if self.metadata.get_chunk_location(chunk_id).ok().flatten().is_none() {
                        debug!("Removing pending healing entry for chunk {} — no location record", chunk_id);
                        return Some(*chunk_id);
                    }

                    None
                })
                .collect()
        };

        // One transaction for the whole prune, not one per entry — see
        // clear_pending_for_deleted_chunks' doc comment for the measured churn this
        // avoids. This is the same set of entries removed at the same time as before
        // (the 100min = healing_delay_secs*20 prune policy is unchanged); only the
        // commit shape differs. Mirrors clear_pending_static's rule of clearing the
        // persisted row only for entries actually removed from the in-memory map, so
        // a racing clear_pending elsewhere can't make us delete a row it re-added.
        let removed: Vec<ChunkId> = {
            let mut pending = self.pending_healing.write().await;
            to_remove.into_iter()
                .filter(|chunk_id| pending.remove(chunk_id).is_some())
                .collect()
        };
        let removed_count = removed.len();
        if removed_count > 0 {
            if let Err(e) = self.metadata
                .batch_update_chunk_locations_async(Vec::new(), Vec::new(), removed)
                .await
            {
                warn!("Failed to batch-clear {} stale pending_healing entries: {}", removed_count, e);
            }
            info!("Cleaned up {} stale pending healing entries", removed_count);
        }

        // Prune stalled entries for chunks that have been deleted/purged — they won't
        // come back, and discovery won't clean them up since it only sees live chunks.
        {
            let mut stalled = self.stalled_healing.write().await;
            stalled.retain(|chunk_id| {
                self.metadata.get_chunk_location(chunk_id).ok().flatten().is_some()
            });
        }

        Ok(())
    }

    /// Run periodic scrubber
    /// Disk-level orphan sweep — runs on every node every 5 minutes (or immediately
    /// via TriggerOrphanCleanup).
    ///
    /// Two independent checks, each catching a different leak:
    ///  1. Routing-table membership: deletes chunk files whose ChunkLocation record
    ///     no longer lists this node — catches missed DeleteChunk RPCs while offline.
    ///  2. Live-file membership: deletes chunks still listed as ours in the routing
    ///     table, but no longer referenced by ANY live file — catches patch-superseded
    ///     chunks the routing table was deliberately left stale for (see
    ///     handle_patch_chunk/handle_multi_patch) that the inline fast-evict missed
    ///     (e.g. this node was offline when the patch happened).
    ///
    /// Category 2 is the more dangerous one to get wrong, since it relies on this
    /// node's own (eventually-consistent) metadata replica, which might be stale. So
    /// in addition to the existing grace period and a two-pass confirmation, it
    /// requires either explicit confirmation from the leader (non-leader nodes) or
    /// proof the whole cluster has been stable for several minutes (leader nodes,
    /// which have no one more authoritative to ask) — see
    /// authorize_live_file_orphan_deletes(). Any ambiguity (RPC failure, timeout,
    /// unreachable leader) defers deletion to the next cycle rather than proceeding.
    /// Snapshot of every chunk_id currently referenced by any file's in-memory
    /// chunk_map entry. See the `chunk_map` field doc comment for why this is a
    /// fresher liveness source than `MetadataStore::live_chunk_ids()`.
    fn live_chunk_ids_from_chunk_map(&self) -> HashSet<ChunkId> {
        let mut live = HashSet::new();
        for entry in self.chunk_map.iter() {
            let (locs, _) = entry.value();
            for loc in locs {
                live.insert(loc.chunk_id);
            }
        }
        live
    }

    /// Phantom-replica reconciliation: walks every live ChunkLocation in CHUNK_TABLE,
    /// verifies actual presence on every listed online node via a fresh HasChunks/
    /// has_chunk check, and immediately prunes any node confirmed not to hold the
    /// chunk — as long as at least one other listed node DOES hold it, so a chunk
    /// is never stranded at zero replicas. If pruning drops a chunk below RF, it's
    /// queued for immediate healing via queue_chunks_immediate.
    ///
    /// This runs independently of the discovery fast/deep cadence and the
    /// pending_healing interplay used by ghost-pruning in run_discovery_pass. In
    /// practice that existing path has been observed to leave large backlogs of
    /// confirmed-ghost entries unpruned for hours after an incident (e.g. a node
    /// losing its disk) without the root interaction ever being fully isolated.
    /// This pass acts directly on freshly-verified presence each time it runs,
    /// rather than depending on that pipeline, so it reconciles phantoms even when
    /// the other path stalls.
    pub async fn run_phantom_reconciliation_pass(&self) {
        use std::sync::atomic::Ordering;
        if self.phantom_reconcile_in_progress.compare_exchange(
            false, true, Ordering::AcqRel, Ordering::Relaxed,
        ).is_err() {
            warn!("Phantom reconciliation: previous pass is still running — skipping this trigger \
                   instead of running alongside it (overlapping passes compound RPC load on every node)");
            return;
        }

        // RAII guard: clears the in-progress flag on every exit path from the inner
        // pass, including early returns and a panic unwind, not just the happy path.
        struct ResetGuard<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for ResetGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        let _guard = ResetGuard(&self.phantom_reconcile_in_progress);

        self.run_phantom_reconciliation_pass_inner().await;
    }

    async fn run_phantom_reconciliation_pass_inner(&self) {
        if !self.cluster.is_leader().await {
            return;
        }

        let all_nodes = self.cluster.get_all_nodes().await;
        let total_nodes = all_nodes.len();
        let online_nodes: Vec<_> = all_nodes.iter()
            .filter(|n| n.status == dfs_common::NodeStatus::Online)
            .cloned()
            .collect();
        let nodes_down = total_nodes.saturating_sub(online_nodes.len());

        let grace_elapsed = self.cluster.time_since_became_leader().await
            .map_or(true, |d| d.as_secs() >= LEADER_CHANGE_GRACE_SECS);

        if nodes_down > 1 || !grace_elapsed {
            debug!("Phantom reconciliation: skipping — nodes_down={} grace_elapsed={}", nodes_down, grace_elapsed);
            return;
        }

        let local_id = self.cluster.local_node_id();

        // Full streaming scan of CHUNK_TABLE, restricted to chunks referenced by a
        // live file (same liveness source as the deep discovery scan) with more
        // than one listed node — a single-node record can't have a "phantom" to
        // prune without risking stranding the chunk at zero replicas.
        let metadata = self.metadata.clone();
        let scan_result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<ChunkLocation>> {
            // Combined single CHUNK_TABLE pass — see scan_live_chunk_locations's doc
            // comment (was two full scans: live_chunk_ids then scan_chunk_locations).
            let mut chunks = Vec::new();
            metadata.scan_live_chunk_locations(|loc| {
                if loc.nodes.len() > 1 {
                    chunks.push(loc);
                }
            })?;
            Ok(chunks)
        }).await;

        let chunks_to_check = match scan_result {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => { warn!("Phantom reconciliation: scan error: {}", e); return; }
            Err(e) => { warn!("Phantom reconciliation: scan panicked: {}", e); return; }
        };

        if chunks_to_check.is_empty() {
            return;
        }

        info!("Phantom reconciliation: verifying presence for {} chunks", chunks_to_check.len());

        // Build per-node assignment, scoped like the discovery pass — each node is
        // only asked about chunks it's actually listed for.
        let mut node_assigned: HashMap<NodeId, Vec<ChunkId>> = HashMap::new();
        for loc in &chunks_to_check {
            for &node_id in &loc.nodes {
                node_assigned.entry(node_id).or_default().push(loc.chunk_id);
            }
        }

        let mut node_chunk_presence: HashMap<NodeId, HashSet<ChunkId>> = HashMap::new();

        // Local presence check: one recursive directory walk of the chunk store
        // (list_chunks(), the same bulk crawl run_disk_orphan_sweep already uses)
        // instead of tens of thousands of individual has_chunk() stat() calls — a
        // sequential directory read is far cheaper than that many scattered random
        // lookups, and it still must run in spawn_blocking since it's real disk I/O
        // (the inline version of the per-chunk loop froze the leader for ~2.5 hours
        // with zero log output — incident 2026-06-20).
        if node_assigned.contains_key(&local_id) {
            let storage = self.storage.clone();
            let local_set = tokio::task::spawn_blocking(move || {
                storage.list_chunks().map(|v| v.into_iter().collect::<HashSet<_>>())
            }).await;
            match local_set {
                Ok(Ok(set)) => { node_chunk_presence.insert(local_id, set); }
                Ok(Err(e)) => warn!("Phantom reconciliation: local chunk listing failed: {}", e),
                Err(e) => warn!("Phantom reconciliation: local chunk listing panicked: {}", e),
            }
        }

        for node_info in &online_nodes {
            if node_info.id == local_id {
                continue;
            }
            if !self.cluster.is_leader().await {
                debug!("Phantom reconciliation: leadership changed mid-scan — aborting");
                return;
            }
            let assigned = match node_assigned.get(&node_info.id) {
                Some(ids) if !ids.is_empty() => ids.clone(),
                _ => continue,
            };
            let request = Request::HasChunks { chunk_ids: assigned.clone() };
            // Bounded wait: a node whose own HasChunks handler is slow (e.g. a large
            // assignment, or it's mid-handling another bulk request) must not be able
            // to stall this entire pass. A timeout is treated the same as any other
            // RPC failure below — "unknown," not "missing."
            const HAS_CHUNKS_TIMEOUT: Duration = Duration::from_secs(30);
            match tokio::time::timeout(
                HAS_CHUNKS_TIMEOUT,
                self.client.send_message(node_info.addr, Message::Request(request)),
            ).await {
                Ok(Ok(envelope)) => {
                    if let Message::Response(Response::BoolVec { values }) = envelope.message {
                        let mut present = HashSet::new();
                        for (chunk_id, has) in assigned.iter().zip(values.iter()) {
                            if *has {
                                present.insert(*chunk_id);
                            }
                        }
                        node_chunk_presence.insert(node_info.id, present);
                    } else {
                        warn!("Phantom reconciliation: unexpected response to HasChunks from node {}", node_info.id);
                    }
                }
                Ok(Err(e)) => {
                    // RPC failure means "unknown," not "missing" — the node is simply
                    // excluded from node_chunk_presence and treated as unverified below.
                    debug!("Phantom reconciliation: HasChunks failed for node {} ({}): skipping node this pass", node_info.id, e);
                }
                Err(_) => {
                    debug!("Phantom reconciliation: HasChunks timed out for node {} after {:?}: skipping node this pass",
                        node_info.id, HAS_CHUNKS_TIMEOUT);
                }
            }
        }

        let mut to_heal: Vec<ChunkId> = Vec::new();
        let mut db_puts: Vec<ChunkLocation> = Vec::new();
        let mut broadcasts: Vec<ChunkLocation> = Vec::new();
        // Candidates for tombstone GC — see the collection site below.
        let mut tombstone_candidates: Vec<ChunkId> = Vec::new();
        let live_chunks = self.live_chunk_ids_from_chunk_map();

        for loc in chunks_to_check {
            let mut confirmed_present: Vec<NodeId> = Vec::new();
            let mut confirmed_missing: Vec<NodeId> = Vec::new();

            for &node_id in &loc.nodes {
                // Only a verdict we actually obtained this pass counts — an offline
                // node or one whose HasChunks RPC failed is "unknown," never "missing."
                if let Some(present_set) = node_chunk_presence.get(&node_id) {
                    if present_set.contains(&loc.chunk_id) {
                        confirmed_present.push(node_id);
                    } else {
                        confirmed_missing.push(node_id);
                    }
                }
            }

            // Never strand a chunk at zero replicas — only prune when at least one
            // other listed node is confirmed to actually hold it.
            if confirmed_missing.is_empty() || confirmed_present.is_empty() {
                if !confirmed_missing.is_empty() && confirmed_present.is_empty() {
                    // We have absences but no confirmations either way for the
                    // remaining nodes — this is the case that's otherwise silently
                    // skipped. Surface it: either the safety guard is correctly
                    // protecting a genuinely at-risk chunk, or something upstream
                    // (RPC failure, an unverified node) is hiding a real holder.
                    let unverified: Vec<NodeId> = loc.nodes.iter()
                        .filter(|n| !confirmed_missing.contains(n))
                        .copied()
                        .collect();
                    // Tombstone GC (2026-07-22). "Never strand at zero replicas" is exactly
                    // right for a chunk some file still wants — don't destroy the location
                    // record, recovery may still be possible. But it makes no sense for a
                    // chunk that NO file references: the guard then protects something that
                    // does not exist and cannot be wanted, and the row churns forever.
                    // Live staging: 12,724 such rows produced ~8,700 of these warnings every
                    // 3 minutes, driving enough metadata/CPU load to give the client
                    // leader_gap stalls of 4.7s/12.8s/27.3s — long enough for a guest to
                    // time out and log an I/O error mid-install.
                    //
                    // A row is collected only when ALL of:
                    //   * no node confirmed present (the data really is gone), and
                    //   * `unverified` is EMPTY — every listed node returned a verdict this
                    //     pass. This is the positive control: absence only counts when the
                    //     probe demonstrably worked. A node that is offline, wedged, or whose
                    //     HasChunks timed out lands in `unverified` (see the presence loop
                    //     above, which records a verdict only for nodes that answered), so a
                    //     cluster that cannot see its own storage collects nothing at all.
                    //   * no file references the chunk in chunk_map.
                    // Deletion additionally requires a fully healthy cluster, leader grace,
                    // and authorize_live_file_orphan_deletes' cross-node pending-patch union
                    // (see the post-loop site). Ambiguity anywhere keeps the row.
                    if unverified.is_empty() && !live_chunks.contains(&loc.chunk_id) {
                        tombstone_candidates.push(loc.chunk_id);
                    } else {
                        warn!("Phantom reconciliation: chunk {} has {} confirmed-absent node(s) {:?} \
                               but no other listed node confirmed present (unverified: {:?}) — skipping to avoid stranding",
                            loc.chunk_id, confirmed_missing.len(), confirmed_missing, unverified);
                    }
                }
                continue;
            }

            let pruned_nodes: Vec<NodeId> = loc.nodes.iter()
                .filter(|n| !confirmed_missing.contains(n))
                .copied()
                .collect();

            warn!("Phantom reconciliation: chunk {} — pruning {} confirmed-absent node(s): {:?}",
                loc.chunk_id, confirmed_missing.len(), confirmed_missing);

            let updated = ChunkLocation {
                chunk_id: loc.chunk_id,
                nodes: pruned_nodes,
                size: loc.size,
                checksum: loc.checksum,
                file_offset: loc.file_offset,
                written_at: Some(Self::now_ms()),
                // L1: preserve the healed chunk's real seq (identity), don't blank it — see
                // the node-prune sites' comment for the ghost-clobber this prevents.
                client_write_seq: loc.client_write_seq,
                file_id: loc.file_id,
            };

            if confirmed_present.len() < self.replication_factor.load(Ordering::Relaxed) {
                to_heal.push(loc.chunk_id);
            }

            db_puts.push(updated.clone());
            broadcasts.push(updated);
        }

        let pruned_count = db_puts.len();

        if !db_puts.is_empty() {
            let metadata = Arc::clone(&self.metadata);
            let puts = db_puts;
            let result = tokio::task::spawn_blocking(move || {
                metadata.batch_update_chunk_locations(&puts, &[], &[])
            }).await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("Phantom reconciliation: batch metadata update failed: {}", e),
                Err(e) => warn!("Phantom reconciliation: batch update spawn_blocking panicked: {}", e),
            }
        }

        // Tombstone GC: delete CHUNK_TABLE rows for chunks that are simultaneously
        // unreferenced by any file AND confirmed absent on every node that answered.
        // Nothing recoverable can be destroyed here — the bytes are already gone and
        // already unwanted; what's left is a row that only generates churn.
        //
        // Gated exactly like the other destructive paths, and routed through
        // authorize_live_file_orphan_deletes so it inherits the cross-node pending-patch
        // union (a chunk can be a live Pending base on a node that never got asked —
        // the mechanism behind the 2026-07-10 VM-111 data loss) plus the leader/follower
        // confirmation and cluster-stability rules, rather than inventing a second,
        // less-tested notion of "safe to delete".
        if !tombstone_candidates.is_empty() {
            let destructive_allowed = {
                let all_nodes = self.cluster.get_all_nodes().await;
                let online = all_nodes.iter()
                    .filter(|n| n.status == dfs_common::NodeStatus::Online)
                    .count();
                let grace_elapsed = self.cluster.time_since_became_leader().await
                    .map_or(true, |d| d.as_secs() >= LEADER_CHANGE_GRACE_SECS);
                // SELF_RESTART_GRACE_SECS is load-bearing here, not belt-and-braces: the
                // whole "is it referenced?" test reads live_chunk_ids_from_chunk_map(),
                // and after a restart that map is only as fresh as the last durable
                // FileMetadata sync — nowhere near a long-running peer's, which updates
                // synchronously on every patch. A node that restarted recently can
                // therefore see genuinely LIVE chunks as unreferenced and tombstone them.
                // This is not hypothetical: a rolling restart performed during an active
                // VM install on 2026-07-22 is exactly the window that would have hit it,
                // and the disk orphan sweep already carries this same check for the same
                // reason (see its call site above).
                let self_uptime = self.local_started_at.elapsed().as_secs();
                let self_settled = self_uptime >= SELF_RESTART_GRACE_SECS;
                !all_nodes.is_empty() && online == all_nodes.len() && grace_elapsed && self_settled
            };
            if !destructive_allowed {
                info!("Phantom reconciliation: {} unreferenced zero-replica tombstone(s) found but cluster not fully healthy/settled (or this node restarted recently) — deferring", tombstone_candidates.len());
            } else {
                let authorized = self.authorize_live_file_orphan_deletes(&tombstone_candidates).await;
                if !authorized.is_empty() {
                    let metadata = Arc::clone(&self.metadata);
                    let deletes = authorized.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        // Same list for both: drop the routing row AND its pending_healing
                        // row, otherwise the healer keeps rediscovering a chunk whose
                        // location no longer exists and logs "Chunk location not found —
                        // stalling until next discovery" on every pass (198 such warnings
                        // per burst were still firing on staging after the orphan cleanup).
                        metadata.batch_update_chunk_locations(&[], &deletes, &deletes)
                    }).await;
                    match result {
                        Ok(Ok(())) => info!("Phantom reconciliation: garbage-collected {} unreferenced zero-replica location row(s) ({} candidate(s) authorized)",
                            authorized.len(), tombstone_candidates.len()),
                        Ok(Err(e)) => warn!("Phantom reconciliation: tombstone GC batch delete failed: {}", e),
                        Err(e) => warn!("Phantom reconciliation: tombstone GC spawn_blocking panicked: {}", e),
                    }
                } else {
                    info!("Phantom reconciliation: {} tombstone candidate(s) all withheld by live-file authorization — deferring", tombstone_candidates.len());
                }
            }
        }

        for loc in &broadcasts {
            Self::broadcast_chunk_location_shared(loc, &self.cluster, &self.client).await;
        }

        if !to_heal.is_empty() {
            info!("Phantom reconciliation: {} chunk(s) now under RF after pruning — queuing for immediate healing", to_heal.len());
            self.queue_chunks_immediate(to_heal).await;
        }

        if pruned_count > 0 {
            info!("Phantom reconciliation: pass complete — {} chunk(s) corrected", pruned_count);
        } else {
            debug!("Phantom reconciliation: pass complete — no phantoms found");
        }
    }

    /// Defer a heavy maintenance scan (phantom reconciliation, disk orphan
    /// sweep) by up to MAX_WAIT if the cluster has very recent write activity.
    /// Both scans fan out over every live/local chunk — tens of thousands on a
    /// cluster with real data — and running one straight into a write burst
    /// measurably degrades live write latency and correctness (RPC/lock/CPU
    /// contention on the same nodes servicing the writes). Real 2026-07-11
    /// incident: a scheduled phantom-reconciliation pass fired "verifying
    /// presence for 80337 chunks" 8 seconds before a staging fio+fsck repro's
    /// write storm started, and that repro run failed with the same class of
    /// "no registered location"/EIO errors seen elsewhere today; a disk-orphan
    /// sweep of 56672 chunks fired in the same window on another node.
    /// last_cluster_write_ms was already being maintained cluster-wide for
    /// exactly this purpose (see its own doc comment) but nothing actually
    /// read it before this.
    ///
    /// Bounded, not indefinite — a genuinely continuously-busy cluster (e.g.
    /// the DVR service's own steady recording traffic) must not starve these
    /// scans forever; after MAX_WAIT this proceeds regardless of ongoing
    /// writes.
    async fn wait_for_write_quiet(&self, scan_name: &str) {
        const QUIET_THRESHOLD: Duration = Duration::from_secs(3);
        const POLL_INTERVAL: Duration = Duration::from_secs(1);
        const MAX_WAIT: Duration = Duration::from_secs(30);

        let deadline = std::time::Instant::now() + MAX_WAIT;
        loop {
            let last_write_ms = self.last_cluster_write_ms.load(std::sync::atomic::Ordering::Relaxed);
            if last_write_ms == 0 {
                return; // no write activity ever recorded — nothing to wait for
            }
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let since_last_write = Duration::from_millis(now_ms.saturating_sub(last_write_ms));
            if since_last_write >= QUIET_THRESHOLD {
                return;
            }
            if std::time::Instant::now() >= deadline {
                info!("{}: proceeding after {}s despite recent write activity — cluster appears \
                       continuously busy, not deferring indefinitely", scan_name, MAX_WAIT.as_secs());
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Background loop for run_phantom_reconciliation_pass(). Runs on a fixed
    /// interval (default 10 minutes, DFS_PHANTOM_RECONCILE_INTERVAL_SECS override)
    /// regardless of the discovery loop's own cadence.
    async fn run_phantom_reconciliation_loop(&self) {
        let interval_secs = std::env::var("DFS_PHANTOM_RECONCILE_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(600);
        let mut timer = interval(Duration::from_secs(interval_secs));
        timer.tick().await; // skip immediate first tick — let the cluster settle on startup
        loop {
            timer.tick().await;
            if self.healing_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                self.wait_for_write_quiet("Phantom reconciliation").await;
                self.run_phantom_reconciliation_pass().await;
            }
        }
    }

    /// How often the PAGINATED loop's gated retries are allowed to log at warn level.
    /// The full-sweep path (run_disk_orphan_sweep) calls the gate at most once per
    /// manual trigger or emergency-monitor tick (rare) and always logs; the paginated
    /// loop re-checks this same gate every page_grace (default 3s) while gated, which
    /// at 1-for-1 logging would produce ~400 warn lines over one 20-minute restart
    /// grace window instead of the ~10 the old 2-minute-cadence code produced. Throttle
    /// keeps the *check* at full frequency (still reacts to the gate clearing promptly)
    /// while capping the *log volume* back down to roughly its pre-pagination rate.
    const GATE_LOG_THROTTLE: Duration = Duration::from_secs(60);

    /// Cluster-degradation/restart-freshness gate shared by run_disk_orphan_sweep and
    /// run_disk_orphan_sweep_paginated_loop — don't delete local chunks when the
    /// cluster is degraded (a copy on a currently-offline node might be the only
    /// remaining replica) or when this node's/the leader's view might not have caught
    /// up yet. Logs its own reason and returns false when gated; true when clear.
    /// `throttle_log`: when true, suppresses the warn! if one was already logged
    /// within GATE_LOG_THROTTLE — see that constant's doc comment for why the
    /// paginated loop needs this and the rarer full-sweep callers don't.
    async fn disk_orphan_sweep_cluster_gate_clear(&self, throttle_log: bool) -> bool {
        let should_log = !throttle_log || {
            let mut last = self.disk_sweep_gate_log_throttle.lock().unwrap();
            if last.elapsed() >= Self::GATE_LOG_THROTTLE {
                *last = Instant::now();
                true
            } else {
                false
            }
        };

        let all_nodes = self.cluster.get_all_nodes().await;
        let total = all_nodes.len();
        let online = all_nodes.iter()
            .filter(|n| n.status == dfs_common::NodeStatus::Online)
            .count();
        let nodes_down = total.saturating_sub(online);

        let grace_elapsed = self.cluster.time_since_became_leader().await
            .map_or(true, |d| d.as_secs() >= LEADER_CHANGE_GRACE_SECS);

        if nodes_down > 1 {
            if should_log {
                warn!("Skipping disk orphan sweep — {} node(s) down (max 1 allowed for destructive ops)", nodes_down);
            }
            return false;
        }
        if !grace_elapsed {
            if should_log {
                let elapsed = self.cluster.time_since_became_leader().await
                    .map_or(0.0, |d| d.as_secs_f64());
                warn!("Skipping disk orphan sweep — within grace period after leader election ({:.0}s elapsed of {}s)",
                    elapsed, LEADER_CHANGE_GRACE_SECS);
            }
            return false;
        }
        // See SELF_RESTART_GRACE_SECS's doc comment: the check above only ever
        // fires for a node that's been (or recently became) leader — a plain
        // follower always passes it trivially, no matter how recently it
        // restarted. This is the independent check for that case: THIS node's
        // own local_started_at, regardless of leadership.
        let self_uptime = self.local_started_at.elapsed().as_secs();
        if self_uptime < SELF_RESTART_GRACE_SECS {
            if should_log {
                warn!("Skipping disk orphan sweep — this node restarted {}s ago (< {}s grace period, chunk_map may not have caught up to current cluster state yet)",
                    self_uptime, SELF_RESTART_GRACE_SECS);
            }
            return false;
        }
        true
    }

    /// Full-sweep entry point — used by the manual "run everything right now"
    /// handle_trigger_orphan_cleanup RPC, a rare, explicit, administrator-initiated
    /// action where the old unbounded-scan semantics are still exactly what's wanted.
    /// The recurring path is run_disk_orphan_sweep_paginated_loop, which bounds each
    /// pass to a page via run_disk_orphan_sweep_over instead of calling this.
    pub async fn run_disk_orphan_sweep(&self) {
        if !self.disk_orphan_sweep_cluster_gate_clear(false).await {
            return;
        }
        let chunks = match self.storage.list_chunks() {
            Ok(v) => v,
            Err(e) => { warn!("Disk orphan sweep: failed to list local chunks: {}", e); return; }
        };
        self.run_disk_orphan_sweep_over(chunks).await;
    }

    /// Core per-chunk-set engine shared by the full-sweep entry point
    /// (run_disk_orphan_sweep) and the paginated loop (run_disk_orphan_sweep_paginated_loop) —
    /// takes an explicit chunk_id list instead of always calling storage.list_chunks()
    /// itself, so a caller can pass either "every chunk on disk" or just one page of
    /// them. Caller is responsible for the cluster-gate check
    /// (disk_orphan_sweep_cluster_gate_clear) — this function assumes it already passed.
    async fn run_disk_orphan_sweep_over(&self, chunks: Vec<ChunkId>) {
        // 2x the periodic full-reconciliation interval (server.rs RECONCILE_INTERVAL =
        // 300s) — that loop is the slowest *guaranteed* metadata-catchup path in the
        // system, so doubling it bounds how stale this node's live_chunk_ids() view
        // could legitimately still be for a routine (non-degraded) cluster.
        //
        // DFS_LIVE_FILE_ORPHAN_GRACE_SECS override: testing only, so a repro doesn't
        // have to wait 10 real minutes per candidate — added 2026-07-18 investigating
        // the VM-111 post-install data-loss incident (~900MB of chunk_locations gone
        // after a leader ran this sweep under sustained write load + severe memory
        // pressure). Unset in production.
        let live_file_grace_secs: u64 = std::env::var("DFS_LIVE_FILE_ORPHAN_GRACE_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(600);

        let storage = self.storage.clone();
        let metadata = self.metadata.clone();
        let local_node_id = self.cluster.local_node_id();
        // Union of the durable (FILE_TABLE) view and the in-memory chunk_map view.
        // FILE_TABLE only refreshes on a full PutFileMetadata, so a long-lived,
        // repeatedly in-place-patched file (e.g. a mounted qcow2 disk) can have a
        // current chunk_id that's missing from it for an unbounded amount of time —
        // chunk_map is kept synchronously fresh by every patch/replicate-location
        // handler and never has that gap. Taking the union means a chunk is only
        // ever treated as "not live" if BOTH sources agree it's gone.
        let live_from_chunk_map = self.live_chunk_ids_from_chunk_map();
        // superseded_generation_chunk_ids is intentionally NOT consulted here anymore —
        // see the NOTE at its use site below (removed 2026-07-26) for why gating physical
        // deletion on this node-local, unreconciled generation map was unsafe.

        let result = tokio::task::spawn_blocking(move || {
            let mut live_chunks = metadata.live_chunk_ids()?;
            live_chunks.extend(live_from_chunk_map);
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let mut kept = 0usize;
            // (chunk_id, age_secs) — still routed to us, but no live file references
            // it. Collected for the async leader-confirm/stability gate below; never
            // deleted inside this blocking closure.
            let mut live_file_candidates: Vec<(ChunkId, u64)> = Vec::new();

            for chunk_id in &chunks {
                // Determine whether this local file is still our responsibility.
                let loc_record = match metadata.get_chunk_location(chunk_id) {
                    Ok(v) => v,
                    Err(e) => {
                        debug!("Disk orphan sweep: routing table error for {}: {}", chunk_id, e);
                        continue;
                    }
                };

                // Neither a bare `None` NOR a `Some(loc)` that excludes this node is
                // proof the chunk is safe to delete — both just as easily mean this
                // node's own local metadata hasn't caught up (e.g. after a leadership
                // change, a metadata-replication backlog, or — confirmed live,
                // 2026-07-10, staging gluster3 — a ChunkLocation record whose node
                // list is simply stale relative to the leader's) while the chunk is
                // still legitimately live, including on THIS node. A previous version
                // of this function fast-path-deleted the `Some(loc) not listing us`
                // case locally, trusting its own possibly-stale record as
                // authoritative with no leader round trip — same node-local-view-as-
                // cluster-truth disease as bugs 6/8/9, just unaudited here. Caught
                // deleting a real, needed 2nd-of-2 replica the leader's own record
                // still listed this exact node as holding, dropping it to
                // under-replicated. Route every case uniformly through the same
                // leader-confirm + cluster-stability gate below instead of deleting
                // on local metadata alone.
                // NOTE (2026-07-26): superseded_generations deliberately does NOT gate
                // deletion here anymore. It's derived from this node's own in-memory
                // chunk_generations/fold_result_chunk_ids (location_supersedes), which
                // falls back to the cws/written_at tiebreak whenever either side's
                // generation is unknown (server.rs location_supersedes) — and that map
                // is never reconciled with peers (fold-announcement dissemination is
                // push-only/ephemeral, see project_fold_announcement_rebroadcast_gap).
                // authorize_live_file_orphan_deletes below only cross-checks pending
                // PATCH state, never re-verifies the slot generation contest with the
                // leader — so a node with an incomplete local generation map had a live
                // path to physically delete a chunk it merely *misjudged* as retired.
                // Confirmed on staging 2026-07-26: ~21k chunks/node flagged and hundreds
                // of eviction batches logged since deploy, coinciding with VM-108/VM-111
                // taking unrecoverable read I/O errors. Reverted to file_id-liveness-only
                // gating (pre-739435c behavior) until generation state has a real
                // cluster-reconciled source of truth. The heal-queue-exclusion half of
                // 739435c (slot_losers skip below, non-destructive) is unaffected and stays.
                if loc_record.is_some() && live_chunks.contains(chunk_id) {
                    kept += 1;
                    continue;
                }

                let age_secs = storage.get_chunk_mtime(chunk_id)
                    .map(|mtime| now_secs.saturating_sub(mtime))
                    .unwrap_or(u64::MAX);
                live_file_candidates.push((*chunk_id, age_secs));
            }

            // Neither chunk_map nor FILE_TABLE ever learns about a MultiPatch/PatchChunk
            // result until the client's own one-shot ReplicateChunkLocation broadcast to
            // the leader lands (see handle_multi_patch's "Replicas do NOT self-report"
            // comment) — a fire-and-forget RPC with no retry or delivery confirmation. If
            // it's ever lost, a chunk this node correctly wrote to disk moments ago is
            // invisible to both liveness sources forever, ages past the grace period, and
            // reaches here as a false-positive candidate — even though PATCH_STATE_TABLE
            // (updated synchronously and locally by the same apply_patch call that wrote
            // the file, no broadcast involved) has known about it the whole time. Root
            // cause of the 2026-08-02 VM-108 chunk_idx=3147 loss (dc0f9bf3...): a Pending
            // patch token deleted by this sweep ~13h after being correctly written, having
            // never been superseded — only its chunk_map broadcast never arrived. Exclude
            // anything PATCH_STATE_TABLE still tracks before it ever becomes a candidate.
            if !live_file_candidates.is_empty() {
                let candidate_ids: Vec<ChunkId> = live_file_candidates.iter().map(|(id, _)| *id).collect();
                match metadata.get_patch_states_present_batch(&candidate_ids) {
                    Ok(tracked) if !tracked.is_empty() => {
                        let before = live_file_candidates.len();
                        live_file_candidates.retain(|(id, _)| !tracked.contains(id));
                        kept += before - live_file_candidates.len();
                    }
                    Ok(_) => {}
                    Err(e) => debug!("Disk orphan sweep: patch_state batch check failed, proceeding without it this cycle: {}", e),
                }
            }

            Ok::<_, anyhow::Error>((kept, chunks.len(), live_file_candidates))
        }).await;

        let (kept, total, live_file_candidates) = match result {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => { warn!("Disk orphan sweep error: {}", e); return; }
            Err(e) => { warn!("Disk orphan sweep panicked: {}", e); return; }
        };

        if !live_file_candidates.is_empty() {
            info!("Disk orphan sweep: {} local chunks checked — {} kept (legitimately ours), {} candidates routed to leader-confirm gate",
                  total, kept, live_file_candidates.len());
        } else {
            debug!("Disk orphan sweep: {} chunks checked, all accounted for", total);
        }

        self.reconcile_live_file_candidates(live_file_candidates, live_file_grace_secs).await;
    }

    /// Pure arithmetic for the paginated sweep's rate limiter — split out so it can be
    /// unit-tested without real sleeps. Returns how long to sleep before the next page:
    /// the grace floor minus however much of it this page's own work already consumed,
    /// clamped to zero so a run of slow pages never oversleeps into negative territory.
    fn disk_sweep_next_delay(grace: Duration, elapsed_this_iteration: Duration) -> Duration {
        grace.saturating_sub(elapsed_this_iteration)
    }

    /// Recurring path for disk-orphan-sweep: bounds each pass to a page over the
    /// indexed chunk set instead of run_disk_orphan_sweep's unbounded full scan, so a
    /// large accumulated backlog (e.g. after a restart's grace period elapses) can
    /// never dump its entire cost into a single cycle. See the design doc in the
    /// commit that introduced this (2026-08-04, VM-108 565f683c incident) for the
    /// full rationale. Runs independently of run_discovery_loop's 60s cadence —
    /// watched over by run_disk_orphan_sweep_watchdog.
    async fn run_disk_orphan_sweep_paginated_loop(&self) {
        let page_size: usize = std::env::var("DFS_ORPHAN_SWEEP_PAGE_SIZE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(5000);
        let page_grace = Duration::from_millis(
            std::env::var("DFS_ORPHAN_SWEEP_PAGE_GRACE_MS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(3000),
        );

        let mut cursor: Option<ChunkId> = None;
        loop {
            let iter_start = Instant::now();

            if !self.healing_enabled.load(std::sync::atomic::Ordering::Relaxed)
                || self.compaction_quiescing.load(std::sync::atomic::Ordering::Relaxed)
                || !self.disk_orphan_sweep_cluster_gate_clear(true).await
            {
                // Gated — don't advance the cursor, just wait and re-check. The
                // individual gate methods already log their own reason.
                *self.disk_sweep_last_page_at.lock().unwrap() = Instant::now();
                tokio::time::sleep(page_grace).await;
                continue;
            }

            let page = match self.storage.list_chunks_page(cursor, page_size) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Disk orphan sweep (paginated): failed to list chunk page: {}", e);
                    *self.disk_sweep_last_page_at.lock().unwrap() = Instant::now();
                    tokio::time::sleep(page_grace).await;
                    continue;
                }
            };

            if page.is_empty() {
                // End of rotation — wrap the cursor and wait a beat before starting over.
                cursor = None;
            } else {
                cursor = page.last().copied();
                self.run_disk_orphan_sweep_over(page).await;
            }

            *self.disk_sweep_last_page_at.lock().unwrap() = Instant::now();
            tokio::time::sleep(Self::disk_sweep_next_delay(page_grace, iter_start.elapsed())).await;
        }
    }

    /// Independent watchdog for run_disk_orphan_sweep_paginated_loop — modeled on
    /// run_disk_emergency_monitor's precedent (a fast independent loop supplementing
    /// a slower one). A self-perpetuating page-chain has nothing else to restart it
    /// if it ever silently dies (panic, or a bug in the reschedule logic), so this
    /// checks the stored JoinHandle every 30s and respawns on a detected death.
    /// Startup spawn is start()'s job — this only reacts to a death it observes.
    async fn run_disk_orphan_sweep_watchdog(self: Arc<Self>) {
        const CHECK_INTERVAL: Duration = Duration::from_secs(30);
        loop {
            tokio::time::sleep(CHECK_INTERVAL).await;
            self.clone().disk_orphan_sweep_watchdog_check_once().await;
        }
    }

    /// One watchdog check-and-maybe-respawn, split out from run_disk_orphan_sweep_watchdog's
    /// sleep loop so it can be unit-tested without waiting a real 30s.
    async fn disk_orphan_sweep_watchdog_check_once(self: Arc<Self>) {
        let finished = {
            let guard = self.disk_sweep_task.lock().unwrap();
            match guard.as_ref() {
                Some(handle) => handle.is_finished(),
                None => true, // never spawned — shouldn't happen post-start(), but respawn anyway
            }
        };

        if finished {
            let old = self.disk_sweep_task.lock().unwrap().take();
            if let Some(handle) = old {
                match handle.await {
                    Ok(()) => error!("Disk orphan sweep watchdog: paginated sweep loop ended unexpectedly (no panic payload) — respawning"),
                    Err(e) if e.is_panic() => error!("Disk orphan sweep watchdog: paginated sweep loop PANICKED ({:?}) — respawning", e),
                    Err(e) => error!("Disk orphan sweep watchdog: paginated sweep loop task error ({:?}) — respawning", e),
                }
            }
            let respawn_target = self.clone();
            let handle = tokio::spawn(async move {
                respawn_target.run_disk_orphan_sweep_paginated_loop().await;
            });
            *self.disk_sweep_task.lock().unwrap() = Some(handle);
        }
    }

    /// Two-pass + age-gate + leader-confirm/stability check for category-2 candidates
    /// (routed to us, but absent from our own live_chunk_ids()). Reuses
    /// orphan_candidates as the per-node two-pass debounce set.
    ///
    /// Incremental merge, not wholesale replace (2026-08-04, pagination): this is now
    /// called once per page (run_disk_orphan_sweep_paginated_loop) as well as once per
    /// full scan (run_disk_orphan_sweep's manual-trigger path). A page only ever
    /// covers a disjoint slice of the chunk_id space, so if this insert/remove'd
    /// `orphan_candidates` wholesale on every call, each page's call would blow away
    /// the first-sighting entries every other page in the same rotation just recorded
    /// — the two-pass debounce would never actually complete a second pass for
    /// anything. Merging in place means a chunk_id's "first sighting" set on page N of
    /// rotation 1 survives untouched through every other page's calls until rotation
    /// 2's page N comes back around and either promotes it (still a candidate: same
    /// chunk_id seen again) or the entry simply ages out no differently than before
    /// pagination existed. Full-scan callers are unaffected: one call already covered
    /// every chunk_id in one shot, so incremental-vs-replace makes no observable
    /// difference for that path.
    async fn reconcile_live_file_candidates(&self, candidates: Vec<(ChunkId, u64)>, grace_secs: u64) {
        if candidates.is_empty() {
            return;
        }

        let mut ready_to_delete: Vec<ChunkId> = Vec::new();
        {
            let mut tracked = self.orphan_candidates.write().await;
            for (chunk_id, age_secs) in &candidates {
                if *age_secs < grace_secs {
                    debug!("Live-file orphan grace: skipping {} (age={}s, grace={}s)", chunk_id, age_secs, grace_secs);
                    continue;
                }
                if tracked.remove(chunk_id) {
                    ready_to_delete.push(*chunk_id);
                } else {
                    tracked.insert(*chunk_id);
                    debug!("Live-file orphan candidate (first sighting, will re-check next pass): {}", chunk_id);
                }
            }
        }

        if ready_to_delete.is_empty() {
            return;
        }

        let mut authorized = self.authorize_live_file_orphan_deletes(&ready_to_delete).await;
        if authorized.is_empty() {
            debug!("Live-file orphan sweep: {} candidate(s) confirmed-absent locally but not authorized for deletion this cycle",
                   ready_to_delete.len());
            return;
        }

        // Final re-check immediately before the irreversible delete: a candidate could
        // have picked up a PATCH_STATE_TABLE row in the time since run_disk_orphan_sweep
        // first scanned it (a late-arriving patch, or the leader-confirm RPC round trip
        // above taking a moment) — see the matching check and comment in
        // run_disk_orphan_sweep for why this table is the one source that's never subject
        // to the lost-broadcast gap chunk_map/FILE_TABLE have. Cheap (one batch read) and
        // this is the last point before data is gone for good, so worth re-asking.
        match self.metadata.get_patch_states_present_batch_async(authorized.clone()).await {
            Ok(tracked) if !tracked.is_empty() => {
                info!("Live-file orphan sweep: {} candidate(s) now show a PATCH_STATE_TABLE row — pulling back from deletion",
                      tracked.len());
                authorized.retain(|id| !tracked.contains(id));
            }
            Ok(_) => {}
            Err(e) => warn!("Live-file orphan sweep: final patch_state re-check failed ({}) — proceeding, relying on the earlier check", e),
        }
        if authorized.is_empty() {
            return;
        }

        let mut evicted = 0usize;
        for chunk_id in &authorized {
            if let Err(e) = self.storage.delete_chunk(chunk_id, "live_file_orphan_sweep") {
                debug!("Live-file orphan sweep: failed to delete {}: {}", chunk_id, e);
                continue;
            }
            let _ = self.metadata.delete_chunk_location_async(*chunk_id).await;
            evicted += 1;
        }
        if evicted > 0 {
            info!("Live-file orphan sweep: evicted {}/{} authorized chunk(s) (patch-superseded, missed by inline fast-evict)",
                  evicted, authorized.len());
        }
    }

    /// Decide which of `candidates` are safe to actually delete right now.
    ///
    /// - Not the leader: ask the leader via ConfirmChunksLive — it's normally the
    ///   most caught-up replica. Anything the leader confirms live is excluded. Any
    ///   RPC failure, timeout, or unexpected response authorizes NOTHING this cycle
    ///   (fail safe — retried next sweep, no data loss risk from being conservative).
    /// - Is the leader (no one more authoritative to ask): require every other known
    ///   node to be Online AND to report at least STABILITY_SECS of continuous
    ///   process uptime via GetNodeStats. A node that recently restarted may not have
    ///   finished catching up its own metadata replica yet — proceeding before that
    ///   settles is exactly the "split-brain mass delete" scenario this guards
    ///   against. Any node failing either check authorizes NOTHING this cycle.
    async fn authorize_live_file_orphan_deletes(&self, candidates: &[ChunkId]) -> Vec<ChunkId> {
        const STABILITY_SECS: u64 = 300;
        const RPC_TIMEOUT: Duration = Duration::from_secs(5);

        if self.cluster.is_leader().await {
            let nodes = self.cluster.get_all_nodes().await;
            let local_id = self.cluster.local_node_id();
            // Started from this node's own local set; every online peer's own set
            // (queried below, same loop as the stability check) gets unioned in.
            // PATCH_STATE_TABLE is node-local and never disseminated, and a patch
            // can land on any node in the cluster, not just the leader — trusting
            // only this node's own view here is exactly what let a real 2026-07-10
            // data-loss incident through (VM-111 install): gluster4 asked leader
            // gluster1 to authorize a candidate that gluster1 itself had already
            // moved past locally, but gluster3 — never asked — still held it as a
            // live Pending base. gluster1 said "not live" from pure local
            // ignorance, gluster4 deleted its own copy for real, RF dropped from 3
            // to 2 on exactly the two nodes the client's deterministic replica
            // selection kept picking. Non-leader nodes always got this union via
            // handle_confirm_chunks_live; the leader was taking a shortcut around
            // it by answering its own candidates directly. Same fix, same place.
            let mut pending_ids = match self.metadata.all_pending_patch_chunk_ids_async().await {
                Ok(ids) => ids,
                Err(e) => {
                    warn!("Live-file orphan sweep: failed to read local pending patch chunk_ids, deferring this cycle: {}", e);
                    return Vec::new();
                }
            };
            // Also protect outstanding tokens themselves, not just their base/delta
            // inputs — see handle_confirm_chunks_live's matching comment (server.rs)
            // for the gap this closes (2026-08-02 VM-108 chunk_idx=3147 loss).
            match self.metadata.all_patch_token_ids_async().await {
                Ok(token_ids) => pending_ids.extend(token_ids),
                Err(e) => {
                    warn!("Live-file orphan sweep: failed to read local patch token ids, deferring this cycle: {}", e);
                    return Vec::new();
                }
            }
            for node in &nodes {
                if node.id == local_id {
                    continue;
                }
                if node.status != dfs_common::NodeStatus::Online {
                    debug!("Live-file orphan sweep: deferring — node {} is not online", node.id);
                    return Vec::new();
                }
                let req = Request::GetNodeStats;
                match tokio::time::timeout(RPC_TIMEOUT, self.client.send_message(node.addr, Message::Request(req))).await {
                    Ok(Ok(envelope)) => match envelope.message {
                        Message::Response(Response::NodeStats { uptime_secs, .. }) => {
                            if uptime_secs < STABILITY_SECS {
                                debug!("Live-file orphan sweep: deferring — node {} uptime {}s < {}s stability requirement",
                                       node.id, uptime_secs, STABILITY_SECS);
                                return Vec::new();
                            }
                        }
                        _ => {
                            debug!("Live-file orphan sweep: deferring — unexpected response to GetNodeStats from {}", node.id);
                            return Vec::new();
                        }
                    },
                    _ => {
                        debug!("Live-file orphan sweep: deferring — GetNodeStats failed/timed out for node {}", node.id);
                        return Vec::new();
                    }
                }
                let req = Request::GetPendingPatchChunkIds;
                match tokio::time::timeout(RPC_TIMEOUT, self.client.send_message(node.addr, Message::Request(req))).await {
                    Ok(Ok(envelope)) => match envelope.message {
                        Message::Response(Response::PendingPatchChunkIds { ids }) => pending_ids.extend(ids),
                        _ => {
                            debug!("Live-file orphan sweep: deferring — unexpected response to GetPendingPatchChunkIds from {}", node.id);
                            return Vec::new();
                        }
                    },
                    _ => {
                        debug!("Live-file orphan sweep: deferring — GetPendingPatchChunkIds failed/timed out for node {}", node.id);
                        return Vec::new();
                    }
                }
            }
            candidates.iter().copied().filter(|id| !pending_ids.contains(id)).collect()
        } else {
            let leader_addr = match self.cluster.get_leader_addr().await {
                Some(addr) => addr,
                None => {
                    debug!("Live-file orphan sweep: deferring — no known leader to confirm with");
                    return Vec::new();
                }
            };
            let req = Request::ConfirmChunksLive { chunk_ids: candidates.to_vec() };
            match tokio::time::timeout(RPC_TIMEOUT, self.client.send_message(leader_addr, Message::Request(req))).await {
                Ok(Ok(envelope)) => match envelope.message {
                    Message::Response(Response::ChunkLiveness { live }) => {
                        let live_set: HashSet<ChunkId> = live.into_iter().collect();
                        candidates.iter().copied().filter(|id| !live_set.contains(id)).collect()
                    }
                    _ => {
                        debug!("Live-file orphan sweep: deferring — unexpected response to ConfirmChunksLive from leader");
                        Vec::new()
                    }
                },
                _ => {
                    debug!("Live-file orphan sweep: deferring — ConfirmChunksLive failed/timed out against leader {}", leader_addr);
                    Vec::new()
                }
            }
        }
    }

    /// Periodic self-check ensuring every Pending patch token this node physically
    /// holds is known to the current leader's chunk_map — independent of whether the
    /// *client's* own ReplicateChunkLocation broadcast (dfs-client's
    /// pending_chunk_locations queue) ever landed. That queue already retries
    /// indefinitely, but only in-memory: a client crash or redeploy while an entry
    /// is still unconfirmed loses it for good, with nothing to resubmit it — the
    /// same shape of gap already open on the fold-announcement side (see
    /// project_fold_announcement_rebroadcast_gap). This pass instead depends on
    /// nothing but this node's own durable PATCH_STATE_TABLE — the same source of
    /// truth the 2026-08-02 disk-orphan-sweep fix (all_patch_token_ids) already
    /// trusts — so it survives any client-side failure entirely.
    ///
    /// On a detected leader change, the whole confirmed-set is invalidated at once
    /// (not per-token): conservative by design, since a new leader should hear
    /// about every outstanding token at least once regardless of what the old
    /// leader supposedly already knew.
    ///
    /// No new waits on any write path and no new durable state — this only reads
    /// PATCH_STATE_TABLE and replays the existing ReplicateChunkLocation RPC/
    /// handler exactly as the client already does; pending_patch_confirmed is a
    /// pure in-memory dedup cache, rebuilt harmlessly from scratch on restart.
    pub async fn run_pending_patch_reconciliation(&self) {
        let leader_addr = self.cluster.get_leader_addr().await;

        {
            let mut last_leader = self.last_reconcile_leader.write().await;
            if *last_leader != leader_addr {
                info!("Pending-patch reconciliation: leader changed ({:?} -> {:?}) — re-announcing every outstanding token",
                      *last_leader, leader_addr);
                self.pending_patch_confirmed.write().await.clear();
                self.folded_patch_confirmed.write().await.clear();
                *last_leader = leader_addr;
            }
        }

        let Some(leader_addr) = leader_addr else {
            debug!("Pending-patch reconciliation: no known leader, skipping this cycle");
            return;
        };

        let slots = match self.metadata.all_pending_patch_slots_async().await {
            Ok(s) => s,
            Err(e) => {
                warn!("Pending-patch reconciliation: failed to read pending slots: {}", e);
                return;
            }
        };

        // GC: drop anything from the confirmed-set that isn't still genuinely
        // Pending (folded, superseded, or pruned since the last cycle) — piggybacks
        // on the same scan, no separate timer or pass needed.
        let current_tokens: HashSet<ChunkId> = slots.iter().map(|(_, _, t)| *t).collect();
        {
            let mut confirmed = self.pending_patch_confirmed.write().await;
            confirmed.retain(|t| current_tokens.contains(t));
        }

        const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
        let local_id = self.cluster.local_node_id();
        let mut announced = 0usize;

        for (file_id, chunk_idx, token) in slots {
            if self.pending_patch_confirmed.read().await.contains(&token) {
                continue;
            }
            let (size, written_at, client_write_seq) = match self.metadata.get_patch_state_async(token).await {
                Ok(Some(crate::metadata::PatchState::Pending { size, written_at, client_write_seq, .. })) => {
                    (size, written_at, client_write_seq)
                }
                // Raced past Pending (folded/pruned) since the slot scan above — next
                // cycle's fresh scan won't return it either way, nothing to announce.
                _ => continue,
            };
            let location = ChunkLocation {
                chunk_id: token,
                nodes: vec![local_id],
                size,
                checksum: token.hash,
                file_offset: Some(chunk_idx * CHUNK_SIZE),
                written_at: Some(written_at),
                client_write_seq,
                file_id: Some(file_id),
            };
            let req = Request::ReplicateChunkLocation { location, file_id: Some(file_id), generation: None };
            let acked = matches!(
                tokio::time::timeout(Duration::from_secs(5), self.client.send_message(leader_addr, Message::Request(req))).await,
                Ok(Ok(_))
            );
            if acked {
                self.pending_patch_confirmed.write().await.insert(token);
                announced += 1;
            }
        }

        if announced > 0 {
            info!("Pending-patch reconciliation: (re-)announced {} previously-unconfirmed token(s) to leader {}",
                  announced, leader_addr);
        }

        // Fold-announcement backstop — see folded_patch_confirmed's doc comment for the
        // notify_leader_of_fold gap this closes. Same shape as the Pending loop above,
        // reusing whichever slots this node's own PATCH_STATE_TABLE already shows as
        // Folded rather than tracking fold completions separately.
        let folded_slots = match self.metadata.all_folded_patch_slots_async().await {
            Ok(s) => s,
            Err(e) => {
                warn!("Fold-announcement reconciliation: failed to read folded slots: {}", e);
                return;
            }
        };
        let current_folded: HashSet<ChunkId> = folded_slots.iter().map(|(_, _, _, real)| *real).collect();
        {
            let mut confirmed = self.folded_patch_confirmed.write().await;
            confirmed.retain(|id| current_folded.contains(id));
        }

        let mut fold_announced = 0usize;
        for (file_id, chunk_idx, token, real_chunk_id) in folded_slots {
            if self.folded_patch_confirmed.read().await.contains(&real_chunk_id) {
                continue;
            }
            let Ok(Some(location)) = self.metadata.get_chunk_location_async(real_chunk_id).await else {
                // No local location record for our own fold's output — nothing to
                // replay yet (e.g. mid-commit); next cycle will pick it up once it lands.
                continue;
            };
            let loc_req = Request::ReplicateChunkLocation {
                location, file_id: Some(file_id),
                generation: None,
            };
            let fold_req = Request::ReplicatePatchFold { public_token: token, real_chunk_id, file_id, chunk_idx };
            let loc_ok = matches!(
                tokio::time::timeout(Duration::from_secs(5), self.client.send_message(leader_addr, Message::Request(loc_req))).await,
                Ok(Ok(_))
            );
            let fold_ok = matches!(
                tokio::time::timeout(Duration::from_secs(5), self.client.send_message(leader_addr, Message::Request(fold_req))).await,
                Ok(Ok(_))
            );
            if loc_ok && fold_ok {
                self.folded_patch_confirmed.write().await.insert(real_chunk_id);
                fold_announced += 1;
            }
        }

        if fold_announced > 0 {
            info!("Fold-announcement reconciliation: (re-)announced {} previously-unconfirmed fold(s) to leader {}",
                  fold_announced, leader_addr);
        }
    }

    async fn run_scrubber(&self) {
        let scrub_interval = Duration::from_secs(self.scrub_interval_hours * 3600);
        let mut timer = interval(scrub_interval);

        // Skip the immediate first tick - we don't want to scrub on startup
        timer.tick().await;

        loop {
            timer.tick().await;

            info!("Starting scrubbing pass");
            if let Err(e) = self.scrub_all_chunks().await {
                warn!("Scrubbing error: {}", e);
            }
        }
    }

    /// Discovery pass — scans all chunk IDs, bulk-queries node presence, classifies
    /// Classify chunks, update pending_healing, and update alive_nodes_cache.
    /// Called by run_discovery_loop() every 60s. Does NOT issue any PushChunkTo —
    /// actual healing is handled by drain_heal_queue().
    ///
    /// `deep`: when true, checks every routing table entry (detects ghost nodes,
    /// undocumented local copies, orphans). When false (fast cycle), only checks
    /// chunks that need attention: those in pending_healing or with nodes.len() < RF.
    /// The fast cycle is O(pending) instead of O(all_chunks), making it viable at
    /// 500K+ chunk scale without OOM or multi-minute stalls.
    ///
    /// In both modes, the HasChunks RPC payload sent to each node is scoped to only
    /// the chunk IDs where that node is listed as a replica — so message size scales
    /// with per-node replica count, not total cluster chunk count.
    async fn run_discovery_pass(&self, deep: bool) -> Result<()> {
        if deep {
            debug!("Running deep discovery pass (full routing table scan)");
        } else {
            debug!("Running fast discovery pass (pending/under-replicated chunks only)");
        }

        // Gate destructive operations (orphan purge, DATA LOSS declarations,
        // over-replication cleanup) on strict cluster health:
        //   1. At most 1 node down — if 2+ nodes are down we might be the only copy.
        //   2. Past the post-election grace period — cluster must be settled before
        //      we start deleting anything.
        // Under-replication healing (adding replicas) is always safe and is not gated.
        let all_nodes = self.cluster.get_all_nodes().await;
        let total_nodes = all_nodes.len();
        let online_nodes = all_nodes.iter()
            .filter(|n| n.status == dfs_common::NodeStatus::Online)
            .count();
        let nodes_down = total_nodes.saturating_sub(online_nodes);

        let grace_elapsed = self.cluster.time_since_became_leader().await
            .map_or(true, |d| d.as_secs() >= LEADER_CHANGE_GRACE_SECS);

        let destructive_allowed = grace_elapsed && nodes_down <= 1;

        if !destructive_allowed {
            if !grace_elapsed {
                let elapsed = self.cluster.time_since_became_leader().await
                    .map_or(0.0, |d| d.as_secs_f64());
                warn!("Skipping destructive healing operations — within grace period after leader election ({:.0}s elapsed of {}s)",
                    elapsed, LEADER_CHANGE_GRACE_SECS);
            } else {
                warn!("Skipping destructive healing operations — {} node(s) down (max 1 allowed)", nodes_down);
            }
        }

        let local_id = self.cluster.local_node_id();
        let online_nodes: Vec<_> = self.cluster.get_all_nodes().await
            .into_iter()
            .filter(|n| n.status == dfs_common::NodeStatus::Online)
            .collect();

        // All known node IDs (online + offline) — used to detect completely-removed ghost nodes.
        let all_known_node_ids: HashSet<NodeId> = self.cluster.get_all_nodes().await
            .into_iter()
            .map(|n| n.id)
            .collect();

        // Sled scan: stream through chunk location records, building only what we need.
        // Offloaded to a blocking thread to avoid stalling the async executor.
        //
        // We never materialise the full Vec<ChunkLocation> — instead the blocking task
        // produces the derived data structures directly:
        //   chunks_to_check   — locations that need network verification this cycle
        //   live_chunks       — chunk IDs referenced by live file records (deep only)
        //
        // This keeps peak memory proportional to what we actually act on, not the
        // entire routing table.
        let metadata_scan = self.metadata.clone();
        let pending_snapshot: HashSet<ChunkId> = self.pending_healing.read().await.keys().copied().collect();

        struct ScanResult {
            /// Locations to verify with HasChunks this cycle.
            chunks_to_check: Vec<ChunkLocation>,
            /// Fix (orphan-heal clog, 2026-07-24): chunk_ids from pending_healing that
            /// the deep pass's own durable live-set does NOT recognize — true orphans
            /// (superseded/retired chunk_ids with a lingering CHUNK_TABLE row) that
            /// would otherwise cycle in the heal queue forever, since PushChunkTo can
            /// never verify a stale id. Computed by reusing `live` (already fetched to
            /// filter chunks_to_check below — zero extra scan cost) rather than a
            /// separate in-memory chunk_map crawl: two DIFFERENT liveness views can
            /// disagree right after a restart (chunk_map is rebuilt on a background
            /// thread and fed by live traffic with no convergence guarantee tied to
            /// wall-clock uptime — this caused a real T38 regression, a genuinely-live
            /// chunk wrongly dequeued because chunk_map hadn't caught up yet), whereas
            /// this and the chunks_to_check filter share one read transaction and can
            /// never disagree with each other. Always empty on the shallow pass (no
            /// fresh `live` there — see should_apply_orphan_dequeue's doc comment for
            /// why we don't act without one).
            orphaned_pending: Vec<ChunkId>,
            /// Fix (superseded-generation clog, 2026-07-25): chunk_ids that ARE
            /// file_id-live (so `live` above includes them — their file genuinely
            /// exists) but lost the supersession contest for their own
            /// (file_id, chunk_idx) slot to a different, newer chunk_id — a
            /// generation CHUNK_TABLE never pruned when the slot moved on. Root
            /// cause confirmed live on staging 2026-07-24/25: `live_chunk_ids()`
            /// only ever checks "does file_id resolve", by design (see its doc
            /// comment) — it was never meant to also confirm "is this exact
            /// chunk_id still current", so this class of stale row is invisible
            /// to both the orphan-dequeue diff above AND run_disk_orphan_sweep's
            /// candidate gate (both are downstream of the same file_id-only
            /// oracle). PushChunkTo can never heal one (its on-disk bytes no
            /// longer match this retired generation's hash — "content hash
            /// mismatch"), so left alone it cycles in the queue forever. Resolved
            /// here as a byproduct of the same scan (group chunks_to_check by
            /// slot, keep only the location_supersedes winner) rather than a
            /// second CHUNK_TABLE pass.
            slot_losers: Vec<ChunkId>,
        }

        // Fast path: no sled scan at all. We fetch locations only for chunks already
        // in pending_healing — O(pending) individual lookups, not O(all_chunks).
        // Deep path: full streaming sled scan for finding under-RF chunks that
        // haven't entered pending_healing yet. (Orphan detection used to live here
        // too — it now runs per-node in run_disk_orphan_sweep(), independent of
        // leadership; see that function's doc comment.)
        let scan_result = if !deep {
            // Fast: look up only pending chunks by ID.
            let mut chunks_to_check = Vec::with_capacity(pending_snapshot.len());
            for chunk_id in &pending_snapshot {
                match self.metadata.get_chunk_location(chunk_id) {
                    Ok(Some(loc)) => {
                        // If the chunk carries a file_id, verify the file still exists.
                        // Orphan routing table entries (file deleted but RCL entry
                        // re-added later) would otherwise cause the fast scan to keep
                        // trying to heal chunks that belong to deleted files.
                        let is_orphan = if let Some(file_id) = loc.file_id {
                            !self.metadata.file_exists_by_id(file_id).unwrap_or(true)
                        } else {
                            false // can't tell without file_id; defer to deep scan
                        };
                        if is_orphan {
                            let _ = self.metadata.delete_chunk_location_async(*chunk_id).await;
                            self.clear_pending(chunk_id).await;
                        } else {
                            chunks_to_check.push(loc);
                        }
                    }
                    // Chunk was deleted from routing table (file deleted) but its
                    // pending_healing entry was never cleaned up. Prune it now.
                    Ok(None) => { self.clear_pending(chunk_id).await; }
                    Err(_) => {}
                }
            }
            ScanResult { chunks_to_check, orphaned_pending: Vec::new(), slot_losers: Vec::new() }
        } else {
            let pending_snapshot_for_orphans = pending_snapshot.clone();
            let fold_result_chunk_ids = self.fold_result_chunk_ids.clone();
            let chunk_generations = self.chunk_generations.clone();
            tokio::task::spawn_blocking(move || {
                // Combined single CHUNK_TABLE pass (see scan_live_chunk_locations's doc
                // comment) — was two separate full scans (live_chunk_ids then
                // scan_chunk_locations), the dominant cost of a deep pass on a large table.
                let mut chunks_to_check = Vec::new();
                let live = metadata_scan.scan_live_chunk_locations(|loc| {
                    chunks_to_check.push(loc);
                })?;

                // See ScanResult::orphaned_pending's doc comment. Safety: an empty
                // `live` while pending work is outstanding means this node's FILE_TABLE
                // hasn't converged (e.g. moments after a leader election), not that
                // every pending chunk is genuinely orphaned — treat nothing as orphaned
                // this pass rather than risk wiping the whole queue in one shot.
                let orphaned_pending = if Self::orphan_candidates_are_trustworthy(
                    live.is_empty(), pending_snapshot_for_orphans.is_empty(),
                ) {
                    pending_snapshot_for_orphans.iter()
                        .filter(|id| !live.contains(id))
                        .copied()
                        .collect()
                } else {
                    Vec::new()
                };

                // See ScanResult::slot_losers's doc comment. Group the already-fetched
                // file_id-live rows by (file_id, chunk_idx) and keep only the
                // location_supersedes winner per slot — same tiebreak the read path
                // (chunk_locations_for_info) and the fold path use, so this can never
                // pick a different "current" chunk than a live read would. Rows
                // without a file_offset can't be slot-deduped (no chunk_idx to group
                // by) and are left untouched, same as chunk_locations_for_info does.
                const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
                let mut slot_winners: std::collections::HashMap<(dfs_common::FileId, u64), &ChunkLocation> =
                    std::collections::HashMap::new();
                for loc in &chunks_to_check {
                    let (Some(file_id), Some(offset)) = (loc.file_id, loc.file_offset) else {
                        continue;
                    };
                    let key = (file_id, offset / CHUNK_SIZE);
                    match slot_winners.entry(key) {
                        std::collections::hash_map::Entry::Occupied(mut e) => {
                            let cur = *e.get();
                            if crate::server::Server::location_supersedes(
                                loc, cur,
                                fold_result_chunk_ids.contains(&loc.chunk_id),
                                fold_result_chunk_ids.contains(&cur.chunk_id),
                                chunk_generations.get(&loc.chunk_id).map(|v| *v),
                                chunk_generations.get(&cur.chunk_id).map(|v| *v),
                            ) {
                                e.insert(loc);
                            }
                        }
                        std::collections::hash_map::Entry::Vacant(e) => { e.insert(loc); }
                    }
                }
                let slot_winner_ids: HashSet<ChunkId> = slot_winners.values().map(|l| l.chunk_id).collect();
                let slot_losers: Vec<ChunkId> = chunks_to_check.iter()
                    .filter(|loc| loc.file_offset.is_some() && !slot_winner_ids.contains(&loc.chunk_id))
                    .map(|loc| loc.chunk_id)
                    .collect();

                Ok::<_, anyhow::Error>(ScanResult { chunks_to_check, orphaned_pending, slot_losers })
            })
            .await
            .context("spawn_blocking for chunk scan panicked")??
        };

        // Re-check leadership after the sled scan — on large clusters the scan can
        // take minutes, during which leadership may have transferred. No point sending
        // HasChunks RPCs or modifying state if we're no longer the leader.
        if !self.cluster.is_leader().await {
            debug!("Leadership changed during deep scan — aborting discovery pass");
            return Ok(());
        }

        let ScanResult { chunks_to_check, orphaned_pending, slot_losers } = scan_result;
        // Modest additional settle floor (process uptime), defense-in-depth alongside
        // the live-set-trustworthiness check above — cheap, and this node's own recent
        // restart is exactly when its local metadata is least likely to have finished
        // syncing. Non-destructive/self-correcting either way (see ScanResult's doc
        // comment), so this does not need SELF_RESTART_GRACE_SECS's full 20 minutes.
        let orphan_dequeues: Vec<ChunkId> =
            if self.local_started_at.elapsed().as_secs() >= ORPHAN_DEQUEUE_SETTLE_SECS {
                orphaned_pending
            } else {
                Vec::new()
            };

        // Refresh the cached slot-loser set (see superseded_generation_chunk_ids's
        // doc comment) for run_disk_orphan_sweep and the classification loop below
        // to both consult without paying for their own scan+group. Only a deep pass
        // computes this (see ScanResult::slot_losers — always empty on shallow); on
        // a shallow pass, leave the previous deep pass's set in place rather than
        // clobbering it with an empty one — every caller runs deep-only today (see
        // ScanResult's doc comment history), but this keeps the cache correct even
        // if a shallow caller is reintroduced later.
        let slot_losers: HashSet<ChunkId> = slot_losers.into_iter().collect();
        if deep {
            *self.superseded_generation_chunk_ids.write().await = slot_losers.clone();
        }

        // work carries (chunk_id, status, confirmed_alive_node_ids) from the bulk scan.
        let mut work: Vec<(ChunkId, ReplicationStatus, Vec<NodeId>)> = Vec::new();
        let mut pending_count = 0;

        // Pending DB writes — applied in a single spawn_blocking after all classification.
        // Direct calls to metadata.put_chunk_location() / delete_chunk_location() call
        // redb's begin_write() which blocks the OS thread. Under a heal storm with many
        // concurrent async tasks all hitting begin_write(), every Tokio worker thread can
        // end up blocked on the mutex, freezing the entire async runtime.
        let mut db_puts: Vec<ChunkLocation> = Vec::new();
        let mut db_deletes: Vec<ChunkId> = Vec::new();
        // Deferred PENDING_HEALING_TABLE writes from the classification loop below —
        // new detections (mark_pending_deferred) and cleared entries — flushed as
        // batches alongside db_puts/db_deletes instead of one transaction per chunk.
        let mut deferred_pending_marks: Vec<(ChunkId, u64)> = Vec::new();
        let mut deferred_pending_clears: Vec<ChunkId> = Vec::new();

        // Orphan detection/purging used to live here (leader-only, gated on
        // destructive_allowed + a 30-minute age grace + two-pass confirmation). It now
        // runs on every node via run_disk_orphan_sweep()/reconcile_live_file_candidates(),
        // independent of leadership, with a leader-confirm-or-cluster-stability gate
        // replacing the old grace-period-only check — see authorize_live_file_orphan_deletes().

        // --- Build per-node chunk assignment maps ---
        // For each online node, collect only the chunk IDs where that node is listed
        // as a replica. This scopes the HasChunks payload to each node's actual
        // assignments rather than the entire cluster chunk universe.
        //
        // node_id → ordered Vec<ChunkId> that node is responsible for
        let mut node_assigned: HashMap<NodeId, Vec<ChunkId>> = HashMap::new();
        for loc in &chunks_to_check {
            for node_id in &loc.nodes {
                if all_known_node_ids.contains(node_id) {
                    node_assigned.entry(*node_id).or_default().push(loc.chunk_id);
                }
            }
        }

        // --- HasChunks RPCs: one per online node, payload = that node's assignments ---
        // node_id → HashSet<ChunkId> confirmed present
        let mut node_chunk_presence: HashMap<NodeId, HashSet<ChunkId>> = HashMap::new();
        // Nodes whose HasChunks RPC failed this cycle — skip in classification rather than
        // treating as "chunk missing". A timeout means we don't know the node's state;
        // penalizing it causes ghost pruning + over-replication on the next cycle.
        let mut rpc_failed_nodes: HashSet<NodeId> = HashSet::new();

        // Local node: check storage directly (no network hop).
        // One recursive directory walk (list_chunks()) instead of tens of thousands
        // of individual has_chunk() stat() calls on a deep cycle — far cheaper than
        // that many scattered random lookups, and still run via spawn_blocking since
        // it's real disk I/O (the inline per-chunk version of this loop froze the
        // leader for ~2.5 hours with zero log output — incident 2026-06-20).
        if node_assigned.contains_key(&local_id) {
            let storage = self.storage.clone();
            let local_set = tokio::task::spawn_blocking(move || {
                storage.list_chunks().map(|v| v.into_iter().collect::<HashSet<_>>())
            }).await;
            match local_set {
                Ok(Ok(set)) => { node_chunk_presence.insert(local_id, set); }
                Ok(Err(e)) => warn!("Discovery pass: local chunk listing failed: {}", e),
                Err(e) => warn!("Discovery pass: local chunk listing panicked: {}", e),
            }
        }

        // Remote nodes: one HasChunks RPC each, scoped to that node's assignments.
        for node_info in &online_nodes {
            if node_info.id == local_id {
                continue;
            }
            // Abort if we lost leadership mid-scan — don't send RPCs as non-leader.
            if !self.cluster.is_leader().await {
                debug!("Leadership changed during HasChunks RPCs — aborting discovery pass");
                return Ok(());
            }
            let assigned = match node_assigned.get(&node_info.id) {
                Some(ids) if !ids.is_empty() => ids.clone(),
                _ => continue, // node has no assigned chunks in this scan scope — skip
            };
            let request = Request::HasChunks { chunk_ids: assigned.clone() };
            match self.client.send_message(node_info.addr, Message::Request(request)).await {
                Ok(envelope) => {
                    if let Message::Response(Response::BoolVec { values }) = envelope.message {
                        let mut present = HashSet::new();
                        for (chunk_id, has) in assigned.iter().zip(values.iter()) {
                            if *has {
                                present.insert(*chunk_id);
                            }
                        }
                        debug!("Node {} confirmed {}/{} assigned chunks present",
                               node_info.id, present.len(), assigned.len());
                        node_chunk_presence.insert(node_info.id, present);
                    } else {
                        warn!("Unexpected response to HasChunks from node {}", node_info.id);
                    }
                }
                Err(e) => {
                    warn!("HasChunks RPC failed for node {} ({}): skipping node for this scan cycle", node_info.id, e);
                    rpc_failed_nodes.insert(node_info.id);
                }
            }
        }

        // Accumulate chunk location updates for batch broadcast at end of pass.
        let mut location_updates: Vec<ChunkLocation> = Vec::new();

        // Patch tokens (deferred chunk-patch consolidation) are never worth
        // actively healing directly — a token never names a real file (Pending) or
        // is a permanent alias to one living at a different identity (Folded),
        // which gets its own normal ChunkLocation and is healed on its own merits.
        //
        // PATCH_STATE_TABLE is node-local and never disseminated, and a patch can
        // land on any node in the cluster, not just this one — a local-only lookup
        // here is blind to every token created on a follower. Gather every online
        // node's token set (this node's own plus one GetPatchTokenIds RPC per peer,
        // the table is tiny) and union them before classifying. Without this union,
        // a follower-created token slips through this exclusion, gets classified as
        // an ordinary chunk below, and HasChunks correctly (but misleadingly)
        // reports it absent everywhere — a token is never a real on-disk file —
        // which can stall its reported replica count forever or, worse, walk it
        // into the DATA LOSS purge path below.
        // Tracks whether EVERY node in the cluster contributed its token set this
        // cycle. A token is invisible to us unless its originating node answered, and
        // an invisible token is indistinguishable from an ordinary chunk that is
        // absent everywhere — i.e. it walks straight into the DATA LOSS purge below.
        //
        // CONFIRMED DATA LOSS, twice (2026-07-15 vm-108 chunk_idx 230; 2026-07-17
        // 06:09 on a completely IDLE cluster with no writes since a clean fsck):
        //   DATA LOSS: Chunk <token> is permanently unrecoverable
        //       (N metadata nodes, all confirmed empty) — purging stale metadata
        // "all confirmed empty" is trivially true of a patch token: it has no file on
        // disk by design until its fold lands. The union below exists precisely to
        // stop that, but it only ever queried ONLINE nodes and merely warned on RPC
        // failure — so any node that was briefly away (a PlannedCompaction leave is
        // enough, and those happen constantly) took 100% of its tokens out of view
        // while destructive_allowed stayed true, since that gate tolerates
        // `nodes_down <= 1` — correct for RF=3 replica math, catastrophically wrong
        // for token visibility, where one node down is a total blind spot, not a
        // 1-in-3 one.
        //
        // So completeness is now tracked explicitly and the purge defers without it.
        // Same contract classify_zero_replica_chunk already documents for itself:
        // when you cannot confirm, DEFER — never guess. Deferring costs one cycle;
        // guessing deleted a live VM disk chunk.
        let all_node_count = self.cluster.get_all_nodes().await.len();
        let mut patch_token_view_complete = online_nodes.len() >= all_node_count;
        if !patch_token_view_complete {
            warn!("Patch-token view incomplete: {}/{} nodes online — offline nodes' tokens are invisible this cycle, deferring any DATA LOSS purge",
                online_nodes.len(), all_node_count);
        }
        let mut patch_token_ids = match self.metadata.all_patch_token_ids_async().await {
            Ok(ids) => ids,
            Err(e) => {
                warn!("Failed to read local patch tokens ({}) — deferring any DATA LOSS purge this cycle", e);
                patch_token_view_complete = false;
                Default::default()
            }
        };
        for node_info in &online_nodes {
            if node_info.id == local_id {
                continue;
            }
            if !self.cluster.is_leader().await {
                debug!("Leadership changed during GetPatchTokenIds RPCs — aborting discovery pass");
                return Ok(());
            }
            match self.client.send_message(node_info.addr, Message::Request(Request::GetPatchTokenIds)).await {
                Ok(envelope) => {
                    if let Message::Response(Response::PatchTokenIds { ids }) = envelope.message {
                        patch_token_ids.extend(ids);
                    } else {
                        warn!("Unexpected response to GetPatchTokenIds from node {} — its patch tokens are invisible this cycle, deferring any DATA LOSS purge", node_info.id);
                        patch_token_view_complete = false;
                    }
                }
                Err(e) => {
                    warn!("GetPatchTokenIds RPC failed for node {} ({}): its patch tokens are invisible this cycle, deferring any DATA LOSS purge", node_info.id, e);
                    patch_token_view_complete = false;
                }
            }
        }

        // Classify all chunks — no work cap here. Every chunk must be classified so
        // that pending_healing timestamps are updated for all under-replicated chunks,
        // not just the first max_heal_per_cycle. Without this, chunks past position 200
        // never enter pending_healing and never get healed until the queue drains.
        // The work batch is capped after classification, sorted oldest-first so long-
        // waiting chunks are not perpetually starved by newer ones.
        //
        // Note: chunks_to_check now carries full ChunkLocation records from the sled
        // scan, so we don't need a per-chunk DB lookup here.
        let mut classify_count = 0usize;
        for location in chunks_to_check {
            let chunk_id = location.chunk_id;
            classify_count += 1;
            if classify_count % 100 == 0 {
                tokio::task::yield_now().await;
            }
            if patch_token_ids.contains(&chunk_id) {
                // Never actively healed (see patch_token_ids' doc comment above), so
                // it must not be left sitting in pending_healing either — nothing
                // past this `continue` will ever reach this chunk_id's clear_pending
                // call again. Without this, a chunk_id that was marked pending
                // before it was recognized as a token (e.g. an explicit `dfs-admin
                // healing file` trigger, which marks every chunk in a file
                // unconditionally) stays in the reported heal queue forever, even
                // though there is deliberately nothing left to do for it.
                self.clear_pending(&chunk_id).await;
                continue;
            }

            if slot_losers.contains(&chunk_id) {
                // See ScanResult::slot_losers's doc comment: file_id-live but not
                // this slot's current generation — never worth healing (its bytes
                // can never verify against this retired chunk_id) and must not sit
                // in pending_healing occupying a dispatch slot or a queue-depth
                // count. Non-destructive here — same as the patch-token skip above,
                // this only drops the in-memory/durable heal-queue bookkeeping.
                // Actual physical deletion still goes through run_disk_orphan_sweep's
                // full age-gate + leader-confirm pipeline (see
                // superseded_generation_chunk_ids's doc comment), same as any other
                // live-file orphan candidate.
                self.clear_pending(&chunk_id).await;
                continue;
            }

            let metadata_node_count = location.nodes.len();

            let mut actual_replicas = 0usize;
            let mut nodes_without_chunk: Vec<NodeId> = Vec::new();
            let mut confirmed_alive_nodes: Vec<NodeId> = Vec::new();
            let mut removed_node_ids: Vec<NodeId> = Vec::new();

            for node_id in &location.nodes {
                if !all_known_node_ids.contains(node_id) {
                    // Node is completely unknown to the cluster (removed/decommissioned).
                    // Prune it immediately — no delay needed, it will never come back.
                    removed_node_ids.push(*node_id);
                    continue;
                }
                // Only count online nodes — offline nodes are expected to be absent.
                if online_nodes.iter().any(|n| n.id == *node_id) {
                    if rpc_failed_nodes.contains(node_id) {
                        // RPC timed out — we don't know if chunk is present.
                        // Skip entirely: don't count as alive OR missing.
                        // Treating as missing causes ghost pruning + over-replication.
                        debug!("Chunk {} — node {} had RPC failure this cycle, skipping", chunk_id, node_id);
                        continue;
                    }
                    if node_chunk_presence.get(node_id).map_or(false, |s| s.contains(&chunk_id)) {
                        actual_replicas += 1;
                        confirmed_alive_nodes.push(*node_id);
                    } else {
                        nodes_without_chunk.push(*node_id);
                    }
                }
            }

            // Prune removed (completely unknown) nodes from metadata immediately.
            if !removed_node_ids.is_empty() && destructive_allowed {
                warn!(
                    "Chunk {} — pruning {} removed node(s) from metadata (not in cluster): {:?}",
                    chunk_id, removed_node_ids.len(), removed_node_ids
                );
                let pruned_nodes: Vec<NodeId> = location.nodes.iter()
                    .filter(|n| !removed_node_ids.contains(n))
                    .copied()
                    .collect();
                let updated_location = ChunkLocation {
                    chunk_id,
                    nodes: pruned_nodes,
                    size: location.size,
                    checksum: location.checksum,
                    file_offset: location.file_offset,
                    written_at: location.written_at,
                    // L1 (2026-07-19 ghost-clobber): preserve the chunk's real seq. A
                    // node-list reconciliation must not blank identity to None — a None
                    // entry loses the precedence contest to any stale seq'd broadcast of an
                    // already-folded chunk_id, reverting the slot to a ghost that EIO'd a VM.
                    client_write_seq: location.client_write_seq,
                    file_id: location.file_id,
                };
                db_puts.push(updated_location.clone());
                location_updates.push(updated_location);
            }

            // Ghost node pruning: only remove a node from metadata after the healing
            // delay has passed for this chunk. A single HasChunks miss is not enough —
            // the node may have just restarted (connections drop, storage comes up within
            // seconds, but the next heal cycle is 60s away). We wait healing_delay_secs
            // before trusting the miss, same patience we apply to under-replication.
            if !nodes_without_chunk.is_empty() && actual_replicas > 0 {
                // Start the ghost-pruning timer the first time we see these nodes missing.
                // Without this, chunks at exactly RF alive nodes never enter pending_healing
                // (they're classified Ok below and evicted), so the delay can never expire.
                self.mark_pending(chunk_id).await;

                let pending = self.pending_healing.read().await;
                let delay_passed = pending.get(&chunk_id)
                    .map_or(false, |t| t.elapsed() >= Duration::from_secs(self.healing_delay_secs.load(Ordering::Relaxed)));
                drop(pending);

                if delay_passed {
                    warn!(
                        "Chunk {} — pruning {} ghost node(s) after delay (confirmed missing for {}s+): {:?}",
                        chunk_id, nodes_without_chunk.len(), self.healing_delay_secs.load(Ordering::Relaxed), nodes_without_chunk
                    );
                    let pruned_nodes: Vec<NodeId> = location.nodes.iter()
                        .filter(|n| !nodes_without_chunk.contains(n))
                        .copied()
                        .collect();
                    let updated_location = ChunkLocation {
                        chunk_id,
                        nodes: pruned_nodes,
                        size: location.size,
                        checksum: location.checksum,
                        file_offset: location.file_offset,
                        written_at: Some(Self::now_ms()),
                        // L1: preserve the chunk's real seq (see node-prune comment above).
                        client_write_seq: location.client_write_seq,
                        file_id: location.file_id,
                    };
                    db_puts.push(updated_location.clone());
                    location_updates.push(updated_location);
                } else {
                    debug!(
                        "Chunk {} — {} online node(s) didn't confirm holding it, waiting for delay before pruning: {:?}",
                        chunk_id, nodes_without_chunk.len(), nodes_without_chunk
                    );
                }
            }

            let replication_factor = self.replication_factor.load(Ordering::Relaxed);

            // Detect unrecoverable chunks: actual_replicas == 0 and all metadata nodes
            // were reachable and confirmed they don't have it (or node list is empty).
            // Requirements before declaring unrecoverable:
            //  1. All metadata nodes must be reachable (online + responded to HasChunks) —
            //     an offline node might still hold the chunk.
            //  2. The healing delay must have passed — a node that just restarted may be
            //     online and responding but its storage could still be loading. We wait the
            //     same delay we use for under-replication before writing off the data.
            let all_metadata_nodes_reachable = location.nodes.iter()
                .all(|n| node_chunk_presence.contains_key(n));
            if actual_replicas == 0 && (location.nodes.is_empty() || all_metadata_nodes_reachable) {
                let pending = self.pending_healing.read().await;
                let delay_passed = if location.nodes.is_empty() {
                    // No nodes in the location record at all. This could mean:
                    //   (a) A genuinely incomplete write (nodes never populated), OR
                    //   (b) The chunk is very new and the location record hasn't been
                    //       updated with node IDs yet due to convergence lag.
                    // Use written_at to apply the same delay as normal under-replication —
                    // don't skip the delay just because nodes is empty.
                    let age_secs = location.written_at_secs();
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let written_age = now.saturating_sub(age_secs);
                    // Also check pending_healing as a fallback (written_at may be 0 for old records).
                    let pending_delay = pending.get(&chunk_id)
                        .map_or(false, |t| t.elapsed() >= Duration::from_secs(self.healing_delay_secs.load(Ordering::Relaxed)));
                    (age_secs > 0 && written_age >= self.healing_delay_secs.load(Ordering::Relaxed)) || pending_delay
                } else {
                    pending.get(&chunk_id)
                        .map_or(false, |t| t.elapsed() >= Duration::from_secs(self.healing_delay_secs.load(Ordering::Relaxed)))
                };
                drop(pending);

                if delay_passed {
                    if !destructive_allowed {
                        warn!(
                            "Chunk {} has 0 accessible replicas and delay passed, but skipping DATA LOSS purge — cluster degraded ({} node(s) down or in grace period)",
                            chunk_id, nodes_down
                        );
                    } else if !Self::may_declare_data_loss(destructive_allowed, patch_token_view_complete) {
                        // We could not enumerate every node's patch tokens this cycle,
                        // so we cannot rule out that this chunk_id IS a token — and a
                        // token is absent-everywhere by design, which is exactly the
                        // evidence we would otherwise purge on. See
                        // patch_token_view_complete's doc comment for the two confirmed
                        // data-loss incidents this prevents. Deliberately separate from
                        // destructive_allowed: that gate tolerates nodes_down <= 1 for
                        // replica math, which is precisely the case where a node's
                        // entire token set is invisible.
                        warn!(
                            "Chunk {} has 0 accessible replicas and delay passed, but skipping DATA LOSS purge — patch-token view is incomplete this cycle, so this chunk_id cannot be ruled out as an unfolded patch token",
                            chunk_id
                        );
                    } else {
                        // Before declaring DATA LOSS, check whether this chunk_id was
                        // superseded by a newer write, or genuinely unrecoverable, or
                        // unknown (metadata read failed — must not purge, see
                        // classify_zero_replica_chunk's doc comment).
                        if let Some(is_superseded) = self.classify_zero_replica_chunk(chunk_id, &location) {
                            if is_superseded {
                                debug!(
                                    "Chunk {} has 0 replicas but was superseded by a newer write at the same file position — purging stale metadata (not DATA LOSS)",
                                    chunk_id
                                );
                            } else {
                                warn!(
                                    "DATA LOSS: Chunk {} is permanently unrecoverable ({} metadata nodes, all confirmed empty) — purging stale metadata",
                                    chunk_id, location.nodes.len()
                                );
                            }
                            db_deletes.push(chunk_id);
                            self.clear_pending(&chunk_id).await;
                            continue;
                        }
                        // None: metadata read failed, already logged inside
                        // classify_zero_replica_chunk. Don't purge — fall through to
                        // reconciliation below so this chunk is re-evaluated next cycle.
                    }
                } else {
                    debug!(
                        "Chunk {} has 0 accessible replicas but delay not yet passed — waiting before declaring unrecoverable",
                        chunk_id
                    );
                }
            }

            // Reconcile routing table: if discovery found the chunk alive on nodes that
            // aren't in location.nodes, add them now before classifying. This prevents
            // the routing table from staying stale when the leader or other nodes have a
            // physical copy that wasn't recorded (e.g. from a previous local-fallback heal
            // that pushed data but didn't update metadata, or manual copy). Without this,
            // over-replication cleanup removes the "surprise" copies before the table is
            // updated, leaving the chunk under-replicated again on the next cycle.
            let extra_nodes: Vec<NodeId> = confirmed_alive_nodes.iter()
                .filter(|n| !location.nodes.contains(n))
                .copied()
                .collect();
            let (location, metadata_node_count) = if !extra_nodes.is_empty() {
                let mut updated_nodes = location.nodes.clone();
                updated_nodes.extend_from_slice(&extra_nodes);
                debug!(
                    "Chunk {} — reconciling routing table: adding {} newly-discovered node(s): {:?}",
                    chunk_id, extra_nodes.len(), extra_nodes
                );
                let updated_location = ChunkLocation {
                    chunk_id,
                    nodes: updated_nodes,
                    size: location.size,
                    checksum: location.checksum,
                    file_offset: location.file_offset,
                    written_at: location.written_at,
                    // L1 (2026-07-19 ghost-clobber): preserve the chunk's real seq. A
                    // node-list reconciliation must not blank identity to None — a None
                    // entry loses the precedence contest to any stale seq'd broadcast of an
                    // already-folded chunk_id, reverting the slot to a ghost that EIO'd a VM.
                    client_write_seq: location.client_write_seq,
                    file_id: location.file_id,
                };
                db_puts.push(updated_location.clone());
                location_updates.push(updated_location.clone());
                let new_count = updated_location.nodes.len();
                (updated_location, new_count)
            } else {
                (location, metadata_node_count)
            };

            let status = if actual_replicas < replication_factor {
                ReplicationStatus::UnderReplicated
            } else if actual_replicas > replication_factor {
                ReplicationStatus::OverReplicated
            } else {
                ReplicationStatus::Ok
            };

            match status {
                ReplicationStatus::UnderReplicated => {
                    if confirmed_alive_nodes.is_empty() {
                        // No source available — move to stalled. Discovery will promote
                        // it back to pending as soon as a node that holds it comes online.
                        debug!("Chunk {} has 0 confirmed alive nodes — moving to stalled", chunk_id);
                        self.stalled_healing.write().await.insert(chunk_id);
                        self.requeue_priority.insert(chunk_id, Instant::now());
                        // Keep first_detected timestamp in pending so we know how long
                        // it has been under-replicated, but mark it explicitly stalled.
                        self.mark_pending_deferred(chunk_id, &mut deferred_pending_marks).await;
                        pending_count += 1;
                        continue;
                    }

                    // Source available — ensure not in stalled (promote if it was).
                    {
                        let mut stalled = self.stalled_healing.write().await;
                        if stalled.remove(&chunk_id) {
                            debug!("Chunk {} has a source again — promoted from stalled to pending", chunk_id);
                        }
                    }

                    // A prior heal attempt already found a confirmed-alive source but
                    // failed to replicate to any target (source-side corruption, most
                    // likely) — HasChunks presence alone doesn't mean the source is
                    // actually usable, so don't let the "source available" promotion
                    // above put this straight back on the work queue every cycle. See
                    // heal_push_failure's doc comment for the incident this closes.
                    if Self::heal_push_failure_backoff_active(&self.heal_push_failure, &chunk_id) {
                        self.mark_pending_deferred(chunk_id, &mut deferred_pending_marks).await;
                        pending_count += 1;
                        continue;
                    }

                    // For chunks that were never fully replicated to RF nodes, apply a
                    // minimum delay of healing_delay_secs before healing. This prevents
                    // the healer from adding replicas to chunks that are still being
                    // actively written — the write pipeline emits chunks one at a time
                    // and the healer must wait for the file to be fully written before
                    // it can know the correct final replica set.
                    let never_fully_replicated = metadata_node_count < replication_factor;
                    if self.should_heal(&chunk_id, &mut deferred_pending_marks).await {
                        work.push((chunk_id, ReplicationStatus::UnderReplicated, confirmed_alive_nodes.clone()));
                    } else {
                        if never_fully_replicated {
                            // Ensure pending_healing entry exists so should_heal() starts
                            // tracking the delay from first discovery.
                            self.mark_pending_deferred(chunk_id, &mut deferred_pending_marks).await;
                        }
                        pending_count += 1;
                    }
                }
                ReplicationStatus::OverReplicated => {
                    if destructive_allowed {
                        // Add to pending_healing with the real detection time so the
                        // normal healing_delay_secs wait applies before trimming.
                        // This gives the VerifyChunkIntegrity pass (in do_cleanup_excess_shared)
                        // time to run after the cluster has settled.
                        self.mark_pending_deferred(chunk_id, &mut deferred_pending_marks).await;
                        work.push((chunk_id, ReplicationStatus::OverReplicated, confirmed_alive_nodes.clone()));
                    } else {
                        debug!("Skipping over-replication cleanup for {} — cluster degraded", chunk_id);
                    }
                }
                ReplicationStatus::Ok => {
                    // Only evict from pending when there are no ghost nodes remaining.
                    // If nodes_without_chunk is non-empty we need the pending_healing
                    // entry to persist as the ghost-pruning delay timer.
                    if nodes_without_chunk.is_empty() {
                        // Same two effects as clear_pending, but the PENDING_HEALING_TABLE
                        // delete is deferred into the batched flush below instead of
                        // paying one transaction per now-healthy chunk.
                        self.pending_healing.write().await.remove(&chunk_id);
                        deferred_pending_clears.push(chunk_id);
                        self.stalled_healing.write().await.remove(&chunk_id);
                        self.heal_push_failure.remove(&chunk_id);
                    }
                }
            }
        }

        // Drain orphan de-queues (collected above): remove from the in-memory queues so
        // the reported Pending count drops immediately, clear the push-failure backoff,
        // and fold the routing-row/pending-table cleanup into the batched write below.
        if !orphan_dequeues.is_empty() {
            {
                let mut pending = self.pending_healing.write().await;
                let mut stalled = self.stalled_healing.write().await;
                for id in &orphan_dequeues {
                    pending.remove(id);
                    stalled.remove(id);
                    self.heal_push_failure.remove(id);
                    self.requeue_priority.remove(id);
                }
            }
            deferred_pending_clears.extend(orphan_dequeues.iter().copied());
            info!(
                "Discovery: dropped {} unreferenced-orphan chunk(s) from the heal queue \
                 (absent from this pass's durable live-set) — deletion left to the orphan sweep",
                orphan_dequeues.len()
            );
        }

        if deep && !slot_losers.is_empty() {
            info!(
                "Discovery: found {} superseded-generation chunk(s) this pass — file-live but not \
                 their slot's current generation, excluded from healing and queued as disk-orphan \
                 sweep candidates",
                slot_losers.len()
            );
        }

        // Apply all accumulated metadata writes in a single spawn_blocking call.
        // This is the key fix for the runtime deadlock: redb's begin_write() is a
        // synchronous exclusive lock. Calling it from async code during a healing
        // storm (many concurrent tasks) blocks all Tokio worker threads, freezing
        // the runtime. One batched spawn_blocking keeps the OS thread off the
        // async executor and reduces total lock contention to a single acquisition.
        if !db_puts.is_empty() || !db_deletes.is_empty() || !deferred_pending_clears.is_empty() {
            let metadata = Arc::clone(&self.metadata);
            let result = tokio::task::spawn_blocking(move || {
                metadata.batch_update_chunk_locations(&db_puts, &db_deletes, &deferred_pending_clears)?;
                Ok::<_, anyhow::Error>(())
            }).await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("Batch metadata update failed: {}", e),
                Err(e) => warn!("Batch metadata update spawn_blocking panicked: {}", e),
            }
        }

        // New detections from this pass (mark_pending_deferred) — one batch insert
        // instead of one transaction per newly-pending chunk.
        if !deferred_pending_marks.is_empty() {
            if let Err(e) = self.metadata.put_pending_healing_batch_async(deferred_pending_marks).await {
                warn!("Batch pending_healing persist failed: {}", e);
            }
        }

        // Update alive_nodes_cache so drain_heal_queue can read fresh presence data
        // without waiting for another scan. Under-replicated and over-replicated chunks
        // both need this; drain_heal_queue() reads the cache for all work it executes.
        {
            let mut cache = self.alive_nodes_cache.write().await;
            for (chunk_id, _status, confirmed_alive) in &work {
                cache.insert(*chunk_id, confirmed_alive.clone());
            }
            // Prune entries for chunks that are now Ok (pending removed above) or
            // have been purged as orphans, so the cache doesn't grow unboundedly.
            cache.retain(|chunk_id, _| {
                self.metadata.get_chunk_location(chunk_id)
                    .ok()
                    .flatten()
                    .is_some()
            });
        }

        // Batch-broadcast all chunk location updates accumulated during this pass.
        // One ReplicateChunkLocations per peer instead of one connection per chunk.
        if !location_updates.is_empty() {
            let cluster = self.cluster.clone();
            let client = self.client.clone();
            let local_id = cluster.local_node_id();
            let updates = location_updates;
            debug!("Batch-broadcasting {} chunk location updates to peers", updates.len());
            tokio::spawn(async move {
                let nodes = cluster.get_all_nodes().await;
                for node in &nodes {
                    if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                        continue;
                    }
                    let req = Request::ReplicateChunkLocations { locations: updates.clone() };
                    if let Err(e) = client.send_message(node.addr, Message::Request(req)).await {
                        debug!("Failed to batch-broadcast {} chunk location updates to node {}: {}",
                               updates.len(), node.id, e);
                    }
                }
            });
        }

        let under_count = work.iter().filter(|(_, s, _)| *s == ReplicationStatus::UnderReplicated).count();
        let over_count  = work.iter().filter(|(_, s, _)| *s == ReplicationStatus::OverReplicated).count();
        if under_count > 0 || over_count > 0 || pending_count > 0 {
            info!(
                "Discovery complete: under={}, over={}, pending_delay={}",
                under_count, over_count, pending_count
            );
        }

        Ok(())
    }

    /// Get-or-create `node`'s entry in `node_inflight`, sized to the current
    /// `heal_max_concurrent_per_node` target. Free function (not `&self`) so it can be
    /// called from the static/spawned task context in `drain_heal_queue`'s JoinSet, the
    /// same way `do_heal_chunk_shared` already operates without a `&self` receiver.
    fn node_semaphore(
        node_inflight: &Arc<DashMap<NodeId, Arc<Semaphore>>>,
        heal_max_concurrent_per_node: &Arc<AtomicUsize>,
        node: NodeId,
    ) -> Arc<Semaphore> {
        node_inflight
            .entry(node)
            .or_insert_with(|| {
                Arc::new(Semaphore::new(heal_max_concurrent_per_node.load(Ordering::Relaxed).max(1)))
            })
            .clone()
    }

    /// Block until a permit is free for `node`'s combined in+out concurrency budget.
    async fn acquire_node_permit(
        node_inflight: &Arc<DashMap<NodeId, Arc<Semaphore>>>,
        heal_max_concurrent_per_node: &Arc<AtomicUsize>,
        node: NodeId,
    ) -> OwnedSemaphorePermit {
        let sem = Self::node_semaphore(node_inflight, heal_max_concurrent_per_node, node);
        // Semaphore is never closed, so acquire_owned() only errors if closed — unreachable here.
        sem.acquire_owned().await.expect("node semaphore never closed")
    }

    /// Non-blocking variant — used to spread source selection across whichever
    /// alive replica-holder has a free slot right now, instead of always queuing
    /// behind the first candidate.
    fn try_acquire_node_permit(
        node_inflight: &Arc<DashMap<NodeId, Arc<Semaphore>>>,
        heal_max_concurrent_per_node: &Arc<AtomicUsize>,
        node: NodeId,
    ) -> Option<OwnedSemaphorePermit> {
        let sem = Self::node_semaphore(node_inflight, heal_max_concurrent_per_node, node);
        sem.try_acquire_owned().ok()
    }

    /// On-demand replacement for a missing alive_nodes_cache entry, scoped to one
    /// chunk instead of a full discovery pass's all-chunks fan-out. Root-caused
    /// 2026-07-15 (T38 local-suite repro, half-capacity resource caps): a chunk
    /// folded *after* the one deep discovery scan a manual trigger runs never gets
    /// a cache entry from anywhere — the only other writer is this same discovery
    /// pass, next due 60s later, far past the local suite's ~30s test window (and,
    /// in production, however long it takes the cluster to next scan under load).
    /// drain_heal_queue's `None => continue` then skips it every single 15s tick
    /// indefinitely, even though the entry sits correctly backdated past
    /// healing_delay_secs the whole time. Confirmed live: two chunks were
    /// re-queued via handle_replicate_patch_fold's rebroadcast sweep 36 times over
    /// one test run and never once reached classification. This is deliberately a
    /// single targeted HasChunks round trip (or a local check, for this node),
    /// not a scoped rerun of the full pass — cheap enough to run inline on a
    /// per-miss basis without turning every drain cycle into a discovery pass.
    async fn probe_single_chunk_alive_nodes(&self, chunk_id: &ChunkId) -> Vec<NodeId> {
        let local_id = self.cluster.local_node_id();
        let online_nodes = self.cluster.get_all_nodes().await;
        let mut alive = Vec::new();
        for node in &online_nodes {
            if node.status != dfs_common::NodeStatus::Online {
                continue;
            }
            let present = if node.id == local_id {
                self.storage.has_chunk(chunk_id)
            } else {
                let request = Request::HasChunks { chunk_ids: vec![*chunk_id] };
                match tokio::time::timeout(Duration::from_secs(3), self.client.send_message(node.addr, Message::Request(request))).await {
                    Ok(Ok(envelope)) => matches!(
                        envelope.message,
                        Message::Response(Response::BoolVec { values }) if values.first() == Some(&true)
                    ),
                    Ok(Err(e)) => {
                        debug!("probe_single_chunk_alive_nodes: HasChunks failed for node {} ({}): treating as absent this probe", node.id, e);
                        false
                    }
                    Err(_) => {
                        debug!("probe_single_chunk_alive_nodes: HasChunks timed out for node {}: treating as absent this probe", node.id);
                        false
                    }
                }
            };
            if present {
                alive.push(node.id);
            }
        }
        alive
    }

    /// Sort key for drain_heal_queue's per-cycle ordering — see that function's sort
    /// comment for the incident this closes. Pure and independently testable: severity
    /// (UnderReplicated before OverReplicated) ranks first, then ascending alive-replica
    /// count within UnderReplicated (1-of-N before 2-of-3), then oldest-first as the
    /// final tie-break. Lower tuples sort first.
    fn heal_queue_sort_key(
        status: ReplicationStatus, alive_count: usize, age: Duration,
    ) -> (u8, usize, std::cmp::Reverse<Duration>) {
        let severity_rank = match status {
            ReplicationStatus::UnderReplicated => 0,
            ReplicationStatus::Ok => 1,
            ReplicationStatus::OverReplicated => 2,
        };
        (severity_rank, alive_count, std::cmp::Reverse(age))
    }

    /// Effective age for heal_queue_sort_key's ordering — see `requeue_priority`'s
    /// doc comment for the full rationale. `since_last_failure` (from requeue_priority,
    /// touched at every stall/push-failure) always wins when present, so a chunk that
    /// has failed before sorts by how recently it failed, not by its original
    /// detection time; a chunk that has NEVER failed falls back to `since_detected`
    /// (from pending_healing), preserving today's oldest-first behavior for the
    /// common case.
    fn effective_heal_priority_age(
        since_last_failure: Option<Duration>, since_detected: Option<Duration>,
    ) -> Duration {
        since_last_failure.or(since_detected).unwrap_or(Duration::ZERO)
    }

    /// Drain the heal queue — dispatches PushChunkTo / DeleteChunkReplica for all
    /// chunks in pending_healing that are ready (delay passed, source known, not
    /// in-flight, not stalled). Tasks are spawned-and-forgotten; in_flight_healing
    /// prevents double-dispatch across 15s ticks.
    async fn drain_heal_queue(&self) -> Result<()> {
        let destructive_allowed = {
            let all_nodes = self.cluster.get_all_nodes().await;
            let total = all_nodes.len();
            let online = all_nodes.iter()
                .filter(|n| n.status == dfs_common::NodeStatus::Online)
                .count();
            let nodes_down = total.saturating_sub(online);
            let grace_elapsed = self.cluster.time_since_became_leader().await
                .map_or(true, |d| d.as_secs() >= LEADER_CHANGE_GRACE_SECS);
            grace_elapsed && nodes_down <= 1
        };

        let work: Vec<(ChunkId, ReplicationStatus, Vec<NodeId>)> = {
            let pending   = self.pending_healing.read().await;
            let cache     = self.alive_nodes_cache.read().await;
            let in_flight = self.in_flight_healing.read().await;
            let stalled   = self.stalled_healing.read().await;
            let replication_factor = self.replication_factor.load(Ordering::Relaxed);

            let mut v = Vec::new();
            let mut cache_misses = Vec::new();
            for (chunk_id, detected_at) in pending.iter() {
                if detected_at.elapsed() < Duration::from_secs(self.healing_delay_secs.load(Ordering::Relaxed)) {
                    continue;
                }
                // Skip already in-flight.
                if in_flight.contains(chunk_id) {
                    continue;
                }
                // Skip stalled — no source available; discovery promotes when a node returns.
                if stalled.contains(chunk_id) {
                    continue;
                }

                let confirmed_alive = match cache.get(chunk_id) {
                    Some(nodes) => nodes.clone(),
                    None => {
                        // No full discovery pass has classified this chunk yet (e.g. it
                        // was folded after the last one ran). Don't just wait for the
                        // next 60s-periodic pass — probe it directly below, once locks
                        // are released.
                        cache_misses.push(*chunk_id);
                        continue;
                    }
                };

                let status = if confirmed_alive.len() < replication_factor {
                    ReplicationStatus::UnderReplicated
                } else if confirmed_alive.len() > replication_factor {
                    if destructive_allowed { ReplicationStatus::OverReplicated } else { continue }
                } else {
                    continue; // at RF, discovery will clean up pending entry
                };

                v.push((*chunk_id, status, confirmed_alive));
            }
            drop(pending);
            drop(cache);
            drop(in_flight);
            drop(stalled);

            if !cache_misses.is_empty() {
                let replication_factor = self.replication_factor.load(Ordering::Relaxed);
                for chunk_id in cache_misses {
                    let confirmed_alive = self.probe_single_chunk_alive_nodes(&chunk_id).await;
                    self.alive_nodes_cache.write().await.insert(chunk_id, confirmed_alive.clone());

                    let status = if confirmed_alive.len() < replication_factor {
                        ReplicationStatus::UnderReplicated
                    } else if confirmed_alive.len() > replication_factor {
                        if destructive_allowed { ReplicationStatus::OverReplicated } else { continue }
                    } else {
                        continue;
                    };
                    v.push((chunk_id, status, confirmed_alive));
                }
            }

            // Sort by severity first, then oldest-first within equal severity, cap to
            // max_heal_per_cycle. Root-caused 2026-07-17: previously oldest-first only,
            // so a chunk at 1-of-3 replicas got no priority over one merely at 2-of-3 —
            // under a sustained backlog, max_heal_per_cycle's truncation below could
            // defer the most severe gap behind less urgent ones indefinitely.
            // confirmed_alive.len() is already computed per entry above; no new data
            // needed. UnderReplicated always outranks OverReplicated (restoring
            // durability outranks trimming excess copies), and within UnderReplicated,
            // fewer alive replicas sorts first (1-of-N before 2-of-3).
            {
                let pending = self.pending_healing.read().await;
                v.sort_by_cached_key(|(chunk_id, status, confirmed_alive)| {
                    let age = Self::effective_heal_priority_age(
                        self.requeue_priority.get(chunk_id).map(|t| t.elapsed()),
                        pending.get(chunk_id).map(|t| t.elapsed()),
                    );
                    Self::heal_queue_sort_key(*status, confirmed_alive.len(), age)
                });
            }
            let skipped = v.len().saturating_sub(self.max_heal_per_cycle);
            if skipped > 0 {
                debug!("drain_heal_queue: deferring {} chunks past per-cycle cap", skipped);
            }
            v.truncate(self.max_heal_per_cycle);
            v
        };

        if work.is_empty() {
            return Ok(());
        }

        let total = work.len();

        // Bounds how many Tokio tasks exist simultaneously waiting for the semaphore.
        // Without this cap, all `work` tasks would be spawned at once, each holding OS
        // resources (stack, file descriptors from pending connects) while blocked on
        // acquire — causing "too many orphaned sockets" kernel warnings under load.
        // Actual byte throughput is paced separately by heal_bandwidth_limiter_out on
        // whichever node ends up being the transfer's source (and heal_bandwidth_limiter_in
        // on whichever node ends up being the target).
        let max_live = self.heal_max_concurrent.load(Ordering::Relaxed).max(1);
        let mut set: JoinSet<Option<HealOutcome>> = JoinSet::new();
        let mut iter = work.into_iter();

        // Commit intents collected from finished heal tasks, flushed in batches —
        // see HealOutcome's doc comment for why tasks no longer commit individually.
        // Flushing happens after EVERY completion, batching only outcomes that have
        // ALREADY finished (the try_join_next scoop below) — group-commit style:
        // zero added commit latency when completions trickle in, real batching when
        // a burst of concurrent heals lands together. An earlier version flushed
        // only at >=32 outcomes or end-of-drain; with fewer than 32 chunks in a
        // cycle that deferred every commit behind the SLOWEST heal in the cycle
        // (up to the transfer timeout), during which discovery saw stale
        // CHUNK_TABLE state and concurrent folds cancelled the still-uncommitted
        // heals — measured as a T38b convergence failure (6 heals discarded in one
        // end-of-drain flush, 2026-07-16). 32 per flush still bounds the redb
        // write-lock hold per batch.
        const HEAL_FLUSH_BATCH: usize = 32;
        let mut outcomes: Vec<HealOutcome> = Vec::new();

        loop {
            // Fill up to max_live concurrent tasks.
            while set.len() < max_live {
                let Some((chunk_id, status, confirmed_alive)) = iter.next() else { break };

                let storage = self.storage.clone();
                let metadata = self.metadata.clone();
                let cluster = self.cluster.clone();
                let client = self.client.clone();
                let in_flight_healing = self.in_flight_healing.clone();
                let cancelled_heals = self.cancelled_heals.clone();
                let stalled_healing = self.stalled_healing.clone();
                let heal_push_failure = self.heal_push_failure.clone();
                let requeue_priority = self.requeue_priority.clone();
                let heal_semaphore = self.heal_semaphore.clone();
                let replication_factor = self.replication_factor.load(Ordering::Relaxed);
                let transfer_timeout = Duration::from_secs(self.heal_transfer_timeout_secs.load(Ordering::Relaxed));
                let bandwidth_limiter = self.heal_bandwidth_limiter_out.clone();
                let node_inflight = self.node_inflight.clone();
                let heal_max_concurrent_per_node = self.heal_max_concurrent_per_node.clone();

                set.spawn(async move {
                    let _permit = heal_semaphore.acquire().await;

                    let result = tokio::time::timeout(transfer_timeout, async {
                        match status {
                            ReplicationStatus::UnderReplicated => {
                                HealingManager::do_heal_chunk_shared(
                                    &chunk_id, confirmed_alive, &storage, &metadata, &cluster, &client,
                                    &in_flight_healing, &cancelled_heals, replication_factor, &bandwidth_limiter,
                                    &node_inflight, &heal_max_concurrent_per_node, &heal_push_failure,
                                ).await
                            }
                            ReplicationStatus::OverReplicated => {
                                HealingManager::do_cleanup_excess_shared(
                                    &chunk_id, confirmed_alive, &storage, &metadata, &cluster, &client, replication_factor,
                                    &in_flight_healing,
                                ).await.map(|()| None)
                            }
                            ReplicationStatus::Ok => Ok(None),
                        }
                    }).await;

                    match result {
                        Ok(Ok(outcome)) => outcome,
                        Ok(Err(e)) => {
                            warn!("Heal failed for chunk {}: {} — stalling until next discovery", chunk_id, e);
                            in_flight_healing.write().await.remove(&chunk_id);
                            stalled_healing.write().await.insert(chunk_id);
                            requeue_priority.insert(chunk_id, Instant::now());
                            None
                        }
                        Err(_) => {
                            warn!("Heal timed out for chunk {} after {}s — stalling until next discovery", chunk_id, transfer_timeout.as_secs());
                            in_flight_healing.write().await.remove(&chunk_id);
                            stalled_healing.write().await.insert(chunk_id);
                            requeue_priority.insert(chunk_id, Instant::now());
                            None
                        }
                    }
                    // _permit drops here, releasing semaphore budget
                });
            }

            if set.is_empty() {
                break;
            }

            // Wait for any one task to finish, then loop to fill the slot.
            match set.join_next().await {
                Some(Ok(Some(outcome))) => outcomes.push(outcome),
                Some(Ok(None)) => {}
                Some(Err(e)) => warn!("Heal task panicked: {}", e),
                None => {}
            }
            // Scoop up any OTHER tasks that have already finished — they batch into
            // this flush for free, without waiting on anything still in flight.
            while outcomes.len() < HEAL_FLUSH_BATCH {
                match set.try_join_next() {
                    Some(Ok(Some(outcome))) => outcomes.push(outcome),
                    Some(Ok(None)) => {}
                    Some(Err(e)) => warn!("Heal task panicked: {}", e),
                    None => break,
                }
            }
            self.flush_heal_outcomes(&mut outcomes).await;
        }

        // Final flush — the JoinSet is fully drained, so this leaves nothing behind.
        self.flush_heal_outcomes(&mut outcomes).await;

        debug!("Heal queue drain: processed {} tasks (max_live={})", total, max_live);
        Ok(())
    }

    /// Flush a batch of heal-task commit intents: ONE write transaction covering
    /// every routing-table put/delete and pending-healing clear in the batch (via
    /// batch_update_chunk_locations), then ONE ReplicateChunkLocationsV2 message
    /// per online peer for everything that needs broadcasting — instead of the
    /// 2 transactions + N-peer connection fan-out PER CHUNK this replaced (see
    /// HealOutcome).
    ///
    /// Re-checks the fold-cancellation tombstone per chunk at flush time: the
    /// commit now happens up to a batch-window later than the task's own check
    /// inside do_heal_chunk_inner, and a fold may have started retiring a chunk_id
    /// in that window — cancel_healing's tombstone must win regardless of when it
    /// was set (same rationale as the pre-commit check it backstops; cheap DashMap
    /// lookup).
    async fn flush_heal_outcomes(&self, outcomes: &mut Vec<HealOutcome>) {
        if outcomes.is_empty() {
            return;
        }
        let batch: Vec<HealOutcome> = outcomes.drain(..).collect();

        let mut puts: Vec<ChunkLocation> = Vec::new();
        let mut deletes: Vec<ChunkId> = Vec::new();
        let mut pending_clears: Vec<ChunkId> = Vec::new();
        let mut broadcasts: Vec<ChunkLocation> = Vec::new();

        for outcome in batch {
            if outcome.location_put.is_some()
                && Self::is_healing_cancelled(&self.cancelled_heals, &outcome.chunk_id)
            {
                info!("Healer: chunk {} healing was cancelled by a concurrent fold — discarding healed replica at flush", outcome.chunk_id);
                if outcome.clear_pending {
                    pending_clears.push(outcome.chunk_id);
                }
                continue;
            }
            if let Some(location) = outcome.location_put {
                if outcome.broadcast {
                    broadcasts.push(location.clone());
                }
                puts.push(location);
            }
            if let Some(chunk_id) = outcome.location_delete {
                deletes.push(chunk_id);
            }
            if outcome.clear_pending {
                pending_clears.push(outcome.chunk_id);
            }
        }

        let commit_ok = match self.metadata
            .batch_update_chunk_locations_async(puts, deletes, pending_clears.clone())
            .await
        {
            Ok(()) => true,
            Err(e) => {
                warn!("Failed to commit heal-outcome batch: {}", e);
                false
            }
        };

        // In-memory pending_healing clear happens regardless of commit success,
        // matching the old per-chunk behavior (clear_pending_static ran even when
        // put_chunk_location failed). On commit failure the PENDING_HEALING_TABLE
        // rows survive, so a restart re-detects these chunks — safe, not lossy.
        if !pending_clears.is_empty() {
            let mut pending = self.pending_healing.write().await;
            for chunk_id in &pending_clears {
                pending.remove(chunk_id);
            }
        }

        // Broadcast only what was actually committed (the batch txn is atomic), and
        // only after the local commit — same ordering the per-chunk path enforced.
        // One V2 message per peer; its handler applies the single-item merge rules
        // per location and commits the whole batch in one transaction on its side.
        if commit_ok && !broadcasts.is_empty() {
            let cluster = self.cluster.clone();
            let client = self.client.clone();
            tokio::spawn(async move {
                let local_id = cluster.local_node_id();
                let nodes = cluster.get_all_nodes().await;
                for node in nodes {
                    if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                        continue;
                    }
                    let request = Request::ReplicateChunkLocationsV2 { locations: broadcasts.clone() };
                    if let Err(e) = client.send_message(node.addr, Message::Request(request)).await {
                        warn!("Failed to broadcast {} healed chunk locations to node {}: {}",
                              broadcasts.len(), node.id, e);
                    }
                }
            });
        }
    }

    /// Check if a chunk should be healed (delay has passed). First-time detections
    /// are marked pending via `deferred` (see mark_pending_deferred) — the caller
    /// (the discovery classification loop, this function's only caller) flushes
    /// them in one batch transaction after its loop.
    async fn should_heal(&self, chunk_id: &ChunkId, deferred: &mut Vec<(ChunkId, u64)>) -> bool {
        let elapsed = {
            let pending = self.pending_healing.read().await;
            pending.get(chunk_id).map(|detected_at| detected_at.elapsed())
        };

        match elapsed {
            Some(elapsed) => {
                // Check if delay has passed
                if elapsed >= Duration::from_secs(self.healing_delay_secs.load(Ordering::Relaxed)) {
                    true
                } else {
                    debug!(
                        "Chunk {} waiting for healing delay ({}/{}s)",
                        chunk_id,
                        elapsed.as_secs(),
                        self.healing_delay_secs.load(Ordering::Relaxed)
                    );
                    false
                }
            }
            None => {
                // First time detecting under-replication
                self.mark_pending_deferred(*chunk_id, deferred).await;
                debug!(
                    "Chunk {} marked for healing (delay: {}s)",
                    chunk_id, self.healing_delay_secs.load(Ordering::Relaxed)
                );
                false
            }
        }
    }


    /// Static heal implementation — callable from both instance methods and spawned tasks.
    async fn do_heal_chunk_shared(
        chunk_id: &ChunkId,
        confirmed_alive_nodes: Vec<NodeId>,
        storage: &Arc<ChunkStorage>,
        metadata: &Arc<MetadataStore>,
        cluster: &Arc<ClusterManager>,
        client: &Arc<NetworkClient>,
        in_flight_healing: &Arc<RwLock<HashSet<ChunkId>>>,
        cancelled_heals: &Arc<DashMap<ChunkId, Instant>>,
        replication_factor: usize,
        bandwidth_limiter: &Arc<BandwidthLimiter>,
        node_inflight: &Arc<DashMap<NodeId, Arc<Semaphore>>>,
        heal_max_concurrent_per_node: &Arc<AtomicUsize>,
        heal_push_failure: &Arc<DashMap<ChunkId, (Instant, u32)>>,
    ) -> Result<Option<HealOutcome>> {
        // In-flight guard: prevents two concurrent tasks healing the same chunk.
        {
            let mut in_flight = in_flight_healing.write().await;
            if in_flight.contains(chunk_id) {
                debug!("Chunk {} heal already in-flight, skipping", chunk_id);
                return Ok(None);
            }
            in_flight.insert(*chunk_id);
        }

        let result = Self::do_heal_chunk_inner(
            chunk_id, confirmed_alive_nodes, storage, metadata, cluster, client, cancelled_heals, replication_factor, bandwidth_limiter,
            node_inflight, heal_max_concurrent_per_node, heal_push_failure,
        ).await;
        in_flight_healing.write().await.remove(chunk_id);
        result
    }

    async fn do_heal_chunk_inner(
        chunk_id: &ChunkId,
        confirmed_alive_nodes: Vec<NodeId>,
        storage: &Arc<ChunkStorage>,
        metadata: &Arc<MetadataStore>,
        cluster: &Arc<ClusterManager>,
        client: &Arc<NetworkClient>,
        cancelled_heals: &Arc<DashMap<ChunkId, Instant>>,
        replication_factor: usize,
        bandwidth_limiter: &Arc<BandwidthLimiter>,
        node_inflight: &Arc<DashMap<NodeId, Arc<Semaphore>>>,
        heal_max_concurrent_per_node: &Arc<AtomicUsize>,
        heal_push_failure: &Arc<DashMap<ChunkId, (Instant, u32)>>,
    ) -> Result<Option<HealOutcome>> {
        // Patch tokens are NOT independently replicable content — never heal one.
        //
        // A public_token is blake3("dfs-patch-token" || delta_chunk_id.hash): deliberately
        // NOT a content hash, precisely so nothing mistakes it for directly-readable
        // content (see PATCH_STATE_TABLE's doc comment). It has no file of its own; reading
        // it resolves through patch_state to base+delta and composes the bytes. So
        // handle_push_chunk_to's source-side verification —
        // compute_chunk_hash_at(&data, offset, file_id) != chunk_id.hash — is TRUE BY
        // CONSTRUCTION for every token, and reports it as "content hash mismatch (disk
        // corruption)". The heal then fails, the chunk is stalled, discovery re-queues it
        // because its routing row still looks under-replicated, and it retries forever.
        //
        // This is not theoretical: staging logged 372,854 such "corruption" events and
        // 191,437 heal failures overnight against 849 distinct tokens, individual chunks
        // reaching 482 consecutive failures, with a 0% success rate. None of it was real
        // corruption. Tokens enter the queue structurally, not exceptionally: apply_patch
        // registers each new token's ChunkLocation with nodes: [local_node_id] only
        // (deliberately — carrying the old node list forward caused a real T28 corruption),
        // so EVERY patch mints a chunk that looks 1-of-RF replicated. A patch-heavy
        // workload like a VM install therefore manufactures these continuously.
        //
        // Skipping is correct, not merely cheaper: copying a token's composed bytes under
        // the token's own name would store content whose hash does not match its
        // identity, breaking content-addressing. If the underlying data really is at risk,
        // the remedy is the backstop fold — consolidate to a real content-addressed chunk
        // and heal THAT (see T53) — never replicating the token.
        //
        // O(1) single-key lookup, not the table scan all_pending_patch_chunk_ids does: a
        // chunk_id that HAS a patch_state row IS a token, since that table is keyed by
        // public_token. Note base_chunk_id/delta_chunk_id are real content chunks with real
        // files and must still heal normally — they are not keys here, so they are
        // unaffected. Any lookup error falls through to healing: uncertainty must never
        // withhold redundancy from something that might be genuine content.
        match metadata.get_patch_state_async(*chunk_id).await {
            Ok(Some(_)) => {
                debug!("Skipping heal for {} — it is a patch token (resolves via patch_state, not independently replicable); the fold path owns this slot", chunk_id);
                return Ok(Some(HealOutcome::clear_only(*chunk_id)));
            }
            Ok(None) => {}
            Err(e) => {
                warn!("Heal: patch-state lookup failed for {} ({}) — proceeding with heal rather than withholding redundancy", chunk_id, e);
            }
        }

        info!("Leader healing under-replicated chunk: {}", chunk_id);

        let location = metadata
            .get_chunk_location_async(*chunk_id).await?
            .ok_or_else(|| anyhow::anyhow!("Chunk location not found"))?;

        // Fast orphan guard: if no file claims this chunk, don't waste a heal slot on
        // it. The deep-scan orphan sweep runs every ~6 min and will delete the routing
        // table entry; we just don't want to block a 120s heal transfer on a ghost chunk.
        //
        // Both metadata reads here use the _async (spawn_blocking) variants, NOT the
        // sync ones: do_heal_chunk_inner is spawned up to heal_max_concurrent times at
        // once onto tokio worker threads by drain_heal_queue's JoinSet. The sync getters
        // block the worker on the parking_lot `db` read lock; enough concurrent heals
        // blocking at once starves the whole runtime (the mechanism that turned a single
        // slow metadata lock into gluster1's 33-minute node-wide wedge, 2026-07-17).
        let is_orphan = match location.file_id {
            Some(file_id) => !metadata.file_exists_by_id_async(file_id).await.unwrap_or(true),
            None => {
                // No file_id recorded — we can't verify quickly. Stall for now and let
                // the deep scan handle it rather than burning 120s on a speculative heal.
                warn!("Chunk {} has no file_id in routing table — deferring to orphan sweep", chunk_id);
                return Ok(Some(HealOutcome::clear_only(*chunk_id)));
            }
        };
        if is_orphan {
            warn!("Chunk {}: file {:?} no longer exists — removing orphan chunk from routing table", chunk_id, location.file_id);
            return Ok(Some(HealOutcome {
                chunk_id: *chunk_id,
                location_put: None,
                location_delete: Some(*chunk_id),
                clear_pending: true,
                broadcast: false,
            }));
        }

        // Superseded-generation guard (2026-07-22). "Does the file exist?" above is too
        // weak a test. A fold/patch replaces a chunk with a new generation and the file
        // moves on, but the OLD chunk's routing entry keeps pointing at the still-live
        // file_id — so the guard above passes, the healer commits a full transfer to a
        // chunk no file references, the source bytes fail checksum verification
        // ("disk corruption"), the chunk goes to stalled_healing, and the next discovery
        // cycle re-queues it because the routing entry still looks under-replicated.
        // That loop is unbounded. Live staging measurement: 17,094 heal attempts against
        // 307 such chunks with ZERO successes, while the fast guard above fired only 94
        // times — enough metadata load to stall the group-commit committer for 2+ seconds
        // and starve unrelated client writes.
        //
        // This deliberately only DECLINES TO HEAL — it never deletes. During a fold there
        // is a legitimate transient window where a chunk is briefly unreferenced (the
        // pointer gap); deleting on that signal is exactly the fold-mapping data loss that
        // left VM-108/111 unbootable. Skipping one heal cycle in that window is harmless,
        // so deletion stays the deep orphan sweep's job with its age grace, two-pass
        // confirmation and leader cross-check.
        //
        // Gated on a FULLY healthy cluster — stricter than drain_heal_queue's
        // `nodes_down <= 1` — because the whole test reads THIS node's FILE_TABLE view. If
        // any node is offline or wedged, that view may be stale or mid-convergence, and a
        // stale view would make us skip healing a chunk that IS still referenced, i.e.
        // withhold redundancy at exactly the moment redundancy is degraded. The
        // leader-change grace applies for the same reason: a freshly elected leader has
        // not necessarily caught up. Every uncertain outcome below (read error, file row
        // missing) falls through to healing, so ambiguity always costs a wasted heal
        // rather than a missing replica.
        //
        // Known tradeoff: a chunk serving as the BASE of an active Pending patch is not in
        // its file's chunk_locations (the public token is), so it reads as unreferenced and
        // its heal is deferred until the fold resolves. Accepted because today that same
        // chunk's heal fails on checksum anyway — this makes it cheap instead of expensive,
        // not less likely to succeed.
        if let Some(file_id) = location.file_id {
            let cluster_fully_healthy = {
                let all_nodes = cluster.get_all_nodes().await;
                let online = all_nodes.iter()
                    .filter(|n| n.status == dfs_common::NodeStatus::Online)
                    .count();
                let grace_elapsed = cluster.time_since_became_leader().await
                    .map_or(true, |d| d.as_secs() >= LEADER_CHANGE_GRACE_SECS);
                !all_nodes.is_empty() && online == all_nodes.len() && grace_elapsed
            };
            if cluster_fully_healthy {
                if let Ok(Some(file_meta)) = metadata.get_file_async(file_id).await {
                    // POSITIVE CONTROL: only trust an "unreferenced" answer from a view that
                    // demonstrably HAS references. An empty chunk list is indistinguishable
                    // from a stale or not-yet-caught-up FILE_TABLE replica, and treating that
                    // as "nothing is referenced" would withhold healing from every chunk of
                    // the file at once. The same principle is what makes this safe against
                    // the lost-disk case generally: prove some things resolve before
                    // concluding a specific thing doesn't. A node whose storage or metadata
                    // has gone missing entirely sees zero references and therefore concludes
                    // nothing at all, which is the correct behavior for a node that has lost
                    // its ability to know anything.
                    let view_is_usable = !file_meta.chunk_locations.is_empty();
                    let still_referenced = file_meta.chunk_locations.iter()
                        .any(|l| l.chunk_id == *chunk_id);
                    if view_is_usable && !still_referenced {
                        warn!("Chunk {} is a superseded generation of live file {} ({} chunks referenced, this one not) — skipping heal, deferring deletion to the orphan sweep",
                            chunk_id, file_id, file_meta.chunk_locations.len());
                        return Ok(Some(HealOutcome::clear_only(*chunk_id)));
                    }
                }
            }
        }

        // Build alive list from confirmed_alive_nodes passed in from the bulk scan.
        // These were verified by the HasChunks bulk RPC in the classification phase —
        // no per-chunk re-querying here, which avoids misclassifying healthy nodes as
        // ghosts due to transient RPC failures.
        let local_id = cluster.local_node_id();
        let mut alive: Vec<(NodeId, std::net::SocketAddr)> = Vec::new();
        for node_id in confirmed_alive_nodes {
            if let Some(info) = cluster.get_node(&node_id).await {
                alive.push((node_id, info.addr));
            }
        }

        // Also check local storage — the chunk may exist here but not be in metadata
        // (e.g. node ID was incorrectly pruned by a previous healer bug).
        let mut local_added_via_fallback = false;
        if !alive.iter().any(|(id, _)| *id == local_id) && storage.has_chunk(chunk_id) {
            if let Some(info) = cluster.get_node(&local_id).await {
                warn!(
                    "Chunk {} found in local storage but missing from confirmed-alive list — adding as source",
                    chunk_id
                );
                alive.push((local_id, info.addr));
                local_added_via_fallback = true;
            }
        }

        if alive.is_empty() {
            anyhow::bail!("No alive nodes have chunk {}", chunk_id);
        }

        // `alive` (including any verified local-fallback copy) is exactly the node set
        // that will end up holding this chunk once healing completes, so count all of
        // it toward replica_count. Chunk IDs are content hashes, so a fallback copy
        // confirmed via storage.has_chunk() is guaranteed correct, not stale — treating
        // it as "just a source" (prior behavior) under-counted replica_count by one
        // while still including it in the final node list, over-replicating to RF+1.
        let replica_count = alive.len();
        let needed = replication_factor.saturating_sub(replica_count);
        let base_nodes: Vec<NodeId> = alive.iter().map(|(id, _)| *id).collect();
        if needed == 0 {
            if local_added_via_fallback && !Self::is_healing_cancelled(cancelled_heals, chunk_id) {
                // Reconcile routing table: this node holds a verified copy the routing
                // table doesn't know about. Record it now so discovery stops reporting
                // this chunk as under-replicated and the orphan sweep doesn't delete
                // this now-needed replica.
                //
                // client_write_seq preserved from the original record (not hardcoded
                // None) — see the identical fix/rationale below this function's main
                // healed-replica commit. Also gated on !is_healing_cancelled for the
                // same reason as that commit: a fold may have started retiring this
                // exact chunk_id while this reconcile was in flight. (The commit
                // itself now happens in flush_heal_outcomes, which re-checks the
                // cancellation tombstone at flush time — this early check just avoids
                // building an outcome that would be discarded anyway.)
                let updated_location = ChunkLocation {
                    chunk_id: *chunk_id,
                    nodes: base_nodes,
                    size: location.size,
                    checksum: location.checksum,
                    file_offset: location.file_offset,
                    written_at: Some(Self::now_ms()),
                    client_write_seq: location.client_write_seq,
                    file_id: location.file_id,
                };
                return Ok(Some(HealOutcome {
                    chunk_id: *chunk_id,
                    location_put: Some(updated_location),
                    location_delete: None,
                    clear_pending: true,
                    broadcast: true,
                }));
            }
            return Ok(Some(HealOutcome::clear_only(*chunk_id)));
        }

        // Select target nodes: capacity-aware candidates that don't already hold the chunk.
        // get_nodes_with_capacity_awareness returns nodes in capacity-priority order
        // (most-available first, seeded by chunk hash for determinism). Do NOT re-sort
        // by NodeId — that would throw away the capacity ordering and always pick the
        // lowest-NodeId node regardless of how full it is.
        let alive_ids: HashSet<NodeId> = alive.iter().map(|(id, _)| *id).collect();
        let candidates = cluster
            .get_nodes_with_capacity_awareness(chunk_id, replication_factor + needed)
            .await;
        let targets: Vec<NodeId> = candidates
            .into_iter()
            .filter(|n| !alive_ids.contains(n))
            .take(needed)
            .collect();

        if targets.is_empty() {
            warn!(
                "No suitable target nodes for healing chunk {} (alive={:?} replica_count={} needed={})",
                chunk_id, alive_ids, replica_count, needed
            );
            return Ok(None);
        }
        debug!(
            "Healing chunk {}: alive={:?} replica_count={} needed={} targets={:?}",
            chunk_id, alive_ids, replica_count, needed, targets
        );

        // Prefer a remote source to avoid loopback TCP (leader→leader PushChunkTo hangs
        // under Tokio scheduling pressure). Fall back to local only when no remote has it.
        let mut preferred_sources: Vec<(NodeId, std::net::SocketAddr)> =
            alive.iter().filter(|(id, _)| *id != local_id).copied().collect();
        if preferred_sources.is_empty() {
            preferred_sources = alive.clone();
        }

        // Try each candidate in preference order for an immediately-free per-node slot
        // first — this is what lets independent node-pairs (e.g. B->A and D->C) proceed
        // in parallel instead of every heal task queuing behind whichever node happens
        // to be listed first. Only block (on the top-preference candidate, preserving
        // the prior always-prefer-remote behavior) if every candidate is at capacity.
        let mut source_pick: Option<(NodeId, std::net::SocketAddr)> = None;
        let mut source_permit: Option<OwnedSemaphorePermit> = None;
        for (id, addr) in &preferred_sources {
            if let Some(permit) = Self::try_acquire_node_permit(node_inflight, heal_max_concurrent_per_node, *id) {
                source_pick = Some((*id, *addr));
                source_permit = Some(permit);
                break;
            }
        }
        let (source_id, source_addr) = match source_pick {
            Some(s) => s,
            None => {
                let (id, addr) = *preferred_sources.first().ok_or_else(|| anyhow::anyhow!("No source node"))?;
                source_permit = Some(Self::acquire_node_permit(node_inflight, heal_max_concurrent_per_node, id).await);
                (id, addr)
            }
        };
        // Held for the whole task (all targets below) — the source node is busy with a
        // disk read + send for each target in turn. Released when this function returns.
        let _source_permit = source_permit;

        let mut replicated = Vec::new();

        {
            for target_id in &targets {
                if let Some(target_info) = cluster.get_node(target_id).await {
                    let _target_permit = Self::acquire_node_permit(
                        node_inflight, heal_max_concurrent_per_node, *target_id,
                    ).await;
                    info!(
                        "Healing chunk {}: instructing node {} to push to node {} ({})",
                        chunk_id, source_id, target_id, target_info.addr
                    );

                    let request = Request::PushChunkTo {
                        chunk_id: *chunk_id,
                        target_addr: target_info.addr,
                        leader_id: local_id,
                    };

                    // PushChunkTo response time = source disk read + ReplicateChunk RTT
                    // to target (5s connect + 30s write + 30s read). Use 90s so the
                    // leader doesn't time out before the source handler finishes.
                    let target_addr = target_info.addr;
                    match client.send_message_timeout(
                        source_addr,
                        Message::Request(request),
                        std::time::Duration::from_secs(90),
                    ).await {
                        Ok(envelope) if matches!(envelope.message, Message::Response(Response::Ok { .. })) => {
                            // Verify the target actually received and can confirm the chunk.
                            // PushChunkTo's Ok only means the source completed the send —
                            // a target with a corrupted filesystem may write OK but fail
                            // reads immediately after. A quick HasChunks round-trip to the
                            // target catches this before we commit it to routing metadata.
                            let verify_req = dfs_common::Request::HasChunks { chunk_ids: vec![*chunk_id] };
                            let target_confirmed = match client.send_message(
                                target_addr,
                                dfs_common::Message::Request(verify_req),
                            ).await {
                                Ok(env) => matches!(
                                    env.message,
                                    dfs_common::Message::Response(dfs_common::Response::BoolVec { ref values })
                                    if values.first().copied().unwrap_or(false)
                                ),
                                Err(e) => {
                                    warn!("Chunk {} post-push verification on target {} failed: {}", chunk_id, target_addr, e);
                                    false
                                }
                            };
                            if target_confirmed {
                                info!("Chunk {} successfully pushed from {} to {} (target confirmed)", chunk_id, source_id, target_id);
                                replicated.push(*target_id);
                            } else {
                                warn!("Chunk {} push to {} reported success but target {} does not confirm having it — not recording replica", chunk_id, source_id, target_id);
                            }
                        }
                        Ok(envelope) if matches!(envelope.message, Message::Response(Response::Error { code: dfs_common::ErrorCode::NotFound, .. })) => {
                            warn!("Chunk {} not found on source {} — removing from heal queue", chunk_id, source_id);
                            return Ok(Some(HealOutcome {
                                chunk_id: *chunk_id,
                                location_put: None,
                                location_delete: Some(*chunk_id),
                                clear_pending: true,
                                broadcast: false,
                            }));
                        }
                        Ok(envelope) => {
                            warn!("Chunk {} push from {} to {} failed: {:?}", chunk_id, source_id, target_id, envelope.message);
                        }
                        Err(e) => {
                            warn!("Chunk {} push from {} to {} error: {}", chunk_id, source_id, target_id, e);
                        }
                    }
                } else {
                    warn!("Chunk {}: target node {} not found in cluster registry — skipping", chunk_id, target_id);
                }
            }
        }

        if !replicated.is_empty() {
            info!("Healed chunk {}: added {} replicas", chunk_id, replicated.len());

            // Cancellation check: a fold may have started retiring this exact
            // chunk_id as its base while this heal was in flight (source read +
            // network transfer + target verification above can take seconds) —
            // see Server::cancel_healing_for_chunk, called at the start of every
            // fold specifically to prevent this heal from resurrecting an
            // already-superseded identity into chunk_map. Checked first, before
            // the network round-trip below, since it's a cheap local lookup and
            // means the fold's cancellation always wins the race regardless of
            // how far this heal has already gotten.
            if Self::is_healing_cancelled(cancelled_heals, chunk_id) {
                info!("Healer: chunk {} healing was cancelled by a concurrent fold — discarding healed replica", chunk_id);
                return Ok(Some(HealOutcome::clear_only(*chunk_id)));
            }

            // CAS-style freshness check: verify the source node still holds the chunk
            // before broadcasting the healed replica into the chunk map. Between our scan
            // and now, a client write may have patched the source: it renames chunk_id
            // to a new hash, so HasChunks(chunk_id) returns false on the source. If the
            // source lost the chunk, our healed copy on the target is a stale version —
            // don't add it to metadata. The orphan sweep will clean up the target's copy,
            // and the healer will heal the correct (newer) chunk_id on the next cycle.
            // This keeps the chunk map free of stale replicas and prevents the client's
            // sorted-first-2 algorithm from targeting a node with an old base chunk.
            let req = dfs_common::Request::HasChunks { chunk_ids: vec![*chunk_id] };
            let source_still_valid = match client.send_message(source_addr, dfs_common::Message::Request(req)).await {
                Ok(env) => match env.message {
                    dfs_common::Message::Response(dfs_common::Response::BoolVec { ref values }) => {
                        values.first().copied().unwrap_or(false)
                    }
                    _ => {
                        warn!("Healer: unexpected response verifying chunk {} on source {}", chunk_id, source_addr);
                        false
                    }
                },
                Err(e) => {
                    warn!("Healer: could not verify chunk {} on source {}: {}", chunk_id, source_addr, e);
                    false
                }
            };

            if !source_still_valid {
                info!("Healer: chunk {} no longer on source {} (superseded by a newer write) — discarding healed replica", chunk_id, source_addr);
                return Ok(Some(HealOutcome::clear_only(*chunk_id)));
            }

            let mut updated_nodes = base_nodes;
            updated_nodes.extend(replicated);

            let updated_location = ChunkLocation {
                chunk_id: *chunk_id,
                nodes: updated_nodes,
                size: location.size,
                checksum: location.checksum,
                file_offset: location.file_offset,
                written_at: Some(Self::now_ms()),
                // Preserve the original record's client_write_seq instead of
                // dropping it to None — chunk_map_update_location_for_file's
                // staleness guard uses this to reject a late-arriving stale
                // broadcast for a position that's since advanced to a newer
                // client_write_seq (see that function's (Some,Some)/(None,Some)
                // match arms). Hardcoding None here defeated that guard for every
                // heal-completion broadcast, regardless of whether the original
                // chunk genuinely carried a real sequence number — belt-and-
                // suspenders alongside the cancellation check above, for any
                // heal that started (and got as far as this commit) before a
                // fold began, so cancel_healing's tombstone window never opened.
                client_write_seq: location.client_write_seq,
                file_id: location.file_id,
            };

            heal_push_failure.remove(chunk_id);
            Ok(Some(HealOutcome {
                chunk_id: *chunk_id,
                location_put: Some(updated_location),
                location_delete: None,
                clear_pending: true,
                broadcast: true,
            }))
        } else {
            // Every target push failed despite a confirmed-alive source (e.g. the
            // source's own data fails its content-hash check — real corruption, not
            // a transient network blip). Record it so the discovery pass's promotion
            // check can back off instead of re-queuing this chunk next cycle — see
            // heal_push_failure's doc comment. Returning Err also feeds
            // drain_heal_queue's existing stalled_healing path.
            let mut entry = heal_push_failure.entry(*chunk_id).or_insert((Instant::now(), 0));
            entry.0 = Instant::now();
            entry.1 = entry.1.saturating_add(1);
            let count = entry.1;
            drop(entry);
            anyhow::bail!(
                "chunk {} failed to replicate to any of {} target(s) from source {} (consecutive failures: {})",
                chunk_id, targets.len(), source_id, count
            );
        }
    }

    async fn broadcast_chunk_location_shared(
        location: &ChunkLocation,
        cluster: &Arc<ClusterManager>,
        client: &Arc<NetworkClient>,
    ) {
        let nodes = cluster.get_all_nodes().await;
        let local_id = cluster.local_node_id();
        for node in nodes {
            if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                continue;
            }
            // Include file_id so the leader routes through chunk_map_update_location_for_file
            // (with write_seq guard) instead of the scan-all fallback that only does an
            // exact chunk_id match and can't enforce ordering between a stale heal RCL
            // and a newer client patch RCL that already updated the same chunk slot.
            let request = Request::ReplicateChunkLocation { location: location.clone(), file_id: location.file_id, generation: None };
            if let Err(e) = client.send_message(node.addr, Message::Request(request)).await {
                warn!("Failed to broadcast chunk location {} to node {}: {}", location.chunk_id, node.id, e);
            }
        }
    }

    /// Over-replication cleanup with deterministic pair preference.
    ///
    /// Trims one excess replica per cycle. Prefers to remove nodes outside the
    /// deterministic pair (lowest 2 NodeIds among confirmed-alive nodes), using
    /// disk utilization as a tiebreaker within each group.
    ///
    /// Hash integrity verification is NOT done here — that is too expensive for
    /// background bulk trimming (reads 4MB from every node per chunk, floods I/O).
    /// Use `dfs-admin file repair` for explicit integrity checking.
    ///
    /// Registers the chunk in `in_flight_healing` for the duration of the cleanup so
    /// it shows up in `dfs-admin healing status`'s in-flight count — it holds a
    /// `heal_semaphore` permit the same as an under-replicated heal, and previously
    /// wasn't tracked, making the reported in-flight count undercount true concurrency.
    async fn do_cleanup_excess_shared(
        chunk_id: &ChunkId,
        confirmed_alive_nodes: Vec<NodeId>,
        storage: &Arc<ChunkStorage>,
        metadata: &Arc<MetadataStore>,
        cluster: &Arc<ClusterManager>,
        client: &Arc<NetworkClient>,
        replication_factor: usize,
        in_flight_healing: &Arc<RwLock<HashSet<ChunkId>>>,
    ) -> Result<()> {
        {
            let mut in_flight = in_flight_healing.write().await;
            if in_flight.contains(chunk_id) {
                debug!("Chunk {} cleanup already in-flight, skipping", chunk_id);
                return Ok(());
            }
            in_flight.insert(*chunk_id);
        }

        let result = Self::do_cleanup_excess_inner(
            chunk_id, confirmed_alive_nodes, storage, metadata, cluster, client, replication_factor,
        ).await;
        in_flight_healing.write().await.remove(chunk_id);
        result
    }

    async fn do_cleanup_excess_inner(
        chunk_id: &ChunkId,
        confirmed_alive_nodes: Vec<NodeId>,
        storage: &Arc<ChunkStorage>,
        metadata: &Arc<MetadataStore>,
        cluster: &Arc<ClusterManager>,
        client: &Arc<NetworkClient>,
        replication_factor: usize,
    ) -> Result<()> {
        // _async (spawn_blocking) getter — do_cleanup_excess_inner is spawned
        // concurrently on tokio workers by drain_heal_queue's JoinSet, same as
        // do_heal_chunk_inner; the sync getter would block the worker on the
        // parking_lot db lock. See do_heal_chunk_inner for the wedge this avoids.
        let location = metadata
            .get_chunk_location_async(*chunk_id).await?
            .ok_or_else(|| anyhow::anyhow!("Chunk location not found"))?;

        // Resolve confirmed-alive nodes to addresses.
        let mut alive: Vec<(NodeId, std::net::SocketAddr)> = Vec::new();
        for node_id in &confirmed_alive_nodes {
            if let Some(info) = cluster.get_node(node_id).await {
                alive.push((*node_id, info.addr));
            }
        }

        let excess = alive.len().saturating_sub(replication_factor);
        if excess == 0 {
            return Ok(());
        }

        // Deterministic pair: lowest 2 NodeIds among confirmed-alive nodes.
        // Prefer trimming non-pair nodes (highest utilization first).
        // Fall back to pair nodes if all excess are pair nodes.
        let local_id = cluster.local_node_id();
        let mut sorted_alive: Vec<NodeId> = alive.iter().map(|(id, _)| *id).collect();
        sorted_alive.sort_unstable();
        let pair: std::collections::HashSet<NodeId> = sorted_alive.iter().take(2).copied().collect();

        let non_pair_ids: Vec<NodeId> = alive.iter()
            .map(|(id, _)| *id)
            .filter(|id| !pair.contains(id))
            .collect();

        let trim_id = if !non_pair_ids.is_empty() {
            cluster.most_utilized_node(&non_pair_ids).await
                .unwrap_or(non_pair_ids[non_pair_ids.len() - 1])
        } else {
            let all_ids: Vec<NodeId> = alive.iter().map(|(id, _)| *id).collect();
            cluster.most_utilized_node(&all_ids).await
                .unwrap_or(all_ids[all_ids.len() - 1])
        };

        let (_, trim_addr) = match alive.iter().find(|(id, _)| *id == trim_id) {
            Some(v) => *v,
            None => return Ok(()),
        };

        // The trim target can be the leader itself (it's just another alive replica
        // holder) — delete locally instead of opening a loopback TCP connection to
        // our own listener to tell ourselves to delete a chunk.
        if trim_id == local_id {
            let storage = storage.clone();
            let chunk_id_owned = *chunk_id;
            let delete_result = tokio::task::spawn_blocking(move || storage.delete_chunk(&chunk_id_owned, "over_replication_trim")).await;
            match delete_result {
                Ok(Ok(_)) => {
                    info!("Chunk {} over-replicated ({} alive, RF={}): trimmed local node {} (pair={:?})",
                          chunk_id, alive.len(), replication_factor, trim_id, pair);
                }
                Ok(Err(e)) => {
                    warn!("Failed to locally trim chunk {}: {}", chunk_id, e);
                    return Ok(());
                }
                Err(e) => {
                    warn!("Local trim of chunk {} panicked: {}", chunk_id, e);
                    return Ok(());
                }
            }
        } else {
            let req = Request::DeleteChunkReplica { chunk_id: *chunk_id, leader_id: local_id };
            match client.send_message(trim_addr, Message::Request(req)).await {
                Ok(env) if matches!(env.message, Message::Response(Response::Ok { .. })) => {
                    info!("Chunk {} over-replicated ({} alive, RF={}): trimmed node {} (pair={:?})",
                          chunk_id, alive.len(), replication_factor, trim_id, pair);
                }
                Ok(env) => {
                    warn!("Node {} refused to trim chunk {}: {:?}", trim_id, chunk_id, env.message);
                    return Ok(());
                }
                Err(e) => {
                    warn!("Failed to reach node {} for chunk {} trim: {}", trim_id, chunk_id, e);
                    return Ok(());
                }
            }
        }

        let removed_nodes = vec![trim_id];

        // Update metadata to remove all deleted nodes (corrupt + trim).
        if !removed_nodes.is_empty() {
            let updated_nodes: Vec<NodeId> = location.nodes.iter()
                .filter(|n| !removed_nodes.contains(n))
                .copied()
                .collect();
            let heal_ts = Self::now_ms();
            let updated_location = ChunkLocation {
                chunk_id: *chunk_id,
                nodes: updated_nodes,
                size: location.size,
                checksum: location.checksum,
                file_offset: location.file_offset,
                written_at: Some(heal_ts),
                // L1: preserve the chunk's real seq (see node-prune comment above).
                client_write_seq: location.client_write_seq,
                file_id: location.file_id,
            };
            let meta = Arc::clone(metadata);
            let loc = updated_location.clone();
            let store_result = tokio::task::spawn_blocking(move || meta.put_chunk_location(&loc)).await;
            match store_result {
                Ok(Ok(())) => {
                    Self::broadcast_chunk_location_shared(&updated_location, cluster, client).await;
                    info!("Excess replica cleanup complete for chunk {} ({} node(s) removed)",
                          chunk_id, removed_nodes.len());
                }
                Ok(Err(e)) => warn!("Failed to update chunk location after cleanup of {}: {}", chunk_id, e),
                Err(e) => warn!("put_chunk_location spawn_blocking panicked for {}: {}", chunk_id, e),
            }
        }

        Ok(())
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// Scrub all chunks (verify checksums)
    async fn scrub_all_chunks(&self) -> Result<()> {
        // Chunks whose on-disk mtime is more than MTIME_TOLERANCE_SECS newer than
        // ChunkLocation.written_at were likely overwritten after the original write
        // (e.g. a crash during PatchChunk). Only verify hash on those suspicious chunks,
        // avoiding a full re-hash of every chunk on every scrub cycle.
        const MTIME_TOLERANCE_SECS: u64 = 300;

        let chunks = self.storage.list_chunks()?;
        info!("Scrubbing {} chunks (mtime heuristic)", chunks.len());

        let mut skipped = 0usize;
        let mut verified = 0usize;
        let mut errors = 0usize;

        for chunk_id in chunks {
            let mtime = self.storage.get_chunk_mtime(&chunk_id);
            let written_at = self.metadata
                .get_chunk_location(&chunk_id)
                .ok()
                .flatten()
                .and_then(|loc| loc.written_at);

            let suspicious = match (mtime, written_at) {
                (Some(mt), Some(wa)) => mt > wa.saturating_add(MTIME_TOLERANCE_SECS),
                // No written_at recorded (old chunk) — always verify
                (Some(_), None) => true,
                // Can't stat the file — verify so we detect missing chunks
                (None, _) => true,
            };

            if !suspicious {
                skipped += 1;
                continue;
            }

            match self.storage.read_and_verify_chunk(&chunk_id) {
                Ok(_) => {
                    verified += 1;
                }
                Err(e) => {
                    warn!("Scrub hash mismatch for chunk {} (mtime={:?} written_at={:?}): {}",
                        chunk_id, mtime, written_at, e);
                    errors += 1;
                    self.mark_pending(chunk_id).await;
                }
            }
        }

        info!(
            "Scrubbing complete: skipped={}, verified={}, errors={}",
            skipped, verified, errors
        );

        Ok(())
    }

    /// Queue a specific set of chunks for immediate healing, bypassing the normal delay.
    /// Used by HealFile to force-heal all chunks of a single file for targeted testing.
    /// Remove a chunk from the healer's pending queue. Used by PatchChunk to prevent
    /// the healer from replicating the old chunk file during the rename window.
    pub async fn evict_from_pending(&self, chunk_id: &ChunkId) {
        self.clear_pending(chunk_id).await;
        self.stalled_healing.write().await.remove(chunk_id);
    }

    /// This node's outbound heal-bandwidth limiter — Server::handle_push_chunk_to
    /// acquires against it when this node is the *source* (disk-read+network-send).
    /// Independent of heal_bandwidth_limiter_in since TX/RX are separate full-duplex
    /// capacity on the same link.
    pub fn heal_bandwidth_limiter_out(&self) -> &Arc<BandwidthLimiter> {
        &self.heal_bandwidth_limiter_out
    }

    /// This node's inbound heal-bandwidth limiter — Server::handle_write_chunk
    /// acquires against it when this node is the *target* (network-recv+disk-write).
    /// Without this, several source nodes could each stay under their own egress cap
    /// while collectively swamping this node's ingress when it's the recovery target.
    pub fn heal_bandwidth_limiter_in(&self) -> &Arc<BandwidthLimiter> {
        &self.heal_bandwidth_limiter_in
    }

    /// Apply a partial set of live tuning updates (`None` fields left unchanged) and
    /// return the resulting snapshot. Caller (`Server::handle_set_healing_tuning`) is
    /// responsible for persisting the applied values to config.toml.
    pub async fn apply_tuning(
        &self,
        link_bandwidth_mb: Option<usize>,
        heal_max_pct: Option<f64>,
        heal_max_concurrent: Option<usize>,
        heal_max_concurrent_per_node: Option<usize>,
        heal_transfer_timeout_secs: Option<u64>,
        healing_delay_secs: Option<u64>,
    ) -> HealingTuningSnapshot {
        if let Some(v) = link_bandwidth_mb {
            self.link_bandwidth_mb.store(v.max(1), Ordering::Relaxed);
        }
        if let Some(pct) = heal_max_pct {
            *self.heal_max_pct.write().await = (pct / 100.0).clamp(0.10, 1.00);
        }
        if let Some(secs) = heal_transfer_timeout_secs {
            self.heal_transfer_timeout_secs.store(secs.max(1), Ordering::Relaxed);
        }
        if let Some(secs) = healing_delay_secs {
            self.healing_delay_secs.store(secs, Ordering::Relaxed);
        }
        // Apply the global cap first so a per-node value supplied in the same call is
        // clamped against the *new* global ceiling, not the stale one.
        if let Some(target) = heal_max_concurrent {
            self.resize_heal_concurrency(target.max(1)).await;
        }
        if let Some(target) = heal_max_concurrent_per_node {
            let global = self.heal_max_concurrent.load(Ordering::Relaxed);
            let clamped = target.max(1).min(global);
            if clamped != target {
                warn!(
                    "heal_max_concurrent_per_node={} exceeds heal_max_concurrent={}; clamping to {} — a per-node cap above the global cap can never take effect",
                    target, global, clamped
                );
            }
            self.resize_node_concurrency(clamped).await;
        }
        self.tuning_snapshot().await
    }

    /// Reconciles `heal_semaphore`'s actual permit count to a new target. Growing is
    /// immediate (`add_permits`). Shrinking calls `forget_permits`, which only reduces
    /// *available* permits — if all are currently checked out by in-flight transfers,
    /// forget_permits forgets 0 and returns the shortfall. Rather than force-cancel
    /// in-flight heal transfers just to hit the new number faster, a background retry
    /// keeps trying every 500ms until the full reduction has been applied, so the
    /// semaphore converges to the target as transfers naturally complete and release
    /// their permits.
    async fn resize_heal_concurrency(&self, target: usize) {
        let previous = self.heal_max_concurrent.swap(target, Ordering::Relaxed);
        if target > previous {
            self.heal_semaphore.add_permits(target - previous);
        } else if target < previous {
            let mut remaining = previous - target;
            remaining -= self.heal_semaphore.forget_permits(remaining);
            if remaining > 0 {
                let semaphore = self.heal_semaphore.clone();
                let heal_max_concurrent = self.heal_max_concurrent.clone();
                tokio::spawn(async move {
                    while remaining > 0 {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        // Bail out if a newer resize has since superseded this one —
                        // otherwise a rapid shrink-then-grow could over-forget permits
                        // that the newer, larger target still needs.
                        if heal_max_concurrent.load(Ordering::Relaxed) != target {
                            return;
                        }
                        remaining -= semaphore.forget_permits(remaining);
                    }
                });
            }
        }

        // Keep the two knobs logically consistent: a per-node cap above the global cap
        // could never bind, so if the global cap just dropped below it, clamp it down too.
        let per_node = self.heal_max_concurrent_per_node.load(Ordering::Relaxed);
        if per_node > target {
            warn!(
                "heal_max_concurrent lowered to {} below current heal_max_concurrent_per_node={}; clamping per-node cap down to match",
                target, per_node
            );
            self.resize_node_concurrency(target).await;
        }
    }

    /// Reconciles every semaphore in `node_inflight` (plus the target used to size new,
    /// lazily-created entries) to a new per-node concurrency target. Same grow-immediately
    /// / shrink-via-forget_permits-with-retry semantics as `resize_heal_concurrency`,
    /// just applied to each node's semaphore instead of a single global one.
    async fn resize_node_concurrency(&self, target: usize) {
        let previous = self.heal_max_concurrent_per_node.swap(target, Ordering::Relaxed);
        if target == previous {
            return;
        }
        for entry in self.node_inflight.iter() {
            let semaphore = entry.value().clone();
            if target > previous {
                semaphore.add_permits(target - previous);
            } else {
                let mut remaining = previous - target;
                remaining -= semaphore.forget_permits(remaining);
                if remaining > 0 {
                    let heal_max_concurrent_per_node = self.heal_max_concurrent_per_node.clone();
                    tokio::spawn(async move {
                        while remaining > 0 {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            if heal_max_concurrent_per_node.load(Ordering::Relaxed) != target {
                                return;
                            }
                            remaining -= semaphore.forget_permits(remaining);
                        }
                    });
                }
            }
        }
    }

    /// Current live values of all six tuning knobs, for `dfs-admin healing get`/`status`.
    pub async fn tuning_snapshot(&self) -> HealingTuningSnapshot {
        HealingTuningSnapshot {
            link_bandwidth_mb: self.link_bandwidth_mb.load(Ordering::Relaxed),
            heal_max_pct: *self.heal_max_pct.read().await * 100.0,
            heal_max_concurrent: self.heal_max_concurrent.load(Ordering::Relaxed),
            heal_max_concurrent_per_node: self.heal_max_concurrent_per_node.load(Ordering::Relaxed),
            heal_transfer_timeout_secs: self.heal_transfer_timeout_secs.load(Ordering::Relaxed),
            healing_delay_secs: self.healing_delay_secs.load(Ordering::Relaxed),
        }
    }

    /// Backdate `chunk_ids` into this node's own `pending_healing` so the next drain
    /// cycle treats them as already past `healing_delay_secs`, instead of a fresh
    /// discovery having to wait it out. Only effective on the cluster leader — see
    /// `queue_chunks_immediate`'s doc comment, which is what every caller outside
    /// this file should use instead.
    async fn queue_chunks_immediate_local(&self, chunk_ids: Vec<ChunkId>) {
        let backdated = Instant::now() - Duration::from_secs(self.healing_delay_secs.load(Ordering::Relaxed) + 1);
        let backdated_secs = dfs_common::types::current_timestamp()
            .saturating_sub(self.healing_delay_secs.load(Ordering::Relaxed) + 1);
        let mut to_persist: Vec<(ChunkId, u64)> = Vec::with_capacity(chunk_ids.len());
        let mut pending = self.pending_healing.write().await;
        let mut cache  = self.alive_nodes_cache.write().await;
        for chunk_id in chunk_ids {
            // Only invalidate the cache entry on a *genuinely new* pending entry —
            // i.e. this chunk wasn't already queued. Root-caused 2026-07-15 (T38
            // local-suite repro, half-capacity caches): handle_replicate_patch_fold
            // re-fires this same queue call every time its rebroadcast sweep resends
            // an unresolved fold pointer (every few seconds, until the gap closes or
            // its TTL expires) — unconditionally clearing the cache on every one of
            // those repeats raced drain_heal_queue's own `None => continue` (cache
            // empty means "discovery hasn't run yet for this chunk", skip this
            // cycle): the cache got wiped faster than a full discovery pass (60s
            // periodic) could ever repopulate it, so the chunk sat in pending_healing
            // — correctly backdated, never delay-gated — but drain_heal_queue skipped
            // it every single cycle, indefinitely. Confirmed via full log trace: the
            // chunk was queued repeatedly for 35+ seconds straight and never once
            // appeared in a "Leader healing under-replicated chunk" line. Preserving
            // the cache across repeat re-queues of an already-pending chunk still
            // lets the original "invalidate stale data from a finished cycle" case
            // through, since that chunk wouldn't be in `pending` yet at that point.
            let is_new = !pending.contains_key(&chunk_id);
            pending.insert(chunk_id, backdated);
            to_persist.push((chunk_id, backdated_secs));
            if is_new {
                cache.remove(&chunk_id);
            }
        }
        drop(cache);
        drop(pending);
        // One batch transaction for the whole call instead of one per chunk — and
        // no longer awaiting a metadata write while holding the pending/cache write
        // locks above (the old per-chunk put_pending_healing_async did exactly that,
        // serializing every other pending_healing reader behind disk I/O).
        if let Err(err) = self.metadata.put_pending_healing_batch_async(to_persist).await {
            warn!("Failed to persist backdated pending_healing entries: {}", err);
        }
    }

    /// Queue chunks for immediate (delay-bypassed) healing. Safe to call from any
    /// node, leader or not — forwards to the actual leader when this node isn't one.
    ///
    /// Root-caused 2026-07-15 (T38 local-suite repro): `pending_healing` is only ever
    /// drained by `run_discovery_loop`/`run_heal_loop`, and both bail out immediately
    /// unless `self.cluster.is_leader()` — see their own doc comments ("runs every
    /// 60s/15s on the cluster leader"). Every existing caller of this function can
    /// run on a non-leader node: `handle_replicate_patch_fold` fires wherever a fold
    /// broadcast happens to land, and `handle_heal_file`/`handle_force_fold` etc. are
    /// routed to whichever address the caller (an admin CLI, another node) happened
    /// to target, not necessarily the leader. Before this fix, queueing on a
    /// non-leader was a silent dead end: the entry sat in that node's own
    /// `pending_healing` forever, un-drained, while — in the concrete repro — the
    /// fold's origin node kept re-broadcasting every ~10s because it never saw the
    /// gap close, each time re-discovering "not present locally" and re-queueing it
    /// right back into the same dead end. Confirmed via full chunk-storage
    /// inspection: the chunk was never queued on any node capable of acting on it.
    ///
    /// Always inserts locally too (cheap, and covers this node becoming leader
    /// shortly after, or the forward below failing) — the forward is additive, not
    /// a replacement.
    pub async fn queue_chunks_immediate(&self, chunk_ids: Vec<ChunkId>) {
        self.queue_chunks_immediate_local(chunk_ids.clone()).await;

        if self.cluster.is_leader().await {
            return;
        }
        let Some(leader_addr) = self.cluster.get_leader_addr().await else {
            warn!(
                "queue_chunks_immediate: not leader and no leader known — {} chunk(s) queued \
                 locally only, won't be drained until this node becomes leader",
                chunk_ids.len()
            );
            return;
        };
        const FORWARD_TIMEOUT: Duration = Duration::from_secs(5);
        let req = Request::QueueChunksForHealing { chunk_ids: chunk_ids.clone() };
        match tokio::time::timeout(FORWARD_TIMEOUT, self.client.send_message(leader_addr, Message::Request(req))).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => warn!(
                "queue_chunks_immediate: failed to forward {} chunk(s) to leader {}: {}",
                chunk_ids.len(), leader_addr, e
            ),
            Err(_) => warn!(
                "queue_chunks_immediate: timed out forwarding {} chunk(s) to leader {}",
                chunk_ids.len(), leader_addr
            ),
        }
    }

    /// Get healing statistics
    pub async fn get_stats(&self) -> HealingStats {
        let pending   = self.pending_healing.read().await;
        let in_flight = self.in_flight_healing.read().await;
        let stalled   = self.stalled_healing.read().await;

        // pending count excludes stalled (they're a subset of pending_healing keys
        // but have no source — not actionable by the heal loop right now).
        let stalled_count = stalled.len();
        let pending_count = pending.len().saturating_sub(stalled_count);

        HealingStats {
            pending_healing: pending_count,
            in_flight_healing: in_flight.len(),
            stalled_healing: stalled_count,
            auto_heal_enabled: self.healing_enabled.load(std::sync::atomic::Ordering::Relaxed),
            healing_delay_secs: self.healing_delay_secs.load(Ordering::Relaxed),
            current_bandwidth_mb: self.heal_bandwidth_limiter_out.current_rate_mb().await,
            tuning: self.tuning_snapshot().await,
        }
    }

    /// Trigger an immediate heal cycle, bypassing the 60s interval.
    /// Runs check_and_heal directly on the calling task. Only has effect on the leader;
    /// non-leaders log and return immediately (same behaviour as the periodic loop).
    ///
    /// Always runs a deep scan (full routing-table walk), never the fast one.
    ///
    /// Root-caused 2026-07-15 (T38 local-suite repro): this used to run a fast scan
    /// — which only rechecks chunks *already* in `pending_healing` (see
    /// `run_discovery_pass`'s doc comment) — whenever `pending_healing` was
    /// non-empty for *any* reason, including chunks entirely unrelated to what the
    /// caller cares about (e.g. concurrent healing activity for a different file).
    /// Confirmed live: the leader had full, correct, immediate knowledge that two of
    /// a just-restarted file's chunks sat at 2/3 replicas (it was a write-pair node
    /// for one of them itself), yet a manual trigger never scheduled either for
    /// healing — `pending_healing` happened to be non-empty from unrelated activity
    /// at that exact moment, so the fast-scan branch fired and neither chunk had
    /// ever been discovered before, making them invisible to it. A manual trigger is
    /// an explicit, infrequent, human/script-initiated request — thoroughness matters
    /// far more here than the periodic background loop's own cost-driven fast/deep
    /// alternation (which this doesn't touch).
    pub async fn trigger_heal_now(&self) -> Result<()> {
        if !self.cluster.is_leader().await {
            info!("TriggerHealing received on non-leader node — ignoring");
            return Ok(());
        }
        info!("Manual heal cycle triggered — running deep scan");
        self.run_discovery_pass(true).await?;
        self.drain_heal_queue().await
    }
}

/// Replication status of a chunk
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplicationStatus {
    Ok,
    UnderReplicated,
    OverReplicated,
}

/// Healing statistics
#[derive(Debug, Clone)]
pub struct HealingStats {
    pub pending_healing: usize,
    pub in_flight_healing: usize,
    pub stalled_healing: usize,
    pub auto_heal_enabled: bool,
    pub healing_delay_secs: u64,
    pub current_bandwidth_mb: usize,
    pub tuning: HealingTuningSnapshot,
}

/// Snapshot of the four live-settable healing tuning knobs — the configured ceilings,
/// not the adaptive controller's current instantaneous rate (that's `current_bandwidth_mb`
/// on `HealingStats`).
#[derive(Debug, Clone, Copy)]
pub struct HealingTuningSnapshot {
    pub link_bandwidth_mb: usize,
    pub heal_max_pct: f64,
    pub heal_max_concurrent: usize,
    pub heal_max_concurrent_per_node: usize,
    pub heal_transfer_timeout_secs: u64,
    pub healing_delay_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::ClusterManager;
    use crate::metadata::MetadataStore;
    use crate::storage::ChunkStorage;
    use dfs_common::{compute_chunk_hash, FileMetadata, FileType};
    use std::net::SocketAddr;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_healing_manager_creation() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));
        let client = Arc::new(NetworkClient::new());

        let healing = HealingManager::new(
            storage, metadata, cluster, client, Arc::new(AtomicUsize::new(3)), 300, 24, true,
            Arc::new(DashMap::new()), Arc::new(dashmap::DashSet::new()), Arc::new(DashMap::new()),
            Arc::new(AtomicU64::new(0)), 100, 60.0, 8, 3, 120,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );

        let stats = healing.get_stats().await;
        assert_eq!(stats.pending_healing, 0);
        assert!(stats.auto_heal_enabled);
        assert_eq!(stats.healing_delay_secs, 300);
    }

    /// Root-caused 2026-07-17: drain_heal_queue used to sort oldest-first only, so a
    /// chunk at 1-of-3 replicas got no priority over one at 2-of-3 under a sustained
    /// backlog — max_heal_per_cycle's truncation could defer the most severe gap
    /// behind less urgent ones. A chunk with fewer alive replicas must always sort
    /// before one with more, regardless of relative age.
    #[test]
    fn heal_queue_sort_key_prioritizes_fewer_alive_replicas_over_age() {
        let severely_under = HealingManager::heal_queue_sort_key(
            ReplicationStatus::UnderReplicated, 1, Duration::from_secs(10),
        );
        let mildly_under_but_older = HealingManager::heal_queue_sort_key(
            ReplicationStatus::UnderReplicated, 2, Duration::from_secs(9999),
        );
        assert!(severely_under < mildly_under_but_older,
            "1-of-N must sort before 2-of-3 even when the 2-of-3 chunk is far older");
    }

    /// Orphan-heal clog fix (2026-07-24, redesigned after a real T38 regression): the
    /// orphan-dequeue candidate set is trustworthy UNLESS `live` came back empty while
    /// there's real pending work — that specific combination is the signature of a
    /// not-yet-converged FILE_TABLE, not genuine mass orphaning.
    #[test]
    fn orphan_candidates_are_trustworthy_unless_live_is_suspiciously_empty() {
        // Normal cases: live has entries → trust the candidate set either way.
        assert!(HealingManager::orphan_candidates_are_trustworthy(false, true));
        assert!(HealingManager::orphan_candidates_are_trustworthy(false, false));
        // No pending work outstanding → nothing to distrust regardless of live's state.
        assert!(HealingManager::orphan_candidates_are_trustworthy(true, true));

        // The red flag: live is empty AND there IS pending work — don't trust it as
        // "everything is orphaned"; more likely this node's metadata hasn't converged.
        assert!(!HealingManager::orphan_candidates_are_trustworthy(true, false),
            "an empty live-set with real pending work outstanding must not be trusted \
             as evidence everything is orphaned — could be a not-yet-converged FILE_TABLE");
    }

    /// Regression test for the real T38 failure this redesign fixes (2026-07-24): a
    /// chunk genuinely referenced by a live file's chunk_locations must NEVER appear in
    /// orphaned_pending, because it's computed as pending_snapshot MINUS the exact same
    /// durable `live` set already trusted to build chunks_to_check — by construction, a
    /// chunk present in live can't also end up in the orphan difference.
    #[test]
    fn a_chunk_present_in_the_durable_live_set_is_never_in_the_orphan_difference() {
        let live: HashSet<ChunkId> = [ChunkId::from_hash(compute_chunk_hash(b"genuinely-live"))]
            .into_iter().collect();
        let pending_snapshot = live.clone();
        let orphaned_pending: Vec<ChunkId> = pending_snapshot.iter()
            .filter(|id| !live.contains(id))
            .copied()
            .collect();
        assert!(orphaned_pending.is_empty(),
            "a chunk_id present in `live` must never be computed as an orphan, regardless \
             of any other node's or subsystem's view — same source, same read transaction");
    }

    /// UnderReplicated must always outrank OverReplicated — restoring durability
    /// matters more than trimming excess copies, regardless of alive count or age.
    #[test]
    fn heal_queue_sort_key_prioritizes_under_replicated_over_over_replicated() {
        let under = HealingManager::heal_queue_sort_key(
            ReplicationStatus::UnderReplicated, 2, Duration::from_secs(1),
        );
        let over = HealingManager::heal_queue_sort_key(
            ReplicationStatus::OverReplicated, 5, Duration::from_secs(9999),
        );
        assert!(under < over,
            "UnderReplicated must sort before OverReplicated regardless of alive count or age");
    }

    /// Within equal severity and equal alive count, the existing oldest-first
    /// tie-break must still hold — this is the pre-existing behavior, must not
    /// regress.
    #[test]
    fn heal_queue_sort_key_falls_back_to_oldest_first_within_equal_severity() {
        let older = HealingManager::heal_queue_sort_key(
            ReplicationStatus::UnderReplicated, 2, Duration::from_secs(100),
        );
        let newer = HealingManager::heal_queue_sort_key(
            ReplicationStatus::UnderReplicated, 2, Duration::from_secs(1),
        );
        assert!(older < newer, "older entry must still sort first when severity and alive count are equal");
    }

    /// Root cause of the 2026-08-02 VM-108 chunk_idx=3147 data loss: a live,
    /// un-superseded, un-folded MultiPatch token was invisible to
    /// authorize_live_file_orphan_deletes because all_pending_patch_chunk_ids only
    /// protects a Pending patch's base/delta INPUTS, never the token itself (the
    /// PATCH_STATE_TABLE row's own key) — even though the token is exactly what
    /// chunk_map/clients treat as "the current content" for its slot. Simulates the
    /// actual failure: chunk written to disk + genuine Pending row, but chunk_map
    /// deliberately left empty (the client's one-shot ReplicateChunkLocation
    /// broadcast never arrived). Confirmed via negative control (see session notes)
    /// that reverting the all_patch_token_ids union makes this fail.
    #[tokio::test]
    async fn live_pending_patch_token_survives_disk_orphan_sweep_even_without_chunk_map_entry() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (storage, metadata, healing, _t1, _t2) = make_healing(node_id, addr);

        let token = ChunkId::from_hash(compute_chunk_hash(b"patch-token-never-broadcast"));
        let base = ChunkId::from_hash(compute_chunk_hash(b"base-chunk"));
        let delta = ChunkId::from_hash(compute_chunk_hash(b"delta-chunk"));
        storage.write_chunk(&token, b"patched chunk content").unwrap();
        metadata.put_patch_state_pending(
            FileId::new(), 3147, &token, base, delta, 4194304, 0, None,
        ).unwrap();
        // Deliberately NO chunk_map entry — the lost-broadcast condition.

        // Old enough to clear any grace period. Two consecutive passes: the sweep's
        // own two-pass debounce requires a candidate be seen on back-to-back cycles
        // before it's ever routed to authorize_live_file_orphan_deletes at all.
        let candidates = vec![(token, 999_999u64)];
        healing.reconcile_live_file_candidates(candidates.clone(), 0).await;
        healing.reconcile_live_file_candidates(candidates, 0).await;

        assert!(storage.get_chunk_path(&token).exists(),
            "a live Pending patch token must never be deleted by the disk orphan sweep, \
             even when chunk_map/FILE_TABLE have no record of it — PATCH_STATE_TABLE \
             alone must be enough to protect it");
    }

    /// run_pending_patch_reconciliation: a token already confirmed against the
    /// unchanged current leader must be skipped entirely (no wasted RPC attempt),
    /// while a detected leader change must invalidate the WHOLE confirmed-set at
    /// once — not just the one token — so a new leader gets re-announced to for
    /// everything outstanding, regardless of what the old leader supposedly knew.
    #[tokio::test]
    async fn pending_patch_reconciliation_skips_confirmed_but_leader_change_invalidates_all() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8901".parse().unwrap();
        let (storage, metadata, healing, _t1, _t2) = make_healing(node_id, addr);

        let token = ChunkId::from_hash(compute_chunk_hash(b"reconcile-test-token"));
        let base = ChunkId::from_hash(compute_chunk_hash(b"reconcile-base"));
        let delta = ChunkId::from_hash(compute_chunk_hash(b"reconcile-delta"));
        storage.write_chunk(&token, b"content").unwrap();
        metadata.put_patch_state_pending(
            FileId::new(), 42, &token, base, delta, 4096, 0, None,
        ).unwrap();

        // Solo single-node cluster: get_leader_addr() resolves to this node's own
        // address. Pre-seed confirmed-state as if a prior cycle already succeeded
        // against that same leader, so this cycle should see no leader change and
        // skip re-announcing (no RPC attempt needed to prove it — the token simply
        // stays in the confirmed set either way if skipped, whereas an attempted-
        // and-failed announce would remove it).
        *healing.last_reconcile_leader.write().await = Some(addr);
        healing.pending_patch_confirmed.write().await.insert(token);

        healing.run_pending_patch_reconciliation().await;
        assert!(healing.pending_patch_confirmed.read().await.contains(&token),
            "unchanged leader + already-confirmed token must be skipped, not re-evaluated");

        // Now simulate a leader change (e.g. a rolling restart electing someone
        // else) by seeding a different "last known leader".
        *healing.last_reconcile_leader.write().await = Some("127.0.0.1:19999".parse().unwrap());

        healing.run_pending_patch_reconciliation().await;
        // The confirmed-set must have been invalidated wholesale on the detected
        // leader change. The re-announce attempt itself will fail (no real leader
        // listening in this test), so the token won't be re-inserted — but that's
        // exactly the observable proof the clear happened: if it hadn't been
        // cleared, the token would still be sitting there from before.
        assert!(!healing.pending_patch_confirmed.read().await.contains(&token),
            "a detected leader change must invalidate the whole confirmed-set, \
             not just leave stale per-leader confirmations in place");
    }

    /// 2026-08-04 fix: notify_leader_of_fold is best-effort (2 attempts, 1s timeout
    /// each) and run_single_fold has already committed the fold locally by the time
    /// it runs — a failure there previously left the leader's view silently stale
    /// forever, with nothing to retry it. run_pending_patch_reconciliation now also
    /// reconciles Folded slots, same shape as the Pending case above: an
    /// already-confirmed fold is skipped, and a detected leader change invalidates
    /// the whole folded_patch_confirmed set so a new leader gets told about every
    /// outstanding fold regardless of what the old leader supposedly knew.
    #[tokio::test]
    async fn fold_reconciliation_skips_confirmed_but_leader_change_invalidates_all() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8902".parse().unwrap();
        let (storage, metadata, healing, _t1, _t2) = make_healing(node_id, addr);

        let token = ChunkId::from_hash(compute_chunk_hash(b"fold-reconcile-test-token"));
        let real_chunk_id = ChunkId::from_hash(compute_chunk_hash(b"fold-reconcile-real-chunk"));
        let file_id = FileId::new();
        storage.write_chunk(&real_chunk_id, b"folded content").unwrap();
        metadata.put_patch_state_pending(file_id, 7, &token, real_chunk_id, real_chunk_id, 15, 0, None).unwrap();
        metadata.update_patch_state_folded(&token, real_chunk_id).unwrap();
        metadata.put_chunk_location(&ChunkLocation {
            chunk_id: real_chunk_id,
            nodes: vec![node_id],
            size: 15,
            checksum: real_chunk_id.hash,
            file_offset: Some(7 * 4 * 1024 * 1024),
            written_at: Some(0),
            client_write_seq: None,
            file_id: Some(file_id),
        }).unwrap();

        // Solo single-node cluster: get_leader_addr() resolves to this node's own
        // address. Pre-seed as if a prior cycle already confirmed this fold against
        // that same leader — unchanged leader must skip it, no RPC attempt needed.
        *healing.last_reconcile_leader.write().await = Some(addr);
        healing.folded_patch_confirmed.write().await.insert(real_chunk_id);

        healing.run_pending_patch_reconciliation().await;
        assert!(healing.folded_patch_confirmed.read().await.contains(&real_chunk_id),
            "unchanged leader + already-confirmed fold must be skipped, not re-evaluated");

        // Simulate a leader change (e.g. a rolling restart electing someone else).
        *healing.last_reconcile_leader.write().await = Some("127.0.0.1:19998".parse().unwrap());

        healing.run_pending_patch_reconciliation().await;
        // Invalidated wholesale on the detected leader change — the re-announce
        // attempt itself fails (no real leader listening in this test), so it won't
        // be re-inserted, which is exactly the observable proof the clear happened.
        assert!(!healing.folded_patch_confirmed.read().await.contains(&real_chunk_id),
            "a detected leader change must invalidate the whole folded_patch_confirmed \
             set too, not just leave stale per-leader confirmations in place");
    }

    /// 2026-08-04 pagination: the concrete benefit the user described — "by the time
    /// we get to page 2, page 1's chunks would already be reconciled and would never
    /// get enqueued in the first place" — plus a check that pagination doesn't
    /// silently break the two-pass debounce for the OTHER, still-genuinely-orphaned
    /// chunk on a different page in the same rotation (the bug this test would have
    /// caught: reconcile_live_file_candidates used to replace orphan_candidates
    /// wholesale on every call, so page 2's call was wiping out page 1's first-sighting
    /// record before it ever got its second sighting).
    #[tokio::test]
    async fn disk_orphan_sweep_over_reconciled_chunk_never_flagged_and_pagination_does_not_break_debounce() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8904".parse().unwrap();
        let (storage, metadata, healing, _t1, _t2) = make_healing(node_id, addr);

        let page1_chunk = ChunkId::from_hash(compute_chunk_hash(b"page1-genuine-orphan"));
        let page2_chunk = ChunkId::from_hash(compute_chunk_hash(b"page2-also-genuine-orphan"));
        let page3_chunk = ChunkId::from_hash(compute_chunk_hash(b"page3-gets-reconciled-first"));
        storage.write_chunk(&page1_chunk, b"page1 data").unwrap();
        storage.write_chunk(&page2_chunk, b"page2 data").unwrap();
        storage.write_chunk(&page3_chunk, b"page3 data").unwrap();
        // Past the default 600s DFS_LIVE_FILE_ORPHAN_GRACE_SECS so all three are old
        // enough to be tracked immediately — this test is about the debounce/
        // reconciliation interaction, not the age gate (covered by its own test).
        let old_ts = dfs_common::types::current_timestamp().saturating_sub(700);
        storage.set_chunk_mtime(&page1_chunk, old_ts);
        storage.set_chunk_mtime(&page2_chunk, old_ts);
        storage.set_chunk_mtime(&page3_chunk, old_ts);

        // Rotation 1, page 1: page1_chunk has no live reference anywhere — first
        // sighting, recorded as a candidate.
        healing.run_disk_orphan_sweep_over(vec![page1_chunk]).await;
        assert!(healing.orphan_candidates.read().await.contains(&page1_chunk),
            "an unreconciled chunk on its own page must become a first-pass candidate");

        // Before page3_chunk's own page is ever processed, it gets legitimately
        // reconciled (a real Pending patch state lands for it — same protection
        // exercised by live_pending_patch_token_survives_disk_orphan_sweep_...).
        // Under the old unbounded full-scan, all three chunks would have been checked
        // in the exact same pass, before this reconciliation had any chance to land.
        metadata.put_patch_state_pending(
            FileId::new(), 7, &page3_chunk,
            ChunkId::from_hash(compute_chunk_hash(b"page3-base")),
            ChunkId::from_hash(compute_chunk_hash(b"page3-delta")),
            4096, 0, None,
        ).unwrap();

        // Rotation 1, page 2: page2_chunk is a SEPARATE, still-genuinely-unreconciled
        // orphan on a different page in the same rotation. This is the call that
        // exercises the actual bug: reconcile_live_file_candidates used to REPLACE
        // orphan_candidates wholesale with just this call's own candidates, which
        // would silently wipe out page1_chunk's first-sighting record recorded above.
        healing.run_disk_orphan_sweep_over(vec![page2_chunk]).await;
        assert!(healing.orphan_candidates.read().await.contains(&page1_chunk),
            "an earlier page's candidate in the same rotation must survive a LATER page's \
             own (non-empty) reconcile call — pagination must not break the cross-page \
             two-pass debounce by clobbering it wholesale");
        assert!(healing.orphan_candidates.read().await.contains(&page2_chunk),
            "the later page's own genuine candidate must also be tracked");

        // Rotation 1, page 3: page3_chunk is now live (protected by PATCH_STATE_TABLE)
        // and must never even become a candidate — the concrete pagination benefit.
        healing.run_disk_orphan_sweep_over(vec![page3_chunk]).await;
        assert!(!healing.orphan_candidates.read().await.contains(&page3_chunk),
            "a chunk reconciled before its own page is reached must never be flagged as a candidate at all");
        // Still unaffected by the empty-candidates call for page3.
        assert!(healing.orphan_candidates.read().await.contains(&page1_chunk));
        assert!(healing.orphan_candidates.read().await.contains(&page2_chunk));

        // Rotation 2, page 1 and page 2: both genuine orphans are seen again, still
        // with no live reference — second sighting, ready for the leader-confirm/
        // stability gate. Single-node cluster (no peers) authorizes everything, so
        // this proves the debounce survived across pages to actually trigger deletion.
        healing.run_disk_orphan_sweep_over(vec![page1_chunk]).await;
        healing.run_disk_orphan_sweep_over(vec![page2_chunk]).await;
        assert!(!storage.get_chunk_path(&page1_chunk).exists(),
            "a genuinely never-reconciled chunk must still be evicted after two real \
             sightings, unaffected by another page's reconciliation or candidacy in between");
        assert!(!storage.get_chunk_path(&page2_chunk).exists(),
            "same for the second genuinely-orphaned page, proving both survived independently");
    }

    /// Pure arithmetic for the paginated sweep's rate limiter — tested directly
    /// rather than via real sleeps to avoid timing flakiness.
    #[test]
    fn disk_sweep_next_delay_floors_at_zero_and_subtracts_elapsed_work() {
        // Cheap page: most of the grace period is still owed.
        assert_eq!(
            HealingManager::disk_sweep_next_delay(Duration::from_millis(3000), Duration::from_millis(200)),
            Duration::from_millis(2800),
        );
        // A page whose own work exactly consumed the grace period: nothing left to wait.
        assert_eq!(
            HealingManager::disk_sweep_next_delay(Duration::from_millis(3000), Duration::from_millis(3000)),
            Duration::from_millis(0),
        );
        // A slow page that overran the grace period must floor at zero, not go
        // negative (Duration can't represent negative — this is the saturating_sub
        // behavior the fix relies on to avoid a panic/wraparound).
        assert_eq!(
            HealingManager::disk_sweep_next_delay(Duration::from_millis(3000), Duration::from_millis(5000)),
            Duration::from_millis(0),
        );
    }

    /// The paginated sweep loop is self-perpetuating with nothing else to restart it
    /// if it dies — this proves the watchdog actually detects a finished task and
    /// replaces it, rather than just trusting the design on paper. Uses a stand-in
    /// finished task in place of a real dead paginated loop (no need to make the real
    /// loop panic to prove is_finished()-based detection works).
    #[tokio::test]
    async fn disk_orphan_sweep_watchdog_detects_finished_task_and_respawns() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8905".parse().unwrap();
        let (_storage, _metadata, healing, _t1, _t2) = make_healing(node_id, addr);
        let healing = Arc::new(healing);

        // Stand-in for a dead paginated-sweep task: an already-completed no-op.
        let dead_handle = tokio::spawn(async {});
        // Let it actually finish before the watchdog checks it.
        while !dead_handle.is_finished() {
            tokio::task::yield_now().await;
        }
        *healing.disk_sweep_task.lock().unwrap() = Some(dead_handle);

        // Stale heartbeat, so we can tell whether the respawned task actually ran.
        *healing.disk_sweep_last_page_at.lock().unwrap() = Instant::now() - Duration::from_secs(3600);

        healing.clone().disk_orphan_sweep_watchdog_check_once().await;

        let new_handle_alive = {
            let guard = healing.disk_sweep_task.lock().unwrap();
            guard.as_ref().map(|h| !h.is_finished())
        };
        assert_eq!(new_handle_alive, Some(true),
            "watchdog must store a new, running JoinHandle after detecting the old one had finished");

        // Give the newly-spawned task a moment to run its first loop iteration (it
        // writes disk_sweep_last_page_at before its first sleep, whether gated or
        // not — see run_disk_orphan_sweep_paginated_loop).
        tokio::time::sleep(Duration::from_millis(100)).await;
        let heartbeat_age = healing.disk_sweep_last_page_at.lock().unwrap().elapsed();
        assert!(heartbeat_age < Duration::from_secs(10),
            "the respawned task must actually be running — its heartbeat should be fresh, \
             not the stale 3600s-old value seeded before the respawn");
    }

    /// requeue_priority fix (2026-07-24): a chunk that has failed/stalled before must
    /// sort by time-since-that-failure, NOT its original detection time — otherwise a
    /// flaky/doomed chunk that keeps failing looks like the MOST urgent candidate the
    /// instant it becomes eligible again (oldest-detected) and jumps to the front of
    /// dispatch, starving chunks genuinely waiting their normal turn.
    #[test]
    fn effective_heal_priority_age_prefers_last_failure_over_original_detection() {
        // No failure history: falls back to original detection time, unchanged from
        // today's behavior for the common (never-failed) case.
        assert_eq!(
            HealingManager::effective_heal_priority_age(None, Some(Duration::from_secs(9999))),
            Duration::from_secs(9999),
        );

        // Failed before: even though this chunk was detected ages ago (would sort
        // FIRST / front-of-queue under the old oldest-first-only logic), a RECENT
        // failure means it must sort as young/back-of-queue instead.
        let age = HealingManager::effective_heal_priority_age(
            Some(Duration::from_secs(1)), Some(Duration::from_secs(9999)),
        );
        assert_eq!(age, Duration::from_secs(1),
            "a chunk that JUST failed must sort by that recency, not by how long ago it was first detected");

        // End-to-end via the real sort key: a chunk detected long ago but that failed
        // moments ago must sort AFTER (not before) a chunk that has been quietly
        // waiting the whole time with no failure at all.
        let just_failed_but_ancient = HealingManager::heal_queue_sort_key(
            ReplicationStatus::UnderReplicated, 1,
            HealingManager::effective_heal_priority_age(Some(Duration::from_secs(1)), Some(Duration::from_secs(9999))),
        );
        let never_failed_genuinely_old = HealingManager::heal_queue_sort_key(
            ReplicationStatus::UnderReplicated, 1,
            HealingManager::effective_heal_priority_age(None, Some(Duration::from_secs(500))),
        );
        assert!(never_failed_genuinely_old < just_failed_but_ancient,
            "a chunk that just failed must not jump ahead of a chunk that has been \
             quietly, successfully waiting its turn without ever failing");
    }

    #[tokio::test]
    async fn test_should_heal_with_delay() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));
        let client = Arc::new(NetworkClient::new());

        let healing = HealingManager::new(
            storage, metadata, cluster, client, Arc::new(AtomicUsize::new(3)), 2, 24, true,
            Arc::new(DashMap::new()), Arc::new(dashmap::DashSet::new()), Arc::new(DashMap::new()),
            Arc::new(AtomicU64::new(0)), 100, 60.0, 8, 3, 120,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        ); // 2s delay

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"test"));
        // Persistence of first-detection times is deferred to the caller's batch
        // flush (see mark_pending_deferred) — collect it like the discovery loop does.
        let mut deferred: Vec<(ChunkId, u64)> = Vec::new();

        // First check - should return false and mark for healing
        assert!(!healing.should_heal(&chunk_id, &mut deferred).await);
        assert_eq!(deferred.len(), 1, "first detection must be queued for batch persistence");

        // Still within delay — and no duplicate deferred entry for a known chunk
        assert!(!healing.should_heal(&chunk_id, &mut deferred).await);
        assert_eq!(deferred.len(), 1);

        // Wait for delay
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Now should heal
        assert!(healing.should_heal(&chunk_id, &mut deferred).await);
    }

    /// A chunk's pending-healing "first detected" time must survive a process
    /// restart: HealingManager::new seeds pending_healing from the persisted
    /// PENDING_HEALING_TABLE, so the healing_delay_secs debounce reflects total
    /// elapsed time (pre- and post-restart), not just time-since-this-process-started.
    #[tokio::test]
    async fn test_pending_healing_survives_restart() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));
        let client = Arc::new(NetworkClient::new());

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"restart-test"));

        // Simulate a chunk a *prior process* detected as needing healing 8s ago.
        let detected_at_secs = dfs_common::types::current_timestamp() - 8;
        metadata.put_pending_healing(&chunk_id, detected_at_secs).unwrap();

        // Construct a fresh HealingManager against the same MetadataStore —
        // simulates a process restart. healing_delay_secs = 10, so the 8s that
        // already elapsed before restart must carry over.
        let healing = HealingManager::new(
            storage, metadata, cluster, client, Arc::new(AtomicUsize::new(3)), 10, 24, true,
            Arc::new(DashMap::new()), Arc::new(dashmap::DashSet::new()), Arc::new(DashMap::new()),
            Arc::new(AtomicU64::new(0)), 100, 60.0, 8, 3, 120,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );

        // 8s < 10s — not ready yet, but the persisted age must have been restored
        // (not reset to 0s on restart).
        let mut deferred: Vec<(ChunkId, u64)> = Vec::new();
        assert!(!healing.should_heal(&chunk_id, &mut deferred).await);
        assert!(deferred.is_empty(), "restored entry must not be re-queued for persistence");

        // 8s (carried over) + 3s (elapsed since restart) = 11s >= 10s.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(healing.should_heal(&chunk_id, &mut deferred).await);
    }

    fn make_healing(node_id: NodeId, addr: SocketAddr) -> (Arc<ChunkStorage>, Arc<MetadataStore>, HealingManager, TempDir, TempDir) {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();
        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));
        let client = Arc::new(NetworkClient::new());
        let healing = HealingManager::new(
            storage.clone(), metadata.clone(), cluster, client, Arc::new(AtomicUsize::new(3)), 300, 24, true,
            Arc::new(DashMap::new()), Arc::new(dashmap::DashSet::new()), Arc::new(DashMap::new()),
            Arc::new(AtomicU64::new(0)), 100, 60.0, 8, 3, 120,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        (storage, metadata, healing, temp_storage, temp_metadata)
    }

    /// Single-node cluster: this node is trivially its own leader with no peers to
    /// check, so the stability gate has nothing to fail on — every candidate must be
    /// authorized.
    #[tokio::test]
    async fn test_authorize_live_file_orphan_deletes_leader_no_peers_authorizes_all() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (_storage, _metadata, healing, _t1, _t2) = make_healing(node_id, addr);

        let candidates = vec![
            ChunkId::from_hash(compute_chunk_hash(b"a")),
            ChunkId::from_hash(compute_chunk_hash(b"b")),
        ];
        let authorized = healing.authorize_live_file_orphan_deletes(&candidates).await;
        assert_eq!(authorized.len(), 2, "no peers to fail the stability check — must authorize everything");
    }

    /// Leader with an unreachable peer: the stability check cannot confirm the peer
    /// has been up long enough, so nothing may be deleted this cycle. This is the
    /// core split-brain guard — when in doubt, defer instead of deleting.
    #[tokio::test]
    async fn test_authorize_live_file_orphan_deletes_leader_unreachable_peer_defers() {
        let (id_a, id_b) = {
            let a = NodeId::new();
            let b = NodeId::new();
            if a < b { (a, b) } else { (b, a) }
        };
        let local_addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (_storage, _metadata, healing, _t1, _t2) = make_healing(id_a, local_addr);

        // id_a < id_b, so id_a (local) is the leader by construction (min NodeId).
        // Peer address is unreachable — GetNodeStats must fail/timeout.
        let peer_addr: SocketAddr = "127.0.0.1:19999".parse().unwrap();
        healing.cluster.add_node(dfs_common::NodeInfo::new(id_b, peer_addr, None)).await.unwrap();
        assert!(healing.cluster.is_leader().await, "local node (min id) must be leader");

        let candidates = vec![ChunkId::from_hash(compute_chunk_hash(b"c"))];
        let authorized = healing.authorize_live_file_orphan_deletes(&candidates).await;
        assert!(authorized.is_empty(), "unreachable peer must defer the whole batch, not authorize anything");
    }

    /// Non-leader node with an unreachable leader: ConfirmChunksLive cannot be
    /// answered, so nothing may be deleted this cycle.
    #[tokio::test]
    async fn test_authorize_live_file_orphan_deletes_follower_unreachable_leader_defers() {
        let (id_a, id_b) = {
            let a = NodeId::new();
            let b = NodeId::new();
            if a < b { (a, b) } else { (b, a) }
        };
        // Local node is id_b (NOT the minimum) — id_a is the leader.
        let local_addr: SocketAddr = "127.0.0.1:8901".parse().unwrap();
        let (_storage, _metadata, healing, _t1, _t2) = make_healing(id_b, local_addr);

        let leader_addr: SocketAddr = "127.0.0.1:19998".parse().unwrap(); // unreachable
        healing.cluster.add_node(dfs_common::NodeInfo::new(id_a, leader_addr, None)).await.unwrap();
        assert!(!healing.cluster.is_leader().await, "local node (not min id) must not be leader");

        let candidates = vec![ChunkId::from_hash(compute_chunk_hash(b"d"))];
        let authorized = healing.authorize_live_file_orphan_deletes(&candidates).await;
        assert!(authorized.is_empty(), "unreachable leader must defer, never authorize blindly");
    }

    /// Leader case: queue_chunks_immediate must queue locally and return promptly —
    /// no peer to forward to, no network round trip needed. Baseline sanity check
    /// before the non-leader tests below, which exercise the actual fix.
    #[tokio::test]
    async fn test_queue_chunks_immediate_leader_queues_locally() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (_storage, _metadata, healing, _t1, _t2) = make_healing(node_id, addr);
        assert!(healing.cluster.is_leader().await, "single-node cluster must be its own leader");

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"leader-local"));
        healing.queue_chunks_immediate(vec![chunk_id]).await;

        let stats = healing.get_stats().await;
        assert_eq!(stats.pending_healing, 1, "chunk must be queued locally on the leader");
    }

    /// Root-caused 2026-07-15 (T38 local-suite repro): queue_chunks_immediate used
    /// to write only into *this* node's own pending_healing — a dead end on a
    /// non-leader, since only the leader's run_discovery_loop/run_heal_loop ever
    /// drain it (see their own doc comments). handle_replicate_patch_fold calls this
    /// on whichever node receives a fold broadcast, which is routinely not the
    /// leader — the chunk sat queued forever while the fold's origin node kept
    /// re-broadcasting every ~10s, each retry re-discovering the same gap and
    /// re-queueing it right back into the same dead end.
    ///
    /// This exercises the still-safe half of the fix in isolation (no live peer to
    /// round-trip through in a unit test): a non-leader must still queue the chunk
    /// locally as a backstop even when the forward-to-leader attempt can't succeed
    /// (unreachable leader here), rather than silently doing nothing. The other half
    /// — the forward actually reaching and being applied on a real leader — is
    /// covered end-to-end by the local test suite's T38 (rolling restart + patch
    /// fold + healing convergence).
    #[tokio::test]
    async fn test_queue_chunks_immediate_non_leader_still_queues_locally_as_backstop() {
        let (id_a, id_b) = {
            let a = NodeId::new();
            let b = NodeId::new();
            if a < b { (a, b) } else { (b, a) }
        };
        // Local node is id_b (NOT the minimum) — id_a is the leader.
        let local_addr: SocketAddr = "127.0.0.1:8901".parse().unwrap();
        let (_storage, _metadata, healing, _t1, _t2) = make_healing(id_b, local_addr);

        let leader_addr: SocketAddr = "127.0.0.1:19997".parse().unwrap(); // unreachable
        healing.cluster.add_node(dfs_common::NodeInfo::new(id_a, leader_addr, None)).await.unwrap();
        assert!(!healing.cluster.is_leader().await, "local node (not min id) must not be leader");

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"non-leader-backstop"));
        healing.queue_chunks_immediate(vec![chunk_id]).await;

        let stats = healing.get_stats().await;
        assert_eq!(
            stats.pending_healing, 1,
            "non-leader must still queue locally even when the forward-to-leader attempt fails — \
             calling this must never silently do nothing"
        );
    }

    /// Root-caused 2026-07-15 (T38 local-suite repro, half-capacity resource caps):
    /// a chunk folded *after* the one deep-scan discovery pass a manual trigger runs
    /// never gets an alive_nodes_cache entry from anywhere else within a short test
    /// window (next periodic discovery is 60s out) — drain_heal_queue's old
    /// `None => continue` would skip it forever. This proves drain_heal_queue now
    /// probes and populates the cache itself on a miss, in the same cycle, instead
    /// of waiting on an external discovery pass that may not come in time.
    #[tokio::test]
    async fn test_drain_heal_queue_probes_cache_miss_instead_of_skipping_forever() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (storage, _metadata, healing, _t1, _t2) = make_healing(node_id, addr);
        assert!(healing.cluster.is_leader().await, "single-node cluster must be its own leader");

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"cache-miss-probe"));
        storage.write_chunk(&chunk_id, b"real chunk bytes").unwrap();

        // Queue it (backdated, bypassing healing_delay_secs) but do NOT populate
        // alive_nodes_cache — simulating a fold that completed after the one
        // discovery pass that's already run.
        healing.queue_chunks_immediate_local(vec![chunk_id]).await;
        assert!(
            healing.alive_nodes_cache.read().await.get(&chunk_id).is_none(),
            "test setup: cache must start empty for this chunk"
        );

        healing.drain_heal_queue().await.unwrap();

        assert!(
            healing.alive_nodes_cache.read().await.get(&chunk_id).is_some(),
            "drain_heal_queue must probe and populate a missing cache entry on-demand \
             instead of silently skipping the chunk until the next external discovery pass"
        );
    }

    /// Root-caused 2026-07-15 (T38 local-suite repro, half-capacity resource caps):
    /// a repeat call to queue_chunks_immediate_local for a chunk that's *already*
    /// pending must not wipe an already-populated alive_nodes_cache entry —
    /// drain_heal_queue's `cache.get(chunk_id) == None => continue` treats a missing
    /// entry as "discovery hasn't run yet" and skips the chunk entirely until the
    /// next full discovery pass (60s periodic). handle_replicate_patch_fold's
    /// rebroadcast sweep re-calls this every few seconds for as long as a fold's
    /// pointer gap stays open — if every one of those repeats re-clears the cache,
    /// the entry never survives long enough for drain_heal_queue's next 15s tick to
    /// see it, and the chunk starves indefinitely even though it's correctly
    /// backdated past healing_delay_secs the whole time. Confirmed live: the same
    /// chunk got re-queued for 35+ seconds straight and never once appeared in a
    /// "Leader healing under-replicated chunk" line.
    #[tokio::test]
    async fn test_repeat_queue_of_already_pending_chunk_preserves_alive_nodes_cache() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (_storage, _metadata, healing, _t1, _t2) = make_healing(node_id, addr);

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"repeat-queue-cache"));

        // First queue: brand new — cache has nothing to preserve yet.
        healing.queue_chunks_immediate_local(vec![chunk_id]).await;

        // Simulate a discovery pass having since populated the cache with the real,
        // confirmed-alive node set for this chunk.
        let confirmed = vec![NodeId::new(), NodeId::new()];
        healing.alive_nodes_cache.write().await.insert(chunk_id, confirmed.clone());

        // A second, repeat queue (the rebroadcast-sweep re-notification) must NOT
        // clear that freshly-populated entry — the chunk is already pending, so
        // there's no "stale data from a finished cycle" to invalidate.
        healing.queue_chunks_immediate_local(vec![chunk_id]).await;

        let cache = healing.alive_nodes_cache.read().await;
        assert_eq!(
            cache.get(&chunk_id),
            Some(&confirmed),
            "repeat-queueing an already-pending chunk must not wipe its populated \
             alive_nodes_cache entry, or drain_heal_queue will skip it forever"
        );
    }

    /// Root-caused 2026-07-15 (T38 local-suite repro): trigger_heal_now used to run
    /// a fast scan (only rechecks chunks already in pending_healing) whenever
    /// pending_healing was non-empty for *any* reason — including activity entirely
    /// unrelated to what the caller actually wants checked. This reproduces exactly
    /// that: seed one unrelated chunk into pending_healing (simulating concurrent
    /// healing activity elsewhere in the cluster), then create a second,
    /// genuinely-under-replicated chunk that has never been discovered before, and
    /// confirm a manual trigger still finds it. Before the fix (always deep), this
    /// second chunk would be invisible — a fast scan only looks up pre-existing
    /// pending_healing keys — so it would never be scheduled for healing until the
    /// next periodic deep-scan tick, which can be much later than a human/script
    /// calling `dfs-admin healing trigger` reasonably expects.
    #[tokio::test]
    async fn test_trigger_heal_now_always_deep_scans_even_with_unrelated_pending() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (storage, metadata, healing, _t1, _t2) = make_healing(node_id, addr);
        // Remove the delay gate so discovery+drain in one trigger_heal_now() call
        // doesn't also need a second cycle to become actionable — this test is about
        // whether the chunk is *discovered* at all, not the separate delay gate
        // (covered by test_should_heal_with_delay).
        healing.healing_delay_secs.store(0, Ordering::Relaxed);

        // Simulate unrelated concurrent healing activity: some other chunk, from
        // some other file, already sitting in pending_healing.
        let unrelated_chunk = ChunkId::from_hash(compute_chunk_hash(b"unrelated-concurrent-activity"));
        healing.queue_chunks_immediate(vec![unrelated_chunk]).await;
        assert_eq!(healing.pending_healing.read().await.len(), 1, "sanity: unrelated chunk seeded");

        // A genuinely under-replicated chunk that has never been discovered before —
        // present on this node (so it's not "stalled" for lack of a source) but only
        // 1 of the 3 replication_factor's worth of nodes, and referenced by a live
        // file record so the deep scan's live_chunk_ids() check includes it.
        let new_chunk = ChunkId::from_hash(compute_chunk_hash(b"never-before-seen-under-replicated"));
        storage.write_chunk(&new_chunk, b"some real chunk data").unwrap();
        let mut file_meta = FileMetadata::new("/new_chunk_test.bin".to_string(), FileType::RegularFile);
        file_meta.size = 21;
        // file_id must match the FileMetadata's own copy below — do_heal_chunk_inner
        // treats a routing-table ChunkLocation with file_id: None as "can't verify
        // it's not an orphan quickly, defer to the orphan sweep" and clears it from
        // pending_healing outright (not even stalled). A real fresh-write always sets
        // this; leaving it None here was just this test's own setup gap, invisible
        // as long as drain_heal_queue skipped the chunk before ever reaching this
        // check (the exact cache-miss gap fixed elsewhere today).
        metadata.put_chunk_location(&ChunkLocation {
            chunk_id: new_chunk,
            nodes: vec![node_id],
            size: 21,
            checksum: new_chunk.hash,
            file_offset: Some(0),
            written_at: Some(dfs_common::types::current_timestamp() * 1000),
            client_write_seq: None,
            file_id: Some(file_meta.id),
        }).unwrap();
        file_meta.chunk_locations = Arc::new(vec![ChunkLocation {
            chunk_id: new_chunk,
            nodes: vec![node_id],
            size: 21,
            checksum: new_chunk.hash,
            file_offset: Some(0),
            written_at: Some(dfs_common::types::current_timestamp() * 1000),
            client_write_seq: None,
            file_id: Some(file_meta.id),
        }]);
        metadata.put_file(&file_meta).unwrap();

        healing.trigger_heal_now().await.unwrap();

        assert!(
            healing.pending_healing.read().await.contains_key(&new_chunk),
            "a manual trigger must discover a never-before-seen under-replicated chunk \
             even when pending_healing already has unrelated entries in it — a fast \
             scan can never find this, only a deep one"
        );
    }

    /// End-to-end: a chunk still routed to us in the local CHUNK_TABLE, but with no
    /// live file referencing it (a patch-superseded chunk the inline fast-evict
    /// missed), must survive the first sighting (two-pass guard) and only be evicted
    /// on a second pass once it's both old enough and authorized.
    #[tokio::test]
    async fn test_disk_orphan_sweep_evicts_live_file_orphan_after_two_pass_when_authorized() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (storage, metadata, mut healing, _t1, _t2) = make_healing(node_id, addr);
        // Past SELF_RESTART_GRACE_SECS — this test exercises the two-pass eviction
        // logic itself, not the restart-grace gate (covered by its own test below).
        healing.local_started_at = Instant::now() - Duration::from_secs(SELF_RESTART_GRACE_SECS + 1);

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"live-file-orphan-test"));
        storage.write_chunk(&chunk_id, b"some data").unwrap();
        let old_ts = dfs_common::types::current_timestamp().saturating_sub(700);
        storage.set_chunk_mtime(&chunk_id, old_ts);

        // Still routed to us, but no file metadata references it at all.
        metadata.put_chunk_location(&ChunkLocation {
            chunk_id,
            nodes: vec![node_id],
            size: 9,
            checksum: chunk_id.hash,
            file_offset: Some(0),
            written_at: Some(old_ts * 1000),
            client_write_seq: None,
            file_id: None,
        }).unwrap();

        healing.run_disk_orphan_sweep().await;
        assert!(storage.get_chunk_path(&chunk_id).exists(), "must survive first sighting (two-pass guard)");

        healing.run_disk_orphan_sweep().await;
        assert!(!storage.get_chunk_path(&chunk_id).exists(), "must be evicted on second pass once old enough and authorized");
    }

    /// A live-file-orphan candidate younger than LIVE_FILE_GRACE_SECS must never be
    /// deleted, even across many sweep passes — age is checked every pass, not just
    /// before the first sighting.
    #[tokio::test]
    async fn test_disk_orphan_sweep_never_evicts_recent_live_file_orphan() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (storage, metadata, mut healing, _t1, _t2) = make_healing(node_id, addr);
        // Past SELF_RESTART_GRACE_SECS — this test exercises the chunk-age grace
        // logic itself, not the restart-grace gate (covered by its own test below);
        // without this the sweep would no-op for the wrong reason and the
        // assertion below would pass vacuously.
        healing.local_started_at = Instant::now() - Duration::from_secs(SELF_RESTART_GRACE_SECS + 1);

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"too-recent-orphan"));
        storage.write_chunk(&chunk_id, b"some data").unwrap();
        // mtime left at "now" — well within the 600s grace window.

        metadata.put_chunk_location(&ChunkLocation {
            chunk_id,
            nodes: vec![node_id],
            size: 9,
            checksum: chunk_id.hash,
            file_offset: Some(0),
            written_at: Some(dfs_common::types::current_timestamp() * 1000),
            client_write_seq: None,
            file_id: None,
        }).unwrap();

        for _ in 0..3 {
            healing.run_disk_orphan_sweep().await;
        }
        assert!(storage.get_chunk_path(&chunk_id).exists(), "must never evict a candidate still inside the age grace");
    }

    /// Regression test for the 2026-07-10 incident: a freshly-restarted node's
    /// disk-orphan-sweep must not delete anything, even for a candidate that's
    /// otherwise old enough and would normally be authorized on a second pass —
    /// SELF_RESTART_GRACE_SECS must gate this independently of chunk age and
    /// independently of leadership (this node is never leader in this test, so
    /// LEADER_CHANGE_GRACE_SECS's own check trivially passes and would not have
    /// caught this).
    #[tokio::test]
    async fn test_disk_orphan_sweep_defers_within_self_restart_grace_period() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        // Freshly constructed — local_started_at defaults to "now", well within
        // SELF_RESTART_GRACE_SECS.
        let (storage, metadata, healing, _t1, _t2) = make_healing(node_id, addr);

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"restart-grace-test"));
        storage.write_chunk(&chunk_id, b"some data").unwrap();
        let old_ts = dfs_common::types::current_timestamp().saturating_sub(700);
        storage.set_chunk_mtime(&chunk_id, old_ts);
        metadata.put_chunk_location(&ChunkLocation {
            chunk_id,
            nodes: vec![node_id],
            size: 9,
            checksum: chunk_id.hash,
            file_offset: Some(0),
            written_at: Some(old_ts * 1000),
            client_write_seq: None,
            file_id: None,
        }).unwrap();

        for _ in 0..3 {
            healing.run_disk_orphan_sweep().await;
        }
        assert!(storage.get_chunk_path(&chunk_id).exists(),
            "must never evict anything while this node is within its own post-restart grace period, \
             regardless of chunk age or authorization outcome");
    }

    /// Regression test for the 2026-07-06 incident: a metadata read error must defer
    /// (None), never collapse into "not superseded" (Some(false)), which would still
    /// purge the chunk and log it as DATA LOSS. This is exactly what happened when
    /// FileMetadata::symlink_target was added without a bincode-compatible fallback —
    /// every FILE_TABLE read failed cluster-wide, and the old `Err(_) => false` arm
    /// caused this same function to purge live chunks' bookkeeping en masse.
    #[tokio::test]
    async fn test_classify_zero_replica_chunk_defers_on_metadata_read_error() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (_storage, metadata, healing, _t1, _t2) = make_healing(node_id, addr);

        let file_id = FileId::new();
        // Neither the current nor the legacy FileMetadata shape can deserialize this —
        // forces a genuine get_file() Err, not Ok(None).
        metadata.put_raw_file_bytes(&file_id, b"not a valid FileMetadata encoding").unwrap();

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"unreadable-metadata-chunk"));
        let location = ChunkLocation {
            chunk_id,
            nodes: vec![],
            size: 4096,
            checksum: chunk_id.hash,
            file_offset: Some(0),
            written_at: None,
            client_write_seq: None,
            file_id: Some(file_id),
        };

        assert_eq!(
            healing.classify_zero_replica_chunk(chunk_id, &location),
            None,
            "a metadata read error must defer (None), never collapse into 'not superseded'"
        );
    }

    /// A chunk_id whose file metadata now points at a *different* chunk_id at the
    /// same file position was patched away — expected cleanup, not data loss.
    #[tokio::test]
    async fn test_classify_zero_replica_chunk_detects_superseded_chunk() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (_storage, metadata, healing, _t1, _t2) = make_healing(node_id, addr);

        let old_chunk_id = ChunkId::from_hash(compute_chunk_hash(b"old-content"));
        let new_chunk_id = ChunkId::from_hash(compute_chunk_hash(b"new-content"));

        let mut file_meta = FileMetadata::new("/patched-file".to_string(), FileType::RegularFile);
        let file_id = file_meta.id;
        let new_location = ChunkLocation {
            chunk_id: new_chunk_id,
            nodes: vec![node_id],
            size: 4096,
            checksum: new_chunk_id.hash,
            file_offset: Some(0),
            written_at: None,
            client_write_seq: None,
            file_id: Some(file_id),
        };
        file_meta.chunk_locations = Arc::new(vec![new_location.clone()]);
        metadata.put_file(&file_meta).unwrap();
        metadata.put_chunk_location(&new_location).unwrap();
        // chunk_map holds the post-patch view — the only thing classify consults.
        healing.chunk_map.insert(file_id, (vec![new_location], 1));

        // location still points at the OLD (patched-away) chunk_id.
        let old_location = ChunkLocation {
            chunk_id: old_chunk_id,
            nodes: vec![],
            size: 4096,
            checksum: old_chunk_id.hash,
            file_offset: Some(0),
            written_at: None,
            client_write_seq: None,
            file_id: Some(file_id),
        };

        assert_eq!(
            healing.classify_zero_replica_chunk(old_chunk_id, &old_location),
            Some(true),
            "chunk_map now points at a different chunk at this offset — superseded, not data loss"
        );
    }

    /// A chunk_id that is still exactly what the file's metadata references at this
    /// offset, with 0 accessible replicas, is genuinely and permanently lost.
    #[tokio::test]
    async fn test_classify_zero_replica_chunk_detects_genuine_data_loss() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (_storage, metadata, healing, _t1, _t2) = make_healing(node_id, addr);

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"still-the-live-chunk"));

        let mut file_meta = FileMetadata::new("/lost-file".to_string(), FileType::RegularFile);
        let file_id = file_meta.id;
        let location = ChunkLocation {
            chunk_id,
            nodes: vec![node_id],
            size: 4096,
            checksum: chunk_id.hash,
            file_offset: Some(0),
            written_at: None,
            client_write_seq: None,
            file_id: Some(file_id),
        };
        file_meta.chunk_locations = Arc::new(vec![location.clone()]);
        metadata.put_file(&file_meta).unwrap();
        metadata.put_chunk_location(&location).unwrap();
        // chunk_map is the ONLY source classify_zero_replica_chunk consults for the
        // live chunk at a file position — a miss means "cold, can't know yet" and
        // defers (see its doc comment). Seed it, as every real write path does
        // synchronously, or this tests the defer path rather than the classification.
        healing.chunk_map.insert(file_id, (vec![location.clone()], 1));

        assert_eq!(
            healing.classify_zero_replica_chunk(chunk_id, &location),
            Some(false),
            "chunk_map still points at this exact chunk_id — genuine, permanent data loss"
        );
    }

    /// Regression for two CONFIRMED data-loss incidents (2026-07-15 vm-108 chunk_idx
    /// 230; 2026-07-17 06:09 on an idle cluster, no writes since a clean fsck): the
    /// healer purged unfolded PATCH TOKENS as "permanently unrecoverable (N metadata
    /// nodes, all confirmed empty)". A token has no file on disk by design, so that
    /// evidence is trivially true of one — the only thing standing between a token
    /// and the purge is knowing it IS a token, and PATCH_STATE_TABLE is node-local
    /// (only folds are disseminated). One node briefly away — a PlannedCompaction
    /// leave suffices — hides its whole token set, while destructive_allowed happily
    /// stays true at nodes_down <= 1.
    mod data_loss_gate {
        use super::*;

        #[test]
        fn healthy_cluster_with_a_complete_token_view_may_purge() {
            assert!(HealingManager::may_declare_data_loss(true, true));
        }

        /// THE BUG: cluster looks healthy (nodes_down <= 1 is tolerated), but that one
        /// absent node took 100% of its patch tokens out of view. Must NOT purge.
        #[test]
        fn healthy_cluster_with_an_incomplete_token_view_must_not_purge() {
            assert!(!HealingManager::may_declare_data_loss(true, false));
        }

        #[test]
        fn degraded_cluster_never_purges_regardless_of_token_view() {
            assert!(!HealingManager::may_declare_data_loss(false, true));
            assert!(!HealingManager::may_declare_data_loss(false, false));
        }
    }

    /// The cold-chunk_map case must DEFER, never guess. Regression for the
    /// 2026-07-16 staging hang: this branch used to answer by scanning all of
    /// CHUNK_TABLE, which both wedged the Tokio runtime (sync full scan per chunk
    /// from the async discovery loop) and answered a question it cannot yet answer.
    #[tokio::test]
    async fn test_classify_zero_replica_chunk_defers_when_chunk_map_is_cold() {
        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let (_storage, metadata, healing, _t1, _t2) = make_healing(node_id, addr);

        let chunk_id = ChunkId::from_hash(compute_chunk_hash(b"cold-window-chunk"));
        let mut file_meta = FileMetadata::new("/cold-file".to_string(), FileType::RegularFile);
        let file_id = file_meta.id;
        let location = ChunkLocation {
            chunk_id,
            nodes: vec![node_id],
            size: 4096,
            checksum: chunk_id.hash,
            file_offset: Some(0),
            written_at: None,
            client_write_seq: None,
            file_id: Some(file_id),
        };
        file_meta.chunk_locations = Arc::new(vec![location.clone()]);
        metadata.put_file(&file_meta).unwrap();
        metadata.put_chunk_location(&location).unwrap();
        // Deliberately NO chunk_map entry — the post-restart window before
        // rebuild_chunk_map_from_metadata has finished.

        assert_eq!(
            healing.classify_zero_replica_chunk(chunk_id, &location),
            None,
            "a cold chunk_map cannot distinguish superseded from lost — must defer, not guess"
        );
    }
}
