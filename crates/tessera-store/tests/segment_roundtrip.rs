//! Round-trip test for the tiler + segment writers over the `tessera_id`/`priority` schema
//! (contracts §2.6): sort a batch, write
//! `columns.arrow` / `morton.u32` / `permutation.bin`, and read every byte back.

use std::fs;
use std::io::Read;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, Float32Array, UInt16Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::reader::FileReader;
use arrow::ipc::writer::FileWriter;
use arrow::record_batch::RecordBatch;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use tessera_spatial::tiler::{sort_batch, ScalarType, TilerItem};
use tessera_spatial::Extent;
use tessera_store::manifest::Manifest;
use tessera_store::write::{write_permutation, write_segment};
use tessera_store::{ColumnsRef, StoreError};
use tessera_types::{EntityId, TesseraId};

/// A synthetic `tessera_id`-shaped value for test fixtures: full splitmix64 output over a
/// seed, so its top 16 bits are a `priority` prefix like any real `tessera_id` (contracts
/// §2.6), without claiming this is the actual Feistel construction — `tessera-types`'s identity
/// tests cover that separately.
fn synthetic_tessera_id(seed: u64) -> TesseraId {
    let mut z = seed.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^= z >> 31;
    TesseraId::new(z)
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
            tessera_id: synthetic_tessera_id(entity_id),
            x: rng.gen_range(0.0f32..1.0),
            y: rng.gen_range(0.0f32..1.0),
            scalars: vec![],
        })
        .collect();
    let mut entity_ids: Vec<EntityId> = (0..n).map(EntityId::new).collect();

    let extent = unit_extent();
    let codes = sort_batch(&mut items, &mut entity_ids, &extent);
    assert_eq!(codes.len(), items.len());

    let dir = tempfile::tempdir().expect("tempdir");
    write_segment(dir.path(), &items, &codes, &[]).expect("write_segment");

    let row_order_entities: Vec<EntityId> = entity_ids.clone();
    let bound = n; // entity ids are 0..n, contiguous
    let perm_path = dir.path().join("permutation.bin");
    write_permutation(&perm_path, &row_order_entities, bound).expect("write_permutation");

    // (a) morton.u32: non-decreasing, equal to the codes sort_batch returned.
    let morton_bytes = fs::read(dir.path().join("morton.u32")).expect("read morton.u32");
    assert_eq!(morton_bytes.len(), items.len() * 4);
    let file_codes: Vec<u32> = morton_bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(file_codes, codes);
    assert!(file_codes.windows(2).all(|w| w[0] <= w[1]));

    // (b) columns.arrow: schema field names/types match contracts §2.6 r6; row 0 matches
    // items[0] post-sort.
    let file = fs::File::open(dir.path().join("columns.arrow")).expect("open columns.arrow");
    let mut reader = FileReader::try_new(file, None).expect("FileReader::try_new");
    let schema = reader.schema();
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, vec!["tessera_id", "x", "y", "priority"]);
    assert_eq!(schema.field(0).data_type(), &DataType::UInt64);
    assert_eq!(schema.field(1).data_type(), &DataType::Float32);
    assert_eq!(schema.field(2).data_type(), &DataType::Float32);
    assert_eq!(schema.field(3).data_type(), &DataType::UInt16);

    let batch = reader.next().expect("one batch").expect("batch ok");
    assert!(reader.next().is_none(), "expected exactly one record batch");
    assert_eq!(batch.num_rows(), items.len());

    let tessera_id_col = batch
        .column(0)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    assert_eq!(tessera_id_col.value(0), items[0].tessera_id.raw());
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
    let priority_col = batch
        .column(3)
        .as_any()
        .downcast_ref::<UInt16Array>()
        .unwrap();
    assert_eq!(priority_col.value(0), items[0].tessera_id.priority());

    for (i, item) in items.iter().enumerate() {
        assert_eq!(tessera_id_col.value(i), item.tessera_id.raw());
        assert_eq!(x_col.value(i), item.x);
        assert_eq!(y_col.value(i), item.y);
        assert_eq!(priority_col.value(i), item.tessera_id.priority());
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

    for (row, entity_id) in row_order_entities.iter().enumerate() {
        assert_eq!(slots[entity_id.raw() as usize], row as u32);
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
fn morton_file_is_u32_four_bytes_per_row_and_the_u64_file_is_gone() {
    let extent = unit_extent();
    let mut items: Vec<TilerItem> = (0..1000u64)
        .map(|entity_id| TilerItem {
            tessera_id: synthetic_tessera_id(entity_id),
            x: ((entity_id * 7919) % 1000) as f32 / 1000.0,
            y: ((entity_id * 104_729) % 1000) as f32 / 1000.0,
            scalars: vec![],
        })
        .collect();
    let mut entity_ids: Vec<EntityId> = (0..1000u64).map(EntityId::new).collect();
    let codes = sort_batch(&mut items, &mut entity_ids, &extent);

    let dir = tempfile::tempdir().expect("tempdir");
    write_segment(dir.path(), &items, &codes, &[]).expect("write_segment");

    assert!(
        !dir.path().join("morton.u64").exists(),
        "the u64 file must not be written any more (contracts r5)"
    );

    let bytes = fs::read(dir.path().join("morton.u32")).expect("read morton.u32");
    assert_eq!(bytes.len(), items.len() * 4, "4 bytes per row, no header");

    let read_back: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(read_back, codes, "bytes must round-trip sort_batch's codes");
    assert!(read_back.windows(2).all(|w| w[0] <= w[1]));
}

#[test]
fn tiebreak_orders_equal_morton_by_tessera_id() {
    let extent = unit_extent();
    // Three items at the identical coordinate (identical Morton code): the row order must be
    // exactly ascending `tessera_id`, with no further tiebreak (contracts §2.6 r6).
    let mut items = vec![
        TilerItem {
            tessera_id: TesseraId::new(100),
            x: 0.5,
            y: 0.5,
            scalars: vec![],
        },
        TilerItem {
            tessera_id: TesseraId::new(1),
            x: 0.5,
            y: 0.5,
            scalars: vec![],
        },
        TilerItem {
            tessera_id: TesseraId::new(2),
            x: 0.5,
            y: 0.5,
            scalars: vec![],
        },
    ];
    let mut entity_ids = vec![EntityId::new(100), EntityId::new(1), EntityId::new(2)];
    let codes = sort_batch(&mut items, &mut entity_ids, &extent);
    assert_eq!(codes[0], codes[1]);
    assert_eq!(codes[1], codes[2]);
    let ordered: Vec<u64> = items.iter().map(|i| i.tessera_id.raw()).collect();
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
            tessera_id: TesseraId::new(1),
            x: 0.1,
            y: 0.2,
            scalars: vec![ScalarValue::U64(42), ScalarValue::Utf8("alpha".to_string())],
        },
        TilerItem {
            tessera_id: TesseraId::new(2),
            x: 0.8,
            y: 0.9,
            scalars: vec![ScalarValue::U64(7), ScalarValue::Utf8("beta".to_string())],
        },
    ];
    let mut entity_ids = vec![EntityId::new(1), EntityId::new(2)];
    let codes = sort_batch(&mut items, &mut entity_ids, &extent);

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
        vec!["tessera_id", "x", "y", "priority", "count", "label"]
    );
    let batch = reader.next().unwrap().unwrap();
    let count_col = batch
        .column(4)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    let label_col = batch
        .column(5)
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .unwrap();
    for (i, item) in items.iter().enumerate() {
        let ScalarValue::U64(v) = &item.scalars[0] else {
            panic!(
                "expected ScalarValue::U64 at scalars[0] for tessera_id {}, got {:?}",
                item.tessera_id.raw(),
                item.scalars[0]
            );
        };
        assert_eq!(count_col.value(i), *v);

        let ScalarValue::Utf8(v) = &item.scalars[1] else {
            panic!(
                "expected ScalarValue::Utf8 at scalars[1] for tessera_id {}, got {:?}",
                item.tessera_id.raw(),
                item.scalars[1]
            );
        };
        assert_eq!(label_col.value(i), v.as_str());
    }
}

