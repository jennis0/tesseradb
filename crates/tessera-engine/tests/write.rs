//! The write path at engine level — the acceptance path, asserted against `Engine` directly rather
//! than through HTTP.
//!
//! **Why an engine-level file at all.** The properties here are orderings, not HTTP behaviours: a
//! success ack following its generation swap, a deny not queued behind work. Reaching the executor
//! through the HTTP surface to observe them would make every one of them depend on the server's
//! own scheduling as well as the executor's.
//!
//! ## What these cases are and are not
//!
//! They assert the **ack contract** (lifecycle §4) and the **deny priority lane** (§1.3): that a
//! success ack follows its generation swap, that a deny is never queued behind work, that a deny
//! whose WAL append failed is applied anyway, and that a poisoned WAL trips the not-ready posture
//! rather than being retried. Each needs a fault the engine cannot be *asked* for, which is why
//! `tessera-lifecycle`'s `faults` module exists at all — see its module doc, and in particular its
//! fidelity rule: an injected failure must be
//! indistinguishable from a real one, which `tessera-lifecycle/tests/wal.rs` pins independently.
//!
//! **No test here sleeps.** Every wait is on a condition the executor itself publishes — the
//! switchboard's per-site arrival counter, the submitted-command counters — so a failure means the
//! property broke, never that a runner was slow.
//!
//! **One assertion form here is not a witness, and is marked as such wherever it appears.**
//! `!handle.is_finished()` states that another thread has *not* made progress, and no such
//! statement can be established without waiting: it is true the instant it is checked whether or
//! not the property holds. Those assertions are kept for the message they carry when they do fire,
//! never relied on. Where an ordering has to be *proved*, it is proved by engine state observed
//! while the executor is demonstrably parked, or by the switchboard's step log — see
//! [`ack_follows_fsync_then_swap`], which was measurably passing over a planted fail-open before
//! this distinction was drawn.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tempfile::TempDir;

use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{
    AcceptError, Engine, ExecutorPosture, DENY_DURABILITY_ATTEMPTS, DENY_WINDOW_MAX_ENTRIES,
};
use tessera_lifecycle::command::{SubmitError, UnallocatedRow};
use tessera_lifecycle::faults::{FaultSwitchboard, PauseAction, PauseSite, Step};
use tessera_lifecycle::ChangeOp;
use tessera_types::EntityId;

use common::{build_fixture, full_coverage_credential, open_engine, source_id_key, tick, N_ITEMS};

const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// A fixture engine with its executor running, and the switchboard armed on it.
fn engine_with_faults(tmp: &TempDir, queue_bound: usize) -> (Engine, Arc<FaultSwitchboard>) {
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let mut engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    let faults = Arc::new(FaultSwitchboard::new());
    engine
        .start_write_executor_with_faults(queue_bound, Arc::clone(&faults))
        .expect("the executor starts once");
    (engine, faults)
}

fn row(key: &str) -> UnallocatedRow {
    UnallocatedRow {
        external_id: Some(key.as_bytes().to_vec()),
        view: "s0".to_string(),
        join: None,
        descriptors: Vec::new(),
        x: 0.0,
        y: 0.0,
        scalars: Vec::new(),
        terms: Vec::new(),
        scoped: Vec::new(),
    }
}

/// How many items a full-coverage viewport can see. Buffered (ingested) items have no row geometry
/// at all, there being no flush (⊘), so this counts the built corpus only — which is exactly what
/// a suppression must move.
fn visible(engine: &Engine) -> u64 {
    let session = engine
        .authorise(&full_coverage_credential())
        .expect("authorise");
    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], (N_ITEMS + 10) as usize),
        )
        .expect("viewport");
    out.tiles[0].visible
}

/// Block until `cond` holds, polling a value the executor publishes rather than sleeping on a guess
/// about how long it takes to get there. Bounded, so a property that never arrives fails the test
/// instead of hanging CI.
fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + WAIT;
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting: {what}"
        );
        std::thread::yield_now();
    }
}

fn entity_of(engine: &Engine, source_id: u64) -> EntityId {
    engine
        .resolve_external_id(&source_id_key(source_id))
        .expect("resolve")
        .expect("fixture item resolves")
}

// =================================================================================================
// The migrated case, and I9
// =================================================================================================

/// An entity ingested after the build has no locator slot and no extent entry — the
/// live map must answer first, or `external_id_of` would wrongly report "this item has no external
/// id" for one that does.
///
/// **The test asserts the id it gets *back*, and that is the point.** A caller cannot choose an
/// entity id: `Command::Ingest` carries `UnallocatedRow`, which has no id field, because the
/// executor must be free to assign a whole window's ids in one signature-sorted run. Whatever
/// **The extent check guards the engine's own boundary, not one HTTP handler** (§6).
///
/// This is the whole reason it moved. `Engine::accept_ingest` has more than one caller — the
/// server, every bench arm, and these tests — and the invariant it establishes is *every buffered
/// row has a cell*, which is a fact about the buffer. Guarding only the HTTP path left every other
/// caller writing points the quantiser silently clamps onto the edge of the grid, with a clamped
/// boundary point indistinguishable from one that belongs there.
///
/// It is also what lets `plan_flush` quantise the buffer without re-checking: the state a second
/// check would detect cannot arise, and a second copy of the predicate is how the two would come to
/// disagree.
#[test]
fn an_out_of_extent_row_is_refused_at_the_engine_boundary_with_no_id_burned() {
    let tmp = TempDir::new().unwrap();
    let (engine, _faults) = engine_with_faults(&tmp, 8);

    let before = engine.allocator_high_water();
    let mut adrift = row("adrift");
    adrift.x = 5000.0; // the fixture's extent is 0..1000 on both axes

    let err = engine
        .accept_ingest(vec![adrift], "batch-adrift".to_string(), [0u8; 32])
        .expect_err("a coordinate with no cell is refused");
    assert!(matches!(
        err,
        tessera_engine::AcceptError::OutsideExtent { index: 0, .. }
    ));
    assert_eq!(
        engine.allocator_high_water(),
        before,
        "refused before the submit: no entity id is burned, so I9 loses nothing to a bad row"
    );

    // NaN has no cell either, and `as u32` would saturate it to zero rather than erroring.
    let mut nan = row("nan");
    nan.y = f64::NAN;
    assert!(engine
        .accept_ingest(vec![nan], "batch-nan".to_string(), [0u8; 32])
        .is_err());

    // A point exactly at the maximum occupies the top of the grid and belongs there.
    let mut edge = row("edge");
    edge.x = 1000.0;
    edge.y = 1000.0;
    engine
        .accept_ingest(vec![edge], "batch-edge".to_string(), [0u8; 32])
        .expect("the boundary is inside");
}

/// assigns the id, the live map answers for it before the build's locator does.
#[test]
fn the_executor_assigns_the_ids_and_the_live_map_answers_for_them() {
    let tmp = TempDir::new().unwrap();
    let (engine, _faults) = engine_with_faults(&tmp, 8);

    let before = engine.allocator_high_water();
    let ids = engine
        .accept_ingest(
            vec![row("post-build-key")],
            "batch-1".to_string(),
            [0u8; 32],
        )
        .expect("ingest is accepted");

    assert_eq!(ids.len(), 1);
    let assigned = ids[0];
    assert_eq!(
        assigned,
        EntityId::new(before),
        "the executor assigns from the I9 high-water; the caller supplies nothing"
    );
    assert_eq!(
        engine.allocator_high_water(),
        before + 1,
        "and the high-water advances by exactly the batch"
    );

    let resolved = engine.resolve_external_id(b"post-build-key").unwrap();
    assert_eq!(resolved, Some(assigned));
    let external = engine.external_id_of(assigned).unwrap();
    assert_eq!(external.as_deref(), Some(&b"post-build-key"[..]));
}

/// I9 across commands: ids are strictly monotone and assigned in **signature-sorted** order within
/// each command (design §11.1).
///
/// Pinned at command scope here so that the window-scope test below is measuring a change of
/// *scope* rather than the arrival of sorting at all.
#[test]
fn per_command_assignment_is_signature_sorted_and_monotone() {
    let tmp = TempDir::new().unwrap();
    let (engine, _faults) = engine_with_faults(&tmp, 8);

    // Two distinct signatures, interleaved in submission order: sorted assignment must group them
    // into contiguous id runs, which is what makes their postings compress as runs.
    let sig_low = engine.resolve_terms(&[b"0".to_vec()]);
    let sig_high = engine.resolve_terms(&[b"1".to_vec()]);
    assert_ne!(
        sig_low, sig_high,
        "the fixture must offer two real signatures"
    );

    let mut rows = Vec::new();
    for i in 0..6u64 {
        let mut r = row(&format!("sig-{i}"));
        r.terms = if i % 2 == 0 {
            sig_high.clone()
        } else {
            sig_low.clone()
        };
        rows.push(r);
    }

    let ids = engine
        .accept_ingest(rows, "sorted".to_string(), [1u8; 32])
        .expect("ingest is accepted");

    // Ids come back in the caller's row order, so the odd rows (the lower signature) must all hold
    // lower ids than the even rows. A scattered assignment would interleave them.
    let odd: Vec<u64> = ids.iter().skip(1).step_by(2).map(|e| e.raw()).collect();
    let even: Vec<u64> = ids.iter().step_by(2).map(|e| e.raw()).collect();
    assert!(
        odd.iter().max() < even.iter().min(),
        "items sharing a signature must land in one contiguous id run (§11.1); \
         got odd={odd:?} even={even:?}"
    );

    let next = engine
        .accept_ingest(vec![row("after")], "after".to_string(), [2u8; 32])
        .unwrap()[0];
    assert!(
        next.raw() > *even.iter().max().unwrap(),
        "ids are strictly monotone across commands (I9)"
    );
}

// =================================================================================================
// The ack contract
// =================================================================================================

/// **The fail-open this exists to catch is an ack that precedes the swap** — a client observing 200
/// for a suppression that is not yet in force (lifecycle §4).
///
/// ## Two legs, and which half of each one is the witness
///
/// The first draft of this comment said "the behavioural one is the test" and called the ordering
/// log "the corroborating diagnostic … on its own it would assert little more than the source order
/// of four `record` calls". **Measured, that was exactly backwards.** A reviewer planted a real
/// ack-before-swap in the deny commit path; only the log assertion failed, and with that one assertion
/// deleted the test was green five runs out of five with the fail-open live. The cause was
/// structural rather than luck: there was one pause site and it sat **before both** the ack and the
/// swap, so parking there said nothing about their relative order, and after `join()` the main
/// thread's own `authorise` + `viewport` round-trip meant the swap had always landed by the time
/// anything was checked.
///
/// So the fix is a second armed site, and the two legs below are what each site can actually prove:
///
/// - **Leg 1, parked at `AfterFsync`** — durable, not yet in force. The behavioural half is real
///   here (the item must still be visible; nothing has been applied) but it cannot see ack ordering,
///   for the reason above. Its ordering witness is the step log.
/// - **Leg 2, parked at `BeforeAck`** — in force, not yet acknowledged. This is the discriminating
///   leg. The pause point lives *inside* `Executor::ack`, one statement above the send, so it
///   travels with the ack rather than with a line number: a build that acks before it swaps parks
///   here with the swap still ahead of it, and `visible()` still shows the item. **The assertion
///   that fails is `visible(&engine) == before - 1` — engine state, no reference to the log.**
///
/// One honest limit, because it is what made the first draft wrong: `!handle.is_finished()` proves
/// nothing on its own. A negative statement about another thread's progress cannot be established
/// without waiting, so those assertions can only ever fail *late*, never soon enough to be relied
/// on. They are kept because when they do fire they name the fault precisely; they are not the
/// witness. The witnesses are the two `visible()` assertions and the step log.
#[test]
fn ack_follows_fsync_then_swap() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 8);
    let engine = Arc::new(engine);

    let before = visible(&engine);
    assert_eq!(
        before, N_ITEMS,
        "the items must be visible before we suppress them"
    );

    // --- Leg 1: parked after fsync, before the swap. Durable, not yet in force. -----------------
    let entity = entity_of(&engine, 3);
    faults.clear_log();
    faults.arm_pause(PauseSite::AfterFsync, PauseAction::Stall);

    let e = Arc::clone(&engine);
    let suppress = std::thread::spawn(move || e.accept_change(entity, ChangeOp::Suppress));

    // Wait for the executor to be *demonstrably* parked. Not a sleep: this returns only once the
    // executor has published its arrival.
    faults.await_arrivals(PauseSite::AfterFsync, 1, WAIT);

    assert!(
        !suppress.is_finished(),
        "the caller must still be waiting: the effect is durable but not yet in force"
    );
    assert_eq!(
        visible(&engine),
        before,
        "and the item must still be visible — nothing has been applied yet"
    );
    assert_eq!(
        faults.log(),
        vec![Step::Append, Step::Fsync],
        "at the kill point exactly the durability steps have run"
    );

    faults.release();
    suppress.join().unwrap().expect("the suppression succeeds");

    assert_eq!(
        faults.log(),
        vec![Step::Append, Step::Fsync, Step::Swap, Step::Ack],
        "and the ack lands strictly after the swap"
    );
    assert_eq!(
        visible(&engine),
        before - 1,
        "so the 200 the caller now holds is a promise the effect is in force"
    );

    // --- Leg 2: parked after the swap, before the ack. In force, not yet acknowledged. ----------
    //
    // This is the leg that discriminates. An executor that acked before it swapped parks here with
    // the swap still ahead of it, and the assertion below reads the *engine*, not the log.
    let second = entity_of(&engine, 6);
    faults.clear_log();
    faults.arm_pause(PauseSite::BeforeAck, PauseAction::Stall);

    let e = Arc::clone(&engine);
    let suppress = std::thread::spawn(move || e.accept_change(second, ChangeOp::Suppress));

    faults.await_arrivals(PauseSite::BeforeAck, 1, WAIT);

    assert_eq!(
        visible(&engine),
        before - 2,
        "the executor is parked in `ack`, one statement before the send: the effect it is about \
         to acknowledge MUST already be in force. Seeing {} here means the ack reached this point \
         with its swap still ahead of it — lifecycle §4's ack-ordering fail-open.",
        before - 1
    );
    assert!(
        !suppress.is_finished(),
        "and the receipt has not been sent yet"
    );
    assert_eq!(
        faults.log(),
        vec![Step::Append, Step::Fsync, Step::Swap],
        "the swap has run and the ack has not"
    );

    faults.release();
    suppress.join().unwrap().expect("the suppression succeeds");
    assert_eq!(
        faults.log(),
        vec![Step::Append, Step::Fsync, Step::Swap, Step::Ack]
    );
}

