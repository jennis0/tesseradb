//! Map projections: a place on the Earth to a coordinate in a view's frame
//! (`projections.md` §5).
//!
//! The enumerated set, forward and inverse, and nothing else. A projection here is a pure
//! function of two `f64`s — it does not read a declaration, does not know an extent, and does not
//! quantise. `morton.rs` takes it from there.
//!
//! **⊘ Partially reached.** A view declares its projection, and both entry points transform every
//! coordinate through it: a *build* at the read of a points file (`projections.md` §2, §4;
//! `tessera_build::config`) and an *ingest* at the decode of an Arrow batch, whose coordinate
//! columns are `lon`/`lat` for a projected view and whose write-ahead log holds the frame
//! coordinates this module produced (§3; `tessera_server`'s `control` module). What remains: the
//! two built geographic corpora are still projected outside the build by
//! `test_corpora/common/projection.py` before their rows are ever seen; the build's frame report
//! does not yet name the projection or the snap (§8); `/v1/meta` publishes the frame alone, so a
//! client cannot yet tell a geographic corpus from an embedding (§9); and a shape declared in
//! longitude and latitude is still refused at parse (§10).
//!
//! **Every projection's output is the unit square, x east and y south** (§4). The frame is
//! `[0, 1]` on both axes whatever the projection, so tile addressing is integer arithmetic and a
//! declaration carries no magic constant; for [`Projection::WebMercator`] a 16-bit cell is then
//! exactly an XYZ tile at zoom 16. The southward y is applied here, at the definition, and never
//! left to a caller: an XYZ tile `y = 0` is the northernmost row and this system's cell y
//! increases with tile y (measured against deck.gl's own tileset implementation —
//! `clients/ts/core/src/coords.ts`), while EPSG:3857's northing increases *northward*. A frame
//! that gets this wrong is mirrored against every basemap, and `Bounds::validate` requires
//! `y_max > y_min`, so it cannot be repaired by inverting the extent afterwards.
//!
//! [`Projection::None`] is the exception that proves the axis rule rather than breaking it: it
//! performs no transform at all, so there is no unit square and no north, and coordinates keep
//! exactly the meaning they have today.
//!
//! **Clipping is not clamping, and that is why [`Projection::is_clipped`] exists separately**
//! (§7). A latitude beyond a projection's own domain is moved onto the frame's edge — and a point
//! *on* the edge is not clamped by the quantisation rule (`contracts.md` §2.5), which says
//! `v = max` lands in the top cell. So the build's clamp counter structurally cannot see a single
//! clipped point, however many there are, and a count that is taken at all has to be taken here.
//! The predicate is over the input latitude for that reason: after the transform the evidence is
//! gone.
//!
//! **This module is checked against the incumbent it replaces.** The vectors both are held to live
//! in `test_corpora/common/projection-vectors.json` — one description that two languages read —
//! and [`tests::agrees_with_the_python_reference`] runs the Python module over a sample and
//! requires the same stored position, not merely a similar number.

use std::f64::consts::PI;

/// The latitude at which Web Mercator's projected northing reaches half the projected world:
/// `degrees(2·atan(exp(π)) − π/2)`. Beyond it the world is no longer square, so every tile scheme
/// cuts here.
///
/// A literal rather than a computation because `atan` and `exp` are not `const`; it is the exact
/// `f64` that expression produces, pinned by [`tests::max_latitude_is_the_computed_one`].
pub const WEB_MERCATOR_MAX_LATITUDE_DEG: f64 = 85.0511287798066;

