//! **A generating set is projected over the whole row space** (`annotation-write-cycle.md` §4.1;
//! issue #150, owner ruling (a) of 2026-09-18).
//!
//! A content gated `require_member_visibility = "all"` is served to a principal who holds every
//! member of its generating set and withheld from one who lacks a single member — whether those
//! members were read by the build, arrived through `/control/ingest` and were flushed, or are a
//! mixture. A member still in the commit buffer has no row, so the set is short and the content is
//! withheld from everyone until the flush that gives it one.
//!
//! The same answers hold through a growth, a merge, a fold and a restart, and a suppressed member
//! withholds the content from everyone until it is unsuppressed.

mod common;

use std::sync::Arc;

use arrow::array::{BinaryArray, Float32Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use common::*;
use serde_json::json;
use tempfile::TempDir;
use tessera_engine::EngineConfig;

const TOPICS: &str = "topics/generating";

/// A built item's member address.
fn member(source_id: u64) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(external_id_of(source_id))
}

/// An ingested item's member address. Its external id is the caller's own bytes.
fn ingested(name: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(name.as_bytes())
}

/// An Arrow ingest batch whose rows may carry **several** access labels each, which is what lets
/// one ingested item be visible to both principals and another to one of them.
fn ingest_batch(rows: &[(&str, f32, f32, &[&str])]) -> Vec<u8> {
    let access = access_lists(&rows.iter().map(|(_, _, _, a)| *a).collect::<Vec<_>>());
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter_values(
                rows.iter().map(|(id, _, _, _)| id.as_bytes()),
            )),
            Arc::new(Float32Array::from_iter_values(
                rows.iter().map(|(_, x, _, _)| *x),
            )),
            Arc::new(Float32Array::from_iter_values(
                rows.iter().map(|(_, _, y, _)| *y),
            )),
            Arc::new(access),
        ],
    )
    .unwrap();
    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

async fn open(tmp: &TempDir) -> TestServer {
    spawn_server(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await
}

async fn serve(tmp: &TempDir) -> TestServer {
    build_fixture(
        &tmp.path().join("bundle"),
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    open(tmp).await
}

/// A layer whose one supplied content is gated on **every** member of its generating set.
async fn register(server: &TestServer) {
    let declaration = json!({
        "name": TOPICS,
        "title": TOPICS,
        "views": ["s0"],
        "membership": "enumerated",
        "value_set": "closed",
        "visibility": null,
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": null,
        "hierarchy": { "kind": "flat", "prune_children": true },
        "content": { "computed": [], "supplied": [
            { "name": "topic", "type": "text", "require_member_visibility": "all" }
        ] },
        "depends_on": [],
        "levels": []
    });
    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&declaration)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "the layer registers");
}

fn artifacts_url(server: &TestServer) -> String {
    server.control_url(&format!(
        "/control/layers/{}/artifacts",
        TOPICS.replace('/', "%2F")
    ))
}

/// Publish artifacts. The acknowledgement is durable; nothing is published until the next tick.
async fn put(server: &TestServer, artifacts: serde_json::Value) -> serde_json::Value {
    let resp = server
        .client
        .put(artifacts_url(server))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "addressing": "external", "artifacts": artifacts }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    assert!(status < 300, "{status}: {body}");
    body
}

/// Page a generating set with the entities that joined it.
async fn patch(server: &TestServer, artifacts: serde_json::Value) -> serde_json::Value {
    let resp = server
        .client
        .patch(artifacts_url(server))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "addressing": "external", "artifacts": artifacts }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    assert!(status < 300, "{status}: {body}");
    body
}

/// Ingest one batch under a fresh batch id.
async fn ingest(server: &TestServer, batch_id: &str, rows: &[(&str, f32, f32, &[&str])]) {
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(ingest_batch(rows))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap()
    );
}

/// One entity's disposition.
async fn change(server: &TestServer, external_id: &str, op: &str) {
    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!([{ "external_id": external_id, "op": op }]))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap()
    );
}

