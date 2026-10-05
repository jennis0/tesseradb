//! **Ingest rows without coordinates**: rows naming items that exist, by `tessera_id` or by the
//! value of the unique `id`, carrying only the values they change, in JSON by default and Arrow by
//! content type. Such a row edits the item it names and places it nowhere new; one naming no item creates
//! nothing and is refused. What this file pins is the wire: the counts each answer carries, the two
//! encodings landing identical values, and the rows the route refuses.

mod common;

use std::sync::Arc;

use arrow::array::StringArray;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use serde_json::{json, Value};

const N: u64 = 40;

/// The `id` of the item most tests ingest and then edit; the build's items hold `0..N`.
const SUBJECT: u64 = 1_000;

/// One rendered, indexed `f32` at the build, and two `public` vocabularies no build column names,
/// one closed and one open, so a runtime category over either fixes the width at the declaration.
/// The fixture adds the unique `id` ([`ID_ATTRIBUTE`]).
const SCHEMA_TOML: &str = r#"
[[vocabulary]]
name       = "dept"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  eng = 5
  ops = 6

[[vocabulary]]
name       = "grade"
width      = "u16"
value_set  = "open"
visibility = "public"

[[attribute]]
name   = "score"
type   = "f32"
render = true
index  = true
"#;

/// A served fixture with two runtime columns declared: an indexed keyword and an indexed
/// category, each of which a row without coordinates can set.
async fn serve() -> Served {
    let served =
        Served::build(|dir| build_scored(dir, N, &format!("{SCHEMA_TOML}{ID_ATTRIBUTE}"))).await;
    declare(
        &served,
        json!({"name": "tag", "type": "keyword", "index": true}),
    )
    .await;
    declare(
        &served,
        json!({"name": "dept", "type": "category", "vocabulary": "dept", "index": true}),
    )
    .await;
    served
}

async fn declare(served: &Served, body: Value) {
    let resp = served
        .server
        .client
        .put(served.server.control_url("/control/attributes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap_or(Value::Null);
    assert!(
        status == 200 || status == 201,
        "the declaration is accepted: {status} {answer}"
    );
}

/// Ingest one point holding `id` and answer its `tessera_id`. The runtime columns [`serve`]
/// declares are null, since a later row sets them.
async fn ingest_point(served: &Served, batch_id: &str, id: u64) -> u64 {
    ingest_point_with(served, batch_id, json!({"id": id})).await
}

/// Ingest one point with `columns` added to the row, and answer its `tessera_id`.
async fn ingest_point_with(served: &Served, batch_id: &str, columns: Value) -> u64 {
    let mut row = json!({
        "x": 500.0,
        "y": 500.0,
        "access": ["0"],
        "score": 1.0,
        "tag": null,
        "dept": null,
    });
    for (name, value) in columns.as_object().unwrap() {
        row[name] = value.clone();
    }
    let body = json!([row]);
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", "s0")
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "the point is ingested: {answer}");
    ingested_ids(&answer)[0]
}

/// One `POST /control/ingest` request of rows without coordinates, with the view header where
/// `view` says so.
async fn values(served: &Served, batch_id: &str, view: Option<&str>, body: Value) -> (u16, Value) {
    let mut request = served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id);
    if let Some(view) = view {
        request = request.header("x-tessera-view", view);
    }
    let resp = request.json(&body).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// One Arrow batch of one row naming its item by `id`.
fn arrow_values(id: u64, tag: &str) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt64, true),
        Field::new("tag", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(arrow::array::UInt64Array::from_iter([Some(id)])),
            Arc::new(StringArray::from_iter([Some(tag)])),
        ],
    )
    .unwrap();
    let mut writer = arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

