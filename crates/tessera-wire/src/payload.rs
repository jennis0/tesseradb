//! Arrow IPC payload construction for the viewer plane (Reference Sheet R5).
//!
//! `viewport_ipc` builds the two record batches a `POST /v1/viewport` response carries — tile
//! counts, then sampled points — and never touches the underlying entity-ID type: it accepts a
//! caller-supplied `tessera_id: u64` column and plain scalar slices exclusively (I10; enforced by
//! `scripts/check-layers.sh`, which greps this file for the forbidden identity type by name). No
//! engine or store type crosses into this module either — `tessera-server` reads `tessera_id`
//! straight off the engine's `PointOut` (contracts r6, owner decision 2026-07-29) and passes the
//! resulting plain slice here; there is no per-session translation left to do on this path (see
//! `crate::handles` for why the module that used to do that translation is retained, not deleted).
//!
//! The two batches have different schemas, so they cannot share one Arrow IPC stream. The
//! returned bytes are therefore two independent, complete Arrow IPC streams concatenated, framed
//! by a leading 4-byte little-endian length so a reader can find the boundary without parsing
//! Arrow metadata first:
//!
//! ```text
//! u32 LE: byte length of the tile stream
//! <tile stream bytes>      -- Arrow IPC stream, schema (tile: uint64, visible: uint64, matched: uint64)
//! <points stream bytes>    -- Arrow IPC stream, schema (tessera_id: uint64, x: float32, y: float32, ...scalars)
//! ```

use std::sync::Arc;

use arrow::array::{ArrayRef, Float32Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;

/// One named scalar column of declared-scalar values for the points batch.
///
/// Plain data only — no engine or store type. Each variant's slice must be the same length as
/// `points_tessera_ids`/`xs`/`ys` in the corresponding [`viewport_ipc`] call.
pub enum ScalarColumn<'a> {
    U64(&'a [u64]),
    F32(&'a [f32]),
    Utf8(&'a [String]),
}

/// Build the framed Arrow IPC payload for one `/v1/viewport` response.
///
/// `tile`/`visible`/`matched` must be the same length (one row per non-empty tile in the
/// response). `points_tessera_ids`/`xs`/`ys` and every slice inside `scalars` must be the same
/// length (one row per sampled point); `scalars` supplies the declared-scalar columns in the
/// schema's declared order, each tagged with its field name.
///
/// # Panics
///
/// Panics if the length invariants above are violated, or if Arrow's batch/stream construction
/// fails — both indicate a caller bug (mismatched slice lengths), not a runtime condition this
/// crate can recover from.
pub fn viewport_ipc(
    tile: &[u64],
    visible: &[u64],
    matched: &[u64],
    points_tessera_ids: &[u64],
    xs: &[f32],
    ys: &[f32],
    scalars: &[(&str, ScalarColumn)],
) -> Vec<u8> {
    assert_eq!(
        tile.len(),
        visible.len(),
        "viewport_ipc: tile/visible length mismatch"
    );
    assert_eq!(
        tile.len(),
        matched.len(),
        "viewport_ipc: tile/matched length mismatch"
    );
    assert_eq!(
        points_tessera_ids.len(),
        xs.len(),
        "viewport_ipc: points_tessera_ids/xs length mismatch"
    );
    assert_eq!(
        points_tessera_ids.len(),
        ys.len(),
        "viewport_ipc: points_tessera_ids/ys length mismatch"
    );
    for (name, col) in scalars {
        let len = match col {
            ScalarColumn::U64(s) => s.len(),
            ScalarColumn::F32(s) => s.len(),
            ScalarColumn::Utf8(s) => s.len(),
        };
        assert_eq!(
            points_tessera_ids.len(),
            len,
            "viewport_ipc: scalar column {name:?} length mismatch"
        );
    }

    let tile_stream = encode_tile_batch(tile, visible, matched);
    let points_stream = encode_points_batch(points_tessera_ids, xs, ys, scalars);

    let mut out = Vec::with_capacity(4 + tile_stream.len() + points_stream.len());
    out.extend_from_slice(&(tile_stream.len() as u32).to_le_bytes());
    out.extend_from_slice(&tile_stream);
    out.extend_from_slice(&points_stream);
    out
}

fn encode_tile_batch(tile: &[u64], visible: &[u64], matched: &[u64]) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("tile", DataType::UInt64, false),
        Field::new("visible", DataType::UInt64, false),
        Field::new("matched", DataType::UInt64, false),
    ]));

    let tile_col: ArrayRef = Arc::new(UInt64Array::from_iter_values(tile.iter().copied()));
    let visible_col: ArrayRef = Arc::new(UInt64Array::from_iter_values(visible.iter().copied()));
    let matched_col: ArrayRef = Arc::new(UInt64Array::from_iter_values(matched.iter().copied()));

    let batch = RecordBatch::try_new(schema.clone(), vec![tile_col, visible_col, matched_col])
        .expect("viewport_ipc: tile batch construction");

    write_stream(&schema, &batch)
}

fn encode_points_batch(
    points_tessera_ids: &[u64],
    xs: &[f32],
    ys: &[f32],
    scalars: &[(&str, ScalarColumn)],
) -> Vec<u8> {
    let mut fields = vec![
        Field::new("tessera_id", DataType::UInt64, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
    ];
    for (name, col) in scalars {
        let ty = match col {
            ScalarColumn::U64(_) => DataType::UInt64,
            ScalarColumn::F32(_) => DataType::Float32,
            ScalarColumn::Utf8(_) => DataType::Utf8,
        };
        fields.push(Field::new(*name, ty, false));
    }
    let schema = Arc::new(Schema::new(fields));

    let id_col: ArrayRef = Arc::new(UInt64Array::from_iter_values(
        points_tessera_ids.iter().copied(),
    ));
    let x_col: ArrayRef = Arc::new(Float32Array::from_iter_values(xs.iter().copied()));
    let y_col: ArrayRef = Arc::new(Float32Array::from_iter_values(ys.iter().copied()));

    let mut columns: Vec<ArrayRef> = vec![id_col, x_col, y_col];
    for (_, col) in scalars {
        let array: ArrayRef = match col {
            ScalarColumn::U64(s) => Arc::new(UInt64Array::from_iter_values(s.iter().copied())),
            ScalarColumn::F32(s) => Arc::new(Float32Array::from_iter_values(s.iter().copied())),
            ScalarColumn::Utf8(s) => Arc::new(StringArray::from_iter_values(s.iter())),
        };
        columns.push(array);
    }

    let batch = RecordBatch::try_new(schema.clone(), columns)
        .expect("viewport_ipc: points batch construction");

    write_stream(&schema, &batch)
}

fn write_stream(schema: &Schema, batch: &RecordBatch) -> Vec<u8> {
    let mut writer = StreamWriter::try_new(Vec::new(), schema)
        .expect("viewport_ipc: stream writer construction");
    writer.write(batch).expect("viewport_ipc: stream write");
    writer.into_inner().expect("viewport_ipc: stream finish")
}
