//! The materialisers against the lookups: every way in must state the same corpus (spec §12.1's
//! "one source"), so each written form is read back and compared against [`Corpus::item`] and
//! [`Corpus::terms`] — the functions total verification will later hold served rows against.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use arrow::array::{
    Array, ArrayAccessor, BinaryArray, Float64Array, StringArray, TimestampMicrosecondArray,
    UInt32Array, UInt64Array,
};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use tessera_corpus::materialise::PARTITION_LAYER;
use tessera_corpus::Corpus;
use tessera_spatial::Bounds;

fn corpus(n: u64) -> Corpus {
    Corpus::new(
        20260815,
        n,
        Bounds {
            x_min: 0.0,
            x_max: 65536.0,
            y_min: 0.0,
            y_max: 65536.0,
        },
    )
    .unwrap()
}

fn read_parquet(path: &std::path::Path) -> Vec<RecordBatch> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(path).unwrap())
        .unwrap()
        .build()
        .unwrap();
    reader.map(|b| b.unwrap()).collect()
}

fn column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> &'a T {
    batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("no column '{name}'"))
        .as_any()
        .downcast_ref::<T>()
        .unwrap_or_else(|| panic!("column '{name}' has the wrong type"))
}

/// Every row of the points file is its item: geometry and all six declared fields, absence as
/// null, exactly as the generator states them.
#[test]
fn points_parquet_rows_are_the_items() {
    let c = corpus(300);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("points.parquet");
    c.write_points_parquet(&path).unwrap();

    let mut rows = 0u64;
    for batch in read_parquet(&path) {
        let entity_id = column::<UInt64Array>(&batch, "entity_id");
        let x = column::<Float64Array>(&batch, "x");
        let y = column::<Float64Array>(&batch, "y");
        let fx_key = column::<UInt64Array>(&batch, "fx_key");
        let weight = column::<UInt32Array>(&batch, "weight");
        let seen_at = column::<TimestampMicrosecondArray>(&batch, "seen_at");
        let bay = column::<StringArray>(&batch, "bay");
        let tag = column::<StringArray>(&batch, "tag");
        let blurb = column::<StringArray>(&batch, "blurb");
        for i in 0..batch.num_rows() {
            let e = entity_id.value(i);
            assert_eq!(e, rows, "entity ids are the items, in order");
            let item = c.item(e);
            assert_eq!(x.value(i), item.x);
            assert_eq!(y.value(i), item.y);
            assert_eq!(fx_key.value(i), item.fx_key);
            assert_eq!(c.item_of_fx_key(fx_key.value(i)), e, "the join inverts");
            assert_eq!(opt(weight, i), item.weight);
            assert_eq!(opt(seen_at, i), item.seen_at);
            assert_eq!(opt_str(bay, i).as_deref(), item.bay);
            assert_eq!(opt_str(tag, i), item.tag);
            assert_eq!(opt_str(blurb, i), item.blurb);
            rows += 1;
        }
    }
    assert_eq!(rows, c.n());
}

fn opt<'a, A, T>(array: &'a A, i: usize) -> Option<T>
where
    A: Array,
    &'a A: arrow::array::ArrayAccessor<Item = T>,
{
    (!array.is_null(i)).then(|| array.value(i))
}

fn opt_str(array: &StringArray, i: usize) -> Option<String> {
    (!array.is_null(i)).then(|| array.value(i).to_string())
}

