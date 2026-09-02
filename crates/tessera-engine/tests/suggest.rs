//! **Value suggestion, end to end**: a built bundle, a real principal's mask, and the page a
//! `GET /v1/categories/{column}/suggest` would serve (`docs/design/value-suggestion.md`).
//!
//! What this file exists to pin is the **gate**, which is the one thing about this surface that can
//! be wrong invisibly: the index is principal-blind by construction and holds every value of the
//! vocabulary, visible to the asking principal or not, so a walk that forgot its predicate would
//! return a longer page and pass every shape assertion. Every case here therefore asks the same
//! question of three principals — one who sees everything, one who sees a third of the corpus, and
//! one who sees nothing — and the expected answers are computed from the fixture's own inputs
//! rather than from the index.
//!
//! The index's own shape — the sorted order, the runs, the side map, the retraction set — is
//! covered by `tessera_engine::suggest`'s unit tests. This is the layer above them.

mod common;

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use tessera_analyse::SuggestionField;
use tessera_build::config::{Config, Schema};
use tessera_build::{build, BuildArgs};
use tessera_engine::{Engine, EngineError, SuggestPage};
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::WalScalar;

const N: u64 = 60;

/// Two titled vocabularies over one partition of the corpus, under the two visibilities.
///
/// `department` is `derived` — a value is offered iff the asking principal can see an item carrying
/// it — and `archive` is `public`, whose value names are an authored assertion and are served to
/// anybody with a session. Both carry a value nothing in the corpus carries (`ops`, `ww`), which is
/// the pair that separates the two rules: the `public` one is still offered and the `derived` one
/// is offered to nobody.
///
/// The titles are chosen so that a word start is reachable by a prefix that matches no key and no
/// whole title — `counsel`, `marketing`, `room` — and so that one value's *key* and another's
/// *title* both begin with `l`.
const SCHEMA_TOML: &str = r#"
[sources]
departments = "departments.parquet"
archives    = "archives.parquet"

[[vocabulary]]
name       = "department"
width      = "u16"
value_set  = "closed"
visibility = "derived"
source     = "departments"

[[vocabulary]]
name       = "archive"
width      = "u16"
value_set  = "closed"
visibility = "public"
source     = "archives"

[[attribute]]
name       = "department"
type       = "category"
render     = true
index      = true
vocabulary = "department"

[[attribute]]
name       = "archive"
type       = "category"
render     = true
index      = true
vocabulary = "archive"
"#;

const DEPARTMENTS: &[(&str, u32, &str)] = &[
    ("eng", 1, "Engineering Team"),
    ("sales", 2, "Sales and Marketing"),
    ("legal", 3, "Legal Counsel"),
    ("ops", 4, "Operations"),
];

const ARCHIVES: &[(&str, u32, &str)] = &[
    ("xx", 11, "Machine Room"),
    ("yy", 22, "Sales Archive"),
    ("zz", 33, "Legal Archive"),
    ("ww", 44, "Warehouse"),
];

/// Source id → department key. Every fifth item carries none.
///
/// **Decorrelated from the permission model**, as `filtering.rs`' own copy is and for its reason:
/// `terms_of` grants `SUBSET_TERM` on `e % 3`, so a department keyed on `e % 3` would make the
/// gate and the fixture select the same items for different reasons.
fn department_of(e: u64) -> Option<&'static str> {
    if e.is_multiple_of(5) {
        None
    } else {
        Some(["eng", "sales", "legal"][(e / 2 % 3) as usize])
    }
}

fn archive_of(e: u64) -> Option<&'static str> {
    match department_of(e) {
        Some("eng") => Some("xx"),
        Some("sales") => Some("yy"),
        Some("legal") => Some("zz"),
        _ => None,
    }
}

/// Which source ids a credential's principal may see, from the term model alone.
fn visible_to(credential: &[u8]) -> Vec<u64> {
    let subset = subset_credential();
    let full = full_coverage_credential();
    (0..N)
        .filter(|&e| {
            if credential == full.as_slice() {
                true
            } else if credential == subset.as_slice() {
                terms_of(e).contains(&SUBSET_TERM)
            } else {
                false
            }
        })
        .collect()
}

