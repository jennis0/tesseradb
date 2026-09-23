//! **The per-point membership column over HTTP** (D12, `client-components.md` §5.10): a request
//! naming a layer gets one nullable `membership:<layer>` column in its points frames; `layers: []`
//! gets none; a response that serves no artifact carries none. And the join, at the wire: every
//! value the column carries is a `tessera_id` in the same body's artifacts frame.
//!
//! **And the `layers` field's two spellings** (D9; owner ruling 2026-08-25): omitted or `[]` is
//! no layers, `"all"` is every reachable one, anything else a `422` — and `all` cannot be a
//! layer's name.
//!
//! The engine's own cases (`tessera-engine/tests/membership_column.rs`) cover the resolution; this
//! file is about the frame — the name, the nullability, the position after the scalars, and the
//! column's presence following the request and the response rather than the registry.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;

use arrow::array::{Array, UInt64Array};
use arrow::ipc::reader::StreamReader;
use common::*;
use serde_json::json;
use tempfile::TempDir;

const TREE: &str = "clusters/tree";

async fn register_and_plant(server: &TestServer) {
    let tree = json!({
        "name": TREE,
        "title": "tree",
        "views": ["s0"],
        "membership": "enumerated",
        "visibility": null,
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": null,
        "hierarchy": { "kind": "flat", "prune_children": false },
        "content": { "computed": ["centroid"], "supplied": [] },
        "depends_on": [],
        "levels": []
    });
    register(server, tree).await;
    let resp = server
        .client
        .put(server.control_url(&format!(
            "/control/layers/{}/artifacts",
            TREE.replace('/', "%2F")
        )))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "addressing": "external",
            // Flat over HTTP: the publish route carries no parent edge (lineage arrives with an
            // ingest batch's list column), and the tree's resolution is the engine tests' business.
            // What this file pins is the frame.
            "artifacts": [
                { "key": "a1", "members": members(0..100) },
                { "key": "a2", "members": members(100..200) },
                { "key": "b", "members": members(200..400) },
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "{}", resp.text().await.unwrap());
}

/// The points frames' schema and the membership columns, read by **name** and asserted to sit
/// after the scalars — the two facts a client decoder relies on.
struct Points {
    names: Vec<String>,
    ids: Vec<u64>,
    membership: BTreeMap<String, Vec<Option<u64>>>,
}

fn decode_points(body: &[u8]) -> Points {
    let mut names = Vec::new();
    let mut ids = Vec::new();
    let mut membership: BTreeMap<String, Vec<Option<u64>>> = BTreeMap::new();
    for (kind, payload) in tessera_wire::split_frames(body).unwrap() {
        if kind != tessera_wire::FRAME_POINTS {
            continue;
        }
        let reader = StreamReader::try_new(Cursor::new(payload.to_vec()), None).unwrap();
        let schema = reader.schema();
        names = schema.fields().iter().map(|f| f.name().clone()).collect();
        for batch in reader {
            let batch = batch.unwrap();
            let id = batch
                .column(0)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap();
            ids.extend(id.values().iter().copied());
            for (i, field) in schema.fields().iter().enumerate() {
                let Some(layer) = field.name().strip_prefix("membership:") else {
                    continue;
                };
                assert!(field.is_nullable(), "a membership column is nullable");
                let col = batch
                    .column(i)
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .expect("a membership column is uint64");
                membership
                    .entry(layer.to_string())
                    .or_default()
                    .extend((0..col.len()).map(|r| col.is_valid(r).then(|| col.value(r))));
            }
        }
    }
    Points {
        names,
        ids,
        membership,
    }
}

async fn viewport(server: &TestServer, token: &str, layers: serde_json::Value) -> Vec<u8> {
    let mut req = json!({
        "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200
    });
    if !layers.is_null() {
        req["layers"] = layers;
    }
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    resp.bytes().await.unwrap().to_vec()
}

