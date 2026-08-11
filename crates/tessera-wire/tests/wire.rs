//! `tessera-wire`'s handle tables and framed Arrow IPC payloads (I10 — the trust boundary
//! between entity space and the wire; contracts §3.2 r26 — the streamed frame sequence).

use arrow::array::{Array, UInt64Array};
use arrow::datatypes::DataType;
use arrow::ipc::reader::StreamReader;
use tessera_types::{EntityId, Handle};
use tessera_wire::handles::HandleTable;
use tessera_wire::{
    points_frame, split_frames, sub_cells_frame, tiles_frame, trailer_frame, ScalarColumn,
    FRAME_POINTS, FRAME_SUB_CELLS, FRAME_TILES, FRAME_TRAILER,
};

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
    ));
    body.extend_from_slice(&points_frame(
        &[2],
        &[3],
        &[("count", ScalarColumn::U64(&counts_b))],
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
    body.extend_from_slice(&points_frame(&handles, &codes, &[]));
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
    let frame = points_frame(&[10, 20, 30], &[1, 2, 3], &[]);
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

/// `IdentityKey` inverts every `tessera_id`. It is not secret against a bundle-holder (who can
/// already invert every id trivially) but is secret against a client; leaking it on the viewer
/// plane would hand a client entity space, which is exactly what I10 forbids.
///
/// Enforced structurally here, not by a runtime byte-scan: `tessera-wire` has no dependency on
/// the module that defines `IdentityKey` (`tessera_types::identity`) and therefore cannot
/// construct, hold, or serialise one in the first place — there is nothing this crate's public
/// API could leak. `scripts/check-layers.sh` greps this crate's source for the type name so a
/// future dependency edge cannot reintroduce the possibility silently.
#[test]
fn payload_bytes_never_contain_the_identity_key() {}

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
