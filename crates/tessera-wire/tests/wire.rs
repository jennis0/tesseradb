//! `tessera-wire`'s handle tables and framed Arrow IPC payloads (I10 — the trust boundary
//! between entity space and the wire; contracts §3.2 r26 — the streamed frame sequence).

use arrow::array::{Array, UInt64Array};
use arrow::datatypes::DataType;
use arrow::ipc::reader::StreamReader;
use tessera_types::{EntityId, Handle};
use tessera_wire::handles::HandleTable;
use tessera_wire::{
    artifacts_frame, artifacts_identity_frame, points_frame, split_frames, sub_cells_frame,
    tiles_frame, trailer_frame, ArtifactRow, ScalarColumn, FRAME_ARTIFACTS, FRAME_HEADER_BYTES,
    FRAME_POINTS, FRAME_SUB_CELLS, FRAME_TILES, FRAME_TRAILER,
};

/// The frame header's literal bytes, against `contracts §3.2`: five bytes of `u8 kind` then
/// `u32 LE payload length`, in that order, and the payload immediately after them. Every other
/// assertion in this file reaches a frame through `split_frames`, the reader half of the pair
/// that writes it, so the two agree by construction and the layout itself is pinned by nothing
/// on this side. The second reader is the TypeScript client, which decodes little-endian
/// independently (`clients/ts/core/src/frame.ts`); a change here that both Rust halves accept
/// breaks it at runtime.
///
/// Mutations this kills: writing or reading the length big-endian; emitting the length before
/// the kind; changing the header's width.
#[test]
fn the_frame_header_is_a_kind_byte_then_a_little_endian_length() {
    // Two frames, so the second's position also pins the header width and the length's meaning.
    let tiles = tiles_frame(&[30, 31], &[10, 5], &[10, 5], &[2, 1]);
    let mut body = tiles.clone();
    body.extend_from_slice(&trailer_frame(b"{}"));

    assert_eq!(
        FRAME_HEADER_BYTES, 5,
        "the header is one kind byte and four length bytes"
    );

    let tiles_payload_len = tiles.len() - FRAME_HEADER_BYTES;
    // Non-vacuity: a length whose two byte orders coincide would pin nothing.
    let len32 = u32::try_from(tiles_payload_len).unwrap();
    assert_ne!(
        len32.to_le_bytes(),
        len32.to_be_bytes(),
        "the fixture's payload length must distinguish the two byte orders, or this test \
         discriminates nothing"
    );

    assert_eq!(body[0], FRAME_TILES, "byte 0 of a frame is its kind");
    assert_eq!(
        &body[1..5],
        &len32.to_le_bytes(),
        "bytes 1..5 are the payload length, little-endian"
    );

    // The payload begins immediately after the header, and the next frame's header begins
    // immediately after the payload — so the length counts payload bytes and nothing else.
    let trailer_at = FRAME_HEADER_BYTES + tiles_payload_len;
    assert_eq!(
        body[trailer_at], FRAME_TRAILER,
        "the next frame's kind byte follows the previous frame's payload with no padding"
    );
    let trailer_len = u32::from_le_bytes(body[trailer_at + 1..trailer_at + 5].try_into().unwrap());
    assert_eq!(
        trailer_at + FRAME_HEADER_BYTES + trailer_len as usize,
        body.len(),
        "the declared lengths account for the whole body"
    );
}

/// (a) Handle stability + per-session isolation: the same entity, minted in two independent
/// tables, gets a handle stable within each table but not necessarily equal across tables.
#[test]
fn handle_is_stable_and_sessions_are_isolated() {
    let mut session_a = HandleTable::new();
    let mut session_b = HandleTable::new();
    let e = EntityId::new(4242);

    let h_a1 = session_a.handle_for(e);
    let h_a2 = session_a.handle_for(e);
    assert_eq!(h_a1, h_a2, "handle must be stable within a session");

    // Session B visits a different entity first, so the same entity lands on a different
    // handle number than in session A — demonstrating the two tables do not share state.
    let _ = session_b.handle_for(EntityId::new(1));
    let h_b = session_b.handle_for(e);
    assert_ne!(
        h_a1.raw(),
        h_b.raw(),
        "independent sessions must not share handle assignment for the same entity"
    );

    assert_eq!(session_a.entity_of(h_a1), Some(e));
    assert_eq!(session_b.entity_of(h_b), Some(e));
}

