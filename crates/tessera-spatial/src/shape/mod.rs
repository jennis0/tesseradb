//! Shapes — box, circle, ellipse and polygon — with one semantics: **the rows whose stored
//! position is inside the shape, exactly** (`polygon-membership.md` §4.1).
//!
//! The module owns the geometry and nothing else: the model, its readers (WKB, WKT), the
//! canonical grid-unit form and its bytes, the descent that turns a shape into interior tiles and
//! boundary cells ([`decompose`]), the point test a boundary cell applies, and the two things the
//! wire needs — a simplification weight per polygon vertex and a densified ring for a curve. It
//! knows the grid (`morton.rs`) and does not know the store, the engine or a principal.
//!
//! **Two questions of a shape, and nothing else.** The descent asks a shape to classify a tile —
//! disjoint, wholly inside, or crossed by the boundary — and to test a point. Every kind answers
//! both through the [`Region`] trait, so the descent is written once and *the same semantics* for
//! four kinds is a construction rather than a promise. A polygon's answers ride on a per-tile
//! context (the edges crossing the tile and the parity of one corner) that the descent refines
//! down the tree; the three closed forms need no context at all.
//!
//! **A shape and the points are placed by one function.** A shape declared in longitude and
//! latitude is put through the *view's own* projection before it is canonicalised ([`project`],
//! [`Space`]), each edge densified first because the space a shape is declared in defines the
//! plane its edges are straight in (`polygon-membership.md` R10). A view that projects nothing
//! has one space and refuses the second.
//!
//! **Exact where it can be, deterministic everywhere.** A polygon is tested in integer arithmetic
//! over the 32-bit grid with one symbolic perturbation rule for ties (`polygon.rs`); a circle or an
//! ellipse is a general conic once the extent's two axes scale differently, so its test is
//! correctly-rounded `f64` — the same licence `artifact-shapes.md` §1 takes for the hull's two
//! floating steps — and two platforms agree because every IEEE-754 operation it uses is correctly
//! rounded.
//!
//! **What is held per artifact, and why.** A polygon's edge table ([`PolygonRegion`]) is built
//! once and held beside the canonical polygon for the artifact's life (`polygon-membership.md`
//! §6.3): it borrows the vertices and adds four bytes per edge, so the descent and every
//! boundary-cell test run over one copy of the coordinates. [`Shape::prepared`] is that: a
//! [`PreparedShape`] answers `contains` and `decompose` without rebuilding anything, and a caller
//! on a hot path holds one. `Shape::contains` and `Shape::decompose` prepare per call and are for
//! the one-off — a test, a check.

mod canon;
mod conic;
mod decompose;
mod encode;
mod polygon;
mod project;
mod simplify;
mod wkb;
mod wkt;

pub use canon::{CanonError, CanonReport, ShapeF64};
pub use conic::Conic;
pub use decompose::{contexts_at, decompose, BoundaryCell, Class, Decomposition, Rect, Region};
pub use encode::DecodeError;
pub use polygon::{Part, PolyCtx, Polygon, PolygonRegion, Ring, Vertex};
pub use project::{Space, DENSIFY_TOLERANCE_CELLS};
pub use wkb::{read_wkb, WkbError};
pub use wkt::{read_wkt, WktError};

/// A position on the 32-bit-per-axis grid — what `fixed32` produces and `unsplit32` recovers.
pub type GridPoint = (u32, u32);

/// An axis-aligned box in grid units, closed on every side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bbox {
    pub min_x: u32,
    pub min_y: u32,
    pub max_x: u32,
    pub max_y: u32,
}

/// A shape in its canonical grid-unit form — what is stored, tested and served.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Bbox(Bbox),
    /// A circle or an ellipse. Both are one conic in grid units, because an extent whose axes
    /// scale differently turns a circle into an ellipse and a rotated ellipse into a differently
    /// rotated one; keeping the caller's five numbers would store a shape the grid does not hold.
    Conic(Conic),
    Polygon(Polygon),
}

