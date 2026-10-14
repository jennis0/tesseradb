//! The set and the reference set of one page, composed from inside the viewer's visible set.
//!
//! Both are routed through the viewport's filter machinery over the page's mask, in each shard the
//! page reads under that shard's entity candidate, so a region or `member_of` leaf both name is
//! resolved once. A set's part in a shard is held in the form its filter routed to there, and the
//! other form is derived only where a count asks for it: the entities of a part routed in entity
//! space are restricted to those with a row in the view, and the rows of one routed in row space
//! are the leaf's rows the filter admits.
//! A shard's items are its own, so a set's size is the sum of its parts'.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use croaring::Bitmap;

use super::{AggregateRequest, Reference};
use crate::cancel::CancelToken;
use crate::cells::CellSet;
use crate::compose::{EffectiveMask, MaskedSet, WholeMask};
use crate::engine::Engine;
use crate::error::{EngineError, Result};
use crate::filter::{FilterExpr, RoutedFilter};
use crate::region::RegionVerdict;
use crate::viewport::{OpenView, ResolvedLeaves, ServedSide};
use crate::Generation;

/// The page's set and reference set, and the entity candidates both were routed under.
pub(super) struct Sets {
    pub(super) set: Set,
    pub(super) reference: Option<Set>,
    reads: Vec<usize>,
    /// By place, for each side the page reads, every entity this viewer may see in its shard, in
    /// every view: what a `derived` value's visibility is derived from.
    candidates: Vec<Option<Bitmap>>,
}

impl Sets {
    /// The coarsest verdict either set's region leaves reached.
    pub(super) fn region(&self) -> Option<RegionVerdict> {
        RegionVerdict::coarsest(
            self.set.region,
            self.reference.as_ref().and_then(|r| r.region),
        )
    }

    /// The candidate of the side at `place`, or `None` where the page does not read it.
    pub(super) fn candidate(&self, place: usize) -> Option<&Bitmap> {
        self.candidates[place].as_ref()
    }
}

/// One composed set, in the view.
pub(super) struct Set {
    /// By place; `None` for a side the page does not read.
    parts: Vec<Option<Part>>,
    whole: bool,
    size: u64,
    region: Option<RegionVerdict>,
}

/// A set's items in one shard.
struct Part {
    held: Held,
    size: u64,
    rows: OnceLock<Bitmap>,
    entities: OnceLock<Bitmap>,
    /// Entities read through rows to find this part's entities.
    crossed: AtomicU64,
}

enum Held {
    /// Every item this viewer may see in the view: the leaf itself.
    Whole,
    /// A filter answered in entity space, restricted to the entities holding a row in the view.
    Entities(Bitmap),
    /// A filter answered in row space, under the leaf.
    Rows(Bitmap),
}

impl Part {
    fn of(held: Held, size: u64) -> Part {
        Part {
            held,
            size,
            rows: OnceLock::new(),
            entities: OnceLock::new(),
            crossed: AtomicU64::new(0),
        }
    }
}

impl Set {
    /// Every item this viewer may see in `open`'s view, held in each side of `reads`.
    fn whole(open: &OpenView<'_>, reads: &[usize]) -> Set {
        let mut parts: Vec<Option<Part>> = open.served.sides.iter().map(|_| None).collect();
        for &place in reads {
            let size = open
                .served
                .leaf(&open.mask, place)
                .expect("a side the page reads")
                .visible_total();
            parts[place] = Some(Part::of(Held::Whole, size));
        }
        Set {
            parts,
            whole: true,
            size: open.mask.visible_total(),
            region: None,
        }
    }

    /// A filtered set over `sides` sides, holding nothing until its parts are added.
    fn filtered(sides: usize) -> Set {
        Set {
            parts: (0..sides).map(|_| None).collect(),
            whole: false,
            size: 0,
            region: None,
        }
    }

    fn add(&mut self, place: usize, (part, region): Routed) {
        self.size += part.size;
        self.parts[place] = Some(part);
        self.region = RegionVerdict::coarsest(self.region, region);
    }

    /// Whether this is every item this viewer may see in the view.
    pub(super) fn is_whole(&self) -> bool {
        self.whole
    }

    /// How many items the set holds, in every shard.
    pub(super) fn size(&self) -> u64 {
        self.size
    }

    /// How many items the set holds in the side at `place`.
    pub(super) fn size_in(&self, place: usize) -> u64 {
        self.part(place).size
    }

    fn part(&self, place: usize) -> &Part {
        self.parts[place].as_ref().expect("a side the page reads")
    }

