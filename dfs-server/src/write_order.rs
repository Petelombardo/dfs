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
//! Prototype limits (the measurement build, not yet the full 3c): state is in memory only;
//! a secondary that never received a version (primary crashed mid-send, or restarted)
//! skips over it after `wait_turn`'s timeout and logs the gap instead of fetching the
//! missing write from the primary; folds are not yet ordered.

use dashmap::DashMap;
use dfs_common::FileId;
use std::collections::HashMap;
use std::sync::Arc;

type SlotKey = (FileId, u64, u64); // file, chunk_idx, isr_epoch

#[derive(Default)]
pub struct WriteOrdering {
    slots: DashMap<SlotKey, Arc<SlotOrder>>,
}

impl WriteOrdering {
    pub fn slot(&self, file_id: FileId, chunk_idx: u64, isr_epoch: u64) -> Arc<SlotOrder> {
        self.slots.entry((file_id, chunk_idx, isr_epoch)).or_default().clone()
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
    /// write_id -> version, as the primary announced it (secondary side).
    orders: HashMap<u128, u64>,
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

    /// Secondary: the primary says `write_id` is `version`.
    pub fn record_order(&self, write_id: u128, version: u64) {
        self.inner.lock().unwrap().orders.insert(write_id, version);
        self.changed.notify_waiters();
    }

    /// Secondary: wait until `write_id` has a version and every earlier version is applied.
    pub async fn wait_turn(&self, write_id: u128, timeout: std::time::Duration) -> Result<u64, TurnError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let g = self.inner.lock().unwrap();
                if let Some(&v) = g.orders.get(&write_id) {
                    if g.applied + 1 >= v {
                        return Ok(v);
                    }
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                let g = self.inner.lock().unwrap();
                return Err(match g.orders.get(&write_id) {
                    Some(&version) => TurnError::Gap { version, applied: g.applied },
                    None => TurnError::NoOrder,
                });
            }
        }
    }

    /// Either side: `version` is applied here (or given up on: a gap the secondary skips).
    pub fn applied(&self, write_id: u128, version: u64) {
        let mut g = self.inner.lock().unwrap();
        g.applied = g.applied.max(version);
        g.orders.remove(&write_id);
        drop(g);
        self.changed.notify_waiters();
    }

    /// Secondary skipping a gap: treat everything before `version` as applied.
    pub fn skip_to(&self, version: u64) {
        let mut g = self.inner.lock().unwrap();
        g.applied = g.applied.max(version.saturating_sub(1));
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
        s.record_order(20, 2);
        s.record_order(10, 1);
        assert_eq!(s.wait_turn(10, Duration::from_secs(1)).await, Ok(1));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!late.is_finished(), "version 2 must wait for version 1 to be applied");
        s.applied(10, 1);
        assert_eq!(late.await.unwrap(), Ok(2));
    }

    #[tokio::test]
    async fn missing_order_or_predecessor_times_out_with_what_is_missing() {
        let o = WriteOrdering::default();
        let s = o.slot(FileId::new(), 0, 3);
        assert_eq!(s.wait_turn(7, Duration::from_millis(50)).await, Err(TurnError::NoOrder));
        s.record_order(7, 3); // versions 1 and 2 never arrive
        assert_eq!(s.wait_turn(7, Duration::from_millis(50)).await, Err(TurnError::Gap { version: 3, applied: 0 }));
        s.skip_to(3);
        assert_eq!(s.wait_turn(7, Duration::from_millis(50)).await, Ok(3));
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
