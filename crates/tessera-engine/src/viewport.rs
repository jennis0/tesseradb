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
//!
//! **Calibration task: below [`SERIAL_FALLBACK_MAX_ROWS`], the fan-out above does not run at
//! all.** Measured (2.42M-fixture, w=10 grant, zoom 8) at 2.97x-13x slower at
//! `compute_threads = default` than at `compute_threads = 1` for a typical small viewport — the
//! `pool.install` fan-out's own entry/scheduling cost dominates the ~µs of real per-tile work a
//! sparse, ~256-tile request produces. `Engine::viewport` instead folds `tile_result` serially,
//! in tile order, producing the identical `Vec<Result<Option<TileResult>>>` shape the fold below
//! already consumes — so the fold, and therefore the response, is unaffected by which branch ran.
//! See [`SERIAL_FALLBACK_MAX_ROWS`]'s doc for the predictor argument and the sweep data, and the
//! calibration report for the full method.

use std::ops::Range;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use rayon::prelude::*;

use tessera_spatial::{tiles_for_bbox, tiles_for_bbox_count, Extent, Tile};
use tessera_store::manifest::{DeclaredScalar, Quantisation};
use tessera_store::read::{ScalarSlice, SegmentData};
use tessera_store::{tile_ranges_all, tile_ranges_within};
use tessera_types::{EntityId, PinId, TesseraId, API_VERSION};

