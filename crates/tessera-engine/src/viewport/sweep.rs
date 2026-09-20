//! The tile sweep and the gather: one tile's count and selection, and the columns it emits.

use super::*;
use super::out::flat_families;

/// The emit pass's hard per-frame accumulation cap, applied under any `flush_bytes` — including
/// the deliberately huge value that means "one flush per response". The wire's frame length is a
/// `u32`, so an unbounded accumulation would panic at serialisation; 1 GiB keeps a frame two
/// factors below that bound while being far above any threshold an operator would set on
/// purpose. Not a knob: nothing legitimate sits on the other side of it.
pub(super) const MAX_POINTS_FRAME_BYTES: usize = 1 << 30;

/// Calibration task threshold: below this many total rows spanned by a request's resolved tiles
/// PLUS its §3.3 underlay cell demand if any (`Σ range.len() + underlay_cells_demanded`, pre-mask
/// — see the call site's `total_rows_in_ranges`), `Engine::viewport` folds `tile_sweep` serially
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
/// - At 1e9 (the bench's own dense grant, the realistic one):
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
/// Mask density remains unmodelled by this pre-mask predictor, and deliberately so: the re-sweep
/// used both a sparse and the bench's dense grant at every scale and found the shape-family split
/// above (natural vs full-extent) under BOTH, so mask density is not the dominant driver of the
/// three-scale tension.
pub const SERIAL_FALLBACK_MAX_ROWS: u64 = 500_000_000;

/// The predictor, pulled out as its own pure function so it is unit-testable without an `Engine`
/// or a bundle (see the `tests` module at the bottom of this file) — the behavioural claim ("a
/// below-threshold request runs the serial fold") is otherwise only observable through output
/// equality or timing, neither of which makes a good unit test on its own.
///
/// Takes `threshold` explicitly rather than reading `SERIAL_FALLBACK_MAX_ROWS`
/// directly, so the one call site (`Engine::viewport`) can supply either the production constant
/// or a test's override — see `Engine::set_serial_fallback_max_rows_for_test`'s doc for why an
/// override exists at all and why it lives on `Engine`, not here.
/// **Two terms since 2026-08-01** (owner decision, on the two-axis sweep —
/// docs/evidence/memos/2026-07-31-tile-parallelism-calibration.md, "Follow-up 4, answered", and
/// `probes/2026-08-01-two-axis-sweep/`). The row term is unchanged; [`TILE_PAR_MIN_TILES`] is new,
/// and the fan-out runs when **either** fires.
#[inline]
pub(super) fn should_fold_serially(total_rows_in_ranges: u64, threshold: u64, tiles: usize) -> bool {
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

/// The `natural` viewport family cannot reach [`TILE_PAR_MIN_TILES`] — **checked by the compiler,
/// not by a test.**
///
/// Anonymous (`const _`) to match the pattern already used for this class of guard in
/// `tessera-server/tests/http.rs`; naming it would only make it dead code.
///
/// This is the property that makes the tile arm unable to reopen the 10⁹ regression
/// [`SERIAL_FALLBACK_MAX_ROWS`] exists to protect. The family's span is exactly 16 cells wide at
/// every depth, so it resolves 81 tiles at z4 and 289 from z5 up, at every scale — including
/// `natural/z4/s6`, the 354,900,645-row shape that sized the row threshold.
///
/// A runtime `assert!` over two constants can only fail in a binary that was already built, which
/// is the wrong moment: by then the arm can already fire on ordinary client viewports at 10⁹. As a
/// `const` item it fails to **compile** instead, so a future worker who widens the family's span or
/// lowers the arm is stopped at the point of the edit and told which guarantee they are spending.
const _: () = {
    const NATURAL_MAX_TILES: usize = 289;
    assert!(
        NATURAL_MAX_TILES < TILE_PAR_MIN_TILES,
        "the natural viewport family (289 tiles at z5+) would reach the tile arm: the fan-out \
         could then fire on ordinary client viewports at 10^9, which is the regression \
         SERIAL_FALLBACK_MAX_ROWS was raised to 500,000,000 to fix."
    );
};

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
/// per-tile cost is highly non-uniform — an empty-tile skip (`tile_sweep` returning `Ok(None)`
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
/// **Claim, scoped precisely — it does not generalise.** `8` is
/// decisively best on `full-extent/{z2,z3,z4,z5}` at 1e8 and 1e9 — these are the shapes that
/// actually reach the parallel branch at the calibrated threshold (`full-extent`'s row count is
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
pub(super) const TILE_PAR_MIN_LEN: usize = 8;

/// The tiles one request answers over, and the §3.3 underlay demanded beneath them.
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
        // **This view's frame, not the bundle's** (decision 0040): every tile address below is a
        // fraction of the extent the requested view's positions were quantised against, so
        // reading another view's would address different ground under the same prefix. An unknown
        // name refuses rather than defaulting — there is no frame a view that does not exist
        // could be drawn in.
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
                    depth: req.zoom,
                })
                .collect(),
            None => tiles_for_bbox(req.bbox, req.zoom, &extent),
        };
        probe.lap(|t| &mut t.tiles_for_bbox_ns);
        probe.count(|t| &mut t.tiles_resolved, tiles.len() as u64);

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

