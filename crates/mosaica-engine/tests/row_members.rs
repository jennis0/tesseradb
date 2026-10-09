//! **Every row-major column has its members by artifact and their coverings beside it, on every path
//! that writes, composes or changes the column.**
//!
//! A member bitmap is the transpose of the column's labels over its base rows, and a covering is at
//! most 32 row ranges holding all of an artifact's members, cut at its widest gaps. The cases read
//! what is stored after a build, a fold, a restart and a publication at runtime, and what the engine
//! holds after a growth, a recomposition and a dropped column, each against the column's own labels
//! transposed by hand, and each covering must hold every member in at most 32 ranges. Which split
//! a fresh covering takes is the writer's own test, beside it in `mosaica-store`.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{ArrayRef, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use mosaica_build::BuildArgs;
use mosaica_engine::row_column::RowColumn;
use mosaica_engine::{Engine, LayerSelection};
use mosaica_lifecycle::{IncomingArtifact, IncomingGrowth};
use mosaica_store::manifest::{DerivedForm, SegmentsManifest};
use mosaica_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource, ServingLayout,
};
use mosaica_types::EntityId;

const N: u64 = 3_000;
/// Sources at or past this are in no artifact of the built layers, so a growth can take them
/// without two artifacts claiming a row.
const CLAIMED: u64 = 2_800;
const COVERING_RANGES: usize = 32;

/// A label column served from its column alone: source `e` is in artifact `e % 120`.
const LABEL: &str = "grid/label";
const LABEL_ARTIFACTS: u64 = 120;
/// A list column: source `e` is in artifact `e % 30`, and every eleventh also in the next one.
const LIST: &str = "grid/list";
const LIST_ARTIFACTS: u64 = 30;
/// A label column whose layer derives a hull, so its form holds its rows and loses its column
/// where a growth makes two artifacts claim a row.
const HULL: &str = "grid/hull";
const HULL_ARTIFACTS: u64 = 20;

fn layer_toml(name: &str, layout: &str, computed: &str) -> String {
    let stem = name.replace('/', "_");
    format!(
        r#"
[[layer]]
name = "{name}"
views = ["s0"]
source = "{stem}"
membership = "enumerated"
layout = "{layout}"
visibility = "public"
artifact_visibility = {{ default = "inherited" }}
require_member_visibility = "none"
hierarchy = {{ kind = "flat" }}
content = {{ computed = [{computed}] }}

  [layer.members]
  source = "{stem}_members"
  fields = {{ id = "entity" }}
"#
    )
}

fn config_toml() -> String {
    let mut out = String::from("[sources]\n");
    for name in [LABEL, LIST, HULL] {
        let stem = name.replace('/', "_");
        out.push_str(&format!(
            "{stem} = \"{stem}.parquet\"\n{stem}_members = \"{stem}_members.parquet\"\n"
        ));
    }
    out.push_str(
        r#"
[[attribute]]
name   = "id"
type   = "u64"
unique = true
field  = "entity_id"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }
"#,
    );
    out.push_str(&layer_toml(LABEL, "column", "\"centroid\""));
    out.push_str(&layer_toml(LIST, "list", "\"centroid\""));
    out.push_str(&layer_toml(HULL, "column", "\"hull\""));
    out
}

fn key(layer: &str, artifact: u64) -> String {
    format!("{}-{artifact:03}", &layer[5..])
}

/// Each built layer's `(key, source)` memberships.
fn memberships(layer: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for e in 0..CLAIMED {
        match layer {
            LABEL => out.push((key(LABEL, e % LABEL_ARTIFACTS), e)),
            LIST => {
                out.push((key(LIST, e % LIST_ARTIFACTS), e));
                if e % 11 == 0 {
                    out.push((key(LIST, (e + 1) % LIST_ARTIFACTS), e));
                }
            }
            _ => out.push((key(HULL, e % HULL_ARTIFACTS), e)),
        }
    }
    out
}

