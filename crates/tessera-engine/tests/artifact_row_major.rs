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

    fn open_with(&self, config: tessera_engine::EngineConfig) -> Engine {
        let mut engine = Engine::open(
            &self.root,
            &self.cache,
            &self.wal,
            config,
        )
        .expect("the engine opens");
        engine.start_write_executor(8).expect("the executor starts once");
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
    publish_into(fx, &engine, flat, treed_layout);
    engine
}

fn publish_into(
    fx: &Fixture,
    engine: &Engine,
    flat: Option<ServingLayout>,
    treed_layout: Option<ServingLayout>,
) {
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
    wait_for_publication(fx, engine, 1);
}

/// `n` points ingested at spread positions, every other one visible to the subset principal as
/// well as the full one.
fn ingest_points(engine: &Engine, batch: &str, n: u64) -> Vec<EntityId> {
    let rows = (0..n)
        .map(|i| {
            let descriptors = if i % 2 == 0 {
                vec![b"0".to_vec(), b"1".to_vec()]
            } else {
                vec![b"0".to_vec()]
            };
            tessera_lifecycle::UnallocatedRow {
                view: "s0".to_string(),
                join: None,
                terms: engine.resolve_terms(&descriptors),
                descriptors,
                x: ((i * 97) % 1000) as f64 + 0.5,
                y: ((i * 61) % 1000) as f64 + 0.5,
                scalars: Vec::new(),
                scoped: Vec::new(),
            }
        })
        .collect();
    engine
        .ingest_rows(rows, batch.to_string(), [0u8; 32])
        .expect("ingest is accepted")
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
            tick(engine);
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

        // **Rows above the fold's base, in a segment of their own, labelled by an amendment.**
        // Points ingested now are published by a flush into a new segment, and a growth puts them
        // in an artifact of each layer, so the counts and the geometry are read across segments
        // and from the column's amendment as well as its pack.
        for engine in [&major, &minor] {
            let fresh = ingest_points(engine, "past-the-fold", 40);
            tick(engine);
            let growth = |key: &str, members: &[EntityId]| {
                tessera_lifecycle::IncomingGrowth::from_entities(key.into(), members.to_vec())
            };
            engine
                .grow_memberships(FLAT.into(), 0, vec![growth("p3", &fresh[..20])])
                .expect("a growth into an artifact that exists");
            engine
                .grow_memberships(
                    TREED.into(),
                    0,
                    vec![growth("t2", &fresh[20..]), growth("t2.1", &fresh[20..])],
                )
                .expect("a growth into an artifact that exists");
            tick(engine);
        }
        assert_same(
            &sweep(&major),
            &sweep(&minor),
            "after a flush and a growth past the fold",
        );

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
                    .artifact(&session, artifact.tessera_id, "s0", None)
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
        "and wrote no tile index for a level that has nothing to index: a level served from its \
         column derives its extents from the column's own bytes"
    );

    let extents: Vec<_> = engine
        .generation()
        .bundle
        .partitions
        .values()
        .flat_map(|p| p.manifest.derived_extents.iter().cloned())
        .filter(|e| matches!(e.form, tessera_store::manifest::DerivedForm::RowColumn { .. }))
        .collect();
    assert_eq!(extents.len(), fx.row_column_files(&engine).len());
    for extent in &extents {
        assert_eq!(
            extent.form,
            tessera_store::manifest::DerivedForm::RowColumn {
                layout: ServingLayout::RowMajorLabel
            }
        );
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
    // **And it built no artifact-major form at all.** The column is the level's membership and its
    // own bytes give every artifact's extent, so the level is served from the one file: the column
    // answers candidacy, the counts and the declared sizes, and the extents place each artifact in
    // the tile index. The sweep above compared the centroid and the bounding box of every served
    // artifact, each of which is a function of `membership ∩ M_auth` alone, so this assertion is
    // claiming the accumulated route produced them.
    let form = reopened
        .held_artifact_form_for_test("s0", FLAT, 0)
        .expect("the sweep left the level's form held");
    assert!(
        !form.membership().rows_held(),
        "a level served from its column builds no artifact-major form"
    );
    // **And the drill-down agrees with the viewport on such a level.** It is a different route to
    // `membership ∩ M_auth` — `Engine::artifact` derives the geometry from the artifact alone —
    // and on a column-only form both routes take the column walk rather than a held bitmap.
    //
    // **The drill-down is taken from a second session**, so its derived geometry is computed
    // rather than read from the per-principal cache the viewport just filled: the two routes
    // share that cache by design, and comparing them inside one session compares one answer with
    // itself. A client following a saved link reaches this route cold.
    for credential in [full_coverage_credential(), subset_credential()] {
        let looking = reopened.authorise(&credential).unwrap();
        let response = reopened
            .viewport(
                &looking,
                ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
            )
            .expect("a viewport");
        assert!(!response.artifacts.is_empty());
        assert!(
            response.artifacts.iter().any(|a| a.derived.centroid.is_some()),
            "the layer derives a centroid, or this comparison asserts nothing"
        );
        let cold = reopened.authorise(&credential).unwrap();
        for artifact in &response.artifacts {
            let alone = reopened
                .artifact(&cold, artifact.tessera_id, "s0", None)
                .expect("the identifier resolves")
                .expect("and the artifact is served to the viewer the viewport served it to");
            assert_eq!(alone.masked_count, artifact.masked_count);
            assert_eq!(
                alone.derived.centroid, artifact.derived.centroid,
                "the drill-down and the viewport disagreed about {:?}",
                artifact.key
            );
            assert_eq!(alone.derived.bbox, artifact.derived.bbox);
        }
    }
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
        engine.figures_stats().hits > 0,
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

    // **A growth of an artifact the level already holds, asked inside the window between the
    // write and the tick.** That window is where the store and the row form disagree about the
    // level version: `ArtifactProjections::get_or_build` hands back the level as last published,
    // and the store already counts the write. A histogram of that form filed under the *store's*
    // version is the entry every later request in the session reads once the tick has moved the
    // form to it — and on a row-major level the histogram decides existence as well as the number
    // beside it, so an artifact whose only visible rows arrived in the growth would be withheld
    // for the life of the key.
    //
    // **The ordinal count does not move here, which is what makes this the case that matters.** A
    // publication resizes the column, so candidacy's length check against the histogram catches a
    // stale entry; a growth leaves both at the level's ordinal count. Rows 8,000 onwards are in no
    // artifact of this layer, so the memberships stay disjoint and the level keeps its column.
    engine
        .grow_memberships(
            FLAT.into(),
            0,
            vec![tessera_lifecycle::IncomingGrowth::from_entities(
                "p0".into(),
                fx.members(8_000..8_500),
            )],
        )
        .expect("points joining an artifact that exists is an ordinary write");
    let in_window = ask(&engine);
    assert_eq!(
        in_window[&Some("p0".to_string())],
        deleted[&Some("p0".to_string())],
        "the form in the window is the level as last published, so the count is the pre-growth one"
    );
    tick(&engine);
    let joined = ask(&engine);
    assert_eq!(
        joined[&Some("p0".to_string())],
        deleted[&Some("p0".to_string())] + 500,
        "the growth reaches the count on the first request after the tick; a histogram of the \
         pre-tick form filed under the store's version would still be the pre-growth one"
    );

    // **A publication** moves the level version too, and the counts move with it.
    engine
        .publish_artifacts(
            FLAT.into(),
            0,
            // Source id 401 is the deleted one, and a batch naming a deleted entity is refused.
            vec![labelled(&fx, "grown", (2..400).collect(), vec![9, 18, 27])],
        )
        .unwrap();
    let _ = ask(&engine);
    tick(&engine);
    let grown = ask(&engine);
    assert!(
        grown.contains_key(&Some("grown".to_string())),
        "the grown artifact is served"
    );
    assert_eq!(
        grown[&Some("p0".to_string())],
        joined[&Some("p0".to_string())],
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
        engine.figures_stats().entries,
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
    engine.resolve_tessera_ids(&[id]).unwrap()[0].expect("it names what was issued")
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

/// **A column the level has moved past is completed, not recomposed.** A membership only grows
/// between folds, so the fold's column labels a subset of the level: the restart takes it and adds
/// the rows it misses, rather than projecting the level whole and composing a column again.
///
/// The direction of the mistake is why the answers are compared: a growth adds rows the column
/// does not label, and an unlabelled row is one no artifact claims, so a column adopted without
/// its completion serves a masked count short with nothing reporting a fault.
#[test]
fn a_column_the_level_has_moved_past_is_completed_rather_than_recomposed() {
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
        "the fold wrote a column to be behind"
    );

    // The level moves after the fold wrote its column: an artifact published over base rows the
    // column has never labelled.
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
    tick(&engine);
    let grown = sweep(&engine);
    drop(engine);

    let reopened = fx.open();
    assert_eq!(
        sweep(&reopened),
        grown,
        "the restart serves what the live engine served"
    );
    assert!(
        reopened.columns_adopted() > 0,
        "the fold's column is taken although the level has moved past it"
    );
    assert_eq!(
        reopened.columns_composed(),
        0,
        "and completed, so nothing is composed"
    );
    let form = reopened
        .held_artifact_form_for_test("s0", FLAT, 0)
        .expect("the sweep left the level's form held");
    assert!(
        !form.membership().rows_held(),
        "the completed level is served from its column and holds no rows"
    );
}

/// **The differential over a level that holds no bitmaps at all**, which is what a row-major level
/// is after a fold and a restart: the column is the membership, its own bytes give every artifact's
/// extent, and nothing is transposed back — a publication, a growth and a deny each reach the
/// column and the extents rather than a row form.
///
/// So the same corpus is served twice, once with both levels pinned artifact-major and once
/// row-major, and every answer is compared after each of those writes. The row-major side is
/// asserted to be holding no rows at each step, or the comparison is of the artifact-major route
/// against itself.
///
/// **Both column forms**: the flat level takes a label column and the treed one a list column, so
/// the list form's amendment is covered here as well as the label form's.
///
/// Mutations this kills: taking a publication's rows out of a form that holds none (every new
/// artifact would serve a zero count); leaving the extents behind a growth (the artifact's rows
/// past the old extent would read as absent, which is a short `member_of` operand and a short
/// region leaf); and transposing the column back on the first write, which the residency saving
/// exists to avoid.
#[test]
fn a_level_that_holds_no_rows_takes_every_write_through_its_column() {
    let major_fx = fixture();
    let minor_fx = fixture();
    let major = published(
        &major_fx,
        Some(ServingLayout::ArtifactMajor),
        Some(ServingLayout::ArtifactMajor),
    );
    let minor = published(
        &minor_fx,
        Some(ServingLayout::RowMajorLabel),
        Some(ServingLayout::RowMajorList),
    );
    assert_same(&sweep(&major), &sweep(&minor), "at publication");

    // The fold writes each level's column into the new prefix; the restart claims it and builds no
    // artifact-major half.
    fold(&major);
    fold(&minor);
    drop(major);
    drop(minor);
    let major = major_fx.open();
    let minor = minor_fx.open();
    let baseline = sweep(&major);
    assert_same(&baseline, &sweep(&minor), "after a fold and a restart");

    let holds_no_rows = |engine: &Engine, what: &str| {
        for layer in [FLAT, TREED] {
            let form = engine
                .held_artifact_form_for_test("s0", layer, 0)
                .unwrap_or_else(|| panic!("{what}: {layer}'s form is held"));
            assert!(
                !form.membership().rows_held(),
                "{what}: {layer} holds per-artifact rows, so this compares the artifact-major \
                 route against itself"
            );
            assert!(
                form.column().is_some(),
                "{what}: {layer} is served from its column"
            );
        }
    };
    holds_no_rows(&minor, "after a restart");

    // **A region leaf by artifact**, which is one of the two readers that still takes one
    // artifact's rows out of the column. Its operand is `membership ∩ M_auth`, so a short one shows
    // up as a lower `matched` than the artifact-major twin's.
    let region_matched = |engine: &Engine| -> Vec<(Option<String>, u64)> {
        let session = engine.authorise(&full_coverage_credential()).unwrap();
        let served = engine
            .viewport(
                &session,
                ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
            )
            .expect("a viewport")
            .artifacts;
        served
            .iter()
            .map(|artifact| {
                let out = engine
                    .viewport(
                        &session,
                        ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize).filter(
                            tessera_engine::filter::FilterExpr::Region(
                                tessera_engine::filter::RegionLeaf::Artifact(artifact.tessera_id),
                            ),
                        ),
                    )
                    .expect("a region leaf by artifact");
                let matched: u64 = out.tiles.iter().map(|t| t.matched).sum();
                (artifact.key.clone(), matched)
            })
            .collect()
    };
    let mut by_key = region_matched(&major);
    let mut minor_by_key = region_matched(&minor);
    by_key.sort();
    minor_by_key.sort();
    assert!(
        by_key.iter().any(|(_, matched)| *matched > 0),
        "the region leaf matched nothing anywhere, so it asserts nothing"
    );
    assert_eq!(
        by_key, minor_by_key,
        "a region leaf by artifact read a different membership out of the column"
    );

    // **A publication into a level that holds no rows**, on the overlapping level: its memberships
    // overlap by construction, so a list column takes the new artifact's rows without the double
    // claim that would cost a label column its column. The rows go to the column and the extents to
    // the index; nothing is transposed.
    for (fx, engine) in [(&major_fx, &major), (&minor_fx, &minor)] {
        engine
            .publish_artifacts(
                TREED.into(),
                0,
                vec![IncomingArtifact::from_entities(
                    Some("published-after-fold".into()),
                    fx.members(0..200),
                )],
            )
            .unwrap();
        tick(engine);
    }
    assert_same(&sweep(&major), &sweep(&minor), "after a publication");
    holds_no_rows(&minor, "after a publication");

    // **A growth**, which extends an artifact's rows past the extent the column's own bytes gave
    // it: an artifact whose extent stopped short would have its rows beyond it read as absent.
    for (fx, engine) in [(&major_fx, &major), (&minor_fx, &minor)] {
        engine
            .publish_artifacts(
                TREED.into(),
                0,
                vec![IncomingArtifact::from_entities(
                    Some("published-after-fold".into()),
                    fx.members(0..N_ITEMS),
                )],
            )
            .unwrap();
        tick(engine);
    }
    assert_same(&sweep(&major), &sweep(&minor), "after a growth");
    holds_no_rows(&minor, "after a growth");
    assert_eq!(
        by_key,
        {
            let mut after = region_matched(&minor);
            after.sort();
            after.retain(|(key, _)| by_key.iter().any(|(k, _)| k == key));
            after
        },
        "the grown level's region leaves moved for artifacts the growth did not touch"
    );

    // **And a deny**, which the level takes through the overlay exactly as an artifact-major one
    // does: nothing about the column or the extents moves.
    for (fx, engine) in [(&major_fx, &major), (&minor_fx, &minor)] {
        engine
            .accept_change(fx.member(7), ChangeOp::Suppress)
            .unwrap();
        engine
            .accept_change(fx.member(11), ChangeOp::Delete)
            .unwrap();
    }
    assert_same(&sweep(&major), &sweep(&minor), "after a deny");
    holds_no_rows(&minor, "after a deny");
}

