//! A server over a bundle a build wrote past its `[shards] size`: ready, serving reads, and
//! refusing every write with a 409 before reading the write's body.

mod common;

use std::path::Path;

use common::*;
use mosaica_build::{build, BuildArgs};
use reqwest::Method;
use tempfile::TempDir;

/// Items the fixture builds.
const N: u64 = 40;

/// The standard fixture of [`N`] items, built into `dir/bundle` at a size that writes two shards.
fn build_two_shards(dir: &Path) {
    let points = dir.join("points.parquet");
    write_points_n(&points, N);
    write_pairs_n(&dir.join("pairs.parquet"), N);
    let view = view_args(
        "s0",
        &points,
        mosaica_build::config::AccessInput::relation(dir.join("pairs.parquet")),
    );
    let args = BuildArgs {
        shard_size: Some(N - 15),
        ..build_args(&dir.join("bundle"), vec![view])
    };
    build(&with_id(args, &points)).expect("the build writes two shards");
}

#[tokio::test]
async fn a_server_over_several_shards_is_ready_and_refuses_every_write() {
    let dir = TempDir::new().unwrap();
    build_two_shards(dir.path());
    let server = open(dir.path()).await;

    for url in [server.viewer_url("/readyz"), server.session_url("/readyz")] {
        let status = server.client.get(&url).send().await.unwrap().status();
        assert_eq!(status, 200, "{url}");
    }
    let resp = server
        .client
        .get(server.control_url("/control/status"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let status: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        status["entity_id_high_water"], N,
        "both shards' high waters"
    );
    assert_eq!(status["partitions"][0]["watermark"], N);
    assert_eq!(status["write_executor"]["posture"], "running");

    // Every route of the write router, each with a body that is not a write, so the refusal
    // cannot have read it.
    let routes = [
        (Method::POST, "/control/ingest"),
        (Method::POST, "/control/changes"),
        (Method::PUT, "/control/layers"),
        (Method::DELETE, "/control/layers/l"),
        (Method::PUT, "/control/attributes"),
        (Method::PUT, "/control/shards"),
        (Method::PUT, "/control/vocabularies/v"),
        (Method::PATCH, "/control/vocabularies/v/values"),
        (Method::PUT, "/control/views/g/k"),
        (Method::DELETE, "/control/views/g/k"),
        (Method::PUT, "/control/view_groups/g"),
        (Method::PUT, "/control/views/p"),
        (Method::PUT, "/control/layers/l/artifacts"),
        (Method::PATCH, "/control/layers/l/artifacts"),
        (Method::POST, "/control/flush"),
        (Method::POST, "/control/compact"),
    ];
    let write = async |method: &Method, path: &str, credential: &str| {
        server
            .client
            .request(method.clone(), server.control_url(path))
            .bearer_auth(credential)
            .body("not a write")
            .send()
            .await
            .unwrap()
            .status()
    };
    for (method, path) in &routes {
        assert_eq!(
            write(method, path, OPERATOR_CREDENTIAL).await,
            409,
            "{method} {path}"
        );
        // A caller who may not write learns nothing about the bundle.
        assert_eq!(
            write(method, path, &server.integrator_key).await,
            403,
            "{method} {path}"
        );
    }

    // The identity catalogue is not the bundle, and takes its writes.
    let principal = server
        .client
        .post(server.control_url("/control/principals"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&serde_json::json!({ "name": "ann", "kind": "person" }))
        .send()
        .await
        .unwrap();
    assert_eq!(principal.status(), 200);

    // Reads answer.
    let token = token_for(&server, &["0"]).await;
    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
