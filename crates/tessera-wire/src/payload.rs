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
//! kind 3  points     Arrow IPC stream (tessera_id: uint64, code: uint64, ...scalars,
//!                    ...membership:<layer>); zero or more, whole tiles per frame, concatenating
//!                    to the full points stream
//! kind 4  trailer    JSON; exactly one, last — its presence is the completeness signal
//! kind 5  artifacts  Arrow IPC stream, one row per served artifact, in the request's projection
//!                    (full, or the identity four-column schema); at most one, after tiles and
//!                    before any points, ABSENT when nothing is served
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
    ArrayRef, BooleanArray, DictionaryArray, Float32Array, Float64Array, Int16Array, Int32Array,
    Int64Array, Int8Array, ListBuilder, StringArray, StringBuilder, TimestampMicrosecondArray,
    UInt16Array, UInt32Array, UInt32Builder, UInt64Array, UInt8Array,
};
use arrow::datatypes::{DataType, Field, Schema, UInt16Type};
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
    pub key: Option<&'a str>,
    pub masked_count: u64,
    /// Derived geometry, in the **grid units** the points frame's `code` is built from — the
    /// client needs no quantisation extent to draw either. Each is present exactly when the
    /// artifact's layer declared it, and describes the members *this* principal can see.
    pub centroid: Option<[f64; 2]>,
    /// `[qx_min, qy_min, qx_max, qy_max]`.
    pub bbox: Option<[u32; 4]>,
    /// The hull's rings, each a closed ring of vertices in grid units. **Several rings, one per
    /// separated group of the visible members** — a membership that is two clouds is drawn as two
    /// shapes rather than as one polygon over the gap between them.
    pub hull: Option<&'a [Vec<[u32; 2]>]>,
    /// The publisher's supplied content — **one entry of the ranked `contents`, entire**, one value per kind the layer
    /// declares, in declaration order. Empty where the layer declares none.
    ///
    /// A viewer receiving this artifact contains that entry's generating set completely; one
    /// who contains none receives no artifact at all rather than this list empty. So there is no
    /// *content withheld* state on this wire and no shape to express one.
    pub content: &'a [String],
    /// The identifier of this artifact's parent, **and only ever one that is in this same
    /// response**.
    ///
    /// This is the structure a client needs to nest what it draws, or to filter to one subtree
    /// while still drawing the rest of the map. It is what a hierarchy is *for* on a levelled
    /// layer, whose edges carry containment rather than a ladder to coarsen along.
    ///
    /// **Null is the fail-closed answer and covers two different situations deliberately.** The
    /// artifact may be a root; or its parent may exist and not have been served — below its own
    /// criterion for this viewer, suppressed, or dropped by the frontier. Naming a parent in the
    /// second case would disclose that a coarser grouping exists which this principal is not
    /// cleared to see, so the two are one value here and a client must read null as *no parent in
    /// this response* rather than as *no parent*.
    pub parent_id: Option<u64>,
    /// **The resolution a client draws this artifact at**, computed the right way for its layer's
    /// kind so no client has to know which way that is (`artifact-fetch-protocol.md` §5.3, the
    /// rung ruling; it renamed and re-meant the `level` column this field carried until then).
    ///
    /// On a **levelled** layer it is the declared level — a fact about the artifact, the same
    /// number for every principal served it, indexing the level set `/v1/meta` publishes. On a
    /// **treed** layer it is the response-local parent-chain depth: the depth of this row in the
    /// forest the response's own `parent_id` links form, after the budget cut, so a re-rooted
    /// subtree's root reads 0. On a **flat** layer it is 0. The two derivations disagree on real
    /// data — a tiered layer's edges skip levels and its roots arrive parentless — which is why
    /// the server computes the right one per layer rather than leaving every client to pick
    /// (and one shipped client to pick wrongly, which is what happened).
    pub rung: u32,
    /// **Whether this artifact holds a member the request's filter admits** — one that this
    /// principal may see and that lies inside the request's tiles
    /// ([decision 0104](../../../docs/decisions/0104-a-filter-answers-a-boolean-per-served-artifact.md)).
    ///
    /// `None` where the request carried no filter: there was no question, and a `false` would
    /// answer one that was never asked. A whole frame of nulls is what an unfiltered response
    /// carries.
    ///
    /// **A boolean rather than a filtered count**, so that nothing here competes with
    /// `masked_count` for what the client is showing. Existence and the count are anchored on
    /// `M_auth` whatever the filter did, so this is the only field of the row a filter moves.
    ///
    /// **It is clipped to the request's tiles where the count is not**: the count and the geometry
    /// describe the whole visible membership, this the part of it in view, that being the extent
    /// every filter-crossing route can answer over.
    pub matched: Option<bool>,
}

