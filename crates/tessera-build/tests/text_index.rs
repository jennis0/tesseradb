//! The base build's text index: a token dictionary, postings over it, **and** a blob row
//! (`records-and-search.md` §4.4).
//!
//! **The two-homes assertion is the point of this file.** Every other family puts a value in one
//! place; an indexed `text` column puts its terms in entity space and its prose in the record
//! blob, because postings answer `match` and reconstruct nothing — no drill-down can rebuild a
//! sentence from the set of words it contained. A build that wrote only the index would serve
//! `match` correctly and return an empty field, which is the failure this test exists to catch.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{BinaryArray, Float64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::schema::{Attribute, Schema};
use tessera_build::{build, BuildArgs};
use tessera_filter::{Access, RecordBlob, RecordValue, SortedDict};
use tessera_spatial::tiler::ScalarType;
use tessera_spatial::Bounds;
use tessera_store::read::open_bundle;
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const N: u64 = 64;

/// Source `e`'s prose. Mixed script on purpose: the analyser is one pipeline with nothing
/// declared, and a build that quietly split on spaces would index the CJK half as one term.
fn prose_of(e: u64) -> String {
    match e % 4 {
        0 => "the quick brown fox".to_string(),
        1 => "quick silver fox".to_string(),
        2 => "日本語のテキスト quick".to_string(),
        _ => "brown bear".to_string(),
    }
}

fn write_points(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("abstract", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter().map(|e| (e % 100) as f64).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter().map(|e| ((e * 7) % 100) as f64).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|&e| Some(prose_of(e))).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_empty_pairs(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(Vec::<u64>::new())),
            Arc::new(UInt64Array::from(Vec::<u64>::new())),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn text_schema(index: bool) -> Schema {
    Schema {
        attributes: vec![Attribute {
            name: "abstract".to_string(),
            ty: ScalarType::Text,
            analyser: Some("unicode/icu4x-2.2/p1".to_string()),
            vocabulary: None,
            vocabulary_kind: None,
            index,
            render: false,
        }],
        vocabularies: HashMap::new(),
    }
}

fn build_with(schema: Schema) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points(&points);
    write_empty_pairs(&pairs);
    let out = dir.path().join("bundle");
    build(&BuildArgs {
        points,
        pairs,
        out,
        extent: Bounds {
            x_min: 0.0,
            x_max: 1000.0,
            y_min: 0.0,
            y_max: 1000.0,
        },
        slice_id: "s0".to_string(),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    })
    .expect("a build with a text column succeeds");
    dir
}

fn current_prefix(out: &Path) -> String {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("CURRENT")).unwrap()).unwrap();
    current["prefix"].as_str().unwrap().to_string()
}

fn partition_dir(out: &Path) -> PathBuf {
    let bundle = open_bundle(out).unwrap();
    let phash = bundle.partitions.keys().next().unwrap().clone();
    out.join(current_prefix(out)).join("partitions").join(phash)
}

fn source_to_entity(out: &Path) -> HashMap<u64, u32> {
    let bundle = open_bundle(out).unwrap();
    let part = bundle.partitions.values().next().unwrap();
    let prefix = current_prefix(out);
    let mut map = HashMap::new();
    for rel in &part.manifest.external_id_runs {
        let path = out.join(&prefix).join(rel);
        let reader =
            arrow::ipc::reader::FileReader::try_new(File::open(&path).unwrap(), None).unwrap();
        for batch in reader {
            let batch = batch.unwrap();
            let ext = batch
                .column(0)
                .as_any()
                .downcast_ref::<BinaryArray>()
                .unwrap();
            let ent = batch
                .column(1)
                .as_any()
                .downcast_ref::<UInt32Array>()
                .unwrap();
            for i in 0..batch.num_rows() {
                map.insert(
                    u64::from_le_bytes(ext.value(i).try_into().unwrap()),
                    ent.value(i),
                );
            }
        }
    }
    map
}

