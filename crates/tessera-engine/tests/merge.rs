//! The row-space merge, end to end — the half of merge that permutes row ids (write-path §7,
//! decision 0044's D2/D3).
//!
//! Five properties, and each is fail-open or fail-closed the other way round:
//!
//! - **The segment count comes down**, which is the axis the entity-space coalesce cannot bound
//!   and the whole reason this half exists.
//! - **No item is lost.** A merge is row-count preserving and drops no posting: every item stays
//!   visible, at the same coordinates, and every external id still resolves to the same entity.
//!   Dropping a row would be the compaction *fold*, which is invariant-bearing work this layer
//!   must not do.
//! - **Row space is permuted, so a projection that spans it cannot be served stale**, and the
//!   refresh that produces the replacement is armed before the swap.
//! - **A restart opens what was committed.**
//! - **A deny still denies the entity it named, not the row it happened to occupy** — the deny
//!   cases below, and the only fail-open on this list that no count assertion can see.
//!
//! ## Why the deny cases compare point sets rather than counts
//!
//! The deny mask is a bitmap over **rows**; the overlay names **entities**. A merge permutes row
//! ids inside the merged span, so a mask carried across one keeps denying the row — which now
//! names a different entity. The visible *count* is then still exactly right, because one item
//! left the visible set and one entered it; what changed is *which*. Every count assertion in
//! this file passes against that bug.
//!
//! So these cases assert on the served `tessera_id` set. `tessera_id` is a blinding permutation of
//! the entity id under the deployment key (I10, decision 0014) — a function of the entity, never
//! of the row — so it is stable across a merge by construction, and set equality across the swap
//! is exactly the discrimination a count cannot make. `publish_merge` re-derives the mask over the
//! new row space for this reason; these are the tests that hold it to it.
//!
//! **The seam between a merge's execution on the pool and its publication on the executor now
//! has its pause site** — `faults::PauseSite::BeforeMergePublish`, the first statement of
//! `publish_merge` (decision 0071; correctness-suite §12.3) — and
//! `the_merge_publication_seam_parks_the_executor…` below holds it to the pause-site contract:
//! executed on the pool, parked before publication, nothing swapped, and the publication lands on
//! release. That parked state is what a crash-mid-merge test kills into: the merged segment an
//! orphan, its inputs untouched.
//!
//! **A suppression accepted in that window is covered, and the pause site is not what covers it.**
//! `publish_merge` re-derives the deny mask from the live overlay at swap time; the case that
//! tests it needs a deny *acked* between a merge's execution and its publication.
//! `BeforeMergePublish` cannot assemble that, because the parked thread is the deny lane's own
//! thread: a deny submitted while it is parked is applied only after the release has already
//! published, which is the ordering a stale capture survives unnoticed.
//! `Engine::set_merge_publication_paused_for_test` can, and does — it holds the completed merge
//! undrained in its channel and leaves the executor free to accept the deny, which is the same
//! window the watermark case below lands a *flush* in.
//! `a_suppression_accepted_inside_a_merges_flight_survives_its_publication` is that interleaving
//! with its ordering chosen; `a_suppression_racing_a_merge_is_in_force_once_both_have_landed`
//! keeps the uncontrolled one beside it.
//!
//! *(This module recorded the interleaving as out of reach from 2026-08-05 until 2026-08-30. The
//! paragraph was rewritten on 2026-08-15 to turn on the pause site — the same day the publication
//! hold that does reach it landed, in another commit, for the watermark case.)*

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{Engine, EngineConfig, ViewportRequest};
use tessera_lifecycle::{ChangeOp, UnallocatedRow};
use tessera_types::EntityId;

/// `MergePolicy::tier_width` — how many adjacent, same-tier extents select a merge.
const TIER_WIDTH: usize = 4;

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn open_engine_at(tmp: &std::path::Path, root: &std::path::Path) -> Engine {
    Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: 3600,
            max_merged_segment_bytes: None,
            // Compaction §9's trigger is off unless a deployment configures one.
            compaction: tessera_engine::CompactionSchedule::off(),
            ..config_uncapped()
        },
    )
    .expect("engine opens")
}

fn engine_at(tmp: &std::path::Path, root: &std::path::Path) -> Engine {
    let mut engine = open_engine_at(tmp, root);
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    engine
}

/// [`engine_at`], with the executor run against a switchboard the test holds the other end of —
/// the seam-pause case below is its one caller.
fn engine_at_with_faults(
    tmp: &std::path::Path,
    root: &std::path::Path,
) -> (
    Engine,
    std::sync::Arc<tessera_lifecycle::faults::FaultSwitchboard>,
) {
    let mut engine = open_engine_at(tmp, root);
    let faults = std::sync::Arc::new(tessera_lifecycle::faults::FaultSwitchboard::new());
    engine
        .start_write_executor_with_faults(64, std::sync::Arc::clone(&faults))
        .expect("the executor starts once");
    (engine, faults)
}

fn whole_extent() -> ViewportRequest<'static> {
    ViewportRequest::new("s0", 2, [0.0, 0.0, 1000.0, 1000.0], N_ITEMS as usize)
}

