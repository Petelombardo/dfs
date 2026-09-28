//! Leader terms and node leases (SLOT-OWNERSHIP-PLAN.md Phase 1).
//!
//! A node may act as primary for its slots only while it holds a lease from the
//! leader. The whole scheme exists to guarantee one thing: **a node the leader has
//! declared expired has already stopped acting as primary.** Everything below is
//! arranged so that each side's clock errs in the safe direction:
//!
//! - The holder counts its lease from when it *sent* the renewal, not when the
//!   grant arrived, and gives up `margin` early. So it stops no later than
//!   `sent + lease - margin`.
//! - The leader counts from when it *received* the renewal, which is after the send,
//!   and adds `margin`. So it declares expiry no earlier than `recv + lease + margin`.
//!
//! Leadership changes are ordered by terms. A leader must win a promise for a new
//! term from a majority before granting anything, and may grant only while a
//! majority keeps renewing with it under that term. A deposed leader on the minority
//! side of a partition therefore loses the ability to grant within one lease period.
//! Its last grant can run one more lease period, so a new leader treats every node
//! as `Unknown` (never `Expired`) for `2 * lease + margin` after establishing its
//! term. That wait replaces a replicated lease table: nothing about old leases has
//! to survive a leadership change.
//!
//! All decision logic takes `now` as a parameter and does no I/O, so the safety
//! properties are tested deterministically rather than by racing real clocks.

use dfs_common::NodeId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct LeaseConfig {
    pub lease: Duration,
    pub margin: Duration,
}

impl LeaseConfig {
    /// DFS_LEASE_MS (default 10000) and DFS_LEASE_MARGIN_MS (default 1000).
    pub fn from_env() -> Self {
        let ms = |k: &str, d: u64| {
            Duration::from_millis(std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d))
        };
        Self { lease: ms("DFS_LEASE_MS", 10_000), margin: ms("DFS_LEASE_MARGIN_MS", 1_000) }
    }

    /// How often a holder renews: three chances per lease period.
    pub fn renew_every(&self) -> Duration {
        self.lease / 3
    }

    /// How long a newly established leader treats every node as `Unknown`.
    pub fn takeover_wait(&self) -> Duration {
        self.lease * 2 + self.margin
    }
}

/// The leader's view of one node's lease, as published to every node.
pub use dfs_common::NodeLeaseState as LeaseState;

// ---------------------------------------------------------------------------
// Leader side
// ---------------------------------------------------------------------------

/// State kept only by the node that currently believes it is leader.
#[derive(Debug)]
pub struct LeaderState {
    pub term: u64,
    established_at: Instant,
    /// When each node's latest renewal under this term arrived.
    last_renewal: HashMap<NodeId, Instant>,
}

impl LeaderState {
    /// Called once a majority has promised `term`.
    pub fn established(term: u64, now: Instant) -> Self {
        Self { term, established_at: now, last_renewal: HashMap::new() }
    }

    /// Record a renewal from `node` that arrived at `now`, and answer it. The
    /// leader's own renewals come through here too; they count toward its majority.
    /// Returns false (no grant) unless the leader still has a majority behind it.
    pub fn renew(&mut self, node: NodeId, now: Instant, cluster_size: usize, cfg: &LeaseConfig) -> bool {
        self.last_renewal.insert(node, now);
        self.has_majority(now, cluster_size, cfg)
    }

    /// Every node that has renewed under this term.
    pub fn known_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.last_renewal.keys().copied()
    }

    /// A majority of the cluster (the leader included) has renewed within the last
    /// lease period. Without it this leader may already be deposed, so it grants nothing.
    pub fn has_majority(&self, now: Instant, cluster_size: usize, cfg: &LeaseConfig) -> bool {
        let fresh = self.last_renewal.values()
            .filter(|t| now.saturating_duration_since(**t) < cfg.lease)
            .count();
        fresh >= cluster_size / 2 + 1
    }

    /// This leader's view of `node`'s lease. `Expired` only once the node has
    /// provably stopped: `lease + margin` past its last renewal's arrival, and the
    /// takeover wait for leases from earlier terms has passed.
    pub fn state_of(&self, node: NodeId, now: Instant, cfg: &LeaseConfig) -> LeaseState {
        if let Some(t) = self.last_renewal.get(&node) {
            if now.saturating_duration_since(*t) < cfg.lease + cfg.margin {
                return LeaseState::Valid;
            }
        }
        if now.saturating_duration_since(self.established_at) < cfg.takeover_wait() {
            LeaseState::Unknown
        } else {
            LeaseState::Expired
        }
    }
}

