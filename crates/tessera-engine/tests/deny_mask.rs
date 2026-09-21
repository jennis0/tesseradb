//! The deny mask rides the generation (deny-lifecycle memo §1, §3).
//!
//! Composition used to answer the row-space question — *is this row denied* — by walking the deny
//! sets and resolving `row_of` per denied entity, on every request. That made per-request work grow
//! with denies **ever accepted**, which is a cost curve nothing retires: deletions wait on a
//! compaction fold that does not exist. `Generation::denied` is the same fact as
//! a row-space bitmap, folded into `compose`'s existing diffs with one `andnot`.
//!
//! **Two representations of one truth, so the differential test is the licence for holding them.**
//! The entity-space stores stay authoritative and `compose::verdict` stays the single answer
//! for `visible_to`, label gating and cluster visibility. `visible_to(e) ≡ contains_row(row_of(e))`
//! wherever a row exists is what keeps them from drifting, and is the reason the mask may exist at
//! all.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{Engine, EngineConfig, ViewportRequest};
use tessera_lifecycle::wal::ChangeOp;
use tessera_lifecycle::UnallocatedRow;
use tessera_types::EntityId;

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn engine_at(tmp: &Path, root: &Path, tick_secs: u64) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: tick_secs,
            // The shipped row trigger, four commit windows (`DEFAULT_FLUSH_MAX_ITEMS`):
            // what bounds the window close's O(buffered) copy. Nothing here reaches it.
            flush_max_items: 40_000,
            max_merged_segment_bytes: None,
            // Compaction §9's trigger is off unless a deployment configures one.
            compaction: tessera_engine::CompactionSchedule::off(),
            ..config_uncapped()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    engine
}

fn fixture(tmp: &Path) -> PathBuf {
    let root = tmp.join("bundle");
    build_fixture(
        &root,
        &tmp.join("points.parquet"),
        &tmp.join("pairs.parquet"),
    );
    root
}

/// Ingest one item at (5, 5) carrying the fixture's `ALL_TERM`.
fn ingest(engine: &Engine, external_id: &str) -> EntityId {
    let row = UnallocatedRow {
        external_id: Some(external_id.as_bytes().to_vec()),
        view: "s0".to_string(),
        join: None,
        descriptors: vec![b"0".to_vec()],
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[b"0".to_vec()]),
        scoped: Vec::new(),
    };
    engine
        .accept_ingest(vec![row], external_id.to_string(), [0u8; 32])
        .expect("ingest is accepted")[0]
}

fn change(engine: &Engine, entity: EntityId, op: ChangeOp) {
    engine
        .accept_change(entity, op)
        .expect("the change is accepted");
}

/// The whole fixture at a zoom that serves every visible row (θ saturated, caps raised).
fn whole_map() -> ViewportRequest<'static> {
    ViewportRequest::new("s0", 4, [0.0, 0.0, 1000.0, 1000.0], N_ITEMS as usize)
}

fn visible_count(engine: &Engine, session: &tessera_engine::Session) -> u64 {
    engine
        .viewport(session, whole_map())
        .expect("a viewport")
        .tiles
        .iter()
        .map(|t| t.visible)
        .sum()
}

/// The mask for `s0`, from the live generation — asserted directly, because the derivation rule is
/// what these tests are about and a count could be right for the wrong reason.
fn denied_rows(engine: &Engine) -> croaring::Bitmap {
    engine
        .generation()
        .denied()
        .get("s0")
        .expect("every view the bundle carries has an entry")
        .clone()
}

/// The entity-space route, as a client reaches it: `/v1/items` returns `None` for an identifier
/// that names nothing *and* for one that names an invisible item, which is `visible_to`'s answer.
fn visible_in_entity_space(
    engine: &Engine,
    session: &tessera_engine::Session,
    entity: EntityId,
) -> bool {
    let id = engine
        .tessera_id_of(entity)
        .expect("identity is computable");
    engine
        .item(session, id, None)
        .expect("the drill-down succeeds")
        .is_some()
}

/// The row-space route: every mark the viewport draws, by `tessera_id`.
fn drawn_marks(
    engine: &Engine,
    session: &tessera_engine::Session,
) -> std::collections::HashSet<u64> {
    engine
        .viewport(session, whole_map())
        .expect("a viewport")
        .points
        .iter()
        .map(|(id, _)| id.raw())
        .collect()
}

