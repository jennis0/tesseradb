//! `point_visibility = { field }`: a point's access terms read from a column of the view's own
//! source, and `public` reserved at term 0 (`configuration.md` §1, `per-point-attributes.md` §3.8).
//!
//! **The one case in this stage that changes what a request computes**, so it is asserted end to
//! end rather than at the reader: a bundle is built from a `list<string>` column and then
//! *authorised against*, because the properties at issue are properties of a principal's mask.
//! Four of them, each the fail-closed half of a plausible misreading:
//!
//! - a **null** value and an **empty list** both mean *no access terms*, which means visible to no
//!   principal — never unrestricted. Where a `default` is declared it fills those rows, and where
//!   that default is a label nobody holds they stay invisible;
//! - **filling never overrides**: a point carrying terms of its own keeps exactly those, and is not
//!   also given the default. A point's terms are disjunctive, so a label added to one can only
//!   widen it;
//! - **terms are trimmed**, so ` ir:analyst ` and `ir:analyst` are one term rather than two that no
//!   credential spells the same way;
//! - **`public` reaches every principal by construction**, including a credential carrying no
//!   descriptors at all — and it is added inside the engine rather than granted or plugged in.

mod common;

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, ListArray, StringArray, UInt64Array};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::{extent, open_engine, test_key, TEST_KEY_HEX};
use tessera_build::config::{AccessInput, AccessSource};
use tessera_build::{build, BuildArgs};

/// Five points, one per shape a row can take: two terms, null, empty, a term needing a trim, and
/// one term.
fn access_of(e: u64) -> Option<Vec<&'static str>> {
    match e {
        0 => Some(vec!["ir:analyst", "ir:legal"]),
        1 => None,
        2 => Some(vec![]),
        3 => Some(vec!["  public  "]),
        _ => Some(vec![" ir:analyst"]),
    }
}

const N: u64 = 5;

