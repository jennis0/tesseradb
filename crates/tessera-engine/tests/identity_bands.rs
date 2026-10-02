//! What a bundle stores for the identity bands: every segment's `bands.bin` and `cell-codes.u32`,
//! and each partitioning level's label column with its copy in band order
//! (`tessera_store::bands`), after a build, a flush, a deletion, a growth, a fold and a restart.
//!
//! Each check reads the files the served generation names and compares them with the columns they
//! were taken from, so a producer that skipped them or wrote them from other rows fails here.

mod common;

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, Int32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_build::{build, BuildArgs};
use tessera_engine::Engine;
use tessera_lifecycle::wal::{ChangeOp, WalScalar};
use tessera_lifecycle::{IncomingArtifact, IncomingGrowth, UnallocatedRow};
use tessera_store::bands::{band_of, BandLabels, FIRST_BAND};
use tessera_store::manifest::{DerivedExtent, DerivedForm};
use tessera_store::membership::{LabelColumnPack, ROW_COLUMN_HOLE};
use tessera_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource, ServingLayout,
};
use tessera_types::EntityId;

/// Enough rows for band 6, which holds about one row in 64, to hold a few dozen.
const ROWS: u64 = 3_000;

/// A bundle of [`ROWS`] items with a rendered `score`, so every band file carries a copy.
fn build_scored(root: &Path) {
    let dir = root.parent().unwrap();
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("score", DataType::Int32, false),
    ]));
    let ids: Vec<u64> = (0..ROWS).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| ((e * 37) % 1000) as f64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| ((e * 53) % 1000) as f64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int32Array::from(
                ids.iter()
                    .map(|e| (*e as i32) * 3 - 500)
                    .collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(&points).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
    write_pairs_n(&pairs, ROWS);

    let mut schema = id_schema();
    schema.attributes.push(tessera_build::config::Attribute {
        field: None,
        name: "score".to_string(),
        title: None,
        ty: tessera_spatial::tiler::ScalarType::I32,
        analyser: None,
        vocabulary: None,
        value_set: None,
        index: false,
        render: true,
        unique: false,
    });
    let args = BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(points, &schema),
        out: root.to_path_buf(),
        limit: None,
        strict: false,
        identity_key: test_key(),
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    };
    build(&args).expect("the scored fixture builds");
}

/// Check every segment the served generation holds: its bands are its rows with their render
/// copies, and its cell codes are its cells. Returns each segment's id and band identities.
fn check_segments(engine: &Engine) -> BTreeMap<String, Vec<u64>> {
    let generation = engine.generation();
    let mut seen = BTreeMap::new();
    for partition in generation.bundle.partitions.values() {
        for data in partition.views.values() {
            for segment in &data.segments {
                segment
                    .bands
                    .check_against(segment.morton.u32(), &segment.columns)
                    .unwrap_or_else(|e| panic!("segment {}: {e}", segment.seg_id));
                let codes = segment.morton.u32();
                let expected: Vec<u32> = segment
                    .cuts
                    .starts()
                    .iter()
                    .map(|&s| codes[s as usize])
                    .collect();
                assert_eq!(segment.cell_codes.codes(), expected.as_slice());
                assert_eq!(
                    segment.bands.copy_names().collect::<Vec<_>>(),
                    vec!["score"],
                    "segment {} copies its render column",
                    segment.seg_id
                );
                seen.insert(segment.seg_id.clone(), segment.bands.ids().to_vec());
            }
        }
    }
    seen
}

fn row(x: f64, y: f64, score: i32, key: &str) -> UnallocatedRow {
    UnallocatedRow {
        view: "s0".to_string(),
        join: None,
        descriptors: vec![b"0".to_vec()],
        x,
        y,
        scalars: vec![WalScalar::U64(key_id(key)), WalScalar::I32(score)],
        terms: Vec::new(),
        scoped: Vec::new(),
    }
}

