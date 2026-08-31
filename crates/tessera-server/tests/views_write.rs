//! **A view group grows while the service runs** (`views.md` §3.2, §3.4; decision 0108): the
//! create operation, the drop, and what each does to the roster, to the row space and to a
//! restart.
//!
//! What is at stake here is not that a *declaration* becomes row spaces — `tessera-build`'s own
//! tests cover that — but the half above it, which has three ways of being wrong while every
//! build test passes:
//!
//! - **A created view is a view.** It answers a viewer verb the moment it exists, empty, and
//!   serves its own rows after the first flush. A view a caller can create and cannot address is
//!   a 404 with an acknowledgement in front of it.
//! - **The roster survives a restart.** The WAL carries the create for replay and rotation
//!   reclaims it, so the *segments manifest* is the durable home — a roster that lived only in
//!   the log comes back missing, and the next create then reissues an ordinal a live view holds.
//! - **A key and an ordinal are burnt by a drop.** A recreated key with different contents would
//!   silently repoint every bookmark and every client cache keyed on the view (decision 0029).
//!
//! The fixture is built here rather than taken from `test_corpora/` because these tests assert the
//! shapes the declaration states, and a synthetic corpus states them with no data dependency —
//! `multiview_serving.rs`' reason, and its shape.

mod common;

use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_build::{
    build, BuildArgs, GroupDescriptor, GroupMetadataField, GroupViewDescriptor, Quantisation,
    ViewArgs, ViewMetadataType, ViewMetadataValue,
};

const ENTITIES: u64 = 20;
/// The plain view holds every entity; the group's one declared view holds the first half. The
/// overlap is what makes `delete_dangling` a question with two answers.
const WORLD: std::ops::Range<u64> = 0..ENTITIES;
const Q1: std::ops::Range<u64> = 0..10;

fn position(view: &str, e: u64) -> (f64, f64) {
    match view.split_once(':') {
        None => ((e % 5) as f64 * 100.0, (e / 5) as f64 * 100.0),
        Some(_) => (900.0 - (e % 5) as f64 * 100.0, (e / 5) as f64 * 70.0),
    }
}

fn write_points(path: &Path, view: &str, ids: std::ops::Range<u64>) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = ids.collect();
    let xs: Vec<f64> = ids.iter().map(|&e| position(view, e).0).collect();
    let ys: Vec<f64> = ids.iter().map(|&e| position(view, e).1).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn view_args(view: &str, points: &Path, pairs: &Path) -> ViewArgs {
    ViewArgs {
        view_id: view.to_string(),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: points.to_path_buf(),
        point_fields: Default::default(),
        select: None,
        access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
    }
}

fn group_frame() -> Quantisation {
    let e = extent();
    Quantisation {
        x_min: e.x_min,
        x_max: e.x_max,
        y_min: e.y_min,
        y_max: e.y_max,
    }
}