#[test]
fn a_pre_r6_columns_file_is_a_typed_error_not_a_half_read() {
    // Contracts §2.6 r6: renaming the identity column is what makes an old bundle fail
    // closed. Write a five-column pre-r6 schema by hand and assert the reader rejects
    // it by name, rather than reading `entity_id`'s bytes as `tessera_id` -- which
    // would silently publish entity IDs on the wire, the one thing I10 forbids.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("columns.arrow");
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("node_id", DataType::UInt32, false),
        Field::new("priority", DataType::UInt16, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(vec![1u64])) as ArrayRef,
            Arc::new(Float32Array::from(vec![0.5f32])) as ArrayRef,
            Arc::new(Float32Array::from(vec![0.5f32])) as ArrayRef,
            Arc::new(UInt32Array::from(vec![0xFFFF_FFFFu32])) as ArrayRef,
            Arc::new(UInt16Array::from(vec![0u16])) as ArrayRef,
        ],
    )
    .expect("build batch");

    let file = fs::File::create(&path).expect("create columns.arrow");
    let mut writer = FileWriter::try_new(file, &schema).expect("FileWriter::try_new");
    writer.write(&batch).expect("write batch");
    writer.finish().expect("finish");

    let err = ColumnsRef::load(&path).unwrap_err();
    assert!(
        matches!(err, StoreError::InvalidColumns { .. }),
        "a pre-r6 columns.arrow must be a typed reader error, got {err:?}"
    );
}

