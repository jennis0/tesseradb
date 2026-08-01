//! Track B's engine-level test file (Phase 2 stage 2.1) — the acceptance path, asserted against
//! `Engine` directly rather than through HTTP.
//!
//! **Why this file exists.** Task 3a moves the WAL behind a single writer thread, which changes the
//! shape of the acceptance API: entity IDs stop being supplied by the caller and start being
//! assigned by the executor (that is the point of the change — Task 7a then assigns a whole
//! window's IDs in one signature-sorted run). Every existing caller therefore has to move, and the
//! case below lived in `tests/viewport.rs`, which Task 0c froze for **every** track because its
//! residue spans several of them. Track B would have had nowhere to put the migration, and nowhere
//! to put engine-level ordering assertions either — its four Task 3a tests would all have had to
//! reach the executor through the HTTP surface to observe an ordering that is not an HTTP property.
//!
//! ## What the Task 3a cases are and are not
//!
//! They assert the **ack contract** (lifecycle §4) and the **deny priority lane** (§1.3): that a
//! success ack follows its generation swap, that a deny is never queued behind work, that a deny
//! whose WAL append failed is applied anyway, and that a poisoned WAL trips the not-ready posture
//! rather than being retried. Each needs a fault the engine cannot be *asked* for, which is why
//! `tessera-lifecycle`'s `faults` module lands with this task rather than with stage 2.4's pause
//! points — see its module doc, and in particular its fidelity rule: an injected failure must be
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
use tessera_engine::{AcceptError, Engine, ExecutorPosture, DENY_DURABILITY_ATTEMPTS};
use tessera_lifecycle::command::{SubmitError, UnallocatedRow};
use tessera_lifecycle::faults::{FaultSwitchboard, PauseAction, PauseSite, Step};
use tessera_lifecycle::ChangeOp;
use tessera_types::EntityId;

use common::{build_fixture, full_coverage_credential, open_engine, source_id_key, N_ITEMS};

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
        descriptors: Vec::new(),
        x: 0.0,
        y: 0.0,
        scalars: Vec::new(),
        terms: Vec::new(),
    }
}

/// How many items a full-coverage viewport can see. Buffered (ingested) items have no row geometry
/// until stage 2.2's flush, so this counts the built corpus only — which is exactly what a
/// suppression must move.
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

fn entity_of(engine: &Engine, source_id: u64) -> EntityId {
    engine
        .resolve_external_id(&source_id_key(source_id))
        .expect("resolve")
        .expect("fixture item resolves")
}

// =================================================================================================
// The migrated case, and I9
// =================================================================================================

/// IMPORTANT I-9: an entity ingested after the build has no locator slot and no extent entry — the
/// live map must answer first, or `external_id_of` would wrongly report "this item has no external
/// id" for one that does.
///
/// Moved here from `tests/viewport.rs` (controller ruling on the Task 3a report's F1) and then
/// migrated by Task 3a itself. **The migration is the assertion.** The old form built a `WalRow`
/// and supplied `entity_id: EntityId::new(engine.allocator_high_water())` — the caller choosing the
/// id. It cannot any more: `Command::Ingest` carries `UnallocatedRow`, which has no id field,
/// because Task 7a must be free to assign a whole window's ids in one signature-sorted run. So the
/// test asserts the id it gets *back*, and that the property it was written for survives unaltered:
/// whatever assigns the id, the live map answers for it before the build's locator does.
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
/// Task 7a widens that scope from the command to the commit window. Pinning it here means 7a's
/// headline test is measuring a change of *scope* rather than the arrival of sorting at all.
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
    let suppress = std::thread::spawn(move || {
        e.accept_change(source_id_key(3), entity, ChangeOp::Suppress, None)
    });

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
    let suppress = std::thread::spawn(move || {
        e.accept_change(source_id_key(6), second, ChangeOp::Suppress, None)
    });

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
/// documented spelling for turning the window off and the one Task 10's A/B needs to exist.
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
        let _ = e.accept_change(source_id_key(3), entity, ChangeOp::Suppress, None);
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

