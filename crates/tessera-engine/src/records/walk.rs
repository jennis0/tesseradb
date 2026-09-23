//! The walk a page takes its rows by, and the filter evaluated over the stretch ahead of it.
//!
//! A stretch is the part of the view a walk has evaluated the filter over: a range of map cells
//! in map order, a range of item numbers in stored order. It starts at the scan position and
//! spans a target number of rows, and when a page empties one before filling, the next is four
//! times longer, so a sparse filter costs a few evaluations per response rather than one per
//! page. The filter's answer is held in the row positions of the publication it was evaluated
//! under, so a stretch is evaluated again when the prefix, the segment set or the overlay moves.
//! Visibility is never held: every page tests every row against the mask composed for that page.
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
use crate::region::RegionVerdict;
use crate::viewport::{crossing_domain, segment_holding, OpenView, RoutedRows};
use crate::Generation;

/// The first stretch spans at least this many rows, or candidate items in stored order, whatever
/// the page size.
const STRETCH_MIN: u32 = 4096;
const STRETCH_GROWTH: u32 = 4;
/// The most a stretch holds per row it spans: a stored stretch keeps each candidate item with its
/// row, two `u32`s, and a map stretch one `u32` item per visible row while it is evaluated. The
/// bitmaps beside them hold at most as much again.
const STRETCH_BYTES_PER_ROW: usize = 8;

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
    /// The response must stop. A walk stops for time only while it holds no row, and for
    /// cancellation whatever it holds.
    Stopped(ResponseEndedBy),
}

pub(super) struct Collected {
    pub(super) rows: Vec<Taken>,
    pub(super) walked: Walked,
    /// The position after every row taken: the last of them, and how far the scan went.
    pub(super) position: Position,
}

/// A response's time budget and cancellation.
pub(super) struct Clock {
    started: Instant,
    budget: Duration,
    pub(super) cancel: Option<CancelToken>,
    /// Stretches this response has scanned to their end. A response stops for time only after
    /// one, so every response moves the scan position on.
    scanned: u32,
}

impl Clock {
    pub(super) fn new(started: Instant, budget: Duration, cancel: Option<CancelToken>) -> Clock {
        Clock {
            started,
            budget,
            cancel,
            scanned: 0,
        }
    }

    pub(super) fn cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(CancelToken::is_cancelled)
    }

    pub(super) fn out_of_time(&self) -> bool {
        self.started.elapsed() >= self.budget
    }

    fn stop_scan(&self, holding_rows: bool) -> Option<ResponseEndedBy> {
        if self.cancelled() {
            return Some(ResponseEndedBy::Deadline);
        }
        (!holding_rows && self.scanned > 0 && self.out_of_time())
            .then_some(ResponseEndedBy::BudgetTime)
    }
}

/// A walk through one response: the position reached and the stretch ahead of it.
pub(super) struct Walk {
    filter: Option<FilterExpr>,
    keep_unmatched: bool,
    /// The rows the next stretch spans, across every segment.
    target: u32,
    /// The rows a stretch may span, from the byte ceiling a stretch is held to.
    ceiling: u32,
    stretch: Option<Stretch>,
    pub(super) position: Position,
    /// The first region verdict an evaluation reached, for the response's header.
    pub(super) region: Option<RegionVerdict>,
}