/// The layer's served `(key, content)` pairs for a principal holding `terms`. An artifact whose
/// content this principal is not contained in is absent whole (decision 0076), so a key missing
/// here is a content withheld.
async fn served(server: &TestServer, terms: &[&str]) -> Vec<(String, Vec<String>)> {
    let auth = authorise(server, terms).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&json!({
            "view": "s0",
            "zoom": 0,
            "bbox": [0.0, 0.0, 1000.0, 1000.0],
            "k": 200,
            "layers": [TOPICS],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let mut rows: Vec<(String, Vec<String>)> = decode_viewport_frames(&resp.bytes().await.unwrap())
        .artifacts
        .unwrap_or_default()
        .into_iter()
        .filter(|row| row.layer == TOPICS)
        .filter_map(|row| row.key.clone().map(|key| (key, row.content.clone())))
        .collect();
    rows.sort();
    rows
}

/// The keys served to a principal holding `terms`.
async fn keys(server: &TestServer, terms: &[&str]) -> Vec<String> {
    served(server, terms)
        .await
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

/// The four artifacts of the matrix, by the provenance of their generating sets.
const BUILT: &str = "built";
const FLUSHED: &str = "flushed";
const MIXED: &str = "mixed";
const GROWN: &str = "grown";
/// Published in the same commit window as the entities its set names.
const BUFFERED: &str = "buffered";

/// Every key the broad principal is served once the matrix is built.
fn all_keys() -> Vec<String> {
    let mut keys: Vec<String> = [BUILT, FLUSHED, MIXED, GROWN, BUFFERED]
        .iter()
        .map(|k| k.to_string())
        .collect();
    keys.sort();
    keys
}

/// Build the matrix on a running server: five all-gated labels whose generating sets differ only
/// in where their members' rows come from.
///
/// The two principals are the fixture's. The broad one holds term `0`, which every built item
/// carries; the narrow one holds term `1`, which every third built item carries. An ingested item
/// carries the labels its batch gave it, so a set can be made to hold exactly one member the
/// narrow principal cannot see.
async fn build_matrix(server: &TestServer) {
    register(server).await;

    // Members whose rows the build wrote. `0`, `3` and `6` carry both terms; `1` carries only the
    // broad principal's, and is the one member the narrow principal lacks.
    let shared = vec![member(0), member(3), member(6)];
    let broad_only = member(1);

    // A set of built members alone.
    put(
        server,
        json!([{
            "key": BUILT,
            "members": (0..40).map(member).collect::<Vec<_>>(),
            "content": [{
                "values": ["from the build"],
                "generated_from": shared.iter().cloned().chain([broad_only.clone()]).collect::<Vec<_>>()
            }]
        }]),
    )
    .await;

    // Three ingested members: two visible to both principals, one to the broad principal alone.
    ingest(
        server,
        "generating-flushed",
        &[
            ("flush-a", 11.0, 11.0, &["0", "1"]),
            ("flush-b", 12.0, 12.0, &["0", "1"]),
            ("flush-c", 13.0, 13.0, &["0"]),
        ],
    )
    .await;
    tick(server).await;

    // A set of flushed members alone, and one mixing the two provenances.
    put(
        server,
        json!([
            {
                "key": FLUSHED,
                "members": (0..40).map(member).collect::<Vec<_>>(),
                "content": [{
                    "values": ["from the ingest"],
                    "generated_from": [ingested("flush-a"), ingested("flush-b"), ingested("flush-c")]
                }]
            },
            {
                "key": MIXED,
                "members": (0..40).map(member).collect::<Vec<_>>(),
                "content": [{
                    "values": ["from both"],
                    "generated_from": shared.iter().cloned().chain([ingested("flush-c")]).collect::<Vec<_>>()
                }]
            },
            {
                // Published over built members alone; a flushed member joins the set below.
                "key": GROWN,
                "members": (0..40).map(member).collect::<Vec<_>>(),
                "content": [{ "values": ["grown at ingest"], "generated_from": shared.clone() }]
            }
        ]),
    )
    .await;
    tick(server).await;

    // **A growth by a member that already has a row.** The set gains it at this publication.
    patch(
        server,
        json!([{ "key": GROWN, "rank": 0, "members": [ingested("flush-c")] }]),
    )
    .await;
    tick(server).await;

    // **Published in the same commit window as its members.** The entities are buffered and have
    // no rows, so the set is short of its declared size and the content is withheld from everyone.
    ingest(
        server,
        "generating-buffered",
        &[
            ("buffer-a", 21.0, 21.0, &["0", "1"]),
            ("buffer-b", 22.0, 22.0, &["0"]),
        ],
    )
    .await;
    put(
        server,
        json!([{
            "key": BUFFERED,
            "members": (0..40).map(member).collect::<Vec<_>>(),
            "content": [{
                "values": ["published with its members"],
                "generated_from": [ingested("buffer-a"), ingested("buffer-b")]
            }]
        }]),
    )
    .await;
}

/// Assert the matrix's answers: every label served to the principal who holds every member of its
/// generating set, and every one withheld from the principal who lacks a single member.
async fn assert_matrix(server: &TestServer, at: &str) {
    assert_eq!(
        keys(server, &["0"]).await,
        all_keys(),
        "{at}: the broad principal holds every member of every set and must be served all five"
    );
    assert_eq!(
        keys(server, &["1"]).await,
        Vec::<String>::new(),
        "{at}: the narrow principal lacks one member of each set and must be served none"
    );
    assert_eq!(
        served(server, &["0"]).await,
        vec![
            (BUFFERED.to_string(), vec!["published with its members".to_string()]),
            (BUILT.to_string(), vec!["from the build".to_string()]),
            (FLUSHED.to_string(), vec!["from the ingest".to_string()]),
            (GROWN.to_string(), vec!["grown at ingest".to_string()]),
            (MIXED.to_string(), vec!["from both".to_string()]),
        ],
        "{at}: the served text is the caller's own"
    );
}

/// **The matrix.** Built, flushed, mixed, grown and same-commit-window generating sets, each
/// served to a principal who holds every member and withheld from one who lacks a single member —
/// and the same answers after a fold and after a restart.
#[tokio::test]
async fn an_all_gated_label_is_served_over_flushed_members_and_withheld_from_a_viewer_missing_one()
{
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    build_matrix(&server).await;

    // Before the flush the buffered set is short of its declared size, so its content is withheld
    // from everyone. The other four are unaffected.
    let mut without_buffered = all_keys();
    without_buffered.retain(|key| key != BUFFERED);
    assert_eq!(
        keys(&server, &["0"]).await,
        without_buffered,
        "a set whose members have no rows yet is short, and its content is withheld from everyone"
    );
    assert_eq!(keys(&server, &["1"]).await, Vec::<String>::new());

    // The flush gives them rows, and the label is served at that publication.
    tick(&server).await;
    assert_matrix(&server, "after the flush").await;

    // A fold folds every extent into the base. The answers do not move.
    fold(&server).await;
    assert_matrix(&server, "after the fold").await;
}

/// **A suppressed member withholds the content from everyone, and an unsuppress restores it.** The
/// deny mask is derived over the whole row space, so a suppression reaches a flushed member's
/// extent row exactly as it reaches a built member's base row. Asserted across a restart, which
/// re-derives both the row forms and the mask from the durable record.
#[tokio::test]
async fn a_suppressed_member_of_a_generating_set_withholds_the_content_from_everyone() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    build_matrix(&server).await;
    tick(&server).await;
    assert_matrix(&server, "after the flush").await;

    // The mixed set's flushed member, which only the broad principal can see and which is the one
    // member that makes the narrow principal fail it.
    change(&server, &ingested("flush-c"), "suppress").await;
    let mut without_suppressed = all_keys();
    without_suppressed.retain(|key| key != MIXED && key != FLUSHED && key != GROWN);
    assert_eq!(
        keys(&server, &["0"]).await,
        without_suppressed,
        "every set holding the suppressed member is withheld from the principal who otherwise \
         holds all of it"
    );
    assert_eq!(keys(&server, &["1"]).await, Vec::<String>::new());

    // **A restart in the middle of it.** The suppression is durable, so the answers are the same
    // once the process reopens the database.
    let server = {
        server.shutdown().await;
        open(&tmp).await
    };
    assert_eq!(
        keys(&server, &["0"]).await,
        without_suppressed,
        "the suppression is in force after the reopen"
    );
    assert_eq!(keys(&server, &["1"]).await, Vec::<String>::new());

    // An unsuppress restores the member, and the content is served again.
    change(&server, &ingested("flush-c"), "unsuppress").await;
    assert_matrix(&server, "after the unsuppress").await;
}

/// **The same answers across a merge.** A merge renumbers the rows inside the span it consumes, so
/// a generating set holding a flushed member holds rows the merge moved.
#[tokio::test]
async fn a_merge_leaves_every_answer_where_it_was() {
    let tmp = TempDir::new().unwrap();
    build_fixture(
        &tmp.path().join("bundle"),
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    // `tier_width` 2, so two flushed segments select a merge.
    let server = spawn_server_with_config(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        EngineConfig {
            tier_width: Some(2),
            ..default_engine_config()
        },
    )
    .await;
    build_matrix(&server).await;
    tick(&server).await;
    assert_matrix(&server, "after the flush").await;

    // Flush further segments until a merge has published.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut filler = 0;
    while server.state.engine.write_executor_stats().merges == 0 {
        assert!(std::time::Instant::now() < deadline, "no merge published");
        filler += 1;
        let name = format!("merge-filler-{filler}");
        ingest(
            &server,
            &name,
            &[(&name, 30.0 + filler as f32, 30.0, &["0", "1"])],
        )
        .await;
        tick(&server).await;
    }
    assert_matrix(&server, "after the merge").await;
}

/// Fold every extent into the base.
async fn fold(server: &TestServer) {
    let before = server.state.engine.write_executor_stats();
    let resp = server
        .client
        .post(server.control_url("/control/compact"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let now = server.state.engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        if now.folds > before.folds {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "the fold never ran");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
