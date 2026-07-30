//! Perf harness (not a test): open the 2.4M-item bundle at `/tmp/tessera-2m4` (built by
//! `viewport.rs`'s `latency_sanity_at_2_4m_p99_under_50ms`, reused here) and then block, so
//! `/usr/bin/time -v` (or any external RSS sampler) can read this process's peak/resident memory
//! after `Engine::open` returns. Used to measure the external-id index's memory contribution
//! before/after the mmap'd binary-search rewrite (perf-extid task).
//!
//! Usage: `cargo run --release --example open_rss -- /tmp/tessera-2m4`

use std::env;
use std::path::PathBuf;

use tempfile::TempDir;
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;

fn main() {
    let bundle_root = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/tessera-2m4"));

    let tmp = TempDir::new().unwrap();
    let engine = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Passthrough::new(),
        EngineConfig {
            token_max_lifetime_secs: 3600,
            max_k: 200,
            k_min: 2,
            k_max_marks: 200,
            theta_target_marks: u64::MAX,
            max_underlay_offset: 4,
            max_underlay_cells: 8192,
        },
    )
    .expect("engine should open the 2.4M bundle");

    // Keep `engine` (and its external-id index) alive until the process is measured and killed.
    println!("opened bundle at {}", bundle_root.display());
    std::mem::forget(engine);
}
