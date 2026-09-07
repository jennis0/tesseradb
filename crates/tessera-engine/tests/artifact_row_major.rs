//! **The twin differential: the layout decides what a request costs and never what it answers.**
//!
//! [decision 0094](../../../docs/decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)
//! rests on one claim — *both layouts answer identically* — and that claim is what makes a layout a
//! latency record rather than a contract: nothing on the wire names one, so a fold may flip one
//! freely. If the claim is false the failure is silent in both directions. A row-major level that
//! dropped a candidate serves an artifact as absent, which a viewer cannot tell from one that failed
//! its criterion; one that counted a row twice serves a number that is simply wrong, with no error
//! anywhere.
//!
//! So the same corpus is built and served **twice**, once with each layout pinned through the
//! declaration's own `layout` key — which tests the override end to end at the same time — and the
//! two responses are compared artifact for artifact: served set, masked count, content rank, parent,
//! and the derived geometry beside them. Over principals, viewports, overlay states, a flat layer
//! and a treed one, and before and after both a fold and a growth.
//!
//! The comparison is on the **publisher's key** rather than on the `tessera_id`, deliberately: an
//! identifier is a blinding of an entity id, and two bundles built independently are entitled to
//! issue different ones. What must not differ is which artifacts are served and what is said about
//! them. Parents are compared by the key they resolve to *within the same response*, which is the
//! only thing a parent identifier means to a client.

mod common;

use std::collections::BTreeMap;

use common::*;
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{ArtifactOut, Engine};
use tessera_lifecycle::membership::IncomingContent;
use tessera_lifecycle::wal::ChangeOp;
use tessera_lifecycle::IncomingArtifact;
use tessera_types::layer::{
    ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind, LayerDeclaration,
    MembershipSource, ServingLayout,
};
use tessera_types::EntityId;

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];
const FLAT: &str = "clusters/flat";
const TREED: &str = "clusters/treed";

/// The corpus is `N_ITEMS` points at `x = (e * 37) % 1000`, `y = (e * 53) % 1000`, so these four
/// boxes are genuinely different slices of row space rather than four spellings of the same one.
/// The whole map is first because it is the cell the two layouts are furthest apart on in cost —
/// and must be identical in answer.
const VIEWPORTS: [[f64; 4]; 4] = [
    WHOLE_MAP,
    [0.0, 0.0, 500.0, 500.0],
    [250.0, 250.0, 750.0, 750.0],
    [900.0, 0.0, 1000.0, 120.0],
];

fn declaration(
    name: &str,
    kind: HierarchyKind,
    criterion: Option<ExistenceCriterion>,
    layout: Option<ServingLayout>,
) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: Some(format!("{name} (title)")),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: criterion,
        hierarchy: Hierarchy {
            kind,
            prune_children: false,
        },
        // **Derived content is declared**, so the differential covers the one thing computed from
        // `membership ∩ M_auth` rather than counted over it: a centroid taken over a different set
        // from the count served beside it is the disagreement a viewer would see and could not
        // explain.
        content: ContentDeclaration {
            computed: vec!["centroid".into(), "box".into()],
            // **Ranked supplied content on the flat layer**, so the differential covers containment
            // as well as the count: a viewer is served the first content whose generating set they
            // hold entire, and which rank that is has to be the same under either layout. The treed
            // layer declares none, so both states — a layer with contents and a layer without — are
            // in the sweep.
            supplied: if name == FLAT {
                vec![tessera_types::layer::SuppliedContent {
                    name: "label".into(),
                    ty: "text".into(),
                    require_member_visibility: tessera_types::layer::SuppliedRequirement::All,
                }]
            } else {
                Vec::new()
            },
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
    /// Source id to entity id, read once at build. **Once, because a fold reclaims the prefix it
    /// would otherwise be read from** — and these cases deliberately move the overlay after one.
    /// Entity ids do not move at a fold, so one reading is good for the fixture's life.
    map: BTreeMap<u64, u64>,
}

fn fixture() -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let map = source_to_new_map(&root, "v00000");
    Fixture {
        root,
        cache: tmp.path().join("cache"),
        wal: tmp.path().join("wal.log"),
        map,
        _tmp: tmp,
    }
}

impl Fixture {
    fn open(&self) -> Engine {
        let engine = open_engine_publishing(&self.root, &self.cache, &self.wal);
        engine.set_background_refresh_for_test(false);
        engine
    }

