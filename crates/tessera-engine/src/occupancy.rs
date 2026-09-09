//! `N_occ(d)` — how many depth-*d* tiles hold at least one row this session may see.
//!
//! Design §7.2 anchors the selection threshold at `θ_d = m_target · N_occ(d) / V_total`. This
//! module counts `N_occ(d)`; [`EffectiveMask::visible_total`] counts `V_total`, and
//! [`crate::select::Threshold::at_depth`] turns the pair into a cut point.
//!
//! # It is a composed quantity, and I2 requires it
//!
//! `N_occ(d)` is counted over the **composed** mask — the [`EffectiveMask`] after the overlay
//! diff — and never over the cached [`crate::compose::RowProjection`], which is `M_auth` *before*
//! that diff. The argument is [`EffectiveMask::visible_total`]'s, term for term: a viewer can
//! aggregate mark counts across tiles, solve for θ, and difference it against the per-tile
//! `visible` §7.1 discloses exactly. If either factor of θ were pre-overlay, that difference is a
//! running estimate of how many of the viewer's own items have been denied — a count of items
//! outside `M_auth`, which Appendix C admits nowhere. `N_occ` carries the signal as directly as
//! `V_total` does: a suppression that empties a tile lowers it.
//!
//! It is also **filter-blind**, and for **I12**'s reason: [`crate::compose::EffectiveMask`] with a
//! filter attached is a narrower set, so counting occupied tiles under one would make θ a function
//! of what the viewer typed and move the frontier down. The call site takes the anchor before the
//! filter is evaluated.
//!
//! # Two properties the design rests on
//!
//! **Monotone in depth.** Every occupied depth-*d* tile has at least one occupied child, and
//! children of distinct parents are distinct tiles, so `N_occ(d+1) >= N_occ(d)`. §7.2's nesting
//! proof needs θ non-decreasing in depth and gets it from that structure. Nothing here clamps or
//! carries a running maximum: a clamp would hide a counting bug rather than prevent one.
//!
//! **`N_occ(d) <= min(4^d, |mask|)`.** There are only `4^d` tiles at depth *d*, and a tile is
//! counted only when a visible row falls in it.
//!
//! # The walk, and the two routes that were measured and rejected
//!
//! Rows are stored in Morton order within a segment, so the code column ascends with the row id
//! and a tile is a contiguous range of it. The walk takes the mask's visible runs and, inside a
//! run, hops from one occupied tile's code boundary to the next by exponential search
//! ([`gallop`]). Cost is `O((runs + N_occ(d)) · log)` per depth, against the oracle's one pass per
//! visible row.
//!
//! **Endpoint arithmetic on a run is wrong.** Crediting `tile(end) − tile(start) + 1` per run
//! counts the tile indices a run *spans*, not the occupied tiles inside it: a run's Morton codes
//! are not dense in tile-index space, so every empty tile between two occupied ones is counted.
//! Measured against the linear-scan oracle it is wrong by 1.6×–225× at depth 12 across four
//! corpora. The gallop is what makes the answer exact.
//!
//! **A descent over the tile tree is not affordable.** Visiting occupied nodes breadth-first pays
//! a Morton binary search per node to find that node's row range plus one mask question to decide
//! whether it is empty, and the node count is the answer being computed: reaching depth 16 visits
//! `Σ_d N_occ(d)` nodes where this walk visits one depth's runs. Measured over 1M–13.5M-row
//! corpora a descent took 9.2–59 s for 17 depths against 21–191 ms for this walk, and the variant
//! testing emptiness with a short-circuiting intersection rather than a full count stayed far
//! behind.
//!
//! Both figures are from the route comparison recorded in
//! [decision 0137](../../../docs/decisions/0137-theta-is-anchored-on-the-occupied-tile-count.md).
//!
//! **What it costs on the request path**, measured end to end through the response trailer's
//! `theta_occupancy_ns` against `treeoflife-1m` (1.05 × 10⁶ rows, 744,241 of them visible to the
//! session, full extent, 2026-09-09): **1.55–2.21 ms** for the first request at a depth, and
//! **1–3 µs** for every request after it, the memo being the whole of the difference. The whole
//! request took 4.2 ms at depth 0 and 157 ms at depth 9. `V_total` beside it is 0.15–0.67 µs,
//! which is why the two have separate stage fields.

