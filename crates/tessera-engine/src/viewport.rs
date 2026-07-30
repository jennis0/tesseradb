//! The masked viewport query (task-11 brief, design §2.6 retrieve steps 1–9).
//!
//! [`Engine::viewport`] loads the generation pointer exactly once, validates or mints the pin
//! (I11: geometry identity only, `(prefix, segments_version)` — never `overlay_version`, so an
//! overlay swap never invalidates an outstanding pin), gets-or-builds the session's cached row
//! projection, composes the effective mask (I1), and for every tile touching `bbox` counts and
//! selects.
//!
//! Selection is §7.2's real definition — floor ∪ threshold ∪ cap over `tessera_id`, evaluated
//! inside the mask (I7). It lives in [`crate::select`], which carries the definition, the nesting
//! argument and the two evaluation routes. This module's job is only to resolve the per-request
//! parameters (notably θ's anchor, which **must** be the composed visible cardinality — see
//! [`crate::select::Threshold::anchor`] for the I2 argument) and to gather what selection returns.
//!
//! **D-D/D-F: the per-tile body of the count-and-select loop runs on the engine's shared rayon
//! pool.** [`tile_result`] is the pure per-tile function — no `&self`, no engine method, nothing
//! but `&`-borrowed inputs and an owned result — that [`Engine::viewport`] fans out over every
//! tile via `self.pool.install(|| tiles.par_iter().zip(..).map(tile_result).collect::<Vec<_>>())`.
//! The collect target is deliberately `Vec<Result<Option<TileResult>, EngineError>>`, never
//! `Result<Vec<TileResult>, EngineError>`: a `Result` collect drops rayon onto its unindexed
//! reduce path, and this response's byte-equality claim (same request, same bytes, at
//! `compute_threads = 1` or `8`) would then rest on an implementation detail of that reduce
//! strategy rather than on anything stated here. Collecting `Vec<Result<..>>` stays on rayon's
//! *indexed* collect path, so the output vector's order equals the input tiles' order **by
//! construction** — not by convention, not by observation of the current rayon version. A serial,
//! in-order fold over that vector (still in `Engine::viewport`) then short-circuits on the first
//! `Err` (D-C's per-tile cancellation check, moved inside `tile_result` — see its doc) and
//! concatenates `tile_counts`/`points`/`sub_cells` exactly as the pre-Task-6 serial loop did.

use std::ops::Range;
use std::sync::Arc;

use rayon::prelude::*;

use tessera_spatial::{tiles_for_bbox, tiles_for_bbox_count, Extent, Tile};
use tessera_store::manifest::{DeclaredScalar, Quantisation};
use tessera_store::read::{ScalarSlice, SegmentData};
use tessera_store::StoreError;
use tessera_store::{tile_ranges_all, tile_ranges_within};
use tessera_types::{EntityId, PinId, TesseraId, API_VERSION};

use crate::cancel::CancelToken;
use crate::compose::{compose, visible_to, EffectiveMask, RowProjection};
use crate::select::{SelectParams, Selection, Threshold};
use crate::session::{Engine, EngineError, Result, Session};
use crate::timing::{Probe, StageTimings, TileProbe, TileStats};
use crate::Generation;

/// One declared-scalar value carried alongside a point (mirrors `tessera_spatial::ScalarValue`'s
/// three Phase 1 kinds, but on the *output* side — read from `ColumnsRef`, not staged for write).
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarOut {
    U64(u64),
    F32(f32),
    Utf8(String),
}

/// One tile's count row: `matched == visible` always in Phase 1 (no filters yet — Reference
/// Sheet R5).
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

