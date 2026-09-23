//! `POST /v1/items`: a viewer reads, page by page, every item they may see in one view that
//! matches a filter, with the fields they name. [`Engine::items_stream`] serves one response: a
//! head, then pages, each an Arrow batch with a page end carrying the cursor to resume from, and a
//! trailer it returns.
//!
//! Every page is built from the latest generation with the visible set composed again, exactly as
//! a viewport composes it, so a deletion or suppression accepted during a read applies from the
//! next page. A page takes its rows from that mask and reads fields for those rows alone. Rows are
//! addressed by `tessera_id`; an entity id or any other internal position reaches the caller only
//! inside a sealed cursor.
//!
//! Map order is `(cell, tessera_id)` merged across the view's segments, and stored order is
//! ascending item number. Either resumes from a position that is a value, found again in whatever
//! segments the next page's generation holds, so a flush, merge or fold between pages loses no
//! row and repeats none.

mod columns;
mod cursor;
mod plan;
mod walk;

use std::time::{Duration, Instant};

use arrow::record_batch::RecordBatch;

use crate::cancel::CancelToken;
use crate::engine::Engine;
use crate::error::{EngineError, Result};
use crate::filter::FilterExpr;
use crate::region::RegionVerdict;
use crate::session::Session;
use crate::timing::Probe;
use crate::viewport::{filter_refusal, meta_of, SinkClosed, SinkResult};

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
pub struct ItemsLimits {
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
    pub limits: ItemsLimits,
    /// Cancellation ends the response with a trailer whose `ended_by` is `deadline`, after a
    /// page holding whatever rows the page under way had reached. The walk honours it only once
    /// stopping moves the cursor on, so it may run on for one stretch's filter evaluation and one
    /// chunk of the scan after it. A token cancelled before the response walks its first page
    /// ends the response with no rows, no counts and the cursor it was given, since the client
    /// has gone.
    pub cancel: Option<CancelToken>,
}

/// The visible and matching counts a head carries under `count`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemsCounts {
    /// Visible items in the view: the count the viewport serves.
    pub visible: u64,
    /// Of those, the ones matching the filter; `visible` without one.
    pub matched: u64,
}

/// What precedes a response's pages.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemsHead {
    pub order: RecordsOrder,
    /// The page size used, after the ceiling.
    pub page_rows: u32,
    pub counts: Option<ItemsCounts>,
    /// The coarsest verdict the response's region leaves reached before the head was sent: in
    /// the count's evaluation and the first page's, for the response's header.
    pub region: Option<RegionVerdict>,
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
pub struct ItemsPageEnd {
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
pub struct ItemsTrailer {
    pub pages: u64,
    pub rows: u64,
    /// The cursor to resume from, `None` where no row remains. It can be past the last page end,
    /// where the scan went on without finding a row.
    pub next: Option<String>,
    pub ended_by: ResponseEndedBy,
}

/// Where a response is delivered: the head once, first, then each page with its end. A refusal
/// means the consumer has gone, and ends the response with [`EngineError::Cancelled`].
pub trait ItemsSink {
    fn head(&mut self, head: &ItemsHead) -> SinkResult;
    fn page(&mut self, batch: &RecordBatch, end: &ItemsPageEnd) -> SinkResult;
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
type Counted = (ItemsCounts, Option<RegionVerdict>);

/// Everything a response fixes before its first page: the request, its fields and order, and the
/// binding its cursors are sealed under.
struct Planned<'r> {
    session: &'r Session,
    req: ItemsRequest<'r>,
    plan: FieldPlan,
    order: RecordsOrder,
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
        sink: &mut dyn ItemsSink,
    ) -> Result<ItemsTrailer> {
        let started = Instant::now();
        refuse_shape(&req)?;
        let (planned, walk) = self.plan_items(session, req)?;
        self.serve_pages(&planned, walk, started, sink)
    }

