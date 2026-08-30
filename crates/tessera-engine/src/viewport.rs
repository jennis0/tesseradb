//! The masked viewport query (design §2.6, retrieve steps 1–9).
//!
//! [`Engine::viewport`] loads the generation pointer exactly once — which is the whole of I11's
//! within-request rule, and the reason nothing here has to resolve anything against a superseded
//! generation (`crate::geometry`) — gets-or-builds the session's cached row projection, composes
//! the effective mask (I1), and for every tile touching `bbox` counts and selects.
//!
//! Selection is §7.2's real definition — floor ∪ threshold ∪ cap over `tessera_id`, evaluated
//! inside the mask (I7). It lives in [`crate::select`], which carries the definition, the nesting
//! argument and the two evaluation routes. This module's job is only to resolve the per-request
//! parameters (notably θ's anchor, which **must** be the composed visible cardinality — see
//! [`crate::select::Threshold::anchor`] for the I2 argument) and to gather what selection returns.
//!
//! **The request is two phases, split at the seam streaming needs** (`streamed-serving.md`):
//! the **sweep** — count, select and underlay per tile, no gather — and the **emit** — a serial
//! pass over the swept tiles in response order that gathers and hands off flush-sized
//! [`PointColumns`] chunks. [`Engine::viewport_stream`] is the producer; [`Engine::viewport`] is
//! the same producer run into a collecting sink, returning the batch [`ViewportOut`].
//!
//! **D-D/D-F: the sweep's per-tile body runs on the engine's shared rayon pool.**
//! [`tile_sweep`] is the pure per-tile function — no `&self`, no engine method, nothing
//! but `&`-borrowed inputs and an owned result — that the sweep fans out over every
//! tile via `self.pool.install(|| tiles.par_iter().zip(..).map(tile_sweep).collect::<Vec<_>>())`.
//! The collect target is deliberately `Vec<Result<Option<TileSweepOut>, EngineError>>`, never
//! `Result<Vec<TileSweepOut>, EngineError>`: a `Result` collect drops rayon onto its unindexed
//! reduce path, and this response's byte-equality claim (same request, same bytes, at
//! `compute_threads = 1` or `8`) would then rest on an implementation detail of that reduce
//! strategy rather than on anything stated here. Collecting `Vec<Result<..>>` stays on rayon's
//! *indexed* collect path, so the output vector's order equals the input tiles' order **by
//! construction** — not by convention, not by observation of the current rayon version. A serial,
//! in-order fold over that vector then short-circuits on the first
//! `Err` (D-C's per-tile cancellation check, moved inside `tile_sweep` — see its doc) and
//! collects `tile_counts`/`sub_cells` plus each tile's selected rows for the emit pass.
//! **The emit pass is deliberately serial** — obviously correct first, and its wall cost
//! overlaps transmission under streaming; if a measurement shows it binding, parallel
//! gather-ahead inside the emit loop is a contained change.
//!
//! **Calibration task: below [`SERIAL_FALLBACK_MAX_ROWS`], the fan-out above does not run at
//! all.** Measured (2.42M-fixture, w=10 grant, zoom 8) at 2.97x-13x slower at
//! `compute_threads = default` than at `compute_threads = 1` for a typical small viewport — the
//! `pool.install` fan-out's own entry/scheduling cost dominates the ~µs of real per-tile work a
//! sparse, ~256-tile request produces. `Engine::viewport` instead folds `tile_sweep` serially,
//! in tile order, producing the identical `Vec<Result<Option<TileResult>>>` shape the fold below
//! already consumes — so the fold, and therefore the response, is unaffected by which branch ran.
//! See [`SERIAL_FALLBACK_MAX_ROWS`]'s doc for the predictor argument and the sweep data, and the
//! calibration report for the full method.

use std::ops::Range;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use rayon::prelude::*;
use sha2::{Digest, Sha256};

use tessera_authz::FrozenFragment;
use tessera_spatial::tiler::ScalarType;
use tessera_spatial::{tiles_for_bbox, tiles_for_bbox_count, Bounds, Projection, Tile};
use tessera_store::manifest::{DeclaredScalar, Quantisation};
use tessera_store::read::{ScalarSlice, SegmentData};
use tessera_store::vocabulary::Vocabularies;
use tessera_store::{tile_ranges_all, tile_ranges_within};
use tessera_types::layer::ComputedProperty;
use tessera_types::{EntityId, GenerationStamp, RowId, TesseraId, API_VERSION};

use crate::cache::{CacheWaitEnded, Peek, RowProjectionKey, SessionGeometry};
use crate::cancel::CancelToken;
use crate::compose::{compose, visible_to, EffectiveMask, FilterRows, MaskedSet, RowProjection};
use crate::filter::{Endpoint, Family, FilterOperand, Scalar};
use crate::membership_column::{ServedLayer, ServedLevel};
use crate::select::{SelectParams, Selection, SelectionPart, SelectionParts, Threshold};
use crate::session::{Engine, EngineError, Result, Session};
use crate::timing::{Probe, StageTimings, TileProbe, TileStats};
use crate::Generation;

/// One declared-scalar value carried alongside a point (mirrors `tessera_spatial::ScalarValue`,
/// but on the *output* side — read from `ColumnsRef`, not staged for write).
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

/// One tile's count row.
///
/// `visible` is the composed count — how many of this tile's items the principal may see — and
/// `matched` how many of those the request's filter admits. They are equal on an unfiltered
/// request, and deliberately separate fields: collapsing them would make a filter read as a
/// permission change, and would put a filter-dependent quantity where §7.1 discloses an exact
/// composed one.
#[derive(Debug, Clone, PartialEq)]
pub struct TileCount {
    /// The tile's Morton prefix at the request's zoom depth.
    pub tile: u64,
    pub visible: u64,
    pub matched: u64,
    /// How many of this tile's points are in [`ViewportOut::points`] — §7.2's `m(T)`.
    ///
    /// **Why it is here.** `points` is a flat concatenation in tile order, and every reader used to
    /// recover the per-tile grouping arithmetically as `min(k, visible)`. Under the density rule the
    /// per-tile count is `min(min(cap, max(k_min, C_θ)), visible)`, which that arithmetic cannot
    /// reproduce — so the differential oracle could not split the points batch, and a client could
    /// not truncate per tile to its own budget, which is what the nesting argument's
    /// client-truncation clause requires.
    ///
    /// **It is a convenience, not a new capability, and the distinction matters.** The grouping was
    /// always recoverable without it: every point carries `x`/`y`, `GET /v1/meta` publishes the
    /// quantisation extents, and `morton_of(x, y, extent) >> (32 − 2·zoom)` is the containing tile
    /// (contracts §2.5) — the reference oracle already recomputes exactly that. So `served`
    /// discloses nothing: it removes a recomputation from every client. Do not let this field be
    /// cited later as precedent that some *other* quantity must go on the wire because it is
    /// otherwise underivable.
    pub served: u64,
}

/// The sampled points, column-major: one buffer per field, all of the same length.
///
/// **Column-major because that is what the wire wants and what the read is cheapest as.** The
/// row-major shape this replaced built one `Vec<ScalarOut>` per point and the server then
/// transposed it, which at 10⁶ points across nineteen columns was a million small heap
/// allocations and a second full pass over every value. Measured
/// (`tessera-bench --bin gather_shape`, 10⁶ rows in 62-row tiles, nineteen columns): 944 ms
/// row-major-then-transpose against 51 ms gathered column-major.
///
/// **I10, strengthened (contracts r6):** no entity ID leaves the engine on this path, because
/// none is stored. `columns.arrow` carries `tessera_id` at the row, so the gather reads the
/// identity it is allowed to show and cannot read the one it is not. Entity IDs survive only in
/// entity-space structures and as `permutation.bin`'s index — never as a value on any path
/// reaching `tessera-wire`.
///
/// **The three vectors are parallel and must stay so.** Index *i* of `tessera_ids`, of `codes`
/// and of every buffer in `scalars` is one point. Nothing enforces that in the type, so every
/// producer here appends to all of them for every gathered row, and [`Self::len`] is the
/// cross-check the wire layer asserts against.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PointColumns {
    pub tessera_ids: Vec<u64>,
    /// Each point's position as the 64-bit Morton interleave of its two 32-bit fixed-point axes:
    /// the row's cell code in the high half, its stored residual in the low. Deinterleaving and
    /// scaling against the extent `/v1/meta` publishes recovers the coordinates; shifting right
    /// by `32 - 2·zoom` gives the containing tile without recomputing anything (contracts §3.2).
    pub codes: Vec<u64>,
    /// One buffer per **render** scalar, **in declaration order and always the render list's
    /// length** — see [`ViewportOut::scalar_names`], which is the parallel name list. Never the
    /// full declaration: a `filter`-only or blob-resident column occupies no slot in a segment's
    /// tail (contracts §2.6), so a buffer under its name could only be invented values.
    pub scalars: Vec<ColumnBuf>,
    /// One column per layer this response served artifacts from, in the response's layer order:
    /// the deepest served artifact each point belongs to, or `None` — see
    /// [`crate::membership_column`]. Empty when no artifact was served. Parallel to the three
    /// buffers above, and named by the chunk rather than by the head because which layers get a
    /// column is not known until the artifact pass has run, which is after the head is delivered.
    pub membership: Vec<crate::membership_column::MembershipColumn>,
}

impl PointColumns {
    pub fn len(&self) -> usize {
        self.tessera_ids.len()
    }

    /// `(tessera_id, code)` per point, in served order — the identity half of the response,
    /// without the scalar tail.
    ///
    /// The two vectors are parallel by construction (see this type's doc), so zipping them is the
    /// row view; anything wanting a *scalar* alongside indexes the column at the same position.
    pub fn iter(&self) -> impl Iterator<Item = (TesseraId, u64)> + '_ {
        self.tessera_ids
            .iter()
            .zip(&self.codes)
            .map(|(&id, &code)| (TesseraId::new(id), code))
    }

    pub fn is_empty(&self) -> bool {
        self.tessera_ids.is_empty()
    }

    /// Concatenate `other` onto this buffer, column by column — the response's in-order
    /// accumulation, shared by the emit pass's flush buffer and any collecting
    /// [`ViewportSink`].
    ///
    /// A type disagreement means two tiles read the same declared column at different types,
    /// which no well-formed bundle produces — see [`ColumnBuf::append`], which this defers to.
    pub fn append(
        &mut self,
        other: PointColumns,
    ) -> std::result::Result<(), (&'static str, &'static str)> {
        self.tessera_ids.extend(other.tessera_ids);
        self.codes.extend(other.codes);
        // Positional: both sides were built from the same declared-scalar list, so index i is
        // the same column on both. A length disagreement between the two `scalars` vectors is a
        // producer bug caught by the zip running short — the appended columns then fail the
        // wire layer's length assertion rather than silently dropping a column.
        for (dst, src) in self.scalars.iter_mut().zip(other.scalars) {
            dst.append(src)?;
        }
        // The same positional rule for the membership columns, and the same reason: every chunk
        // of one response is resolved against the same served layers in the same order. The
        // layer names are checked rather than assumed, because unlike a scalar's type nothing
        // downstream would catch a column appended under another layer's name.
        for (dst, src) in self.membership.iter_mut().zip(other.membership) {
            assert_eq!(
                dst.layer, src.layer,
                "chunks of one response cannot disagree on their membership layers"
            );
            dst.ids.extend(src.ids);
        }
        Ok(())
    }

    /// Estimated wire bytes of these columns as a points frame payload — the emit pass's flush
    /// threshold. Deterministic (a pure function of the data), and a hint rather than a
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
        bytes
    }
}

/// A same-typed column of gathered scalar values.
///
/// Owned rather than borrowed: it outlives the segment mappings any one tile read, because a
/// response concatenates tiles that may come from different segments.
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

/// Every scalar family paired with its element type, so the four places this module builds,
/// empties, appends and measures a column cannot drift apart. `Bool` and `Utf8` are hand-written
/// at each site: neither is stored as a flat slice of itself.
macro_rules! flat_families {
    ($mac:ident) => {
        $mac! {
            (U8, u8), (U16, u16), (U32, u32), (U64, u64),
            (I8, i8), (I16, i16), (I32, i32), (I64, i64),
            (F32, f32), (F64, f64), (TimestampUs, i64),
        }
    };
}

impl ColumnBuf {
    /// An empty buffer of the declared type.
    ///
    /// **Typed from the manifest's declaration, never from the first value seen.** Deriving the
    /// column set from the first gathered point is how a request whose first tile came from a
    /// narrower segment silently drops a column, or shifts every later one left; the declaration
    /// is the same for every tile by construction.
    fn empty(ty: ScalarType) -> Self {
        macro_rules! arms {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match ty {
                    $(ScalarType::$v => ColumnBuf::$v(Vec::new()),)*
                    ScalarType::Bool => ColumnBuf::Bool(Vec::new()),
                    // **The whole render path treats a keyword as its bytes.** The dictionary and
                    // the ordinal are the filter index's, not the hot column's, and `render` on a
                    // keyword is refused at the declaration — so this arm is unreachable, and it
                    // is stated with `utf8` rather than apart so that the segment writer's type
                    // (`store::write::arrow_type_of`) and this reader cannot come to disagree.
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

    /// Concatenate `other` onto this buffer — the per-column half of the response's in-order fold.
    ///
    /// A type disagreement means two tiles read the same declared column at different types, which
    /// no well-formed bundle produces (the write end's `gather_scalars` refuses it) and which
    /// would otherwise append values under a name that does not describe them. Refused rather
    /// than dropped: a short column is caught downstream by the wire layer's length assertion,
    /// but a *wrong* one is not caught anywhere.
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

    /// This column's approximate wire size — element widths for the fixed families, a bitmap
    /// for `Bool`, offsets-plus-data for `Utf8`. Mirrors `tessera-wire`'s buffer-sizing
    /// arithmetic without depending on that crate (the dependency edge runs the other way).
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

/// One §3.3 underlay sub-cell: a Morton prefix at depth `zoom + offset`, and the exact number of
/// visible items inside it.
///
/// **I2 no-op, and here is the argument rather than the assertion.** A depth-`d+s` sub-cell count is
/// exactly what a `zoom = d+s` viewport request already returns — §7.1 gives the exact masked count
/// of any tile at any zoom. The underlay saves round-trips and discloses no quantity a viewer could
/// not already obtain in one request. Omitting empty sub-cells conveys `count == 0`, itself a masked
/// count, exactly as the existing whole-tile skip does. Differencing across zooms or pans yields
/// only differences of masked counts.
///
/// The depth is not carried: it is `zoom + offset` from the request, and an out-of-range offset is
/// rejected rather than clamped, so the caller always knows it.
#[derive(Debug, Clone, PartialEq)]
pub struct SubCellCount {
    /// The sub-cell's Morton prefix at depth `zoom + offset`.
    pub cell: u64,
    pub count: u64,
}

/// Which annotation layers a viewport answers for.
///
/// Two shapes and no third: the empty list is *none* and costs nothing, and there is no value
/// meaning *the default*, so a caller who did not think about layers cannot pay for all of them
/// by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerSelection<'a> {
    /// Every layer this principal reaches.
    All,
    /// These, intersected with what the principal reaches — never unioned. Empty is none.
    Named(&'a [&'a str]),
}

/// Which of a levelled layer's declared resolutions a viewport answers for.
///
/// **Three shapes, and the default is the declaration's own** — unlike [`LayerSelection`], whose
/// default is *none* because the artifact pass is the expensive one to opt into. Here the pass has
/// already been paid for by naming the layer, and what is left is which rungs of it to answer at.
/// The costly answer is *every level*, and it is the one a caller must ask for by name.
///
/// **Why the declaration decides rather than the client.** A layer declares a zoom range per level
/// (`configuration.md`'s `[[layer.levels]]`), `/v1/meta` publishes it, and a request already
/// carries the depth it is asking at — three facts that until now were never joined, so a client
/// following the published map paid for every level and drew one. The map stays the client's to
/// override; what changes is that ignoring it is now the deliberate act rather than the accidental
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelSelection<'a> {
    /// The levels whose declared zoom range contains the request's depth.
    ///
    /// **A layer that declares no range on any level yields every level** — the levelled layer whose
    /// author stated titles and no scales. A level with no range of its own, in a layer where others
    /// have one, is served at every depth: it has no scale to be outside of, and inventing one for
    /// it would drop artifacts on a guess.
    Declared,
    /// Every level the layer holds, whatever the depth asked at.
    All,
    /// Exactly these, intersected with what the layer holds — never unioned. A level the layer does
    /// not hold is absent from the answer rather than a refusal, by the same route an unreachable
    /// layer name is: naming a level is not a way to learn whether it exists.
    ///
    /// **Empty is none**, as `layers: []` is: a caller who names no level has asked for no artifacts
    /// from any layer that declares levels. It is reachable only deliberately — the *absent* request
    /// field is [`LevelSelection::Declared`] and not this.
    Named(&'a [u32]),
}

/// Which of a layer's **declared** computed properties a viewport answers for.
///
/// **The property this is built to preserve: a request may narrow the declaration and can never
/// widen it.** Every form below is intersected with what the layer declared, so asking for `hull`
/// on a layer that declares none yields none, and asking for less is never a route to more. The
/// closure rule is untouched — whatever is computed is still a function of `membership ∩ M_auth`
/// and nothing else (`annotations.md` §4.2) — so this is a **cost** control of exactly the kind the
/// declaration itself is, moved one step closer to the request that pays for it.
///
/// **Why the request needs a say at all.** The declaration is per layer and the drawing is per
/// artifact: a client draws a hull for the one artifact under the pointer and centroids for the
/// other 196, and with only a layer-level declaration it had to be served 197 hulls to draw one.
/// Measured on `clusters/hdbscan` over the 2.42M corpus, that was 94% of a `k = 0` artifacts
/// request (`artifact-shapes.md` §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputedSelection<'a> {
    /// Everything the layer declared — the absent request field, and what every client received
    /// before there was a field.
    Declared,
    /// Exactly these, intersected with the declaration. **Empty is none**: a caller who names no
    /// property has asked for counts and no geometry, which is a real request and not a mistake.
    Named(&'a [ComputedProperty]),
}

impl ComputedSelection<'_> {
    /// Whether a declared property is answered for.
    pub(crate) fn selects(&self, property: ComputedProperty) -> bool {
        match self {
            ComputedSelection::Declared => true,
            ComputedSelection::Named(names) => names.contains(&property),
        }
    }
}

/// Which columns of the artifacts frame a viewport answers with — `artifact-fetch-protocol.md`
/// §5.2's projection, the one wire affordance of that design.
///
/// **The row set, the `matched` bits and the `rung` values are identical under either value;
/// only the columns change.** Candidacy, the verdict, the cut and the filter probe run
/// identically — what [`ArtifactRows::Identity`] skips is payload *production* only: derived
/// geometry ([`crate::derived::compute`]), the key lookup, and the materialisation of supplied
/// content (its *servability* is still tested, because an artifact whose content cannot be
/// served is withheld, and a projection must not resurrect it). The skip is a CPU saving and
/// nothing else; a projection that altered selection would break §5.2's contract sentence and
/// with it every cross-reference in the response.
///
/// It discloses nothing: an identity response is a column subset of what the same caller's
/// identical request would have been served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArtifactRows {
    /// Every column — the default, and the answer a caller who has read nothing receives.
    #[default]
    Full,
    /// `layer`, `tessera_id`, `rung`, `matched` — for the caller that already holds the payload
    /// columns and wants this filter's bits over the same rows.
    Identity,
}

/// Whether one level of one layer is answered for.
///
/// Split out of the serving loop so the rule is readable on its own and a test can state it
/// directly: the loop's job is to skip, and this is what it skips on.
pub(crate) fn level_is_selected(
    selection: LevelSelection<'_>,
    declared: &[tessera_types::layer::LevelDeclaration],
    level: u32,
    zoom: u8,
) -> bool {
    // **A layer that declares no levels is not selectable, in any of the three forms.** A treed or
    // flat layer sits entirely at level 0 (decision 0082) and a level number names nothing about it,
    // so a request naming levels for the tiered layer beside it must not blank it. Without this the
    // uniform reading — the selection applies to every layer named — makes `levels: [1]` alongside
    // `layers: "all"` serve nothing at all from every clustering in the deployment.
    if declared.is_empty() {
        return true;
    }
    match selection {
        LevelSelection::All => true,
        LevelSelection::Named(levels) => levels.contains(&level),
        LevelSelection::Declared => {
            // Nothing declared a scale, so there is no map to follow and every level answers. This
            // is the treed and flat case, and also the levelled layer whose author declared titles
            // and no ranges.
            if !declared.iter().any(|d| d.zoom.is_some()) {
                return true;
            }
            match declared.iter().find(|d| d.level == level) {
                // Declared, so the range decides.
                Some(d) => match d.zoom {
                    Some((lo, hi)) => (lo..=hi).contains(&u32::from(zoom)),
                    // A level with no range of its own in a layer that has them: no scale to be
                    // outside of.
                    None => true,
                },
                // A run with no declaration behind it — level 0 of a treed layer reached through a
                // layer that also declares levels cannot happen, but a run beyond the declared
                // list would otherwise vanish silently.
                None => true,
            }
        }
    }
}

/// One `/v1/viewport` request, as the engine sees it.
///
/// A struct rather than a positional argument list: the query is the system's main entry point and
/// keeps acquiring parameters (`served`, the §3.3 underlay, and the §8.2 filter contract next), so
/// naming them at the call site keeps both the signature and every caller readable as it grows.
/// Construct with [`ViewportRequest::new`] and add the optional parts.
#[derive(Debug, Clone)]
pub struct ViewportRequest<'a> {
    /// A view id from `GET /v1/meta`.
    pub view: &'a str,
    /// Tile depth, 0–16.
    pub zoom: u8,
    /// `[x0, y0, x1, y1]` in the bundle's declared extent. Ignored when `tiles` is present.
    pub bbox: [f64; 4],
    /// The exact tiles to answer for, as depth-`zoom` Morton prefixes — in place of deriving them
    /// from `bbox`.
    ///
    /// **This is how a client with a replica elides.** A tile it can prove it already holds is
    /// simply absent from the list, and the engine then does no range derivation, no counting, no
    /// selection scan and no gather for it — which is the only mechanism that makes server work
    /// scale with what is *new* rather than with viewport area (`delta-serving.md` §1). A bbox
    /// spends the full per-tile pipeline on every tile it spans whether the client needs it or not.
    ///
    /// **The naive path is unaffected.** A request carrying no list is answered from `bbox` exactly
    /// as before, self-contained, with no declaration logic anywhere in the client
    /// (`client-interaction.md` §5's REPLACE default).
    ///
    /// Deduplicated by the caller boundary before it reaches here (first occurrence kept, order
    /// preserved): a repeated tile would be served — and drawn — twice. **The list's order is the
    /// response's order** — the tiles batch reports in it and the points stream concatenates in
    /// it — which is how a streaming client orders its own arrival sequence (centre-out, say)
    /// with no server-side ordering policy at all (`streamed-serving.md` §3). The range
    /// derivation is order-independent (`tile_ranges_all` sweeps in Morton order internally and
    /// writes back positionally), so an arbitrary order costs nothing.
    pub tiles: Option<&'a [u64]>,
    /// The client's per-tile mark budget. Clamped to `max_k` (the machine ceiling) and then to
    /// `k_max_marks` (§7.2's cap clause).
    ///
    /// **Must be non-decreasing as the client zooms in.** §7.2's nesting property holds for a fixed
    /// cap; lowering `k` on descent forfeits it and marks will pop out. The engine sees one request
    /// at a time and cannot enforce this — see [`crate::select`]'s module doc.
    pub k: usize,
    /// The stamp of the response the client is currently holding, echoed back.
    ///
    /// **Advisory, and the request is answered from live geometry regardless.** It never selects a
    /// generation, never expires and never produces an error; all it does is set
    /// [`ViewportOut::stale`] when the live geometry has moved since. Presenting a stamp from a
    /// superseded generation — or from a superseded *prefix* — is an ordinary request with an
    /// ordinary answer (`geometry-pinning.md` §7, §12's obligations 3 and 4).
    pub stamp: Option<GenerationStamp>,
    /// Request §3.3 underlay sub-cell counts at depth `zoom + offset`. `None` or `Some(0)` serves
    /// none and costs nothing.
    pub underlay_offset: Option<u8>,
    /// D-C: cooperative cancellation (the rapid-pan case) — checked once per tile and before each
    /// long serial-prefix stage; see [`Engine::viewport`]'s doc for the exact checkpoints. `None`
    /// costs one `Option` branch per check and nothing else, so every non-server embedder of this
    /// API is unaffected. Never threaded into the slot-state single-flight builders (Tasks 1-2,
    /// D-G) — a build already in flight runs to completion regardless of this token, because its
    /// result serves later arrivals too (D-C's scope note: bounded, useful work).
    pub cancel: Option<CancelToken>,
    /// The request's filter expression, or `None` for an unfiltered request.
    ///
    /// **Applied above the mask, never folded into it** (`filter-surface.md` §5.1). A filter narrows
    /// which marks are *drawn*; it never moves the selection threshold, which stays anchored on the
    /// unfiltered composed total (**I12**: a filter may move the frontier up, never down).
    pub filter: Option<crate::filter::FilterExpr>,
    /// Which annotation layers to answer for. `None` answers for every layer this principal
    /// reaches; an empty slice answers for none.
    ///
    /// **The same eliding this request's `tiles` list does, one axis over.** A client rendering one
    /// layer should not pay for the others' candidacy sweeps, and a client rendering none should
    /// pay nothing — a naming here does no candidacy work at all for a layer it omits.
    ///
    /// **It narrows and never widens.** A name this principal does not reach is simply absent from
    /// the answer, by the same route a name nobody registered is: the request is intersected with
    /// the session's resolved set, so asking for a layer is not a way to learn whether it exists.
    ///
    /// [`ViewportRequest::new`] starts at [`LayerSelection::All`]. **The wire's default is the
    /// opposite** (owner ruling 2026-08-25): a `/v1/viewport` request that omits `layers` names
    /// none, and asks for every layer with the string `"all"`. A Rust caller has no *omitted* —
    /// it constructs the request and names its selection — and the batch entry point keeps the
    /// serve-everything default its callers were written against.
    pub layers: LayerSelection<'a>,
    /// The client's artifact budget — how many artifacts it wants back at most, in the same shape
    /// as the `k` mark budget beside it ([decision 0083](../../../docs/decisions/0083-the-frontier-is-a-request-time-budget.md)).
    ///
    /// **Honoured structurally, never by sampling.** Artifacts cannot be sampled: dropping half the
    /// boundaries gives a wrong map rather than half a map, and no ordering over artifacts makes
    /// the retained half stand for the discarded half. A budget that cannot be met by serving
    /// everything is met by serving ancestors *instead of* their descendants — reduction by the
    /// layer's own structure.
    ///
    /// **A flat layer therefore ignores it**, and the whole of Stage 2 is flat layers: with no
    /// lineage there are no ancestors to cut to, so the only two answers are serve them all and
    /// refuse, and every artifact here passed its own existence test independently. The field is
    /// defined now rather than when the cut is built because it is a wire shape, and adding a
    /// request field to a shipped frame later is the change this ordering exists to avoid.
    ///
    /// **A budget is not a disclosure control**, and it sits where one used to: §8.4's maximum
    /// depth was a control, and confusing the two is the mistake this comment exists to prevent.
    /// Both directions are safe here — cutting shallower serves strictly less, cutting deeper
    /// serves more artifacts that each passed against `M_auth`.
    pub artifact_budget: Option<u32>,
    /// Which of each named layer's levels to answer for. See [`LevelSelection`].
    ///
    /// **Applies to every layer named**, against that layer's own declaration — a level number is a
    /// rung of one layer and means nothing across two, so there is no per-layer map here and under
    /// [decision 0096](../../../docs/decisions/0096-layers-are-usually-one-and-the-picker-offers-the-closure.md)
    /// a request names one layer anyway. [`LevelSelection::Declared`] needs no such map at all,
    /// each layer's own ranges deciding for it.
    ///
    /// **This is a request bound and never a control.** Every artifact a level holds passed its own
    /// existence criterion against `M_auth` before any of this ran (decision 0080), so asking for
    /// fewer levels serves strictly less and asking for more serves only artifacts that had already
    /// cleared their own test. It sits beside `artifact_budget` for that reason and carries the same
    /// warning: §8.4's maximum depth was a disclosure control and this is not one.
    pub levels: LevelSelection<'a>,
    /// Which of each layer's declared computed properties to answer for. See
    /// [`ComputedSelection`].
    ///
    /// [`ViewportRequest::new`] starts at [`ComputedSelection::Declared`], which is what the wire's
    /// absent field means and what every response carried before the field existed.
    pub computed: ComputedSelection<'a>,
    /// Which columns each served artifact answers with — see [`ArtifactRows`]. The row set is
    /// identical under either value; [`ArtifactRows::Identity`] skips payload production only.
    pub artifact_rows: ArtifactRows,
}

impl<'a> ViewportRequest<'a> {
    /// The required parameters; `stamp` and `underlay_offset` default to absent.
    pub fn new(view: &'a str, zoom: u8, bbox: [f64; 4], k: usize) -> Self {
        ViewportRequest {
            view,
            zoom,
            bbox,
            tiles: None,
            k,
            stamp: None,
            underlay_offset: None,
            cancel: None,
            filter: None,
            layers: LayerSelection::All,
            artifact_budget: None,
            levels: LevelSelection::Declared,
            computed: ComputedSelection::Declared,
            artifact_rows: ArtifactRows::Full,
        }
    }

    /// Answer for exactly these layers rather than for every one this principal reaches.
    pub fn layers(mut self, layers: LayerSelection<'a>) -> Self {
        self.layers = layers;
        self
    }

    /// See [`ViewportRequest::artifact_budget`].
    pub fn artifact_budget(mut self, budget: Option<u32>) -> Self {
        self.artifact_budget = budget;
        self
    }

    /// Answer for these computed properties of every layer that declares them. See
    /// [`ComputedSelection`].
    pub fn computed(mut self, computed: ComputedSelection<'a>) -> Self {
        self.computed = computed;
        self
    }

    /// Answer for these levels of every named layer. See [`LevelSelection`].
    pub fn levels(mut self, levels: LevelSelection<'a>) -> Self {
        self.levels = levels;
        self
    }

    /// Answer each artifact with these columns. See [`ArtifactRows`].
    pub fn artifact_rows(mut self, rows: ArtifactRows) -> Self {
        self.artifact_rows = rows;
        self
    }

    /// Attach a filter expression. See [`ViewportRequest::filter`].
    pub fn filter(mut self, filter: crate::filter::FilterExpr) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Answer for exactly these depth-`zoom` Morton prefixes rather than for `bbox`'s span.
    pub fn tiles(mut self, tiles: Option<&'a [u64]>) -> Self {
        self.tiles = tiles;
        self
    }

    pub fn stamp(mut self, stamp: Option<GenerationStamp>) -> Self {
        self.stamp = stamp;
        self
    }

    pub fn underlay_offset(mut self, offset: Option<u8>) -> Self {
        self.underlay_offset = offset;
        self
    }

    /// D-C: attach a cooperative-cancellation token. See [`Self::cancel`]'s field doc for the
    /// checkpoints and the single-flight-builder exemption.
    pub fn cancel(mut self, cancel: Option<CancelToken>) -> Self {
        self.cancel = cancel;
        self
    }
}

/// The two coordinates a client keys its replica on (`delta-serving.md` §2).
///
/// Both are opaque: minted here, echoed back, compared for equality and nothing else. They answer
/// two different questions, and a single coordinate answering both either voids a cache that is
/// still sound or honours a declaration that is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewCoordinates {
    /// Whether a held band may be **rendered at all** — the cache partition key.
    ///
    /// Over the idset, the auth-data hash, the mask fragment's identity and the view: everything
    /// that determines *what this principal may see*. Decision 0029's warning applies to this one —
    /// a client cache keyed more loosely than this serves one principal's authorised data to
    /// another, which is a disclosure and not a staleness bug.
    pub identity_key: [u8; 16],
    /// Whether a held band may be **declared** in a request.
    ///
    /// The identity key, plus the watermark of the geometry this response was actually served
    /// from, plus the overlay version, plus the process's boot nonce. See
    /// [`Engine::view_coordinates`] for why each is there and why the segment-set version is not.
    pub content_key: [u8; 16],
}

/// One artifact, as a viewport serves it.
///
/// **The absences are the design.** There is no ordinal — a position in a dense
/// level, so two of them count what lies between (C8). There is no declared size — a corpus-wide
/// count over items this principal may not see, and the denominator the proportional criterion
/// divides by, which is a predicate input and never a field. There is no membership. And there is
/// no reason-for-absence anywhere in the response, because an artifact that failed its criterion
/// must be indistinguishable from one that was never published.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactOut {
    /// The layer it belongs to. A name the principal reaches, always — the serving pass intersects
    /// with the session's resolved set before it looks at any membership.
    pub layer: String,
    /// Its opaque identifier — the only artifact address that crosses the trust boundary (**I10**),
    /// and what a drill-down or a suppression later names.
    pub tessera_id: TesseraId,
    /// The publisher's own key, if they supplied one. Operator-chosen text, not corpus data.
    pub key: Option<String>,
    /// **How many of this artifact's members this principal can see** — never how many it has. The
    /// same number the existence criterion was tested against, computed once and used for both.
    pub masked_count: u64,
    /// The layer's declared derived properties, recomputed **for this principal** from the same
    /// visible rows the count was taken over — see [`crate::derived`]. Empty where the layer
    /// declares none, which is the default and the cheap path.
    ///
    /// A shape here describes the members this viewer can see and no others, which is what makes
    /// it safe beside a gate that may have admitted the artifact on its own terms: such an artifact
    /// is authorised to *exist*, not to describe members the viewer cannot see.
    pub derived: crate::derived::DerivedContent,
    /// This artifact's parent, **and only ever one that is also in this response**.
    ///
    /// The structure a client needs to nest what it draws, or to filter to one subtree while still
    /// drawing the rest of the map. **Null covers two situations on purpose**: a root, and a parent
    /// that exists but was withheld from this viewer. Distinguishing them would disclose that a
    /// coarser grouping exists which they are not cleared to see.
    pub parent_id: Option<TesseraId>,
    /// **This is one content, entire.** Where an artifact carries several ranked descriptions,
    /// this is the first whose generating set the viewer contains completely; a viewer containing
    /// none receives no artifact at all rather than this list empty. Empty means the layer declares
    /// no supplied content, and nothing else.
    pub content: Vec<String>,
    /// **The resolution a client draws this artifact at**, computed the right way for its
    /// layer's kind (`artifact-fetch-protocol.md` §5.3 — the rung ruling, which renamed and
    /// re-meant the `level` field this carried until then).
    ///
    /// On a **levelled** layer it is the declared level — a fact about the artifact, the same for
    /// every principal served it, indexing the level set `/v1/meta` publishes. A client needs
    /// that number because the alternative it was reduced to is wrong: a tiered layer's edges
    /// skip levels and leave roots parentless, so counting `parent_id` links disagrees with the
    /// declaration on every layer whose data is not a perfect ladder.
    ///
    /// On a **treed** layer — which declares no levels and sits entirely at level 0, its
    /// structure in its edges ([decision 0082](../../../docs/decisions/0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md))
    /// — it is the **response-local parent-chain depth**: the depth of this row in the forest the
    /// response's own `parent_id` links form, *after* the budget cut and every other narrowing,
    /// so the root of a re-rooted subtree reads 0. That is the number walking the served parents
    /// yields, computed server-side so no client has to know which layer kind wants which
    /// derivation (the shipped client picked wrongly once).
    ///
    /// On a **flat** layer it is 0.
    pub rung: u32,
    /// Whether the served shape's vertex budget cut vertices the request's depth alone would have
    /// kept (`polygon-membership.md` §7.2) — a predicate or an authored shape still above 2,048
    /// vertices at that depth. Counted into the trailer's `stage_ns` companion; never a
    /// disclosure, being a fact about a drawing every principal served the artifact receives alike.
    pub shape_guard_fired: bool,
    /// **Whether any member of this artifact that the principal may see, and that lies inside the
    /// request's tiles, matches the request's filter** — `None` where the request carried no
    /// filter, which is *there was no question* rather than *no matches*
    /// ([decision 0104](../../../docs/decisions/0104-a-filter-answers-a-boolean-per-served-artifact.md)).
    ///
    /// **A boolean and never a count.** A filtered count beside [`Self::masked_count`] would put
    /// two numbers on one artifact and make a client choose which it is showing.
    ///
    /// **It is the only filter-dependent field here.** Existence and the count stay anchored on
    /// `M_auth`, so a filter cannot make an artifact appear or vanish and cannot move the number
    /// beside it (**I3**, **I12**) — and a client holding this artifact's payload across a filter
    /// change holds nothing stale but this.
    ///
    /// **It is clipped to the viewport where the count is not.** The count and the geometry are
    /// over the whole visible membership; this is over the part of it in view, because that is the
    /// extent every filter-crossing route can answer over rather than the extent one of them can.
    /// So an artifact whose only matches sit just off screen reads `false` until the viewer pans.
    ///
    /// **A dependent artifact carries its target's**, as its [`Self::masked_count`] does (D13): a
    /// label describes its cluster, and its own membership is a slice of that cluster at best.
    pub matched: Option<bool>,
}