/// (b) `entity_of` of a handle this table never minted is `None`.
#[test]
fn entity_of_an_unminted_handle_is_none() {
    let mut table = HandleTable::new();
    let _ = table.handle_for(EntityId::new(7));
    assert_eq!(table.entity_of(Handle::new(99)), None);
}

/// One well-formed body from the frame builders, in contract order. The chunked points are two
/// frames on purpose: chunk boundaries are not contract, and a reader that only handles one
/// frame is wrong.
fn build_body() -> Vec<u8> {
    let counts_a = [70u64, 80];
    let counts_b = [90u64];
    let mut body = tiles_frame(&[30, 31], &[10, 5], &[10, 5], &[2, 1]);
    body.extend_from_slice(&points_frame(
        &[0, 1],
        &[1, 2],
        &[("count", ScalarColumn::U64(&counts_a))],
        &[],
    ));
    body.extend_from_slice(&points_frame(
        &[2],
        &[3],
        &[("count", ScalarColumn::U64(&counts_b))],
        &[],
    ));
    body.extend_from_slice(&trailer_frame(
        br#"{"stream_us":1,"arrow_serialise_ns":2,"points":3,"flushes":2}"#,
    ));
    body
}

/// (c) Encode a framed viewport body, walk it with `split_frames`, decode every Arrow payload
/// with `StreamReader`, and assert schemas, values and the cross-frame concatenation round-trip.
#[test]
fn viewport_frames_round_trip_through_arrow_ipc() {
    let body = build_body();
    let frames = split_frames(&body).unwrap();
    let kinds: Vec<u8> = frames.iter().map(|(k, _)| *k).collect();
    assert_eq!(
        kinds,
        vec![FRAME_TILES, FRAME_POINTS, FRAME_POINTS, FRAME_TRAILER]
    );

    let mut tile_reader = StreamReader::try_new(frames[0].1, None).unwrap();
    {
        let schema = tile_reader.schema();
        assert_eq!(schema.field(0).name(), "tile");
        assert_eq!(schema.field(1).name(), "visible");
        assert_eq!(schema.field(2).name(), "matched");
        // Appended, not inserted: decoders that index this batch positionally exist, so the
        // position of `served` is contract.
        assert_eq!(schema.field(3).name(), "served");
        assert_eq!(schema.fields().len(), 4);
    }
    let tile_batch = tile_reader.next().unwrap().unwrap();
    assert_eq!(tile_batch.num_rows(), 2);
    assert_eq!(
        tile_batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .values(),
        &[30u64, 31]
    );
    assert!(tile_reader.next().is_none(), "exactly one tile batch");

    // The two points frames decode independently — each is a complete stream — and their rows
    // concatenate to the full points set, in order.
    let mut ids = Vec::new();
    let mut counts = Vec::new();
    for (kind, payload) in &frames {
        if *kind != FRAME_POINTS {
            continue;
        }
        let mut reader = StreamReader::try_new(*payload, None).unwrap();
        let schema = reader.schema();
        assert_eq!(schema.field(0).name(), "tessera_id");
        assert_eq!(schema.field(1).name(), "code");
        assert_eq!(schema.field(2).name(), "count");
        for batch in reader.by_ref() {
            let batch = batch.unwrap();
            let id = batch
                .column(0)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap();
            let count = batch
                .column(2)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap();
            ids.extend(id.values().iter().copied());
            counts.extend(count.values().iter().copied());
        }
    }
    assert_eq!(ids, vec![0, 1, 2]);
    assert_eq!(counts, vec![70, 80, 90]);
}