/// One plain view, a group of one view carrying two metadata names, and a second group over the
/// same keys — the three shapes a create has to get right (`views.md` §3.1, §3.3).
fn build_fixture_bundle(dir: &Path) -> std::path::PathBuf {
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, ENTITIES);
    let world_points = dir.join("world.parquet");
    write_points(&world_points, "world", WORLD);
    let mut views = vec![view_args("world", &world_points, &pairs)];
    for group in ["quarter", "quarter_map"] {
        let id = format!("{group}:2026-Q1");
        let points = dir.join(format!("{group}-q1.parquet"));
        write_points(&points, &id, Q1);
        views.push(view_args(&id, &points, &pairs));
    }
    let roster = |with_metadata: bool| {
        vec![GroupViewDescriptor {
            key: "2026-Q1".to_string(),
            ordinal: 0,
            visibility: None,
            metadata: if with_metadata {
                [
                    (
                        "label".to_string(),
                        ViewMetadataValue::Text("Q1 2026".to_string()),
                    ),
                    (
                        "starts".to_string(),
                        ViewMetadataValue::TimestampUs(1_767_225_600_000_000),
                    ),
                ]
                .into_iter()
                .collect()
            } else {
                Default::default()
            },
        }]
    };
    let out = dir.join("bundle");
    build(&BuildArgs {
        views,
        anchor: 0,
        groups: vec![
            GroupDescriptor {
                name: "quarter".to_string(),
                members_of: None,
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                metadata: vec![
                    GroupMetadataField {
                        name: "label".to_string(),
                        ty: ViewMetadataType::Text,
                        vocabulary: None,
                    },
                    GroupMetadataField {
                        name: "starts".to_string(),
                        ty: ViewMetadataType::TimestampUs,
                        vocabulary: None,
                    },
                ],
                views: roster(true),
            },
            GroupDescriptor {
                name: "quarter_map".to_string(),
                members_of: Some("quarter".to_string()),
                quantisation: group_frame(),
                projection: tessera_spatial::Projection::None,
                metadata: Vec::new(),
                views: roster(false),
            },
        ],
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect("a three-view build succeeds");
    out
}

struct Served {
    server: TestServer,
    token: String,
    tmp: TempDir,
}

async fn serve() -> Served {
    let tmp = TempDir::new().unwrap();
    let bundle = build_fixture_bundle(tmp.path());
    let served = open(tmp).await;
    assert!(bundle.exists());
    served
}

async fn open(tmp: TempDir) -> Served {
    let server = spawn_server(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let auth = authorise(&server, &["0", "1"]).await;
    let token = auth["token"].as_str().unwrap().to_string();
    Served { server, token, tmp }
}

/// Reopen the same bundle and the same WAL — the restart every durability claim below is made
/// against. The old server is dropped first so its executor releases the log.
async fn restart(served: Served) -> Served {
    let Served { server, tmp, .. } = served;
    drop(server);
    open(tmp).await
}

async fn create(served: &Served, group: &str, key: &str, record: Value) -> reqwest::Response {
    served
        .server
        .client
        .put(
            served
                .server
                .control_url(&format!("/control/views/{group}/{key}")),
        )
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&record)
        .send()
        .await
        .unwrap()
}

/// The record the fixture's `quarter` group declares: both names, typed.
fn q_record(label: &str, starts: i64) -> Value {
    json!({ "metadata": { "label": label, "starts": starts } })
}

async fn drop_view(served: &Served, group: &str, key: &str, delete_dangling: bool) -> Value {
    let resp = served
        .server
        .client
        .delete(served.server.control_url(&format!(
            "/control/views/{group}/{key}?delete_dangling={delete_dangling}"
        )))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "a drop of a live view is accepted");
    resp.json().await.unwrap()
}