/// The masked viewport response. No `serde` derive (I10) — see [`PointOut`]'s doc.
#[derive(Debug, Clone)]
pub struct ViewportOut {
    /// The coordinates a client keys its replica on. See [`ViewCoordinates`].
    pub coordinates: ViewCoordinates,
    /// The geometry this response was answered from — what a client echoes back next time.
    pub stamp: GenerationStamp,
    /// Whether the geometry moved since the stamp the request presented.
    ///
    /// `false` when no stamp was presented (there is nothing to be stale relative to) and when the
    /// presented stamp equals this response's. A client learns its held view is out of date at the
    /// moment it asks, and decides what to do — the server does nothing on its behalf.
    ///
    /// **This is the broadcast form: no per-session mask intersection.** It reports that *the
    /// corpus* moved, not that anything this principal can see moved, so a viewer whose visible set
    /// is unchanged is still told the geometry advanced. That is a deliberate ruling — knowing that
    /// data has been ingested is not a security leak (`geometry-pinning.md` §14) — and it is what
    /// makes this one comparison rather than a per-session diff. `client-interaction.md` §6.1's
    /// argument for per-session scoping survives as an efficiency argument (do not wake clients
    /// whose view did not change), not a security one.
    pub stale: bool,
    /// See [`ViewportHead::region`].
    pub region: Option<crate::region::RegionVerdict>,
    pub tiles: Vec<TileCount>,
    /// The annotation artifacts intersecting the request's tiles, each with the count *this*
    /// principal's visible set generates — see [`ArtifactOut`]. Empty when the principal reaches no
    /// layer, when the request named none, when nothing published intersects the viewport, and when
    /// everything that does failed its existence criterion; those four are one answer and are meant
    /// to be.
    pub artifacts: Vec<ArtifactOut>,
    /// The served points, column-major — see [`PointColumns`].
    pub points: PointColumns,
    /// The §3.3 density underlay, when requested — empty otherwise. Only non-empty cells appear.
    pub sub_cells: Vec<SubCellCount>,
    /// The render columns' names, in declaration order, from the SAME generation this response's
    /// points were gathered from — the parallel name list to `points.scalars`, and exactly its
    /// length. Carried here rather than left for the caller to
    /// re-fetch via `Engine::meta()` — that second call would `load_full()` the generation
    /// pointer a second time, against lifecycle §1.1's "exactly once, at request start". The
    /// names come from the same manifest either way, so response bytes are unaffected; this only
    /// removes a redundant load.
    pub scalar_names: Vec<String>,
    /// Per-stage breakdown, all zeros unless built with `bench-timing` (see
    /// [`crate::timing`]). **Excluded from `PartialEq`** — see the hand-written impl below.
    pub timings: StageTimings,
}

/// `PartialEq` ignoring `timings`, hand-written rather than derived.
///
/// Two responses carrying the same stamp, tiles, points and scalar names *are* the same response;
/// the wall-clock it took to produce them is not part of that identity. A derived impl would make
/// every `assert_eq!` over a whole `ViewportOut` in the test suite timing-dependent, and
/// therefore flaky the moment `bench-timing` is enabled — which is exactly when those tests
/// matter most.
///
/// `stale` joins it for the same reason: it is a function of the request's own stamp and the
/// generation the response came from, both already compared.
///
/// `scalar_names` joins the comparison: it is drawn from the same manifest as `points`'
/// values, in the same generation, so two responses that agree on `points` already agree on it —
/// including it costs nothing and is more honest than silently exempting a field that happens
/// never to differ in practice.
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

/// Everything about a viewport response that is known **before the sweep runs** — the HTTP
/// headers derive from it, which is why it is delivered first (`streamed-serving.md` §4).
#[derive(Debug, Clone)]
pub struct ViewportHead {
    pub coordinates: ViewCoordinates,
    /// The geometry this response is answered from — what a client echoes back next time.
    pub stamp: GenerationStamp,
    /// See [`ViewportOut::stale`].
    pub stale: bool,
    /// The region leaves' verdict — `x-tessera-region` — or `None` where the request carried no
    /// region leaf. A function of the shapes and the grid alone, settled before any row is read
    /// (selection-operand §6).
    pub region: Option<crate::region::RegionVerdict>,
    /// The **render**-column schema, in declaration order, from the SAME generation the response
    /// is served from — names for the wire's column headers, types so a collecting sink can seed
    /// empty columns for a response that emits no points chunk at all.
    ///
    /// The render narrowing, never the full declaration, because both consumers pair this list
    /// positionally with [`PointColumns::scalars`], which the emit pass gathers against
    /// `Manifest::render_scalars` — a wider list here serves one column's values under another
    /// column's name. The full compiled schema is `/v1/meta`'s to publish ([`EngineMeta`]), where
    /// it describes the ingest plane rather than a row.
    pub render_scalars: Vec<DeclaredScalar>,
}

/// The sink told the producer to stop: the consumer is gone (a closed channel, an expired
/// stream deadline). The producer treats it exactly as a cancellation — abandoned work, not a
/// fault.
#[derive(Debug)]
pub struct SinkClosed;

pub type SinkResult = std::result::Result<(), SinkClosed>;

/// Where [`Engine::viewport_stream`] delivers a response, in strict order: `head`, then
/// `counts`, then zero or more `points` chunks. The producer returning `Ok` is the completeness
/// signal — there is no `done` callback, so a sink that needs one (the server's trailer) writes
/// it after the call returns.
///
/// Every callback may refuse with [`SinkClosed`], which aborts the request as a cancellation
/// (D-C posture): the remaining work is abandoned, nothing partial is recorded anywhere, and
/// the caller gets [`EngineError::Cancelled`].
pub trait ViewportSink {
    /// Everything the response headers need. Exactly once, before any other callback.
    fn head(&mut self, head: ViewportHead) -> SinkResult;
    /// Sweep complete: every tile's counts, and the §3.3 underlay. Exactly once, before any
    /// points. `sub_cells` is `None` when the request did not ask for the underlay and `Some`
    /// (possibly of an empty slice) when it did — the wire's frame-presence rule needs the
    /// distinction, and an empty slice cannot carry it.
    fn counts(&mut self, tiles: &[TileCount], sub_cells: Option<&[SubCellCount]>) -> SinkResult;
    /// The artifacts intersecting the request's tiles. At most once, after `counts` and before any
    /// points, and **never with an empty slice** — the points callback's rule, for the same reason:
    /// a deployment with no layers, or a viewport over a region holding none, would otherwise pay a
    /// frame on every request to say nothing. A client learns which layers it reaches from
    /// `/v1/meta`, so an absent frame and an empty one carry the same information and only one of
    /// them costs bytes.
    ///
    /// **Required rather than defaulted, deliberately.** A default that dropped the frame would let
    /// a consumer compile against a response it never renders — a map with its clusters silently
    /// missing, which looks exactly like a principal who may not see them.
    fn artifacts(&mut self, artifacts: &[ArtifactOut]) -> SinkResult;
    /// One flush chunk: whole tiles' worth of points, in response order, ascending by
    /// `tessera_id` within each tile. Never called with an empty chunk.
    fn points(&mut self, chunk: PointColumns) -> SinkResult;
}

/// [`Engine::viewport`]'s sink: collect everything, so the batch caller sees exactly what a
/// streaming consumer would have seen, concatenated.
#[derive(Default)]
struct CollectSink {
    head: Option<ViewportHead>,
    tiles: Vec<TileCount>,
    sub_cells: Vec<SubCellCount>,
    artifacts: Vec<ArtifactOut>,
    points: Option<PointColumns>,
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
                // Unreachable: every chunk of one response is gathered against the same
                // declared-scalar list, and a per-chunk type disagreement is refused inside the
                // gather (`gather_tile_columns`) before it could reach here.
                .expect("chunks of one response cannot disagree on a column's type"),
        }
        Ok(())
    }
}

/// `POST /v1/items/{handle}`'s payload (R5): a visible item's full record — every declared field
/// that carries a value, by **declared name** — plus its caller-supplied external id, if it has
/// one.
///
/// Names, not tags, and nothing else (I10): a blob field's tag is a declaration position and an
/// index internal, resolved to the declared name engine-side; no tag, no entity id and no blob
/// addressing detail crosses the trust boundary. A category field carries its vocabulary **key**,
/// never its code-as-value ambiguity — the code is what the hot path ships, and the drill-down is
/// precisely the surface that resolves it.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemOut {
    /// Present fields only, in declaration order — an absent field is absent, not null, which is
    /// the same statement the record blob makes byte-wise (records §3).
    pub fields: Vec<ItemField>,
    pub external_id: Option<Vec<u8>>,
}

/// One declared field of a drill-down record: the column's declared name and its value.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemField {
    pub name: String,
    pub value: ScalarOut,
}

/// `GET /v1/meta`'s payload (R5) — the bundle-level facts a viewer client needs before it can
/// issue a sensible `/v1/viewport` call.
#[derive(Debug, Clone)]
pub struct EngineMeta {
    pub api_version: u32,
    pub bundle_format: u32,
    /// `(id, display_name)` pairs, in manifest order.
    pub views: Vec<(String, String)>,
    pub quantisation: Quantisation,
    /// **What placed every position in this bundle before the frame did** (`projections.md` §3),
    /// read from the view the build materialised — a build materialises exactly one coordinate
    /// system, so the manifest carries one view and there is one answer.
    ///
    /// Here because a `region` leaf and a published shape declared in longitude and latitude are
    /// put through *this* function and no other (`polygon-membership.md` §4.3, R12). ⊘ It is not
    /// yet published on `/v1/meta`, which carries the frame alone, so a client still cannot tell
    /// a geographic corpus from an embedding (`projections.md` §9).
    pub projection: Projection,
    pub declared_scalars: Vec<DeclaredScalar>,
    /// The live category bindings, from the same generation as `declared_scalars`.
    ///
    /// **Ingest resolves keys through this, and never mints.** A declared vocabulary is immutable
    /// between builds, so a handler's snapshot cannot be stale for one; a discovered vocabulary's
    /// novel keys travel to the write executor as keys, because two handlers racing one novel key
    /// would draw two codes for it and split its rows between them.
    pub vocabularies: Arc<Vocabularies>,
    /// The idset (contracts §2.2/§2.6 r6). `GET /v1/meta` reports this
    /// verbatim as `idset`; `POST /v1/items/{tessera_id}` compares an optional
    /// caller-supplied `idset` against it. Never the identity **key** — that never leaves the
    /// server, on any plane (design Appendix C, C17; I10).
    pub idset: u32,
}

impl Engine {
    /// `GET /v1/meta` (R5): read-only bundle facts, no session/authorisation involved. Loads the
    /// generation once, like every other request path.
    pub fn meta(&self) -> EngineMeta {
        let generation = self.generation.load_full();
        let manifest = &generation.bundle.manifest;
        EngineMeta {
            api_version: API_VERSION,
            bundle_format: manifest.bundle_format,
            views: manifest
                .views
                .iter()
                .map(|s| (s.id.clone(), s.display_name.clone()))
                .collect(),
            quantisation: manifest.quantisation,
            projection: manifest
                .views
                .first()
                .map(|v| v.projection)
                .unwrap_or(Projection::None),
            // The **full** compiled schema, including `filter`-only columns: `/v1/meta` describes
            // what a caller may declare and supply on the ingest plane, not what occupies a row.
            // The segment-facing readers narrow to `render_scalars` at their own sites.
            declared_scalars: manifest.declared_scalars.clone(),
            vocabularies: Arc::clone(&generation.vocabularies),
            idset: manifest.identity.idset,
        }
    }

    /// Is `entity` visible to `session` under `generation` — the ONE BIT `/v1/items` needs. An
    /// **entity-space** question (see `crate::compose::visible_to`'s doc): three constant-time
    /// probes, no `RowProjection` constructed or consulted, so this costs the same whether
    /// `entity` exists and is visible, exists and is not, or does not exist at all (Critical
    /// C-5, closed rather than narrowed).
    ///
    /// **Takes the fragment as an argument rather than reading `session.fragment`.** A session's
    /// own fragment goes stale at every flush — see [`Engine::fragment_for`] — and a drill-down
    /// that answered from the stale one would report a flushed item as invisible while the viewport
    /// beside it drew the mark. The caller has already resolved the generation once (lifecycle
    /// §1.1) and brings the fragment forward against that same snapshot.
    pub fn visible_to(
        &self,
        fragment: &FrozenFragment,
        session: &Session,
        generation: &Generation,
        entity: EntityId,
    ) -> bool {
        visible_to(
            fragment,
            &session.satisfied,
            &generation.overlay,
            &generation.buffer,
            entity,
        )
    }

    /// `POST /v1/items/{handle}` (R5): validate `idset` if the caller sent one, invert `id` to
    /// its entity, test visibility in entity space, and only then locate a row and read its
    /// scalars/external id.
    ///
    /// **`idset` is checked against the SAME generation this call loads for the lookup below —
    /// never a separate `Engine::meta()` call.** A handler that called `Engine::meta()` (its own
    /// `generation.load_full()`, plus a clone of every declared scalar and view name, just to read
    /// one field) before calling this method would load the generation twice for one logical
    /// request, against lifecycle §1.1's one-load-per-request invariant. Checking here, first,
    /// against the snapshot already in hand is not merely cheaper: it closes the gap where a
    /// generation swap landing between the two calls validates the idset against one
    /// generation and serves the lookup from another.
    ///
    /// Returns `Ok(None)` both when `id` names nothing in this bundle and when it names an item
    /// the principal may not see — deliberately one outcome from one code path, so the server
    /// cannot differentiate what the engine does not tell it (contracts §3.2).
    ///
    /// **The timing channel is closed, not narrowed** (design Appendix C, C4
    /// annotation). The idset check is entity-independent — it runs identically for every `id`,
    /// before inversion, and does not read `id` at all — so it opens no channel of its own.
    /// Inversion is a pure function taking no I/O. The visibility test that follows is an
    /// entity-space question — three constant-time probes — and is **the same three probes for
    /// an identifier that names nothing and one that names an invisible item**. No
    /// `RowProjection` is constructed or read, so there is no per-ID cost for an attacker to
    /// correlate against, warm or cold. A row is located only after the answer is already
    /// "visible", and the sidecar is read only after that.
    ///
    /// **Returns `Err` rather than a fail-open `None`.** A digest mismatch, an
    /// out-of-order extent or a short locator is a `500`, never an item served with
    /// `external_id: null` — `.ok().flatten()` would discard exactly the typed errors the sidecar
    /// exists to produce. This does not reopen the timing channel: the sidecar is touched only for
    /// an item already established as visible, so no attacker-drivable path can raise it.
    pub fn item(
        &self,
        session: &Session,
        id: TesseraId,
        idset: Option<u32>,
    ) -> Result<Option<ItemOut>> {
        let generation = self.generation.load_full();

        // Checked FIRST, against the generation this call already loaded above — see this
        // method's doc for why that (not a separate `Engine::meta()` call) is load-bearing here.
        if let Some(e) = idset {
            if e != generation.bundle.manifest.identity.idset {
                return Err(EngineError::StaleIdSet);
            }
        }

        let (shard, entity) = self.identity_key.invert(id);
        if shard != generation.bundle.manifest.identity.shard_id {
            return Ok(None);
        }

        // Brought forward before the visibility test, not after: the whole point of the test is
        // that it is the same three probes for every identifier (C-5), and a fragment resolved
        // per-entity would make the cost depend on which entity was asked for.
        //
        // **The served fragment, not the live one** (decision 0044). Rebuilding at the live
        // watermark is a *measured* ~200 ms per credential per publication on this thread, and it
        // would answer from a different watermark than the viewport beside it serves from — the
        // two enforcement representations drifting under stale-serve. Falls back to a build only
        // when this session has no resident entry at all, which is establishment. **No projection
        // is constructed either way**: this is a read of the cache, never a claim on it.
        // **Scoped to this generation's prefix**, so a fold's flip cannot answer an
        // entity-space question from a fragment built against the term index it replaced — see
        // `RowProjectionCache::freshest_fragment`.
        let fragment = match self
            .row_projection_cache
            .freshest_fragment(session.token_id, &generation.prefix)
        {
            Some(fragment) => fragment,
            None => self.fragment_for(session, &generation)?,
        };

        // ONE BIT, in entity space, O(1), before anything is looked up in row space.
        if !self.visible_to(&fragment, session, &generation, entity) {
            return Ok(None);
        }

        // Visible. Now — and only now — find the row, so the cost below is never reachable by
        // an identifier the principal may not see.
        //
        // **The record is assembled from its three homes** (records §3): render fields from the
        // row's scalar tail, indexed and category fields from their entity-space structures — a
        // category's code resolved to its vocabulary key — and everything else from one record
        // blob read. Field identity is the declared *name*, resolved engine-side from the blob's
        // positional tag; no tag, no entity id and no blob internal reaches the wire (I10).
        //
        // **Every read below sits strictly after the visibility verdict, so C4 stays closed by
        // construction, not by measure.** The verdict above is the same three constant-time
        // entity-space probes for an identifier that names nothing and one that names an
        // invisible item; the row lookup, the entity-space value reads, the blob block read and
        // the sidecar read are all reachable only for an item already established visible —
        // exactly the position the external-id sidecar has always occupied. The blob's block
        // decompression is therefore not a probe-able cost: no attacker-drivable path reaches it
        // for an item the principal cannot see (X1's surface, bounded the same way the sidecar's
        // is).
        let manifest = &generation.bundle.manifest;
        // **Render columns only in the row read.** The compiled schema includes entity-space and
        // blob-resident columns, which are absent from `columns.arrow` by design; those are
        // homes 2 and 3 below, never a column of nulls under a name a client can see.
        let render_scalars: Vec<_> = manifest.render_scalars().cloned().collect();
        // The allocator caps entity ids at `u32::MAX` (I9), and inversion produced this one from
        // a 32-bit half; checked rather than cast so a violated invariant fails loudly.
        let entity_raw =
            u32::try_from(entity.raw()).expect("entity ids are capped at u32::MAX by I9");
        for partition in generation.bundle.partitions.values() {
            for (view, view_data) in &partition.views {
                // The permutation is the only entity→row bridge (I4, §5.1) — an O(1)
                // bounds-checked slot read, not a scan.
                let Some(row) = view_data.row_space.row_of(entity) else {
                    continue;
                };
                // **A view holds more than one segment once anything has flushed**, and `row` is
                // a *view*-space row: it must be resolved to the segment that owns it and to that
                // segment's local index before anything is read. Taking the first segment and
                // indexing it with a view row read past the build segment's end for every
                // flushed item.
                let segments = segments_with_row_bases(view, view_data)?;
                let Some(&(segment, row_base)) =
                    segments.iter().rev().find(|(_, base)| row.raw() >= *base)
                else {
                    continue;
                };

                // One value slot per declared column, filled home by home; a column no home
                // holds a value in stays `None` and is omitted — absence is absence.
                let mut values: Vec<Option<ScalarOut>> =
                    vec![None; manifest.declared_scalars.len()];

                // Home 1: the row. The same `resolve_scalars` the viewport gather uses, so the
                // two read paths cannot disagree about what a stored type decodes to.
                let resolved = resolve_scalars(segment, &render_scalars);
                let local = (row.raw() - row_base) as usize;
                for (slot, declared_index) in manifest.render_indices().enumerate() {
                    let Some(view) = &resolved[slot] else {
                        continue;
                    };
                    let d = &manifest.declared_scalars[declared_index];
                    values[declared_index] =
                        row_field_out(view, local, d, &generation.vocabularies);
                }

                // Home 2: entity space — every non-rendered column with a value column (indexed
                // columns, and the per-viewer vocabulary floor), at drill-down cadence.
                for (declared_index, d) in manifest.declared_scalars.iter().enumerate() {
                    if d.render || values[declared_index].is_some() {
                        continue;
                    }
                    if let Some(stored) =
                        generation.filter_columns.stored_value(&d.name, entity_raw)
                    {
                        values[declared_index] =
                            stored_field_out(stored, d, &generation.vocabularies);
                    }
                }

                // Home 3: the record blob — one block read, strictly after the verdict (see this
                // method's doc). Fail-closed: a malformed row, a tag past the schema or an
                // addressing defect refuses the request rather than serving a neighbour's field
                // under this item's identity (records §3, review B6).
                if let Some(blob_fields) = generation
                    .filter_columns
                    .records()
                    .fields_of(entity_raw)
                    .map_err(|e| EngineError::Malformed(e.to_string()))?
                {
                    for field in blob_fields {
                        let declared_index = field.tag as usize;
                        let Some(d) = manifest.declared_scalars.get(declared_index) else {
                            return Err(EngineError::Malformed(format!(
                                "a record-blob row carries field tag {} where the schema \
                                 declares {} columns; the blob and the manifest disagree",
                                field.tag,
                                manifest.declared_scalars.len()
                            )));
                        };
                        if values[declared_index].is_none() {
                            values[declared_index] =
                                stored_field_out(field.value, d, &generation.vocabularies);
                        }
                    }
                }

                let fields = manifest
                    .declared_scalars
                    .iter()
                    .zip(values)
                    .filter_map(|(d, value)| {
                        value.map(|value| ItemField {
                            name: d.name.clone(),
                            value,
                        })
                    })
                    .collect();
                return Ok(Some(ItemOut {
                    fields,
                    // N-3: propagate, never swallow. `EngineError::Store`, the same wrapping
                    // every other store-backed call in this crate uses (see `Engine::open`).
                    // Against the generation this request loaded, never a second `load()`: the
                    // sidecar is per-generation now, and a fold rewrites it.
                    external_id: self
                        .external_id_of_in(&generation, entity)
                        .map_err(EngineError::Store)?,
                }));
            }
        }
        // Visible in entity space but with no row anywhere: a buffered item awaiting flush. Same
        // `Ok(None)`, same 404 — it has no geometry to return.
        Ok(None)
    }
}

/// One render column's drill-down value, read from the row: a category's code resolved to its
/// key — code 0, the absent sentinel, resolving to absence — and every other family as stored.
///
/// A rendered *number*'s absence is still stored as the type's zero (`or_render_placeholder`;
/// 0064's wire half being deferred), so a numeric zero here may be a real zero or an absence —
/// the row cannot say which, and this reports the stored value rather than inventing a rule. The
/// entity-space and blob homes do not share the ambiguity.
fn row_field_out(
    view: &ScalarSlice<'_>,
    idx: usize,
    d: &DeclaredScalar,
    vocabularies: &Vocabularies,
) -> Option<ScalarOut> {
    if d.vocabulary.is_some() {
        let code = match view {
            ScalarSlice::U8(s) => s[idx] as u32,
            ScalarSlice::U16(s) => s[idx] as u32,
            ScalarSlice::U32(s) => s[idx],
            // A category is one of the three widths; anything else is a malformed tail the
            // gather refuses on its own path. Absence is the honest answer here.
            _ => return None,
        };
        return category_key_out(code, d, vocabularies);
    }
    // Generated for the flat members; `Bool` and `Utf8` read through their arrays because
    // neither is stored as a flat slice of itself.
    macro_rules! out {
        ($(($v:ident, $t:ty)),* $(,)?) => {
            match view {
                $(ScalarSlice::$v(s) => ScalarOut::$v(s[idx]),)*
                ScalarSlice::Bool(a) => ScalarOut::Bool(a.value(idx)),
                ScalarSlice::Utf8(a) => ScalarOut::Utf8(a.value(idx).to_string()),
            }
        };
    }
    Some(flat_families!(out))
}

/// One stored value's drill-down form, for the entity-space and blob homes: the storage-typed
/// [`tessera_filter::RecordValue`] adapted through the declaration — a category code to its key,
/// a `bool`'s `u8` storage back to `bool`, a `timestamp_us`'s `i64` back to its unit.
fn stored_field_out(
    value: tessera_filter::RecordValue,
    d: &DeclaredScalar,
    vocabularies: &Vocabularies,
) -> Option<ScalarOut> {
    use tessera_filter::RecordValue as RV;
    if d.vocabulary.is_some() {
        let code = match value {
            RV::U8(c) => c as u32,
            RV::U16(c) => c as u32,
            RV::U32(c) => c,
            _ => return None,
        };
        return category_key_out(code, d, vocabularies);
    }
    Some(match (d.arrow_type, value) {
        (ScalarType::Bool, RV::U8(x)) => ScalarOut::Bool(x != 0),
        (ScalarType::Bool, RV::Bool(b)) => ScalarOut::Bool(b),
        (ScalarType::TimestampUs, RV::I64(x)) | (ScalarType::TimestampUs, RV::TimestampUs(x)) => {
            ScalarOut::TimestampUs(x)
        }
        (_, RV::U8(x)) => ScalarOut::U8(x),
        (_, RV::U16(x)) => ScalarOut::U16(x),
        (_, RV::U32(x)) => ScalarOut::U32(x),
        (_, RV::U64(x)) => ScalarOut::U64(x),
        (_, RV::I8(x)) => ScalarOut::I8(x),
        (_, RV::I16(x)) => ScalarOut::I16(x),
        (_, RV::I32(x)) => ScalarOut::I32(x),
        (_, RV::I64(x)) => ScalarOut::I64(x),
        (_, RV::F32(x)) => ScalarOut::F32(x),
        (_, RV::F64(x)) => ScalarOut::F64(x),
        (_, RV::Bool(b)) => ScalarOut::Bool(b),
        (_, RV::TimestampUs(x)) => ScalarOut::TimestampUs(x),
        (_, RV::Utf8(s)) => ScalarOut::Utf8(s),
        // ⊘ Lists land with epic 3's multi surface; no writer produces one today, and a reader
        // that met one would be looking at a future format — absence, not a guess.
        (_, RV::List(_)) => return None,
    })
}

/// A category code's drill-down value: its vocabulary **key**. Code 0 — the reserved absent
/// sentinel — is absence, and a code no binding explains is omitted rather than served raw,
/// the same rule `/v1/categories` applies to an unresolvable code.
fn category_key_out(
    code: u32,
    d: &DeclaredScalar,
    vocabularies: &Vocabularies,
) -> Option<ScalarOut> {
    if code == 0 {
        return None;
    }
    let vocabulary = vocabularies.get(d.vocabulary.as_deref()?)?;
    let (key, _) = vocabulary.bindings().find(|&(_, c)| c == code)?;
    Some(ScalarOut::Utf8(key.to_string()))
}

/// D-C: `Err(EngineError::Cancelled)` if `cancel` has been flipped, `Ok(())` otherwise (including
/// when `cancel` is `None` — most callers, and every non-server embedder of this API, never set
/// one). Called at the checkpoints [`Engine::viewport`]'s doc lists; **not** called around the
/// row-projection single-flight build (D-G) — that stage is deliberately not gated by this check,
/// so a build already in flight always runs to completion regardless of this particular caller's
/// interest in it (its result serves later arrivals too).
#[inline]
fn check_cancelled(cancel: &Option<CancelToken>) -> Result<()> {
    if cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
        return Err(EngineError::Cancelled);
    }
    Ok(())
}

impl Engine {
    /// This session's fragment and row projection for this request — **the three-rung ladder
    /// decision 0044's D1 puts in front of every viewport.**
    ///
    /// 1. **The live entry**, if the background refresh has produced it. The steady state, and
    ///    zero work on this thread.
    /// 2. **The one-generation-stale entry**, served as it is. Sound for a flush and *only* for a
    ///    flush: a flush appends, so every row id the stale entry holds still names the same
    ///    entity, and what it lacks is the rows of items flushed since — which the live overlay
    ///    and deny mask then compose over unchanged. The session sees the newest items one refresh
    ///    later; that is fail-closed staleness, never a deny miss. `extends_to` is the predicate,
    ///    and it is exact rather than heuristic: after a *merge* the boundary segment differs, so
    ///    this rung refuses and rung 3 decides.
    /// 3. **Build, or refuse.** If a refresh is in flight the request is shed with
    ///    `ProjectionBuilding` (429, `Retry-After: 1`) rather than paying a rebuild the refresh is
    ///    already paying — the bounded residual 0044 permits, and after a merge the only thing
    ///    standing between a racer and the measured 1.28 s. If no refresh is in flight, nothing is
    ///    coming and this request builds: session establishment, or a rebuild after eviction,
    ///    neither of which is update-induced.
    ///
    /// **The fragment travels with the projection, and that is why they are one cache value.**
    /// Resolving the fragment separately at the live watermark — which is what this path did until
    /// stale-serve — would pair a live fragment with a stale projection and, worse, insert the
    /// result under the *live* key, pinning the session's freshly flushed items invisible until
    /// the next publication. See [`crate::cache::SessionGeometry`].
    /// Mint the two coordinates of `delta-serving.md` §2 for one answered request.
    ///
    /// **From the geometry actually served, not from the generation.** `session_geometry`'s rung 2
    /// answers from the projection and fragment one `segments_version` below when that projection
    /// still covers the row space, so two responses under a single generation snapshot can have
    /// different visible sets — the stale one lacking rows flushed since. Minting from the
    /// generation would give both the same key, and a later live response would then elide against
    /// a bound the client declared from the stale one: a hole in the client's own picture that it
    /// cannot detect, and one the server caused. The fragment's watermark is what distinguishes
    /// them, so the fragment that was served is what is hashed.
    ///
    /// **The watermark, and deliberately not `segments_version`.** Only *additions* to a tile's
    /// visible set can make an elision unsound: `tessera_id` is a keyed permutation and is not
    /// monotone in the entity id, so a newly ingested entity can land below any declared bound.
    /// Removals cannot — the client holds every visible identity below its bound, so a shrinking
    /// set leaves it holding a superset. The watermark is what counts rows added; a merge or a
    /// compaction moves `segments_version` and the prefix without adding one, and a declaration is
    /// expressed in identity space, so keying on the segment-set version would void every
    /// declaration on every background compaction for no correctness reason at all.
    ///
    /// **`overlay_version` earns its place on coherence, not safety.** Removals cannot open a hole,
    /// but they move `visible` and `matched`, which a client must not go on presenting as current.
    fn view_coordinates(
        &self,
        generation: &Generation,
        geometry: &SessionGeometry,
        view: &str,
    ) -> ViewCoordinates {
        let mut hasher = Sha256::new();
        hasher.update(b"tessera-identity-key-v1");
        hasher.update(generation.bundle.manifest.identity.idset.to_le_bytes());
        hasher.update(geometry.auth_data_hash);
        hasher.update(geometry.fragment.identity);
        hasher.update((view.len() as u64).to_le_bytes());
        hasher.update(view.as_bytes());
        let identity_digest: [u8; 32] = hasher.finalize().into();
        let mut identity_key = [0u8; 16];
        identity_key.copy_from_slice(&identity_digest[..16]);

        let mut hasher = Sha256::new();
        hasher.update(b"tessera-content-key-v1");
        hasher.update(identity_key);
        hasher.update(geometry.fragment.watermark.to_le_bytes());
        hasher.update(generation.overlay_version.to_le_bytes());
        hasher.update(self.boot_nonce.to_le_bytes());
        let content_digest: [u8; 32] = hasher.finalize().into();
        let mut content_key = [0u8; 16];
        content_key.copy_from_slice(&content_digest[..16]);

        ViewCoordinates {
            identity_key,
            content_key,
        }
    }

