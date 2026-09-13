//! One executor owns a bundle root (write-path §1.2).
//!
//! Every name a publication allocates comes from state one executor holds in memory — the
//! side-manifest number, an entity id, a `seg_id` — and a second writer over the same root takes
//! the same names from the same seed. The allocator's floor over the files on disc keeps a node
//! that meets a second writer from livelocking; this is what keeps the second writer from starting.

mod common;

use common::*;
use tessera_engine::ExecutorStartError;
use tessera_engine::{Engine, EngineConfig};

fn engine_over(root: &std::path::Path, tmp: &tempfile::TempDir, wal: &str) -> Engine {
    Engine::open(
        root,
        &tmp.path().join(format!("cache-{wal}")),
        &tmp.path().join(wal),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: 3600,
            ..config()
        },
    )
    .expect("engine opens")
}

/// **A second executor over one bundle root refuses to start, and the first's exit releases it.**
///
/// Two engines in one process, which is the shape this was found in: a restart whose predecessor
/// is still running. A `fcntl` lock would grant both, being held per process; the lock is `flock`,
/// held per open file description.
///
/// The second engine is given its own WAL, so nothing but the bundle root is shared — a lock on the
/// WAL would let this pair through.
///
/// **Mutation:** remove the `acquire` from `WritePath::start_executor` and the second start
/// succeeds.
#[test]
fn a_second_executor_over_one_bundle_root_refuses_to_start() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let mut first = engine_over(&root, &tmp, "wal-first.log");
    first
        .start_write_executor(8)
        .expect("the first executor starts");

    let mut second = engine_over(&root, &tmp, "wal-second.log");
    let refused = second
        .start_write_executor(8)
        .expect_err("the second executor must refuse");
    let ExecutorStartError::BundleLocked(held) = &refused else {
        panic!("the refusal says the bundle is held, not that this engine started twice: {refused}");
    };
    assert_eq!(
        *held,
        tessera_engine::BundleLockError::Held {
            path: root.clone(),
            holder: Some(std::process::id()),
        },
        "the refusal names the root and the process holding it — here this one, both executors \
         being in one process"
    );
    assert!(
        refused.to_string().contains("bundle"),
        "the message names what is held: {refused}"
    );

    // The refusal left this engine startable: the WAL was not consumed by the attempt.
    drop(first);
    second
        .start_write_executor(8)
        .expect("the lock is released when the first executor's engine is dropped");
}

/// **A reader takes no lock.** `Engine::open` without an executor is the embedder's and the CLI's
/// posture, and a bundle being written must stay openable for reading.
#[test]
fn opening_a_bundle_for_reading_takes_no_lock() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let mut writer = engine_over(&root, &tmp, "wal-writer.log");
    writer
        .start_write_executor(8)
        .expect("the writing executor starts");

    let reader = engine_over(&root, &tmp, "wal-reader.log");
    assert!(
        reader.generation().bundle.partitions.contains_key("default"),
        "the bundle opens for reading while another process writes it"
    );
    tessera_store::open_bundle(&root).expect("and opens through the store as well");
}
