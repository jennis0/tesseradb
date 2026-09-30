//! **A membership names its members in a table**: columns keyed by `tessera_id` and unique field
//! names, one row per member, resolved by the identity rule. A member naming no item, or two, is
//! left out and listed in the answer's `refused`, or, under `strict=true`, refuses the request
//! with nothing written. A content's generating set is refused whole at any such member, strict or
//! not. The Arrow growth carries the table as `list<struct>`.

mod common;

use std::sync::Arc;

use arrow::array::{ArrayRef, ListArray, RecordBatch, StringArray, StructArray, UInt64Array};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Fields, Schema};
use arrow::ipc::writer::StreamWriter;
use common::*;
use serde_json::{json, Value};
use tempfile::TempDir;

const LAYER: &str = "clusters/tables";

fn artifacts_url(server: &TestServer, strict: bool) -> String {
    let query = if strict { "?strict=true" } else { "" };
    server.control_url(&format!(
        "/control/layers/{}/artifacts{query}",
        LAYER.replace('/', "%2F")
    ))
}

async fn put(server: &TestServer, strict: bool, artifacts: Value) -> (u16, Value) {
    let resp = server
        .client
        .put(artifacts_url(server, strict))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "artifacts": artifacts }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.json().await.unwrap_or(Value::Null);
    if status < 300 {
        tick(server).await;
    }
    (status, body)
}

async fn patch_arrow(server: &TestServer, body: Vec<u8>) -> (u16, Value) {
    let resp = server
        .client
        .patch(artifacts_url(server, false))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.json().await.unwrap_or(Value::Null);
    if status < 300 {
        tick(server).await;
    }
    (status, body)
}

/// The member counts the broad principal is shown for the layer's artifacts, ascending, having
/// checked that `served` artifacts are served.
async fn count(server: &TestServer, served: usize) -> Vec<u64> {
    let token = token_for(server, &["0"]).await;
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200,
            "layers": [LAYER],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let rows = decode_viewport_frames(&resp.bytes().await.unwrap())
        .artifacts
        .unwrap_or_default();
    assert_eq!(rows.len(), served, "{rows:?}");
    let mut counts: Vec<u64> = rows.iter().map(|row| row.masked_count).collect();
    counts.sort_unstable();
    counts
}

/// Members named by `tessera_id`, by a unique field, and by both agreeing, in one table.
#[tokio::test]
async fn a_table_names_members_by_any_identifier_they_hold() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, flat_layer(LAYER)).await;
    let (zero, two) = (tessera_id_of(&server, 0), tessera_id_of(&server, 2));

    let (status, body) = put(
        &server,
        false,
        json!([{
            "key": "a",
            "members": {
                "tessera_id": [zero.to_string(), null, two.to_string()],
                "id": [null, "1", 2],
            },
        }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["joined"], 3, "{body}");
    assert_eq!(body["refused"], json!([]));
    assert_eq!(count(&server, 1).await, vec![3]);
}

/// A member naming no item, or two, is left out and listed; under `strict=true` the request is
/// `404` or `409` and nothing is published.
#[tokio::test]
async fn a_member_naming_no_item_or_two_is_left_out_or_refuses_a_strict_request() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, flat_layer(LAYER)).await;
    let three = tessera_id_of(&server, 3);

    let nothing = json!([{ "key": "a", "members": { "id": [0, 999_999] } }]);
    let two = json!([{
        "key": "a",
        "members": { "tessera_id": [null, three.to_string()], "id": [0, 4] },
    }]);
    let (status, body) = put(&server, true, nothing).await;
    assert_eq!(status, 404, "{body}");
    let (status, body) = put(&server, true, two).await;
    assert_eq!(status, 409, "{body}");
    count(&server, 0).await;

    let (status, body) = put(
        &server,
        false,
        json!([
            { "key": "a", "members": { "id": [0, 999_999, 5, null] } },
            {
                "key": "b",
                "members": { "tessera_id": [three.to_string(), three.to_string()], "id": [4, 3] },
            },
        ]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        body["refused"],
        json!([
            { "artifact": 0, "list": "members", "row": 1, "reason": "names_no_item" },
            { "artifact": 0, "list": "members", "row": 3, "reason": "names_no_item" },
            { "artifact": 1, "list": "members", "row": 0, "reason": "names_two_items" },
        ])
    );
    assert_eq!(count(&server, 2).await, vec![1, 2]);
}