#[test]
fn a_manifest_without_an_identity_object_is_a_typed_error() {
    // Contracts §2.2 r6: `identity` is required, not defaulted. A bundle read without a key
    // cannot invert a tessera_id, and a *defaulted* key would invert every identifier to the
    // wrong entity -- suppressing the wrong item on /control/changes.
    let json = serde_json::json!({
        "bundle_format": 1,
        "created_at": "2026-07-28T00:00:00Z",
        "data_plugin_hash": "builtin:passthrough:1",
        "small_term_threshold": 32,
        "quantisation": {"x_min": 0.0, "x_max": 1.0, "y_min": 0.0, "y_max": 1.0},
        "entity_id_high_water": 0,
        "slices": [],
        "partitions": [],
        "files": {}
    });
    let err = serde_json::from_value::<Manifest>(json)
        .expect_err("a manifest without `identity` must fail to deserialise, not default it");
    let _ = err; // a typed deserialisation error, not a defaulted/half-read Manifest.
}

/// Write `bytes` to `path`, mmap it, and wrap the mapping as an arrow `Buffer` without copying
/// — the exact handover shape `write_columns_from_parts` exists for (a file-backed column the
/// build pipeline spilled, mapped page-aligned at offset 0).
fn mmap_buffer(path: &std::path::Path, bytes: &[u8]) -> arrow::buffer::Buffer {
    fs::write(path, bytes).expect("write scratch column file");
    let file = fs::File::open(path).expect("open scratch column file");
    // SAFETY: the mapping is read-only and lives inside the Arc the Buffer captures as its
    // allocation, so it outlives every view of it; nothing writes the file after this.
    let mmap = unsafe { memmap2::Mmap::map(&file) }.expect("mmap scratch column file");
    let len = mmap.len();
    let arc = Arc::new(mmap);
    let ptr = std::ptr::NonNull::new(arc.as_ptr() as *mut u8).expect("mmap base is non-null");
    unsafe { arrow::buffer::Buffer::from_custom_allocation(ptr, len, arc) }
}

