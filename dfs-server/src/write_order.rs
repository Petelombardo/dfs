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
//!
//! Still prototype limits: state is in memory only; a secondary that never received a version
//! (primary crashed mid-send, or restarted) skips over it after `wait_turn`'s timeout and logs
//! the gap instead of fetching the missing write from the primary; folds are not yet ordered,
//! so a replica-local fold can give the same content a different id on the two replicas.

use dashmap::DashMap;
use dfs_common::{ChunkId, FileId};
use std::collections::HashMap;
use std::sync::Arc;

type SlotKey = (FileId, u64, u64); // file, chunk_idx, isr_epoch

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
}

#[derive(Debug, PartialEq, Eq)]
pub enum TurnError {
    /// The primary never announced a version for this write.
    NoOrder,
    /// This write is version `version`, but versions after `applied` and before it never
    /// arrived.
    Gap { version: u64, applied: u64 },
}

impl SlotOrder {
    /// Primary: the next version for this slot.
    pub fn assign(&self) -> u64 {
        let mut g = self.inner.lock().unwrap();
        g.next += 1;
        g.next
    }

    /// Secondary: the primary says `write_id` is `version`, applied onto `base`.
    pub fn record_order(&self, write_id: u128, version: u64, base: ChunkId) {
        self.inner.lock().unwrap().orders.insert(write_id, (version, base));
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

    #[test]
    fn primary_hands_out_consecutive_versions_per_slot_and_epoch() {
        let o = WriteOrdering::default();
        let f = FileId::new();
        assert_eq!((o.slot(f, 0, 1).assign(), o.slot(f, 0, 1).assign()), (1, 2));
        assert_eq!(o.slot(f, 1, 1).assign(), 1, "another chunk is its own stream");
        assert_eq!(o.slot(f, 0, 2).assign(), 1, "a new ISR epoch starts a new stream");
    }
}
