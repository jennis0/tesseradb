//! **Filtering, end to end**: a built bundle, a real principal's mask, a real deny, and the answer
//! checked against a brute-force reference computed from the source data.
//!
//! Everything the filter index had been tested with before this was a hand-made candidate bitmap
//! over a single build segment. That covers the scan and misses the two things most likely to be
//! wrong: whether the candidate a session actually produces is the *composed* one, and whether the
//! result agrees with what the corpus says once permissions are applied.
//!
//! **The reference is computed from the fixture, not from the artefact.** Every assertion works out
//! the expected set from `terms_of` and the source attribute values — the same inputs the build
//! was given — so an implementation that stored the wrong thing and then read it back consistently
//! fails here. Comparing the index against itself is the failure mode this file exists to avoid.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use croaring::Bitmap;
use tessera_build::schema::Schema;
use tessera_build::{build, BuildArgs};
use tessera_engine::filter::{candidate, FilterColumns, FilterOperand};
use tessera_store::read::open_bundle;
use tessera_types::AttrLocalId;

const N: u64 = 60;

/// A `u8` category and a per-item string. The category is `per_viewer`, which is the shape that
/// also owes membership postings; the string is `filter`-only, which is the shape that owes none.
const SCHEMA_TOML: &str = r#"
[[attribute]]
name       = "department"
type       = "category"
width      = "u8"
used_for   = ["render", "filter"]
vocabulary = "declared"
listing    = "per_viewer"
  [attribute.values]
  eng = 1
  sales = 2
  legal = 3

[[attribute]]
name     = "title"
type     = "utf8"
used_for = ["filter"]
"#;

/// Source id → department key. Every fifth item carries none, so the absent path is exercised
/// against a real build rather than only in a unit test.
///
/// **Deliberately decorrelated from the permission model.** `terms_of` grants `SUBSET_TERM` on
/// `e % 3`, so a department keyed on `e % 3` would make every "eng" item exactly the set the subset
/// principal can see — and every cross-principal assertion would pass while proving nothing, because
/// masking and filtering would be selecting the same items for different reasons. `e / 2 % 3` shares
/// no factor with the term model, so the two genuinely cut across each other.
fn department_of(e: u64) -> Option<&'static str> {
    if e.is_multiple_of(5) {
        None
    } else {
        Some(["eng", "sales", "legal"][(e / 2 % 3) as usize])
    }
}

fn title_of(e: u64) -> String {
    format!("paper-{e:02}")
}

