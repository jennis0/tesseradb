//! The set and the reference set of one page, composed from inside the viewer's visible set.
//!
//! Both are routed through the viewport's filter machinery over the page's mask and one entity
//! candidate, so a region or `member_of` leaf both name is resolved once. A set is held in the
//! form its filter routed to, and the other form is derived only where a count asks for it: the
//! entities of a set routed in entity space are restricted to those with a row in the view, and
//! the rows of one routed in row space are the mask's rows the filter admits.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use croaring::Bitmap;

use super::{AggregateRequest, Reference};
use crate::cancel::CancelToken;
use crate::cells::CellSet;
use crate::compose::{MaskedSet, WholeMask};
use crate::engine::Engine;
use crate::error::{EngineError, Result};
use crate::filter::RoutedFilter;
use crate::region::RegionVerdict;
use crate::viewport::{OpenView, ResolvedLeaves};
use crate::Generation;

/// The page's set and reference set, and the entity candidate both were routed under.
pub(super) struct Sets {
    pub(super) set: Set,
    pub(super) reference: Option<Set>,
    /// Every entity this viewer may see, in every view: what a `derived` value's visibility is
    /// derived from.
    pub(super) candidate: Bitmap,
}

impl Sets {
    /// The coarsest verdict either set's region leaves reached.
    pub(super) fn region(&self) -> Option<RegionVerdict> {
        RegionVerdict::coarsest(
            self.set.region,
            self.reference.as_ref().and_then(|r| r.region),
        )
    }
}

/// One composed set, in the view.
pub(super) struct Set {
    held: Held,
    size: u64,
    region: Option<RegionVerdict>,
    rows: OnceLock<Bitmap>,
    entities: OnceLock<Bitmap>,
    /// Entities read through rows to find this set's entities.
    crossed: AtomicU64,
}

enum Held {
    /// Every item this viewer may see in the view: the mask itself.
    Whole,
    /// A filter answered in entity space, restricted to the entities holding a row in the view.
    Entities(Bitmap),
    /// A filter answered in row space, under the mask.
    Rows(Bitmap),
}

impl Set {
    fn of(held: Held, size: u64, region: Option<RegionVerdict>) -> Set {
        Set {
            held,
            size,
            region,
            rows: OnceLock::new(),
            entities: OnceLock::new(),
            crossed: AtomicU64::new(0),
        }
    }

    /// How many items the set holds.
    pub(super) fn size(&self) -> u64 {
        self.size
    }

    /// Whether the set's entities are at hand without reading rows.
    pub(super) fn has_entities(&self) -> bool {
        !matches!(self.held, Held::Rows(_))
    }

    /// Entities read through rows so far.
    pub(super) fn crossed(&self) -> u64 {
        self.crossed.load(Ordering::Relaxed)
    }

    /// The set's rows for a cell count.
    pub(super) fn cells<'s>(&'s self, cx: &'s Cx<'_>) -> CellSet<'s> {
        match &self.held {
            Held::Whole => CellSet::Mask(&cx.open.mask),
            _ => CellSet::Rows(self.rows(cx)),
        }
    }

    /// The set's rows in the view.
    pub(super) fn rows<'s>(&'s self, cx: &'s Cx<'_>) -> &'s Bitmap {
        match &self.held {
            Held::Whole => cx.open.mask.visible_all(),
            Held::Rows(rows) => rows,
            Held::Entities(entities) => self.rows.get_or_init(|| {
                let projected = cx.open.served.data.row_space.project(entities);
                cx.open.mask.visible_rows(&projected)
            }),
        }
    }

    /// The set's entities, each holding a row in the view.
    pub(super) fn entities<'s>(&'s self, cx: &'s Cx<'_>) -> Result<&'s Bitmap> {
        match &self.held {
            Held::Entities(entities) => Ok(entities),
            Held::Whole => {
                if let Some(held) = self.entities.get() {
                    return Ok(held);
                }
                let restricted = restrict(cx, &cx.sets.candidate)?;
                Ok(self.entities.get_or_init(|| restricted))
            }
            Held::Rows(rows) => {
                if let Some(held) = self.entities.get() {
                    return Ok(held);
                }
                let crossed = cross(cx, rows)?;
                self.crossed
                    .fetch_add(rows.cardinality(), Ordering::Relaxed);
                Ok(self.entities.get_or_init(|| crossed))
            }
        }
    }
}