/// **The write a label column cannot express, and what a level holding no rows does instead.** A
/// label column refuses a row that would come to carry two artifacts, and such a level's column
/// *is* its membership — so there is nothing to fall back to. It takes the **list** form instead,
/// composed through the disk-backed partition route from the column it already holds plus the
/// pairs the amendment added: the form the fold would choose for a level that has stopped
/// partitioning (decision 0094), reached without ever materialising the artifact-major bitmaps.
///
/// Mutations this kills: dropping the form and projecting the level on the next request, which at
/// corpus scale is the residency and the request-path cost the layout exists to avoid; and
/// recomposing it without the amendment's own pairs, which would serve the publication's members
/// as belonging to nobody.
#[test]
fn a_publication_that_costs_a_label_column_its_partition_recomposes_it_as_a_list() {
    let major_fx = fixture();
    let minor_fx = fixture();
    let major = published(
        &major_fx,
        Some(ServingLayout::ArtifactMajor),
        Some(ServingLayout::ArtifactMajor),
    );
    let minor = published(&minor_fx, Some(ServingLayout::RowMajorLabel), None);
    fold(&major);
    fold(&minor);
    drop(major);
    drop(minor);
    let major = major_fx.open();
    let minor = minor_fx.open();
    assert_same(&sweep(&major), &sweep(&minor), "after a fold and a restart");
    assert!(
        !minor
            .held_artifact_form_for_test("s0", FLAT, 0)
            .expect("the sweep left the level's form held")
            .membership()
            .rows_held(),
        "the level is served from its column alone"
    );

    // Over the whole corpus, so every row the partitioning already claimed is claimed twice.
    for (fx, engine) in [(&major_fx, &major), (&minor_fx, &minor)] {
        engine
            .publish_artifacts(
                FLAT.into(),
                0,
                vec![labelled(fx, "overlapping", (0..N_ITEMS).collect(), vec![0])],
            )
            .unwrap();
        tick(engine);
    }
    assert_same(&sweep(&major), &sweep(&minor), "after the overlap");
    let form = minor
        .held_artifact_form_for_test("s0", FLAT, 0)
        .expect("the level's form is still held");
    assert!(
        !form.membership().rows_held(),
        "the level took the list form rather than materialising the bitmaps it never held"
    );
    assert!(
        form.column().is_some(),
        "and it is still served from a column"
    );
    assert_eq!(
        form.layout(),
        ServingLayout::RowMajorList,
        "the served layout is the form a level that has stopped partitioning takes"
    );
    // ⊘ The *record* still says `column`: it is the registry's, and a fold is what moves it
    // (decision 0094). What moved here is the form the level is served in.
    assert_eq!(minor.recorded_layout(FLAT, 0), Some(ServingLayout::RowMajorLabel));

    // **And it goes on taking writes**, which is the whole of what the list form buys: it refuses
    // no membership, so there is no second fallback below this one.
    for (fx, engine) in [(&major_fx, &major), (&minor_fx, &minor)] {
        engine
            .publish_artifacts(
                FLAT.into(),
                0,
                vec![labelled(fx, "overlapping-again", (0..N_ITEMS).collect(), vec![0])],
            )
            .unwrap();
        tick(engine);
    }
    assert_same(&sweep(&major), &sweep(&minor), "after a second overlap");
    let form = minor
        .held_artifact_form_for_test("s0", FLAT, 0)
        .expect("the level's form is still held");
    assert!(!form.membership().rows_held());
    assert_eq!(form.layout(), ServingLayout::RowMajorList);
}