fn write_points(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("department", DataType::Utf8, true),
        Field::new("title", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let departments: Vec<Option<String>> = ids
        .iter()
        .map(|&e| department_of(e).map(|s| s.to_string()))
        .collect();
    let titles: Vec<Option<String>> = ids.iter().map(|&e| Some(title_of(e))).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(departments)),
            Arc::new(StringArray::from(titles)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// The same term model `common` uses — every item carries `ALL_TERM`, every third also
/// `SUBSET_TERM` — so `subset_credential()` is a principal seeing one item in three.
fn write_pairs(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities: Vec<u64> = Vec::new();
    let mut terms: Vec<u32> = Vec::new();
    for e in 0..N {
        for t in terms_of(e) {
            entities.push(e);
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

struct Fixture {
    _dir: tempfile::TempDir,
    bundle: std::path::PathBuf,
    /// Source id → entity id. Entity ids are signature-sorted, so a source id is not its own.
    entity_of: BTreeMap<u64, u64>,
    columns: FilterColumns,
    codes: HashMap<String, u32>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);
    let bundle = dir.path().join("bundle");

    let schema_path = dir.path().join("schema.toml");
    std::fs::write(&schema_path, SCHEMA_TOML).unwrap();
    let schema = Schema::parse(&schema_path, &HashMap::new()).unwrap();

    build(&BuildArgs {
        points: points.clone(),
        pairs: pairs.clone(),
        out: bundle.clone(),
        extent: extent(),
        slice_id: "s0".to_string(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: "000102030405060708090a0b0c0d0e0f".to_string(),
        idset: 1,
        shard_id: 0,
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    })
    .expect("the fixture builds");

    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle.join("CURRENT")).unwrap()).unwrap();
    let prefix = current["prefix"].as_str().unwrap().to_string();
    let entity_of = source_to_new_map(&bundle, &prefix);

    let opened = open_bundle(&bundle).unwrap();
    let phash = opened.partitions.keys().next().unwrap().clone();
    let partition_dir = bundle.join(&prefix).join("partitions").join(&phash);
    let columns = FilterColumns::open(&partition_dir, &opened.manifest.declared_scalars)
        .expect("declared filter columns open");
    let codes = opened
        .manifest
        .vocabularies
        .iter()
        .find(|v| v.name == "department")
        .expect("the manifest records the vocabulary")
        .values
        .iter()
        .map(|v| (v.key.clone(), v.code))
        .collect();

    Fixture {
        _dir: dir,
        bundle,
        entity_of,
        columns,
        codes,
    }
}

/// The brute-force reference: source ids satisfying `pred`, restricted to those a principal holding
/// `terms` can see, mapped to entity ids.
fn expected(fx: &Fixture, terms: &[u64], pred: impl Fn(u64) -> bool) -> Vec<u32> {
    let mut v: Vec<u32> = (0..N)
        .filter(|&e| terms_of(e).iter().any(|t| terms.contains(t)))
        .filter(|&e| pred(e))
        .map(|e| fx.entity_of[&e] as u32)
        .collect();
    v.sort_unstable();
    v
}

fn as_vec(b: &Bitmap) -> Vec<u32> {
    b.iter().collect()
}

/// Open an engine, authorise, and take the composed entity-space candidate the design requires.
fn candidate_for(fx: &Fixture, credential: &[u8]) -> (tessera_engine::Engine, Bitmap) {
    let cache = fx._dir.path().join(format!("cache-{}", credential.len()));
    let wal = fx._dir.path().join(format!("wal-{}", credential.len()));
    let engine = open_engine(&fx.bundle, &cache, &wal);
    let session = engine.authorise(credential).expect("the credential resolves");
    let generation = engine.generation();
    let cand = candidate(
        &session.fragment,
        &session.satisfied,
        &generation.overlay,
        &generation.buffer,
    );
    (engine, cand)
}

/// **A filter result is what the corpus says, restricted to what the principal may see.** The
/// reference comes from the fixture's own inputs, not from the index.
#[test]
fn a_category_filter_agrees_with_the_corpus_under_a_real_mask() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &subset_credential());

    let eng = AttrLocalId::new(fx.codes["eng"]);
    let got = fx
        .columns
        .resolve("department", &FilterOperand::Equals(eng), &cand)
        .expect("department is filterable");

    assert_eq!(
        as_vec(&got),
        expected(&fx, &[SUBSET_TERM], |e| department_of(e) == Some("eng")),
        "the filtered set must be exactly the corpus's, masked"
    );
    // And it is inside the candidate — I12, asserted rather than assumed.
    assert!(got.andnot(&cand).is_empty());
}

/// The **same operand under a wider principal returns a strict superset**, and the narrower result
/// is exactly the wider one intersected with the narrower mask. A filter that leaked would break
/// this by returning something the subset principal cannot account for.
#[test]
fn a_wider_principal_sees_a_superset_of_the_same_filter() {
    let fx = fixture();
    let (_e1, narrow) = candidate_for(&fx, &subset_credential());
    let (_e2, wide) = candidate_for(&fx, &full_coverage_credential());

    let eng = AttrLocalId::new(fx.codes["eng"]);
    let narrow_hits = fx
        .columns
        .resolve("department", &FilterOperand::Equals(eng), &narrow)
        .unwrap();
    let wide_hits = fx
        .columns
        .resolve("department", &FilterOperand::Equals(eng), &wide)
        .unwrap();

    assert!(narrow_hits.andnot(&wide_hits).is_empty(), "not a subset");
    assert_eq!(as_vec(&narrow_hits), as_vec(&wide_hits.and(&narrow)));
    // Guards against a vacuous fixture: if the attribute correlated with the permission term the
    // two sets would be equal, both assertions above would hold, and neither would mean anything.
    assert!(
        wide_hits.cardinality() > narrow_hits.cardinality(),
        "the fixture must make masking and filtering select different items"
    );
    assert!(narrow_hits.cardinality() > 0, "and the narrow set must be non-empty");
}

/// **A principal who can see nothing gets nothing**, for every operand — including one naming a
/// value that certainly exists. This is the shape an index that filtered *after* answering would
/// get wrong.
#[test]
fn a_zero_coverage_principal_matches_nothing() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &zero_credential());
    assert!(cand.is_empty());

    for operand in [
        FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"])),
        FilterOperand::TextPrefix("paper".to_string()),
        FilterOperand::TextContains("-".to_string()),
    ] {
        let column = match operand {
            FilterOperand::Equals(_) => "department",
            _ => "title",
        };
        assert!(fx
            .columns
            .resolve(column, &operand, &cand)
            .unwrap()
            .is_empty());
    }
}

