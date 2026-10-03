//! The masked viewport query. [`Engine::viewport_stream`] resolves the view and this session's
//! mask (`open_view`), the request's tiles and their row ranges (`resolve_tiles`, `tile_ranges`),
//! the display threshold θ (`theta`), narrows to any filter (`narrow_to_filters`), sweeps every
//! tile for its count and selection (`sweep_tiles`), tags the selected points with the artifacts
//! of each requested layer (`tag`), gathers and emits them (`emit_points`), and then serves the
//! annotation artifacts (`serve_artifacts`).
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
use tessera_store::tile_ranges_all;
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
mod tag;

pub use item::{ItemField, ItemOut, ItemScoped, ItemView};
pub use meta::{EngineMeta, LeafColumn, MetaGroup, MetaRoster, MetaView, TileAddress};
pub use out::{
    ArtifactOut, ColumnBuf, PointColumns, PointScalar, ScalarOut, SinkClosed, SinkResult,
    SubCellCount, TileCount, ViewCoordinates, ViewportHead, ViewportOut, ViewportSink,
};
pub use request::{
    ArtifactRows, ComputedSelection, LayerSelection, LevelSelection, PointRows, ViewportRequest,
};
pub use sweep::{segments_with_row_bases, SERIAL_FALLBACK_MAX_ROWS, TILE_PAR_MIN_TILES};

pub(crate) use artifacts::{authored_rings, response_rungs, DependencyContext, Supplied};
pub(crate) use item::{
    category_code, category_key, flushed_row_scalar, slice_value, stored_field_out,
};
pub(crate) use meta::{meta_of, owning_key_of, Resolution};
pub(crate) use row_filter::{
    crossing_domain, filter_refusal, predicate_source, predicate_vocabulary, unique_holders,
    ResolvedLeaves, RoutedRows,
};
pub(crate) use served::ServedView;
pub(crate) use sweep::{
    scoped_family_views, scoped_render_families, segment_holding, segment_row_of, RowPresence,
};

