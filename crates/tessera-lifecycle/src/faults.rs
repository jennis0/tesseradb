//! Deliberate fault injection for the write executor — **test builds only** (Phase 2 stage 2.1,
//! Task 3a).
//!
//! ## Why this exists, and why now rather than at stage 2.4
//!
//! Four of Task 3a's named tests assert behaviour that only occurs when durability *fails*: a
//! suppression applied despite a disk-full append, a poisoned WAL tripping the not-ready posture,
//! an ack that must not precede its generation swap, a deny that must not queue behind work. None
//! of those can be reached by a test that can only ask the executor to succeed. The Phase 1 ledger
//! already carries "deny-op WAL-failure path inspection-only — fault-injectable Wal would test it"
//! as a deferral, and stage 2.4's conformance pause points are three stages away. So the hooks
//! land with the executor they instrument.
//!
//! ## The compile gate, and exactly what it does and does not buy
//!
//! Everything in this module is behind `feature = "fault-injection"`, enabled **only via a self
//! dev-dependency** (`tessera-lifecycle = { path = ".", features = ["fault-injection"] }` in this
//! crate's own `[dev-dependencies]`, and the same in `tessera-engine`'s).
//!
//! **What that buys: `cargo build` does not build dev-dependencies, so nothing `cargo build`
//! produces can carry this code.** Measured, not assumed.
//!
//! **What it does not buy, stated because the first draft of this paragraph claimed otherwise and
//! was wrong:** a `cargo test --workspace` build still unifies the feature across the workspace and
//! into `tessera-server`, exactly as `bench-timing` does. The self dev-dependency is *not* stronger
//! than `bench-timing` on that axis; it is stronger only on the release axis, because
//! `tessera-bench` declares `bench-timing` as a **normal** dependency with `default =
//! ["bench-timing"]`, so a `cargo build --workspace --release` reaches it and cannot reach this.
//!
//! The consequence to respect rather than re-derive: a `#[cfg(feature = "fault-injection")]` block
//! compiles differently under `cargo test -p tessera-server` than under `cargo test --workspace`.
//! So **nothing outside this crate and `tessera-engine`'s own tests may let its behaviour depend on
//! this feature** — `crates/tessera-server/tests/http.rs` already demonstrates the footgun with a
//! `bench-timing` block that silently covers less when the feature is absent.
//! `scripts/check-layers.sh` asserts the part that can be asserted: no *normal* dependency anywhere
//! enables it.
//!
//! ## The fidelity rule
//!
//! **An injected failure must be indistinguishable from a real one, in variant and in order.** A
//! real `Wal` returns [`WalError::Io`] on the failing call and [`WalError::Poisoned`] on every
//! call after it (`wal.rs`'s `append` error arm and `fsync`'s two arms). So an injected failure
//! does the same: `Io(StorageFull)` first, `Poisoned` after. Returning `Poisoned` on the *first*
//! call would diverge on precisely the error the 500 mapping, the operator alarm and the deny
//! apply-anyway branch all switch on — the one call whose variant matters most. The real sequence
//! is pinned independently by `tests/wal.rs`'s `a_real_fsync_failure_poisons_the_handle`, which
//! provokes a genuine `io::Error` rather than an injected one; if these two ever disagree, the
//! tests that depend on injection are measuring the harness.

use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "fault-injection")]
use std::sync::{atomic::AtomicUsize, Condvar, Mutex};

/// Append and fsync counts for the executor's WAL handle.
///
/// **Not test-only, and deliberately so.** Task 7a's `one_fsync_per_window` is an assertion about
/// this counter, and "how many fsyncs has this partition paid" is ordinary operator telemetry that
/// belongs on the bearer-gated `/control/status` — not a debugging affordance to be compiled out.
/// It is here rather than in `wal.rs` because the counting belongs to the *executor's* handle, not
/// to the durability primitive: `Wal` should stay a file format and a positional CRC rule.
#[derive(Debug, Default)]
pub struct WalMeter {
    appends: AtomicU64,
    fsyncs: AtomicU64,
}

impl WalMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Successful appends since this handle was opened. Counts *successes* only: a failed append
    /// wrote an unknown number of bytes and named no record, which is the whole reason the handle
    /// poisons rather than retries.
    pub fn appends(&self) -> u64 {
        self.appends.load(Ordering::Relaxed)
    }

    /// Successful fsyncs since this handle was opened — the quantity Task 7a's group commit is
    /// measured in, and the one the ingest baseline memo's ~3.2 ms floor is a cost per unit of.
    pub fn fsyncs(&self) -> u64 {
        self.fsyncs.load(Ordering::Relaxed)
    }

    pub(crate) fn record_append(&self) {
        self.appends.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_fsync(&self) {
        self.fsyncs.fetch_add(1, Ordering::Relaxed);
    }
}

/// One observable step of the ack contract, recorded in submission order by the switchboard's log.
///
/// The sequence the executor must produce for one command is `Append, Fsync, Swap, Ack`, and the
/// fail-open this exists to catch is an `Ack` before its `Swap` — a client observing 200 for a
/// suppression that is not yet in force (lifecycle §4's ack ordering, plan Task 3a).
#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Append,
    Fsync,
    Swap,
    Ack,
}

