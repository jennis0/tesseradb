//! The control plane's **write path**, end to end: `/control/ingest`, `/control/changes`,
//! `/control/status`, batch-id idempotency, external-id duplicate detection, entity allocation,
//! WAL behaviour and the health/readiness endpoints.
//!
//! Split out of `tests/http.rs` by Task 0c (Phase 2 stage 2.1) so that stage 2.1's write-path
//! track owns this file outright. **No test moved here was otherwise changed** — same names, same
//! assertions, same fixture values; only the binary they live in. Shared fixtures live in
//! [`common`]; a few doc comments below refer to tests that stayed in `tests/http.rs`.

mod common;

use std::sync::Arc;

use arrow::array::{BinaryArray, Float32Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use base64::Engine as _;
use tempfile::TempDir;

use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;

use common::*;

fn build_ingest_batch(rows: &[(u64, f32, f32, &str)]) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("access", DataType::Utf8, false),
    ]));
    let ext: Vec<Vec<u8>> = rows
        .iter()
        .map(|(id, _, _, _)| external_id_of(*id))
        .collect();
    let ext_array = BinaryArray::from_iter_values(ext.iter().map(|v| v.as_slice()));
    let x_array = Float32Array::from_iter_values(rows.iter().map(|(_, x, _, _)| *x));
    let y_array = Float32Array::from_iter_values(rows.iter().map(|(_, _, y, _)| *y));
    let access_array = StringArray::from_iter_values(rows.iter().map(|(_, _, _, a)| *a));

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(ext_array),
            Arc::new(x_array),
            Arc::new(y_array),
            Arc::new(access_array),
        ],
    )
    .unwrap();

    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

/// Like [`build_ingest_batch`], but takes the raw `external_id` bytes directly rather than
/// deriving them from a source id — needed for Task 11's duplicate-detection and cap tests, which
/// must construct exact byte strings (repeats across rows, or a specific length) that
/// `external_id_of`'s 8-byte little-endian convention cannot express.
fn build_ingest_batch_raw(rows: &[(&[u8], f32, f32, &str)]) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("access", DataType::Utf8, false),
    ]));
    let ext_array = BinaryArray::from_iter_values(rows.iter().map(|(id, _, _, _)| *id));
    let x_array = Float32Array::from_iter_values(rows.iter().map(|(_, x, _, _)| *x));
    let y_array = Float32Array::from_iter_values(rows.iter().map(|(_, _, y, _)| *y));
    let access_array = StringArray::from_iter_values(rows.iter().map(|(_, _, _, a)| *a));

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(ext_array),
            Arc::new(x_array),
            Arc::new(y_array),
            Arc::new(access_array),
        ],
    )
    .unwrap();

    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

/// Like [`build_ingest_batch_raw`], but `external_id` is `Option<&[u8]>` per row — contracts §3.4
/// r6: an ingested item may carry no external id at all, in which case it is addressable only by
/// the `tessera_id` `/control/ingest`'s response returns for it. The column is declared nullable
/// here (unlike the other two builders, which happen to always supply a value): this is the
/// null-within-the-column shape the server must accept.
fn build_ingest_batch_optional(rows: &[(Option<&[u8]>, f32, f32, &str)]) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("access", DataType::Utf8, false),
    ]));
    let ext_array = BinaryArray::from_iter(rows.iter().map(|(id, _, _, _)| *id));
    let x_array = Float32Array::from_iter_values(rows.iter().map(|(_, x, _, _)| *x));
    let y_array = Float32Array::from_iter_values(rows.iter().map(|(_, _, y, _)| *y));
    let access_array = StringArray::from_iter_values(rows.iter().map(|(_, _, _, a)| *a));

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(ext_array),
            Arc::new(x_array),
            Arc::new(y_array),
            Arc::new(access_array),
        ],
    )
    .unwrap();

    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

#[tokio::test]
async fn e_suppress_via_changes_drops_the_count_without_reauthorising() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let viewport_req = serde_json::json!({
        "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
    });

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&viewport_req)
        .send()
        .await
        .unwrap();
    let (tiles_before, _) = decode_viewport(&resp.bytes().await.unwrap());

    const SUPPRESS_SOURCE_ID: u64 = 5;
    let external_id =
        base64::engine::general_purpose::STANDARD.encode(external_id_of(SUPPRESS_SOURCE_ID));
    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&serde_json::json!([{ "external_id": external_id, "op": "suppress" }]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&viewport_req)
        .send()
        .await
        .unwrap();
    let (tiles_after, _) = decode_viewport(&resp.bytes().await.unwrap());

    assert_eq!(tiles_after[0].1, tiles_before[0].1 - 1);
}

#[tokio::test]
async fn f_ingest_is_wal_before_ack_and_idempotent() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let body = build_ingest_batch(&[(N_ITEMS + 1, 10.0, 10.0, "0")]);

    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "batch-1")
        .header("content-type", "application/octet-stream")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["accepted"], 1);
    assert_eq!(json["over_bound"], 0);

    // Replay of the same batch id + body: idempotent 200.
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "batch-1")
        .header("content-type", "application/octet-stream")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Same id, different body: 409.
    let different_body = build_ingest_batch(&[(N_ITEMS + 2, 11.0, 11.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "batch-1")
        .header("content-type", "application/octet-stream")
        .body(different_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
}

/// Contracts §3.1: duplicate external ids *within* one batch are `409 conflict`, and the batch
/// has NO effect at all -- not even the non-duplicate rows are accepted.
#[tokio::test]
async fn ingest_rejects_duplicate_external_ids_within_one_batch() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let high_water_before = control_status(&server).await["entity_id_high_water"].clone();

    let body = build_ingest_batch_raw(&[
        (b"a".as_slice(), 1.0, 1.0, "0"),
        (b"b".as_slice(), 2.0, 2.0, "0"),
        (b"a".as_slice(), 3.0, 3.0, "0"),
    ]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "dup-batch")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["error"], "conflict");

    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water_before,
        "a 409 batch must have no effect at all -- not even the non-duplicate rows"
    );
}

/// Important I-8: dedup must consult `Engine::established`, not only the bundle's external-id
/// sidecar. The sidecar covers only the bundle built at open time; an id ingested five minutes
/// ago in a *separate*, already-accepted batch lives only in the live map, and a dedup check
/// that misses it would silently allocate a second entity and orphan the first
/// (`write.rs`'s `WritePath::accept_ingest` doc — the acceptance path moved out of `session.rs`
/// behind the Task 0a seam).
#[tokio::test]
async fn ingest_rejects_an_external_id_ingested_after_the_build() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let first_body = build_ingest_batch_raw(&[(b"z".as_slice(), 1.0, 1.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "z-batch-1")
        .header("content-type", "application/octet-stream")
        .body(first_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let high_water_after_first = control_status(&server).await["entity_id_high_water"].clone();

    // A fresh batch id, re-ingesting the same external id: must be rejected, not silently
    // allocate a second entity for "z".
    let second_body = build_ingest_batch_raw(&[(b"z".as_slice(), 9.0, 9.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "z-batch-2")
        .header("content-type", "application/octet-stream")
        .body(second_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["error"], "conflict");

    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water_after_first,
        "the rejected re-ingest must not have allocated a second entity"
    );
}

/// The bundle's own external-id sidecar half of duplicate detection: an id already present in
/// the built bundle (not merely ingested live) must also be rejected.
#[tokio::test]
async fn ingest_rejects_an_external_id_already_in_the_bundle() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let high_water_before = control_status(&server).await["entity_id_high_water"].clone();

    // `external_id_of(0)` names a real item baked into the fixture at build time.
    let body = build_ingest_batch(&[(0, 5.0, 5.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "bundle-dup-batch")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["error"], "conflict");

    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water_before
    );
}

/// Ordering matters and is not incidental: the batch-id replay check stays FIRST. An idempotent
/// retry of an already-accepted batch id + body is a 200 no-op, even though the external id it
/// carries is (correctly) "already known" by the time the duplicate check would run.
#[tokio::test]
async fn an_idempotent_retry_of_an_accepted_batch_is_a_200_not_a_409() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let body = build_ingest_batch_raw(&[(b"replay-me".as_slice(), 1.0, 1.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "replay-batch")
        .header("content-type", "application/octet-stream")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "replay-batch")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "a byte-identical replay of an already-acked batch id must stay a 200, never be caught \
         by the duplicate-external-id check"
    );
}

/// Contracts §1 (r6): external ids are capped at ≤ 64 bytes. Off-by-one is the whole point: 64
/// bytes exactly is accepted, 65 is a typed error, never a silent truncation.
#[tokio::test]
async fn ingest_external_id_cap_is_64_bytes_exactly() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let exactly_64 = vec![b'x'; 64];
    let body = build_ingest_batch_raw(&[(exactly_64.as_slice(), 1.0, 1.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "cap-64")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "exactly 64 bytes must be accepted");

    let high_water_before = control_status(&server).await["entity_id_high_water"].clone();

    let sixty_five = vec![b'y'; 65];
    let body = build_ingest_batch_raw(&[(sixty_five.as_slice(), 2.0, 2.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "cap-65")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        422,
        "65 bytes must be a typed contract error, never truncated to 64"
    );
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["error"], "contract");

    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water_before,
        "a rejected over-length batch must have no effect"
    );
}

