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
mod served;
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
pub(crate) use served::ServedView;
pub(crate) use sweep::{scoped_render_families, segment_row_of};

use geometry::OpenView;
use out::{emit_points, CollectSink, FilterBits, PointSchema};
use row_filter::crossing_domain;
use sweep::{
    gather_tile_columns, resolve_scalars, scoped_render_scalars, tile_ranges, Swept, TileSweepOut,
    Tiling, MAX_POINTS_FRAME_BYTES,
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
        let mut probe = Probe::new();

        // Load the generation pointer exactly once, at request start (see `GenerationHandle`'s
        // doc at its definition) — every subsequent read below (pin check, row-projection cache,
        // composition) comes from this one snapshot, so a concurrent overlay/bundle swap
        // mid-request can never mix state from two generations.
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
        let stale = req
            .stamp
            .as_ref()
            .is_some_and(|presented| *presented != answered_from);
        probe.lap(|t| &mut t.stamp_compare_ns);

        let k = req.k.min(self.config.max_k);

        let OpenView {
            served,
            mask,
            geometry,
            coordinates,
            render_scalars,
        } = self.open_view(session, &generation, req.view, &req.cancel, &mut probe)?;

        let tiles = self.resolve_tiles(&served, &req, &mut probe)?;

        // D-C checkpoint: before θ's anchor, the second long serial-prefix stage this task guards.
        check_cancelled(&req.cancel)?;
        let theta = self.theta(&served, &generation, &geometry, &mask, &req, &mut probe);
        let params = SelectParams {
            k_min: self.config.k_min,
            // The client may ask for less than the overplot ceiling; it may not ask for more.
            // Applying it here rather than truncating afterwards is free — the served set is a
            // prefix, so the two agree — and it bounds the selection heap and the output gather.
            cap: k.min(self.config.k_max_marks),
            threshold: theta.threshold,
        };

        let tiling = tile_ranges(tiles, &served.segments, &mut probe);

        let (mask, region) =
            self.narrow_to_filters(&served, mask, &tiling, theta.v_total, &req, &mut probe)?;

        // **`point_rows = "highlight"` is a column projection and nothing else** (§2): the row
        // set, the tile split and `served` are what the same request answers under `"full"`,
        // because none of them depends on the highlight. What changes is that the gather reads no
        // render column and the membership resolver is not built — so a client changing only its
        // highlight is served the bits it asked for and not the payload it already holds. Without
        // a `highlight` on the request there is nothing to project to, and this answers as
        // `"full"` does rather than serving a column of nulls.
        let highlight_only = req.point_rows == PointRows::Highlight && mask.has_highlight();
        let render_scalars: &[DeclaredScalar] = if highlight_only { &[] } else { &render_scalars };

        // The head, delivered before the sweep: everything the response headers derive from is
        // known here — the region verdict last, settled by the decomposition above and never by a
        // row — and a server that waits for the first flush before committing a status needs it
        // in hand by then. A refusal is the consumer gone — cancellation, not a fault.
        sink.head(ViewportHead {
            coordinates,
            stamp: answered_from.clone(),
            stale,
            region,
            render_scalars: render_scalars.to_vec(),
            highlighted: mask.has_highlight(),
        })
        .map_err(|SinkClosed| EngineError::Cancelled)?;
        // Reset the clock so the head's construction and delivery are unattributed rather than
        // silently charged to the stage that follows.
        probe.skip();

        let Swept {
            tile_counts,
            sub_cells,
            swept,
        } = self.sweep_tiles(&served, &mask, &tiling, &params, &req, &mut probe)?;

        // The first flush: every count, before any point (`streamed-serving.md` §2 — the number
        // channel is exact from the first paint). `None`, not an empty slice, when the underlay
        // was not requested: the wire's frame-presence rule needs the distinction.
        sink.counts(
            &tile_counts,
            tiling.underlay_offset.map(|_| sub_cells.as_slice()),
        )
        .map_err(|SinkClosed| EngineError::Cancelled)?;
        probe.skip();

        // The artifacts frame, after the counts and before any point. It is an aggregate channel,
        // not a point one — a cluster's masked count belongs beside a tile's, not beside a mark.
        let (artifacts, served_layers) = self.serve_artifacts(
            &served,
            &tiling.ranges,
            &mask,
            req.layers,
            req.artifact_budget,
            req.levels,
            req.computed,
            req.zoom,
            req.artifact_rows,
            &req.cancel,
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

        emit_points(
            &swept,
            &PointSchema {
                render_scalars,
                membership,
            },
            &mask,
            flush_bytes,
            &req.cancel,
            &mut probe,
            sink,
        )?;

        // `total_ns` (stamped by `finish`) is this call's wall clock, which under streaming
        // includes the sink's sends — consumer-paced time, not compute. Per-stage figures are
        // unaffected: gather laps bracket gather-and-append only, and no lap spans a send.
        Ok(probe.finish())
    }
}