use std::ops::Range;

use croaring::Bitmap;
use tessera_store::read::SegmentData;

use crate::compose::EffectiveMask;

/// How many rows one [`EffectiveMask::for_each_visible_run`] call covers.
///
/// **The walk is chunked rather than taken over the whole row space in one call.** That method
/// walks `base` in place only while the overlay diffs are empty; with any diff present it takes
/// the [`EffectiveMask::rows_in_range`] fallback, which materialises the composed mask over the
/// range it is handed. Passing the whole row space would therefore build a copy of a 233-million-
/// row bitmap on every request that arrives after a suppression. Chunking bounds that temporary to
/// one chunk's containers and costs, on the diffs-empty route, one extra cursor reset per chunk.
///
/// 2²² rows is 64 croaring containers — small enough that the temporary is a few hundred
/// kilobytes, large enough that a 233-million-row view is 56 chunks rather than thousands.
///
/// Splitting a run at a chunk boundary changes no answer: [`Walk::visit_run`] carries the last
/// credited tile across runs, so the two halves credit their shared tile once.
const CHUNK_ROWS: u32 = 1 << 22;

/// The depth-`depth` tile index of a Morton code.
///
/// `u64` throughout: at depth 0 the shift is 32, which is undefined on a `u32`. The result is
/// below `4^depth <= 2^32`, so it fits a `u32` for every depth the grid allows.
#[inline]
fn tile_of(code: u32, depth: u8) -> u64 {
    (code as u64) >> (32 - 2 * depth as u32)
}

/// `from + codes[from..].partition_point(|c| c < target)`, by exponential search out from `from`.
///
/// The walk's steps are short where occupied tiles are close together and long where they are far
/// apart, so the search cost follows the step rather than the length of the tail.
fn gallop(codes: &[u32], from: usize, target: u64) -> usize {
    let base = from.min(codes.len());
    let tail = &codes[base..];
    let mut lo = 0usize;
    let mut hi = 1usize;
    while hi <= tail.len() && (tail[hi - 1] as u64) < target {
        lo = hi;
        hi = hi.saturating_mul(2);
    }
    let hi = hi.min(tail.len());
    base + lo + tail[lo..hi].partition_point(|&c| (c as u64) < target)
}

/// One segment's walk state: the depth, the last tile credited, and where the credits go.
struct Walk<'a, F: FnMut(u64)> {
    codes: &'a [u32],
    depth: u8,
    /// The shift from a code to its depth-`depth` tile index.
    shift: u32,
    /// The highest tile index credited so far in this segment, or `u64::MAX` before the first.
    /// A `u32` code cannot produce that index at any depth, so it is a sound "nothing yet".
    last: u64,
    emit: F,
}

impl<F: FnMut(u64)> Walk<'_, F> {
    /// Credit every occupied tile in one segment-local run, ascending.
    fn visit_run(&mut self, run: Range<u32>) {
        let end = run.end as usize;
        let mut i = run.start as usize;
        while i < end {
            let tile = tile_of(self.codes[i], self.depth);
            if tile != self.last {
                self.last = tile;
                (self.emit)(tile);
            }
            // The first code at or above the next tile's low bound. At depth 0 the shift is 32 and
            // `(tile + 1) << 32` exceeds every code, so the gallop lands on `end` and the run
            // finishes in one step — one tile covers the grid.
            let target = (tile + 1) << self.shift;
            i = gallop(&self.codes[..end], i + 1, target).max(i + 1);
        }
    }
}

/// Credit every depth-`depth` tile of `segment` holding a row visible to `mask`, ascending and
/// without repetition.
///
/// `row_base` is where this segment's rows begin in the view's row space, which is the space the
/// mask is over; `segment.morton` is indexed segment-locally.
fn for_each_occupied_tile(
    mask: &EffectiveMask,
    segment: &SegmentData,
    row_base: u32,
    depth: u8,
    emit: impl FnMut(u64),
) {
    let codes = segment.morton.u32();
    let mut walk = Walk {
        codes,
        depth,
        shift: 32 - 2 * depth as u32,
        last: u64::MAX,
        emit,
    };
    let rows = segment.row_count.min(codes.len() as u32);
    let mut start = 0u32;
    while start < rows {
        let end = start.saturating_add(CHUNK_ROWS).min(rows);
        mask.for_each_visible_run(row_base + start..row_base + end, |run| {
            walk.visit_run(run.start - row_base..run.end - row_base);
        });
        start = end;
    }
}

