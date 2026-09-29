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

use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_build::{
    build, BuildArgs, GroupDescriptor, GroupMetadataField, GroupViewDescriptor, ViewArgs,
    ViewMetadataType, ViewMetadataValue,
};

const ENTITIES: u64 = 20;
/// The plain view holds every entity; the group's one declared view holds the first half, so
/// dropping it leaves every declared item in a view and deletes none.
const WORLD: std::ops::Range<u64> = 0..ENTITIES;
const Q1: std::ops::Range<u64> = 0..10;
/// The first `id` above the fixture's, for an item a test creates and names again.
const NEW_ID: u64 = 1_000;

fn position(view: &str, e: u64) -> (f64, f64) {
    match view.split_once(':') {
        None => ((e % 5) as f64 * 100.0, (e / 5) as f64 * 100.0),
        Some(_) => (900.0 - (e % 5) as f64 * 100.0, (e / 5) as f64 * 70.0),
    }
}

/// One view's points. `key` is the discriminator value every row carries, for the group whose
/// points are one file behind `fields.view`; `None` is a file that *is* the view.
fn write_view_points(path: &Path, view: &str, ids: std::ops::Range<u64>, key: Option<&str>) {
    let ids: Vec<u64> = ids.collect();
    let score = arrow::array::Int32Array::from(ids.iter().map(|&e| e as i32).collect::<Vec<_>>());
    // One declared attribute, so the join rule's entity-scoped arm has a column to disagree
    // about.
    let mut extra = vec![column("score", true, score)];
    if let Some(key) = key {
        extra.push(column(
            "quarter",
            false,
            arrow::array::StringArray::from(vec![key; ids.len()]),
        ));
    }
    write_points(path, &ids, |e| position(view, e), extra);
}

