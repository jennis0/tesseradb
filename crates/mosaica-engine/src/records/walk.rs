//! The walk a page takes its rows by, and the filter evaluated over the stretch ahead of it.
//!
//! A stretch is the part of the view a walk has evaluated the filter over: a range of map cells
//! across every shard in map order, a range of item numbers within one shard in stored order,
//! which never spans two. It starts at the scan position and spans a target number of rows, and
//! when a page empties one that spanned its target before filling, the next is four times longer,
//! so a sparse filter costs a few evaluations per read rather than one per page. The size reached
//! travels in the cursor, so a resumed read continues at it.
//!
//! A stretch's filter is evaluated under the viewer's candidate set, brought forward to the
//! page's generation. In map order that set is narrowed to the items of the stretch's rows the
//! page's mask admits; in stored order, to the stretch's own items. Its answer is held in the row
//! positions of the generation and the mask it was evaluated under, so a stretch is evaluated
//! again when the prefix, the segment set, the overlay or the session's composed mask moves, as
//! it does when a projection served stale is refreshed. Visibility is never held: every page
//! tests every row against the mask composed for that page.
//!
//! Every leaf is evaluated on the row route wherever the column affords one, in both orders, so
//! the two orders test each row by the same rule and return the same rows. A region past the
//! cell budget is answered by the same cover in both, from the one decomposition cache.
//!
//! A filter whose matches an index names, a unique field's `eq` or `in` or a `member_of`, bounds
//! the rows it can match. Where they are no more than a stretch at the ceiling spans, the walk is
//! driven from them: one stretch runs to the end of the view and holds those rows alone, so the
//! read costs the matches the viewer can see rather than the view. The rows are taken, tested and
//! resumed from exactly as a stretch over the view's rows would take them, so either route
//! returns the same rows, pages and positions.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ops::Range;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use croaring::Bitmap;
use mosaica_roaring::for_each_run_in;
use mosaica_store::read::SegmentData;
use mosaica_types::{EntityId, IdentityKey, ShardId};

use super::cursor::{Key, Position};
use super::{RecordsOrder, ResponseEndedBy};
use crate::cancel::CancelToken;
use crate::compose::{EffectiveMask, FilterRows, MaskedSet};
use crate::engine::Engine;
use crate::error::Result;
use crate::filter::{FilterExpr, RegionLeaf};
use crate::figures::MaskIdentity;
use crate::region::RegionVerdict;
use crate::viewport::{
    crossing_domain, filter_refusal, segment_holding, unique_holders, OpenView, ResolvedLeaves,
    RoutedRows, ServedSide,
};
use crate::Generation;

/// The first stretch spans at least this many rows, or candidate items in stored order, whatever
/// the page size.
const STRETCH_MIN: u32 = 4096;
const STRETCH_GROWTH: u32 = 4;
/// The most a stretch holds per row it spans, as `tests/records_memory.rs` measures it. A stored
/// stretch keeps each candidate item with its row, two `u32`s, and while the filter is evaluated
/// the stretch's rows, their ranges and the filter's rows beside them: about 16.3 bytes a row at
/// its peak. A map stretch keeps its items, one `u32` a row while it is evaluated, and their
/// bitmap: about 4.2. Seventeen bytes a row bounds either, so a stretch holds no more than the
/// page ceiling it is derived from.
const STRETCH_BYTES_PER_ROW: usize = 17;
/// The most a driven stretch holds per row it holds, modelled and not measured: each item with
/// its row, eight bytes; the rows' ranges, one a row where the rows are scattered, held twice
/// while the filter is evaluated, sixteen; and four bitmaps of the rows or their items, the
/// candidate among them and the filter's rows, up to four bytes each.
const DRIVEN_BYTES_PER_ROW: usize = 40;

/// What a walk reads a page through, side by side, each side named by its place in
/// [`crate::viewport::ServedView::sides`]. [`PageCx`] answers from the engine.
pub(super) trait WalkCx {
    /// The places of the sides the page reads, ascending
    /// ([`crate::compose::ShardedMask::filtered_leaves`]).
    fn reads(&self) -> &[usize];
    fn shard(&self, place: usize) -> ShardId;
    /// Every segment of the view in the side at `place`, with its first row.
    fn segments(&self, place: usize) -> &[(&SegmentData, u32)];
    /// Panics for a side the page does not read.
    fn leaf(&self, place: usize) -> &EffectiveMask;
    fn total_rows(&self, place: usize) -> u32;
    /// The row `entity` holds in the view in the side at `place`, where it holds one.
    fn row_of(&self, place: usize, entity: u32) -> Option<u32>;
    /// The entity row `local` of `segment`, a segment of the side at `place`, belongs to.
    fn entity_of(&self, place: usize, segment: &SegmentData, local: u32) -> u32;
    /// The viewer's candidate set in the side at `place`'s shard.
    fn candidate(&self, place: usize) -> Result<Bitmap>;
    /// The members of `within` in that candidate set, at the cost of `within`.
    fn candidate_within(&self, place: usize, within: &Bitmap) -> Result<Bitmap>;
    /// A filter's rows over `domain` in the side at `place`, routed under `candidate`, or under
    /// the viewer's whole candidate set there where that is `None`: every leaf takes the row route
    /// where its column affords one, and a region or `member_of` leaf is resolved once a page in
    /// each side.
    fn filter_rows(
        &self,
        place: usize,
        expr: &FilterExpr,
        candidate: Option<&Bitmap>,
        domain: &[Range<u32>],
        per_tile_only: bool,
        cancel: &Option<CancelToken>,
    ) -> Result<RoutedRows>;
    /// The rows of the side at `place` an index bounds one leaf's matches to: a unique field's
    /// `eq` or `in`, as the rows of the items holding the values named that are in the viewer's
    /// candidate set, tested only at the holders; a `member_of`, as the artifact's members the
    /// page's mask admits. `None` for any other expression.
    fn index_rows(&self, place: usize, expr: &FilterExpr) -> Result<Option<Bitmap>>;
    /// The verdict a region leaf reaches in the side at `place`.
    fn region_verdict(
        &self,
        place: usize,
        leaf: &RegionLeaf,
        cancel: &Option<CancelToken>,
    ) -> Result<RegionVerdict>;
    /// Whether a read may be driven from its filter's matches.
    fn driven_reads(&self) -> bool;
    /// Count one driven stretch opened.
    fn note_driven(&self);
    /// The generation the page is walked in, whose row positions a stretch is held in.
    fn generation(&self) -> &Arc<Generation>;
    /// The page's composed mask, which a stretch is held under.
    fn mask_identity(&self) -> MaskIdentity;
}

/// What one page is walked in: the engine, the view resolved and its mask composed for the page,
/// and the generation that resolution came from.
pub(super) struct PageCx<'a> {
    pub(super) engine: &'a Engine,
    pub(super) open: &'a OpenView<'a>,
    pub(super) generation: &'a Arc<Generation>,
    /// The region and `member_of` leaves answered under this page's mask.
    resolved: ResolvedLeaves<'a>,
    reads: Vec<usize>,
}

impl<'a> PageCx<'a> {
    pub(super) fn new(
        engine: &'a Engine,
        open: &'a OpenView<'a>,
        generation: &'a Arc<Generation>,
    ) -> PageCx<'a> {
        let reads = open
            .served
            .filtered_sides(&open.mask)
            .map(|(place, ..)| place)
            .collect();
        PageCx {
            engine,
            open,
            generation,
            resolved: ResolvedLeaves::new(&open.mask),
            reads,
        }
    }

    fn side(&self, place: usize) -> &'a ServedSide<'a> {
        &self.open.served.sides[place]
    }
}

