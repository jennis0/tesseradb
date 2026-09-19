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
//! [`crate::select::Threshold::at_depth`] for the I2 argument) and to gather what selection
//! returns.
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

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use rayon::prelude::*;
use sha2::{Digest, Sha256};

use tessera_authz::FrozenFragment;
use tessera_spatial::projection::Projection;
use tessera_spatial::tiler::ScalarType;
use tessera_spatial::{tiles_for_bbox, tiles_for_bbox_count, Bounds, Tile};
use tessera_store::manifest::{DeclaredScalar, Quantisation, ViewMetadataValue};
use tessera_store::read::{ScalarSlice, SegmentData};
use tessera_store::vocabulary::Vocabularies;
use tessera_store::{tile_ranges_all, tile_ranges_within};
use tessera_types::layer::ComputedProperty;
use tessera_types::{EntityId, GenerationStamp, RowId, TermId, TesseraId, API_VERSION};

use crate::cache::{CacheWaitEnded, Peek, RowProjectionKey, SessionGeometry};
use crate::cancel::CancelToken;
use crate::compose::{compose, visible_to, EffectiveMask, FilterRows};
use crate::filter::{Endpoint, Family, FilterOperand, Scalar};
use crate::membership_column::{ServedLayer, ServedLevel};
use crate::select::{SelectParams, Selection, SelectionPart, SelectionParts, Threshold};
use crate::engine::Engine;
use crate::error::{EngineError, Result};
use crate::session::Session;
use crate::timing::{Probe, StageTimings, TileProbe, TileStats};
use crate::Generation;

mod artifacts;
mod geometry;
mod item;
mod meta;
mod out;
mod request;
mod row_filter;
mod sweep;

pub use item::{ItemField, ItemOut, ItemScoped, ItemView};
pub use meta::{EngineMeta, LeafColumn, MetaGroup, MetaRoster, MetaView, TileAddress};
pub use out::{
    ArtifactOut, ColumnBuf, PointColumns, ScalarOut, SinkClosed, SinkResult, SubCellCount,
    TileCount, ViewCoordinates, ViewportHead, ViewportOut, ViewportSink,
};
pub use request::{
    ArtifactRows, ComputedSelection, LayerSelection, LevelSelection, PointRows, ViewportRequest,
};
pub use sweep::{segments_with_row_bases, SERIAL_FALLBACK_MAX_ROWS, TILE_PAR_MIN_TILES};

pub(crate) use artifacts::response_rungs;
pub(crate) use item::flushed_row_scalar;
pub(crate) use meta::owning_key_of;
pub(crate) use row_filter::{predicate_source, predicate_vocabulary};
pub(crate) use sweep::{scoped_render_families, segment_row_of};