    fn session_geometry(
        &self,
        session: &Session,
        generation: &Generation,
        view: &str,
        view_data: &tessera_store::read::ViewData,
        cancel: &Option<CancelToken>,
        probe: &mut Probe,
    ) -> Result<Arc<SessionGeometry>> {
        let key = RowProjectionKey {
            token_id: session.token_id,
            view: view.to_string(),
            segments_version: generation.segments_version,
            prefix: generation.prefix.clone(),
        };
        if let Peek::Ready(geometry) = self.row_projection_cache.peek(&key) {
            return Ok(geometry);
        }

        // Rung 2. The generation exactly one below is the only one the retention depth keeps
        // (`crate::cache::KEEP_SUPERSEDED_GENERATIONS`) and the only one an append can be served
        // across.
        let space = &view_data.row_space;
        if let Some(previous) = key.segments_version.checked_sub(1) {
            let stale_key = RowProjectionKey {
                segments_version: previous,
                ..key.clone()
            };
            if let Peek::Ready(geometry) = self.row_projection_cache.peek(&stale_key) {
                if geometry.projection.extends_to(space) {
                    self.stale_serves.fetch_add(1, Ordering::Relaxed);
                    return Ok(geometry);
                }
            }
        }

        // Rung 3. **Compared against this request's own generation**, not read as a boolean: the
        // claim names the `segments_version` whose refresh is running, so a pass still finishing
        // for a *superseded* generation does not shed a request whose key nothing is coming to
        // produce — which would be a 429 with no end.
        if self.refresh_in_flight.load(Ordering::SeqCst) == key.segments_version {
            return Err(EngineError::ProjectionBuilding);
        }

        // D-G slot-state single-flight (F4, `tessera-bench/src/arms/load.rs:34-76`): the map lock
        // is held only for the O(1) `Building`/`Ready` transition — never across the build — so
        // distinct sessions' first viewports do not serialise behind one global lock.
        //
        // **A concurrent request racing the *same* key waits for that build and is served its
        // result** (decision 0058). It used to be refused, which reached the client as a 429 whose
        // `Retry-After` was shorter than the build it was waiting for — so at 10⁹ a client racing
        // itself exhausted its retries before work that was always going to succeed finished, and
        // showed a blank map. The wait is bounded by `serve.single_flight_wait_ms` and observes
        // this request's cancellation token, so a disconnected client releases its
        // `ComputeGate` permit rather than holding it to the budget.
        //
        // Blocking here is safe because of the guardrail below: this resolves on the **calling**
        // thread, never a rayon worker. `crate::refresh` is the caller for which that is not true,
        // and it does not wait.
        //
        // **Guardrail (D-D/D-F): nothing reachable from a rayon worker below may touch this
        // cache.** The value is resolved once, here, on the calling thread, strictly before the
        // parallel tile sweep begins, and is then only *borrowed* by every `tile_sweep` call.
        let fragment = self.fragment_for(session, generation)?;
        probe.lap(|t| &mut t.fragment_forward_ns);
        // Constructed here rather than at the top of this function so the allocation lands only on
        // the path that can actually park — every warm request returns at rung 1 above. An
        // embedder that supplies no token (every non-server caller of this API) waits on one that
        // is never flipped, which is the same posture `check_cancelled` takes for `None`.
        let never_cancelled = CancelToken::new();
        let cancel = cancel.as_ref().unwrap_or(&never_cancelled);
        self.row_projection_cache
            .get_or_derive_waiting(key, None, cancel, |_source| {
                // Crosses entity space into row space over the *whole* fragment
                // (`Permutation::project`'s cost note: seconds at 10⁹ rows). `Permutation::project`
                // parallelises internally but owns no pool of its own — this is the one call site
                // that supplies one, the same shared pool the tile sweep uses (D-D: no second,
                // per-request pool).
                probe.mark_projection_built();
                self.full_projection_builds.fetch_add(1, Ordering::Relaxed);
                let projection = self.pool.install(|| RowProjection::new(&fragment, space));
                SessionGeometry {
                    fragment: Arc::clone(&fragment),
                    projection: Arc::new(projection),
                    satisfied_sorted: Arc::clone(&session.satisfied_sorted),
                    auth_data_hash: session.auth_data_hash,
                    satisfied_at: session.segments_version_at_authorise,
                }
            })
            .map_err(|ended| match ended {
                // The budget expiry is what `SingleFlightBackpressure` now means: the request
                // waited for a build and it did not arrive. A cancellation is the client's own
                // disconnect and must not be dressed up as backpressure — reporting it as a 429
                // would tell an operator the server is shedding when a browser closed a tab.
                CacheWaitEnded::Budget => EngineError::ProjectionBuilding,
                CacheWaitEnded::Cancelled => EngineError::Cancelled,
            })
    }

    /// The masked viewport query, batch form: [`Self::viewport_stream`] run into a collecting
    /// sink. Same producer, same order, same bytes-at-completion — this is the surface the
    /// bench harness, the tests and any embedder consume, and the reason the streamed and
    /// batch answers cannot disagree: there is only one answer.
    pub fn viewport(&self, session: &Session, req: ViewportRequest<'_>) -> Result<ViewportOut> {
        let mut sink = CollectSink::default();
        // `usize::MAX`: never flush mid-emit, so the collector receives at most one chunk and
        // the memory profile matches the pre-streaming fold (one concatenation, no doubling).
        let timings = self.viewport_stream(session, req, usize::MAX, &mut sink)?;
        let head = sink
            .head
            .expect("viewport_stream delivers a head before returning Ok");
        // An empty response emits no points chunk at all (the sink contract), but the batch
        // shape still carries one buffer per render column — seeded from the head's schema,
        // which is the same render narrowing every gathered chunk has.
        let points = sink.points.unwrap_or_else(|| PointColumns {
            tessera_ids: Vec::new(),
            codes: Vec::new(),
            scalars: head
                .render_scalars
                .iter()
                .map(|d| ColumnBuf::empty(d.arrow_type))
                .collect(),
            membership: Vec::new(),
        });
        Ok(ViewportOut {
            coordinates: head.coordinates,
            stamp: head.stamp,
            stale: head.stale,
            region: head.region,
            tiles: sink.tiles,
            artifacts: sink.artifacts,
            points,
            sub_cells: sink.sub_cells,
            scalar_names: head.render_scalars.iter().map(|d| d.name.clone()).collect(),
            timings,
        })
    }

    /// The masked viewport query, streamed (`streamed-serving.md`) — see [`ViewportRequest`]
    /// for the parameters and for the non-decreasing-`k` obligation that §7.2's nesting
    /// property rests on, and [`ViewportSink`] for the delivery order. `flush_bytes` is the
    /// emit pass's chunk threshold: a points chunk is handed off once its estimated wire size
    /// reaches it, always at a whole-tile boundary.
    ///
    /// **D-C cancellation checkpoints** (cooperative, the rapid-pan case): once per tile in the
    /// sweep, at the top of [`tile_sweep`]; once before [`compose`] runs; once before θ's
    /// anchor (`mask.visible_total()`); once per tile in the emit pass. The
    /// row-projection single-flight build (D-G, above the compose checkpoint) is deliberately
    /// NOT gated — see [`check_cancelled`]'s doc.
    ///
    /// **I13a, annotated at the claim** (architecture §4 carries the ruling): a hit at any
    /// checkpoint — including the sink refusing a delivery — aborts the request with
    /// [`EngineError::Cancelled`], and nothing partial is observable by any *other* request.
    /// The requesting consumer itself may by then have received a prefix of the response; that
    /// prefix is exact (whole tiles, each an id-order prefix of its served set), the consumer
    /// can detect the truncation (the producer never signalled completion), and delta-serving
    /// §7 licenses drawing it — a partial *presented as complete* is what the invariant
    /// forbids, and no path here produces one.
    pub fn viewport_stream(
        &self,
        session: &Session,
        req: ViewportRequest<'_>,
        flush_bytes: usize,
        sink: &mut dyn ViewportSink,
    ) -> Result<StageTimings> {
        let ViewportRequest {
            filter: _,
            view,
            zoom,
            bbox,
            tiles: requested_tiles,
            k,
            stamp,
            underlay_offset,
            cancel,
            layers: req_layers,
            artifact_budget,
            levels: req_levels,
            computed: req_computed,
            artifact_rows,
        } = req;
        // Load the generation pointer exactly once, at request start (see `GenerationHandle`'s
        // doc at its definition) — every subsequent read below (pin check, row-projection cache,
        // composition) comes from this one snapshot, so a concurrent overlay/bundle swap
        // mid-request can never mix state from two generations.
        let mut probe = Probe::new();

        let generation = self.generation.load_full();
        probe.lap(|t| &mut t.generation_resolve_ns);

        // The stamp: what this response was answered from, and whether that is newer than what the
        // client is holding. **No lock, no drain list, no lookup** — one clone and one comparison
        // against the generation already loaded above. Everything below reads `generation`, so
        // geometry, overlay and buffer all come from the one snapshot, which is I11's
        // within-request rule in full (`crate::geometry`).
        let answered_from = GenerationStamp {
            prefix: generation.prefix.clone(),
            segments_version: generation.segments_version,
        };
        // A presented stamp that names a superseded generation, or a superseded prefix, is
        // answered here exactly as an absent one is. It sets a flag; it never selects, refuses or
        // expires.
        let stale = stamp.is_some_and(|presented| presented != answered_from);

        probe.lap(|t| &mut t.stamp_compare_ns);

        let k = k.min(self.config.max_k);

        // Fail closed on a view spanning partitions, for the same reason the segment guard below
        // exists: this resolves to ONE partition, and theta's anchor and every rank are then taken
        // over that partition alone — which §12.3 forbids (the anchor must be session-global, or
        // "below the cut" means different things in different partitions). The build emits one
        // partition, so this is unreachable; it is here so a §12 bundle cannot be served
        // half-masked with no error, which is the failure the multi-segment guard already refuses.
        let carriers = generation
            .bundle
            .partitions
            .values()
            .filter(|partition| partition.views.contains_key(view))
            .count();
        if carriers > 1 {
            return Err(EngineError::MultiPartitionView(view.to_string()));
        }
        let view_data = generation
            .bundle
            .partitions
            .values()
            .find_map(|partition| partition.views.get(view))
            .ok_or_else(|| EngineError::UnknownView(view.to_string()))?;

        // Every segment of the view, each with where its rows begin in the view's row space.
        //
        // **Keyed on `seg_id`, never on position.** `Bundle::with_segment` appends a flush
        // segment to `segments` while `RowSpace::with_extent` appends its extent, so the two lists
        // agree positionally after a flush — but `Bundle::with_merged` pushes the merged segment
        // at the *end* of `segments` while `RowSpace::collapsing` puts the merged extent where the
        // consumed run was. After one merge the positions diverge, and a positional zip would
        // silently pair a segment with another segment's `row_base`: every count right, every
        // point drawn from the wrong entity. `seg_id`s are never reused (contracts §2.1), so the
        // lookup is exact.
        //
        // The build segment is the one `permutation.bin` addresses and has no extent; it is
        // therefore the one with no entry here, and its rows begin at 0.
        let segments = segments_with_row_bases(view, view_data)?;
        probe.lap(|t| &mut t.view_lookup_ns);

        // **Zero update-induced work on this thread, in the steady state** (decision 0044's D1).
        // Every flush advances `segments_version`, so every flush rotates this key for every live
        // session; the *measured* costs of doing anything about that here are 1.28 s for a rebuild
        // and 40.9 ms for the patch's bitmap clone alone (`probes/2026-08-04-refresh-ladder/`),
        // against a budget of 0.2 ms. Neither fits. What runs instead is a background refresh at
        // each publication (`crate::refresh`), and this is its request-side face: a three-rung
        // ladder that builds nothing a refresh is about to produce.
        //
        // **Guardrail (D-D/D-F): nothing reachable from a rayon worker below may touch this
        // cache.** The value is resolved once, here, on the calling thread, strictly before the
        // parallel tile sweep begins, and is then only *borrowed* (via `compose`'s
        // `EffectiveMask`) by every `tile_sweep` call — never re-fetched or re-built per tile.
        let geometry =
            self.session_geometry(session, &generation, view, view_data, &cancel, &mut probe)?;
        // Minted here, from the geometry that actually resolved — see `view_coordinates`.
        let coordinates = self.view_coordinates(&generation, &geometry, view);
        // The same rule one structure along: the masked-count cache's key names the fragment this
        // request composes against, which under stale-serve is the entry's and not the newest one.
        let mask_identity = self.mask_identity(session, &generation, &geometry);
        let base = Arc::clone(&geometry.projection);
        probe.lap(|t| &mut t.row_projection_ns);

        // **Render columns only, narrowed once — for the head and the gather alike.**
        // `declared_scalars` is the compiled schema and includes `filter`-only and blob-resident
        // columns, which are entity-space and absent from `columns.arrow` by design; taking the
        // full list would publish a column of nulls under a name a client can see, and — because
        // the head's names are zipped positionally with the gathered buffers — caption a render
        // column's values with a non-render column's name wherever the two lists diverge. One
        // construction site is what keeps the names and the buffers the same list.
        let render_scalars: Vec<_> = generation
            .bundle
            .manifest
            .render_scalars()
            .cloned()
            .collect();

        // The head is delivered below, after the filter is evaluated and before the sweep: its
        // region verdict is settled by the decomposition, which needs the tile ranges the filter
        // is evaluated beside, and a server that waits for the first flush before committing a
        // status has it in hand by then.

        // D-C checkpoint: before compose, one of the two long serial-prefix stages this task
        // guards. Placed after the (non-cancellable, D-G) row-projection build so a cancellation
        // observed here never interrupts that build — only work this request would otherwise go
        // on to do itself.
        check_cancelled(&cancel)?;

        // **Fail-closed on a missing entry.** Every view the bundle carries has one, empty when
        // nothing is denied (`compose::derive_denied`), so an absent key means the mask and the
        // bundle disagree about what this generation holds. Serving that as "nothing is denied
        // here" would publish suppressed and deleted rows on the map with no error anywhere —
        // the same shape as `SegmentWithoutRowBase`, and refused the same way.
        let denied = generation
            .denied
            .get(view)
            .ok_or_else(|| EngineError::DenyMaskMissing {
                view: view.to_string(),
            })?;

        let mask = compose(
            &session.satisfied,
            &generation.overlay,
            &generation.buffer,
            base,
            &view_data.row_space,
            denied,
        );
        probe.lap(|t| &mut t.compose_ns);

        // The filter is NOT evaluated here. Its route — entity space, row space, or a mixed tree
        // of both (decision 0068) — turns on the request's own `rows_in_ranges` and on θ's
        // unfiltered anchor, neither resolved yet, so evaluation sits below the tile-range sweep
        // beside the crossing it feeds. Everything between here and there is deliberately blind
        // to the filter.

        let q = &generation.bundle.manifest.quantisation;
        let extent = Bounds {
            x_min: q.x_min,
            x_max: q.x_max,
            y_min: q.y_min,
            y_max: q.y_max,
        };

        // Refuse an over-large tile set **before allocating it**. `zoom` and `bbox` are both
        // attacker-chosen, and the tile set is their product: at zoom 16 over the full extent that
        // is 65536² = 4.29e9 tiles at 16 B each — ~69 GB in one `Vec`, i.e. an out-of-memory abort
        // from a single authenticated request, reached before any masking work happens. Counting
        // first (`tiles_for_bbox_count` allocates nothing) is what makes this a 422 instead.
        //
        // Both independent reviews of this file flagged that an earlier revision bounded only the
        // *derived* underlay fan-out below while commenting that "`tiles_for_bbox` is itself
        // uncapped" — guarding the second-order factor and leaving the first-order one open. This
        // is the first-order bound; the underlay's is now genuinely second-order.
        // The same bound applies to an explicit list — the count is attacker-chosen either way, and
        // a list makes it *more* directly so than a bbox does.
        let tile_count = match requested_tiles {
            Some(list) => list.len() as u64,
            None => tiles_for_bbox_count(bbox, zoom, &extent),
        };
        if tile_count > self.config.max_tiles_per_request as u64 {
            return Err(EngineError::TooManyTiles {
                demanded: tile_count,
                limit: self.config.max_tiles_per_request,
            });
        }
        let tiles = match requested_tiles {
            // **An explicit list replaces the derivation, and that is where the saving is.** Every
            // tile a client can prove it already holds is absent, and absence costs nothing at all:
            // no row range, no `count_range`, no selection scan, no gather. Ordering is the
            // caller's — deduplicated at the request boundary, first occurrence kept, NOT sorted
            // (contracts §3.2 r26) — and it is the order the tile stream reports and the points
            // stream concatenates in.
            Some(list) => list
                .iter()
                .map(|&prefix| Tile {
                    prefix,
                    depth: zoom,
                })
                .collect(),
            None => tiles_for_bbox(bbox, zoom, &extent),
        };
        probe.lap(|t| &mut t.tiles_for_bbox_ns);
        probe.count(|t| &mut t.tiles_resolved, tiles.len() as u64);

        // The emit pass gathers against the same render list the head carried — see its
        // construction above for why the two must be one list.
        let render_scalars = &render_scalars[..];

        // §3.3 underlay bounds, all three checked up front and all three *rejecting* rather than
        // clamping (see `EngineError::UnderlayRefused`). The cell budget is checked before any
        // counting work because the underlay multiplies the (already-bounded) tile set by 4^offset.
        //
        // `underlay_cells_demanded` (0 when no underlay was requested) is captured
        // here, outside the match, so the serial-fallback predictor below can see it — review
        // caught that the predictor was blind to underlay cost entirely (`total_rows_in_ranges`
        // alone), which is a real gap since a saturated underlay (`max_underlay_cells`, default
        // 8192) is comparable work to thousands of spanned rows and was previously invisible to
        // the serial/parallel decision no matter how large it was.
        let mut underlay_cells_demanded: u64 = 0;
        let underlay_offset = match underlay_offset {
            None | Some(0) => None,
            Some(offset) => {
                if offset > self.config.max_underlay_offset {
                    return Err(EngineError::UnderlayRefused(format!(
                        "underlay_offset {offset} exceeds the configured maximum {}",
                        self.config.max_underlay_offset
                    )));
                }
                let sub_depth = zoom as u32 + offset as u32;
                if sub_depth > 16 {
                    return Err(EngineError::UnderlayRefused(format!(
                        "underlay_offset {offset} at zoom {zoom} needs depth {sub_depth}, but the \
                         grid is fixed at 2^16 x 2^16 so depth may not exceed 16 (§5.2)"
                    )));
                }
                let per_tile = 1usize << (2 * offset as u32);
                let demanded = tiles.len().saturating_mul(per_tile);
                if demanded > self.config.max_underlay_cells {
                    return Err(EngineError::UnderlayRefused(format!(
                        "underlay_offset {offset} over {} tiles demands {demanded} sub-cells, \
                         above the configured budget of {}",
                        tiles.len(),
                        self.config.max_underlay_cells
                    )));
                }
                underlay_cells_demanded = demanded as u64;
                Some(offset)
            }
        };

        // θ's anchor: the session's **composed** visible cardinality over this view's whole row
        // space. It must be the composed figure and not `base`'s — see `Threshold::anchor`'s doc
        // for the I2 argument and the concrete channel the pre-overlay figure opens.
        //
        // This is viewport-*invariant*: it depends on the session's mask and the generation, never
        // on `bbox` or `zoom`, so θ does not move when the viewer pans — which is the churn §7.2
        // forbids. It does move on an overlay swap, which is accepted: swaps are rare against pans,
        // and because the served set is a `tessera_id` prefix, a small θ move perturbs only the
        // marks nearest the cut.
        // D-C checkpoint: before θ's anchor, the second long serial-prefix stage this task guards.
        check_cancelled(&cancel)?;
        let v_total = mask.visible_total();
        probe.lap(|t| &mut t.theta_anchor_ns);
        let params = SelectParams {
            k_min: self.config.k_min,
            // The client may ask for less than the overplot ceiling; it may not ask for more.
            // Applying it here rather than truncating afterwards is free — the served set is a
            // prefix, so the two agree — and it bounds the selection heap and the output gather.
            cap: k.min(self.config.k_max_marks),
            threshold: Threshold::anchor(v_total, self.config.theta_target_marks).at_depth(zoom),
        };

        let mut tile_counts = Vec::new();
        let mut sub_cells = Vec::new();

        // Resolve every tile's row range in ONE monotone sweep rather than two full-column binary
        // searches per tile. A few hundred independent `log2(rows)` searches is where a sparse
        // request's time actually goes — measured at 26-64% of one
        // (`docs/evidence/memos/2026-07-30-f1-selection-overdraw.md`), and flat in density, because
        // the cost is the searching rather than the rows found.
        //
        // `tile_ranges_all` returns ranges positionally aligned with `tiles`, so the zip below
        // walks `tiles_for_bbox`'s raster order unchanged. That order is load-bearing (it is the
        // response's tile order, and the wire payload's points are a flat concatenation in it) —
        // the sweep's own Morton order stays inside `tile_ranges_all` and never reaches here.
        //
        // One sweep **per segment**, each in that segment's own Morton column. Transposed below
        // into per-tile part lists, because a tile is the union of its parts across segments
        // (`select::SelectionParts`) while the sweep's monotone advantage is per column.
        //
        // A view with zero segments (an empty build) has nothing visible in any tile: every
        // tile's part list is empty, and the response is empty — as before.
        let per_segment: Vec<Vec<Range<u32>>> = segments
            .iter()
            .map(|&(segment, _)| tile_ranges_all(segment, &tiles))
            .collect();
        let ranges: Vec<Vec<(usize, Range<u32>)>> = (0..tiles.len())
            .map(|t| {
                per_segment
                    .iter()
                    .enumerate()
                    .filter_map(|(s, sweep)| {
                        let range = sweep[t].clone();
                        (range.start < range.end).then_some((s, range))
                    })
                    .collect()
            })
            .collect();
        probe.lap(|t| &mut t.tile_ranges_ns);

        // Calibration task: the predictor decides serial-fold vs `pool.install` fan-out, and it
        // must be available BEFORE either path runs — `Σ range.len()`, the total rows every
        // resolved tile spans (pre-mask, pre-select), is exactly that: already materialised by
        // the `tile_ranges_all` sweep above, costs one pass over `ranges` to sum, and needs no
        // work from either candidate path to compute. See [`SERIAL_FALLBACK_MAX_ROWS`]'s doc for
        // why this predictor (and not tile count) is the one the sweep data supports.
        //
        // `underlay_cells_demanded` is added in, not left out. The underlay's own
        // per-cell cost is "one small binary search plus one bitmap range-count" (the underlay
        // block's own comment, below) — the same shape of operation `count_range` performs per
        // row-range, so summing the two into one row-equivalent total before comparing against
        // the threshold is the natural extension of the same predictor, not a second one bolted
        // on. Before this fix a saturated underlay (`max_underlay_cells`, default 8192) was
        // invisible to this decision entirely, regardless of how large the resulting per-tile
        // sub-cell fan-out actually was.
        //
        // §14.2 fix: this is also `StageTimings::rows_in_ranges`'s whole value, computed here
        // rather than accumulated per-tile inside `tile_sweep`. It used to be counted into each
        // tile's `TileStats` before that tile's own `visible == 0` check, but a tile that fails
        // that check returns `Ok(None)`, and `Engine::viewport`'s fold below discards `Ok(None)`
        // entirely (`let Some(tr) = outcome? else { continue };`) — so a grant that left a tile
        // empty silently dropped that tile's rows from the total, making a field documented as
        // mask-independent (`rows_in_ranges - sigma_visible` is C4's leak-register numerator, and
        // that subtraction is meaningless if the minuend already has the mask baked in) actually
        // depend on the session's mask. `ranges` is already materialised here, before the tile
        // sweep starts and before any mask is consulted, so summing it once is mask-free by
        // construction and cannot regress the same way — see `TileStats`'s doc, which no longer
        // carries this field at all, for the other half of this fix.
        let rows_in_ranges: u64 = ranges
            .iter()
            .flat_map(|parts| parts.iter())
            .map(|(_, r)| r.len() as u64)
            .sum();
        probe.count(|t| &mut t.rows_in_ranges, rows_in_ranges);
        let total_rows_in_ranges: u64 = rows_in_ranges + underlay_cells_demanded;

        // The filter, evaluated here — after the tile ranges, before the sweep. This placement is
        // load-bearing three ways. The route rule needs both its operands in hand: 0068 routes a
        // both-routes column row-space while `rows_in_ranges ≤ |M_auth|` — the request's own span
        // against the principal's own composed total, both quantities the caller could compute,
        // never a statistic about another principal's data (§8.2). The row-space leaves need the
        // request's merged tile ranges, which is what they are evaluated over. And everything
        // above this line is deliberately blind to the filter — `visible_total()` is θ's anchor
        // and stays unfiltered under **I12**, and `rows_in_ranges` is C4's leak-register numerator
        // and stays mask-free (§14.2). Both are already computed.
        //
        // **The fragment is brought forward, not read off the session.** A session's own fragment
        // is fixed at authorise, and composition treats entities below the live watermark as
        // fragment-resident — so composing against the stale one silently omits every entity
        // flushed since, and a filtered viewport under a long-lived session under-reports.
        // Narrowing, and safe under **I12**, which is exactly what makes it the dangerous kind:
        // the answer is indistinguishable from a correct one. `/v1/categories` takes the same care
        // for the same reason. It costs nothing here: `session_geometry` above already resolved
        // the same fragment on this request, so this is the identity short-circuit or a cache hit.
        let mut region_verdict: Option<crate::region::RegionVerdict> = None;
        let mask = match &req.filter {
            None => mask,
            Some(expr) => {
                check_cancelled(&cancel)?;
                let fragment = self.fragment_for(session, &generation)?;
                let candidate = crate::filter::candidate(
                    &fragment,
                    &session.satisfied,
                    &generation.overlay,
                    &generation.buffer,
                );
                // The region leaves' resolver (`crate::region`): a drawn shape through the
                // generation-keyed decomposition cache, its boundary rows tested under **this
                // request's composed mask**; a published shape through the artifact's own verdict.
                // Closed over the mask so the boundary path cannot run without one.
                let regions = |leaf: &crate::filter::RegionLeaf| {
                    self.resolve_region(
                        leaf,
                        session,
                        &generation,
                        view,
                        view_data,
                        &segments,
                        &mask,
                        denied,
                        mask_identity,
                        &cancel,
                    )
                };
                let routed = generation
                    .filter_columns
                    .evaluate_routed(expr, &candidate, rows_in_ranges <= v_total, &regions)
                    .map_err(|e| {
                        // Caller's fault or the deployment's — `FilterError` decides, at the
                        // variants, because that is where the argument for each one lives.
                        let detail = e.to_string();
                        if e.is_callers_fault() {
                            EngineError::FilterMalformed(detail)
                        } else {
                            EngineError::FilterRefused(detail)
                        }
                    })?;
                probe.lap(|t| &mut t.filter_eval_ns);
                // One crossing per request, whichever shape came back (0062's tree; 0068). The
                // row of `filter_matched` reports what the route produced: matched entities on
                // the entity route, matched rows-in-domain on the row route.
                let rows = match routed {
                    crate::filter::RoutedFilter::Entity(entities) => {
                        probe.count(|t| &mut t.filter_matched, entities.cardinality());
                        self.cross_filter_into_row_space(
                            &view_data.row_space,
                            &entities,
                            &ranges,
                            &segments,
                            rows_in_ranges,
                        )
                    }
                    crate::filter::RoutedFilter::Row(tree) => {
                        let row_bases: Vec<u32> = segments.iter().map(|&(_, base)| base).collect();
                        let domain = crossing_domain(&ranges, &row_bases);
                        region_verdict = tree.region_verdict();
                        let rows = self.evaluate_row_route(
                            &tree,
                            &view_data.row_space,
                            &segments,
                            &domain,
                            rows_in_ranges,
                            view_data.row_space.total_rows(),
                        )?;
                        self.filter_row_routed.fetch_add(1, Ordering::Relaxed);
                        // **Not counted when a region is in the tree.** Its interior rows have
                        // not met the mask yet, so the cardinality would be a pre-mask quantity
                        // about the region — the number selection-operand §7 says may not be
                        // computed, for a metric or for anything else.
                        if !tree.has_region() {
                            probe.count(|t| &mut t.filter_matched, rows.rows().cardinality());
                        }
                        rows
                    }
                };
                probe.lap(|t| &mut t.filter_cross_ns);
                mask.with_filter(rows)
            }
        };

        // The head, delivered before the sweep: everything the response headers derive from is
        // known here — the region verdict last, settled by the decomposition above and never by a
        // row — and a server that waits for the first flush before committing a status needs it
        // in hand by then. A refusal is the consumer gone — cancellation, not a fault.
        sink.head(ViewportHead {
            coordinates,
            stamp: answered_from.clone(),
            stale,
            region: region_verdict,
            render_scalars: render_scalars.to_vec(),
        })
        .map_err(|SinkClosed| EngineError::Cancelled)?;
        // Reset the clock so the head's construction and delivery are unattributed rather than
        // silently charged to the stage that follows.
        probe.skip();

        // §8.5's match-layer count rule: a filtered request serves every match, up to the cap —
        // the θ threshold clause does not thin a filtered selection. Saturating the threshold is
        // how the definition says "in full": `C_θ = |vis(T)|` by construction, so
        // `served = min(matched, cap)` per tile, with the cap-many smallest `tessera_id`s when a
        // tile is over — the same prefix rule as ever, so nesting across zooms is untouched.
        //
        // This is NOT a re-anchor. θ's anchor stays `visible_total()`, unfiltered, and §5.2 of
        // `filter-surface.md` forbids anchoring on `M_sel` (a threshold that moved as the viewer
        // typed). The rule here is the other half of the same section: the anchor never narrows,
        // and the match layer never samples. Before this, a filtered tile was pushed through the
        // unfiltered θ odds — a tile narrowed from 4,000 visible to 40 matched drew ~1% of 40,
        // i.e. the k_min floor — so the map thinned in proportion to the filter's selectivity
        // instead of showing the matches.
        let params = if req.filter.is_some() {
            SelectParams {
                threshold: Threshold::Saturated,
                ..params
            }
        } else {
            params
        };

        // D-D/D-F, calibrated: below `SERIAL_FALLBACK_MAX_ROWS`, fold `tile_sweep` in place —
        // same function, same input order, no `pool.install` — since below that line the fan-out's
        // own entry/scheduling cost exceeds the per-tile work it would parallelise (measured; see
        // the constant's doc). At or above it, the existing `pool.install` fan-out runs, on the
        // ONE shared pool this engine built at `Engine::open` — no second, per-request pool, no
        // nested throttling (D-D). Every input to `tile_sweep` is borrowed or `Copy`:
        // `mask`/`segment`/`render_scalars`/`params` are the generation- and request-derived
        // values already resolved above (lifecycle §1.1 — nothing is re-loaded per tile), and
        // `cancel` is the D-C token, checked inside `tile_sweep` at the very top (moved there
        // at the very top of that function rather than here).
        //
        // Both branches produce `Vec<Result<Option<TileResult>>>` (this crate's `Result<T>` alias
        // for `std::result::Result<T, EngineError>`), in `tiles`' order, so the fold below is
        // identical either way — this is what makes the two paths byte-identical (see this
        // module's doc; `with_min_len(TILE_PAR_MIN_LEN)` and the parallel branch's own collect
        // shape are load-bearing for THAT claim within the parallel branch itself).
        //
        // D-C cancellation bound, both branches: a `Cancelled` observed inside `tile_sweep`
        // propagates to the fold below regardless of path, which discards every result after the
        // first `Err` it walks (see the fold's own comment). What differs is how much wasted work
        // can be IN FLIGHT past the checkpoint at the instant of cancellation. Serial fold: at
        // most ONE tile — the one `tile_sweep` call currently running, since nothing else is
        // concurrently past the checkpoint by construction. Parallel fan-out: at most
        // `compute_threads` tiles (one per worker) — every tile that had already passed the
        // checkpoint keeps running to completion; every tile whose worker had not yet reached it
        // observes the flip there instead and returns immediately. The serial path's bound is
        // therefore strictly tighter, not merely no-worse.
        // One closure, not two independently-maintained copies of the same 8-argument
        // call — the duplication was a divergence risk (a future change to `tile_sweep`'s
        // argument list would need to be made twice, silently, with no compiler help if one copy
        // were missed). `run` captures only shared references and `Copy` values (`&mask`,
        // `&segments`, `&params`, `zoom`, `underlay_offset`, `&cancel`), so it is
        // `Sync` for free and usable from both the serial `Iterator::map` below and rayon's
        // parallel `map` inside `pool.install` — no new bound this file did not already require of
        // these captures for the parallel branch to compile before this change.
        let run = |tile: &Tile, tile_parts: &[(usize, Range<u32>)]| {
            tile_sweep(
                tile,
                tile_parts,
                &mask,
                &segments,
                &params,
                zoom,
                underlay_offset,
                &cancel,
            )
        };

        // The threshold is read from `self`, not the constant directly, so
        // `set_serial_fallback_max_rows_for_test` (session.rs, test-only) can override it per-
        // `Engine` — see that method's doc. **The `serial_fallback_max_rows` field and this load
        // are unconditional — present and paid in EVERY build, not just `bench-timing` ones.**
        // Only the setter method is `bench-timing`-gated; nothing outside it ever writes the
        // field, so in a build without that feature (every shipped binary) this load always
        // yields `SERIAL_FALLBACK_MAX_ROWS` — behaviourally identical to reading the constant
        // directly, at the cost of one `Relaxed` atomic load, negligible against the request's
        // own atomic operations elsewhere. Deliberately not `#[cfg]`-gated to a second code path
        // here too: that would cost more to audit than the load itself costs to run.
        let serial_fallback_max_rows = self.serial_fallback_max_rows.load(Ordering::Relaxed);
        let tile_outcomes: Vec<Result<Option<TileSweepOut>>> =
            if should_fold_serially(total_rows_in_ranges, serial_fallback_max_rows, tiles.len()) {
                tiles
                    .iter()
                    .zip(&ranges)
                    .map(|(tile, tile_parts)| run(tile, tile_parts))
                    .collect::<Vec<Result<Option<TileSweepOut>>>>()
            } else {
                self.pool.install(|| {
                    tiles
                        .par_iter()
                        .zip(ranges.par_iter())
                        .with_min_len(TILE_PAR_MIN_LEN)
                        .map(|(tile, tile_parts)| run(tile, tile_parts))
                        .collect::<Vec<Result<Option<TileSweepOut>>>>()
                })
            };
        // D-E: neither branch's own wall time is a named stage — it is already fully accounted
        // for, per tile, inside each `TileResult::stats` (folded below) — so this resets the clock
        // without charging the stretch to whatever lap runs next, rather than leaving it to be
        // silently misattributed. True of the serial branch too: its per-tile costs are equally
        // captured in `TileStats`, so `skip()` here keeps both branches' accounting symmetric.
        probe.skip();

        // D-F: the serial, IN-ORDER fold. `tile_outcomes`' order equals `tiles`' order by
        // construction (the indexed collect path above — this module's doc), so the response
        // order is the request order. Short-circuits on the
        // first `Err` (D-C's `Cancelled`, or any other per-tile error): every tile's own work is
        // already done by this point (the parallel sweep does not itself short-circuit — that is
        // the point of collecting `Vec<Result<..>>` rather than `Result<Vec<..>>`), so bailing out
        // here costs only the remaining `Result`s' worth of `?`, never any recomputation.
        let mut swept: Vec<TileSweepOut> = Vec::new();
        for outcome in tile_outcomes {
            let Some(mut ts) = outcome? else {
                continue;
            };
            ts.stats.fold_into(&mut probe.t);
            tile_counts.push(ts.count.clone());
            sub_cells.append(&mut ts.sub_cells);
            swept.push(ts);
        }

        // The first flush: every count, before any point (`streamed-serving.md` §2 — the number
        // channel is exact from the first paint). `None`, not an empty slice, when the underlay
        // was not requested: the wire's frame-presence rule needs the distinction.
        sink.counts(&tile_counts, underlay_offset.map(|_| sub_cells.as_slice()))
            .map_err(|SinkClosed| EngineError::Cancelled)?;
        probe.skip();

        // The artifacts frame, after the counts and before any point. It is an aggregate channel,
        // not a point one — a cluster's masked count belongs beside a tile's, not beside a mark.
        let (artifacts, served_layers) = self.serve_artifacts(
            session,
            &generation,
            view,
            view_data,
            &ranges,
            &mask,
            req_layers,
            artifact_budget,
            req_levels,
            req_computed,
            zoom,
            mask_identity,
            artifact_rows,
            &cancel,
        )?;
        if !artifacts.is_empty() {
            sink.artifacts(&artifacts)
                .map_err(|SinkClosed| EngineError::Cancelled)?;
        }
        probe.skip();

        // The per-point membership column, resolved once for the whole response against the
        // served set the artifacts frame just carried (`crate::membership_column`). No artifact
        // served, no work: the resolver is not built and no chunk carries a column.
        let membership = if artifacts.is_empty() {
            None
        } else {
            let gathered: Vec<u32> = swept
                .iter()
                .flat_map(|ts| ts.rows.iter().copied())
                .collect();
            let resolved = crate::membership_column::Resolved::new(gathered, &served_layers);
            (!resolved.is_empty()).then_some(resolved)
        };
        // Unattributed, as the artifact pass above is: a serial stage between two the header
        // names, measured by `tests/membership_column.rs` rather than by a stage field.
        probe.skip();

        // The emit pass: gather and hand off, serial, in response order (this module's doc says
        // why serial). The buffer is seeded from the declaration rather than from whichever tile
        // arrives first — a request whose first tile is narrower than a later one must not fix
        // the column set from it — and re-seeded identically at each flush.
        let seed = || PointColumns {
            tessera_ids: Vec::new(),
            codes: Vec::new(),
            scalars: render_scalars
                .iter()
                .map(|d| ColumnBuf::empty(d.arrow_type))
                .collect(),
            membership: membership
                .as_ref()
                .map(|m| m.empty_columns())
                .unwrap_or_default(),
        };
        let mut buf = seed();
        let mut buf_bytes = 0usize;
        for ts in &swept {
            // D-C: the emit pass's per-tile checkpoint — one atomic read, so an abandoned
            // stream stops gathering within one tile even when the sink is not refusing yet.
            check_cancelled(&cancel)?;
            let mut stats = TileProbe::new();
            let parts = SelectionParts::new(&ts.parts);
            let mut tile_points = gather_tile_columns(&parts, &ts.rows, render_scalars)?;
            if let Some(membership) = &membership {
                tile_points.membership = membership.columns_for(&ts.rows);
            }
            stats.count(|t| &mut t.points_gathered, tile_points.len() as u64);
            buf_bytes += tile_points.wire_bytes_estimate();
            if let Err((want, got)) = buf.append(tile_points) {
                return Err(EngineError::Malformed(format!(
                    "two tiles of one response hold the same declared column at different \
                     types ({want} and {got}); the bundle's segments disagree about it"
                )));
            }
            // Brackets gather-and-append only — the flush below (a channel send, under
            // streaming) must not inflate a figure documented as CPU cost.
            stats.lap(|t| &mut t.gather_ns);
            stats.t.fold_into(&mut probe.t);
            // Two guards on the threshold (both from the implementation review):
            // `!buf.is_empty()`, because a zero-row buffer can still carry estimate bytes — a
            // Utf8 column's offset table is 4 bytes at zero rows — so `k = 0` (a legal
            // counts-only request) over enough tiles would otherwise emit empty points frames
            // against the sink contract; and `MAX_POINTS_FRAME_BYTES`, so a deliberately huge
            // `flush_bytes` ("one flush per response") cannot accumulate a frame past the
            // wire's u32 length field, which is a panic there and a bounded split here.
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

        // `total_ns` (stamped by `finish`) is this call's wall clock, which under streaming
        // includes the sink's sends — consumer-paced time, not compute. Per-stage figures are
        // unaffected: gather laps bracket gather-and-append only, and no lap spans a send.
        Ok(probe.finish())
    }

    /// Cross a filter's entity-space result into one view's row space, by whichever of the two
    /// routes is cheaper for this request.
    ///
    /// **Project** — [`RowSpace::project`] — crosses the whole result and costs ~20–30 ns per set
    /// bit, so it scales with *what matched*. **Per tile** walks the rows the request's own tiles
    /// span and asks each one whether its entity matched, at ~20–29 ns per row on a clumped result
    /// and ~57–106 ns on a scattered one, so it scales with *what is on screen*. Neither dominates:
    /// at a 300,000-row viewport over 10⁸ items, project is 1.2 ms against 18.3 ms at a 10⁴ result
    /// and 216 ms against 32 ms at a 10⁷ one (`probes/2026-08-11-viewport-crossing/`).
    ///
    /// **The per-tile route is what makes a mid-to-high coverage principal affordable at scale**,
    /// which is the case it exists for. Project scales with the result, so a 10⁸-match result is
    /// ~2.2 s at 10⁹ rows — outside §2.2's 0.5–1 s filter budget outright — while the per-tile route
    /// stays in tens of milliseconds however much matched. A principal seeing half the corpus and
    /// filtering to a tenth of what they see is past the crossover, not near it.
    ///
    /// The route is **latency only**: the two answers agree exactly over every range the request
    /// can ask about, which is what [`FilterRows`] carries the domain to keep true, and what
    /// `filter_routes_agree_over_the_domain` asserts. A view that published no `row-entity.u32`
    /// cannot take the per-tile route at all and silently gets the projecting one.
    fn cross_filter_into_row_space(
        &self,
        row_space: &tessera_store::permutation::RowSpace,
        entities: &croaring::Bitmap,
        ranges: &[Vec<(usize, Range<u32>)>],
        segments: &[(&SegmentData, u32)],
        rows_in_ranges: u64,
    ) -> FilterRows {
        let per_tile_looks_cheaper =
            entities.cardinality() > rows_in_ranges.saturating_mul(PER_TILE_CROSSING_RATIO);
        if per_tile_looks_cheaper && row_space.can_invert() {
            let row_bases: Vec<u32> = segments.iter().map(|&(_, base)| base).collect();
            let domain = crossing_domain(ranges, &row_bases);
            // `None` is the row space declining to answer — a row it cannot invert, which
            // `can_invert` says should not happen and which is corruption if it does. Falling
            // through to the exact route is the right response either way: it costs latency and
            // nothing else, where trusting a partial answer would drop rows from the map.
            if let Some(rows) = self
                .pool
                .install(|| per_tile_crossing(row_space, entities, &domain, rows_in_ranges))
            {
                self.filter_crossings_per_tile
                    .fetch_add(1, Ordering::Relaxed);
                return FilterRows::Viewport { rows, domain };
            }
        }
        self.filter_crossings_projected
            .fetch_add(1, Ordering::Relaxed);
        FilterRows::Complete(row_space.project(entities))
    }

    /// Evaluate a routed filter tree with row-space leaves over the request's own rows — the
    /// render-column route (decision 0068, records §6.2), exact over `domain` and silent outside
    /// it.
    ///
    /// **The leaves read the hot column and nothing else.** A leaf is the dense variant the
    /// placement memo prefers for its channel argument: every row of the domain is read whatever
    /// the principal may see, so the work is a function of the request's ranges and the column
    /// alone — never of the mask and never of the value sought. Code 0 is the vocabulary's real
    /// absent sentinel and matches **nothing**: not a value list containing it (an unresolvable
    /// key parses to 0 precisely so it matches no row), and not a `none_of`'s presence half.
    /// This is the row-path statement of the rule the entity path keeps via its presence bitmap —
    /// the 2026-08-11 absent-as-zero defect must not return by this route.
    ///
    /// **The composed verdict is the candidate, by construction** (records §6, review N2): the
    /// bitmap returned here still contains suppressed rows — the hot column holds them, Rule S
    /// says it must — and it narrows the request only through `EffectiveMask::with_filter`, whose
    /// every consumer intersects it with the composed mask last. The entity-space verdicts inside
    /// `tree` were evaluated under the composed candidate before they got here. The suppression
    /// differential in `tests/filtering.rs` pins both halves.
    ///
    /// **One crossing per request** (0062's composition; placement memo §2.2): every
    /// entity-space verdict in the tree is crossed in a single joint walk — or a projection per
    /// verdict when the measured rule says the result side is cheaper — and the tree then
    /// combines entirely in row space. Evaluation runs on the engine's one shared pool, split
    /// over the domain exactly as the per-tile crossing splits, which is the "existing
    /// parallelism" records §6.2 prices the coarse-zoom cell against.
    ///
    /// **The result's extent is the tree's.** A tree of region leaves and projected entity
    /// verdicts answers over the whole view and comes back [`FilterRows::Complete`]; a render
    /// leaf anywhere in it, or a per-tile crossing, bounds the answer to the request's domain
    /// and it comes back [`FilterRows::Viewport`] (selection-operand §5).
    fn evaluate_row_route(
        &self,
        tree: &crate::filter::RowExpr,
        row_space: &tessera_store::permutation::RowSpace,
        segments: &[(&SegmentData, u32)],
        domain: &[Range<u32>],
        rows_in_ranges: u64,
        total_rows: u64,
    ) -> Result<FilterRows> {
        // The one crossing: every entity-space verdict's row image, computed together. The route
        // between the two crossing shapes is the measured rule the single-operand path uses,
        // summed over the verdicts because that is what the projection would cost.
        let verdicts = tree.entity_verdicts();
        let mut whole_view = tree.is_whole_view();
        let images: Vec<croaring::Bitmap> = if verdicts.is_empty() {
            Vec::new()
        } else {
            let total_matched: u64 = verdicts.iter().map(|v| v.cardinality()).sum();
            let per_tile_looks_cheaper =
                total_matched > rows_in_ranges.saturating_mul(PER_TILE_CROSSING_RATIO);
            let walked = (per_tile_looks_cheaper && row_space.can_invert())
                .then(|| {
                    self.pool.install(|| {
                        per_tile_crossing_multi(row_space, &verdicts, domain, rows_in_ranges)
                    })
                })
                .flatten();
            match walked {
                Some(images) => {
                    self.filter_crossings_per_tile
                        .fetch_add(1, Ordering::Relaxed);
                    // A walk over the request's rows is silent outside them, whatever else the
                    // tree holds.
                    whole_view = false;
                    images
                }
                None => {
                    self.filter_crossings_projected
                        .fetch_add(1, Ordering::Relaxed);
                    if whole_view {
                        // Projection crosses each verdict whole, and with nothing in the tree
                        // bounded by the domain, whole is what the answer is.
                        verdicts.iter().map(|v| row_space.project(v)).collect()
                    } else {
                        // Clamped to the domain so the combined answer never claims a row
                        // outside what `FilterRows::Viewport` says was tested.
                        let mut domain_rows = croaring::Bitmap::new();
                        for range in domain {
                            domain_rows.add_range(range.clone());
                        }
                        verdicts
                            .iter()
                            .map(|v| row_space.project(v).and(&domain_rows))
                            .collect()
                    }
                }
            }
        };
        // What a negated region's presence half is, and what a region's rows are clamped to
        // where the tree is domain-bounded: the whole view, or the request's own rows.
        let scope = if whole_view {
            RowScope::WholeView {
                total_rows: u32::try_from(total_rows).unwrap_or(u32::MAX),
            }
        } else {
            let mut domain_rows = croaring::Bitmap::new();
            for range in domain {
                domain_rows.add_range(range.clone());
            }
            RowScope::Domain(domain_rows)
        };
        let mut next_image = 0usize;
        let rows = self
            .pool
            .install(|| eval_row_expr(tree, &images, &mut next_image, segments, domain, &scope))?;
        Ok(if whole_view {
            FilterRows::Complete(rows)
        } else {
            FilterRows::Viewport {
                rows,
                domain: domain.to_vec(),
            }
        })
    }
}

/// The rows a row-space evaluation answers over — see [`Engine::evaluate_row_route`].
enum RowScope {
    /// Every row of the view: the tree holds nothing the request's domain bounds.
    WholeView { total_rows: u32 },
    /// The request's own rows, as one bitmap, because a sibling leaf is bounded by them.
    Domain(croaring::Bitmap),
}

impl RowScope {
    /// Every row in scope — a negated region's presence half.
    fn all_rows(&self) -> croaring::Bitmap {
        match self {
            RowScope::WholeView { total_rows } => croaring::Bitmap::from_range(0..*total_rows),
            RowScope::Domain(rows) => rows.clone(),
        }
    }