async fn meta(served: &Served) -> Value {
    let resp = served
        .server
        .client
        .get(served.server.viewer_url("/v1/meta"))
        .bearer_auth(&served.token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

async fn viewport(served: &Served, view: &str) -> reqwest::Response {
    served
        .server
        .client
        .post(served.server.viewer_url("/v1/viewport"))
        .bearer_auth(&served.token)
        .json(&json!({
            "view": view, "zoom": 8, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200
        }))
        .send()
        .await
        .unwrap()
}

async fn points(served: &Served, view: &str) -> Vec<PointRow> {
    let resp = viewport(served, view).await;
    assert_eq!(resp.status().as_u16(), 200, "a served view answers: {view}");
    let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
    points
}

/// One ingest batch of `(external id, x, y, access)` rows into `view`.
async fn ingest(
    served: &Served,
    batch_id: &str,
    view: &str,
    rows: &[(Vec<u8>, f32, f32, &str)],
) -> reqwest::Response {
    let borrowed: Vec<(Option<&[u8]>, f32, f32, &str)> = rows
        .iter()
        .map(|(id, x, y, access)| (Some(id.as_slice()), *x, *y, *access))
        .collect();
    let body = build_ingest_batch_optional(&borrowed);
    served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", view)
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn flush(served: &Served) {
    let before = served.server.state.engine.write_executor_stats().flushes;
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/flush"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while served.server.state.engine.write_executor_stats().flushes == before {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never published"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// The roster entry `/v1/meta` publishes for one view id, or `None` where the document does not
/// carry the view at all.
fn roster_of(meta: &Value, id: &str) -> Option<Value> {
    meta["views"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == id)
        .cloned()
}

fn view_ids(meta: &Value) -> Vec<String> {
    meta["views"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect()
}

/// **The whole operation, end to end** (`views.md` §3.2): a key that did not exist becomes a view
/// with the next ordinal, on `/v1/meta` with its typed metadata, answering a viewer verb empty;
/// ingest into it lands; a flush gives it a row space; and a restart reproduces every part of that
/// from the segments manifest and the log.
#[tokio::test]
async fn a_created_view_is_served_ingested_flushed_and_survives_a_restart() {
    let served = serve().await;

    let resp = create(
        &served,
        "quarter",
        "2026-Q5",
        q_record("Q5 2026", 1_800_000_000_000_000),
    )
    .await;
    assert_eq!(resp.status(), 201, "a free key on a declared group creates");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["view"], "quarter:2026-Q5");
    assert_eq!(
        body["ordinal"], 1,
        "the ordinal continues the build's roster rather than restarting at 0"
    );

    // On `/v1/meta`, in ordinal order, with its typed record — and on the sharing group too,
    // because keys and ordinals belong to the group that owns them (`views.md` §3.3).
    let document = meta(&served).await;
    let entry = roster_of(&document, "quarter:2026-Q5").expect("the created view is published");
    assert_eq!(entry["ordinal"], 1);
    assert_eq!(entry["key"], "2026-Q5");
    assert_eq!(entry["group"], "quarter");
    assert_eq!(
        entry["metadata"],
        json!({
            "label": {"type": "text", "value": "Q5 2026"},
            // A JSON integer under a `timestamp_us` declaration is microseconds since the epoch,
            // and the roster stores it as the declaration types it rather than as an `int`.
            "starts": {"type": "timestamp_us", "value": 1_800_000_000_000_000i64},
        }),
        "one typed value per declared name: {entry}"
    );
    assert!(
        roster_of(&document, "quarter_map:2026-Q5").is_some(),
        "creating a key on the owner creates it, empty, on every group sharing its views"
    );

    // It answers a viewer verb before it has a single row: empty, not 404, and not a fault.
    assert!(
        points(&served, "quarter:2026-Q5").await.is_empty(),
        "a created view owns no rows until its first flush"
    );

    // Populate it. These are entities the corpus has never seen, so this is ordinary ingest into
    // a view that did not exist when the bundle was built.
    let rows: Vec<(Vec<u8>, f32, f32, &str)> = (0..4)
        .map(|i| (format!("q5-{i}").into_bytes(), 100.0 + i as f32, 200.0, "0"))
        .collect();
    let resp = ingest(&served, "q5-batch", "quarter:2026-Q5", &rows).await;
    assert_eq!(resp.status(), 200, "a created view accepts rows");
    flush(&served).await;

    let served_points = points(&served, "quarter:2026-Q5").await;
    assert_eq!(
        served_points.len(),
        4,
        "the first flush gives a created view its row space"
    );

    // The restart: the roster comes back from the segments manifest, and the rows from the
    // segment the flush published.
    let served = restart(served).await;
    let document = meta(&served).await;
    let entry = roster_of(&document, "quarter:2026-Q5").expect("the roster survives a restart");
    assert_eq!(entry["ordinal"], 1);
    assert_eq!(entry["metadata"]["label"]["value"], "Q5 2026");
    assert_eq!(
        points(&served, "quarter:2026-Q5").await.len(),
        4,
        "and so do its rows"
    );
}

/// Every arm the create refuses, and the status each takes (`views.md` §3.2). They are told apart
/// because the caller's remedy differs: a refused record is one to correct, a taken or burnt key
/// is one to replace, and an unknown group is not a view at all.
#[tokio::test]
async fn a_create_refuses_a_taken_key_a_bad_key_a_wrong_record_and_an_unknown_group() {
    let served = serve().await;

    assert_eq!(
        create(&served, "quarter", "2026-Q1", q_record("again", 1))
            .await
            .status(),
        409,
        "a roster record is immutable, so an existing key is a conflict and not an update"
    );
    assert_eq!(
        create(&served, "no-such-group", "k", q_record("x", 1))
            .await
            .status(),
        404,
        "a group is declared at a build; there is no create that mints one"
    );
    assert_eq!(
        create(&served, "quarter_map", "2026-Q5", json!({}))
            .await
            .status(),
        422,
        "a group taking another's views takes no create of its own (views §3.3)"
    );
    assert_eq!(
        create(&served, "quarter", "2026:Q5", q_record("x", 1))
            .await
            .status(),
        422,
        "`:` is reserved out of a key — it is what joins a group to one"
    );
    assert_eq!(
        create(&served, "quarter", "2026-Q5", json!({ "metadata": {} }))
            .await
            .status(),
        422,
        "every declared name is required: the record is immutable, so an omission is permanent"
    );
    assert_eq!(
        create(
            &served,
            "quarter",
            "2026-Q5",
            json!({ "metadata": { "label": 7, "starts": 1 } })
        )
        .await
        .status(),
        422,
        "a metadata value is typed against the group's declaration"
    );
    assert_eq!(
        create(
            &served,
            "quarter",
            "2026-Q5",
            json!({ "metadata": { "label": "Q5", "starts": 1, "ends": 2 } })
        )
        .await
        .status(),
        422,
        "an undeclared name has no type, so it is refused rather than stored"
    );
    assert_eq!(
        create(
            &served,
            "quarter",
            "2026-Q5",
            json!({ "visibility": "finance", "metadata": { "label": "Q5", "starts": 1 } })
        )
        .await
        .status(),
        422,
        "no gate is evaluated (views §6), so a label would be a control accepted and never enforced"
    );

    // None of the refusals created anything, and the ordinal they would have taken is still free.
    let resp = create(&served, "quarter", "2026-Q5", q_record("Q5", 1)).await;
    assert_eq!(resp.status(), 201);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["ordinal"],
        1,
        "a refused create spends no ordinal"
    );
}

/// **A drop takes the key out of the roster and burns it** (`views.md` §3.4): the view is a 404
/// from then on, on every group sharing it, the key is refused on recreation for ever, and its
/// ordinal is never handed to another view.
#[tokio::test]
async fn a_drop_burns_the_key_and_the_ordinal_and_survives_a_restart() {
    let served = serve().await;
    let created = create(&served, "quarter", "2026-Q5", q_record("Q5", 1)).await;
    assert_eq!(created.status(), 201);

    let body = drop_view(&served, "quarter", "2026-Q5", false).await;
    assert_eq!(body["deleted"], 0, "dropping a view deletes no entity");

    let document = meta(&served).await;
    assert!(
        roster_of(&document, "quarter:2026-Q5").is_none(),
        "the view leaves the roster on acknowledgement"
    );
    assert!(
        roster_of(&document, "quarter_map:2026-Q5").is_none(),
        "and leaves every group sharing its views (views §3.3)"
    );
    assert_eq!(
        viewport(&served, "quarter:2026-Q5").await.status(),
        404,
        "a request naming a dropped view is the same 404 as one that never existed"
    );
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5 again", 1))
            .await
            .status(),
        409,
        "a dropped key is never reused"
    );

    // The next create takes the *next* ordinal, not the dropped one.
    let resp = create(&served, "quarter", "2026-Q6", q_record("Q6", 2)).await;
    assert_eq!(resp.status(), 201);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["ordinal"],
        2,
        "an ordinal is burnt with its key"
    );

    // And all of it comes back from the segments manifest.
    let served = restart(served).await;
    let document = meta(&served).await;
    assert!(roster_of(&document, "quarter:2026-Q5").is_none());
    assert_eq!(
        roster_of(&document, "quarter:2026-Q6").unwrap()["ordinal"],
        2
    );
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5", 1))
            .await
            .status(),
        409,
        "the tombstone survives the restart, or the key comes back to life"
    );
    // A declared view can be dropped too, and the same rules hold for it.
    assert!(view_ids(&document).contains(&"quarter:2026-Q1".to_string()));
    drop_view(&served, "quarter", "2026-Q1", false).await;
    assert_eq!(viewport(&served, "quarter:2026-Q1").await.status(), 404);
}
