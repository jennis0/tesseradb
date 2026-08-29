//! Circles and ellipses as one conic in grid units.
//!
//! A circle of radius *r* in data units is `(p − c)ᵀ A (p − c) ≤ 1` with `A = I / r²`; an ellipse
//! with semi-axes *a*, *b* rotated by θ has `A = R diag(1/a², 1/b²) Rᵀ`. Quantisation scales the
//! two axes by `sx = 2³² / (x_max − x_min)` and `sy` likewise, so in grid units the matrix is
//! `M = S⁻¹ A S⁻¹` — a circle on a non-square extent is an ellipse here, and a rotated ellipse a
//! differently rotated one. Storing the caller's five numbers would store a shape the grid does
//! not hold; storing the conic stores the one it does.
//!
//! The test is `q(d) = m11·dx² + 2·m12·dx·dy + m22·dy² ≤ 1` in `f64`, with `dx`, `dy` exact
//! (a difference of two `u32`s is exactly representable). Every operation is correctly rounded,
//! so the result is the same on every platform; it is not exact at the last bit, and the property
//! tests skip positions within rounding of the boundary rather than pretend otherwise.

use super::decompose::{Class, Rect, Region};
use super::{Bbox, GridPoint};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Conic {
    pub cx: u32,
    pub cy: u32,
    pub m11: f64,
    pub m12: f64,
    pub m22: f64,
}

impl Conic {
    /// A conic from data-space parameters and the per-axis grid scales.
    ///
    /// `angle` is degrees anticlockwise from the x axis in data space. `a`, `b` and `r` are
    /// data units. `None` if any axis is not positive or any input is not finite.
    pub fn from_ellipse(
        (cx, cy): GridPoint,
        a: f64,
        b: f64,
        angle_degrees: f64,
        (sx, sy): (f64, f64),
    ) -> Option<Conic> {
        if !(a > 0.0 && b > 0.0 && a.is_finite() && b.is_finite() && angle_degrees.is_finite()) {
            return None;
        }
        let t = angle_degrees.to_radians();
        let (s, c) = t.sin_cos();
        let ia = 1.0 / (a * a);
        let ib = 1.0 / (b * b);
        // A = R diag(ia, ib) Rᵀ with R = [[c, -s], [s, c]].
        let a11 = c * c * ia + s * s * ib;
        let a12 = c * s * ia - s * c * ib;
        let a22 = s * s * ia + c * c * ib;
        // M = S⁻¹ A S⁻¹.
        Some(Conic {
            cx,
            cy,
            m11: a11 / (sx * sx),
            m12: a12 / (sx * sy),
            m22: a22 / (sy * sy),
        })
    }

    fn q(&self, (x, y): GridPoint) -> f64 {
        let dx = f64::from(x) - f64::from(self.cx);
        let dy = f64::from(y) - f64::from(self.cy);
        self.m11 * dx * dx + 2.0 * self.m12 * dx * dy + self.m22 * dy * dy
    }

    pub fn contains(&self, p: GridPoint) -> bool {
        self.q(p) <= 1.0
    }

    /// The semi-axis lengths in grid units and the major axis's angle, from the eigen-decomposition
    /// of `M`: `(major, minor, angle_radians)`.
    fn axes(&self) -> (f64, f64, f64) {
        let tr = self.m11 + self.m22;
        let det = self.m11 * self.m22 - self.m12 * self.m12;
        let disc = (tr * tr / 4.0 - det).max(0.0).sqrt();
        let l_min = (tr / 2.0 - disc).max(f64::MIN_POSITIVE); // smallest eigenvalue: major axis
        let l_max = tr / 2.0 + disc;
        // The major axis is the eigenvector of the smallest eigenvalue, `(m12, λ − m11)`; an axis
        // is a line, so the angle is taken modulo π.
        let angle = if self.m12 == 0.0 {
            if self.m11 <= self.m22 {
                0.0
            } else {
                std::f64::consts::FRAC_PI_2
            }
        } else {
            (l_min - self.m11)
                .atan2(self.m12)
                .rem_euclid(std::f64::consts::PI)
        };
        (1.0 / l_min.sqrt(), 1.0 / l_max.sqrt(), angle)
    }

    /// The grid box enclosing the conic — the half-widths are `sqrt(M⁻¹)`'s diagonal.
    pub fn bounds(&self) -> Bbox {
        let det = (self.m11 * self.m22 - self.m12 * self.m12).max(f64::MIN_POSITIVE);
        let hx = (self.m22 / det).sqrt();
        let hy = (self.m11 / det).sqrt();
        let clamp = |v: f64| v.round().clamp(0.0, u32::MAX as f64) as u32;
        Bbox {
            min_x: clamp(f64::from(self.cx) - hx),
            min_y: clamp(f64::from(self.cy) - hy),
            max_x: clamp(f64::from(self.cx) + hx),
            max_y: clamp(f64::from(self.cy) + hy),
        }
    }

