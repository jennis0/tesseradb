//! Layers, and the artifacts in them, declared as build inputs.
//!
//! What this file is *for* is that the build plane and the control plane produce the same object:
//! the same declarations refused, the same entities allocated, the same extents written. So the
//! assertions are against the manifest a served bundle is read from, and against the marks whose
//! loss would let a later online registration reissue ids a built layer already holds.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    ArrayRef, FixedSizeListArray, Float64Array, Int64Array, ListArray, ListBuilder, StringArray,
    StringBuilder, UInt32Array, UInt64Array,
};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::{build, build_in_memory, BuildArgs};
use tessera_spatial::Bounds;
use tessera_store::read::open_bundle;
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const N_ITEMS: u64 = 250;

fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

fn write_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..N_ITEMS).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_pairs(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for e in 0..N_ITEMS {
        entities.push(e);
        terms.push((e % 5) as u32);
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// The coordinate system every fixture layer here is drawn on. Held apart from the layer blocks
/// so a test can reorder or rewrite those without disturbing the view they all name.
const VIEW_TOML: &str = r#"
[[view]]
name             = "s0"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }
"#;

/// A clustering, and a label layer attached into it — the shape the whole feature exists for.
const LAYERS_TOML: &str = r#"
[[layer]]
name = "clusters/a"
title = "clusters"
views = ["s0"]
source = "clusters.parquet"
membership = "enumerated"
visibility = "0"
artifact_visibility = { default = "inherited" }
require_member_visibility = { count = 2 }
hierarchy = { kind = "flat", prune_children = false }
content = { computed = ["centroid"] }

  [layer.members]
  source = "clusters_members.parquet"

[[layer]]
name = "topics/x"
title = "topics"
views = ["s0"]
source = "topics.parquet"
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "none"
hierarchy = { kind = "flat" }
depends_on = ["clusters/a"]

  [layer.members]
  source = "topics_members.parquet"

  [[layer.content.supplied]]
  name = "topic"
  type = "text"
  require_member_visibility = "all"
"#;

/// Write one Parquet file.
fn write(path: &Path, schema: Arc<Schema>, batch: RecordBatch) {
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// The clustering: **one row per artifact**, and no column names the layer — the file is the
/// layer's own.
fn write_clusters(path: &Path) {
    let schema = Arc::new(Schema::new(vec![Field::new("key", DataType::Utf8, false)]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(vec!["c-0000", "c-0001"])) as ArrayRef],
    )
    .unwrap();
    write(path, schema, batch);
}

/// The label layer: one row, carrying its whole ranking in `contents` — best first — and the edge
/// it hangs from.
fn write_topics(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("contents", ranked(), true),
        Field::new("attached_layer", DataType::Utf8, true),
        Field::new("attached_key", DataType::Utf8, true),
    ]));
    let mut contents = ListBuilder::new(ListBuilder::new(StringBuilder::new()));
    contents.values().values().append_value("the whole cluster");
    contents.values().append(true);
    contents.values().values().append_value("the visible part");
    contents.values().append(true);
    contents.append(true);
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec!["l-0000"])) as ArrayRef,
            Arc::new(contents.finish()),
            Arc::new(StringArray::from(vec![Some("clusters/a")])),
            Arc::new(StringArray::from(vec![Some("c-0000")])),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

/// The `contents` column's type: one entry per rank, each a value per supplied kind.
fn ranked() -> DataType {
    DataType::List(Arc::new(Field::new(
        "item",
        DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
        true,
    )))
}

