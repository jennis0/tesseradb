//! The bulk reads. `POST /v1/items`: a viewer reads, page by page, every item they may see in one
//! view that matches a filter, with the fields they name. `POST /v1/artifacts`: every artifact of
//! one layer they are served (the walk is in `artifacts`). [`Engine::items_stream`] and
//! [`Engine::artifacts_stream`] each serve one response through one loop: a head, then pages, each
//! an Arrow batch with a page end carrying the cursor to resume from, and a trailer it returns.
//!
//! Every page of either route is built from the latest generation with the visible set composed
//! again, exactly as a viewport composes it, so a deletion or suppression accepted during a read
//! applies from the next page. A page takes its rows from that mask and reads fields for those
//! rows alone. Rows are addressed by `tessera_id`; an entity id or any other internal position
//! reaches the caller only inside a sealed cursor.
//!
//! An items read's map order is `(cell, tessera_id)` merged across the view's segments, and its
//! stored order is ascending item number. Either resumes from a position that is a value, found
//! again in whatever segments the next page's generation holds, so a flush, merge or fold between
//! pages loses no row and repeats none.

mod artifacts;
mod columns;
mod cursor;
mod plan;
mod walk;

use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::record_batch::RecordBatch;

use crate::cancel::CancelToken;
use crate::engine::Engine;
use crate::error::{EngineError, Result};
use crate::filter::FilterExpr;
use crate::region::RegionVerdict;
use crate::session::Session;
use crate::timing::Probe;
use crate::viewport::{filter_refusal, meta_of, OpenView, SinkClosed, SinkResult};
use crate::Generation;

pub use artifacts::ArtifactsRequest;
pub(crate) use cursor::CursorKey;
use cursor::{Binding, ItemsCursor, Position, Route};
use columns::read_page;
use plan::FieldPlan;
use walk::{filter_rows, Clock, Collected, PageCx, Walk, Walked};

/// The order a read returns its rows in. Both return the same rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordsOrder {
    /// By map cell in the view, then `tessera_id`.
    Map,
    /// By the internal item numbering the record store holds rows in.
    Stored,
}

impl RecordsOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordsOrder::Map => "map",
            RecordsOrder::Stored => "stored",
        }
    }
}

/// A response's ceilings and budgets, from the deployment's configuration.
#[derive(Debug, Clone, Copy)]
pub struct RecordsLimits {
    /// Rows per page; a request asking for more is served this many.
    pub max_page_rows: u32,
    /// Arrow bytes per page, before compression. A single row larger than this is sent alone.
    /// A stretch the filter is evaluated over holds no more than this either.
    pub max_page_bytes: usize,
    /// Arrow bytes a response carries. No page starts that could take it past this.
    pub response_bytes: usize,
    /// Time a response runs, read between pages and between chunks of a page's scan. A page
    /// holding rows when it runs out is sent short. The walk honours it only once stopping moves
    /// the cursor on, so every response advances the read, and a response runs past it by at most
    /// one stretch's filter evaluation and one chunk of the scan.
    pub response_time: Duration,
}

/// One `POST /v1/items` response, as the engine sees it.
#[derive(Debug, Clone)]
pub struct ItemsRequest<'a> {
    /// A view id from `/v1/meta`. One this session cannot reach is an unknown view.
    pub view: &'a str,
    /// Declared fields, by name, in the order their columns are wanted. A group-scoped field may
    /// be pinned as `<field>@<key>`.
    pub fields: &'a [String],
    /// Any of `position`, `external_id` and `labels`, in the order their columns are wanted.
    pub system_fields: &'a [String],
    /// The viewport's filter, resolved as it resolves one. `None` is every visible item.
    pub filter: Option<FilterExpr>,
    /// Every visible item with a `tessera:matched` column, instead of the matching ones only.
    pub keep_unmatched: bool,
    /// Put the visible and matching counts in the head. Refused with a cursor.
    pub count: bool,
    /// `None` takes the cursor's order, or with no cursor the engine's choice.
    pub order: Option<RecordsOrder>,
    /// Rows per page; `None` is the ceiling.
    pub page_rows: Option<u32>,
    /// The most pages this response may carry; `None` is as many as the budgets allow.
    pub pages: Option<u32>,
    /// A previous response's `next`, unchanged.
    pub cursor: Option<&'a str>,
    /// The idset the caller holds `tessera_id`s under, checked as the item route checks it.
    pub idset: Option<u32>,
    pub limits: RecordsLimits,
    /// Cancellation ends the response with a trailer whose `ended_by` is `deadline`, after a
    /// page holding whatever rows the page under way had reached. The walk honours it only once
    /// stopping moves the cursor on, so it may run on for one stretch's filter evaluation and one
    /// chunk of the scan after it. A token cancelled before the response walks its first page
    /// ends the response with no rows, no counts and the cursor it was given, since the client
    /// has gone.
    pub cancel: Option<CancelToken>,
}

