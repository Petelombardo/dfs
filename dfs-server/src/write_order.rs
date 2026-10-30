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

/// The version a primary announces for a write it turned away as busy (`Admission`): nothing is
/// decided for it, and a resend of the same write may still be ordered. The secondary answers
/// its copy busy too, so the client's pair retry stays in step.
pub const BUSY: u64 = u64::MAX;

/// Test-only (DFS_FAULT_INJECTION=1, `Request::InjectOrderedApplyFailures`): the primary
/// fails its own apply of this many upcoming ordered writes, after announcing them.
pub static INJECTED_APPLY_FAILURES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Consume one injected apply failure, if any are armed.
pub fn take_injected_apply_failure() -> bool {
    INJECTED_APPLY_FAILURES
        .fetch_update(std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
}

/// Test-only (DFS_FAULT_INJECTION=1 and DFS_FAULT_SECONDARY_EXIT="before:N" or "after:N"): this
/// node exits, as a crash would, on its Nth ordered write as a secondary, before applying it or
/// after applying it but before answering (suite T120).
pub fn secondary_exit_point() -> Option<(bool, u64)> {
    static AT: std::sync::OnceLock<Option<(bool, u64)>> = std::sync::OnceLock::new();
    *AT.get_or_init(|| {
        if std::env::var("DFS_FAULT_INJECTION").ok().as_deref() != Some("1") {
            return None;
        }
        let v = std::env::var("DFS_FAULT_SECONDARY_EXIT").ok()?;
        let (when, n) = v.split_once(':')?;
        Some((when == "after", n.parse().ok()?))
    })
}

/// Count one ordered write as a secondary; whether it is the one `secondary_exit_point` names.
pub fn secondary_exit_now() -> Option<bool> {
    static SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let (after, n) = secondary_exit_point()?;
    (SEEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == n).then_some(after)
}

/// Test-only (DFS_FAULT_INJECTION=1, `Request::InjectSlowBackgroundFolds`): the next
/// `INJECTED_SLOW_FOLDS` background folds hold the slot's patch lock this long first.
pub static INJECTED_SLOW_FOLD_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static INJECTED_SLOW_FOLDS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Consume one injected slow fold, if any are armed: how long to hold.
pub fn take_injected_slow_fold() -> Option<u64> {
    INJECTED_SLOW_FOLDS
        .fetch_update(std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst, |n| n.checked_sub(1))
        .ok()
        .map(|_| INJECTED_SLOW_FOLD_MS.load(std::sync::atomic::Ordering::SeqCst))
}

/// Test-only (DFS_FAULT_INJECTION=1, `Request::InjectSlowFoldAdopts`): the next
/// `INJECTED_SLOW_FOLD_ADOPTS` peer fold results this node receives wait this long before
/// it records them, so a write can land between a fold and its holders adopting it.
pub static INJECTED_SLOW_FOLD_ADOPT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static INJECTED_SLOW_FOLD_ADOPTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Consume one injected slow fold adopt, if any are armed: how long to wait.
pub fn take_injected_slow_fold_adopt() -> Option<u64> {
    INJECTED_SLOW_FOLD_ADOPTS
        .fetch_update(std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst, |n| n.checked_sub(1))
        .ok()
        .map(|_| INJECTED_SLOW_FOLD_ADOPT_MS.load(std::sync::atomic::Ordering::SeqCst))
}

/// Test-only (DFS_FAULT_INJECTION=1, `Request::InjectChunkLocationFailures`): until this
/// time (ms since UNIX epoch), chunk-location registrations fail as a failed commit would.
/// Time-based, not a count: the ordered and batched senders retry at very different rates.
pub static INJECTED_LOCATION_FAILURES_UNTIL_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Test-only (`Request::InjectIgnoreMetadataVersions`): until this time (ms since UNIX epoch),
/// metadata puts keep the stored chunk versions.
pub static INJECTED_IGNORE_METADATA_VERSIONS_UNTIL_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether a metadata put arriving now should keep the stored chunk versions.
pub fn ignoring_metadata_versions() -> bool {
    let until = INJECTED_IGNORE_METADATA_VERSIONS_UNTIL_MS.load(std::sync::atomic::Ordering::SeqCst);
    until != 0 && now_ms() < until
}

/// Whether a chunk-location registration arriving now should fail.
pub fn take_injected_location_failure() -> bool {
    let until = INJECTED_LOCATION_FAILURES_UNTIL_MS.load(std::sync::atomic::Ordering::SeqCst);
    until != 0 && now_ms() < until
}

/// Milliseconds since the UNIX epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// How many decided writes each slot remembers, so a resent copy of a write gets the same answer.
/// A decision younger than DECIDED_MIN_AGE is kept past this: a client resends a write for as
/// long as its secondary answers busy, and a primary that forgot the write ordered the resend
/// as a new version, applying its old bytes again over newer writes (suite T66[secondary],
/// 1030b: forgotten after 621 writes in 2 s, re-ordered as v622, and the pair split).
const DECIDED_CAP: usize = 1024;
const DECIDED_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(120);
/// The bound on memory whatever the decisions' age.
const DECIDED_HARD_CAP: usize = 65536;

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
    /// Highest ISR epoch at which this node caught up as the slot's secondary (the durable
    /// copy is the metadata store's SLOT_SYNCED_TABLE; this saves a write per ordered write).
    synced: DashMap<(FileId, u64), u64>,
    /// The ordered write work this node has on hand (see Admission).
    pub admission: Arc<Admission>,
}

