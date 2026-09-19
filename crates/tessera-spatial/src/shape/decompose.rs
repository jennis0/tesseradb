//! The descent: a shape against the Morton grid, into interior tiles and boundary cells
//! (`selection-operand.md` §3, which is normative for it).
//!
//! From the root, a tile disjoint from the shape is discarded whole; a tile wholly inside is an
//! **interior tile** and is never opened; a tile the boundary crosses is refined into its four
//! children, down to depth 16, where a crossing tile is a **boundary cell** whose rows are tested
//! one by one. The number of crossing tiles at depth *d* grows with the shape's perimeter rather
//! than its area, so the whole descent — and the interior it emits — is linear in the perimeter.
//!
//! **The descent is breadth-first, and that is what the budget needs.** Under `max_boundary_cells`
//! the descent stops at the deepest depth whose crossing-tile count fits, and the crossing tiles
//! at that depth are emitted as **cover** — a superset of the shape, marked as such by the caller
//! (selection-operand §6). Breadth-first is what makes "the deepest depth that fits" a single
//! depth rather than a ragged frontier, and it bounds memory to one level of crossing tiles.
//! A published shape passes no budget and the descent always reaches the grid.

use tessera_types::MortonCode;

use crate::morton::{compact, Tile};

use super::GridPoint;

/// A closed rectangle of grid positions: `x0..=x1` by `y0..=y1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl Rect {
    /// The whole grid.
    pub const ALL: Rect = Rect {
        x0: 0,
        y0: 0,
        x1: u32::MAX,
        y1: u32::MAX,
    };

    /// The grid positions a tile covers. At depth 16 this is one cell — `1 << 16` positions on
    /// each axis, which is what the residual addresses.
    pub fn of_tile(tile: &Tile) -> Rect {
        debug_assert!(tile.depth <= 16);
        if tile.depth == 0 {
            return Rect::ALL;
        }
        let prefix = tile.prefix as u32;
        let tx = compact(prefix);
        let ty = compact(prefix >> 1);
        // Each axis has 16 cell bits and 16 residual bits; a depth-d tile fixes d of them.
        let side = 1u64 << (32 - u32::from(tile.depth));
        let x0 = (u64::from(tx) * side) as u32;
        let y0 = (u64::from(ty) * side) as u32;
        Rect {
            x0,
            y0,
            x1: (u64::from(x0) + side - 1) as u32,
            y1: (u64::from(y0) + side - 1) as u32,
        }
    }

    /// The cell at depth 16 whose code this is.
    pub fn of_cell(cell: MortonCode) -> Rect {
        Rect::of_tile(&Tile {
            prefix: u64::from(cell.raw()),
            depth: 16,
        })
    }

    pub fn contains(&self, (x, y): GridPoint) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }

    /// Whether this rectangle lies wholly inside `outer`.
    pub fn within(&self, outer: &Rect) -> bool {
        self.x0 >= outer.x0 && self.x1 <= outer.x1 && self.y0 >= outer.y0 && self.y1 <= outer.y1
    }

    pub fn disjoint(&self, other: &Rect) -> bool {
        self.x1 < other.x0 || other.x1 < self.x0 || self.y1 < other.y0 || other.y1 < self.y0
    }

    /// The four corners.
    pub fn corners(&self) -> [GridPoint; 4] {
        [
            (self.x0, self.y0),
            (self.x1, self.y0),
            (self.x0, self.y1),
            (self.x1, self.y1),
        ]
    }
}

/// How a tile stands to a shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// No position in the tile is inside.
    Disjoint,
    /// Every position in the tile is inside.
    Inside,
    /// The boundary passes through it — or the shape cannot cheaply tell, which is answered the
    /// same way: refine, and at the grid, test the rows. A conservative `Crossed` is never wrong.
    Crossed,
}

/// The two questions the descent asks of a shape, plus the context it may carry per tile.
///
/// `Ctx` is what a tile needs beyond the shape itself to answer cheaply: a polygon carries the
/// edges that cross the tile and the parity of its lower corner, refined from the parent's; the
/// closed forms carry nothing. The descent never inspects it.
pub trait Region {
    type Ctx: Clone;

    /// The context at the root tile.
    fn root(&self) -> Self::Ctx;

    /// The context of a child tile `to`, derived from its parent's context over `from`. The
    /// child lies within the parent.
    fn refine(&self, parent: &Self::Ctx, from: Rect, to: Rect) -> Self::Ctx;

    fn classify(&self, rect: Rect, ctx: &Self::Ctx) -> Class;

    /// Whether `p`, a position inside `rect`, is inside the shape.
    fn contains(&self, p: GridPoint, rect: Rect, ctx: &Self::Ctx) -> bool;
}

/// A depth-16 cell the boundary crosses, with what its rows are tested against.
#[derive(Debug, Clone)]
pub struct BoundaryCell<C> {
    pub cell: MortonCode,
    pub ctx: C,
}

