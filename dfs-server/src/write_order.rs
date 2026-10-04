//! Per-slot write ordering (SLOT-OWNERSHIP-PLAN 3c, `DFS_ORDERED_WRITES`).
//!
//! Without it two clients writing one chunk can land their patches on the two replicas in
//! different orders (suite T64: replica disagreements and a lost acked write). The client
//! still sends every write's bytes to both ISR members in parallel; the primary gives each
//! write the slot's next version under `order_lock` and tells the secondary with a small
//! `WriteOrder` message; the secondary applies a write only once it holds that write's
//! version and has applied every earlier one. Versions are per (file, chunk, ISR epoch):
//! a new epoch is a new stream.
//!
//! Each replica applies version `n` onto its own result for version `n-1` (`head`), never onto
//! the leader's chunk_map: that map is the leader's arbitration, propagated asynchronously, and
//! routinely names an identity this replica doesn't hold (suite T64d).
//!
//! Continuity is tracked BY VERSION, never by comparing chunk ids. A patch token's id is
//! `hash(accumulated delta)`: it omits the base, so two replicas can share a token id while
//! holding different bytes, and hold the same bytes under different ids (a fold on one side,
//! a backfill). `head_version` says which version this replica's head reflects; the secondary
//! applies `v` only onto a head at `v-1`. Real chunk ids are content hashes, so they ARE
//! trustworthy: they are the anchors. The primary starts every stream (and restarts it after
//! one of its own applies failed) from a real chunk, and a secondary handed a real base adopts
//! it, pulling it hash-verified if it lacks it. A secondary whose head can't continue (an
//! apply failed here, a version never arrived, it just started, an unordered write touched
//! the slot) RESYNCS: the primary materializes its head as a real chunk at its current
//! version and the secondary pulls it; writes up to that version are refused here (the client
//! backfills them) and the stream continues from the shared anchor (suite T66[secondary]).
//!
//! `stream` is the primary's boot nonce: a restarted primary hands out versions from 1 again,
//! which must never be read as continuing the old stream. State is in memory only.

use dashmap::DashMap;
use dfs_common::{ChunkId, FileId, Response};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

type SlotKey = (FileId, u64, u64); // file, chunk_idx, isr_epoch

/// The version a primary announces for a write it refused to order. Real versions start at 1.
pub const REFUSED: u64 = 0;

