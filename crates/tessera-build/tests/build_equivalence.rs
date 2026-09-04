//! The load-bearing test for the streaming build: it must produce **byte-identical** bundles to
//! the linear reference build, and must be deterministic run to run.
//!
//! Entity IDs are assigned by signature order and are permanent under I9 — the ordering chosen
//! at the first build is the ordering the corpus keeps forever. A streaming build that ordered
//! items even slightly differently would not be a faster build, it would be a *different
//! corpus*, and every posting, permutation and handle derived from it would be invalid. Byte
//! equality against `build_in_memory` is how that is proved, and it covers the derived files
//! too: the dictionary's term-id assignment, the postings' tag choice and Roaring encoding, the
//! `(term, entity)` order in `pairs.parquet`, the external-ids byte sort, the tiler's
//! `(morton, priority, entity)` order and the permutation.
//!
//! Two files are compared after normalisation rather than raw: `MANIFEST.json` carries a
//! wall-clock `created_at`, and `CURRENT` carries that manifest's digest. Everything else,
//! including every digest `MANIFEST.json` records for every other file, is compared verbatim —
//! so a single differing byte anywhere in the bundle still fails here.
//!
//! **Where byte equality stops, and why.** Most fixtures here declare no attributes, and the
//! attributed pair (`attributed_*`) exists because a bundle carrying a scalar tail is a different
//! object to compare: `columns.arrow`, its `MANIFEST.declared_scalars` and its
//! `MANIFEST.vocabularies` are all derived files the two implementations could disagree on and
//! nothing else here would notice. Every vocabulary in that fixture is **fully seeded**, so no
//! code is minted and the bundle is reproducible. A *freshly minting* vocabulary is deliberately
//! out of scope: its codes are drawn from OS entropy at first build and are pinned rather than
//! reproduced (`tessera_store::vocabulary`), so two independent builds must differ and byte
//! equality is the wrong instrument. The equivalence that does hold there — the same key set, and
//! the same key per row — is asserted in `discovered_vocabulary.rs`'s
//! `both_implementations_agree_on_keys_though_fresh_codes_differ`. Threading a seeded RNG in to
//! close that gap is the thing that module's header exists to refuse.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    BooleanArray, Float64Array, StringArray, TimestampMicrosecondArray, UInt32Array, UInt64Array,
    UInt8Array,
};
use arrow::datatypes::{DataType, Field as ArrowField, Schema as ArrowSchema, TimeUnit};
use arrow::datatypes::{Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::{build, build_in_memory, BuildArgs};
use tessera_spatial::Bounds;
use tessera_types::IdentityKey;

const N_ITEMS: u64 = 4_000;
const N_TERMS: u64 = 61;

/// A fixed, non-degenerate test key shared by every fixture in this file — never a mint, since
/// the tests need a stable, reproducible identity to assert byte equality against.
const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

fn test_key() -> IdentityKey {
    IdentityKey::from_hex(TEST_KEY_HEX).unwrap()
}

fn extent() -> Bounds {
    Bounds {
        x_min: -20.0,
        x_max: 980.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

/// Deterministic synthetic input, shaped to exercise every branch the two builds could disagree
/// on: items with no terms at all, many items sharing an identical signature, signatures that
/// agree on their first two terms but differ later (the streaming build's pre-sort key ties),
/// signatures longer than two terms, duplicate `(entity, term)` rows, and terms whose first
/// appearance is late in entity order (which is what fixes their term id).
fn synth_terms(e: u64) -> Vec<u64> {
    match e % 13 {
        0 => vec![],
        1 => vec![7],
        2 => vec![7, 11],
        3 => vec![11, 7], // same set, different input order
        4 => vec![7, 11, 13],
        5 => vec![7, 11, 13, 17],
        6 => vec![7, 11, 19],
        7 => vec![7, 11, 13, 17, 19, 23, 29],
        8 => vec![(e / 97) % N_TERMS],
        9 => vec![41, 43],
        10 => vec![41, 43, (e / 31) % N_TERMS],
        11 => vec![N_TERMS - 1], // first appears late, so its term id is late
        _ => {
            let mut t = vec![e % 5, (e * 7) % N_TERMS, (e / 11) % N_TERMS];
            t.sort_unstable();
            t.dedup();
            t
        }
    }
}

/// The geometry every fixture in this file shares: `(source_id, x, y)`.
///
/// Source ids are deliberately neither dense nor in file order — the build must not depend on
/// either, and the ordinal space it derives has to be the sorted one — and coordinates repeat on
/// purpose, so the tiler's priority tiebreak is exercised.
fn synth_geometry() -> (Vec<u64>, Vec<f64>, Vec<f64>) {
    let ids: Vec<u64> = (0..N_ITEMS).map(|e| (e * 7919) % 1_000_003).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e % 40) * 25) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e % 37) * 27) as f64).collect();
    (ids, xs, ys)
}