/// A viewport, retried past the bounded `ProjectionBuilding` a merge's refresh window answers
/// with. Decision 0044 permits exactly that residual — stale-serve is unsound across a merge —
/// and a test that did not retry would be asserting the residual does not exist.
fn viewport(
    engine: &Engine,
    session: &tessera_engine::Session,
) -> tessera_engine::viewport::ViewportOut {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match engine.viewport(session, whole_extent()) {
            Ok(out) => return out,
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "timed out retrying a viewport: {e}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Deny across a merge — see this module's doc for why these compare served sets, not counts.
// ---------------------------------------------------------------------------------------------

/// Items per interleaved segment. Above `TIER_WIDTH`, so the merged order cycles through every
/// segment more than once and no row's position is a coincidence of the first cycle.
const ROWS_EACH: usize = 8;

/// The served set as `tessera_id`s.
///
/// `tessera_id` is a blinding permutation of the entity id under the deployment key (I10) — a
/// function of the entity, never of the row — so it is stable across a merge by construction, and
/// set equality across the swap is the discrimination a count cannot make.
fn served_ids(
    engine: &Engine,
    session: &tessera_engine::Session,
) -> BTreeSet<tessera_types::TesseraId> {
    viewport(engine, session)
        .points
        .iter()
        .map(|(id, _)| id)
        .collect()
}

fn segment_count(engine: &Engine) -> usize {
    engine.generation().bundle.partitions["default"].views["s0"]
        .segments
        .len()
}

fn rows_of(engine: &Engine, entities: &[EntityId]) -> Vec<Option<u32>> {
    let generation = engine.generation();
    let row_space = &generation.bundle.partitions["default"].views["s0"].row_space;
    entities
        .iter()
        .map(|e| row_space.row_of(*e).map(|r| r.raw()))
        .collect()
}

/// Flush `TIER_WIDTH` segments of [`ROWS_EACH`] items each, laid out so that the segments
/// **interleave in Morton order**.
///
/// **This is what makes the merge's permutation non-trivial, and it is load-bearing rather than
/// incidental.** The obvious fixture — one item per flush, at ascending x — produces four
/// single-row extents that are already in Morton order, so the merged segment concatenates them
/// unchanged and *every row keeps its id*. Every assertion about a permuted row space is then
/// vacuously true, including the deny cases below: the mask can be carried across the swap instead
/// of re-derived and nothing observes the difference. Measured, not supposed — the single-item
/// fixture reports rows 64,65,66,67 on both sides of the merge.
///
/// So segment `s` takes the x positions congruent to `s` modulo `TIER_WIDTH`, at a constant y.
/// Morton order over a constant y is monotone in x, so the merged segment orders the rows
/// `s0t0, s1t0, s2t0, s3t0, s0t1, …` and every row but the first cycle's moves. [`run_merge`]
/// asserts that it did, so this can never silently regress to the identity.
///
/// **Every item carries `ALL_TERM`; the even-`t` ones additionally carry `SUBSET_TERM`**, so a
/// sparse principal's view across the merge is expressible — the postings side of the same
/// question, which a full-coverage credential cannot see.
fn flush_interleaved_segments(engine: &Engine) -> Vec<Vec<(EntityId, String)>> {
    let mut by_segment = Vec::new();
    for s in 0..TIER_WIDTH {
        let mut rows = Vec::new();
        let mut items = Vec::new();
        for t in 0..ROWS_EACH {
            let external_id = format!("ext-{s}-{t}");
            let descriptors = if t.is_multiple_of(2) {
                vec![b"0".to_vec(), b"1".to_vec()]
            } else {
                vec![b"0".to_vec()]
            };
            rows.push(UnallocatedRow {
                external_id: Some(external_id.as_bytes().to_vec()),
                view: "s0".to_string(),
                join: None,
                // x ≡ s (mod TIER_WIDTH), scaled to distinct cells inside the extent.
                x: ((t * TIER_WIDTH + s) * 20) as f64,
                y: 5.0,
                scalars: Vec::new(),
                terms: engine.resolve_terms(&descriptors),
                descriptors,
            });
            items.push(external_id);
        }
        let entities = engine
            .accept_ingest(rows, format!("batch-{s}"), [s as u8; 32])
            .expect("ingest is accepted");
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_until("the flush to publish", || {
            engine.write_executor_stats().flushes > flushes
        });
        by_segment.push(entities.into_iter().zip(items).collect());
    }
    by_segment
}

/// An engine with `TIER_WIDTH` interleaved extents flushed and merge held until asked.
fn engine_with_pending_merge(
    tmp: &tempfile::TempDir,
    root: &std::path::Path,
) -> (Engine, Vec<(EntityId, String)>) {
    build_fixture_n(
        root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );
    let engine = engine_at(tmp.path(), root);
    engine.set_merge_for_test(false);
    let flat = flush_interleaved_segments(&engine)
        .into_iter()
        .flatten()
        .collect();
    (engine, flat)
}

/// Run the merge, and **assert its permutation is not the identity**.
///
/// The guard belongs here rather than in one case: a fixture that stops interleaving makes every
/// deny assertion below vacuously true rather than false, so nothing else in this file would
/// notice. See [`flush_interleaved_segments`] for how that happened.
fn run_merge(engine: &Engine, entities: &[EntityId]) {
    let before = rows_of(engine, entities);
    engine.set_merge_for_test(true);
    engine.request_flush();
    wait_until("the merge to publish", || {
        engine.write_executor_stats().merges >= 1
    });
    let after = rows_of(engine, entities);
    assert!(
        before.iter().all(Option::is_some) && after.iter().all(Option::is_some),
        "every flushed entity must still have a row: {before:?} -> {after:?}"
    );
    assert_ne!(
        before, after,
        "this merge permuted nothing, so every case below is vacuous — see \
         flush_interleaved_segments"
    );
}

/// **A suppression accepted before a merge still hides the same item afterwards.**
///
/// The fail-open this exists to catch is the one `publish_merge` re-derives the deny mask to
/// avoid, and the one `crate::merge`'s own module doc names as its mutation: carry the mask
/// forward instead, and every denied row id keeps denying a row that now belongs to a different
/// entity. Two items are suppressed here and two stay hidden either way — so the assertion is set
/// equality on the served `tessera_id`s, which tells "the same two are hidden" from "two are
/// hidden".
///
/// **One suppression sits inside the merged span and one outside it.** The merge consumes the four
/// flushed extents, not the base segment, so a base-segment entity's row does not move. A mask
/// carried forward still gets that one right — which is how a bug here could survive a case that
/// suppressed a single item and happened to pick the wrong one.
///
/// **Mutation:** replace `derive_denied(&live.overlay, &next_bundle)` in `publish_merge` with
/// `Arc::clone(&live.denied)` and this fails on the set while every count in this file stays green.
#[test]
fn a_suppression_survives_a_merge_and_still_hides_the_same_item() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let (engine, items) = engine_with_pending_merge(&tmp, &root);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let entities: Vec<EntityId> = items.iter().map(|(e, _)| *e).collect();

    let unsuppressed = served_ids(&engine, &session);
    assert_eq!(unsuppressed.len(), 64 + TIER_WIDTH * ROWS_EACH);

    // Inside the merged span: a flushed item the permutation moves. Taken from the *second* cycle
    // (`t >= 1`), because the first cycle's rows are the one part of the merged order that can
    // coincide with the pre-merge layout.
    let inside = items[1].0;
    // Outside it: a base-segment entity, whose row does not move. See this test's doc.
    let outside = EntityId::new(source_to_new_map(&root, &engine.generation().prefix)[&11]);
    for entity in [inside, outside] {
        engine
            .accept_change(entity, ChangeOp::Suppress)
            .expect("a suppression is accepted");
    }

    let suppressed = served_ids(&engine, &session);
    assert_eq!(
        suppressed.len(),
        unsuppressed.len() - 2,
        "both suppressions are in force before the merge"
    );
    assert!(suppressed.is_subset(&unsuppressed));

    run_merge(&engine, &entities);

    assert_eq!(
        served_ids(&engine, &session),
        suppressed,
        "a merge must hide the same two items afterwards — an equal-sized set hiding a different \
         pair is the deny mask carried across the permutation instead of re-derived over it"
    );
    for entity in [inside, outside] {
        assert!(
            engine.generation().overlay.is_suppressed(entity),
            "entity {} lost its suppression to the merge",
            entity.raw()
        );
    }
}

/// **A sparse principal sees exactly its own items across a merge**, and a suppression inside that
/// subset removes exactly one of them.
///
/// The full-coverage cases above exercise the row-space half. This is the postings half: the merge
/// leaves every consumed segment's delta tier listed (`rebase_into`'s third rule) while moving the
/// rows those postings' entities occupy, so a session whose visible set is a *subset* is where a
/// tier dropped, double-counted, or resolved against the wrong row space would show. A principal
/// holding only `SUBSET_TERM` sees the even-`t` ingested items and the base fixture's every-third.
#[test]
fn a_sparse_principal_sees_the_same_subset_across_a_merge() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let (engine, items) = engine_with_pending_merge(&tmp, &root);
    let entities: Vec<EntityId> = items.iter().map(|(e, _)| *e).collect();

    let sparse = engine.authorise(&subset_credential()).unwrap();
    let full = engine.authorise(&full_coverage_credential()).unwrap();

    let before = served_ids(&engine, &sparse);
    // Every third base item carries SUBSET_TERM (common::terms_of), plus the even-t ingested ones.
    let expected = (0..64u64).filter(|i| i.is_multiple_of(3)).count() + TIER_WIDTH * ROWS_EACH / 2;
    assert_eq!(
        before.len(),
        expected,
        "the sparse principal's set before the merge"
    );
    assert!(
        before.is_subset(&served_ids(&engine, &full)),
        "and it is a subset of what full coverage sees"
    );

    // `(s=0, t=4)`: an even `t`, so it carries SUBSET_TERM and is in this principal's set, and far
    // enough into the cycle that the merge genuinely moves its row. `(s=0, t=0)` holds the smallest
    // x in the fixture, so it keeps row `row_base` on both sides of the swap and would make this
    // case pass against a mask that was carried rather than re-derived.
    let suppressed_entity = items[4].0;
    engine
        .accept_change(suppressed_entity, ChangeOp::Suppress)
        .expect("a suppression is accepted");
    let after_suppress = served_ids(&engine, &sparse);
    assert_eq!(after_suppress.len(), before.len() - 1);

    run_merge(&engine, &entities);

    assert_eq!(
        served_ids(&engine, &sparse),
        after_suppress,
        "the sparse principal's set must be unchanged by the merge, item for item — a tier \
         resolved against the pre-merge row space keeps the cardinality and moves the membership"
    );
}