    /// A whole-view row set, narrowed to the scope where the scope is narrower.
    fn clamp(&self, rows: &croaring::Bitmap) -> croaring::Bitmap {
        match self {
            RowScope::WholeView { .. } => rows.clone(),
            RowScope::Domain(domain) => rows.and(domain),
        }
    }
}

/// Evaluate one routed node over `domain`, in row space. `images` are the pre-crossed row images
/// of the tree's entity-space verdicts, consumed in the same pre-order
/// [`crate::filter::RowExpr::entity_verdicts`] collects them — `next_image` is that cursor.
fn eval_row_expr(
    expr: &crate::filter::RowExpr,
    images: &[croaring::Bitmap],
    next_image: &mut usize,
    segments: &[(&SegmentData, u32)],
    domain: &[Range<u32>],
    scope: &RowScope,
) -> Result<croaring::Bitmap> {
    use crate::filter::RowExpr;
    match expr {
        RowExpr::Entity(_) => {
            let image = images[*next_image].clone();
            *next_image += 1;
            Ok(image)
        }
        RowExpr::Leaf {
            column,
            family,
            operand,
        } => {
            let values = LeafValues::of(*family, operand);
            scan_rows(segments, domain, column, values.predicate())
        }
        RowExpr::Region(region) => Ok(scope.clamp(&region.rows)),
        RowExpr::NotInRegion(kids) => {
            // The complement within the scope: every rowed entity carries a position, so the
            // presence half of this negation is every row (selection-operand §5). No early exit
            // on an empty difference — the image cursor's positional rule is simpler kept whole
            // here than skipped, and a region leaf's kids are already resolved.
            let mut out = scope.all_rows();
            for kid in kids {
                out.andnot_inplace(&eval_row_expr(kid, images, next_image, segments, domain, scope)?);
            }
            Ok(out)
        }
        RowExpr::AllOf(kids) => {
            let mut out: Option<croaring::Bitmap> = None;
            for kid in kids {
                let kid_rows = eval_row_expr(kid, images, next_image, segments, domain, scope)?;
                out = Some(match out {
                    None => kid_rows,
                    Some(mut acc) => {
                        acc.and_inplace(&kid_rows);
                        acc
                    }
                });
            }
            // Unreachable empty: an empty `all_of` is entity-pure and never routes here.
            Ok(out.unwrap_or_default())
        }
        RowExpr::AnyOf(kids) => {
            let mut out = croaring::Bitmap::new();
            for kid in kids {
                out |= eval_row_expr(kid, images, next_image, segments, domain, scope)?;
            }
            Ok(out)
        }
        RowExpr::NoneOf {
            column,
            family,
            kids,
        } => {
            // `present ∖ matched` — the positive predicate, in row space, presence being whatever
            // this column's family stores it as: a non-sentinel code for a category, the presence
            // bitmap for every other. Either way an absent item matches no negation, and a row
            // that cannot be read under-reports rather than widening (I12's sign, exactly as the
            // entity path argues it).
            let mut out = scan_rows(segments, domain, column, RowPredicate::present_in(*family))?;
            for (i, kid) in kids.iter().enumerate() {
                out.andnot_inplace(&eval_row_expr(kid, images, next_image, segments, domain, scope)?);
                if out.is_empty() {
                    // Nothing below can widen an empty difference, so the remaining kids are not
                    // evaluated — **but `images` is positional and their verdicts are still in
                    // it**. `entity_verdicts` collects every `Entity` node in the tree whether or
                    // not evaluation reaches it, so leaving the cursor here would hand the next
                    // `Entity` anywhere in the tree someone else's image: a filter that silently
                    // answers with a different clause's verdict, or with the candidate itself.
                    // Reachable — a kid is normally a row leaf on this one column, but an empty
                    // combinator is entity-pure by construction and `check_negations` admits it,
                    // since it contributes no column to the one-column rule.
                    for skipped in &kids[i + 1..] {
                        *next_image += skipped.entity_verdicts().len();
                    }
                    break;
                }
            }
            Ok(out)
        }
    }
}

/// A row-space leaf's test against one row of the hot column.
///
/// **Absence is a per-family rule, and it is carried here rather than inferred.** A category's
/// absence is its vocabulary's reserved code 0, held in the column itself. Every other family's is
/// decision 0064's presence bitmap beside the column: the hot column is non-nullable, so an absent
/// number is written as the type's zero, which is an ordinary value — and a range containing zero
/// would otherwise match every row that has no value at all (the 2026-08-11 defect, on this route).
enum RowPredicate<'a> {
    /// A category's code is non-sentinel and in this set. An empty set matches nothing.
    CodeIn(&'a [u32]),
    /// A category's code is non-sentinel — the presence half of a negation over one.
    CodePresent,
    /// A number's value equals one of these. An empty set matches nothing.
    NumberIn(&'a [Scalar]),
    /// A number's value lies between these bounds. Either may be absent, which is an open side,
    /// and each carries its own inclusivity — [`crate::filter::FilterOperand::Range`]'s semantics,
    /// which the entity route reads the same bounds by.
    Range {
        lo: Option<Endpoint>,
        hi: Option<Endpoint>,
    },
    /// The row carries a value, whatever it is — the presence half of a negation over a column
    /// whose absence lives in the bitmap, where the stored bytes say nothing at all.
    ValuePresent,
}

impl RowPredicate<'_> {
    /// The presence half of a negation over a column of this family.
    fn present_in(family: Family) -> RowPredicate<'static> {
        match family {
            Family::Category => RowPredicate::CodePresent,
            Family::Numeric => RowPredicate::ValuePresent,
            // A string column is never row-placed, and text is not even entity-space: `render` is
            // refused on both at the schema. An empty code set is the fail-closed reading if one
            // ever arrived.
            Family::Keyword | Family::Text => RowPredicate::CodeIn(&[]),
        }
    }

    /// Does this family read absence from the presence bitmap? A category does not: its absence is
    /// a code in the column, and it has no bitmap by construction (`render_presence`'s module doc).
    fn reads_presence(&self) -> bool {
        match self {
            RowPredicate::CodeIn(_) | RowPredicate::CodePresent => false,
            RowPredicate::NumberIn(_) | RowPredicate::Range { .. } | RowPredicate::ValuePresent => {
                true
            }
        }
    }
}

/// One row-space leaf's comparands, owned for as long as the scan borrows them.
///
/// A family/operand pair the parse would have refused becomes an empty set, which matches nothing:
/// the second line of defence the entity-space scan keeps for the same reason (`filter.rs`'s
/// `codes_of`), never a panic and never a number compared against a code.
enum LeafValues {
    Codes(Vec<u32>),
    Numbers(Vec<Scalar>),
    Range {
        lo: Option<Endpoint>,
        hi: Option<Endpoint>,
    },
}

impl LeafValues {
    fn of(family: Family, operand: &FilterOperand) -> LeafValues {
        match (family, operand) {
            (Family::Category, FilterOperand::Equals(v)) => LeafValues::Codes(vec![v.raw()]),
            (Family::Category, FilterOperand::In(vs)) => {
                LeafValues::Codes(vs.iter().map(|v| v.raw()).collect())
            }
            (Family::Numeric, FilterOperand::NumEquals(n)) => LeafValues::Numbers(vec![*n]),
            (Family::Numeric, FilterOperand::NumIn(ns)) => LeafValues::Numbers(ns.clone()),
            (Family::Numeric, FilterOperand::Range { lo, hi }) => {
                LeafValues::Range { lo: *lo, hi: *hi }
            }
            _ => LeafValues::Codes(Vec::new()),
        }
    }

    fn predicate(&self) -> RowPredicate<'_> {
        match self {
            LeafValues::Codes(codes) => RowPredicate::CodeIn(codes),
            LeafValues::Numbers(numbers) => RowPredicate::NumberIn(numbers),
            LeafValues::Range { lo, hi } => RowPredicate::Range { lo: *lo, hi: *hi },
        }
    }
}

/// One render column's typed slice per segment — resolved once per leaf evaluation, exactly as
/// the gather resolves per segment rather than per row.
///
/// Every declarable type but `utf8`, which the schema refuses from the hot column outright. A
/// category is one of the three unsigned widths; the rest are a number, a datetime or a bool.
enum HotSlice<'a> {
    Bool(&'a arrow::array::BooleanArray),
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
    /// Microseconds since the epoch — an `i64`, compared as one, exactly as the entity route
    /// compares it.
    TimestampUs(&'a [i64]),
}

/// One contiguous run of rows inside one segment: the rows that match the predicate **and** carry
/// a value.
///
/// **The presence bitmap is intersected once per run, outside the row loop.** `present` is this
/// segment's presence for the column, already shifted into view row space by
/// [`scan_rows`], and `None` means every row carries a value — the representation an absent file
/// has, so the common column costs neither bytes nor an intersection. Testing presence per row
/// instead would put a bitmap lookup inside the loop the hoist below exists to keep flat.
#[inline]
fn scan_run(
    slice: &HotSlice<'_>,
    base: u32,
    run: Range<u32>,
    predicate: &RowPredicate<'_>,
    present: Option<&croaring::Bitmap>,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    match present {
        None => match_run(slice, base, run, predicate, rows, buf),
        Some(present) => {
            let mut matched = croaring::Bitmap::new();
            match_run(slice, base, run, predicate, &mut matched, buf);
            matched.and_inplace(present);
            rows.or_inplace(&matched);
        }
    }
}

/// One contiguous run of rows, tested against the predicate alone — presence is [`scan_run`]'s.
///
/// **The dispatch is hoisted out of the row loop, and that is the whole point of this function.**
/// The obvious shape — resolve the segment, match the stored width and match the predicate for
/// each row in turn — costs about four branches and two bounds checks per row, none of them
/// hoistable, and it measured 2.5–3.4 ns per row against the 0.48–0.73 ns a flat compare reaches
/// (`docs/evidence/memos/2026-08-12-records-and-search-epic-1-measurements.md` §2). The tell in
/// that data is that the constant was **insensitive to the code width**: a loop bound by moving
/// one or two bytes per row would not be, so the loop was bound by its own branching. Deciding the
/// width and the predicate once per run leaves a monomorphic compare over a slice, which is the
/// loop the probe measured — and a range's bounds are narrowed to the column's own type in the
/// same hoist, so no comparison widens a value.
///
/// A category's absent sentinel keeps its rule at every instantiation: code 0 matches nothing —
/// not a value list that names it, not the presence half of a negation. `run_matching` never sees
/// it: each caller below excludes it before the loop, which is the same statement made where it
/// cannot cost a comparison per row.
///
/// `buf` is empty on entry and on return. It is a parameter so its allocation is reused across the
/// runs of a chunk, never to carry rows between them: a run's matches must be complete before
/// [`scan_run`] intersects them with presence.
fn match_run(
    slice: &HotSlice<'_>,
    base: u32,
    run: Range<u32>,
    predicate: &RowPredicate<'_>,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    let span = (run.start - base) as usize..(run.end - base) as usize;
    match predicate {
        RowPredicate::CodeIn(codes) => code_run(slice, span, run.start, codes, rows, buf),
        RowPredicate::CodePresent => code_present_run(slice, span, run.start, rows, buf),
        RowPredicate::NumberIn(needles) => number_run(slice, span, run.start, needles, rows, buf),
        RowPredicate::Range { lo, hi } => range_run(slice, span, run.start, *lo, *hi, rows, buf),
        // No value is consulted: for this family the column says nothing about absence, so every
        // row of the run is present unless the bitmap [`scan_run`] intersects says otherwise.
        RowPredicate::ValuePresent => {
            rows.add_range(run);
            return;
        }
    }
    rows.add_many(buf);
    buf.clear();
}

/// A category's codes. Any slice that is not one of the three code widths matches nothing: a
/// category is stored at one of them, so anything else is a tail that disagrees with the
/// declaration, and comparing a float to a code would be worse than answering short.
#[inline]
fn code_run(
    slice: &HotSlice<'_>,
    span: Range<usize>,
    first_row: u32,
    codes: &[u32],
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    // The common shape by far — `eq`, and `in` over a single surviving code. One comparison per
    // row against a constant.
    if codes.len() == 1 {
        let needle = codes[0];
        if needle == 0 {
            return; // The sentinel names no row; the whole run is a non-match.
        }
        match slice {
            // A needle outside the column's code space matches no row, and the width test happens
            // once per run rather than once per comparison.
            HotSlice::U8(v) => {
                if let Ok(n) = u8::try_from(needle) {
                    run_matching(&v[span], first_row, rows, buf, |c| *c == n);
                }
            }
            HotSlice::U16(v) => {
                if let Ok(n) = u16::try_from(needle) {
                    run_matching(&v[span], first_row, rows, buf, |c| *c == n);
                }
            }
            HotSlice::U32(v) => run_matching(&v[span], first_row, rows, buf, |c| *c == needle),
            _ => {}
        }
        return;
    }
    match slice {
        HotSlice::U8(v) => run_matching(&v[span], first_row, rows, buf, |c| {
            *c != 0 && codes.contains(&u32::from(*c))
        }),
        HotSlice::U16(v) => run_matching(&v[span], first_row, rows, buf, |c| {
            *c != 0 && codes.contains(&u32::from(*c))
        }),
        HotSlice::U32(v) => run_matching(&v[span], first_row, rows, buf, |c| {
            *c != 0 && codes.contains(c)
        }),
        _ => {}
    }
}

/// A category carries a value: a non-sentinel code.
#[inline]
fn code_present_run(
    slice: &HotSlice<'_>,
    span: Range<usize>,
    first_row: u32,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    match slice {
        HotSlice::U8(v) => run_matching(&v[span], first_row, rows, buf, |c| *c != 0),
        HotSlice::U16(v) => run_matching(&v[span], first_row, rows, buf, |c| *c != 0),
        HotSlice::U32(v) => run_matching(&v[span], first_row, rows, buf, |c| *c != 0),
        _ => {}
    }
}

/// A number's value lies within the bounds — the row-space transcription of
/// `ValueColumn::scan_range`, and it must stay one.
///
/// **A deliberate second copy of the narrowing, across a crate boundary.** `tessera-filter`'s is
/// private to the entity-space column, and the two routes must agree exactly over the domain or
/// 0068's licence to choose a route on cost alone fails. The rules copied here are the ones that
/// are wrong in silence if they drift: an exclusive integer bound is folded by one step; a
/// fractional bound rounds *into* the constraint (`> 3.2` and `>= 3.2` both admit 4); a NaN bound
/// satisfies nothing; a bound past the type's floor or ceiling is no constraint or no match rather
/// than a wrapped comparison. `the_row_route_and_the_entity_route_agree_over_the_domain` is what
/// holds the copies together, over a numeric predicate as well as a category one.
#[inline]
fn range_run(
    slice: &HotSlice<'_>,
    span: Range<usize>,
    first_row: u32,
    lo: Option<Endpoint>,
    hi: Option<Endpoint>,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    macro_rules! int_range {
        ($v:expr, $t:ty) => {{
            let lo_b = match narrow_lo::<$t>(lo) {
                Narrowed::Unsatisfiable => return,
                Narrowed::Unbounded => None,
                Narrowed::At(x) => Some(x),
            };
            let hi_b = match narrow_hi::<$t>(hi) {
                Narrowed::Unsatisfiable => return,
                Narrowed::Unbounded => None,
                Narrowed::At(x) => Some(x),
            };
            run_matching($v, first_row, rows, buf, move |x| {
                lo_b.is_none_or(|b| *x >= b) && hi_b.is_none_or(|b| *x <= b)
            })
        }};
    }
    // Floats keep the `f64` comparison: NaN must stay unordered, and narrowing through an integer
    // would destroy that.
    macro_rules! float_range {
        ($v:expr) => {{
            let lo_f = lo.map(|e| (as_f64(e.value), e.inclusive));
            let hi_f = hi.map(|e| (as_f64(e.value), e.inclusive));
            run_matching($v, first_row, rows, buf, move |x| {
                let x = *x as f64;
                lo_f.is_none_or(|(b, inc)| if inc { x >= b } else { x > b })
                    && hi_f.is_none_or(|(b, inc)| if inc { x <= b } else { x < b })
            })
        }};
    }
    match slice {
        HotSlice::Bool(a) => {
            // A bool is compared as the 0/1 the entity route stores it as, so `>= 1` means true on
            // both — the mapping is `u8::from`, in one place on each side.
            let lo_b = match narrow_lo::<u8>(lo) {
                Narrowed::Unsatisfiable => return,
                Narrowed::Unbounded => None,
                Narrowed::At(x) => Some(x),
            };
            let hi_b = match narrow_hi::<u8>(hi) {
                Narrowed::Unsatisfiable => return,
                Narrowed::Unbounded => None,
                Narrowed::At(x) => Some(x),
            };
            bool_matching(a, span, first_row, rows, buf, move |x| {
                lo_b.is_none_or(|b| x >= b) && hi_b.is_none_or(|b| x <= b)
            })
        }
        HotSlice::U8(v) => int_range!(&v[span], u8),
        HotSlice::U16(v) => int_range!(&v[span], u16),
        HotSlice::U32(v) => int_range!(&v[span], u32),
        HotSlice::U64(v) => int_range!(&v[span], u64),
        HotSlice::I8(v) => int_range!(&v[span], i8),
        HotSlice::I16(v) => int_range!(&v[span], i16),
        HotSlice::I32(v) => int_range!(&v[span], i32),
        HotSlice::I64(v) | HotSlice::TimestampUs(v) => int_range!(&v[span], i64),
        HotSlice::F32(v) => float_range!(&v[span]),
        HotSlice::F64(v) => float_range!(&v[span]),
    }
}

/// A number's value equals one of the needles — the row-space transcription of
/// `ValueColumn::scan_num_in`, with the same rules: a needle the column's type cannot hold matches
/// nothing and is dropped before the loop rather than compared away per row, and the survivors are
/// sorted and searched because a linear `contains` costs O(needles) per row.
#[inline]
fn number_run(
    slice: &HotSlice<'_>,
    span: Range<usize>,
    first_row: u32,
    needles: &[Scalar],
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    macro_rules! int_in {
        ($v:expr, $t:ty) => {{
            let mut w: Vec<$t> = needles
                .iter()
                .filter_map(|n| match n {
                    Scalar::Int(i) => <$t>::try_from(*i).ok(),
                    Scalar::Float(_) => None,
                })
                .collect();
            if w.is_empty() {
                return;
            }
            w.sort_unstable();
            w.dedup();
            run_matching($v, first_row, rows, buf, move |x| {
                w.binary_search(x).is_ok()
            })
        }};
    }
    macro_rules! float_in {
        ($v:expr) => {{
            // NaN equals nothing, itself included — so a NaN needle matches no row, which the
            // comparison gives without a special case.
            let w: Vec<f64> = needles.iter().map(|n| as_f64(*n)).collect();
            run_matching($v, first_row, rows, buf, move |x| {
                let x = *x as f64;
                w.iter().any(|n| x == *n)
            })
        }};
    }
    match slice {
        HotSlice::Bool(a) => {
            let mut w: Vec<u8> = needles
                .iter()
                .filter_map(|n| match n {
                    Scalar::Int(i) => u8::try_from(*i).ok(),
                    Scalar::Float(_) => None,
                })
                .collect();
            if w.is_empty() {
                return;
            }
            w.sort_unstable();
            w.dedup();
            bool_matching(a, span, first_row, rows, buf, move |x| {
                w.binary_search(&x).is_ok()
            })
        }
        HotSlice::U8(v) => int_in!(&v[span], u8),
        HotSlice::U16(v) => int_in!(&v[span], u16),
        HotSlice::U32(v) => int_in!(&v[span], u32),
        HotSlice::U64(v) => int_in!(&v[span], u64),
        HotSlice::I8(v) => int_in!(&v[span], i8),
        HotSlice::I16(v) => int_in!(&v[span], i16),
        HotSlice::I32(v) => int_in!(&v[span], i32),
        HotSlice::I64(v) | HotSlice::TimestampUs(v) => int_in!(&v[span], i64),
        HotSlice::F32(v) => float_in!(&v[span]),
        HotSlice::F64(v) => float_in!(&v[span]),
    }
}

/// The monomorphic inner loop every flat-slice arm above resolves to: one slice, one test, one
/// buffered flush. Generic over the stored type so each width compiles to its own loop.
#[inline]
fn run_matching<T: Copy>(
    values: &[T],
    first_row: u32,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
    matches: impl Fn(&T) -> bool,
) {
    for (offset, code) in values.iter().enumerate() {
        if matches(code) {
            buf.push(first_row + offset as u32);
            if buf.len() == 1024 {
                rows.add_many(buf);
                buf.clear();
            }
        }
    }
}

/// [`run_matching`] for the one fixed-width type Arrow does not store as a flat slice of itself.
/// The test is hoisted exactly as the others are; what differs is only the bit extraction.
#[inline]
fn bool_matching(
    values: &arrow::array::BooleanArray,
    span: Range<usize>,
    first_row: u32,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
    matches: impl Fn(u8) -> bool,
) {
    let start = span.start;
    for idx in span {
        if matches(u8::from(values.value(idx))) {
            buf.push(first_row + (idx - start) as u32);
            if buf.len() == 1024 {
                rows.add_many(buf);
                buf.clear();
            }
        }
    }
}

/// What a range bound becomes once narrowed to the column's own type — see [`range_run`] for why
/// this mirrors `tessera-filter`'s private original rather than calling it.
enum Narrowed<T> {
    /// No constraint on this side — the bound lies beyond the type's range in the permissive
    /// direction, or was absent.
    Unbounded,
    /// Nothing can satisfy it: the bound lies beyond the type's range in the excluding direction.
    Unsatisfiable,
    /// An inclusive native bound. Exclusivity is folded in by moving the bound one step, which is
    /// exact for integers.
    At(T),
}

/// The integer widths' extremes as `i128`, so the narrowing can tell "below the floor" (no
/// constraint) from "above the ceiling" (nothing matches) without a per-type arm.
trait NativeBound {
    fn min_i128() -> i128;
    fn max_i128() -> i128;
}
macro_rules! native_bound {
    ($($t:ty),*) => { $(impl NativeBound for $t {
        fn min_i128() -> i128 { <$t>::MIN as i128 }
        fn max_i128() -> i128 { <$t>::MAX as i128 }
    })* };
}
native_bound!(u8, u16, u32, u64, i8, i16, i32, i64);

/// The lower bound as an **inclusive** native value.
fn narrow_lo<T>(e: Option<Endpoint>) -> Narrowed<T>
where
    T: TryFrom<i128> + NativeBound,
{
    let Some(e) = e else {
        return Narrowed::Unbounded;
    };
    // `gt x` over integers is `gte x+1`; the saturating add keeps the shift exact at the ceiling,
    // where `x+1` would not exist and the answer is "nothing above it".
    let want = match e.value {
        Scalar::Int(i) if e.inclusive => i,
        Scalar::Int(i) => i.saturating_add(1),
        // A fractional lower bound rounds *up* to the next integer the column can hold: `> 3.2`
        // and `>= 3.2` both admit 4 and exclude 3.
        Scalar::Float(f) => {
            if f.is_nan() {
                return Narrowed::Unsatisfiable;
            }
            f.ceil() as i128
        }
    };
    match T::try_from(want) {
        Ok(v) => Narrowed::At(v),
        // Below the floor: every value satisfies it. Above the ceiling: none does.
        Err(_) if want < T::min_i128() => Narrowed::Unbounded,
        Err(_) => Narrowed::Unsatisfiable,
    }
}

/// The upper bound as an **inclusive** native value.
fn narrow_hi<T>(e: Option<Endpoint>) -> Narrowed<T>
where
    T: TryFrom<i128> + NativeBound,
{
    let Some(e) = e else {
        return Narrowed::Unbounded;
    };
    let want = match e.value {
        Scalar::Int(i) if e.inclusive => i,
        Scalar::Int(i) => i.saturating_sub(1),
        Scalar::Float(f) => {
            if f.is_nan() {
                return Narrowed::Unsatisfiable;
            }
            f.floor() as i128
        }
    };
    match T::try_from(want) {
        Ok(v) => Narrowed::At(v),
        Err(_) if want > T::max_i128() => Narrowed::Unbounded,
        Err(_) => Narrowed::Unsatisfiable,
    }
}

fn as_f64(s: Scalar) -> f64 {
    match s {
        Scalar::Int(i) => i as f64,
        Scalar::Float(f) => f,
    }
}

/// One segment's share of a row-space leaf: where its rows begin, the column's values, and which
/// of those rows carry one.
struct ScannedSegment<'a> {
    row_base: u32,
    values: HotSlice<'a>,
    /// The rows that carry a value, **in view row space** — the presence bitmap shifted by
    /// `row_base` once, here, rather than per run. `None` where every row does.
    present: Option<croaring::Bitmap>,
}

