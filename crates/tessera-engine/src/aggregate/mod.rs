//! `POST /v1/aggregate`: how a set of the items a viewer may see in one view is distributed, across
//! the values of a category field, across bins of a number or timestamp field, across the
//! artifacts of one level of a layer, across the cells of the map, or across a group and cells
//! together. Every figure is an exact count over the
//! viewer's visible set, and a second set can be given to compare each figure with.
//!
//! [`Engine::aggregate_stream`] serves one response: a head, then each grouping's table in order,
//! each table a head and pages of rows, then a trailer. Every page composes the visible set again
//! from the latest generation, so a deletion or suppression accepted during a read applies from
//! the next page. Which groups a table lists under `top` is fixed at its first page and carried in
//! the cursor; a histogram is served in one page.
//!
//! Rows carry vocabulary keys, bin edges, artifacts' `tessera_id`s and cell prefixes. A code, an ordinal or an
//! entity id reaches the caller only inside a sealed cursor.

mod artifacts;
mod bins;
mod cursor;
pub(crate) mod set;
mod table;
mod values;

use std::time::Instant;

use arrow::record_batch::RecordBatch;
use tessera_types::TesseraId;

use crate::cancel::CancelToken;
use crate::engine::Engine;
use crate::error::{EngineError, Result};
use crate::filter::{FilterExpr, Scalar};
use crate::records::{
    page_rows_of, refuse_shape, Binding, Clock, PageEnd, PageEndedBy, RecordsLimits,
    ResponseEndedBy, Route,
};
use crate::region::RegionVerdict;
use crate::session::Session;
use crate::timing::Probe;
use crate::viewport::{filter_refusal, SinkClosed, SinkResult};

use cursor::{AggregateCursor, Position};
use table::Plan;
pub(crate) use table::{ALONE_ROWS, MIN_CHUNK_ROWS};

/// One `POST /v1/aggregate` response, as the engine sees it.
#[derive(Debug, Clone)]
pub struct AggregateRequest<'a> {
    /// A view id from `/v1/meta`. One this session cannot reach is an unknown view.
    pub view: &'a str,
    /// The set, resolved as the viewport resolves a filter. `None` is every item this viewer may
    /// see in the view.
    pub filter: Option<FilterExpr>,
    /// The set each count is compared with, drawn from the same visible set in the same view.
    pub reference: Option<Reference>,
    /// One table each, in this order.
    pub groupings: &'a [Grouping],
    /// Rows per page; `None` is the ceiling.
    pub page_rows: Option<u32>,
    /// The most pages this response may carry; `None` is as many as the budgets allow.
    pub pages: Option<u32>,
    /// A previous response's `next`, unchanged, under the same request.
    pub cursor: Option<&'a str>,
    pub limits: RecordsLimits,
    pub caps: AggregateCaps,
    /// As [`crate::ItemsRequest::cancel`]: cancelled before the first page, the response ends with
    /// no rows and the cursor it was given.
    pub cancel: Option<CancelToken>,
}

/// The set counts are compared with.
#[derive(Debug, Clone)]
pub enum Reference {
    /// Every item this viewer may see in the view.
    Visible,
    Filter(FilterExpr),
}

/// One table: an optional outer level of groups and an optional inner level of cells. With
/// neither, the table is the size of the set.
#[derive(Debug, Clone, PartialEq)]
pub struct Grouping {
    pub by: Option<By>,
    /// The depth of the cells, 0 to 32.
    pub cells: Option<u8>,
    /// With `cells`, the bbox `[x0, y0, x1, y1]` in the view's frame whose cells are listed, as a
    /// viewport's `bbox` lists tiles; `None` is the view's whole extent. A listed cell counts all
    /// of its items.
    pub area: Option<[f64; 4]>,
}

/// A grouping's outer level.
#[derive(Debug, Clone, PartialEq)]
pub enum By {
    /// The values of a category field, as a resolved column name: a group-scoped field arrives
    /// pinned to one view's column.
    Field { column: String, pick: Pick<String> },
    /// The values of a number or timestamp field, as a resolved column name, counted in at most
    /// `bins` bins of `range`, or with no range in readable bins around the values of the items
    /// the viewer may see in the view.
    Bins {
        column: String,
        bins: u32,
        range: Option<(Scalar, Scalar)>,
    },
    /// The artifacts of one level of a layer. `level` is required on a layer with several levels
    /// and refused on one with a single level.
    Layer {
        layer: String,
        level: Option<u32>,
        pick: Pick<TesseraId>,
    },
}

