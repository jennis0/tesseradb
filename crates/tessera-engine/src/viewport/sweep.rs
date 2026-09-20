//! The tile sweep and the gather: one tile's count and selection, and the columns it emits.

use super::*;
use super::out::flat_families;

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
pub(super) fn should_fold_serially(total_rows_in_ranges: u64, threshold: u64, tiles: usize) -> bool {
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
pub(super) struct Tiling {
    pub(super) tiles: Vec<Tile>,
    pub(super) underlay_offset: Option<u8>,
    pub(super) underlay_cells_demanded: u64,
    /// Positionally aligned with `tiles`, each entry the tile's `(segment, range)` parts.
    pub(super) ranges: Vec<Vec<(usize, Range<u32>)>>,
    /// `Σ range.len()` — the rows every resolved tile spans, pre-mask and pre-select.
    pub(super) rows_in_ranges: u64,
}

impl Engine {
    /// The request's tiles, counted and refused before they are allocated, and the underlay's three
    /// bounds.
    pub(super) fn resolve_tiles(
        &self,
        served: &ServedView<'_>,
        req: &ViewportRequest<'_>,
        probe: &mut Probe,
    ) -> Result<TileSet> {
        // This view's frame, not the bundle's: reading another view's would address different
        // ground under the same prefix. An unknown name refuses rather than defaulting.
        let q = served
            .generation
            .bundle
            .manifest
            .quantisation_of(served.name)
            .ok_or_else(|| EngineError::UnknownView(served.name.to_string()))?;
        let extent = Bounds {
            x_min: q.x_min,
            x_max: q.x_max,
            y_min: q.y_min,
            y_max: q.y_max,
        };

        // Refuse an over-large tile set before allocating it: `zoom` and `bbox` are both
        // attacker-chosen, and at zoom 16 over the full extent the tile set is 65536² tiles, an
        // out-of-memory abort reached before any masking work. Counting first allocates nothing,
        // which is what makes this a 422 instead. The same bound applies to an explicit list.
        let tile_count = match req.tiles {
            Some(list) => list.len() as u64,
            None => tiles_for_bbox_count(req.bbox, req.zoom, &extent),
        };
        if tile_count > self.config.max_tiles_per_request as u64 {
            return Err(EngineError::TooManyTiles {
                demanded: tile_count,
                limit: self.config.max_tiles_per_request,
            });
        }
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
/// order stays inside `tile_ranges_all`. One sweep per segment, transposed below into per-tile
/// part lists, because a tile is the union of its parts across segments.
///
/// `rows_in_ranges` is summed here, before any mask is consulted, so the figure is mask-free by
/// construction: summing it inside [`tile_sweep`] instead let a tile that failed its
/// `visible == 0` check return `Ok(None)` and be discarded by the caller's fold, silently dropping
/// that tile's rows from a total documented as mask-independent.
pub(super) fn tile_ranges(
    set: TileSet,
    segments: &[(&SegmentData, u32)],
    probe: &mut Probe,
) -> Tiling {
    let TileSet {
        tiles,
        underlay_offset,
        underlay_cells_demanded,
    } = set;
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
        ranges,
        rows_in_ranges,
    }
}

/// One tile's sweep contribution: count, select and underlay, with no gather — the emit pass
/// gathers later from the `rows`/`parts` returned here. Safe to call concurrently from any rayon
/// worker: every parameter is `&`-borrowed or `Copy`, nothing here reaches back into `Engine` or
/// any state shared across tiles, and the return value is owned outright by the caller.
///
/// `Ok(None)` for an empty tile: no segment for this view, or nothing visible in `range` — no
/// count row, no selection work. `Err` carries [`EngineError::Cancelled`] from the cancellation
/// checkpoint below, checked first so a flip observed here costs only one atomic read, never any
/// of this tile's own work.
#[allow(clippy::too_many_arguments)]
pub(super) fn tile_sweep<'a>(
    tile: &Tile,
    tile_parts: &[(usize, Range<u32>)],
    mask: &EffectiveMask,
    segments: &[(&'a SegmentData, u32)],
    params: &SelectParams,
    zoom: u8,
    underlay_offset: Option<u8>,
    cancel: &Option<CancelToken>,
) -> Result<Option<TileSweepOut<'a>>> {
    check_cancelled(cancel)?;

    if tile_parts.is_empty() {
        return Ok(None);
    }

    // `rows_in_ranges` is summed in `tile_ranges`, not here — see its doc.
    let mut stats = TileProbe::new();

    // Summed over every segment the tile touches, each shifted into view row space by its
    // `row_base`: a tile straddling a build segment and a fresh flush segment must report their
    // union, not just one segment's share.
    let parts: Vec<SelectionPart<'_>> = tile_parts
        .iter()
        .map(|(s, range)| {
            let (segment, row_base) = segments[*s];
            // The count selection draws from, `M_sel`'s: a fully-visible range takes the
            // `FullRange` decode tier, which extends every row without consulting the mask, so an
            // unfiltered count here would serve every row under a filter that had narrowed the
            // counts correctly. `TileCount::visible` below stays unfiltered.
            let visible = mask.count_matched_range(row_base + range.start..row_base + range.end);
            SelectionPart {
                segment,
                range: range.clone(),
                row_base,
                visible,
            }
        })
        .collect();
    // What selection draws from: `M_sel`'s count, equal to the composed count when unfiltered.
    let matched: u64 = parts.iter().map(|p| p.visible).sum();
    // The composed count: how many of this tile's items the principal may see, unaffected by a
    // filter, summed over the same segment ranges as `matched`.
    let visible: u64 = tile_parts
        .iter()
        .map(|(s, range)| {
            let (_, row_base) = segments[*s];
            mask.count_range(row_base + range.start..row_base + range.end)
        })
        .sum();
    stats.lap(|t| &mut t.count_ns);

    if visible == 0 {
        // Skip empty: no count row, no selection work for a tile with nothing visible.
        return Ok(None);
    }
    stats.count(|t| &mut t.tiles_nonempty, 1);
    stats.count(|t| &mut t.sigma_visible, matched);

    // The owned list survives the call: the emit pass rebuilds a `SelectionParts` over it to
    // resolve each selected row at gather time.
    let part_list = parts;
    let parts = SelectionParts::new(&part_list);
    // Anchored on `matched`, not `visible`: selection's cap and tier decisions are about the set
    // it draws from. θ's threshold anchor stays unfiltered and, on a filtered request, arrives
    // saturated, so `served = min(matched, cap)` there.
    let selected = Selection::of(mask, &parts, params, matched);
    stats.lap(|t| &mut t.select_ns);
    // Counted by `Selection::of` itself, inside the loops that do the reading — not from
    // `visible`, which would make the `visited == sigma_visible` cross-check a tautology.
    stats.count(|t| &mut t.select_rows_visited, selected.rows_visited);

    let count = TileCount {
        tile: tile.prefix,
        // The composed count, never the filtered one: `visible` answers "how many items here may
        // this principal see", which a filter does not change, and is disclosed exactly.
        visible,
        // How many of those the filter admits. Equal to `visible` on an unfiltered request.
        matched,
        served: selected.rows.len() as u64,
        // Of `matched`, how many also satisfy the highlight — one `and_cardinality` per segment
        // range, the operation `matched` already is. Equal to `matched` with no highlight.
        highlighted: tile_parts
            .iter()
            .map(|(s, range)| {
                let (_, row_base) = segments[*s];
                mask.count_highlighted_range(row_base + range.start..row_base + range.end)
            })
            .sum(),
    };

    // The tile's sub-cells, each an exact masked count over a contiguous Morton range. Only
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
            // Searches only the parent's range, over bytes already touched by the parent's own
            // `count_range`. Summed across segments so a sub-cell count is exact for the cell.
            let sub_count: u64 = parts
                .as_slice()
                .iter()
                .map(|part| {
                    let local = tile_ranges_within(part.segment, &sub_tile, part.range.clone());
                    mask.count_range(part.row_base + local.start..part.row_base + local.end)
                })
                .sum();
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

    Ok(Some(TileSweepOut {
        count,
        rows: selected.rows,
        parts: part_list,
        sub_cells,
        stats: stats.t,
    }))
}

/// [`tile_sweep`]'s return payload, folded serially and in-order into the request's counts and
/// sub-cells and then consumed by the emit pass, which gathers `rows` through a `SelectionParts`
/// rebuilt over `parts`. Not part of this crate's public API.
///
/// `rows` are view-space rows ascending by `tessera_id` — the order the wire requires within a
/// tile, and the property every mid-stream cut's validity rests on.
pub(super) struct TileSweepOut<'a> {
    pub(super) count: TileCount,
    pub(super) rows: Vec<u32>,
    pub(super) parts: Vec<SelectionPart<'a>>,
    pub(super) sub_cells: Vec<SubCellCount>,
    pub(super) stats: TileStats,
}

/// What the sweep produced for the frames below it: every non-empty tile's count row, the
/// underlay's sub-cells, and the swept tiles themselves, which the emit pass gathers from.
pub(super) struct Swept<'a> {
    pub(super) tile_counts: Vec<TileCount>,
    pub(super) sub_cells: Vec<SubCellCount>,
    pub(super) swept: Vec<TileSweepOut<'a>>,
}