/// A smaller build input is a **prefix** of a larger one, row for row — the file-level face of
/// spec §8's first property, and what makes a big run's failure reducible by `--limit` alone.
#[test]
fn a_smaller_points_file_is_a_prefix_of_a_larger_one() {
    let dir = tempfile::tempdir().unwrap();
    let small = dir.path().join("small.parquet");
    let large = dir.path().join("large.parquet");
    corpus(100).write_points_parquet(&small).unwrap();
    corpus(300).write_points_parquet(&large).unwrap();

    let rows = |path| {
        read_parquet(path)
            .iter()
            .flat_map(|batch| {
                let e = column::<UInt64Array>(batch, "entity_id");
                let fx = column::<UInt64Array>(batch, "fx_key");
                let x = column::<Float64Array>(batch, "x");
                (0..batch.num_rows())
                    .map(|i| (e.value(i), fx.value(i), x.value(i).to_bits()))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    let (small_rows, large_rows) = (rows(&small), rows(&large));
    assert_eq!(small_rows.len(), 100);
    assert_eq!(&large_rows[..100], &small_rows[..]);
}

/// The pairs file is the exploded `terms` relation, grouped back and compared per item.
#[test]
fn pairs_parquet_rows_are_the_terms() {
    let c = corpus(300);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pairs.parquet");
    c.write_pairs_parquet(&path).unwrap();

    let mut by_item: HashMap<u64, Vec<u32>> = HashMap::new();
    for batch in read_parquet(&path) {
        let entity_id = column::<UInt64Array>(&batch, "entity_id");
        let term_id = column::<UInt32Array>(&batch, "term_id");
        for i in 0..batch.num_rows() {
            by_item
                .entry(entity_id.value(i))
                .or_default()
                .push(term_id.value(i));
        }
    }
    assert_eq!(
        by_item.len() as u64,
        c.n(),
        "every item carries at least one pair"
    );
    for e in 0..c.n() {
        let expected: Vec<u32> = c.terms(e).iter().map(|t| t.raw()).collect();
        assert_eq!(by_item[&e], expected, "item {e}");
    }
}

/// The ingest batch is the wire shape — `(external_id, x, y, access, the declared scalars)` —
/// with the workspace's 8-byte little-endian external-id convention, the access labels as a list
/// (one term per element, decision 0129), and the same values as every other materialiser. A range past `n` draws from the same
/// functions, which is what lets a driver ingest beyond the built prefix.
#[test]
fn ingest_batch_is_the_wire_shape_of_the_same_items() {
    let c = corpus(10);
    let batch = c.ingest_batch(10..30);
    assert_eq!(batch.num_rows(), 20);
    assert_eq!(
        batch
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect::<Vec<_>>(),
        [
            "external_id",
            "x",
            "y",
            "access",
            "fx_key",
            "weight",
            "seen_at",
            "bay",
            "tag",
            "blurb",
            "partition"
        ]
    );

    let external_id = column::<BinaryArray>(&batch, "external_id");
    let x = column::<Float64Array>(&batch, "x");
    let access = column::<arrow::array::ListArray>(&batch, "access");
    let fx_key = column::<UInt64Array>(&batch, "fx_key");
    let bay = column::<StringArray>(&batch, "bay");
    let partition = column::<UInt32Array>(&batch, "partition");
    for i in 0..batch.num_rows() {
        let e = 10 + i as u64;
        let item = c.item(e);
        assert_eq!(external_id.value(i), e.to_le_bytes());
        assert_eq!(x.value(i), item.x);
        // Every row of this batch is past `n = 10`, which is the point: the partition value is a
        // function of `(seed, layer, e)` and is answerable beyond the built prefix.
        assert_eq!(
            u64::from(partition.value(i)),
            c.partition_artifact_of(PARTITION_LAYER, e)
        );
        let expected_access: Vec<String> = c.terms(e).iter().map(|t| t.raw().to_string()).collect();
        let labels = access.value(i);
        let labels = labels
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("access is a list of utf8");
        let got: Vec<String> = (0..labels.len())
            .map(|j| labels.value(j).to_string())
            .collect();
        assert_eq!(
            got, expected_access,
            "item {e}: one label per element, none joined"
        );
        assert_eq!(fx_key.value(i), item.fx_key);
        assert_eq!(opt_str(bay, i).as_deref(), item.bay);
    }

    // The reserved wire columns are non-nullable; every declared scalar but the join key admits
    // absence. A nullability defect here surfaces server-side as a refused batch, so pin it.
    for field in batch.schema().fields() {
        let admits_absence = matches!(
            field.name().as_str(),
            "weight" | "seen_at" | "bay" | "tag" | "blurb"
        );
        assert_eq!(
            field.is_nullable(),
            admits_absence,
            "column '{}'",
            field.name()
        );
    }
}

/// The artifact fixture's files agree with the closed forms they were written from — the same
/// property [`points_parquet_rows_are_the_items`] pins for the item arm, extended to all four
/// closed-form artifact arms (flat, partition, boundary, treed). Small `n`, so a brute-force
/// comparison is affordable in the test itself.
#[test]
fn the_artifact_fixture_agrees_with_the_closed_forms() {
    use tessera_corpus::materialise::{
        ArtifactFixtureCounts, BOUNDARY_LAYER, FIXTURE_LEVEL, FLAT_LAYER,
    };

    let c = corpus(6_000);
    let dir = tempfile::tempdir().unwrap();
    let counts = c.write_artifact_fixtures(dir.path()).unwrap();

    // The partition column on points.parquet agrees with `partition_artifact_of` for every entity.
    let points_path = dir.path().join("points.parquet");
    c.write_points_parquet(&points_path).unwrap();
    let mut partition_by_entity: HashMap<u64, u32> = HashMap::new();
    for batch in read_parquet(&points_path) {
        let entity_id = column::<UInt64Array>(&batch, "entity_id");
        let partition = column::<UInt32Array>(&batch, "partition");
        for i in 0..batch.num_rows() {
            partition_by_entity.insert(entity_id.value(i), partition.value(i));
        }
    }
    for e in 0..c.n() {
        assert_eq!(
            u64::from(partition_by_entity[&e]),
            c.partition_artifact_of(PARTITION_LAYER, e),
            "points.parquet's partition column disagrees with the closed form at entity {e}"
        );
    }

    // The flat roster and membership: `flat_artifacts.len()` artifacts, each artifact's members
    // exactly `artifact_members` — both directions, since the roster names the count and the
    // membership file is checked entity by entity below.
    let roster: Vec<String> = read_parquet(&dir.path().join("flat_artifacts.parquet"))
        .iter()
        .flat_map(|b| {
            let key = column::<StringArray>(b, "key");
            (0..b.num_rows())
                .map(|i| key.value(i).to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(roster.len() as u64, counts.flat_artifacts);
    assert_eq!(
        roster,
        (0..counts.flat_artifacts)
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
    );

    let mut flat_members: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
    let mut flat_rows = 0u64;
    for batch in read_parquet(&dir.path().join("flat_members.parquet")) {
        let key = column::<StringArray>(&batch, "key");
        let entity = column::<UInt64Array>(&batch, "entity");
        for i in 0..batch.num_rows() {
            let a: u64 = key.value(i).parse().unwrap();
            flat_members.entry(a).or_default().insert(entity.value(i));
            flat_rows += 1;
        }
    }
    assert_eq!(flat_rows, counts.flat_member_rows);
    for a in 0..counts.flat_artifacts {
        let expected: BTreeSet<u64> = c
            .artifact_members(FLAT_LAYER, FIXTURE_LEVEL, a)
            .into_iter()
            .collect();
        assert_eq!(
            flat_members.get(&a).cloned().unwrap_or_default(),
            expected,
            "artifact {a}'s materialised membership disagrees with `artifact_members`"
        );
    }

    // The partition roster and its enumerated twin: single-valued, so every entity names exactly
    // one key, and that key is `partition_artifact_of`.
    let mut partition_members: HashMap<u64, u64> = HashMap::new();
    let mut partition_rows = 0u64;
    for batch in read_parquet(&dir.path().join("partition_members.parquet")) {
        let key = column::<StringArray>(&batch, "key");
        let entity = column::<UInt64Array>(&batch, "entity");
        for i in 0..batch.num_rows() {
            let a: u64 = key.value(i).parse().unwrap();
            let prior = partition_members.insert(entity.value(i), a);
            assert!(prior.is_none(), "entity {} named twice", entity.value(i));
            partition_rows += 1;
        }
    }
    assert_eq!(partition_rows, c.n(), "the partition twin is exhaustive");
    assert_eq!(partition_rows, counts.partition_member_rows);
    for e in 0..c.n() {
        assert_eq!(
            partition_members[&e],
            c.partition_artifact_of(PARTITION_LAYER, e)
        );
    }

    // The boundary roster: every key is an authored prefix, and the set is exactly
    // `boundary_artifacts`'s.
    let boundary_roster: BTreeSet<u64> =
        read_parquet(&dir.path().join("boundary_artifacts.parquet"))
            .iter()
            .flat_map(|b| {
                let key = column::<StringArray>(b, "key");
                (0..b.num_rows())
                    .map(|i| key.value(i).parse::<u64>().unwrap())
                    .collect::<Vec<_>>()
            })
            .collect();
    let expected_boundary: BTreeSet<u64> = c
        .boundary_artifacts(BOUNDARY_LAYER, FIXTURE_LEVEL)
        .into_iter()
        .collect();
    assert_eq!(boundary_roster, expected_boundary);
    assert_eq!(boundary_roster.len() as u64, counts.boundary_artifacts);

    // The treed roster and lineage: `key` ascending `0..treed_artifacts`, `parent` null only at
    // the root and otherwise `artifact_parent`'s answer, and the membership file agreeing with
    // `treed_members` node by node.
    let mut treed_parent: BTreeMap<u64, Option<u64>> = BTreeMap::new();
    for batch in read_parquet(&dir.path().join("treed_artifacts.parquet")) {
        let key = column::<StringArray>(&batch, "key");
        let parent = column::<StringArray>(&batch, "parent");
        for i in 0..batch.num_rows() {
            let a: u64 = key.value(i).parse().unwrap();
            let p = (!parent.is_null(i)).then(|| parent.value(i).parse::<u64>().unwrap());
            treed_parent.insert(a, p);
        }
    }
    assert_eq!(treed_parent.len() as u64, counts.treed_artifacts);
    for a in 0..counts.treed_artifacts {
        assert_eq!(
            treed_parent[&a],
            c.artifact_parent(a),
            "node {a}'s materialised parent disagrees with `artifact_parent`"
        );
    }

    let mut treed_members: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
    let mut treed_rows = 0u64;
    for batch in read_parquet(&dir.path().join("treed_members.parquet")) {
        let key = column::<StringArray>(&batch, "key");
        let entity = column::<UInt64Array>(&batch, "entity");
        for i in 0..batch.num_rows() {
            let a: u64 = key.value(i).parse().unwrap();
            treed_members.entry(a).or_default().insert(entity.value(i));
            treed_rows += 1;
        }
    }
    assert_eq!(treed_rows, counts.treed_member_rows);
    for a in 0..counts.treed_artifacts {
        let expected: BTreeSet<u64> = c
            .treed_members(counts.treed_artifacts, a)
            .into_iter()
            .collect();
        assert_eq!(
            treed_members.get(&a).cloned().unwrap_or_default(),
            expected,
            "node {a}'s materialised membership disagrees with `treed_members`"
        );
    }

    let _: ArtifactFixtureCounts = counts; // the type is part of the public surface under test
}