/// The counts a head carries under `count`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordsCounts {
    /// The rows the read serves before its filter: the view's visible items, which is the count
    /// the viewport serves, or the layer's artifacts this viewer is served.
    pub served: u64,
    /// Of those, the ones the filter matches; `served` without one.
    pub matched: u64,
}

/// What precedes a response's pages.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordsHead {
    /// The order an items response uses; `None` on the artifacts route, which has one order.
    pub order: Option<RecordsOrder>,
    /// The page size used, after the ceiling.
    pub page_rows: u32,
    pub counts: Option<RecordsCounts>,
    /// The coarsest verdict the response's region leaves reached before the head was sent: in
    /// the count's evaluation and the first page's, for the response's header.
    pub region: Option<RegionVerdict>,
    /// The viewport's identity coordinate for this session and view, from the first page's
    /// geometry, for the response's header. `None` only where the response was cancelled before
    /// its first page, which happens only when the client has gone.
    pub identity_key: Option<[u8; 16]>,
}

/// Why a page ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageEndedBy {
    /// It holds the page size.
    Rows,
    /// The next row would have taken it past the byte ceiling.
    Bytes,
    /// The response's time ran out, or it was cancelled, while the page held rows; the response
    /// ends after it.
    Time,
    /// No row remains.
    End,
}

impl PageEndedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            PageEndedBy::Rows => "rows",
            PageEndedBy::Bytes => "bytes",
            PageEndedBy::Time => "time",
            PageEndedBy::End => "end",
        }
    }
}

/// What follows a page's batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageEnd {
    /// The cursor to resume after this page from, base64url; `None` where no row remains.
    pub next: Option<String>,
    pub ended_by: PageEndedBy,
    /// The page's Arrow bytes, as counted against the ceilings.
    pub bytes: usize,
}

/// Why a response ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseEndedBy {
    End,
    Pages,
    BudgetBytes,
    BudgetTime,
    /// Cancelled.
    Deadline,
}

impl ResponseEndedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            ResponseEndedBy::End => "end",
            ResponseEndedBy::Pages => "pages",
            ResponseEndedBy::BudgetBytes => "budget_bytes",
            ResponseEndedBy::BudgetTime => "budget_time",
            ResponseEndedBy::Deadline => "deadline",
        }
    }
}

/// What closes a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordsTrailer {
    pub pages: u64,
    pub rows: u64,
    /// The cursor to resume from, `None` where no row remains. It can be past the last page end,
    /// where the scan went on without finding a row.
    pub next: Option<String>,
    pub ended_by: ResponseEndedBy,
}

/// Where a response is delivered: the head once, first, then each page with its end. A refusal
/// means the consumer has gone, and ends the response with [`EngineError::Cancelled`].
pub trait RecordsSink {
    fn head(&mut self, head: &RecordsHead) -> SinkResult;
    fn page(&mut self, batch: &RecordBatch, end: &PageEnd) -> SinkResult;
}

