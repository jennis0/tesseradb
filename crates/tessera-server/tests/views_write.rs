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
//!   the log comes back missing, and the drop it recorded is then undone.
//! - **A dropped key is reusable, and a recreate adopts nothing**
//!   ([decision 0115](../../../docs/decisions/0115-a-dropped-view-key-is-reusable.md)). The key is
//!   a name the caller chose; the predecessor's row spaces, columns and derived structures linger
//!   until the fold reclaims them, and the internal incarnation is what keeps them out of the view
//!   created under the reused name.
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

/// One view's points. `key` is the discriminator value every row carries, for the group whose
/// points are one file behind `fields.view`; `None` is a file that *is* the view.
fn write_points(path: &Path, view: &str, ids: std::ops::Range<u64>, key: Option<&str>) {
    let mut fields = vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        // One declared attribute, so the join rule's entity-scoped arm has a column to disagree
        // about (`views.md` §4).
        Field::new("score", DataType::Int32, true),
    ];
    if key.is_some() {
        fields.push(Field::new("quarter", DataType::Utf8, false));
    }
    let schema = Arc::new(Schema::new(fields));
    let ids: Vec<u64> = ids.collect();
    let xs: Vec<f64> = ids.iter().map(|&e| position(view, e).0).collect();
    let ys: Vec<f64> = ids.iter().map(|&e| position(view, e).1).collect();
    let scores: Vec<i32> = ids.iter().map(|&e| e as i32).collect();
    let mut columns: Vec<arrow::array::ArrayRef> = vec![
        Arc::new(UInt64Array::from(ids.clone())),
        Arc::new(Float64Array::from(xs)),
        Arc::new(Float64Array::from(ys)),
        Arc::new(arrow::array::Int32Array::from(scores)),
    ];
    if let Some(key) = key {
        columns.push(Arc::new(arrow::array::StringArray::from(vec![
            key;
            ids.len()
        ])));
    }
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn view_args(view: &str, points: &Path, pairs: &Path) -> ViewArgs {
    ViewArgs {
        visibility: None,
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
    write_points(&world_points, "world", WORLD, None);
    let mut views = vec![view_args("world", &world_points, &pairs)];
    for group in ["quarter", "quarter_map"] {
        let id = format!("{group}:2026-Q1");
        let points = dir.join(format!("{group}-q1.parquet"));
        write_points(&points, &id, Q1, None);
        views.push(view_args(&id, &points, &pairs));
    }
    let roster = |with_metadata: bool| {
        vec![GroupViewDescriptor {
            key: "2026-Q1".to_string(),
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
                title: None,
                visibility: None,
                scoped_scalars: Vec::new(),
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
                title: None,
                visibility: None,
                scoped_scalars: Vec::new(),
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

/// **Take a fresh session** (`views.md` §6). The visible-view set is resolved once at authorise
/// and is fixed for the session's life, so a view created since is a 404 to a session that
/// predates it — deliberately, and the owner's ruling. A test that creates a view and then reads
/// it therefore re-authorises first, exactly as a client would; `tests/views_gate.rs` is where
/// the *not*-re-authorising case is asserted.
async fn reauthorise(served: &mut Served) {
    served.token = authorise(&served.server, &["0", "1"]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
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
    // Two responses here are the server behaving as specified under machine load, and neither is
    // the answer under test. A 429 is the admission gate shedding (contracts §3.1) — honour
    // `Retry-After` and ask again. `x-tessera-stale: 1` is a publication served from a superseded
    // generation while the refresh runs (`geometry-pinning.md` §7) — serve-stale-not-block is the
    // design, so wait for a fresh one. Anything else is asserted as the real response.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let resp = viewport(served, view).await;
        let unsettled = std::time::Instant::now() < deadline;
        if resp.status().as_u16() == 429 && unsettled {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            continue;
        }
        assert_eq!(resp.status().as_u16(), 200, "a served view answers: {view}");
        if resp
            .headers()
            .get("x-tessera-stale")
            .is_some_and(|v| v == "1")
            && unsettled
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        }
        let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
        return points;
    }
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
    // Always drive at least one flush: `buffered_items()` lags by up to one apply — its own doc
    // says so, deliberately — so an acked batch can still read as 0 here, and gating the first
    // flush on it skips the flush entirely under load. Every caller flushes straight after an
    // acked ingest, so the buffer is genuinely non-empty on the first iteration and the flush
    // counter is guaranteed to move.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
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
        if served.server.state.engine.buffered_items() == 0 {
            break;
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

/// **The whole operation, end to end** (`views.md` §3.2): a key that did not exist becomes a view,
/// on `/v1/meta` with its typed metadata, answering a viewer verb empty;
/// ingest into it lands; a flush gives it a row space; and a restart reproduces every part of that
/// from the segments manifest and the log.
#[tokio::test]
async fn a_created_view_is_served_ingested_flushed_and_survives_a_restart() {
    let mut served = serve().await;

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
    assert_eq!(body["key"], "2026-Q5");
    // The visible-view set is fixed per session (`views.md` §6), so a reader of the newly created
    // view takes a new session, exactly as a client would.
    reauthorise(&mut served).await;

    // On `/v1/meta`, after the view it was created behind — and on the sharing group too, because
    // a key belongs to the group that owns the views (`views.md` §3.3).
    let document = meta(&served).await;
    let entry = roster_of(&document, "quarter:2026-Q5").expect("the created view is published");
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

    // The flush's publication and a live session's sight of the entities it minted are two
    // events, and the second follows the first by an asynchronous refresh with no wire signal on
    // the interim response — a viewport between them is a fresh 200 serving the pre-flush answer.
    // So wait for the settled count rather than asserting the first response.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut served_points = points(&served, "quarter:2026-Q5").await;
    while served_points.len() != 4 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        served_points = points(&served, "quarter:2026-Q5").await;
    }
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
            json!({ "visibility": " , , ", "metadata": { "label": "Q5", "starts": 1 } })
        )
        .await
        .status(),
        422,
        "a gate naming no terms is satisfied by nobody, so the view would be reachable by no \
         principal at all (views §6)"
    );

    // None of the refusals created anything, so the key they named is still free.
    let resp = create(&served, "quarter", "2026-Q5", q_record("Q5", 1)).await;
    assert_eq!(resp.status(), 201, "a refused create takes no key");
}

/// **A drop takes the key out of the roster and frees it** (`views.md` §3.4, decision 0115): the
/// view is a 404 from then on, on every group sharing it, and the key may be created again — at a
/// fresh incarnation, so the recreated view is empty rather than the old one under a new record.
#[tokio::test]
async fn a_drop_frees_the_key_and_the_drop_survives_a_restart() {
    let mut served = serve().await;
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
    // **The key is free, and the record is the new one** (decision 0115): a recreate under a
    // dropped key is a 201, and what the name means from here is what the new record says.
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5 again", 9))
            .await
            .status(),
        201,
        "a dropped key is reusable"
    );
    // A view created since this session authorised is a 404 to it until it re-authorises
    // (`views.md` §6), and that is as true of a recreate as of a first create.
    reauthorise(&mut served).await;
    let document = meta(&served).await;
    assert_eq!(
        roster_of(&document, "quarter:2026-Q5").unwrap()["metadata"]["label"]["value"],
        "Q5 again",
        "the recreated key carries its own record, not its predecessor's"
    );
    // And drop it again, so the restart below is over a key with two dead incarnations behind it.
    drop_view(&served, "quarter", "2026-Q5", false).await;

    // A different key still creates: a drop touches the key it named and nothing else.
    let resp = create(&served, "quarter", "2026-Q6", q_record("Q6", 2)).await;
    assert_eq!(resp.status(), 201);

    // And all of it comes back from the segments manifest.
    let served = restart(served).await;
    let document = meta(&served).await;
    assert!(roster_of(&document, "quarter:2026-Q5").is_none());
    assert_eq!(
        roster_of(&document, "quarter:2026-Q6").unwrap()["key"],
        "2026-Q6"
    );
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5", 1))
            .await
            .status(),
        201,
        "the drop survives the restart as an absence, and the key is still free"
    );
    drop_view(&served, "quarter", "2026-Q5", false).await;
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
    let mut served = serve().await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5", 1))
            .await
            .status(),
        201
    );
    // The visible-view set is fixed per session (`views.md` §6), so a reader of the
    // newly created view takes a new session, exactly as a client would.
    reauthorise(&mut served).await;

    let id = b"joiner".to_vec();
    assert_eq!(
        ingest(
            &served,
            "first",
            "world",
            &[(id.clone(), 10.0, 10.0, "0", Some(7))]
        )
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
    let world_code = in_world.iter().find(|p| p.0 == joined).unwrap().1;
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
        ingest(
            &served,
            "first",
            "world",
            &[(id.clone(), 10.0, 10.0, "0", Some(7))]
        )
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
    let mut served = serve().await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5", 1))
            .await
            .status(),
        201
    );
    // The visible-view set is fixed per session (`views.md` §6), so a reader of the
    // newly created view takes a new session, exactly as a client would.
    reauthorise(&mut served).await;
    let id = b"hidden".to_vec();
    let resp = ingest(
        &served,
        "first",
        "world",
        &[(id.clone(), 10.0, 10.0, "0", Some(7))],
    )
    .await;
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
        let body =
            json!([{ "tessera_id": tessera_id.to_string(), "idset": FIXTURE_IDSET, "op": op }]);
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

    // **A buffered join row goes with the view it named, and its entity does not.** The row is
    // geometry for a coordinate system that no longer exists, so nothing would ever give it a
    // place; left in the buffer it would pin the WAL's reclaim bound for the life of the process.
    assert_eq!(
        create(&served, "quarter", "2026-Q7", q_record("Q7", 7))
            .await
            .status(),
        201
    );
    let both = b"both".to_vec();
    let buffered_before = served.server.state.engine.buffered_items();
    assert_eq!(
        ingest(
            &served,
            "both-world",
            "world",
            &[(both.clone(), 60.0, 60.0, "0", Some(6))]
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        ingest(
            &served,
            "both-q7",
            "quarter:2026-Q7",
            &[(both.clone(), 800.0, 300.0, "0", Some(6))]
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        served.server.state.engine.buffered_items(),
        buffered_before + 2,
        "one row per (entity, view), both awaiting a flush"
    );
    let body = drop_view(&served, "quarter", "2026-Q7", true).await;
    assert_eq!(
        body["deleted"], 0,
        "the entity holds a row in `world`, so it is not dangling: {body}"
    );
    assert_eq!(
        served.server.state.engine.buffered_items(),
        buffered_before + 1,
        "the dropped view's buffered row went with it, and the other stayed"
    );
    flush(&served).await;
    assert_eq!(
        ingest(
            &served,
            "both-again",
            "world",
            &[(both.clone(), 70.0, 70.0, "0", Some(6))]
        )
        .await
        .status(),
        409,
        "and the entity is alive, in `world`"
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

// ---------------------------------------------------------------------------------------------
// A group whose roster was minted (`views.md` §3.1's third form)
// ---------------------------------------------------------------------------------------------

/// The same fixture as [`build_fixture_bundle`]'s `quarter`, from a declaration that names **no
/// keys at all** — and built the way the binary builds one, through `build_views` and
/// `group_registry`, so what the service opens is the mint's own output rather than a descriptor
/// written here (`views.md` §3.1, §7).
fn build_minted_bundle(dir: &Path) -> std::path::PathBuf {
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, ENTITIES);
    write_points(&dir.join("world.parquet"), "world", WORLD, None);
    write_points(
        &dir.join("quarter.parquet"),
        "quarter:2026-Q1",
        Q1,
        Some("2026-Q1"),
    );
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[sources]
world   = "world.parquet"
quarter = "quarter.parquet"

[defaults]
allocation_view = "world"

[[view]]
name             = "world"
source           = "world"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }

[[view_group]]
name             = "quarter"
extent           = { min = 0.0, max = 1000.0 }
source           = "quarter"
fields           = { view = "quarter" }
point_visibility = { default = "public" }

[[attribute]]
name   = "score"
type   = "i32"
render = true
"#,
    )
    .unwrap();
    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("the declaration parses");
    let registry = config.build_views().expect("the roster is minted");
    let anchor = config
        .anchor_view(&registry)
        .expect("the anchor is declared");
    let views: Vec<ViewArgs> = registry
        .iter()
        .map(|view| ViewArgs {
            visibility: view.visibility.clone(),
            view_id: view.id.clone(),
            projection: view.projection,
            extent: extent(),
            points: view.source.clone().expect("every view names its points"),
            point_fields: view.fields.clone(),
            select: view.select.clone(),
            access: tessera_build::config::AccessInput::relation(pairs.clone()),
        })
        .collect();
    let groups = config.group_registry(&registry, &views);
    let out = dir.join("bundle");
    build(&BuildArgs {
        views,
        anchor,
        groups,
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
    .expect("the minted build succeeds");
    out
}

/// **A minted group is a group** (decision 0091: a build is ingest into an empty database). Its
/// roster records were read off the data rather than written into the declaration, and nothing
/// above the mint may be able to tell: the create verb, the drop and the join rule work on it
/// exactly as they do on a declared roster, and an unknown key is the same 404 — there is no
/// first-batch-creates route here, the view coming into being through the create operation like
/// any other (`views.md` §3.2).
#[tokio::test]
async fn a_minted_group_takes_a_create_a_drop_and_a_join() {
    let tmp = TempDir::new().unwrap();
    build_minted_bundle(tmp.path());
    let mut served = open(tmp).await;

    // The minted key is served like a declared one, carrying no record of its own.
    let document = meta(&served).await;
    assert_eq!(
        roster_of(&document, "quarter:2026-Q1").expect("the minted view is served")["metadata"],
        json!({})
    );

    // **A batch naming a key the roster does not carry is a 404**, minted group or not.
    assert_eq!(
        ingest(
            &served,
            "unknown-key",
            "quarter:2026-Q9",
            &[(b"nobody".to_vec(), 10.0, 10.0, "0", Some(1))]
        )
        .await
        .status(),
        404
    );

    // **Create.** The group declares no metadata, so the record is empty — and the view is a view
    // from the acknowledgement.
    assert_eq!(
        create(&served, "quarter", "2026-Q2", json!({}))
            .await
            .status(),
        201
    );
    reauthorise(&mut served).await;
    assert!(view_ids(&meta(&served).await).contains(&"quarter:2026-Q2".to_string()));
    assert_eq!(viewport(&served, "quarter:2026-Q2").await.status(), 200);

    // **Join.** An entity the build already knows joins the new view by its external id, and is
    // placed there with its own geometry — the same identity, a second row space.
    //
    // The batch carries the entity's **own** label set. Source 3 is a multiple of three, so the
    // fixture gave it `{0, 1}`; naming only `0` is now the 409 `views.md` §4 always specified,
    // the entity→term transpose having made the arm exact past its own flush.
    let known = external_id_of(3);
    let resp = ingest(
        &served,
        "join-minted",
        "quarter:2026-Q2",
        &[(known.clone(), 400.0, 400.0, "0,1", Some(3))],
    )
    .await;
    assert_eq!(resp.status(), 200, "a join into a minted group's view");
    let joined = resp.json::<Value>().await.unwrap()["tessera_ids"][0]
        .as_u64()
        .unwrap();
    flush(&served).await;
    reauthorise(&mut served).await;
    let in_q2 = points(&served, "quarter:2026-Q2").await;
    assert_eq!(in_q2.len(), 1, "the joined row, and only it");
    assert_eq!(
        in_q2[0].0, joined,
        "the row is the one the acknowledgement named"
    );
    let in_world: Vec<u64> = points(&served, "world").await.iter().map(|p| p.0).collect();
    assert!(
        in_world.contains(&joined),
        "the same identity in two views, not a fresh entity in one: {joined} is not in \
         {in_world:?}"
    );
    let resp = ingest(
        &served,
        "join-minted-world",
        "world",
        &[(known, 60.0, 60.0, "0", Some(3))],
    )
    .await;
    assert_eq!(resp.status(), 409, "the entity is alive in `world` already");

    // **Drop**, and the key is freed — on a minted view exactly as on a declared one.
    let body = drop_view(&served, "quarter", "2026-Q1", false).await;
    assert_eq!(body["deleted"], 0);
    reauthorise(&mut served).await;
    assert!(!view_ids(&meta(&served).await).contains(&"quarter:2026-Q1".to_string()));
    assert_eq!(viewport(&served, "quarter:2026-Q1").await.status(), 404);
    assert_eq!(
        create(&served, "quarter", "2026-Q1", json!({}))
            .await
            .status(),
        201,
        "a dropped key is reusable (decision 0115)"
    );
}

/// Request a compaction fold and block until it has published (`POST /control/compact`,
/// contracts §3.4). The counter is the only "done" there is: the fold runs on its own thread and
/// publishes at the executor's next loop iteration, so the acceptance code says nothing about
/// completion.
async fn fold(served: &Served) {
    let before = served.server.state.engine.write_executor_stats().folds;
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/compact"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "a fold is accepted at any time");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let stats = served.server.state.engine.write_executor_stats();
        assert_eq!(
            stats.fold_failures, 0,
            "the fold failed rather than publishing"
        );
        if stats.folds > before {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published: {} folds, {} discarded",
            stats.folds,
            stats.fold_failures
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The prefix `CURRENT` names, and the directory it is.
fn live_prefix(served: &Served) -> (String, std::path::PathBuf) {
    let root = served.tmp.path().join("bundle");
    let current: Value =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).unwrap()).unwrap();
    let prefix = current["prefix"].as_str().unwrap().to_string();
    let dir = root.join(&prefix);
    (prefix, dir)
}

/// The views the newest `SEGMENTS-<n>.json` of `prefix`'s only partition names a segment for.
fn segment_views(prefix_dir: &Path) -> Vec<String> {
    let partition = prefix_dir.join("partitions/default");
    let mut manifests: Vec<std::path::PathBuf> = std::fs::read_dir(&partition)
        .expect("the partition directory")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("SEGMENTS-") && n.ends_with(".json"))
        })
        .collect();
    manifests.sort();
    let newest = manifests.last().expect("a side-manifest per partition");
    let document: Value = serde_json::from_slice(&std::fs::read(newest).unwrap()).unwrap();
    let mut views: Vec<String> = document["segments"]
        .as_array()
        .expect("the segment list")
        .iter()
        .map(|s| s["view"].as_str().unwrap().to_string())
        .collect();
    views.sort();
    views.dedup();
    views
}

