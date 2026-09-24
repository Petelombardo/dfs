//! Per-FUSE-read trace: records every network round-trip and wait a single guest
//! read makes, and reports the read at WARN when it is slow or still pending.
//!
//! Why (2026-09-24, VM-108 on server4): the guest hung on boot with errors on sdb
//! while the client log held no ERROR, no EIO and no read activity at all for that
//! disk — reads are only logged at debug, so a read stuck for 20s against a stalled
//! leader was indistinguishable from a read that never happened. Server-side
//! NETTIMING later showed requests from server4 held for up to 26.5s on gluster1,
//! but nothing on the client could say which guest reads had waited on them.
//!
//! Every read runs inside `run`, which scopes a `ReadTrace` as a tokio task-local.
//! Network helpers call `record` after each round-trip; it is a no-op outside a
//! traced read (writes, flushes and background prefetch are never charged to a
//! read). Tasks the read spawns and then awaits must be wrapped in `in_current` so
//! their round-trips are charged too — task-locals do not follow `tokio::spawn`.
//!
//! Reported:
//!  - `SLOW READ`    once, on completion, when the read took >= SLOW_READ.
//!  - `READ PENDING` every PENDING_REPORT_EVERY while the read has not completed,
//!    so a read that never returns (the guest-hang case) still leaves a record.
//!  - `READ TRACE`   at debug, for every other read that made at least one network
//!    step — lets a repro prove WHICH file's reads actually reached the servers
//!    (a cache or a second file's traffic can otherwise make a test pass without
//!    touching the path under test). Never built at info.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

pub const SLOW_READ: Duration = Duration::from_secs(1);
pub const PENDING_REPORT_EVERY: Duration = Duration::from_secs(5);
/// Bound on recorded steps per read — a read that retries a dead node in a loop
/// must not grow its trace without limit. Overflow is counted, not stored.
const MAX_STEPS: usize = 32;

tokio::task_local! {
    static CURRENT: Arc<ReadTrace>;
}

struct Step {
    /// Offset from the start of the read at which the step finished.
    at: Duration,
    took: Duration,
    what: &'static str,
    addr: Option<SocketAddr>,
    detail: String,
}

pub struct ReadTrace {
    start: Instant,
    label: OnceLock<String>,
    steps: Mutex<(Vec<Step>, usize)>,
    pending_reports: std::sync::atomic::AtomicUsize,
}

impl ReadTrace {
    fn new() -> Self {
        Self { start: Instant::now(), label: OnceLock::new(), steps: Mutex::new((Vec::new(), 0)), pending_reports: Default::default() }
    }

    fn push(&self, step: Step) {
        let mut steps = self.steps.lock().unwrap();
        if steps.0.len() < MAX_STEPS {
            steps.0.push(step);
        } else {
            steps.1 += 1;
        }
    }

    fn has_steps(&self) -> bool {
        !self.steps.lock().unwrap().0.is_empty()
    }

    fn render(&self) -> String {
        let steps = self.steps.lock().unwrap();
        if steps.0.is_empty() {
            return "no network steps recorded".to_string();
        }
        let mut out = String::new();
        for (i, s) in steps.0.iter().enumerate() {
            if i > 0 {
                out.push_str(" | ");
            }
            out.push_str(&format!("+{}ms {} ", s.at.as_millis(), s.what));
            if let Some(addr) = s.addr {
                out.push_str(&format!("@{} ", addr));
            }
            out.push_str(&format!("{}ms {}", s.took.as_millis(), s.detail));
        }
        if steps.1 > 0 {
            out.push_str(&format!(" | (+{} more steps not recorded)", steps.1));
        }
        out
    }
}

fn current() -> Option<Arc<ReadTrace>> {
    CURRENT.try_with(|t| t.clone()).ok()
}

/// True inside a traced read. Lets a caller skip building step details (or
/// taking timestamps) on the untraced write/background paths.
pub fn active() -> bool {
    CURRENT.try_with(|_| ()).is_ok()
}

