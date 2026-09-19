//! Frames of the streamed `POST /v1/viewport` response.
//!
//! A body is a sequence of frames: a `u8` kind, a `u32` little-endian payload length, then the
//! payload. Every payload decodes on its own, so no reader walks Arrow messages to find a
//! boundary.
//!
//! ```text
//! kind 1  tiles      Arrow stream; exactly one, first
//! kind 5  artifacts  Arrow stream, full or identity projection; at most one, after tiles and
//!                    before any points, absent when no artifact is served
//! kind 2  sub-cells  Arrow stream; present exactly when the request asked for the underlay,
//!                    with zero rows when it is empty
//! kind 3  points     Arrow stream; zero or more, whole tiles per frame, concatenating to the
//!                    full points set
//! kind 4  trailer    JSON; exactly one, last. Its presence says the body is complete
//! ```
//!
//! Clients index the tiles batch and the artifacts batch's fixed columns by position, so a new
//! column is appended and never inserted.
//!
//! Nothing here takes an entity id: identities arrive as `tessera_id: u64` columns.

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BooleanArray, DictionaryArray, Float32Array, Float64Array, Int16Array,
    Int32Array, Int64Array, Int8Array, ListBuilder, StringArray, StringBuilder,
    TimestampMicrosecondArray, UInt16Array, UInt32Array, UInt32Builder, UInt64Array,
    UInt64Builder, UInt8Array,
};
use arrow::datatypes::{DataType, Field, Schema, UInt16Type};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;

/// Frame kinds. A reader refuses a kind it does not know.
pub const FRAME_TILES: u8 = 1;
pub const FRAME_SUB_CELLS: u8 = 2;
pub const FRAME_POINTS: u8 = 3;
pub const FRAME_TRAILER: u8 = 4;
pub const FRAME_ARTIFACTS: u8 = 5;

pub const FRAME_HEADER_BYTES: usize = 5;

/// One declared scalar column of a points frame, as long as the frame's `tessera_ids`.
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
    /// Microseconds since the Unix epoch, sent as Arrow `Timestamp(Microsecond, None)`.
    TimestampUs(&'a [i64]),
    Utf8(&'a [String]),
}

impl ScalarColumn<'_> {
    fn array(&self) -> ArrayRef {
        macro_rules! copied {
            ($array:ident, $values:expr) => {
                Arc::new($array::from_iter_values($values.iter().copied()))
            };
        }
        match self {
            ScalarColumn::Bool(s) => bool_column(s),
            ScalarColumn::U8(s) => copied!(UInt8Array, s),
            ScalarColumn::U16(s) => copied!(UInt16Array, s),
            ScalarColumn::U32(s) => copied!(UInt32Array, s),
            ScalarColumn::U64(s) => copied!(UInt64Array, s),
            ScalarColumn::I8(s) => copied!(Int8Array, s),
            ScalarColumn::I16(s) => copied!(Int16Array, s),
            ScalarColumn::I32(s) => copied!(Int32Array, s),
            ScalarColumn::I64(s) => copied!(Int64Array, s),
            ScalarColumn::F32(s) => copied!(Float32Array, s),
            ScalarColumn::F64(s) => copied!(Float64Array, s),
            ScalarColumn::TimestampUs(s) => copied!(TimestampMicrosecondArray, s),
            ScalarColumn::Utf8(s) => Arc::new(StringArray::from_iter_values(s.iter())),
        }
    }
}

fn u64_column(values: &[u64]) -> ArrayRef {
    Arc::new(UInt64Array::from_iter_values(values.iter().copied()))
}

fn bool_column(values: &[bool]) -> ArrayRef {
    Arc::new(BooleanArray::from(values.to_vec()))
}

fn required(name: &str, column: &ArrayRef) -> Field {
    Field::new(name, column.data_type().clone(), false)
}

fn nullable(name: &str, column: &ArrayRef) -> Field {
    Field::new(name, column.data_type().clone(), true)
}

/// One frame whose payload `write` appends to the buffer.
///
/// # Panics
///
/// Panics if the payload is longer than `u32::MAX` bytes.
fn frame(kind: u8, payload_hint: usize, write: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_BYTES + payload_hint);
    out.push(kind);
    out.extend_from_slice(&[0u8; 4]);
    write(&mut out);
    let len = u32::try_from(out.len() - FRAME_HEADER_BYTES).expect("frame payload exceeds u32");
    out[1..FRAME_HEADER_BYTES].copy_from_slice(&len.to_le_bytes());
    out
}