/// The members of `entities` holding a row in the view's rows the page's mask was projected over.
fn restrict_under(open: &OpenView<'_>, engine: &Engine, entities: &Bitmap) -> Result<Bitmap> {
    let row_space = &open.served.data.row_space;
    match row_space.restrict_to_view(entities, open.geometry.projection.covers_through()) {
        Some(restricted) => Ok(restricted),
        // A merge has collapsed the extent the projection was taken through, so which of the
        // extents' entities its rows cover is read from the mask's rows above the base.
        None => {
            let mut held = row_space
                .restrict_to_view(entities, None)
                .expect("the base alone is always covered");
            let above_base = open.mask.visible_all().and(&Bitmap::from_range(
                row_space.base_rows()..u32::try_from(row_space.total_rows()).unwrap_or(u32::MAX),
            ));
            let mut extents = crossing(engine, open, &above_base)?;
            extents.and_inplace(entities);
            held.or_inplace(&extents);
            Ok(held)
        }
    }
}

fn restrict(cx: &Cx<'_>, entities: &Bitmap) -> Result<Bitmap> {
    restrict_under(cx.open, cx.engine, entities)
}

fn cross(cx: &Cx<'_>, rows: &Bitmap) -> Result<Bitmap> {
    crossing(cx.engine, cx.open, rows)
}

/// The entities of `rows`, on the engine's pool.
fn crossing(engine: &Engine, open: &OpenView<'_>, rows: &Bitmap) -> Result<Bitmap> {
    let row_space = &open.served.data.row_space;
    engine
        .pool
        .install(|| row_space.entities_of_rows(rows))
        .ok_or_else(|| {
            EngineError::Malformed(format!(
                "view '{}' has no row-to-entity table, so a set routed by its rows cannot be \
                 counted by entity; rebuild the bundle",
                open.served.name
            ))
        })
}

/// Compose the page's set and reference set under `open`'s mask.
pub(super) fn compose(
    engine: &Engine,
    open: &OpenView<'_>,
    generation: &Arc<Generation>,
    req: &AggregateRequest<'_>,
    prefer_row: bool,
) -> Result<Sets> {
    check_cancelled(&req.cancel)?;
    let served = &open.served;
    let candidate = engine.filter_candidate(served.session, generation)?;
    let resolved = ResolvedLeaves::default();
    let whole = || Set::of(Held::Whole, open.mask.visible_total(), None);
    let total = served.data.row_space.total_rows();
    let whole_view = 0..u32::try_from(total).unwrap_or(u32::MAX);
    let domain = std::slice::from_ref(&whole_view);
    let (set, reference) = engine.route_filters_under(
        served,
        &open.mask,
        &candidate,
        &resolved,
        &req.cancel,
        |route| {
            let routed = |expr: &crate::filter::FilterExpr| -> Result<Set> {
                check_cancelled(&req.cancel)?;
                Ok(match route(expr, prefer_row)? {
                    RoutedFilter::Entity(entities) => {
                        let restricted = restrict_under(open, engine, &entities)?;
                        let size = restricted.cardinality();
                        Set::of(Held::Entities(restricted), size, None)
                    }
                    RoutedFilter::Row(tree) => {
                        let region = tree.region_verdict();
                        let matched =
                            engine.evaluate_row_route(&tree, served, domain, total, false)?;
                        let rows = open.mask.visible_rows(matched.rows());
                        let size = rows.cardinality();
                        Set::of(Held::Rows(rows), size, region)
                    }
                })
            };
            let set = match &req.filter {
                None => whole(),
                Some(expr) => routed(expr)?,
            };
            let reference = match &req.reference {
                None => None,
                Some(Reference::Visible) => Some(whole()),
                Some(Reference::Filter(expr)) => Some(routed(expr)?),
            };
            Ok((set, reference))
        },
    )?;
    Ok(Sets {
        set,
        reference,
        candidate,
    })
}

fn check_cancelled(cancel: &Option<CancelToken>) -> Result<()> {
    if cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
        return Err(EngineError::Cancelled);
    }
    Ok(())
}

/// What one page counts under.
pub(super) struct Cx<'a> {
    pub(super) engine: &'a Engine,
    pub(super) open: &'a OpenView<'a>,
    pub(super) generation: &'a Arc<Generation>,
    pub(super) sets: &'a Sets,
    pub(super) cancel: &'a Option<CancelToken>,
}

impl<'a> Cx<'a> {
    pub(super) fn new(
        engine: &'a Engine,
        open: &'a OpenView<'a>,
        generation: &'a Arc<Generation>,
        sets: &'a Sets,
        cancel: &'a Option<CancelToken>,
    ) -> Cx<'a> {
        Cx {
            engine,
            open,
            generation,
            sets,
            cancel,
        }
    }

    /// Every segment of the view, with its first row.
    pub(super) fn segments(&self) -> &[(&'a tessera_store::read::SegmentData, u32)] {
        &self.open.served.segments
    }

    /// How many rows the view holds.
    pub(super) fn view_rows(&self) -> u64 {
        self.open.served.data.row_space.total_rows()
    }

    pub(super) fn check_cancelled(&self) -> Result<()> {
        check_cancelled(self.cancel)
    }
}