/// What the executor does when it reaches an armed pause point.
///
/// The point itself is **between fsync and swap** — the one place the plan needs twice: Task 9's
/// coupled-ack test parks there to prove the receipt is still outstanding while the effect is not
/// yet in force, and Task 8's crash test needs a process that died exactly there to replay rather
/// than reallocate (lifecycle §8's "after fsync, before swap" crash row).
#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseAction {
    /// Block until released. The command then completes normally.
    Stall,
    /// Stop executing without acking, as a killed process would: the responder drops, the caller's
    /// `recv()` fails, and the posture becomes dead. The fsynced record survives for replay.
    Abort,
    /// Panic on the executor thread. Exercises the drop guard that reports the posture — the one
    /// construction that survives a panic anywhere in the loop body.
    Panic,
}

#[cfg(feature = "fault-injection")]
#[derive(Debug, Default)]
struct Pause {
    /// `None` when disarmed. Set by `arm_pause`, cleared by `release`.
    action: Option<PauseAction>,
    /// How many times the executor has reached the point. A test waits on *this* rather than
    /// sleeping: "the executor is demonstrably parked" is a condition to observe, not a duration
    /// to guess at.
    arrivals: u64,
    released: bool,
}

/// The armed faults for one executor, shared between the test thread and the executor thread.
///
/// Every switch is *consumed* as it fires (`fail_appends` counts down), so arming is a statement
/// about the next N operations rather than a mode the harness has to remember to turn off.
#[cfg(feature = "fault-injection")]
#[derive(Debug, Default)]
pub struct FaultSwitchboard {
    fail_appends: AtomicUsize,
    fail_fsyncs: AtomicUsize,
    pause: Mutex<Pause>,
    pause_cv: Condvar,
    log: Mutex<Vec<Step>>,
}

#[cfg(feature = "fault-injection")]
impl FaultSwitchboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fail the next `n` appends with a genuine-looking `io::Error`, then poison — see this
    /// module's fidelity rule.
    pub fn fail_next_appends(&self, n: usize) {
        self.fail_appends.store(n, Ordering::SeqCst);
    }

    /// Fail the next `n` fsyncs. This is the "disk full on a suppress" the brief names: the record
    /// may be in the page cache, but nothing is durable, so the deny must be applied in memory and
    /// the caller must get 500 (lifecycle §4).
    pub fn fail_next_fsyncs(&self, n: usize) {
        self.fail_fsyncs.store(n, Ordering::SeqCst);
    }

    pub(crate) fn take_append_failure(&self) -> bool {
        consume(&self.fail_appends)
    }

    pub(crate) fn take_fsync_failure(&self) -> bool {
        consume(&self.fail_fsyncs)
    }

    /// Arm the between-fsync-and-swap point.
    pub fn arm_pause(&self, action: PauseAction) {
        let mut pause = self.pause.lock().unwrap_or_else(|e| e.into_inner());
        pause.action = Some(action);
        pause.released = false;
    }

    /// Release a stalled executor and disarm the point.
    pub fn release(&self) {
        let mut pause = self.pause.lock().unwrap_or_else(|e| e.into_inner());
        pause.action = None;
        pause.released = true;
        self.pause_cv.notify_all();
    }

    /// Block until the executor has reached the pause point at least `n` times.
    ///
    /// The deterministic alternative to sleeping: it observes the executor's own state rather than
    /// betting on how long "the executor has got that far" takes on this run's scheduler. Bounded
    /// so a genuine hang fails the test rather than hanging CI.
    pub fn await_arrivals(&self, n: u64, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        let mut pause = self.pause.lock().unwrap_or_else(|e| e.into_inner());
        while pause.arrivals < n {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "executor did not reach the pause point {n} time(s) within {timeout:?} \
                 (arrivals: {})",
                pause.arrivals
            );
            let (guard, _) = self
                .pause_cv
                .wait_timeout(pause, remaining)
                .unwrap_or_else(|e| e.into_inner());
            pause = guard;
        }
    }

    /// The executor's side of the pause point. Returns the action to take, having already blocked
    /// for [`PauseAction::Stall`].
    pub fn pause_point(&self) -> Option<PauseAction> {
        let mut pause = self.pause.lock().unwrap_or_else(|e| e.into_inner());
        let action = pause.action?;
        pause.arrivals += 1;
        self.pause_cv.notify_all();
        if action == PauseAction::Stall {
            while !pause.released {
                pause = self.pause_cv.wait(pause).unwrap_or_else(|e| e.into_inner());
            }
        }
        Some(action)
    }

    pub fn record(&self, step: Step) {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(step);
    }

    /// The steps recorded so far, in order.
    pub fn log(&self) -> Vec<Step> {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn clear_log(&self) {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// Decrement-if-positive, so an armed count is consumed exactly once per firing even under a
/// concurrent reader.
#[cfg(feature = "fault-injection")]
fn consume(counter: &AtomicUsize) -> bool {
    counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            if n == 0 {
                None
            } else {
                Some(n - 1)
            }
        })
        .is_ok()
}
