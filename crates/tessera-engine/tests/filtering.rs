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
use tessera_engine::filter::{
    candidate, Endpoint, FilterColumns, FilterError, FilterExpr, FilterOperand, Scalar,
};
use tessera_engine::ViewportRequest;
use tessera_store::read::open_bundle;
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::WalScalar;
use tessera_types::AttrLocalId;

const N: u64 = 60;
/// The whole declared extent, so a depth-0 request covers every item.
const FULL_VIEWPORT: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

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

[[attribute]]
name     = "score"
type     = "i32"
used_for = ["render", "filter"]
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

/// A numeric column, decorrelated from both the term model and the department cycle.
fn score_of(e: u64) -> i32 {
    (e as i32 * 7) % 100
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
        Field::new("score", DataType::Int32, false),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let departments: Vec<Option<String>> = ids
        .iter()
        .map(|&e| department_of(e).map(|s| s.to_string()))
        .collect();
    let titles: Vec<Option<String>> = ids.iter().map(|&e| Some(title_of(e))).collect();
    let scores: Vec<i32> = ids.iter().map(|&e| score_of(e)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(departments)),
            Arc::new(StringArray::from(titles)),
            Arc::new(arrow::array::Int32Array::from(scores)),
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
    let columns = FilterColumns::open(
        &partition_dir,
        &opened.manifest.declared_scalars,
        opened.manifest.entity_id_high_water as u32,
    )
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

fn leaf(column: &str, operand: FilterOperand) -> FilterExpr {
    FilterExpr::Leaf {
        column: column.to_string(),
        operand,
    }
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
        .evaluate(
            &FilterExpr::AllOf(vec![leaf("department", eng), leaf("title", prefix)]),
            &cand,
        )
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
    assert!(matches!(
        fx.columns
            .resolve("no_such_column", &FilterOperand::TextPrefix("x".into()), &cand),
        Err(FilterError::UndeclaredColumn(ref c)) if c == "no_such_column"
    ));
    assert!(fx
        .columns
        .resolve("title", &FilterOperand::TextPrefix("zzz".into()), &cand)
        .is_ok_and(|b| b.is_empty()));
}

/// **⊘ A filter refuses once the candidate reaches entities the artefact does not cover.**
///
/// A flush appends entities. `filter-index.md` §2.1 specifies a value-column extent per flush and
/// nothing emits one, so an entity allocated since the build carries no filter values at all.
/// Answering anyway would omit it — safe under I12, since it only narrows `M_sel`, and *wrong* in a
/// way the caller cannot detect, because a short result looks exactly like a correct one.
///
/// The refusal is as narrow as the gap: a principal whose visible set predates the build is still
/// answered normally, which the second half of this test pins so the guard cannot quietly become
/// "refuse everything once anything has flushed".
#[test]
fn a_filter_refuses_rather_than_answering_short_after_a_flush() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-flush");
    let wal = fx._dir.path().join("wal-flush");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);

    let row = UnallocatedRow {
        external_id: Some(b"post-build".to_vec()),
        slice: "s0".to_string(),
        descriptors: vec![b"0".to_vec()],
        x: 5.0,
        y: 5.0,
        // One per declared column, positionally — including the `filter`-only `title`, which is
        // why `declared_scalars` keeps the full list while the *segment* narrows to render columns.
        // A category arrives as its **key**, never a code (contracts §2.4).
        scalars: vec![
            WalScalar::Utf8("eng".to_string()),
            WalScalar::Utf8("paper-99".to_string()),
            WalScalar::I32(42),
        ],
        terms: engine.resolve_terms(&[b"0".to_vec()]),
    };
    let allocated = engine
        .accept_ingest(vec![row], "batch-1".to_string(), [0u8; 32])
        .expect("ingest is accepted")[0];
    assert!(
        allocated.raw() >= N,
        "the new entity is above the build's high-water"
    );

    let session = engine
        .authorise(&full_coverage_credential())
        .expect("credential resolves");
    let generation = engine.generation();
    let cand = candidate(
        &session.fragment,
        &session.satisfied,
        &generation.overlay,
        &generation.buffer,
    );
    assert!(
        cand.contains(allocated.raw() as u32),
        "the ingested entity is in the candidate — it is visible, it simply has no filter values"
    );

    let eng = AttrLocalId::new(fx.codes["eng"]);
    let err = fx
        .columns
        .resolve("department", &FilterOperand::Equals(eng), &cand)
        .expect_err("a candidate past the artefact must be refused");
    assert!(
        matches!(err, FilterError::CoverageEndsAtBuild { .. }),
        "{err:?}"
    );
    // The message names what is absent rather than refusing generically (decision 0013).
    assert!(format!("{err}").contains("specified and not built"));

    // And the guard is narrow: a candidate entirely below the build's high-water still answers.
    let below = cand.and(&Bitmap::from_range(0..N as u32));
    assert!(fx
        .columns
        .resolve("department", &FilterOperand::Equals(eng), &below)
        .is_ok());
}

