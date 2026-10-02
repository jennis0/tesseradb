//! **A layer with no lineage tags its points from its labels, and the tag is the walk's.** Every
//! case asks one request twice, once with the labels answering and once with every layer resolved
//! against the walk's served set as before, and compares the membership columns point for point.
//! The layouts, the principals, the zooms on and off the band route, a growth that leaves a level
//! without a current label column, an ingest whose rows lie above the base, a fold that writes
//! the columns again, and a suppression accepted between two requests each have a case.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use tessera_engine::{
    ArtifactOut, Engine, LayerSelection, PointColumns, SinkResult, SubCellCount,
    TileCount, ViewportHead, ViewportOut, ViewportRequest, ViewportSink,
};
use tessera_lifecycle::membership::IncomingAttachment;
use tessera_lifecycle::wal::{ChangeOp, WalScalar};
use tessera_lifecycle::{IncomingArtifact, IncomingGrowth, UnallocatedRow};
use tessera_store::manifest::DerivedForm;
use tessera_types::layer::{
    ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind, LayerDeclaration,
    LevelDeclaration, MembershipSource, ServingLayout,
};
use tessera_types::EntityId;

const ROWS: u64 = 3_000;

const TIERED: &str = "clusters/tiered";
const FLAT: &str = "clusters/flat";
const OVERLAP: &str = "clusters/overlap";
const TREE: &str = "clusters/tree";
const NOTES: &str = "clusters/notes";

fn declaration(name: &str, kind: HierarchyKind, layout: Option<ServingLayout>) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: None,
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind,
            prune_children: false,
        },
        content: ContentDeclaration::default(),
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout,
        shape: None,
    }
}

/// A tiered layer of two levels whose finer level needs fifty visible members, which the subset
/// principal's third of them does not reach.
fn tiered() -> LayerDeclaration {
    let mut d = declaration(TIERED, HierarchyKind::Tiered, None);
    d.require_member_visibility = Some(ExistenceCriterion::Count(50));
    d.levels = (0..2)
        .map(|level| LevelDeclaration {
            level,
            title: None,
            zoom: None,
        })
        .collect();
    d
}

fn notes() -> LayerDeclaration {
    let mut d = declaration(NOTES, HierarchyKind::Flat, None);
    d.depends_on = vec![FLAT.into()];
    d
}

/// One request's membership column for `layer`, with an absent column read as all null.
fn column(out: &ViewportOut, layer: &str) -> Vec<Option<u64>> {
    out.points
        .membership
        .iter()
        .find(|c| c.layer == layer)
        .map(|c| c.ids.clone())
        .unwrap_or_else(|| vec![None; out.points.len()])
}

/// Every non-null tag names an artifact of its layer in the same response's frame.
fn assert_joined(out: &ViewportOut) {
    let frame: BTreeSet<(&str, u64)> = out
        .artifacts
        .iter()
        .map(|a| (a.layer.as_str(), a.tessera_id.raw()))
        .collect();
    for c in &out.points.membership {
        for id in c.ids.iter().flatten() {
            assert!(
                frame.contains(&(c.layer.as_str(), *id)),
                "{} names {id}, which the frame does not carry",
                c.layer
            );
        }
    }
}

/// What one sweep compared: tags read from labels, tiles answered from the bands.
#[derive(Default, Debug)]
struct Compared {
    tagged: BTreeMap<String, usize>,
    banded: u64,
}

