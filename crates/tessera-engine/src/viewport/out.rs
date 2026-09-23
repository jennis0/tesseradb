//! What a viewport answers with: its response types, its column buffers and its sink.

use super::*;

/// One declared-scalar value carried alongside a point, read from `ColumnsRef` rather than staged
/// for write.
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarOut {
    Bool(bool),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    /// Microseconds since the Unix epoch.
    TimestampUs(i64),
    Utf8(String),
}

/// One tile's count row. `visible` is the composed count: how many of this tile's items the
/// principal may see. `matched` is how many of those the request's filter admits, kept separate
/// so a filter cannot be mistaken for a permission change.
#[derive(Debug, Clone, PartialEq)]
pub struct TileCount {
    /// The tile's Morton prefix at the request's zoom depth.
    pub tile: u64,
    pub visible: u64,
    pub matched: u64,
    /// How many of this tile's points are in [`ViewportOut::points`]. Under the density rule the
    /// per-tile count cannot be recovered by arithmetic on `visible` alone, so this field carries
    /// it. Discloses nothing: derivable from each point's `x`/`y` and the published extent.
    pub served: u64,
    /// Of this tile's `matched`, how many also satisfy the request's `highlight`. Always present
    /// and equal to `matched` with no highlight. `highlighted ≤ matched ≤ visible` by construction.
    pub highlighted: u64,
}

/// The sampled points, column-major: one buffer per field, all of the same length. Column-major
/// because the wire wants it and the read is cheapest as it, measured at 944 ms row-major-then-
/// transpose against 51 ms gathered column-major over 10⁶ points in nineteen columns. No entity
/// id leaves the engine on this path, because none is stored. `tessera_ids`, `codes` and every
/// buffer in `scalars` are parallel: index *i* is one point in all of them, checked by
/// [`Self::len`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PointColumns {
    pub tessera_ids: Vec<u64>,
    /// Each point's position as the 64-bit Morton interleave of its two 32-bit fixed-point axes.
    /// Shifting right by `32 - 2·zoom` gives the containing tile.
    pub codes: Vec<u64>,
    /// One buffer per render scalar, in declaration order — see [`ViewportOut::scalar_names`]. A
    /// `filter`-only or blob-resident column has no slot here.
    pub scalars: Vec<ColumnBuf>,
    /// One column per layer this response served artifacts from: the deepest served artifact
    /// each point belongs to, or `None` — see [`crate::membership_column`]. Empty if none served.
    pub membership: Vec<crate::membership_column::MembershipColumn>,
    /// One bit per point: whether it satisfies the request's `highlight`. `None` where the
    /// request carried none — an absent column, not an all-false one. Sits after the render
    /// scalars and before the membership columns.
    pub highlighted: Option<Vec<bool>>,
}

impl PointColumns {
    pub fn len(&self) -> usize {
        self.tessera_ids.len()
    }