fn write(path: &Path, schema: Arc<Schema>, batch: RecordBatch) {
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_layer(dir: &Path, layer: &str, artifacts: u64, rows: &[(String, u64)]) {
    let stem = layer.replace('/', "_");
    let keys: Vec<String> = (0..artifacts).map(|i| key(layer, i)).collect();
    let schema = Arc::new(Schema::new(vec![Field::new("key", DataType::Utf8, false)]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(keys)) as ArrayRef],
    )
    .unwrap();
    write(&dir.join(format!("{stem}.parquet")), schema, batch);
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("rank", DataType::UInt32, true),
        Field::new("entity", DataType::UInt64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(
                rows.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            )) as ArrayRef,
            Arc::new(UInt32Array::from(vec![None::<u32>; rows.len()])),
            Arc::new(UInt64Array::from(
                rows.iter().map(|(_, e)| *e).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    write(&dir.join(format!("{stem}_members.parquet")), schema, batch);
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    cache: PathBuf,
    wal: PathBuf,
    map: BTreeMap<u64, u64>,
}

/// The three layers built over `N` points, with `extra` added to the label layer's memberships.
fn fixture_with(extra: &[(String, u64)]) -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    let root = dir.join("bundle");
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    write_points_n(&points, N);
    write_pairs_n(&pairs, N);
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, config_toml()).unwrap();
    let config = mosaica_build::config::Config::parse(&config_path, &Default::default())
        .expect("the fixture's declaration parses");
    let mut label = memberships(LABEL);
    label.extend_from_slice(extra);
    write_layer(dir, LABEL, LABEL_ARTIFACTS, &label);
    write_layer(dir, LIST, LIST_ARTIFACTS, &memberships(LIST));
    write_layer(dir, HULL, HULL_ARTIFACTS, &memberships(HULL));
    let args = BuildArgs {
        views: vec![mosaica_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: mosaica_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: mosaica_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: mosaica_build::config::AttributeSource::over(points, &config.schema),
        out: root.clone(),
        limit: None,
        strict: false,
        identity_key: test_key(),
        shard_id: 0,
        layers: config.layers,
        layer_inputs: config.layer_sources,
        scoped_layers: Default::default(),
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: config.schema,
    };
    mosaica_build::build(&args).expect("the fixture builds");
    let map = source_to_new_map(&root, "v00000");
    Fixture {
        root,
        cache: dir.join("cache"),
        wal: dir.join("wal.log"),
        map,
        _tmp: tmp,
    }
}

fn fixture() -> Fixture {
    fixture_with(&[])
}

impl Fixture {
    fn open(&self) -> Engine {
        let engine = open_engine_publishing(&self.root, &self.cache, &self.wal);
        engine.set_background_refresh_for_test(false);
        engine
    }

    fn entities(&self, sources: impl IntoIterator<Item = u64>) -> Vec<EntityId> {
        sources
            .into_iter()
            .map(|s| EntityId::new(self.map[&s]))
            .collect()
    }
}

/// What one artifact's stored or held forms are: its members ascending, and its covering.
type Forms = Vec<(Vec<u32>, Vec<(u32, u32)>)>;

/// Every artifact's base rows, read off a column's labels one row at a time.
fn labels_transposed(column: &RowColumn) -> Vec<Vec<u32>> {
    let mut sets = vec![Vec::new(); column.len()];
    for row in 0..column.base_rows() {
        column.for_each_label(row, |ordinal| sets[ordinal as usize].push(row));
    }
    sets
}

/// A covering that holds `members` in at most 32 ascending, disjoint ranges.
fn assert_covers(covering: &[(u32, u32)], members: &[u32], what: &str) {
    assert!(
        covering.len() <= COVERING_RANGES,
        "{what}: {} ranges",
        covering.len()
    );
    assert!(
        covering.windows(2).all(|w| w[0].1 < w[1].0) && covering.iter().all(|r| r.0 <= r.1),
        "{what}: the ranges are not ascending and disjoint"
    );
    assert_eq!(covering.is_empty(), members.is_empty(), "{what}");
    for row in members {
        assert!(
            covering.iter().any(|&(lo, hi)| lo <= *row && *row <= hi),
            "{what}: member row {row} is outside the covering"
        );
    }
}

/// The forms an engine holds beside a column, checked against its labels.
fn held_forms(column: &RowColumn, what: &str) -> Forms {
    let members = column
        .members()
        .unwrap_or_else(|| panic!("{what}: a served column holds its members"));
    assert_eq!(members.base_rows(), column.base_rows(), "{what}");
    let expected = labels_transposed(column);
    let mut out = Vec::new();
    for (ordinal, rows) in expected.iter().enumerate() {
        let held: Vec<u32> = members.members(ordinal as u32).to_bitmap().iter().collect();
        assert_eq!(&held, rows, "{what}: ordinal {ordinal}'s members");
        let covering = members.covering(ordinal as u32);
        assert_covers(&covering, rows, &format!("{what}: ordinal {ordinal}"));
        out.push((held, covering));
    }
    out
}

/// The prefix `CURRENT` names and its partition manifest.
fn manifest_at(root: &Path) -> (PathBuf, SegmentsManifest) {
    let bundle = mosaica_store::read::open_bundle(root).expect("the bundle opens");
    let prefix = root.join(current_prefix(root));
    let manifest = bundle.partitions["default"].manifest.clone();
    (prefix, manifest)
}

/// Every stored member file, per `(layer, level)`, checked against its column's labels.
fn stored_forms(root: &Path) -> BTreeMap<(String, u32), Forms> {
    let (prefix, manifest) = manifest_at(root);
    let mut out = BTreeMap::new();
    for column in &manifest.derived_extents {
        let DerivedForm::RowColumn { layout } = column.form else {
            continue;
        };
        let beside: Vec<_> = manifest
            .derived_extents
            .iter()
            .filter(|e| e.form == DerivedForm::RowMembers && e.same_level(column))
            .collect();
        assert_eq!(
            beside.len(),
            1,
            "{}: one member file beside its column",
            column.layer
        );
        let labels = RowColumn::open_labels(&prefix.join(&column.path), layout).unwrap();
        let pack = mosaica_store::row_members::RowMembersPack::open(&prefix.join(&beside[0].path))
            .expect("the member file opens");
        assert_eq!(pack.rows(), labels.base_rows());
        assert_eq!(pack.ordinals() as usize, labels.len());
        let mut forms = Vec::new();
        for (ordinal, rows) in labels_transposed(&labels).iter().enumerate() {
            let ordinal = ordinal as u32;
            let held: Vec<u32> = pack
                .members(ordinal)
                .map(|v| v.iter().collect())
                .unwrap_or_default();
            assert_eq!(
                &held, rows,
                "{} ordinal {ordinal}'s stored members",
                column.layer
            );
            let covering: Vec<(u32, u32)> = pack.covering(ordinal).collect();
            assert_covers(
                &covering,
                rows,
                &format!("{} ordinal {ordinal}'s stored covering", column.layer),
            );
            forms.push((held, covering));
        }
        assert!(
            out.insert((column.layer.clone(), column.level), forms)
                .is_none(),
            "one column per level in one view"
        );
    }
    out
}

/// Ask for every layer over the whole map, so the engine holds each level's form.
fn touch(engine: &Engine, layers: &[&str]) {
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    engine
        .viewport_artifacts(
            &session,
            mosaica_engine::ViewportArtifactsRequest::new("s0", 0, WHOLE_MAP, usize::MAX)
                .layers(LayerSelection::Named(layers)),
        )
        .expect("a viewport naming the layers");
}

fn held_column(engine: &Engine, layer: &str) -> Option<RowColumn> {
    engine
        .held_artifact_form_for_test("s0", layer, 0)
        .expect("the level's form is held")
        .column()
        .cloned()
}

#[test]
fn a_build_writes_the_members_and_coverings_beside_every_row_major_column() {
    let fx = fixture();
    let stored = stored_forms(&fx.root);
    let layers: Vec<&str> = stored.keys().map(|(layer, _)| layer.as_str()).collect();
    assert_eq!(
        layers,
        vec![HULL, LABEL, LIST],
        "a member file beside each row-major column"
    );
    let label = &stored[&(LABEL.to_string(), 0)];
    assert_eq!(
        label.iter().map(|(rows, _)| rows.len() as u64).sum::<u64>(),
        CLAIMED,
        "the label layer's members are every claimed source"
    );
    let list = &stored[&(LIST.to_string(), 0)];
    assert_eq!(
        list.iter().map(|(rows, _)| rows.len() as u64).sum::<u64>(),
        CLAIMED + CLAIMED.div_ceil(11),
        "an overlapping row is a member of each artifact claiming it"
    );
    let report = mosaica_build::verify_deep(&fx.root, &mosaica_build::VerifyOpts::default())
        .expect("the bundle verifies");
    assert_eq!(report.row_member_files, 3);
}

#[test]
fn build_and_fold_write_the_same_forms_for_the_same_data() {
    let fx = fixture();
    let built = stored_forms(&fx.root);
    let engine = fx.open();
    fold(&engine);
    drop(engine);
    let folded = stored_forms(&fx.root);
    assert_ne!(
        manifest_at(&fx.root).0,
        fx.root.join("v00000"),
        "the fold published a prefix"
    );
    assert_eq!(built, folded);
}

#[test]
fn a_fold_after_ingest_and_growth_writes_the_forms_over_its_new_rows_and_a_restart_adopts_them() {
    let fx = fixture();
    let engine = fx.open();
    let ingested: Vec<EntityId> = (0..40).map(|i| ingest(&engine, &format!("b{i}"))).collect();
    publish_buffered(&engine);
    let mut joining = ingested.clone();
    joining.extend(fx.entities(CLAIMED..CLAIMED + 10));
    engine
        .grow_memberships(
            LABEL.into(),
            0,
            vec![IncomingGrowth::from_entities(key(LABEL, 7), joining)],
        )
        .expect("a growth into an artifact that exists");
    tick(&engine);
    fold(&engine);
    drop(engine);

    let stored = stored_forms(&fx.root);
    let label = &stored[&(LABEL.to_string(), 0)];
    assert_eq!(
        label.iter().map(|(rows, _)| rows.len() as u64).sum::<u64>(),
        CLAIMED + 50,
        "the ingested and grown members are in the fold's member file"
    );

    // A restart adopts the fold's files rather than composing the columns again, and holds what
    // they hold.
    let engine = fx.open();
    touch(&engine, &[LABEL, LIST, HULL]);
    assert!(engine.columns_adopted() > 0);
    assert_eq!(
        engine.columns_composed(),
        0,
        "the restart composed a column it was handed"
    );
    for layer in [LABEL, LIST, HULL] {
        let column = held_column(&engine, layer).expect("a row-major level holds its column");
        let held = held_forms(&column, &format!("{layer} after the restart"));
        assert_eq!(held, stored[&(layer.to_string(), 0)], "{layer}");
    }
}

#[test]
fn a_restart_after_a_growth_without_a_fold_completes_the_adopted_members() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .grow_memberships(
            LABEL.into(),
            0,
            vec![IncomingGrowth::from_entities(
                key(LABEL, 9),
                fx.entities(CLAIMED..CLAIMED + 30),
            )],
        )
        .expect("a growth into an artifact that exists");
    tick(&engine);
    drop(engine);

    // The prefix still names the build's column and member file, written before the growth; the
    // restart adopts both and brings them over the rows the growth added.
    let engine = fx.open();
    touch(&engine, &[LABEL]);
    assert!(
        engine.columns_adopted() > 0,
        "the build's column was adopted"
    );
    assert_eq!(
        engine.columns_composed(),
        0,
        "and completed rather than composed again"
    );
    let column = held_column(&engine, LABEL).expect("the level keeps its column");
    let held = held_forms(&column, "after a restart past a growth");
    assert_eq!(
        held.iter().map(|(rows, _)| rows.len() as u64).sum::<u64>(),
        CLAIMED + 30,
        "the grown members are held beside the adopted column"
    );
}