/// **The index and the blob, both, from one build.**
#[test]
fn an_indexed_text_column_writes_a_token_index_and_a_blob_row() {
    let dir = build_with(text_schema(true));
    let out = dir.path().join("bundle");
    let column_dir = partition_dir(&out).join("attrs").join("abstract");

    // --- entity space: the dictionary holds the analyser's terms, deduplicated and sorted.
    let dict = SortedDict::open_dir(&column_dir, Access::Read).expect("the token dictionary opens");
    dict.self_check().expect("it decodes end to end");
    let mut terms = Vec::new();
    dict.walk(|_, key| terms.push(key.to_string())).unwrap();
    let mut expected: Vec<String> = (0..N)
        .flat_map(|e| tessera_analyse::Analyser::new().tokens(&prose_of(e)))
        .collect();
    expected.sort();
    expected.dedup();
    assert_eq!(terms, expected, "the dictionary is the distinct term set");
    assert!(
        terms.iter().any(|t| t == "日本語"),
        "a CJK run was lost: {terms:?}"
    );

    // --- entity space: a term's posting names exactly the entities whose prose carries it.
    let postings =
        tessera_authz::postings::PostingsReader::open(&column_dir.join("postings.arrow"), false)
            .expect("the postings open");
    assert_eq!(postings.term_count(), terms.len() as u32);
    let source_of = source_to_entity(&out);
    for probe in ["quick", "fox", "日本語", "bear"] {
        let ordinal = terms.iter().position(|t| t == probe).expect("a known term");
        let posting = postings
            .posting_at(ordinal as u32)
            .expect("a readable posting")
            .expect("the term has one");
        let mut got: Vec<u32> = match posting {
            tessera_authz::postings::PostingRef::Array(bytes) => bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .collect(),
            tessera_authz::postings::PostingRef::Roaring(view) => view.iter().collect(),
        };
        got.sort_unstable();
        let mut want: Vec<u32> = (0..N)
            .filter(|&e| {
                tessera_analyse::Analyser::new()
                    .tokens(&prose_of(e))
                    .iter()
                    .any(|t| t == probe)
            })
            .map(|e| source_of[&e])
            .collect();
        want.sort_unstable();
        assert_eq!(got, want, "posting for {probe:?}");
    }

    // --- and the third home: the prose itself, which no posting could reconstruct.
    let blob = RecordBlob::open_dir(
        &partition_dir(&out).join("attrs").join("record"),
        Access::Read,
    )
    .expect("an indexed text column still has a blob row");
    for source in [0u64, 1, 2, 3, 63] {
        let entity = source_of[&source];
        let fields = blob
            .fields_of(entity)
            .expect("a clean read")
            .unwrap_or_else(|| panic!("source {source} has no blob row"));
        assert_eq!(
            fields,
            vec![tessera_filter::RecordField {
                tag: 0,
                value: RecordValue::Utf8(prose_of(source)),
            }],
            "source {source}'s prose"
        );
    }
}

/// An **unindexed** text column has the blob row and no index at all — `index = true` adds the
/// token index, it does not move the value.
#[test]
fn an_unindexed_text_column_has_the_blob_row_and_no_index() {
    let dir = build_with(text_schema(false));
    let out = dir.path().join("bundle");
    let column_dir = partition_dir(&out).join("attrs").join("abstract");
    assert!(
        !column_dir.join(tessera_filter::DICT_FILE).exists(),
        "an unindexed text column wrote a dictionary"
    );
    assert!(
        !column_dir.join("postings.arrow").exists(),
        "an unindexed text column wrote postings"
    );

    let blob = RecordBlob::open_dir(
        &partition_dir(&out).join("attrs").join("record"),
        Access::Read,
    )
    .expect("the blob is where an unindexed text value lives");
    let source_of = source_to_entity(&out);
    assert_eq!(
        blob.fields_of(source_of[&5]).unwrap(),
        Some(vec![tessera_filter::RecordField {
            tag: 0,
            value: RecordValue::Utf8(prose_of(5)),
        }])
    );
}

/// **The manifest records which analyser indexed the column**, per column. An index built by one
/// analyser and queried by another matches on precisely the strings whose segmentation differs,
/// with no error anywhere — this field is the only thing that can catch it.
#[test]
fn the_manifest_records_the_analyser_identity_per_column() {
    let dir = build_with(text_schema(true));
    let out = dir.path().join("bundle");
    let bundle = open_bundle(&out).unwrap();
    let declared = &bundle.manifest.declared_scalars;
    let text = declared
        .iter()
        .find(|d| d.name == "abstract")
        .expect("the column is declared");
    assert_eq!(
        text.analyser.as_deref(),
        Some("unicode/icu4x-2.2/p1"),
        "a text column records the identity that indexed it"
    );
    assert_eq!(
        text.arrow_type,
        ScalarType::Text,
        "and the type it was declared as"
    );
}

// ---------------------------------------------------------------------------------------------
// The read route
// ---------------------------------------------------------------------------------------------

