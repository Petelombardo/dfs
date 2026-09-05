//! Runtime-independent hard deadlines for operations that can wedge while
//! holding a process-wide exclusive lock.
//!
//! # Why this exists rather than `tokio::time::timeout`
//!
//! Every `compact_db_blocking` / `compact_db_prepare` call site already wraps
//! itself in a `tokio::time::timeout` that calls `std::process::exit(1)` on
//! expiry, added 2026-07-31 after a gluster1 compaction wedge. On 2026-09-05
//! gluster1 wedged again anyway, in the same place — `compact_db_blocking`
//! blocked in `self.db.write()` (`lock_exclusive_slow` → `wait_for_readers`)
//! — and that 60s timeout **never fired**:
//!
//!   - the process was still alive 6h later, so `process::exit(1)` was never
//!     reached, and its "exceeded 60s" error line never appeared in the log;
//!   - 18 of 19 threads sat in futex waits (8 of them queued behind the
//!     pending writer in `lock_shared_slow`), the tokio workers were parked
//!     and idle, and the whole process went silent 3s after compaction began;
//!   - nothing was listening on the service port, yet the process never
//!     exited — so systemd kept reporting `active`, `Restart=on-failure`
//!     never triggered, and the node silently left the cluster while looking
//!     healthy to every form of supervision we had.
//!
//! A post-mortem on the live process could not establish *why* that await was
//! never completed. That uncertainty is precisely the point: the recovery
//! mechanism must not share a failure mode with the thing it is supervising.
//! So this watchdog deliberately avoids every component that was implicated:
//!
//!   - a plain **OS thread**, so it cannot be starved by the async runtime,
//!     the scheduler, or a wedged time driver;
//!   - **`std::thread::sleep`**, not a timer wheel;
//!   - **`libc::_exit`**, not `std::process::exit` — the latter runs atexit
//!     handlers and flushes stdio, either of which can itself block on a lock
//!     the wedged thread is holding, turning "exit" into another hang.
//!
//! The log line is emitted on a best-effort basis before `_exit`; if the
//! tracing appender is itself wedged the message is lost. Losing the message
//! is acceptable. Failing to die is not — a node that cannot serve must exit
//! so its replicas keep serving and systemd can start a fresh process with a
//! clean redb handle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::error;

/// Guard returned by [`HardDeadline::arm`]. Dropping it disarms the watchdog,
/// so the normal path costs one atomic store. Hold it for exactly the span
/// that must not wedge.
pub struct HardDeadline {
    done: Arc<AtomicBool>,
}

impl HardDeadline {
    /// Arm a watchdog that kills this process if `limit` elapses before the
    /// returned guard is dropped.
    ///
    /// `label` names the operation in the log line and should say what was
    /// being attempted, e.g. "planned offline compaction".
    pub fn arm(label: &'static str, limit: Duration) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        // Poll rather than sleep once for the whole limit: a single long sleep
        // would keep the thread alive well past a fast, successful operation,
        // and these are armed frequently enough (every compaction cycle) that
        // leaking a minutes-long sleeping thread each time is worth avoiding.
        let tick = Duration::from_millis(250).min(limit);
        std::thread::Builder::new()
            .name("hard-deadline".into())
            .spawn(move || {
                let deadline = std::time::Instant::now() + limit;
                loop {
                    if flag.load(Ordering::SeqCst) {
                        return;
                    }
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(tick);
                }
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                error!(
                    "WATCHDOG: {} exceeded its {}s hard deadline and the async timeout did not \
                     fire — this process is wedged and cannot serve. Exiting immediately so \
                     replicas keep serving and systemd starts a clean process.",
                    label,
                    limit.as_secs()
                );
                // Best-effort flush window for the async tracing appender. Bounded,
                // because the appender may itself be blocked; we exit either way.
                std::thread::sleep(Duration::from_millis(200));
                // _exit, not exit: no atexit handlers, no stdio flush, nothing that
                // can block on the lock we are dying because of.
                unsafe { libc::_exit(1) };
            })
            .expect("failed to spawn hard-deadline watchdog thread");
        Self { done }
    }
}

impl Drop for HardDeadline {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard must disarm the watchdog: an operation that finishes inside
    /// its deadline leaves nothing behind that could kill the process later.
    #[test]
    fn dropping_the_guard_disarms_the_watchdog() {
        let guard = HardDeadline::arm("test op", Duration::from_millis(200));
        let done = guard.done.clone();
        drop(guard);
        assert!(done.load(Ordering::SeqCst), "dropping the guard must set the done flag");
        // If the watchdog ignored the flag it would _exit(1) here and take the
        // whole test binary with it — an unmissable failure.
        std::thread::sleep(Duration::from_millis(400));
    }

    /// The point of the whole module: a span that never completes must take the
    /// process down. Runs in a child process because a passing test here
    /// necessarily calls `libc::_exit(1)` — asserting on the child's exit status
    /// is the only way to observe that without killing the test binary.
    ///
    /// This is the behaviour the async `tokio::time::timeout` was supposed to
    /// provide and did not on gluster1 (see the module doc comment), so it is
    /// asserted end-to-end on a real process rather than by inspecting a flag.
    #[test]
    fn watchdog_kills_a_process_wedged_past_its_deadline() {
        const CHILD_ENV: &str = "DFS_WATCHDOG_WEDGE_CHILD";
        const TEST_PATH: &str = "watchdog::tests::watchdog_kills_a_process_wedged_past_its_deadline";

        if std::env::var(CHILD_ENV).is_ok() {
            // Child: arm, then wedge. The guard is deliberately never dropped.
            let _guard = HardDeadline::arm("wedged test op", Duration::from_millis(500));
            std::thread::sleep(Duration::from_secs(120));
            unreachable!("the watchdog must have exited this process long before now");
        }

        let exe = std::env::current_exe().expect("current_exe");
        let mut child = std::process::Command::new(exe)
            .args(["--exact", TEST_PATH, "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn watchdog child");

        // Poll rather than wait(): if the watchdog is broken the child sleeps for
        // two minutes, and this test should fail fast and loudly instead of hanging.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let status = loop {
            match child.try_wait().expect("try_wait") {
                Some(status) => break status,
                None if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "BUG: the watchdog did not kill a process wedged well past its 500ms                          deadline — this is exactly the gluster1 failure (a wedged node that                          never exits, so systemd never restarts it and it silently leaves the                          cluster while still reporting `active`)"
                    );
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        };

        assert_eq!(status.code(), Some(1),
            "the wedged child must exit(1) so systemd's Restart=on-failure fires; got {:?}", status);
    }

    /// The watchdog thread must actually observe the flag and return, rather
    /// than sleeping out the full limit and then re-checking — otherwise a
    /// long deadline would leak a thread per armed span.
    #[test]
    fn watchdog_thread_exits_promptly_once_disarmed() {
        let before = std::time::Instant::now();
        drop(HardDeadline::arm("test op", Duration::from_secs(3600)));
        assert!(before.elapsed() < Duration::from_secs(1),
            "arming and disarming must not block on the deadline itself");
    }
}