/// Every directory under `root` whose path names `view_id`'s own two components — the shape
/// `tessera_store::view_rel` lays a group's view down in, `views/<group>/<key>`.
fn view_dirs(root: &Path, group: &str, key: &str) -> Vec<std::path::PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>, want: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path.ends_with(want) {
                out.push(path.clone());
            }
            walk(&path, out, want);
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out, &Path::new("views").join(group).join(key));
    out
}

/// **A fold after a drop reclaims the dropped view, by omission** (`views.md` §3.4). The claim had
/// no test: the drop's own coverage stops at the roster and the 404, and the fold's stops at a
/// bundle nothing was dropped from — so between them nothing drove a fold over a bundle a view had
/// left, which is where the reclamation actually happens.
///
/// Four things are asserted, and each is a different mechanism:
///
/// - **The plan omits it.** The fold plans over the bundle the drop published, from which the view
///   was retained out, so the new prefix carries a segment for every surviving view and none for
///   the dropped one. There is no sweep and nothing that names the dropped view — omission is the
///   whole mechanism, which is why a test that only checked the files were gone would pass over a
///   fold that had copied them and then deleted them.
/// - **The files go with the old prefix.** The new prefix has no directory for the view at all,
///   and the superseded prefix — which still holds them, mapped, until every reader of it is gone
///   — is reclaimed whole. A restart's orphan sweep is the deterministic end of that: after it, no
///   prefix under the bundle root names the view.
/// - **A restart serves the survivors**, so the omission took the dropped view and nothing else.
/// - **The drop outlives the fold.** The fold rewrites the manifest; a key the manifest no longer
///   carries must not come back as a view of it, record and rows and all.
/// - **And the two removal rules compose.** A second drop, this one with `delete_dangling`, puts
///   ordinary deletions on the deny lane; the fold that omits the view's segments is also the fold
///   that executes them, and their overlay entries retire there (Rule F) rather than at the drop.
#[tokio::test]
async fn a_fold_after_a_drop_reclaims_the_dropped_view_and_a_recreate_adopts_nothing() {
    let mut served = serve().await;

    // A second key, so the group still has a view after the drop and the survivors are a set
    // rather than one plain view. Its rows arrive through ingest, so the dropped view is not the
    // only one whose files the fold has to carry.
    assert_eq!(
        create(&served, "quarter", "2026-Q2", q_record("Q2 2026", 2))
            .await
            .status(),
        201
    );
    reauthorise(&mut served).await;
    let rows: Vec<Row<'_>> = (0..4)
        .map(|i| {
            (
                format!("q2-{i}").into_bytes(),
                100.0 + i as f32,
                200.0,
                "0",
                Some(i),
            )
        })
        .collect();
    assert_eq!(
        ingest(&served, "q2-batch", "quarter:2026-Q2", &rows)
            .await
            .status(),
        200
    );
    flush(&served).await;

    let (_, base_dir) = live_prefix(&served);
    assert_eq!(
        segment_views(&base_dir),
        vec![
            "quarter:2026-Q1".to_string(),
            "quarter:2026-Q2".to_string(),
            "quarter_map:2026-Q1".to_string(),
            "world".to_string(),
        ],
        "before the drop the bundle carries a segment for every view"
    );

    let body = drop_view(&served, "quarter", "2026-Q1", false).await;
    assert_eq!(body["deleted"], 0, "a drop by itself deletes no entity");

    fold(&served).await;
    let (folded, folded_dir) = live_prefix(&served);
    assert_ne!(folded, "v00000", "the fold published a new prefix");

    // (a) The plan omitted the dropped view — on the owner and on the group sharing its key.
    assert_eq!(
        segment_views(&folded_dir),
        vec!["quarter:2026-Q2".to_string(), "world".to_string()],
        "the fold carries no segment for a view the bundle no longer has"
    );

    // (b) And wrote no files for it: the new prefix has no directory of its own for the key,
    // under either group.
    for group in ["quarter", "quarter_map"] {
        assert!(
            view_dirs(&folded_dir, group, "2026-Q1").is_empty(),
            "the folded prefix lays down nothing for {group}:2026-Q1"
        );
    }
    assert!(
        !view_dirs(&folded_dir, "quarter", "2026-Q2").is_empty(),
        "and does lay down the view that survived"
    );

    // (c) A restart serves the survivors and still 404s the dropped key. The restart is also what
    // makes the reclamation deterministic: the superseded prefix is reclaimed when its last reader
    // lets go, and the startup sweep takes any that stands.
    let served = restart(served).await;
    let root = served.tmp.path().join("bundle");
    assert!(
        view_dirs(&root, "quarter", "2026-Q1").is_empty(),
        "after the fold and the sweep no prefix under the bundle root holds the dropped view"
    );
    let document = meta(&served).await;
    assert_eq!(
        view_ids(&document),
        vec![
            "world".to_string(),
            "quarter:2026-Q2".to_string(),
            "quarter_map:2026-Q2".to_string(),
        ],
        "the survivors are served, and the dropped key is on no group"
    );
    assert_eq!(
        points(&served, "quarter:2026-Q2").await.len(),
        4,
        "the surviving view kept its rows across the fold"
    );
    assert_eq!(
        viewport(&served, "quarter:2026-Q1").await.status(),
        404,
        "the dropped view is the 404 a name nobody declared gets"
    );

    // (d) **The key created again after the fold is an empty view** (decision 0115). The old
    // incarnation's files are gone by now, so this arm is about the roster: the recreate lands,
    // and the view it makes carries the new record and no rows.
    assert_eq!(
        create(&served, "quarter", "2026-Q1", q_record("Q1 again", 1))
            .await
            .status(),
        201,
        "a key dropped before the fold is free after it"
    );
    let mut served = served;
    reauthorise(&mut served).await;
    assert_eq!(
        points(&served, "quarter:2026-Q1").await.len(),
        0,
        "the recreated key is an empty view, not the build's own rows under a new record"
    );
    drop_view(&served, "quarter", "2026-Q1", false).await;
    reauthorise(&mut served).await;

    // (e) **`delete_dangling`'s deletions retire at the fold that omits their view's segments**
    // (`views.md` §3.4, Rule F, write-path §5.4). The two removal rules meet here and only here:
    // the *view* goes by omission at the publication, and the *entities* the drop submitted go the
    // ordinary way, through the overlay, at the fold that executes them. Nothing else in this file
    // reaches the second half — a drop's own test ends at the acknowledgement — and the reasoning
    // that they compose is exactly the reasoning that has been wrong twice.
    //
    // The four entities ingested into `2026-Q2` hold a row in that view and nowhere else: they
    // were minted by that batch, and `quarter_map:2026-Q2` was created empty beside it. So the
    // drop's probe finds all four dangling.
    let before = served.server.state.engine.retirable_deletions();
    let body = drop_view(&served, "quarter", "2026-Q2", true).await;
    assert_eq!(
        body["deleted"], 4,
        "every entity of the view was in no other: {body}"
    );
    assert_eq!(
        served.server.state.engine.retirable_deletions(),
        before + 4,
        "the dangling entities are ordinary deletions and enter the overlay"
    );

    fold(&served).await;
    assert_eq!(
        served.server.state.engine.retirable_deletions(),
        0,
        "the fold executed them, so their overlay entries retire (Rule F) — an entity carried \
         forward by a segment the fold kept would not have"
    );
    let (_, second) = live_prefix(&served);
    assert_eq!(
        segment_views(&second),
        vec!["world".to_string()],
        "and the second dropped view left the plan as the first did"
    );
}