/// Which groups a table lists.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick<K> {
    /// The `n` with the most items in the set, ties by key or `tessera_id`.
    Top(u32),
    /// These, in this order, a repeat counted once.
    Named(Vec<K>),
}

/// The deployment's ceilings on a request's shape.
#[derive(Debug, Clone, Copy)]
pub struct AggregateCaps {
    pub groupings: u32,
    pub top: u32,
    pub named: u32,
    /// The most bins a histogram may ask for.
    pub bins: u32,
    /// The most cells a grouping's cell level may list, whatever its groups.
    pub cells: u64,
}

/// What precedes a response's tables, for its headers.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateHead {
    /// The viewport's identity coordinate for this session and view. `None` only where the
    /// response was cancelled before its first page.
    pub identity_key: Option<[u8; 16]>,
    /// The coarsest verdict the set's and the reference's region leaves reached on the first
    /// page; `None` where neither carries a region.
    pub region: Option<RegionVerdict>,
}

/// What precedes a table's first page in a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableHead {
    /// The table's position in the request's groupings.
    pub grouping: u32,
    /// Items in the set.
    pub total: u64,
    /// Items in the reference set, where one was given.
    pub reference_total: Option<u64>,
    /// With an outer level, the groups with an item in the set before the cut to `top` or the
    /// named list: visible distinct values, or served artifacts with a member in the set.
    pub groups: Option<u64>,
    /// Whether the table continues from a cursor.
    pub resumed: bool,
}

/// Where a response is delivered: the head once, then for each table its head and its pages. A
/// refusal means the consumer has gone, and ends the response with [`EngineError::Cancelled`].
pub trait AggregateSink {
    fn head(&mut self, head: &AggregateHead) -> SinkResult;
    fn table(&mut self, head: &TableHead) -> SinkResult;
    fn page(&mut self, grouping: u32, batch: &RecordBatch, end: &PageEnd) -> SinkResult;
}

/// What closes a response.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateTrailer {
    pub pages: u64,
    pub rows: u64,
    /// The cursor to resume from, `None` where no row of any table remains.
    pub next: Option<String>,
    pub ended_by: ResponseEndedBy,
    /// Whether a page counted over a different state of the corpus from the page before it, in
    /// this response or the one the cursor came from.
    pub recomposed: bool,
    /// The coarsest verdict any page's region leaves reached.
    pub region: Option<RegionVerdict>,
    pub timings: AggregateTimings,
}

/// Where a response spent its time, and how much it counted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AggregateTimings {
    pub compose_ns: u64,
    pub count_ns: u64,
    /// Counting cells, of which `pass_ns` walked the rows and the rest gathered their counts.
    pub cells_ns: u64,
    pub pass_ns: u64,
    /// Building the pages' Arrow batches.
    pub batch_ns: u64,
    /// Entities read through the rows of a set routed in row space.
    pub entities_crossed: u64,
    /// Cells counted by range, or chunks of rows walked by the pass.
    pub cells_walked: u64,
    /// For each table a page counted cells for, how: `ranges` or `pass`.
    pub methods: Vec<(u32, &'static str)>,
}

/// A `POST /v1/aggregate` request the caller can correct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateRefused {
    NoGroupings,
    /// More groupings, a longer top or a longer named list than the deployment allows.
    OverCap {
        what: &'static str,
        cap: &'static str,
        given: usize,
        limit: u32,
    },
    ZeroTop,
    EmptyNamed,
    /// Not a category field this deployment can count: undeclared, not a category, or a category
    /// with no per-value record and not drawn.
    NotCountable(String),
    /// Not a number or timestamp field this deployment can bin: not a number, or a number
    /// declared with neither `index` nor `render`.
    NotBinnable(String),
    /// A grouping by bins of a `bool` field.
    BinsOnBool(String),
    ZeroBins,
    /// A range whose lower bound is not below its upper bound.
    EmptyRange,
    /// A fractional bound on a timestamp field's range.
    FractionalTime(String),
    /// A histogram with a cell level.
    BinsWithCells,
    DepthPast32(u8),
    /// No `level` on a layer with several.
    LevelRequired(String),
    /// More cells at `depth` in the grouping's area than the deployment allows; `deepest` is the
    /// deepest depth at which the area fits, if any does.
    TooManyCells {
        depth: u8,
        count: u64,
        limit: u64,
        deepest: Option<u8>,
    },
}

