//! Round-trip test for the tiler + segment writers over the `tessera_id`/`priority` schema
//! (contracts §2.6): sort a batch, write
//! `columns.arrow` / `morton.u32` / `permutation.bin`, and read every byte back.

use tessera_plugin::Plugin;
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

use tessera_spatial::split32;
use tessera_spatial::tiler::{sort_batch, ScalarType, TilerItem};
use tessera_store::manifest::Manifest;
use tessera_store::read::ScalarSlice;
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

/// Quantise a coordinate against the unit extent the way the importer does — the tiler takes
/// fixed point, never a coordinate.
fn q(v: f64) -> u32 {
    tessera_spatial::fixed32(v, 0.0, 1.0)
}

#[test]
fn tiler_and_segment_writers_round_trip() {
    let mut rng = StdRng::seed_from_u64(42);
    let n = 1_000u64;

    let mut items: Vec<TilerItem> = (0..n)
        .map(|entity_id| TilerItem {
            tessera_id: synthetic_tessera_id(entity_id),
            qx: q(rng.gen_range(0.0f64..1.0)),
            qy: q(rng.gen_range(0.0f64..1.0)),
            scalars: vec![],
        })
        .collect();
    let mut entity_ids: Vec<EntityId> = (0..n).map(EntityId::new).collect();

    let codes = sort_batch(&mut items, &mut entity_ids);
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
    assert_eq!(names, vec!["tessera_id", "residual"]);
    assert_eq!(schema.field(0).data_type(), &DataType::UInt64);
    assert_eq!(schema.field(1).data_type(), &DataType::UInt32);

    let batch = reader.next().expect("one batch").expect("batch ok");
    assert!(reader.next().is_none(), "expected exactly one record batch");
    assert_eq!(batch.num_rows(), items.len());

    let tessera_id_col = batch
        .column(0)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    assert_eq!(tessera_id_col.value(0), items[0].tessera_id.raw());
    let residual_col = batch
        .column(1)
        .as_any()
        .downcast_ref::<UInt32Array>()
        .unwrap();
    assert_eq!(residual_col.value(0), split32(items[0].qx, items[0].qy).1);

    // The stored pair is one splitting of one position: the residual at row i and the code at
    // row i are the two halves of `split32` over the same item, so this also pins that
    // `morton.u32` and `columns.arrow` describe the same points.
    for (i, item) in items.iter().enumerate() {
        assert_eq!(tessera_id_col.value(i), item.tessera_id.raw());
        let (cell, residual) = split32(item.qx, item.qy);
        assert_eq!(residual_col.value(i), residual);
        assert_eq!(codes[i], cell.raw());
    }

    // (c) permutation.bin: the paged header, then perm[entity_id[i]] == i for every row, and a
    // never-used entity slot reads the absent sentinel. Read as bytes rather than through
    // `Permutation` because the point is the layout, not the reader's interpretation of it.
    let mut perm_bytes = Vec::new();
    fs::File::open(&perm_path)
        .expect("open permutation.bin")
        .read_to_end(&mut perm_bytes)
        .expect("read permutation.bin");

    assert_eq!(&perm_bytes[0..4], b"TSPM");
    let version = u16::from_le_bytes(perm_bytes[4..6].try_into().unwrap());
    let page_shift = u16::from_le_bytes(perm_bytes[6..8].try_into().unwrap());
    let file_bound = u64::from_le_bytes(perm_bytes[8..16].try_into().unwrap());
    let page_count = u32::from_le_bytes(perm_bytes[16..20].try_into().unwrap());
    let present_count = u32::from_le_bytes(perm_bytes[20..24].try_into().unwrap());
    assert_eq!(version, 2, "the paged form is version 2");
    assert_eq!(page_shift, 16);
    assert_eq!(file_bound, bound);
    // `bound` here is well under one page, and every entity in it has a row.
    assert_eq!(page_count, 1);
    assert_eq!(present_count, 1);

    assert_eq!(
        perm_bytes.len(),
        paged::PAYLOAD_START + paged::PAGE_BYTES,
        "one present page, on a 4 KiB boundary"
    );
    for (row, entity_id) in row_order_entities.iter().enumerate() {
        assert_eq!(paged::slot(&perm_bytes, entity_id.raw()), row as u32);
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
    for unused_id in bound..wider_bound {
        assert_eq!(paged::slot(&perm_bytes2, unused_id), 0xFFFF_FFFF);
    }
}

/// Reading `permutation.bin`'s bytes without going through `Permutation`, for the tests whose
/// subject is the layout itself (contracts §2.6; `tessera_store::permutation` for the diagram).
///
/// Only the single-page case, which is every fixture here: a bound under 2¹⁶ gives one page, so
/// its slot is 0 whenever the page is present at all.
mod paged {
    /// magic, version, page shift, bound, page count, present count — then the directory, then
    /// zero padding to a 4 KiB boundary.
    pub const HEADER_LEN: usize = 24;
    pub const PAGE_ENTRIES: usize = 1 << 16;
    pub const PAGE_BYTES: usize = PAGE_ENTRIES * 4;
    /// Where the payload starts for a one-page directory.
    pub const PAYLOAD_START: usize = 4096;

    /// Entity `entity`'s slot, from a file whose bound is under one page.
    pub fn slot(bytes: &[u8], entity: u64) -> u32 {
        assert_eq!(
            u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            1,
            "this helper reads one-page permutations only"
        );
        assert_eq!(
            u32::from_le_bytes(bytes[HEADER_LEN..HEADER_LEN + 4].try_into().unwrap()),
            0,
            "page 0 must be present at slot 0"
        );
        let at = PAYLOAD_START + entity as usize * 4;
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }
}

#[test]
fn morton_file_is_u32_four_bytes_per_row_and_the_u64_file_is_gone() {
    let mut items: Vec<TilerItem> = (0..1000u64)
        .map(|entity_id| TilerItem {
            tessera_id: synthetic_tessera_id(entity_id),
            qx: q(((entity_id * 7919) % 1000) as f64 / 1000.0),
            qy: q(((entity_id * 104_729) % 1000) as f64 / 1000.0),
            scalars: vec![],
        })
        .collect();
    let mut entity_ids: Vec<EntityId> = (0..1000u64).map(EntityId::new).collect();
    let codes = sort_batch(&mut items, &mut entity_ids);

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
    // Three items at the identical coordinate (identical Morton code): the row order must be
    // exactly ascending `tessera_id`, with no further tiebreak (contracts §2.6 r6).
    let mut items = vec![
        TilerItem {
            tessera_id: TesseraId::new(100),
            qx: q(0.5),
            qy: q(0.5),
            scalars: vec![],
        },
        TilerItem {
            tessera_id: TesseraId::new(1),
            qx: q(0.5),
            qy: q(0.5),
            scalars: vec![],
        },
        TilerItem {
            tessera_id: TesseraId::new(2),
            qx: q(0.5),
            qy: q(0.5),
            scalars: vec![],
        },
    ];
    let mut entity_ids = vec![EntityId::new(100), EntityId::new(1), EntityId::new(2)];
    let codes = sort_batch(&mut items, &mut entity_ids);
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

/// A bound above `2^32` names no entity that could occupy a slot (R1: entity ids fit `u32` in
/// `bundle_format = 1`), and is refused **before the slot array is allocated**.
///
/// **The file assertion is the test.** The bound was always rejected — but by
/// `PermutationWriter::set`, after `create` had already sized the file for `bound` entities and
/// filled it with the row-absent sentinel. A `2^33` bound therefore wrote 32 GB to the temp volume in
/// order to return an error, and an interrupted run left it there: four such files, 68 GB, were
/// recovered from `/tmp` on 2026-08-07. Asserting `is_err()` alone cannot tell the two orderings
/// apart, which is why the earlier version of this test passed for as long as it did.
#[test]
fn write_permutation_rejects_a_bound_above_the_u32_entity_ceiling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("permutation.bin");
    let entities = vec![EntityId::new(1)];
    let result = write_permutation(&path, &entities, (1u64 << 32) + 1);
    assert!(result.is_err(), "a bound above 2^32 must be rejected");
    assert!(
        !path.exists(),
        "an unsatisfiable bound must cost no allocation: {} was created",
        path.display()
    );
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

    let mut items = vec![
        TilerItem {
            tessera_id: TesseraId::new(1),
            qx: q(0.1),
            qy: q(0.2),
            scalars: vec![ScalarValue::U64(42), ScalarValue::Utf8("alpha".to_string())],
        },
        TilerItem {
            tessera_id: TesseraId::new(2),
            qx: q(0.8),
            qy: q(0.9),
            scalars: vec![ScalarValue::U64(7), ScalarValue::Utf8("beta".to_string())],
        },
    ];
    let mut entity_ids = vec![EntityId::new(1), EntityId::new(2)];
    let codes = sort_batch(&mut items, &mut entity_ids);

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
    assert_eq!(names, vec!["tessera_id", "residual", "count", "label"]);
    let batch = reader.next().unwrap().unwrap();
    let count_col = batch
        .column(2)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    let label_col = batch
        .column(3)
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
fn a_pre_residual_columns_file_is_a_typed_error_rather_than_a_misread_position() {
    // The cell-plus-residual change carries **no `bundle_format` bump** — format 1 has never
    // been published — and that is only safe because `validate_schema` compares the fixed
    // columns by name *and* type. This is the test that makes it so: a bundle written before
    // the change carries `x`/`y` `f32` where `residual` now sits, and must fail at open rather
    // than reading a float's bits as a sub-cell position. Without this, an old bundle against a
    // new reader is silent nonsense geometry. (The fixture's `priority` column also makes it a
    // pre-0046 file, which the two-column schema refuses for the same reason.)
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("columns.arrow");
    let schema = Arc::new(Schema::new(vec![
        Field::new("tessera_id", DataType::UInt64, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("priority", DataType::UInt16, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(vec![1u64])) as ArrayRef,
            Arc::new(Float32Array::from(vec![0.5f32])) as ArrayRef,
            Arc::new(Float32Array::from(vec![0.5f32])) as ArrayRef,
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
        "a pre-residual columns.arrow must be a typed reader error, got {err:?}"
    );
}

#[test]
fn a_manifest_without_an_identity_object_is_a_typed_error() {
    // Contracts §2.2 r6: `identity` is required, not defaulted. A bundle read without a key
    // cannot invert a tessera_id, and a *defaulted* key would invert every identifier to the
    // wrong entity -- suppressing the wrong item on /control/changes.
    let json = serde_json::json!({
        "bundle_format": 2,
        "created_at": "2026-07-28T00:00:00Z",
        "data_plugin_hash": tessera_plugin::Passthrough::new().data_plugin_hash(),
        "small_term_threshold": 32,
        "entity_id_high_water": 0,
        "views": [],
        "partitions": [],
        "files": {}
    });
    let err = serde_json::from_value::<Manifest>(json)
        .expect_err("a manifest without `identity` must fail to deserialise, not default it");
    let _ = err; // a typed deserialisation error, not a defaulted/half-read Manifest.
}

#[test]
fn a_view_without_a_quantisation_extent_is_a_typed_error() {
    // Decision 0040: the extent belongs to the view, and there is no bundle-level fallback to
    // read one from. A view whose frame went missing is malformed, not unframed — every position
    // it holds is a fraction of *some* extent, and a reader that guessed one would mis-decode all
    // of them silently. Pre-release there is no older shape to tolerate (decision 0048), so the
    // missing field refuses at open. This is the same rule `projection` beside it keeps.
    let json = serde_json::json!({
        "bundle_format": 4,
        "created_at": "2026-08-30T00:00:00Z",
        "data_plugin_hash": tessera_plugin::Passthrough::new().data_plugin_hash(),
        "vocabularies": [],
        "small_term_threshold": 32,
        "entity_id_high_water": 0,
        "identity": {
            "construction": "siphash-2-4",
            "rounds": 1,
            "key": "0123456789abcdef0123456789abcdef",
            "shard_id": 0,
            "idset": 1
        },
        "views": [{"id": "s0", "display_name": "s0", "projection": "none"}],
        "partitions": [],
        "files": {}
    });
    let err = serde_json::from_value::<Manifest>(json).expect_err(
        "a view without `quantisation` must fail to deserialise, not fall back to a bundle extent",
    );
    assert!(
        err.to_string().contains("quantisation"),
        "the refusal must name the missing field, got: {err}"
    );
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
        let residual: Vec<u32> = (0..rows)
            .map(|i| (i as u32).wrapping_mul(2_654_435_761))
            .collect();

        let via_vecs = dir.path().join(format!("vecs-{rows}.arrow"));
        // No scalar tail: this test is about the two *fixed*-column paths agreeing, and
        // `write_columns` delegates to `write_columns_from_parts` in exactly that case.
        write_columns(&via_vecs, tessera.clone(), residual.clone(), Vec::new())
            .expect("write_columns");
        let vec_bytes = fs::read(&via_vecs).expect("read write_columns output");

        // Heap-backed buffers through the from-parts door.
        let via_parts = dir.path().join(format!("parts-{rows}.arrow"));
        write_columns_from_parts(
            &via_parts,
            Buffer::from_vec(tessera.clone()),
            Buffer::from_vec(residual.clone()),
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
            let r_bytes: Vec<u8> = residual.iter().flat_map(|v| v.to_le_bytes()).collect();
            let via_mmap = dir.path().join(format!("mmap-{rows}.arrow"));
            write_columns_from_parts(
                &via_mmap,
                mmap_buffer(&dir.path().join(format!("t-{rows}.bin")), &t_bytes),
                mmap_buffer(&dir.path().join(format!("r-{rows}.bin")), &r_bytes),
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
        // accepts it. (No `priority` column — decision 0046.)
        let cols = ColumnsRef::load(&via_parts).expect("ColumnsRef must load from_parts output");
        assert_eq!(cols.row_count() as usize, rows);
        assert_eq!(cols.tessera_id(), &tessera[..]);
        assert_eq!(cols.residual(), &residual[..]);
    }
}

#[test]
fn write_columns_from_parts_rejects_short_and_misaligned_buffers_without_panicking() {
    use arrow::buffer::Buffer;
    use tessera_store::write::write_columns_from_parts;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("never-written.arrow");
    let residual = Buffer::from_vec(vec![0u32; 4]);

    // Too short: 3 u64s cannot back 4 rows.
    let err = write_columns_from_parts(&path, Buffer::from_vec(vec![0u64; 3]), residual.clone(), 4)
        .expect_err("a buffer shorter than `rows` values must be a typed error");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    // Misaligned: slicing a u64 buffer at byte 4 moves it off 8-byte alignment. Arrow's own
    // ScalarBuffer conversion would panic here; the writer must fail closed with an error
    // instead.
    let misaligned = Buffer::from_vec(vec![0u64; 5]).slice(4);
    let err = write_columns_from_parts(&path, misaligned, residual, 4)
        .expect_err("a misaligned buffer must be a typed error, not an arrow panic");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("aligned"),
        "error should name the alignment failure: {err}"
    );
}

/// **Scatter order is free, and it produces exactly the bytes the sequential path does.**
///
/// This is the property compaction's pass 1 needs and the old writer could not offer: the fold
/// emits rows in `(morton, tessera_id)` order and learns `perm[entity]` in *that* order, which is
/// not entity order. `write_permutation` remains the sequential producer, and the two must not be
/// allowed to drift — so the assertion is on the whole file, not on a slot.
///
/// **Mutation:** write the slot natively rather than little-endian in `PermutationWriter::set`,
/// or move the header's field order, and these bytes stop matching.
#[test]
fn a_scattered_permutation_is_byte_identical_to_a_sequential_one() {
    use tessera_store::write::PermutationWriter;

    let dir = tempfile::tempdir().expect("tempdir");
    let bound = 64u64;
    // Row order with deliberate gaps: entities 7, 19 and 40 never get a row, so the absent
    // sentinel has to survive in three interior slots rather than only at the tail.
    let row_order: Vec<EntityId> = [3u64, 11, 0, 55, 28, 63, 1, 44]
        .into_iter()
        .map(EntityId::new)
        .collect();

    let sequential = dir.path().join("sequential.bin");
    write_permutation(&sequential, &row_order, bound).expect("the sequential path writes");

    // The same mapping, learned in an order unrelated to either entity or row — which is what a
    // Morton-ordered pass 1 produces.
    let scattered = dir.path().join("scattered.bin");
    let mut writer = PermutationWriter::create(&scattered, bound).expect("create");
    let mut shuffled: Vec<(usize, EntityId)> = row_order.iter().copied().enumerate().collect();
    shuffled.sort_by_key(|(row, entity)| entity.raw().wrapping_mul(7).wrapping_add(*row as u64));
    for (row, entity) in shuffled {
        writer.set(entity, row as u32).expect("set");
    }
    writer.finish().expect("finish");

    assert_eq!(
        fs::read(&sequential).unwrap(),
        fs::read(&scattered).unwrap(),
        "the scattered and sequential producers must write the same permutation.bin byte for byte"
    );
}

/// **An entity that never got a row reads as absent, not as row 0.**
///
/// A freshly extended file reads as zeros and zero is a real row belonging to a real entity, so a
/// writer that skipped the sentinel fill would serve one entity's coordinates under every id that
/// has none — a cross-identity disclosure with no error anywhere. The mapped writer fills
/// `bound × 4` bytes of `0xFF` up front for exactly this reason.
///
/// **Mutation:** delete the payload's `fill(0xFF)` in `PermutationWriter::open` and every gap
/// below resolves to row 0.
#[test]
fn an_entity_with_no_row_is_absent_rather_than_row_zero() {
    use tessera_store::write::PermutationWriter;
    use tessera_store::Permutation;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("permutation.bin");
    let mut writer = PermutationWriter::create(&path, 16).expect("create");
    // Only entity 9 gets a row, and it is row 0 — the value an unfilled slot would masquerade as.
    writer.set(EntityId::new(9), 0).expect("set");
    writer.finish().expect("finish");

    let perm = Permutation::load(&path).expect("the permutation loads");
    assert_eq!(
        perm.row_of(EntityId::new(9)).map(|r| r.raw()),
        Some(0),
        "the one entity with a row keeps it"
    );
    for entity in (0..16u64).filter(|e| *e != 9) {
        assert!(
            perm.row_of(EntityId::new(entity)).is_none(),
            "entity {entity} never got a row and must be absent, not row 0"
        );
    }
}

/// One entity cannot occupy two rows, and the refusal names it — the check that stops a scatter
/// silently overwriting a slot it already filled.
#[test]
fn a_scattered_duplicate_entity_is_refused() {
    use tessera_store::write::PermutationWriter;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("permutation.bin");
    let mut writer = PermutationWriter::create(&path, 16).expect("create");
    writer
        .set(EntityId::new(4), 1)
        .expect("the first set lands");
    let err = writer
        .set(EntityId::new(4), 2)
        .expect_err("a second row for one entity must be refused");
    assert!(
        err.to_string().contains('4'),
        "the refusal must name the entity: {err}"
    );
}

/// **Every declarable width survives a write and a read, including the two that are not flat.**
///
/// The tail is stored and read back positionally, so a type handled in the writer and missed in
/// the reader — or handled in both at different widths — puts every later column's values under
/// the wrong name with the row count still agreeing. Thirteen types is past the number anyone
/// checks by eye, which is why this asserts each one's value rather than only the schema.
///
/// **`bool` is the case worth having.** It is the one member Arrow packs — a bit per row, so the
/// segment writer accumulates a partial byte and flushes it at eight — and the row count here is
/// deliberately **not** a multiple of eight, because a writer that dropped its trailing partial
/// byte would lose up to seven rows' values while `row_count` still matched.
#[test]
fn every_declared_width_round_trips_including_a_packed_bool() {
    use tessera_spatial::tiler::ScalarValue;

    let schema: Vec<(String, ScalarType)> = vec![
        ("flag".into(), ScalarType::Bool),
        ("u8c".into(), ScalarType::U8),
        ("u16c".into(), ScalarType::U16),
        ("u32c".into(), ScalarType::U32),
        ("u64c".into(), ScalarType::U64),
        ("i8c".into(), ScalarType::I8),
        ("i16c".into(), ScalarType::I16),
        ("i32c".into(), ScalarType::I32),
        ("i64c".into(), ScalarType::I64),
        ("f32c".into(), ScalarType::F32),
        ("f64c".into(), ScalarType::F64),
        ("when".into(), ScalarType::TimestampUs),
        ("name".into(), ScalarType::Utf8),
    ];

    // 13 rows: not a multiple of 8, so the bool column ends mid-byte.
    let n = 13u64;
    let scalars_for = |i: u64| {
        vec![
            ScalarValue::Bool(i.is_multiple_of(3)),
            ScalarValue::U8(i as u8),
            ScalarValue::U16(1000 + i as u16),
            ScalarValue::U32(100_000 + i as u32),
            ScalarValue::U64(10_000_000_000 + i),
            // Signed, and negative — the whole reason the narrow signed widths exist.
            ScalarValue::I8(-(i as i8)),
            ScalarValue::I16(-1000 + i as i16),
            ScalarValue::I32(-100_000 + i as i32),
            ScalarValue::I64(-10_000_000_000 + i as i64),
            ScalarValue::F32(i as f32 * 0.5),
            // A value no `f32` holds, so a column that silently narrowed would fail here.
            ScalarValue::F64(1.0 / 3.0 + i as f64),
            ScalarValue::TimestampUs(1_700_000_000_000_000 + i as i64),
            ScalarValue::Utf8(format!("row-{i}")),
        ]
    };

    let mut items: Vec<TilerItem> = (0..n)
        .map(|i| TilerItem {
            tessera_id: synthetic_tessera_id(i),
            qx: q(i as f64 / n as f64),
            qy: q((n - i) as f64 / n as f64),
            scalars: scalars_for(i),
        })
        .collect();
    let mut entity_ids: Vec<EntityId> = (0..n).map(EntityId::new).collect();
    let codes = sort_batch(&mut items, &mut entity_ids);

    let dir = tempfile::tempdir().expect("tempdir");
    write_segment(dir.path(), &items, &codes, &schema).expect("write_segment");

    let cols = ColumnsRef::load(&dir.path().join("columns.arrow")).expect("columns.arrow loads");
    assert_eq!(cols.row_count() as u64, n);

    // Compared against each item's *own* scalars at its post-sort row, because `sort_batch`
    // reorders: asserting against `scalars_for(row)` would pass on a writer that carried values
    // forward unpermuted.
    for (row, item) in items.iter().enumerate() {
        for ((name, ty), expected) in schema.iter().zip(&item.scalars) {
            let view = cols
                .scalar(name)
                .unwrap_or_else(|| panic!("column '{name}'"));
            let got = match (view, ty) {
                (ScalarSlice::Bool(v), ScalarType::Bool) => ScalarValue::Bool(v.value(row)),
                (ScalarSlice::U8(v), ScalarType::U8) => ScalarValue::U8(v[row]),
                (ScalarSlice::U16(v), ScalarType::U16) => ScalarValue::U16(v[row]),
                (ScalarSlice::U32(v), ScalarType::U32) => ScalarValue::U32(v[row]),
                (ScalarSlice::U64(v), ScalarType::U64) => ScalarValue::U64(v[row]),
                (ScalarSlice::I8(v), ScalarType::I8) => ScalarValue::I8(v[row]),
                (ScalarSlice::I16(v), ScalarType::I16) => ScalarValue::I16(v[row]),
                (ScalarSlice::I32(v), ScalarType::I32) => ScalarValue::I32(v[row]),
                (ScalarSlice::I64(v), ScalarType::I64) => ScalarValue::I64(v[row]),
                (ScalarSlice::F32(v), ScalarType::F32) => ScalarValue::F32(v[row]),
                (ScalarSlice::F64(v), ScalarType::F64) => ScalarValue::F64(v[row]),
                (ScalarSlice::TimestampUs(v), ScalarType::TimestampUs) => {
                    ScalarValue::TimestampUs(v[row])
                }
                (ScalarSlice::Utf8(v), ScalarType::Utf8) => {
                    ScalarValue::Utf8(v.value(row).to_string())
                }
                (other, ty) => {
                    panic!(
                        "column '{name}' declared {ty:?} read back as {}",
                        other.type_name()
                    )
                }
            };
            assert_eq!(&got, expected, "column '{name}' at row {row}");
        }
    }

    // Genuinely packed, not a byte per row: 13 bits is two bytes.
    let packed = match cols.scalar("flag").unwrap() {
        ScalarSlice::Bool(a) => a.values().inner().len(),
        other => panic!("flag read back as {}", other.type_name()),
    };
    assert!(
        packed <= 2,
        "13 packed bools should occupy 2 bytes, found {packed} — the column is not bit-packed"
    );
}
