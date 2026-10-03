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
//! routinely names an identity this replica doesn't hold (another replica's patch token). Built
//! on it, a secondary rejected ordered writes as stale and two concurrent writers drew EIO
//! (suite T64d). The primary sends the base it applied each version onto with `WriteOrder`;
//! a replica with no head yet (first ordered write, or an unordered patch cleared it) uses that.
//! Patch-token ids are local to each replica's accumulator, so the secondary follows its own
//! head rather than comparing ids with the primary's base.
//!
//! Still prototype limits: state is in memory only; a secondary that never received a version
//! (primary crashed mid-send, or restarted) skips over it after `wait_turn`'s timeout and logs
//! the gap instead of fetching the missing write from the primary; folds are not yet ordered,
//! so a replica-local fold can give the same content a different id on the two replicas.

use dashmap::DashMap;
use dfs_common::{ChunkId, FileId, Response};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

type SlotKey = (FileId, u64, u64); // file, chunk_idx, isr_epoch

/// The version a primary announces for a write it refused to order. Real versions start at 1.
pub const REFUSED: u64 = 0;

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

#[derive(Default)]
pub struct WriteOrdering {
    slots: DashMap<SlotKey, Arc<SlotOrder>>,
    /// Latest ISR epoch seen per (file, chunk_idx), so an unordered write can find the
    /// slot's stream without scanning every slot.
    latest_epoch: DashMap<(FileId, u64), u64>,
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

    /// An unordered write changed this slot here: the ordered stream's head no longer
    /// names this replica's current state, so the next ordered write takes its base fresh.
    pub fn clear_head(&self, file_id: FileId, chunk_idx: u64) {
        let Some(epoch) = self.latest_epoch.get(&(file_id, chunk_idx)).map(|e| *e) else { return };
        if let Some(slot) = self.slots.get(&(file_id, chunk_idx, epoch)) {
            slot.inner.lock().unwrap().head = None;
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
    decided: HashMap<u128, Decision>,
    decided_order: VecDeque<u128>,
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
    /// This write is version `version`, but versions after `applied` and before it never
    /// arrived.
    Gap { version: u64, applied: u64 },
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

    /// Secondary: the primary says `write_id` is `version`, applied onto `base`. The first
    /// word on a write stands: the primary decides each write once, so a second message can
    /// only be a resend.
    pub fn record_order(&self, write_id: u128, version: u64, base: ChunkId) {
        let mut g = self.inner.lock().unwrap();
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
                    if g.applied + 1 >= v {
                        return Ok((v, base));
                    }
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                let g = self.inner.lock().unwrap();
                return Err(match g.orders.get(&write_id) {
                    Some(&(version, _)) => TurnError::Gap { version, applied: g.applied },
                    None => TurnError::NoOrder,
                });
            }
        }
    }

    /// Either side: `version` is done here. `new_head` is the chunk id it produced, or None
    /// if applying it failed (the head stays on the last version that did apply).
    pub fn applied(&self, write_id: u128, version: u64, new_head: Option<ChunkId>) {
        let mut g = self.inner.lock().unwrap();
        g.applied = g.applied.max(version);
        g.orders.remove(&write_id);
        if new_head.is_some() {
            g.head = new_head;
        }
        drop(g);
        self.changed.notify_waiters();
    }

    /// Secondary skipping a gap: treat everything before `version` as applied. The head
    /// misses the skipped writes, so drop it: the next write takes the primary's base.
    pub fn skip_to(&self, version: u64) {
        let mut g = self.inner.lock().unwrap();
        g.applied = g.applied.max(version.saturating_sub(1));
        g.head = None;
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
        s.record_order(20, 2, b2);
        s.record_order(10, 1, b1);
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
        s.record_order(7, 3, cid(3)); // versions 1 and 2 never arrive
        assert_eq!(s.wait_turn(7, Duration::from_millis(50)).await, Err(TurnError::Gap { version: 3, applied: 0 }));
        s.skip_to(3);
        assert_eq!(s.wait_turn(7, Duration::from_millis(50)).await, Ok((3, cid(3))));
    }

    fn cid(n: u8) -> ChunkId {
        ChunkId { hash: [n; 32] }
    }

    #[test]
    fn head_follows_applied_versions_and_is_dropped_by_gaps_and_unordered_writes() {
        let o = WriteOrdering::default();
        let f = FileId::new();
        let s = o.slot(f, 4, 2);
        assert_eq!(s.head(), None, "no ordered write yet: take the primary's base");
        s.applied(1, 1, Some(cid(1)));
        assert_eq!(s.head(), Some(cid(1)));
        s.applied(2, 2, None);
        assert_eq!(s.head(), Some(cid(1)), "a failed apply leaves the head on the last success");
        o.clear_head(f, 4);
        assert_eq!(s.head(), None, "an unordered write to the slot drops the head");
        s.applied(3, 3, Some(cid(3)));
        s.skip_to(9);
        assert_eq!(s.head(), None, "a skipped gap drops the head");
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
        s.record_order(5, REFUSED, cid(0));
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
        s.record_order(9, 3, cid(3));
        s.record_order(9, REFUSED, cid(0));
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