impl std::fmt::Display for AggregateRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AggregateRefused::NoGroupings => {
                write!(f, "groupings is empty; send at least one grouping")
            }
            AggregateRefused::OverCap {
                what,
                cap,
                given,
                limit,
            } => write!(
                f,
                "{given} {what} is more than selection.{cap} allows; send at most {limit}"
            ),
            AggregateRefused::ZeroTop => {
                write!(f, "top is 0; ask for at least one group, or name them")
            }
            AggregateRefused::EmptyNamed => write!(
                f,
                "the named list is empty; name at least one group, or ask for top"
            ),
            AggregateRefused::NotCountable(field) => write!(
                f,
                "field '{field}' cannot be counted; name a category field declared with index or \
                 render"
            ),
            AggregateRefused::NotBinnable(field) => write!(
                f,
                "field '{field}' cannot be binned; name a number or timestamp field declared \
                 with index or render"
            ),
            AggregateRefused::ZeroBins => write!(f, "bins is 0; ask for at least one bin"),
            AggregateRefused::EmptyRange => write!(
                f,
                "the range is empty or reversed; give [lower, upper] with lower below upper"
            ),
            AggregateRefused::BinsOnBool(field) => write!(
                f,
                "field '{field}' is a bool, which has no bins; count true and false with a filter \
                 on each"
            ),
            AggregateRefused::FractionalTime(field) => write!(
                f,
                "field '{field}' holds timestamps, so its range is whole microseconds; give \
                 integers"
            ),
            AggregateRefused::BinsWithCells => write!(
                f,
                "a grouping by bins has no cell level; ask for cells in a grouping of its own"
            ),
            AggregateRefused::DepthPast32(depth) => write!(
                f,
                "cells depth {depth} is past the stored resolution; ask for a depth from 0 to 32"
            ),
            AggregateRefused::LevelRequired(layer) => {
                write!(f, "layer '{layer}' has several levels; name one with level")
            }
            AggregateRefused::TooManyCells { limit: 0, .. } => write!(
                f,
                "selection.max_aggregate_cells is 0, so no cell level can be served; ask \
                 without cells"
            ),
            AggregateRefused::TooManyCells {
                depth,
                count,
                limit,
                deepest,
            } => {
                write!(
                    f,
                    "{count} cells at depth {depth} in this area is more than \
                     selection.max_aggregate_cells allows ({limit}); "
                )?;
                match deepest {
                    Some(d) => write!(f, "ask for depth {d} or less, or a smaller area"),
                    None => write!(f, "no depth fits"),
                }
            }
        }
    }
}

/// Everything a response fixes before its first page.
struct Planned<'r> {
    req: AggregateRequest<'r>,
    plans: Vec<Plan>,
    page_rows: u32,
    binding: Binding<'r>,
    /// Whether a set is routed in row space where a column affords it: where a table counts
    /// cells, artifacts or a field held only in the view's rows.
    prefer_row: bool,
}

impl Engine {
    /// Serve one `POST /v1/aggregate` response into `sink` and return its trailer. Every refusal is
    /// decided before the head: the request's shape, the view, each grouping, the cursor, then the
    /// filters. An `Err` after the head leaves the response without a trailer, which a client
    /// reads as incomplete and resumes from the last page end.
    pub fn aggregate_stream(
        &self,
        session: &Session,
        req: AggregateRequest<'_>,
        sink: &mut dyn AggregateSink,
    ) -> Result<AggregateTrailer> {
        let started = Instant::now();
        refuse_shape(req.page_rows, req.pages, false, req.cursor)?;
        refuse_groupings(req.groupings, &req.caps)?;
        let (planned, position) = self.plan_aggregate(session, req)?;
        self.serve_tables(session, planned, position, started, sink)
    }