/// Name the read (normally the file path) for the report line.
pub fn set_label(label: &str) {
    if let Some(t) = current() {
        let _ = t.label.set(label.to_string());
    }
}

/// Record a step that started at `started` and has just finished. `detail` is
/// only evaluated inside a traced read.
pub fn record(what: &'static str, addr: Option<SocketAddr>, started: Instant, detail: impl FnOnce() -> String) {
    if let Some(t) = current() {
        let now = Instant::now();
        t.push(Step {
            at: now.duration_since(t.start),
            took: now.duration_since(started),
            what,
            addr,
            detail: detail(),
        });
    }
}

/// Records a step when dropped, including when the future holding it is
/// cancelled — a caller-side `tokio::time::timeout` drops the request future
/// before any code after its `.await` runs, which is exactly the stuck step a
/// slow-read report exists to show. Call `finish` on the normal path.
pub struct StepGuard {
    what: &'static str,
    addr: Option<SocketAddr>,
    started: Instant,
    detail: Option<String>,
    active: bool,
}

impl StepGuard {
    pub fn finish(mut self, detail: impl FnOnce() -> String) {
        if self.active {
            self.detail = Some(detail());
        }
    }
}

impl Drop for StepGuard {
    fn drop(&mut self) {
        if self.active {
            let detail = self.detail.take()
                .unwrap_or_else(|| "CANCELLED (caller gave up waiting)".to_string());
            record(self.what, self.addr, self.started, || detail);
        }
    }
}

/// Start a step; see `StepGuard`. Inert (no allocation) outside a traced read.
pub fn step(what: &'static str, addr: Option<SocketAddr>) -> StepGuard {
    StepGuard { what, addr, started: Instant::now(), detail: None, active: active() }
}

/// Run `fut` inside the current read's trace, if there is one. Wrap a future
/// with this before `tokio::spawn`-ing it when the read awaits the result.
pub fn in_current<F: Future>(fut: F) -> impl Future<Output = F::Output> {
    let trace = current();
    async move {
        match trace {
            Some(t) => CURRENT.scope(t, fut).await,
            None => fut.await,
        }
    }
}

/// `tokio::spawn` for work the current read will await — see `in_current`.
/// Background work the read does not wait on (read-ahead, swarm prefetch) must use
/// a plain `tokio::spawn` so it is not charged to whichever read happened to start it.
pub fn spawn_in_current<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(in_current(fut))
}

/// Run one FUSE read under a fresh trace, reporting it while pending and on
/// completion if slow.
pub async fn run<F: Future<Output = ()>>(ino: u64, offset: i64, size: u32, fut: F) {
    run_traced(Arc::new(ReadTrace::new()), ino, offset, size, fut).await
}

async fn run_traced<F: Future<Output = ()>>(trace: Arc<ReadTrace>, ino: u64, offset: i64, size: u32, fut: F) {
    let fut = CURRENT.scope(trace.clone(), fut);
    tokio::pin!(fut);
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::from_std(trace.start) + PENDING_REPORT_EVERY,
        PENDING_REPORT_EVERY,
    );
    loop {
        tokio::select! {
            _ = &mut fut => break,
            _ = ticker.tick() => {
                trace.pending_reports.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                warn!("READ PENDING ino={} offset={} size={} path={} pending_for={}ms steps: {}",
                    ino, offset, size, label(&trace), trace.start.elapsed().as_millis(), trace.render());
            }
        }
    }
    let took = trace.start.elapsed();
    if took >= SLOW_READ {
        warn!("SLOW READ ino={} offset={} size={} path={} took={}ms steps: {}",
            ino, offset, size, label(&trace), took.as_millis(), trace.render());
    } else if tracing::enabled!(tracing::Level::DEBUG) && trace.has_steps() {
        debug!("READ TRACE ino={} offset={} size={} path={} took={}ms steps: {}",
            ino, offset, size, label(&trace), took.as_millis(), trace.render());
    }
}