/// A records request the caller can correct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordsRefused {
    /// No declared field, or none this viewer's groups reach, has this name.
    UnknownField(String),
    RepeatedField(String),
    /// A group-scoped field named bare under a view that decides none of its group's columns.
    Unpinned { field: String, group: String },
    /// A group-scoped `text` field, which has no stored value.
    ScopedText(String),
    /// A pin on a field that is not group-scoped.
    PinOnUnscoped(String),
    UnknownSystemField(String),
    ZeroPageRows,
    ZeroPages,
    CountWithCursor,
    /// Not a layer `/v1/meta` publishes to this viewer in the view: one answer for every reason.
    UnknownLayer(String),
    /// `level` on a layer whose kind has one level.
    OneLevel(String),
    /// `level` past the levels the layer holds.
    NoSuchLevel { layer: String, held: usize },
    /// A property the artifacts route does not serve.
    UnknownProperty(String),
    /// `parent` and `q` together.
    ParentWithQ,
}

impl std::fmt::Display for RecordsRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordsRefused::UnknownField(name) => write!(
                f,
                "field '{name}' is not declared; name a field /v1/meta publishes"
            ),
            RecordsRefused::RepeatedField(name) => {
                write!(f, "field '{name}' is named twice; name each field once")
            }
            RecordsRefused::Unpinned { field, group } => write!(
                f,
                "field '{field}' is declared per view of group '{group}', and this view decides \
                 none of them; pin one as '{field}@<key>'"
            ),
            RecordsRefused::ScopedText(name) => write!(
                f,
                "field '{name}' is a group-scoped text field, which has no stored value to \
                 return; leave it out of fields"
            ),
            RecordsRefused::PinOnUnscoped(name) => write!(
                f,
                "field '{name}' is not group-scoped, so it takes no pin; name it without '@'"
            ),
            RecordsRefused::UnknownSystemField(name) => write!(
                f,
                "system field '{name}' is none of position, external_id or labels; name one of \
                 those"
            ),
            RecordsRefused::ZeroPageRows => write!(
                f,
                "page_rows is 0; ask for at least one row per page, or leave it out for the \
                 ceiling"
            ),
            RecordsRefused::ZeroPages => write!(
                f,
                "pages is 0; ask for at least one page, or leave it out for as many as the \
                 response allows"
            ),
            RecordsRefused::CountWithCursor => write!(
                f,
                "count is asked for with a cursor; ask for counts on a request without one"
            ),
            RecordsRefused::UnknownLayer(layer) => write!(
                f,
                "'{layer}' is not a layer published to you in this view; name one /v1/meta lists"
            ),
            RecordsRefused::OneLevel(layer) => write!(
                f,
                "layer '{layer}' has one level, so level names nothing; leave it out"
            ),
            RecordsRefused::NoSuchLevel { layer, held } => write!(
                f,
                "layer '{layer}' holds {held} level(s), numbered from 0; name one of those"
            ),
            RecordsRefused::UnknownProperty(name) => write!(
                f,
                "'{name}' is not an artifact property; name any of key, level, parents, target, \
                 masked_count, content, centroid, box and shape"
            ),
            RecordsRefused::ParentWithQ => write!(
                f,
                "parent and q are both given; send one of them"
            ),
        }
    }
}

/// One page's outcome.
enum Paged {
    Rows {
        batch: RecordBatch,
        bytes: usize,
        ended_by: PageEndedBy,
        /// Why the response ends after this page, where it does for time or cancellation.
        then: Option<ResponseEndedBy>,
    },
    End,
    Stopped(ResponseEndedBy),
}

/// The head's counts, and the verdict the counting evaluation's region leaves reached.
type Counted = (RecordsCounts, Option<RegionVerdict>);

/// What a response of either route fixes before its first page, and which the response loop
/// reads: the session, the view every page opens, the page size and the budgets.
struct Response<'r> {
    session: &'r Session,
    view: &'r str,
    order: Option<RecordsOrder>,
    page_rows: u32,
    pages: Option<u32>,
    count: bool,
    limits: RecordsLimits,
    cancel: Option<CancelToken>,
}

