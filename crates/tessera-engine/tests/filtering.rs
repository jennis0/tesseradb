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
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::WalScalar;
use tessera_store::read::open_bundle;
use tessera_types::AttrLocalId;

const N: u64 = 60;
/// The whole declared extent, so a depth-0 request covers every item.
const FULL_VIEWPORT: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

/// Two `u8` categories and a per-item string. `department` is `per_viewer`, which is the shape that
/// owes membership postings *and* keeps the scan for filtering (decision 0063); `archive` is
/// `public`, which is the shape whose filter is routed through those postings. The two carry the
/// same value distribution under different names, so the routed answer and the scanned one are
/// comparable value by value. The string is `index`-only, which is the shape that owes no postings
/// at all.
///
/// `score` is `index`-only where it was once rendered-and-filterable: `index` on a rendered
/// number is refused until decision 0064's render half lands (records §6.2 — review X3's named
/// regression), and what this file exercises is the filter, which the entity-space column
/// serves either way.
///
/// `ops` and `ww` are declared and carried by nothing, which is the empty-value case: a code the
/// vocabulary binds, that no posting holds, and that no principal may be offered until something
/// carries it.
const SCHEMA_TOML: &str = r#"
[[attribute]]
name       = "department"
type       = "category"
width      = "u8"
render     = true
index      = true
vocabulary = "declared"
listing    = "per_viewer"
  [attribute.values]
  eng = 1
  sales = 2
  legal = 3
  ops = 4

[[attribute]]
name       = "archive"
type       = "category"
width      = "u8"
render     = true
index      = true
vocabulary = "declared"
listing    = "public"
  [attribute.values]
  xx = 11
  yy = 22
  zz = 33
  ww = 44

[[attribute]]
name     = "title"
type     = "utf8"
index    = true

[[attribute]]
name     = "score"
type     = "i32"
index    = true

[[attribute]]
name     = "bonus"
type     = "i32"
index    = true
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

/// The `public` column's value, one-to-one with the `per_viewer` one's. Two columns carrying the
/// same partition of the corpus under different `listing`s is what makes the routed answer and the
/// scanned answer comparable set for set.
fn archive_of(e: u64) -> Option<&'static str> {
    match department_of(e) {
        Some("eng") => Some("xx"),
        Some("sales") => Some("yy"),
        Some("legal") => Some("zz"),
        _ => None,
    }
}

/// A numeric column, decorrelated from both the term model and the department cycle.
fn score_of(e: u64) -> i32 {
    (e as i32 * 7) % 100
}

/// A numeric column **with absences**, and the one place this corpus exercises them.
///
/// Every third item carries no bonus, and the values that *are* carried straddle zero — which is
/// the whole point. Absence used to be stored as `0` and marked present (decision 0064), so an item
/// with no bonus matched every range containing zero; a fixture whose values were all positive
/// could not tell the two apart.
fn bonus_of(e: u64) -> Option<i32> {
    if e.is_multiple_of(3) {
        None
    } else {
        Some((e as i32 % 21) - 10)
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
        Field::new("archive", DataType::Utf8, true),
        Field::new("title", DataType::Utf8, true),
        Field::new("score", DataType::Int32, false),
        Field::new("bonus", DataType::Int32, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let departments: Vec<Option<String>> = ids
        .iter()
        .map(|&e| department_of(e).map(|s| s.to_string()))
        .collect();
    let archives: Vec<Option<String>> = ids
        .iter()
        .map(|&e| archive_of(e).map(|s| s.to_string()))
        .collect();
    let titles: Vec<Option<String>> = ids.iter().map(|&e| Some(title_of(e))).collect();
    let scores: Vec<i32> = ids.iter().map(|&e| score_of(e)).collect();
    let bonuses: Vec<Option<i32>> = ids.iter().map(|&e| bonus_of(e)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(departments)),
            Arc::new(StringArray::from(archives)),
            Arc::new(StringArray::from(titles)),
            Arc::new(arrow::array::Int32Array::from(scores)),
            Arc::new(arrow::array::Int32Array::from(bonuses)),
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
    /// The `public` column's bindings, kept apart from `department`'s because the two vocabularies
    /// mint independently and a shared map would silently resolve one column's key against the
    /// other's code space.
    archive_codes: HashMap<String, u32>,
    prefix: String,
    /// The manifest's own view of the columns, snapshotted at build. Kept so a test that has
    /// deliberately corrupted a *file* can still reopen the columns: `open_bundle` verifies every
    /// digest, so it refuses first and the reader under test is never reached.
    phash: String,
    declared: Vec<tessera_store::manifest::DeclaredScalar>,
    vocabularies: Vec<tessera_store::manifest::ManifestVocabulary>,
    extents: Vec<tessera_store::manifest::AttrExtent>,
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
    let columns = FilterColumns::open(
        &bundle.join(&prefix),
        &phash,
        &opened.manifest.declared_scalars,
        &opened.manifest.vocabularies,
        // A freshly built bundle has flushed nothing, so its columns are the base layer alone.
        &opened.partitions[&phash].manifest.attr_extents,
        &opened.partitions[&phash].manifest.record_extents,
        // Mapped, which is what the engine does at session open — so the round-trip these tests
        // assert is the one a served request actually takes.
        true,
    )
    .expect("declared filter columns open");
    let bindings = |name: &str| -> HashMap<String, u32> {
        opened
            .manifest
            .vocabularies
            .iter()
            .find(|v| v.name == name)
            .expect("the manifest records the vocabulary")
            .values
            .iter()
            .map(|v| (v.key.clone(), v.code))
            .collect()
    };
    let codes = bindings("department");
    let archive_codes = bindings("archive");

    Fixture {
        _dir: dir,
        bundle,
        entity_of,
        columns,
        codes,
        archive_codes,
        prefix,
        phash: phash.clone(),
        declared: opened.manifest.declared_scalars.clone(),
        vocabularies: opened.manifest.vocabularies.clone(),
        extents: opened.partitions[&phash].manifest.attr_extents.clone(),
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
    let session = engine
        .authorise(credential)
        .expect("the credential resolves");
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
    assert!(
        narrow_hits.cardinality() > 0,
        "and the narrow set must be non-empty"
    );
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
        .resolve("title", &FilterOperand::TextEquals(title_of(3)), &cand)
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

// =================================================================================================
// The per-flush extent (filter-index §2.1, §2.5)
// =================================================================================================

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting: {what}"
        );
        std::thread::yield_now();
    }
}

/// Ingest one row and wait for the flush that publishes it.
///
/// `department` is passed as a `WalScalar` rather than a key so a caller can express **absence** —
/// the reserved code 0, which is what the ingest boundary turns a null category into — beside the
/// ordinary key case a declared vocabulary resolves at the commit window.
fn ingest_and_flush(
    engine: &tessera_engine::Engine,
    external: &str,
    department: WalScalar,
    title: &str,
    score: i32,
) -> u64 {
    ingest_and_flush_with(
        engine,
        external,
        department,
        WalScalar::U8(0),
        title,
        score,
        WalScalar::Null,
    )
}

/// As [`ingest_and_flush`], but naming the `public` column's value too — the case the routed filter
/// and `/v1/categories`' extent sweep both have to see.
#[allow(clippy::too_many_arguments)]
fn ingest_and_flush_with(
    engine: &tessera_engine::Engine,
    external: &str,
    department: WalScalar,
    archive: WalScalar,
    title: &str,
    score: i32,
    bonus: WalScalar,
) -> u64 {
    let flushes_before = engine.write_executor_stats().flushes;
    let row = UnallocatedRow {
        external_id: Some(external.as_bytes().to_vec()),
        slice: "s0".to_string(),
        descriptors: vec![b"0".to_vec()],
        x: 5.0,
        y: 5.0,
        // One per declared column, positionally — including the `filter`-only `title`, which is
        // why `declared_scalars` keeps the full list while the *segment* narrows to render columns.
        // A category arrives as its **key**, never a code (contracts §2.4).
        scalars: vec![
            department,
            archive,
            WalScalar::Utf8(title.to_string()),
            WalScalar::I32(score),
            // `bonus` is the nullable numeric: the default above passes `WalScalar::Null`, which is
            // how an ingested item says it carries no value for a column (decision 0064).
            bonus,
        ],
        terms: engine.resolve_terms(&[b"0".to_vec()]),
    };
    let allocated = engine
        .accept_ingest(vec![row], external.to_string(), [0u8; 32])
        .expect("ingest is accepted")[0];
    assert!(
        allocated.raw() >= N,
        "the new entity is above the build's high-water"
    );
    engine.request_flush();
    wait_until("the flush to publish", || {
        std::thread::sleep(std::time::Duration::from_millis(2));
        engine.write_executor_stats().flushes > flushes_before
    });
    allocated.raw()
}

/// The candidate a full-coverage session sees against the engine's live generation.
fn live_candidate(engine: &tessera_engine::Engine) -> (Arc<tessera_engine::Generation>, Bitmap) {
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
    (generation, cand)
}

/// **An entity ingested after the build answers a filter on its own value.**
///
/// This is the whole point of the per-flush extent (`filter-index.md` §2.1): the build's column
/// covers `[0, entity_id_high_water)`, the flush appends the entities it publishes, and the reader
/// composes the two. The negative half is what makes it a filter rather than a bit that says
/// "recent": the same entity must be *absent* from a filter naming a different value, in all three
/// families.
#[test]
fn an_entity_ingested_after_the_build_answers_a_filter_on_its_own_value() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-flush");
    let wal = fx._dir.path().join("wal-flush");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);

    let new = ingest_and_flush(
        &engine,
        "post-build",
        WalScalar::Utf8("eng".to_string()),
        "paper-99",
        42,
    ) as u32;

    let (generation, cand) = live_candidate(&engine);
    assert!(cand.contains(new), "the ingested entity is visible");
    let columns = &generation.filter_columns;
    let matched = |operand: FilterOperand, column: &str| -> Bitmap {
        columns
            .resolve(column, &operand, &cand)
            .expect("a composed column answers rather than refusing")
    };

    let eng = AttrLocalId::new(fx.codes["eng"]);
    let sales = AttrLocalId::new(fx.codes["sales"]);
    assert!(matched(FilterOperand::Equals(eng), "department").contains(new));
    assert!(!matched(FilterOperand::Equals(sales), "department").contains(new));
    assert!(matched(FilterOperand::In(vec![sales, eng]), "department").contains(new));

    assert!(matched(FilterOperand::TextEquals("paper-99".into()), "title").contains(new));
    assert!(!matched(FilterOperand::TextEquals("paper-01".into()), "title").contains(new));
    assert!(matched(FilterOperand::TextPrefix("paper-9".into()), "title").contains(new));
    assert!(matched(FilterOperand::TextContains("per-99".into()), "title").contains(new));

    let at = |v: i128| Endpoint {
        value: Scalar::Int(v),
        inclusive: true,
    };
    assert!(matched(
        FilterOperand::Range {
            lo: Some(at(42)),
            hi: Some(at(42))
        },
        "score"
    )
    .contains(new));
    assert!(!matched(
        FilterOperand::Range {
            lo: Some(at(43)),
            hi: None
        },
        "score"
    )
    .contains(new));

    // A tree over columns reaches the extent too — every leaf composes its own layers, so a
    // conjunction narrowing the candidate as it goes cannot lose the new entity between them.
    let tree = FilterExpr::AllOf(vec![
        FilterExpr::Leaf {
            column: "department".to_string(),
            operand: FilterOperand::Equals(eng),
        },
        FilterExpr::Leaf {
            column: "title".to_string(),
            operand: FilterOperand::TextPrefix("paper-9".into()),
        },
    ]);
    assert!(columns.evaluate(&tree, &cand).unwrap().contains(new));
}

/// **The build's own entities answer exactly as they did before the flush.**
///
/// An extent adds a layer; it must not move what the base layer says. Asserted as bitmap equality
/// over every column and every family rather than by spot check, because the failure this guards —
/// an extent's slots being read against base entities — shifts *values along entities* and would
/// leave most spot checks passing.
#[test]
fn a_flush_does_not_disturb_what_the_build_already_answered() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-undisturbed");
    let wal = fx._dir.path().join("wal-undisturbed");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);

    let eng = AttrLocalId::new(fx.codes["eng"]);
    let operands: Vec<(&str, FilterOperand)> = vec![
        ("department", FilterOperand::Equals(eng)),
        ("title", FilterOperand::TextPrefix("paper-1".into())),
        ("title", FilterOperand::TextContains("2".into())),
        (
            "score",
            FilterOperand::Range {
                lo: Some(Endpoint {
                    value: Scalar::Int(20),
                    inclusive: true,
                }),
                hi: Some(Endpoint {
                    value: Scalar::Int(60),
                    inclusive: false,
                }),
            },
        ),
    ];

    let (_generation, cand) = live_candidate(&engine);
    let built: Vec<Bitmap> = operands
        .iter()
        .map(|(column, operand)| fx.columns.resolve(column, operand, &cand).unwrap())
        .collect();

    ingest_and_flush(
        &engine,
        "post-build",
        WalScalar::Utf8("eng".to_string()),
        "paper-12",
        42,
    );

    let (generation, cand_after) = live_candidate(&engine);
    let below = cand_after.and(&Bitmap::from_range(0..N as u32));
    for ((column, operand), before) in operands.iter().zip(&built) {
        let after = generation
            .filter_columns
            .resolve(column, operand, &below)
            .unwrap();
        assert_eq!(
            after.to_vec(),
            before.to_vec(),
            "column '{column}' answered differently for the build's own entities after a flush"
        );
    }
}

