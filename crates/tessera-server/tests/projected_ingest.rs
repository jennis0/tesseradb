//! **A projected view is ingestable, not merely buildable** (`projections.md` §3).
//!
//! Building a database and adding rows to one are the same operation ([decision
//! 0091](../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md)), so a view that
//! declares a projection has to place a point the same way whichever door the point came through.
//! The build transforms at the read of a points file (`tessera_build::input`); this file is about
//! the other door — the Arrow batch `/control/ingest` decodes, whose coordinate columns are
//! `lon`/`lat`, whose latitudes outside the projection's domain are clipped and counted rather than
//! refused, and whose write-ahead log holds the **frame** coordinates the transform produced.
//!
//! **The order the three checks run in is the subject of half of these tests.** Project, then clip,
//! then ask whether the result is inside the frame — so the engine's out-of-frame refusal sees a
//! coordinate that has been projected and clipped and never an out-of-domain latitude. Reversed, a
//! polar row would be refused for leaving a frame it never reached in the first place, and a build
//! would accept a row an ingest rejected.
//!
//! Nothing here uses `projection = "none"` except
//! [`a_view_with_no_projection_refuses_the_geographic_spelling`], which is the guard on the surface
//! every existing ingest uses: under no projection the columns stay `x` and `y` and the transform
//! is the identity, bit for bit.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{BinaryArray, Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use tempfile::TempDir;

use tessera_build::config::Fields;
use tessera_build::input::deinterleave;
use tessera_build::{build, BuildArgs};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::wal::{Wal, WalRecord};
use tessera_plugin::Passthrough;
use tessera_spatial::{Bounds, Projection, WEB_MERCATOR_MAX_LATITUDE_DEG};

use common::*;

/// The whole-world `web_mercator` frame, which **is** the unit square (`projections.md` §4): every
/// projection's output is normalised to `[0, 1]` on both axes, x east and y south, so a 16-bit
/// cell here is exactly an XYZ tile at zoom 16.
fn world_frame() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
}

/// Places, in longitude and latitude — the order GeoJSON and WKT use, which is the whole reason
/// this view's columns are named for what they hold.
///
/// The last row is **beyond Web Mercator's domain** at 89.5°N. It is in the shared fixture rather
/// than in a test of its own so that every property below is asserted over a set containing one,
/// which is what stops a clipped row being handled correctly in the test written for it and
/// nowhere else.
fn places() -> Vec<(f64, f64)> {
    vec![
        (-0.1276, 51.5072),     // London
        (139.6917, 35.6895),    // Tokyo
        (-74.0060, 40.7128),    // New York
        (151.2093, -33.8688),   // Sydney
        (0.0, 0.0),             // the origin of both axes
        (-180.0, 0.0),          // the western edge of the world
        (18.4241, -33.9249),    // Cape Town
        (-58.3816, -34.6037),   // Buenos Aires
        (37.6173, 55.7558),     // Moscow
        (-155.5828, 19.8968),   // Hawai'i
        (10.0, -85.0511287798), // just inside the southern domain cut
        (10.0, 89.5),           // POLAR: outside the domain, and therefore clipped
    ]
}

/// The one row `places()` clips.
const POLAR: usize = 11;

/// A points file under the names a projected view reads (`projections.md` §2).
fn write_lon_lat_points(path: &Path, points: &[(f64, f64)]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("lon", DataType::Float64, false),
        Field::new("lat", DataType::Float64, false),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(UInt64Array::from(
                (0..points.len() as u64).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                points.iter().map(|p| p.0).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                points.iter().map(|p| p.1).collect::<Vec<_>>(),
            )),
        ],
    )
    .expect("the fixture batch is well-formed");
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// A bundle whose one view is projected `web_mercator` against the whole world.
///
/// `Fields::moved` is what a projected `[[view]]` compiles to — the geographic names carried on the
/// canonical axes — so the build reads `lon`/`lat` here exactly as it would from a declaration.
fn build_projected(out: &Path, tmp: &Path, points: &[(f64, f64)]) {
    let points_path = tmp.join("points.parquet");
    let pairs_path = tmp.join("pairs.parquet");
    write_lon_lat_points(&points_path, points);
    write_pairs_n(&pairs_path, points.len() as u64);
    build(&BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: Projection::WebMercator,
            extent: world_frame(),
            points: points_path,
            point_fields: Fields::moved("view 's0'", [("x", "lon"), ("y", "lat")]),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs_path),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.to_path_buf(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect("the projected fixture builds");
}

/// An ingest batch under caller-chosen column names, so the refusal tests can spell them wrong.
fn ingest_batch(columns: (&str, &str), rows: &[(Vec<u8>, f64, f64, &str)]) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new(columns.0, DataType::Float64, false),
        Field::new(columns.1, DataType::Float64, false),
        Field::new("access", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(BinaryArray::from_iter_values(
                rows.iter().map(|(id, _, _, _)| id.as_slice()),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|(_, x, _, _)| *x).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|(_, _, y, _)| *y).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|(_, _, _, a)| *a),
            )),
        ],
    )
    .unwrap();
    let mut w = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    w.write(&batch).unwrap();
    w.into_inner().unwrap()
}