fn runtime_declaration(name: &str, layout: ServingLayout) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: None,
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: mosaica_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: vec!["centroid".into()],
            supplied: Vec::new(),
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: Some(layout),
        shape: None,
    }
}

#[test]
fn a_level_published_at_runtime_holds_the_members_of_the_column_it_composes() {
    let fx = fixture();
    let engine = fx.open();
    for (name, layout, artifacts) in [
        ("late/label", ServingLayout::RowMajorLabel, 25u64),
        ("late/list", ServingLayout::RowMajorList, 9),
    ] {
        engine
            .register_layer(runtime_declaration(name, layout))
            .unwrap();
        let published: Vec<IncomingArtifact> = (0..artifacts)
            .map(|i| {
                let mut sources: Vec<u64> = (0..N).filter(|e| e % artifacts == i).collect();
                if layout == ServingLayout::RowMajorList {
                    sources.extend((0..N).filter(|e| e % 13 == 0 && (e + 1) % artifacts == i));
                }
                IncomingArtifact::from_entities(Some(format!("{name}-{i}")), fx.entities(sources))
            })
            .collect();
        engine.publish_artifacts(name.into(), 0, published).unwrap();
    }
    tick(&engine);
    touch(&engine, &["late/label", "late/list"]);
    for name in ["late/label", "late/list"] {
        let column = held_column(&engine, name).expect("the level is served from a column");
        let held = held_forms(&column, name);
        assert!(
            held.iter().any(|(rows, _)| !rows.is_empty()),
            "{name} holds members"
        );
    }
}

