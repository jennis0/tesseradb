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
    ArrayRef, Float64Array, ListBuilder, StringArray, StringBuilder, UInt32Array, UInt64Array,
};
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

/// A clustering, and a label layer attached into it — the shape the whole feature exists for.
const LAYERS_TOML: &str = r#"
[[layer]]
name = "clusters/a"
title = "clusters"
views = ["s0"]
membership = "enumerated"
gate = "0"
artifacts_carry_own = false
visible_when = { min_visible = 2 }
hierarchy = { kind = "flat", prune_children = false }
content = { derived = ["centroid"] }

[[layer]]
name = "topics/x"
title = "topics"
views = ["s0"]
membership = "enumerated"
ungated = true
artifacts_carry_own = false
visible_when = "none"
hierarchy = { kind = "flat" }
depends_on = ["clusters/a"]

[[layer.content.supplied]]
kind = "label_text"
corpus_derived = true
"#;

/// One row per `(artifact, variation)`: two clusters with no content, and one label carrying two
/// ranked descriptions and hanging from the first cluster. No parent/child edges in this fixture.
fn write_artifacts(path: &Path) {
    write_artifacts_named(path, "topics/x")
}

/// The same, with the label layer under another name — so a test can put it either side of the
/// cluster layer alphabetically.
fn write_artifacts_named(path: &Path, labels: &str) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("layer", DataType::Utf8, false),
        Field::new("stable_key", DataType::Utf8, false),
        Field::new("variation", DataType::UInt32, true),
        Field::new(
            "values",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            true,
        ),
        Field::new("attached_layer", DataType::Utf8, true),
        Field::new("attached_key", DataType::Utf8, true),
        Field::new("parent_key", DataType::Utf8, true),
        Field::new(
            "children_keys",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            true,
        ),
    ]));
    let layers = StringArray::from(vec!["clusters/a", "clusters/a", labels, labels]);
    let keys = StringArray::from(vec!["c-0000", "c-0001", "l-0000", "l-0000"]);
    let variation = UInt32Array::from(vec![None, None, Some(0), Some(1)]);
    let mut values = ListBuilder::new(StringBuilder::new());
    values.append(false);
    values.append(false);
    values.values().append_value("the whole cluster");
    values.append(true);
    values.values().append_value("the visible part");
    values.append(true);
    let attached_layer = StringArray::from(vec![None, None, Some("clusters/a"), Some("clusters/a")]);
    let attached_key = StringArray::from(vec![None, None, Some("c-0000"), Some("c-0000")]);
    let parent_key: StringArray = vec![None::<&str>, None, None, None].into();
    let mut children_keys = ListBuilder::new(StringBuilder::new());
    children_keys.append(false);
    children_keys.append(false);
    children_keys.append(false);
    children_keys.append(false);

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(layers) as ArrayRef,
            Arc::new(keys),
            Arc::new(variation),
            Arc::new(values.finish()),
            Arc::new(attached_layer),
            Arc::new(attached_key),
            Arc::new(parent_key),
            Arc::new(children_keys.finish()),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// One row per `(artifact, member)`, in **source** entity ids — and deliberately shuffled, since
/// ordinals must be a function of the artifacts and not of the file's row order.
fn write_members(path: &Path, members_of_first_cluster: &[u64]) {
    write_members_of(path, members_of_first_cluster, "topics/x")
}

fn write_members_named(path: &Path, labels: &str) {
    write_members_of(path, &(0..30).collect::<Vec<u64>>(), labels)
}