/// **An ingested entity with no value for a column is absent from every predicate on it**, which is
/// not the same as matching nothing: its *other* columns still answer.
///
/// A category spends the reserved code 0 on absence (per-point-attributes §3.4), so the flush gives
/// that entity no slot in the department extent at all — while the same flush's title and score
/// extents carry it. An extent that treated absence as a value would put every such item in
/// whichever bucket code 0 named.
#[test]
fn an_ingested_entity_with_no_value_is_absent_from_every_predicate_on_that_column() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-absent");
    let wal = fx._dir.path().join("wal-absent");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);

    // Code 0 at the declared width: what the ingest boundary turns a null category into.
    let new = ingest_and_flush(&engine, "no-dept", WalScalar::U8(0), "paper-98", 7) as u32;

    let (generation, cand) = live_candidate(&engine);
    let columns = &generation.filter_columns;
    let every_code: Vec<AttrLocalId> = fx.codes.values().map(|c| AttrLocalId::new(*c)).collect();
    for operand in [
        FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"])),
        FilterOperand::Equals(AttrLocalId::new(fx.codes["sales"])),
        FilterOperand::Equals(AttrLocalId::new(fx.codes["legal"])),
        FilterOperand::In(every_code),
        // Code 0 itself is the unresolvable sentinel, and an item carrying no value must not match
        // a filter naming it either.
        FilterOperand::Equals(AttrLocalId::new(0)),
    ] {
        assert!(
            !columns
                .resolve("department", &operand, &cand)
                .unwrap()
                .contains(new),
            "an item with no department matched {operand:?}"
        );
    }
    assert!(
        columns
            .resolve(
                "title",
                &FilterOperand::TextEquals("paper-98".into()),
                &cand
            )
            .unwrap()
            .contains(new),
        "the same flush's other columns still carry it"
    );
}

/// **The flush writes absence the way the build does**, so a flushed item with no number and a
/// built one answer a range identically.
///
/// The two write paths are separate code — `write_column_values` reads a `ScalarValue` out of a
/// points file, `extent_values` reads a `WalScalar` out of the buffer — and the failure this pins is
/// one of them treating every entity as present. That is invisible against the build's own items
/// (they are in another layer) and shows up only where a range containing zero meets a flushed item
/// with no value.
#[test]
fn a_flushed_item_with_no_number_matches_no_range_either() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-flush-absent");
    let wal = fx._dir.path().join("wal-flush-absent");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);

    let with_bonus = ingest_and_flush_with(
        &engine,
        "has-bonus",
        WalScalar::Utf8("eng".to_string()),
        WalScalar::Utf8("xx".to_string()),
        "paper-90",
        11,
        WalScalar::I32(4),
    ) as u32;
    let without = ingest_and_flush_with(
        &engine,
        "no-bonus",
        WalScalar::Utf8("eng".to_string()),
        WalScalar::Utf8("xx".to_string()),
        "paper-91",
        12,
        WalScalar::Null,
    ) as u32;

    let (generation, cand) = live_candidate(&engine);
    let range = |lo: i128, hi: i128| FilterOperand::Range {
        lo: Some(Endpoint {
            value: Scalar::Int(lo),
            inclusive: true,
        }),
        hi: Some(Endpoint {
            value: Scalar::Int(hi),
            inclusive: true,
        }),
    };

    let straddling_zero = generation
        .filter_columns
        .resolve("bonus", &range(-100, 100), &cand)
        .unwrap();
    assert!(
        straddling_zero.contains(with_bonus),
        "the flushed item that carries a bonus lost it"
    );
    assert!(
        !straddling_zero.contains(without),
        "a flushed item with no bonus matched a range containing zero"
    );

    // And the same flush's other columns still carry the item — absence in one column is not
    // absence from the corpus.
    assert!(
        generation
            .filter_columns
            .resolve(
                "title",
                &FilterOperand::TextEquals("paper-91".into()),
                &cand
            )
            .unwrap()
            .contains(without),
        "the item disappeared from a column it does carry a value for"
    );
}

/// **Several flushes compose**, and each entity answers on its own value.
///
/// One extent working is not the property; the property is that layers accumulate. A composition
/// that kept only the newest extent, or that read the second extent's slots through the first's
/// presence, passes the single-flush test and fails this one.
#[test]
fn several_flushes_compose_and_each_entity_keeps_its_own_value() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-many");
    let wal = fx._dir.path().join("wal-many");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);

    let first = ingest_and_flush(
        &engine,
        "first",
        WalScalar::Utf8("eng".to_string()),
        "alpha",
        1,
    ) as u32;
    let second = ingest_and_flush(
        &engine,
        "second",
        WalScalar::Utf8("sales".to_string()),
        "beta",
        2,
    ) as u32;
    let third = ingest_and_flush(
        &engine,
        "third",
        WalScalar::Utf8("legal".to_string()),
        "gamma",
        3,
    ) as u32;

    let (generation, cand) = live_candidate(&engine);
    let columns = &generation.filter_columns;
    for (entity, key, title, score) in [
        (first, "eng", "alpha", 1i128),
        (second, "sales", "beta", 2),
        (third, "legal", "gamma", 3),
    ] {
        let code = AttrLocalId::new(fx.codes[key]);
        let by_code = columns
            .resolve("department", &FilterOperand::Equals(code), &cand)
            .unwrap();
        assert!(by_code.contains(entity), "{key} lost its own value");
        for (other, other_key) in [(first, "eng"), (second, "sales"), (third, "legal")] {
            if other != entity {
                assert!(
                    !by_code.contains(other),
                    "{other_key} matched {key}'s value: the layers are being read against the \
                     wrong entities"
                );
            }
        }
        assert!(columns
            .resolve("title", &FilterOperand::TextEquals(title.into()), &cand)
            .unwrap()
            .contains(entity));
        let at = |v: i128| Endpoint {
            value: Scalar::Int(v),
            inclusive: true,
        };
        assert!(columns
            .resolve(
                "score",
                &FilterOperand::Range {
                    lo: Some(at(score)),
                    hi: Some(at(score))
                },
                &cand
            )
            .unwrap()
            .contains(entity));
    }
}

/// **The extents survive a restart**, which is what makes them the artefact rather than a cache of
/// what this process happened to flush.
///
/// The reopened engine composes exactly the extents the partition's side-manifest names — so this
/// also pins that they *are* named there, since a manifest that recorded nothing would reopen with
/// the build's coverage alone and answer this filter short.
#[test]
fn a_restart_composes_the_extents_the_manifest_names() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-restart");
    let wal = fx._dir.path().join("wal-restart");
    let new = {
        let engine = open_engine_publishing(&fx.bundle, &cache, &wal);
        ingest_and_flush(
            &engine,
            "post-build",
            WalScalar::Utf8("legal".to_string()),
            "paper-97",
            11,
        ) as u32
    };

    let engine = open_engine(&fx.bundle, &cache, &wal);
    let (generation, cand) = live_candidate(&engine);
    assert!(cand.contains(new), "the flushed entity is visible again");
    let legal = AttrLocalId::new(fx.codes["legal"]);
    assert!(generation
        .filter_columns
        .resolve("department", &FilterOperand::Equals(legal), &cand)
        .unwrap()
        .contains(new));
    assert!(generation
        .filter_columns
        .resolve(
            "title",
            &FilterOperand::TextEquals("paper-97".into()),
            &cand
        )
        .unwrap()
        .contains(new));
}

/// **A named extent that is not there refuses to open**, rather than degrading to "those entities
/// carry no value".
///
/// That degradation is the failure mode this whole composition replaced a refusal to avoid: a
/// short answer is indistinguishable from a correct one. The manifest names and digests both files,
/// so a missing one means the bundle is not what its manifest says — and the presence bitmap is the
/// half a reader could most easily be tempted to treat as optional, since a *base* column's missing
/// presence file legitimately means positional addressing.
///
/// Asserted against the reader rather than against `Engine::open`, deliberately: a bundle whose
/// newest side-manifest names a missing file is *stepped past* by the loader (contracts §2.3), so
/// an engine opening over one answers from the previous manifest and never reaches this rule. The
/// rule still has to hold, because the manifest a loader does select is one every file of which it
/// believes to be there.
#[test]
fn an_extent_file_the_manifest_names_but_that_is_absent_refuses_to_open() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-missing");
    let wal = fx._dir.path().join("wal-missing");
    {
        let engine = open_engine_publishing(&fx.bundle, &cache, &wal);
        ingest_and_flush(
            &engine,
            "post-build",
            WalScalar::Utf8("eng".to_string()),
            "paper-96",
            5,
        );
    }
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fx.bundle.join("CURRENT")).unwrap()).unwrap();
    let prefix = fx.bundle.join(current["prefix"].as_str().unwrap());
    let opened = open_bundle(&fx.bundle).unwrap();
    let phash = opened.partitions.keys().next().unwrap().clone();
    let extents = opened.partitions[&phash].manifest.attr_extents.clone();
    assert!(
        !extents.is_empty(),
        "the flush recorded its extents in the side-manifest"
    );
    let open = |extents: &[tessera_store::manifest::AttrExtent]| {
        FilterColumns::open(
            &prefix,
            &phash,
            &opened.manifest.declared_scalars,
            &opened.manifest.vocabularies,
            extents,
            &[],
            true,
        )
    };
    assert!(open(&extents).is_ok(), "the extents as written compose");

    for named in [&extents[0].values, &extents[0].presence] {
        let path = prefix.join(named);
        let held = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(
            open(&extents).is_err(),
            "an extent the manifest names but that is absent ({named}) must refuse, never read as \
             'those entities carry no value'"
        );
        std::fs::write(&path, held).unwrap();
    }
    assert!(open(&extents).is_ok(), "restored, it composes again");
}

/// **Two layers may not claim one entity, and that is checked rather than reasoned about.**
///
/// Entity ids are permanent and issued from the high-water (**I9**), so an extent can only add ids
/// no earlier layer holds — the overlap is unreachable through the write path. What makes it worth
/// a check is the symptom if I9 ever failed: the entity would carry two values at once and a filter
/// naming either would return it, with nothing anywhere to notice.
#[test]
fn an_extent_overlapping_an_earlier_layer_is_refused() {
    let fx = fixture();
    let mut presence = Bitmap::new();
    presence.add(0);
    let overlapping = Arc::new(
        tessera_filter::ValueColumn::partial(
            tessera_filter::Codes::text(vec!["collision".to_string()]),
            presence,
        )
        .unwrap(),
    );
    let err = fx
        .columns
        .with_extents(&[(
            "title".to_string(),
            "attrs/title/extents/overlapping.arrow".to_string(),
            overlapping,
        )])
        .expect_err("an extent claiming entity 0 overlaps the base column");
    assert!(format!("{err}").contains("I9"), "{err}");

    // And an extent for a column the schema does not declare filterable is refused too: it would
    // otherwise be silently dropped, which is a value column quietly going missing.
    let mut presence = Bitmap::new();
    presence.add(N as u32 + 1);
    let stray = Arc::new(
        tessera_filter::ValueColumn::partial(
            tessera_filter::Codes::text(vec!["stray".to_string()]),
            presence,
        )
        .unwrap(),
    );
    assert!(fx
        .columns
        .with_extents(&[(
            "no_such_column".to_string(),
            "attrs/no_such_column/extents/stray.arrow".to_string(),
            stray,
        )])
        .is_err());
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
        // The schema declares five columns.
        scalars: vec![WalScalar::Utf8("eng".to_string())],
        terms: engine.resolve_terms(&[b"0".to_vec()]),
    };
    let err = engine
        .accept_ingest(vec![short], "batch-short".to_string(), [1u8; 32])
        .expect_err("a short row is refused");
    assert!(
        format!("{err}").contains("carries 1 scalars, but the schema declares 5"),
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
            WalScalar::Utf8("xx".to_string()),
            WalScalar::Utf8("paper-98".to_string()),
            WalScalar::I32(43),
            // A value for every declared column, absence included: `bonus` is nullable and this
            // row carries none, which is a complete row rather than a short one.
            WalScalar::Null,
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
/// **A filtered viewport composes the fragment brought forward, not the session's own.**
///
/// A session's fragment is fixed at authorise, and composition treats every entity below the live
/// watermark as fragment-resident — so composing a filter against the session's own fragment omits
/// everything flushed since it authorised. That failure is *narrowing*, which **I12** permits, and
/// that is exactly what makes it dangerous: the response is a correct-looking subset with no error
/// anywhere. The unfiltered path has always brought the fragment forward; this pins the filtered
/// one to the same rule.
///
/// Asserted by agreement between two sessions rather than by a count alone: one authorised before
/// the flush and one after must answer the same filtered viewport identically, which is the
/// property, and the count then says the flushed entity is genuinely in both.
#[test]
fn a_filtered_viewport_sees_entities_flushed_since_the_session_authorised() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-vp-stale");
    let wal = fx._dir.path().join("wal-vp-stale");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);

    // Authorised *before* the flush: this session's own fragment cannot contain the new entity.
    let before = engine.authorise(&full_coverage_credential()).unwrap();

    ingest_and_flush(
        &engine,
        "post-build-vp",
        WalScalar::Utf8("eng".to_string()),
        "paper-99",
        42,
    );

    // Authorised after it, so its own fragment already holds the entity — the control.
    let after = engine.authorise(&full_coverage_credential()).unwrap();

    let eng = FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"]));
    let filtered = |session: &tessera_engine::Session| {
        engine
            .viewport(
                session,
                ViewportRequest::new("s0", 0, FULL_VIEWPORT, 10_000)
                    .filter(leaf("department", eng.clone())),
            )
            .expect("a filtered viewport answers")
    };

    let stale = filtered(&before);
    let fresh = filtered(&after);

    let built = (0..N).filter(|&e| department_of(e) == Some("eng")).count() as u64;
    assert!(
        built > 0,
        "the fixture has matching entities before the flush"
    );
    assert_eq!(
        fresh.points.len() as u64,
        built + 1,
        "the flushed entity carries `eng` and is served to a session that post-dates it"
    );
    assert_eq!(
        stale.points.len(),
        fresh.points.len(),
        "a session authorised before the flush sees the same filtered set as one authorised after"
    );

    let stale_ids: std::collections::HashSet<u64> =
        stale.points.tessera_ids.iter().copied().collect();
    let fresh_ids: std::collections::HashSet<u64> =
        fresh.points.tessera_ids.iter().copied().collect();
    assert_eq!(
        stale_ids, fresh_ids,
        "and the same identities, not merely as many"
    );
}