/// The part of the view ahead of a scan position that one filter evaluation covers.
struct Stretch {
    under: Arc<Generation>,
    /// The scan position it was opened after.
    from: Option<Key>,
    /// Every row whose key's first half is below this; `2^32` past the last.
    until: u64,
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

/// The first local row of `segment` whose cell is `until` or past it.
fn first_at_cell(segment: &SegmentData, until: u64) -> u32 {
    tessera_store::read::first_code_at_or_past(segment, until, 0..segment.row_count)
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
    /// A walk from `position` whose stretches hold no more than `stretch_bytes`.
    pub(super) fn new(
        filter: Option<FilterExpr>,
        keep_unmatched: bool,
        page_rows: u32,
        stretch_bytes: usize,
        position: Position,
    ) -> Walk {
        let ceiling = u32::try_from(stretch_bytes / STRETCH_BYTES_PER_ROW)
            .unwrap_or(u32::MAX)
            .max(STRETCH_MIN);
        Walk {
            filter,
            keep_unmatched,
            target: page_rows.clamp(STRETCH_MIN, ceiling),
            ceiling,
            stretch: None,
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
        let Position {
            order,
            last,
            mut scan,
        } = self.position;
        let mut rows: Vec<Taken> = Vec::new();
        let walked = loop {
            if self.stretch.as_ref().is_some_and(|stretch| {
                !same_publication(&stretch.under, cx.generation) || stretch.from > scan
            }) {
                self.stretch = None;
            }
            if self.stretch.is_none() {
                if let Some(reason) = clock.stop_scan(!rows.is_empty()) {
                    break Walked::Stopped(reason);
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
            let stretch = self.stretch.as_ref().expect("a stretch is open");
            match order {
                RecordsOrder::Map => take_map(cx, stretch, self.keep_unmatched, scan, need, &mut rows),
                RecordsOrder::Stored => {
                    take_stored(cx, stretch, self.keep_unmatched, scan, need, &mut rows)
                }
            }
            if rows.len() == need {
                let row = rows.last().expect("a filled page holds a row");
                scan = Some(key_of(order, cx.segments(), row));
                break Walked::Filled;
            }
            clock.scanned += 1;
            let until = stretch.until;
            self.stretch = None;
            self.target = self.target.saturating_mul(STRETCH_GROWTH).min(self.ceiling);
            if until > u64::from(u32::MAX) {
                scan = Some((u32::MAX, u64::MAX));
                break Walked::End;
            }
            scan = Some(((until - 1) as u32, u64::MAX));
        };
        let last = rows
            .last()
            .map(|row| key_of(order, cx.segments(), row))
            .or(last);
        Ok(Collected {
            rows,
            walked,
            position: Position { order, last, scan },
        })
    }

    /// The position just after one taken row, for a page cut before the rows that followed it.
    pub(super) fn position_at(&self, cx: &PageCx<'_>, row: &Taken) -> Position {
        let key = key_of(self.position.order, cx.segments(), row);
        Position {
            order: self.position.order,
            last: Some(key),
            scan: Some(key),
        }
    }

    /// The map stretch past `scan`: every row of every segment whose cell is below the nearest
    /// cell a segment reaches its share of `target` rows ahead, and at least the nearest cell any
    /// segment holds, so a stretch always covers a row. `None` where no segment holds a row past
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
        let mut until: u64 = 1 << 32;
        let mut nearest: Option<u32> = None;
        for (&(segment, _), &start) in segments.iter().zip(&starts) {
            let cells = segment.morton.u32();
            let Some(&cell) = cells.get(start as usize) else {
                continue;
            };
            nearest = Some(nearest.map_or(cell, |n| n.min(cell)));
            if let Some(&reach) = cells.get(start as usize + share) {
                until = until.min(u64::from(reach));
            }
        }
        let Some(nearest) = nearest else {
            return Ok(None);
        };
        let until = until.max(u64::from(nearest) + 1);
        let filter = match &self.filter {
            None => None,
            Some(expr) => {
                let parts: Vec<(usize, Range<u32>)> = segments
                    .iter()
                    .zip(&starts)
                    .enumerate()
                    .filter_map(|(s, (&(segment, _), &start))| {
                        let end = first_at_cell(segment, until);
                        (start < end).then_some((s, start..end))
                    })
                    .collect();
                // The candidate is the stretch's visible items, so an entity-space scan costs the
                // stretch and not the view.
                let mut entities: Vec<u32> = Vec::new();
                for (s, range) in &parts {
                    let (segment, base) = segments[*s];
                    let ids = segment.columns.tessera_id();
                    let visible = cx.open.mask.rows_in_range(base + range.start..base + range.end);
                    entities.extend(visible.iter().map(|row| cx.entity_of(ids[(row - base) as usize])));
                }
                entities.sort_unstable();
                let row_bases: Vec<u32> = segments.iter().map(|&(_, base)| base).collect();
                let domain = crossing_domain(&[parts], &row_bases);
                let routed =
                    filter_rows(cx, expr, Some(&Bitmap::of(&entities)), &domain, true, cancel)?;
                self.region = self.region.or(routed.region);
                Some(routed.rows)
            }
        };
        Ok(Some(Stretch {
            under: Arc::clone(cx.generation),
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
        let until: u64 = u32::try_from(before + u64::from(self.target))
            .ok()
            .and_then(|rank| candidate.select(rank))
            .map_or(1 << 32, u64::from);
        let mut range = Bitmap::new();
        range.add_range(from as u32..=(until - 1) as u32);
        range.and_inplace(&candidate);
        let row_space = &cx.open.served.data.row_space;
        let items: Vec<(u32, u32)> = range
            .iter()
            .filter_map(|entity| {
                row_space
                    .row_of(EntityId::new(u64::from(entity)))
                    .map(|row| (entity, row.raw()))
            })
            .collect();
        let filter = match &self.filter {
            None => None,
            Some(expr) => {
                // The stretch's own rows, which the view need not hold contiguously.
                let rows = Bitmap::of(&items.iter().map(|&(_, row)| row).collect::<Vec<_>>());
                let mut domain: Vec<Range<u32>> = Vec::new();
                let total = u32::try_from(row_space.total_rows()).unwrap_or(u32::MAX);
                for_each_run_in(&rows, 0..total, &mut |run| domain.push(run));
                let routed = filter_rows(cx, expr, Some(&range), &domain, true, cancel)?;
                self.region = self.region.or(routed.region);
                Some(routed.rows)
            }
        };
        Ok(Some(Stretch {
            under: Arc::clone(cx.generation),
            from: scan,
            until,
            filter,
            items,
        }))
    }
}

/// Take map-order rows past `scan` and in the stretch until `rows` holds `need`: each segment's
/// rows that the page's mask admits, and the filter where rows must match, merged across segments
/// by `(cell, tessera_id)`.
fn take_map(
    cx: &PageCx<'_>,
    stretch: &Stretch,
    keep_unmatched: bool,
    scan: Option<Key>,
    need: usize,
    rows: &mut Vec<Taken>,
) {
    let mut runs: Vec<SegmentRun<'_>> = cx
        .segments()
        .iter()
        .enumerate()
        .map(|(seg, &(segment, base))| SegmentRun {
            seg,
            segment,
            base,
            next: first_after(segment, scan),
            end: first_at_cell(segment, stretch.until),
            chunk: u32::try_from(need - rows.len()).unwrap_or(u32::MAX).max(1),
            buffered: Vec::new(),
            at: 0,
        })
        .collect();
    let mut heads: BinaryHeap<Reverse<(Key, usize)>> = BinaryHeap::with_capacity(runs.len());
    for (i, run) in runs.iter_mut().enumerate() {
        if let Some(key) = run.head(cx, stretch, keep_unmatched) {
            heads.push(Reverse((key, i)));
        }
    }
    while rows.len() < need {
        let Some(Reverse((_, i))) = heads.pop() else {
            return;
        };
        let run = &mut runs[i];
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
        if let Some(key) = run.head(cx, stretch, keep_unmatched) {
            heads.push(Reverse((key, i)));
        }
    }
}

/// One segment's rows in a map stretch, gathered through the page's mask a chunk at a time: the
/// first chunk is as long as the page still needs, and each after it twice the one before.
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
    /// The key of the next row this segment serves, gathering another chunk where the last is
    /// spent; `None` where the stretch holds no more.
    fn head(&mut self, cx: &PageCx<'_>, stretch: &Stretch, keep_unmatched: bool) -> Option<Key> {
        while self.at == self.buffered.len() {
            if self.next >= self.end {
                return None;
            }
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
        let local = self.buffered[self.at].0 as usize;
        Some((
            self.segment.morton.u32()[local],
            self.segment.columns.tessera_id()[local],
        ))
    }
}

/// Take stored-order rows past `scan` from a stretch's items until `rows` holds `need`: each item
/// whose row the page's mask admits, and that matches where rows must match.
fn take_stored(
    cx: &PageCx<'_>,
    stretch: &Stretch,
    keep_unmatched: bool,
    scan: Option<Key>,
    need: usize,
    rows: &mut Vec<Taken>,
) {
    let start = match scan {
        None => 0,
        Some(scan) => stretch
            .items
            .partition_point(|&(entity, _)| (entity, 0) <= scan),
    };
    let segments = cx.segments();
    for &(entity, row) in &stretch.items[start..] {
        if rows.len() == need {
            return;
        }
        if !cx.open.mask.contains_row(row) {
            continue;
        }
        let matched = stretch
            .filter
            .as_ref()
            .is_none_or(|filter| filter.rows().contains(row));
        if !matched && !keep_unmatched {
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
}