fn write_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let (ids, xs, ys) = synth_geometry();
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
        let source_id = (e * 7919) % 1_000_003;
        for t in synth_terms(e) {
            entities.push(source_id);
            terms.push(t as u32);
            // Every twelfth item repeats its rows: the label set is a *set*, and a repeated
            // input row must not become a repeated posting in either build.
            if e % 12 == 0 {
                entities.push(source_id);
                terms.push(t as u32);
            }
        }
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

// ---- the attributed fixture --------------------------------------------------------------------
//
// The same geometry and the same terms, plus a scalar tail: one column per shape the two build
// implementations could encode differently, and two categories whose codes are pinned rather than
// drawn. See this file's header for why fresh minting is not compared here.

/// Every declared column, at every shape that costs a distinct branch. `department` is
/// `discovered` and `archive` is `declared`: the two kinds reach the column by different routes
/// (`BatchColumn::Discovered`, minted through a seeded `VocabularyMinter`, versus
/// `BatchColumn::Keys`, resolved per row), and only the declared one refuses an unknown key —
/// so a build that confused them would still produce a column, with different bytes in it.
///
/// Both carry `index` as well, which is what puts the two builds' filter postings under this
/// file's byte comparison. The emits are written from different shapes — the streaming build
/// reads its attribute values column-major, the reference transposes them out of the staged
/// items — and a keyed postings file that disagreed on code order or on which empty postings it
/// dropped would still open and still answer, just not the same set.
const ATTRIBUTED_SCHEMA: &str = r#"
[sources]
archive_values    = "archive-values.parquet"
department_values = "department-values.parquet"

[[vocabulary]]
name       = "archive"
width      = "u8"
value_set  = "closed"
# `public` is only what a closed set can safely publish (§3.8), so this fixture covers both.
visibility = "public"
source     = "archive_values"

[[vocabulary]]
name       = "department"
width      = "u16"
value_set  = "open"
visibility = "derived"
source     = "department_values"

[[attribute]]
name       = "archive"
type       = "category"
render     = true
index      = true
vocabulary = "archive"

[[attribute]]
name       = "department"
type       = "category"
render     = true
index      = true
vocabulary = "department"

[[attribute]]
name     = "author_count"
type     = "u8"
render = true

[[attribute]]
name     = "score"
type     = "f64"
render = true

[[attribute]]
name     = "submitted_at"
type     = "timestamp_us"
render = true

[[attribute]]
name     = "active"
type     = "bool"
render = true
"#;

/// The keys the attributed fixture uses, with **scattered** codes — a seed file is the one place
/// a test can pin codes, and pinning them densely would model something the minter never
/// produces (`tessera_store::vocabulary`: a dense code is a lower bound on cardinality).
const ARCHIVE_VALUES: &[(&str, u32)] = &[("astro-ph", 211), ("cs", 37), ("math", 149)];
const DEPARTMENT_VALUES: &[(&str, u32)] = &[
    ("eng", 40_351),
    ("finance", 8_803),
    ("legal", 61_129),
    ("ops", 22_477),
    ("sales", 1_559),
];