/// **An unsuppress after a merge restores the item that was suppressed**, and no other.
///
/// Rule S (write-path §5.4) is the rule under test: a suppression retires **only** on unsuppress,
/// and never touches postings — so the baseline it restores to is exact, and set equality against
/// it is what names the restored item rather than merely counting it.
///
/// **Weaker than its siblings against the carried-mask mutation, and here is why**, so that nobody
/// reads it as covering that: the unsuppress publishes its own generation, and *that* publication
/// re-derives the mask correctly whatever the merge did. So this case still passes against a merge
/// that carried the mask forward — the deny publication repairs it before the assertion looks. It
/// covers Rule S across a permutation; `a_suppression_survives_a_merge_and_still_hides_the_same_item`
/// is what covers the derivation.
#[test]
fn an_unsuppress_after_a_merge_restores_the_item_that_was_suppressed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let (engine, items) = engine_with_pending_merge(&tmp, &root);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let entities: Vec<EntityId> = items.iter().map(|(e, _)| *e).collect();

    let baseline = served_ids(&engine, &session);
    let entity = items[ROWS_EACH + 2].0;
    engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect("a suppression is accepted");
    assert_eq!(served_ids(&engine, &session).len(), baseline.len() - 1);

    run_merge(&engine, &entities);

    engine
        .accept_change(entity, ChangeOp::Unsuppress)
        .expect("an unsuppress is accepted");
    assert_eq!(
        served_ids(&engine, &session),
        baseline,
        "the unsuppress must restore the entity that was suppressed, at its own identity — a set \
         of the right size naming a different item is a mask resolved against a stale row space"
    );
    assert!(!engine.generation().overlay.is_suppressed(entity));
}