fn row_of(engine: &Engine, entity: EntityId) -> u32 {
    engine
        .generation()
        .bundle
        .partitions
        .values()
        .find_map(|p| p.views["s0"].row_space.row_of(entity))
        .expect("the entity has a row")
        .raw()
}

/// **The derivation rule's trap** (memo §1): after `delete → suppress → unsuppress` the row must
/// stay masked, because `deleted` still holds the entity.
///
/// **Mutation:** make the unsuppress subtract `row_of(e)` from the mask instead of re-deriving, and
/// this fails — a deleted item back on the map, with no error anywhere. That is the one way this
/// mask can fail open, and it is why any window carrying a removal re-derives from the union.
#[test]
fn unsuppress_after_delete_keeps_the_row_masked() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = engine_at(tmp.path(), &fixture(tmp.path()), 3600);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let before = visible_count(&engine, &session);

    // An entity from the build, so it has a row without needing a flush.
    let entity = EntityId::new(7);
    let row = row_of(&engine, entity);

    change(&engine, entity, ChangeOp::Delete);
    assert!(denied_rows(&engine).contains(row), "the delete masks it");

    change(&engine, entity, ChangeOp::Suppress);
    assert!(denied_rows(&engine).contains(row));

    change(&engine, entity, ChangeOp::Unsuppress);
    assert!(
        denied_rows(&engine).contains(row),
        "the unsuppress removed the suppression, but `deleted` still holds the entity: \
         re-derivation from the union is what keeps the row masked"
    );
    assert_eq!(
        visible_count(&engine, &session),
        before - 1,
        "and the composed answer agrees — the item is still gone from the map"
    );
    assert!(
        !visible_in_entity_space(&engine, &session, entity),
        "the entity-space verb agrees too, which is what the two representations must never stop \
         doing"
    );
}

/// A suppressed item still in the buffer has **no row**, so it appears in no mask — and it appears
/// in no viewport either, every map verb being a row-space question. The publication that gives it
/// a row is what puts that row into the mask.
#[test]
fn a_suppressed_buffered_item_is_masked_the_moment_it_gains_a_row() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let engine = engine_at(tmp.path(), &root, 1);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let before = visible_count(&engine, &session);

    let entity = ingest(&engine, "ext-buffered");
    change(&engine, entity, ChangeOp::Suppress);

    assert!(
        denied_rows(&engine).is_empty(),
        "buffered, so it has no row to be denied at — the mask is complete for what it governs"
    );
    assert_eq!(
        visible_count(&engine, &session),
        before,
        "and it is in no viewport either, suppressed or not: it has no geometry yet"
    );

    wait_until("the flush to publish", || {
        engine.write_executor_stats().flushes >= 1
    });

    let row = row_of(&engine, entity);
    assert!(
        denied_rows(&engine).contains(row),
        "the publication rebuilt the mask against the row space it just extended"
    );
    assert_eq!(
        visible_count(&engine, &session),
        before,
        "so the item never becomes visible: it went from rowless to masked with no window between"
    );
}

/// **The differential obligation** (memo §6): the entity-space verdict and the row-space mask agree
/// for every entity with a row, across every disposition. Two representations of one truth are
/// only licensed while this holds.
#[test]
fn visible_to_agrees_with_contains_row_for_every_disposition() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = engine_at(tmp.path(), &fixture(tmp.path()), 3600);
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    // One entity per disposition, plus one left alone.
    let deleted = EntityId::new(11);
    let suppressed = EntityId::new(12);
    let unsuppressed = EntityId::new(13);
    let untouched = EntityId::new(15);

    change(&engine, deleted, ChangeOp::Delete);
    change(&engine, suppressed, ChangeOp::Suppress);
    change(&engine, unsuppressed, ChangeOp::Suppress);
    change(&engine, unsuppressed, ChangeOp::Unsuppress);
    // Both routes as a client reaches them: `/v1/items` answers in entity space (`visible_to`
    // over the overlay and the buffer), and a drawn mark is the row-space mask's answer — caps are
    // raised and θ is saturated in this fixture, so selection serves every visible row and
    // "is it drawn" is exactly `contains_row`.
    let drawn = drawn_marks(&engine, &session);

    for entity in [deleted, suppressed, unsuppressed, untouched] {
        let row = row_of(&engine, entity);
        let by_entity = visible_in_entity_space(&engine, &session, entity);
        let by_row = drawn.contains(&engine.tessera_id_of(entity).unwrap().raw());
        assert_eq!(
            by_entity,
            by_row,
            "entity {} (row {row}): the entity-space verdict and the row-space mask disagree",
            entity.raw()
        );
    }
    // …and the fixture is doing what it claims: not all four agree by being uniformly visible.
    assert!(
        !visible_in_entity_space(&engine, &session, deleted)
            && visible_in_entity_space(&engine, &session, untouched),
        "the dispositions must actually differ, or this test proves nothing"
    );
}

