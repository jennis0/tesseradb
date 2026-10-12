//! An artifact's and a layer's `mosaica_id` carry the layer's space under the kind bit, and route
//! by it on the wire: neither names an item as a member, a generating-set entry or an ingest row,
//! and `/control/changes` takes both. A layer is removed by `DELETE /control/layers/{name}` and
//! never by deleting its own identifier.

mod common;

use common::*;
use mosaica_types::{ItemHigh, LayerSpace, MosaicaId, ShardId, Tenancy};
use serde_json::{json, Value};
use tempfile::TempDir;

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

/// `clusters/a`, holding one artifact `c0` over sources 0 to 4: the layer's and the artifact's
/// identifiers.
async fn plant(server: &TestServer) -> (u64, u64) {
    let (status, layer) = put_layer(server, flat_layer("clusters/a")).await;
    assert_eq!(status, 201, "{layer}");
    let (status, body) = publish(
        server,
        "clusters/a",
        "",
        json!({ "artifacts": [{ "key": "c0", "members": members(0..5) }] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    (parse_id(&layer["mosaica_id"]), parse_id(&body["artifacts"][0]["mosaica_id"]))
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

    assert_eq!(post_artifact(&server, &token, artifact, "s0").await.status(), 200);
    assert_eq!(post_item(&server, &token, item).await.status(), 200);
    for id in [artifact, layer, as_item(artifact)] {
        assert_eq!(post_item(&server, &token, id).await.status(), 404, "{id}");
    }
    // The layer is entity 0 of its space, and the item identifier over its bits names item 0.
    assert_eq!(post_item(&server, &token, as_item(layer)).await.status(), 200);
    for id in [item, layer, as_item(artifact), as_item(layer)] {
        assert_eq!(post_artifact(&server, &token, id, "s0").await.status(), 404, "{id}");
    }

    let resp = post_changes(
        &server,
        &json!([
            { "op": "suppress", "match": { "mosaica_id": as_item(artifact).to_string() } },
            { "op": "suppress", "match": { "mosaica_id": artifact.to_string() } },
        ]),
    )
    .await;
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["accepted"], 1, "{body}");
    assert_eq!(
        body["refused"],
        json!([{ "row": 0, "reason": "unknown_mosaica_id" }])
    );
    assert_eq!(post_artifact(&server, &token, artifact, "s0").await.status(), 404);
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

    let resp = post_changes(
        &server,
        &json!([
            { "op": "delete", "match": { "mosaica_id": artifact.to_string() } },
            { "op": "delete", "match": { "mosaica_id": layer.to_string() } },
        ]),
    )
    .await;
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"], "contract", "{body}");
    let layers = meta_layers(&server, &["0"]).await;
    assert_eq!(layers.iter().map(|l| &l["name"]).collect::<Vec<_>>(), ["clusters/a"]);
    assert_eq!(
        post_artifact(&server, &token, artifact, "s0").await.status(),
        200,
        "nothing in the refused request was applied"
    );

    for (op, layers) in [("suppress", 0), ("unsuppress", 1)] {
        let resp = post_changes(
            &server,
            &json!([{ "op": op, "match": { "mosaica_id": layer.to_string() } }]),
        )
        .await;
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(status, 200, "{body}");
        assert_eq!(meta_layers(&server, &["0"]).await.len(), layers);
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
        "clusters/a",
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
    let (unknown_status, _) = publish(&server, "clusters/a", "?strict=true", strict(unknown)).await;
    assert_eq!(unknown_status, 404);
    for id in [artifact, layer] {
        let (status, body) = publish(&server, "clusters/a", "?strict=true", strict(id)).await;
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

    let resp = post_changes(
        &server,
        &json!([
            { "op": "delete", "match": { "mosaica_id": artifact.to_string() } },
            { "op": "suppress", "match": { "mosaica_id": layer.to_string() } },
            { "op": "suppress", "match": { "mosaica_id": item.to_string() } },
        ]),
    )
    .await;
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
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
fn space_of(id: u64) -> LayerSpace {
    test_key().invert_artifact(MosaicaId::new(id)).unwrap().0
}

/// The spaces the newest side-manifest of the bundle at `root` names: its layer space records',
/// and its shard's member packs' and row forms'.
fn spaces_stored(root: &std::path::Path) -> Vec<LayerSpace> {
    let manifest = newest_side_manifest(root);
    let shard = &manifest["shards"]["records"][0];
    let mut spaces: Vec<LayerSpace> = manifest["layer_spaces"]
        .as_array()
        .unwrap()
        .iter()
        .chain(shard["membership_extents"].as_array().unwrap())
        .chain(shard["derived_extents"].as_array().unwrap())
        .map(|record| serde_json::from_value(record["space"].clone()).unwrap())
        .collect();
    spaces.sort();
    spaces.dedup();
    spaces
}

/// A dropped layer leaves nothing behind. Of a layer holding a published, a suppressed and a
/// deleted artifact, nothing is served after the drop and no identifier of it names anything; its
/// space leaves the next side-manifest, and after the next fold no file of it is in the served
/// prefix. A restart brings none of it back, and the name registered again is a new layer none of
/// whose identifiers the dropped one issued, before a restart or after one.
#[tokio::test]
async fn a_dropped_layer_is_neither_served_nor_stored() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let server = serve_standard(&tmp).await;
    let (layer, c0) = plant(&server).await;
    let (status, body) = publish(
        &server,
        "clusters/a",
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
        .map(|a| parse_id(&a["mosaica_id"]))
        .collect();
    let (c1, c2) = (ids[0], ids[1]);
    let (status, other) = put_layer(&server, flat_layer("clusters/b")).await;
    assert_eq!(status, 201, "{other}");
    let (status, body) = publish(
        &server,
        "clusters/b",
        "",
        json!({ "artifacts": [{ "key": "k0", "members": members(0..5) }] }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let kept = parse_id(&body["artifacts"][0]["mosaica_id"]);
    let resp = post_changes(
        &server,
        &json!([
            { "op": "suppress", "match": { "mosaica_id": c1.to_string() } },
            { "op": "delete", "match": { "mosaica_id": c2.to_string() } },
        ]),
    )
    .await;
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    fold(&server).await;
    let (dropped_space, kept_space) = (space_of(layer), space_of(parse_id(&other["mosaica_id"])));
    assert_eq!(spaces_stored(&root), vec![dropped_space, kept_space]);

    assert_eq!(drop_layer(&server, "clusters/a").await.0, 200);
    let dropped = [layer, c0, c1, c2];
    let served = |server: TestServer, layers: &'static [&'static str]| async move {
        let token = token_for(&server, &["0"]).await;
        let names = meta_layers(&server, &["0"]).await;
        assert_eq!(names.iter().map(|l| &l["name"]).collect::<Vec<_>>(), layers);
        for id in dropped {
            assert_eq!(post_artifact(&server, &token, id, "s0").await.status(), 404, "{id}");
        }
        assert_eq!(post_artifact(&server, &token, kept, "s0").await.status(), 200);
        let names: Vec<Value> = dropped
            .iter()
            .map(|id| json!({ "op": "suppress", "match": { "mosaica_id": id.to_string() } }))
            .collect();
        let resp = post_changes(&server, &Value::Array(names)).await;
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["accepted"], 0, "{body}");
        assert_eq!(body["refused"].as_array().unwrap().len(), dropped.len(), "{body}");
        server
    };
    let server = served(server, &["clusters/b"]).await;
    tick(&server).await;
    assert_eq!(spaces_stored(&root), vec![kept_space], "the drop's publication names none of it");

    fold(&server).await;
    assert_eq!(spaces_stored(&root), vec![kept_space]);
    let (_, prefix) = live_prefix(&root);
    let space_dir = mosaica_store::LayerSpaceLayout::new("default", dropped_space).dir(&prefix);
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

    let server = served(restart(server, &tmp).await, &["clusters/b"]).await;
    let (again, again_artifact) = plant(&server).await;
    assert!(space_of(again) > dropped_space.max(kept_space));
    assert!(!dropped.contains(&again) && !dropped.contains(&again_artifact));
    let again_served = |server: TestServer| async move {
        let token = token_for(&server, &["0"]).await;
        assert_eq!(post_artifact(&server, &token, again_artifact, "s0").await.status(), 200);
        served(server, &["clusters/a", "clusters/b"]).await
    };
    let server = again_served(server).await;
    again_served(restart(server, &tmp).await).await;
}