    fn plan_aggregate<'r>(
        &self,
        session: &'r Session,
        req: AggregateRequest<'r>,
    ) -> Result<(Planned<'r>, Position)> {
        let generation = self.generation.load_full();
        let manifest = &generation.bundle.manifest;
        let unknown_view = || EngineError::UnknownView(req.view.to_string());
        if !session.visible_views().contains_view(req.view) {
            return Err(unknown_view());
        }
        let plans = req
            .groupings
            .iter()
            .enumerate()
            .map(|(index, grouping)| {
                Plan::of(self, session, &generation, req.view, index, grouping, req.caps.cells)
            })
            .collect::<Result<Vec<_>>>()?;
        let layers: Vec<Option<u64>> = plans.iter().map(Plan::layer_entity).collect();
        let binding = Binding {
            route: Route::Aggregate,
            view: req.view,
            incarnation: manifest.incarnation_of(req.view).ok_or_else(unknown_view)?,
            auth_data_hash: session.auth_data_hash(),
            layer: None,
            request: Some(cursor::digest(&req, &layers)),
        };
        let position = match req.cursor {
            None => Position::start(),
            Some(token) => {
                let position = AggregateCursor::decode(&self.cursor_key.open(&binding, token)?)?;
                if position.table as usize > req.groupings.len() {
                    return Err(EngineError::CursorRefused);
                }
                position
            }
        };
        let reaches = |layer: &str| self.reaches_layer(session, layer);
        for expr in req.filter.iter().chain(match &req.reference {
            Some(Reference::Filter(expr)) => Some(expr),
            _ => None,
        }) {
            generation
                .filter_columns
                .admit(expr, true, &reaches)
                .map_err(filter_refusal)?;
        }
        let prefer_row = plans.iter().any(Plan::wants_rows);
        let page_rows = page_rows_of(req.page_rows, &req.limits);
        Ok((
            Planned {
                req,
                plans,
                page_rows,
                binding,
                prefer_row,
            },
            position,
        ))
    }

    /// The head, then pages until every table is sent or the response ends, then the trailer. A
    /// page holds one table's rows. Each page opens the view and composes the sets again; the head
    /// follows the first composition, so it carries that page's region verdict.
    fn serve_tables(
        &self,
        session: &Session,
        planned: Planned<'_>,
        mut position: Position,
        started: Instant,
        sink: &mut dyn AggregateSink,
    ) -> Result<AggregateTrailer> {
        let req = &planned.req;
        let limits = &req.limits;
        let clock = Clock::new(started, limits.response_time, req.cancel.clone());
        let mut timings = AggregateTimings::default();
        let (mut pages, mut rows, mut bytes) = (0u64, 0u64, 0usize);
        let mut region: Option<RegionVerdict> = None;
        let mut head_sent = false;
        let mut recomposed = false;
        // The table whose head this response has sent.
        let mut table_sent: Option<u32> = None;
        let mut held = None;
        let tables = req.groupings.len() as u32;
        let ended_by = loop {
            if position.table >= tables {
                break ResponseEndedBy::End;
            }
            if pages > 0 {
                if req.pages.is_some_and(|limit| pages >= u64::from(limit)) {
                    break ResponseEndedBy::Pages;
                }
                if bytes.saturating_add(limits.max_page_bytes) > limits.response_bytes {
                    break ResponseEndedBy::BudgetBytes;
                }
                if clock.out_of_time() {
                    break ResponseEndedBy::BudgetTime;
                }
            }
            if clock.cancelled() {
                break ResponseEndedBy::Deadline;
            }
            let generation = self.generation.load_full();
            let open = match self.open_view(
                session,
                &generation,
                req.view,
                &req.cancel,
                &mut Probe::new(),
            ) {
                Err(EngineError::Cancelled) => break ResponseEndedBy::Deadline,
                open => open?,
            };
            let composing = Instant::now();
            let sets = match set::compose(self, &open, &generation, req, planned.prefer_row) {
                Err(EngineError::Cancelled) => break ResponseEndedBy::Deadline,
                sets => sets?,
            };
            timings.compose_ns += composing.elapsed().as_nanos() as u64;
            let page_region = sets.region();
            region = RegionVerdict::coarsest(region, page_region);
            if !head_sent {
                head_sent = true;
                let head = AggregateHead {
                    identity_key: Some(open.coordinates.identity_key),
                    region: page_region,
                };
                sink.head(&head)
                    .map_err(|SinkClosed| EngineError::Cancelled)?;
            }
            let stamp = (
                generation.segments_version,
                generation.overlay_version,
                open.geometry.fragment.watermark,
            );
            if position.stamp.is_some_and(|held| held != stamp) {
                recomposed = true;
            }
            position.stamp = Some(stamp);

            let grouping = position.table;
            let plan = &planned.plans[grouping as usize];
            let cx = set::Cx::new(self, &open, &generation, &sets, &req.cancel);
            let budget = table::Budget {
                deadline: (pages > 0).then(|| started + limits.response_time),
                page_rows: planned.page_rows,
                max_page_bytes: limits.max_page_bytes,
                pages_left: req.pages.map_or(u64::MAX, |limit| u64::from(limit) - pages),
                response_bytes_left: (limits.response_bytes as u64).saturating_sub(bytes as u64),
            };
            let page = match plan.page(&cx, &position, &budget, &mut timings, &mut held) {
                Err(EngineError::Cancelled) => break ResponseEndedBy::Deadline,
                Ok(None) => break ResponseEndedBy::BudgetTime,
                Ok(Some(page)) => page,
                Err(e) => return Err(e),
            };
            position = page.next.clone();
            if table_sent != Some(grouping) {
                table_sent = Some(grouping);
                sink.table(&page.head)
                    .map_err(|SinkClosed| EngineError::Cancelled)?;
            }
            pages += 1;
            rows += page.batch.num_rows() as u64;
            bytes += page.bytes;
            let done = position.table >= tables;
            let end = PageEnd {
                next: (!done).then(|| self.seal_aggregate(&planned.binding, &position)),
                ended_by: if done {
                    PageEndedBy::End
                } else if page.cut_by_bytes {
                    PageEndedBy::Bytes
                } else {
                    PageEndedBy::Rows
                },
                bytes: page.bytes,
            };
            sink.page(grouping, &page.batch, &end)
                .map_err(|SinkClosed| EngineError::Cancelled)?;
        };
        if !head_sent {
            sink.head(&AggregateHead {
                identity_key: None,
                region: None,
            })
            .map_err(|SinkClosed| EngineError::Cancelled)?;
        }
        Ok(AggregateTrailer {
            pages,
            rows,
            next: (position.table < tables)
                .then(|| self.seal_aggregate(&planned.binding, &position)),
            ended_by,
            recomposed,
            region,
            timings,
        })
    }

    fn seal_aggregate(&self, binding: &Binding<'_>, position: &Position) -> String {
        self.cursor_key
            .seal(binding, &AggregateCursor::encode(position))
    }
}

