//! **The identity column the declaration names is the row's external id, whatever type it holds.**
//!
//! `[defaults].entity_id_field` says where a row's identity is read from (`configuration.md` §1,
//! §8), and the bytes there are the caller-supplied external id contracts §2.4 defines. Three
//! routes, and this file builds a corpus down each:
//!
//! - a **string** id column, joined to a members table and an attribute source naming the same
//!   strings, writing the external-id index from those strings and nothing else;
//! - an **integer** id column, which keeps what it always wrote: eight little-endian bytes per
//!   row, the runs sorted over those bytes, under `--mint-external-ids`;
//! - **no id column at all**, contracts §2.4's caller who supplied no identity: no extent, no
//!   locator, and rows addressable by `tessera_id` alone.
//!
//! The string bundle is deep-verified, the external-id sidecar being one of the things
//! `tessera verify --deep` walks in both directions (`tessera_build::deep`).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, Int64Array, ListArray, StringArray, UInt64Array};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::{build, verify_deep, BuildArgs, VerifyOpts};
use tessera_spatial::Bounds;
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const N: usize = 24;

/// The keys this corpus names its rows by, **not** in the order the file writes them. A build
/// interns them bytewise, and a fixture whose file order already agreed could not tell the two
/// apart.
fn keys() -> Vec<String> {
    (0..N).map(|i| format!("doc-{:02}", (i * 7) % N)).collect()
}

/// Integer ids whose byte order is not their numeric order, so the two sorts are distinguishable.
fn integers() -> Vec<u64> {
    (0..N as u64).map(|i| i * 0x0100).collect()
}

