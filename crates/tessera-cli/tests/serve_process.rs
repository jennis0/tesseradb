//! `tessera serve` as a process, and `tessera health` asking it whether it is ready: what a
//! container runs and what its health check runs.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const KEY: &str = "000102030405060708090a0b0c0d0e0f";

fn tessera() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tessera"))
}

/// A port nothing is listening on, taken from the kernel and released.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A generated corpus built into a bundle, with the viewer on `0.0.0.0` so the health check has
/// to reach it on loopback.
fn deployment(dir: &Path) {
    let materialised = tessera()
        .args(["corpus", "materialise", "--seed", "1", "--n", "2000", "--out"])
        .arg(dir)
        .output()
        .unwrap();
    assert!(materialised.status.success(), "{materialised:?}");
    std::fs::rename(dir.join("corpus-config.toml"), dir.join("schema.toml")).unwrap();
    std::fs::write(dir.join("session.cred"), "session-credential").unwrap();
    std::fs::write(dir.join("operator.cred"), "operator-credential").unwrap();
    std::fs::write(
        dir.join("tessera.toml"),
        format!(
            r#"
[bundle]
path  = "bundle"
cache = "cache"
wal   = "wal.log"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "0.0.0.0:{}"
session = "127.0.0.1:{}"
control = "127.0.0.1:{}"
session_credential_file  = "session.cred"
operator_credential_file = "operator.cred"
"#,
            free_port(),
            free_port(),
            free_port()
        ),
    )
    .unwrap();
    let built = tessera()
        .arg("build")
        .current_dir(dir)
        .env("TESSERA_IDENTITY_KEY", KEY)
        .output()
        .unwrap();
    assert!(built.status.success(), "{built:?}");
}

fn healthy(dir: &Path) -> bool {
    tessera()
        .arg("health")
        .current_dir(dir)
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// Kills the server if the test fails before it is stopped.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn health_follows_the_server_and_sigterm_stops_it_cleanly() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    deployment(dir);

    assert!(!healthy(dir), "nothing is serving yet");

    let mut server = Server(
        tessera()
            .arg("serve")
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    while !healthy(dir) {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the server did not become ready within a minute"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let signalled = Command::new("kill")
        .args(["-TERM", &server.0.id().to_string()])
        .status()
        .unwrap();
    assert!(signalled.success());
    let stopping = Instant::now();
    let status = loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            stopping.elapsed() < Duration::from_secs(10),
            "the server did not exit within ten seconds of SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "SIGTERM ends serve with {status}");
    assert!(!healthy(dir), "nothing is serving after the stop");
}