/// **The join rule's label arm past its own flush** (`views.md` §4). Until the entity→term
/// transpose existed the arm compared against the commit window's buffer alone, so a second view's
/// row naming a *different* label for an already-flushed entity was accepted — inert, the row
/// carrying no descriptors, but unreported. It is now the `409` the rule always specified.
///
/// The order matters and is the whole test: ingest, **flush** (so the buffer no longer holds the
/// entity's own row), then join. A join before the flush is already refused by the old arm, so a
/// test that skipped the flush would pass against the code this one exists to check.
#[tokio::test]
async fn a_join_naming_a_different_label_is_refused_after_the_entity_has_flushed() {
    let served = serve().await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5", 1))
            .await
            .status(),
        201
    );
    let id = b"flushed-label".to_vec();
    assert_eq!(
        ingest(
            &served,
            "first",
            "world",
            &[(id.clone(), 10.0, 10.0, "0", Some(7))]
        )
        .await
        .status(),
        200
    );
    flush(&served).await;

    let resp = ingest(
        &served,
        "relabel-after-flush",
        "quarter:2026-Q5",
        &[(id.clone(), 800.0, 300.0, "1", Some(7))],
    )
    .await;
    assert_eq!(
        resp.status(),
        409,
        "a second view's row is not a route to a new access label, buffered or flushed"
    );
    let body: Value = resp.json().await.unwrap();
    let detail = body["detail"].as_str().unwrap();
    assert!(
        detail.contains("under a different access label"),
        "the refusal names the rule it is enforcing: {detail}"
    );
    assert!(
        !detail.contains("'0'") && !detail.contains("'1'"),
        "and names no descriptor: the refusal names the row, never either label: {detail}"
    );

    // **The same batch with the entity's own label still joins**, which is what keeps this a
    // refusal of a re-label rather than a refusal of the join rule itself.
    assert_eq!(
        ingest(
            &served,
            "join-after-flush",
            "quarter:2026-Q5",
            &[(id.clone(), 800.0, 300.0, "0", Some(7))]
        )
        .await
        .status(),
        200,
        "the label the entity already carries is not a change, and the join lands"
    );
}

