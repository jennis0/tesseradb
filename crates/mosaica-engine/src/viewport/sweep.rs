//! The tile sweep and the gather: one tile's count and selection, and the columns it emits.

use super::out::flat_families;
use super::*;
use mosaica_store::bands::{element, page_size, will_need, CopyType, CopyValue};

/// The emit pass's hard per-frame accumulation cap, applied under any `flush_bytes` — including
/// the deliberately huge value that means "one flush per response". The wire's frame length is a
/// `u32`, so an unbounded accumulation would panic at serialisation; 1 GiB keeps a frame two
/// factors below that bound while being far above any threshold an operator would set on
/// purpose. Not a knob: nothing legitimate sits on the other side of it.
pub(super) const MAX_POINTS_FRAME_BYTES: usize = 1 << 30;

/// Calibration threshold: below this many total rows spanned by a request's resolved tiles plus
/// any underlay cell demand (pre-mask), `Engine::viewport` folds `tile_sweep` serially instead of
/// calling `self.pool.install`.
///
/// `pub` (unlike [`TILE_PAR_MIN_LEN`]) so the byte-equality tests in `tests/viewport.rs` can
/// assert a fixture genuinely cleared it, rather than duplicating the number and risking drift.
///
/// Measured across three corpus scales (2.42M, 1e8, 1e9 rows), no single row-count threshold
/// classifies every shape correctly: the parallel path's own scheduling cost dominates a sparse
/// request's real per-tile work at small scales, while a realistic "natural" client viewport at
/// 1e9 can span up to 354,900,645 rows and stays serial-favouring throughout that range. Set at
/// 500,000,000, above that figure, so ordinary client traffic stays serial at every scale
/// measured; the fan-out is dormant for essentially all traffic below 1e9 and remains reachable at
/// 1e9 for a whole-corpus-scale request. A wrongly parallel choice measured 2-9x slower; a wrongly
/// serial one forfeits at most a 2-3x win. Row count is not sufficient alone — see
/// [`TILE_PAR_MIN_TILES`] for the second term.
pub const SERIAL_FALLBACK_MAX_ROWS: u64 = 500_000_000;

/// The predictor, pulled out as its own pure function so it is unit-testable without an `Engine`
/// or a bundle. Takes `threshold` explicitly rather than reading `SERIAL_FALLBACK_MAX_ROWS`
/// directly, so the one call site can supply either the production constant or a test's override.
///
/// Two terms, either of which fires the fan-out: the row term above, and the tile term
/// ([`TILE_PAR_MIN_TILES`]).
#[inline]
pub(super) fn should_fold_serially(
    total_rows_in_ranges: u64,
    threshold: u64,
    tiles: usize,
) -> bool {
    total_rows_in_ranges < threshold && tiles < TILE_PAR_MIN_TILES
}

/// The tile-count arm: at or above this many tiles, take the fan-out whatever the row count says.
///
/// The row term alone cannot classify every shape: the "natural" client-viewport family holds a
/// near-constant tile count across scale and density (81 at zoom 4, 289 from zoom 5 up) while its
/// row count varies, and a `full-extent` bbox spanning the whole grid tops out at 1,024 tiles
/// regardless of scale. The per-tile floor (range setup, `count_range` entry) measures a few
/// hundred nanoseconds across a 400x change in corpus size, where the row coefficient varies by
/// two orders of magnitude across the same range, so tile count is the stable axis. 4,096 measured
/// as the regret-minimising value; it sits 14-50x above the entire `natural` family's tile count,
/// so this arm cannot reopen the row term's serial-favouring regression — the `const _` guard
/// below enforces that at compile time.
pub const TILE_PAR_MIN_TILES: usize = 4_096;

/// The `natural` viewport family cannot reach [`TILE_PAR_MIN_TILES`], checked at compile time
/// rather than by a runtime assertion: a runtime check over two constants can only fail in a
/// binary that was already built, by which point the arm could already be firing on ordinary
/// client viewports. This `const` item fails to compile instead, stopping a future edit that
/// widens the family's span or lowers the arm at the point of the change.
const _: () = {
    const NATURAL_MAX_TILES: usize = 289;
    assert!(
        NATURAL_MAX_TILES < TILE_PAR_MIN_TILES,
        "the natural viewport family (289 tiles at z5+) would reach the tile arm: the fan-out \
         could then fire on ordinary client viewports at 10^9, which is the regression \
         SERIAL_FALLBACK_MAX_ROWS was raised to 500,000,000 to fix."
    );
};

/// The per-tile scheduling grain: the number of tiles rayon hands to one worker before splitting
/// the range again. Only reachable once the fan-out itself runs (above
/// [`SERIAL_FALLBACK_MAX_ROWS`] or [`TILE_PAR_MIN_TILES`]) — this grain governs the fan-out's own
/// behaviour, not whether it runs.
///
/// Per-tile cost is highly non-uniform: an empty-tile skip is a handful of comparisons, while a
/// dense tile at a high cap is a bitmap-range read plus a bounded heap sort. A large chunk handed
/// to one worker while other workers' chunks are all-empty sits unstolen for the chunk's length;
/// too small a chunk instead pays scheduling overhead on every tile, including the common
/// empty-tile skip. Measured across clearly-parallel shapes at several corpus scales, `8` is
/// consistently best or tied-best; it does not generalise to every shape at every scale (a
/// `full-extent` bbox with very few tiles favours a larger grain, since there are too few units for
/// a small one to help), but the shapes that reach the parallel branch at production thresholds
/// favour it decisively.
pub(super) const TILE_PAR_MIN_LEN: usize = 8;

/// The tiles one request answers over, and the underlay demanded beneath them.
pub(super) struct TileSet {
    /// In response order: the request's own list where it carried one, `tiles_for_bbox`'s raster
    /// order otherwise.
    pub(super) tiles: Vec<Tile>,
    /// `None` where no underlay was requested, and then no sub-cell frame is served.
    pub(super) underlay_offset: Option<u8>,
    /// How many sub-cells the underlay demands, 0 where none was requested — a term of the
    /// serial-fallback predictor, which is otherwise blind to the underlay's cost.
    pub(super) underlay_cells_demanded: u64,
}

/// A [`TileSet`] with every tile's row range resolved: what the sweep, the filter's crossing and
/// the artifact pass all read the request's extent from. It cannot be built before the tile set is,
/// which is the order the response needs.
pub(super) struct Tiling<'a> {
    pub(super) tiles: Vec<Tile>,
    pub(super) underlay_offset: Option<u8>,
    pub(super) underlay_cells_demanded: u64,
    /// Every segment of the view in every shard, which a part names by its place.
    pub(super) segments: ViewSegments<'a>,
    /// Positionally aligned with `tiles`, each entry the tile's `(segment, range)` parts, ascending
    /// by the segment's place in `segments`.
    pub(super) ranges: Vec<Vec<(usize, Range<u32>)>>,
    /// Where each tile's parts begin among every tile's parts in turn.
    pub(super) starts: Vec<usize>,
    /// Each side's parts, as `(tile, place among every part)`, in tile order: what a pass over one
    /// shard reads, so no pass reads another shard's parts.
    pub(super) by_side: Vec<Vec<(usize, usize)>>,
    /// `Σ range.len()` — the rows every resolved tile spans, pre-mask and pre-select.
    pub(super) rows_in_ranges: u64,
}

impl Tiling<'_> {
    /// One part of `side`, from [`Self::by_side`]: its segment and its range in the shard's rows.
    pub(super) fn part(&self, (tile, at): (usize, usize)) -> (&ViewSegment<'_>, Range<u32>) {
        let (s, range) = &self.ranges[tile][at - self.starts[tile]];
        let segment = &self.segments.segments[*s];
        (segment, segment.row_base + range.start..segment.row_base + range.end)
    }

    /// The rows the parts in `side` span, in its shard's row space, sorted and merged as
    /// [`crossing_domain`] merges them, and how many rows that is.
    pub(super) fn domain_in(&self, side: usize) -> (Vec<Range<u32>>, u64) {
        let spans: Vec<Range<u32>> = self.by_side[side]
            .iter()
            .map(|&part| self.part(part).1)
            .collect();
        let rows = spans.iter().map(|r| r.len() as u64).sum();
        (super::row_filter::merged_spans(spans), rows)
    }
}

/// One segment of a view in one shard.
#[derive(Clone, Copy)]
pub(super) struct ViewSegment<'a> {
    /// The place of its shard's side in [`ServedView::sides`].
    pub(super) side: usize,
    pub(super) shard: ShardId,
    pub(super) segment: &'a SegmentData,
    /// Where its rows begin in the shard's row space for the view.
    pub(super) row_base: u32,
}