/// The ingested-row external id for source `i`, distinct from the build's own 8-byte spelling so
/// both copies of a place are addressable.
fn ingested_id(i: usize) -> Vec<u8> {
    format!("ingested-{i}").into_bytes()
}

/// `POST /control/ingest`, returning the status and the decoded body.
async fn post_ingest(
    server: &TestServer,
    batch_id: &str,
    body: Vec<u8>,
) -> (reqwest::StatusCode, serde_json::Value) {
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let json = resp.json::<serde_json::Value>().await.unwrap_or_default();
    (status, json)
}

/// `POST /control/flush`, waited out — a buffered row has no geometry until it is flushed, so
/// nothing below can read a position without this.
async fn flush(server: &TestServer) {
    let before = server.state.engine.write_executor_stats().flushes;
    let resp = server
        .client
        .post(server.control_url("/control/flush"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    wait_for_flush(&server.state.engine, before);
}

fn wait_for_flush(engine: &Engine, before: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().flushes == before {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Every served point's 64-bit position, by `tessera_id`.
fn served_positions(engine: &Engine, k: usize) -> BTreeMap<u64, u64> {
    let session = engine
        .authorise(br#"{"terms": ["0"]}"#)
        .expect("the fixture's pairs grant term 0 to every row");
    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1.0, 1.0], k),
        )
        .expect("the viewport answers over the whole frame");
    out.points
        .iter()
        .map(|(id, code)| (id.raw(), code))
        .collect()
}

/// The position an external id's row was placed at, or a panic naming the id.
fn position_of(engine: &Engine, positions: &BTreeMap<u64, u64>, external_id: &[u8]) -> u64 {
    let entity = engine
        .resolve_external_ids(std::slice::from_ref(&external_id.to_vec()))
        .expect("the sidecar and the live map both answer")[0]
        .expect("this external id was established by a build or an accepted batch");
    let id = engine
        .tessera_id_of(entity)
        .expect("a live entity has an identifier");
    *positions.get(&id.raw()).unwrap_or_else(|| {
        panic!(
            "the row for external id {} was not served",
            String::from_utf8_lossy(external_id)
        )
    })
}

/// An engine config that flushes only when asked, so every position below is read after a flush
/// this test caused.
fn config() -> EngineConfig {
    EngineConfig {
        flush_max_age_secs: 3600,
        // **The row trigger off.** This cell drives publication itself — it pins `B`
        // by flushing and waiting, so a trigger that published on its own would
        // measure a different buffer depth than the one the sweep set.
        flush_max_items: usize::MAX,
        ..default_engine_config()
    }
}

/// **The test that proves the phase**, and it is decision 0091's own: the same rows reached by a
/// build and by an ingest into the database that build produced place every point in the same cell,
/// **for a projected view**.
///
/// The equivalent test in `tessera-engine`'s `coordinate_precision.rs` runs before any projection
/// exists and proves only that the two routes carry the same width. This one is the case that
/// matters, because the two routes now run *different code* to reach the quantiser:
/// `tessera_build::input` projects at the read of a Parquet file, and `/control/ingest` projects at
/// the decode of an Arrow batch, on the other side of a wire, a WAL record and the ingest buffer.
/// A projection applied on one side and not the other is invisible in every count and every type
/// signature, and shows up only as a point in the wrong hemisphere.
#[tokio::test]
async fn a_build_and_an_ingest_place_a_projected_coordinate_in_one_cell() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let points = places();
    build_projected(&root, tmp.path(), &points);

    let server = spawn_server_with_config(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        config(),
    )
    .await;

    let rows: Vec<(Vec<u8>, f64, f64, &str)> = points
        .iter()
        .enumerate()
        .map(|(i, (lon, lat))| (ingested_id(i), *lon, *lat, "0"))
        .collect();
    let (status, body) = post_ingest(&server, "batch-1", ingest_batch(("lon", "lat"), &rows)).await;
    assert_eq!(status, 200, "the projected batch is accepted: {body}");
    assert_eq!(body["accepted"], points.len());
    flush(&server).await;

    let positions = served_positions(&server.state.engine, 400);
    assert_eq!(
        positions.len(),
        points.len() * 2,
        "both copies of every place must be served, or the comparison below is vacuous"
    );
    for (i, (lon, lat)) in points.iter().enumerate() {
        let from_build = position_of(&server.state.engine, &positions, &external_id_of(i as u64));
        let from_ingest = position_of(&server.state.engine, &positions, &ingested_id(i));
        assert_eq!(
            from_build, from_ingest,
            "lon {lon}, lat {lat} was placed differently by the build and the ingest"
        );
    }

    // The fixture is a real test of the transform rather than of two identities: an unprojected
    // path would put London's -0.1276 outside the [0, 1] frame entirely, and every place would
    // share one cell.
    let cells: std::collections::BTreeSet<u64> =
        positions.values().map(|code| code >> 32).collect();
    assert!(
        cells.len() >= points.len() - 1,
        "the places must occupy distinct cells — {} cells for {} places means the transform did \
         not run",
        cells.len(),
        points.len()
    );
}

/// **A latitude outside the projection's domain is clipped and counted, never refused** — and the
/// build accepts the same row, so the two doors agree about it (`projections.md` §3, §7).
///
/// The three assertions are three separate ways this could go wrong. The response's `clipped`
/// count is the only report such a row gets on this path, the build's frame report being the only
/// other one and a batch having no build. The stored position is the frame's northern edge, which
/// is *not* where the row was written and is the data loss the count exists to name. And the built
/// copy is in the same cell, which is what makes the clip a property of the projection rather than
/// of the door.
///
/// **The clamp counter cannot stand in for any of this.** A clipped point lands exactly on the
/// frame's edge, which is where the quantisation rule says a point is *not* out of frame — so the
/// engine's out-of-frame refusal, which runs after this transform, structurally cannot see one at
/// the whole-world frame.
#[tokio::test]
async fn a_polar_row_is_clipped_counted_and_lands_on_the_frames_edge() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let points = places();
    build_projected(&root, tmp.path(), &points);

    let server = spawn_server_with_config(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        config(),
    )
    .await;

    let (lon, lat) = points[POLAR];
    assert!(
        lat > WEB_MERCATOR_MAX_LATITUDE_DEG,
        "the fixture row must actually be outside the domain, or this test asserts nothing"
    );
    let rows = vec![(ingested_id(POLAR), lon, lat, "0")];
    let (status, body) = post_ingest(&server, "polar", ingest_batch(("lon", "lat"), &rows)).await;
    assert_eq!(
        status, 200,
        "a clipped row is accepted, never refused: {body}"
    );
    assert_eq!(body["accepted"], 1);
    assert_eq!(
        body["clipped"], 1,
        "the response carries the clip count beside the out-of-bound count it already returns"
    );
    flush(&server).await;

    let positions = served_positions(&server.state.engine, 400);
    let ingested = position_of(&server.state.engine, &positions, &ingested_id(POLAR));
    let built = position_of(
        &server.state.engine,
        &positions,
        &external_id_of(POLAR as u64),
    );
    assert_eq!(
        ingested, built,
        "the same polar row through the two doors must land in the same cell"
    );
    // y runs SOUTH, so the northern domain cut is y = 0 — cell 0, the frame's edge, and not the
    // latitude the caller wrote. The top 32 bits of a position are the two axes' cells,
    // interleaved.
    let (_, cell_y) = deinterleave((ingested >> 32) as u32);
    assert_eq!(
        cell_y, 0,
        "a clipped northern row is stored on the frame's northern edge"
    );
}