/// The refusals a request's groupings decide from their shape alone.
fn refuse_groupings(groupings: &[Grouping], caps: &AggregateCaps) -> Result<()> {
    let refused = |why| Err(EngineError::AggregateRefused(why));
    if groupings.is_empty() {
        return refused(AggregateRefused::NoGroupings);
    }
    if groupings.len() > caps.groupings as usize {
        return refused(AggregateRefused::OverCap {
            what: "groupings",
            cap: "max_aggregate_groupings",
            given: groupings.len(),
            limit: caps.groupings,
        });
    }
    for grouping in groupings {
        if let Some(depth) = grouping.cells.filter(|&depth| depth > 32) {
            return refused(AggregateRefused::DepthPast32(depth));
        }
        let (top, named) = match &grouping.by {
            None => continue,
            Some(By::Bins { bins, range, .. }) => {
                refuse_bins(*bins, range.as_ref(), grouping.cells.is_some(), caps)?;
                continue;
            }
            Some(By::Field { pick, .. }) => match pick {
                Pick::Top(n) => (Some(*n), None),
                Pick::Named(keys) => (None, Some(keys.len())),
            },
            Some(By::Layer { pick, .. }) => match pick {
                Pick::Top(n) => (Some(*n), None),
                Pick::Named(ids) => (None, Some(ids.len())),
            },
        };
        match (top, named) {
            (Some(0), _) => return refused(AggregateRefused::ZeroTop),
            (Some(n), _) if n > caps.top => {
                return refused(AggregateRefused::OverCap {
                    what: "top",
                    cap: "max_aggregate_top",
                    given: n as usize,
                    limit: caps.top,
                })
            }
            (_, Some(0)) => return refused(AggregateRefused::EmptyNamed),
            (_, Some(n)) if n > caps.named as usize => {
                return refused(AggregateRefused::OverCap {
                    what: "named groups",
                    cap: "max_aggregate_named",
                    given: n,
                    limit: caps.named,
                })
            }
            _ => {}
        }
    }
    Ok(())
}

/// The refusals a grouping by bins earns from its shape alone.
fn refuse_bins(
    bins: u32,
    range: Option<&(Scalar, Scalar)>,
    cells: bool,
    caps: &AggregateCaps,
) -> Result<()> {
    let refused = |why| Err(EngineError::AggregateRefused(why));
    if bins == 0 {
        return refused(AggregateRefused::ZeroBins);
    }
    if bins > caps.bins {
        return refused(AggregateRefused::OverCap {
            what: "bins",
            cap: "max_aggregate_bins",
            given: bins as usize,
            limit: caps.bins,
        });
    }
    if range.is_some_and(|&(lower, upper)| !bins::below(lower, upper)) {
        return refused(AggregateRefused::EmptyRange);
    }
    if cells {
        return refused(AggregateRefused::BinsWithCells);
    }
    Ok(())
}
