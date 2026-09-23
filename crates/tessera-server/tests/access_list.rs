//! `/control/ingest`'s `access` column is a **list of labels**, one element per label, taken
//! verbatim (contracts §3.4, decision 0129).
//!
//! What this proves, against a running server: a list column ingests and every element lands as
//! one term whatever it contains — a label with a comma is one label, not two; a scalar `utf8`
//! column is refused at the schema naming the column and the shape it takes; an empty list is a
//! row with no label, which the view's declared `point_visibility.default` fills and which a view
//! declaring none refuses naming the count (decision 0133); a null list and a null element are
//! refused naming the row.
//!
//! The comma case is the one that found the defect: a corpus whose compartment keys are free text
//! (`Natural History Museum, Vienna`) split on a wire that carried one string per row, and the
//! fragments were minted as terms — some of them the names of other compartments.

mod common;

use std::path::Path;
use std::sync::Arc;

use arrow::array::{
    ArrayRef, BinaryArray, Float32Array, ListArray, ListBuilder, StringArray, StringBuilder,
};
use arrow::buffer::OffsetBuffer;
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
        .header("content-type", "application/vnd.apache.arrow.stream")
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
    let server = serve(&tmp).await;
    (tmp, server)
}

/// The same fixture under a `point_visibility` declaring `default`, or none.
async fn served_with_default(default: Option<&str>) -> (TempDir, TestServer) {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    let pairs = tmp.path().join("pairs.parquet");
    build_fixture_with_access(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &pairs,
        N_ITEMS,
        tessera_build::config::AccessInput {
            source: tessera_build::config::AccessSource::Relation(pairs.clone()),
            default: default.map(str::to_string),
        },
    );
    let server = open(&tmp).await;
    (tmp, server)
}

/// A points file carrying its own `list<string>` access column, `N_ITEMS` rows: every row
/// labelled `ir:analyst` except `NULL_ROW`, whose label is null. The field-sourced shape, which
/// is the one the build fills a null label on.
const NULL_ROW: u64 = 5;

fn write_field_points(path: &Path) {
    let ids: Vec<u64> = (0..N_ITEMS).collect();
    let mut offsets: Vec<i32> = vec![0];
    let mut flat: Vec<&str> = Vec::new();
    let mut present: Vec<bool> = Vec::new();
    for &e in &ids {
        if e == NULL_ROW {
            present.push(false);
        } else {
            present.push(true);
            flat.push("ir:analyst");
        }
        offsets.push(flat.len() as i32);
    }
    let values: ArrayRef = Arc::new(StringArray::from(flat));
    let list = ListArray::new(
        Arc::new(Field::new("item", DataType::Utf8, true)),
        OffsetBuffer::new(offsets.into()),
        values,
        Some(present.into()),
    );
    write_points(path, &ids, scatter, vec![column("categories", true, list)]);
}

/// A server over a field-sourced view (`point_visibility = { field = "categories", default }`)
/// built from [`write_field_points`].
async fn served_field_sourced(default: &str) -> (TempDir, TestServer) {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    write_field_points(&points);
    let access = AccessInput {
        source: tessera_build::config::AccessSource::Field("categories".to_string()),
        default: Some(default.to_string()),
    };
    tessera_build::build(&build_args(
        &bundle_root,
        vec![view_args("s0", &points, access)],
    ))
    .expect("a field-sourced view with a null row builds under a declared default");
    let server = open(&tmp).await;
    (tmp, server)
}

/// **The two doors agree on a field-sourced view**, which is the case decision 0133 exists for:
/// the build filled its null-label row with the declared default, and an ingested row with an
/// empty label list lands under the same default. Both are served to a holder of that term and
/// to nobody else; the rows labelled by the file are untouched by either fill.
#[tokio::test]
async fn a_field_sourced_view_fills_a_null_label_and_an_empty_list_alike() {
    let (_tmp, server) = served_field_sourced("ir:sealed").await;

    // The build's fill: one row, `NULL_ROW`, reachable by the default's term alone.
    assert_eq!(visible_to(&server, &["ir:sealed"]).await, 1);
    assert_eq!(visible_to(&server, &["ir:analyst"]).await, N_ITEMS - 1);
    assert_eq!(visible_to(&server, &[]).await, 0);

    let body = body_with_access(1, Arc::new(access_lists(&[&[]])));
    let resp = ingest(&server, "field-empty", body).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    flush(&server).await;

    // The ingest door's fill: the same term now reaches both rows, and no other principal
    // gained one.
    assert_eq!(visible_to(&server, &["ir:sealed"]).await, 2);
    assert_eq!(visible_to(&server, &["ir:analyst"]).await, N_ITEMS - 1);
    assert_eq!(visible_to(&server, &[]).await, 0);
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

/// An empty list is a row with no label, and the view's declared default fills it (decision
/// 0133): under `default = "ir:sealed"` the row is served to a principal holding that term and
/// to no other, as the build serves a null or empty label under the same declaration. A row
/// carrying labels of its own is not also given the default.
#[tokio::test]
async fn an_empty_list_takes_the_views_declared_default() {
    let (_tmp, server) = served_with_default(Some("ir:sealed")).await;
    let sealed_before = visible_to(&server, &["ir:sealed"]).await;
    let zero_before = visible_to(&server, &["0"]).await;

    let body = body_with_access(2, Arc::new(access_lists(&[&[], &["0"]])));
    let resp = ingest(&server, "empty-1", body).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["accepted"], 2);
    flush(&server).await;

    assert_eq!(
        visible_to(&server, &["ir:sealed"]).await,
        sealed_before + 1,
        "the unlabelled row landed under the declared default"
    );
    assert_eq!(
        visible_to(&server, &["0"]).await,
        zero_before + 1,
        "the labelled row kept its own label and was not also given the default"
    );
    assert_eq!(
        visible_to(&server, &[]).await,
        0,
        "the default is a label, never everyone"
    );
}

