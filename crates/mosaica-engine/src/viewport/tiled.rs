//! **What things are here**: the artifacts of each requested layer that this viewer is served and
//! that hold a member the viewer can see inside a tile, tile by tile in the request's order. Within
//! one tile and one level they go by whole visible count, largest first, then by `mosaica_id`, up
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
//! index, as the whole-viewport walk does, and probes every candidate it proposes. Each shard the
//! viewer sees anything in proposes and probes its own slice of the level in the tile; an artifact
//! is present where any shard finds a visible member, and the quota is filled once over the union.
//!
//! A treed layer (`nested`, `dag`) keeps the whole-viewport walk and its budget cut, and is served
//! as one frame with no tile, before the tiles. A dependent in a tile names a target served in that
//! tile or in the treed frame.
//!
//! Every count, centroid and box is the level's figures over the viewer's visible set in every
//! shard ([`crate::figures::LevelFigures`]); nothing about a tile changes them.

use std::collections::BinaryHeap;

use croaring::Bitmap;
use rustc_hash::FxHashMap;

use mosaica_types::layer::ComputedProperty;

use super::artifacts::{
    level_is_selected, lineage_kind, matched_sides, matches_any, present_in, requested_layers,
    settle_response, spans_rows, viewport_sets, ArtifactPass, LayerPass, LevelPass, Outside,
    Passing, ViewportSets, Walked,
};
use super::*;
use crate::artifact_content::LevelContent;
use crate::artifacts::{ArtifactVerdict, ArtifactView};
use crate::figures::LevelFigures;
use crate::layer_read::LevelForms;
use crate::row_column::RowColumn;
use crate::row_members::LevelMembers;

/// What one probe of a member bitmap against a tile's visible rows is taken to cost, in rows of
/// the scan it competes with, for each 2¹⁶-row chunk the tile spans below the base. A probe that
/// finds nothing measures 10 to 180 (`tests::probe_against_scan`), dearer in smaller tiles; one
/// that finds a member stops sooner.
const PROBE_ROWS_PER_CHUNK: u64 = 64;

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
    /// The palette size each artifact's slot is chosen for ([`crate::slots`]); `None` serves no
    /// slot.
    pub palette_size: Option<u8>,
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
            palette_size: None,
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

    pub fn palette_size(mut self, palette_size: Option<u8>) -> Self {
        self.palette_size = palette_size;
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
            .filter(|artifact| seen.insert(artifact.mosaica_id))
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
    /// By ordinal: what the gate passed, or `None` where it is withheld.
    admitted: FxHashMap<u32, Option<Arc<Admitted>>>,
    contents: Option<Arc<LevelContent>>,
}

/// An artifact the gate passed, its identifier formed and its content read: what its assembly
/// needs beyond its tile.
struct Admitted {
    mosaica_id: u64,
    passing: Passing,
    content: Vec<String>,
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
/// members do not hold. Usually there are none, and a scan of few rows costs what they do
/// ([`RowColumn::candidates_cost`]).
fn labelled_above(column: &RowColumn, set: &Bitmap, base: u32) -> Bitmap {
    let above = match set.maximum() {
        Some(last) if last >= base => set.and(&Bitmap::from_range(base..=last)),
        _ => return Bitmap::new(),
    };
    match above.is_empty() {
        true => Bitmap::new(),
        false => column.candidates(&above),
    }
}

/// What one tile is answered over, and what the frame it builds may name.
struct TileAsk<'a> {
    /// The tile's sets in each side the request reads and the tile spans a row of.
    sets: &'a [ViewportSets<'a>],
    /// The rows the tile spans in each side holding a part of it, ascending by place.
    spans: &'a [(usize, Vec<Range<u32>>)],
    per_tile: usize,
    /// The layers the request names: a dependent whose target's layer is among them is served only
    /// where the target is.
    in_request: &'a std::collections::BTreeSet<String>,
    /// The treed frame's rows.
    outside: &'a Outside,
}

/// One side's rows a request's filter, or its highlight, admits in a tile: the side's members, the
/// rows, and the artifacts those rows carry at or above the members' base.
type Admitting<'a> = (&'a LevelMembers, &'a Bitmap, Bitmap);