/// **A delete before a merge stays deleted across it and across a restart**, and the merge carries
/// its tombstone into the manifest it commits.
///
/// Rule F (write-path §5.4): a deletion retires **only** at the compaction fold that executes it,
/// and no fold runs in this case — so nothing here retires, and the delete must still be in force
/// after the merge has rewritten the segment its row lived in. A merge is row-count preserving by
/// design (it is not the fold), so the deleted row is still *present* in the merged segment; what
/// must survive is the overlay entry that hides it.
///
/// **Mutation:** have `rebase_into` drop `tombstones`, and the item returns on the restart rather
/// than on the merge — which is why this case reopens.
#[test]
fn a_delete_before_a_merge_stays_deleted_across_it_and_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let (engine, items) = engine_with_pending_merge(&tmp, &root);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let entities: Vec<EntityId> = items.iter().map(|(e, _)| *e).collect();

    let baseline = served_ids(&engine, &session);
    let deleted = items[ROWS_EACH * 2 + 3].0;
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("a delete is accepted");
    let after_delete = served_ids(&engine, &session);
    assert_eq!(after_delete.len(), baseline.len() - 1);

    run_merge(&engine, &entities);
    assert_eq!(
        served_ids(&engine, &session),
        after_delete,
        "the deleted item must still be the hidden one after the merge"
    );

    assert!(
        engine.generation().bundle.partitions["default"]
            .manifest
            .tombstones
            .contains(&deleted.raw()),
        "the merge's manifest must carry the tombstone forward — nothing retires it before the \
         fold, and no fold runs here"
    );

    drop(engine);
    let reopened = engine_at(tmp.path(), &root);
    let session = reopened.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(
        served_ids(&reopened, &session),
        after_delete,
        "and it is still deleted once the merged manifest is what the node opens"
    );
    assert!(reopened.generation().overlay.is_deleted(deleted));
}

/// **A suppression accepted while the merge is in flight is in force once it lands.**
///
/// **The uncontrolled ordering**, kept beside the controlled one: the deny is submitted after the
/// tick that dispatches the merge, so it may be applied before or after `publish_merge`
/// re-derives, and the property must hold either way. A publication that re-derived from a
/// *captured* overlay rather than the live one loses the deny on one of the two orderings only, so
/// what this case catches depends on which ordering the scheduler produced — it caught a planted
/// stale capture 20 runs out of 20 on one box, which is a fact about that box rather than a
/// guarantee. `a_suppression_accepted_inside_a_merges_flight_survives_its_publication` is the same
/// property with the ordering chosen rather than raced; this one stays because the ordering it
/// usually lands is the one a real deny takes when it beats the planner.
#[test]
fn a_suppression_racing_a_merge_is_in_force_once_both_have_landed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let (engine, items) = engine_with_pending_merge(&tmp, &root);
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    let baseline = served_ids(&engine, &session);
    let entity = items[ROWS_EACH * 3 + 1].0;

    engine.set_merge_for_test(true);
    engine.request_flush();
    engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect("a suppression is accepted");
    wait_until("the merge to publish", || {
        engine.write_executor_stats().merges >= 1
    });

    let served = served_ids(&engine, &session);
    assert_eq!(
        served.len(),
        baseline.len() - 1,
        "the suppression is in force whichever side of the swap it landed"
    );
    assert!(served.is_subset(&baseline));
    assert!(engine.generation().overlay.is_suppressed(entity));
}