/// Every segment of a view, side by side in shard order and each side's ascending by row base.
pub(super) struct ViewSegments<'a> {
    pub(super) segments: Vec<ViewSegment<'a>>,
    /// Each side's shard and where its segments begin in `segments`, ascending by shard.
    sides: Vec<(ShardId, usize)>,
}

impl<'a> ViewSegments<'a> {
    pub(super) fn of(served: &ServedView<'a>) -> Self {
        ViewSegments::new(
            served
                .sides
                .iter()
                .map(|side| (side.shard.id(), side.segments.iter().copied())),
        )
    }

    /// Each side's shard and its segments with their row bases, ascending by shard.
    fn new<S>(sides: impl IntoIterator<Item = (ShardId, S)>) -> Self
    where
        S: IntoIterator<Item = (&'a SegmentData, u32)>,
    {
        let mut segments = Vec::new();
        let mut held = Vec::new();
        for (at, (shard, side)) in sides.into_iter().enumerate() {
            held.push((shard, segments.len()));
            segments.extend(side.into_iter().map(|(segment, row_base)| ViewSegment {
                side: at,
                shard,
                segment,
                row_base,
            }));
        }
        ViewSegments {
            segments,
            sides: held,
        }
    }

    /// The shard of the side at `side`.
    pub(super) fn shard_of(&self, side: usize) -> ShardId {
        self.sides[side].0
    }

    /// The segment holding `row`, by its place in the list, and the row's index within it.
    pub(super) fn place(&self, row: ShardRow) -> Option<(usize, u32)> {
        let shard = row.shard();
        let at = self.sides.binary_search_by_key(&shard, |&(held, _)| held).ok()?;
        let start = self.sides[at].1;
        let end = self.sides.get(at + 1).map_or(self.segments.len(), |&(_, end)| end);
        let row = row.row_in(shard)?.raw();
        let within = &self.segments[start..end];
        let i = within.partition_point(|s| s.row_base <= row).checked_sub(1)?;
        Some((start + i, row - within[i].row_base))
    }
}

impl Engine {
    /// `view`'s extent, refusing a request for more tiles than `max_tiles_per_request`: the
    /// `listed` tiles where the request names them, otherwise those `bbox` covers at `zoom`.
    ///
    /// `zoom` and `bbox` are both chosen by the caller, and at zoom 16 over the full extent the
    /// tile set is 65536² tiles, an out-of-memory abort reached before any masking work. Counting
    /// allocates nothing, which is what makes this a refusal instead.
    pub(crate) fn tile_extent(
        &self,
        generation: &Generation,
        view: &str,
        zoom: u8,
        bbox: [f64; 4],
        listed: Option<usize>,
    ) -> Result<Bounds> {
        // This view's frame, not the bundle's: reading another view's would address different
        // ground under the same prefix. An unknown name refuses rather than defaulting.
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
        let demanded = match listed {
            Some(listed) => listed as u64,
            None => tiles_for_bbox_count(bbox, zoom, &extent),
        };
        let limit = self.config.max_tiles_per_request;
        if demanded > limit as u64 {
            return Err(EngineError::TooManyTiles { demanded, limit });
        }
        Ok(extent)
    }

