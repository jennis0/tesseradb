//! The announce line: `serve` binds what the deployment declares, including a TCP address whose
//! port is 0, and writes one JSON line naming the three addresses it bound.
//!
//! A supervisor that started the process reads that line and sends its first request to the
//! address in it, so what these tests pin is the line's exact text, that it is the only thing on
//! the stream, and that the announced viewer address is already answering when it appears.
//!
//! The process writes the line to stdout; `serve_announcing` is the same code with the stream
//! supplied, which is the seam a test can read. That `tessera serve` passes stdout, and its
//! diagnostics stderr, is in `tessera-cli`.

mod common;

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use common::{build_fixture, wait_for, OPERATOR_CREDENTIAL, SESSION_CREDENTIAL};

/// The announce stream, readable from the test while the server holds it.
#[derive(Clone)]
struct SharedStream(Arc<Mutex<Vec<u8>>>);

impl SharedStream {
    fn new() -> SharedStream {
        SharedStream(Arc::new(Mutex::new(Vec::new())))
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).expect("the announce line is UTF-8")
    }
}

impl Write for SharedStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A deployment over a fixture bundle, with the three planes as `control` names them.
fn write_deployment(tmp: &Path, control: &str) -> std::path::PathBuf {
    let bundle_root = tmp.join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.join("points.parquet"),
        &tmp.join("pairs.parquet"),
    );
    let session_credential = tmp.join("session.cred");
    let operator_credential = tmp.join("operator.cred");
    std::fs::write(&session_credential, SESSION_CREDENTIAL).unwrap();
    std::fs::write(&operator_credential, OPERATOR_CREDENTIAL).unwrap();

    let text = format!(
        r#"
        [bundle]
        path = "{bundle}"
        cache = "{cache}"
        wal = "{wal}"
        [plugin]
        module = "builtin:passthrough"
        [disclosure]
        token_max_lifetime = 3600
        [serve]
        viewer = "127.0.0.1:0"
        session = "127.0.0.1:0"
        control = "{control}"
        session_credential_file = "{session_cred}"
        operator_credential_file = "{operator_cred}"
        "#,
        bundle = bundle_root.display(),
        cache = tmp.join("cache").display(),
        wal = tmp.join("wal.log").display(),
        session_cred = session_credential.display(),
        operator_cred = operator_credential.display(),
    );
    let path = tmp.join("tessera.toml");
    std::fs::write(&path, text).unwrap();
    path
}

/// The first complete line on the stream, waited for rather than raced against.
async fn announce_line(stream: &SharedStream) -> String {
    wait_for(
        "the server announcing its addresses",
        std::time::Duration::from_secs(60),
        async || {
            let text = stream.text();
            match text.split_once('\n') {
                Some((line, _)) => Ok(line.to_string()),
                None => Err(format!("the stream holds {text:?}")),
            }
        },
    )
    .await
}

/// Port 0 on all three planes: each is bound to a port the kernel chose, the line names the ports
/// actually bound, and the viewer plane answers at the address it names.
#[tokio::test(flavor = "multi_thread")]
async fn port_zero_planes_announce_the_ports_the_kernel_chose() {
    let tmp = TempDir::new().unwrap();
    let deployment = write_deployment(tmp.path(), "127.0.0.1:0");
    let prepared = tessera_server::prepare(&deployment).expect("the deployment must start");

    let stream = SharedStream::new();
    let serving = tokio::spawn({
        let stream = stream.clone();
        async move { tessera_server::serve_announcing(prepared, stream).await }
    });

    let line = announce_line(&stream).await;
    let announced: serde_json::Value = serde_json::from_str(&line)
        .unwrap_or_else(|e| panic!("the line must be JSON: {e}, {line}"));
    let viewer = announced["viewer"]
        .as_str()
        .expect("a viewer address")
        .to_string();
    let session = announced["session"]
        .as_str()
        .expect("a session address")
        .to_string();
    let control = announced["control"]
        .as_str()
        .expect("a control address")
        .to_string();

    // The exact text a supervisor parses: one object, these four keys, this order.
    assert_eq!(
        line,
        format!(
            r#"{{"event":"listening","viewer":"{viewer}","session":"{session}","control":"{control}"}}"#
        )
    );
    for (what, addr) in [
        ("viewer", &viewer),
        ("session", &session),
        ("control", &control),
    ] {
        let addr: std::net::SocketAddr = addr.parse().expect("an announced address parses");
        assert_ne!(addr.port(), 0, "the {what} port must be the bound one");
    }

    let client = reqwest::Client::new();
    let health = client
        .get(format!("http://{viewer}/healthz"))
        .send()
        .await
        .expect("the announced viewer address must accept a request");
    assert_eq!(health.status(), 200);
    // The viewer router, not just a socket: `/v1/meta` costs a session token, so an unauthenticated
    // request is refused by the handler rather than by the absence of a route.
    let meta = client
        .get(format!("http://{viewer}/v1/meta"))
        .send()
        .await
        .expect("the viewer plane must answer");
    assert_eq!(meta.status(), 401);
    for addr in [&session, &control] {
        tokio::net::TcpStream::connect(addr)
            .await
            .unwrap_or_else(|e| panic!("the announced address {addr} must be listening: {e}"));
    }

    // One line, and nothing after it: the supervisor reads the first line of the stream and stops.
    assert_eq!(
        stream.text(),
        format!("{line}\n"),
        "the announce stream carries the one line"
    );
    serving.abort();
}

/// A unix-socket control plane is announced as `unix:` and its path, so the supervisor has the
/// one address per plane whatever the plane is listening on.
#[tokio::test(flavor = "multi_thread")]
async fn a_unix_control_plane_is_announced_by_its_path() {
    let tmp = TempDir::new().unwrap();
    let socket = tmp.path().join("control.sock");
    let deployment = write_deployment(tmp.path(), &format!("unix:{}", socket.display()));
    let prepared = tessera_server::prepare(&deployment).expect("the deployment must start");

    let stream = SharedStream::new();
    let serving = tokio::spawn({
        let stream = stream.clone();
        async move { tessera_server::serve_announcing(prepared, stream).await }
    });

    let line = announce_line(&stream).await;
    let announced: serde_json::Value = serde_json::from_str(&line).expect("the line must be JSON");
    assert_eq!(
        announced["control"].as_str(),
        Some(format!("unix:{}", socket.display()).as_str())
    );
    tokio::net::UnixStream::connect(&socket)
        .await
        .expect("the announced socket must be listening");
    serving.abort();
}