/// What [`Admitting`] is in each side of `sets`, by the rows `admitted` gives there, or `None`
/// where the request carries no such expression. `columns` holds each side's column and members,
/// in the order of the view's sides.
fn admitted_in<'a>(
    sets: &'a [ViewportSets<'_>],
    columns: &[(&'a RowColumn, &'a LevelMembers)],
    admitted: impl Fn(&'a ViewportSets<'_>) -> Option<&'a Bitmap>,
) -> Option<Vec<Admitting<'a>>> {
    let sides: Vec<Admitting<'a>> = sets
        .iter()
        .filter_map(|set| {
            let rows = admitted(set)?;
            let (column, members) = columns[set.place];
            Some((members, rows, labelled_above(column, rows, members.base_rows())))
        })
        .collect();
    (!sides.is_empty()).then_some(sides)
}

impl TileAsk<'_> {
    /// The rows the tile spans in the side at `place`.
    fn spans_in(&self, place: usize) -> &[Range<u32>] {
        self.spans
            .binary_search_by_key(&place, |(held, _)| *held)
            .map_or(&[][..], |at| self.spans[at].1.as_slice())
    }
}

/// Each side's column and members, indexed like `served.sides`, and the level's figures over
/// every shard: what a tile of a level stored by row with its members is answered from.
type Stored<'a> = (Vec<(&'a RowColumn, &'a LevelMembers)>, &'a LevelFigures);

/// [`Stored`] for a level where every side's form has a column holding its members and the
/// level's figures are summed; `None` otherwise, and the tile index answers.
fn stored(forms: &LevelForms) -> Option<Stored<'_>> {
    let figures = forms.figures.as_ref()?;
    let columns = forms
        .rows
        .iter()
        .map(|rows| {
            let column = rows.column()?;
            Some((column, column.members()?))
        })
        .collect::<Option<Vec<_>>>()?;
    Some((columns, figures))
}

/// The first `per_tile` artifacts of a level stored by row with its members that `admitted`
/// serves, `targeted` keeps and that hold a visible member in `tile`, by [`by_count`]. Each side
/// of `tile.sets` proposes the artifacts its members cover in the tile's spans there and those
/// labelling its rows above the members' base, and answers presence for its own rows; the quota is
/// filled once over every side's proposals.
fn chosen_stored<A>(
    tile: &TileAsk<'_>,
    columns: &[(&RowColumn, &LevelMembers)],
    counts: &LevelFigures,
    admitted: impl FnMut(u32) -> Option<(u64, A)>,
    targeted: &dyn Fn(u32) -> bool,
) -> Vec<A> {
    let mut presences: Vec<Presence<'_>> = Vec::with_capacity(tile.sets.len());
    let mut candidates = Bitmap::new();
    for set in tile.sets {
        let (column, members) = columns[set.place];
        let spans = tile.spans_in(set.place);
        let here = set.viewport.here();
        let base = members.base_rows();
        let chunks: u64 = spans
            .iter()
            .filter(|span| span.start < base)
            .map(|span| {
                let last = span.end.min(base) - 1;
                u64::from((last >> 16) - (span.start >> 16) + 1)
            })
            .sum();
        let presence = Presence {
            members,
            column,
            here,
            above: labelled_above(column, here, base),
            scanned: None,
            probe: chunks * PROBE_ROWS_PER_CHUNK,
            spent: 0,
            scan: column.candidates_cost(here.cardinality()),
        };
        candidates.or_inplace(&presence.above);
        for span in spans.iter().filter(|span| span.start < base) {
            candidates.or_inplace(&members.overlapping(span.start..=span.end.min(base) - 1));
        }
        presences.push(presence);
    }
    by_count(
        &candidates,
        counts,
        tile.per_tile,
        admitted,
        targeted,
        |ordinal| presences.iter_mut().any(|presence| presence.holds(ordinal)),
    )
}