/// Ask every case twice, from the labels and from the walk, and require the same answer.
fn assert_tags_match(
    engine: &Engine,
    stage: &str,
    credentials: &[Vec<u8>],
    selections: &[&[&str]],
    cases: &[(u8, [f64; 4], usize)],
) -> Compared {
    let mut compared = Compared::default();
    for credential in credentials {
        let session = engine.authorise(credential).unwrap();
        for layers in selections {
            for &(zoom, bbox, k) in cases {
                let request =
                    ViewportRequest::new("s0", zoom, bbox, k).layers(LayerSelection::Named(layers));
                engine.set_tags_from_labels_for_test(false);
                let walked = engine.viewport(&session, request.clone()).unwrap();
                engine.set_tags_from_labels_for_test(true);
                let labelled = engine.viewport(&session, request).unwrap();
                let at = format!("{stage}: layers {layers:?} at zoom {zoom} over {bbox:?}");
                assert_eq!(
                    labelled.points.tessera_ids, walked.points.tessera_ids,
                    "{at}: the points"
                );
                let frame = |out: &ViewportOut| -> BTreeSet<(String, u64)> {
                    out.artifacts
                        .iter()
                        .map(|a| (a.layer.clone(), a.tessera_id.raw()))
                        .collect()
                };
                assert_eq!(frame(&labelled), frame(&walked), "{at}: the frame");
                for layer in layers.iter() {
                    let tags = column(&labelled, layer);
                    assert_eq!(tags, column(&walked, layer), "{at}: {layer}");
                    *compared.tagged.entry(layer.to_string()).or_default() +=
                        tags.iter().flatten().count();
                }
                assert_joined(&labelled);
                compared.banded += labelled.timings.tiles_from_bands;
            }
        }
    }
    compared
}

fn cases() -> Vec<(u8, [f64; 4], usize)> {
    vec![
        // Small budgets, which the bands answer.
        (0, WHOLE_MAP, 8),
        (1, WHOLE_MAP, 4),
        (2, [0.0, 0.0, 500.0, 500.0], 3),
        // Every visible point, from the scan.
        (0, WHOLE_MAP, 200),
        // Past the band route's zoom.
        (11, [0.0, 0.0, 120.0, 120.0], 50),
    ]
}

/// An engine whose display threshold is a cut, so the band route answers the small budgets.
fn open(root: &std::path::Path, tmp: &std::path::Path) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        tessera_engine::EngineConfig {
            theta_target_marks: 16,
            ..config()
        },
    )
    .unwrap();
    engine.start_write_executor(8).unwrap();
    engine.set_background_refresh_for_test(false);
    engine
}

fn has_labels(engine: &Engine, layer: &str, level: u32) -> bool {
    engine
        .generation()
        .bundle
        .partitions
        .values()
        .flat_map(|p| p.manifest.derived_extents.iter())
        .any(|e| {
            e.layer == layer && e.level == level && matches!(e.form, DerivedForm::BandLabels { .. })
        })
}