/// Test every row of `domain` against `column`'s hot values — the render-column scan, parallel
/// over the domain on the caller's installed pool, chunked exactly as the per-tile crossing is.
///
/// A segment that does not hold the column at a fixed width is a **malformed bundle**, refused
/// like the gather's equivalent: serving it as "matches nothing" would be an answer about values
/// that were never read.
fn scan_rows(
    segments: &[(&SegmentData, u32)],
    domain: &[Range<u32>],
    column: &str,
    predicate: RowPredicate<'_>,
) -> Result<croaring::Bitmap> {
    // Per-segment slices and presence, resolved once. `segments` is ascending by `row_base`
    // (`segments_with_row_bases` sorts), which the per-row resolution below relies on.
    let slices: Vec<ScannedSegment<'_>> = segments
        .iter()
        .map(|&(segment, row_base)| {
            let values = match segment.columns.scalar(column) {
                Some(ScalarSlice::Bool(a)) => HotSlice::Bool(a),
                Some(ScalarSlice::U8(s)) => HotSlice::U8(s),
                Some(ScalarSlice::U16(s)) => HotSlice::U16(s),
                Some(ScalarSlice::U32(s)) => HotSlice::U32(s),
                Some(ScalarSlice::U64(s)) => HotSlice::U64(s),
                Some(ScalarSlice::I8(s)) => HotSlice::I8(s),
                Some(ScalarSlice::I16(s)) => HotSlice::I16(s),
                Some(ScalarSlice::I32(s)) => HotSlice::I32(s),
                Some(ScalarSlice::I64(s)) => HotSlice::I64(s),
                Some(ScalarSlice::F32(s)) => HotSlice::F32(s),
                Some(ScalarSlice::F64(s)) => HotSlice::F64(s),
                Some(ScalarSlice::TimestampUs(s)) => HotSlice::TimestampUs(s),
                // `utf8` and a column the tail does not hold alike: the schema refuses `render` on
                // a string, so either way the segment and the manifest disagree about the tail.
                _ => {
                    return Err(EngineError::Malformed(format!(
                        "a segment of this view has no rendered column '{column}' at a fixed \
                         width, which the routed filter requires; the manifest and the segment \
                         disagree about the tail"
                    )))
                }
            };
            // Only for a family that stores absence beside the column. A category's absence is a
            // code in the column itself and it has no bitmap at all, so asking for one would be
            // the sentinel-and-bitmap muddle decision 0064 declines.
            let present = predicate
                .reads_presence()
                .then(|| present_rows(segment, column, row_base))
                .flatten();
            Ok(ScannedSegment {
                row_base,
                values,
                present,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let chunks = domain_chunks(domain, domain.iter().map(|r| r.len() as u64).sum());
    let parts: Vec<croaring::Bitmap> = chunks
        .par_iter()
        .map(|chunk| {
            let mut rows = croaring::Bitmap::new();
            let mut buf: Vec<u32> = Vec::with_capacity(1024);
            // The segment owning `chunk.start`, advanced as the walk crosses a boundary — the
            // domain's ranges never span rows outside a segment, but a *merged* range can span
            // two adjacent segments.
            let mut seg = slices.partition_point(|s| s.row_base <= chunk.start) - 1;
            let mut row = chunk.start;
            while row < chunk.end {
                while seg + 1 < slices.len() && slices[seg + 1].row_base <= row {
                    seg += 1;
                }
                // The run this segment owns: to the next segment's base, or the chunk's end.
                let seg_end = slices
                    .get(seg + 1)
                    .map_or(chunk.end, |next| next.row_base.min(chunk.end));
                let segment = &slices[seg];
                scan_run(
                    &segment.values,
                    segment.row_base,
                    row..seg_end,
                    &predicate,
                    segment.present.as_ref(),
                    &mut rows,
                    &mut buf,
                );
                row = seg_end;
            }
            rows
        })
        .collect();
    let refs: Vec<&croaring::Bitmap> = parts.iter().collect();
    Ok(croaring::Bitmap::fast_or(&refs))
}

/// The rows of one segment that carry a value for `column`, **in view row space** — `None` where
/// every row does.
///
/// `ColumnsRef::presence` answers for a column with no file, and for a name it does not know, with
/// an all-present bitmap — so there is no branch here and no way for a caller to read a missing
/// artefact as an absence. A damaged bitmap has already refused, at `ColumnsRef::load`.
///
/// The shift into view row space belongs here rather than in the scan: the bitmap is over the
/// segment's own `0..row_count` (`render_presence`'s module doc — a merge permutes rows, so it can
/// be nothing else), and shifting once per segment keeps the run loop comparing bitmaps in one
/// numbering.
fn present_rows(segment: &SegmentData, column: &str, row_base: u32) -> Option<croaring::Bitmap> {
    segment
        .columns
        .presence(column)
        .bitmap()
        .map(|rows| rows.add_offset(i64::from(row_base)))
}

/// How many times larger than the viewport a filter result must be before the per-tile crossing is
/// taken instead of projecting — the crossover of [`Engine::cross_filter_into_row_space`]'s two
/// cost curves, expressed as a ratio because that is what the measurement supports.
///
/// **Measured range 1–5, and this sits at the high end deliberately.** The crossover is 1× the
/// viewport's rows for a result contiguous in entity space and 3–5× for a scattered one
/// (`probes/2026-08-11-viewport-crossing/`), and the realistic case for an ingest-ordered column is
/// scattered: entity ids are assigned in permission-signature order and are uncorrelated with any
/// attribute. Sitting at 3 keeps the exact-everywhere route in play a little longer than the
/// contiguous case would justify, which is the cheap direction to be wrong in — the loss is
/// milliseconds either side of the crossover, while the win the route exists for is two orders of
/// magnitude out (216 ms against 32 ms at a 10⁷ result).
///
/// **Not measured: how this moves with thread count.** The probe was single-threaded and both
/// routes parallelise, each over its own axis — project over the result, the per-tile crossing over
/// the viewport — so the ratio is *modelled* to survive, not shown to.
/// `Engine::filter_crossing_routes` is the observable that would catch it being wrong in a way a
/// bench never reproduces.
/// How long a chain of dependencies one request will follow.
///
/// **A backstop, not a limit anyone should reach.** A dependency graph is acyclic by construction —
/// a layer is registered only after every layer it names in `depends_on` — so a real chain is
/// bounded by the number of declared layers and is one or two links deep in practice. This bounds
/// the recursion anyway, because the alternative to a bound on a request path is a stack that a
/// disagreeing store could run off; refusing a chain longer than this withholds artifacts, which is
/// the direction a backstop must fail in.
const DEPENDENCY_CHAIN_MAX: u32 = 16;

/// One request's state, as the dependency prerequisite needs it.
///
/// Gathered once per response rather than per artifact: every field is a property of the request —
/// the viewer, the generation, the view and the composed mask — and none of them is a property of
/// the artifact being tested.
/// What [`Engine::gated_artifact`] answers: the artifact, located, with its level's row form and
/// the verdict's two outputs.
struct GatedArtifact {
    name: String,
    level: u32,
    ordinal: u32,
    entity: tessera_types::EntityId,
    layer: tessera_types::layer::RegisteredLayer,
    rows: Arc<crate::artifacts::ArtifactRows>,
    masked_count: u64,
    rank: Option<u32>,
}

struct DependencyContext<'a> {
    generation: &'a crate::Generation,
    satisfied: &'a rustc_hash::FxHashSet<tessera_types::TermId>,
    view: &'a str,
    view_data: &'a tessera_store::ViewData,
    mask: &'a crate::compose::EffectiveMask,
    /// This view's `deleted ∪ suppressed` in row space — the containment partition's acceptance
    /// test, carried here for the same reason `mask` is: a dependency's verdict is the *same*
    /// verdict, so it must be reached with the same inputs.
    denied: &'a croaring::Bitmap,
    reachable: &'a tessera_lifecycle::ResolvedLayers,
    /// What this request's composed mask *is* — the masked-count cache's key, carried here for
    /// `mask`'s reason: a dependency's verdict is the same verdict, and on a row-major target it
    /// reads the same histogram.
    mask_identity: crate::histogram::MaskIdentity,
}

const PER_TILE_CROSSING_RATIO: u64 = 3;

/// Split a chunk of the crossing domain no smaller than this, so a viewport small enough that the
/// fan-out costs more than the walk does not pay for one. 4,096 rows is ~0.1 ms of crossing work at
/// the scattered constant — comfortably above rayon's own per-task cost, and small enough that a
/// realistic viewport still splits hundreds of ways.
const CROSSING_CHUNK_MIN_ROWS: u32 = 4096;

/// The view-space rows a request's tiles span: every tile part shifted into view row space by its
/// segment's `row_base`, sorted, and merged.
///
/// **Merged, and that is not tidiness.** Adjacent tiles are adjacent Morton ranges, so merging
/// turns a few hundred separate walks into a handful of long contiguous ones — which is what makes
/// the per-tile crossing's reads of `row-entity.u32` sequential, and what lets
/// [`FilterRows::covers`] answer with one binary search. Merging `[a, b)` with `[b, c)` yields
/// exactly their union, so the domain is never widened by it.
fn crossing_domain(ranges: &[Vec<(usize, Range<u32>)>], row_bases: &[u32]) -> Vec<Range<u32>> {
    let mut spans: Vec<Range<u32>> = ranges
        .iter()
        .flat_map(|parts| parts.iter())
        .map(|(s, r)| row_bases[*s] + r.start..row_bases[*s] + r.end)
        .collect();
    spans.sort_unstable_by_key(|r| r.start);
    let mut merged: Vec<Range<u32>> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// **The vocabulary a predicate column's values are named by**, or `None` where the column has
/// none — in which case an artifact's key is the value's own canonical decimal spelling
/// (`tessera_types::layer::attribute_value_key`).
///
/// `None` also for a layer whose membership is not an attribute predicate at all, which is what
/// makes the closure built from this total: it answers *no code* for every key of such a layer, and
/// no such layer is ever asked.
fn predicate_vocabulary<'a>(
    generation: &'a crate::Generation,
    declaration: &tessera_types::layer::LayerDeclaration,
) -> Option<&'a tessera_store::vocabulary::VocabularyMinter> {
    let tessera_types::layer::MembershipSource::Attribute(field) = &declaration.membership else {
        return None;
    };
    let name = generation
        .bundle
        .manifest
        .declared_scalars
        .iter()
        .find(|scalar| &scalar.name == field)?
        .vocabulary
        .as_deref()?;
    generation.vocabularies.get(name)
}

/// **Where a predicate layer's membership comes from, for one request against one generation.**
///
/// `None` for an enumerated layer, and for a predicate layer whose rule cannot be evaluated at all
/// — a column this generation does not hold, or a spatial layer that declares no shape. Both are
/// the fail-closed answer: such a level is served with no membership, so none of its artifacts is a
/// candidate anywhere, rather than every artifact being one.
///
/// A spatial level's source is its held structures (`crate::shapes`), taken at the store's current
/// level version — built at open and at every publication into the level, so a request finds them
/// held; the join over the segments is the request's own O(containers) step.
#[allow(clippy::too_many_arguments)]
fn predicate_source<'a>(
    declaration: &tessera_types::layer::LayerDeclaration,
    generation: &'a crate::Generation,
    view: &str,
    view_data: &tessera_store::read::ViewData,
    segments: &'a [(&'a tessera_store::read::SegmentData, u32)],
    code_of_key: &'a dyn Fn(&str) -> Option<u32>,
    shapes: &crate::shapes::ShapeStore,
    store: &tessera_lifecycle::membership::ArtifactStore,
    level: u32,
) -> Option<crate::artifacts::PredicateSource<'a>> {
    match &declaration.membership {
        tessera_types::layer::MembershipSource::Enumerated => None,
        tessera_types::layer::MembershipSource::Attribute(field) => {
            let values = generation.filter_columns.value_layers(field)?;
            Some(crate::artifacts::PredicateSource::Attribute(
                crate::artifacts::AttributeSource {
                    values,
                    code_of_key,
                },
            ))
        }
        // ⊘ A spatial layer with no `shape` holds no artifacts and has nothing to resolve — the
        // state this surface has always had, and the one the generator's boundary fixture is in.
        tessera_types::layer::MembershipSource::Spatial => {
            declaration.shape?;
            // On the request path only where a publication route missed the level; nothing
            // persisted is claimable here, and the fallback is loud (`crate::shapes`).
            let held = shapes.level(
                view,
                &declaration.name,
                level,
                store,
                &crate::shapes::PersistedPieces::none(),
            );
            Some(crate::artifacts::PredicateSource::Spatial(
                crate::artifacts::SpatialSource {
                    level: held,
                    segments,
                    total_rows: u32::try_from(view_data.row_space.total_rows()).unwrap_or(u32::MAX),
                },
            ))
        }
    }
}

impl Engine {
    /// What this request's composed mask is, for the masked-count cache's key.
    ///
    /// **Taken from the geometry that actually resolved**, never from the live generation's idea of
    /// it: a session may be served a one-generation-stale projection (decision 0044), so the
    /// fragment a request composes against is the entry's and not the newest one there is. A key
    /// naming the wrong fragment would file one visible set's counts under another's.
    fn mask_identity(
        &self,
        session: &Session,
        generation: &crate::Generation,
        geometry: &crate::cache::SessionGeometry,
    ) -> crate::histogram::MaskIdentity {
        crate::histogram::MaskIdentity {
            token_id: session.token_id,
            segments_version: generation.segments_version,
            overlay_version: generation.overlay_version,
            fragment_identity: geometry.fragment.identity,
            fragment_watermark: geometry.fragment.watermark,
        }
    }

    /// This level's masked counts, where the level is served row-major and so has no other route to
    /// them.
    ///
    /// **`None` on an artifact-major level, and that is not a fallback**: such a level counts one
    /// artifact at a time against the composed mask, which a request's budget bounds.
    ///
    /// **Built lazily, on the first request that needs it** — a whole walk of the mask, which is the
    /// 0.85–1.7 s at 10⁷ artifacts decision 0093 prices. A cold drill-down on a row-major level
    /// therefore pays the level's whole histogram to answer about one artifact, which is stated here
    /// rather than discovered: the column has no per-artifact route to a masked count, so the choice
    /// is between this and re-scanning the mask for every drill-down.
    #[allow(clippy::too_many_arguments)]
    fn masked_counts(
        &self,
        identity: &crate::histogram::MaskIdentity,
        view: &str,
        layer: &str,
        level: u32,
        level_version: u64,
        rows: &crate::artifacts::ArtifactRows,
        mask: &crate::compose::EffectiveMask,
    ) -> Option<Arc<crate::histogram::MaskedCounts>> {
        let column = rows.column()?;
        Some(
            self.masked_counts
                .get_or_build(identity.key(view, layer, level, level_version), || {
                    crate::histogram::MaskedCounts::new(column.histogram(mask))
                }),
        )
    }

    /// **One artifact, located and gated for one principal** — the predicate
    /// [`Engine::artifact`] answers by, shared with the region leaf by artifact
    /// (`polygon-membership.md` §8) so that a shape a viewer may filter through is exactly a shape
    /// they would be served, by the same call.
    ///
    /// **`None` is the only failure shape.** An identifier naming nothing, one naming a point, one
    /// whose layer this principal does not reach or which is suppressed, one on another view, and
    /// one below its layer's existence criterion are one answer — C17's posture, and what keeps
    /// the leaf by artifact from being an oracle over shapes a viewer was not served.
    #[allow(clippy::too_many_arguments)]
    fn gated_artifact(
        &self,
        session: &Session,
        generation: &crate::Generation,
        view: &str,
        view_data: &tessera_store::read::ViewData,
        segments: &[(&SegmentData, u32)],
        mask: &EffectiveMask,
        denied: &croaring::Bitmap,
        mask_identity: crate::histogram::MaskIdentity,
        id: TesseraId,
    ) -> Result<Option<GatedArtifact>> {
        let (shard, entity) = self.identity_key.invert(id);
        if shard != generation.bundle.manifest.identity.shard_id {
            return Ok(None);
        }

        // Addressing, before authorisation and cheaply: which artifact, if any, this entity is.
        let Some((name, level, ordinal)) = self.write.locate_artifact(entity) else {
            return Ok(None);
        };
        let Some(layer) = self.write.registered_layer(&name) else {
            return Ok(None);
        };
        if !layer.declaration.views.iter().any(|s| s == view) {
            return Ok(None);
        }
        // Reachability, then the live suppression of the layer itself — the same two steps in the
        // same order `Engine::visible_layers` and `serve_artifacts` take.
        let reachable = self.write.resolve_layers(
            |term| session.satisfied.contains(&term),
            |label| generation.dict.lookup(label.as_bytes()),
        );
        if !reachable.contains(&name)
            || generation.overlay.is_deleted(layer.entity)
            || generation.overlay.is_suppressed(layer.entity)
        {
            return Ok(None);
        }

        let source = generation.partition_source();
        let recorded = layer.layout_of(level);
        // The predicate's own inputs, resolved once for this identifier — the same rule the
        // viewport resolves per layer, from the same generation, so an artifact reached by
        // identifier and one reached by viewport cannot be evaluated against different memberships.
        let vocabulary = predicate_vocabulary(generation, &layer.declaration);
        let code_of_key = |key: &str| match vocabulary {
            Some(vocabulary) => vocabulary.code_of(key),
            None => key.parse::<u32>().ok(),
        };
        let (rows, level_version) = self.write.with_artifacts(|store| {
            let predicate = predicate_source(
                &layer.declaration,
                generation,
                view,
                view_data,
                segments,
                &code_of_key,
                &self.shapes,
                store,
                level,
            );
            (
                self.artifact_projections.get_or_build(
                    &generation.prefix,
                    view,
                    &name,
                    level,
                    store,
                    &view_data.row_space,
                    Some(&source),
                    recorded,
                    predicate.as_ref(),
                    generation.segments_version,
                ),
                store.level_version(&name, level),
            )
        });
        // ⊘ **A cold drill-down on a row-major level pays the level's whole histogram**, because
        // the column has no per-artifact route to a masked count — see `Engine::masked_counts`.
        let counts = self.masked_counts(
            &mask_identity,
            view,
            &name,
            level,
            level_version,
            &rows,
            mask,
        );
        // The same containment answers the viewport builds, from the same partition: an identifier
        // route that resolved containment by a different arm would be a second ranking nobody
        // wrote. Lazily, because this route resolves one identifier — see `answer_for_one`.
        let containment = rows
            .partition()
            .map(|p| p.answer_for_one(&session.satisfied));
        let ctx = DependencyContext {
            generation,
            satisfied: &session.satisfied,
            view,
            view_data,
            mask,
            denied,
            reachable: &reachable,
            mask_identity,
        };
        let dependency_served = self.dependency_gate(&ctx);
        let artifact_view = crate::artifacts::ArtifactView {
            declaration: &layer.declaration,
            overlay: &generation.overlay,
            satisfied: &session.satisfied,
            layer_reachable: true,
            rows: &rows,
            mask,
            dependency_served: &dependency_served,
            containment,
            denied,
            counts,
        };
        // ⊘ Per-artifact terms arrive with content (Stage 3); until then a layer whose
        // `artifact_visibility` names a field withholds here as it does on the viewport, which is
        // the same fail-closed answer reached by the same call.
        let crate::artifacts::ArtifactVerdict::Serve { masked_count, rank } =
            artifact_view.verdict(entity, ordinal, None)
        else {
            return Ok(None);
        };
        Ok(Some(GatedArtifact {
            name,
            level,
            ordinal,
            entity,
            layer,
            rows,
            masked_count,
            rank,
        }))
    }

    /// Answer one region leaf for one request (`crate::region`; `crate::filter::RegionResolver`).
    ///
    /// A drawn shape: its decomposition from the generation-keyed cache — shared across
    /// principals, it carries no authorisation — with the boundary rows tested under **this
    /// request's composed mask**. A published shape: the artifact's held membership, whole and
    /// exact, only where this principal would be served the artifact; otherwise the empty
    /// operand, identically for every reason (`polygon-membership.md` §8). An artifact whose
    /// layer draws an authored shape is an empty operand too — its drawing is content, not a
    /// membership (§4.1).
    #[allow(clippy::too_many_arguments)]
    fn resolve_region(
        &self,
        leaf: &crate::filter::RegionLeaf,
        session: &Session,
        generation: &crate::Generation,
        view: &str,
        view_data: &tessera_store::read::ViewData,
        segments: &[(&SegmentData, u32)],
        mask: &EffectiveMask,
        denied: &croaring::Bitmap,
        mask_identity: crate::histogram::MaskIdentity,
        cancel: &Option<CancelToken>,
    ) -> std::result::Result<crate::region::RegionRows, crate::filter::FilterError> {
        use crate::filter::{FilterError, RegionLeaf};
        let never_cancelled = CancelToken::new();
        let cancel = cancel.as_ref().unwrap_or(&never_cancelled);
        use crate::region::{digest_of, RegionDecomposition, RegionKey, RegionRows, RegionVerdict};
        match leaf {
            RegionLeaf::Shape(shape) => {
                let max_cells = self.max_region_cells.load(Ordering::Relaxed) as usize;
                let canonical = shape.encode();
                let key = RegionKey {
                    view: view.to_string(),
                    prefix: generation.prefix.clone(),
                    segments_version: generation.segments_version,
                    digest: digest_of(&canonical),
                    max_cells,
                };
                let build = || RegionDecomposition::build(Arc::clone(shape), max_cells, segments);
                let entry = match self
                    .region_cache
                    .get_or_derive_waiting(key, None, cancel, |_| build())
                {
                    // A hit is a hit only for these bytes: a digest collision is detected here
                    // and answered from a fresh, unretained decomposition (selection-operand §5).
                    Ok(entry) if entry.is_of(&canonical) => entry,
                    Ok(_) => Arc::new(build()),
                    Err(crate::single_flight::WaitEnded::Cancelled) => {
                        return Err(FilterError::RegionUnavailable(
                            "the request was cancelled while its region was being decomposed"
                                .to_string(),
                        ))
                    }
                    // A wait that ran out is answered by building here, unretained: the
                    // decomposition is a perimeter's worth of work, and refusing it would make a
                    // second viewer's identical lasso a 429.
                    Err(crate::single_flight::WaitEnded::Budget) => Arc::new(build()),
                };
                Ok(RegionRows {
                    rows: entry.rows_under(mask, segments),
                    verdict: entry.verdict(),
                })
            }
            RegionLeaf::Artifact(id) => {
                let gated = self
                    .gated_artifact(
                        session,
                        generation,
                        view,
                        view_data,
                        segments,
                        mask,
                        denied,
                        mask_identity,
                        *id,
                    )
                    .map_err(|e| FilterError::RegionUnavailable(e.to_string()))?;
                let rows = match gated {
                    Some(gated)
                        if gated.layer.declaration.drawn_shape()
                            != Some(tessera_types::layer::DrawnShape::Authored) =>
                    {
                        gated.rows.get(gated.ordinal).cloned().unwrap_or_default()
                    }
                    _ => croaring::Bitmap::new(),
                };
                Ok(RegionRows {
                    rows,
                    verdict: RegionVerdict::Exact,
                })
            }
        }
    }

    /// Drill down on one artifact by the identifier a response handed out.
    ///
    /// **The same predicate the viewport calls, and that is the whole design of this method.** An
    /// artifact reachable by identifier but not by viewport — or the reverse — is two
    /// transcriptions of one rule, which is the failure mode this codebase has written down more
    /// than once. So this resolves the address, resolves the layer, and then calls
    /// [`crate::artifacts::ArtifactView::verdict`], exactly as `serve_artifacts` does. The only
    /// difference is that there is no tile candidacy: the caller named the artifact.
    ///
    /// **`None` is the only failure shape.** An identifier naming nothing, one naming a point
    /// rather than an artifact, one whose layer this principal does not reach, one whose artifact
    /// is suppressed, and one below its layer's existence criterion are one answer. That last route
    /// reads as new and is not — Appendix C's C17 annotation: the criterion tests the **masked**
    /// count, so it can only cross the bar when this principal's own visible membership changes.
    ///
    /// **On the cost channel.** In the steady state every route here is cheap and comparable: the
    /// session's geometry is resolved from the per-session cache a viewport already filled, and the
    /// membership's row form from the per-deployment cache. The one expensive path — building a
    /// projection — is deployment-wide state keyed on what was published, not on who is asking, so
    /// its timing carries nothing about a principal.
    ///
    /// `zoom` is the depth the caller draws at, for the vertex rule a predicate or an authored
    /// shape is served under (`polygon-membership.md` §7.2); `None` serves the whole presimplified
    /// shape under the budget alone. The derived kind — the hull — is unaffected by it.
    pub fn artifact(
        &self,
        session: &Session,
        id: TesseraId,
        idset: Option<u32>,
        view: &str,
        zoom: Option<u8>,
    ) -> Result<Option<ArtifactOut>> {
        let generation = self.generation.load_full();
        if let Some(e) = idset {
            if e != generation.bundle.manifest.identity.idset {
                return Err(EngineError::StaleIdSet);
            }
        }
        let carriers = generation
            .bundle
            .partitions
            .values()
            .filter(|partition| partition.views.contains_key(view))
            .count();
        if carriers > 1 {
            return Err(EngineError::MultiPartitionView(view.to_string()));
        }
        let view_data = generation
            .bundle
            .partitions
            .values()
            .find_map(|partition| partition.views.get(view))
            .ok_or_else(|| EngineError::UnknownView(view.to_string()))?;

        let mut probe = Probe::new();
        let geometry =
            self.session_geometry(session, &generation, view, view_data, &None, &mut probe)?;
        let denied = generation
            .denied
            .get(view)
            .ok_or_else(|| EngineError::DenyMaskMissing {
                view: view.to_string(),
            })?;
        let mask = compose(
            &session.satisfied,
            &generation.overlay,
            &generation.buffer,
            Arc::clone(&geometry.projection),
            &view_data.row_space,
            denied,
        );
        let segments = segments_with_row_bases(view, view_data)?;
        let mask_identity = self.mask_identity(session, &generation, &geometry);
        // The one predicate, shared with the viewport and with the region leaf by artifact.
        let Some(gated) = self.gated_artifact(
            session,
            &generation,
            view,
            view_data,
            &segments,
            &mask,
            denied,
            mask_identity,
            id,
        )?
        else {
            return Ok(None);
        };
        let GatedArtifact {
            name,
            level,
            ordinal,
            entity,
            layer,
            rows,
            masked_count,
            rank,
        } = gated;
        // Same resolution as the viewport's, by the same call — an identifier route that served a
        // different content would be a second ranking nobody wrote.
        let Some(content) = self.supplied_content(
            &generation,
            &name,
            level,
            ordinal,
            entity,
            layer.declaration.content.supplied.len(),
            rank,
            true,
            // One artifact, so the direct read: the level's table would answer this in O(1) and
            // cost a pass over the level to build, which is the wrong trade for a route that
            // resolves one identifier.
            None,
        ) else {
            return Ok(None);
        };
        // The same computation the viewport does, from the same composed mask — one route's
        // geometry differing from the other's would be two transcriptions of one rule, which is
        // exactly what the shared predicate above exists to prevent.
        let declared_derived: Vec<crate::derived::ComputedProperty> = layer
            .declaration
            .content
            .computed
            .iter()
            .filter_map(|name| crate::derived::ComputedProperty::parse(name))
            .collect();
        //
        // **The same per-principal cache the viewport reads** (`crate::derived_cache`), and this is
        // the route that most needs it: the client asks the viewport for centroids and this for the
        // one shape it draws (`artifact-shapes.md` §9), so a viewer moving the pointer back over a
        // cluster they have already hovered pays nothing.
        let derived = if declared_derived.is_empty() {
            crate::derived::DerivedContent::default()
        } else {
            let key = crate::derived_cache::DerivedKey {
                token_id: mask_identity.token_id,
                view: view.to_string(),
                layer: name.clone(),
                level,
                ordinal,
                level_version: self
                    .write
                    .with_artifacts(|store| store.level_version(&name, level)),
                segments_version: mask_identity.segments_version,
                overlay_version: mask_identity.overlay_version,
                fragment_identity: mask_identity.fragment_identity,
                fragment_watermark: mask_identity.fragment_watermark,
                properties: crate::derived_cache::properties_bits(&declared_derived),
            };
            let content = self.derived_geometry.get_or_derive(key, || {
                let Ok(segments) = segments_with_row_bases(view, view_data) else {
                    // Unreachable in practice — the view resolved above — and an empty content is
                    // the fail-closed reading of a row space that cannot be assembled.
                    return crate::derived::DerivedContent::default();
                };
                let locator = crate::derived::RowLocator::new(segments);
                let visible = rows
                    .get(ordinal)
                    .map(|members| mask.visible_rows(members))
                    .unwrap_or_default();
                crate::derived::compute(&declared_derived, &visible, &locator)
            });
            (*content).clone()
        };
        // The one drawn geometry of the other two kinds (`polygon-membership.md` §7.1): this
        // route is asked for the one shape a client draws, so it always answers.
        let mut content = content;
        let mut derived = derived;
        let shape_guard_fired = self.drawn_shape(
            &layer.declaration,
            view,
            &name,
            level,
            ordinal,
            &mut content,
            &mut derived,
            zoom,
        );
        Ok(Some(ArtifactOut {
            content,
            layer: name.clone(),
            tessera_id: id,
            key: self.write.with_artifacts(|store| {
                store.get(&name, level, ordinal).and_then(|r| r.key.clone())
            }),
            masked_count,
            derived,
            // The declared level — which is the rung on every layer kind *for this route*: a
            // treed layer's stored level is 0, and its response-local chain depth is also 0 here,
            // this response being one artifact with no parent links to be deep in.
            rung: level,
            // **Always null on this route, and not by omission.** A parent is named only where it
            // is also in the response, and this response is one artifact — so there is nothing for
            // it to name. Resolving the parent here anyway would hand a caller who holds one
            // identifier the existence of a coarser artifact they were never served.
            parent_id: None,
            // The identifier route carries no filter to answer about (decision 0104), and there is
            // no viewport for the answer to be scoped to either.
            matched: None,
            shape_guard_fired,
        }))
    }

    /// **The predicate and the authored kind of an artifact's one drawn geometry**
    /// (`polygon-membership.md` §7.1), filled into `derived.shape` beside the count — the derived
    /// kind, the hull, is already there from [`crate::derived::compute`]. Returns whether the
    /// vertex budget fired.
    ///
    /// A **predicate** shape is the level's held canonical shape at this ordinal
    /// (`crate::shapes`), served at the request's depth (`crate::shapes::served_rings`) — the
    /// same bytes for every principal, which is what `/v1/meta`'s kind tells a client. It is
    /// served under the artifact's own verdict and nothing else: this is reached only for an
    /// artifact that verdict admitted.
    ///
    /// An **authored** shape is the supplied content at the layer's shape slot, which
    /// `supplied_content` already gated by that content's own `require_member_visibility`: the
    /// canonical per-view bytes are read back out of the slot, the request's view's shape is
    /// served at the same rule, and **the slot is blanked** — the wire's `content` carries the
    /// layer's texts, and the geometry travels as rings in `shape_x`/`shape_y`. A slot that does
    /// not read as a shape draws nothing rather than a guess.
    ///
    /// **Never on a request that did not ask**: the caller passes `derived` only where the
    /// request's `computed` selected the shape, and passes the content list only where it was
    /// materialised.
    #[allow(clippy::too_many_arguments)]
    fn drawn_shape(
        &self,
        declaration: &tessera_types::layer::LayerDeclaration,
        view: &str,
        layer: &str,
        level: u32,
        ordinal: u32,
        content: &mut [String],
        derived: &mut crate::derived::DerivedContent,
        zoom: Option<u8>,
    ) -> bool {
        match declaration.drawn_shape() {
            None | Some(crate::shapes::DrawnShape::Derived) => false,
            Some(crate::shapes::DrawnShape::Predicate) => {
                let held = match self.shapes.get(view, layer, level) {
                    Some(held) => held,
                    // Not yet held for this view — a publication route this module was not
                    // wired into; the fallback is the loud one every other reader takes.
                    None => self.write.with_artifacts(|store| {
                        self.shapes.level(
                            view,
                            layer,
                            level,
                            store,
                            &crate::shapes::PersistedPieces::none(),
                        )
                    }),
                };
                let Some(shape) = held.shapes.get(ordinal as usize).and_then(|s| s.as_ref())
                else {
                    return false;
                };
                let (parts, guarded) = crate::shapes::served_rings(&shape.shape, zoom);
                derived.shape = Some(parts);
                guarded
            }
            Some(crate::shapes::DrawnShape::Authored) => {
                let Some((slot, _)) = declaration.authored_shape() else {
                    return false;
                };
                let Some(text) = content.get_mut(slot) else {
                    return false;
                };
                let shapes = tessera_lifecycle::membership::ArtifactShapes::from_content_text(text);
                text.clear();
                let Some(shape) = shapes
                    .as_ref()
                    .and_then(|s| s.for_view(view))
                    .and_then(|bytes| tessera_spatial::shape::Shape::decode(bytes).ok())
                else {
                    return false;
                };
                let (parts, guarded) = crate::shapes::served_rings(&shape, zoom);
                derived.shape = Some(parts);
                guarded
            }
        }
    }