/// A bound vocabulary file: `key`/`code`, no `title` (configuration.md §1).
fn write_values(path: &Path, values: &[(&str, u32)]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("code", DataType::UInt32, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(
                values.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            )),
            Arc::new(UInt32Array::from(
                values.iter().map(|(_, c)| *c).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// Parse `ATTRIBUTED_SCHEMA` with both seed files written under `dir`.
///
/// **Both vocabularies are seeded with every key the data uses**, which is what keeps this
/// fixture byte-reproducible: a seeded key returns its pinned code without touching the draw, so
/// nothing here consumes entropy.
fn attributed_schema(dir: &Path) -> tessera_build::config::Schema {
    write_values(&dir.join("archive-values.parquet"), ARCHIVE_VALUES);
    write_values(&dir.join("department-values.parquet"), DEPARTMENT_VALUES);
    let schema_path = dir.join("config.toml");
    std::fs::write(&schema_path, ATTRIBUTED_SCHEMA).unwrap();
    // Each vocabulary names its own file, relative to this document (`configuration.md` §3), so
    // the fixture needs no bindings at all.
    tessera_build::config::Config::parse(&schema_path, &Default::default())
        .expect("the fixture schema parses")
        .schema
}

/// `write_points`'s geometry with `ATTRIBUTED_SCHEMA`'s six columns beside it.
///
/// Every column is nullable and every column has nulls, on a **different** stride per column, so
/// an implementation that lost the null mask, or applied one column's mask to another, produces
/// different bytes. For a category, absent is `ABSENT_CODE` rather than a value; for the rest it
/// is the scalar tail's own absent encoding — the two are separate paths and both are on the line
/// here.
fn write_attributed_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("archive", DataType::Utf8, true),
        Field::new("department", DataType::Utf8, true),
        Field::new("author_count", DataType::UInt8, true),
        Field::new("score", DataType::Float64, true),
        Field::new(
            "submitted_at",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
        Field::new("active", DataType::Boolean, true),
    ]));
    let (ids, xs, ys) = synth_geometry();
    let pick = |values: &[(&str, u32)], e: u64, absent_every: u64| -> Option<String> {
        (!e.is_multiple_of(absent_every))
            .then(|| values[(e % values.len() as u64) as usize].0.to_string())
    };
    let archives: Vec<Option<String>> = ids.iter().map(|&e| pick(ARCHIVE_VALUES, e, 7)).collect();
    let departments: Vec<Option<String>> = ids
        .iter()
        .map(|&e| pick(DEPARTMENT_VALUES, e, 11))
        .collect();
    let author_counts: Vec<Option<u8>> = ids
        .iter()
        .map(|&e| (!e.is_multiple_of(13)).then_some((e % 251) as u8))
        .collect();
    // Values a naive f32 round-trip would not return: the declaration is `f64` and the bytes must
    // be the caller's.
    let scores: Vec<Option<f64>> = ids
        .iter()
        .map(|&e| (!e.is_multiple_of(17)).then_some((e as f64) / 3.0))
        .collect();
    let submitted: Vec<Option<i64>> = ids
        .iter()
        .map(|&e| (!e.is_multiple_of(19)).then_some(1_600_000_000_000_000 + (e as i64) * 997))
        .collect();
    let active: Vec<Option<bool>> = ids
        .iter()
        .map(|&e| (!e.is_multiple_of(23)).then_some(e.is_multiple_of(3)))
        .collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(archives)),
            Arc::new(StringArray::from(departments)),
            Arc::new(UInt8Array::from(author_counts)),
            Arc::new(Float64Array::from(scores)),
            Arc::new(TimestampMicrosecondArray::from(submitted)),
            Arc::new(BooleanArray::from(active)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn args_for(points: &Path, pairs: &Path, out: PathBuf) -> BuildArgs {
    BuildArgs {
        arena_order: Default::default(),
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out,
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    }
}

/// Every file under `root`, keyed by its `root`-relative slash-separated path.
fn collect(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
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
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Compare two bundles file for file. `MANIFEST.json` is compared with its wall-clock
/// `created_at` blanked, and `CURRENT` (which is nothing but that manifest's digest) with it.
/// Every other file, including all the digests `MANIFEST.json` records, is compared verbatim.
fn assert_bundles_identical(left: &Path, right: &Path, what: &str) {
    let a = collect(left);
    let b = collect(right);
    assert_eq!(
        a.keys().collect::<Vec<_>>(),
        b.keys().collect::<Vec<_>>(),
        "{what}: the two bundles do not contain the same files"
    );
    for (name, left_bytes) in &a {
        let right_bytes = &b[name];
        if name == "v00000/MANIFEST.json" {
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
            continue; // the digest of a manifest that legitimately differs in created_at only
        }
        assert_eq!(
            left_bytes,
            right_bytes,
            "{what}: {name} is not byte-identical ({} vs {} bytes)",
            left_bytes.len(),
            right_bytes.len()
        );
    }
    // A bundle that only contained CURRENT and MANIFEST would pass the loop vacuously.
    assert!(
        a.len() > 6,
        "{what}: expected a full bundle, found {} files",
        a.len()
    );
}

#[test]
fn streaming_build_is_byte_identical_to_the_reference_build() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);

    let reference_out = temp.path().join("reference");
    let streaming_out = temp.path().join("streaming");
    let reference = build_in_memory(&args_for(&points, &pairs, reference_out.clone())).unwrap();
    let streaming = build(&args_for(&points, &pairs, streaming_out.clone())).unwrap();

    assert_eq!(reference.items, streaming.items);
    assert_eq!(reference.terms, streaming.terms);
    assert_eq!(reference.pairs, streaming.pairs);
    assert_eq!(reference.bundle_bytes, streaming.bundle_bytes);
    // **The occupancy figure is one of the report's numbers, not one path's.** It is counted at
    // each build's own segment write, and a resolution warning that appeared on one path and not
    // the other would be worse than none — which path ran is not something the caller chose.
    assert_eq!(reference.views[0].occupancy, streaming.views[0].occupancy);
    assert_eq!(reference.views[0].occupancy.points, reference.items);
    assert_bundles_identical(&reference_out, &streaming_out, "streaming vs reference");
}