/// (d) Byte-scan (I10): the 8-byte little-endian encoding of a set of entity ids must not appear
/// anywhere in the encoded body bytes — only the identities the caller passed (and plain
/// coordinate/scalar columns) may cross into the frame builders. The identity column is `u64`
/// (it carries `tessera_id`, not a `Handle`), so the handles minted here are widened to `u64`
/// before being passed in; the property under test — that the sensitive raw entity ids never
/// appear as bytes anywhere in the body — is unchanged from the pre-streaming format.
#[test]
fn frame_bytes_never_contain_a_raw_entity_id_encoding() {
    let sensitive_ids = [0xDEAD_BEEFu64, 7, 1_000_000];

    let mut table = HandleTable::new();
    let handles: Vec<u64> = sensitive_ids
        .iter()
        .map(|&raw| table.handle_for(EntityId::new(raw)).raw() as u64)
        .collect();
    let codes = vec![1u64; handles.len()];

    let n = handles.len() as u64;
    let mut body = tiles_frame(&[0], &[n], &[n], &[n]);
    body.extend_from_slice(&points_frame(&handles, &codes, &[], &[]));
    body.extend_from_slice(&trailer_frame(b"{}"));

    for &raw in &sensitive_ids {
        let needle = raw.to_le_bytes();
        assert!(
            !body.windows(needle.len()).any(|window| window == needle),
            "body bytes contain the 8-byte LE encoding of entity id {raw:#x}"
        );
    }
}

/// The points frame's identity column is `tessera_id: uint64` — the wire identity after the
/// boundary changed (decision 0006), replacing the per-session `handle: uint32` this crate
/// used to emit.
#[test]
fn the_points_frame_identity_column_is_tessera_id() {
    let frame = points_frame(&[10, 20, 30], &[1, 2, 3], &[], &[]);
    let (kind, payload) = split_frames(&frame).unwrap()[0];
    assert_eq!(kind, FRAME_POINTS);

    let mut points_reader = StreamReader::try_new(payload, None).unwrap();
    let schema = points_reader.schema();
    assert_eq!(schema.field(0).name(), "tessera_id");
    assert_eq!(schema.field(0).data_type(), &DataType::UInt64);

    let batch = points_reader.next().unwrap().unwrap();
    assert_eq!(
        batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .values(),
        &[10u64, 20, 30]
    );
}

// **The identity key on the viewer plane, and why no test here asserts it.**
//
// `IdentityKey` inverts every `tessera_id`. It is not secret against a bundle-holder (who can
// already invert every id trivially) but is secret against a client; leaking it on the viewer
// plane would hand a client entity space, which is exactly what I10 forbids.
//
// Nothing in `crates/tessera-wire/src/` constructs, holds or serialises an `IdentityKey`. That
// is a property of the code as written and **not** of the dependency graph, which permits one:
// this crate depends on `tessera-types`, `identity` is re-exported from that crate's root, and
// `IdentityKey::from_hex` is public — a function here that parsed a key and derived a
// `tessera_id` would compile. The mechanical guard is `scripts/check-layers.sh`'s grep for the
// type name over this crate's source, which is tight (the only route to a key is `from_hex`,
// which cannot be called without naming the type) and is the *only* one. It runs in a different
// CI job from the one that runs these tests, so a change that dropped it would not be noticed
// here.
//
// The byte-level evidence for I10 is the conformance suite's byte-scanner (`conformance.md`
// §4.3), which §4.6 cites as the I10 row's evidence — not anything in this file. A `#[test]`
// asserting the property from inside this crate would have to supply the key material itself,
// since the frame builders take `u64` columns and `&str` layer names, so the assertion would be
// about the fixture rather than about the code. `crates/tessera-types/tests/compile_fail.rs`'s
// module doc has already ruled on that shape in the opposite direction, refusing to write an I8
// placeholder because "a placeholder asserting that some stand-in type is immutable would report
// green while checking nothing".

/// The requested-but-empty underlay (contracts §3.2's r12 rule, carried into the framing): a
/// present kind-2 frame whose payload is a schema-only, zero-row stream — decodable, zero rows,
/// and visibly distinct from the unrequested case, which is no frame at all.
#[test]
fn an_empty_sub_cells_frame_is_schema_only_and_decodes_to_zero_rows() {
    let frame = sub_cells_frame(&[], &[]);
    let (kind, payload) = split_frames(&frame).unwrap()[0];
    assert_eq!(kind, FRAME_SUB_CELLS);
    assert!(!payload.is_empty(), "schema-only is bytes, not absence");

    let mut reader = StreamReader::try_new(payload, None).unwrap();
    let schema = reader.schema();
    assert_eq!(schema.field(0).name(), "cell");
    assert_eq!(schema.field(1).name(), "count");
    let rows: usize = reader.by_ref().map(|b| b.unwrap().num_rows()).sum();
    assert_eq!(rows, 0);
}