/// `match` is *every named token appears in the field*, and `minimum_should_match` relaxes it to
/// at least m of n — both answered from the postings, both intersected with the candidate.
#[test]
fn match_and_minimum_should_match_answer_from_the_index() {
    use tessera_engine::filter::FilterOperand;

    let dir = build_with(text_schema(true));
    let out = dir.path().join("bundle");
    let columns = open_columns(&out);
    let source_of = source_to_entity(&out);
    let all: croaring::Bitmap = (0..N).map(|e| source_of[&e]).collect();

    // A helper predicting the answer from the fixture's own values, which is the oracle's relation:
    // what the corpus was *given*, upstream of what the build stored.
    let expected = |predicate: &dyn Fn(&[String]) -> bool| -> Vec<u32> {
        let analyser = tessera_analyse::Analyser::new();
        let mut out: Vec<u32> = (0..N)
            .filter(|&e| predicate(&analyser.tokens(&prose_of(e))))
            .map(|e| source_of[&e])
            .collect();
        out.sort_unstable();
        out
    };
    let got = |operand: FilterOperand| -> Vec<u32> {
        let mut v: Vec<u32> = columns
            .resolve("abstract", &operand, &all)
            .expect("a declared text column resolves")
            .iter()
            .collect();
        v.sort_unstable();
        v
    };
    let m = |query: &str, minimum: Option<u32>| FilterOperand::Match {
        query: query.to_string(),
        minimum,
    };

    // Every token must appear, and they may appear in any order.
    assert_eq!(
        got(m("quick fox", None)),
        expected(&|t| t.iter().any(|x| x == "quick") && t.iter().any(|x| x == "fox")),
        "match is a conjunction"
    );
    assert_eq!(got(m("fox quick", None)), got(m("quick fox", None)), "order is not a term");

    // A token no document carries makes the conjunction empty — and does not make it *everything*,
    // which is what a short circuit that skipped an unresolved token would produce.
    assert!(got(m("quick zzzznope", None)).is_empty());

    // m-of-n counts, and the unresolved token still consumes its place in the count.
    assert_eq!(
        got(m("quick brown", Some(1))),
        expected(&|t| t.iter().any(|x| x == "quick") || t.iter().any(|x| x == "brown")),
        "one of two is the union"
    );
    assert_eq!(
        got(m("quick brown zzzznope", Some(2))),
        expected(&|t| {
            [ "quick", "brown" ].iter().filter(|w| t.iter().any(|x| &x == w)).count() >= 2
        }),
        "an absent token keeps its place in the denominator"
    );

    // The query is analysed by the column's analyser, so a fullwidth or uppercase query finds the
    // same documents a plain one does — the property that a wire-side tokeniser would put at risk.
    assert_eq!(got(m("QUICK", None)), got(m("quick", None)));
    assert_eq!(got(m("日本語", None)), expected(&|t| t.iter().any(|x| x == "日本語")));
    assert!(!got(m("日本語", None)).is_empty(), "the CJK term is findable");
}

/// **The candidate bounds the answer.** Postings are corpus-wide; nothing derived from them may
/// name an entity outside `M_sel` (I2), and the intersection happens per token rather than once at
/// the end, where it could be forgotten.
#[test]
fn match_never_answers_outside_the_candidate() {
    use tessera_engine::filter::FilterOperand;

    let dir = build_with(text_schema(true));
    let out = dir.path().join("bundle");
    let columns = open_columns(&out);
    let source_of = source_to_entity(&out);

    let everything = FilterOperand::Match {
        query: "quick".to_string(),
        minimum: None,
    };
    let all: croaring::Bitmap = (0..N).map(|e| source_of[&e]).collect();
    let wide = columns.resolve("abstract", &everything, &all).unwrap();
    assert!(wide.cardinality() > 2, "the fixture must have something to narrow");

    // Two entities only, one of which carries the term.
    let narrow_mask: croaring::Bitmap = [source_of[&0], source_of[&3]].into_iter().collect();
    let narrow = columns.resolve("abstract", &everything, &narrow_mask).unwrap();
    assert!(
        narrow.andnot(&narrow_mask).is_empty(),
        "the answer named an entity the candidate did not"
    );
    assert_eq!(narrow.cardinality(), 1, "source 0 carries `quick`, source 3 does not");
}

fn open_columns(out: &Path) -> tessera_engine::filter::FilterColumns {
    let bundle = open_bundle(out).unwrap();
    let phash = bundle.partitions.keys().next().unwrap().clone();
    tessera_engine::filter::FilterColumns::open(
        &out.join(current_prefix(out)),
        &phash,
        &bundle.manifest.declared_scalars,
        &bundle.manifest.vocabularies,
        &[],
        &[],
        &[],
        false,
    )
    .expect("the text column opens")
}