// ---------------------------------------------------------------------------
// Holder side (every node, the leader included)
// ---------------------------------------------------------------------------

/// A node's own lease, plus the latest lease view the leader sent it.
#[derive(Debug, Default)]
pub struct HolderState {
    /// End of this node's own lease, already shortened by `margin`.
    valid_until: Option<Instant>,
    /// Term and leader of the grant behind `valid_until`.
    pub granted_term: u64,
    pub granted_by: Option<NodeId>,
    /// The leader's view of every node, as of the last grant.
    pub view: HashMap<NodeId, LeaseState>,
}

impl HolderState {
    /// A grant arrived for a renewal this node sent at `sent_at`.
    pub fn on_grant(&mut self, sent_at: Instant, term: u64, leader: NodeId,
                    view: HashMap<NodeId, LeaseState>, cfg: &LeaseConfig) {
        let until = sent_at + cfg.lease.saturating_sub(cfg.margin);
        // Never let an older term's grant, or a reordered earlier grant, move the
        // lease around: keep the latest end within the highest term seen.
        if term > self.granted_term || self.valid_until.is_none_or(|u| until > u) {
            self.valid_until = Some(until);
        }
        if term >= self.granted_term {
            self.granted_term = term;
            self.granted_by = Some(leader);
            self.view = view;
        }
    }

    /// Whether this node may act as primary right now.
    pub fn holds_lease(&self, now: Instant) -> bool {
        self.valid_until.is_some_and(|u| now < u)
    }

    pub fn remaining(&self, now: Instant) -> Duration {
        self.valid_until.map_or(Duration::ZERO, |u| u.saturating_duration_since(now))
    }
}

/// The primary for a slot with in-sync replica list `isr` (ordered; see the plan's
/// "ISR order"): its first member whose lease is live. `me` is judged by its own
/// lease, everyone else by the leader's published view, where only `Expired` lets
/// the next member take over. Returns None if the first live-or-unknown member is
/// not provably the primary (a member ahead of it is `Unknown`).
pub fn primary_of(isr: &[NodeId], me: NodeId, holder: &HolderState, now: Instant) -> Option<NodeId> {
    for &n in isr {
        let live = if n == me {
            holder.holds_lease(now)
        } else {
            match holder.view.get(&n) {
                Some(LeaseState::Valid) => true,
                Some(LeaseState::Expired) => false,
                // Unknown, or not in the view at all: can't skip past it safely.
                Some(LeaseState::Unknown) | None => return None,
            }
        };
        if live {
            return Some(n);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Promised term (durable, every node)
// ---------------------------------------------------------------------------

/// The highest term this node has promised, and to whom. Durable: a node that
/// forgot its promise across a restart could help two leaders win the same term.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Promise {
    pub term: u64,
    pub leader: Option<NodeId>,
}

impl Promise {
    /// Accept `term` from `leader` if it's newer than anything promised, or the same
    /// term again from the same leader (a retry). Returns whether it was accepted.
    pub fn offer(&mut self, term: u64, leader: NodeId) -> bool {
        if term > self.term || (term == self.term && self.leader == Some(leader)) {
            self.term = term;
            self.leader = Some(leader);
            true
        } else {
            false
        }
    }
}

/// The largest cluster membership this node has ever seen, persisted. Majorities are
/// counted over at least this many nodes. Counting over the membership a node happens
/// to know right now is unsound: a node that has just started knows only itself, and
/// on the first local run of this code all five nodes each won "1 of 1" and led term 1
/// at once. A permanently removed node keeps the majority at the old size until this
/// is lowered, which makes leases harder to win, never easier.
pub struct MembershipFile {
    path: PathBuf,
}

impl MembershipFile {
    pub fn new(dir: &Path) -> Self {
        Self { path: dir.join("lease_membership.json") }
    }

    pub fn load(&self) -> usize {
        std::fs::read(&self.path).ok()
            .and_then(|b| serde_json::from_slice::<usize>(&b).ok())
            .unwrap_or(0)
    }

    pub fn store(&self, n: usize) -> std::io::Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&n).expect("usize serializes"))?;
        std::fs::File::open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, &self.path)
    }
}

