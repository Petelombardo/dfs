//! Per-chunk in-sync replica list, agreed by a majority (SLOT-OWNERSHIP-PLAN.md Phase 3a).
//!
//! Each chunk slot has an ISR record, `SlotIsr { epoch, members }`: its replicas in order,
//! primary first. Every change moves it to the next epoch, and each epoch's value is chosen
//! by single-decree Paxos among all nodes, so exactly one value can ever be chosen for a
//! given (slot, epoch), whoever proposes and whatever the network does. No leader is
//! involved: a leader's death or stall doesn't affect ISR changes.
//!
//! Plain compare-and-set on each node is not enough. Two proposers racing for epoch 1 can
//! each win a different minority, leaving conflicting epoch-1 records behind; Paxos's
//! prepare/promise round is what rules that out.
//!
//! - An acceptor's state per slot: the highest committed record it knows, and for the next
//!   epoch the highest ballot it has promised and the value it accepted (if any). All of it
//!   must be durable before the acceptor answers.
//! - A proposer picks a ballot higher than any it has seen, gathers promises from a
//!   majority, adopts the highest-ballot accepted value among them if there is one (it may
//!   already be chosen), otherwise proposes its own, and needs a majority of accepts.
//! - Once chosen, the value is broadcast as committed. An acceptor that already has a
//!   committed record at or past an epoch answers with that record, so stale proposers catch
//!   up instead of re-deciding.
//!
//! Everything below is a pure state machine; the runtime moves messages. The safety
//! property (at most one value chosen per slot and epoch) is tested by a seeded randomized
//! simulation with concurrent proposers, loss, duplication, reordering and restarts.

use dfs_common::NodeId;
use serde::{Deserialize, Serialize};

pub use dfs_common::{AcceptReply, Ballot, PrepareReply, SlotIsr};

/// One acceptor's durable state for one slot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptorState {
    /// The highest-epoch record this acceptor knows was chosen.
    pub committed: Option<SlotIsr>,
    /// The epoch the promise/accept below belong to (always committed epoch + 1 once
    /// anything is in flight; stale in-flight state is discarded when it's surpassed).
    pub instance_epoch: u64,
    pub promised: Option<Ballot>,
    pub accepted: Option<(Ballot, SlotIsr)>,
}

impl AcceptorState {
    fn committed_epoch(&self) -> u64 {
        self.committed.as_ref().map_or(0, |c| c.epoch)
    }

    /// Discard in-flight state that belongs to an epoch this acceptor has moved past.
    fn align(&mut self, epoch: u64) {
        if self.instance_epoch != epoch {
            self.instance_epoch = epoch;
            self.promised = None;
            self.accepted = None;
        }
    }

    /// Phase 1. The caller must persist `self` before sending the reply.
    pub fn on_prepare(&mut self, epoch: u64, ballot: Ballot) -> PrepareReply {
        if let Some(c) = &self.committed {
            if c.epoch >= epoch {
                return PrepareReply::Committed(c.clone());
            }
        }
        if epoch > self.committed_epoch() + 1 {
            // Can't take part in an epoch whose predecessor it hasn't learned: a proposer
            // must build on the latest committed record.
            return PrepareReply::Reject { promised: None };
        }
        if self.instance_epoch == epoch && self.promised.is_some_and(|p| p >= ballot) {
            return PrepareReply::Reject { promised: self.promised };
        }
        self.align(epoch);
        self.promised = Some(ballot);
        PrepareReply::Promise { accepted: self.accepted.clone() }
    }

    /// Phase 2. The caller must persist `self` before sending the reply.
    pub fn on_accept(&mut self, ballot: Ballot, value: SlotIsr) -> AcceptReply {
        let epoch = value.epoch;
        if let Some(c) = &self.committed {
            if c.epoch >= epoch {
                return AcceptReply::Committed(c.clone());
            }
        }
        if epoch > self.committed_epoch() + 1 {
            return AcceptReply::Reject { promised: None };
        }
        if self.instance_epoch == epoch && self.promised.is_some_and(|p| p > ballot) {
            return AcceptReply::Reject { promised: self.promised };
        }
        self.align(epoch);
        self.promised = Some(ballot);
        self.accepted = Some((ballot, value));
        AcceptReply::Accepted
    }

