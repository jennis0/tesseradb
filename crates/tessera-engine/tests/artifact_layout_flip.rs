//! **The flip: a level published artifact-major, folded, and served row-major afterwards.**
//!
//! [decision 0094](../../../docs/decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)'s
//! fourth clause is that the fold re-evaluates the automatic choice from the shape it observes. That
//! is the clause with the most machinery behind it and the least visible symptom when it is wrong: a
//! flip taken *after* the registry snapshot would publish a level in the old layout with a record
//! claiming the new one, and the reader would then either adopt a file the manifest mis-describes or
//! quietly recompose one on every request for the process's life.
//!
//! **The flip here is the real heuristic on a real shape, not a hook.** The fixture is large enough
//! that a membership spread across it touches ten or more Roaring containers — which is what
//! `blocks per artifact` counts, and what the threshold is expressed in — and holds more artifacts
//! than the count tiebreak wants. Nothing overrides anything: the level is registered with no pin,
//! is artifact-major at publication because a level with no artifacts has no shape to observe, and
//! flips when the fold looks at what actually landed.
//!
//! It is in a file of its own because it builds a corpus two orders of magnitude larger than the
//! other artifact fixtures — the smallest one in which the measured axis is expressible at all.

mod common;

use common::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{ArtifactOut, Engine};
use tessera_lifecycle::IncomingArtifact;
use tessera_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource, ServingLayout,
};
use tessera_types::EntityId;

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];
const LAYER: &str = "clusters/scattered";

/// Rows enough for eleven Roaring containers — a container is 65 536 ids, and the threshold the
/// automatic pick compares against is ten. A fixture of ten thousand rows is *one* container, so the
/// axis the heuristic is expressed in cannot be exercised at that size at all.
const ROWS: u64 = 700_000;
/// Above [`tessera_engine::layout::ROW_MAJOR_MIN_ARTIFACTS`], which is the count tiebreak.
const ARTIFACTS: u64 = 1_100;
/// Members per artifact, drawn uniformly, so a membership touches essentially every container:
/// `11 x (10/11)^100` is under a thousandth of a container missed in expectation.
const MEMBERS: usize = 100;

fn declaration() -> LayerDeclaration {
    LayerDeclaration {
        name: LAYER.into(),
        title: Some("scattered".into()),
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
        content: ContentDeclaration::default(),
        depends_on: Vec::new(),
        levels: Vec::new(),
        // **No pin.** The whole point is that the fold's own observation moves the record.
        layout: None,
    }
}

fn fold(engine: &Engine) {
    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
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
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Every served artifact as a client sees it, ordered by the publisher's key.
fn answers(engine: &Engine, credential: &[u8]) -> Vec<(Option<String>, u64)> {
    let session = engine.authorise(credential).unwrap();
    let mut out: Vec<(Option<String>, u64)> = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, ROWS as usize),
        )
        .expect("a viewport")
        .artifacts
        .iter()
        .map(|a: &ArtifactOut| (a.key.clone(), a.masked_count))
        .collect();
    out.sort();
    out
}

fn files(root: &std::path::Path, engine: &Engine, dir: &str) -> Vec<std::path::PathBuf> {
    let dir = root
        .join(&engine.generation().prefix)
        .join("partitions")
        .join("default")
        .join(dir);
    std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect()
}