// ---------------------------------------------------------------------------------------------
// The join rule's **attribute arm past the flush** (`views.md` §4) — its own fixture, because the
// arm has three storage homes and the fixture above declares one column, which reaches one of
// them. What is at stake is not that a value is stored but that the *comparison* can still be made
// once the entity's own row has left the commit-window buffer: before `Engine::flushed_scalar`
// this arm simply stopped there, so a join naming a flushed entity under a different value was
// accepted — silently for two homes, and *destructively* for the third, a rendered column's value
// travelling in the joining row's own tail into the joined view's hot column.
// ---------------------------------------------------------------------------------------------

/// The five columns, chosen so the three homes and three families are each reached by one
/// (records §3, decision 0068):
///
/// | column | family | home |
/// |---|---|---|
/// | `score` | numeric | the **hot column** — `render`, no `index`, so no entity-space column is owed |
/// | `depth` | numeric | the **value column** — `index` |
/// | `tag` | keyword | the **value column**, through this layer's sorted dictionary |
/// | `note` | keyword | the **record blob** — neither flag, so it has no other home |
/// | `archive` | category | the **hot column**, its code, a `public` vocabulary owing no floor |
const FAMILIES_SCHEMA: &str = r#"
[[vocabulary]]
name       = "archive"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  astro = 11
  cond = 22
  hep = 33

