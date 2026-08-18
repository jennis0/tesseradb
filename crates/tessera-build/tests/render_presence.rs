//! A render column's presence bitmap, through a real build (decision 0064).
//!
//! `columns.arrow` is non-nullable by contract (R4), so an item with no score is written as the
//! type's zero and is indistinguishable from one scoring zero — which makes it match a range
//! containing zero, a wrong answer rather than a missing feature. The bitmap beside the column is
//! what tells the two apart, and these are the cases that hold the build to writing it.
//!
//! The declarations are constructed rather than parsed, as `tests/record_blob.rs` does, because a
//! category's vocabulary is what a fixture would otherwise spend most of its lines on.
//!
//! **The two builds' agreement is not asserted here.** `tests/build_equivalence.rs` compares every
//! byte of both bundles over a fixture whose rendered numbers already carry nulls, so a build that
//! wrote a different bitmap — or none — fails there, on the comparison that exists for exactly
//! that class of divergence.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Float64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::config::{Attribute, Listing, Schema, Vocabulary, ValueSet};
use tessera_build::{build, BuildArgs};
use tessera_spatial::tiler::ScalarType;
use tessera_spatial::Bounds;
use tessera_store::open_bundle;
use tessera_store::read::{ColumnsRef, ScalarSlice};
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const N: u64 = 40;

/// Every fifth item has no score. **Never zero where it is present**, so a row holding zero in the
/// column is exactly a row with no value and the assertions need no second source of truth.
fn score_of(e: u64) -> Option<f64> {
    (!e.is_multiple_of(5)).then_some(e as f64 + 1.0)
}

/// A rendered number with no absence anywhere — the column that must cost no file.
fn weight_of(e: u64) -> f64 {
    (e % 7) as f64 + 0.5
}

/// A rendered **category** with absences. Its vocabulary reserves code 0 out of the value space,
/// so it says "nothing" in the column itself and must not acquire a bitmap as well.
fn archive_of(e: u64) -> Option<String> {
    (!e.is_multiple_of(3)).then(|| ARCHIVE_VALUES[(e % 2) as usize].0.to_string())
}

const ARCHIVE_VALUES: &[(&str, u32)] = &[("astro-ph", 211), ("cs", 37)];