    /// The artifacts of this viewport: every one the request asked for, that this principal
    /// reaches, that has a visible member inside the requested tiles, and that passes the one
    /// predicate.
    ///
    /// **Four narrowings, in that order, and the order is the disclosure control.** Reachability
    /// first, because it costs one set probe and a name the principal cannot reach must not have
    /// its membership touched at all. Candidacy second, because it is the cheap masked question and
    /// it keeps the count off every artifact outside the viewport. The predicate last, because it
    /// is the one that decides, and it is [`crate::artifacts::ArtifactView::verdict`] — the same
    /// function drill-down and every later route calls.
    ///
    /// **The count is over the whole membership, not over the tiles.** A viewer is told how many of
    /// a cluster's documents they can see, which does not change as they pan; a per-viewport count
    /// would move with the box and let a viewer difference two boxes for the members in between.
    /// Candidacy is the only per-tile question here.
    #[allow(clippy::too_many_arguments)]
    /// The values of the content the predicate chose, or `None` where it chose one whose content
    /// cannot be read back.
    ///
    /// **`rank` is the index into the artifact's ranked `contents`** — not a Morton rank and not a
    /// rank within a bitmap, both of which this module uses the word for elsewhere.
    ///
    /// `Some(vec![])` and `None` are different answers and the difference is the whole point:
    /// the first is *this layer declares no supplied content*, which is most layers; the second is
    /// *this artifact should carry content and it is not here*, which withholds the artifact.
    ///
    /// **`materialise = false` runs the same servability test and copies nothing** — the identity
    /// projection's setting (`artifact-fetch-protocol.md` §5.2). `Some`/`None` is decided by
    /// identical checks on either setting, because that answer withholds the artifact and a
    /// projection must not move the row set; all `false` skips is the string copies, and its
    /// `Some` always carries the empty vector. One function with a flag rather than a probing
    /// sibling, so the two readings of "servable" cannot drift apart.
    ///
    /// **`table` is the level's contents, read once for the level** — the viewport pass supplies
    /// it, and it is what keeps a response of thousands of artifacts off a zstd block read per
    /// artifact (`crate::artifact_content`, which carries the measurement). `None` reads the one
    /// entity's row directly: the drill-down route asks about one artifact, and building a whole
    /// level's table to answer that would trade a block read for a pass over the level.
    #[allow(clippy::too_many_arguments)]
    fn supplied_content(
        &self,
        generation: &crate::Generation,
        layer: &str,
        level: u32,
        ordinal: u32,
        entity: EntityId,
        kinds: usize,
        rank: Option<u32>,
        materialise: bool,
        table: Option<&crate::artifact_content::LevelContent>,
    ) -> Option<Vec<String>> {
        let Some(rank) = rank else {
            return Some(Vec::new());
        };
        // The publication's own copy, while it is still in memory — the log is the only home the
        // content has between the publish and the manifest that carries it.
        let held = self.write.with_artifacts(|store| {
            store
                .get(layer, level, ordinal)
                .and_then(|record| record.contents.get(rank as usize))
                .and_then(|set| match (&set.values, materialise) {
                    (Some(values), true) => Some(values.clone()),
                    (Some(_), false) => Some(Vec::new()),
                    (None, _) => None,
                })
        });
        if let Some(values) = held {
            return Some(values);
        }

        // Otherwise the record blob, at this artifact's own entity. Tags are `rank × kinds + kind`
        // against the layer's declaration — see `ArtifactStore::unpublished_content`.
        if kinds == 0 {
            return Some(Vec::new());
        }
        let base = (rank as usize).checked_mul(kinds)?;
        let entity = u32::try_from(entity.raw()).ok()?;
        /// **Every declared kind or none.** A row missing one is content that did not survive its
        /// write, and serving the rest would hand a client an artifact short of what its layer
        /// says it carries — which is indistinguishable, from the client's side, from content
        /// withheld. Stated once, for both routes below.
        fn values_for<'a>(
            base: usize,
            kinds: usize,
            materialise: bool,
            text_at: impl Fn(u16) -> Option<&'a str>,
        ) -> Option<Vec<String>> {
            let mut values = Vec::with_capacity(if materialise { kinds } else { 0 });
            for k in 0..kinds {
                let text = text_at(u16::try_from(base + k).ok()?)?;
                if materialise {
                    values.push(text.to_string());
                }
            }
            Some(values)
        }
        // **The two routes decide identically**, which is the whole reason the tag walk above is
        // one loop over a lookup rather than two loops: the table holds the row's utf8 fields, and
        // a tag it does not hold is a tag the row did not carry *or* one whose value was not text
        // — both of which withhold on the direct route too.
        match table {
            Some(table) => {
                let tagged = table.tagged(entity)?;
                values_for(base, kinds, materialise, |tag| {
                    tagged
                        .binary_search_by_key(&tag, |(t, _)| *t)
                        .ok()
                        .map(|at| tagged[at].1.as_str())
                })
            }
            None => {
                let fields = generation.filter_columns.records().fields_of(entity).ok()??;
                values_for(base, kinds, materialise, |tag| {
                    fields.iter().find(|f| f.tag == tag).and_then(|f| {
                        match &f.value {
                            tessera_filter::RecordValue::Utf8(text) => Some(text.as_str()),
                            _ => None,
                        }
                    })
                })
            }
        }
    }

    /// The dependency prerequisite, shared by both serving routes: **is the artifact this one
    /// attaches to served to this viewer?**
    ///
    /// One function rather than two call sites doing the same steps, on the argument the shared
    /// predicate itself rests on: a route that gated dependencies differently from the other would
    /// be two transcriptions of one rule, and the one that drifted would be serving labels for
    /// clusters their viewer cannot see.
    ///
    /// **The target's own `verdict`, not a cheaper summary of it**
    /// ([decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md),
    /// rule 2). Its layer's gate, its live suppression, its existence, its own terms, its existence
    /// criterion against *this viewer's* masked count, and its containment all decide here, because
    /// "visible" means the same thing for a dependency as it does for anything else. The
    /// conjunction can only narrow, so the term introduces no disclosure of its own.
    ///
    /// **The order matters and is the order the served layer's own path takes**: reachability
    /// first, then the layer's live disposition, then the level and the slot, then the predicate.
    /// A reachability resolved once per session may be cached; a disposition may not, and asking
    /// them in this order is what keeps a layer suppression from being outlived by a session.
    ///
    /// **Recursion, bounded by the declaration graph.** A dependency may itself be a dependent — a
    /// label on a label — and the chain terminates because a layer is registered only after every
    /// layer it names in `depends_on`, which makes the graph acyclic by construction. `depth` is a
    /// backstop for a store that somehow disagrees with that, and it fails closed rather than
    /// deep: a chain longer than any real declaration is refused, not followed.
    fn dependency_served(
        &self,
        ctx: &DependencyContext<'_>,
        attachment: &tessera_lifecycle::membership::Attachment,
        depth: u32,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        // A name this principal does not reach, and a layer dropped since the resolution, are one
        // answer here for the reason they are one answer everywhere: which of them applies is
        // exactly the fact being withheld.
        if !ctx.reachable.contains(&attachment.layer) {
            return false;
        }
        let Some(layer) = self.write.registered_layer(&attachment.layer) else {
            return false;
        };
        if ctx.generation.overlay.is_deleted(layer.entity)
            || ctx.generation.overlay.is_suppressed(layer.entity)
        {
            return false;
        }
        // A layer that does not live in this view has no membership in this row space, so there is
        // nothing here that could be served.
        if !layer.declaration.views.iter().any(|s| s == ctx.view) {
            return false;
        }
        let record = self.write.with_artifacts(|store| {
            store
                .get(&attachment.layer, attachment.level, attachment.ordinal)
                .map(|record| record.entity)
        });
        // **The slot answers, and it must answer with the entity the edge names.** A hole is what
        // the fold leaves where it executed a deletion — in the same publication that retired the
        // overlay entry saying so — and an ordinal holding a *different* entity is an edge into an
        // artifact that is gone and has been republished over. Both are absent.
        let Some(entity) = record.filter(|entity| *entity == attachment.entity) else {
            return false;
        };
        let recorded = layer.layout_of(attachment.level);
        // The target's own membership, evaluated the same way its own serving route would — a
        // dependency answered from a different rule would be a second membership nobody wrote.
        let Ok(segments) = segments_with_row_bases(ctx.view, ctx.view_data) else {
            return false;
        };
        let vocabulary = predicate_vocabulary(ctx.generation, &layer.declaration);
        let code_of_key = |key: &str| match vocabulary {
            Some(vocabulary) => vocabulary.code_of(key),
            None => key.parse::<u32>().ok(),
        };
        let (rows, level_version) = self.write.with_artifacts(|store| {
            let predicate = predicate_source(
                &layer.declaration,
                ctx.generation,
                ctx.view,
                ctx.view_data,
                &segments,
                &code_of_key,
                &self.shapes,
                store,
                attachment.level,
            );
            (
                self.artifact_projections.get_or_build(
                    &ctx.generation.prefix,
                    ctx.view,
                    &attachment.layer,
                    attachment.level,
                    store,
                    &ctx.view_data.row_space,
                    Some(&ctx.generation.partition_source()),
                    recorded,
                    predicate.as_ref(),
                    ctx.generation.segments_version,
                ),
                store.level_version(&attachment.layer, attachment.level),
            )
        });
        // The target's own count, from whichever structure its layout puts it in — the same
        // histogram the viewport would read, under the same key, so a dependency answered here and
        // the target answered directly cannot disagree.
        let counts = self.masked_counts(
            &ctx.mask_identity,
            ctx.view,
            &attachment.layer,
            attachment.level,
            level_version,
            &rows,
            ctx.mask,
        );
        let nested = |a: &tessera_lifecycle::membership::Attachment| {
            self.dependency_served(ctx, a, depth - 1)
        };
        // **Lazily, and this one is load-bearing rather than tidy.** The prerequisite runs once per
        // attached candidate, so settling a level's whole expression table here would turn a
        // per-artifact question into whole-population work per artifact.
        let containment = rows.partition().map(|p| p.answer_for_one(ctx.satisfied));
        crate::artifacts::ArtifactView {
            declaration: &layer.declaration,
            overlay: &ctx.generation.overlay,
            satisfied: ctx.satisfied,
            layer_reachable: true,
            rows: &rows,
            mask: ctx.mask,
            dependency_served: &nested,
            containment,
            denied: ctx.denied,
            counts,
        }
        // ⊘ Per-artifact terms arrive with content, so the target's own label is `None` here
        // exactly as it is on the two serving routes — the same fail-closed answer reached by the
        // same call.
        .verdict(entity, attachment.ordinal, None)
        .is_served()
    }

    /// The prerequisite as the predicate takes it: a closure over one request's state.
    fn dependency_gate<'a>(
        &'a self,
        ctx: &'a DependencyContext<'a>,
    ) -> impl Fn(&tessera_lifecycle::membership::Attachment) -> bool + 'a {
        move |attachment| self.dependency_served(ctx, attachment, DEPENDENCY_CHAIN_MAX)
    }

    // Ten, and every one is a thing the artifact pass genuinely needs from the request it is part
    // of: the session, the generation, the view and its data, the resolved tile ranges, the
    // composed mask, and the request's own three artifact parameters. Bundling them into a struct
    // would name the same ten things one call earlier.
    #[allow(clippy::too_many_arguments)]
    fn serve_artifacts(
        &self,
        session: &Session,
        generation: &crate::Generation,
        view: &str,
        view_data: &tessera_store::ViewData,
        ranges: &[Vec<(usize, Range<u32>)>],
        mask: &crate::compose::EffectiveMask,
        requested: LayerSelection<'_>,
        artifact_budget: Option<u32>,
        levels: LevelSelection<'_>,
        // Which declared properties this request pays for — a narrowing of the declaration and
        // never a widening of it (`ComputedSelection`).
        computed: ComputedSelection<'_>,
        // The request's tile depth, which `LevelSelection::Declared` joins against each layer's
        // declared per-level zoom ranges. The two are the same 0–16 coordinate.
        zoom: u8,
        mask_identity: crate::histogram::MaskIdentity,
        artifact_rows: ArtifactRows,
        // D-C: checked once per artifact served. The derived sweep is the response's dominant
        // CPU and it runs between two flushes, so without a checkpoint here a client that has
        // gone — or a stream the server has shed — is discovered only when the whole frame is
        // ready to send: three abandoned GeoNames requests each held a worker for minutes
        // (2026-08-28), deriving geometry nobody would read.
        cancel: &Option<CancelToken>,
    ) -> Result<(Vec<ArtifactOut>, Vec<ServedLayer>)> {
        // Which layers this principal may know exist — one set probe for a gate-failed name and a
        // never-registered one alike (`LayerRegistry::resolve_for`).
        let reachable = self.write.resolve_layers(
            |term| session.satisfied.contains(&term),
            |label| generation.dict.lookup(label.as_bytes()),
        );
        // **Intersected with the request, never unioned.** A name the principal does not reach is
        // absent whether or not they asked for it, so asking is not a way to learn what exists.
        let names: Vec<String> = match requested {
            LayerSelection::Named(list) => list
                .iter()
                .filter(|name| reachable.contains(name))
                .map(|name| name.to_string())
                .collect(),
            LayerSelection::All => reachable.names().map(str::to_string).collect(),
        };
        if names.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        // Built once for the whole response, and from the *same* resolution the names above came
        // from: a label's target may live in any layer its own declares in `depends_on`, reachable
        // or not, and asking a second resolution would be a second answer to one question.
        // **Fail-closed on a missing entry**, exactly as the point path is: every view the bundle
        // carries has one, empty when nothing is denied (`compose::derive_denied`), so an absent
        // key means the mask and the bundle disagree about what this generation holds. Reading it
        // as *nothing is denied here* would let the containment partition serve content generated
        // from suppressed and deleted documents, with no error anywhere.
        let denied = generation
            .denied
            .get(view)
            .ok_or_else(|| EngineError::DenyMaskMissing {
                view: view.to_string(),
            })?;
        let ctx = DependencyContext {
            generation,
            satisfied: &session.satisfied,
            view,
            view_data,
            mask,
            denied,
            reachable: &reachable,
            mask_identity,
        };
        let dependency_served = self.dependency_gate(&ctx);

        // The viewport as one row-space set, built once for every layer: the merged global spans of
        // every tile this request resolved. `crossing_domain` already merges and globalises them
        // for the filter's crossing, and reusing it is what keeps the two from disagreeing about
        // which rows a request covers.
        let segments = segments_with_row_bases(view, view_data)?;
        let row_bases: Vec<u32> = segments.iter().map(|&(_, base)| base).collect();
        // The same list a shape's ranges are resolved against — built once for the response, and
        // deliberately the same one the viewport's own tiles resolve through, so a membership and a
        // viewport that overlap on the map overlap in row space.
        let segments_for_shapes = segments.clone();
        // Built once per request rather than per layer: it is the same view's segment list for
        // every artifact in the response, and a layer declaring no derived content never asks it
        // anything.
        let locator = crate::derived::RowLocator::new(segments);
        let mut tile_rows = croaring::Bitmap::new();
        for span in crossing_domain(ranges, &row_bases) {
            tile_rows.add_range(span);
        }
        if tile_rows.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        // **The one composition, hoisted out of every layer and every artifact**
        // (`design/artifact-serving-at-scale.md` §4 step 2, and `crate::tile_index::Viewport`).
        // Built once per request: it is the same set for every layer in the response, and its cost
        // is the viewport's containers rather than the population's.
        let viewport = crate::tile_index::Viewport::compose(&tile_rows, mask);
        // **The filter's half of the same hoisting, and it is composed once for the request too**:
        // `viewport ∩ M_auth ∩ M_sel`, the one set decision 0104's bit is asked against. `None` is
        // an unfiltered request — no question, and no column on the wire to answer it.
        let matched_here = mask.matched_rows(viewport.here());

        let shard = generation.bundle.manifest.identity.shard_id;
        // Built once for the whole response: the postings and the manifest's plugin are the
        // generation's, not the layer's, and the gate they carry is one decision per request.
        let source = generation.partition_source();

        // **The layers this response walks**, which is what makes the dependent drop below
        // decidable. A target missing from a response that never looked at its layer was not
        // removed from anything — see [`orphaned_dependents`].
        let in_request: std::collections::BTreeSet<String> = names.iter().cloned().collect();

        let mut out = Vec::new();
        // The layers whose rung is the response-local chain depth rather than the declared level —
        // the treed (nested) ones, decision 0082's edges-not-levels shape. Collected during the
        // walk, applied after the response's row set is final (below).
        let mut treed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        // Where each served artifact ended up, and what each points at — collected during the walk
        // and reconciled after it.
        let mut served_at: std::collections::BTreeMap<(String, u32, u32), TesseraId> =
            std::collections::BTreeMap::new();
        let mut placed: Vec<Placement> = Vec::new();
        // Every level this pass walked, with the structures the membership column reads the
        // served set back through — the same row form and the same lineage the verdicts and the
        // cut used, so the column cannot describe a level the artifacts frame did not.
        let mut served_layers: Vec<ServedLayer> = Vec::new();
        for name in names {
            let Some(layer) = self.write.registered_layer(&name) else {
                // Dropped between the resolution and here. Absent is the right answer and the same
                // one a gate failure gives.
                continue;
            };
            // A layer declares which views it lives in; one it did not declare has no membership
            // in this row space to project.
            if !layer.declaration.views.iter().any(|s| s == view) {
                continue;
            }
            // **The live half, asked per request.** A layer's own entity carries its suppression,
            // and a resolution may cache reachability but never the verdict — see
            // `Engine::visible_layers`, which takes the same two steps in the same order.
            if generation.overlay.is_deleted(layer.entity)
                || generation.overlay.is_suppressed(layer.entity)
            {
                continue;
            }
            if layer.declaration.hierarchy.kind == tessera_types::layer::HierarchyKind::Nested {
                treed.insert(name.clone());
            }

            // Parsed once per layer. A name outside the vocabulary cannot reach here — the
            // declaration was refused at registration — so an unparseable one is dropped rather
            // than erroring the whole response.
            //
            // **The request narrows it, and only ever narrows it.** The intersection is taken here
            // so that a property the request did not ask for is never computed at all — the point
            // of the field is the work it does not do, and filtering the *result* would keep the
            // hull's cost while dropping its bytes.
            let declared_derived: Vec<crate::derived::ComputedProperty> = layer
                .declaration
                .content
                .computed
                .iter()
                .filter_map(|name| crate::derived::ComputedProperty::parse(name))
                .filter(|property| computed.selects(*property))
                .collect();

            // **The predicate's inputs, resolved per level.** An attribute layer's every level
            // reads the same column; a spatial layer's held structures are per level, because
            // each level holds its own shapes. A layer with a stored membership resolves nothing.
            let vocabulary = predicate_vocabulary(generation, &layer.declaration);
            let code_of_key = |key: &str| match vocabulary {
                Some(vocabulary) => vocabulary.code_of(key),
                None => key.parse::<u32>().ok(),
            };

            let mut served_levels: Vec<ServedLevel> = Vec::new();
            for (level, runs) in layer.runs.iter().enumerate() {
                let level = level as u32;
                // **Skipped before the projection is built, not after it is served.** A level the
                // request did not ask for costs nothing at all here: no `get_or_build`, no
                // candidate walk, no masked probe and no derived geometry over its members. That is
                // the whole point of the field — a whole-layer response over a five-level
                // administrative hierarchy pays a pass over every member at every level, and the
                // levels a client was never going to draw dominate it.
                if !level_is_selected(levels, &layer.declaration.levels, level, zoom) {
                    continue;
                }
                let recorded = layer.layout_of(level);
                let (rows, level_version) = self.write.with_artifacts(|store| {
                    let predicate = predicate_source(
                        &layer.declaration,
                        generation,
                        view,
                        view_data,
                        &segments_for_shapes,
                        &code_of_key,
                        &self.shapes,
                        store,
                        level,
                    );
                    (
                        self.artifact_projections.get_or_build(
                            &generation.prefix,
                            view,
                            &name,
                            level,
                            store,
                            &view_data.row_space,
                            Some(&source),
                            recorded,
                            predicate.as_ref(),
                            generation.segments_version,
                        ),
                        store.level_version(&name, level),
                    )
                });
                // **The count's route, decided by the level's layout and by nothing about the
                // request.** An artifact-major level counts per served artifact; a row-major one has
                // no per-artifact membership to intersect and reads the histogram, which is built
                // once per session per generation and cached under a key that moves with every
                // accepted deny (`crate::histogram`).
                let counts = self.masked_counts(
                    &mask_identity,
                    view,
                    &name,
                    level,
                    level_version,
                    &rows,
                    mask,
                );
                let containment = rows.partition().map(|p| p.answers(&session.satisfied));
                // Captured before the shadow below: `view` becomes the artifact predicate's value,
                // and the derived-geometry key needs the view's *name*.
                let view_name = view;
                // **Built after the candidacy route and never as part of it**: the filter decides
                // nothing about which artifacts are served (decision 0104), so this is computed
                // beside the verdict rather than inside it, and is skipped whole on an unfiltered
                // request.
                let matched = matched_here.as_ref().map(|here| rows.matched(here));
                let view = crate::artifacts::ArtifactView {
                    declaration: &layer.declaration,
                    overlay: &generation.overlay,
                    satisfied: &session.satisfied,
                    layer_reachable: true,
                    rows: &rows,
                    mask,
                    dependency_served: &dependency_served,
                    containment,
                    denied,
                    counts,
                };
                // **Every candidate is tested before any is cut**, and the two passes are separate
                // for a reason that is not performance: the verdict is a per-artifact question
                // with no lineage input (decision 0080), and a loop that decided *and* pruned in
                // one step would have the shape that lets a node's neighbours reach its verdict.
                let mut passing = Vec::new();
                // **The walk replaces the sweep over every ordinal.** Cost is the viewport's
                // perimeter in the hierarchy rather than the level's population: an artifact in no
                // node the viewport touches has no member there, so it cannot have a *visible* one
                // and skipping it withholds nothing (`crate::tile_index`, and §4.1 on why this is a
                // candidate generator and never an answer). Holes and artifacts whose membership
                // projects to nothing are in no node either, so neither reaches the predicate here
                // — and both remain live on the identifier route, which walks no index.
                //
                // **Or the scan, where the level is served row-major**: one pass over
                // `viewport ∩ M_auth` marking labels, which answers the same question at a cost in
                // *points* rather than in artifacts (`ArtifactRows::candidacy`). Which route is
                // taken is a property of the level and never of the request.
                let candidates = rows.candidacy(&viewport);
                for ordinal in candidates.iter() {
                    // **Every candidate pays a masked probe**, on whichever of the three routes the
                    // classification makes cheapest — see `ArtifactRows::candidate_in`, which is
                    // the one place the choice is made and the one the differential drives.
                    if !rows.candidate_in(ordinal, &candidates, &viewport, mask) {
                        continue;
                    }
                    let Some(entity) = runs.entity_of(ordinal as u64).map(EntityId::new) else {
                        continue;
                    };
                    // ⊘ **No artifact carries its own terms yet**, so a layer whose
                    // `artifact_visibility` names a field serves nothing here — fail-closed, and
                    // visibly so. The per-artifact label arrives with content (Stage 3); until then
                    // the named field has nothing to satisfy, and admitting the artifact instead
                    // would make a missing declaration a grant to everyone.
                    let crate::artifacts::ArtifactVerdict::Serve { masked_count, rank } =
                        view.verdict(entity, ordinal, None)
                    else {
                        continue;
                    };
                    passing.push((ordinal, entity, masked_count, rank));
                }

                // The level's lineage, read from the parent pointers of **every** artifact and not
                // only the passing ones: an ancestor that failed its own criterion is still an
                // ancestor, and a cut blind to it would keep a node its descendant covers.
                //
                // **Within-level edges only, and that is the whole of the tiered shape's
                // treatment here** (owner ruling, 2026-08-18). A tiered layer's edges run
                // between levels and are *information* — what contains what, so a client can nest
                // what it draws or filter to one subtree — rather than a ladder to coarsen along.
                // Climbing them would substitute a state for its counties and draw one large
                // polygon across a region whose neighbours are still counties. So the cut does not
                // see them, such a layer's lineage is empty here, and its budget is inert exactly
                // as a flat layer's is.
                //
                // **Held per generation, not derived per request.** The pointers depend on neither
                // the mask nor the viewport, so a request that rebuilds them is doing generation
                // work: ~96 ms at a level of ten million, against the ~3 ms the cut over them now
                // costs.
                //
                // **The version and the build are taken inside one hold of the artifacts lock**,
                // which is what makes the cached lineage the lineage *of* the version it is filed
                // under: read separately, a write landing between the two would file the new
                // level's edges under the old level's version, and the next request would serve a
                // cut through a tree that has moved.
                let lineage = self.write.with_artifacts(|store| {
                    self.lineages.get_or_build(
                        &name,
                        level,
                        store.level_version(&name, level),
                        || {
                            crate::cut::Lineage::new(store.level(&name, level).map(
                                |(ordinal, record)| {
                                    let within = record
                                        .parent
                                        .filter(|parent| parent.level == level)
                                        .map(|parent| parent.ordinal);
                                    (ordinal, within)
                                },
                            ))
                        },
                    )
                });
                let ordinals: Vec<u32> = passing.iter().map(|&(o, ..)| o).collect();
                // Ascending and deduplicated, which the cut guarantees — so the membership test in
                // the emit loop below is a binary search rather than a scan of the served set once
                // per candidate.
                //
                // **`prune_children` is the layer's, and it is a rendering choice rather than a
                // disclosure one.** Pruned, a passing parent is dropped where a passing child sits
                // beneath it; unpruned, both are served and the client receives the whole visible
                // tree — which is what lets it nest what it draws, or filter to one subtree while
                // still drawing the rest. Every artifact in either set cleared its own criterion,
                // so neither is the safer answer.
                let served = crate::cut::cut(
                    &lineage,
                    &ordinals,
                    artifact_budget,
                    layer.declaration.hierarchy.prune_children,
                );
                // **The level's supplied content, read once for the level rather than once per
                // served artifact** (`crate::artifact_content`, which carries the measurement that
                // put it here: 408 ms of a response whose points half is 1.3 ms, all of it one
                // zstd block decompressed per artifact served — 6.7 ms once the level's contents
                // are read together). Built after the cut, so a level whose artifacts all failed
                // their verdict reads nothing at all, and skipped whole where the layer declares no
                // supplied content — which is most layers.
                let contents = if layer.declaration.content.supplied.is_empty() || served.is_empty()
                {
                    None
                } else {
                    Some(self.level_contents.get_or_build(
                        &name,
                        level,
                        level_version,
                        generation.segments_version,
                        || {
                            crate::artifact_content::LevelContent::build(
                                generation.filter_columns.records(),
                                runs,
                            )
                        },
                    ))
                };
                served_levels.push(ServedLevel {
                    level,
                    rows: Arc::clone(&rows),
                    lineage: Arc::clone(&lineage),
                    // Filled once the response's membership is settled, below.
                    served: std::collections::HashMap::new(),
                });

                for (ordinal, entity, masked_count, rank) in passing {
                    if served.binary_search(&ordinal).is_err() {
                        continue;
                    }
                    check_cancelled(cancel)?;
                    // The one content this viewer contains, entire. ⊘ A content restored from a
                    // packed extent carries no values yet (its content belongs in the record blob,
                    // decision 0077, and that write is unbuilt), and is **withheld** rather than
                    // served with its description missing.
                    //
                    // **Asked under the identity projection too, and deliberately** — with
                    // `materialise = false`, so the values are not copied but the *servability*
                    // test is identical. Content-cannot-be-served withholds the artifact, so
                    // skipping the probe here would let an identity response carry a row the full
                    // response withholds, breaking §5.2's row-set contract sentence.
                    let Some(content) = self.supplied_content(
                        generation,
                        &name,
                        level,
                        ordinal,
                        entity,
                        layer.declaration.content.supplied.len(),
                        rank,
                        artifact_rows == ArtifactRows::Full,
                        contents.as_deref(),
                    ) else {
                        continue;
                    };
                    // The blinding is total over the space the allocator issues, so this cannot
                    // fail for an entity that came out of the runs above; a failure would mean the
                    // manifest and the allocator disagree, and dropping the artifact is the
                    // fail-closed reading of that.
                    let Ok(tessera_id) = self.identity_key.forward(shard, entity) else {
                        continue;
                    };
                    // **From the composed mask, and only from it.** The visible rows are the
                    // artifact's membership intersected with what this principal may see, so every
                    // property below is a function of `membership ∩ M_auth` and nothing else
                    // (`annotations.md` §4.2). Skipped entirely where the layer declares nothing,
                    // which is what keeps a count-only layer at count-only cost — and skipped
                    // whole under the identity projection, which is that projection's point: the
                    // derived sweep is the response's dominant CPU and decides nothing about
                    // which rows are served (`artifact-fetch-protocol.md` §5.2).
                    //
                    // **Held per principal between requests** (`crate::derived_cache`): a pan
                    // re-serves mostly the same artifacts to the same viewer, and a shape is the
                    // most expensive thing this loop does. The key names the principal, so a hit
                    // answers the request that would have derived the same value.
                    let derived = if artifact_rows == ArtifactRows::Identity
                        || declared_derived.is_empty()
                    {
                        crate::derived::DerivedContent::default()
                    } else {
                        let key = crate::derived_cache::DerivedKey {
                            token_id: mask_identity.token_id,
                            view: view_name.to_string(),
                            layer: name.clone(),
                            level,
                            ordinal,
                            level_version,
                            segments_version: mask_identity.segments_version,
                            overlay_version: mask_identity.overlay_version,
                            fragment_identity: mask_identity.fragment_identity,
                            fragment_watermark: mask_identity.fragment_watermark,
                            properties: crate::derived_cache::properties_bits(&declared_derived),
                        };
                        (*self.derived_geometry.get_or_derive(key, || {
                            let visible = rows
                                .get(ordinal)
                                .map(|members| mask.visible_rows(members))
                                .unwrap_or_default();
                            crate::derived::compute(&declared_derived, &visible, &locator)
                        }))
                        .clone()
                    };
                    // The predicate or the authored shape, **only where the request asked for the
                    // shape** (`polygon-membership.md` §7.1) and the row is materialised — the
                    // identity projection carries no geometry and no content at all.
                    let mut content = content;
                    let mut derived = derived;
                    let shape_guard_fired = if artifact_rows == ArtifactRows::Full
                        && computed.selects(crate::derived::ComputedProperty::Hull)
                    {
                        self.drawn_shape(
                            &layer.declaration,
                            view_name,
                            &name,
                            level,
                            ordinal,
                            &mut content,
                            &mut derived,
                            Some(zoom),
                        )
                    } else {
                        false
                    };
                    // **The parent comes from the level's own records and the key from the store.**
                    // Both are per-ordinal facts of one generation, but only one of them is held
                    // in the row form: a key is a caller's string, one per artifact, and copying
                    // ten million of them into a cached structure buys nothing the store's own
                    // lookup does not already answer. The key is payload, so the identity
                    // projection skips the lookup.
                    let parent = rows.parent(ordinal);
                    let key = match artifact_rows {
                        ArtifactRows::Identity => None,
                        ArtifactRows::Full => self
                            .write
                            .with_artifacts(|store| store.get(&name, level, ordinal)?.key.clone()),
                    };
                    // Recorded, not resolved: which artifacts this response holds is not known
                    // until every layer and level has been walked, and a parent — or the artifact
                    // a dependent hangs from — may sit in a level this loop has not reached.
                    served_at.insert((name.clone(), level, ordinal), tessera_id);
                    placed.push(Placement {
                        at: (name.clone(), level, ordinal),
                        parent: parent.map(|p| (name.clone(), p.level, p.ordinal)),
                        attached_to: rows
                            .attachment(ordinal)
                            .map(|a| (a.layer.clone(), a.level, a.ordinal)),
                    });
                    out.push(ArtifactOut {
                        content,
                        layer: name.clone(),
                        tessera_id,
                        key,
                        masked_count,
                        derived,
                        // The declared level. On a treed layer — where every artifact sits at
                        // level 0 and the rung is the response-local chain depth — this is
                        // recomputed below, once the response's row set is final.
                        rung: level,
                        // Filled in below, once the response's own membership is settled.
                        parent_id: None,
                        // Asked only of the artifacts that survived the cut: the bit describes what
                        // is served, and an artifact the response drops has no row to carry one.
                        matched: matched.as_ref().map(|m| rows.matches(m, ordinal)),
                        shape_guard_fired,
                    });
                }
            }
            served_layers.push(ServedLayer {
                name,
                levels: served_levels,
            });
        }
        // **The cut ran after the verdicts, so a dependent may have passed on a target this
        // response then removed.** Dropping it here — before the parents are resolved, so a
        // dependent that goes takes its own name out of `served_at` with it — is what keeps one
        // response from describing a cluster it does not contain (decision 0089).
        let dropped = orphaned_dependents(&placed, &in_request, &mut served_at);

        // **A dependent carries its target's masked count** (D13; owner ruling 2026-08-25): a
        // label describes its cluster, so the number beside it is the cluster's — how many of
        // *that* artifact's members this principal can see — and not the label's own membership,
        // which a publisher may leave empty. The target is in this response with that very count
        // (the drop above guarantees it), so the value is derivable from the artifacts frame and
        // discloses nothing new (decision 0023). Filter-blind, as every masked count is
        // (`MaskedSet::count_intersection`, I12): the request's filter never moves it.
        let count_at: std::collections::BTreeMap<&(String, u32, u32), u64> = placed
            .iter()
            .zip(&out)
            .map(|(place, artifact)| (&place.at, artifact.masked_count))
            .collect();
        let target_counts: Vec<Option<u64>> = placed
            .iter()
            .map(|place| {
                place
                    .attached_to
                    .as_ref()
                    .filter(|target| in_request.contains(&target.0))
                    .and_then(|target| count_at.get(target).copied())
            })
            .collect();
        // **And its target's filter bit, on D13's own argument** (decision 0104). A label describes
        // its cluster, so *does anything here match* is a question about the cluster; the label's
        // own membership is often empty, and a bit over it would read `false` for every label under
        // every filter — the same defect the count rule exists to prevent, in the field beside it.
        // Derivable from the target's own row in this response, which the drop above guarantees is
        // present, so it discloses nothing new (decision 0023).
        let matched_at: std::collections::BTreeMap<&(String, u32, u32), Option<bool>> = placed
            .iter()
            .zip(&out)
            .map(|(place, artifact)| (&place.at, artifact.matched))
            .collect();
        let target_matched: Vec<Option<Option<bool>>> = placed
            .iter()
            .map(|place| {
                place
                    .attached_to
                    .as_ref()
                    .filter(|target| in_request.contains(&target.0))
                    .and_then(|target| matched_at.get(target).copied())
            })
            .collect();

        // **A parent is named only where it is also in this response**, which is the whole of the
        // disclosure rule for this field. An artifact whose parent exists but was withheld — below
        // its own criterion for this viewer, suppressed, or dropped by the frontier — carries a
        // null here, indistinguishable from a root. Naming it would tell the viewer that a coarser
        // grouping exists which they are not cleared to see, which is a disclosure the rest of this
        // pass takes care to avoid making.
        let mut served = Vec::with_capacity(out.len());
        for ((((mut artifact, place), dropped), target_count), target_bit) in out
            .into_iter()
            .zip(&placed)
            .zip(dropped)
            .zip(target_counts)
            .zip(target_matched)
        {
            if dropped {
                continue;
            }
            if let Some(count) = target_count {
                artifact.masked_count = count;
            }
            if let Some(bit) = target_bit {
                artifact.matched = bit;
            }
            artifact.parent_id = place
                .parent
                .as_ref()
                .and_then(|key| served_at.get(key))
                .copied();
            served.push(artifact);
        }
        // **A treed layer's rung is the response-local parent-chain depth** — the depth of each
        // row in the forest this response's own `parent_id` links form
        // (`artifact-fetch-protocol.md` §5.3). Computed here, after the cut, the content
        // withholds and the dependent drop, because those are what make the forest
        // response-local: a row whose ancestors were pruned, withheld or cut away is a root of
        // its subtree and reads 0, whatever its depth in the stored tree.
        if !treed.is_empty() {
            let parent_of: std::collections::HashMap<u64, Option<u64>> = served
                .iter()
                .filter(|a| treed.contains(&a.layer))
                .map(|a| (a.tessera_id.raw(), a.parent_id.map(|p| p.raw())))
                .collect();
            let edges = parent_of.values().filter(|p| p.is_some()).count();
            let mut depths: std::collections::HashMap<u64, u32> =
                std::collections::HashMap::with_capacity(parent_of.len());
            for artifact in served.iter_mut().filter(|a| treed.contains(&a.layer)) {
                artifact.rung =
                    response_depth(artifact.tessera_id.raw(), &parent_of, &mut depths, edges);
            }
        }
        // **The membership column's served set is `served_at` after the drop** — exactly the
        // artifacts in `served`, and the only identifiers the column can name.
        for ((name, level, ordinal), tessera_id) in &served_at {
            if let Some(slot) = served_layers
                .iter_mut()
                .find(|l| &l.name == name)
                .and_then(|l| l.levels.iter_mut().find(|l| l.level == *level))
            {
                slot.served.insert(*ordinal, *tessera_id);
            }
        }
        Ok((served, served_layers))
    }
}

