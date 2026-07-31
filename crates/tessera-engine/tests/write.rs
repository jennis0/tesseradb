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
use tessera_engine::{Engine, ExecutorPosture};
use tessera_lifecycle::command::UnallocatedRow;
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
/// ack-before-swap in `execute_change`; only the log assertion failed, and with that one assertion
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
///
/// Sequencing is deterministic, not raced. The executor is stalled inside its first work item, the
/// work queue is filled to its bound behind it, and the deny is released only once the engine's own
/// `deny_submitted` counter proves it is *queued* — that counter is bumped after the enqueue and
/// before the blocking wait for exactly this purpose.
#[test]
fn a_deny_is_never_queued_behind_work() {
    const BOUND: usize = 4;
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, BOUND);
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

// =================================================================================================
// WAL failure
// =================================================================================================

/// **Lifecycle §4: never a refusal that leaves a deny unapplied.** An injected disk-full on a
/// suppress returns an error *and* the item is gone.
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

    faults.fail_next_fsyncs(1);
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
    faults.fail_next_fsyncs(1);
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

/// The drop guard. An executor that panics must report `Dead`, however it panicked.
///
/// Without a guard on the thread's own stack the posture would stay `Running` for ever and a
/// not-ready gate built on it would be green over a dead writer — the worst available outcome,
/// since a caller would keep being told its suppressions are in flight.
#[test]
fn an_executor_panic_is_reported_dead() {
    let tmp = TempDir::new().unwrap();
    let (engine, faults) = engine_with_faults(&tmp, 8);

    assert_eq!(engine.write_executor_posture(), ExecutorPosture::Running);

    faults.arm_pause(PauseSite::AfterFsync, PauseAction::Panic);
    let _ = engine.accept_ingest(vec![row("boom")], "boom".to_string(), [5u8; 32]);

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