/// The descent's output.
#[derive(Debug, Clone)]
pub struct Decomposition<C> {
    /// Tiles wholly inside, at whatever depth the descent found them. Every position in each is
    /// inside the shape, so a tile is a whole row range and never opened.
    pub interior: Vec<Tile>,
    /// Depth-16 cells the boundary crosses.
    pub boundary: Vec<BoundaryCell<C>>,
    /// Non-empty only when a budget stopped the descent: the crossing tiles at the depth it
    /// stopped, every one taken as inside. The answer is then exact for a **cover** of the shape.
    pub cover: Vec<Tile>,
    /// The depth the cover was taken at, when there is one.
    pub cover_depth: Option<u8>,
}

impl<C> Decomposition<C> {
    pub fn is_cover(&self) -> bool {
        self.cover_depth.is_some()
    }

    pub(crate) fn map_ctx<D>(self, f: impl Fn(C) -> D) -> Decomposition<D> {
        Decomposition {
            interior: self.interior,
            boundary: self
                .boundary
                .into_iter()
                .map(|b| BoundaryCell {
                    cell: b.cell,
                    ctx: f(b.ctx),
                })
                .collect(),
            cover: self.cover,
            cover_depth: self.cover_depth,
        }
    }
}

/// Decompose a shape against the grid.
///
/// `max_boundary_cells` bounds the number of crossing tiles held at one depth; when refining the
/// next depth would exceed it, the current depth's crossing tiles become the cover. `None` is
/// unbounded and always reaches depth 16.
pub fn decompose<R: Region>(shape: &R, max_boundary_cells: Option<usize>) -> Decomposition<R::Ctx> {
    let mut out = Decomposition {
        interior: Vec::new(),
        boundary: Vec::new(),
        cover: Vec::new(),
        cover_depth: None,
    };
    let root = Tile {
        prefix: 0,
        depth: 0,
    };
    // The crossing tiles at the current depth, with their contexts.
    let mut pending: Vec<(Tile, R::Ctx)> = Vec::new();
    match classify_into(shape, root, shape.root(), &mut out.interior) {
        Some(p) => pending.push(p),
        None => return out,
    }
    while !pending.is_empty() {
        let depth = pending[0].0.depth;
        if depth == 16 {
            for (tile, ctx) in pending.drain(..) {
                out.boundary.push(BoundaryCell {
                    cell: MortonCode::new(tile.prefix as u32),
                    ctx,
                });
            }
            break;
        }
        // Classify the children into a staging area first: under a budget the whole level is
        // accepted or rejected together, and a rejected level's interior tiles must not leak into
        // the output beside a cover of their parents.
        let mut staged: Vec<Tile> = Vec::new();
        let mut next: Vec<(Tile, R::Ctx)> = Vec::new();
        'level: for (tile, ctx) in &pending {
            let from = Rect::of_tile(tile);
            for k in 0..4u64 {
                let child = Tile {
                    prefix: (tile.prefix << 2) | k,
                    depth: tile.depth + 1,
                };
                let to = Rect::of_tile(&child);
                let child_ctx = shape.refine(ctx, from, to);
                if let Some(p) = classify_into(shape, child, child_ctx, &mut staged) {
                    next.push(p);
                    if max_boundary_cells.is_some_and(|b| next.len() > b) {
                        break 'level;
                    }
                }
            }
        }
        if max_boundary_cells.is_some_and(|b| next.len() > b) {
            out.cover_depth = Some(depth);
            out.cover = pending.into_iter().map(|(t, _)| t).collect();
            return out;
        }
        out.interior.extend(staged);
        pending = next;
    }
    out
}

/// **The context of each named cell, re-derived by a descent that opens only their ancestors.**
///
/// A held decomposition keeps a boundary cell as its code and parity alone
/// (`polygon-membership.md` §6.3): the per-cell edge list is the structure that dominated memory at
/// world scale, so it is not held but re-derived when a segment first puts rows in the cell. This
/// is that derivation, shared across the cells of one shape: from the root, a tile is refined only
/// if some named cell lies under it, so a segment touching *k* of a polygon's cells pays the
/// ancestors of those *k* cells rather than a walk of every edge per cell. `cells` is ascending;
/// the result is parallel to it.
///
/// No tile is classified on the way down, because a named cell is by construction one the
/// boundary crosses and every ancestor of a crossed tile is crossed. A cell that is *not* a
/// boundary cell still gets a well-formed context — its inherited edges and parity — and
/// [`Region::contains`] answers correctly over it, so a caller passing an interior cell by mistake
/// gets a slower right answer rather than a wrong one.
pub fn contexts_at<R: Region>(shape: &R, cells: &[MortonCode]) -> Vec<R::Ctx> {
    let mut out = Vec::with_capacity(cells.len());
    if cells.is_empty() {
        return out;
    }
    debug_assert!(cells.windows(2).all(|w| w[0].raw() < w[1].raw()));
    let root = Tile {
        prefix: 0,
        depth: 0,
    };
    descend_to(shape, root, shape.root(), cells, &mut out);
    out
}

