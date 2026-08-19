//! The base build's half of the keyword family: a sorted dictionary of the column's distinct
//! values, and a `u32` ordinal per present entity naming a position in it
//! (`records-and-search.md` §4.3, §7).
//!
//! Read back through the readers that will serve them — `tessera_filter::SortedDict` and
//! `ValueColumn` — rather than by re-decoding the files here, for `filter_postings.rs`'s reason: a
//! test that reimplemented the format would agree with itself. What these assert that no unit test
//! can is the **pair**: the ordinal at an entity's slot, resolved against the dictionary written
//! beside it, is the value that entity carried in the points file.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{BinaryArray, Float64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::config::{Config, Schema};
use tessera_build::{build, BuildArgs};
use tessera_filter::{Access, Codes, SortedDict, ValueColumn, DICT_FILE};
use tessera_spatial::Bounds;
use tessera_store::open_bundle;
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const N: u64 = 40;

/// The fixture's shape is the family's argument in miniature: `doi` repeats heavily (interning is
/// what makes the dictionary smaller than the flat column on this shape), `arxiv_id` is unique per
/// item (the shape front coding wins on), and every seventh item carries neither — absence, which
/// must be no slot at all rather than ordinal 0.
fn doi_of(e: u64) -> Option<String> {
    if e.is_multiple_of(7) {
        return None;
    }
    Some(format!("10.1000/journal-{}", e % 3))
}

fn arxiv_id_of(e: u64) -> Option<String> {
    if e.is_multiple_of(7) {
        return None;
    }
    Some(format!("2601.{e:05}"))
}

fn test_key() -> IdentityKey {
    IdentityKey::from_hex(TEST_KEY_HEX).unwrap()
}

fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

fn write_points(path: &Path, doi: &dyn Fn(u64) -> Option<String>) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("doi", DataType::Utf8, true),
        Field::new("arxiv_id", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let dois: Vec<Option<String>> = ids.iter().map(|&e| doi(e)).collect();
    let arxiv: Vec<Option<String>> = ids.iter().map(|&e| arxiv_id_of(e)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(dois)),
            Arc::new(StringArray::from(arxiv)),
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
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(Vec::<u64>::new())),
            Arc::new(UInt32Array::from(Vec::<u32>::new())),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn parse_schema(text: &str) -> Schema {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("schema.toml");
    std::fs::write(&path, text).unwrap();
    Config::parse(&path, &HashMap::new()).expect("schema parses").schema
}

fn args(points: &Path, pairs: &Path, out: PathBuf, schema: Schema) -> BuildArgs {
    BuildArgs {
        point_fields: Default::default(),
        corpus_fields: Default::default(),
        points: points.to_path_buf(),
        corpus: Some(points.to_path_buf()),
        access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
        out,
        extent: extent(),
        view_id: "s0".to_string(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    }
}

const KEYWORD_SCHEMA: &str = r#"
[[attribute]]
name = "doi"
type = "keyword"
index = true

[[attribute]]
name = "arxiv_id"
type = "keyword"
index = true
"#;

fn current_prefix(out: &Path) -> String {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("CURRENT")).unwrap()).unwrap();
    current["prefix"].as_str().unwrap().to_string()
}

fn column_dir(out: &Path, column: &str) -> PathBuf {
    let bundle = open_bundle(out).unwrap();
    let phash = bundle.partitions.keys().next().unwrap().clone();
    out.join(current_prefix(out))
        .join("partitions")
        .join(phash)
        .join("attrs")
        .join(column)
}

/// Source id → entity id, through the external-id sidecar. Entity ids are signature-sorted, so a
/// source id is emphatically not its own entity id.
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
                let source = u64::from_le_bytes(ext.value(i).try_into().unwrap());
                map.insert(source, ent.value(i));
            }
        }
    }
    map
}

fn build_keyword_bundle(dir: &Path, doi: &dyn Fn(u64) -> Option<String>) -> PathBuf {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    write_points(&points, doi);
    write_empty_pairs(&pairs);
    let out = dir.join("bundle");
    build(&args(
        &points,
        &pairs,
        out.clone(),
        parse_schema(KEYWORD_SCHEMA),
    ))
    .expect("the build succeeds");
    out
}