/// The response-local parent-chain depth of one served treed artifact — its `rung`
/// (`artifact-fetch-protocol.md` §5.3).
///
/// `parent_of` holds every served row of the treed layers, keyed by `tessera_id`, valued with the
/// response's own `parent_id` — which, by that field's contract, only ever names an identifier in
/// the same response, and within the artifact's own layer. `None`, and an identifier `parent_of`
/// does not hold, are both roots: *no parent in this response* is rung 0, whatever the stored
/// tree says.
///
/// Memoised through `depths` because ancestors are shared, exactly as [`crate::cut::Lineage`]'s
/// depth table is; the cycle guard is the edge count, as there — the publish refuses a cycle, so
/// exceeding it means a malformed store, and the fail-safe answer is a root.
fn response_depth(
    id: u64,
    parent_of: &std::collections::HashMap<u64, Option<u64>>,
    depths: &mut std::collections::HashMap<u64, u32>,
    edges: usize,
) -> u32 {
    let mut chain: Vec<u64> = Vec::new();
    let mut at = id;
    let base = loop {
        if let Some(&known) = depths.get(&at) {
            break known;
        }
        match parent_of.get(&at).copied().flatten() {
            None => {
                depths.insert(at, 0);
                break 0;
            }
            Some(up) => {
                if chain.len() > edges {
                    depths.insert(at, 0);
                    break 0;
                }
                chain.push(at);
                at = up;
            }
        }
    };
    let mut depth = base;
    for &node in chain.iter().rev() {
        depth += 1;
        depths.insert(node, depth);
    }
    depths[&id]
}

/// Where one served artifact sits, and what it points at.
///
/// Both edges are recorded during the walk and resolved after it, because whether either end is in
/// the response is not known until every layer and level has been walked.
struct Placement {
    /// Its own address — `(layer, level, ordinal)`, the triple an [`Attachment`] carries.
    at: (String, u32, u32),
    /// The address of its parent, where it names one. Within its own layer by construction.
    parent: Option<(String, u32, u32)>,
    /// The address of the artifact it depends on, where its layer declares a dependency.
    attached_to: Option<(String, u32, u32)>,
}

/// **A dependent whose target this response does not contain, and everything hanging from it.**
///
/// [Decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)
/// makes a dependent visible exactly where its target is, and `Engine::dependency_served` enforces
/// that by asking the target's own `verdict`. **The cut runs after the verdicts** — it serves fewer
/// artifacts and never evaluates fewer — so a request carrying an `artifact_budget` over a treed
/// layer, alongside a layer depending on it, would otherwise be answered with labels describing
/// clusters that same response does not hold. One response never contradicts itself.
///
/// **Server-side, and the attachment identifier never reaches the wire.** Publishing it so a client
/// could filter for itself was declined for the reason `parent_id` carries a null rather than a
/// withheld parent's name: handing over the identifier names an artifact the response does not
/// contain. A client never told the relationship cannot notice what is missing from it.
///
/// **The target's layer must be in this request.** A request naming the dependent layer *alone* —
/// "give me just the labels" — finds no target here, and a naive lookup would drop every label.
/// That is a legitimate call and refusing it is outside the disclosure surface; such a request
/// behaves exactly as it did before this pass existed. What the condition catches is a response
/// that walked the target's layer and did not serve the target: cut to a budget, pruned in favour
/// of a child, or withheld at the emit step. A target outside this viewport falls under the same
/// rule and is dropped with them — its label is describing something this response does not draw,
/// and separating the two cases would mean carrying a reason per absent candidate through a pass
/// that deliberately collapses reasons.
///
/// **This does not make the budget a disclosure control**
/// ([decision 0083](../../../docs/decisions/0083-the-frontier-is-a-request-time-budget.md) stands).
/// The pass can only remove, and everything it removes already passed its own test. It decides what
/// is *drawn*, and a label describing something not drawn is not drawn either.
///
/// Chains cascade: a label on a label goes when the label it hangs from goes. The worklist walks
/// the edges the response holds rather than rescanning it per drop, and terminates because each
/// index is dropped at most once — a chain deeper than `DEPENDENCY_CHAIN_MAX` was already refused
/// by the prerequisite that admitted these artifacts in the first place.
///
/// Returns one flag per placement, positionally, and removes what it drops from `served_at` so a
/// dropped artifact cannot be named as anything's parent.
fn orphaned_dependents(
    placed: &[Placement],
    in_request: &std::collections::BTreeSet<String>,
    served_at: &mut std::collections::BTreeMap<(String, u32, u32), TesseraId>,
) -> Vec<bool> {
    let mut dependents_of: std::collections::BTreeMap<(&str, u32, u32), Vec<usize>> =
        std::collections::BTreeMap::new();
    let mut queue: Vec<usize> = Vec::new();
    for (i, place) in placed.iter().enumerate() {
        let Some(target) = &place.attached_to else {
            continue;
        };
        dependents_of
            .entry((target.0.as_str(), target.1, target.2))
            .or_default()
            .push(i);
        if in_request.contains(&target.0) && !served_at.contains_key(target) {
            queue.push(i);
        }
    }
    let mut dropped = vec![false; placed.len()];
    while let Some(i) = queue.pop() {
        if dropped[i] {
            continue;
        }
        dropped[i] = true;
        served_at.remove(&placed[i].at);
        // Whatever hung from it goes too, and its target's layer is in this request by
        // construction — this response walked the layer, which is how the artifact reached `out`.
        let at = &placed[i].at;
        if let Some(hanging) = dependents_of.get(&(at.0.as_str(), at.1, at.2)) {
            queue.extend(hanging.iter().copied());
        }
    }
    dropped
}

/// Test every row of `domain` against `entities`, giving the rows that matched.
///
/// `None` where the row space declined to invert a row — see the call site.
///
/// Parallel over the domain, on the engine's own pool (D-D: there is one), because the route it
/// competes with is parallel over *its* axis and a serial walk here would move the crossover
/// without anything in the design saying so. Chunks are cut by row count rather than by range, so
/// neither a viewport of one huge range nor one of a thousand slivers defeats the split.
fn per_tile_crossing(
    row_space: &tessera_store::permutation::RowSpace,
    entities: &croaring::Bitmap,
    domain: &[Range<u32>],
    rows_in_ranges: u64,
) -> Option<croaring::Bitmap> {
    per_tile_crossing_multi(row_space, &[entities], domain, rows_in_ranges)
        .map(|mut images| images.pop().expect("one set in, one image out"))
}

/// [`per_tile_crossing`] over several entity sets at once — **one walk, one `entity_of` per row**,
/// however many entity-space verdicts a mixed tree carries. This is what keeps 0062's
/// one-crossing rule true for the row route: the expensive half of a crossing is the inversion,
/// and each additional set costs one bitmap probe per row on top of it, not a second walk.
///
/// Returns one row image per input set, positionally. `None` where the row space declined to
/// invert a row — the caller falls back to projection, same as the single-set form.
fn per_tile_crossing_multi(
    row_space: &tessera_store::permutation::RowSpace,
    entity_sets: &[&croaring::Bitmap],
    domain: &[Range<u32>],
    rows_in_ranges: u64,
) -> Option<Vec<croaring::Bitmap>> {
    let chunks = domain_chunks(domain, rows_in_ranges);

    let parts: Option<Vec<Vec<croaring::Bitmap>>> = chunks
        .par_iter()
        .map(|chunk| {
            // Rows accumulate ascending into a small buffer per set and enter the bitmap in
            // batches: `add_many` on a sorted run appends to the container being built, where a
            // per-row `add` re-locates it every time.
            let mut rows: Vec<croaring::Bitmap> = entity_sets
                .iter()
                .map(|_| croaring::Bitmap::new())
                .collect();
            let mut bufs: Vec<Vec<u32>> = entity_sets
                .iter()
                .map(|_| Vec::with_capacity(1024))
                .collect();
            for row in chunk.clone() {
                let entity = row_space.entity_of(RowId::new(row))?;
                let raw = entity.raw() as u32;
                for (i, set) in entity_sets.iter().enumerate() {
                    if set.contains(raw) {
                        bufs[i].push(row);
                        if bufs[i].len() == 1024 {
                            rows[i].add_many(&bufs[i]);
                            bufs[i].clear();
                        }
                    }
                }
            }
            for (image, buf) in rows.iter_mut().zip(&bufs) {
                image.add_many(buf);
            }
            Some(rows)
        })
        .collect();

    let parts = parts?;
    let images = (0..entity_sets.len())
        .map(|i| {
            let refs: Vec<&croaring::Bitmap> = parts.iter().map(|p| &p[i]).collect();
            croaring::Bitmap::fast_or(&refs)
        })
        .collect();
    Some(images)
}

/// Cut `domain` into parallel chunks by row count — shared by the crossing walk and the
/// render-column scan, so the two fan out identically. Chunks are cut by row count rather than by
/// range, so neither a viewport of one huge range nor one of a thousand slivers defeats the split.
fn domain_chunks(domain: &[Range<u32>], rows_in_ranges: u64) -> Vec<Range<u32>> {
    let threads = rayon::current_num_threads().max(1) as u64;
    let target = (rows_in_ranges / (threads * 8))
        .max(CROSSING_CHUNK_MIN_ROWS as u64)
        .min(u32::MAX as u64) as u32;
    domain
        .iter()
        .flat_map(|range| {
            (range.start..range.end)
                .step_by(target as usize)
                .map(move |start| start..range.end.min(start.saturating_add(target)))
        })
        .collect()
}

/// The emit pass's hard per-frame accumulation cap, applied under any `flush_bytes` — including
/// the deliberately huge value that means "one flush per response". The wire's frame length is a
/// `u32`, so an unbounded accumulation would panic at serialisation; 1 GiB keeps a frame two
/// factors below that bound while being far above any threshold an operator would set on
/// purpose. Not a knob: nothing legitimate sits on the other side of it.
const MAX_POINTS_FRAME_BYTES: usize = 1 << 30;

/// Calibration task threshold: below this many total rows spanned by a request's resolved tiles
/// PLUS its §3.3 underlay cell demand if any (`Σ range.len() + underlay_cells_demanded`, pre-mask
/// — see the call site's `total_rows_in_ranges`), `Engine::viewport` folds `tile_sweep` serially
/// instead of calling `self.pool.install`.
///
/// `pub` (unlike [`TILE_PAR_MIN_LEN`]) so the byte-equality tests in `tests/viewport.rs` can
/// assert a fixture genuinely cleared it, rather than duplicating the number and risking drift.
///
/// **§14 re-calibration (post-B9, three scales, sweeps run on merge commit `2c19e13` —
/// `concurrency/viewpath` merged with `main`'s spilling build pipeline, `main` itself already
/// carrying B9's decode via the earlier `3862a61` merge — 2026-07-31): 200,000 → 500,000,000.**
/// B9's three-tier adaptive selection decode (`a62341f`) made per-row
/// count/select/gather cost enough cheaper that the 2.42M-only calibration this constant
/// originally carried (see the superseded argument below, kept for its still-valid tile-count
/// reasoning) stopped holding at realistic corpus sizes — `bench-1e9-report.md` measured
/// `compute_threads = default` **2.52x slower** on a small viewport and **2.19x slower** on the
/// "large work" viewport that was supposed to win, at 1e9. Re-swept at three scales (2.42M, 1e8,
/// 1e9; `examples/calibration_sweep.rs --bundle <path> [--dense]`, dense = the bench's own
/// `GrantShape::Random`) with **no single row-count threshold able to serve all three** — the
/// numbers force this, not a preference:
///
/// - At 1e9 (the bench's own dense grant, the realistic one):
///   every "natural" client-viewport-shaped sample (a fixed-size window at any zoom) measured
///   SERIAL-favouring, up to the highest row count that shape family reached in the sweep
///   (354,900,645 rows). No amount of additional row count made that family favour parallel under
///   a realistic grant.
/// - At 1e8, the "full-extent" family (a bbox spanning the whole 65536×65536 grid — few, very
///   large, near-uniformly-dense tiles) measured reliably PARALLEL-favouring, and by construction
///   spans essentially the WHOLE segment: 100,000,000 rows at this scale.
/// - **100,000,000 < 354,900,645.** A threshold that protects 1e9's natural-viewport regression
///   (needs > 354,900,645) is *necessarily* above 1e8's genuine full-extent win (needs <
///   100,000,000) — one number cannot sit on both sides of that gap. (2.42M sharpens the same
///   point from the other end: its own genuine parallel wins topped out under 2,422,486 rows
///   total, so a threshold large enough to protect 1e9's regression forfeits 2.42M's wins outright
///   — the corpus is smaller than the threshold needs to be.) A fraction-of-corpus formula was
///   tried and rejected on the same data: 1e9's natural family stays serial-favouring up to 35.5%
///   of the segment's rows, while 2.42M's own genuine parallel wins started around 12-40% of ITS
///   (much smaller) segment — the same *fraction* is parallel-favourable at one scale and
///   serial-favourable at another, so scaling the threshold by corpus size does not separate the
///   classes either. Full numbers, both grants, all three scales: calibration report §14.
///
/// **Resolution landed here, reported rather than silently chosen.** Given the asymmetric cost
/// this whole task keeps finding (wrongly-parallel measured 2-9x slower here and up to 20x in
/// earlier rounds; wrongly-serial forfeits a win worth at most ~2-3x, never a regression against
/// the pre-parallel baseline) and that ordinary client viewport traffic — not a whole-corpus
/// low-zoom scan — is what this constant exists to protect, `500,000,000` is set above the
/// highest observed natural-family SERIAL-favouring row count (354,900,645, ~40% margin) so
/// realistic client traffic stays serial and safe at every scale this task could build a fixture
/// for. **This makes the fan-out DORMANT for essentially all traffic at 2.42M and 1e8** (neither
/// corpus has 500,000,000 rows to spend on one request) **and for "natural" traffic at 1e9**; it
/// remains reachable at 1e9 for a whole-corpus-scale request (1,000,000,000 rows comfortably
/// clears the bar). The machinery is not deleted — see [`TILE_PAR_MIN_LEN`]'s doc, re-measured at
/// this same three-scale sweep and unchanged, for what still runs on the far side of this
/// threshold. **This is a reported recommendation, not a claim of the unique right number**: the
/// calibration report's §14 concerns section lays out the constant/formula/knob tension in full
/// and flags that a corpus-size- or deployment-aware knob is the more complete fix if the
/// controller wants one — this constant is the safe, single-number compromise that ships without
/// one.
///
/// **Superseded 2.42M-only argument, kept for its tile-count reasoning (still valid) — see above
/// for why an updated row-count threshold from this argument no longer transfers.** Tile count
/// does NOT discriminate: the "natural" viewport family (a fixed-size client window at increasing
/// zoom) resolves a near-constant ~289 tiles at every zoom from 6 to 14 regardless of density, yet
/// the measured serial/parallel verdict at that SAME tile count varies with how many rows those
/// tiles actually spanned — true at every scale re-measured in §14, not just 2.42M. `rows_in_ranges`
/// tracks that driver directly and is consistent with the measured cost model this codebase
/// designs against (module doc, and CLAUDE.md: "bitmap operations cost O(containers touched)",
/// which scales with the range read, not the tile count) — it is still the right *kind* of
/// predictor, it just no longer maps to one right *number* across scales.
///
/// Mask density remains unmodelled by this pre-mask predictor, and deliberately so: the re-sweep
/// used both a sparse and the bench's dense grant at every scale and found the shape-family split
/// above (natural vs full-extent) under BOTH, so mask density is not the dominant driver of the
/// three-scale tension.
pub const SERIAL_FALLBACK_MAX_ROWS: u64 = 500_000_000;

/// The predictor, pulled out as its own pure function so it is unit-testable without an `Engine`
/// or a bundle (see the `tests` module at the bottom of this file) — the behavioural claim ("a
/// below-threshold request runs the serial fold") is otherwise only observable through output
/// equality or timing, neither of which makes a good unit test on its own.
///
/// Takes `threshold` explicitly rather than reading `SERIAL_FALLBACK_MAX_ROWS`
/// directly, so the one call site (`Engine::viewport`) can supply either the production constant
/// or a test's override — see `Engine::set_serial_fallback_max_rows_for_test`'s doc for why an
/// override exists at all and why it lives on `Engine`, not here.
/// **Two terms since 2026-08-01** (owner decision, on the two-axis sweep —
/// `docs/evidence/memos/2026-07-31-tile-parallelism-calibration.md`, "Follow-up 4, answered", and
/// `probes/2026-08-01-two-axis-sweep/`). The row term is unchanged; [`TILE_PAR_MIN_TILES`] is new,
/// and the fan-out runs when **either** fires.
#[inline]
fn should_fold_serially(total_rows_in_ranges: u64, threshold: u64, tiles: usize) -> bool {
    total_rows_in_ranges < threshold && tiles < TILE_PAR_MIN_TILES
}

/// The tile-count arm: at or above this many tiles, take the fan-out whatever the row count says.
///
/// **Why a second term exists.** §14's campaign proved no row-count constant and no
/// fraction-of-corpus formula can classify correctly, and named "row count, tile count" as the
/// disproved quantities — but it never varied tile count independently. The `natural` family holds
/// it near-constant (81 at z4, 289 from z5 up, at every scale, because its span formula is exactly
/// 16 cells wide at every depth) and `full-extent` tops out at 1,024, so **no measurement in that
/// campaign exceeded 1,024 tiles**. The 2026-08-01 sweep varied it — constant bbox, varying depth,
/// 210 cells over 35 shapes x 3 scales x 2 grant densities, each cell a single-variable A/B of this
/// very branch — and found the axis the campaign could not see.
///
/// **Measured.** Total regret against the per-cell best arm falls **244.47 ms -> 18.38 ms (13.3x)**;
/// worst single cell 4.09x -> 2.81x; misclassified cells 73/210 -> 27/210. Both terms earn their
/// place: dropping the row term costs 56% more regret (it catches `full-extent` at 10^9, which has
/// only 16-1,024 tiles), and dropping this one is the status quo. 4,096 is an optimum on that data
/// rather than a round number — 2,048 measures 20.37 ms, 8,192 measures 34.55 ms.
///
/// **Why tile count is the stable axis and row count is not.** The per-tile *floor* — range setup,
/// `count_range` entry, probe overhead — measures **85 -> 141 ns across a 400x change in corpus
/// size and both grant densities**, mask-independent by construction. The row coefficient has no
/// such stability: **0.46 ns/row** for a whole-corpus z2 shape at 2.42M against **0.0019 ns/row**
/// for a natural z4 shape at 10^9 — a 240x spread in the same coefficient. That is B9's tiered
/// decode stated as a mechanism: rows stopped measuring work, tiles did not.
///
/// **This cannot reopen the 10^9 regression the row term protects.** The 354,900,645-row shape that
/// sized [`SERIAL_FALLBACK_MAX_ROWS`] is `natural/z4/s6`, which resolves **81 tiles** — this
/// threshold sits 14-50x above the entire `natural` family, and all eighteen of its cells
/// re-measured serial-favouring. `the_natural_family_cannot_reach_the_tile_arm` pins the arithmetic.
///
/// **What it does not fix**, recorded so nobody reads it as complete: every residual above 1.6x is
/// `full-extent` at <= 1,024 tiles — a whole-corpus sweep at low zoom, where this axis has nothing
/// to say and the row term is below threshold because the corpus is. That is §14.4's tension,
/// undiminished; the calibration memo's recommendation 1 (a corpus-size-aware row threshold) is
/// what addresses it. Everything this arm itself introduces is <= 1.57x and <= 0.21 ms absolute, in
/// six cells, all at exactly 4,225 tiles.
///
/// **The label axis was then tested too, and it does not move this number** *(2026-08-01;
/// `probes/2026-08-01-label-contiguity/`, memo "Follow-up 4, addendum: the label axis")*. The
/// original sweep used one label configuration, and contiguity — measured run ratio spanning
/// 1.00-5.11 across label sets at equal coverage — feeds per-tile work directly, so it was the
/// obvious way for a single constant to be wrong. Re-swept at 2.42M across `surnames`,
/// `categories-subclass` and `categories-archive` at matched coverage (measured row-space run ratio
/// **1.05 / 1.89 / 4.03**, spanning the whole published range): the crossover is **identical in
/// every cell**, highest serial-favouring tile count 4,225 in all three, nothing at or above 8,281
/// serial-favouring anywhere. Over 315 cells, 4,096 is the optimum on this axis as well — 12.7x
/// less regret than the one-term predictor, and the only candidate with a worst case under 4x.
///
/// **A hypothesis was falsified on the way, and it is worth stating because it is the intuitive
/// one.** The prediction was that a *more contiguous* mask means fewer containers, so less work per
/// tile, so the floor dominates longer and the crossover rises — making `categories-archive` the
/// risk case and implying a higher threshold. Measured, the opposite: at fixed grant width the most
/// contiguous mask had the *lowest* crossover, and both movements vanished once coverage was
/// matched. **What moves the crossover is coverage, not contiguity.** 8,192 — the threshold that
/// hypothesis implied — measures 1.80x *worse* on `categories-archive`, the very set it would have
/// been protecting.
///
/// Related, and why fixed-width grants mislead here: a fixed `w` is not a fixed principal. `w = 10`
/// buys 47.8% of the corpus on archive's 38-term dictionary and **0.0051%** on surnames' 404,104-term
/// one, varying coverage and contiguity together and in opposite directions.
///
/// **Still untested:** contiguity at 1e8/1e9. At 2.42M the corpus spans 37 containers total and
/// every non-degenerate mask touches all of them, so containers-touched never varied — contiguity
/// showed up only as run structure *within* containers. If it bites anywhere it is at scale.
pub const TILE_PAR_MIN_TILES: usize = 4_096;

/// The `natural` viewport family cannot reach [`TILE_PAR_MIN_TILES`] — **checked by the compiler,
/// not by a test.**
///
/// Anonymous (`const _`) to match the pattern already used for this class of guard in
/// `tessera-server/tests/http.rs`; naming it would only make it dead code.
///
/// This is the property that makes the tile arm unable to reopen the 10⁹ regression
/// [`SERIAL_FALLBACK_MAX_ROWS`] exists to protect. The family's span is exactly 16 cells wide at
/// every depth, so it resolves 81 tiles at z4 and 289 from z5 up, at every scale — including
/// `natural/z4/s6`, the 354,900,645-row shape that sized the row threshold.
///
/// A runtime `assert!` over two constants can only fail in a binary that was already built, which
/// is the wrong moment: by then the arm can already fire on ordinary client viewports at 10⁹. As a
/// `const` item it fails to **compile** instead, so a future worker who widens the family's span or
/// lowers the arm is stopped at the point of the edit and told which guarantee they are spending.
const _: () = {
    const NATURAL_MAX_TILES: usize = 289;
    assert!(
        NATURAL_MAX_TILES < TILE_PAR_MIN_TILES,
        "the natural viewport family (289 tiles at z5+) would reach the tile arm: the fan-out \
         could then fire on ordinary client viewports at 10^9, which is the regression \
         SERIAL_FALLBACK_MAX_ROWS was raised to 500,000,000 to fix."
    );
};

/// D-F's per-tile scheduling grain: the number of tiles rayon hands to one worker before it will
/// split the range again. Only reachable once `total_rows_in_ranges >= SERIAL_FALLBACK_MAX_ROWS`
/// (the calibration task's serial fallback, above) — this grain governs the fan-out's own
/// behaviour, not whether it runs at all.
///
/// **Measured (2.42M `categories-subclass`, w=10, 12-core WSL2 box, 2026-07-31)**, sweeping
/// 4/8/16/32/64 across six clearly-parallel shapes (natural client windows and full-extent views
/// spanning 16-1,024 tiles — table in the calibration report). 16 and above were consistently and
/// often substantially worse than 4 or 8 (e.g. a 16-tile full-extent view: ~1.6 ms at 4 vs ~2.8 ms
/// at 16 vs ~3.2 ms at 64) — confirming this constant's original "keep it small" reasoning, kept
/// below verbatim. Between 4 and 8, two repeated trials found 8 reproducibly at least as fast
/// everywhere tested and meaningfully faster on the lower-tile-count shapes (a 289-tile natural
/// window: ~1.0 ms at 4 vs ~0.8 ms at 8; a 16-tile full-extent view: ~1.8 ms at 4 vs ~1.6 ms at
/// 8), with no shape favouring 4. `8` replaces the original argued-not-measured `4`.
///
/// **The original reasoning, still the shape of the argument, only the number moves.** Measured
/// per-tile cost is highly non-uniform — an empty-tile skip (`tile_sweep` returning `Ok(None)`
/// after one `count_range`) is a handful of comparisons, while a dense tile at a high cap is a
/// bitmap-range read plus a bounded heap sort — so work-stealing needs to be able to move
/// *individual* tiles between workers rather than being locked into a few large, coarse chunks; a
/// chunk of, say, 64 tiles handed to one worker while the other workers' chunks are all-empty
/// would sit unstolen for the length of that chunk. `1` (rayon's own default for `par_iter`
/// without `with_min_len`) avoids that entirely but pays a scheduling/steal-queue overhead on
/// every single tile, including the very common empty-tile skip that is otherwise nearly free.
/// `8` is a conservative middle point: small enough that a viewport of a few hundred tiles still
/// splits into dozens of independently-stealable chunks, large enough to amortise the per-task
/// overhead over the cheap tiles that dominate a sparse or clustered corpus — and, unlike `4`, the
/// value the sweep actually measured as best or tied-best on every shape tried.
///
/// **§14 re-calibration (post-B9, three scales, 2026-07-31): re-measured, UNCHANGED.** The
/// coordinator's own hypothesis going in was that B9's cheaper per-row decode might favour a much
/// bigger chunk (values up to 512 were swept: 8/32/128/512, `examples/min_len_sweep.rs`, on the
/// `full-extent` shape family and `natural/z4`).
///
/// **Claim, scoped precisely — it does not generalise.** `8` is
/// decisively best on `full-extent/{z2,z3,z4,z5}` at 1e8 and 1e9 — these are the shapes that
/// actually reach the parallel branch at the calibrated threshold (`full-extent`'s row count is
/// ~always the whole segment, comfortably above 500,000,000 at those two scales), and the wins
/// there are large, not marginal (1e9 full-extent/z3: 2.16 ms at 8 vs 3.02 ms at 32, 4.00 ms at
/// 128, 3.58 ms at 512; full-extent/z4: 1.76 ms at 8 vs 2.92/3.88/4.27 ms). It is NOT uniformly
/// best everywhere measured, and the claim must not be read that way: `full-extent/z1` (only 4
/// tiles — too few units for a small grain to help) measured faster at 512 than at 8 (1e9: 2.97 ms
/// vs 3.25 ms), and `natural/z4` — which no longer reaches the parallel branch in production at
/// any scale this task tested, since its own row count tops out at 354,900,645, below the new
/// 500,000,000 threshold — sometimes measured faster at 32 than at 8 (1e9: 457 µs at 32 vs 832 µs
/// at 8; 1e8: 522 µs at 32 vs 766 µs at 8). `8` is kept on the strength of the shapes that matter
/// now, not because it won everywhere it was tried. Full table: calibration report §14.5.
const TILE_PAR_MIN_LEN: usize = 8;

/// One tile's sweep contribution (D-F): count, select and underlay — **no gather**, which the
/// emit pass does later from the `rows`/`parts` returned here (`streamed-serving.md` §4). The
/// pure per-tile body pulled out of what was, before streaming, a fused count-select-gather
/// loop. Safe to call concurrently from any rayon worker: every parameter is `&`-borrowed or
/// `Copy`, nothing here reaches back into `Engine` or any state shared across tiles (see the
/// guardrail comment at the row-projection cache call site in `Engine::viewport_stream`,
/// above), and the return value is owned outright by the caller — no shared mutable state, no
/// interior mutability, nothing to synchronise.
///
/// `Ok(None)` — an empty tile: no segment for this view, or nothing visible in `range`. Exactly
/// the "skip empty" rule the old inline loop applied (no count row, no selection work). `Err`
/// carries [`EngineError::Cancelled`] from the per-tile cancellation checkpoint below — checked
/// first, so a flip
/// observed here costs only the one atomic read, never any of this tile's own
/// count/select/underlay work.
#[allow(clippy::too_many_arguments)]
fn tile_sweep<'a>(
    tile: &Tile,
    tile_parts: &[(usize, Range<u32>)],
    mask: &EffectiveMask,
    segments: &[(&'a SegmentData, u32)],
    params: &SelectParams,
    zoom: u8,
    underlay_offset: Option<u8>,
    cancel: &Option<CancelToken>,
) -> Result<Option<TileSweepOut<'a>>> {
    check_cancelled(cancel)?;

    if tile_parts.is_empty() {
        return Ok(None);
    }

    // §14.2 fix: `rows_in_ranges` is no longer counted here. It is now summed once, mask-free,
    // over `ranges` in `Engine::viewport`'s serial prefix — see that call site's comment. Counting
    // it per-tile put it in `TileStats`, whose contribution this function's `Ok(None)` returns
    // (this one included, three lines below) cause `Engine::viewport`'s fold to discard outright —
    // silently making a documented-mask-independent field depend on which tiles a grant leaves
    // empty.
    let mut stats = TileProbe::new();

    // **The count is the sum over the segments the tile touches** — each segment's own tile range
    // shifted into view row space by its `row_base`, counted there, and added. §7.1's exact
    // masked count is a property of the tile, not of whichever segment happens to hold the rows,
    // so a tile straddling a build segment and a fresh flush segment must report their union.
    let parts: Vec<SelectionPart<'_>> = tile_parts
        .iter()
        .map(|(s, range)| {
            let (segment, row_base) = segments[*s];
            // **The count selection draws from, which under a filter is `M_sel`'s.** Selection
            // picks rows from the filtered set, and its decode-tier choice turns on this figure:
            // a fully-visible range takes the `FullRange` tier, which extends every row *without
            // consulting the mask*. Supplying the unfiltered count there served every row in the
            // range under a filter that had narrowed the counts correctly — the filter applied to
            // `matched` and bypassed entirely in what was drawn.
            //
            // `TileCount::visible` is computed separately and stays unfiltered; the two figures
            // answer different questions (§7.1).
            let visible = mask.count_matched_range(row_base + range.start..row_base + range.end);
            SelectionPart {
                segment,
                range: range.clone(),
                row_base,
                visible,
            }
        })
        .collect();
    // What selection draws from: `M_sel`'s count, equal to the composed count when unfiltered.
    let matched: u64 = parts.iter().map(|p| p.visible).sum();
    // The composed count — how many of this tile's items the principal may see, which a filter does
    // not change. Summed over the same segment ranges, so the two cannot disagree about which rows
    // this tile covers.
    let visible: u64 = tile_parts
        .iter()
        .map(|(s, range)| {
            let (_, row_base) = segments[*s];
            mask.count_range(row_base + range.start..row_base + range.end)
        })
        .sum();
    stats.lap(|t| &mut t.count_ns);

    if visible == 0 {
        // Skip empty: no count row, no selection work for a tile with nothing visible — the same
        // rule the old inline loop applied.
        return Ok(None);
    }
    stats.count(|t| &mut t.tiles_nonempty, 1);
    stats.count(|t| &mut t.sigma_visible, matched);

    // The owned list survives the call: the emit pass rebuilds a `SelectionParts` over it to
    // resolve each selected row at gather time.
    let part_list = parts;
    let parts = SelectionParts::new(&part_list);
    // Anchored on `matched`, not `visible`: selection's cap and tier decisions are about the set
    // it draws from. θ's *threshold* anchor is separate and stays unfiltered — `visible_total()`
    // — and on a filtered request the threshold arrives saturated (§8.5's match-layer rule; see
    // the params override in `Engine::viewport`), so `served = min(matched, cap)` there.
    let selected = Selection::of(mask, &parts, params, matched);
    stats.lap(|t| &mut t.select_ns);
    // Counted by `Selection::of` itself, inside the loops that do the reading — not from
    // `visible`, which would make the `visited == sigma_visible` cross-check a tautology.
    stats.count(|t| &mut t.select_rows_visited, selected.rows_visited);

    let count = TileCount {
        tile: tile.prefix,
        // The **composed** count, never the filtered one: `visible` answers "how many items here
        // may this principal see", which a filter does not change. §7.1 discloses it exactly.
        visible,
        // How many of those the filter admits. Equal to `visible` on an unfiltered request.
        matched,
        served: selected.rows.len() as u64,
    };

    // §3.3: the tile's sub-cells, each an exact masked count over a contiguous Morton range. Only
    // non-empty cells are emitted, exactly as empty tiles are skipped above.
    let mut sub_cells = Vec::new();
    if let Some(offset) = underlay_offset {
        let sub_depth = zoom + offset;
        let first = tile.prefix << (2 * offset as u32);
        for i in 0..(1u64 << (2 * offset as u32)) {
            let cell = first + i;
            let sub_tile = Tile {
                prefix: cell,
                depth: sub_depth,
            };
            // Search only the parent's range **in each segment**: sub-cells partition their
            // parent, so this is exactly what `tile_ranges` would return, over tens of kilobytes
            // already touched by the parent's own `count_range` rather than ~30 levels of a 4 GB
            // mmap. Summed across segments for the same reason the whole-tile count is: a sub-cell
            // count is an exact masked count of the cell, not of one segment's share of it.
            let sub_count: u64 = parts
                .as_slice()
                .iter()
                .map(|part| {
                    let local = tile_ranges_within(part.segment, &sub_tile, part.range.clone());
                    mask.count_range(part.row_base + local.start..part.row_base + local.end)
                })
                .sum();
            if sub_count > 0 {
                sub_cells.push(SubCellCount {
                    cell,
                    count: sub_count,
                });
            }
        }
        stats.lap(|t| &mut t.underlay_ns);
        // Evaluated, not emitted: the gap between this and `sub_cells.len()` is the work spent
        // discovering that a sub-cell was empty, which on a clustered corpus is most of it.
        stats.count(
            |t| &mut t.underlay_cells_evaluated,
            1u64 << (2 * offset as u32),
        );
    }

    Ok(Some(TileSweepOut {
        count,
        rows: selected.rows,
        parts: part_list,
        sub_cells,
        stats: stats.t,
    }))
}