    /// The generation, the idset, the view, the cursor, the fields and the filter, in that order,
    /// then the order, the page size and the counts.
    fn plan_items<'r>(
        &self,
        session: &'r Session,
        req: ItemsRequest<'r>,
    ) -> Result<(Planned<'r>, Walk)> {
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
        let page_rows = req
            .page_rows
            .unwrap_or(u32::MAX)
            .min(req.limits.max_page_rows.max(1));
        let walk = Walk::new(
            req.filter.clone(),
            req.keep_unmatched,
            resumed.map_or(page_rows, |cursor| cursor.stretch),
            req.limits.max_page_bytes,
            resumed.map_or(Position::start(order), |cursor| cursor.position),
        );
        Ok((
            Planned {
                session,
                req,
                plan,
                order,
                page_rows,
                idset,
                binding,
            },
            walk,
        ))
    }

    /// The head, then pages until the response ends, then the trailer. The head follows the first
    /// page's walk, so a failure there is a plain refusal and the head carries the first
    /// stretch's region verdict.
    fn serve_pages(
        &self,
        planned: &Planned<'_>,
        mut walk: Walk,
        started: Instant,
        sink: &mut dyn ItemsSink,
    ) -> Result<ItemsTrailer> {
        let limits = &planned.req.limits;
        let mut clock = Clock::new(started, limits.response_time, planned.req.cancel.clone());
        let cursor_at = |walk: &Walk| {
            let cursor = ItemsCursor {
                idset: planned.idset,
                position: walk.position,
                stretch: walk.target,
            };
            self.cursor_key.seal(&planned.binding, &cursor.encode())
        };
        let mut counted: Option<Counted> = None;
        let mut head_sent = false;
        let mut send_head = |walk: &Walk, counted, sink: &mut dyn ItemsSink| -> Result<()> {
            if !head_sent {
                head_sent = true;
                sink.head(&planned.head(walk, counted))
                    .map_err(|SinkClosed| EngineError::Cancelled)?;
            }
            Ok(())
        };
        let (mut pages, mut rows, mut bytes) = (0u64, 0u64, 0usize);
        let ended_by = loop {
            if pages > 0 {
                if planned.req.pages.is_some_and(|limit| pages >= u64::from(limit)) {
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
            let req = &planned.req;
            let open = self.open_view(
                planned.session,
                &generation,
                req.view,
                &req.cancel,
                &mut Probe::new(),
            )?;
            let cx = PageCx::new(self, &open, &generation);
            if req.count && counted.is_none() {
                counted = Some(self.items_counts(&cx, req)?);
            }
            let page = self.items_page(planned, &cx, &mut walk, &mut clock)?;
            send_head(&walk, counted, sink)?;
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
                    let end = ItemsPageEnd {
                        next: (ended_by != PageEndedBy::End).then(|| cursor_at(&walk)),
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
        send_head(&walk, counted, sink)?;
        Ok(ItemsTrailer {
            pages,
            rows,
            next: (ended_by != ResponseEndedBy::End).then(|| cursor_at(&walk)),
            ended_by,
        })
    }

    /// One page under `cx`: the rows walked from the position, their fields read and the batch cut
    /// to the byte ceiling. A walk stopped for time or cancellation while holding rows makes a
    /// short page of them. The walk's position moves to the page's end, or where the page holds no
    /// row, to the scan position reached.
    fn items_page(
        &self,
        planned: &Planned<'_>,
        cx: &PageCx<'_>,
        walk: &mut Walk,
        clock: &mut Clock,
    ) -> Result<Paged> {
        let req = &planned.req;
        let Collected {
            rows,
            walked,
            position,
        } = walk.collect(cx, planned.page_rows as usize, clock)?;
        if rows.is_empty() {
            walk.position = position;
            return Ok(match walked {
                Walked::Stopped(reason) => Paged::Stopped(reason),
                _ => Paged::End,
            });
        }
        let (batch, kept, bytes) = read_page(
            cx,
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
            walk.position = walk.position_at(cx, &rows[kept - 1]);
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

    /// The head's counts: the view's visible items, and of them the ones the filter matches, by
    /// one evaluation of the filter over the whole view on the route every page takes, under the
    /// first page's mask.
    fn items_counts(
        &self,
        cx: &PageCx<'_>,
        req: &ItemsRequest<'_>,
    ) -> Result<Counted> {
        let open = cx.open;
        let visible = open.mask.visible_total();
        let Some(expr) = &req.filter else {
            return Ok((
                ItemsCounts {
                    visible,
                    matched: visible,
                },
                None,
            ));
        };
        let total = u32::try_from(open.served.data.row_space.total_rows()).unwrap_or(u32::MAX);
        let whole_view = std::iter::once(0..total).collect::<Vec<_>>();
        let routed = filter_rows(cx, expr, None, &whole_view, false, &req.cancel)?;
        let matched = open
            .mask
            .rows_in_range(0..total)
            .and_cardinality(routed.rows.rows());
        Ok((ItemsCounts { visible, matched }, routed.region))
    }
}

/// The refusals a request's own arguments decide, before anything is read.
fn refuse_shape(req: &ItemsRequest<'_>) -> Result<()> {
    let refused = |why| Err(EngineError::RecordsRefused(why));
    if req.page_rows == Some(0) {
        return refused(RecordsRefused::ZeroPageRows);
    }
    if req.pages == Some(0) {
        return refused(RecordsRefused::ZeroPages);
    }
    if req.count && req.cursor.is_some() {
        return refused(RecordsRefused::CountWithCursor);
    }
    Ok(())
}

impl Planned<'_> {
    /// The head, with the counts and the verdict the counting evaluation reached, where the
    /// request asked for counts.
    fn head(&self, walk: &Walk, counted: Option<Counted>) -> ItemsHead {
        ItemsHead {
            order: self.order,
            page_rows: self.page_rows,
            counts: counted.map(|(counts, _)| counts),
            region: RegionVerdict::coarsest(counted.and_then(|(_, region)| region), walk.region),
        }
    }
}