/// The file set, and the ordinal column's width.
///
/// A keyword column's values file is fixed-width `u32` — not the flat string column `utf8` gets —
/// which is the storage claim §4.3 makes and the reason `eq` stops paying string prices. It earns
/// no postings: 0067's per-term postings are a separate, ruled addition, and nothing derives them
/// here.
#[test]
fn a_keyword_column_writes_a_dictionary_and_an_ordinal_column() {
    let dir = tempfile::tempdir().unwrap();
    let out = build_keyword_bundle(dir.path(), &doi_of);

    for column in ["doi", "arxiv_id"] {
        let cdir = column_dir(&out, column);
        assert!(cdir.join("values.arrow").exists(), "{column}: values");
        assert!(cdir.join(DICT_FILE).exists(), "{column}: dictionary");
        assert!(
            cdir.join("presence.roaring").exists(),
            "{column}: every seventh item carries nothing, so presence is partial"
        );
        assert!(
            !cdir.join("postings.arrow").exists(),
            "{column}: a keyword derives no postings at the build (decision 0067's are a \
             separate, ruled addition)"
        );

        let values = ValueColumn::open_dir(&cdir, Access::Mapped).unwrap();
        assert!(
            matches!(values.codes(), Codes::U32(_)),
            "{column}: the values file holds ordinals, not strings"
        );
    }
}

/// **The pair, which is the whole artefact.** An ordinal names a position in the dictionary written
/// beside it; resolving it anywhere else is a recolouring with no symptom, so what is checked here
/// is that this base's ordinals and this base's dictionary reconstruct the values the points file
/// carried — entity by entity, in both directions.
#[test]
fn an_entitys_ordinal_resolves_to_the_value_it_carried() {
    let dir = tempfile::tempdir().unwrap();
    let out = build_keyword_bundle(dir.path(), &doi_of);
    let entity_of = source_to_entity(&out);

    for (column, expected) in [
        ("doi", &doi_of as &dyn Fn(u64) -> Option<String>),
        ("arxiv_id", &arxiv_id_of),
    ] {
        let cdir = column_dir(&out, column);
        let values = ValueColumn::open_dir(&cdir, Access::Mapped).unwrap();
        let dict = SortedDict::open(&cdir.join(DICT_FILE), Access::Mapped).unwrap();
        dict.self_check().expect("the dictionary is well formed");
        let mut scratch = Vec::new();

        for source in 0..N {
            let entity = entity_of[&source];
            match expected(source) {
                None => assert_eq!(
                    values.value_of(entity),
                    None,
                    "{column}: source {source} carries no value, so it occupies no slot"
                ),
                Some(value) => {
                    let ordinal = values
                        .value_of(entity)
                        .unwrap_or_else(|| panic!("{column}: source {source} has an ordinal"));
                    assert_eq!(
                        dict.key_of(ordinal.raw(), &mut scratch).unwrap(),
                        value,
                        "{column}: source {source}"
                    );
                    // And the other direction: the needle the caller would present resolves to
                    // exactly the ordinal the column stores.
                    assert_eq!(
                        dict.resolve(&value).unwrap(),
                        Some(ordinal.raw()),
                        "{column}: source {source}"
                    );
                }
            }
        }
    }
}

/// **Each distinct value once, whatever its multiplicity.** The interning is where the repeat-heavy
/// shape's bytes go, and a dictionary that stored a key per entity would still pass every
/// round-trip above.
#[test]
fn the_dictionary_holds_each_distinct_value_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let out = build_keyword_bundle(dir.path(), &doi_of);

    for (column, expected) in [
        ("doi", &doi_of as &dyn Fn(u64) -> Option<String>),
        ("arxiv_id", &arxiv_id_of),
    ] {
        let distinct: HashSet<String> = (0..N).filter_map(expected).collect();
        let cdir = column_dir(&out, column);
        let dict = SortedDict::open(&cdir.join(DICT_FILE), Access::Mapped).unwrap();
        assert_eq!(dict.len() as usize, distinct.len(), "{column}");

        let mut walked = Vec::new();
        dict.walk(|_, key| walked.push(key.to_string())).unwrap();
        let mut sorted: Vec<String> = distinct.into_iter().collect();
        sorted.sort();
        assert_eq!(walked, sorted, "{column}: sorted, distinct, and complete");
    }
}