/// **Lifecycle §1.3's deny priority lane.** A deny must not queue behind work.
///
/// The discrimination is real rather than incidental: with one FIFO queue the deny's receipt would
/// arrive after *all* `BOUND` work items; with deny-first it arrives after **at most one** — the
/// item already executing when it landed.
#[test]
fn a_deny_is_never_queued_behind_work() {
    deny_priority_survives_the_window(None);
}

/// **The same property with group commit disabled** — `commit_window_max_items = 1`, which is the
/// documented spelling for turning the window off.
///
/// This leg is not a variation for its own sake. At that setting *every* entry trips the row bound,
/// so the window's close-and-continue path runs on each one; a `run_work_pass` that closed a window
/// and then kept draining the work queue would never return to `Executor::run`'s deny drain while
/// submissions keep arriving, and the deny would land last however the lanes are ordered. It is red
/// on exactly that defect and green on nothing else, which is why it takes a config no default sets.
#[test]
fn a_deny_is_never_queued_behind_work_with_group_commit_disabled() {
    deny_priority_survives_the_window(Some(1));
}

/// Sequencing is deterministic, not raced. The executor is stalled inside its first work item, the
/// work queue is filled to its bound behind it, and the deny is released only once the engine's own
/// `deny_submitted` counter proves it is *queued* — that counter is bumped after the enqueue and
/// before the blocking wait for exactly this purpose.
fn deny_priority_survives_the_window(window_max_rows: Option<usize>) {
    const BOUND: usize = 4;
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, BOUND);
    if let Some(rows) = window_max_rows {
        engine.set_commit_window_max_rows(rows);
    }
    let engine = Arc::new(engine);

    let entity = entity_of(&engine, 3);

    // A shared completion log: each finisher appends its tag, in real completion order.
    let order = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let started = Arc::new(AtomicU64::new(0));

    // Park the executor inside the first work item so the queue can be filled behind it.
    faults.arm_pause(PauseSite::AfterFsync, PauseAction::Stall);

    let mut workers = Vec::new();
    for i in 0..=BOUND {
        let e = Arc::clone(&engine);
        let order = Arc::clone(&order);
        let started = Arc::clone(&started);
        workers.push(std::thread::spawn(move || {
            started.fetch_add(1, Ordering::SeqCst);
            let _ = e.accept_ingest(
                vec![row(&format!("w-{i}"))],
                format!("w-{i}"),
                [i as u8; 32],
            );
            order.lock().unwrap().push("work");
        }));
        if i == 0 {
            // Make sure the first submission is the one occupying the executor, so the remaining
            // BOUND fill the queue rather than racing for the running slot.
            faults.await_arrivals(PauseSite::AfterFsync, 1, WAIT);
        }
    }

    // Wait until every work submission is enqueued, then submit the deny behind all of them.
    let deadline = std::time::Instant::now() + WAIT;
    while engine.write_executor_stats().work_submitted < (BOUND + 1) as u64 {
        assert!(
            std::time::Instant::now() < deadline,
            "work never filled the queue: {:?}",
            engine.write_executor_stats()
        );
        std::thread::yield_now();
    }

    let e = Arc::clone(&engine);
    let order_d = Arc::clone(&order);
    let deny = std::thread::spawn(move || {
        let _ = e.accept_change(entity, ChangeOp::Suppress);
        order_d.lock().unwrap().push("deny");
    });
    while engine.write_executor_stats().deny_submitted == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the deny never enqueued"
        );
        std::thread::yield_now();
    }

    faults.release();
    deny.join().unwrap();
    for w in workers {
        w.join().unwrap();
    }

    let order = order.lock().unwrap().clone();
    let deny_at = order
        .iter()
        .position(|s| *s == "deny")
        .expect("the deny completed");
    assert!(
        deny_at <= 1,
        "the deny must complete after at most the one work item already executing; it completed \
         at position {deny_at} of {order:?}. A single FIFO queue would put it last."
    );
}

/// **The same property on the third close path**, which the row bound does not reach.
///
/// `run_work_pass` closes a window mid-drain when an entry names an external id the open window
/// already holds (`CommitWindow::holds_external_id_of` — the mechanism that keeps the
/// unreachable-duplicate hole closed). A close that *continued* draining would never trip the row
/// bound on a conflict-heavy stream, because `window.rows()` resets with the replacement: a pass
/// could perform an unbounded number of full `append → fsync → apply → swap` cycles without ever
/// returning to `Executor::run`'s deny drain. That is lifecycle §1.3's prohibition verbatim — a
/// deny queued behind work of unbounded duration — and it is reachable at the shipped defaults from
/// a client re-ingesting an `external_id` a still-open window holds.
///
/// **The workload is pairs sharing an `external_id` under different batch ids, and the choice
/// matters.** Pairs sharing a *batch id* would assert nothing: a held batch id joins from inside
/// the window and forces no close at all, so there would be no close, no yield, and no test in the
/// tree for this property. Whichever member of an external-id pair the drain meets
/// second conflicts, whatever order the submitting threads reach the queue in.
///
/// The executor is parked at `AfterFsync` inside that first conflict-forced close, which is what
/// lets the deny be enqueued *during* the pass rather than before it — enqueueing it before would
/// prove nothing, since `Executor::run` drains deny at the top of every iteration anyway.
///
/// Red on the defect (`Admission::YieldedAfterClose`'s `break` removed): the deny lands last, after
/// all six work items. Green with it: after two.
#[test]
fn a_deny_is_never_queued_behind_a_conflict_forced_window_split() {
    const PAIRS: usize = 3;
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 16);
    // Well above the six single-row submissions, so the *row* bound cannot trip and take the credit
    // for a yield this test attributes to the conflict path.
    engine.set_commit_window_max_rows(1_000);
    let engine = Arc::new(engine);
    let entity = entity_of(&engine, 3);

    let order = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));

    // Park the executor inside the priming submission's **ack**, leaving `AfterFsync` free for the
    // conflict close this test is about. `Window::park` cannot be reused: it parks at `AfterFsync`.
    faults.arm_pause(PauseSite::BeforeAck, PauseAction::Stall);
    let e = Arc::clone(&engine);
    let prime =
        std::thread::spawn(move || e.accept_ingest(vec![row("prime")], "prime".into(), [0xEE; 32]));
    faults.await_arrivals(PauseSite::BeforeAck, 1, WAIT);

    // **Enqueued one at a time**, each wait on the executor's own `work_submitted` counter, so the
    // queue order is `pair-0 a, pair-0 b, pair-1 a, …` rather than a race between six threads. The
    // first draft spawned them together and flaked 2 runs in 8: with the order `0a, 1a, 2a, 0b` the
    // window legitimately holds three entries before the first conflict, so the deny lands at 3.
    // Determinism here is what lets the assertion be `<= 2` — tight enough to discriminate — and it
    // is also what makes the defect produce *three* conflict-forced closes rather than one.
    let mut workers = Vec::new();
    let deadline = std::time::Instant::now() + WAIT;
    for i in 0..PAIRS {
        for half in 0..2u8 {
            let e = Arc::clone(&engine);
            let order = Arc::clone(&order);
            workers.push(std::thread::spawn(move || {
                // Same **external id** within a pair, different batch ids: the second one 409s
                // on `established_collisions` once the close has applied the first. (A shared
                // *batch id* would force no close at all — the join answers it in place — which is
                // why the workload is shaped this way.)
                let _ = e.accept_ingest(
                    vec![row(&format!("c-{i}"))],
                    format!("pair-{i}-{half}"),
                    [(i as u8) * 2 + half; 32],
                );
                order.lock().unwrap().push("work");
            }));
            let want = workers.len() as u64 + 1; // +1 for the priming submission
            while engine.write_executor_stats().work_submitted < want {
                assert!(
                    std::time::Instant::now() < deadline,
                    "submission {want} never reached the queue: {:?}",
                    engine.write_executor_stats()
                );
                std::thread::yield_now();
            }
        }
    }

    // Armed before the release that starts the run it must observe — the shape `release_site`
    // exists for. The priming window's own `AfterFsync` is already behind it.
    faults.arm_pause(PauseSite::AfterFsync, PauseAction::Stall);
    faults.release_site(PauseSite::BeforeAck);
    faults.await_arrivals(PauseSite::AfterFsync, 1, WAIT);

    let e = Arc::clone(&engine);
    let order_d = Arc::clone(&order);
    let deny = std::thread::spawn(move || {
        let _ = e.accept_change(entity, ChangeOp::Suppress);
        order_d.lock().unwrap().push("deny");
    });
    while engine.write_executor_stats().deny_submitted == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the deny never enqueued"
        );
        std::thread::yield_now();
    }

    faults.release();
    deny.join().unwrap();
    for w in workers {
        w.join().unwrap();
    }
    prime.join().unwrap().expect("the priming submission");

    let order = order.lock().unwrap().clone();
    assert_eq!(
        order.len(),
        PAIRS * 2 + 1,
        "every submission and the deny must have completed: {order:?}"
    );
    let deny_at = order
        .iter()
        .position(|s| *s == "deny")
        .expect("the deny completed");
    assert!(
        deny_at <= 2,
        "the deny must complete after the conflict-forced close and the entry that forced it — at \
         most two work items; it completed at position {deny_at} of {order:?}. A pass that kept \
         draining after a conflict close would put it last."
    );
}

// =================================================================================================
// WAL failure
// =================================================================================================

/// **Lifecycle §4: never a refusal that leaves a deny unapplied.** A durability failure the
/// executor cannot repair returns an error *and* the item is gone.
///
/// Every attempt is armed to fail — `DENY_DURABILITY_ATTEMPTS`, not a hard-coded number — because
/// this case is about the *exhausted* path. Arming one failure now exercises recovery instead
/// (`a_deny_whose_first_sync_fails_is_retried_into_durability`), and a test hard-coding the count
/// would silently change subject the day the retry schedule does, while staying green either way.
///
/// The pre-assertion that the item is visible first is not ceremony: without it a fixture that
/// never contained the item would satisfy "no longer shows the item" and this would pass vacuously.
#[test]
fn deny_append_failure_still_applies() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 8);

    let entity = entity_of(&engine, 3);
    let before = visible(&engine);
    assert_eq!(
        before, N_ITEMS,
        "the item must be visible before the suppression"
    );

    faults.fail_next_fsyncs(DENY_DURABILITY_ATTEMPTS);
    let err = engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect_err("a failed durability write must be reported, never silently swallowed");

    assert_eq!(
        visible(&engine),
        before - 1,
        "…and the item must be hidden ANYWAY: an under-durable deny beats a refused one ({err})"
    );

    // The node went unready over the failure and comes back by discarding the undurable region.
    // Asserted through the recovery counter rather than through the posture, because the posture is
    // no longer a stable value to read from another thread: the executor clears it on its own next
    // pass, so a bare `assert_eq!(posture, WalPoisoned)` here would be a race that happens to win.
    // A counter that only ever rises records the same event without one.
    wait_until("the executor discarded the undurable region", || {
        engine.write_executor_stats().wal_recoveries == 1
    });
    assert_eq!(
        engine.write_executor_posture(),
        ExecutorPosture::Running,
        "recovery is the point: a node must not stay unready over a condition that has cleared"
    );
    assert_eq!(
        visible(&engine),
        before - 1,
        "and recovery must not un-apply the suppression — discarding the log's undurable tail says \
         nothing about the overlay, which keeps the item hidden for the life of this process"
    );
}