/// The point of batching resolution: a large batch must open each bundle extent at most once,
/// not once per row. The fixture bundle has one external-id extent (built with `N_ITEMS` rows),
/// so a batch of many distinct, never-before-seen external ids must resolve against it without
/// the sidecar opening more than that one extent.
#[tokio::test]
async fn a_batch_resolution_opens_each_extent_at_most_once() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let rows: Vec<(u64, f32, f32, &str)> = (0..2_000)
        .map(|i| (N_ITEMS + 10_000 + i, i as f32, i as f32, "0"))
        .collect();
    let body = build_ingest_batch(&rows);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "big-batch")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["accepted"], 2_000);
}

/// Important 1: `GET /control/status` must require the operator bearer credential — it discloses
/// `entity_id_high_water`, a global unmasked corpus-size fact, and the control listener may be
/// plain loopback TCP, not only a unix socket.
#[tokio::test]
async fn control_status_requires_bearer() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let resp = server
        .client
        .get(server.control_url("/control/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let resp = server
        .client
        .get(server.control_url("/control/status"))
        .bearer_auth("not-the-operator-credential")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let resp = server
        .client
        .get(server.control_url("/control/status"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// Important 2: a `/control/changes` batch whose *later* item fails validation (unknown external
/// id) must leave every earlier item in the same batch unapplied — validate-first, not
/// apply-then-abort. Suppresses a real item first in the batch, then names a nonexistent external
/// id second; the whole request must 404, and the real item's count must be unaffected.
#[tokio::test]
async fn changes_batch_validates_before_applying_anything() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let viewport_req = serde_json::json!({
        "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
    });
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&viewport_req)
        .send()
        .await
        .unwrap();
    let (tiles_before, _) = decode_viewport(&resp.bytes().await.unwrap());

    const REAL_SOURCE_ID: u64 = 9;
    let real_external_id =
        base64::engine::general_purpose::STANDARD.encode(external_id_of(REAL_SOURCE_ID));
    // Not a real external id (never ingested/built) — must 404 during validation.
    let bogus_external_id =
        base64::engine::general_purpose::STANDARD.encode(b"this-external-id-does-not-exist");

    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&serde_json::json!([
            { "external_id": real_external_id, "op": "suppress" },
            { "external_id": bogus_external_id, "op": "suppress" },
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&viewport_req)
        .send()
        .await
        .unwrap();
    let (tiles_after, _) = decode_viewport(&resp.bytes().await.unwrap());

    assert_eq!(
        tiles_after[0].1, tiles_before[0].1,
        "the batch's first item must not have been applied once a later item failed validation"
    );
}

/// Critical 1: two concurrent acceptances (one `/control/ingest`, one `/control/changes`) must
/// both survive — Phase 1's unlocked apply+swap allowed a lost-update race where whichever
/// `store()` won silently discarded the other's already-fsynced, already-acked change. Runs the
/// engine's `accept_ingest`/`accept_change` directly (not through HTTP) on two OS threads, synced
/// to start together, so both race for the executor.
///
/// **Kept even though Task 3a makes the race structurally impossible.** There is now exactly one
/// thread that can publish a generation, so there is no second `store()` to lose to — the mutex
/// this test was written against has been deleted along with the hazard. What it still buys is a
/// regression alarm on the *property* rather than on the mechanism: any future change that
/// reintroduced a second publisher (stage 2.2's flush is the obvious candidate, and lifecycle
/// §1.3 requires it to submit rather than store) would show up here as a silently lost
/// suppression. That is Track C's S2, and this is the behavioural half of the guard —
/// `scripts/check-layers.sh`'s `.store(Arc::new(` rule is the mechanical half.
#[test]
fn concurrent_ingest_and_change_both_survive() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let mut engine = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Passthrough::new(),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            max_k: 200,
            k_min: 2,
            k_max_marks: 200,
            theta_target_marks: u64::MAX,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
            max_tiles_per_request: 262_144,
            compute_threads: tessera_engine::default_compute_threads(),
            pin_ttl_secs: 300,
            pins_per_session_max: 4,
        },
    )
    .expect("engine should open");
    engine
        .start_write_executor(64)
        .expect("the write executor starts once");
    let engine = Arc::new(engine);

    const SUPPRESS_SOURCE_ID: u64 = 3;
    let suppress_entity = engine
        .resolve_external_id(&external_id_of(SUPPRESS_SOURCE_ID))
        .expect("resolve_external_id should not fail for a healthy bundle")
        .expect("fixture item must resolve");

    let barrier = Arc::new(std::sync::Barrier::new(2));

    let engine_a = Arc::clone(&engine);
    let barrier_a = Arc::clone(&barrier);
    let change_thread = std::thread::spawn(move || {
        barrier_a.wait();
        engine_a
            .accept_change(
                external_id_of(SUPPRESS_SOURCE_ID),
                suppress_entity,
                tessera_lifecycle::ChangeOp::Suppress,
                None,
            )
            .expect("change should be accepted");
    });

    let engine_b = Arc::clone(&engine);
    let barrier_b = Arc::clone(&barrier);
    let ingest_thread = std::thread::spawn(move || {
        let new_external_id = external_id_of(N_ITEMS + 100);
        // Unallocated: Task 3a moved signature-sorted assignment onto the executor, so a caller no
        // longer names the entity id at all.
        let row = tessera_lifecycle::UnallocatedRow {
            external_id: Some(new_external_id.clone()),
            descriptors: vec![b"0".to_vec()],
            x: 5.0,
            y: 5.0,
            scalars: Vec::new(),
            terms: engine_b.resolve_terms(std::slice::from_ref(&b"0".to_vec())),
        };
        barrier_b.wait();
        engine_b
            .accept_ingest(vec![row], "concurrent-batch".to_string(), [7u8; 32])
            .expect("ingest should be accepted")[0]
    });

    change_thread.join().unwrap();
    // The id the EXECUTOR assigned, not one this test chose: Task 3a moved assignment off the
    // caller, so the identity to assert against is the one that comes back.
    let ingested_entity = ingest_thread.join().unwrap();

    // The suppression's effect: a viewport count one lower than the full-coverage baseline.
    // (Phase 1's ingested/buffered items have no row geometry yet — no flush — so the ingested
    // item contributes nothing to any tile's count regardless of correctness; its effect is
    // checked separately below, via the established external-id map a lost swap would revert.)
    let session = engine
        .authorise(br#"{"terms": ["0"]}"#)
        .expect("authorise should succeed");
    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], (N_ITEMS + 10) as usize),
        )
        .expect("viewport should succeed");

    assert_eq!(
        out.tiles[0].visible,
        N_ITEMS - 1,
        "the concurrent suppression must have survived — a lost update would leave the count \
         unchanged"
    );

    // The ingest's effect: the newly-accepted external id must resolve to its assigned entity —
    // a lost update (the ingest's generation swap silently reverted by a racing change, or vice
    // versa) would make this `None`.
    let new_external_id = external_id_of(N_ITEMS + 100);
    assert_eq!(
        engine
            .resolve_external_id(&new_external_id)
            .expect("resolve_external_id should not fail for a healthy bundle"),
        Some(ingested_entity),
        "the concurrent ingest must have survived — a lost update would drop it from the live \
         buffer/established state"
    );
}

/// Contracts §3.4 (r6): an item ingested with no external id at all is still accepted, and the
/// `tessera_id` the 200 response returns for it is a genuine, correctly-shard-scoped identity for
/// the entity that was actually allocated — the only way the item is addressable at all, since it
/// has no external id.
///
/// This does not assert a `200` from `/v1/items`: Phase 1 has no flush yet, so *any* freshly
/// ingested item — with or without an external id — has no row geometry until the next
/// `tessera build`, and `Engine::item`'s own doc records that a visible-but-geometryless entity
/// is a `404`, identical to an unknown one. That is a pre-existing Phase 1 limitation, orthogonal
/// to this feature. What this test checks instead is the thing this feature actually promises:
/// inverting the returned `tessera_id` with the deployment's own identity key yields the right
/// shard and a freshly-allocated entity id (at or past the bundle's `N_ITEMS` high-water mark),
/// so the caller genuinely learned a working identity for its item, not a decoy.
#[tokio::test]
async fn ingest_with_a_null_external_id_returns_a_genuinely_resolvable_tessera_id() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let body = build_ingest_batch_optional(&[(None, 20.0, 20.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "null-ext-batch")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["accepted"], 1);
    let tessera_ids = json["tessera_ids"].as_array().unwrap();
    assert_eq!(tessera_ids.len(), 1);
    let tessera_id = tessera_ids[0].as_u64().unwrap();

    // The fixture's own identity key (matches `TEST_KEY_HEX`, shard 0) — inverting independently
    // of the server proves the response carries a real, working identity, not an opaque number.
    let (shard, entity) = test_key().invert(tessera_types::TesseraId::new(tessera_id));
    assert_eq!(shard, 0, "the fixture bundle is shard 0");
    assert!(
        entity.raw() >= N_ITEMS,
        "a freshly-ingested item must get an entity id past the bundle's own N_ITEMS range, not \
         collide with a built-in item"
    );

    // Phase 1's documented limitation, not a defect this feature introduces: no flush yet means
    // no row geometry for any freshly-ingested item, so `/v1/items` 404s identically to an
    // unknown id (`Engine::item`'s doc).
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let resp = post_item(&server, token, tessera_id).await;
    assert_eq!(
        resp.status(),
        404,
        "a buffered (unflushed) item 404s on /v1/items regardless of external id, per Phase 1's \
         documented row-geometry limitation"
    );
}