#[test]
fn a_growth_after_the_build_holds_what_a_rebuild_with_it_stores() {
    let grown: Vec<(String, u64)> = (CLAIMED..CLAIMED + 60)
        .map(|e| (key(LABEL, e % 5), e))
        .collect();
    let fx = fixture();
    let engine = fx.open();
    touch(&engine, &[LABEL]);
    let mut by_key: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for (key, source) in &grown {
        by_key.entry(key.clone()).or_default().push(*source);
    }
    engine
        .grow_memberships(
            LABEL.into(),
            0,
            by_key
                .into_iter()
                .map(|(key, sources)| IncomingGrowth::from_entities(key, fx.entities(sources)))
                .collect(),
        )
        .expect("a growth into artifacts that exist");
    tick(&engine);
    touch(&engine, &[LABEL]);
    let column = held_column(&engine, LABEL).expect("the level keeps its column");
    let held = held_forms(&column, "after the growth");

    let rebuilt = stored_forms(&fixture_with(&grown).root);
    let rebuilt = &rebuilt[&(LABEL.to_string(), 0)];
    assert_eq!(held.len(), rebuilt.len());
    for (ordinal, ((rows, covering), (expected, _))) in held.iter().zip(rebuilt).enumerate() {
        assert_eq!(
            rows, expected,
            "ordinal {ordinal}'s members against the rebuild"
        );
        assert_covers(
            covering,
            expected,
            &format!("ordinal {ordinal} against the rebuild"),
        );
    }
}

