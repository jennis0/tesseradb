//! **A writer's terms bound what it writes**: a principal holding `write` and not `write-all`
//! names only the items its terms reach, suppressed ones included. An item outside them, named by
//! `mosaica_id` or by a unique value, names nothing, and the answer is the one an absent item
//! gets, in body and status, strict or not. The one exception is uniqueness: a row creating an
//! item, or setting a value on the item it names, with a value an item outside holds is refused
//! as `value_held`, naming no item. A row setting a label the terms do not satisfy is refused as
//! `label_not_held` and the batch's other rows apply. A writer holding `write-all` writes as the
//! operator does, and all of it holds across a restart.
//!
//! The fixture's items hold the terms [`terms_of`] gives them: every third item `0` and `1`, the
//! rest `0` alone. The writer holds `1`, so its scope is every third item.

mod common;

use std::path::Path;

use arrow::array::{StringArray, UInt64Array};
use common::*;
use serde_json::{json, Value};

const N: u64 = 20;
const WRITER: &str = "pipeline";
const LAYER: &str = "picked";

fn gid_of(e: u64) -> u64 {
    1_000 + e
}

/// An item inside the writer's terms, and one outside.
const INSIDE: u64 = 3;
const OUTSIDE: u64 = 1;
/// An item the operator deletes, whose `mosaica_id` and values then name nothing for anyone.
const DELETED: u64 = 2;
/// A `gid` no item holds.
const ABSENT_GID: u64 = 999_999;

fn fixture(dir: &Path) -> std::path::PathBuf {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let ids: Vec<u64> = (0..N).collect();
    write_points(
        &points,
        &ids,
        scatter,
        vec![
            column(
                "gid",
                false,
                UInt64Array::from_iter_values(ids.iter().map(|e| gid_of(*e))),
            ),
            column(
                "code",
                false,
                StringArray::from_iter_values(ids.iter().map(|e| format!("c{e}"))),
            ),
        ],
    );
    write_pairs_n(&pairs, N);
    let out = dir.join("bundle");
    let schema = "[[attribute]]\nname = \"gid\"\ntype = \"u64\"\nunique = true\n\n\
                  [[attribute]]\nname = \"code\"\ntype = \"keyword\"\nindex = true\n";
    build_declared(&out, &points, &pairs, &format!("{schema}{ID_ATTRIBUTE}"));
    out
}