/// `N_occ(depth)` over the whole view: how many depth-`depth` tiles hold at least one row visible
/// to this mask.
///
/// **Never restricted to a viewport.** θ is viewport-invariant (§7.2), so this takes the view's
/// whole row space exactly as `V_total` does. A bbox or a zoom reaching this would make θ move on
/// a pan.
///
/// `segments` is the view's segments with their row bases, in the order
/// `viewport::segments_with_row_bases` returns them.
pub fn occupied_tiles(mask: &EffectiveMask, segments: &[(&SegmentData, u32)], depth: u8) -> u64 {
    debug_assert!(depth <= 16, "the grid is 2^16 x 2^16, so depth 16 is the deepest");
    match segments {
        [] => 0,
        // One segment: its codes ascend, so the walk already emits each tile once and a counter is
        // the whole accumulator.
        [(segment, row_base)] => {
            let mut count = 0u64;
            for_each_occupied_tile(mask, segment, *row_base, depth, |_| count += 1);
            count
        }
        // More than one: a tile can hold rows from several segments, and `N_occ` counts tiles
        // rather than per-segment shares of them. The union is taken over tile indices, which are
        // below `4^16 = 2^32` and so are `u32`s — the same set arithmetic the rest of the engine
        // uses, and O(containers) rather than O(tiles) in memory.
        many => {
            let mut union = Bitmap::new();
            for (segment, row_base) in many {
                for_each_occupied_tile(mask, segment, *row_base, depth, |tile| {
                    debug_assert!(tile < 1 << 32, "a depth-{depth} tile index exceeds u32");
                    union.add(tile as u32);
                });
            }
            union.cardinality()
        }
    }
}

/// What `N_occ(d)` is a function of: the composed mask, the view's row space, and the depth.
///
/// **Named fields rather than a tuple**, on [`crate::cache::RowProjectionKey`]'s argument: four of
/// the terms are `u64`, so a transposition at the construction site would compile, run, and answer
/// one principal's θ from another's occupancy.
///
/// Every term is a reason the composed mask or the row space moved, and the set is
/// [`crate::histogram::MaskIdentity`]'s plus the view and the depth:
///
/// - **`token_id`** — the count is inside one principal's mask, and is never shared across
///   principals. Never the bearer token, and never reused within a process
///   ([`crate::cache::RowProjectionKey`]'s fact 1 is what makes that sound).
/// - **`view`** — `N_occ` is counted over one view's rows, exactly as `V_total` is.
/// - **`depth`** — the quantity is per depth. A session touches a handful of depths, which is why
///   this is evaluated per requested depth rather than for all 17 at once.
/// - **`segments_version`** — row ids mean something only within one geometry, and a flush changes
///   which tiles hold rows.
/// - **`overlay_version`** — a suppression or a deletion removes rows from the composed mask and
///   can empty a tile. The response to a deny is at accept, so the correction may not wait for a
///   refresh.
/// - **the fragment's identity and watermark** — a session may be served a one-generation-stale
///   projection (decision 0044), so two requests at one `segments_version` can compose against
///   different fragments.
///
/// **The attribute filter is not a term and must not become one**: θ is anchored on the unfiltered
/// composed mask (**I12**), so there is no filtered variant of this quantity to key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct OccupancyKey {
    pub token_id: u64,
    pub view: String,
    pub depth: u8,
    pub segments_version: u64,
    pub overlay_version: u64,
    pub fragment_identity: [u8; 32],
    pub fragment_watermark: u64,
}

/// One memoised `N_occ(d)`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OccupiedTiles(pub u64);

impl crate::single_flight::CacheWeight for OccupiedTiles {
    fn cache_weight_bytes(&self) -> u64 {
        // The value is a `u64`; the per-entry floor the cache applies is what actually bounds the
        // entry count, and it is the honest charge for a key holding a view name.
        std::mem::size_of::<u64>() as u64
    }
}