impl Shape {
    /// The kind's name, as `disclosure.json` and the reports spell it. A circle and an ellipse are
    /// one conic once quantised (§4.1), so the stored form cannot say which was declared; the
    /// layer's own declaration is where that word lives.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Shape::Bbox(_) => "bbox",
            Shape::Conic(_) => "conic",
            Shape::Polygon(_) => "polygon",
        }
    }

    /// How many parts and rings a polygon carries, `(0, 0)` for a closed form.
    pub fn parts_and_rings(&self) -> (u64, u64) {
        match self {
            Shape::Polygon(p) => (
                p.parts.len() as u64,
                p.parts.iter().map(|part| part.rings.len() as u64).sum(),
            ),
            _ => (0, 0),
        }
    }

    /// How many vertices the shape carries — the quantity `max_shape_vertices` caps. Zero for a
    /// closed form, which has parameters rather than vertices.
    pub fn vertex_count(&self) -> u64 {
        match self {
            Shape::Polygon(p) => p.vertex_count(),
            _ => 0,
        }
    }

    /// The grid-unit box enclosing the shape, `None` for a polygon with no rings.
    pub fn bounds(&self) -> Option<Bbox> {
        match self {
            Shape::Bbox(b) => Some(*b),
            Shape::Conic(c) => Some(c.bounds()),
            Shape::Polygon(p) => p.bounds(),
        }
    }

    /// The shape with its edge table built: what a caller holds for the shape's life.
    pub fn prepared(&self) -> PreparedShape<'_> {
        PreparedShape {
            shape: self,
            region: match self {
                Shape::Polygon(p) => Some(p.region()),
                _ => None,
            },
        }
    }

    /// Decompose against the grid: interior tiles, boundary cells, and — only under a budget —
    /// cover tiles. See [`decompose`]. Prepares per call; hold a [`PreparedShape`] on a hot path.
    pub fn decompose(&self, max_boundary_cells: Option<usize>) -> Decomposition<PolyCtx> {
        self.prepared().decompose(max_boundary_cells)
    }

    /// Whether a point is inside — the direct test, without a decomposition. What every boundary
    /// cell's test agrees with, and what the tests in `tests/shape.rs` hold the descent to.
    /// Prepares per call; hold a [`PreparedShape`] on a hot path.
    pub fn contains(&self, p: GridPoint) -> bool {
        self.prepared().contains(p)
    }

    /// The shape as rings for the wire — parts, then rings, then vertices — at a resolution:
    /// a polygon filtered to the vertices whose weight is at least `min_weight` (grid units, the
    /// side of the cell at the request's depth) and to its `budget` heaviest; a curve densified
    /// so that no chord departs from it by more than `min_weight`; a box as its four corners.
    pub fn rings(&self, min_weight: u32, budget: usize) -> Vec<Vec<Vec<GridPoint>>> {
        self.rings_guarded(min_weight, budget).0
    }

    /// [`Shape::rings`], and whether the `budget` guard fired — a polygon with more vertices above
    /// `min_weight` than the budget, or a curve whose chord tolerance asked for more vertices than
    /// it — so a serve can record that the drawing is coarser than the depth alone would make it
    /// (`polygon-membership.md` §7.2). A box never fires it.
    pub fn rings_guarded(&self, min_weight: u32, budget: usize) -> (Vec<Vec<Vec<GridPoint>>>, bool) {
        match self {
            Shape::Bbox(b) => (
                vec![vec![vec![
                    (b.min_x, b.min_y),
                    (b.max_x, b.min_y),
                    (b.max_x, b.max_y),
                    (b.min_x, b.max_y),
                ]]],
                false,
            ),
            Shape::Conic(c) => {
                let (ring, guarded) = c.ring_guarded(min_weight.max(1), budget);
                (vec![vec![ring]], guarded)
            }
            Shape::Polygon(p) => p.rings_guarded(min_weight, budget),
        }
    }
}

/// A shape and, for a polygon, its edge table — built once, held for the shape's life (§6.3).
#[derive(Debug, Clone)]
pub struct PreparedShape<'a> {
    shape: &'a Shape,
    region: Option<PolygonRegion<'a>>,
}

impl<'a> PreparedShape<'a> {
    pub fn shape(&self) -> &'a Shape {
        self.shape
    }

    /// The polygon's edge table; `None` for a closed form.
    pub fn region(&self) -> Option<&PolygonRegion<'a>> {
        self.region.as_ref()
    }

    /// See [`Shape::decompose`].
    pub fn decompose(&self, max_boundary_cells: Option<usize>) -> Decomposition<PolyCtx> {
        match (self.shape, &self.region) {
            (Shape::Bbox(b), _) => {
                decompose(b, max_boundary_cells).map_ctx(|()| PolyCtx::default())
            }
            (Shape::Conic(c), _) => {
                decompose(c, max_boundary_cells).map_ctx(|()| PolyCtx::default())
            }
            (Shape::Polygon(_), Some(region)) => decompose(region, max_boundary_cells),
            (Shape::Polygon(_), None) => unreachable!("a prepared polygon holds its region"),
        }
    }

    /// See [`Shape::contains`].
    pub fn contains(&self, p: GridPoint) -> bool {
        match (self.shape, &self.region) {
            (Shape::Bbox(b), _) => b.contains(p),
            (Shape::Conic(c), _) => c.contains(p),
            (Shape::Polygon(_), Some(region)) => region.contains_direct(p),
            (Shape::Polygon(_), None) => unreachable!("a prepared polygon holds its region"),
        }
    }

    /// The test a boundary cell applies to a position in it, with the cell's carried context —
    /// for a polygon the cell's edges and corner parity; a closed form ignores the context.
    pub fn contains_in_cell(&self, p: GridPoint, cell: Rect, ctx: &PolyCtx) -> bool {
        match &self.region {
            Some(region) => region.contains(p, cell, ctx),
            None => self.contains(p),
        }
    }
}

impl Bbox {
    pub fn contains(&self, (x, y): GridPoint) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }
}