/// **A growth in the same tick as the overlap that recomposes the column.**
///
/// A growth on a form that holds no rows offers the artifact's **whole** membership as the pairs
/// the column gains — it has no held set to subtract one from — and the recomposition's feed is the
/// column's own pairs plus those. A feed that concatenated the two would give a list row naming one
/// ordinal twice, which counts that artifact twice in the histogram, twice in the declared size the
/// proportional criterion divides by, and twice in the accumulated centroid. Every one of those is
/// a number a viewer sees, and none of them is an error anywhere.
///
/// So the same tick carries both, and every answer is compared against the twin that holds its
/// bitmaps.
#[test]
fn a_growth_in_the_tick_that_recomposes_the_column_counts_each_row_once() {
    let major_fx = fixture();
    let minor_fx = fixture();
    let major = published(
        &major_fx,
        Some(ServingLayout::ArtifactMajor),
        Some(ServingLayout::ArtifactMajor),
    );
    let minor = published(&minor_fx, Some(ServingLayout::RowMajorLabel), None);
    fold(&major);
    fold(&minor);
    drop(major);
    drop(minor);
    let major = major_fx.open();
    let minor = minor_fx.open();
    assert_same(&sweep(&major), &sweep(&minor), "after a fold and a restart");
    assert!(
        !minor
            .held_artifact_form_for_test("s0", FLAT, 0)
            .expect("the sweep left the level's form held")
            .membership()
            .rows_held(),
        "the level is served from its column alone"
    );

    // **One tick, two writes**: a growth of an artifact the level already holds — whose pairs the
    // column already carries, so every one of them is a repeat — and a publication that makes the
    // memberships overlap, which is what sends the level through the recomposition.
    for (fx, engine) in [(&major_fx, &major), (&minor_fx, &minor)] {
        engine.set_background_refresh_for_test(false);
        engine
            .grow_memberships(
                FLAT.into(),
                0,
                vec![tessera_lifecycle::IncomingGrowth::from_entities(
                    "p0".into(),
                    fx.members(0..500),
                )],
            )
            .expect("the growth is accepted");
        engine
            .publish_artifacts(
                FLAT.into(),
                0,
                vec![labelled(fx, "overlapping", (0..N_ITEMS).collect(), vec![0])],
            )
            .unwrap();
        tick(engine);
    }
    assert_same(
        &sweep(&major),
        &sweep(&minor),
        "after a growth and an overlap in one tick",
    );
    let form = minor
        .held_artifact_form_for_test("s0", FLAT, 0)
        .expect("the level's form is still held");
    assert!(
        !form.membership().rows_held(),
        "the level took the list form rather than materialising the bitmaps it never held"
    );
    assert_eq!(form.layout(), ServingLayout::RowMajorList);
    // **The declared size is the criterion's denominator**, and it is where a repeated pair shows
    // up as a number rather than as a set: it must be what the twin's membership cardinality is.
    let column = form.column().expect("served from a column");
    let twin = major
        .held_artifact_form_for_test("s0", FLAT, 0)
        .expect("the twin's form is held");
    for ordinal in 0..form.len() as u32 {
        assert_eq!(
            column.declared_size(ordinal),
            twin.get(ordinal).map(croaring::Bitmap::cardinality).unwrap_or(0),
            "ordinal {ordinal}: the recomposed column counts a row twice"
        );
    }
}

