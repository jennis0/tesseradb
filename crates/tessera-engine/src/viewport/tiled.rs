//! **What things are here**: the artifacts of each requested layer that this viewer is served and
//! that hold a member the viewer can see inside a tile, tile by tile in the request's order. Within
//! one tile and one level they go by whole visible count, largest first, then by `tessera_id`, up
//! to the request's quota. An artifact with visible members in several tiles is served in each,
//! with the same whole figures.
//!
//! A level stored by row with its members ([`crate::row_members`]) proposes a tile's candidates
//! from its coverings, and the rows above its base from the column's own labels. Every proposal is
//! a candidate only: the walk takes them in order and keeps each one whose members meet the
//! viewer's visible rows inside the tile, until the quota is met. That presence test is either a
//! probe of the artifact's member bitmap against the tile's visible rows, or one scan of those rows
//! reading their labels ([`RowColumn::candidates`]); both are exact, and the walk probes until the
//! probes have cost what the scan would, then scans. A level stored any other way asks the tile
//! index, as the whole-viewport walk does, and probes every candidate it proposes.
//!
//! A treed layer (`nested`, `dag`) keeps the whole-viewport walk and its budget cut, and is served
//! as one frame with no tile, before the tiles. A dependent in a tile names a target served in that
//! tile or in the treed frame.
//!
//! Every count, centroid and box is the level's figures over the viewer's visible set
//! ([`crate::figures`]); nothing about a tile changes them.

use std::collections::BinaryHeap;

use croaring::Bitmap;
use rustc_hash::FxHashMap;

use tessera_types::layer::ComputedProperty;

use super::artifacts::{
    level_is_selected, lineage_kind, requested_layers, settle_response, tile_rows, viewport_sets,
    ArtifactPass, LayerPass, LevelPass, Passing, ViewportSets, Walked,
};
use super::*;
use crate::artifact_content::LevelContent;
use crate::artifacts::{ArtifactVerdict, ArtifactView};
use crate::row_column::RowColumn;
use crate::row_members::LevelMembers;

/// What one probe of a member bitmap against a tile's visible rows is taken to cost, in rows of
/// the scan it competes with, for each 2¹⁶-row chunk the tile spans below the base.
const PROBE_ROWS_PER_CHUNK: u64 = 256;

/// One `POST /v1/artifacts/viewport` request, as the engine sees it. Construct with
/// [`ViewportArtifactsRequest::new`] and add the optional parts.
#[derive(Debug, Clone)]
pub struct ViewportArtifactsRequest<'a> {
    /// A view id from `GET /v1/meta`.
    pub view: &'a str,
    /// Tile depth, 0–16.
    pub zoom: u8,
    /// `[x0, y0, x1, y1]` in the view's extent. Ignored when `tiles` is present.
    pub bbox: [f64; 4],
    /// The depth-`zoom` Morton prefixes to answer for, in this order, in place of `bbox`'s.
    pub tiles: Option<&'a [u64]>,
    /// The stamp of the response the client holds; see [`ViewportRequest::stamp`].
    pub stamp: Option<GenerationStamp>,
    /// Checked between tiles and once per artifact served.
    pub cancel: Option<CancelToken>,
    /// Sets each artifact's `matched` bit, over its visible members inside its tile.
    pub filter: Option<crate::filter::FilterExpr>,
    /// Sets each artifact's `highlighted` bit, over its visible members inside its tile that the
    /// filter also admits.
    pub highlight: Option<crate::filter::FilterExpr>,
    /// The layers to answer for, intersected with what this principal reaches.
    pub layers: LayerSelection<'a>,
    /// Which of each layer's levels to answer for.
    pub levels: LevelSelection<'a>,
    /// Which of `centroid` and `box` to answer with, narrowing each layer's declaration. A shape
    /// is never served here.
    pub computed: ComputedSelection<'a>,
    /// The budget a treed layer's walk is cut to.
    pub budget: Option<u32>,
    /// The most artifacts one level serves in one tile.
    pub per_tile: usize,
}

