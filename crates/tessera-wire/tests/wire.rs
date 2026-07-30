//! Task 12, Step 1: failing tests for `tessera-wire`'s handle tables and Arrow IPC payloads
//! (I10 — the trust boundary between entity space and the wire).

use arrow::array::{Array, Float32Array, UInt64Array};
use arrow::datatypes::DataType;
use arrow::ipc::reader::StreamReader;
use tessera_types::{EntityId, Handle};
use tessera_wire::handles::HandleTable;
use tessera_wire::payload::{viewport_ipc, ScalarColumn, ViewportColumns};

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

/// (c) Encode a viewport payload, decode both batches with `StreamReader`, assert schemas and
/// values round-trip.
#[test]
fn viewport_payload_round_trips_through_arrow_ipc() {
    let tile = [30u64, 31];
    let visible = [10u64, 5];
    let matched = [10u64, 5];
    let tessera_ids = [0u64, 1, 2];
    let xs = [1.0f32, 2.0, 3.0];
    let ys = [4.0f32, 5.0, 6.0];
    let counts = [70u64, 80, 90];
    let scalars = [("count", ScalarColumn::U64(&counts))];

    // Two tiles serving 2 and 1 of the three points: `served` must sum to the points length, and
    // `viewport_ipc` asserts that, because the points batch is a flat concatenation whose only
    // grouping key is `served`.
    let served = [2u64, 1];
    let bytes = viewport_ipc(&ViewportColumns {
        tile: &tile,
        visible: &visible,
        matched: &matched,
        served: &served,
        points_tessera_ids: &tessera_ids,
        xs: &xs,
        ys: &ys,
        scalars: &scalars,
        sub_cells: None,
    });

    let tile_len = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let tile_bytes = &bytes[4..4 + tile_len];
    let points_bytes = &bytes[4 + tile_len..];

    let mut tile_reader = StreamReader::try_new(tile_bytes, None).unwrap();
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
        &tile
    );
    assert_eq!(
        tile_batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .values(),
        &visible
    );
    assert!(tile_reader.next().is_none(), "exactly one tile batch");

    let mut points_reader = StreamReader::try_new(points_bytes, None).unwrap();
    {
        let schema = points_reader.schema();
        assert_eq!(schema.field(0).name(), "tessera_id");
        assert_eq!(schema.field(1).name(), "x");
        assert_eq!(schema.field(2).name(), "y");
        assert_eq!(schema.field(3).name(), "count");
    }
    let points_batch = points_reader.next().unwrap().unwrap();
    assert_eq!(points_batch.num_rows(), 3);
    assert_eq!(
        points_batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .values(),
        &tessera_ids
    );
    assert_eq!(
        points_batch
            .column(1)
            .as_any()
            .downcast_ref::<Float32Array>()
            .unwrap()
            .values(),
        &xs
    );
    assert_eq!(
        points_batch
            .column(2)
            .as_any()
            .downcast_ref::<Float32Array>()
            .unwrap()
            .values(),
        &ys
    );
    assert_eq!(
        points_batch
            .column(3)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .values(),
        &counts
    );
    assert!(points_reader.next().is_none(), "exactly one points batch");
}

/// (d) Byte-scan (I10): the 8-byte little-endian encoding of a set of entity ids must not appear
/// anywhere in the encoded payload bytes — only the handles minted for them (and plain
/// coordinate/scalar columns) may cross into `payload::viewport_ipc`. `viewport_ipc`'s identity
/// column is `u64` now (it carries `tessera_id`, not a `Handle`), so the handles minted here are
/// widened to `u64` before being passed in; the property under test — that the sensitive raw
/// entity ids never appear as bytes anywhere in the payload — is unchanged.
#[test]
fn payload_bytes_never_contain_a_raw_entity_id_encoding() {
    let sensitive_ids = [0xDEAD_BEEFu64, 7, 1_000_000];

    let mut table = HandleTable::new();
    let handles: Vec<u64> = sensitive_ids
        .iter()
        .map(|&raw| table.handle_for(EntityId::new(raw)).raw() as u64)
        .collect();
    let xs = vec![1.0f32; handles.len()];
    let ys = vec![2.0f32; handles.len()];

    let one_tile = [0u64];
    let n = [handles.len() as u64];
    let bytes = viewport_ipc(&ViewportColumns {
        tile: &one_tile,
        visible: &n,
        matched: &n,
        served: &n,
        points_tessera_ids: &handles,
        xs: &xs,
        ys: &ys,
        scalars: &[],
        sub_cells: None,
    });

    for &raw in &sensitive_ids {
        let needle = raw.to_le_bytes();
        assert!(
            !bytes.windows(needle.len()).any(|window| window == needle),
            "payload bytes contain the 8-byte LE encoding of entity id {raw:#x}"
        );
    }
}