/// Contracts §3.4 (r6): a batch mixing items with and without an external id is accepted whole,
/// and duplicate detection considers only the supplied ones — the null-external-id rows have
/// nothing to collide on and must not be rejected or interfere with the others' dedup check.
#[tokio::test]
async fn ingest_mixed_batch_only_supplied_external_ids_participate_in_dedup() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let body = build_ingest_batch_optional(&[
        (Some(b"mixed-a".as_slice()), 1.0, 1.0, "0"),
        (None, 2.0, 2.0, "0"),
        (None, 3.0, 3.0, "0"),
        (Some(b"mixed-b".as_slice()), 4.0, 4.0, "0"),
    ]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "mixed-batch")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "two null external ids in one batch must not be treated as duplicates of each other"
    );
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["accepted"], 4);
    let tessera_ids = json["tessera_ids"].as_array().unwrap();
    assert_eq!(tessera_ids.len(), 4);

    // A follow-up batch re-using one of the *supplied* external ids must still be caught.
    let dup_body = build_ingest_batch_optional(&[(Some(b"mixed-a".as_slice()), 9.0, 9.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "mixed-batch-dup")
        .header("content-type", "application/octet-stream")
        .body(dup_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
}

/// Contracts §3.4 (r6): two items with no external id in the *same* batch must not collide with
/// each other — `null` is not a key that can be duplicated.
#[tokio::test]
async fn ingest_two_null_external_ids_in_one_batch_do_not_collide() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let body = build_ingest_batch_optional(&[(None, 1.0, 1.0, "0"), (None, 2.0, 2.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "two-nulls-batch")
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["accepted"], 2);
    let tessera_ids = json["tessera_ids"].as_array().unwrap();
    assert_eq!(tessera_ids.len(), 2);
    assert_ne!(
        tessera_ids[0].as_u64().unwrap(),
        tessera_ids[1].as_u64().unwrap(),
        "two null-external-id items must still get distinct entities/tessera_ids"
    );
}

/// Task 3 (D-A): `/healthz` must stay prompt while a viewport request runs, even on this test's
/// single-threaded (`#[tokio::test]` default, current-thread) runtime — the strongest possible
/// demonstration of the bug this task fixes. Pre-refactor, `viewport`'s whole body (the engine
/// call through Arrow IPC framing) is synchronous Rust with no `.await` inside it; once tokio's
/// one worker thread starts polling that task it cannot be interrupted, so a concurrent
/// `/healthz` task cannot even be *polled* — let alone answered — until the viewport handler
/// returns. `spawn_blocking` gives the viewport task a genuine `.await` point: the blocking work
/// moves to tokio's separate blocking-thread pool (a real OS thread, regardless of runtime
/// flavor), freeing the one reactor thread to service `/healthz` while it runs.
///
/// Slowness is engineered deterministically via the §3.3 density underlay's `4^offset` sub-cell
/// fan-out (`tessera_engine::viewport`'s cost model — each sub-cell costs one small binary search
/// plus one bitmap range-count, independent of corpus size), not via corpus size — so the fixture
/// stays at the file's default `N_ITEMS` and builds in the same sub-second time every other test
/// here does. `offset = 12` at `zoom = 0` (one tile, so the tile-count bound never engages) asks
/// for `4^12 ≈ 16.8M` sub-cell evaluations.
///
/// **The passing (post-refactor) bound is self-scaling, not a fixed wall-clock bet.** A fixed
/// `healthz_elapsed < 1s` assumed this debug-profile binary's absolute speed; on a slower or more
/// loaded runner the sweep itself takes longer, and there is no reason `/healthz`'s bound should
/// stay pinned to 1s while the workload it is racing against grows. Instead this asserts
/// `healthz_elapsed < viewport_elapsed / 4`, computed from the *same run*'s own measurements:
/// `/healthz` does no engine work at all (a constant in-memory response) and runs on a different
/// OS thread than the viewport's `spawn_blocking` closure post-refactor, so its cost is bounded by
/// ambient connection/scheduling overhead only — independent of how long the sweep happens to take
/// on this particular machine. A quarter is generous headroom over that overhead on any runner,
/// while still failing loudly if the reactor were starved for anywhere close to the sweep's own
/// duration. The `viewport_elapsed > 200ms` floor below is a much weaker, absolute sanity check
/// only — it exists so a degenerate near-zero workload (e.g. a future edit that shrinks `offset`)
/// cannot make the ratio pass without genuinely engineering slowness — it is not the bound this
/// test relies on for its pass/fail signal.
#[tokio::test]
async fn healthz_stays_prompt_while_a_long_viewport_runs() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let mut config = default_engine_config();
    // Wide enough to let the request below through `Engine::viewport`'s own bounds checks
    // (`EngineError::UnderlayRefused`) rather than being rejected before it ever costs anything.
    config.max_underlay_offset = 12;
    config.max_underlay_cells = 20_000_000;
    let server = spawn_server_with_config(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        config,
    )
    .await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    let viewer_url = server.viewer_url("/v1/viewport");
    let client = server.client.clone();
    let viewport_task = tokio::spawn(async move {
        let start = std::time::Instant::now();
        let resp = client
            .post(viewer_url)
            .bearer_auth(token)
            .json(&serde_json::json!({
                "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 1,
                "underlay_offset": 12
            }))
            .send()
            .await
            .unwrap();
        (resp.status(), start.elapsed())
    });

    // A short, empirically-bounded poll rather than a single `yield_now()`: one yield already
    // reliably gets the freshly-`tokio::spawn`ed viewport task its first turn on this runtime (a
    // newly spawned task is highly likely to run next), but a couple more give the scheduler a
    // little extra room to actually get it moving through connect/accept/parse before this task's
    // own `/healthz` clock starts, without over-polling.
    //
    // **This number is deliberately small, and was tuned, not guessed.** Pre-refactor, once the
    // viewport task's poll reaches the synchronous handler body it runs to completion in that
    // same turn with no further yield -- there is no observable "started but not finished" state
    // to poll for. So more polling here does not make the wait-for-start more precise, it just
    // gives the scheduler more chances to run the *entire* pre-refactor request (client connect
    // through server response) to completion before `/healthz` is ever sent, which would silently
    // stop this test from racing anything at all. Measured directly against the pre-refactor code
    // (temporarily reverting the four `src/` files this task changes): looping 1 or 2 times still
    // reliably starves `/healthz` (this test correctly fails); looping 3 or more times reliably
    // lets the whole pre-refactor viewport request finish first, turning this into a no-op race
    // every time (this test wrongly passes). `2` is the largest value on the correct side of that
    // measured boundary.
    for _ in 0..2 {
        tokio::task::yield_now().await;
    }

    let healthz_start = std::time::Instant::now();
    let healthz_resp = server
        .client
        .get(server.viewer_url("/healthz"))
        .send()
        .await
        .unwrap();
    let healthz_elapsed = healthz_start.elapsed();
    assert_eq!(healthz_resp.status(), 200);

    let (viewport_status, viewport_elapsed) = viewport_task.await.unwrap();
    assert_eq!(viewport_status, 200);

    // Weak absolute sanity floor only -- see this test's doc for why the real pass/fail signal is
    // the relative bound below, not this one.
    assert!(
        viewport_elapsed > std::time::Duration::from_millis(200),
        "the viewport request finished in {viewport_elapsed:?}, too fast to exercise this test's \
         starvation scenario -- widen the underlay offset"
    );
    assert!(
        healthz_elapsed < viewport_elapsed / 4,
        "/healthz took {healthz_elapsed:?}, more than a quarter of the {viewport_elapsed:?} the \
         concurrent viewport request took -- the reactor was starved"
    );
}

/// `/control/ingest`'s external ids for [`concurrent_ingests_do_not_delay_a_control_changes_suppress`],
/// chosen well clear of every other test's ranges in this file (`N_ITEMS`, and the `N_ITEMS +
/// 10_000 ..` range `a_batch_resolution_opens_each_extent_at_most_once` uses) so a shared-fixture
/// mistake would show up as a collision 409 rather than silently aliasing another test's ids.
const CONCURRENT_INGEST_BASE_ID: u64 = 50_000_000;
const CONCURRENT_INGEST_BATCHES: u64 = 8;
const CONCURRENT_INGEST_ROWS_PER_BATCH: u64 = 40_000;

/// Task 3 (D-A), review finding 7: a `/control/changes` suppression must not queue behind N
/// concurrent `/control/ingest` batches durability-syncing (lifecycle §1.3's deny priority lane,
/// reached through the reactor) — and this must hold even though `/control/ingest` and
/// `/control/changes` are NEVER behind the Task 4 admission gate (that gate is viewer/session
/// only). Same single-threaded-runtime argument as
/// `healthz_stays_prompt_while_a_long_viewport_runs`: pre-refactor, each ingest handler's Arrow
/// decode, term resolution and WAL append/fsync run synchronously with no `.await`, so once the
/// reactor thread starts executing one, it cannot service any other task — including accepting
/// or reading the suppress request's own connection — until that handler returns. Post-refactor,
/// both handlers do only their bearer check and header/body parse on the reactor, then hand off
/// to `spawn_blocking`'s separate thread pool — so the suppress request's own closure only has to
/// wait, at most, for whichever ONE ingest the single write executor happens to be executing at
/// that instant (Task 3a deleted the WAL mutex this comment used to name: the WAL now moves by
/// value onto one thread, so serialisation is a consequence of ownership rather than of a lock.
/// The bug this task closed is reactor-thread occupation, not that serialisation).
///
/// **Why this is unflaky despite real TCP connections being involved.** Unlike the single
/// `/healthz` race above, this test cannot rely on "the one other task must already be running
/// and cannot be interrupted" alone: `CONCURRENT_INGEST_BATCHES` separate connections are
/// accepted in whatever order the kernel happens to deliver their readiness, so pre-refactor the
/// suppress request is not guaranteed to queue behind literally all of them — only behind
/// whichever are already executing or queued ahead of it. All `CONCURRENT_INGEST_BATCHES`
/// requests are constructed and hand off to `tokio::spawn` before the suppress request is ever
/// sent, so it always races genuinely in-flight ingests, not hypothetical future ones.
///
/// **The passing (post-refactor) bound is self-scaling, not a fixed wall-clock bet** — this was
/// flagged in review: a fixed `suppress_elapsed < 1s` assumes this debug-profile binary's absolute
/// speed, but post-refactor the suppress closure still contends with up to `CONCURRENT_INGEST_
/// BATCHES` blocking-pool threads for CPU and for the single write executor, which serialises
/// every append+fsync onto one thread by owning the WAL outright (Task 3a) — on a
/// slow-fsync or few-core runner that contention genuinely grows, and a fixed 1s bound could trip
/// for reasons that have nothing to do with this task's bug. So this asserts
/// `suppress_elapsed < total_ingest_elapsed / 2`, where `total_ingest_elapsed` is this same run's
/// own wall-clock time for every concurrent ingest batch to complete (measured from the same
/// `Instant` the batches were spawned from, to the last one's `JoinHandle` resolving). That is a
/// fair comparison because both numbers absorb the same runner's slowness together: whatever a
/// batch's parse/resolve/WAL cost is on this machine right now, `total_ingest_elapsed` reflects
/// roughly that cost repeated `CONCURRENT_INGEST_BATCHES` times (parse/resolve run in parallel
/// across the blocking pool, but the WAL section is serialised by the mutex, so the total is
/// dominated by something like `CONCURRENT_INGEST_BATCHES` WAL sections plus overhead), while the
/// suppress request post-refactor only ever has to reach the reactor (bearer check, fast) and
/// then wait for **at most one** ingest's WAL critical section before it gets the mutex itself —
/// a small, close-to-constant fraction of the total regardless of how slow that one section is on
/// this runner. `/2` leaves comfortable headroom over that expected ~1-in-`CONCURRENT_INGEST_
/// BATCHES` fraction even allowing for the WAL mutex's lack of strict fairness. Pre-refactor this
/// stays comfortably RED: the suppress request cannot even begin until the reactor is free, so it
/// queues behind a large share of the full (parse+resolve+WAL) handler bodies, not just one WAL
/// section — measured at ~3.0s suppress against a ~3.3s total in this task's tuning run, i.e. the
/// ratio sits near 1, not under 1/2.
#[tokio::test]
async fn concurrent_ingests_do_not_delay_a_control_changes_suppress() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    // Starts here, not just before the suppress request below: `total_ingest_elapsed` (used for
    // the self-scaling bound at the end of this test) must cover every batch's full wall-clock
    // life, spawn to completion, not just the portion that overlaps the suppress request.
    let ingest_start = std::time::Instant::now();
    let mut ingest_tasks = Vec::with_capacity(CONCURRENT_INGEST_BATCHES as usize);
    for batch in 0..CONCURRENT_INGEST_BATCHES {
        let rows: Vec<(u64, f32, f32, &str)> = (0..CONCURRENT_INGEST_ROWS_PER_BATCH)
            .map(|row| {
                let id = CONCURRENT_INGEST_BASE_ID + batch * CONCURRENT_INGEST_ROWS_PER_BATCH + row;
                (id, row as f32, row as f32, "0")
            })
            .collect();
        let body = build_ingest_batch(&rows);
        let client = server.client.clone();
        let url = server.control_url("/control/ingest");
        let batch_id = format!("concurrent-{batch}");
        ingest_tasks.push(tokio::spawn(async move {
            client
                .post(url)
                .bearer_auth(OPERATOR_CREDENTIAL)
                .header("x-tessera-batch-id", batch_id)
                .header("content-type", "application/octet-stream")
                .body(body)
                .send()
                .await
                .unwrap()
                .status()
        }));
    }

    // Every ingest task is now on the runtime's queue, none of them awaited yet — the suppress
    // request below genuinely races them, not a hypothetical future batch.
    tokio::task::yield_now().await;

    const SUPPRESS_SOURCE_ID: u64 = 7;
    let external_id =
        base64::engine::general_purpose::STANDARD.encode(external_id_of(SUPPRESS_SOURCE_ID));
    let suppress_start = std::time::Instant::now();
    let suppress_resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&serde_json::json!([{ "external_id": external_id, "op": "suppress" }]))
        .send()
        .await
        .unwrap();
    let suppress_elapsed = suppress_start.elapsed();
    assert_eq!(suppress_resp.status(), 200);

    for task in ingest_tasks {
        assert_eq!(
            task.await.unwrap(),
            200,
            "every concurrent ingest batch should still succeed"
        );
    }
    // Only measured once every batch has actually finished — see this test's doc for why this
    // (rather than a fixed wall-clock bound) is what `suppress_elapsed` is compared against.
    let total_ingest_elapsed = ingest_start.elapsed();

    assert!(
        suppress_elapsed < total_ingest_elapsed / 2,
        "/control/changes suppress took {suppress_elapsed:?}, more than half of the \
         {total_ingest_elapsed:?} the {CONCURRENT_INGEST_BATCHES} concurrent ingest batches took \
         to all complete -- it queued behind them on the reactor instead of reaching its own \
         spawn_blocking call promptly"
    );
}