/// **A suppression accepted *inside* a merge's flight survives its publication** — the ordering
/// chosen rather than raced.
///
/// The interleaving the case above can only stumble into: the deny is applied and acked while the
/// completed merge is held undrained in its channel, so the publication that follows is
/// unambiguously the *later* of the two and must re-derive the deny mask from an overlay that
/// already carries it. A publication deriving from an overlay captured when the merge was planned
/// re-exposes the suppressed entity here every time.
///
/// **The hold is the lever, and the pause site is not.** `PauseSite::BeforeMergePublish` parks the
/// executor thread — which is the deny lane's own thread — so a deny submitted while it is parked
/// is applied *after* the release completes the publication, which is the ordering the stale-capture
/// defect survives. `set_merge_publication_paused_for_test` holds the completed merge in its
/// channel instead and leaves the executor free, which is the only way in the tree to put an acked
/// deny strictly between a merge's execution and its publication. The watermark case above uses
/// the same hold to land a *flush* in the same window.
///
/// **Mutations this kills:** deriving `denied` in `publish_merge` from an overlay captured when
/// the merge was dispatched rather than from `live.overlay`, and the same substitution in
/// `write_deny_state`, which is the durable half of it. Planted, it re-exposes the suppressed
/// entity here on every run — 20 of 20 — where the racing case can only catch it on the ordering
/// the scheduler happens to give. The re-exposure is what this asserts; in a debug build the same
/// plant is stopped earlier still, by `publish_arc`'s derivation assertion, so the served-set
/// demonstration was taken with debug assertions off.
#[test]
fn a_suppression_accepted_inside_a_merges_flight_survives_its_publication() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let (engine, items) = engine_with_pending_merge(&tmp, &root);
    let entities: Vec<EntityId> = items.iter().map(|(e, _)| *e).collect();
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    let baseline = served_ids(&engine, &session);
    let rows_before = rows_of(&engine, &entities);
    let entity = items[ROWS_EACH * 3 + 1].0;

    // The merge executes and completes, and is held undrained — the flight, held open for a deny
    // rather than for a flush.
    engine.set_merge_publication_paused_for_test(true);
    engine.set_merge_for_test(true);
    engine.request_flush();
    wait_until("the merge to complete and hold", || {
        engine.merge_publication_is_held_for_test()
    });
    // Off again, so nothing dispatches a second merge over the same extents.
    engine.set_merge_for_test(false);

    engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect("a suppression is accepted");
    let inside = served_ids(&engine, &session);
    assert_eq!(
        inside.len(),
        baseline.len() - 1,
        "the deny is in force before the merge publishes, which is what makes the ordering chosen          rather than raced"
    );
    assert!(engine.generation().overlay.is_suppressed(entity));

    engine.set_merge_publication_paused_for_test(false);
    wait_until("the merge to publish", || {
        engine.write_executor_stats().merges >= 1
    });

    assert_ne!(
        rows_of(&engine, &entities),
        rows_before,
        "the publication permuted row space — an identity permutation would make the assertion          below vacuous (see flush_interleaved_segments)"
    );
    assert_eq!(
        served_ids(&engine, &session),
        inside,
        "the publication re-derived the deny mask over the new row space from the live overlay:          the suppressed entity is still absent and no other entity took its place"
    );
    assert!(engine.generation().overlay.is_suppressed(entity));
}

/// **The segment count comes down and nothing is lost doing it.**
///
/// Without this the tile path pays one binary search and one `range_cardinality` per live segment
/// per tile, and a 90 s tick reaches ~960 segments in a day — the axis the entity-space coalesce
/// leaves untouched by construction.
///
/// **Mutation:** make `execute_merge` drop a row (the compaction fold, arriving as an
/// optimisation) and the visible count falls short.
#[test]
fn a_merge_collapses_segments_and_loses_no_item() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let (engine, items) = engine_with_pending_merge(&tmp, &root);
    let entities: Vec<EntityId> = items.iter().map(|(e, _)| *e).collect();
    // The build segment plus one per flush.
    assert_eq!(segment_count(&engine), TIER_WIDTH + 1);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let before = viewport(&engine, &session);
    let visible_before: u64 = before.tiles.iter().map(|t| t.visible).sum();
    assert_eq!(visible_before, (64 + TIER_WIDTH * ROWS_EACH) as u64);

    run_merge(&engine, &entities);

    assert_eq!(
        segment_count(&engine),
        2,
        "the build segment plus the one merged segment"
    );
    let after = viewport(&engine, &session);
    assert_eq!(
        after.tiles.iter().map(|t| t.visible).sum::<u64>(),
        visible_before,
        "a merge is row-count preserving: every item is still visible"
    );
    // Everything but the stamp, which names the new geometry version by construction.
    assert_eq!(
        after.tiles, before.tiles,
        "every tile's counts are unchanged — a merge moves rows, never items"
    );
    assert_eq!(
        after.points, before.points,
        "and every point keeps its identity and its exact code — the merge is byte-exact through \
         the Morton code, never through a dequantise-and-requantise"
    );

    // Every binding survives the run coalesce the merge performed on the way.
    for (entity, external_id) in &items {
        assert_eq!(
            engine
                .resolve_external_id(external_id.as_bytes())
                .expect("resolvable"),
            Some(*entity),
            "external id {external_id} lost its binding to the merge"
        );
        assert_eq!(
            engine.external_id_of(*entity).expect("no inconsistency"),
            Some(external_id.as_bytes().to_vec()),
            "and the reverse direction still answers for it"
        );
    }
}

/// **A merge bumps `segments_version` and arms the refresh before the swap.**
///
/// Row ids inside the merged span name different entities afterwards (I11), so no cached
/// projection covering the span may be served: `extends_to` refuses, and rung 3 of the ladder is
/// what a racer meets. The refresh replaces the entry with an extents-only re-projection rather
/// than the *measured* 1 277 ms rebuild, which is what keeps the residual bounded.
///
/// **Mutation:** carry the deny mask forward instead of re-deriving it and a suppressed row keeps
/// its old id — which after a permutation names a different entity.
#[test]
fn a_merge_moves_geometry_and_the_refresh_replaces_every_projection() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let (engine, items) = engine_with_pending_merge(&tmp, &root);
    let entities: Vec<EntityId> = items.iter().map(|(e, _)| *e).collect();

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    viewport(&engine, &session);
    let geometry_before = engine.generation().segments_version;
    let builds_before = engine.full_projection_builds();

    run_merge(&engine, &entities);
    assert!(
        engine.generation().segments_version > geometry_before,
        "a merge permutes row space, so it must bump the geometry version — the only safe \
         discriminator a row-space artefact may key on"
    );
    wait_until("the refresh to replace the entry", || {
        engine.refreshes() >= 1
    });

    // Served from the refresh's entry, not rebuilt on this thread.
    viewport(&engine, &session);
    assert_eq!(
        engine.full_projection_builds(),
        builds_before,
        "the post-merge viewport must be served from the refresh's extents-only re-projection, \
         never from a full rebuild on the request thread"
    );
}