#[test]
fn write_columns_from_parts_matches_write_columns_byte_for_byte() {
    use arrow::buffer::Buffer;
    use tessera_store::write::{write_columns, write_columns_from_parts};

    let dir = tempfile::tempdir().expect("tempdir");
    for rows in [0usize, 1, 1000] {
        let tessera: Vec<u64> = (0..rows as u64)
            .map(|i| synthetic_tessera_id(i).raw())
            .collect();
        let x: Vec<f32> = (0..rows).map(|i| i as f32 * 0.25).collect();
        let y: Vec<f32> = (0..rows).map(|i| 1.0 - i as f32 * 0.125).collect();

        let via_vecs = dir.path().join(format!("vecs-{rows}.arrow"));
        write_columns(&via_vecs, tessera.clone(), x.clone(), y.clone()).expect("write_columns");
        let vec_bytes = fs::read(&via_vecs).expect("read write_columns output");

        // Heap-backed buffers through the from-parts door.
        let via_parts = dir.path().join(format!("parts-{rows}.arrow"));
        write_columns_from_parts(
            &via_parts,
            Buffer::from_vec(tessera.clone()),
            Buffer::from_vec(x.clone()),
            Buffer::from_vec(y.clone()),
            rows,
        )
        .expect("write_columns_from_parts (heap buffers)");
        assert_eq!(
            fs::read(&via_parts).expect("read from_parts output"),
            vec_bytes,
            "{rows} rows: heap-buffer from_parts output must be byte-identical"
        );

        // Mmap-backed buffers — the 48 GB-avoidance case this function exists for. (Skipped
        // at zero rows: Linux refuses to mmap an empty file, and an empty column has nothing
        // to spill anyway — the heap-buffer case above covers rows == 0.)
        if rows > 0 {
            let t_bytes: Vec<u8> = tessera.iter().flat_map(|v| v.to_le_bytes()).collect();
            let x_bytes: Vec<u8> = x.iter().flat_map(|v| v.to_le_bytes()).collect();
            let y_bytes: Vec<u8> = y.iter().flat_map(|v| v.to_le_bytes()).collect();
            let via_mmap = dir.path().join(format!("mmap-{rows}.arrow"));
            write_columns_from_parts(
                &via_mmap,
                mmap_buffer(&dir.path().join(format!("t-{rows}.bin")), &t_bytes),
                mmap_buffer(&dir.path().join(format!("x-{rows}.bin")), &x_bytes),
                mmap_buffer(&dir.path().join(format!("y-{rows}.bin")), &y_bytes),
                rows,
            )
            .expect("write_columns_from_parts (mmap buffers)");
            assert_eq!(
                fs::read(&via_mmap).expect("read mmap-backed output"),
                vec_bytes,
                "{rows} rows: mmap-backed from_parts output must be byte-identical"
            );
        }

        // And the strict reader (one batch, uncompressed, aligned, fixed schema, no nulls)
        // accepts it, with `priority` derived from `tessera_id` exactly as contracts §2.6 r6
        // defines it.
        let cols = ColumnsRef::load(&via_parts).expect("ColumnsRef must load from_parts output");
        assert_eq!(cols.row_count() as usize, rows);
        assert_eq!(cols.tessera_id(), &tessera[..]);
        assert_eq!(cols.x(), &x[..]);
        assert_eq!(cols.y(), &y[..]);
        let expected_priority: Vec<u16> = tessera
            .iter()
            .map(|&id| TesseraId::new(id).priority())
            .collect();
        assert_eq!(cols.priority(), &expected_priority[..]);
    }
}

#[test]
fn write_columns_from_parts_rejects_short_and_misaligned_buffers_without_panicking() {
    use arrow::buffer::Buffer;
    use tessera_store::write::write_columns_from_parts;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("never-written.arrow");
    let x = Buffer::from_vec(vec![0f32; 4]);
    let y = Buffer::from_vec(vec![0f32; 4]);

    // Too short: 3 u64s cannot back 4 rows.
    let err = write_columns_from_parts(
        &path,
        Buffer::from_vec(vec![0u64; 3]),
        x.clone(),
        y.clone(),
        4,
    )
    .expect_err("a buffer shorter than `rows` values must be a typed error");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    // Misaligned: slicing a u64 buffer at byte 4 moves it off 8-byte alignment. Arrow's own
    // ScalarBuffer conversion would panic here; the writer must fail closed with an error
    // instead.
    let misaligned = Buffer::from_vec(vec![0u64; 5]).slice(4);
    let err = write_columns_from_parts(&path, misaligned, x, y, 4)
        .expect_err("a misaligned buffer must be a typed error, not an arrow panic");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("aligned"),
        "error should name the alignment failure: {err}"
    );
}