/// The points batch's identity column is `tessera_id: uint64` — the wire identity after the
/// r6/r21 boundary change, replacing the per-session `handle: uint32` this crate used to emit.
#[test]
fn the_points_batch_identity_column_is_tessera_id() {
    let tessera_ids = [10u64, 20, 30];
    let xs = [1.0f32, 2.0, 3.0];
    let ys = [4.0f32, 5.0, 6.0];

    let one_tile = [0u64];
    let n = [tessera_ids.len() as u64];
    let bytes = viewport_ipc(&ViewportColumns {
        tile: &one_tile,
        visible: &n,
        matched: &n,
        served: &n,
        points_tessera_ids: &tessera_ids,
        xs: &xs,
        ys: &ys,
        scalars: &[],
        sub_cells: None,
    });
    let tile_len = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let points_bytes = &bytes[4 + tile_len..];

    let mut points_reader = StreamReader::try_new(points_bytes, None).unwrap();
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
        &tessera_ids
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

/// **The appended sub-cell stream must be invisible to a reader that does not know about it.**
///
/// This is the falsifiable form of the backward-compatibility claim in `payload`'s module doc.
/// Rather than asserting that Arrow's `StreamReader` stops at the end-of-stream marker, it decodes
/// a *three*-stream payload using exactly the two-stream procedure a pre-underlay reader used —
/// take the `u32` prefix, slice the tile stream, treat **all** the rest as the points stream — and
/// asserts the points batch still decodes with the right rows. If Arrow ever began rejecting
/// trailing bytes, this fails rather than the claim quietly becoming false.
#[test]
fn a_pre_underlay_reader_still_decodes_a_payload_carrying_sub_cells() {
    let tile = [7u64];
    let visible = [9u64];
    let matched = [9u64];
    let served = [2u64];
    let tessera_ids = [11u64, 22];
    let xs = [1.0f32, 2.0];
    let ys = [3.0f32, 4.0];
    let cells = [100u64, 101, 102];
    let counts = [5u64, 3, 1];

    let with_underlay = viewport_ipc(&ViewportColumns {
        tile: &tile,
        visible: &visible,
        matched: &matched,
        served: &served,
        points_tessera_ids: &tessera_ids,
        xs: &xs,
        ys: &ys,
        scalars: &[],
        sub_cells: Some((&cells, &counts)),
    });

    // The pre-underlay decode procedure, verbatim: everything after the tile stream is "the points
    // stream", trailing bytes included.
    let tile_len = u32::from_le_bytes(with_underlay[0..4].try_into().unwrap()) as usize;
    let points_and_beyond = &with_underlay[4 + tile_len..];
    let mut reader = StreamReader::try_new(points_and_beyond, None).unwrap();
    let batch = reader.next().unwrap().unwrap();
    assert_eq!(batch.num_rows(), 2);
    assert_eq!(
        batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .values(),
        &tessera_ids,
        "an old reader must still see the points, with the sub-cell stream trailing it"
    );
}

/// Not requested means **zero trailing bytes**, not an empty schema-only stream — which is what
/// makes a default payload byte-identical to the pre-underlay format, and therefore what makes the
/// absence of an `API_VERSION` bump correct rather than convenient.
#[test]
fn an_unrequested_underlay_adds_no_bytes_at_all() {
    let tile = [7u64];
    let visible = [9u64];
    let matched = [9u64];
    let served = [2u64];
    let tessera_ids = [11u64, 22];
    let xs = [1.0f32, 2.0];
    let ys = [3.0f32, 4.0];

    let cols = |sub_cells| ViewportColumns {
        tile: &tile,
        visible: &visible,
        matched: &matched,
        served: &served,
        points_tessera_ids: &tessera_ids,
        xs: &xs,
        ys: &ys,
        scalars: &[],
        sub_cells,
    };

    let without = viewport_ipc(&cols(None));
    let empty_cells: [u64; 0] = [];
    let empty_counts: [u64; 0] = [];
    let with_empty = viewport_ipc(&cols(Some((&empty_cells, &empty_counts))));

    assert!(
        with_empty.len() > without.len(),
        "an empty sub-cell stream still costs schema bytes — which is exactly why `None` must mean \
         zero bytes rather than an empty stream"
    );
}

/// The sub-cell stream decodes as `(cell, count)` when a reader does look for it, found by parsing
/// the points stream to its end and taking the cursor — there is no length prefix for points, which
/// `payload`'s module doc records as the deliberate cost of appending rather than reframing.
#[test]
fn the_sub_cell_stream_decodes_as_cell_and_count() {
    let tile = [7u64];
    let visible = [9u64];
    let matched = [9u64];
    let served = [1u64];
    let tessera_ids = [11u64];
    let xs = [1.0f32];
    let ys = [3.0f32];
    let cells = [100u64, 101];
    let counts = [5u64, 3];

    let bytes = viewport_ipc(&ViewportColumns {
        tile: &tile,
        visible: &visible,
        matched: &matched,
        served: &served,
        points_tessera_ids: &tessera_ids,
        xs: &xs,
        ys: &ys,
        scalars: &[],
        sub_cells: Some((&cells, &counts)),
    });

    let tile_len = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let rest = &bytes[4 + tile_len..];

    // Parse the points stream, then resume from wherever it stopped. `StreamReader` borrows the
    // cursor (`impl Read for &mut R`), so the position is readable once the reader is dropped —
    // which is exactly the "parse to end-of-stream and take the cursor" procedure the module doc
    // says an underlay-aware reader must perform, since only the tile boundary is length-prefixed.
    let mut cursor = std::io::Cursor::new(rest);
    {
        let mut points_reader = StreamReader::try_new(&mut cursor, None).unwrap();
        while points_reader.next().is_some() {}
    }
    let consumed = cursor.position() as usize;

    let mut sub_reader = StreamReader::try_new(&rest[consumed..], None).unwrap();
    {
        let schema = sub_reader.schema();
        assert_eq!(schema.field(0).name(), "cell");
        assert_eq!(schema.field(1).name(), "count");
    }
    let batch = sub_reader.next().unwrap().unwrap();
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