// =================================================================================================
// Task 3b — the readiness posture, and the status a partly-applied change batch reports
// =================================================================================================

/// Provoke a **real** WAL failure by making the WAL's directory read-only.
///
/// `Wal::fsync` writes a sidecar tmp-then-rename, whose `open` needs write permission on the
/// *directory*, so this fails with a genuine `EACCES` and sets the real poison flag —
/// `tessera-lifecycle`'s `a_real_fsync_failure_poisons_the_handle` is the unit test that pins the
/// exact branch and its `Io`-then-`Poisoned` sequence.
///
/// **Deliberately not fault injection.** `tessera-lifecycle`'s `faults` module is gated behind a
/// feature whose own module doc says nothing outside that crate and `tessera-engine`'s tests may
/// depend on it, and reaching it from here would have meant a new dev-dependency plus falsifying
/// three in-tree statements to buy two tests. A real `EACCES` needs none of that and is a stronger
/// witness besides: these tests exercise the failure the WAL actually produces, not a switchboard's
/// imitation of it.
///
/// **Restored by `Drop`, not by a trailing statement.** A failing assertion unwinds, and a
/// `TempDir` cannot delete the contents of a directory it may not write — so a bare restore at the
/// end of the test leaks a directory on exactly the runs where a test fails. This box runs near a
/// full disk (Task 3a's A6), which makes that a real cost rather than a tidiness point.
struct ReadOnlyWalDir<'a>(&'a std::path::Path);

impl<'a> ReadOnlyWalDir<'a> {
    fn new(dir: &'a std::path::Path) -> Self {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        ReadOnlyWalDir(dir)
    }
}

impl Drop for ReadOnlyWalDir<'_> {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// `chmod` does not bind uid 0, so under root these tests would assert nothing rather than fail.
fn skip_under_root() -> bool {
    // SAFETY: `geteuid` is always safe; it takes no arguments and cannot fail.
    let root = unsafe { libc_geteuid() } == 0;
    if root {
        eprintln!("skipped: running as root, where a read-only directory is not read-only");
    }
    root
}

extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

async fn readyz_status(server: &TestServer, url: String) -> u16 {
    server
        .client
        .get(url)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// **The readiness wiring**: `/readyz` reports the write executor's posture, on every listener.
///
/// Driven with `NotStarted` rather than `Dead`, and the reason is a **dependency**, not a race.
/// `ExecutorPosture` is published with `fetch_max`, so it is monotone and `Dead` is absorbing: a
/// bounded poll of `/readyz` after inducing a panic is sound, and is exactly the form
/// `tessera-engine`'s `an_executor_panic_is_reported_dead` uses. What this crate's test binary
/// cannot do is **induce** the panic — that needs `tessera-engine/fault-injection` as a
/// `tessera-server` dev-dependency, declined at Task 3b's design gate (report D4). The `Dead` row is
/// asserted exactly instead, in `health.rs`'s `only_a_running_executor_is_ready`.
///
/// **What this leaves uncovered, stated rather than counted as coverage:** no test drives a
/// panicked executor through the HTTP readiness surface. Taking the dev-dependency is the whole of
/// what it costs.
///
/// Mutations this kills: `readyz` returning `OK` unconditionally (its Phase 1 body); and
/// `is_ready` written as `p != Dead`, which this catches and a `Dead`-only test could not.
#[tokio::test]
async fn an_engine_without_a_write_executor_is_not_ready() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    // `mount_server`, not `spawn_server_from_engine`: the latter starts an executor unconditionally.
    let engine = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Passthrough::new(),
        default_engine_config(),
    )
    .unwrap();
    let server = mount_server(engine, 200, generous_test_gate()).await;

    for url in [
        server.control_url("/readyz"),
        server.viewer_url("/readyz"),
        server.session_url("/readyz"),
    ] {
        assert_eq!(
            readyz_status(&server, url.clone()).await,
            503,
            "a node with no write executor must not report ready on {url}"
        );
    }

    // Liveness is a different question and must stay green: the process is up and answering.
    assert_eq!(
        readyz_status(&server, server.control_url("/healthz")).await,
        200,
        "healthz is liveness, not readiness — a writer fault must not make the process look dead"
    );
}

