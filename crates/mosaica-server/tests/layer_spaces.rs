//! An artifact's and a layer's `mosaica_id` carry the layer's space under the kind bit, and route
//! by it on the wire: neither names an item as a member, a generating-set entry or an ingest row,
//! and `/control/changes` takes both. A layer is removed by `DELETE /control/layers/{name}` and
//! never by deleting its own identifier.

mod common;

use common::*;
use mosaica_types::{ItemHigh, MosaicaId, ShardId, Tenancy};
use serde_json::{json, Value};
use tempfile::TempDir;

async fn publish(server: &TestServer, query: &str, body: Value) -> (u16, Value) {
    let resp = server
        .client
        .put(server.control_url(&format!("/control/layers/clusters%2Fa/artifacts{query}")))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn changes(server: &TestServer, body: Value) -> (u16, Value) {
    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn ingest(server: &TestServer, query: &str, batch: &str, rows: Value) -> (u16, Value) {
    let resp = server
        .client
        .post(server.control_url(&format!("/control/ingest{query}")))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-mosaica-batch-id", batch)
        .header("x-mosaica-view", "s0")
        .json(&rows)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn artifact_card(server: &TestServer, token: &str, id: u64) -> u16 {
    server
        .client
        .post(server.viewer_url(&format!("/v1/artifacts/{id}")))
        .bearer_auth(token)
        .json(&json!({ "view": "s0" }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn layer_names(server: &TestServer) -> Vec<String> {
    let token = token_for(server, &["0"]).await;
    let meta: Value = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    meta["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["name"].as_str().unwrap().to_string())
        .collect()
}

fn id_of(value: &Value) -> u64 {
    value.as_str().unwrap().parse().unwrap()
}

/// `clusters/a`, holding one artifact `c0` over sources 0 to 4: the layer's and the artifact's
/// identifiers.
async fn plant(server: &TestServer) -> (u64, u64) {
    let (status, layer) = put_layer(server, flat_layer("clusters/a")).await;
    assert_eq!(status, 201, "{layer}");
    let (status, body) = publish(
        server,
        "",
        json!({ "artifacts": [{ "key": "c0", "members": members(0..5) }] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    (id_of(&layer["mosaica_id"]), id_of(&body["artifacts"][0]["mosaica_id"]))
}

/// The item identifier over an artifact's or a layer's own 32 bits: the same bits but the kind.
fn as_item(id: u64) -> u64 {
    let key = test_key();
    let (space, entity) = key.invert_artifact(MosaicaId::new(id)).unwrap();
    assert_eq!(space.raw(), 0, "the first layer takes space 0, which differs by the kind bit");
    key.forward(ItemHigh::new(ShardId::ZERO, Tenancy::ZERO), entity)
        .unwrap()
        .raw()
}

/// Each identifier answers on its own kind's routes, and the item identifier differing from an
/// artifact's or a layer's by the kind bit alone names nothing anywhere.
#[tokio::test]
async fn an_identifier_routes_by_its_kind() {
    let tmp = TempDir::new().unwrap();
    let server = serve_standard(&tmp).await;
    let (layer, artifact) = plant(&server).await;
    let token = token_for(&server, &["0"]).await;
    let item = mosaica_id_of(&server, 0);

    assert_eq!(artifact_card(&server, &token, artifact).await, 200);
    assert_eq!(post_item(&server, &token, item).await.status(), 200);
    for id in [artifact, layer, as_item(artifact), as_item(layer)] {
        assert_eq!(post_item(&server, &token, id).await.status(), 404, "{id}");
    }
    for id in [item, layer, as_item(artifact), as_item(layer)] {
        assert_eq!(artifact_card(&server, &token, id).await, 404, "{id}");
    }

    // The changes route takes each kind, and refuses the twins as naming nothing.
    let (status, body) = changes(
        &server,
        json!([
            { "op": "suppress", "match": { "mosaica_id": as_item(artifact).to_string() } },
            { "op": "suppress", "match": { "mosaica_id": artifact.to_string() } },
        ]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["accepted"], 1, "{body}");
    assert_eq!(
        body["refused"],
        json!([{ "row": 0, "reason": "unknown_mosaica_id" }])
    );
    assert_eq!(artifact_card(&server, &token, artifact).await, 404);
    assert_eq!(post_item(&server, &token, item).await.status(), 200);
}

/// A layer's identifier deletes nothing through `/control/changes`: the request is refused whole,
/// and the layer is removed by dropping it. Its suppression is accepted.
#[tokio::test]
async fn a_layers_identifier_suppresses_it_and_does_not_delete_it() {
    let tmp = TempDir::new().unwrap();
    let server = serve_standard(&tmp).await;
    let (layer, artifact) = plant(&server).await;
    let token = token_for(&server, &["0"]).await;

    let (status, body) = changes(
        &server,
        json!([
            { "op": "delete", "match": { "mosaica_id": artifact.to_string() } },
            { "op": "delete", "match": { "mosaica_id": layer.to_string() } },
        ]),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"], "contract", "{body}");
    assert_eq!(layer_names(&server).await, vec!["clusters/a".to_string()]);
    assert_eq!(
        artifact_card(&server, &token, artifact).await,
        200,
        "nothing in the refused request was applied"
    );

    for (op, layers) in [("suppress", 0), ("unsuppress", 1)] {
        let (status, body) = changes(
            &server,
            json!([{ "op": op, "match": { "mosaica_id": layer.to_string() } }]),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(layer_names(&server).await.len(), layers);
    }
}

/// An artifact's or a layer's identifier in a member table or an ingest row is refused as naming
/// nothing, row by row, as an identifier nobody holds is, and refuses a strict request as one does.
#[tokio::test]
async fn an_artifacts_or_a_layers_identifier_names_no_item_in_a_write() {
    let tmp = TempDir::new().unwrap();
    let server = serve_standard(&tmp).await;
    let (layer, artifact) = plant(&server).await;
    let item = mosaica_id_of(&server, 7);
    let unknown = 12_345;

    let (status, body) = publish(
        &server,
        "",
        json!({ "artifacts": [{
            "key": "c1",
            "members": members_by_mosaica_id([artifact, layer, unknown, item]),
        }] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let refused: Vec<(u64, Value)> = body["refused"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["row"].as_u64().unwrap(), r["reason"].clone()))
        .collect();
    assert_eq!(
        refused,
        vec![
            (0, json!("unknown_mosaica_id")),
            (1, json!("unknown_mosaica_id")),
            (2, json!("unknown_mosaica_id")),
        ]
    );

    let strict = |id: u64| {
        json!({ "artifacts": [{ "key": "c2", "members": members_by_mosaica_id([id]) }] })
    };
    let (unknown_status, _) = publish(&server, "?strict=true", strict(unknown)).await;
    assert_eq!(unknown_status, 404);
    for id in [artifact, layer] {
        let (status, body) = publish(&server, "?strict=true", strict(id)).await;
        assert_eq!(status, unknown_status, "{body}");
    }

    let row = |id: u64| {
        json!([{ "mosaica_id": id.to_string(), "x": 1.0, "y": 1.0, "access": ["0"] }])
    };
    let (_, by_unknown) = ingest(&server, "", "by-unknown", row(unknown)).await;
    for (batch, id) in [("by-artifact", artifact), ("by-layer", layer)] {
        let (status, body) = ingest(&server, "", batch, row(id)).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["refused"], by_unknown["refused"], "{body}");
    }
    let (unknown_status, _) = ingest(&server, "?strict=true", "s-unknown", row(unknown)).await;
    let (status, body) = ingest(&server, "?strict=true", "s-artifact", row(artifact)).await;
    assert_eq!(status, unknown_status, "{body}");
}

/// A dropped layer's name registers again as a new layer, and none of the dropped layer's
/// identifiers names it or its artifacts, before a restart or after one.
#[tokio::test]
async fn a_dropped_name_registers_again_and_its_old_identifiers_name_nothing() {
    let tmp = TempDir::new().unwrap();
    let server = serve_standard(&tmp).await;
    let (old_layer, old_artifact) = plant(&server).await;
    let resp = server
        .client
        .delete(server.control_url("/control/layers/clusters%2Fa"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let (new_layer, new_artifact) = plant(&server).await;
    assert_ne!(new_layer, old_layer);
    assert_ne!(new_artifact, old_artifact);

    let check = |server: TestServer| async move {
        let token = token_for(&server, &["0"]).await;
        assert_eq!(artifact_card(&server, &token, new_artifact).await, 200);
        assert_eq!(artifact_card(&server, &token, old_artifact).await, 404);
        for id in [old_layer, old_artifact] {
            let (status, body) = changes(
                &server,
                json!([{ "op": "suppress", "match": { "mosaica_id": id.to_string() } }]),
            )
            .await;
            assert_eq!(status, 200, "{body}");
            assert_eq!(body["accepted"], 0, "{body}");
        }
        assert_eq!(layer_names(&server).await, vec!["clusters/a".to_string()]);
        assert_eq!(artifact_card(&server, &token, new_artifact).await, 200);
        server
    };
    let server = check(server).await;
    let server = restart(server, &tmp).await;
    check(server).await;
}

/// `/control/status` counts the point shard's denies under `overlay` and the layer spaces' under
/// `layer_spaces`, and a fold removes the deleted artifact's record and takes it off both counts.
#[tokio::test]
async fn status_counts_the_layer_spaces_denies_apart_from_the_shards() {
    let tmp = TempDir::new().unwrap();
    let server = serve_standard(&tmp).await;
    let (layer, artifact) = plant(&server).await;
    let item = mosaica_id_of(&server, 0);
    let status = control_status(&server).await;
    assert_eq!(
        status["layer_spaces"],
        json!({ "count": 1, "depth": 0, "retirable": 0 })
    );

    let (status, body) = changes(
        &server,
        json!([
            { "op": "delete", "match": { "mosaica_id": artifact.to_string() } },
            { "op": "suppress", "match": { "mosaica_id": layer.to_string() } },
            { "op": "suppress", "match": { "mosaica_id": item.to_string() } },
        ]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let status = control_status(&server).await;
    assert_eq!(status["overlay"]["depth"], 1, "{status}");
    assert_eq!(status["overlay"]["retirable"], 0, "{status}");
    assert_eq!(
        status["layer_spaces"],
        json!({ "count": 1, "depth": 2, "retirable": 1 })
    );

    fold(&server).await;
    let status = control_status(&server).await;
    assert_eq!(status["overlay"]["depth"], 1, "{status}");
    assert_eq!(
        status["layer_spaces"],
        json!({ "count": 1, "depth": 1, "retirable": 0 }),
        "the fold removed the deleted artifact, and the layer's suppression stands"
    );
}