#[test]
fn every_segment_carries_its_bands_through_a_flush_a_deletion_a_fold_and_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_scored(&root);
    let (cache, wal) = (tmp.path().join("cache"), tmp.path().join("wal.log"));

    let engine = open_engine_publishing(&root, &cache, &wal);
    let built = check_segments(&engine);
    assert_eq!(built.len(), 1, "a build writes one segment");
    let base_ids = built.values().next().unwrap().clone();
    assert!(
        base_ids.len() as u64 > ROWS / 64,
        "the base segment's bands hold its rows with {FIRST_BAND} or more leading zeros, nested"
    );

    // An ingest flushed: the new segment has its own bands, and its copies hold the scores sent.
    let rows: Vec<UnallocatedRow> = (0..400)
        .map(|i| {
            row(
                (i * 7 % 1000) as f64,
                (i * 11 % 1000) as f64,
                -i,
                &format!("new-{i}"),
            )
        })
        .collect();
    let mut rows = rows;
    for r in &mut rows {
        r.terms = engine.resolve_terms(&r.descriptors);
    }
    engine
        .ingest_rows(rows, "bands-1".to_string(), [1u8; 32])
        .expect("the ingest is accepted");
    publish_buffered(&engine);
    let flushed = check_segments(&engine);
    assert_eq!(flushed.len(), 2, "the flush wrote a segment");

    // A deleted row stays in its segment's bands until a compaction rewrites the segment.
    let gone = *base_ids
        .iter()
        .find(|&&id| band_of(id) >= FIRST_BAND)
        .unwrap();
    let entity = engine
        .resolve_tessera_ids(&[tessera_types::TesseraId::new(gone)])
        .unwrap()[0]
        .expect("a band entry names an item");
    engine.accept_change(entity, ChangeOp::Delete).unwrap();
    publish_buffered(&engine);
    let after_delete = check_segments(&engine);
    assert!(
        after_delete.values().any(|ids| ids.contains(&gone)),
        "a deleted row stays in its segment's bands until the fold"
    );

    fold(&engine);
    let folded = check_segments(&engine);
    assert_eq!(folded.len(), 1, "the fold writes one segment a view");
    assert!(
        folded.values().all(|ids| !ids.contains(&gone)),
        "the fold dropped the deleted row from the bands"
    );
    let folded_entries: usize = folded.values().map(Vec::len).sum();
    drop(engine);

    let engine = open_engine_publishing(&root, &cache, &wal);
    let reopened = check_segments(&engine);
    assert_eq!(reopened, folded, "a restart serves the same bands");
    assert_eq!(
        reopened.values().map(Vec::len).sum::<usize>(),
        folded_entries
    );
    drop(engine);

    let report = tessera_build::verify_deep(&root, &tessera_build::VerifyOpts::default())
        .expect("the bundle verifies deep");
    assert_eq!(report.band_entries as usize, folded_entries);
}

// ---------------------------------------------------------------------------------------------
// The levels' labels
// ---------------------------------------------------------------------------------------------

const SERVED: &str = "clusters/served";
const WALKED: &str = "clusters/walked";
const OVERLAPPING: &str = "clusters/overlapping";

fn flat(name: &str, layout: Option<ServingLayout>) -> LayerDeclaration {
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
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration::default(),
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout,
        shape: None,
    }
}

/// The derived files the served generation names for `layer`.
fn extents_of(engine: &Engine, layer: &str) -> Vec<DerivedExtent> {
    engine
        .generation()
        .bundle
        .partitions
        .values()
        .flat_map(|p| p.manifest.derived_extents.iter().cloned())
        .filter(|e| e.layer == layer)
        .collect()
}

/// `layer`'s label column and its band copy, checked against each other and against the base
/// segment's bands: the copy holds the column's label at every entry's row. Returns the column.
fn labels_of(root: &Path, engine: &Engine, layer: &str) -> LabelColumnPack {
    let generation = engine.generation();
    let prefix = root.join(&generation.prefix);
    let extents = extents_of(engine, layer);
    let column = extents
        .iter()
        .find(|e| {
            matches!(
                e.form,
                DerivedForm::LevelLabels
                    | DerivedForm::RowColumn {
                        layout: ServingLayout::RowMajorLabel
                    }
            )
        })
        .unwrap_or_else(|| panic!("{layer} has a label column: {extents:?}"));
    let copy = extents
        .iter()
        .find(|e| matches!(e.form, DerivedForm::BandLabels { .. }))
        .unwrap_or_else(|| panic!("{layer} has a band copy: {extents:?}"));
    assert_eq!(copy.level_version, column.level_version);
    let DerivedForm::BandLabels { seg_id } = &copy.form else {
        unreachable!()
    };
    let view = &generation.bundle.partitions.values().next().unwrap().views["s0"];
    let base = &view.segments[0];
    assert_eq!(
        seg_id, &base.seg_id,
        "the copy follows the base segment's bands"
    );
    let labels = LabelColumnPack::open(&prefix.join(&column.path)).unwrap();
    let copied = BandLabels::open(&prefix.join(&copy.path), &base.bands).unwrap();
    assert!(base.bands.entries() > 0);
    for (e, &row) in base.bands.rows().iter().enumerate() {
        assert_eq!(
            copied.label(e),
            labels.label(row as usize),
            "{layer}, entry {e}"
        );
    }
    labels
}