    /// Learn a chosen value. Monotonic: an older epoch never replaces a newer one.
    pub fn on_commit(&mut self, value: SlotIsr) {
        if value.epoch > self.committed_epoch() {
            let epoch = value.epoch;
            self.committed = Some(value);
            if self.instance_epoch <= epoch {
                self.instance_epoch = epoch + 1;
                self.promised = None;
                self.accepted = None;
            }
        }
    }
}

/// Proposer bookkeeping for one attempt at choosing `epoch`'s value.
#[derive(Debug, Clone)]
pub struct Proposal {
    pub epoch: u64,
    pub ballot: Ballot,
    /// The value this proposer wants, used only if no promise reports an accepted value.
    pub own: SlotIsr,
}

/// What a proposer should do after phase 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterPrepare {
    /// Send Accept(ballot, value) to everyone.
    Accept(SlotIsr),
    /// Someone already committed this epoch (or later): learn it and stop.
    Learn(SlotIsr),
    /// No majority of promises: retry with a higher ballot than `seen`.
    Retry { seen: Option<Ballot> },
}

impl Proposal {
    /// Decide phase 2 from phase 1's replies (this node's own reply included), keyed by
    /// acceptor. Each acceptor counts once: counting a duplicated reply twice can fake a
    /// majority (the simulation's first seed did exactly that).
    pub fn after_prepare(&self, replies: &[(NodeId, PrepareReply)], majority: usize) -> AfterPrepare {
        let mut promisers = std::collections::HashSet::new();
        let mut best: Option<(Ballot, SlotIsr)> = None;
        let mut seen: Option<Ballot> = None;
        for (from, r) in replies {
            match r {
                PrepareReply::Committed(c) => return AfterPrepare::Learn(c.clone()),
                PrepareReply::Promise { accepted } => {
                    promisers.insert(*from);
                    if let Some((b, v)) = accepted {
                        if best.as_ref().is_none_or(|(bb, _)| b > bb) {
                            best = Some((*b, v.clone()));
                        }
                    }
                }
                PrepareReply::Reject { promised } => {
                    if promised.is_some() && *promised > seen {
                        seen = *promised;
                    }
                }
            }
        }
        if promisers.len() < majority {
            return AfterPrepare::Retry { seen };
        }
        // Paxos: if any promiser accepted a value, it may already be chosen, so propose
        // the highest-ballot one; only otherwise is this proposer free to propose its own.
        AfterPrepare::Accept(best.map(|(_, v)| v).unwrap_or_else(|| self.own.clone()))
    }

    /// Decide from phase 2's replies, keyed by acceptor (each counted once), whether the
    /// value is chosen.
    pub fn chosen(replies: &[(NodeId, AcceptReply)], majority: usize) -> Result<(), Option<SlotIsr>> {
        let mut acceptors = std::collections::HashSet::new();
        for (from, r) in replies {
            match r {
                AcceptReply::Accepted => { acceptors.insert(*from); }
                AcceptReply::Committed(c) => return Err(Some(c.clone())),
                AcceptReply::Reject { .. } => {}
            }
        }
        if acceptors.len() >= majority { Ok(()) } else { Err(None) }
    }
}

/// The ISR a chunk starts with: its holders in rendezvous order (balanced and the same on
/// every node for the same holder set), keeping at most `rf` members.
pub fn initial_members(file_id: dfs_common::FileId, chunk_idx: u64, holders: &[NodeId], rf: usize) -> Vec<NodeId> {
    let score = |node: &NodeId| {
        let mut h = blake3::Hasher::new();
        h.update(b"slot-isr");
        h.update(file_id.0.as_bytes());
        h.update(&chunk_idx.to_le_bytes());
        h.update(node.as_bytes());
        *h.finalize().as_bytes()
    };
    let mut order: Vec<NodeId> = holders.to_vec();
    order.sort();
    order.dedup();
    order.sort_by_cached_key(|n| std::cmp::Reverse(score(n)));
    order.truncate(rf.max(1));
    order
}

