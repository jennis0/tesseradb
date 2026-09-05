//! `/control/ingest`'s `access` column is a **list of labels**, one element per label, taken
//! verbatim (contracts §3.4, decision 0129).
//!
//! What this proves, against a running server: a list column ingests and every element lands as
//! one term whatever it contains — a label with a comma is one label, not two; a scalar `utf8`
//! column is refused at the schema naming the column and the shape it takes; an empty list is a
//! row with no label, accepted and visible to nobody; a null list and a null element are refused
//! naming the row.
//!
//! The comma case is the one that found the defect: a corpus whose compartment keys are free text
//! (`Natural History Museum, Vienna`) split on a wire that carried one string per row, and the
//! fragments were minted as terms — some of them the names of other compartments.

mod common;

use std::sync::Arc;

use arrow::array::{BinaryArray, Float32Array, ListBuilder, StringArray, StringBuilder};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use tempfile::TempDir;

use common::*;

const VIENNA: &str = "Natural History Museum, Vienna";
const LONDON: &str = "Natural History Museum";

/// One ingest body with an `access` column of the caller's making, so the refusal cases can spell
/// it wrong.
fn body_with_access(rows: usize, access: arrow::array::ArrayRef) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("access", access.data_type().clone(), true),
    ]));
    let ids: Vec<Vec<u8>> = (0..rows as u64)
        .map(|i| external_id_of(N_ITEMS + 100 + i))
        .collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter_values(
                ids.iter().map(|v| v.as_slice()),
            )),
            Arc::new(Float32Array::from_iter_values(
                (0..rows).map(|i| 10.0 + i as f32),
            )),
            Arc::new(Float32Array::from_iter_values(
                (0..rows).map(|i| 10.0 + i as f32),
            )),
            access,
        ],
    )
    .unwrap();
    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

async fn ingest(server: &TestServer, batch_id: &str, body: Vec<u8>) -> reqwest::Response {
    server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap()
}

/// The whole-extent count a principal holding exactly `terms` is served.
async fn visible_to(server: &TestServer, terms: &[&str]) -> u64 {
    let auth = authorise(server, terms).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let (tiles, _) = decode_viewport(&resp.bytes().await.unwrap());
    tiles.iter().map(|t| t.1).sum()
}

/// `POST /control/flush`, waited for: an ingested row is served once its flush has published.
async fn flush(server: &TestServer) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let before = server.state.engine.write_executor_stats().flushes;
        let resp = server
            .client
            .post(server.control_url("/control/flush"))
            .bearer_auth(OPERATOR_CREDENTIAL)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        while server.state.engine.write_executor_stats().flushes == before {
            assert!(
                std::time::Instant::now() < deadline,
                "the flush never published"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        if server.state.engine.buffered_items() == 0 {
            break;
        }
    }
}

async fn served() -> (TempDir, TestServer) {
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
    (tmp, server)
}

/// Every element is one label, verbatim. A label containing a comma is one term: the rows
/// labelled `Natural History Museum, Vienna` are visible to a principal holding that label and
/// to nobody holding `Natural History Museum`, which a comma grammar would have made its first
/// fragment.
#[tokio::test]
async fn a_list_column_ingests_and_each_element_is_one_label_verbatim() {
    let (_tmp, server) = served().await;

    let body = body_with_access(
        3,
        Arc::new(access_lists(&[&[VIENNA], &[LONDON], &["0", VIENNA]])),
    );
    let resp = ingest(&server, "list-1", body).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["accepted"], 3);
    assert_eq!(json["over_bound"], 0);
    flush(&server).await;

    assert_eq!(
        visible_to(&server, &[VIENNA]).await,
        2,
        "rows 0 and 2 carry the label whole"
    );
    assert_eq!(
        visible_to(&server, &[LONDON]).await,
        1,
        "row 1 alone: no fragment of row 0's or row 2's label landed in this compartment"
    );
    // Row 2 carries two labels, so it is also in `0`'s compartment beside the fixture's own rows.
    let fixture_zero = (0..N_ITEMS).filter(|e| terms_of(*e).contains(&0)).count() as u64;
    assert_eq!(visible_to(&server, &["0"]).await, fixture_zero + 1);
}

/// A scalar `utf8` column is the shape a separator grammar lived in, and it is refused at the
/// schema — whole batch, no effect — naming the column and the shape it takes.
#[tokio::test]
async fn a_scalar_utf8_access_column_is_refused_naming_the_column_and_the_shape() {
    let (_tmp, server) = served().await;
    let high_water_before = control_status(&server).await["entity_id_high_water"].clone();

    let body = body_with_access(1, Arc::new(StringArray::from(vec![VIENNA])));
    let resp = ingest(&server, "scalar-1", body).await;
    assert_eq!(resp.status(), 422);
    let detail = resp.text().await.unwrap();
    assert!(detail.contains("column 'access'"), "{detail}");
    assert!(detail.contains("utf8, one string per row"), "{detail}");
    assert!(detail.contains("list<utf8>"), "{detail}");
    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water_before,
        "a refused batch has no effect"
    );
}

/// An empty list is a row with no label: accepted, and visible to no principal.
#[tokio::test]
async fn an_empty_list_is_a_row_with_no_label() {
    let (_tmp, server) = served().await;
    let before = visible_to(&server, &["0"]).await;

    let body = body_with_access(1, Arc::new(access_lists(&[&[]])));
    let resp = ingest(&server, "empty-1", body).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["accepted"], 1);
    flush(&server).await;

    assert_eq!(
        visible_to(&server, &["0"]).await,
        before,
        "no label, so no principal sees it"
    );
}

/// A null list could mean "no label" or "not supplied", and a null element has no bytes to be a
/// label; both are refused naming the row, and the batch has no effect.
#[tokio::test]
async fn a_null_list_and_a_null_element_are_refused_naming_the_row() {
    let (_tmp, server) = served().await;
    let high_water_before = control_status(&server).await["entity_id_high_water"].clone();

    let mut null_list = ListBuilder::new(StringBuilder::new());
    null_list.values().append_value("0");
    null_list.append(true);
    null_list.append(false);
    let resp = ingest(
        &server,
        "null-list",
        body_with_access(2, Arc::new(null_list.finish())),
    )
    .await;
    assert_eq!(resp.status(), 422);
    let detail = resp.text().await.unwrap();
    assert!(
        detail.contains("column 'access' is null at row 1"),
        "{detail}"
    );

    let mut null_element = ListBuilder::new(StringBuilder::new());
    null_element.values().append_value("0");
    null_element.values().append_null();
    null_element.append(true);
    let resp = ingest(
        &server,
        "null-element",
        body_with_access(1, Arc::new(null_element.finish())),
    )
    .await;
    assert_eq!(resp.status(), 422);
    let detail = resp.text().await.unwrap();
    assert!(detail.contains("null element at row 0"), "{detail}");

    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water_before,
        "neither refused batch had any effect"
    );
}
