//! The flush's laps partition its wall clock (write-path §4; `FlushStage`).
//!
//! Two spans, one per thread. The pool's `execute_flush` is partitioned by `FlushStage::EXECUTE`
//! and measured whole as `PoolWall`; the executor's `publish_flush` is partitioned by
//! `FlushStage::PUBLISH` and measured whole as `PublishWall`. Each partition must sum to within
//! a small slack of its wall, or the stages leave part of the flush unaccounted for and a reader
//! attributing the flush's cost at scale would be told the residue is nothing.
//!
//! Runs only under `bench-timing`: without it every lap is zero by construction, and the
//! `tessera-engine` self dev-dependency turns the feature on for `cargo test -p tessera-engine`.

#![cfg(feature = "bench-timing")]

mod common;

use std::time::Duration;

use common::*;
use tessera_engine::{Engine, EngineConfig, FlushStage};

const WAIT: Duration = Duration::from_secs(30);

fn ingest_rows(engine: &Engine, batch: &str, n: usize) {
    let rows: Vec<tessera_lifecycle::UnallocatedRow> = (0..n)
        .map(|i| tessera_lifecycle::UnallocatedRow {
            external_id: Some(format!("{batch}-{i}").into_bytes()),
            view: "s0".to_string(),
            join: None,
            descriptors: vec![b"0".to_vec()],
            x: 0.25 + (i as f64) / (4.0 * n as f64),
            y: 0.5,
            scalars: Vec::new(),
            terms: engine.resolve_terms(&[b"0".to_vec()]),
            scoped: Vec::new(),
        })
        .collect();
    let mut key = [0u8; 32];
    key[..batch.len().min(32)].copy_from_slice(&batch.as_bytes()[..batch.len().min(32)]);
    engine
        .accept_ingest(rows, batch.to_string(), key)
        .expect("the batch is accepted");
}

/// The slack a partition is allowed against its wall: the clock reads themselves and the few
/// statements between the last lap and the wall's. Five per cent or a millisecond, whichever is
/// larger, so a slow box does not fail a test whose subject is the sum.
fn slack(wall: u64) -> u64 {
    (wall / 20).max(1_000_000)
}

fn assert_partition(name: &str, nanos: &[u64], stages: &[FlushStage], wall: FlushStage) {
    let wall_ns = nanos[wall as usize];
    let sum: u64 = stages.iter().map(|s| nanos[*s as usize]).sum();
    assert!(wall_ns > 0, "{name}: the wall clock was measured");
    assert!(
        sum <= wall_ns,
        "{name}: the stages sum to {sum} ns, more than their wall {wall_ns} ns; a stage is \
         counted twice or lapped outside the span"
    );
    assert!(
        wall_ns - sum <= slack(wall_ns),
        "{name}: {} ns of the {wall_ns} ns wall is unattributed, more than the slack; a span \
         between two laps is charged to no stage",
        wall_ns - sum
    );
}

#[test]
fn the_flush_stages_partition_both_walls_and_accumulate_across_flushes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            // Only the request below publishes, so every lap read here is one flush's.
            flush_max_age_secs: 3600,
            ..config()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(8)
        .expect("the executor starts once");

    for batch in ["a", "b", "c"] {
        ingest_rows(&engine, batch, 50);
    }
    engine.request_flush();
    // `PublishWall` is lapped after `publish_flush` returns, so it moving is what says every
    // publication stage has been charged; `flushes` moves before the last two.
    wait_until(
        "the first flush publishes and its wall is lapped",
        WAIT,
        || engine.write_executor_stats().flush_stage_nanos[FlushStage::PublishWall as usize] > 0,
    );

    let stats = engine.write_executor_stats();
    assert_eq!(stats.flushes, 1);
    assert_eq!(
        stats.flush_executions, 1,
        "one execute_flush returned on the pool"
    );
    assert_eq!(stats.flush_rows_executed, 150);
    assert_eq!(stats.flush_rows_published, 150);
    let nanos = stats.flush_stage_nanos;
    assert_partition(
        "execute_flush",
        &nanos,
        &FlushStage::EXECUTE,
        FlushStage::PoolWall,
    );
    assert_partition(
        "publish_flush",
        &nanos,
        &FlushStage::PUBLISH,
        FlushStage::PublishWall,
    );
    for stage in [
        FlushStage::Plan,
        FlushStage::Dispatch,
        FlushStage::Segment,
        FlushStage::Digests,
        FlushStage::Commit,
        FlushStage::BufferRebase,
        FlushStage::Swap,
        FlushStage::Rotate,
    ] {
        assert!(
            nanos[stage as usize] > 0,
            "{} does work on every flush and must be lapped",
            stage.name()
        );
    }

    // A second flush adds to the totals rather than replacing them, and both partitions hold
    // over the accumulated figures too.
    ingest_rows(&engine, "d", 20);
    engine.request_flush();
    wait_until("the second flush publishes", WAIT, || {
        engine.write_executor_stats().flushes >= 2
            && engine.write_executor_stats().flush_stage_nanos[FlushStage::PublishWall as usize]
                > nanos[FlushStage::PublishWall as usize]
    });
    let after = engine.write_executor_stats();
    assert_eq!(after.flush_executions, 2);
    assert_eq!(after.flush_rows_executed, 170);
    assert_eq!(after.flush_rows_published, 170);
    for stage in FlushStage::EXECUTOR.iter().chain(FlushStage::POOL.iter()) {
        assert!(
            after.flush_stage_nanos[*stage as usize] >= nanos[*stage as usize],
            "{} is a running total and cannot fall",
            stage.name()
        );
    }
    assert_partition(
        "execute_flush, two flushes",
        &after.flush_stage_nanos,
        &FlushStage::EXECUTE,
        FlushStage::PoolWall,
    );
    assert_partition(
        "publish_flush, two flushes",
        &after.flush_stage_nanos,
        &FlushStage::PUBLISH,
        FlushStage::PublishWall,
    );
}