/// The first `per_tile` of `candidates` that `admitted` serves, with its `mosaica_id`, that
/// `targeted` keeps and that are `present` in the tile, by the level's figures over every shard:
/// count descending, then `mosaica_id`. Candidates are taken a count at a time, so a verdict and a
/// presence test are paid only down to the count that fills the quota, and a candidate `targeted`
/// drops takes no place in it.
fn by_count<A>(
    candidates: &Bitmap,
    counts: &LevelFigures,
    per_tile: usize,
    mut admitted: impl FnMut(u32) -> Option<(u64, A)>,
    targeted: &dyn Fn(u32) -> bool,
    mut present: impl FnMut(u32) -> bool,
) -> Vec<A> {
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
        let mut ranked: Vec<(u64, u32, A)> = group
            .iter()
            .filter_map(|&ordinal| {
                let (id, admitted) = admitted(ordinal)?;
                Some((id, ordinal, admitted))
            })
            .filter(|&(_, ordinal, _)| targeted(ordinal))
            .collect();
        ranked.sort_unstable_by_key(|&(id, ..)| id);
        for (_, ordinal, admitted) in ranked {
            if chosen.len() == per_tile {
                break;
            }
            if present(ordinal) {
                chosen.push(admitted);
            }
        }
    }
    chosen
}