/// **A deny whose first sync fails is retried into durability, and answered 200.**
///
/// The whole point of the retry: durability that was still reachable is reached, so the caller is
/// told the truth — the disposition *is* durable — instead of being handed a 500 and an obligation
/// it does not owe. One failure is armed, so the first repair attempt succeeds.
///
/// **The reopen is the assertion, not the 200.** A success receipt is a claim about the live node;
/// only a fresh `Engine` — which replays the WAL's durable prefix and rebuilds the overlay from it —
/// says whether the suppression outlived the process. Without that half this test would pass over an
/// implementation that acked 200 and wrote nothing, which is the fail-open the whole ack contract
/// exists to prevent.
#[test]
fn a_deny_whose_first_sync_fails_is_retried_into_durability() {
    let tmp = TempDir::new().unwrap();
    let wal_path = tmp.path().join("wal.log");
    let (engine, faults) = engine_with_faults(&tmp, 8);

    let entity = entity_of(&engine, 3);
    let before = visible(&engine);
    assert_eq!(
        before, N_ITEMS,
        "the item is visible before the suppression"
    );

    faults.fail_next_fsyncs(1);
    engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect("the retry reached durability, so the honest answer is a success, not a 500");

    assert_eq!(visible(&engine), before - 1, "and the item is hidden");
    assert_eq!(
        engine.write_executor_posture(),
        ExecutorPosture::Running,
        "durability was reached, so nothing is owed and the node may still claim ready"
    );
    drop(engine);

    let mut reopened = open_engine(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache-reopened"),
        &wal_path,
    );
    reopened
        .start_write_executor(8)
        .expect("the executor starts once");
    assert_eq!(
        visible(&reopened),
        before - 1,
        "the suppression must survive the restart — that is what the 200 promised"
    );
}

/// **And the residual, stated as a test rather than as a caveat.** When every attempt fails the old
/// behaviour stands in full: applied in memory, 500, and gone after a restart.
///
/// This is the case contracts §3.1's retry clause exists for. It is not a leak — the item returns to
/// exactly the visibility it had before the request — but it is a divergence between what a live node
/// shows and what a restarted one shows, and the only thing that closes it is the caller retrying.
///
/// Two assertions, because either alone passes over the wrong implementation: the *in-memory* hide
/// (which a refusal would skip, leaving the item visible — lifecycle §4's third forbidden answer)
/// and the *absence after reopen* (which a replay past the durable prefix would undo).
#[test]
fn a_deny_whose_retries_are_exhausted_is_hidden_now_and_visible_after_a_restart() {
    let tmp = TempDir::new().unwrap();
    let wal_path = tmp.path().join("wal.log");
    let (engine, faults) = engine_with_faults(&tmp, 8);

    let entity = entity_of(&engine, 3);
    let before = visible(&engine);

    faults.fail_next_fsyncs(DENY_DURABILITY_ATTEMPTS);
    engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect_err("every attempt failed, so durability is owed and the answer is an error");
    assert_eq!(
        visible(&engine),
        before - 1,
        "the item is hidden anyway for as long as this process lives"
    );
    drop(engine);

    let mut reopened = open_engine(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache-reopened"),
        &wal_path,
    );
    reopened
        .start_write_executor(8)
        .expect("the executor starts once");
    assert_eq!(
        visible(&reopened),
        before,
        "nothing was ever made durable, so replay cannot reinstate it — the caller was told to \
         retry, and this is what it costs if they do not"
    );
}

/// The negative control for the rule above: **the apply-anyway exception is deny-scoped**.
///
/// An ingest whose append failed applies nothing — applying un-fsynced ingest would make items
/// appear and vanish across a crash, and that holds per window as it does per command. The extra
/// assertion that the high-water *did* advance keeps "nothing applied" from being satisfied
/// vacuously by a failure that happened before assignment: it pins that the batch reached
/// allocation and burned its ids, which is the documented, I9-safe behaviour.
#[test]
fn an_ingest_append_failure_applies_nothing() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 8);

    let before_hw = engine.allocator_high_water();
    faults.fail_next_fsyncs(1);
    let err = engine
        .accept_ingest(vec![row("never-lands")], "doomed".to_string(), [3u8; 32])
        .expect_err("an ingest whose append failed must be refused");

    assert!(
        engine
            .resolve_external_id(b"never-lands")
            .unwrap()
            .is_none(),
        "nothing may be applied for an ingest that was never made durable ({err})"
    );
    assert_eq!(
        engine.allocator_high_water(),
        before_hw + 1,
        "the batch did reach assignment and burned its ids — monotone, never reused (I9)"
    );
}

/// **And a restart must not undo that refusal.** An ingest whose durability write failed is
/// refused, applies nothing — and is still absent after the process reopens the WAL.
///
/// This is the mirror of the acknowledgement contract: §4 forbids acknowledging an item that is
/// not durable, and equally forbids an item becoming durable after its caller was told it was not.
/// The failure leaves the record's bytes in the file — the append succeeded and only the fsync did
/// not — so a replay that reinstated every well-framed record would make the item exist behind the
/// caller's back. A caller who did what the 500 tells them to do and retried under a fresh batch
/// identifier would then hold two.
///
/// Asserted through `Engine` rather than `Wal` because the divergence is only observable as an
/// item: `resolve_external_id` answers from state replay rebuilt.
#[test]
fn an_ingest_whose_durability_failed_stays_absent_across_a_reopen() {
    let tmp = TempDir::new().unwrap();
    let wal_path = tmp.path().join("wal.log");
    let (engine, faults) = engine_with_faults(&tmp, 8);

    faults.fail_next_fsyncs(1);
    engine
        .accept_ingest(vec![row("never-lands")], "doomed".to_string(), [3u8; 32])
        .expect_err("an ingest whose fsync failed must be refused");
    drop(engine);

    let mut reopened = open_engine(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache-reopened"),
        &wal_path,
    );
    reopened
        .start_write_executor(8)
        .expect("the executor starts once");

    assert!(
        reopened
            .resolve_external_id(b"never-lands")
            .unwrap()
            .is_none(),
        "the caller was told this ingest was not durable; a restart must not make it so"
    );
}

/// **The apply-anyway exception is scoped to the two deny ops, and this is the control that pins
/// the scope.** An `Unsuppress` whose durability write failed applies **nothing**.
///
/// Lifecycle §4 makes a `Delete` or a `Suppress` whose WAL write failed apply anyway: an
/// under-durable deny beats a refused one, because the alternative is a refusal that leaves an item
/// visible. The exception cannot extend to `Unsuppress`. Applying one without durability re-exposes
/// an item that replay still hides — the item is visible now, invisible again after a restart, and
/// the caller's own 500 body reports the change as *not* applied, because
/// `exec_failure_may_be_in_force` scopes "may be in force" to the deny ops as well. So an operator
/// is told the item is still suppressed while every viewer can see it. That is the fail-open the
/// deny rules exist to prevent, reached by widening a rule written to prevent a different one.
///
/// **Why this test and not the two beside it.** `deny_append_failure_still_applies` exercises
/// `Suppress` only and asserts the *applies* half, so widening the scope leaves it green.
/// `an_ingest_append_failure_applies_nothing` is the ingest control — a different lane, a different
/// rule. Neither constrains which **ops** the exception covers, and the only other `Unsuppress`
/// assertions in the tree are pure-function tests over the response body, which stay green while the
/// engine does the opposite of what they report. Widening the op filter in `Executor::commit_denies`'s
/// failure fold was a green mutation across the whole suite before this test existed.
///
/// The suppression is established **durably** first, with no fault armed. Without that the test
/// would be asserting that an item nothing ever hid stayed hidden — and it also fixes the order the
/// mechanism requires: `Wal::fsync` poisons the handle on failure and `Wal::append` refuses every
/// later write, so the fault must be armed after the state this test needs is already on disk.
#[test]
fn an_unsuppress_append_failure_applies_nothing() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 8);

    let entity = entity_of(&engine, 3);
    let before = visible(&engine);
    assert_eq!(
        before, N_ITEMS,
        "the item is visible before it is suppressed"
    );

    engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect("the suppression is durable: no fault is armed yet");
    let suppressed = visible(&engine);
    assert_eq!(
        suppressed,
        before - 1,
        "the item must be hidden before the unsuppress, or this test asserts nothing"
    );

    faults.fail_next_fsyncs(DENY_DURABILITY_ATTEMPTS);
    let err = engine
        .accept_change(entity, ChangeOp::Unsuppress)
        .expect_err("a failed durability write must be reported, never silently swallowed");

    assert_eq!(
        visible(&engine),
        suppressed,
        "the item must STAY hidden: an unsuppress that is not durable must not be applied, or a \
         restart re-hides an item the operator was told was still suppressed anyway ({err})"
    );

    // **And recovery must not apply it either.** This is the sharpest form of the rule: the
    // `Unsuppress` record was appended before the sync failed, so it sits in the undurable region,
    // and a recovery that made that region durable rather than discarding it would un-hide the item
    // at the next replay — behind a 500 whose body says nothing was applied.
    wait_until("the executor discarded the undurable region", || {
        engine.write_executor_stats().wal_recoveries == 1
    });
    assert_eq!(
        visible(&engine),
        suppressed,
        "the item must still be hidden after recovery"
    );
}

/// **A torn WAL trips the posture, stays there, and still applies denies** (lifecycle §4).
///
/// The second half is what stops an obvious optimisation from being fail-open. "The posture is
/// poisoned, so skip the WAL call and return the error" would satisfy the first assertion while
/// leaving **every deny after the first unapplied** — the exact failure §4 exists to prevent. So
/// this submits a suppression *after* the poison and requires that the item still disappears.
///
/// **The fault is an append failure, not a sync failure, and that is now load-bearing.** A partial
/// `write_all` leaves no record boundary, so it is terminal and the posture is a stable value another
/// thread can read. A *sync* failure is recoverable: the executor discards the undurable region on
/// its next pass and the posture returns to `Running`, so asserting `WalPoisoned` after one would be
/// a race that happens to win — see `a_recovered_wal_returns_to_ready_without_a_restart` for that
/// path asserted as the eventual condition it is.
#[test]
fn a_torn_wal_stays_poisoned_and_still_applies_denies() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 8);

    let doomed = entity_of(&engine, 3);
    faults.fail_next_appends(1);
    let _ = engine.accept_change(doomed, ChangeOp::Suppress);
    assert_eq!(
        engine.write_executor_posture(),
        ExecutorPosture::WalPoisoned
    );

    // An ingest after the poison is refused outright and applies nothing.
    assert!(engine
        .accept_ingest(vec![row("after-poison")], "after".to_string(), [4u8; 32])
        .is_err());
    assert!(engine
        .resolve_external_id(b"after-poison")
        .unwrap()
        .is_none());

    // But a deny after the poison is still APPLIED, and still reported as failed.
    let before = visible(&engine);
    let second = entity_of(&engine, 6);
    let err = engine.accept_change(second, ChangeOp::Suppress);
    assert!(
        err.is_err(),
        "durability is still owed, so this is still a 500"
    );
    assert_eq!(
        visible(&engine),
        before - 1,
        "a poisoned WAL must not stop denies being applied — that is why WalPoisoned is a posture \
         and not a shutdown"
    );

    // **And it must never leave.** The recovery path exists and runs on a timer while degraded, so
    // this asserts the WAL's own terminality rather than trusting that nothing calls it: there is no
    // boundary to rewind a torn append to, and a node that reported ready again would be claiming a
    // log it cannot describe.
    assert_eq!(
        engine.write_executor_stats().wal_recoveries,
        0,
        "a torn WAL offers nothing to discard, so no recovery may be recorded"
    );
    assert_eq!(
        engine.write_executor_posture(),
        ExecutorPosture::WalPoisoned,
        "and the posture stays there for the life of the process"
    );
}

/// **A node that loses durability and gets it back returns to ready without a restart.**
///
/// The failure latched the posture for the life of the process before this: every cause — a
/// filesystem that filled and was relieved, a device that stumbled — cost the node its routing until
/// someone noticed. Denies were never blocked (they are applied in memory and answered 500 whatever
/// the posture says), so what was lost was reads, to a node that was fine.
///
/// The wait is on `wal_recoveries`, a value the executor publishes, not on a duration.
#[test]
fn a_recovered_wal_returns_to_ready_without_a_restart() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 8);

    let entity = entity_of(&engine, 3);
    faults.fail_next_fsyncs(DENY_DURABILITY_ATTEMPTS);
    engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect_err("every attempt failed, so durability is owed");

    wait_until("the executor recovered", || {
        engine.write_executor_stats().wal_recoveries == 1
    });
    assert_eq!(
        engine.write_executor_posture(),
        ExecutorPosture::Running,
        "the condition has cleared, so the node must stop reporting it"
    );

    // Ready means ready: the next write must actually land, durably, with no fault armed.
    let second = entity_of(&engine, 6);
    let before = visible(&engine);
    engine
        .accept_change(second, ChangeOp::Suppress)
        .expect("a recovered executor accepts writes again");
    assert_eq!(visible(&engine), before - 1);
}