impl<'a> ViewportArtifactsRequest<'a> {
    /// Every layer the principal reaches, at the levels each declares for `zoom`, with what each
    /// declares computed.
    pub fn new(view: &'a str, zoom: u8, bbox: [f64; 4], per_tile: usize) -> Self {
        ViewportArtifactsRequest {
            view,
            zoom,
            bbox,
            tiles: None,
            stamp: None,
            cancel: None,
            filter: None,
            highlight: None,
            layers: LayerSelection::All,
            levels: LevelSelection::Declared,
            computed: ComputedSelection::Declared,
            budget: None,
            per_tile,
        }
    }

    pub fn tiles(mut self, tiles: Option<&'a [u64]>) -> Self {
        self.tiles = tiles;
        self
    }

    pub fn stamp(mut self, stamp: Option<GenerationStamp>) -> Self {
        self.stamp = stamp;
        self
    }

    pub fn cancel(mut self, cancel: Option<CancelToken>) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn filter(mut self, filter: crate::filter::FilterExpr) -> Self {
        self.filter = Some(filter);
        self
    }

    pub fn highlight(mut self, highlight: crate::filter::FilterExpr) -> Self {
        self.highlight = Some(highlight);
        self
    }

    pub fn layers(mut self, layers: LayerSelection<'a>) -> Self {
        self.layers = layers;
        self
    }

    pub fn levels(mut self, levels: LevelSelection<'a>) -> Self {
        self.levels = levels;
        self
    }

    pub fn computed(mut self, computed: ComputedSelection<'a>) -> Self {
        self.computed = computed;
        self
    }

    pub fn budget(mut self, budget: Option<u32>) -> Self {
        self.budget = budget;
        self
    }
}

/// What the response headers need, known before any tile is walked.
#[derive(Debug, Clone)]
pub struct ViewportArtifactsHead {
    pub coordinates: ViewCoordinates,
    /// The geometry this response is answered from.
    pub stamp: GenerationStamp,
    /// Whether the request presented a stamp other than [`Self::stamp`].
    pub stale: bool,
    /// The region leaves' verdict, where the request's filter or highlight carried one.
    pub region: Option<crate::region::RegionVerdict>,
}

/// One frame of the response: one tile's artifacts, or the treed layers' with no tile.
#[derive(Debug, Clone)]
pub struct ArtifactsFrame {
    /// The tile's Morton prefix at the request's depth; `None` for the treed layers' frame.
    pub tile: Option<u64>,
    pub artifacts: Vec<ArtifactOut>,
}

/// The whole response, collected.
#[derive(Debug, Clone)]
pub struct ViewportArtifactsOut {
    pub coordinates: ViewCoordinates,
    pub stamp: GenerationStamp,
    pub stale: bool,
    pub region: Option<crate::region::RegionVerdict>,
    /// The treed frame, where it holds a row, then one frame per tile in the request's order.
    pub frames: Vec<ArtifactsFrame>,
}

impl ViewportArtifactsOut {
    /// Every artifact served anywhere in the response, once, in the order first served.
    pub fn artifacts(&self) -> Vec<ArtifactOut> {
        let mut seen = std::collections::HashSet::new();
        self.frames
            .iter()
            .flat_map(|frame| &frame.artifacts)
            .filter(|artifact| seen.insert(artifact.tessera_id))
            .cloned()
            .collect()
    }
}

/// Where [`Engine::viewport_artifacts_stream`] delivers a response: `head` once, then each frame
/// in order. Returning `Ok` is the completeness signal. A refusal aborts the request as a
/// cancellation.
pub trait ViewportArtifactsSink {
    fn head(&mut self, head: ViewportArtifactsHead) -> SinkResult;
    /// The treed layers' frame (`tile` `None`), only where it holds a row; then every tile's,
    /// empty or not.
    fn frame(&mut self, tile: Option<u64>, artifacts: &[ArtifactOut]) -> SinkResult;
}

#[derive(Default)]
struct CollectFrames {
    head: Option<ViewportArtifactsHead>,
    frames: Vec<ArtifactsFrame>,
}