/// Resolve every tile's row range in ONE monotone sweep rather than two full-column binary
/// searches per tile. A few hundred independent `log2(rows)` searches is where a sparse
/// request's time actually goes — measured at 26-64% of one
/// (docs/evidence/memos/2026-07-30-f1-selection-overdraw.md), and flat in density, because
/// the cost is the searching rather than the rows found.
///
/// `tile_ranges_all` returns ranges positionally aligned with `tiles`, so the zip below
/// walks `tiles_for_bbox`'s raster order unchanged. That order is load-bearing (it is the
/// response's tile order, and the wire payload's points are a flat concatenation in it) —
/// the sweep's own Morton order stays inside `tile_ranges_all` and never reaches here.
///
/// One sweep **per segment**, each in that segment's own Morton column. Transposed below
/// into per-tile part lists, because a tile is the union of its parts across segments
/// (`select::SelectionParts`) while the sweep's monotone advantage is per column.
///
/// A view with zero segments (an empty build) has nothing visible in any tile: every
/// tile's part list is empty, and the response is empty — as before.
///
/// **§14.2 fix: `rows_in_ranges` is summed here**, rather than accumulated per-tile inside
/// [`tile_sweep`]. It used to be counted into each tile's `TileStats` before that tile's own
/// `visible == 0` check, but a tile that fails that check returns `Ok(None)`, and the fold over the
/// sweep discards `Ok(None)` entirely — so a grant that left a tile empty silently dropped that
/// tile's rows from the total, making a field documented as mask-independent
/// (`rows_in_ranges - sigma_visible` is C4's leak-register numerator, and that subtraction is
/// meaningless if the minuend already has the mask baked in) actually depend on the session's mask.
/// The ranges are materialised here, before the tile sweep starts and before any mask is consulted,
/// so summing them once is mask-free by construction and cannot regress the same way — see
/// `TileStats`'s doc, which no longer carries this field at all, for the other half of this fix.
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