/// **The whole clause, straddled.** The level is artifact-major at publication, the fold observes a
/// shape that argues otherwise, and every answer is the same on both sides of the flip.
#[test]
fn the_fold_flips_a_scattered_level_and_the_answers_do_not_move() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        ROWS,
    );
    let map = source_to_new_map(&root, "v00000");
    let engine = open_engine_publishing(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    engine.set_background_refresh_for_test(false);
    engine.register_layer(declaration()).unwrap();

    // Memberships drawn uniformly over the corpus, so each touches essentially every container —
    // the `scattered` shape §5 of the scale design names, and the one the row-major layout exists
    // for. They overlap heavily, which is what makes the fold choose the **list** form.
    let mut rng = StdRng::seed_from_u64(0x5ca77e5ed);
    let batch: Vec<IncomingArtifact> = (0..ARTIFACTS)
        .map(|i| {
            let members: Vec<EntityId> = (0..MEMBERS)
                .map(|_| EntityId::new(map[&rng.gen_range(0..ROWS)]))
                .collect();
            IncomingArtifact::from_entities(Some(format!("a{i}")), members)
        })
        .collect();
    engine.publish_artifacts(LAYER.into(), 0, batch).unwrap();

    // **Artifact-major at publication**, because a registration has no shape to observe and the
    // conservative pick is the form every derived structure already exists for.
    assert_eq!(
        engine.recorded_layout(LAYER, 0),
        Some(ServingLayout::ArtifactMajor)
    );
    let before_broad = answers(&engine, &full_coverage_credential());
    let before_narrow = answers(&engine, &subset_credential());
    assert_eq!(before_broad.len(), ARTIFACTS as usize);
    assert_eq!(
        engine.columns_composed(),
        0,
        "an artifact-major level composes no column"
    );

    fold(&engine);

    // **The flip.** The fold observed a scattered level of eleven hundred artifacts and moved the
    // record; the memberships overlap, so the form is the list one and not the label one.
    assert_eq!(
        engine.recorded_layout(LAYER, 0),
        Some(ServingLayout::RowMajorList),
        "the fold's own observation is what moved the record — nothing here pins anything"
    );

    // **The answers straddle it unchanged**, which is the claim the whole decision rests on.
    assert_eq!(answers(&engine, &full_coverage_credential()), before_broad);
    assert_eq!(answers(&engine, &subset_credential()), before_narrow);
    assert_eq!(
        engine.layout_fallbacks(),
        0,
        "the list form represents an overlapping level, so nothing falls back"
    );

    // **The old form's files are not named and the new form's are.** The fold rewrites every level
    // into the prefix it publishes, so what it did not write is simply absent from the manifest —
    // which is how a flip drops what nothing will read again.
    let columns = files(&root, &engine, "row-column");
    assert!(!columns.is_empty(), "the fold wrote the list column");
    assert!(
        files(&root, &engine, "tile-index").is_empty(),
        "and wrote no tile index for a level that has nothing to index"
    );
    let extents: Vec<_> = engine
        .generation()
        .bundle
        .partitions
        .values()
        .flat_map(|p| p.manifest.row_column_extents.iter().cloned())
        .collect();
    assert_eq!(
        extents.len(),
        columns.len(),
        "the manifest and the files agree"
    );
    for extent in &extents {
        assert_eq!(extent.layout, ServingLayout::RowMajorList);
        assert!(root
            .join(&engine.generation().prefix)
            .join(&extent.path)
            .exists());
    }
    let index_extents: usize = engine
        .generation()
        .bundle
        .partitions
        .values()
        .map(|p| p.manifest.tile_index_extents.len())
        .sum();
    assert_eq!(
        index_extents, 0,
        "the flipped level's old-form entries are gone from the manifest"
    );

    // **A restart adopts the new form at its coordinate** rather than recomposing it — which is the
    // difference between a fold that saved the work and one that merely did it twice.
    drop(engine);
    let reopened = open_engine_publishing(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    reopened.set_background_refresh_for_test(false);
    assert_eq!(
        reopened.recorded_layout(LAYER, 0),
        Some(ServingLayout::RowMajorList),
        "the record's durable home is the manifest the fold wrote"
    );
    assert_eq!(
        answers(&reopened, &full_coverage_credential()),
        before_broad
    );
    assert_eq!(answers(&reopened, &subset_credential()), before_narrow);
    assert!(reopened.columns_adopted() > 0);
    assert_eq!(
        reopened.columns_composed(),
        0,
        "nothing was recomposed, which is what writing the column bought"
    );
}