/// Every artifact of the flat layer one session is served over the whole map: its count and its
/// derived geometry, by key.
fn flat_served(engine: &Engine, session: &tessera_engine::Session) -> BTreeMap<Option<String>, Served> {
    let response = engine
        .viewport(
            session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
        )
        .expect("a viewport");
    served(&response.artifacts)
        .into_iter()
        .filter(|a| a.layer == FLAT)
        .map(|a| (a.key.clone(), a))
        .collect()
}

/// **Sessions holding one term set share one build of a level's counts, and a suppression still
/// reaches the next request of each.** The level is folded first, so it is served from its column
/// alone and its centroids and boxes are in the shared entry too.
#[test]
fn sessions_with_one_term_set_share_the_counts_and_a_suppression_reaches_the_next_request() {
    let fx = fixture();
    let engine = published(
        &fx,
        Some(ServingLayout::RowMajorLabel),
        Some(ServingLayout::ArtifactMajor),
    );
    fold(&engine);
    let builds = || engine.figures_stats().misses;

    let first = engine.authorise(&full_coverage_credential()).unwrap();
    let seen = flat_served(&engine, &first);
    assert!(seen.values().any(|a| a.centroid.is_some() && a.bbox.is_some()));
    assert!(
        !engine
            .held_artifact_form_for_test("s0", FLAT, 0)
            .expect("the level's form is held")
            .membership()
            .rows_held(),
        "the level is served from its column, so its geometry comes from the shared entry"
    );
    let after_first = builds();
    assert!(after_first > 0);

    let second = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(flat_served(&engine, &second), seen);
    assert_eq!(
        builds(),
        after_first,
        "a second session with the same terms reads the first session's counts"
    );

    let narrower = engine.authorise(&subset_credential()).unwrap();
    assert_ne!(flat_served(&engine, &narrower), seen);
    assert!(
        builds() > after_first,
        "a session with other terms builds its own counts"
    );

    // Source id 400 is in block `p0`, visible to the full principal. Its suppression is accepted
    // between the first session's request and the second's.
    engine
        .accept_change(fx.member(400), ChangeOp::Suppress)
        .unwrap();
    let before_suppression = builds();
    let corrected = flat_served(&engine, &second);
    let p0 = Some("p0".to_string());
    assert_eq!(
        corrected[&p0].masked_count,
        seen[&p0].masked_count - 1,
        "the request after the suppression is served the corrected count"
    );
    assert_ne!(corrected[&p0].centroid, None);
    assert_eq!(flat_served(&engine, &first), corrected);
    assert_eq!(
        builds(),
        before_suppression,
        "the suppression is subtracted from the shared counts, which are not built again"
    );
}