/// One plain view, a group of one view carrying two metadata names, and a second group over the
/// same keys — the three shapes a create has to get right (`views.md` §3.1, §3.3).
fn build_fixture_bundle(dir: &Path) -> std::path::PathBuf {
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, ENTITIES);
    let world_points = dir.join("world.parquet");
    write_view_points(&world_points, "world", WORLD, None);
    let mut views = vec![view_args(
        "world",
        &world_points,
        AccessInput::relation(&pairs),
    )];
    for group in ["quarter", "quarter_map"] {
        let id = format!("{group}:2026-Q1");
        let points = dir.join(format!("{group}-q1.parquet"));
        write_view_points(&points, &id, Q1, None);
        views.push(view_args(&id, &points, AccessInput::relation(&pairs)));
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
    // The declaration, for its schema alone: an entity-space attribute and the unique `id` over
    // the world file, which is the file that holds every entity.
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
"#
        .to_string()
            + ID_ATTRIBUTE,
    )
    .unwrap();
    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("the fixture declaration parses");
    let out = dir.join("bundle");
    build(&BuildArgs {
        groups: vec![
            GroupDescriptor {
                title: None,
                point_default: Some("public".to_string()),
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
                point_default: Some("public".to_string()),
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
        attribute_sources: tessera_build::config::AttributeSource::over(
            dir.join("world.parquet"),
            &config.schema,
        ),
        schema: config.schema,
        ..build_args(&out, views)
    })
    .expect("a three-view build succeeds");
    out
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

async fn drop_view(served: &Served, group: &str, key: &str) -> Value {
    let resp = served
        .server
        .client
        .delete(served.server.control_url(&format!("/control/views/{group}/{key}")))
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
    // A session whose cache entry predates the last flush is served from that entry while the
    // refresh runs, with no header, so a test that reads rows published since its session was
    // authorised re-authorises first or polls for the count it expects.
    let resp = settled(async || viewport(served, view).await).await;
    decode_viewport(&resp.bytes().await.unwrap()).1
}

/// One ingest row: the `id` naming its item, or `None` for a new item without one, its position,
/// its access labels and the declared `score`.
type Row<'a> = (Option<u64>, f32, f32, &'a [&'a str], Option<i32>);

/// One ingest body.
fn batch(rows: &[Row<'_>]) -> Vec<u8> {
    let labels: Vec<&[&str]> = rows.iter().map(|(_, _, _, access, _)| *access).collect();
    let access = access_lists(&labels);
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt64, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access),
        Field::new("score", DataType::Int32, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(arrow::array::UInt64Array::from_iter(
                rows.iter().map(|(id, ..)| *id),
            )),
            Arc::new(arrow::array::Float32Array::from_iter_values(
                rows.iter().map(|(_, x, ..)| *x),
            )),
            Arc::new(arrow::array::Float32Array::from_iter_values(
                rows.iter().map(|(_, _, y, ..)| *y),
            )),
            Arc::new(access),
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
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(body)
        .send()
        .await
        .unwrap()
}

/// One JSON ingest batch into `view`, each record as sent: a key a record leaves out is a column
/// the row leaves out, which keeps what the item it names stores.
async fn ingest_json(served: &Served, batch_id: &str, view: &str, rows: Value) -> reqwest::Response {
    served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", view)
        .json(&rows)
        .send()
        .await
        .unwrap()
}

/// An accepted ingest response's body.
async fn accepted(resp: reqwest::Response) -> Value {
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    body
}

/// A JSON record naming the item holding `id` at `(x, y)` and carrying no other column.
fn located(id: u64, x: f32, y: f32) -> Value {
    json!({ "id": id, "x": x, "y": y })
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
    let mut served = Served::build(build_fixture_bundle).await;

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
    served.reauthorise().await;

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
                None,
                100.0 + i as f32,
                200.0,
                &["0"][..],
                Some(i),
            )
        })
        .collect();
    let resp = ingest(&served, "q5-batch", "quarter:2026-Q5", &rows).await;
    assert_eq!(resp.status(), 200, "a created view accepts rows");
    drain(&served.server).await;

    // The flush's publication and a live session's sight of the entities it minted are two
    // events, and the second follows the first by an asynchronous refresh with no wire signal on
    // the interim response — a viewport between them is a fresh 200 serving the pre-flush answer.
    // So wait for the settled count rather than asserting the first response.
    wait_until(
        "the first flush gives a created view its row space",
        std::time::Duration::from_secs(60),
        async || {
            let settled = points(&served, "quarter:2026-Q5").await.len() == 4;
            if !settled {
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            }
            settled
        },
    )
    .await;

    // The restart: the roster comes back from the segments manifest, and the rows from the
    // segment the flush published.
    let served = served.restart().await;
    let document = meta(&served).await;
    let entry = roster_of(&document, "quarter:2026-Q5").expect("the roster survives a restart");
    assert_eq!(entry["metadata"]["label"]["value"], "Q5 2026");
    assert_eq!(
        points(&served, "quarter:2026-Q5").await.len(),
        4,
        "and so do its rows"
    );
}

/// **A created view stays on `/v1/meta` through a flush that writes no file for it** (issue #151;
/// contracts §3.4 r55: the view is listed and answers a viewer verb, empty, from its
/// acknowledgement).
///
/// The test above creates a view and then *populates* it, so its descriptor is rebuilt by the
/// flush that gives it a row space. This one never puts a row in it, which is the state a client
/// that lists views from meta actually meets: the view was created for rows that have not arrived,
/// and a flush of the rows of *other* views must not take it off the list. A view listed at the
/// acknowledgement and gone one flush later is worse than one never listed — the SDK reads meta to
/// know what exists, and would create it again and take the 409.
#[tokio::test]
async fn a_created_view_with_no_rows_survives_a_flush_and_is_listed_once() {
    let mut served = Served::build(build_fixture_bundle).await;

    let resp = create(
        &served,
        "quarter",
        "2026-Q5",
        q_record("Q5 2026", 1_800_000_000_000_000),
    )
    .await;
    assert_eq!(resp.status(), 201, "a free key on a declared group creates");
    served.reauthorise().await;

    let listed = |document: &Value| {
        view_ids(document)
            .into_iter()
            .filter(|id| id == "quarter:2026-Q5")
            .count()
    };
    let document = meta(&served).await;
    assert_eq!(
        listed(&document),
        1,
        "the created view is listed from its acknowledgement: {:?}",
        view_ids(&document)
    );
    assert_eq!(
        document["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["name"] == "quarter")
            .unwrap()["views"],
        json!(["quarter:2026-Q1", "quarter:2026-Q5"]),
        "and on its group's roster, in creation order"
    );

    // A flush carrying rows for another view, so the created one gets no file of its own.
    let resp = ingest(
        &served,
        "world-batch",
        "world",
        &[(None, 10.0, 20.0, &["0"][..], Some(0))],
    )
    .await;
    assert_eq!(resp.status(), 200);
    drain(&served.server).await;
    served.reauthorise().await;

    let document = meta(&served).await;
    assert_eq!(
        listed(&document),
        1,
        "and is still listed exactly once after a flush that wrote no file for it: {:?}",
        view_ids(&document)
    );
    assert!(
        points(&served, "quarter:2026-Q5").await.is_empty(),
        "it still answers a viewer verb, empty"
    );

    let mut served = served.restart().await;
    served.reauthorise().await;
    let document = meta(&served).await;
    assert_eq!(
        listed(&document),
        1,
        "and comes back from the segments manifest: {:?}",
        view_ids(&document)
    );
}

/// Every arm the create refuses, and the status each takes (`views.md` §3.2). They are told apart
/// because the caller's remedy differs: a refused record is one to correct, a taken or burnt key
/// is one to replace, and an unknown group is not a view at all.
#[tokio::test]
async fn a_create_refuses_a_taken_key_a_bad_key_a_wrong_record_and_an_unknown_group() {
    let served = Served::build(build_fixture_bundle).await;

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
    let empty_list = create(
        &served,
        "quarter",
        "2026-Q5",
        json!({ "visibility": [], "metadata": { "label": "Q5", "starts": 1 } }),
    )
    .await;
    assert_eq!(
        empty_list.status(),
        422,
        "a gate naming no terms is satisfied by nobody, so the view would be reachable by no \
         principal at all (views §6)"
    );
    // **An empty element is refused naming its position** (decision 0132): each element of a
    // gate is one label, and an empty one is no label rather than a term to drop.
    for declared in [json!([""]), json!(["finance", ""])] {
        let resp = create(
            &served,
            "quarter",
            "2026-Q5",
            json!({ "visibility": declared, "metadata": { "label": "Q5", "starts": 1 } }),
        )
        .await;
        assert_eq!(resp.status(), 422, "an empty element is refused: {declared}");
    }
    assert_eq!(
        create(
            &served,
            "quarter",
            "2026-Q5",
            json!({ "visibility": ["public", "finance"], "metadata": { "label": "Q5", "starts": 1 } })
        )
        .await
        .status(),
        422,
        "`public` beside another label is a gate everybody passes, spelled as if narrower"
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
    let mut served = Served::build(build_fixture_bundle).await;
    let created = create(&served, "quarter", "2026-Q5", q_record("Q5", 1)).await;
    assert_eq!(created.status(), 201);

    let body = drop_view(&served, "quarter", "2026-Q5").await;
    assert_eq!(body["deleted"], 0, "an empty view's drop deletes nothing");

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
    served.reauthorise().await;
    let document = meta(&served).await;
    assert_eq!(
        roster_of(&document, "quarter:2026-Q5").unwrap()["metadata"]["label"]["value"],
        "Q5 again",
        "the recreated key carries its own record, not its predecessor's"
    );
    // And drop it again, so the restart below is over a key with two dead incarnations behind it.
    drop_view(&served, "quarter", "2026-Q5").await;

    // A different key still creates: a drop touches the key it named and nothing else.
    let resp = create(&served, "quarter", "2026-Q6", q_record("Q6", 2)).await;
    assert_eq!(resp.status(), 201);

    // And all of it comes back from the segments manifest.
    let served = served.restart().await;
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
    drop_view(&served, "quarter", "2026-Q5").await;
    // A declared view can be dropped too, and the same rules hold for it.
    assert!(view_ids(&document).contains(&"quarter:2026-Q1".to_string()));
    drop_view(&served, "quarter", "2026-Q1").await;
    assert_eq!(viewport(&served, "quarter:2026-Q1").await.status(), 404);
}

/// **One entity, two views** (`views.md` §4): a row naming an existing item by its `id`, in a view
/// the item is not in, is accepted, the row lands in that view's pending segment, and after the flush the point
/// is in both views at its own position in each.
///
/// The entity is untouched by the second batch — that is what makes this a join rather than an
/// update — and the arms below are the ways a caller could try to make it an update by accident.
#[tokio::test]
async fn a_known_id_joins_a_second_view_and_is_placed_in_each() {
    let mut served = Served::build(build_fixture_bundle).await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5", 1))
            .await
            .status(),
        201
    );
    // The visible-view set is fixed per session (`views.md` §6), so a reader of the
    // newly created view takes a new session, exactly as a client would.
    served.reauthorise().await;

    let id = Some(NEW_ID);
    assert_eq!(
        ingest(
            &served,
            "first",
            "world",
            &[(id, 10.0, 10.0, &["0"][..], Some(7))]
        )
        .await
        .status(),
        200,
        "an `id` naming nothing creates an item"
    );
    // The same id, a different view: accepted. Before the flush and after it.
    let resp = ingest(
        &served,
        "join",
        "quarter:2026-Q5",
        &[(id, 800.0, 300.0, &["0"][..], Some(7))],
    )
    .await;
    assert_eq!(
        resp.status(),
        200,
        "a known `id` naming a view the item is not in is a join: {:?}",
        resp.text().await
    );
    drain(&served.server).await;

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

    // A row moving the item in a view it is in edits it, and it keeps its identity.
    let moved = accepted(
        ingest(
            &served,
            "join-again",
            "quarter:2026-Q5",
            &[(id, 810.0, 310.0, &["0"][..], Some(7))]
        )
        .await,
    )
    .await;
    assert_eq!(moved["edited"], 1, "{moved}");
    assert_eq!(ingested_ids(&moved), vec![joined]);
    drain(&served.server).await;
    assert!(points(&served, "world").await.iter().any(|p| p.0 == joined));
    let moved_code = points(&served, "quarter:2026-Q5")
        .await
        .into_iter()
        .find(|p| p.0 == joined)
        .expect("the edited item is placed in the view again")
        .1;
    assert_ne!(moved_code, q5_code, "at its new position");
}

/// **A row naming an item and changing it edits it**, however it changes it: a second position in
/// a view it is in, a new label, a different value, a null clearing one. Each keeps the item's
/// `tessera_id`, and a row leaving every column out changes nothing.
#[tokio::test]
async fn a_second_row_a_relabel_and_a_changed_attribute_edit_the_item() {
    let served = Served::build(build_fixture_bundle).await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5", 1))
            .await
            .status(),
        201
    );
    let id = Some(NEW_ID);
    let first = accepted(
        ingest(
            &served,
            "first",
            "world",
            &[(id, 10.0, 10.0, &["0"][..], Some(7))],
        )
        .await,
    )
    .await;
    let tid = ingested_ids(&first)[0];

    for (batch_id, view, x, label, score) in [
        ("same-view", "world", 11.0, "0", Some(7)),
        ("relabel", "quarter:2026-Q5", 800.0, "1", Some(7)),
        ("reattribute", "quarter:2026-Q5", 800.0, "1", Some(9)),
        ("attribute-null", "quarter:2026-Q5", 800.0, "1", None),
    ] {
        let body = accepted(
            ingest(&served, batch_id, view, &[(id, x, 300.0, &[label][..], score)]).await,
        )
        .await;
        assert_eq!(body["edited"], 1, "{batch_id}: {body}");
        assert_eq!(ingested_ids(&body), vec![tid], "{batch_id} keeps the item's identity");
    }
    let unchanged = accepted(
        ingest_json(&served, "left-out", "quarter:2026-Q5", json!([located(NEW_ID, 800.0, 300.0)]))
            .await,
    )
    .await;
    assert_eq!(unchanged["unchanged"], 1, "{unchanged}");
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
    let mut served = Served::build(build_fixture_bundle).await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5", 1))
            .await
            .status(),
        201
    );
    // The visible-view set is fixed per session (`views.md` §6), so a reader of the
    // newly created view takes a new session, exactly as a client would.
    served.reauthorise().await;
    let id = Some(NEW_ID);
    let resp = ingest(
        &served,
        "first",
        "world",
        &[(id, 10.0, 10.0, &["0"][..], Some(7))],
    )
    .await;
    assert_eq!(resp.status(), 200);
    let tessera_id = ingested_ids(&resp.json::<Value>().await.unwrap())[0];
    drain(&served.server).await;
    assert!(
        points(&served, "world")
            .await
            .iter()
            .any(|p| p.0 == tessera_id),
        "visible before the suppression"
    );

    let change = |op: &str| {
        let body =
            json!([{ "tessera_id": tessera_id.to_string(), "op": op }]);
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
            &[(id, 800.0, 300.0, &["0"][..], Some(7))]
        )
        .await
        .status(),
        200,
        "a suppressed holder takes the same arms as a live one"
    );
    drain(&served.server).await;
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

