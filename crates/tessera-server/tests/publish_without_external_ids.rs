//! A publication under external addressing into a bundle that holds no external-id run is one
//! refusal naming the deployment, not one `404` per member.
//!
//! Found reconciling the ingest campaign's driver (2026-09-04): every base built before the
//! driver passed `--mint-external-ids` answered "id 0 of artifact 0 names nothing this deployment
//! holds" for every publication, which reads as a list of typos when the truth is that nothing in
//! the deployment can be named by an external id. The per-member refusal stays for a bundle that
//! does carry ids, where a member that resolves to nothing is the caller's data being wrong.

mod common;

use std::path::Path;

use common::*;
use serde_json::json;
use tempfile::TempDir;
use tessera_build::{build, BuildArgs};

/// The shared fixture, with the external-id run minted or not.
fn build_fixture_minting(out: &Path, dir: &Path, mint_external_ids: bool) {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    write_points_n(&points, 64);
    write_pairs_n(&pairs, 64);
    let args = BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.to_path_buf(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    };
    build(&args).expect("the fixture build succeeds");
}

async fn serve_fixture(mint_external_ids: bool) -> (TempDir, TestServer) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_minting(&root, tmp.path(), mint_external_ids);
    let server = spawn_server(&root, &tmp.path().join("cache"), &tmp.path().join("wal")).await;
    (tmp, server)
}

async fn register_flat_layer(server: &TestServer, name: &str) {
    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "name": name,
            "title": name,
            "views": ["s0"],
            "membership": "enumerated",
            "value_set": "closed",
            "visibility": null,
            "artifact_visibility": { "field": null, "default": "inherited" },
            "require_member_visibility": null,
            "hierarchy": { "kind": "flat", "prune_children": false },
            "content": { "computed": [], "supplied": [], "withdraw_on_member_deletion": true },
            "depends_on": [],
            "levels": []
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "the layer registers");
}

/// Publish one artifact whose members are the given external ids; return status and body text.
async fn publish(server: &TestServer, layer: &str, members: &[u64]) -> (u16, String) {
    use base64::Engine as _;
    let members: Vec<String> = members
        .iter()
        .map(|e| base64::engine::general_purpose::STANDARD.encode(external_id_of(*e)))
        .collect();
    let encoded = layer.replace('/', "%2F");
    let resp = server
        .client
        .put(server.control_url(&format!("/control/layers/{encoded}/artifacts")))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "addressing": "external",
            "artifacts": [{ "key": "a", "members": members }]
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

#[tokio::test]
async fn a_bundle_with_no_external_ids_refuses_the_publication_once_and_says_why() {
    let (_tmp, server) = serve_fixture(false).await;
    register_flat_layer(&server, "flat/x").await;
    let (status, body) = publish(&server, "flat/x", &[1, 2, 3]).await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body.contains("carries no external ids"),
        "the refusal names the deployment, not a member: {body}"
    );
    assert!(
        !body.contains("names nothing this deployment holds"),
        "not the per-member refusal: {body}"
    );
}

#[tokio::test]
async fn a_bundle_with_external_ids_still_refuses_an_unknown_member_by_position() {
    let (_tmp, server) = serve_fixture(true).await;
    register_flat_layer(&server, "flat/x").await;
    // 1 and 2 exist; 10_000 names nothing. The member is reported by its position.
    let (status, body) = publish(&server, "flat/x", &[1, 2, 10_000]).await;
    assert_eq!(status, 404, "{body}");
    assert!(
        body.contains("id 2 of artifact 0 names nothing this deployment holds"),
        "{body}"
    );
    // And a publication whose members all exist lands.
    let (status, body) = publish(&server, "flat/x", &[1, 2]).await;
    assert_eq!(status, 201, "{body}");
}