/// One sampled point.
///
/// **I10, strengthened (contracts r6):** no entity ID leaves the engine on this path, because
/// none is stored. `columns.arrow` carries `tessera_id` at the row, so the gather reads the
/// identity it is allowed to show and cannot read the one it is not. Entity IDs survive only in
/// entity-space structures and as `permutation.bin`'s index — never as a value on any path
/// reaching `tessera-wire`.
#[derive(Debug, Clone, PartialEq)]
pub struct PointOut {
    pub tessera_id: TesseraId,
    pub x: f32,
    pub y: f32,
    pub scalars: Vec<ScalarOut>,
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

/// One `/v1/viewport` request, as the engine sees it.
///
/// A struct rather than a positional argument list: the query is the system's main entry point and
/// keeps acquiring parameters (`served`, the §3.3 underlay, and the §8.2 filter contract next), so
/// naming them at the call site keeps both the signature and every caller readable as it grows.
/// Construct with [`ViewportRequest::new`] and add the optional parts.
#[derive(Debug, Clone)]
pub struct ViewportRequest<'a> {
    /// A slice id from `GET /v1/meta`.
    pub slice: &'a str,
    /// Tile depth, 0–16.
    pub zoom: u8,
    /// `[x0, y0, x1, y1]` in the bundle's declared extent.
    pub bbox: [f64; 4],
    /// The client's per-tile mark budget. Clamped to `max_k` (the machine ceiling) and then to
    /// `k_max_marks` (§7.2's cap clause).
    ///
    /// **Must be non-decreasing as the client zooms in.** §7.2's nesting property holds for a fixed
    /// cap; lowering `k` on descent forfeits it and marks will pop out. The engine sees one request
    /// at a time and cannot enforce this — see [`crate::select`]'s module doc.
    pub k: usize,
    /// Re-pin geometry to a prior response's `(prefix, segments_version)` (I11).
    pub pin: Option<PinId>,
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
}

impl<'a> ViewportRequest<'a> {
    /// The required parameters; `pin` and `underlay_offset` default to absent.
    pub fn new(slice: &'a str, zoom: u8, bbox: [f64; 4], k: usize) -> Self {
        ViewportRequest {
            slice,
            zoom,
            bbox,
            k,
            pin: None,
            underlay_offset: None,
            cancel: None,
        }
    }

    pub fn pin(mut self, pin: Option<PinId>) -> Self {
        self.pin = pin;
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

/// The masked viewport response. No `serde` derive (I10) — see [`PointOut`]'s doc.
#[derive(Debug, Clone)]
pub struct ViewportOut {
    pub pin: PinId,
    pub tiles: Vec<TileCount>,
    pub points: Vec<PointOut>,
    /// The §3.3 density underlay, when requested — empty otherwise. Only non-empty cells appear.
    pub sub_cells: Vec<SubCellCount>,
    /// The declared-scalar names, in manifest order, from the SAME generation this response's
    /// points were gathered from (Task 8). Carried here rather than left for the caller to
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
/// Two responses carrying the same pin, tiles, points and scalar names *are* the same response;
/// the wall-clock it took to produce them is not part of that identity. A derived impl would make
/// every `assert_eq!` over a whole `ViewportOut` in the test suite timing-dependent, and
/// therefore flaky the moment `bench-timing` is enabled — which is exactly when those tests
/// matter most.
///
/// `scalar_names` joins the comparison (Task 8): it is drawn from the same manifest as `points`'
/// values, in the same generation, so two responses that agree on `points` already agree on it —
/// including it costs nothing and is more honest than silently exempting a field that happens
/// never to differ in practice.
impl PartialEq for ViewportOut {
    fn eq(&self, other: &Self) -> bool {
        self.pin == other.pin
            && self.tiles == other.tiles
            && self.points == other.points
            && self.sub_cells == other.sub_cells
            && self.scalar_names == other.scalar_names
    }
}

/// `POST /v1/items/{handle}`'s payload (R5): a visible item's scalars plus its caller-supplied
/// external id, if it has one.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemOut {
    pub scalars: Vec<ScalarOut>,
    pub external_id: Option<Vec<u8>>,
}

/// `GET /v1/meta`'s payload (R5) — the bundle-level facts a viewer client needs before it can
/// issue a sensible `/v1/viewport` call.
#[derive(Debug, Clone)]
pub struct EngineMeta {
    pub api_version: u32,
    pub bundle_format: u32,
    /// `(id, display_name)` pairs, in manifest order.
    pub slices: Vec<(String, String)>,
    pub quantisation: Quantisation,
    pub declared_scalars: Vec<DeclaredScalar>,
    /// The transport-identity epoch (contracts §2.2/§2.6 r6). `GET /v1/meta` reports this
    /// verbatim as `identity_epoch`; `POST /v1/items/{tessera_id}` compares an optional
    /// caller-supplied `epoch` against it. Never the identity **key** — that never leaves the
    /// server, on any plane (design Appendix C, C17; I10).
    pub identity_epoch: u32,
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
            slices: manifest
                .slices
                .iter()
                .map(|s| (s.id.clone(), s.display_name.clone()))
                .collect(),
            quantisation: manifest.quantisation,
            declared_scalars: manifest.declared_scalars.clone(),
            identity_epoch: manifest.identity.epoch,
        }
    }

