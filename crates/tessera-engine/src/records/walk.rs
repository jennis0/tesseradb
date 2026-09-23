//! The walk a page takes its rows by, and the filter evaluated over the stretch ahead of it.
//!
//! A stretch is the part of the view a walk has evaluated the filter over: a range of map cells
//! in map order, a range of item numbers in stored order. It starts at the scan position and
//! spans a target number of rows, and when a page empties one before filling, the next is four
//! times longer, so a sparse filter costs a few evaluations per read rather than one per page. The
//! size reached travels in the cursor, so a resumed read continues at it.
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

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use croaring::Bitmap;
use tessera_store::read::SegmentData;
use tessera_types::{EntityId, TesseraId};

use super::cursor::{Key, Position};
use super::{RecordsOrder, ResponseEndedBy};
use crate::cancel::CancelToken;
use crate::compose::{for_each_run_in, FilterRows};
use crate::engine::Engine;
use crate::error::Result;
use crate::filter::FilterExpr;
use crate::histogram::MaskIdentity;
use crate::region::RegionVerdict;
use crate::viewport::{crossing_domain, segment_holding, OpenView, RoutedRows};
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

/// What one page is walked in: the engine, the view resolved and its mask composed for the page,
/// and the generation that resolution came from.
pub(super) struct PageCx<'a> {
    pub(super) engine: &'a Engine,
    pub(super) open: &'a OpenView<'a>,
    pub(super) generation: &'a Arc<Generation>,
}

impl PageCx<'_> {
    fn segments(&self) -> &[(&SegmentData, u32)] {
        &self.open.served.segments
    }

    fn entity_of(&self, tessera_id: u64) -> u32 {
        let (_, entity) = self.engine.identity_key.invert(TesseraId::new(tessera_id));
        u32::try_from(entity.raw()).expect("inversion yields a 32-bit entity")
    }
}

/// One row a page takes, located in the generation the page was walked in.
pub(super) struct Taken {
    /// Index into the view's segments, ascending by row base.
    pub(super) seg: usize,
    pub(super) local: u32,
    pub(super) tessera_id: u64,
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
pub(super) struct Clock {
    started: Instant,
    budget: Duration,
    pub(super) cancel: Option<CancelToken>,
}

impl Clock {
    pub(super) fn new(started: Instant, budget: Duration, cancel: Option<CancelToken>) -> Clock {
        Clock {
            started,
            budget,
            cancel,
        }
    }

    pub(super) fn cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(CancelToken::is_cancelled)
    }

    pub(super) fn out_of_time(&self) -> bool {
        self.started.elapsed() >= self.budget
    }

