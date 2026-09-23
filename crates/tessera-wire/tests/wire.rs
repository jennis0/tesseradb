//! The frames `tessera-wire` writes, read back the way a client reads them.

use arrow::array::{
    Array, BooleanArray, DictionaryArray, ListArray, StringArray, UInt32Array, UInt64Array,
};
use arrow::datatypes::{DataType, TimeUnit, UInt16Type};
use arrow::ipc::reader::StreamReader;
use arrow::record_batch::RecordBatch;
use tessera_wire::{
    artifacts_frame, artifacts_identity_frame, records_head_frame, page_end_frame, points_frame,
    points_highlight_frame, records_frame, split_frames, sub_cells_frame, tiles_frame,
    trailer_frame, ArtifactRow, FrameError, RecordsCompression, ScalarColumn, FRAME_ARTIFACTS,
    FRAME_HEADER_BYTES, FRAME_RECORDS_HEAD, FRAME_PAGE_END, FRAME_POINTS, FRAME_RECORDS,
    FRAME_SUB_CELLS, FRAME_TILES, FRAME_TRAILER,
};

/// The batches of one Arrow payload.
fn batches(payload: &[u8]) -> Vec<RecordBatch> {
    StreamReader::try_new(payload, None)
        .expect("an arrow stream")
        .map(|batch| batch.expect("a batch decodes"))
        .collect()
}

/// The one batch of a single frame of `kind`.
fn batch_of(frame: &[u8], kind: u8) -> RecordBatch {
    let frames = split_frames(frame).expect("a well-formed frame");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, kind);
    let mut batches = batches(frames[0].1);
    assert_eq!(batches.len(), 1);
    batches.remove(0)
}

fn names(batch: &RecordBatch) -> Vec<String> {
    batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect()
}

fn column<'a, A: Array + 'static>(batch: &'a RecordBatch, name: &str) -> &'a A {
    batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("no column {name}"))
        .as_any()
        .downcast_ref::<A>()
        .unwrap_or_else(|| panic!("{name} is not the expected array type"))
}

fn u64s(batch: &RecordBatch, name: &str) -> Vec<Option<u64>> {
    column::<UInt64Array>(batch, name).iter().collect()
}

fn bools(batch: &RecordBatch, name: &str) -> Vec<Option<bool>> {
    column::<BooleanArray>(batch, name).iter().collect()
}

fn nullable(batch: &RecordBatch, name: &str) -> bool {
    batch.schema().field_with_name(name).unwrap().is_nullable()
}

fn row(layer: &str, tessera_id: u64) -> ArtifactRow<'_> {
    ArtifactRow {
        layer,
        tessera_id,
        ..Default::default()
    }
}