fn write(path: &Path, fields: Vec<Field>, columns: Vec<ArrayRef>) {
    let schema = Arc::new(ArrowSchema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

fn coordinates() -> (Vec<f64>, Vec<f64>) {
    (
        (0..N).map(|i| ((i * 37) % 1000) as f64).collect(),
        (0..N).map(|i| ((i * 53) % 1000) as f64).collect(),
    )
}

/// The points file under an id column named `doc_id`, carrying its own access labels and one
/// declared attribute, or, where `ids` is `None`, carrying no identity column at all.
fn write_points(path: &Path, ids: Option<ArrayRef>) {
    let (xs, ys) = coordinates();
    let mut fields = Vec::new();
    let mut columns: Vec<ArrayRef> = Vec::new();
    if let Some(ids) = ids {
        fields.push(Field::new("doc_id", ids.data_type().clone(), false));
        columns.push(ids);
    }
    fields.push(Field::new("x", DataType::Float64, false));
    fields.push(Field::new("y", DataType::Float64, false));
    fields.push(Field::new("visibility", DataType::Utf8, false));
    fields.push(Field::new("flag", DataType::Int64, false));
    columns.push(Arc::new(Float64Array::from(xs)));
    columns.push(Arc::new(Float64Array::from(ys)));
    columns.push(Arc::new(StringArray::from(vec!["public"; N])));
    columns.push(Arc::new(Int64Array::from(
        (0..N).map(|i| (i % 3) as i64).collect::<Vec<_>>(),
    )));
    write(path, fields, columns);
}

/// `(key, doc_id)`: one row per `(artifact, entity)`, naming entities the way the points file
/// spells them.
fn write_members(path: &Path, ids: ArrayRef) {
    let cluster: Vec<String> = (0..ids.len()).map(|i| format!("c{}", i % 3)).collect();
    write(
        path,
        vec![
            Field::new("key", DataType::Utf8, false),
            Field::new("doc_id", ids.data_type().clone(), false),
        ],
        vec![Arc::new(StringArray::from(cluster)), ids],
    );
}

/// One row per artifact, its membership a list of entities in the cell.
fn write_artifacts(path: &Path) {
    let members = ListArray::new(
        Arc::new(Field::new("item", DataType::UInt64, true)),
        OffsetBuffer::from_lengths([3usize, 2]),
        Arc::new(UInt64Array::from(vec![0u64, 3, 5, 7, 9])) as ArrayRef,
        None,
    );
    write(
        path,
        vec![
            Field::new("key", DataType::Utf8, false),
            Field::new(
                "members",
                DataType::List(Arc::new(Field::new("item", DataType::UInt64, true))),
                true,
            ),
        ],
        vec![
            Arc::new(StringArray::from(vec!["c0", "c1"])),
            Arc::new(members),
        ],
    );
}

const DECLARATION: &str = r#"
[sources]
points   = "points.parquet"
members  = "members.parquet"
clusters = "clusters.parquet"

[defaults]
source          = "points"
entity_id_field = "doc_id"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { field = "visibility", default = "public" }

[[attribute]]
name = "flag"
type = "i64"
"#;

const LAYER: &str = r#"
[[layer]]
name       = "clusters/a"
views      = ["s0"]
membership = "enumerated"
hierarchy  = { kind = "flat" }
value_set  = "open"

visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "none"

  [layer.members]
  source = "members"
  fields = { entity = "doc_id" }
"#;

/// A layer whose artifacts file carries the membership as a list per artifact, the other of
/// `configuration.md` §8's two shapes.
const ARTIFACT_LAYER: &str = r#"
[[layer]]
name       = "clusters/a"
views      = ["s0"]
source     = "clusters"
membership = "enumerated"
hierarchy  = { kind = "flat" }
value_set  = "open"

visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "none"
"#;

/// The same layer with its artifacts, and their memberships, written in the document.
const INLINE_LAYER: &str = r#"
[[layer]]
name       = "clusters/a"
views      = ["s0"]
membership = "enumerated"
hierarchy  = { kind = "flat" }
value_set  = "open"
artifacts  = [{ key = "c0", members = [0, 3, 5] }, { key = "c1", members = [7, 9] }]

visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "none"
"#;

/// The declaration, parsed against the files in `dir`, with `layer` appended where the case wants
/// a layer.
fn declaration(dir: &Path, layer: &str) -> tessera_build::config::Config {
    let path = dir.join("schema.toml");
    std::fs::write(&path, format!("{DECLARATION}{layer}")).unwrap();
    tessera_build::config::Config::parse(&path, &Default::default())
        .expect("the declaration parses")
}

/// One build over that declaration, into `out`.
fn args(dir: &Path, layer: &str, mint: bool, out: &Path) -> BuildArgs {
    let config = declaration(dir, layer);
    let view = &config.views[0];
    BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: view.name.clone(),
            projection: tessera_spatial::Projection::None,
            extent: Bounds {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            points: view.source.clone().expect("the view names a source"),
            point_fields: view.fields.clone(),
            select: None,
            access: tessera_build::config::AccessInput {
                source: tessera_build::config::AccessSource::Field("visibility".to_string()),
                default: Some("public".to_string()),
            },
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: config.attribute_sources.clone(),
        out: out.to_path_buf(),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: config.layers.clone(),
        layer_inputs: config.layer_sources.clone(),
        scoped_layers: Default::default(),
        mint_external_ids: mint,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: config.schema.clone(),
    }
}

/// The bundle's external-id sidecar, or `None` where it wrote none.
fn sidecar(root: &Path) -> Option<tessera_store::ExternalIdSidecar> {
    let bundle = tessera_store::read::open_bundle(root).expect("the built bundle opens");
    let partition = bundle.partitions.get("default").expect("one partition");
    if partition.manifest.external_id_runs.is_empty() {
        return None;
    }
    Some(
        tessera_store::ExternalIdSidecar::deferred_from_manifest(
            &bundle.manifest,
            &partition.manifest,
            &root.join("v00000"),
        )
        .expect("the sidecar constructs"),
    )
}

/// Every external id the sidecar binds, in ascending entity order, read back through the
/// drill-down direction the locator serves.
fn bound(root: &Path, count: usize) -> Vec<Vec<u8>> {
    let sidecar = sidecar(root).expect("the bundle carries a sidecar");
    (0..count as u64)
        .map(|entity| {
            sidecar
                .external_id_of(tessera_types::EntityId::new(entity))
                .expect("the locator reads")
                .unwrap_or_else(|| panic!("entity {entity} has an external id"))
        })
        .collect()
}

// -------------------------------------------------------------------------------------------

/// **A string id column is the external id, byte for byte, and the members table joins on it.**
#[test]
fn a_string_id_column_is_the_external_id_and_the_join_key() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let keys = keys();
    write_points(
        &dir.join("points.parquet"),
        Some(Arc::new(StringArray::from(keys.clone()))),
    );
    write_members(
        &dir.join("members.parquet"),
        Arc::new(StringArray::from(keys.clone())),
    );
    let out = dir.join("bundle");
    // No `--mint-external-ids`: the caller named every row, so the index is written without a flag.
    let report = build(&args(dir, LAYER, false, &out)).expect("the build succeeds");
    assert_eq!(report.items, N as u64);

    // Every key round-trips in both directions, which is the sidecar's whole contract.
    let sidecar = sidecar(&out).expect("a supplied id column writes the sidecar");
    let mut entities = Vec::new();
    for key in &keys {
        let entity = sidecar
            .resolve(key.as_bytes())
            .expect("the sidecar resolves")
            .unwrap_or_else(|| panic!("'{key}' is bound"));
        assert_eq!(
            sidecar
                .external_id_of(entity)
                .expect("the locator reads")
                .as_deref(),
            Some(key.as_bytes()),
            "the drill-down returns the bytes the caller supplied"
        );
        entities.push(entity.raw());
    }
    entities.sort_unstable();
    assert_eq!(
        entities,
        (0..N as u64).collect::<Vec<_>>(),
        "every entity is bound exactly once"
    );

    // The members table named those strings: a key naming no row is a refusal, so a build that
    // got here joined all of them, and the artifacts the open value set minted are the clusters.
    assert_eq!(report.minted_artifacts, 3, "three cluster keys");
    assert_eq!(report.unclustered_member_rows, 0);

    verify_deep(&out, &VerifyOpts::default()).expect("the string-keyed bundle verifies");
}

/// **The integer route writes the bytes it always wrote**: each row's eight little-endian bytes,
/// under the flag and not otherwise.
#[test]
fn an_integer_id_column_keeps_its_eight_little_endian_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let ids = integers();
    write_points(
        &dir.join("points.parquet"),
        Some(Arc::new(UInt64Array::from(ids.clone()))),
    );
    write_members(
        &dir.join("members.parquet"),
        Arc::new(UInt64Array::from(ids.clone())),
    );

    // Without the flag an integer column mints nothing: a source-corpus number is not a namespace
    // the caller owns.
    let plain = dir.join("plain");
    build(&args(dir, LAYER, false, &plain)).expect("the build succeeds");
    assert!(sidecar(&plain).is_none(), "no flag, no sidecar");

    let minted = dir.join("minted");
    build(&args(dir, LAYER, true, &minted)).expect("the minted build succeeds");
    let held = bound(&minted, N);
    let mut expected: Vec<Vec<u8>> = ids.iter().map(|id| id.to_le_bytes().to_vec()).collect();
    let mut found = held.clone();
    expected.sort();
    found.sort();
    assert_eq!(found, expected);
    for key in &expected {
        assert!(
            sidecar(&minted)
                .unwrap()
                .resolve(key)
                .expect("the sidecar resolves")
                .is_some(),
            "every integer id is bound under its own eight bytes"
        );
    }
    verify_deep(&minted, &VerifyOpts::default()).expect("the integer-keyed bundle verifies");
}

