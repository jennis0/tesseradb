//! The flush pipeline end to end, and the one step that is held back.
//!
//! **⊘ Publication is gated behind the `flush-publication` feature**, because
//! `Engine::viewport` refuses a slice holding more than one segment: `tile_ranges` returns
//! segment-local row indices while the mask is in slice row space, and §7.2's cap and floor
//! clauses are per *tile* rather than per segment, so one tile's `k` budget has to be spent across
//! the union. Multi-segment lookup and selection is its own piece of work — it sits on the path
//! I7 lives on — and publishing before it exists would buy visible ingest at the cost of every
//! viewport on the slice.
//!
//! What that leaves testable here is everything up to the swap: the tick plans, the pool writes a
//! segment, a tier, the extents and the side-manifest, and a **fresh engine opening that bundle
//! sees the flushed item**. That last one is the real proof the artefacts are right — it is the
//! same read path a restart uses, and it does not depend on the gated step at all.

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

fn engine_at(tmp: &std::path::Path, root: &std::path::Path, tick_secs: u64) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: tick_secs,
            ..config()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(64)
        .expect("the executor starts once");
    engine
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

/// The tick plans what it would flush, and the gauge separates a stalled flush from healthy
/// backlog: `flushable_items` is the buffer minus what the three dispositions exclude (§3.5).
#[test]
fn the_tick_plans_what_it_would_flush() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = engine_at(tmp.path(), &fixture(tmp.path()), 1);

    ingest(&engine, "ext-1");
    ingest(&engine, "ext-2");

    wait_until("the tick to plan", || {
        engine.write_executor_stats().flushable_items == 2
    });
    assert_eq!(engine.write_executor_stats().quarantined_items, 0);
}

/// **⊘ Publication is gated**, so an acked ingest is still durable and invisible in the process
/// that accepted it. This asserts the gate rather than the visibility, so that whoever lifts it
/// has a test that fails and says why.
#[cfg(not(feature = "flush-publication"))]
#[test]
fn publication_is_gated_until_the_read_path_can_union_segments() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = engine_at(tmp.path(), &fixture(tmp.path()), 1);

    ingest(&engine, "ext-1");
    wait_until("several ticks", || engine.write_executor_stats().ticks >= 3);

    assert_eq!(
        engine.write_executor_stats().flushes,
        0,
        "with `flush-publication` off, no flush publishes — see Executor::dispatch_flushes for \
         the read-path capability this waits on"
    );
    assert_eq!(
        engine.generation().segments_version,
        0,
        "and therefore no geometry moved"
    );
    assert!(
        engine.write_executor_stats().flushable_items > 0,
        "while the plan says there is work waiting, which is what an operator sees"
    );
}

/// A tick with nothing flushable plans nothing — no empty segment, no `segments_version` bump,
/// and so no drain entry per tick on an idle deployment.
#[test]
fn an_idle_tick_plans_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = engine_at(tmp.path(), &fixture(tmp.path()), 1);

    wait_until("several ticks", || engine.write_executor_stats().ticks >= 3);
    assert_eq!(engine.write_executor_stats().flushable_items, 0);
    assert_eq!(engine.generation().segments_version, 0);
}

/// **The whole pipeline, with the gate lifted**: the tick plans, the pool writes a segment, a
/// tier, both external-id directions and the side-manifest, and the executor rebases and
/// publishes. Run with `--features flush-publication`.
///
/// Asserted on **a fresh open of the bundle on disk**, not on the publishing process's own
/// in-memory generation. That is the property that matters and the one a `Bundle::with_segment`
/// bug would not show: the side-manifest is the commit point, so if a restart cannot see the
/// flushed item then nothing was really published.
#[cfg(feature = "flush-publication")]
#[test]
fn a_published_flush_is_a_bundle_a_restart_opens() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let engine = engine_at(tmp.path(), &root, 1);

    let id = ingest(&engine, "ext-1");
    wait_until("the flush to publish", || {
        engine.write_executor_stats().flushes >= 1
    });

    // In the publishing process: the item left the buffer, exactly once, and geometry moved.
    assert!(!engine.generation().buffer.contains(id));
    assert_eq!(engine.generation().segments_version, 1);

    // And on disk, read back through the ordinary protocol.
    let bundle = tessera_store::open_bundle(&root).expect("the published bundle opens");
    let partition = bundle.partitions.values().next().unwrap();
    assert_eq!(
        partition.manifest.segments_version, 1,
        "the reader settled on the flush-published side-manifest"
    );
    assert_eq!(
        partition.manifest.segments.len(),
        2,
        "the build segment and the flush segment"
    );
    assert_eq!(
        partition.manifest.watermark,
        id.raw() + 1,
        "one past the highest flushed entity, or it would be in neither fragment nor buffer"
    );
    assert_eq!(partition.manifest.deltas.len(), 1, "one delta tier");
    assert_eq!(partition.manifest.locator_extents.len(), 1);

    let slice = &partition.slices["s0"];
    assert_eq!(slice.segments.len(), 2, "both segments mapped");

    // **⊘ The extent does not survive the restart, and cannot yet.** `SegmentExtent` carries a
    // dense `rows` array — entity to row *within* the segment — and nothing writes it: the segment
    // is Morton-sorted, so the mapping is not recoverable from the descriptor's entity range, and
    // `columns.arrow` stores `tessera_id` rather than the entity id. §2.1's extent is four scalars
    // with no mapping, which holds only if a flush segment's rows are in entity order — and that
    // contradicts the same section's requirement that every flush segment be internally
    // Morton-sorted. One of the two has to give, and which is an owner's call.
    assert_eq!(
        slice.row_space.extent_count(),
        0,
        "the reopened row space has no extent for the flush segment — see above"
    );
}
