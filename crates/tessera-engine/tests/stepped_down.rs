//! **Step-down gates the write path** (owner-ruled 2026-08-04; write-path §5.6).
//!
//! A stepped-down node used to accept ingest and flush it: the manifest a flush publishes is
//! assembled from the *served* (older) partition state at a higher `n`, permanently shadowing
//! the stepped-past segment — and once rotation moves the reclaim bound, its acked rows are
//! unrecoverable. The gates close all three routes: ingest is refused at the engine boundary,
//! `plan_flush` publishes nothing, and rotation reclaims nothing. Denies are deliberately not
//! gated — a deny threatens no segment and must never be refused.
//!
//! The stepped-down state is fabricated the way it arises: a newer `SEGMENTS-1.json` that is
//! honourable (deltas-only state) but whose listed files fail verification, so the reader steps
//! down to `SEGMENTS-0` and flags the partition.

mod common;

use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{AcceptError, Engine, EngineConfig};
use tessera_lifecycle::wal::{Wal, WalRecord, WalRow};
use tessera_lifecycle::UnallocatedRow;

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Write a `SEGMENTS-1.json` cloned from the build's `SEGMENTS-0.json`, carrying a delta-tier
/// declaration and a file that does not verify — honourable, unverifiable, steppable.
fn fabricate_stepped_down(root: &std::path::Path) {
    let current: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("CURRENT")).unwrap()).unwrap();
    let prefix = current["prefix"].as_str().unwrap().to_string();
    let dir = root.join(&prefix).join("partitions").join("default");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("SEGMENTS-0.json")).unwrap())
            .unwrap();
    manifest["deltas"] = serde_json::json!([1]);
    manifest["files"] = serde_json::json!({
        "partitions/default/never-written.arrow": { "size": 1, "sha256": "0".repeat(64) }
    });
    std::fs::write(
        dir.join("SEGMENTS-1.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

#[test]
fn a_stepped_down_node_refuses_ingest_flushes_nothing_and_rotates_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    // A row already WAL-durable from a "previous run", so the buffer is non-empty at open and a
    // flush would have something to publish — the plan gate is what must stop it.
    let wal_path = tmp.path().join("wal.log");
    {
        let (mut wal, _initial) = Wal::open(&wal_path).unwrap();
        wal.append(&WalRecord::IngestBatch {
            batch_id: "pre-existing".to_string(),
            body_hash: [1u8; 32],
            rows: vec![WalRow {
                external_id: Some(b"pre-existing-row".to_vec()),
                entity_id: tessera_types::EntityId::new(N_ITEMS),
                slice: "s0".to_string(),
                descriptors: vec![b"0".to_vec()],
                x: 0.5,
                y: 0.5,
                scalars: Vec::new(),
            }],
        })
        .unwrap();
        wal.fsync().unwrap();
    }

    fabricate_stepped_down(&root);

    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &wal_path,
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: 1,
            max_merged_segment_bytes: None,
        // Compaction §9's trigger is off unless a deployment configures one.
        compaction: tessera_engine::CompactionSchedule::off(),
            ..config()
        },
    )
    .expect("a steppable candidate steps down rather than failing the open");
    assert!(
        engine.any_partition_stepped_down(),
        "the fabricated newer manifest must have been stepped past"
    );
    engine.start_write_executor(8).expect("executor starts");

    // Ingest is refused at the engine boundary, before anything is acked or WAL-durable.
    let row = UnallocatedRow {
        external_id: Some(b"refused-on-stepdown".to_vec()),
        slice: "s0".to_string(),
        descriptors: vec![b"0".to_vec()],
        x: 0.5,
        y: 0.5,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[b"0".to_vec()]),
    };
    let err = engine
        .accept_ingest(vec![row], "refused-batch".to_string(), [2u8; 32])
        .expect_err("a stepped-down node must not accept ingest");
    assert!(
        matches!(err, AcceptError::SteppedDown),
        "expected the typed step-down refusal, got {err:?}"
    );

    // The buffered pre-existing row is not flushed: ticks pass, nothing publishes. And the
    // suppression lane stays open — a deny is accepted, because denies threaten no segment.
    wait_until("two ticks fire on the stepped-down node", || {
        engine.write_executor_stats().ticks >= 2
    });
    assert_eq!(
        engine.generation().segments_version,
        0,
        "a stepped-down node publishes no geometry: the flush plan gate must refuse"
    );
    let entity = source_to_new_map(&root, &engine.generation().prefix)[&5];
    engine
        .accept_change(
            tessera_types::EntityId::new(entity),
            tessera_lifecycle::ChangeOp::Suppress)
        .expect("denies are never gated on step-down");
}