    /// Whether the set's entities in the side at `place` are at hand without reading rows.
    pub(super) fn has_entities(&self, place: usize) -> bool {
        !matches!(self.part(place).held, Held::Rows(_))
    }

    /// Entities read through rows so far, in every shard.
    pub(super) fn crossed(&self) -> u64 {
        self.parts
            .iter()
            .flatten()
            .map(|part| part.crossed.load(Ordering::Relaxed))
            .sum()
    }

    /// The set's rows in the side at `place`, for a cell count.
    pub(super) fn cells<'s>(&'s self, cx: &'s Cx<'_>, place: usize) -> CellSet<'s> {
        match &self.part(place).held {
            Held::Whole => CellSet::Mask(cx.leaf(place)),
            _ => CellSet::Rows(self.rows(cx, place)),
        }
    }

    /// The set's rows in the view in the side at `place`.
    pub(super) fn rows<'s>(&'s self, cx: &'s Cx<'_>, place: usize) -> &'s Bitmap {
        let part = self.part(place);
        match &part.held {
            Held::Whole => cx.leaf(place).visible_all(),
            Held::Rows(rows) => rows,
            Held::Entities(entities) => part.rows.get_or_init(|| {
                let projected = cx.side(place).data.row_space.project(entities);
                cx.leaf(place).visible_rows(&projected)
            }),
        }
    }

    /// The set's entities in the side at `place`, each holding a row in the view.
    pub(super) fn entities<'s>(&'s self, cx: &'s Cx<'_>, place: usize) -> Result<&'s Bitmap> {
        let part = self.part(place);
        match &part.held {
            Held::Entities(entities) => Ok(entities),
            Held::Whole => {
                if let Some(held) = part.entities.get() {
                    return Ok(held);
                }
                let candidate = cx.sets.candidate(place).expect("a side the page reads");
                let restricted = in_view(cx.open, place, candidate)?;
                Ok(part.entities.get_or_init(|| restricted))
            }
            Held::Rows(rows) => {
                if let Some(held) = part.entities.get() {
                    return Ok(held);
                }
                let crossed = crossing(cx.engine, cx.open, place, rows)?;
                part.crossed
                    .fetch_add(rows.cardinality(), Ordering::Relaxed);
                Ok(part.entities.get_or_init(|| crossed))
            }
        }
    }
}

/// A set's part in one side, and the verdict its region leaves reached there.
type Routed = (Part, Option<RegionVerdict>);

/// The members of `entities`, of the side at `place`'s shard, holding a row in the view's rows
/// the page's mask was projected over there.
pub(crate) fn in_view(open: &OpenView<'_>, place: usize, entities: &Bitmap) -> Result<Bitmap> {
    // The projection covers an extent of this generation's row space: a projection served from
    // the generation before is served only where this one extends it, which a merge does not.
    let side = &open.served.sides[place];
    side.data
        .row_space
        .restrict_to_view(entities, side.geometry.projection.covers_through())
        .ok_or_else(|| {
            EngineError::Malformed(format!(
                "view '{}' no longer holds the extent its projection was taken through",
                open.served.name
            ))
        })
}

