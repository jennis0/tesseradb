//! The deny mask rides the generation (deny-lifecycle memo §1, §3).
//!
//! Composition used to answer the row-space question — *is this row denied* — by walking the deny
//! sets and resolving `row_of` per denied entity, on every request. That made per-request work grow
//! with denies **ever accepted**, which is a cost curve nothing retires: deletions and predicate
//! changes wait on a compaction fold that does not exist. `Generation::denied` is the same fact as
//! a row-space bitmap, folded into `compose`'s existing diffs with one `andnot`.
//!
//! **Two representations of one truth, so the differential test is the licence for holding them.**
//! The three entity-space stores stay authoritative and `compose::verdict` stays the single answer
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
            max_merged_segment_bytes: None,
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
        slice: "s0".to_string(),
        descriptors: vec![b"0".to_vec()],
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[b"0".to_vec()]),
    };
    engine
        .accept_ingest(vec![row], external_id.to_string(), [0u8; 32])
        .expect("ingest is accepted")[0]
}

fn change(engine: &Engine, entity: EntityId, op: ChangeOp) {
    engine
        .accept_change(entity, op, None)
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
        .denied
        .get("s0")
        .expect("every slice the bundle carries has an entry")
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
        .map(|p| p.tessera_id.raw())
        .collect()
}

fn row_of(engine: &Engine, entity: EntityId) -> u32 {
    engine
        .generation()
        .bundle
        .partitions
        .values()
        .find_map(|p| p.slices["s0"].row_space.row_of(entity))
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
/// for every entity with a row, across all four dispositions. Two representations of one truth are
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
    let predicated = EntityId::new(14);
    let untouched = EntityId::new(15);

    change(&engine, deleted, ChangeOp::Delete);
    change(&engine, suppressed, ChangeOp::Suppress);
    change(&engine, unsuppressed, ChangeOp::Suppress);
    change(&engine, unsuppressed, ChangeOp::Unsuppress);
    engine
        .accept_change(predicated,
            ChangeOp::Predicate,
            // A term this session does not hold, so the predicate flips it invisible.
            Some(vec![b"1".to_vec()]),
        )
        .expect("the predicate change is accepted");

    // Both routes as a client reaches them: `/v1/items` answers in entity space (`visible_to`
    // over the overlay and the buffer), and a drawn mark is the row-space mask's answer — caps are
    // raised and θ is saturated in this fixture, so selection serves every visible row and
    // "is it drawn" is exactly `contains_row`.
    let drawn = drawn_marks(&engine, &session);

    for entity in [deleted, suppressed, unsuppressed, predicated, untouched] {
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
    // …and the fixture is doing what it claims: not all five agree by being uniformly visible.
    assert!(
        !visible_in_entity_space(&engine, &session, deleted)
            && visible_in_entity_space(&engine, &session, untouched),
        "the four dispositions must actually differ, or this test proves nothing"
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
                .submit_change(*entity, ChangeOp::Suppress, None)
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