[[attribute]]
name   = "score"
type   = "i32"
render = true

[[attribute]]
name  = "depth"
type  = "i32"
index = true

[[attribute]]
name  = "tag"
type  = "keyword"
index = true

[[attribute]]
name = "note"
type = "keyword"

[[attribute]]
name       = "archive"
type       = "category"
render     = true
vocabulary = "archive"
"#;

fn write_families_points(path: &Path, view: &str, ids: std::ops::Range<u64>) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("score", DataType::Int32, true),
        Field::new("depth", DataType::Int32, true),
        Field::new("tag", DataType::Utf8, true),
        Field::new("note", DataType::Utf8, true),
        Field::new("archive", DataType::Utf8, false),
    ]));
    let ids: Vec<u64> = ids.collect();
    let xs: Vec<f64> = ids.iter().map(|&e| position(view, e).0).collect();
    let ys: Vec<f64> = ids.iter().map(|&e| position(view, e).1).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(arrow::array::Int32Array::from_iter(
                ids.iter().map(|&e| Some(e as i32)),
            )),
            Arc::new(arrow::array::Int32Array::from_iter(
                ids.iter().map(|&e| Some(e as i32)),
            )),
            Arc::new(arrow::array::StringArray::from_iter_values(
                ids.iter().map(|e| format!("t{e}")),
            )),
            Arc::new(arrow::array::StringArray::from_iter_values(
                ids.iter().map(|e| format!("n{e}")),
            )),
            Arc::new(arrow::array::StringArray::from_iter_values(
                ids.iter()
                    .map(|e| ["astro", "cond", "hep"][(e % 3) as usize]),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// One plain view and one group, as the fixture above, over [`FAMILIES_SCHEMA`]'s five columns.
async fn serve_families() -> Served {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, ENTITIES);
    let world_points = dir.join("world.parquet");
    write_families_points(&world_points, "world", WORLD);
    let q1_points = dir.join("quarter-q1.parquet");
    write_families_points(&q1_points, "quarter:2026-Q1", Q1);
    let schema_path = dir.join("schema.toml");
    std::fs::write(&schema_path, FAMILIES_SCHEMA).unwrap();
    let config = tessera_build::config::Config::parse(&schema_path, &Default::default())
        .expect("the families declaration parses");
    let out = dir.join("bundle");
    build(&BuildArgs {
        views: vec![
            view_args("world", &world_points, &pairs),
            view_args("quarter:2026-Q1", &q1_points, &pairs),
        ],
        anchor: 0,
        groups: vec![GroupDescriptor {
            title: None,
            visibility: None,
            scoped_scalars: Vec::new(),
            name: "quarter".to_string(),
            members_of: None,
            quantisation: group_frame(),
            projection: tessera_spatial::Projection::None,
            metadata: Vec::new(),
            views: vec![GroupViewDescriptor {
                key: "2026-Q1".to_string(),
                visibility: None,
                metadata: Default::default(),
            }],
        }],
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(
            world_points.clone(),
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
    .expect("the families build succeeds");
    open(tmp).await
}

/// One value per declared column, in declaration order, at the shape the wire carries: `None` is a
/// null cell, which is *absence* for every family but a category, whose absence is its reserved
/// code and which therefore never travels as null at all.
#[derive(Clone, Copy)]
struct Attrs<'a> {
    score: Option<i32>,
    depth: Option<i32>,
    tag: Option<&'a str>,
    note: Option<&'a str>,
    archive: Option<&'a str>,
}

const HELD: Attrs<'static> = Attrs {
    score: Some(1),
    depth: Some(2),
    tag: Some("alpha"),
    note: Some("nb"),
    archive: Some("astro"),
};

fn families_batch(id: &[u8], x: f32, y: f32, a: Attrs<'_>) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("access", DataType::Utf8, false),
        Field::new("score", DataType::Int32, true),
        Field::new("depth", DataType::Int32, true),
        Field::new("tag", DataType::Utf8, true),
        Field::new("note", DataType::Utf8, true),
        Field::new("archive", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(arrow::array::BinaryArray::from_iter([Some(id)])),
            Arc::new(arrow::array::Float32Array::from_iter_values([x])),
            Arc::new(arrow::array::Float32Array::from_iter_values([y])),
            Arc::new(arrow::array::StringArray::from_iter_values(["0"])),
            Arc::new(arrow::array::Int32Array::from_iter([a.score])),
            Arc::new(arrow::array::Int32Array::from_iter([a.depth])),
            Arc::new(arrow::array::StringArray::from_iter([a.tag])),
            Arc::new(arrow::array::StringArray::from_iter([a.note])),
            Arc::new(arrow::array::StringArray::from_iter([a.archive])),
        ],
    )
    .unwrap();
    let mut writer = arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

async fn families_ingest(
    served: &Served,
    batch_id: &str,
    view: &str,
    id: &[u8],
    x: f32,
    y: f32,
    a: Attrs<'_>,
) -> reqwest::Response {
    served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", view)
        .header("content-type", "application/octet-stream")
        .body(families_batch(id, x, y, a))
        .send()
        .await
        .unwrap()
}

/// **The attribute arm is exact past the flush**, over all three homes and all three families
/// (`views.md` §4): the same values join, a differing one is a 409 naming the column, and an
/// absent one is not disagreement.
#[tokio::test]
async fn a_join_compares_attribute_values_after_the_entity_has_flushed() {
    let mut served = serve_families().await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", json!({ "metadata": {} }))
            .await
            .status(),
        201
    );
    reauthorise(&mut served).await;

    let id = b"families".to_vec();
    assert_eq!(
        families_ingest(&served, "first", "world", &id, 10.0, 10.0, HELD)
            .await
            .status(),
        200
    );
    // **The flush is the whole point**: past it the entity's own row is out of the buffer, and
    // every comparison below is made against the bundle.
    flush(&served).await;
    assert_eq!(served.server.state.engine.buffered_items(), 0);

    // (a) The same values, in a view the entity is not in: a join.
    assert_eq!(
        families_ingest(&served, "same", "quarter:2026-Q5", &id, 800.0, 300.0, HELD)
            .await
            .status(),
        200,
        "a joining row that byte-matches every stored value is the join views §4 specifies"
    );

    // (b) One differing value per home and per family, each a 409 naming its column. Each runs
    // against `quarter:2026-Q1`, a view the entity is *also* not in, because the join above has
    // now put it in `2026-Q5` and a second row there would be refused as a duplicate instead.
    for (column, differing) in [
        (
            "score",
            Attrs {
                score: Some(9),
                ..HELD
            },
        ),
        (
            "depth",
            Attrs {
                depth: Some(9),
                ..HELD
            },
        ),
        (
            "tag",
            Attrs {
                tag: Some("beta"),
                ..HELD
            },
        ),
        (
            "note",
            Attrs {
                note: Some("other"),
                ..HELD
            },
        ),
        (
            "archive",
            Attrs {
                archive: Some("hep"),
                ..HELD
            },
        ),
    ] {
        let resp = families_ingest(
            &served,
            &format!("differ-{column}"),
            "quarter:2026-Q1",
            &id,
            700.0,
            200.0,
            differing,
        )
        .await;
        assert_eq!(
            resp.status(),
            409,
            "a flushed entity's stored '{column}' is read back and compared"
        );
        let body: Value = resp.json().await.unwrap();
        assert!(
            body["detail"].as_str().unwrap().contains(column),
            "the refusal names the column: {body}"
        );
    }

    // (c) Absent is not disagreement, for every family — a category included, whose absence is its
    // reserved code rather than a null cell.
    assert_eq!(
        families_ingest(
            &served,
            "absent",
            "quarter:2026-Q1",
            &id,
            700.0,
            200.0,
            Attrs {
                score: None,
                depth: None,
                tag: None,
                note: None,
                archive: None,
            },
        )
        .await
        .status(),
        200,
        "a joining row byte-matches the stored value or omits it"
    );
}

/// (d) **A value for a column the entity never held is accepted** — the arm the spec does not
/// state, resolved to the behaviour the buffered arm has always had (a held `None` continues).
///
/// The rule `views.md` §4 states is one-directional: a joining row must not *change* a stored
/// value. An entity holding nothing for a column has nothing to change, and the row writes nothing
/// into entity space, so there is no value for the two to disagree about. Recorded here so the
/// choice is a test rather than an accident; if it is ever ruled the other way this is the test
/// that moves.
#[tokio::test]
async fn a_join_may_carry_a_value_for_a_column_the_entity_never_held() {
    let mut served = serve_families().await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", json!({ "metadata": {} }))
            .await
            .status(),
        201
    );
    reauthorise(&mut served).await;

    let id = b"never-held".to_vec();
    // **`archive` is absent too, and a category says that with its reserved code** rather than a
    // null cell (per-point-attributes §3.4) — so this row also pins `is_absent_value` on the
    // *flushed* side: the code read back out of the hot column is absence, not a value `hep` could
    // contradict. Reading absence only in its `null` spelling made this arm a 409.
    let sparse = Attrs {
        score: None,
        depth: None,
        tag: None,
        note: None,
        archive: None,
    };
    assert_eq!(
        families_ingest(&served, "sparse", "world", &id, 20.0, 20.0, sparse)
            .await
            .status(),
        200
    );
    flush(&served).await;

    assert_eq!(
        families_ingest(
            &served,
            "supply",
            "quarter:2026-Q5",
            &id,
            800.0,
            300.0,
            Attrs {
                score: Some(4),
                depth: Some(5),
                tag: Some("gamma"),
                note: Some("later"),
                archive: Some("hep"),
            },
        )
        .await
        .status(),
        200,
        "an entity holding no value for a column has none for a joining row to contradict"
    );
}