/// **A drop deletes the items it leaves in no view**, flushed and buffered alike, as ordinary
/// deletions: they enter the overlay and retire at the fold like any deletion, and a deleted
/// item's `id` names nothing afterwards. An item that also has a row in another view, even
/// one still buffered, stays.
#[tokio::test]
async fn a_drop_deletes_only_the_items_it_leaves_in_no_view() {
    let served = Served::build(build_fixture_bundle).await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5", 1))
            .await
            .status(),
        201
    );

    let only_here = Some(NEW_ID);
    let also_elsewhere = Some(NEW_ID + 1);
    assert_eq!(
        ingest(
            &served,
            "elsewhere",
            "world",
            &[(also_elsewhere, 10.0, 10.0, &["0"][..], Some(1))]
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
                (only_here, 800.0, 300.0, &["0"][..], Some(2)),
                (also_elsewhere, 810.0, 310.0, &["0"][..], Some(1)),
            ]
        )
        .await
        .status(),
        200,
        "one new entity and one join"
    );
    drain(&served.server).await;

    // A third item, still in the buffer when the drop runs: its only row is the one the drop
    // discards, so it is deleted with the flushed one.
    let buffered = Some(NEW_ID + 2);
    assert_eq!(
        ingest(
            &served,
            "buffered",
            "quarter:2026-Q5",
            &[(buffered, 820.0, 320.0, &["0"][..], Some(3))]
        )
        .await
        .status(),
        200
    );

    let body = drop_view(&served, "quarter", "2026-Q5").await;
    assert_eq!(
        body["deleted"], 2,
        "the two items this view alone held, the flushed one and the buffered one, and not the \
         one that also sits in `world`: {body}"
    );

    // The ordinary deletion observables. A deleted item's `id` names nothing, so a row carrying it
    // creates a new item; a live item's names it.
    assert_eq!(
        ingest(
            &served,
            "reingest-deleted",
            "world",
            &[(only_here, 20.0, 20.0, &["0"][..], Some(2))]
        )
        .await
        .status(),
        200,
        "a deleted item's `id` names nothing"
    );
    let live = accepted(
        ingest(
            &served,
            "reingest-live",
            "world",
            &[(also_elsewhere, 30.0, 30.0, &["0"][..], Some(1))]
        )
        .await,
    )
    .await;
    assert_eq!(
        live["edited"], 1,
        "the entity that was also in `world` was not deleted, and the row moves it there: {live}"
    );
    assert_eq!(
        ingest(
            &served,
            "reingest-buffered",
            "world",
            &[(buffered, 40.0, 40.0, &["0"][..], Some(3))]
        )
        .await
        .status(),
        200,
        "the buffered row's entity was deleted with the rest"
    );

    // **A buffered row goes with the view it named, and its item does not.** The row is
    // geometry for a coordinate system that no longer exists, so nothing would ever give it a
    // place; left in the buffer it would pin the WAL's reclaim bound for the life of the process.
    assert_eq!(
        create(&served, "quarter", "2026-Q7", q_record("Q7", 7))
            .await
            .status(),
        201
    );
    let both = Some(NEW_ID + 3);
    let buffered_before = served.server.state.engine.buffered_items();
    assert_eq!(
        ingest(
            &served,
            "both-world",
            "world",
            &[(both, 60.0, 60.0, &["0"][..], Some(6))]
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
            &[(both, 800.0, 300.0, &["0"][..], Some(6))]
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
    let body = drop_view(&served, "quarter", "2026-Q7").await;
    assert_eq!(
        body["deleted"], 0,
        "the item holds a row in `world`, so the drop leaves it: {body}"
    );
    assert_eq!(
        served.server.state.engine.buffered_items(),
        buffered_before + 1,
        "the dropped view's buffered row went with it, and the other stayed"
    );
    drain(&served.server).await;
    let again = accepted(
        ingest(
            &served,
            "both-again",
            "world",
            &[(both, 70.0, 70.0, &["0"][..], Some(6))]
        )
        .await,
    )
    .await;
    assert_eq!(again["edited"], 1, "and the entity is alive, in `world`: {again}");

    // An item flushed into one view alone, dropped with it: after a restart, which replays the
    // deletion from the drop's own record, a row carrying its `id` creates a new item.
    assert_eq!(
        create(&served, "quarter", "2026-Q6", q_record("Q6", 2))
            .await
            .status(),
        201
    );
    let solitary = NEW_ID + 4;
    let resp = ingest(
        &served,
        "q6",
        "quarter:2026-Q6",
        &[(Some(solitary), 800.0, 300.0, &["0"][..], Some(4))],
    )
    .await;
    assert_eq!(resp.status(), 200);
    let before = ingested_ids(&resp.json::<Value>().await.unwrap())[0];
    drain(&served.server).await;
    let body = drop_view(&served, "quarter", "2026-Q6").await;
    assert_eq!(body["deleted"], 1, "{body}");
    let served = served.restart().await;
    let again = accepted(
        ingest_json(
            &served,
            "reingest-solitary",
            "world",
            json!([located(solitary, 50.0, 50.0)]),
        )
        .await,
    )
    .await;
    assert_eq!(again["created"], 1, "a deleted item names nothing: {again}");
    assert_ne!(
        ingested_ids(&again)[0],
        before,
        "the new item has a new tessera_id"
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
    write_view_points(&dir.join("world.parquet"), "world", WORLD, None);
    write_view_points(
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
"#
        .to_string()
            + ID_ATTRIBUTE
            + "source = \"world\"\n",
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
        anchor,
        groups,
        attribute_sources: tessera_build::config::AttributeSource::over(
            dir.join("world.parquet"),
            &config.schema,
        ),
        schema: config.schema,
        ..build_args(&out, views)
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
    let mut served = Served::open(tmp).await;

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
            &[(None, 10.0, 10.0, &["0"][..], Some(1))]
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
    served.reauthorise().await;
    assert!(view_ids(&meta(&served).await).contains(&"quarter:2026-Q2".to_string()));
    assert_eq!(viewport(&served, "quarter:2026-Q2").await.status(), 200);

    // **Join.** An entity the build already knows joins the new view by its `id`, and is
    // placed there with its own geometry — the same identity, a second row space.
    //
    // The batch carries the entity's **own** label set. Source 3 is a multiple of three, so the
    // fixture gave it `{0, 1}`; naming only `0` is now the 409 `views.md` §4 always specified,
    // the entity→term transpose having made the arm exact past its own flush.
    let known = Some(3);
    let resp = ingest(
        &served,
        "join-minted",
        "quarter:2026-Q2",
        &[(known, 400.0, 400.0, &["0", "1"][..], Some(3))],
    )
    .await;
    assert_eq!(resp.status(), 200, "a join into a minted group's view");
    let joined = ingested_ids(&resp.json::<Value>().await.unwrap())[0];
    drain(&served.server).await;
    served.reauthorise().await;
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
    let moved = accepted(
        ingest(
            &served,
            "join-minted-world",
            "world",
            &[(known, 60.0, 60.0, &["0"][..], Some(3))],
        )
        .await,
    )
    .await;
    assert_eq!(moved["edited"], 1, "the entity is in `world` already, so the row edits it");

    // **Drop**, and the key is freed — on a minted view exactly as on a declared one.
    let body = drop_view(&served, "quarter", "2026-Q1").await;
    assert_eq!(body["deleted"], 0);
    served.reauthorise().await;
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
/// - **And the two removal rules compose.** A second drop leaves items in no view and deletes them;
///   the fold that omits the view's segments is also the fold that executes the deletions, and
///   their overlay entries retire there (Rule F) rather than at the drop.
#[tokio::test]
async fn a_fold_after_a_drop_reclaims_the_dropped_view_and_a_recreate_adopts_nothing() {
    let mut served = Served::build(build_fixture_bundle).await;

    // A second key, so the group still has a view after the drop and the survivors are a set
    // rather than one plain view. Its rows arrive through ingest, so the dropped view is not the
    // only one whose files the fold has to carry.
    assert_eq!(
        create(&served, "quarter", "2026-Q2", q_record("Q2 2026", 2))
            .await
            .status(),
        201
    );
    served.reauthorise().await;
    let rows: Vec<Row<'_>> = (0..4)
        .map(|i| {
            (
                None,
                100.0 + i as f32,
                200.0,
                &["0"][..],
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
    drain(&served.server).await;

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

    let body = drop_view(&served, "quarter", "2026-Q1").await;
    assert_eq!(body["deleted"], 0, "every item of the view is in `world` too");

    fold(&served.server).await;
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
    let served = served.restart().await;
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
    served.reauthorise().await;
    assert_eq!(
        points(&served, "quarter:2026-Q1").await.len(),
        0,
        "the recreated key is an empty view, not the build's own rows under a new record"
    );
    drop_view(&served, "quarter", "2026-Q1").await;
    served.reauthorise().await;

    // (e) **A drop's deletions retire at the fold that omits their view's segments** (Rule F).
    // The *view* goes by omission at the publication, and the *items* the drop deleted go the
    // ordinary way, through the overlay, at the fold that executes them.
    //
    // The four items ingested into `2026-Q2` hold a row in that view and nowhere else: they were
    // created by that batch, and `quarter_map:2026-Q2` was created empty beside it. So the drop
    // deletes all four.
    let before = served.server.state.engine.retirable_deletions();
    let body = drop_view(&served, "quarter", "2026-Q2").await;
    assert_eq!(
        body["deleted"], 4,
        "every entity of the view was in no other: {body}"
    );
    assert_eq!(
        served.server.state.engine.retirable_deletions(),
        before + 4,
        "the drop's deletions are ordinary deletions and enter the overlay"
    );

    fold(&served.server).await;
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
    let ids: Vec<u64> = ids.collect();
    let ints = || arrow::array::Int32Array::from_iter(ids.iter().map(|&e| Some(e as i32)));
    let text = |prefix: &str| {
        arrow::array::StringArray::from_iter_values(ids.iter().map(|e| format!("{prefix}{e}")))
    };
    let archive = arrow::array::StringArray::from_iter_values(
        ids.iter()
            .map(|e| ["astro", "cond", "hep"][(e % 3) as usize]),
    );
    let extra = vec![
        column("score", true, ints()),
        column("depth", true, ints()),
        column("tag", true, text("t")),
        column("note", true, text("n")),
        column("archive", false, archive),
    ];
    write_points(path, &ids, |e| position(view, e), extra);
}

/// One plain view and one group, as the fixture above, over [`FAMILIES_SCHEMA`]'s five columns and
/// the unique `id`.
fn build_families(dir: &Path) {
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, ENTITIES);
    let world_points = dir.join("world.parquet");
    write_families_points(&world_points, "world", WORLD);
    let q1_points = dir.join("quarter-q1.parquet");
    write_families_points(&q1_points, "quarter:2026-Q1", Q1);
    let schema_path = dir.join("schema.toml");
    std::fs::write(&schema_path, format!("{FAMILIES_SCHEMA}{ID_ATTRIBUTE}")).unwrap();
    let config = tessera_build::config::Config::parse(&schema_path, &Default::default())
        .expect("the families declaration parses");
    let out = dir.join("bundle");
    build(&BuildArgs {
        groups: vec![GroupDescriptor {
            title: None,
            point_default: Some("public".to_string()),
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
        attribute_sources: tessera_build::config::AttributeSource::over(
            world_points.clone(),
            &config.schema,
        ),
        schema: config.schema,
        ..build_args(
            &out,
            vec![
                view_args("world", &world_points, AccessInput::relation(&pairs)),
                view_args("quarter:2026-Q1", &q1_points, AccessInput::relation(&pairs)),
            ],
        )
    })
    .expect("the families build succeeds");
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

fn families_batch(id: u64, x: f32, y: f32, a: Attrs<'_>) -> Vec<u8> {
    let access = access_column(["0"]);
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt64, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access),
        Field::new("score", DataType::Int32, true),
        Field::new("depth", DataType::Int32, true),
        Field::new("tag", DataType::Utf8, true),
        Field::new("note", DataType::Utf8, true),
        Field::new("archive", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(arrow::array::UInt64Array::from_iter_values([id])),
            Arc::new(arrow::array::Float32Array::from_iter_values([x])),
            Arc::new(arrow::array::Float32Array::from_iter_values([y])),
            Arc::new(access),
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
    id: u64,
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
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(families_batch(id, x, y, a))
        .send()
        .await
        .unwrap()
}

/// **An edit after the flush carries what the item holds in every home and changes what the row
/// names**, over all three homes and all three families: the same values add the item to a view,
/// each differing value edits it, keeping every other value, and nulls clear what they name.
#[tokio::test]
async fn an_edit_after_the_flush_carries_every_home_and_changes_what_it_names() {
    let mut served = Served::build(build_families).await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", json!({ "metadata": {} }))
            .await
            .status(),
        201
    );
    served.reauthorise().await;

    let id = NEW_ID;
    let first = accepted(families_ingest(&served, "first", "world", id, 10.0, 10.0, HELD).await).await;
    let tid = ingested_ids(&first)[0];
    // Past the flush every value is read back from the bundle's homes.
    drain(&served.server).await;
    assert_eq!(served.server.state.engine.buffered_items(), 0);

    let added = accepted(
        families_ingest(&served, "same", "quarter:2026-Q5", id, 800.0, 300.0, HELD).await,
    )
    .await;
    assert_eq!(added["added"], 1, "the same values add the item to the view: {added}");

    let mut held = HELD;
    for (column, differing) in [
        ("score", Attrs { score: Some(9), ..HELD }),
        ("depth", Attrs { depth: Some(8), ..HELD }),
        ("tag", Attrs { tag: Some("beta"), ..HELD }),
        ("note", Attrs { note: Some("other"), ..HELD }),
        ("archive", Attrs { archive: Some("hep"), ..HELD }),
    ] {
        // Only the column under test is sent; every other one is left to the stored value.
        let mut row = json!({
            "id": id,
        });
        row[column] = match column {
            "score" => json!(differing.score),
            "depth" => json!(differing.depth),
            "tag" => json!(differing.tag),
            "note" => json!(differing.note),
            _ => json!(differing.archive),
        };
        let body = accepted(ingest_json(&served, &format!("differ-{column}"), "world", json!([row])).await).await;
        assert_eq!(body["edited"], 1, "{column}: {body}");
        assert_eq!(ingested_ids(&body), vec![tid]);
        match column {
            "score" => held.score = differing.score,
            "depth" => held.depth = differing.depth,
            "tag" => held.tag = differing.tag,
            "note" => held.note = differing.note,
            _ => held.archive = differing.archive,
        }
        drain(&served.server).await;
        let card = item_card(&served, tid).await;
        let field = |name: &str| card["fields"].get(name).cloned().unwrap_or(Value::Null);
        assert_eq!(field("score"), json!(held.score), "after '{column}': {card}");
        assert_eq!(field("depth"), json!(held.depth), "after '{column}': {card}");
        assert_eq!(field("tag"), json!(held.tag), "after '{column}': {card}");
        assert_eq!(field("note"), json!(held.note), "after '{column}': {card}");
        assert_eq!(field("archive"), json!(held.archive), "after '{column}': {card}");
        let views: Vec<&str> = card["views"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect();
        assert_eq!(views, ["quarter:2026-Q5", "world"], "after '{column}': {card}");
    }

    let cleared = accepted(
        families_ingest(
            &served,
            "nulls",
            "quarter:2026-Q5",
            id,
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
        .await,
    )
    .await;
    assert_eq!(cleared["edited"], 1, "nulls clear held values: {cleared}");
}

/// The card `/v1/items/{tessera_id}` serves this server's principal.
async fn item_card(served: &Served, tessera_id: u64) -> Value {
    let resp = served
        .server
        .client
        .post(served.server.viewer_url(&format!("/v1/items/{tessera_id}")))
        .bearer_auth(&served.token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    resp.json().await.unwrap()
}

/// **A value for a column the item holds none of changes the item**: a row adding the item to a
/// view and giving it that value edits it. A row carrying its position alone changes nothing.
#[tokio::test]
async fn a_row_giving_an_item_a_value_it_never_held_changes_it() {
    let mut served = Served::build(build_families).await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", json!({ "metadata": {} }))
            .await
            .status(),
        201
    );
    served.reauthorise().await;

    let id = NEW_ID;
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
        families_ingest(&served, "sparse", "world", id, 20.0, 20.0, sparse)
            .await
            .status(),
        200
    );
    drain(&served.server).await;

    assert_eq!(
        families_ingest(
            &served,
            "supply",
            "quarter:2026-Q5",
            id,
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
        "a value the item never held is a change to it"
    );
    let left_out = accepted(
        ingest_json(&served, "added", "quarter:2026-Q5", json!([located(id, 800.0, 300.0)]))
            .await,
    )
    .await;
    assert_eq!(
        left_out["unchanged"], 1,
        "a row carrying the item's position and nothing else changes nothing: {left_out}"
    );
}

/// One view's points under a filter, so a test can ask what a **view's own tail** carries rather
/// than what the drill-down reports. The drill-down answers from the first view holding a row and
/// is therefore blind to a disagreement between two of them, which is exactly the property under
/// test here; a `render` column is filterable through the row route (decision 0068), and a filter
/// evaluated against one view's rows is that view's tail and no other's.
async fn filtered_points(served: &Served, view: &str, filter: Value) -> Vec<PointRow> {
    let resp = settled(async || {
        served
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
            .unwrap()
    })
    .await;
    decode_viewport(&resp.bytes().await.unwrap()).1
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
    let mut served = Served::build(build_families).await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", json!({ "metadata": {} }))
            .await
            .status(),
        201
    );
    served.reauthorise().await;

    let id = NEW_ID;
    let resp = families_ingest(&served, "first", "world", id, 10.0, 10.0, HELD).await;
    assert_eq!(resp.status(), 200);
    let tessera_id = ingested_ids(&resp.json::<Value>().await.unwrap())[0];
    drain(&served.server).await;

    // The row adding the item leaves every value out, which is what would otherwise put an
    // absence in this view's tail.
    assert_eq!(
        ingest_json(&served, "omit", "quarter:2026-Q5", json!([located(id, 800.0, 300.0)]))
            .await
            .status(),
        200
    );
    drain(&served.server).await;

    let score_is_one = json!({ "score": { "eq": 1 } });
    for view in ["world", "quarter:2026-Q5"] {
        let matched = filtered_points(&served, view, score_is_one.clone()).await;
        assert!(
            matched.iter().any(|(id, _)| *id == tessera_id),
            "the entity renders its one stored score under '{view}': {matched:?}"
        );
    }
}



/// **A row whose item is deleted under it creates a fresh item that keeps its label.**
///
/// The handler resolves an `id` to a live item and plans an edit. If the item is deleted
/// between that plan and the apply, the executor answers stale, the handler plans again against a
/// generation without the item, and the row creates a fresh item. It must still carry its
/// descriptors, or the fresh item is written with no label at all: visible to no principal, and
/// reachable by no deny either.
///
/// The first half runs the two in sequence, so the handler sees the deleted item itself; it
/// asserts the property, not the ordering. The second half sends the delete and the re-ingest
/// together, which is the only way to reach the ordering over HTTP, and the executor decides which
/// lands first. A row that created an item is served under its label. A row that edited the live
/// item was applied before the delete, which then removed the item, so it is not served.
#[tokio::test]
async fn a_row_whose_item_is_deleted_under_it_creates_a_fresh_item_that_keeps_its_label() {
    let mut served = Served::build(build_fixture_bundle).await;
    let id = Some(NEW_ID);
    let resp = ingest(
        &served,
        "demote-first",
        "world",
        &[(id, 10.0, 10.0, &["0"][..], Some(7))],
    )
    .await;
    assert_eq!(resp.status(), 200);
    let first = ingested_ids(&resp.json::<Value>().await.unwrap())[0];
    drain(&served.server).await;

    // The item is deleted, so the re-ingest below names no live item and creates one, where a
    // live item would have been edited.
    let body = json!([{ "tessera_id": first.to_string(), "op": "delete" }]);
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the item is deleted");

    // The same `id` again, under a label this principal holds.
    let resp = ingest(
        &served,
        "demote-second",
        "quarter:2026-Q1",
        &[(id, 800.0, 300.0, &["1"][..], Some(7))],
    )
    .await;
    assert_eq!(resp.status(), 200, "a deleted item does not collide");
    let second = ingested_ids(&resp.json::<Value>().await.unwrap())[0];
    assert_ne!(second, first, "a fresh item, not the deleted one");
    drain(&served.server).await;
    served.reauthorise().await;

    // The fresh item carries the label the batch named, so a principal that satisfies it is
    // served the row. A row that had lost its descriptors would be stored with no label and be
    // visible to nobody.
    assert!(
        points(&served, "quarter:2026-Q1")
            .await
            .iter()
            .any(|p| p.0 == second),
        "the re-ingested row is served under the label it carried"
    );

    // The concurrent half. The deny lane has priority over the work queue, so a delete sent beside
    // an ingest can apply between that ingest's plan and its apply.
    let mut created = Vec::new();
    let mut deleted_after_edit = Vec::new();
    for round in 0..8u32 {
        let id = Some(NEW_ID + 1 + u64::from(round));
        let seed = ingest(
            &served,
            &format!("demote-race-seed-{round}"),
            "world",
            &[(id, 10.0, 10.0, &["0"][..], Some(7))],
        )
        .await;
        assert_eq!(seed.status(), 200);
        let holder = ingested_ids(&seed.json::<Value>().await.unwrap())[0];

        let body = json!([{ "tessera_id": holder.to_string(), "op": "delete" }]);
        let batch_id = format!("demote-race-again-{round}");
        let rows = [(id, 800.0, 300.0, &["1"][..], Some(7))];
        let (deleted, again) = tokio::join!(
            served
                .server
                .client
                .post(served.server.control_url("/control/changes"))
                .bearer_auth(OPERATOR_CREDENTIAL)
                .json(&body)
                .send(),
            ingest(&served, &batch_id, "quarter:2026-Q1", &rows),
        );
        assert_eq!(
            deleted.unwrap().status(),
            200,
            "round {round}: the delete lands"
        );
        let status = again.status().as_u16();
        let text = again.text().await.unwrap();
        match status {
            200 => {
                let resp: Value = serde_json::from_str(&text).unwrap();
                let taken = ingested_ids(&resp)[0];
                if resp["created"] == 1 {
                    assert_ne!(taken, holder, "round {round}: a fresh item: {text}");
                    created.push(taken);
                } else {
                    assert_eq!(resp["edited"], 1, "round {round}: {text}");
                    assert_eq!(taken, holder, "round {round}: the edit names the item: {text}");
                    deleted_after_edit.push(taken);
                }
            }
            // Planned twice against an item that moved each time.
            409 => assert!(
                text.contains("\"conflict\""),
                "round {round}: the only lawful refusal here is a conflict: {text}"
            ),
            other => panic!("round {round}: unexpected {other}: {text}"),
        }
    }
    drain(&served.server).await;
    // A session authorised before the flush can be answered from its projection at the previous
    // generation until the background refresh replaces it; a fresh session is built at the live one.
    served.reauthorise().await;
    let served_ids: Vec<u64> = points(&served, "quarter:2026-Q1")
        .await
        .iter()
        .map(|p| p.0)
        .collect();
    for id in created {
        assert!(
            served_ids.contains(&id),
            "every item a row created is served under the label it carried; {id} is not in \
             {served_ids:?}"
        );
    }
    for id in deleted_after_edit {
        assert!(
            !served_ids.contains(&id),
            "an item deleted after its edit is not served; {id} is in {served_ids:?}"
        );
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
    let mut served = Served::build(build_fixture_bundle).await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5 first", 1))
            .await
            .status(),
        201
    );
    served.reauthorise().await;

    // The first incarnation's rows, flushed, so the view owns a segment and a row space.
    let first: Vec<Row<'_>> = (0..6)
        .map(|i| {
            (
                None,
                300.0 + i as f32,
                300.0,
                &["0"][..],
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
    drain(&served.server).await;
    assert_eq!(points(&served, "quarter:2026-Q5").await.len(), 6);

    // **And rows that never flushed**, so the drop below meets them in the buffer and the restart
    // meets them in the log. Without this arm the buffered half of the hazard is invisible: the
    // flushed rows above are dropped from the replayed buffer by the ordinary "this row already
    // has geometry" filter, whatever the drop does.
    let stale: Vec<Row<'_>> = (0..3)
        .map(|i| {
            (
                None,
                320.0 + i as f32,
                320.0,
                &["0"][..],
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

    // **Drop and recreate in one window** — no flush, no fold, no publication between them. The
    // drop deletes the nine items it leaves in no view, flushed and buffered alike.
    assert_eq!(
        drop_view(&served, "quarter", "2026-Q5").await["deleted"],
        9
    );
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5 second", 2))
            .await
            .status(),
        201,
        "a dropped key is reusable, and in the same window"
    );
    served.reauthorise().await;
    assert_eq!(
        points(&served, "quarter:2026-Q5").await.len(),
        0,
        "the recreated key is an empty view: the predecessor's segment is on disc and unreachable"
    );

    // The second incarnation's rows, left **unflushed**, so the restart below has to replay them.
    let second: Vec<Row<'_>> = (0..4)
        .map(|i| {
            (
                None,
                500.0 + i as f32,
                500.0,
                &["0"][..],
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
    let mut served = served.restart().await;
    served.reauthorise().await;
    let document = meta(&served).await;
    assert_eq!(
        roster_of(&document, "quarter:2026-Q5").unwrap()["metadata"]["label"]["value"],
        "Q5 second",
        "the surviving record is the recreate's, so the deaths were applied before the creations"
    );
    // The rows are in the buffer and nowhere else, so the flush is what gives them geometry —
    // and what makes the count below a statement about *which* rows replay kept.
    drain(&served.server).await;
    let after_replay = points(&served, "quarter:2026-Q5").await;
    assert_eq!(
        after_replay.len(),
        4,
        "the replayed view holds the second batch and none of the first: {}",
        after_replay.len()
    );

    // ---- the fold --------------------------------------------------------------------------
    fold(&served.server).await;
    let (_, folded_dir) = live_prefix(&served);
    assert!(
        segment_views(&folded_dir).contains(&"quarter:2026-Q5".to_string()),
        "the recreated view is folded like any other"
    );
    let served = served.restart().await;
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
    let mut served = Served::build(build_fixture_bundle).await;

    // ---- (A) the live path: drop on the owner, rows buffered under the sharing group ---------
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5 first", 1))
            .await
            .status(),
        201
    );
    served.reauthorise().await;
    let shared: Vec<Row<'_>> = (0..5)
        .map(|i| {
            (
                None,
                600.0 + i as f32,
                600.0,
                &["0"][..],
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
    // The five items buffered under the other spelling are in no other view, so the drop deletes
    // them.
    assert_eq!(
        drop_view(&served, "quarter", "2026-Q5").await["deleted"],
        5
    );
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5 second", 2))
            .await
            .status(),
        201
    );
    served.reauthorise().await;
    // **One row of the new incarnation's own**, so the flush below has work and the count is a
    // statement rather than an empty buffer's silence: five stale rows would make it six.
    assert_eq!(
        ingest(
            &served,
            "fresh-q5",
            "quarter:2026-Q5",
            &[(None, 610.0, 610.0, &["0"][..], Some(0))],
        )
        .await
        .status(),
        200
    );
    drain(&served.server).await;
    for id in ["quarter:2026-Q5", "quarter_map:2026-Q5"] {
        assert_eq!(
            points(&served, id).await.len(),
            if id == "quarter:2026-Q5" { 1 } else { 0 },
            "{id}: the recreated key adopted rows buffered under the other spelling"
        );
    }

    // ---- (B) the replay arm: rows buffered under the sharing group, and no flush ------------
    drop_view(&served, "quarter", "2026-Q5").await;
    assert_eq!(
        create(&served, "quarter", "2026-Q6", q_record("Q6 first", 3))
            .await
            .status(),
        201
    );
    served.reauthorise().await;
    let owned: Vec<Row<'_>> = (0..5)
        .map(|i| {
            (
                None,
                700.0 + i as f32,
                700.0,
                &["0"][..],
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
        drop_view(&served, "quarter_map", "2026-Q6").await["deleted"],
        5
    );
    assert_eq!(
        create(&served, "quarter", "2026-Q6", q_record("Q6 second", 4))
            .await
            .status(),
        201
    );
    // **No flush before the restart**: the rows are in the log and nowhere else, so what keeps
    // them out of the recreated view is replay's own `ViewDrop` arm.
    let mut served = served.restart().await;
    served.reauthorise().await;
    assert_eq!(
        ingest(
            &served,
            "fresh-q6",
            "quarter:2026-Q6",
            &[(None, 710.0, 710.0, &["0"][..], Some(0))],
        )
        .await
        .status(),
        200
    );
    drain(&served.server).await;
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

/// `GET /control/status`'s publication counter.
async fn publication(served: &Served) -> u64 {
    let resp = served
        .server
        .client
        .get(served.server.control_url("/control/status"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    body["publication"].as_u64().expect("status carries it")
}

/// One `POST /control/flush?wait=visible`, answering when its cycle has completed.
async fn flush_waiting(served: &Served) -> Value {
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/flush?wait=visible"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    resp.json().await.unwrap()
}

/// **A view created and fed in one commit publishes in one cycle** (issue #153; decision 0144).
/// The ingest acknowledgement names the cycle its rows become visible in, so a client that waits
/// for that number and reads the views must find every row the commit sent — including the rows
/// of a view this same commit created. `held` is a view the bundle already carried, fed in the
/// same commit: a flush unit is one view, so the two together are what makes the cycle span more
/// than one publication.
async fn a_view_created_and_fed_publishes_in_one_cycle(
    served: &mut Served,
    created: &str,
    held: &str,
) {
    // The held view already carries rows; what this asserts is the four this commit adds.
    let held_before = points(served, held).await.len();
    let mut promised = 0;
    for view in [created, held] {
        let rows: Vec<Row<'_>> = (0..4)
            .map(|i| {
                (
                    None,
                    100.0 + i as f32,
                    200.0,
                    &["0"][..],
                    Some(i),
                )
            })
            .collect();
        let resp = ingest(served, &format!("{view}-batch"), view, &rows).await;
        assert_eq!(resp.status(), 200, "{view} accepts rows");
        let body: Value = resp.json().await.unwrap();
        promised = body["publication"]
            .as_u64()
            .unwrap_or_else(|| panic!("the ingest acknowledgement carries the number: {body}"));
    }

    let answer = flush_waiting(served).await;
    assert_eq!(answer["visible"], json!(true), "the wait completed: {answer}");
    assert_eq!(
        publication(served).await,
        promised,
        "{created}: the counter names the cycle the rows are visible in and passes it no earlier"
    );
    served.reauthorise().await;
    for (view, wanted) in [(created, 4), (held, held_before + 4)] {
        assert_eq!(
            points(served, view).await.len(),
            wanted,
            "{view}: the rows the acknowledgement promised at {promised} are served there"
        );
    }
    assert_eq!(
        served.server.state.engine.buffered_items(),
        0,
        "the number was not reached with rows still buffered"
    );
}

/// **The group case issue #153 reported**: the view is created on a declared group, and the
/// commit that creates it feeds it and a view the bundle already carried.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_view_created_in_a_commit_serves_its_rows_at_the_number_it_was_promised() {
    let mut served = Served::build(build_fixture_bundle).await;
    let resp = create(
        &served,
        "quarter",
        "2026-Q7",
        q_record("Q7 2026", 1_800_000_000_000_000),
    )
    .await;
    assert_eq!(resp.status(), 201, "a free key on a declared group creates");
    a_view_created_and_fed_publishes_in_one_cycle(&mut served, "quarter:2026-Q7", "quarter:2026-Q1")
        .await;
}

/// The plain case beside it, in the same shape. The kind of view is not what decides this — how
/// many views one commit feeds is — and these two are here to hold that true for both kinds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plain_view_created_in_a_commit_serves_its_rows_at_the_number_it_was_promised() {
    let mut served = Served::build(build_fixture_bundle).await;
    let resp = served
        .server
        .client
        .put(served.server.control_url("/control/views/extra"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "extent": { "x": [0.0, 1000.0], "y": [0.0, 1000.0] },
            "point_visibility": { "default": "public" }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "a plain view is created");
    a_view_created_and_fed_publishes_in_one_cycle(&mut served, "extra", "world").await;
}


/// **An edit whose own row is in a view dropped before the tick loses nothing**: the item's row in
/// the view it keeps takes its label and values, so the flush writes what the edit gave it.
#[tokio::test]
async fn an_edit_in_a_view_dropped_before_the_tick_is_still_written() {
    const DEPTH: i32 = 4242;
    let mut served = Served::build(build_families).await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", json!({ "metadata": {} }))
            .await
            .status(),
        201
    );
    served.reauthorise().await;

    let id = NEW_ID;
    let sparse = Attrs {
        score: None,
        depth: None,
        tag: None,
        note: None,
        archive: None,
    };
    let first = accepted(families_ingest(&served, "sparse", "world", id, 20.0, 20.0, sparse).await).await;
    let tid = ingested_ids(&first)[0];
    drain(&served.server).await;

    // The edit places the item in Q5, so its label and values travel in that view's row.
    let edited = accepted(
        families_ingest(
            &served,
            "edit",
            "quarter:2026-Q5",
            id,
            800.0,
            300.0,
            Attrs {
                depth: Some(DEPTH),
                ..sparse
            },
        )
        .await,
    )
    .await;
    assert_eq!(edited["edited"], 1, "{edited}");
    drop_view(&served, "quarter", "2026-Q5").await;
    served.reauthorise().await;
    drain(&served.server).await;

    let holds_depth = json!({ "depth": { "eq": DEPTH } });
    let holding: Vec<u64> = filtered_points(&served, "world", holds_depth.clone())
        .await
        .iter()
        .map(|p| p.0)
        .collect();
    assert_eq!(
        holding,
        vec![tid],
        "the item is served in the view it kept, holding the edit's value"
    );
    assert_eq!(
        served.server.state.engine.generation().buffer.oldest_wal_pos(),
        None,
        "nothing buffered holds the log"
    );

    let served = served.restart().await;
    assert_eq!(
        filtered_points(&served, "world", holds_depth).await.len(),
        1,
        "and after a restart"
    );
}

/// What a viewer is served of `world` after the buffer drains: every point, the points whose
/// `score` is 7, and the points a principal holding only `1` sees.
async fn world_as_served(served: &mut Served) -> (Vec<u64>, Vec<u64>, Vec<u64>) {
    drain(&served.server).await;
    served.reauthorise().await;
    let ids = |rows: Vec<PointRow>| {
        let mut ids: Vec<u64> = rows.into_iter().map(|(id, _)| id).collect();
        ids.sort_unstable();
        ids
    };
    let all = ids(points(served, "world").await);
    let scored = ids(filtered_points(served, "world", json!({ "score": { "eq": 7 } })).await);
    let token = std::mem::replace(&mut served.token, token_for(&served.server, &["1"]).await);
    let restricted = ids(points(served, "world").await);
    served.token = token;
    (all, scored, restricted)
}

/// **A restart serves a dropped view's joined item as the live service does.** An item's own row
/// is flushed into a created view, a row joining it to `world` waits in the buffer, and the view
/// is dropped. The live service has only the join buffered when the drop arrives; a restart
/// replays both rows and meets the drop between them. Either way `world` serves the item once,
/// with its score, to the principals its label admits.
#[tokio::test]
async fn a_dropped_views_joined_item_is_served_alike_live_and_after_a_restart() {
    let mut observed = Vec::new();
    for restart in [false, true] {
        let mut served = Served::build(build_fixture_bundle).await;
        assert_eq!(
            create(&served, "quarter", "2026-Q5", q_record("Q5", 1))
                .await
                .status(),
            201
        );
        served.reauthorise().await;
        let item = Some(NEW_ID + 50);
        let own = accepted(
            ingest(
                &served,
                "own",
                "quarter:2026-Q5",
                &[(item, 800.0, 300.0, &["0"][..], Some(7))],
            )
            .await,
        )
        .await;
        let tessera_id = ingested_ids(&own)[0];
        drain(&served.server).await;
        accepted(
            ingest(
                &served,
                "join",
                "world",
                &[(item, 60.0, 60.0, &["0"][..], Some(7))],
            )
            .await,
        )
        .await;
        assert_eq!(drop_view(&served, "quarter", "2026-Q5").await["deleted"], 0);
        if restart {
            served = served.restart().await;
        }
        let (all, scored, restricted) = world_as_served(&mut served).await;
        assert!(
            all.contains(&tessera_id) && scored.contains(&tessera_id),
            "`world` serves the item with its score (restart {restart})"
        );
        assert!(
            !restricted.contains(&tessera_id),
            "and not to a principal its label does not admit (restart {restart})"
        );
        observed.push((all, scored, restricted));
    }
    assert_eq!(observed[0], observed[1], "live, then after a restart");
}

/// **A key dropped and created again, with flushed rows on both sides of the drop and one batch
/// still buffered, restarts to what the live service served.**
#[tokio::test]
async fn a_recreated_key_with_flushed_rows_on_both_sides_restarts_to_what_was_served() {
    let mut served = Served::build(build_fixture_bundle).await;
    let rows = |x: f32, n: i32| -> Vec<Row<'static>> {
        (0..n)
            .map(|i| (None, x + i as f32, x, &["0"][..], Some(i)))
            .collect()
    };
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5 first", 1))
            .await
            .status(),
        201
    );
    served.reauthorise().await;
    accepted(ingest(&served, "first", "quarter:2026-Q5", &rows(300.0, 6)).await).await;
    drain(&served.server).await;
    drop_view(&served, "quarter", "2026-Q5").await;
    assert_eq!(
        create(&served, "quarter", "2026-Q5", q_record("Q5 second", 2))
            .await
            .status(),
        201
    );
    served.reauthorise().await;
    accepted(ingest(&served, "second", "quarter:2026-Q5", &rows(500.0, 4)).await).await;
    drain(&served.server).await;
    accepted(ingest(&served, "third", "quarter:2026-Q5", &rows(600.0, 2)).await).await;
    let buffered = served.server.state.engine.buffered_items();
    let before = points(&served, "quarter:2026-Q5").await.len();
    assert_eq!(before, 4, "the second incarnation's flushed rows");

    let mut served = served.restart().await;
    served.reauthorise().await;
    assert_eq!(
        served.server.state.engine.buffered_items(),
        buffered,
        "the restart buffers what the live service had buffered"
    );
    assert_eq!(points(&served, "quarter:2026-Q5").await.len(), before);
    drain(&served.server).await;
    served.reauthorise().await;
    assert_eq!(
        points(&served, "quarter:2026-Q5").await.len(),
        6,
        "the buffered rows flush once, beside the second incarnation's and none of the first's"
    );
}
