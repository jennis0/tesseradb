//! A publication under external addressing into a bundle that holds no external ids is one
//! refusal for the deployment, not one `404` per member.

mod common;

use std::path::Path;

use common::*;
use serde_json::json;
use tempfile::TempDir;
use tessera_build::{build, BuildArgs};

/// 64 items placed and labelled as the standard fixture's are, built into `dir/bundle` with no
/// external ids.
fn build_without_external_ids(dir: &Path) {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    write_points_n(&points, 64);
    write_pairs_n(&pairs, 64);
    let args = BuildArgs {
        mint_external_ids: false,
        ..build_args(
            &dir.join("bundle"),
            vec![view_args("s0", &points, AccessInput::relation(pairs))],
        )
    };
    build(&args).expect("the fixture build succeeds");
}

/// Publish one artifact whose members are the given external ids; return the status and the body.
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
async fn a_bundle_with_no_external_ids_refuses_the_publication_once() {
    let tmp = TempDir::new().unwrap();
    build_without_external_ids(tmp.path());
    let server = open(&tmp).await;
    register(&server, flat_layer("flat/x")).await;
    let (status, body) = publish(&server, "flat/x", &[1, 2, 3]).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"], "contract", "{body}");

    // The same body lands on a bundle that holds external ids, so the refusal is the bundle's.
    let tmp = TempDir::new().unwrap();
    let server = serve_standard(&tmp).await;
    register(&server, flat_layer("flat/x")).await;
    let (status, body) = publish(&server, "flat/x", &[1, 2, 3]).await;
    assert_eq!(status, 201, "{body}");
}
