//! **A write amends the level's row form; it does not invalidate it.**
//!
//! A growth, a publication and a flush each change what a level's row form should say, and each of
//! them used to change it by moving a coordinate the form was filed under — so the next request
//! naming that level projected the whole thing again. At rung 3 that was 94 to 177 s inside a
//! request against a 60 s stream deadline
//! (`docs/evidence/memos/2026-09-03-post-flush-artifact-frames.md`). The deltas are small and they
//! are known where they are accepted, so they are applied there.
//!
//! **The file's spine is the last test.** Amending a held form and building one from scratch are
//! two routes to one structure, and a maintained form that is *close* is a masked count that is
//! wrong with nothing anywhere to notice — which is the failure mode the whole artifact stage is
//! written against. So `an_amended_form_equals_one_built_from_scratch` compares the two ordinal for
//! ordinal, bitmap for bitmap, extent for extent and, on the row-major route, count for count. The
//! cases above it say which writes are covered and what a viewer sees; that one says the answer is
//! the same object.

mod common;

use common::*;
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{ArtifactOut, Engine};
use tessera_lifecycle::{IncomingArtifact, IncomingGrowth};
use tessera_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource, ServingLayout,
};
use tessera_types::EntityId;

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];
const LAYER: &str = "clusters/a";

/// **No existence criterion**, as in the fold's and the growth's own cases: these assertions are
/// about what a count *is*, and a criterion would turn a wrong count into an absence, which is the
/// weaker of the two observations.
fn declaration(name: &str, layout: Option<ServingLayout>) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: Some(format!("{name} (title)")),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: vec!["centroid".into()],
            supplied: Vec::new(),
            withdraw_on_member_deletion: true,
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout,
        shape: None,
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    cache: std::path::PathBuf,
    wal: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    Fixture {
        root,
        cache: tmp.path().join("cache"),
        wal: tmp.path().join("wal.log"),
        _tmp: tmp,
    }
}

impl Fixture {
    fn open(&self) -> Engine {
        let engine = open_engine_publishing(&self.root, &self.cache, &self.wal);
        engine.set_background_refresh_for_test(false);
        engine
    }

    /// The corpus entities behind a run of source ids.
    fn members(&self, source_ids: std::ops::Range<u64>) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        source_ids.map(|s| EntityId::new(map[&s])).collect()
    }
}

fn artifacts_of(engine: &Engine) -> Vec<ArtifactOut> {
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
        )
        .expect("a viewport over the whole map")
        .artifacts
}

/// Every served artifact's key and masked count, ascending by key — what a viewer is told, which
/// is what every assertion here is finally about.
fn served(engine: &Engine) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = artifacts_of(engine)
        .into_iter()
        .map(|a| (a.key.unwrap_or_default(), a.masked_count))
        .collect();
    out.sort();
    out
}

fn publish(engine: &Engine, key: &str, members: Vec<EntityId>) {
    engine
        .publish_artifacts(
            LAYER.into(),
            0,
            vec![IncomingArtifact::from_entities(Some(key.into()), members)],
        )
        .expect("a publication into a registered layer");
}

fn grow(engine: &Engine, key: &str, members: Vec<EntityId>) {
    engine
        .grow_memberships(
            LAYER.into(),
            0,
            vec![IncomingGrowth::from_entities(key.into(), members)],
        )
        .expect("points joining an artifact that exists is an ordinary write");
}

/// One ingested point, under a batch key used once — a second ingest under a key already seen is
/// *replayed* rather than accepted, so a fixed key would silently ingest nothing the second time.
fn ingest(engine: &Engine, external_id: &[u8]) -> EntityId {
    let descriptors = vec![b"0".to_vec()];
    let mut key = [0u8; 32];
    for (slot, byte) in key.iter_mut().zip(external_id) {
        *slot = *byte;
    }
    let row = tessera_lifecycle::command::UnallocatedRow {
        external_id: Some(external_id.to_vec()),
        view: "s0".to_string(),
        join: None,
        descriptors: descriptors.clone(),
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&descriptors),
        scoped: Vec::new(),
    };
    engine
        .accept_ingest(
            vec![row],
            String::from_utf8_lossy(external_id).into_owned(),
            key,
        )
        .expect("the ingest is accepted")[0]
}