/// **A restart opens what the merge committed**, with the consumed segments gone from the manifest
/// and every item still visible.
///
/// The delta tiers of the consumed segments stay listed — their entities still have rows, in the
/// merged segment — so this also pins the rule most easily got wrong: a merge that dropped a
/// consumed segment's tier would make every item that tier carries invisible to every session,
/// silently.
#[test]
fn a_merged_manifest_reopens_with_every_item_and_every_tier() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");

    let items = {
        let (engine, items) = engine_with_pending_merge(&tmp, &root);
        let entities: Vec<EntityId> = items.iter().map(|(e, _)| *e).collect();
        // The merge is selected on the tick, and the last flush's own tick ran before that flush
        // published — so one more tick is what dispatches it.
        run_merge(&engine, &entities);
        items
    };

    let reopened = engine_at(tmp.path(), &root);
    let generation = reopened.generation();
    assert_eq!(
        generation.bundle.partitions["default"].views["s0"]
            .segments
            .len(),
        2,
        "the reopened bundle holds the build segment and the merged one"
    );
    assert_eq!(
        generation.delta_postings.len(),
        TIER_WIDTH,
        "every consumed segment's tier is still listed — its entities have rows in the merged \
         segment, and dropping one would make them invisible"
    );

    let session = reopened.authorise(&full_coverage_credential()).unwrap();
    let out = viewport(&reopened, &session);
    assert_eq!(
        out.tiles.iter().map(|t| t.visible).sum::<u64>(),
        (64 + TIER_WIDTH * ROWS_EACH) as u64,
        "every item survives the merge and the restart"
    );
    for (entity, external_id) in &items {
        assert!(
            generation.bundle.partitions["default"].views["s0"]
                .row_space
                .row_of(*entity)
                .is_some(),
            "entity {} lost its row across the merge and the restart",
            entity.raw()
        );
        assert_eq!(
            reopened
                .resolve_external_id(external_id.as_bytes())
                .expect("resolvable"),
            Some(*entity)
        );
    }
}

/// **A reboot reads back a watermark no lower than the one the running process served**, across
/// the interleaving of a merge and a flush on one write path.
///
/// A merge's plan is taken against one watermark; a flush publishing during its flight advances
/// the live value; the merge publishes last. The side-manifest the merge commits is a clone of
/// the live one and must keep the flush's watermark — a merge moves no entity into or out of the
/// visible set, so it has nothing to say about the watermark at all (write-path §7). Stamping its
/// plan-time snapshot instead regressed the durable value by one batch, silently: the boot
/// rebuilds the buffer by `has_row`, not by the watermark, so every answer stayed right while the
/// durable record under-reported — which is how it survived until the endurance tier compared the
/// disc against the wire. The publication hold is what makes the flight deterministic
/// (`Engine::set_merge_publication_paused_for_test`); unassisted, the window is one pool
/// scheduling race wide.
///
/// **Mutations this kills:** re-stamping `manifest.watermark` (or `entity_id_high_water`) from
/// the completed unit in `crate::merge::rebase_into`; and gutting `check_manifest_publishable`,
/// which is what turns the same mistake on the next durable scalar into a refused write rather
/// than a silent regression.
#[test]
fn a_reboot_after_a_merge_reads_back_the_watermark_the_process_served() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");

    let watermark_served = {
        let (engine, _items) = engine_with_pending_merge(&tmp, &root);

        // The merge executes and completes, but its publication is held — the flight, made wide
        // enough to land a flush in.
        engine.set_merge_publication_paused_for_test(true);
        engine.set_merge_for_test(true);
        engine.request_flush();
        wait_until("the merge to complete and hold", || {
            engine.merge_publication_is_held_for_test()
        });
        // Off again, so the tick that publishes the flush below does not dispatch a second merge
        // over the same four extents.
        engine.set_merge_for_test(false);

        // The flush that advances the watermark past the merge's plan.
        let batch: Vec<UnallocatedRow> = (0..ROWS_EACH)
            .map(|t| {
                let descriptors = vec![b"0".to_vec()];
                UnallocatedRow {
                    external_id: Some(format!("late-{t}").into_bytes()),
                    view: "s0".to_string(),
                    join: None,
                    x: ((t * TIER_WIDTH) * 20) as f64,
                    y: 45.0,
                    scalars: Vec::new(),
                    terms: engine.resolve_terms(&descriptors),
                    descriptors,
                }
            })
            .collect();
        engine
            .accept_ingest(batch, "batch-late".to_string(), [9u8; 32])
            .expect("ingest is accepted");
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_until("the late flush to publish", || {
            engine.write_executor_stats().flushes > flushes
        });
        let watermark_served = engine.generation().watermark;

        engine.set_merge_publication_paused_for_test(false);
        wait_until("the merge to publish", || {
            engine.write_executor_stats().merges >= 1
        });

        let generation = engine.generation();
        assert_eq!(
            generation.watermark, watermark_served,
            "a merge moves the watermark for no one, in memory included"
        );
        assert_eq!(
            generation.bundle.partitions["default"].manifest.watermark, watermark_served,
            "the manifest the merge published must carry the live watermark, not its plan-time \
             snapshot — the manifest is what a restart believes, and every later publication \
             clones it forward"
        );
        watermark_served
    };

    let reopened = open_engine_at(tmp.path(), &root);
    let read_back = reopened.generation().watermark;
    assert!(
        read_back >= watermark_served,
        "the reboot read back watermark {read_back} where the running process served \
         {watermark_served} — the durable record regressed by the batch the flush published \
         inside the merge's flight"
    );
}