/// The populated sub-cells frame decodes as `(cell, count)` — no cursor arithmetic, no walking
/// another stream to its end: the frame boundary is the length prefix, which is the whole point
/// of §8.6(2)'s prefix-everything rule.
#[test]
fn the_sub_cells_frame_decodes_as_cell_and_count() {
    let cells = [100u64, 101];
    let counts = [5u64, 3];
    let frame = sub_cells_frame(&cells, &counts);
    let (_, payload) = split_frames(&frame).unwrap()[0];

    let mut reader = StreamReader::try_new(payload, None).unwrap();
    let batch = reader.next().unwrap().unwrap();
    assert_eq!(batch.num_rows(), 2);
    assert_eq!(
        batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .values(),
        &cells
    );
    assert_eq!(
        batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .values(),
        &counts
    );
}

/// A truncated body must never decode to a plausible shorter response — cut anywhere, the walk
/// refuses. This is the wire half of the truncation contract; the response half (a missing
/// trailer marks the body incomplete) is the consumers' to enforce and the server tests'.
#[test]
fn a_body_cut_at_any_byte_boundary_never_splits_cleanly_short() {
    let body = build_body();
    for cut in 1..body.len() {
        let frames = split_frames(&body[..cut]);
        match frames {
            Err(_) => {}
            Ok(frames) => {
                // A cut that lands exactly on a frame boundary walks cleanly — and is then
                // caught one level up by the missing trailer. Assert that is the only clean
                // case.
                assert_ne!(
                    frames.last().map(|(k, _)| *k),
                    Some(FRAME_TRAILER),
                    "a strict prefix of the body must never end in a trailer (cut at {cut})"
                );
            }
        }
    }
}

