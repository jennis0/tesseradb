//! A projected view, built: the transform between the file and the quantiser
//! (`projections.md` §3), the frame it is quantised against (§4), and the clip counter that the
//! clamp counter structurally cannot stand in for (§7).
//!
//! **The addresses here are published, not recorded.** A build under a whole-world `web_mercator`
//! frame makes a 16-bit cell *identically* an XYZ tile at zoom 16, so a place's cell is the tile
//! address every slippy map already agrees on — and a frame mirrored north-south, or one that
//! exchanged the axes, round-trips perfectly while failing every one of them. The figures come
//! from `test_corpora/common/projection-vectors.json`, which is the contract this transform and
//! the Python module that placed the built geographic corpora are both held to.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::config::{frame_view, Extent, Fields, LonLatBox};
use tessera_build::input::{read_points, PointSurvey};
use tessera_spatial::{fixed32, AlignedSquare, Bounds, Projection};

/// A points file with the coordinate columns under the names a projected view reads.
fn write_points(path: &Path, columns: (&str, &str), xs: &[f64], ys: &[f64]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new(columns.0, DataType::Float64, false),
        Field::new(columns.1, DataType::Float64, false),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(UInt64Array::from((0..xs.len() as u64).collect::<Vec<_>>())) as ArrayRef,
            Arc::new(Float64Array::from(xs.to_vec())),
            Arc::new(Float64Array::from(ys.to_vec())),
        ],
    )
    .expect("the fixture batch is well-formed");
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// The `fields` map a projected view compiles to: the geographic names on the canonical axes.
fn geographic() -> Fields {
    Fields::moved("view 'world'", [("x", "lon"), ("y", "lat")])
}

/// The whole of `web_mercator`'s domain, which is the unit square exactly.
fn whole_world() -> Extent {
    Extent::LonLat(LonLatBox {
        lon_min: -180.0,
        lon_max: 180.0,
        lat_min: -tessera_spatial::WEB_MERCATOR_MAX_LATITUDE_DEG,
        lat_max: tessera_spatial::WEB_MERCATOR_MAX_LATITUDE_DEG,
    })
}

/// The cells a file's rows land in, by source id.
fn cells(path: &Path, projection: Projection, extent: &Bounds) -> Vec<(u16, u16)> {
    let mut rows = read_points(path, &geographic(), projection, extent, None).expect("points read");
    rows.sort_by_key(|r| r.source_id);
    rows.iter()
        .map(|r| ((r.qx >> 16) as u16, (r.qy >> 16) as u16))
        .collect()
}

/// **The test that proves the phase.** Seven places, at their published zoom-16 XYZ tile
/// addresses, land in exactly those cells.
///
/// A whole-world `web_mercator` frame is `[0, 1]` on both axes, so a 16-bit cell is
/// `floor(x · 2^16)` — the XYZ tile at zoom 16 by definition. The addresses below are
/// `projection-vectors.json`'s, none of them produced by this code, and they are the only real
/// test of the y direction: a frame mirrored north-south puts London in Antarctica and still
/// round-trips.
#[test]
fn a_projected_build_places_a_place_at_its_published_tile() {
    // place, lon, lat, tile x, tile y at zoom 16.
    const PLACES: &[(&str, f64, f64, u16, u16)] = &[
        ("London", -0.1276, 51.5072, 32744, 21792),
        ("Sydney", 151.2093, -33.8688, 60294, 39327),
        ("Buenos Aires", -58.3816, -34.6037, 22139, 39489),
        ("Tokyo", 139.6917, 35.6895, 58198, 25804),
        ("Nairobi", 36.8219, -1.2921, 39471, 33003),
        ("Reykjavik", -21.8277, 64.1265, 28794, 17425),
        ("Tromso", 18.956, 69.6496, 36218, 14851),
    ];
    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("places.parquet");
    let lons: Vec<f64> = PLACES.iter().map(|p| p.1).collect();
    let lats: Vec<f64> = PLACES.iter().map(|p| p.2).collect();
    write_points(&points, ("lon", "lat"), &lons, &lats);

    let frame = frame_view(
        "world",
        Projection::WebMercator,
        &whole_world(),
        &points,
        &geographic(),
        None,
    )
    .expect("the frame resolves");
    // The domain is the whole unit square, and only the whole world contains it.
    assert_eq!(frame.snap.expect("a projected frame snaps").square, AlignedSquare::WORLD);
    assert_eq!(
        frame.extent,
        Bounds {
            x_min: 0.0,
            x_max: 1.0,
            y_min: 0.0,
            y_max: 1.0
        }
    );
    assert_eq!(frame.clipped(), 0, "no place here is beyond the domain");

    let got = cells(&points, Projection::WebMercator, &frame.extent);
    for (i, (place, ..)) in PLACES.iter().enumerate() {
        assert_eq!(
            got[i],
            (PLACES[i].3, PLACES[i].4),
            "{place} landed in the wrong cell"
        );
    }
}