/// A content's generating set is refused whole at a member naming nothing, strict or not: a
/// viewer must see the whole set to be served the content.
#[tokio::test]
async fn a_generating_set_member_naming_nothing_refuses_the_request() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let mut layer = flat_layer(LAYER);
    layer["content"]["supplied"] =
        json!([{ "name": "topic", "type": "text", "require_member_visibility": "all" }]);
    register(&server, layer).await;

    let (status, body) = put(
        &server,
        false,
        json!([{
            "key": "a",
            "members": { "id": [0, 1] },
            "content": [{ "values": ["t"], "generated_from": { "id": [0, 999_999] } }],
        }]),
    )
    .await;
    assert_eq!(status, 404, "{body}");
    count(&server, 0).await;
}

/// A column that is neither `tessera_id` nor a unique field names nothing: it is ignored whatever
/// its cells hold, as a build ignores it, and the answer names it.
#[tokio::test]
async fn a_column_that_names_nothing_is_ignored_and_named() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, flat_layer(LAYER)).await;
    let (status, body) = put(
        &server,
        false,
        json!([{
            "key": "a",
            "members": {
                "id": [0, 1],
                "name": ["x", "y"],
                "tags": [[1, "b"], { "c": 2 }],
                "weight": [1.5, true],
            },
        }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["ignored_columns"], json!(["name", "tags", "weight"]));
    assert_eq!(body["refused"], json!([]));
    assert_eq!(count(&server, 1).await, vec![2]);
}

/// Members joining a generating set through a growth are refused whole in the same way, strict or
/// not; a member leaving it that names nothing is listed and the rest apply.
#[tokio::test]
async fn a_generating_set_grown_by_a_member_naming_nothing_or_two_refuses_the_request() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let mut layer = flat_layer(LAYER);
    layer["content"]["supplied"] =
        json!([{ "name": "topic", "type": "text", "require_member_visibility": "all" }]);
    register(&server, layer).await;
    let (status, body) = put(
        &server,
        false,
        json!([{
            "key": "a",
            "members": { "id": [0, 1, 2] },
            "content": [{ "values": ["t"], "generated_from": { "id": [0, 1] } }],
        }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    let patch = |body: Value| {
        server
            .client
            .patch(artifacts_url(&server, false))
            .bearer_auth(OPERATOR_CREDENTIAL)
            .json(&body)
            .send()
    };
    let three = tessera_id_of(&server, 3);
    for (members, status) in [
        (json!({ "id": [2, 999_999] }), 404),
        (
            json!({ "id": [2, 4], "tessera_id": [null, three.to_string()] }),
            409,
        ),
    ] {
        let resp = patch(json!({ "artifacts": [{ "key": "a", "rank": 0, "members": members }] }))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), status);
    }
    let resp = patch(json!({ "artifacts": [
        { "key": "a", "rank": 0, "leaving": { "id": [1, 999_999] } }
    ] }))
    .await
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["artifacts"][0]["left"], 1, "{body}");
    assert_eq!(
        body["refused"],
        json!([{ "artifact": 0, "list": "leaving", "row": 1, "reason": "names_no_item" }])
    );
}

/// The shape of a table is checked before anything is resolved.
#[tokio::test]
async fn a_malformed_table_is_refused_whole() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, flat_layer(LAYER)).await;
    for (what, artifacts) in [
        (
            "columns of different lengths",
            json!([{ "key": "a", "members": { "id": [0, 1], "tessera_id": [null] } }]),
        ),
        (
            "no column that names items",
            json!([{ "key": "a", "members": { "name": ["x"] } }]),
        ),
        (
            "a list in place of a table",
            json!([{ "key": "a", "members": ["0"] }]),
        ),
        (
            "a tessera_id that is a number",
            json!([{ "key": "a", "members": { "tessera_id": [12] } }]),
        ),
        (
            "a fraction for an integer field",
            json!([{ "key": "a", "members": { "id": [0, 1.5] } }]),
        ),
    ] {
        let (status, body) = put(&server, false, artifacts).await;
        assert_eq!(status, 422, "{what}: {body}");
    }
    let resp = server
        .client
        .put(artifacts_url(&server, false))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "field": "id", "artifacts": [{ "key": "a", "members": { "id": [0] } }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 422, "the batch-level field is gone");
    count(&server, 0).await;
}