/// One row per `(artifact, entity)` for **one layer**, deliberately in the order given — ordinals
/// must be a function of the artifacts and not of the file's row order.
fn write_members_rows(path: &Path, rows: &[(&str, Option<u32>, u64)]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("rank", DataType::UInt32, true),
        Field::new("entity", DataType::UInt64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(
                rows.iter().map(|(k, _, _)| *k).collect::<Vec<_>>(),
            )) as ArrayRef,
            Arc::new(UInt32Array::from(
                rows.iter().map(|(_, r, _)| *r).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                rows.iter().map(|(_, _, e)| *e).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

fn cluster_member_rows(members_of_first_cluster: &[u64]) -> Vec<(&'static str, Option<u32>, u64)> {
    let mut rows: Vec<(&str, Option<u32>, u64)> = members_of_first_cluster
        .iter()
        .map(|&m| ("c-0000", None, m))
        .collect();
    rows.extend((100..110u64).map(|m| ("c-0001", None, m)));
    rows
}

fn topic_member_rows(members_of_first_cluster: &[u64]) -> Vec<(&'static str, Option<u32>, u64)> {
    let mut rows = Vec::new();
    for &m in members_of_first_cluster {
        rows.push(("l-0000", None, m));
        // Rank 0 was generated from the whole cluster; rank 1 from a third of it.
        rows.push(("l-0000", Some(0), m));
        if m % 3 == 0 {
            rows.push(("l-0000", Some(1), m));
        }
    }
    rows
}

struct Inputs {
    _tmp: tempfile::TempDir,
    points: PathBuf,
    pairs: PathBuf,
    config: PathBuf,
    dir: PathBuf,
}

impl Inputs {
    /// A file in the fixture's directory, which is also what a relative `source` resolves against.
    fn at(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

fn inputs() -> Inputs {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let config = dir.join("config.toml");
    write_points(&points);
    write_pairs(&pairs);
    std::fs::write(&config, format!("{VIEW_TOML}{LAYERS_TOML}")).unwrap();
    write_clusters(&dir.join("clusters.parquet"));
    write_topics(&dir.join("topics.parquet"));
    let members: Vec<u64> = (0..30).collect();
    write_members_rows(
        &dir.join("clusters_members.parquet"),
        &cluster_member_rows(&members),
    );
    write_members_rows(
        &dir.join("topics_members.parquet"),
        &topic_member_rows(&members),
    );
    Inputs {
        _tmp: tmp,
        points,
        pairs,
        config,
        dir,
    }
}

fn args(inputs: &Inputs, out: &Path) -> BuildArgs {
    BuildArgs {
        point_fields: Default::default(),
        corpus_fields: Default::default(),
        points: inputs.points.clone(),
        corpus: Some(inputs.points.clone()),
        access: tessera_build::config::AccessInput::relation(inputs.pairs.clone()),
        out: out.to_path_buf(),
        extent: extent(),
        view_id: "s0".to_string(),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: false,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    }
}

/// Parse the fixture config and run the build over it — the two halves the CLI does in order, so a
/// declaration refusal and a build refusal reach a test as the same `Result`.
fn run(inputs: &Inputs, out: &Path) -> Result<tessera_build::BuildReport, tessera_build::BuildError> {
    let config = tessera_build::config::Config::parse(&inputs.config, &Default::default())?;
    let mut args = args(inputs, out);
    args.layers = config.layers;
    args.layer_inputs = config.layer_sources;
    args.schema = config.schema;
    build(&args)
}

fn manifest_of(root: &Path) -> tessera_store::manifest::SegmentsManifest {
    let bundle = open_bundle(root).expect("a bundle built with layers opens");
    bundle
        .partitions
        .values()
        .next()
        .expect("one partition")
        .manifest
        .clone()
}

/// **The headline: a built bundle carries its layers, and the mark that keeps their ids theirs.**
///
/// A manifest without `entity_id_low_water` below the ceiling would hand the next online
/// registration ids these layers already hold — two entities under one `tessera_id`.
#[test]
fn a_build_registers_its_layers_and_publishes_their_artifacts() {
    let inputs = inputs();
    let out = inputs.dir.join("bundle");
    run(&inputs, &out).expect("a build with layers succeeds");
    let manifest = manifest_of(&out);

    let names: Vec<&str> = manifest
        .layers
        .iter()
        .map(|l| l.declaration.name.as_str())
        .collect();
    assert_eq!(names, vec!["clusters/a", "topics/x"]);
    assert!(
        manifest.entity_id_low_water < tessera_types::layer::ROWLESS_CEILING,
        "the row-less mark must record what the layers claimed"
    );
    // Layer entities and artifact runs alike come out of the row-less region, above every point.
    for layer in &manifest.layers {
        assert!(layer.entity.raw() > N_ITEMS);
        assert!(layer.entity.raw() >= manifest.entity_id_low_water);
        for runs in &layer.runs {
            assert!(runs.capacity() >= tessera_types::layer::RESERVED_BLOCK);
        }
    }

    // Two levels' worth of memberships — one extent per level — and one content extent for the
    // label's two descriptions.
    let mut extents: Vec<(&str, u32)> = manifest
        .membership_extents
        .iter()
        .map(|e| (e.layer.as_str(), e.count))
        .collect();
    extents.sort();
    assert_eq!(extents, vec![("clusters/a", 2), ("topics/x", 1)]);
    assert_eq!(manifest.artifact_record_extents.len(), 1);
    assert_eq!(
        manifest.membership_extents.iter().map(|e| e.ordinal_lo).max(),
        Some(0),
        "a build publishes each level once, from ordinal zero"
    );
}

/// The two build implementations must agree about layers as they agree about everything else —
/// they are byte-identity oracles for each other, and a layer section that differed would be an
/// identity difference the equivalence test cannot see.
#[test]
fn both_build_paths_place_the_same_layers_on_the_same_entities() {
    let inputs = inputs();
    let streamed = inputs.dir.join("streamed");
    let linear = inputs.dir.join("linear");
    let config = tessera_build::config::Config::parse(&inputs.config, &Default::default())
        .expect("the fixture config parses");
    let mut linear_args = args(&inputs, &linear);
    linear_args.layers = config.layers;
    linear_args.layer_inputs = config.layer_sources;
    linear_args.schema = config.schema;
    run(&inputs, &streamed).unwrap();
    build_in_memory(&linear_args).unwrap();

    let a = manifest_of(&streamed);
    let b = manifest_of(&linear);
    assert_eq!(a.layers, b.layers);
    assert_eq!(a.entity_id_low_water, b.entity_id_low_water);
    assert_eq!(a.membership_extents, b.membership_extents);

    // **The bytes, not only the descriptors.** A `MembershipExtent` carries a path, a layer, a
    // level, an ordinal range and a count — and no digest — so two builds that resolved a member
    // to *different entities* would agree on every field above while their packed memberships
    // differed. The entity is what the Roaring blob holds, and this is the only place it is
    // compared.
    assert!(!a.membership_extents.is_empty());
    for extent in &a.membership_extents {
        let one = std::fs::read(streamed.join("v00000").join(&extent.path)).unwrap();
        let other = std::fs::read(linear.join("v00000").join(&extent.path)).unwrap();
        assert_eq!(one, other, "the packed memberships of {} differ", extent.layer);
    }
    assert_eq!(a.artifact_record_extents, b.artifact_record_extents);
    for extent in &a.artifact_record_extents {
        for file in [&extent.blocks, &extent.hasrow, &extent.directory] {
            let one = std::fs::read(streamed.join("v00000").join(file)).unwrap();
            let other = std::fs::read(linear.join("v00000").join(file)).unwrap();
            assert_eq!(one, other, "the artifact content in {file} differs");
        }
    }
}

/// **A member the build did not assign refuses the build.** Dropping it instead would move both
/// the count a viewer is shown and the size a proportional criterion divides by, quietly, in the
/// direction of hiding the artifact.
#[test]
fn a_member_naming_nothing_this_build_assigned_refuses_it() {
    let inputs = inputs();
    write_members_rows(
        &inputs.at("clusters_members.parquet"),
        &cluster_member_rows(&[0, 1, N_ITEMS + 500]),
    );
    let out = inputs.dir.join("bundle");
    let err = run(&inputs, &out).expect_err("an unknown member is a refusal");
    let message = format!("{err}");
    assert!(message.contains(&format!("{}", N_ITEMS + 500)), "{message}");
    assert!(message.contains("refused"), "{message}");
}

/// The registry's own refusals reach the build unchanged: a layer must be declared after the
/// layers it depends on, and an artifact may only attach where its layer declared it edges.
#[test]
fn the_registrys_refusals_are_the_builds_refusals() {
    let inputs = inputs();
    let mut blocks: Vec<String> = LAYERS_TOML
        .split("[[layer]]")
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!("[[layer]]{s}"))
        .collect();
    blocks.reverse();
    let reversed = blocks.join("\n");
    std::fs::write(&inputs.config, format!("{VIEW_TOML}{reversed}")).unwrap();
    let out = inputs.dir.join("bundle");
    let err = run(&inputs, &out).expect_err("a dependency must exist before its dependent");
    assert!(format!("{err}").contains("depends_on"), "{err}");
}

/// **A key the artifacts file does not declare is a refusal, not a new artifact.** A mistyped key
/// in the members file would otherwise publish a phantom beside a real cluster whose count is
/// quietly short — and on a layer declaring no supplied content nothing downstream notices.
#[test]
fn a_member_row_naming_an_undeclared_artifact_is_refused() {
    let inputs = inputs();
    // Every member of `c-0000` except one, whose key is mistyped.
    write_members_rows(
        &inputs.at("clusters_members.parquet"),
        &[("c-0000", None, 0), ("c-OOO0", None, 1)],
    );
    let out = inputs.dir.join("bundle");
    let err = run(&inputs, &out).expect_err("an undeclared key is a refusal");
    assert!(format!("{err}").contains("c-OOO0"), "{err}");
}

/// **A null entity is not entity zero.** Arrow reads the values buffer whatever the validity
/// bitmap says, so a producer whose join missed a row would publish the corpus's lowest-numbered
/// document into the artifact, moving its masked count for whoever can see that document.
#[test]
fn a_null_member_is_refused_rather_than_read_as_entity_zero() {
    let inputs = inputs();
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("entity", DataType::UInt64, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec!["c-0000", "c-0000"])) as ArrayRef,
            Arc::new(UInt64Array::from(vec![Some(7u64), None])),
        ],
    )
    .unwrap();
    write(&inputs.at("clusters_members.parquet"), schema, batch);

    let out = inputs.dir.join("bundle");
    let err = run(&inputs, &out).expect_err("a null entity is a refusal");
    assert!(format!("{err}").contains("null entity"), "{err}");
}

/// **Publication follows the declaration order, not the alphabet.** An attachment resolves against
/// what is already published, so a label layer must be published after the layer it attaches into —
/// and the file's own order is what states that, `depends_on` having been declared in it.
#[test]
fn a_label_layer_sorting_before_its_target_still_publishes() {
    let inputs = inputs();
    // `annotations/…` sorts before `clusters/…`, which is the case an alphabetical publication
    // order would refuse with "holds no such artifact" for a target that is plainly there.
    let renamed = |text: &str| text.replace("topics/x", "annotations/topics");
    std::fs::write(&inputs.config, renamed(&format!("{VIEW_TOML}{LAYERS_TOML}"))).unwrap();

    let out = inputs.dir.join("bundle");
    run(&inputs, &out).expect("declaration order is what decides, not the layer's name");
    let manifest = manifest_of(&out);
    assert!(manifest
        .membership_extents
        .iter()
        .any(|e| e.layer == "annotations/topics"));
}

/// **The existence criterion cannot be omitted**, on the gate's argument: the value an absent line
/// would supply is the widest one — no criterion at all, serving the existence and masked count of
/// every artifact down to a single member. The control plane's JSON demands the field too, so a
/// declaration moved from one route to the other cannot lose a disclosure control on the way.
#[test]
fn a_layer_omitting_its_existence_criterion_is_refused() {
    let inputs = inputs();
    let mut without = String::new();
    for line in LAYERS_TOML.lines() {
        if !line.starts_with("require_member_visibility") {
            without.push_str(line);
            without.push('\n');
        }
    }
    std::fs::write(&inputs.config, format!("{VIEW_TOML}{without}")).unwrap();
    let out = inputs.dir.join("bundle");
    let err = run(&inputs, &out).expect_err("an absent criterion is a refusal");
    assert!(
        format!("{err}").contains("require_member_visibility"),
        "{err}"
    );
}

/// A layer in a view this build does not write would be registered, reachable and empty — which
/// no client can tell from a layer whose artifacts were all withheld. Refused.
#[test]
fn a_layer_naming_a_view_this_build_does_not_write_is_refused() {
    let inputs = inputs();
    std::fs::write(
        &inputs.config,
        format!(
            "{VIEW_TOML}{}",
            LAYERS_TOML.replace(r#"views = ["s0"]"#, r#"views = ["s7"]"#)
        ),
    )
    .unwrap();
    let out = inputs.dir.join("bundle");
    let err = run(&inputs, &out).expect_err("a view that does not exist is a refusal");
    assert!(format!("{err}").contains("s7"), "{err}");
}

/// Artifacts without a layer file are a refusal rather than a build that quietly drops them: a
/// bundle whose clusters are absent cannot be told from one whose clusters failed their criterion.
#[test]
fn artifacts_without_a_layer_file_are_refused() {
    let inputs = inputs();
    let config = tessera_build::config::Config::parse(&inputs.config, &Default::default()).unwrap();
    let out = inputs.dir.join("bundle");
    let mut args = args(&inputs, &out);
    args.layer_inputs = config.layer_sources;
    let err = build(&args).expect_err("artifacts need layers");
    assert!(format!("{err}").contains("no `[[layer]]` block"), "{err}");
}

// ---------------------------------------------------------------------------------------------
// Hierarchies: the parent/child edges a treed layer publishes, and what the build checks of them
// ---------------------------------------------------------------------------------------------

/// A treed layer, which declares no levels and sits at level 0 on one reserved run — the shape
/// [decision 0082](../../../docs/decisions/0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md)
/// gives a hierarchy. Its lineage is entirely in its edges.
const TREED_LAYERS_TOML: &str = r#"
[[layer]]
name = "clusters/tree"
title = "a hierarchy"
views = ["s0"]
source = "tree.parquet"
membership = "enumerated"
visibility = "0"
artifact_visibility = { default = "inherited" }
require_member_visibility = { count = 2 }
hierarchy = { kind = "nested", prune_children = false }
content = { computed = ["centroid"] }

  [layer.members]
  source = "tree_members.parquet"
"#;

/// A three-node tree: one root and two children, written with `parent` on each child.
///
/// The parent direction is the only one written, and the only one there is: the child direction is
/// derived by inverting these edges, so there is no second column for it to disagree with.
fn write_treed_artifacts(path: &Path, child_parent: &[(&str, Option<&str>)]) {
    write_edged_artifacts(
        path,
        &child_parent
            .iter()
            .map(|(k, p)| (0u32, *k, *p))
            .collect::<Vec<_>>(),
    )
}

/// The same with an explicit level per artifact, for a layer whose edges run between levels.
fn write_edged_artifacts(path: &Path, rows: &[(u32, &str, Option<&str>)]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("level", DataType::UInt32, true),
        Field::new("key", DataType::Utf8, false),
        Field::new("parent", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt32Array::from(
                rows.iter().map(|(l, ..)| Some(*l)).collect::<Vec<_>>(),
            )) as ArrayRef,
            Arc::new(StringArray::from(
                rows.iter().map(|(_, k, _)| *k).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|(.., p)| *p).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

/// Memberships for a treed level, one row per `(artifact, entity)`.
fn write_treed_members(path: &Path, membership: &[(&str, Vec<u64>)]) {
    let rows: Vec<(&str, Option<u32>, u64)> = membership
        .iter()
        .flat_map(|(key, members)| members.iter().map(move |&m| (*key, None, m)))
        .collect();
    write_members_rows(path, &rows);
}

/// Build a treed fixture and return the containment report the build wrote.
fn treed_build(
    child_parent: &[(&str, Option<&str>)],
    membership: &[(&str, Vec<u64>)],
) -> (tessera_build::error::Result<()>, PathBuf, tempfile::TempDir) {
    let inputs = inputs();
    std::fs::write(&inputs.config, format!("{VIEW_TOML}{TREED_LAYERS_TOML}")).unwrap();
    write_treed_artifacts(&inputs.at("tree.parquet"), child_parent);
    write_treed_members(&inputs.at("tree_members.parquet"), membership);
    let out = inputs.dir.join("bundle");
    let result = run(&inputs, &out).map(|_| ());
    (result, out, inputs._tmp)
}

fn containment_report(root: &Path) -> serde_json::Value {
    let bytes = std::fs::read(root.join("reports").join("containment.json"))
        .expect("every build writes a containment report, empty or not");
    serde_json::from_slice(&bytes).unwrap()
}

/// **The headline for hierarchies: a tree whose children sit inside their parents builds clean**,
/// and the report says so rather than saying nothing.
///
/// The children deliberately do **not** exhaust the root — entities 20–29 are the root's alone.
/// That is the real condensed tree's defining property (20–25% of a parent's points fall out as
/// noise at each split), and a build that treated stray members as a violation would report every
/// real hierarchy it was ever given.
#[test]
fn a_tree_whose_children_sit_inside_their_parents_reports_nothing() {
    let (result, out, _tmp) = treed_build(
        &[("t-root", None), ("t-a", Some("t-root")), ("t-b", Some("t-root"))],
        &[
            ("t-root", (0..30).collect()),
            ("t-a", (0..10).collect()),
            ("t-b", (10..20).collect()),
        ],
    );
    result.expect("a well-formed hierarchy builds");
    assert_eq!(
        containment_report(&out)["violations"].as_array().unwrap().len(),
        0,
        "stray members in the parent are the normal case, not a violation"
    );
}

/// **A child holding a member its parent does not is named, and published anyway.**
///
/// Containment is what makes rollup sound under an absolute criterion — a child's masked count can
/// never exceed its parent's — so an edge that breaks it withdraws that guarantee for its branch.
/// The corpus may legitimately be that way, so the build reports the edge rather than refusing it,
/// and an operator sees it before a viewer meets its consequences.
#[test]
fn a_child_escaping_its_parent_is_reported_by_name() {
    let (result, out, _tmp) = treed_build(
        &[("t-root", None), ("t-a", Some("t-root"))],
        &[
            ("t-root", (0..10).collect()),
            // 10 and 11 are the child's and not the root's — the escape.
            ("t-a", (5..12).collect()),
        ],
    );
    result.expect("an uncontained edge is a report, not a refusal");
    let report = containment_report(&out);
    let violations = report["violations"].as_array().unwrap();
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0]["child"], "t-a");
    assert_eq!(violations[0]["parent"], "t-root");
    assert_eq!(violations[0]["escaping_members"], 2);
}

/// **A split that loses members is named in advance, and named as normal rather than as a fault.**
///
/// The stray is what makes a parent visible *alone*, with none of its children, to a principal who
/// can see those members and nothing else. That is the right answer and a surprising one, so an
/// operator should meet it in this report rather than in a support question about a cluster that
/// has no children on the map.
#[test]
fn a_split_that_loses_members_is_named_with_its_stray_count() {
    let (result, out, _tmp) = treed_build(
        &[("t-root", None), ("t-a", Some("t-root")), ("t-b", Some("t-root"))],
        &[
            ("t-root", (0..30).collect()),
            ("t-a", (0..10).collect()),
            ("t-b", (10..20).collect()),
        ],
    );
    result.expect("a non-covering split is not a fault");
    let report = containment_report(&out);
    assert_eq!(report["violations"].as_array().unwrap().len(), 0);

    let splits = &report["splits"];
    assert_eq!(splits["total"], 1, "one internal node");
    assert_eq!(splits["non_covering"], 1);
    assert_eq!(splits["not_listed"], 0, "a small tree lists every split");
    let row = &splits["by_stray_members"][0];
    assert_eq!(row["parent"], "t-root");
    assert_eq!(row["children"], 2);
    assert_eq!(row["members"], 30);
    // 20..30 belong to the root and to neither child.
    assert_eq!(row["stray_members"], 10);
}

/// A split whose children exhaust it reports zero stray — the covering case, which is what a
/// planted tree produces and a real clustering almost never does.
#[test]
fn a_covering_split_reports_no_stray() {
    let (result, out, _tmp) = treed_build(
        &[("t-root", None), ("t-a", Some("t-root")), ("t-b", Some("t-root"))],
        &[
            ("t-root", (0..20).collect()),
            ("t-a", (0..10).collect()),
            ("t-b", (10..20).collect()),
        ],
    );
    result.expect("a covering split builds");
    let splits = &containment_report(&out)["splits"];
    assert_eq!(splits["non_covering"], 0);
    assert_eq!(splits["by_stray_members"][0]["stray_members"], 0);
}

/// A parent key naming an artifact the level does not declare describes no tree at all, so there
/// is nothing to publish — a refusal, unlike an uncontained edge.
#[test]
fn a_parent_key_with_no_artifact_behind_it_is_refused() {
    let (result, _out, _tmp) = treed_build(
        &[("t-a", Some("t-nobody"))],
        &[("t-a", (0..10).collect())],
    );
    let err = result.expect_err("a parent that does not exist is a refusal");
    assert!(format!("{err}").contains("t-nobody"), "{err}");
}

/// Edges holding a cycle have no root to descend a cut from, so they are refused rather than
/// published as a tree the serving path would walk forever.
#[test]
fn edges_holding_a_cycle_are_refused() {
    let (result, _out, _tmp) = treed_build(
        &[("t-a", Some("t-b")), ("t-b", Some("t-a"))],
        &[("t-a", (0..10).collect()), ("t-b", (0..10).collect())],
    );
    let err = result.expect_err("a cycle is a refusal");
    assert!(format!("{err}").contains("cycle"), "{err}");
}

// ---------------------------------------------------------------------------------------------
// The tiered shape: levels, and edges that run between them
// ---------------------------------------------------------------------------------------------

/// Countries, states, counties — one layer, three levels, containment edges between them.
///
/// **Its edges are information, not roll-up.** The levels carry the resolution: a client picks
/// "states" rather than asking the server to coarsen for it, and the cut never climbs these edges.
/// What they are for is telling a client what contains what.
const TIERED_LAYERS_TOML: &str = r#"
[[layer]]
name = "admin/boundaries"
title = "administrative boundaries"
views = ["s0"]
source = "admin.parquet"
membership = "enumerated"
visibility = "0"
artifact_visibility = { default = "inherited" }
require_member_visibility = { count = 1 }
hierarchy = { kind = "tiered", prune_children = false }
content = { computed = ["centroid"] }

  [layer.members]
  source = "admin_members.parquet"

[[layer.levels]]
level = 0
title = "countries"

[[layer.levels]]
level = 1
title = "states"

[[layer.levels]]
level = 2
title = "counties"
"#;

fn tiered_build(
    rows: &[(u32, &str, Option<&str>)],
    membership: &[(u32, &str, Vec<u64>)],
) -> (tessera_build::error::Result<()>, PathBuf, tempfile::TempDir) {
    let inputs = inputs();
    std::fs::write(&inputs.config, format!("{VIEW_TOML}{TIERED_LAYERS_TOML}")).unwrap();
    write_edged_artifacts(&inputs.at("admin.parquet"), rows);
    write_levelled_members(&inputs.at("admin_members.parquet"), membership);
    let out = inputs.dir.join("bundle");
    let result = run(&inputs, &out).map(|_| ());
    (result, out, inputs._tmp)
}

fn write_levelled_members(path: &Path, membership: &[(u32, &str, Vec<u64>)]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("level", DataType::UInt32, true),
        Field::new("key", DataType::Utf8, false),
        Field::new("entity", DataType::UInt64, false),
    ]));
    let (mut levels, mut keys, mut entity) = (vec![], vec![], vec![]);
    for (level, key, members) in membership {
        for &m in members {
            levels.push(Some(*level));
            keys.push(key.to_string());
            entity.push(m);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt32Array::from(levels)) as ArrayRef,
            Arc::new(StringArray::from(keys)),
            Arc::new(UInt64Array::from(entity)),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

/// **The headline for the administrative shape: it builds, and its containment is checked across
/// levels exactly as a tree's is within one.**
#[test]
fn a_tiered_layer_publishes_edges_between_its_levels() {
    let (result, out, _tmp) = tiered_build(
        &[
            (0, "country", None),
            (1, "state-a", Some("country")),
            (1, "state-b", Some("country")),
            (2, "county-a1", Some("state-a")),
        ],
        &[
            (0, "country", (0..40).collect()),
            (1, "state-a", (0..20).collect()),
            (1, "state-b", (20..30).collect()),
            (2, "county-a1", (0..10).collect()),
        ],
    );
    result.expect("a tiered layer builds");

    let report = containment_report(&out);
    assert_eq!(report["violations"].as_array().unwrap().len(), 0);
    let splits = &report["splits"];
    // The country (two states) and state-a (one county) are both internal.
    assert_eq!(splits["total"], 2);
    // The country keeps 30..40 away from both its states, so its split is non-covering.
    let country = splits["by_stray_members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["parent"] == "country")
        .expect("the country's split is reported");
    assert_eq!(country["members"], 40);
    assert_eq!(country["stray_members"], 10);
}

/// **An edge running against the resolution is refused rather than reinterpreted.** An
/// administrative layer's whole guarantee is that lineage never runs from a finer level to a
/// coarser one; accepting one would make "a level is a scale" untrue without anything saying so.
#[test]
fn a_tiered_edge_within_one_level_is_refused() {
    let (result, _out, _tmp) = tiered_build(
        &[(1, "state-a", None), (1, "state-b", Some("state-a"))],
        &[
            (1, "state-a", (0..10).collect()),
            (1, "state-b", (10..20).collect()),
        ],
    );
    let err = result.expect_err("a same-level parent is not a tiered edge");
    assert!(format!("{err}").contains("coarser"), "{err}");
}

/// A layer that declares no lineage may carry no edges — refused rather than silently ignored,
/// since an ignored edge is a hierarchy the caller believes they published and nobody has.
#[test]
fn edges_on_a_layer_declaring_no_lineage_are_refused() {
    let inputs = inputs();
    std::fs::write(
        &inputs.config,
        format!(
            "{VIEW_TOML}{}",
            TIERED_LAYERS_TOML.replace(r#"kind = "tiered""#, r#"kind = "stacked""#)
        ),
    )
    .unwrap();
    write_edged_artifacts(
        &inputs.at("admin.parquet"),
        &[(0, "country", None), (1, "state-a", Some("country"))],
    );
    write_levelled_members(
        &inputs.at("admin_members.parquet"),
        &[(0, "country", (0..20).collect()), (1, "state-a", (0..10).collect())],
    );
    let out = inputs.dir.join("bundle");
    let err = run(&inputs, &out).expect_err("a stacked layer has no lineage");
    assert!(format!("{err}").contains("no lineage"), "{err}");
}

// ---------------------------------------------------------------------------------------------
// The spellings: three ways to say one membership, and one bundle out of each
// ---------------------------------------------------------------------------------------------

/// One flat layer whose artifacts are written a different way in each case below. Everything a
/// bundle is built from is held constant apart from the spelling, so the comparison is exactly the
/// property being asserted.
const CURATED_LAYER: &str = r#"
[[layer]]
name = "curated/a"
title = "curated"
views = ["s0"]
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = { count = 1 }
hierarchy = { kind = "flat" }
"#;

/// The two artifacts every spelling below describes.
const CURATED: [(&str, &[u64]); 2] = [("c-0", &[0, 1, 2]), ("c-1", &[3, 4])];

/// Build a bundle from `layers` — the `[[layer]]` block — with `write` laying down whatever files
/// it names. Returns the bundle root, and the tempdir that must outlive it.
fn build_spelling(
    layers: &str,
    write_sources: impl FnOnce(&Inputs),
) -> (PathBuf, tempfile::TempDir) {
    let (out, tmp, _) = build_spelling_reported(layers, write_sources);
    (out, tmp)
}

/// The same, keeping the build's own report — what it counted as well as what it wrote.
fn build_spelling_reported(
    layers: &str,
    write_sources: impl FnOnce(&Inputs),
) -> (PathBuf, tempfile::TempDir, tessera_build::BuildReport) {
    let inputs = inputs();
    std::fs::write(&inputs.config, format!("{VIEW_TOML}{layers}")).unwrap();
    write_sources(&inputs);
    let out = inputs.dir.join("spelled");
    let report = run(&inputs, &out).expect("the spelling builds");
    (out, inputs._tmp, report)
}

/// The same, for a declaration expected to be refused.
fn refuse_spelling(layers: &str, write_sources: impl FnOnce(&Inputs)) -> String {
    let inputs = inputs();
    std::fs::write(&inputs.config, format!("{VIEW_TOML}{layers}")).unwrap();
    write_sources(&inputs);
    let out = inputs.dir.join("spelled");
    format!("{}", run(&inputs, &out).expect_err("the spelling is refused"))
}

/// Every file of two bundles, compared byte for byte — `MANIFEST.json` with its wall-clock
/// `created_at` blanked, and `CURRENT`, which is nothing but that manifest's digest, skipped with
/// it. Every digest the manifest records for every other file is compared verbatim, so a
/// membership that differed by one entity fails here.
fn assert_bundles_identical(left: &Path, right: &Path, what: &str) {
    fn collect(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        fn walk(
            root: &Path,
            dir: &Path,
            out: &mut std::collections::BTreeMap<String, Vec<u8>>,
        ) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.insert(rel, std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(root, root, &mut out);
        out
    }
    let (a, b) = (collect(left), collect(right));
    assert_eq!(
        a.keys().collect::<Vec<_>>(),
        b.keys().collect::<Vec<_>>(),
        "{what}: the two bundles do not contain the same files"
    );
    for (name, left_bytes) in &a {
        let right_bytes = &b[name];
        if name.ends_with("MANIFEST.json") {
            let normalise = |bytes: &[u8]| {
                let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                value["created_at"] = serde_json::Value::Null;
                value
            };
            assert_eq!(
                normalise(left_bytes),
                normalise(right_bytes),
                "{what}: MANIFEST.json differs (ignoring created_at)"
            );
            continue;
        }
        if name == "CURRENT" {
            continue;
        }
        assert_eq!(left_bytes, right_bytes, "{what}: {name} is not byte-identical");
    }
    assert!(a.len() > 6, "{what}: expected a full bundle, found {}", a.len());
}

/// The artifact row carrying its own membership as a list.
fn write_curated_with_members(path: &Path, artifacts: &[(&str, Vec<u64>)]) {
    write_curated_column(path, "members", artifacts)
}

fn write_curated_column(path: &Path, column: &str, artifacts: &[(&str, Vec<u64>)]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new(
            column,
            DataType::List(Arc::new(Field::new("item", DataType::UInt64, true))),
            true,
        ),
        ]));
    let mut lists = ListBuilder::new(arrow::array::UInt64Builder::new());
    for (_, members) in artifacts {
        for &m in members {
            lists.values().append_value(m);
        }
        lists.append(true);
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(
                artifacts.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            )) as ArrayRef,
            Arc::new(lists.finish()),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

fn curated() -> Vec<(&'static str, Vec<u64>)> {
    CURATED
        .iter()
        .map(|(key, members)| (*key, members.to_vec()))
        .collect()
}

/// **A layer written out in the declaration and the same layer read from a file build the same
/// bundle, byte for byte.** Inline exists so a handful of curated sets need no Parquet file — it
/// is a spelling, and a spelling that produced a different bundle would be a second kind of layer.
#[test]
fn an_inline_layer_and_a_sourced_layer_build_the_same_bundle() {
    let sourced = format!("{CURATED_LAYER}source = \"curated.parquet\"\n");
    let (from_file, _a) = build_spelling(&sourced, |inputs| {
        write_curated_with_members(&inputs.at("curated.parquet"), &curated())
    });
    let inline = format!(
        "{CURATED_LAYER}artifacts = [{}]\n",
        CURATED
            .iter()
            .map(|(key, members)| format!(
                "{{ key = \"{key}\", members = {members:?} }}"
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let (from_declaration, _b) = build_spelling(&inline, |_| {});
    assert_bundles_identical(&from_file, &from_declaration, "inline against sourced");
}

/// **A membership named by exclusion and the same membership named by inclusion build the same
/// bundle, byte for byte.** The complement happens once, at the build, against the corpus — so
/// nothing downstream carries the spelling, and no request-time complement is expressible: one
/// evaluated against a viewer's mask would disclose the existence of items outside it.
#[test]
fn an_excluded_membership_and_its_complement_build_the_same_bundle() {
    let excluded: Vec<u64> = vec![5, 7, 9];
    let included: Vec<u64> = (0..N_ITEMS).filter(|e| !excluded.contains(e)).collect();

    let by_inclusion = format!("{CURATED_LAYER}source = \"curated.parquet\"\n");
    let (from_members, _a) = build_spelling(&by_inclusion, |inputs| {
        write_curated_with_members(&inputs.at("curated.parquet"), &[("c-0", included)])
    });
    let by_exclusion = format!("{CURATED_LAYER}source = \"curated.parquet\"\n");
    let (from_excluding, _b) = build_spelling(&by_exclusion, |inputs| {
        write_curated_column(
            &inputs.at("curated.parquet"),
            "excluding",
            &[("c-0", excluded)],
        )
    });
    assert_bundles_identical(&from_members, &from_excluding, "excluding against members");
}

/// **A membership on the artifact row and the same membership in a `[layer.members]` source build
/// the same bundle, byte for byte.** The second exists for a membership no single cell should hold
/// — a condensed tree's root — and not to mean anything different.
#[test]
fn a_row_membership_and_a_member_source_build_the_same_bundle() {
    let on_the_row = format!("{CURATED_LAYER}source = \"curated.parquet\"\n");
    let (from_row, _a) = build_spelling(&on_the_row, |inputs| {
        write_curated_with_members(&inputs.at("curated.parquet"), &curated())
    });
    let in_a_source = format!(
        "{CURATED_LAYER}source = \"curated.parquet\"\n  [layer.members]\n  source = \"curated_members.parquet\"\n"
    );
    let (from_source, _b) = build_spelling(&in_a_source, |inputs| {
        let schema = Arc::new(Schema::new(vec![Field::new("key", DataType::Utf8, false)]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(StringArray::from(
                CURATED.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            )) as ArrayRef],
        )
        .unwrap();
        write(&inputs.at("curated.parquet"), schema, batch);
        let rows: Vec<(&str, Option<u32>, u64)> = CURATED
            .iter()
            .flat_map(|(key, members)| members.iter().map(move |&m| (*key, None, m)))
            .collect();
        write_members_rows(&inputs.at("curated_members.parquet"), &rows);
    });
    assert_bundles_identical(&from_row, &from_source, "a member source against a row");
}

/// **An excluded id this build did not assign refuses the build**, where an unknown *member*
/// refuses it for the mirror-image reason: an exclusion that resolves to nothing silently widens
/// the membership by the item it was written to keep out.
#[test]
fn an_exclusion_naming_nothing_this_build_assigned_refuses_it() {
    let inputs = inputs();
    std::fs::write(
        &inputs.config,
        format!("{VIEW_TOML}{CURATED_LAYER}source = \"curated.parquet\"\n"),
    )
    .unwrap();
    write_curated_column(
        &inputs.at("curated.parquet"),
        "excluding",
        &[("c-0", vec![1, N_ITEMS + 500])],
    );
    let out = inputs.dir.join("bundle");
    let err = run(&inputs, &out).expect_err("an unknown exclusion is a refusal");
    let message = format!("{err}");
    assert!(message.contains(&format!("{}", N_ITEMS + 500)), "{message}");
    assert!(message.contains("exclusion"), "{message}");
}

/// **A file carrying both a `members` and an `excluding` column has two memberships for one
/// artifact**, and every masked count divides by one of them. Refused, as the declaration naming
/// both is.
#[test]
fn a_source_carrying_both_members_and_excluding_is_refused() {
    let inputs = inputs();
    std::fs::write(
        &inputs.config,
        format!("{VIEW_TOML}{CURATED_LAYER}source = \"curated.parquet\"\n"),
    )
    .unwrap();
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new(
            "members",
            DataType::List(Arc::new(Field::new("item", DataType::UInt64, true))),
            true,
        ),
        Field::new(
            "excluding",
            DataType::List(Arc::new(Field::new("item", DataType::UInt64, true))),
            true,
        ),
    ]));
    let mut members = ListBuilder::new(arrow::array::UInt64Builder::new());
    members.values().append_value(0);
    members.append(true);
    let mut excluding = ListBuilder::new(arrow::array::UInt64Builder::new());
    excluding.values().append_value(1);
    excluding.append(true);
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec!["c-0"])) as ArrayRef,
            Arc::new(members.finish()),
            Arc::new(excluding.finish()),
        ],
    )
    .unwrap();
    write(&inputs.at("curated.parquet"), schema, batch);
    let out = inputs.dir.join("bundle");
    let err = run(&inputs, &out).expect_err("two memberships is a refusal");
    assert!(format!("{err}").contains("two spellings"), "{err}");
}