impl ViewportArtifactsSink for CollectFrames {
    fn head(&mut self, head: ViewportArtifactsHead) -> SinkResult {
        self.head = Some(head);
        Ok(())
    }

    fn frame(&mut self, tile: Option<u64>, artifacts: &[ArtifactOut]) -> SinkResult {
        self.frames.push(ArtifactsFrame {
            tile,
            artifacts: artifacts.to_vec(),
        });
        Ok(())
    }
}

/// One level's answers carried from tile to tile: each verdict asked so far, and the level's
/// supplied contents.
struct LevelState {
    /// By ordinal: the identifier and what the gate passed, or `None` where it is withheld.
    admitted: FxHashMap<u32, Option<(u64, Passing)>>,
    contents: Option<Arc<LevelContent>>,
}

/// Which artifacts of one level hold a member the viewer can see in one tile: a probe of each
/// artifact's members against the tile's visible rows, until the probes have cost what one scan of
/// those rows would, and that scan after.
struct Presence<'a> {
    members: &'a LevelMembers,
    column: &'a RowColumn,
    /// The tile's visible rows.
    here: &'a Bitmap,
    /// The artifacts labelling a visible row of the tile at or above the members' base, which the
    /// column holds and the members do not.
    above: Bitmap,
    scanned: Option<Bitmap>,
    /// Each probe's cost, and what has been spent, against the scan's.
    probe: u64,
    spent: u64,
    scan: u64,
}

impl Presence<'_> {
    fn holds(&mut self, ordinal: u32) -> bool {
        if self.scanned.is_none() && self.spent + self.probe > self.scan {
            self.scanned = Some(self.column.candidates(self.here));
        }
        if let Some(scanned) = &self.scanned {
            return scanned.contains(ordinal);
        }
        self.spent += self.probe;
        self.above.contains(ordinal) || self.members.members(ordinal).intersects(self.here)
    }

}

/// The artifacts of `column` labelling a row of `set` at or above `base`, the rows a level's
/// members do not hold.
fn labelled_above(column: &RowColumn, set: &Bitmap, base: u32) -> Bitmap {
    match set.maximum() {
        Some(last) if last >= base => column.candidates(&set.and(&Bitmap::from_range(base..=last))),
        _ => Bitmap::new(),
    }
}

impl Engine {
    /// [`Self::viewport_artifacts_stream`] run into a collecting sink.
    pub fn viewport_artifacts(
        &self,
        session: &Session,
        req: ViewportArtifactsRequest<'_>,
    ) -> Result<ViewportArtifactsOut> {
        let mut sink = CollectFrames::default();
        self.viewport_artifacts_stream(session, req, &mut sink)?;
        let head = sink
            .head
            .expect("viewport_artifacts_stream delivers a head before returning Ok");
        Ok(ViewportArtifactsOut {
            coordinates: head.coordinates,
            stamp: head.stamp,
            stale: head.stale,
            region: head.region,
            frames: sink.frames,
        })
    }