/// A healthy server is ready on every listener. The anti-vacuity control for the two tests above
/// and below: without it, a `readyz` that returned 503 unconditionally would pass both.
#[tokio::test]
async fn a_healthy_server_is_ready_on_every_listener() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    for url in [
        server.control_url("/readyz"),
        server.viewer_url("/readyz"),
        server.session_url("/readyz"),
    ] {
        assert_eq!(readyz_status(&server, url.clone()).await, 200, "at {url}");
    }
}

/// **The posture's whole justification.** A poisoned WAL makes the node not-ready *and* the node
/// keeps accepting and applying denies.
///
/// Both halves are asserted, and the second is the one that matters. `WalPoisoned` is a posture
/// rather than a shutdown precisely so that every subsequent suppression is still applied to the
/// live overlay and 500'd (lifecycle §4: never a refusal that leaves a deny unapplied). A build
/// that gated `/control/changes` on readiness would apply the first failing suppression and then
/// refuse every later one **without applying it** — refused *and* unapplied, the fail-open this
/// posture exists to prevent.
///
/// Mutations this kills:
/// - add a readiness gate to `changes()` → the **second** suppress answers 503 and item B stays
///   visible → RED. No engine-level test can see this, because the gate would live in `control.rs`.
/// - in `execute_change`, drop the apply-anyway arm (skip the *apply*, not the WAL call — skipping
///   only `wal.append` still lands in that arm and hides the item either way) → both items stay
///   visible → RED.
/// - `readyz` returning `OK` unconditionally → RED at the posture assertion.
#[tokio::test]
async fn a_poisoned_wal_is_not_ready_but_still_accepts_a_deny() {
    if skip_under_root() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    // The WAL gets its own directory: `chmod` is applied to the *directory*, and the bundle and
    // cache must stay writable.
    let wal_dir = tmp.path().join("wal");
    std::fs::create_dir_all(&wal_dir).unwrap();
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &wal_dir.join("wal.log"),
    )
    .await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let viewport_req = serde_json::json!({
        "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
    });
    let visible = |token: String, req: serde_json::Value| {
        let client = server.client.clone();
        let url = server.viewer_url("/v1/viewport");
        async move {
            let bytes = client
                .post(url)
                .bearer_auth(token)
                .json(&req)
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            decode_viewport(&bytes).0[0].1
        }
    };

    let before = visible(token.to_string(), viewport_req.clone()).await;
    assert_eq!(
        readyz_status(&server, server.control_url("/readyz")).await,
        200,
        "the node must start ready, or every assertion below passes vacuously"
    );

    let suppress = |id: u64| {
        let client = server.client.clone();
        let url = server.control_url("/control/changes");
        let external_id = base64::engine::general_purpose::STANDARD.encode(external_id_of(id));
        async move {
            client
                .post(url)
                .bearer_auth(OPERATOR_CREDENTIAL)
                .json(&serde_json::json!([{ "external_id": external_id, "op": "suppress" }]))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };

    let _read_only = ReadOnlyWalDir::new(&wal_dir);

    // (1) The first suppress: its fsync genuinely fails, so it is applied anyway and 500s.
    assert_eq!(
        suppress(5).await,
        500,
        "a deny whose append failed is 500, not 2xx"
    );
    assert_eq!(
        visible(token.to_string(), viewport_req.clone()).await,
        before - 1,
        "lifecycle §4: the item is hidden immediately even though durability failed"
    );

    // (2) The node is now not-ready — on every listener, and the operator plane says why.
    assert_eq!(
        readyz_status(&server, server.control_url("/readyz")).await,
        503,
        "a poisoned WAL must trip the readiness posture"
    );
    let status: serde_json::Value = server
        .client
        .get(server.control_url("/control/status"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["write_executor"]["posture"], "wal-poisoned");
    assert_eq!(status["write_executor"]["ready"], false);

    // (3) **The half that matters.** A second suppress, submitted to the now-not-ready node, is
    // still accepted and still applied. Never a 503 that leaves the deny unapplied.
    assert_eq!(
        suppress(11).await,
        500,
        "a not-ready node must still ACCEPT a deny — a 503 here would be refused AND unapplied"
    );
    assert_eq!(
        visible(token.to_string(), viewport_req.clone()).await,
        before - 2,
        "the second suppression must be in force too; readiness governs routing, not deny \
         acceptance"
    );
}

/// **What a partly-applied change batch reports** — the second Task 3a gate finding.
///
/// `[suppress A, predicate B, suppress C]` with the WAL poisoned mid-batch. A's fsync fails, so A
/// is applied anyway and the handle poisons; B is a **non**-deny, so its refused append applies
/// nothing (lifecycle §4's apply-anyway rule is scoped to `Delete`/`Suppress`, deliberately — an
/// unsuppress applied without durability would re-expose an item replay still hides); C's append is
/// refused by the poison and is applied anyway.
///
/// So the caller gets **one** status for a batch in which some items took effect and one did not,
/// and both the status and the body have to say what actually happened.
///
/// Mutation this kills: restore Task 3a's `?`-abort in `run_changes` → C is never submitted and
/// stays visible → RED. (The *status* half is exercised exhaustively in `error.rs`'s fold tests,
/// where the dispositions can be constructed directly; here all three failures are `Exec(Wal)`, so
/// the status alone would not discriminate a fold from first-error reporting.)
#[tokio::test]
async fn a_partially_applied_change_batch_reports_one_honest_status() {
    if skip_under_root() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let wal_dir = tmp.path().join("wal");
    std::fs::create_dir_all(&wal_dir).unwrap();
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &wal_dir.join("wal.log"),
    )
    .await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let viewport_req = serde_json::json!({
        "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
    });
    let visible = |token: String, req: serde_json::Value| {
        let client = server.client.clone();
        let url = server.viewer_url("/v1/viewport");
        async move {
            let bytes = client
                .post(url)
                .bearer_auth(token)
                .json(&req)
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            decode_viewport(&bytes).0[0].1
        }
    };
    let before = visible(token.to_string(), viewport_req.clone()).await;

    let b64 = |id: u64| base64::engine::general_purpose::STANDARD.encode(external_id_of(id));
    let _read_only = ReadOnlyWalDir::new(&wal_dir);

    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&serde_json::json!([
            { "external_id": b64(5),  "op": "suppress" },
            { "external_id": b64(9),  "op": "predicate", "access": "0" },
            { "external_id": b64(11), "op": "suppress" },
        ]))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 500, "not durable, so never a 2xx");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "fail-closed");
    let detail = body["detail"].as_str().unwrap();
    assert!(
        detail.contains("may be in force"),
        "the operator must be told the denies took hold; got: {detail}"
    );
    assert!(
        !detail.contains("refused"),
        "'refused' is false of the apply-anyway case and invites a retry of a suppression that has \
         already taken hold; got: {detail}"
    );
    // **Fix round 1: the middle item is a `predicate`, and it was NOT applied.** Lifecycle §4's
    // apply-anyway rule covers `Delete`/`Suppress` only, and `execute_change` honours that — a
    // `Predicate` whose append fails is refused without touching the overlay. The op-blind fold
    // counted it as possibly-in-force along with the two suppressions, so `some_not_applied` was
    // false and this body never told the operator that a third of their batch had not taken hold.
    // Constructible with the shape this test already had, which is why the assertion lands here.
    assert!(
        detail.contains("NOT applied"),
        "item 2 is a `predicate`: refused without applying, so the operator must be told to \
         re-submit rather than assume the whole batch took hold; got: {detail}"
    );

    // **The item assertion is the one that discriminates.** C is the third item; a batch that
    // aborted at A's failure would never have submitted it.
    assert_eq!(
        visible(token.to_string(), viewport_req.clone()).await,
        before - 2,
        "both suppressions must be in force — the batch continues past a WAL failure, or every \
         deny after the first is silently unapplied while the WAL stays poisoned"
    );
}

// =================================================================================================
// Task 6 — the admission bound, the batch caps, and the never-shed asymmetry
// =================================================================================================

/// A `Passthrough` that can be made to **park inside `terms_of_label`**, on command.
///
/// `terms_of_label` is called from inside `/control/ingest`'s `spawn_blocking` closure
/// (`control::run_ingest`), which is precisely the blocking-pool thread Task 6's admission bound
/// exists to ration — so parking here holds exactly the resource under test, with no
/// `fault-injection` dependency and no sleep anywhere.
///
/// Every other method delegates, **including both hashes**: the bundle's `MANIFEST.json` records
/// the plugin hash and `Engine::open` refuses a mismatch, so this must be `builtin:passthrough`'s
/// identity in every respect but its willingness to block.
///
/// `armed` is a switch rather than a permanent block because the fixture has to *use* the plugin
/// before it can saturate anything: `/session/authorise` resolves auth terms, and a
/// `/control/changes` item can only name an external id that ingest already established.
struct ParkingPlugin {
    inner: tessera_plugin::Passthrough,
    armed: Arc<std::sync::atomic::AtomicBool>,
    arrived: tokio::sync::mpsc::UnboundedSender<()>,
    release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

impl tessera_plugin::Plugin for ParkingPlugin {
    fn terms_of_label(
        &self,
        access: &[u8],
    ) -> Result<Vec<tessera_plugin::Descriptor>, tessera_plugin::PluginError> {
        if self.armed.load(std::sync::atomic::Ordering::SeqCst) {
            // Publish arrival **before** blocking, so the test waits on a condition this thread
            // has actually reached rather than on a duration it hopes is enough.
            let _ = self.arrived.send(());
            let (lock, cv) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = cv.wait(released).unwrap();
            }
        }
        self.inner.terms_of_label(access)
    }