/// The `layer` column, dictionary-encoded — one utf8 value per distinct layer, a `u16` key per
/// row (`artifact-fetch-protocol.md` §8: the name is ~14% of every full row written plain, and a
/// response's distinct layers are a handful).
///
/// Keys are minted in first-appearance order; `u16` bounds a response at 65,536 distinct layers,
/// which is a caller bug long before it is a limit.
fn layer_dictionary(rows: &[ArtifactRow<'_>]) -> ArrayRef {
    let mut index: std::collections::HashMap<&str, u16> = std::collections::HashMap::new();
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
    Arc::new(
        DictionaryArray::<UInt16Type>::try_new(keys, values)
            .expect("layer dictionary construction"),
    )
}

/// `layer`'s schema field — `Dictionary(UInt16, Utf8)`, shared by both artifact frame shapes so
/// they cannot disagree about the encoding.
fn layer_field() -> Field {
    Field::new(
        "layer",
        DataType::Dictionary(Box::new(DataType::UInt16), Box::new(DataType::Utf8)),
        false,
    )
}

/// The kind-5 artifacts frame: one row per served artifact, in the full (default) projection.
///
/// `masked_count` is `UInt64` and `tessera_id` is `UInt64`, matching the points frame's `tessera_id`
/// column so a client's decoder has one identifier type across the response.
///
/// **Column positions are contract for the fixed prefix; optional columns trail.** Decoders that
/// index this batch positionally exist, so the fourteen fixed columns — `layer` through `matched`
/// — sit at fixed positions, and the only columns whose presence varies, `hull_x`/`hull_y`, come
/// after all of them (`artifact-fetch-protocol.md` §8; this superseded the earlier
/// appended-last-per-revision rule when the hull columns moved to the tail). The frame kinds are
/// unchanged and `api_version` stays at 1 (contracts §3.2: no published deployment exists and
/// every in-repo reader moves in lockstep, decision 0048) — the reordering is the loud break, a
/// positional decoder finding `content` where `hull_x` sat rather than one column's values under
/// another's meaning of the same type.
///
/// **`layer` is dictionary-encoded** — see [`layer_dictionary`].
///
/// **The geometry columns are nullable and per-row**, because one response carries artifacts from
/// several layers and layers declare different vocabularies. A null is *this layer declares no
/// centroid*; it is never *withheld*, since an artifact whose content could not be served is
/// absent entirely (decision 0076).
///
/// **The hull columns are present exactly when some row carries a hull** — i.e. when a served
/// layer declares one — **and absent from the schema otherwise.** An absent column is
/// distinguishable from a null one, so decision 0076's rule (a null means *this layer declares no
/// such property*, never *withheld*) gains no third reading: when the columns are present, a
/// per-row null keeps exactly its 0076 meaning. When present they travel as two
/// `List<List<UInt32>>` columns — one per axis so that a client reads an axis without a stride,
/// and nested so that the ring boundaries are in the type rather than in a convention. A reader
/// written against the single-ring shape descends one level, finds a list where it expected a
/// `UInt32`, and fails; a flat encoding with a separate offsets column would let the same reader
/// concatenate every ring into one polygon and draw a chord between them, silently. The two axes
/// carry the same ring structure by construction, and a decoder that zips them should check the
/// lengths agree rather than assume it (`contracts.md` §3.2).
///
/// # Panics
///
/// Panics on Arrow construction failure.
pub fn artifacts_frame(rows: &[ArtifactRow<'_>]) -> Vec<u8> {
    // One definition of each of the hull's two nested element fields, used by the schema and by the
    // builders below: a `ListBuilder` builds a **nullable** item field by default, and neither a
    // vertex nor a ring is ever null — a hull is a list of rings of positions, or it is absent
    // entirely. Declaring them twice is how the two drift into the mismatch Arrow then refuses at
    // batch construction.
    let vertex = || Arc::new(Field::new("item", DataType::UInt32, false));
    let ring = || Arc::new(Field::new("item", DataType::List(vertex()), false));

    // A row carries a hull exactly when its layer declares one (a declared hull over a served
    // artifact always computes — a served artifact has a visible member), so *any row carries one*
    // and *a served layer declares one* are the same test, and it is decidable here from the rows
    // alone.
    let hulls = rows.iter().any(|r| r.hull.is_some());

    let mut fields = vec![
        layer_field(),
        Field::new("tessera_id", DataType::UInt64, false),
        // A publisher need not supply a key.
        Field::new("key", DataType::Utf8, true),
        Field::new("masked_count", DataType::UInt64, false),
        Field::new("centroid_x", DataType::Float64, true),
        Field::new("centroid_y", DataType::Float64, true),
        Field::new("box_min_x", DataType::UInt32, true),
        Field::new("box_min_y", DataType::UInt32, true),
        Field::new("box_max_x", DataType::UInt32, true),
        Field::new("box_max_y", DataType::UInt32, true),
        // One list per artifact, positional to its layer's declared kinds. A list rather than a
        // column per kind, because one response carries artifacts from several layers and their
        // declarations differ; the client reads the kinds from `/v1/meta` and zips.
        Field::new(
            "content",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, false))),
            false,
        ),
        Field::new("parent_id", DataType::UInt64, true),
        // Non-nullable: every artifact has a rung — a levelled layer's declared level, a treed
        // layer's response-local chain depth, a flat layer's 0 (see [`ArtifactRow::rung`]). There
        // is no *withheld* state to express — an artifact whose content could not be served is
        // absent whole (decision 0076).
        Field::new("rung", DataType::UInt32, false),
        // Last of the fixed columns, and **nullable because null is a value here**: an unfiltered
        // request asked no question, and a `false` would answer one. So the column is all-null on
        // every response that carried no `filter`, rather than absent (decision 0104).
        Field::new("matched", DataType::Boolean, true),
    ];
    if hulls {
        fields.push(Field::new("hull_x", DataType::List(ring()), true));
        fields.push(Field::new("hull_y", DataType::List(ring()), true));
    }
    let schema = Arc::new(Schema::new(fields));

    let mut content = ListBuilder::new(StringBuilder::new()).with_field(Arc::new(Field::new(
        "item",
        DataType::Utf8,
        false,
    )));
    for row in rows {
        for value in row.content {
            content.values().append_value(value);
        }
        // Never null: an artifact with no supplied content has an *empty* list, because its layer
        // declares none. A null would have to mean something else, and there is nothing else.
        content.append(true);
    }

    let mut columns: Vec<ArrayRef> = vec![
        layer_dictionary(rows),
        Arc::new(UInt64Array::from_iter_values(
            rows.iter().map(|r| r.tessera_id),
        )),
        Arc::new(StringArray::from_iter(rows.iter().map(|r| r.key))),
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
        Arc::new(content.finish()),
        Arc::new(UInt64Array::from_iter(rows.iter().map(|r| r.parent_id))),
        Arc::new(UInt32Array::from_iter_values(rows.iter().map(|r| r.rung))),
        Arc::new(BooleanArray::from_iter(rows.iter().map(|r| r.matched))),
    ];
    if hulls {
        let mut hull_x =
            ListBuilder::new(ListBuilder::new(UInt32Builder::new()).with_field(vertex()))
                .with_field(ring());
        let mut hull_y =
            ListBuilder::new(ListBuilder::new(UInt32Builder::new()).with_field(vertex()))
                .with_field(ring());
        for row in rows {
            match row.hull {
                Some(rings) => {
                    for r in rings {
                        for v in r {
                            hull_x.values().values().append_value(v[0]);
                            hull_y.values().values().append_value(v[1]);
                        }
                        hull_x.values().append(true);
                        hull_y.values().append(true);
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
        columns.push(Arc::new(hull_x.finish()));
        columns.push(Arc::new(hull_y.finish()));
    }
    let batch =
        RecordBatch::try_new(schema.clone(), columns).expect("artifacts frame batch construction");

    let mut out = Vec::with_capacity(FRAME_HEADER_BYTES + rows.len() * 96 + 1024);
    let len_at = begin_frame(&mut out, FRAME_ARTIFACTS);
    write_stream_into(&schema, &batch, &mut out);
    patch_frame_len(&mut out, len_at);
    out
}

/// The kind-5 artifacts frame in the **identity projection** — `artifact_rows: "identity"`,
/// `artifact-fetch-protocol.md` §5.2: the same rows as [`artifacts_frame`] would carry, in a
/// fixed four-column schema of `layer` (dictionary-encoded), `tessera_id`, `rung`, `matched`.
///
/// **The row set, the `matched` bits and the `rung` values are identical under either
/// projection; only the columns change.** That sentence is the contract: no parent can dangle
/// and the points frame's membership columns still name identifiers present here, because no row
/// was dropped — and the response is a column subset of what the same caller's identical request
/// would have been served, which is why the projection discloses nothing. The payload columns are
/// **absent from the schema, not null**, so decision 0076's null rule gains no third reading.
///
/// Measured at 13.6 B/row against 125 for the pre-dictionary full row (§8 of the design; the
/// size-regression test in `tests/wire.rs` holds the bounds).
///
/// # Panics
///
/// Panics on Arrow construction failure.
pub fn artifacts_identity_frame(rows: &[ArtifactRow<'_>]) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        layer_field(),
        Field::new("tessera_id", DataType::UInt64, false),
        Field::new("rung", DataType::UInt32, false),
        Field::new("matched", DataType::Boolean, true),
    ]));
    let columns: Vec<ArrayRef> = vec![
        layer_dictionary(rows),
        Arc::new(UInt64Array::from_iter_values(
            rows.iter().map(|r| r.tessera_id),
        )),
        Arc::new(UInt32Array::from_iter_values(rows.iter().map(|r| r.rung))),
        Arc::new(BooleanArray::from_iter(rows.iter().map(|r| r.matched))),
    ];
    let batch = RecordBatch::try_new(schema.clone(), columns)
        .expect("identity artifacts frame batch construction");

    let mut out = Vec::with_capacity(FRAME_HEADER_BYTES + rows.len() * 16 + 1024);
    let len_at = begin_frame(&mut out, FRAME_ARTIFACTS);
    write_stream_into(&schema, &batch, &mut out);
    patch_frame_len(&mut out, len_at);
    out
}

/// The points-frame column name a layer's membership column travels under: `membership:<layer>`.
///
/// One definition, shared by the writer and every Rust reader, so the two cannot spell it
/// differently. The Python oracle and the TS client carry their own, deliberately (contracts
/// §0.2's second-reader posture).
pub fn membership_column_name(layer: &str) -> String {
    format!("membership:{layer}")
}

/// The kind-3 points frame: one `tessera_id` and one 64-bit position `code` per point, plus the
/// declared scalars in schema order, then one nullable membership column per layer.
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
/// `membership` is the per-point membership column of design D12 (`client-components.md`
/// §5.10): per layer, the `tessera_id` of the **deepest served** artifact the point belongs to
/// in this response, `null` where no served artifact holds it. Plain `Option<u64>` slices —
/// nothing here knows what an artifact is, and the engine has already bounded every value to the
/// response's own artifacts frame. Columns are named [`membership_column_name`] and appended
/// after the scalars, so a decoder that indexes scalars positionally is unaffected. Empty when
/// the request resolved to no layers or the response served no artifact: an absent column and an
/// all-null one would say the same thing, and only one of them costs bytes.
///
/// # Panics
///
/// Panics on any column length mismatch or Arrow construction failure.
pub fn points_frame(
    tessera_ids: &[u64],
    codes: &[u64],
    scalars: &[(&str, ScalarColumn)],
    membership: &[(&str, &[Option<u64>])],
) -> Vec<u8> {
    let points = tessera_ids.len();
    assert_eq!(points, codes.len(), "points/codes length mismatch");
    for (name, col) in scalars {
        let len = wire_column_len(col);
        assert_eq!(points, len, "scalar column {name:?} length mismatch");
    }
    for (layer, col) in membership {
        assert_eq!(points, col.len(), "membership column {layer:?} length mismatch");
    }

    let mut fields = vec![
        Field::new("tessera_id", DataType::UInt64, false),
        Field::new("code", DataType::UInt64, false),
    ];
    for (name, col) in scalars {
        fields.push(Field::new(*name, wire_column_type(col), false));
    }
    // **After the render scalars, one per named layer in request order, and nullable** — the only
    // nullable columns in this frame. `membership:` prefixes the layer name so a declared scalar
    // can never collide with it: a layer is path-shaped (`clusters/hdbscan`) and a scalar name is
    // an identifier, but the prefix is what makes that structural rather than a coincidence.
    for (layer, _) in membership {
        fields.push(Field::new(
            membership_column_name(layer),
            DataType::UInt64,
            true,
        ));
    }
    let schema = Arc::new(Schema::new(fields));

    let mut columns: Vec<ArrayRef> = Vec::with_capacity(2 + scalars.len() + membership.len());
    columns.push(Arc::new(UInt64Array::from_iter_values(
        tessera_ids.iter().copied(),
    )));
    columns.push(Arc::new(UInt64Array::from_iter_values(codes.iter().copied())));
    for (_, col) in scalars {
        columns.push(wire_column_array(col));
    }
    for (_, col) in membership {
        columns.push(Arc::new(UInt64Array::from_iter(col.iter().copied())));
    }
    let batch =
        RecordBatch::try_new(schema.clone(), columns).expect("points frame batch construction");

    let mut out = Vec::with_capacity(
        FRAME_HEADER_BYTES + estimated_points_bytes(points, scalars, membership.len()),
    );
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
fn estimated_points_bytes(
    points: usize,
    scalars: &[(&str, ScalarColumn)],
    membership_columns: usize,
) -> usize {
    // `tessera_id` and `code`, both u64.
    let mut bytes = points * 16;
    // A membership column is a u64 per point plus its validity bitmap, and the same per-buffer
    // padding and per-field descriptor a scalar pays.
    bytes += membership_columns * (points * 8 + points.div_ceil(8) + 64 + 128);
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
    use arrow::array::Array;

    #[test]
    fn frames_roundtrip_through_split() {
        let tiles = tiles_frame(&[5, 9], &[100, 3], &[100, 3], &[10, 3]);
        let subs = sub_cells_frame(&[], &[]);
        let names3 = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let points = points_frame(
            &[1, 2, 3],
            &[10, 20, 30],
            &[("w", ScalarColumn::U16(&[7, 8, 9])), ("n", ScalarColumn::Utf8(&names3))],
            &[],
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

    /// Decode one points payload into `(schema, batches)`.
    fn decode_points(frame: &[u8]) -> (Arc<Schema>, Vec<RecordBatch>) {
        let frames = split_frames(frame).expect("a single well-formed frame");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, FRAME_POINTS);
        let cursor = std::io::Cursor::new(frames[0].1.to_vec());
        let reader = arrow::ipc::reader::StreamReader::try_new(cursor, None).unwrap();
        let schema = reader.schema();
        let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
        (schema, batches)
    }

    /// **Zero, one and two membership columns**, and the nullability of each survives the IPC
    /// round trip. The column set is what a client keys its colouring on, so its position (after
    /// the scalars, in request order), its name and its nulls are each pinned here.
    #[test]
    fn membership_columns_are_named_nullable_and_after_the_scalars() {
        let ids = [1u64, 2, 3];
        let codes = [10u64, 20, 30];
        let scalars = [("w", ScalarColumn::U16(&[7, 8, 9]))];

        // Zero: the schema is exactly the scalars'.
        let (schema, _) = decode_points(&points_frame(&ids, &codes, &scalars, &[]));
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, vec!["tessera_id", "code", "w"]);

        // One and two: appended, named, nullable, in the order given.
        let a: [Option<u64>; 3] = [Some(100), None, Some(300)];
        let b: [Option<u64>; 3] = [None, None, Some(999)];
        let frame = points_frame(
            &ids,
            &codes,
            &scalars,
            &[("clusters/hdbscan", &a), ("regions/admin", &b)],
        );
        let (schema, batches) = decode_points(&frame);
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(
            names,
            vec![
                "tessera_id",
                "code",
                "w",
                "membership:clusters/hdbscan",
                "membership:regions/admin"
            ]
        );
        assert!(!schema.field(2).is_nullable(), "a scalar is never null");
        assert!(schema.field(3).is_nullable());
        assert!(schema.field(4).is_nullable());
        assert_eq!(schema.field(3).data_type(), &DataType::UInt64);

        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        let read = |i: usize| -> Vec<Option<u64>> {
            let col = batch
                .column(i)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap();
            (0..col.len())
                .map(|r| col.is_valid(r).then(|| col.value(r)))
                .collect()
        };
        assert_eq!(read(3), a.to_vec());
        assert_eq!(read(4), b.to_vec());
    }

    /// The size estimate is a hint, but a hint that ignores a column is a reallocation on every
    /// flush; the estimate must grow by at least the column's data.
    #[test]
    fn the_size_estimate_covers_the_membership_columns() {
        let points = 1000;
        let scalars = [("w", ScalarColumn::U16(&[0u16; 1000]))];
        let without = estimated_points_bytes(points, &scalars, 0);
        let with_two = estimated_points_bytes(points, &scalars, 2);
        assert!(with_two >= without + 2 * (points * 8 + points.div_ceil(8)));

        // And the estimate is an over-estimate of the real payload, which is what lets the frame
        // buffer be sized once.
        let ids = vec![0u64; points];
        let col: Vec<Option<u64>> = (0..points)
            .map(|i| (i % 3 != 0).then_some(i as u64))
            .collect();
        let frame = points_frame(&ids, &ids, &scalars, &[("a", &col), ("b", &col)]);
        assert!(
            frame.len() <= FRAME_HEADER_BYTES + with_two,
            "estimate {with_two} short of the {} bytes written",
            frame.len()
        );
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