/// **A racer inside a merge's refresh window is shed, not made to pay the rebuild** — decision
/// 0044's bounded 429 residual, and the one place the design accepts a refusal.
///
/// Stale-serve is unsound across a merge: the stale entry's bits inside the merged span name
/// different entities now. So rung 2 refuses, and rung 3's choice is the whole of what 0044
/// bought — shed for the refresh's bounded duration, or pay the *measured* 1 277 ms rebuild on
/// the request thread. The refresh is **held** here rather than switched off, because those are
/// different states: a refresh that finishes without producing anything clears the flag and rung 3
/// builds, which is the liveness floor, not this.
///
/// **Mutation:** clear `refresh_in_flight` before the swap instead of setting it, and the racer
/// takes the rebuild silently.
#[test]
fn a_racer_inside_a_merges_refresh_window_is_shed_rather_than_rebuilding() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let (engine, items) = engine_with_pending_merge(&tmp, &root);
    let entities: Vec<EntityId> = items.iter().map(|(e, _)| *e).collect();

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    viewport(&engine, &session);
    let builds_before = engine.full_projection_builds();

    // Hold the refresh, then let the merge publish: the window stays open until we release it.
    engine.set_refresh_paused_for_test(true);
    run_merge(&engine, &entities);

    let refused = engine
        .viewport(&session, whole_extent())
        .expect_err("a racer inside the refresh window must be shed");
    assert!(
        matches!(refused, tessera_engine::EngineError::ProjectionBuilding),
        "and shed as backpressure with a Retry-After, never as a failure: {refused}"
    );
    assert_eq!(
        engine.full_projection_builds(),
        builds_before,
        "the racer must not have rebuilt — that is the 1 277 ms this residual exists to avoid"
    );

    // Released, the window closes and the same request is served.
    engine.set_refresh_paused_for_test(false);
    wait_until("the refresh to land", || engine.refreshes() >= 1);
    let served = viewport(&engine, &session);
    assert_eq!(
        served.tiles.iter().map(|t| t.visible).sum::<u64>(),
        (64 + TIER_WIDTH * ROWS_EACH) as u64,
        "every item is visible once the refresh has replaced the entry"
    );
    assert_eq!(
        engine.full_projection_builds(),
        builds_before,
        "and the replacement was an extents-only re-projection, not a rebuild"
    );
}

// ---------------------------------------------------------------------------------------------
// The configured policy knobs (write-path §10). Until 2026-08-15 `serve.tier_width` and
// `serve.segment_floor_bytes` were parsed and validated by the server and reached the engine
// nowhere — the merge policy hard-coded 4 and 16 MiB — so a configured value changed nothing,
// silently (the inert-key defect decision 0045 forbids). These two tests are the wiring's proof,
// and they are behavioural on purpose: a test that asserted a struct field's value would pass
// against that exact defect.
// ---------------------------------------------------------------------------------------------

/// One flushed segment of `rows` items, at unique external ids namespaced by `tag`.
fn flush_one_segment(engine: &Engine, tag: usize, rows: usize) -> Vec<EntityId> {
    let mut batch = Vec::new();
    for t in 0..rows {
        let external_id = format!("cfg-{tag}-{t}");
        let descriptors = vec![b"0".to_vec()];
        batch.push(UnallocatedRow {
            external_id: Some(external_id.into_bytes()),
            view: "s0".to_string(),
            join: None,
            x: ((t % 47) * 20) as f64,
            y: 5.0,
            scalars: Vec::new(),
            terms: engine.resolve_terms(&descriptors),
            descriptors,
        });
    }
    let entities = engine
        .accept_ingest(batch, format!("cfg-batch-{tag}"), [(101 + tag) as u8; 32])
        .expect("ingest is accepted");
    let flushes = engine.write_executor_stats().flushes;
    engine.request_flush();
    wait_until("the flush to publish", || {
        engine.write_executor_stats().flushes > flushes
    });
    entities
}

/// The live extent list as `(seg_id, entity_lo, entity_hi)`, in listed (entity) order.
fn extents_of(engine: &Engine) -> Vec<(String, u64, u64)> {
    engine.generation().bundle.partitions["default"].views["s0"]
        .row_space
        .extents()
        .iter()
        .map(|e| (e.seg_id.clone(), e.entity_lo, e.entity_hi))
        .collect()
}

/// **A configured `tier_width` reaches selection and changes when a merge fires.**
///
/// The fixture is two flushed extents — half the built-in width of 4, which can never select a
/// merge over them (`MergePolicy::select` needs `tier_width` same-class segments). The only way
/// this merge can fire is the configured 2 arriving at the policy, so against the inert-key
/// defect this test fails by timeout rather than passing vacuously.
#[test]
fn a_configured_tier_width_reaches_selection_and_changes_when_a_merge_fires() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );
    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: 3600,
            tier_width: Some(2),
            ..config_uncapped()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    engine.set_merge_for_test(false);

    let entities: Vec<EntityId> = (0..2)
        .flat_map(|s| flush_one_segment(&engine, s, ROWS_EACH))
        .collect();
    assert_eq!(extents_of(&engine).len(), 2, "one extent per flush");

    engine.set_merge_for_test(true);
    engine.request_flush();
    wait_until("the width-2 merge to publish", || {
        engine.write_executor_stats().merges >= 1
    });
    assert_eq!(
        extents_of(&engine).len(),
        1,
        "two extents merged into one at the configured width"
    );
    let after = rows_of(&engine, &entities);
    assert!(
        after.iter().all(Option::is_some),
        "every flushed entity still has a row: {after:?}"
    );
}

