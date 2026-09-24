//! `/control/ingest`'s `access` column, read by the rule the build reads a points file's access
//! column by.
//!
//! What this proves, against a running server: every element is one label whatever it contains,
//! so a label with a comma is one label, not two; a scalar string column and a dictionary are one
//! label per row; each label is trimmed, and a credential holding the trimmed label sees the row;
//! an empty list, a null, an empty element and a null element are all no label, which the view's
//! declared `point_visibility.default` fills and which a view declaring none refuses naming the
//! count.

mod common;

use std::path::Path;
use std::sync::Arc;

use arrow::array::{
    ArrayRef, BinaryArray, DictionaryArray, Float32Array, GenericListBuilder, LargeStringBuilder,
    ListArray, ListBuilder, StringArray, StringBuilder,
};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Int32Type, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use tempfile::TempDir;

use common::*;

const VIENNA: &str = "Natural History Museum, Vienna";
const LONDON: &str = "Natural History Museum";

/// One ingest body with an `access` column of the caller's making, so the refusal cases can spell
/// it wrong.
fn body_with_access(rows: usize, access: arrow::array::ArrayRef) -> Vec<u8> {
    body_with_access_from(100, rows, access)
}

/// [`body_with_access`] with its external ids starting `first` past the fixture's own.
fn body_with_access_from(first: u64, rows: usize, access: arrow::array::ArrayRef) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("access", access.data_type().clone(), true),
    ]));
    let ids: Vec<Vec<u8>> = (0..rows as u64)
        .map(|i| external_id_of(N_ITEMS + first + i))
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
    drain(&server).await;

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
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    let body = body_with_access(
        3,
        Arc::new(access_lists(&[&[VIENNA], &[LONDON], &["0", VIENNA]])),
    );
    let resp = ingest(&server, "list-1", body).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["accepted"], 3);
    assert_eq!(json["over_bound"], 0);
    drain(&server).await;

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

/// A scalar string column and a dictionary of strings are one whole label per row, as at the
/// build: a label with a comma in it is never split.
#[tokio::test]
async fn a_scalar_and_a_dictionary_column_are_one_label_per_row() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    let body = body_with_access(1, Arc::new(StringArray::from(vec![VIENNA])));
    let resp = ingest(&server, "scalar-1", body).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let dictionary: DictionaryArray<Int32Type> = vec![VIENNA].into_iter().collect();
    let body = body_with_access_from(200, 1, Arc::new(dictionary));
    let resp = ingest(&server, "dictionary-1", body).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    drain(&server).await;

    assert_eq!(visible_to(&server, &[VIENNA]).await, 2);
    assert_eq!(visible_to(&server, &[LONDON]).await, 0);
}

/// A padded label is stored trimmed: a credential holding the trimmed label sees the row,
/// whichever encoding carried it, and so does one holding the padded spelling, which the
/// credential side trims alike.
#[tokio::test]
async fn a_padded_label_is_stored_trimmed_on_every_encoding() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let public_before = visible_to(&server, &[]).await;
    let mut large = GenericListBuilder::<i32, _>::new(LargeStringBuilder::new());
    large.values().append_value(" red ");
    large.append(true);
    let bodies: Vec<(&str, ArrayRef)> = vec![
        ("list", Arc::new(access_lists(&[&[" red "]]))),
        ("large", Arc::new(large.finish())),
        ("scalar", Arc::new(StringArray::from(vec![" red "]))),
        (
            "dictionary",
            Arc::new(vec![" red "].into_iter().collect::<DictionaryArray<Int32Type>>()),
        ),
    ];
    for (first, (batch_id, access)) in (100..).step_by(100).zip(bodies) {
        let resp = ingest(&server, batch_id, body_with_access_from(first, 1, access)).await;
        assert_eq!(resp.status(), 200, "{batch_id}: {}", resp.text().await.unwrap());
    }
    drain(&server).await;

    assert_eq!(visible_to(&server, &["red"]).await, 4);
    assert_eq!(visible_to(&server, &[" red "]).await, 4);
    assert_eq!(visible_to(&server, &[]).await, public_before, "a label is never everyone");

    let server = restart(server, &tmp).await;
    assert_eq!(visible_to(&server, &["red"]).await, 4, "the trimmed label survives a restart");
    assert_eq!(visible_to(&server, &[]).await, public_before);
}