    /// The artifacts here, tile by tile — see the module doc. The view, the mask, the tiles and
    /// the filter are resolved as [`Self::viewport_stream`] resolves them, and every refusal is
    /// made before the head. Each level's figures are read before the first frame.
    pub fn viewport_artifacts_stream(
        &self,
        session: &Session,
        mut req: ViewportArtifactsRequest<'_>,
        sink: &mut dyn ViewportArtifactsSink,
    ) -> Result<()> {
        let mut probe = Probe::new();
        let generation = self.generation.load_full();
        let OpenView {
            served,
            mask,
            stamp: answered_from,
            coordinates,
            ..
        } = self.open_view(session, &generation, req.view, &req.cancel, &mut probe)?;
        let stale = req
            .stamp
            .as_ref()
            .is_some_and(|presented| *presented != answered_from);

        // The viewport's own resolution of the tiles and the filter, so the two routes cannot
        // disagree about which rows a tile holds or which of them a filter admits.
        let mut tiles_req = ViewportRequest::new(req.view, req.zoom, req.bbox, 0)
            .tiles(req.tiles)
            .cancel(req.cancel.clone());
        tiles_req.filter = req.filter.take();
        tiles_req.highlight = req.highlight.take();
        let tiles = self.resolve_tiles(&served, &tiles_req, &mut probe)?;
        let tiling = tile_ranges(tiles, &served.segments, &mut probe);
        check_cancelled(&req.cancel)?;
        let v_total = mask.visible_total();
        let (mask, region) =
            self.narrow_to_filters(&served, mask, &tiling, v_total, &tiles_req, &mut probe)?;

        sink.head(ViewportArtifactsHead {
            coordinates,
            stamp: answered_from,
            stale,
            region,
        })
        .map_err(|SinkClosed| EngineError::Cancelled)?;

        let computed: Vec<ComputedProperty> = [ComputedProperty::Centroid, ComputedProperty::Box]
            .into_iter()
            .filter(|property| req.computed.selects(*property))
            .collect();
        let ask = ArtifactAsk {
            zoom: req.zoom,
            levels: req.levels,
            computed: ComputedSelection::Named(&computed),
            budget: req.budget,
            rows: ArtifactRows::Full,
            cancel: req.cancel.clone(),
        };
        let reachable = self.reachable_layers(session);
        let names = requested_layers(req.layers, &reachable);
        let in_request: std::collections::BTreeSet<String> = names.iter().cloned().collect();
        let (treed, tiled): (Vec<String>, Vec<String>) = names.into_iter().partition(|name| {
            self.write
                .live()
                .registered_layer(name)
                .is_some_and(|layer| lineage_kind(layer.declaration.hierarchy.kind).is_some())
        });
        let ctx = DependencyContext::new(&served, &mask, &reachable);
        let dependency_served = self.dependency_gate(&ctx);

        // The treed frame first, so a dependent in a tile can name a target in it.
        let treed_in_request: std::collections::BTreeSet<String> = treed.iter().cloned().collect();
        let treed_frame = self.serve_artifacts(
            &served,
            &mask,
            &tiling,
            &ask,
            treed,
            &treed_in_request,
            &dependency_served,
        )?;
        ctx.finish()?;
        if !treed_frame.out.is_empty() {
            sink.frame(None, &treed_frame.out)
                .map_err(|SinkClosed| EngineError::Cancelled)?;
        }
        let outside = treed_frame.served_at;

        let locator = crate::derived::RowLocator::new(served.segments.clone());
        let source = generation.partition_source();
        let pass = ArtifactPass {
            served: &served,
            ask: &ask,
            mask: &mask,
            dependency_served: &dependency_served,
            locator: &locator,
            source: &source,
            shard: generation.bundle.manifest.identity.shard_id,
        };
        let layers: Vec<LayerPass<'_>> = tiled
            .into_iter()
            .filter_map(|name| self.layer_pass(&pass, name))
            .collect();
        let mut levels: Vec<LevelPass<'_>> = Vec::new();
        for layer in &layers {
            for (number, runs) in layer.registered.runs.iter().enumerate() {
                let number = number as u32;
                if level_is_selected(req.levels, &layer.registered.declaration.levels, number, req.zoom)
                {
                    levels.push(self.level_pass(layer, number, runs)?);
                }
            }
        }
        ctx.finish()?;
        let views: Vec<ArtifactView<'_, EffectiveMask>> =
            levels.iter().map(|level| self.level_view(level)).collect();
        let mut states: Vec<LevelState> = levels
            .iter()
            .map(|level| LevelState {
                admitted: FxHashMap::default(),
                contents: self.level_contents_of(level, true),
            })
            .collect();

        let row_bases: Vec<u32> = served.segments.iter().map(|&(_, base)| base).collect();
        for (at, tile) in tiling.tiles.iter().enumerate() {
            check_cancelled(&req.cancel)?;
            let parts = std::slice::from_ref(&tiling.ranges[at]);
            let mut walked = Walked::default();
            if let Some(rows) = tile_rows(&served, parts) {
                let spans = crossing_domain(parts, &row_bases);
                let sets = viewport_sets(&rows, &mask);
                for ((level, view), state) in levels.iter().zip(&views).zip(&mut states) {
                    self.serve_tile_level(level, view, state, &sets, &spans, req.per_tile, &mut walked)?;
                }
            }
            ctx.finish()?;
            let settled = settle_response(walked, &in_request, &outside);
            sink.frame(Some(tile.prefix), &settled.out)
                .map_err(|SinkClosed| EngineError::Cancelled)?;
        }
        Ok(())
    }

