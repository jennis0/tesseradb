//! Round-trip test for the tiler + segment writers (Task 3, Step 1): sort a batch, write
//! `columns.arrow` / `morton.u64` / `permutation.bin`, and read every byte back.

use std::fs;
use std::io::Read;

use arrow::array::{Array, Float32Array, UInt16Array, UInt32Array, UInt64Array};
use arrow::datatypes::DataType;
use arrow::ipc::reader::FileReader;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use tessera_spatial::tiler::{sort_batch, ScalarType, TilerItem};
use tessera_spatial::Extent;
use tessera_store::write::{write_permutation, write_segment};
use tessera_types::EntityId;

/// R3 priority: `(splitmix64(entity_id) >> 48) as u16`.
fn priority(entity_id: u64) -> u16 {
    let mut z = entity_id.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^= z >> 31;
    (z >> 48) as u16
}

fn unit_extent() -> Extent {
    Extent {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
}

#[test]
fn tiler_and_segment_writers_round_trip() {
    let mut rng = StdRng::seed_from_u64(42);
    let n = 1_000u64;

    let mut items: Vec<TilerItem> = (0..n)
        .map(|entity_id| TilerItem {
            entity_id: EntityId::new(entity_id),
            x: rng.gen_range(0.0f32..1.0),
            y: rng.gen_range(0.0f32..1.0),
            node_id: 0xFFFF_FFFF,
            priority: priority(entity_id),
            scalars: vec![],
        })
        .collect();

    let extent = unit_extent();
    let codes = sort_batch(&mut items, &extent);
    assert_eq!(codes.len(), items.len());

    let dir = tempfile::tempdir().expect("tempdir");
    write_segment(dir.path(), &items, &codes, &[]).expect("write_segment");

    let row_order_entities: Vec<EntityId> = items.iter().map(|i| i.entity_id).collect();
    let bound = n; // entity ids are 0..n, contiguous
    let perm_path = dir.path().join("permutation.bin");
    write_permutation(&perm_path, &row_order_entities, bound).expect("write_permutation");

    // (a) morton.u64: non-decreasing, equal to the codes sort_batch returned.
    let morton_bytes = fs::read(dir.path().join("morton.u64")).expect("read morton.u64");
    assert_eq!(morton_bytes.len(), items.len() * 8);
    let file_codes: Vec<u64> = morton_bytes
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(file_codes, codes);
    assert!(file_codes.windows(2).all(|w| w[0] <= w[1]));

    // (b) columns.arrow: schema field names/types match R4; row 0 matches items[0] post-sort.
    let file = fs::File::open(dir.path().join("columns.arrow")).expect("open columns.arrow");
    let mut reader = FileReader::try_new(file, None).expect("FileReader::try_new");
    let schema = reader.schema();
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, vec!["entity_id", "x", "y", "node_id", "priority"]);
    assert_eq!(schema.field(0).data_type(), &DataType::UInt64);
    assert_eq!(schema.field(1).data_type(), &DataType::Float32);
    assert_eq!(schema.field(2).data_type(), &DataType::Float32);
    assert_eq!(schema.field(3).data_type(), &DataType::UInt32);
    assert_eq!(schema.field(4).data_type(), &DataType::UInt16);

    let batch = reader.next().expect("one batch").expect("batch ok");
    assert!(reader.next().is_none(), "expected exactly one record batch");
    assert_eq!(batch.num_rows(), items.len());

    let entity_id_col = batch
        .column(0)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    assert_eq!(entity_id_col.value(0), items[0].entity_id.raw());
    let x_col = batch
        .column(1)
        .as_any()
        .downcast_ref::<Float32Array>()
        .unwrap();
    assert_eq!(x_col.value(0), items[0].x);
    let y_col = batch
        .column(2)
        .as_any()
        .downcast_ref::<Float32Array>()
        .unwrap();
    assert_eq!(y_col.value(0), items[0].y);
    let node_id_col = batch
        .column(3)
        .as_any()
        .downcast_ref::<UInt32Array>()
        .unwrap();
    assert_eq!(node_id_col.value(0), items[0].node_id);
    let priority_col = batch
        .column(4)
        .as_any()
        .downcast_ref::<UInt16Array>()
        .unwrap();
    assert_eq!(priority_col.value(0), items[0].priority);

    for (i, item) in items.iter().enumerate() {
        assert_eq!(entity_id_col.value(i), item.entity_id.raw());
        assert_eq!(x_col.value(i), item.x);
        assert_eq!(y_col.value(i), item.y);
        assert_eq!(node_id_col.value(i), item.node_id);
        assert_eq!(priority_col.value(i), item.priority);
    }

    // (c) permutation.bin: header, then perm[entity_id[i]] == i for every row, and a
    // never-used entity slot reads the absent sentinel.
    let mut perm_bytes = Vec::new();
    fs::File::open(&perm_path)
        .expect("open permutation.bin")
        .read_to_end(&mut perm_bytes)
        .expect("read permutation.bin");

    assert_eq!(&perm_bytes[0..4], b"TSPM");
    let version = u16::from_le_bytes(perm_bytes[4..6].try_into().unwrap());
    let reserved = u16::from_le_bytes(perm_bytes[6..8].try_into().unwrap());
    let file_bound = u64::from_le_bytes(perm_bytes[8..16].try_into().unwrap());
    assert_eq!(version, 1);
    assert_eq!(reserved, 0);
    assert_eq!(file_bound, bound);

    let slots_bytes = &perm_bytes[16..];
    assert_eq!(slots_bytes.len(), (bound as usize) * 4);
    let slots: Vec<u32> = slots_bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();

    for (row, item) in items.iter().enumerate() {
        assert_eq!(slots[item.entity_id.raw() as usize], row as u32);
    }

    // Test with a bound strictly larger than the entity id space to exercise a genuinely
    // unused slot (all of 0..n are used above, so bound must exceed n for this check).
    let wider_bound = bound + 5;
    let perm_path2 = dir.path().join("permutation-wider.bin");
    write_permutation(&perm_path2, &row_order_entities, wider_bound).expect("write_permutation");
    let mut perm_bytes2 = Vec::new();
    fs::File::open(&perm_path2)
        .expect("open permutation-wider.bin")
        .read_to_end(&mut perm_bytes2)
        .expect("read permutation-wider.bin");
    let slots2_bytes = &perm_bytes2[16..];
    let slots2: Vec<u32> = slots2_bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    for unused_id in bound..wider_bound {
        assert_eq!(slots2[unused_id as usize], 0xFFFF_FFFF);
    }
}

