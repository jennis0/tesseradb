//! Shapes: box, circle, ellipse and polygon, with one semantics: the rows whose stored position
//! is inside the shape, exactly.
//!
//! The module owns the geometry and nothing else: the model, its readers (WKB, WKT), the
//! canonical grid-unit form and its bytes, the descent that turns a shape into interior tiles and
//! boundary cells ([`decompose`]), the point test a boundary cell applies, and the two things the
//! wire needs, a simplification weight per polygon vertex and a densified ring for a curve.
//!
//! The descent asks a shape two questions through the [`Region`] trait: classify a tile as
//! disjoint, wholly inside, or crossed by the boundary, and test a point. A polygon's answers
//! ride on a per-tile context (the edges crossing the tile and the parity of one corner) that the
//! descent refines down the tree; the three closed forms need no context at all.
//!
//! A polygon's edge table ([`PolygonRegion`]) is built once and held beside the canonical polygon
//! for the artifact's life. [`Shape::prepared`] is that: a [`PreparedShape`] answers `contains`
//! and `decompose` without rebuilding anything, and a caller on a hot path holds one.
//! `Shape::contains` and `Shape::decompose` prepare per call and are for the one-off.

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

/// A position on the 32-bit-per-axis grid: what `fixed32` produces and `unsplit32` recovers.
pub type GridPoint = (u32, u32);

/// An axis-aligned box in grid units, closed on every side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bbox {
    pub min_x: u32,
    pub min_y: u32,
    pub max_x: u32,
    pub max_y: u32,
}

/// A shape in its canonical grid-unit form: what is stored, tested and served.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Bbox(Bbox),
    /// A circle or an ellipse: both are one conic once the extent's axes scale differently.
    Conic(Conic),
    Polygon(Polygon),
}

impl Shape {
    /// The kind's name, as `disclosure.json` and the reports spell it. The stored form cannot say
    /// whether a conic was declared as a circle or an ellipse; the layer's declaration has that.
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

    /// How many vertices the shape carries, the quantity `max_shape_vertices` caps; zero for a
    /// closed form.
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

    /// See [`decompose`]. Prepares per call; hold a [`PreparedShape`] on a hot path.
    pub fn decompose(&self, max_boundary_cells: Option<usize>) -> Decomposition<PolyCtx> {
        self.prepared().decompose(max_boundary_cells)
    }

    /// The direct test, without a decomposition. Prepares per call; hold a [`PreparedShape`].
    pub fn contains(&self, p: GridPoint) -> bool {
        self.prepared().contains(p)
    }

    /// The shape as rings for the wire at a resolution: a polygon filtered to the vertices whose
    /// weight is at least `min_weight` and to its `budget` heaviest; a curve densified so that no
    /// chord departs from it by more than `min_weight`; a box as its four corners.
    pub fn rings(&self, min_weight: u32, budget: usize) -> Vec<Vec<Vec<GridPoint>>> {
        self.rings_guarded(min_weight, budget).0
    }

    /// [`Shape::rings`], and whether the `budget` guard fired: a polygon with more vertices above
    /// `min_weight` than the budget, or a curve whose chord tolerance asked for more than it. A
    /// box never fires it.
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

/// A shape and, for a polygon, its edge table, built once and held for the shape's life.
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

    /// The test a boundary cell applies, with the cell's carried context: for a polygon the
    /// cell's edges and corner parity; a closed form ignores it.
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