/// A view's projection, from the closed set of `projections.md` §5.
///
/// The set is closed and stays cylindrical: longitude maps linearly to x and latitude
/// monotonically to y, which is what lets an extent be written in degrees (§4.2). A conic or an
/// azimuthal entry would break that, and neither is in the set.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Projection {
    /// The pseudo-Mercator of every slippy-map tile scheme (§5.1). Conformal to about 0.7% — it
    /// puts a geodetic latitude through the spherical formula with the ellipsoid's semi-major
    /// axis as the sphere radius — which is the one every basemap uses, and the only entry here
    /// under which a drawn circle is a circle on the ground.
    WebMercator,

    /// Equidistant cylindrical: latitude and longitude used directly as coordinates (§5.2). No
    /// transcendental function appears in the transform, so it is bit-exact on every platform,
    /// and unlike Web Mercator it reaches the poles.
    ///
    /// **The standard parallel does not appear in the transform.** Normalised to the unit square
    /// every member of the family stores *identical* positions, and φ₁ survives only as
    /// [`Projection::world_aspect`] — the shape a client draws the world at. It is carried on the
    /// variant because the name a caller writes chooses it, not because the arithmetic reads it;
    /// a build in which it reached a stored coordinate would have made the parallel part of the
    /// format, which is precisely what makes this family one entry rather than five.
    Equirectangular {
        /// φ₁ in degrees.
        standard_parallel_deg: f64,
    },

    /// No projection (§5.3). Coordinates are whatever produced them — an embedding layout, a
    /// synthetic corpus, any space that is not the Earth — the axes are `x` and `y`, and no
    /// basemap or inversion is offered. The default, so a corpus with no geography is never asked
    /// to name a projection.
    None,
}

impl Projection {
    /// `plate_carree`: equirectangular at φ₁ = 0°, a 2:1 world.
    pub const PLATE_CARREE: Self = Projection::Equirectangular {
        standard_parallel_deg: 0.0,
    };

    /// `gall_isographic`: equirectangular at φ₁ = 45°, a √2:1 world.
    pub const GALL_ISOGRAPHIC: Self = Projection::Equirectangular {
        standard_parallel_deg: 45.0,
    };

