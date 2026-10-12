//! The flush's laps partition its wall clock (write-path §4; `FlushStage`).
//!
//! Two spans, one per thread. The pool's `execute_flush` is partitioned by `FlushStage::EXECUTE`
//! and measured whole as `PoolWall`; the executor's `publish_flush` is partitioned by
//! `FlushStage::PUBLISH` and measured whole as `PublishWall`. Each partition must sum to within
//! a small slack of its wall, or the stages leave part of the flush unaccounted for and a reader
//! attributing the flush's cost at scale would be told the residue is nothing.
//!
//! Runs only under `bench-timing`: without it every lap is zero by construction, and the
//! `mosaica-engine` self dev-dependency turns the feature on for `cargo test -p mosaica-engine`.

#![cfg(feature = "bench-timing")]

mod common;

use std::time::Duration;

use common::*;
use mosaica_engine::{Engine, EngineConfig, FlushStage};

const WAIT: Duration = Duration::from_secs(30);

fn ingest_rows(engine: &Engine, batch: &str, n: usize) {
    let rows: Vec<mosaica_lifecycle::UnallocatedRow> = (0..n)
        .map(|i| mosaica_lifecycle::UnallocatedRow {
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
        .ingest_rows(rows, batch.to_string(), key)
        .expect("the batch is accepted");
}

/// The slack a partition is allowed against its wall: the clock reads themselves and the few
/// statements between the last lap and the wall's. Five per cent or a millisecond, whichever is
/// larger, so a slow box does not fail a test whose subject is the sum.
fn slack(wall: u64) -> u64 {
    (wall / 20).max(1_000_000)
}

/// Flushes the test publishes. A span charged to no stage recurs in every one, where the
/// scheduler taking the thread between the last lap and the wall's lands in one; so a partition is
/// judged on its smallest gap across them.
const FLUSHES: usize = 5;

/// One flush's `(unattributed, wall)` under `wall`, from its laps alone.
fn gap(name: &str, nanos: &[u64], stages: &[FlushStage], wall: FlushStage) -> (u64, u64) {
    let wall_ns = nanos[wall as usize];
    let sum: u64 = stages.iter().map(|s| nanos[*s as usize]).sum();
    assert!(wall_ns > 0, "{name}: the wall clock was measured");
    assert!(
        sum <= wall_ns,
        "{name}: the stages sum to {sum} ns, more than their wall {wall_ns} ns; a stage is \
         counted twice or lapped outside the span"
    );
    (wall_ns - sum, wall_ns)
}

fn assert_partition(name: &str, gaps: &[(u64, u64)]) {
    let (unattributed, wall) = *gaps.iter().min().expect("a flush was measured");
    assert!(
        unattributed <= slack(wall),
        "{name}: at least {unattributed} ns of a {wall} ns wall is unattributed in every one of \
         {} flushes (unattributed, wall: {gaps:?}), more than the slack; a span between two laps \
         is charged to no stage",
        gaps.len()
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
        EngineConfig {
            // Only the requests below publish, so every lap read here is one flush's.
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
    let mut before = [0u64; FlushStage::COUNT];
    let mut execute_gaps = Vec::new();
    let mut publish_gaps = Vec::new();
    for flush in 1..=FLUSHES {
        if flush > 1 {
            ingest_rows(&engine, &format!("batch-{flush}"), 20);
        }
        engine.request_flush();
        // `PublishWall` is lapped after `publish_flush` returns, so it moving is what says every
        // publication stage has been charged; `flushes` moves before the last two.
        wait_until("the flush publishes and its wall is lapped", WAIT, || {
            let stats = engine.write_executor_stats();
            stats.flushes >= flush as u64
                && stats.flush_stage_nanos[FlushStage::PublishWall as usize]
                    > before[FlushStage::PublishWall as usize]
        });
        let stats = engine.write_executor_stats();
        assert_eq!(stats.flushes, flush as u64);
        assert_eq!(
            stats.flush_executions, flush as u64,
            "one execute_flush returned on the pool per flush"
        );
        assert_eq!(stats.flush_rows_executed, 150 + 20 * (flush as u64 - 1));
        assert_eq!(stats.flush_rows_published, stats.flush_rows_executed);
        let after = stats.flush_stage_nanos;
        for stage in FlushStage::EXECUTOR.iter().chain(FlushStage::POOL.iter()) {
            assert!(
                after[*stage as usize] >= before[*stage as usize],
                "{} is a running total and cannot fall",
                stage.name()
            );
        }
        let laps: Vec<u64> = after.iter().zip(&before).map(|(a, b)| a - b).collect();
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
                laps[stage as usize] > 0,
                "{} does work on every flush and must be lapped",
                stage.name()
            );
        }
        execute_gaps.push(gap(
            "execute_flush",
            &laps,
            &FlushStage::EXECUTE,
            FlushStage::PoolWall,
        ));
        publish_gaps.push(gap(
            "publish_flush",
            &laps,
            &FlushStage::PUBLISH,
            FlushStage::PublishWall,
        ));
        before = after;
    }
    assert_partition("execute_flush", &execute_gaps);
    assert_partition("publish_flush", &publish_gaps);
}