    /// The request's tiles with every tile's row range resolved.
    pub(super) fn tiling<'a>(
        &self,
        served: &ServedView<'a>,
        req: &ViewportRequest<'_>,
        probe: &mut Probe,
    ) -> Result<Tiling<'a>> {
        let tiles = self.resolve_tiles(served, req, probe)?;
        Ok(tile_ranges(tiles, ViewSegments::of(served), probe))
    }

    /// The request's tiles, counted and refused before they are allocated, and the underlay's three
    /// bounds.
    pub(super) fn resolve_tiles(
        &self,
        served: &ServedView<'_>,
        req: &ViewportRequest<'_>,
        probe: &mut Probe,
    ) -> Result<TileSet> {
        let extent = self.tile_extent(
            served.generation,
            served.name,
            req.zoom,
            req.bbox,
            req.tiles.map(<[u64]>::len),
        )?;
        let tiles = match req.tiles {
            // An explicit list replaces the derivation: a tile a client can prove it already holds
            // is absent, and absence costs nothing — no row range, no scan, no gather. Ordering is
            // the caller's, deduplicated and not sorted, the order the tile and points streams use.
            Some(list) => list
                .iter()
                .map(|&prefix| Tile {
                    prefix,
                    depth: req.zoom,
                })
                .collect(),
            None => tiles_for_bbox(req.bbox, req.zoom, &extent),
        };
        probe.lap(|t| &mut t.tiles_for_bbox_ns);
        probe.count(|t| &mut t.tiles_resolved, tiles.len() as u64);

        // Underlay bounds, all three checked up front and rejecting rather than clamping. The cell
        // budget is checked before any counting work, since the underlay multiplies the tile set
        // by 4^offset. `underlay_cells_demanded` lets the serial-fallback predictor below see a
        // saturated underlay, which is otherwise invisible to it.
        let mut underlay_cells_demanded: u64 = 0;
        let underlay_offset = match req.underlay_offset {
            None | Some(0) => None,
            Some(offset) => {
                if offset > self.config.max_underlay_offset {
                    return Err(EngineError::UnderlayRefused(format!(
                        "underlay_offset {offset} exceeds the configured maximum {}",
                        self.config.max_underlay_offset
                    )));
                }
                let sub_depth = req.zoom as u32 + offset as u32;
                if sub_depth > 16 {
                    return Err(EngineError::UnderlayRefused(format!(
                        "underlay_offset {offset} at zoom {} needs depth {sub_depth}, but the \
                         grid is fixed at 2^16 x 2^16 so depth may not exceed 16 (§5.2)",
                        req.zoom
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

        Ok(TileSet {
            tiles,
            underlay_offset,
            underlay_cells_demanded,
        })
    }
}

/// Resolve every tile's row range in one monotone sweep rather than two binary searches per tile:
/// a few hundred independent `log2(rows)` searches is where a sparse request's time actually goes,
/// flat in density because it is the searching rather than the rows found.
///
/// `tile_ranges_all` returns ranges positionally aligned with `tiles`, preserving the response's
/// tile order, which the wire payload's points are a flat concatenation in; the sweep's own Morton
/// order stays inside `tile_ranges_all`. One sweep per segment of every shard, transposed below
/// into per-tile part lists, because a tile is the union of its parts across segments and shards.
///
/// `rows_in_ranges` is summed here, before any mask is consulted, so the figure is mask-free by
/// construction: summing it inside [`tile_sweep`] instead let a tile that failed its
/// `visible == 0` check return `Ok(None)` and be discarded by the caller's fold, silently dropping
/// that tile's rows from a total documented as mask-independent.
pub(super) fn tile_ranges<'a>(
    set: TileSet,
    segments: ViewSegments<'a>,
    probe: &mut Probe,
) -> Tiling<'a> {
    let TileSet {
        tiles,
        underlay_offset,
        underlay_cells_demanded,
    } = set;
    let per_segment: Vec<Vec<Range<u32>>> = segments
        .segments
        .iter()
        .map(|held| tile_ranges_all(held.segment, &tiles))
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
    let mut starts = Vec::with_capacity(tiles.len());
    let mut by_side: Vec<Vec<(usize, usize)>> = vec![Vec::new(); segments.sides.len()];
    let mut at = 0;
    for (t, parts) in ranges.iter().enumerate() {
        starts.push(at);
        for (s, _) in parts {
            by_side[segments.segments[*s].side].push((t, at));
            at += 1;
        }
    }
    probe.lap(|t| &mut t.tile_ranges_ns);

    let rows_in_ranges: u64 = ranges
        .iter()
        .flat_map(|parts| parts.iter())
        .map(|(_, r)| r.len() as u64)
        .sum();
    probe.count(|t| &mut t.rows_in_ranges, rows_in_ranges);

    Tiling {
        tiles,
        underlay_offset,
        underlay_cells_demanded,
        segments,
        ranges,
        starts,
        by_side,
        rows_in_ranges,
    }
}

/// Each part's counts, positionally aligned with the parts of every tile in turn: what the sweep
/// takes for every tile at once, one call per shard.
pub(super) struct PartCounts {
    /// The composed count, unaffected by a filter.
    visible: Vec<u64>,
    /// Of `visible`, how many the filter admits: the count selection draws from.
    matched: Vec<u64>,
    /// Of `matched`, how many also satisfy the highlight.
    highlighted: Vec<u64>,
}

impl PartCounts {
    /// Every part of every tile counted under `mask`: one walk of each shard's leaf over that
    /// shard's parts. A part in a shard with no leaf counts zero and is not read.
    fn of(mask: &ShardedMask, tiling: &Tiling<'_>) -> PartCounts {
        let parts: usize = tiling.ranges.iter().map(Vec::len).sum();
        let mut counts = PartCounts {
            visible: vec![0; parts],
            matched: vec![0; parts],
            highlighted: vec![0; parts],
        };
        for (side, held) in tiling.by_side.iter().enumerate() {
            let Some(leaf) = mask.leaf(tiling.segments.shard_of(side)) else {
                continue;
            };
            let ranges: Vec<Range<u32>> = held.iter().map(|&part| tiling.part(part).1).collect();
            let tallies = leaf.count_parts(&ranges);
            for (k, &(_, at)) in held.iter().enumerate() {
                counts.visible[at] = tallies.visible[k];
                counts.matched[at] = tallies.matched[k];
                counts.highlighted[at] = tallies.highlighted[k];
            }
        }
        counts
    }
}

/// One tile's sweep contribution: count and select, with no gather — the emit pass gathers later
/// from the `rows`/`tile_parts` returned here. Safe to call concurrently from any rayon worker:
/// every parameter is `&`-borrowed or `Copy`, nothing here reaches back into `Engine` or any state
/// shared across tiles, and the return value is owned outright by the caller.
///
/// `Ok(None)` for an empty tile: no segment for this view, or nothing visible in `range` — no
/// count row, no selection work. `Err` carries [`EngineError::Cancelled`] from the cancellation
/// checkpoint below, checked first so a flip observed here costs only one atomic read, never any
/// of this tile's own work.
///
/// `bands` says whether the request is answered from the identity bands ([`crate::bands`]) where
/// they can answer a tile; the rest are read as the shipped scan reads them.
///
/// `t` is the tile's place in `tiling`, and `counts` every part's counts, which the sweep takes
/// for every tile at once.
pub(super) fn tile_sweep<'a>(
    tiling: &'a Tiling<'a>,
    t: usize,
    counts: &PartCounts,
    mask: &ShardedMask,
    params: &SelectParams,
    bands: bool,
    cancel: &Option<CancelToken>,
) -> Result<Option<TileSweepOut<'a>>> {
    check_cancelled(cancel)?;
    let (tile, tile_parts, at) = (&tiling.tiles[t], &tiling.ranges[t][..], tiling.starts[t]);
    let segments = &tiling.segments;

    if tile_parts.is_empty() {
        return Ok(None);
    }

    // `rows_in_ranges` is summed in `tile_ranges`, not here — see its doc.
    let mut stats = TileProbe::new();

    // Summed over every segment of every shard the tile touches: a tile straddling a build segment
    // and a fresh flush segment, or two shards, must report their union, not one part's share.
    let here = at..at + tile_parts.len();
    // The composed count: how many of this tile's items the principal may see, unaffected by a
    // filter.
    let visible: u64 = counts.visible[here.clone()].iter().sum();
    // What selection draws from: `M_sel`'s count, equal to the composed count when unfiltered. A
    // fully-visible range takes the `FullRange` decode tier, which extends every row without
    // consulting the mask, so an unfiltered count there would serve every row under a filter that
    // had narrowed the counts correctly.
    let matched: u64 = counts.matched[here.clone()].iter().sum();
    stats.lap(|t| &mut t.count_ns);

    if visible == 0 {
        // Skip empty: no count row, no selection work for a tile with nothing visible.
        return Ok(None);
    }
    stats.count(|t| &mut t.tiles_nonempty, 1);
    stats.count(|t| &mut t.sigma_visible, matched);

    // Each shard's parts, read under that shard's leaf. A shard with no leaf has nothing here.
    let mut part_list: Vec<SelectionPart<'_>> = Vec::with_capacity(tile_parts.len());
    let mut shard_of: Vec<ShardId> = Vec::with_capacity(tile_parts.len());
    for ((s, range), &matched) in tile_parts.iter().zip(&counts.matched[here.clone()]) {
        let held = segments.segments[*s];
        if mask.leaf(held.shard).is_none() {
            continue;
        }
        part_list.push(SelectionPart {
            segment: held.segment,
            range: range.clone(),
            row_base: held.row_base,
            visible: matched,
        });
        shard_of.push(held.shard);
    }
    let mut shards: Vec<ShardParts<'_>> = Vec::new();
    let mut start = 0;
    while start < part_list.len() {
        let shard = shard_of[start];
        let end = start + shard_of[start..].partition_point(|&held| held == shard);
        shards.push(ShardParts {
            shard,
            mask: mask.leaf(shard).expect("a part of a shard with a leaf"),
            parts: &part_list[start..end],
        });
        start = end;
    }

    // Anchored on `matched`, not `visible`: selection's cap and tier decisions are about the set
    // it draws from. θ's threshold anchor stays unfiltered and, on a filtered request, arrives
    // saturated, so `served = min(matched, cap)` there.
    let answer = if bands {
        crate::bands::select(&shards, params, matched)?
    } else {
        crate::bands::BandAnswer::Declined
    };
    let (rows, entries) = match answer {
        crate::bands::BandAnswer::Served {
            rows,
            entries,
            entries_read,
            widened,
        } => {
            stats.count(|t| &mut t.tiles_from_bands, 1);
            stats.count(|t| &mut t.tiles_bands_widened, u64::from(widened));
            stats.count(|t| &mut t.select_rows_visited, entries_read);
            (rows, Some(entries))
        }
        declined => {
            if let crate::bands::BandAnswer::Sparse { read } = declined {
                stats.count(|t| &mut t.tiles_sparse_after_read, u64::from(read));
                will_need_visible_identities(&shards);
            }
            let selected = TileSelection::of(&shards, params, matched);
            // Counted by the selection itself, inside the loops that do the reading — not from
            // `visible`, which would make the `visited == sigma_visible` cross-check a tautology.
            stats.count(|t| &mut t.select_rows_visited, selected.rows_visited);
            (selected.rows, None)
        }
    };
    stats.lap(|t| &mut t.select_ns);

    let count = TileCount {
        tile: tile.prefix,
        // The composed count, never the filtered one: `visible` answers "how many items here may
        // this principal see", which a filter does not change, and is disclosed exactly.
        visible,
        // How many of those the filter admits. Equal to `visible` on an unfiltered request.
        matched,
        served: rows.len() as u64,
        // Of `matched`, how many also satisfy the highlight. Equal to `matched` with no highlight.
        highlighted: counts.highlighted[here].iter().sum(),
    };

    Ok(Some(TileSweepOut {
        tile: t,
        count,
        rows,
        entries,
        tile_parts,
        stats: stats.t,
    }))
}

/// [`tile_sweep`]'s return payload, folded serially and in-order into the request's counts and
/// then consumed by the underlay and the emit pass, which gathers `rows`. Not part of this crate's
/// public API.
///
/// `rows` ascend by `mosaica_id` — the order the wire requires within a tile, and the property
/// every mid-stream cut's validity rests on.
pub(super) struct TileSweepOut<'a> {
    /// The tile's place in the request's tiling.
    pub(super) tile: usize,
    pub(super) count: TileCount,
    pub(super) rows: Vec<ShardRow>,
    /// Where the tile was answered from the bands, each row's entry in its segment's bands, which
    /// the gather reads its position and copies from.
    pub(super) entries: Option<Vec<u32>>,
    /// The tiling's entry for this tile: each segment the tile spans, by its place in the view's
    /// segments, and the tile's rows in it.
    pub(super) tile_parts: &'a [(usize, Range<u32>)],
    pub(super) stats: TileStats,
}

/// What the sweep produced for the frames below it: every non-empty tile's count row, the
/// underlay's sub-cells, and the swept tiles themselves, which the emit pass gathers from.
pub(super) struct Swept<'a> {
    pub(super) tile_counts: Vec<TileCount>,
    pub(super) sub_cells: Vec<SubCellCount>,
    pub(super) swept: Vec<TileSweepOut<'a>>,
}