fn write_points(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("score", DataType::Float64, true),
        Field::new("weight", DataType::Float64, true),
        Field::new("archive", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(Float64Array::from(
                ids.iter().map(|&e| score_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter().map(|&e| Some(weight_of(e))).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|&e| archive_of(e)).collect::<Vec<_>>(),
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

fn render(name: &str, ty: ScalarType) -> Attribute {
    Attribute {
        field: None,
        name: name.to_string(),
        title: None,
        ty,
        analyser: None,
        vocabulary: None,
        value_set: None,
        index: false,
        render: true,
    }
}

fn schema() -> Schema {
    let archive = Attribute {
        field: None,
        name: "archive".to_string(),
        title: None,
        ty: ScalarType::U8,
        analyser: None,
        vocabulary: Some("archive".to_string()),
        value_set: Some(ValueSet::Closed),
        index: false,
        render: true,
    };
    Schema {
        attributes: vec![
            render("score", ScalarType::F64),
            render("weight", ScalarType::F64),
            archive,
        ],
        vocabularies: HashMap::from([(
            "archive".to_string(),
            Vocabulary {
                name: "archive".to_string(),
                title: None,
                value_set: ValueSet::Closed,
                width: ScalarType::U8,
                visibility: Listing::Public,
                codes: ARCHIVE_VALUES
                    .iter()
                    .map(|(k, c)| (k.to_string(), *c))
                    .collect::<BTreeMap<_, _>>(),
                titles: BTreeMap::new(),
                reserved: Vec::new(),
            },
        )]),
    }
}

fn args(points: &Path, pairs: &Path, out: PathBuf) -> BuildArgs {
    BuildArgs {
        point_fields: Default::default(),
        corpus_fields: Default::default(),
        points: points.to_path_buf(),
        corpus: Some(points.to_path_buf()),
        access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
        out,
        extent: Bounds {
            x_min: 0.0,
            x_max: 1000.0,
            y_min: 0.0,
            y_max: 1000.0,
        },
        view_id: "s0".to_string(),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        artifacts: None,
        artifact_members: None,
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: schema(),
    }
}

fn build_bundle() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points(&points);
    write_empty_pairs(&pairs);
    build(&args(&points, &pairs, dir.path().join("bundle"))).expect("the build succeeds");
    dir
}

fn segment_dir(out: &Path) -> PathBuf {
    out.join("v00000/partitions/default/views/s0/segments/seg-0")
}

/// The bitmap says exactly which rows carry a score, and the column still holds the type's zero at
/// the rows that do not — which is the pair of facts the whole ruling is about. Read through
/// `ColumnsRef`, which is how the serving path will read it.
#[test]
fn a_built_segment_marks_the_rows_with_no_value_and_stores_zero_at_them() {
    let dir = build_bundle();
    let out = dir.path().join("bundle");
    let columns = ColumnsRef::load(&segment_dir(&out).join("columns.arrow")).expect("columns");

    let ScalarSlice::F64(scores) = columns.scalar("score").expect("score column") else {
        panic!("score is declared f64");
    };
    let presence = columns.presence("score");
    let absent = (0..N as usize).filter(|&row| scores[row] == 0.0).count();
    assert_eq!(
        absent,
        (0..N).filter(|e| score_of(*e).is_none()).count(),
        "the fixture's absent items are the ones the column holds zero for"
    );
    assert!(absent > 0, "a fixture with no absence tests nothing here");
    for (row, score) in scores.iter().enumerate() {
        assert_eq!(
            presence.contains(row as u32),
            *score != 0.0,
            "row {row} holds {score} and the bitmap disagrees about whether that is a value"
        );
    }
}

/// **A column with no absence has no file, and a category never has one at all.** The category is
/// the case worth pinning: it has absences, and it already spends its vocabulary's reserved code 0
/// on them, so a second mechanism beside the column is the muddle decision 0064 declines.
#[test]
fn no_file_is_written_for_a_full_column_or_for_a_category() {
    let dir = build_bundle();
    let out = dir.path().join("bundle");
    let presence_dir = segment_dir(&out).join("presence");

    let mut written: Vec<String> = std::fs::read_dir(&presence_dir)
        .expect("the score column wrote one, so the directory exists")
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    written.sort();
    assert_eq!(written, vec!["score.roaring".to_string()]);

    let columns = ColumnsRef::load(&segment_dir(&out).join("columns.arrow")).expect("columns");
    assert!(
        (0..N as u32).all(|row| columns.presence("weight").contains(row)),
        "no file means every row present, and nothing downstream may tell that from a full bitmap"
    );
    let ScalarSlice::U8(archives) = columns.scalar("archive").expect("archive column") else {
        panic!("archive is declared u8");
    };
    assert!(
        archives.contains(&0),
        "the fixture's category must carry absences, or it pins nothing"
    );
    assert!((0..N as u32).all(|row| columns.presence("archive").contains(row)));
}

/// The manifest names and digests it like every other per-segment artefact, so a bundle whose
/// bitmap went missing refuses at open instead of reading as "every row present" — which is the
/// fail-open answer, every valueless item matching a range containing zero.
#[test]
fn the_bitmap_is_digested_and_a_missing_one_refuses_at_open() {
    let dir = build_bundle();
    let out = dir.path().join("bundle");
    let rel = "partitions/default/views/s0/segments/seg-0/presence/score.roaring";

    let bundle = open_bundle(&out).expect("the bundle opens");
    let manifest = &bundle.manifest;
    let digest = manifest
        .files
        .get(rel)
        .expect("MANIFEST.files must name the bitmap");
    assert_eq!(
        digest.size,
        std::fs::metadata(segment_dir(&out).join("presence/score.roaring"))
            .unwrap()
            .len()
    );

    std::fs::remove_file(segment_dir(&out).join("presence/score.roaring")).unwrap();
    assert!(
        open_bundle(&out).is_err(),
        "a named file that is gone is a refusal, never a column that reads as fully present"
    );
}
