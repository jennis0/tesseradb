//! **A shape layer over views whose frames differ** — [decision 0111](../../../docs/decisions/0111-a-shape-spans-projected-views-through-wgs84.md),
//! `polygon-membership.md` §4.3.
//!
//! One `wgs84` boundary declared once and canonicalised **per view**, through each view's own
//! transform and against each view's own extent. Everything here is about the case no fixture
//! carried until now: a layer drawn on views that do *not* share a frame. The single-frame case is
//! `projected_build.rs`'s, and it must not change — one of these tests holds it fixed.

use tessera_spatial::shape::Shape;
use tessera_spatial::{fixed32, AlignedSquare, Bounds, Projection};
use tessera_store::derived::{
    canonical_shapes, check_shape_span, shape_input, ShapeInput, ShapeSpace, ViewFrame,
};
use tessera_types::layer::{ShapeKind, DEFAULT_MAX_SHAPE_VERTICES};

use tessera_build::shapes::{ShapeContext, ShapeReader};

/// A box over Britain, in degrees. Straight edges in the longitude/latitude plane, so it is a
/// `wgs84` declaration on every projected view and means nothing read as frame coordinates.
const UK: [f64; 4] = [-8.0, 50.0, 2.0, 58.0];

/// The whole world in Web Mercator: `[0, 1]` on both axes, the frame `world` quantises against.
fn world() -> ViewFrame {
    ViewFrame::new(
        "world",
        Projection::WebMercator,
        AlignedSquare::WORLD.bounds(),
    )
}

/// A regional frame: the same projection, a **sub-box** of the unit square. A view zoomed on
/// Europe spends its whole 32-bit grid there, which is the whole reason the extent is per view
/// (decision 0040) and the whole reason a shape cannot be canonicalised once for both.
fn europe() -> ViewFrame {
    let (x0, y0) = Projection::WebMercator.forward(-15.0, 62.0);
    let (x1, y1) = Projection::WebMercator.forward(35.0, 35.0);
    ViewFrame::new(
        "europe",
        Projection::WebMercator,
        Bounds {
            x_min: x0,
            x_max: x1,
            y_min: y0,
            y_max: y1,
        },
    )
}

/// A third frame differing in the **projection** rather than the extent: equirectangular over the
/// same unit square. Its `y` is linear in latitude where Mercator's is not, so one boundary is two
/// different canonical forms on two frames a caller could mistake for one.
fn flat() -> ViewFrame {
    ViewFrame::new(
        "flat",
        Projection::Equirectangular {
            standard_parallel_deg: 0.0,
        },
        AlignedSquare::WORLD.bounds(),
    )
}

fn reader(views: Vec<ViewFrame>, default_space: ShapeSpace) -> ShapeReader {
    ShapeReader::new(
        "regions/uk",
        ShapeKind::Bbox,
        ShapeContext {
            views,
            max_vertices: DEFAULT_MAX_SHAPE_VERTICES,
        },
        default_space,
    )
}

/// Where a point in degrees lands on one view's grid — the same two steps the build's geometry
/// pass takes for a point: the view's own transform, then the view's own extent.
fn grid_point(frame: &ViewFrame, lon: f64, lat: f64) -> (u32, u32) {
    let (x, y) = frame.projection.forward(lon, lat);
    (
        fixed32(x, frame.extent.x_min, frame.extent.x_max),
        fixed32(y, frame.extent.y_min, frame.extent.y_max),
    )
}

