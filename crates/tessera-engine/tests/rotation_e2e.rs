//! Rotation end to end: a published flush reclaims the log, and nothing acked is lost doing it.
//!
//! `tessera-lifecycle`'s `rotation.rs` pins the sequence's own properties — oldest-first deletion,
//! the gap refusal, the position chain. What needs a whole engine is the two things the ordering
//! exists for, because both need a flush to have happened: that a row acked while a flush was in
//! flight survives the rotation that follows it, and that a suppression outlives the reclamation of
//! the `Change` record that carried it.

mod common;

use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::{ChangeOp, UnallocatedRow};
use tessera_types::EntityId;

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn fixture(tmp: &std::path::Path) -> std::path::PathBuf {
    let root = tmp.join("bundle");
    build_fixture(
        &root,
        &tmp.join("points.parquet"),
        &tmp.join("pairs.parquet"),
    );
    root
}

fn engine_at(tmp: &std::path::Path, root: &std::path::Path, tick_secs: u64) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: tick_secs,
            max_merged_segment_bytes: None,
            // Compaction §9's trigger is off unless a deployment configures one.
            compaction: tessera_engine::CompactionSchedule::off(),
            ..config()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    engine
}

fn ingest(engine: &Engine, external_id: &str) -> EntityId {
    let row = UnallocatedRow {
        external_id: Some(external_id.as_bytes().to_vec()),
        view: "s0".to_string(),
        join: None,
        descriptors: vec![b"0".to_vec()],
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[b"0".to_vec()]),
    };
    engine
        .accept_ingest(vec![row], external_id.to_string(), [0u8; 32])
        .expect("ingest is accepted")[0]
}

fn members(tmp: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(tmp)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("wal-") && n.ends_with(".log"))
        .collect();
    names.sort();
    names
}

/// **A flush rotates the log**, which is the whole point of the sequence: without this the WAL
/// grows for the life of the deployment, because nothing else can reclaim the front of it.
#[test]
fn a_published_flush_rotates_the_log() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let engine = engine_at(tmp.path(), &root, 1);

    assert_eq!(members(tmp.path()), vec!["wal-000001.log"]);

    ingest(&engine, "ext-1");
    wait_until("the flush to publish", || {
        engine.write_executor_stats().flushes >= 1
    });
    wait_until("the rotation to follow it", || {
        members(tmp.path()).len() >= 2
    });

    // The buffer is empty — everything ingested has geometry — so the whole durable prefix was
    // reclaimable and member 1 goes. What survives is the member the snapshot was just written to.
    wait_until("member 1 to be reclaimed", || {
        !members(tmp.path()).contains(&"wal-000001.log".to_string())
    });
}

/// **A row acked while a flush was in flight survives the rotation that follows it.**
///
/// This is `wal_pos`'s definition, pinned by its consequence rather than by its prose. Under the
/// other reading — `wal_pos` is the `Flush` record's own offset — rotation deletes rows appended
/// after the flush's snapshot point, which were never consumed and carry entity ids at or above the
/// new watermark. §7.1 then reconstructs them from nothing: acked ingest, silently lost at the next
/// restart.
///
/// The second row is ingested while the first flush is still running, so it is in the buffer, not
/// in a segment, when the rotation decides what to reclaim.
#[test]
fn a_row_acked_during_a_flush_survives_rotation_and_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());

    let second = {
        // A slow tick, so the two ingests land in different flushes rather than the same one.
        let engine = engine_at(tmp.path(), &root, 1);
        ingest(&engine, "ext-1");
        wait_until("the first flush", || {
            engine.write_executor_stats().flushes >= 1
        });

        // Acked after that flush consumed the first row, so it is buffered when the rotation runs.
        let second = ingest(&engine, "ext-2");
        assert!(engine.generation().buffer.contains(second));
        wait_until("a rotation", || members(tmp.path()).len() >= 2);
        second
    };

    // The reopened engine must find the second row somewhere — buffered from a surviving WAL
    // member, or already flushed into a segment. What it must not be is nowhere.
    let reopened = engine_at(tmp.path(), &root, 3600);
    let bundle = tessera_store::open_bundle(&root).expect("the bundle opens");
    let partition = bundle.partitions.values().next().unwrap();
    let has_geometry = partition.views["s0"].row_space.row_of(second).is_some();
    assert!(
        reopened.generation().buffer.contains(second) || has_geometry,
        "an acked ingest was silently lost: entity {} is in neither the buffer nor a segment after \
         a rotation and a restart",
        second.raw()
    );
}

