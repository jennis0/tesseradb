//! **`PUT /control/view_groups/{name}` and `PUT /control/views/{name}`** (`ingest.md` §1.3 and
//! §10 R9; decision 0136, track T6): a view group and a plain view are declared at a running
//! service, an identical redeclaration answers the one that exists, a differing one is refused, a
//! view is created under a runtime group, a runtime plain view takes rows at its first flush and
//! serves a viewport, and both declarations survive a restart and a fold.
//!
//! **The plain-view route is what R9 overturns.** `views.md` §7 ruled that a plain view is
//! declared when the corpus is built; decision 0134's wider rule is that anything a build can
//! create, live ingest can create.
//!
//! The roster half — a view created under a group the *build* declared — is
//! `tests/views_write.rs`; what this file pins is the two declaration routes and what a group
//! declared here can then do.

mod common;

use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_build::build;

const N: u64 = 20;

fn write_points(path: &Path, n: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut w =
        parquet::arrow::ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None)
            .unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// One plain view, `s0`, and no group at all: every group and every other view below is one the
/// running service declared.
fn build_fixture_bundle(dir: &Path) -> std::path::PathBuf {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    write_points(&points, N);
    write_pairs_n(&pairs, N);
    let out = dir.join("bundle");
    build(&build_args(
        &out,
        vec![view_args("s0", &points, AccessInput::relation(pairs))],
    ))
    .expect("fixture build should succeed");
    out
}

struct Served {
    server: TestServer,
    token: String,
    tmp: TempDir,
}

async fn serve() -> Served {
    let tmp = TempDir::new().unwrap();
    build_fixture_bundle(tmp.path());
    open(tmp).await
}