fn label(t: &ReadTrace) -> &str {
    t.label.get().map(String::as_str).unwrap_or("?")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn record_is_a_no_op_outside_a_traced_read() {
        assert!(!active());
        record("ReadChunk", None, Instant::now(), || panic!("detail must not be built outside a trace"));
    }

    #[tokio::test]
    async fn spawned_work_is_charged_only_when_wrapped() {
        let trace = Arc::new(ReadTrace::new());
        CURRENT.scope(trace.clone(), async {
            record("direct", None, Instant::now(), || "a".into());
            tokio::spawn(in_current(async { record("wrapped", None, Instant::now(), || "b".into()) })).await.unwrap();
            tokio::spawn(async { record("unwrapped", None, Instant::now(), || "c".into()) }).await.unwrap();
        }).await;
        let rendered = trace.render();
        assert!(rendered.contains("direct") && rendered.contains("wrapped"), "{}", rendered);
        assert!(!rendered.contains("unwrapped"), "a bare tokio::spawn must not inherit the trace: {}", rendered);
    }

    // Real clock, not start_paused: steps are timed with std::time::Instant,
    // which tokio's paused clock does not advance.
    #[tokio::test]
    async fn a_request_cancelled_by_a_caller_timeout_is_still_recorded() {
        // The shape read_chunk_range_from_server has: timeout(1s, send_request(..)).
        // Pre-fix, the stuck attempt vanished from the report and only the
        // successful fallback showed.
        let trace = Arc::new(ReadTrace::new());
        CURRENT.scope(trace.clone(), async {
            let stuck = async {
                let g = step("ReadChunkRange", None);
                std::future::pending::<()>().await;
                g.finish(|| "ok".into());
            };
            let _ = tokio::time::timeout(Duration::from_millis(50), stuck).await;
            let g = step("ReadChunkRange", None);
            g.finish(|| "ok".into());
        }).await;
        let rendered = trace.render();
        let (cancelled, ok) = rendered.split_once(" | ").expect("both attempts recorded");
        assert!(cancelled.contains("CANCELLED"), "the timed-out attempt must be in the report: {}", rendered);
        let took: u64 = cancelled.split_whitespace().nth(2).unwrap().trim_end_matches("ms").parse().unwrap();
        assert!(took >= 50, "the cancelled step must carry its time spent waiting: {}", rendered);
        assert!(ok.ends_with("ok"), "{}", rendered);
    }

    #[tokio::test]
    async fn steps_are_capped() {
        let trace = Arc::new(ReadTrace::new());
        CURRENT.scope(trace.clone(), async {
            for _ in 0..(MAX_STEPS + 5) {
                record("ReadChunk", None, Instant::now(), String::new);
            }
        }).await;
        assert!(trace.render().contains("(+5 more steps not recorded)"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_that_never_finishes_is_reported_every_interval_while_pending() {
        // Paused clock: the 12s hang costs no wall time. The read never completes
        // and is cut off by the outer timeout — exactly the guest-hang shape, where
        // a completion-only log would leave nothing behind.
        let trace = Arc::new(ReadTrace::new());
        let hung = std::future::pending::<()>();
        let r = tokio::time::timeout(Duration::from_secs(12), run_traced(trace.clone(), 1, 0, 4096, hung)).await;
        assert!(r.is_err(), "the inner read never completes");
        assert_eq!(trace.pending_reports.load(std::sync::atomic::Ordering::Relaxed), 2, "reported at 5s and 10s");
    }

    #[tokio::test(start_paused = true)]
    async fn a_fast_read_is_never_reported_pending() {
        let trace = Arc::new(ReadTrace::new());
        run_traced(trace.clone(), 1, 0, 4096, tokio::time::sleep(Duration::from_millis(200))).await;
        assert_eq!(trace.pending_reports.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
}