/// Depth changes no answer. The *cost* claim — that per-request work no longer grows with denies
/// ever accepted — is the soak's to measure (Task 25); this asserts only that the mask computes the
/// same thing the walk did, at a depth where a per-entity walk would be doing real work.
#[test]
fn a_deep_deny_set_changes_no_answer() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = engine_at(tmp.path(), &fixture(tmp.path()), 3600);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let before = visible_count(&engine, &session);

    // Every third entity, suppressed. Submitted without waiting per entry, then drained.
    let victims: Vec<EntityId> = (0..N_ITEMS).step_by(3).map(EntityId::new).collect();
    let pending: Vec<_> = victims
        .iter()
        .map(|entity| {
            engine
                .submit_change(*entity, ChangeOp::Suppress)
                .expect("the change is submitted")
        })
        .collect();
    for p in pending {
        p.wait().expect("the change is applied");
    }

    assert_eq!(
        denied_rows(&engine).cardinality(),
        victims.len() as u64,
        "every suppression reached the mask"
    );
    assert_eq!(
        visible_count(&engine, &session),
        before - victims.len() as u64,
        "and the composed count is exactly the walk's answer, at depth"
    );
}

/// An engine over `root` that flushes only when asked, with the switchboard its executor reads.
/// Reopening over the same `tmp` reopens the same WAL, which is what a restart is here.
fn engine_flushing_only_on_request(
    tmp: &Path,
    root: &Path,
) -> (Engine, std::sync::Arc<tessera_lifecycle::faults::FaultSwitchboard>) {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: 3600,
            flush_max_items: usize::MAX,
            max_merged_segment_bytes: None,
            compaction: tessera_engine::CompactionSchedule::off(),
            ..config_uncapped()
        },
    )
    .expect("engine opens");
    let faults = std::sync::Arc::new(tessera_lifecycle::faults::FaultSwitchboard::new());
    engine
        .start_write_executor_with_faults(64, std::sync::Arc::clone(&faults))
        .expect("the executor starts once");
    (engine, faults)
}