async fn open(tmp: TempDir) -> Served {
    let server = spawn_server(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let token = authorise(&server, &["0", "1"][..]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    Served { server, token, tmp }
}

/// **A fresh session** (`views.md` §6): the visible-view set is resolved once at authorise, so a
/// view declared since is a 404 to a session that predates it.
async fn reauthorise(served: &mut Served) {
    served.token = authorise(&served.server, &["0", "1"][..]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
}

async fn restart(served: Served) -> Served {
    let Served { server, tmp, .. } = served;
    server.shutdown().await;
    open(tmp).await
}

async fn put(served: &Served, path: &str, body: Value) -> (u16, Value) {
    let resp = served
        .server
        .client
        .put(served.server.control_url(path))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn declare_group(served: &Served, name: &str, body: Value) -> (u16, Value) {
    put(served, &format!("/control/view_groups/{name}"), body).await
}

async fn declare_view(served: &Served, name: &str, body: Value) -> (u16, Value) {
    put(served, &format!("/control/views/{name}"), body).await
}

fn frame() -> Value {
    json!({ "x": [0.0, 1000.0], "y": [0.0, 1000.0] })
}

/// The group the tests below declare: one frame, one metadata name, no gate.
fn quarter() -> Value {
    json!({
        "title": "Quarters",
        "extent": frame(),
        "point_visibility": { "default": "public" },
        "metadata": [{ "name": "label", "type": "text" }]
    })
}

/// A plain view: a frame and nothing else.
fn embedding() -> Value {
    json!({ "extent": frame(), "point_visibility": { "default": "public" } })
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

async fn view_ids(served: &Served) -> Vec<String> {
    meta(served).await["views"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect()
}

async fn group_names(served: &Served) -> Vec<String> {
    meta(served).await["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap().to_string())
        .collect()
}

fn batch(rows: &[(&str, f32, f32)]) -> Vec<u8> {
    let labels: Vec<&[&str]> = rows.iter().map(|_| &["0"][..]).collect();
    let access = access_lists(&labels);
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(arrow::array::BinaryArray::from_iter(
                rows.iter().map(|r| Some(r.0.as_bytes())),
            )),
            Arc::new(arrow::array::Float32Array::from_iter_values(
                rows.iter().map(|r| r.1),
            )),
            Arc::new(arrow::array::Float32Array::from_iter_values(
                rows.iter().map(|r| r.2),
            )),
            Arc::new(access),
        ],
    )
    .unwrap();
    let mut writer = arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

async fn ingest(served: &Served, batch_id: &str, view: &str, rows: &[(&str, f32, f32)]) -> u16 {
    served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", view)
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(batch(rows))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn flush(served: &Served) {
    // 120 s, the fold helper's patience below, rather than the 60 s the older files use: a tick
    // is 90 s by default and this box runs several test binaries at once, so the shorter deadline
    // fails on load rather than on an answer.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
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
                "the flush never published: {} rows buffered, {} flushes, {} failures, {} flushable",
                served.server.state.engine.buffered_items(),
                served.server.state.engine.write_executor_stats().flushes,
                served.server.state.engine.write_executor_stats().flush_failures,
                served.server.state.engine.write_executor_stats().flushable_items,
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        if served.server.state.engine.buffered_items() == 0 {
            break;
        }
    }
}

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
    assert_eq!(resp.status(), 202);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let stats = served.server.state.engine.write_executor_stats();
        assert_eq!(stats.fold_failures, 0, "the fold failed rather than publishing");
        if stats.folds > before {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The rows a viewport answers for `view`, by `tessera_id`.
async fn points(served: &Served, view: &str) -> Vec<u64> {
    let resp = served
        .server
        .client
        .post(served.server.viewer_url("/v1/viewport"))
        .bearer_auth(&served.token)
        .json(&json!({
            "view": view, "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "a declared view answers: {view}"
    );
    let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
    points.into_iter().map(|(id, _)| id).collect()
}

// ---------------------------------------------------------------------------------------------

/// **The group route's answers** (contracts §3.4): `201` for a new group, `200` for an identical
/// redeclaration, `409` for a held name under another identity, `422` for a declaration the rules
/// refuse, and `404` for a `members` group naming a group this deployment does not carry.
#[tokio::test]
async fn the_group_route_declares_answers_redeclarations_and_refuses_what_the_rules_refuse() {
    let mut served = serve().await;
    assert!(group_names(&served).await.is_empty());

    let (status, body) = declare_group(&served, "quarter", quarter()).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        without_publication(body),
        json!({ "group": "quarter", "existing": false })
    );

    let (status, body) = declare_group(&served, "quarter", quarter()).await;
    assert_eq!(status, 200, "identical: the group that exists: {body}");
    assert_eq!(
        without_publication(body),
        json!({ "group": "quarter", "existing": true })
    );

    let mut moved = quarter();
    moved["extent"] = json!({ "x": [0.0, 500.0], "y": [0.0, 1000.0] });
    let (status, body) = declare_group(&served, "quarter", moved).await;
    assert_eq!(status, 409, "a held name at another frame: {body}");
    assert_eq!(body["error"], "conflict");

    // A sharing group, and the two ways of getting one wrong.
    let (status, body) = declare_group(
        &served,
        "quarter_map",
        json!({ "extent": frame(), "members": "quarter" }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let (status, body) = declare_group(
        &served,
        "chained",
        json!({ "extent": frame(), "members": "quarter_map" }),
    )
    .await;
    assert_eq!(status, 422, "a chain is refused: {body}");
    assert!(body["detail"].as_str().unwrap().contains("Chains"), "{body}");
    let (status, body) = declare_group(
        &served,
        "orphan",
        json!({ "extent": frame(), "members": "nothing" }),
    )
    .await;
    assert_eq!(status, 404, "an unknown owner: {body}");

    for bad in [
        json!({ "extent": { "x": [0.0, 0.0], "y": [0.0, 1000.0] } }),
        json!({ "extent": frame(), "projection": "nonsense" }),
        json!({ "extent": frame(), "metadata": [{ "name": "key", "type": "text" }] }),
        json!({ "extent": frame(), "visibility": [] }),
    ] {
        let (status, body) = declare_group(&served, "other", bad.clone()).await;
        assert_eq!(status, 422, "{bad}: {body}");
        assert!(body["detail"].is_string(), "{bad}: {body}");
    }

    // A view and a group are one namespace: `s0` is the build's plain view.
    let (status, body) = declare_group(&served, "s0", quarter()).await;
    assert_eq!(status, 422, "{body}");

    reauthorise(&mut served).await;
    assert_eq!(group_names(&served).await, ["quarter", "quarter_map"]);
}

/// **A group created at runtime accepts a view under it** — the roster route, unchanged, over a
/// group no build declared — and the view is empty until its first flush, which is what a view of
/// a build-declared group already gets (`views.md` §3.2).
#[tokio::test]
async fn a_group_created_at_runtime_accepts_a_view_under_it() {
    let mut served = serve().await;
    assert_eq!(declare_group(&served, "quarter", quarter()).await.0, 201);

    let (status, body) = put(
        &served,
        "/control/views/quarter/2026-Q1",
        json!({ "metadata": { "label": "Q1 2026" } }),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    // A record the group's declaration refuses is refused here too: the metadata is the group's.
    let (status, body) = put(
        &served,
        "/control/views/quarter/2026-Q2",
        json!({ "metadata": { "nothing": "x" } }),
    )
    .await;
    assert_eq!(status, 422, "{body}");

    reauthorise(&mut served).await;
    let ids = view_ids(&served).await;
    assert!(
        ids.contains(&"quarter:2026-Q1".to_string()),
        "the created view is served: {ids:?}"
    );
    assert!(
        points(&served, "quarter:2026-Q1").await.is_empty(),
        "it starts with an empty row space"
    );

    assert_eq!(
        ingest(&served, "q1", "quarter:2026-Q1", &[("a", 100.0, 100.0)]).await,
        200
    );
    flush(&served).await;
    reauthorise(&mut served).await;
    assert_eq!(
        points(&served, "quarter:2026-Q1").await.len(),
        1,
        "and takes rows at its first flush"
    );
}

/// **A roster integer that does not fit its declared width is refused** when a view is created at
/// a running service, as a build refuses it in a roster table; one that fits is accepted and
/// served as written.
#[tokio::test]
async fn a_roster_integer_past_its_declared_width_is_refused_at_a_create() {
    let mut served = serve().await;
    let group = json!({
        "extent": frame(),
        "point_visibility": { "default": "public" },
        "metadata": [{ "name": "tier", "type": "u8" }]
    });
    assert_eq!(declare_group(&served, "tiers", group).await.0, 201);

    let (status, body) = put(
        &served,
        "/control/views/tiers/wide",
        json!({ "metadata": { "tier": 300 } }),
    )
    .await;
    assert_eq!(status, 422, "{body}");

    let (status, body) = put(
        &served,
        "/control/views/tiers/fits",
        json!({ "metadata": { "tier": 255 } }),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    reauthorise(&mut served).await;
    let document = meta(&served).await;
    let views = document["views"].as_array().unwrap();
    assert!(
        !views.iter().any(|v| v["id"] == "tiers:wide"),
        "the refused view was not created"
    );
    let fits = views
        .iter()
        .find(|v| v["id"] == "tiers:fits")
        .expect("the accepted view is served");
    assert_eq!(fits["metadata"]["tier"]["value"], 255, "{fits}");
}

/// **A plain view created at runtime takes rows at its first flush and serves a viewport**
/// (decision 0136, R9). Its answers are the group route's, and it shares a namespace with the
/// groups.
#[tokio::test]
async fn a_plain_view_created_at_runtime_takes_rows_at_its_first_flush() {
    let mut served = serve().await;
    let (status, body) = declare_view(&served, "embedding", embedding()).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        without_publication(body),
        json!({ "view": "embedding", "existing": false })
    );

    assert_eq!(
        declare_view(&served, "embedding", embedding()).await.0,
        200,
        "an identical redeclaration answers the view that exists"
    );
    let mut moved = embedding();
    moved["extent"] = json!({ "x": [0.0, 10.0], "y": [0.0, 10.0] });
    assert_eq!(
        declare_view(&served, "embedding", moved).await.0,
        409,
        "a held name at another frame"
    );
    assert_eq!(
        declare_view(&served, "s0", embedding()).await.0,
        200,
        "the build's own view is a held name, and a declaration matching it is the no-op any \
         identical redeclaration is"
    );
    let mut other_frame = embedding();
    other_frame["extent"] = json!({ "x": [0.0, 10.0], "y": [0.0, 10.0] });
    assert_eq!(
        declare_view(&served, "s0", other_frame).await.0,
        409,
        "and one differing from it is refused, a frame being immutable for a view's life"
    );
    assert_eq!(declare_group(&served, "quarter", quarter()).await.0, 201);
    let (status, body) = declare_view(&served, "quarter", embedding()).await;
    assert_eq!(status, 422, "a group's name is not free for a view: {body}");

    reauthorise(&mut served).await;
    let ids = view_ids(&served).await;
    assert!(ids.contains(&"embedding".to_string()), "{ids:?}");
    assert!(
        points(&served, "embedding").await.is_empty(),
        "it starts with an empty row space"
    );

    assert_eq!(
        ingest(
            &served,
            "into-embedding",
            "embedding",
            &[("e1", 100.0, 100.0), ("e2", 200.0, 300.0)]
        )
        .await,
        200
    );
    flush(&served).await;
    reauthorise(&mut served).await;
    assert_eq!(points(&served, "embedding").await.len(), 2);
    assert_eq!(
        points(&served, "s0").await.len(),
        N as usize,
        "and the build's view is untouched"
    );
}

/// **A gate is one label or a list** (decision 0132), on both routes, and a label the plugin
/// cannot read is refused rather than stored as a gate nobody could satisfy.
#[tokio::test]
async fn a_gate_is_one_label_or_a_list_on_both_routes() {
    let served = serve().await;

    let mut one = embedding();
    one["visibility"] = json!("0");
    assert_eq!(declare_view(&served, "gated_one", one).await.0, 201);

    let mut many = embedding();
    many["visibility"] = json!(["0", "1"]);
    assert_eq!(declare_view(&served, "gated_many", many).await.0, 201);

    let mut group_gate = quarter();
    group_gate["visibility"] = json!(["0", "1"]);
    assert_eq!(declare_group(&served, "gated_group", group_gate).await.0, 201);

    let mut public_beside = embedding();
    public_beside["visibility"] = json!(["public", "0"]);
    let (status, body) = declare_view(&served, "bad_gate", public_beside).await;
    assert_eq!(status, 422, "{body}");
    assert!(body["detail"].as_str().unwrap().contains("public"), "{body}");

    let mut empty_element = embedding();
    empty_element["visibility"] = json!(["0", ""]);
    let (status, body) = declare_view(&served, "bad_gate", empty_element).await;
    assert_eq!(status, 422, "{body}");

    // The stored gate is the list as written, so a view gated on a label this session holds is
    // served and one gated on a label it does not is a 404.
    let token = authorise(&served.server, &["0"][..]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = served
        .server
        .client
        .get(served.server.viewer_url("/v1/meta"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    let meta: Value = resp.json().await.unwrap();
    let ids: Vec<&str> = meta["views"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"gated_one"), "{ids:?}");
    assert!(
        ids.contains(&"gated_many"),
        "a list gate is satisfied where the principal holds one of its labels: {ids:?}"
    );

    // **And the half that matters**: a principal holding none of a gate's terms cannot see the
    // view exists, and a request naming it is the 404 an unknown name is (`views.md` §6). The
    // fixture's principal `1` holds term `1` and not term `0`.
    let outside = authorise(&served.server, &["1"][..]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = served
        .server
        .client
        .get(served.server.viewer_url("/v1/meta"))
        .bearer_auth(&outside)
        .send()
        .await
        .unwrap();
    let meta: Value = resp.json().await.unwrap();
    let ids: Vec<&str> = meta["views"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert!(
        !ids.contains(&"gated_one"),
        "a gate-failed view is absent from /v1/meta: {ids:?}"
    );
    assert!(
        ids.contains(&"gated_many"),
        "and one whose list names a term this principal does hold is not: {ids:?}"
    );
    let resp = served
        .server
        .client
        .post(served.server.viewer_url("/v1/viewport"))
        .bearer_auth(&outside)
        .json(&json!({
            "view": "gated_one", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "a request naming a gate-failed view is the 404 an unknown name is"
    );
}

/// **`point_visibility.default` goes through the plugin**, on the gate's rule: it is given to
/// every point that carries no label of its own (decision 0133), so a label the plugin cannot
/// read would put those points in no principal's mask, and the refusal belongs at the
/// declaration rather than at every batch. `inherited` and the empty string are the build's own
/// two refusals, transcribed.
#[tokio::test]
async fn a_point_default_is_measured_against_the_plugin_on_both_routes() {
    let served = serve().await;

    // The plugin arm is exercised by no case here: this fixture's plugin reads every non-empty
    // label as a term, so a label it *cannot* read has no spelling. What the two cases below
    // cover is the pair the build refuses too, and the plugin call itself is the one
    // `check_visibility` makes, on the same descriptors.
    for default in ["", "inherited"] {
        let mut view = embedding();
        view["point_visibility"] = json!({ "default": default });
        let (status, body) = declare_view(&served, "bad_default", view).await;
        assert_eq!(status, 422, "{default:?}: {body}");

        let mut group = quarter();
        group["point_visibility"] = json!({ "default": default });
        let (status, body) = declare_group(&served, "bad_default", group).await;
        assert_eq!(status, 422, "{default:?} on a group: {body}");
    }

    // A label the plugin reads, and `public`, are both accepted.
    let mut labelled = embedding();
    labelled["point_visibility"] = json!({ "default": "0" });
    assert_eq!(declare_view(&served, "labelled", labelled).await.0, 201);
    assert_eq!(declare_view(&served, "public_default", embedding()).await.0, 201);
}

/// **Both declarations survive a restart and a fold** (`ingest.md` §1.3): from the log alone
/// before any publication, from the segments manifest after one, and from `MANIFEST.json` after
/// the fold that writes them there. A view created under a runtime group survives with it, which
/// is what the manifest's merge order exists for.
#[tokio::test]
async fn the_declarations_survive_a_restart_and_a_fold() {
    let served = serve().await;
    assert_eq!(declare_group(&served, "quarter", quarter()).await.0, 201);
    assert_eq!(declare_view(&served, "embedding", embedding()).await.0, 201);
    assert_eq!(
        put(
            &served,
            "/control/views/quarter/2026-Q1",
            json!({ "metadata": { "label": "Q1 2026" } })
        )
        .await
        .0,
        201
    );

    // Replayed from the log, nothing having been published yet.
    let mut served = restart(served).await;
    reauthorise(&mut served).await;
    assert_eq!(group_names(&served).await, ["quarter"]);
    let ids = view_ids(&served).await;
    assert!(ids.contains(&"embedding".to_string()), "{ids:?}");
    assert!(
        ids.contains(&"quarter:2026-Q1".to_string()),
        "a view created under a runtime group comes back with it: {ids:?}"
    );
    assert_eq!(
        declare_group(&served, "quarter", quarter()).await.0,
        200,
        "the replayed group is the one a redeclaration meets"
    );

    // Published into a segments manifest, then replayed from it.
    assert_eq!(
        ingest(&served, "rows", "embedding", &[("e1", 100.0, 100.0)]).await,
        200
    );
    flush(&served).await;
    let mut served = restart(served).await;
    reauthorise(&mut served).await;
    assert_eq!(group_names(&served).await, ["quarter"]);
    assert_eq!(points(&served, "embedding").await.len(), 1);

    // Folded into `MANIFEST.json`, then replayed from it.
    fold(&served).await;
    let mut served = restart(served).await;
    reauthorise(&mut served).await;
    assert_eq!(group_names(&served).await, ["quarter"]);
    let ids = view_ids(&served).await;
    assert!(ids.contains(&"embedding".to_string()), "{ids:?}");
    assert!(ids.contains(&"quarter:2026-Q1".to_string()), "{ids:?}");
    assert_eq!(
        points(&served, "embedding").await.len(),
        1,
        "and the rows it took are still served"
    );
    assert_eq!(
        declare_view(&served, "embedding", embedding()).await.0,
        200,
        "the folded view is still the one a redeclaration meets"
    );
}

/// A declaration's answer with `publication` taken out, so a whole-object comparison stays a
/// whole-object comparison. The number is a running count and a test cannot name it, but its
/// absence would be a route that stopped telling a caller when its declaration becomes visible
/// (contracts §3.4), so this asserts it was there.
fn without_publication(mut body: Value) -> Value {
    assert!(
        body.as_object_mut()
            .expect("the answer is an object")
            .remove("publication")
            .is_some(),
        "every write acknowledgement carries a publication number: {body}"
    );
    body
}