/// **Clipping is not clamping, and the counters cannot see each other's rows** (§7).
///
/// A latitude beyond `web_mercator`'s ±85.0511287798066° is moved onto the frame's edge — where
/// the quantisation rule says a point is *not* clamped, `v = min` landing in cell 0 and `v = max`
/// in cell 65535. So a corpus of polar rows reports a clip count and a clamp count of zero, which
/// is the case a single counter would have to report as one or the other and get wrong either way.
#[test]
fn clipped_points_are_counted_and_clamped_points_are_not() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("polar.parquet");
    // Two beyond the domain to the north, one to the south, one at the boundary itself (inside),
    // and one ordinary.
    let max = tessera_spatial::WEB_MERCATOR_MAX_LATITUDE_DEG;
    let lons = [0.0, 10.0, -10.0, 20.0, -0.1276];
    let lats = [89.9, 85.06, -87.0, max, 51.5072];
    write_points(&points, ("lon", "lat"), &lons, &lats);

    let frame = frame_view(
        "world",
        Projection::WebMercator,
        &whole_world(),
        &points,
        &geographic(),
        None,
    )
    .expect("the frame resolves");
    assert_eq!(frame.clipped(), 3, "two north of the domain and one south");
    let PointSurvey::Coordinates(survey) = frame.survey else {
        panic!("a coordinate source surveys as coordinates");
    };
    assert_eq!(survey.rows, 5);
    assert_eq!(
        (survey.clamped, survey.clamped_x, survey.clamped_y),
        (0, 0, 0),
        "a clipped point lands exactly where the clamp rule says nothing is clamped"
    );

    // And it lands on the edge rather than where it was written: the north pole is the frame's y
    // minimum and the south pole its maximum, y running south.
    let got = cells(&points, Projection::WebMercator, &frame.extent);
    assert_eq!(got[0].1, 0, "89.9°N is on the frame's northern edge");
    assert_eq!(got[1].1, 0, "85.06°N is too");
    assert_eq!(got[2].1, 65535, "87°S is on the southern edge");
    assert_eq!(got[3].1, 0, "the domain boundary itself is the same cell");
    assert_eq!(got[4], (32744, 21792), "London is untouched by any of it");

    // The build reports and never refuses, at any proportion: three fifths of this corpus is
    // clipped and the frame earns no refusal, because a clipped point's position is the
    // projection's own domain boundary and no choice of frame moves it.
    assert!(frame.refusal().is_none(), "clipping is never a refusal");
}

/// **`auto` is the same snap over the data's own longitude/latitude box** (§4.2).
///
/// Hand-computed under `equirectangular`, whose transform is `x = (lon + 180)/360` and
/// `y = 0.5 - lat/180` and is exact on these values. The data spans lon `[-144, -36]` and lat
/// `[-72, -18]`, so the projected box is `x [0.1, 0.4], y [0.6, 0.9]`. That straddles the z2
/// boundary at `x = 0.25`, and sits inside the z1 tile `(0, 1)` — so the frame is
/// `x [0, 0.5], y [0.5, 1]` and nothing clamps.
#[test]
fn auto_snaps_the_datas_own_lon_lat_box() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("south.parquet");
    let lons = [-144.0, -90.0, -36.0, -100.0];
    let lats = [-72.0, -45.0, -18.0, -30.0];
    write_points(&points, ("lon", "lat"), &lons, &lats);

    let frame = frame_view(
        "world",
        Projection::PLATE_CARREE,
        &Extent::AutoLonLat,
        &points,
        &geographic(),
        None,
    )
    .expect("the frame resolves");
    let snap = frame.snap.expect("a projected frame snaps");
    assert_eq!(snap.square, AlignedSquare { z: 1, x: 0, y: 1 });
    assert!(!snap.floored, "the box constrains the offset, not the cap");
    assert_eq!(
        frame.extent,
        Bounds {
            x_min: 0.0,
            x_max: 0.5,
            y_min: 0.5,
            y_max: 1.0
        }
    );
    // The survey's own box is in the frame's space, and the snapped square contains it — which is
    // why `auto` clamps nothing by construction.
    let PointSurvey::Coordinates(survey) = frame.survey else {
        panic!("a coordinate source surveys as coordinates");
    };
    assert_eq!(
        survey.bounds,
        Some(Bounds {
            x_min: 0.1,
            x_max: 0.4,
            y_min: 0.6,
            y_max: 0.9
        })
    );
    assert_eq!(survey.clamped, 0);
    assert_eq!(survey.clipped, 0, "equirectangular reaches the poles");
}