    /// Is `entity` visible to `session` under `generation` — the ONE BIT `/v1/items` needs. An
    /// **entity-space** question (see `crate::compose::visible_to`'s doc): three constant-time
    /// probes, no `RowProjection` constructed or consulted, so this costs the same whether
    /// `entity` exists and is visible, exists and is not, or does not exist at all (Critical
    /// C-5, closed rather than narrowed).
    pub fn visible_to(&self, session: &Session, generation: &Generation, entity: EntityId) -> bool {
        visible_to(
            &session.fragment,
            &session.satisfied,
            &generation.overlay,
            &generation.buffer,
            entity,
        )
    }

    /// `POST /v1/items/{handle}` (R5): invert `id` to its entity, test visibility in entity
    /// space, and only then locate a row and read its scalars/external id.
    ///
    /// Returns `Ok(None)` both when `id` names nothing in this bundle and when it names an item
    /// the principal may not see — deliberately one outcome from one code path, so the server
    /// cannot differentiate what the engine does not tell it (owner ruling; contracts §3.2).
    ///
    /// **The timing channel is closed, not narrowed** (Critical C-5; design Appendix C, C4
    /// annotation). Inversion is a pure function taking no I/O. The visibility test that follows
    /// is an entity-space question — three constant-time probes — and is **the same three probes
    /// for an identifier that names nothing and one that names an invisible item**. No
    /// `RowProjection` is constructed or read, so there is no per-ID cost for an attacker to
    /// correlate against, warm or cold. A row is located only after the answer is already
    /// "visible", and the sidecar is read only after that.
    ///
    /// **Returns `Err` rather than a fail-open `None`** (Critical N-3). A digest mismatch, an
    /// out-of-order extent or a short locator is a `500`, never an item served with
    /// `external_id: null` — `.ok().flatten()` would discard exactly the typed errors Task 8
    /// exists to produce. This does not reopen C-5: the sidecar is touched only for an item
    /// already established as visible, so no attacker-drivable path can raise it.
    pub fn item(
        &self,
        session: &Session,
        id: TesseraId,
    ) -> std::result::Result<Option<ItemOut>, StoreError> {
        let generation = self.generation.load_full();
        let (shard, entity) = self.identity_key.invert(id);
        if shard != generation.bundle.manifest.identity.shard_id {
            return Ok(None);
        }

        // ONE BIT, in entity space, O(1), before anything is looked up in row space.
        if !self.visible_to(session, &generation, entity) {
            return Ok(None);
        }

        // Visible. Now — and only now — find the row, so the cost below is never reachable by
        // an identifier the principal may not see.
        let declared_scalars = &generation.bundle.manifest.declared_scalars;
        for partition in generation.bundle.partitions.values() {
            for slice_data in partition.slices.values() {
                // The permutation is the only entity→row bridge (I4, §5.1) — an O(1)
                // bounds-checked slot read, not a scan.
                let Some(row) = slice_data.permutation.row_of(entity) else {
                    continue;
                };
                // Phase 1 always has exactly one segment per (partition, slice) (R4 — the same
                // invariant `Engine::viewport`'s `MultiSegmentSlice` guard rests on); the
                // permutation addresses that single segment's row space.
                let Some(segment) = slice_data.segments.first() else {
                    continue;
                };
                return Ok(Some(ItemOut {
                    scalars: row_to_point(segment, row.raw(), declared_scalars).scalars,
                    external_id: self.external_id_of(entity)?, // N-3: propagate, never swallow
                }));
            }
        }
        // Visible in entity space but with no row anywhere: a buffered item awaiting flush. Same
        // `Ok(None)`, same 404 — it has no geometry to return.
        Ok(None)
    }
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
    /// The masked viewport query — see [`ViewportRequest`] for the parameters and for the
    /// non-decreasing-`k` obligation that §7.2's nesting property rests on.
    ///
    /// **D-C cancellation checkpoints** (cooperative, the rapid-pan case): once per tile, at the
    /// top of the tile loop below; once before [`compose`] runs; once before θ's anchor
    /// (`mask.visible_total()`). A hit at any of these aborts the WHOLE request with
    /// [`EngineError::Cancelled`] — no partial `ViewportOut` is ever returned (I13). The
    /// row-projection single-flight build (D-G, above the compose checkpoint) is deliberately
    /// NOT gated — see [`check_cancelled`]'s doc.
    pub fn viewport(&self, session: &Session, req: ViewportRequest<'_>) -> Result<ViewportOut> {
        let ViewportRequest {
            slice,
            zoom,
            bbox,
            k,
            pin,
            underlay_offset,
            cancel,
        } = req;
        // Load the generation pointer exactly once, at request start (see `GenerationHandle`'s
        // doc at its definition) — every subsequent read below (pin check, row-projection cache,
        // composition) comes from this one snapshot, so a concurrent overlay/bundle swap
        // mid-request can never mix state from two generations.
        let mut probe = Probe::new();

        let generation = self.generation.load_full();
        probe.lap(|t| &mut t.generation_resolve_ns);

        let effective_pin = match pin {
            Some(presented) => {
                // I11 / lifecycle §2.3: a pin is geometry identity only — `(prefix,
                // segments_version)` — never `overlay_version`. An overlay swap (any accepted
                // suppression/delete/predicate change) must not invalidate this pin; only a
                // bundle swap (new `prefix`/`segments_version`) does.
                if presented.prefix != generation.prefix
                    || presented.segments_version != generation.segments_version
                {
                    return Err(EngineError::PinExpired);
                }
                presented
            }
            None => PinId {
                prefix: generation.prefix.clone(),
                segments_version: generation.segments_version,
            },
        };

        probe.lap(|t| &mut t.pin_resolve_ns);

        let k = k.min(self.config.max_k);

        // Fail closed on a slice spanning partitions, for the same reason the segment guard below
        // exists: this resolves to ONE partition, and theta's anchor and every rank are then taken
        // over that partition alone — which §12.3 forbids (the anchor must be session-global, or
        // "below the cut" means different things in different partitions). Phase 1 emits one
        // partition, so this is unreachable; it is here so a §12 bundle cannot be served
        // half-masked with no error, which is the failure the multi-segment guard already refuses.
        let carriers = generation
            .bundle
            .partitions
            .values()
            .filter(|partition| partition.slices.contains_key(slice))
            .count();
        if carriers > 1 {
            return Err(EngineError::MultiPartitionSlice(slice.to_string()));
        }
        let slice_data = generation
            .bundle
            .partitions
            .values()
            .find_map(|partition| partition.slices.get(slice))
            .ok_or_else(|| EngineError::UnknownSlice(slice.to_string()))?;

        // Fail closed on more than one segment (see `EngineError::MultiSegmentSlice`'s doc):
        // `tile_ranges` returns segment-local row indices, but `mask` is built from the slice's
        // single `Permutation`, which addresses exactly one segment's row space. Phase 1's build
        // never produces more than one, so this is not reachable today — but silently iterating
        // "just in case" would mis-count/mis-index the moment it became reachable, which is worse
        // than refusing outright.
        if slice_data.segments.len() > 1 {
            return Err(EngineError::MultiSegmentSlice(slice.to_string()));
        }
        let segment = slice_data.segments.first();
        probe.lap(|t| &mut t.slice_lookup_ns);

        let cache_key = (
            session.token_id,
            slice.to_string(),
            generation.segments_version,
        );
        // D-G slot-state single-flight (F4, `tessera-bench/src/arms/load.rs:34-76`): the map
        // lock (`SingleFlightCache`) is held only for the O(1) `Building`/`Ready` transition —
        // never across the build below — so distinct sessions' first viewports no longer
        // serialise behind one global lock. Do NOT reintroduce that serialisation by narrowing
        // this back to "lock, check, build, insert, unlock"; the F4 memo names exactly that as
        // the anti-fix. A concurrent request racing the *same* key while this build is in flight
        // does not wait for it — it gets `EngineError::ProjectionBuilding` and retries.
        //
        // **Guardrail (D-D/D-F): nothing reachable from a rayon worker below may touch this
        // cache.** `base` is resolved once, here, on the calling thread, strictly before the
        // parallel tile sweep begins, and is then only *borrowed* (via `compose`'s `EffectiveMask`)
        // by every `tile_result` call — never re-fetched or re-built per tile. If a future change
        // ever did call `get_or_build` from inside a rayon worker, it would still be safe rather
        // than corrupting: `SingleFlightCache`'s state machine (this module's doc above) has no
        // notion of "friendly" re-entrancy, so a worker racing an in-flight build on the *same*
        // key would simply see `Slot::Building` and get back `EngineError::ProjectionBuilding`,
        // same as any other concurrent caller. That safety is incidental, not a licence — the
        // design intent is that this cache is touched once per request, from the serial prefix,
        // full stop.
        let base: Arc<RowProjection> = self
            .row_projection_cache
            .get_or_build(cache_key, || {
                // Crosses entity space into row space over the *whole* fragment
                // (`Permutation::project`'s cost note: seconds at 10⁹ rows) — paid once per
                // (token, slice, segments_version) and cached here, never recomputed on a
                // per-viewport path (shared-context constraint 8).
                probe.mark_projection_built();
                // Task 7: `Permutation::project` parallelises internally (ambient rayon,
                // `par_chunks`/`par_sort_unstable`) but owns no pool of its own — this is the
                // one call site that supplies one, the same shared pool `Engine::viewport`'s
                // tile sweep uses (D-D: no second, per-request pool). Wrapping only this build,
                // not the whole `get_or_build`, keeps the single-flight map lock's O(1) hold
                // time (D-G) unaffected by the pool boundary.
                self.pool
                    .install(|| RowProjection::new(&session.fragment, &slice_data.permutation))
            })
            .map_err(|_building| EngineError::ProjectionBuilding)?;
        probe.lap(|t| &mut t.row_projection_ns);

        // D-C checkpoint: before compose, one of the two long serial-prefix stages this task
        // guards. Placed after the (non-cancellable, D-G) row-projection build so a cancellation
        // observed here never interrupts that build — only work this request would otherwise go
        // on to do itself.
        check_cancelled(&cancel)?;

        let mask = compose(
            &session.fragment,
            &session.satisfied,
            &generation.overlay,
            &generation.buffer,
            base,
            &slice_data.permutation,
        );
        probe.lap(|t| &mut t.compose_ns);

        let q = &generation.bundle.manifest.quantisation;
        let extent = Extent {
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
        let tile_count = tiles_for_bbox_count(bbox, zoom, &extent);
        if tile_count > self.config.max_tiles_per_request as u64 {
            return Err(EngineError::TooManyTiles {
                demanded: tile_count,
                limit: self.config.max_tiles_per_request,
            });
        }
        let tiles = tiles_for_bbox(bbox, zoom, &extent);
        probe.lap(|t| &mut t.tiles_for_bbox_ns);
        probe.count(|t| &mut t.tiles_resolved, tiles.len() as u64);

        let declared_scalars = &generation.bundle.manifest.declared_scalars;

        // §3.3 underlay bounds, all three checked up front and all three *rejecting* rather than
        // clamping (see `EngineError::UnderlayRefused`). The cell budget is checked before any
        // counting work because the underlay multiplies the (already-bounded) tile set by 4^offset.
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
                Some(offset)
            }
        };

