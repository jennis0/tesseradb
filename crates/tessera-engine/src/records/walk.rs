//! The two walks a page takes its rows by, and the filter evaluated over the stretch ahead of them.
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

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use croaring::Bitmap;
use tessera_store::read::SegmentData;
use tessera_types::{EntityId, IdentityKey, TesseraId};

use super::cursor::{MapKey, Position};
use super::ResponseEndedBy;
use crate::cancel::CancelToken;
use crate::compose::{EffectiveMask, FilterRows};
use crate::engine::Engine;
use crate::error::Result;
use crate::filter::{FilterExpr, RoutedFilter};
use crate::region::RegionVerdict;
use crate::viewport::{crossing_domain, OpenView, ServedView};
use crate::Generation;

/// The first stretch spans at least this many rows per segment, or candidate items in stored
/// order, whatever the page size.
const STRETCH_MIN: u32 = 4096;
/// A stretch never spans more than this: the bitmaps a stretch holds are bounded by it.
const STRETCH_MAX: u32 = 1 << 24;
const STRETCH_GROWTH: u32 = 4;

/// One row a page takes, located in the generation the page was walked in.
pub(crate) struct Taken {
    /// Index into the view's segments, ascending by row base.
    pub(crate) seg: usize,
    pub(crate) local: u32,
    pub(crate) tessera_id: u64,
    pub(crate) entity: u32,
    pub(crate) matched: bool,
}

/// Why a walk stopped taking rows.
pub(crate) enum Walked {
    /// The page holds as many rows as it asked for.
    Filled,
    /// Nothing remains past the rows taken.
    End,
    /// The response must stop. A walk stops for time only while it holds no row, and for
    /// cancellation whatever it holds.
    Stopped(ResponseEndedBy),
}

pub(crate) struct Collected {
    pub(crate) rows: Vec<Taken>,
    pub(crate) walked: Walked,
    /// The position after every row taken: the last of them, and how far the scan went.
    pub(crate) position: Position,
}

/// A response's time budget and cancellation.
pub(crate) struct Clock {
    started: Instant,
    budget: Duration,
    cancel: Option<CancelToken>,
    /// Stretches this response has scanned to their end. A response stops for time only after
    /// one, so every response moves the scan position on.
    scanned: u32,
}

impl Clock {
    pub(crate) fn new(started: Instant, budget: Duration, cancel: Option<CancelToken>) -> Clock {
        Clock {
            started,
            budget,
            cancel,
            scanned: 0,
        }
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(CancelToken::is_cancelled)
    }

    pub(crate) fn out_of_time(&self) -> bool {
        self.started.elapsed() >= self.budget
    }