/// One tile's parallel-sweep output — [`tile_sweep`]'s return payload, folded serially and
/// in-order into the request's `tile_counts`/`sub_cells`/[`StageTimings`] and then consumed by
/// the emit pass, which gathers `rows` through a `SelectionParts` rebuilt over `parts`. An
/// implementation detail of the sweep, not part of this crate's public API — [`ViewportOut`]
/// and the [`ViewportSink`] callbacks are what callers see.
///
/// `rows` are view-space rows **ascending by `tessera_id`** ([`Selection::rows`]) — the order
/// the wire requires within a tile, and the property every mid-stream cut's validity rests on.
struct TileSweepOut<'a> {
    count: TileCount,
    rows: Vec<u32>,
    parts: Vec<SelectionPart<'a>>,
    sub_cells: Vec<SubCellCount>,
    stats: TileStats,
}

/// A view's segments paired with their `row_base` in view row space, ascending.
///
/// **Keyed on `seg_id`, never zipped positionally.** `Bundle::with_segment` appends to `segments`
/// while `RowSpace::with_extent` appends the extent, so after a flush the two lists agree by
/// position — but `Bundle::with_merged` pushes the merged segment at the *end* of `segments` while
/// `RowSpace::collapsing` puts the merged extent where the consumed run was. After one merge the
/// positions diverge, and a positional zip would silently pair a segment with another segment's
/// `row_base`: every count right, every point drawn from the wrong entity. `seg_id`s are never
/// reused (contracts §2.1), so the lookup is exact.
///
/// The build segment is the one `permutation.bin` addresses and has no extent; it is therefore the
/// one with no entry in the row space, and its rows begin at 0.
///
/// **One definition, because two read paths need it.** `Engine::viewport` selects over the parts
/// and `Engine::item` resolves a single row to its owner; when `item` had its own version — take
/// `segments.first()` and index it with a *view*-space row — a drill-down on any flushed item
/// read past the build segment's end and panicked. A second copy is how the two come to disagree.
pub(crate) fn segments_with_row_bases<'a>(
    view: &str,
    view_data: &'a tessera_store::read::ViewData,
) -> Result<Vec<(&'a SegmentData, u32)>> {
    let row_bases: std::collections::HashMap<&str, u32> = view_data
        .row_space
        .extents()
        .iter()
        .map(|extent| (extent.seg_id.as_str(), extent.row_base))
        .collect();
    let mut base_seen = false;
    let mut segments: Vec<(&SegmentData, u32)> = Vec::with_capacity(view_data.segments.len());
    for segment in &view_data.segments {
        let row_base = match row_bases.get(segment.seg_id.as_str()) {
            Some(&row_base) => row_base,
            // No extent: the build segment, at 0. Legitimate exactly once — see
            // `EngineError::SegmentWithoutRowBase` for why a second one is a 500 rather than
            // another segment defaulted to 0.
            None if !base_seen => {
                base_seen = true;
                0
            }
            None => {
                return Err(EngineError::SegmentWithoutRowBase {
                    view: view.to_string(),
                    seg_id: segment.seg_id.clone(),
                })
            }
        };
        segments.push((segment.as_ref(), row_base));
    }
    // Ascending in `row_base`, which `SelectionParts::resolve`'s reverse scan relies on. Sorted
    // rather than assumed, for the `with_merged` reason above.
    segments.sort_unstable_by_key(|&(_, row_base)| row_base);
    Ok(segments)
}

/// Gather one row's `tessera_id`/position/declared scalars — zero-copy reads, no per-row
/// allocation beyond what a `Utf8` scalar's owned `String` requires.
///
/// The position takes one load from each of the two files that hold it, `morton.u32` for the
/// cell and `columns.arrow` for the residual, over the same row span the old `x`/`y` pair swept:
/// the same two loads, and the concatenation is a shift and an or.
/// One segment's declared columns, resolved once, in declaration order.
///
/// **Hoisted out of the row loop, and that is the whole point.** `ColumnsRef::scalar` is a hash
/// lookup on the column name plus an Arrow downcast; calling it per column *per row* made it 19
/// lookups per point on the wide fixture — 1.9e7 for a 10^6-mark viewport. Measured
/// (`tessera-bench --bin gather_shape`, 10^6 rows in 62-row tiles, 19 columns): resolving per row
/// costs **944 ms** against **136 ms** resolved per segment, ~86% of a gather whose shape is
/// otherwise unchanged.
///
/// `None` is a declared column this segment does not hold, kept **positionally** so the entry
/// order still matches the list resolved against — collapsing the absent ones here would silently shift
/// every later column left, which is the failure `gather_scalars` refuses at the write end.
type ResolvedScalars<'a> = Vec<Option<ScalarSlice<'a>>>;

/// Resolve `declared` against one segment's columns, once.
fn resolve_scalars<'a>(
    segment: &'a SegmentData,
    declared: &[DeclaredScalar],
) -> ResolvedScalars<'a> {
    declared
        .iter()
        // A declared scalar absent from this segment's schema resolves to `None` rather than
        // being an error — nothing here is authorisation-relevant, and the fail-closed check is at
        // the write end: `gather_scalars` refuses a segment missing a declared column, so a merge
        // or fold cannot propagate one. What reaches here is a read of a segment already
        // published.
        .map(|d| segment.columns.scalar(&d.name))
        .collect()
}

/// Gather one tile's selected rows **column-major**.
///
/// `rows` are view-space rows ascending by `tessera_id` — not by segment — so consecutive rows
/// can land in different parts. They are therefore resolved to `(part, local)` **once**, in one
/// pass, and every column then walks that placement rather than re-resolving per value. Together
/// with the per-part slice resolution this leaves the inner loop a bounds-checked index into a
/// typed slice, with no name lookup, no downcast and no per-value type dispatch.
///
/// The column set comes from `declared` and is always its length, so a request cannot end up with
/// a column set derived from whichever tile happened to be first.
///
/// A declared column a segment does not hold, or holds at another type, is a **malformed bundle**
/// rather than a silently skipped column. That cannot arise from a bundle this codebase wrote —
/// `gather_scalars` refuses it at the write end for every producer — and the alternative is to
/// append a short or wrongly-typed buffer under a name that does not describe it.
fn gather_tile_columns(
    parts: &SelectionParts<'_>,
    rows: &[u32],
    declared: &[DeclaredScalar],
) -> Result<PointColumns> {
    let placed: Vec<(u32, u32)> = rows
        .iter()
        .map(|&row| {
            let (part, _, local) = parts.resolve_indexed(row);
            (part as u32, local)
        })
        .collect();

    let mut tessera_ids = Vec::with_capacity(rows.len());
    let mut codes = Vec::with_capacity(rows.len());
    for &(part, local) in &placed {
        let segment = parts.as_slice()[part as usize].segment;
        let idx = local as usize;
        tessera_ids.push(segment.columns.tessera_id()[idx]);
        codes.push(
            ((segment.morton.u32()[idx] as u64) << 32) | segment.columns.residual()[idx] as u64,
        );
    }

    let resolved: Vec<ResolvedScalars<'_>> = parts
        .as_slice()
        .iter()
        .map(|part| resolve_scalars(part.segment, declared))
        .collect();

    let malformed = |d: &DeclaredScalar| {
        EngineError::Malformed(format!(
            "a segment of this view has no scalar column '{}' at the declared type {}, which \
             the manifest's render declaration requires; serving it would put values under \
             another column's name",
            d.name,
            d.arrow_type.arrow_type_name()
        ))
    };

    let mut scalars = Vec::with_capacity(declared.len());
    for (ci, d) in declared.iter().enumerate() {
        // The typed slice per part is resolved BEFORE the row loop, so the loop below carries no
        // `match` at all — that hoist is the whole reason this shape is cheaper than the
        // row-major one it replaced.
        macro_rules! build {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match d.arrow_type {
                    $(ScalarType::$v => {
                        let mut per_part: Vec<&[$t]> = Vec::with_capacity(resolved.len());
                        for r in &resolved {
                            match r[ci] {
                                Some(ScalarSlice::$v(s)) => per_part.push(s),
                                _ => return Err(malformed(d)),
                            }
                        }
                        let mut out = Vec::with_capacity(rows.len());
                        for &(part, local) in &placed {
                            out.push(per_part[part as usize][local as usize]);
                        }
                        ColumnBuf::$v(out)
                    })*
                    ScalarType::Bool => {
                        let mut per_part = Vec::with_capacity(resolved.len());
                        for r in &resolved {
                            match r[ci] {
                                Some(ScalarSlice::Bool(a)) => per_part.push(a),
                                _ => return Err(malformed(d)),
                            }
                        }
                        let mut out = Vec::with_capacity(rows.len());
                        for &(part, local) in &placed {
                            out.push(per_part[part as usize].value(local as usize));
                        }
                        ColumnBuf::Bool(out)
                    }
                    // A keyword shares this arm for `ColumnBuf::empty`'s reason: rendered, it is
                    // its bytes, and it is never rendered. A segment that carried anything else
                    // under the name refuses here rather than being served.
                    ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => {
                        let mut per_part = Vec::with_capacity(resolved.len());
                        for r in &resolved {
                            match r[ci] {
                                Some(ScalarSlice::Utf8(a)) => per_part.push(a),
                                _ => return Err(malformed(d)),
                            }
                        }
                        let mut out = Vec::with_capacity(rows.len());
                        for &(part, local) in &placed {
                            out.push(per_part[part as usize].value(local as usize).to_string());
                        }
                        ColumnBuf::Utf8(out)
                    }
                }
            };
        }
        scalars.push(flat_families!(build));
    }

    Ok(PointColumns {
        tessera_ids,
        codes,
        scalars,
        membership: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The predictor's behaviour at its boundary, both edges — asserted directly rather than
    /// through test-only instrumentation of the call site.
    #[test]
    fn should_fold_serially_is_a_strict_less_than_at_the_calibrated_boundary() {
        let t = SERIAL_FALLBACK_MAX_ROWS;
        assert!(should_fold_serially(0, t, 1));
        assert!(should_fold_serially(t - 1, t, 1));
        assert!(!should_fold_serially(t, t, 1));
        assert!(!should_fold_serially(t + 1, t, 1));
        assert!(!should_fold_serially(u64::MAX, t, 1));
    }

    /// `should_fold_serially` takes its threshold as a parameter (so
    /// `Engine::set_serial_fallback_max_rows_for_test` has something to feed it) — this pins that
    /// it is a genuine parameter, not the constant in disguise, at a threshold far from the real
    /// production value.
    #[test]
    fn should_fold_serially_honours_an_arbitrary_threshold_not_just_the_constant() {
        assert!(should_fold_serially(5, 10, 1));
        assert!(!should_fold_serially(10, 10, 1));
        assert!(!should_fold_serially(15, 10, 1));
        // The override this task added forces parallel unconditionally by setting the threshold
        // to 0 (`total_rows_in_ranges < 0` is never true for a `u64`) — pin that too.
        assert!(!should_fold_serially(0, 0, 1));
    }

    /// The tile arm at its exact boundary (owner decision 2026-08-01; [`TILE_PAR_MIN_TILES`] carries
    /// the 210-cell evidence).
    ///
    /// **The row count is held far below its threshold in every case**, so reverting to the
    /// one-term predictor fails here and nowhere else. That is the mutation this test exists to
    /// kill, and it was verified to do so.
    #[test]
    fn the_tile_arm_takes_the_fan_out_at_its_boundary_whatever_the_row_count_says() {
        let rows_far_below = 1_u64;
        let t = SERIAL_FALLBACK_MAX_ROWS;

        assert!(
            should_fold_serially(rows_far_below, t, TILE_PAR_MIN_TILES - 1),
            "one tile below the arm, rows far below their threshold: serial"
        );
        assert!(
            !should_fold_serially(rows_far_below, t, TILE_PAR_MIN_TILES),
            "AT the arm the fan-out runs though the row term alone would fold serially -- this is \
             the whole of the 2026-08-01 change"
        );
        assert!(!should_fold_serially(
            rows_far_below,
            t,
            TILE_PAR_MIN_TILES + 1
        ));
    }

    /// The rule is a disjunction: either term alone suffices and neither is necessary. A mutation
    /// making it a conjunction must fail.
    #[test]
    fn the_two_terms_are_a_disjunction_not_a_conjunction() {
        let t = SERIAL_FALLBACK_MAX_ROWS;
        assert!(!should_fold_serially(t, t, 1), "rows fire alone");
        assert!(
            !should_fold_serially(1, t, TILE_PAR_MIN_TILES),
            "tiles fire alone"
        );
        assert!(
            should_fold_serially(t - 1, t, TILE_PAR_MIN_TILES - 1),
            "neither fires -- the only serial case"
        );
        assert!(!should_fold_serially(t, t, TILE_PAR_MIN_TILES), "both fire");
    }

    /// The `natural` viewport family cannot reach the tile arm, which is what makes the 2026-08-01
    /// change unable to reopen the 10^9 regression [`SERIAL_FALLBACK_MAX_ROWS`] exists to protect.
    ///
    /// Its span is exactly 16 cells wide at every depth, giving 81 tiles at z4 and 289 from z5 up at
    /// every scale — including `natural/z4/s6`, the 354,900,645-row shape that sized the row
    /// threshold. Pinning the arithmetic tells a future worker who widens that span, or lowers the
    /// arm, which guarantee they are spending.
    ///
    /// **The family bound is asserted at compile time, not here.** Both operands are constants, so
    /// a runtime assertion over them can only fail in a binary that was already built — the
    /// `const _: () = { ... }` guard beside [`TILE_PAR_MIN_TILES`] fails to *compile* instead. This
    /// test carries the half that genuinely runs: the concrete shape that sized the row threshold
    /// still folds serially.
    #[test]
    fn the_natural_family_cannot_reach_the_tile_arm() {
        assert!(should_fold_serially(
            354_900_645,
            SERIAL_FALLBACK_MAX_ROWS,
            81
        ));
    }

    /// §14: `SERIAL_FALLBACK_MAX_ROWS` rose to 500,000,000 (see its doc). A fixture that genuinely
    /// crosses it is impractical to build inside a unit test — even the fast pipeline takes real
    /// minutes at that scale, and the §3.3 underlay route costs exactly the real per-cell work the
    /// threshold exists to gate, so there is no cheap way to reach it artificially either. The two
    /// `tests/viewport.rs` integration byte-equality tests that used to cross the (much lower)
    /// pre-§14 threshold now exercise the SERIAL branch on both `compute_threads` configs instead
    /// — still a real, useful claim (engine wiring is thread-count-independent end to end), just
    /// not the parallel branch specifically. What genuinely needs re-proof at any new threshold
    /// value is narrower and decoupled from `Engine`/mask/fixture size entirely: that rayon's
    /// INDEXED collect, over the exact shape `Engine::viewport` uses
    /// (`Vec<Result<Option<TileResult>>>`, never `Result<Vec<TileResult>>` — this module's doc),
    /// preserves input order regardless of pool size or `with_min_len`. That is what this test
    /// isolates, cheaply, at every pool/grain combination this file's constants and sweeps used.
    #[test]
    fn indexed_collect_of_tile_shaped_results_preserves_order_at_any_pool_size() {
        // The exact collect target shape as `tile_sweep`'s call sites use, without needing a
        // real `Engine`, `mask` or bundle to produce one: `Result<Option<T>>` per item, `Ok(None)`
        // standing in for `tile_sweep`'s empty-tile skip.
        let items: Vec<u32> = (0..2000).collect();
        let make = |i: &u32| -> Result<Option<u32>> {
            if i.is_multiple_of(7) {
                Ok(None)
            } else {
                Ok(Some(*i))
            }
        };
        let expected: Vec<Option<u32>> = items.iter().map(|i| make(i).unwrap()).collect();

        for &(threads, min_len) in &[
            (1usize, 1usize),
            (1, TILE_PAR_MIN_LEN),
            (8, 1),
            (8, TILE_PAR_MIN_LEN),
            (8, 512), // the largest value §14's min_len sweep tried
        ] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let got: Vec<Option<u32>> = pool
                .install(|| {
                    items
                        .par_iter()
                        .with_min_len(min_len)
                        .map(make)
                        .collect::<Vec<_>>()
                })
                .into_iter()
                .map(|r| r.unwrap())
                .collect();
            assert_eq!(
                got, expected,
                "collect order diverged from input order at threads={threads} min_len={min_len}"
            );
        }
    }

    /// A row space over a deliberately non-identity row order, with its `row-entity.u32` attached —
    /// the shape both crossing routes read. An identity order would let a route that returned the
    /// row back as the entity pass.
    fn row_space_over(
        dir: &std::path::Path,
        row_order: &[u32],
    ) -> tessera_store::permutation::RowSpace {
        use tessera_store::permutation::{Permutation, RowSpace};
        use tessera_store::row_entity::{write_row_entity, RowToEntity, ROW_ENTITY_FILE};

        let perm_path = dir.join("permutation.bin");
        let entities: Vec<EntityId> = row_order.iter().map(|&e| EntityId::new(e as u64)).collect();
        tessera_store::write::write_permutation(&perm_path, &entities, row_order.len() as u64)
            .expect("permutation writes");
        let table_path = dir.join(ROW_ENTITY_FILE);
        write_row_entity(&table_path, row_order).expect("table writes");

        RowSpace::new(
            Arc::new(Permutation::load(&perm_path).expect("permutation loads")),
            row_order.len() as u32,
        )
        .with_row_entity(Arc::new(
            RowToEntity::load(&table_path).expect("table loads"),
        ))
    }

    /// **The claim the whole two-route design rests on**: over every range the request can ask
    /// about, testing the viewport's rows one at a time and projecting the whole result give the
    /// same set. The route is a latency choice and nothing else.
    ///
    /// Asserted against a domain with all three shapes a real viewport produces — a long run, a
    /// sliver, and a gap between them — and at a chunk size small enough that the parallel split
    /// genuinely happens, since a route that is correct only when it runs as one chunk is not
    /// correct.
    #[test]
    fn filter_routes_agree_over_the_domain() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 20,011 is coprime with the row count, so the order is a genuine shuffle rather than a
        // shift, and no row's entity is near it.
        let rows = 40_000u32;
        let row_order: Vec<u32> = (0..rows)
            .map(|r| (r as u64 * 20_011 % rows as u64) as u32)
            .collect();
        let space = row_space_over(dir.path(), &row_order);

        // Every seventh entity, plus a dense block — a result that is neither uniform nor one run.
        let mut entities = croaring::Bitmap::new();
        entities.add_many(&(0..rows).step_by(7).collect::<Vec<u32>>());
        entities.add_range(1_000u32..9_000);

        let domain = vec![0u32..12_345, 20_000..20_003, 30_000..40_000];
        let mut domain_rows = croaring::Bitmap::new();
        for range in &domain {
            domain_rows.add_range(range.clone());
        }

        // `rows_in_ranges` here is only the chunker's sizing hint; pass the real span so the split
        // is the one a viewport of this size would take.
        let per_tile = per_tile_crossing(&space, &entities, &domain, domain_rows.cardinality())
            .expect("a row space with a table can always invert");
        let projected = space.project(&entities);

        assert_eq!(
            per_tile,
            projected.and(&domain_rows),
            "the per-tile crossing and the projection disagree inside the domain"
        );
        // And the per-tile route claims nothing outside it — the property `FilterRows::Viewport`
        // exists to keep a consumer honest about.
        assert!(
            per_tile.andnot(&domain_rows).is_empty(),
            "the per-tile crossing returned rows it never tested"
        );
        assert!(
            !projected.andnot(&domain_rows).is_empty(),
            "the fixture is degenerate: every matching row is inside the domain, so the two routes \
             would agree even if the domain were ignored"
        );
    }

    /// **A short-circuited `none_of` must still consume its skipped kids' images.**
    ///
    /// `images` is positional: `RowExpr::entity_verdicts` collects every `Entity` node in the tree
    /// whether or not evaluation reaches it, and [`eval_row_expr`] walks the same pre-order with a
    /// cursor. `NoneOf` stops early once its difference is empty — nothing below can widen it —
    /// and leaving the cursor there hands the *next* `Entity` anywhere in the tree someone else's
    /// image. The tree below is the reachable shape: an empty combinator is entity-pure by
    /// construction, so `route` emits `RowExpr::Entity(candidate)` for it, and `check_negations`
    /// admits it inside a `none_of` because it contributes no column to the one-column rule.
    ///
    /// Without the cursor advance the union below answers with the **candidate** — a filter that
    /// silently matches every visible row — instead of with the second clause's verdict.
    #[test]
    fn a_short_circuited_negation_still_consumes_its_skipped_images() {
        use crate::filter::{Family, FilterOperand, RowExpr};

        let skipped_image = croaring::Bitmap::from_iter(0u32..1_000);
        let wanted_image = croaring::Bitmap::from_iter([7u32, 11, 13]);
        let images = vec![skipped_image.clone(), wanted_image.clone()];

        let tree = RowExpr::AnyOf(vec![
            RowExpr::NoneOf {
                column: "band".to_string(),
                family: Family::Category,
                // The leaf is evaluated and empties the difference; the `Entity` after it is
                // skipped, and its image is the first in `images`.
                kids: vec![
                    RowExpr::Leaf {
                        column: "band".to_string(),
                        family: Family::Category,
                        operand: FilterOperand::Equals(tessera_types::AttrLocalId::new(1)),
                    },
                    RowExpr::Entity(croaring::Bitmap::new()),
                ],
            },
            RowExpr::Entity(croaring::Bitmap::new()),
        ]);
        assert_eq!(
            tree.entity_verdicts().len(),
            images.len(),
            "the fixture must hand one image per Entity node, as the caller does"
        );

        // An empty domain, so every row scan is empty and the negation short-circuits on its
        // first kid — which is what makes the skipped `Entity` the one under test. The images are
        // already crossed against the domain by the caller, so they are unaffected.
        let mut next_image = 0usize;
        let scope = RowScope::Domain(croaring::Bitmap::new());
        let out = eval_row_expr(&tree, &images, &mut next_image, &[], &[], &scope)
            .expect("an empty domain scans cleanly");

        assert_eq!(
            out, wanted_image,
            "the union answered with the skipped kid's image instead of the second clause's"
        );
        assert_eq!(
            next_image,
            images.len(),
            "every image must be consumed, or a later Entity reads the wrong one"
        );
    }

    /// A view with no `row-entity.u32` declines the per-tile route rather than answering from a
    /// base it cannot invert. `entity_of` returning `None` on such a row means "ask another way",
    /// and reading it as "this row has no entity" would drop rows from a filtered viewport
    /// silently — so the route decision asks `can_invert` before committing, and the walk itself
    /// still bails if it ever meets one.
    #[test]
    fn a_row_space_without_a_table_declines_the_per_tile_route() {
        use tessera_store::permutation::{Permutation, RowSpace};

        let dir = tempfile::tempdir().expect("tempdir");
        let perm_path = dir.path().join("permutation.bin");
        let entities_in_order: Vec<EntityId> = [4u64, 2, 0, 5, 1, 3]
            .iter()
            .map(|&e| EntityId::new(e))
            .collect();
        tessera_store::write::write_permutation(&perm_path, &entities_in_order, 6)
            .expect("permutation writes");
        let space = RowSpace::new(Arc::new(Permutation::load(&perm_path).expect("loads")), 6);

        assert!(!space.can_invert());
        let mut entities = croaring::Bitmap::new();
        entities.add_many(&[0, 1, 2, 3, 4, 5]);
        let domain = vec![0u32..2, 4..6];
        assert!(
            per_tile_crossing(&space, &entities, &domain, 4).is_none(),
            "the walk must decline rather than return the rows it happened to resolve"
        );
    }

    /// The domain is the request's tile parts in view row space: shifted by each segment's
    /// `row_base`, sorted across segments, and merged where they touch. Merging is what makes the
    /// walk sequential and `FilterRows::covers` a single binary search; it must never widen.
    #[test]
    fn the_crossing_domain_shifts_by_row_base_and_merges_only_what_touches() {
        // Two segments: segment 0 based at row 0, segment 1 at row 1,000. Three tiles, the first
        // two adjacent within segment 0 and the third split across both.
        let ranges = vec![
            vec![(0usize, 0u32..10)],
            vec![(0usize, 10u32..25)],
            vec![(0usize, 40u32..50), (1usize, 0u32..5)],
        ];
        let domain = crossing_domain(&ranges, &[0, 1_000]);
        assert_eq!(
            domain,
            vec![0u32..25, 40..50, 1_000..1_005],
            "adjacent tiles merge, a gap survives, and segment 1's rows land at its row_base"
        );
        assert_eq!(
            domain.iter().map(|r| r.len()).sum::<usize>(),
            10 + 15 + 10 + 5,
            "merging changed how many rows the domain covers"
        );
    }

    /// The route decision, at its boundary. Strictly greater, so a result exactly at the ratio
    /// still projects — the exact-everywhere route wins ties.
    #[test]
    fn the_per_tile_route_is_taken_only_past_the_ratio() {
        let looks_cheaper = |matched: u64, viewport: u64| {
            matched > viewport.saturating_mul(PER_TILE_CROSSING_RATIO)
        };
        assert!(!looks_cheaper(300_000, 300_000), "1x projects");
        assert!(
            !looks_cheaper(900_000, 300_000),
            "exactly at the ratio projects"
        );
        assert!(looks_cheaper(900_001, 300_000), "just past it does not");
        // An empty viewport: the per-tile route walks nothing and is free, where projecting would
        // pay for the whole result to reach the same empty answer.
        assert!(looks_cheaper(1, 0));
        assert!(
            !looks_cheaper(0, 0),
            "nothing matched -- either route is empty"
        );
    }

    /// Every row of one run, matched and narrowed to the rows that carry a value.
    fn run(
        slice: &HotSlice<'_>,
        predicate: &RowPredicate<'_>,
        present: Option<&croaring::Bitmap>,
    ) -> Vec<u32> {
        let rows_in_slice = match slice {
            HotSlice::Bool(a) => a.len(),
            HotSlice::I32(v) => v.len(),
            HotSlice::U8(v) => v.len(),
            HotSlice::F64(v) => v.len(),
            HotSlice::I64(v) | HotSlice::TimestampUs(v) => v.len(),
            _ => unreachable!("the fixtures below use these widths"),
        } as u32;
        let mut rows = croaring::Bitmap::new();
        let mut buf = Vec::with_capacity(1024);
        scan_run(
            slice,
            0,
            0..rows_in_slice,
            predicate,
            present,
            &mut rows,
            &mut buf,
        );
        assert!(buf.is_empty(), "a run must leave its buffer empty");
        rows.iter().collect()
    }

    /// The rows that carry a value, as [`present_rows`] hands them over — `None` is every row.
    fn presence(absent: &[u32], rows: u32) -> croaring::Bitmap {
        let mut present = croaring::Bitmap::new();
        present.add_range(0..rows);
        for row in absent {
            present.remove(*row);
        }
        present
    }

    /// **A row with no number matches no range — including one containing zero, and including an
    /// unbounded one.**
    ///
    /// This is the 2026-08-11 defect on the row route. The hot column is non-nullable, so an absent
    /// number is written as the type's zero and is indistinguishable *in the column* from a real
    /// zero; a range containing zero then matches every row that never had a value. Decision 0064
    /// puts absence in a bitmap beside the column, and this is the scan honouring it.
    ///
    /// The fixture is built so that a scan ignoring presence passes no assertion by luck: rows 1
    /// and 3 carry no value and hold the stored zero, row 4 carries a genuine zero, and the range
    /// straddles zero. Against `[1, 10]` the honouring and the ignoring scan would agree.
    #[test]
    fn an_absent_number_matches_no_range_not_even_one_containing_zero() {
        // rows:      0    1*   2    3*   4    5     (* = no value, stored as the type's zero)
        let values = [7i32, 0, -3, 0, 0, 40];
        let slice = HotSlice::I32(&values);
        let present = presence(&[1, 3], 6);

        let straddling_zero = RowPredicate::Range {
            lo: Some(Endpoint {
                value: Scalar::Int(-10),
                inclusive: true,
            }),
            hi: Some(Endpoint {
                value: Scalar::Int(10),
                inclusive: true,
            }),
        };
        assert_eq!(
            run(&slice, &straddling_zero, Some(&present)),
            vec![0, 2, 4],
            "a row with no number matched a range containing zero"
        );

        // The other half of the same rule: a genuine zero must survive it. An over-eager presence
        // rule that dropped the value with the absence would pass the assertion above.
        let zero_only = RowPredicate::Range {
            lo: Some(Endpoint {
                value: Scalar::Int(0),
                inclusive: true,
            }),
            hi: Some(Endpoint {
                value: Scalar::Int(0),
                inclusive: true,
            }),
        };
        assert_eq!(
            run(&slice, &zero_only, Some(&present)),
            vec![4],
            "a real zero stopped matching"
        );

        // An unbounded range is "carries a value", not "every row" — the same reading the entity
        // route gives it, and the one an absent row must still fail.
        assert_eq!(
            run(
                &slice,
                &RowPredicate::Range { lo: None, hi: None },
                Some(&present)
            ),
            vec![0, 2, 4, 5]
        );
        // `eq` over a list is the same rule: a needle of zero names the genuine zero only.
        assert_eq!(
            run(
                &slice,
                &RowPredicate::NumberIn(&[Scalar::Int(0), Scalar::Int(40)]),
                Some(&present)
            ),
            vec![4, 5]
        );
        // And the presence half of a negation reads the bitmap alone: the column's bytes say
        // nothing about absence for this family.
        assert_eq!(
            run(&slice, &RowPredicate::ValuePresent, Some(&present)),
            vec![0, 2, 4, 5]
        );
    }

    /// **A category reads absence from its own code 0 and has no bitmap at all** (decision 0064 —
    /// its vocabulary reserves the code before any data exists, so a second mechanism would be the
    /// muddle that decision declines). The scan asks for no presence on this family, so the same
    /// run answers the same rows however the bitmap would have read.
    #[test]
    fn a_category_reads_absence_from_its_sentinel_and_asks_for_no_bitmap() {
        let codes = [1u8, 0, 2, 0, 1, 3];
        let slice = HotSlice::U8(&codes);
        assert!(!RowPredicate::CodeIn(&[1]).reads_presence());
        assert!(!RowPredicate::CodePresent.reads_presence());

        assert_eq!(run(&slice, &RowPredicate::CodeIn(&[1]), None), vec![0, 4]);
        assert_eq!(
            run(&slice, &RowPredicate::CodeIn(&[1, 2]), None),
            vec![0, 2, 4]
        );
        assert_eq!(
            run(&slice, &RowPredicate::CodeIn(&[0]), None),
            Vec::<u32>::new(),
            "the absent sentinel names no row, even asked for by code"
        );
        assert_eq!(
            run(&slice, &RowPredicate::CodePresent, None),
            vec![0, 2, 4, 5]
        );
    }

    /// The bounds are the entity route's, endpoint for endpoint: exclusivity folded by one step
    /// over integers, a bound past the type's ceiling excluding everything and one past its floor
    /// constraining nothing, a NaN bound satisfying nothing, and a fractional bound rounding *into*
    /// the constraint. These are the rules that are wrong in silence if the two copies drift.
    #[test]
    fn a_range_over_the_hot_column_reads_its_endpoints_as_the_entity_route_does() {
        let values = [0u8, 1, 2, 254, 255];
        let slice = HotSlice::U8(&values);
        let at = |v: i128, inclusive: bool| {
            Some(Endpoint {
                value: Scalar::Int(v),
                inclusive,
            })
        };
        let range = |lo, hi| RowPredicate::Range { lo, hi };

        assert_eq!(
            run(&slice, &range(at(1, true), at(2, true)), None),
            vec![1, 2]
        );
        assert_eq!(
            run(&slice, &range(at(0, false), at(254, false)), None),
            vec![1, 2],
            "an exclusive integer bound is the next value along"
        );
        // Beyond the type in either direction, which is where a wrapped comparison would show.
        assert_eq!(
            run(&slice, &range(at(-5, true), None), None),
            vec![0, 1, 2, 3, 4],
            "a bound below the floor constrains nothing"
        );
        assert_eq!(
            run(&slice, &range(at(300, true), None), None),
            Vec::<u32>::new(),
            "a bound above the ceiling excludes everything"
        );
        assert_eq!(
            run(&slice, &range(None, at(-1, true)), None),
            Vec::<u32>::new()
        );
        assert_eq!(
            run(&slice, &range(at(255, false), None), None),
            Vec::<u32>::new(),
            "`> 255` over a u8 is nothing, not everything wrapped"
        );

        // A fractional bound rounds into the constraint, on both sides.
        let fractional = |v: f64, inclusive: bool| {
            Some(Endpoint {
                value: Scalar::Float(v),
                inclusive,
            })
        };
        assert_eq!(
            run(
                &slice,
                &range(fractional(0.5, true), fractional(2.5, true)),
                None
            ),
            vec![1, 2]
        );

        // NaN is unordered: it satisfies nothing as a bound, and matches nothing as a value.
        assert_eq!(
            run(&slice, &range(fractional(f64::NAN, true), None), None),
            Vec::<u32>::new()
        );
        let floats = [1.0f64, f64::NAN, 3.0];
        assert_eq!(
            run(&HotSlice::F64(&floats), &range(None, None), None),
            vec![0, 1, 2],
            "an unbounded range asks only that the row carry a value"
        );
        assert_eq!(
            run(
                &HotSlice::F64(&floats),
                &range(at(0, true), at(4, true)),
                None
            ),
            vec![0, 2],
            "NaN is outside every bounded range"
        );
        assert_eq!(
            run(
                &HotSlice::F64(&floats),
                &RowPredicate::NumberIn(&[Scalar::Float(f64::NAN)]),
                None
            ),
            Vec::<u32>::new(),
            "NaN equals nothing, itself included"
        );
    }

    /// A bool and a datetime are read as the entity route stores them — `u8::from` for the one,
    /// microseconds as an `i64` for the other — so a predicate means the same thing on both routes.
    #[test]
    fn a_bool_and_a_datetime_compare_as_their_entity_space_storage_does() {
        let flags = arrow::array::BooleanArray::from(vec![true, false, true, false]);
        let slice = HotSlice::Bool(&flags);
        let (yes, no) = ([Scalar::Int(1)], [Scalar::Int(0)]);
        assert_eq!(run(&slice, &RowPredicate::NumberIn(&yes), None), vec![0, 2]);
        assert_eq!(run(&slice, &RowPredicate::NumberIn(&no), None), vec![1, 3]);
        // Absence for a bool is the bitmap too: `false` is a value, not a missing one.
        assert_eq!(
            run(
                &slice,
                &RowPredicate::NumberIn(&no),
                Some(&presence(&[3], 4))
            ),
            vec![1],
            "a bool with no value matched `false`"
        );

        let micros = [1_000i64, 2_000, 3_000];
        assert_eq!(
            run(
                &HotSlice::TimestampUs(&micros),
                &RowPredicate::Range {
                    lo: Some(Endpoint {
                        value: Scalar::Int(2_000),
                        inclusive: true,
                    }),
                    hi: None,
                },
                None
            ),
            vec![1, 2]
        );
    }
}