/// **One row is one artifact.** The `(artifact, rank)` grain needed a cross-row agreement check
/// because a key spanned several rows; one row per artifact removes the disagreement rather than
/// detecting it — and what it leaves, a key written twice, is two artifacts under one name.
#[test]
fn an_artifact_written_on_two_rows_is_refused() {
    let inputs = inputs();
    std::fs::write(
        &inputs.config,
        format!("{VIEW_TOML}{CURATED_LAYER}source = \"curated.parquet\"\n"),
    )
    .unwrap();
    write_curated_with_members(
        &inputs.at("curated.parquet"),
        &[("c-0", vec![0, 1]), ("c-0", vec![2])],
    );
    let out = inputs.dir.join("bundle");
    let err = run(&inputs, &out).expect_err("one key is one artifact");
    let message = format!("{err}");
    assert!(message.contains("more than one row"), "{message}");
    assert!(message.contains("c-0"), "{message}");
}

// -------------------------------------------------------------------------------------------
// `[layer.labels]` — the sugar, and the bundle that proves it is sugar
// -------------------------------------------------------------------------------------------

/// The clustering, with its labels declared where they are used. The label layer declares no
/// `visibility`, so it takes the clustering's own gate — which is what the written-out form below
/// spells out.
const SUGARED_TOML: &str = r#"
[[layer]]
name = "clusters/a"
title = "clusters"
views = ["s0"]
source = "clusters.parquet"
membership = "enumerated"
visibility = "0"
artifact_visibility = { default = "inherited" }
require_member_visibility = { count = 2 }
hierarchy = { kind = "flat", prune_children = false }
content = { computed = ["centroid"] }

  [layer.members]
  source = "clusters_members.parquet"

  [layer.labels]
  name = "topics/x"
  title = "topics"
  source = "topics.parquet"
  type = "text"
  membership = "enumerated"
  require_member_visibility = "none"
  artifact_visibility = { default = "inherited" }

    [layer.labels.content]
    require_member_visibility = "all"

    [layer.labels.members]
    source = "topics_members.parquet"