fn descend_to<R: Region>(
    shape: &R,
    tile: Tile,
    ctx: R::Ctx,
    cells: &[MortonCode],
    out: &mut Vec<R::Ctx>,
) {
    if tile.depth == 16 {
        debug_assert_eq!(cells.len(), 1);
        out.push(ctx);
        return;
    }
    let from = Rect::of_tile(&tile);
    for k in 0..4u64 {
        let child = Tile {
            prefix: (tile.prefix << 2) | k,
            depth: tile.depth + 1,
        };
        let (lo, hi) = child.code_range();
        let start = cells.partition_point(|c| u64::from(c.raw()) < lo);
        let end = cells.partition_point(|c| u64::from(c.raw()) < hi);
        if start == end {
            continue;
        }
        let to = Rect::of_tile(&child);
        let child_ctx = shape.refine(&ctx, from, to);
        descend_to(shape, child, child_ctx, &cells[start..end], out);
    }
}

/// Classify one tile, record it if settled, and hand it back if it needs refining.
fn classify_into<R: Region>(
    shape: &R,
    tile: Tile,
    ctx: R::Ctx,
    interior: &mut Vec<Tile>,
) -> Option<(Tile, R::Ctx)> {
    match shape.classify(Rect::of_tile(&tile), &ctx) {
        Class::Disjoint => None,
        Class::Inside => {
            interior.push(tile);
            None
        }
        Class::Crossed => Some((tile, ctx)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::morton::interleave_bits;

    #[test]
    fn a_tile_rect_is_its_prefix_range_on_both_axes() {
        let t = Tile {
            prefix: interleave_bits(5, 3, 4),
            depth: 4,
        };
        let r = Rect::of_tile(&t);
        assert_eq!(r.x0, 5 << 28);
        assert_eq!(r.y0, 3 << 28);
        assert_eq!(r.x1, (6u64 << 28) as u32 - 1);
        assert_eq!(r.y1, (4u64 << 28) as u32 - 1);
        let c = Rect::of_cell(MortonCode::new(interleave_bits(65535, 0, 16) as u32));
        assert_eq!((c.x0, c.x1, c.y0, c.y1), (0xFFFF_0000, u32::MAX, 0, 0xFFFF));
    }

    #[test]
    fn children_partition_their_parent() {
        let t = Tile {
            prefix: interleave_bits(2, 7, 3),
            depth: 3,
        };
        let parent = Rect::of_tile(&t);
        let mut cells = 0u64;
        for k in 0..4 {
            let c = Rect::of_tile(&Tile {
                prefix: (t.prefix << 2) | k,
                depth: 4,
            });
            assert!(c.within(&parent));
            cells += (u64::from(c.x1 - c.x0) + 1) * (u64::from(c.y1 - c.y0) + 1);
        }
        assert_eq!(
            cells,
            (u64::from(parent.x1 - parent.x0) + 1) * (u64::from(parent.y1 - parent.y0) + 1)
        );
    }

    /// **The pruned descent hands every named cell the context the full descent gave it.** The
    /// full descent is the oracle: a boundary cell's rows are tested against exactly the edges and
    /// parity it carried down, so the re-derivation must reproduce both.
    #[test]
    fn the_pruned_descent_reproduces_each_boundary_cells_context() {
        use crate::shape::{Part, Polygon, Ring, Vertex};
        let v = |x: u32, y: u32| Vertex { x, y, weight: 0 };
        let polygon = Polygon {
            parts: vec![Part {
                rings: vec![Ring {
                    vertices: vec![
                        v(1 << 20, 1 << 20),
                        v(7 << 20, 2 << 20),
                        v(5 << 20, 6 << 20),
                        v(2 << 20, 5 << 20),
                    ],
                }],
            }],
        };
        let region = polygon.region();
        let full = decompose(&region, None);
        assert!(!full.boundary.is_empty());
        let mut cells: Vec<(MortonCode, _)> =
            full.boundary.iter().map(|b| (b.cell, b.ctx.clone())).collect();
        cells.sort_by_key(|(c, _)| c.raw());
        // Every third cell, so the pruned walk has gaps to skip.
        let picked: Vec<(MortonCode, _)> = cells.into_iter().step_by(3).collect();
        let codes: Vec<MortonCode> = picked.iter().map(|(c, _)| *c).collect();
        let contexts = contexts_at(&region, &codes);
        assert_eq!(contexts.len(), picked.len());
        for ((_, expected), got) in picked.iter().zip(&contexts) {
            assert_eq!(expected, got);
        }
    }
}