/// **The two arms are one arm**: the refusal a *buffered* entity's mismatch produces is byte for
/// byte the refusal a *flushed* entity's produces.
///
/// This is what stops the fix from being half a fix. Two arms with two messages would let an
/// operator reading a report tell which side of a flush a batch landed on — a distinction the rule
/// does not draw and an implementation detail no report should carry — and would be the first
/// place the two comparisons drifted apart.
#[tokio::test]
async fn the_buffered_and_flushed_attribute_arms_refuse_identically() {
    let mut served = serve_families().await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", json!({ "metadata": {} }))
            .await
            .status(),
        201
    );
    reauthorise(&mut served).await;

    let flushed = b"arm-flushed".to_vec();
    assert_eq!(
        families_ingest(&served, "f-first", "world", &flushed, 30.0, 30.0, HELD)
            .await
            .status(),
        200
    );
    flush(&served).await;

    // The second entity is ingested *after* the flush, so its own row is still in the buffer.
    let buffered = b"arm-buffered".to_vec();
    assert_eq!(
        families_ingest(&served, "b-first", "world", &buffered, 40.0, 40.0, HELD)
            .await
            .status(),
        200
    );

    let differing = Attrs {
        score: Some(9),
        ..HELD
    };
    let mut bodies = Vec::new();
    for (batch_id, id) in [("f-join", &flushed), ("b-join", &buffered)] {
        let resp = families_ingest(
            &served,
            batch_id,
            "quarter:2026-Q5",
            id,
            800.0,
            300.0,
            differing,
        )
        .await;
        assert_eq!(resp.status(), 409);
        bodies.push(resp.text().await.unwrap());
    }
    assert_eq!(
        bodies[0], bodies[1],
        "one rule, one message: the flushed arm's refusal is the buffered arm's"
    );
}

/// One view's points under a filter, so a test can ask what a **view's own tail** carries rather
/// than what the drill-down reports. The drill-down answers from the first view holding a row and
/// is therefore blind to a disagreement between two of them, which is exactly the property under
/// test here; a `render` column is filterable through the row route (decision 0068), and a filter
/// evaluated against one view's rows is that view's tail and no other's.
async fn filtered_points(served: &Served, view: &str, filter: Value) -> Vec<PointRow> {
    // A 429 is the admission gate shedding under machine load and a stale hint is
    // serve-stale-not-block — neither is the answer under test; retry both, as points() does.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let resp = served
            .server
            .client
            .post(served.server.viewer_url("/v1/viewport"))
            .bearer_auth(&served.token)
            .json(&json!({
                "view": view, "zoom": 8, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200,
                "filters": filter.clone()
            }))
            .send()
            .await
            .unwrap();
        let unsettled = std::time::Instant::now() < deadline;
        if resp.status().as_u16() == 429 && unsettled {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            continue;
        }
        assert_eq!(
            resp.status().as_u16(),
            200,
            "a filtered view answers: {view}"
        );
        if resp
            .headers()
            .get("x-tessera-stale")
            .is_some_and(|v| v == "1")
            && unsettled
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        }
        return decode_viewport(&resp.bytes().await.unwrap()).1;
    }
}

/// **An omitted render value is backfilled into the joined view's tail** (`views.md` §4, owner
/// ruling 2026-08-31), not written there as an absence.
///
/// A joining row is geometry-only in *entity* space, but a `render` column's value travels in its
/// own row tail — so a batch that lawfully omitted one would leave the joined view rendering
/// nothing for a point every other view renders a value for. An entity-scoped attribute is one
/// value per entity (§5); a value that reads differently under two views is not one.
#[tokio::test]
async fn an_omitted_render_value_is_backfilled_into_the_joined_views_tail() {
    let mut served = serve_families().await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", json!({ "metadata": {} }))
            .await
            .status(),
        201
    );
    reauthorise(&mut served).await;

    let id = b"backfill".to_vec();
    let resp = families_ingest(&served, "first", "world", &id, 10.0, 10.0, HELD).await;
    assert_eq!(resp.status(), 200);
    let tessera_id: u64 = resp.json::<Value>().await.unwrap()["tessera_ids"][0]
        .as_u64()
        .unwrap();
    flush(&served).await;

    // The join omits every value, which the rule permits — and which is what would otherwise put
    // an absence in this view's tail.
    assert_eq!(
        families_ingest(
            &served,
            "omit",
            "quarter:2026-Q5",
            &id,
            800.0,
            300.0,
            Attrs {
                score: None,
                depth: None,
                tag: None,
                note: None,
                archive: None,
            },
        )
        .await
        .status(),
        200
    );
    flush(&served).await;

    let score_is_one = json!({ "score": { "eq": 1 } });
    for view in ["world", "quarter:2026-Q5"] {
        let matched = filtered_points(&served, view, score_is_one.clone()).await;
        assert!(
            matched.iter().any(|(id, _)| *id == tessera_id),
            "the entity renders its one stored score under '{view}': {matched:?}"
        );
    }
}