    /// `(tessera_id, code)` per point, in served order: the identity half of the response.
    pub fn iter(&self) -> impl Iterator<Item = (TesseraId, u64)> + '_ {
        self.tessera_ids
            .iter()
            .zip(&self.codes)
            .map(|(&id, &code)| (TesseraId::new(id), code))
    }

    pub fn is_empty(&self) -> bool {
        self.tessera_ids.is_empty()
    }

    /// Concatenate `other` onto this buffer, column by column. A type disagreement means two
    /// tiles read the same declared column at different types, which no well-formed bundle
    /// produces — see [`ColumnBuf::append`], which this defers to.
    pub fn append(
        &mut self,
        other: PointColumns,
    ) -> std::result::Result<(), (&'static str, &'static str)> {
        self.tessera_ids.extend(other.tessera_ids);
        self.codes.extend(other.codes);
        // Positional: index i is the same declared column on both sides.
        for (dst, src) in self.scalars.iter_mut().zip(other.scalars) {
            dst.append(src)?;
        }
        // Layer names are checked, not assumed: nothing else would catch a column appended under
        // another layer's name.
        for (dst, src) in self.membership.iter_mut().zip(other.membership) {
            assert_eq!(
                dst.layer, src.layer,
                "chunks of one response cannot disagree on their membership layers"
            );
            dst.ids.extend(src.ids);
        }
        // Presence is decided once for the response; a chunk cannot introduce or drop it.
        if let (Some(dst), Some(src)) = (self.highlighted.as_mut(), other.highlighted) {
            dst.extend(src);
        }
        Ok(())
    }

    /// Estimated wire bytes of these columns, the emit pass's flush threshold. A hint, not a
    /// contract: it decides where chunks split, never what they contain.
    pub fn wire_bytes_estimate(&self) -> usize {
        // `tessera_id` and `code`, both u64.
        let mut bytes = self.tessera_ids.len() * 16;
        for col in &self.scalars {
            bytes += col.wire_bytes_estimate();
        }
        // A u64 and a validity bit per point per membership column.
        bytes += self.membership.len()
            * (self.tessera_ids.len() * 8 + self.tessera_ids.len().div_ceil(8));
        // A bit per point, in Arrow's packed boolean buffer.
        if self.highlighted.is_some() {
            bytes += self.tessera_ids.len().div_ceil(8);
        }
        bytes
    }
}

/// A same-typed column of gathered scalar values, owned rather than borrowed: it outlives the
/// segment mappings any one tile read, since a response concatenates tiles from different segments.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnBuf {
    Bool(Vec<bool>),
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
    I8(Vec<i8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    TimestampUs(Vec<i64>),
    Utf8(Vec<String>),
}

/// Every scalar family paired with its element type, so this module's build, empty, append and
/// measure sites cannot drift apart. `Bool` and `Utf8` are hand-written at each site.
macro_rules! flat_families {
    ($mac:ident) => {
        $mac! {
            (U8, u8), (U16, u16), (U32, u32), (U64, u64),
            (I8, i8), (I16, i16), (I32, i32), (I64, i64),
            (F32, f32), (F64, f64), (TimestampUs, i64),
        }
    };
}
pub(super) use flat_families;

impl ColumnBuf {
    /// An empty buffer of the declared type, from the manifest rather than the first value seen:
    /// deriving it from the first gathered point would drop or shift columns on a wider tile.
    pub(super) fn empty(ty: ScalarType) -> Self {
        macro_rules! arms {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match ty {
                    $(ScalarType::$v => ColumnBuf::$v(Vec::new()),)*
                    ScalarType::Bool => ColumnBuf::Bool(Vec::new()),
                    // `render` on a keyword is refused at declaration, so this arm is
                    // unreachable, grouped with `Utf8` to avoid disagreeing with the writer.
                    ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => ColumnBuf::Utf8(Vec::new()),
                }
            };
        }
        flat_families!(arms)
    }

    pub fn len(&self) -> usize {
        macro_rules! arms {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match self {
                    $(ColumnBuf::$v(x) => x.len(),)*
                    ColumnBuf::Bool(x) => x.len(),
                    ColumnBuf::Utf8(x) => x.len(),
                }
            };
        }
        flat_families!(arms)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Concatenate `other` onto this buffer. A type disagreement means two tiles read the same
    /// declared column at different types, which no well-formed bundle produces. Refused rather
    /// than dropped: a short column is caught downstream by the wire layer's length assertion,
    /// but a wrong one is not caught anywhere else.
    fn append(
        &mut self,
        other: ColumnBuf,
    ) -> std::result::Result<(), (&'static str, &'static str)> {
        macro_rules! arms {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match (self, other) {
                    $((ColumnBuf::$v(dst), ColumnBuf::$v(src)) => { dst.extend(src); Ok(()) })*
                    (ColumnBuf::Bool(dst), ColumnBuf::Bool(src)) => { dst.extend(src); Ok(()) }
                    (ColumnBuf::Utf8(dst), ColumnBuf::Utf8(src)) => { dst.extend(src); Ok(()) }
                    (dst, src) => Err((dst.type_name(), src.type_name())),
                }
            };
        }
        flat_families!(arms)
    }

    pub fn type_name(&self) -> &'static str {
        macro_rules! arms {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match self {
                    $(ColumnBuf::$v(_) => stringify!($v),)*
                    ColumnBuf::Bool(_) => "Bool",
                    ColumnBuf::Utf8(_) => "Utf8",
                }
            };
        }
        flat_families!(arms)
    }

    /// This column's approximate wire size: element widths for the fixed families, a bitmap for
    /// `Bool`, offsets-plus-data for `Utf8`.
    fn wire_bytes_estimate(&self) -> usize {
        macro_rules! arms {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match self {
                    $(ColumnBuf::$v(x) => x.len() * std::mem::size_of::<$t>(),)*
                    ColumnBuf::Bool(x) => x.len().div_ceil(8),
                    ColumnBuf::Utf8(x) => {
                        4 * (x.len() + 1) + x.iter().map(|v| v.len()).sum::<usize>()
                    }
                }
            };
        }
        flat_families!(arms)
    }
}

