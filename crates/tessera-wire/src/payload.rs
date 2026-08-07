//! Arrow IPC payload construction for the viewer plane (contracts §3.2).
//!
//! `viewport_ipc` builds the two record batches a `POST /v1/viewport` response carries — tile
//! counts, then sampled points — and never touches the underlying entity-ID type: it accepts a
//! caller-supplied `tessera_id: u64` column and plain scalar slices exclusively (I10; enforced by
//! `scripts/check-layers.sh`, which greps this file for the forbidden identity type by name). No
//! engine or store type crosses into this module either — `tessera-server` reads `tessera_id`
//! straight off the engine's `PointOut` (contracts §2.6) and passes the
//! resulting plain slice here; there is no per-session translation left to do on this path (see
//! `crate::handles` for why the module that used to do that translation is retained, not deleted).
//!
//! The batches have different schemas, so they cannot share one Arrow IPC stream. The returned
//! bytes are therefore independent, complete Arrow IPC streams concatenated, framed by a leading
//! 4-byte little-endian length so a reader can find the *first* boundary without parsing Arrow
//! metadata first:
//!
//! ```text
//! u32 LE: byte length of the tile stream
//! <tile stream bytes>      -- Arrow IPC stream, schema (tile: uint64, visible: uint64, matched: uint64, served: uint64)
//! <points stream bytes>    -- Arrow IPC stream, schema (tessera_id: uint64, x: float32, y: float32, ...scalars)
//! <subcell stream bytes>   -- Arrow IPC stream, schema (cell: uint64, count: uint64); ABSENT ENTIRELY
//!                             (zero bytes) unless the request asked for the §3.3 underlay
//! ```
//!
//! **`served` is appended after `matched`, and the position is contract.** Decoders that index the
//! tile batch positionally exist, so inserting rather than appending would silently rebind
//! `visible`/`matched` in them.
//!
//! **The sub-cell stream is appended without a length prefix, and that is a deliberate relaxation
//! of the framing property above.** A reader that wants the sub-cells must parse the points stream
//! to its end-of-stream marker and take the cursor position, because only the *tile* boundary is
//! prefixed. Two reasons this is the right trade: the property survives untouched for every reader
//! that does not ask for the underlay, and the alternative — inserting a second length prefix — is
//! a breaking change to a frame that today's readers already parse. Because a request that does not
//! ask for the underlay produces **zero** trailing bytes (not an empty schema-only stream), such a
//! payload is byte-identical to what this module produced before the underlay existed, so no
//! `API_VERSION` bump is warranted: Arrow's `StreamReader` and `pyarrow.ipc.open_stream` both stop
//! at the end-of-stream marker without inspecting what follows.

use std::sync::Arc;