/// **A suppression outlives the reclamation of the `Change` record that carried it.**
///
/// The overlay's only durable home is the WAL, and a suppression retires *only* on unsuppress — so
/// the rotation's snapshot is the sole thing standing between a reclaimed member and an item that
/// silently comes back at the next restart.
#[test]
fn a_suppression_accepted_before_a_rotation_is_still_in_force_after_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());

    let suppressed = {
        let engine = engine_at(tmp.path(), &root, 1);
        let id = ingest(&engine, "ext-1");
        engine
            .accept_change(id, ChangeOp::Suppress)
            .expect("the suppression is accepted");

        wait_until("the flush", || engine.write_executor_stats().flushes >= 1);
        wait_until("member 1 to be reclaimed", || {
            !members(tmp.path()).contains(&"wal-000001.log".to_string())
        });
        id
    };

    let reopened = engine_at(tmp.path(), &root, 3600);
    assert!(
        reopened.generation().overlay.is_suppressed(suppressed),
        "the record that carried this suppression was reclaimed; only the rotation's snapshot \
         stands between that and a re-exposed item"
    );
}

/// **A row deleted before its first flush does not pin the log** (write-path §4.2).
///
/// A deleted row acquires no geometry — `plan_flush` skips it — so no flush ever consumes it, and
/// before the delete removed it from the buffer it sat there for the process's lifetime holding
/// `oldest_wal_pos` down: its member, and every member after it, unreclaimable. A deployment that
/// deletes before flushing therefore stopped reclaiming the log at all, which is the one lane that
/// structurally cannot be shed.
///
/// The end state asserted here is decision 0047's *forgotten*: no row, no buffer entry, the id
/// still burned, and the deny still in force across a restart from the rotation's snapshot alone.
#[test]
fn a_row_deleted_before_its_first_flush_stops_pinning_the_log() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());

    let deleted = {
        let engine = engine_at(tmp.path(), &root, 1);
        let id = ingest(&engine, "ext-1");
        assert!(engine.generation().buffer.contains(id));
        engine
            .accept_change(id, ChangeOp::Delete)
            .expect("the delete is accepted");
        assert!(
            !engine.generation().buffer.contains(id),
            "the delete must drop the row from the buffer; nothing else ever will"
        );

        // No flush publishes — the one buffered row was deleted — so what rotates is the tick's
        // own growth-gated rotation, and it may only reclaim because the buffer is now empty.
        wait_until("member 1 to be reclaimed", || {
            !members(tmp.path()).contains(&"wal-000001.log".to_string())
        });
        id
    };

    let reopened = engine_at(tmp.path(), &root, 3600);
    let generation = reopened.generation();
    assert!(
        generation.overlay.is_deleted(deleted),
        "the record that carried this deletion was reclaimed; the rotation's snapshot is what \
         keeps it in force"
    );
    assert!(
        !generation.buffer.contains(deleted),
        "a reclaimed row must reconstruct nothing — the entity is forgotten (decision 0047)"
    );
    let bundle = tessera_store::open_bundle(&root).expect("the bundle opens");
    let partition = bundle.partitions.values().next().unwrap();
    assert!(
        partition.views["s0"].row_space.row_of(deleted).is_none(),
        "and it acquired no geometry on the way out"
    );
    assert!(
        reopened.allocator_high_water() >= deleted.raw(),
        "the id stays burned (I9) — a deletion never returns one to the allocator"
    );
}
