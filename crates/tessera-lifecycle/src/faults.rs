//! Deliberate fault injection for the write executor — **never in a default-features build**.
//!
//! ## Why this exists
//!
//! The executor's most load-bearing behaviour only occurs when durability *fails*: a suppression
//! applied despite a disk-full append, a poisoned WAL tripping the not-ready posture, an ack that
//! must not precede its generation swap, a deny that must not queue behind work. None of those can
//! be reached by a test that can only ask the executor to succeed, so the hooks live beside the
//! executor they instrument rather than waiting for the conformance suite's own pause points.
//!
//! ## The compile gate, and exactly what it does and does not buy
//!
//! Everything in this module is behind `feature = "fault-injection"`, reachable by exactly two
//! routes (decision 0071):
//!
//! - **a self dev-dependency** (`tessera-lifecycle = { path = ".", features = ["fault-injection"]
//!   }` in this crate's own `[dev-dependencies]`, and the same shape in `tessera-engine`'s and
//!   `tessera-server`'s) — how every test in the tree gets it; and
//! - **a declared, default-off feature on `tessera-server` and `tessera-cli`** — how the
//!   correctness suite's driver gets a *served binary* that carries the seam pause sites. That
//!   binary is built to **its own target directory**, never `target/release/tessera`, which the
//!   oracle harness builds with default features always:
//!
//!   ```text
//!   cargo build --release -p tessera-cli --features fault-injection --target-dir target/faults
//!   ```
//!
//! **What the gate buys: no default-features build carries this code.** `cargo build` does not
//! build dev-dependencies, and the server/CLI feature is off unless a build asks for it by name —
//! so the release binary every deployment gets cannot carry the switchboard. Measured, not
//! assumed, and asserted from the *resolved feature graph* by `scripts/check-layers.sh` rule 2:
//! **no default-features release build reaches `fault-injection`**. The narrower claim that used
//! to stand here — *no normal dependency edge anywhere enables it* — was retired by decision 0071,
//! because the faults build enables it on normal edges deliberately; what the guard still refuses
//! is that enablement ever becoming anyone's default.
//!
//! **What it does not buy, stated because the obvious reading overclaims:** a `cargo test
//! --workspace` build still unifies the feature across the workspace and
//! into `tessera-server`, exactly as `bench-timing` does. The self dev-dependency is *not* stronger
//! than `bench-timing` on that axis; it is stronger only on the release axis, because
//! `tessera-bench` declares `bench-timing` as a **normal** dependency with `default =
//! ["bench-timing"]`, so a `cargo build --workspace --release` reaches it and cannot reach this.
//!
//! The consequence to respect rather than re-derive: a `#[cfg(feature = "fault-injection")]` block
//! compiles differently under `cargo test -p tessera-lifecycle` than under `cargo test
//! --workspace`. So nothing may let a *default* build's behaviour depend on this feature —
//! `crates/tessera-server/tests/http.rs` already demonstrates the footgun with a
//! `bench-timing` block that silently covers less when the feature is absent. The crates that
//! *declare* the feature (`tessera-server`, `tessera-cli`) depend on it only to add the arming
//! surface and the switchboard's construction; every default-build code path is identical with
//! and without it.
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
//! tests that depend on injection are measuring the harness. Both injection arms are exercised:
//! `an_injected_append_failure_follows_the_real_sequence` and its fsync twin, in `tests/wal.rs`.
//!
//! **Repairability is part of the sequence, not a detail beside it.** A real append failure is
//! terminal and a real sync failure is not (`crate::wal`'s two kinds of write failure), so the two
//! injection arms poison differently and [`FaultSwitchboard::fail_next_fsyncs`] arms a *count* the
//! repair path consumes. Making every injected failure terminal is the safe-looking error here, and
//! it would leave the deny lane's durability retry with no test that could distinguish recovery
//! from exhaustion — both would look like exhaustion, and both would pass.
//!
//! ## Why this module is here and not in `tessera-engine`
//!
//! [`Step`], [`PauseSite`] and [`FaultSwitchboard::pause_point`] describe **the executor**, and
//! `ExecutorHealth` was moved into `tessera-engine` on exactly that argument (the crate that
//! deliberately owns no executor should not own the executor's vocabulary). This module breaks
//! that rule knowingly: the switches ride on [`crate::wal::ExecutorWal`], which is here, and one
//! switchboard a test can arm in a single call beats two half-boards that have to agree about
//! arming and release. Ergonomics won over the boundary rule; the boundary rule is the one that
//! would have to be re-argued to move it, not this note.