        // θ's anchor: the session's **composed** visible cardinality over this slice's whole row
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
        let mut points = Vec::new();
        let mut sub_cells = Vec::new();

        // Resolve every tile's row range in ONE monotone sweep rather than two full-column binary
        // searches per tile. A few hundred independent `log2(rows)` searches is where a sparse
        // request's time actually goes — measured at 26-64% of one
        // (`docs/design-memos/2026-07-30-f1-selection-overdraw.md`), and flat in density, because
        // the cost is the searching rather than the rows found.
        //
        // `tile_ranges_all` returns ranges positionally aligned with `tiles`, so the zip below
        // walks `tiles_for_bbox`'s raster order unchanged. That order is load-bearing (it is the
        // response's tile order, and the wire payload's points are a flat concatenation in it) —
        // the sweep's own Morton order stays inside `tile_ranges_all` and never reaches here.
        //
        // A slice with zero segments (an empty build) has nothing visible in any tile: `ranges` is
        // empty, the zip yields nothing, and the response is empty — as before.
        let ranges: Vec<Range<u32>> = match segment {
            Some(segment) => tile_ranges_all(segment, &tiles),
            None => Vec::new(),
        };
        probe.lap(|t| &mut t.tile_ranges_ns);

        // D-D/D-F: the parallel tile sweep, on the ONE shared pool this engine built at
        // `Engine::open` — no second, per-request pool, no nested throttling (D-D). Every input
        // below is borrowed or `Copy`: `mask`/`segment`/`declared_scalars`/`params` are the
        // generation- and request-derived values already resolved above (lifecycle §1.1 — nothing
        // is re-loaded per tile), and `cancel` is the D-C token, checked inside `tile_result` at
        // the very top (moved there from the old loop's first line — Task 5).
        //
        // `with_min_len(TILE_PAR_MIN_LEN)` — see that constant's doc for the number. Collecting
        // `Vec<Result<Option<TileResult>>>` (this crate's `Result<T>` alias for
        // `std::result::Result<T, EngineError>`) rather than `Result<Vec<TileResult>>` is
        // load-bearing for the byte-equality claim below — see this module's doc.
        let tile_outcomes: Vec<Result<Option<TileResult>>> = self.pool.install(|| {
            tiles
                .par_iter()
                .zip(ranges.into_par_iter())
                .with_min_len(TILE_PAR_MIN_LEN)
                .map(|(tile, range)| {
                    tile_result(
                        tile,
                        range,
                        &mask,
                        segment,
                        declared_scalars,
                        &params,
                        zoom,
                        underlay_offset,
                        &cancel,
                    )
                })
                .collect::<Vec<Result<Option<TileResult>>>>()
        });
        // D-E: the parallel section's own wall time is not a named stage — it is already fully
        // accounted for, per tile, inside each `TileResult::stats` (folded below) — so this resets
        // the clock without charging the stretch to whatever lap runs next, rather than leaving it
        // to be silently misattributed.
        probe.skip();