#[test]
fn a_filtered_viewport_serves_only_matching_marks() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-vp");
    let wal = fx._dir.path().join("wal-vp");
    let engine = open_engine_uncapped(&fx.bundle, &cache, &wal);
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    let unfiltered = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, FULL_VIEWPORT, 10_000),
        )
        .expect("an unfiltered viewport answers");

    let eng = FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"]));
    let filtered = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, FULL_VIEWPORT, 10_000).filter(leaf("department", eng)),
        )
        .expect("a filtered viewport answers");

    let expected_count = (0..N).filter(|&e| department_of(e) == Some("eng")).count() as u64;
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

/// A viewport far smaller than the filter's result crosses into row space by **testing its own
/// rows** rather than projecting the whole result, and the two routes agree.
///
/// This is the end-to-end half of `viewport::tests::filter_routes_agree_over_the_domain`, which
/// asserts the same equality directly over a 40,000-row domain. What it adds is the wiring: that
/// the route is actually reached through a served request, that the restricted bitmap it produces
/// survives every count and selection the request goes on to take (the debug assertion in
/// `EffectiveMask` fires here if a range outside the tile set is ever consulted under a filter),
/// and that the answer is the same one the projecting route gives for the same window.
///
/// The window is chosen to sit past `PER_TILE_CROSSING_RATIO`, and the route counters assert it
/// landed there rather than leaving the test to pass on the route it was meant to exercise.
///
/// **The predicate is on `score`, an entity-only column, and that is load-bearing.** This test
/// drove `department` until decision 0068 admitted the row-space operand: a rendered category
/// affords the row route, and for a window this narrow the route rule chooses it — so the
/// request stopped crossing at all and the counters went to zero. The crossing is still the
/// only route an entity-only column has, and that is what this test exists to cover; the row
/// route has its own tests below.
#[test]
fn a_narrow_viewport_over_a_broad_filter_tests_its_own_rows() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-pertile");
    let wal = fx._dir.path().join("wal-pertile");
    let engine = open_engine_uncapped(&fx.bundle, &cache, &wal);
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    // `score_of(e) = (7e) % 100`, so this range holds most of the 60 items against a window of
    // ~11 rows: broad enough to clear the ratio, and still a genuine narrowing.
    let any_department = leaf(
        "score",
        FilterOperand::Range {
            lo: Some(Endpoint {
                value: Scalar::Int(0),
                inclusive: true,
            }),
            hi: Some(Endpoint {
                value: Scalar::Int(80),
                inclusive: false,
            }),
        },
    );
    let narrow = [0.0, 0.0, 400.0, 400.0];
    let zoom = 8;

    let unfiltered_narrow = engine
        .viewport(&session, ViewportRequest::new("s0", zoom, narrow, 10_000))
        .expect("the narrow window answers unfiltered");

    let before = engine.filter_crossing_routes();
    let filtered_narrow = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", zoom, narrow, 10_000).filter(any_department.clone()),
        )
        .expect("the narrow window answers filtered");
    let after = engine.filter_crossing_routes();
    assert_eq!(
        (after.0 - before.0, after.1 - before.1),
        (0, 1),
        "this request was supposed to take the per-tile route; the window or the ratio moved"
    );

    // The same filter over the whole extent is far below the ratio and projects — so this is the
    // other route's answer to the same question, computed independently of the one under test.
    let before = engine.filter_crossing_routes();
    let filtered_full = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, FULL_VIEWPORT, 10_000).filter(any_department),
        )
        .expect("the full extent answers filtered");
    let after = engine.filter_crossing_routes();
    assert_eq!(
        (after.0 - before.0, after.1 - before.1),
        (1, 0),
        "the control request was supposed to project"
    );

    let ids = |out: &tessera_engine::ViewportOut| -> std::collections::BTreeSet<u64> {
        out.points.tessera_ids.iter().copied().collect()
    };
    let (narrow_all, narrow_matched, full_matched) = (
        ids(&unfiltered_narrow),
        ids(&filtered_narrow),
        ids(&filtered_full),
    );

    assert_eq!(
        narrow_matched,
        full_matched.intersection(&narrow_all).copied().collect(),
        "the per-tile route's marks differ from the projecting route's over the same window"
    );
    // Non-degenerate in both directions: the window holds items, and the filter removed some of
    // them. Without this the equality above would hold over two empty sets.
    assert!(!narrow_matched.is_empty(), "the window matched nothing");
    assert!(
        narrow_matched.len() < narrow_all.len(),
        "the filter removed nothing from this window, so the routes agreeing proves little"
    );

    // And the per-tile bitmap is a legitimate answer for the *counts* too, not just the marks: a
    // restricted bitmap that under-reported would show up here first, since `matched` is a count
    // over the whole tile rather than over the served prefix.
    let matched_total: u64 = filtered_narrow.tiles.iter().map(|t| t.matched).sum();
    assert_eq!(
        matched_total,
        narrow_matched.len() as u64,
        "Σ matched over tiles disagrees with the marks the same request served"
    );
    let visible_total: u64 = filtered_narrow.tiles.iter().map(|t| t.visible).sum();
    assert_eq!(
        visible_total,
        narrow_all.len() as u64,
        "a filter changed `visible`, which is the composed count and must not move (§7.1)"
    );
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
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, FULL_VIEWPORT, 10_000),
        )
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
            ViewportRequest::new("s0", 0, FULL_VIEWPORT, 10_000).filter(leaf(
                "no_such_column",
                FilterOperand::TextPrefix("x".into()),
            )),
        )
        .expect_err("an undeclared column is refused");
    assert!(
        format!("{err}").contains("not declared filterable"),
        "{err}"
    );
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

/// **`none_of` means *carries a value in this column, and none of these matches it*** — not the
/// complement of the candidate.
///
/// The distinction is the whole of why a negation is expressible at all, and it is asserted here
/// against a column that genuinely has absences: an item with no department must be missing from
/// `none_of: [eng]`, where a complement would return it.
#[test]
fn none_of_requires_a_value_rather_than_taking_the_complement() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &full_coverage_credential());

    let expr = FilterExpr::NoneOf(vec![leaf(
        "department",
        FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"])),
    )]);
    let got = fx.columns.evaluate(&expr, &cand).unwrap();

    assert_eq!(
        as_vec(&got),
        expected(&fx, &[ALL_TERM], |e| department_of(e).is_some()
            && department_of(e) != Some("eng")),
        "none_of returned items carrying no department, which is the complement rather than the \
         negation"
    );
    // Non-degenerate in both directions: the corpus has items with no department at all, and the
    // negation really did exclude something.
    assert!((0..N).any(|e| department_of(e).is_none()));
    assert!((0..N).any(|e| department_of(e) == Some("eng")));
    // I12 by shape: a negation is still a subset of the candidate.
    assert!(got.andnot(&cand).is_empty());
}

/// **The positivity property, asserted rather than argued** (filter-index §5).
///
/// Every "this failure degrades safely under I12" argument in the design holds because each operand
/// is positive: an entity whose value is unreachable matches nothing, so a lost value under-reports
/// and under-reporting narrows. A complement-style negation inverts that — the same failures would
/// *widen*.
///
/// The unreachable entity here is a **buffered** one: accepted and acked, in the candidate, and in
/// no layer until its flush (filter-index §5 rules on exactly this lag). Under a complement it
/// would match every `none_of`; under the built semantics it matches none.
#[test]
fn an_entity_whose_value_is_not_yet_reachable_matches_no_negation() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-positivity");
    let wal = fx._dir.path().join("wal-positivity");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);

    // Accepted and acked, deliberately *not* flushed — so it is in the candidate and in no layer.
    let row = UnallocatedRow {
        external_id: Some(b"buffered".to_vec()),
        slice: "s0".to_string(),
        descriptors: vec![b"0".to_vec()],
        x: 5.0,
        y: 5.0,
        scalars: vec![
            WalScalar::Utf8("eng".to_string()),
            WalScalar::Utf8("xx".to_string()),
            WalScalar::Utf8("paper-77".to_string()),
            WalScalar::I32(3),
            WalScalar::Null,
        ],
        terms: engine.resolve_terms(&[b"0".to_vec()]),
    };
    let buffered = engine
        .accept_ingest(vec![row], "batch-buffered".to_string(), [9u8; 32])
        .expect("ingest is accepted")[0]
        .raw() as u32;

    let (generation, cand) = live_candidate(&engine);
    assert!(
        cand.contains(buffered),
        "the buffered entity must be in the candidate, or this proves nothing"
    );

    for expr in [
        FilterExpr::NoneOf(vec![leaf(
            "department",
            FilterOperand::Equals(AttrLocalId::new(fx.codes["legal"])),
        )]),
        FilterExpr::NoneOf(vec![leaf("title", FilterOperand::TextPrefix("zzz".into()))]),
    ] {
        let got = generation.filter_columns.evaluate(&expr, &cand).unwrap();
        assert!(
            !got.contains(buffered),
            "an entity with no reachable value matched a negation — the failure arithmetic has \
             inverted and every 'degrades safely under I12' argument with it"
        );
    }
}

/// **Decision 0062's C11 existence oracle, closed by construction.**
///
/// The attack: `none_of: [every value I was offered]` returning a non-empty set would prove there
/// exist values of a `per_viewer` category the principal was not shown. It cannot, and not because
/// of an extra intersection — because evaluation happens inside the candidate, so an entity in the
/// candidate carrying value *v* is itself the witness that makes *v* visible under C11's
/// derivation. Every value reachable in the result was therefore offered.
///
/// Asserted with a **narrow** principal, since a full-coverage one is offered everything and the
/// oracle has nothing to reveal to it.
#[test]
fn none_of_every_offered_value_proves_no_unoffered_value_exists() {
    let fx = fixture();
    let (engine, cand) = candidate_for(&fx, &subset_credential());
    let session = engine.authorise(&subset_credential()).unwrap();
    let generation = engine.generation();

    // The vocabulary this principal is actually offered, derived the way `/v1/categories` derives
    // it — membership against its own composed candidate.
    let membership = generation
        .filter_columns
        .category_membership("department", &cand)
        .expect("a per_viewer category carries membership postings");
    let offered: Vec<AttrLocalId> = fx
        .codes
        .values()
        .filter(|c| membership.carries(**c).unwrap())
        .map(|c| AttrLocalId::new(*c))
        .collect();
    // **The fixture must withhold a value the corpus actually carries**, or the oracle has nothing
    // to reveal and an empty result proves nothing. `legal` is that value: the subset principal
    // sees `e % 3 == 0`, and every item carrying `legal` falls outside it.
    assert!(!offered.is_empty(), "this principal is offered nothing");
    let withheld: Vec<&str> = ["eng", "sales", "legal"]
        .into_iter()
        .filter(|k| !membership.carries(fx.codes[*k]).unwrap())
        .collect();
    assert_eq!(
        withheld,
        vec!["legal"],
        "the fixture must withhold a value some item genuinely carries"
    );
    assert!(
        (0..N).any(|e| department_of(e) == Some("legal")),
        "'legal' must be carried by something, or withholding it discloses nothing"
    );

    let expr = FilterExpr::NoneOf(vec![leaf("department", FilterOperand::In(offered.clone()))]);
    let got = generation.filter_columns.evaluate(&expr, &cand).unwrap();

    assert!(
        got.is_empty(),
        "none_of over every offered value returned {} entities, which proves to this principal \
         that values it was not shown exist (C11)",
        got.cardinality()
    );
    let _ = session;
}