/// One frame holding `columns` as a single-batch Arrow stream.
///
/// # Panics
///
/// Panics if the columns differ in length or a non-nullable field holds a null.
fn arrow_frame(kind: u8, columns: Vec<(Field, ArrayRef)>) -> Vec<u8> {
    let (fields, arrays): (Vec<Field>, Vec<ArrayRef>) = columns.into_iter().unzip();
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), arrays).expect("frame columns form a batch");
    // Sizes the buffer once; it changes no byte written. The stream holds the arrays' buffers, a
    // validity bitmap for every column whether or not it has nulls, and the schema and batch
    // metadata.
    let per_column = batch.num_rows().div_ceil(8) + 256;
    let hint = batch.get_array_memory_size() + batch.num_columns() * per_column + 1024;
    frame(kind, hint, |out| {
        let mut writer = StreamWriter::try_new(out, &schema).expect("frame stream begins");
        writer.write(&batch).expect("frame batch writes");
        writer.finish().expect("frame stream finishes");
    })
}

/// The tiles frame: one row per non-empty tile. `highlighted` equals `matched` where the request
/// carried no highlight.
///
/// # Panics
///
/// Panics if the columns differ in length.
pub fn tiles_frame(
    tile: &[u64],
    visible: &[u64],
    matched: &[u64],
    served: &[u64],
    highlighted: &[u64],
) -> Vec<u8> {
    let columns = [
        ("tile", tile),
        ("visible", visible),
        ("matched", matched),
        ("served", served),
        ("highlighted", highlighted),
    ];
    arrow_frame(
        FRAME_TILES,
        columns
            .into_iter()
            .map(|(name, values)| {
                let column = u64_column(values);
                (required(name, &column), column)
            })
            .collect(),
    )
}

/// The sub-cells frame: masked counts over Morton cells at the depth the request named. The
/// depth is the request's own, so it is not echoed.
///
/// # Panics
///
/// Panics if the columns differ in length.
pub fn sub_cells_frame(cells: &[u64], counts: &[u64]) -> Vec<u8> {
    let (cells, counts) = (u64_column(cells), u64_column(counts));
    arrow_frame(
        FRAME_SUB_CELLS,
        vec![
            (required("cell", &cells), cells),
            (required("count", &counts), counts),
        ],
    )
}

/// One served artifact.
///
/// The row carries nothing that describes items the viewer cannot see: no position within its
/// level, no corpus-wide membership size, no membership, and no reason another artifact is
/// missing.
#[derive(Debug, Clone, Default)]
pub struct ArtifactRow<'a> {
    pub layer: &'a str,
    pub tessera_id: u64,
    pub key: Option<&'a str>,
    pub masked_count: u64,
    /// Over the members this viewer can see, in the grid units of the points frame's `code`.
    /// Present when the layer declares a centroid.
    pub centroid: Option<[f64; 2]>,
    /// `[x_min, y_min, x_max, y_max]`, in the same units and on the same terms.
    pub bbox: Option<[u32; 4]>,
    /// The drawn geometry, in the same units: parts, then rings, then vertices. A part's first
    /// ring is its outline and the rest are holes.
    pub shape: Option<&'a [Vec<Vec<[u32; 2]>>]>,
    /// One value per content kind the layer declares, in declaration order.
    pub content: &'a [String],
    /// The parents that are rows of this same frame, ascending. Empty for a root and equally for
    /// an artifact whose parents were not served, so the list never reveals a parent the viewer
    /// was not sent.
    pub parent_ids: Vec<u64>,
    /// The resolution to draw at: the declared level on a levelled layer, the depth within this
    /// response's parent links on a treed layer, 0 on a flat one.
    pub rung: u32,
    /// Whether a visible member inside the request's tiles passes the request's filter. `None`
    /// when the request had no filter.
    pub matched: Option<bool>,
    /// The same for the filter and the highlight together. `None` when the request had no
    /// highlight.
    pub highlighted: Option<bool>,
    /// The `tessera_id` of the row in this frame that this artifact is attached to, such as a
    /// label's cluster. `None` for an artifact attached to nothing.
    pub target: Option<u64>,
}