/// Every row of one artifact carries one label, no two artifacts share one, and every other row
/// is a hole.
fn assert_partition(engine: &Engine, labels: &LabelColumnPack, members: &[Vec<EntityId>]) {
    let generation = engine.generation();
    let space = &generation.bundle.partitions.values().next().unwrap().views["s0"].row_space;
    let mut claimed = vec![false; labels.rows() as usize];
    let mut seen = std::collections::BTreeSet::new();
    for artifact in members {
        let rows: Vec<usize> = artifact
            .iter()
            .filter_map(|e| space.row_of(*e))
            .map(|r| r.raw() as usize)
            .collect();
        let label = labels.label(rows[0]);
        assert_ne!(label, ROW_COLUMN_HOLE);
        assert!(seen.insert(label), "two artifacts share label {label}");
        for row in rows {
            assert_eq!(labels.label(row), label);
            claimed[row] = true;
        }
    }
    for (row, claimed) in claimed.iter().enumerate() {
        if !claimed {
            assert_eq!(
                labels.label(row),
                ROW_COLUMN_HOLE,
                "row {row} belongs to no artifact"
            );
        }
    }
}

#[test]
fn every_partitioning_level_has_a_label_column_and_a_band_copy_that_follow_the_fold() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_scored(&root);
    let (cache, wal) = (tmp.path().join("cache"), tmp.path().join("wal.log"));
    let map = source_to_new_map(&root, "v00000");
    let entity = |source: u64| EntityId::new(map[&source]);

    let engine = open_engine_publishing(&root, &cache, &wal);
    engine.set_background_refresh_for_test(false);
    engine
        .register_layer(flat(SERVED, Some(ServingLayout::RowMajorLabel)))
        .unwrap();
    engine.register_layer(flat(WALKED, None)).unwrap();
    engine.register_layer(flat(OVERLAPPING, None)).unwrap();

    // Ten artifacts over every item; five over the first half; two that overlap.
    let served: Vec<Vec<EntityId>> = (0..10)
        .map(|a| (0..ROWS).filter(|s| s % 10 == a).map(entity).collect())
        .collect();
    let mut walked: Vec<Vec<EntityId>> = (0..5)
        .map(|a| (0..ROWS / 2).filter(|s| s % 5 == a).map(entity).collect())
        .collect();
    let publish = |layer: &str, members: &[Vec<EntityId>]| {
        let batch = members
            .iter()
            .enumerate()
            .map(|(i, m)| IncomingArtifact::from_entities(Some(format!("a{i}")), m.clone()))
            .collect();
        engine.publish_artifacts(layer.into(), 0, batch).unwrap();
    };
    publish(SERVED, &served);
    publish(WALKED, &walked);
    publish(
        OVERLAPPING,
        &[
            (0..ROWS / 2).map(entity).collect(),
            (ROWS / 4..ROWS).map(entity).collect(),
        ],
    );

    fold(&engine);
    assert_eq!(
        engine.recorded_layout(WALKED, 0),
        Some(ServingLayout::ArtifactMajor),
        "a level of five artifacts is served artifact-major"
    );
    assert!(
        extents_of(&engine, OVERLAPPING).iter().all(|e| !matches!(
            e.form,
            DerivedForm::LevelLabels | DerivedForm::BandLabels { .. }
        )),
        "a level whose memberships overlap has no label column"
    );
    assert!(extents_of(&engine, WALKED)
        .iter()
        .any(|e| e.form == DerivedForm::LevelLabels));
    assert_partition(&engine, &labels_of(&root, &engine, SERVED), &served);
    assert_partition(&engine, &labels_of(&root, &engine, WALKED), &walked);

    // A growth moves the level, so its label column and copy are no longer current and the next
    // manifest names neither; the served level is untouched and keeps its copy.
    let joining = entity(ROWS - 1);
    engine
        .grow_memberships(
            WALKED.into(),
            0,
            vec![IncomingGrowth::from_entities("a0".to_string(), [joining])],
        )
        .unwrap();
    walked[0].push(joining);
    let mut fresh = row(500.0, 500.0, 7, "after-the-fold");
    fresh.terms = engine.resolve_terms(&fresh.descriptors);
    engine
        .ingest_rows(vec![fresh], "labels-1".to_string(), [2u8; 32])
        .unwrap();
    publish_buffered(&engine);
    let unlabelled = |engine: &Engine| {
        extents_of(engine, WALKED).iter().all(|e| {
            !matches!(
                e.form,
                DerivedForm::LevelLabels | DerivedForm::BandLabels { .. }
            )
        })
    };
    assert!(
        unlabelled(&engine),
        "a moved level names no label column or copy"
    );
    labels_of(&root, &engine, SERVED);
    drop(engine);

    // A restart serves the same manifest, which still verifies.
    let engine = open_engine_publishing(&root, &cache, &wal);
    assert!(unlabelled(&engine));
    drop(engine);
    tessera_build::verify_deep(&root, &tessera_build::VerifyOpts::default())
        .expect("a bundle whose moved level has no copy verifies deep");

    // The fold writes the level again, with the growth in both.
    let engine = open_engine_publishing(&root, &cache, &wal);
    fold(&engine);
    assert_partition(&engine, &labels_of(&root, &engine, WALKED), &walked);
    assert_partition(&engine, &labels_of(&root, &engine, SERVED), &served);
    drop(engine);

    let report = tessera_build::verify_deep(&root, &tessera_build::VerifyOpts::default())
        .expect("the bundle verifies deep");
    assert_eq!(report.band_label_copies, 2);
}

