//! A writer holding `write` and not `write-all` writes only within its terms. The fixture's items
//! hold the terms [`terms_of`] gives them; the writer holds `1`, so its scope is every third item.

mod common;

use std::path::Path;

use arrow::array::{StringArray, UInt64Array};
use common::*;
use mosaica_catalogue::{Grantee, Permission};
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

/// A response's status and body, without the publication it was answered at.
async fn answered(resp: reqwest::Response) -> (u16, Value) {
    let status = resp.status().as_u16();
    let mut body: Value = resp.json().await.unwrap_or(Value::Null);
    if let Some(object) = body.as_object_mut() {
        object.remove("publication");
    }
    (status, body)
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

/// One batch into `view` under `query`, sent with `key`.
async fn ingest_into(
    served: &Served,
    key: &str,
    view: &str,
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
        .header("x-mosaica-view", view)
        .json(&rows)
        .send()
        .await
        .unwrap();
    answered(resp).await
}

/// One batch into `s0`, answered once its rows are served.
async fn ingest(
    served: &Served,
    key: &str,
    batch_id: &str,
    strict: bool,
    rows: Value,
) -> (u16, Value) {
    let query = format!("strict={strict}&wait=visible");
    ingest_into(served, key, "s0", batch_id, &query, rows).await
}

async fn changes(served: &Served, key: &str, strict: bool, changes: Value) -> (u16, Value) {
    let path = format!("/control/changes?strict={strict}&wait=visible");
    send(served, key, reqwest::Method::POST, &path, changes).await
}

/// How many items a reader holding `token` is served under `filters`.
async fn served_where(served: &Served, token: &str, filters: Value) -> usize {
    let resp = served
        .server
        .client
        .post(served.server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
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

/// Whether a reader holding both terms is served the built item `e` with its built values.
async fn served_as_built(served: &Served, e: u64) -> bool {
    let gid = json!({ "gid": { "eq": gid_of(e).to_string() } });
    let code = json!({ "code": { "eq": format!("c{e}") } });
    served_where(served, &served.token, gid).await == 1
        && served_where(served, &served.token, code).await == 1
}

/// The fixture served, the writer's key, and the `mosaica_id` of [`DELETED`], which the operator
/// has deleted.
async fn setup() -> (Served, String, String) {
    let served = Served::build(fixture).await;
    let key = writer_key(&served.server, WRITER, Permission::Write, &["1"]);
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

/// The writer's grant of `write-all`.
fn grant_write_all(served: &Served) {
    served
        .server
        .state
        .catalogue
        .grant_permission(Grantee::Principal(WRITER), Permission::WriteAll)
        .unwrap();
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
            // A refusal may echo the `mosaica_id` the caller sent, and nothing else of it.
            assert_eq!(
                spelt(&a, &outside, "ID"),
                spelt(&b, &deleted, "ID"),
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

/// **A strict batch refused for a value held outside names no item**: its `409` says the value is
/// held and nothing of the item outside the writer's terms that holds it.
#[tokio::test]
async fn a_strict_batch_refused_for_a_value_held_outside_names_no_item() {
    let (served, key, _) = setup().await;
    let outside = mosaica_id_of(&served.server, OUTSIDE).to_string();
    let row = json!([{ "gid": gid_of(OUTSIDE), "x": 500.5, "y": 500.5 }]);
    let (status, body) = ingest(&served, &key, "insert-strict", true, row).await;
    assert_eq!(status, 409, "{body}");
    assert!(
        !body.to_string().contains(&outside),
        "the refusal names no item: {body}"
    );
}

/// **Whether a value is held outside is decided where the batch applies**: a row creating an item
/// with a value an item outside the writer's terms holds, whose holder the operator deletes after
/// the batch arrived and before it applies, creates the item.
#[tokio::test]
async fn a_value_whose_holder_outside_goes_before_the_batch_applies_is_given() {
    let (served, key, _) = setup().await;
    let outside = mosaica_id_of(&served.server, OUTSIDE).to_string();
    let row = json!([{ "gid": gid_of(OUTSIDE), "x": 500.5, "y": 500.5 }]);
    let inserting = ingest(&served, &key, "late", false, row);
    let deleting = async {
        let deleted = json!([{ "op": "delete", "match": { "mosaica_id": outside } }]);
        let (status, body) = changes(&served, OPERATOR_CREDENTIAL, false, deleted).await;
        assert_eq!((status, &body["accepted"]), (200, &json!(1)), "{body}");
    };
    let (status, body) = while_write_held(&served.server.state.engine, inserting, deleting).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        (&body["created"], &body["refused"]),
        (&json!(1), &json!([])),
        "{body}"
    );
    assert!(body["mosaica_ids"][0].is_string(), "{body}");
}

/// **An item inside the writer's terms keeps its label where a row would relabel it with one the
/// writer does not hold**: a reader holding `1` alone is still served it.
#[tokio::test]
async fn a_row_relabelling_an_item_with_a_label_the_writer_does_not_hold_leaves_it_served() {
    let (served, key, _) = setup().await;
    let inside = mosaica_id_of(&served.server, INSIDE).to_string();
    let rows = json!([{ "mosaica_id": inside, "access": ["0"] }]);
    let (status, body) = ingest(&served, &key, "labels", false, rows).await;
    assert_eq!(status, 200, "{body}");
    let only_one = token_for(&served.server, &["1"]).await;
    let gid = json!({ "gid": { "eq": gid_of(INSIDE).to_string() } });
    assert_eq!(served_where(&served, &only_one, gid).await, 1);
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
    let gid = json!({ "gid": { "eq": gid_of(OUTSIDE).to_string() } });
    assert_eq!(served_where(&served, &served.token, gid).await, 0);

    register(&served.server, flat_layer(LAYER)).await;
    let path = format!("/control/layers/{LAYER}/artifacts");
    let body = json!({ "artifacts": [{ "key": "all-but-one", "excluding": members([INSIDE]) }] });
    let (status, body) = send(&served, &key, reqwest::Method::PUT, &path, body).await;
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
    grant_write_all(&served);
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

/// `answer` with every `from` in its body spelled `to`: two answers naming different things are
/// compared as answers.
fn spelt(answer: &(u16, Value), from: &str, to: &str) -> (u16, String) {
    (answer.0, answer.1.to_string().replace(from, to))
}

const LABELLED: &str = "labelled";

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

/// `artifacts` published into `layer` by the operator, and their `mosaica_id`s, the last deleted.
async fn published(served: &Served, layer: &str, artifacts: Value) -> Vec<String> {
    let body = json!({ "artifacts": artifacts });
    let (status, body) = publish(&served.server, layer, "?wait=visible", body).await;
    assert_eq!(status, 201, "{body}");
    let ids = artifact_ids(&body);
    let gone = ids.last().unwrap();
    let deleted = json!([{ "op": "delete", "match": { "mosaica_id": gone } }]);
    let (status, body) = changes(served, OPERATOR_CREDENTIAL, false, deleted).await;
    assert_eq!((status, &body["accepted"]), (200, &json!(1)), "{body}");
    ids
}

/// The labelled layer, with `secret` labelled `0`, outside the writer's terms, `reached` labelled
/// `1`, inside them, and `gone`, which the operator deleted: the fixture, the writer's key, and
/// the `mosaica_id`s of `secret` and `gone`.
async fn labelled_setup() -> (Served, String, String, String) {
    let (served, key, _) = setup().await;
    register(&served.server, labelled_layer()).await;
    let ids = published(
        &served,
        LABELLED,
        json!([
            { "key": "secret", "members": members([INSIDE]), "access": ["0"] },
            { "key": "reached", "members": members([INSIDE]), "access": ["1"] },
            { "key": "gone", "members": members([INSIDE]), "access": ["1"] },
        ]),
    )
    .await;
    (served, key, ids[0].clone(), ids[2].clone())
}

/// The writes `key` sends naming `name` of `layer`, each answered as the same write naming a key
/// nobody holds; publishing under its key is refused as the key taken, and a change naming `id` is
/// answered as one naming `gone`.
#[allow(clippy::too_many_arguments)]
async fn named_by_nothing(
    served: &Served,
    key: &str,
    layer: &str,
    name: &str,
    id: &str,
    gone: &str,
    parts: &Value,
) {
    let path = format!("/control/layers/{layer}/artifacts");
    let with = |mut artifact: Value| {
        for (part, value) in parts.as_object().unwrap() {
            artifact[part] = value.clone();
        }
        json!({ "artifacts": [artifact] })
    };
    let (put, patch) = (reqwest::Method::PUT, reqwest::Method::PATCH);
    let writes = |named: &str| {
        vec![
            (
                patch.clone(),
                json!({ "artifacts": [{ "key": named, "members": members([6]) }] }),
            ),
            (
                patch.clone(),
                json!({ "artifacts": [{ "key": "reached", "parent": [named] }] }),
            ),
            (
                put.clone(),
                with(json!({ "key": "child", "parent": [named], "members": members([6]) })),
            ),
        ]
    };
    for ((method, a), (_, b)) in writes(name).into_iter().zip(writes("absent")) {
        let a = send(served, key, method.clone(), &path, a).await;
        let b = send(served, key, method.clone(), &path, b).await;
        assert!(a.0 >= 400, "{method} {name}: {a:?}");
        assert_eq!(spelt(&a, name, "K"), spelt(&b, "absent", "K"), "{method}");
    }
    let held = with(json!({ "key": name, "members": members([6]) }));
    let (status, body) = send(served, key, put, &path, held).await;
    assert_eq!(status, 409, "{body}");
    assert!(!body.to_string().contains(id), "{body}");
    for op in ["suppress", "delete"] {
        let a = changes(
            served,
            key,
            false,
            json!([{ "op": op, "match": { "mosaica_id": id } }]),
        )
        .await;
        let b = changes(
            served,
            key,
            false,
            json!([{ "op": op, "match": { "mosaica_id": gone } }]),
        )
        .await;
        assert_eq!(
            a.1["refused"],
            json!([{ "row": 0, "reason": "unknown_mosaica_id" }]),
            "{a:?}"
        );
        assert_eq!(spelt(&a, id, "ID"), spelt(&b, gone, "ID"));
    }
}

/// **An artifact outside the writer's terms is named by nothing**, on the writes
/// [`named_by_nothing`] sends, strict or not, and as the target of an attachment. A writer holding
/// `write-all` grows it.
#[tokio::test]
async fn an_artifact_outside_the_writers_terms_is_named_by_nothing_and_its_key_is_kept() {
    let (served, key, secret, gone) = labelled_setup().await;
    let path = format!("/control/layers/{LABELLED}/artifacts");
    let parts = json!({ "access": ["1"] });
    named_by_nothing(&served, &key, LABELLED, "secret", &secret, &gone, &parts).await;
    let (put, patch) = (reqwest::Method::PUT, reqwest::Method::PATCH);
    let grow = |name: &str| json!({ "artifacts": [{ "key": name, "members": members([6]) }] });
    let strict = format!("{path}?strict=true");
    let a = send(&served, &key, patch.clone(), &strict, grow("secret")).await;
    let b = send(&served, &key, patch.clone(), &strict, grow("absent")).await;
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
    let notes = "/control/layers/notes/artifacts";
    let a = send(&served, &key, put.clone(), notes, attached("secret")).await;
    let b = send(&served, &key, put, notes, attached("absent")).await;
    assert!(a.0 >= 400, "{a:?}");
    assert_eq!(spelt(&a, "secret", "K"), spelt(&b, "absent", "K"));

    grant_write_all(&served);
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

/// **A scoped write that waits on its grant's fragment past the wait budget is answered `429`** on
/// every route naming an artifact: a publication, a growth, an ingest naming one in a layer's
/// column and a change naming one, and none is written. Once the fragment is built, each is
/// written.
#[tokio::test]
async fn a_scoped_write_waiting_on_its_grants_fragment_is_answered_backpressure() {
    let (served, key, secret, _) = labelled_setup().await;
    register(&served.server, flat_layer(LAYER)).await;
    let body = json!({ "artifacts": [{ "key": "a", "members": members([INSIDE]) }] });
    let (status, body) = publish(&served.server, LAYER, "", body).await;
    assert_eq!(status, 201, "{body}");
    let path = format!("/control/layers/{LABELLED}/artifacts");
    let writes = async |batch: &str| {
        let (put, patch) = (reqwest::Method::PUT, reqwest::Method::PATCH);
        let fresh = json!({ "key": "fresh", "members": members([INSIDE]), "access": ["1"] });
        let publication = json!({ "artifacts": [fresh] });
        let growth = json!({ "artifacts": [{ "key": "reached", "members": members([6]) }] });
        let row = json!([{ "gid": 8_101, "x": 2.5, "y": 2.5, "access": ["1"], LAYER: "a" }]);
        let change = json!([{ "op": "suppress", "match": { "mosaica_id": secret } }]);
        [
            send(&served, &key, put, &path, publication).await.0,
            send(&served, &key, patch, &path, growth).await.0,
            ingest(&served, &key, batch, false, row).await.0,
            changes(&served, &key, false, change).await.0,
        ]
    };
    let engine = &served.server.state.engine;
    engine.set_fragments_building_for_test(true);
    assert_eq!(writes("building").await, [429; 4]);
    engine.set_fragments_building_for_test(false);
    assert_eq!(writes("built").await, [201, 200, 200, 200]);
}

/// Whether a reader holding the writer's terms is served the artifact `id` in `s0`.
async fn served_to_the_writers_terms(served: &Served, id: &str) -> bool {
    let token = token_for(&served.server, &["1"]).await;
    let resp = served
        .server
        .client
        .post(served.server.viewer_url(&format!("/v1/artifacts/{id}")))
        .bearer_auth(token)
        .json(&json!({ "view": "s0" }))
        .send()
        .await
        .unwrap();
    resp.status().as_u16() == 200
}

/// **An artifact is inside the writer's terms only where a reader holding them would be served
/// it**: one attached to an artifact outside them, one whose every content is generated from an
/// item outside them, and one on a layer requiring a visible member whose members are all outside
/// them are each named by nothing, on the writes [`named_by_nothing`] sends. Their neighbours,
/// served to such a reader, grow.
#[tokio::test]
async fn an_artifact_a_reader_of_the_writers_terms_is_not_served_is_named_by_nothing() {
    let (served, key, _) = setup().await;
    let nested = |name: &str| {
        let mut layer = labelled_layer();
        layer["name"] = json!(name);
        layer["artifact_visibility"] = json!({ "field": null, "default": "inherited" });
        layer
    };
    register(&served.server, labelled_layer()).await;
    published(
        &served,
        LABELLED,
        json!([
            { "key": "hidden", "members": members([INSIDE]), "access": ["0"] },
            { "key": "shown", "members": members([INSIDE]), "access": ["1"] },
            { "key": "gone", "members": members([INSIDE]), "access": ["1"] },
        ]),
    )
    .await;
    let mut notes = nested("notes");
    notes["depends_on"] = json!([LABELLED]);
    let on = |name: &str, topic: &str| {
        json!({ "key": name, "members": members([INSIDE]),
                "attached_to": { "layer": LABELLED, "level": 0, "key": topic } })
    };
    let mut described = nested("described");
    described["content"] = json!({
        "computed": [],
        "supplied": [{ "name": "name", "type": "text", "require_member_visibility": "all" }]
    });
    let content = |items: &[u64]| json!([{ "values": ["c"], "generated_from": members(items.iter().copied()) }]);
    let with = |name: &str, items: &[u64]| json!({ "key": name, "members": members([INSIDE]), "content": content(items) });
    let mut counted = nested("counted");
    counted["require_member_visibility"] = json!({ "count": 1 });
    let of = |name: &str, items: [u64; 2]| json!({ "key": name, "members": members(items) });
    let cases = [
        (
            notes,
            json!([
                on("outer", "hidden"),
                on("reached", "shown"),
                on("gone", "shown")
            ]),
            json!({}),
        ),
        (
            described,
            json!([
                with("outer", &[INSIDE, OUTSIDE]),
                with("reached", &[INSIDE, 6]),
                with("gone", &[INSIDE]),
            ]),
            json!({ "content": content(&[INSIDE]) }),
        ),
        (
            counted,
            json!([
                of("outer", [OUTSIDE, 4]),
                of("reached", [OUTSIDE, INSIDE]),
                of("gone", [INSIDE, 6]),
            ]),
            json!({}),
        ),
    ];
    for (layer, artifacts, parts) in cases {
        let name = layer["name"].as_str().unwrap().to_string();
        register(&served.server, layer).await;
        let ids = published(&served, &name, artifacts).await;
        let (outer, reached, gone) = (&ids[0], &ids[1], &ids[2]);
        assert!(!served_to_the_writers_terms(&served, outer).await, "{name}");
        assert!(
            served_to_the_writers_terms(&served, reached).await,
            "{name}"
        );

        named_by_nothing(&served, &key, &name, "outer", outer, gone, &parts).await;
        let grow = json!({ "artifacts": [{ "key": "reached", "members": members([6]) }] });
        let path = format!("/control/layers/{name}/artifacts");
        let (status, body) = send(&served, &key, reqwest::Method::PATCH, &path, grow).await;
        assert_eq!(
            (status, &body["artifacts"][0]["joined"]),
            (200, &json!(1)),
            "{name}: {body}"
        );
    }
}

/// **A layer column naming an artifact outside the writer's terms names nothing**: on a closed
/// layer it is answered as a key no artifact holds, and on an open one it is refused as the key
/// taken, naming no artifact, where a key nobody holds mints.
#[tokio::test]
async fn an_ingest_column_naming_an_artifact_outside_the_writers_terms_names_nothing() {
    let (served, key, _) = setup().await;
    let mut closed = flat_layer("counted");
    closed["require_member_visibility"] = json!({ "count": 1 });
    let mut open = closed.clone();
    open["name"] = json!("minted");
    open["value_set"] = json!("open");
    for layer in [closed, open] {
        let name = layer["name"].as_str().unwrap().to_string();
        register(&served.server, layer).await;
        let body =
            json!({ "artifacts": [{ "key": "outsiders", "members": members([OUTSIDE, 4]) }] });
        let (status, body) = publish(&served.server, &name, "", body).await;
        assert_eq!(status, 201, "{body}");
    }
    let row = |layer: &str, artifact: &str| json!([{ "gid": 8_001, "x": 2.5, "y": 2.5, "access": ["1"], layer: artifact }]);
    let a = ingest(
        &served,
        &key,
        "closed-outside",
        false,
        row("counted", "outsiders"),
    )
    .await;
    let b = ingest(
        &served,
        &key,
        "closed-absent",
        false,
        row("counted", "absent"),
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
        wait_until(
            "the work is queued",
            std::time::Duration::from_secs(30),
            async || engine.work_enqueued_for_test() >= n,
        )
        .await;
    };
    engine.set_work_pass_paused_for_test(true);
    let enqueued = engine.work_enqueued_for_test();
    let row = json!([{ "gid": gid, "x": 2.5, "y": 2.5, "access": ["1"], "minted": artifact }]);
    let ingesting = ingest(served, key, batch, false, row);
    let publishing = async {
        queued(enqueued + 1).await;
        let body = json!({ "artifacts": [{ "key": artifact, "members": members([OUTSIDE, 4]) }] });
        publish(&served.server, "minted", "", body).await
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
    let gid = json!({ "gid": { "eq": "8002" } });
    assert_eq!(served_where(&served, &served.token, gid).await, 0);

    grant_write_all(&served);
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

/// A publication and a growth into [`HIDDEN`], sent with `key`, each answered as the same write
/// into `never`, which nobody declared, and naming nothing out of reach.
async fn probe_out_of_reach(served: &Served, key: &str) {
    let artifacts = json!({ "artifacts": [{ "key": "a", "members": members([INSIDE]) }] });
    for method in [reqwest::Method::PUT, reqwest::Method::PATCH] {
        let path = |name: &str| format!("/control/layers/{name}/artifacts");
        let a = send(
            served,
            key,
            method.clone(),
            &path(HIDDEN),
            artifacts.clone(),
        )
        .await;
        let b = send(
            served,
            key,
            method.clone(),
            &path("never"),
            artifacts.clone(),
        )
        .await;
        assert!(a.0 >= 400, "{method} {HIDDEN}: {a:?}");
        assert_eq!(
            spelt(&a, HIDDEN, "NAME"),
            spelt(&b, "never", "NAME"),
            "{method}"
        );
        for other in [HIDDEN, SECRET_VIEW, GROUP] {
            assert!(
                !b.1.to_string().contains(other),
                "{method} names {other}: {b:?}"
            );
        }
    }
}

/// **A layer or a view out of the writer's reach is unknown to it**: ingesting into the view,
/// publishing into the layer or naming it in a column, and naming the layer by its `mosaica_id`
/// are each answered as for a name nobody declared, and no refusal lists it.
#[tokio::test]
async fn a_layer_or_view_out_of_the_writers_reach_is_unknown() {
    let (served, key, hidden) = gated_setup().await;

    // Ingest names a view by its header and a layer by its column.
    let ingest_to = |view: &'static str, column: &'static str| {
        let row = json!([{ "gid": 7_001, "x": 1.5, "y": 1.5, column: "a" }]);
        let batch = format!("{view}-{column}");
        let (served, key) = (&served, key.clone());
        async move { ingest_into(served, &key, view, &batch, "", row).await }
    };
    let a = ingest_to(SECRET_VIEW, "code").await;
    let b = ingest_to("never", "code").await;
    assert_eq!(spelt(&a, SECRET_VIEW, "NAME"), spelt(&b, "never", "NAME"));
    let a = ingest_to("s0", HIDDEN).await;
    let b = ingest_to("s0", "never").await;
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

    probe_out_of_reach(&served, &key).await;
}

/// **A shape's report names only the views a reader holding the writer's terms is shown**: a
/// publication into a layer drawn on a view out of the writer's reach reports the shape on the
/// views `/v1/meta` lists for such a reader, and the operator's on every one.
#[tokio::test]
async fn a_shape_report_names_only_the_views_the_writers_terms_reach() {
    let (served, key, _) = gated_setup().await;
    let mut layer = flat_layer("boxes");
    layer["views"] = json!(["s0", SECRET_VIEW]);
    layer["membership"] = json!("spatial");
    layer["shape"] = json!({ "kind": "bbox" });
    register(&served.server, layer).await;
    let path = "/control/layers/boxes/artifacts";
    let reported = |body: &Value| -> Vec<Value> {
        body["shapes"][0]["views"]
            .as_array()
            .unwrap()
            .iter()
            .map(|report| report["view"].clone())
            .collect()
    };
    let shape = |key: &str| json!({ "artifacts": [{ "key": key, "members": {}, "bbox": [0.0, 0.0, 10.0, 10.0] }] });
    let (status, body) = send(&served, &key, reqwest::Method::PUT, path, shape("theirs")).await;
    assert_eq!(status, 201, "{body}");
    let shown: Vec<Value> = meta(&served.server, &["1"]).await["views"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].clone())
        .collect();
    assert_eq!(reported(&body), vec![json!("s0")], "{body}");
    assert!(shown.contains(&json!("s0")) && !shown.contains(&json!(SECRET_VIEW)));
    let (status, body) = publish(&served.server, "boxes", "", shape("ours")).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        reported(&body),
        vec![json!("s0"), json!(SECRET_VIEW)],
        "{body}"
    );
}

/// **Across a restart** the writer still names nothing outside its terms, a layer out of its reach
/// stays unknown to it, an artifact outside its terms stays named by nothing, and a batch it sent
/// before is answered the receipt it was given, its refusals' reasons included.
#[tokio::test]
async fn what_is_out_of_the_writers_reach_stays_so_across_a_restart() {
    let (served, key, hidden) = gated_setup().await;
    register(&served.server, labelled_layer()).await;
    let artifact = json!({ "key": "secret", "members": members([INSIDE]), "access": ["0"] });
    let body = json!({ "artifacts": [artifact] });
    let (status, body) = publish(&served.server, LABELLED, "", body).await;
    assert_eq!(status, 201, "{body}");
    let rows = json!([
        { "gid": gid_of(OUTSIDE), "x": 500.5, "y": 500.5 },
        { "gid": 5_001, "x": 400.5, "y": 400.5, "access": ["0"] },
        { "gid": 5_002, "x": 401.5, "y": 401.5, "access": ["1"] },
    ]);
    let first = ingest(&served, &key, "before", false, rows.clone()).await;
    assert_eq!(first.0, 200, "{first:?}");
    assert_eq!(
        first.1["refused"],
        json!([{ "row": 0, "reason": "value_held" }, { "row": 1, "reason": "label_not_held" }])
    );
    let outside = mosaica_id_of(&served.server, OUTSIDE).to_string();

    let served = served.restart().await;
    let replayed = ingest(&served, &key, "before", false, rows).await;
    assert_eq!(replayed.1["replayed"], true, "{replayed:?}");
    assert_eq!(replayed.1["refused"], first.1["refused"]);
    assert_eq!(replayed.1["mosaica_ids"], first.1["mosaica_ids"]);

    for named in [&outside, &hidden] {
        let named = json!([{ "op": "suppress", "match": { "mosaica_id": named } }]);
        let (_, body) = changes(&served, &key, false, named).await;
        assert_eq!(
            body["refused"],
            json!([{ "row": 0, "reason": "unknown_mosaica_id" }]),
            "{body}"
        );
    }
    let row = json!([{ "gid": gid_of(OUTSIDE), "x": 500.5, "y": 500.5 }]);
    let (_, again) = ingest(&served, &key, "after", false, row).await;
    assert_eq!(
        again["refused"],
        json!([{ "row": 0, "reason": "value_held" }])
    );
    assert!(served_as_built(&served, OUTSIDE).await);

    let path = format!("/control/layers/{LABELLED}/artifacts");
    let grow = |name: &str| json!({ "artifacts": [{ "key": name, "members": members([6]) }] });
    let a = send(&served, &key, reqwest::Method::PATCH, &path, grow("secret")).await;
    let b = send(&served, &key, reqwest::Method::PATCH, &path, grow("absent")).await;
    assert!(a.0 >= 400, "{a:?}");
    assert_eq!(spelt(&a, "secret", "K"), spelt(&b, "absent", "K"));
    probe_out_of_reach(&served, &key).await;
}

/// **A page of a derived vocabulary's values answers a writer whose terms bound its writes its
/// name alone**, and the operator `added` and `existing` beside it.
#[tokio::test]
async fn a_scoped_page_of_a_derived_vocabulary_answers_its_name_alone() {
    let (served, key, _) = setup().await;
    let declared = json!({
        "value_set": "open", "visibility": "derived", "width": "u8", "values": [{ "key": "a" }]
    });
    let (status, body) = send(
        &served,
        OPERATOR_CREDENTIAL,
        reqwest::Method::PUT,
        "/control/vocabularies/tags",
        declared,
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let page = json!({ "values": [{ "key": "a" }, { "key": "b" }] });
    let path = "/control/vocabularies/tags/values";
    let (status, body) = send(&served, &key, reqwest::Method::PATCH, path, page).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({ "name": "tags" }));
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
    assert_eq!((&body["added"], &body["existing"]), (&json!(1), &json!(1)));
}