async fn control(served: &Served, method: reqwest::Method, path: &str, body: Value) -> Value {
    let resp = served
        .server
        .client
        .request(method.clone(), served.server.control_url(path))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    assert!(status < 300, "{method} {path}: {status} {text}");
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// A service principal holding `write` and the term `1`, and an API key for it.
async fn writer(served: &Served) -> String {
    let post = reqwest::Method::POST;
    control(
        served,
        post.clone(),
        "/control/principals",
        json!({ "name": WRITER, "kind": "service" }),
    )
    .await;
    control(
        served,
        post.clone(),
        "/control/grants",
        json!({ "principal": WRITER, "permission": "write" }),
    )
    .await;
    control(
        served,
        post.clone(),
        "/control/grants",
        json!({ "principal": WRITER, "terms": ["1"] }),
    )
    .await;
    let key = control(
        served,
        post,
        &format!("/control/principals/{WRITER}/keys"),
        json!({}),
    )
    .await;
    key["key"].as_str().unwrap().to_owned()
}

/// A response's status and body, without the publication it was answered at.
async fn answered(resp: reqwest::Response) -> (u16, Value) {
    let status = resp.status().as_u16();
    let mut body: Value = resp.json().await.unwrap_or(Value::Null);
    if let Some(object) = body.as_object_mut() {
        object.remove("publication");
    }
    (status, body)
}

async fn ingest(
    served: &Served,
    key: &str,
    batch_id: &str,
    strict: bool,
    rows: Value,
) -> (u16, Value) {
    let query = format!("strict={strict}&wait=visible");
    send_ingest(served, key, batch_id, &query, rows).await
}

/// One batch under `query`. A batch answered after `wait=visible` may be answered after a flush
/// that rotates the log past it, which ends its replays, so a replayed batch is sent without one.
async fn send_ingest(
    served: &Served,
    key: &str,
    batch_id: &str,
    query: &str,
    rows: Value,
) -> (u16, Value) {
    let resp = served
        .server
        .client
        .post(
            served
                .server
                .control_url(&format!("/control/ingest?{query}")),
        )
        .bearer_auth(key)
        .header("x-mosaica-batch-id", batch_id)
        .header("x-mosaica-view", "s0")
        .json(&rows)
        .send()
        .await
        .unwrap();
    answered(resp).await
}

async fn changes(served: &Served, key: &str, strict: bool, changes: Value) -> (u16, Value) {
    let resp = served
        .server
        .client
        .post(
            served
                .server
                .control_url(&format!("/control/changes?strict={strict}&wait=visible")),
        )
        .bearer_auth(key)
        .json(&changes)
        .send()
        .await
        .unwrap();
    answered(resp).await
}

/// How many items a reader holding both terms is served under `filters`.
async fn served_where(served: &Served, filters: Value) -> usize {
    let resp = served
        .server
        .client
        .post(served.server.viewer_url("/v1/viewport"))
        .bearer_auth(&served.token)
        .json(&json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200,
            "filters": &filters
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(
        status,
        200,
        "{filters}: {}",
        String::from_utf8_lossy(&bytes)
    );
    decode_viewport(&bytes).1.len()
}

/// Whether the reader is served the built item `e` with its built values.
async fn served_as_built(served: &Served, e: u64) -> bool {
    served_where(served, json!({ "gid": { "eq": gid_of(e).to_string() } })).await == 1
        && served_where(served, json!({ "code": { "eq": format!("c{e}") } })).await == 1
}

/// The fixture served, the writer's key, and the `mosaica_id` of [`DELETED`], which the operator
/// has deleted.
async fn setup() -> (Served, String, String) {
    let served = Served::build(fixture).await;
    let key = writer(&served).await;
    let deleted = mosaica_id_of(&served.server, DELETED).to_string();
    let (status, body) = changes(
        &served,
        OPERATOR_CREDENTIAL,
        false,
        json!([{ "op": "delete", "match": { "mosaica_id": deleted } }]),
    )
    .await;
    assert_eq!((status, &body["accepted"]), (200, &json!(1)), "{body}");
    (served, key, deleted)
}

/// Pairs of requests the writer sends, one naming [`OUTSIDE`] and one naming nothing held, by
/// `mosaica_id` and by unique value: each pair is answered alike.
fn probes(outside: &str, deleted: &str) -> Vec<(&'static str, Value, Value)> {
    let by_id = |id: &str| json!({ "mosaica_id": id });
    let by_gid = |gid: u64| json!({ "gid": gid.to_string() });
    let mut out = Vec::new();
    for op in ["delete", "suppress", "unsuppress"] {
        out.push((
            "changes",
            json!([{ "op": op, "match": by_id(outside) }]),
            json!([{ "op": op, "match": by_id(deleted) }]),
        ));
        out.push((
            "changes",
            json!([{ "op": op, "match": by_gid(gid_of(OUTSIDE)) }]),
            json!([{ "op": op, "match": by_gid(ABSENT_GID) }]),
        ));
    }
    out.push((
        "ingest",
        json!([{ "mosaica_id": outside, "code": "taken" }]),
        json!([{ "mosaica_id": deleted, "code": "taken" }]),
    ));
    out.push((
        "ingest",
        json!([{ "gid": gid_of(OUTSIDE), "code": "taken" }]),
        json!([{ "gid": ABSENT_GID, "code": "taken" }]),
    ));
    out
}

/// **Outside the writer's terms an item is absent**: an edit, a deletion, a suppression or an
/// unsuppression naming it by `mosaica_id` or by a unique value names nothing, and is answered as
/// one naming an item nobody holds is, in status and body, strict or not. The item is served to
/// others as it was built.
#[tokio::test]
async fn a_write_naming_an_item_outside_the_writers_terms_is_answered_as_one_naming_nothing() {
    let (served, key, deleted) = setup().await;
    let outside = mosaica_id_of(&served.server, OUTSIDE).to_string();
    // A refusal may echo the `mosaica_id` the caller sent, and nothing else of it.
    let sent = |answer: (u16, Value), id: &str| (answer.0, answer.1.to_string().replace(id, "ID"));
    let mut batch = 0;
    for strict in [false, true] {
        for (route, named, absent) in probes(&outside, &deleted) {
            let (a, b) = match route {
                "changes" => (
                    changes(&served, &key, strict, named.clone()).await,
                    changes(&served, &key, strict, absent).await,
                ),
                _ => {
                    batch += 2;
                    (
                        ingest(&served, &key, &format!("p{batch}"), strict, named.clone()).await,
                        ingest(&served, &key, &format!("p{}", batch + 1), strict, absent).await,
                    )
                }
            };
            assert!(a.0 == 200 || a.0 == 404 || a.0 == 409, "{a:?}");
            assert_eq!(
                sent(a.clone(), &outside),
                sent(b, &deleted),
                "{route} {named} (strict: {strict})"
            );
            if a.0 == 200 {
                let refused = a.1["refused"].as_array().unwrap();
                assert_eq!(refused.len(), 1, "{a:?}");
            }
        }
    }
    assert!(
        served_as_built(&served, OUTSIDE).await,
        "the item outside is served as it was built"
    );
}

/// **A value held outside is never given twice**: a row creating an item with a unique value an
/// item outside the writer's terms holds is refused as `value_held`, with no `mosaica_id` and
/// nothing created, and a strict batch's `409` names no item. A row naming an item inside and
/// setting such a value is refused alike. Inside the terms a row naming an item edits it.
#[tokio::test]
async fn an_insert_whose_value_is_held_outside_is_refused_value_held_and_inside_edits() {
    let (served, key, _) = setup().await;
    let outside = mosaica_id_of(&served.server, OUTSIDE).to_string();
    let inside = mosaica_id_of(&served.server, INSIDE).to_string();
    let (x, y) = (500.5, 500.5);

    let (status, body) = ingest(
        &served,
        &key,
        "insert",
        false,
        json!([
            { "gid": gid_of(OUTSIDE), "x": x, "y": y },
            { "mosaica_id": inside, "gid": gid_of(OUTSIDE) },
            { "gid": gid_of(INSIDE), "code": "edited" },
        ]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["refused"],
        json!([{ "row": 0, "reason": "value_held" }, { "row": 1, "reason": "value_held" }])
    );
    assert_eq!(body["mosaica_ids"], json!([null, null, inside]));
    assert_eq!(
        (&body["created"], &body["edited"]),
        (&json!(0), &json!(1)),
        "{body}"
    );

    let (status, body) = ingest(
        &served,
        &key,
        "insert-strict",
        true,
        json!([{ "gid": gid_of(OUTSIDE), "x": x, "y": y }]),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        !body.to_string().contains(&outside),
        "the refusal names no item: {body}"
    );

    assert!(served_as_built(&served, OUTSIDE).await);
    assert_eq!(
        served_where(&served, json!({ "code": { "eq": "edited" } })).await,
        1,
        "the item inside was edited"
    );
}

/// **A label the writer's terms do not satisfy is refused by row**: a new item labelled with it,
/// and an item inside relabelled with it, are refused as `label_not_held`; the batch's other rows
/// apply.
#[tokio::test]
async fn a_row_setting_a_label_the_writer_does_not_hold_is_refused_and_the_others_apply() {
    let (served, key, _) = setup().await;
    let inside = mosaica_id_of(&served.server, INSIDE).to_string();
    let (status, body) = ingest(
        &served,
        &key,
        "labels",
        false,
        json!([
            { "gid": 5_001, "x": 400.5, "y": 400.5, "access": ["0"] },
            { "gid": 5_002, "x": 401.5, "y": 401.5, "access": ["1"] },
            { "mosaica_id": inside, "access": ["0"] },
        ]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["refused"],
        json!([{ "row": 0, "reason": "label_not_held" }, { "row": 2, "reason": "label_not_held" }])
    );
    assert_eq!(body["created"], 1, "{body}");
    assert!(body["mosaica_ids"][1].is_string(), "{body}");
    assert!(
        body["mosaica_ids"][0].is_null() && body["mosaica_ids"][2].is_null(),
        "{body}"
    );

    // The item inside keeps its label: a reader holding `1` alone is still served it, and the
    // new item labelled `1`.
    let only_one = token_for(&served.server, &["1"]).await;
    for (gid, expected) in [(gid_of(INSIDE), 1), (5_002, 1), (5_001, 0)] {
        let resp = served
            .server
            .client
            .post(served.server.viewer_url("/v1/viewport"))
            .bearer_auth(&only_one)
            .json(&json!({
                "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200,
                "filters": { "gid": { "eq": gid.to_string() } }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            decode_viewport(&resp.bytes().await.unwrap()).1.len(),
            expected,
            "{gid}"
        );
    }
}

/// **A scoped writer lifts a suppression inside its terms and never sees one outside**, and a
/// membership spelled by exclusion is complemented within its terms.
#[tokio::test]
async fn a_scoped_writer_unsuppresses_inside_and_complements_within_its_terms() {
    let (served, key, _) = setup().await;
    let inside = mosaica_id_of(&served.server, INSIDE).to_string();
    let outside = mosaica_id_of(&served.server, OUTSIDE).to_string();
    for id in [&inside, &outside] {
        let (status, _) = changes(
            &served,
            OPERATOR_CREDENTIAL,
            false,
            json!([{ "op": "suppress", "match": { "mosaica_id": id } }]),
        )
        .await;
        assert_eq!(status, 200);
    }
    let (status, body) = changes(
        &served,
        &key,
        false,
        json!([
            { "op": "unsuppress", "match": { "mosaica_id": inside } },
            { "op": "unsuppress", "match": { "mosaica_id": outside } },
        ]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["accepted"], 1, "{body}");
    assert_eq!(
        body["refused"],
        json!([{ "row": 1, "reason": "unknown_mosaica_id" }])
    );
    assert!(served_as_built(&served, INSIDE).await);
    assert_eq!(
        served_where(
            &served,
            json!({ "gid": { "eq": gid_of(OUTSIDE).to_string() } })
        )
        .await,
        0
    );

    register(&served.server, flat_layer(LAYER)).await;
    let resp = served
        .server
        .client
        .put(
            served
                .server
                .control_url(&format!("/control/layers/{LAYER}/artifacts")),
        )
        .bearer_auth(&key)
        .json(&json!({ "artifacts": [{ "key": "all-but-one", "excluding": members([INSIDE]) }] }))
        .send()
        .await
        .unwrap();
    let (status, body) = answered(resp).await;
    assert_eq!(status, 201, "{body}");
    let inside_count = (0..N).filter(|e| e % 3 == 0).count() - 1;
    assert_eq!(
        body["joined"], inside_count,
        "the complement is the items inside, but one: {body}"
    );
}

/// **`write-all` writes as the operator does**: the same writer, granted it, edits, suppresses
/// and upserts the item outside its terms.
#[tokio::test]
async fn a_writer_holding_write_all_names_every_item() {
    let (served, key, _) = setup().await;
    let outside = mosaica_id_of(&served.server, OUTSIDE).to_string();
    control(
        &served,
        reqwest::Method::POST,
        "/control/grants",
        json!({ "principal": WRITER, "permission": "write-all" }),
    )
    .await;
    let (status, body) = ingest(
        &served,
        &key,
        "upsert",
        false,
        json!([{ "gid": gid_of(OUTSIDE), "x": 500.5, "y": 500.5, "code": "moved" }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["refused"], json!([]));
    assert_eq!(body["mosaica_ids"], json!([outside]));
    assert_eq!(body["edited"], 1, "{body}");
    let (status, body) = changes(
        &served,
        &key,
        false,
        json!([{ "op": "suppress", "match": { "gid": gid_of(OUTSIDE).to_string() } }]),
    )
    .await;
    assert_eq!((status, &body["accepted"]), (200, &json!(1)), "{body}");
}

/// **Across a restart** the writer still names nothing outside its terms, and a batch it sent
/// before is answered the receipt it was given, its refusals' reasons included.
#[tokio::test]
async fn a_scoped_writers_answers_hold_across_a_restart() {
    let (served, key, _) = setup().await;
    let rows = json!([
        { "gid": gid_of(OUTSIDE), "x": 500.5, "y": 500.5 },
        { "gid": 5_001, "x": 400.5, "y": 400.5, "access": ["0"] },
        { "gid": 5_002, "x": 401.5, "y": 401.5, "access": ["1"] },
    ]);
    let first = send_ingest(&served, &key, "before", "", rows.clone()).await;
    assert_eq!(first.0, 200, "{first:?}");
    assert_eq!(
        first.1["refused"],
        json!([{ "row": 0, "reason": "value_held" }, { "row": 1, "reason": "label_not_held" }])
    );
    let outside = mosaica_id_of(&served.server, OUTSIDE).to_string();

    // The test server's catalogue is its process's, so the writer is declared again.
    let served = served.restart().await;
    let key = writer(&served).await;
    let replayed = send_ingest(&served, &key, "before", "", rows).await;
    assert_eq!(replayed.1["replayed"], true, "{replayed:?}");
    assert_eq!(replayed.1["refused"], first.1["refused"]);
    assert_eq!(replayed.1["mosaica_ids"], first.1["mosaica_ids"]);

    let named = changes(
        &served,
        &key,
        false,
        json!([{ "op": "delete", "match": { "mosaica_id": outside } }]),
    )
    .await;
    assert_eq!(
        named.1["refused"],
        json!([{ "row": 0, "reason": "unknown_mosaica_id" }])
    );
    let again = ingest(
        &served,
        &key,
        "after",
        false,
        json!([{ "gid": gid_of(OUTSIDE), "x": 500.5, "y": 500.5 }]),
    )
    .await;
    assert_eq!(
        again.1["refused"],
        json!([{ "row": 0, "reason": "value_held" }])
    );
    assert!(served_as_built(&served, OUTSIDE).await);
}