/// **A row carrying the wrong number of scalars is refused, not a panic.**
///
/// The commit window indexes `row.scalars` positionally against `declared_scalars` to find a
/// category key's vocabulary, so a short row indexed out of bounds — panicking inside the write
/// executor and reaching the caller as a lost receipt, which reads as an infrastructure fault
/// rather than the malformed request it is.
#[test]
fn a_row_with_the_wrong_scalar_count_is_refused_rather_than_panicking() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-arity");
    let wal = fx._dir.path().join("wal-arity");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);

    let short = UnallocatedRow {
        external_id: Some(b"short".to_vec()),
        slice: "s0".to_string(),
        descriptors: vec![b"0".to_vec()],
        x: 5.0,
        y: 5.0,
        // The schema declares three columns.
        scalars: vec![WalScalar::Utf8("eng".to_string())],
        terms: engine.resolve_terms(&[b"0".to_vec()]),
    };
    let err = engine
        .accept_ingest(vec![short], "batch-short".to_string(), [1u8; 32])
        .expect_err("a short row is refused");
    assert!(
        format!("{err}").contains("carries 1 scalars, but the schema declares 3"),
        "{err}"
    );

    // The engine is still usable — a refusal before the submit acks nothing, burns no entity id
    // (I9) and leaves the executor running.
    let good = UnallocatedRow {
        external_id: Some(b"good".to_vec()),
        slice: "s0".to_string(),
        descriptors: vec![b"0".to_vec()],
        x: 5.0,
        y: 5.0,
        scalars: vec![
            WalScalar::Utf8("eng".to_string()),
            WalScalar::Utf8("paper-98".to_string()),
            WalScalar::I32(43),
        ],
        terms: engine.resolve_terms(&[b"0".to_vec()]),
    };
    assert!(engine
        .accept_ingest(vec![good], "batch-good".to_string(), [2u8; 32])
        .is_ok());
}

/// **A filtered viewport returns only matching marks — end to end, through the engine.**
///
/// This is the first assertion that the filter reaches the *served* answer rather than an
/// entity-space bitmap a test built itself. The expected set comes from the fixture's inputs, and
/// the comparison is on `tessera_id` rather than row, because the served order is the engine's.
#[test]
fn a_filtered_viewport_serves_only_matching_marks() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-vp");
    let wal = fx._dir.path().join("wal-vp");
    let engine = open_engine_uncapped(&fx.bundle, &cache, &wal);
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    let unfiltered = engine
        .viewport(&session, ViewportRequest::new("s0", 0, FULL_VIEWPORT, 10_000))
        .expect("an unfiltered viewport answers");

    let eng = FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"]));
    let filtered = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, FULL_VIEWPORT, 10_000)
                .filter(leaf("department", eng)),
        )
        .expect("a filtered viewport answers");

    let expected_count = (0..N)
        .filter(|&e| department_of(e) == Some("eng"))
        .count() as u64;
    assert!(expected_count > 0 && expected_count < N, "a real subset");

    assert_eq!(
        filtered.points.len() as u64,
        expected_count,
        "the filtered viewport draws exactly the matching items"
    );
    assert_eq!(unfiltered.points.len() as u64, N);

    // Every filtered mark is one the unfiltered request also served — a filter narrows and never
    // widens (I12), asserted on the served identities rather than on counts alone.
    let unfiltered_ids: std::collections::HashSet<u64> =
        unfiltered.points.tessera_ids.iter().copied().collect();
    for id in &filtered.points.tessera_ids {
        assert!(unfiltered_ids.contains(id));
    }
}

