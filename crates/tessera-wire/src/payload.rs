//! Frame construction for the viewer plane's streamed `POST /v1/viewport` response
//! (`docs/design/streamed-serving.md`; contracts §3.2).
//!
//! A response body is a sequence of frames, each `u8 kind` + `u32 LE payload length` + payload.
//! Every frame is a complete, independently decodable unit — an Arrow IPC stream for the three
//! data kinds, JSON for the trailer — so no reader ever walks Arrow message framing to find a
//! boundary (client-interaction §8.6(2)'s length-prefix-everything item):
//!
//! ```text
//! kind 1  tiles      Arrow IPC stream (tile, visible, matched, served — all uint64); exactly
//!                    one, first
//! kind 2  sub-cells  Arrow IPC stream (cell: uint64, count: uint64); exactly one, iff the
//!                    request asked for the §3.3 underlay — schema-only when requested-but-empty,
//!                    ABSENT ENTIRELY when unrequested
//! kind 3  points     Arrow IPC stream (tessera_id: uint64, code: uint64, ...scalars); zero or
//!                    more, whole tiles per frame, concatenating to the full points stream
//! kind 4  trailer    JSON; exactly one, last — its presence is the completeness signal
//! ```
//!
//! This module never touches the underlying entity-ID type: it accepts a caller-supplied
//! `tessera_id: u64` column and plain scalar slices exclusively (I10; enforced by
//! `scripts/check-layers.sh`, which greps this file for the forbidden identity type by name — the
//! module NAME is therefore load-bearing: moving the frame writer out of `payload.rs` would leave
//! that rule permanently green). No engine or store type crosses into this module either —
//! `tessera-server` reads `tessera_id` straight off the engine's `PointColumns` and passes plain
//! slices here.
//!
//! **`served` is appended after `matched` in the tiles batch, and the position is contract.**
//! Decoders that index the tile batch positionally exist, so inserting rather than appending
//! would silently rebind `visible`/`matched` in them.
//!
//! The pre-streaming monolithic framing (`viewport_ipc`: one `u32` prefix on the tile stream
//! only, everything else concatenated bare) is deleted rather than kept alongside — decision
//! 0048: no deployment, bundle or client exists outside this repository, and two framings would
//! be two conformance surfaces forever.

use std::sync::Arc;