/// A `none_of` naming two columns is refused, because it would have to pick which column's presence
/// to require and either choice answers a question the caller did not ask. `all_of` of two
/// single-column negations is the same set and says which.
#[test]
fn a_negation_spanning_two_columns_is_refused_and_composes_instead() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &full_coverage_credential());

    let spanning = FilterExpr::NoneOf(vec![
        leaf(
            "department",
            FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"])),
        ),
        leaf("title", FilterOperand::TextPrefix("paper-1".into())),
    ]);
    let err = fx
        .columns
        .evaluate(&spanning, &cand)
        .expect_err("a negation over two columns is refused");
    let text = format!("{err}");
    assert!(
        text.contains("department") && text.contains("title"),
        "{text}"
    );
    assert!(
        text.contains("all_of"),
        "the refusal names the way to say it: {text}"
    );

    // And the composition it points at is accepted, and is the intersection of the two negations.
    let composed = FilterExpr::AllOf(vec![
        FilterExpr::NoneOf(vec![leaf(
            "department",
            FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"])),
        )]),
        FilterExpr::NoneOf(vec![leaf(
            "title",
            FilterOperand::TextPrefix("paper-1".into()),
        )]),
    ]);
    let got = fx.columns.evaluate(&composed, &cand).unwrap();
    assert_eq!(
        as_vec(&got),
        expected(&fx, &[ALL_TERM], |e| department_of(e).is_some()
            && department_of(e) != Some("eng")
            && !title_of(e).starts_with("paper-1"))
    );

    // An empty negation names no column at all, and is refused for the same reason.
    assert!(fx
        .columns
        .evaluate(&FilterExpr::NoneOf(vec![]), &cand)
        .is_err());
}

/// A negation nests inside the other combinators and counts against the same depth budget.
#[test]
fn a_negation_nests_and_counts_towards_the_depth_limit() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &full_coverage_credential());

    let expr = FilterExpr::AnyOf(vec![
        FilterExpr::NoneOf(vec![leaf(
            "department",
            FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"])),
        )]),
        leaf("title", FilterOperand::TextEquals("paper-00".into())),
    ]);
    assert_eq!(expr.depth(), 3);
    let got = fx.columns.evaluate(&expr, &cand).unwrap();
    assert_eq!(
        as_vec(&got),
        expected(&fx, &[ALL_TERM], |e| (department_of(e).is_some()
            && department_of(e) != Some("eng"))
            || title_of(e) == "paper-00")
    );
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

/// **An item with no value for a numeric column matches no range — including one containing zero.**
///
/// The bug this pins: absence was stored as `0` and marked present, so "score between −10 and 10"
/// returned every item that never had a score. It is a wrong answer rather than a missing feature,
/// which is why it is asserted against a range straddling zero rather than any range at all —
/// against `[1, 10]` the broken and the fixed build agree, and the test would pass on both.
///
/// [Decision 0064]: absence is the presence bitmap beside the column, which is the same mechanism
/// the other two families already use.
///
/// [Decision 0064]: ../../../../docs/decisions/0064-an-absent-number-is-a-presence-bitmap-beside-the-column.md
#[test]
fn an_item_with_no_number_matches_no_range_not_even_one_containing_zero() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &full_coverage_credential());

    let straddling_zero = leaf(
        "bonus",
        FilterOperand::Range {
            lo: Some(Endpoint {
                value: Scalar::Int(-10),
                inclusive: true,
            }),
            hi: Some(Endpoint {
                value: Scalar::Int(10),
                inclusive: true,
            }),
        },
    );
    let got = fx.columns.evaluate(&straddling_zero, &cand).unwrap();

    // Every item that carries a bonus is inside [-10, 10] by construction, so this range is
    // "everything with a value" — and the absent third must be missing from it.
    assert_eq!(
        as_vec(&got),
        expected(&fx, &[ALL_TERM], |e| bonus_of(e).is_some()),
        "an item with no bonus matched a range containing zero"
    );
    // Non-degenerate: the fixture really does have absences, and really does have values that
    // would land in the range if they were read.
    assert!((0..N).any(|e| bonus_of(e).is_none()), "no absences to test");
    assert!(
        (0..N).any(|e| bonus_of(e) == Some(0)),
        "no genuine zero to distinguish absence from"
    );
    // And a genuine zero still matches — the fix must not have thrown out the value with the
    // absence, which an over-eager presence rule would.
    let zero_only = leaf(
        "bonus",
        FilterOperand::Range {
            lo: Some(Endpoint {
                value: Scalar::Int(0),
                inclusive: true,
            }),
            hi: Some(Endpoint {
                value: Scalar::Int(0),
                inclusive: true,
            }),
        },
    );
    assert_eq!(
        as_vec(&fx.columns.evaluate(&zero_only, &cand).unwrap()),
        expected(&fx, &[ALL_TERM], |e| bonus_of(e) == Some(0)),
        "a real zero stopped matching"
    );
}

/// The presence bitmap and the value slots must stay in step: the *k*-th set bit's value is at slot
/// *k*, so an absent entity occupying a slot would shift every later item's number onto its
/// neighbour — every value present, none against its own identity, and no error anywhere.
///
/// Asserted by reading every entity's value back individually rather than through a predicate,
/// because a uniform shift is exactly what a predicate over a whole column can miss.
#[test]
fn every_entity_reads_back_its_own_number_across_the_absences() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &full_coverage_credential());

    for source in 0..N {
        let Some(&entity) = fx.entity_of.get(&source) else {
            continue;
        };
        let entity = entity as u32;
        if !cand.contains(entity) {
            continue;
        }
        let want = bonus_of(source);
        // A one-value range is the narrowest question the numeric surface can ask.
        let hit = |v: i32| {
            let expr = leaf(
                "bonus",
                FilterOperand::Range {
                    lo: Some(Endpoint {
                        value: Scalar::Int(v as i128),
                        inclusive: true,
                    }),
                    hi: Some(Endpoint {
                        value: Scalar::Int(v as i128),
                        inclusive: true,
                    }),
                },
            );
            fx.columns.evaluate(&expr, &cand).unwrap().contains(entity)
        };
        match want {
            Some(v) => assert!(
                hit(v),
                "source {source} (entity {entity}) lost its bonus {v}"
            ),
            None => {
                for v in -10..=10 {
                    assert!(
                        !hit(v),
                        "source {source} (entity {entity}) has no bonus but matched {v}"
                    );
                }
            }
        }
    }
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
            && (department_of(e) == Some("eng")
                || title_of(e).starts_with("paper-1")))
    );
}

// =================================================================================================
// The category postings route (decision 0063) and `/v1/categories` under `per_viewer`
// =================================================================================================

/// Where the build wrote one column's derived postings.
fn postings_path(fx: &Fixture, column: &str) -> std::path::PathBuf {
    fx.bundle
        .join(&fx.prefix)
        .join("partitions")
        .join(&fx.phash)
        .join("attrs")
        .join(column)
        .join("postings.arrow")
}

/// Reopen the fixture's columns from disk — used after a test has rewritten a postings file, since
/// the route is decided and the file mapped at open.
fn reopen(fx: &Fixture) -> std::io::Result<FilterColumns> {
    FilterColumns::open(
        &fx.bundle.join(&fx.prefix),
        &fx.phash,
        &fx.declared,
        &fx.vocabularies,
        &fx.extents,
        &[],
        true,
    )
}

/// **A `public` category answers `eq` and `in` exactly as the corpus says**, under a real mask.
///
/// The reference is the fixture's own inputs, so a routed answer that read the wrong posting, or
/// that forgot to intersect with the candidate, fails here rather than agreeing with itself.
#[test]
fn a_public_category_route_agrees_with_the_corpus_under_a_real_mask() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &subset_credential());
    let terms = [SUBSET_TERM];

    let xx = AttrLocalId::new(fx.archive_codes["xx"]);
    let yy = AttrLocalId::new(fx.archive_codes["yy"]);
    let ww = AttrLocalId::new(fx.archive_codes["ww"]);

    let got = fx
        .columns
        .resolve("archive", &FilterOperand::Equals(xx), &cand)
        .unwrap();
    assert_eq!(
        as_vec(&got),
        expected(&fx, &terms, |e| archive_of(e) == Some("xx"))
    );
    assert!(got.andnot(&cand).is_empty(), "I12: inside the candidate");

    let both = fx
        .columns
        .resolve("archive", &FilterOperand::In(vec![xx, yy]), &cand)
        .unwrap();
    assert_eq!(
        as_vec(&both),
        expected(&fx, &terms, |e| matches!(
            archive_of(e),
            Some("xx") | Some("yy")
        ))
    );

    // A declared value nothing carries is an empty answer, not an error: a keyed postings file
    // drops empty records, so this is the `None` arm of `posting_at` reaching the surface.
    assert!(fx
        .columns
        .resolve("archive", &FilterOperand::Equals(ww), &cand)
        .unwrap()
        .is_empty());
    // As is the reserved absent sentinel, which no entity may match on either route.
    assert!(fx
        .columns
        .resolve(
            "archive",
            &FilterOperand::Equals(AttrLocalId::new(0)),
            &cand
        )
        .unwrap()
        .is_empty());
}

/// **The route is the declaration, and this is the assertion that proves it.**
///
/// Both columns carry the same partition of the corpus. Each column's postings file is rewritten
/// with a deliberately wrong mapping — every code claiming entity 0 and nothing else — and the two
/// then answer differently: the `public` column returns the corrupted postings' answer, because it
/// is routed through them; the `per_viewer` column returns the *correct* answer, because decision
/// 0063 keeps it on the scan. Nothing else in this file can tell the two routes apart, since a
/// working route and a working scan agree by construction.
#[test]
fn a_public_column_reads_its_postings_and_a_per_viewer_one_does_not() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &full_coverage_credential());

    // Every code in both vocabularies, mapped to entity 0 alone.
    for (column, codes) in [("archive", &fx.archive_codes), ("department", &fx.codes)] {
        let mut entries: Vec<(u32, Vec<u32>)> =
            codes.values().map(|&code| (code, vec![0u32])).collect();
        entries.sort_by_key(|(code, _)| *code);
        tessera_authz::write_delta_tier_at(&postings_path(&fx, column), &entries, 32).unwrap();
    }
    let columns = reopen(&fx).expect("the rewritten files are well-formed and open");

    let archived = columns
        .resolve(
            "archive",
            &FilterOperand::Equals(AttrLocalId::new(fx.archive_codes["xx"])),
            &cand,
        )
        .unwrap();
    assert_eq!(
        as_vec(&archived),
        vec![0u32],
        "a `public` column must answer from its postings — this is what decision 0063 buys, and \
         with the file corrupted it is the only way the answer can be this"
    );

    let departmental = columns
        .resolve(
            "department",
            &FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"])),
            &cand,
        )
        .unwrap();
    assert_eq!(
        as_vec(&departmental),
        expected(&fx, &[ALL_TERM], |e| department_of(e) == Some("eng")),
        "a `per_viewer` column must be answered by the masked scan, whatever its postings say: \
         the postings' work is a function of the value named, which is the disclosure \
         `listing = \"per_viewer\"` exists to prevent (decision 0063)"
    );
    assert!(
        departmental.cardinality() > 1,
        "the fixture must make the two answers distinguishable"
    );
}

/// **A routed column still sees everything ingested since the build.**
///
/// The postings cover `[0, entity_id_high_water)` and no flush writes any, so an answer taken from
/// them alone would omit every entity flushed since — narrower, safe under I12, and
/// indistinguishable from a correct answer. Several flushes, because one extent working is not the
/// property: the property is that they accumulate.
#[test]
fn a_routed_public_category_unions_the_postings_with_every_extent() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-routed-extents");
    let wal = fx._dir.path().join("wal-routed-extents");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);

    let first = ingest_and_flush_with(
        &engine,
        "routed-1",
        WalScalar::Utf8("eng".to_string()),
        WalScalar::Utf8("xx".to_string()),
        "routed-alpha",
        1,
        WalScalar::Null,
    ) as u32;
    let second = ingest_and_flush_with(
        &engine,
        "routed-2",
        WalScalar::Utf8("sales".to_string()),
        WalScalar::Utf8("yy".to_string()),
        "routed-beta",
        2,
        WalScalar::Null,
    ) as u32;
    // `ww` is declared and carried by nothing in the build, so this entity is its *only* member —
    // the case a postings-only answer gets exactly backwards.
    let third = ingest_and_flush_with(
        &engine,
        "routed-3",
        WalScalar::Utf8("legal".to_string()),
        WalScalar::Utf8("ww".to_string()),
        "routed-gamma",
        3,
        WalScalar::Null,
    ) as u32;

    let (generation, cand) = live_candidate(&engine);
    let columns = &generation.filter_columns;
    let code = |key: &str| AttrLocalId::new(fx.archive_codes[key]);
    let hits = |operand: FilterOperand| columns.resolve("archive", &operand, &cand).unwrap();

    let xx = hits(FilterOperand::Equals(code("xx")));
    assert!(xx.contains(first), "the first flush's entity is missing");
    assert!(!xx.contains(second));
    assert!(!xx.contains(third));
    // And the base build's members are still there, so the union is a union and not a replacement.
    assert!(
        xx.cardinality() > 1,
        "the build's own `xx` members must survive the union with the extents"
    );

    assert!(hits(FilterOperand::Equals(code("yy"))).contains(second));

    let ww = hits(FilterOperand::Equals(code("ww")));
    assert_eq!(
        as_vec(&ww),
        vec![third],
        "a value whose only member arrived after the build must still be found"
    );

    let any = hits(FilterOperand::In(vec![code("xx"), code("ww")]));
    assert!(any.contains(first) && any.contains(third) && !any.contains(second));
}

