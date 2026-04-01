use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::debug;

/// Lock type for byte-range locks
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockType {
    Shared,    // F_RDLCK - multiple processes can hold shared locks
    Exclusive, // F_WRLCK - only one process can hold exclusive lock
}

/// Represents a single byte-range lock on a file
#[derive(Debug, Clone)]
pub struct FileLock {
    /// Lock owner (FUSE lock_owner, not PID - used for ownership tracking)
    pub owner: u64,
    /// Process ID (for informational purposes only, e.g., getlk responses)
    pub pid: u32,
    /// Lock type (Shared or Exclusive)
    pub lock_type: LockType,
    /// Start offset in file (0 for whole-file lock)
    pub start: u64,
    /// Length of locked region (0 means "to EOF" or whole-file if start=0)
    pub len: u64,
    /// When this lock was acquired
    pub acquired_at: Instant,
}

impl FileLock {
    /// Create a new file lock
    pub fn new(owner: u64, pid: u32, lock_type: LockType, start: u64, len: u64) -> Self {
        Self {
            owner,
            pid,
            lock_type,
            start,
            len,
            acquired_at: Instant::now(),
        }
    }

    /// Check if this lock overlaps with another lock's byte range
    pub fn overlaps(&self, other: &FileLock) -> bool {
        // Calculate end positions
        // len=0 means "to EOF" or whole-file
        let self_end = if self.start == 0 && self.len == 0 {
            u64::MAX // Whole-file lock
        } else if self.len == 0 {
            u64::MAX // To EOF
        } else {
            self.start.saturating_add(self.len)
        };

        let other_end = if other.start == 0 && other.len == 0 {
            u64::MAX // Whole-file lock
        } else if other.len == 0 {
            u64::MAX // To EOF
        } else {
            other.start.saturating_add(other.len)
        };

        // Ranges overlap if: start1 < end2 AND start2 < end1
        self.start < other_end && other.start < self_end
    }

    /// Check if this lock conflicts with another lock
    /// Conflicts occur when:
    /// 1. Locks are from different owners (same owner never conflicts - POSIX semantics)
    /// 2. Locks overlap in byte range
    /// 3. At least one lock is exclusive
    pub fn conflicts_with(&self, other: &FileLock) -> bool {
        // Same owner never conflicts (POSIX semantics)
        if self.owner == other.owner {
            return false;
        }

        // Must overlap in byte range
        if !self.overlaps(other) {
            return false;
        }

        // At least one must be exclusive for conflict
        self.lock_type == LockType::Exclusive || other.lock_type == LockType::Exclusive
    }
}

/// Manages byte-range locks for all files
pub struct LockManager {
    /// Locks per inode: inode -> Vec<FileLock>
    /// Using Vec since SQLite primarily uses whole-file locks (typically 1-5 locks per file)
    locks: Arc<Mutex<HashMap<u64, Vec<FileLock>>>>,
}