        // D-F: the serial, IN-ORDER fold. `tile_outcomes`' order equals `tiles`' order by
        // construction (the indexed collect path above — this module's doc), so this reconstructs
        // exactly the concatenation the pre-Task-6 serial loop produced. Short-circuits on the
        // first `Err` (D-C's `Cancelled`, or any other per-tile error): every tile's own work is
        // already done by this point (the parallel sweep does not itself short-circuit — that is
        // the point of collecting `Vec<Result<..>>` rather than `Result<Vec<..>>`), so bailing out
        // here costs only the remaining `Result`s' worth of `?`, never any recomputation.
        for outcome in tile_outcomes {
            let Some(tr) = outcome? else {
                continue;
            };
            tr.stats.fold_into(&mut probe.t);
            tile_counts.push(tr.count);
            points.extend(tr.points);
            sub_cells.extend(tr.sub_cells);
        }

        Ok(ViewportOut {
            pin: effective_pin,
            tiles: tile_counts,
            points,
            sub_cells,
            // Task 8: from the SAME `declared_scalars` slice `row_to_point` read for every point
            // above (`generation.bundle.manifest.declared_scalars`), not a fresh `meta()` call —
            // that would `load_full()` the generation pointer a second time.
            scalar_names: declared_scalars.iter().map(|d| d.name.clone()).collect(),
            timings: probe.finish(),
        })
    }
}