/// **A routed column whose postings cannot be read refuses**, rather than degrading to the scan or
/// to "no entity carries this value". Both would answer, and one of them would answer *correctly* —
/// which is worse, because the artefact would be broken with nothing to notice.
#[test]
fn a_public_column_with_unreadable_postings_refuses_to_open() {
    let fx = fixture();
    let path = postings_path(&fx, "archive");
    let held = std::fs::read(&path).unwrap();

    std::fs::remove_file(&path).unwrap();
    assert!(
        reopen(&fx).is_err(),
        "a declared column whose postings are missing must refuse at open"
    );

    std::fs::write(&path, &held[..held.len() / 2]).unwrap();
    assert!(
        reopen(&fx).is_err(),
        "a truncated postings file must refuse at open"
    );

    std::fs::write(&path, &held).unwrap();
    assert!(reopen(&fx).is_ok(), "restored, it opens again");
}

/// **A `per_viewer` category column is not filterable merely because it is held.**
///
/// The map now carries every column the build wrote a value column for, which includes a
/// `per_viewer` category declared `render`-only — its postings are what `/v1/categories` derives
/// visibility from. Holding it must open no operand the schema did not declare, so `resolve` gates
/// on the declaration and not on presence. (`department` here *is* declared filterable; the guard
/// is asserted at the seam it protects, in `filter.rs`'s `Layers::filterable`.)
#[test]
fn a_column_the_schema_did_not_declare_filterable_is_still_refused() {
    let fx = fixture();
    let (_engine, cand) = candidate_for(&fx, &full_coverage_credential());
    assert!(matches!(
        fx.columns.resolve(
            "no_such_column",
            &FilterOperand::Equals(AttrLocalId::new(1)),
            &cand
        ),
        Err(FilterError::UndeclaredColumn(_))
    ));
}

// =================================================================================================
// `/v1/categories` under `listing = "per_viewer"` (per-point-attributes §3.3)
// =================================================================================================

/// Every value `column` offers this principal, in key order, paged at `limit` so the cursor is
/// exercised on the ordinary path rather than only on a contrived one.
fn offered(
    engine: &tessera_engine::Engine,
    session: &tessera_engine::Session,
    column: &str,
    limit: usize,
) -> Vec<String> {
    let mut keys = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = engine
            .categories(
                session,
                column,
                tessera_engine::CategoryQuery::Page {
                    after: after.as_deref(),
                    limit,
                },
            )
            .expect("the column is served")
            .expect("the column is a category");
        keys.extend(page.values.iter().map(|v| v.key.clone()));
        match page.next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    keys
}

/// **A value is offered iff the principal can see an item carrying it** (§3.3), derived per request
/// and never maintained.
///
/// `legal` is the assertion that matters: the subset principal's items are exactly `e % 3 == 0`,
/// and none of those carries `legal` — so a value that certainly exists, and that a wider principal
/// is offered, is withheld from this one. `ops` is declared and carried by nobody, so it is offered
/// to neither: the empty-value case, which membership-derivation answers by hiding.
#[test]
fn a_per_viewer_value_is_offered_only_where_a_visible_item_carries_it() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-cats");
    let wal = fx._dir.path().join("wal-cats");
    let engine = open_engine(&fx.bundle, &cache, &wal);

    let wide = engine.authorise(&full_coverage_credential()).unwrap();
    let narrow = engine.authorise(&subset_credential()).unwrap();
    let none = engine.authorise(&zero_credential()).unwrap();

    assert_eq!(
        offered(&engine, &wide, "department", 2),
        ["eng", "legal", "sales"]
    );
    assert_eq!(offered(&engine, &narrow, "department", 2), ["eng", "sales"]);
    assert!(
        offered(&engine, &none, "department", 2).is_empty(),
        "a principal who can see nothing is offered nothing — and is told so with a real answer, \
         since an empty value set is what that principal's derivation yields"
    );

    // Cross-check the fixture rather than trusting the arithmetic above: `legal` must genuinely be
    // a value the narrow principal has no member of, or this test asserts nothing.
    assert!((0..N)
        .filter(|e| terms_of(*e).contains(&SUBSET_TERM))
        .all(|e| department_of(e) != Some("legal")));
}

/// **One gate, both request forms.** Bulk lookup and enumeration are separate walks, and a gate
/// applied to one is the existence oracle reached through the other.
#[test]
fn both_category_request_forms_apply_the_membership_gate() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-cats-forms");
    let wal = fx._dir.path().join("wal-cats-forms");
    let engine = open_engine(&fx.bundle, &cache, &wal);
    let narrow = engine.authorise(&subset_credential()).unwrap();

    let asked: Vec<u32> = ["eng", "sales", "legal", "ops"]
        .iter()
        .map(|k| fx.codes[*k])
        .collect();
    let page = engine
        .categories(
            &narrow,
            "department",
            tessera_engine::CategoryQuery::Codes(&asked),
        )
        .unwrap()
        .unwrap();
    let keys: Vec<&str> = page.values.iter().map(|v| v.key.as_str()).collect();
    assert_eq!(
        keys,
        ["eng", "sales"],
        "a code naming a value this principal has no member of must be omitted, exactly as an \
         unbound code is — the two must be one outcome"
    );
}

/// **The page is cut after the gate, never before.** Filtering a page once it has been taken
/// returns short pages whose length counts what the principal cannot see, and terminates the walk
/// early — here it would drop `sales` entirely, since `ops` is invisible and sits between.
#[test]
fn a_per_viewer_page_is_filtered_before_it_is_cut() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-cats-page");
    let wal = fx._dir.path().join("wal-cats-page");
    let engine = open_engine(&fx.bundle, &cache, &wal);
    let wide = engine.authorise(&full_coverage_credential()).unwrap();

    let first = engine
        .categories(
            &wide,
            "department",
            tessera_engine::CategoryQuery::Page {
                after: None,
                limit: 2,
            },
        )
        .unwrap()
        .unwrap();
    let keys: Vec<&str> = first.values.iter().map(|v| v.key.as_str()).collect();
    assert_eq!(keys, ["eng", "legal"]);
    assert_eq!(
        first.next.as_deref(),
        Some("legal"),
        "a third visible value remains — `ops` sits between it and this page in key order and \
         must not be allowed to end the walk"
    );

    let second = engine
        .categories(
            &wide,
            "department",
            tessera_engine::CategoryQuery::Page {
                after: Some("legal"),
                limit: 2,
            },
        )
        .unwrap()
        .unwrap();
    let keys: Vec<&str> = second.values.iter().map(|v| v.key.as_str()).collect();
    assert_eq!(keys, ["sales"]);
    assert!(second.next.is_none());
}

/// **A `public` vocabulary is served as authored, to every principal alike** — including a value
/// nothing carries, and including a principal who can see nothing. It derives no membership at all,
/// which is why its filter may be routed through the postings (decision 0063) while a `per_viewer`
/// one may not.
#[test]
fn a_public_vocabulary_is_served_as_authored_to_every_principal() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-cats-public");
    let wal = fx._dir.path().join("wal-cats-public");
    let engine = open_engine(&fx.bundle, &cache, &wal);

    for credential in [
        full_coverage_credential(),
        subset_credential(),
        zero_credential(),
    ] {
        let session = engine.authorise(&credential).unwrap();
        assert_eq!(
            offered(&engine, &session, "archive", 3),
            ["ww", "xx", "yy", "zz"],
            "a `public` set is an authored assertion, so every principal is served the same one"
        );
    }
}

/// **A value carried only by entities ingested since the build is still offered.**
///
/// The membership postings cover `[0, entity_id_high_water)` and no flush writes any, so deriving
/// visibility from them alone would withhold a value the principal can plainly see — the same
/// omission the routed filter has to avoid, in the endpoint that publishes the legend. `ops` is
/// declared and carried by nothing in the build, so the flushed entity is its only member.
#[test]
fn a_value_carried_only_since_the_build_is_offered_to_whoever_can_see_it() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-cats-flush");
    let wal = fx._dir.path().join("wal-cats-flush");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);

    let wide = engine.authorise(&full_coverage_credential()).unwrap();
    let none = engine.authorise(&zero_credential()).unwrap();
    assert!(
        !offered(&engine, &wide, "department", 4).contains(&"ops".to_string()),
        "nothing carries `ops` in the build"
    );

    ingest_and_flush_with(
        &engine,
        "ops-1",
        WalScalar::Utf8("ops".to_string()),
        WalScalar::U8(0),
        "ops-paper",
        9,
        WalScalar::Null,
    );

    assert_eq!(
        offered(&engine, &wide, "department", 4),
        ["eng", "legal", "ops", "sales"],
        "the flushed entity's value must be offered to a principal who can see it"
    );
    assert!(
        offered(&engine, &none, "department", 4).is_empty(),
        "and to nobody who cannot"
    );
}

// =================================================================================================
// The fold's attribute pass (`filter-index.md` §6.2)
// =================================================================================================
//
// A fold rewrites a bundle into a new prefix and reclaims the old one, so every artefact it does
// not carry forward is *gone*. These cases are the attribute half of that: that a folded bundle
// answers what the pre-fold one answered over the build's entities and the flushed ones alike,
// that a deleted entity's value bytes leave the corpus while a suppressed one's do not (Rules F and
// S, which must never be conflated), and that the two obligations publication owes — the files and
// the manifest's `attr_extents` list — are both discharged. Doing one of those two is worse than
// doing neither: the bundle then opens cleanly and answers filters short, which is a wrong answer
// wearing a correct one's clothes.