#[test]
fn tiebreak_orders_equal_morton_by_priority_then_entity_id() {
    let extent = unit_extent();
    // Two items with identical coordinates (identical Morton code) but different priority
    // and entity id: must order by (priority, entity_id) per contracts §2.6.
    let mut items = vec![
        TilerItem {
            entity_id: EntityId::new(100),
            x: 0.5,
            y: 0.5,
            node_id: 0,
            priority: 7,
            scalars: vec![],
        },
        TilerItem {
            entity_id: EntityId::new(1),
            x: 0.5,
            y: 0.5,
            node_id: 0,
            priority: 3,
            scalars: vec![],
        },
        TilerItem {
            entity_id: EntityId::new(2),
            x: 0.5,
            y: 0.5,
            node_id: 0,
            priority: 3,
            scalars: vec![],
        },
    ];
    let codes = sort_batch(&mut items, &extent);
    assert_eq!(codes[0], codes[1]);
    assert_eq!(codes[1], codes[2]);
    let ordered: Vec<u64> = items.iter().map(|i| i.entity_id.raw()).collect();
    // priority 3 < priority 7, and within priority 3, entity 1 < entity 2.
    assert_eq!(ordered, vec![1, 2, 100]);
}

#[test]
fn write_permutation_rejects_entity_id_at_or_above_bound() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("permutation.bin");
    let entities = vec![EntityId::new(5)];
    let result = write_permutation(&path, &entities, 5);
    assert!(result.is_err(), "entity id == bound must be rejected");
}

#[test]
fn write_permutation_rejects_entity_id_not_fitting_u32() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("permutation.bin");
    let entities = vec![EntityId::new(1u64 << 32)];
    let result = write_permutation(&path, &entities, 1u64 << 33);
    assert!(result.is_err(), "entity id >= 2^32 must be rejected");
}

#[test]
fn write_permutation_rejects_duplicate_entity_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("permutation.bin");
    // Entity 3 occupies both row 0 and row 2 — must be rejected, not silently overwritten.
    let entities = vec![EntityId::new(3), EntityId::new(4), EntityId::new(3)];
    let result = write_permutation(&path, &entities, 10);
    assert!(
        result.is_err(),
        "a duplicate entity id across rows must be rejected"
    );
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains('3'),
        "error should name the duplicate entity id, got: {message}"
    );
}

#[test]
fn write_segment_scalars_round_trip() {
    use tessera_spatial::tiler::ScalarValue;

    let extent = unit_extent();
    let mut items = vec![
        TilerItem {
            entity_id: EntityId::new(1),
            x: 0.1,
            y: 0.2,
            node_id: 0,
            priority: 1,
            scalars: vec![ScalarValue::U64(42), ScalarValue::Utf8("alpha".to_string())],
        },
        TilerItem {
            entity_id: EntityId::new(2),
            x: 0.8,
            y: 0.9,
            node_id: 1,
            priority: 2,
            scalars: vec![ScalarValue::U64(7), ScalarValue::Utf8("beta".to_string())],
        },
    ];
    let codes = sort_batch(&mut items, &extent);

    let dir = tempfile::tempdir().expect("tempdir");
    let scalar_schema = vec![
        ("count".to_string(), ScalarType::U64),
        ("label".to_string(), ScalarType::Utf8),
    ];
    write_segment(dir.path(), &items, &codes, &scalar_schema).expect("write_segment");

    let file = fs::File::open(dir.path().join("columns.arrow")).expect("open columns.arrow");
    let mut reader = FileReader::try_new(file, None).expect("FileReader::try_new");
    let schema = reader.schema();
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(
        names,
        vec![
            "entity_id",
            "x",
            "y",
            "node_id",
            "priority",
            "count",
            "label"
        ]
    );
    let batch = reader.next().unwrap().unwrap();
    let count_col = batch
        .column(5)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    let label_col = batch
        .column(6)
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .unwrap();
    for (i, item) in items.iter().enumerate() {
        let ScalarValue::U64(v) = &item.scalars[0] else {
            panic!(
                "expected ScalarValue::U64 at scalars[0] for entity {}, got {:?}",
                item.entity_id.raw(),
                item.scalars[0]
            );
        };
        assert_eq!(count_col.value(i), *v);

        let ScalarValue::Utf8(v) = &item.scalars[1] else {
            panic!(
                "expected ScalarValue::Utf8 at scalars[1] for entity {}, got {:?}",
                item.entity_id.raw(),
                item.scalars[1]
            );
        };
        assert_eq!(label_col.value(i), v.as_str());
    }
}