/// **The same byte identity over a view whose access terms are a `list<string>` field**, where the
/// two implementations reach the dictionary by genuinely different routes.
///
/// The linear build walks items in source-id order and interns each descriptor as it meets it; the
/// streaming build ranks distinct terms by `(first ordinal, source term)` over a relation it scans
/// twice, a source term being a position in a sorted vocabulary. Those two agree only because the
/// vocabulary is sorted — which is the sort of premise a differential is for, since a disagreement
/// about term numbering is a disagreement about every permanent entity id (I9) and shows up as a
/// bundle that is well-formed and differently numbered.
#[test]
fn a_field_sourced_build_is_byte_identical_to_the_reference_build() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    write_field_sourced_points(&points);

    let mut reference_args = args_for(&points, &points, temp.path().join("reference"));
    reference_args.views[0].access = tessera_build::config::AccessInput {
        source: tessera_build::config::AccessSource::Field("categories".to_string()),
        default: "public".to_string(),
    };
    let mut streaming_args = reference_args.clone();
    streaming_args.out = temp.path().join("streaming");

    let reference = build_in_memory(&reference_args).unwrap();
    let streaming = build(&streaming_args).unwrap();
    assert_eq!(reference.items, streaming.items);
    assert_eq!(reference.terms, streaming.terms);
    assert_eq!(reference.pairs, streaming.pairs);
    assert_bundles_identical(
        &reference_args.out,
        &streaming_args.out,
        "field-sourced streaming vs reference",
    );
}