/// **Requests arriving while a level's counts are building wait for that build** rather than
/// walking the mask a second time.
#[test]
fn concurrent_requests_with_one_term_set_wait_for_one_build() {
    let fx = fixture();
    let engine = std::sync::Arc::new(published(
        &fx,
        Some(ServingLayout::RowMajorLabel),
        Some(ServingLayout::ArtifactMajor),
    ));
    let wait = std::time::Duration::from_secs(60);
    let ask = |engine: std::sync::Arc<Engine>| {
        std::thread::spawn(move || {
            let session = engine.authorise(&full_coverage_credential()).unwrap();
            flat_served(&engine, &session)
        })
    };

    engine.hold_next_masked_count_build_for_test();
    let first = ask(std::sync::Arc::clone(&engine));
    wait_until("the held build never started", wait, || {
        engine.figures_stats().misses > 0
    });
    let second = ask(std::sync::Arc::clone(&engine));
    wait_until("the second request never waited for the build", wait, || {
        engine.figures_stats().waiters > 0
    });
    engine.release_masked_count_build_for_test();
    let (first, second) = (first.join().unwrap(), second.join().unwrap());
    assert!(!first.is_empty());
    assert_eq!(first, second);
    let stats = engine.figures_stats();
    assert_eq!(
        stats.misses as usize, stats.entries,
        "every key the two requests read was built once"
    );
}

