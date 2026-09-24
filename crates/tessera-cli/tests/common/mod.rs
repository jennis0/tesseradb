//! What the CLI's tests of a served bundle share: the binary, a generated deployment built into a
//! bundle, and a server process stopped when its test ends.

// Each test binary compiles this module on its own, so a helper one binary does not use is dead
// code there.
#![allow(dead_code)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const KEY: &str = "000102030405060708090a0b0c0d0e0f";
pub const SESSION_CREDENTIAL: &str = "session-credential";

pub fn tessera() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tessera"))
}

/// A port nothing is listening on, taken from the kernel and released.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The ports a deployment's viewer and session planes listen on.
pub struct Ports {
    pub viewer: u16,
    pub session: u16,
}

/// A generated corpus of 2,000 items built into a bundle in `dir`, with external ids and the viewer
/// on `0.0.0.0`, so that a health check has to reach it on loopback.
pub fn deployment(dir: &Path) -> Ports {
    let materialised = tessera()
        .args(["corpus", "materialise", "--seed", "1", "--n", "2000", "--out"])
        .arg(dir)
        .output()
        .unwrap();
    assert!(materialised.status.success(), "{materialised:?}");
    std::fs::rename(dir.join("corpus-config.toml"), dir.join("schema.toml")).unwrap();
    std::fs::write(dir.join("session.cred"), SESSION_CREDENTIAL).unwrap();
    std::fs::write(dir.join("operator.cred"), "operator-credential").unwrap();
    let ports = Ports {
        viewer: free_port(),
        session: free_port(),
    };
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
            ports.viewer,
            ports.session,
            free_port()
        ),
    )
    .unwrap();
    let built = tessera()
        .args(["build", "--mint-external-ids"])
        .current_dir(dir)
        .env("TESSERA_IDENTITY_KEY", KEY)
        .output()
        .unwrap();
    assert!(built.status.success(), "{built:?}");
    ports
}

pub fn healthy(dir: &Path) -> bool {
    tessera()
        .arg("health")
        .current_dir(dir)
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// `tessera serve` over the deployment in `dir`, killed when this drops.
pub struct Server(pub Child);

impl Server {
    /// Start the server and wait until it answers its health check.
    pub fn start(dir: &Path) -> Server {
        let server = Server(
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
        server
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