impl Engine {
    /// Sweep every tile — serially or on the pool — and fold the outcomes in tile order.
    pub(super) fn sweep_tiles<'a>(
        &self,
        served: &ServedView<'a>,
        mask: &EffectiveMask,
        tiling: &Tiling,
        params: &SelectParams,
        req: &ViewportRequest<'_>,
        probe: &mut Probe,
    ) -> Result<Swept<'a>> {
        // `Σ range.len()`, already materialised by the `tile_ranges_all` sweep above, plus
        // `underlay_cells_demanded`: the underlay's per-cell cost is the same shape of operation
        // `count_range` performs per row-range, so the two combine into one predictor.
        let total_rows_in_ranges = tiling.rows_in_ranges + tiling.underlay_cells_demanded;

        // Below `SERIAL_FALLBACK_MAX_ROWS`, fold `tile_sweep` in place, since the fan-out's own
        // entry/scheduling cost exceeds the per-tile work it would parallelise below that line. At
        // or above it, the fan-out runs on the one shared pool this engine built at `Engine::open`.
        // Both branches produce `Vec<Result<Option<TileSweepOut>>>` in `tiles`' order, so the fold
        // below is identical either way, which keeps the two paths byte-identical. A `Cancelled`
        // observed inside `tile_sweep` propagates to the fold regardless of path; the serial fold
        // has at most one tile of wasted work in flight at cancellation, where the parallel
        // fan-out has at most `compute_threads` tiles, since every tile already past the
        // checkpoint runs to completion.
        let run = |tile: &Tile, tile_parts: &[(usize, Range<u32>)]| {
            tile_sweep(
                tile,
                tile_parts,
                mask,
                &served.segments,
                params,
                req.zoom,
                tiling.underlay_offset,
                &req.cancel,
            )
        };

        // Read from `self`, not the constant, so a test can override it per-`Engine` via
        // `Engine::set_serial_fallback_max_rows_for_test`; a shipped binary always reads
        // `SERIAL_FALLBACK_MAX_ROWS` here.
        let serial_fallback_max_rows =
            self.switches.serial_fallback_max_rows.load(Ordering::Relaxed);
        let tile_outcomes: Vec<Result<Option<TileSweepOut>>> = if should_fold_serially(
            total_rows_in_ranges,
            serial_fallback_max_rows,
            tiling.tiles.len(),
        ) {
            tiling
                .tiles
                .iter()
                .zip(&tiling.ranges)
                .map(|(tile, tile_parts)| run(tile, tile_parts))
                .collect::<Vec<Result<Option<TileSweepOut>>>>()
        } else {
            self.pool.install(|| {
                tiling
                    .tiles
                    .par_iter()
                    .zip(tiling.ranges.par_iter())
                    .with_min_len(TILE_PAR_MIN_LEN)
                    .map(|(tile, tile_parts)| run(tile, tile_parts))
                    .collect::<Vec<Result<Option<TileSweepOut>>>>()
            })
        };
        // Neither branch's own wall time is a named stage — already accounted for per tile inside
        // each tile's stats — so this resets the clock rather than misattributing the stretch.
        probe.skip();

        // The serial, in-order fold: `tile_outcomes`' order equals `tiles`' order by construction.
        // Short-circuits on the first `Err`; every tile's own work is already done by this point.
        let mut tile_counts = Vec::new();
        let mut sub_cells = Vec::new();
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
    view_data: &'a tessera_store::read::ViewData,
    entity: EntityId,
) -> Result<Option<(&'a SegmentData, usize)>> {
    let Some(row) = view_data.row_space.row_of(entity) else {
        return Ok(None);
    };
    let segments = segments_with_row_bases(view, view_data)?;
    let Some(&(segment, row_base)) = segments.iter().rev().find(|(_, base)| row.raw() >= *base)
    else {
        return Ok(None);
    };
    Ok(Some((segment, (row.raw() - row_base) as usize)))
}