/// **A points request does not wait for a count build.** With one compute thread, a build on the
/// compute pool would hold the thread every points request needs. The build is held on its own
/// pool and the points request completes while it is held.
#[test]
fn a_points_request_does_not_wait_for_a_count_build() {
    let fx = fixture();
    let engine = std::sync::Arc::new(fx.open_with(tessera_engine::EngineConfig {
        compute_threads: 1,
        ..config()
    }));
    publish_into(
        &fx,
        &engine,
        Some(ServingLayout::RowMajorLabel),
        Some(ServingLayout::ArtifactMajor),
    );
    // Every points request takes the pool rather than the calling thread.
    engine.set_serial_fallback_max_rows_for_test(0);
    let session = std::sync::Arc::new(engine.authorise(&full_coverage_credential()).unwrap());
    let points = |engine: &Engine, session: &tessera_engine::Session| {
        engine
            .viewport(
                session,
                ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize)
                    .layers(tessera_engine::viewport::LayerSelection::Named(&[])),
            )
            .expect("a points viewport")
            .points
            .len()
    };
    // The session's projection is built before the hold, so the points request below needs
    // nothing but the compute pool.
    assert!(points(&engine, &session) > 0);

    let wait = std::time::Duration::from_secs(60);
    engine.hold_next_masked_count_build_for_test();
    let clusters = {
        let (engine, session) = (std::sync::Arc::clone(&engine), std::sync::Arc::clone(&session));
        std::thread::spawn(move || flat_served(&engine, &session))
    };
    wait_until("the held build never started", wait, || {
        engine.figures_stats().misses > 0
    });
    let (done, finished) = std::sync::mpsc::channel();
    let pointer = {
        let (engine, session) = (std::sync::Arc::clone(&engine), std::sync::Arc::clone(&session));
        std::thread::spawn(move || {
            let _ = done.send(points(&engine, &session));
        })
    };
    let served = finished.recv_timeout(wait);
    engine.release_masked_count_build_for_test();
    assert!(!clusters.join().unwrap().is_empty());
    pointer.join().unwrap();
    assert!(
        served.is_ok_and(|n| n > 0),
        "the points request waited for the count build"
    );
}