/// Test-only (DFS_FAULT_INJECTION=1, `Request::InjectOrderedApplyFailures`): the primary
/// fails its own apply of this many upcoming ordered writes, after announcing them.
pub static INJECTED_APPLY_FAILURES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Consume one injected apply failure, if any are armed.
pub fn take_injected_apply_failure() -> bool {
    INJECTED_APPLY_FAILURES
        .fetch_update(std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
}

/// How many decided writes each slot remembers, so a resent copy of a write gets the same answer.
const DECIDED_CAP: usize = 1024;

/// What a replica decided for one write. Each write is decided exactly once: the client's
/// transport resends a request whose reply timed out (a stalled node), and a primary that
/// refused the stale copy (lease lapsed during the stall) and then ordered the resent one left
/// its secondary failing a write the primary applied, and the pair diverged (suite T66[primary]).
#[derive(Clone, Debug)]
pub enum Decision {
    Refused,
    /// `response` is None while the write is still being applied.
    Ordered { version: u64, response: Option<Response> },
}

pub struct WriteOrdering {
    /// This process's boot nonce, sent with every `WriteOrder` this node issues as a primary.
    pub boot: u64,
    slots: DashMap<SlotKey, Arc<SlotOrder>>,
    /// Latest ISR epoch seen per (file, chunk_idx), so an unordered write can find the
    /// slot's stream without scanning every slot.
    latest_epoch: DashMap<(FileId, u64), u64>,
}

impl Default for WriteOrdering {
    fn default() -> Self {
        WriteOrdering { boot: uuid::Uuid::new_v4().as_u64_pair().0, slots: DashMap::new(), latest_epoch: DashMap::new() }
    }
}

impl WriteOrdering {
    pub fn slot(&self, file_id: FileId, chunk_idx: u64, isr_epoch: u64) -> Arc<SlotOrder> {
        self.latest_epoch.entry((file_id, chunk_idx))
            .and_modify(|e| *e = (*e).max(isr_epoch))
            .or_insert(isr_epoch);
        self.slots.entry((file_id, chunk_idx, isr_epoch)).or_default().clone()
    }

    /// Whether `id` is this replica's head for the slot's newest ordered stream: acked data
    /// that nothing on this node may discard as superseded.
    pub fn is_head(&self, file_id: FileId, chunk_idx: u64, id: ChunkId) -> bool {
        let Some(epoch) = self.latest_epoch.get(&(file_id, chunk_idx)).map(|e| *e) else { return false };
        self.slots.get(&(file_id, chunk_idx, epoch))
            .is_some_and(|slot| slot.inner.lock().unwrap().head == Some(id))
    }

    /// This replica's head for the slot's newest ordered stream, if it has one.
    pub fn latest_head(&self, file_id: FileId, chunk_idx: u64) -> Option<ChunkId> {
        let epoch = self.latest_epoch.get(&(file_id, chunk_idx)).map(|e| *e)?;
        self.slots.get(&(file_id, chunk_idx, epoch)).and_then(|slot| slot.inner.lock().unwrap().head)
    }

    /// A fold turned the patch token `token` of slot (file, chunk_idx) into the real chunk
    /// `result` (same bytes). If that token is still the slot's ordered head, the head becomes
    /// `result`, at the same version: left on the token, the head pointed at a retired id once
    /// the fold's cleanup removed it (suite T66: a replica's head unreadable), and the next
    /// ordered write had to re-anchor or resync. Both replicas fold the same token into the
    /// same content-addressed result, so this also re-converges their ids. Compare-and-set: an
    /// ordered write that already moved the head past `token` wins.
    pub fn on_folded(&self, file_id: FileId, chunk_idx: u64, token: ChunkId, result: ChunkId) {
        if let Some(slot) = self.latest_slot(file_id, chunk_idx) {
            let mut g = slot.inner.lock().unwrap();
            if g.head == Some(token) {
                g.head = Some(result);
            }
        }
    }

    /// Whether the slot's ordered stream applied a version here within `window`. A background
    /// (unordered) fold must wait while it is: folding one replica mid-stream resets its
    /// accumulator, so later writes mint different ids on the two replicas (suite T67 after a
    /// replacement: a no-op retry folded anchor+delta -> anchor on the secondary only). While
    /// the stream is active the client's ordered ForceFold folds; once it is quiet, both
    /// replicas fold the same content into the same content-addressed id.
    pub fn stream_active(&self, file_id: FileId, chunk_idx: u64, window: std::time::Duration) -> bool {
        self.latest_slot(file_id, chunk_idx).is_some_and(|slot| {
            slot.inner.lock().unwrap().last_active.is_some_and(|t| t.elapsed() < window)
        })
    }

    /// The slot's newest ordered stream, if it has one (never creates one).
    pub fn latest_slot(&self, file_id: FileId, chunk_idx: u64) -> Option<Arc<SlotOrder>> {
        let epoch = self.latest_epoch.get(&(file_id, chunk_idx)).map(|e| *e)?;
        self.slots.get(&(file_id, chunk_idx, epoch)).map(|s| s.clone())
    }

    /// An unordered write changed this slot here: the ordered stream's head no longer
    /// names this replica's current state, so the next ordered write takes its base fresh.
    pub fn clear_head(&self, file_id: FileId, chunk_idx: u64) {
        let Some(epoch) = self.latest_epoch.get(&(file_id, chunk_idx)).map(|e| *e) else { return };
        if let Some(slot) = self.slots.get(&(file_id, chunk_idx, epoch)) {
            let mut g = slot.inner.lock().unwrap();
            g.head = None;
            g.head_version = None;
        }
    }
}

#[derive(Default)]
pub struct SlotOrder {
    /// Held from assigning (primary) or taking (secondary) a version until that write is
    /// applied here, so this node applies in version order.
    pub order_lock: tokio::sync::Mutex<()>,
    inner: std::sync::Mutex<Inner>,
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct Inner {
    /// Last version handed out (primary side).
    next: u64,
    /// Last version applied on this node.
    applied: u64,
    /// write_id -> (version, the primary's base for it), as the primary announced it
    /// (secondary side).
    orders: HashMap<u128, (u64, ChunkId)>,
    /// This replica's chunk id for the slot after the last version it applied.
    head: Option<ChunkId>,
    /// The version `head` reflects (every version up to it applied here, in order). None: the
    /// head can't be continued (nothing yet, an apply failed, an unordered write, a new stream).
    head_version: Option<u64>,
    /// Secondary: the primary's boot nonce for the stream being followed.
    stream: Option<u64>,
    decided: HashMap<u128, Decision>,
    decided_order: VecDeque<u128>,
    /// When this replica last applied (or anchored) a version of the stream.
    last_active: Option<std::time::Instant>,
}

impl Inner {
    fn decide(&mut self, write_id: u128, d: Decision) {
        if self.decided.insert(write_id, d).is_none() {
            self.decided_order.push_back(write_id);
            while self.decided_order.len() > DECIDED_CAP {
                if let Some(old) = self.decided_order.pop_front() {
                    self.decided.remove(&old);
                }
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TurnError {
    /// The primary never announced a version for this write.
    NoOrder,
    /// The primary refused to order this write (not the primary, no lease, stale ISR):
    /// it announced version `REFUSED`, so the secondary fails it at once.
    Refused,
    /// This write is version `version` on `base`, but versions after `applied` and before it
    /// never arrived.
    Gap { version: u64, base: ChunkId, applied: u64 },
}

impl SlotOrder {
    /// Primary: the next version for this slot.
    #[cfg(test)]
    pub fn assign(&self) -> u64 {
        let mut g = self.inner.lock().unwrap();
        g.next += 1;
        g.next
    }

    /// What this replica already decided for `write_id`, if anything.
    pub fn decided(&self, write_id: u128) -> Option<Decision> {
        self.inner.lock().unwrap().decided.get(&write_id).cloned()
    }

    /// Primary: the next version for `write_id`, or what was already decided for it.
    pub fn assign_once(&self, write_id: u128) -> Result<u64, Decision> {
        let mut g = self.inner.lock().unwrap();
        if let Some(d) = g.decided.get(&write_id) {
            return Err(d.clone());
        }
        g.next += 1;
        let version = g.next;
        g.decide(write_id, Decision::Ordered { version, response: None });
        Ok(version)
    }

    /// Primary: refuse `write_id`. Returns what was already decided instead, if it was.
    pub fn refuse_once(&self, write_id: u128) -> Option<Decision> {
        let mut g = self.inner.lock().unwrap();
        if let Some(d) = g.decided.get(&write_id) {
            return Some(d.clone());
        }
        g.decide(write_id, Decision::Refused);
        None
    }

    /// Either side: `write_id` (version `version`) is applied here and answered `response`.
    pub fn finish(&self, write_id: u128, version: u64, response: &Response) {
        let mut g = self.inner.lock().unwrap();
        g.decided.remove(&write_id);
        g.decide(write_id, Decision::Ordered { version, response: Some(response.clone()) });
    }

    /// Secondary: the primary (boot nonce `stream`) says `write_id` is `version`, applied onto
    /// `base`. The first word on a write stands: the primary decides each write once, so a
    /// second message can only be a resend. A new nonce is a restarted primary: a new stream.
    pub fn record_order(&self, write_id: u128, version: u64, base: ChunkId, stream: u64) {
        let mut g = self.inner.lock().unwrap();
        if g.stream.is_some_and(|s| s != stream) {
            g.applied = 0;
            g.head_version = None;
            g.orders.clear();
        }
        g.stream = Some(stream);
        if g.decided.contains_key(&write_id) {
            return;
        }
        g.orders.entry(write_id).or_insert((version, base));
        drop(g);
        self.changed.notify_waiters();
    }

    /// This replica's chunk id for the slot after its last applied version, if it has one.
    pub fn head(&self) -> Option<ChunkId> {
        self.inner.lock().unwrap().head
    }

    /// The version this replica's head reflects, if the head can be continued.
    pub fn head_version(&self) -> Option<u64> {
        self.inner.lock().unwrap().head_version
    }

    /// Primary: the last version handed out.
    pub fn assigned(&self) -> u64 {
        self.inner.lock().unwrap().next
    }

    /// Either side: this replica now holds the real chunk `id` as its state after `version`
    /// (stream start, a real base from the primary, or a resync). Orders up to it are moot.
    pub fn set_anchor(&self, id: ChunkId, version: u64) {
        let mut g = self.inner.lock().unwrap();
        g.last_active = Some(std::time::Instant::now());
        g.head = Some(id);
        g.head_version = Some(version);
        g.applied = g.applied.max(version);
        g.orders.retain(|_, (v, _)| *v > version);
        drop(g);
        self.changed.notify_waiters();
    }

    /// Secondary: wait until `write_id` has a version and every earlier version is applied.
    /// Returns the version and the primary's base for it.
    pub async fn wait_turn(&self, write_id: u128, timeout: std::time::Duration) -> Result<(u64, ChunkId), TurnError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let g = self.inner.lock().unwrap();
                if let Some(&(v, base)) = g.orders.get(&write_id) {
                    if v == REFUSED {
                        drop(g);
                        self.inner.lock().unwrap().orders.remove(&write_id);
                        return Err(TurnError::Refused);
                    }
                    // Wait for every earlier version even when this replica's head can't be
                    // continued: at stream start that is the normal state, and v2 must not
                    // overtake v1 (a failed version counts as applied, so it never blocks).
                    if g.applied + 1 >= v {
                        return Ok((v, base));
                    }
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                let g = self.inner.lock().unwrap();
                return Err(match g.orders.get(&write_id) {
                    Some(&(version, base)) => TurnError::Gap { version, base, applied: g.applied },
                    None => TurnError::NoOrder,
                });
            }
        }
    }

    /// Either side: `version` was applied onto the head at `version - 1`. `new_head` is the
    /// chunk id it produced, or None if applying it failed: the head no longer reflects the
    /// stream (on the primary the next write re-anchors; a secondary resyncs).
    pub fn applied(&self, write_id: u128, version: u64, new_head: Option<ChunkId>) {
        let mut g = self.inner.lock().unwrap();
        g.last_active = Some(std::time::Instant::now());
        g.applied = g.applied.max(version);
        g.orders.remove(&write_id);
        match new_head {
            Some(id) => { g.head = Some(id); g.head_version = Some(version); }
            None => g.head_version = None,
        }
        drop(g);
        self.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn secondary_applies_in_the_primarys_order_whatever_order_payloads_arrive_in() {
        let o = WriteOrdering::default();
        let s = o.slot(FileId::new(), 1, 1);
        // Payloads for writes 20 and 10 arrive first; the primary ordered 10 before 20.
        let s2 = s.clone();
        let late = tokio::spawn(async move { s2.wait_turn(20, Duration::from_secs(2)).await });
        let (b1, b2) = (cid(1), cid(2));
        s.set_anchor(cid(9), 0);
        s.record_order(20, 2, b2, 1);
        s.record_order(10, 1, b1, 1);
        assert_eq!(s.wait_turn(10, Duration::from_secs(1)).await, Ok((1, b1)));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!late.is_finished(), "version 2 must wait for version 1 to be applied");
        s.applied(10, 1, Some(b2));
        assert_eq!(late.await.unwrap(), Ok((2, b2)));
    }

    #[tokio::test]
    async fn missing_order_or_predecessor_times_out_with_what_is_missing() {
        let o = WriteOrdering::default();
        let s = o.slot(FileId::new(), 0, 3);
        assert_eq!(s.wait_turn(7, Duration::from_millis(50)).await, Err(TurnError::NoOrder));
        s.record_order(7, 3, cid(3), 1); // versions 1 and 2 never arrive
        assert_eq!(s.wait_turn(7, Duration::from_millis(50)).await, Err(TurnError::Gap { version: 3, base: cid(3), applied: 0 }),
            "a fresh stream still waits for v1 and v2 before v3");
        s.set_anchor(cid(1), 1);
        s.record_order(8, 4, cid(4), 1);
        assert_eq!(s.wait_turn(8, Duration::from_millis(50)).await, Err(TurnError::Gap { version: 4, base: cid(4), applied: 1 }));
    }

    #[test]
    fn a_fold_of_the_head_token_moves_the_head_to_the_result() {
        let o = WriteOrdering::default();
        let f = FileId::new();
        let s = o.slot(f, 2, 1);
        s.set_anchor(cid(1), 0);
        s.applied(1, 1, Some(cid(2)));
        o.on_folded(f, 2, cid(9), cid(8));
        assert_eq!(s.head(), Some(cid(2)), "a fold of another token leaves the head alone");
        o.on_folded(f, 2, cid(2), cid(3));
        assert_eq!((s.head(), s.head_version()), (Some(cid(3)), Some(1)), "same version, real id");
    }

    #[test]
    fn a_restarted_primary_starts_a_new_stream() {
        let o = WriteOrdering::default();
        let s = o.slot(FileId::new(), 0, 1);
        s.set_anchor(cid(1), 0);
        s.record_order(1, 1, cid(1), 100);
        s.applied(1, 1, Some(cid(2)));
        assert_eq!(s.head_version(), Some(1));
        // Same epoch, new boot nonce: versions restart at 1 and must not count as continuing.
        s.record_order(2, 1, cid(3), 200);
        assert_eq!(s.head_version(), None);
        assert_eq!(s.inner.lock().unwrap().applied, 0);
    }

    fn cid(n: u8) -> ChunkId {
        ChunkId { hash: [n; 32] }
    }

    #[test]
    fn head_follows_applied_versions_and_is_dropped_by_gaps_and_unordered_writes() {
        let o = WriteOrdering::default();
        let f = FileId::new();
        let s = o.slot(f, 4, 2);
        assert_eq!((s.head(), s.head_version()), (None, None), "no ordered write yet");
        s.set_anchor(cid(1), 0);
        s.applied(1, 1, Some(cid(1)));
        assert_eq!((s.head(), s.head_version()), (Some(cid(1)), Some(1)));
        s.applied(2, 2, None);
        assert_eq!(s.head_version(), None, "a failed apply: the head no longer reflects the stream");
        s.record_order(3, 2, cid(5), 1);
        s.record_order(4, 3, cid(5), 1);
        s.set_anchor(cid(5), 2);
        assert_eq!(s.inner.lock().unwrap().orders.len(), 1, "an anchor makes orders up to it moot");
        o.clear_head(f, 4);
        assert_eq!((s.head(), s.head_version()), (None, None), "an unordered write to the slot drops the head");
        o.slot(f, 4, 3).applied(1, 1, Some(cid(7)));
        assert!(o.is_head(f, 4, cid(7)) && !o.is_head(f, 4, cid(3)) && !o.is_head(f, 5, cid(7)));
        o.clear_head(f, 4);
        assert_eq!(o.slot(f, 4, 3).head(), None, "clear_head reaches the newest epoch's stream");
    }

    #[tokio::test]
    async fn a_refused_write_fails_at_once_instead_of_waiting_out_the_timeout() {
        let o = WriteOrdering::default();
        let s = o.slot(FileId::new(), 1, 1);
        let s2 = s.clone();
        let waiting = tokio::spawn(async move { s2.wait_turn(5, Duration::from_secs(30)).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        s.record_order(5, REFUSED, cid(0), 1);
        let r = tokio::time::timeout(Duration::from_secs(1), waiting).await
            .expect("the refusal must end the wait, not the 30s timeout").unwrap();
        assert_eq!(r, Err(TurnError::Refused));
    }

    #[test]
    fn each_write_is_decided_once_whatever_copy_arrives_later() {
        let o = WriteOrdering::default();
        let s = o.slot(FileId::new(), 1, 1);
        // Refused first (lease lapsed), then the resent copy arrives with the lease back.
        assert!(s.refuse_once(7).is_none());
        assert!(matches!(s.assign_once(7), Err(Decision::Refused)));
        // Ordered first: a later refusal attempt gets the order back, and so does a resend.
        let v = s.assign_once(8).unwrap();
        assert!(matches!(s.refuse_once(8), Some(Decision::Ordered { version, response: None }) if version == v));
        s.finish(8, v, &Response::Ok { data: None });
        assert!(matches!(s.decided(8), Some(Decision::Ordered { response: Some(Response::Ok { .. }), .. })));
        // The secondary keeps the first word on a write.
        s.record_order(9, 3, cid(3), 1);
        s.record_order(9, REFUSED, cid(0), 1);
        assert_eq!(s.inner.lock().unwrap().orders.get(&9).map(|o| o.0), Some(3));
    }

    #[test]
    fn primary_hands_out_consecutive_versions_per_slot_and_epoch() {
        let o = WriteOrdering::default();
        let f = FileId::new();
        assert_eq!((o.slot(f, 0, 1).assign(), o.slot(f, 0, 1).assign()), (1, 2));
        assert_eq!(o.slot(f, 1, 1).assign(), 1, "another chunk is its own stream");
        assert_eq!(o.slot(f, 0, 2).assign(), 1, "a new ISR epoch starts a new stream");
    }
}
