//! Legal frames: the aligned squares a projected view may be quantised against, and the outward
//! snap that reaches one from a stated box.
//!
//! A frame is either the projection's whole world or a 2^k-aligned sub-square, the square one
//! tile covers at some integer zoom offset. Under [`crate::projection::Projection::WebMercator`]
//! a square here is an XYZ tile; under an equirectangular view it is aligned to a tiling no
//! server publishes, so the type names an offset rather than a tile.
//!
//! Every coordinate here is in the unit square, x east and y south, the space
//! [`crate::projection::Projection::forward`] produces.
//!
//! [`snap_outward`] takes the smallest legal frame containing a stated box, and [`Snap`] reports
//! when the cap rather than the box decided it.

use crate::morton::Bounds;
use crate::projection::Projection;

/// The finest offset a frame may be taken at.
///
/// At 16 the frame is exactly one cell of the whole-world grid; a frame finer than one cell of
/// the grid it prefixes would no longer be a prefix of anything.
pub const MAX_ZOOM_OFFSET: u32 = 16;

/// One tile of the projection's own world, at an integer zoom offset: the only shape a frame may
/// have besides the whole world, which is this at `z = 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlignedSquare {
    /// The zoom offset, `0..=`[`MAX_ZOOM_OFFSET`]. The world is divided into `2^z` per axis.
    pub z: u32,
    /// The column, `0..2^z`, counting east from the world's west edge.
    pub x: u32,
    /// The row, `0..2^z`, counting south from the world's north edge, the XYZ convention the
    /// unit square's y direction already carries.
    pub y: u32,
}

impl AlignedSquare {
    /// The projection's whole world: the frame taken when nothing narrower contains its box.
    pub const WORLD: AlignedSquare = AlignedSquare { z: 0, x: 0, y: 0 };

    /// This square as a frame over the unit square: exact, since `2^-z` and its multiples are
    /// representable in `f64`, so no rounding separates two views that declared the same square.
    pub fn bounds(&self) -> Bounds {
        let side = 1.0 / f64::from(1u32 << self.z);
        Bounds {
            x_min: f64::from(self.x) * side,
            x_max: f64::from(self.x + 1) * side,
            y_min: f64::from(self.y) * side,
            y_max: f64::from(self.y + 1) * side,
        }
    }

    /// The square a frame is, or `None` where the frame is not one of these.
    ///
    /// The exact inverse of [`AlignedSquare::bounds`]: every edge must match the `f64` the
    /// square's own arithmetic produces, with no tolerance, so a frame an ulp off an aligned
    /// square is `None` rather than rounded into alignment.
    pub fn of_bounds(b: &Bounds) -> Option<AlignedSquare> {
        if !b.is_finite() {
            return None;
        }
        // The side length fixes the offset, so at most one square can match.
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
/// every basemap server publishes; see [`tile_scheme`] for why the answer is a name not a bool.
pub const XYZ: &str = "xyz";

/// The tile scheme a view's frame addresses, and the tile it is under it, or `None` where no
/// published scheme addresses this frame.
///
/// The answer is a name rather than a boolean: an equirectangular frame is a square of a square
/// tiling too, so [`AlignedSquare::of_bounds`] answers for it exactly as for Web Mercator, but no
/// server publishes that tiling.
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
    /// True when the box fits inside a square finer than [`MAX_ZOOM_OFFSET`] allows: a degenerate
    /// box (zero width, zero height, a point) or one smaller than a whole-world cell.
    pub floored: bool,
}

/// The tile a unit-square coordinate falls in at zoom `z`, half-open: a coordinate on a boundary
/// belongs to the higher tile, and the world's maximum `1.0` belongs to the last one.
///
/// Defined for `v` in `[0, 1]`, `z` no more than one past the cap; an out-of-range coordinate is
/// held at the edge, already done by [`crate::projection::Projection::forward`].
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

/// Whether one aligned square at offset `z` contains the whole box: both corners of an axis
/// falling in one tile. A box whose maximum lies on a tile boundary spans two tiles, since the
/// boundary belongs to the higher tile, and snaps a level coarser.
fn fits(b: &Bounds, z: u32) -> bool {
    tile_of(b.x_min, z) == tile_of(b.x_max, z) && tile_of(b.y_min, z) == tile_of(b.y_max, z)
}

/// The smallest aligned square containing `b`, at an offset no finer than [`MAX_ZOOM_OFFSET`].
/// A degenerate box (zero width or height) is legal here, unlike a frame, which is the point of
/// [`Snap::floored`].
///
/// Containment is monotone in the offset, so the search walks down from the cap and stops at the
/// first square that holds the box.
pub fn snap_outward(b: &Bounds) -> Snap {
    debug_assert!(
        b.is_finite(),
        "snap_outward(): the box must be finite, got {b:?}"
    );
    debug_assert!(
        b.x_min <= b.x_max && b.y_min <= b.y_max,
        "snap_outward(): the box must not be inverted, got {b:?}"
    );
    // Floored means the cap, not the box, bound the offset.
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
        // The last square of each axis ends at the world's edge exactly, not an ulp short.
        let last = AlignedSquare {
            z: 16,
            x: 65535,
            y: 65535,
        }
        .bounds();
        assert_eq!((last.x_max, last.y_max), (1.0, 1.0));
    }

    #[test]
    fn a_coordinate_on_a_boundary_belongs_to_the_higher_tile() {
        assert_eq!(tile_of(0.5, 1), 1);
        assert_eq!(tile_of(0.4999999999, 1), 0);
        assert_eq!(tile_of(0.25, 2), 1);
        assert_eq!(tile_of(0.0, 16), 0);
        // The world's own maximum is the last tile, never one past it: the cell grid's rule.
        assert_eq!(tile_of(1.0, 0), 0);
        assert_eq!(tile_of(1.0, 4), 15);
        assert_eq!(tile_of(1.0, 16), 65535);
    }

    /// Hand-computed: `x = (lon + 180)/360`, `y = 0.5 - lat/180`. Corners on a boundary snap
    /// coarser, e.g. `[-180, -90] x [45, 90]` sits on the z2 boundary and so snaps to z1.
    #[test]
    fn the_snap_is_the_smallest_containing_square() {
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

        // Zero width and zero height, inside one whole-world cell on the other axis.
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

        // A degenerate axis constrains nothing; the other axis still does.
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

    #[test]
    fn a_frame_is_read_back_as_the_square_that_wrote_it() {
        for z in 0..=MAX_ZOOM_OFFSET {
            let last = (1u32 << z) - 1;
            for (x, y) in [(0, 0), (last, last), (last / 3, last / 7), (last / 2, 0)] {
                let square = AlignedSquare { z, x, y };
                assert_eq!(AlignedSquare::of_bounds(&square.bounds()), Some(square));
            }
        }

        // Not a square of any tiling: a rectangle, a wrong-side square, one offset from the grid.
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

        // Nearly aligned is not aligned: one ulp on an edge is off the tile grid.
        let mut nudged = AlignedSquare { z: 3, x: 2, y: 5 }.bounds();
        nudged.x_max = f64::from_bits(nudged.x_max.to_bits() + 1);
        assert_eq!(AlignedSquare::of_bounds(&nudged), None);
    }

    /// An equirectangular frame is as aligned as a Web Mercator one but addresses no scheme.
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