    /// One level's artifacts in one tile: the first `per_tile` of those served and present there,
    /// by whole visible count and then `tessera_id`, assembled into `walked` with their filter
    /// bits taken inside the tile.
    #[allow(clippy::too_many_arguments)]
    fn serve_tile_level(
        &self,
        level: &LevelPass<'_>,
        view: &ArtifactView<'_, EffectiveMask>,
        state: &mut LevelState,
        sets: &ViewportSets<'_>,
        spans: &[Range<u32>],
        per_tile: usize,
        walked: &mut Walked,
    ) -> Result<()> {
        let rows: &crate::artifacts::ArtifactRows = &level.rows;
        let stored = rows
            .column()
            .and_then(|column| Some((column, column.members()?, level.counts.as_deref()?)));
        let chosen: Vec<(Passing, FilterBits)> = match stored {
            Some((column, members, counts)) => {
                let here = sets.viewport.here();
                let base = members.base_rows();
                let chunks: u64 = spans
                    .iter()
                    .filter(|span| span.start < base)
                    .map(|span| {
                        let last = span.end.min(base) - 1;
                        u64::from((last >> 16) - (span.start >> 16) + 1)
                    })
                    .sum();
                let mut presence = Presence {
                    members,
                    column,
                    here,
                    above: labelled_above(column, here, base),
                    scanned: None,
                    probe: chunks * PROBE_ROWS_PER_CHUNK,
                    spent: 0,
                    scan: here.cardinality(),
                };
                let mut candidates = presence.above.clone();
                for span in spans.iter().filter(|span| span.start < base) {
                    candidates.or_inplace(&members.overlapping(span.start..=span.end.min(base) - 1));
                }
                let chosen = self.by_count(level, view, state, counts, &candidates, per_tile, |o| {
                    presence.holds(o)
                });
                // The filter's and the highlight's rows in the tile, each with the artifacts its rows
                // above the base carry.
                let narrowed = |set: &Option<Bitmap>| {
                    set.as_ref()
                        .map(|set| (set.clone(), labelled_above(column, set, base)))
                };
                let (matched, highlighted) =
                    (narrowed(&sets.matched_here), narrowed(&sets.highlighted_here));
                let holds = |ordinal: u32, set: &Option<(Bitmap, Bitmap)>| {
                    set.as_ref().map(|(rows, above)| {
                        above.contains(ordinal) || members.members(ordinal).intersects(rows)
                    })
                };
                chosen
                    .into_iter()
                    .map(|passing| {
                        let bits = (holds(passing.0, &matched), holds(passing.0, &highlighted));
                        (passing, bits)
                    })
                    .collect()
            }
            None => {
                let chosen = self.by_index(level, view, state, sets, per_tile);
                let matched = sets.matched_here.as_ref().map(|here| rows.matched(here));
                let highlighted = sets.highlighted_here.as_ref().map(|here| rows.matched(here));
                chosen
                    .into_iter()
                    .map(|passing| {
                        let bits = (
                            matched.as_ref().map(|m| rows.matches(m, passing.0)),
                            highlighted.as_ref().map(|m| rows.matches(m, passing.0)),
                        );
                        (passing, bits)
                    })
                    .collect()
            }
        };
        let keys = self.keys_of(level, chosen.iter().map(|(passing, _)| passing.0));
        for (key, (passing, bits)) in keys.into_iter().zip(chosen) {
            self.assemble_one(level, state.contents.as_deref(), key, passing, bits, walked)?;
        }
        Ok(())
    }

