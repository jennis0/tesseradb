//! What a caller sends: the viewport request and the selections it carries.

use super::*;

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

/// Which columns each **served point** answers with (`highlight-and-hierarchy.md` §2), mirroring
/// [`ArtifactRows`] exactly.
///
/// **The row set and the `served` split are identical under either value; only the columns
/// change.** That sentence is the contract, and it is what makes the projection disclose nothing:
/// a highlight answer is a column subset of what the same caller's identical request would have
/// been served, because the served set does not depend on the highlight at all. A client that
/// changes only its highlight already holds every point it needs and wants only the bits, which
/// join its held points by `tessera_id`.
///
/// **Bound to a generation.** The served set is deterministic within one, and a stamp move
/// (`x-tessera-stale`) means the held set may no longer be what the same request serves — the
/// client re-asks with [`PointRows::Full`], exactly as it does for `artifact_rows`. A client
/// asking for this while holding nothing meets identifiers it cannot draw, knows it, and re-asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PointRows {
    /// Every column — the default, and the answer a caller who has read nothing receives.
    #[default]
    Full,
    /// `tessera_id` and `highlighted` alone. **Without a `highlight` on the request there is
    /// nothing to project to**, so this answers as [`PointRows::Full`] does rather than serving a
    /// column of nulls.
    Highlight,
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
    /// The request's **highlight** expression, in exactly [`Self::filter`]'s grammar
    /// (`highlight-and-hierarchy.md` §2).
    ///
    /// **It never changes which rows the response holds.** The cap clause, the density sampling
    /// and `served` all run over the `filters` candidate exactly as they do without it, so the set
    /// of points a viewer sees is the same with and without a highlight — the map does not move,
    /// the marks do not resample, and a point the viewer was looking at stays where it is with its
    /// brightness changed. What it adds is three answers, all conjunctions with the filter's
    /// candidate: a count per tile, a bit per served point, and a bit per served artifact.
    pub highlight: Option<crate::filter::FilterExpr>,
    /// Which columns each served point answers with — see [`PointRows`].
    pub point_rows: PointRows,
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
            highlight: None,
            point_rows: PointRows::Full,
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

    /// Attach a highlight expression. See [`ViewportRequest::highlight`].
    pub fn highlight(mut self, highlight: crate::filter::FilterExpr) -> Self {
        self.highlight = Some(highlight);
        self
    }

    /// Answer each point with these columns. See [`PointRows`].
    pub fn point_rows(mut self, rows: PointRows) -> Self {
        self.point_rows = rows;
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
