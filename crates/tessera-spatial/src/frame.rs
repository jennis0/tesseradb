//! Legal frames: the aligned squares a projected view may be quantised against
//! (`projections.md` §4.1), and the outward snap that reaches one from a stated box (§4.2).
//!
//! A frame is either **the projection's whole world** or a **2^k-aligned sub-square** — the square
//! one tile covers at some integer zoom offset of that world. Nothing else is legal, because a
//! frame that is not one of these can never coincide with a tile grid, and a corpus quantised to a
//! tight bounding box would be unable to line up with any basemap with nothing saying so. Under
//! [`crate::projection::Projection::WebMercator`] a square here **is** an XYZ tile, so a view whose
//! frame is one of these addresses the scheme every map client already speaks; under an
//! equirectangular view the square is aligned to a tiling no server publishes, which is why the
//! type names an offset rather than a tile.
//!
//! **Every coordinate here is in the unit square** — the space
//! [`crate::projection::Projection::forward`] produces, x east and y south. This module knows
//! nothing about degrees.
//!
//! **The snap is outward and the frame must contain the box.** A box written in degrees will
//! essentially never project onto an aligned square, so a caller says roughly where and
//! [`snap_outward`] takes the smallest legal frame containing it. The difference is resolution the
//! corpus does not get, which is why [`Snap`] carries enough to print it rather than absorbing it.

use crate::morton::Bounds;
use crate::projection::Projection;

/// The finest offset a frame may be taken at (§4.1).
///
/// At 16 the frame is exactly one cell of the whole-world grid, and a frame finer than one cell of
/// the grid it is meant to be a prefix of has stopped being a prefix of anything. It is also far
/// past any real corpus: under Web Mercator that frame is 611 m across, quantised to 9.3 mm cells.
pub const MAX_ZOOM_OFFSET: u32 = 16;

/// One tile of the projection's own world, at an integer zoom offset: the only shape a frame may
/// have besides the whole world, which is this at `z = 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlignedSquare {
    /// The zoom offset, `0..=`[`MAX_ZOOM_OFFSET`]. The world is divided into `2^z` per axis.
    pub z: u32,
    /// The column, `0..2^z`, counting east from the world's west edge.
    pub x: u32,
    /// The row, `0..2^z`, counting **south** from the world's north edge — the XYZ convention,
    /// which the unit square's y direction already carries (`projections.md` §4).
    pub y: u32,
}

impl AlignedSquare {
    /// The projection's whole world: the frame a view takes when nothing narrower contains its
    /// box.
    pub const WORLD: AlignedSquare = AlignedSquare { z: 0, x: 0, y: 0 };

    /// This square as a frame over the unit square.
    ///
    /// Exact: `2^-z` and every multiple of it are representable in `f64` for every offset this
    /// type permits, so a frame's edges are the numbers they are written as and no rounding
    /// separates two views that declared the same square.
    pub fn bounds(&self) -> Bounds {
        let side = 1.0 / f64::from(1u32 << self.z);
        Bounds {
            x_min: f64::from(self.x) * side,
            x_max: f64::from(self.x + 1) * side,
            y_min: f64::from(self.y) * side,
            y_max: f64::from(self.y + 1) * side,
        }
    }

