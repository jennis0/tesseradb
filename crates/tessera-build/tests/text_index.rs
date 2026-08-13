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