/// **The same property on the third close path — the one Task 7a's F1 fix did not reach.**
///
/// `run_work_pass` closes a window mid-drain when an entry names an external id the open window
/// already holds (`CommitWindow::holds_external_id_of` — the mechanism that keeps Task 3a's
/// security C1 closed). Until Task 7b that close *continued* draining, and `window.rows()` resets
/// with the replacement, so the row bound could never trip on a conflict-heavy stream: a pass could
/// perform an unbounded number of full `append → fsync → apply → swap` cycles without ever
/// returning to `Executor::run`'s deny drain. That is lifecycle §1.3's prohibition verbatim — a
/// deny queued behind work of unbounded duration — and it is reachable at the shipped defaults from
/// a client re-ingesting an `external_id` a still-open window holds.
///
/// **The workload is pairs sharing an `external_id` under different batch ids, and Task 8 is why.**
/// It was pairs sharing a `batch_id` — until Task 8's `Held` join answered that case from inside
/// the window and stopped it forcing a close at all. Left as it was, this test would have kept
/// passing while asserting nothing: no close, no yield, and the property 7b's CRITICAL fix exists
/// for would have had **no test in the tree**. (7b's report records that all twenty other tests
/// stayed green under the mutation; this is the one.) Whichever member of a pair the drain meets
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
                // Same **external id** within a pair, different batch ids: the conflict is Task
                // 3a's C1 shape, and the second one 409s on `established_collisions` once the
                // close has applied the first. (A shared *batch id* no longer forces a close —
                // Task 8's join answers it in place — which is why this workload changed.)
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
        let _ = e.accept_change(source_id_key(3), entity, ChangeOp::Suppress, None);
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
        .accept_change(source_id_key(3), entity, ChangeOp::Suppress, None)
        .expect_err("a failed durability write must be reported, never silently swallowed");

    assert_eq!(
        visible(&engine),
        before - 1,
        "…and the item must be hidden ANYWAY: an under-durable deny beats a refused one ({err})"
    );
    assert_eq!(
        engine.write_executor_posture(),
        ExecutorPosture::WalPoisoned,
        "durability is owed, so the node must stop claiming it is ready"
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
        .accept_change(source_id_key(3), entity, ChangeOp::Suppress, None)
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
        .accept_change(source_id_key(3), entity, ChangeOp::Suppress, None)
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
/// appear and vanish across a crash, and Task 7b's rule 2 restates this per window. The extra
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
        .accept_change(source_id_key(3), entity, ChangeOp::Suppress, None)
        .expect("the suppression is durable: no fault is armed yet");
    let suppressed = visible(&engine);
    assert_eq!(
        suppressed,
        before - 1,
        "the item must be hidden before the unsuppress, or this test asserts nothing"
    );

    faults.fail_next_fsyncs(DENY_DURABILITY_ATTEMPTS);
    let err = engine
        .accept_change(source_id_key(3), entity, ChangeOp::Unsuppress, None)
        .expect_err("a failed durability write must be reported, never silently swallowed");

    assert_eq!(
        visible(&engine),
        suppressed,
        "the item must STAY hidden: an unsuppress that is not durable must not be applied, or a \
         restart re-hides an item the operator was told was still suppressed anyway ({err})"
    );
    assert_eq!(
        engine.write_executor_posture(),
        ExecutorPosture::WalPoisoned,
        "durability is owed, so the node must stop claiming it is ready"
    );
}

/// **A poisoned WAL trips the posture rather than being retried** (lifecycle §4).
///
/// The second half is what stops an obvious optimisation from being fail-open. "The posture is
/// poisoned, so skip the WAL call and return the error" would satisfy the first assertion while
/// leaving **every deny after the first unapplied** — the exact failure §4 exists to prevent. So
/// this submits a suppression *after* the poison and requires that the item still disappears.
///
/// Named for the posture, not for readiness: wiring `readyz` is Task 3b's, and this is the signal
/// it reads.
#[test]
fn a_poisoned_wal_trips_the_not_ready_posture() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 8);

    let doomed = entity_of(&engine, 3);
    faults.fail_next_fsyncs(DENY_DURABILITY_ATTEMPTS);
    let _ = engine.accept_change(source_id_key(3), doomed, ChangeOp::Suppress, None);
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
    let err = engine.accept_change(source_id_key(6), second, ChangeOp::Suppress, None);
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
}

/// The drop guard, **and the error the in-flight caller is handed while it fires**. An executor that
/// panics must report `Dead`, however it panicked; the submitter whose command it was holding must
/// be told `ReceiptLost`, never `ExecutorDead`.
///
/// Without a guard on the thread's own stack the posture would stay `Running` for ever and a
/// not-ready gate built on it would be green over a dead writer — the worst available outcome,
/// since a caller would keep being told its suppressions are in flight.
///
/// **The in-flight assertion is the Task 3b design gate's unanimous CRITICAL, pinned at its
/// producer.** `write.rs`'s `submit` answers `SubmitError::ReceiptLost` when the responder is
/// dropped, and `tessera-server`'s `map_accept_error` maps that to a fail-closed **500** rather than
/// the **503 `not-ready`** `ExecutorDead` gets — because a command the executor died *holding* may
/// have been appended, fsynced, applied and swapped, and 503's whole meaning is "this node did not
/// take your write". Until this assertion existed, every `ReceiptLost` in the workspace's tests was
/// a variant *constructed by the test*: reverting `submit`'s two `ReceiptLost` producers back to
/// `ExecutorDead` compiled and passed `cargo test --workspace` in full, leaving the split enforced
/// only by `error.rs`'s mapping over a variant nothing produced.
///
/// That matters more than an ordinary coverage gap because **Task 7a rewrites exactly this region**
/// — a commit window performing one swap and then acking N waiters in a loop, which
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
        .accept_change(source_id_key(3), EntityId::new(1), ChangeOp::Suppress, None)
        .expect_err("a dead executor must be reported, never swallowed");
    assert!(format!("{err}").contains("not running"), "got: {err}");
}