use out::{CollectSink, FilterBits};
use row_filter::crossing_domain;
use sweep::{
    gather_tile_columns, resolve_scalars, scoped_render_scalars, should_fold_serially,
    tile_sweep, TileSweepOut, MAX_POINTS_FRAME_BYTES, TILE_PAR_MIN_LEN,
};

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
            highlighted: head.highlighted.then(Vec::new),
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
            highlight: _,
            point_rows,
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
        let mut render_scalars: Vec<_> = generation
            .bundle
            .manifest
            .render_scalars()
            .cloned()
            .collect();
        // **Then this view's scoped render columns, and only this view's** (`views.md` §5). A
        // group-scoped attribute declaring `render` occupies a slot in the row tail of every view
        // of its group — and of any group sharing those views — and in no other, so the list is
        // per view where the bundle-wide half above is not. Appended rather than interleaved,
        // which is the order the build and every flush write the lanes in.
        render_scalars.extend(scoped_render_scalars(
            &generation.bundle.manifest,
            view,
            session.visible_views(),
        ));

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
            .denied()
            .get(view)
            .ok_or_else(|| EngineError::DenyMaskMissing {
                view: view.to_string(),
            })?;

        let mask = compose(
            session.satisfied(),
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

        // **This view's frame, not the bundle's** (decision 0040): every tile address below is a
        // fraction of the extent the requested view's positions were quantised against, so
        // reading another view's would address different ground under the same prefix. An unknown
        // name refuses rather than defaulting — there is no frame a view that does not exist
        // could be drawn in.
        let q = generation
            .bundle
            .manifest
            .quantisation_of(view)
            .ok_or_else(|| EngineError::UnknownView(view.to_string()))?;
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

        // θ's two anchors, both over the session's **composed** mask and this view's whole row
        // space: `V_total`, the visible cardinality, and `N_occ(zoom)`, the number of depth-`zoom`
        // tiles holding at least one visible row. Both must be the composed figures and not
        // `base`'s — see `Threshold::at_depth`'s doc for the I2 argument and the concrete channel
        // the pre-overlay figures open.
        //
        // Both are viewport-*invariant*: they depend on the session's mask, the view and the
        // generation, never on `bbox`, so θ does not move when the viewer pans — which is the churn
        // §7.2 forbids. `N_occ` is a function of the depth, so θ moves on a *zoom*, which is what
        // makes the mean occupied tile draw `m_target` marks at every depth; nesting survives it
        // because `N_occ` is non-decreasing in depth, so θ is monotone (§7.2). Both move on an
        // overlay swap, which is accepted: swaps are rare against pans, and because the served set
        // is a `tessera_id` prefix, a small θ move perturbs only the marks nearest the cut.
        // D-C checkpoint: before θ's anchor, the second long serial-prefix stage this task guards.
        check_cancelled(&cancel)?;
        let v_total = mask.visible_total();
        probe.lap(|t| &mut t.theta_anchor_ns);
        let threshold = match req.filter {
            // §8.5's match-layer count rule: a filtered request serves every match, up to the cap.
            // The θ threshold clause does not thin a filtered selection, and saturating is how the
            // definition says "in full": `C_θ = |vis(T)|`, so `served = min(matched, cap)` per
            // tile, with the cap-many smallest `tessera_id`s when a tile is over — the same prefix
            // rule as ever, so nesting across zooms is untouched. Neither anchor is walked, both
            // being inputs to a threshold this request does not consult.
            //
            // This is NOT a re-anchor. θ's anchors stay the unfiltered composed mask's, and §5.2 of
            // `filter-surface.md` forbids anchoring on `M_sel` (a threshold that moved as the
            // viewer typed). The rule here is the other half of the same section: the anchor never
            // narrows, and the match layer never samples. Before this, a filtered tile was pushed
            // through the unfiltered θ odds — a tile narrowed from 4,000 visible to 40 matched drew
            // ~1% of 40, i.e. the k_min floor — so the map thinned in proportion to the filter's
            // selectivity instead of showing the matches.
            Some(_) => Threshold::Saturated,
            None => Threshold::at_depth(
                v_total,
                self.config.theta_target_marks,
                self.occupied_tiles(&mask_identity, view, &segments, &mask, zoom),
            ),
        };
        probe.lap(|t| &mut t.theta_occupancy_ns);
        // **The rest of the ladder, on the pool, after this request has its own rung** — see
        // `crate::stage`. This request walked at `zoom` and kept every rung below it; the fill
        // takes the ladder the rest of the way to `stage::BACKGROUND_DEPTH`, so a session's zoom
        // to depth 12 and its zoom back out cost nothing. Spawned here rather than at authorise
        // because `N_occ` is counted over the composed mask, which needs the row projection this
        // request has just resolved and which no session has before its first request.
        //
        // **Only where the anchor was taken.** A filtered request saturates θ and consults neither
        // anchor (above), so it has no reason to warm one; the next unfiltered request spawns the
        // fill for itself.
        if req.filter.is_none() && zoom < crate::stage::BACKGROUND_DEPTH {
            self.spawn_ladder_fill(&mask_identity, view, session, &generation, &geometry);
        }
        let params = SelectParams {
            k_min: self.config.k_min,
            // The client may ask for less than the overplot ceiling; it may not ask for more.
            // Applying it here rather than truncating afterwards is free — the served set is a
            // prefix, so the two agree — and it bounds the selection heap and the output gather.
            cap: k.min(self.config.k_max_marks),
            threshold,
        };

        let mut tile_counts = Vec::new();
        let mut sub_cells = Vec::new();

        // Resolve every tile's row range in ONE monotone sweep rather than two full-column binary
        // searches per tile. A few hundred independent `log2(rows)` searches is where a sparse
        // request's time actually goes — measured at 26-64% of one
        // (docs/evidence/memos/2026-07-30-f1-selection-overdraw.md), and flat in density, because
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
        //
        // **`filters` and `highlight` are two expressions of one request, evaluated here together**
        // (`highlight-and-hierarchy.md` §2.1). Both run against the same candidate and through the
        // same resolvers, closed over the same **pre-filter** mask — a highlight is a conjunction
        // with the filter's candidate by construction, and evaluating its region or `member_of`
        // leaves against an already-filtered mask would make the two positions of one clause mean
        // different things. The mask takes both results afterwards, in separate fields: only
        // `with_filter`'s narrows what is drawn.
        let mut region_verdict: Option<crate::region::RegionVerdict> = None;
        let mask = if req.filter.is_none() && req.highlight.is_none() {
            mask
        } else {
            check_cancelled(&cancel)?;
            let fragment = self.fragment_for(session, &generation)?;
            let candidate = crate::filter::candidate(
                &fragment,
                session.satisfied(),
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
            // The `member_of` leaves' resolver, closed over the same mask for the same
            // reason: the answer is `membership ∩ M_auth`, and a resolver that could be
            // called without one would be a route to the unmasked membership.
            let members = |leaf: &crate::filter::MemberOfLeaf| {
                self.resolve_member_of(
                    leaf,
                    session,
                    &generation,
                    view,
                    view_data,
                    &segments,
                    &mask,
                    denied,
                    mask_identity,
                )
            };
            let resolvers = crate::filter::RowLeafResolvers {
                regions: &regions,
                members: &members,
            };
            let row_bases: Vec<u32> = segments.iter().map(|&(_, base)| base).collect();
            let domain = crossing_domain(&ranges, &row_bases);
            // One transcription of the evaluate-route-cross sequence, called for each expression,
            // so the two positions of a clause cannot drift apart. `per_tile_only` is the
            // highlight's route and `count_matched` its exclusion from the `filter_matched` probe
            // — a highlight's own cardinality is not the filter's, and adding it there would make
            // one gauge report two quantities.
            let mut evaluate = |expr: &crate::filter::FilterExpr,
                                per_tile_only: bool,
                                count_matched: bool|
             -> Result<(FilterRows, Option<crate::region::RegionVerdict>)> {
                let routed = generation
                    .filter_columns
                    .evaluate_routed(expr, &candidate, rows_in_ranges <= v_total, &resolvers)
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
                // One crossing per expression, whichever shape came back (0062's tree; 0068). The
                // row of `filter_matched` reports what the route produced: matched entities on
                // the entity route, matched rows-in-domain on the row route.
                let out = match routed {
                    crate::filter::RoutedFilter::Entity(entities) => {
                        if count_matched {
                            probe.count(|t| &mut t.filter_matched, entities.cardinality());
                        }
                        (
                            self.cross_filter_into_row_space(
                                &view_data.row_space,
                                &entities,
                                &ranges,
                                &segments,
                                rows_in_ranges,
                                per_tile_only,
                            ),
                            None,
                        )
                    }
                    crate::filter::RoutedFilter::Row(tree) => {
                        let verdict = tree.region_verdict();
                        let rows = self.evaluate_row_route(
                            &tree,
                            &view_data.row_space,
                            &segments,
                            &domain,
                            rows_in_ranges,
                            view_data.row_space.total_rows(),
                            per_tile_only,
                        )?;
                        self.counters.filter_row_routed.fetch_add(1, Ordering::Relaxed);
                        // **Not counted when a region is in the tree.** Its interior rows have
                        // not met the mask yet, so the cardinality would be a pre-mask quantity
                        // about the region — the number selection-operand §7 says may not be
                        // computed, for a metric or for anything else.
                        if count_matched && !tree.has_region() {
                            probe.count(|t| &mut t.filter_matched, rows.rows().cardinality());
                        }
                        (rows, verdict)
                    }
                };
                probe.lap(|t| &mut t.filter_cross_ns);
                Ok(out)
            };
            let filter_rows = match &req.filter {
                None => None,
                Some(expr) => {
                    let (rows, verdict) = evaluate(expr, false, true)?;
                    region_verdict = verdict;
                    Some(rows)
                }
            };
            // **The highlight always takes the per-tile walk**, whatever it matched corpus-wide:
            // its three answers are all inside the request's tiles, so the whole-view projection
            // would be paid for nothing (§2.1).
            let highlight_rows = match &req.highlight {
                None => None,
                Some(expr) => {
                    let (rows, verdict) = evaluate(expr, true, false)?;
                    // The coarsest of the two, exactly as two region leaves of one expression
                    // combine: a cover anywhere makes the response's verdict a cover.
                    region_verdict = match (region_verdict, verdict) {
                        (Some(a), Some(b)) => Some(a.coarser(b)),
                        (a, b) => a.or(b),
                    };
                    Some(rows)
                }
            };
            let mask = match filter_rows {
                Some(rows) => mask.with_filter(rows),
                None => mask,
            };
            match highlight_rows {
                Some(rows) => mask.with_highlight(rows),
                None => mask,
            }
        };

        // **`point_rows = "highlight"` is a column projection and nothing else** (§2): the row
        // set, the tile split and `served` are what the same request answers under `"full"`,
        // because none of them depends on the highlight. What changes is that the gather reads no
        // render column and the membership resolver is not built — so a client changing only its
        // highlight is served the bits it asked for and not the payload it already holds. Without
        // a `highlight` on the request there is nothing to project to, and this answers as
        // `"full"` does rather than serving a column of nulls.
        let highlight_only = point_rows == PointRows::Highlight && mask.has_highlight();
        let render_scalars: &[DeclaredScalar] = if highlight_only { &[] } else { render_scalars };

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
            highlighted: mask.has_highlight(),
        })
        .map_err(|SinkClosed| EngineError::Cancelled)?;
        // Reset the clock so the head's construction and delivery are unattributed rather than
        // silently charged to the stage that follows.
        probe.skip();

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
        let serial_fallback_max_rows =
            self.switches.serial_fallback_max_rows.load(Ordering::Relaxed);
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
        let membership = if artifacts.is_empty() || highlight_only {
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
            highlighted: mask.has_highlight().then(Vec::new),
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

        // `total_ns` (stamped by `finish`) is this call's wall clock, which under streaming
        // includes the sink's sends — consumer-paced time, not compute. Per-stage figures are
        // unaffected: gather laps bracket gather-and-append only, and no lap spans a send.
        Ok(probe.finish())
    }
}