/// D-F's per-tile scheduling grain: the number of tiles rayon hands to one worker before it will
/// split the range again. A single-digit constant, deliberately not calibrated by its own probe
/// (Phase 0's probes covered corpus/mask shape, not this scheduling knob) but argued from the
/// shape of the workload: measured per-tile cost is highly non-uniform — an empty-tile skip
/// (`tile_result` returning `Ok(None)` after one `count_range`) is a handful of comparisons, while
/// a dense tile at a high cap is a bitmap-range read plus a bounded heap sort — so work-stealing
/// needs to be able to move *individual* tiles between workers rather than being locked into a few
/// large, coarse chunks; a chunk of, say, 64 tiles handed to one worker while the other workers'
/// chunks are all-empty would sit unstolen for the length of that chunk. `1` (rayon's own default
/// for `par_iter` without `with_min_len`) avoids that entirely but pays a scheduling/steal-queue
/// overhead on every single tile, including the very common empty-tile skip that is otherwise
/// nearly free. `4` is a conservative middle point: small enough that a viewport of a few hundred
/// tiles still splits into dozens of independently-stealable chunks (so an unlucky worker with an
/// all-empty run is never stuck for long), large enough to amortise the per-task overhead over the
/// cheap tiles that dominate a sparse or clustered corpus.
const TILE_PAR_MIN_LEN: usize = 4;