/// **The selection threshold stays anchored on the unfiltered total** (§8.4, I12). A filter may
/// move the frontier up, never down — so the anchor a filtered request uses is the same one the
/// unfiltered request uses, and a viewer typing does not coarsen their own map.
#[test]
fn a_filter_does_not_move_the_threshold_anchor() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-theta");
    let wal = fx._dir.path().join("wal-theta");
    let engine = open_engine_uncapped(&fx.bundle, &cache, &wal);
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    let unfiltered = engine
        .viewport(&session, ViewportRequest::new("s0", 0, FULL_VIEWPORT, 10_000))
        .unwrap();
    let filtered = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, FULL_VIEWPORT, 10_000).filter(leaf(
                "department",
                FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"])),
            )),
        )
        .unwrap();

    let sum = |v: &[tessera_engine::TileCount], f: fn(&tessera_engine::TileCount) -> u64| -> u64 {
        v.iter().map(f).sum()
    };
    // `visible` is the composed count and must not move with the filter — that is what keeps θ's
    // anchor unfiltered, and with it I12's "a filter moves the frontier up, never down".
    assert_eq!(
        sum(&filtered.tiles, |t| t.visible),
        sum(&unfiltered.tiles, |t| t.visible),
        "`visible` must not move with the filter"
    );
    // `matched` is what does move, and it is the filtered figure.
    assert_eq!(
        sum(&filtered.tiles, |t| t.matched),
        (0..N).filter(|&e| department_of(e) == Some("eng")).count() as u64
    );
    assert_eq!(
        sum(&unfiltered.tiles, |t| t.matched),
        sum(&unfiltered.tiles, |t| t.visible),
        "unfiltered, matched == visible"
    );
}

/// An undeclared column refuses the request rather than serving an empty viewport — an empty
/// answer is a real one, and must not stand in for one that could not be computed.
#[test]
fn a_viewport_naming_an_undeclared_column_is_refused() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-bad");
    let wal = fx._dir.path().join("wal-bad");
    let engine = open_engine_uncapped(&fx.bundle, &cache, &wal);
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    let err = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, FULL_VIEWPORT, 10_000)
                .filter(leaf("no_such_column", FilterOperand::TextPrefix("x".into()))),
        )
        .expect_err("an undeclared column is refused");
    assert!(format!("{err}").contains("not declared filterable"), "{err}");
}

/// **Disjunction across columns** — the case `in` cannot express, since `in` only ORs values within
/// one column. Checked against the corpus, not against the index.
#[test]
fn any_of_unions_across_columns() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &full_coverage_credential());

    let expr = FilterExpr::AnyOf(vec![
        leaf(
            "department",
            FilterOperand::Equals(AttrLocalId::new(fx.codes["legal"])),
        ),
        leaf("title", FilterOperand::TextPrefix("paper-1".into())),
    ]);
    let got = fx.columns.evaluate(&expr, &cand).unwrap();

    assert_eq!(
        as_vec(&got),
        expected(&fx, &[ALL_TERM], |e| department_of(e) == Some("legal")
            || title_of(e).starts_with("paper-1"))
    );
    // A union of subsets of the candidate is still a subset — I12 by shape rather than by check.
    assert!(got.andnot(&cand).is_empty());
}

/// Nesting: a conjunction one of whose clauses is a disjunction.
#[test]
fn a_conjunction_may_contain_a_disjunction() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &full_coverage_credential());

    let expr = FilterExpr::AllOf(vec![
        leaf("title", FilterOperand::TextContains("-1".into())),
        FilterExpr::AnyOf(vec![
            leaf(
                "department",
                FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"])),
            ),
            leaf(
                "department",
                FilterOperand::Equals(AttrLocalId::new(fx.codes["legal"])),
            ),
        ]),
    ]);
    let got = fx.columns.evaluate(&expr, &cand).unwrap();

    assert_eq!(
        as_vec(&got),
        expected(&fx, &[ALL_TERM], |e| title_of(e).contains("-1")
            && matches!(department_of(e), Some("eng") | Some("legal")))
    );
}