use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "fault-injection")]
use std::sync::{atomic::AtomicUsize, Condvar, Mutex};

/// Append and fsync counts for the executor's WAL handle.
///
/// **Not test-only, and deliberately so.** `one_fsync_per_window` is an assertion about this
/// counter, and "how many fsyncs has this partition paid" is ordinary operator telemetry that
/// belongs on the bearer-gated `/control/status` — not a debugging affordance to be compiled out.
/// It is here rather than in `wal.rs` because the counting belongs to the *executor's* handle, not
/// to the durability primitive: `Wal` should stay a file format and a positional CRC rule.
///
/// **Reachable from outside this crate, which is a property to keep rather than assume.** The meter
/// is constructed in `WritePath::start_executor` and *cloned* into the `ExecutorWal`;
/// `ExecutorHealth` keeps the other end and surfaces it as
/// `ExecutorStats::wal_appends`/`wal_fsyncs`. Moving it instead would leave this crate's own tests
/// as the only readers in the tree, and the group-commit assertion defined against a counter it
/// cannot reach.
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

    /// Successful fsyncs since this handle was opened — the quantity group commit is measured in,
    /// and the one the ingest baseline memo's ~3.2 ms floor is a cost per unit of.
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
/// suppression that is not yet in force (lifecycle §4's ack ordering).
#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Append,
    Fsync,
    Swap,
    Ack,
}