/// An engine that never started an executor refuses writes rather than pretending.
///
/// This is the state every read-only test, bench and example is in after Task 3a — starting no
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
    assert!(format!("{err}").contains("already knows"), "got: {err}");

    assert_eq!(
        engine.resolve_external_id(b"dup").unwrap(),
        Some(first),
        "the original must still own the key — an overwrite is what would make the first item \
         unreachable by any deny"
    );
}

// =================================================================================================
// The commit window (Task 7a)
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
/// incidental: under Task 3a's per-command allocation each submission's 25 rows are sorted among
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
/// counter is the other half: one record per entry is what preserves batch identity for Task 8.
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
        "and exactly one record per entry: batch identity survives the window (Task 8 joins on it)"
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

/// **The `ReceiptLost` widening** (Task 3b's split, named there as Task 7a's): a window swaps once
/// and then acks N waiters in a loop, so a death partway through the loop leaves some waiters acked
/// and some not.
///
/// Every un-acked waiter must get `SubmitError::ReceiptLost` → **500**, never
/// `SubmitError::ExecutorDead` → 503. Their ingest is durably in force by then — appended, fsynced,
/// applied and swapped — and 503's meaning is "this node did not take your write". Task 3b's
/// existing assertion covers a single command's shape only; this is the partial case.
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

/// Task 3a's security backstop (C1) **survives the window**, which is the one place a naive
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

/// **An empty window is never opened**, and the in-flight gauge is armed at the first entry rather
/// than at window construction.
///
/// The executor reaches its work pass with an empty queue on every iteration that a deny woke it
/// for, and on every spurious doorbell token. A gauge armed at construction would be armed there
/// and cleared by nothing — and `ExecutorStats::service_nanos_for_estimate` takes
/// `max(ewma, in-flight elapsed)`, so an idle node would answer every later 429 with a
/// `retry_after_s` that grows without bound towards the 300 s clamp. That is Task 6's F7 correction
/// running backwards.
#[test]
fn an_idle_work_pass_arms_nothing() {
    let tmp = TempDir::new().unwrap();
    let (engine, _faults) = engine_with_faults(&tmp, 8);

    // A deny wakes the executor; its work pass then finds an empty queue.
    let entity = entity_of(&engine, 3);
    engine
        .accept_change(source_id_key(3), entity, ChangeOp::Suppress, None)
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
// Task 8 — the batch-id state machine across a held window
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

/// **A byte-identical retry that lands in a held window JOINS it** (Task 8; contracts §3.4 r8's
/// third state, lifecycle §5.1: "a retry must join the open window rather than allocate a second
/// time").
///
/// **What discriminates this from the pre-state, and why the obvious assertions do not.** Before
/// Task 8 a held `batch_id` forced the window to close, and the retry was then answered from the
/// durable index. That answer was already correct: same ids, one WAL record, high-water up by the
/// batch's rows once. Those three assertions pass on the pre-state and prove nothing about this
/// task. What the close cost, and the join does not, is a **second window**: the pass yields after
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
/// Task 3a deleted an `Abort` variant that was a second `panic!` with a different message, because
/// arming it would have modelled a clean shutdown and called it a crash, and left this task the
/// obligation to build the real thing.
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
/// This is the **owner-confirmable default** of Task 8 brief §3 — contracts §3.4's "the batch has
/// no effect" reaches the refused retry and not the accepted original — and it is asserted where it
/// has to be, on the *original's* outcome. Asserting only that the retry 409s would pass equally
/// well on a build that threw the original away.
///
/// "In force" for a buffered ingest in stage 2.1 is the **live external-id map**: buffered rows
/// have no geometry until 2.2's flush, so `visible()` cannot see them and `resolve_external_id` is
/// the observable (Task 7a fix round, F5).
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
/// swap-precedes-ack. A stronger witness is unavailable in stage 2.1: nothing a reader can observe
/// distinguishes the two, because buffered rows have no geometry until stage 2.2's flush and
/// `visible()` therefore cannot see the ingest at all under either ordering. When flush lands, this
/// leg should read the swapped-in generation instead.
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
                .submit_change(b"k3".to_vec(), entity, ChangeOp::Suppress, None)
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