/// A build writes the label columns and their copies for the layers it publishes, as a fold
/// does: the corpus generator's partition is enumerated and flat, and its boundary layer is
/// spatial, so both partition the rows; its flat and treed layers overlap and have none.
#[test]
fn a_build_writes_the_label_columns_and_their_copies() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let corpus = tessera_corpus::Corpus::new(7, ROWS, extent()).unwrap();
    corpus
        .write_points_parquet(&tmp.path().join("points.parquet"))
        .unwrap();
    corpus
        .write_pairs_parquet(&tmp.path().join("pairs.parquet"))
        .unwrap();
    build_corpus_fixture_with_layers(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        &corpus,
    );
    let engine = open_engine(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    labels_of(&root, &engine, "generator/partition-enumerated");
    for overlapping in ["generator/flat", "generator/treed"] {
        assert!(
            extents_of(&engine, overlapping).iter().all(|e| !matches!(
                e.form,
                DerivedForm::LevelLabels | DerivedForm::BandLabels { .. }
            )),
            "{overlapping} overlaps and has no label column"
        );
    }
    drop(engine);
    let report = tessera_build::verify_deep(&root, &tessera_build::VerifyOpts::default())
        .expect("the built bundle verifies deep");
    assert!(report.band_label_copies >= 1);
}

/// `tessera verify --deep` refuses a band label copy that does not hold its column's labels, and
/// a current label column that has no copy, naming the file in each case.
#[test]
fn deep_verification_refuses_a_damaged_copy_and_a_missing_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_scored(&root);
    let map = source_to_new_map(&root, "v00000");
    let engine = open_engine_publishing(&root, &tmp.path().join("cache"), &tmp.path().join("wal"));
    engine.register_layer(flat(WALKED, None)).unwrap();
    let batch = (0..5)
        .map(|a| {
            IncomingArtifact::from_entities(
                Some(format!("a{a}")),
                (0..ROWS)
                    .filter(|s| s % 5 == a)
                    .map(|s| EntityId::new(map[&s])),
            )
        })
        .collect();
    engine.publish_artifacts(WALKED.into(), 0, batch).unwrap();
    fold(&engine);
    let prefix = root.join(&engine.generation().prefix);
    let extents = extents_of(&engine, WALKED);
    drop(engine);
    let path_of = |form: fn(&DerivedForm) -> bool| {
        prefix.join(&extents.iter().find(|e| form(&e.form)).unwrap().path)
    };
    let copy = path_of(|f| matches!(f, DerivedForm::BandLabels { .. }));
    let column = path_of(|f| *f == DerivedForm::LevelLabels);
    let refused_on = |expected: &Path| match tessera_build::verify_deep(
        &root,
        &tessera_build::VerifyOpts::default(),
    ) {
        Err(tessera_build::BuildError::Store(
            tessera_store::StoreError::FileVerificationFailed { path, .. },
        )) => assert_eq!(path, expected),
        other => panic!(
            "expected a refusal naming {}, got {other:?}",
            expected.display()
        ),
    };
    tessera_build::verify_deep(&root, &tessera_build::VerifyOpts::default())
        .expect("the folded bundle verifies deep");

    // A label changed in the copy.
    let good = std::fs::read(&copy).unwrap();
    let mut damaged = good.clone();
    let last = damaged.len() - 1;
    damaged[last] ^= 1;
    std::fs::write(&copy, &damaged).unwrap();
    refused_on(&copy);
    std::fs::write(&copy, &good).unwrap();

    // The copy no longer named by the manifest the bundle serves.
    let partition = prefix.join("partitions/default");
    let newest = std::fs::read_dir(&partition)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("SEGMENTS-") && n.ends_with(".json"))
        })
        .max_by_key(|p| {
            let name = p.file_name().unwrap().to_str().unwrap();
            name["SEGMENTS-".len()..name.len() - ".json".len()]
                .parse::<u64>()
                .unwrap()
        })
        .unwrap();
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&newest).unwrap()).unwrap();
    manifest["derived_extents"]
        .as_array_mut()
        .unwrap()
        .retain(|e| !e["path"].as_str().unwrap().contains("/band-labels/"));
    std::fs::write(&newest, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    refused_on(&column);
}