pub struct PromiseFile {
    path: PathBuf,
}

impl PromiseFile {
    pub fn new(dir: &Path) -> Self {
        Self { path: dir.join("lease_promise.json") }
    }

    pub fn load(&self) -> Promise {
        std::fs::read(&self.path).ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Write-then-rename with fsync, so a crash leaves either the old or the new promise.
    pub fn store(&self, p: &Promise) -> std::io::Result<()> {
        use std::io::Write;
        let tmp = self.path.with_extension("json.tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec(p).expect("Promise always serializes"))?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        if let Some(dir) = self.path.parent() {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Runtime: the loops and request handlers that drive the state machines above
// ---------------------------------------------------------------------------

use crate::cluster::ClusterManager;
use crate::network::NetworkClient;
use dfs_common::{LeaseStatusReport, Message, Request, Response};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// Owns all lease state for this node. Deliberately isolated: its locks are its own
/// std Mutexes, held only for in-memory updates, and the renewal handler never
/// touches the metadata DB, the healer, chunk_map or the cluster membership lock.
/// A stall in any of those must not cost anyone a lease (plan: Phase 1 leader-stall gate).
pub struct LeaseRuntime {
    cfg: LeaseConfig,
    me: NodeId,
    cluster: Arc<ClusterManager>,
    client: Arc<NetworkClient>,
    promise_file: PromiseFile,
    promise: Mutex<Promise>,
    leader: Mutex<Option<LeaderState>>,
    holder: Mutex<HolderState>,
    /// The size majorities are counted over: max(membership known now, persisted
    /// high-water mark, DFS_LEASE_CLUSTER_SIZE). Refreshed by the loop so the renewal
    /// handler never takes the cluster membership lock.
    cluster_size: std::sync::atomic::AtomicUsize,
    membership_file: MembershipFile,
    /// Whether this node held its lease at the end of the previous tick.
    held_last_tick: std::sync::atomic::AtomicBool,
    /// Nodes already logged as Expired (each transition is logged once).
    expired_logged: Mutex<std::collections::BTreeSet<NodeId>>,
    /// Wall-clock end of the lease this node last held, for the "lost" log line.
    last_until_wall_ms: Mutex<Option<u128>>,
}

/// Wall-clock milliseconds for an `Instant`, for logs that a test compares across processes.
fn wall_ms(at: Instant) -> u128 {
    let now_i = Instant::now();
    let now_s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    if at >= now_i {
        (now_s + (at - now_i)).as_millis()
    } else {
        now_s.saturating_sub(now_i - at).as_millis()
    }
}

impl LeaseRuntime {
    pub fn new(me: NodeId, cluster: Arc<ClusterManager>, client: Arc<NetworkClient>, dir: &Path) -> Self {
        let promise_file = PromiseFile::new(dir);
        let promise = promise_file.load();
        let membership_file = MembershipFile::new(dir);
        let floor = std::env::var("DFS_LEASE_CLUSTER_SIZE").ok().and_then(|v| v.parse().ok()).unwrap_or(0usize);
        let size = membership_file.load().max(floor).max(1);
        Self {
            cfg: LeaseConfig::from_env(),
            me,
            cluster,
            client,
            promise_file,
            promise: Mutex::new(promise),
            leader: Mutex::new(None),
            holder: Mutex::new(HolderState::default()),
            cluster_size: std::sync::atomic::AtomicUsize::new(size),
            membership_file,
            held_last_tick: std::sync::atomic::AtomicBool::new(false),
            expired_logged: Mutex::new(std::collections::BTreeSet::new()),
            last_until_wall_ms: Mutex::new(None),
        }
    }

    pub fn start(self: Arc<Self>) {
        let rt = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(rt.cfg.renew_every());
            loop {
                tick.tick().await;
                rt.refresh_cluster_size().await;
                rt.lead_if_leader().await;
                rt.renew_own_lease().await;
            }
        });
        info!("LEASE: started (lease {:?}, margin {:?})", self.cfg.lease, self.cfg.margin);
    }

    async fn refresh_cluster_size(&self) {
        let known = self.cluster.get_all_nodes().await.len();
        let current = self.cluster_size.load(std::sync::atomic::Ordering::Relaxed);
        if known > current {
            // Grow only. Persist before using it, so a restart can't forget a larger cluster.
            let file = MembershipFile { path: self.membership_file.path.clone() };
            if matches!(tokio::task::spawn_blocking(move || file.store(known)).await, Ok(Ok(()))) {
                self.cluster_size.store(known, std::sync::atomic::Ordering::Relaxed);
                info!("LEASE: membership high-water now {} (majority {})", known, known / 2 + 1);
            }
        }
    }

    fn majority(&self) -> usize {
        self.cluster_size.load(std::sync::atomic::Ordering::Relaxed) / 2 + 1
    }

    /// Become an established leader (win a term from a majority) if membership says
    /// this node leads, or drop leader state if it no longer does.
    async fn lead_if_leader(&self) {
        let should_lead = self.cluster.is_leader().await;
        let leading = self.leader.lock().unwrap().is_some();
        if !should_lead {
            if leading {
                *self.leader.lock().unwrap() = None;
                info!("LEASE: no longer leader; stopped granting");
            }
            return;
        }
        if leading {
            return;
        }
        // Discover the highest term a majority has promised.
        let peers: Vec<_> = self.cluster.get_all_nodes().await.into_iter()
            .filter(|n| n.id != self.me && n.status == dfs_common::NodeStatus::Online)
            .collect();
        let mut max_term = self.promise.lock().unwrap().term;
        let mut answered = 1; // this node
        for p in &peers {
            if let Some(Response::LeaseTerm { promised_term, .. }) = self.ask(p.addr, Request::GetLeaseTerm).await {
                answered += 1;
                max_term = max_term.max(promised_term);
            }
        }
        if answered < self.majority() {
            debug!("LEASE: term discovery reached {} of {} needed", answered, self.majority());
            return;
        }
        let term = max_term + 1;
        if !self.accept_promise(term, self.me).await {
            return;
        }
        let mut accepted = 1;
        for p in &peers {
            if let Some(Response::LeaseTermPromise { accepted: true, .. }) =
                self.ask(p.addr, Request::PromiseLeaseTerm { term, leader: self.me }).await
            {
                accepted += 1;
            }
        }
        if accepted >= self.majority() {
            *self.leader.lock().unwrap() = Some(LeaderState::established(term, Instant::now()));
            info!("LEASE: established as leader for term {} ({} of {} promised); every node Unknown for {:?}",
                term, accepted, self.cluster_size.load(std::sync::atomic::Ordering::Relaxed), self.cfg.takeover_wait());
        } else {
            info!("LEASE: term {} won only {} promises (need {}); will retry", term, accepted, self.majority());
        }
    }

    async fn ask(&self, addr: std::net::SocketAddr, req: Request) -> Option<Response> {
        match self.client.send_message_timeout(addr, Message::Request(req), self.cfg.renew_every()).await {
            Ok(env) => match env.message {
                Message::Response(r) => Some(r),
                _ => None,
            },
            Err(e) => {
                debug!("LEASE: {} unreachable: {}", addr, e);
                None
            }
        }
    }

    /// Promise `term` to `leader` if allowed, persisting before answering. A promise
    /// that isn't on disk doesn't count: a restart must not forget it.
    async fn accept_promise(&self, term: u64, leader: NodeId) -> bool {
        let mut next = *self.promise.lock().unwrap();
        if !next.offer(term, leader) {
            return false;
        }
        let file = PromiseFile { path: self.promise_file.path.clone() };
        let stored = tokio::task::spawn_blocking(move || file.store(&next)).await;
        if !matches!(stored, Ok(Ok(()))) {
            warn!("LEASE: failed to persist promise of term {} to {}: {:?}", term, leader, stored);
            return false;
        }
        let mut p = self.promise.lock().unwrap();
        if next.term >= p.term {
            *p = next;
        }
        // Promising a newer term to someone else deposes any leadership held here.
        let mut l = self.leader.lock().unwrap();
        if l.as_ref().is_some_and(|s| s.term < term) && leader != self.me {
            *l = None;
            info!("LEASE: promised term {} to {}; stepped down", term, leader);
        }
        true
    }

    /// Renew this node's own lease with whoever membership says leads.
    async fn renew_own_lease(&self) {
        let sent = Instant::now();
        let promised_term = self.promise.lock().unwrap().term;
        let leader_addr = self.cluster.get_leader_addr().await;
        let answer = if leader_addr == Some(self.cluster.local_addr()) {
            Some(self.handle_renew(self.me, promised_term))
        } else if let Some(addr) = leader_addr {
            self.ask(addr, Request::RenewNodeLease { node: self.me, promised_term }).await
        } else {
            None
        };
        if let Some(Response::NodeLeaseGrant { granted: true, term, leader, view }) = answer {
            if term < self.promise.lock().unwrap().term {
                debug!("LEASE: ignoring grant from term {} (promised a newer one)", term);
            } else {
                let mut h = self.holder.lock().unwrap();
                let old_view = std::mem::take(&mut h.view);
                h.on_grant(sent, term, leader, view.into_iter().collect(), &self.cfg);
                for (n, st) in &h.view {
                    if old_view.get(n) != Some(st) {
                        info!("LEASE view: node {} is {:?} (term {})", n, st, term);
                    }
                }
            }
        }
        let h = self.holder.lock().unwrap();
        let now = Instant::now();
        let has = h.holds_lease(now);
        // Compare with the previous tick, not with the send time: a refused renewal fails
        // instantly, so the lease usually lapses between ticks rather than during one.
        let had = self.held_last_tick.swap(has, std::sync::atomic::Ordering::Relaxed);
        if has && !had {
            info!("LEASE own: acquired (term {}, until_wall_ms {})", h.granted_term, wall_ms(now + h.remaining(now)));
        } else if has {
            debug!("LEASE own: renewed, until_wall_ms {}", wall_ms(now + h.remaining(now)));
        } else if had {
            // The exact end, so a test can check it precedes the leader's Expired declaration.
            info!("LEASE own: lost; it ended at wall_ms {}",
                self.last_until_wall_ms.lock().unwrap().map_or("?".to_string(), |w| w.to_string()));
        }
        if has {
            *self.last_until_wall_ms.lock().unwrap() = Some(wall_ms(now + h.remaining(now)));
        }
    }

    /// Leader-side answer to a renewal. Pure in-memory work under this runtime's own locks.
    pub fn handle_renew(&self, node: NodeId, promised_term: u64) -> Response {
        let now = Instant::now();
        let size = self.cluster_size.load(std::sync::atomic::Ordering::Relaxed);
        let mut guard = self.leader.lock().unwrap();
        let Some(state) = guard.as_mut() else {
            return Response::NodeLeaseGrant { granted: false, term: 0, leader: self.me, view: vec![] };
        };
        if promised_term > state.term {
            // The renewing node has promised a newer term: this leadership is over.
            info!("LEASE: node {} promised term {} > ours {}; stepping down", node, promised_term, state.term);
            *guard = None;
            return Response::NodeLeaseGrant { granted: false, term: promised_term, leader: self.me, view: vec![] };
        }
        let granted = state.renew(node, now, size, &self.cfg);
        let term = state.term;
        let view = self.view_of(state, now);
        Response::NodeLeaseGrant { granted, term, leader: self.me, view }
    }

    fn view_of(&self, state: &LeaderState, now: Instant) -> Vec<(NodeId, LeaseState)> {
        let mut ids: Vec<NodeId> = state.known_nodes().collect();
        ids.sort();
        let mut out = Vec::with_capacity(ids.len());
        let mut logged = self.expired_logged.lock().unwrap();
        for id in ids {
            let st = state.state_of(id, now, &self.cfg);
            if st == LeaseState::Expired && logged.insert(id) {
                info!("LEASE leader: node {} Expired at wall_ms {} (term {})", id, wall_ms(now), state.term);
            } else if st != LeaseState::Expired {
                logged.remove(&id);
            }
            out.push((id, st));
        }
        out
    }

    pub fn handle_get_term(&self) -> Response {
        let p = *self.promise.lock().unwrap();
        Response::LeaseTerm { promised_term: p.term, promised_leader: p.leader }
    }

    pub async fn handle_promise(&self, term: u64, leader: NodeId) -> Response {
        let accepted = self.accept_promise(term, leader).await;
        Response::LeaseTermPromise { accepted, promised_term: self.promise.lock().unwrap().term }
    }

    pub fn status(&self) -> LeaseStatusReport {
        let now = Instant::now();
        let p = *self.promise.lock().unwrap();
        let h = self.holder.lock().unwrap();
        let l = self.leader.lock().unwrap();
        let mut view: Vec<_> = h.view.iter().map(|(k, v)| (*k, *v)).collect();
        view.sort_by_key(|(k, _)| *k);
        LeaseStatusReport {
            node: self.me,
            promised_term: p.term,
            promised_leader: p.leader,
            holds_lease: h.holds_lease(now),
            lease_remaining_ms: h.remaining(now).as_millis() as u64,
            granted_term: h.granted_term,
            granted_by: h.granted_by,
            view,
            leader_term: l.as_ref().map(|s| s.term),
            leader_has_majority: l.as_ref().is_some_and(|s| {
                s.has_majority(now, self.cluster_size.load(std::sync::atomic::Ordering::Relaxed), &self.cfg)
            }),
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LeaseConfig {
        LeaseConfig { lease: Duration::from_secs(10), margin: Duration::from_secs(1) }
    }
    fn ids(n: usize) -> Vec<NodeId> {
        (0..n).map(|i| NodeId::from_bytes([i as u8 + 1; 16])).collect()
    }
    fn s(x: u64) -> Duration { Duration::from_secs(x) }

    /// The core safety property: at every instant where the leader calls a node
    /// Expired, that node already considers its own lease gone. Checked over a
    /// sweep of renewal-send times, network delays and observation times.
    #[test]
    fn leader_never_declares_expired_while_holder_still_holds() {
        let c = cfg();
        let n = ids(3);
        let t0 = Instant::now();
        for delay_ms in [0u64, 1, 250, 900, 2_500] {
            let mut leader = LeaderState::established(1, t0);
            let mut holder = HolderState::default();
            let sent = t0 + s(30); // well past the takeover wait
            let recv = sent + Duration::from_millis(delay_ms);
            leader.renew(n[0], recv, 3, &c);
            leader.renew(n[1], recv, 3, &c);
            leader.renew(n[2], recv, 3, &c);
            holder.on_grant(sent, 1, n[1], HashMap::new(), &c);
            for step_ms in (0..40_000u64).step_by(50) {
                let now = sent + Duration::from_millis(step_ms);
                if leader.state_of(n[0], now, &c) == LeaseState::Expired {
                    assert!(!holder.holds_lease(now),
                        "overlap at +{}ms with {}ms delay: leader says Expired, holder still holds", step_ms, delay_ms);
                }
            }
        }
    }

    /// A new leader must not call anyone Expired until leases from the previous
    /// term can't still be running, even for nodes it has never heard from.
    #[test]
    fn new_leader_waits_out_previous_terms_leases() {
        let c = cfg();
        let n = ids(3);
        let t0 = Instant::now();
        let leader = LeaderState::established(2, t0);
        assert_eq!(leader.state_of(n[2], t0 + c.takeover_wait() - Duration::from_millis(1), &c), LeaseState::Unknown);
        assert_eq!(leader.state_of(n[2], t0 + c.takeover_wait(), &c), LeaseState::Expired);

        // Worst case for the old term: its last grant went out the instant before the
        // new term formed, from an old leader that still (just) had its majority.
        let mut holder = HolderState::default();
        holder.on_grant(t0, 1, n[0], HashMap::new(), &c);
        for step_ms in (0..40_000u64).step_by(50) {
            let now = t0 + Duration::from_millis(step_ms);
            if leader.state_of(n[2], now, &c) == LeaseState::Expired {
                assert!(!holder.holds_lease(now), "old-term lease outlived the takeover wait at +{}ms", step_ms);
            }
        }
    }

    /// A leader cut off from its majority stops granting within one lease period,
    /// which is what bounds how long an old term's grants keep appearing.
    #[test]
    fn leader_without_majority_stops_granting() {
        let c = cfg();
        let n = ids(5);
        let t0 = Instant::now();
        let mut leader = LeaderState::established(1, t0);
        for id in &n { assert!(leader.renew(*id, t0, 5, &c) || id != &n[4]); }
        assert!(leader.has_majority(t0, 5, &c));
        // Partition: only the leader and one other keep renewing.
        let later = t0 + c.lease + s(1);
        assert!(!leader.renew(n[0], later, 5, &c), "2 of 5 fresh is not a majority");
        assert!(!leader.renew(n[1], later, 5, &c));
        assert!(!leader.has_majority(later, 5, &c));
    }

    /// 2026-09-28 local T58: every node, just started and knowing only itself, won
    /// "1 of 1" and established itself leader for term 1, five leaders at once. A node
    /// that has ever seen a 5-node cluster must need 3 promises, whatever it knows now.
    #[tokio::test]
    async fn restarted_node_alone_cannot_establish_a_term() {
        let dir = tempfile::TempDir::new().unwrap();
        MembershipFile::new(dir.path()).store(5).unwrap();
        let me = ids(1)[0];
        let cluster = Arc::new(ClusterManager::new(me, "127.0.0.1:19350".parse().unwrap(), 10, 30));
        assert!(cluster.is_leader().await, "precondition: membership alone says this node leads (1 of 1)");
        let rt = LeaseRuntime::new(me, cluster, Arc::new(NetworkClient::new()), dir.path());
        rt.refresh_cluster_size().await;
        rt.lead_if_leader().await;
        assert!(rt.leader.lock().unwrap().is_none(),
            "a node that has seen 5 members established a term on its own: split-brain");
        assert_eq!(rt.majority(), 3);
    }

    #[test]
    fn promise_accepts_only_newer_terms_or_same_leader_retry() {
        let n = ids(2);
        let mut p = Promise::default();
        assert!(p.offer(3, n[0]));
        assert!(p.offer(3, n[0]), "same term, same leader: a retry");
        assert!(!p.offer(3, n[1]), "same term, other leader: two leaders must not both win it");
        assert!(!p.offer(2, n[1]), "older term");
        assert!(p.offer(4, n[1]));
    }

    #[test]
    fn promise_survives_a_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = PromiseFile::new(dir.path());
        assert_eq!(f.load(), Promise::default());
        let p = Promise { term: 7, leader: Some(ids(1)[0]) };
        f.store(&p).unwrap();
        assert_eq!(PromiseFile::new(dir.path()).load(), p);
    }

    /// Takeover happens only past a member the leader has declared Expired, never
    /// past Unknown, and the holder judges itself by its own lease.
    #[test]
    fn primary_follows_isr_order_and_needs_expired_to_skip() {
        let c = cfg();
        let n = ids(2);
        let t0 = Instant::now();
        let isr = [n[0], n[1]];
        let mut view = HashMap::new();

        // Seen from S (n[1]): P valid -> P is primary.
        let mut s_holder = HolderState::default();
        view.insert(n[0], LeaseState::Valid);
        s_holder.on_grant(t0, 1, n[0], view.clone(), &c);
        assert_eq!(primary_of(&isr, n[1], &s_holder, t0), Some(n[0]));

        // P unknown (new leader waiting): nobody may take over.
        view.insert(n[0], LeaseState::Unknown);
        s_holder.on_grant(t0, 2, n[0], view.clone(), &c);
        assert_eq!(primary_of(&isr, n[1], &s_holder, t0), None);

        // P expired: S takes over, but only while S's own lease is live.
        view.insert(n[0], LeaseState::Expired);
        s_holder.on_grant(t0, 2, n[0], view.clone(), &c);
        assert_eq!(primary_of(&isr, n[1], &s_holder, t0), Some(n[1]));
        assert_eq!(primary_of(&isr, n[1], &s_holder, t0 + c.lease), None, "S's own lease ran out");

        // Seen from P itself: primary exactly while its own lease holds.
        let mut p_holder = HolderState::default();
        p_holder.on_grant(t0, 1, n[0], HashMap::new(), &c);
        assert_eq!(primary_of(&isr, n[0], &p_holder, t0), Some(n[0]));
        assert!(primary_of(&isr, n[0], &p_holder, t0 + c.lease).is_none());
    }
}