    fn members(&self, source_ids: impl Iterator<Item = u64>) -> Vec<EntityId> {
        source_ids.map(|s| EntityId::new(self.map[&s])).collect()
    }

    fn member(&self, source_id: u64) -> EntityId {
        self.members(std::iter::once(source_id))[0]
    }

    fn row_column_files(&self, engine: &Engine) -> Vec<std::path::PathBuf> {
        let dir = self
            .root
            .join(&engine.generation().prefix)
            .join("partitions")
            .join("default")
            .join("row-column");
        std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect()
    }

    fn tile_index_files(&self, engine: &Engine) -> Vec<std::path::PathBuf> {
        let dir = self
            .root
            .join(&engine.generation().prefix)
            .join("partitions")
            .join("default")
            .join("tile-index");
        std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect()
    }
}

/// One served artifact, as a client sees it and with every identifier resolved to something two
/// independently built bundles can be expected to agree on.
#[derive(Debug, Clone, PartialEq)]
struct Served {
    layer: String,
    key: Option<String>,
    masked_count: u64,
    /// The keys of the parents named — every one in the same response. Empty is *no parent
    /// named*, which covers a root and a parent withheld from this viewer alike — the ambiguity is
    /// deliberate on the wire and is kept here.
    parent_keys: Vec<Option<String>>,
    centroid: Option<[f64; 2]>,
    bbox: Option<[u32; 4]>,
    content: Vec<String>,
}

fn served(artifacts: &[ArtifactOut]) -> Vec<Served> {
    let by_id: BTreeMap<_, _> = artifacts
        .iter()
        .map(|a| (a.tessera_id, a.key.clone()))
        .collect();
    let mut out: Vec<Served> = artifacts
        .iter()
        .map(|a| Served {
            layer: a.layer.clone(),
            key: a.key.clone(),
            masked_count: a.masked_count,
            parent_keys: a
                .parent_ids
                .iter()
                .map(|id| by_id.get(id).cloned().flatten())
                .collect(),
            centroid: a.derived.centroid,
            bbox: a.derived.bbox,
            content: a.content.clone(),
        })
        .collect();
    // Ordered by what the two bundles agree on, so the comparison is of sets rather than of the
    // order the walk happened to produce — which is legitimately different between the routes.
    out.sort_by(|a, b| (&a.layer, &a.key).cmp(&(&b.layer, &b.key)));
    out
}

/// Every `(credential, viewport)` cell, as the served set.
fn sweep(engine: &Engine) -> Vec<(usize, usize, Vec<Served>)> {
    let credentials = [
        full_coverage_credential(),
        subset_credential(),
        zero_credential(),
    ];
    let mut out = Vec::new();
    for (c, credential) in credentials.iter().enumerate() {
        let session = engine.authorise(credential).unwrap();
        for (v, bbox) in VIEWPORTS.iter().enumerate() {
            let response = engine
                .viewport(
                    &session,
                    ViewportRequest::new("s0", 0, *bbox, N_ITEMS as usize),
                )
                .expect("a viewport");
            out.push((c, v, served(&response.artifacts)));
        }
    }
    out
}

fn assert_same(
    left: &[(usize, usize, Vec<Served>)],
    right: &[(usize, usize, Vec<Served>)],
    what: &str,
) {
    assert_eq!(left.len(), right.len(), "{what}: different sweep shapes");
    for ((lc, lv, l), (rc, rv, r)) in left.iter().zip(right) {
        assert_eq!((lc, lv), (rc, rv));
        assert_eq!(
            l, r,
            "{what}: credential {lc} at viewport {lv} — the two layouts disagreed. \
             A layout decides what a request costs and never what it answers"
        );
    }
    assert!(
        left.iter().any(|(_, _, set)| !set.is_empty()),
        "{what}: the sweep served nothing anywhere, so it asserts nothing"
    );
    // **Containment has to be doing something**, or the content column is a constant and comparing
    // it asserts nothing about which rank a viewer was served.
    let labels: std::collections::BTreeSet<&Vec<String>> = left
        .iter()
        .flat_map(|(_, _, set)| set.iter().map(|s| &s.content))
        .collect();
    assert!(
        labels.len() > 2,
        "{what}: the sweep saw {} distinct contents, so the rank served is a constant and the \
         comparison says nothing about containment",
        labels.len()
    );
}