/// The field-route fixture: a `list<string>` access column carrying terms whose **lexicographic**
/// order and whose order of first appearance deliberately disagree, so a build ranking them by the
/// wrong one is caught rather than coincidentally right.
fn write_field_sourced_points(path: &Path) {
    use arrow::array::{ArrayRef, ListArray};
    use arrow::buffer::OffsetBuffer;
    let schema = Arc::new(ArrowSchema::new(vec![
        ArrowField::new("entity_id", DataType::UInt64, false),
        ArrowField::new("x", DataType::Float64, false),
        ArrowField::new("y", DataType::Float64, false),
        ArrowField::new(
            "categories",
            DataType::List(Arc::new(ArrowField::new("item", DataType::Utf8, true))),
            true,
        ),
    ]));
    let (ids, xs, ys) = synth_geometry();
    let vocabulary = ["zeta", "alpha", "mu", "beta", "public"];
    let mut offsets: Vec<i32> = vec![0];
    let mut flat: Vec<&str> = Vec::new();
    let mut present: Vec<bool> = Vec::new();
    for &e in &ids {
        // Every twelfth item carries nothing at all — null and empty alike, so the fill is on the
        // line here too — and the rest draw from the vocabulary in an order unrelated to its sort.
        match e % 12 {
            0 => present.push(false),
            1 => present.push(true),
            group => {
                present.push(true);
                for k in 0..(group % 3) + 1 {
                    flat.push(vocabulary[((group * 7 + k) % vocabulary.len() as u64) as usize]);
                }
            }
        }
        offsets.push(flat.len() as i32);
    }
    let values: ArrayRef = Arc::new(StringArray::from(flat));
    let list = ListArray::new(
        Arc::new(ArrowField::new("item", DataType::Utf8, true)),
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

/// **The same byte identity over a bundle that carries a scalar tail**, which no other fixture
/// here does — every other one builds with no attributes at all, so `columns.arrow`,
/// `declared_scalars` and `vocabularies` were never on the line between the two implementations.
///
/// The single-batch and batched cases run together because the batch seam is where the two are
/// most likely to part: a batch boundary splits the points file mid-corpus, and a scalar tail
/// assembled per batch has to reach the same column as one assembled in a single pass.
///
/// **What a differential cannot see.** The two implementations share `input::scan_attributes`,
/// so a decode both inherit — a null read as a value, a width taken from the data rather than the
/// declaration — produces the same wrong bytes twice and passes here. This test bounds the
/// *divergence* between the two assemblies (`pipeline::read_attributes_by_entity` against
/// `lib`'s staging pass), which is the part no other test covers; the shared decode is covered by
/// `input.rs`'s own cases and by the refusals in `schema.rs`.
#[test]
fn attributed_build_is_byte_identical_to_the_reference_build() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_attributed_points(&points);
    write_pairs(&pairs);

    let make_args = |out: PathBuf, batch: Option<u64>| {
        let mut args = args_for(&points, &pairs, out);
        args.schema = attributed_schema(temp.path());
        args.attribute_sources =
            tessera_build::config::AttributeSource::over(points.clone(), &args.schema);
        args.batch_items = batch;
        args
    };

    let reference_out = temp.path().join("reference");
    let streaming_out = temp.path().join("streaming");
    build_in_memory(&make_args(reference_out.clone(), None)).unwrap();
    build(&make_args(streaming_out.clone(), None)).unwrap();
    assert_bundles_identical(&reference_out, &streaming_out, "attributed, single batch");

    let batched_reference = temp.path().join("batched-reference");
    let batched_streaming = temp.path().join("batched-streaming");
    build_in_memory(&make_args(batched_reference.clone(), Some(2_100))).unwrap();
    build(&make_args(batched_streaming.clone(), Some(2_100))).unwrap();
    assert_bundles_identical(
        &batched_reference,
        &batched_streaming,
        "attributed, batched",
    );

    // The tail is genuinely present, read from the built artefact rather than from the fixture
    // that produced it: a schema silently dropped would leave every assertion above comparing two
    // bundles that agree because neither has any columns.
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(streaming_out.join("v00000/MANIFEST.json")).unwrap())
            .unwrap();
    let declared = manifest["declared_scalars"].as_array().unwrap();
    assert_eq!(
        declared.len(),
        6,
        "all six declared columns must reach the manifest, got {declared:?}"
    );
    let vocabularies = manifest["vocabularies"].as_array().unwrap();
    assert_eq!(vocabularies.len(), 2, "both vocabularies must be recorded");
    let files = collect(&streaming_out);
    let columns: Vec<_> = files
        .iter()
        .filter(|(path, _)| path.ends_with("columns.arrow"))
        .collect();
    assert!(
        !columns.is_empty(),
        "a declared scalar tail must produce a columns.arrow to compare; bundle holds {:?}",
        files.keys().collect::<Vec<_>>()
    );
    // Non-trivial: an empty column file would compare equal between the two builds while carrying
    // none of the six declarations.
    assert!(
        columns.iter().all(|(_, bytes)| bytes.len() > 1_024),
        "each columns.arrow must carry the tail, not just an Arrow IPC header"
    );
}