/// A batch spelling its coordinate columns `x`/`y` against a **projected** view is refused, naming
/// the spelling this view uses (`projections.md` §2).
///
/// The protection this is half of exists at the build already: a projected `[[view]]` refuses
/// `fields.x`. Without it here, a projected view would be something that could be built correctly
/// and ingested into wrongly — and the failure is silent, a corpus with longitude and latitude
/// exchanged being mirrored about the diagonal rather than malformed.
#[tokio::test]
async fn a_projected_view_refuses_the_cartesian_spelling() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_projected(&root, tmp.path(), &places());
    let server = spawn_server_with_config(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        config(),
    )
    .await;

    let rows = vec![(ingested_id(0), 0.0, 0.0, "0")];
    let (status, body) = post_ingest(&server, "wrong", ingest_batch(("x", "y"), &rows)).await;
    assert_eq!(status, 422, "the wrong spelling is a contract refusal");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("'lon'") && detail.contains("'lat'"),
        "the refusal must name the spelling to use, not merely the one that was wrong: {detail}"
    );
}

/// A batch spelling its coordinate columns `lon`/`lat` against a view with **no** projection is
/// refused, naming `x`/`y` (`projections.md` §5.3: under no projection there is no longitude).
///
/// This is also the regression guard on the surface every existing ingest uses: the fixture here is
/// the unprojected one every other test in this crate builds.
#[tokio::test]
async fn a_view_with_no_projection_refuses_the_geographic_spelling() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server_with_config(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        config(),
    )
    .await;

    let rows = vec![(ingested_id(0), 10.0, 10.0, "0")];
    let (status, body) = post_ingest(&server, "wrong", ingest_batch(("lon", "lat"), &rows)).await;
    assert_eq!(status, 422, "the wrong spelling is a contract refusal");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("'x'") && detail.contains("'y'"),
        "the refusal must name the spelling to use: {detail}"
    );
}