/// The two empty cases are the identities of their operators, and they differ. `all_of: []` is the
/// whole candidate — nothing was asked for, so nothing is excluded. `any_of: []` is empty — items
/// matching one of no alternatives. Stated because the asymmetry looks like a bug on sight.
#[test]
fn the_empty_combinators_are_their_operators_identities() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &subset_credential());

    assert_eq!(
        fx.columns
            .evaluate(&FilterExpr::AllOf(vec![]), &cand)
            .unwrap()
            .cardinality(),
        cand.cardinality()
    );
    assert!(fx
        .columns
        .evaluate(&FilterExpr::AnyOf(vec![]), &cand)
        .unwrap()
        .is_empty());
}

/// Nesting is bounded and **refused rather than flattened** — a silently flattened expression
/// answers a different question. Unbounded depth is unbounded per-request work from one
/// authenticated call, the argument §7.3's sub-cell budget already makes.
#[test]
fn an_over_deep_expression_is_refused() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &full_coverage_credential());

    let mut expr = leaf("title", FilterOperand::TextPrefix("p".into()));
    for _ in 0..8 {
        expr = FilterExpr::AllOf(vec![expr]);
    }
    let err = fx.columns.evaluate(&expr, &cand).expect_err("too deep");
    assert!(matches!(err, FilterError::TooDeep { .. }), "{err:?}");
    assert!(format!("{err}").contains("rather than flattened"));
}

/// A disjunction whose branches a principal cannot see is empty, not an error — and costs the same
/// as one they can, since every branch is evaluated under their own candidate.
#[test]
fn a_disjunction_stays_inside_a_narrow_principals_mask() {
    let fx = fixture();
    let (_engine, narrow) = candidate_for(&fx, &subset_credential());

    let expr = FilterExpr::AnyOf(vec![
        leaf(
            "department",
            FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"])),
        ),
        leaf(
            "department",
            FilterOperand::Equals(AttrLocalId::new(fx.codes["sales"])),
        ),
    ]);
    let got = fx.columns.evaluate(&expr, &narrow).unwrap();
    assert_eq!(
        as_vec(&got),
        expected(&fx, &[SUBSET_TERM], |e| matches!(
            department_of(e),
            Some("eng") | Some("sales")
        ))
    );
    assert!(got.andnot(&narrow).is_empty());
}

/// **A range is a scan, and it agrees with the corpus under a real mask.** No level tree, no bit
/// slicing, no zone map — `filter-index.md` §3 declines all three, zone maps outright because their
/// block skip consults unmasked extrema.
#[test]
fn a_numeric_range_agrees_with_the_corpus() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &subset_credential());

    // [40, 70)
    let expr = leaf(
        "score",
        FilterOperand::Range {
            lo: Some(Endpoint {
                value: Scalar::Int(40),
                inclusive: true,
            }),
            hi: Some(Endpoint {
                value: Scalar::Int(70),
                inclusive: false,
            }),
        },
    );
    let got = fx.columns.evaluate(&expr, &cand).unwrap();
    assert_eq!(
        as_vec(&got),
        expected(&fx, &[SUBSET_TERM], |e| (40..70).contains(&score_of(e)))
    );
    assert!(got.andnot(&cand).is_empty());
}

/// An open side is a bound on one end only.
#[test]
fn an_open_ended_range_bounds_one_side() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &full_coverage_credential());
    let expr = leaf(
        "score",
        FilterOperand::Range {
            lo: Some(Endpoint {
                value: Scalar::Int(90),
                inclusive: true,
            }),
            hi: None,
        },
    );
    assert_eq!(
        as_vec(&fx.columns.evaluate(&expr, &cand).unwrap()),
        expected(&fx, &[ALL_TERM], |e| score_of(e) >= 90)
    );
}

/// A range composes with the other families by the same combinators.
#[test]
fn a_range_composes_with_a_category_and_a_string() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &full_coverage_credential());
    let expr = FilterExpr::AllOf(vec![
        leaf(
            "score",
            FilterOperand::Range {
                lo: Some(Endpoint {
                    value: Scalar::Int(50),
                    inclusive: true,
                }),
                hi: None,
            },
        ),
        FilterExpr::AnyOf(vec![
            leaf(
                "department",
                FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"])),
            ),
            leaf("title", FilterOperand::TextPrefix("paper-1".into())),
        ]),
    ]);
    assert_eq!(
        as_vec(&fx.columns.evaluate(&expr, &cand).unwrap()),
        expected(&fx, &[ALL_TERM], |e| score_of(e) >= 50
            && (department_of(e) == Some("eng") || title_of(e).starts_with("paper-1")))
    );
}