/// Request a fold and block until it has published, asserting it was not discarded.
///
/// `tests/fold.rs`'s helper, duplicated rather than shared: `common` is the fixture module and this
/// binary's fixture is its own (the one with declared filter columns), so the alternative is
/// widening `common` for two callers that agree about nothing else.
fn fold(engine: &tessera_engine::Engine) {
    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        if now.folds > before.folds {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// One operand per family and per route, answered against `columns` under `candidate`.
///
/// **Both routes are here on purpose.** `archive` is `listing = "public"`, so its `eq`/`in` are
/// answered by the derived postings — which the fold *rebuilds*, where it *merges* the value column
/// — and `department` is `per_viewer`, so the same values are answered by the scan. A pass that
/// rebuilt one and lost the other would leave half of this table right.
fn probe_every_operand(
    fx: &Fixture,
    columns: &FilterColumns,
    candidate: &Bitmap,
) -> Vec<(String, Vec<u32>)> {
    let eng = AttrLocalId::new(fx.codes["eng"]);
    let sales = AttrLocalId::new(fx.codes["sales"]);
    let xx = AttrLocalId::new(fx.archive_codes["xx"]);
    let yy = AttrLocalId::new(fx.archive_codes["yy"]);
    let at = |v: i128| Endpoint {
        value: Scalar::Int(v),
        inclusive: true,
    };
    let probes: Vec<(&str, &str, FilterOperand)> = vec![
        ("department", "eq eng", FilterOperand::Equals(eng)),
        (
            "department",
            "in eng+sales",
            FilterOperand::In(vec![eng, sales]),
        ),
        ("archive", "eq xx (routed)", FilterOperand::Equals(xx)),
        (
            "archive",
            "in xx+yy (routed)",
            FilterOperand::In(vec![xx, yy]),
        ),
        (
            "title",
            "eq paper-07",
            FilterOperand::TextEquals("paper-07".into()),
        ),
        (
            "title",
            "prefix paper-1",
            FilterOperand::TextPrefix("paper-1".into()),
        ),
        (
            "title",
            "contains er-2",
            FilterOperand::TextContains("er-2".into()),
        ),
        (
            "score",
            "range 20..=60",
            FilterOperand::Range {
                lo: Some(at(20)),
                hi: Some(at(60)),
            },
        ),
    ];
    probes
        .into_iter()
        .map(|(column, label, operand)| {
            let answer = columns
                .resolve(column, &operand, candidate)
                .expect("a declared column answers");
            (format!("{column} {label}"), as_vec(&answer))
        })
        .collect()
}

/// **A folded bundle answers exactly what it answered before the fold** — over the build's entities
/// and over one ingested since it, in every family and on both routes.
///
/// This is the case the gap was loudest in: a fold carries forward exactly the files its new
/// manifest names, `attrs/` was in none of those lists, and the folded bundle had no filter
/// artefact at all. What makes it a *filter* test rather than an openability one is the flushed
/// entity: its value lives in an extent, the fold consumes every extent into the new base, and an
/// answer that lost it would be narrower, safe under **I12**, and indistinguishable from a correct
/// one.
///
/// **Mutations this kills:** dropping the attribute pass (nothing to open — the bundle refuses);
/// folding the base column and skipping the extents (the flushed entity answers nothing); writing
/// the new base without blanking (caught by the deletion case below rather than here).
#[test]
fn a_folded_bundle_answers_every_filter_it_answered_before() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-fold-answers");
    let wal = fx._dir.path().join("wal-fold-answers");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);

    let flushed = ingest_and_flush_with(
        &engine,
        "post-build",
        WalScalar::Utf8("eng".to_string()),
        WalScalar::Utf8("xx".to_string()),
        "paper-77",
        42,
        WalScalar::Null,
    ) as u32;

    let (generation, cand) = live_candidate(&engine);
    let before = probe_every_operand(&fx, &generation.filter_columns, &cand);
    // The reference is the corpus, not the index: the build's own answer for one operand is
    // checked against the fixture's inputs, so a pass that folded a self-consistent lie fails here
    // and not only against itself.
    let build_eng = expected(&fx, &[ALL_TERM], |e| department_of(e) == Some("eng"));
    assert!(
        before[0]
            .1
            .iter()
            .filter(|e| **e != flushed)
            .copied()
            .eq(build_eng.iter().copied()),
        "the pre-fold answer is the corpus's own"
    );
    assert!(before[0].1.contains(&flushed));
    drop(generation);

    fold(&engine);
    assert_eq!(engine.generation().prefix, "v00001");

    let (folded, cand_after) = live_candidate(&engine);
    assert_eq!(
        as_vec(&cand_after),
        as_vec(&cand),
        "the fold retires nothing here, so the candidate is the same entity set"
    );
    let after = probe_every_operand(&fx, &folded.filter_columns, &cand_after);
    assert_eq!(
        after, before,
        "every operand answers what it answered before"
    );
    assert_eq!(
        folded
            .bundle
            .partitions
            .values()
            .next()
            .expect("one partition")
            .manifest
            .attr_extents
            .len(),
        0,
        "every snapshot extent folded into the base; carrying an untouched one forward is declined"
    );
}

/// **A node restarts onto a folded bundle and opens** — the failure the gap produced today, stated
/// as its own case because it is the one an operator meets first.
///
/// A declared column whose files are missing is an *error* rather than an absence
/// (`FilterColumns::open`), which is what made the missing `attrs/` a loud refusal instead of a
/// silent one. That rule is unchanged; what the pass changes is which files it binds.
#[test]
fn a_node_restarts_onto_a_folded_bundle_and_answers_from_it() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-fold-restart");
    let wal = fx._dir.path().join("wal-fold-restart");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);
    ingest_and_flush_with(
        &engine,
        "post-build",
        WalScalar::Utf8("legal".to_string()),
        WalScalar::Utf8("zz".to_string()),
        "paper-88",
        11,
        WalScalar::Null,
    );
    fold(&engine);
    let (generation, cand) = live_candidate(&engine);
    let before = probe_every_operand(&fx, &generation.filter_columns, &cand);
    drop(generation);
    drop(engine);

    let restarted = open_engine_publishing(&fx.bundle, &cache, &wal);
    assert_eq!(restarted.generation().prefix, "v00001");
    let (generation, cand) = live_candidate(&restarted);
    assert_eq!(
        probe_every_operand(&fx, &generation.filter_columns, &cand),
        before,
        "a restart onto the folded prefix composes exactly what the process that folded it served"
    );
}

/// The folded prefix's value column for one attribute, opened directly off disc.
fn folded_column(fx: &Fixture, prefix: &str, column: &str) -> tessera_filter::ValueColumn {
    tessera_filter::ValueColumn::open_dir(
        &fx.bundle
            .join(prefix)
            .join("partitions")
            .join(&fx.phash)
            .join("attrs")
            .join(column),
        tessera_filter::Access::Read,
    )
    .expect("the folded column opens")
}

/// **A deleted entity is gone from every predicate after the fold, and its bytes are gone with
/// it** (Rule F, and the retention asymmetry §6 gives as the reason for blanking).
///
/// The masked scan would never have visited its slot — the fold has already removed it from
/// `M_auth` — so this is not a correctness repair but a *retention* one: after a fold the item's
/// render value is gone because its row is gone, while its filter value would persist because the
/// slot is positional and **I9** forbids renumbering it away. The byte assertion is what makes that
/// a test rather than a claim.
///
/// **Mutations this kills:** blanking by writing a sentinel over the slot (the title's bytes are
/// still in `values.arrow`); skipping `D₀` in the presence bitmap but not in the values (every
/// value after the deleted entity is paired with the wrong entity, so the surviving titles move);
/// rebuilding the postings from the pre-fold column (the deleted entity is still a member).
#[test]
fn a_deleted_entitys_value_leaves_the_column_and_every_predicate() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-fold-delete");
    let wal = fx._dir.path().join("wal-fold-delete");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);

    // Sources 2 and 8 carry the same department and archive values and the titles `paper-02` and
    // `paper-08`: one is deleted and the other is the survivor every assertion is read against, so
    // "gone" cannot be satisfied by emptying the column.
    let deleted = fx.entity_of[&2];
    let survivor = fx.entity_of[&8];
    assert_eq!(department_of(2), department_of(8));
    assert_eq!(archive_of(2), archive_of(8));
    let dept = AttrLocalId::new(fx.codes[department_of(2).expect("source 2 carries one")]);
    let arch = AttrLocalId::new(fx.archive_codes[archive_of(2).expect("source 2 carries one")]);
    engine
        .accept_change(
            tessera_types::EntityId::new(deleted),
            tessera_lifecycle::wal::ChangeOp::Delete,
        )
        .expect("a delete is accepted");
    fold(&engine);
    assert_eq!(
        engine.overlay_depth(),
        0,
        "the tombstone retired in the fold's own publication (Rule F)"
    );

    let (generation, cand) = live_candidate(&engine);
    let columns = &generation.filter_columns;
    for (column, operand) in [
        ("department", FilterOperand::Equals(dept)),
        ("archive", FilterOperand::Equals(arch)),
        ("title", FilterOperand::TextPrefix("paper-".into())),
        ("title", FilterOperand::TextEquals(title_of(2))),
    ] {
        let answer = columns.resolve(column, &operand, &cand).expect("answers");
        assert!(
            !answer.contains(deleted as u32),
            "the deleted entity still matches {column}"
        );
    }
    assert!(columns
        .resolve("department", &FilterOperand::Equals(dept), &cand)
        .unwrap()
        .contains(survivor as u32));

    // The artefact itself: no slot, no bytes, no posting.
    let titles = folded_column(&fx, "v00001", "title");
    assert_eq!(titles.text_of(deleted as u32), None);
    assert_eq!(titles.text_of(survivor as u32).unwrap(), title_of(8));
    let bytes = std::fs::read(
        fx.bundle
            .join("v00001")
            .join("partitions")
            .join(&fx.phash)
            .join("attrs/title/values.arrow"),
    )
    .expect("the folded column is on disc");
    let needle = title_of(2);
    assert!(
        !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
        "the deleted entity's value bytes are still in the folded column — blanking is removal \
         from presence and no value bytes, never a sentinel over them"
    );

    let postings = tessera_filter::ColumnPostings::open_keyed(
        &fx.bundle
            .join("v00001")
            .join("partitions")
            .join(&fx.phash)
            .join("attrs/archive/postings.arrow"),
    )
    .expect("the rebuilt postings open");
    let members = postings.entities(arch).expect("the code has members");
    assert!(!members.contains(deleted as u32), "and no posting names it");
    assert!(members.contains(survivor as u32));
}

/// **A suppression changes no attribute artefact at all** (Rule S), and the fold is where that is
/// most easily got wrong — the two removal rules have been conflated twice in this project's review
/// history, and giving a suppression any retirement route is fail-open.
///
/// So the folded column still holds the suppressed entity's value and the rebuilt postings still
/// name it; what hides the item is the overlay, which the fold leaves standing. The unsuppress at
/// the end is what proves the value was still there to be revealed rather than merely unreachable.
#[test]
fn a_suppression_changes_no_attribute_artefact_across_the_fold() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-fold-suppress");
    let wal = fx._dir.path().join("wal-fold-suppress");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);

    let suppressed = fx.entity_of[&2];
    let arch = AttrLocalId::new(fx.archive_codes[archive_of(2).expect("source 2 carries one")]);
    engine
        .accept_change(
            tessera_types::EntityId::new(suppressed),
            tessera_lifecycle::wal::ChangeOp::Suppress,
        )
        .expect("a suppression is accepted");
    fold(&engine);
    assert_eq!(
        engine.overlay_depth(),
        1,
        "Rule S: a suppression never retires, and the fold does not execute one"
    );

    let titles = folded_column(&fx, "v00001", "title");
    assert_eq!(
        titles.text_of(suppressed as u32).map(|s| s.to_string()),
        Some(title_of(2)),
        "the suppressed entity keeps its slot and its bytes: no attribute artefact changes for a \
         suppression"
    );
    let postings = tessera_filter::ColumnPostings::open_keyed(
        &fx.bundle
            .join("v00001")
            .join("partitions")
            .join(&fx.phash)
            .join("attrs/archive/postings.arrow"),
    )
    .expect("the rebuilt postings open");
    assert!(postings
        .entities(arch)
        .expect("members")
        .contains(suppressed as u32));

    // Hidden by the overlay throughout, and revealed by the unsuppress — which is only possible
    // because the artefact still holds the value.
    let (generation, cand) = live_candidate(&engine);
    assert!(!cand.contains(suppressed as u32), "still hidden");
    drop(generation);
    engine
        .accept_change(
            tessera_types::EntityId::new(suppressed),
            tessera_lifecycle::wal::ChangeOp::Unsuppress,
        )
        .expect("an unsuppress is accepted");
    let (generation, cand) = live_candidate(&engine);
    assert!(generation
        .filter_columns
        .resolve("title", &FilterOperand::TextEquals(title_of(2)), &cand)
        .expect("answers")
        .contains(suppressed as u32));
}

/// **An extent published during the fold's flight survives it, is listed, and is composed.**
///
/// This is publication's two obligations in one case. The extent's *files* are hard-linked into the
/// new prefix and its *entry* is written into the new `SEGMENTS-<n>.json`'s `attr_extents`; either
/// one alone is worse than neither. Files without the entry produce a bundle that opens cleanly and
/// answers filters missing every post-snapshot entity — a wrong answer with no symptom — and the
/// entry without the files is a refusal at the next open.
///
/// **Mutations this kills:** publishing `attr_extents: Vec::new()` (the mid-flight entity answers
/// nothing, here and after a restart); leaving the extent's files out of the carry-forward set (the
/// new prefix does not open once the old one is reclaimed).
#[test]
fn an_extent_published_during_the_folds_flight_is_carried_forward_and_composed() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-fold-flight");
    let wal = fx._dir.path().join("wal-fold-flight");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);

    let before = engine.write_executor_stats();
    engine.set_fold_paused_for_test(true);
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !engine.fold_is_holding_for_test() {
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never reached its hold"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // Accepted and flushed *after* the fold's snapshot: its extent is post-snapshot, so the pass
    // never saw it and publication must carry it.
    let mid_flight = ingest_and_flush_with(
        &engine,
        "mid-flight",
        WalScalar::Utf8("sales".to_string()),
        WalScalar::Utf8("yy".to_string()),
        "paper-66",
        7,
        WalScalar::Null,
    ) as u32;
    engine.set_fold_paused_for_test(false);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold discarded"
        );
        if now.folds > before.folds {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let (generation, cand) = live_candidate(&engine);
    let partition = generation
        .bundle
        .partitions
        .values()
        .next()
        .expect("one partition");
    let extents = &partition.manifest.attr_extents;
    assert!(
        !extents.is_empty(),
        "the flight's extents are named in the folded manifest — the list is half of publishing \
         an attribute artefact, and the half that fails silently"
    );
    for extent in extents {
        for rel in [&extent.values, &extent.presence] {
            assert!(
                fx.bundle.join("v00001").join(rel).exists(),
                "{rel} is named by the folded manifest but is not under the new prefix"
            );
        }
    }
    assert!(generation
        .filter_columns
        .resolve(
            "title",
            &FilterOperand::TextEquals("paper-66".into()),
            &cand
        )
        .expect("answers")
        .contains(mid_flight));
    assert!(generation
        .filter_columns
        .resolve(
            "archive",
            &FilterOperand::Equals(AttrLocalId::new(fx.archive_codes["yy"])),
            &cand
        )
        .expect("answers")
        .contains(mid_flight));
    drop(generation);

    // And after a restart, which is what reads the list rather than the process's own composition.
    drop(engine);
    let restarted = open_engine_publishing(&fx.bundle, &cache, &wal);
    let (generation, cand) = live_candidate(&restarted);
    assert!(generation
        .filter_columns
        .resolve(
            "department",
            &FilterOperand::Equals(AttrLocalId::new(fx.codes["sales"])),
            &cand
        )
        .expect("answers")
        .contains(mid_flight));
}