/// One tile's sweep contribution (D-F): count, select and underlay — **no gather**, which the
/// emit pass does later from the `rows`/`parts` returned here (`streamed-serving.md` §4). The
/// pure per-tile body pulled out of what was, before streaming, a fused count-select-gather
/// loop. Safe to call concurrently from any rayon worker: every parameter is `&`-borrowed or
/// `Copy`, nothing here reaches back into `Engine` or any state shared across tiles (see the
/// guardrail comment at the row-projection cache call site in `Engine::viewport_stream`,
/// above), and the return value is owned outright by the caller — no shared mutable state, no
/// interior mutability, nothing to synchronise.
///
/// `Ok(None)` — an empty tile: no segment for this view, or nothing visible in `range`. Exactly
/// the "skip empty" rule the old inline loop applied (no count row, no selection work). `Err`
/// carries [`EngineError::Cancelled`] from the per-tile cancellation checkpoint below — checked
/// first, so a flip
/// observed here costs only the one atomic read, never any of this tile's own
/// count/select/underlay work.
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

    // §14.2 fix: `rows_in_ranges` is no longer counted here. It is now summed once, mask-free,
    // over `ranges` in `Engine::viewport`'s serial prefix — see that call site's comment. Counting
    // it per-tile put it in `TileStats`, whose contribution this function's `Ok(None)` returns
    // (this one included, three lines below) cause `Engine::viewport`'s fold to discard outright —
    // silently making a documented-mask-independent field depend on which tiles a grant leaves
    // empty.
    let mut stats = TileProbe::new();

    // **The count is the sum over the segments the tile touches** — each segment's own tile range
    // shifted into view row space by its `row_base`, counted there, and added. §7.1's exact
    // masked count is a property of the tile, not of whichever segment happens to hold the rows,
    // so a tile straddling a build segment and a fresh flush segment must report their union.
    let parts: Vec<SelectionPart<'_>> = tile_parts
        .iter()
        .map(|(s, range)| {
            let (segment, row_base) = segments[*s];
            // **The count selection draws from, which under a filter is `M_sel`'s.** Selection
            // picks rows from the filtered set, and its decode-tier choice turns on this figure:
            // a fully-visible range takes the `FullRange` tier, which extends every row *without
            // consulting the mask*. Supplying the unfiltered count there served every row in the
            // range under a filter that had narrowed the counts correctly — the filter applied to
            // `matched` and bypassed entirely in what was drawn.
            //
            // `TileCount::visible` is computed separately and stays unfiltered; the two figures
            // answer different questions (§7.1).
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
    // The composed count — how many of this tile's items the principal may see, which a filter does
    // not change. Summed over the same segment ranges, so the two cannot disagree about which rows
    // this tile covers.
    let visible: u64 = tile_parts
        .iter()
        .map(|(s, range)| {
            let (_, row_base) = segments[*s];
            mask.count_range(row_base + range.start..row_base + range.end)
        })
        .sum();
    stats.lap(|t| &mut t.count_ns);

    if visible == 0 {
        // Skip empty: no count row, no selection work for a tile with nothing visible — the same
        // rule the old inline loop applied.
        return Ok(None);
    }
    stats.count(|t| &mut t.tiles_nonempty, 1);
    stats.count(|t| &mut t.sigma_visible, matched);

    // The owned list survives the call: the emit pass rebuilds a `SelectionParts` over it to
    // resolve each selected row at gather time.
    let part_list = parts;
    let parts = SelectionParts::new(&part_list);
    // Anchored on `matched`, not `visible`: selection's cap and tier decisions are about the set
    // it draws from. θ's *threshold* anchor is separate and stays unfiltered — `visible_total()`
    // — and on a filtered request the threshold arrives saturated (§8.5's match-layer rule; see
    // the params override in `Engine::viewport`), so `served = min(matched, cap)` there.
    let selected = Selection::of(mask, &parts, params, matched);
    stats.lap(|t| &mut t.select_ns);
    // Counted by `Selection::of` itself, inside the loops that do the reading — not from
    // `visible`, which would make the `visited == sigma_visible` cross-check a tautology.
    stats.count(|t| &mut t.select_rows_visited, selected.rows_visited);

    let count = TileCount {
        tile: tile.prefix,
        // The **composed** count, never the filtered one: `visible` answers "how many items here
        // may this principal see", which a filter does not change. §7.1 discloses it exactly.
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
            // Search only the parent's range **in each segment**: sub-cells partition their
            // parent, so this is exactly what `tile_ranges` would return, over tens of kilobytes
            // already touched by the parent's own `count_range` rather than ~30 levels of a 4 GB
            // mmap. Summed across segments for the same reason the whole-tile count is: a sub-cell
            // count is an exact masked count of the cell, not of one segment's share of it.
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

/// One tile's parallel-sweep output — [`tile_sweep`]'s return payload, folded serially and
/// in-order into the request's `tile_counts`/`sub_cells`/[`StageTimings`] and then consumed by
/// the emit pass, which gathers `rows` through a `SelectionParts` rebuilt over `parts`. An
/// implementation detail of the sweep, not part of this crate's public API — [`ViewportOut`]
/// and the [`ViewportSink`] callbacks are what callers see.
///
/// `rows` are view-space rows **ascending by `tessera_id`** ([`Selection::rows`]) — the order
/// the wire requires within a tile, and the property every mid-stream cut's validity rests on.
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
        // Calibration task: the predictor decides serial-fold vs `pool.install` fan-out, and it
        // must be available BEFORE either path runs — `Σ range.len()`, the total rows every
        // resolved tile spans (pre-mask, pre-select), is exactly that: already materialised by
        // the `tile_ranges_all` sweep above, costs one pass over `ranges` to sum, and needs no
        // work from either candidate path to compute. See [`SERIAL_FALLBACK_MAX_ROWS`]'s doc for
        // why this predictor (and not tile count) is the one the sweep data supports.
        //
        // `underlay_cells_demanded` is added in, not left out. The underlay's own
        // per-cell cost is "one small binary search plus one bitmap range-count" (the underlay
        // block's own comment, in `tile_sweep`) — the same shape of operation `count_range`
        // performs per row-range, so summing the two into one row-equivalent total before
        // comparing against the threshold is the natural extension of the same predictor, not a
        // second one bolted on. Before this fix a saturated underlay (`max_underlay_cells`,
        // default 8192) was invisible to this decision entirely, regardless of how large the
        // resulting per-tile sub-cell fan-out actually was.
        let total_rows_in_ranges = tiling.rows_in_ranges + tiling.underlay_cells_demanded;

        // D-D/D-F, calibrated: below `SERIAL_FALLBACK_MAX_ROWS`, fold `tile_sweep` in place —
        // same function, same input order, no `pool.install` — since below that line the fan-out's
        // own entry/scheduling cost exceeds the per-tile work it would parallelise (measured; see
        // the constant's doc). At or above it, the existing `pool.install` fan-out runs, on the
        // ONE shared pool this engine built at `Engine::open` — no second, per-request pool, no
        // nested throttling (D-D). Every input to `tile_sweep` is borrowed or `Copy`:
        // `mask`/`segment`/`params` are the generation- and request-derived values already
        // resolved above (lifecycle §1.1 — nothing is re-loaded per tile), and `cancel` is the D-C
        // token, checked inside `tile_sweep` at the very top (moved there rather than here).
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
        // were missed). `run` captures only shared references and `Copy` values, so it is
        // `Sync` for free and usable from both the serial `Iterator::map` below and rayon's
        // parallel `map` inside `pool.install` — no new bound this file did not already require of
        // these captures for the parallel branch to compile before this change.
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

/// One entity's row in one view, resolved to the **segment that owns it and that segment's local
/// index** — the step every entity→row-tail read takes, and the one that must not be written twice.
///
/// `Ok(None)` where the view's permutation holds no row for the entity, and where no segment's
/// `row_base` covers the row it does hold; `Err` where the view's segment set cannot be resolved
/// at all, which is a malformed bundle and is the caller's to interpret — the drill-down refuses
/// on it, the join rule's oracle warns and moves on.
///
/// **One definition, because three read paths need it.** `Engine::item` had its own copy, and
/// `flushed_row_scalar` was written as a fourth variation on it; the three lines that differed
/// between them were exactly where a false accept got in (`views.md` §4, r24 review F1). Row is
/// *view*-space and a segment is indexed locally, so getting the subtraction or the `rev()` wrong
/// reads a neighbour's value under this entity's identity.
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
/// **Keyed on `seg_id`, never zipped positionally.** `Bundle::with_segment` appends to `segments`
/// while `RowSpace::with_extent` appends the extent, so after a flush the two lists agree by
/// position — but `Bundle::with_merged` pushes the merged segment at the *end* of `segments` while
/// `RowSpace::collapsing` puts the merged extent where the consumed run was. After one merge the
/// positions diverge, and a positional zip would silently pair a segment with another segment's
/// `row_base`: every count right, every point drawn from the wrong entity. `seg_id`s are never
/// reused (contracts §2.1), so the lookup is exact.
///
/// The build segment is the one `permutation.bin` addresses and has no extent; it is therefore the
/// one with no entry in the row space, and its rows begin at 0.
///
/// **One definition, because two read paths need it.** `Engine::viewport` selects over the parts
/// and `Engine::item` resolves a single row to its owner; when `item` had its own version — take
/// `segments.first()` and index it with a *view*-space row — a drill-down on any flushed item
/// read past the build segment's end and panicked. A second copy is how the two come to disagree.
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
            // No extent: the build segment, at 0. Legitimate exactly once — see
            // `EngineError::SegmentWithoutRowBase` for why a second one is a 500 rather than
            // another segment defaulted to 0.
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
    // Ascending in `row_base`, which `SelectionParts::resolve`'s reverse scan relies on. Sorted
    // rather than assumed, for the `with_merged` reason above.
    segments.sort_unstable_by_key(|&(_, row_base)| row_base);
    Ok(segments)
}

/// Gather one row's `tessera_id`/position/declared scalars — zero-copy reads, no per-row
/// allocation beyond what a `Utf8` scalar's owned `String` requires.
///
/// The position takes one load from each of the two files that hold it, `morton.u32` for the
/// cell and `columns.arrow` for the residual, over the same row span the old `x`/`y` pair swept:
/// the same two loads, and the concatenation is a shift and an or.
/// One segment's declared columns, resolved once, in declaration order.
///
/// **Hoisted out of the row loop, and that is the whole point.** `ColumnsRef::scalar` is a hash
/// lookup on the column name plus an Arrow downcast; calling it per column *per row* made it 19
/// lookups per point on the wide fixture — 1.9e7 for a 10^6-mark viewport. Measured
/// (`tessera-bench --bin gather_shape`, 10^6 rows in 62-row tiles, 19 columns): resolving per row
/// costs **944 ms** against **136 ms** resolved per segment, ~86% of a gather whose shape is
/// otherwise unchanged.
///
/// `None` is a declared column this segment does not hold, kept **positionally** so the entry
/// order still matches the list resolved against — collapsing the absent ones here would silently shift
/// every later column left, which is the failure `gather_scalars` refuses at the write end.
type ResolvedScalars<'a> = Vec<Option<ScalarSlice<'a>>>;

/// The **group-scoped** render columns a request under `view` carries in its row tail
/// (`views.md` §5), in manifest order, as the declared scalars they are indistinguishable from
/// once resolved.
///
/// **The view set is the scope's, and the gate is inside it.** A family renders under a view of
/// its own group, and under a view of a group declaring `members` of that group — the keys being
/// the owner's by construction (§3.3) — and under nothing else: that is the rule
/// `per-point-attributes.md` §3.9 gives `render_in`, with the view set decided by the scope
/// instead of listed. A principal whose group gate fails takes the answer an undeclared attribute
/// takes, here as at every other surface (§5, §6): the column is absent from the response's
/// schema, so the sealed group is named in no response such a principal receives.
///
/// **A family with no column for this view is not in the list**, which is what a view created
/// after the build has: `scoped_scalars[..].views` names the views that *have* a column, and no
/// batch can write one (a buffered row's scalars are positional against `declared_scalars`, which
/// a family is deliberately absent from). Absence there is decided by the manifest; absence in a
/// *segment* of a view that does have one — anything a flush wrote — is
/// [`gather_tile_columns`]'s, and comes out as the row's placeholder.
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

/// The **group-scoped** render families whose column `view`'s row tail carries — the gate-free
/// half of [`scoped_render_scalars`], and the one statement of which lanes a view's rows hold.
///
/// **The write path asks this and the read path asks the wrapper above.** A writer has no
/// principal and must produce the lane whatever any session may see; a request narrows the same
/// list by the session's visible-view set. Splitting them here is what stops the two rules — which
/// row spaces carry a lane, and which of those a principal is told about — from being restated in
/// a second place and coming to disagree: a merge or a fold taking a *narrower* list would drop a
/// lane the build wrote, and the rows would read as the ordinary absence below.
pub(crate) fn scoped_render_families<'a>(
    manifest: &'a tessera_store::manifest::Manifest,
    view: &str,
) -> Vec<&'a tessera_store::manifest::ScopedScalar> {
    // This view's roster record — the group it belongs to and the key it holds there. Matched
    // against the roster rather than parsed out of the id: a view id is `<group>:<key>` by
    // construction, and the roster is what decides which group and which key that is.
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
            // §3.3's rule, stated once in `owning_key_of`, and then the family's own list: a view
            // of the group that has no column — one created since the build — renders nothing.
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
        // A declared scalar absent from this segment's schema resolves to `None`, which every
        // reader takes as the column's absence for every row of the segment (`ingest.md` §6.3):
        // a segment written before the column was declared carries no lane for it, and the
        // answer is the schema's, never a blob read's. Nothing here is authorisation-relevant.
        .map(|d| segment.columns.scalar(&d.name))
        .collect()
}

