//! `[serve] request_log`: a deployment that names a file gets one JSON line per viewer-plane and
//! session-plane request in it, with the session's `token_id` and never a token or credential; a
//! restarted server appends; a deployment that names none writes nothing.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};
use tempfile::TempDir;

use common::{wait_until, write_deployment, OPERATOR_CREDENTIAL, SESSION_CREDENTIAL};
use tessera_server::state::AppState;

/// A server prepared from the deployment at `deployment`, as `tessera serve` prepares one, with
/// its viewer and session routers served on tasks this test can stop.
struct Running {
    state: Arc<AppState>,
    serving: Vec<tokio::task::JoinHandle<()>>,
    viewer: String,
    session: String,
}

async fn start(deployment: &Path) -> Running {
    let prepared = tessera_server::prepare(deployment).expect("the deployment must start");
    let state = prepared.state;
    let mut serving = Vec::new();
    let mut urls = Vec::new();
    for router in [
        tessera_server::viewer::router(Arc::clone(&state)),
        tessera_server::session::router(Arc::clone(&state)),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        urls.push(format!("http://{}", listener.local_addr().unwrap()));
        serving.push(tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        }));
    }
    let [viewer, session] = <[String; 2]>::try_from(urls).unwrap();
    Running {
        state,
        serving,
        viewer,
        session,
    }
}

impl Running {
    /// Stops serving and waits until the state, and with it the log's writer, is dropped. The
    /// caller drops its client first, so no kept-alive connection holds the state.
    async fn stop(self) {
        for task in &self.serving {
            task.abort();
        }
        for task in self.serving {
            let _ = task.await;
        }
        let state = self.state;
        wait_until(
            "the connection tasks releasing the state",
            Duration::from_secs(30),
            async || Arc::strong_count(&state) == 1,
        )
        .await;
        drop(state);
    }