/// One route's part of a response: its counts, its pages and its cursor. The loop that calls
/// these, the budgets it keeps and the head, page ends and trailer it sends are shared.
trait Pager {
    /// The head's counts, under the first page's view.
    fn count(&mut self, engine: &Engine, open: &OpenView<'_>, generation: &Arc<Generation>)
        -> Result<Counted>;
    /// The next page under `open`, moving the position to its end.
    fn page(
        &mut self,
        engine: &Engine,
        open: &OpenView<'_>,
        generation: &Arc<Generation>,
        clock: &mut Clock,
    ) -> Result<Paged>;
    /// The position reached, sealed.
    fn cursor(&self, engine: &Engine) -> String;
    /// The coarsest verdict the pages' region leaves have reached.
    fn region(&self) -> Option<RegionVerdict>;
}

/// The items route's pager: the request as planned and the walk through the view.
struct ItemsPager<'r> {
    planned: Planned<'r>,
    walk: Walk,
}

/// Everything an items response fixes before its first page: the request, its fields and order,
/// and the binding its cursors are sealed under.
struct Planned<'r> {
    req: ItemsRequest<'r>,
    plan: FieldPlan,
    page_rows: u32,
    idset: u32,
    binding: Binding<'r>,
}

impl Engine {
    /// Serve one `POST /v1/items` response into `sink` and return its trailer. Every refusal is
    /// decided before the head: the request's shape, the idset, the view, the cursor (before any
    /// position in it is used), the fields, then the filter. An `Err` after the head leaves the
    /// response without a trailer, which a client reads as incomplete and resumes from the last
    /// page end.
    pub fn items_stream(
        &self,
        session: &Session,
        req: ItemsRequest<'_>,
        sink: &mut dyn RecordsSink,
    ) -> Result<RecordsTrailer> {
        let started = Instant::now();
        refuse_shape(req.page_rows, req.pages, req.count, req.cursor)?;
        let (response, mut pager) = self.plan_items(session, req)?;
        self.serve_pages(&response, &mut pager, started, sink)
    }