/// **The recovery discards the undurable region; it does not publish it.** This is the fail-open the
/// direction of the recovery is chosen to avoid, asserted end to end.
///
/// An `Unsuppress` is appended to the WAL like every other deny entry, but lifecycle §4 scopes the
/// apply-anyway rule to deletion and suppression, so a failed window deliberately does **not** apply
/// it: the item stays hidden and its caller is told the change did not take. Those bytes are
/// nonetheless sitting above the durable boundary. A recovery that finished the interrupted write —
/// which is the intuitive reading of "repair" and is free for the half where the data is already on
/// disk and only the boundary record is missing — would make them durable, and the next replay would
/// un-hide an item whose operator was told it was still suppressed.
///
/// So the assertion is on a **reopened** engine, because the divergence is invisible on the live one:
/// the overlay keeps the item hidden either way, and only replay shows which bytes were kept.
#[test]
fn recovery_discards_the_undurable_region_rather_than_publishing_it() {
    let tmp = TempDir::new().unwrap();
    let wal_path = tmp.path().join("wal.log");
    let (engine, faults) = engine_with_faults(&tmp, 8);

    let entity = entity_of(&engine, 3);
    let before = visible(&engine);
    engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect("the suppression is durable: no fault is armed yet");
    let suppressed = visible(&engine);
    assert_eq!(
        suppressed,
        before - 1,
        "the item must be hidden before the unsuppress, or this test asserts nothing"
    );

    faults.fail_next_fsyncs(DENY_DURABILITY_ATTEMPTS);
    engine
        .accept_change(entity, ChangeOp::Unsuppress)
        .expect_err("the unsuppress never became durable");
    wait_until("the executor recovered", || {
        engine.write_executor_stats().wal_recoveries == 1
    });
    drop(engine);

    let mut reopened = open_engine(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache-reopened"),
        &wal_path,
    );
    reopened
        .start_write_executor(8)
        .expect("the executor starts once");
    assert_eq!(
        visible(&reopened),
        suppressed,
        "the item must STILL be suppressed after the restart: the unsuppress was refused, so \
         recovery must have discarded its record rather than publishing it"
    );
}

/// The drop guard, **and the error the in-flight caller is handed while it fires**. An executor that
/// panics must report `Dead`, however it panicked; the submitter whose command it was holding must
/// be told `ReceiptLost`, never `ExecutorDead`.
///
/// Without a guard on the thread's own stack the posture would stay `Running` for ever and a
/// not-ready gate built on it would be green over a dead writer — the worst available outcome,
/// since a caller would keep being told its suppressions are in flight.
///
/// **The in-flight assertion pins `ReceiptLost` at its producer.**
/// `write.rs`'s `submit` answers `SubmitError::ReceiptLost` when the responder is
/// dropped, and `tessera-server`'s `map_accept_error` maps that to a fail-closed **500** rather than
/// the **503 `not-ready`** `ExecutorDead` gets — because a command the executor died *holding* may
/// have been appended, fsynced, applied and swapped, and 503's whole meaning is "this node did not
/// take your write". Until this assertion existed, every `ReceiptLost` in the workspace's tests was
/// a variant *constructed by the test*: reverting `submit`'s two `ReceiptLost` producers back to
/// `ExecutorDead` compiled and passed `cargo test --workspace` in full, leaving the split enforced
/// only by `error.rs`'s mapping over a variant nothing produced.
///
/// That matters more than an ordinary coverage gap because **this region gets rewritten**: a commit
/// window performs one swap and then acks N waiters in a loop, which
/// `SubmitError::ReceiptLost`'s own doc names as the widening. A refactor that reinstates
/// `ExecutorDead` here reinstates a fail-open: a durable, in-force suppression answered
/// "nothing in this request was applied".
///
/// Deterministic, not raced: `PauseAction::Panic` fires *inside* the executor's `AfterFsync` pause
/// point, so the job is provably still in `Executor::execute`'s frame — the receipt channel's sender
/// is dropped by the unwind and `rx.recv()` cannot succeed. The posture poll below is separate and
/// is what the drop guard is asserted with.
#[test]
fn an_executor_panic_is_reported_dead() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 8);

    assert_eq!(engine.write_executor_posture(), ExecutorPosture::Running);

    faults.arm_pause(PauseSite::AfterFsync, PauseAction::Panic);
    let in_flight = engine
        .accept_ingest(vec![row("boom")], "boom".to_string(), [5u8; 32])
        .expect_err("the executor panicked while holding this command, so no receipt can arrive");
    assert!(
        matches!(in_flight, AcceptError::Submit(SubmitError::ReceiptLost)),
        "a command the executor died HOLDING may be fully applied and swapped in; only \
         `ReceiptLost` maps to the fail-closed 500. `ExecutorDead` here is the 503 that reports an \
         in-force suppression as a no-op. Got: {in_flight:?}"
    );

    let deadline = std::time::Instant::now() + WAIT;
    while engine.write_executor_posture() != ExecutorPosture::Dead {
        assert!(
            std::time::Instant::now() < deadline,
            "the posture never reached Dead: got {:?}",
            engine.write_executor_posture()
        );
        std::thread::yield_now();
    }

    // And every subsequent submit reports it rather than answering a hopeful 202.
    faults.release();
    let err = engine
        .accept_change(EntityId::new(1), ChangeOp::Suppress)
        .expect_err("a dead executor must be reported, never swallowed");
    assert!(
        matches!(
            err,
            tessera_engine::AcceptError::Submit(tessera_lifecycle::SubmitError::ExecutorDead)
        ),
        "got: {err}"
    );

    // **`Dead` is absorbing, and it has to be asserted now that the posture is composed rather than
    // latched as one value.** The WAL this executor left behind is perfectly healthy — the panic was
    // at a pause point, not a durability failure — so a composition that read the WAL first, or that
    // let the thread's own state be re-stored rather than raised, would answer `Running` for a
    // process with no writer at all. That is the worst available answer: a caller would go on being
    // told its suppressions are in flight.
    for _ in 0..64 {
        assert_eq!(
            engine.write_executor_posture(),
            ExecutorPosture::Dead,
            "a dead executor must never report anything else, whatever its WAL says"
        );
        std::thread::yield_now();
    }
}

/// An engine that never started an executor refuses writes rather than pretending.
///
/// This is the state every read-only test, bench and example is in — starting no
/// thread is the point of the explicit start call — so the refusal must be honest rather than a
/// panic or a hang, and reads must be entirely unaffected.
#[test]
fn an_engine_without_an_executor_refuses_writes_and_still_reads() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );

    assert_eq!(engine.write_executor_posture(), ExecutorPosture::NotStarted);
    assert!(engine
        .accept_ingest(vec![row("nope")], "nope".to_string(), [6u8; 32])
        .is_err());
    assert_eq!(visible(&engine), N_ITEMS);
}

/// The backstop for the check-to-apply race the executor widened (security review C1): a second
/// batch naming an already-established external id is **refused**, not silently applied over the
/// top.
///
/// Left unclosed, the first item would stay visible, byte-identical to the second, and reachable by
/// no external id at all — so no `suppress` could ever name it. The handler's own duplicate check
/// cannot close this, because it reads a map that is now written a whole queue drain later; only a
/// check on the thread that also performs the insert can.
#[test]
fn a_duplicate_external_id_is_refused_on_the_executor() {
    let tmp = TempDir::new().unwrap();
    let (engine, _faults) = engine_with_faults(&tmp, 8);

    let first = engine
        .accept_ingest(vec![row("dup")], "b1".to_string(), [7u8; 32])
        .expect("the first batch is accepted")[0];

    // A *different* batch id, as a client retry after a timeout produces — so the idempotency index
    // does not catch it and only the external-id backstop can.
    let err = engine
        .accept_ingest(vec![row("dup")], "b2".to_string(), [8u8; 32])
        .expect_err("a duplicate external id must be refused on the executor");
    assert!(
        matches!(
            err,
            tessera_engine::AcceptError::Exec(
                tessera_lifecycle::ExecError::DuplicateExternalId { count: 1 }
            )
        ),
        "got: {err}"
    );

    assert_eq!(
        engine.resolve_external_id(b"dup").unwrap(),
        Some(first),
        "the original must still own the key — an overwrite is what would make the first item \
         unreachable by any deny"
    );
}

// =================================================================================================
// The commit window
// =================================================================================================

/// A window assembled **deterministically**, with no sleep and no timing assumption.
///
/// The problem it solves: "these N submissions landed in one window" is a race unless the executor
/// is held still while they queue. So it is held still — parked inside a priming submission's own
/// window, at the `AfterFsync` site — and released only once the engine's own `work_submitted`
/// counter proves all N are *enqueued*. Both waits are on conditions the executor publishes (the
/// switchboard's arrival count; the submission counter bumped after `try_send`), exactly as
/// `a_deny_is_never_queued_behind_work` does.
///
/// Returns each submission's result **in submission order**, and the priming submission's id.
type Accepted = Result<Vec<EntityId>, AcceptError>;
type Submissions = Arc<std::sync::Mutex<Vec<Option<Result<Vec<EntityId>, String>>>>>;
/// One row's sort key material: `(submission, row, signature, external_id)`.
type SortKey = (usize, usize, Vec<u32>, Option<Vec<u8>>);

struct Window {
    engine: Arc<Engine>,
    faults: Arc<FaultSwitchboard>,
    prime: std::sync::Mutex<Option<std::thread::JoinHandle<Accepted>>>,
    /// The allocator's high-water mark with the priming submission's ids already issued — the id
    /// the window's own first assignment starts from. Read while the executor is parked *after*
    /// its fsync, and allocation precedes the append, so the priming row is already counted.
    high_water_before: u64,
}

impl Window {
    /// Park the executor inside a priming submission, so a window can be assembled behind it.
    fn park(engine: Arc<Engine>, faults: Arc<FaultSwitchboard>) -> Self {
        faults.arm_pause(PauseSite::AfterFsync, PauseAction::Stall);
        let e = Arc::clone(&engine);
        let prime = std::thread::spawn(move || {
            e.accept_ingest(vec![row("prime")], "prime".to_string(), [0xEE; 32])
        });
        faults.await_arrivals(PauseSite::AfterFsync, 1, WAIT);
        let high_water_before = engine.allocator_high_water();
        Window {
            engine,
            faults,
            prime: std::sync::Mutex::new(Some(prime)),
            high_water_before,
        }
    }

    /// Join the priming submission. Called after a release; it must have succeeded, or the fixture
    /// itself is what failed.
    fn join_prime(&self) {
        if let Some(h) = self.prime.lock().unwrap().take() {
            h.join()
                .unwrap()
                .expect("the priming submission is accepted");
        }
    }

    /// Submit `batches` concurrently, wait until every one is *enqueued*, then release the executor
    /// so they are drained into one window.
    fn run(
        &self,
        batches: Vec<(String, Vec<UnallocatedRow>)>,
    ) -> Vec<Result<Vec<EntityId>, String>> {
        let n = batches.len();
        let results: Submissions = Arc::new(std::sync::Mutex::new((0..n).map(|_| None).collect()));
        let mut handles = Vec::new();
        for (i, (batch_id, rows)) in batches.into_iter().enumerate() {
            let e = Arc::clone(&self.engine);
            let out = Arc::clone(&results);
            handles.push(std::thread::spawn(move || {
                let hash = [i as u8; 32];
                let r = e
                    .accept_ingest(rows, batch_id, hash)
                    .map_err(|err| format!("{err:?}"));
                out.lock().unwrap()[i] = Some(r);
            }));
        }

        // Every submission enqueued: +1 for the priming one already in flight.
        let deadline = std::time::Instant::now() + WAIT;
        while self.engine.write_executor_stats().work_submitted < (n + 1) as u64 {
            assert!(
                std::time::Instant::now() < deadline,
                "submissions never reached the queue: {:?}",
                self.engine.write_executor_stats()
            );
            std::thread::yield_now();
        }

        self.faults.release();
        for h in handles {
            h.join().unwrap();
        }
        self.join_prime();
        let mut out = results.lock().unwrap().clone();
        out.drain(..)
            .map(|r| r.expect("every thread ran"))
            .collect()
    }
}

/// Signature-sorted assignment over the whole window, recomputed in the test: the rows in
/// `(submission, row)` order, ordered by `(sorted-deduplicated terms, external_id)`, assigned
/// `lo + rank`. §11.1's rule, independently of the implementation of it.
fn expected_assignment(batches: &[(String, Vec<UnallocatedRow>)], lo: u64) -> Vec<Vec<u64>> {
    let mut flat: Vec<SortKey> = Vec::new();
    for (b, (_, rows)) in batches.iter().enumerate() {
        for (i, r) in rows.iter().enumerate() {
            let mut key: Vec<u32> = r.terms.iter().map(|t| t.raw()).collect();
            key.sort_unstable();
            key.dedup();
            flat.push((b, i, key, r.external_id.clone()));
        }
    }
    let mut order: Vec<usize> = (0..flat.len()).collect();
    order.sort_by(|a, b| {
        flat[*a]
            .2
            .cmp(&flat[*b].2)
            .then_with(|| flat[*a].3.cmp(&flat[*b].3))
    });
    let mut out: Vec<Vec<u64>> = batches.iter().map(|(_, r)| vec![0; r.len()]).collect();
    for (rank, idx) in order.into_iter().enumerate() {
        let (b, i, _, _) = &flat[idx];
        out[*b][*i] = lo + rank as u64;
    }
    out
}