    fn terms_of_auth(
        &self,
        auth_data: &[u8],
    ) -> Result<tessera_plugin::AuthTerms, tessera_plugin::PluginError> {
        self.inner.terms_of_auth(auth_data)
    }

    fn declared_bounds(&self) -> tessera_plugin::DeclaredBounds {
        self.inner.declared_bounds()
    }

    fn data_plugin_hash(&self) -> String {
        self.inner.data_plugin_hash()
    }

    fn auth_plugin_hash(&self) -> String {
        self.inner.auth_plugin_hash()
    }
}

/// Everything the two saturation tests share: a server whose ingest handlers can be parked on
/// command, with the pool small enough that unbounded ingest would exhaust it.
struct ParkedFixture {
    server: TestServer,
    armed: Arc<std::sync::atomic::AtomicBool>,
    arrived: tokio::sync::mpsc::UnboundedReceiver<()>,
    release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    token: String,
}

impl ParkedFixture {
    fn release(&self) {
        let (lock, cv) = &*self.release;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
}

/// **Release on unwind, or a failing assertion hangs instead of failing.**
///
/// Found by planting this file's own mutations: with the admission bound removed, the surplus
/// requests park, the assertion panics, and `release()` is never reached — so the parked handlers
/// hold their blocking threads forever and the runtime's shutdown blocks joining them. The test
/// was red, but it took three minutes to say so and left threads behind, which in CI is
/// indistinguishable from a hang. A test must fail promptly for the reason it names.
impl Drop for ParkedFixture {
    fn drop(&mut self) {
        self.release();
    }
}

/// `ingest_admission = 2` against a **4-thread** blocking pool: two parked handlers leave two
/// threads, and an unbounded ingest would take all four.
const PARKED_MAX_BLOCKING_THREADS: usize = 4;
const PARKED_INGEST_ADMISSION: usize = 2;

async fn parked_fixture(tmp: &TempDir) -> ParkedFixture {
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (arrived_tx, arrived) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

    let mut engine = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        ParkingPlugin {
            inner: Passthrough::new(),
            armed: Arc::clone(&armed),
            arrived: arrived_tx,
            release: Arc::clone(&release),
        },
        default_engine_config(),
    )
    .expect("engine should open against a freshly built bundle");
    // Generous, so nothing here can 429 for the *queue*: this fixture is about the admission
    // bound, and `ingest_429s_when_the_queue_is_full` is about the other one.
    engine
        .start_write_executor(1024)
        .expect("the write executor starts once");

    let server = mount_server_with_ingest_limits(
        engine,
        200,
        generous_test_gate(),
        IngestLimits {
            admission: PARKED_INGEST_ADMISSION,
            max_batch_rows: 200_000,
            max_batch_bytes: 64 * 1024 * 1024,
        },
    )
    .await;

    // **Warm every connection before arming.** reqwest resolves and connects lazily, and a fresh
    // connection's DNS resolution goes through `spawn_blocking` — on this deliberately tiny pool
    // that would queue behind the parked handlers and turn a real result into a hang. One
    // round-trip per plane now means every later request rides a pooled keep-alive connection.
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();
    assert_eq!(viewport_status(&server, &token).await, 200);
    assert_eq!(control_status(&server).await["ingest"]["in_flight"], 0);

    ParkedFixture {
        server,
        armed,
        arrived,
        release,
        token,
    }
}

/// One `/v1/viewport` on the warmed connection, returning its status.
async fn viewport_status(server: &TestServer, token: &str) -> u16 {
    server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// Post one small, valid ingest batch. Returns `(status, body)`.
async fn post_ingest(
    server: &TestServer,
    batch_id: &str,
    rows: &[(u64, f32, f32, &str)],
    auth: bool,
) -> (u16, serde_json::Value) {
    let mut req = server
        .client
        .post(server.control_url("/control/ingest"))
        .header("x-tessera-batch-id", batch_id)
        .header("content-type", "application/octet-stream")
        .body(build_ingest_batch(rows));
    if auth {
        req = req.bearer_auth(OPERATOR_CREDENTIAL);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

/// Fresh source ids for Task 6's fixtures. Offset well clear of `N_ITEMS`, or every batch here
/// collides with the bundle's own external ids and answers 409 before any bound is consulted.
fn rows_from(base: u64, n: u64) -> Vec<(u64, f32, f32, &'static str)> {
    const TASK_6_ID_BASE: u64 = 1_000_000;
    (0..n)
        .map(|i| (TASK_6_ID_BASE + base + i, i as f32, i as f32, "0"))
        .collect()
}

/// **The class Task 3b left open, closed and demonstrated rather than argued.**
///
/// Task 3b's own deferral, verbatim: *"the viewer plane still shares the blocking FIFO with
/// unbounded ingest, with no timeout on the wait, so an admitted viewport hangs rather than
/// shedding."* `ComputeGate::admit` is `async` and awaited **before** `spawn_blocking`, so viewer
/// *demand* is bounded — but an admitted viewport's closure still queues behind ingest closures in
/// tokio's shared, unbounded FIFO, and there is no timeout on that queue. The failure is a hang,
/// not a shed, and it is invisible to every gauge the viewer plane has.
///
/// The construction: a 4-thread blocking pool, `ingest_admission = 2`, and two ingest handlers
/// parked *inside* `terms_of_label` — i.e. holding two of the four threads as a **fact**, since
/// each publishes its arrival before blocking and the test waits on those arrivals. Two more ingest
/// requests must then be refused **before** `spawn_blocking`, and a viewport must still run.
///
/// **The mutation is the `try_admit` call in `control::ingest`** (delete it, or raise the bound to
/// 4): the two surplus requests then park too, all four threads are held, and both the 429
/// assertions and the viewport time out. The timeouts are in the *failing* path only — on a healthy
/// build every one of these resolves in milliseconds.
///
/// There is deliberately no assertion that the surplus requests *did not* run: a negative statement
/// about another thread's progress is not establishable without waiting (Task 3a fix round 1,
/// CRITICAL 1). What is asserted is that they were refused with the admission body, and that the
/// viewer plane was still served while the pool was demonstrably occupied.
#[test]
fn ingest_admission_sheds_before_the_blocking_pool_fills() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(PARKED_MAX_BLOCKING_THREADS)
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let tmp = TempDir::new().unwrap();
        let mut fx = parked_fixture(&tmp).await;

        fx.armed.store(true, std::sync::atomic::Ordering::SeqCst);

        let mut parked = Vec::new();
        for i in 0..PARKED_INGEST_ADMISSION {
            let client = fx.server.client.clone();
            let url = fx.server.control_url("/control/ingest");
            let body = build_ingest_batch(&rows_from(500 + i as u64 * 10, 1));
            parked.push(tokio::spawn(async move {
                client
                    .post(url)
                    .bearer_auth(OPERATOR_CREDENTIAL)
                    .header("x-tessera-batch-id", format!("parked-{i}"))
                    .header("content-type", "application/octet-stream")
                    .body(body)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            }));
        }
        // Both handlers are inside `terms_of_label`, holding a blocking thread each. A fact, not a
        // hope: each published its arrival before it blocked.
        for _ in 0..PARKED_INGEST_ADMISSION {
            fx.arrived.recv().await.expect("a handler must park");
        }

        // Every admission permit is now held, and `/control/status` says so.
        let status = control_status(&fx.server).await;
        assert_eq!(status["ingest"]["in_flight"], PARKED_INGEST_ADMISSION);

        // The surplus is refused **before** `spawn_blocking`, so it takes no thread.
        for i in 0..2 {
            let (code, body) = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                post_ingest(
                    &fx.server,
                    &format!("surplus-{i}"),
                    &rows_from(900 + i * 10, 1),
                    true,
                ),
            )
            .await
            .expect(
                "a surplus ingest request did not answer within 20s: it is holding a blocking \
                 thread instead of being refused, which is the unbounded-ingest shape Task 3b left \
                 open",
            );
            assert_eq!(code, 429, "surplus ingest must be shed, not queued");
            assert_eq!(body["error"], "backpressure");
            assert!(
                body["detail"]
                    .as_str()
                    .unwrap()
                    .contains("ingest-admission bound"),
                "this must be the ADMISSION 429, not the queue's — the two are discriminated by \
                 body, and this fixture's queue bound is 1024 so the queue cannot be full: {body}"
            );
            // Deviation 11: the header and the body field must agree, and the value must be one
            // this subject argued for.
            assert!(body["retry_after_s"].as_u64().unwrap() >= 1);
        }

        // **And the viewer plane is still served**, on a pool two of whose four threads are held.
        let viewport = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            viewport_status(&fx.server, &fx.token),
        )
        .await
        .expect(
            "an admitted viewport did not complete within 20s while ingest handlers held blocking \
             threads — it is queued behind them in tokio's shared FIFO, with no timeout, which is \
             exactly the hang-rather-than-shed failure this bound exists to prevent",
        );
        assert_eq!(viewport, 200);

        fx.release();
        for task in parked {
            assert_eq!(
                task.await.unwrap(),
                200,
                "a parked ingest must still succeed"
            );
        }
    });
}

