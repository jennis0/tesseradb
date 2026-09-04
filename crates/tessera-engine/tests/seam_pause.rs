//! The write path's publication-seam pause sites, shown to actually pause — a thread arrives,
//! blocks, and proceeds on release (decision 0071; correctness-suite §10.1, §12.3).
//!
//! ## Why these seams and not others
//!
//! A crash test's whole subject is the instant between a stage's two endpoints, and an arbitrary
//! `SIGKILL` essentially never lands on one. The write path has exactly three such instants —
//! commit points where bytes exist on disc and nothing durable names them yet:
//!
//! - **before a side-manifest commits into the live prefix** (flush, merge, coalesce, and the
//!   overlay's deny-state write share this one, and one crash story with it);
//! - **before the fold's `CURRENT` flip** — the single rename that is the fold's commit point,
//!   and the simplest crash story in the system: the whole unflipped prefix is unreferenced;
//! - **between a merge's execution on the pool and its publication on the executor** — exercised
//!   in `tests/merge.rs`, beside the machinery that provokes a genuinely permuting merge, because
//!   that file's module doc is where the site's absence was recorded as the reason a
//!   crash-mid-merge test could not be written.
//!
//! Every other line of a publication is on one side of a commit point or the other, so a kill
//! there is indistinguishable from a kill at the nearest seam — a fourth site would add arming
//! surface without adding a reachable state. The sites extend the ack contract's existing
//! switchboard rather than standing beside it, and they inherit its two rules unmodified: an
//! injected failure is indistinguishable from a real one in variant and in order, and **a pause
//! site parks a thread holding no lock** — each site is the first statement of its publication or
//! sits between two calls, with every lock the executor takes already released.
//!
//! ## What "shown to pause" means here
//!
//! Arrival alone cannot distinguish "parked" from "about to complete", so each case settles
//! briefly after the arrival and asserts the publication has *not* happened — a `Stall` that
//! failed to block turns that into a reliable failure rather than a racy pass — and then
//! releases and asserts it *does*. The parked state each case pins is the discard rule
//! correctness-suite §12.3's table names for its site, read off the disc.
//!
//! The manifest seam is additionally held over real HTTP by
//! `tessera-server/tests/faults_surface.rs`, which is the same site reached the way the
//! correctness suite's driver reaches it.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::faults::{FaultSwitchboard, PauseAction, PauseSite};
use tessera_lifecycle::{ChangeOp, UnallocatedRow};
use tessera_types::EntityId;

const WAIT: Duration = Duration::from_secs(30);
/// Long enough that a `Stall` which failed to block would almost always have published by the
/// time the post-settle assertion runs, short enough to cost nothing against the binary's runtime.
const SETTLE: Duration = Duration::from_millis(50);

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A fixture engine whose executor runs against a switchboard the test holds the other end of.
/// The tick is held long so the test owns the clock, exactly as the driver will
/// (correctness-suite §12.3).
fn engine_with_faults(tmp: &Path, root: &Path) -> (Engine, Arc<FaultSwitchboard>) {
    build_fixture(
        root,
        &tmp.join("points.parquet"),
        &tmp.join("pairs.parquet"),
    );
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: 3600,
            ..config_uncapped()
        },
    )
    .expect("engine should open against a freshly built bundle");
    let faults = Arc::new(FaultSwitchboard::new());
    engine
        .start_write_executor_with_faults(8, Arc::clone(&faults))
        .expect("the executor starts once");
    (engine, faults)
}

/// The prefix `CURRENT` durably names, read from the disc rather than from the engine — the
/// commit point is the file, and the file is what a restart would open.
fn current_prefix(root: &Path) -> String {
    let bytes = std::fs::read(root.join("CURRENT")).expect("CURRENT exists");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("CURRENT is JSON");
    json["prefix"]
        .as_str()
        .expect("CURRENT names a prefix")
        .to_string()
}

/// How many side-manifests the live prefix's default partition carries — the durable name count
/// a manifest publish increments.
fn side_manifest_count(root: &Path, prefix: &str) -> usize {
    std::fs::read_dir(root.join(prefix).join("partitions/default"))
        .expect("the partition directory exists")
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("SEGMENTS-") && name.ends_with(".json")
        })
        .count()
}

/// **The fold parks before the `CURRENT` flip: folded tree complete on disc, old prefix still
/// committed.** Killed there, a restart opens the old prefix and the startup sweep reclaims the
/// unnamed tree whole — §12.3's simplest discard rule, which is why this seam is the first the
/// crash modifier builds against.
#[test]
fn the_current_flip_site_parks_the_fold_with_the_old_prefix_still_committed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let (engine, faults) = engine_with_faults(tmp.path(), &root);

    // The fold has work: one accepted deletion, executed at the fold and nowhere else
    // (write-path §5.4, Rule F).
    let deleted = EntityId::new(source_to_new_map(&root, "v00000")[&4]);
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("a delete is accepted");

    faults.arm_pause(PauseSite::BeforeCurrentFlip, PauseAction::Stall);
    engine.request_fold();

    // Arrives: the five passes ran on the fold's thread and the executor reached the flip.
    faults.await_arrivals(PauseSite::BeforeCurrentFlip, 1, WAIT);

    // Blocks: the folded tree is on disc in full, and nothing durable names it.
    assert!(
        root.join("v00001").is_dir(),
        "the folded prefix is written before the flip"
    );
    std::thread::sleep(SETTLE);
    assert_eq!(
        engine.write_executor_stats().folds,
        0,
        "a parked fold has published nothing"
    );
    assert_eq!(
        current_prefix(&root),
        "v00000",
        "CURRENT still names the old prefix while the executor is parked — a kill here is the \
         'whole unflipped prefix discarded' crash state"
    );

    // Proceeds: release, and the rename lands.
    faults.release();
    wait_until("the released fold publishes", || {
        engine.write_executor_stats().folds >= 1
    });
    assert_eq!(engine.write_executor_stats().fold_failures, 0);
    assert_eq!(
        current_prefix(&root),
        "v00001",
        "the released fold flipped CURRENT onto the folded prefix"
    );
}