fn write_members_of(path: &Path, members_of_first_cluster: &[u64], labels: &str) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("layer", DataType::Utf8, false),
        Field::new("stable_key", DataType::Utf8, false),
        Field::new("variation", DataType::UInt32, true),
        Field::new("member", DataType::UInt64, false),
    ]));
    let mut layers = Vec::new();
    let mut keys = Vec::new();
    let mut variation: Vec<Option<u32>> = Vec::new();
    let mut member = Vec::new();
    let mut row = |layer: &str, key: &str, v: Option<u32>, m: u64| {
        layers.push(layer.to_string());
        keys.push(key.to_string());
        variation.push(v);
        member.push(m);
    };
    for &m in members_of_first_cluster {
        row("clusters/a", "c-0000", None, m);
        row(labels, "l-0000", None, m);
        // Variation 0 was generated from the whole cluster; variation 1 from a third of it.
        row(labels, "l-0000", Some(0), m);
        if m % 3 == 0 {
            row(labels, "l-0000", Some(1), m);
        }
    }
    for m in 100..110u64 {
        row("clusters/a", "c-0001", None, m);
    }

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(layers)) as ArrayRef,
            Arc::new(StringArray::from(keys)),
            Arc::new(UInt32Array::from(variation)),
            Arc::new(UInt64Array::from(member)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

struct Inputs {
    _tmp: tempfile::TempDir,
    points: PathBuf,
    pairs: PathBuf,
    layers: PathBuf,
    artifacts: PathBuf,
    members: PathBuf,
    dir: PathBuf,
}

fn inputs() -> Inputs {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let layers = dir.join("layers.toml");
    let artifacts = dir.join("artifacts.parquet");
    let members = dir.join("members.parquet");
    write_points(&points);
    write_pairs(&pairs);
    std::fs::write(&layers, LAYERS_TOML).unwrap();
    write_artifacts(&artifacts);
    write_members(&members, &(0..30).collect::<Vec<u64>>());
    Inputs {
        _tmp: tmp,
        points,
        pairs,
        layers,
        artifacts,
        members,
        dir,
    }
}

fn args(inputs: &Inputs, out: &Path) -> BuildArgs {
    BuildArgs {
        points: inputs.points.clone(),
        pairs: inputs.pairs.clone(),
        out: out.to_path_buf(),
        extent: extent(),
        view_id: "s0".to_string(),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Some(inputs.layers.clone()),
        artifacts: Some(inputs.artifacts.clone()),
        artifact_members: Some(inputs.members.clone()),
        mint_external_ids: false,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    }
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
    build(&args(&inputs, &out)).expect("a build with layers succeeds");
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
    build(&args(&inputs, &streamed)).unwrap();
    build_in_memory(&args(&inputs, &linear)).unwrap();

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
    write_members(&inputs.members, &[0, 1, N_ITEMS + 500]);
    let out = inputs.dir.join("bundle");
    let err = build(&args(&inputs, &out)).expect_err("an unknown member is a refusal");
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
    std::fs::write(&inputs.layers, reversed).unwrap();
    let out = inputs.dir.join("bundle");
    let err = build(&args(&inputs, &out)).expect_err("a dependency must exist before its dependent");
    assert!(format!("{err}").contains("depends_on"), "{err}");
}

/// **A key the artifacts file does not declare is a refusal, not a new artifact.** A mistyped key
/// in the members file would otherwise publish a phantom beside a real cluster whose count is
/// quietly short — and on a layer declaring no supplied content nothing downstream notices.
#[test]
fn a_member_row_naming_an_undeclared_artifact_is_refused() {
    let inputs = inputs();
    // Every member of `c-0000` except one, whose key is mistyped.
    let schema = Arc::new(Schema::new(vec![
        Field::new("layer", DataType::Utf8, false),
        Field::new("stable_key", DataType::Utf8, false),
        Field::new("member", DataType::UInt64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec!["clusters/a", "clusters/a"])) as ArrayRef,
            Arc::new(StringArray::from(vec!["c-0000", "c-OOO0"])),
            Arc::new(UInt64Array::from(vec![0u64, 1])),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(&inputs.members).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();

    let out = inputs.dir.join("bundle");
    let err = build(&args(&inputs, &out)).expect_err("an undeclared key is a refusal");
    assert!(format!("{err}").contains("c-OOO0"), "{err}");
}

/// **A null member is not entity zero.** Arrow reads the values buffer whatever the validity
/// bitmap says, so a producer whose join missed a row would publish the corpus's lowest-numbered
/// document into the artifact, moving its masked count for whoever can see that document.
#[test]
fn a_null_member_is_refused_rather_than_read_as_entity_zero() {
    let inputs = inputs();
    let schema = Arc::new(Schema::new(vec![
        Field::new("layer", DataType::Utf8, false),
        Field::new("stable_key", DataType::Utf8, false),
        Field::new("member", DataType::UInt64, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec!["clusters/a", "clusters/a"])) as ArrayRef,
            Arc::new(StringArray::from(vec!["c-0000", "c-0000"])),
            Arc::new(UInt64Array::from(vec![Some(7u64), None])),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(&inputs.members).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();

    let out = inputs.dir.join("bundle");
    let err = build(&args(&inputs, &out)).expect_err("a null member is a refusal");
    assert!(format!("{err}").contains("null member"), "{err}");
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
    std::fs::write(&inputs.layers, renamed(LAYERS_TOML)).unwrap();
    write_artifacts_named(&inputs.artifacts, "annotations/topics");
    write_members_named(&inputs.members, "annotations/topics");

    let out = inputs.dir.join("bundle");
    build(&args(&inputs, &out)).expect("declaration order is what decides, not the layer's name");
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
        if !line.starts_with("visible_when") {
            without.push_str(line);
            without.push('\n');
        }
    }
    std::fs::write(&inputs.layers, without).unwrap();
    let out = inputs.dir.join("bundle");
    let err = build(&args(&inputs, &out)).expect_err("an absent criterion is a refusal");
    assert!(format!("{err}").contains("visible_when"), "{err}");
}

/// A layer in a view this build does not write would be registered, reachable and empty — which
/// no client can tell from a layer whose artifacts were all withheld. Refused.
#[test]
fn a_layer_naming_a_view_this_build_does_not_write_is_refused() {
    let inputs = inputs();
    std::fs::write(
        &inputs.layers,
        LAYERS_TOML.replace(r#"views = ["s0"]"#, r#"views = ["s7"]"#),
    )
    .unwrap();
    let out = inputs.dir.join("bundle");
    let err = build(&args(&inputs, &out)).expect_err("a view that does not exist is a refusal");
    assert!(format!("{err}").contains("s7"), "{err}");
}

/// Artifacts without a layer file are a refusal rather than a build that quietly drops them: a
/// bundle whose clusters are absent cannot be told from one whose clusters failed their criterion.
#[test]
fn artifacts_without_a_layer_file_are_refused() {
    let inputs = inputs();
    let out = inputs.dir.join("bundle");
    let mut args = args(&inputs, &out);
    args.layers = None;
    let err = build(&args).expect_err("artifacts need layers");
    assert!(format!("{err}").contains("--layers"), "{err}");
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
membership = "enumerated"
gate = "0"
artifacts_carry_own = false
visible_when = { min_visible = 2 }
hierarchy = { kind = "nested", prune_children = false }
content = { derived = ["centroid"] }
"#;

/// A three-node tree: one root and two children, written with `parent_key` on each child.
///
/// `children_keys` is left null throughout. The parent direction is the source of truth and the
/// child direction is derived from it — a fixture that wrote both would be testing whether two
/// hand-written columns agree, which is not a property of the system.
fn write_treed_artifacts(path: &Path, child_parent: &[(&str, Option<&str>)]) {
    write_edged_artifacts(path, "clusters/tree", &child_parent
        .iter()
        .map(|(k, p)| (0u32, *k, *p))
        .collect::<Vec<_>>())
}

/// The same with an explicit level per artifact, for a layer whose edges run between levels.
fn write_edged_artifacts(path: &Path, layer: &str, rows: &[(u32, &str, Option<&str>)]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("layer", DataType::Utf8, false),
        Field::new("level", DataType::UInt32, true),
        Field::new("stable_key", DataType::Utf8, false),
        Field::new("parent_key", DataType::Utf8, true),
    ]));
    let layers = StringArray::from(vec![layer; rows.len()]);
    let levels = UInt32Array::from(rows.iter().map(|(l, ..)| Some(*l)).collect::<Vec<_>>());
    let keys = StringArray::from(rows.iter().map(|(_, k, _)| *k).collect::<Vec<_>>());
    let parents = StringArray::from(rows.iter().map(|(.., p)| *p).collect::<Vec<_>>());

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(layers) as ArrayRef,
            Arc::new(levels),
            Arc::new(keys),
            Arc::new(parents),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

#[allow(dead_code)]
fn write_treed_artifacts_unused(path: &Path, child_parent: &[(&str, Option<&str>)]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("layer", DataType::Utf8, false),
        Field::new("stable_key", DataType::Utf8, false),
        Field::new("parent_key", DataType::Utf8, true),
    ]));
    let layers = StringArray::from(vec!["clusters/tree"; child_parent.len()]);
    let keys = StringArray::from(child_parent.iter().map(|(k, _)| *k).collect::<Vec<_>>());
    let parents = StringArray::from(child_parent.iter().map(|(_, p)| *p).collect::<Vec<_>>());

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(layers) as ArrayRef,
            Arc::new(keys),
            Arc::new(parents),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// Memberships for a treed level, one row per `(artifact, member)`.
fn write_treed_members(path: &Path, membership: &[(&str, Vec<u64>)]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("layer", DataType::Utf8, false),
        Field::new("stable_key", DataType::Utf8, false),
        Field::new("member", DataType::UInt64, false),
    ]));
    let mut layers = Vec::new();
    let mut keys = Vec::new();
    let mut member = Vec::new();
    for (key, members) in membership {
        for &m in members {
            layers.push("clusters/tree".to_string());
            keys.push(key.to_string());
            member.push(m);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(layers)) as ArrayRef,
            Arc::new(StringArray::from(keys)),
            Arc::new(UInt64Array::from(member)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// Build a treed fixture and return the containment report the build wrote.
fn treed_build(
    child_parent: &[(&str, Option<&str>)],
    membership: &[(&str, Vec<u64>)],
) -> (tessera_build::error::Result<()>, PathBuf, tempfile::TempDir) {
    let inputs = inputs();
    std::fs::write(&inputs.layers, TREED_LAYERS_TOML).unwrap();
    write_treed_artifacts(&inputs.artifacts, child_parent);
    write_treed_members(&inputs.members, membership);
    let out = inputs.dir.join("bundle");
    let result = build(&args(&inputs, &out)).map(|_| ());
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
membership = "enumerated"
gate = "0"
artifacts_carry_own = false
visible_when = { min_visible = 1 }
hierarchy = { kind = "tiered", prune_children = false }
content = { derived = ["centroid"] }

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
    std::fs::write(&inputs.layers, TIERED_LAYERS_TOML).unwrap();
    write_edged_artifacts(&inputs.artifacts, "admin/boundaries", rows);
    write_levelled_members(&inputs.members, "admin/boundaries", membership);
    let out = inputs.dir.join("bundle");
    let result = build(&args(&inputs, &out)).map(|_| ());
    (result, out, inputs._tmp)
}

fn write_levelled_members(path: &Path, layer: &str, membership: &[(u32, &str, Vec<u64>)]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("layer", DataType::Utf8, false),
        Field::new("level", DataType::UInt32, true),
        Field::new("stable_key", DataType::Utf8, false),
        Field::new("member", DataType::UInt64, false),
    ]));
    let (mut layers, mut levels, mut keys, mut member) = (vec![], vec![], vec![], vec![]);
    for (level, key, members) in membership {
        for &m in members {
            layers.push(layer.to_string());
            levels.push(Some(*level));
            keys.push(key.to_string());
            member.push(m);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(layers)) as ArrayRef,
            Arc::new(UInt32Array::from(levels)),
            Arc::new(StringArray::from(keys)),
            Arc::new(UInt64Array::from(member)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
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
        &inputs.layers,
        TIERED_LAYERS_TOML.replace(r#"kind = "tiered""#, r#"kind = "stacked""#),
    )
    .unwrap();
    write_edged_artifacts(
        &inputs.artifacts,
        "admin/boundaries",
        &[(0, "country", None), (1, "state-a", Some("country"))],
    );
    write_levelled_members(
        &inputs.members,
        "admin/boundaries",
        &[(0, "country", (0..20).collect()), (1, "state-a", (0..10).collect())],
    );
    let out = inputs.dir.join("bundle");
    let err = build(&args(&inputs, &out)).expect_err("a stacked layer has no lineage");
    assert!(format!("{err}").contains("no lineage"), "{err}");
}
