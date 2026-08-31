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
        // One declared attribute, so the join rule's entity-scoped arm has a column to disagree
        // about (`views.md` §4).
        Field::new("score", DataType::Int32, true),
    ]));
    let ids: Vec<u64> = ids.collect();
    let xs: Vec<f64> = ids.iter().map(|&e| position(view, e).0).collect();
    let ys: Vec<f64> = ids.iter().map(|&e| position(view, e).1).collect();
    let scores: Vec<i32> = ids.iter().map(|&e| e as i32).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(arrow::array::Int32Array::from(scores)),
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
    // The declaration, for its schema alone: one entity-space attribute over the world file,
    // which is the file that holds every entity.
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[sources]
points = "world.parquet"

[[view]]
name             = "world"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }

[[attribute]]
name   = "score"
type   = "i32"
render = true
"#,
    )
    .unwrap();
    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("the fixture declaration parses");
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
        attribute_sources: tessera_build::config::AttributeSource::over(
            dir.join("world.parquet"),
            &config.schema,
        ),
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
        schema: config.schema,
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
/// One ingest row: the external id, its position, its access label and the declared attribute —
/// every declared column, which the schema check requires.
type Row<'a> = (Vec<u8>, f32, f32, &'a str, Option<i32>);

/// One ingest body.
fn batch(rows: &[Row<'_>]) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("access", DataType::Utf8, false),
        Field::new("score", DataType::Int32, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(arrow::array::BinaryArray::from_iter(
                rows.iter().map(|(id, ..)| Some(id.as_slice())),
            )),
            Arc::new(arrow::array::Float32Array::from_iter_values(
                rows.iter().map(|(_, x, ..)| *x),
            )),
            Arc::new(arrow::array::Float32Array::from_iter_values(
                rows.iter().map(|(_, _, y, ..)| *y),
            )),
            Arc::new(arrow::array::StringArray::from_iter_values(
                rows.iter().map(|(_, _, _, access, _)| *access),
            )),
            Arc::new(arrow::array::Int32Array::from_iter(
                rows.iter().map(|(.., score)| *score),
            )),
        ],
    )
    .unwrap();
    let mut writer = arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

async fn ingest(
    served: &Served,
    batch_id: &str,
    view: &str,
    rows: &[Row<'_>],
) -> reqwest::Response {
    let body = batch(rows);
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

/// Flush until the buffer is empty — **a flush unit is one view**, and a tick publishes one of
/// them, so a batch that landed in two views needs two ticks (write path's `dispatch_flushes`).
async fn flush(served: &Served) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while served.server.state.engine.buffered_items() > 0 {
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
        while served.server.state.engine.write_executor_stats().flushes == before {
            assert!(
                std::time::Instant::now() < deadline,
                "the flush never published: {} rows buffered, {} flushes",
                served.server.state.engine.buffered_items(),
                served.server.state.engine.write_executor_stats().flushes
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
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
    let rows: Vec<Row<'_>> = (0..4)
        .map(|i| {
            (
                format!("q5-{i}").into_bytes(),
                100.0 + i as f32,
                200.0,
                "0",
                Some(i),
            )
        })
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

/// **One entity, two views** (`views.md` §4): a known `external_id` naming a view the entity is
/// not in is accepted, the row lands in that view's pending segment, and after the flush the point
/// is in both views at its own position in each.
///
/// The entity is untouched by the second batch — that is what makes this a join rather than an
/// update — and the arms below are the ways a caller could try to make it an update by accident.
#[tokio::test]
async fn a_known_external_id_joins_a_second_view_and_is_placed_in_each() {
    let served = serve().await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5", 1))
            .await
            .status(),
        201
    );

    let id = b"joiner".to_vec();
    assert_eq!(
        ingest(&served, "first", "world", &[(id.clone(), 10.0, 10.0, "0", Some(7))])
            .await
            .status(),
        200,
        "an unknown external id allocates, as it always did"
    );
    // The same id, a different view: accepted. Before the flush and after it.
    let resp = ingest(
        &served,
        "join",
        "quarter:2026-Q5",
        &[(id.clone(), 800.0, 300.0, "0", Some(7))],
    )
    .await;
    assert_eq!(
        resp.status(),
        200,
        "a known external id naming a view the entity is not in is a join: {:?}",
        resp.text().await
    );
    flush(&served).await;

    let in_world = points(&served, "world").await;
    let in_q5 = points(&served, "quarter:2026-Q5").await;
    let world_ids: Vec<u64> = in_world.iter().map(|p| p.0).collect();
    let q5_ids: Vec<u64> = in_q5.iter().map(|p| p.0).collect();
    let shared: Vec<u64> = world_ids
        .iter()
        .copied()
        .filter(|id| q5_ids.contains(id))
        .collect();
    assert_eq!(
        shared.len(),
        1,
        "one identity, in two views: world {world_ids:?}, q5 {q5_ids:?}"
    );
    let joined = shared[0];
    let world_code = in_world
        .iter()
        .find(|p| p.0 == joined)
        .unwrap()
        .1;
    let q5_code = in_q5.iter().find(|p| p.0 == joined).unwrap().1;
    assert_ne!(
        world_code, q5_code,
        "a view owns everything downstream of the permutation: the same point sits somewhere \
         different in each"
    );

    // And the second batch is now a duplicate *in that view*: positions are not updated in place.
    assert_eq!(
        ingest(
            &served,
            "join-again",
            "quarter:2026-Q5",
            &[(id.clone(), 810.0, 310.0, "0", Some(7))]
        )
        .await
        .status(),
        409,
        "already in the named view"
    );
}

/// The three arms a join refuses, each of which would otherwise be a way to change entity space
/// through a second view's row (`views.md` §4).
#[tokio::test]
async fn a_join_refuses_a_second_row_a_relabel_and_a_changed_attribute() {
    let served = serve().await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5", 1))
            .await
            .status(),
        201
    );
    let id = b"arms".to_vec();
    assert_eq!(
        ingest(&served, "first", "world", &[(id.clone(), 10.0, 10.0, "0", Some(7))])
            .await
            .status(),
        200
    );

    // Already in the named view — the buffer's half of the arm, before any flush has run.
    assert_eq!(
        ingest(
            &served,
            "same-view",
            "world",
            &[(id.clone(), 11.0, 11.0, "0", Some(7))]
        )
        .await
        .status(),
        409,
        "'in the view' is the permutation AND the commit window's buffer"
    );

    // A different label on a known id: a re-label is a delete plus a re-ingest (decision 0047).
    assert_eq!(
        ingest(
            &served,
            "relabel",
            "quarter:2026-Q5",
            &[(id.clone(), 800.0, 300.0, "1", Some(7))]
        )
        .await
        .status(),
        409,
        "a second view's row is not a route to a new access label"
    );

    // A different value for an entity-scoped attribute, refused naming the column.
    let resp = ingest(
        &served,
        "reattribute",
        "quarter:2026-Q5",
        &[(id.clone(), 800.0, 300.0, "0", Some(9))],
    )
    .await;
    assert_eq!(resp.status(), 409);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["detail"].as_str().unwrap().contains("score"),
        "the refusal names the column: {body}"
    );

    // Null is absence, and absence is not disagreement: the join lands.
    assert_eq!(
        ingest(
            &served,
            "attribute-absent",
            "quarter:2026-Q5",
            &[(id.clone(), 800.0, 300.0, "0", None)]
        )
        .await
        .status(),
        200,
        "a joining row byte-matches the stored value or omits it"
    );
}

/// **A suppressed holder takes the same arms and stays hidden** (`views.md` §4) — the rule's edge,
/// stated because the two removal rules have been conflated twice.
///
/// The new row lands on the *same* entity, suppression composes in entity space, and the entity is
/// invisible in the new view as in every other. What write-path §2.1 refuses is a byte-identical
/// re-ingest *past* a suppression — a second copy under a fresh entity — and attaching a view to
/// the suppressed entity creates no copy. **The suppression retires only by unsuppress** (Rule S).
#[tokio::test]
async fn a_suppressed_holder_joins_a_view_and_stays_hidden_until_it_is_unsuppressed() {
    let served = serve().await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5", 1))
            .await
            .status(),
        201
    );
    let id = b"hidden".to_vec();
    let resp = ingest(&served, "first", "world", &[(id.clone(), 10.0, 10.0, "0", Some(7))]).await;
    assert_eq!(resp.status(), 200);
    let tessera_id: u64 = resp.json::<Value>().await.unwrap()["tessera_ids"][0]
        .as_u64()
        .expect("the 200 returns one identifier per accepted row");
    flush(&served).await;
    assert!(
        points(&served, "world")
            .await
            .iter()
            .any(|p| p.0 == tessera_id),
        "visible before the suppression"
    );

    let change = |op: &str| {
        let body = json!([{ "tessera_id": tessera_id.to_string(), "idset": FIXTURE_IDSET, "op": op }]);
        served
            .server
            .client
            .post(served.server.control_url("/control/changes"))
            .bearer_auth(OPERATOR_CREDENTIAL)
            .json(&body)
            .send()
    };
    assert_eq!(change("suppress").await.unwrap().status(), 200);
    assert!(
        !points(&served, "world")
            .await
            .iter()
            .any(|p| p.0 == tessera_id),
        "suppressed"
    );

    // The join is accepted past the suppression, because it creates no copy.
    assert_eq!(
        ingest(
            &served,
            "join",
            "quarter:2026-Q5",
            &[(id.clone(), 800.0, 300.0, "0", Some(7))]
        )
        .await
        .status(),
        200,
        "a suppressed holder takes the same arms as a live one"
    );
    flush(&served).await;
    assert!(
        !points(&served, "quarter:2026-Q5")
            .await
            .iter()
            .any(|p| p.0 == tessera_id),
        "hidden in the new view from the moment the row exists — suppression is entity-space"
    );
    assert!(
        !points(&served, "world")
            .await
            .iter()
            .any(|p| p.0 == tessera_id),
        "and still hidden in the first"
    );

    // **Rule S**: the entry leaves `suppressed` only by its unsuppress. Nothing above — not the
    // join, not the flush that gave it a second row — retired it, and the item comes back in both
    // views when, and only when, it is unsuppressed.
    assert_eq!(change("unsuppress").await.unwrap().status(), 200);
    assert!(
        points(&served, "world")
            .await
            .iter()
            .any(|p| p.0 == tessera_id),
        "unsuppress is the one retirement route, and it reveals the row"
    );
    assert!(
        points(&served, "quarter:2026-Q5")
            .await
            .iter()
            .any(|p| p.0 == tessera_id),
        "in the joined view too"
    );
}

/// **`delete_dangling` is sugar over the ordinary deletion path** (`views.md` §3.4): at the drop,
/// the entities of the dropped view that hold a row in no other view — the buffer included — are
/// submitted as ordinary deletions, which enter the overlay and retire at the fold like any
/// deletion. It is not a second retirement route, and everything asserted below is an ordinary
/// deletion's observable.
///
/// An entity that is *also* somewhere else survives, which is the half that makes the option a
/// question rather than a shorthand for "delete everything this view could see".
#[tokio::test]
async fn delete_dangling_deletes_only_the_entities_this_view_alone_held() {
    let served = serve().await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5", 1))
            .await
            .status(),
        201
    );

    let only_here = b"only-here".to_vec();
    let also_elsewhere = b"also-elsewhere".to_vec();
    assert_eq!(
        ingest(
            &served,
            "elsewhere",
            "world",
            &[(also_elsewhere.clone(), 10.0, 10.0, "0", Some(1))]
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        ingest(
            &served,
            "into-q5",
            "quarter:2026-Q5",
            &[
                (only_here.clone(), 800.0, 300.0, "0", Some(2)),
                (also_elsewhere.clone(), 810.0, 310.0, "0", Some(1)),
            ]
        )
        .await
        .status(),
        200,
        "one new entity and one join"
    );
    flush(&served).await;

    // A third entity, still in the buffer when the drop runs: the probe counts the buffer as this
    // view's rows, or it would call an entity dangling that a caller was told had landed.
    let buffered = b"buffered".to_vec();
    assert_eq!(
        ingest(
            &served,
            "buffered",
            "quarter:2026-Q5",
            &[(buffered.clone(), 820.0, 320.0, "0", Some(3))]
        )
        .await
        .status(),
        200
    );

    let body = drop_view(&served, "quarter", "2026-Q5", true).await;
    assert_eq!(
        body["deleted"], 2,
        "the two entities this view alone held — the flushed one and the buffered one — and not \
         the one that also sits in `world`: {body}"
    );

    // The ordinary deletion observables. A deleted holder is forgotten at the interchange boundary
    // (decision 0047), so its external id may be ingested again and allocates fresh; a live one
    // still collides.
    assert_eq!(
        ingest(
            &served,
            "reingest-deleted",
            "world",
            &[(only_here.clone(), 20.0, 20.0, "0", Some(2))]
        )
        .await
        .status(),
        200,
        "a deleted holder is not a duplicate"
    );
    assert_eq!(
        ingest(
            &served,
            "reingest-live",
            "world",
            &[(also_elsewhere.clone(), 30.0, 30.0, "0", Some(1))]
        )
        .await
        .status(),
        409,
        "the entity that was also in `world` was not deleted, and is still in `world`"
    );
    assert_eq!(
        ingest(
            &served,
            "reingest-buffered",
            "world",
            &[(buffered.clone(), 40.0, 40.0, "0", Some(3))]
        )
        .await
        .status(),
        200,
        "the buffered row's entity was deleted with the rest"
    );

    // And the drop without the option deletes nothing at all, which is the default.
    assert_eq!(
        create(&served, "quarter", "2026-Q6", q_record("Q6", 2))
            .await
            .status(),
        201
    );
    let solitary = b"solitary".to_vec();
    let resp = ingest(
        &served,
        "q6",
        "quarter:2026-Q6",
        &[(solitary.clone(), 800.0, 300.0, "0", Some(4))],
    )
    .await;
    assert_eq!(resp.status(), 200);
    let before: u64 = resp.json::<Value>().await.unwrap()["tessera_ids"][0]
        .as_u64()
        .unwrap();
    flush(&served).await;
    let body = drop_view(&served, "quarter", "2026-Q6", false).await;
    assert_eq!(body["deleted"], 0);

    // **Dropping a view deletes no entity.** The item still exists — with its label, its
    // attributes and its identity — in no view at all, and a later batch into another view picks
    // it up by `external_id` under the join rule, which is the ordinary shape of a corpus whose
    // items come and go between slices (`views.md` §3.4, §4).
    let resp = ingest(
        &served,
        "reingest-solitary",
        "world",
        &[(solitary.clone(), 50.0, 50.0, "0", Some(4))],
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["tessera_ids"][0]
            .as_u64()
            .unwrap(),
        before,
        "the same identity, joined to a new view — not a fresh entity, which is what a deletion          would have made of it"
    );
}
