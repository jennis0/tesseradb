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
    publish_into(server, "clusters%2Fa", query, body).await
}

async fn publish_into(server: &TestServer, layer: &str, query: &str, body: Value) -> (u16, Value) {
    let resp = server
        .client
        .put(server.control_url(&format!("/control/layers/{layer}/artifacts{query}")))
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
/// artifact's by the kind bit alone names nothing anywhere.
#[tokio::test]
async fn an_identifier_routes_by_its_kind() {
    let tmp = TempDir::new().unwrap();
    let server = serve_standard(&tmp).await;
    let (layer, artifact) = plant(&server).await;
    let token = token_for(&server, &["0"]).await;
    let item = mosaica_id_of(&server, 0);

    assert_eq!(artifact_card(&server, &token, artifact).await, 200);
    assert_eq!(post_item(&server, &token, item).await.status(), 200);
    for id in [artifact, layer, as_item(artifact)] {
        assert_eq!(post_item(&server, &token, id).await.status(), 404, "{id}");
    }
    // The layer is entity 0 of its space, and the item identifier over its bits names item 0.
    assert_eq!(post_item(&server, &token, as_item(layer)).await.status(), 200);
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

/// The layer space an artifact's or a layer's identifier carries.
fn space_of(id: u64) -> u64 {
    let (space, _) = test_key().invert_artifact(MosaicaId::new(id)).unwrap();
    u64::from(space.raw())
}

/// The spaces the newest side-manifest of the bundle at `root` names: its layer space records',
/// and its shard's member packs' and row forms'.
fn spaces_stored(root: &std::path::Path) -> Vec<u64> {
    let manifest = newest_side_manifest(root);
    let shard = &manifest["shards"]["records"][0];
    let mut spaces: Vec<u64> = manifest["layer_spaces"]
        .as_array()
        .unwrap()
        .iter()
        .chain(shard["membership_extents"].as_array().unwrap())
        .chain(shard["derived_extents"].as_array().unwrap())
        .map(|record| record["space"].as_u64().unwrap())
        .collect();
    spaces.sort();
    spaces.dedup();
    spaces
}

/// A dropped layer leaves nothing behind. Of a layer holding a published, a suppressed and a
/// deleted artifact, nothing is served after the drop and no identifier of it names anything; its
/// space leaves the next side-manifest, and after the next fold no file of it is in the served
/// prefix. A restart brings none of it back, and the name registered again is a new layer none of
/// whose identifiers the dropped one issued.
#[tokio::test]
async fn a_dropped_layer_is_neither_served_nor_stored() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let server = serve_standard(&tmp).await;
    let (layer, c0) = plant(&server).await;
    let (status, body) = publish(
        &server,
        "",
        json!({ "artifacts": [
            { "key": "c1", "members": members(5..10) },
            { "key": "c2", "members": members(10..15) },
        ] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let ids: Vec<u64> = body["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| id_of(&a["mosaica_id"]))
        .collect();
    let (c1, c2) = (ids[0], ids[1]);
    let (status, other) = put_layer(&server, flat_layer("clusters/b")).await;
    assert_eq!(status, 201, "{other}");
    let (status, body) = publish_into(
        &server,
        "clusters%2Fb",
        "",
        json!({ "artifacts": [{ "key": "k0", "members": members(0..5) }] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let kept = id_of(&body["artifacts"][0]["mosaica_id"]);
    let (status, body) = changes(
        &server,
        json!([
            { "op": "suppress", "match": { "mosaica_id": c1.to_string() } },
            { "op": "delete", "match": { "mosaica_id": c2.to_string() } },
        ]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    fold(&server).await;
    let (dropped_space, kept_space) = (space_of(layer), space_of(id_of(&other["mosaica_id"])));
    assert_eq!(spaces_stored(&root), vec![dropped_space, kept_space]);

    let resp = server
        .client
        .delete(server.control_url("/control/layers/clusters%2Fa"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let dropped = [layer, c0, c1, c2];
    let served = |server: TestServer| async move {
        let token = token_for(&server, &["0"]).await;
        assert_eq!(layer_names(&server).await, vec!["clusters/b".to_string()]);
        for id in dropped {
            assert_eq!(artifact_card(&server, &token, id).await, 404, "{id}");
        }
        assert_eq!(artifact_card(&server, &token, kept).await, 200);
        let names: Vec<Value> = dropped
            .iter()
            .map(|id| json!({ "op": "suppress", "match": { "mosaica_id": id.to_string() } }))
            .collect();
        let (status, body) = changes(&server, Value::Array(names)).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["accepted"], 0, "{body}");
        assert_eq!(body["refused"].as_array().unwrap().len(), dropped.len(), "{body}");
        server
    };
    let server = served(server).await;
    tick(&server).await;
    assert_eq!(spaces_stored(&root), vec![kept_space], "the drop's publication names none of it");

    fold(&server).await;
    assert_eq!(spaces_stored(&root), vec![kept_space]);
    let (_, prefix) = live_prefix(&root);
    let space_dir = mosaica_store::LayerSpaceLayout::new(
        "default",
        mosaica_types::LayerSpace::new(dropped_space as u32).unwrap(),
    )
    .dir(&prefix);
    assert!(!space_dir.exists(), "no file of the dropped space is in the served prefix");
    let mut packs: Vec<String> = newest_shard_record(&root)["membership_extents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|extent| extent["path"].as_str().unwrap().to_string())
        .collect();
    packs.sort();
    let mut files: Vec<String> = std::fs::read_dir(prefix.join(SHARD.members_rel()))
        .unwrap()
        .flatten()
        .map(|entry| format!("{}/{}", SHARD.members_rel(), entry.file_name().to_string_lossy()))
        .collect();
    files.sort();
    assert_eq!(files, packs, "every member pack in the prefix is the kept layer's");

    let server = served(restart(server, &tmp).await).await;
    let (again, again_artifact) = plant(&server).await;
    assert!(space_of(again) > dropped_space.max(kept_space));
    assert!(!dropped.contains(&again) && !dropped.contains(&again_artifact));
    let token = token_for(&server, &["0"]).await;
    assert_eq!(artifact_card(&server, &token, again_artifact).await, 200);
    for id in dropped {
        assert_eq!(artifact_card(&server, &token, id).await, 404, "{id}");
    }
}