"#;

/// The same thing, written out: every key the expansion supplies, spelled by hand.
const WRITTEN_OUT_TOML: &str = r#"
[[layer]]
name = "clusters/a"
title = "clusters"
views = ["s0"]
source = "clusters.parquet"
membership = "enumerated"
visibility = "0"
artifact_visibility = { default = "inherited" }
require_member_visibility = { count = 2 }
hierarchy = { kind = "flat", prune_children = false }
content = { computed = ["centroid"] }

  [layer.members]
  source = "clusters_members.parquet"

[[layer]]
name = "topics/x"
title = "topics"
views = ["s0"]
source = "topics.parquet"
membership = "enumerated"
visibility = "0"
artifact_visibility = { default = "inherited" }
require_member_visibility = "none"
hierarchy = { kind = "flat", prune_children = false }
depends_on = ["clusters/a"]

  [layer.members]
  source = "topics_members.parquet"

  [[layer.content.supplied]]
  name = "topics/x"
  type = "text"
  require_member_visibility = "all"
"#;

/// Build `text` as this fixture's declaration, into `out`.
fn run_config(inputs: &Inputs, text: &str, out: &Path) -> tessera_build::BuildReport {
    let path = inputs.dir.join(format!(
        "{}.toml",
        out.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&path, format!("{VIEW_TOML}{text}")).unwrap();
    let config = tessera_build::config::Config::parse(&path, &Default::default())
        .expect("the declaration parses");
    let mut args = args(inputs, out);
    args.layers = config.layers;
    args.layer_inputs = config.layer_sources;
    args.schema = config.schema;
    build(&args).expect("the build runs")
}

/// Every file under `root`, keyed by its `root`-relative slash-separated path.
fn collect(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// **The definition of sugar, asserted where it counts.** `[layer.labels]` and the second
/// `[[layer]]` it expands to produce the *same bundle, byte for byte* — so the sugar is a spelling
/// and never a second kind of layer, and nothing downstream of the parser has a label layer to
/// treat differently (`annotation-write-cycle.md` §6.1).
///
/// `MANIFEST.json` carries a wall-clock `created_at` and `CURRENT` is that manifest's digest, so
/// those two are compared with the timestamp blanked. Every other file, including every digest the
/// manifest records, is compared verbatim.
#[test]
fn the_label_sugar_and_the_layer_written_out_build_the_same_bundle() {
    let inputs = inputs();
    let sugared = inputs.dir.join("sugared");
    let written = inputs.dir.join("written");
    run_config(&inputs, SUGARED_TOML, &sugared);
    run_config(&inputs, WRITTEN_OUT_TOML, &written);

    let a = collect(&sugared);
    let b = collect(&written);
    assert_eq!(
        a.keys().collect::<Vec<_>>(),
        b.keys().collect::<Vec<_>>(),
        "the two bundles do not contain the same files"
    );
    assert!(
        a.len() > 6,
        "expected a full bundle, found {} files",
        a.len()
    );
    let mut compared = 0;
    for (name, left) in &a {
        let right = &b[name];
        if name == "v00000/MANIFEST.json" {
            let normalise = |bytes: &[u8]| {
                let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                value["created_at"] = serde_json::Value::Null;
                value
            };
            assert_eq!(
                normalise(left),
                normalise(right),
                "MANIFEST.json differs (ignoring created_at)"
            );
            compared += 1;
            continue;
        }
        if name == "CURRENT" {
            continue; // nothing but the digest of a manifest differing in created_at alone
        }
        assert_eq!(left, right, "{name} is not byte-identical");
        compared += 1;
    }
    assert!(compared > 6, "only {compared} files were compared");
    // And the layer really is in there: a vacuous pass over two bundles with no label layer would
    // otherwise assert nothing at all.
    let manifest = manifest_of(&sugared);
    assert!(
        manifest
            .layers
            .iter()
            .any(|l| l.declaration.name == "topics/x"),
        "the sugared bundle carries no label layer: {:?}",
        manifest
            .layers
            .iter()
            .map(|l| &l.declaration.name)
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------------------------
// Membership from a point table (`artifacts-from-points.md` §2, §3)
// ---------------------------------------------------------------------------------------------

/// The points file every case below is built from, carrying a cluster column beside the geometry.
///
/// **The same file the build reads its geometry from**, which is the whole of §2's claim: a point
/// table with a cluster column already *is* one row per `(artifact, entity)`, so the geometry and
/// the membership come out of one file and no producer has to write a second.
///
/// `clusters[e]` is entity `e`'s cluster, `None` a null cell. Written as text or as `int64` — the
/// two spellings of one key.
fn write_clustered_points(path: &Path, clusters: &[Option<i64>], as_text: bool) {
    let column: ArrayRef = if as_text {
        Arc::new(StringArray::from(
            clusters
                .iter()
                .map(|c| c.map(|c| c.to_string()))
                .collect::<Vec<_>>(),
        ))
    } else {
        Arc::new(arrow::array::Int64Array::from(clusters.to_vec()))
    };
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new(
            "cluster_id",
            if as_text {
                DataType::Utf8
            } else {
                DataType::Int64
            },
            true,
        ),
    ]));
    let ids: Vec<u64> = (0..N_ITEMS).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)) as ArrayRef,
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            column,
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