/// **The flip opens the filter columns over the new prefix**, rather than cloning the live
/// generation's.
///
/// The clone is the shape this replaced, and its symptom is not a wrong answer — a folded entity is
/// outside every candidate anyway, so the values a stale column serves are unreachable — which is
/// exactly why it needs a test that looks at the *mappings* rather than at an answer. What a clone
/// costs is the fold's reason for existing: the superseded prefix's files stay mapped for the
/// process's lifetime, so the reclamation unlinks directory entries and frees nothing, and the
/// bundle keeps a second copy of every value column on disc until a restart.
///
/// Linux-only, and skipped rather than failed elsewhere: `/proc/self/maps` is the only route to the
/// question, and the alternative — asserting on an answer — is the thing that does not work here.
#[test]
fn the_flip_maps_the_new_prefixs_columns_and_not_the_superseded_ones() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-fold-maps");
    let wal = fx._dir.path().join("wal-fold-maps");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);
    let Ok(before) = std::fs::read_to_string("/proc/self/maps") else {
        return;
    };
    let folded_attrs = fx
        .bundle
        .join("v00001")
        .join("partitions")
        .join(&fx.phash)
        .join("attrs")
        .display()
        .to_string();
    assert!(!before.contains(&folded_attrs));

    fold(&engine);
    let after = std::fs::read_to_string("/proc/self/maps").expect("maps");
    assert!(
        after.contains(&folded_attrs),
        "nothing in this process maps the folded prefix's value columns, so the generation is \
         serving the superseded prefix's mappings — files the reclamation has just unlinked"
    );
}

// ---------------------------------------------------------------------------------------------
// The extent coalesce (`filter-index.md` §5.2)
//
// Two files per column per flush is ~31,000 a day at sixteen columns, and nothing between folds
// bounds it. The pass that does is the engine's entity-space coalesce, and these are the claims it
// has to make good on: the file count comes **down**, every answer is unchanged because the merge
// is a content-preserving re-encode, the bound survives a restart, a coalesced extent coalesces
// again, and **nothing retires** — a deleted entity's value rides through, because removal is the
// fold's (Rule F).
// ---------------------------------------------------------------------------------------------

/// The coalesce policy's width: a column needs this many extents before a window is selected.
const COALESCE_WIDTH: usize = 8;

/// One flush per ingested row, with the merge held off so the assertions are about this axis.
///
/// The row-space merge is independent and safe beside a coalesce — each discards a plan that no
/// longer rebases — but not deterministic enough to assert list lengths against.
fn engine_for_coalesce(fx: &Fixture, tag: &str) -> tessera_engine::Engine {
    let engine = open_engine_publishing(
        &fx.bundle,
        &fx._dir.path().join(format!("cache-{tag}")),
        &fx._dir.path().join(format!("wal-{tag}")),
    );
    engine.set_merge_for_test(false);
    engine
}

/// Ingest and flush `COALESCE_WIDTH` rows, then drive the tick the coalesce is selected on and wait
/// for it to publish. Returns the entities, in ingest order.
fn flush_a_window(engine: &tessera_engine::Engine, tag: &str, from: usize) -> Vec<u64> {
    let coalesces = engine.write_executor_stats().coalesces;
    let entities: Vec<u64> = (from..from + COALESCE_WIDTH)
        .map(|i| {
            ingest_and_flush_with(
                engine,
                &format!("{tag}-{i}"),
                WalScalar::Utf8(["eng", "sales", "legal"][i % 3].to_string()),
                WalScalar::Utf8(["xx", "yy", "zz"][i % 3].to_string()),
                &format!("{tag}-title-{i:02}"),
                1_000 + i as i32,
                WalScalar::Null,
            )
        })
        .collect();
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while engine.write_executor_stats().coalesces <= coalesces {
        assert!(
            std::time::Instant::now() < deadline,
            "the coalesce never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    entities
}

/// This partition's live `attr_extents`, per column.
fn extents_per_column(fx: &Fixture) -> BTreeMap<String, usize> {
    let opened = open_bundle(&fx.bundle).unwrap();
    let phash = opened.partitions.keys().next().unwrap().clone();
    let mut counts = BTreeMap::new();
    for extent in &opened.partitions[&phash].manifest.attr_extents {
        *counts.entry(extent.column.clone()).or_insert(0) += 1;
    }
    counts
}

/// Every filter answer this fixture can express, over the live candidate — the whole surface a
/// content-preserving re-encode has to leave alone.
fn every_answer(engine: &tessera_engine::Engine, fx: &Fixture) -> Vec<(String, Vec<u32>)> {
    let (generation, cand) = live_candidate(engine);
    let mut out = Vec::new();
    for key in ["eng", "sales", "legal"] {
        for (column, codes) in [("department", &fx.codes), ("archive", &fx.archive_codes)] {
            let code = match column {
                "department" => codes[key],
                _ => {
                    codes[match key {
                        "eng" => "xx",
                        "sales" => "yy",
                        _ => "zz",
                    }]
                }
            };
            let answer = generation
                .filter_columns
                .resolve(
                    column,
                    &FilterOperand::Equals(AttrLocalId::new(code)),
                    &cand,
                )
                .expect("answers");
            out.push((format!("{column}={key}"), answer.iter().collect()));
        }
    }
    for operand in [
        FilterOperand::TextPrefix("paper-".into()),
        FilterOperand::TextPrefix("coalesce-title-".into()),
        FilterOperand::TextContains("title-0".into()),
    ] {
        let answer = generation
            .filter_columns
            .resolve("title", &operand, &cand)
            .expect("answers");
        out.push((format!("title {operand:?}"), answer.iter().collect()));
    }
    let answer = generation
        .filter_columns
        .resolve(
            "score",
            &FilterOperand::Range {
                lo: Some(Endpoint {
                    value: Scalar::Int(1_000),
                    inclusive: true,
                }),
                hi: None,
            },
            &cand,
        )
        .expect("answers");
    out.push(("score >= 1000".to_string(), answer.iter().collect()));
    out
}

/// **A window of extents becomes one file per column, and every answer is unchanged** — and a
/// coalesced extent is coalesced again, which is the recursion the per-column selection unit is
/// what makes free.
///
/// The layers are unioned at composition, so their division into files is immaterial and what must
/// not change is the set of `(entity, column, value)` triples. Asserted as *every* answer the
/// fixture can express rather than a sample, because the failure this guards against — a merge that
/// pairs values with the wrong entities — moves some answers and not others.
///
/// **Mutations this kills:** dropping the coalesced extent from `attr_extents` (the post-build
/// entities stop matching); pushing the coalesced layer without removing the consumed ones (the
/// disjointness check refuses at the replace); merging in list order rather than in entity order
/// (the values pair with the wrong entities).
#[test]
fn a_window_of_extents_becomes_one_file_per_column_and_answers_identically() {
    let fx = fixture();
    let engine = engine_for_coalesce(&fx, "coalesce");
    let entities = flush_a_window(&engine, "coalesce", 0);

    for (column, count) in extents_per_column(&fx) {
        assert_eq!(
            count, 1,
            "column '{column}' still holds {count} extents where the window collapsed to one"
        );
        // **And the live generation serves from the coalesced layer**, base plus one — or the
        // bound is a manifest edit the running process keeps ignoring until its next restart.
        assert_eq!(
            engine.generation().filter_columns.layer_count(&column),
            Some(2),
            "column '{column}' is still served from the layers the coalesce replaced"
        );
    }
    let after = every_answer(&engine, &fx);
    let eng = AttrLocalId::new(fx.codes["eng"]);
    let (generation, cand) = live_candidate(&engine);
    let matched = generation
        .filter_columns
        .resolve("department", &FilterOperand::Equals(eng), &cand)
        .expect("answers");
    for (i, entity) in entities.iter().enumerate() {
        assert_eq!(
            matched.contains(*entity as u32),
            i % 3 == 0,
            "entity {entity} (ingested at {i}) answers 'eng' wrongly after the coalesce"
        );
    }

    // The recursion: another window's worth of flushes, and the coalesced extent is an entry in
    // the same per-column subsequence, selected at the next rung identically.
    let more = flush_a_window(&engine, "again", COALESCE_WIDTH);
    assert!(engine.write_executor_stats().coalesces >= 2);
    for (column, count) in extents_per_column(&fx) {
        assert!(
            count <= 2,
            "column '{column}' holds {count} extents; a coalesced extent must coalesce again"
        );
    }
    let (generation, cand) = live_candidate(&engine);
    let matched = generation
        .filter_columns
        .resolve("department", &FilterOperand::Equals(eng), &cand)
        .expect("answers");
    for (i, entity) in more.iter().enumerate() {
        assert_eq!(
            matched.contains(*entity as u32),
            (i + COALESCE_WIDTH).is_multiple_of(3),
            "entity {entity} answers wrongly after the second coalesce"
        );
    }
    // Every answer the first coalesce left, still whole after the second — the first window's
    // entities are inside the second coalesce's inputs, so this is what says the recursion carried
    // them rather than re-encoding only the newest layers.
    let last = every_answer(&engine, &fx);
    for ((what, before), (_, now)) in after.iter().zip(&last) {
        assert!(
            before.iter().all(|e| now.contains(e)),
            "the second coalesce lost entities from '{what}'"
        );
    }
}

/// **A restart opens what the coalesce committed.** The manifest edit and the live swap must
/// describe the same bundle, or a process that had coalesced comes back holding a different set of
/// layers than the one it was serving from — and for this artefact the symptom of getting the
/// *files* half right and the list half wrong is a bundle that opens cleanly and answers filters
/// missing every entity the window held (filter-index §6.2).
#[test]
fn a_coalesced_column_reopens_and_answers_over_every_post_build_entity() {
    let fx = fixture();
    let entities = {
        let engine = engine_for_coalesce(&fx, "coalesce-restart");
        flush_a_window(&engine, "restart", 0)
    };

    let reopened = engine_for_coalesce(&fx, "coalesce-restart-2");
    let (generation, cand) = live_candidate(&reopened);
    for (i, entity) in entities.iter().enumerate() {
        let key = ["eng", "sales", "legal"][i % 3];
        let answer = generation
            .filter_columns
            .resolve(
                "department",
                &FilterOperand::Equals(AttrLocalId::new(fx.codes[key])),
                &cand,
            )
            .expect("answers");
        assert!(
            answer.contains(*entity as u32),
            "entity {entity} lost its value to the restart"
        );
        let title = generation
            .filter_columns
            .resolve(
                "title",
                &FilterOperand::TextEquals(format!("restart-title-{i:02}")),
                &cand,
            )
            .expect("answers");
        assert!(title.contains(*entity as u32));
    }
}

/// **A coalesce retires nothing** (§5.2, §6): a deleted-but-unfolded entity's value rides through
/// untouched, because removal is the fold's alone (Rule F) and the deny model depends on the two
/// retirement routes never being conflated.
///
/// Read from the artefact rather than from an answer, and it has to be: a deleted entity is outside
/// every candidate, so a pass that *had* blanked it here would be invisible to every filter until
/// an unsuppress or a restore made it matter.
///
/// **Mutation:** hand the overlay's tombstones to `coalesce_attr_extents` and the value is gone.
#[test]
fn a_coalesce_carries_a_deleted_but_unfolded_entitys_value_through() {
    let fx = fixture();
    let engine = engine_for_coalesce(&fx, "coalesce-delete");
    let entities = flush_a_window(&engine, "kept", 0);
    let deleted = entities[3];
    engine
        .accept_change(
            tessera_types::EntityId::new(deleted),
            tessera_lifecycle::wal::ChangeOp::Delete,
        )
        .expect("a delete is accepted");
    assert!(
        engine.overlay_depth() > 0,
        "the tombstone is live, unfolded"
    );
    // A second window, so a coalesce runs with the deletion outstanding.
    flush_a_window(&engine, "kept2", COALESCE_WIDTH);

    let opened = open_bundle(&fx.bundle).unwrap();
    let phash = opened.partitions.keys().next().unwrap().clone();
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fx.bundle.join("CURRENT")).unwrap()).unwrap();
    let prefix = fx.bundle.join(current["prefix"].as_str().unwrap());
    let mut held = None;
    for extent in &opened.partitions[&phash].manifest.attr_extents {
        if extent.column != "title" {
            continue;
        }
        let column = tessera_filter::open_extent(
            &prefix.join(&extent.values),
            &prefix.join(&extent.presence),
            tessera_filter::Access::Read,
        )
        .expect("a listed extent opens");
        if let Some(value) = column.text_of(deleted as u32) {
            held = Some(value.to_string());
        }
    }
    assert_eq!(
        held.as_deref(),
        Some("kept-title-03"),
        "the deleted entity's value left the corpus at a coalesce; only the fold may retire it \
         (Rule F), and conflating the two removal rules is fail-open"
    );
}