impl Default for WriteOrdering {
    fn default() -> Self {
        WriteOrdering {
            boot: uuid::Uuid::new_v4().as_u64_pair().0, slots: DashMap::new(), latest_epoch: DashMap::new(),
            synced: DashMap::new(), admission: Arc::new(Admission::from_env()),
        }
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
        for slot in self.newest_two(file_id, chunk_idx) {
            let mut g = slot.inner.lock().unwrap();
            if g.head == Some(token) {
                g.head = Some(result);
            }
        }
    }

    /// The slot's newest stream and the one before it (see `previous_epoch_head`).
    fn newest_two(&self, file_id: FileId, chunk_idx: u64) -> Vec<Arc<SlotOrder>> {
        let Some(epoch) = self.latest_epoch.get(&(file_id, chunk_idx)).map(|e| *e) else { return Vec::new() };
        [Some(epoch), epoch.checked_sub(1)].into_iter().flatten()
            .filter_map(|e| self.slots.get(&(file_id, chunk_idx, e)).map(|s| s.clone()))
            .collect()
    }

    /// This node's continuable head of the slot's stream at the epoch just before `epoch`.
    /// It was in that epoch's ISR, and a write there was acked only once both members applied
    /// it, so this head holds every write acked before the change. A primary that stays on
    /// across an ISR change starts the new stream here: from the client's base instead (often
    /// a token this node had merged past and retired) it couldn't anchor, the replacement
    /// secondary couldn't resync, and the client's write failed with EIO (suite T74d, 10-13).
    /// None after an unordered write here (`clear_head`), a failed apply, or a restart.
    pub fn previous_epoch_head(&self, file_id: FileId, chunk_idx: u64, epoch: u64) -> Option<ChunkId> {
        let slot = self.slots.get(&(file_id, chunk_idx, epoch.checked_sub(1)?))?;
        let g = slot.inner.lock().unwrap();
        g.head_version.and(g.head)
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

    /// Record that this node caught up as the slot's secondary at `epoch`. True if that's
    /// news (the caller then persists it).
    pub fn raise_synced(&self, file_id: FileId, chunk_idx: u64, epoch: u64) -> bool {
        let mut e = self.synced.entry((file_id, chunk_idx)).or_insert(0);
        if *e >= epoch {
            return false;
        }
        *e = epoch;
        true
    }

    /// The highest epoch this process saw this node catch up at as the slot's secondary.
    pub fn synced_epoch(&self, file_id: FileId, chunk_idx: u64) -> u64 {
        self.synced.get(&(file_id, chunk_idx)).map_or(0, |e| *e)
    }

    /// The file was truncated to zero: every slot of it this node has an ordered stream for
    /// ends here (`clear_head` on each). Its bytes are gone, so no stream head may still name
    /// them to a reader or serve as the next ordered write's base.
    pub fn clear_file_heads(&self, file_id: FileId) {
        let idxs: Vec<u64> = self.latest_epoch.iter().filter(|e| e.key().0 == file_id).map(|e| e.key().1).collect();
        for idx in idxs {
            self.clear_head(file_id, idx);
        }
    }

    /// An unordered write changed this slot here: the ordered stream's head no longer
    /// names this replica's current state, so the next ordered write takes its base fresh.
    /// Clears the previous epoch's head too: it must not carry into a new stream (see
    /// `previous_epoch_head`) once this replica's state has moved past it.
    pub fn clear_head(&self, file_id: FileId, chunk_idx: u64) {
        for slot in self.newest_two(file_id, chunk_idx) {
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
    decided_order: VecDeque<(u128, std::time::Instant)>,
    /// Primary: the base each recent version was ordered on (oldest first, DECIDED_CAP of
    /// them), so a resent write's WriteOrder can be announced again exactly.
    bases: VecDeque<(u64, ChunkId)>,
    /// When this replica last applied (or anchored) a version of the stream.
    last_active: Option<std::time::Instant>,
}

impl Inner {
    fn decide(&mut self, write_id: u128, d: Decision) {
        if self.decided.insert(write_id, d).is_none() {
            let now = std::time::Instant::now();
            self.decided_order.push_back((write_id, now));
            while self.decided_order.len() > DECIDED_CAP {
                let Some(&(old, at)) = self.decided_order.front() else { break };
                if self.decided_order.len() <= DECIDED_HARD_CAP && now.duration_since(at) < DECIDED_MIN_AGE {
                    break;
                }
                self.decided_order.pop_front();
                self.decided.remove(&old);
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
    /// The primary turned this copy of the write away as busy (version `BUSY`); a resend may
    /// still be ordered.
    Busy,
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
        // In place: removing it first queued the write a second time in decided_order, so the
        // cap held half as many writes, and the stale entry's eviction dropped the live one.
        match g.decided.get_mut(&write_id) {
            Some(d) => *d = Decision::Ordered { version, response: Some(response.clone()) },
            None => g.decide(write_id, Decision::Ordered { version, response: Some(response.clone()) }),
        }
        drop(g);
        self.changed.notify_waiters();
    }

    /// What was decided for `write_id`, waiting up to `timeout` for a write still being applied
    /// to finish. A resent copy arrives because the first copy's reply timed out: on a node
    /// whose disk takes seconds per commit the write is usually nearly done, and answering
    /// "still being applied" failed a write the primary was about to ack (G8 1028 fix7: server2
    /// as primary, an fsync EIO 6s in). Returns the in-progress decision if it outlasts `timeout`.
    pub async fn await_decided(&self, write_id: u128, timeout: std::time::Duration) -> Option<Decision> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let d = self.decided(write_id);
            if !matches!(d, Some(Decision::Ordered { response: None, .. })) {
                return d;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.decided(write_id);
            }
        }
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
        // A real order replaces a busy one: the primary admitted a resend, and the two
        // WriteOrders can arrive in either order.
        if g.orders.get(&write_id).is_none_or(|&(v, _)| v == BUSY) {
            g.orders.insert(write_id, (version, base));
        }
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

    /// Primary: version `version` was ordered on `base`.
    pub fn note_base(&self, version: u64, base: ChunkId) {
        let mut g = self.inner.lock().unwrap();
        g.bases.push_back((version, base));
        while g.bases.len() > DECIDED_CAP {
            g.bases.pop_front();
        }
    }

    /// Primary: the base version `version` was ordered on, if it is recent.
    pub fn base_of(&self, version: u64) -> Option<ChunkId> {
        self.inner.lock().unwrap().bases.iter().rev().find(|(v, _)| *v == version).map(|(_, b)| *b)
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
        g.orders.retain(|_, (v, _)| *v > version && *v != BUSY);
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
                    if v == REFUSED || v == BUSY {
                        drop(g);
                        self.inner.lock().unwrap().orders.remove(&write_id);
                        return Err(if v == BUSY { TurnError::Busy } else { TurnError::Refused });
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

/// How much ordered write work this node has on hand, and whether it takes another write as a
/// primary. Every ordered write counts from the moment it arrives until it is done: a primary's
/// write from admission (before it waits for the slot's order lock), and a secondary's copy from
/// arrival, through waiting its turn and catching up, to its apply, even after its request was
/// answered "still being applied" (see Server::handle_ordered_inner). Only the primary may turn a
/// write away, before ordering it: after that both copies must apply it. So the primary admits a
/// write only while three things hold:
///
/// - fewer writes are in progress here than the limit. The limit adapts the way TCP's window
///   does: it halves (at most once per `target`) when a write takes longer than `target` plus its
///   payload's transfer time, and grows by about one per window of writes that finish in time.
///   A node that took every write it was sent worked on all of them at once when it fell behind,
///   each took longer, the client's 3 s reply deadline passed, the client sent the write again,
///   and the node did it twice (G8 1029 bafd835f: server2, a 2-core Celeron on a SATA SSD).
/// - the oldest write in progress here has waited less than `STALL_TARGETS` targets: a count
///   alone misreads 64 small writes on a fast node and 4 large ones on a slow one alike.
/// - the slot's secondary last reported (in its reply to a WriteOrder) its own oldest write
///   younger than that. A slow secondary is never dropped (its storage is the cluster's), so
///   the primary slows the writes it orders to the secondary's pace instead (suite T119: a
///   loaded secondary fell 30 s behind while its primary kept ordering).
///
/// A write turned away is answered busy at once, with no work done and nothing decided for it,
/// and the client sends it again shortly; it waits, it doesn't fail. With nothing in progress a
/// write is always admitted, so a node makes progress however slow it is.
pub struct Admission {
    target: std::time::Duration,
    max: f64,
    st: std::sync::Mutex<AdmissionState>,
    /// Each peer's last report (see `report`).
    peers: DashMap<dfs_common::NodeId, PeerLoad>,
}

/// What a peer last reported of its ordered write work.
#[derive(Clone, Copy, Debug)]
pub struct PeerLoad {
    /// How long its oldest write in progress had waited.
    pub oldest: std::time::Duration,
    /// How long it had gone without finishing any write while it had some in progress: slow
    /// work finishes, a stuck node's doesn't. None from a peer that doesn't report it.
    pub stalled: Option<std::time::Duration>,
    /// When the report arrived.
    pub at: std::time::Instant,
}

struct AdmissionState {
    limit: f64,
    next_id: u64,
    /// Start of each write in progress, by permit id.
    started: std::collections::BTreeMap<u64, std::time::Instant>,
    /// When a write in progress here last finished.
    last_done: Option<std::time::Instant>,
    last_cut: Option<std::time::Instant>,
}

/// One write in progress; reports how long it took when dropped.
pub struct AdmitPermit {
    adm: Arc<Admission>,
    id: u64,
    allowance: std::time::Duration,
}

impl Drop for AdmitPermit {
    fn drop(&mut self) {
        self.adm.done(self.id, self.allowance);
    }
}

/// Payload transfer time an admitted write is allowed beyond the target: a 4 MiB patch on a
/// healthy node is not a sign of overload.
const ADMISSION_BYTES_PER_SEC: f64 = 32.0 * 1024.0 * 1024.0;

/// A write in progress this many targets old means work is piling up here.
const STALL_TARGETS: u32 = 3;

/// A node whose work in progress has finished nothing for this long is stuck, not slow: its
/// slots' primaries replace it in their ISRs (Server::replace_isr_member).
pub const STALLED_MAX: std::time::Duration = std::time::Duration::from_secs(10);

/// A peer's report counts for this long.
const PEER_REPORT_TTL: std::time::Duration = std::time::Duration::from_secs(5);

impl Admission {
    pub fn new(target: std::time::Duration, max: usize) -> Self {
        let max = max.max(1) as f64;
        Admission {
            target, max,
            st: std::sync::Mutex::new(AdmissionState {
                limit: max, next_id: 0, started: std::collections::BTreeMap::new(), last_done: None, last_cut: None,
            }),
            peers: DashMap::new(),
        }
    }

    /// From DFS_WRITE_ADMISSION_TARGET_MS (default 1000, under the client's 3 s reply
    /// deadline) and DFS_WRITE_ADMISSION_MAX (default 64, the client's flush concurrency).
    pub fn from_env() -> Self {
        let env = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(d);
        Self::new(std::time::Duration::from_millis(env("DFS_WRITE_ADMISSION_TARGET_MS", 1000)), env("DFS_WRITE_ADMISSION_MAX", 64) as usize)
    }

    fn permit(self: &Arc<Self>, g: &mut AdmissionState, payload: usize) -> AdmitPermit {
        g.next_id += 1;
        g.started.insert(g.next_id, std::time::Instant::now());
        AdmitPermit {
            adm: self.clone(),
            id: g.next_id,
            allowance: std::time::Duration::from_secs_f64(payload as f64 / ADMISSION_BYTES_PER_SEC),
        }
    }

    /// Work this node must do whatever its load (a secondary's copy of an ordered write).
    pub fn admit(self: &Arc<Self>, payload: usize) -> AdmitPermit {
        let mut g = self.st.lock().unwrap();
        self.permit(&mut g, payload)
    }

    /// Admit a primary write of `payload` bytes for a slot whose secondary is `secondary`, or
    /// None (with why) if work is piling up here or there.
    pub fn try_admit(self: &Arc<Self>, payload: usize, secondary: dfs_common::NodeId) -> Result<AdmitPermit, String> {
        let stall = self.target * STALL_TARGETS;
        let mut g = self.st.lock().unwrap();
        if !g.started.is_empty() {
            let limit = (g.limit as usize).max(1);
            if g.started.len() >= limit {
                return Err(format!("{} of its {} writes in progress", g.started.len(), limit));
            }
            let oldest = g.started.values().min().map(|t| t.elapsed()).unwrap_or_default();
            if oldest > stall {
                return Err(format!("its oldest write in progress is {:?} old", oldest));
            }
        }
        if let Some(behind) = self.peer_backlog(secondary).filter(|b| *b > stall) {
            return Err(format!("its secondary {} reports its oldest write in progress {:?} old", secondary, behind));
        }
        Ok(self.permit(&mut g, payload))
    }

    fn done(&self, id: u64, allowance: std::time::Duration) {
        let mut g = self.st.lock().unwrap();
        let Some(start) = g.started.remove(&id) else { return };
        g.last_done = Some(std::time::Instant::now());
        if start.elapsed() > self.target + allowance {
            if g.last_cut.is_none_or(|t| t.elapsed() >= self.target) {
                g.limit = (g.limit / 2.0).max(1.0);
                g.last_cut = Some(std::time::Instant::now());
            }
        } else {
            g.limit = (g.limit + 1.0 / g.limit).min(self.max);
        }
    }

    /// How long this node has gone without finishing a write while it has some in progress
    /// (counted from the oldest one's start if none finished since): zero when idle.
    fn stalled(g: &AdmissionState) -> std::time::Duration {
        let Some(oldest) = g.started.values().min() else { return std::time::Duration::ZERO };
        g.last_done.map_or(*oldest, |d| d.max(*oldest)).elapsed()
    }

    /// This node's load for its WriteOrder replies, as 24 bytes (little-endian u64s): how many
    /// writes are in progress, how long the oldest has waited (ms), and how long it has gone
    /// without finishing one (ms; see `stalled`).
    pub fn report(&self) -> Vec<u8> {
        let g = self.st.lock().unwrap();
        let oldest = g.started.values().min().map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
        let mut out = (g.started.len() as u64).to_le_bytes().to_vec();
        out.extend_from_slice(&oldest.to_le_bytes());
        out.extend_from_slice(&(Self::stalled(&g).as_millis() as u64).to_le_bytes());
        out
    }

    /// A peer's `report`, from its reply to a WriteOrder. Anything else (an older peer answers
    /// with no data) is ignored.
    pub fn note_peer(&self, peer: dfs_common::NodeId, report: &[u8]) {
        let field = |i: usize| report.get(i * 8..i * 8 + 8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes);
        if let Some(ms) = field(1) {
            self.peers.insert(peer, PeerLoad {
                oldest: std::time::Duration::from_millis(ms),
                stalled: field(2).map(std::time::Duration::from_millis),
                at: std::time::Instant::now(),
            });
        }
    }

    /// The oldest write in progress `peer` last reported, if it reported recently.
    pub fn peer_backlog(&self, peer: dfs_common::NodeId) -> Option<std::time::Duration> {
        self.peers.get(&peer).filter(|e| e.at.elapsed() < PEER_REPORT_TTL).map(|e| e.oldest)
    }

    /// `peer`'s last report, however old.
    pub fn peer_load(&self, peer: dfs_common::NodeId) -> Option<PeerLoad> {
        self.peers.get(&peer).map(|e| *e)
    }

    /// The current limit and how many writes are in progress, for logs and tests.
    pub fn state(&self) -> (usize, usize) {
        let g = self.st.lock().unwrap();
        (g.limit as usize, g.started.len())
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

    /// Suite T66[secondary] (1030b): the primary forgot a write it had ordered as v2 after 621
    /// more writes, and ordered the client's resend of it as v622. Every decided write must
    /// still be remembered while it is recent, however many writes follow it.
    #[test]
    fn a_primary_remembers_a_recent_write_it_ordered_however_many_follow() {
        let o = WriteOrdering::default();
        let s = o.slot(FileId::new(), 1, 2);
        for id in 1..=3000u128 {
            let v = s.assign_once(id).unwrap();
            s.finish(id, v, &Response::Ok { data: None });
        }
        for id in [1u128, 600, 1500] {
            assert!(matches!(s.assign_once(id), Err(Decision::Ordered { .. })),
                "BUG REPRODUCED: write {} was forgotten and its resend ordered again as a new version", id);
        }
        assert_eq!(s.inner.lock().unwrap().decided_order.len(), 3000, "each write is queued once");
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

    #[test]
    fn the_previous_epochs_head_carries_only_to_the_next_epoch_and_only_while_current() {
        let o = WriteOrdering::default();
        let f = FileId::new();
        let (a, b) = (ChunkId::from_hash([1u8; 32]), ChunkId::from_hash([2u8; 32]));
        o.slot(f, 0, 2).set_anchor(a, 4);
        let _ = o.slot(f, 0, 3);
        assert_eq!(o.previous_epoch_head(f, 0, 3), Some(a));
        assert_eq!(o.previous_epoch_head(f, 0, 4), None, "an epoch this node skipped breaks the chain");
        o.on_folded(f, 0, a, b);
        assert_eq!(o.previous_epoch_head(f, 0, 3), Some(b), "a fold of the carried head follows it");
        o.clear_head(f, 0);
        assert_eq!(o.previous_epoch_head(f, 0, 3), None, "an unordered write here retires the carried head");
    }

    #[test]
    fn a_truncate_to_zero_ends_every_slot_stream_of_that_file_only() {
        let o = WriteOrdering::default();
        let (f, g) = (FileId::new(), FileId::new());
        for (file, idx, n) in [(f, 0, 1), (f, 3, 2), (g, 0, 3)] {
            let s = o.slot(file, idx, 1);
            s.set_anchor(cid(n), 0);
            s.applied(1, 1, Some(cid(n)));
        }
        o.clear_file_heads(f);
        assert_eq!((o.latest_head(f, 0), o.latest_head(f, 3)), (None, None), "the truncated file's heads are gone");
        assert_eq!(o.latest_head(g, 0), Some(cid(3)), "another file's stream is untouched");
    }

    #[tokio::test]
    async fn a_busy_order_fails_the_turn_as_busy_and_a_real_order_for_the_resend_replaces_it() {
        let s = SlotOrder::default();
        s.record_order(1, BUSY, cid(0), 7);
        assert_eq!(s.wait_turn(1, Duration::from_millis(50)).await, Err(TurnError::Busy));
        assert_eq!(s.wait_turn(1, Duration::from_millis(50)).await, Err(TurnError::NoOrder), "busy is consumed, nothing decided");
        // The resend's real order and the first copy's busy order can arrive either way round.
        s.record_order(2, BUSY, cid(0), 7);
        s.record_order(2, 1, cid(4), 7);
        assert_eq!(s.wait_turn(2, Duration::from_millis(50)).await, Ok((1, cid(4))));
        s.applied(2, 1, Some(cid(4)));
        s.record_order(3, 2, cid(5), 7);
        s.record_order(3, BUSY, cid(0), 7);
        assert_eq!(s.wait_turn(3, Duration::from_millis(50)).await, Ok((2, cid(5))), "a late busy doesn't undo a real order");
        s.record_order(4, BUSY, cid(0), 7);
        s.set_anchor(cid(6), 3);
        assert_eq!(s.wait_turn(4, Duration::from_millis(50)).await, Err(TurnError::NoOrder), "an anchor drops stale busy orders");
    }

    #[test]
    fn admission_halves_on_slow_writes_grows_on_fast_ones_and_always_admits_one() {
        let a = Arc::new(Admission::new(Duration::from_millis(20), 8));
        let sec = dfs_common::NodeId::new();
        let permits: Vec<_> = (0..8).map(|_| a.try_admit(0, sec).expect("starts at the max")).collect();
        assert!(a.try_admit(0, sec).is_err(), "the ninth is turned away");
        std::thread::sleep(Duration::from_millis(30));
        drop(permits);
        assert_eq!(a.state(), (4, 0), "slow writes halve the limit once per target");
        std::thread::sleep(Duration::from_millis(25));
        for _ in 0..3 {
            let p = a.try_admit(0, sec).unwrap();
            std::thread::sleep(Duration::from_millis(30));
            drop(p);
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(a.state(), (1, 0), "it never goes below one");
        let p = a.try_admit(0, sec).expect("with nothing in progress a write is always admitted");
        assert!(a.try_admit(0, sec).is_err());
        drop(p);
        for _ in 0..40 {
            drop(a.try_admit(0, sec).unwrap());
        }
        assert!(a.state().0 >= 4, "fast writes grow it back: {:?}", a.state());
        let forced: Vec<_> = (0..a.state().0).map(|_| a.admit(0)).collect();
        assert!(a.try_admit(0, sec).is_err(), "work this node must do counts toward the limit");
        drop(forced);
        let big = a.try_admit(4 << 20, sec).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        let before = a.state().0;
        drop(big);
        assert!(a.state().0 >= before, "a 4 MiB write gets its transfer time on top of the target");
    }

    #[test]
    fn admission_turns_writes_away_while_old_work_piles_up_here_or_on_the_secondary() {
        let a = Arc::new(Admission::new(Duration::from_millis(10), 64));
        let (sec, other) = (dfs_common::NodeId::new(), dfs_common::NodeId::new());
        let stuck = a.admit(0);
        assert!(a.try_admit(0, sec).is_ok(), "young work in progress doesn't block");
        std::thread::sleep(Duration::from_millis(40));
        let why = a.try_admit(0, sec).err().expect("a secondary copy waiting 4 targets blocks new primary writes");
        assert!(why.contains("oldest write in progress"), "{}", why);
        drop(stuck);
        assert!(a.try_admit(0, sec).is_ok());
        // The secondary's report, as its WriteOrder reply carries it.
        let peer = Arc::new(Admission::new(Duration::from_millis(10), 64));
        let held = peer.admit(0);
        std::thread::sleep(Duration::from_millis(40));
        a.note_peer(sec, &peer.report());
        let why = a.try_admit(0, sec).err().expect("a secondary that's behind slows its primary");
        assert!(why.contains("secondary"), "{}", why);
        assert!(a.try_admit(0, other).is_ok(), "a slot with another secondary is unaffected");
        drop(held);
        a.note_peer(sec, &peer.report());
        assert!(a.try_admit(0, sec).is_ok(), "it caught up");
        a.note_peer(other, &[]);
        assert_eq!(a.peer_backlog(other), None, "an older peer's empty reply is ignored");
    }

    #[test]
    fn a_report_says_how_long_work_in_progress_has_finished_nothing() {
        let a = Arc::new(Admission::new(Duration::from_millis(10), 64));
        let peer = dfs_common::NodeId::new();
        a.note_peer(peer, &a.report());
        assert_eq!(a.peer_load(peer).unwrap().stalled, Some(Duration::ZERO), "idle is not stalled");
        let stuck = a.admit(0);
        std::thread::sleep(Duration::from_millis(30));
        a.note_peer(peer, &a.report());
        assert!(a.peer_load(peer).unwrap().stalled.unwrap() >= Duration::from_millis(30), "nothing finished since it started");
        let quick = a.admit(0);
        drop(quick);
        a.note_peer(peer, &a.report());
        assert!(a.peer_load(peer).unwrap().stalled.unwrap() < Duration::from_millis(10), "slow work that finishes is progress");
        drop(stuck);
        let mut old = a.report();
        old.truncate(16);
        a.note_peer(peer, &old);
        assert_eq!(a.peer_load(peer).unwrap().stalled, None, "a 16-byte report from an older peer has no stall field");
    }
}
