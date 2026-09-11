//! **A corpus that numbers its rows is built without a source-ids array**, and the bundle is the
//! same bundle either way.
//!
//! Pass one proves the union of the views' source ids is one unbroken range from a presence bitmap
//! a sixty-fourth of the array's size, and where it holds, the ordinal of an id is `id - first`:
//! no array is allocated, none is sorted and none is written (28 GB of writes at the GBIF rung).
//! The proof needs a bound on the union's span before it can size the bitmap, and it takes that
//! from each points file's own parquet statistics. **A file whose row groups carry no statistics
//! cannot be bounded, so the build falls back to the array** — same corpus, same ids, the other
//! route through pass one.
//!
//! That is what this file forces, and it forces it without a test seam: the same ids are written
//! to the same path twice, once with statistics and once with `EnabledStatistics::None`. The route
//! each build took is read back from its own report rather than assumed, so a change that stopped
//! taking either one would fail here rather than pass while checking nothing. The two bundles must
//! be byte-identical, because the ordinal is what the entity-id assignment breaks its last tie on
//! (decision 0112) and a route that moved an ordinal would move every entity id the build assigns
//! (I9).

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Float64Array, Int64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::{EnabledStatistics, WriterProperties};

use tessera_build::config::{Attribute, Schema};
use tessera_build::{build, BuildArgs};
use tessera_spatial::tiler::ScalarType;
use tessera_spatial::Bounds;
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const N: u64 = 600;

/// The points file, its `entity_id` column either described by statistics or not.
///
/// Row groups small enough that the file holds several of them, so the statistics fold below has
/// more than one bound to fold.
fn write_points(path: &Path, statistics: EnabledStatistics) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("note", DataType::Utf8, true),
        Field::new("flag", DataType::Int64, false),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter().map(|e| ((e * 37) % 1000) as f64).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter().map(|e| ((e * 53) % 1000) as f64).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter()
                    .map(|&e| (!e.is_multiple_of(7)).then(|| format!("note-{}", e % 13)))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                ids.iter().map(|&e| (e % 3) as i64).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let properties = WriterProperties::builder()
        .set_statistics_enabled(statistics)
        .set_max_row_group_row_count(Some(128))
        .build();
    let mut writer =
        ArrowWriter::try_new(File::create(path).unwrap(), schema, Some(properties)).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// The access relation: every item in two of four terms, so the dictionary pass, the packed
/// relation and the postings all resolve ordinals rather than counting an empty file.
fn write_pairs(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for entity in 0..N {
        for term in [entity % 4, (entity / 4) % 4] {
            entities.push(entity);
            terms.push(term as u32);
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
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

fn schema() -> Schema {
    Schema {
        attributes: vec![
            Attribute {
                field: None,
                name: "note".to_string(),
                title: None,
                ty: ScalarType::Keyword,
                analyser: None,
                vocabulary: None,
                value_set: None,
                index: true,
                render: false,
            },
            Attribute {
                field: None,
                name: "flag".to_string(),
                title: None,
                ty: ScalarType::I64,
                analyser: None,
                vocabulary: None,
                value_set: None,
                index: false,
                render: true,
            },
        ],
        vocabularies: Default::default(),
    }
}

fn args(points: &Path, pairs: &Path, out: PathBuf) -> BuildArgs {
    let schema = schema();
    BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: Bounds {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            points: points.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(
            points.to_path_buf(),
            &schema,
        ),
        out,
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
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

/// Compare two bundles file for file, with `MANIFEST.json`'s wall-clock `created_at` blanked and
/// the `CURRENT` that carries its digest skipped. `tests/build_equivalence.rs` and
/// `tests/extent_route.rs` hold the same comparison; it is repeated here rather than shared
/// because a test binary is its own crate.
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
        if name.ends_with("MANIFEST.json") {
            let normalise = |bytes: &[u8]| {
                let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                value["created_at"] = serde_json::Value::Null;
                value
            };
            assert_eq!(
                normalise(left_bytes),
                normalise(right_bytes),
                "{what}: {name} differs (ignoring created_at)"
            );
            continue;
        }
        if name == "CURRENT" {
            continue;
        }
        assert_eq!(
            left_bytes,
            right_bytes,
            "{what}: {name} is not byte-identical ({} vs {} bytes)",
            left_bytes.len(),
            right_bytes.len()
        );
    }
    assert!(
        a.len() > 6,
        "{what}: expected a full bundle, found {} files",
        a.len()
    );
}

/// **The route pass one takes to the ordinal space is not in the bundle.** One corpus, both
/// routes, one set of bytes.
#[test]
fn a_bounded_and_an_unbounded_id_column_build_the_same_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_pairs(&pairs);

    // Bounded: the statistics bound the span, the bitmap proves the range, no array is written.
    write_points(&points, EnabledStatistics::Chunk);
    let bounded = dir.path().join("bounded");
    let bounded_report =
        build(&args(&points, &pairs, bounded.clone())).expect("the bounded build succeeds");
    assert_eq!(
        bounded_report.source_id_slots, 0,
        "a bounded id column that is one unbroken range is built with no source-ids array"
    );
    let bounded_files = collect(&bounded);

    // Unbounded: the same ids in a file that states nothing about them, so pass one reads them
    // into the array, sorts it and dedups it, exactly as it did before the range was provable.
    write_points(&points, EnabledStatistics::None);
    let unbounded = dir.path().join("unbounded");
    let unbounded_report =
        build(&args(&points, &pairs, unbounded.clone())).expect("the unbounded build succeeds");
    assert_eq!(
        unbounded_report.source_id_slots, N,
        "an id column the statistics cannot bound is read into the array, one slot a row"
    );

    assert!(
        !bounded_files.is_empty(),
        "the bounded build wrote a bundle to compare"
    );
    assert_bundles_identical(&bounded, &unbounded, "the two routes to the ordinal space");
}