// ---------------------------------------------------------------------------
// Runtime: acceptor handlers, the proposer, and the committed-record cache
// ---------------------------------------------------------------------------

use crate::lease::LeaseRuntime;
use crate::metadata::MetadataStore;
use crate::network::NetworkClient;
use dashmap::DashMap;
use dfs_common::{FileId, Message, Request, Response};
use std::sync::Arc;
use tracing::{debug, info, warn};

type Slot = (FileId, u64);

/// Slot ISR agreement for this node: answers other proposers as an acceptor, proposes
/// values itself, and caches every committed record it knows so hot paths (the fold owner)
/// never touch the disk.
pub struct SlotIsrService {
    me: NodeId,
    client: Arc<NetworkClient>,
    metadata: Arc<MetadataStore>,
    lease: Arc<LeaseRuntime>,
    committed: DashMap<Slot, SlotIsr>,
    /// Highest ballot round seen per slot, so a retry always outbids.
    max_round: DashMap<Slot, u64>,
    /// Advances every catch-up pass so each pass asks different peers.
    catch_up_turn: std::sync::atomic::AtomicUsize,
}

const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const MAX_ATTEMPTS: usize = 4;

impl SlotIsrService {
    pub fn new(me: NodeId, client: Arc<NetworkClient>, metadata: Arc<MetadataStore>, lease: Arc<LeaseRuntime>) -> Self {
        let committed = DashMap::new();
        match metadata.slot_isr_all_committed() {
            Ok(rows) => {
                for (slot, isr) in rows {
                    committed.insert(slot, isr);
                }
            }
            Err(e) => warn!("SLOT ISR: could not load committed records: {}", e),
        }
        Self { me, client, metadata, lease, committed, max_round: DashMap::new(), catch_up_turn: Default::default() }
    }

    /// The committed ISR this node knows for a slot (possibly behind; every later use
    /// carries the epoch, so staleness is detected, never trusted).
    pub fn get(&self, file_id: FileId, chunk_idx: u64) -> Option<SlotIsr> {
        self.committed.get(&(file_id, chunk_idx)).map(|e| e.value().clone())
    }

    /// Test-only: act as if `isr` had been committed for the slot.
    #[cfg(test)]
    pub fn set_committed_for_test(&self, file_id: FileId, chunk_idx: u64, isr: SlotIsr) {
        self.learn((file_id, chunk_idx), &isr);
    }

    pub fn committed_count(&self) -> usize {
        self.committed.len()
    }

    fn learn(&self, slot: Slot, isr: &SlotIsr) {
        let mut e = self.committed.entry(slot).or_insert_with(|| isr.clone());
        if isr.epoch > e.epoch {
            *e = isr.clone();
        }
    }