/// **A join row can publish before the entity's own row**, and the entity's mark in the joined
/// view is drawn only by the buffer's `plus`.
///
/// A flush is planned per view and one view publishes per tick, so an entity ingested into `s0`
/// and joined to `s1` can have `s1`'s row published while its own row is still buffered. A join
/// writes no postings, so until `s0` flushes the entity is in no session's fragment and its `s1`
/// row is outside the cached projection: what puts the mark on the map is the walk over the
/// buffer in `compose`, which finds a row for a buffered entity here.
///
/// The executor is parked at the second flush's manifest seam, so the state is assembled rather
/// than waited for.
#[test]
fn a_join_published_before_the_entitys_own_row_is_drawn_from_the_buffer() {
    use tessera_lifecycle::faults::{PauseAction, PauseSite};

    const JOINED_VIEW: &str = "s1";
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let (engine, faults) = engine_flushing_only_on_request(tmp.path(), &root);

    engine
        .create_plain_view(tessera_engine::PlainViewDeclaration {
            name: JOINED_VIEW.to_string(),
            title: None,
            projection: "none".to_string(),
            frame: tessera_engine::DeclaredFrame {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            visibility: None,
            point_default: None,
        })
        .expect("the view is created");
    tick(&engine);

    // One row into each view, in one window, so both views have a plan at the same tick. The
    // anchor holds the older entity id — ids are assigned by signature then external id, and
    // "anchor" sorts below "joiner" — so `s1`'s plan is the one the dispatch sends first.
    let ingest_into = |batch: &str, view: &str, external_id: &str, descriptors: Vec<Vec<u8>>| {
        let row = UnallocatedRow {
            external_id: Some(external_id.as_bytes().to_vec()),
            view: view.to_string(),
            join: None,
            x: 5.0,
            y: 5.0,
            scalars: Vec::new(),
            terms: engine.resolve_terms(&descriptors),
            descriptors,
            scoped: Vec::new(),
        };
        engine
            .accept_ingest(vec![row], batch.to_string(), [0u8; 32])
            .expect("ingest is accepted")[0]
    };
    ingest_into("b-anchor", JOINED_VIEW, "anchor", vec![b"0".to_vec(), b"1".to_vec()]);
    let joiner = ingest_into("b-own", "s0", "joiner", vec![b"0".to_vec()]);
    // The same external id in the other view: the admission resolves it to `joiner` and the row
    // becomes a join, carrying geometry and no terms of its own.
    ingest_into("b-join", JOINED_VIEW, "joiner", vec![b"0".to_vec()]);

    // Let `s1`'s flush publish and park `s0`'s before it commits its side-manifest.
    faults.arm_pause_after(PauseSite::BeforeManifestPublish, PauseAction::Stall, 1);
    engine.request_flush();
    faults.await_arrivals(
        PauseSite::BeforeManifestPublish,
        2,
        Duration::from_secs(30),
    );

    let generation = engine.generation();
    let view_of = |view: &str| {
        generation
            .bundle
            .partitions
            .values()
            .find_map(|p| p.views.get(view))
            .expect("the view is in the generation")
    };
    assert!(
        view_of(JOINED_VIEW).row_space.row_of(joiner).is_some(),
        "the join row published, so the entity has a row in the joined view"
    );
    assert!(
        view_of("s0").row_space.row_of(joiner).is_none(),
        "its own row is still buffered: `s0` is the plan parked at the seam"
    );

    let whole = |view: &'static str| {
        ViewportRequest::new(view, 4, [0.0, 0.0, 1000.0, 1000.0], N_ITEMS as usize)
    };
    let entitled = engine.authorise(&full_coverage_credential()).unwrap();
    let unentitled = engine.authorise(&subset_credential()).unwrap();
    let served = |session: &tessera_engine::Session| {
        let response = engine
            .viewport(session, whole(JOINED_VIEW))
            .expect("a viewport");
        let counted: u64 = response.tiles.iter().map(|t| t.visible).sum();
        let drawn: std::collections::HashSet<u64> =
            response.points.iter().map(|(id, _)| id.raw()).collect();
        (counted, drawn)
    };
    let mark = engine.tessera_id_of(joiner).unwrap().raw();

    let (counted, drawn) = served(&entitled);
    assert!(
        drawn.contains(&mark),
        "the entity's mark is drawn in the joined view before its own row's flush"
    );
    assert_eq!(counted, 2, "and it is counted beside the anchor");

    let (counted, drawn) = served(&unentitled);
    assert!(
        !drawn.contains(&mark),
        "a session holding none of the entity's terms is served no mark for it"
    );
    assert_eq!(counted, 1, "and counts only the anchor");

    faults.release();
}

