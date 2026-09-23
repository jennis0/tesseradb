//! The masked viewport query. [`Engine::viewport_stream`] resolves the view and this session's
//! mask (`open_view`), the request's tiles and their row ranges (`resolve_tiles`, `tile_ranges`),
//! the display threshold θ (`theta`), narrows to any filter (`narrow_to_filters`), sweeps every
//! tile for its count and selection (`sweep_tiles`), resolves any annotation artifacts
//! (`serve_artifacts`), and gathers and emits the selected points (`emit_points`).
//! [`Engine::viewport`] runs the same producer into a collecting sink and returns the batch
//! [`ViewportOut`]. Selection itself — floor, threshold and cap over `tessera_id`, evaluated
//! inside the mask — lives in [`crate::select`]; this module resolves the per-request parameters
//! and gathers what selection returns. The request is two phases: the **sweep**, which counts,
//! selects and reads the underlay per tile with no gather, and the **emit**, a serial pass over
//! the swept tiles in response order that gathers and hands off flush-sized [`PointColumns`]
//! chunks.
//!
//! A request loads the generation pointer once, at the start, and every phase reads from that
//! snapshot, so a concurrent publication cannot mix state from two generations into one answer.
//! Every count, shape, content and label a phase produces is taken through the composed mask; an
//! unmasked figure would disclose the existence or extent of items a viewer may not see. The
//! verdict for a tile, artifact or item is decided before anything about it is read, and an
//! invisible, suppressed or nonexistent item takes the same path to that verdict as a visible one,
//! so a timing difference cannot tell a viewer which it was. θ's anchor and the masked counts are
//! taken from the pre-filter mask, and the filter and the highlight are both evaluated against
//! that same mask, so an artifact's presence and a tile's density do not move as a viewer types or
//! highlights. A refusal is preferred to an empty answer wherever the answer could not actually be
//! computed: an empty response is a real one. Entity ids, ordinals and term ids never reach a
//! response or a log line. Nothing reachable from a rayon worker touches the single-flight
//! row-projection cache built in `open_view` — it would deadlock the pool — and the availability
//! bounds on tile count, underlay cells and frame bytes are what keep one request's resource use
//! bounded.

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
pub(crate) use row_filter::{crossing_domain, predicate_source, predicate_vocabulary};
pub(crate) use served::ServedView;
pub(crate) use sweep::{scoped_render_families, segment_row_of};

pub(crate) use geometry::OpenView;
use out::{emit_points, CollectSink, FilterBits, PointSchema};
use sweep::{
    gather_tile_columns, resolve_scalars, scoped_render_scalars, tile_ranges, Swept, TileSweepOut,
    Tiling, MAX_POINTS_FRAME_BYTES,
};

/// `Err(EngineError::Cancelled)` once `cancel` has flipped, `Ok(())` otherwise (`None` never
/// flips). Not called around the row-projection build: one already in flight runs to completion
/// regardless of this caller's interest, since its result serves later arrivals too.
#[inline]
fn check_cancelled(cancel: &Option<CancelToken>) -> Result<()> {
    if cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
        return Err(EngineError::Cancelled);
    }
    Ok(())
}