    /// The token a region's decomposition waits on.
    fn token(&self) -> &Option<CancelToken> {
        &self.cancel
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
pub(crate) struct Walk {
    filter: Option<FilterExpr>,
    keep_unmatched: bool,
    target: u32,
    stretch: Option<Stretch>,
    pub(crate) position: Position,
    /// The first region verdict an evaluation reached, for the response's header.
    pub(crate) region: Option<RegionVerdict>,
}

struct Stretch {
    under: Arc<Generation>,
    body: StretchBody,
}

enum StretchBody {
    Map {
        /// The scan position the stretch was opened after.
        from: Option<MapKey>,
        /// Every row whose cell is below this, `2^32` past the last cell.
        until: u64,
        filter: Option<FilterRows>,
    },
    Stored {
        from: Option<u32>,
        /// Every item numbered below this, `2^32` past the last.
        until: u64,
        /// The candidate items of the stretch that hold a row in the view, ascending, with it.
        items: Vec<(u32, u32)>,
        /// The items the filter admits, where there is a filter.
        matched: Option<Bitmap>,
    },
}

/// Whether two generations share the row positions and the deny state a stretch was evaluated
/// under.
fn same_publication(a: &Generation, b: &Generation) -> bool {
    a.prefix == b.prefix
        && a.segments_version == b.segments_version
        && Arc::ptr_eq(&a.overlay, &b.overlay)
}

fn entity_of(identity: &IdentityKey, tessera_id: u64) -> u32 {
    let (_, entity) = identity.invert(TesseraId::new(tessera_id));
    u32::try_from(entity.raw()).expect("inversion yields a 32-bit entity")
}

/// A taken row's map-order position.
fn map_key(segments: &[(&SegmentData, u32)], row: &Taken) -> MapKey {
    let (segment, _) = segments[row.seg];
    (segment.morton.u32()[row.local as usize], row.tessera_id)
}

/// The first local row of `segment` past `after` in `(cell, tessera_id)` order, the order every
/// segment's rows are stored in.
fn first_after(segment: &SegmentData, after: Option<MapKey>) -> u32 {
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

/// The first local row of `segment` whose cell is at or past `until`.
fn first_at_cell(segment: &SegmentData, until: u64) -> u32 {
    segment
        .morton
        .u32()
        .partition_point(|&cell| u64::from(cell) < until) as u32
}

/// Sorted rows as maximal ascending runs.
fn runs_of(rows: &[u32]) -> Vec<Range<u32>> {
    let mut runs: Vec<Range<u32>> = Vec::new();
    for &row in rows {
        match runs.last_mut() {
            Some(run) if run.end == row => run.end = row + 1,
            _ => runs.push(row..row + 1),
        }
    }
    runs
}

impl Walk {
    pub(crate) fn new(
        filter: Option<FilterExpr>,
        keep_unmatched: bool,
        page_rows: u32,
        position: Position,
    ) -> Walk {
        Walk {
            filter,
            keep_unmatched,
            target: page_rows.clamp(STRETCH_MIN, STRETCH_MAX),
            stretch: None,
            position,
            region: None,
        }
    }

    /// Take up to `need` rows past the position, in the walk's order, from the view `open`
    /// resolved in `generation`. The position is left where it was; the caller commits the one
    /// [`Collected`] carries, or one of its own after cutting the page.
    pub(crate) fn collect(
        &mut self,
        engine: &Engine,
        open: &OpenView<'_>,
        generation: &Arc<Generation>,
        need: usize,
        clock: &mut Clock,
    ) -> Result<Collected> {
        match self.position {
            Position::Map { last, scan } => {
                self.collect_map(engine, open, generation, need, clock, last, scan)
            }
            Position::Stored { last, scan } => {
                self.collect_stored(engine, open, generation, need, clock, last, scan)
            }
        }
    }

    /// The position just after one taken row, for a page cut before the rows that followed it.
    pub(crate) fn position_at(&self, open: &OpenView<'_>, row: &Taken) -> Position {
        match self.position {
            Position::Map { .. } => {
                let key = map_key(&open.served.segments, row);
                Position::Map {
                    last: Some(key),
                    scan: Some(key),
                }
            }
            Position::Stored { .. } => Position::Stored {
                last: Some(row.entity),
                scan: Some(row.entity),
            },
        }
    }

    /// Forget a stretch evaluated under another publication, or opened past `scan`.
    fn keep_stretch_if(
        &mut self,
        generation: &Generation,
        opened_by: impl Fn(&StretchBody) -> bool,
    ) {
        if let Some(stretch) = &self.stretch {
            if !same_publication(&stretch.under, generation) || !opened_by(&stretch.body) {
                self.stretch = None;
            }
        }
    }

    fn grow(&mut self) {
        self.target = self.target.saturating_mul(STRETCH_GROWTH).min(STRETCH_MAX);
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_map(
        &mut self,
        engine: &Engine,
        open: &OpenView<'_>,
        generation: &Arc<Generation>,
        need: usize,
        clock: &mut Clock,
        last: Option<MapKey>,
        mut scan: Option<MapKey>,
    ) -> Result<Collected> {
        let segments = &open.served.segments;
        let mut rows: Vec<Taken> = Vec::new();
        let walked = loop {
            self.keep_stretch_if(generation, |body| {
                matches!(body, StretchBody::Map { from, .. } if *from <= scan)
            });
            if self.stretch.is_none() {
                if let Some(reason) = clock.stop_scan(!rows.is_empty()) {
                    break Walked::Stopped(reason);
                }
                match self.open_map_stretch(engine, open, generation, scan, clock.token())? {
                    Some(stretch) => self.stretch = Some(stretch),
                    None => break Walked::End,
                }
            }
            let Some(Stretch {
                body: StretchBody::Map { until, filter, .. },
                ..
            }) = &self.stretch
            else {
                unreachable!("a map walk opens map stretches");
            };
            let until = *until;
            take_map(
                segments,
                &open.mask,
                filter.as_ref(),
                self.keep_unmatched,
                scan,
                until,
                need,
                &engine.identity_key,
                &mut rows,
            );
            if rows.len() == need {
                scan = Some(map_key(segments, rows.last().expect("a filled page holds a row")));
                break Walked::Filled;
            }
            clock.scanned += 1;
            self.stretch = None;
            self.grow();
            if until > u64::from(u32::MAX) {
                scan = Some((u32::MAX, u64::MAX));
                break Walked::End;
            }
            scan = Some(((until - 1) as u32, u64::MAX));
        };
        let last = rows.last().map(|row| map_key(segments, row)).or(last);
        Ok(Collected {
            rows,
            walked,
            position: Position::Map { last, scan },
        })
    }

    /// The stretch past `scan`: every row of every segment whose cell is below the nearest cell a
    /// segment reaches `target` rows ahead, and at least the nearest cell any segment holds, so a
    /// stretch always covers a row. `None` where no segment holds a row past `scan`.
    fn open_map_stretch(
        &mut self,
        engine: &Engine,
        open: &OpenView<'_>,
        generation: &Arc<Generation>,
        scan: Option<MapKey>,
        cancel: &Option<CancelToken>,
    ) -> Result<Option<Stretch>> {
        let segments = &open.served.segments;
        let starts: Vec<u32> = segments
            .iter()
            .map(|&(segment, _)| first_after(segment, scan))
            .collect();
        let mut until: u64 = 1 << 32;
        let mut nearest: Option<u32> = None;
        for (&(segment, _), &start) in segments.iter().zip(&starts) {
            let cells = segment.morton.u32();
            let Some(&cell) = cells.get(start as usize) else {
                continue;
            };
            nearest = Some(nearest.map_or(cell, |n| n.min(cell)));
            if let Some(&reach) = cells.get(start as usize + self.target as usize) {
                until = until.min(u64::from(reach));
            }
        }
        let Some(nearest) = nearest else {
            return Ok(None);
        };
        let until = until.max(u64::from(nearest) + 1);
        let filter = match self.filter.clone() {
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
                Some(self.evaluate_map(engine, open, &expr, parts, cancel)?)
            }
        };
        Ok(Some(Stretch {
            under: Arc::clone(generation),
            body: StretchBody::Map {
                from: scan,
                until,
                filter,
            },
        }))
    }

    /// The filter over a map stretch's rows, as rows. The candidate is the stretch's visible
    /// items, so every entity-space scan costs the stretch and not the view.
    fn evaluate_map(
        &mut self,
        engine: &Engine,
        open: &OpenView<'_>,
        expr: &FilterExpr,
        parts: Vec<(usize, Range<u32>)>,
        cancel: &Option<CancelToken>,
    ) -> Result<FilterRows> {
        let served = &open.served;
        let mask = &open.mask;
        let rows_in_ranges: u64 = parts.iter().map(|(_, r)| r.len() as u64).sum();
        let mut entities: Vec<u32> = Vec::new();
        for (s, range) in &parts {
            let (segment, base) = served.segments[*s];
            let ids = segment.columns.tessera_id();
            for row in mask.rows_in_range(base + range.start..base + range.end).iter() {
                entities.push(entity_of(&engine.identity_key, ids[(row - base) as usize]));
            }
        }
        entities.sort_unstable();
        let candidate = Bitmap::of(&entities);
        let ranges = [parts];
        let row_bases: Vec<u32> = served.segments.iter().map(|&(_, base)| base).collect();
        let (rows, verdict) = engine.route_filters_under(served, mask, &candidate, cancel, |route| {
            Ok(match route(expr, true)? {
                RoutedFilter::Entity(matched) => (
                    engine.cross_filter_into_row_space(
                        served,
                        &matched,
                        &ranges,
                        rows_in_ranges,
                        true,
                    ),
                    None,
                ),
                RoutedFilter::Row(tree) => {
                    let domain = crossing_domain(&ranges, &row_bases);
                    (
                        engine.evaluate_row_route(&tree, served, &domain, rows_in_ranges, true)?,
                        tree.region_verdict(),
                    )
                }
            })
        })?;
        self.region = self.region.or(verdict);
        Ok(rows)
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_stored(
        &mut self,
        engine: &Engine,
        open: &OpenView<'_>,
        generation: &Arc<Generation>,
        need: usize,
        clock: &mut Clock,
        last: Option<u32>,
        mut scan: Option<u32>,
    ) -> Result<Collected> {
        let segments = &open.served.segments;
        let mut rows: Vec<Taken> = Vec::new();
        let walked = loop {
            self.keep_stretch_if(generation, |body| {
                matches!(body, StretchBody::Stored { from, .. } if *from <= scan)
            });
            if self.stretch.is_none() {
                if let Some(reason) = clock.stop_scan(!rows.is_empty()) {
                    break Walked::Stopped(reason);
                }
                match self.open_stored_stretch(engine, open, generation, scan, clock.token())? {
                    Some(stretch) => self.stretch = Some(stretch),
                    None => break Walked::End,
                }
            }
            let Some(Stretch {
                body:
                    StretchBody::Stored {
                        until,
                        items,
                        matched,
                        ..
                    },
                ..
            }) = &self.stretch
            else {
                unreachable!("a stored walk opens stored stretches");
            };
            let until = *until;
            take_stored(
                segments,
                &open.mask,
                items,
                matched.as_ref(),
                self.keep_unmatched,
                scan,
                need,
                &mut rows,
            );
            if rows.len() == need {
                scan = Some(rows.last().expect("a filled page holds a row").entity);
                break Walked::Filled;
            }
            clock.scanned += 1;
            self.stretch = None;
            self.grow();
            if until > u64::from(u32::MAX) {
                scan = Some(u32::MAX);
                break Walked::End;
            }
            scan = Some((until - 1) as u32);
        };
        let last = rows.last().map(|row| row.entity).or(last);
        Ok(Collected {
            rows,
            walked,
            position: Position::Stored { last, scan },
        })
    }

    /// The stretch past `scan`: the next `target` items of the viewer's candidate set, and of
    /// them the ones that hold a row in the view. `None` where the candidate holds nothing past
    /// `scan`.
    fn open_stored_stretch(
        &mut self,
        engine: &Engine,
        open: &OpenView<'_>,
        generation: &Arc<Generation>,
        scan: Option<u32>,
        cancel: &Option<CancelToken>,
    ) -> Result<Option<Stretch>> {
        let candidate = engine.filter_candidate(open.served.session, generation)?;
        let from: u64 = scan.map_or(0, |e| u64::from(e) + 1);
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
        let row_space = &open.served.data.row_space;
        let items: Vec<(u32, u32)> = range
            .iter()
            .filter_map(|entity| {
                row_space
                    .row_of(EntityId::new(u64::from(entity)))
                    .map(|row| (entity, row.raw()))
            })
            .collect();
        let matched = match self.filter.clone() {
            None => None,
            Some(expr) => Some(self.evaluate_stored(engine, open, &expr, &range, &items, cancel)?),
        };
        Ok(Some(Stretch {
            under: Arc::clone(generation),
            body: StretchBody::Stored {
                from: scan,
                until,
                items,
                matched,
            },
        }))
    }

    /// The filter over a stored stretch, as the items it admits. A row-space leaf is answered
    /// over the stretch's own rows, which the view need not hold contiguously.
    fn evaluate_stored(
        &mut self,
        engine: &Engine,
        open: &OpenView<'_>,
        expr: &FilterExpr,
        candidate: &Bitmap,
        items: &[(u32, u32)],
        cancel: &Option<CancelToken>,
    ) -> Result<Bitmap> {
        let served: &ServedView<'_> = &open.served;
        let (matched, verdict) =
            engine.route_filters_under(served, &open.mask, candidate, cancel, |route| {
                Ok(match route(expr, true)? {
                    RoutedFilter::Entity(matched) => (matched, None),
                    RoutedFilter::Row(tree) => {
                        let mut rows: Vec<u32> = items.iter().map(|&(_, row)| row).collect();
                        rows.sort_unstable();
                        let domain = runs_of(&rows);
                        let answered = engine.evaluate_row_route(
                            &tree,
                            served,
                            &domain,
                            rows.len() as u64,
                            true,
                        )?;
                        let admitted: Vec<u32> = items
                            .iter()
                            .filter(|&&(_, row)| answered.rows().contains(row))
                            .map(|&(entity, _)| entity)
                            .collect();
                        (Bitmap::of(&admitted), tree.region_verdict())
                    }
                })
            })?;
        self.region = self.region.or(verdict);
        Ok(matched)
    }
}

/// Take map-order rows past `scan` and below `until` until `rows` holds `need`: each segment's
/// rows in range that the page's mask admits, and the filter where rows must match, merged across
/// segments by `(cell, tessera_id)`.
#[allow(clippy::too_many_arguments)]
fn take_map(
    segments: &[(&SegmentData, u32)],
    mask: &EffectiveMask,
    filter: Option<&FilterRows>,
    keep_unmatched: bool,
    scan: Option<MapKey>,
    until: u64,
    need: usize,
    identity: &IdentityKey,
    rows: &mut Vec<Taken>,
) {
    struct Run<'a> {
        seg: usize,
        segment: &'a SegmentData,
        base: u32,
        rows: Vec<u32>,
        at: usize,
        /// Where every row is served, which rows matched.
        matched: Option<Bitmap>,
    }
    let mut runs: Vec<Run<'_>> = Vec::new();
    for (seg, &(segment, base)) in segments.iter().enumerate() {
        let start = first_after(segment, scan);
        let end = first_at_cell(segment, until);
        if start >= end {
            continue;
        }
        let visible = mask.rows_in_range(base + start..base + end);
        let (served, matched) = match (filter, keep_unmatched) {
            (None, _) => (visible, None),
            (Some(filter), false) => (visible.and(filter.rows()), None),
            (Some(filter), true) => {
                let matched = visible.and(filter.rows());
                (visible, Some(matched))
            }
        };
        if served.is_empty() {
            continue;
        }
        // No segment gives a page more rows than the page still needs.
        runs.push(Run {
            seg,
            segment,
            base,
            rows: served.iter().take(need - rows.len()).collect(),
            at: 0,
            matched,
        });
    }
    let key = |run: &Run<'_>| {
        let local = (run.rows[run.at] - run.base) as usize;
        (run.segment.morton.u32()[local], run.segment.columns.tessera_id()[local])
    };
    while rows.len() < need {
        let Some(next) = runs
            .iter()
            .enumerate()
            .filter(|(_, run)| run.at < run.rows.len())
            .min_by_key(|(_, run)| key(run))
            .map(|(i, _)| i)
        else {
            return;
        };
        let run = &mut runs[next];
        let row = run.rows[run.at];
        let local = row - run.base;
        let tessera_id = run.segment.columns.tessera_id()[local as usize];
        rows.push(Taken {
            seg: run.seg,
            local,
            tessera_id,
            entity: entity_of(identity, tessera_id),
            matched: run.matched.as_ref().is_none_or(|m| m.contains(row)),
        });
        run.at += 1;
    }
}

/// Take stored-order rows past `scan` from a stretch's items until `rows` holds `need`: each item
/// whose row the page's mask admits, and that matches where rows must match.
#[allow(clippy::too_many_arguments)]
fn take_stored(
    segments: &[(&SegmentData, u32)],
    mask: &EffectiveMask,
    items: &[(u32, u32)],
    matched: Option<&Bitmap>,
    keep_unmatched: bool,
    scan: Option<u32>,
    need: usize,
    rows: &mut Vec<Taken>,
) {
    let start = match scan {
        None => 0,
        Some(scan) => items.partition_point(|&(entity, _)| entity <= scan),
    };
    for &(entity, row) in &items[start..] {
        if rows.len() == need {
            return;
        }
        if !mask.contains_row(row) {
            continue;
        }
        let is_matched = matched.is_none_or(|m| m.contains(entity));
        if !is_matched && !keep_unmatched {
            continue;
        }
        let seg = segments.partition_point(|&(_, base)| base <= row) - 1;
        let (segment, base) = segments[seg];
        let local = row - base;
        rows.push(Taken {
            seg,
            local,
            tessera_id: segment.columns.tessera_id()[local as usize],
            entity,
            matched: is_matched,
        });
    }
}
