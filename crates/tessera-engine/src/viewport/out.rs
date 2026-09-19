//! What a viewport answers with: its response types, its column buffers and its sink.

use super::*;

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
    /// **Of this tile's `matched`, how many also satisfy the request's `highlight`**
    /// (`highlight-and-hierarchy.md` §2) — the fifth column, always present, and equal to
    /// `matched` where the request carried no highlight.
    ///
    /// **Always present rather than optional**, because an absent highlight is the identity for
    /// this quantity: `highlighted = matched` says exactly what a missing column would, costs
    /// eight bytes a tile, and leaves the wire with one schema instead of two.
    ///
    /// `highlighted ≤ matched ≤ visible` holds by construction — each is the count of a subset of
    /// the last — which is what makes the wash the client draws from it comparable, tile to tile,
    /// with the number beside it.
    pub highlighted: u64,
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
    /// One bit per point: whether it satisfies the request's `highlight`
    /// (`highlight-and-hierarchy.md` §2). `None` where the request carried none, which is an
    /// **absent column** on the wire rather than an all-false one — a `false` would answer a
    /// question nobody asked.
    ///
    /// Parallel to the three buffers above when present. It sits after the render scalars and
    /// before the membership columns, so a decoder indexing scalars positionally is unaffected.
    pub highlighted: Option<Vec<bool>>,
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
        // Seeded from the request rather than from the first chunk, exactly as the scalars are, so
        // presence is decided once for the response and a chunk cannot introduce or drop it.
        if let (Some(dst), Some(src)) = (self.highlighted.as_mut(), other.highlighted) {
            dst.extend(src);
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
        // A bit per point, in Arrow's packed boolean buffer.
        if self.highlighted.is_some() {
            bytes += self.tessera_ids.len().div_ceil(8);
        }
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
pub(super) use flat_families;

impl ColumnBuf {
    /// An empty buffer of the declared type.
    ///
    /// **Typed from the manifest's declaration, never from the first value seen.** Deriving the
    /// column set from the first gathered point is how a request whose first tile came from a
    /// narrower segment silently drops a column, or shifts every later one left; the declaration
    /// is the same for every tile by construction.
    pub(super) fn empty(ty: ScalarType) -> Self {
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

/// An artifact's two filter answers — `(matched, highlighted)`, each `None` where the request
/// asked no such question. They are named as a pair because a dependent inherits both or neither
/// (`highlight-and-hierarchy.md` §2; decision 0104's argument for the first).
pub(super) type FilterBits = (Option<bool>, Option<bool>);

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
    /// This artifact's parents, **and only ever those that are also in this response**, ascending
    /// by identifier (`dag-hierarchies.md` §7). A tree's list is at most one long; a `dag`
    /// layer's may name several.
    ///
    /// The structure a client needs to nest what it draws, or to filter to one subtree while still
    /// drawing the rest of the map. **An absent entry covers two situations on purpose**: a root,
    /// and a parent that exists but was withheld from this viewer (C29, per entry). Distinguishing
    /// them would disclose that a coarser grouping exists which they are not cleared to see.
    pub parent_ids: Vec<TesseraId>,
    /// **The artifact this one is attached to, named by the identifier this same response served
    /// it under** — a label's cluster (owner ruling, 2026-09-18). `None` where the artifact is
    /// attached to nothing, and `None` on the identifier route, whose response is one artifact
    /// and so holds no row for a target to name.
    ///
    /// **It always names a row in the same response.** A dependent whose target this response
    /// does not hold is dropped entire before this is filled ([decision 0089] and
    /// `orphaned_dependents`), so this field never reaches past the served set — the rule
    /// [`Self::parent_ids`] follows, for the same reason. It is a `tessera_id` and never an entity
    /// id or a stored address (**I10**).
    ///
    /// [decision 0089]: ../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md
    pub target: Option<TesseraId>,
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
    /// skip levels and leave roots parentless, so counting `parent_ids` links disagrees with the
    /// declaration on every layer whose data is not a perfect ladder.
    ///
    /// On a **treed** layer — which declares no levels and sits entirely at level 0, its
    /// structure in its edges ([decision 0082](../../../docs/decisions/0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md))
    /// — it is the **response-local depth**: the longest parent chain to this row in the forest
    /// the response's own `parent_ids` links form (`dag-hierarchies.md` §5), *after* the budget
    /// cut and every other narrowing, so the root of a re-rooted subtree reads 0. That is the
    /// number walking the served parents yields, computed server-side so no client has to know
    /// which layer kind wants which derivation (the shipped client picked wrongly once).
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
    /// **A dependent artifact carries its target's** ([decision 0104](../../../docs/decisions/0104-a-filter-answers-a-boolean-per-served-artifact.md)):
    /// a label describes its cluster, and its own membership is a slice of that cluster at best.
    pub matched: Option<bool>,
    /// **The same bit for `all_of[filters, highlight]`** (`highlight-and-hierarchy.md` §2):
    /// whether a member this principal may see, inside the request's tiles, satisfies **both**
    /// expressions — `None` where the request carried no `highlight`, which is *there was no
    /// question* rather than *no matches*.
    ///
    /// Every rule [`Self::matched`] carries holds here unchanged, because this is that bit under a
    /// second expression and not a second kind of answer: a boolean and never a count, clipped to
    /// the viewport where the count is not, and the only other filter-dependent field on the row.
    pub highlighted: Option<bool>,
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
    /// Whether the *points* frame carries a `highlighted` column — i.e. whether the request
    /// carried a `highlight` (`highlight-and-hierarchy.md` §2). Settled here, before the first
    /// byte, for the reason [`Self::render_scalars`] is: the frame's schema is fixed for the
    /// response and a chunk may not introduce or drop a column.
    pub highlighted: bool,
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

/// The columns every points chunk of one response carries: the render columns the head published,
/// and the membership resolver where the artifacts frame carried anything to resolve against.
///
/// One value, because the two are decided together — `point_rows = "highlight"` empties both — and
/// because a chunk seeded from one list and gathered against another would caption a column's
/// values with another column's name.
pub(super) struct PointSchema<'a> {
    pub(super) render_scalars: &'a [DeclaredScalar],
    pub(super) membership: Option<crate::membership_column::Resolved>,
}

/// The emit pass: gather and hand off, serial, in response order (this module's doc says
/// why serial). A chunk is delivered once its estimated wire size reaches `flush_bytes`, always at
/// a whole-tile boundary.
pub(super) fn emit_points(
    swept: &[TileSweepOut<'_>],
    schema: &PointSchema<'_>,
    mask: &EffectiveMask,
    flush_bytes: usize,
    cancel: &Option<CancelToken>,
    probe: &mut Probe,
    sink: &mut dyn ViewportSink,
) -> Result<()> {
    // The buffer is seeded from the declaration rather than from whichever tile arrives first — a
    // request whose first tile is narrower than a later one must not fix the column set from it —
    // and re-seeded identically at each flush.
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
        // D-C: the emit pass's per-tile checkpoint — one atomic read, so an abandoned
        // stream stops gathering within one tile even when the sink is not refusing yet.
        check_cancelled(cancel)?;
        let mut stats = TileProbe::new();
        let parts = SelectionParts::new(&ts.parts);
        let mut tile_points = gather_tile_columns(&parts, &ts.rows, schema.render_scalars)?;
        if let Some(membership) = &schema.membership {
            tile_points.membership = membership.columns_for(&ts.rows);
        }
        // One `contains` per served point against the crossed highlight set — at most
        // `k_max_marks` lookups for the whole response (`highlight-and-hierarchy.md` §2.1).
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
                // Unreachable: every chunk of one response is gathered against the same
                // declared-scalar list, and a per-chunk type disagreement is refused inside the
                // gather (`gather_tile_columns`) before it could reach here.
                .expect("chunks of one response cannot disagree on a column's type"),
        }
        Ok(())
    }
}