/// The sub-cells of every swept tile, each an exact masked count over a contiguous Morton range,
/// summed across the tile's segments and shards; only non-empty cells, in tile order and ascending
/// within a tile. Every shard's occupied cells are counted in one walk of its leaf, on the
/// request's thread: `max_underlay_cells` bounds the cells a request may ask for.
fn underlay(
    mask: &ShardedMask,
    tiling: &Tiling<'_>,
    swept: &[TileSweepOut<'_>],
    depth: u8,
    offset: u8,
) -> (Vec<SubCellCount>, u64) {
    let per_tile = 1u64 << (2 * u32::from(offset));
    let mut swept_at: Vec<Option<usize>> = vec![None; tiling.tiles.len()];
    for (w, ts) in swept.iter().enumerate() {
        swept_at[ts.tile] = Some(w);
    }
    let mut found: Vec<Vec<(u64, u64)>> = vec![Vec::new(); swept.len()];
    let mut parts_read = vec![0usize; swept.len()];
    let mut visited = 0u64;
    for (side, held) in tiling.by_side.iter().enumerate() {
        let Some(leaf) = mask.leaf(tiling.segments.shard_of(side)) else {
            continue;
        };
        let (mut cells, mut rows, mut owner) = (Vec::new(), Vec::new(), Vec::new());
        for &part in held {
            let Some(w) = swept_at[part.0] else {
                continue;
            };
            let (segment, range) = tiling.part(part);
            let first = swept[w].count.tile << (2 * u32::from(offset));
            crate::cells::cell_ranges(
                segment.segment,
                segment.row_base,
                range.start - segment.row_base..range.end - segment.row_base,
                depth,
                first..first + per_tile,
                &mut cells,
                &mut rows,
            );
            owner.resize(rows.len(), w);
            parts_read[w] += 1;
        }
        visited += rows.len() as u64;
        if rows.is_empty() {
            continue;
        }
        let counted = leaf.count_ranges(&rows);
        for ((&cell, n), &w) in cells.iter().zip(counted).zip(&owner) {
            if n > 0 {
                found[w].push((cell, n));
            }
        }
    }
    let mut sub_cells = Vec::new();
    for (cells, parts) in found.into_iter().zip(parts_read) {
        let cells = match parts > 1 {
            true => crate::cells::add_equal_keys(cells),
            false => cells,
        };
        sub_cells.extend(cells.into_iter().map(|(cell, count)| SubCellCount { cell, count }));
    }
    (sub_cells, visited)
}

impl Engine {
    /// Sweep every tile — serially or on the pool — and fold the outcomes in tile order.
    pub(super) fn sweep_tiles<'a>(
        &self,
        mask: &ShardedMask,
        tiling: &'a Tiling<'a>,
        params: &SelectParams,
        req: &ViewportRequest<'_>,
        probe: &mut Probe,
    ) -> Result<Swept<'a>> {
        // `Σ range.len()`, already materialised by the `tile_ranges_all` sweep above, plus
        // `underlay_cells_demanded`: the underlay's per-cell cost is the same shape of operation
        // `count_range` performs per row-range, so the two combine into one predictor.
        let total_rows_in_ranges = tiling.rows_in_ranges + tiling.underlay_cells_demanded;

        // Every tile's parts counted together, each of a leaf's bitmaps walked once: tiles next to
        // each other share containers, which a count per part popcounts again for each.
        probe.skip();
        let counts = PartCounts::of(mask, tiling);
        probe.lap(|t| &mut t.count_ns);

        // Below `SERIAL_FALLBACK_MAX_ROWS`, fold `tile_sweep` in place, since the fan-out's own
        // entry/scheduling cost exceeds the per-tile work it would parallelise below that line. At
        // or above it, the fan-out runs on the one shared pool this engine built at `Engine::open`.
        // Both branches produce `Vec<Result<Option<TileSweepOut>>>` in `tiles`' order, so the fold
        // below is identical either way, which keeps the two paths byte-identical. A `Cancelled`
        // observed inside `tile_sweep` propagates to the fold regardless of path; the serial fold
        // has at most one tile of wasted work in flight at cancellation, where the parallel
        // fan-out has at most `compute_threads` tiles, since every tile already past the
        // checkpoint runs to completion.
        let bands = req.zoom < self.switches.bands_below_zoom.load(Ordering::Relaxed);
        let run = |t: usize| tile_sweep(tiling, t, &counts, mask, params, bands, &req.cancel);

        // Read from `self`, not the constant, so a test can override it per-`Engine` via
        // `Engine::set_serial_fallback_max_rows_for_test`; a shipped binary always reads
        // `SERIAL_FALLBACK_MAX_ROWS` here.
        let serial_fallback_max_rows = self
            .switches
            .serial_fallback_max_rows
            .load(Ordering::Relaxed);
        let tile_outcomes: Vec<Result<Option<TileSweepOut>>> = if should_fold_serially(
            total_rows_in_ranges,
            serial_fallback_max_rows,
            tiling.tiles.len(),
        ) {
            (0..tiling.tiles.len())
                .map(run)
                .collect::<Vec<Result<Option<TileSweepOut>>>>()
        } else {
            self.pool.install(|| {
                (0..tiling.tiles.len())
                    .into_par_iter()
                    .with_min_len(TILE_PAR_MIN_LEN)
                    .map(run)
                    .collect::<Vec<Result<Option<TileSweepOut>>>>()
            })
        };
        // Neither branch's own wall time is a named stage — already accounted for per tile inside
        // each tile's stats — so this resets the clock rather than misattributing the stretch.
        probe.skip();

        // The serial, in-order fold: `tile_outcomes`' order equals `tiles`' order by construction.
        // Short-circuits on the first `Err`; every tile's own work is already done by this point.
        let mut tile_counts = Vec::new();
        let mut swept: Vec<TileSweepOut> = Vec::new();
        for outcome in tile_outcomes {
            let Some(ts) = outcome? else {
                continue;
            };
            ts.stats.fold_into(&mut probe.t);
            tile_counts.push(ts.count.clone());
            swept.push(ts);
        }

        // The underlay beneath the non-empty tiles.
        let mut sub_cells = Vec::new();
        if let Some(offset) = tiling.underlay_offset {
            probe.skip();
            let (cells, visited) = underlay(mask, tiling, &swept, req.zoom + offset, offset);
            sub_cells = cells;
            probe.lap(|t| &mut t.underlay_ns);
            probe.count(|t| &mut t.underlay_cells_evaluated, visited);
        }
        Ok(Swept {
            tile_counts,
            sub_cells,
            swept,
        })
    }
}

/// One entity's row in one view, resolved to the segment that owns it and that segment's local
/// index — the step every entity-to-row-tail read takes.
///
/// `Ok(None)` where the view's permutation holds no row for the entity, or no segment's
/// `row_base` covers the row it does hold; `Err` where the view's segment set cannot be resolved
/// at all, a malformed bundle the caller interprets — the drill-down refuses on it, the join
/// rule's oracle warns and moves on.
///
/// One definition shared by every read path that needs it: row is view-space and a segment is
/// indexed locally, so getting the subtraction or the reverse-scan direction wrong reads a
/// neighbour's value under this entity's identity.
pub(crate) fn segment_row_of<'a>(
    view: &str,
    view_data: &'a mosaica_store::read::ViewData,
    entity: EntityId,
) -> Result<Option<(&'a SegmentData, usize)>> {
    let Some(row) = view_data.row_space.row_of(entity) else {
        return Ok(None);
    };
    let segments = segments_with_row_bases(view, view_data)?;
    Ok(segment_holding(&segments, row.raw()).map(|(at, local)| (segments[at].0, local as usize)))
}

/// Which of `segments`, ascending by row base, holds view row `row`, and the row's index within
/// it. `None` where no segment's base is at or below `row`.
pub(crate) fn segment_holding(segments: &[(&SegmentData, u32)], row: u32) -> Option<(usize, u32)> {
    let at = segments
        .partition_point(|&(_, base)| base <= row)
        .checked_sub(1)?;
    Some((at, row - segments[at].1))
}

/// [`mosaica_store::read::ViewData::segments_by_row_base`], refused in this crate's error.
// Public for `mosaica-bench`'s `identity_bands_probe`; not part of the engine's API.
#[doc(hidden)]
pub fn segments_with_row_bases<'a>(
    view: &str,
    view_data: &'a mosaica_store::read::ViewData,
) -> Result<Vec<(&'a SegmentData, u32)>> {
    view_data
        .segments_by_row_base()
        .map_err(|seg_id| EngineError::SegmentWithoutRowBase {
            view: view.to_string(),
            seg_id,
        })
}

/// One segment's declared columns, resolved once, in declaration order. Resolving per column per
/// row instead, a hash lookup on the column name plus an Arrow downcast each time, measured about
/// seven times slower on a wide fixture.
///
/// `None` is a declared column this segment does not hold, kept positionally so the entry order
/// still matches the list resolved against — collapsing the absent ones here would silently shift
/// every later column left.
type ResolvedScalars<'a> = Vec<Option<ScalarSlice<'a>>>;