/// **A member naming an id the points file does not carry is refused**, for a supplied key as for
/// an integer.
#[test]
fn a_member_naming_an_unknown_key_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let keys = keys();
    write_points(
        &dir.join("points.parquet"),
        Some(Arc::new(StringArray::from(keys.clone()))),
    );
    let mut named = keys.clone();
    named[0] = "doc-99".to_string();
    write_members(
        &dir.join("members.parquet"),
        Arc::new(StringArray::from(named)),
    );
    let said = build(&args(dir, LAYER, false, &dir.join("bundle")))
        .expect_err("a member naming no entity is refused")
        .to_string();
    assert!(said.contains("doc-99"), "{said}");
    assert!(
        said.contains("no points file of this build carries"),
        "{said}"
    );
}

/// **A members table in the other type family is refused once, against the column.** Resolving it
/// row by row would refuse on the first row as an unknown key, which is an answer to the wrong
/// question: the declaration named one identity and this producer has not been rewritten.
#[test]
fn a_members_column_in_the_other_family_is_refused_naming_both_types() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_points(
        &dir.join("points.parquet"),
        Some(Arc::new(StringArray::from(keys()))),
    );
    write_members(
        &dir.join("members.parquet"),
        Arc::new(UInt64Array::from(integers())),
    );
    let said = build(&args(dir, LAYER, false, &dir.join("bundle")))
        .expect_err("a members column in the other family is refused")
        .to_string();
    assert!(said.contains("UInt64"), "{said}");
    assert!(said.contains("Utf8"), "{said}");
    assert!(said.contains("One identity is one type"), "{said}");
}