/// The drill-down's fields for one `tessera_id`, by name.
async fn item_fields(served: &Served, id: u64) -> Value {
    let token = token_for(&served.server, &["0", "1"][..]).await;
    let resp = served
        .server
        .client
        .post(served.server.viewer_url(&format!("/v1/items/{id}")))
        .bearer_auth(token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    body["fields"].clone()
}

// ---------------------------------------------------------------------------------------------

/// **A row without coordinates edits the item it names, restates, and names nothing it cannot
/// place.** A `200` counts what the batch did; a restatement changes nothing and is counted
/// unchanged; a different value edits the item again; a row naming no item and carrying no
/// position is refused and listed by its row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rows_without_coordinates_edit_restate_and_create_nothing() {
    let served = serve().await;
    let id = ingest_point(&served, "points-1", SUBJECT).await;
    tick(&served.server).await;

    let (status, answer) = values(
        &served,
        "values-1",
        Some("s0"),
        json!([{"id": SUBJECT, "tag": "alpha", "dept": "ops"}]),
    )
    .await;
    assert_eq!(status, 200, "the row is accepted: {answer}");
    assert_eq!(answer["rows"], 1);
    assert_eq!(answer["edited"], 1, "{answer}");
    assert_eq!(
        ingested_ids(&answer),
        vec![id],
        "the item keeps its tessera_id"
    );
    tick(&served.server).await;

    let fields = item_fields(&served, id).await;
    assert_eq!(fields["tag"], json!("alpha"), "{fields}");
    assert_eq!(fields["dept"], json!("ops"), "{fields}");
    assert_eq!(
        fields["score"],
        json!(1.0),
        "a value the row left out is kept: {fields}"
    );

    // A restatement under a fresh batch id changes nothing and is counted unchanged.
    let (status, answer) = values(
        &served,
        "values-2",
        Some("s0"),
        json!([{"id": SUBJECT, "tag": "alpha"}]),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["unchanged"], 1, "{answer}");

    // A different value edits the item again.
    let (status, answer) = values(
        &served,
        "values-3",
        None,
        json!([{"id": SUBJECT, "tag": "beta"}]),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["edited"], 1, "{answer}");
    tick(&served.server).await;
    assert_eq!(item_fields(&served, id).await["tag"], json!("beta"));

    // A row naming no item and carrying no position creates nothing and is refused.
    let (status, answer) = values(
        &served,
        "values-4",
        Some("s0"),
        json!([{"id": SUBJECT + 1, "tag": "gamma"}]),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer["refused"],
        json!([{ "row": 0, "reason": "names_no_item" }]),
        "the refusal names the row and not the id: {answer}"
    );
    assert_eq!(answer["tessera_ids"], json!([null]), "{answer}");
    assert_eq!(answer["created"], 0, "{answer}");
}

/// The keys a viewer route that lists a column's values answers, on one page.
async fn listed_keys(served: &Served, path: &str) -> Vec<String> {
    let token = token_for(&served.server, &["0", "1"]).await;
    let resp = served
        .server
        .client
        .get(served.server.viewer_url(path))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    body["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["key"].as_str().unwrap().to_string())
        .collect()
}

/// `grade`'s value list and its suggestions both carry `key`.
async fn assert_minted_key_is_listed(served: &Served, key: &str) {
    for path in ["/v1/categories/grade", "/v1/categories/grade/suggest?q=g"] {
        let keys = listed_keys(served, path).await;
        assert!(keys.iter().any(|k| k == key), "{path} lists {keys:?}");
    }
}

/// A key of an open vocabulary that no ingest has used is minted by the row without coordinates
/// that names it, and the cell is served after a flush and after a restart. A closed vocabulary's
/// unknown key is refused with nothing written, and the next flush still publishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_row_without_coordinates_mints_a_new_key_of_an_open_vocabulary() {
    let served = serve().await;
    declare(
        &served,
        json!({"name": "grade", "type": "category", "vocabulary": "grade", "index": true}),
    )
    .await;
    let id = ingest_point_with(&served, "points-1", json!({"id": SUBJECT, "grade": null})).await;
    tick(&served.server).await;

    let (status, answer) = values(
        &served,
        "values-1",
        Some("s0"),
        json!([{"id": SUBJECT, "grade": "g0"}]),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["edited"], 1, "{answer}");

    let (status, answer) = values(
        &served,
        "values-2",
        Some("s0"),
        json!([{"id": SUBJECT, "dept": "legal"}]),
    )
    .await;
    assert_eq!(
        status, 422,
        "a closed vocabulary's unknown key is refused: {answer}"
    );

    tick(&served.server).await;
    let fields = item_fields(&served, id).await;
    assert_eq!(fields["grade"], json!("g0"), "{fields}");
    assert!(
        fields["dept"].is_null(),
        "the refused batch wrote nothing: {fields}"
    );
    assert_minted_key_is_listed(&served, "g0").await;

    let served = served.restart().await;
    assert_eq!(item_fields(&served, id).await["grade"], json!("g0"));
    assert_minted_key_is_listed(&served, "g0").await;
    let second = ingest_point_with(&served, "points-2", json!({"grade": null})).await;
    tick(&served.server).await;
    assert!(
        item_fields(&served, second).await["grade"].is_null(),
        "a flush after the restart publishes"
    );
}