/// The points source, carrying its own `list<string>` access column.
fn write_points(path: &Path, access: impl Fn(u64) -> Option<Vec<&'static str>>) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new(
            "categories",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            true,
        ),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();

    let mut offsets: Vec<i32> = vec![0];
    let mut flat: Vec<&str> = Vec::new();
    let mut present: Vec<bool> = Vec::new();
    for &e in &ids {
        match access(e) {
            None => present.push(false),
            Some(terms) => {
                present.push(true);
                flat.extend(terms);
            }
        }
        offsets.push(flat.len() as i32);
    }
    let values: ArrayRef = Arc::new(StringArray::from(flat));
    let list = ListArray::new(
        Arc::new(Field::new("item", DataType::Utf8, true)),
        OffsetBuffer::new(offsets.into()),
        values,
        Some(present.into()),
    );

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(list),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn args(points: &Path, out: &Path, default: &str) -> BuildArgs {
    BuildArgs {
        views: vec![tessera_build::ViewArgs {
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.to_path_buf(),
            point_fields: Default::default(),
            access: AccessInput {
                source: AccessSource::Field("categories".to_string()),
                default: default.to_string(),
            },
        }],
        anchor: 0,
        groups: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.to_path_buf(),
        limit: None,
        identity_key: test_key(),
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

/// How many of the corpus's points a credential carrying `terms` may see.
fn visible(dir: &Path, terms: &[&str]) -> u64 {
    let engine = open_engine(
        &dir.join("bundle"),
        &dir.join("cache"),
        &dir.join("wal.log"),
    );
    let credential = format!(
        "{{\"terms\": [{}]}}",
        terms
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let session = engine
        .authorise(credential.as_bytes())
        .expect("a credential of known descriptors authorises");
    let cardinality = session.fragment.view().cardinality();
    cardinality
}

/// The whole of the field route's semantics, in one corpus, under `default = "public"`.
#[test]
fn a_field_sourced_view_masks_on_the_terms_its_rows_carry() {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"), access_of);
    build(&args(
        &dir.path().join("points.parquet"),
        &dir.path().join("bundle"),
        "public",
    ))
    .expect("a field-sourced view builds");

    // A credential carrying nothing at all still holds `public`, which is added by the engine and
    // not by the credential: points 1 and 2 took the fill, point 3 wrote the label itself after a
    // trim. Points 0 and 4 carry their own terms and are **not** also given the default.
    assert_eq!(visible(dir.path(), &[]), 3, "public, and only public");
    // The trim: the credential spells the label without the padding the file carried.
    assert_eq!(visible(dir.path(), &["public"]), 3, "one term, not two");
    // Two more, and no double-counting of the point that carries both descriptors.
    assert_eq!(visible(dir.path(), &["ir:analyst"]), 5);
    assert_eq!(visible(dir.path(), &["ir:legal"]), 4);
    // A descriptor no point carries adds nothing, and takes nothing away.
    assert_eq!(visible(dir.path(), &["ir:nobody"]), 3);
}

/// **Null is not unrestricted.** Under a default no principal holds, the rows it fills are visible
/// to nobody — which is the reading that makes a fill safe in the first place.
#[test]
fn a_null_row_is_visible_to_no_principal_when_the_default_is_not_public() {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"), access_of);
    build(&args(
        &dir.path().join("points.parquet"),
        &dir.path().join("bundle"),
        "ir:sealed",
    ))
    .expect("a field-sourced view builds");

    // Only point 3, which wrote `public` itself.
    assert_eq!(visible(dir.path(), &[]), 1);
    // The two filled rows are reachable by the declared label and by nothing else.
    assert_eq!(visible(dir.path(), &["ir:sealed"]), 3);
    assert_eq!(visible(dir.path(), &["ir:analyst"]), 3);
}

/// A view declaring only a `default` — the corpus with no permission model — gives every point
/// that one label and nothing else.
#[test]
fn a_default_alone_gives_every_point_the_declared_label() {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"), access_of);
    let mut args = args(
        &dir.path().join("points.parquet"),
        &dir.path().join("bundle"),
        "public",
    );
    args.views[0].access.source = AccessSource::Default;
    build(&args).expect("a default-only view builds");
    assert_eq!(visible(dir.path(), &[]), N);
}

/// **A comma is an ordinary byte in a term.** The build hands the plugin the caller's terms as a
/// *list*, so a term spelling `ir:analyst,ir:legal` interns as one descriptor and reaches exactly
/// the principal who holds that whole string — never the holder of either half, which is what a
/// comma-joined label would have done.
#[test]
fn a_term_containing_a_comma_interns_as_one_term() {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"), |e| {
        if e == 2 {
            Some(vec!["ir:analyst,ir:legal"])
        } else {
            access_of(e)
        }
    });
    build(&args(
        &dir.path().join("points.parquet"),
        &dir.path().join("bundle"),
        "public",
    ))
    .expect("a term carrying a comma builds");

    // Points 1 (null, filled) and 3 (`public`) and nothing else.
    assert_eq!(visible(dir.path(), &[]), 2);
    // Neither half reaches point 2 — the whole of the point.
    assert_eq!(
        visible(dir.path(), &["ir:analyst"]),
        4,
        "public, point 0 and point 4 — not point 2"
    );
    assert_eq!(
        visible(dir.path(), &["ir:legal"]),
        3,
        "public and point 0 — not point 2"
    );
    // The whole string is the descriptor, and it reaches its one point.
    assert_eq!(visible(dir.path(), &["ir:analyst,ir:legal"]), 3);
}

/// A plain `string` column is the same declaration for a point carrying one term.
#[test]
fn a_plain_string_access_column_is_one_term_per_point() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("points.parquet");
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("categories", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let values: Vec<Option<&str>> = vec![Some("ir:analyst"), None, Some(""), Some("public"), None];
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(values)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(&path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();

    build(&args(&path, &dir.path().join("bundle"), "public")).expect("a string column builds");
    // The one point carrying a term of its own is not also given the default; the empty string is
    // no term at all, so that row is filled.
    assert_eq!(visible(dir.path(), &[]), 4);
    assert_eq!(visible(dir.path(), &["ir:analyst"]), 5);
}