/// **One declaration, three canonical forms.** The bytes stored under each view's name are that
/// view's own — not the first view's copied — and each equals what canonicalising the shape
/// against that frame alone produces.
#[test]
fn a_wgs84_shape_canonicalises_once_per_view_against_that_views_own_frame() {
    let frames = vec![world(), europe(), flat()];
    let mut reader = reader(frames.clone(), ShapeSpace::Wgs84);
    let shapes = reader
        .row("uk", Some(ShapeInput::Bbox(UK)), None)
        .expect("a `wgs84` box canonicalises on every projected view")
        .expect("the row carries a shape");

    for frame in &frames {
        let alone = canonical_shapes(
            &shape_input(ShapeKind::Bbox, ShapeInput::Bbox(UK)).unwrap(),
            std::slice::from_ref(frame),
            ShapeSpace::Wgs84,
            DEFAULT_MAX_SHAPE_VERTICES,
        )
        .expect("the frame alone canonicalises");
        assert_eq!(
            shapes.for_view(&frame.view).expect("this view's shape"),
            alone.by_view[0].1.as_slice(),
            "view '{}' holds the form its own frame produces",
            frame.view
        );
    }

    // Three frames, three different byte sequences: the extent differs between `world` and
    // `europe`, the projection between `world` and `flat`.
    let bytes: Vec<&[u8]> = frames
        .iter()
        .map(|f| shapes.for_view(&f.view).expect("a shape per view"))
        .collect();
    assert_ne!(bytes[0], bytes[1], "a different extent is a different form");
    assert_ne!(
        bytes[0], bytes[2],
        "a different projection is a different form"
    );
}

/// **Membership is resolved per view, and each view's answer is the direct one.** A point is
/// tested against the shape *this* view holds, at the position *this* view quantised it to — which
/// is what makes two views of one geography able to disagree at all, and what bounds the
/// disagreement to a cell.
#[test]
fn per_view_membership_agrees_with_a_direct_computation_in_each_frame() {
    let frames = vec![world(), europe(), flat()];
    let mut reader = reader(frames.clone(), ShapeSpace::Wgs84);
    let shapes = reader
        .row("uk", Some(ShapeInput::Bbox(UK)), None)
        .unwrap()
        .unwrap();

    // Inside the box, outside it in longitude, outside it in latitude, and two corners.
    let probes = [
        (-3.0, 54.0, true),
        (-3.0, 40.0, false),
        (12.0, 54.0, false),
        (-8.0, 50.0, true),
        (2.0, 58.0, true),
        (-20.0, 54.0, false),
    ];
    for frame in &frames {
        let shape = Shape::decode(shapes.for_view(&frame.view).unwrap()).expect("stored bytes");
        for (lon, lat, inside) in probes {
            // `europe`'s frame does not reach -20°: a point outside a view's extent quantises onto
            // its edge, and the assertion below is about the shape rather than the clamp, so the
            // probe is skipped where the frame cannot hold it.
            let (x, y) = frame.projection.forward(lon, lat);
            let holds = x >= frame.extent.x_min
                && x <= frame.extent.x_max
                && y >= frame.extent.y_min
                && y <= frame.extent.y_max;
            if !holds {
                continue;
            }
            assert_eq!(
                shape.contains(grid_point(frame, lon, lat)),
                inside,
                "view '{}' at ({lon}, {lat})",
                frame.view
            );
        }
    }
}

/// **A shape wholly outside one view's extent is empty there and unaffected elsewhere** — warned,
/// never a refusal (§4.3). The count is per view, because a sum over views cannot say which view
/// the operator's extent is wrong for.
#[test]
fn a_shape_outside_one_views_extent_is_empty_there_published_and_counted_per_view() {
    // A box over New Zealand: inside `world`, and nowhere near `europe`'s frame.
    let antipodes = [166.0, -47.0, 179.0, -34.0];
    let frames = vec![world(), europe()];
    let mut reader = reader(frames.clone(), ShapeSpace::Wgs84);
    let shapes = reader
        .row("nz", Some(ShapeInput::Bbox(antipodes)), None)
        .expect("an out-of-extent shape is not a refusal")
        .expect("it is published all the same");
    assert!(shapes.for_view("europe").is_some(), "published in both views");

    // A point inside it is a member in `world` and in no view whose extent excludes the shape.
    let world_shape = Shape::decode(shapes.for_view("world").unwrap()).unwrap();
    assert!(world_shape.contains(grid_point(&frames[0], 172.0, -41.0)));
    let europe_shape = Shape::decode(shapes.for_view("europe").unwrap()).unwrap();
    assert!(
        europe_shape.bounds().is_none() || !europe_shape.contains(grid_point(&frames[1], 172.0, -41.0)),
        "the shape holds no rows in a view whose extent it lies outside"
    );

    let report = reader.finish(Vec::new());
    let outside = |view: &str| {
        report
            .by_view
            .iter()
            .find(|v| v.view == view)
            .expect("a row per view")
            .outside
    };
    assert_eq!(outside("world"), 0, "inside the world frame");
    assert_eq!(outside("europe"), 1, "outside the regional frame");
    assert_eq!(report.outside, 1, "the layer's total is the sum over views");
}