    /// Why the scan would stop here.
    fn stop(&self) -> Option<ResponseEndedBy> {
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
    /// The composed mask the filter was evaluated under, whose region and `member_of` leaves
    /// answer through it.
    mask: MaskIdentity,
    /// The scan position it was opened after.
    from: Option<Key>,
    /// The first key past the stretch; `None` where the stretch runs to the end.
    until: Option<Key>,
    /// The rows the filter admits, over the stretch's rows, where there is a filter.
    filter: Option<FilterRows>,
    /// In stored order, the candidate items of the stretch that hold a row in the view,
    /// ascending, with that row. Empty in map order, whose rows are the segments' own.
    items: Vec<(u32, u32)>,
}

/// Whether two generations share the row positions and the deny state a stretch was evaluated
/// under.
fn same_publication(a: &Generation, b: &Generation) -> bool {
    a.prefix == b.prefix
        && a.segments_version == b.segments_version
        && Arc::ptr_eq(&a.overlay, &b.overlay)
}

/// A taken row's key in `order`.
fn key_of(order: RecordsOrder, segments: &[(&SegmentData, u32)], row: &Taken) -> Key {
    match order {
        RecordsOrder::Map => {
            let (segment, _) = segments[row.seg];
            (segment.morton.u32()[row.local as usize], row.tessera_id)
        }
        RecordsOrder::Stored => (row.entity, 0),
    }
}

/// The key just before `key`, or `None` where nothing is.
fn before(key: Key) -> Option<Key> {
    match key {
        (cell, 0) => cell.checked_sub(1).map(|cell| (cell, u64::MAX)),
        (cell, id) => Some((cell, id - 1)),
    }
}

/// The first local row of `segment` past `after` in `(cell, tessera_id)` order, the order every
/// segment's rows are stored in.
fn first_after(segment: &SegmentData, after: Option<Key>) -> u32 {
    let Some(after) = after else {
        return 0;
    };
    let cells = segment.morton.u32();
    let ids = segment.columns.tessera_id();
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

/// A filter's rows over `domain`, routed under `candidate`, or under the session's whole
/// candidate where that is `None`: every leaf takes the row route where its column affords one.
pub(super) fn filter_rows(
    cx: &PageCx<'_>,
    expr: &FilterExpr,
    candidate: Option<&Bitmap>,
    domain: &[Range<u32>],
    per_tile_only: bool,
    cancel: &Option<CancelToken>,
) -> Result<RoutedRows> {
    let engine = cx.engine;
    let served = &cx.open.served;
    let rows_in_ranges: u64 = domain.iter().map(|r| r.len() as u64).sum();
    let body = |route: &dyn Fn(&FilterExpr, bool) -> Result<crate::filter::RoutedFilter>| {
        engine.rows_of_routed(served, route(expr, true)?, domain, rows_in_ranges, per_tile_only)
    };
    match candidate {
        Some(candidate) => {
            engine.route_filters_under(served, &cx.open.mask, candidate, cancel, body)
        }
        None => engine.route_filters(served, &cx.open.mask, cancel, body),
    }
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
        Walk {
            filter,
            keep_unmatched,
            target: stretch.clamp(STRETCH_MIN, ceiling),
            ceiling,
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
        cx: &PageCx<'_>,
        need: usize,
        clock: &mut Clock,
    ) -> Result<Collected> {
        let Position { order, mut scan } = self.position;
        let mut rows: Vec<Taken> = Vec::new();
        let walked = loop {
            if self.stretch.as_ref().is_some_and(|stretch| {
                !same_publication(&stretch.under, cx.generation)
                    || stretch.mask != cx.open.served.mask_identity
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
                let opened = match order {
                    RecordsOrder::Map => self.open_map_stretch(cx, scan, &clock.cancel)?,
                    RecordsOrder::Stored => self.open_stored_stretch(cx, scan, &clock.cancel)?,
                };
                match opened {
                    Some(stretch) => self.stretch = Some(stretch),
                    None => break Walked::End,
                }
            }
            let take = Take {
                cx,
                stretch: self.stretch.as_ref().expect("a stretch is open"),
                keep_unmatched: self.keep_unmatched,
                moved: scan > self.origin,
            };
            let stopped = match order {
                RecordsOrder::Map => take.map(scan, need, clock, &mut rows),
                RecordsOrder::Stored => take.stored(scan, need, clock, &mut rows),
            };
            let stretch = take.stretch;
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
                scan = Some(key_of(order, cx.segments(), row));
                break Walked::Filled;
            }
            let until = stretch.until;
            self.stretch = None;
            self.target = self.target.saturating_mul(STRETCH_GROWTH).min(self.ceiling);
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
    pub(super) fn position_at(&self, cx: &PageCx<'_>, row: &Taken) -> Position {
        let key = key_of(self.position.order, cx.segments(), row);
        Position {
            order: self.position.order,
            scan: Some(key),
        }
    }

    /// The map stretch past `scan`: every row of every segment whose key is below the nearest key
    /// a segment reaches its share of `target` rows ahead. The segment reaching it holds a row
    /// before it, so a stretch always covers a row. `None` where no segment holds a row past
    /// `scan`.
    fn open_map_stretch(
        &mut self,
        cx: &PageCx<'_>,
        scan: Option<Key>,
        cancel: &Option<CancelToken>,
    ) -> Result<Option<Stretch>> {
        let segments = cx.segments();
        let starts: Vec<u32> = segments
            .iter()
            .map(|&(segment, _)| first_after(segment, scan))
            .collect();
        let ahead = segments
            .iter()
            .zip(&starts)
            .filter(|&(&(segment, _), &start)| start < segment.row_count)
            .count();
        let share = (self.target as usize / ahead.max(1)).max(1);
        if ahead == 0 {
            return Ok(None);
        }
        let until: Option<Key> = segments
            .iter()
            .zip(&starts)
            .filter(|&(&(segment, _), &start)| start < segment.row_count)
            .filter_map(|(&(segment, _), &start)| {
                let reach = start as usize + share;
                (reach < segment.row_count as usize).then(|| {
                    (
                        segment.morton.u32()[reach],
                        segment.columns.tessera_id()[reach],
                    )
                })
            })
            .min();
        let filter = match &self.filter {
            None => None,
            Some(expr) => {
                let parts: Vec<(usize, Range<u32>)> = segments
                    .iter()
                    .zip(&starts)
                    .enumerate()
                    .filter_map(|(s, (&(segment, _), &start))| {
                        let end = first_at(segment, until);
                        (start < end).then_some((s, start..end))
                    })
                    .collect();
                // The items of the stretch's rows this page's mask admits, so an entity-space
                // scan costs the stretch and not the view. The stretch is held only while the
                // mask is the one it was opened under.
                let mut entities: Vec<u32> = Vec::new();
                for (s, range) in &parts {
                    let (segment, base) = segments[*s];
                    let ids = segment.columns.tessera_id();
                    let visible = cx.open.mask.rows_in_range(base + range.start..base + range.end);
                    entities.extend(
                        visible
                            .iter()
                            .map(|row| cx.entity_of(ids[(row - base) as usize])),
                    );
                }
                entities.sort_unstable();
                let mut candidate = Bitmap::of(&entities);
                candidate.and_inplace(
                    &cx.engine
                        .filter_candidate(cx.open.served.session, cx.generation)?,
                );
                let row_bases: Vec<u32> = segments.iter().map(|&(_, base)| base).collect();
                let domain = crossing_domain(&[parts], &row_bases);
                let routed = filter_rows(cx, expr, Some(&candidate), &domain, true, cancel)?;
                self.region = RegionVerdict::coarsest(self.region, routed.region);
                Some(routed.rows)
            }
        };
        Ok(Some(Stretch {
            under: Arc::clone(cx.generation),
            mask: cx.open.served.mask_identity,
            from: scan,
            until,
            filter,
            items: Vec::new(),
        }))
    }

    /// The stored stretch past `scan`: the next `target` items of the viewer's candidate set, and
    /// of them the ones that hold a row in the view. `None` where the candidate holds nothing past
    /// `scan`.
    fn open_stored_stretch(
        &mut self,
        cx: &PageCx<'_>,
        scan: Option<Key>,
        cancel: &Option<CancelToken>,
    ) -> Result<Option<Stretch>> {
        let candidate = cx
            .engine
            .filter_candidate(cx.open.served.session, cx.generation)?;
        let from: u64 = scan.map_or(0, |(entity, _)| u64::from(entity) + 1);
        if from > u64::from(u32::MAX) {
            return Ok(None);
        }
        let before = if from == 0 {
            0
        } else {
            candidate.rank((from - 1) as u32)
        };
        if before >= candidate.cardinality() {
            return Ok(None);
        }
        let until: Option<u32> = u32::try_from(before + u64::from(self.target))
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
        let row_space = &cx.open.served.data.row_space;
        let mut items: Vec<(u32, u32)> = Vec::with_capacity(range.cardinality() as usize);
        items.extend(range.iter().filter_map(|entity| {
            row_space
                .row_of(EntityId::new(u64::from(entity)))
                .map(|row| (entity, row.raw()))
        }));
        let filter = match &self.filter {
            None => None,
            Some(expr) => {
                // The stretch's own rows, which the view need not hold contiguously.
                let rows = Bitmap::of(&items.iter().map(|&(_, row)| row).collect::<Vec<_>>());
                let mut domain: Vec<Range<u32>> = Vec::new();
                let total = u32::try_from(row_space.total_rows()).unwrap_or(u32::MAX);
                for_each_run_in(&rows, 0..total, &mut |run| domain.push(run));
                let routed = filter_rows(cx, expr, Some(&range), &domain, true, cancel)?;
                self.region = RegionVerdict::coarsest(self.region, routed.region);
                Some(routed.rows)
            }
        };
        Ok(Some(Stretch {
            under: Arc::clone(cx.generation),
            mask: cx.open.served.mask_identity,
            from: scan,
            until: until.map(|entity| (entity, 0)),
            filter,
            items,
        }))
    }
}

/// One segment's rows in a map stretch, gathered through the page's mask a chunk at a time: the
/// first chunk is as long as the page still needs, and at least [`CHUNK_MIN`] rows, and each after
/// it twice the one before.
struct SegmentRun<'a> {
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
    /// The next chunk of this segment's part of the stretch, through the page's mask.
    fn gather(&mut self, cx: &PageCx<'_>, stretch: &Stretch, keep_unmatched: bool) {
        let hi = self.end.min(self.next.saturating_add(self.chunk));
        let visible = cx
            .open
            .mask
            .rows_in_range(self.base + self.next..self.base + hi);
        self.buffered.clear();
        self.at = 0;
        match &stretch.filter {
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
        self.next = hi;
        self.chunk = self.chunk.saturating_mul(2);
    }

    fn key(&self, local: u32) -> Key {
        let local = local as usize;
        (
            self.segment.morton.u32()[local],
            self.segment.columns.tessera_id()[local],
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
/// The fewest rows a map segment's first chunk spans, whatever the page still needs, so a
/// response the clock stops still moves the scan by that much.
const CHUNK_MIN: u32 = 1024;

/// One stretch's rows being taken into a page.
struct Take<'a> {
    cx: &'a PageCx<'a>,
    stretch: &'a Stretch,
    keep_unmatched: bool,
    /// Whether the scan has already passed the position the response started from.
    moved: bool,
}

impl Take<'_> {
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
    /// segment's rows that the page's mask admits, and the filter where rows must match, merged
    /// across segments by `(cell, tessera_id)`. The least key the scan has not reached is taken
    /// where it is a gathered row, and gathered where it is not, so the clock, read before each
    /// chunk, can stop the scan after one. `Some` where it did, with the first key the scan did
    /// not reach.
    fn map(
        &self,
        scan: Option<Key>,
        need: usize,
        clock: &Clock,
        rows: &mut Vec<Taken>,
    ) -> Option<(ResponseEndedBy, Key)> {
        let cx = self.cx;
        let mut runs: Vec<SegmentRun<'_>> = cx
            .segments()
            .iter()
            .enumerate()
            .map(|(seg, &(segment, base))| SegmentRun {
                seg,
                segment,
                base,
                next: first_after(segment, scan),
                end: first_at(segment, self.stretch.until),
                chunk: u32::try_from(need - rows.len())
                    .unwrap_or(u32::MAX)
                    .max(CHUNK_MIN),
                buffered: Vec::new(),
                at: 0,
            })
            .collect();
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
                let tessera_id = run.segment.columns.tessera_id()[local as usize];
                rows.push(Taken {
                    seg: run.seg,
                    local,
                    tessera_id,
                    entity: cx.entity_of(tessera_id),
                    matched,
                });
                run.at += 1;
            } else {
                if let Some(stop) = self.stop(clock, rows, key, began_at) {
                    return Some(stop);
                }
                run.gather(cx, self.stretch, self.keep_unmatched);
            }
            if let Some(key) = runs[i].first_unreached() {
                unreached.push(Reverse((key, i)));
            }
        }
        None
    }

    /// Take stored-order rows past `scan` from the stretch's items until `rows` holds `need`:
    /// each item whose row the page's mask admits, and that matches where rows must match.
    /// `Some` where the clock stopped the scan, with the first key it did not reach.
    fn stored(
        &self,
        scan: Option<Key>,
        need: usize,
        clock: &Clock,
        rows: &mut Vec<Taken>,
    ) -> Option<(ResponseEndedBy, Key)> {
        let stretch = self.stretch;
        let start = match scan {
            None => 0,
            Some(scan) => stretch
                .items
                .partition_point(|&(entity, _)| (entity, 0) <= scan),
        };
        let segments = self.cx.segments();
        let &(began_at, _) = stretch.items.get(start)?;
        for (i, &(entity, row)) in stretch.items[start..].iter().enumerate() {
            if rows.len() == need {
                return None;
            }
            if i % STORED_CHUNK == 0 {
                if let Some(stop) = self.stop(clock, rows, (entity, 0), (began_at, 0)) {
                    return Some(stop);
                }
            }
            if !self.cx.open.mask.contains_row(row) {
                continue;
            }
            let matched = stretch
                .filter
                .as_ref()
                .is_none_or(|filter| filter.rows().contains(row));
            if !matched && !self.keep_unmatched {
                continue;
            }
            let (seg, local) =
                segment_holding(segments, row).expect("the first segment's rows begin at 0");
            rows.push(Taken {
                seg,
                local,
                tessera_id: segments[seg].0.columns.tessera_id()[local as usize],
                entity,
                matched,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