/// One underlay sub-cell: a Morton prefix at depth `zoom + offset` and the exact number of
/// visible items inside it. Discloses nothing beyond what a viewport request at that zoom
/// already returns. The depth is not carried: an out-of-range offset is rejected, not clamped.
#[derive(Debug, Clone, PartialEq)]
pub struct SubCellCount {
    /// The sub-cell's Morton prefix at depth `zoom + offset`.
    pub cell: u64,
    pub count: u64,
}

/// The two coordinates a client keys its replica on. Both are opaque: minted here, echoed back,
/// compared for equality and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewCoordinates {
    /// Whether a held band may be rendered at all — the cache partition key: the idset, the
    /// auth-data hash, the mask fragment's identity and the view. Keying a cache more loosely
    /// than this serves one principal's authorised data to another.
    pub identity_key: [u8; 16],
    /// Whether a held band may be declared in a request: the identity key plus the watermark of
    /// the geometry served, the overlay version and the boot nonce.
    pub content_key: [u8; 16],
}

/// An artifact's two filter answers — `(matched, highlighted)`. Named as a pair because a
/// dependent inherits both or neither.
pub(super) type FilterBits = (Option<bool>, Option<bool>);

/// One artifact, as a viewport serves it. No ordinal, no declared size and no membership: a
/// declared size would be a corpus-wide count over items this principal may not see. There is no
/// reason-for-absence anywhere in the response, because an artifact that failed its criterion
/// must be indistinguishable from one that was never published.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactOut {
    /// The layer it belongs to. Always a name the principal reaches: the serving pass
    /// intersects with the session's resolved set before it looks at any membership.
    pub layer: String,
    /// Its opaque identifier — the only artifact address that crosses the trust boundary, and
    /// what a drill-down or a suppression later names.
    pub tessera_id: TesseraId,
    /// The publisher's own key, if they supplied one. Operator-chosen text, not corpus data.
    pub key: Option<String>,
    /// How many of this artifact's members this principal can see, never how many it has. The
    /// same number the existence criterion was tested against.
    pub masked_count: u64,
    /// The layer's declared derived properties, recomputed for this principal from the visible
    /// rows the count was taken over — see [`crate::derived`]. Empty where the layer declares
    /// none; describes only the members this viewer can see.
    pub derived: crate::derived::DerivedContent,
    /// This artifact's parents, only those also in this response, ascending by identifier. A
    /// tree's list is at most one long; a `dag` layer's may name several. An absent entry covers
    /// a root and a withheld parent alike, so distinguishing them cannot disclose a coarser
    /// grouping.
    pub parent_ids: Vec<TesseraId>,
    /// The artifact this one is attached to, named by the identifier this response served it
    /// under. `None` where attached to nothing, or on the identifier route. Always names a row in
    /// this same response; a dependent whose target is absent is dropped before this is filled.
    /// Never an entity id or a stored address.
    pub target: Option<TesseraId>,
    /// One content, entire: the first ranked description whose generating set the viewer
    /// contains completely. A viewer containing none gets no artifact, not this list empty.
    pub content: Vec<String>,
    /// The resolution a client draws this artifact at. On a levelled layer, the declared level:
    /// the same fact for every principal served it, indexing the level set `/v1/meta` publishes.
    /// On a treed layer, the response-local depth: the longest parent chain to this row in the
    /// forest the response's own `parent_ids` links form, after the budget cut and every other
    /// narrowing, so a re-rooted subtree's root reads 0. On a flat layer, 0.
    pub rung: u32,
    /// Whether the served shape's vertex budget cut vertices the request's depth alone would
    /// have kept. The same for every principal served the artifact, so it discloses nothing.
    pub shape_guard_fired: bool,
    /// Whether any member of this artifact the principal may see, inside the request's tiles,
    /// matches the request's filter. `None` means no question was asked, not no matches. A
    /// boolean, never a count: existence and `masked_count` stay anchored on the unfiltered
    /// visible set. Clipped to the viewport, unlike the count. A dependent carries its target's.
    pub matched: Option<bool>,
    /// The same bit for `all_of[filters, highlight]`: whether a member the principal may see,
    /// inside the request's tiles, satisfies both expressions. `None` where the request carried
    /// no `highlight`. Every rule [`Self::matched`] carries holds here unchanged.
    pub highlighted: Option<bool>,
}