/// A well-formed body in contract order, with the points in two frames.
fn build_body() -> Vec<u8> {
    let mut body = tiles_frame(&[30, 31], &[10, 5], &[9, 4], &[2, 1], &[8, 3]);
    body.extend(points_frame(
        &[0, 1],
        &[1, 2],
        &[("count", ScalarColumn::U64(&[70, 80]))],
        None,
        &[],
    ));
    body.extend(points_frame(
        &[2],
        &[3],
        &[("count", ScalarColumn::U64(&[90]))],
        None,
        &[],
    ));
    body.extend(trailer_frame(br#"{"points":3}"#));
    body
}

/// The TypeScript and Python clients read the header independently, so its bytes are pinned here
/// without going through `split_frames`.
#[test]
fn the_frame_header_is_a_kind_byte_then_a_little_endian_length() {
    let tiles = tiles_frame(&[30, 31], &[10, 5], &[10, 5], &[2, 1], &[10, 5]);
    let mut body = tiles.clone();
    body.extend(trailer_frame(b"{}"));

    assert_eq!(FRAME_HEADER_BYTES, 5);
    let payload_len = u32::try_from(tiles.len() - FRAME_HEADER_BYTES).unwrap();
    assert_ne!(
        payload_len.to_le_bytes(),
        payload_len.to_be_bytes(),
        "the fixture must tell the byte orders apart"
    );
    assert_eq!(body[0], FRAME_TILES);
    assert_eq!(body[1..5], payload_len.to_le_bytes());
    assert_eq!(body[tiles.len()], FRAME_TRAILER);
    assert_eq!(body[tiles.len() + 1..tiles.len() + 5], 2u32.to_le_bytes());
    assert_eq!(&body[tiles.len() + 5..], b"{}");
}

#[test]
fn a_body_splits_into_frames_that_each_decode_alone() {
    let body = build_body();
    let frames = split_frames(&body).unwrap();
    let kinds: Vec<u8> = frames.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(
        kinds,
        [FRAME_TILES, FRAME_POINTS, FRAME_POINTS, FRAME_TRAILER]
    );
    assert_eq!(frames[3].1, br#"{"points":3}"#);

    let tiles = batches(frames[0].1);
    assert_eq!(tiles.len(), 1);
    assert_eq!(
        names(&tiles[0]),
        ["tile", "visible", "matched", "served", "highlighted"]
    );
    for (name, want) in [
        ("tile", [30, 31]),
        ("visible", [10, 5]),
        ("matched", [9, 4]),
        ("served", [2, 1]),
        ("highlighted", [8, 3]),
    ] {
        assert_eq!(column::<UInt64Array>(&tiles[0], name).values(), &want);
    }

    let mut ids = Vec::new();
    let mut counts = Vec::new();
    for (_, payload) in &frames[1..3] {
        for batch in batches(payload) {
            assert_eq!(names(&batch), ["tessera_id", "code", "count"]);
            ids.extend(u64s(&batch, "tessera_id"));
            counts.extend(u64s(&batch, "count"));
        }
    }
    assert_eq!(ids, [Some(0), Some(1), Some(2)]);
    assert_eq!(counts, [Some(70), Some(80), Some(90)]);
}

#[test]
fn split_refuses_truncation_and_unknown_kinds() {
    let tiles = tiles_frame(&[1], &[1], &[1], &[1], &[1]);
    assert_eq!(
        split_frames(&tiles[..tiles.len() - 1]),
        Err(FrameError::TruncatedPayload { at: 0 })
    );
    assert_eq!(
        split_frames(&[FRAME_TILES]),
        Err(FrameError::TruncatedHeader { at: 0 })
    );
    let mut body = tiles.clone();
    body.push(9);
    body.extend(0u32.to_le_bytes());
    assert_eq!(
        split_frames(&body),
        Err(FrameError::UnknownKind {
            kind: 9,
            at: tiles.len()
        })
    );
}

/// A cut on a frame boundary splits cleanly, and the missing trailer is then what tells the
/// consumer the body is short.
#[test]
fn a_strict_prefix_of_a_body_never_ends_in_a_trailer() {
    let body = build_body();
    for cut in 1..body.len() {
        if let Ok(frames) = split_frames(&body[..cut]) {
            assert_ne!(
                frames.last().map(|(kind, _)| *kind),
                Some(FRAME_TRAILER),
                "cut at {cut}"
            );
        }
    }
}

#[test]
fn every_scalar_type_arrives_as_its_arrow_type() {
    let text = ["a".to_string(), String::new()];
    let scalars = [
        ("bool", ScalarColumn::Bool(&[true, false]), DataType::Boolean),
        ("u8", ScalarColumn::U8(&[1, 2]), DataType::UInt8),
        ("u16", ScalarColumn::U16(&[1, 2]), DataType::UInt16),
        ("u32", ScalarColumn::U32(&[1, 2]), DataType::UInt32),
        ("u64", ScalarColumn::U64(&[1, 2]), DataType::UInt64),
        ("i8", ScalarColumn::I8(&[-1, 2]), DataType::Int8),
        ("i16", ScalarColumn::I16(&[-1, 2]), DataType::Int16),
        ("i32", ScalarColumn::I32(&[-1, 2]), DataType::Int32),
        ("i64", ScalarColumn::I64(&[-1, 2]), DataType::Int64),
        ("f32", ScalarColumn::F32(&[0.5, 2.0]), DataType::Float32),
        ("f64", ScalarColumn::F64(&[0.5, 2.0]), DataType::Float64),
        (
            "time",
            ScalarColumn::TimestampUs(&[-1, 2]),
            DataType::Timestamp(TimeUnit::Microsecond, None),
        ),
        ("text", ScalarColumn::Utf8(&text), DataType::Utf8),
    ];
    let mut want = Vec::new();
    let mut columns = Vec::new();
    for (name, scalar, data_type) in scalars {
        want.push((name, data_type));
        columns.push((name, scalar));
    }
    let batch = batch_of(&points_frame(&[1, 2], &[3, 4], &columns, None, &[]), FRAME_POINTS);
    let schema = batch.schema();
    for (at, (name, data_type)) in want.iter().enumerate() {
        let field = schema.field(at + 2);
        assert_eq!(field.name(), name);
        assert_eq!(field.data_type(), data_type);
        assert!(!field.is_nullable());
    }
    let shown = |name: &str| -> Vec<String> {
        let column = batch.column_by_name(name).unwrap();
        (0..column.len())
            .map(|row| arrow::util::display::array_value_to_string(column, row).unwrap())
            .collect()
    };
    assert_eq!(shown("bool"), ["true", "false"]);
    assert_eq!(shown("i8"), ["-1", "2"]);
    assert_eq!(shown("u64"), ["1", "2"]);
    assert_eq!(shown("f32"), ["0.5", "2.0"]);
    assert_eq!(shown("text"), ["a", ""]);
    assert_eq!(
        column::<arrow::array::TimestampMicrosecondArray>(&batch, "time").values(),
        &[-1, 2]
    );
}

#[test]
fn highlighted_follows_the_scalars_and_membership_follows_it() {
    let ids = [1u64, 2, 3];
    let scalars = [("w", ScalarColumn::U16(&[7, 8, 9]))];
    let a = [Some(100), None, Some(300)];
    let b = [None, None, Some(999)];

    let plain = batch_of(&points_frame(&ids, &ids, &scalars, None, &[]), FRAME_POINTS);
    assert_eq!(names(&plain), ["tessera_id", "code", "w"]);

    let frame = points_frame(
        &ids,
        &ids,
        &scalars,
        Some(&[true, false, true]),
        &[("clusters/hdbscan", &a), ("regions/admin", &b)],
    );
    let batch = batch_of(&frame, FRAME_POINTS);
    assert_eq!(
        names(&batch),
        [
            "tessera_id",
            "code",
            "w",
            "highlighted",
            "membership:clusters/hdbscan",
            "membership:regions/admin"
        ]
    );
    assert!(!nullable(&batch, "highlighted"));
    assert_eq!(
        bools(&batch, "highlighted"),
        [Some(true), Some(false), Some(true)]
    );
    assert!(nullable(&batch, "membership:clusters/hdbscan"));
    assert_eq!(u64s(&batch, "membership:clusters/hdbscan"), a);
    assert_eq!(u64s(&batch, "membership:regions/admin"), b);
}

#[test]
fn the_highlight_projection_is_the_identifier_and_the_bit() {
    let batch = batch_of(
        &points_highlight_frame(&[5, 6], &[false, true]),
        FRAME_POINTS,
    );
    assert_eq!(names(&batch), ["tessera_id", "highlighted"]);
    assert_eq!(u64s(&batch, "tessera_id"), [Some(5), Some(6)]);
    assert_eq!(bools(&batch, "highlighted"), [Some(false), Some(true)]);
    assert!(!nullable(&batch, "highlighted"));
}

/// An underlay that was asked for and is empty is a frame with a schema and no rows.
#[test]
fn the_sub_cells_frame_is_cell_and_count_even_when_empty() {
    let batch = batch_of(&sub_cells_frame(&[100, 101], &[5, 3]), FRAME_SUB_CELLS);
    assert_eq!(names(&batch), ["cell", "count"]);
    assert_eq!(u64s(&batch, "cell"), [Some(100), Some(101)]);
    assert_eq!(u64s(&batch, "count"), [Some(5), Some(3)]);

    let frames_of_empty = sub_cells_frame(&[], &[]);
    let frames = split_frames(&frames_of_empty).unwrap();
    assert_eq!(frames[0].0, FRAME_SUB_CELLS);
    let mut reader = StreamReader::try_new(frames[0].1, None).unwrap();
    assert_eq!(reader.schema().fields().len(), 2);
    assert_eq!(reader.by_ref().map(|b| b.unwrap().num_rows()).sum::<usize>(), 0);
}

/// The dictionary-decoded `layer` of every row.
fn layers(batch: &RecordBatch) -> Vec<String> {
    let column = column::<DictionaryArray<UInt16Type>>(batch, "layer");
    let values = column
        .values()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    (0..column.len())
        .map(|row| values.value(column.key(row).unwrap()).to_string())
        .collect()
}

#[test]
fn the_artifacts_frame_has_sixteen_fixed_columns_then_the_shape() {
    let content = ["label".to_string(), "summary".to_string()];
    let shape = vec![vec![vec![[1u32, 2], [3, 4], [5, 6]]]];
    let full = ArtifactRow {
        layer: "clusters/a",
        tessera_id: 7,
        key: Some("k7"),
        masked_count: 12,
        centroid: Some([1.5, 2.5]),
        bbox: Some([1, 2, 3, 4]),
        shape: None,
        content: &content,
        parent_ids: vec![3, 4],
        rung: 3,
        matched: Some(true),
        highlighted: Some(false),
        target: None,
    };
    let rows = [full.clone(), row("regions/b", 8), row("clusters/a", 9)];
    let batch = batch_of(&artifacts_frame(&rows), FRAME_ARTIFACTS);
    let fixed = [
        "layer",
        "tessera_id",
        "key",
        "masked_count",
        "centroid_x",
        "centroid_y",
        "box_min_x",
        "box_min_y",
        "box_max_x",
        "box_max_y",
        "content",
        "parent_ids",
        "rung",
        "matched",
        "highlighted",
        "target",
    ];
    assert_eq!(names(&batch), fixed, "no row has a shape");
    assert_eq!(layers(&batch), ["clusters/a", "regions/b", "clusters/a"]);
    assert_eq!(u64s(&batch, "masked_count"), [Some(12), Some(0), Some(0)]);
    let key = column::<StringArray>(&batch, "key");
    assert_eq!(key.iter().collect::<Vec<_>>(), [Some("k7"), None, None]);
    let centroid_y = column::<arrow::array::Float64Array>(&batch, "centroid_y");
    assert_eq!(centroid_y.iter().collect::<Vec<_>>(), [Some(2.5), None, None]);
    let box_max_x = column::<UInt32Array>(&batch, "box_max_x");
    assert_eq!(box_max_x.iter().collect::<Vec<_>>(), [Some(3), None, None]);
    assert_eq!(column::<UInt32Array>(&batch, "rung").values(), &[3, 0, 0]);

    let lists = |name: &str| -> Vec<String> {
        let column = batch.column_by_name(name).unwrap();
        assert!(!nullable(&batch, name) && column.null_count() == 0);
        (0..column.len())
            .map(|row| arrow::util::display::array_value_to_string(column, row).unwrap())
            .collect()
    };
    assert_eq!(lists("content"), ["[label, summary]", "[]", "[]"]);
    assert_eq!(lists("parent_ids"), ["[3, 4]", "[]", "[]"]);

    let shaped = [
        ArtifactRow {
            shape: Some(&shape),
            ..full
        },
        row("regions/b", 8),
    ];
    let mut with_shape: Vec<&str> = fixed.to_vec();
    with_shape.extend(["shape_x", "shape_y"]);
    assert_eq!(
        names(&batch_of(&artifacts_frame(&shaped), FRAME_ARTIFACTS)),
        with_shape
    );
}

/// A null answers a request that asked no such question, which `false` would not.
#[test]
fn matched_highlighted_and_target_are_null_when_nothing_was_asked() {
    let rows = [
        ArtifactRow {
            matched: Some(true),
            highlighted: Some(false),
            ..row("clusters/a", 7)
        },
        ArtifactRow {
            matched: Some(false),
            ..row("clusters/a", 8)
        },
        ArtifactRow {
            target: Some(8),
            ..row("labels/a", 9)
        },
    ];
    for frame in [artifacts_frame(&rows), artifacts_identity_frame(&rows)] {
        let batch = batch_of(&frame, FRAME_ARTIFACTS);
        assert!(nullable(&batch, "matched") && nullable(&batch, "highlighted"));
        assert_eq!(bools(&batch, "matched"), [Some(true), Some(false), None]);
        assert_eq!(bools(&batch, "highlighted"), [Some(false), None, None]);
    }
    let batch = batch_of(&artifacts_frame(&rows), FRAME_ARTIFACTS);
    assert!(nullable(&batch, "target"));
    assert_eq!(u64s(&batch, "target"), [None, None, Some(8)]);
}

#[test]
fn the_identity_projection_is_five_columns_whatever_the_rows_hold() {
    let shape = vec![vec![vec![[1u32, 2], [3, 4], [5, 6]]]];
    let rows = [
        ArtifactRow {
            rung: 2,
            shape: Some(&shape),
            target: Some(8),
            ..row("clusters/a", 7)
        },
        row("regions/b", 8),
    ];
    let batch = batch_of(&artifacts_identity_frame(&rows), FRAME_ARTIFACTS);
    assert_eq!(
        names(&batch),
        ["layer", "tessera_id", "rung", "matched", "highlighted"]
    );
    assert_eq!(layers(&batch), ["clusters/a", "regions/b"]);
    assert_eq!(u64s(&batch, "tessera_id"), [Some(7), Some(8)]);
    assert_eq!(column::<UInt32Array>(&batch, "rung").values(), &[2, 0]);
}

/// A shape is parts of rings of vertices, three lists deep, so a second part cannot be read as a
/// hole of the first.
#[test]
fn a_shape_travels_as_parts_of_rings() {
    let two_parts = vec![
        vec![
            vec![[1u32, 2], [3, 4], [5, 6]],
            vec![[70, 80], [90, 100], [110, 120], [130, 140]],
        ],
        vec![vec![[7, 1], [8, 1], [9, 2]]],
    ];
    let rows = [
        ArtifactRow {
            shape: Some(&two_parts),
            ..row("clusters/a", 7)
        },
        row("clusters/a", 8),
    ];
    let batch = batch_of(&artifacts_frame(&rows), FRAME_ARTIFACTS);

    fn list(array: &dyn Array) -> &ListArray {
        array.as_any().downcast_ref::<ListArray>().expect("a list")
    }
    let axis = |name: &str| -> Vec<Option<Vec<Vec<Vec<u32>>>>> {
        let shapes = column::<ListArray>(&batch, name);
        (0..shapes.len())
            .map(|row| {
                shapes.is_valid(row).then(|| {
                    let parts = shapes.value(row);
                    list(&parts)
                        .iter()
                        .map(|rings| {
                            list(&rings.expect("a part is never null"))
                                .iter()
                                .map(|vertices| {
                                    vertices
                                        .expect("a ring is never null")
                                        .as_any()
                                        .downcast_ref::<UInt32Array>()
                                        .expect("vertices are u32")
                                        .values()
                                        .to_vec()
                                })
                                .collect()
                        })
                        .collect()
                })
            })
            .collect()
    };
    assert_eq!(
        axis("shape_x"),
        [
            Some(vec![
                vec![vec![1, 3, 5], vec![70, 90, 110, 130]],
                vec![vec![7, 8, 9]]
            ]),
            None
        ]
    );
    assert_eq!(
        axis("shape_y"),
        [
            Some(vec![
                vec![vec![2, 4, 6], vec![80, 100, 120, 140]],
                vec![vec![1, 1, 2]]
            ]),
            None
        ]
    );
}

/// Loose ceilings on bytes per row, so neither projection grows unnoticed.
#[test]
fn artifact_rows_stay_within_their_size_bounds() {
    const ROWS: usize = 100_000;
    let keys: Vec<String> = (0..ROWS).map(|i| format!("key-{i:07}")).collect();
    let content: Vec<Vec<String>> = (0..ROWS).map(|i| vec![format!("label {i}")]).collect();
    let rows: Vec<ArtifactRow<'_>> = (0..ROWS)
        .map(|i| ArtifactRow {
            layer: "clusters/hdbsca",
            tessera_id: i as u64,
            key: Some(&keys[i]),
            masked_count: (i % 1000) as u64,
            centroid: Some([i as f64, (i * 2) as f64]),
            bbox: Some([i as u32, i as u32, i as u32 + 5, i as u32 + 5]),
            shape: None,
            content: &content[i],
            parent_ids: if i % 7 != 0 {
                vec![(i / 7) as u64]
            } else {
                Vec::new()
            },
            rung: (i % 3) as u32,
            matched: Some(i % 2 == 0),
            highlighted: Some(i % 3 == 0),
            target: None,
        })
        .collect();

    let identity = artifacts_identity_frame(&rows).len() as f64 / ROWS as f64;
    assert!(identity < 20.0, "identity rows are {identity:.1} B/row");
    let full = artifacts_frame(&rows).len() as f64 / ROWS as f64;
    assert!(full < 140.0, "full rows are {full:.1} B/row");
}

/// A page of records: an identifier, a nullable number, a category's keys under a dictionary and
/// a list of labels, so every buffer kind a records frame carries is present.
fn records_page(rows: u64) -> RecordBatch {
    use arrow::array::{ArrayRef, Float64Array, Int32Array, ListBuilder, StringBuilder};
    use arrow::datatypes::{Field, Int32Type, Schema};
    use std::sync::Arc;
    let ids: ArrayRef = Arc::new(UInt64Array::from_iter_values(
        (0..rows).map(|r| r.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
    ));
    let score: ArrayRef = Arc::new(Float64Array::from_iter(
        (0..rows).map(|r| (r % 5 != 0).then_some(r as f64 * 0.5)),
    ));
    let keys = Int32Array::from_iter_values((0..rows).map(|r| (r % 3) as i32));
    let values = StringArray::from(vec!["astro", "cond", "hep"]);
    let archive: ArrayRef =
        Arc::new(DictionaryArray::<Int32Type>::try_new(keys, Arc::new(values)).unwrap());
    let mut labels = ListBuilder::new(StringBuilder::new());
    for r in 0..rows {
        for label in ["public", "restricted"].iter().take((r % 3) as usize) {
            labels.values().append_value(label);
        }
        labels.append(true);
    }
    let labels: ArrayRef = Arc::new(labels.finish());
    let schema = Arc::new(Schema::new(vec![
        Field::new("tessera_id", DataType::UInt64, false),
        Field::new("score", DataType::Float64, true),
        Field::new("archive", archive.data_type().clone(), true),
        Field::new("tessera:labels", labels.data_type().clone(), false),
    ]));
    RecordBatch::try_new(schema, vec![ids, score, archive, labels]).unwrap()
}

/// An items body is a head, records frames each followed by a page end, and a trailer, and each
/// splits and decodes alone.
#[test]
fn an_items_body_splits_into_its_four_kinds() {
    let page = records_page(10);
    let mut body = records_head_frame(br#"{"order":"map","page_rows":10}"#);
    body.extend(records_frame(&page, RecordsCompression::None).unwrap());
    body.extend(page_end_frame(br#"{"next":null,"ended_by":"end"}"#));
    body.extend(trailer_frame(br#"{"pages":1,"rows":10,"next":null,"ended_by":"end"}"#));
    let frames = split_frames(&body).unwrap();
    let kinds: Vec<u8> = frames.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(
        kinds,
        vec![FRAME_RECORDS_HEAD, FRAME_RECORDS, FRAME_PAGE_END, FRAME_TRAILER]
    );
    assert_eq!(frames[0].1, br#"{"order":"map","page_rows":10}"#);
    assert_eq!(batches(frames[1].1), vec![page]);
}

/// A compressed records frame decodes to the same batch as an uncompressed one, and its buffers
/// are compressed rather than the frame being copied.
#[test]
fn a_zstd_records_frame_decodes_to_the_same_batch() {
    let page = records_page(5_000);
    let plain = records_frame(&page, RecordsCompression::None).unwrap();
    let zstd = records_frame(&page, RecordsCompression::Zstd).unwrap();
    assert_eq!(batch_of(&plain, FRAME_RECORDS), page);
    assert_eq!(batch_of(&zstd, FRAME_RECORDS), page);
    assert!(
        zstd.len() < plain.len(),
        "compressed {} bytes against {} plain",
        zstd.len(),
        plain.len()
    );
}