/// Request a fold and block until it has published.
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

fn wait_for_publication(fx: &Fixture, engine: &Engine, files: usize) {
    let dir = fx
        .root
        .join(&engine.generation().prefix)
        .join("partitions")
        .join("default")
        .join("members");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let held = std::fs::read_dir(&dir).into_iter().flatten().count();
        if held >= files {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the membership extents were never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// One artifact of the flat layer: a key, its members, and two ranked labels over `sample` — the
/// wide rank first and a third of it as the narrow one, so the rank a viewer is served is a real
/// answer rather than a constant.
fn labelled(fx: &Fixture, key: &str, members: Vec<u64>, sample: Vec<u64>) -> IncomingArtifact {
    let mut artifact =
        IncomingArtifact::from_entities(Some(key.into()), fx.members(members.into_iter()));
    let narrow: Vec<u64> = sample.iter().copied().filter(|s| s % 9 == 0).collect();
    artifact.contents = vec![
        IncomingContent::new(
            vec![format!("wide label for {key}")],
            fx.members(sample.into_iter()),
        ),
        IncomingContent::new(
            vec![format!("narrow label for {key}")],
            fx.members(narrow.into_iter()),
        ),
    ];
    artifact
}

/// **A partitioning population**: sixteen disjoint blocks of the corpus, which is the shape a
/// single-valued attribute predicate produces and the one a label column can represent.
fn partitioning(fx: &Fixture) -> Vec<IncomingArtifact> {
    (0..16u64)
        .map(|block| {
            let lo = block * 500;
            labelled(
                fx,
                &format!("p{block}"),
                (lo..(lo + 500).min(N_ITEMS)).collect(),
                (lo..(lo + 12).min(N_ITEMS)).collect(),
            )
        })
        .collect()
}

/// A treed population over the same corpus: four parents of four children each, the children
/// disjoint and each parent's membership the union of its children's — so the level **overlaps** and
/// only the list form can represent it.
///
/// The parents come first in the batch, because an edge names a target that must already exist.
fn treed(fx: &Fixture) -> Vec<IncomingArtifact> {
    let mut out = Vec::new();
    for parent in 0..4u64 {
        let lo = parent * 2_000;
        out.push(IncomingArtifact::from_entities(
            Some(format!("t{parent}")),
            fx.members(lo..(lo + 2_000).min(N_ITEMS)),
        ));
    }
    for parent in 0..4u64 {
        let lo = parent * 2_000;
        for child in 0..4u64 {
            let clo = lo + child * 500;
            let mut node = IncomingArtifact::from_entities(
                Some(format!("t{parent}.{child}")),
                fx.members(clo..(clo + 500).min(N_ITEMS)),
            );
            node.parent_keys = vec![format!("t{parent}")];
            out.push(node);
        }
    }
    out
}

/// Build one bundle, publish the fixture's two layers under `layout`, and return the engine.
///
/// **The pin travels through the declaration**, which is what makes this a test of the override as
/// well as of the two routes.
fn published(
    fx: &Fixture,
    flat: Option<ServingLayout>,
    treed_layout: Option<ServingLayout>,
) -> Engine {
    let engine = fx.open();
    engine
        .register_layer(declaration(
            FLAT,
            HierarchyKind::Flat,
            Some(ExistenceCriterion::Fraction(0.05)),
            flat,
        ))
        .unwrap();
    engine
        .register_layer(declaration(
            TREED,
            HierarchyKind::Nested,
            None,
            treed_layout,
        ))
        .unwrap();
    engine
        .publish_artifacts(FLAT.into(), 0, partitioning(fx))
        .unwrap();
    engine
        .publish_artifacts(TREED.into(), 0, treed(fx))
        .unwrap();
    wait_for_publication(fx, &engine, 1);
    engine
}

/// **The stage's spine.** The same corpus under each layout, swept over principals × viewports,
/// before and after a fold and a growth, with the overlay moved under both.
#[test]
fn the_two_layouts_answer_identically() {
    // A flat partitioning layer and an overlapping treed one, so both column forms are exercised
    // against the artifact-major route they must match.
    for (flat, treed_layout) in [
        (ServingLayout::RowMajorLabel, ServingLayout::RowMajorList),
        (ServingLayout::RowMajorList, ServingLayout::RowMajorList),
    ] {
        let major_fx = fixture();
        let minor_fx = fixture();
        let major = published(
            &major_fx,
            Some(ServingLayout::ArtifactMajor),
            Some(ServingLayout::ArtifactMajor),
        );
        let minor = published(&minor_fx, Some(flat), Some(treed_layout));

        assert_same(&sweep(&major), &sweep(&minor), "at publication");
        // **The differential asserts nothing if the row-major side quietly fell back**, which is
        // the way a test like this passes while testing the artifact-major route twice. Both pinned
        // levels compose their column, and neither falls back.
        assert_eq!(
            minor.recorded_layout(FLAT, 0),
            Some(flat),
            "the declaration's pin is the record"
        );
        assert_eq!(minor.recorded_layout(TREED, 0), Some(treed_layout));
        assert_eq!(
            minor.layout_fallbacks(),
            0,
            "{flat:?}/{treed_layout:?}: a pinned level fell back, so this sweep compared \
             artifact-major against artifact-major"
        );
        assert!(
            minor.columns_composed() >= 2,
            "both pinned levels composed a column"
        );
        assert_eq!(
            major.columns_composed(),
            0,
            "and the artifact-major twin composed none"
        );

        // **The overlay, moved under both.** A suppression and a deletion each take rows out of the
        // composed mask, which on the row-major route means the histogram's key has to have moved —
        // a stale entry would serve a count over documents the viewer may no longer see.
        for (fx, engine) in [(&major_fx, &major), (&minor_fx, &minor)] {
            engine
                .accept_change(fx.member(7), ChangeOp::Suppress)
                .unwrap();
            engine
                .accept_change(fx.member(11), ChangeOp::Delete)
                .unwrap();
            engine
                .accept_change(fx.member(4_001), ChangeOp::Suppress)
                .unwrap();
        }
        assert_same(&sweep(&major), &sweep(&minor), "after a deny");

        // **A growth**: more artifacts on the same level, which moves the level version and so
        // every derived structure's coordinate — the column and the histogram included.
        for (fx, engine) in [(&major_fx, &major), (&minor_fx, &minor)] {
            engine
                .publish_artifacts(
                    FLAT.into(),
                    0,
                    // **Not the deleted member.** A batch naming a deleted entity is refused — a
                    // deleted member contributes to no count and would be published into silence —
                    // so the growth is over what is left.
                    vec![labelled(
                        fx,
                        "grown",
                        (0..N_ITEMS).filter(|s| *s != 11).collect(),
                        vec![0, 3, 6],
                    )],
                )
                .unwrap();
        }
        // **The growth is deliberately a membership over the whole corpus**, so a level pinned to
        // a label column stops partitioning and falls back — the second half of the pin's refusal,
        // reached at a growth rather than at a fold. The answers are unchanged either way, which is
        // what the comparison below says.
        assert_same(&sweep(&major), &sweep(&minor), "after a growth");
        if flat == ServingLayout::RowMajorLabel {
            assert!(
                minor.layout_fallbacks() > 0,
                "a level pinned `column` that stops partitioning must fall back"
            );
        }

        // **A fold**, which renumbers row space wholesale, executes the deletion, and writes the
        // columns and indexes into the new prefix for the next request to adopt.
        fold(&major);
        fold(&minor);
        assert_same(&sweep(&major), &sweep(&minor), "after a fold");

        // And an unsuppress, which must **re-derive**: `delete → suppress → unsuppress` leaves the
        // entity deleted, and the two routes have to agree about that too.
        for (fx, engine) in [(&major_fx, &major), (&minor_fx, &minor)] {
            engine
                .accept_change(fx.member(7), ChangeOp::Unsuppress)
                .unwrap();
            engine
                .accept_change(fx.member(11), ChangeOp::Suppress)
                .unwrap();
            engine
                .accept_change(fx.member(11), ChangeOp::Unsuppress)
                .unwrap();
        }
        assert_same(
            &sweep(&major),
            &sweep(&minor),
            "after delete then suppress then unsuppress",
        );
    }
}

/// **The drill-down takes the same route.** An artifact reachable by identifier but not by viewport
/// — or answered with a different number — is two transcriptions of one rule, and a row-major level
/// reaching a masked count through a histogram rather than through its own membership is exactly
/// where that would happen.
#[test]
fn a_drill_down_agrees_with_the_viewport_under_either_layout() {
    for layout in [
        ServingLayout::ArtifactMajor,
        ServingLayout::RowMajorLabel,
        ServingLayout::RowMajorList,
    ] {
        let fx = fixture();
        let engine = published(&fx, Some(layout), Some(ServingLayout::ArtifactMajor));
        let idset = engine.generation().bundle.manifest.identity.idset;
        for credential in [full_coverage_credential(), subset_credential()] {
            let session = engine.authorise(&credential).unwrap();
            let response = engine
                .viewport(
                    &session,
                    ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
                )
                .expect("a viewport");
            assert!(!response.artifacts.is_empty());
            for artifact in &response.artifacts {
                // ⊘ A cold drill-down on a row-major level pays the level's whole histogram; this
                // is where that is exercised as well as asserted.
                let alone = engine
                    .artifact(&session, artifact.tessera_id, Some(idset), "s0", None)
                    .expect("the identifier resolves")
                    .expect("and the artifact is served to the viewer the viewport served it to");
                assert_eq!(
                    alone.masked_count, artifact.masked_count,
                    "{layout:?}: the drill-down and the viewport disagreed about {:?}",
                    artifact.key
                );
                assert_eq!(alone.key, artifact.key);
                assert_eq!(alone.derived.centroid, artifact.derived.centroid);
            }
        }
    }
}

/// **A level pinned `column` whose memberships overlap is served artifact-major, loudly.** The
/// refusal cannot be made at parse — single-valuedness is a property of the data — so this is the
/// fail-closed reading of the pin: the level is served correctly and the record is reported as not
/// having been honoured, rather than a column being written whose labels would each be whichever
/// artifact wrote last.
#[test]
fn a_label_pin_on_an_overlapping_level_falls_back_and_says_so() {
    let fx = fixture();
    // The treed population overlaps by construction: a parent's membership is the union of its
    // children's.
    let engine = published(
        &fx,
        Some(ServingLayout::ArtifactMajor),
        Some(ServingLayout::RowMajorLabel),
    );
    let answers = sweep(&engine);

    assert!(
        engine.layout_fallbacks() > 0,
        "the overlapping level was pinned to a label column and must have fallen back"
    );

    // And the answers are the artifact-major ones, artifact for artifact.
    let control_fx = fixture();
    let control = published(
        &control_fx,
        Some(ServingLayout::ArtifactMajor),
        Some(ServingLayout::ArtifactMajor),
    );
    assert_same(&answers, &sweep(&control), "a fallen-back label pin");

    // The fold writes no column for it either, and the level keeps its tile index — a fallback is
    // not a level with neither structure.
    fold(&engine);
    let _ = sweep(&engine);
    assert!(
        fx.row_column_files(&engine).is_empty(),
        "no column is written for a level whose memberships do not partition"
    );
    assert!(
        !fx.tile_index_files(&engine).is_empty(),
        "and the artifact-major structures are still written for it"
    );
}

/// **A fold over a pinned row-major level**: it writes the column and no tile index, the manifest
/// and the files agree, the answers straddle it unchanged, and a restart adopts the column at its
/// coordinate rather than composing one.
///
/// The *record* moving is the other half of this, and it needs a level whose observed shape argues
/// for a flip — which a ten-thousand-row fixture cannot express, a Roaring container being 65 536
/// ids. That half is `tests/artifact_layout_flip.rs`, over the smallest corpus in which the
/// heuristic's own axis exists.
#[test]
fn a_fold_over_a_row_major_level_writes_its_column_and_changes_no_answer() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration(
            FLAT,
            HierarchyKind::Flat,
            Some(ExistenceCriterion::Fraction(0.05)),
            Some(ServingLayout::RowMajorLabel),
        ))
        .unwrap();
    engine
        .publish_artifacts(FLAT.into(), 0, partitioning(&fx))
        .unwrap();
    wait_for_publication(&fx, &engine, 1);

    let before = sweep(&engine);
    fold(&engine);
    let after = sweep(&engine);
    assert_eq!(
        before, after,
        "a fold that writes a level's column changes nothing a client sees"
    );
    assert!(
        !fx.row_column_files(&engine).is_empty(),
        "the fold wrote the row-major column"
    );
    assert!(
        fx.tile_index_files(&engine).is_empty(),
        "and wrote no tile index for a level that has nothing to index"
    );

    let extents: Vec<_> = engine
        .generation()
        .bundle
        .partitions
        .values()
        .flat_map(|p| p.manifest.row_column_extents.iter().cloned())
        .collect();
    assert_eq!(extents.len(), fx.row_column_files(&engine).len());
    for extent in &extents {
        assert_eq!(extent.layout, ServingLayout::RowMajorLabel);
        assert!(fx
            .root
            .join(&engine.generation().prefix)
            .join(&extent.path)
            .exists());
    }
    drop(engine);

    let reopened = fx.open();
    assert_eq!(
        sweep(&reopened),
        after,
        "a restart serves what the fold published"
    );
    assert!(
        reopened.columns_adopted() > 0,
        "and it adopted the fold's column rather than composing one"
    );
    assert_eq!(
        reopened.columns_composed(),
        0,
        "which is the point of writing it: nothing was recomposed"
    );
    assert_eq!(
        reopened.recorded_layout(FLAT, 0),
        Some(ServingLayout::RowMajorLabel)
    );
    assert_eq!(
        reopened.layout_fallbacks(),
        0,
        "a level that adopted its column has not fallen back"
    );
}