fn sig_rows(
    prefix: &str,
    n: usize,
    low: &[tessera_types::TermId],
    high: &[tessera_types::TermId],
) -> Vec<UnallocatedRow> {
    (0..n)
        .map(|i| {
            let mut r = row(&format!("{prefix}-{i}"));
            r.terms = if i % 2 == 0 {
                high.to_vec()
            } else {
                low.to_vec()
            };
            r
        })
        .collect()
}

/// **THE HEADLINE.** Four 25-row submissions produce an assignment *identical* to one 100-row
/// submission of the same rows — design §11.1's sort scope is the **window, at the server**, not
/// whatever chunk size a client happened to pick.
///
/// *If this does not hold, group commit is decoration.* The discrimination is real rather than
/// incidental: under per-command allocation each submission's 25 rows are sorted among
/// themselves and the four blocks land in four disjoint id ranges, so the interleaved signatures
/// stay interleaved in entity space and every posting run is 25 long instead of 50.
///
/// Two engines, because the comparison is between two *worlds*: both are primed identically, so
/// both allocators are at the same high-water when the compared assignment begins.
#[test]
fn the_sort_scope_is_the_window_not_the_request() {
    let tmp_a = TempDir::new().unwrap();
    let (engine_a, faults_a) = engine_with_faults(&tmp_a, 64);
    let engine_a = Arc::new(engine_a);
    let low = engine_a.resolve_terms(&[b"0".to_vec()]);
    let high = engine_a.resolve_terms(&[b"1".to_vec()]);
    assert_ne!(low, high, "the fixture must offer two real signatures");

    let split: Vec<(String, Vec<UnallocatedRow>)> = (0..4)
        .map(|s| (format!("s{s}"), sig_rows(&format!("k{s}"), 25, &low, &high)))
        .collect();

    let window = Window::park(Arc::clone(&engine_a), faults_a);
    let lo = window.high_water_before;
    let got = window.run(split.clone());
    let split_ids: Vec<Vec<u64>> = got
        .iter()
        .map(|r| {
            r.as_ref()
                .expect("every submission in the window is accepted")
                .iter()
                .map(|e| e.raw())
                .collect()
        })
        .collect();

    // The same rows, submitted as one batch, on a fresh engine primed the same way.
    let tmp_b = TempDir::new().unwrap();
    let (engine_b, faults_b) = engine_with_faults(&tmp_b, 64);
    let engine_b = Arc::new(engine_b);
    let whole_rows: Vec<UnallocatedRow> = split.iter().flat_map(|(_, r)| r.clone()).collect();
    let window_b = Window::park(Arc::clone(&engine_b), faults_b);
    assert_eq!(
        window_b.high_water_before, lo,
        "both worlds start from one id"
    );
    let whole = window_b.run(vec![("whole".to_string(), whole_rows)]);
    let whole_ids: Vec<u64> = whole[0]
        .as_ref()
        .expect("the single submission is accepted")
        .iter()
        .map(|e| e.raw())
        .collect();

    let flat_split: Vec<u64> = split_ids.iter().flatten().copied().collect();
    assert_eq!(
        flat_split, whole_ids,
        "four submissions in one window must be allocated exactly as one submission of the same \
         rows. Per-request scope gives four disjoint sorted blocks instead — group commit's whole \
         subject (design §11.1, lifecycle §5.1)"
    );

    // And the sort is doing something: every low-signature row precedes every high one. Taken from
    // the rows' own terms rather than from an index parity — the submissions hold an odd number of
    // rows each, so parity in the flattened list is not parity within a submission.
    let mut low_ids = Vec::new();
    let mut high_ids = Vec::new();
    for (b, (_, rows)) in split.iter().enumerate() {
        for (i, r) in rows.iter().enumerate() {
            if r.terms == low {
                low_ids.push(split_ids[b][i]);
            } else {
                high_ids.push(split_ids[b][i]);
            }
        }
    }
    assert!(
        low_ids.iter().max() < high_ids.iter().min(),
        "items sharing a signature must land in one contiguous run ACROSS submissions: \
         low={low_ids:?} high={high_ids:?}"
    );

    // The independent recomputation of §11.1's rule, which also pins each submission's ids to its
    // own rows in its own order.
    assert_eq!(split_ids, expected_assignment(&split, lo));
}

/// **One fsync per window**, asserted on the counter rather than on a proxy.
///
/// The exact delta, not "did not rise": `WalMeter` counts *successes*, so an implementation that
/// skipped the fsync entirely would satisfy "did not rise with the number of submissions" while
/// acknowledging writes that are not durable — the one thing the ack contract forbids. The append
/// counter is the other half: one record per entry is what preserves batch identity, which is what
/// a joined retry is answered off.
#[test]
fn one_fsync_per_window() {
    const N: usize = 5;
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 64);
    let engine = Arc::new(engine);

    let window = Window::park(Arc::clone(&engine), faults);
    let parked = engine.write_executor_stats();
    assert_eq!(
        (parked.wal_appends, parked.wal_fsyncs),
        (1, 1),
        "the priming submission has appended and fsynced exactly once"
    );

    let batches: Vec<(String, Vec<UnallocatedRow>)> = (0..N)
        .map(|i| (format!("b{i}"), vec![row(&format!("f{i}"))]))
        .collect();
    for r in window.run(batches) {
        r.expect("every submission is accepted");
    }

    let after = engine.write_executor_stats();
    assert_eq!(
        after.wal_fsyncs - parked.wal_fsyncs,
        1,
        "{N} submissions in one window must cost exactly ONE fsync — that is the amortisation. \
         Got {} fsyncs for {N} submissions",
        after.wal_fsyncs - parked.wal_fsyncs
    );
    assert_eq!(
        after.wal_appends - parked.wal_appends,
        N as u64,
        "and exactly one record per entry: batch identity survives the window, which is what a \
         joined retry is answered off"
    );
}

/// Each waiter receives **its own** rows' ids, in **its own** submitted row order.
///
/// A window that returned the right multiset in the wrong order is a silent misattribution: the
/// handler turns each id into the `tessera_id` it hands back per row (contracts §3.4), so a
/// permuted answer tells a client that row 3 is the entity that is really row 7 — and every later
/// deny it issues names the wrong item.
#[test]
fn each_waiter_gets_its_own_ids() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 64);
    let engine = Arc::new(engine);
    let low = engine.resolve_terms(&[b"0".to_vec()]);
    let high = engine.resolve_terms(&[b"1".to_vec()]);

    // Deliberately ragged: different row counts, so a implementation that scattered by a fixed
    // stride rather than by (entry, row) position cannot pass by coincidence.
    let batches: Vec<(String, Vec<UnallocatedRow>)> = vec![
        ("a".to_string(), sig_rows("a", 3, &low, &high)),
        ("b".to_string(), sig_rows("b", 1, &low, &high)),
        ("c".to_string(), sig_rows("c", 4, &low, &high)),
    ];

    let window = Window::park(Arc::clone(&engine), faults);
    let lo = window.high_water_before;
    let got = window.run(batches.clone());
    let ids: Vec<Vec<u64>> = got
        .iter()
        .map(|r| r.as_ref().unwrap().iter().map(|e| e.raw()).collect())
        .collect();

    assert_eq!(
        ids,
        expected_assignment(&batches, lo),
        "each waiter's ids must be its own rows', in its own row order"
    );

    // And the live map agrees, which is what a later deny resolves through.
    for (b, (_, rows)) in batches.iter().enumerate() {
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(
                engine
                    .resolve_external_id(r.external_id.as_ref().unwrap())
                    .unwrap()
                    .map(|e| e.raw()),
                Some(ids[b][i]),
                "the external id must resolve to the id its own caller was told"
            );
        }
    }
}

/// **The `ReceiptLost` widening**: a window swaps once and then acks N waiters in a loop, so a
/// death partway through the loop leaves some waiters acked and some not.
///
/// Every un-acked waiter must get `SubmitError::ReceiptLost` → **500**, never
/// `SubmitError::ExecutorDead` → 503. Their ingest is durably in force by then — appended, fsynced,
/// applied and swapped — and 503's meaning is "this node did not take your write".
/// `an_executor_panic_is_reported_dead` covers a single command's shape; this is the partial case.
///
/// **Both halves are asserted, and the second is why.** "Every waiter got `ReceiptLost`" is
/// satisfied by an executor that panicked before acking *anybody* — including by a `fire_after`
/// that does not work — so the test would pass while its own subject never occurred. At least one
/// waiter must have been acked for this to be a partially-acked window at all.
///
/// Deterministic: arrival 1 at `BeforeAck` is the priming submission's own ack, arrival 2 is the
/// window's first waiter, and `fire_after = 2` panics on the second. `release_site` releases the
/// `AfterFsync` park **without** disarming `BeforeAck`, which a release-all would.
#[test]
fn every_unacked_waiter_in_a_partially_acked_window_gets_receipt_lost() {
    const N: usize = 3;
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 64);
    let engine = Arc::new(engine);

    faults.arm_pause(PauseSite::AfterFsync, PauseAction::Stall);
    let e = Arc::clone(&engine);
    let prime = std::thread::spawn(move || {
        e.accept_ingest(vec![row("prime")], "prime".to_string(), [0xEE; 32])
    });
    faults.await_arrivals(PauseSite::AfterFsync, 1, WAIT);

    let mut handles = Vec::new();
    for i in 0..N {
        let e = Arc::clone(&engine);
        handles.push(std::thread::spawn(move || {
            e.accept_ingest(
                vec![row(&format!("lost-{i}"))],
                format!("lost-{i}"),
                [i as u8; 32],
            )
        }));
    }
    let deadline = std::time::Instant::now() + WAIT;
    while engine.write_executor_stats().work_submitted < (N + 1) as u64 {
        assert!(std::time::Instant::now() < deadline, "never enqueued");
        std::thread::yield_now();
    }

    // Armed BEFORE the release, or the window's first ack races the arming.
    faults.arm_pause_after(PauseSite::BeforeAck, PauseAction::Panic, 2);
    faults.release_site(PauseSite::AfterFsync);

    prime.join().unwrap().expect("the priming submission acks");

    let mut acked = 0;
    let mut lost = 0;
    for h in handles {
        match h.join().unwrap() {
            Ok(_) => acked += 1,
            Err(AcceptError::Submit(SubmitError::ReceiptLost)) => lost += 1,
            other => panic!(
                "a waiter in a window that swapped must be acked or told its receipt was LOST — \
                 `ExecutorDead` would report a durable, in-force ingest as 'nothing happened'. \
                 Got: {other:?}"
            ),
        }
    }
    assert_eq!(
        acked, 1,
        "the window must have been PARTIALLY acked — otherwise this test has not constructed its \
         own subject and would pass against an executor that acked nobody"
    );
    assert_eq!(lost, N - 1, "and every remaining waiter is `ReceiptLost`");
}

/// The duplicate-external-id backstop **survives the window**, which is the one place a naive
/// implementation re-opens it.
///
/// Both retries reach the executor while the window is open, so neither can see the other in the
/// live map — that map is written at *apply*. Without the window's own conflict check they would
/// both be admitted, both allocate, and the second `established.insert` would overwrite the first:
/// the first item stays visible, byte-identical to the second, and reachable by **no external id at
/// all**, so no deny could ever name it. The remedy is a forced close, after which the ordinary
/// check answers.
#[test]
fn a_duplicate_external_id_across_one_window_is_still_refused() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 64);
    let engine = Arc::new(engine);

    let window = Window::park(Arc::clone(&engine), faults);
    let got = window.run(vec![
        ("first".to_string(), vec![row("same-key")]),
        // A *fresh* batch id, as a client retry after a timeout produces — the idempotency index
        // cannot catch it, so only the external-id backstop can.
        ("retry".to_string(), vec![row("same-key")]),
    ]);

    let accepted = got.iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        accepted, 1,
        "exactly one of two submissions naming one external id may be accepted, however they are \
         batched: got {got:?}"
    );
    let refused = got
        .iter()
        .find(|r| r.is_err())
        .unwrap()
        .as_ref()
        .unwrap_err();
    assert!(
        refused.contains("DuplicateExternalId"),
        "and the refusal must be the duplicate, not something incidental: {refused}"
    );

    // A refused entry still occupied a queue slot, so it must be counted as completed. Otherwise
    // `work_depth` — the operand of every ingest 429's `retry_after_s` — drifts upward by one per
    // refusal and never comes back down.
    let stats = engine.write_executor_stats();
    assert_eq!(
        (stats.work_submitted, stats.work_completed),
        (3, 3),
        "every job taken off the work queue must be counted completed, refusals included: {stats:?}"
    );
    assert_eq!(stats.work_depth, 0);
}