/// The masked viewport response. No `serde` derive.
#[derive(Debug, Clone)]
pub struct ViewportOut {
    /// The coordinates a client keys its replica on. See [`ViewCoordinates`].
    pub coordinates: ViewCoordinates,
    /// The generation this response was answered from, which is one behind the live generation
    /// while a refresh has not yet replaced the session's projection. A client echoes it back.
    pub stamp: GenerationStamp,
    /// Whether the geometry moved since the stamp the request presented. `false` when no stamp
    /// was presented, or when the presented stamp equals this response's. Reports that the corpus
    /// moved, not that anything this principal can see moved: a viewer whose visible set is
    /// unchanged is still told the geometry advanced, which is not a disclosure.
    pub stale: bool,
    /// See [`ViewportHead::region`].
    pub region: Option<crate::region::RegionVerdict>,
    pub tiles: Vec<TileCount>,
    /// The annotation artifacts intersecting the request's tiles, with the count this
    /// principal's visible set generates — see [`ArtifactOut`]. Empty whether the principal
    /// reaches no layer, nothing intersects, or everything failed its existence criterion.
    pub artifacts: Vec<ArtifactOut>,
    /// The served points, column-major — see [`PointColumns`].
    pub points: PointColumns,
    /// The density underlay, when requested — empty otherwise. Only non-empty cells appear.
    pub sub_cells: Vec<SubCellCount>,
    /// The render columns' names, in declaration order, from the same generation this response's
    /// points were gathered from. Carried here rather than fetched separately via
    /// `Engine::meta()`, which would load the generation a second time for one request.
    pub scalar_names: Vec<String>,
    /// Per-stage breakdown, all zeros unless built with `bench-timing` (see [`crate::timing`]).
    /// Excluded from `PartialEq` — see the hand-written impl below.
    pub timings: StageTimings,
}

/// `PartialEq` ignoring `timings`, hand-written: a derived impl would make every `assert_eq!`
/// timing-dependent and flaky whenever `bench-timing` is enabled. `stale` needs no separate
/// check; `scalar_names` is included because agreeing on `points` already agrees on it.
impl PartialEq for ViewportOut {
    fn eq(&self, other: &Self) -> bool {
        self.stamp == other.stamp
            && self.stale == other.stale
            && self.tiles == other.tiles
            && self.points == other.points
            && self.sub_cells == other.sub_cells
            && self.scalar_names == other.scalar_names
    }
}