/// **A suppression accepted while a build is in flight is subtracted from it for every request that
/// starts after the acknowledgement.** The build counts the grant's rows whatever the overlay
/// holds; a request that starts after the suppression waits for it and subtracts the suppressed
/// row, read at its own start.
#[test]
fn a_build_in_flight_at_a_suppression_is_corrected_for_it() {
    let fx = fixture();
    let engine = std::sync::Arc::new(published(
        &fx,
        Some(ServingLayout::RowMajorLabel),
        Some(ServingLayout::ArtifactMajor),
    ));
    let p0 = Some("p0".to_string());
    let wait = std::time::Duration::from_secs(60);

    engine.hold_next_masked_count_build_for_test();
    let before = {
        let engine = std::sync::Arc::clone(&engine);
        std::thread::spawn(move || {
            let session = engine.authorise(&full_coverage_credential()).unwrap();
            flat_served(&engine, &session)
        })
    };
    wait_until("the held build never started", wait, || {
        engine.figures_stats().misses > 0
    });

    // Source id 400 is a visible member of `p0`.
    engine
        .accept_change(fx.member(400), ChangeOp::Suppress)
        .unwrap();
    let corrected = {
        let engine = std::sync::Arc::clone(&engine);
        std::thread::spawn(move || {
            let after = engine.authorise(&full_coverage_credential()).unwrap();
            flat_served(&engine, &after)
        })
    };
    wait_until("the request after the suppression never waited for the build", wait, || {
        engine.figures_stats().waiters > 0
    });

    engine.release_masked_count_build_for_test();
    let held = before.join().unwrap();
    let corrected = corrected.join().unwrap();
    assert_eq!(
        corrected[&p0].masked_count,
        held[&p0].masked_count - 1,
        "the request after the suppression is served a count without the suppressed member"
    );
    let later = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(flat_served(&engine, &later), corrected);
}

/// A counts-only request for every layer's artifacts: its masked counts are read after its
/// drawing span, so its builds are the ones that give way.
fn artifacts_only(
    engine: &Engine,
    session: &tessera_engine::Session,
    cancel: Option<tessera_engine::CancelToken>,
) -> Result<usize, tessera_engine::EngineError> {
    engine
        .viewport(
            session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, 0).cancel(cancel),
        )
        .map(|response| response.artifacts.len())
}

/// A points request with no layers, held in its drawing span by the test hook, on its own thread.
fn held_drawing(
    engine: &std::sync::Arc<Engine>,
    session: &std::sync::Arc<tessera_engine::Session>,
) -> std::thread::JoinHandle<usize> {
    engine.hold_next_drawing_for_test();
    let drawer = {
        let (engine, session) = (std::sync::Arc::clone(engine), std::sync::Arc::clone(session));
        std::thread::spawn(move || {
            engine
                .viewport(
                    &session,
                    ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize)
                        .layers(tessera_engine::viewport::LayerSelection::Named(&[])),
                )
                .expect("a points viewport")
                .points
                .len()
        })
    };
    wait_until(
        "the points request never started drawing",
        std::time::Duration::from_secs(60),
        || engine.drawing_is_held_for_test(),
    );
    drawer
}

/// **A build stops giving way once it has waited its budget**, so a steady run of points
/// requests delays a level's counts by about the budget and no more.
#[test]
fn a_build_under_continuous_drawing_finishes_after_its_budget() {
    let fx = fixture();
    let engine = std::sync::Arc::new(published(
        &fx,
        Some(ServingLayout::RowMajorLabel),
        Some(ServingLayout::ArtifactMajor),
    ));
    engine.set_masked_count_give_way_ms(200);
    let session = std::sync::Arc::new(engine.authorise(&full_coverage_credential()).unwrap());
    let drawer = held_drawing(&engine, &session);

    let (done, finished) = std::sync::mpsc::channel();
    let builder = {
        let (engine, session) = (std::sync::Arc::clone(&engine), std::sync::Arc::clone(&session));
        std::thread::spawn(move || {
            let _ = done.send(artifacts_only(&engine, &session, None));
        })
    };
    let served = finished.recv_timeout(std::time::Duration::from_secs(60));
    let still_drawing = engine.drawing_is_held_for_test();
    engine.release_drawing_for_test();
    builder.join().unwrap();
    assert!(drawer.join().unwrap() > 0);
    assert!(
        matches!(served, Ok(Ok(n)) if n > 0),
        "the build never stopped giving way: {served:?}"
    );
    assert!(still_drawing, "the build finished while the points request was drawing");
}