/// **`auto` over a source selecting no rows is refused**, on the projected spelling as on the
/// other: there is no data to fit a box around, and a default frame would be four numbers nothing
/// justifies.
#[test]
fn auto_over_an_empty_source_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("empty.parquet");
    write_points(&points, ("lon", "lat"), &[], &[]);

    let message = format!(
        "{}",
        frame_view(
            "world",
            Projection::WebMercator,
            &Extent::AutoLonLat,
            &points,
            &geographic(),
            None,
        )
        .expect_err("an empty source frames nothing")
    );
    assert!(message.contains("selects no rows"), "{message}");
    assert!(message.contains("lon = ["), "{message}");
}

/// **A coordinate outside ±180 or ±90 is not a coordinate** (§2), and the refusal names the row.
#[test]
fn a_coordinate_outside_the_wgs84_range_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let refused = |lons: &[f64], lats: &[f64]| {
        let points = tmp.path().join(format!("bad{}.parquet", lons.len() + lats.len()));
        write_points(&points, ("lon", "lat"), lons, lats);
        frame_view(
            "world",
            Projection::WebMercator,
            &whole_world(),
            &points,
            &geographic(),
            None,
        )
        .expect_err("a value outside the range is not a coordinate")
        .to_string()
    };

    // Web Mercator's world is 20,037,508 metres across, and a source in metres reads as a
    // longitude of twenty million — which is the ordinary way this arrives.
    let message = refused(&[0.0, 20_037_508.0], &[0.0, 6_710_219.0]);
    assert!(message.contains("is not a place"), "{message}");
    assert!(message.contains("WGS84"), "{message}");
    assert!(message.contains("entity_id 1"), "{message}");
    assert!(message.contains("projection = \"none\""), "{message}");

    let message = refused(&[0.0, 10.0, 20.0], &[0.0, 95.0, 0.0]);
    assert!(message.contains("lat 95"), "{message}");
}

/// **`projection = "none"` transforms nothing**, and its stored positions are the ones the
/// quantiser produces from the file's own numbers.
///
/// This is the regression the phase turns on: the surface every existing build already uses must
/// behave exactly as it did. Asserted against `fixed32` computed here rather than against a
/// recorded digest, so it says *what* the positions are rather than that they have not moved.
#[test]
fn an_unprojected_view_stores_the_files_own_coordinates() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("plain.parquet");
    let xs = [-17.0, 0.0, 3.5, 17.75];
    let ys = [-20.5, 1.25, 0.0, 22.5];
    write_points(&points, ("x", "y"), &xs, &ys);

    let extent = Bounds {
        x_min: -25.0,
        x_max: 25.0,
        y_min: -25.0,
        y_max: 25.0,
    };
    let frame = frame_view(
        "s0",
        Projection::None,
        &Extent::Fixed(extent),
        &points,
        &Default::default(),
        None,
    )
    .expect("the frame resolves");
    assert_eq!(frame.extent, extent, "an unprojected frame is what was stated");
    assert!(frame.snap.is_none(), "there is nothing to snap");
    assert_eq!(frame.clipped(), 0, "`none` has no domain to clip against");

    let mut rows = read_points(&points, &Default::default(), Projection::None, &extent, None)
        .expect("points read");
    rows.sort_by_key(|r| r.source_id);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            (row.qx, row.qy),
            (
                fixed32(xs[i], extent.x_min, extent.x_max),
                fixed32(ys[i], extent.y_min, extent.y_max)
            ),
            "row {i}"
        );
    }
}