/// **The shape travels as parts of rings, and a reader that expects rings of vertices fails
/// rather than concatenating them** (`polygon-membership.md` §7.1). Both halves are the point of
/// the nesting: a flat encoding with a separate offsets column would let a reader that ignored the
/// offsets draw a chord from the end of one ring to the start of the next, silently and in the
/// shape of a real boundary; and one list of rings would have a second part drawn as a hole.
#[test]
fn the_artifacts_frame_carries_a_shape_as_parts_of_rings() {
    let two_parts = vec![
        vec![
            vec![[1u32, 2], [3, 4], [5, 6]],
            vec![[70, 80], [90, 100], [110, 120], [130, 140]],
        ],
        vec![vec![[7, 1], [8, 1], [9, 2]]],
    ];
    let rows = vec![
        ArtifactRow {
            layer: "clusters/a",
            tessera_id: 7,
            masked_count: 12,
            shape: Some(&two_parts),
            ..Default::default()
        },
        // A layer with no drawn geometry: null, and null is never *withheld*.
        ArtifactRow {
            layer: "clusters/a",
            tessera_id: 8,
            masked_count: 3,
            shape: None,
            ..Default::default()
        },
    ];

    let bytes = artifacts_frame(&rows);
    let frames = split_frames(&bytes).expect("one well-formed frame");
    assert_eq!(frames[0].0, FRAME_ARTIFACTS);
    let batch = StreamReader::try_new(std::io::Cursor::new(frames[0].1), None)
        .expect("arrow stream")
        .next()
        .expect("one batch")
        .expect("decodes");

    // The schema says *rings*, so a decoder written against the single-ring shape stops here.
    // And the two shape columns are the TRAILING columns — the only ones whose presence varies,
    // after every fixed-position column (`artifact-fetch-protocol.md` §8).
    let schema = batch.schema();
    let n = schema.fields().len();
    assert_eq!(schema.field(n - 2).name(), "shape_x");
    assert_eq!(schema.field(n - 1).name(), "shape_y");
    for name in ["shape_x", "shape_y"] {
        let field = schema.field_with_name(name).expect("column present");
        let DataType::List(part) = field.data_type() else {
            panic!("{name} is not a list");
        };
        let vertices = std::sync::Arc::new(arrow::datatypes::Field::new(
            "item",
            DataType::UInt32,
            false,
        ));
        let ring = std::sync::Arc::new(arrow::datatypes::Field::new(
            "item",
            DataType::List(vertices),
            false,
        ));
        assert_eq!(
            part.data_type(),
            &DataType::List(ring),
            "{name} is parts of rings of vertices, three lists deep"
        );
    }

    let axis = |name: &str| -> Vec<Option<Vec<Vec<Vec<u32>>>>> {
        let column = batch.column_by_name(name).unwrap();
        let outer = column
            .as_any()
            .downcast_ref::<arrow::array::ListArray>()
            .unwrap();
        (0..outer.len())
            .map(|i| {
                outer.is_valid(i).then(|| {
                    let parts = outer.value(i);
                    let parts = parts
                        .as_any()
                        .downcast_ref::<arrow::array::ListArray>()
                        .expect("a shape column is a list of parts");
                    (0..parts.len())
                        .map(|p| {
                            let rings = parts.value(p);
                            let rings = rings
                                .as_any()
                                .downcast_ref::<arrow::array::ListArray>()
                                .expect("a part is a list of rings");
                            (0..rings.len())
                                .map(|r| {
                                    let v = rings.value(r);
                                    let v = v
                                        .as_any()
                                        .downcast_ref::<arrow::array::UInt32Array>()
                                        .unwrap();
                                    (0..v.len()).map(|k| v.value(k)).collect()
                                })
                                .collect()
                        })
                        .collect()
                })
            })
            .collect()
    };
    let (xs, ys) = (axis("shape_x"), axis("shape_y"));
    // Part 0 is an outer with one hole; part 1 is a second outer — a hole and a second part are
    // different things to a renderer, and the nesting keeps them apart.
    assert_eq!(
        xs[0],
        Some(vec![
            vec![vec![1, 3, 5], vec![70, 90, 110, 130]],
            vec![vec![7, 8, 9]]
        ])
    );
    assert_eq!(
        ys[0],
        Some(vec![
            vec![vec![2, 4, 6], vec![80, 100, 120, 140]],
            vec![vec![1, 1, 2]]
        ])
    );
    assert_eq!(
        xs[1], None,
        "a layer with no drawn geometry is null, not an empty list"
    );
    assert_eq!(ys[1], None);
}

/// **`matched` is nullable because null is a value**: an unfiltered request asked no question, and
/// a `false` would answer one. Its position — last of the fixed columns, after `rung` — is
/// contract: decoders index this batch positionally, and only the shape columns may trail it.
#[test]
fn the_artifacts_frame_carries_the_filter_bit_with_null_meaning_no_filter() {
    let rows = vec![
        ArtifactRow {
            layer: "clusters/a",
            tessera_id: 7,
            masked_count: 12,
            matched: Some(true),
            ..Default::default()
        },
        ArtifactRow {
            layer: "clusters/a",
            tessera_id: 8,
            masked_count: 3,
            matched: Some(false),
            ..Default::default()
        },
        // The unfiltered request: no question was asked of this artifact.
        ArtifactRow {
            layer: "clusters/a",
            tessera_id: 9,
            masked_count: 1,
            matched: None,
            ..Default::default()
        },
    ];

    let bytes = artifacts_frame(&rows);
    let frames = split_frames(&bytes).expect("one well-formed frame");
    let batch = StreamReader::try_new(std::io::Cursor::new(frames[0].1), None)
        .expect("arrow stream")
        .next()
        .expect("one batch")
        .expect("decodes");

    let schema = batch.schema();
    assert_eq!(
        schema.fields().len() - 1,
        schema.index_of("matched").expect("column present"),
        "`matched` is the last column, and its position is contract"
    );
    let field = schema.field_with_name("matched").unwrap();
    assert_eq!(field.data_type(), &DataType::Boolean);
    assert!(
        field.is_nullable(),
        "null is *the request carried no filter*"
    );

    let column = batch
        .column_by_name("matched")
        .unwrap()
        .as_any()
        .downcast_ref::<arrow::array::BooleanArray>()
        .expect("a nullable Boolean");
    let read: Vec<Option<bool>> = (0..column.len())
        .map(|i| column.is_valid(i).then(|| column.value(i)))
        .collect();
    assert_eq!(read, vec![Some(true), Some(false), None]);
}

