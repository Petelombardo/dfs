//! Majority node leases (SLOT-OWNERSHIP-PLAN.md Phase 1).
//!
//! A node may act as primary for its slots only while it holds a lease, and it holds
//! a lease only while a **majority** of the cluster keeps acknowledging its renewals.
//! No leader is involved, so a leader's death costs no write availability; only the
//! death or isolation of a node costs that node's slots, after about `lease + margin`.
//!
//! The guarantee everything below serves: **once a majority has voted node P expired
//! at incarnation i, P holds no lease at any incarnation <= i.**
//!
//! - Renewal: P asks every peer. Each ack is recorded by the voter at its receive time.
//!   P holds its lease until `sent + lease - margin` if a majority (P included) acked a
//!   renewal it sent at `sent`. Counting from the send and giving up a margin early
//!   errs toward P stopping sooner.
//! - Expiry vote: a voter votes P expired only if it has not acked P for
//!   `lease + margin`. Once it votes, it refuses P's renewals at or below that
//!   incarnation. Any majority that kept P's lease alive shares at least one voter with
//!   any majority that voted P out, and that voter can do neither inside the other's
//!   window. After a vote, every majority P could reach contains a voter that refuses it.
//! - A vote names the incarnation it expires. Each voter answers with the highest
//!   incarnation it has seen from P, and the vote fails if any voter has seen a newer
//!   one: P's latest lease came from a majority, so some voter in any majority knows it.
//! - A voter persists its fences before answering, and casts no expiry vote for
//!   `lease + margin` after starting, because it may have acked P just before a crash.
//! - A fenced node rejoins by renewing under a higher incarnation.
//!
//! All decision logic takes `now` as a parameter and does no I/O, so the safety
//! properties are tested deterministically, including a seeded randomized simulation.

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
    /// DFS_LEASE_MS (default 3000) and DFS_LEASE_MARGIN_MS (default 500). Short on
    /// purpose: a primary's failover takes about lease + margin, and it has to fit well
    /// inside a guest's 30 s SCSI timeout.
    pub fn from_env() -> Self {
        let ms = |k: &str, d: u64| {
            Duration::from_millis(std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d))
        };
        Self { lease: ms("DFS_LEASE_MS", 3_000), margin: ms("DFS_LEASE_MARGIN_MS", 500) }
    }

    /// How often a holder renews: three chances per lease period.
    pub fn renew_every(&self) -> Duration {
        self.lease / 3
    }

    /// How long a voter must not have acked a node before voting it expired, and how
    /// long a restarted voter abstains.
    pub fn silence(&self) -> Duration {
        self.lease + self.margin
    }
}

/// How one node sees another's lease, for status reports and (from Phase 3) primary
/// takeover. Only `Expired` makes it safe to act in that node's place.
pub use dfs_common::NodeLeaseState as LeaseState;

// ---------------------------------------------------------------------------
// Holder side
// ---------------------------------------------------------------------------

/// A node's own lease.
#[derive(Debug)]
pub struct HolderState {
    pub incarnation: u64,
    valid_until: Option<Instant>,
}

impl HolderState {
    pub fn new(incarnation: u64) -> Self {
        Self { incarnation, valid_until: None }
    }

    /// A renewal round sent at `sent` got `acks` acknowledgements, this node included.
    pub fn on_round(&mut self, sent: Instant, acks: usize, majority: usize, cfg: &LeaseConfig) {
        if acks >= majority {
            let until = sent + cfg.lease.saturating_sub(cfg.margin);
            if self.valid_until.is_none_or(|u| until > u) {
                self.valid_until = Some(until);
            }
        }
    }

    /// A voter said this node is fenced at `fenced`: drop the lease and rejoin above it.
    /// Returns true if the incarnation changed (the caller persists it before renewing).
    pub fn on_fenced(&mut self, fenced: u64) -> bool {
        if fenced >= self.incarnation {
            self.incarnation = fenced + 1;
            self.valid_until = None;
            true
        } else {
            false
        }
    }

    pub fn holds_lease(&self, now: Instant) -> bool {
        self.valid_until.is_some_and(|u| now < u)
    }

    pub fn remaining(&self, now: Instant) -> Duration {
        self.valid_until.map_or(Duration::ZERO, |u| u.saturating_duration_since(now))
    }
}