/// **The histogram cache's edges**: a deny moves the count on the next request, a growth moves it,
/// and a budget of nothing is a slower answer rather than a wrong one.
#[test]
fn the_masked_count_cache_is_bounded_and_a_deny_is_not_outlived() {
    let fx = fixture();
    let engine = published(
        &fx,
        Some(ServingLayout::RowMajorLabel),
        Some(ServingLayout::ArtifactMajor),
    );
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let ask = |engine: &Engine| -> BTreeMap<Option<String>, u64> {
        engine
            .viewport(
                &session,
                ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
            )
            .expect("a viewport")
            .artifacts
            .into_iter()
            .filter(|a| a.layer == FLAT)
            .map(|a| (a.key, a.masked_count))
            .collect()
    };

    let first = ask(&engine);
    assert!(!first.is_empty());
    // The second request is a hit, which is what the exception buys.
    let second = ask(&engine);
    assert_eq!(first, second);
    assert!(
        engine.masked_count_cache_stats().hits > 0,
        "a second request over the same level must read the held histogram"
    );

    // **The deny edge.** Source id 400 is in block `p0` and in none of its generating sets — so
    // suppressing it moves the *count* without moving containment, which is what this case is
    // about. It must take one off on the **next** request, not at the next refresh.
    let before = first[&Some("p0".to_string())];
    engine
        .accept_change(fx.member(400), ChangeOp::Suppress)
        .unwrap();
    let after = ask(&engine);
    assert_eq!(
        after[&Some("p0".to_string())],
        before - 1,
        "a suppression moves the count on the next request; a stale histogram would not"
    );

    // `delete → suppress → unsuppress` leaves the entity deleted, here as everywhere.
    engine
        .accept_change(fx.member(401), ChangeOp::Delete)
        .unwrap();
    engine
        .accept_change(fx.member(401), ChangeOp::Suppress)
        .unwrap();
    engine
        .accept_change(fx.member(401), ChangeOp::Unsuppress)
        .unwrap();
    let deleted = ask(&engine);
    assert_eq!(
        deleted[&Some("p0".to_string())],
        before - 2,
        "the unsuppress re-derives, so the deleted member does not come back"
    );

    // **A growth** moves the level version, and the counts move with it.
    engine
        .publish_artifacts(
            FLAT.into(),
            0,
            // Source id 401 is the deleted one, and a batch naming a deleted entity is refused.
            vec![labelled(&fx, "grown", (2..400).collect(), vec![9, 18, 27])],
        )
        .unwrap();
    let grown = ask(&engine);
    assert!(
        grown.contains_key(&Some("grown".to_string())),
        "the grown artifact is served"
    );
    assert_eq!(
        grown[&Some("p0".to_string())],
        deleted[&Some("p0".to_string())],
        "and the artifacts that did not move keep their counts"
    );

    // **A budget of one byte admits nothing**, and the answers are unchanged: eviction only ever
    // removes, so a rebuilt histogram is the one that was evicted.
    engine.set_masked_count_cache_bytes(1);
    let starved = ask(&engine);
    assert_eq!(
        starved, grown,
        "a cache that admits nothing is slower, not wrong"
    );
    assert_eq!(
        engine.masked_count_cache_stats().entries,
        0,
        "and nothing is held"
    );
}