/// **Asking for more words than the query has is unsatisfiable, not the conjunction.**
///
/// `minimum_should_match` counts *distinct* tokens, the query being deduplicated before it is
/// resolved, so "three of these two words" is a question no item can meet. The branch that answers
/// plain `match` was reached on `minimum >= tokens.len()` and therefore swallowed the case,
/// returning every item carrying both words — a wrong answer in the over-inclusive direction, on
/// the one operand this family has.
///
/// **Mutations this kills:** restoring `>=` in place of `==` in `text_match`'s intersection branch;
/// dropping the `minimum > tokens.len()` guard; counting raw rather than deduplicated tokens (the
/// repeated-word case then answers the conjunction).
#[test]
fn a_minimum_above_the_query_s_token_count_matches_nothing() {
    use tessera_engine::filter::FilterOperand;

    let dir = build_with(text_schema(true));
    let out = dir.path().join("bundle");
    let columns = open_columns(&out);
    let source_of = source_to_entity(&out);
    let all: croaring::Bitmap = (0..N).map(|e| source_of[&e]).collect();
    let got = |query: &str, minimum: Option<u32>| -> u64 {
        columns
            .resolve(
                "abstract",
                &FilterOperand::Match {
                    query: query.to_string(),
                    minimum,
                },
                &all,
            )
            .expect("a declared text column resolves")
            .cardinality()
    };

    // The control: both words together match something, so "nothing" below is a statement about
    // the minimum rather than about the fixture.
    assert!(got("quick fox", None) > 0);
    assert_eq!(
        got("quick fox", Some(3)),
        0,
        "three of two words is unsatisfiable; answering the conjunction is a wider question than \
         the one asked"
    );
    assert_eq!(got("quick fox", Some(9)), 0);

    // The same shape through deduplication: `quick quick brown` is two distinct words, so a
    // minimum of three is unsatisfiable for exactly the same reason.
    assert_eq!(got("quick quick brown", None), got("quick brown", None));
    assert_eq!(
        got("quick quick brown", Some(3)),
        0,
        "the denominator is the distinct token count, so a repeated word does not raise it"
    );
    // And the boundary still behaves: two of two is the conjunction, one of two the union.
    assert_eq!(got("quick fox", Some(2)), got("quick fox", None));
    assert!(got("quick fox", Some(1)) > got("quick fox", Some(2)));
}

/// **A negation over a text column is refused, not answered empty.**
///
/// `none_of` is `present ∖ matched`, and `present` is what makes it a positive predicate. A text
/// column stores no per-item value to be present — its index is the words its documents use, and
/// its prose is a blob row — so the presence half had nothing to union and every such request
/// answered "no items" with a 200: indistinguishable from a corpus where nothing matches, for every
/// principal, silently.
///
/// **Mutations this kills:** removing the `layers.is_empty()` guard in `present_in` (the request
/// answers empty and succeeds); making the guard return `Ok(candidate.clone())` instead (the
/// negation widens past what the column knows, which is the direction that discloses).
#[test]
fn a_negation_over_a_text_column_is_refused() {
    use tessera_engine::filter::{FilterExpr, FilterOperand};

    let dir = build_with(text_schema(true));
    let out = dir.path().join("bundle");
    let columns = open_columns(&out);
    let source_of = source_to_entity(&out);
    let all: croaring::Bitmap = (0..N).map(|e| source_of[&e]).collect();

    let negation = FilterExpr::NoneOf(vec![FilterExpr::Leaf {
        column: "abstract".to_string(),
        operand: FilterOperand::Match {
            query: "quick".to_string(),
            minimum: None,
        },
    }]);
    let err = columns
        .evaluate(&negation, &all)
        .expect_err("a negation over a text column must refuse rather than answer");
    assert!(
        err.is_callers_fault(),
        "the caller can fix this by asking a different question, so it is a 422 and not a 500"
    );
    let text = err.to_string();
    assert!(
        text.contains("abstract") && text.contains("text"),
        "the refusal must name the column and why: {text}"
    );

    // The positive form of the same question still answers, so what is refused is the negation and
    // not the column.
    assert!(
        columns
            .resolve(
                "abstract",
                &FilterOperand::Match {
                    query: "quick".to_string(),
                    minimum: None,
                },
                &all,
            )
            .expect("the positive predicate answers")
            .cardinality()
            > 0
    );
}