/// The entities of `rows` in the side at `place`, on the engine's pool.
pub(super) fn crossing(
    engine: &Engine,
    open: &OpenView<'_>,
    place: usize,
    rows: &Bitmap,
) -> Result<Bitmap> {
    let row_space = &open.served.sides[place].data.row_space;
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

/// Compose the page's set and reference set under `open`'s mask, in each side it reads.
pub(super) fn compose(
    engine: &Engine,
    open: &OpenView<'_>,
    req: &AggregateRequest<'_>,
    prefer_row: bool,
) -> Result<Sets> {
    check_cancelled(&req.cancel)?;
    let served = &open.served;
    let sides = served.sides.len();
    let reads: Vec<usize> = served
        .filtered_sides(&open.mask)
        .map(|(place, ..)| place)
        .collect();
    let reference_expr = match &req.reference {
        Some(Reference::Filter(expr)) => Some(expr),
        _ => None,
    };
    let resolved = ResolvedLeaves::new(&open.mask);
    let mut candidates: Vec<Option<Bitmap>> = vec![None; sides];
    let (mut set, mut reference) = (Set::filtered(sides), Set::filtered(sides));
    for &place in &reads {
        let side = &served.sides[place];
        let candidate =
            engine.filter_candidate(served.session, side.shard, Some(&side.geometry.fragment))?;
        engine.route_filters_under(served, side, &candidate, &resolved, &req.cancel, |route| {
            let route_one = |expr: &FilterExpr, into: &mut Set| -> Result<()> {
                check_cancelled(&req.cancel)?;
                into.add(
                    place,
                    routed(engine, open, place, route(expr, prefer_row)?)?,
                );
                Ok(())
            };
            if let Some(expr) = &req.filter {
                route_one(expr, &mut set)?;
            }
            if let Some(expr) = reference_expr {
                route_one(expr, &mut reference)?;
            }
            Ok(())
        })?;
        candidates[place] = Some(candidate);
    }
    let set = match &req.filter {
        None => Set::whole(open, &reads),
        Some(_) => set,
    };
    let reference = match &req.reference {
        None => None,
        Some(Reference::Visible) => Some(Set::whole(open, &reads)),
        Some(Reference::Filter(_)) => Some(reference),
    };
    Ok(Sets {
        set,
        reference,
        reads,
        candidates,
    })
}

/// One routed filter as a set's part in the side at `place`: an entity-space answer kept to
/// the entities the leaf's rows cover, a row-space one taken under the leaf over the whole view.
fn routed(
    engine: &Engine,
    open: &OpenView<'_>,
    place: usize,
    routed: RoutedFilter,
) -> Result<Routed> {
    Ok(match routed {
        RoutedFilter::Entity(entities) => {
            let restricted = in_view(open, place, &entities)?;
            let size = restricted.cardinality();
            (Part::of(Held::Entities(restricted), size), None)
        }
        RoutedFilter::Row(tree) => {
            let side = &open.served.sides[place];
            let region = tree.region_verdict();
            let total = side.data.row_space.total_rows();
            let whole_view = 0..u32::try_from(total).unwrap_or(u32::MAX);
            let matched = engine.evaluate_row_route(
                &tree,
                side,
                std::slice::from_ref(&whole_view),
                total,
                false,
            )?;
            let rows = open
                .served
                .leaf(&open.mask, place)
                .expect("a side the page reads")
                .visible_rows(matched.rows());
            let size = rows.cardinality();
            (Part::of(Held::Rows(rows), size), region)
        }
    })
}

/// The members of `candidate`, of the side at `place`'s shard, holding a row in `open`'s view
/// that pass `expr`, and the coarsest verdict its region leaves reached: one filtered set's part,
/// composed as [`compose`] composes a page's, for a caller counting by entity alone. `resolved`
/// is the request's, under `open`'s mask.
pub(crate) fn entities_passing(
    engine: &Engine,
    open: &OpenView<'_>,
    place: usize,
    candidate: &Bitmap,
    expr: &FilterExpr,
    resolved: &ResolvedLeaves<'_>,
    cancel: &Option<CancelToken>,
) -> Result<(Bitmap, Option<RegionVerdict>)> {
    check_cancelled(cancel)?;
    let served = &open.served;
    let side = &served.sides[place];
    engine.route_filters_under(served, side, candidate, resolved, cancel, |route| {
        let (part, region) = routed(engine, open, place, route(expr, false)?)?;
        check_cancelled(cancel)?;
        let entities = match part.held {
            Held::Entities(entities) => entities,
            Held::Rows(rows) => crossing(engine, open, place, &rows)?,
            Held::Whole => unreachable!("a routed filter is never the whole view"),
        };
        Ok((entities, region))
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

    /// The places of the sides the page reads, ascending.
    pub(super) fn reads(&self) -> impl Iterator<Item = usize> + 'a {
        self.sets.reads.iter().copied()
    }

    pub(super) fn side(&self, place: usize) -> &'a ServedSide<'a> {
        &self.open.served.sides[place]
    }

    /// The viewer's leaf in the side at `place`, which the page reads.
    pub(super) fn leaf(&self, place: usize) -> &'a EffectiveMask {
        self.open
            .served
            .leaf(&self.open.mask, place)
            .expect("a side the page reads")
    }

    /// Every segment of the view in the side at `place`, with its first row.
    pub(super) fn segments(
        &self,
        place: usize,
    ) -> &'a [(&'a mosaica_store::read::SegmentData, u32)] {
        &self.side(place).segments
    }

    /// How many rows the view holds in the sides the page reads.
    pub(super) fn view_rows(&self) -> u64 {
        self.reads()
            .map(|place| self.side(place).data.row_space.total_rows())
            .sum()
    }

    pub(super) fn check_cancelled(&self) -> Result<()> {
        check_cancelled(self.cancel)
    }
}