/// The entity behind one served artifact of the flat layer, by its key, through the admin
/// plane's own resolver — the route `/control/changes` takes.
fn flat_artifact_entity(engine: &Engine, key: &str) -> EntityId {
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let response = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
        )
        .expect("a viewport over the whole map");
    let id = response
        .artifacts
        .iter()
        .find(|a| a.layer == FLAT && a.key.as_deref() == Some(key))
        .expect("the artifact is served")
        .tessera_id;
    let idset = engine.generation().bundle.manifest.identity.idset;
    engine.resolve_tessera_ids(&[id], idset).unwrap()[0].expect("it names what was issued")
}

/// The row of each of `sources` in the served row space, for a column check.
fn rows_of(fx: &Fixture, engine: &Engine, sources: std::ops::Range<u64>) -> Vec<u32> {
    let generation = engine.generation();
    let space = &generation.bundle.partitions["default"].views["s0"].row_space;
    fx.members(sources)
        .into_iter()
        .map(|entity| space.row_of(entity).expect("every member has a row").raw())
        .collect()
}

/// **A fold that retires a member of a row-major level writes the level's column, and the
/// publication transposes it rather than projecting the level.** The column is the memberships
/// addressed by row over the folded row space, in which a retired member has no row, so it is the
/// same column before and after the retirement shrinks the membership; the fold composes it from
/// the records the retirement leaves and stamps it with the version the level has once the
/// retirement has run. What is served is what the artifact-major twin serves, live and after a
/// restart.
///
/// The retired member is outside every generating set (`labelled` samples the first twelve of
/// each block), so the retirement moves the level through its membership alone and no content
/// rank shifts: a restart after a strict withdrawal reads the withdrawn content's text at the
/// surviving rank, under either layout, and that is not this case's subject.
#[test]
fn a_fold_that_retires_a_member_writes_the_column_the_publication_transposes() {
    let fx = fixture();
    let engine = published(
        &fx,
        Some(ServingLayout::RowMajorLabel),
        Some(ServingLayout::ArtifactMajor),
    );
    engine
        .accept_change(fx.member(13), ChangeOp::Delete)
        .expect("the delete is accepted");
    fold(&engine);
    assert!(
        !fx.row_column_files(&engine).is_empty(),
        "the fold wrote the level's column although the retirement moved the level"
    );
    assert_eq!(
        engine.columns_adopted(),
        1,
        "the publication adopted the column and its warm transposed it"
    );
    assert_eq!(
        engine.columns_composed(),
        0,
        "nothing was composed from a projected form"
    );
    let live = sweep(&engine);

    let twin_fx = fixture();
    let twin = published(
        &twin_fx,
        Some(ServingLayout::ArtifactMajor),
        Some(ServingLayout::ArtifactMajor),
    );
    twin.accept_change(twin_fx.member(13), ChangeOp::Delete)
        .unwrap();
    fold(&twin);
    assert_same(&live, &sweep(&twin), "after a fold that retired a member");
    drop(engine);

    let reopened = fx.open();
    assert_eq!(
        sweep(&reopened),
        live,
        "the restart serves what the live engine served"
    );
    assert_eq!(
        reopened.columns_adopted(),
        1,
        "the stated version is the one the level restarts at, so the restart claims the column"
    );
    assert_eq!(reopened.columns_composed(), 0);
}