/// The roster: one row per cluster, keyed by the cluster id's decimal spelling.
fn write_cluster_roster(path: &Path, keys: &[i64]) {
    let schema = Arc::new(Schema::new(vec![Field::new("key", DataType::Utf8, false)]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(
            keys.iter().map(|k| k.to_string()).collect::<Vec<_>>(),
        )) as ArrayRef],
    )
    .unwrap();
    write(path, schema, batch);
}

/// The member table the point column is being asserted equal to: one row per `(artifact, entity)`,
/// on the canonical names.
fn write_cluster_members(path: &Path, clusters: &[Option<i64>]) {
    let rows: Vec<(String, u64)> = clusters
        .iter()
        .enumerate()
        .filter_map(|(entity, cluster)| {
            cluster
                .filter(|&c| c != -1)
                .map(|c| (c.to_string(), entity as u64))
        })
        .collect();
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("entity", DataType::UInt64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(
                rows.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            )) as ArrayRef,
            Arc::new(UInt64Array::from(
                rows.iter().map(|(_, e)| *e).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

/// A `[layer.members]` block reading the point table's own columns.
const FROM_POINTS: &str = r#"
  [layer.members]
  source = "points.parquet"
  fields = { key = "cluster_id", entity = "entity_id" }
"#;

const FROM_MEMBER_TABLE: &str = r#"
  [layer.members]
  source = "cluster_members.parquet"
"#;

/// Every point clustered, so the case turns on nothing but where the membership was read from.
fn every_point_clustered() -> Vec<Option<i64>> {
    (0..N_ITEMS).map(|e| Some((e % 3) as i64)).collect()
}

/// **§2: membership from a point table needs no new surface.** `[layer.members]` already means one
/// row per `(artifact, entity)`, and a point table with a cluster column is that shape — so the
/// same layer read from the points and read from a member table build the same bundle, byte for
/// byte.
#[test]
fn a_cluster_column_on_the_points_is_a_member_source() {
    let clusters = every_point_clustered();
    let layer = format!("{CURATED_LAYER}source = \"roster.parquet\"\n{FROM_POINTS}");
    let (from_points, _a) = build_spelling(&layer, |inputs| {
        write_clustered_points(&inputs.points, &clusters, true);
        write_cluster_roster(&inputs.at("roster.parquet"), &[0, 1, 2]);
    });
    let layer = format!("{CURATED_LAYER}source = \"roster.parquet\"\n{FROM_MEMBER_TABLE}");
    let (from_table, _b) = build_spelling(&layer, |inputs| {
        write_clustered_points(&inputs.points, &clusters, true);
        write_cluster_roster(&inputs.at("roster.parquet"), &[0, 1, 2]);
        write_cluster_members(&inputs.at("cluster_members.parquet"), &clusters);
    });
    assert_bundles_identical(&from_points, &from_table, "a point column against a member table");
}

/// **§2's two reader changes, against the member table they must agree with.** Cluster ids are
/// integers, so the key column is one — canonicalised to its decimal spelling, and converted once
/// per artifact rather than once per point. A null cell, and exactly `-1`, mean the point is in no
/// artifact: a condensed tree drops a fifth to a quarter of its points as noise at each split, so
/// refusing them would fail the build on every clusterer's ordinary output.
#[test]
fn an_integer_cluster_column_skips_its_noise_and_matches_a_member_table() {
    let mut clusters = every_point_clustered();
    for point in clusters.iter_mut().take(225).skip(200) {
        *point = Some(-1);
    }
    for point in clusters.iter_mut().skip(225) {
        *point = None;
    }

    let layer = format!("{CURATED_LAYER}source = \"roster.parquet\"\n{FROM_POINTS}");
    let (from_points, _a, report) = build_spelling_reported(&layer, |inputs| {
        write_clustered_points(&inputs.points, &clusters, false);
        write_cluster_roster(&inputs.at("roster.parquet"), &[0, 1, 2]);
    });
    // Counted and reported, never silent: a clustering that skipped *every* row named the wrong
    // column, and only the number says so.
    assert_eq!(report.unclustered_member_rows, 50);

    let layer = format!("{CURATED_LAYER}source = \"roster.parquet\"\n{FROM_MEMBER_TABLE}");
    let (from_table, _b) = build_spelling(&layer, |inputs| {
        write_clustered_points(&inputs.points, &clusters, false);
        write_cluster_roster(&inputs.at("roster.parquet"), &[0, 1, 2]);
        write_cluster_members(&inputs.at("cluster_members.parquet"), &clusters);
    });
    assert_bundles_identical(&from_points, &from_table, "an integer column against a member table");
}

/// **`3` and `"3"` name one artifact.** The roster is text and the points are integers, which is
/// the ordinary case — a producer writes cluster names and a clusterer writes cluster ids — so the
/// two spellings must resolve to one address rather than to an artifact each.
#[test]
fn an_integer_key_and_its_decimal_spelling_are_one_artifact() {
    let clusters = every_point_clustered();
    let layer = format!("{CURATED_LAYER}source = \"roster.parquet\"\n{FROM_POINTS}");
    let (from_integers, _a) = build_spelling(&layer, |inputs| {
        write_clustered_points(&inputs.points, &clusters, false);
        write_cluster_roster(&inputs.at("roster.parquet"), &[0, 1, 2]);
    });
    let (from_text, _b) = build_spelling(&layer, |inputs| {
        write_clustered_points(&inputs.points, &clusters, true);
        write_cluster_roster(&inputs.at("roster.parquet"), &[0, 1, 2]);
    });
    assert_bundles_identical(&from_integers, &from_text, "an integer key against its spelling");
}

/// **A key the artifacts omit is still a refusal by default.** `value_set` defaults to `closed`,
/// which is the roster rule the build has always had — the phantom-artifact refusal is not
/// something a points column quietly escapes.
#[test]
fn a_closed_layer_still_refuses_a_cluster_no_artifact_declares() {
    let clusters = every_point_clustered();
    let layer = format!("{CURATED_LAYER}source = \"roster.parquet\"\n{FROM_POINTS}");
    let message = refuse_spelling(&layer, |inputs| {
        write_clustered_points(&inputs.points, &clusters, false);
        // Cluster 2 is on the points and not on the roster.
        write_cluster_roster(&inputs.at("roster.parquet"), &[0, 1]);
    });
    assert!(message.contains("do not declare"), "{message}");
    assert!(message.contains("value_set"), "{message}");
}

/// **§3: under `value_set = "open"` a cluster exists because points say it does.** The artifacts
/// source becomes enrichment — so a cluster the points name and the table omits exists with no
/// title, a cluster the table carries and no point names is an artifact with no members, and
/// neither is an error.
#[test]
fn an_open_layer_mints_the_clusters_its_points_name() {
    let clusters = every_point_clustered();
    let layer = format!(
        "{CURATED_LAYER}value_set = \"open\"\nsource = \"roster.parquet\"\n{FROM_POINTS}"
    );
    let (out, _tmp, report) = build_spelling_reported(&layer, |inputs| {
        write_clustered_points(&inputs.points, &clusters, false);
        // The table knows about cluster 0 and about a cluster 9 that no point is in; the points
        // name 1 and 2, which it has never heard of.
        write_cluster_roster(&inputs.at("roster.parquet"), &[0, 9]);
    });
    let manifest = manifest_of(&out);
    let counts: Vec<u32> = manifest
        .membership_extents
        .iter()
        .map(|e| e.count)
        .collect();
    assert_eq!(counts, vec![4], "clusters 0, 1 and 2 from the points, and 9 from the table");
    // **What was created is reported**, because it cannot be undone: 1 and 2 are the two the table
    // never declared, and a build that minted every key would be a typo in the column rather than
    // enrichment the operator left out. The wire says the same number for an ingest batch.
    assert_eq!(report.minted_artifacts, 2);
}

/// **A bare clustering declares no artifacts at all.** The roster refusal is the closed set's
/// alone, so an open layer may name a member source and nothing else — which is the whole of what
/// a clusterer's output is.
#[test]
fn an_open_layer_needs_no_artifacts_source() {
    let clusters = every_point_clustered();
    let layer = format!("{CURATED_LAYER}value_set = \"open\"\n{FROM_POINTS}");
    let (out, _tmp, _) = build_spelling_reported(&layer, |inputs| {
        write_clustered_points(&inputs.points, &clusters, true);
    });
    let manifest = manifest_of(&out);
    assert_eq!(
        manifest
            .membership_extents
            .iter()
            .map(|e| e.count)
            .collect::<Vec<_>>(),
        vec![3]
    );
}

/// **A minted artifact and a declared one are the same artifact.** Minting is where an artifact
/// came from, not what it is — so an open layer built from the points alone publishes the same
/// memberships, on the same entities, as the roster and member table that describe the same
/// clustering. The two bundles' manifests differ by one field and only one: the `value_set` that
/// produced them.
#[test]
fn a_minted_artifact_and_a_declared_one_are_the_same_artifact() {
    let clusters = every_point_clustered();
    let minted = format!("{CURATED_LAYER}value_set = \"open\"\n{FROM_POINTS}");
    let (from_points, _a) = build_spelling(&minted, |inputs| {
        write_clustered_points(&inputs.points, &clusters, false);
    });
    let declared = format!("{CURATED_LAYER}source = \"roster.parquet\"\n{FROM_MEMBER_TABLE}");
    let (from_table, _b) = build_spelling(&declared, |inputs| {
        write_clustered_points(&inputs.points, &clusters, false);
        write_cluster_roster(&inputs.at("roster.parquet"), &[0, 1, 2]);
        write_cluster_members(&inputs.at("cluster_members.parquet"), &clusters);
    });

    let (minted, declared) = (manifest_of(&from_points), manifest_of(&from_table));
    assert_eq!(minted.membership_extents, declared.membership_extents);
    assert_eq!(minted.entity_id_low_water, declared.entity_id_low_water);
    assert_eq!(
        minted.layers.iter().map(|l| &l.runs).collect::<Vec<_>>(),
        declared.layers.iter().map(|l| &l.runs).collect::<Vec<_>>()
    );
    // **The bytes, not only the descriptors**: an extent carries no digest, so two builds that put
    // one member on a different entity would agree on every field above.
    assert!(!minted.membership_extents.is_empty());
    for extent in &minted.membership_extents {
        assert_eq!(
            std::fs::read(from_points.join("v00000").join(&extent.path)).unwrap(),
            std::fs::read(from_table.join("v00000").join(&extent.path)).unwrap(),
            "the packed memberships of {} differ",
            extent.layer
        );
    }
    assert_eq!(
        minted.layers[0].declaration.value_set,
        tessera_types::layer::ValueSet::Open
    );
    assert_eq!(
        declared.layers[0].declaration.value_set,
        tessera_types::layer::ValueSet::Closed
    );
}

// ---------------------------------------------------------------------------------------------
// A list key column: the lineage the points declare (`artifacts-from-points.md` §4)
// ---------------------------------------------------------------------------------------------

/// The points, carrying a **list** of cluster ids per point beside the geometry — what a
/// hierarchical clusterer emits.
///
/// `lists[e]` is entity `e`'s list and an entry of `None` is a null one. Written as text or as
/// `int64`, and as a variable-length list or a fixed-size one, which are the shapes §4's table
/// distinguishes: the fixed one is a levelled analysis and the variable one a lineage.
fn write_listed_points(path: &Path, lists: &[Vec<Option<i64>>], as_text: bool, fixed: Option<i32>) {
    let item = Arc::new(Field::new(
        "item",
        if as_text {
            DataType::Utf8
        } else {
            DataType::Int64
        },
        true,
    ));
    let mut offsets: Vec<i32> = vec![0];
    let mut entries: Vec<Option<i64>> = Vec::new();
    for list in lists {
        entries.extend(list.iter().copied());
        offsets.push(entries.len() as i32);
    }
    let child: ArrayRef = if as_text {
        Arc::new(StringArray::from(
            entries
                .iter()
                .map(|e| e.map(|e| e.to_string()))
                .collect::<Vec<_>>(),
        ))
    } else {
        Arc::new(Int64Array::from(entries))
    };
    let column: ArrayRef = match fixed {
        Some(size) => Arc::new(FixedSizeListArray::new(item, size, child, None)),
        None => Arc::new(ListArray::new(
            item,
            OffsetBuffer::new(offsets.into()),
            child,
            None,
        )),
    };
    let ids: Vec<u64> = (0..lists.len() as u64).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("lineage", column.data_type().clone(), true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)) as ArrayRef,
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            column,
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

/// The same layer as `CURATED_LAYER` at another hierarchy kind — the one word §4's table turns on.
fn layer_of_kind(kind: &str) -> String {
    CURATED_LAYER.replace(r#"kind = "flat""#, &format!(r#"kind = "{kind}""#))
}

/// A `[layer.members]` block reading the point table's list column.
const FROM_LINEAGE: &str = r#"
  [layer.members]
  source = "points.parquet"
  fields = { key = "lineage", entity = "entity_id" }
"#;

/// The equivalent enumeration: an artifact table carrying the edges, and a member table carrying
/// one row per `(artifact, entity)`.
const FROM_TREE_TABLES: &str = r#"source = "tree.parquet"
  [layer.members]
  source = "tree_members.parquet"
"#;

const FROM_LEVELLED_TABLES: &str = r#"source = "admin.parquet"
  [layer.members]
  source = "admin_members.parquet"
"#;

/// Three levels, which is what a fixed-length list of three entries must agree with.
const THREE_LEVELS: &str = r#"
[[layer.levels]]
level = 0

[[layer.levels]]
level = 1

[[layer.levels]]
level = 2
"#;

/// Entity `e`'s lineage: one root, three groups under it, six leaves under those.
fn lineage_of(entity: u64) -> Vec<Option<i64>> {
    vec![
        Some(1),
        Some(10 + (entity % 3) as i64),
        Some(100 + (entity % 6) as i64),
    ]
}

/// The same clustering written out: every artifact, and the parent it hangs from.
fn lineage_artifacts() -> Vec<(String, Option<String>)> {
    let mut rows = vec![("1".to_string(), None)];
    for group in 0..3u64 {
        rows.push((format!("{}", 10 + group), Some("1".to_string())));
    }
    for leaf in 0..6u64 {
        rows.push((
            format!("{}", 100 + leaf),
            Some(format!("{}", 10 + leaf % 3)),
        ));
    }
    rows
}

/// The same clustering's memberships: a point belongs to every artifact its lineage names.
fn lineage_memberships() -> Vec<(String, Vec<u64>)> {
    let mut rows: std::collections::BTreeMap<String, Vec<u64>> = Default::default();
    for entity in 0..N_ITEMS {
        for key in lineage_of(entity).into_iter().flatten() {
            rows.entry(key.to_string()).or_default().push(entity);
        }
    }
    rows.into_iter().collect()
}

/// **A flat layer takes a list, and it means plain multi-membership.** A document under three
/// topics is ordinary, and the same membership written as three rows of a member table has always
/// been legal — so refusing the list spelling would make two spellings of one membership disagree,
/// which is the property every other input spelling here is held to. No positions are read: a flat
/// layer has no levels and no lineage, so the entries are a set.
#[test]
fn a_list_on_a_flat_layer_is_multi_membership_and_matches_a_member_table() {
    // Entity e belongs to every artifact whose id divides into it — overlapping groupings, which
    // is the case a scalar column cannot express at all.
    let lists: Vec<Vec<Option<i64>>> = (0..N_ITEMS)
        .map(|e| {
            let mut of: Vec<Option<i64>> = (1..4i64).filter(|d| e as i64 % d == 0).collect::<Vec<_>>()
                .into_iter().map(Some).collect();
            if of.is_empty() {
                of.push(None);
            }
            of
        })
        .collect();

    let from_points = format!("{CURATED_LAYER}value_set = \"open\"\n{FROM_LINEAGE}");
    let (from_points, _a) = build_spelling(&from_points, |inputs| {
        write_listed_points(&inputs.points, &lists, false, None);
    });

    // The same memberships, one row per (artifact, entity).
    let mut by_key: std::collections::BTreeMap<String, Vec<u64>> = Default::default();
    for (entity, of) in lists.iter().enumerate() {
        for key in of.iter().flatten() {
            by_key.entry(key.to_string()).or_default().push(entity as u64);
        }
    }
    let from_table = format!("{CURATED_LAYER}value_set = \"open\"\n{FROM_TREE_TABLES}");
    let (from_table, _b) = build_spelling(&from_table, |inputs| {
        write_listed_points(&inputs.points, &lists, false, None);
        let keys: Vec<(&str, Option<&str>)> = by_key.keys().map(|k| (k.as_str(), None)).collect();
        write_treed_artifacts(&inputs.at("tree.parquet"), &keys);
        let membership: Vec<(&str, Vec<u64>)> = by_key
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        write_treed_members(&inputs.at("tree_members.parquet"), &membership);
    });

    assert_bundles_identical(&from_points, &from_table, "a flat list against a member table");
}

/// **The headline for §4: a lineage column and an artifact table with a `parent` column build the
/// same bundle, byte for byte.**
///
/// It is the strongest available statement that the reader takes the structure the caller supplied
/// rather than inventing one: every edge, every membership and every entity assignment comes out
/// where the enumerated form put them, and the enumerated form is the shape the build has always
/// read. The keys are integers on one side and their decimal spellings on the other, which is the
/// ordinary case — a clusterer writes ids and a producer writes names.
#[test]
fn a_lineage_column_and_an_edged_artifact_table_build_the_same_bundle() {
    let lists: Vec<Vec<Option<i64>>> = (0..N_ITEMS).map(lineage_of).collect();
    let from_points = format!("{}value_set = \"open\"\n{FROM_LINEAGE}", layer_of_kind("nested"));
    let (from_points, _a) = build_spelling(&from_points, |inputs| {
        write_listed_points(&inputs.points, &lists, false, None);
    });

    let from_tables = format!(
        "{}value_set = \"open\"\n{FROM_TREE_TABLES}",
        layer_of_kind("nested")
    );
    let (from_tables, _b) = build_spelling(&from_tables, |inputs| {
        write_listed_points(&inputs.points, &lists, false, None);
        let artifacts = lineage_artifacts();
        write_treed_artifacts(
            &inputs.at("tree.parquet"),
            &artifacts
                .iter()
                .map(|(key, parent)| (key.as_str(), parent.as_deref()))
                .collect::<Vec<_>>(),
        );
        write_treed_members(
            &inputs.at("tree_members.parquet"),
            &lineage_memberships()
                .iter()
                .map(|(key, members)| (key.as_str(), members.clone()))
                .collect::<Vec<_>>(),
        );
    });
    assert_bundles_identical(
        &from_points,
        &from_tables,
        "a lineage column against an artifact table with parents",
    );
}

/// Entity `e`'s levels, coarse → fine: one country, two states, four counties.
fn levels_of(entity: u64) -> Vec<Option<i64>> {
    vec![
        Some(1),
        Some(10 + (entity % 2) as i64),
        Some(100 + (entity % 4) as i64),
    ]
}

/// **A fixed-length list against `tiered` is the member table with a `level` column.** Entry *k* is
/// the artifact at level *k*, and the containment edges between consecutive entries are the ones
/// the enumerated form spells out in a `parent` column.
#[test]
fn a_levelled_column_and_a_levelled_member_table_build_the_same_bundle() {
    let lists: Vec<Vec<Option<i64>>> = (0..N_ITEMS).map(levels_of).collect();
    let layer = format!(
        "{}value_set = \"open\"\n{{members}}{THREE_LEVELS}",
        layer_of_kind("tiered")
    );
    let (from_points, _a) = build_spelling(&layer.replace("{members}", FROM_LINEAGE), |inputs| {
        write_listed_points(&inputs.points, &lists, false, Some(3));
    });

    let (from_tables, _b) =
        build_spelling(&layer.replace("{members}", FROM_LEVELLED_TABLES), |inputs| {
            write_listed_points(&inputs.points, &lists, false, Some(3));
            write_edged_artifacts(
                &inputs.at("admin.parquet"),
                &[
                    (0, "1", None),
                    (1, "10", Some("1")),
                    (1, "11", Some("1")),
                    (2, "100", Some("10")),
                    (2, "101", Some("11")),
                    (2, "102", Some("10")),
                    (2, "103", Some("11")),
                ],
            );
            let mut membership: Vec<(u32, &str, Vec<u64>)> = vec![(0, "1", (0..N_ITEMS).collect())];
            for (level, key, modulus, residue) in [
                (1u32, "10", 2u64, 0u64),
                (1, "11", 2, 1),
                (2, "100", 4, 0),
                (2, "101", 4, 1),
                (2, "102", 4, 2),
                (2, "103", 4, 3),
            ] {
                membership.push((
                    level,
                    key,
                    (0..N_ITEMS).filter(|e| e % modulus == residue).collect(),
                ));
            }
            write_levelled_members(&inputs.at("admin_members.parquet"), &membership);
        });
    assert_bundles_identical(
        &from_points,
        &from_tables,
        "a levelled column against a levelled member table",
    );
}

/// **A child naming two different parents is refused, and both are named.** The data is not the
/// tree the layer declared: there is no correct output, and choosing a parent would publish a
/// hierarchy nobody wrote.
#[test]
fn a_child_named_under_two_parents_is_refused_naming_both() {
    let lists: Vec<Vec<Option<i64>>> = (0..N_ITEMS)
        .map(|e| vec![Some(if e % 2 == 0 { 900 } else { 901 }), Some(950)])
        .collect();
    let layer = format!("{}value_set = \"open\"\n{FROM_LINEAGE}", layer_of_kind("nested"));
    let message = refuse_spelling(&layer, |inputs| {
        write_listed_points(&inputs.points, &lists, false, None);
    });
    assert!(message.contains("950"), "{message}");
    assert!(message.contains("900"), "{message}");
    assert!(message.contains("901"), "{message}");
}

/// **A variable-length list against a levelled declaration is refused.** Entry *k* means level *k*
/// only because there are as many entries as levels; a row of another length is a lineage, and
/// reading it as one would publish levels the layer did not declare.
#[test]
fn a_variable_length_list_against_a_tiered_layer_is_refused() {
    let lists: Vec<Vec<Option<i64>>> = (0..N_ITEMS)
        .map(|e| {
            let mut list = levels_of(e);
            // One point's clusterer stopped a level short — the shape the declaration forbids.
            if e == 7 {
                list.pop();
            }
            list
        })
        .collect();
    let layer = format!(
        "{}value_set = \"open\"\n{FROM_LINEAGE}{THREE_LEVELS}",
        layer_of_kind("tiered")
    );
    let message = refuse_spelling(&layer, |inputs| {
        write_listed_points(&inputs.points, &lists, false, None);
    });
    assert!(message.contains("declares 3 levels"), "{message}");
    assert!(message.contains("nested"), "{message}");
}

/// **A fixed-length list against `nested` is refused.** A lineage is as deep as the point's own
/// branch; a fixed arity is one entry per level, which is the other declaration entirely.
#[test]
fn a_fixed_length_list_against_a_nested_layer_is_refused() {
    let lists: Vec<Vec<Option<i64>>> = (0..N_ITEMS).map(lineage_of).collect();
    let layer = format!("{}value_set = \"open\"\n{FROM_LINEAGE}", layer_of_kind("nested"));
    let message = refuse_spelling(&layer, |inputs| {
        write_listed_points(&inputs.points, &lists, false, Some(3));
    });
    assert!(message.contains("fixed-size list"), "{message}");
    assert!(message.contains("nested"), "{message}");
}

/// **A null entry, and exactly `-1`, place the point at every level it named and at no other.** A
/// point is noise at a fine resolution and clustered at a coarse one — a quarter of a condensed
/// tree's points at each split — so the rule §2 states for a scalar key is the rule for an entry.
///
/// Asserted against the member table that enumerates exactly those memberships, so "exactly the
/// levels it named" is compared entity by entity rather than counted. The column is a plain list of
/// a uniform length, which is the shape a producer writing through Arrow's list builder emits.
///
/// **And an entry of noise links nothing across itself.** 400 is named at level 2 by points whose
/// level-1 entry is noise, so nothing says what contains it and it is published as a root — where
/// reading past the gap would have hung it under the country, which is a containment claim no row
/// makes and which the next point clustered at that resolution would contradict.
#[test]
fn null_and_noise_entries_place_a_point_at_the_levels_it_named() {
    let lists: Vec<Vec<Option<i64>>> = (0..N_ITEMS)
        .map(|e| {
            if e >= 240 {
                // In no artifact at any resolution: a whole row of noise, counted as one row.
                return vec![Some(-1), Some(-1), None];
            }
            if e >= 230 {
                // Noise at level 1, clustered at level 2.
                return vec![Some(1), Some(-1), Some(400)];
            }
            vec![
                Some(1),
                if e % 2 == 0 { Some(20) } else { Some(-1) },
                if e % 4 == 0 { Some(300) } else { None },
            ]
        })
        .collect();
    let layer = format!(
        "{}value_set = \"open\"\n{{members}}{THREE_LEVELS}",
        layer_of_kind("tiered")
    );
    let (from_points, _a, report) =
        build_spelling_reported(&layer.replace("{members}", FROM_LINEAGE), |inputs| {
            write_listed_points(&inputs.points, &lists, false, None);
        });
    assert_eq!(
        report.unclustered_member_rows, 10,
        "a row whose every entry is noise is one row in no artifact"
    );

    let (from_tables, _b) =
        build_spelling(&layer.replace("{members}", FROM_LEVELLED_TABLES), |inputs| {
            write_listed_points(&inputs.points, &lists, false, None);
            write_edged_artifacts(
                &inputs.at("admin.parquet"),
                // 300's parent is 20 and 20's is 1 — the edges the points that named both
                // declared. 400 has none: every point that named it was noise at level 1.
                &[
                    (0, "1", None),
                    (1, "20", Some("1")),
                    (2, "300", Some("20")),
                    (2, "400", None),
                ],
            );
            write_levelled_members(
                &inputs.at("admin_members.parquet"),
                &[
                    (0, "1", (0..240).collect()),
                    (1, "20", (0..230).filter(|e| e % 2 == 0).collect()),
                    (2, "300", (0..230).filter(|e| e % 4 == 0).collect()),
                    (2, "400", (230..240).collect()),
                ],
            );
        });
    assert_bundles_identical(
        &from_points,
        &from_tables,
        "a column with null and -1 entries against the memberships it names",
    );
}

/// **Under `open`, a lineage naming clusters no artifact declares mints all of them — the interior
/// parents included.** A cluster exists because points say it does, and a parent named only as
/// somebody's parent is as much a cluster as the leaf that named it.
#[test]
fn an_open_layer_mints_the_interior_parents_a_lineage_names() {
    let lists: Vec<Vec<Option<i64>>> = (0..N_ITEMS).map(lineage_of).collect();
    let layer = format!(
        "{}value_set = \"open\"\nsource = \"roster.parquet\"\n{FROM_LINEAGE}",
        layer_of_kind("nested")
    );
    let (out, _tmp, _) = build_spelling_reported(&layer, |inputs| {
        write_listed_points(&inputs.points, &lists, false, None);
        // The table has heard of the root alone. The three groups under it are named by no row of
        // its own and by no leaf's own key — only as the parent inside a lineage.
        write_cluster_roster(&inputs.at("roster.parquet"), &[1]);
    });
    let manifest = manifest_of(&out);
    assert_eq!(
        manifest
            .membership_extents
            .iter()
            .map(|e| e.count)
            .collect::<Vec<_>>(),
        vec![10],
        "one root, three groups and six leaves"
    );
    // The groups are internal nodes, which is the assertion that their edges were minted with
    // them: a parent nothing hangs from is not a split.
    assert_eq!(containment_report(&out)["splits"]["total"], 4);
    assert_eq!(
        containment_report(&out)["violations"].as_array().unwrap().len(),
        0
    );
}