/// Everything about a viewport response that is known before the sweep runs. The HTTP headers
/// derive from it, which is why it is delivered first.
#[derive(Debug, Clone)]
pub struct ViewportHead {
    pub coordinates: ViewCoordinates,
    /// The geometry this response is answered from — what a client echoes back next time.
    pub stamp: GenerationStamp,
    /// See [`ViewportOut::stale`].
    pub stale: bool,
    /// The region leaves' verdict — `x-tessera-region` — or `None` where the request carried no
    /// region leaf. A function of the shapes and the grid alone, settled before any row is read.
    pub region: Option<crate::region::RegionVerdict>,
    /// The render-column schema, in declaration order, from the same generation the response is
    /// served from: names for the wire's column headers, types to seed empty columns. Never the
    /// full declaration — it pairs positionally with [`PointColumns::scalars`], and a wider list
    /// would serve one column's values under another's name.
    pub render_scalars: Vec<DeclaredScalar>,
    /// Whether the points frame carries a `highlighted` column, i.e. whether the request carried
    /// a `highlight`. Settled here, before the first byte: the frame's schema is fixed for the
    /// response and a chunk may not introduce or drop a column.
    pub highlighted: bool,
}

/// The sink told the producer to stop: the consumer is gone. The producer treats it as a
/// cancellation, not a fault.
#[derive(Debug)]
pub struct SinkClosed;

pub type SinkResult = std::result::Result<(), SinkClosed>;

/// Where [`Engine::viewport_stream`] delivers a response, in strict order: `head`, then
/// `counts`, then zero or more `points` chunks. The producer returning `Ok` is the completeness
/// signal; there is no `done` callback. Every callback may refuse with [`SinkClosed`], which
/// aborts the request as a cancellation: the remaining work is abandoned and the caller gets
/// [`EngineError::Cancelled`].
pub trait ViewportSink {
    /// Everything the response headers need. Exactly once, before any other callback.
    fn head(&mut self, head: ViewportHead) -> SinkResult;
    /// Sweep complete: every tile's counts, and the density underlay. Exactly once, before any
    /// points. `sub_cells` is `None` when the request did not ask for the underlay and `Some`
    /// (possibly empty) when it did.
    fn counts(&mut self, tiles: &[TileCount], sub_cells: Option<&[SubCellCount]>) -> SinkResult;
    /// The artifacts intersecting the request's tiles. At most once, after `counts`, never with
    /// an empty slice: an absent frame and an empty one carry the same information. Required
    /// rather than defaulted, so a consumer cannot compile against a response it never renders.
    fn artifacts(&mut self, artifacts: &[ArtifactOut]) -> SinkResult;
    /// One flush chunk: whole tiles' worth of points, in response order, ascending by
    /// `tessera_id` within each tile. Never called with an empty chunk.
    fn points(&mut self, chunk: PointColumns) -> SinkResult;
}

/// The columns every points chunk of one response carries: the render columns the head published,
/// and the membership resolver where the artifacts frame carried anything to resolve against. One
/// value because the two are decided together — `point_rows = "highlight"` empties both.
pub(super) struct PointSchema<'a> {
    pub(super) render_scalars: &'a [DeclaredScalar],
    pub(super) membership: Option<crate::membership_column::Resolved>,
}

