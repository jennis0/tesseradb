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
        mint_external_ids,
        ..build_args(
            out,
            vec![view_args("s0", &points, AccessInput::relation(pairs))],
        )
    };
    build(&args).expect("the fixture build succeeds");
}

async fn serve_fixture(mint_external_ids: bool) -> (TempDir, TestServer) {
    let tmp = TempDir::new().unwrap();
    build_fixture_minting(&tmp.path().join("bundle"), tmp.path(), mint_external_ids);
    let server = open(&tmp).await;
    (tmp, server)
}

/// Publish one artifact whose members are the given external ids; return status and body text.
async fn publish(server: &TestServer, layer: &str, members: &[u64]) -> (u16, serde_json::Value) {
    let members: Vec<String> = members.iter().map(|e| member(*e)).collect();
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
    (status, resp.json().await.unwrap_or_default())
}

#[tokio::test]
async fn a_bundle_with_no_external_ids_refuses_the_publication_once_and_says_why() {
    let (_tmp, server) = serve_fixture(false).await;
    register(&server, flat_layer("flat/x")).await;
    let (status, body) = publish(&server, "flat/x", &[1, 2, 3]).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"], "contract", "{body}");
}

#[tokio::test]
async fn a_bundle_with_external_ids_still_refuses_an_unknown_member_by_position() {
    let (_tmp, server) = serve_fixture(true).await;
    register(&server, flat_layer("flat/x")).await;
    // 1 and 2 exist; 10_000 names nothing. The member is reported by its position.
    let (status, body) = publish(&server, "flat/x", &[1, 2, 10_000]).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"], "unknown", "{body}");
    // And a publication whose members all exist lands.
    let (status, body) = publish(&server, "flat/x", &[1, 2]).await;
    assert_eq!(status, 201, "{body}");
}
