//! `mosaica serve` as a process, and `mosaica health` asking it whether it is ready: what a
//! container runs and what its health check runs.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::{deployment, healthy, Ports, Server};

#[test]
fn health_follows_the_server_and_sigterm_stops_it_cleanly() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    deployment(dir, &Ports::free());

    assert!(!healthy(dir), "nothing is serving yet");

    let mut server = Server::start(dir);

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