/// **A membership written beside an artifact names rows too**, and on the positional route there
/// are no rows for it to name. Both spellings of that shape are refused, naming the layer and,
/// where there is one, the file.
#[test]
fn a_membership_beside_an_artifact_is_refused_with_no_id_column() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_points(&dir.join("points.parquet"), None);
    write_artifacts(&dir.join("clusters.parquet"));
    let said = build(&args(dir, ARTIFACT_LAYER, false, &dir.join("from-file")))
        .expect_err("an artifacts file carrying a membership is refused")
        .to_string();
    assert!(said.contains("clusters/a"), "{said}");
    assert!(said.contains("clusters.parquet"), "{said}");
    assert!(said.contains("Declare an identity column"), "{said}");

    let said = build(&args(dir, INLINE_LAYER, false, &dir.join("inline")))
        .expect_err("an inline membership is refused")
        .to_string();
    assert!(said.contains("clusters/a"), "{said}");
    assert!(said.contains("Declare an identity column"), "{said}");
}

/// **A points file that stores Morton codes and no identity column builds.** The positional route
/// has no identity column to project, so it projects this file's own geometry, and a corpus
/// holding codes carries no `x` at all.
#[test]
fn a_morton_points_file_with_no_id_column_builds() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write(
        &dir.join("points.parquet"),
        vec![
            Field::new("morton", DataType::UInt64, false),
            Field::new("visibility", DataType::Utf8, false),
            Field::new("flag", DataType::Int64, false),
        ],
        vec![
            Arc::new(UInt64Array::from(
                (0..N)
                    .map(|i| (i as u64 * 131) % 65_536)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(vec!["public"; N])),
            Arc::new(Int64Array::from(
                (0..N).map(|i| (i % 3) as i64).collect::<Vec<_>>(),
            )),
        ],
    );
    let out = dir.join("bundle");
    let mut args = args(dir, "", false, &out);
    args.views[0].extent = tessera_build::input::IDENTITY_EXTENT;
    let report = build(&args).expect("a Morton points file with no id builds");
    assert_eq!(report.items, N as u64);
    assert!(sidecar(&out).is_none(), "no id column, no external ids");
}

/// **A points file with no id column builds, with no external ids at all**, which is contracts
/// §2.4's rule for a caller who supplied none. The rows are addressable by `tessera_id` and by nothing else.
#[test]
fn a_points_file_with_no_id_column_writes_no_external_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_points(&dir.join("points.parquet"), None);
    let out: PathBuf = dir.join("bundle");
    let report = build(&args(dir, "", false, &out)).expect("a points file with no id builds");
    assert_eq!(report.items, N as u64);
    assert!(sidecar(&out).is_none(), "no id column, no external ids");
}