use arrow::array::{
    ArrayRef, Float32Array, Int64Array, StringArray, UInt16Array, UInt32Array, UInt64Array,
    UInt8Array,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;

/// One named scalar column of declared-scalar values for the points batch.
///
/// Plain data only — no engine or store type. Each variant's slice must be the same length as
/// `points_tessera_ids`/`codes` in the corresponding [`viewport_ipc`] call.
pub enum ScalarColumn<'a> {
    U8(&'a [u8]),
    U16(&'a [u16]),
    U32(&'a [u32]),
    U64(&'a [u64]),
    I64(&'a [i64]),
    F32(&'a [f32]),
    Utf8(&'a [String]),
}

/// One `/v1/viewport` response's columns, ready to encode.
///
/// A struct rather than a positional argument list because the count reached double figures once
/// `served` and the §3.3 underlay landed, and four of them are `&[u64]` — positional arguments of
/// the same type are exactly the shape a silent transposition hides in.
pub struct ViewportColumns<'a> {
    /// Tile batch: one row per non-empty tile. All four must be the same length.
    pub tile: &'a [u64],
    pub visible: &'a [u64],
    pub matched: &'a [u64],
    /// How many points this tile contributed to `points_tessera_ids`, in tile order — §7.2's
    /// `m(T)`. The points batch is a flat concatenation, so this is what lets a reader split it.
    pub served: &'a [u64],

    /// Points batch: one row per served point. All must be the same length.
    pub points_tessera_ids: &'a [u64],
    /// Each point's 64-bit Morton position code (see [`encode_points_batch`]).
    pub codes: &'a [u64],
    /// Declared-scalar columns in the schema's declared order, each tagged with its field name.
    pub scalars: &'a [(&'a str, ScalarColumn<'a>)],

    /// §3.3 underlay sub-cells: `(morton prefix at depth zoom+offset, exact masked count)`. `None`
    /// when the request did not ask for the underlay, which emits **zero** trailing bytes rather
    /// than an empty stream — see this module's doc. Both slices must be the same length.
    pub sub_cells: Option<(&'a [u64], &'a [u64])>,
}

/// Build the framed Arrow IPC payload for one `/v1/viewport` response.
///
/// # Panics
///
/// Panics if [`ViewportColumns`]' stated length invariants are violated, or if Arrow's batch/stream
/// construction fails — both indicate a caller bug, not a runtime condition this crate can recover
/// from.
pub fn viewport_ipc(cols: &ViewportColumns<'_>) -> Vec<u8> {
    let tiles = cols.tile.len();
    assert_eq!(tiles, cols.visible.len(), "tile/visible length mismatch");
    assert_eq!(tiles, cols.matched.len(), "tile/matched length mismatch");
    assert_eq!(tiles, cols.served.len(), "tile/served length mismatch");

    let points = cols.points_tessera_ids.len();
    assert_eq!(points, cols.codes.len(), "points/codes length mismatch");
    // The points batch is a flat concatenation whose only grouping key is `served`; if they
    // disagree, every consumer mis-splits it, so catch it here rather than at the client.
    let served_total: u64 = cols.served.iter().sum();
    assert_eq!(
        served_total, points as u64,
        "sum of served ({served_total}) != number of points ({points})"
    );
    for (name, col) in cols.scalars {
        let len = match col {
            ScalarColumn::U8(s) => s.len(),
            ScalarColumn::U16(s) => s.len(),
            ScalarColumn::U32(s) => s.len(),
            ScalarColumn::U64(s) => s.len(),
            ScalarColumn::I64(s) => s.len(),
            ScalarColumn::F32(s) => s.len(),
            ScalarColumn::Utf8(s) => s.len(),
        };
        assert_eq!(points, len, "scalar column {name:?} length mismatch");
    }
    if let Some((cells, counts)) = cols.sub_cells {
        assert_eq!(cells.len(), counts.len(), "sub-cell length mismatch");
    }

    let tile_stream = encode_tile_batch(cols.tile, cols.visible, cols.matched, cols.served);
    let points_stream = encode_points_batch(cols.points_tessera_ids, cols.codes, cols.scalars);
    let subcell_stream = cols
        .sub_cells
        .map(|(cells, counts)| encode_subcell_batch(cells, counts));

    let mut out = Vec::with_capacity(
        4 + tile_stream.len()
            + points_stream.len()
            + subcell_stream.as_ref().map_or(0, |s| s.len()),
    );
    out.extend_from_slice(&(tile_stream.len() as u32).to_le_bytes());
    out.extend_from_slice(&tile_stream);
    out.extend_from_slice(&points_stream);
    // Absent means zero bytes, not an empty stream — that is what keeps a no-underlay payload
    // byte-identical to the pre-underlay format.
    if let Some(subcells) = subcell_stream {
        out.extend_from_slice(&subcells);
    }
    out
}

fn encode_tile_batch(tile: &[u64], visible: &[u64], matched: &[u64], served: &[u64]) -> Vec<u8> {
    // `served` is APPENDED. Decoders that index this batch positionally exist, so inserting it
    // earlier would silently rebind `visible`/`matched` in them.
    let schema = Arc::new(Schema::new(vec![
        Field::new("tile", DataType::UInt64, false),
        Field::new("visible", DataType::UInt64, false),
        Field::new("matched", DataType::UInt64, false),
        Field::new("served", DataType::UInt64, false),
    ]));

    let columns: Vec<ArrayRef> = [tile, visible, matched, served]
        .into_iter()
        .map(|c| Arc::new(UInt64Array::from_iter_values(c.iter().copied())) as ArrayRef)
        .collect();

    let batch = RecordBatch::try_new(schema.clone(), columns)
        .expect("viewport_ipc: tile batch construction");

    write_stream(&schema, &batch)
}

/// The §3.3 density underlay's sub-cell counts: exact masked cardinalities over contiguous Morton
/// ranges at depth `zoom + offset`.
///
/// The depth is **not** carried here: it is `zoom + offset` from the caller's own request, and the
/// server rejects rather than clamps an out-of-range offset, so the client always knows it. A Morton
/// prefix does not encode its own depth, so the alternative would have been to echo it.
fn encode_subcell_batch(cells: &[u64], counts: &[u64]) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("cell", DataType::UInt64, false),
        Field::new("count", DataType::UInt64, false),
    ]));
    let columns: Vec<ArrayRef> = [cells, counts]
        .into_iter()
        .map(|c| Arc::new(UInt64Array::from_iter_values(c.iter().copied())) as ArrayRef)
        .collect();
    let batch = RecordBatch::try_new(schema.clone(), columns)
        .expect("viewport_ipc: sub-cell batch construction");
    write_stream(&schema, &batch)
}

/// The points batch: one `tessera_id` and one 64-bit position `code` per point.
///
/// `code` is the Morton interleave of the point's two 32-bit fixed-point axes against the extent
/// `/v1/meta` publishes — the same 16 bytes per point the `x`/`y` `f32` pair cost, carrying 32
/// bits per axis instead of an `f32` mantissa's 24, and letting the client derive the containing
/// tile at any zoom by a shift rather than by re-quantising (contracts §3.2).
fn encode_points_batch(
    points_tessera_ids: &[u64],
    codes: &[u64],
    scalars: &[(&str, ScalarColumn)],
) -> Vec<u8> {
    let mut fields = vec![
        Field::new("tessera_id", DataType::UInt64, false),
        Field::new("code", DataType::UInt64, false),
    ];
    for (name, col) in scalars {
        let ty = match col {
            ScalarColumn::U8(_) => DataType::UInt8,
            ScalarColumn::U16(_) => DataType::UInt16,
            ScalarColumn::U32(_) => DataType::UInt32,
            ScalarColumn::U64(_) => DataType::UInt64,
            ScalarColumn::I64(_) => DataType::Int64,
            ScalarColumn::F32(_) => DataType::Float32,
            ScalarColumn::Utf8(_) => DataType::Utf8,
        };
        fields.push(Field::new(*name, ty, false));
    }
    let schema = Arc::new(Schema::new(fields));

    let id_col: ArrayRef = Arc::new(UInt64Array::from_iter_values(
        points_tessera_ids.iter().copied(),
    ));
    let code_col: ArrayRef = Arc::new(UInt64Array::from_iter_values(codes.iter().copied()));

    let mut columns: Vec<ArrayRef> = vec![id_col, code_col];
    for (_, col) in scalars {
        let array: ArrayRef = match col {
            ScalarColumn::U8(s) => Arc::new(UInt8Array::from_iter_values(s.iter().copied())),
            ScalarColumn::U16(s) => Arc::new(UInt16Array::from_iter_values(s.iter().copied())),
            ScalarColumn::U32(s) => Arc::new(UInt32Array::from_iter_values(s.iter().copied())),
            ScalarColumn::U64(s) => Arc::new(UInt64Array::from_iter_values(s.iter().copied())),
            ScalarColumn::I64(s) => Arc::new(Int64Array::from_iter_values(s.iter().copied())),
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