#[test]
fn labels_tag_points_as_the_walk_does_through_growth_ingest_fold_and_suppression() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        ROWS,
    );
    let map = source_to_new_map(&root, "v00000");
    let entity = |source: u64| EntityId::new(map[&source]);
    let engine = open(&root, tmp.path());

    engine.register_layer(tiered()).unwrap();
    engine
        .register_layer(declaration(
            FLAT,
            HierarchyKind::Flat,
            Some(ServingLayout::RowMajorLabel),
        ))
        .unwrap();
    engine
        .register_layer(declaration(OVERLAP, HierarchyKind::Flat, None))
        .unwrap();
    engine
        .register_layer(declaration(TREE, HierarchyKind::Nested, None))
        .unwrap();
    engine.register_layer(notes()).unwrap();

    let keyed = |prefix: &str, sets: Vec<Vec<u64>>| -> Vec<IncomingArtifact> {
        sets.into_iter()
            .enumerate()
            .map(|(i, sources)| {
                IncomingArtifact::from_entities(
                    Some(format!("{prefix}{i}")),
                    sources.into_iter().map(entity),
                )
            })
            .collect()
    };
    // Level 0 partitions every item four ways; level 1 twenty ways over the first 2,400, so a
    // point past them is tagged at level 0.
    engine
        .publish_artifacts(
            TIERED.into(),
            0,
            keyed("c", (0..4).map(|a| (0..ROWS).filter(|s| s % 4 == a).collect()).collect()),
        )
        .unwrap();
    engine
        .publish_artifacts(
            TIERED.into(),
            1,
            keyed("r", (0..20).map(|a| (0..2_400).filter(|s| s % 20 == a).collect()).collect()),
        )
        .unwrap();
    engine
        .publish_artifacts(
            FLAT.into(),
            0,
            keyed("f", (0..10).map(|a| (0..2_000).filter(|s| s % 10 == a).collect()).collect()),
        )
        .unwrap();
    engine
        .publish_artifacts(
            OVERLAP.into(),
            0,
            keyed("o", vec![(0..1_800).collect(), (1_200..ROWS).collect()]),
        )
        .unwrap();
    let mut tree = keyed("t", vec![(0..ROWS).collect(), (0..1_500).collect(), (1_500..ROWS).collect()]);
    tree[1].parent_keys = vec!["t0".into()];
    tree[2].parent_keys = vec!["t0".into()];
    engine.publish_artifacts(TREE.into(), 0, tree).unwrap();
    // Notes on two of the flat layer's artifacts, each over its own members.
    let note = |i: u64| {
        IncomingArtifact::attached(
            Some(format!("n{i}")),
            (0..2_000).filter(|s| s % 10 == i).map(entity),
            Vec::new(),
            IncomingAttachment {
                layer: FLAT.into(),
                level: 0,
                key: format!("f{i}"),
            },
        )
    };
    engine
        .publish_artifacts(NOTES.into(), 0, vec![note(0), note(1)])
        .unwrap();
    tick(&engine);

    let credentials = [full_coverage_credential(), subset_credential()];
    let selections: [&[&str]; 3] = [&[TIERED, FLAT, OVERLAP, TREE], &[NOTES], &[FLAT, NOTES]];
    let cases = cases();

    // Published at a running service: every form is in memory and no level has a label file.
    let published = assert_tags_match(&engine, "published", &credentials, &selections, &cases);
    for layer in [TIERED, FLAT, OVERLAP, TREE, NOTES] {
        assert!(published.tagged[layer] > 0, "{layer} tagged no point: {published:?}");
    }

    // The fold writes each partitioning level's label column and its band copy.
    fold(&engine);
    assert!(has_labels(&engine, TIERED, 1) && has_labels(&engine, FLAT, 0));
    assert!(!has_labels(&engine, OVERLAP, 0), "an overlapping level has no label column");
    let folded = assert_tags_match(&engine, "folded", &credentials, &selections, &cases);
    assert!(folded.banded > 0, "no tile was answered from the bands: {folded:?}");
    assert_eq!(folded.tagged, published.tagged, "the fold moved no tag");

    // A growth moves both levels it reaches, so neither has a current label column, and an
    // ingest puts rows above the base that a growth then gives to artifacts.
    let fresh: Vec<UnallocatedRow> = (0..60)
        .map(|i| {
            let descriptors = vec![b"0".to_vec()];
            UnallocatedRow {
                view: "s0".to_string(),
                join: None,
                terms: engine.resolve_terms(&descriptors),
                descriptors,
                x: (i * 13 % 1000) as f64,
                y: (i * 29 % 1000) as f64,
                scalars: Vec::<WalScalar>::new(),
                scoped: Vec::new(),
            }
        })
        .collect();
    let ingested = engine
        .ingest_rows(fresh, "tag-1".to_string(), [3u8; 32])
        .unwrap();
    publish_buffered(&engine);
    let grow = |layer: &str, level: u32, key: &str, joining: Vec<EntityId>| {
        engine
            .grow_memberships(
                layer.into(),
                level,
                vec![IncomingGrowth::from_entities(key.to_string(), joining)],
            )
            .unwrap();
    };
    grow(TIERED, 1, "r0", (2_400..2_500).map(entity).chain(ingested[..30].iter().copied()).collect());
    grow(FLAT, 0, "f3", (2_000..2_100).map(entity).chain(ingested[30..].iter().copied()).collect());
    // A flush publishes the manifest that no longer names the grown level's files.
    let mut one = generator_row(&engine, 0, 500.0, 500.0);
    one.scalars.clear();
    engine.ingest_rows(vec![one], "tag-2".to_string(), [4u8; 32]).unwrap();
    publish_buffered(&engine);
    assert!(!has_labels(&engine, TIERED, 1), "a grown level's label column is not current");
    assert!(has_labels(&engine, TIERED, 0), "and the level it did not reach keeps its own");
    let grown = assert_tags_match(&engine, "grown", &credentials, &selections, &cases);
    assert!(grown.tagged[FLAT] >= published.tagged[FLAT]);

    // A suppression accepted between two requests applies to the second.
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let request = ViewportRequest::new("s0", 0, WHOLE_MAP, 200)
        .layers(LayerSelection::Named(&[TIERED, FLAT]));
    let before = engine.viewport(&session, request.clone()).unwrap();
    let named = |out: &ViewportOut, layer: &str, key: &str| -> Option<u64> {
        out.artifacts
            .iter()
            .find(|a| a.layer == layer && a.key.as_deref() == Some(key))
            .map(|a| a.tessera_id.raw())
    };
    let r1 = named(&before, TIERED, "r1").expect("r1 is served");
    let f2 = named(&before, FLAT, "f2").expect("f2 is served");
    assert!(column(&before, TIERED).contains(&Some(r1)));
    assert!(column(&before, FLAT).contains(&Some(f2)));
    for id in [r1, f2] {
        engine
            .accept_change(
                artifact_entity(&engine, tessera_types::TesseraId::new(id)),
                ChangeOp::Suppress,
            )
            .unwrap();
    }
    let after = engine.viewport(&session, request).unwrap();
    assert!(!column(&after, TIERED).contains(&Some(r1)), "a suppressed artifact tags nothing");
    assert!(!column(&after, FLAT).contains(&Some(f2)));
    // A point r1 held falls to its level-0 artifact.
    let held_by_r1: Vec<usize> = column(&before, TIERED)
        .iter()
        .enumerate()
        .filter(|(_, id)| **id == Some(r1))
        .map(|(i, _)| i)
        .collect();
    let level0 = column(&after, TIERED);
    assert!(held_by_r1.iter().all(|&i| level0[i].is_some()));
    assert_tags_match(&engine, "suppressed", &credentials, &selections, &cases);

    // The next fold writes the grown levels' label columns again.
    fold(&engine);
    assert!(has_labels(&engine, TIERED, 1));
    assert_tags_match(&engine, "refolded", &credentials, &selections, &cases);
}