use arrow::array::{
    ArrayRef, BooleanArray, Float32Array, Float64Array, Int16Array, Int32Array, Int64Array,
    Int8Array, ListBuilder, StringArray, TimestampMicrosecondArray, UInt16Array, UInt32Array,
    UInt32Builder, UInt64Array, UInt8Array,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;

/// Frame kinds, one `u8` each. An unknown kind is a decoder error, never skipped: skipping would
/// let a future frame kind carry data an old reader silently drops.
pub const FRAME_TILES: u8 = 1;
pub const FRAME_SUB_CELLS: u8 = 2;
pub const FRAME_POINTS: u8 = 3;
pub const FRAME_TRAILER: u8 = 4;
pub const FRAME_ARTIFACTS: u8 = 5;

/// Bytes of frame header preceding every payload: the kind byte and the `u32 LE` length.
pub const FRAME_HEADER_BYTES: usize = 5;

/// One named scalar column of declared-scalar values for a points frame.
///
/// Plain data only — no engine or store type. Each variant's slice must be the same length as
/// `tessera_ids`/`codes` in the corresponding [`points_frame`] call.
pub enum ScalarColumn<'a> {
    Bool(&'a [bool]),
    U8(&'a [u8]),
    U16(&'a [u16]),
    U32(&'a [u32]),
    U64(&'a [u64]),
    I8(&'a [i8]),
    I16(&'a [i16]),
    I32(&'a [i32]),
    I64(&'a [i64]),
    F32(&'a [f32]),
    F64(&'a [f64]),
    /// Microseconds since the Unix epoch; encoded as Arrow `Timestamp(Microsecond, None)` so a
    /// client reads a time rather than an integer it has to be told about out of band.
    TimestampUs(&'a [i64]),
    Utf8(&'a [String]),
}

/// The three per-variant facts this module needs from a [`ScalarColumn`]: its length, its Arrow
/// type and its array.
///
/// **One table, three functions**, because the three were three separate `match`es over the same
/// thirteen variants — and a type present in two of them and missing from the third is a column
/// that validates, types correctly and encodes as something else.
///
/// `Bool` and `Utf8` are hand-written in each: Arrow builds both from an owned collection rather
/// than `from_iter_values`, because neither target is a flat copy of the input — a bitmap and an
/// offset table respectively.
macro_rules! wire_columns {
    ($mac:ident) => {
        $mac! {
            (U8, UInt8Array, DataType::UInt8),
            (U16, UInt16Array, DataType::UInt16),
            (U32, UInt32Array, DataType::UInt32),
            (U64, UInt64Array, DataType::UInt64),
            (I8, Int8Array, DataType::Int8),
            (I16, Int16Array, DataType::Int16),
            (I32, Int32Array, DataType::Int32),
            (I64, Int64Array, DataType::Int64),
            (F32, Float32Array, DataType::Float32),
            (F64, Float64Array, DataType::Float64),
            (TimestampUs, TimestampMicrosecondArray,
             DataType::Timestamp(arrow::datatypes::TimeUnit::Microsecond, None)),
        }
    };
}

fn wire_column_len(col: &ScalarColumn) -> usize {
    macro_rules! arms {
        ($(($v:ident, $arr:ident, $dt:expr)),* $(,)?) => {
            match col {
                $(ScalarColumn::$v(s) => s.len(),)*
                ScalarColumn::Bool(s) => s.len(),
                ScalarColumn::Utf8(s) => s.len(),
            }
        };
    }
    wire_columns!(arms)
}

fn wire_column_type(col: &ScalarColumn) -> DataType {
    macro_rules! arms {
        ($(($v:ident, $arr:ident, $dt:expr)),* $(,)?) => {
            match col {
                $(ScalarColumn::$v(_) => $dt,)*
                ScalarColumn::Bool(_) => DataType::Boolean,
                ScalarColumn::Utf8(_) => DataType::Utf8,
            }
        };
    }
    wire_columns!(arms)
}

fn wire_column_array(col: &ScalarColumn) -> ArrayRef {
    macro_rules! arms {
        ($(($v:ident, $arr:ident, $dt:expr)),* $(,)?) => {
            match col {
                $(ScalarColumn::$v(s) => Arc::new($arr::from_iter_values(s.iter().copied())),)*
                ScalarColumn::Bool(s) => Arc::new(BooleanArray::from(s.to_vec())),
                ScalarColumn::Utf8(s) => Arc::new(StringArray::from_iter_values(s.iter())),
            }
        };
    }
    wire_columns!(arms)
}

/// Append one frame header to `out`, returning the offset of its length field so
/// [`patch_frame_len`] can complete it once the payload is written. Splitting the write this way
/// is what lets the points payload stream straight into the response buffer (see
/// [`points_frame`]) instead of materialising once and being copied behind a known length.
fn begin_frame(out: &mut Vec<u8>, kind: u8) -> usize {
    out.push(kind);
    let len_at = out.len();
    out.extend_from_slice(&[0u8; 4]);
    len_at
}

/// Complete the frame begun at `len_at`: everything appended since is the payload.
///
/// # Panics
///
/// Panics if the payload exceeds `u32::MAX` bytes — a caller bug (an unflushed accumulation)
/// rather than a runtime condition: the emit pass caps every points frame at 1 GiB regardless of
/// the configured flush threshold (`tessera_engine`'s `MAX_POINTS_FRAME_BYTES`), and no other
/// frame kind grows with the corpus.
fn patch_frame_len(out: &mut [u8], len_at: usize) {
    let len = out.len() - len_at - 4;
    let len = u32::try_from(len).expect("frame payload exceeds u32::MAX");
    out[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
}

/// The kind-1 tiles frame: one row per non-empty tile, `(tile, visible, matched, served)`.
///
/// # Panics
///
/// Panics on a length mismatch between the four columns, or on Arrow construction failure —
/// caller bugs, not runtime conditions this crate can recover from.
pub fn tiles_frame(tile: &[u64], visible: &[u64], matched: &[u64], served: &[u64]) -> Vec<u8> {
    let tiles = tile.len();
    assert_eq!(tiles, visible.len(), "tile/visible length mismatch");
    assert_eq!(tiles, matched.len(), "tile/matched length mismatch");
    assert_eq!(tiles, served.len(), "tile/served length mismatch");

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
    let batch =
        RecordBatch::try_new(schema.clone(), columns).expect("tiles frame batch construction");

    let mut out = Vec::with_capacity(FRAME_HEADER_BYTES + tiles * 32 + 1024);
    let len_at = begin_frame(&mut out, FRAME_TILES);
    write_stream_into(&schema, &batch, &mut out);
    patch_frame_len(&mut out, len_at);
    out
}

/// The kind-2 sub-cells frame: the §3.3 density underlay's exact masked counts over contiguous
/// Morton ranges at depth `zoom + offset`.
///
/// The depth is **not** carried here: it is `zoom + offset` from the caller's own request, and the
/// server rejects rather than clamps an out-of-range offset, so the client always knows it. A
/// Morton prefix does not encode its own depth, so the alternative would have been to echo it.
///
/// Emitted iff the request asked for the underlay — a requested-but-empty underlay is this frame
/// with a schema-only, zero-row stream, never an absent frame, so presence is decided by the
/// request rather than by the result (contracts §3.2 r12's rule, carried forward).
///
/// # Panics
///
/// Panics on a `cells`/`counts` length mismatch or Arrow construction failure.
pub fn sub_cells_frame(cells: &[u64], counts: &[u64]) -> Vec<u8> {
    assert_eq!(cells.len(), counts.len(), "sub-cell length mismatch");
    let schema = Arc::new(Schema::new(vec![
        Field::new("cell", DataType::UInt64, false),
        Field::new("count", DataType::UInt64, false),
    ]));
    let columns: Vec<ArrayRef> = [cells, counts]
        .into_iter()
        .map(|c| Arc::new(UInt64Array::from_iter_values(c.iter().copied())) as ArrayRef)
        .collect();
    let batch =
        RecordBatch::try_new(schema.clone(), columns).expect("sub-cells frame batch construction");

    let mut out = Vec::with_capacity(FRAME_HEADER_BYTES + cells.len() * 16 + 1024);
    let len_at = begin_frame(&mut out, FRAME_SUB_CELLS);
    write_stream_into(&schema, &batch, &mut out);
    patch_frame_len(&mut out, len_at);
    out
}

/// One served artifact, as the kind-5 frame carries it.
///
/// **A struct rather than parallel slices**, because the geometry columns are optional per row and
/// keeping eleven slices aligned at the call site is the shape a mismatch hides in.
///
/// **What is not here is the design.** No ordinal: a position in a dense level, so two of them
/// count what lies between (C8). No declared membership size: a corpus-wide count over items this
/// principal may not see, and the denominator the proportional criterion divides by — a predicate
/// input, never a field. No membership. And no reason an artifact is absent, because one that
/// failed its criterion must be indistinguishable from one that was never published.
#[derive(Debug, Clone, Default)]
pub struct ArtifactRow<'a> {
    pub layer: &'a str,
    pub tessera_id: u64,
    pub stable_key: Option<&'a str>,
    pub masked_count: u64,
    /// Derived geometry, in the **grid units** the points frame's `code` is built from — the
    /// client needs no quantisation extent to draw either. Each is present exactly when the
    /// artifact's layer declared it, and describes the members *this* principal can see.
    pub centroid: Option<[f64; 2]>,
    /// `[qx_min, qy_min, qx_max, qy_max]`.
    pub bbox: Option<[u32; 4]>,
    pub hull: Option<&'a [[u32; 2]]>,
}

/// The kind-5 artifacts frame: one row per served artifact.
///
/// `masked_count` is `UInt64` and `tessera_id` is `UInt64`, matching the points frame's `tessera_id`
/// column so a client's decoder has one identifier type across the response.
///
/// **The geometry columns are nullable and the schema is fixed**, because one response carries
/// artifacts from several layers and layers declare different vocabularies. A null is *this layer
/// declares no centroid*; it is never *withheld*, since an artifact whose content could not be
/// served is absent entirely (decision 0076). The hull travels as two `List<UInt32>` columns rather
/// than one interleaved list so that a client reads an axis without a stride.
///
/// # Panics
///
/// Panics on Arrow construction failure.
pub fn artifacts_frame(rows: &[ArtifactRow<'_>]) -> Vec<u8> {
    // One definition of the hull's element field, used by the schema and by the builders below:
    // a `ListBuilder` builds a **nullable** item field by default, and a vertex is never null — a
    // hull is a list of positions or it is absent entirely. Declaring it twice is how the two drift
    // into the mismatch Arrow then refuses at batch construction.
    let item = || Arc::new(Field::new("item", DataType::UInt32, false));

    let schema = Arc::new(Schema::new(vec![
        Field::new("layer", DataType::Utf8, false),
        Field::new("tessera_id", DataType::UInt64, false),
        // A publisher need not supply a key.
        Field::new("stable_key", DataType::Utf8, true),
        Field::new("masked_count", DataType::UInt64, false),
        Field::new("centroid_x", DataType::Float64, true),
        Field::new("centroid_y", DataType::Float64, true),
        Field::new("box_min_x", DataType::UInt32, true),
        Field::new("box_min_y", DataType::UInt32, true),
        Field::new("box_max_x", DataType::UInt32, true),
        Field::new("box_max_y", DataType::UInt32, true),
        Field::new(
            "hull_x",
            DataType::List(item()),
            true,
        ),
        Field::new(
            "hull_y",
            DataType::List(item()),
            true,
        ),
    ]));

    let mut hull_x = ListBuilder::new(UInt32Builder::new()).with_field(item());
    let mut hull_y = ListBuilder::new(UInt32Builder::new()).with_field(item());
    for row in rows {
        match row.hull {
            Some(vertices) => {
                for v in vertices {
                    hull_x.values().append_value(v[0]);
                    hull_y.values().append_value(v[1]);
                }
                hull_x.append(true);
                hull_y.append(true);
            }
            None => {
                hull_x.append_null();
                hull_y.append_null();
            }
        }
    }

    let columns: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.layer))),
        Arc::new(UInt64Array::from_iter_values(
            rows.iter().map(|r| r.tessera_id),
        )),
        Arc::new(StringArray::from_iter(rows.iter().map(|r| r.stable_key))),
        Arc::new(UInt64Array::from_iter_values(
            rows.iter().map(|r| r.masked_count),
        )),
        Arc::new(Float64Array::from_iter(
            rows.iter().map(|r| r.centroid.map(|c| c[0])),
        )),
        Arc::new(Float64Array::from_iter(
            rows.iter().map(|r| r.centroid.map(|c| c[1])),
        )),
        Arc::new(UInt32Array::from_iter(
            rows.iter().map(|r| r.bbox.map(|b| b[0])),
        )),
        Arc::new(UInt32Array::from_iter(
            rows.iter().map(|r| r.bbox.map(|b| b[1])),
        )),
        Arc::new(UInt32Array::from_iter(
            rows.iter().map(|r| r.bbox.map(|b| b[2])),
        )),
        Arc::new(UInt32Array::from_iter(
            rows.iter().map(|r| r.bbox.map(|b| b[3])),
        )),
        Arc::new(hull_x.finish()),
        Arc::new(hull_y.finish()),
    ];
    let batch =
        RecordBatch::try_new(schema.clone(), columns).expect("artifacts frame batch construction");

    let mut out = Vec::with_capacity(FRAME_HEADER_BYTES + rows.len() * 96 + 1024);
    let len_at = begin_frame(&mut out, FRAME_ARTIFACTS);
    write_stream_into(&schema, &batch, &mut out);
    patch_frame_len(&mut out, len_at);
    out
}

/// The kind-3 points frame: one `tessera_id` and one 64-bit position `code` per point, plus the
/// declared scalars in schema order.
///
/// `code` is the Morton interleave of the point's two 32-bit fixed-point axes against the extent
/// `/v1/meta` publishes — the same 16 bytes per point the `x`/`y` `f32` pair cost, carrying 32
/// bits per axis instead of an `f32` mantissa's 24, and letting the client derive the containing
/// tile at any zoom by a shift rather than by re-quantising (contracts §2.5).
///
/// **The stream is written straight into the frame buffer**, sized once from
/// [`estimated_points_bytes`]: at a saturated flush this payload is a megabyte-plus, so the copy
/// a materialise-then-frame shape would cost is the largest single memmove in the response.
///
/// # Panics
///
/// Panics on any column length mismatch or Arrow construction failure.
pub fn points_frame(
    tessera_ids: &[u64],
    codes: &[u64],
    scalars: &[(&str, ScalarColumn)],
) -> Vec<u8> {
    let points = tessera_ids.len();
    assert_eq!(points, codes.len(), "points/codes length mismatch");
    for (name, col) in scalars {
        let len = wire_column_len(col);
        assert_eq!(points, len, "scalar column {name:?} length mismatch");
    }

    let mut fields = vec![
        Field::new("tessera_id", DataType::UInt64, false),
        Field::new("code", DataType::UInt64, false),
    ];
    for (name, col) in scalars {
        fields.push(Field::new(*name, wire_column_type(col), false));
    }
    let schema = Arc::new(Schema::new(fields));

    let mut columns: Vec<ArrayRef> = Vec::with_capacity(2 + scalars.len());
    columns.push(Arc::new(UInt64Array::from_iter_values(
        tessera_ids.iter().copied(),
    )));
    columns.push(Arc::new(UInt64Array::from_iter_values(codes.iter().copied())));
    for (_, col) in scalars {
        columns.push(wire_column_array(col));
    }
    let batch =
        RecordBatch::try_new(schema.clone(), columns).expect("points frame batch construction");

    let mut out =
        Vec::with_capacity(FRAME_HEADER_BYTES + estimated_points_bytes(points, scalars));
    let len_at = begin_frame(&mut out, FRAME_POINTS);
    write_stream_into(&schema, &batch, &mut out);
    patch_frame_len(&mut out, len_at);
    out
}

/// The kind-4 trailer frame. The payload is caller-supplied JSON bytes — this module frames, it
/// does not author; what the object must contain is contracts §3.2's business, and the server is
/// the one place it is written.
pub fn trailer_frame(json: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_BYTES + json.len());
    let len_at = begin_frame(&mut out, FRAME_TRAILER);
    out.extend_from_slice(json);
    patch_frame_len(&mut out, len_at);
    out
}

/// Split a complete response body into `(kind, payload)` frames.
///
/// Strict: a short header, a payload running past the end of the body, or an unknown kind is an
/// error, never a partial success — a truncated body must not decode to a plausible shorter
/// response. This is the reader half every Rust consumer (the server's own tests, the bench
/// harness) shares; the Python oracle and the TS client carry independent implementations of the
/// same walk, deliberately (contracts §0.2's second-reader posture).
pub fn split_frames(body: &[u8]) -> Result<Vec<(u8, &[u8])>, FrameError> {
    let mut frames = Vec::new();
    let mut at = 0usize;
    while at < body.len() {
        if body.len() - at < FRAME_HEADER_BYTES {
            return Err(FrameError::TruncatedHeader { at });
        }
        let kind = body[at];
        if !matches!(
            kind,
            FRAME_TILES | FRAME_SUB_CELLS | FRAME_POINTS | FRAME_TRAILER | FRAME_ARTIFACTS
        ) {
            return Err(FrameError::UnknownKind { kind, at });
        }
        let len = u32::from_le_bytes(body[at + 1..at + 5].try_into().expect("4 bytes")) as usize;
        let start = at + FRAME_HEADER_BYTES;
        let end = start
            .checked_add(len)
            .ok_or(FrameError::TruncatedPayload { at })?;
        if end > body.len() {
            return Err(FrameError::TruncatedPayload { at });
        }
        frames.push((kind, &body[start..end]));
        at = end;
    }
    Ok(frames)
}

/// [`split_frames`]' failures. Byte offsets are into the body, for the error message's benefit —
/// nothing programmatic hangs off them.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    TruncatedHeader { at: usize },
    TruncatedPayload { at: usize },
    UnknownKind { kind: u8, at: usize },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::TruncatedHeader { at } => {
                write!(f, "truncated frame header at byte {at}")
            }
            FrameError::TruncatedPayload { at } => {
                write!(f, "frame at byte {at} claims a payload past the end of the body")
            }
            FrameError::UnknownKind { kind, at } => {
                write!(f, "unknown frame kind {kind} at byte {at}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// How large a points payload will be, near enough to size the frame buffer once.
///
/// **A hint, never a contract.** A short estimate costs a reallocation and a long one costs
/// transient memory; neither changes a byte of output, which is why this is allowed to approximate
/// nothing — the fixed-width columns are exact and `utf8` is walked.
fn estimated_points_bytes(points: usize, scalars: &[(&str, ScalarColumn)]) -> usize {
    // `tessera_id` and `code`, both u64.
    let mut bytes = points * 16;
    for (_, col) in scalars {
        bytes += match col {
            ScalarColumn::Bool(_) => points.div_ceil(8),
            // Offsets plus data. The data length is the one thing here worth a walk: it is exact
            // and `utf8` columns are the only ones that can dwarf the estimate if guessed.
            ScalarColumn::Utf8(s) => 4 * (points + 1) + s.iter().map(|v| v.len()).sum::<usize>(),
            other => points * wire_column_width(other),
        };
        // Arrow pads every buffer to an 8-byte boundary and prefixes each with its own metadata.
        bytes += 64;
    }
    // Schema and record-batch metadata: a few hundred bytes, plus a field descriptor apiece.
    bytes + 1024 + 128 * scalars.len()
}

/// Element width in bytes for the fixed-width families. `Bool` and `Utf8` are not fixed-width and
/// are handled by their own arms in [`estimated_points_bytes`]; both return 0 here.
fn wire_column_width(col: &ScalarColumn) -> usize {
    macro_rules! arms {
        ($(($v:ident, $arr:ident, $dt:expr)),* $(,)?) => {
            match col {
                $(ScalarColumn::$v(_) => std::mem::size_of::<wire_elem!($v)>(),)*
                ScalarColumn::Bool(_) | ScalarColumn::Utf8(_) => 0,
            }
        };
    }
    wire_columns!(arms)
}

/// The element type behind each `ScalarColumn` variant, for `size_of`.
macro_rules! wire_elem {
    (U8) => { u8 }; (U16) => { u16 }; (U32) => { u32 }; (U64) => { u64 };
    (I8) => { i8 }; (I16) => { i16 }; (I32) => { i32 }; (I64) => { i64 };
    (F32) => { f32 }; (F64) => { f64 }; (TimestampUs) => { i64 };
}
use wire_elem;

/// Serialise `batch` by **appending** to `out` — straight into the frame buffer, no intermediate
/// allocation and no copy of the finished stream.
fn write_stream_into(schema: &Schema, batch: &RecordBatch, out: &mut Vec<u8>) {
    let mut writer =
        StreamWriter::try_new(out, schema).expect("frame stream writer construction");
    writer.write(batch).expect("frame stream write");
    writer.finish().expect("frame stream finish");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip_through_split() {
        let tiles = tiles_frame(&[5, 9], &[100, 3], &[100, 3], &[10, 3]);
        let subs = sub_cells_frame(&[], &[]);
        let names3 = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let points = points_frame(
            &[1, 2, 3],
            &[10, 20, 30],
            &[("w", ScalarColumn::U16(&[7, 8, 9])), ("n", ScalarColumn::Utf8(&names3))],
        );
        let trailer = trailer_frame(br#"{"stream_us":1}"#);

        let mut body = Vec::new();
        body.extend_from_slice(&tiles);
        body.extend_from_slice(&subs);
        body.extend_from_slice(&points);
        body.extend_from_slice(&trailer);

        let frames = split_frames(&body).expect("well-formed body splits");
        let kinds: Vec<u8> = frames.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec![FRAME_TILES, FRAME_SUB_CELLS, FRAME_POINTS, FRAME_TRAILER]);
        assert_eq!(frames[3].1, br#"{"stream_us":1}"#);
        // Each payload is a complete Arrow stream: decodable alone.
        for (kind, payload) in &frames[..3] {
            let cursor = std::io::Cursor::new(payload.to_vec());
            let reader = arrow::ipc::reader::StreamReader::try_new(cursor, None)
                .unwrap_or_else(|e| panic!("frame kind {kind} not a complete stream: {e}"));
            for batch in reader {
                batch.expect("frame batch decodes");
            }
        }
    }

    #[test]
    fn split_refuses_truncation_and_unknown_kinds() {
        let tiles = tiles_frame(&[1], &[1], &[1], &[1]);
        // Truncated payload: cut the last byte.
        let cut = &tiles[..tiles.len() - 1];
        assert!(matches!(
            split_frames(cut),
            Err(FrameError::TruncatedPayload { .. })
        ));
        // Truncated header: a lone kind byte.
        assert!(matches!(
            split_frames(&[FRAME_TILES]),
            Err(FrameError::TruncatedHeader { .. })
        ));
        // Unknown kind: refused, never skipped.
        let mut body = tiles.clone();
        body.push(9);
        body.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            split_frames(&body),
            Err(FrameError::UnknownKind { kind: 9, .. })
        ));
    }
}