/// The group-scoped render columns a request under `view` carries in its row tail, in manifest
/// order, as the declared scalars they are indistinguishable from once resolved.
///
/// A family renders under a view of its own group, or a view of a group declaring members of that
/// group, and nowhere else. A principal whose group gate fails is shown the same absence an
/// undeclared attribute takes: the column is missing from the response's schema, so a sealed group
/// is named in no response such a principal receives.
///
/// A family with no column for this view — one created after the build — is not in the list;
/// absence in a segment of a view that does have one is [`Gather`]'s to resolve, and
/// comes out as an absent value.
pub(super) fn scoped_render_scalars(
    manifest: &mosaica_store::manifest::Manifest,
    view: &str,
    visible: &crate::gate::VisibleViews,
) -> Vec<DeclaredScalar> {
    scoped_render_families(manifest, view)
        .into_iter()
        .filter(|f| visible.contains_group(&f.group))
        .map(|f| DeclaredScalar {
            name: f.name.clone(),
            arrow_type: f.arrow_type,
            vocabulary: f.vocabulary.clone(),
            analyser: f.analyser.clone(),
            index: f.index,
            render: true,
            unique: false,
        })
        .collect()
}

/// The group-scoped render families whose column `view`'s row tail carries — the gate-free half
/// of [`scoped_render_scalars`].
///
/// The write path asks this directly; the read path asks the wrapper above, which narrows by the
/// session's visible-view set. Splitting them here stops the two rules — which row spaces carry a
/// lane, and which a principal is told about — from being restated and coming to disagree: a fold
/// taking a narrower list would drop a lane the build wrote.
pub(crate) fn scoped_render_families<'a>(
    manifest: &'a mosaica_store::manifest::Manifest,
    view: &str,
) -> Vec<&'a mosaica_store::manifest::ScopedScalar> {
    manifest
        .groups
        .iter()
        .flat_map(|g| g.scoped_scalars.iter())
        .filter(|f| f.render && scoped_column_under(manifest, f, view))
        .collect()
}

/// Every view whose row tail carries a column of `family`, in roster order: the one list
/// [`scoped_render_families`] and `/v1/meta` both read. Unfiltered by the gate.
pub(crate) fn scoped_family_views(
    manifest: &mosaica_store::manifest::Manifest,
    family: &mosaica_store::manifest::ScopedScalar,
) -> Vec<String> {
    manifest
        .groups
        .iter()
        .flat_map(|g| {
            g.views
                .iter()
                .map(move |v| format!("{}{}{}", g.name, mosaica_store::GROUP_SEPARATOR, v.key))
        })
        .filter(|view| scoped_column_under(manifest, family, view))
        .collect()
}

/// Whether `view`'s row tail carries a column of `family`: `view` is a view of the family's group,
/// or of a group declaring members of it, and the family has a column for that key.
fn scoped_column_under(
    manifest: &mosaica_store::manifest::Manifest,
    family: &mosaica_store::manifest::ScopedScalar,
    view: &str,
) -> bool {
    // Matched against the roster rather than parsed out of the id, since the roster decides which
    // group and key a view id names.
    let Some(roster) = manifest.groups.iter().find_map(|g| {
        let key = view
            .strip_prefix(g.name.as_str())?
            .strip_prefix(mosaica_store::GROUP_SEPARATOR)?;
        g.views
            .iter()
            .any(|v| v.key == key)
            .then_some((g.name.as_str(), key))
    }) else {
        return false;
    };
    let members_of = |name: &str| {
        manifest
            .groups
            .iter()
            .find(|g| g.name == name)?
            .members_of
            .as_deref()
    };
    // A view of the group with no column — created since the build — carries none.
    owning_key_of(roster, members_of, &family.group).is_some_and(|key| {
        family.views.contains(&format!(
            "{}{}{key}",
            family.group,
            mosaica_store::GROUP_SEPARATOR
        ))
    })
}

/// Resolve `declared` against one segment's columns, once.
pub(super) fn resolve_scalars<'a>(
    segment: &'a SegmentData,
    declared: &[DeclaredScalar],
) -> ResolvedScalars<'a> {
    declared
        .iter()
        // Absent from this segment's schema resolves to `None`, taken as the column's absence for
        // every row of the segment: a segment written before the column was declared carries no
        // lane for it, and the answer is the schema's, never a blob read's.
        .map(|d| segment.columns.scalar(&d.name))
        .collect()
}

/// Which rows of one segment carry a value for one render column.
///
/// A category's absence is its code 0 in the column, so a category is `Every` here. A number,
/// bool, timestamp or string is absent where the segment's presence record says so, and on every
/// row of a segment that has no such column.
#[derive(Clone, Copy)]
pub(crate) enum RowPresence<'a> {
    Every,
    NoRow,
    Rows(&'a croaring::Bitmap),
}

impl<'a> RowPresence<'a> {
    pub(crate) fn of(segment: &'a SegmentData, name: &str, category: bool) -> Self {
        Self::read(
            segment,
            name,
            category,
            segment.columns.scalar(name).is_some(),
        )
    }

    /// [`Self::of`] where the caller has already looked up whether the segment holds the column.
    fn read(segment: &'a SegmentData, name: &str, category: bool, held: bool) -> Self {
        if category {
            return RowPresence::Every;
        }
        if !held {
            return RowPresence::NoRow;
        }
        match segment.columns.presence(name).bitmap() {
            None => RowPresence::Every,
            Some(rows) => RowPresence::Rows(rows),
        }
    }

    /// This presence over `rows` alone: `Every` where each of them carries a value.
    fn within(self, rows: Range<u32>) -> Self {
        match self {
            RowPresence::Rows(held) if held.contains_range(rows) => RowPresence::Every,
            other => other,
        }
    }

    #[inline]
    pub(crate) fn contains(&self, row: u32) -> bool {
        match self {
            RowPresence::Every => true,
            RowPresence::NoRow => false,
            RowPresence::Rows(rows) => rows.contains(row),
        }
    }
}

/// One column's values at `placed`, each read by `value(point, segment, local)`, and which of them
/// carry a value. `presence` is `None` where every row the tile spans carries one, and then no row
/// is tested.
#[inline]
fn gather_column<T>(
    placed: &[(u32, u32)],
    presence: Option<&[RowPresence<'_>]>,
    value: impl Fn(usize, usize, usize) -> T,
) -> (Vec<T>, Option<Vec<bool>>) {
    let mut out = Vec::with_capacity(placed.len());
    for (point, &(segment, local)) in placed.iter().enumerate() {
        out.push(value(point, segment as usize, local as usize));
    }
    let Some(presence) = presence else {
        return (out, None);
    };
    let present: Vec<bool> = placed
        .iter()
        .map(|&(segment, local)| presence[segment as usize].contains(local))
        .collect();
    let every = present.iter().all(|&held| held);
    (out, (!every).then_some(present))
}

/// One segment's identity and position columns, and the same fields of its band entries, empty
/// for a segment no tile touches.
#[derive(Default)]
struct SegmentRows<'a> {
    mosaica_id: &'a [u64],
    morton: &'a [u32],
    residual: &'a [u32],
    band_ids: &'a [u64],
    band_codes: &'a [u32],
    band_residuals: &'a [u32],
}

macro_rules! column_slices {
    ($(($v:ident, $t:ty)),* $(,)?) => {
        /// One render column's typed values in each segment of the view, and its band copy in
        /// each, `None` where the segment's schema lacks the column or no tile of the request
        /// touches the segment. A string is not copied.
        enum ColumnSlices<'a> {
            $($v {
                values: Vec<Option<&'a [$t]>>,
                copies: Vec<Option<&'a [$t]>>,
            },)*
            Bool {
                values: Vec<Option<&'a arrow::array::BooleanArray>>,
                copies: Vec<Option<&'a [u8]>>,
            },
            Utf8(Vec<Option<&'a arrow::array::StringArray>>),
        }
    };
}
flat_families!(column_slices);

/// One declared render column across the view's segments.
struct RenderColumn<'a> {
    values: ColumnSlices<'a>,
    /// Per segment. `NoRow` for a segment no tile touches, which no tile reads.
    presence: Vec<RowPresence<'a>>,
}

fn malformed(d: &DeclaredScalar) -> EngineError {
    EngineError::Malformed(format!(
        "a segment of this view holds scalar column '{}' at a type other than the declared {}; \
         serving it would put values under another column's name",
        d.name,
        d.arrow_type.arrow_type_name()
    ))
}