fn write_vocabulary(path: &Path, values: &[(&str, u32, &str)]) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("code", DataType::UInt32, false),
        Field::new("title", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(
                values.iter().map(|v| v.0).collect::<Vec<_>>(),
            )),
            Arc::new(UInt32Array::from(
                values.iter().map(|v| v.1).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                values.iter().map(|v| v.2).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_points(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("department", DataType::Utf8, true),
        Field::new("archive", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let departments: Vec<Option<String>> = ids
        .iter()
        .map(|&e| department_of(e).map(str::to_string))
        .collect();
    let archives: Vec<Option<String>> = ids
        .iter()
        .map(|&e| archive_of(e).map(str::to_string))
        .collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(departments)),
            Arc::new(StringArray::from(archives)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// `points.parquet` with `entity_id`, `x`, `y` and one utf8 column that is null for every row.
fn write_points_with_absent_column(path: &Path, column: &str) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new(column, DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(vec![None::<String>; N as usize])),
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
}

fn parse_schema(dir: &Path, text: &str) -> Schema {
    let path = dir.join("schema.toml");
    std::fs::write(&path, text).unwrap();
    Config::parse(&path, &HashMap::new())
        .expect("the fixture schema parses")
        .schema
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points(&points);
    write_pairs_n(&pairs, N);
    write_vocabulary(&dir.path().join("departments.parquet"), DEPARTMENTS);
    write_vocabulary(&dir.path().join("archives.parquet"), ARCHIVES);
    let schema = parse_schema(dir.path(), SCHEMA_TOML);
    let bundle = dir.path().join("bundle");

    build(&BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs.clone()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(points.clone(), &schema),
        out: bundle.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    })
    .expect("the fixture builds");

    Fixture { _dir: dir, bundle }
}

fn engine_for(fx: &Fixture, tag: &str) -> Engine {
    open_engine(
        &fx.bundle,
        &fx._dir.path().join(format!("cache-{tag}")),
        &fx._dir.path().join(format!("wal-{tag}.log")),
    )
}

/// One suggestion page, for a principal, at the generous limits every case but the `more` ones
/// wants.
fn page(engine: &Engine, credential: &[u8], column: &str, q: &str) -> SuggestPage {
    page_with(engine, credential, column, q, 20, false, 100_000)
}

fn page_with(
    engine: &Engine,
    credential: &[u8],
    column: &str,
    q: &str,
    limit: usize,
    counts: bool,
    budget: u64,
) -> SuggestPage {
    let session = engine.authorise(credential).expect("the credential resolves");
    engine
        .suggest(&session, column, q, limit, counts, budget)
        .expect("the column suggests")
        .expect("the column is a category")
}

fn keys(page: &SuggestPage) -> Vec<String> {
    page.values.iter().map(|v| v.key.clone()).collect()
}

// ---------------------------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------------------------

/// **A `derived` column offers exactly the values with a member this principal can see** — nothing
/// more for a narrow principal, and nothing at all for one who can see no item.
///
/// The expected set is computed from the fixture's own inputs. `ops` is the control: it is a
/// declared value the corpus carries nowhere, so it must be offered to nobody however wide their
/// mask, and a walk that skipped its predicate would offer it to everyone.
///
/// **Mutations this kills:** dropping the predicate; applying it to the page rather than to the
/// walk; treating an empty candidate as "everything"; offering a value whose posting exists but
/// whose members are all invisible.
#[test]
fn a_derived_column_offers_only_values_with_a_visible_member() {
    let fx = fixture();
    let engine = engine_for(&fx, "gate");

    for credential in [
        full_coverage_credential(),
        subset_credential(),
        zero_credential(),
    ] {
        let visible = visible_to(&credential);
        let mut expected: Vec<String> = DEPARTMENTS
            .iter()
            .map(|v| v.0)
            .filter(|key| {
                visible
                    .iter()
                    .any(|&e| department_of(e) == Some(*key))
            })
            .map(str::to_string)
            .collect();
        expected.sort_unstable();

        // The empty query is the widest range there is — every entry in the index — so it is the
        // strongest form of this assertion.
        let mut offered = keys(&page(&engine, &credential, "department", ""));
        offered.sort_unstable();
        assert_eq!(
            offered, expected, "credential {credential:?}, empty query");

        assert!(
            !offered.iter().any(|k| k == "ops"),
            "a value the corpus carries nowhere was offered"
        );
        // And by prefix, which is the surface's own route rather than the whole-set one.
        for (prefix, key) in [("op", "ops"), ("eng", "eng"), ("le", "legal")] {
            let offered = keys(&page(&engine, &credential, "department", prefix));
            assert_eq!(
                offered.iter().any(|k| k == key),
                expected.iter().any(|k| k == key),
                "prefix {prefix:?} and the empty query disagree about {key}"
            );
        }
    }
}

/// **A viewer who can see no item is told nothing, and is told it the same way an absent value
/// is** — an empty page rather than a refusal, a status or a gap.
#[test]
fn a_viewer_who_sees_nothing_gets_no_value_and_no_span() {
    let fx = fixture();
    let engine = engine_for(&fx, "zero");
    for q in ["", "e", "eng", "team", "counsel", "zzz"] {
        let got = page(&engine, &zero_credential(), "department", q);
        assert!(got.values.is_empty(), "query {q:?} offered something");
        assert!(!got.more, "query {q:?} claimed there was more");
    }
}

/// **A `public` value set is served as authored, to anybody with a session** — including the value
/// nothing in the corpus carries, which is the whole of the difference from `derived`.
#[test]
fn a_public_column_is_suggested_as_authored() {
    let fx = fixture();
    let engine = engine_for(&fx, "public");
    for credential in [
        full_coverage_credential(),
        subset_credential(),
        zero_credential(),
    ] {
        let mut offered = keys(&page(&engine, &credential, "archive", ""));
        offered.sort_unstable();
        assert_eq!(offered, ["ww", "xx", "yy", "zz"]);
        assert_eq!(
            keys(&page(&engine, &credential, "archive", "warehouse")),
            ["ww"],
            "a value nothing carries is still an authored name"
        );
    }
}

/// An undeclared name and a plain scalar are one outcome, exactly as they are on the enumeration.
#[test]
fn a_name_that_is_not_a_category_is_no_answer_rather_than_a_refusal() {
    let fx = fixture();
    let engine = engine_for(&fx, "unknown");
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    for column in ["nonesuch", "x", "entity_id"] {
        assert!(engine
            .suggest(&session, column, "", 20, false, 100_000)
            .unwrap()
            .is_none());
    }
}

// ---------------------------------------------------------------------------------------------
// What matches
// ---------------------------------------------------------------------------------------------

/// **The three entry kinds, reachable through the gate**, with `match` in characters of the served
/// string.
#[test]
fn a_key_a_title_and_a_word_start_all_match() {
    let fx = fixture();
    let engine = engine_for(&fx, "match");
    let full = full_coverage_credential();

    // The key.
    let got = page(&engine, &full, "department", "eng");
    assert_eq!(keys(&got), ["eng"]);
    let span = got.values[0].span;
    assert_eq!(span.field, SuggestionField::Key);
    assert_eq!((span.start, span.len), (0, 3));

    // The whole title.
    let got = page(&engine, &full, "department", "engineering t");
    assert_eq!(keys(&got), ["eng"]);
    let span = got.values[0].span;
    assert_eq!(span.field, SuggestionField::Title);
    assert_eq!((span.start, span.len), (0, 13));

    // A word start, reported against the field the word came from and the start of that word.
    let got = page(&engine, &full, "department", "counsel");
    assert_eq!(keys(&got), ["legal"]);
    let span = got.values[0].span;
    assert_eq!(span.field, SuggestionField::Title);
    assert_eq!(
        (span.start, span.len),
        (6, 7),
        "'Legal Counsel' — the word starts at character 6"
    );

    // The word start of a *later* word, which is what infix matching is usually wanted for.
    let got = page(&engine, &full, "department", "market");
    assert_eq!(keys(&got), ["sales"]);
    assert_eq!(got.values[0].span.start, 10, "'Sales and Marketing'");
}

/// **The fold is applied to the query and to the index alike**, so case, NFKC and whitespace are
/// not a matching decision.
#[test]
fn the_query_is_folded_before_it_is_matched() {
    let fx = fixture();
    let engine = engine_for(&fx, "fold");
    let full = full_coverage_credential();
    for q in [
        "ENG",
        "Eng",
        "  eng  ",
        "ＥＮＧ",
        "engineering team",
        "ENGINEERING   TEAM",
    ] {
        assert_eq!(
            keys(&page(&engine, &full, "department", q)),
            ["eng"],
            "query {q:?}"
        );
    }
}

/// **A value appears once**, at its first matching entry in §7's order, even where several of its
/// entries are under the prefix.
#[test]
fn a_value_under_a_prefix_by_two_entries_is_served_once() {
    let fx = fixture();
    let engine = engine_for(&fx, "once");
    // `sales` matches by key and, through `Sales and Marketing`, by title too.
    let got = page(&engine, &full_coverage_credential(), "department", "sale");
    assert_eq!(keys(&got), ["sales"]);
    assert_eq!(
        got.values[0].span.field,
        SuggestionField::Key,
        "a key match precedes a title match at the same entry string (§7)"
    );
}

/// The empty query is the picker's initial list: every visible value, in index order, and no span.
#[test]
fn an_empty_query_is_the_whole_visible_set() {
    let fx = fixture();
    let engine = engine_for(&fx, "empty");
    let got = page(&engine, &full_coverage_credential(), "department", "");
    let mut offered = keys(&got);
    offered.sort_unstable();
    assert_eq!(offered, ["eng", "legal", "sales"]);
    assert!(got.values.iter().all(|v| v.span.len == 0));
}

// ---------------------------------------------------------------------------------------------
// `more`, and counts
// ---------------------------------------------------------------------------------------------

/// **`more` on both of its conditions, and on neither.**
///
/// A page that fills is `true`; a walk whose budget runs out is `true` — and that arm is the one
/// that carries C31's one bit, a thresholded pre-mask count of the values under the prefix. A page
/// that exhausts its range exactly is `false`, which is what makes the flag mean anything.
#[test]
fn more_is_true_on_a_filled_page_and_on_a_spent_budget() {
    let fx = fixture();
    let engine = engine_for(&fx, "more");
    let full = full_coverage_credential();

    // Exhausted: three visible values, a limit above them, a budget above the index.
    let got = page_with(&engine, &full, "department", "", 20, false, 100_000);
    assert_eq!(got.values.len(), 3);
    assert!(!got.more, "the range was exhausted");

    // Filled.
    let got = page_with(&engine, &full, "department", "", 2, false, 100_000);
    assert_eq!(got.values.len(), 2);
    assert!(got.more);

    // Spent budget: one value examined, and the walk stops with the range unexhausted.
    let got = page_with(&engine, &full, "department", "", 20, false, 1);
    assert_eq!(got.values.len(), 1);
    assert!(got.more);

    // And a budget spent entirely on values the principal cannot see still says so — this is the
    // arm §8 registers, where `more: false` would hide visible values behind a flag saying there
    // were none.
    let got = page_with(&engine, &zero_credential(), "department", "", 20, false, 1);
    assert!(got.values.is_empty());
    assert!(got.more);
}

/// **A count is the viewer's own number**, exact, and agrees with what a filter on that value
/// returns for the same principal.
#[test]
fn a_count_is_the_number_of_items_this_viewer_may_see() {
    let fx = fixture();
    let engine = engine_for(&fx, "counts");
    for credential in [full_coverage_credential(), subset_credential()] {
        let visible = visible_to(&credential);
        let got = page_with(&engine, &credential, "department", "", 20, true, 100_000);
        assert!(!got.values.is_empty());
        for value in &got.values {
            let expected = visible
                .iter()
                .filter(|&&e| department_of(e) == Some(value.key.as_str()))
                .count() as u64;
            assert_eq!(value.count, Some(expected), "{}", value.key);
            assert!(expected > 0, "a value with no visible member was offered");
        }
        // Without the flag there is no number at all — never a `0` standing in for one.
        let got = page_with(&engine, &credential, "department", "", 20, false, 100_000);
        assert!(got.values.iter().all(|v| v.count.is_none()));
    }
}

// ---------------------------------------------------------------------------------------------
// The side map, at the seam a mint actually arrives through
// ---------------------------------------------------------------------------------------------

/// An **open, `public`** vocabulary: keys are minted at ingest, and their names are an authored
/// assertion, so the mint alone decides whether a value is suggestible.
const OPEN_PUBLIC: &str = r#"
[[vocabulary]]
name       = "team"
width      = "u16"
value_set  = "open"
visibility = "public"

[[attribute]]
name       = "team"
type       = "category"
render     = true
vocabulary = "team"
"#;

/// **A value minted by an ingest is suggestible on the next keystroke**, before any flush — which
/// is exactly what the side map exists for, the base index having been built at open from a
/// vocabulary that did not contain it.
///
/// **Mutations this kills:** feeding the side map from the rebuild rather than from the mint;
/// carrying the previous generation's index across the window that minted; ranging the side map by
/// the unfolded query.
#[test]
fn a_value_minted_at_ingest_is_suggested_before_any_flush() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    // A `team` column carried by no row: every binding this fixture acquires is minted through
    // ingest, never through the build, which is what makes the base index start empty.
    write_points_with_absent_column(&points, "team");
    write_pairs_n(&pairs, N);
    let schema = parse_schema(dir.path(), OPEN_PUBLIC);
    let bundle = dir.path().join("bundle");
    build(&BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs.clone()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(points.clone(), &schema),
        out: bundle.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    })
    .expect("the fixture builds");

    let mut engine = open_engine(
        &bundle,
        &dir.path().join("cache"),
        &dir.path().join("wal.log"),
    );
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);

    let full = full_coverage_credential();
    assert!(
        page(&engine, &full, "team", "").values.is_empty(),
        "the vocabulary starts empty"
    );

    engine
        .accept_ingest(
            vec![UnallocatedRow {
                external_id: Some(b"row-1".to_vec()),
                view: "s0".to_string(),
                join: None,
                descriptors: vec![b"0".to_vec()],
                x: 1.0,
                y: 1.0,
                scalars: vec![WalScalar::Utf8("Platform Infrastructure".to_string())],
                terms: engine.resolve_terms(&[b"0".to_vec()]),
                scoped: Vec::new(),
            }],
            "batch-1".to_string(),
            [0u8; 32],
        )
        .expect("the ingest is accepted");

    // The whole key, and the word start of its second word — the side map holds §4's entry set,
    // not just the key.
    for q in ["platform", "PLATFORM  inf", "infra"] {
        assert_eq!(
            keys(&page(&engine, &full, "team", q)),
            ["Platform Infrastructure"],
            "query {q:?}"
        );
    }
    let infra = page(&engine, &full, "team", "infra");
    assert_eq!(
        (infra.values[0].span.field, infra.values[0].span.start),
        (SuggestionField::Key, 9),
        "a title-less value's word starts come from its key"
    );
    assert!(infra.values[0].title.is_none());
}

/// A column whose vocabulary has no suggestion index refuses, rather than answering empty — the
/// same reasoning the enumeration refuses an underivable predicate on.
#[test]
fn a_column_with_no_index_refuses_rather_than_answering_empty() {
    let fx = fixture();
    let engine = engine_for(&fx, "refuse");
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    // Reached by removing the built index from under the live engine, which is the fault state the
    // refusal is for: nothing request-shaped can produce it.
    let generation = engine.generation();
    let index = generation
        .suggest
        .get("department")
        .expect("the fixture builds one");
    std::fs::remove_dir_all(index.base().dir()).expect("the directory is the engine's own");
    drop(generation);
    // The mapping outlives the directory entry, so this still answers — which is the property the
    // rebuild's unlink depends on, asserted here rather than assumed.
    assert!(engine
        .suggest(&session, "department", "eng", 20, false, 100_000)
        .is_ok());

    // The refusal itself is reachable only through a vocabulary with no entry at all.
    let missing = engine.suggest(&session, "department@s0:none", "", 20, false, 100_000);
    assert!(matches!(
        missing,
        Ok(None) | Err(EngineError::SuggestionUnavailable { .. })
    ));
}