/// String predicates over a real mask: equality, prefix and substring, each against the corpus.
#[test]
fn string_filters_agree_with_the_corpus_under_a_real_mask() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &subset_credential());
    let terms = [SUBSET_TERM];

    let eq = fx
        .columns
        .resolve(
            "title",
            &FilterOperand::TextEquals(title_of(3)),
            &cand,
        )
        .unwrap();
    assert_eq!(as_vec(&eq), expected(&fx, &terms, |e| e == 3));

    let prefix = fx
        .columns
        .resolve("title", &FilterOperand::TextPrefix("paper-1".into()), &cand)
        .unwrap();
    assert_eq!(
        as_vec(&prefix),
        expected(&fx, &terms, |e| title_of(e).starts_with("paper-1"))
    );

    let contains = fx
        .columns
        .resolve("title", &FilterOperand::TextContains("-2".into()), &cand)
        .unwrap();
    assert_eq!(
        as_vec(&contains),
        expected(&fx, &terms, |e| title_of(e).contains("-2"))
    );
}

/// **Composition is intersection, and it commutes with the corpus.** Two operands over different
/// columns give exactly the items satisfying both.
#[test]
fn two_operands_compose_by_intersection() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &full_coverage_credential());

    let eng = FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"]));
    let prefix = FilterOperand::TextPrefix("paper-1".to_string());
    let got = fx
        .columns
        .resolve_all([("department", &eng), ("title", &prefix)], &cand)
        .unwrap();

    assert_eq!(
        as_vec(&got),
        expected(&fx, &[ALL_TERM], |e| department_of(e) == Some("eng")
            && title_of(e).starts_with("paper-1"))
    );
}

/// **An item with no value matches no operand over that column** — not equality, not a prefix that
/// would match the empty string. The absent path against a real build.
#[test]
fn an_item_with_no_category_matches_no_category_operand() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &full_coverage_credential());

    let mut union = Bitmap::new();
    for code in fx.codes.values() {
        union |= fx
            .columns
            .resolve(
                "department",
                &FilterOperand::Equals(AttrLocalId::new(*code)),
                &cand,
            )
            .unwrap();
    }
    for source in (0..N).filter(|e| department_of(*e).is_none()) {
        assert!(
            !union.contains(fx.entity_of[&source] as u32),
            "source {source} carries no department"
        );
    }
}

/// **An undeclared column is `None`, not an empty result.** A caller naming a column the schema
/// never made filterable is a caller error; an empty answer would be indistinguishable from a
/// correctly-computed one.
#[test]
fn an_undeclared_column_is_distinguishable_from_an_empty_result() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &full_coverage_credential());
    assert!(fx
        .columns
        .resolve("no_such_column", &FilterOperand::TextPrefix("x".into()), &cand)
        .is_none());
    assert!(fx
        .columns
        .resolve("title", &FilterOperand::TextPrefix("zzz".into()), &cand)
        .is_some_and(|b| b.is_empty()));
}