/// Column `ci` of each resolved segment as `typed` reads it, refusing a segment that holds the
/// column at another type.
fn per_segment<'a, T>(
    resolved: &[Option<ResolvedScalars<'a>>],
    ci: usize,
    d: &DeclaredScalar,
    typed: impl Fn(&ScalarSlice<'a>) -> Option<T>,
) -> Result<Vec<Option<T>>> {
    resolved
        .iter()
        .map(|r| match r.as_ref().and_then(|r| r[ci].as_ref()) {
            None => Ok(None),
            Some(slice) => typed(slice).map(Some).ok_or_else(|| malformed(d)),
        })
        .collect()
}

/// Column `ci`'s band copy in each touched segment that holds the column, `None` elsewhere.
fn band_copies<'a, T: CopyValue>(
    touched: &[Option<&'a SegmentData>],
    resolved: &[Option<ResolvedScalars<'a>>],
    ci: usize,
    d: &DeclaredScalar,
    ty: CopyType,
) -> Vec<Option<&'a [T]>> {
    touched
        .iter()
        .zip(resolved)
        .map(|(segment, r)| match (segment, r) {
            (Some(segment), Some(r)) if r[ci].is_some() => segment.bands.copy_values(&d.name, ty),
            _ => None,
        })
        .collect()
}

/// The emit pass's reader. Every segment a swept tile touches has its identity and position
/// columns, its band entries, and each declared render column's typed values, band copy and
/// presence resolved once for the request, indexed by the segment's place in the view's segments
/// across every shard. A tile then narrows presence to its rows in each segment and runs the row
/// loops.
///
/// A tile answered from the bands reads each point's identity, position and render values from its
/// band entry, and a string, or a column the segment's bands hold no copy of, from the column at
/// the entry's row. A tile answered by the scan reads every field from the columns.
///
/// A segment holding a declared column at another type is refused. A segment whose schema lacks a
/// declared column reads as absent on every row, as a group-scoped family's lane does in a segment
/// written before the family existed. An absent value is the type's zero in the buffer and false
/// in the column's `present`, except a category's, which is code 0 in the buffer.
pub(super) struct Gather<'a> {
    /// The view's segments in every shard: where a selected row is placed.
    view: &'a ViewSegments<'a>,
    /// Per segment, whether a swept tile reads it: a segment of a shard with no leaf holds no
    /// served row.
    touched: Vec<bool>,
    segments: Vec<SegmentRows<'a>>,
    /// One per declared render column, in declaration order.
    columns: Vec<RenderColumn<'a>>,
    /// The current tile's selected rows as `(segment, local)`.
    placed: Vec<(u32, u32)>,
    /// Each segment's presence over the rows the current tile spans in it, for the current column.
    presence: Vec<RowPresence<'a>>,
    /// The first tile whose pages have not been asked for, and the points from the tile being
    /// gathered up to it.
    ahead: usize,
    ahead_points: usize,
    /// Page addresses, reused across tiles.
    pages: Vec<usize>,
}

/// How many points ahead of the gather a sparse tile's pages are asked for.
const PREFETCH_POINTS: usize = 16_384;

/// A tile is sparse, and its pages asked for ahead, where its points are at least this many rows
/// apart on average. Denser, consecutive points share pages the kernel's read-ahead already serves,
/// and the calls cost more than they save.
const SPARSE_ROWS_PER_POINT: u64 = 64;

/// Whether a tile's pages are asked for ahead of the gather: always for one answered from the
/// bands, whose file is read without read-ahead, and for a sparse one answered by the scan.
fn is_scattered(ts: &TileSweepOut<'_>) -> bool {
    if ts.entries.is_some() {
        return true;
    }
    let spanned: u64 = ts
        .tile_parts
        .iter()
        .map(|(_, r)| (r.end - r.start) as u64)
        .sum();
    spanned >= ts.rows.len() as u64 * SPARSE_ROWS_PER_POINT
}