// ---------------------------------------------------------------------------------------------
// The generator's own layers: a partition by list and by rule, a spatial boundary, an overlapping
// flat layer and a tree, built rather than published.
// ---------------------------------------------------------------------------------------------

/// The generator's schema has seven columns, `partition` the last.
fn generator_row(engine: &Engine, partition: u32, x: f64, y: f64) -> UnallocatedRow {
    let descriptors = vec![b"0".to_vec()];
    let mut scalars = vec![WalScalar::Null; 7];
    scalars[6] = WalScalar::U32(partition);
    UnallocatedRow {
        view: "s0".to_string(),
        join: None,
        terms: engine.resolve_terms(&descriptors),
        descriptors,
        x,
        y,
        scalars,
        scoped: Vec::new(),
    }
}

#[test]
fn a_built_bundle_tags_alike_before_and_after_an_ingest() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let corpus = tessera_corpus::Corpus::new(0x7A6, ROWS, extent()).unwrap();
    let (points, pairs) = (tmp.path().join("points.parquet"), tmp.path().join("pairs.parquet"));
    corpus.write_points_parquet(&points).unwrap();
    corpus.write_pairs_parquet(&pairs).unwrap();
    build_corpus_fixture_with_layers(&root, &points, &pairs, &corpus);
    let engine = open(&root, tmp.path());

    let layers: Vec<String> = engine
        .generation()
        .bundle
        .partitions
        .values()
        .flat_map(|p| p.manifest.derived_extents.iter().map(|e| e.layer.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let names: Vec<&str> = layers.iter().map(String::as_str).collect();
    let selections: [&[&str]; 1] = [&names];
    let credentials: Vec<Vec<u8>> = ["0,1,2,3", "1,2", "5"]
        .iter()
        .map(|grant| grant_credential(grant))
        .collect();
    let cases = cases();
    let built = assert_tags_match(&engine, "built", &credentials, &selections, &cases);

    assert!(
        built.tagged.values().filter(|&&n| n > 0).count() >= 3,
        "too few layers tagged anything to compare: {built:?}"
    );

    let rows: Vec<UnallocatedRow> = (0..80)
        .map(|i| generator_row(&engine, i % 5, (i * 11 % 1000) as f64, (i * 17 % 1000) as f64))
        .collect();
    engine.ingest_rows(rows, "tag-built".to_string(), [5u8; 32]).unwrap();
    publish_buffered(&engine);
    assert_tags_match(&engine, "ingested", &credentials, &selections, &cases);
}

/// A spatial layer's flushed rows join its shapes with no write to the layer, so its built label
/// column stays current while rows above the base belong to its artifacts.
#[test]
fn a_spatial_level_tags_rows_above_its_base_as_the_walk_does() {
    const BOXES: &str = "regions/boxes";
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let corpus = tessera_corpus::Corpus::new(0x5EED, ROWS, extent()).unwrap();
    let (points, pairs) = (tmp.path().join("points.parquet"), tmp.path().join("pairs.parquet"));
    corpus.write_points_parquet(&points).unwrap();
    corpus.write_pairs_parquet(&pairs).unwrap();
    let config_path = tmp.path().join("spatial.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[sources]
points = "points.parquet"
pairs  = "pairs.parquet"

[[view]]
name             = "s0"
extent           = {{ min = 0.0, max = 1000.0 }}
point_visibility = {{ source = "pairs", default = "public" }}

[[layer]]
name                      = "{BOXES}"
views                     = ["s0"]
membership                = "spatial"
hierarchy                 = {{ kind = "flat" }}
visibility                = "public"
artifact_visibility       = {{ default = "inherited" }}
require_member_visibility = "none"
artifacts = [
  {{ key = "sw", bbox = [100.0, 100.0, 300.0, 300.0] }},
  {{ key = "se", bbox = [600.0, 100.0, 800.0, 300.0] }},
  {{ key = "nw", bbox = [100.0, 600.0, 300.0, 800.0] }},
  {{ key = "ne", bbox = [600.0, 600.0, 800.0, 800.0] }},
]

  [layer.shape]
  kind = "bbox"
"#
        ),
    )
    .unwrap();
    let config = tessera_build::config::Config::parse(&config_path, &Default::default()).unwrap();
    build_with_layers(&root, &points, &pairs, &corpus, config);
    let engine = open(&root, tmp.path());
    assert!(has_labels(&engine, BOXES, 0), "the build wrote the boxes' label column");

    let credentials: Vec<Vec<u8>> = ["0,1,2,3", "1,2"].iter().map(|g| grant_credential(g)).collect();
    let selections: [&[&str]; 1] = [&[BOXES]];
    let inside: Vec<(u8, [f64; 4], usize)> = vec![
        (0, WHOLE_MAP, 8),
        (5, [100.0, 100.0, 300.0, 300.0], 200),
        (11, [150.0, 150.0, 200.0, 200.0], 200),
    ];
    let built = assert_tags_match(&engine, "built", &credentials, &selections, &inside);
    assert!(built.tagged[BOXES] > 0);

    // Points inside the south-west box, visible to every grant above.
    let rows: Vec<UnallocatedRow> = (0..120)
        .map(|i| {
            let descriptors = vec![b"1".to_vec()];
            UnallocatedRow {
                view: "s0".to_string(),
                join: None,
                terms: engine.resolve_terms(&descriptors),
                descriptors,
                x: 150.0 + (i % 12) as f64 * 4.0,
                y: 150.0 + (i / 12) as f64 * 4.0,
                scalars: Vec::new(),
                scoped: Vec::new(),
            }
        })
        .collect();
    let ingested = engine.ingest_rows(rows, "boxes-1".to_string(), [6u8; 32]).unwrap();
    publish_buffered(&engine);
    assert!(has_labels(&engine, BOXES, 0), "the flush left the label column current");
    let after = assert_tags_match(&engine, "ingested", &credentials, &selections, &inside);
    assert!(after.tagged[BOXES] > built.tagged[BOXES], "{built:?} then {after:?}");
    let session = engine.authorise(&credentials[0]).unwrap();
    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 11, [150.0, 150.0, 200.0, 200.0], 200)
                .layers(LayerSelection::Named(&[BOXES])),
        )
        .unwrap();
    let fresh: BTreeSet<u64> = ingested
        .iter()
        .map(|&e| engine.tessera_id_of(e).unwrap().raw())
        .collect();
    let tags = column(&out, BOXES);
    assert!(
        out.points
            .tessera_ids
            .iter()
            .zip(&tags)
            .any(|(id, tag)| fresh.contains(id) && tag.is_some()),
        "an ingested point inside a box is tagged with it"
    );
}