/// Retire the flat artifact `key` by its own entity, fold, and check the column the fold writes
/// leaves it out: the publication transposes the column, no row of `retired` carries a label,
/// every row of `survivor` carries one, the artifact is served to nobody, and the artifact-major
/// twin agrees.
fn own_entity_retired(key: &str, retired: std::ops::Range<u64>, survivor: std::ops::Range<u64>) {
    let fx = fixture();
    let engine = published(
        &fx,
        Some(ServingLayout::RowMajorLabel),
        Some(ServingLayout::ArtifactMajor),
    );
    let entity = flat_artifact_entity(&engine, key);
    engine
        .accept_change(entity, ChangeOp::Delete)
        .expect("an artifact takes a deletion like any other entity");
    fold(&engine);
    assert_eq!(
        engine.columns_adopted(),
        1,
        "the warm transposed the fold's column"
    );

    let form = engine
        .held_artifact_form_for_test("s0", FLAT, 0)
        .expect("the level's form is held");
    let column = form.column().expect("the level is served row-major");
    let labels_at = |row: u32| {
        let mut at = Vec::new();
        column.for_each_label(row, |ordinal| at.push(ordinal));
        at
    };
    for row in rows_of(&fx, &engine, retired) {
        assert!(
            labels_at(row).is_empty(),
            "row {row} was a member of the retired artifact and is labelled with nothing"
        );
    }
    let surviving: std::collections::BTreeSet<Vec<u32>> = rows_of(&fx, &engine, survivor)
        .into_iter()
        .map(labels_at)
        .collect();
    assert_eq!(
        surviving.len(),
        1,
        "every row of the surviving artifact carries its one ordinal"
    );
    assert_eq!(surviving.iter().next().unwrap().len(), 1);
    let live = sweep(&engine);
    assert!(
        !live
            .iter()
            .any(|(_, _, set)| set.iter().any(|s| s.key.as_deref() == Some(key))),
        "the retired artifact is served to nobody"
    );

    let twin_fx = fixture();
    let twin = published(
        &twin_fx,
        Some(ServingLayout::ArtifactMajor),
        Some(ServingLayout::ArtifactMajor),
    );
    let twin_entity = flat_artifact_entity(&twin, key);
    twin.accept_change(twin_entity, ChangeOp::Delete).unwrap();
    fold(&twin);
    assert_same(
        &live,
        &sweep(&twin),
        "after a fold that retired an artifact",
    );
}