/// An Arrow growth names its members as `list<struct>`, the struct's fields the table's columns,
/// and lists a member naming nothing as the JSON form does.
#[tokio::test]
async fn an_arrow_growth_names_members_as_a_list_of_structs() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, flat_layer(LAYER)).await;
    let (status, body) = put(
        &server,
        false,
        json!([{ "key": "a", "members": { "id": [0] } }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    let one = tessera_id_of(&server, 1);
    let fields = Fields::from(vec![
        Field::new("tessera_id", DataType::Utf8, true),
        Field::new("id", DataType::UInt64, true),
    ]);
    let elements = StructArray::new(
        fields.clone(),
        vec![
            Arc::new(StringArray::from(vec![Some(one.to_string()), None, None])) as ArrayRef,
            Arc::new(UInt64Array::from(vec![None, Some(2), Some(999_999)])) as ArrayRef,
        ],
        None,
    );
    let item = Arc::new(Field::new("item", DataType::Struct(fields), false));
    let members = ListArray::new(
        item.clone(),
        OffsetBuffer::from_lengths([3]),
        Arc::new(elements),
        None,
    );
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("members", DataType::List(item), false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(vec!["a"])), Arc::new(members)],
    )
    .unwrap();
    let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();

    let (status, body) = patch_arrow(&server, writer.into_inner().unwrap()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["artifacts"][0]["joined"], 2, "{body}");
    assert_eq!(
        body["refused"],
        json!([{ "artifact": 0, "list": "members", "row": 2, "reason": "names_no_item" }])
    );
    assert_eq!(count(&server, 1).await, vec![3]);
}

/// An Arrow member struct's columns are checked only where they name items: a unique field of
/// another type than its values, and a column named twice, are refused; a column naming nothing is
/// ignored whatever its type; and a `tessera_id` column may be uint64, as a read sends it.
#[tokio::test]
async fn an_arrow_member_column_is_checked_only_where_it_names_items() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, flat_layer(LAYER)).await;
    let (status, body) = put(
        &server,
        false,
        json!([{ "key": "a", "members": { "id": [0] } }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    let body_of = |fields: Vec<Field>, columns: Vec<ArrayRef>| {
        let rows = columns[0].len();
        let fields = Fields::from(fields);
        let elements = StructArray::new(fields.clone(), columns, None);
        let item = Arc::new(Field::new("item", DataType::Struct(fields), false));
        let members = ListArray::new(
            item.clone(),
            OffsetBuffer::from_lengths([rows]),
            Arc::new(elements),
            None,
        );
        let schema = Arc::new(Schema::new(vec![
            Field::new("key", DataType::Utf8, false),
            Field::new("members", DataType::List(item), false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(StringArray::from(vec!["a"])), Arc::new(members)],
        )
        .unwrap();
        let mut writer = StreamWriter::try_new(Vec::new(), &schema).unwrap();
        writer.write(&batch).unwrap();
        writer.into_inner().unwrap()
    };
    let float = body_of(
        vec![Field::new("id", DataType::Float64, true)],
        vec![Arc::new(arrow::array::Float64Array::from(vec![1.0])) as ArrayRef],
    );
    let (status, body) = patch_arrow(&server, float).await;
    assert_eq!(status, 422, "{body}");
    let twice = body_of(
        vec![
            Field::new("id", DataType::UInt64, true),
            Field::new("id", DataType::UInt64, true),
        ],
        vec![
            Arc::new(UInt64Array::from(vec![1])) as ArrayRef,
            Arc::new(UInt64Array::from(vec![2])) as ArrayRef,
        ],
    );
    let (status, body) = patch_arrow(&server, twice).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(count(&server, 1).await, vec![1]);

    let ignored = body_of(
        vec![
            Field::new("id", DataType::UInt64, true),
            Field::new("weight", DataType::Float64, true),
            Field::new("seen", DataType::Boolean, true),
        ],
        vec![
            Arc::new(UInt64Array::from(vec![1])) as ArrayRef,
            Arc::new(arrow::array::Float64Array::from(vec![0.5])) as ArrayRef,
            Arc::new(arrow::array::BooleanArray::from(vec![true])) as ArrayRef,
        ],
    );
    let (status, body) = patch_arrow(&server, ignored).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ignored_columns"], json!(["weight", "seen"]));
    assert_eq!(count(&server, 1).await, vec![2]);

    let read_back = body_of(
        vec![Field::new("tessera_id", DataType::UInt64, true)],
        vec![Arc::new(UInt64Array::from(vec![tessera_id_of(&server, 2)])) as ArrayRef],
    );
    let (status, body) = patch_arrow(&server, read_back).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["refused"], json!([]));
    assert_eq!(count(&server, 1).await, vec![3]);
}