#[tokio::test]
async fn a_request_naming_a_layer_gets_its_column_and_the_column_joins_the_artifacts_frame() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register_and_plant(&server).await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let body = viewport(&server, token, json!([TREE])).await;
    let decoded = decode_viewport_frames(&body);
    let served: BTreeSet<u64> = decoded
        .artifacts
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|a| a.tessera_id)
        .collect();
    let keys: BTreeSet<&str> = decoded
        .artifacts
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|a| a.key.as_deref())
        .collect();
    assert_eq!(keys, BTreeSet::from(["a1", "a2", "b"]));

    let points = decode_points(&body);
    assert_eq!(
        points.names.last().map(String::as_str),
        Some("membership:clusters/tree"),
        "one column, named for its layer, after every scalar: {:?}",
        points.names
    );
    assert_eq!(points.membership.len(), 1);
    let column = &points.membership[TREE];
    assert_eq!(column.len(), points.ids.len(), "one value per point");
    let named = column.iter().flatten().count();
    assert!(named > 0 && named < column.len(), "both named and null points were served");
    for id in column.iter().flatten() {
        assert!(
            served.contains(id),
            "the column names {id}, which the artifacts frame does not carry"
        );
    }
    // The existing decoder, which reads the scalars positionally, is unaffected.
    assert_eq!(decoded.points.len(), points.ids.len());
}

#[tokio::test]
async fn an_empty_layer_list_gets_no_column_and_so_does_a_response_serving_nothing() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register_and_plant(&server).await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let body = viewport(&server, token, json!([])).await;
    let decoded = decode_viewport_frames(&body);
    assert!(decoded.artifacts.is_none(), "no artifacts frame");
    let points = decode_points(&body);
    assert!(!points.ids.is_empty(), "points still flow");
    assert!(points.membership.is_empty(), "and carry no column: {:?}", points.names);

    // A layer named that this principal does not reach — or that nobody registered — serves
    // nothing, so there is no column, by the same route.
    let body = viewport(&server, token, json!(["nobody/registered"])).await;
    let decoded = decode_viewport_frames(&body);
    assert!(decoded.artifacts.is_none());
    assert!(decode_points(&body).membership.is_empty());

    // A principal who sees nothing is served no point and no artifact — no points frame at all.
    let auth = authorise(&server, &[]).await;
    let token = auth["token"].as_str().unwrap();
    let body = viewport(&server, token, json!([TREE])).await;
    let decoded = decode_viewport_frames(&body);
    assert!(decoded.artifacts.is_none());
    assert!(decoded.points.is_empty());
}

/// **Omitted means none, `"all"` means every reachable layer, and any other string is refused.**
/// A client that never mentions layers pays no artifact pass and gets no column.
#[tokio::test]
async fn omitted_layers_means_none_and_the_word_all_means_every_reachable_layer() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register_and_plant(&server).await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let body = viewport(&server, token, serde_json::Value::Null).await;
    let decoded = decode_viewport_frames(&body);
    assert!(decoded.artifacts.is_none(), "omitted: no artifacts frame");
    assert!(decode_points(&body).membership.is_empty(), "omitted: no column");

    let body = viewport(&server, token, json!("all")).await;
    let decoded = decode_viewport_frames(&body);
    assert_eq!(
        decoded.artifacts.as_deref().map(<[_]>::len),
        Some(3),
        "\"all\": every reachable layer's artifacts"
    );
    assert_eq!(decode_points(&body).membership.len(), 1);

    for bogus in [json!("ALL"), json!("everything"), json!(7)] {
        let resp = server
            .client
            .post(server.viewer_url("/v1/viewport"))
            .bearer_auth(token)
            .json(&json!({
                "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200,
                "layers": bogus
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 422, "{bogus} is neither a list nor the word");
    }
}

/// `all` is reserved on the wire, so the registry refuses it as a layer name.
#[tokio::test]
async fn a_layer_cannot_be_registered_under_the_reserved_word() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "name": "all",
            "title": "all",
            "views": ["s0"],
            "membership": "enumerated",
            "visibility": null,
            "artifact_visibility": { "field": null, "default": "inherited" },
            "require_member_visibility": null,
            "hierarchy": { "kind": "flat", "prune_children": false },
            "content": { "computed": [], "supplied": [] },
            "depends_on": [],
            "levels": []
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 422);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "contract", "{body}");
}