/// Gather one tile's selected rows **column-major**.
///
/// `rows` are view-space rows ascending by `tessera_id` — not by segment — so consecutive rows
/// can land in different parts. They are therefore resolved to `(part, local)` **once**, in one
/// pass, and every column then walks that placement rather than re-resolving per value. Together
/// with the per-part slice resolution this leaves the inner loop a bounds-checked index into a
/// typed slice, with no name lookup, no downcast and no per-value type dispatch.
///
/// The column set comes from `declared` and is always its length, so a request cannot end up with
/// a column set derived from whichever tile happened to be first.
///
/// A declared column a segment holds at another type is a **malformed bundle** rather than a
/// silently skipped column: the alternative is to append a wrongly-typed buffer under a name that
/// does not describe it.
///
/// **A column a segment's schema does not hold is absent for every row of that segment**
/// (`ingest.md` §6.3), answered from the schema and never from a blob read. The state that
/// produces it is a group-scoped family's lane that a segment of the view was written without
/// (`views.md` §5). A column declared at a running service reaches this list from no route:
/// `PUT /control/attributes` refuses `render`, as an interim (decision 0136's amendment). Such a
/// segment's rows take the column's placeholder, the type's zero, exactly what the build writes
/// into the slot of an entity that has no value, decision 0064's absence for the tail.
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
        // The typed slice per part is resolved BEFORE the row loop, so the loop below carries no
        // `match` at all — that hoist is the whole reason this shape is cheaper than the
        // row-major one it replaced.
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
                    // A keyword shares this arm for `ColumnBuf::empty`'s reason: rendered, it is
                    // its bytes, and it is never rendered. A segment that carried anything else
                    // under the name refuses here rather than being served.
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
        // The gather is mask-blind by construction — it reads the segment's columns for rows
        // selection already chose — so the highlight's bits are attached by the emit pass beside
        // the membership columns rather than read here.
        highlighted: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The predictor's behaviour at its boundary, both edges — asserted directly rather than
    /// through test-only instrumentation of the call site.
    #[test]
    fn should_fold_serially_is_a_strict_less_than_at_the_calibrated_boundary() {
        let t = SERIAL_FALLBACK_MAX_ROWS;
        assert!(should_fold_serially(0, t, 1));
        assert!(should_fold_serially(t - 1, t, 1));
        assert!(!should_fold_serially(t, t, 1));
        assert!(!should_fold_serially(t + 1, t, 1));
        assert!(!should_fold_serially(u64::MAX, t, 1));
    }

    /// `should_fold_serially` takes its threshold as a parameter (so
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

    /// The `natural` viewport family cannot reach the tile arm, which is what makes the 2026-08-01
    /// change unable to reopen the 10^9 regression [`SERIAL_FALLBACK_MAX_ROWS`] exists to protect.
    ///
    /// Its span is exactly 16 cells wide at every depth, giving 81 tiles at z4 and 289 from z5 up at
    /// every scale — including `natural/z4/s6`, the 354,900,645-row shape that sized the row
    /// threshold. Pinning the arithmetic tells a future worker who widens that span, or lowers the
    /// arm, which guarantee they are spending.
    ///
    /// **The family bound is asserted at compile time, not here.** Both operands are constants, so
    /// a runtime assertion over them can only fail in a binary that was already built — the
    /// `const _: () = { ... }` guard beside [`TILE_PAR_MIN_TILES`] fails to *compile* instead. This
    /// test carries the half that genuinely runs: the concrete shape that sized the row threshold
    /// still folds serially.
    #[test]
    fn the_natural_family_cannot_reach_the_tile_arm() {
        assert!(should_fold_serially(
            354_900_645,
            SERIAL_FALLBACK_MAX_ROWS,
            81
        ));
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
        // The exact collect target shape as `tile_sweep`'s call sites use, without needing a
        // real `Engine`, `mask` or bundle to produce one: `Result<Option<T>>` per item, `Ok(None)`
        // standing in for `tile_sweep`'s empty-tile skip.
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