use crate::cache::RowProjectionKey;
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

    /// `POST /v1/items/{handle}` (R5): validate `epoch` if the caller sent one, invert `id` to
    /// its entity, test visibility in entity space, and only then locate a row and read its
    /// scalars/external id.
    ///
    /// **`epoch` is checked against the SAME generation this call loads for the lookup below —
    /// never a separate `Engine::meta()` call.** Fix wave, Task 2 finding: the handler used to
    /// call `Engine::meta()` (its own `generation.load_full()`, plus a clone of every declared
    /// scalar and slice name, just to read one field) before calling this method, which loads
    /// the generation again — two independent loads for one logical request, against lifecycle
    /// §1.1's one-load-per-request invariant. Checking here, first, against the snapshot already
    /// in hand removes the second load and closes the (correctness, not just cost) gap where a
    /// generation swap landing between the two calls could validate the epoch against one
    /// generation and serve the lookup from another.
    ///
    /// Returns `Ok(None)` both when `id` names nothing in this bundle and when it names an item
    /// the principal may not see — deliberately one outcome from one code path, so the server
    /// cannot differentiate what the engine does not tell it (owner ruling; contracts §3.2).
    ///
    /// **The timing channel is closed, not narrowed** (Critical C-5; design Appendix C, C4
    /// annotation). The epoch check is entity-independent — it runs identically for every `id`,
    /// before inversion, and does not read `id` at all — so it opens no channel of its own.
    /// Inversion is a pure function taking no I/O. The visibility test that follows is an
    /// entity-space question — three constant-time probes — and is **the same three probes for
    /// an identifier that names nothing and one that names an invisible item**. No
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
        epoch: Option<u32>,
    ) -> Result<Option<ItemOut>> {
        let generation = self.generation.load_full();

        // Checked FIRST, against the generation this call already loaded above — see this
        // method's doc for why that (not a separate `Engine::meta()` call) is load-bearing here.
        if let Some(e) = epoch {
            if e != generation.bundle.manifest.identity.epoch {
                return Err(EngineError::StaleIdentityEpoch);
            }
        }

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
                    // N-3: propagate, never swallow. `EngineError::Store`, the same wrapping
                    // every other store-backed call in this crate uses (see `Engine::open`).
                    external_id: self.external_id_of(entity).map_err(EngineError::Store)?,
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

        // I11 is enforced in `pins`; the whole rule, and both failures it names, are stated in that
        // module's doc and nowhere else. Three consequences land *here*, and each is checkable by
        // reading the lines below rather than by trusting this comment:
        //
        // 1. `resolve` hands back `PinnedGeometry` — geometry ONLY. Overlay, buffer and
        //    `overlay_version` are read from the LIVE `generation` below, as this loop always has,
        //    so a suppression accepted mid-request applies to a pinned request too (R-open).
        // 2. This call takes no lock unless `pin` names a superseded generation (`pins` module doc,
        //    property 1). It is on the path of every admitted request at 48-way concurrency.
        // 3. A `410` from here after a restart is correct and must stay correct: a restarted
        //    worker's drain list is empty, and both tempting repairs are forbidden — see
        //    `PinManager::resolve_drained`, which names them at the site that would implement them.
        let geometry = self.pins.resolve(pin, session.token_id, &generation)?;
        let effective_pin = geometry.pin_id();

        probe.lap(|t| &mut t.pin_resolve_ns);

        let k = k.min(self.config.max_k);

        // Fail closed on a slice spanning partitions, for the same reason the segment guard below
        // exists: this resolves to ONE partition, and theta's anchor and every rank are then taken
        // over that partition alone — which §12.3 forbids (the anchor must be session-global, or
        // "below the cut" means different things in different partitions). Phase 1 emits one
        // partition, so this is unreachable; it is here so a §12 bundle cannot be served
        // half-masked with no error, which is the failure the multi-segment guard already refuses.
        let carriers = geometry
            .bundle
            .partitions
            .values()
            .filter(|partition| partition.slices.contains_key(slice))
            .count();
        if carriers > 1 {
            return Err(EngineError::MultiPartitionSlice(slice.to_string()));
        }
        let slice_data = geometry
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

        let cache_key = RowProjectionKey {
            token_id: session.token_id,
            slice: slice.to_string(),
            segments_version: geometry.segments_version,
        };
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
            .map_err(|_busy| EngineError::ProjectionBuilding)?;
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

        let q = &geometry.bundle.manifest.quantisation;
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

        let declared_scalars = &geometry.bundle.manifest.declared_scalars;

        // §3.3 underlay bounds, all three checked up front and all three *rejecting* rather than
        // clamping (see `EngineError::UnderlayRefused`). The cell budget is checked before any
        // counting work because the underlay multiplies the (already-bounded) tile set by 4^offset.
        //
        // Fix round 1: `underlay_cells_demanded` (0 when no underlay was requested) is captured
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

        // Calibration task: the predictor decides serial-fold vs `pool.install` fan-out, and it
        // must be available BEFORE either path runs — `Σ range.len()`, the total rows every
        // resolved tile spans (pre-mask, pre-select), is exactly that: already materialised by
        // the `tile_ranges_all` sweep above, costs one pass over `ranges` to sum, and needs no
        // work from either candidate path to compute. See [`SERIAL_FALLBACK_MAX_ROWS`]'s doc for
        // why this predictor (and not tile count) is the one the sweep data supports.
        //
        // Fix round 1: `underlay_cells_demanded` is added in, not left out. The underlay's own
        // per-cell cost is "one small binary search plus one bitmap range-count" (the underlay
        // block's own comment, below) — the same shape of operation `count_range` performs per
        // row-range, so summing the two into one row-equivalent total before comparing against
        // the threshold is the natural extension of the same predictor, not a second one bolted
        // on. Before this fix a saturated underlay (`max_underlay_cells`, default 8192) was
        // invisible to this decision entirely, regardless of how large the resulting per-tile
        // sub-cell fan-out actually was.
        //
        // §14.2 fix: this is also `StageTimings::rows_in_ranges`'s whole value, computed here
        // rather than accumulated per-tile inside `tile_result`. It used to be counted into each
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
        let rows_in_ranges: u64 = ranges.iter().map(|r| r.len() as u64).sum();
        probe.count(|t| &mut t.rows_in_ranges, rows_in_ranges);
        let total_rows_in_ranges: u64 = rows_in_ranges + underlay_cells_demanded;

        // D-D/D-F, calibrated: below `SERIAL_FALLBACK_MAX_ROWS`, fold `tile_result` in place —
        // same function, same input order, no `pool.install` — since below that line the fan-out's
        // own entry/scheduling cost exceeds the per-tile work it would parallelise (measured; see
        // the constant's doc). At or above it, the existing `pool.install` fan-out runs, on the
        // ONE shared pool this engine built at `Engine::open` — no second, per-request pool, no
        // nested throttling (D-D). Every input to `tile_result` is borrowed or `Copy`:
        // `mask`/`segment`/`declared_scalars`/`params` are the generation- and request-derived
        // values already resolved above (lifecycle §1.1 — nothing is re-loaded per tile), and
        // `cancel` is the D-C token, checked inside `tile_result` at the very top (moved there
        // from the old loop's first line — Task 5).
        //
        // Both branches produce `Vec<Result<Option<TileResult>>>` (this crate's `Result<T>` alias
        // for `std::result::Result<T, EngineError>`), in `tiles`' order, so the fold below is
        // identical either way — this is what makes the two paths byte-identical (see this
        // module's doc; `with_min_len(TILE_PAR_MIN_LEN)` and the parallel branch's own collect
        // shape are unchanged from Task 6, still load-bearing for THAT claim within the parallel
        // branch itself).
        //
        // D-C cancellation bound, both branches: a `Cancelled` observed inside `tile_result`
        // propagates to the fold below regardless of path, which discards every result after the
        // first `Err` it walks (see the fold's own comment). What differs is how much wasted work
        // can be IN FLIGHT past the checkpoint at the instant of cancellation. Serial fold: at
        // most ONE tile — the one `tile_result` call currently running, since nothing else is
        // concurrently past the checkpoint by construction. Parallel fan-out: at most
        // `compute_threads` tiles (one per worker) — every tile that had already passed the
        // checkpoint keeps running to completion; every tile whose worker had not yet reached it
        // observes the flip there instead and returns immediately. The serial path's bound is
        // therefore strictly tighter, not merely no-worse.
        // Fix round 1: one closure, not two independently-maintained copies of the same 9-argument
        // call — the duplication was a divergence risk (a future change to `tile_result`'s
        // argument list would need to be made twice, silently, with no compiler help if one copy
        // were missed). `run` captures only shared references and `Copy` values (`&mask`,
        // `segment`, `declared_scalars`, `&params`, `zoom`, `underlay_offset`, `&cancel`), so it is
        // `Sync` for free and usable from both the serial `Iterator::map` below and rayon's
        // parallel `map` inside `pool.install` — no new bound this file did not already require of
        // these captures for the parallel branch to compile before this change.
        let run = |tile: &Tile, range: Range<u32>| {
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
        };

        // §14 fix round 1/2: the threshold is read from `self`, not the constant directly, so
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
        let tile_outcomes: Vec<Result<Option<TileResult>>> =
            if should_fold_serially(total_rows_in_ranges, serial_fallback_max_rows, tiles.len()) {
                tiles
                    .iter()
                    .zip(ranges)
                    .map(|(tile, range)| run(tile, range))
                    .collect::<Vec<Result<Option<TileResult>>>>()
            } else {
                self.pool.install(|| {
                    tiles
                        .par_iter()
                        .zip(ranges.into_par_iter())
                        .with_min_len(TILE_PAR_MIN_LEN)
                        .map(|(tile, range)| run(tile, range))
                        .collect::<Vec<Result<Option<TileResult>>>>()
                })
            };
        // D-E: neither branch's own wall time is a named stage — it is already fully accounted
        // for, per tile, inside each `TileResult::stats` (folded below) — so this resets the clock
        // without charging the stretch to whatever lap runs next, rather than leaving it to be
        // silently misattributed. True of the serial branch too: its per-tile costs are equally
        // captured in `TileStats`, so `skip()` here keeps both branches' accounting symmetric.
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

/// Calibration task threshold: below this many total rows spanned by a request's resolved tiles
/// PLUS its §3.3 underlay cell demand if any (`Σ range.len() + underlay_cells_demanded`, pre-mask
/// — see the call site's `total_rows_in_ranges`), `Engine::viewport` folds `tile_result` serially
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
/// - At 1e9 (the bench's own dense grant — the realistic one, per fix round 1's own finding):
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
/// Mask density (fix round 1) remains unmodelled by this pre-mask predictor for the same reason
/// as before — see the historical argument in this crate's git history (commit `7e5b553`) for the
/// full reasoning; §14's re-sweep used both a sparse and the bench's dense grant at every scale and
/// found the shape-family split above (natural vs full-extent) under BOTH, so mask density is not
/// the dominant driver of the three-scale tension this revision addresses.
pub const SERIAL_FALLBACK_MAX_ROWS: u64 = 500_000_000;

/// The predictor, pulled out as its own pure function so it is unit-testable without an `Engine`
/// or a bundle (see the `tests` module at the bottom of this file) — the behavioural claim ("a
/// below-threshold request runs the serial fold") is otherwise only observable through output
/// equality or timing, neither of which makes a good unit test on its own.
///
/// Takes `threshold` explicitly (§14 fix round 1) rather than reading `SERIAL_FALLBACK_MAX_ROWS`
/// directly, so the one call site (`Engine::viewport`) can supply either the production constant
/// or a test's override — see `Engine::set_serial_fallback_max_rows_for_test`'s doc for why an
/// override exists at all and why it lives on `Engine`, not here.
/// **Two terms since 2026-08-01** (owner decision, on the two-axis sweep —
/// `docs/design-memos/2026-07-31-tile-parallelism-calibration.md`, "Follow-up 4, answered", and
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
/// per-tile cost is highly non-uniform — an empty-tile skip (`tile_result` returning `Ok(None)`
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
/// **Claim, scoped precisely (fix round 1 correction — the first pass over-generalised).** `8` is
/// decisively best on `full-extent/{z2,z3,z4,z5}` at 1e8 and 1e9 — these are the shapes that
/// actually reach the parallel branch after §14's threshold change (`full-extent`'s row count is
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

    // §14.2 fix: `rows_in_ranges` is no longer counted here. It is now summed once, mask-free,
    // over `ranges` in `Engine::viewport`'s serial prefix — see that call site's comment. Counting
    // it per-tile put it in `TileStats`, whose contribution this function's `Ok(None)` returns
    // (this one included, three lines below) cause `Engine::viewport`'s fold to discard outright —
    // silently making a documented-mask-independent field depend on which tiles a grant leaves
    // empty.
    let mut stats = TileProbe::new();

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Calibration task's own behavioural test on the predictor, per the brief's preference for
    /// this over test-only instrumentation: exact boundary behaviour, both edges.
    #[test]
    fn should_fold_serially_is_a_strict_less_than_at_the_calibrated_boundary() {
        let t = SERIAL_FALLBACK_MAX_ROWS;
        assert!(should_fold_serially(0, t, 1));
        assert!(should_fold_serially(t - 1, t, 1));
        assert!(!should_fold_serially(t, t, 1));
        assert!(!should_fold_serially(t + 1, t, 1));
        assert!(!should_fold_serially(u64::MAX, t, 1));
    }

    /// §14 fix round 1: `should_fold_serially` takes its threshold as a parameter now (so
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
        assert!(!should_fold_serially(rows_far_below, t, TILE_PAR_MIN_TILES + 1));
    }

    /// The rule is a disjunction: either term alone suffices and neither is necessary. A mutation
    /// making it a conjunction must fail.
    #[test]
    fn the_two_terms_are_a_disjunction_not_a_conjunction() {
        let t = SERIAL_FALLBACK_MAX_ROWS;
        assert!(!should_fold_serially(t, t, 1), "rows fire alone");
        assert!(!should_fold_serially(1, t, TILE_PAR_MIN_TILES), "tiles fire alone");
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
    #[test]
    fn the_natural_family_cannot_reach_the_tile_arm() {
        const NATURAL_MAX_TILES: usize = 289;
        assert!(
            NATURAL_MAX_TILES < TILE_PAR_MIN_TILES,
            "the natural family tops out at {NATURAL_MAX_TILES} tiles and the arm is at \
             {TILE_PAR_MIN_TILES}. If this fails, the tile arm can fire on ordinary client \
             viewports at 10^9 -- the regression the row threshold was raised to fix."
        );
        assert!(should_fold_serially(354_900_645, SERIAL_FALLBACK_MAX_ROWS, 81));
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
        // The exact collect target shape as `tile_result`'s call sites use, without needing a
        // real `Engine`, `mask` or bundle to produce one: `Result<Option<T>>` per item, `Ok(None)`
        // standing in for `tile_result`'s empty-tile skip.
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
}