/// A view's segments paired with their `row_base` in view row space, ascending.
///
/// Keyed on `seg_id`, never zipped positionally: a merge pushes the merged segment to the end of
/// `segments` while the row space puts the merged extent where the consumed run was, so after one
/// merge a positional zip would silently pair a segment with another segment's `row_base` — every
/// count right, every point drawn from the wrong entity. `seg_id`s are never reused, so a lookup
/// keyed on them is exact.
///
/// The build segment has no extent and is therefore the one with no entry in the row space; its
/// rows begin at 0.
///
/// One definition shared by the two read paths that need it: `Engine::viewport` selects over the
/// parts and `Engine::item` resolves a single row to its owner.
// Public for `tessera-bench`'s `identity_bands_probe`; not part of the engine's API.
#[doc(hidden)]
pub fn segments_with_row_bases<'a>(
    view: &str,
    view_data: &'a tessera_store::read::ViewData,
) -> Result<Vec<(&'a SegmentData, u32)>> {
    let row_bases: std::collections::HashMap<&str, u32> = view_data
        .row_space
        .extents()
        .iter()
        .map(|extent| (extent.seg_id.as_str(), extent.row_base))
        .collect();
    let mut base_seen = false;
    let mut segments: Vec<(&SegmentData, u32)> = Vec::with_capacity(view_data.segments.len());
    for segment in &view_data.segments {
        let row_base = match row_bases.get(segment.seg_id.as_str()) {
            Some(&row_base) => row_base,
            // No extent: the build segment, at 0. Legitimate exactly once.
            None if !base_seen => {
                base_seen = true;
                0
            }
            None => {
                return Err(EngineError::SegmentWithoutRowBase {
                    view: view.to_string(),
                    seg_id: segment.seg_id.clone(),
                })
            }
        };
        segments.push((segment.as_ref(), row_base));
    }
    // Ascending in `row_base`, which `SelectionParts::resolve`'s reverse scan relies on.
    segments.sort_unstable_by_key(|&(_, row_base)| row_base);
    Ok(segments)
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
/// absence in a segment of a view that does have one is [`gather_tile_columns`]'s to resolve, and
/// comes out as the row's placeholder.
pub(super) fn scoped_render_scalars(
    manifest: &tessera_store::manifest::Manifest,
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
    manifest: &'a tessera_store::manifest::Manifest,
    view: &str,
) -> Vec<&'a tessera_store::manifest::ScopedScalar> {
    // Matched against the roster rather than parsed out of the id, since the roster decides which
    // group and key a view id names.
    let Some(roster) = manifest.groups.iter().find_map(|g| {
        let key = view
            .strip_prefix(g.name.as_str())?
            .strip_prefix(tessera_store::GROUP_SEPARATOR)?;
        g.views
            .iter()
            .any(|v| v.key == key)
            .then_some((g.name.as_str(), key))
    }) else {
        return Vec::new();
    };
    let members_of = |name: &str| {
        manifest
            .groups
            .iter()
            .find(|g| g.name == name)?
            .members_of
            .as_deref()
    };
    manifest
        .groups
        .iter()
        .flat_map(|g| g.scoped_scalars.iter())
        .filter(|f| f.render)
        .filter(|f| {
            // A view of the group with no column — created since the build — renders nothing.
            owning_key_of(roster, members_of, &f.group).is_some_and(|key| {
                f.views.contains(&format!(
                    "{}{}{key}",
                    f.group,
                    tessera_store::GROUP_SEPARATOR
                ))
            })
        })
        .collect()
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

/// Gather one tile's selected rows column-major. `tessera_id` and position are one load each from
/// `columns.arrow` and `morton.u32`. `rows` are view-space, ascending by `tessera_id` and not by
/// segment, so consecutive rows can land in different parts; they are resolved to `(part, local)`
/// once, and every column then walks that placement, leaving the inner loop a bounds-checked
/// index into a typed slice with no name lookup, no downcast and no per-value type dispatch.
///
/// The column set is always `declared`'s length, so a request cannot end up with a column set
/// derived from whichever tile happened to be first. A declared column a segment holds at another
/// type is a malformed bundle, refused rather than silently skipped. A column a segment's schema
/// does not hold is absent for every row of that segment, answered from the schema and never a
/// blob read, and takes the type's zero placeholder — the state a group-scoped family's lane in a
/// segment written before it existed produces.
pub(super) fn gather_tile_columns(
    parts: &SelectionParts<'_>,
    rows: &[u32],
    declared: &[DeclaredScalar],
) -> Result<PointColumns> {
    let placed: Vec<(u32, u32)> = rows
        .iter()
        .map(|&row| {
            let (part, _, local) = parts.resolve_indexed(row);
            (part as u32, local)
        })
        .collect();

    let mut tessera_ids = Vec::with_capacity(rows.len());
    let mut codes = Vec::with_capacity(rows.len());
    for &(part, local) in &placed {
        let segment = parts.as_slice()[part as usize].segment;
        let idx = local as usize;
        tessera_ids.push(segment.columns.tessera_id()[idx]);
        codes.push(
            ((segment.morton.u32()[idx] as u64) << 32) | segment.columns.residual()[idx] as u64,
        );
    }

    let resolved: Vec<ResolvedScalars<'_>> = parts
        .as_slice()
        .iter()
        .map(|part| resolve_scalars(part.segment, declared))
        .collect();

    let malformed = |d: &DeclaredScalar| {
        EngineError::Malformed(format!(
            "a segment of this view holds scalar column '{}' at a type other than the declared \
             {}; serving it would put values under another column's name",
            d.name,
            d.arrow_type.arrow_type_name()
        ))
    };

    let mut scalars = Vec::with_capacity(declared.len());
    for (ci, d) in declared.iter().enumerate() {
        // The typed slice per part is resolved before the row loop, so the loop below carries no
        // `match` at all.
        macro_rules! build {
            ($(($v:ident, $t:ty)),* $(,)?) => {
                match d.arrow_type {
                    $(ScalarType::$v => {
                        let mut per_part: Vec<Option<&[$t]>> = Vec::with_capacity(resolved.len());
                        for r in &resolved {
                            match r[ci] {
                                Some(ScalarSlice::$v(s)) => per_part.push(Some(s)),
                                None => per_part.push(None),
                                _ => return Err(malformed(d)),
                            }
                        }
                        let mut out = Vec::with_capacity(rows.len());
                        for &(part, local) in &placed {
                            out.push(match per_part[part as usize] {
                                Some(s) => s[local as usize],
                                None => <$t>::default(),
                            });
                        }
                        ColumnBuf::$v(out)
                    })*
                    ScalarType::Bool => {
                        let mut per_part = Vec::with_capacity(resolved.len());
                        for r in &resolved {
                            match r[ci] {
                                Some(ScalarSlice::Bool(a)) => per_part.push(Some(a)),
                                None => per_part.push(None),
                                _ => return Err(malformed(d)),
                            }
                        }
                        let mut out = Vec::with_capacity(rows.len());
                        for &(part, local) in &placed {
                            out.push(match per_part[part as usize] {
                                Some(a) => a.value(local as usize),
                                None => false,
                            });
                        }
                        ColumnBuf::Bool(out)
                    }
                    // A keyword shares this arm: rendered, it is its bytes. A segment carrying
                    // anything else under the name refuses here rather than being served.
                    ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => {
                        let mut per_part = Vec::with_capacity(resolved.len());
                        for r in &resolved {
                            match r[ci] {
                                Some(ScalarSlice::Utf8(a)) => per_part.push(Some(a)),
                                None => per_part.push(None),
                                _ => return Err(malformed(d)),
                            }
                        }
                        let mut out = Vec::with_capacity(rows.len());
                        for &(part, local) in &placed {
                            out.push(match per_part[part as usize] {
                                Some(a) => a.value(local as usize).to_string(),
                                None => String::new(),
                            });
                        }
                        ColumnBuf::Utf8(out)
                    }
                }
            };
        }
        scalars.push(flat_families!(build));
    }

    Ok(PointColumns {
        tessera_ids,
        codes,
        scalars,
        membership: Vec::new(),
        // The gather is mask-blind: it reads columns for rows selection already chose. The
        // highlight's bits are attached by the emit pass instead.
        highlighted: None,
    })
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