/// Where the executor parks: two sites inside the ack contract's `append → fsync → apply → swap
/// → ack` sequence, and three at the write path's publication seams (decision 0071;
/// correctness-suite §10.1, §12.3).
///
/// **Two ack sites, because one cannot discriminate the ordering it exists to protect.** With only
/// [`PauseSite::AfterFsync`], parking proves nothing about the *relative* order of the swap and
/// the ack: both are still ahead of the parked executor, so a build that acked first and swapped
/// second parks in exactly the same place and presents exactly the same engine state. Measured,
/// not reasoned: with a real ack-before-swap planted in the deny commit path, every behavioural
/// assertion in `ack_follows_fsync_then_swap` passed and only the step-log assertion failed.
/// [`PauseSite::BeforeAck`] is what closes that — see its own doc for why its *position inside
/// `ack`* rather than at a call site is the load-bearing part.
///
/// **Three seam sites, because the write path has exactly three commit points a crash test needs
/// and an arbitrary kill essentially never lands on** — instants where bytes exist on disc and
/// nothing durable names them. Every other line of a publication is on one side of a commit
/// point or the other, so a kill there is indistinguishable from a kill at the nearest seam; a
/// further site would add arming surface without adding a reachable state. Each variant's doc
/// names its crash story, which is §12.3's discard rule for that seam.
#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseSite {
    /// After fsync, before the generation swap: **durable, not yet in force**.
    ///
    /// The position lifecycle §8's crash table calls "after fsync, before swap" (at risk: none),
    /// and where a crash test needs a process to have died so that replay reinstates the effect
    /// rather than reallocating it.
    AfterFsync,
    /// After the generation swap, before the receipt is sent: **in force, not yet acknowledged**.
    ///
    /// Armed *inside* `Executor::ack`, not at its call site, and that is the whole point. A pause
    /// point at a call site is a statement about source order — move the `ack(..)` call above the
    /// swap and the pause point stays obediently below it. Inside `ack`, the point **travels with
    /// the ack**: a build that acks before it swaps parks here with the swap still ahead of it, so
    /// a test parked at this site sees the effect *not* in force and fails on engine state alone,
    /// with no reference to the step log. Any rewrite of the executor's ack loop is caught by that
    /// discrimination, because the point moves with the code rather than with the line.
    BeforeAck,
    /// Before a side-manifest commits into the **live** prefix: the publication's files are on
    /// disc, and nothing durable names them yet.
    ///
    /// The first of the three **publication-seam** sites (correctness-suite §10.1, §12.3). The
    /// two above discriminate the *ack contract*; these three park the executor at the instants a
    /// crash test needs, because an arbitrary `SIGKILL` essentially never lands on one. A killed
    /// process parked here leaves exactly the state §12.3's discard table names: the side-manifest
    /// being written, and any segment file it names that no earlier manifest does — orphans a
    /// restart never opens.
    ///
    /// Fires at **every** executor-side side-manifest commit into the live prefix — flush, merge,
    /// coalesce, and the overlay's deny-state write — because all four share one crash story. It
    /// deliberately does **not** fire for the fold's manifest writes: those land in a prefix
    /// nothing references until `CURRENT` names it, so their crash story is
    /// [`PauseSite::BeforeCurrentFlip`]'s (the whole unflipped prefix is discardable), and a site
    /// that fired for both would hand a driver two different discard rules under one name.
    BeforeManifestPublish,
    /// Before the fold's `CURRENT` rename: the folded prefix is complete and synced, and no
    /// durable pointer names it.
    ///
    /// The fold's commit point is a single rename — everything before it is reversible, nothing
    /// after it is — which makes this the seam whose crash story is simplest: a kill while parked
    /// here leaves a whole `v#####` tree `CURRENT` never named, and the startup sweep reclaims it
    /// (compaction §7). No per-file bookkeeping, which is why the correctness suite builds its
    /// crash modifier against this site first.
    BeforeCurrentFlip,
    /// Between a merge's execution on the pool and its publication on the executor: the merged
    /// segment exists, its inputs are untouched, and the executor has not begun to publish.
    ///
    /// The hook `tessera-engine/tests/merge.rs`'s module doc recorded as missing — both for a
    /// crash-mid-merge test (kill while parked: the output segment is an orphan, the inputs still
    /// stand) and for ordering a suppression against the publication that re-derives the deny mask
    /// over the new row space. Sited at the top of the executor's `publish_merge`, before it reads
    /// the live overlay, so a parked executor has committed to nothing.
    BeforeMergePublish,
}

#[cfg(feature = "fault-injection")]
impl PauseSite {
    const COUNT: usize = 5;
    fn index(self) -> usize {
        match self {
            PauseSite::AfterFsync => 0,
            PauseSite::BeforeAck => 1,
            PauseSite::BeforeManifestPublish => 2,
            PauseSite::BeforeCurrentFlip => 3,
            PauseSite::BeforeMergePublish => 4,
        }
    }

    /// The wire name the control plane's arming surface speaks — one vocabulary, defined beside
    /// the sites so the driver's names cannot drift from the switchboard's
    /// (correctness-suite §12.3 writes `kill_at=PauseSite.BEFORE_CURRENT_FLIP`; these are those
    /// names, lowered).
    pub fn name(self) -> &'static str {
        match self {
            PauseSite::AfterFsync => "after_fsync",
            PauseSite::BeforeAck => "before_ack",
            PauseSite::BeforeManifestPublish => "before_manifest_publish",
            PauseSite::BeforeCurrentFlip => "before_current_flip",
            PauseSite::BeforeMergePublish => "before_merge_publish",
        }
    }

    /// The inverse of [`PauseSite::name`]. `None` for an unknown name — the arming surface's 422.
    pub fn from_name(name: &str) -> Option<Self> {
        [
            PauseSite::AfterFsync,
            PauseSite::BeforeAck,
            PauseSite::BeforeManifestPublish,
            PauseSite::BeforeCurrentFlip,
            PauseSite::BeforeMergePublish,
        ]
        .into_iter()
        .find(|site| site.name() == name)
    }
}