/// A refused command that is not an ingest is counted completed once, as an accepted one is.
#[test]
fn a_refused_view_create_is_counted_completed_once() {
    let tmp = TempDir::new().unwrap();
    let (engine, _faults) = engine_with_faults(&tmp, 64);

    let refused = engine.create_view(
        "no-such-group".to_string(),
        "k".to_string(),
        None,
        Default::default(),
    );
    assert!(refused.is_err());

    // The executor counts the job after it has answered, so wait for the count and then give a
    // second one time to land.
    let deadline = std::time::Instant::now() + WAIT;
    while engine.write_executor_stats().work_completed == 0 {
        assert!(std::time::Instant::now() < deadline, "the job was never counted");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    let stats = engine.write_executor_stats();
    assert_eq!((stats.work_submitted, stats.work_completed), (1, 1));
}

/// **An empty window is never opened**, and the in-flight gauge is armed at the first entry rather
/// than at window construction.
///
/// The executor reaches its work pass with an empty queue on every iteration that a deny woke it
/// for, and on every spurious doorbell token. A gauge armed at construction would be armed there
/// and cleared by nothing — and `ExecutorStats::service_nanos_for_estimate` takes
/// `max(ewma, in-flight elapsed)`, so an idle node would answer every later 429 with a
/// `retry_after_s` that grows without bound towards the 300 s clamp. That is the correction
/// running backwards.
#[test]
fn an_idle_work_pass_arms_nothing() {
    let tmp = TempDir::new().unwrap();
    let (engine, _faults) = engine_with_faults(&tmp, 8);

    // A deny wakes the executor; its work pass then finds an empty queue.
    let entity = entity_of(&engine, 3);
    engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect("the suppression is applied");

    let stats = engine.write_executor_stats();
    assert_eq!(
        stats.work_in_flight_nanos, 0,
        "the executor is idle and no work item is in flight: {stats:?}"
    );
    assert_eq!(stats.work_depth, 0);
    assert_eq!(
        stats.wal_fsyncs, 1,
        "and an empty window costs no fsync of its own"
    );
}

// =================================================================================================
// The batch-id state machine across a held window
// =================================================================================================

/// Two anonymous rows: **no external id**, so no map can catch a double-ingest and the batch-id
/// path is the only thing under test. It is also the case the recorded-ids design exists for —
/// nothing could re-derive these ids from anything a retry sends.
fn anonymous_pair() -> Vec<UnallocatedRow> {
    (0..2)
        .map(|_| {
            let mut r = row("ignored");
            r.external_id = None;
            r
        })
        .collect()
}

/// Submit one ingest on its own thread and wait until it is **enqueued**, so the drain order is
/// deterministic rather than a race between threads. `Window::run` cannot be reused for these
/// cases: it spawns every submission at once and derives each body hash from the submission index
/// (`let hash = [i as u8; 32]`), so it cannot express a byte-identical retry at all.
fn enqueue(
    engine: &Arc<Engine>,
    batch_id: &str,
    body_hash: [u8; 32],
    rows: Vec<UnallocatedRow>,
    want_submitted: u64,
) -> std::thread::JoinHandle<Result<Vec<EntityId>, AcceptError>> {
    let e = Arc::clone(engine);
    let id = batch_id.to_string();
    let handle = std::thread::spawn(move || e.accept_ingest(rows, id, body_hash));
    let deadline = std::time::Instant::now() + WAIT;
    while engine.write_executor_stats().work_submitted < want_submitted {
        assert!(
            std::time::Instant::now() < deadline,
            "submission {want_submitted} never reached the queue: {:?}",
            engine.write_executor_stats()
        );
        std::thread::yield_now();
    }
    handle
}

/// **A byte-identical retry that lands in a held window JOINS it** (contracts §3.4 r8's
/// third state, lifecycle §5.1: "a retry must join the open window rather than allocate a second
/// time").
///
/// **What discriminates the join, and why the obvious assertions do not.** The alternative — a
/// held `batch_id` forcing the window to close, so the retry is answered from the
/// durable index — is also correct: same ids, one WAL record, high-water up by the
/// batch's rows once. Those three assertions pass on both and prove nothing about the
/// join. What the close costs, and the join does not, is a **second window**: the pass yields after
/// a conflict, so `A, A', C` committed in two windows and two fsyncs where the join commits one.
/// The `wal_fsyncs` leg is therefore the load-bearing one, and it is `== 1` rather than "did not
/// rise" because a build that never fsynced would satisfy the weaker form.
#[test]
fn a_retry_joins_rather_than_reallocating() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 64);
    let engine = Arc::new(engine);

    let window = Window::park(Arc::clone(&engine), faults);
    let before = window.high_water_before;
    let fsyncs_before = engine.write_executor_stats().wal_fsyncs;

    // `A` and `A'`: same batch id, same body hash. `C`: a fresh batch, which is what makes the
    // fsync count discriminating — without it there is nothing left in the pass to share a window
    // with, and both shapes commit one window.
    let a = enqueue(&engine, "same-batch", [9; 32], anonymous_pair(), 2);
    let retry = enqueue(&engine, "same-batch", [9; 32], anonymous_pair(), 3);
    let c = enqueue(&engine, "other-batch", [7; 32], vec![row("c-0")], 4);

    window.faults.release();
    let first = a.join().unwrap().expect("accepted");
    let second = retry
        .join()
        .unwrap()
        .expect("the retry is answered, not refused");
    let third = c.join().unwrap().expect("accepted");
    window.join_prime();

    assert_eq!(
        first, second,
        "a byte-identical retry must answer with the SAME ids (contracts §3.4), not a second \
         allocation"
    );
    assert_eq!(first.len(), 2);
    assert_eq!(
        engine.allocator_high_water(),
        before + 3,
        "the id space must advance by A's two rows and C's one — not by A's rows twice"
    );

    let stats = engine.write_executor_stats();
    assert_eq!(
        stats.wal_fsyncs - fsyncs_before,
        1,
        "the retry must JOIN the open window, not split the pass into two windows and two \
         fsyncs: {stats:?}"
    );
    assert_eq!(
        stats.work_depth, 0,
        "a joined retry occupied a queue slot and must be counted completed, or `work_depth` \
         drifts up one per retry forever and every 429's retry_after_s inherits it: {stats:?}"
    );
    // And the join added a waiter, not an entry: two records for two batches, not three.
    assert_eq!(
        stats.wal_appends - 1,
        2,
        "one record per ENTRY — the priming batch aside, A and C, with no record for the retry: \
         {stats:?}"
    );
    assert!(!third.is_empty());
}

/// The child half of [`crash_between_fsync_and_swap_replays_rather_than_reallocates`] runs this
/// same test binary with this variable set to the scratch directory to work in.
const CRASH_CHILD_DIR: &str = "TESSERA_TASK8_CRASH_CHILD_DIR";
const CRASH_BATCH_ID: &str = "crash-batch";
const CRASH_BODY_HASH: [u8; 32] = [5; 32];

/// **A process killed between fsync and swap replays its batch; a retry does not reallocate.**
///
/// Lifecycle §8's crash row ("after fsync, before swap", at risk: none) and contracts §3.4's
/// idempotency rule, met across a **real** process death.
///
/// **Why a child process, and why no in-process fault can stand in.** `faults.rs`'s `PauseAction`
/// carries exactly one kill action, `Panic`, and its own doc says why: a panic unwinds, runs
/// `DeathGuard`, drops the `Job` and closes the `ExecutorWal`, and a `SIGKILL` does none of those.
/// There is deliberately no `Abort` variant to arm — it could only be a second `panic!` with a
/// different message, which would model a clean shutdown and call it a crash — so this test builds
/// the real thing.
///
/// The assembly, with no sleep anywhere: the child builds a fixture, arms `AfterFsync`/`Stall`,
/// submits one batch from a thread and waits on the switchboard's own arrival counter — at which
/// point the record is **durable and the swap has not happened** — then creates a marker file and
/// blocks forever. The parent polls for the marker (deadline-bounded, like every other wait in this
/// file), `SIGKILL`s the child, and reopens the WAL.
///
/// The expected ids are read **from the WAL**, not from the child: the child never acknowledged,
/// which is the whole property. `WritePath::reconstruct` rebuilds `accepted_batches` from the
/// replayed `IngestBatch` records, so the parent's retry meets `BatchState::Accepted` and is
/// replayed rather than re-ingested.
#[test]
fn crash_between_fsync_and_swap_replays_rather_than_reallocates() {
    match std::env::var(CRASH_CHILD_DIR) {
        Ok(dir) => crash_child(std::path::PathBuf::from(dir)),
        Err(_) => crash_parent(),
    }
}

/// The child: get one batch durable, publish that fact, and wait to be killed. Never returns.
fn crash_child(dir: std::path::PathBuf) {
    let bundle_root = dir.join("bundle");
    build_fixture(
        &bundle_root,
        &dir.join("points.parquet"),
        &dir.join("pairs.parquet"),
    );
    let mut engine = open_engine(&bundle_root, &dir.join("cache"), &dir.join("wal.log"));
    let faults = Arc::new(FaultSwitchboard::new());
    engine
        .start_write_executor_with_faults(8, Arc::clone(&faults))
        .expect("the executor starts once");

    faults.arm_pause(PauseSite::AfterFsync, PauseAction::Stall);
    let e = Arc::new(engine);
    let submitter = Arc::clone(&e);
    std::thread::spawn(move || {
        let _ = submitter.accept_ingest(
            vec![row("crash-0"), row("crash-1")],
            CRASH_BATCH_ID.to_string(),
            CRASH_BODY_HASH,
        );
    });
    faults.await_arrivals(PauseSite::AfterFsync, 1, WAIT);

    // Durable, not in force, not acknowledged. Say so, then wait to die — `std::mem::forget` on the
    // engine so that even an unexpected unwind cannot run the executor's drop path.
    std::mem::forget(e);
    std::fs::write(dir.join("parked"), b"parked").expect("the marker is written");
    loop {
        std::thread::park();
    }
}

/// Kills the child on the way out, so a failing assertion cannot leak a process that outlives the
/// `TempDir` it is holding open.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn crash_parent() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    let child = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .arg("crash_between_fsync_and_swap_replays_rather_than_reallocates")
        .arg("--exact")
        .arg("--nocapture")
        .env(CRASH_CHILD_DIR, &dir)
        .spawn()
        .expect("the child test process starts");
    let mut child = ChildGuard(child);

    let marker = dir.join("parked");
    let deadline = std::time::Instant::now() + WAIT;
    while !marker.exists() {
        if let Some(status) = child.0.try_wait().expect("the child is waitable") {
            panic!("the child exited before parking: {status}");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the child never reached the after-fsync pause point"
        );
        std::thread::yield_now();
    }

    // **A real kill**: SIGKILL, no unwinding, no destructors, no WAL close.
    child.0.kill().expect("the child is killable");
    let status = child.0.wait().expect("the child is waitable");
    assert!(
        !status.success(),
        "the child must have died by signal, not exited cleanly: {status}"
    );

    let wal_path = dir.join("wal.log");
    let durable = crash_batch_ids(&wal_path);
    assert_eq!(
        durable.len(),
        2,
        "the killed process's batch must be durable — fsync returned before it was killed"
    );

    // Reopen: replay reinstates the rows AND the idempotency index.
    let mut engine = open_engine(&dir.join("bundle"), &dir.join("cache2"), &wal_path);
    engine
        .start_write_executor(8)
        .expect("the executor starts once");
    let high_water_after_replay = engine.allocator_high_water();

    let replayed = engine
        .accept_ingest(
            vec![row("crash-0"), row("crash-1")],
            CRASH_BATCH_ID.to_string(),
            CRASH_BODY_HASH,
        )
        .expect("the retry is answered, not refused");

    assert_eq!(
        replayed, durable,
        "the retry must replay the ids the crashed process's WAL record carries — not allocate a \
         second set for rows that are already durable"
    );
    assert_eq!(
        engine.allocator_high_water(),
        high_water_after_replay,
        "and it must burn no entity ids"
    );
    drop(engine);

    assert_eq!(
        crash_batch_ids(&wal_path).len(),
        2,
        "the replay must append no second record for this batch id"
    );
}

/// The entity ids the WAL's `IngestBatch` record for [`CRASH_BATCH_ID`] carries, in row order.
fn crash_batch_ids(wal_path: &std::path::Path) -> Vec<EntityId> {
    let (_wal, records) = tessera_lifecycle::Wal::open(wal_path).expect("the WAL reopens");
    records
        .iter()
        .filter_map(|r| match r {
            tessera_lifecycle::WalRecord::IngestBatch { batch_id, rows, .. }
                if batch_id == CRASH_BATCH_ID =>
            {
                Some(rows.iter().map(|row| row.entity_id))
            }
            _ => None,
        })
        .flatten()
        .collect()
}

