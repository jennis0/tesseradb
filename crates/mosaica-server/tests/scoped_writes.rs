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

/// One batch under `query`.
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

/// `method path` with `body`, sent with `key`: the status and the body, without the publication.
async fn send(
    served: &Served,
    key: &str,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> (u16, Value) {
    let resp = served
        .server
        .client
        .request(method, served.server.control_url(path))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .unwrap();
    answered(resp).await
}

/// `answer` with every `from` in its body spelled `to`: two answers naming different things are
/// compared as answers.
fn spelt(answer: &(u16, Value), from: &str, to: &str) -> (u16, String) {
    (answer.0, answer.1.to_string().replace(from, to))
}

/// The writer's grant of `write-all`.
async fn grant_write_all(served: &Served) {
    control(
        served,
        reqwest::Method::POST,
        "/control/grants",
        json!({ "principal": WRITER, "permission": "write-all" }),
    )
    .await;
}

const LABELLED: &str = "labelled";
const COUNTED: &str = "counted";

/// A nested layer over `s0` whose artifacts carry their own labels.
fn labelled_layer() -> Value {
    let mut layer = flat_layer(LABELLED);
    layer["artifact_visibility"] = json!({ "field": "team", "default": "inherited" });
    layer["hierarchy"] = json!({ "kind": "nested", "prune_children": false });
    layer
}

/// The `mosaica_id` of each artifact a publication answered, in its order.
fn artifact_ids(body: &Value) -> Vec<String> {
    body["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["mosaica_id"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The labelled layer, with `secret` labelled `0`, outside the writer's terms, `open` labelled
/// `1`, inside them, and the `mosaica_id`s of `secret` and of an artifact the operator deleted.
async fn labelled_setup() -> (Served, String, String, String) {
    let (served, key, _) = setup().await;
    register(&served.server, labelled_layer()).await;
    let put = reqwest::Method::PUT;
    let path = format!("/control/layers/{LABELLED}/artifacts");
    let (status, body) = send(
        &served,
        OPERATOR_CREDENTIAL,
        put,
        &path,
        json!({ "artifacts": [
            { "key": "secret", "members": members([INSIDE]), "access": ["0"] },
            { "key": "open", "members": members([INSIDE]), "access": ["1"] },
            { "key": "gone", "members": members([INSIDE]), "access": ["1"] },
        ] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let ids = artifact_ids(&body);
    let (status, body) = changes(
        &served,
        OPERATOR_CREDENTIAL,
        false,
        json!([{ "op": "delete", "match": { "mosaica_id": ids[2] } }]),
    )
    .await;
    assert_eq!((status, &body["accepted"]), (200, &json!(1)), "{body}");
    (served, key, ids[0].clone(), ids[2].clone())
}

/// **An artifact outside the writer's terms is named by nothing**: a growth naming its key, a
/// publication naming it as a parent, and a change naming its `mosaica_id` are answered as ones
/// naming an artifact nobody holds. A publication under its key is refused as the key taken,
/// naming no artifact, since a key names one artifact. A writer holding `write-all` grows it.
#[tokio::test]
async fn an_artifact_outside_the_writers_terms_is_named_by_nothing_and_its_key_is_kept() {
    let (served, key, secret, gone) = labelled_setup().await;
    let path = format!("/control/layers/{LABELLED}/artifacts");
    let (put, patch) = (reqwest::Method::PUT, reqwest::Method::PATCH);
    let grow = |name: &str| json!({ "artifacts": [{ "key": name, "members": members([6]) }] });
    let child = |parent: &str| {
        json!({ "artifacts": [
            { "key": "child", "parent": [parent], "members": members([6]), "access": ["1"] },
        ] })
    };
    for strict in [false, true] {
        let path = format!("{path}?strict={strict}");
        let a = send(&served, &key, patch.clone(), &path, grow("secret")).await;
        let b = send(&served, &key, patch.clone(), &path, grow("absent")).await;
        assert!(a.0 >= 400, "{a:?}");
        assert_eq!(spelt(&a, "secret", "K"), spelt(&b, "absent", "K"));
        let a = send(&served, &key, put.clone(), &path, child("secret")).await;
        let b = send(&served, &key, put.clone(), &path, child("absent")).await;
        assert!(a.0 >= 400, "{a:?}");
        assert_eq!(spelt(&a, "secret", "K"), spelt(&b, "absent", "K"));
    }
    // A fill naming it as a parent, and an attachment to it from a layer depending on this one.
    let fill = |parent: &str| json!({ "artifacts": [{ "key": "open", "parent": [parent] }] });
    let a = send(&served, &key, patch.clone(), &path, fill("secret")).await;
    let b = send(&served, &key, patch.clone(), &path, fill("absent")).await;
    assert!(a.0 >= 400, "{a:?}");
    assert_eq!(spelt(&a, "secret", "K"), spelt(&b, "absent", "K"));
    let mut notes = flat_layer("notes");
    notes["depends_on"] = json!([LABELLED]);
    register(&served.server, notes).await;
    let attached = |target: &str| {
        json!({ "artifacts": [{
            "key": "note", "members": members([6]),
            "attached_to": { "layer": LABELLED, "level": 0, "key": target },
        }] })
    };
    let a = send(
        &served,
        &key,
        put.clone(),
        "/control/layers/notes/artifacts",
        attached("secret"),
    )
    .await;
    let b = send(
        &served,
        &key,
        put.clone(),
        "/control/layers/notes/artifacts",
        attached("absent"),
    )
    .await;
    assert!(a.0 >= 400, "{a:?}");
    assert_eq!(spelt(&a, "secret", "K"), spelt(&b, "absent", "K"));

    for op in ["suppress", "delete"] {
        let a = changes(
            &served,
            &key,
            false,
            json!([{ "op": op, "match": { "mosaica_id": secret } }]),
        )
        .await;
        let b = changes(
            &served,
            &key,
            false,
            json!([{ "op": op, "match": { "mosaica_id": gone } }]),
        )
        .await;
        assert_eq!(
            a.1["refused"],
            json!([{ "row": 0, "reason": "unknown_mosaica_id" }]),
            "{a:?}"
        );
        assert_eq!(spelt(&a, &secret, "ID"), spelt(&b, &gone, "ID"));
    }

    let (status, body) = send(
        &served,
        &key,
        put.clone(),
        &path,
        json!({ "artifacts": [{ "key": "secret", "members": members([6]), "access": ["1"] }] }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        !body.to_string().contains(&secret),
        "the refusal names no artifact: {body}"
    );

    let (status, body) = send(&served, &key, patch.clone(), &path, grow("open")).await;
    assert_eq!(
        (status, &body["artifacts"][0]["joined"]),
        (200, &json!(1)),
        "{body}"
    );

    grant_write_all(&served).await;
    let (status, body) = send(&served, &key, patch, &path, grow("secret")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["artifacts"][0]["mosaica_id"], json!(secret));
}

/// **An artifact label the writer's terms do not satisfy is refused by artifact**: the
/// publication's other artifacts are published, and the refused one has no `mosaica_id`. A strict
/// publication carrying one is refused whole.
#[tokio::test]
async fn an_artifact_labelled_out_of_the_writers_terms_is_refused_and_the_others_publish() {
    let (served, key, _, _) = labelled_setup().await;
    let path = format!("/control/layers/{LABELLED}/artifacts");
    let batch = json!({ "artifacts": [
        { "key": "theirs", "members": members([INSIDE]), "access": ["0"] },
        { "key": "ours", "members": members([INSIDE]), "access": ["1"] },
    ] });
    let (status, body) = send(
        &served,
        &key,
        reqwest::Method::PUT,
        &format!("{path}?strict=true"),
        batch.clone(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    let (status, body) = send(&served, &key, reqwest::Method::PUT, &path, batch).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        body["refused"],
        json!([{ "artifact": 0, "reason": "label_not_held" }])
    );
    assert_eq!(body["created"], 1, "{body}");
    assert!(body["artifacts"][0]["mosaica_id"].is_null(), "{body}");
    assert!(body["artifacts"][1]["mosaica_id"].is_string(), "{body}");

    let (status, body) = send(
        &served,
        &key,
        reqwest::Method::PATCH,
        &path,
        json!({ "artifacts": [{ "key": "theirs", "members": members([6]) }] }),
    )
    .await;
    assert!(
        status >= 400,
        "the refused artifact was not published: {body}"
    );
}

/// **An artifact is inside the writer's terms only where a reader holding them would be served
/// it**: on a layer requiring a visible member, an artifact whose members are all outside the
/// terms is named by nothing, and one with a member inside grows.
#[tokio::test]
async fn an_artifact_whose_members_are_all_outside_the_writers_terms_is_named_by_nothing() {
    let (served, key, _) = setup().await;
    let mut layer = flat_layer(COUNTED);
    layer["require_member_visibility"] = json!({ "count": 1 });
    register(&served.server, layer).await;
    let path = format!("/control/layers/{COUNTED}/artifacts");
    let (status, body) = send(
        &served,
        OPERATOR_CREDENTIAL,
        reqwest::Method::PUT,
        &path,
        json!({ "artifacts": [
            { "key": "outsiders", "members": members([OUTSIDE, 4]) },
            { "key": "mixed", "members": members([OUTSIDE, INSIDE]) },
        ] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let grow = |name: &str| json!({ "artifacts": [{ "key": name, "members": members([6]) }] });
    let a = send(
        &served,
        &key,
        reqwest::Method::PATCH,
        &path,
        grow("outsiders"),
    )
    .await;
    let b = send(&served, &key, reqwest::Method::PATCH, &path, grow("absent")).await;
    assert!(a.0 >= 400, "{a:?}");
    assert_eq!(spelt(&a, "outsiders", "K"), spelt(&b, "absent", "K"));
    let (status, body) = send(&served, &key, reqwest::Method::PATCH, &path, grow("mixed")).await;
    assert_eq!(
        (status, &body["artifacts"][0]["joined"]),
        (200, &json!(1)),
        "{body}"
    );
}

/// **A layer column naming an artifact outside the writer's terms names nothing**: on a closed
/// layer it is answered as a key no artifact holds, and on an open one it is refused as the key
/// taken, naming no artifact, where a key nobody holds mints.
#[tokio::test]
async fn an_ingest_column_naming_an_artifact_outside_the_writers_terms_names_nothing() {
    let (served, key, _) = setup().await;
    let mut closed = flat_layer(COUNTED);
    closed["require_member_visibility"] = json!({ "count": 1 });
    let mut open = closed.clone();
    open["name"] = json!("minted");
    open["value_set"] = json!("open");
    for layer in [closed, open] {
        let name = layer["name"].as_str().unwrap().to_string();
        register(&served.server, layer).await;
        let (status, body) = send(
            &served,
            OPERATOR_CREDENTIAL,
            reqwest::Method::PUT,
            &format!("/control/layers/{name}/artifacts"),
            json!({ "artifacts": [{ "key": "outsiders", "members": members([OUTSIDE, 4]) }] }),
        )
        .await;
        assert_eq!(status, 201, "{body}");
    }
    let row = |layer: &str, artifact: &str| json!([{ "gid": 8_001, "x": 2.5, "y": 2.5, "access": ["1"], layer: artifact }]);
    let a = ingest(
        &served,
        &key,
        "closed-outside",
        false,
        row(COUNTED, "outsiders"),
    )
    .await;
    let b = ingest(
        &served,
        &key,
        "closed-absent",
        false,
        row(COUNTED, "absent"),
    )
    .await;
    assert!(a.0 >= 400, "{a:?}");
    assert_eq!(spelt(&a, "outsiders", "K"), spelt(&b, "absent", "K"));

    let (status, body) = ingest(
        &served,
        &key,
        "open-outside",
        false,
        row("minted", "outsiders"),
    )
    .await;
    assert!(status >= 400, "{body}");
    let (status, body) = ingest(&served, &key, "open-absent", false, row("minted", "fresh")).await;
    assert_eq!((status, &body["minted"]), (200, &json!(1)), "{body}");
}

/// `key`'s batch `batch`, one row naming the artifact `artifact` of the open layer `minted`, held
/// before its window closes while the operator publishes `artifact` with members outside the
/// writer's terms: the batch's answer.
async fn ingest_raced_by_a_publication(
    served: &Served,
    key: &str,
    batch: &str,
    gid: u64,
    artifact: &str,
) -> (u16, Value) {
    let engine = &served.server.state.engine;
    let queued = |n: u64| async move {
        while engine.work_enqueued_for_test() < n {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    engine.set_work_pass_paused_for_test(true);
    let enqueued = engine.work_enqueued_for_test();
    let row = json!([{ "gid": gid, "x": 2.5, "y": 2.5, "access": ["1"], "minted": artifact }]);
    let ingesting = ingest(served, key, batch, false, row);
    let publishing = async {
        queued(enqueued + 1).await;
        send(
            served,
            OPERATOR_CREDENTIAL,
            reqwest::Method::PUT,
            "/control/layers/minted/artifacts",
            json!({ "artifacts": [{ "key": artifact, "members": members([OUTSIDE, 4]) }] }),
        )
        .await
    };
    let releasing = async {
        queued(enqueued + 2).await;
        engine.set_work_pass_paused_for_test(false);
    };
    let (ingested, published, ()) = tokio::join!(ingesting, publishing, releasing);
    assert_eq!(published.0, 201, "{published:?}");
    ingested
}

/// **A key an artifact outside the writer's terms comes to hold between its batch's admission and
/// the close is answered as at admission**: the batch is refused as the key held, and its item
/// joins nothing. A writer holding `write-all` grows the artifact, as before.
#[tokio::test]
async fn a_key_held_outside_the_writers_terms_by_the_close_is_answered_as_at_admission() {
    let (served, key, _) = setup().await;
    let mut layer = flat_layer("minted");
    layer["value_set"] = json!("open");
    layer["require_member_visibility"] = json!({ "count": 1 });
    register(&served.server, layer).await;

    let raced = ingest_raced_by_a_publication(&served, &key, "raced", 8_002, "late").await;
    assert!(raced.0 >= 400, "{raced:?}");
    let row = json!([{ "gid": 8_002, "x": 2.5, "y": 2.5, "access": ["1"], "minted": "late" }]);
    let admitted = ingest(&served, &key, "raced", false, row).await;
    assert_eq!(raced, admitted);
    assert_eq!(
        served_where(&served, json!({ "gid": { "eq": "8002" } })).await,
        0
    );

    grant_write_all(&served).await;
    let (status, body) =
        ingest_raced_by_a_publication(&served, &key, "raced-all", 8_003, "later").await;
    assert_eq!(
        (status, &body["minted"], &body["joined"]),
        (200, &json!(0), &json!(1)),
        "{body}"
    );
}

const HIDDEN: &str = "hidden";
const SECRET_VIEW: &str = "secret";
const GROUP: &str = "quarters";

/// A view, a group, a view of it and a layer, each gated by a term the writer does not hold, and
/// the layer's `mosaica_id`.
async fn gated_setup() -> (Served, String, String) {
    let (served, key, _) = setup().await;
    let mut layer = flat_layer(HIDDEN);
    layer["visibility"] = json!("2");
    let (status, body) = put_layer(&served.server, layer).await;
    assert_eq!(status, 201, "{body}");
    let hidden = body["mosaica_id"].as_str().unwrap().to_string();
    let put = reqwest::Method::PUT;
    for (path, body) in [
        (format!("/control/views/{SECRET_VIEW}"), plain_view("2")),
        (format!("/control/view_groups/{GROUP}"), plain_view("2")),
        (format!("/control/views/{GROUP}/q1"), json!({})),
    ] {
        let (status, body) = send(&served, OPERATOR_CREDENTIAL, put.clone(), &path, body).await;
        assert_eq!(status, 201, "{path}: {body}");
    }
    (served, key, hidden)
}

/// A plain view's or a group's declaration, gated by `term`.
fn plain_view(term: &str) -> Value {
    json!({ "extent": { "x": [0.0, 1000.0], "y": [0.0, 1000.0] }, "visibility": term })
}

/// Requests the writer sends, each naming a layer, view or group out of its reach, as
/// `(method, path, body, name)` with `NAME` in the path and body standing for the name: the drops
/// last, so that a drop that went through would not change what the others meet.
fn named_out_of_reach() -> Vec<(reqwest::Method, String, Value, &'static str)> {
    let artifacts = json!({ "artifacts": [{ "key": "a", "members": members([INSIDE]) }] });
    let mut layer = flat_layer("fresh");
    layer["views"] = json!(["NAME"]);
    vec![
        (
            reqwest::Method::PUT,
            "/control/layers/NAME/artifacts".into(),
            artifacts.clone(),
            HIDDEN,
        ),
        (
            reqwest::Method::PATCH,
            "/control/layers/NAME/artifacts".into(),
            artifacts,
            HIDDEN,
        ),
        (
            reqwest::Method::PUT,
            "/control/layers".into(),
            layer,
            SECRET_VIEW,
        ),
        (
            reqwest::Method::PUT,
            "/control/views/NAME/q2".into(),
            json!({}),
            GROUP,
        ),
        (
            reqwest::Method::DELETE,
            "/control/views/NAME/q1".into(),
            Value::Null,
            GROUP,
        ),
        (
            reqwest::Method::DELETE,
            "/control/layers/NAME".into(),
            Value::Null,
            HIDDEN,
        ),
    ]
}

/// Each of [`named_out_of_reach`] sent with `key`, and the same naming `never`, which nobody
/// declared: answered alike, and the second's answer names nothing out of reach.
async fn probe_out_of_reach(served: &Served, key: &str) {
    for (method, path, body, held) in named_out_of_reach() {
        let named = |name: &str| {
            let body = serde_json::from_str(&body.to_string().replace("NAME", name)).unwrap();
            (path.replace("NAME", name), body)
        };
        let (a_path, a_body) = named(held);
        let (b_path, b_body) = named("never");
        let a = send(served, key, method.clone(), &a_path, a_body).await;
        let b = send(served, key, method.clone(), &b_path, b_body).await;
        assert!(a.0 >= 400, "{method} {a_path}: {a:?}");
        assert_eq!(
            spelt(&a, held, "NAME"),
            spelt(&b, "never", "NAME"),
            "{method} {a_path}"
        );
        for other in [HIDDEN, SECRET_VIEW, GROUP] {
            assert!(
                !b.1.to_string().contains(other),
                "{method} {b_path} names {other}: {b:?}"
            );
        }
    }
}

/// **A layer, a view or a group out of the writer's reach is unknown to it**: dropping it,
/// publishing into it, naming it in a layer's views or as a group to create a view of are each
/// answered as for a name nobody declared, and no refusal lists it. Declaring one under its name
/// is refused as the name taken, never answered as the declaration held or as one differing from
/// it. A writer holding `write-all` is answered as the operator is.
#[tokio::test]
async fn a_layer_view_or_group_out_of_the_writers_reach_is_unknown_and_its_name_taken() {
    let (served, key, hidden) = gated_setup().await;

    // Ingest names a view by its header and a layer by its column.
    let ingest_into = |view: &'static str, column: &'static str| {
        let served = &served;
        let key = key.clone();
        async move {
            let resp = served
                .server
                .client
                .post(served.server.control_url("/control/ingest"))
                .bearer_auth(&key)
                .header("x-mosaica-batch-id", format!("{view}-{column}"))
                .header("x-mosaica-view", view)
                .json(&json!([{ "gid": 7_001, "x": 1.5, "y": 1.5, column: "a" }]))
                .send()
                .await
                .unwrap();
            answered(resp).await
        }
    };
    let a = ingest_into(SECRET_VIEW, "code").await;
    let b = ingest_into("never", "code").await;
    assert_eq!(spelt(&a, SECRET_VIEW, "NAME"), spelt(&b, "never", "NAME"));
    let a = ingest_into("s0", HIDDEN).await;
    let b = ingest_into("s0", "never").await;
    assert_eq!(spelt(&a, HIDDEN, "NAME"), spelt(&b, "never", "NAME"));

    // The layer's `mosaica_id` names nothing.
    let a = changes(
        &served,
        &key,
        false,
        json!([{ "op": "suppress", "match": { "mosaica_id": hidden } }]),
    )
    .await;
    assert_eq!(
        a.1["refused"],
        json!([{ "row": 0, "reason": "unknown_mosaica_id" }]),
        "{a:?}"
    );

    // A declaration the writer would reach, under a name out of its reach, is the name taken,
    // alike for two declarations.
    for (path, held) in [
        (format!("/control/views/{SECRET_VIEW}"), plain_view("2")),
        (format!("/control/view_groups/{GROUP}"), plain_view("2")),
    ] {
        let mine = plain_view("1");
        let mut other = mine.clone();
        other["extent"]["x"] = json!([0.0, 10.0]);
        let a = send(&served, &key, reqwest::Method::PUT, &path, mine).await;
        let b = send(&served, &key, reqwest::Method::PUT, &path, other).await;
        assert_eq!(a.0, 409, "{path}: {a:?}");
        assert_eq!(a, b, "{path}");
        let (status, body) = send(
            &served,
            OPERATOR_CREDENTIAL,
            reqwest::Method::PUT,
            &path,
            held.clone(),
        )
        .await;
        assert_eq!(
            (status, &body["existing"]),
            (200, &json!(true)),
            "{path}: {body}"
        );
    }
    probe_out_of_reach(&served, &key).await;

    grant_write_all(&served).await;
    let (status, body) = send(
        &served,
        &key,
        reqwest::Method::PUT,
        &format!("/control/views/{SECRET_VIEW}"),
        plain_view("2"),
    )
    .await;
    assert_eq!((status, &body["existing"]), (200, &json!(true)), "{body}");
}

/// The declarations of a layer, a plain view, a view group and a view of the group [`OURS`],
/// each named for `name` and gated by `term`, as `(path, body)`.
fn declarations(name: &str, term: &str) -> Vec<(String, Value)> {
    let mut layer = flat_layer(&format!("layer-{name}"));
    layer["visibility"] = json!(term);
    vec![
        ("/control/layers".to_string(), layer),
        (format!("/control/views/plain-{name}"), plain_view(term)),
        (
            format!("/control/view_groups/group-{name}"),
            plain_view(term),
        ),
        (
            format!("/control/views/{OURS}/view-{name}"),
            json!({ "visibility": term }),
        ),
    ]
}

/// A view group the writer reaches.
const OURS: &str = "ours";

/// **A writer cannot declare what it would not reach**: a layer, a plain view, a view group or a
/// view of a group whose visibility its terms do not satisfy is refused, `422`, and nothing is
/// declared, alike whether or not something out of its reach holds the name. One whose visibility
/// they satisfy is declared, and a writer holding `write-all` declares either.
#[tokio::test]
async fn a_declaration_the_writer_would_not_reach_is_refused() {
    let (served, key, _) = gated_setup().await;
    let (status, body) = send(
        &served,
        OPERATOR_CREDENTIAL,
        reqwest::Method::PUT,
        &format!("/control/view_groups/{OURS}"),
        plain_view("1"),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    for (path, body) in declarations("theirs", "2") {
        let (status, refused) = send(&served, &key, reqwest::Method::PUT, &path, body).await;
        assert_eq!(status, 422, "{path}: {refused}");
    }
    // Under a name something out of the writer's reach holds, the answer is the same.
    let mut hidden = flat_layer(HIDDEN);
    hidden["visibility"] = json!("2");
    for (path, held, fresh) in [
        ("/control/layers".to_string(), hidden, "layer-never"),
        (
            format!("/control/views/{SECRET_VIEW}"),
            plain_view("2"),
            "never",
        ),
        (
            format!("/control/view_groups/{GROUP}"),
            plain_view("2"),
            "never",
        ),
    ] {
        let named = |name: &str| path.replace(SECRET_VIEW, name).replace(GROUP, name);
        let mut absent = held.clone();
        if absent["name"].is_string() {
            absent["name"] = json!(fresh);
        }
        let a = send(&served, &key, reqwest::Method::PUT, &path, held).await;
        let b = send(&served, &key, reqwest::Method::PUT, &named(fresh), absent).await;
        assert_eq!(a.0, 422, "{path}: {a:?}");
        let held_name = [HIDDEN, SECRET_VIEW, GROUP];
        let spelt_a = held_name
            .iter()
            .fold(a.1.to_string(), |text, name| text.replace(name, "NAME"));
        assert_eq!(spelt_a, b.1.to_string().replace(fresh, "NAME"), "{path}");
    }
    let (status, body) = send(
        &served,
        OPERATOR_CREDENTIAL,
        reqwest::Method::PUT,
        "/control/views/plain-theirs",
        plain_view("2"),
    )
    .await;
    assert_eq!(
        (status, &body["existing"]),
        (201, &json!(false)),
        "nothing was declared: {body}"
    );

    for (path, body) in declarations("mine", "1") {
        let (status, created) = send(&served, &key, reqwest::Method::PUT, &path, body).await;
        assert_eq!(status, 201, "{path}: {created}");
    }

    grant_write_all(&served).await;
    for (path, body) in declarations("all", "2") {
        let (status, created) = send(&served, &key, reqwest::Method::PUT, &path, body).await;
        assert_eq!(status, 201, "{path}: {created}");
    }
}

/// **Across a restart** a layer, a view or a group out of the writer's reach stays unknown to it,
/// and an artifact outside its terms stays named by nothing.
#[tokio::test]
async fn what_is_out_of_the_writers_reach_stays_so_across_a_restart() {
    let (served, _, hidden) = gated_setup().await;
    register(&served.server, labelled_layer()).await;
    let path = format!("/control/layers/{LABELLED}/artifacts");
    let (status, body) = send(
        &served,
        OPERATOR_CREDENTIAL,
        reqwest::Method::PUT,
        &path,
        json!({ "artifacts": [{ "key": "secret", "members": members([INSIDE]), "access": ["0"] }] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let served = served.restart().await;
    let key = writer(&served).await;
    let a = changes(
        &served,
        &key,
        false,
        json!([{ "op": "suppress", "match": { "mosaica_id": hidden } }]),
    )
    .await;
    assert_eq!(
        a.1["refused"],
        json!([{ "row": 0, "reason": "unknown_mosaica_id" }]),
        "{a:?}"
    );
    let grow = |name: &str| json!({ "artifacts": [{ "key": name, "members": members([6]) }] });
    let a = send(&served, &key, reqwest::Method::PATCH, &path, grow("secret")).await;
    let b = send(&served, &key, reqwest::Method::PATCH, &path, grow("absent")).await;
    assert!(a.0 >= 400, "{a:?}");
    assert_eq!(spelt(&a, "secret", "K"), spelt(&b, "absent", "K"));
    probe_out_of_reach(&served, &key).await;
}

/// `PUT /control/attributes` declaring `code` unique.
fn unique_code() -> Value {
    json!({ "name": "code", "type": "keyword", "index": true, "unique": true })
}

/// **A `unique` declaration over values held twice names to the writer only values every holder
/// of which is inside its terms**: the count and the values of those, or, where there are none,
/// that a value is held twice and nothing more. The operator is told every one.
#[tokio::test]
async fn a_scoped_unique_declaration_names_only_duplicates_held_inside() {
    let (served, key, _) = setup().await;
    let rows = json!([
        { "gid": gid_of(INSIDE), "code": "dup-in" },
        { "gid": gid_of(6), "code": "dup-in" },
        { "gid": gid_of(OUTSIDE), "code": "dup-out" },
        { "gid": gid_of(4), "code": "dup-out" },
    ]);
    let (status, body) = ingest(&served, OPERATOR_CREDENTIAL, "dups", false, rows).await;
    assert_eq!((status, &body["edited"]), (200, &json!(4)), "{body}");
    let put = reqwest::Method::PUT;
    let (status, body) = send(
        &served,
        &key,
        put.clone(),
        "/control/attributes",
        unique_code(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    let text = body["detail"].as_str().unwrap_or_default().to_string();
    assert!(
        text.contains("dup-in") && !text.contains("dup-out"),
        "{text}"
    );
    assert_eq!(numbers_in(&text), vec!["1"], "{text}");
    let (status, body) = send(
        &served,
        OPERATOR_CREDENTIAL,
        put.clone(),
        "/control/attributes",
        unique_code(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    let text = body["detail"].as_str().unwrap_or_default().to_string();
    assert!(
        text.contains("dup-in") && text.contains("dup-out"),
        "{text}"
    );
    assert_eq!(numbers_in(&text), vec!["2"], "{text}");

    // With only the duplicate held outside left, the writer is told a value is held twice.
    let rows = json!([{ "gid": gid_of(6), "code": "single" }]);
    let (status, body) = ingest(&served, OPERATOR_CREDENTIAL, "undup", false, rows).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = send(&served, &key, put, "/control/attributes", unique_code()).await;
    assert_eq!(status, 409, "{body}");
    let text = body["detail"].as_str().unwrap_or_default().to_string();
    assert!(
        !text.contains("dup-out") && numbers_in(&text).is_empty(),
        "{text}"
    );
}

/// **A page of a derived vocabulary's values answers a writer whose terms bound its writes one
/// count, of the keys bound**, and the operator `added` and `existing` beside it.
#[tokio::test]
async fn a_scoped_page_of_a_derived_vocabulary_answers_how_many_keys_are_bound() {
    let (served, key, _) = setup().await;
    let put = reqwest::Method::PUT;
    let (status, body) = send(
        &served,
        OPERATOR_CREDENTIAL,
        put,
        "/control/vocabularies/tags",
        json!({ "value_set": "open", "visibility": "derived", "width": "u8", "values": [{ "key": "a" }] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let page = json!({ "values": [{ "key": "a" }, { "key": "b" }] });
    let path = "/control/vocabularies/tags/values";
    let (status, body) = send(&served, &key, reqwest::Method::PATCH, path, page.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({ "name": "tags", "bound": 2 }));
    let page = json!({ "values": [{ "key": "b" }, { "key": "c" }] });
    let (status, body) = send(
        &served,
        OPERATOR_CREDENTIAL,
        reqwest::Method::PATCH,
        path,
        page,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        (&body["bound"], &body["added"], &body["existing"]),
        (&json!(2), &json!(1), &json!(1))
    );
}