/// Under `default = "public"` the same row is public: the fixture every other test here runs on
/// declares that, so an empty list there is a row every principal sees.
#[tokio::test]
async fn an_empty_list_under_a_public_default_is_public() {
    let (_tmp, server) = served().await;
    let before = visible_to(&server, &[]).await;

    let body = body_with_access(1, Arc::new(access_lists(&[&[]])));
    let resp = ingest(&server, "empty-public", body).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    flush(&server).await;

    assert_eq!(visible_to(&server, &[]).await, before + 1);
}

/// A view declaring no default refuses the batch (decision 0133), naming the count of unlabelled
/// rows and the view, in the terms the build refuses the same corpus; the batch has no effect.
/// An empty *element* stays refused as it was, whatever the view declares.
#[tokio::test]
async fn an_empty_list_is_refused_naming_the_count_where_no_default_is_declared() {
    let (_tmp, server) = served_with_default(None).await;
    let high_water_before = control_status(&server).await["entity_id_high_water"].clone();

    let body = body_with_access(3, Arc::new(access_lists(&[&[], &["0"], &[]])));
    let resp = ingest(&server, "empty-refused", body).await;
    assert_eq!(resp.status(), 422);
    let detail = resp.text().await.unwrap();
    assert!(
        detail.contains("view 's0': 2 row(s) carry an empty access label"),
        "{detail}"
    );
    assert!(
        detail.contains("declares no `point_visibility.default`"),
        "{detail}"
    );

    // A labelled batch on the same view is unaffected: the refusal is about the rows.
    let body = body_with_access(1, Arc::new(access_lists(&[&["0"]])));
    let resp = ingest(&server, "labelled", body).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    // An empty element is not a label, on either declaration.
    let body = body_with_access(1, Arc::new(access_lists(&[&[""]])));
    let resp = ingest(&server, "empty-element", body).await;
    assert_eq!(resp.status(), 422);
    let detail = resp.text().await.unwrap();
    assert!(detail.contains("access column"), "{detail}");

    let json = control_status(&server).await;
    assert_eq!(
        json["entity_id_high_water"].as_u64().unwrap(),
        high_water_before.as_u64().unwrap() + 1,
        "only the labelled batch had an effect"
    );
}

/// **The four cases at the Arrow door** (decision 0133): a null list cell and an empty list are
/// one case, a row with no label, filled by a declared default and refused with the count where
/// none is declared; an empty element is refused as no label at all; the column absent from the
/// batch is refused at the schema. The JSON door's four are in `ingest_wire.rs`.
#[tokio::test]
async fn a_null_cell_and_an_empty_list_are_one_case_at_the_arrow_door() {
    fn null_and_empty() -> Vec<u8> {
        let mut lists = ListBuilder::new(StringBuilder::new());
        lists.append(false);
        lists.append(true);
        body_with_access(2, Arc::new(lists.finish()))
    }
    let (_tmp, server) = served_with_default(Some("ir:sealed")).await;
    let before = visible_to(&server, &["ir:sealed"]).await;
    let resp = ingest(&server, "filled", null_and_empty()).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    flush(&server).await;
    assert_eq!(
        visible_to(&server, &["ir:sealed"]).await,
        before + 2,
        "the null cell and the empty list both landed under the declared default"
    );

    let (_tmp, server) = served_with_default(None).await;
    let high_water_before = control_status(&server).await["entity_id_high_water"].clone();
    let resp = ingest(&server, "refused", null_and_empty()).await;
    assert_eq!(resp.status(), 422);
    let detail = resp.text().await.unwrap();
    assert!(
        detail.contains("2 row(s)") && detail.contains("declares no `point_visibility.default`"),
        "the refusal counts the null cell with the empty list: {detail}"
    );

    let mut empty_element = ListBuilder::new(StringBuilder::new());
    empty_element.values().append_value("");
    empty_element.append(true);
    let resp = ingest(
        &server,
        "empty-element",
        body_with_access(1, Arc::new(empty_element.finish())),
    )
    .await;
    assert_eq!(resp.status(), 422, "an empty element is no label at all");

    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter_values([external_id_of(
                N_ITEMS + 900,
            )])),
            Arc::new(Float32Array::from_iter_values([10.0])),
            Arc::new(Float32Array::from_iter_values([10.0])),
        ],
    )
    .unwrap();
    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    let resp = ingest(&server, "absent", writer.into_inner().unwrap()).await;
    assert_eq!(resp.status(), 422);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "contract", "{body}");
    assert!(body["detail"].as_str().unwrap().contains("'access'"), "{body}");
    assert_eq!(
        control_status(&server).await["entity_id_high_water"],
        high_water_before,
        "no refused batch had any effect"
    );
}

/// A null element has no bytes to be a label; it is refused naming the row, and the batch has no
/// effect.
#[tokio::test]
async fn a_null_element_is_refused_naming_the_row() {
    let (_tmp, server) = served().await;
    let high_water_before = control_status(&server).await["entity_id_high_water"].clone();

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
        "the refused batch had no effect"
    );
}