/// What the executor does when it reaches an armed pause point.
///
/// **One action, deliberately, and there is no `Abort`.** An in-process "abort" can only be a
/// second `panic!` with a different message, and a panic unwinds: it runs `DeathGuard`, drops the
/// `Job` and closes the `ExecutorWal`. A `SIGKILL` does none of those. Offering one would let a
/// test model a clean shutdown and call it a crash, against a crash table (lifecycle §8) that turns
/// on exactly that difference. A test that needs a real crash builds one — a child process it kills
/// — because no in-process switch can fake it.
#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseAction {
    /// Block until released. The command then completes normally.
    Stall,
    /// Panic on the executor thread. Exercises the drop guard that reports the posture — the one
    /// construction that survives a panic anywhere in the loop body. **Unwinds**; it is not a
    /// crash, and nothing may read it as one.
    Panic,
}

#[cfg(feature = "fault-injection")]
#[derive(Debug, Default, Clone, Copy)]
struct Pause {
    /// `None` when disarmed. Set by `arm_pause`, cleared by `release`.
    action: Option<PauseAction>,
    /// How many times the executor has reached this site. A test waits on *this* rather than
    /// sleeping: "the executor is demonstrably parked" is a condition to observe, not a duration
    /// to guess at.
    arrivals: u64,
    /// Arrivals to let **through** before `action` fires. `0` — [`FaultSwitchboard::arm_pause`]'s
    /// value — means "fire on the first arrival". See [`FaultSwitchboard::arm_pause_after`].
    fire_after: u64,
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
    /// One [`Pause`] per [`PauseSite`], behind **one** mutex and **one** condvar: a test arms one
    /// site at a time, and a single wait set means `release` cannot leave a thread parked at the
    /// other site asleep.
    pause: Mutex<[Pause; PauseSite::COUNT]>,
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

    /// Fail the next `n` syncs — the first `fsync` and then each repair attempt, in order, since
    /// the deny lane's retry consumes from the same count.
    ///
    /// This is the "disk full on a suppress" the durability rules are written for: the record may be
    /// in the page cache, but nothing is durable. **The count is what selects which behaviour is
    /// under test.** One failure exercises the repair succeeding, and the deny is acknowledged
    /// normally; `tessera_engine::DENY_DURABILITY_ATTEMPTS` failures exhaust it, and the deny is
    /// applied in memory with a 500 to its caller (lifecycle §4).
    pub fn fail_next_fsyncs(&self, n: usize) {
        self.fail_fsyncs.store(n, Ordering::SeqCst);
    }

    pub(crate) fn take_append_failure(&self) -> bool {
        consume(&self.fail_appends)
    }

    pub(crate) fn take_fsync_failure(&self) -> bool {
        consume(&self.fail_fsyncs)
    }

    /// Arm one [`PauseSite`]. Arming resets that site's arrival count, so a test that arms twice
    /// counts arrivals for the leg it is on rather than for the whole run.
    pub fn arm_pause(&self, site: PauseSite, action: PauseAction) {
        self.arm_pause_after(site, action, 0);
    }