/// **A retry of a held batch with different bytes 409s, and the held original is undisturbed.**
///
/// The reading taken: contracts §3.4's "the batch has no effect" reaches the refused retry and not
/// the accepted original — and it is asserted where it has to be, on the *original's* outcome.
/// Asserting only that the retry 409s would pass equally well on a build that threw the original
/// away.
///
/// "In force" for a buffered ingest is the **live external-id map**: buffered rows
/// have no geometry, there being no flush (⊘), so `visible()` cannot see them and
/// `resolve_external_id` is the observable.
#[test]
fn held_plus_different_bytes_409s_without_disturbing_the_original() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 64);
    let engine = Arc::new(engine);

    let window = Window::park(Arc::clone(&engine), faults);
    let before = window.high_water_before;
    let fsyncs_before = engine.write_executor_stats().wal_fsyncs;

    let a = enqueue(&engine, "same-batch", [1; 32], vec![row("held-0")], 2);
    let retry = enqueue(&engine, "same-batch", [2; 32], vec![row("held-1")], 3);
    let c = enqueue(&engine, "other-batch", [7; 32], vec![row("c-0")], 4);

    window.faults.release();
    let original = a.join().unwrap().expect("the held original still applies");
    let refused = retry.join().unwrap();
    c.join().unwrap().expect("accepted");
    window.join_prime();

    assert!(
        matches!(
            refused,
            Err(AcceptError::Exec(
                tessera_lifecycle::ExecError::BatchConflict { .. }
            ))
        ),
        "a held batch id with different bytes is a 409, not an acceptance: {refused:?}"
    );
    assert_eq!(original.len(), 1);
    assert_eq!(
        engine
            .resolve_external_id(b"held-0")
            .expect("resolve")
            .expect("the original's row is established"),
        original[0],
        "the original is in force with the ids it was acked"
    );
    assert_eq!(
        engine.resolve_external_id(b"held-1").expect("resolve"),
        None,
        "and the refused retry's rows are not — a 409 batch has no effect"
    );
    assert_eq!(
        engine.allocator_high_water(),
        before + 2,
        "the refused retry burns no entity ids"
    );

    let stats = engine.write_executor_stats();
    assert_eq!(
        stats.wal_fsyncs - fsyncs_before,
        1,
        "and the 409 does not close the window either: {stats:?}"
    );
    assert_eq!(
        stats.work_depth, 0,
        "the 409'd retry must be counted completed too: {stats:?}"
    );
}

/// Every id a window hands out is in the WAL — read back from the file, not from the executor.
///
/// Replay reuses the ids the records carry (lifecycle §5.1, SA §6.2), so an id acked but never
/// framed is an entity that exists for exactly as long as the process does, and whose id is handed
/// out again after a restart.
#[test]
fn every_id_a_window_issues_is_in_the_wal() {
    let tmp = TempDir::new().unwrap();
    let wal_path = tmp.path().join("wal.log");
    let acked: Vec<u64> = {
        let bundle_root = tmp.path().join("bundle");
        build_fixture(
            &bundle_root,
            &tmp.path().join("points.parquet"),
            &tmp.path().join("pairs.parquet"),
        );
        let mut engine = open_engine(&bundle_root, &tmp.path().join("cache"), &wal_path);
        let faults = Arc::new(FaultSwitchboard::new());
        engine
            .start_write_executor_with_faults(64, Arc::clone(&faults))
            .unwrap();
        let engine = Arc::new(engine);

        let window = Window::park(Arc::clone(&engine), faults);
        let batches: Vec<(String, Vec<UnallocatedRow>)> = (0..4)
            .map(|i| {
                (
                    format!("w{i}"),
                    vec![row(&format!("x{i}")), row(&format!("y{i}"))],
                )
            })
            .collect();
        let mut ids: Vec<u64> = window
            .run(batches)
            .into_iter()
            .flat_map(|r| r.expect("accepted").into_iter().map(|e| e.raw()))
            .collect();
        ids.sort_unstable();
        ids
    }; // the engine is dropped, which joins the executor and closes the WAL

    let (_wal, records) = tessera_lifecycle::Wal::open(&wal_path).expect("the WAL reopens");
    let mut framed: Vec<u64> = Vec::new();
    for record in &records {
        if let tessera_lifecycle::WalRecord::IngestBatch { rows, .. } = record {
            framed.extend(rows.iter().map(|r| r.entity_id.raw()));
        }
    }
    framed.sort_unstable();
    for id in &acked {
        assert!(
            framed.contains(id),
            "id {id} was acknowledged to a caller and is in no WAL record: replay would hand it \
             out again. Framed: {framed:?}"
        );
    }
}

/// **A window's acks follow its established-map insert** — the ingest half of
/// `ack_follows_fsync_then_swap`, which covers a change.
///
/// **What it asserts, exactly, and what it does not.** The witness is engine state, not a step log:
/// the executor is parked *inside* `Executor::ack`, one statement before the send, and
/// `resolve_external_id` answers. That reads `LiveState::established`, which `Executor::apply_window`
/// writes *before* it calls `publish` — so what is proven is **insert-precedes-ack**, not
/// swap-precedes-ack. **A stronger witness is unavailable**: nothing a reader can observe
/// distinguishes the two, because buffered rows have no geometry without a flush (⊘) and
/// `visible()` therefore cannot see the ingest at all under either ordering. When a flush lands,
/// this leg should read the swapped-in generation instead.
///
/// The fail-open it does catch: a window that acked its N waiters before establishing them. Every
/// caller holds ids whose entities no `/control/changes` lookup can resolve yet, so a suppression
/// issued immediately after a 200 is answered 404 for an item that exists. It is the first test in
/// the tree to assert ingest ack-ordering at all.
#[test]
fn a_windows_acks_follow_its_swap() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 8);
    let engine = Arc::new(engine);

    faults.arm_pause(PauseSite::BeforeAck, PauseAction::Stall);
    let e = Arc::clone(&engine);
    let submit = std::thread::spawn(move || {
        e.accept_ingest(vec![row("in-force")], "in-force".to_string(), [3; 32])
    });
    faults.await_arrivals(PauseSite::BeforeAck, 1, WAIT);

    assert!(
        engine.resolve_external_id(b"in-force").unwrap().is_some(),
        "the executor is parked one statement before the ack: the window it is about to \
         acknowledge MUST already be established. Seeing nothing here means the acks ran with the \
         apply still ahead of them — lifecycle §4's ack-ordering fail-open, one window wide"
    );

    faults.release();
    submit.join().unwrap().expect("the ingest is accepted");
}

/// **The deny window closes at its bound**, so the drain cannot run forever.
///
/// [`tessera_engine::DENY_WINDOW_MAX_ENTRIES`] is what stops a window growing for as long as denies
/// keep arriving — and the failure it prevents is not "a large window" but a drain that never
/// returns, so no deny is ever acked at all. The executor is parked inside a priming submission so
/// the whole batch is provably queued before anything drains; the drain then has to produce two
/// windows for `bound + 1` entries and one for a bound that is not enforced.
///
/// **What this pins and what it does not.** It pins the *close*: the bound fires and splits the
/// window. It does **not** pin the live-lock the bound exists for, which needs sustained concurrent
/// enqueueing against a draining executor — a race, not a test. Same distinction as
/// `ingest_429s_when_the_queue_is_full`, which pins its mapping rather than the transition into
/// fullness.
///
/// One entity is suppressed `bound + 1` times rather than `bound + 1` entities being suppressed
/// once. The subject is the window's size, and each submission is its own command and its own WAL
/// record either way; using one entity keeps the fixture from having to be `bound + 1` items wide.
#[test]
fn the_deny_window_closes_at_its_bound() {
    let bound = tessera_engine::DENY_WINDOW_MAX_ENTRIES;
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 64);
    let entity = entity_of(&engine, 3);

    // Park the executor after the priming submission's fsync: nothing can drain the deny lane
    // while it is stalled there, so every enqueue below is provably still queued.
    faults.arm_pause(PauseSite::AfterFsync, PauseAction::Stall);
    let engine = Arc::new(engine);
    let e = Arc::clone(&engine);
    let prime =
        std::thread::spawn(move || e.accept_ingest(vec![row("prime")], "prime".into(), [0xEE; 32]));
    faults.await_arrivals(PauseSite::AfterFsync, 1, WAIT);

    let parked = engine.write_executor_stats();
    let pending: Vec<_> = (0..bound + 1)
        .map(|_| {
            engine
                .submit_change(entity, ChangeOp::Suppress)
                .expect("the deny lane is unbounded and never refuses for load")
        })
        .collect();

    faults.release();
    for p in pending {
        p.wait().expect("every deny is applied and durable");
    }
    prime.join().unwrap().expect("the priming submission");

    let after = engine.write_executor_stats();
    assert_eq!(
        after.wal_appends - parked.wal_appends,
        (bound + 1) as u64,
        "one record per deny, whatever the window count"
    );
    assert_eq!(
        after.wal_fsyncs - parked.wal_fsyncs,
        2,
        "{} queued denies must close TWO windows — one at the bound and one for the remainder. \
         One fsync means the bound is not enforced and the drain's length is the arrival rate",
        bound + 1
    );
}

// =================================================================================================
// The fragmentation figure — the executor's half
// =================================================================================================

/// **The counters `/control/status` publishes are fed by real window closes, and they move with the
/// window size.**
///
/// The arithmetic itself is proved at window scope in `tessera-lifecycle`'s `window_props.rs`,
/// where a corpus can be assigned three ways with no WAL in the loop. What only this level can show
/// is the **wiring**: that `Executor::close_window` folds each allocation's tally into
/// `ExecutorHealth`, and that the numbers a reader gets off `ExecutorStats` therefore describe the
/// windows this executor actually closed.
///
/// Two engines over the same rows. One takes them as a single submission — one window of `ROWS`
/// rows. The other has `set_commit_window_max_rows(1)`, the documented way to turn group commit off
/// (`ingest.commit_window_max_items = 1`), and submits them one at a time, so every window holds
/// one row.
///
/// **Two terms per row, and `k < W`, deliberately.** With one term per row a term's `k` equals its
/// window's `W`, where the baseline `k·(W − k + 1)/W` is 1 and a perfect run is 1 — so both arms
/// would report exactly 1.0 and every assertion here would hold while measuring nothing.
///
/// `ROWS` is small because the one-row arm pays a WAL fsync per submission; the magnitudes are the
/// other file's subject, and what is asserted here is direction and provenance.
#[test]
fn the_fragmentation_counters_are_fed_by_window_closes_and_move_with_the_window() {
    const ROWS: usize = 40;

    fn corpus() -> Vec<UnallocatedRow> {
        (0..ROWS)
            .map(|i| {
                let s = (i as u32 * 7) % 8;
                let mut r = row(&format!("frag-{i:03}"));
                r.terms = vec![
                    tessera_types::TermId::new(s),
                    tessera_types::TermId::new((s + 1) % 8),
                ];
                r
            })
            .collect()
    }

    // --- One window of ROWS rows ------------------------------------------------------------
    let tmp_one = TempDir::new().expect("tempdir");
    let (engine_one, _) = engine_with_faults(&tmp_one, 64);
    let before = engine_one.write_executor_stats();
    assert_eq!(
        before.fragmentation_windows, 0,
        "nothing has been ingested yet"
    );
    assert!(
        before.run_ratio().is_none() && before.postings_per_container().is_none(),
        "both ratios must be absent, not zero, before any window has closed — a zero is a value \
         of this quantity and would read as a measurement"
    );

    engine_one
        .accept_ingest(corpus(), "one".to_string(), [1u8; 32])
        .expect("the batch is accepted");
    let one = engine_one.write_executor_stats();

    // --- ROWS windows of one row each -------------------------------------------------------
    let tmp_many = TempDir::new().expect("tempdir");
    let (engine_many, _) = engine_with_faults(&tmp_many, 64);
    engine_many.set_commit_window_max_rows(1);
    for (i, r) in corpus().into_iter().enumerate() {
        engine_many
            .accept_ingest(vec![r], format!("many-{i}"), [i as u8; 32])
            .expect("the batch is accepted");
    }
    let many = engine_many.write_executor_stats();

    // Provenance: the counters came from window closes, and the two arms closed different numbers
    // of windows over the same rows.
    assert_eq!(one.fragmentation_windows, 1, "one submission, one window");
    assert_eq!(
        many.fragmentation_windows, ROWS as u64,
        "grouping disabled closes one window per row"
    );
    assert_eq!(
        one.fragmentation.rows, ROWS as u64,
        "every row is counted exactly once"
    );
    assert_eq!(many.fragmentation.rows, ROWS as u64);
    assert_eq!(
        one.fragmentation.postings, many.fragmentation.postings,
        "the same rows carry the same postings however they were windowed — only the RUNS differ"
    );

    // And the figure itself.
    let (one_ratio, many_ratio) = (
        one.run_ratio().expect("a window has closed"),
        many.run_ratio().expect("windows have closed"),
    );
    println!(
        "executor-level run_ratio: one {ROWS}-row window {one_ratio:.2} ({} runs), \
         {ROWS} one-row windows {many_ratio:.2} ({} runs)",
        one.fragmentation.runs, many.fragmentation.runs
    );
    assert_eq!(
        many_ratio, 1.0,
        "a one-row window has baseline 1 and one run, for every corpus — the reading is 'group \
         commit disabled collects nothing', not a measurement"
    );
    assert!(
        one_ratio > 4.0,
        "one window over the whole corpus must collect materially more run length: {one_ratio}"
    );
    assert!(
        one.fragmentation.runs < many.fragmentation.runs,
        "the un-normalised half of the same statement: {} vs {}",
        one.fragmentation.runs,
        many.fragmentation.runs
    );
}

