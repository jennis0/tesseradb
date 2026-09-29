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
use rustc_hash::FxHashSet;
use tessera_analyse::SuggestionField;
use tessera_build::config::{Config, Schema};
use tessera_build::{build, BuildArgs};
use tessera_engine::filter::{FilterExpr, FilterOperand, RegionLeaf};
use tessera_engine::shapes::ShapeF64;
use tessera_engine::{Engine, EngineError, SuggestPage, SuggestRequest};
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::WalScalar;
use tessera_spatial::fixed32;
use tessera_spatial::shape::Space;
use tessera_types::AttrLocalId;

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

/// The items of the second view, `s1`: every item but those carrying `legal`, so a value visible
/// in `s0` is carried by no item of `s1`.
fn in_second_view(e: u64) -> bool {
    department_of(e) != Some("legal")
}

/// `s1`'s geometry: `entity_id`, `x` and `y` for the items [`in_second_view`] keeps.
fn write_second_view(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..N).filter(|&e| in_second_view(e)).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 41) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 43) % 1000) as f64).collect();
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
    /// The `CURRENT` prefix, so a test can map source ids to the entity ids the build assigned.
    prefix: String,
}

fn build_args(points: &Path, pairs: &Path, out: &Path, schema: Schema) -> BuildArgs {
    let schema = with_id(schema);
    let view = |view_id: &str, points: &Path| tessera_build::ViewArgs {
        visibility: None,
        view_id: view_id.to_string(),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: points.to_path_buf(),
        point_fields: Default::default(),
        select: None,
        access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
    };
    let second = points.with_file_name("second-view.parquet");
    let mut views = vec![view("s0", points)];
    if second.exists() {
        views.push(view("s1", &second));
    }
    BuildArgs {
        views,
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(
            points.to_path_buf(),
            &schema,
        ),
        out: out.to_path_buf(),
        limit: None,
        identity_key: test_key(),
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    }
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
    write_second_view(&dir.path().join("second-view.parquet"));
    write_pairs_n(&pairs, N);
    write_vocabulary(&dir.path().join("departments.parquet"), DEPARTMENTS);
    write_vocabulary(&dir.path().join("archives.parquet"), ARCHIVES);
    let schema = parse_schema(dir.path(), SCHEMA_TOML);
    let bundle = dir.path().join("bundle");

    build(&build_args(&points, &pairs, &bundle, schema)).expect("the fixture builds");

    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle.join("CURRENT")).unwrap()).unwrap();
    let prefix = current["prefix"].as_str().unwrap().to_string();
    Fixture {
        _dir: dir,
        bundle,
        prefix,
    }
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

/// **The probe route, always** — `max_suggest_set_entities = 0`, so no set is ever built and no
/// case below can be answered by one.
///
/// Every case above the set-route section pins §6.2's route, which is where the walk budget is
/// spent and where `more` may mean a spent budget. The set route is exercised deliberately, by the
/// cases that hold a session still until a set lands (`with_set`), rather than raced into by
/// whichever of the two an async sweep happened to win — a race that would make `more` on a spent
/// budget flap.
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
    suggest_with(engine, &session, column, None, q, limit, counts, budget, 0)
}

#[allow(clippy::too_many_arguments)]
fn suggest_with(
    engine: &Engine,
    session: &tessera_engine::Session,
    column: &str,
    view: Option<&str>,
    q: &str,
    limit: usize,
    counts: bool,
    budget: u64,
    max_suggest_set_entities: u64,
) -> SuggestPage {
    engine
        .suggest(
            session,
            SuggestRequest {
                column,
                view,
                filter: None,
                q,
                limit,
                counts,
                walk_budget: budget,
                max_suggest_set_entities,
                cancel: None,
            },
        )
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
            .suggest(
                &session,
                SuggestRequest {
                    column,
                    view: None,
                    filter: None,
                    q: "",
                    limit: 20,
                    counts: false,
                    walk_budget: 100_000,
                    max_suggest_set_entities: 0,
                    cancel: None,
                },
            )
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
    let carrying = |column: &str, e: u64| match column {
        "department" => department_of(e),
        _ => archive_of(e),
    };
    for credential in [full_coverage_credential(), subset_credential()] {
        let visible = visible_to(&credential);
        for column in ["department", "archive"] {
            let got = page_with(&engine, &credential, column, "", 20, true, 100_000);
            assert!(!got.values.is_empty());
            for value in &got.values {
                let expected = visible
                    .iter()
                    .filter(|&&e| carrying(column, e) == Some(value.key.as_str()))
                    .count() as u64;
                assert_eq!(value.count, Some(expected), "{column}: {}", value.key);
            }
            // Without the flag there is no number at all — never a `0` standing in for one.
            let got = page_with(&engine, &credential, column, "", 20, false, 100_000);
            assert!(got.values.iter().all(|v| v.count.is_none()));
        }
    }
}