/// **A layer's views are all projected or all `none`** (decision 0111). Refused, naming both sides
/// — `wgs84` means nothing in an embedding, so no geometry spans the two kinds of space.
#[test]
fn a_layer_mixing_a_projected_and_an_unprojected_view_is_refused() {
    let embedding = ViewFrame::new(
        "quarter:2026-Q1",
        Projection::None,
        Bounds {
            x_min: -40.0,
            x_max: 40.0,
            y_min: -40.0,
            y_max: 40.0,
        },
    );
    let refusal = check_shape_span(&[world(), embedding.clone()], ShapeSpace::Wgs84)
        .expect_err("a projected view and an embedding do not share a kind of space");
    let message = refusal.to_string();
    assert!(message.contains("'world'"), "{message}");
    assert!(message.contains("'quarter:2026-Q1'"), "{message}");
    assert!(message.contains("decision 0111"), "{message}");

    // And the same refusal reaches a row, whatever space it declares: this is a property of the
    // layer, so it is checked before any geometry is read.
    for space in [ShapeSpace::View, ShapeSpace::Wgs84] {
        assert!(check_shape_span(&[world(), embedding.clone()], space).is_err());
    }
}

/// **`view`-space geometry spans only identical frames.** Its coordinates are one frame's, so over
/// two frames it names two different places; `wgs84` is the spelling that spans, and the refusal
/// says so.
#[test]
fn a_view_space_shape_over_unequal_frames_is_refused_and_wgs84_is_not() {
    let refusal = check_shape_span(&[world(), europe()], ShapeSpace::View)
        .expect_err("two frames cannot both be the frame the coordinates are in");
    let message = refusal.to_string();
    assert!(message.contains("'world'"), "{message}");
    assert!(message.contains("'europe'"), "{message}");
    assert!(message.contains("wgs84"), "{message}");

    check_shape_span(&[world(), europe()], ShapeSpace::Wgs84)
        .expect("a `wgs84` shape spans frames, which is the whole of decision 0111");

    // Two views of one group share a frame by construction, so `view` space is legal over them —
    // the case that must not have become harder.
    let same = ViewFrame::new("world_copy", Projection::WebMercator, world().extent);
    check_shape_span(&[world(), same], ShapeSpace::View)
        .expect("identical frames are one frame, whatever they are named");
}

/// **A layer whose views share a frame stores one form under each name, and pays nothing new.**
/// The group case (`views.md` §3.5) held fixed: identical frames in, identical bytes out.
#[test]
fn views_sharing_a_frame_canonicalise_identically() {
    let a = world();
    let b = ViewFrame::new("world_copy", a.projection, a.extent);
    let mut reader = reader(vec![a, b], ShapeSpace::Wgs84);
    let shapes = reader
        .row("uk", Some(ShapeInput::Bbox(UK)), None)
        .unwrap()
        .unwrap();
    assert_eq!(
        shapes.for_view("world").unwrap(),
        shapes.for_view("world_copy").unwrap()
    );
    let report = reader.finish(Vec::new());
    assert!(
        report.by_view.iter().all(|v| v.outside == 0 && v.clipped == 0),
        "nothing to warn about where the frames agree"
    );
}