// ---------------------------------------------------------------------------
// Voter side (every node, about every other node)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    pub acked: bool,
    /// Highest incarnation of the renewing node this voter has fenced.
    pub fenced: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vote {
    pub granted: bool,
    /// Highest incarnation of the target this voter has seen renew.
    pub highest_seen: u64,
}

/// What this node knows about other nodes' leases. `fences` must be persisted
/// before a granted vote is reported (see `LeaseStateFile`).
#[derive(Debug)]
pub struct VoterState {
    started_at: Instant,
    /// Last ack given to each node: when (receive time) and the highest incarnation seen.
    last_ack: HashMap<NodeId, (Instant, u64)>,
    /// Highest incarnation of each node this voter has voted expired.
    pub fences: HashMap<NodeId, u64>,
}

impl VoterState {
    pub fn new(started_at: Instant, fences: HashMap<NodeId, u64>) -> Self {
        Self { started_at, last_ack: HashMap::new(), fences }
    }

    pub fn on_renew(&mut self, node: NodeId, incarnation: u64, now: Instant) -> Ack {
        let fenced = self.fences.get(&node).copied().unwrap_or(0);
        if self.fences.contains_key(&node) && incarnation <= fenced {
            return Ack { acked: false, fenced };
        }
        let seen = self.last_ack.get(&node).map_or(incarnation, |(_, i)| (*i).max(incarnation));
        self.last_ack.insert(node, (now, seen));
        Ack { acked: true, fenced }
    }

    /// Whether this voter could vote `target` expired now: it has been silent about
    /// the target for long enough, and it isn't still in its post-restart abstention.
    pub fn could_vote(&self, target: NodeId, now: Instant, cfg: &LeaseConfig) -> bool {
        if now.saturating_duration_since(self.started_at) < cfg.silence() {
            return false;
        }
        self.last_ack.get(&target).is_none_or(|(t, _)| now.saturating_duration_since(*t) >= cfg.silence())
    }

    /// Whether this voter has ever acked or fenced `target`. A node nobody here has
    /// heard from holds no lease this voter helped grant, so there is nothing to expire.
    pub fn has_seen(&self, target: NodeId) -> bool {
        self.last_ack.contains_key(&target) || self.fences.contains_key(&target)
    }

    pub fn highest_seen(&self, target: NodeId) -> u64 {
        let acked = self.last_ack.get(&target).map_or(0, |(_, i)| *i);
        acked.max(self.fences.get(&target).copied().unwrap_or(0))
    }

    /// Vote `target` expired at `incarnation`. A grant fences the target here (the
    /// caller must persist `fences` before reporting it).
    pub fn on_vote(&mut self, target: NodeId, incarnation: u64, now: Instant, cfg: &LeaseConfig) -> Vote {
        let highest_seen = self.highest_seen(target);
        let granted = highest_seen <= incarnation && self.could_vote(target, now, cfg);
        if granted {
            let f = self.fences.entry(target).or_insert(incarnation);
            *f = (*f).max(incarnation);
        }
        Vote { granted, highest_seen }
    }

    /// How this voter sees `node` right now.
    pub fn state_of(&self, node: NodeId, now: Instant, cfg: &LeaseConfig) -> LeaseState {
        let highest = self.highest_seen(node);
        if self.fences.get(&node).is_some_and(|f| *f >= highest) {
            LeaseState::Expired
        } else if self.last_ack.get(&node).is_some_and(|(t, _)| now.saturating_duration_since(*t) < cfg.silence()) {
            LeaseState::Valid
        } else {
            LeaseState::Unknown
        }
    }

    pub fn known_nodes(&self) -> Vec<NodeId> {
        let mut v: Vec<NodeId> = self.last_ack.keys().chain(self.fences.keys()).copied().collect();
        v.sort();
        v.dedup();
        v
    }
}