/// **The fold's cost on `/control/status` covers its publication.** The staircase the fold thread
/// records ends at its fifth pass; the executor continues it through the publication's phases, so
/// `last_fold_secs` and the pass list report the fold from the thread's entry to the superseded
/// prefix's reclaim. Shown by parking the executor at the flip for over a second: the thread's
/// passes over this fixture take milliseconds, so a gauge that covered them alone would read zero.
#[test]
fn the_fold_status_covers_the_publication() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let (engine, faults) = engine_with_faults(tmp.path(), &root);
    let deleted = EntityId::new(source_to_new_map(&root, "v00000")[&4]);
    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("a delete is accepted");

    faults.arm_pause(PauseSite::BeforeCurrentFlip, PauseAction::Stall);
    engine.request_fold();
    faults.await_arrivals(PauseSite::BeforeCurrentFlip, 1, WAIT);
    let held = Duration::from_millis(1_200);
    std::thread::sleep(held);
    faults.release();
    wait_until("the released fold publishes", || {
        engine.write_executor_stats().folds >= 1
    });

    let passes = engine.last_fold_passes();
    let names: Vec<&str> = passes.iter().map(|p| p.pass).collect();
    let publication = [
        "6 hand-off",
        "7 memberships",
        "8 derived",
        "9 report",
        "10 manifest",
        "11 flip",
        "12 retire",
        "13 open",
        "14 adopt",
        "15 warm",
        "16 wal",
        "17 reclaim",
    ];
    assert_eq!(
        &names[names.len() - publication.len()..],
        &publication,
        "the publication's phases follow the thread's passes, in execution order: {names:?}"
    );
    assert_eq!(names[0], "entry");
    assert!(
        names.contains(&"5 digests + fsync"),
        "and the thread's passes are still there: {names:?}"
    );
    let flip = passes
        .iter()
        .find(|p| p.pass == "11 flip")
        .expect("the flip is a row");
    assert!(
        flip.elapsed >= held,
        "the hold at the flip site is attributed to the flip row: {:?}",
        flip.elapsed
    );
    let whole: Duration = passes.iter().map(|p| p.elapsed).sum();
    assert_eq!(
        engine.write_executor_stats().last_fold_secs,
        whole.as_secs(),
        "last_fold_secs is the whole staircase in seconds"
    );
    assert!(
        engine.write_executor_stats().last_fold_secs >= 1,
        "which is at least the publication's wall: the thread's passes alone are under a second here"
    );
    assert!(
        passes.iter().all(|p| p.rss > 0),
        "every row carries a resident-set sample"
    );
}

/// **A flush parks before its side-manifest commit: segment written, WAL still holding its rows,
/// no durable name.** Killed there, the segment files are orphans no restart opens and the rows
/// replay from the WAL — §12.3's manifest-publish discard rule.
#[test]
fn the_manifest_publish_site_parks_a_flush_with_its_segment_unreferenced() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let (engine, faults) = engine_with_faults(tmp.path(), &root);

    let descriptors = vec![b"0".to_vec()];
    let row = UnallocatedRow {
        external_id: Some(b"seam-flush-1".to_vec()),
        view: "s0".to_string(),
        join: None,
        x: 5.0,
        y: 5.0,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&descriptors),
        descriptors,
        scoped: Vec::new(),
    };
    engine
        .accept_ingest(vec![row], "seam-batch-1".to_string(), [0u8; 32])
        .expect("ingest is accepted — the seam sits at publication, not on the ack path");

    let manifests_before = side_manifest_count(&root, "v00000");
    faults.arm_pause(PauseSite::BeforeManifestPublish, PauseAction::Stall);
    engine.request_flush();

    // Arrives: the flush executed on the pool and its publication reached the commit.
    faults.await_arrivals(PauseSite::BeforeManifestPublish, 1, WAIT);

    // Blocks: nothing published, no new durable name.
    std::thread::sleep(SETTLE);
    assert_eq!(
        engine.write_executor_stats().flushes,
        0,
        "a parked flush has published nothing"
    );
    assert_eq!(
        side_manifest_count(&root, "v00000"),
        manifests_before,
        "no side-manifest names the flushed segment while the executor is parked — a kill here \
         leaves orphan files and a WAL that replays the rows"
    );

    // Proceeds.
    faults.release();
    wait_until("the released flush publishes", || {
        engine.write_executor_stats().flushes >= 1
    });
    assert_eq!(
        side_manifest_count(&root, "v00000"),
        manifests_before + 1,
        "the released flush committed exactly one side-manifest"
    );
}