/// **Under a view, a count is the viewer's items of that view**: the values offered are the ones
/// offered without it, and a value this viewer sees in another view, carried by no item of this
/// one, counts 0. A view the viewer cannot reach is refused.
#[test]
fn a_count_under_a_view_counts_that_views_items() {
    let fx = fixture();
    let engine = engine_for(&fx, "view-counts");
    let carrying = |column: &str, e: u64| match column {
        "department" => department_of(e),
        _ => archive_of(e),
    };
    for credential in [full_coverage_credential(), subset_credential()] {
        let session = engine.authorise(&credential).expect("the credential resolves");
        let visible = visible_to(&credential);
        for column in ["department", "archive"] {
            let everywhere = suggest_with(&engine, &session, column, None, "", 20, true, 100_000, 0);
            let within =
                suggest_with(&engine, &session, column, Some("s1"), "", 20, true, 100_000, 0);
            assert_eq!(keys(&within), keys(&everywhere), "{column}: a view changes no value offered");
            for value in &within.values {
                let expected = visible
                    .iter()
                    .filter(|&&e| {
                        in_second_view(e) && carrying(column, e) == Some(value.key.as_str())
                    })
                    .count() as u64;
                assert_eq!(value.count, Some(expected), "{column}: {}", value.key);
            }
            // Every item carrying these is outside `s1`. The full principal sees one in `s0`.
            let absent = if column == "department" { "legal" } else { "zz" };
            let served = within.values.iter().find(|v| v.key == absent);
            if credential == full_coverage_credential() {
                assert!(served.is_some(), "{column}: {absent} is offered");
            }
            if let Some(served) = served {
                assert_eq!(served.count, Some(0), "{column}: {absent} counts 0 in the view");
            }
        }
    }
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    assert!(matches!(
        engine.suggest(
            &session,
            SuggestRequest {
                column: "department",
                view: Some("nowhere"),
                filter: None,
                q: "",
                limit: 20,
                counts: true,
                walk_budget: 100_000,
                max_suggest_set_entities: 0,
                cancel: None,
            },
        ),
        Err(EngineError::UnknownView(_))
    ));
}