    /// The least value of `q` over a closed rectangle. `q` is convex, so it is zero when the
    /// centre lies inside, and otherwise attained on one of the four sides, where it is a
    /// one-dimensional quadratic clamped to the side.
    fn min_over(&self, r: Rect) -> f64 {
        if r.contains((self.cx, self.cy)) {
            return 0.0;
        }
        let (x0, x1) = (
            f64::from(r.x0) - f64::from(self.cx),
            f64::from(r.x1) - f64::from(self.cx),
        );
        let (y0, y1) = (
            f64::from(r.y0) - f64::from(self.cy),
            f64::from(r.y1) - f64::from(self.cy),
        );
        let q =
            |dx: f64, dy: f64| self.m11 * dx * dx + 2.0 * self.m12 * dx * dy + self.m22 * dy * dy;
        let mut best = f64::INFINITY;
        for dx in [x0, x1] {
            // Minimise over dy ∈ [y0, y1]: derivative zero at dy = −m12·dx / m22.
            let dy = (-self.m12 * dx / self.m22).clamp(y0, y1);
            best = best.min(q(dx, dy));
        }
        for dy in [y0, y1] {
            let dx = (-self.m12 * dy / self.m11).clamp(x0, x1);
            best = best.min(q(dx, dy));
        }
        best
    }

    /// A ring around the conic with no chord further than `tolerance` grid units from the curve,
    /// at most `budget` vertices, anticlockwise from the major axis.
    pub fn ring(&self, tolerance: u32, budget: usize) -> Vec<GridPoint> {
        self.ring_guarded(tolerance, budget).0
    }

    /// [`Conic::ring`], and whether the budget held the vertex count below what the tolerance
    /// asked for — the guard of `polygon-membership.md` §7.2, fired for a curve as for a polygon.
    pub fn ring_guarded(&self, tolerance: u32, budget: usize) -> (Vec<GridPoint>, bool) {
        let (major, minor, angle) = self.axes();
        let tol = f64::from(tolerance);
        // Sagitta of a chord subtending 2π/n on a circle of radius R is R(1 − cos(π/n)).
        let wanted = if major <= tol {
            8.0
        } else {
            (std::f64::consts::PI / (1.0 - tol / major).acos()).ceil()
        };
        let guarded = wanted > budget.max(8) as f64;
        let n = (wanted as usize).clamp(8, budget.max(8));
        let (s, c) = angle.sin_cos();
        let clamp = |v: f64| v.round().clamp(0.0, u32::MAX as f64) as u32;
        let ring: Vec<GridPoint> = (0..n)
            .map(|k| {
                let t = 2.0 * std::f64::consts::PI * k as f64 / n as f64;
                let (u, v) = (major * t.cos(), minor * t.sin());
                let x = f64::from(self.cx) + c * u - s * v;
                let y = f64::from(self.cy) + s * u + c * v;
                (clamp(x), clamp(y))
            })
            .collect();
        (ring, guarded)
    }
}

impl Region for Conic {
    type Ctx = ();

    fn root(&self) {}

    fn refine(&self, _: &(), _: Rect, _: Rect) {}

    fn classify(&self, rect: Rect, _: &()) -> Class {
        // Convex: a rectangle is inside iff its corners are.
        if rect.corners().iter().all(|&c| self.q(c) <= 1.0) {
            return Class::Inside;
        }
        if self.min_over(rect) > 1.0 {
            return Class::Disjoint;
        }
        Class::Crossed
    }

    fn contains(&self, p: GridPoint, _: Rect, _: &()) -> bool {
        self.contains(p)
    }
}

impl Region for Bbox {
    type Ctx = ();

    fn root(&self) {}

    fn refine(&self, _: &(), _: Rect, _: Rect) {}

    fn classify(&self, rect: Rect, _: &()) -> Class {
        let own = Rect {
            x0: self.min_x,
            y0: self.min_y,
            x1: self.max_x,
            y1: self.max_y,
        };
        if rect.within(&own) {
            Class::Inside
        } else if rect.disjoint(&own) {
            Class::Disjoint
        } else {
            Class::Crossed
        }
    }

    fn contains(&self, p: GridPoint, _: Rect, _: &()) -> bool {
        Bbox::contains(self, p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_circle_on_a_square_extent_has_equal_axes() {
        let c =
            Conic::from_ellipse((1 << 31, 1 << 31), 100.0, 100.0, 0.0, (1024.0, 1024.0)).unwrap();
        let (a, b, _) = c.axes();
        assert!((a - 102_400.0).abs() < 1e-3 && (b - 102_400.0).abs() < 1e-3);
        assert!(c.contains((1 << 31, (1u32 << 31) + 102_399)));
        assert!(!c.contains((1 << 31, (1u32 << 31) + 102_401)));
    }

    #[test]
    fn a_rotated_ellipse_keeps_its_axes_through_the_decomposition_of_m() {
        let c = Conic::from_ellipse((1 << 31, 1 << 31), 300.0, 100.0, 30.0, (1.0, 1.0)).unwrap();
        let (a, b, angle) = c.axes();
        assert!((a - 300.0).abs() < 1e-6, "{a}");
        assert!((b - 100.0).abs() < 1e-6, "{b}");
        assert!(
            (angle.to_degrees() - 30.0).abs() < 1e-6,
            "{}",
            angle.to_degrees()
        );
    }

    #[test]
    fn the_ring_lies_on_the_curve() {
        let c = Conic::from_ellipse((1 << 31, 1 << 31), 5000.0, 2000.0, 45.0, (1.0, 1.0)).unwrap();
        for p in c.ring(16, 4096) {
            assert!((c.q(p) - 1.0).abs() < 1e-2);
        }
    }
}