impl Engine {
    /// Mint this response's identity and content keys from the geometry actually served, not the
    /// generation: [`Engine::session_geometry`] can answer from a one-generation-stale projection,
    /// so two responses under one generation snapshot can cover different visible sets, and keying
    /// on the generation would let a later live response elide against a bound the client declared
    /// from the stale one.
    ///
    /// The content key hashes the fragment's watermark, not `segments_version` — a merge or
    /// compaction can move `segments_version` without adding rows to the visible set, which is
    /// what would make an elision unsound — and `overlay_version`, because a suppression or
    /// restoration moves counts a client must not keep presenting as current.
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
    /// sink, so the streamed and batch answers cannot disagree.
    pub fn viewport(&self, session: &Session, req: ViewportRequest<'_>) -> Result<ViewportOut> {
        let mut sink = CollectSink::default();
        // `usize::MAX`: never flush mid-emit, so the collector gets at most one chunk.
        let timings = self.viewport_stream(session, req, usize::MAX, &mut sink)?;
        let head = sink
            .head
            .expect("viewport_stream delivers a head before returning Ok");
        // An empty response emits no points chunk, but the batch shape still carries one buffer
        // per render column, seeded from the head's schema.
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

    /// The masked viewport query, streamed — see [`ViewportRequest`] for the parameters and
    /// [`ViewportSink`] for the delivery order. `flush_bytes` is the emit pass's chunk threshold,
    /// applied at whole-tile boundaries.
    ///
    /// Cancellation is checked once per tile in the sweep, once before [`compose`], once before
    /// θ's anchor, and once per tile in the emit pass; the row-projection build is not gated by it
    /// (see [`check_cancelled`]'s doc). A hit aborts the request with [`EngineError::Cancelled`].
    /// A consumer that already holds a prefix of the response by then can detect the truncation,
    /// because the prefix is whole tiles and the producer never signalled completion.
    pub fn viewport_stream(
        &self,
        session: &Session,
        req: ViewportRequest<'_>,
        flush_bytes: usize,
        sink: &mut dyn ViewportSink,
    ) -> Result<StageTimings> {
        let mut probe = Probe::new();

        // Loaded once, at request start; every read below comes from this one snapshot, so a
        // concurrent overlay or bundle swap cannot mix state from two generations into one answer.
        let generation = self.generation.load_full();
        probe.lap(|t| &mut t.generation_resolve_ns);

        // What this response was answered from, and whether that is newer than what the client
        // presented — one clone and one comparison against the generation already loaded above.
        let answered_from = GenerationStamp {
            prefix: generation.prefix.clone(),
            segments_version: generation.segments_version,
        };
        // A presented stamp naming a superseded generation or prefix is answered exactly as an
        // absent one is: it sets a flag, never selects, refuses or expires.
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

        // Checkpoint before θ's anchor.
        check_cancelled(&req.cancel)?;
        let theta = self.theta(&served, &generation, &geometry, &mask, &req, &mut probe);
        let params = SelectParams {
            k_min: self.config.k_min,
            // The client may ask for less than the overplot ceiling, never more. Applying it here
            // is free, since the served set is a prefix, and bounds the selection heap too.
            cap: k.min(self.config.k_max_marks),
            threshold: theta.threshold,
        };

        let tiling = tile_ranges(tiles, &served.segments, &mut probe);

        let (mask, region) =
            self.narrow_to_filters(&served, mask, &tiling, theta.v_total, &req, &mut probe)?;

        // `point_rows = "highlight"` is a column projection only — the row set and tile split are
        // unaffected — so the gather reads no render column and builds no membership resolver: a
        // client re-requesting only the highlight is not served the payload it already holds.
        let highlight_only = req.point_rows == PointRows::Highlight && mask.has_highlight();
        let render_scalars: &[DeclaredScalar] = if highlight_only { &[] } else { &render_scalars };

        // The head, delivered before the sweep: the region verdict is settled by the decomposition
        // above and never by a row. A refusal from the sink means the consumer is gone.
        sink.head(ViewportHead {
            coordinates,
            stamp: answered_from.clone(),
            stale,
            region,
            render_scalars: render_scalars.to_vec(),
            highlighted: mask.has_highlight(),
        })
        .map_err(|SinkClosed| EngineError::Cancelled)?;
        // Reset the clock so the head's delivery is not charged to the stage that follows.
        probe.skip();

        let Swept {
            tile_counts,
            sub_cells,
            swept,
        } = self.sweep_tiles(&served, &mask, &tiling, &params, &req, &mut probe)?;

        // The first flush: every count, before any point. `None`, not an empty slice, when the
        // underlay was not requested — the wire's frame-presence rule needs that distinction.
        sink.counts(
            &tile_counts,
            tiling.underlay_offset.map(|_| sub_cells.as_slice()),
        )
        .map_err(|SinkClosed| EngineError::Cancelled)?;
        probe.skip();

        // An aggregate channel, after the counts and before any point: a cluster's masked count
        // belongs beside a tile's, not beside a mark.
        let (artifacts, served_layers) = self.serve_artifacts(&served, &mask, &tiling, &req)?;
        if !artifacts.is_empty() {
            sink.artifacts(&artifacts)
                .map_err(|SinkClosed| EngineError::Cancelled)?;
        }
        probe.skip();

        // Resolved once against the served set the artifacts frame just carried. No artifact
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

        // `total_ns` is this call's wall clock, which under streaming includes the sink's sends —
        // consumer-paced time, not compute.
        Ok(probe.finish())
    }
}