fn flush(engine: &Engine) {
    let before = engine.write_executor_stats().flushes;
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while engine.write_executor_stats().flushes == before {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn fold(engine: &Engine) {
    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        if now.folds > before.folds {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

// ---- (a) the delta reaches the held form ---------------------------------------------------

/// **A growth is served from the form that was already warm.**
///
/// The count moves and the build counter does not: the level's version moved, the held form was
/// amended with the entities that joined, and its key moved with it. A form left to be rebuilt
/// would answer the same and cost the level's whole projection to do it.
#[test]
fn a_growth_is_applied_to_the_warm_form_rather_than_rebuilding_it() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration(LAYER, None)).unwrap();
    publish(&engine, "a0", fx.members(0..100));
    publish(&engine, "a1", fx.members(500..600));

    // Warm: whatever the first request derives, it derives before the growth.
    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 100), ("a1".to_string(), 100)]
    );
    let warm = engine.artifact_cache_builds().0;

    grow(&engine, "a0", fx.members(100..150));

    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 150), ("a1".to_string(), 100)],
        "the members that joined are in the very next request's count, and the artifact that did \
         not grow is where it was"
    );
    assert_eq!(
        engine.artifact_cache_builds().0,
        warm,
        "nothing was projected again: the delta the write accepted is the delta the form took"
    );
}

/// **A publication into a warm level is the same story one ordinal along.** The new artifact serves
/// from the form that was already held, which the level's version move would otherwise have
/// discarded whole.
#[test]
fn a_publication_is_applied_to_the_warm_form_rather_than_rebuilding_it() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration(LAYER, None)).unwrap();
    publish(&engine, "a0", fx.members(0..100));
    assert_eq!(served(&engine), vec![("a0".to_string(), 100)]);
    let warm = engine.artifact_cache_builds().0;

    publish(&engine, "a1", fx.members(500..600));

    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 100), ("a1".to_string(), 100)],
        "the artifact published second is served on the next request, at its own count"
    );
    assert_eq!(
        engine.artifact_cache_builds().0,
        warm,
        "the new ordinal was placed in the form that was held, not projected beside a new one"
    );
}

// ---- (c) the flush extends the form by the segment it publishes -----------------------------

/// **An ingested member counts from its flush, before any fold.**
///
/// The member has no row while it is buffered and an *extent* row once the flush publishes; the
/// form covers the whole row space, and the flush extends every held form by the segment it wrote.
/// The fold that follows renumbers the row and changes no count, which is the second half of the
/// same assertion: what the fold does here is move rows, not find members.
#[test]
fn an_ingested_member_counts_at_its_flush_and_the_fold_changes_nothing() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration(LAYER, None)).unwrap();

    let fresh = ingest(&engine, b"joins-a0");
    let mut members = fx.members(0..100);
    members.push(fresh);
    publish(&engine, "a0", members);

    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 100)],
        "buffered: the member has no row anywhere, so it is in no count"
    );

    flush(&engine);
    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 101)],
        "flushed: it holds an extent row and the flush put that segment's rows into the form"
    );

    fold(&engine);
    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 101)],
        "folded: the same member on a base row — the fold renumbers what is counted, not how many"
    );
}

/// The same for a member that joins by **growth** after its own flush: it holds an extent row
/// already, so what has to reach the form is the join rather than the segment.
#[test]
fn a_member_that_joins_after_its_flush_counts_at_the_growth() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(declaration(LAYER, None)).unwrap();
    publish(&engine, "a0", fx.members(0..100));

    let fresh = ingest(&engine, b"joins-later");
    flush(&engine);
    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 100)],
        "the flushed point is in no artifact yet"
    );

    grow(&engine, "a0", vec![fresh]);
    assert_eq!(
        served(&engine),
        vec![("a0".to_string(), 101)],
        "an extent row joins a membership exactly as a base row does"
    );
}

// ---- the differential ------------------------------------------------------------------------