impl WalkCx for PageCx<'_> {
    fn reads(&self) -> &[usize] {
        &self.reads
    }

    fn shard(&self, place: usize) -> ShardId {
        self.side(place).shard.id()
    }

    fn segments(&self, place: usize) -> &[(&SegmentData, u32)] {
        &self.side(place).segments
    }

    fn leaf(&self, place: usize) -> &EffectiveMask {
        self.open
            .served
            .leaf(&self.open.mask, place)
            .expect("a side the page reads")
    }

    fn total_rows(&self, place: usize) -> u32 {
        u32::try_from(self.side(place).data.row_space.total_rows()).unwrap_or(u32::MAX)
    }

    fn row_of(&self, place: usize, entity: u32) -> Option<u32> {
        let row_space = &self.side(place).data.row_space;
        row_space
            .row_of(EntityId::new(u64::from(entity)))
            .map(|row| row.raw())
    }

    fn entity_of(&self, place: usize, segment: &SegmentData, local: u32) -> u32 {
        entity_in(segment, local, &self.engine.identity_key, self.shard(place))
    }

    fn candidate(&self, place: usize) -> Result<Bitmap> {
        let side = self.side(place);
        self.engine
            .filter_candidate(self.open.served.session, side.shard, Some(&side.geometry.fragment))
    }

    fn candidate_within(&self, place: usize, within: &Bitmap) -> Result<Bitmap> {
        let side = self.side(place);
        self.engine.filter_candidate_within(
            self.open.served.session,
            side.shard,
            Some(&side.geometry.fragment),
            within,
        )
    }

    fn filter_rows(
        &self,
        place: usize,
        expr: &FilterExpr,
        candidate: Option<&Bitmap>,
        domain: &[Range<u32>],
        per_tile_only: bool,
        cancel: &Option<CancelToken>,
    ) -> Result<RoutedRows> {
        let engine = self.engine;
        let side = self.side(place);
        let rows_in_ranges: u64 = domain.iter().map(|r| r.len() as u64).sum();
        let body = |route: &dyn Fn(&FilterExpr, bool) -> Result<crate::filter::RoutedFilter>| {
            engine.rows_of_routed(side, route(expr, true)?, domain, rows_in_ranges, per_tile_only)
        };
        let whole;
        let candidate = match candidate {
            Some(candidate) => candidate,
            None => {
                whole = self.candidate(place)?;
                &whole
            }
        };
        let served = &self.open.served;
        engine.route_filters_under(served, side, candidate, &self.resolved, cancel, body)
    }

    fn index_rows(&self, place: usize, expr: &FilterExpr) -> Result<Option<Bitmap>> {
        let side = self.side(place);
        Ok(match expr {
            FilterExpr::Leaf { column, operand } => {
                let Some(keys) = side.shard.filter_columns.unique_keys(column, operand) else {
                    return Ok(None);
                };
                let holders: Bitmap = unique_holders(side.shard, column, &keys)
                    .map_err(filter_refusal)?
                    .into_iter()
                    .collect();
                let visible = self.candidate_within(place, &holders)?;
                Some(
                    visible
                        .iter()
                        .filter_map(|entity| self.row_of(place, entity))
                        .collect(),
                )
            }
            FilterExpr::MemberOf(leaf) => Some(
                self.engine
                    .member_rows(leaf, &self.open.served, side, &self.resolved)
                    .map_err(filter_refusal)?,
            ),
            _ => None,
        })
    }

    fn region_verdict(
        &self,
        place: usize,
        leaf: &RegionLeaf,
        cancel: &Option<CancelToken>,
    ) -> Result<RegionVerdict> {
        let served = &self.open.served;
        let rows = self
            .engine
            .region_rows(leaf, served, self.side(place), &self.resolved, cancel)
            .map_err(filter_refusal)?;
        Ok(rows.verdict)
    }

    fn driven_reads(&self) -> bool {
        self.engine
            .switches
            .driven_reads_enabled
            .load(Ordering::Relaxed)
    }

    fn note_driven(&self) {
        self.engine
            .counters
            .driven_stretches
            .fetch_add(1, Ordering::Relaxed);
    }

    fn generation(&self) -> &Arc<Generation> {
        self.generation
    }

    fn mask_identity(&self) -> MaskIdentity {
        self.open.served.mask_identity
    }
}

/// Each side `cx`'s page reads, with `stretch`'s filter rows there.
fn take_sides<'s>(cx: &'s impl WalkCx, stretch: Option<&'s Stretch>) -> Vec<TakeSide<'s>> {
    cx.reads()
        .iter()
        .map(|&place| TakeSide {
            place,
            shard: cx.shard(place),
            segments: cx.segments(place),
            leaf: cx.leaf(place),
            filter: stretch.and_then(|stretch| stretch.filter.get(place)?.as_ref()),
        })
        .collect()
}

/// [`WalkCx::entity_of`] for a side's shard.
fn entity_in(segment: &SegmentData, local: u32, key: &IdentityKey, shard: ShardId) -> u32 {
    let entity = segment
        .entity_of(local, key, shard)
        .expect("an opened segment's rows invert to its own shard");
    u32::try_from(entity.raw()).expect("entity ids are capped at u32::MAX by I9")
}

/// A stored-order row's key: its shard, then its number there.
fn stored_key(shard: ShardId, entity: u32) -> Key {
    (shard.raw(), u64::from(entity))
}

/// One side a take reads, with the stretch's filter rows there.
pub(super) struct TakeSide<'a> {
    place: usize,
    shard: ShardId,
    segments: &'a [(&'a SegmentData, u32)],
    leaf: &'a EffectiveMask,
    filter: Option<&'a FilterRows>,
}

/// One row a page takes, located in the generation the page was walked in.
pub(super) struct Taken {
    /// The place of the side holding it.
    pub(super) place: usize,
    /// Index into that side's segments, ascending by row base.
    pub(super) seg: usize,
    pub(super) local: u32,
    pub(super) mosaica_id: u64,
    pub(super) entity: u32,
    pub(super) matched: bool,
}

/// Why a walk stopped taking rows.
pub(super) enum Walked {
    /// The page holds as many rows as it asked for.
    Filled,
    /// Nothing remains past the rows taken.
    End,
    /// The response must stop, for time or cancellation, with whatever rows the walk holds.
    Stopped(ResponseEndedBy),
}

pub(super) struct Collected {
    pub(super) rows: Vec<Taken>,
    pub(super) walked: Walked,
    /// The position after every row taken: how far the scan went.
    pub(super) position: Position,
}

/// A response's time budget and cancellation, read before each chunk of a walk's scan and never
/// per row. A walk honours either only where stopping moves the position past the one the
/// response started from, so every response the walk begins moves the scan on. The scan gathers
/// the least key it has not reached first, so one chunk moves it; a response runs past its
/// deadline by at most one stretch's filter evaluation, which is not interrupted, and one chunk.
pub(crate) struct Clock {
    started: Instant,
    budget: Duration,
    pub(super) cancel: Option<CancelToken>,
}

impl Clock {
    pub(crate) fn new(started: Instant, budget: Duration, cancel: Option<CancelToken>) -> Clock {
        Clock {
            started,
            budget,
            cancel,
        }
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(CancelToken::is_cancelled)
    }

    pub(crate) fn out_of_time(&self) -> bool {
        self.started.elapsed() >= self.budget
    }

    /// Why the scan would stop here.
    pub(super) fn stop(&self) -> Option<ResponseEndedBy> {
        if self.cancelled() {
            return Some(ResponseEndedBy::Deadline);
        }
        self.out_of_time().then_some(ResponseEndedBy::BudgetTime)
    }
}

/// A walk through one response: the position reached and the stretch ahead of it.
pub(super) struct Walk {
    filter: Option<FilterExpr>,
    keep_unmatched: bool,
    /// The rows the next stretch spans, across every segment.
    pub(super) target: u32,
    /// The rows a stretch may span, from the byte ceiling a stretch is held to.
    ceiling: u32,
    /// The rows a driven stretch may hold, from the same byte ceiling.
    driven_most: u32,
    /// Whether the next stretch may be driven from the filter's matches: cleared once the filter
    /// is found to bound none, or more than `driven_most`.
    driving: bool,
    stretch: Option<Stretch>,
    pub(super) position: Position,
    /// The scan position the response started from: a stop is honoured only past it.
    origin: Option<Key>,
    /// The coarsest verdict the walk's evaluations have reached, for the response's header.
    pub(super) region: Option<RegionVerdict>,
}

/// The part of the view ahead of a scan position that one filter evaluation covers.
struct Stretch {
    under: Arc<Generation>,
    /// The composed mask the filter was evaluated under: a map stretch's candidate is the items
    /// of its rows the mask admits, and region and `member_of` leaves answer through it.
    mask: MaskIdentity,
    /// The scan position it was opened after.
    from: Option<Key>,
    /// The first key past the stretch; `None` where the stretch runs to the end.
    until: Option<Key>,
    /// The rows the filter admits over the stretch's rows in each side: `None` in a side the
    /// stretch does not reach, and empty where there is no filter.
    filter: Vec<Option<FilterRows>>,
    /// In stored order, the candidate items of the stretch that hold a row in the view, ascending,
    /// each with that row, under the place of the side holding them, ascending. Empty in map
    /// order, whose rows are the segments' own.
    items: Vec<(usize, Vec<(u32, u32)>)>,
    /// Whether the stretch holds only the rows its filter bounds, so a map-order take gathers
    /// from the filter's rows rather than the mask's, and nothing in a side it has none in.
    driven: bool,
    /// Whether the stretch spans its whole target, so a page that empties it before filling has
    /// found the filter sparse there and the next stretch is longer.
    spanned: bool,
}

/// Whether two generations share the row positions and the deny state a stretch was evaluated
/// under.
pub(crate) fn same_publication(a: &Generation, b: &Generation) -> bool {
    a.prefix == b.prefix
        && a.segments_version == b.segments_version
        && a.shards().count() == b.shards().count()
        && a.shards()
            .zip(b.shards())
            .all(|(x, y)| Arc::ptr_eq(&x.overlay, &y.overlay))
}

/// A taken row's key in `order`.
fn key_of(cx: &impl WalkCx, order: RecordsOrder, row: &Taken) -> Key {
    match order {
        RecordsOrder::Map => {
            let (segment, _) = cx.segments(row.place)[row.seg];
            (segment.morton.u32()[row.local as usize], row.mosaica_id)
        }
        RecordsOrder::Stored => stored_key(cx.shard(row.place), row.entity),
    }
}