    /// The first `per_tile` of `candidates` that are served and `present`, by the level's figures:
    /// count descending, then `tessera_id`. Candidates are taken a count at a time, so a verdict
    /// and a presence test are paid only down to the count that fills the quota.
    #[allow(clippy::too_many_arguments)]
    fn by_count(
        &self,
        level: &LevelPass<'_>,
        view: &ArtifactView<'_, EffectiveMask>,
        state: &mut LevelState,
        counts: &crate::figures::Figures,
        candidates: &Bitmap,
        per_tile: usize,
        mut present: impl FnMut(u32) -> bool,
    ) -> Vec<Passing> {
        let mut heap: BinaryHeap<(u64, u32)> = candidates
            .iter()
            .filter_map(|ordinal| {
                let count = counts.get(ordinal);
                (count > 0).then_some((count, ordinal))
            })
            .collect();
        let mut chosen = Vec::new();
        let mut group: Vec<u32> = Vec::new();
        while chosen.len() < per_tile {
            let Some((count, first)) = heap.pop() else {
                break;
            };
            group.clear();
            group.push(first);
            while heap.peek().is_some_and(|&(next, _)| next == count) {
                group.extend(heap.pop().map(|(_, ordinal)| ordinal));
            }
            let mut ranked: Vec<(u64, Passing)> = group
                .iter()
                .filter_map(|&ordinal| self.admitted(level, view, state, ordinal))
                .collect();
            ranked.sort_unstable_by_key(|&(id, _)| id);
            for (_, passing) in ranked {
                if chosen.len() == per_tile {
                    break;
                }
                if present(passing.0) {
                    chosen.push(passing);
                }
            }
        }
        chosen
    }

    /// The first `per_tile` artifacts the tile index proposes in the tile that hold a visible
    /// member there and are served, by whole visible count and then `tessera_id`.
    fn by_index(
        &self,
        level: &LevelPass<'_>,
        view: &ArtifactView<'_, EffectiveMask>,
        state: &mut LevelState,
        sets: &ViewportSets<'_>,
        per_tile: usize,
    ) -> Vec<Passing> {
        let rows: &crate::artifacts::ArtifactRows = &level.rows;
        let candidates = rows.candidacy(&sets.viewport, level.counts.as_deref());
        let mut ranked: Vec<(u64, u64, Passing)> = Vec::new();
        for ordinal in candidates.iter() {
            // An artifact its own label withholds has no membership probed.
            if !view.admits_label(ordinal) {
                continue;
            }
            if !rows.candidate_in(ordinal, &candidates, &sets.viewport, level.layer.pass.mask) {
                continue;
            }
            if let Some((id, passing)) = self.admitted(level, view, state, ordinal) {
                ranked.push((passing.2, id, passing));
            }
        }
        ranked.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        ranked.truncate(per_tile);
        ranked.into_iter().map(|(_, _, passing)| passing).collect()
    }

    /// The artifact at `ordinal` where this viewer is served it, its content can be read back and
    /// its identifier formed, as the assembly will need them: its `tessera_id` and what the
    /// verdict passed. Asked once per request and level.
    fn admitted(
        &self,
        level: &LevelPass<'_>,
        view: &ArtifactView<'_, EffectiveMask>,
        state: &mut LevelState,
        ordinal: u32,
    ) -> Option<(u64, Passing)> {
        let contents = state.contents.as_deref();
        *state.admitted.entry(ordinal).or_insert_with(|| {
            let pass = level.layer.pass;
            let entity = level.runs.entity_of(u64::from(ordinal)).map(EntityId::new)?;
            let ArtifactVerdict::Serve { masked_count, rank } = view.verdict(entity, ordinal) else {
                return None;
            };
            self.supplied_content(
                pass.served.generation,
                &level.layer.registered.declaration,
                level.level,
                ordinal,
                entity,
                rank,
                false,
                contents,
            )?;
            let id = self.identity_key.forward(pass.shard, entity).ok()?.raw();
            Some((id, (ordinal, entity, masked_count, rank)))
        })
    }
}
