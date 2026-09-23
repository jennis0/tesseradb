//! **What a client is told it is looking at** (`projections.md` §9): `/v1/meta`'s `projection`,
//! `world_aspect`, `tile_scheme` and `tile`, per view.
//!
//! The four are deployment constants derived from the declaration — the same class as the frame
//! beside them, identical for every principal — so what is at stake here is not disclosure but
//! whether a host draws the right thing. One of them decides that on its own.
//!
//! **`tile_scheme` is the field a boolean would get wrong**, and
//! [`an_equirectangular_view_publishes_no_scheme_though_its_frame_is_aligned`] is the case that
//! shows it: an equirectangular frame is a square of a square tiling exactly as a Web Mercator
//! frame is, and no tile server serves that tiling — the published longitude/latitude schemes are
//! 2:1 at their top level. A host reading alignment as availability would draw a Mercator basemap
//! under a corpus that cannot line up with one, which is a wrong map rather than a missing one.

mod common;

use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use serde_json::Value;
use tempfile::TempDir;
use tessera_build::build;
use tessera_spatial::frame::AlignedSquare;
use tessera_spatial::{Bounds, Projection};

/// A points file in **longitude and latitude**, which is the only spelling a projected view reads
/// (`projections.md` §2). The coordinates are a coarse graticule over the whole world, so every
/// frame below holds some of them and none of the builds refuses for want of data.
fn write_lon_lat_points(path: &Path, n: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("lon", DataType::Float64, false),
        Field::new("lat", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let lons: Vec<f64> = ids.iter().map(|e| -180.0 + (e % 360) as f64).collect();
    let lats: Vec<f64> = ids.iter().map(|e| -80.0 + (e % 160) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(lons)),
            Arc::new(Float64Array::from(lats)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// A bundle built under `projection` against `frame`.
///
/// `frame` is what a projected build records — the aligned square a declared box snapped to, over
/// the unit square the transform produces (`projections.md` §4.2) — and is passed outright here so
/// that a test can state the square it means and then compute the tile address it expects by hand.
fn build_projected(out: &Path, tmp: &Path, projection: Projection, frame: Bounds) {
    let points = tmp.join(format!(
        "points-{}.parquet",
        out.display().to_string().len()
    ));
    let pairs = tmp.join(format!("pairs-{}.parquet", out.display().to_string().len()));
    write_lon_lat_points(&points, N_ITEMS);
    write_pairs_n(&pairs, N_ITEMS);
    let args = build_args(
        out,
        vec![tessera_build::ViewArgs {
            projection,
            extent: frame,
            // `lon`/`lat` become the canonical `x`/`y` at the declaration, which is what
            // `compile_projected_fields` does for a `[[view]]` block; built outright here because
            // there is no document around this build.
            point_fields: tessera_build::config::Fields::moved(
                "points",
                [("x", "lon"), ("y", "lat")],
            ),
            ..view_args("s0", &points, AccessInput::relation(&pairs))
        }],
    );
    build(&args).expect("a projected fixture build should succeed");
}

/// `/v1/meta`'s single view, fetched over real HTTP with a real session.
async fn meta_view(bundle_root: &Path, tmp: &Path, tag: &str) -> Value {
    let server = spawn_server(
        bundle_root,
        &tmp.join(format!("cache-{tag}")),
        &tmp.join(format!("wal-{tag}.log")),
    )
    .await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let views = body["views"].as_array().unwrap();
    assert_eq!(views.len(), 1, "the fixture builds one view: {body}");
    views[0].clone()
}

/// The whole world under every entry of the enumerated set, and the alias that is distinguishable
/// only by the aspect it publishes.
///
/// The expectations are the design's own table (`projections.md` §5.2, §9), not the code's: a
/// square world for `web_mercator`, 2:1 for `equirectangular` and `plate_carree` — which are one
/// name on the wire — and √2:1 for `gall_isographic`, whose standard parallel is 45°.
#[tokio::test]
async fn each_projection_publishes_its_own_name_and_world_aspect() {
    let root2 = 2.0_f64.sqrt();
    for (projection, name, aspect, scheme) in [
        (Projection::WebMercator, "web_mercator", 1.0, Some("xyz")),
        (Projection::PLATE_CARREE, "equirectangular", 2.0, None),
        (Projection::GALL_ISOGRAPHIC, "gall_isographic", root2, None),
    ] {
        let tmp = TempDir::new().unwrap();
        let bundle_root = tmp.path().join("bundle");
        build_projected(
            &bundle_root,
            tmp.path(),
            projection,
            AlignedSquare::WORLD.bounds(),
        );
        let view = meta_view(&bundle_root, tmp.path(), name).await;

        assert_eq!(view["projection"], name, "{view}");
        let published = view["world_aspect"].as_f64().unwrap();
        assert!(
            (published - aspect).abs() < 1e-12,
            "{name} publishes world_aspect {published}, expected {aspect}"
        );
        assert_eq!(
            view["tile_scheme"],
            match scheme {
                Some(s) => Value::from(s),
                None => Value::Null,
            },
            "{name}: {view}"
        );
        // The world under a scheme is that scheme's own top tile.
        assert_eq!(
            view["tile"],
            match scheme {
                Some(_) => serde_json::json!({"z": 0, "x": 0, "y": 0}),
                None => Value::Null,
            },
            "{name}: {view}"
        );
    }
}

/// **The case a boolean gets wrong.** An equirectangular frame is aligned — it is the same
/// aligned square a Web Mercator view would publish `xyz` and a tile address for — and it
/// addresses no published scheme, so it publishes none and no tile.
///
/// The frame is the z2 tile (1, 1): `x [0.25, 0.5], y [0.25, 0.5]`, which under
/// `x = (λ+180)/360, y = 0.5 − φ/180` is longitude −90°…0° and latitude 0°…45°. Alignment is
/// asserted directly rather than assumed, because without it this test would pass for the wrong
/// reason — a frame that was not a square would publish nothing under any projection.
#[tokio::test]
async fn an_equirectangular_view_publishes_no_scheme_though_its_frame_is_aligned() {
    let frame = AlignedSquare { z: 2, x: 1, y: 1 }.bounds();
    assert_eq!(
        AlignedSquare::of_bounds(&frame),
        Some(AlignedSquare { z: 2, x: 1, y: 1 }),
        "the premise: this frame is a square of the tiling"
    );

    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_projected(&bundle_root, tmp.path(), Projection::PLATE_CARREE, frame);
    let view = meta_view(&bundle_root, tmp.path(), "equirect-sub").await;

    assert_eq!(view["projection"], "equirectangular");
    assert_eq!(view["world_aspect"], 2.0);
    assert_eq!(
        view["tile_scheme"],
        Value::Null,
        "an aligned equirectangular frame addresses no published scheme — the published \
         longitude/latitude schemes are 2:1 at their top level, so a host that read alignment as \
         availability would draw a Mercator basemap under a corpus that cannot line up: {view}"
    );
    assert_eq!(view["tile"], Value::Null, "{view}");
}

/// A Web Mercator sub-square frame publishes the tile it is.
///
/// **Computed by hand, not recorded.** At zoom 3 the world is 8 tiles per axis, so a tile is
/// `1/8 = 0.125` of the unit square and the tile (5, 2) is `x [0.625, 0.75], y [0.25, 0.375]` —
/// east of the prime meridian (which is `x = 0.5`) and north of the equator (`y = 0.5`), with
/// **y counting south from the north edge**, the XYZ convention. That is the address `/v1/meta`
/// must publish, and the one a `TileLayer` asks a tile server for.
#[tokio::test]
async fn a_web_mercator_sub_square_publishes_the_tile_it_is() {
    let frame = Bounds {
        x_min: 0.625,
        x_max: 0.75,
        y_min: 0.25,
        y_max: 0.375,
    };
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_projected(&bundle_root, tmp.path(), Projection::WebMercator, frame);
    let view = meta_view(&bundle_root, tmp.path(), "mercator-sub").await;

    assert_eq!(view["projection"], "web_mercator");
    assert_eq!(view["world_aspect"], 1.0);
    assert_eq!(view["tile_scheme"], "xyz");
    assert_eq!(
        view["tile"],
        serde_json::json!({"z": 3, "x": 5, "y": 2}),
        "{view}"
    );
}

/// **A view that projects nothing is told so, and the client path is unchanged.** `none` is the
/// default and every corpus built before this document existed is one: the name is published, the
/// scheme is null and there is no tile, and a client must treat that exactly as it treats today's
/// corpora — which is what the frame's presence, and the absence of any other change to the
/// document, says here.
#[tokio::test]
async fn a_none_view_publishes_none_and_no_scheme() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let view = meta_view(&bundle_root, tmp.path(), "none").await;

    assert_eq!(view["projection"], "none");
    assert_eq!(
        view["world_aspect"],
        Value::Null,
        "`none` has no world to draw: {view}"
    );
    assert_eq!(view["tile_scheme"], Value::Null, "{view}");
    assert_eq!(view["tile"], Value::Null, "{view}");
    assert_eq!(view["id"], "s0");
    assert_eq!(view["display_name"], "s0");
}