/// One tile's contribution to a `/v1/viewport` response (D-F) — the pure per-tile body pulled out
/// of what was, before this task, a serial `for` loop over `Engine::viewport`'s tiles. Safe to
/// call concurrently from any rayon worker: every parameter is `&`-borrowed or `Copy`, nothing
/// here reaches back into `Engine` or any state shared across tiles (see the guardrail comment at
/// the row-projection cache call site in `Engine::viewport`, above), and the return value is
/// owned outright by the caller — no shared mutable state, no interior mutability, nothing to
/// synchronise.
///
/// `Ok(None)` — an empty tile: no segment for this slice, or nothing visible in `range`. Exactly
/// the "skip empty" rule the old inline loop applied (no count row, no selection work). `Err`
/// carries [`EngineError::Cancelled`] from the D-C per-tile cancellation checkpoint below (moved
/// here, unchanged, from the top of the old loop body — Task 5) — checked first, so a flip
/// observed here costs only the one atomic read, never any of this tile's own
/// count/select/gather/underlay work.
#[allow(clippy::too_many_arguments)]
fn tile_result(
    tile: &Tile,
    range: Range<u32>,
    mask: &EffectiveMask,
    segment: Option<&SegmentData>,
    declared_scalars: &[DeclaredScalar],
    params: &SelectParams,
    zoom: u8,
    underlay_offset: Option<u8>,
    cancel: &Option<CancelToken>,
) -> Result<Option<TileResult>> {
    check_cancelled(cancel)?;

    let Some(segment) = segment else {
        return Ok(None);
    };

    let mut stats = TileProbe::new();
    stats.count(|t| &mut t.rows_in_ranges, range.len() as u64);

    let visible = mask.count_range(range.clone());
    stats.lap(|t| &mut t.count_ns);

    if visible == 0 {
        // Skip empty: no count row, no selection work for a tile with nothing visible — the same
        // rule the old inline loop applied.
        return Ok(None);
    }
    stats.count(|t| &mut t.tiles_nonempty, 1);
    stats.count(|t| &mut t.sigma_visible, visible);

    let selected = Selection::of(mask, segment, range.clone(), params, visible);
    stats.lap(|t| &mut t.select_ns);
    // Counted by `Selection::of` itself, inside the loops that do the reading — not from
    // `visible`, which would make the `visited == sigma_visible` cross-check a tautology.
    stats.count(|t| &mut t.select_rows_visited, selected.rows_visited);

    let count = TileCount {
        tile: tile.prefix,
        visible,
        // Phase 1 has no filters (Reference Sheet R5): matched == visible everywhere.
        matched: visible,
        served: selected.rows.len() as u64,
    };

    let points: Vec<PointOut> = selected
        .rows
        .into_iter()
        .map(|row| row_to_point(segment, row, declared_scalars))
        .collect();
    stats.lap(|t| &mut t.gather_ns);
    stats.count(|t| &mut t.points_gathered, points.len() as u64);

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
            // Search only the parent's range: sub-cells partition their parent, so this is exactly
            // `tile_ranges` would return, over tens of kilobytes already touched by the parent's
            // own `count_range` rather than ~30 levels of a 4 GB mmap.
            let sub_count = mask.count_range(tile_ranges_within(segment, &sub_tile, range.clone()));
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

    Ok(Some(TileResult {
        count,
        points,
        sub_cells,
        stats: stats.t,
    }))
}