    /// The generation, the idset, the view, the cursor, the fields and the filter, in that order,
    /// then the order, the page size and the counts.
    fn plan_items<'r>(
        &self,
        session: &'r Session,
        req: ItemsRequest<'r>,
    ) -> Result<(Response<'r>, ItemsPager<'r>)> {
        let generation = self.generation.load_full();
        let manifest = &generation.bundle.manifest;
        let idset = manifest.identity.idset;
        if req.idset.is_some_and(|presented| presented != idset) {
            return Err(EngineError::StaleIdSet);
        }
        let unknown_view = || EngineError::UnknownView(req.view.to_string());
        if !session.visible_views().contains_view(req.view) {
            return Err(unknown_view());
        }
        let binding = Binding {
            route: Route::Items,
            view: req.view,
            incarnation: manifest.incarnation_of(req.view).ok_or_else(unknown_view)?,
            auth_data_hash: session.auth_data_hash(),
            layer: None,
        };
        let resumed = match req.cursor {
            None => None,
            Some(token) => Some(ItemsCursor::decode(&self.cursor_key.open(&binding, token)?)?),
        };
        if let Some(cursor) = &resumed {
            if cursor.idset != idset {
                return Err(EngineError::StaleIdSet);
            }
            if req.order.is_some_and(|order| order != cursor.position.order) {
                return Err(EngineError::CursorRefused);
            }
        }
        let plan = FieldPlan::resolve(
            &meta_of(&generation),
            req.view,
            session.visible_views(),
            req.fields,
            req.system_fields,
        )?;
        if let Some(expr) = &req.filter {
            generation
                .filter_columns
                .admit(expr, true, &|layer| self.reaches_layer(session, layer))
                .map_err(filter_refusal)?;
        }
        let order = resumed
            .map(|cursor| cursor.position.order)
            .or(req.order)
            .unwrap_or_else(|| plan.preferred_order());
        let page_rows = page_rows_of(req.page_rows, &req.limits);
        let walk = Walk::new(
            req.filter.clone(),
            req.keep_unmatched,
            resumed.map_or(page_rows, |cursor| cursor.stretch),
            req.limits.max_page_bytes,
            resumed.map_or(Position::start(order), |cursor| cursor.position),
        );
        let response = Response {
            session,
            view: req.view,
            order: Some(order),
            page_rows,
            pages: req.pages,
            count: req.count,
            limits: req.limits,
            cancel: req.cancel.clone(),
        };
        let pager = ItemsPager {
            planned: Planned {
                req,
                plan,
                page_rows,
                idset,
                binding,
            },
            walk,
        };
        Ok((response, pager))
    }

    /// The head, then pages until the response ends, then the trailer. The head follows the first
    /// page, so a failure there is a plain refusal and the head carries the first page's region
    /// verdict.
    fn serve_pages(
        &self,
        response: &Response<'_>,
        pager: &mut dyn Pager,
        started: Instant,
        sink: &mut dyn RecordsSink,
    ) -> Result<RecordsTrailer> {
        let limits = &response.limits;
        let mut clock = Clock::new(started, limits.response_time, response.cancel.clone());
        let mut counted: Option<Counted> = None;
        let mut head_sent = false;
        let mut send_head = |pager: &dyn Pager,
                             counted: Option<Counted>,
                             identity_key,
                             sink: &mut dyn RecordsSink|
         -> Result<()> {
            if !head_sent {
                head_sent = true;
                let head = RecordsHead {
                    order: response.order,
                    page_rows: response.page_rows,
                    counts: counted.map(|(counts, _)| counts),
                    region: RegionVerdict::coarsest(
                        counted.and_then(|(_, region)| region),
                        pager.region(),
                    ),
                    identity_key,
                };
                sink.head(&head).map_err(|SinkClosed| EngineError::Cancelled)?;
            }
            Ok(())
        };
        let (mut pages, mut rows, mut bytes) = (0u64, 0u64, 0usize);
        let ended_by = loop {
            if pages > 0 {
                if response.pages.is_some_and(|limit| pages >= u64::from(limit)) {
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
            // Each page reads the latest generation and composes the view's mask for itself; the
            // first also counts under them, when the request asked for counts.
            let generation = self.generation.load_full();
            // A cancellation that lands while the view opens ends the response as one seen above.
            let open = match self.open_view(
                response.session,
                &generation,
                response.view,
                &response.cancel,
                &mut Probe::new(),
            ) {
                Err(EngineError::Cancelled) => break ResponseEndedBy::Deadline,
                open => open?,
            };
            if response.count && counted.is_none() {
                counted = Some(pager.count(self, &open, &generation)?);
            }
            let page = pager.page(self, &open, &generation, &mut clock)?;
            send_head(pager, counted, Some(open.coordinates.identity_key), sink)?;
            match page {
                Paged::Rows {
                    batch,
                    bytes: page_bytes,
                    ended_by,
                    then,
                } => {
                    pages += 1;
                    rows += batch.num_rows() as u64;
                    bytes += page_bytes;
                    let end = PageEnd {
                        next: (ended_by != PageEndedBy::End).then(|| pager.cursor(self)),
                        ended_by,
                        bytes: page_bytes,
                    };
                    sink.page(&batch, &end)
                        .map_err(|SinkClosed| EngineError::Cancelled)?;
                    if ended_by == PageEndedBy::End {
                        break ResponseEndedBy::End;
                    }
                    if let Some(reason) = then {
                        break reason;
                    }
                }
                Paged::End => break ResponseEndedBy::End,
                Paged::Stopped(reason) => break reason,
            }
        };
        send_head(pager, counted, None, sink)?;
        Ok(RecordsTrailer {
            pages,
            rows,
            next: (ended_by != ResponseEndedBy::End).then(|| pager.cursor(self)),
            ended_by,
        })
    }
}

impl Pager for ItemsPager<'_> {
    /// The view's visible items, and of them the ones the filter matches, by one evaluation of
    /// the filter over the whole view on the route every page takes, under the first page's mask.
    fn count(
        &mut self,
        engine: &Engine,
        open: &OpenView<'_>,
        generation: &Arc<Generation>,
    ) -> Result<Counted> {
        let cx = PageCx::new(engine, open, generation);
        let req = &self.planned.req;
        let visible = open.mask.visible_total();
        let Some(expr) = &req.filter else {
            return Ok((
                RecordsCounts {
                    served: visible,
                    matched: visible,
                },
                None,
            ));
        };
        let total = u32::try_from(open.served.data.row_space.total_rows()).unwrap_or(u32::MAX);
        let whole_view = std::iter::once(0..total).collect::<Vec<_>>();
        let routed = filter_rows(&cx, expr, None, &whole_view, false, &req.cancel)?;
        let matched = open
            .mask
            .rows_in_range(0..total)
            .and_cardinality(routed.rows.rows());
        Ok((
            RecordsCounts {
                served: visible,
                matched,
            },
            routed.region,
        ))
    }

    /// The rows walked from the position, their fields read and the batch cut to the byte
    /// ceiling. A walk stopped for time or cancellation while holding rows makes a short page of
    /// them. The walk's position moves to the page's end, or where the page holds no row, to the
    /// scan position reached.
    fn page(
        &mut self,
        engine: &Engine,
        open: &OpenView<'_>,
        generation: &Arc<Generation>,
        clock: &mut Clock,
    ) -> Result<Paged> {
        let cx = PageCx::new(engine, open, generation);
        let planned = &self.planned;
        let walk = &mut self.walk;
        let req = &planned.req;
        let Collected {
            rows,
            walked,
            position,
        } = walk.collect(&cx, planned.page_rows as usize, clock)?;
        if rows.is_empty() {
            walk.position = position;
            return Ok(match walked {
                Walked::Stopped(reason) => Paged::Stopped(reason),
                _ => Paged::End,
            });
        }
        let (batch, kept, bytes) = read_page(
            &cx,
            &planned.plan,
            &rows,
            req.keep_unmatched,
            req.limits.max_page_bytes,
        )?;
        let then = match walked {
            Walked::Stopped(reason) => Some(reason),
            _ => None,
        };
        let ended_by = if kept < rows.len() {
            walk.position = walk.position_at(&cx, &rows[kept - 1]);
            PageEndedBy::Bytes
        } else {
            walk.position = position;
            match walked {
                Walked::End => PageEndedBy::End,
                Walked::Stopped(_) => PageEndedBy::Time,
                Walked::Filled => PageEndedBy::Rows,
            }
        };
        Ok(Paged::Rows {
            batch,
            bytes,
            ended_by,
            then,
        })
    }

    fn cursor(&self, engine: &Engine) -> String {
        let cursor = ItemsCursor {
            idset: self.planned.idset,
            position: self.walk.position,
            stretch: self.walk.target,
        };
        engine.cursor_key.seal(&self.planned.binding, &cursor.encode())
    }

    fn region(&self) -> Option<RegionVerdict> {
        self.walk.region
    }
}

/// The page size a request is served: its own, held to the ceiling, or the ceiling.
fn page_rows_of(asked: Option<u32>, limits: &RecordsLimits) -> u32 {
    asked.unwrap_or(u32::MAX).min(limits.max_page_rows.max(1))
}

/// The refusals a request's own paging arguments decide, before anything is read.
fn refuse_shape(
    page_rows: Option<u32>,
    pages: Option<u32>,
    count: bool,
    cursor: Option<&str>,
) -> Result<()> {
    let refused = |why| Err(EngineError::RecordsRefused(why));
    if page_rows == Some(0) {
        return refused(RecordsRefused::ZeroPageRows);
    }
    if pages == Some(0) {
        return refused(RecordsRefused::ZeroPages);
    }
    if count && cursor.is_some() {
        return refused(RecordsRefused::CountWithCursor);
    }
    Ok(())
}