/// **A fold that retires an artifact's own entity leaves that artifact out of the column it
/// writes.** The column is composed from the records the retirement leaves, so no row carries the
/// retired artifact's ordinal; the survivors' rows carry theirs. Checked on the column the
/// publication adopted, row by row, and against the artifact-major twin. The retired artifact
/// sits between live ordinals.
#[test]
fn a_fold_that_retires_an_artifacts_own_entity_leaves_it_out_of_the_column() {
    own_entity_retired("p3", 1_500..2_000, 1_000..1_500);
}

/// **The top ordinal retired.** The reader refuses a column shorter than one past the highest
/// live ordinal and the fold sizes its column the same way, so the column it writes for a level
/// whose last artifact left still covers every survivor.
#[test]
fn a_fold_that_retires_the_top_artifact_writes_a_column_the_survivors_fit() {
    own_entity_retired("p15", 7_500..8_000, 7_000..7_500);
}

/// **A column whose coordinate has moved is not adopted**, and the level recomposes on first use.
///
/// The direction of the mistake is what makes equality the only admissible test: a growth adds rows
/// the column does not label, and an unlabelled row is one no artifact claims — so an artifact
/// holding it silently stops being a candidate there and its masked count comes back short. That is
/// a *narrower* answer with nothing reporting a fault, which is exactly what an existence criterion
/// then renders as absence.
#[test]
fn a_column_the_level_has_moved_past_is_recomposed_rather_than_adopted() {
    let fx = fixture();
    let engine = published(
        &fx,
        Some(ServingLayout::RowMajorLabel),
        Some(ServingLayout::ArtifactMajor),
    );
    fold(&engine);
    let _ = sweep(&engine);
    assert!(
        !fx.row_column_files(&engine).is_empty(),
        "the fold wrote a column to be stale about"
    );

    // The level moves after the fold wrote its column: a growth the column has never labelled.
    engine
        .publish_artifacts(
            FLAT.into(),
            0,
            vec![labelled(
                &fx,
                "after-the-fold",
                (9_000..N_ITEMS).collect(),
                vec![9_000, 9_003],
            )],
        )
        .unwrap();
    let grown = sweep(&engine);
    drop(engine);

    let reopened = fx.open();
    assert_eq!(
        sweep(&reopened),
        grown,
        "the restart serves what the live engine served"
    );
    assert_eq!(
        reopened.columns_adopted(),
        0,
        "the fold's column describes a level version the store has moved past"
    );
    assert!(
        reopened.columns_composed() > 0,
        "so it is recomposed on first use, which is what every request did before the fold wrote \
         anything"
    );
}