/// Every observable half of one level's row form, in a shape two forms can be compared on.
fn form_of(engine: &Engine, layer: &str) -> Vec<String> {
    let rows = engine
        .held_artifact_form_for_test("s0", layer, 0)
        .expect("the level's form is held");
    let mut out = vec![format!(
        "ordinals={} layout={:?} row_count={} everywhere={}",
        rows.len(),
        rows.layout(),
        rows.index().row_count(),
        rows.index().everywhere()
    )];
    for ordinal in 0..rows.len() as u32 {
        let membership = rows
            .get(ordinal)
            .map(|bitmap| bitmap.to_vec())
            .map(|v| format!("{v:?}"))
            .unwrap_or_else(|| "hole".to_string());
        let generating: Vec<String> = rows
            .membership()
            .generating(ordinal)
            .iter()
            .map(|set| format!("{:?}", set.to_vec()))
            .collect();
        // The row-major route's own count of an artifact's rows, which is what a column answers
        // from and the artifact-major route never reads.
        let declared = rows
            .column()
            .map(|column| column.declared_size(ordinal).to_string())
            .unwrap_or_else(|| "-".to_string());
        out.push(format!(
            "{ordinal}: rows={membership} extent={:?} generating={generating:?} declared={declared}",
            rows.index().extent(ordinal)
        ));
    }
    out
}

/// **The maintained form is the built form.**
///
/// A publication, a growth, a flush and a growth over the flushed row all amend one held form in
/// place; the same sequence is then compared against the form built from scratch over the same
/// records and the same row space. Equal ordinal for ordinal, row bitmap for row bitmap, extent
/// for extent, generating set for generating set — and on the row-major route the column's own
/// per-artifact count too, which is the number that route serves from.
///
/// Run under both layouts, because they are the two routes a count is reached by and only one of
/// them holds a column at all.
#[test]
fn an_amended_form_equals_one_built_from_scratch() {
    for layout in [ServingLayout::ArtifactMajor, ServingLayout::RowMajorLabel] {
        let fx = fixture();
        let engine = fx.open();
        engine
            .register_layer(declaration(LAYER, Some(layout)))
            .unwrap();

        // Warm the form, then move it by every route that moves it.
        publish(&engine, "a0", fx.members(0..100));
        let warm = served(&engine);
        assert_eq!(warm, vec![("a0".to_string(), 100)]);

        publish(&engine, "a1", fx.members(500..600));
        grow(&engine, "a0", fx.members(100..150));
        let fresh = ingest(&engine, b"differential");
        flush(&engine);
        grow(&engine, "a1", vec![fresh]);

        let maintained_answers = served(&engine);
        let maintained = form_of(&engine, LAYER);
        // Both routes have to be *taken*, or the row-major run asserts the artifact-major one
        // twice: a level whose memberships turned out to overlap falls back, correctly and
        // silently, and these two are disjoint on purpose so it does not.
        assert!(
            maintained[0].contains(&format!("layout={layout:?}")),
            "{layout:?}: the amended form fell back to another layout, so this run proves nothing \
             about the one it names — {}",
            maintained[0]
        );
        assert_eq!(
            maintained_answers,
            vec![("a0".to_string(), 150), ("a1".to_string(), 101)],
            "{layout:?}: the maintained form's counts are the memberships' own sizes"
        );

        // From scratch: every derived structure for this layer dropped, and the next request
        // projects the level over the same records and the same row space.
        engine.forget_artifact_forms_for_test(LAYER);
        let rebuilt_answers = served(&engine);
        let rebuilt = form_of(&engine, LAYER);

        assert_eq!(
            maintained_answers, rebuilt_answers,
            "{layout:?}: a viewer is told the same thing by the two forms"
        );
        assert_eq!(
            maintained.len(),
            rebuilt.len(),
            "{layout:?}: the two forms cover the same ordinals"
        );
        for (amended, built) in maintained.iter().zip(&rebuilt) {
            assert_eq!(
                amended, built,
                "{layout:?}: the amended form and the built one describe different levels"
            );
        }
    }
}