/// Decode a kind-5 payload into its one batch.
fn artifact_batch(bytes: &[u8]) -> arrow::record_batch::RecordBatch {
    let frames = split_frames(bytes).expect("one well-formed frame");
    assert_eq!(frames[0].0, FRAME_ARTIFACTS);
    StreamReader::try_new(std::io::Cursor::new(frames[0].1), None)
        .expect("arrow stream")
        .next()
        .expect("one batch")
        .expect("decodes")
}

/// The dictionary-decoded `layer` value of one row.
fn layer_at(batch: &arrow::record_batch::RecordBatch, row: usize) -> String {
    let column = batch
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::DictionaryArray<arrow::datatypes::UInt16Type>>()
        .expect("`layer` is dictionary-encoded, u16 keys over utf8 values");
    let values = column
        .values()
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .unwrap();
    values
        .value(column.key(row).expect("layer is never null"))
        .to_string()
}

/// **The full frame's fixed columns sit at fixed positions and the shape columns trail** —
/// `artifact-fetch-protocol.md` §8's reordering. `layer` is dictionary-encoded and decodes to the
/// layer names; `rung` is the renamed, re-meant `level` (§5.3) and is non-nullable.
#[test]
fn the_artifacts_frame_fixes_its_column_order_and_dictionary_encodes_the_layer() {
    let rows = vec![
        ArtifactRow {
            layer: "clusters/a",
            tessera_id: 7,
            masked_count: 12,
            rung: 3,
            ..Default::default()
        },
        ArtifactRow {
            layer: "regions/b",
            tessera_id: 8,
            masked_count: 3,
            rung: 0,
            ..Default::default()
        },
        ArtifactRow {
            layer: "clusters/a",
            tessera_id: 9,
            masked_count: 1,
            rung: 1,
            ..Default::default()
        },
    ];
    let batch = artifact_batch(&artifacts_frame(&rows));
    let names: Vec<String> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    assert_eq!(
        names,
        vec![
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
        ],
        "no row carries a shape, so the two trailing shape columns are ABSENT from the schema"
    );
    assert_eq!(layer_at(&batch, 0), "clusters/a");
    assert_eq!(layer_at(&batch, 1), "regions/b");
    assert_eq!(layer_at(&batch, 2), "clusters/a");
    let rung = batch
        .column_by_name("rung")
        .unwrap()
        .as_any()
        .downcast_ref::<arrow::array::UInt32Array>()
        .expect("`rung` is a non-nullable UInt32");
    assert_eq!(rung.values(), &[3u32, 0, 1]);
}

/// **The identity projection is its own fixed four-column schema** (`artifact-fetch-protocol.md`
/// §5.2): `layer` (dictionary-encoded), `tessera_id`, `rung`, `matched` — the payload columns
/// absent from the schema, never null, so decision 0076's null rule gains no third reading.
#[test]
fn the_identity_frame_is_four_columns_with_the_payload_absent_not_null() {
    let shape = vec![vec![vec![[1u32, 2], [3, 4], [5, 6]]]];
    let rows = vec![
        ArtifactRow {
            layer: "clusters/a",
            tessera_id: 7,
            masked_count: 12,
            rung: 2,
            matched: Some(true),
            shape: Some(&shape),
            ..Default::default()
        },
        ArtifactRow {
            layer: "regions/b",
            tessera_id: 8,
            masked_count: 3,
            rung: 0,
            matched: None,
            ..Default::default()
        },
    ];
    let batch = artifact_batch(&artifacts_identity_frame(&rows));
    let names: Vec<String> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    assert_eq!(
        names,
        vec!["layer", "tessera_id", "rung", "matched"],
        "a shape on the row does not put a shape column in the identity schema"
    );
    assert_eq!(layer_at(&batch, 0), "clusters/a");
    assert_eq!(layer_at(&batch, 1), "regions/b");
    let ids = batch
        .column_by_name("tessera_id")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    assert_eq!(ids.values(), &[7u64, 8]);
    let matched = batch
        .column_by_name("matched")
        .unwrap()
        .as_any()
        .downcast_ref::<arrow::array::BooleanArray>()
        .unwrap();
    assert!(matched.is_valid(0) && matched.value(0));
    assert!(!matched.is_valid(1), "null stays *no question was asked*");
}

