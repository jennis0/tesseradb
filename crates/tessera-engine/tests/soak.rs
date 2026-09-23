//! **The soak** (write-path §14's obligation 10): under sustained ingest, the four axes flush
//! grows are **bounded**, and nothing is lost bounding them.
//!
//! Flush appends one segment, one delta tier, one external-id run and one locator extent per tick.
//! Each is a term in a steady-state cost — a viewport pays a binary search and a
//! `range_cardinality` per live segment per tile, a fragment build probes every tier, the ingest
//! duplicate check scans every run — and at a 90 s tick a day of ingest produces ~960 of each.
//! Two maintenance passes bound them, on separate cadences and by different arguments: the
//! entity-space coalesce (`crate::coalesce`, no `segments_version` bump) and the row-space merge
//! (`crate::merge`, its own swap behind the background refresh).
//!
//! **The assertion is the bound, not the count.** What matters is that the numbers stop growing
//! with the number of flushes; the exact figures are a function of the policy widths and would
//! make this a test of the constants. The generous ceilings below are what a *stopped* pass fails:
//! with either one disabled the counts reach `ROUNDS`.

mod common;

use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{Engine, EngineConfig, ViewportRequest};
use tessera_lifecycle::UnallocatedRow;
use tessera_types::EntityId;

const WAIT: Duration = Duration::from_secs(60);

/// Flush rounds. Enough that an unbounded axis is unmistakable against the ceilings below, and
/// small enough that the suite stays a suite.
const ROUNDS: usize = 40;
const CORPUS: u64 = 64;

fn viewport(
    engine: &Engine,
    session: &tessera_engine::Session,
) -> tessera_engine::viewport::ViewportOut {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match engine.viewport(
            session,
            ViewportRequest::new("s0", 2, [0.0, 0.0, 1000.0, 1000.0], N_ITEMS as usize),
        ) {
            Ok(out) => return out,
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "timed out retrying a viewport: {e}"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

/// Wait until no publication or refresh is in flight. A request that lands between a publication
/// and its refresh is served from the generation before, or, if a merge published before the
/// flush's refresh built its entry, builds inline; either would make the counts below measure the
/// race rather than the steady state.
fn settle(engine: &Engine) {
    wait_until("maintenance and the refresh to go idle", WAIT, || {
        engine.maintenance_idle_for_test()
    });
}

/// **Sustained ingest, and every axis stops growing.**
///
/// **Mutation:** disable either maintenance pass and the corresponding count reaches `ROUNDS`.
#[test]
fn sustained_ingest_leaves_every_axis_bounded_and_every_item_visible() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        CORPUS,
    );
    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            // Prompt flushes only, so the round count is the flush count and nothing depends on
            // wall-clock.
            flush_max_age_secs: 3600,
            // **The row trigger off.** This cell drives publication itself — it pins `B`
            // by flushing and waiting, so a trigger that published on its own would
            // measure a different buffer depth than the one the sweep set.
            flush_max_items: usize::MAX,
            max_merged_segment_bytes: None,
            // Compaction §9's trigger is off unless a deployment configures one.
            compaction: tessera_engine::CompactionSchedule::off(),
            ..config_uncapped()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    viewport(&engine, &session);

    let mut ingested: Vec<(EntityId, String)> = Vec::new();
    for round in 0..ROUNDS {
        let external_id = format!("soak-{round}");
        // **Two descriptors: the grant's, and a novel one.** The novel one makes the flush promote,
        // so the dictionary-extent axis grows rather than being vacuously bounded. It cannot be the
        // only one: `satisfied` is fixed at authorise and never re-resolved (§3.3), so an item
        // carrying only a term promoted afterwards is invisible to this session by design — and the
        // visibility assertion below would then be measuring that rule instead of the merge's.
        let descriptors = vec![b"0".to_vec(), format!("soak-term-{round}").into_bytes()];
        let row = UnallocatedRow {
            external_id: Some(external_id.as_bytes().to_vec()),
            view: "s0".to_string(),
            join: None,
            descriptors: descriptors.clone(),
            x: 3.0 * (round as f64 + 1.0),
            y: 7.0,
            scalars: Vec::new(),
            terms: engine.resolve_terms(&descriptors),
            scoped: Vec::new(),
        };
        let entity = engine
            .accept_ingest(vec![row], external_id.clone(), [round as u8; 32])
            .expect("ingest is accepted")[0];
        ingested.push((entity, external_id));

        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_until("the flush to publish", WAIT, || {
            engine.write_executor_stats().flushes > flushes
        });
        // A request every round, so the cache stays resident and the refresh has work — the
        // steady state this soak is about, rather than an idle node.
        settle(&engine);
        viewport(&engine, &session);
    }

    // **Let the maintenance passes drain.** Each is selected on a tick and at most one of each
    // runs at a time, so reaching the steady state takes several ticks after the last flush; the
    // loop stops when a round changes nothing rather than after a fixed count, so it does not
    // encode how many rounds the policy widths happen to need.
    let mut settled = 0;
    for _ in 0..200 {
        let before = engine.write_executor_stats();
        engine.request_flush();
        std::thread::sleep(Duration::from_millis(20));
        let after = engine.write_executor_stats();
        if after.merges == before.merges && after.coalesces == before.coalesces {
            settled += 1;
            if settled >= 3 {
                break;
            }
        } else {
            settled = 0;
        }
    }

    settle(&engine);
    let generation = engine.generation();
    let partition = &generation.bundle.partitions["default"];
    let manifest = &partition.manifest;
    let segments = partition.views["s0"].segments.len();
    let stats = engine.write_executor_stats();
    eprintln!(
        "soak: {ROUNDS} flushes → segments {segments}, deltas {}, runs {}, dict extents {}, \
         locators {} (merges {}, coalesces {}, refreshes {}, stale serves {}, full builds {})",
        manifest.deltas.len(),
        manifest.external_id_runs.len(),
        manifest.dict_extents.len(),
        manifest.locator_extents.len(),
        stats.merges,
        stats.coalesces,
        engine.refreshes(),
        engine.stale_serves(),
        engine.full_projection_builds(),
    );

    assert!(
        segments <= ROUNDS / 2,
        "the segment axis is unbounded: {segments} segments after {ROUNDS} flushes"
    );
    assert!(
        manifest.deltas.len() <= ROUNDS * 3 / 4,
        "the delta-tier axis is unbounded: {} tiers",
        manifest.deltas.len()
    );
    assert!(
        manifest.external_id_runs.len() <= ROUNDS / 2,
        "the external-id run axis is unbounded: {} runs",
        manifest.external_id_runs.len()
    );
    assert!(
        manifest.dict_extents.len() <= ROUNDS * 3 / 4,
        "the dictionary-extent axis is unbounded: {} extents",
        manifest.dict_extents.len()
    );
    assert!(
        stats.merge_failures == 0 && stats.coalesce_failures == 0 && stats.flush_failures == 0,
        "maintenance must not be failing its way to a low count: {stats:?}"
    );

    // One full projection build, the session's own establishment, across {ROUNDS} flush
    // publications and every merge among them: each later round's entry was built by the
    // background refresh, and the request only read it.
    assert_eq!(
        engine.full_projection_builds(),
        1,
        "a session that asked in every round must have rebuilt exactly once, at establishment"
    );
    assert!(
        engine.refreshes() >= ROUNDS as u64,
        "and it must have been served from the refresh's entries rather than building them"
    );

    // **Nothing was lost bounding them**, which is the half a count assertion cannot see.
    settle(&engine);
    let out = viewport(&engine, &session);
    assert_eq!(
        out.tiles.iter().map(|t| t.visible).sum::<u64>(),
        CORPUS + ROUNDS as u64,
        "every ingested item is still on the map"
    );
    for (entity, external_id) in &ingested {
        assert_eq!(
            engine
                .resolve_external_id(external_id.as_bytes())
                .expect("resolvable"),
            Some(*entity),
            "{external_id} lost its binding"
        );
        assert!(
            partition.views["s0"].row_space.row_of(*entity).is_some(),
            "entity {} lost its row",
            entity.raw()
        );
    }
}