/// **A build every caller has left stops, and holds nothing**, even while it is giving way.
#[test]
fn a_build_whose_callers_have_gone_stops() {
    let fx = fixture();
    let engine = std::sync::Arc::new(published(
        &fx,
        Some(ServingLayout::RowMajorLabel),
        Some(ServingLayout::ArtifactMajor),
    ));
    engine.set_masked_count_give_way_ms(600_000);
    let session = std::sync::Arc::new(engine.authorise(&full_coverage_credential()).unwrap());
    let drawer = held_drawing(&engine, &session);

    let gone = tessera_engine::CancelToken::new();
    let (done, finished) = std::sync::mpsc::channel();
    let builder = {
        let (engine, session, gone) = (
            std::sync::Arc::clone(&engine),
            std::sync::Arc::clone(&session),
            gone.clone(),
        );
        std::thread::spawn(move || {
            let _ = done.send(artifacts_only(&engine, &session, Some(gone)));
        })
    };
    wait_until("the build never started", std::time::Duration::from_secs(60), || {
        engine.figures_stats().misses > 0
    });
    gone.cancel();
    let answered = finished.recv_timeout(std::time::Duration::from_secs(60));
    let held = engine.figures_stats().entries;
    engine.release_drawing_for_test();
    builder.join().unwrap();
    assert!(drawer.join().unwrap() > 0);
    assert!(
        matches!(answered, Ok(Err(tessera_engine::EngineError::Cancelled))),
        "the abandoned build went on giving way: {answered:?}"
    );
    assert_eq!(held, 0, "a stopped build holds nothing");
}

/// A sink whose counts frame blocks until the test lets it go, as a client that stops reading.
struct StalledSink(std::sync::mpsc::Receiver<()>);

impl tessera_engine::ViewportSink for StalledSink {
    fn head(&mut self, _: tessera_engine::ViewportHead) -> tessera_engine::SinkResult {
        Ok(())
    }

    fn counts(
        &mut self,
        _: &[tessera_engine::TileCount],
        _: Option<&[tessera_engine::SubCellCount]>,
    ) -> tessera_engine::SinkResult {
        let _ = self.0.recv();
        Ok(())
    }

    fn artifacts(&mut self, _: &[ArtifactOut]) -> tessera_engine::SinkResult {
        Ok(())
    }

    fn points(&mut self, _: tessera_engine::PointColumns) -> tessera_engine::SinkResult {
        Ok(())
    }
}

/// **A points request blocked on its client holds no build.** The stalled request is between its
/// sweep and its last point, but sending, so a build need not wait for the stream to be shed.
#[test]
fn a_stalled_stream_holds_no_build() {
    let fx = fixture();
    let engine = std::sync::Arc::new(published(
        &fx,
        Some(ServingLayout::RowMajorLabel),
        Some(ServingLayout::ArtifactMajor),
    ));
    engine.set_masked_count_give_way_ms(600_000);
    let session = std::sync::Arc::new(engine.authorise(&full_coverage_credential()).unwrap());

    let (unstall, stalled) = std::sync::mpsc::channel::<()>();
    let reader = {
        let (engine, session) = (std::sync::Arc::clone(&engine), std::sync::Arc::clone(&session));
        std::thread::spawn(move || {
            engine
                .viewport_stream(
                    &session,
                    ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize)
                        .layers(tessera_engine::viewport::LayerSelection::Named(&[])),
                    1 << 20,
                    &mut StalledSink(stalled),
                )
                .is_ok()
        })
    };

    let (done, finished) = std::sync::mpsc::channel();
    let builder = {
        let (engine, session) = (std::sync::Arc::clone(&engine), std::sync::Arc::clone(&session));
        std::thread::spawn(move || {
            let _ = done.send(artifacts_only(&engine, &session, None));
        })
    };
    let served = finished.recv_timeout(std::time::Duration::from_secs(60));
    unstall.send(()).unwrap();
    builder.join().unwrap();
    assert!(reader.join().unwrap());
    assert!(
        matches!(served, Ok(Ok(n)) if n > 0),
        "the build waited on a stream blocked on its client: {served:?}"
    );
}