/// **The oracle scans every view, and an absence in one does not answer for a value in another**
/// (r24 review F1).
///
/// The scan used to stop at the first view whose permutation held a row, over a `HashMap` of
/// partitions and a `HashMap` of views, and to read that row's clear presence bit as *no value
/// held*. Two views can hold different tails lawfully — this test builds the case the backfill
/// does not close, an entity holding **no** value in the view it was ingested into and being
/// joined into a second view *with* one — and under first-view-wins the third view's join then
/// answered `200` or `409` by hash order.
///
/// **Each repetition takes a fresh server**, which is what varies the order: a `HashMap`'s
/// iteration order is fixed for the life of one map, so a loop inside one process re-reads the
/// same order however many times it runs. The in-process loop below is there for the cheaper
/// half — that one process answers one way every time — and the outer repetitions for the half
/// that actually flips.
#[tokio::test]
async fn the_value_oracle_does_not_let_one_views_absence_answer_for_anothers_value() {
    for attempt in 0..5 {
        let mut served = serve_families().await;
        for key in ["2026-Q5", "2026-Q6"] {
            assert_eq!(
                create(&served, "quarter", key, json!({ "metadata": {} }))
                    .await
                    .status(),
                201
            );
        }
        reauthorise(&mut served).await;

        // Held in no view: `score` is absent where the entity was first ingested.
        let id = b"divergent".to_vec();
        let sparse = Attrs {
            score: None,
            depth: None,
            tag: None,
            note: None,
            archive: Some("astro"),
        };
        assert_eq!(
            families_ingest(&served, "first", "world", &id, 15.0, 15.0, sparse)
                .await
                .status(),
            200
        );
        flush(&served).await;

        // Accepted: an entity holding nothing for a column has nothing a joining row contradicts.
        // The joined view's tail now carries `4` where `world`'s carries an absence — the one
        // lawful disagreement the backfill does not close, because there was no stored value to
        // backfill from.
        assert_eq!(
            families_ingest(
                &served,
                "supply",
                "quarter:2026-Q5",
                &id,
                800.0,
                300.0,
                Attrs {
                    score: Some(4),
                    ..sparse
                },
            )
            .await
            .status(),
            200
        );
        flush(&served).await;

        // A third view, a differing value: `409`, every time, whichever view the scan reaches
        // first. Reading `world`'s absence as the answer would accept it.
        for round in 0..10 {
            let resp = families_ingest(
                &served,
                &format!("third-{attempt}-{round}"),
                "quarter:2026-Q6",
                &id,
                700.0,
                200.0,
                Attrs {
                    score: Some(7),
                    ..sparse
                },
            )
            .await;
            assert_eq!(
                resp.status(),
                409,
                "attempt {attempt}, round {round}: one view's absence must not answer for \
                 another's value"
            );
            let body: Value = resp.json().await.unwrap();
            assert!(
                body["detail"].as_str().unwrap().contains("score"),
                "the refusal names the column: {body}"
            );
        }
    }
}

/// **A key dropped and created again adopts nothing of its predecessor's, across a restart that
/// replays the log and across the fold that reclaims the files**
/// ([decision 0115](../../../docs/decisions/0115-a-dropped-view-key-is-reusable.md)).
///
/// This is the whole reason the burn existed, and the reason the incarnation replaces it. A
/// dropped view's segments, columns and buffered rows outlive the drop — the segments until a fold
/// reclaims them, the buffered rows until the log is replayed — so a recreate that resolved
/// artifacts by *view id* would serve the old view's points under the new name. Nothing about that
/// is visible from the outside: the answer would simply be wrong.
///
/// Three mechanisms, and each needs its own arm because each keeps the predecessor out at a
/// different layer:
///
/// - **The segments.** The first batch is flushed, so `2026-Q5` has a segment on disc when it is
///   dropped. The recreated key's row space must not adopt it — `Bundle::with_views` blanks a view
///   whose data carries a dead incarnation, and the fold's carry-forward omits its descriptors.
/// - **The buffered rows.** The second batch is *not* flushed before the restart, so it is in the
///   WAL and nowhere else — and so is the first batch's, whose member the flush has not yet let
///   go. Replay meets `ViewDrop` between them and discards the rows of the incarnation it kills,
///   which is what makes the ordered replay the resolution site rather than a stamp on every row.
/// - **The manifest's roster.** The drop and the recreate happen in one window with no publication
///   between them, so both reach the side manifest as one `(views, dead_view_incarnations)` pair —
///   which is the case that decides whether `with_roster` applies the deaths before the creations
///   or after. The wrong order deletes the view the caller was just told it had.
///
/// **Nothing here is visible on a wire.** The incarnation is on no response, so the assertions are
/// about *contents*: the recreated view holds the second batch's four rows and none of the first's.
#[tokio::test]
async fn a_recreated_key_holds_only_its_own_rows_across_a_replay_and_a_fold() {
    let mut served = serve().await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5 first", 1))
            .await
            .status(),
        201
    );
    reauthorise(&mut served).await;

    // The first incarnation's rows, flushed, so the view owns a segment and a row space.
    let first: Vec<Row<'_>> = (0..6)
        .map(|i| {
            (
                format!("first-{i}").into_bytes(),
                300.0 + i as f32,
                300.0,
                "0",
                Some(i),
            )
        })
        .collect();
    assert_eq!(
        ingest(&served, "first-batch", "quarter:2026-Q5", &first)
            .await
            .status(),
        200
    );
    flush(&served).await;
    assert_eq!(points(&served, "quarter:2026-Q5").await.len(), 6);

    // **And rows that never flushed**, so the drop below meets them in the buffer and the restart
    // meets them in the log. Without this arm the buffered half of the hazard is invisible: the
    // flushed rows above are dropped from the replayed buffer by the ordinary "this row already
    // has geometry" filter, whatever the drop does.
    let stale: Vec<Row<'_>> = (0..3)
        .map(|i| {
            (
                format!("stale-{i}").into_bytes(),
                320.0 + i as f32,
                320.0,
                "0",
                Some(i),
            )
        })
        .collect();
    assert_eq!(
        ingest(&served, "stale-batch", "quarter:2026-Q5", &stale)
            .await
            .status(),
        200
    );

    // **Drop and recreate in one window** — no flush, no fold, no publication between them.
    assert_eq!(
        drop_view(&served, "quarter", "2026-Q5", false).await["deleted"],
        0
    );
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5 second", 2))
            .await
            .status(),
        201,
        "a dropped key is reusable, and in the same window"
    );
    reauthorise(&mut served).await;
    assert_eq!(
        points(&served, "quarter:2026-Q5").await.len(),
        0,
        "the recreated key is an empty view: the predecessor's segment is on disc and unreachable"
    );

    // The second incarnation's rows, left **unflushed**, so the restart below has to replay them.
    let second: Vec<Row<'_>> = (0..4)
        .map(|i| {
            (
                format!("second-{i}").into_bytes(),
                500.0 + i as f32,
                500.0,
                "0",
                Some(i),
            )
        })
        .collect();
    assert_eq!(
        ingest(&served, "second-batch", "quarter:2026-Q5", &second)
            .await
            .status(),
        200
    );

    // ---- the replay ------------------------------------------------------------------------
    let mut served = restart(served).await;
    reauthorise(&mut served).await;
    let document = meta(&served).await;
    assert_eq!(
        roster_of(&document, "quarter:2026-Q5").unwrap()["metadata"]["label"]["value"],
        "Q5 second",
        "the surviving record is the recreate's, so the deaths were applied before the creations"
    );
    // The rows are in the buffer and nowhere else, so the flush is what gives them geometry —
    // and what makes the count below a statement about *which* rows replay kept.
    flush(&served).await;
    let after_replay = points(&served, "quarter:2026-Q5").await;
    assert_eq!(
        after_replay.len(),
        4,
        "the replayed view holds the second batch and none of the first: {}",
        after_replay.len()
    );

    // ---- the fold --------------------------------------------------------------------------
    fold(&served).await;
    let (_, folded_dir) = live_prefix(&served);
    assert!(
        segment_views(&folded_dir).contains(&"quarter:2026-Q5".to_string()),
        "the recreated view is folded like any other"
    );
    let served = restart(served).await;
    assert_eq!(
        points(&served, "quarter:2026-Q5").await.len(),
        4,
        "and the fold reclaimed the dead incarnation's files without touching the live one's"
    );
}