/// The emit pass: gather and hand off, serial, in response order. A chunk is delivered once its
/// estimated wire size reaches `flush_bytes`, always at a whole-tile boundary.
pub(super) fn emit_points(
    swept: &[TileSweepOut<'_>],
    schema: &PointSchema<'_>,
    mask: &EffectiveMask,
    flush_bytes: usize,
    cancel: &Option<CancelToken>,
    probe: &mut Probe,
    sink: &mut dyn ViewportSink,
) -> Result<()> {
    // Seeded from the declaration, not from whichever tile arrives first: a narrower first tile
    // must not fix the column set. Re-seeded identically at each flush.
    let seed = || PointColumns {
        tessera_ids: Vec::new(),
        codes: Vec::new(),
        scalars: schema
            .render_scalars
            .iter()
            .map(|d| ColumnBuf::empty(d.arrow_type))
            .collect(),
        membership: schema
            .membership
            .as_ref()
            .map(|m| m.empty_columns())
            .unwrap_or_default(),
        highlighted: mask.has_highlight().then(Vec::new),
    };
    let mut buf = seed();
    let mut buf_bytes = 0usize;
    for ts in swept {
        // Per-tile cancellation checkpoint, so an abandoned stream stops within one tile.
        check_cancelled(cancel)?;
        let mut stats = TileProbe::new();
        let parts = SelectionParts::new(&ts.parts);
        let mut tile_points = gather_tile_columns(&parts, &ts.rows, schema.render_scalars)?;
        if let Some(membership) = &schema.membership {
            tile_points.membership = membership.columns_for(&ts.rows);
        }
        // One `contains` per served point against the crossed highlight set — at most
        // `k_max_marks` lookups for the whole response.
        if mask.has_highlight() {
            tile_points.highlighted =
                Some(ts.rows.iter().map(|&row| mask.is_highlighted(row)).collect());
        }
        stats.count(|t| &mut t.points_gathered, tile_points.len() as u64);
        buf_bytes += tile_points.wire_bytes_estimate();
        if let Err((want, got)) = buf.append(tile_points) {
            return Err(EngineError::Malformed(format!(
                "two tiles of one response hold the same declared column at different \
                 types ({want} and {got}); the bundle's segments disagree about it"
            )));
        }
        // Brackets gather-and-append only: the flush must not inflate this CPU-cost figure.
        stats.lap(|t| &mut t.gather_ns);
        stats.t.fold_into(&mut probe.t);
        // `!buf.is_empty()`: a zero-row buffer can still carry estimate bytes, so a `k = 0`
        // counts-only request would otherwise emit empty points frames. `MAX_POINTS_FRAME_BYTES`
        // stops a huge `flush_bytes` accumulating a frame past the wire's u32 length field.
        if buf_bytes >= flush_bytes.min(MAX_POINTS_FRAME_BYTES) && !buf.is_empty() {
            let chunk = std::mem::replace(&mut buf, seed());
            buf_bytes = 0;
            sink.points(chunk)
                .map_err(|SinkClosed| EngineError::Cancelled)?;
        }
    }
    if !buf.is_empty() {
        sink.points(buf)
            .map_err(|SinkClosed| EngineError::Cancelled)?;
    }
    Ok(())
}

/// [`Engine::viewport`]'s sink: collect everything, so the batch caller sees exactly what a
/// streaming consumer would have seen, concatenated.
#[derive(Default)]
pub(super) struct CollectSink {
    pub(super) head: Option<ViewportHead>,
    pub(super) tiles: Vec<TileCount>,
    pub(super) sub_cells: Vec<SubCellCount>,
    pub(super) artifacts: Vec<ArtifactOut>,
    pub(super) points: Option<PointColumns>,
}

impl ViewportSink for CollectSink {
    fn head(&mut self, head: ViewportHead) -> SinkResult {
        self.head = Some(head);
        Ok(())
    }

    fn counts(&mut self, tiles: &[TileCount], sub_cells: Option<&[SubCellCount]>) -> SinkResult {
        self.tiles = tiles.to_vec();
        self.sub_cells = sub_cells.unwrap_or_default().to_vec();
        Ok(())
    }

    fn artifacts(&mut self, artifacts: &[ArtifactOut]) -> SinkResult {
        self.artifacts = artifacts.to_vec();
        Ok(())
    }

    fn points(&mut self, chunk: PointColumns) -> SinkResult {
        match &mut self.points {
            None => self.points = Some(chunk),
            Some(points) => points
                .append(chunk)
                // Unreachable: a per-chunk type disagreement is refused inside the gather before
                // it could reach here.
                .expect("chunks of one response cannot disagree on a column's type"),
        }
        Ok(())
    }
}