/// `layers`, each after every layer of them it depends on, so a tile's dependents are chosen
/// knowing which targets it holds. Otherwise in the request's order.
fn targets_first(mut layers: Vec<LayerPass<'_>>) -> Vec<LayerPass<'_>> {
    let mut ordered: Vec<LayerPass<'_>> = Vec::with_capacity(layers.len());
    while !layers.is_empty() {
        let ready = layers
            .iter()
            .position(|layer| {
                layer
                    .registered
                    .declaration
                    .depends_on
                    .iter()
                    .all(|target| !layers.iter().any(|waiting| &waiting.name == target))
            })
            .unwrap_or(0);
        ordered.push(layers.remove(ready));
    }
    ordered
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
        let tiling = self.tiling(&served, &tiles_req, &mut probe)?;
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
            palette: req.palette_size,
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

        let pass = ArtifactPass::new(&served, &ask, &mask, &dependency_served);
        let mut passes: Vec<LayerPass<'_>> = Vec::new();
        for name in tiled {
            passes.extend(self.layer_pass(&pass, name)?);
        }
        let layers = targets_first(passes);
        let mut levels: Vec<LevelPass<'_>> = Vec::new();
        for layer in &layers {
            for (number, runs) in layer.registered.runs.iter().enumerate() {
                let number = number as u32;
                if level_is_selected(
                    req.levels,
                    &layer.registered.declaration.levels,
                    number,
                    req.zoom,
                ) {
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
        let stored: Vec<Option<Stored<'_>>> =
            levels.iter().map(|level| stored(&level.forms)).collect();

        for (at, tile) in tiling.tiles.iter().enumerate() {
            check_cancelled(&req.cancel)?;
            // Only the sides the request reads.
            let spans: Vec<(usize, Vec<Range<u32>>)> = tiling
                .tile_domains(at)
                .into_iter()
                .filter(|(place, _)| served.leaf(&mask, *place).is_some())
                .collect();
            let rows: Vec<(usize, Bitmap)> = spans
                .iter()
                .map(|(place, spans)| (*place, spans_rows(spans)))
                .filter(|(_, rows)| !rows.is_empty())
                .collect();
            let mut walked = Walked::default();
            if !rows.is_empty() {
                let sets = viewport_sets(&served, &rows, &mask);
                let tile = TileAsk {
                    sets: &sets,
                    spans: &spans,
                    per_tile: req.per_tile,
                    in_request: &in_request,
                    outside: &outside,
                };
                for (((level, view), state), stored) in
                    levels.iter().zip(&views).zip(&mut states).zip(&stored)
                {
                    self.serve_tile_level(level, view, state, stored.as_ref(), &tile, &mut walked)?;
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
    /// by whole visible count and then `mosaica_id`, assembled into `walked` with their filter
    /// bits taken inside the tile. A dependent whose target the frame does not hold takes no place
    /// in the quota, since the frame would drop it.
    fn serve_tile_level(
        &self,
        level: &LevelPass<'_>,
        view: &ArtifactView<'_, EffectiveMask>,
        state: &mut LevelState,
        stored: Option<&Stored<'_>>,
        tile: &TileAsk<'_>,
        walked: &mut Walked,
    ) -> Result<()> {
        let (sets, per_tile) = (tile.sets, tile.per_tile);
        let forms = &level.forms;
        let first: &crate::artifacts::ArtifactRows = forms.first();
        let held: &Walked = walked;
        let targeted = |ordinal: u32| match first.attachment(ordinal) {
            Some(target) if tile.in_request.contains(&target.layer) => {
                let at = (target.layer.clone(), target.level, target.ordinal);
                held.serves(&at) || tile.outside.contains_key(&at)
            }
            _ => true,
        };
        let chosen: Vec<(Arc<Admitted>, FilterBits)> = match stored {
            Some((columns, counts)) => {
                let admitted = |ordinal: u32| {
                    self.admitted(level, view, state, ordinal)
                        .map(|admitted| (admitted.mosaica_id, admitted))
                };
                let chosen = chosen_stored(tile, columns, counts, admitted, &targeted);
                // The filter's and the highlight's rows in the tile, each with the artifacts its
                // rows above the base carry.
                let matched = admitted_in(sets, columns, |set| set.matched_here.as_ref());
                let highlighted = admitted_in(sets, columns, |set| set.highlighted_here.as_ref());
                let holds = |ordinal: u32, sides: Option<&[Admitting<'_>]>| {
                    sides.map(|sides| {
                        sides.iter().any(|(members, rows, above)| {
                            above.contains(ordinal) || members.members(ordinal).intersects(rows)
                        })
                    })
                };
                chosen
                    .into_iter()
                    .map(|admitted| {
                        let ordinal = admitted.passing.0;
                        let bits = (
                            holds(ordinal, matched.as_deref()),
                            holds(ordinal, highlighted.as_deref()),
                        );
                        (admitted, bits)
                    })
                    .collect()
            }
            None => {
                let chosen = self.by_index(level, view, state, sets, per_tile, &targeted);
                let matched = matched_sides(forms, sets, |set| set.matched_here.as_ref());
                let highlighted = matched_sides(forms, sets, |set| set.highlighted_here.as_ref());
                chosen
                    .into_iter()
                    .map(|admitted| {
                        let ordinal = admitted.passing.0;
                        let bits = (
                            matches_any(forms, matched.as_deref(), ordinal),
                            matches_any(forms, highlighted.as_deref(), ordinal),
                        );
                        (admitted, bits)
                    })
                    .collect()
            }
        };
        let keys = self.keys_of(level, chosen.iter().map(|(admitted, _)| admitted.passing.0));
        for (key, (admitted, bits)) in keys.into_iter().zip(chosen) {
            self.assemble_one(
                level,
                admitted.content.clone(),
                key,
                admitted.passing,
                bits,
                walked,
            )?;
        }
        Ok(())
    }

    /// The first `per_tile` artifacts the tile index proposes in the tile that hold a visible
    /// member there and are served and `targeted`, by whole visible count and then `mosaica_id`.
    fn by_index(
        &self,
        level: &LevelPass<'_>,
        view: &ArtifactView<'_, EffectiveMask>,
        state: &mut LevelState,
        sets: &[ViewportSets<'_>],
        per_tile: usize,
        targeted: &dyn Fn(u32) -> bool,
    ) -> Vec<Arc<Admitted>> {
        let present = present_in(&level.forms, sets, |ordinal| view.admits_label(ordinal));
        let mut ranked: Vec<Arc<Admitted>> = Vec::new();
        for ordinal in present.iter() {
            if let Some(admitted) = self.admitted(level, view, state, ordinal) {
                if targeted(ordinal) {
                    ranked.push(admitted);
                }
            }
        }
        ranked.sort_unstable_by(|a, b| {
            (b.passing.2.cmp(&a.passing.2)).then(a.mosaica_id.cmp(&b.mosaica_id))
        });
        ranked.truncate(per_tile);
        ranked
    }

    /// The artifact at `ordinal` where this viewer is served it, its content can be read back and
    /// its identifier formed, with both. Asked once per request and level.
    fn admitted(
        &self,
        level: &LevelPass<'_>,
        view: &ArtifactView<'_, EffectiveMask>,
        state: &mut LevelState,
        ordinal: u32,
    ) -> Option<Arc<Admitted>> {
        let contents = state.contents.as_deref();
        let admitted = state.admitted.entry(ordinal).or_insert_with(|| {
            let pass = level.layer.pass;
            let entity = level
                .runs
                .entity_of(u64::from(ordinal))
                .map(EntityId::new)?;
            let ArtifactVerdict::Serve { masked_count, rank } = view.verdict(entity, ordinal)
            else {
                return None;
            };
            let supplied = self.supplied_content(
                pass.served
                    .generation
                    .space_content(level.layer.registered.space)
                    .map(Arc::as_ref),
                &level.layer.registered.declaration,
                level.level,
                ordinal,
                entity,
                rank,
                true,
                contents,
            )?;
            let mosaica_id = self.artifact_id(level.layer.registered.space, entity)?.raw();
            Some(Arc::new(Admitted {
                mosaica_id,
                passing: (ordinal, entity, masked_count, rank),
                content: supplied.values,
            }))
        });
        admitted.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::MembershipRows;
    use crate::viewport::artifacts::fixture::{sets_over, Held};
    use mosaica_types::layer::ServingLayout;

    /// [`chosen_stored`] over `shards`, each a side of the tile, by `ids` with every candidate
    /// served.
    fn chosen_over(shards: &[&Held], ids: &[u64], per_tile: usize) -> Vec<u32> {
        let forms: Vec<crate::artifacts::ArtifactRows> = shards
            .iter()
            .map(|held| {
                let sets: Vec<Option<&[u32]>> =
                    held.sets.iter().map(|set| Some(set.as_slice())).collect();
                crate::artifacts::ArtifactRows::synthetic_column_only(&sets, held.rows)
            })
            .collect();
        let columns: Vec<(&RowColumn, &LevelMembers)> = forms
            .iter()
            .map(|form| {
                let column = form.column().expect("composed above");
                (column, column.members().expect("a composed column holds its members"))
            })
            .collect();
        let leaves: Vec<EffectiveMask> = shards.iter().map(|held| held.leaf()).collect();
        let rows: Vec<Bitmap> = shards.iter().map(|held| held.tile_rows()).collect();
        let sets = sets_over(&leaves, &rows);
        let spans: Vec<(usize, Vec<Range<u32>>)> = shards
            .iter()
            .enumerate()
            .map(|(place, held)| (place, held.spans.clone()))
            .collect();
        let counts: Vec<Vec<u32>> = shards
            .iter()
            .map(|held| {
                held.sets
                    .iter()
                    .map(|set| set.iter().filter(|row| held.visible.contains(row)).count() as u32)
                    .collect()
            })
            .collect();
        let (in_request, outside) = (std::collections::BTreeSet::new(), Outside::default());
        let tile = TileAsk {
            sets: &sets,
            spans: &spans,
            per_tile,
            in_request: &in_request,
            outside: &outside,
        };
        let counts = LevelFigures::of_counts(&counts);
        chosen_stored(&tile, &columns, &counts, |o| Some((ids[o as usize], o)), &|_| true)
    }

    #[test]
    fn a_stored_levels_tile_is_proposed_and_found_in_every_shard() {
        let a = Held {
            sets: vec![vec![0, 1], vec![2, 3], vec![4, 5], vec![6], vec![]],
            rows: 8,
            visible: vec![0, 2, 4, 5, 6, 7],
            spans: vec![Range { start: 0, end: 4 }],
        };
        let b = Held {
            sets: vec![vec![], vec![0], vec![1, 2], vec![3], vec![4, 5]],
            rows: 6,
            visible: vec![0, 1, 3, 4, 5],
            spans: vec![Range { start: 2, end: 6 }],
        };
        let one = Held::joined(&a, &b, 8);
        let ids = [50, 40, 30, 20, 10];
        // Ordinal 2, the most visible over both shards, has no visible member in the tile. By
        // count over both shards, then identifier: 4, 3 and 1 (2 each; ids 10, 20, 40), then 0.
        let ranked = [4, 3, 1, 0];
        for per_tile in 0..=5 {
            let expected: Vec<u32> = ranked.iter().copied().take(per_tile).collect();
            assert_eq!(chosen_over(&[&a, &b], &ids, per_tile), expected, "{per_tile}");
            assert_eq!(chosen_over(&[&one], &ids, per_tile), expected, "{per_tile}");
        }
    }

    /// Ranked by the count over both shards, then `mosaica_id`; a dependent whose target the frame
    /// does not hold takes no place.
    #[test]
    fn a_tiles_artifacts_over_two_shards_are_those_of_one_shard_holding_both() {
        let counts: [Vec<u32>; 2] = [vec![5, 0, 3, 2, 0, 4, 1, 0], vec![0, 6, 3, 2, 1, 0, 0, 2]];
        let summed: Vec<u32> = (0..8).map(|o| counts[0][o] + counts[1][o]).collect();
        let two = LevelFigures::of_counts(&counts);
        let one = LevelFigures::of_counts(std::slice::from_ref(&summed));
        let candidates = [Bitmap::of(&[0, 2, 3, 5, 6]), Bitmap::of(&[1, 2, 3, 4, 7])];
        let present = [Bitmap::of(&[0, 3, 5]), Bitmap::of(&[1, 2, 3, 4, 7])];
        let union = |sets: &[Bitmap; 2]| sets[0].or(&sets[1]);
        // Identifiers out of ordinal order. The verdict withholds ordinal 6, and ordinal 1 depends
        // on a target the frame does not hold.
        let ids: [u64; 8] = [40, 10, 30, 31, 50, 20, 60, 11];
        let admitted = |o: u32| (o != 6).then_some((ids[o as usize], o));
        let targeted = |o: u32| o != 1;
        for per_tile in 0..=8 {
            let over_two = by_count(
                &union(&candidates),
                &two,
                per_tile,
                admitted,
                &targeted,
                |o| present.iter().any(|side| side.contains(o)),
            );
            let over_one = by_count(
                &union(&candidates),
                &one,
                per_tile,
                admitted,
                &targeted,
                |o| union(&present).contains(o),
            );
            let mut expected: Vec<u32> = (0..8u32)
                .filter(|&o| summed[o as usize] > 0 && o != 6 && o != 1)
                .filter(|&o| union(&present).contains(o))
                .collect();
            expected.sort_by_key(|&o| (std::cmp::Reverse(summed[o as usize]), ids[o as usize]));
            expected.truncate(per_tile);
            assert_eq!(over_two, expected, "per_tile {per_tile}");
            assert_eq!(over_one, expected, "per_tile {per_tile}");
        }
    }

    /// What [`PROBE_ROWS_PER_CHUNK`] is set from: the time a probe that finds nothing takes for
    /// each 2¹⁶-row chunk of a tile, over the time the scan takes for each visible row. Run with
    /// `cargo test --release -p mosaica-engine probe_against_scan -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement"]
    fn probe_against_scan() {
        const ROWS: u32 = 1 << 22;
        const ARTIFACTS: u32 = 8192;
        let mix = |row: u32| (u64::from(row).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as u32;
        let mut sets = vec![Bitmap::new(); ARTIFACTS as usize];
        for row in 0..ROWS {
            sets[(mix(row) % ARTIFACTS) as usize].add(row);
        }
        let membership = MembershipRows::of_rows(sets.into_iter().map(Some).collect());
        let scratch = tempfile::tempdir().unwrap();
        let column = RowColumn::compose(
            &membership,
            ROWS,
            ServingLayout::RowMajorLabel,
            scratch.path(),
        )
        .expect("a partition");
        let members = column
            .members()
            .expect("a composed column holds its members");
        let visible: Bitmap = (0..ROWS).filter(|&row| mix(row ^ 0x55) % 3 == 0).collect();
        for shift in [14u32, 16, 18, 20, 22] {
            let here = visible.and(&Bitmap::from_range(0..1 << shift));
            let started = std::time::Instant::now();
            let found = column.candidates(&here);
            let scan_ns = started.elapsed().as_nanos() as f64 / here.cardinality() as f64;
            assert!(!found.is_empty());
            let mut probe_ns = 0f64;
            let probes = 500;
            for ordinal in (0..ARTIFACTS).step_by((ARTIFACTS / probes) as usize) {
                let elsewhere = here.andnot(&members.members(ordinal).to_bitmap());
                let started = std::time::Instant::now();
                assert!(!members.members(ordinal).intersects(&elsewhere));
                probe_ns += started.elapsed().as_nanos() as f64;
            }
            let chunks = f64::from((1u32 << shift).div_ceil(1 << 16));
            let per_chunk = probe_ns / f64::from(probes) / chunks;
            println!(
                "tile of 2^{shift} rows: scan {scan_ns:.1} ns a visible row, a probe finding \
                 nothing {per_chunk:.0} ns a chunk = {:.0} rows of scan",
                per_chunk / scan_ns
            );
        }
    }
}