impl LockManager {
    /// Create a new lock manager
    pub fn new() -> Self {
        Self {
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Try to acquire a lock (non-blocking)
    /// Returns Ok(()) if lock acquired, Err with conflicting lock if it would block
    pub async fn try_lock(
        &self,
        ino: u64,
        owner: u64,
        pid: u32,
        lock_type: LockType,
        start: u64,
        len: u64,
    ) -> Result<()> {
        let mut locks = self.locks.lock().await;
        let file_locks = locks.entry(ino).or_insert_with(Vec::new);

        let new_lock = FileLock::new(owner, pid, lock_type, start, len);

        // Check for conflicts with locks from OTHER owners
        for existing in file_locks.iter() {
            if new_lock.conflicts_with(existing) {
                debug!(
                    "Lock conflict: owner={} pid={} {:?} [{}, {}] conflicts with owner={} pid={} {:?} [{}, {}]",
                    owner,
                    pid,
                    lock_type,
                    start,
                    len,
                    existing.owner,
                    existing.pid,
                    existing.lock_type,
                    existing.start,
                    existing.len
                );
                return Err(anyhow!("Lock would block"));
            }
        }

        // POSIX semantics: Remove overlapping locks from SAME owner
        // This enables lock upgrades (shared → exclusive) and downgrades (exclusive → shared)
        file_locks.retain(|lock| !(lock.owner == owner && lock.overlaps(&new_lock)));

        // Add new lock
        file_locks.push(new_lock);

        debug!(
            "Lock acquired: ino={} owner={} pid={} {:?} [{}, {}]",
            ino, owner, pid, lock_type, start, len
        );

        Ok(())
    }

    /// Acquire a lock with blocking (exponential backoff)
    /// Retries with exponential backoff: 1ms → 2ms → 4ms → ... → 100ms
    pub async fn lock_wait(
        &self,
        ino: u64,
        owner: u64,
        pid: u32,
        lock_type: LockType,
        start: u64,
        len: u64,
    ) -> Result<()> {
        let mut backoff_ms = 1u64;
        let max_backoff_ms = 100u64;

        loop {
            match self.try_lock(ino, owner, pid, lock_type, start, len).await {
                Ok(()) => return Ok(()),
                Err(_) => {
                    // Wait with exponential backoff
                    tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(max_backoff_ms);
                }
            }
        }
    }

    /// Unlock a specific byte range
    /// Removes locks from the specified owner that overlap with the unlock range
    pub async fn unlock(&self, ino: u64, owner: u64, start: u64, len: u64) -> Result<()> {
        let mut locks = self.locks.lock().await;

        if let Some(file_locks) = locks.get_mut(&ino) {
            let unlock_range = FileLock::new(owner, 0, LockType::Shared, start, len);

            // Remove locks from this owner that overlap with unlock range
            let before_count = file_locks.len();
            file_locks.retain(|lock| !(lock.owner == owner && lock.overlaps(&unlock_range)));
            let after_count = file_locks.len();

            debug!(
                "Unlock: ino={} owner={} [{}, {}] - removed {} lock(s)",
                ino,
                owner,
                start,
                len,
                before_count - after_count
            );

            // Clean up empty entries
            if file_locks.is_empty() {
                locks.remove(&ino);
            }
        }

        Ok(())
    }

    /// Get a lock that would conflict with the specified lock request
    /// Returns Some(conflicting_lock) if there would be a conflict, None otherwise
    /// Used for F_GETLK queries
    pub async fn get_conflict(
        &self,
        ino: u64,
        owner: u64,
        pid: u32,
        lock_type: LockType,
        start: u64,
        len: u64,
    ) -> Option<FileLock> {
        let locks = self.locks.lock().await;

        if let Some(file_locks) = locks.get(&ino) {
            let test_lock = FileLock::new(owner, pid, lock_type, start, len);

            for existing in file_locks.iter() {
                if test_lock.conflicts_with(existing) {
                    return Some(existing.clone());
                }
            }
        }

        None
    }

    /// Release all locks held by an owner on a specific inode
    /// Called when a file is closed (FUSE release callback)
    pub async fn release_all(&self, ino: u64, owner: u64) -> Result<()> {
        let mut locks = self.locks.lock().await;

        if let Some(file_locks) = locks.get_mut(&ino) {
            let before_count = file_locks.len();
            file_locks.retain(|lock| lock.owner != owner);
            let after_count = file_locks.len();

            debug!(
                "Release all: ino={} owner={} - removed {} lock(s)",
                ino,
                owner,
                before_count - after_count
            );

            // Clean up empty entries
            if file_locks.is_empty() {
                locks.remove(&ino);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_whole_file_lock_overlaps() {
        let lock1 = FileLock::new(100, 100, LockType::Shared, 0, 0);
        let lock2 = FileLock::new(200, 200, LockType::Exclusive, 5000, 1000);
        assert!(lock1.overlaps(&lock2)); // Whole-file overlaps everything
        assert!(lock2.overlaps(&lock1)); // Should be symmetric
    }

    #[test]
    fn test_byte_range_overlap() {
        let lock1 = FileLock::new(100, 100, LockType::Shared, 1000, 500);
        let lock2 = FileLock::new(200, 200, LockType::Exclusive, 1200, 500);
        assert!(lock1.overlaps(&lock2)); // [1000, 1500) overlaps [1200, 1700)

        let lock3 = FileLock::new(300, 300, LockType::Shared, 2000, 500);
        assert!(!lock1.overlaps(&lock3)); // [1000, 1500) doesn't overlap [2000, 2500)
    }

    #[test]
    fn test_to_eof_lock_overlaps() {
        let lock1 = FileLock::new(100, 100, LockType::Shared, 1000, 0); // From 1000 to EOF
        let lock2 = FileLock::new(200, 200, LockType::Exclusive, 5000, 1000);
        assert!(lock1.overlaps(&lock2)); // To-EOF overlaps anything after start

        let lock3 = FileLock::new(300, 300, LockType::Shared, 500, 400); // [500, 900)
        assert!(!lock1.overlaps(&lock3)); // Doesn't overlap with range before start
    }

    #[test]
    fn test_shared_locks_dont_conflict() {
        let lock1 = FileLock::new(100, 100, LockType::Shared, 0, 0);
        let lock2 = FileLock::new(200, 200, LockType::Shared, 0, 0);
        assert!(!lock1.conflicts_with(&lock2)); // Two shared locks don't conflict
    }

    #[test]
    fn test_exclusive_blocks_shared() {
        let lock1 = FileLock::new(100, 100, LockType::Exclusive, 0, 1000);
        let lock2 = FileLock::new(200, 200, LockType::Shared, 500, 500);
        assert!(lock1.conflicts_with(&lock2)); // Exclusive blocks shared
        assert!(lock2.conflicts_with(&lock1)); // Should be symmetric
    }

    #[test]
    fn test_exclusive_blocks_exclusive() {
        let lock1 = FileLock::new(100, 100, LockType::Exclusive, 0, 1000);
        let lock2 = FileLock::new(200, 200, LockType::Exclusive, 500, 500);
        assert!(lock1.conflicts_with(&lock2)); // Exclusive blocks exclusive
    }

    #[test]
    fn test_same_owner_no_conflict() {
        let lock1 = FileLock::new(100, 100, LockType::Exclusive, 0, 1000);
        let lock2 = FileLock::new(100, 100, LockType::Exclusive, 500, 500);
        assert!(!lock1.conflicts_with(&lock2)); // Same owner never conflicts (POSIX)
    }

    #[test]
    fn test_non_overlapping_no_conflict() {
        let lock1 = FileLock::new(100, 100, LockType::Exclusive, 0, 1000);
        let lock2 = FileLock::new(200, 200, LockType::Exclusive, 2000, 1000);
        assert!(!lock1.conflicts_with(&lock2)); // Non-overlapping ranges don't conflict
    }

    #[tokio::test]
    async fn test_try_lock_success() {
        let manager = LockManager::new();
        let result = manager.try_lock(1, 100, 100, LockType::Shared, 0, 0).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_try_lock_conflict() {
        let manager = LockManager::new();

        // First lock succeeds
        manager
            .try_lock(1, 100, 100, LockType::Exclusive, 0, 0)
            .await
            .unwrap();

        // Second lock from different owner conflicts
        let result = manager.try_lock(1, 200, 200, LockType::Shared, 0, 0).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_multiple_shared_locks() {
        let manager = LockManager::new();

        // Multiple shared locks should succeed
        manager
            .try_lock(1, 100, 100, LockType::Shared, 0, 0)
            .await
            .unwrap();
        manager
            .try_lock(1, 200, 200, LockType::Shared, 0, 0)
            .await
            .unwrap();
        manager
            .try_lock(1, 300, 300, LockType::Shared, 0, 0)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_same_owner_lock_replacement() {
        let manager = LockManager::new();

        // Owner 100 acquires shared lock
        manager
            .try_lock(1, 100, 100, LockType::Shared, 0, 1000)
            .await
            .unwrap();

        // Same owner acquires exclusive lock on overlapping range
        // This should replace the old lock (POSIX semantics)
        manager
            .try_lock(1, 100, 100, LockType::Exclusive, 500, 1000)
            .await
            .unwrap();

        // Verify old lock was removed by checking another owner can't get conflicting lock
        let result = manager.try_lock(1, 200, 200, LockType::Shared, 100, 100).await;
        // This should succeed because the old shared lock [0, 1000) was replaced
        // and the new exclusive lock [500, 1000) doesn't overlap [100, 200)
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_unlock() {
        let manager = LockManager::new();

        // Acquire lock
        manager
            .try_lock(1, 100, 100, LockType::Exclusive, 0, 0)
            .await
            .unwrap();

        // Unlock
        manager.unlock(1, 100, 0, 0).await.unwrap();

        // Another owner should now be able to acquire lock
        let result = manager.try_lock(1, 200, 200, LockType::Exclusive, 0, 0).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_release_all() {
        let manager = LockManager::new();

        // Owner 100 acquires multiple locks
        manager
            .try_lock(1, 100, 100, LockType::Shared, 0, 1000)
            .await
            .unwrap();
        manager
            .try_lock(1, 100, 100, LockType::Shared, 2000, 1000)
            .await
            .unwrap();

        // Release all locks for owner 100
        manager.release_all(1, 100).await.unwrap();

        // Another owner should now be able to acquire exclusive lock
        let result = manager.try_lock(1, 200, 200, LockType::Exclusive, 0, 0).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_get_conflict() {
        let manager = LockManager::new();

        // Owner 100 holds exclusive lock
        manager
            .try_lock(1, 100, 100, LockType::Exclusive, 0, 0)
            .await
            .unwrap();

        // Check if owner 200 would conflict
        let conflict = manager.get_conflict(1, 200, 200, LockType::Shared, 0, 0).await;
        assert!(conflict.is_some());
        assert_eq!(conflict.unwrap().pid, 100);

        // Check if owner 100 itself would conflict (should not - same owner)
        let no_conflict = manager.get_conflict(1, 100, 100, LockType::Shared, 0, 0).await;
        assert!(no_conflict.is_none());
    }
}