    async fn apply<R: Send + 'static>(
        &self,
        slots: Vec<Slot>,
        f: impl FnMut(usize, &mut AcceptorState) -> R + Send + 'static,
    ) -> anyhow::Result<Vec<R>> {
        let metadata = self.metadata.clone();
        tokio::task::spawn_blocking(move || metadata.slot_isr_apply(&slots, f)).await?
    }

    pub async fn handle_prepare(&self, items: Vec<(FileId, u64, u64, Ballot)>) -> Response {
        let slots: Vec<Slot> = items.iter().map(|(f, i, _, _)| (*f, *i)).collect();
        let args: Vec<(u64, Ballot)> = items.iter().map(|(_, _, e, b)| (*e, *b)).collect();
        match self.apply(slots, move |i, st| st.on_prepare(args[i].0, args[i].1)).await {
            Ok(replies) => Response::SlotIsrPrepareReplies { replies },
            Err(e) => Response::Error { message: format!("slot isr prepare: {}", e), code: dfs_common::ErrorCode::InternalError },
        }
    }

    pub async fn handle_accept(&self, items: Vec<(FileId, u64, Ballot, SlotIsr)>) -> Response {
        let slots: Vec<Slot> = items.iter().map(|(f, i, _, _)| (*f, *i)).collect();
        let args: Vec<(Ballot, SlotIsr)> = items.into_iter().map(|(_, _, b, v)| (b, v)).collect();
        match self.apply(slots, move |i, st| st.on_accept(args[i].0, args[i].1.clone())).await {
            Ok(replies) => Response::SlotIsrAcceptReplies { replies },
            Err(e) => Response::Error { message: format!("slot isr accept: {}", e), code: dfs_common::ErrorCode::InternalError },
        }
    }

    pub async fn handle_commit(&self, items: Vec<(FileId, u64, SlotIsr)>) -> Response {
        let slots: Vec<Slot> = items.iter().map(|(f, i, _)| (*f, *i)).collect();
        let values: Vec<SlotIsr> = items.iter().map(|(_, _, v)| v.clone()).collect();
        let vals = values.clone();
        if let Err(e) = self.apply(slots.clone(), move |i, st| st.on_commit(vals[i].clone())).await {
            return Response::Error { message: format!("slot isr commit: {}", e), code: dfs_common::ErrorCode::InternalError };
        }
        for (slot, v) in slots.into_iter().zip(values.iter()) {
            self.learn(slot, v);
        }
        Response::Ok { data: None }
    }

    pub fn handle_get(&self, slots: Vec<(FileId, u64)>) -> Response {
        Response::SlotIsrRecords { records: slots.into_iter().map(|(f, i)| self.get(f, i)).collect() }
    }

    /// Ask two peers (rotating) for their committed records on `slots` and learn any epoch
    /// newer than this node's. Returns how many records were learned.
    pub async fn catch_up(&self, slots: Vec<Slot>) -> usize {
        if slots.is_empty() {
            return 0;
        }
        let mut peers = self.lease.peers();
        if peers.is_empty() {
            return 0;
        }
        // Different peers every pass: asking the same two forever would never learn a commit
        // both of them happen to have missed.
        let offset = self.catch_up_turn.fetch_add(2, std::sync::atomic::Ordering::Relaxed) % peers.len();
        peers.rotate_left(offset);
        peers.truncate(2);
        let mut newer: std::collections::HashMap<Slot, SlotIsr> = std::collections::HashMap::new();
        for (_, addr) in peers {
            let req = Message::Request(Request::GetSlotIsr { slots: slots.clone() });
            let Ok(env) = self.client.send_message_timeout(addr, req, RPC_TIMEOUT).await else { continue };
            let Message::Response(Response::SlotIsrRecords { records }) = env.message else { continue };
            if records.len() != slots.len() {
                continue;
            }
            for (slot, rec) in slots.iter().zip(records) {
                let Some(rec) = rec else { continue };
                let mine = self.get(slot.0, slot.1).map_or(0, |c| c.epoch);
                if rec.epoch > mine && newer.get(slot).is_none_or(|n| rec.epoch > n.epoch) {
                    newer.insert(*slot, rec);
                }
            }
        }
        if newer.is_empty() {
            return 0;
        }
        let items: Vec<(FileId, u64, SlotIsr)> = newer.into_iter().map(|((f, i), v)| (f, i, v)).collect();
        let n = items.len();
        match self.handle_commit(items).await {
            Response::Ok { .. } => n,
            _ => 0,
        }
    }

    /// Send `req` to every peer; collect (peer, response) for those that answered.
    async fn ask_peers(&self, req: Request) -> Vec<(NodeId, Response)> {
        let calls = self.lease.peers().into_iter().map(|(id, addr)| {
            let client = self.client.clone();
            let req = req.clone();
            async move {
                match client.send_message_timeout(addr, Message::Request(req), RPC_TIMEOUT).await {
                    Ok(env) => match env.message {
                        Message::Response(r) => Some((id, r)),
                        _ => None,
                    },
                    Err(e) => {
                        debug!("SLOT ISR: {} ({}) unreachable: {}", id, addr, e);
                        None
                    }
                }
            }
        });
        futures::future::join_all(calls).await.into_iter().flatten().collect()
    }

    /// Choose each slot's next ISR by Paxos. `wanted` holds each slot's proposed value
    /// (its epoch is the one being decided). Returns what was committed for each slot:
    /// this node's value, another proposer's that won, or None if no majority could be
    /// reached in MAX_ATTEMPTS. Batched: one prepare and one accept RPC per peer per attempt.
    pub async fn propose(&self, wanted: Vec<(Slot, SlotIsr)>) -> Vec<Option<SlotIsr>> {
        let n = wanted.len();
        let mut result: Vec<Option<SlotIsr>> = vec![None; n];
        let mut pending: Vec<usize> = (0..n).collect();
        for _attempt in 0..MAX_ATTEMPTS {
            if pending.is_empty() {
                break;
            }
            let majority = self.lease.majority();
            let props: Vec<Proposal> = pending.iter().map(|&i| {
                let (slot, own) = &wanted[i];
                let mut r = self.max_round.entry(*slot).or_insert(0);
                *r += 1;
                Proposal { epoch: own.epoch, ballot: Ballot { round: *r, proposer: self.me }, own: own.clone() }
            }).collect();

            // Phase 1.
            let items: Vec<(FileId, u64, u64, Ballot)> = pending.iter().zip(&props)
                .map(|(&i, p)| (wanted[i].0 .0, wanted[i].0 .1, p.epoch, p.ballot)).collect();
            let mut promises: Vec<Vec<(NodeId, PrepareReply)>> = vec![Vec::new(); pending.len()];
            match self.handle_prepare(items.clone()).await {
                Response::SlotIsrPrepareReplies { replies } => {
                    for (k, r) in replies.into_iter().enumerate() { promises[k].push((self.me, r)); }
                }
                other => { warn!("SLOT ISR: local prepare failed: {:?}", other); return result; }
            }
            for (peer, resp) in self.ask_peers(Request::SlotIsrPrepare { items }).await {
                if let Response::SlotIsrPrepareReplies { replies } = resp {
                    if replies.len() == pending.len() {
                        for (k, r) in replies.into_iter().enumerate() { promises[k].push((peer, r)); }
                    }
                }
            }

            let mut to_accept: Vec<(usize, Proposal, SlotIsr)> = Vec::new();
            let mut learned: Vec<(FileId, u64, SlotIsr)> = Vec::new();
            let mut retry: Vec<usize> = Vec::new();
            for (k, &i) in pending.iter().enumerate() {
                match props[k].after_prepare(&promises[k], majority) {
                    AfterPrepare::Accept(v) => to_accept.push((i, props[k].clone(), v)),
                    AfterPrepare::Learn(c) => { learned.push((wanted[i].0 .0, wanted[i].0 .1, c.clone())); result[i] = Some(c); }
                    AfterPrepare::Retry { seen } => {
                        if let Some(b) = seen {
                            let mut r = self.max_round.entry(wanted[i].0).or_insert(0);
                            *r = (*r).max(b.round);
                        }
                        retry.push(i);
                    }
                }
            }

            // Phase 2.
            let mut commits: Vec<(FileId, u64, SlotIsr)> = Vec::new();
            if !to_accept.is_empty() {
                let items: Vec<(FileId, u64, Ballot, SlotIsr)> = to_accept.iter()
                    .map(|(i, p, v)| (wanted[*i].0 .0, wanted[*i].0 .1, p.ballot, v.clone())).collect();
                let mut accepts: Vec<Vec<(NodeId, AcceptReply)>> = vec![Vec::new(); to_accept.len()];
                if let Response::SlotIsrAcceptReplies { replies } = self.handle_accept(items.clone()).await {
                    for (k, r) in replies.into_iter().enumerate() { accepts[k].push((self.me, r)); }
                }
                for (peer, resp) in self.ask_peers(Request::SlotIsrAccept { items }).await {
                    if let Response::SlotIsrAcceptReplies { replies } = resp {
                        if replies.len() == to_accept.len() {
                            for (k, r) in replies.into_iter().enumerate() { accepts[k].push((peer, r)); }
                        }
                    }
                }
                for (k, (i, _, v)) in to_accept.iter().enumerate() {
                    match Proposal::chosen(&accepts[k], majority) {
                        Ok(()) => { commits.push((wanted[*i].0 .0, wanted[*i].0 .1, v.clone())); result[*i] = Some(v.clone()); }
                        Err(Some(c)) => { learned.push((wanted[*i].0 .0, wanted[*i].0 .1, c.clone())); result[*i] = Some(c); }
                        Err(None) => retry.push(*i),
                    }
                }
            }

            // Everything decided this attempt: learn it here, and tell every node.
            let mut all = commits;
            all.extend(learned);
            if !all.is_empty() {
                let _ = self.handle_commit(all.clone()).await;
                let _ = self.ask_peers(Request::SlotIsrCommit { items: all.clone() }).await;
                for (f, i, v) in &all {
                    info!("SLOT ISR: file {} chunk {} epoch {} = {:?}", f, i, v.epoch, v.members);
                }
            }
            pending = retry;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};

    fn ids(n: usize) -> Vec<NodeId> {
        (0..n).map(|i| NodeId::from_bytes([i as u8 + 1; 16])).collect()
    }
    fn isr(epoch: u64, tag: u8) -> SlotIsr {
        SlotIsr { epoch, members: vec![NodeId::from_bytes([tag; 16]), NodeId::from_bytes([tag.wrapping_add(1); 16])] }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
        fn below(&mut self, n: u64) -> u64 { self.next() % n }
    }

    #[derive(Clone, Debug)]
    enum Msg {
        Prepare { from: usize, epoch: u64, ballot: Ballot },
        Promise { to: usize, from: usize, epoch: u64, ballot: Ballot, reply: PrepareReply },
        Accept { from: usize, ballot: Ballot, value: SlotIsr },
        Accepted { to: usize, from: usize, ballot: Ballot, epoch: u64, reply: AcceptReply },
        Commit { value: SlotIsr },
    }

    /// The core safety property, simulated: 5 acceptors, 3 of which also propose competing
    /// values for successive epochs, over a network that drops, duplicates and reorders
    /// messages, with acceptor restarts (durable state survives, in-flight messages don't).
    /// At most one value may ever be chosen (accepted by a majority) per epoch.
    #[test]
    fn simulated_paxos_never_chooses_two_values_for_one_epoch() {
        let n = 5;
        let maj = 3;
        let node = ids(n);
        let mut total_chosen = 0u64;
        for seed in 1..=60u64 {
            let mut rng = Rng(seed.wrapping_mul(0xA24B_AED4_963E_E407) | 1);
            let mut acc: Vec<AcceptorState> = vec![AcceptorState::default(); n];
            let mut net: VecDeque<(usize, Msg)> = VecDeque::new(); // (dest, msg)
            // proposer state: (proposal, promises, accepts)
            let mut props: HashMap<usize, (Proposal, Vec<(NodeId, PrepareReply)>, Vec<(NodeId, AcceptReply)>, Option<SlotIsr>)> = HashMap::new();
            let mut round = vec![0u64; n];
            // chosen[(epoch)] = value, from counting accepts per (epoch, ballot) across acceptors
            let mut accepted_by: HashMap<(u64, Ballot), (SlotIsr, std::collections::HashSet<usize>)> = HashMap::new();
            let mut chosen: HashMap<u64, SlotIsr> = HashMap::new();
            for _step in 0..3000 {
                // Occasionally start (or restart) a proposal from one of the first 3 nodes.
                if rng.below(10) == 0 {
                    let p = rng.below(3) as usize;
                    let epoch = acc[p].committed.as_ref().map_or(0, |c| c.epoch) + 1;
                    round[p] += 1 + rng.below(3);
                    let ballot = Ballot { round: round[p], proposer: node[p] };
                    let own = isr(epoch, (p as u8) * 16 + rng.below(8) as u8 + 1);
                    props.insert(p, (Proposal { epoch, ballot, own }, vec![], vec![], None));
                    for d in 0..n { net.push_back((d, Msg::Prepare { from: p, epoch, ballot })); }
                }
                // Occasionally restart an acceptor: durable state stays, its inbound queue is lost.
                if rng.below(40) == 0 {
                    let r = rng.below(n as u64) as usize;
                    net.retain(|(d, _)| *d != r);
                }
                let Some(idx) = (!net.is_empty()).then(|| rng.below(net.len() as u64) as usize) else { continue };
                let (dest, msg) = net.remove(idx).unwrap();
                if rng.below(8) == 0 { continue; }                       // drop
                if rng.below(10) == 0 { net.push_back((dest, msg.clone())); } // duplicate
                match msg {
                    Msg::Prepare { from, epoch, ballot } => {
                        let reply = acc[dest].on_prepare(epoch, ballot);
                        net.push_back((from, Msg::Promise { to: from, from: dest, epoch, ballot, reply }));
                    }
                    Msg::Promise { to, from, epoch, ballot, reply } => {
                        let Some((prop, promises, _, sent)) = props.get_mut(&to) else { continue };
                        if prop.ballot != ballot || prop.epoch != epoch || sent.is_some() { continue; }
                        promises.push((node[from], reply));
                        match prop.after_prepare(promises, maj) {
                            AfterPrepare::Accept(value) => {
                                *sent = Some(value.clone());
                                for d in 0..n { net.push_back((d, Msg::Accept { from: to, ballot, value: value.clone() })); }
                            }
                            AfterPrepare::Learn(c) => {
                                acc[to].on_commit(c);
                                props.remove(&to);
                            }
                            AfterPrepare::Retry { .. } => {}
                        }
                    }
                    Msg::Accept { from, ballot, value } => {
                        let reply = acc[dest].on_accept(ballot, value.clone());
                        if reply == AcceptReply::Accepted {
                            let e = accepted_by.entry((value.epoch, ballot)).or_insert((value.clone(), Default::default()));
                            assert_eq!(e.0, value, "one ballot proposed two values");
                            e.1.insert(dest);
                            if e.1.len() >= maj {
                                if let Some(prev) = chosen.get(&value.epoch) {
                                    assert_eq!(prev, &value,
                                        "seed {}: two different values chosen for epoch {}: {:?} and {:?}",
                                        seed, value.epoch, prev, value);
                                } else {
                                    chosen.insert(value.epoch, value.clone());
                                    total_chosen += 1;
                                }
                            }
                        }
                        net.push_back((from, Msg::Accepted { to: from, from: dest, ballot, epoch: value.epoch, reply }));
                    }
                    Msg::Accepted { to, from, ballot, epoch, reply } => {
                        let Some((prop, _, accepts, sent)) = props.get_mut(&to) else { continue };
                        if prop.ballot != ballot || prop.epoch != epoch { continue; }
                        accepts.push((node[from], reply));
                        match Proposal::chosen(accepts, maj) {
                            Ok(()) => {
                                let v = sent.clone().expect("accepts only after sending a value");
                                for d in 0..n { net.push_back((d, Msg::Commit { value: v.clone() })); }
                                props.remove(&to);
                            }
                            Err(Some(c)) => { acc[to].on_commit(c); props.remove(&to); }
                            Err(None) => {}
                        }
                    }
                    Msg::Commit { value } => {
                        if let Some(prev) = chosen.get(&value.epoch) {
                            assert_eq!(prev, &value, "a committed value differs from the chosen one");
                        }
                        acc[dest].on_commit(value);
                    }
                }
                // No acceptor ever holds a committed record that wasn't chosen.
                for a in &acc {
                    if let Some(c) = &a.committed {
                        assert_eq!(chosen.get(&c.epoch), Some(c), "seed {}: committed an unchosen value", seed);
                    }
                }
            }
        }
        assert!(total_chosen > 300, "simulation too quiet: {} values chosen", total_chosen);
    }

    #[test]
    fn acceptor_rules() {
        let [p1, p2] = [ids(2)[0], ids(2)[1]];
        let b = |r, p| Ballot { round: r, proposer: p };
        let mut a = AcceptorState::default();
        assert_eq!(a.on_prepare(1, b(1, p1)), PrepareReply::Promise { accepted: None });
        assert!(matches!(a.on_prepare(1, b(1, p1)), PrepareReply::Reject { .. }), "same ballot twice");
        assert_eq!(a.on_prepare(1, b(1, p2)), PrepareReply::Promise { accepted: None }, "ties break by proposer id");
        assert!(matches!(a.on_accept(b(1, p1), isr(1, 1)), AcceptReply::Reject { .. }), "accept below the promise");
        assert_eq!(a.on_accept(b(1, p2), isr(1, 2)), AcceptReply::Accepted);
        assert_eq!(a.on_prepare(1, b(2, p1)), PrepareReply::Promise { accepted: Some((b(1, p2), isr(1, 2))) },
            "a later prepare must be told what was accepted");
        assert!(matches!(a.on_prepare(3, b(9, p1)), PrepareReply::Reject { .. }), "can't skip an epoch");
        a.on_commit(isr(1, 2));
        assert_eq!(a.on_prepare(1, b(9, p1)), PrepareReply::Committed(isr(1, 2)), "decided epochs are answered, not re-run");
        assert_eq!(a.on_prepare(2, b(1, p1)), PrepareReply::Promise { accepted: None }, "the next epoch starts fresh");
        a.on_commit(isr(1, 7));
        assert_eq!(a.committed, Some(isr(1, 2)), "commits are monotonic");
    }

    #[test]
    fn proposer_adopts_the_highest_accepted_value() {
        let p = ids(3);
        let b = |r, i: usize| Ballot { round: r, proposer: p[i] };
        let prop = Proposal { epoch: 1, ballot: b(5, 0), own: isr(1, 9) };
        let replies = vec![
            (p[0], PrepareReply::Promise { accepted: Some((b(2, 1), isr(1, 3))) }),
            (p[1], PrepareReply::Promise { accepted: Some((b(4, 2), isr(1, 4))) }),
            (p[2], PrepareReply::Promise { accepted: None }),
        ];
        assert_eq!(prop.after_prepare(&replies, 3), AfterPrepare::Accept(isr(1, 4)));
        assert_eq!(prop.after_prepare(&replies[2..], 3), AfterPrepare::Retry { seen: None });
        let fresh: Vec<_> = p.iter().map(|n| (*n, PrepareReply::Promise { accepted: None })).collect();
        assert_eq!(prop.after_prepare(&fresh, 3), AfterPrepare::Accept(isr(1, 9)));
        let dup = vec![(p[0], PrepareReply::Promise { accepted: None }); 3];
        assert_eq!(prop.after_prepare(&dup, 3), AfterPrepare::Retry { seen: None },
            "one acceptor's duplicated promise must not count as a majority");
    }

    #[test]
    fn initial_members_are_order_independent_and_capped() {
        let n = ids(5);
        let f = dfs_common::FileId::new();
        let a = initial_members(f, 7, &[n[0], n[1], n[2]], 2);
        let b = initial_members(f, 7, &[n[2], n[0], n[1], n[0]], 2);
        assert_eq!(a, b);
        assert_eq!(a.len(), 2);
    }
}