/// **A drop expands to every id the key names, on both prune paths**
/// ([decision 0115](../../../docs/decisions/0115-a-dropped-view-key-is-reusable.md), `views.md`
/// §3.3).
///
/// A key is one view of the group that owns it *and* one of every group sharing its views, and
/// `Manifest::with_roster` has always taken all of them off the roster together. The two buffer
/// prunes did not: each built a single `<group>:<key>` — the live path from the group the *request*
/// named, the replay arm from the record's owner — so rows buffered under the other spelling
/// survived the drop and flushed into the view created next under that key. Points of a view the
/// operator dropped, served under a name they had just recreated, with nothing to see.
///
/// Both directions are here because the two sites fail the opposite way round:
///
/// - **the live path**, whose id came from the group the *request* named, so a drop addressed to
///   the owner left the sharing group's buffered rows in the generation;
/// - **the replay arm**, whose id came from the record — and a `ViewDrop` record always carries
///   the **owner**, so the sharing group's rows were the ones it never named, whichever spelling
///   the operator used.
///
/// The two halves therefore differ in *where the rows have to be kept out of*, not in which
/// spelling the drop uses: the second ingests through the sharing group and never flushes before
/// the restart, so the rows exist only in the log and the replay arm is the one thing standing
/// between them and the recreated key.
#[tokio::test]
async fn a_drop_prunes_every_spelling_of_the_key_on_the_live_path_and_at_replay() {
    let mut served = serve().await;

    // ---- (A) the live path: drop on the owner, rows buffered under the sharing group ---------
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5 first", 1))
            .await
            .status(),
        201
    );
    reauthorise(&mut served).await;
    let shared: Vec<Row<'_>> = (0..5)
        .map(|i| {
            (
                format!("shared-{i}").into_bytes(),
                600.0 + i as f32,
                600.0,
                "0",
                Some(i),
            )
        })
        .collect();
    assert_eq!(
        ingest(&served, "shared-batch", "quarter_map:2026-Q5", &shared)
            .await
            .status(),
        200,
        "the fixture accepts a batch through the sharing group's spelling"
    );

    // Dropped by the **owner's** name, which is the spelling the live prune used to build from.
    assert_eq!(
        drop_view(&served, "quarter", "2026-Q5", false).await["deleted"],
        0
    );
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5 second", 2))
            .await
            .status(),
        201
    );
    reauthorise(&mut served).await;
    // **One row of the new incarnation's own**, so the flush below has work and the count is a
    // statement rather than an empty buffer's silence: five stale rows would make it six.
    assert_eq!(
        ingest(
            &served,
            "fresh-q5",
            "quarter:2026-Q5",
            &[(b"fresh-q5".to_vec(), 610.0, 610.0, "0", Some(0))],
        )
        .await
        .status(),
        200
    );
    flush(&served).await;
    for id in ["quarter:2026-Q5", "quarter_map:2026-Q5"] {
        assert_eq!(
            points(&served, id).await.len(),
            if id == "quarter:2026-Q5" { 1 } else { 0 },
            "{id}: the recreated key adopted rows buffered under the other spelling"
        );
    }

    // ---- (B) the replay arm: rows buffered under the sharing group, and no flush ------------
    drop_view(&served, "quarter", "2026-Q5", false).await;
    assert_eq!(
        create(&served, "quarter", "2026-Q6", q_record("Q6 first", 3))
            .await
            .status(),
        201
    );
    reauthorise(&mut served).await;
    let owned: Vec<Row<'_>> = (0..5)
        .map(|i| {
            (
                format!("owned-{i}").into_bytes(),
                700.0 + i as f32,
                700.0,
                "0",
                Some(i),
            )
        })
        .collect();
    assert_eq!(
        ingest(&served, "owned-batch", "quarter_map:2026-Q6", &owned)
            .await
            .status(),
        200
    );
    // Dropped by the **sharing group's** name, which changes nothing about the record: a
    // `ViewDrop` always carries the owner, so a replay prune built from the record alone looks
    // under `quarter:2026-Q6` and never under the id these rows are actually in.
    assert_eq!(
        drop_view(&served, "quarter_map", "2026-Q6", false).await["deleted"],
        0
    );
    assert_eq!(
        create(&served, "quarter", "2026-Q6", q_record("Q6 second", 4))
            .await
            .status(),
        201
    );
    // **No flush before the restart**: the rows are in the log and nowhere else, so what keeps
    // them out of the recreated view is replay's own `ViewDrop` arm.
    let mut served = restart(served).await;
    reauthorise(&mut served).await;
    assert_eq!(
        ingest(
            &served,
            "fresh-q6",
            "quarter:2026-Q6",
            &[(b"fresh-q6".to_vec(), 710.0, 710.0, "0", Some(0))],
        )
        .await
        .status(),
        200
    );
    flush(&served).await;
    for id in ["quarter:2026-Q6", "quarter_map:2026-Q6"] {
        assert_eq!(
            points(&served, id).await.len(),
            if id == "quarter:2026-Q6" { 1 } else { 0 },
            "{id}: the replayed drop left rows under a spelling the record did not name"
        );
    }
    assert_eq!(
        roster_of(&meta(&served).await, "quarter:2026-Q6").unwrap()["metadata"]["label"]["value"],
        "Q6 second",
        "and the surviving record is the recreate's"
    );
}
