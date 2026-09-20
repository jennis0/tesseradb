//! What a caller sends: the viewport request and the selections it carries.

use super::*;

/// Which annotation layers a viewport answers for. Empty means none; there is no value meaning
/// "every layer" by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerSelection<'a> {
    /// Every layer this principal reaches.
    All,
    /// These, intersected with what the principal reaches — never unioned. Empty is none.
    Named(&'a [&'a str]),
}

/// Which of a levelled layer's declared resolutions a viewport answers for. The default is the
/// declaration's own zoom range per level, unlike [`LayerSelection`]'s default of none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelSelection<'a> {
    /// The levels whose declared zoom range contains the request's depth. A level with no
    /// declared range is served at every depth.
    Declared,
    /// Every level the layer holds, whatever the depth asked at.
    All,
    /// Exactly these, intersected with what the layer holds — never unioned. A level the layer
    /// does not hold is simply absent from the answer. Empty means none, as `layers: []` does;
    /// the absent request field is [`LevelSelection::Declared`], not this.
    Named(&'a [u32]),
}

/// Which of a layer's declared computed properties a viewport answers for. Narrows the
/// declaration and never widens it. Without it a client drawing one artifact's hull is served
/// every artifact's hull — measured at 94% of a `k = 0` artifacts request's cost on a 2.42M-row
/// corpus.
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

/// Which columns of the artifacts frame a viewport answers with. The row set, the `matched`
/// bits and the `rung` values are identical under either value; [`ArtifactRows::Identity`] skips
/// payload production only, never candidacy, the verdict or the filter probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArtifactRows {
    /// Every column — the default, and the answer a caller who has read nothing receives.
    #[default]
    Full,
    /// `layer`, `tessera_id`, `rung`, `matched` — for the caller that already holds the payload
    /// columns and wants this filter's bits over the same rows.
    Identity,
}

/// Which columns each served point answers with, mirroring [`ArtifactRows`]. The row set and the
/// `served` split are identical under either value, because the served set never depends on the
/// highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PointRows {
    /// Every column — the default, and the answer a caller who has read nothing receives.
    #[default]
    Full,
    /// `tessera_id` and `highlighted` alone. Without a `highlight` on the request there is
    /// nothing to project to, so this answers as [`PointRows::Full`] does.
    Highlight,
}

/// One `/v1/viewport` request, as the engine sees it.
///
/// A struct rather than a positional argument list, so the signature stays readable as
/// parameters accumulate. Construct with [`ViewportRequest::new`] and add the optional parts.
#[derive(Debug, Clone)]
pub struct ViewportRequest<'a> {
    /// A view id from `GET /v1/meta`.
    pub view: &'a str,
    /// Tile depth, 0–16.
    pub zoom: u8,
    /// `[x0, y0, x1, y1]` in the bundle's declared extent. Ignored when `tiles` is present.
    pub bbox: [f64; 4],
    /// The exact tiles to answer for, as depth-`zoom` Morton prefixes, in place of deriving them
    /// from `bbox`. A tile a client already holds is simply absent from the list, so the engine
    /// does no range derivation, counting, selection or gather for it. The list's order is the
    /// response's order.
    pub tiles: Option<&'a [u64]>,
    /// The client's per-tile mark budget. Clamped to `max_k` and then to `k_max_marks`. Must be
    /// non-decreasing as the client zooms in, or marks pop out of view.
    pub k: usize,
    /// The stamp of the response the client is currently holding, echoed back. Advisory: the
    /// request is always answered from live geometry; it only sets [`ViewportOut::stale`] when
    /// the live geometry has moved since.
    pub stamp: Option<GenerationStamp>,
    /// Request underlay sub-cell counts at depth `zoom + offset`. `None` or `Some(0)` serves
    /// none and costs nothing.
    pub underlay_offset: Option<u8>,
    /// Cooperative cancellation, checked once per tile and before each long serial-prefix stage;
    /// see [`Engine::viewport`]'s doc for the checkpoints. Never threaded into the single-flight
    /// geometry builders: a build already in flight runs to completion regardless.
    pub cancel: Option<CancelToken>,
    /// The request's filter expression, or `None` for an unfiltered request. Applied above the
    /// mask, never folded into it: it narrows which marks are drawn but never moves the
    /// selection threshold, which stays anchored on the unfiltered composed total. Otherwise
    /// artifacts would appear and vanish, and density would shift, as a viewer types.
    pub filter: Option<crate::filter::FilterExpr>,
    /// Which annotation layers to answer for. `None` answers for every layer this principal
    /// reaches; an empty slice answers for none. Narrows and never widens: a layer name this
    /// principal does not reach is intersected out of the answer, the same as a name nobody
    /// registered, so naming a layer is not a way to learn whether it exists.
    ///
    /// [`ViewportRequest::new`] starts at [`LayerSelection::All`]. The wire's default is the
    /// opposite: an omitted `layers` on `/v1/viewport` means none, and `"all"` means every layer.
    pub layers: LayerSelection<'a>,
    /// The client's artifact budget: how many artifacts it wants back at most. Honoured
    /// structurally, never by sampling — a budget that cannot be met by serving everything is
    /// met by serving ancestors instead of their descendants. Not a disclosure control: every
    /// artifact here already passed its own existence test.
    pub artifact_budget: Option<u32>,
    /// Which of each named layer's levels to answer for. See [`LevelSelection`]. A request
    /// bound, not a control: every artifact a level holds already passed its own existence
    /// criterion.
    pub levels: LevelSelection<'a>,
    /// Which of each layer's declared computed properties to answer for. See
    /// [`ComputedSelection`].
    ///
    /// [`ViewportRequest::new`] starts at [`ComputedSelection::Declared`], the wire's
    /// absent-field meaning.
    pub computed: ComputedSelection<'a>,
    /// Which columns each served artifact answers with — see [`ArtifactRows`]. The row set is
    /// identical under either value; [`ArtifactRows::Identity`] skips payload production only.
    pub artifact_rows: ArtifactRows,
    /// The request's highlight expression, in the same grammar as [`Self::filter`]. Never
    /// changes which rows the response holds, and is evaluated against the same pre-filter mask
    /// as the filter. Adds three conjunctions with the filter's candidate: a count per tile, a
    /// bit per served point, a bit per served artifact.
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

    /// Attach a cooperative-cancellation token. See [`Self::cancel`]'s field doc for the
    /// checkpoints and the single-flight-builder exemption.
    pub fn cancel(mut self, cancel: Option<CancelToken>) -> Self {
        self.cancel = cancel;
        self
    }
}