/// The key just before `key`, or `None` where nothing is.
fn before(key: Key) -> Option<Key> {
    match key {
        (cell, 0) => cell.checked_sub(1).map(|cell| (cell, u64::MAX)),
        (cell, id) => Some((cell, id - 1)),
    }
}

/// The first local row of `segment` past `after` in `(cell, mosaica_id)` order, the order every
/// segment's rows are stored in.
fn first_after(segment: &SegmentData, after: Option<Key>) -> u32 {
    let Some(after) = after else {
        return 0;
    };
    let cells = segment.morton.u32();
    let ids = segment.columns.mosaica_id();
    let (mut lo, mut hi) = (0usize, cells.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if (cells[mid], ids[mid]) <= after {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo as u32
}

/// The first local row of `segment` whose key is `until` or past it, or the segment's end where
/// `until` is `None`.
fn first_at(segment: &SegmentData, until: Option<Key>) -> u32 {
    match until {
        None => segment.row_count,
        Some(until) => first_after(segment, before(until)),
    }
}

/// The first key past the map stretch after `scan` over `sides`: the nearest key at which a
/// segment of any side reaches its share of `target` rows ahead, `None` within where the stretch
/// runs to the end, and `None` where no segment holds a row past `scan`. The segment reaching it
/// holds a row before it, so a stretch always covers a row.
fn map_until(sides: &[TakeSide<'_>], scan: Option<Key>, target: u32) -> Option<Option<Key>> {
    let ahead: Vec<(&SegmentData, u32)> = sides
        .iter()
        .flat_map(|side| side.segments.iter())
        .map(|&(segment, _)| (segment, first_after(segment, scan)))
        .filter(|&(segment, start)| start < segment.row_count)
        .collect();
    if ahead.is_empty() {
        return None;
    }
    let share = (target as usize / ahead.len()).max(1);
    Some(
        ahead
            .iter()
            .filter_map(|&(segment, start)| {
                let reach = start as usize + share;
                (reach < segment.row_count as usize).then(|| {
                    (
                        segment.morton.u32()[reach],
                        segment.columns.mosaica_id()[reach],
                    )
                })
            })
            .min(),
    )
}

/// The stored stretch past `scan` over `sides`, each a side's place and shard, ascending by
/// shard: the next `target` items of the viewer's candidate set in the first side holding one
/// past `scan`, with that side's place and the first key past them. `candidate` is asked only of
/// the sides the stretch reaches. A stretch never spans two shards: one that takes the last of
/// its side's candidate ends at the next side's first key, or runs to the end after the last
/// side. `None` where no side holds an item past `scan`.
fn stored_stretch(
    sides: &[(usize, ShardId)],
    scan: Option<Key>,
    target: u32,
    mut candidate: impl FnMut(usize) -> Result<Bitmap>,
) -> Result<Option<(usize, Bitmap, Option<Key>)>> {
    for (i, &(place, shard)) in sides.iter().enumerate() {
        let from: u64 = match scan {
            Some((held, _)) if held > shard.raw() => continue,
            Some((held, number)) if held == shard.raw() => match number.checked_add(1) {
                Some(from) => from,
                None => continue,
            },
            _ => 0,
        };
        if from > u64::from(u32::MAX) {
            continue;
        }
        let candidate = candidate(place)?;
        let before = if from == 0 {
            0
        } else {
            candidate.rank((from - 1) as u32)
        };
        if before >= candidate.cardinality() {
            continue;
        }
        let until: Option<u32> = u32::try_from(before + u64::from(target))
            .ok()
            .and_then(|rank| candidate.select(rank));
        // Bounded by the candidate's last item, not the entity space: a range to `u32::MAX` is a
        // container for every 65,536 entities, some 3 MB, before the intersection empties it.
        let last = candidate.maximum().expect("the candidate holds an item past `from`");
        let mut range = Bitmap::new();
        match until {
            Some(until) => range.add_range(from as u32..until),
            None => range.add_range(from as u32..=last),
        }
        range.and_inplace(&candidate);
        let until = match until {
            Some(until) => Some(stored_key(shard, until)),
            None => sides.get(i + 1).map(|&(_, next)| stored_key(next, 0)),
        };
        return Ok(Some((place, range, until)));
    }
    Ok(None)
}

impl Walk {
    /// A walk from `position` whose stretches hold no more than `stretch_bytes`. The first
    /// stretch spans `stretch` rows where a resumed read carries the size it reached, and the
    /// page size otherwise.
    pub(super) fn new(
        filter: Option<FilterExpr>,
        keep_unmatched: bool,
        stretch: u32,
        stretch_bytes: usize,
        position: Position,
    ) -> Walk {
        let ceiling = u32::try_from(stretch_bytes / STRETCH_BYTES_PER_ROW)
            .unwrap_or(u32::MAX)
            .max(STRETCH_MIN);
        let driven_most = u32::try_from(stretch_bytes / DRIVEN_BYTES_PER_ROW)
            .unwrap_or(u32::MAX)
            .max(STRETCH_MIN);
        Walk {
            filter,
            keep_unmatched,
            target: stretch.clamp(STRETCH_MIN, ceiling),
            ceiling,
            driven_most,
            driving: true,
            stretch: None,
            origin: position.scan,
            position,
            region: None,
        }
    }

    /// Take up to `need` rows past the position, in the walk's order. The position is left where
    /// it was; the caller commits the one [`Collected`] carries, or one of its own after cutting
    /// the page.
    pub(super) fn collect(
        &mut self,
        cx: &impl WalkCx,
        need: usize,
        clock: &mut Clock,
    ) -> Result<Collected> {
        let Position { order, mut scan } = self.position;
        let mut rows: Vec<Taken> = Vec::new();
        let walked = loop {
            if self.stretch.as_ref().is_some_and(|stretch| {
                !same_publication(&stretch.under, cx.generation())
                    || stretch.mask != cx.mask_identity()
                    || stretch.from > scan
            }) {
                self.stretch = None;
            }
            if self.stretch.is_none() {
                if let Some(reason) = clock.stop() {
                    if !rows.is_empty() || scan > self.origin {
                        break Walked::Stopped(reason);
                    }
                }
                self.note_regions(cx, &clock.cancel)?;
                let opened = match self.driving_rows(cx)? {
                    Some(rows) => self.open_driven_stretch(cx, scan, &rows, &clock.cancel)?,
                    None => match order {
                        RecordsOrder::Map => self.open_map_stretch(cx, scan, &clock.cancel)?,
                        RecordsOrder::Stored => {
                            self.open_stored_stretch(cx, scan, &clock.cancel)?
                        }
                    },
                };
                match opened {
                    Some(stretch) => self.stretch = Some(stretch),
                    None => break Walked::End,
                }
            }
            let stretch = self.stretch.as_ref().expect("a stretch is open");
            let sides = take_sides(cx, Some(stretch));
            let take = Take {
                sides: &sides,
                until: stretch.until,
                items: &stretch.items,
                driven: stretch.driven,
                keep_unmatched: self.keep_unmatched,
                moved: scan > self.origin,
                cx,
            };
            let stopped = match order {
                RecordsOrder::Map => take.map(scan, need, clock, &mut rows),
                RecordsOrder::Stored => take.stored(scan, need, clock, &mut rows),
            };
            let (until, spanned) = (stretch.until, stretch.spanned);
            if let Some((reason, first_unreached)) = stopped {
                // Every row before the first one the scan did not reach has been taken or passed
                // over, so the scan position moves to just before it. The stretch outlasted the
                // response, so the next response evaluates a shorter one.
                scan = scan.max(before(first_unreached));
                self.target = (self.target / STRETCH_GROWTH).max(STRETCH_MIN);
                break Walked::Stopped(reason);
            }
            if rows.len() == need {
                let row = rows.last().expect("a filled page holds a row");
                scan = Some(key_of(cx, order, row));
                break Walked::Filled;
            }
            self.stretch = None;
            if spanned {
                self.target = self.target.saturating_mul(STRETCH_GROWTH).min(self.ceiling);
            }
            let Some(until) = until else {
                scan = Some((u32::MAX, u64::MAX));
                break Walked::End;
            };
            scan = scan.max(before(until));
        };
        Ok(Collected {
            rows,
            walked,
            position: Position { order, scan },
        })
    }

    /// The position just after one taken row, for a page cut before the rows that followed it.
    pub(super) fn position_at(&self, cx: &impl WalkCx, row: &Taken) -> Position {
        let key = key_of(cx, self.position.order, row);
        Position {
            order: self.position.order,
            scan: Some(key),
        }
    }

    /// A stretch past `scan` holding `filter` and `items`, under the page's publication and mask.
    fn stretch(
        cx: &impl WalkCx,
        scan: Option<Key>,
        until: Option<Key>,
        filter: Vec<Option<FilterRows>>,
        items: Vec<(usize, Vec<(u32, u32)>)>,
        driven: bool,
    ) -> Stretch {
        Stretch {
            under: Arc::clone(cx.generation()),
            mask: cx.mask_identity(),
            from: scan,
            until,
            filter,
            items,
            driven,
            spanned: true,
        }
    }

    /// The map stretch past `scan`: every row of every segment of every side the page reads
    /// whose key is below [`map_until`]'s. `None` where no segment holds a row past `scan`.
    fn open_map_stretch(
        &mut self,
        cx: &impl WalkCx,
        scan: Option<Key>,
        cancel: &Option<CancelToken>,
    ) -> Result<Option<Stretch>> {
        let sides = take_sides(cx, None);
        let Some(until) = map_until(&sides, scan, self.target) else {
            return Ok(None);
        };
        let mut filter: Vec<Option<FilterRows>> = Vec::new();
        if let Some(expr) = &self.filter {
            filter.resize_with(places(cx), || None);
            for side in &sides {
                let parts: Vec<(usize, Range<u32>)> = side
                    .segments
                    .iter()
                    .enumerate()
                    .filter_map(|(s, &(segment, _))| {
                        let (start, end) = (first_after(segment, scan), first_at(segment, until));
                        (start < end).then_some((s, start..end))
                    })
                    .collect();
                // The items of the stretch's rows this page's mask admits, so an entity-space
                // scan costs the stretch and not the view. The stretch is held only while the
                // mask is the one it was opened under.
                let mut entities: Vec<u32> = Vec::new();
                for (s, range) in &parts {
                    let (segment, base) = side.segments[*s];
                    let visible = side
                        .leaf
                        .rows_in_range(base + range.start..base + range.end);
                    entities.extend(
                        visible
                            .iter()
                            .map(|row| cx.entity_of(side.place, segment, row - base)),
                    );
                }
                entities.sort_unstable();
                let mut candidate = Bitmap::of(&entities);
                candidate.and_inplace(&cx.candidate(side.place)?);
                let row_bases: Vec<u32> = side.segments.iter().map(|&(_, base)| base).collect();
                let domain = crossing_domain(&[parts], &row_bases);
                let routed =
                    cx.filter_rows(side.place, expr, Some(&candidate), &domain, true, cancel)?;
                filter[side.place] = Some(routed.rows);
            }
        }
        Ok(Some(Self::stretch(
            cx,
            scan,
            until,
            filter,
            Vec::new(),
            false,
        )))
    }

    /// The stored stretch past `scan` ([`stored_stretch`]), and of its items the ones that hold a
    /// row in the view. A stretch cut at its shard's end, or the view's, before it spans its
    /// target says nothing of how sparse the filter is, so the next one is not grown for it.
    /// `None` where no side's candidate holds an item past `scan`.
    fn open_stored_stretch(
        &mut self,
        cx: &impl WalkCx,
        scan: Option<Key>,
        cancel: &Option<CancelToken>,
    ) -> Result<Option<Stretch>> {
        let sides: Vec<(usize, ShardId)> = cx
            .reads()
            .iter()
            .map(|&place| (place, cx.shard(place)))
            .collect();
        let Some((place, range, until)) =
            stored_stretch(&sides, scan, self.target, |place| cx.candidate(place))?
        else {
            return Ok(None);
        };
        let mut items: Vec<(u32, u32)> = Vec::with_capacity(range.cardinality() as usize);
        items.extend(
            range
                .iter()
                .filter_map(|entity| Some((entity, cx.row_of(place, entity)?))),
        );
        let mut filter: Vec<Option<FilterRows>> = Vec::new();
        if let Some(expr) = &self.filter {
            // The stretch's own rows, which the view need not hold contiguously.
            let rows = Bitmap::of(&items.iter().map(|&(_, row)| row).collect::<Vec<_>>());
            let mut domain: Vec<Range<u32>> = Vec::new();
            for_each_run_in(&rows, 0..cx.total_rows(place), &mut |run| domain.push(run));
            filter.resize_with(places(cx), || None);
            let routed = cx.filter_rows(place, expr, Some(&range), &domain, true, cancel)?;
            filter[place] = Some(routed.rows);
        }
        let mut stretch = Self::stretch(cx, scan, until, filter, vec![(place, items)], false);
        stretch.spanned = until.is_some_and(|(shard, _)| shard == cx.shard(place).raw());
        Ok(Some(stretch))
    }

    /// The verdicts of the filter's region leaves under this page, noted wherever a stretch is
    /// opened on either route, so the head's verdict does not depend on which route answers or on
    /// whether any row remains.
    fn note_regions(&mut self, cx: &impl WalkCx, cancel: &Option<CancelToken>) -> Result<()> {
        if let Some(expr) = &self.filter {
            let verdict = region_verdict(cx, expr, cancel)?;
            self.region = RegionVerdict::coarsest(self.region, verdict);
        }
        Ok(())
    }

    /// The rows the next stretch is driven from, or `None` where it walks the view: under
    /// `keep_unmatched`, which serves every visible row, and wherever [`driving_rows`] declines.
    fn driving_rows(&mut self, cx: &impl WalkCx) -> Result<Option<Vec<(usize, Bitmap)>>> {
        let Some(expr) = self.filter.as_ref().filter(|_| self.driving && !self.keep_unmatched)
        else {
            return Ok(None);
        };
        let rows = driving_rows(cx, expr, self.driven_most)?;
        self.driving = rows.is_some();
        Ok(rows)
    }

    /// The visible rows the filter matches, summed over every side the page reads, for the head
    /// of a read not yet begun, and the coarsest verdict the evaluation's region leaves reached.
    /// Where the read is driven, the count is taken from the first driven stretch, which is kept
    /// for the first page. Otherwise, and always under `keep_unmatched`, whose read walks the
    /// view, it is one evaluation of the filter over the whole view in each side, on the route
    /// every page takes.
    pub(super) fn count(
        &mut self,
        cx: &impl WalkCx,
        cancel: &Option<CancelToken>,
    ) -> Result<(u64, Option<RegionVerdict>)> {
        if let Some(matched) = self.count_driven(cx, cancel)? {
            return Ok((matched, None));
        }
        let expr = self
            .filter
            .as_ref()
            .expect("a count of matches has a filter");
        let (mut matched, mut region) = (0u64, None);
        for &place in cx.reads() {
            let whole_view = 0..cx.total_rows(place);
            let domain = std::slice::from_ref(&whole_view);
            let routed = cx.filter_rows(place, expr, None, domain, false, cancel)?;
            matched += cx.leaf(place).count_intersection(routed.rows.rows());
            region = RegionVerdict::coarsest(region, routed.region);
        }
        Ok((matched, region))
    }

    /// The matching rows the mask admits, summed over every side, where the read's first stretch
    /// is driven. That stretch is opened here, under the first page's mask, and kept for the first
    /// page, so the count costs no evaluation the read would not make. `None` where the read walks
    /// the view.
    fn count_driven(
        &mut self,
        cx: &impl WalkCx,
        cancel: &Option<CancelToken>,
    ) -> Result<Option<u64>> {
        let Some(rows) = self.driving_rows(cx)? else {
            return Ok(None);
        };
        self.note_regions(cx, cancel)?;
        let Some(stretch) = self.open_driven_stretch(cx, self.position.scan, &rows, cancel)? else {
            return Ok(Some(0));
        };
        let matched = stretch
            .filter
            .iter()
            .enumerate()
            .filter_map(|(place, rows)| {
                let rows = rows.as_ref()?;
                Some(cx.leaf(place).count_intersection(rows.rows()))
            })
            .sum();
        self.stretch = Some(stretch);
        Ok(Some(matched))
    }

    /// The driven stretch past `scan`: the rows of `rows` whose key lies past `scan`, running to
    /// the end of the view, with the filter evaluated in each side over those rows alone under the
    /// viewer's candidate set among their items. In stored order only items in that set are taken,
    /// as the stored walk takes them; in map order every row is, and the mask tests it, as the map
    /// walk does. The filter's rows are held to the stretch's rows. `None` where no row lies past
    /// `scan`.
    fn open_driven_stretch(
        &mut self,
        cx: &impl WalkCx,
        scan: Option<Key>,
        rows: &[(usize, Bitmap)],
        cancel: &Option<CancelToken>,
    ) -> Result<Option<Stretch>> {
        cx.note_driven();
        let order = self.position.order;
        let expr = self.filter.as_ref().expect("only a filter drives a stretch");
        let mut filter: Vec<Option<FilterRows>> = Vec::new();
        filter.resize_with(places(cx), || None);
        let mut held: Vec<(usize, Vec<(u32, u32)>)> = Vec::new();
        for &(place, ref rows) in rows {
            let (segments, shard) = (cx.segments(place), cx.shard(place));
            let mut items: Vec<(u32, u32)> = Vec::new();
            for row in rows.iter() {
                let (seg, local) =
                    segment_holding(segments, row).expect("the first segment's rows begin at 0");
                let (segment, _) = segments[seg];
                let entity = cx.entity_of(place, segment, local);
                let key = match order {
                    RecordsOrder::Map => (
                        segment.morton.u32()[local as usize],
                        segment.columns.mosaica_id()[local as usize],
                    ),
                    RecordsOrder::Stored => stored_key(shard, entity),
                };
                if scan.is_none_or(|scan| key > scan) {
                    items.push((entity, row));
                }
            }
            items.sort_unstable();
            let entities: Bitmap = items.iter().map(|&(entity, _)| entity).collect();
            let candidate = cx.candidate_within(place, &entities)?;
            drop(entities);
            if order == RecordsOrder::Stored {
                items.retain(|&(entity, _)| candidate.contains(entity));
            }
            if items.is_empty() {
                continue;
            }
            let ahead: Bitmap = items.iter().map(|&(_, row)| row).collect();
            match order {
                RecordsOrder::Stored => held.push((place, items)),
                RecordsOrder::Map => drop(items),
            }
            let mut domain: Vec<Range<u32>> = Vec::new();
            for_each_run_in(&ahead, 0..cx.total_rows(place), &mut |run| domain.push(run));
            let routed = cx.filter_rows(place, expr, Some(&candidate), &domain, true, cancel)?;
            filter[place] = Some(match routed.rows {
                FilterRows::Viewport { rows, domain } => FilterRows::Viewport {
                    rows: rows.and(&ahead),
                    domain,
                },
                FilterRows::Complete(rows) => FilterRows::Viewport {
                    rows: rows.and(&ahead),
                    domain,
                },
            });
        }
        if filter.iter().all(Option::is_none) {
            return Ok(None);
        }
        Ok(Some(Self::stretch(cx, scan, None, filter, held, true)))
    }
}

/// How many places a stretch's per-side rows span: one past the last side the page reads.
fn places(cx: &impl WalkCx) -> usize {
    cx.reads().last().map_or(0, |&place| place + 1)
}

/// The coarsest verdict `expr`'s region leaves reach under this page in every side it reads,
/// each resolved once a page in each side.
fn region_verdict(
    cx: &impl WalkCx,
    expr: &FilterExpr,
    cancel: &Option<CancelToken>,
) -> Result<Option<RegionVerdict>> {
    Ok(match expr {
        FilterExpr::Region(leaf) => {
            let mut out = None;
            for &place in cx.reads() {
                let verdict = cx.region_verdict(place, leaf, cancel)?;
                out = RegionVerdict::coarsest(out, Some(verdict));
            }
            out
        }
        FilterExpr::AllOf(kids) | FilterExpr::AnyOf(kids) | FilterExpr::NoneOf(kids) => {
            let mut out = None;
            for kid in kids {
                out = RegionVerdict::coarsest(out, region_verdict(cx, kid, cancel)?);
            }
            out
        }
        FilterExpr::Leaf { .. } | FilterExpr::MemberOf(_) => None,
    })
}

/// The rows of the view `expr` can match, where they are no more than `most`, or `None` where
/// the filter's matches must be found by walking the view. This is the one rule that decides
/// between the two routes: a walk costs the visible rows it passes, and a driven read costs the
/// rows returned here, so a read is driven wherever an index bounds its matches to what one
/// driven stretch may hold, summed over every side the page reads. Each bound is taken inside
/// the viewer's visible set before it is counted, so which route answers depends only on what the
/// viewer can see.
fn driving_rows(
    cx: &impl WalkCx,
    expr: &FilterExpr,
    most: u32,
) -> Result<Option<Vec<(usize, Bitmap)>>> {
    if !cx.driven_reads() {
        return Ok(None);
    }
    let (mut out, mut held) = (Vec::with_capacity(cx.reads().len()), 0u64);
    for &place in cx.reads() {
        let Some(rows) = bounded_rows(cx, place, expr)? else {
            return Ok(None);
        };
        held += rows.cardinality();
        if held > u64::from(most) {
            return Ok(None);
        }
        out.push((place, rows));
    }
    Ok(Some(out))
}

/// The rows of the view in the side at `place` an index says `expr` can match at most, or `None`
/// where no index bounds them: a leaf's as [`WalkCx::index_rows`] gives them, an `all_of`'s as the
/// rows every bounded kid admits, and an `any_of`'s whose every kid is bounded as their union.
fn bounded_rows(cx: &impl WalkCx, place: usize, expr: &FilterExpr) -> Result<Option<Bitmap>> {
    Ok(match expr {
        FilterExpr::Leaf { .. } | FilterExpr::MemberOf(_) => cx.index_rows(place, expr)?,
        FilterExpr::AllOf(kids) => {
            let mut out: Option<Bitmap> = None;
            for kid in kids {
                if let Some(rows) = bounded_rows(cx, place, kid)? {
                    out = Some(match out {
                        Some(held) => held.and(&rows),
                        None => rows,
                    });
                }
            }
            out
        }
        FilterExpr::AnyOf(kids) => {
            let mut out = Bitmap::new();
            for kid in kids {
                match bounded_rows(cx, place, kid)? {
                    Some(rows) => out.or_inplace(&rows),
                    None => return Ok(None),
                }
            }
            Some(out)
        }
        FilterExpr::NoneOf(_) | FilterExpr::Region(_) => None,
    })
}

/// One segment's rows in a map stretch, gathered through its side's leaf a chunk at a time: the
/// first chunk is its share of what the page still needs, at least [`CHUNK_MIN`] rows, among the
/// runs with rows ahead, and at least [`CHUNK_FLOOR`]; each after it is twice the one before.
struct SegmentRun<'a> {
    side: &'a TakeSide<'a>,
    seg: usize,
    segment: &'a SegmentData,
    base: u32,
    /// The first local row not yet gathered, and the end of the stretch in this segment.
    next: u32,
    end: u32,
    chunk: u32,
    /// Gathered rows, local, each with whether it matched the filter.
    buffered: Vec<(u32, bool)>,
    at: usize,
}

impl SegmentRun<'_> {
    /// The next chunk of this segment's part of the stretch, through its side's leaf.
    fn gather(&mut self, driven: bool, keep_unmatched: bool) {
        let hi = self.end.min(self.next.saturating_add(self.chunk));
        let range = self.base + self.next..self.base + hi;
        self.buffered.clear();
        self.at = 0;
        self.next = hi;
        self.chunk = self.chunk.saturating_mul(2);
        let (leaf, filter) = (self.side.leaf, self.side.filter);
        if driven {
            let Some(filter) = filter else {
                return;
            };
            let base = self.base;
            let buffered = &mut self.buffered;
            for_each_run_in(filter.rows(), range, &mut |run| {
                buffered.extend(
                    run.filter(|&row| leaf.contains_row(row))
                        .map(|row| (row - base, true)),
                );
            });
            return;
        }
        let visible = leaf.rows_in_range(range);
        match filter {
            None => self
                .buffered
                .extend(visible.iter().map(|row| (row - self.base, true))),
            Some(filter) if keep_unmatched => {
                let matched = visible.and(filter.rows());
                self.buffered.extend(
                    visible
                        .iter()
                        .map(|row| (row - self.base, matched.contains(row))),
                );
            }
            Some(filter) => self.buffered.extend(
                visible
                    .and(filter.rows())
                    .iter()
                    .map(|row| (row - self.base, true)),
            ),
        }
    }

    fn key(&self, local: u32) -> Key {
        let local = local as usize;
        (
            self.segment.morton.u32()[local],
            self.segment.columns.mosaica_id()[local],
        )
    }

    /// The first key in this segment the scan has not passed: the next gathered row not yet
    /// taken, or the first row not yet gathered. `None` where the segment's part of the stretch
    /// is spent.
    fn first_unreached(&self) -> Option<Key> {
        if self.at < self.buffered.len() {
            return Some(self.key(self.buffered[self.at].0));
        }
        (self.next < self.end).then(|| self.key(self.next))
    }
}

/// How many of a stored stretch's items are tested between two readings of the clock.
const STORED_CHUNK: usize = 4096;
/// The fewest rows the first chunks of a map take's runs span between them, whatever the page
/// still needs, so a response the clock stops still moves the scan.
const CHUNK_MIN: u32 = 1024;
/// The fewest rows one run's first chunk spans, however many runs share [`CHUNK_MIN`].
const CHUNK_FLOOR: u32 = 64;

/// One stretch's rows being taken into a page, from every side the page reads.
struct Take<'a, C: WalkCx> {
    /// Ascending by place.
    sides: &'a [TakeSide<'a>],
    /// The first key past the stretch; `None` where it runs to the end.
    until: Option<Key>,
    /// A stored stretch's items, each side's under its place.
    items: &'a [(usize, Vec<(u32, u32)>)],
    driven: bool,
    keep_unmatched: bool,
    /// Whether the scan has already passed the position the response started from.
    moved: bool,
    cx: &'a C,
}