    async fn authorise(&self, client: &reqwest::Client) -> Value {
        let auth_data = base64::engine::general_purpose::STANDARD.encode(r#"{"terms":["0"]}"#);
        let resp = client
            .post(format!("{}/session/authorise", self.session))
            .bearer_auth(SESSION_CREDENTIAL)
            .json(&json!({ "auth_data": auth_data }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        resp.json().await.unwrap()
    }
}

fn lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
        .collect()
}

const VIEWPORT: &str = r#"{"view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 5}"#;

#[tokio::test(flavor = "multi_thread")]
async fn each_request_is_one_line_with_its_session_and_no_secret() {
    let tmp = TempDir::new().unwrap();
    // One request computes at a time, and a queued one waits a minute, so the test can hold the
    // gate and leave a viewport waiting for a client to give up on.
    let deployment = write_deployment(
        tmp.path(),
        "127.0.0.1:0",
        "request_log = \"requests.jsonl\"\ncompute_admission = 1\ncompute_queue = 4\n\
         admission_timeout_ms = 60000",
    );
    let log = tmp.path().join("requests.jsonl");
    let server = start(&deployment).await;
    let client = reqwest::Client::new();

    let auth = server.authorise(&client).await;
    let token = auth["token"].as_str().unwrap().to_string();
    let token_id = auth["token_id"].as_u64().unwrap();

    let meta = client
        .get(format!("{}/v1/meta", server.viewer))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(meta.status(), 200);
    let meta_bytes = meta.bytes().await.unwrap().len() as u64;

    let viewport = client
        .post(format!("{}/v1/viewport?probe=1", server.viewer))
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .body(VIEWPORT)
        .send()
        .await
        .unwrap();
    assert_eq!(viewport.status(), 200);
    let viewport_bytes = viewport.bytes().await.unwrap().len() as u64;

    let items = client
        .post(format!("{}/v1/items", server.viewer))
        .bearer_auth(&token)
        .json(&json!({ "view": "s0", "fields": [], "pages": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(items.status(), 200);
    items.bytes().await.unwrap();

    let unknown = client
        .get(format!("{}/v1/meta", server.viewer))
        .bearer_auth("not-a-token")
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 401);

    let health = client
        .get(format!("{}/healthz", server.viewer))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);

    // A viewport the client gives up on while it waits for the compute permit the test holds.
    let held = server.state.compute_gate.admit().await.expect("the gate is free");
    let gave_up = tokio::time::timeout(
        Duration::from_millis(300),
        reqwest::Client::new()
            .post(format!("{}/v1/viewport", server.viewer))
            .bearer_auth(&token)
            .header("content-type", "application/json")
            .body(VIEWPORT)
            .send(),
    )
    .await;
    assert!(gave_up.is_err(), "the viewport must still be waiting for the gate");
    wait_until(
        "the abandoned viewport's line",
        Duration::from_secs(30),
        async || {
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .contains("cancelled")
        },
    )
    .await;
    drop(held);

    drop(client);
    server.stop().await;

    let logged = lines(&log);
    let routes: Vec<(&str, &str, &str)> = logged
        .iter()
        .map(|l| {
            (
                l["plane"].as_str().unwrap(),
                l["method"].as_str().unwrap(),
                l["path"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        routes,
        [
            ("session", "POST", "/session/authorise"),
            ("viewer", "GET", "/v1/meta"),
            ("viewer", "POST", "/v1/viewport?probe=1"),
            ("viewer", "POST", "/v1/items"),
            ("viewer", "GET", "/v1/meta"),
            ("viewer", "POST", "/v1/viewport"),
        ]
    );

    let [authorise, meta, viewport, items, unknown, abandoned] = &logged[..] else {
        unreachable!()
    };
    for line in [authorise, meta, viewport, items] {
        assert_eq!(line["token_id"], token_id, "{line}");
        assert_eq!(line["outcome"], "completed", "{line}");
        assert_eq!(line["status"], 200, "{line}");
        assert!(line["start_us"].as_u64().unwrap() > 0);
        assert!(line["headers_us"].as_u64().unwrap() <= line["end_us"].as_u64().unwrap());
    }
    // The body the replayer re-sends to mint the same principal.
    assert_eq!(
        authorise["body"]["auth_data"],
        base64::engine::general_purpose::STANDARD.encode(r#"{"terms":["0"]}"#)
    );
    assert_eq!(viewport["body"], serde_json::from_str::<Value>(VIEWPORT).unwrap());
    assert_eq!(meta["bytes"], meta_bytes);
    assert_eq!(viewport["bytes"], viewport_bytes);
    assert!(meta["admission_us"].is_null(), "meta is not gated");
    assert!(viewport["admission_us"].is_u64(), "the viewport waited for the gate");

    assert_eq!(unknown["status"], 401);
    assert!(unknown["token_id"].is_null());

    assert_eq!(abandoned["outcome"], "cancelled");
    assert!(abandoned["status"].is_null(), "it never answered");
    assert_eq!(abandoned["token_id"], token_id);

    let text = std::fs::read_to_string(&log).unwrap();
    for secret in [token.as_str(), SESSION_CREDENTIAL, OPERATOR_CREDENTIAL] {
        assert!(!text.contains(secret), "the log must not hold {secret:?}");
    }
    // It holds `auth_data`, so only its owner may read it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&log).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let run = logged[0]["run"].as_str().expect("each line names its run");
    assert!(logged.iter().all(|l| l["run"] == run));

    // A restarted server appends after the lines already there.
    let server = start(&deployment).await;
    let client = reqwest::Client::new();
    server.authorise(&client).await;
    drop(client);
    server.stop().await;
    let after = lines(&log);
    assert_eq!(after.len(), logged.len() + 1);
    assert_eq!(after[..logged.len()], logged[..]);
    assert_eq!(after[logged.len()]["path"], "/session/authorise");
    assert_ne!(
        after[logged.len()]["run"], run,
        "a restarted server's lines are told apart from the first's, whose token_ids it reuses"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deployment_naming_no_log_writes_none() {
    let tmp = TempDir::new().unwrap();
    let deployment = write_deployment(tmp.path(), "127.0.0.1:0", "");
    let before: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let server = start(&deployment).await;
    let client = reqwest::Client::new();
    let auth = server.authorise(&client).await;
    let meta = client
        .get(format!("{}/v1/meta", server.viewer))
        .bearer_auth(auth["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(meta.status(), 200);
    drop(client);
    server.stop().await;

    // The server's own files are the cache and the write-ahead log; nothing else is new.
    let new: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|name| !before.contains(name))
        .filter(|name| {
            let name = name.to_string_lossy();
            name != "cache" && !name.starts_with("wal")
        })
        .collect();
    assert!(new.is_empty(), "no file but the cache and log: {new:?}");
}

/// A body larger than the log keeps is logged by its size alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_body_too_large_to_keep_is_logged_by_its_size() {
    let tmp = TempDir::new().unwrap();
    let deployment = write_deployment(
        tmp.path(),
        "127.0.0.1:0",
        "request_log = \"requests.jsonl\"",
    );
    let log = tmp.path().join("requests.jsonl");
    let server = start(&deployment).await;
    let client = reqwest::Client::new();
    let auth = server.authorise(&client).await;
    // Over a mebibyte of filter values: still JSON, and still under axum's body limit.
    let names: Vec<String> = (0..120_000).map(|i| format!("v{i:05}")).collect();
    let body = json!({
        "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 5,
        "filters": { "nothing": { "in": names } },
    })
    .to_string();
    assert!(body.len() > 1 << 20);
    client
        .post(format!("{}/v1/viewport", server.viewer))
        .bearer_auth(auth["token"].as_str().unwrap())
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    drop(client);
    server.stop().await;

    let logged = lines(&log);
    let viewport = logged.iter().find(|l| l["path"] == "/v1/viewport").unwrap();
    assert_eq!(viewport["body_bytes"], body.len() as u64);
    assert!(viewport.get("body").is_none());
}