/// **A poisoned node publishes no deny state.** The dispositions it is still applying were
/// answered 500 under the apply-anyway rule, and contracts §3.1's residual is that a restart
/// drops them — so a side-manifest carrying them would make a never-acked deny permanent on every
/// restore, which is the residual inverted.
///
/// Asserted on the filesystem, because the gate's whole job is that nothing is written.
#[test]
fn a_poisoned_node_writes_no_side_manifest_for_its_denies() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 8);
    let root = tmp.path().join("bundle");
    let manifests = || {
        let dir = root.join("v00000/partitions/default");
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("SEGMENTS-") && n.ends_with(".json"))
            .collect();
        names.sort();
        names
    };

    let before = manifests();
    faults.fail_next_appends(1);
    let doomed = entity_of(&engine, 3);
    let _ = engine.accept_change(doomed, ChangeOp::Suppress);
    assert_eq!(
        engine.write_executor_posture(),
        ExecutorPosture::WalPoisoned
    );

    // A second deny, applied in memory under the same rule, with every chance to publish.
    let second = entity_of(&engine, 6);
    let _ = engine.accept_change(second, ChangeOp::Suppress);
    std::thread::sleep(std::time::Duration::from_millis(200));

    assert_eq!(
        manifests(),
        before,
        "a poisoned node writes no side-manifest: its overlay holds dispositions no durable \
         record backs, and publishing them would survive a restart that is supposed to drop them"
    );
    assert_eq!(
        engine.write_executor_stats().overlay_publications,
        0,
        "and the gauge agrees it published nothing"
    );
}

/// **A refused fold is counted and named, and it moves neither counter beside it.**
///
/// This is the gap the gauge closes. `plan_fold` answers six conditions and a refusal advances
/// nothing: `folds` does not, because nothing was folded, and `fold_failures` does not, because a
/// refusal is not a failure. A deployment can therefore ask for compaction on every schedule and
/// never get it while both counters sit still — which matters most for `insufficient_disc`, since
/// the fold is also the only operation that reclaims (compaction §8) and the device does not
/// recover on its own.
///
/// The contrived refusal here is `wal_poisoned`, because it is the one a test can produce
/// deterministically: a torn append is terminal, so the gate is a stable value rather than a race
/// (see `a_torn_wal_stays_poisoned_and_still_applies_denies`). What is asserted is the plumbing —
/// counted, named, and neither of the other two touched — which is the same for every gate.
#[test]
fn a_refused_fold_is_counted_and_named_while_neither_fold_counter_moves() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 8);

    let before = engine.write_executor_stats();
    assert_eq!(before.fold_refusals, 0, "nothing has been refused yet");
    assert_eq!(before.fold_refusals_by_gate, [0; tessera_engine::FOLD_GATES.len()]);
    assert_eq!(before.last_fold_refusal, None);

    faults.fail_next_appends(1);
    let doomed = entity_of(&engine, 3);
    let _ = engine.accept_change(doomed, ChangeOp::Suppress);
    assert_eq!(
        engine.write_executor_posture(),
        ExecutorPosture::WalPoisoned
    );

    engine.request_fold();
    let deadline = std::time::Instant::now() + WAIT;
    let stats = loop {
        let now = engine.write_executor_stats();
        if now.fold_refusals > 0 {
            break now;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the requested fold was neither planned nor recorded as refused"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };

    let refusal = stats
        .last_fold_refusal
        .expect("a counted refusal carries the reason it was counted for");
    assert_eq!(refusal.gate, "wal_poisoned");
    assert_eq!(
        (refusal.need_bytes, refusal.had_bytes),
        (None, None),
        "this gate compares no figures, so it publishes none"
    );
    assert_eq!(
        stats.folds, 0,
        "nothing was folded, so the published-fold counter must not move"
    );
    assert_eq!(
        stats.fold_failures, 0,
        "and a refusal is not a failure: no prefix was written and nothing was discarded, so \
         charging it to fold_failures would put an orphaned-prefix alarm on a fold that never ran"
    );

    // **The gate it was counted under, and the five it was not.** One total says a fold was
    // refused; it does not say which refusal is standing, and on a node whose gate re-fires every
    // tick `last_refusal` is whatever refused most recently rather than what is holding.
    let slot = tessera_engine::FOLD_GATES
        .iter()
        .position(|gate| *gate == "wal_poisoned")
        .expect("the gate a refusal names is one of the six the counters are keyed by");
    assert_eq!(
        stats.fold_refusals_by_gate[slot], stats.fold_refusals,
        "every refusal so far was this gate's, so its counter carries the whole total"
    );
    assert_eq!(
        stats.fold_refusals_by_gate.iter().sum::<u64>(),
        stats.fold_refusals,
        "the total is the sum of the six, so the two cannot disagree"
    );
}

/// **The WAL gauge is walked once a period, not once a tick.**
///
/// The tick is not a cadence the gauge can ride: `flush_max_items` makes a tick due for as long as
/// the buffer stays full, and a loader the flush cannot keep up with therefore ticks once per
/// completed flush, many times a second. The walk is two `stat`s per surviving member,
/// and the member count is unbounded under exactly the pin the gauge exists to report, so an
/// unlimited sample gets dearer as the condition gets worse.
///
/// What is asserted here is the limit holding: ticks advance, the log grows under it, and the
/// published reading does not move. That it expires is asserted in `artifact_growth.rs`, whose
/// gauge case ticks until the walk runs again.
#[test]
fn the_wal_gauge_is_walked_once_a_period_and_not_once_a_tick() {
    let tmp = TempDir::new().unwrap();
    let (engine, _faults) = engine_with_faults(&tmp, 8);

    // The entry sample is taken on the executor thread, which `start_write_executor` does not wait
    // for, so this waits for it rather than assuming it has already happened.
    wait_until("the executor's entry sample", || {
        engine.write_executor_stats().wal.samples > 0
    });
    let first = engine.write_executor_stats().wal;
    assert_eq!(
        first.samples, 1,
        "the executor's first loop iteration takes one, so a node in its first period reports \
         the log it replayed rather than an unsampled zero"
    );

    // Denies append, so a walk taken after these would read a position the entry sample could not
    // have seen.
    for source in 0..8u64 {
        let entity = entity_of(&engine, source);
        engine
            .accept_change(entity, ChangeOp::Suppress)
            .expect("a healthy node accepts a suppression");
    }

    let ticks_before = engine.write_executor_stats().ticks;
    for _ in 0..20 {
        tick(&engine);
    }

    let after = engine.write_executor_stats();
    assert!(
        after.ticks >= ticks_before + 20,
        "the twenty ticks ran: {} to {}",
        ticks_before,
        after.ticks
    );
    assert!(
        after.wal_appends >= 8,
        "and the log took the eight denies: {} appends",
        after.wal_appends
    );
    assert_eq!(
        after.wal, first,
        "the reading is the entry sample still, unchanged through twenty ticks and eight appends \
         — the walk is bounded by the tick period and the tick is not"
    );
}

// =================================================================================================
// The deny-publication liveness floor
// =================================================================================================

/// `OVERLAY_PUBLICATION_MAX_WINDOWS`, restated here because it is private to `crate::write`. The
/// figure is the subject of the case below, so a change to it must change this line — that is the
/// coupling, not an accident of it.
const PUBLICATION_FLOOR_WINDOWS: usize = 64;

/// Park the executor inside a new deny window, at `AfterFsync` — the entry is durable and not yet
/// applied.
///
/// Parking holds the publication count still while it is read. It does not wait for the previous
/// burst's close publication: a deny submitted while the drain is still looking at its lane joins
/// that drain. [`await_overlay_publications`] is the wait.
fn park_on_a_new_deny_window(
    engine: &Engine,
    faults: &FaultSwitchboard,
    entity: EntityId,
) -> tessera_engine::PendingChange {
    faults.arm_pause(PauseSite::AfterFsync, PauseAction::Stall);
    let pending = engine
        .submit_change(entity, ChangeOp::Suppress)
        .expect("the deny lane accepts");
    faults.await_arrivals(PauseSite::AfterFsync, 1, WAIT);
    pending
}

/// Waits for the drain's close publication, so that the next deny opens a new drain.
fn await_overlay_publications(engine: &Engine, at_least: u64) {
    let deadline = std::time::Instant::now() + WAIT;
    while engine.write_executor_stats().overlay_publications < at_least {
        assert!(
            std::time::Instant::now() < deadline,
            "the drain's close never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Drive exactly `windows` deny windows through **one drain that never closes**, the first of them
/// the window `parked` is held in.
///
/// The whole burst is in the queue before any of it is drained, which is what makes the window
/// count exact and the drain uninterrupted: the parked window releases into a lane holding
/// `windows - 1` full windows' worth, and `run_deny_pass` takes `DENY_WINDOW_MAX_ENTRIES` per pass
/// without ever observing the lane empty. A trickle of denies would instead close the drain between
/// windows and publish by the other route entirely, which is the state the floor does *not* address.
fn drain_deny_windows(
    engine: &Engine,
    faults: &FaultSwitchboard,
    entities: &[EntityId],
    windows: usize,
    parked: tessera_engine::PendingChange,
) {
    let fsyncs_before = engine.write_executor_stats().wal_fsyncs;
    let mut pending = vec![parked];
    for i in 0..((windows - 1) * DENY_WINDOW_MAX_ENTRIES) {
        pending.push(
            engine
                .submit_change(entities[i % entities.len()], ChangeOp::Suppress)
                .expect("the deny lane accepts"),
        );
    }
    faults.release();
    for p in pending {
        p.wait().expect("every deny in the burst takes hold");
    }
    assert_eq!(
        engine.write_executor_stats().wal_fsyncs - fsyncs_before,
        (windows - 1) as u64,
        "the burst must be exactly {windows} windows — one fsync per window, the parked window's \
         own already paid before this count was taken — or the floor is being asserted against \
         the wrong number of them"
    );
}

/// **A drain that never closes still publishes** — write-path §5.6's liveness floor.
///
/// The executor's other publication site sits after `while self.run_deny_pass() {}`, so under
/// arrival faster than application the drain loop does not exit and that site is never reached. The
/// floor is then the only route by which the dispositions reach a side-manifest: without it a node
/// under sustained revocation serves them, and logs them, indefinitely without any manifest
/// carrying them. Nothing here is a disclosure and nothing is irreversible — the denies are in
/// force and WAL-durable throughout — what degrades is the restore path and the log's ability to
/// shed the records behind them.
///
/// The boundary is pinned from both sides, because a floor asserted from one side cannot tell an
/// off-by-one from a working floor. One window short of the floor publishes exactly once, at the
/// drain's close; a burst one window past it publishes twice, at the floor and then at the close.
///
/// **One window past, not exactly at it**, and the difference is the whole discrimination. The
/// floor's publication clears `behind_live`, so a burst ending exactly at the floor publishes once
/// whichever route did it and the gauge cannot tell the two apart. The extra window re-dirties the
/// overlay, which is what makes the floor's publication visible as a second one.
///
/// Mutations this kills: raising the constant above the burst, deleting the `>=` branch, inverting
/// the comparison, and moving the `windows_since_publication` reset off the publication.
#[test]
fn a_deny_drain_that_never_closes_publishes_at_the_liveness_floor() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 64);
    let entities: Vec<EntityId> = (0..64).map(|i| entity_of(&engine, i)).collect();
    assert_eq!(
        engine.write_executor_stats().overlay_publications,
        0,
        "the fixture publishes nothing before the burst, or the counts below mean nothing"
    );

    let parked = park_on_a_new_deny_window(&engine, &faults, entities[0]);
    drain_deny_windows(
        &engine,
        &faults,
        &entities,
        PUBLICATION_FLOOR_WINDOWS - 1,
        parked,
    );

    await_overlay_publications(&engine, 1);
    let parked = park_on_a_new_deny_window(&engine, &faults, entities[0]);
    assert_eq!(
        engine.write_executor_stats().overlay_publications,
        1,
        "a burst one window short of the floor publishes only where every burst does, at the \
         drain's close"
    );

    drain_deny_windows(
        &engine,
        &faults,
        &entities,
        PUBLICATION_FLOOR_WINDOWS + 1,
        parked,
    );

    await_overlay_publications(&engine, 3);
    let parked = park_on_a_new_deny_window(&engine, &faults, entities[0]);
    assert_eq!(
        engine.write_executor_stats().overlay_publications,
        3,
        "a burst that passes the floor publishes inside the drain as well as at its close"
    );
    faults.release();
    parked.wait().expect("the barrier's own deny takes hold");
}