/// **A count takes in an item ingested and not yet flushed**, as a filter's candidate does; under
/// a view it does not until its flush, the item holding no row there before, and under a filter
/// neither. After the flush it counts once without a view. Under a view it counts once the
/// session's projection covers the flush, as the map shows it, and until then it does not, with
/// a filter passing everything or without one. A suppressed buffered item counts nowhere.
#[test]
fn a_count_takes_in_an_item_buffered_before_its_flush() {
    let fx = fixture();
    let mut engine = engine_for(&fx, "buffered-counts");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    let session = engine.authorise(&subset_credential()).unwrap();
    let count_of = |view: Option<&str>| {
        suggest_with(&engine, &session, "department", view, "", 20, true, 100_000, 0)
            .values
            .iter()
            .find(|v| v.key == "eng")
            .and_then(|v| v.count)
            .expect("eng is offered")
    };
    let everything = FilterExpr::AllOf(vec![]);
    let filtered_count = || {
        filtered(&engine, &session, "department", "s0", &everything)
            .values
            .iter()
            .find(|v| v.key == "eng")
            .and_then(|v| v.count)
            .expect("eng is offered")
    };
    let (before, before_in_view) = (count_of(None), count_of(Some("s0")));
    assert_eq!(filtered_count(), before_in_view);

    let row = |key: &str| UnallocatedRow {
        view: "s0".to_string(),
        join: None,
        descriptors: vec![b"0".to_vec(), b"1".to_vec()],
        x: 1.0,
        y: 1.0,
        scalars: vec![
            WalScalar::Utf8("eng".to_string()),
            WalScalar::Utf8("xx".to_string()),
            WalScalar::U64(key_id(key)),
        ],
        terms: engine.resolve_terms(&[b"0".to_vec(), b"1".to_vec()]),
        scoped: Vec::new(),
    };
    let entities = engine
        .ingest_rows(vec![row("kept"), row("suppressed")], "batch-1".to_string(), [0u8; 32])
        .expect("the ingest is accepted");
    engine
        .accept_change(entities[1], tessera_lifecycle::wal::ChangeOp::Suppress)
        .expect("a suppression is an ordinary change");

    assert_eq!(count_of(None), before + 1, "the buffered item counts, the suppressed one does not");
    assert_eq!(
        count_of(Some("s0")),
        before_in_view,
        "the buffered item holds no row in the view before its flush"
    );
    assert_eq!(filtered_count(), before_in_view, "nor does it count under a filter");

    let flushes = engine.write_executor_stats().flushes;
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while engine.write_executor_stats().flushes <= flushes {
        assert!(std::time::Instant::now() < deadline, "the flush never published");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(count_of(None), before + 1, "after the flush the item counts once");
    // The session's projection has not been refreshed, so the map does not show the item yet.
    assert_eq!(count_of(Some("s0")), before_in_view, "the view is counted as the map shows it");
    assert_eq!(filtered_count(), before_in_view, "and so it is under a filter");

    // With the refresh running, a second flush brings the session's projection forward over both.
    engine.set_background_refresh_for_test(true);
    engine
        .ingest_rows(vec![row("second")], "batch-2".to_string(), [1u8; 32])
        .expect("the ingest is accepted");
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while count_of(Some("s0")) != before_in_view + 2 {
        assert!(std::time::Instant::now() < deadline, "the refresh never covered the flushes");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(filtered_count(), before_in_view + 2, "both forms count them once they are shown");
}

// ---------------------------------------------------------------------------------------------
// Counts under a filter
// ---------------------------------------------------------------------------------------------

fn category(column: &str, codes: &[u32]) -> FilterExpr {
    FilterExpr::Leaf {
        column: column.to_string(),
        operand: FilterOperand::In(codes.iter().map(|&c| AttrLocalId::new(c)).collect()),
    }
}

/// A square over the fixture's extent, in the frame of the view it is evaluated in.
fn boxed(min: f64, max: f64) -> FilterExpr {
    let shape = ShapeF64::Bbox {
        min_x: min,
        min_y: min,
        max_x: max,
        max_y: max,
    }
    .canonical(Space::View, &extent())
    .expect("a well-formed box")
    .0;
    FilterExpr::Region(RegionLeaf::Shape(Arc::new(shape)))
}

/// Whether `boxed(min, max)` holds source item `e` in `view`, from each view's own positions
/// quantised as the build quantises them.
fn in_box(view: &str, e: u64, min: f64, max: f64) -> bool {
    let (x, y) = match view {
        "s0" => ((e * 37) % 1000, (e * 53) % 1000),
        _ => ((e * 41) % 1000, (e * 43) % 1000),
    };
    let q = |v: f64| fixed32(v, 0.0, 1000.0);
    let (lo, hi) = (q(min), q(max));
    let inside = |v: u64| (lo..=hi).contains(&q(v as f64));
    inside(x) && inside(y)
}

fn in_view(view: &str, e: u64) -> bool {
    view == "s0" || in_second_view(e)
}

/// One counted page under `filter` in `view`, on the probe route.
fn filtered(
    engine: &Engine,
    session: &tessera_engine::Session,
    column: &str,
    view: &str,
    filter: &FilterExpr,
) -> SuggestPage {
    engine
        .suggest(
            session,
            SuggestRequest {
                column,
                view: Some(view),
                filter: Some(filter),
                q: "",
                limit: 20,
                counts: true,
                walk_budget: 100_000,
                max_suggest_set_entities: 0,
                cancel: None,
            },
        )
        .expect("the column suggests")
        .expect("the column is a category")
}

fn code_of(engine: &Engine, column: &str, key: &str) -> u32 {
    page(engine, &full_coverage_credential(), column, "")
        .values
        .iter()
        .find(|v| v.key == key)
        .map(|v| v.code)
        .expect("the value is offered to the full principal")
}

/// **Under a filter, a count is the items this viewer may see in the view that pass the filter
/// and carry the value**, for a principal who sees everything and one who sees a third, in both
/// views, under a filter answered in entity space, one answered by rows, and the two combined.
/// The values offered and their order are the unfiltered page's.
#[test]
fn a_filtered_count_is_the_visible_items_passing_the_filter() {
    let fx = fixture();
    let engine = engine_for(&fx, "filtered-counts");
    let carrying = |column: &str, e: u64| match column {
        "department" => department_of(e),
        _ => archive_of(e),
    };
    let (xx, yy) = (code_of(&engine, "archive", "xx"), code_of(&engine, "archive", "yy"));
    for view in ["s0", "s1"] {
        type Passes<'a> = Box<dyn Fn(u64) -> bool + 'a>;
        let filters: Vec<(FilterExpr, Passes)> = vec![
            (
                category("archive", &[xx, yy]),
                Box::new(|e| matches!(archive_of(e), Some("xx" | "yy"))),
            ),
            (boxed(200.5, 700.5), Box::new(move |e| in_box(view, e, 200.5, 700.5))),
            (
                FilterExpr::AllOf(vec![category("archive", &[yy]), boxed(0.5, 600.5)]),
                Box::new(move |e| archive_of(e) == Some("yy") && in_box(view, e, 0.5, 600.5)),
            ),
        ];
        for credential in [full_coverage_credential(), subset_credential()] {
            let session = engine.authorise(&credential).expect("the credential resolves");
            let visible = visible_to(&credential);
            for column in ["department", "archive"] {
                let unfiltered =
                    suggest_with(&engine, &session, column, Some(view), "", 20, true, 100_000, 0);
                for (i, (filter, passes)) in filters.iter().enumerate() {
                    let got = filtered(&engine, &session, column, view, filter);
                    assert_eq!(keys(&got), keys(&unfiltered), "{view} {column} filter {i}");
                    for value in &got.values {
                        let expected = visible
                            .iter()
                            .filter(|&&e| {
                                in_view(view, e)
                                    && passes(e)
                                    && carrying(column, e) == Some(value.key.as_str())
                            })
                            .count() as u64;
                        assert_eq!(
                            value.count,
                            Some(expected),
                            "{view} {column} filter {i}: {}",
                            value.key
                        );
                    }
                }
            }
        }
    }
}

/// **A value the filter excludes is still offered, with count 0**, under a filter on the counted
/// column itself too: the filter narrows the counts and never the values.
#[test]
fn a_value_the_filter_excludes_is_offered_with_zero() {
    let fx = fixture();
    let engine = engine_for(&fx, "filtered-zero");
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let visible = visible_to(&full_coverage_credential());
    let eng = code_of(&engine, "department", "eng");
    let xx = code_of(&engine, "archive", "xx");
    let carrying_eng = visible
        .iter()
        .filter(|&&e| department_of(e) == Some("eng"))
        .count() as u64;
    for filter in [category("archive", &[xx]), category("department", &[eng])] {
        let got = filtered(&engine, &session, "department", "s0", &filter);
        let mut counts: Vec<(String, Option<u64>)> =
            got.values.iter().map(|v| (v.key.clone(), v.count)).collect();
        counts.sort_unstable();
        assert_eq!(
            counts,
            [
                ("eng".to_string(), Some(carrying_eng)),
                ("legal".to_string(), Some(0)),
                ("sales".to_string(), Some(0)),
            ]
        );
    }
    // Without counts the filter is admitted and changes nothing.
    let archive = category("archive", &[xx]);
    let got = engine
        .suggest(
            &session,
            SuggestRequest {
                column: "department",
                view: Some("s0"),
                filter: Some(&archive),
                q: "",
                limit: 20,
                counts: false,
                walk_budget: 100_000,
                max_suggest_set_entities: 0,
                cancel: None,
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(keys(&got), keys(&page(&engine, &full_coverage_credential(), "department", "")));
    assert!(got.values.iter().all(|v| v.count.is_none()));
}

/// **A suppression applies to the next filtered count**, and so does lifting it.
#[test]
fn a_suppression_between_two_requests_moves_the_filtered_count() {
    let fx = fixture();
    let mut engine = engine_for(&fx, "filtered-suppress");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let filter = boxed(200.5, 700.5);
    let count_of_eng = || {
        filtered(&engine, &session, "department", "s0", &filter)
            .values
            .iter()
            .find(|v| v.key == "eng")
            .and_then(|v| v.count)
            .expect("eng is offered")
    };
    let inside: Vec<u64> = (0..N)
        .filter(|&e| department_of(e) == Some("eng") && in_box("s0", e, 200.5, 700.5))
        .collect();
    assert!(inside.len() > 1, "the fixture puts several eng items in the box");
    let before = count_of_eng();
    assert_eq!(before, inside.len() as u64);

    let entity =
        tessera_types::EntityId::new(source_to_new_map(&fx.bundle, &fx.prefix)[&inside[0]]);
    engine
        .accept_change(entity, tessera_lifecycle::wal::ChangeOp::Suppress)
        .expect("a suppression is an ordinary change");
    assert_eq!(count_of_eng(), before - 1);
    engine
        .accept_change(entity, tessera_lifecycle::wal::ChangeOp::Unsuppress)
        .expect("an unsuppress is an ordinary change");
    assert_eq!(count_of_eng(), before);
}

/// **A deletion applies to the next filtered count.**
#[test]
fn a_deletion_between_two_requests_moves_the_filtered_count() {
    let fx = fixture();
    let mut engine = engine_for(&fx, "filtered-delete");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let filter = boxed(200.5, 700.5);
    let count_of_eng = || {
        filtered(&engine, &session, "department", "s0", &filter)
            .values
            .iter()
            .find(|v| v.key == "eng")
            .and_then(|v| v.count)
            .expect("eng is offered")
    };
    let inside: Vec<u64> = (0..N)
        .filter(|&e| department_of(e) == Some("eng") && in_box("s0", e, 200.5, 700.5))
        .collect();
    let before = count_of_eng();
    assert_eq!(before, inside.len() as u64);
    let entity =
        tessera_types::EntityId::new(source_to_new_map(&fx.bundle, &fx.prefix)[&inside[0]]);
    engine
        .accept_change(entity, tessera_lifecycle::wal::ChangeOp::Delete)
        .expect("a deletion is an ordinary change");
    assert_eq!(count_of_eng(), before - 1);
}

/// **A negated filter counts the visible items carrying a value in its column that match none of
/// it**, and an item with no value in that column is not among them.
#[test]
fn a_negated_filter_counts_what_it_leaves() {
    let fx = fixture();
    let engine = engine_for(&fx, "filtered-negated");
    let xx = code_of(&engine, "archive", "xx");
    let filter = FilterExpr::NoneOf(vec![category("archive", &[xx])]);
    for credential in [full_coverage_credential(), subset_credential()] {
        let session = engine.authorise(&credential).unwrap();
        let visible = visible_to(&credential);
        let got = filtered(&engine, &session, "department", "s0", &filter);
        for value in &got.values {
            let expected = visible
                .iter()
                .filter(|&&e| {
                    matches!(archive_of(e), Some(a) if a != "xx")
                        && department_of(e) == Some(value.key.as_str())
                })
                .count() as u64;
            assert_eq!(value.count, Some(expected), "{}", value.key);
        }
    }
}

/// **A cancelled request stops before it evaluates the filter**, and ends as cancelled.
#[test]
fn a_cancelled_filtered_count_ends_as_cancelled() {
    let fx = fixture();
    let engine = engine_for(&fx, "filtered-cancel");
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let cancel = tessera_engine::CancelToken::new();
    cancel.cancel();
    let filter = boxed(200.5, 700.5);
    let got = engine.suggest(
        &session,
        SuggestRequest {
            column: "department",
            view: Some("s0"),
            filter: Some(&filter),
            q: "",
            limit: 20,
            counts: true,
            walk_budget: 100_000,
            max_suggest_set_entities: 0,
            cancel: Some(cancel),
        },
    );
    assert!(matches!(got, Err(EngineError::Cancelled)), "{got:?}");
}

/// **A filter needs a view, and one naming no filterable column is refused**, as the viewport
/// refuses it.
#[test]
fn a_filter_without_a_view_or_over_an_unknown_column_is_refused() {
    let fx = fixture();
    let engine = engine_for(&fx, "filtered-refusals");
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let archive = category("archive", &[code_of(&engine, "archive", "xx")]);
    assert!(matches!(
        engine.suggest(
            &session,
            SuggestRequest {
                column: "department",
                view: None,
                filter: Some(&archive),
                q: "",
                limit: 20,
                counts: true,
                walk_budget: 100_000,
                max_suggest_set_entities: 0,
                cancel: None,
            },
        ),
        Err(EngineError::FilterMalformed(_))
    ));
    let unknown = category("nonesuch", &[1]);
    assert!(matches!(
        engine.suggest(
            &session,
            SuggestRequest {
                column: "department",
                view: Some("s0"),
                filter: Some(&unknown),
                q: "",
                limit: 20,
                counts: true,
                walk_budget: 100_000,
                max_suggest_set_entities: 0,
                cancel: None,
            },
        ),
        Err(EngineError::FilterMalformed(_) | EngineError::FilterRefused(_))
    ));
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
    build(&build_args(&points, &pairs, &bundle, schema))
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
        .ingest_rows(
            vec![UnallocatedRow {
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

/// **Unlinking a mapped index does not disturb a live reader**, which is the property the rebuild's
/// own unlink depends on — it deletes the superseded directory *after* the swap, and a request
/// still holding the old generation must keep answering out of its pages.
#[test]
fn a_superseded_index_keeps_answering_after_its_files_are_unlinked() {
    let fx = fixture();
    let engine = engine_for(&fx, "unlink");
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let generation = engine.generation();
    let index = generation
        .suggest
        .get("department")
        .expect("the fixture builds one");
    let before = engine
        .suggest(
            &session,
            SuggestRequest {
                column: "department",
                view: None,
                filter: None,
                q: "eng",
                limit: 20,
                counts: false,
                walk_budget: 100_000,
                max_suggest_set_entities: 0,
                cancel: None,
            },
        )
        .unwrap()
        .unwrap();
    std::fs::remove_dir_all(index.base().dir()).expect("the directory is the engine's own");
    let after = engine
        .suggest(
            &session,
            SuggestRequest {
                column: "department",
                view: None,
                filter: None,
                q: "eng",
                limit: 20,
                counts: false,
                walk_budget: 100_000,
                max_suggest_set_entities: 0,
                cancel: None,
            },
        )
        .expect("a mapped index outlives its directory entry")
        .unwrap();
    assert_eq!(keys(&before), keys(&after));
    assert_eq!(keys(&after), ["eng"]);
    drop(generation);
}

/// **A column whose vocabulary has no suggestion index refuses, rather than answering empty** — the
/// same reasoning the enumeration refuses an underivable predicate on: an empty page is a real
/// answer, and serving it here makes a broken surface indistinguishable from a working one.
///
/// Reached by taking the index out from under a live engine, which is the fault state the refusal
/// is for — nothing request-shaped produces it, `Engine::open` building an index for every
/// vocabulary a declared category column names.
#[test]
fn a_column_with_no_index_refuses_rather_than_answering_empty() {
    let fx = fixture();
    let mut engine = engine_for(&fx, "refuse");
    // The hook publishes through the executor, which is the sole publisher (lifecycle §1.3) — so
    // there has to be one to publish through.
    engine.start_write_executor(8).expect("the executor starts");
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    assert!(
        engine.forget_suggestion_index_for_test("department"),
        "the hook must have published, or the refusal below is asserting nothing"
    );

    let refused = engine.suggest(
        &session,
        SuggestRequest {
            column: "department",
            view: None,
            filter: None,
            q: "eng",
            limit: 20,
            counts: false,
            walk_budget: 100_000,
            max_suggest_set_entities: 0,
            cancel: None,
        },
    );
    match refused {
        Err(EngineError::SuggestionUnavailable { column, detail }) => {
            assert_eq!(column, "department");
            assert!(
                detail.contains("department"),
                "the refusal must name the vocabulary it could not reach: {detail}"
            );
        }
        other => panic!("expected SuggestionUnavailable, got {other:?}"),
    }
    // And it is this surface alone that is refused: the enumeration over the same column, under the
    // same gate, is unaffected.
    let listed = engine
        .categories(
            &session,
            "department",
            tessera_engine::CategoryQuery::Page {
                after: None,
                limit: 20,
            },
        )
        .expect("the enumeration does not depend on the suggestion index")
        .expect("the column is a category");
    assert!(!listed.values.is_empty());
}

// ---------------------------------------------------------------------------------------------
// Retirement under suppression
// ---------------------------------------------------------------------------------------------

/// An **open, `derived`** vocabulary: keys are minted at ingest, and a value is offered only to a
/// principal who can see an item carrying it. The pair the retirement case needs.
const OPEN_DERIVED: &str = r#"
[[vocabulary]]
name       = "team"
width      = "u16"
value_set  = "open"
visibility = "derived"

[[attribute]]
name       = "team"
type       = "category"
render     = true
index      = true
vocabulary = "team"
"#;

/// **A value whose last visible member is suppressed stops being offered, on the next request** —
/// membership-derivation self-retiring, which is §3.3's whole argument for deriving per request
/// rather than maintaining a set.
///
/// Asserted through the **base index**, over the built vocabulary: `eng` has sixteen members and
/// all sixteen are suppressed one at a time, so the value is offered until the last one goes and
/// not after. That it is offered *up to* the last suppression is half the test — a retirement rule
/// that fired early would withhold a value the viewer can still see, and would pass an
/// assert-at-the-end.
///
/// **Mutations this kills:** caching the membership across requests; deriving visibility from the
/// session's stale fragment rather than the composed verdict; maintaining a visible-value set.
#[test]
fn suppressing_a_values_last_visible_member_retires_it_from_the_base_index() {
    let fx = fixture();
    let mut engine = engine_for(&fx, "suppress-base");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);

    let full = full_coverage_credential();
    let entity_of = source_to_new_map(&fx.bundle, &fx.prefix);
    let members: Vec<u64> = (0..N).filter(|&e| department_of(e) == Some("eng")).collect();
    assert!(
        members.len() > 1,
        "the value must have several members, or 'the last one' is the only one"
    );

    for (suppressed, source) in members.iter().enumerate() {
        assert!(
            keys(&page(&engine, &full, "department", "eng")).contains(&"eng".to_string()),
            "'eng' stopped being offered after {suppressed} of {} members were suppressed, with \
             visible members left",
            members.len()
        );
        engine
            .accept_change(
                tessera_types::EntityId::new(entity_of[source]),
                tessera_lifecycle::wal::ChangeOp::Suppress,
            )
            .expect("a suppression is an ordinary change");
    }

    assert!(
        !keys(&page(&engine, &full, "department", "eng")).contains(&"eng".to_string()),
        "'eng' is still offered with every member suppressed"
    );
    // The other values are untouched — a retirement that took the vocabulary with it would pass
    // the assertion above.
    let mut left = keys(&page(&engine, &full, "department", ""));
    left.sort_unstable();
    assert_eq!(left, ["legal", "sales"]);

    // And it comes back: a suppression is the reversible one of the two removal rules, and
    // derivation retires and un-retires with it because it is derived per request.
    engine
        .accept_change(
            tessera_types::EntityId::new(entity_of[&members[0]]),
            tessera_lifecycle::wal::ChangeOp::Unsuppress,
        )
        .expect("an unsuppress is an ordinary change");
    assert!(keys(&page(&engine, &full, "department", "eng")).contains(&"eng".to_string()));
}

/// The same rule through the **side map**: a value minted at ingest, made visible by the flush that
/// gives its entity an extent, and retired when that entity is suppressed.
///
/// The base index is built at open and never rebuilt here, so this value exists only in the side
/// map — which is the arm that would be missed by a gate applied to the base's payloads and not to
/// the merged walk. It is also the case that shows a *minted derived* value is not offered before
/// its flush: a buffered entity has no extent and no posting, so it contributes no membership at
/// all (`filter-index.md` §5), and the value is in the index while being invisible.
#[test]
fn suppressing_a_minted_values_only_member_retires_it_from_the_side_map() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points_with_absent_column(&points, "team");
    write_pairs_n(&pairs, N);
    let schema = parse_schema(dir.path(), OPEN_DERIVED);
    let bundle = dir.path().join("bundle");
    build(&build_args(&points, &pairs, &bundle, schema)).expect("the fixture builds");

    let mut engine = open_engine(
        &bundle,
        &dir.path().join("cache"),
        &dir.path().join("wal.log"),
    );
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    let full = full_coverage_credential();

    let entity = engine
        .ingest_rows(
            vec![UnallocatedRow {
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
        .expect("the ingest is accepted")[0];

    // In the side map, and invisible: the entity is buffered, so it has neither a posting nor an
    // extent and derives no membership. The value is in the index and behind the gate.
    assert!(
        keys(&page(&engine, &full, "team", "platform")).is_empty(),
        "a buffered entity must derive no membership"
    );
    assert!(
        engine
            .generation()
            .suggest
            .get("team")
            .expect("the vocabulary has an index")
            .side_len()
            > 0,
        "…and the value must nonetheless be in the side map, or this proves nothing"
    );

    publish_buffered(&engine);
    assert_eq!(
        keys(&page(&engine, &full, "team", "platform")),
        ["Platform Infrastructure"],
        "the flush gives the entity an extent, and the value becomes derivable"
    );

    engine
        .accept_change(entity, tessera_lifecycle::wal::ChangeOp::Suppress)
        .expect("a suppression is an ordinary change");
    assert!(
        keys(&page(&engine, &full, "team", "platform")).is_empty(),
        "the value's only member is suppressed and it is still offered — derivation did not retire"
    );
    // Through its word start too, which is the entry a gate applied only to whole-key payloads
    // would leave reachable.
    assert!(keys(&page(&engine, &full, "team", "infra")).is_empty());
}

// ---------------------------------------------------------------------------------------------
// The second route: the per-session visible-value set (§6.3, decision 0124)
// ---------------------------------------------------------------------------------------------

/// The ceiling every case below runs at — far above this fixture's sixty entities, so the route is
/// decided by the rules under test and never by the number.
const WIDE_CEILING: u64 = 10_000_000;

/// **Hold a session still until its sweep lands**, then answer from the set.
///
/// The first suggest on a `(session, column)` pair dispatches the sweep and is answered by the
/// probe route, so a case that wants the set route has to ask twice and wait between — which is the
/// on-demand rule itself, asserted by every case that uses this rather than by one of its own.
/// Returns the number of admitted sweeps, so a caller can tell "the set answered" from "the probe
/// route answered a second time".
fn wait_for_set(engine: &Engine, session: &tessera_engine::Session, column: &str) -> u64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        // A **hit** is the signal, not a finished sweep: the counters are process-wide, so a build
        // count that moved may be another session's, and a set already held for this one moves it
        // not at all. Each request re-dispatches if nothing is in flight and nothing is held, so an
        // abandoned sweep is retried rather than waited on for ever.
        let before = engine.suggest_set_stats().hits;
        suggest_with(engine, session, column, None, "", 20, false, 100_000, WIDE_CEILING);
        let stats = engine.suggest_set_stats();
        if stats.hits > before {
            return stats.hits;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no sweep landed for '{column}': {stats:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// **The two routes answer the same page** (§6.3, decision 0124) — the same values, in the same
/// order, with the same spans and the same counts, for the same prefix before and after the set
/// lands. Which route answered is not on the wire, and this is what makes that true rather than
/// merely intended.
///
/// **Mutations this kills:** a set built over codes rather than dense positions (every value would
/// miss); a set built from the extents alone (the base build's values would vanish); a set that
/// admitted a value the probe route rejects.
#[test]
fn the_two_routes_answer_the_same_page() {
    let fx = fixture();
    let engine = engine_for(&fx, "routes");
    for credential in [
        full_coverage_credential(),
        subset_credential(),
        zero_credential(),
    ] {
        let session = engine.authorise(&credential).expect("it resolves");
        let mut before = Vec::new();
        for q in ["", "e", "s", "l", "legal", "counsel", "zzz"] {
            before.push(suggest_with(
                &engine, &session, "department", None, q, 20, true, 100_000, 0,
            ));
        }
        wait_for_set(&engine, &session, "department");
        for (q, expected) in ["", "e", "s", "l", "legal", "counsel", "zzz"]
            .iter()
            .zip(&before)
        {
            let got = suggest_with(
                &engine,
                &session,
                "department",
                None,
                q,
                20,
                true,
                100_000,
                WIDE_CEILING,
            );
            assert_eq!(keys(&got), keys(expected), "prefix {q:?}");
            let spans: Vec<_> = got.values.iter().map(|v| v.span).collect();
            let was: Vec<_> = expected.values.iter().map(|v| v.span).collect();
            assert_eq!(spans, was, "prefix {q:?}");
            let counts: Vec<_> = got.values.iter().map(|v| v.count).collect();
            let was: Vec<_> = expected.values.iter().map(|v| v.count).collect();
            assert_eq!(counts, was, "prefix {q:?}");
        }
    }
}

/// **`more` is exact on the set route.** The probe route spends a budget on values it cannot see
/// and reports `more` for it; the set route reads no posting, spends no budget, and says `more`
/// when the page filled and at no other time — which is the one field the two routes may differ on
/// (contracts §3.2, C31's disposition).
#[test]
fn more_goes_from_a_spent_budget_to_exact_once_the_set_lands() {
    let fx = fixture();
    let engine = engine_for(&fx, "more-set");
    let session = engine
        .authorise(&zero_credential())
        .expect("the credential resolves");

    // A principal who sees nothing, at a budget of one: the probe route examines one value and
    // stops.
    let probed = suggest_with(&engine, &session, "department", None, "", 20, false, 1, 0);
    assert!(probed.values.is_empty());
    assert!(probed.more, "the budget was spent");

    wait_for_set(&engine, &session, "department");
    let from_set = suggest_with(&engine, &session, "department", None, "", 20, false, 1, WIDE_CEILING);
    assert!(from_set.values.is_empty());
    assert!(
        !from_set.more,
        "the set route walks the range whole and answers exactly"
    );
}

/// **One sweep in flight per key** (§6.3 rule 2). Eight keystrokes on one pair start one sweep, not
/// eight — asserted through the admitted-build counter, which moves once per sweep that finishes.
#[test]
fn a_burst_of_keystrokes_starts_one_sweep() {
    let fx = fixture();
    let engine = engine_for(&fx, "in-flight");
    let session = engine
        .authorise(&full_coverage_credential())
        .expect("it resolves");
    for q in ["e", "en", "eng", "s", "sa", "sal", "l", "le"] {
        suggest_with(&engine, &session, "department", None, q, 20, false, 100_000, WIDE_CEILING);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while engine.suggest_set_stats().in_flight > 0 {
        assert!(std::time::Instant::now() < deadline, "the sweep never ended");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let stats = engine.suggest_set_stats();
    assert_eq!(stats.builds, 1, "one sweep for one key: {stats:?}");
    assert_eq!(stats.entries, 1, "{stats:?}");
}

/// **A set that no longer matches the live overlay is discarded rather than served** (§6.3 rule 1)
/// — the fail-closed half of the whole route.
///
/// A value's last visible member is suppressed *between* the sweep and the read. The set was taken
/// when that value still had one, so serving it would offer the name of a value whose members this
/// viewer can no longer see: exactly the C11 disclosure `derived` withholds. The overlay's counter
/// moves at the acknowledgement's publication, so the key names an entry nothing will ask for and
/// the request falls back to the probe route, which derives the answer afresh.
///
/// **Mutations this kills:** dropping `overlay_version` from the key; serving a nearest-match entry;
/// keeping the set across a deny "because the vocabulary did not change".
#[test]
fn a_suppression_between_the_sweep_and_the_read_retires_the_value() {
    let fx = fixture();
    let mut engine = engine_for(&fx, "set-suppress");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);

    let full = full_coverage_credential();
    let session = engine.authorise(&full).expect("it resolves");
    let entity_of = source_to_new_map(&fx.bundle, &fx.prefix);
    let members: Vec<u64> = (0..N).filter(|&e| department_of(e) == Some("eng")).collect();

    wait_for_set(&engine, &session, "department");
    assert!(
        keys(&suggest_with(
            &engine,
            &session,
            "department",
            None,
            "eng",
            20,
            false,
            100_000,
            WIDE_CEILING
        ))
        .contains(&"eng".to_string()),
        "the set was built while 'eng' still had visible members"
    );

    for source in &members {
        engine
            .accept_change(
                tessera_types::EntityId::new(entity_of[source]),
                tessera_lifecycle::wal::ChangeOp::Suppress,
            )
            .expect("a suppression is an ordinary change");
    }

    // No wait, and deliberately: the response to a deny is at accept, so the very next request must
    // already withhold the value — a set taken before it may not answer this one.
    let got = suggest_with(
        &engine,
        &session,
        "department",
        None,
        "eng",
        20,
        false,
        100_000,
        WIDE_CEILING,
    );
    assert!(
        !keys(&got).contains(&"eng".to_string()),
        "a set taken before the suppression was served: {:?}",
        keys(&got)
    );
}

/// **Under the ceiling, and nowhere else** (§6.3 rule 3, decision 0124). At
/// `max_suggest_set_entities = 0` no viewer is inside it, so no sweep is ever dispatched and every
/// request takes the probe route — and answers the same page it would have from a set.
#[test]
fn a_viewer_wider_than_the_ceiling_never_gets_a_set() {
    let fx = fixture();
    let engine = engine_for(&fx, "ceiling");
    let session = engine
        .authorise(&full_coverage_credential())
        .expect("it resolves");
    for _ in 0..8 {
        suggest_with(&engine, &session, "department", None, "", 20, false, 100_000, 0);
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    let stats = engine.suggest_set_stats();
    assert_eq!(stats.builds, 0, "{stats:?}");
    assert_eq!(stats.in_flight, 0, "{stats:?}");
    assert!(stats.declined >= 8, "{stats:?}");
}

/// **A `public` column never gets a set**, having no predicate for one to answer: its value names
/// are an authored assertion served to anybody with a session (§3.8), so a per-session set would be
/// a per-session copy of a constant.
#[test]
fn a_public_column_never_gets_a_set() {
    let fx = fixture();
    let engine = engine_for(&fx, "public-set");
    let session = engine
        .authorise(&full_coverage_credential())
        .expect("it resolves");
    for _ in 0..4 {
        suggest_with(&engine, &session, "archive", None, "", 20, false, 100_000, WIDE_CEILING);
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    let stats = engine.suggest_set_stats();
    assert_eq!(stats.builds, 0, "{stats:?}");
    assert_eq!(stats.in_flight, 0, "{stats:?}");
}

/// **A rebuild between the sweep and the read discards the set, and the same request starts its
/// replacement** (§6.3 rule 1's other half).
///
/// A rebuild appends what the side map minted and re-sorts, so it moves the dense positions of
/// every value after each insertion — and it is the one publication that deliberately moves neither
/// `segments_version` nor `overlay_version`, so nothing in the key sees it. The value count is what
/// does. Reading the old set against the new index would name *other values'* positions, which is
/// value names offered to a viewer with no member of them.
///
/// The rejected entry is dropped where it is rejected, not left to expire: left in place, the claim
/// that starts a sweep would find a set held for the key and refuse, so the pair would stay on the
/// probe route until the key rotated or eviction reached it — which for a session typing steadily
/// into one column is neither.
///
/// **Mutations this kills:** reading a set against an index it was not swept over; guarding on the
/// key alone; rejecting the entry without removing it.
#[test]
fn a_rebuild_between_the_sweep_and_the_read_discards_the_set_and_resweeps() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    // An **open, `derived`** vocabulary carried by no row at build: every value it acquires is
    // minted through ingest, so a rebuild between two ingests genuinely lengthens the index —
    // which is the only way a value's dense position moves.
    write_points_with_absent_column(&points, "team");
    write_pairs_n(&pairs, N);
    let schema = parse_schema(dir.path(), OPEN_DERIVED);
    let bundle = dir.path().join("bundle");
    build(&build_args(&points, &pairs, &bundle, schema)).expect("the fixture builds");

    let mut engine = open_engine(
        &bundle,
        &dir.path().join("cache"),
        &dir.path().join("wal.log"),
    );
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);

    let mint = |engine: &Engine, key: &str, batch: &str| {
        engine
            .ingest_rows(
                vec![UnallocatedRow {
                    view: "s0".to_string(),
                    join: None,
                    descriptors: vec![b"0".to_vec()],
                    x: 1.0,
                    y: 1.0,
                    scalars: vec![WalScalar::Utf8(key.to_string())],
                    terms: engine.resolve_terms(&[b"0".to_vec()]),
                    scoped: Vec::new(),
                }],
                batch.to_string(),
                [0u8; 32],
            )
            .expect("the ingest is accepted");
    };

    // Two values, flushed and then rebuilt, so the base index holds them and the side map is empty
    // — the state a warm set is swept in. The flush is what gives a `derived` value a member the
    // gate can see at all: a buffered entity has no row and no extent, so it carries no membership
    // until its flush (`filter-index.md` §5, in its vocabulary form).
    mint(&engine, "alpha", "batch-a");
    mint(&engine, "zulu", "batch-z");
    publish_buffered(&engine);
    assert!(
        engine.rebuild_suggestion_index_for_test("team"),
        "the executor must publish the rebuild"
    );

    // A third value, minted into the **side map** and left there: it sorts first, so folding it
    // into the base moves every existing value's dense position rather than appending harmlessly
    // past them. It is minted *before* the set is swept so that nothing between the sweep and the
    // read is an ingest — an ingest closes a commit window and publishes, which rotates the key and
    // would leave the case asserting about the key instead of about the guard.
    mint(&engine, "aaa-first", "batch-first");

    let full = full_coverage_credential();
    let session = engine.authorise(&full).expect("the credential resolves");
    wait_for_set(&engine, &session, "team");
    let before = engine.suggest_set_stats();
    assert!(before.entries >= 1, "{before:?}");

    // **The rebuild alone**, between the sweep and the read: it folds the side map into the base,
    // lengthening the index and moving positions, and it deliberately moves neither
    // `segments_version` nor `overlay_version` — so the only thing that has changed is the one
    // thing the key cannot carry.
    assert!(engine.rebuild_suggestion_index_for_test("team"));

    // `aaa-first` is buffered, so it has no visible member yet and is not offered — the page is
    // unchanged, which is what makes a page read through the *stale* set (whose positions now name
    // other values) a visible failure rather than a coincidence.
    let got = suggest_with(&engine, &session, "team", None, "", 20, false, 100_000, WIDE_CEILING);
    let mut served = keys(&got);
    served.sort_unstable();
    assert_eq!(served, ["alpha", "zulu"]);

    let after = engine.suggest_set_stats();
    assert_eq!(
        after.discarded,
        before.discarded + 1,
        "the set must be rejected by the value-count guard, not by the key: {after:?}"
    );

    // And the pair is not stranded on the probe route: the rejecting request started the
    // replacement sweep, which lands and then answers.
    wait_for_set(&engine, &session, "team");
    let warm = engine.suggest_set_stats();
    assert!(warm.builds > before.builds, "no re-sweep landed: {warm:?}");
    let mut served = keys(&suggest_with(
        &engine,
        &session,
        "team",
        None,
        "",
        20,
        false,
        100_000,
        WIDE_CEILING,
    ));
    served.sort_unstable();
    assert_eq!(served, ["alpha", "zulu"]);
}

/// **A prune drops the pruned session's visible-value set and nobody else's** — the set is held per
/// `(session, column)` and is pinned by nothing once the session is revoked.
///
/// Both pruners, because the expiry sweep takes the batch form and a set left behind by one of them
/// is memory no request can ever reach again.
#[test]
fn a_prune_drops_the_tokens_visible_value_set() {
    let fx = fixture();
    let engine = engine_for(&fx, "prune");

    let doomed = engine
        .authorise(&full_coverage_credential())
        .expect("the credential resolves");
    let survivor = engine
        .authorise(&subset_credential())
        .expect("the credential resolves");
    wait_for_set(&engine, &doomed, "department");
    wait_for_set(&engine, &survivor, "department");
    assert_eq!(
        engine.suggest_set_stats().entries,
        2,
        "one set each, or this proves nothing"
    );

    engine.prune_token(doomed.token_id());
    assert_eq!(engine.suggest_set_stats().entries, 1);

    // The survivor's own set is still there: a hit rather than a re-sweep.
    let hits = engine.suggest_set_stats().hits;
    let page = suggest_with(
        &engine,
        &survivor,
        "department",
        None,
        "",
        20,
        false,
        100_000,
        WIDE_CEILING,
    );
    assert!(!keys(&page).is_empty(), "and it is still served");
    assert_eq!(engine.suggest_set_stats().hits, hits + 1);

    engine.prune_tokens(&FxHashSet::from_iter([survivor.token_id()]));
    assert_eq!(
        engine.suggest_set_stats().entries,
        0,
        "the batch form removes the same thing"
    );
}