/// **Replacing layers checks presence *equality*, where appending checks disjointness.**
///
/// The two conditions are different and only one of them is `compose`'s. A coalesced layer covering
/// less than the window it replaces would leave `covered` naming entities no layer holds, so every
/// later disjointness check tests against the wrong coverage — silently, and for the life of the
/// generation. The merge's own duplicate guard makes the mismatch unreachable, which is exactly why
/// it is cheap to verify and wrong to assume.
#[test]
fn a_coalesced_layer_that_does_not_cover_its_window_is_refused() {
    let fx = fixture();
    let extent = |entities: &[u32]| {
        let mut presence = Bitmap::new();
        for e in entities {
            presence.add(*e);
        }
        Arc::new(
            tessera_filter::ValueColumn::partial(
                tessera_filter::Codes::text(
                    entities
                        .iter()
                        .map(|e| format!("t-{e}"))
                        .collect::<Vec<_>>(),
                ),
                presence,
            )
            .unwrap(),
        )
    };
    let first = "attrs/title/extents/a.arrow".to_string();
    let second = "attrs/title/extents/b.arrow".to_string();
    let columns = fx
        .columns
        .with_extents(&[
            ("title".to_string(), first.clone(), extent(&[100, 101])),
            ("title".to_string(), second.clone(), extent(&[200, 201])),
        ])
        .expect("two extents above the build's high-water compose");

    let window =
        |values: Arc<tessera_filter::ValueColumn>| tessera_engine::filter::CoalescedWindow {
            column: "title".to_string(),
            consumed: vec![first.clone(), second.clone()],
            values_rel: "coalesced/c-1/attrs/title/values.arrow".to_string(),
            values,
        };
    let err = columns
        .with_coalesced(&[window(extent(&[100, 101, 200]))])
        .expect_err("a coalesced layer short of its window is refused");
    assert!(format!("{err}").contains("coverage"), "{err}");

    // And one naming a layer this generation does not hold: the plan and the process disagree
    // about what the bundle is, which is a refusal rather than a no-op.
    let mut stray = window(extent(&[100, 101, 200, 201]));
    stray
        .consumed
        .push("attrs/title/extents/never.arrow".to_string());
    assert!(columns.with_coalesced(&[stray]).is_err());

    // The well-formed replace, which is what the pass actually publishes.
    let next = columns
        .with_coalesced(&[window(extent(&[100, 101, 200, 201]))])
        .expect("the coalesced layer covers exactly its window");
    let mut cand = Bitmap::new();
    cand.add_range(0..300);
    let answer = next
        .resolve("title", &FilterOperand::TextEquals("t-201".into()), &cand)
        .expect("answers");
    assert_eq!(answer.iter().collect::<Vec<_>>(), vec![201]);
}

// =================================================================================================
// The row-space route (decision 0068): suppression, route agreement, and θ's anchor
// =================================================================================================

/// **A suppressed entity is gone from every answer a render-column filter gives** — the mark, the
/// count and the record alike.
///
/// This is the differential `Engine::evaluate_row_route`'s module doc names, and it pins the half
/// of the route that is easiest to get wrong and impossible to see. The row bitmap the route
/// returns *still contains the suppressed row*: the hot column holds its value and Rule S says no
/// filter artefact may ever be touched by a suppression, so the value is there to be matched. What
/// removes the entity is that the bitmap narrows the request only through `EffectiveMask`, whose
/// consumers intersect the composed mask — fragment minus overlay — last. A route that took the
/// raw fragment as its candidate, or that let the row bitmap stand as the answer, would resurrect
/// every suppressed item that happens to match the predicate, and would do so while every count
/// stayed self-consistent.
///
/// The unsuppress at the end is what distinguishes "hidden" from "never matched": the entity comes
/// back through the same filter, so its value was in the column throughout.
#[test]
fn a_suppressed_entity_is_absent_from_a_render_column_filters_marks_counts_and_record() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-row-suppress");
    let wal = fx._dir.path().join("wal-row-suppress");
    let engine = open_engine_publishing(&fx.bundle, &cache, &wal);
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    // Source 1 carries "sales"; the predicate is its own department, so the entity matches the
    // filter and only the suppression can remove it.
    let source = 1u64;
    let department = department_of(source).expect("source 1 carries a department");
    let entity = fx.entity_of[&source];
    let id = engine
        .tessera_id_of(tessera_types::EntityId::new(entity))
        .expect("identity is computable");
    let raw = id.raw();
    let predicate = leaf(
        "department",
        FilterOperand::Equals(AttrLocalId::new(fx.codes[department])),
    );

    let filtered = |engine: &tessera_engine::Engine, session: &tessera_engine::Session| {
        engine
            .viewport(
                session,
                ViewportRequest::new("s0", 0, FULL_VIEWPORT, 10_000).filter(predicate.clone()),
            )
            .expect("the filtered viewport answers")
    };

    let before = filtered(&engine, &session);
    let drawn: std::collections::BTreeSet<u64> = before.points.tessera_ids.iter().copied().collect();
    assert!(
        drawn.contains(&raw),
        "the fixture is degenerate: the entity must match the filter before it is suppressed"
    );
    let matched = |out: &tessera_engine::ViewportOut| -> u64 {
        out.tiles.iter().map(|t| t.matched).sum()
    };
    let matched_before = matched(&before);

    engine
        .accept_change(
            tessera_types::EntityId::new(entity),
            tessera_lifecycle::wal::ChangeOp::Suppress,
        )
        .expect("a suppression is accepted");

    // A session authorised before the suppression must not still see it: the mask composes
    // against the live overlay, not the one that existed at authorise.
    let after = filtered(&engine, &session);
    let drawn: std::collections::BTreeSet<u64> = after.points.tessera_ids.iter().copied().collect();
    assert!(
        !drawn.contains(&raw),
        "a suppressed entity was drawn as a mark by a render-column filter"
    );
    assert_eq!(
        matched(&after),
        matched_before - 1,
        "the filtered count still counts the suppressed entity"
    );
    assert!(
        engine
            .item(&session, id, None)
            .expect("the drill-down succeeds")
            .is_none(),
        "a suppressed entity still answers drill-down"
    );

    engine
        .accept_change(
            tessera_types::EntityId::new(entity),
            tessera_lifecycle::wal::ChangeOp::Unsuppress,
        )
        .expect("an unsuppress is accepted");
    let restored = filtered(&engine, &session);
    let drawn: std::collections::BTreeSet<u64> =
        restored.points.tessera_ids.iter().copied().collect();
    assert!(
        drawn.contains(&raw),
        "the unsuppress must reveal the entity through the same filter — proving its value was in \
         the column throughout, hidden rather than erased (Rule S)"
    );
}

/// **Both routes answer the same question over the same window**, which is the whole licence for
/// admitting a second operand kind (decision 0068): a row-space result is exact over the request's
/// domain, so the route may be chosen on cost alone.
///
/// The route rule is `rows_in_ranges ≤ |M_auth|`, so what selects the route is the request's row
/// span against the principal's own visible total — the same predicate, the same mask, once
/// through each route, must draw the same marks over the same domain. The counters assert each
/// request landed on the route it was meant to exercise rather than leaving the equality to hold
/// trivially because both took the same one.
///
/// **The mask is a subset credential, and that is what makes the pair reachable.** Under full
/// coverage the whole extent's row span *equals* the visible total, so the rule's `≤` chooses row
/// space for every window and the entity route is unreachable from this fixture. A principal who
/// sees part of the corpus has a visible total below the full extent's row span, which is the
/// ordinary shape the rule was written for.
#[test]
fn the_row_route_and_the_entity_route_agree_over_the_domain() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-route-agree");
    let wal = fx._dir.path().join("wal-route-agree");
    let engine = open_engine_uncapped(&fx.bundle, &cache, &wal);
    let session = engine.authorise(&subset_credential()).unwrap();

    let predicate = FilterExpr::AnyOf(
        ["eng", "sales"]
            .iter()
            .map(|d| {
                leaf(
                    "department",
                    FilterOperand::Equals(AttrLocalId::new(fx.codes[*d])),
                )
            })
            .collect(),
    );
    let ids = |out: &tessera_engine::ViewportOut| -> std::collections::BTreeSet<u64> {
        out.points.tessera_ids.iter().copied().collect()
    };

    // A narrow window: few rows in range, so the row route is the cheaper one and is chosen.
    let narrow = [0.0, 0.0, 400.0, 400.0];
    let before = engine.filter_row_routes();
    let narrow_filtered = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 8, narrow, 10_000).filter(predicate.clone()),
        )
        .expect("the narrow window answers filtered");
    assert_eq!(
        engine.filter_row_routes() - before,
        1,
        "the narrow request was supposed to take the row route; the window or the rule moved"
    );

    // The same window, unfiltered, gives the domain the comparison is made over.
    let narrow_all = ids(&engine
        .viewport(&session, ViewportRequest::new("s0", 8, narrow, 10_000))
        .expect("the narrow window answers unfiltered"));

    // The whole extent: rows in range now exceed the principal's total, so the entity route runs.
    let before = engine.filter_row_routes();
    let full_filtered = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, FULL_VIEWPORT, 10_000).filter(predicate),
        )
        .expect("the full extent answers filtered");
    assert_eq!(
        engine.filter_row_routes() - before,
        0,
        "the full-extent request was supposed to take the entity route"
    );

    let (row_marks, entity_marks) = (ids(&narrow_filtered), ids(&full_filtered));
    assert_eq!(
        row_marks,
        entity_marks
            .intersection(&narrow_all)
            .copied()
            .collect::<std::collections::BTreeSet<u64>>(),
        "the row route's marks differ from the entity route's over the same domain"
    );
    assert!(
        !row_marks.is_empty() && row_marks.len() < narrow_all.len(),
        "non-degenerate in both directions: the window holds matches, and the filter removed some"
    );
}

/// **A filter narrows the selection and never moves θ** (I3/I12): the label frontier anchors on the
/// unfiltered composed mask, so the same viewport answers the same θ filtered or not.
///
/// This is the invariant a row-space route is most likely to break by accident, because the route
/// is evaluated inside the same sweep that computes the anchor — and an anchor taken after the
/// filter would let a caller move the frontier by narrowing, which is a disclosure rather than a
/// view. The unfiltered total is asserted alongside it: it is `visible_total()`, computed above the
/// filter and deliberately blind to it.
#[test]
fn a_render_column_filter_narrows_the_selection_without_moving_the_anchor() {
    let fx = fixture();
    let cache = fx._dir.path().join("cache-anchor");
    let wal = fx._dir.path().join("wal-anchor");
    let engine = open_engine_uncapped(&fx.bundle, &cache, &wal);
    let session = engine.authorise(&full_coverage_credential()).unwrap();

    let narrow = [0.0, 0.0, 400.0, 400.0];
    let unfiltered = engine
        .viewport(&session, ViewportRequest::new("s0", 8, narrow, 10_000))
        .expect("unfiltered");
    let before = engine.filter_row_routes();
    let filtered = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 8, narrow, 10_000).filter(leaf(
                "department",
                FilterOperand::Equals(AttrLocalId::new(fx.codes["eng"])),
            )),
        )
        .expect("filtered");
    assert_eq!(
        engine.filter_row_routes() - before,
        1,
        "this test is only meaningful if the row route ran"
    );

    // `TileCount::visible` is the anchor's own input — the composed mask's count for the tile,
    // computed above the filter — while `matched` is what the filter narrowed to. The first must
    // be identical across the pair; the second must not be.
    let anchor = |out: &tessera_engine::ViewportOut| -> Vec<(u64, u64)> {
        out.tiles.iter().map(|t| (t.tile, t.visible)).collect()
    };
    let (visible, visible_unfiltered) = (anchor(&filtered), anchor(&unfiltered));
    assert_eq!(
        visible, visible_unfiltered,
        "the filter moved the anchor: θ anchors on the unfiltered composed mask (I3/I12)"
    );
    let matched: u64 = filtered.tiles.iter().map(|t| t.matched).sum();
    let visible_total: u64 = filtered.tiles.iter().map(|t| t.visible).sum();
    assert!(
        matched < visible_total,
        "the filter must actually have narrowed the matched set beneath the anchor"
    );
    let drawn: std::collections::BTreeSet<u64> = filtered.points.tessera_ids.iter().copied().collect();
    let all: std::collections::BTreeSet<u64> = unfiltered.points.tessera_ids.iter().copied().collect();
    assert!(
        drawn.is_subset(&all) && drawn.len() < all.len(),
        "a filter may only narrow (I12), and this one must actually have narrowed"
    );
}