/// **The asymmetry, and the test that matters most in this task.** Batching a security operation
/// for latency is acceptable; refusing one for load is fail-open.
///
/// Run **in the same state that 429s ingest**: every admission permit held, every parked handler
/// occupying a blocking thread. `/control/changes` must answer 200 anyway. It does so structurally
/// rather than by luck — it rides its own runtime (`control::DENY_RUNTIME`), its command takes the
/// unbounded lane by `Command::is_never_shed`, and `map_change_batch_error` contains no route from
/// that lane to a 429 at all.
///
/// The suppression is deliberately a bare `{external_id, op}` with **no `access` field**, so
/// `run_changes` makes no `terms_of_label` call and the parking plugin cannot block it. That is a
/// property of the fixture, not of the deny lane, and it is stated here so a later reader does not
/// mistake it for part of what is being proved.
///
/// **Mutations this kills:** an admission bound applied to `changes` as well as `ingest` (429
/// instead of 200); routing `changes` through `tokio::task::spawn_blocking` instead of
/// `spawn_on_deny_lane` (the pool is full, so it hangs and the bounded await fails).
#[test]
fn changes_never_429s() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(PARKED_MAX_BLOCKING_THREADS)
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let tmp = TempDir::new().unwrap();
        let mut fx = parked_fixture(&tmp).await;

        fx.armed.store(true, std::sync::atomic::Ordering::SeqCst);

        let mut parked = Vec::new();
        for i in 0..PARKED_INGEST_ADMISSION {
            let client = fx.server.client.clone();
            let url = fx.server.control_url("/control/ingest");
            let body = build_ingest_batch(&rows_from(600 + i as u64 * 10, 1));
            parked.push(tokio::spawn(async move {
                client
                    .post(url)
                    .bearer_auth(OPERATOR_CREDENTIAL)
                    .header("x-tessera-batch-id", format!("parked-{i}"))
                    .header("content-type", "application/octet-stream")
                    .body(body)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            }));
        }
        for _ in 0..PARKED_INGEST_ADMISSION {
            fx.arrived.recv().await.expect("a handler must park");
        }

        // The state is genuinely one that sheds ingest — asserted here rather than assumed, or
        // this test would prove only that `/control/changes` works on an idle server.
        let (code, body) = post_ingest(&fx.server, "shed-me", &rows_from(950, 1), true).await;
        assert_eq!(code, 429, "the premise: ingest is being shed right now");
        assert_eq!(body["error"], "backpressure");

        const SUPPRESS_SOURCE_ID: u64 = 5;
        let external_id =
            base64::engine::general_purpose::STANDARD.encode(external_id_of(SUPPRESS_SOURCE_ID));
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            fx.server
                .client
                .post(fx.server.control_url("/control/changes"))
                .bearer_auth(OPERATOR_CREDENTIAL)
                .json(&serde_json::json!([{ "external_id": external_id, "op": "suppress" }]))
                .send(),
        )
        .await
        .expect(
            "a suppression did not answer within 20s while ingest was saturated — it is queued \
             behind ingest work, which is lifecycle §1.3's forbidden shape",
        )
        .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "/control/changes is NEVER load-shed (contracts §3.1): refusing a security operation \
             for load is fail-open, where batching one for latency is not"
        );

        fx.release();
        for task in parked {
            assert_eq!(task.await.unwrap(), 200);
        }
    });
}

/// **The queue-full 429, end to end** — Task 3b deferred this here by name.
///
/// The queue is made full **deterministically, with no sleep and no fault injection**: a work queue
/// of bound `0` is a `std::sync::mpsc::sync_channel(0)`, a rendezvous, and `Executor::run` takes
/// work with `try_recv` and blocks only on the doorbell — so there is never a receiver waiting at
/// the rendezvous and `try_send` returns `Full` every time. `config.rs` refuses `0` for an
/// operator, so this spelling is reachable only through `mount_server`, which is what that seam was
/// split out for.
///
/// **What this proves, and what it does not.** It proves the whole path from `TrySendError::Full`
/// through `SubmitError::QueueFull`, `map_accept_error`, `ApiError::WriteBackpressure` and onto the
/// wire, including the derived `retry_after_s` and its agreeing header. It does **not** prove
/// anything about the *transition* into fullness — a queue that is never not full cannot
/// distinguish "429 on Full" from "429 always". Inducing a real transition needs a controllable
/// stall inside the executor, i.e. the `fault-injection` dev-dependency this stage deliberately
/// declines for `tessera-server` (four in-tree statements say nothing outside `tessera-lifecycle`
/// and `tessera-engine`'s tests may depend on it).
///
/// **The discriminator is the body, not the status**, because the admission 429 shares the status.
/// This fixture's admission bound is 8 and one request is in flight, so the admission bound cannot
/// be the one firing.
///
/// **Mutations this kills:** mapping `QueueFull` to anything but 429; deleting `retry_after_s`'s
/// `WriteBackpressure` arm in `error.rs` (the header disappears); `estimate_retry_after_s`
/// returning 0.
#[tokio::test]
async fn ingest_429s_when_the_queue_is_full() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let mut engine = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Passthrough::new(),
        default_engine_config(),
    )
    .unwrap();
    engine
        .start_write_executor(0)
        .expect("the write executor starts once");
    let server = mount_server_with_ingest_limits(
        engine,
        200,
        generous_test_gate(),
        IngestLimits {
            admission: 8,
            max_batch_rows: 200_000,
            max_batch_bytes: 64 * 1024 * 1024,
        },
    )
    .await;

    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "queue-full")
        .header("content-type", "application/octet-stream")
        .body(build_ingest_batch(&rows_from(700, 1)))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 429, "a full work queue is 429, never 500");
    let retry_after_header: u64 = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .expect("contracts §3.1 (r10): EVERY 429 carries Retry-After")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "backpressure");
    assert!(
        body["detail"].as_str().unwrap().contains("write queue"),
        "this must be the QUEUE 429, not the admission bound's (8 permits, one request): {body}"
    );
    assert_eq!(
        body["retry_after_s"].as_u64().unwrap(),
        retry_after_header,
        "§0.3 deviation 11: the header and the body field must carry the SAME number"
    );
    // Nothing has ever completed on this server, so the estimator has no observation at all and
    // answers its floor. That is the documented no-evidence case, not a measurement.
    assert_eq!(retry_after_header, 1);

    // The refusal cost nothing durable.
    let status = control_status(&server).await;
    assert_eq!(status["write_executor"]["work_submitted"], 0);
    assert_eq!(status["write_executor"]["wal_appends"], 0);
    assert_eq!(status["write_executor"]["work_depth"], 0);

    // And the never-shed lane is unaffected in the very same state.
    const SUPPRESS_SOURCE_ID: u64 = 6;
    let external_id =
        base64::engine::general_purpose::STANDARD.encode(external_id_of(SUPPRESS_SOURCE_ID));
    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&serde_json::json!([{ "external_id": external_id, "op": "suppress" }]))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "the deny lane is unbounded; a full *work* queue must never reach it"
    );
}

/// **The row cap costs no queue slot and no WAL append**, asserted on the counters rather than on
/// the status alone.
///
/// The baseline is deliberately **non-zero**: one valid batch is ingested first and
/// `/control/status` is read, so "unchanged" is a statement about a working server rather than one
/// a server that cannot ingest at all would also satisfy.
///
/// The over-cap batch is **valid Arrow** and differs from the accepted one only in its row count,
/// so nothing else in `run_ingest` can be what refuses it.
///
/// **Mutations this kills:** deleting the row-cap check (the batch is accepted, so `work_submitted`
/// moves); moving the check *below* `accept_ingest` (the 422 still arrives, but both counters have
/// moved — which is the whole point of asserting them).
#[tokio::test]
async fn an_oversized_batch_is_422_not_a_queue_slot() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let mut engine = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Passthrough::new(),
        default_engine_config(),
    )
    .unwrap();
    engine.start_write_executor(1024).unwrap();
    let server = mount_server_with_ingest_limits(
        engine,
        200,
        generous_test_gate(),
        IngestLimits {
            admission: 8,
            max_batch_rows: 4,
            max_batch_bytes: 64 * 1024 * 1024,
        },
    )
    .await;

    let (code, _) = post_ingest(&server, "under-cap", &rows_from(800, 3), true).await;
    assert_eq!(code, 200, "a batch under the row cap must be accepted");

    let before = control_status(&server).await;
    assert_eq!(
        before["write_executor"]["work_submitted"], 1,
        "the baseline must be non-zero, or 'unchanged' proves nothing"
    );
    assert!(before["write_executor"]["wal_appends"].as_u64().unwrap() >= 1);

    let (code, body) = post_ingest(&server, "over-cap", &rows_from(810, 9), true).await;
    assert_eq!(
        code, 422,
        "over the row cap is 422 contract, never 429 or 500"
    );
    assert_eq!(body["error"], "contract");
    let detail = body["detail"].as_str().unwrap();
    assert!(detail.contains('9') && detail.contains('4'), "{detail}");

    let after = control_status(&server).await;
    assert_eq!(
        after["write_executor"]["work_submitted"], before["write_executor"]["work_submitted"],
        "a refused batch must cost no queue slot"
    );
    assert_eq!(
        after["write_executor"]["wal_appends"], before["write_executor"]["wal_appends"],
        "a refused batch must cost no WAL append"
    );
    assert_eq!(
        after["entity_id_high_water"],
        before["entity_id_high_water"]
    );
}