    /// The square a frame **is**, or `None` where the frame is not one of these at all.
    ///
    /// The exact inverse of [`AlignedSquare::bounds`], and exact is the word: every edge must be
    /// the `f64` the square's own arithmetic produces, with no tolerance. A frame an ulp off an
    /// aligned square is a frame whose cell grid is an ulp off the tile grid, and a basemap drawn
    /// under it would be wrong by a whole cell somewhere across 65,536 of them — so *nearly
    /// aligned* is a case to answer `None` to rather than to round into alignment. The comparison
    /// is affordable because `bounds` produces only dyadic rationals, which are exact in `f64`.
    ///
    /// Every frame a projected build writes is one of these ([`snap_outward`] returns nothing
    /// else), so the interesting inputs here are the frames a `none` view may declare — an
    /// arbitrary box — and a manifest written by hand.
    pub fn of_bounds(b: &Bounds) -> Option<AlignedSquare> {
        if !b.is_finite() {
            return None;
        }
        // The side length fixes the offset, so at most one square can match; the walk is over
        // seventeen offsets and reconstructs the candidate rather than solving for it, which
        // keeps this obviously the inverse of the four multiplications above.
        for z in 0..=MAX_ZOOM_OFFSET {
            let n = f64::from(1u32 << z);
            let (x, y) = ((b.x_min * n).round(), (b.y_min * n).round());
            if !(0.0..n).contains(&x) || !(0.0..n).contains(&y) {
                continue;
            }
            let square = AlignedSquare {
                z,
                x: x as u32,
                y: y as u32,
            };
            if square.bounds() == *b {
                return Some(square);
            }
        }
        None
    }
}

/// The name of the tile scheme an aligned Web Mercator frame addresses: the slippy-map `z/x/y`
/// every basemap server publishes.
///
/// It is the only scheme this system can name, and it is spelled rather than implied — see
/// [`tile_scheme`] for why a client is told a scheme's name and not a boolean.
pub const XYZ: &str = "xyz";

/// The tile scheme a view's frame addresses, and the tile the frame **is** under it
/// (`projections.md` §9) — or `None` where no published scheme addresses this frame.
///
/// **Grid alignment alone is not enough, and this is the whole reason the answer is a scheme's
/// name rather than a boolean.** An equirectangular frame is a square of a square tiling, so
/// [`AlignedSquare::of_bounds`] answers for it exactly as it does for a Web Mercator one — but the
/// published longitude/latitude schemes are 2:1 at their top level and no server serves the square
/// tiling that frame is aligned to. A caller reading alignment as availability would draw a
/// Mercator basemap under a corpus that cannot line up with one, which is a wrong map rather than a
/// missing one. So the question this answers is *which* scheme, and the absence of an answer is
/// what says: draw the points, draw no basemap.
///
/// [`Projection::None`] has no world, no north and no tiles, and a Web Mercator view whose frame is
/// not an aligned square — a frame a `none` view's spelling could still declare — addresses nothing
/// either.
pub fn tile_scheme(projection: Projection, frame: &Bounds) -> Option<(&'static str, AlignedSquare)> {
    match projection {
        Projection::WebMercator => AlignedSquare::of_bounds(frame).map(|square| (XYZ, square)),
        Projection::Equirectangular { .. } | Projection::None => None,
    }
}

/// The frame a stated box snapped to, and whether the cap chose it rather than the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snap {
    pub square: AlignedSquare,
    /// **The frame was floored rather than fitted**: the box fits inside a square finer than
    /// [`MAX_ZOOM_OFFSET`] allows, so the offset is the cap's answer and not the box's.
    ///
    /// This is the degenerate case of `projections.md` §4.2 — a box of zero width, of zero height,
    /// or a single point is contained in aligned squares without bound — and also the ordinary
    /// case of a box smaller than one whole-world cell. Reported rather than silent, because a
    /// caller whose frame did not come from their box should be told which one they got.
    pub floored: bool,
}

/// The tile a unit-square coordinate falls in at zoom `z`, on the **half-open** convention the
/// cell grid already uses (contracts §2.5): a coordinate on a boundary belongs to the higher
/// tile, and the world's own maximum `1.0` belongs to the last one.
///
/// Defined for `v` in `[0, 1]` and `z` no more than one past the cap; a coordinate outside the
/// unit square is held at the edge, which is what
/// [`crate::projection::Projection::forward`] has already done to anything that reaches here.
pub fn tile_of(v: f64, z: u32) -> u32 {
    debug_assert!(
        z <= MAX_ZOOM_OFFSET + 1,
        "tile_of(): offset {z} is past the cap"
    );
    let n = f64::from(1u32 << z);
    let t = (v * n).floor();
    if t <= 0.0 {
        0
    } else if t >= n - 1.0 {
        (1u32 << z) - 1
    } else {
        t as u32
    }
}