/// The `layer` column, dictionary-encoded with `u16` keys in order of first appearance.
fn layer_column(rows: &[ArtifactRow<'_>]) -> (Field, ArrayRef) {
    let mut index: HashMap<&str, u16> = HashMap::new();
    let mut values: Vec<&str> = Vec::new();
    let keys: UInt16Array = rows
        .iter()
        .map(|r| {
            Some(*index.entry(r.layer).or_insert_with(|| {
                let next = u16::try_from(values.len())
                    .expect("more than 65,536 distinct layers in one response");
                values.push(r.layer);
                next
            }))
        })
        .collect();
    let values = Arc::new(StringArray::from_iter_values(values));
    let column: ArrayRef = Arc::new(
        DictionaryArray::<UInt16Type>::try_new(keys, values).expect("keys index the values"),
    );
    (required("layer", &column), column)
}

/// The columns both artifact projections carry after `layer`.
struct ArtifactIdentity {
    tessera_id: (Field, ArrayRef),
    rung: (Field, ArrayRef),
    matched: (Field, ArrayRef),
    highlighted: (Field, ArrayRef),
}

impl ArtifactIdentity {
    fn of(rows: &[ArtifactRow<'_>]) -> Self {
        let tessera_id: ArrayRef = Arc::new(UInt64Array::from_iter_values(
            rows.iter().map(|r| r.tessera_id),
        ));
        let rung: ArrayRef = Arc::new(UInt32Array::from_iter_values(rows.iter().map(|r| r.rung)));
        let matched: ArrayRef = Arc::new(BooleanArray::from_iter(rows.iter().map(|r| r.matched)));
        let highlighted: ArrayRef =
            Arc::new(BooleanArray::from_iter(rows.iter().map(|r| r.highlighted)));
        ArtifactIdentity {
            tessera_id: (required("tessera_id", &tessera_id), tessera_id),
            rung: (required("rung", &rung), rung),
            matched: (nullable("matched", &matched), matched),
            highlighted: (nullable("highlighted", &highlighted), highlighted),
        }
    }
}

/// The artifacts frame in the full projection.
///
/// Sixteen columns, `layer` to `target`, sit at fixed positions. `shape_x` and `shape_y` follow
/// them when some row has a shape and are absent from the schema otherwise. A null in a nullable
/// column says the layer declares no such property, or the request asked no such question; an
/// artifact that cannot be served is absent whole.
///
/// A shape is two `List<List<List<UInt32>>>` columns, one per axis, with the same nesting in
/// both.
pub fn artifacts_frame(rows: &[ArtifactRow<'_>]) -> Vec<u8> {
    fn optional<A, T>(name: &str, values: impl Iterator<Item = Option<T>>) -> (Field, ArrayRef)
    where
        A: Array + FromIterator<Option<T>> + 'static,
    {
        let column: ArrayRef = Arc::new(values.collect::<A>());
        (nullable(name, &column), column)
    }
    // A list builder makes its item field nullable unless told otherwise, and the schema must
    // match it.
    let item = |data_type| Arc::new(Field::new("item", data_type, false));

    let mut content = ListBuilder::new(StringBuilder::new()).with_field(item(DataType::Utf8));
    let mut parent_ids = ListBuilder::new(UInt64Builder::new()).with_field(item(DataType::UInt64));
    for row in rows {
        for value in row.content {
            content.values().append_value(value);
        }
        content.append(true);
        parent_ids.values().append_slice(&row.parent_ids);
        parent_ids.append(true);
    }
    let (content, parent_ids): (ArrayRef, ArrayRef) =
        (Arc::new(content.finish()), Arc::new(parent_ids.finish()));
    let masked_count: ArrayRef = Arc::new(UInt64Array::from_iter_values(
        rows.iter().map(|r| r.masked_count),
    ));
    let identity = ArtifactIdentity::of(rows);
    let bbox = |name, at: usize| {
        optional::<UInt32Array, _>(name, rows.iter().map(move |r| r.bbox.map(|b| b[at])))
    };

    let mut columns = vec![
        layer_column(rows),
        identity.tessera_id,
        optional::<StringArray, _>("key", rows.iter().map(|r| r.key)),
        (required("masked_count", &masked_count), masked_count),
        optional::<Float64Array, _>("centroid_x", rows.iter().map(|r| r.centroid.map(|c| c[0]))),
        optional::<Float64Array, _>("centroid_y", rows.iter().map(|r| r.centroid.map(|c| c[1]))),
        bbox("box_min_x", 0),
        bbox("box_min_y", 1),
        bbox("box_max_x", 2),
        bbox("box_max_y", 3),
        (required("content", &content), content),
        (required("parent_ids", &parent_ids), parent_ids),
        identity.rung,
        identity.matched,
        identity.highlighted,
        optional::<UInt64Array, _>("target", rows.iter().map(|r| r.target)),
    ];
    if rows.iter().any(|r| r.shape.is_some()) {
        columns.push(shape_column("shape_x", rows, 0));
        columns.push(shape_column("shape_y", rows, 1));
    }
    arrow_frame(FRAME_ARTIFACTS, columns)
}

fn shape_column(name: &str, rows: &[ArtifactRow<'_>], axis: usize) -> (Field, ArrayRef) {
    let item = |data_type| Arc::new(Field::new("item", data_type, false));
    let vertex = item(DataType::UInt32);
    let ring = item(DataType::List(vertex.clone()));
    let part = item(DataType::List(ring.clone()));
    let mut shapes = ListBuilder::new(
        ListBuilder::new(ListBuilder::new(UInt32Builder::new()).with_field(vertex))
            .with_field(ring),
    )
    .with_field(part);
    for row in rows {
        let Some(parts) = row.shape else {
            shapes.append_null();
            continue;
        };
        for rings in parts {
            for vertices in rings {
                for vertex in vertices {
                    shapes.values().values().values().append_value(vertex[axis]);
                }
                shapes.values().values().append(true);
            }
            shapes.values().append(true);
        }
        shapes.append(true);
    }
    let column: ArrayRef = Arc::new(shapes.finish());
    (nullable(name, &column), column)
}

/// The artifacts frame in the identity projection: the rows [`artifacts_frame`] would carry, as
/// `layer`, `tessera_id`, `rung`, `matched` and `highlighted` only. The other columns are absent
/// from the schema.
pub fn artifacts_identity_frame(rows: &[ArtifactRow<'_>]) -> Vec<u8> {
    let identity = ArtifactIdentity::of(rows);
    arrow_frame(
        FRAME_ARTIFACTS,
        vec![
            layer_column(rows),
            identity.tessera_id,
            identity.rung,
            identity.matched,
            identity.highlighted,
        ],
    )
}

/// The points frame in the highlight projection: the points [`points_frame`] would carry, as
/// `tessera_id` and `highlighted` only.
///
/// # Panics
///
/// Panics if the columns differ in length.
pub fn points_highlight_frame(tessera_ids: &[u64], highlighted: &[bool]) -> Vec<u8> {
    let (tessera_ids, highlighted) = (u64_column(tessera_ids), bool_column(highlighted));
    arrow_frame(
        FRAME_POINTS,
        vec![
            (required("tessera_id", &tessera_ids), tessera_ids),
            (required("highlighted", &highlighted), highlighted),
        ],
    )
}

fn membership_column_name(layer: &str) -> String {
    format!("membership:{layer}")
}

/// The points frame. Columns, in order: `tessera_id`; `code`, the Morton interleave of the
/// point's two 32-bit grid coordinates; the declared scalars; `highlighted`, when the request
/// carried a highlight; then one nullable [`membership_column_name`] column per layer, holding
/// the `tessera_id` of the deepest served artifact the point belongs to.
///
/// # Panics
///
/// Panics if the columns differ in length.
pub fn points_frame(
    tessera_ids: &[u64],
    codes: &[u64],
    scalars: &[(&str, ScalarColumn)],
    highlighted: Option<&[bool]>,
    membership: &[(&str, &[Option<u64>])],
) -> Vec<u8> {
    let (tessera_ids, codes) = (u64_column(tessera_ids), u64_column(codes));
    let mut columns = vec![
        (required("tessera_id", &tessera_ids), tessera_ids),
        (required("code", &codes), codes),
    ];
    for (name, scalar) in scalars {
        let column = scalar.array();
        columns.push((required(name, &column), column));
    }
    if let Some(highlighted) = highlighted {
        let column = bool_column(highlighted);
        columns.push((required("highlighted", &column), column));
    }
    for (layer, ids) in membership {
        let column: ArrayRef = Arc::new(UInt64Array::from_iter(ids.iter().copied()));
        columns.push((nullable(&membership_column_name(layer), &column), column));
    }
    arrow_frame(FRAME_POINTS, columns)
}

/// The trailer frame around the caller's JSON.
pub fn trailer_frame(json: &[u8]) -> Vec<u8> {
    frame(FRAME_TRAILER, json.len(), |out| out.extend_from_slice(json))
}

/// Split a body into `(kind, payload)` frames. A short header, a payload running past the end
/// of the body, or an unknown kind is an error, so a truncated body never reads as a shorter
/// response.
pub fn split_frames(body: &[u8]) -> Result<Vec<(u8, &[u8])>, FrameError> {
    let mut frames = Vec::new();
    let mut at = 0usize;
    while at < body.len() {
        let Some(header) = body[at..].first_chunk::<FRAME_HEADER_BYTES>() else {
            return Err(FrameError::TruncatedHeader { at });
        };
        let kind = header[0];
        if !matches!(
            kind,
            FRAME_TILES | FRAME_SUB_CELLS | FRAME_POINTS | FRAME_TRAILER | FRAME_ARTIFACTS
        ) {
            return Err(FrameError::UnknownKind { kind, at });
        }
        let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let start = at + FRAME_HEADER_BYTES;
        let payload = start
            .checked_add(len)
            .and_then(|end| body.get(start..end))
            .ok_or(FrameError::TruncatedPayload { at })?;
        frames.push((kind, payload));
        at = start + len;
    }
    Ok(frames)
}

/// Why [`split_frames`] refused a body. `at` is the byte offset of the frame.
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
                write!(
                    f,
                    "frame at byte {at} claims a payload past the end of the body"
                )
            }
            FrameError::UnknownKind { kind, at } => {
                write!(f, "unknown frame kind {kind} at byte {at}")
            }
        }
    }
}

impl std::error::Error for FrameError {}