/// **A keyword's ordinals are this layer's and nobody else's**, so nothing about them is stable
/// across columns that happen to hold the same value. `doi` repeats three values and `arxiv_id`
/// holds forty distinct ones; the two dictionaries share no numbering, which is what keeps a
/// durable manufactured identity — and C11's reuse hazard with it — structurally out of the family.
#[test]
fn ordinals_are_scoped_to_their_own_dictionary() {
    let dir = tempfile::tempdir().unwrap();
    let out = build_keyword_bundle(dir.path(), &doi_of);

    let doi_dict = SortedDict::open(&column_dir(&out, "doi").join(DICT_FILE), Access::Mapped)
        .expect("the doi dictionary opens");
    let arxiv_dict = SortedDict::open(
        &column_dir(&out, "arxiv_id").join(DICT_FILE),
        Access::Mapped,
    )
    .expect("the arxiv_id dictionary opens");

    assert_eq!(doi_dict.len(), 3, "three distinct dois");
    assert_ne!(
        doi_dict.len(),
        arxiv_dict.len(),
        "two columns' dictionaries are two numberings; an ordinal is meaningful in exactly one"
    );
    // An ordinal legal in one is out of range in the other, which is the refusal a reader gets
    // rather than a neighbour's key.
    let mut scratch = Vec::new();
    assert!(doi_dict.key_of(arxiv_dict.len() - 1, &mut scratch).is_err());
}

/// A column no item carries a value in still gets its file set — the set is a function of the
/// schema, not of the data — and the dictionary is empty rather than absent.
#[test]
fn a_column_with_no_values_still_writes_an_empty_dictionary() {
    let dir = tempfile::tempdir().unwrap();
    let out = build_keyword_bundle(dir.path(), &|_| None);

    let cdir = column_dir(&out, "doi");
    assert!(cdir.join(DICT_FILE).exists());
    let dict = SortedDict::open(&cdir.join(DICT_FILE), Access::Mapped).unwrap();
    assert!(dict.is_empty(), "no values, so no keys");
    dict.self_check()
        .expect("an empty dictionary is well formed");

    let values = ValueColumn::open_dir(&cdir, Access::Mapped).unwrap();
    assert_eq!(values.present().cardinality(), 0);
}

/// **The empty string is not a value in this family**, where it is one in `utf8`'s. Records §7
/// refuses it on the ingest wire for contracts §2.4's reason, and the dictionary has no key for
/// it — so a points file that carries one is refused at the build rather than stored as a key or
/// silently read as absence.
#[test]
fn the_empty_string_is_refused_rather_than_stored_or_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points(&points, &|e| {
        if e == 3 {
            Some(String::new())
        } else {
            doi_of(e)
        }
    });
    write_empty_pairs(&pairs);
    let out = dir.path().join("bundle");
    let error = build(&args(&points, &pairs, out, parse_schema(KEYWORD_SCHEMA)))
        .expect_err("the empty string is refused");
    let message = error.to_string();
    assert!(message.contains("empty string"), "{message}");
    assert!(message.contains("doi"), "{message}");
}

/// The declaration reaches the manifest under its own name, which is what lets a reader tell an
/// ordinal column from a flat string one without opening either.
#[test]
fn the_manifest_records_the_declared_type() {
    let dir = tempfile::tempdir().unwrap();
    let out = build_keyword_bundle(dir.path(), &doi_of);
    let bundle = open_bundle(&out).unwrap();
    let declared = &bundle.manifest.declared_scalars;

    let doi = declared.iter().find(|d| d.name == "doi").expect("declared");
    assert_eq!(doi.arrow_type.arrow_type_name(), "keyword");
    assert!(doi.index);
    assert!(!doi.render, "`render` on a keyword is refused at the parse");
    assert!(
        doi.vocabulary.is_none(),
        "a keyword has no vocabulary — that is what distinguishes it from a category"
    );
}