/// Whether one aligned square at offset `z` contains the whole box.
///
/// Both corners of an axis falling in one tile is exactly containment, because the tile a
/// coordinate falls in is the half-open interval it sits inside. **A box whose maximum lies on a
/// tile boundary therefore spans two tiles at that offset** — the boundary coordinate belongs to
/// the higher tile — and the box snaps a level coarser. That is the design's stated consequence
/// (`projections.md` §4.2) and not a rounding artefact: the frame must contain the box.
fn fits(b: &Bounds, z: u32) -> bool {
    tile_of(b.x_min, z) == tile_of(b.x_max, z) && tile_of(b.y_min, z) == tile_of(b.y_max, z)
}

/// The smallest aligned square containing `b`, at an offset no finer than [`MAX_ZOOM_OFFSET`].
///
/// `b` is a box over the unit square — the image of a longitude/latitude box under a cylindrical
/// projection, whose corners are its bounds (`projections.md` §4.2). A **degenerate** box is
/// legal here and is the point of [`Snap::floored`]: `Bounds::validate` refuses a zero-width box
/// as a *frame*, and this is not a frame, it is the thing a frame is fitted to.
///
/// Containment is monotone in the offset — the tile containing a tile is unique — so the search
/// walks down from the cap and stops at the first square that holds the box. The whole world at
/// `z = 0` always does.
pub fn snap_outward(b: &Bounds) -> Snap {
    debug_assert!(
        b.is_finite(),
        "snap_outward(): the box must be finite, got {b:?}"
    );
    debug_assert!(
        b.x_min <= b.x_max && b.y_min <= b.y_max,
        "snap_outward(): the box must not be inverted, got {b:?}"
    );
    // Floored means the cap bound the answer rather than the box: had one more offset been legal,
    // the box would have fitted there too. One extra containment test says so exactly, which is
    // what lets a degenerate box and a merely tiny one be reported the same way — they are the
    // same case.
    let floored = fits(b, MAX_ZOOM_OFFSET + 1);
    for z in (0..=MAX_ZOOM_OFFSET).rev() {
        if fits(b, z) {
            return Snap {
                square: AlignedSquare {
                    z,
                    x: tile_of(b.x_min, z),
                    y: tile_of(b.y_min, z),
                },
                floored,
            };
        }
    }
    Snap {
        square: AlignedSquare::WORLD,
        floored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every square this type can name is a frame `Bounds::validate` accepts, and its edges are
    /// the exact dyadic rationals rather than something a division rounded.
    #[test]
    fn a_square_is_a_valid_frame() {
        for z in 0..=MAX_ZOOM_OFFSET {
            let last = (1u32 << z) - 1;
            for (x, y) in [(0, 0), (last, last), (last / 3, last / 7)] {
                let b = AlignedSquare { z, x, y }.bounds();
                b.validate()
                    .unwrap_or_else(|e| panic!("z{z} ({x}, {y}): {e}"));
                assert_eq!(b.x_max - b.x_min, 1.0 / f64::from(1u32 << z));
            }
        }
        assert_eq!(
            AlignedSquare::WORLD.bounds(),
            Bounds {
                x_min: 0.0,
                x_max: 1.0,
                y_min: 0.0,
                y_max: 1.0
            }
        );
        // The last square of each axis ends exactly at the world's edge — not an ulp short, which
        // would clamp every point in the easternmost or southernmost frame.
        let last = AlignedSquare {
            z: 16,
            x: 65535,
            y: 65535,
        }
        .bounds();
        assert_eq!((last.x_max, last.y_max), (1.0, 1.0));
    }

    /// The half-open rule, at the boundaries where it is the whole answer.
    #[test]
    fn a_coordinate_on_a_boundary_belongs_to_the_higher_tile() {
        assert_eq!(tile_of(0.5, 1), 1);
        assert_eq!(tile_of(0.4999999999, 1), 0);
        assert_eq!(tile_of(0.25, 2), 1);
        assert_eq!(tile_of(0.0, 16), 0);
        // The world's own maximum is the last tile, never one past it — the cell grid's rule.
        assert_eq!(tile_of(1.0, 0), 0);
        assert_eq!(tile_of(1.0, 4), 15);
        assert_eq!(tile_of(1.0, 16), 65535);
    }

    /// **Hand-computed, not recorded.** `x = (lon + 180)/360` and `y = 0.5 - lat/180` are exact on
    /// these values, so each address below is arithmetic a reader can redo:
    ///
    /// * `[-180, -90] x [45, 90]` is `x [0, 0.25], y [0, 0.25]` — the z2 tile (0, 0) exactly. Its
    ///   maxima sit **on** the z2 boundary and so belong to tile 1, which spans two tiles, so it
    ///   snaps to z1 (0, 0) = `[0, 0.5]`. The frame contains the box, which is the property that
    ///   decides this case.
    /// * `[-180, -91] x [46, 90]` is `x [0, 0.2472…], y [0, 0.2444…]` — inside that boundary, so
    ///   it fits z2 (0, 0).
    /// * `[-144, -36] x [-72, -18]` is `x [0.1, 0.4], y [0.6, 0.9]` — both corners inside the z1
    ///   tile (0, 1), and x straddles the z2 boundary at 0.25, so z1 (0, 1) = `x [0, 0.5],
    ///   y [0.5, 1]`.
    /// * `[-90, 0] x [-45, 0]` is `x [0.25, 0.5], y [0.5, 0.75]`: **x_max is the z1 boundary
    ///   itself**, so it belongs to tile 1 while x_min is in tile 0, and the whole world is the
    ///   only square that holds it. A box whose corner sits on a boundary is the case where the
    ///   frame taken is dramatically coarser than the box's own size suggests.
    /// * `[0, 180] x [-90, 0]` is `x [0.5, 1], y [0.5, 1]` — the z1 tile (1, 1), whose maxima are
    ///   the world's own edge and belong to it, so this one does **not** snap coarser.
    #[test]
    fn the_snap_is_the_smallest_containing_square() {
        // The equirectangular transform, written out rather than called: the point is that these
        // addresses are computable by hand, and a test that borrows the code it checks is not.
        let boxed = |lon: [f64; 2], lat: [f64; 2]| Bounds {
            x_min: (lon[0] + 180.0) / 360.0,
            x_max: (lon[1] + 180.0) / 360.0,
            y_min: 0.5 - lat[1] / 180.0,
            y_max: 0.5 - lat[0] / 180.0,
        };
        for (lon, lat, want) in [
            (
                [-180.0, -90.0],
                [45.0, 90.0],
                AlignedSquare { z: 1, x: 0, y: 0 },
            ),
            (
                [-180.0, -91.0],
                [46.0, 90.0],
                AlignedSquare { z: 2, x: 0, y: 0 },
            ),
            (
                [-144.0, -36.0],
                [-72.0, -18.0],
                AlignedSquare { z: 1, x: 0, y: 1 },
            ),
            ([-90.0, 0.0], [-45.0, 0.0], AlignedSquare::WORLD),
            (
                [0.0, 180.0],
                [-90.0, 0.0],
                AlignedSquare { z: 1, x: 1, y: 1 },
            ),
            ([-180.0, 180.0], [-90.0, 90.0], AlignedSquare::WORLD),
        ] {
            let b = boxed(lon, lat);
            let snap = snap_outward(&b);
            assert_eq!(snap.square, want, "lon {lon:?}, lat {lat:?} → {b:?}");
            assert!(
                !snap.floored,
                "lon {lon:?}, lat {lat:?} was fitted, not floored"
            );
            let frame = snap.square.bounds();
            assert!(
                frame.x_min <= b.x_min
                    && b.x_max <= frame.x_max
                    && frame.y_min <= b.y_min
                    && b.y_max <= frame.y_max,
                "the frame {frame:?} does not contain the box {b:?}"
            );
        }
    }

    /// The boxes with no smallest enclosing square (`projections.md` §4.2), and the one that has
    /// one only because the *other* axis constrains it.
    #[test]
    fn a_degenerate_box_is_floored_at_the_cap() {
        let point = Bounds {
            x_min: 0.3,
            x_max: 0.3,
            y_min: 0.7,
            y_max: 0.7,
        };
        let snap = snap_outward(&point);
        assert_eq!(snap.square.z, MAX_ZOOM_OFFSET);
        assert!(snap.floored);
        // Hand-computed: 0.3 × 65536 = 19660.8 and 0.7 × 65536 = 45875.2, floored.
        assert_eq!((snap.square.x, snap.square.y), (19660, 45875));

        // Zero width and zero height, each inside one whole-world cell on the other axis, so the
        // cap is what stops the search on both. The non-degenerate axis runs from 0.5 — which is
        // cell 32768's own edge — to a quarter of a cell in, so it crosses no boundary at the cap or one past it.
        let quarter_cell = 0.25 / 65536.0;
        for b in [
            Bounds {
                x_min: 0.3,
                x_max: 0.3,
                y_min: 0.5,
                y_max: 0.5 + quarter_cell,
            },
            Bounds {
                x_min: 0.5,
                x_max: 0.5 + quarter_cell,
                y_min: 0.7,
                y_max: 0.7,
            },
        ] {
            let snap = snap_outward(&b);
            assert_eq!(snap.square.z, MAX_ZOOM_OFFSET, "{b:?}");
            assert!(snap.floored, "{b:?}");
        }

        // **A degenerate axis constrains nothing, and the other axis still does.** A box of zero
        // width spanning half the world in y takes the offset y allows — floored would hand it a
        // 2^-16 frame that does not contain it, and the frame must contain the box.
        let tall = Bounds {
            x_min: 0.3,
            x_max: 0.3,
            y_min: 0.1,
            y_max: 0.6,
        };
        let snap = snap_outward(&tall);
        assert_eq!(snap.square, AlignedSquare::WORLD);
        assert!(!snap.floored);
    }

    /// [`AlignedSquare::of_bounds`] recovers every square [`AlignedSquare::bounds`] can write, and
    /// refuses everything else — including a frame an ulp away from one.
    #[test]
    fn a_frame_is_read_back_as_the_square_that_wrote_it() {
        for z in 0..=MAX_ZOOM_OFFSET {
            let last = (1u32 << z) - 1;
            for (x, y) in [(0, 0), (last, last), (last / 3, last / 7), (last / 2, 0)] {
                let square = AlignedSquare { z, x, y };
                assert_eq!(AlignedSquare::of_bounds(&square.bounds()), Some(square));
            }
        }

        // A tight bounding box — what a `none` view's `auto` fits — is not a square of any tiling,
        // and neither is a rectangle, a square of the wrong side, or one offset from the grid.
        for b in [
            Bounds {
                x_min: 0.1,
                x_max: 0.8,
                y_min: 0.2,
                y_max: 0.4,
            },
            Bounds {
                x_min: 0.0,
                x_max: 0.5,
                y_min: 0.0,
                y_max: 1.0,
            },
            Bounds {
                x_min: 0.0,
                x_max: 0.3,
                y_min: 0.0,
                y_max: 0.3,
            },
            Bounds {
                x_min: 0.125,
                x_max: 0.625,
                y_min: 0.0,
                y_max: 0.5,
            },
            Bounds {
                x_min: -1.0,
                x_max: 1.0,
                y_min: -1.0,
                y_max: 1.0,
            },
            Bounds {
                x_min: f64::NAN,
                x_max: 1.0,
                y_min: 0.0,
                y_max: 1.0,
            },
        ] {
            assert_eq!(AlignedSquare::of_bounds(&b), None, "{b:?}");
        }

        // **Nearly aligned is not aligned.** One ulp on one edge is a frame whose grid is off the
        // tile grid, and reading it as the square would put a basemap a cell out somewhere.
        let mut nudged = AlignedSquare { z: 3, x: 2, y: 5 }.bounds();
        nudged.x_max = f64::from_bits(nudged.x_max.to_bits() + 1);
        assert_eq!(AlignedSquare::of_bounds(&nudged), None);
    }

    /// **The case a boolean gets wrong** (`projections.md` §9): an equirectangular frame is as
    /// aligned as a Web Mercator one and addresses no published scheme, so it publishes none.
    ///
    /// The addresses are hand-computed. `[0, 0.5] x [0, 0.5]` is the z1 tile (0, 0) — the world's
    /// north-west quarter, which under Web Mercator is XYZ `1/0/0`; `[0.75, 1] x [0.5, 0.75]` is
    /// z2 (3, 2), the tile three east and two south of the north-west corner at four per axis.
    #[test]
    fn only_an_aligned_web_mercator_frame_addresses_a_scheme() {
        let world = AlignedSquare::WORLD.bounds();
        let quarter = AlignedSquare { z: 1, x: 0, y: 0 }.bounds();
        let sixteenth = AlignedSquare { z: 2, x: 3, y: 2 }.bounds();

        assert_eq!(
            tile_scheme(Projection::WebMercator, &world),
            Some((XYZ, AlignedSquare { z: 0, x: 0, y: 0 }))
        );
        assert_eq!(
            tile_scheme(Projection::WebMercator, &quarter),
            Some((XYZ, AlignedSquare { z: 1, x: 0, y: 0 }))
        );
        assert_eq!(
            tile_scheme(Projection::WebMercator, &sixteenth),
            Some((XYZ, AlignedSquare { z: 2, x: 3, y: 2 }))
        );

        // Aligned, and addressing nothing: the frames are the same three squares.
        for projection in [
            Projection::PLATE_CARREE,
            Projection::GALL_ISOGRAPHIC,
            Projection::None,
        ] {
            for frame in [world, quarter, sixteenth] {
                assert!(
                    AlignedSquare::of_bounds(&frame).is_some(),
                    "the frame is aligned, which is the premise"
                );
                assert_eq!(
                    tile_scheme(projection, &frame),
                    None,
                    "{} published a scheme for an aligned frame",
                    projection.name()
                );
            }
        }

        // A Web Mercator view whose frame is not a square addresses nothing either.
        assert_eq!(
            tile_scheme(
                Projection::WebMercator,
                &Bounds {
                    x_min: 0.1,
                    x_max: 0.8,
                    y_min: 0.2,
                    y_max: 0.4
                }
            ),
            None
        );
    }

    /// Containment is monotone in the offset, which is what lets the search walk down and stop.
    #[test]
    fn containment_is_monotone_in_the_offset() {
        let mut state = 0x243F_6A88_85A3_08D3u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        for _ in 0..20_000 {
            let (a, b) = (next(), next());
            let (c, d) = (next(), next());
            let b = Bounds {
                x_min: a.min(b),
                x_max: a.max(b),
                y_min: c.min(d),
                y_max: c.max(d),
            };
            let snap = snap_outward(&b);
            for z in 0..=snap.square.z {
                assert!(fits(&b, z), "{b:?} fits z{} but not z{z}", snap.square.z);
            }
            if snap.square.z < MAX_ZOOM_OFFSET {
                assert!(
                    !fits(&b, snap.square.z + 1),
                    "{b:?} is not the smallest square"
                );
            }
            let frame = snap.square.bounds();
            assert!(
                frame.x_min <= b.x_min
                    && b.x_max <= frame.x_max
                    && frame.y_min <= b.y_min
                    && b.y_max <= frame.y_max,
                "the frame {frame:?} does not contain the box {b:?}"
            );
        }
    }
}