#[test]
fn streaming_build_is_deterministic() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);

    let first = temp.path().join("first");
    let second = temp.path().join("second");
    build(&args_for(&points, &pairs, first.clone())).unwrap();
    build(&args_for(&points, &pairs, second.clone())).unwrap();
    assert_bundles_identical(&first, &second, "run 1 vs run 2");
}

/// Signatures engineered around the streaming build's tie-group refinement, covering the group
/// shapes `synth_terms` only produces incidentally:
///
/// * an **all-long group** — prefix (1,2) is carried only by >2-term signatures, so its group
///   has no short member at all (the short/long partition must be a no-op at the front);
/// * long tails that are **prefixes of one another** ((1,2,3) vs (1,2,3,4) vs (1,2,3,4,5));
/// * **equal full signatures** among longs (the ordinal tiebreak inside the tail sort);
/// * a **mixed group** (5,6) with shorts and longs interleaved in ordinal order;
/// * a shorts-only group, a single-term group with several members, empty signatures, and an
///   item whose input rows arrive unsorted and duplicated.
fn shape_terms(e: u64) -> Vec<u64> {
    match e % 11 {
        0 => vec![1, 2, 3 + (e % 7)],
        1 => vec![1, 2, 3],
        2 => vec![1, 2, 3, 4],
        3 => vec![1, 2, 3, 4, 5],
        4 => vec![1, 2, 9, 10],
        5 => vec![5, 6],
        6 => vec![5, 6, 7 + (e % 5)],
        7 => vec![8, 9],
        8 => vec![11],
        9 => vec![],
        _ => vec![12, 3, 12], // unsorted, with a repeated term in the input rows
    }
}

const N_SHAPE_ITEMS: u64 = 3_300;

fn shape_source_id(e: u64) -> u64 {
    // Sparse, non-monotonic, arbitrary-looking: nothing about the ids' shape may matter.
    (e * 104_729) % 2_000_003
}

fn write_shape_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..N_SHAPE_ITEMS).map(shape_source_id).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e % 40) * 25) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e % 37) * 27) as f64).collect();
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

fn write_shape_pairs(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for e in 0..N_SHAPE_ITEMS {
        for t in shape_terms(e) {
            entities.push(shape_source_id(e));
            terms.push(t as u32);
        }
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

#[test]
fn tie_group_shapes_are_byte_identical_to_the_reference_build() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_shape_points(&points);
    write_shape_pairs(&pairs);

    let reference_out = temp.path().join("reference");
    let streaming_out = temp.path().join("streaming");
    build_in_memory(&args_for(&points, &pairs, reference_out.clone())).unwrap();
    build(&args_for(&points, &pairs, streaming_out.clone())).unwrap();
    assert_bundles_identical(&reference_out, &streaming_out, "tie-group shapes");
}

/// The spec-conformant default build — no minted external IDs (contracts §2.4), and here also
/// no oracle `pairs.parquet` — must hold the same byte-identity between the two
/// implementations, produce a bundle with no sidecar or pairs files at all, and still pass
/// `verify` (MANIFEST lists only what was written, so nothing is unverifiable).
#[test]
fn conformant_no_mint_no_pairs_build_is_byte_identical_and_verifiable() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);

    let make_args = |out: PathBuf| {
        let mut args = args_for(&points, &pairs, out);
        args.mint_external_ids = false;
        args.emit_oracle_pairs = false;
        args
    };

    let reference_out = temp.path().join("reference");
    let streaming_out = temp.path().join("streaming");
    build_in_memory(&make_args(reference_out.clone())).unwrap();
    build(&make_args(streaming_out.clone())).unwrap();
    assert_bundles_identical(&reference_out, &streaming_out, "conformant no-mint");

    let files = collect(&streaming_out);
    for name in files.keys() {
        assert!(
            !name.contains("external-ids-")
                && !name.contains("ext-locator")
                && !name.ends_with("pairs.parquet"),
            "a no-mint, no-oracle-pairs bundle must not contain {name}"
        );
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(&files["v00000/MANIFEST.json"]).unwrap();
    for listed in manifest["files"].as_object().unwrap().keys() {
        assert!(
            !listed.contains("external-ids-") && !listed.contains("ext-locator"),
            "MANIFEST must not list an unwritten file: {listed}"
        );
    }

    let report = tessera_build::verify(&streaming_out).unwrap();
    assert_eq!(report.rows, N_ITEMS);
}