    /// The names a declaration may write, resolved. `None` for anything else — the caller decides
    /// what a name outside the set costs, and the configuration surface refuses it.
    ///
    /// `plate_carree`, `gall_isographic` and `equirectangular` are three names for one transform
    /// and differ only in the aspect they publish, per the table in §5.2.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "web_mercator" => Some(Projection::WebMercator),
            "equirectangular" | "plate_carree" => Some(Projection::PLATE_CARREE),
            "gall_isographic" => Some(Projection::GALL_ISOGRAPHIC),
            "none" => Some(Projection::None),
            _ => Option::None,
        }
    }

    /// The name a declaration writes for this projection — [`Projection::from_name`]'s inverse.
    ///
    /// `plate_carree` and `equirectangular` are one transform *and* one aspect, so both come back
    /// as `equirectangular`; `gall_isographic` is distinguishable because its standard parallel
    /// is, and it is the aspect a client draws the world at.
    pub fn name(&self) -> &'static str {
        match *self {
            Projection::WebMercator => "web_mercator",
            Projection::Equirectangular {
                standard_parallel_deg: 45.0,
            } => "gall_isographic",
            Projection::Equirectangular { .. } => "equirectangular",
            Projection::None => "none",
        }
    }

    /// Longitude and latitude in degrees to the frame: `[0, 1]` on both axes, x east, **y south**.
    ///
    /// Defined for finite inputs. A latitude outside the projection's domain is clipped — moved
    /// onto the edge, never wrapped — and [`Projection::is_clipped`] is how a caller counts that;
    /// a longitude outside ±180° is likewise held at the edge rather than wrapped, since a
    /// wrapped one would place a point on the far side of the world from where it was written.
    ///
    /// **The result is then held inside the frame, which is not the same operation and is not
    /// cosmetic.** [`WEB_MERCATOR_MAX_LATITUDE_DEG`] is itself the output of `atan` and `exp`, so
    /// projecting it recovers π to a relative 1e-16 rather than to the bit, and the projected pole
    /// lands an ULP *outside* `[0, 1]`. Without the hold, every clipped point would then be
    /// reported as clamped — a real number attributed to the wrong cause, in the one report an
    /// operator reads to judge whether the frame is right.
    pub fn forward(&self, lon: f64, lat: f64) -> (f64, f64) {
        match *self {
            Projection::WebMercator => {
                let lat = lat.clamp(
                    -WEB_MERCATOR_MAX_LATITUDE_DEG,
                    WEB_MERCATOR_MAX_LATITUDE_DEG,
                );
                let merc_y = (PI / 4.0 + lat.to_radians() / 2.0).tan().ln();
                hold((lon + 180.0) / 360.0, 0.5 - merc_y / (2.0 * PI))
            }
            Projection::Equirectangular { .. } => {
                let lat = lat.clamp(-90.0, 90.0);
                hold((lon + 180.0) / 360.0, 0.5 - lat / 180.0)
            }
            Projection::None => (lon, lat),
        }
    }

    /// The frame back to longitude and latitude in degrees. The exact inverse of
    /// [`Projection::forward`] for an unclipped input.
    pub fn inverse(&self, x: f64, y: f64) -> (f64, f64) {
        match *self {
            Projection::WebMercator => {
                let merc_y = (0.5 - y) * 2.0 * PI;
                (
                    x * 360.0 - 180.0,
                    (2.0 * merc_y.exp().atan() - PI / 2.0).to_degrees(),
                )
            }
            Projection::Equirectangular { .. } => (x * 360.0 - 180.0, (0.5 - y) * 180.0),
            Projection::None => (x, y),
        }
    }

    /// The largest latitude this projection is defined at. `None` for [`Projection::None`], which
    /// has no latitude at all.
    pub fn max_latitude_deg(&self) -> Option<f64> {
        match *self {
            Projection::WebMercator => Some(WEB_MERCATOR_MAX_LATITUDE_DEG),
            Projection::Equirectangular { .. } => Some(90.0),
            Projection::None => Option::None,
        }
    }

    /// Whether this latitude falls outside the projection's domain, so that
    /// [`Projection::forward`] will move it onto the frame's edge and lose the difference.
    ///
    /// The clamp report cannot answer this — see the clipping paragraph in the module docs — so a
    /// caller that wants the count takes it here, before projecting. A latitude at *exactly* the
    /// domain boundary is inside it.
    pub fn is_clipped(&self, lat: f64) -> bool {
        match self.max_latitude_deg() {
            Some(max) => lat > max || lat < -max,
            Option::None => false,
        }
    }

    /// The shape a client draws this projection's world at, as width ÷ height, or `None` where
    /// there is no world to draw.
    ///
    /// For the equirectangular family this is `2·cos φ₁` and is the *only* place the standard
    /// parallel is observable: 2:1 at the equator, √2:1 at 45°, square at 60°, taller than wide
    /// beyond. Web Mercator's world is square by construction — that is what cutting the domain at
    /// [`WEB_MERCATOR_MAX_LATITUDE_DEG`] buys.
    pub fn world_aspect(&self) -> Option<f64> {
        match *self {
            Projection::WebMercator => Some(1.0),
            Projection::Equirectangular {
                standard_parallel_deg,
            } => Some(2.0 * standard_parallel_deg.to_radians().cos()),
            Projection::None => Option::None,
        }
    }
}