/// The keys of the artifacts a viewport over the whole frame serves from `layer`.
async fn served_keys(served: &Served, layer: &str) -> Vec<String> {
    let token = token_for(&served.server, &["0", "1"]).await;
    let resp = served
        .server
        .client
        .post(served.server.viewer_url("/v1/artifacts/viewport"))
        .bearer_auth(token)
        .json(&json!({
            "view": "s0", "zoom": 0, "per_tile": 1000, "bbox": [0.0, 0.0, 1000.0, 1000.0], "layers": "all"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let mut keys: Vec<String> = decode_artifact_frames(&resp.bytes().await.unwrap())
        .artifacts
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.layer == layer)
        .filter_map(|a| a.key)
        .collect();
    keys.sort();
    keys
}

/// A layer whose artifacts are the values of a column gets an artifact for a new value whether
/// the value arrives with a new item or by an edit of one, and both survive a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_value_an_edit_gives_derives_its_artifact_as_a_create_does() {
    let served = serve().await;
    declare(
        &served,
        json!({"name": "grade", "type": "category", "vocabulary": "grade", "index": true}),
    )
    .await;
    let grades = json!({
        "name": "grades",
        "title": "Grades",
        "views": ["s0"],
        "membership": { "attribute": "grade" },
        "visibility": null,
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": null,
        "hierarchy": { "kind": "flat", "prune_children": false },
        "content": { "computed": [], "supplied": [] },
        "depends_on": [],
        "levels": []
    });
    register(&served.server, grades).await;

    ingest_point_with(&served, "points-1", json!({"grade": "g1"})).await;
    ingest_point_with(&served, "points-2", json!({"id": SUBJECT, "grade": null})).await;
    tick(&served.server).await;
    let (status, answer) = values(
        &served,
        "values-1",
        Some("s0"),
        json!([{"id": SUBJECT, "grade": "g2"}]),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    tick(&served.server).await;
    assert_eq!(served_keys(&served, "grades").await, ["g1", "g2"]);

    let served = served.restart().await;
    assert_eq!(served_keys(&served, "grades").await, ["g1", "g2"]);
}

/// **The same batch as JSON and as Arrow lands identical values**. Nothing about the route's
/// semantics depends on which encoding carried it: the Arrow batch sets the value, and the JSON
/// restatement of it changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_two_encodings_land_identical_values() {
    let served = serve().await;
    let id = ingest_point(&served, "points-1", SUBJECT).await;
    tick(&served.server).await;

    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "values-arrow")
        .header("x-tessera-view", "s0")
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(arrow_values(SUBJECT, "alpha"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "the Arrow batch is accepted: {answer}");
    assert_eq!(answer["edited"], 1);
    tick(&served.server).await;
    assert_eq!(item_fields(&served, id).await["tag"], json!("alpha"));

    // The JSON spelling of the same batch: the value is held identically, so it changes nothing.
    let (status, answer) = values(
        &served,
        "values-json",
        Some("s0"),
        json!([{"id": SUBJECT, "tag": "alpha"}]),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["unchanged"], 1, "{answer}");
}

/// **A row may name its item by `tessera_id`**, on `/control/changes`' rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_row_may_name_its_entity_by_tessera_id() {
    let served = serve().await;
    let id = ingest_point(&served, "points-1", SUBJECT).await;
    tick(&served.server).await;

    let (status, answer) = values(
        &served,
        "values-1",
        Some("s0"),
        json!([{"tessera_id": id.to_string(), "tag": "alpha"}]),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["edited"], 1);
}