/// Batch-scoped assignment (§11.1 r23): with an explicit batch size the streaming build must
/// (a) byte-match the reference build running the same per-chunk sort, (b) spill its buckets
/// and sweep multiple bands when forced (the band seam), (c) record the batch size in
/// MANIFEST provenance, and (d) stay deterministic. The batch splits ordinal space mid-corpus
/// — including an uneven tail — so cross-batch band emission, per-batch dedup, and the
/// entity-base continuation are all on the line.
#[test]
fn batched_build_is_byte_identical_to_the_batched_reference() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);

    // Two batches with an uneven tail (4000 = 2100 + 1900), a tiny budget that forces the
    // buckets to spill (but stays above the plan's floors), and bands of at most 400 pre-dedup
    // rows so the postings sweep runs several bands.
    let make_args = |out: PathBuf| {
        let mut args = args_for(&points, &pairs, out);
        args.batch_items = Some(2_100);
        args.memory_budget = Some(96 << 20);
        args.band_rows = Some(400);
        args
    };

    let reference_out = temp.path().join("reference");
    let streaming_out = temp.path().join("streaming");
    build_in_memory(&make_args(reference_out.clone())).unwrap();
    build(&make_args(streaming_out.clone())).unwrap();
    assert_bundles_identical(
        &reference_out,
        &streaming_out,
        "batched, forced spill+bands",
    );

    // Determinism of the batched path.
    let again = temp.path().join("again");
    build(&make_args(again.clone())).unwrap();
    assert_bundles_identical(&streaming_out, &again, "batched run 1 vs run 2");

    // The batch size is identity-bearing and must be recorded (and the single-batch builds
    // above must NOT record one — checked in the conformant test's manifest assertions).
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(streaming_out.join("v00000/MANIFEST.json")).unwrap())
            .unwrap();
    assert_eq!(
        manifest["provenance"]["batch_items"].as_u64(),
        Some(2_100),
        "a batched build must record its batch size in provenance"
    );
    assert!(
        !streaming_out.join(".build-tmp").exists(),
        "spill files must not survive a successful build"
    );

    // And the batching genuinely changed the assignment (the fragmentation is real, not a
    // no-op): the batched bundle differs from the single-batch one.
    let single = temp.path().join("single");
    build(&args_for(&points, &pairs, single.clone())).unwrap();
    let batched_perm =
        std::fs::read(streaming_out.join("v00000/partitions/default/views/s0/permutation.bin"))
            .unwrap();
    let single_perm =
        std::fs::read(single.join("v00000/partitions/default/views/s0/permutation.bin")).unwrap();
    assert_ne!(
        batched_perm, single_perm,
        "two batches must produce a different (per-batch) assignment than one"
    );
    let single_manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(single.join("v00000/MANIFEST.json")).unwrap())
            .unwrap();
    assert!(
        single_manifest["provenance"].get("batch_items").is_none(),
        "a single-batch build must not record a batch size"
    );
}

/// The plan's refusals are typed and arrive before any output: a batch size far below what the
/// budget supports permanently fragments posting runs and is refused (the operator states a
/// matching budget to make a small batch deliberate), and `--batch-items 0` is meaningless.
#[test]
fn needlessly_small_batches_are_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);

    let mut args = args_for(&points, &pairs, temp.path().join("out"));
    args.batch_items = Some(50); // 80 batches where the budget supports one
    let err = build(&args).unwrap_err().to_string();
    assert!(
        err.contains("fragment"),
        "refusal must explain the permanent fragmentation: {err}"
    );
    assert!(
        !temp.path().join("out").join("CURRENT").exists(),
        "a refused build must produce no bundle"
    );

    let mut zero = args_for(&points, &pairs, temp.path().join("out2"));
    zero.batch_items = Some(0);
    assert!(build(&zero).is_err());
}