/// **A configured `segment_floor_bytes` reaches selection and changes *which* segments merge.**
///
/// Three extents: one large (128× the rows of the small pair), then two small of one size class.
/// With the configured floor of 1 byte, sizes compare by their own power-of-two class, so the
/// `[large, small]` window is skipped and the merge takes the two smalls — the large extent
/// survives. With the built-in 16 MiB floor every flush here clamps into one class and the first
/// qualifying window is `[large, small]`, consuming the large extent — so the surviving `seg_id`
/// is the configured floor observed at selection, not a fixture accident.
#[test]
fn a_configured_segment_floor_reaches_selection_and_changes_which_segments_merge() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );
    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: 3600,
            tier_width: Some(2),
            segment_floor_bytes: Some(1),
            ..config_uncapped()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    engine.set_merge_for_test(false);

    let large = flush_one_segment(&engine, 0, ROWS_EACH * 128);
    let small: Vec<EntityId> = (1..3)
        .flat_map(|s| flush_one_segment(&engine, s, ROWS_EACH))
        .collect();
    let before = extents_of(&engine);
    assert_eq!(before.len(), 3, "one extent per flush");
    let large_seg = before[0].0.clone();

    engine.set_merge_for_test(true);
    engine.request_flush();
    wait_until("the same-class merge to publish", || {
        engine.write_executor_stats().merges >= 1
    });

    let after = extents_of(&engine);
    assert_eq!(after.len(), 2, "the two same-class extents merged into one");
    assert_eq!(
        after[0].0, large_seg,
        "the large extent must survive: with the floor left at its built-in 16 MiB, every flush \
         here is one size class and the first window taken would be [large, small]"
    );
    assert_eq!(
        (after[1].1, after[1].2),
        (small[0].raw(), small[small.len() - 1].raw()),
        "and the merged extent spans exactly the two small segments' entities"
    );
    let rows = rows_of(
        &engine,
        &large.iter().chain(&small).copied().collect::<Vec<_>>(),
    );
    assert!(
        rows.iter().all(Option::is_some),
        "every flushed entity still has a row: {rows:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// The publication seam — decision 0071's pause site, held to the pause-site contract.
// ---------------------------------------------------------------------------------------------

/// **The executor parks between a merge's execution and its publication, and proceeds on
/// release** — the hook this module's doc recorded as missing, now held: arrive, block, proceed.
///
/// The parked state is the crash-mid-merge state (correctness-suite §10.1): the merged segment
/// exists on the pool's say-so and nothing else — no side-manifest names it, no swap has
/// happened, row space is unpermuted and the served set is untouched. A driver that kills a
/// server parked here restarts into a bundle whose inputs all stand and whose merge output is an
/// orphan, which is §12.3's discard rule for this seam.
///
/// Arrival alone cannot distinguish "parked" from "about to publish", so the case settles briefly
/// after the arrival before asserting nothing published — a `Stall` that failed to block becomes
/// a reliable failure rather than a racy pass.
#[test]
fn the_merge_publication_seam_parks_the_executor_between_execution_and_publication() {
    use tessera_lifecycle::faults::{PauseAction, PauseSite};
    const WAIT: Duration = Duration::from_secs(30);

    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        64,
    );
    let (engine, faults) = engine_at_with_faults(tmp.path(), &root);
    engine.set_merge_for_test(false);
    let items: Vec<(EntityId, String)> = flush_interleaved_segments(&engine)
        .into_iter()
        .flatten()
        .collect();
    let entities: Vec<EntityId> = items.iter().map(|(e, _)| *e).collect();
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    let served_before = served_ids(&engine, &session);
    let segments_before = segment_count(&engine);
    let rows_before = rows_of(&engine, &entities);

    faults.arm_pause(PauseSite::BeforeMergePublish, PauseAction::Stall);
    engine.set_merge_for_test(true);
    engine.request_flush();

    // Arrives — and an arrival here is itself proof the pool's execution completed, because
    // `publish_merge` is reached only with a `CompletedMerge` in hand.
    faults.await_arrivals(PauseSite::BeforeMergePublish, 1, WAIT);

    // Blocks: executed, and nothing published — no swap, no permutation, no served change.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        engine.write_executor_stats().merges,
        0,
        "a parked merge has published nothing"
    );
    assert_eq!(
        segment_count(&engine),
        segments_before,
        "the consumed segments all still stand while the executor is parked"
    );
    assert_eq!(
        rows_of(&engine, &entities),
        rows_before,
        "row space is unpermuted while the executor is parked"
    );
    assert_eq!(
        served_ids(&engine, &session),
        served_before,
        "the served set is untouched while the executor is parked"
    );

    // Proceeds: release, and the publication lands whole.
    faults.release();
    wait_until("the released merge publishes", || {
        engine.write_executor_stats().merges >= 1
    });
    assert!(
        segment_count(&engine) < segments_before,
        "the released merge brought the segment count down"
    );
    assert_ne!(
        rows_of(&engine, &entities),
        rows_before,
        "the released merge permuted row space — an identity permutation would make the \
         parked-state assertions above vacuous (see flush_interleaved_segments)"
    );
    assert_eq!(
        served_ids(&engine, &session),
        served_before,
        "and every item is served across it, at the same identities"
    );
}