    /// [`FaultSwitchboard::arm_pause`], but let `fire_after` arrivals through first.
    ///
    /// **Why this exists.** A commit window performs one generation swap and then acks N waiters in
    /// a loop, so a death partway through the loop leaves some waiters acked and some not — the
    /// state `SubmitError::ReceiptLost` exists for. Firing on the *first* arrival cannot produce it:
    /// nobody has been acked yet, so every waiter is lost and the test's subject never occurs. The
    /// count is what makes "partially acked" constructible.
    ///
    /// Arrivals are counted as they are without this, so `await_arrivals` still observes every one —
    /// including the ones let through.
    pub fn arm_pause_after(&self, site: PauseSite, action: PauseAction, fire_after: u64) {
        let mut pause = self.pause.lock().unwrap_or_else(|e| e.into_inner());
        pause[site.index()] = Pause {
            action: Some(action),
            arrivals: 0,
            fire_after,
            released: false,
        };
    }

    /// Release every parked executor and disarm **every** site.
    ///
    /// All sites, not the one the caller happens to be thinking of: `WritePath::drop` calls this to
    /// guarantee teardown cannot deadlock on a fault a test forgot to clear, and a per-site release
    /// would make that guarantee depend on the test's own bookkeeping.
    pub fn release(&self) {
        let mut pause = self.pause.lock().unwrap_or_else(|e| e.into_inner());
        for p in pause.iter_mut() {
            p.action = None;
            p.released = true;
        }
        self.pause_cv.notify_all();
    }

    /// Release and disarm **one** site, leaving the others armed.
    ///
    /// [`FaultSwitchboard::release`] stays the release-all that `WritePath::drop` depends on; this
    /// is strictly narrower and exists for one shape: a test that parks the executor at one site to
    /// assemble a state, and needs a *second* site to stay armed across the release that lets the
    /// executor run into it. Arming the second site after a release-all would race the very
    /// execution the release starts.
    pub fn release_site(&self, site: PauseSite) {
        let mut pause = self.pause.lock().unwrap_or_else(|e| e.into_inner());
        pause[site.index()].action = None;
        pause[site.index()].released = true;
        self.pause_cv.notify_all();
    }

    /// Block until the executor has reached `site` at least `n` times since it was armed.
    ///
    /// The deterministic alternative to sleeping: it observes the executor's own state rather than
    /// betting on how long "the executor has got that far" takes on this run's scheduler. Bounded
    /// so a genuine hang fails the test rather than hanging CI.
    pub fn await_arrivals(&self, site: PauseSite, n: u64, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        let mut pause = self.pause.lock().unwrap_or_else(|e| e.into_inner());
        while pause[site.index()].arrivals < n {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "executor did not reach {site:?} {n} time(s) within {timeout:?} (arrivals: {})",
                pause[site.index()].arrivals
            );
            let (guard, _) = self
                .pause_cv
                .wait_timeout(pause, remaining)
                .unwrap_or_else(|e| e.into_inner());
            pause = guard;
        }
    }

    /// How many times the executor has reached `site` since it was armed. Non-blocking.
    ///
    /// [`FaultSwitchboard::await_arrivals`] is the in-process form and panics on timeout, which is
    /// right for a test and wrong for an HTTP handler; this is the observation the control plane's
    /// arming surface polls instead. A driver that has armed [`PauseAction::Stall`] and reads a
    /// non-zero count here knows a thread is parked at the site — the precondition for a kill.
    pub fn arrivals(&self, site: PauseSite) -> u64 {
        self.pause.lock().unwrap_or_else(|e| e.into_inner())[site.index()].arrivals
    }

    /// The executor's side of one pause site. Returns the action to take, having already blocked
    /// for [`PauseAction::Stall`].
    pub fn pause_point(&self, site: PauseSite) -> Option<PauseAction> {
        let mut pause = self.pause.lock().unwrap_or_else(|e| e.into_inner());
        let action = pause[site.index()].action?;
        pause[site.index()].arrivals += 1;
        self.pause_cv.notify_all();
        // Counted, then let through: `await_arrivals` must see the arrivals a `fire_after` skips,
        // or a test cannot wait on the state it is assembling.
        if pause[site.index()].arrivals <= pause[site.index()].fire_after {
            return None;
        }
        if action == PauseAction::Stall {
            while !pause[site.index()].released {
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