/// The same equivalence under `--limit`, which selects a prefix of *source* entity space and so
/// changes which terms appear at all, and in what order they first appear.
#[test]
fn streaming_build_matches_the_reference_under_a_limit() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);

    let reference_out = temp.path().join("reference");
    let streaming_out = temp.path().join("streaming");
    let mut a = args_for(&points, &pairs, reference_out.clone());
    a.limit = Some(500_000);
    let mut b = args_for(&points, &pairs, streaming_out.clone());
    b.limit = Some(500_000);
    let reference = build_in_memory(&a).unwrap();
    let streaming = build(&b).unwrap();
    assert!(reference.items > 0 && reference.items < N_ITEMS);
    assert_eq!(reference.items, streaming.items);
    assert_bundles_identical(&reference_out, &streaming_out, "limited");
}

/// I9's assignment rule, asserted directly rather than only through byte equality: entity IDs
/// follow the signature order, so an item's signature is never lexicographically greater than
/// the next entity's, and items with identical signatures occupy a contiguous entity range.
#[test]
fn entity_ids_follow_signature_order() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);
    let out = temp.path().join("bundle");
    build(&args_for(&points, &pairs, out.clone())).unwrap();

    // Recover each entity's signature from pairs.parquet, which is the (term, entity) relation.
    let bundle = tessera_store::read::open_bundle(&out).unwrap();
    let partition = bundle.partitions.values().next().unwrap();
    let n = partition.manifest.entity_id_high_water as usize;
    let pairs_path = out
        .join("v00000")
        .join("partitions")
        .join("default")
        .join("terms")
        .join("pairs.parquet");
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        File::open(&pairs_path).unwrap(),
    )
    .unwrap()
    .build()
    .unwrap();
    let mut signatures: Vec<Vec<u32>> = vec![Vec::new(); n];
    for batch in reader {
        let batch = batch.unwrap();
        let entities = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        let terms = batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        for i in 0..batch.num_rows() {
            signatures[entities.value(i) as usize].push(terms.value(i));
        }
    }
    for signature in &mut signatures {
        signature.sort_unstable();
    }
    for entity in 1..n {
        assert!(
            signatures[entity - 1] <= signatures[entity],
            "entity {entity} breaks the signature order: {:?} then {:?}",
            signatures[entity - 1],
            signatures[entity]
        );
    }
    // The point of the ordering: identical signatures form runs, which is what compresses the
    // postings — measured at 8.9-36.7x (`probes/results.md`).
    let distinct = signatures.windows(2).filter(|w| w[0] != w[1]).count() + 1;
    assert!(
        distinct < n / 4,
        "expected identical signatures to be contiguous runs, saw {distinct} runs over {n} items"
    );
}

/// A manual scale check, ignored by default: builds the probe corpus prefix through the
/// **reference** path so its peak RSS can be compared against the streaming one under
/// `/usr/bin/time -v`. Run as
/// `cargo test --release -p tessera-build --test build_equivalence -- --ignored reference_build_at_scale`.
#[test]
#[ignore = "reads the probe corpus; run manually for a memory comparison"]
fn reference_build_at_scale() {
    let limit: u64 = std::env::var("TESSERA_SCALE_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_400_000);
    let out = PathBuf::from("/tmp/tessera-reference-scale");
    let _ = std::fs::remove_dir_all(&out);
    let report = build_in_memory(&BuildArgs {
        arena_order: Default::default(),
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: Bounds {
                x_min: 0.0,
                x_max: 65536.0,
                y_min: 0.0,
                y_max: 65536.0,
            },
            points: PathBuf::from("data/scaled/geometry.parquet"),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(PathBuf::from(
                "data/scaled/pairs/categories-subclass.pairs.parquet",
            )),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out,
        limit: Some(limit),
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .unwrap();
    assert_eq!(report.items, limit);
}
