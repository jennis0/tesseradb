//! A server over a bundle a build wrote past its `[shards] size`: ready, serving reads, and
//! answering every write as a server over the same items built as one shard answers it.

mod common;

use common::*;
use reqwest::Method;
use tempfile::TempDir;

/// Items the fixture builds.
const N: u64 = 40;
/// The size the fixture is built at: shards of 25 and 15 items.
const SIZE: u64 = 25;

#[tokio::test]
async fn a_server_over_several_shards_is_ready_and_answers_every_write_as_over_one() {
    let dir = TempDir::new().unwrap();
    build_fixture_sized(dir.path(), N, Some(SIZE));
    let server = open(dir.path()).await;
    let one_dir = TempDir::new().unwrap();
    build_fixture_sized(one_dir.path(), N, None);
    let one = open(one_dir.path()).await;

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

    // Every route of the write router, each with a body that is not a write.
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
    let write = async |server: &TestServer, method: &Method, path: &str, credential: &str| {
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
        let answered = write(&server, method, path, OPERATOR_CREDENTIAL).await;
        assert_eq!(
            answered,
            write(&one, method, path, OPERATOR_CREDENTIAL).await,
            "{method} {path}"
        );
        assert_eq!(
            write(&server, method, path, &server.integrator_key).await,
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