/// **The write-ahead log holds frame coordinates, so replay reproduces the positions the original
/// write produced rather than re-running the transform** (`projections.md` §3).
///
/// Two halves, and the first is what makes the second mean something. The log is read directly and
/// every stored pair is the *projection's output*, bit for bit — not the degrees the caller sent.
/// Then a second engine is opened on a copy of that log and of the bundle as it stood before the
/// flush; it replays, and places every row exactly where the transform put it.
///
/// **Nothing in recovery calls `Projection::forward`, and that is the point.** Web Mercator
/// composes a logarithm and a tangent and is not bit-exact across C libraries (§11), so a log
/// holding degrees would make a platform's floating-point library part of recovery — a node could
/// come back up with points in different cells from the ones it acked, with nothing to notice.
#[tokio::test]
async fn replay_reproduces_the_stored_positions_without_re_running_the_transform() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let points = places();
    build_projected(&root, tmp.path(), &points);

    let wal_dir = tmp.path().join("wal");
    std::fs::create_dir_all(&wal_dir).unwrap();
    let server = spawn_server_with_config(
        &root,
        &tmp.path().join("cache"),
        &wal_dir.join("wal.log"),
        config(),
    )
    .await;

    let rows: Vec<(Vec<u8>, f64, f64, &str)> = points
        .iter()
        .enumerate()
        .map(|(i, (lon, lat))| (ingested_id(i), *lon, *lat, "0"))
        .collect();
    let (status, body) = post_ingest(&server, "batch-1", ingest_batch(("lon", "lat"), &rows)).await;
    assert_eq!(status, 200, "{body}");

    // Copied rather than read in place: the running server holds a live handle on this sequence,
    // and `Wal::open` issues one of its own. The batch was acked, so it is fsynced, so the copy is
    // complete.
    let replay_dir = tmp.path().join("wal-copy");
    copy_dir(&wal_dir, &replay_dir);

    // ---- half one: what the log actually holds.
    let (_wal, records) = Wal::open(replay_dir.join("wal.log")).expect("the copied log opens");
    let logged: Vec<(f64, f64)> = records
        .iter()
        .filter_map(|r| match r {
            WalRecord::IngestBatch { rows, .. } => Some(rows),
            _ => None,
        })
        .flatten()
        .map(|row| (row.x, row.y))
        .collect();
    assert_eq!(logged.len(), points.len(), "every row reached the log");
    for ((lon, lat), (x, y)) in points.iter().zip(&logged) {
        let (px, py) = Projection::WebMercator.forward(*lon, *lat);
        assert_eq!(
            (*x, *y),
            (px, py),
            "the log holds the frame coordinate for lon {lon}, lat {lat}, not the degrees"
        );
        assert!(
            (0.0..=1.0).contains(x) && (0.0..=1.0).contains(y),
            "a logged position is inside the frame; ({x}, {y}) is a degree that was never \
             transformed"
        );
    }

    // ---- half two: a restart places them there.
    let replay_root = tmp.path().join("bundle-copy");
    copy_dir(&root, &replay_root);
    let mut replayed = Engine::open(
        &replay_root,
        &tmp.path().join("cache-copy"),
        &replay_dir.join("wal.log"),
        Passthrough::new(),
        config(),
    )
    .expect("the engine opens against the copied bundle and replays the copied log");
    replayed
        .start_write_executor(64)
        .expect("the executor starts once");
    let before = replayed.write_executor_stats().flushes;
    replayed.request_flush();
    wait_for_flush(&replayed, before);

    flush(&server).await;
    let live = served_positions(&server.state.engine, 400);
    let after_restart = served_positions(&replayed, 400);
    for (i, (lon, lat)) in points.iter().enumerate() {
        let id = ingested_id(i);
        assert_eq!(
            position_of(&replayed, &after_restart, &id),
            position_of(&server.state.engine, &live, &id),
            "replay moved lon {lon}, lat {lat}"
        );
    }
}

/// A shallow recursive copy — enough for a bundle directory and a WAL sequence, and small enough
/// that reaching for a dependency would cost more than it saved.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