/// Ask for the identity column's pages under a tile's visible rows, before the scan reads them:
/// a tile the bands could not answer holds few visible rows, scattered over its range, and one
/// request for all of their pages costs less than a fault for each.
fn will_need_visible_identities(shards: &[ShardParts<'_>]) {
    let page = page_size();
    let mut pages = Vec::new();
    for held in shards {
        for part in held.parts {
            let ids = part.segment.columns.mosaica_id();
            let base = part.row_base;
            let rows = base + part.range.start..base + part.range.end;
            held.mask.for_each_run_holding_visible(rows, |run| {
                let last = element(ids, (run.end - 1 - base) as usize);
                let mut at = element(ids, (run.start - base) as usize) & !(page - 1);
                while at <= last {
                    pages.push(at);
                    at += page;
                }
            });
        }
    }
    will_need(&mut pages);
}

impl<'a> Gather<'a> {
    /// The reader of `swept`'s rows. A segment in a shard where `mask` has no leaf holds none of
    /// them and is not resolved.
    pub(super) fn new(
        view: &'a ViewSegments<'a>,
        swept: &[TileSweepOut<'_>],
        declared: &[DeclaredScalar],
        mask: &ShardedMask,
    ) -> Result<Self> {
        let mut touched: Vec<Option<&'a SegmentData>> = vec![None; view.segments.len()];
        for ts in swept {
            for &(s, _) in ts.tile_parts {
                let held = view.segments[s];
                if mask.leaf(held.shard).is_some() {
                    touched[s] = Some(held.segment);
                }
            }
        }
        let resolved: Vec<Option<ResolvedScalars<'a>>> = touched
            .iter()
            .map(|segment| segment.map(|segment| resolve_scalars(segment, declared)))
            .collect();

        let mut columns = Vec::with_capacity(declared.len());
        for (ci, d) in declared.iter().enumerate() {
            let presence = touched
                .iter()
                .zip(&resolved)
                .map(|(segment, r)| match (segment, r) {
                    (Some(segment), Some(r)) => {
                        RowPresence::read(segment, &d.name, d.vocabulary.is_some(), r[ci].is_some())
                    }
                    _ => RowPresence::NoRow,
                })
                .collect();
            macro_rules! typed {
                ($(($v:ident, $t:ty)),* $(,)?) => {
                    match d.arrow_type {
                        $(ScalarType::$v => ColumnSlices::$v {
                            values: per_segment(&resolved, ci, d, |s| match s {
                                ScalarSlice::$v(x) => Some(*x),
                                _ => None,
                            })?,
                            copies: band_copies(&touched, &resolved, ci, d, CopyType::$v),
                        },)*
                        ScalarType::Bool => ColumnSlices::Bool {
                            values: per_segment(&resolved, ci, d, |s| match s {
                                ScalarSlice::Bool(a) => Some(*a),
                                _ => None,
                            })?,
                            copies: band_copies(&touched, &resolved, ci, d, CopyType::Bool),
                        },
                        // A keyword shares this arm: rendered, it is its bytes.
                        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => {
                            ColumnSlices::Utf8(per_segment(&resolved, ci, d, |s| match s {
                                ScalarSlice::Utf8(a) => Some(*a),
                                _ => None,
                            })?)
                        }
                    }
                };
            }
            let values = flat_families!(typed);
            columns.push(RenderColumn { values, presence });
        }

        Ok(Gather {
            view,
            touched: touched.iter().map(Option::is_some).collect(),
            segments: touched
                .iter()
                .map(|segment| {
                    segment.map_or_else(SegmentRows::default, |segment| SegmentRows {
                        mosaica_id: segment.columns.mosaica_id(),
                        morton: segment.morton.u32(),
                        residual: segment.columns.residual(),
                        band_ids: segment.bands.ids(),
                        band_codes: segment.bands.codes(),
                        band_residuals: segment.bands.residuals(),
                    })
                })
                .collect(),
            columns,
            placed: Vec::new(),
            presence: vec![RowPresence::NoRow; view.segments.len()],
            ahead: 0,
            ahead_points: 0,
            pages: Vec::new(),
        })
    }

    /// Ask for the pages of the scattered tiles not yet asked for, until [`PREFETCH_POINTS`]
    /// points lie ahead of the tile being gathered.
    fn prefetch(&mut self, swept: &[TileSweepOut<'_>]) {
        while self.ahead < swept.len() && self.ahead_points < PREFETCH_POINTS {
            let ts = &swept[self.ahead];
            self.ahead += 1;
            self.ahead_points += ts.rows.len();
            if !is_scattered(ts) {
                continue;
            }
            self.pages.clear();
            for (point, &row) in ts.rows.iter().enumerate() {
                let (segment, local) = self
                    .view
                    .place(row)
                    .expect("a selected row lies in the view's row space");
                let rows = &self.segments[segment];
                let local = local as usize;
                let entry = ts.entries.as_ref().map(|entries| entries[point] as usize);
                match entry {
                    Some(e) => {
                        self.pages.push(element(rows.band_ids, e));
                        self.pages.push(element(rows.band_codes, e));
                        self.pages.push(element(rows.band_residuals, e));
                    }
                    None => {
                        self.pages.push(element(rows.mosaica_id, local));
                        self.pages.push(element(rows.morton, local));
                        self.pages.push(element(rows.residual, local));
                    }
                }
                for column in &self.columns {
                    macro_rules! page {
                        ($(($v:ident, $t:ty)),* $(,)?) => {
                            match &column.values {
                                $(ColumnSlices::$v { values, copies } => {
                                    match (entry, copies[segment]) {
                                        (Some(e), Some(copy)) => Some(element(copy, e)),
                                        _ => values[segment].map(|v: &[$t]| element(v, local)),
                                    }
                                })*
                                ColumnSlices::Bool { values, copies } => {
                                    match (entry, copies[segment]) {
                                        (Some(e), Some(copy)) => Some(element(copy, e)),
                                        _ => values[segment].map(|a| {
                                            let bits = a.values();
                                            element(bits.values(), (bits.offset() + local) / 8)
                                        }),
                                    }
                                }
                                // The offsets only: the value bytes' place is read from them, so
                                // asking for it would fault the offsets now. Those pages fault
                                // as the gather reads them.
                                ColumnSlices::Utf8(arrays) => {
                                    arrays[segment].map(|a| element(a.value_offsets(), local))
                                }
                            }
                        };
                    }
                    self.pages.extend(flat_families!(page));
                }
            }
            will_need(&mut self.pages);
        }
    }

    /// Gather one tile's selected rows column-major. `rows` ascend by `mosaica_id` and not by
    /// segment or shard, so consecutive rows can land in different segments; they are placed as
    /// `(segment, local)` once, and every column then walks that placement, leaving the inner loop
    /// a bounds-checked index into a typed slice.
    ///
    /// `swept[at]` is the tile; the tiles are gathered in order, and the pages of scattered tiles
    /// ahead are asked for first.
    pub(super) fn tile(&mut self, swept: &[TileSweepOut<'_>], at: usize) -> PointColumns {
        self.prefetch(swept);
        let ts = &swept[at];
        self.ahead_points -= ts.rows.len();
        let view = self.view;
        self.placed.clear();
        self.placed.extend(ts.rows.iter().map(|&row| {
            let (segment, local) =
                view.place(row).expect("a selected row lies in the view's row space");
            (segment as u32, local)
        }));
        let placed = &self.placed;
        let entries = ts.entries.as_deref();

        let mut mosaica_ids = Vec::with_capacity(placed.len());
        let mut codes = Vec::with_capacity(placed.len());
        match entries {
            Some(entries) => {
                for (&(segment, _), &e) in placed.iter().zip(entries) {
                    let rows = &self.segments[segment as usize];
                    let e = e as usize;
                    mosaica_ids.push(rows.band_ids[e]);
                    codes.push(((rows.band_codes[e] as u64) << 32) | rows.band_residuals[e] as u64);
                }
            }
            None => {
                for &(segment, local) in placed {
                    let rows = &self.segments[segment as usize];
                    let idx = local as usize;
                    mosaica_ids.push(rows.mosaica_id[idx]);
                    codes.push(((rows.morton[idx] as u64) << 32) | rows.residual[idx] as u64);
                }
            }
        }

        let mut scalars = Vec::with_capacity(self.columns.len());
        for column in &self.columns {
            let mut every = true;
            for (s, range) in ts.tile_parts {
                if !self.touched[*s] {
                    continue;
                }
                let within = column.presence[*s].within(range.clone());
                every &= matches!(within, RowPresence::Every);
                self.presence[*s] = within;
            }
            let presence = (!every).then_some(self.presence.as_slice());
            // A tile answered from the bands reads each value from its entry's copy where the
            // segment holds one; a tile answered by the scan reads every value from the column.
            macro_rules! gather {
                ($(($v:ident, $t:ty)),* $(,)?) => {
                    match &column.values {
                        $(ColumnSlices::$v { values, copies } => {
                            let (out, present) = match entries {
                                Some(entries) => gather_column(placed, presence, |point, s, local| {
                                    match (copies[s], values[s]) {
                                        (Some(copy), _) => copy[entries[point] as usize],
                                        (None, Some(v)) => v[local],
                                        (None, None) => <$t>::default(),
                                    }
                                }),
                                None => gather_column(placed, presence, |_, s, local| {
                                    match values[s] {
                                        Some(v) => v[local],
                                        None => <$t>::default(),
                                    }
                                }),
                            };
                            (ColumnBuf::$v(out), present)
                        })*
                        ColumnSlices::Bool { values, copies } => {
                            let (out, present) = match entries {
                                Some(entries) => gather_column(placed, presence, |point, s, local| {
                                    match (copies[s], values[s]) {
                                        (Some(copy), _) => copy[entries[point] as usize] != 0,
                                        (None, Some(a)) => a.value(local),
                                        (None, None) => false,
                                    }
                                }),
                                None => gather_column(placed, presence, |_, s, local| {
                                    match values[s] {
                                        Some(a) => a.value(local),
                                        None => false,
                                    }
                                }),
                            };
                            (ColumnBuf::Bool(out), present)
                        }
                        ColumnSlices::Utf8(arrays) => {
                            let (out, present) = gather_column(placed, presence, |_, s, local| {
                                match arrays[s] {
                                    Some(a) => a.value(local).to_string(),
                                    None => String::new(),
                                }
                            });
                            (ColumnBuf::Utf8(out), present)
                        }
                    }
                };
            }
            let (values, present) = flat_families!(gather);
            scalars.push(PointScalar { values, present });
        }

        PointColumns {
            mosaica_ids,
            codes,
            scalars,
            membership: Vec::new(),
            // The gather is mask-blind: it reads columns for rows selection already chose. The
            // highlight's bits are attached by the emit pass instead.
            highlighted: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The predictor's behaviour at its boundary, both edges.
    #[test]
    fn should_fold_serially_is_a_strict_less_than_at_the_calibrated_boundary() {
        let t = SERIAL_FALLBACK_MAX_ROWS;
        assert!(should_fold_serially(0, t, 1));
        assert!(should_fold_serially(t - 1, t, 1));
        assert!(!should_fold_serially(t, t, 1));
        assert!(!should_fold_serially(t + 1, t, 1));
        assert!(!should_fold_serially(u64::MAX, t, 1));
    }

    /// `threshold` is a genuine parameter, not the constant in disguise, at a value far from
    /// production.
    #[test]
    fn should_fold_serially_honours_an_arbitrary_threshold_not_just_the_constant() {
        assert!(should_fold_serially(5, 10, 1));
        assert!(!should_fold_serially(10, 10, 1));
        assert!(!should_fold_serially(15, 10, 1));
        // A threshold of 0 forces parallel unconditionally: `total_rows_in_ranges < 0` is never
        // true for a `u64`.
        assert!(!should_fold_serially(0, 0, 1));
    }

    /// The tile arm at its exact boundary, with the row count held far below its own threshold in
    /// every case, so reverting to a one-term predictor fails here and nowhere else.
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
        assert!(!should_fold_serially(
            rows_far_below,
            t,
            TILE_PAR_MIN_TILES + 1
        ));
    }

    /// The rule is a disjunction: either term alone suffices and neither is necessary. A mutation
    /// making it a conjunction must fail.
    #[test]
    fn the_two_terms_are_a_disjunction_not_a_conjunction() {
        let t = SERIAL_FALLBACK_MAX_ROWS;
        assert!(!should_fold_serially(t, t, 1), "rows fire alone");
        assert!(
            !should_fold_serially(1, t, TILE_PAR_MIN_TILES),
            "tiles fire alone"
        );
        assert!(
            should_fold_serially(t - 1, t, TILE_PAR_MIN_TILES - 1),
            "neither fires -- the only serial case"
        );
        assert!(!should_fold_serially(t, t, TILE_PAR_MIN_TILES), "both fire");
    }

    /// The `natural` viewport family cannot reach the tile arm, which is what stops it reopening
    /// the regression [`SERIAL_FALLBACK_MAX_ROWS`] exists to protect: its span is exactly 16 cells
    /// wide at every depth, giving 81 tiles at z4, including `natural/z4/s6`, the 354,900,645-row
    /// shape that sized the row threshold. The family bound itself is asserted at compile time,
    /// not here (see the `const _` guard beside [`TILE_PAR_MIN_TILES`]); this test carries the
    /// half that genuinely runs, that this concrete shape still folds serially.
    #[test]
    fn the_natural_family_cannot_reach_the_tile_arm() {
        assert!(should_fold_serially(
            354_900_645,
            SERIAL_FALLBACK_MAX_ROWS,
            81
        ));
    }

    /// A fixture that genuinely crosses `SERIAL_FALLBACK_MAX_ROWS` is impractical to build inside
    /// a unit test, so what is proved here instead, decoupled from `Engine`/mask/fixture size
    /// entirely, is that rayon's indexed collect, over the exact shape `Engine::viewport` uses
    /// (`Vec<Result<Option<TileSweepOut>>>`, never `Result<Vec<TileSweepOut>>`), preserves input
    /// order regardless of pool size or `with_min_len`.
    #[test]
    fn indexed_collect_of_tile_shaped_results_preserves_order_at_any_pool_size() {
        // The exact collect target shape `tile_sweep`'s call sites use, without needing a real
        // `Engine`, mask or bundle: `Result<Option<T>>` per item, `Ok(None)` standing in for
        // `tile_sweep`'s empty-tile skip.
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
            (8, 512), // the largest value the min_len sweep tried
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

#[cfg(test)]
mod across_tests {
    //! The sweep over two shards against one segment holding every row: the counts, the leave-out
    //! and grouping of a tile's parts, the selection, the underlay's merge across shards, where a
    //! row is placed and each shard's crossing domain must all answer as the one segment does.

    use super::*;

    use croaring::Bitmap;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use rustc_hash::FxHashSet;

    use crate::select::fixture::{points, segment_of, tile_of, Written};

    /// One shard: its segments, ascending by row base.
    struct Side {
        shard: ShardId,
        segments: Vec<(Written, u32)>,
    }

    impl Side {
        fn of(shard: u32, segments: Vec<Written>) -> Side {
            let mut base = 0;
            let mut placed = Vec::new();
            for segment in segments {
                let count = segment.ids.len() as u32;
                placed.push((segment, base));
                base += count;
            }
            Side {
                shard: ShardId::new(shard).unwrap(),
                segments: placed,
            }
        }

        /// The rows of this shard whose identity `admits` holds.
        fn rows(&self, admits: impl Fn(u64) -> bool) -> Bitmap {
            let mut rows = Bitmap::new();
            for (segment, base) in &self.segments {
                for (row, &id) in segment.ids.iter().enumerate() {
                    if admits(id) {
                        rows.add(base + row as u32);
                    }
                }
            }
            rows
        }

        fn id_of(&self, row: u32) -> u64 {
            let (segment, base) = self.segments.iter().rev().find(|(_, b)| *b <= row).unwrap();
            segment.ids[(row - base) as usize]
        }
    }

    /// What one view answers for `tiles`: each tile's counts and served identities, and the
    /// underlay.
    #[derive(Debug, PartialEq)]
    struct Answer {
        tiles: Vec<Option<(TileCount, Vec<u64>)>>,
        sub_cells: Vec<SubCellCount>,
    }

    /// The sweep over `sides`, each seeing the identities `visible` holds, filtered and
    /// highlighted by identity where asked.
    fn answer(
        sides: &[Side],
        visible: &FxHashSet<u64>,
        tiles: &[Tile],
        filtered: bool,
        params: &SelectParams,
        bands: bool,
    ) -> Answer {
        let segments = ViewSegments::new(sides.iter().map(|side| {
            let held: Vec<(&SegmentData, u32)> =
                side.segments.iter().map(|(s, base)| (&s.data, *base)).collect();
            (side.shard, held)
        }));
        let set = TileSet {
            tiles: tiles.to_vec(),
            underlay_offset: Some(2),
            underlay_cells_demanded: 0,
        };
        let tiling = tile_ranges(set, segments, &mut Probe::new());
        for (at, side) in sides.iter().enumerate() {
            for &(t, part) in &tiling.by_side[at] {
                let (segment, rows) = tiling.part((t, part));
                assert_eq!(segment.shard, side.shard);
                for row in [rows.start, rows.end - 1] {
                    let placed = ShardRow::new(side.shard, RowId::new(row));
                    let (s, local) = tiling.segments.place(placed).unwrap();
                    let ids = tiling.segments.segments[s].segment.columns.mosaica_id();
                    assert_eq!(ids[local as usize], side.id_of(row));
                }
            }
        }
        let leaves = sides.iter().enumerate().map(|(at, side)| {
            let mut leaf = EffectiveMask::of_rows(side.rows(|id| visible.contains(&id)));
            if filtered {
                leaf = leaf.with_filter(FilterRows::Complete(side.rows(|id| id % 3 != 0)));
                let (domain, rows) = tiling.domain_in(at);
                let lit = side.rows(|id| id % 5 == 0);
                let domain_rows: Bitmap = domain.iter().flat_map(|r| r.clone()).collect();
                assert_eq!(domain_rows.cardinality(), rows);
                leaf = leaf.with_highlight(FilterRows::Viewport {
                    rows: lit.and(&domain_rows),
                    domain,
                });
            }
            (side.shard, leaf)
        });
        let mask = ShardedMask::new(leaves.collect::<Vec<_>>());
        let counts = PartCounts::of(&mask, &tiling);
        let mut swept = Vec::new();
        let mut out = Vec::new();
        for t in 0..tiling.tiles.len() {
            match tile_sweep(&tiling, t, &counts, &mask, params, bands, &None).unwrap() {
                None => out.push(None),
                Some(ts) => {
                    let ids = ts
                        .rows
                        .iter()
                        .map(|row| {
                            let side = sides.iter().find(|s| s.shard == row.shard()).unwrap();
                            side.id_of(row.row_in(side.shard).unwrap().raw())
                        })
                        .collect();
                    out.push(Some((ts.count.clone(), ids)));
                    swept.push(ts);
                }
            }
        }
        let (sub_cells, _) = underlay(&mask, &tiling, &swept, tiles[0].depth + 2, 2);
        Answer {
            tiles: out,
            sub_cells,
        }
    }

    #[test]
    fn a_sweep_over_two_shards_answers_as_one_segment_of_every_row() {
        let mut checked = 0;
        for seed in 0..6u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let all = points(&mut rng, 2_500);
            let mut split: [Vec<(f32, f32, u64)>; 3] = Default::default();
            for &p in &all {
                split[rng.gen_range(0..3)].push(p);
            }
            let [first, second, third] = split;
            // Every other seed, the viewer sees nothing in the first shard, which has no leaf.
            let unseen: FxHashSet<u64> = match seed % 2 {
                0 => first.iter().map(|p| p.2).collect(),
                _ => FxHashSet::default(),
            };
            let visible: FxHashSet<u64> = all
                .iter()
                .filter(|p| !unseen.contains(&p.2) && rng.gen_bool(0.5))
                .map(|p| p.2)
                .collect();
            let whole = [Side::of(0, vec![segment_of(&all)])];
            let sides = [
                Side::of(0, vec![segment_of(&first)]),
                Side::of(1, vec![segment_of(&second), segment_of(&third)]),
            ];
            for depth in [2u8, 4] {
                let mut tiles: Vec<Tile> = all
                    .iter()
                    .step_by(41)
                    .map(|&(x, y, _)| tile_of(x, y, depth))
                    .collect();
                tiles.sort_by_key(|tile| tile.prefix);
                tiles.dedup_by_key(|tile| tile.prefix);
                for threshold in [Threshold::Saturated, Threshold::Cut(1 << 56)] {
                    for (filtered, bands) in [(false, false), (true, false), (false, true)] {
                        let p = SelectParams {
                            k_min: 2,
                            cap: 30,
                            threshold,
                        };
                        let want = answer(&whole, &visible, &tiles, filtered, &p, bands);
                        let got = answer(&sides, &visible, &tiles, filtered, &p, bands);
                        assert_eq!(
                            got, want,
                            "seed {seed} depth {depth} {threshold:?} filtered {filtered}"
                        );
                        assert!(!want.sub_cells.is_empty());
                        checked += want.tiles.iter().flatten().count();
                    }
                }
            }
        }
        assert!(checked > 100, "only {checked} tiles held anything");
    }
}