/// **The control**: with both maintenance passes off, every axis grows one per flush.
///
/// Without this the bounds above could be satisfied by a deployment that never grew them in the
/// first place — a policy that is simply never triggered, which is the silent failure
/// `MergePolicy::segment_floor_bytes` exists to prevent and which no count assertion can
/// distinguish from a working one.
#[test]
fn without_maintenance_every_axis_grows_one_per_flush() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_n(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        CORPUS,
    );
    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: 3600,
            flush_max_items: usize::MAX,
            max_merged_segment_bytes: None,
            // Compaction §9's trigger is off unless a deployment configures one.
            compaction: tessera_engine::CompactionSchedule::off(),
            ..config_uncapped()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    engine.set_merge_for_test(false);
    engine.set_coalesce_for_test(false);

    for round in 0..ROUNDS {
        let external_id = format!("soak-{round}");
        let descriptors = vec![b"0".to_vec(), format!("soak-term-{round}").into_bytes()];
        let row = UnallocatedRow {
            external_id: Some(external_id.as_bytes().to_vec()),
            view: "s0".to_string(),
            join: None,
            descriptors: descriptors.clone(),
            x: 3.0 * (round as f64 + 1.0),
            y: 7.0,
            scalars: Vec::new(),
            terms: engine.resolve_terms(&descriptors),
            scoped: Vec::new(),
        };
        engine
            .accept_ingest(vec![row], external_id, [round as u8; 32])
            .expect("ingest is accepted");
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        wait_until("the flush to publish", WAIT, || {
            engine.write_executor_stats().flushes > flushes
        });
    }

    let generation = engine.generation();
    let partition = &generation.bundle.partitions["default"];
    assert_eq!(
        partition.views["s0"].segments.len(),
        ROUNDS + 1,
        "the build segment plus one per flush — this is what the merge bounds"
    );
    assert_eq!(
        partition.manifest.deltas.len(),
        ROUNDS,
        "one tier per flush — this is what the coalesce bounds"
    );
    assert_eq!(
        partition.manifest.external_id_runs.len(),
        ROUNDS + 1,
        "the build's run plus one per flush"
    );
    assert_eq!(
        partition.manifest.dict_extents.len(),
        ROUNDS + 1,
        "the build's dictionary extent plus one per promoting flush"
    );
}