impl<C: WalkCx> Take<'_, C> {
    /// The stop the clock asks for, where honouring it moves the position: a row is held, the
    /// scan had passed the response's start before this take, or the first key the scan has not
    /// reached lies past the first row the take began at.
    fn stop(
        &self,
        clock: &Clock,
        rows: &[Taken],
        first_unreached: Key,
        began_at: Key,
    ) -> Option<(ResponseEndedBy, Key)> {
        let reason = clock.stop()?;
        (!rows.is_empty() || self.moved || first_unreached > began_at)
            .then_some((reason, first_unreached))
    }

    /// Take map-order rows past `scan` and in the stretch until `rows` holds `need`: each
    /// segment's rows that its side's leaf admits, and the filter where rows must match, merged
    /// across every side's segments by `(cell, mosaica_id)`. The least key the scan has not
    /// reached is taken where it is a gathered row, and gathered where it is not, so the clock,
    /// read before each chunk, can stop the scan after one. `Some` where it did, with the first
    /// key the scan did not reach.
    fn map(
        &self,
        scan: Option<Key>,
        need: usize,
        clock: &Clock,
        rows: &mut Vec<Taken>,
    ) -> Option<(ResponseEndedBy, Key)> {
        let sides = self.sides;
        let mut runs: Vec<SegmentRun<'_>> = sides
            .iter()
            .flat_map(|side| {
                side.segments
                    .iter()
                    .enumerate()
                    .map(move |(seg, &(segment, base))| SegmentRun {
                        side,
                        seg,
                        segment,
                        base,
                        next: first_after(segment, scan),
                        end: first_at(segment, self.until),
                        chunk: 0,
                        buffered: Vec::new(),
                        at: 0,
                    })
            })
            .collect();
        // The page's need is shared among the runs with rows ahead, so a page over many runs
        // gathers about what one over a single run does.
        let ahead = runs.iter().filter(|run| run.next < run.end).count();
        let chunk = u32::try_from(need - rows.len())
            .unwrap_or(u32::MAX)
            .max(CHUNK_MIN)
            .div_ceil(u32::try_from(ahead.max(1)).unwrap_or(u32::MAX))
            .max(CHUNK_FLOOR);
        for run in &mut runs {
            run.chunk = chunk;
        }
        // Every run under the first key in it the scan has not reached.
        let mut unreached: BinaryHeap<Reverse<(Key, usize)>> = runs
            .iter()
            .enumerate()
            .filter_map(|(i, run)| run.first_unreached().map(|key| Reverse((key, i))))
            .collect();
        let &Reverse((began_at, _)) = unreached.peek()?;
        while rows.len() < need {
            let Reverse((key, i)) = unreached.pop()?;
            let run = &mut runs[i];
            if run.at < run.buffered.len() {
                let (local, matched) = run.buffered[run.at];
                let mosaica_id = run.segment.columns.mosaica_id()[local as usize];
                rows.push(Taken {
                    place: run.side.place,
                    seg: run.seg,
                    local,
                    mosaica_id,
                    entity: self.cx.entity_of(run.side.place, run.segment, local),
                    matched,
                });
                run.at += 1;
            } else {
                if let Some(stop) = self.stop(clock, rows, key, began_at) {
                    return Some(stop);
                }
                run.gather(self.driven, self.keep_unmatched);
            }
            if let Some(key) = runs[i].first_unreached() {
                unreached.push(Reverse((key, i)));
            }
        }
        None
    }

    /// Take stored-order rows past `scan` from the stretch's items until `rows` holds `need`,
    /// side by side in shard order: each item whose row its side's leaf admits, and that matches
    /// where rows must match. `Some` where the clock stopped the scan, with the first key it did
    /// not reach.
    fn stored(
        &self,
        scan: Option<Key>,
        need: usize,
        clock: &Clock,
        rows: &mut Vec<Taken>,
    ) -> Option<(ResponseEndedBy, Key)> {
        let mut began_at: Option<Key> = None;
        let mut tested = 0usize;
        for (place, items) in self.items {
            let i = self.sides.binary_search_by_key(place, |side| side.place);
            let side = &self.sides[i.expect("a stored stretch's side is read")];
            let start = match scan {
                None => 0,
                Some(scan) => {
                    items.partition_point(|&(entity, _)| stored_key(side.shard, entity) <= scan)
                }
            };
            for &(entity, row) in &items[start..] {
                if rows.len() == need {
                    return None;
                }
                let key = stored_key(side.shard, entity);
                let began = *began_at.get_or_insert(key);
                if tested.is_multiple_of(STORED_CHUNK) {
                    if let Some(stop) = self.stop(clock, rows, key, began) {
                        return Some(stop);
                    }
                }
                tested += 1;
                if !side.leaf.contains_row(row) {
                    continue;
                }
                let matched = side.filter.is_none_or(|filter| filter.rows().contains(row));
                if !matched && !self.keep_unmatched {
                    continue;
                }
                let (seg, local) = segment_holding(side.segments, row)
                    .expect("the first segment's rows begin at 0");
                rows.push(Taken {
                    place: *place,
                    seg,
                    local,
                    mosaica_id: side.segments[seg].0.columns.mosaica_id()[local as usize],
                    entity,
                    matched,
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    //! Reads over several shards against one shard holding every row, walked page by page by
    //! [`Walk::collect`] as a response walks them, through a [`WalkCx`] answering from fixtures.

    use super::*;
    use crate::filter::FilterOperand;
    use crate::select::fixture::{points, segment_of, Written};
    use mosaica_lifecycle::{IngestBuffer, Overlay};

    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use rustc_hash::FxHashSet;
    use std::cell::Cell;

    /// One page's rows, each as its `mosaica_id` and whether it matched, the position after it,
    /// and the stretch its cursor carries.
    type Page = (Vec<(u64, bool)>, Position, u32);

    /// One shard of a fixture view. An item's number is its row, so stored order is row order.
    struct FakeSide<'w> {
        shard: ShardId,
        segments: Vec<(&'w SegmentData, u32)>,
        leaf: EffectiveMask,
        /// The rows the filter matches, and an index bounds a leaf to where the view is indexed.
        matching: Bitmap,
        candidate: Bitmap,
    }

    impl<'w> FakeSide<'w> {
        /// `written`, each segment's rows after the ones before it, seeing the rows whose
        /// identity `visible` holds and matching the ones `matching` holds, every item a
        /// candidate.
        fn of(
            shard: u32,
            written: &[&'w Written],
            visible: &FxHashSet<u64>,
            matching: &FxHashSet<u64>,
        ) -> FakeSide<'w> {
            let mut base = 0u32;
            let segments: Vec<(&SegmentData, u32)> = written
                .iter()
                .map(|w| {
                    let at = base;
                    base += w.ids.len() as u32;
                    (&w.data, at)
                })
                .collect();
            let rows_in = |ids: &FxHashSet<u64>| {
                let mut rows = Bitmap::new();
                for (w, &(_, base)) in written.iter().zip(&segments) {
                    rows.add_many(&w.rows_of(ids).iter().map(|r| r + base).collect::<Vec<_>>());
                }
                rows
            };
            FakeSide {
                shard: ShardId::new(shard).unwrap(),
                leaf: EffectiveMask::of_rows(rows_in(visible)),
                matching: rows_in(matching),
                candidate: Bitmap::from_range(0..base),
                segments,
            }
        }

        fn total(&self) -> u32 {
            self.segments
                .last()
                .map_or(0, |&(segment, base)| base + segment.row_count)
        }
    }

    struct FakeCx<'w> {
        sides: Vec<FakeSide<'w>>,
        reads: Vec<usize>,
        /// Whether a leaf's matches are bounded by an index, so a read may be driven.
        indexed: bool,
        driven: Cell<u32>,
        generation: Arc<Generation>,
    }

    impl<'w> FakeCx<'w> {
        fn of(sides: Vec<FakeSide<'w>>, indexed: bool) -> FakeCx<'w> {
            let generation =
                Generation::synthetic("v00000", 1, 0, Overlay::new(), IngestBuffer::new());
            FakeCx {
                reads: (0..sides.len()).collect(),
                sides,
                indexed,
                driven: Cell::new(0),
                generation: Arc::new(generation),
            }
        }
    }

    impl WalkCx for FakeCx<'_> {
        fn reads(&self) -> &[usize] {
            &self.reads
        }

        fn shard(&self, place: usize) -> ShardId {
            self.sides[place].shard
        }

        fn segments(&self, place: usize) -> &[(&SegmentData, u32)] {
            &self.sides[place].segments
        }

        fn leaf(&self, place: usize) -> &EffectiveMask {
            assert!(self.reads.contains(&place), "a side the page reads");
            &self.sides[place].leaf
        }

        fn total_rows(&self, place: usize) -> u32 {
            self.sides[place].total()
        }

        fn row_of(&self, place: usize, entity: u32) -> Option<u32> {
            (entity < self.sides[place].total()).then_some(entity)
        }

        fn entity_of(&self, place: usize, segment: &SegmentData, local: u32) -> u32 {
            let segments = &self.sides[place].segments;
            let &(_, base) = segments
                .iter()
                .find(|(held, _)| std::ptr::eq(*held, segment))
                .expect("a segment of the side");
            base + local
        }

        fn candidate(&self, place: usize) -> Result<Bitmap> {
            Ok(self.sides[place].candidate.clone())
        }

        fn candidate_within(&self, place: usize, within: &Bitmap) -> Result<Bitmap> {
            Ok(within.and(&self.sides[place].candidate))
        }

        fn filter_rows(
            &self,
            place: usize,
            _: &FilterExpr,
            candidate: Option<&Bitmap>,
            domain: &[Range<u32>],
            _: bool,
            _: &Option<CancelToken>,
        ) -> Result<RoutedRows> {
            let side = &self.sides[place];
            let mut within = Bitmap::new();
            for range in domain {
                within.add_range(range.clone());
            }
            let rows = side
                .matching
                .and(candidate.unwrap_or(&side.candidate))
                .and(&within);
            Ok(RoutedRows {
                rows: FilterRows::Complete(rows),
                region: None,
                unmasked: None,
            })
        }

        fn index_rows(&self, place: usize, _: &FilterExpr) -> Result<Option<Bitmap>> {
            let side = &self.sides[place];
            Ok(self.indexed.then(|| side.matching.and(&side.candidate)))
        }

        fn region_verdict(
            &self,
            _: usize,
            _: &RegionLeaf,
            _: &Option<CancelToken>,
        ) -> Result<RegionVerdict> {
            unreachable!("no fixture filter holds a region")
        }

        fn driven_reads(&self) -> bool {
            true
        }

        fn note_driven(&self) {
            self.driven.set(self.driven.get() + 1);
        }

        fn generation(&self) -> &Arc<Generation> {
            &self.generation
        }

        fn mask_identity(&self) -> MaskIdentity {
            MaskIdentity::of(0, [0; 32], 1, 1, &[])
        }
    }

    /// A unique field's `eq`, as far as a fixture can tell.
    fn unique_eq() -> FilterExpr {
        FilterExpr::Leaf {
            column: "u".to_string(),
            operand: FilterOperand::TextEquals("x".to_string()),
        }
    }

    const BYTES: usize = 64 << 20;
    /// A ceiling so low a driven stretch holds the fewest rows any may.
    const SMALL: usize = 1 << 10;

    /// Every page a read walks from `from`, `need` rows a page, as the items pager commits them,
    /// and the stretch its last cursor would carry. A read resumed from a cursor carries `from`'s
    /// stretch; a test's stretches are far shorter than any a response opens, so a read of a few
    /// hundred rows crosses several.
    fn read(
        cx: &FakeCx<'_>,
        filter: Option<&FilterExpr>,
        keep_unmatched: bool,
        need: usize,
        (position, stretch): (Position, u32),
    ) -> (Vec<Page>, u32) {
        let mut walk = Walk::new(filter.cloned(), keep_unmatched, stretch, BYTES, position);
        walk.target = stretch;
        let mut clock = Clock::new(Instant::now(), Duration::from_secs(3600), None);
        let mut pages = Vec::new();
        loop {
            let collected = walk.collect(cx, need, &mut clock).unwrap();
            walk.position = collected.position;
            if !collected.rows.is_empty() {
                let rows = collected
                    .rows
                    .iter()
                    .map(|row| (row.mosaica_id, row.matched));
                pages.push((rows.collect(), walk.position, walk.target));
            }
            match collected.walked {
                Walked::Filled => {}
                Walked::End => break,
                Walked::Stopped(_) => unreachable!("the clock never stops a fixture read"),
            }
        }
        (pages, walk.target)
    }

    fn start(order: RecordsOrder, stretch: u32) -> (Position, u32) {
        (Position::start(order), stretch)
    }

    fn rows_of(pages: &[Page]) -> Vec<(u64, bool)> {
        pages
            .iter()
            .flat_map(|(rows, _, _)| rows.iter().copied())
            .collect()
    }

    /// Each row of `sides` the leaf admits and the candidate holds, side by side in row order, as
    /// its `mosaica_id` and whether it matched, kept where it matched or `keep_unmatched`.
    fn stored_rows(sides: &[FakeSide<'_>], keep_unmatched: bool) -> Vec<(u64, bool)> {
        let mut out = Vec::new();
        for side in sides {
            for &(segment, base) in &side.segments {
                for local in 0..segment.row_count {
                    let row = base + local;
                    let matched = side.matching.contains(row);
                    if side.leaf.contains_row(row)
                        && side.candidate.contains(row)
                        && (matched || keep_unmatched)
                    {
                        out.push((segment.columns.mosaica_id()[local as usize], matched));
                    }
                }
            }
        }
        out
    }

    /// The same rows in map order.
    fn map_rows(sides: &[FakeSide<'_>], keep_unmatched: bool) -> Vec<(u64, bool)> {
        let mut keyed: Vec<(Key, bool)> = Vec::new();
        for side in sides {
            for &(segment, base) in &side.segments {
                for local in 0..segment.row_count {
                    let row = base + local;
                    let matched = side.matching.contains(row);
                    if side.leaf.contains_row(row) && (matched || keep_unmatched) {
                        let at = local as usize;
                        let id = segment.columns.mosaica_id()[at];
                        keyed.push(((segment.morton.u32()[at], id), matched));
                    }
                }
            }
        }
        keyed.sort_unstable();
        keyed
            .into_iter()
            .map(|((_, id), matched)| (id, matched))
            .collect()
    }

    /// A third of some points split over two segments, and the rest with 40 points in the last
    /// cell in a third segment.
    struct Corpus {
        a: Written,
        b: Written,
        c: Written,
        ids: Vec<u64>,
    }

    impl Corpus {
        fn of(seed: u64, n: usize) -> Corpus {
            let mut rng = StdRng::seed_from_u64(seed);
            let mut all = points(&mut rng, n);
            let (first, second): (Vec<_>, Vec<_>) =
                all.iter().enumerate().partition(|(at, _)| at % 3 == 0);
            let [first, mut second] = [first, second].map(|held| -> Vec<(f32, f32, u64)> {
                held.into_iter().map(|(_, &point)| point).collect()
            });
            let corner: Vec<_> = (1..=40u64).map(|k| (1023.9, 1023.9, k << 40)).collect();
            second.extend(&corner);
            all.extend(&corner);
            let half = first.len() / 2;
            Corpus {
                a: segment_of(&first[..half]),
                b: segment_of(&first[half..]),
                c: segment_of(&second),
                ids: all.iter().map(|&(_, _, id)| id).collect(),
            }
        }

        /// The corpus over two shards, the first holding the first two segments, and over one
        /// shard holding all three.
        fn layouts(
            &self,
            visible: &FxHashSet<u64>,
            matching: &FxHashSet<u64>,
        ) -> [Vec<FakeSide<'_>>; 2] {
            [
                vec![
                    FakeSide::of(0, &[&self.a, &self.b], visible, matching),
                    FakeSide::of(1, &[&self.c], visible, matching),
                ],
                vec![FakeSide::of(
                    0,
                    &[&self.a, &self.b, &self.c],
                    visible,
                    matching,
                )],
            ]
        }

        fn ids(&self, keep: impl Fn(u64) -> bool) -> FxHashSet<u64> {
            self.ids.iter().copied().filter(|&id| keep(id)).collect()
        }
    }

    /// Over two shards a read in either order returns what one shard holding every row returns,
    /// page for page, with no filter, a filter in each shard and unmatched rows kept. Map order
    /// merges by `(cell, mosaica_id)`, so its positions are the same values; stored order takes
    /// the shards in turn. A read resumed from any page's position reads the rest, including past
    /// the first shard's last row.
    #[test]
    fn a_read_over_two_shards_is_one_shards_holding_every_row() {
        let corpus = Corpus::of(11, 900);
        let visible = corpus.ids(|id| id % 4 != 0);
        let matching = corpus.ids(|id| id % 5 != 0);
        let filter = unique_eq();
        let [two, one] = corpus.layouts(&visible, &matching);
        let (two, one) = (FakeCx::of(two, false), FakeCx::of(one, false));
        for order in [RecordsOrder::Map, RecordsOrder::Stored] {
            for (filter, keep) in [(None, false), (Some(&filter), false), (Some(&filter), true)] {
                let keep_all = keep || filter.is_none();
                let mut expected = match order {
                    RecordsOrder::Map => map_rows(&two.sides, keep_all),
                    RecordsOrder::Stored => stored_rows(&two.sides, keep_all),
                };
                if filter.is_none() {
                    expected.iter_mut().for_each(|(_, matched)| *matched = true);
                }
                for need in [1usize, 7, 64, 5000] {
                    for stretch in [16u32, 300, 1 << 20] {
                        let (pages, _) = read(&two, filter, keep, need, start(order, stretch));
                        let (other, _) = read(&one, filter, keep, need, start(order, stretch));
                        assert_eq!(rows_of(&pages), expected);
                        assert_eq!(rows_of(&other), expected);
                        let (last, filled) = pages.split_last().unwrap();
                        assert!(filled.iter().all(|(rows, _, _)| rows.len() == need));
                        assert!(last.0.len() <= need);
                        if order == RecordsOrder::Map {
                            let at = |pages: &[Page]| -> Vec<Position> {
                                pages.iter().map(|&(_, position, _)| position).collect()
                            };
                            assert_eq!(at(&pages), at(&other));
                        }
                        let mut past = 0;
                        for (at, (rows, position, carried)) in filled.iter().enumerate() {
                            past += rows.len();
                            if at % (filled.len() / 4 + 1) != 0 && at + 1 != filled.len() {
                                continue;
                            }
                            let from = (*position, *carried);
                            let (rest, _) = read(&two, filter, keep, need, from);
                            assert_eq!(rows_of(&rest), expected[past..]);
                            if order == RecordsOrder::Map {
                                let (rest, _) = read(&one, filter, keep, need, from);
                                assert_eq!(rows_of(&rest), expected[past..]);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Shards 0 and 3, both numbering items from 0: a stored read takes shard 0's, then shard
    /// 3's. A stretch ends where its shard does, and a read resumed from the page ending at shard
    /// 0's last item, or from the scan position just before shard 3, reads shard 3 from its first.
    #[test]
    fn a_stored_read_walks_shard_after_shard_and_resumes_at_the_boundary() {
        let mut rng = StdRng::seed_from_u64(5);
        let written: Vec<Written> = [10usize, 14]
            .iter()
            .map(|&n| segment_of(&points(&mut rng, n)))
            .collect();
        let none = FxHashSet::default();
        let mut sides: Vec<FakeSide<'_>> = written
            .iter()
            .zip([0u32, 3])
            .map(|(w, shard)| FakeSide::of(shard, &[w], &none, &none))
            .collect();
        sides[0].candidate = Bitmap::of(&[0, 1, 3, 4, 5, 6, 7, 8, 9]);
        sides[1].candidate.remove(5);
        sides[0].leaf = EffectiveMask::of_rows(Bitmap::from_range(0..10).andnot(&Bitmap::of(&[7])));
        sides[1].leaf = EffectiveMask::of_rows(Bitmap::from_range(1..14));
        let expected: Vec<(u64, bool)> = stored_rows(&sides, true)
            .into_iter()
            .map(|(id, _)| (id, true))
            .collect();
        let cx = FakeCx::of(sides, false);

        let listed: Vec<(usize, ShardId)> = (0..2).map(|place| (place, cx.shard(place))).collect();
        let whole = |scan| stored_stretch(&listed, scan, 1 << 20, |place| cx.candidate(place));
        let (place, range, until) = whole(None).unwrap().unwrap();
        assert_eq!((place, until), (0, Some((3, 0))));
        assert_eq!(range, cx.sides[0].candidate);
        let (place, _, until) = whole(Some(before((3, 0)).unwrap())).unwrap().unwrap();
        assert_eq!((place, until), (1, None));
        assert!(whole(Some((3, 13))).unwrap().is_none());

        for need in 1..=24 {
            for stretch in [1u32, 3, 4096] {
                let (pages, _) = read(&cx, None, false, need, start(RecordsOrder::Stored, stretch));
                assert_eq!(rows_of(&pages), expected);
                let mut past = 0;
                for (rows, position, carried) in &pages {
                    past += rows.len();
                    let (rest, _) = read(&cx, None, false, need, (*position, *carried));
                    assert_eq!(rows_of(&rest), expected[past..]);
                }
            }
        }

        // A page ending at shard 0's last visible item; its cursor resumes at shard 3's first.
        let in_first = 8;
        let (pages, _) = read(&cx, None, false, in_first, start(RecordsOrder::Stored, 2));
        let (_, position, carried) = pages[0];
        assert_eq!(position.scan, Some((0, 9)));
        let sealed = super::super::cursor::ItemsCursor {
            position,
            stretch: carried,
        };
        let resumed = super::super::cursor::ItemsCursor::decode(&sealed.encode()).unwrap();
        let (rest, _) = read(&cx, None, false, in_first, (resumed.position, carried));
        assert_eq!(rows_of(&rest), expected[in_first..]);
        let boundary = Position {
            order: RecordsOrder::Stored,
            scan: before((3, 0)),
        };
        let (rest, _) = read(&cx, None, false, in_first, (boundary, 2));
        assert_eq!(rows_of(&rest), expected[in_first..]);
    }

    /// A stored stretch cut short by its shard's end has not found the filter sparse, so a read
    /// across many small shards carries the stretch it began with. One that spans its target and
    /// fills no page grows the next.
    #[test]
    fn a_stored_stretch_cut_at_its_shards_end_does_not_grow_the_next() {
        let mut rng = StdRng::seed_from_u64(3);
        let written: Vec<Written> = (0..6).map(|_| segment_of(&points(&mut rng, 10))).collect();
        let every: FxHashSet<u64> = written.iter().flat_map(|w| w.ids.iter().copied()).collect();
        let sides: Vec<FakeSide<'_>> = (0u32..)
            .zip(&written)
            .map(|(shard, w)| FakeSide::of(shard, &[w], &every, &every))
            .collect();
        let cx = FakeCx::of(sides, false);
        let (pages, carried) = read(&cx, None, false, 1000, start(RecordsOrder::Stored, 32));
        assert_eq!(rows_of(&pages).len(), 60);
        assert_eq!(carried, 32);

        let one = segment_of(&points(&mut rng, 100));
        let sides = vec![FakeSide::of(0, &[&one], &every, &FxHashSet::default())];
        let cx = FakeCx::of(sides, false);
        let filter = unique_eq();
        let (pages, carried) = read(
            &cx,
            Some(&filter),
            false,
            10,
            start(RecordsOrder::Stored, 16),
        );
        assert!(pages.is_empty());
        assert_eq!(carried, 16 * 4 * 4);
    }

    /// Two shards whose bounds each fit one driven stretch but together do not walk the view, as
    /// one shard holding both does; a driven read returns what the walk returns, resumed from
    /// every page.
    #[test]
    fn a_driven_read_over_two_shards_is_bounded_and_counted_over_both() {
        let corpus = Corpus::of(17, 600);
        let visible = corpus.ids(|id| id % 4 != 0);
        // A seventh of the rows, and the last cell's, which only the second shard holds.
        let few = corpus.ids(|id| id % 7 == 0 || (id.trailing_zeros() >= 40 && id >> 40 <= 40));
        let filter = unique_eq();
        for order in [RecordsOrder::Map, RecordsOrder::Stored] {
            let [two, one] = corpus.layouts(&visible, &few);
            let expected = match order {
                RecordsOrder::Map => map_rows(&two, false),
                RecordsOrder::Stored => stored_rows(&two, false),
            };
            let matched = expected.len() as u64;
            let (two, one) = (FakeCx::of(two, true), FakeCx::of(one, true));
            for cx in [&two, &one] {
                let mut walk = Walk::new(Some(filter.clone()), false, 16, BYTES, start(order, 0).0);
                assert_eq!(walk.count(cx, &None).unwrap(), (matched, None));
                assert!(cx.driven.get() > 0);
                for need in [1usize, 7, 1000] {
                    let (pages, _) = read(cx, Some(&filter), false, need, start(order, 16));
                    assert_eq!(rows_of(&pages), expected);
                    let mut past = 0;
                    for (rows, position, carried) in &pages {
                        past += rows.len();
                        let (rest, _) = read(cx, Some(&filter), false, need, (*position, *carried));
                        assert_eq!(rows_of(&rest), expected[past..]);
                    }
                }
            }
        }

        // Each shard's bound fits a driven stretch, their sum does not: both walk the view.
        let corpus = Corpus::of(19, 9000);
        let map = Position::start(RecordsOrder::Map);
        let most = Walk::new(None, false, 0, SMALL, map).driven_most;
        let many = corpus.ids(|id| id % 2 == 0);
        let [two, one] = corpus.layouts(&corpus.ids(|_| true), &many);
        assert!(two
            .iter()
            .all(|side| side.matching.cardinality() <= u64::from(most)));
        assert!(one[0].matching.cardinality() > u64::from(most));
        for sides in [two, one] {
            let cx = FakeCx::of(sides, true);
            let expected: u64 = cx
                .sides
                .iter()
                .map(|side| side.matching.cardinality())
                .sum();
            let mut walk = Walk::new(Some(filter.clone()), false, 16, SMALL, map);
            assert_eq!(walk.count(&cx, &None).unwrap(), (expected, None));
            assert_eq!(cx.driven.get(), 0);
        }
    }

    #[test]
    fn a_driven_count_leaves_out_a_shard_the_viewer_sees_nothing_in() {
        let corpus = Corpus::of(17, 600);
        let visible = corpus.ids(|id| id % 4 != 0);
        let few = corpus.ids(|id| id % 7 == 0);
        let [two, _] = corpus.layouts(&visible, &few);
        let mut cx = FakeCx::of(two, true);
        cx.reads = vec![1];
        let second = &cx.sides[1];
        let expected = second.leaf.count_intersection(&second.matching);
        assert!(expected > 0);
        for order in [RecordsOrder::Map, RecordsOrder::Stored] {
            let mut walk = Walk::new(Some(unique_eq()), false, 16, BYTES, start(order, 0).0);
            assert_eq!(walk.count(&cx, &None).unwrap(), (expected, None));
        }
        assert!(cx.driven.get() > 0, "the count was driven");
    }

    /// A resumed read carries the stretch it grew to under the ceiling of the response that
    /// issued its cursor; a response with a smaller page ceiling holds it to its own.
    #[test]
    fn a_carried_stretch_is_held_to_the_resuming_responses_ceiling() {
        let start = Position::start(RecordsOrder::Map);
        let bytes = 8 << 20;
        let ceiling = u32::try_from(bytes / STRETCH_BYTES_PER_ROW).unwrap();
        let walk = Walk::new(None, false, 64 << 20, bytes, start);
        assert_eq!(walk.target, ceiling);
        let walk = Walk::new(None, false, 1, bytes, start);
        assert_eq!(walk.target, STRETCH_MIN);
        let walk = Walk::new(None, false, 64 << 20, 1024, start);
        assert_eq!(walk.target, STRETCH_MIN);
    }
}
