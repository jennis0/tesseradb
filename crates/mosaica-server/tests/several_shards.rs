//! A server over a bundle a build wrote past its `[shards] size`: ready, serving reads,
//! answering every write as a server over the same items built as one shard answers it, and
//! reporting each shard in its status.

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

/// Each shard's record in `/control/status`, ascending.
async fn records(server: &TestServer) -> Vec<serde_json::Value> {
    control_status(server).await["shards"]["records"]
        .as_array()
        .unwrap_or_else(|| panic!("status lists the shards"))
        .clone()
}

/// The shards whose records have `field` set to `value`.
fn shards_where(records: &[serde_json::Value], field: &str, value: serde_json::Value) -> Vec<u64> {
    records
        .iter()
        .filter(|record| record[field] == value)
        .map(|record| record["shard"].as_u64().unwrap())
        .collect()
}

/// A record for each shard: its state and rows as built, a deletion in the shard holding the
/// item, and a compaction of that shard alone while it runs and once it has published.
#[tokio::test]
async fn status_holds_a_record_for_each_shard() {
    let dir = TempDir::new().unwrap();
    build_fixture_sized(dir.path(), N, Some(SIZE));
    let server = open(dir.path()).await;

    let status = control_status(&server).await;
    let built: Vec<serde_json::Value> = records(&server)
        .await
        .iter()
        .map(|r| {
            serde_json::json!([
                r["shard"],
                r["state"],
                r["high_water"],
                r["live_rows"],
                r["pool"],
                r["retired"],
                r["deleted"],
                r["suppressed"],
                r["segments"],
                r["last_compaction"],
                r["compacting"]
            ])
        })
        .collect();
    assert_eq!(
        built,
        [
            serde_json::json!([0, "sealed", SIZE, SIZE, 0, 0, 0, 0, 1, null, false]),
            serde_json::json!([1, "open", N - SIZE, N - SIZE, 0, 0, 0, 0, 1, null, false]),
        ]
    );
    assert_eq!(
        status["compaction"]["live_rows"], N,
        "the records' rows sum to it"
    );

    let resp = post_changes(
        &server,
        &serde_json::json!([
            { "op": "delete", "match": { "id": member(5) } },
            { "op": "suppress", "match": { "id": member(6) } },
        ]),
    )
    .await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    tick(&server).await;
    let changed = records(&server).await;
    let [deleting] = shards_where(&changed, "deleted", serde_json::json!(1))[..] else {
        panic!("one shard holds the deletion: {changed:?}");
    };
    assert_eq!(
        shards_where(&changed, "deleted", serde_json::json!(0)).len(),
        1
    );
    let suppressed: u64 = changed
        .iter()
        .map(|r| r["suppressed"].as_u64().unwrap())
        .sum();
    assert_eq!(suppressed, 1, "{changed:?}");

    let engine = &server.state.engine;
    engine.set_fold_paused_for_test(true);
    let resp = server
        .client
        .post(server.control_url("/control/compact"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    wait_until(
        "the compaction holding",
        std::time::Duration::from_secs(120),
        async || engine.fold_is_holding_for_test(),
    )
    .await;
    let running = records(&server).await;
    assert_eq!(
        shards_where(&running, "compacting", serde_json::json!(true)),
        [deleting],
        "{running:?}"
    );
    engine.set_fold_paused_for_test(false);
    wait_for_executor(&server, "the compaction answered", |now| {
        now.folds == 1 && !now.fold_requested
    })
    .await;

    let compacted = records(&server).await;
    assert_eq!(
        shards_where(&compacted, "last_compaction", serde_json::Value::Null).len(),
        1,
        "the other shard has not compacted: {compacted:?}"
    );
    let record = &compacted[deleting as usize];
    assert!(record["last_compaction"]["secs"].is_u64(), "{record}");
    assert!(
        record["last_compaction"]["rss_bytes"].as_u64() > Some(0),
        "{record}"
    );
    assert_eq!(
        (&record["deleted"], &record["pool"], &record["compacting"]),
        (
            &serde_json::json!(0),
            &serde_json::json!(1),
            &serde_json::json!(false)
        ),
        "the deletion is retired and its number freed: {record}"
    );
}