/// An empty list is a row with no label, and the view's declared default fills it (decision
/// 0133): under `default = "ir:sealed"` the row is served to a principal holding that term and
/// to no other, as the build serves a null or empty label under the same declaration. A row
/// carrying labels of its own is not also given the default.
#[tokio::test]
async fn an_empty_list_takes_the_views_declared_default() {
    let (_tmp, server) = serve_with_default(Some("ir:sealed")).await;
    let sealed_before = visible_to(&server, &["ir:sealed"]).await;
    let zero_before = visible_to(&server, &["0"]).await;

    let body = body_with_access(4, Arc::new(access_lists(&[&[], &["0"], &[""], &["  ", ""]])));
    let resp = ingest(&server, "empty-1", body).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["accepted"], 4);
    drain(&server).await;

    assert_eq!(
        visible_to(&server, &["ir:sealed"]).await,
        sealed_before + 3,
        "the empty list and the lists of empty labels landed under the declared default"
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
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let before = visible_to(&server, &[]).await;

    let body = body_with_access(1, Arc::new(access_lists(&[&[]])));
    let resp = ingest(&server, "empty-public", body).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    drain(&server).await;

    assert_eq!(visible_to(&server, &[]).await, before + 1);
}

/// A view declaring no default refuses the batch (decision 0133), naming the count of unlabelled
/// rows and the view, in the terms the build refuses the same corpus; the batch has no effect.
/// A list of one empty label is no label, and is refused alike.
#[tokio::test]
async fn an_empty_list_is_refused_naming_the_count_where_no_default_is_declared() {
    let (_tmp, server) = serve_with_default(None).await;
    let high_water_before = control_status(&server).await["entity_id_high_water"].clone();

    let body = body_with_access(3, Arc::new(access_lists(&[&[], &["0"], &[]])));
    let resp = ingest(&server, "empty-refused", body).await;
    assert_eq!(resp.status(), 422);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "contract", "{body}");
    let detail = body["detail"].as_str().unwrap();
    assert!(mentions(detail, "2") && mentions(detail, "s0"), "the count and the view: {detail}");

    // A labelled batch on the same view is unaffected: the refusal is about the rows.
    let body = body_with_access(1, Arc::new(access_lists(&[&["0"]])));
    let resp = ingest(&server, "labelled", body).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    // An empty element is no label.
    let body = body_with_access(1, Arc::new(access_lists(&[&[""]])));
    let resp = ingest(&server, "empty-element", body).await;
    assert_eq!(resp.status(), 422);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "contract", "{body}");

    let json = control_status(&server).await;
    assert_eq!(
        json["entity_id_high_water"].as_u64().unwrap(),
        high_water_before.as_u64().unwrap() + 1,
        "only the labelled batch had an effect"
    );
}

/// **The four cases at the Arrow door** (decision 0133): a null list cell, an empty list and an
/// empty element are one case, a row with no label, filled by a declared default and refused with
/// the count where none is declared; the column absent from the batch is refused at the schema.
/// The JSON door's four are in `ingest_wire.rs`.
#[tokio::test]
async fn a_null_cell_and_an_empty_list_are_one_case_at_the_arrow_door() {
    fn null_and_empty() -> Vec<u8> {
        let mut lists = ListBuilder::new(StringBuilder::new());
        lists.append(false);
        lists.append(true);
        body_with_access(2, Arc::new(lists.finish()))
    }
    let (_tmp, server) = serve_with_default(Some("ir:sealed")).await;
    let before = visible_to(&server, &["ir:sealed"]).await;
    let resp = ingest(&server, "filled", null_and_empty()).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    drain(&server).await;
    assert_eq!(
        visible_to(&server, &["ir:sealed"]).await,
        before + 2,
        "the null cell and the empty list both landed under the declared default"
    );

    let (_tmp, server) = serve_with_default(None).await;
    let high_water_before = control_status(&server).await["entity_id_high_water"].clone();
    let resp = ingest(&server, "refused", null_and_empty()).await;
    assert_eq!(resp.status(), 422);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "contract", "{body}");
    let detail = body["detail"].as_str().unwrap();
    assert!(
        mentions(detail, "2"),
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
    assert_eq!(resp.status(), 422, "an empty element is no label, and no default is declared");

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

/// A null element is no label and is dropped, as at the build; the row keeps its other labels.
#[tokio::test]
async fn a_null_element_is_dropped() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let zero_before = visible_to(&server, &["0"]).await;

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
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    drain(&server).await;

    assert_eq!(visible_to(&server, &["0"]).await, zero_before + 1);
}