/// **The size regression the design measured** (`artifact-fetch-protocol.md` §8, at 464,655 rows
/// with a 15-byte layer name: 13.6 B/row identity, ~107 B/row full with the dictionary, 125.0
/// without). Bounds, not exact values — Arrow metadata amortises differently at this row count —
/// held at ~100k synthetic rows:
///
/// - the identity projection stays under 20 B/row;
/// - the full row is cheaper with the dictionary than the same frame with a plain utf8 `layer`
///   column, which is asserted against a plain-utf8 stream of just that column, the encoding the
///   dictionary replaced.
#[test]
fn artifact_frame_bytes_per_row_hold_the_measured_bounds() {
    const ROWS: usize = 100_000;
    // A 15-byte name, matching the design's measurement.
    let layer = "clusters/hdbsca";
    assert_eq!(layer.len(), 15);
    let keys: Vec<String> = (0..ROWS).map(|i| format!("key-{i:07}")).collect();
    let content: Vec<Vec<String>> = (0..ROWS).map(|i| vec![format!("label {i}")]).collect();
    let rows: Vec<ArtifactRow<'_>> = (0..ROWS)
        .map(|i| ArtifactRow {
            layer,
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
        })
        .collect();

    let identity = artifacts_identity_frame(&rows).len() as f64 / ROWS as f64;
    assert!(
        identity < 20.0,
        "identity rows measured {identity:.1} B/row; the design's bound is 20"
    );

    // The frame with and without the dictionary differ only in the `layer` column's encoding, so
    // the "cheaper with than without" claim reduces to that column serialised both ways — one
    // Arrow stream each, both measured rather than modelled.
    let layer_column_stream = |schema: arrow::datatypes::Schema,
                               column: std::sync::Arc<dyn arrow::array::Array>|
     -> usize {
        let batch = arrow::record_batch::RecordBatch::try_new(
            std::sync::Arc::new(schema.clone()),
            vec![column],
        )
        .unwrap();
        let mut bytes = Vec::new();
        let mut writer = arrow::ipc::writer::StreamWriter::try_new(&mut bytes, &schema).unwrap();
        writer.write(&batch).unwrap();
        writer.finish().unwrap();
        bytes.len()
    };
    let plain = layer_column_stream(
        arrow::datatypes::Schema::new(vec![arrow::datatypes::Field::new(
            "layer",
            DataType::Utf8,
            false,
        )]),
        std::sync::Arc::new(arrow::array::StringArray::from_iter_values(
            rows.iter().map(|r| r.layer),
        )),
    );
    let dictionary = {
        let keys = arrow::array::UInt16Array::from_iter_values((0..ROWS).map(|_| 0u16));
        let values = std::sync::Arc::new(arrow::array::StringArray::from_iter_values([layer]));
        layer_column_stream(
            arrow::datatypes::Schema::new(vec![arrow::datatypes::Field::new(
                "layer",
                DataType::Dictionary(Box::new(DataType::UInt16), Box::new(DataType::Utf8)),
                false,
            )]),
            std::sync::Arc::new(
                arrow::array::DictionaryArray::<arrow::datatypes::UInt16Type>::try_new(
                    keys, values,
                )
                .unwrap(),
            ),
        )
    };
    assert!(
        dictionary < plain,
        "the dictionary encoding of `layer` ({dictionary} B) must undercut plain utf8 \
         ({plain} B) — the full row is cheaper with it than without by exactly this margin"
    );
    // And a loose absolute ceiling on the full row so the frame cannot quietly regress past the
    // design's measured order of magnitude (~107 B/row, hull-free, with this synthetic payload).
    let full_per_row = artifacts_frame(&rows).len() as f64 / ROWS as f64;
    assert!(
        full_per_row < 140.0,
        "full rows measured {full_per_row:.1} B/row; the design's order is ~107"
    );
    println!(
        "measured: identity {identity:.1} B/row, full {full_per_row:.1} B/row, \
         layer column {dictionary} B dictionary vs {plain} B plain at {ROWS} rows"
    );
}
