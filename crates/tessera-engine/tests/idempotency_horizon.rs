//! What rotation costs the caller: two identity paths that must keep working once the WAL records
//! behind them are gone, and one that stops.
//!
//! All three are consequences of the WAL retention window becoming finite. They are asserted after
//! a **restart**, because that is the only state in which the live external-id map — rebuilt from
//! whatever WAL records survived — no longer covers what a flush published.
//!
//! All three pass. Getting there needed the flush to carry external ids into its segment at all,
//! contracts §2.4's cross-run ordering requirement to go (it was unsatisfiable by construction),
//! and the reverse direction to consult the locator extent a flush publishes.

mod common;

use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::UnallocatedRow;
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
        slice: "s0".to_string(),
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

fn wal_members(tmp: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(tmp)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("wal-") && n.ends_with(".log"))
        .collect();
    names.sort();
    names
}

/// Flush, then rotate away the member that carried the ingest, then reopen. The returned id is the
/// flushed entity, and its `IngestBatch` record no longer exists anywhere.
fn flushed_then_rotated(tmp: &std::path::Path, root: &std::path::Path, key: &str) -> EntityId {
    let engine = engine_at(tmp, root, 1);
    let id = ingest(&engine, key);
    wait_until("the flush", || engine.write_executor_stats().flushes >= 1);
    wait_until("member 1 to be reclaimed", || {
        !wal_members(tmp).contains(&"wal-000001.log".to_string())
    });
    id
}

/// **`/v1/items` on a flushed item still answers after rotation and a restart.**
///
/// The drill-down resolves `entity → external_id` from the live map first and the locator second.
/// After a rotation the live map no longer carries the entity — its WAL record is gone — so the
/// answer has to come from the **locator extent the flush published**. A build-time locator alone
/// is length `entity_id_high_water` at build, and this entity sits past its end: reading there and
/// returning "no external id" would be a wrong answer wearing a legitimate state's clothes
/// (contracts §2.4 r6).
#[test]
fn a_flushed_item_answers_items_after_rotation_and_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let id = flushed_then_rotated(tmp.path(), &root, "ext-1");

    let reopened = engine_at(tmp.path(), &root, 3600);
    assert_eq!(
        reopened.external_id_of(id).expect("the lookup succeeds"),
        Some(b"ext-1".to_vec()),
        "the flush's locator extent is the only thing left that can answer this"
    );
}

/// An item ingested with **no external id** is not an error after rotation — it is the ordinary
/// case (contracts §2.4 r6), and the locator extent carries `LOCATOR_NONE` for it.
///
/// This is the branch most easily got wrong: the reverse direction now consults flush locator
/// extents, and reading a present-but-absent slot as a failure would turn every id-less item into a
/// typed error the moment its WAL record was reclaimed.
#[test]
fn an_item_with_no_external_id_answers_none_after_rotation_rather_than_erroring() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());

    let anonymous = {
        let engine = engine_at(tmp.path(), &root, 1);
        let row = UnallocatedRow {
            external_id: None,
            slice: "s0".to_string(),
            descriptors: vec![b"0".to_vec()],
            x: 5.0,
            y: 5.0,
            scalars: Vec::new(),
            terms: engine.resolve_terms(&[b"0".to_vec()]),
        };
        let id = engine
            .accept_ingest(vec![row], "anon".to_string(), [0u8; 32])
            .expect("an item with no external id is accepted")[0];
        wait_until("the flush", || engine.write_executor_stats().flushes >= 1);
        wait_until("member 1 to be reclaimed", || {
            !wal_members(tmp.path()).contains(&"wal-000001.log".to_string())
        });
        id
    };

    let reopened = engine_at(tmp.path(), &root, 3600);
    assert_eq!(
        reopened.external_id_of(anonymous).expect("not an error"),
        None,
        "an item addressable only by its tessera_id has no external id, and saying so is the \
         answer — not a typed failure"
    );
}

/// **The ingest duplicate check still catches an id that lives only in a flushed extent.**
///
/// `established_collisions` justified its bundle-side half as unable to go stale because the
/// bundle's sidecar is immutable. A flush publishes *new* external-id extents, and rotation removes
/// the live map's copy — so after a restart the flushed extent is the only place the id exists. The
/// failure this guards is the worst one the write path documents: a byte-identical copy of a
/// document that no external id names, so no deny can ever reach it.
/// This is the case that could not pass while contracts §2.4 required external-id files to
/// partition one ascending order: a flush's keys interleave with the build's, so the run it
/// publishes was refused outright. §2.4 now says runs are ordered only within themselves, and the
/// reader searches every run whose own bounds admit the key.
#[test]
fn a_duplicate_external_id_is_caught_against_a_flushed_run() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    flushed_then_rotated(tmp.path(), &root, "ext-1");

    let reopened = engine_at(tmp.path(), &root, 3600);
    let resolved = reopened
        .resolve_external_ids(&[b"ext-1".to_vec()])
        .expect("the lookup succeeds");
    assert!(
        resolved[0].is_some(),
        "the id survives only in the flush's external-id extent, and the duplicate check has to \
         find it there or admit a second copy no deny can reach"
    );
}

/// **The idempotency window equals the WAL retention window**, and that is a client-visible
/// weakening of contracts §3.4's replay rule rather than a silent regression.
///
/// `accepted_batches` is WAL-replay-derived. Once the member carrying a batch's `IngestBatch`
/// record is reclaimed, a restart no longer recognises that batch id, so a byte-identical retry is
/// not answered as a duplicate. Rows carrying an external id are still caught by the check above;
/// rows carrying none would be ingested twice. A client retrying across the window must therefore
/// supply external ids.
#[test]
fn a_batch_older_than_the_retained_wal_is_no_longer_recognised_as_a_duplicate() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    flushed_then_rotated(tmp.path(), &root, "ext-1");

    let reopened = engine_at(tmp.path(), &root, 3600);
    assert!(
        reopened.accepted_batch("ext-1").is_none(),
        "the batch's record was reclaimed, so the horizon has passed it — asserted so the \
         weakening is an observable rather than something a client discovers"
    );
}