pub(crate) use geometry::OpenView;
use out::{emit_points, CollectSink, FilterBits, PointSchema};
use tag::Tagging;
use sweep::{
    resolve_scalars, scoped_render_scalars, tile_ranges, Gather, Swept, TileSweepOut, Tiling,
    MAX_POINTS_FRAME_BYTES,
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
                .map(|d| PointScalar::empty(d.arrow_type))
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

        let k = req.k.min(self.config.max_k);

        let OpenView {
            served,
            mask,
            geometry,
            stamp: answered_from,
            coordinates,
            render_scalars,
        } = self.open_view(session, &generation, req.view, &req.cancel, &mut probe)?;

        // A presented stamp that differs from the one this response is answered from sets a flag,
        // and never selects, refuses or expires.
        let stale = req
            .stamp
            .as_ref()
            .is_some_and(|presented| *presented != answered_from);
        probe.lap(|t| &mut t.stamp_compare_ns);

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

        // `point_rows` is a column projection only — the row set and tile split are unaffected —
        // so the gather reads exactly the render columns it names and no other. Under
        // `"highlight"` it reads none and builds no membership resolver: a client re-requesting
        // only the highlight is not served the payload it already holds.
        let highlight_only = req.point_rows == PointRows::Highlight && mask.has_highlight();
        let render_scalars: Vec<DeclaredScalar> = match req.point_rows {
            PointRows::Highlight if highlight_only => Vec::new(),
            PointRows::Full | PointRows::Highlight => render_scalars,
            PointRows::Columns(names) => {
                if let Some(unknown) = names
                    .iter()
                    .find(|name| !render_scalars.iter().any(|d| &d.name == *name))
                {
                    return Err(EngineError::PointRowsRefused(format!(
                        "`point_rows` names '{unknown}', which is not a render column of view \
                         '{}'; name only columns `/v1/meta` lists as rendered for this view, or \
                         write \"full\"",
                        req.view
                    )));
                }
                render_scalars
                    .into_iter()
                    .filter(|d| names.contains(&d.name))
                    .collect()
            }
        };
        let render_scalars = render_scalars.as_slice();

        // The head, delivered before the sweep: the region verdict is settled by the decomposition
        // above and never by a row. A refusal from the sink means the consumer is gone.
        sink.head(ViewportHead {
            coordinates,
            stamp: answered_from,
            stale,
            region,
            render_scalars: render_scalars.to_vec(),
            highlighted: mask.has_highlight(),
        })
        .map_err(|SinkClosed| EngineError::Cancelled)?;
        // Reset the clock so the head's delivery is not charged to the stage that follows.
        probe.skip();

        // From here to the last point, a masked-count build waits between chunks of its walk,
        // except while this request is blocked on its client.
        let drawing = self.masked_counts.drawing(&served.turn);
        let mut sending = Sending {
            sink: &mut *sink,
            cache: &self.masked_counts,
            turn: &served.turn,
        };
        #[cfg(feature = "fault-injection")]
        self.switches.hold_drawing_if_wanted();
        let Swept {
            tile_counts,
            sub_cells,
            swept,
        } = self.sweep_tiles(&served, &mask, &tiling, &params, &req, &mut probe)?;

        // The first flush: every count, before any point. `None`, not an empty slice, when the
        // underlay was not requested — the wire's frame-presence rule needs that distinction.
        sending.counts(
            &tile_counts,
            tiling.underlay_offset.map(|_| sub_cells.as_slice()),
        )
        .map_err(|SinkClosed| EngineError::Cancelled)?;
        probe.skip();

        // Each requested layer is tagged on its points from its labels where it has no lineage
        // and the request names nothing it depends on, and from the walk's served set otherwise.
        // Only a layer tagged from the walk holds the points back behind it.
        let reachable = self.reachable_layers(session);
        let names = artifacts::requested_layers(req.layers, &reachable);
        let taggings = self.taggings(&served, &names);
        let gathers = !highlight_only && swept.iter().any(|ts| !ts.rows.is_empty());
        let walked_first = if gathers && taggings.iter().any(|t| matches!(t, Tagging::Walk)) {
            Some(self.serve_artifacts(&served, &mask, &tiling, &req)?)
        } else {
            None
        };
        let membership = if gathers {
            let points = tag::tagged_points(&served, &swept);
            let ctx = DependencyContext::new(&served, &mask, &reachable);
            let dependency_served = self.dependency_gate(&ctx);
            let mut labelled = Vec::new();
            for tagging in &taggings {
                if let Tagging::Labels(registered) = tagging {
                    let tags = self.tag_layer(
                        &served,
                        &mask,
                        &req,
                        &dependency_served,
                        registered,
                        &points,
                    )?;
                    labelled.push((registered.declaration.name.clone(), tags));
                }
            }
            ctx.finish()?;
            let walk_names: Vec<&str> = names
                .iter()
                .zip(&taggings)
                .filter(|(_, tagging)| matches!(tagging, Tagging::Walk))
                .map(|(name, _)| name.as_str())
                .collect();
            let rows: Vec<u32> = points.iter().map(|p| p.row).collect();
            let mut columns = walked_first
                .as_ref()
                .map(|(_, layers)| {
                    crate::membership_column::Resolved::new(
                        rows.clone(),
                        layers
                            .iter()
                            .filter(|layer| walk_names.contains(&layer.name.as_str())),
                    )
                    .columns_for(&rows)
                })
                .unwrap_or_default();
            // A layer tagging no point has no column, as a walked layer serving nothing has none.
            columns.extend(
                labelled
                    .into_iter()
                    .filter(|(_, ids)| ids.iter().any(Option::is_some))
                    .map(|(layer, ids)| crate::membership_column::MembershipColumn { layer, ids }),
            );
            columns.sort_by_key(|column| names.iter().position(|name| *name == column.layer));
            columns
        } else {
            Vec::new()
        };
        probe.skip();

        emit_points(
            &swept,
            &PointSchema {
                render_scalars,
                segments: &served.segments,
                membership,
            },
            &mask,
            flush_bytes,
            &req.cancel,
            &mut probe,
            &mut sending,
        )?;
        drop(drawing);

        // The artifacts, after every point: no point waits on a frame its tag does not need.
        let (artifacts, _) = match walked_first {
            Some(walked) => walked,
            None => self.serve_artifacts(&served, &mask, &tiling, &req)?,
        };
        if !artifacts.is_empty() {
            sink.artifacts(&artifacts)
                .map_err(|SinkClosed| EngineError::Cancelled)?;
        }
        probe.skip();

        // `total_ns` is this call's wall clock, which under streaming includes the sink's sends —
        // consumer-paced time, not compute.
        Ok(probe.finish())
    }
}

/// A drawing request's sink: while a call blocks on the client, the request is not counted as
/// drawing, so a slow reader holds no build.
struct Sending<'a> {
    sink: &'a mut dyn ViewportSink,
    cache: &'a crate::histogram::MaskedCountCache,
    turn: &'a crate::histogram::DrawingTurn,
}

impl ViewportSink for Sending<'_> {
    fn head(&mut self, head: ViewportHead) -> SinkResult {
        let _sending = self.cache.sending(self.turn);
        self.sink.head(head)
    }

    fn counts(&mut self, tiles: &[TileCount], sub_cells: Option<&[SubCellCount]>) -> SinkResult {
        let _sending = self.cache.sending(self.turn);
        self.sink.counts(tiles, sub_cells)
    }

    fn artifacts(&mut self, artifacts: &[ArtifactOut]) -> SinkResult {
        let _sending = self.cache.sending(self.turn);
        self.sink.artifacts(artifacts)
    }

    fn points(&mut self, chunk: PointColumns) -> SinkResult {
        let _sending = self.cache.sending(self.turn);
        self.sink.points(chunk)
    }
}