/// One tile's parallel-sweep output — [`tile_result`]'s return payload, folded serially and
/// in-order into the request's `tile_counts`/`points`/`sub_cells`/[`StageTimings`] by
/// `Engine::viewport` (D-F). An implementation detail of the parallel sweep, not part of this
/// crate's public API — `ViewportOut` is what callers see.
struct TileResult {
    count: TileCount,
    points: Vec<PointOut>,
    sub_cells: Vec<SubCellCount>,
    stats: TileStats,
}

/// Gather one row's `entity_id`/`x`/`y`/declared scalars through `ColumnsRef` — zero-copy reads,
/// no per-row allocation beyond what a `Utf8` scalar's owned `String` requires.
fn row_to_point(segment: &SegmentData, row: u32, declared: &[DeclaredScalar]) -> PointOut {
    let idx = row as usize;
    let cols = &segment.columns;
    let tessera_id = TesseraId::new(cols.tessera_id()[idx]);
    let x = cols.x()[idx];
    let y = cols.y()[idx];

    let mut scalars = Vec::with_capacity(declared.len());
    for declared_scalar in declared {
        // A declared scalar absent from this segment's schema (shouldn't happen once the build
        // pipeline writes declared columns, but Phase 1's build never does yet) is skipped rather
        // than treated as an error — nothing here is authorisation-relevant.
        if let Some(value) = cols.scalar(&declared_scalar.name) {
            scalars.push(match value {
                ScalarSlice::U64(s) => ScalarOut::U64(s[idx]),
                ScalarSlice::F32(s) => ScalarOut::F32(s[idx]),
                ScalarSlice::Utf8(arr) => ScalarOut::Utf8(arr.value(idx).to_string()),
            });
        }
    }

    PointOut {
        tessera_id,
        x,
        y,
        scalars,
    }
}