// ---------------------------------------------------------------------------------------------
// The order on the stream
// ---------------------------------------------------------------------------------------------

/// What the producer delivered, in its order.
#[derive(Default)]
struct Recorded(Vec<&'static str>);

impl ViewportSink for Recorded {
    fn head(&mut self, _: ViewportHead) -> SinkResult {
        self.0.push("head");
        Ok(())
    }
    fn counts(&mut self, _: &[TileCount], _: Option<&[SubCellCount]>) -> SinkResult {
        self.0.push("counts");
        Ok(())
    }
    fn artifacts(&mut self, _: &[ArtifactOut]) -> SinkResult {
        self.0.push("artifacts");
        Ok(())
    }
    fn points(&mut self, _: PointColumns) -> SinkResult {
        self.0.push("points");
        Ok(())
    }
}

#[test]
fn the_points_reach_the_sink_before_the_artifacts() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        ROWS,
    );
    let map = source_to_new_map(&root, "v00000");
    let engine = open_engine_publishing(&root, &tmp.path().join("cache"), &tmp.path().join("wal"));
    engine
        .register_layer(declaration(FLAT, HierarchyKind::Flat, None))
        .unwrap();
    engine
        .register_layer(declaration(TREE, HierarchyKind::Nested, None))
        .unwrap();
    for layer in [FLAT, TREE] {
        engine
            .publish_artifacts(
                layer.into(),
                0,
                vec![IncomingArtifact::from_entities(
                    Some("a".into()),
                    (0..ROWS).map(|s| EntityId::new(map[&s])),
                )],
            )
            .unwrap();
    }
    tick(&engine);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    for layers in [&[FLAT][..], &[TREE][..], &[FLAT, TREE][..]] {
        let mut sink = Recorded::default();
        // A small chunk threshold, so the points arrive in several chunks.
        engine
            .viewport_stream(
                &session,
                ViewportRequest::new("s0", 2, WHOLE_MAP, 50).layers(LayerSelection::Named(layers)),
                1 << 10,
                &mut sink,
            )
            .unwrap();
        let points = sink.0.iter().filter(|&&s| s == "points").count();
        assert!(points > 1, "{layers:?}: {:?}", sink.0);
        assert_eq!(&sink.0[..2], ["head", "counts"]);
        assert_eq!(
            sink.0.last(),
            Some(&"artifacts"),
            "{layers:?}: the artifacts come after every point: {:?}",
            sink.0
        );
    }
}