/// Hold a projected pair inside the unit square. See [`Projection::forward`] for why this is
/// separate from the domain clip and why both are needed.
fn hold(x: f64, y: f64) -> (f64, f64) {
    (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// The vectors are compiled in rather than read at runtime: moving or renaming the file then
    /// fails the build loudly instead of skipping the test that reads it.
    const VECTORS_JSON: &str = include_str!("../../../test_corpora/common/projection-vectors.json");

    /// The repository root, from this crate's own location — the Python reference is outside the
    /// Cargo workspace and there is no other way to reach it.
    const REPO_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

    fn vectors() -> Value {
        serde_json::from_str(VECTORS_JSON).expect("projection-vectors.json parses")
    }

    fn f(v: &Value, key: &str) -> f64 {
        v.get(key)
            .and_then(Value::as_f64)
            .unwrap_or_else(|| panic!("vector has no numeric {key}: {v}"))
    }

    /// A deterministic generator, so a sample that passes today is the same sample tomorrow and a
    /// failure is reproducible from the seed alone. SplitMix64, which needs no dependency.
    struct Rng(u64);

    impl Rng {
        /// A uniform draw from `[lo, hi)`.
        fn range(&mut self, lo: f64, hi: f64) -> f64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            lo + (z >> 11) as f64 / (1u64 << 53) as f64 * (hi - lo)
        }
    }

    #[test]
    fn max_latitude_is_the_computed_one() {
        let computed = (2.0 * PI.exp().atan() - PI / 2.0).to_degrees();
        assert_eq!(
            WEB_MERCATOR_MAX_LATITUDE_DEG, computed,
            "the literal must be the exact f64 the derivation produces"
        );
        let file = f(&vectors()["constants"], "max_latitude_deg");
        assert_eq!(WEB_MERCATOR_MAX_LATITUDE_DEG, file);
        let half = f(&vectors()["constants"], "world_half_m");
        assert_eq!(half, PI * f(&vectors()["constants"], "earth_radius_m"));
    }

    /// Published Web Mercator figures in EPSG:3857 metres, normalised to the unit square by the
    /// affine map the file states. None of these values came from this code.
    ///
    /// The tolerance is on the metre figure, so `1e-6` is a micrometre on the ground: far tighter
    /// than the 9.3 mm the finest frame §4.1 permits can even represent.
    #[test]
    fn published_web_mercator_figures() {
        let v = vectors();
        let half = f(&v["constants"], "world_half_m");
        let mut checked = 0;
        for vec in v["web_mercator_metres"]["vectors"].as_array().unwrap() {
            let (lon, lat) = (f(vec, "lon"), f(vec, "lat"));
            let (want_x, want_y) = (f(vec, "x_m"), f(vec, "y_m"));
            let (got_x, got_y) = Projection::WebMercator.forward(lon, lat);
            let (got_x, got_y) = (got_x * 2.0 * half - half, got_y * 2.0 * half - half);
            assert!(
                (got_x - want_x).abs() < 1e-6 && (got_y - want_y).abs() < 1e-6,
                "({lon}, {lat}): got ({got_x}, {got_y}) metres, published ({want_x}, {want_y})"
            );
            checked += 1;
        }
        assert!(checked >= 8, "only {checked} vectors read — the file has at least eight");
    }

    /// XYZ tile addresses at zoom 1, 2, 10 and 16.
    ///
    /// **The test of the y direction.** A frame mirrored north-south round-trips perfectly and
    /// agrees on every vector whose latitude is zero, while putting London in Antarctica. Two of
    /// the published metre figures do catch a mirror, because they are off the equator; a tile
    /// address catches it whatever the vector set happens to hold, and says which way is north in
    /// the vocabulary a basemap uses.
    #[test]
    fn xyz_tile_addresses() {
        let v = vectors();
        let mut zooms: Vec<u32> = Vec::new();
        for vec in v["xyz_tiles"]["vectors"].as_array().unwrap() {
            let (lon, lat) = (f(vec, "lon"), f(vec, "lat"));
            let zoom = vec["zoom"].as_u64().unwrap() as u32;
            let (want_x, want_y) = (
                vec["tile_x"].as_u64().unwrap(),
                vec["tile_y"].as_u64().unwrap(),
            );
            let (x, y) = Projection::WebMercator.forward(lon, lat);
            let scale = f64::from(1u32 << zoom);
            let (got_x, got_y) = ((x * scale) as u64, (y * scale) as u64);
            assert_eq!(
                (got_x, got_y),
                (want_x, want_y),
                "{} at z{zoom}",
                vec["place"].as_str().unwrap()
            );
            if !zooms.contains(&zoom) {
                zooms.push(zoom);
            }
        }
        assert!(zooms.len() >= 3, "several zooms, got {zooms:?}");
    }

    /// The inverse recovers the input across the whole domain, to **1e-12 degrees**.
    ///
    /// That is about 0.1 µm on the ground — five orders below the 9.3 mm cell of the finest frame
    /// §4.1 permits, so an error at this scale cannot move a point between cells. Measured worst
    /// case over this sample is 4.26e-14 degrees, and the Python reference reports the same figure
    /// over its own; 1e-12 is the claim, with the room a different libm needs.
    #[test]
    fn inverse_recovers_the_input() {
        let mut rng = Rng(0x7E55_E4A0_0000_0001);
        let mut worst: f64 = 0.0;
        for _ in 0..200_000 {
            let lon = rng.range(-180.0, 180.0);
            for (proj, lat) in [
                (
                    Projection::WebMercator,
                    rng.range(
                        -WEB_MERCATOR_MAX_LATITUDE_DEG,
                        WEB_MERCATOR_MAX_LATITUDE_DEG,
                    ),
                ),
                (Projection::PLATE_CARREE, rng.range(-90.0, 90.0)),
            ] {
                let (x, y) = proj.forward(lon, lat);
                let (rl, ra) = proj.inverse(x, y);
                worst = worst.max((rl - lon).abs()).max((ra - lat).abs());
            }
        }
        assert!(worst < 1e-12, "worst round-trip error {worst:e} degrees");
    }

    /// Equirectangular is exact arithmetic: no `tan`, `ln`, `exp` or `atan` appears in its
    /// transform, so it produces the same bits on every platform and on every run.
    ///
    /// A test cannot read the source to see that, so it pins the consequence instead: the forward
    /// equals the four-operation closed form *to the bit*, a second evaluation is bit-identical to
    /// the first, and on the dyadic corners — where every step is exactly representable — forward
    /// and inverse are equalities rather than approximations. A transcendental sneaking into the
    /// transform, a `cos φ₁` factor being the one that would, breaks the first of those on the
    /// first sample.
    #[test]
    fn equirectangular_is_exact_arithmetic() {
        let mut rng = Rng(0x9111_1E5A_0000_0002);
        // Every member of the family, not just the one whose parallel is 0°: a `cos φ₁` factor in
        // the transform is invisible at φ₁ = 0, where the cosine is exactly 1.
        for proj in [Projection::PLATE_CARREE, Projection::GALL_ISOGRAPHIC] {
            for _ in 0..100_000 {
                let lon = rng.range(-180.0, 180.0);
                let lat = rng.range(-90.0, 90.0);
                let (x, y) = proj.forward(lon, lat);
                assert_eq!(x.to_bits(), ((lon + 180.0) / 360.0).to_bits());
                assert_eq!(y.to_bits(), (0.5 - lat / 180.0).to_bits());
                let (x2, y2) = proj.forward(lon, lat);
                assert_eq!((x.to_bits(), y.to_bits()), (x2.to_bits(), y2.to_bits()));
                let (rl, ra) = proj.inverse(x, y);
                assert!((rl - lon).abs() < 1e-12 && (ra - lat).abs() < 1e-12);
            }
        }

        // Exact on the dyadic rationals, where every step of the transform is representable.
        for (lon, lat, want) in [
            (-180.0, 90.0, (0.0, 0.0)),
            (0.0, 0.0, (0.5, 0.5)),
            (180.0, -90.0, (1.0, 1.0)),
            (-90.0, 45.0, (0.25, 0.25)),
            (90.0, -45.0, (0.75, 0.75)),
        ] {
            for proj in [Projection::PLATE_CARREE, Projection::GALL_ISOGRAPHIC] {
                assert_eq!(proj.forward(lon, lat), want);
                assert_eq!(proj.inverse(want.0, want.1), (lon, lat));
            }
        }
    }

    /// The three names are one transform and differ only in the aspect they publish (§5.2).
    #[test]
    fn the_aliases_are_one_transform() {
        let names = ["equirectangular", "plate_carree", "gall_isographic"];
        let mut rng = Rng(0xA11A_5E50_0000_0003);
        for _ in 0..10_000 {
            let (lon, lat) = (rng.range(-180.0, 180.0), rng.range(-90.0, 90.0));
            let first = Projection::from_name(names[0]).unwrap().forward(lon, lat);
            for name in &names[1..] {
                let got = Projection::from_name(name).unwrap().forward(lon, lat);
                assert_eq!(
                    (got.0.to_bits(), got.1.to_bits()),
                    (first.0.to_bits(), first.1.to_bits()),
                    "{name} stores a different position from {}",
                    names[0]
                );
            }
        }

        let aspect = |n: &str| Projection::from_name(n).unwrap().world_aspect().unwrap();
        assert_eq!(aspect("plate_carree"), 2.0);
        assert_eq!(aspect("equirectangular"), 2.0);
        assert!((aspect("gall_isographic") - 2.0_f64.sqrt()).abs() < 1e-15);
        assert_eq!(Projection::WebMercator.world_aspect(), Some(1.0));
        assert_eq!(Projection::None.world_aspect(), Option::None);
        assert_eq!(Projection::from_name("lambert_cylindrical"), Option::None);

        // `name` round-trips through `from_name` for every name the surface accepts, which is
        // what lets a declaration be echoed back to a caller from the resolved projection alone.
        for name in ["web_mercator", "equirectangular", "gall_isographic", "none"] {
            assert_eq!(Projection::from_name(name).unwrap().name(), name);
        }
        assert_eq!(Projection::PLATE_CARREE.name(), "equirectangular");
    }

    /// The clip predicate at the boundary and past it, and the reason it cannot be a clamp count.
    ///
    /// A point at exactly ±[`WEB_MERCATOR_MAX_LATITUDE_DEG`] is inside the domain. A point beyond
    /// it lands on the frame's edge, where `contracts.md` §2.5 says `v = max` is in the top cell
    /// and `v = min` in cell zero — neither clamped. That is the whole argument for a separate
    /// predicate, and this asserts both halves of it: clipped, and invisible to the clamp rule.
    #[test]
    fn clipping_is_not_clamping() {
        let m = WEB_MERCATOR_MAX_LATITUDE_DEG;
        let wm = Projection::WebMercator;
        assert!(!wm.is_clipped(m) && !wm.is_clipped(-m), "the boundary is inside");
        assert!(!wm.is_clipped(0.0));
        for lat in [90.0, -90.0, 85.06, -85.06, f64::INFINITY] {
            assert!(wm.is_clipped(lat), "{lat} is beyond ±{m}");
        }
        // The next representable latitude outward is outside; the boundary itself is not.
        assert!(wm.is_clipped(f64::from_bits(m.to_bits() + 1)));

        for lat in [90.0, -90.0, m, -m, 1e300] {
            let (x, y) = wm.forward(0.0, lat);
            assert!((0.0..=1.0).contains(&y), "y = {y} left the frame at lat {lat}");
            assert_eq!(x, 0.5);
        }
        assert_eq!(wm.forward(0.0, 90.0).1, 0.0, "the north pole is the frame's y minimum");
        assert_eq!(wm.forward(0.0, -90.0).1, 1.0, "the south pole is the frame's y maximum");

        // A longitude outside ±180° is held rather than wrapped, for the same reason.
        assert_eq!(wm.forward(400.0, 0.0).0, 1.0);
        assert_eq!(wm.forward(-181.0, 0.0).0, 0.0);

        // Equirectangular reaches the poles, so nothing inside ±90° is ever clipped.
        let eq = Projection::PLATE_CARREE;
        assert!(!eq.is_clipped(90.0) && !eq.is_clipped(-90.0) && !eq.is_clipped(m));
        assert!(eq.is_clipped(90.001) && eq.is_clipped(-90.001));

        // `none` has no latitude and no domain, so nothing is clipped and nothing is transformed.
        assert!(!Projection::None.is_clipped(1e9));
        assert_eq!(Projection::None.max_latitude_deg(), Option::None);
        assert_eq!(Projection::None.forward(-7.5, 1e6), (-7.5, 1e6));
        assert_eq!(Projection::None.inverse(-7.5, 1e6), (-7.5, 1e6));
    }

    /// **The incumbent, over a large sample.** `test_corpora/common/projection.py` placed both
    /// built geographic corpora — 87 million points — so a disagreement is a finding whichever
    /// side is wrong, and the assertion is on the *stored* position rather than on the float: two
    /// implementations that quantise a point to different 32-bit fixed-point coordinates have
    /// placed it in different cells, which is the only difference this system can observe.
    ///
    /// The interpreter is required rather than optional. The repository's gate already runs
    /// `python3 scripts/check-doc-links.py`, so an absent `python3` is a broken checkout and not a
    /// reason to pass a cross-language agreement test without comparing anything.
    #[test]
    fn agrees_with_the_python_reference() {
        const N: usize = 100_000;
        let mut rng = Rng(0x0072_ACE5_0000_0004);
        let mut points = Vec::with_capacity(N);
        let mut input = String::with_capacity(N * 40);
        for _ in 0..N {
            // Drawn past the domain on purpose — about 8% of latitudes beyond ±85.05° and 5% of
            // longitudes beyond ±180°. The clip and the hold are part of the transform, so the two
            // implementations have to agree on where an out-of-domain point lands as much as on
            // where an ordinary one does.
            let lon = rng.range(-190.0, 190.0);
            let lat = rng.range(-92.0, 92.0);
            points.push((lon, lat));
            input.push_str(&format!("{lon:?} {lat:?}\n"));
        }

        let script = r#"
import sys
sys.path.insert(0, sys.argv[1])
from test_corpora.common.projection import project
out = []
for line in sys.stdin:
    lon, lat = line.split()
    x, y = project(float(lon), float(lat), "unit")
    out.append(f"{x!r} {y!r}")
print("\n".join(out))
"#;
        let mut child = Command::new("python3")
            .args(["-c", script, REPO_ROOT])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("python3 must be runnable — the gate already requires it");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "the Python reference did not run");
        let text = String::from_utf8(out.stdout).unwrap();

        let mut worst: f64 = 0.0;
        let mut n = 0;
        for (line, &(lon, lat)) in text.lines().zip(&points) {
            let mut it = line.split_whitespace();
            let px: f64 = it.next().unwrap().parse().unwrap();
            let py: f64 = it.next().unwrap().parse().unwrap();
            let (rx, ry) = Projection::WebMercator.forward(lon, lat);
            worst = worst.max((rx - px).abs()).max((ry - py).abs());
            // The stored form: 32-bit fixed point over the unit frame (`contracts.md` §2.5).
            let q = |v: f64| crate::morton::fixed32(v, 0.0, 1.0);
            assert_eq!(
                (q(rx), q(ry)),
                (q(px), q(py)),
                "({lon}, {lat}) stores differently: rust ({rx:?}, {ry:?}), python ({px:?}, {py:?})"
            );
            n += 1;
        }
        assert_eq!(n, N, "the reference returned {n} of {N} points");
        // Measured, not assumed: over this sample the two agree to the *bit*, on glibc's libm and
        // CPython 3.10 — worst difference exactly zero, clipped points included. The assertion is
        // looser than the measurement on purpose: a different `tan`, `ln` or `atan` may round a
        // last place differently, and 1e-15 of the unit square is still four orders below one step
        // of the 32-bit fixed-point grid, so it cannot move a stored position.
        assert!(worst < 1e-15, "worst float disagreement {worst:e}");
    }
}