#[test]
fn a_column_recomposed_as_a_list_holds_its_new_members_and_a_dropped_one_takes_them_with_it() {
    let fx = fixture();
    let engine = fx.open();
    touch(&engine, &[LABEL, HULL]);
    // Source 0 is in artifact 0 of both layers; joining it to artifact 1 makes a row carry two.
    for layer in [LABEL, HULL] {
        engine
            .grow_memberships(
                layer.into(),
                0,
                vec![IncomingGrowth::from_entities(
                    key(layer, 1),
                    fx.entities([0]),
                )],
            )
            .expect("a growth into an artifact that exists");
    }
    tick(&engine);
    touch(&engine, &[LABEL, HULL]);

    let listed = held_column(&engine, LABEL).expect("a level served from its column keeps one");
    assert_eq!(listed.layout(), ServingLayout::RowMajorList);
    let held = held_forms(&listed, "recomposed as a list");
    let row = held[0].0[0];
    assert!(
        held[1].0.contains(&row),
        "the shared row is a member of both artifacts"
    );

    let dropped = engine.held_artifact_form_for_test("s0", HULL, 0).unwrap();
    assert!(
        dropped.column().is_none(),
        "the hull level lost its column and its members with it"
    );
    assert_eq!(dropped.layout(), ServingLayout::ArtifactMajor);
}

#[test]
fn the_covering_index_returns_every_artifact_with_a_member_in_a_row_range() {
    let fx = fixture();
    let engine = fx.open();
    touch(&engine, &[LABEL, LIST]);
    let check = |column: &RowColumn, what: &str| {
        let members = column.members().unwrap();
        let sets = labels_transposed(column);
        let rows = column.base_rows();
        let mut state = 11u64;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        for _ in 0..500 {
            let lo = next() % rows;
            let hi = (lo + next() % (1 + next() % rows)).min(rows - 1);
            let found = members.overlapping(lo..=hi);
            for (ordinal, set) in sets.iter().enumerate() {
                let inside = set.iter().any(|r| (lo..=hi).contains(r));
                if inside {
                    assert!(
                        found.contains(ordinal as u32),
                        "{what}: ordinal {ordinal} has a member in [{lo}, {hi}] and was not found"
                    );
                }
            }
            for ordinal in found.iter() {
                assert!(
                    members
                        .covering(ordinal)
                        .iter()
                        .any(|&(rlo, rhi)| rlo <= hi && rhi >= lo),
                    "{what}: ordinal {ordinal}'s covering misses [{lo}, {hi}] and it was found"
                );
            }
        }
    };
    for layer in [LABEL, LIST] {
        check(&held_column(&engine, layer).unwrap(), layer);
    }
    engine
        .grow_memberships(
            LABEL.into(),
            0,
            vec![IncomingGrowth::from_entities(
                key(LABEL, 3),
                fx.entities(CLAIMED..CLAIMED + 100),
            )],
        )
        .unwrap();
    tick(&engine);
    touch(&engine, &[LABEL]);
    check(&held_column(&engine, LABEL).unwrap(), "after a growth");
}