/// One Arrow batch of one row naming its item by `tessera_id`.
fn arrow_values_by_tessera_id(id: u64, tag: &str) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("tessera_id", DataType::Utf8, true),
        Field::new("tag", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from_iter([Some(id.to_string())])),
            Arc::new(StringArray::from_iter([Some(tag)])),
        ],
    )
    .unwrap();
    let mut writer = arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

/// An Arrow row names its entity by `tessera_id` as a JSON row does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_arrow_row_addressed_by_tessera_id_fills_its_cell() {
    let served = serve().await;
    let id = ingest_point(&served, "points-1", SUBJECT).await;
    tick(&served.server).await;

    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "values-arrow")
        .header("x-tessera-view", "s0")
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(arrow_values_by_tessera_id(id, "alpha"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["edited"], 1);
    tick(&served.server).await;
    assert_eq!(item_fields(&served, id).await["tag"], json!("alpha"));
}

/// A row may name its item by both forms where they agree. A batch whose rows carry no position
/// and no column to name items by is `422`; a row whose identifying cell is null is refused as
/// naming no item. Neither has any effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_row_naming_its_item_by_both_forms_or_neither() {
    let served = serve().await;
    let id = ingest_point(&served, "points-1", SUBJECT).await;
    tick(&served.server).await;

    let (status, answer) = values(
        &served,
        "values-both",
        Some("s0"),
        json!([{"id": SUBJECT, "tessera_id": id.to_string(),
                "tag": "alpha"}]),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(ingested_ids(&answer), vec![id]);

    let (status, answer) = values(
        &served,
        "values-neither",
        Some("s0"),
        json!([{"tag": "beta"}]),
    )
    .await;
    assert_eq!(status, 422, "{answer}");
    let (status, answer) = values(
        &served,
        "values-null",
        Some("s0"),
        json!([{"id": null, "tag": "beta"}]),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer["refused"],
        json!([{ "row": 0, "reason": "names_no_item" }]),
        "{answer}"
    );
    tick(&served.server).await;
    assert_eq!(item_fields(&served, id).await["tag"], json!("alpha"));
}

/// **A group-scoped column is nameable only on a batch that carries the view header**
/// (`ingest.md` §1.4, `views.md` §5). This bundle declares no group, so `sentiment` is a name
/// nothing declares and takes the undeclared-column refusal — which is the same refusal a scoped
/// column takes on a viewless batch, the families a batch may name being empty without a header.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_undeclared_column_is_refused_naming_the_column() {
    let served = serve().await;
    ingest_point(&served, "points-1", SUBJECT).await;
    tick(&served.server).await;

    let (status, answer) = values(
        &served,
        "values-1",
        Some("s0"),
        json!([{"id": SUBJECT, "sentiment": 0.5}]),
    )
    .await;
    assert_eq!(status, 422, "{answer}");
    assert_eq!(answer["error"], "contract", "{answer}");
    assert!(
        answer["detail"].as_str().unwrap().contains("'sentiment'"),
        "{answer}"
    );

    // And a batch that names no view at all takes the same refusal, which is what keeps a scoped
    // column un-nameable without a header.
    let (status, answer) = values(
        &served,
        "values-2",
        None,
        json!([{"id": SUBJECT, "sentiment": 0.5}]),
    )
    .await;
    assert_eq!(status, 422, "{answer}");
}