/// **The whole life of one entry whose join publishes before its own row**, asserted by what two
/// viewers are served at every step.
///
/// The state the `compose` walk exists for is assembled once, at the top, and then carried through
/// an unrelated ingest, a suppression, its lift, the entity's own flush and two restarts. Every
/// step asserts the served answer for a session holding the entity's term and for one holding none:
/// a walk that lost the entry would blank it for the entitled viewer, and one that stopped
/// consulting the overlay would serve it to the unentitled one.
///
/// The executor is killed at `s0`'s manifest seam rather than parked, because a parked executor
/// takes no writes: nothing durable names that publication's files, so the restart below replays
/// into exactly the state a park would have held.
#[test]
fn an_entrys_whole_life_serves_the_same_answer_at_every_step() {
    use tessera_lifecycle::faults::{PauseAction, PauseSite};

    const JOINED_VIEW: &str = "s1";
    const WHOLE: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

    /// What one credential is served in `view`: the summed visible count, and the marks drawn.
    /// A fresh session per call, as a viewer arriving now would have.
    fn served(
        engine: &Engine,
        credential: &[u8],
        view: &str,
    ) -> (u64, std::collections::HashSet<u64>) {
        let session = engine.authorise(credential).expect("the credential authorises");
        let response = engine
            .viewport(
                &session,
                ViewportRequest::new(view, 4, WHOLE, N_ITEMS as usize),
            )
            .expect("a viewport");
        (
            response.tiles.iter().map(|t| t.visible).sum(),
            response.points.iter().map(|(id, _)| id.raw()).collect(),
        )
    }

    fn row_in_view(engine: &Engine, view: &str, entity: EntityId) -> Option<u32> {
        engine
            .generation()
            .bundle
            .partitions
            .values()
            .find_map(|p| p.views.get(view))
            .expect("the view is in the generation")
            .row_space
            .row_of(entity)
            .map(|row| row.raw())
    }

    /// **The premise the `minus` side leans on**: no fragment covers an entity whose own row is
    /// unflushed, so the row a join published for it is outside the base projection and only the
    /// walk's `plus` can draw it. Asserted of the composed mask's own parts, at each step where the
    /// entity's own row is still buffered.
    fn assert_drawn_only_by_the_diff(
        engine: &Engine,
        credential: &[u8],
        view: &str,
        entity: EntityId,
    ) {
        let session = engine.authorise(credential).expect("the credential authorises");
        let (_, mask) = engine.composed_mask(&session, view).expect("a composed mask");
        let row = row_in_view(engine, view, entity).expect("the join row published");
        let (base, _, plus, _) = mask.parts();
        assert!(
            !base.contains(row),
            "a buffered entity is in no fragment, so its row cannot be in the base projection"
        );
        assert!(plus.contains(row), "the walk's `plus` is what draws it");
    }

    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let entitled = full_coverage_credential();
    let unentitled = subset_credential();

    // One row into each view, in one window, so both views have a plan at the same tick. The
    // anchor holds the older entity id — ids are assigned by signature then external id, and
    // "anchor" sorts below "joiner" — so `s1`'s plan is the one the dispatch sends first.
    let joiner = {
        let (engine, faults) = engine_flushing_only_on_request(tmp.path(), &root);
        engine
            .create_plain_view(tessera_engine::PlainViewDeclaration {
                name: JOINED_VIEW.to_string(),
                title: None,
                projection: "none".to_string(),
                frame: tessera_engine::DeclaredFrame {
                    x_min: 0.0,
                    x_max: 1000.0,
                    y_min: 0.0,
                    y_max: 1000.0,
                },
                visibility: None,
                point_default: None,
            })
            .expect("the view is created");
        tick(&engine);

        let ingest_into = |batch: &str, view: &str, external_id: &str, descriptors: Vec<Vec<u8>>| {
            let row = UnallocatedRow {
                external_id: Some(external_id.as_bytes().to_vec()),
                view: view.to_string(),
                join: None,
                x: 5.0,
                y: 5.0,
                scalars: Vec::new(),
                terms: engine.resolve_terms(&descriptors),
                descriptors,
                scoped: Vec::new(),
            };
            engine
                .accept_ingest(vec![row], batch.to_string(), [0u8; 32])
                .expect("ingest is accepted")[0]
        };
        ingest_into(
            "b-anchor",
            JOINED_VIEW,
            "anchor",
            vec![b"0".to_vec(), b"1".to_vec()],
        );
        let joiner = ingest_into("b-own", "s0", "joiner", vec![b"0".to_vec()]);
        ingest_into("b-join", JOINED_VIEW, "joiner", vec![b"0".to_vec()]);

        // `s1` publishes; `s0`'s publication dies at the seam with its files on disc and nothing
        // durable naming them.
        faults.arm_pause_after(PauseSite::BeforeManifestPublish, PauseAction::Panic, 1);
        engine.request_flush();
        faults.await_arrivals(PauseSite::BeforeManifestPublish, 2, Duration::from_secs(30));
        wait_until("the executor's death is reported", || {
            engine.write_executor_posture() == tessera_engine::ExecutorPosture::Dead
        });

        assert!(
            row_in_view(&engine, JOINED_VIEW, joiner).is_some(),
            "the join row published, so the entity has a row in the joined view"
        );
        assert!(
            row_in_view(&engine, "s0", joiner).is_none(),
            "its own row is still buffered: `s0` is the publication that died"
        );
        faults.release();
        joiner
    };

    let mark = {
        let (engine, _faults) = engine_flushing_only_on_request(tmp.path(), &root);
        let mark = engine.tessera_id_of(joiner).expect("identity is computable").raw();

        // Restart one, before the entity's own flush: replay buffers its own row again, and the
        // join row it dropped is the one `s1` already holds.
        assert!(
            row_in_view(&engine, JOINED_VIEW, joiner).is_some()
                && row_in_view(&engine, "s0", joiner).is_none(),
            "the restart replayed into the state the death left"
        );
        let (counted, drawn) = served(&engine, &entitled, JOINED_VIEW);
        assert!(drawn.contains(&mark), "drawn from the buffer after a restart");
        assert_eq!(counted, 2, "and counted beside the anchor");
        let (counted, drawn) = served(&engine, &unentitled, JOINED_VIEW);
        assert!(!drawn.contains(&mark), "and served to nobody who holds none of its terms");
        assert_eq!(counted, 1);
        assert_drawn_only_by_the_diff(&engine, &entitled, JOINED_VIEW, joiner);

        // Unrelated rows, which buffer beside it and move nothing it is served.
        let mut unrelated = Vec::new();
        for n in 0..3u32 {
            let row = UnallocatedRow {
                external_id: Some(format!("unrelated-{n}").into_bytes()),
                view: JOINED_VIEW.to_string(),
                join: None,
                x: 5.0,
                y: 5.0,
                scalars: Vec::new(),
                terms: engine.resolve_terms(&[b"0".to_vec()]),
                descriptors: vec![b"0".to_vec()],
                scoped: Vec::new(),
            };
            unrelated.extend(
                engine
                    .accept_ingest(vec![row], format!("b-unrelated-{n}"), [0u8; 32])
                    .expect("ingest is accepted"),
            );
        }
        let (counted, drawn) = served(&engine, &entitled, JOINED_VIEW);
        assert!(drawn.contains(&mark), "still drawn with three more rows buffered");
        assert_eq!(counted, 2, "and the unflushed rows are drawn for nobody");
        assert_drawn_only_by_the_diff(&engine, &entitled, JOINED_VIEW, joiner);

        // Suppressed: gone for everyone, from the moment it is accepted.
        change(&engine, joiner, ChangeOp::Suppress);
        let (counted, drawn) = served(&engine, &entitled, JOINED_VIEW);
        assert!(!drawn.contains(&mark), "a suppression takes the mark off the map");
        assert_eq!(counted, 1);
        assert_eq!(served(&engine, &unentitled, JOINED_VIEW).0, 1);

        // Lifted: its own terms decide again.
        change(&engine, joiner, ChangeOp::Unsuppress);
        let (counted, drawn) = served(&engine, &entitled, JOINED_VIEW);
        assert!(drawn.contains(&mark), "the lift restores the buffered verdict");
        assert_eq!(counted, 2);
        assert!(!served(&engine, &unentitled, JOINED_VIEW).1.contains(&mark));
        assert_drawn_only_by_the_diff(&engine, &entitled, JOINED_VIEW, joiner);

        // Its own row flushes: the same answer, now from the fragment rather than the diff. One
        // view publishes per tick, so the buffer is emptied a view at a time.
        let published = |engine: &Engine| {
            row_in_view(engine, "s0", joiner).is_some()
                && unrelated
                    .iter()
                    .all(|e| row_in_view(engine, JOINED_VIEW, *e).is_some())
        };
        for _ in 0..4 {
            if published(&engine) {
                break;
            }
            flush(&engine);
        }
        assert!(published(&engine), "every buffered row reached a segment");
        let (counted, drawn) = served(&engine, &entitled, JOINED_VIEW);
        assert!(drawn.contains(&mark), "drawn from the fragment after its own flush");
        assert_eq!(counted, 5, "with the three unrelated rows now published too");
        let (counted, drawn) = served(&engine, &unentitled, JOINED_VIEW);
        assert!(!drawn.contains(&mark), "and still served to nobody who holds none of its terms");
        assert_eq!(counted, 1);
        mark
    };

    // Restart two, after the flush: the same answers again, now out of the fragment.
    let (engine, _faults) = engine_flushing_only_on_request(tmp.path(), &root);
    let (counted, drawn) = served(&engine, &entitled, JOINED_VIEW);
    assert!(drawn.contains(&mark), "the restart serves what the flush left");
    assert_eq!(counted, 5);
    let (counted, drawn) = served(&engine, &unentitled, JOINED_VIEW);
    assert!(!drawn.contains(&mark));
    assert_eq!(counted, 1);
}