/// The primary for a slot with in-sync replica list `isr` (ordered, see the plan's
/// "ISR order"): its first member that isn't provably out. `me` is judged by its own
/// lease. Any other member can be skipped only once a majority has voted it expired
/// (`expired` answers that); anything less means it may still be acting as primary.
pub fn primary_of(isr: &[NodeId], me: NodeId, holds_own_lease: bool,
                  expired: impl Fn(NodeId) -> bool) -> Option<NodeId> {
    for &n in isr {
        if n == me {
            return holds_own_lease.then_some(me);
        }
        if !expired(n) {
            return Some(n);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// This node's durable lease state: its own incarnation and the fences it has voted.
/// A voter that forgot its fences across a restart could ack a node it had voted
/// out, and let that node renew after another took over its slots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableLeaseState {
    pub incarnation: u64,
    pub fences: HashMap<NodeId, u64>,
}

pub struct LeaseStateFile {
    path: PathBuf,
}

impl LeaseStateFile {
    pub fn new(dir: &Path) -> Self {
        Self { path: dir.join("lease_state.json") }
    }

    pub fn load(&self) -> DurableLeaseState {
        std::fs::read(&self.path).ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Write-then-rename with fsync, so a crash leaves either the old or the new state.
    pub fn store(&self, s: &DurableLeaseState) -> std::io::Result<()> {
        use std::io::Write;
        let tmp = self.path.with_extension("json.tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec(s).expect("lease state serializes"))?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        if let Some(dir) = self.path.parent() {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    }
}

/// The largest cluster membership this node has ever seen, persisted. Majorities are
/// counted over at least this many nodes. Counting over the membership a node happens
/// to know right now is unsound: a node that has just started knows only itself, and
/// on the first local run of the leader-lease version all five nodes each won "1 of 1"
/// at once. A permanently removed node keeps the majority at the old size until this
/// is lowered, which makes leases harder to hold, never easier.
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

// ---------------------------------------------------------------------------
// Runtime: the loop and request handlers that drive the state machines above
// ---------------------------------------------------------------------------

use crate::cluster::ClusterManager;
use crate::network::NetworkClient;
use dfs_common::{LeaseStatusReport, Message, Request, Response};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// Owns all lease state for this node. Deliberately isolated: its locks are its own
/// std Mutexes, held only for in-memory updates; it has its own network client; and
/// the renewal handler never touches the metadata DB, the healer, chunk_map or the
/// cluster membership lock. A stall in any of those must not cost anyone a lease.
pub struct LeaseRuntime {
    cfg: LeaseConfig,
    me: NodeId,
    cluster: Arc<ClusterManager>,
    client: Arc<NetworkClient>,
    state_file: LeaseStateFile,
    membership_file: MembershipFile,
    holder: Mutex<HolderState>,
    voter: Mutex<VoterState>,
    /// The size majorities are counted over: max(membership known now, persisted
    /// high-water mark, DFS_LEASE_CLUSTER_SIZE). Refreshed by the loop.
    cluster_size: AtomicUsize,
    held_last_tick: AtomicBool,
    last_until_wall_ms: Mutex<Option<u128>>,
    acks_last_round: AtomicUsize,
    /// (node, incarnation) this node has already seen voted expired by a majority.
    declared: Mutex<HashMap<NodeId, u64>>,
}

/// Wall-clock milliseconds for an `Instant`, for logs that tests compare across processes.
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
        let state_file = LeaseStateFile::new(dir);
        let durable = state_file.load();
        let membership_file = MembershipFile::new(dir);
        let floor = std::env::var("DFS_LEASE_CLUSTER_SIZE").ok().and_then(|v| v.parse().ok()).unwrap_or(0usize);
        let size = membership_file.load().max(floor).max(1);
        Self {
            cfg: LeaseConfig::from_env(),
            me,
            cluster,
            client,
            state_file,
            membership_file,
            holder: Mutex::new(HolderState::new(durable.incarnation)),
            voter: Mutex::new(VoterState::new(Instant::now(), durable.fences)),
            cluster_size: AtomicUsize::new(size),
            held_last_tick: AtomicBool::new(false),
            last_until_wall_ms: Mutex::new(None),
            acks_last_round: AtomicUsize::new(0),
            declared: Mutex::new(HashMap::new()),
        }
    }

    pub fn start(self: Arc<Self>) {
        info!("LEASE: started (lease {:?}, margin {:?}, majority of {})",
            self.cfg.lease, self.cfg.margin, self.cluster_size.load(Ordering::Relaxed));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(self.cfg.renew_every());
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let peers = self.refresh_peers().await;
                self.renew_round(&peers).await;
                self.expire_silent_peers(&peers).await;
            }
        });
    }

    fn majority(&self) -> usize {
        self.cluster_size.load(Ordering::Relaxed) / 2 + 1
    }

    /// Every other known member (their status doesn't matter: renewals to a dead node
    /// just fail), and grow the persisted membership high-water mark if needed.
    async fn refresh_peers(&self) -> Vec<(NodeId, std::net::SocketAddr)> {
        let nodes = self.cluster.get_all_nodes().await;
        let known = nodes.len();
        if known > self.cluster_size.load(Ordering::Relaxed) {
            let file = MembershipFile { path: self.membership_file.path.clone() };
            if matches!(tokio::task::spawn_blocking(move || file.store(known)).await, Ok(Ok(()))) {
                self.cluster_size.store(known, Ordering::Relaxed);
                info!("LEASE: membership high-water now {} (majority {})", known, known / 2 + 1);
            }
        }
        nodes.into_iter().filter(|n| n.id != self.me).map(|n| (n.id, n.addr)).collect()
    }

    async fn persist(&self) -> bool {
        let durable = DurableLeaseState {
            incarnation: self.holder.lock().unwrap().incarnation,
            fences: self.voter.lock().unwrap().fences.clone(),
        };
        let file = LeaseStateFile { path: self.state_file.path.clone() };
        match tokio::task::spawn_blocking(move || file.store(&durable)).await {
            Ok(Ok(())) => true,
            other => {
                warn!("LEASE: failed to persist lease state: {:?}", other);
                false
            }
        }
    }

    async fn ask_all(&self, peers: &[(NodeId, std::net::SocketAddr)], req: Request) -> Vec<(NodeId, Response)> {
        let timeout = self.cfg.renew_every();
        let calls = peers.iter().map(|(id, addr)| {
            let client = self.client.clone();
            let req = req.clone();
            let (id, addr) = (*id, *addr);
            async move {
                match client.send_message_timeout(addr, Message::Request(req), timeout).await {
                    Ok(env) => match env.message {
                        Message::Response(r) => Some((id, r)),
                        _ => None,
                    },
                    Err(e) => {
                        debug!("LEASE: {} ({}) unreachable: {}", id, addr, e);
                        None
                    }
                }
            }
        });
        futures::future::join_all(calls).await.into_iter().flatten().collect()
    }

    async fn renew_round(&self, peers: &[(NodeId, std::net::SocketAddr)]) {
        let sent = Instant::now();
        let incarnation = self.holder.lock().unwrap().incarnation;
        let replies = self.ask_all(peers, Request::RenewNodeLease { node: self.me, incarnation }).await;
        let mut acks = 1; // this node
        let mut fenced_at = None;
        for (_, r) in &replies {
            if let Response::LeaseAck { acked, fenced } = r {
                if *acked {
                    acks += 1;
                } else {
                    fenced_at = Some(fenced_at.unwrap_or(0).max(*fenced));
                }
            }
        }
        self.acks_last_round.store(acks, Ordering::Relaxed);
        if let Some(f) = fenced_at {
            let bumped = self.holder.lock().unwrap().on_fenced(f);
            if bumped && self.persist().await {
                info!("LEASE own: fenced at incarnation {}; rejoining as {}", f, f + 1);
            }
        }
        let majority = self.majority();
        self.holder.lock().unwrap().on_round(sent, acks, majority, &self.cfg);

        let now = Instant::now();
        let (has, remaining, inc) = {
            let h = self.holder.lock().unwrap();
            (h.holds_lease(now), h.remaining(now), h.incarnation)
        };
        // Compare with the previous tick, not the send time: a refused renewal fails
        // instantly, so a lease usually lapses between ticks rather than during one.
        let had = self.held_last_tick.swap(has, Ordering::Relaxed);
        if has && !had {
            info!("LEASE own: acquired (incarnation {}, {} of {} acked, until_wall_ms {})",
                inc, acks, self.cluster_size.load(Ordering::Relaxed), wall_ms(now + remaining));
        } else if !has && had {
            // The exact end, so a test can check it precedes any expiry declaration.
            info!("LEASE own: lost ({} acks, need {}); it ended at wall_ms {}", acks, majority,
                self.last_until_wall_ms.lock().unwrap().map_or("?".to_string(), |w| w.to_string()));
        }
        if has {
            *self.last_until_wall_ms.lock().unwrap() = Some(wall_ms(now + remaining));
        }
    }

    /// Ask a majority to vote expired any peer this node has heard nothing from for
    /// `lease + margin`. In Phase 1 the result is only logged; from Phase 3 it's what
    /// lets a secondary take over the silent node's slots.
    async fn expire_silent_peers(&self, peers: &[(NodeId, std::net::SocketAddr)]) {
        let now = Instant::now();
        let ids: Vec<NodeId> = peers.iter().map(|(id, _)| *id).collect();
        for (target, inc) in self.expiry_candidates(&ids, now) {
            let mine = self.voter.lock().unwrap().on_vote(target, inc, now, &self.cfg);
            if !mine.granted || !self.persist().await {
                continue;
            }
            let replies = self.ask_all(peers, Request::VoteLeaseExpired { target, incarnation: inc }).await;
            let mut votes = 1;
            let mut newer = None;
            for (_, r) in &replies {
                if let Response::LeaseExpiryVote { granted, highest_seen } = r {
                    if *highest_seen > inc {
                        newer = Some(*highest_seen);
                    } else if *granted {
                        votes += 1;
                    }
                }
            }
            if let Some(n) = newer {
                debug!("LEASE: vote on {} at incarnation {} saw newer incarnation {}; not declared", target, inc, n);
            } else if votes >= self.majority() {
                self.declared.lock().unwrap().insert(target, inc);
                info!("LEASE takeover: node {} incarnation {} expired by majority ({} of {} votes) at wall_ms {}",
                    target, inc, votes, self.cluster_size.load(Ordering::Relaxed), wall_ms(Instant::now()));
            }
        }
    }

    /// Peers this node should ask a majority to expire: heard from before, silent here
    /// for lease + margin, and not already declared at their latest incarnation.
    fn expiry_candidates(&self, peers: &[NodeId], now: Instant) -> Vec<(NodeId, u64)> {
        let v = self.voter.lock().unwrap();
        let declared = self.declared.lock().unwrap();
        peers.iter()
            .map(|id| (*id, v.highest_seen(*id)))
            .filter(|(id, inc)| v.could_vote(*id, now, &self.cfg) && declared.get(id) != Some(inc))
            // Never heard from here: nothing to expire. (Incarnations start at 0, so the
            // incarnation number can't stand in for "seen".)
            .filter(|(id, _)| v.has_seen(*id))
            .collect()
    }

    pub fn handle_renew(&self, node: NodeId, incarnation: u64) -> Response {
        let ack = self.voter.lock().unwrap().on_renew(node, incarnation, Instant::now());
        Response::LeaseAck { acked: ack.acked, fenced: ack.fenced }
    }

    pub async fn handle_vote(&self, target: NodeId, incarnation: u64) -> Response {
        let vote = self.voter.lock().unwrap().on_vote(target, incarnation, Instant::now(), &self.cfg);
        // A fence that isn't on disk doesn't count: a restart must not forget it.
        let granted = vote.granted && self.persist().await;
        Response::LeaseExpiryVote { granted, highest_seen: vote.highest_seen }
    }

    pub fn status(&self) -> LeaseStatusReport {
        let now = Instant::now();
        let (holds, remaining, incarnation) = {
            let h = self.holder.lock().unwrap();
            (h.holds_lease(now), h.remaining(now), h.incarnation)
        };
        let v = self.voter.lock().unwrap();
        let view = v.known_nodes().into_iter().map(|n| (n, v.state_of(n, now, &self.cfg))).collect();
        LeaseStatusReport {
            node: self.me,
            incarnation,
            holds_lease: holds,
            lease_remaining_ms: remaining.as_millis() as u64,
            acks_last_round: self.acks_last_round.load(Ordering::Relaxed) as u64,
            majority: self.majority() as u64,
            view,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LeaseConfig {
        LeaseConfig { lease: Duration::from_secs(3), margin: Duration::from_millis(500) }
    }
    fn ids(n: usize) -> Vec<NodeId> {
        (0..n).map(|i| NodeId::from_bytes([i as u8 + 1; 16])).collect()
    }
    fn ms(x: u64) -> Duration { Duration::from_millis(x) }

    /// Tiny deterministic PRNG so the simulation is reproducible without a dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0
        }
        fn below(&mut self, n: u64) -> u64 { self.next() % n }
    }

    /// The core safety property, simulated: five nodes run renewal rounds and expiry
    /// votes over a network with random delays and random partitions. At every step,
    /// no node holds a lease at an incarnation a majority has already voted expired.
    #[test]
    fn simulated_cluster_never_overlaps_a_lease_with_its_expiry() {
        let c = cfg();
        let n = ids(5);
        let maj = 3;
        let (mut expirations, mut held) = (0u64, 0u64);
        for seed in 1..=40u64 {
            let mut rng = Rng(seed * 0x9E37_79B9_7F4A_7C15);
            let t0 = Instant::now();
            let mut holders: Vec<HolderState> = (0..5).map(|_| HolderState::new(0)).collect();
            let mut voters: Vec<VoterState> = (0..5).map(|_| VoterState::new(t0, HashMap::new())).collect();
            // declared[target] = highest incarnation a majority voted expired.
            let mut declared: HashMap<usize, u64> = HashMap::new();
            // cut[a][b]: a can't reach b.
            let mut cut = [[false; 5]; 5];
            let mut now = t0;
            for step in 0..600 {
                now += ms(100 + rng.below(250));
                if step % 40 == 0 {
                    for a in 0..5 { for b in 0..5 { cut[a][b] = a != b && rng.below(5) == 0; } }
                }
                let a = rng.below(5) as usize;
                if rng.below(2) == 0 {
                    // Renewal round by a, sent now, each ack arriving after a random delay.
                    let inc = holders[a].incarnation;
                    let mut acks = 1;
                    let mut fenced = None;
                    for b in 0..5 {
                        if b == a || cut[a][b] || cut[b][a] { continue; }
                        let ack = voters[b].on_renew(n[a], inc, now + ms(rng.below(400)));
                        if ack.acked { acks += 1 } else { fenced = Some(ack.fenced) }
                    }
                    if let Some(f) = fenced { holders[a].on_fenced(f); }
                    holders[a].on_round(now, acks, maj, &c);
                } else {
                    // a asks everyone to vote some target expired.
                    let t = rng.below(5) as usize;
                    if t == a { continue; }
                    let inc = voters[a].highest_seen(n[t]);
                    let mut votes = 0;
                    let mut newer = false;
                    for b in 0..5 {
                        if b == t || (b != a && (cut[a][b] || cut[b][a])) { continue; }
                        let v = voters[b].on_vote(n[t], inc, now + ms(rng.below(400)), &c);
                        if v.highest_seen > inc { newer = true; } else if v.granted { votes += 1; }
                    }
                    if !newer && votes >= maj {
                        expirations += 1;
                        let d = declared.entry(t).or_insert(inc);
                        *d = (*d).max(inc);
                    }
                }
                for t in 0..5 {
                    held += holders[t].holds_lease(now) as u64;
                    if let Some(d) = declared.get(&t) {
                        assert!(!(holders[t].holds_lease(now) && holders[t].incarnation <= *d),
                            "seed {} step {}: node {} holds a lease at incarnation {} after a majority expired incarnation {}",
                            seed, step, t, holders[t].incarnation, d);
                    }
                }
            }
        }
        // Not vacuous: leases were held and majorities did expire nodes.
        assert!(held > 10_000 && expirations > 100, "simulation too quiet: held={} expirations={}", held, expirations);
    }

    #[test]
    fn minority_acks_give_no_lease() {
        let c = cfg();
        let mut h = HolderState::new(0);
        let t0 = Instant::now();
        h.on_round(t0, 2, 3, &c);
        assert!(!h.holds_lease(t0));
        h.on_round(t0, 3, 3, &c);
        assert!(h.holds_lease(t0));
        assert!(!h.holds_lease(t0 + c.lease - c.margin), "the lease ends a margin early");
    }

    /// A voter votes only after lease + margin of silence, and never while a newer
    /// incarnation is known; once it votes, it refuses the fenced incarnation.
    #[test]
    fn voter_rules() {
        let c = cfg();
        let n = ids(2);
        let t0 = Instant::now();
        let mut v = VoterState::new(t0, HashMap::new());
        let later = t0 + c.silence(); // past the post-start abstention
        assert!(v.on_renew(n[0], 1, later).acked);
        assert!(!v.on_vote(n[0], 1, later + c.silence() - ms(1), &c).granted, "too soon after the last ack");
        assert!(!v.on_vote(n[0], 0, later + c.silence(), &c).granted, "a newer incarnation (1) is known");
        let vote = v.on_vote(n[0], 1, later + c.silence(), &c);
        assert!(vote.granted);
        let ack = v.on_renew(n[0], 1, later + c.silence() + ms(1));
        assert_eq!(ack, Ack { acked: false, fenced: 1 }, "a fenced incarnation is refused");
        assert!(v.on_renew(n[0], 2, later + c.silence() + ms(2)).acked, "a rejoin above the fence is accepted");
    }

    /// A restarted voter forgot whom it acked, so it abstains for lease + margin; and it
    /// must not forget whom it fenced, so fences come back from disk.
    #[test]
    fn restarted_voter_abstains_and_keeps_its_fences() {
        let c = cfg();
        let n = ids(1);
        let dir = tempfile::TempDir::new().unwrap();
        let file = LeaseStateFile::new(dir.path());
        let mut fences = HashMap::new();
        fences.insert(n[0], 4);
        file.store(&DurableLeaseState { incarnation: 2, fences }).unwrap();

        let restarted = Instant::now();
        let loaded = LeaseStateFile::new(dir.path()).load();
        assert_eq!(loaded.incarnation, 2);
        let mut v = VoterState::new(restarted, loaded.fences);
        assert!(!v.on_vote(n[0], 4, restarted + c.silence() - ms(1), &c).granted, "abstains right after a restart");
        assert!(!v.on_renew(n[0], 4, restarted).acked, "fence survived the restart");
        assert!(v.on_renew(n[0], 5, restarted).acked);
    }

    #[test]
    fn fenced_holder_drops_its_lease_and_rejoins_higher() {
        let c = cfg();
        let t0 = Instant::now();
        let mut h = HolderState::new(3);
        h.on_round(t0, 3, 3, &c);
        assert!(h.on_fenced(3));
        assert_eq!(h.incarnation, 4);
        assert!(!h.holds_lease(t0), "a fenced node stops immediately, whatever its lease said");
        assert!(!h.on_fenced(2), "an older fence changes nothing");
    }

    /// 2026-09-28 local T58 (leader-lease version): every node, just started and
    /// knowing only itself, counted 1 of 1 as a majority. A node that has ever seen a
    /// 5-node cluster needs 3, whatever it knows now.
    #[tokio::test]
    async fn restarted_node_alone_needs_the_remembered_majority() {
        let dir = tempfile::TempDir::new().unwrap();
        MembershipFile::new(dir.path()).store(5).unwrap();
        let me = ids(1)[0];
        let cluster = Arc::new(ClusterManager::new(me, "127.0.0.1:19350".parse().unwrap(), 10, 30));
        let rt = LeaseRuntime::new(me, cluster, Arc::new(NetworkClient::new()), dir.path());
        let peers = rt.refresh_peers().await;
        assert!(peers.is_empty(), "precondition: it knows no peers");
        rt.renew_round(&peers).await;
        assert!(!rt.holder.lock().unwrap().holds_lease(Instant::now()),
            "a node that has seen 5 members granted itself a lease on its own ack");
        assert_eq!(rt.majority(), 3);
    }

    /// First local T58 run: no expiry vote was ever started, because "never heard from"
    /// was tested as incarnation == 0, and every node starts at incarnation 0.
    #[tokio::test]
    async fn silent_peer_at_incarnation_zero_is_an_expiry_candidate() {
        let dir = tempfile::TempDir::new().unwrap();
        let n = ids(3);
        let cluster = Arc::new(ClusterManager::new(n[0], "127.0.0.1:19351".parse().unwrap(), 10, 30));
        let rt = LeaseRuntime::new(n[0], cluster, Arc::new(NetworkClient::new()), dir.path());
        let c = rt.cfg;
        let t0 = Instant::now();
        *rt.voter.lock().unwrap() = VoterState::new(t0, HashMap::new());
        rt.voter.lock().unwrap().on_renew(n[1], 0, t0);
        let later = t0 + c.silence() + ms(1);
        assert_eq!(rt.expiry_candidates(&[n[1], n[2]], later), vec![(n[1], 0)],
            "a peer acked at incarnation 0 then silent must be a candidate; one never heard from must not");
        assert!(rt.expiry_candidates(&[n[1]], t0 + ms(100)).is_empty(), "not while it's still renewing");
    }

    /// Takeover goes only past a member a majority has voted expired, never past one
    /// merely silent, and a node judges itself by its own lease.
    #[test]
    fn primary_needs_a_majority_expiry_to_skip() {
        let n = ids(2);
        let isr = [n[0], n[1]];
        assert_eq!(primary_of(&isr, n[1], true, |_| false), Some(n[0]));
        assert_eq!(primary_of(&isr, n[1], true, |x| x == n[0]), Some(n[1]));
        assert_eq!(primary_of(&isr, n[1], false, |x| x == n[0]), None, "S without its own lease");
        assert_eq!(primary_of(&isr, n[0], true, |_| false), Some(n[0]));
        assert_eq!(primary_of(&isr, n[0], false, |_| false), None, "P without its own lease");
    }
}