/// **The byte cap answers 422, not axum's 413** — the finding that made D1 reachable at all.
///
/// axum applies a 2 MiB default body limit to the `Bytes` extractor, well under the 16 MiB
/// `ingest_max_batch_bytes` defaults to, so before Task 6 the configured cap could never be the
/// refusal a caller met and an over-2-MiB batch got a **413**, a status outside contracts §3.1's
/// closed code list. `control::router` now sets the limit to the configured cap and the handler
/// maps the rejection itself.
///
/// The cap is derived from a real body rather than guessed, so the two legs cannot drift with
/// Arrow's framing overhead.
///
/// **Mutation:** remove the `DefaultBodyLimit` layer from `control::router` and the over-cap leg
/// gets **200** — the cap is then enforced nowhere at all, which is the pre-Task-6 state.
#[tokio::test]
async fn an_oversized_body_is_422_not_413() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let one_row = build_ingest_batch(&rows_from(820, 1));
    let cap = one_row.len();

    let mut engine = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Passthrough::new(),
        default_engine_config(),
    )
    .unwrap();
    engine.start_write_executor(1024).unwrap();
    let server = mount_server_with_ingest_limits(
        engine,
        200,
        generous_test_gate(),
        IngestLimits {
            admission: 8,
            max_batch_rows: 200_000,
            max_batch_bytes: cap,
        },
    )
    .await;

    let (code, _) = post_ingest(&server, "at-cap", &rows_from(820, 1), true).await;
    assert_eq!(code, 200, "a body exactly at the cap must be accepted");

    let big = rows_from(830, 200);
    assert!(build_ingest_batch(&big).len() > cap);
    let (code, body) = post_ingest(&server, "over-cap-bytes", &big, true).await;
    assert_eq!(
        code, 422,
        "over the byte cap must be 422 contract — 413 is not in contracts §3.1's closed code list"
    );
    assert_eq!(body["error"], "contract");
    assert!(
        body["detail"].as_str().unwrap().contains(&cap.to_string()),
        "the detail must name the bound the operator set: {body}"
    );

    let status = control_status(&server).await;
    assert_eq!(
        status["write_executor"]["work_submitted"], 1,
        "the refused body cost no queue slot"
    );
}

/// **401 ahead of every pressure signal this endpoint can emit** — the ordering rule the whole of
/// `control::ingest` is arranged around.
///
/// Five states, one ordering. An unauthenticated caller must be unable to learn, from a status code
/// alone: that this server is at its ingest-admission bound, that its work queue is full, what its
/// per-batch row cap is, or what its per-batch byte cap is. Each is checked twice — once without a
/// credential, which must be 401, and once with, which must be the pressure signal — so the test
/// cannot pass by the server simply never producing the signal.
///
/// The **oversize** legs are the ones a `body: Bytes` extractor would fail: axum's extractors run
/// before the handler body, so a rejecting extractor answers before `check_bearer` is ever reached.
/// `body: Result<Bytes, _>` is what moves that decision inside the handler and behind the
/// credential.
///
/// **Mutations this kill:** moving `check_bearer` below the body-rejection mapping, below the
/// admission check, or below the batch-id header check; taking `body: Bytes` instead of
/// `Result<Bytes, _>`.
#[tokio::test]
async fn backpressure_is_invisible_before_auth() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let one_row = build_ingest_batch(&rows_from(840, 1));
    let cap = one_row.len();

    // Server A: the admission bound is saturated by construction (`Semaphore::new(0)` has no
    // permits to give), and both 422 caps are set tight. Three of the four signals live here.
    let mut engine_a = Engine::open(
        &bundle_root,
        &tmp.path().join("cache-a"),
        &tmp.path().join("wal-a.log"),
        Passthrough::new(),
        default_engine_config(),
    )
    .unwrap();
    engine_a.start_write_executor(1024).unwrap();
    let server_a = mount_server_with_ingest_limits(
        engine_a,
        200,
        generous_test_gate(),
        IngestLimits {
            admission: 0,
            max_batch_rows: 2,
            max_batch_bytes: cap,
        },
    )
    .await;

    // 1. Admission 429, invisible without a credential.
    let (code, body) = post_ingest(&server_a, "a1", &rows_from(840, 1), false).await;
    assert_eq!(code, 401, "an unauthenticated caller must not see the 429");
    assert_eq!(body["error"], "bad-credential");
    let (code, body) = post_ingest(&server_a, "a1", &rows_from(840, 1), true).await;
    assert_eq!(code, 429, "and the signal must genuinely be there to see");
    assert_eq!(body["error"], "backpressure");

    // 2. The byte cap's 422, likewise — this is the leg an extractor-level rejection would fail.
    let big = rows_from(850, 200);
    let (code, body) = post_ingest(&server_a, "a2", &big, false).await;
    assert_eq!(
        code, 401,
        "an oversized body must be 401 without a credential, never 413 or 422: the caller would \
         otherwise learn this deployment's batch cap by bisection, unauthenticated"
    );
    assert_eq!(body["error"], "bad-credential");

    // 3. The row cap's 422. Under the byte cap, over the row cap, so the row check is what would
    //    fire.
    let (code, body) = post_ingest(&server_a, "a3", &rows_from(860, 3), false).await;
    assert_eq!(code, 401);
    assert_eq!(body["error"], "bad-credential");

    // 4. The missing-batch-id 422 — the pre-existing check, still behind the credential.
    let resp = server_a
        .client
        .post(server_a.control_url("/control/ingest"))
        .header("content-type", "application/octet-stream")
        .body(one_row.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // Server B: the *queue* 429, the one signal server A cannot produce (its admission bound
    // refuses first, which is itself the ordering under test).
    let mut engine_b = Engine::open(
        &bundle_root,
        &tmp.path().join("cache-b"),
        &tmp.path().join("wal-b.log"),
        Passthrough::new(),
        default_engine_config(),
    )
    .unwrap();
    engine_b.start_write_executor(0).unwrap();
    let server_b = mount_server_with_ingest_limits(
        engine_b,
        200,
        generous_test_gate(),
        IngestLimits {
            admission: 8,
            max_batch_rows: 200_000,
            max_batch_bytes: 64 * 1024 * 1024,
        },
    )
    .await;

    let (code, body) = post_ingest(&server_b, "b1", &rows_from(870, 1), false).await;
    assert_eq!(
        code, 401,
        "an unauthenticated caller must not see the queue 429"
    );
    assert_eq!(body["error"], "bad-credential");
    let (code, body) = post_ingest(&server_b, "b1", &rows_from(870, 1), true).await;
    assert_eq!(code, 429);
    assert!(body["detail"].as_str().unwrap().contains("write queue"));
}

/// **`overlay_soft_limit` alarms, and it does not act** (Task 6, D5). There is no fold until stage
/// 2.3, so an operator who sets this today gets a signal that the overlay is deep, never a
/// mechanism that makes it shallower — and this test asserts exactly that much and no more.
///
/// Two properties, because the check has two sites and only one of them is the executor's:
///
/// 1. **At runtime**, `Executor::apply_change` — the only place the overlay grows once the executor
///    is running — counts each publication at or above the limit.
/// 2. **At configuration**, `Engine::set_overlay_soft_limit` evaluates the predicate once as it
///    lands. That leg exists because the executor is *not* the only place an overlay is built: a
///    WAL replay builds one inside `Engine::open`, before any executor exists, so a node restarting
///    already over its limit would otherwise sit silently over it until the next deny happened to
///    arrive. Asserted here by moving the limit under a live overlay, which is the same state
///    replay produces and the only one a server test can construct.
///
/// **Mutations this kills:** deleting `apply_change`'s check (leg 1 stays at 0); deleting
/// `set_overlay_soft_limit`'s one-shot evaluation (leg 2 stays at its leg-1 value).
#[tokio::test]
async fn the_overlay_soft_limit_alarms_and_does_not_act() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let mut engine = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Passthrough::new(),
        default_engine_config(),
    )
    .unwrap();
    engine.start_write_executor(1024).unwrap();
    // Unset: `usize::MAX` is the engine's "no limit configured", which is what every embedder and
    // every test that never calls the setter gets.
    let server = mount_server(engine, 200, generous_test_gate()).await;

    let suppress = |id: u64| {
        let external_id = base64::engine::general_purpose::STANDARD.encode(external_id_of(id));
        server
            .client
            .post(server.control_url("/control/changes"))
            .bearer_auth(OPERATOR_CREDENTIAL)
            .json(&serde_json::json!([{ "external_id": external_id, "op": "suppress" }]))
            .send()
    };

    assert_eq!(suppress(11).await.unwrap().status(), 200);
    let status = control_status(&server).await;
    assert_eq!(status["overlay"]["depth"], 1);
    assert_eq!(
        status["overlay"]["soft_limit_alarms"], 0,
        "an unset limit must never alarm"
    );

    // Leg 2: the limit lands *under* a live overlay — the restart shape.
    server.state.engine.set_overlay_soft_limit(1);
    let status = control_status(&server).await;
    assert_eq!(
        status["overlay"]["soft_limit_alarms"], 1,
        "a limit set below the overlay it finds must alarm as it lands, not wait for the next deny"
    );

    // Leg 1: the executor's own check, on the next growth.
    assert_eq!(suppress(12).await.unwrap().status(), 200);
    let status = control_status(&server).await;
    assert_eq!(status["overlay"]["depth"], 2);
    assert_eq!(status["overlay"]["soft_limit_alarms"], 2);

    // **And nothing acted**: the overlay is still exactly as deep as the denies made it, and both
    // suppressions are still in force. There is no fold to shrink it and this test must not read
    // as though there were.
    assert_eq!(
        control_status(&server).await["overlay"]["depth"],
        2,
        "the alarm must not have shrunk, folded or retired anything"
    );
}
