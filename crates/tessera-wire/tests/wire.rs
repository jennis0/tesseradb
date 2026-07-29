//! Task 12, Step 1: failing tests for `tessera-wire`'s handle tables and Arrow IPC payloads
//! (I10 — the trust boundary between entity space and the wire).

use arrow::array::{Array, Float32Array, UInt32Array, UInt64Array};
use arrow::ipc::reader::StreamReader;
use tessera_types::{EntityId, Handle};
use tessera_wire::handles::HandleTable;
use tessera_wire::payload::{viewport_ipc, ScalarColumn};

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
    let handles = [0u32, 1, 2];
    let xs = [1.0f32, 2.0, 3.0];
    let ys = [4.0f32, 5.0, 6.0];
    let counts = [70u64, 80, 90];
    let scalars = [("count", ScalarColumn::U64(&counts))];

    let bytes = viewport_ipc(&tile, &visible, &matched, &handles, &xs, &ys, &scalars);

    let tile_len = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let tile_bytes = &bytes[4..4 + tile_len];
    let points_bytes = &bytes[4 + tile_len..];

    let mut tile_reader = StreamReader::try_new(tile_bytes, None).unwrap();
    {
        let schema = tile_reader.schema();
        assert_eq!(schema.field(0).name(), "tile");
        assert_eq!(schema.field(1).name(), "visible");
        assert_eq!(schema.field(2).name(), "matched");
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
        assert_eq!(schema.field(0).name(), "handle");
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
            .downcast_ref::<UInt32Array>()
            .unwrap()
            .values(),
        &handles
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
/// coordinate/scalar columns) may cross into `payload::viewport_ipc`.
#[test]
fn payload_bytes_never_contain_a_raw_entity_id_encoding() {
    let sensitive_ids = [0xDEAD_BEEFu64, 7, 1_000_000];

    let mut table = HandleTable::new();
    let handles: Vec<u32> = sensitive_ids
        .iter()
        .map(|&raw| table.handle_for(EntityId::new(raw)).raw())
        .collect();
    let xs = vec![1.0f32; handles.len()];
    let ys = vec![2.0f32; handles.len()];

    let bytes = viewport_ipc(&[], &[], &[], &handles, &xs, &ys, &[]);

    for &raw in &sensitive_ids {
        let needle = raw.to_le_bytes();
        assert!(
            !bytes.windows(needle.len()).any(|window| window == needle),
            "payload bytes contain the 8-byte LE encoding of entity id {raw:#x}"
        );
    }
}
