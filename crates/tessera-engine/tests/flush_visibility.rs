//! The flush pipeline end to end: an acknowledged ingest becomes a mark on the map.
//!
//! This is the property the whole flush exists for, and it was the last thing to arrive. Until
//! `Engine::viewport` could union tile ranges across segments — counting each segment's ranges in
//! slice row space and spending one tile's `k` budget over the union, §7.2's cap and floor being
//! per *tile* rather than per segment — publishing a second segment into a slice would have made
//! every viewport on it fail. That is now built (`select::SelectionParts`), and publication is
//! unconditional.
//!
//! **Every assertion about the published result is made on a fresh open of the bundle**, never on
//! the publishing process's own in-memory generation. The side-manifest is the commit point, so a
//! flush that a restart cannot see was not really published — and the restart path is also the
//! only one that exercises the two reconstructions a flush leaves no artefact for: the row-space
//! extent (`SegmentExtent::rebuild`) and the live delta postings tiers (`Engine::open`).

mod common;

use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{Engine, EngineConfig, ViewportRequest};
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

/// **The whole pipeline**: the tick plans, the pool writes a segment, a tier, both external-id
/// directions and the side-manifest, and the executor rebases and publishes.
///
/// Asserted on **a fresh open of the bundle on disk**, not on the publishing process's own
/// in-memory generation. That is the property that matters and the one a `Bundle::with_segment`
/// bug would not show: the side-manifest is the commit point, so if a restart cannot see the
/// flushed item then nothing was really published.
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
        partition.segments_n, 1,
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

    // **The extent survives the restart, and nothing on disk carries it.** It is rebuilt from the
    // flush segment's own `tessera_id` column by inverting the identity permutation
    // (`SegmentExtent::rebuild`) — the segment is Morton-sorted, so §2.1's four scalars are not a
    // mapping, and this is what stands in for the file they would otherwise need.
    assert_eq!(
        slice.row_space.extent_count(),
        1,
        "the reopened row space carries an extent for the flush segment"
    );
    let extent = &slice.row_space.extents()[0];
    assert_eq!(extent.entity_lo, id.raw());
    assert_eq!(extent.entity_hi, id.raw());
    assert_eq!(
        slice.row_space.row_of(id).map(|r| r.raw()),
        Some(extent.row_base),
        "and the flushed entity resolves to the first row of its segment"
    );
}

/// **A reopened engine does not re-buffer what it already flushed** — and that is what makes
/// `compose::verdict`'s missing watermark gate safe.
///
/// Replay walks every retained WAL record, the `IngestBatch` rows of already-published flushes
/// included, so without a filter the buffer comes back holding entities that already have segments.
/// Two things then go wrong: the next flush writes each of them a second time, and `verdict` gets a
/// buffer hit for an entity the frozen fragment already accounts for. The second was what the
/// `entity < watermark` gate existed to stop.
///
/// **The filter is `row_of`, not a watermark.** A watermark is exact only while entity-allocation
/// order and flush order coincide — one slice per partition, which write-path §4.3 records as
/// load-bearing and unenforced. `row_of` is the predicate the watermark approximates, so it holds
/// at any number of slices.
///
/// The same WAL is reused deliberately: a separate one would exercise nothing, since the point is
/// precisely that the flushed rows' records are still there.
#[test]
fn a_reopened_engine_does_not_re_buffer_rows_that_already_have_geometry() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());

    let id = {
        let engine = engine_at(tmp.path(), &root, 1);
        let id = ingest(&engine, "ext-1");
        wait_until("the flush to publish", || {
            engine.write_executor_stats().flushes >= 1
        });
        assert!(!engine.generation().buffer.contains(id));
        id
    };

    // The record is still in the WAL — nothing has rotated — so replay will meet it again.
    let reopened = engine_at(tmp.path(), &root, 3600);
    assert!(
        !reopened.generation().buffer.contains(id),
        "the row has geometry, so it must not come back into the buffer: the next flush would \
         write it a second time, and `verdict` would answer from the buffer for an entity the \
         fragment already covers"
    );
    assert!(
        reopened.generation().buffer.is_empty(),
        "and nothing else came back either"
    );

    // The geometry is still there, which is what makes the absence above a filter rather than a
    // loss.
    let bundle = tessera_store::open_bundle(&root).expect("the published bundle opens");
    let partition = bundle.partitions.values().next().unwrap();
    assert!(partition.slices["s0"].row_space.row_of(id).is_some());
}

/// **The allocator floor survives on the side-manifest alone** (I9).
///
/// `MANIFEST.json`'s `entity_id_high_water` is frozen at build; every flush raises the
/// *side*-manifest's past the ids it consumed. Seeding from the build value works today only
/// because the WAL still carries the `Lease` and `IngestBatch` records `high_water_from` derives
/// the rest from — and rotation deletes exactly those. An id reissued after that grants the new
/// item every access the old one had.
///
/// Asserted by reopening against a WAL that carries nothing: the side-manifest is then the only
/// surviving statement of how far allocation has gone.
#[test]
fn the_allocator_floor_comes_from_the_side_manifest() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());

    let flushed = {
        let engine = engine_at(tmp.path(), &root, 1);
        let id = ingest(&engine, "ext-1");
        wait_until("the flush to publish", || {
            engine.write_executor_stats().flushes >= 1
        });
        id
    };

    let bundle = tessera_store::open_bundle(&root).expect("the published bundle opens");
    let partition = bundle.partitions.values().next().unwrap();
    assert!(
        partition.manifest.entity_id_high_water > flushed.raw(),
        "the flush must raise the side-manifest's floor past the ids it consumed"
    );
    assert!(
        bundle.manifest.entity_id_high_water <= flushed.raw(),
        "and the build manifest's must be the stale one, or this test proves nothing"
    );

    // A *fresh* WAL: nothing survives to re-derive the floor from, so only the side-manifest can
    // supply it. This is the state rotation produces.
    let elsewhere = tempfile::TempDir::new().unwrap();
    let reopened = engine_at(elsewhere.path(), &root, 3600);
    let next = ingest(&reopened, "ext-after-rotation");
    assert!(
        next.raw() > flushed.raw(),
        "an id was reissued over a flushed entity: {} is not past {}",
        next.raw(),
        flushed.raw()
    );
}

/// **An acknowledged ingest becomes a mark on the map.** The property the flush exists for, and
/// the one the read path could not serve until a tile could union its segments.
///
/// **The viewport is tight around the ingested point on purpose.** At a whole-extent zoom the
/// tile holds 10,001 visible rows against a cap of 200, so §7.2's threshold clause serves the 200
/// smallest `tessera_id`s and one particular item is drawn only by luck — a test that asserted it
/// there would be asserting the identity permutation's arithmetic, not the union. Zoomed in, the
/// tile's visible count is under the cap, `serves_all_visible` fires, and "is it drawn" is a
/// question about the union and nothing else.
///
/// Asserted on a fresh engine opened on the same directory — the restart case. It reads the
/// extent back through `SegmentExtent::rebuild` and the delta tier back through
/// `Engine::open`'s reopen, so a rebuild that mapped the entity to the wrong row would draw the
/// point at the wrong coordinates rather than not at all.
#[test]
fn a_flushed_item_is_visible_in_a_viewport() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let engine = engine_at(tmp.path(), &root, 1);

    // The ingested item sits at (5, 5) — see `ingest`.
    let request = || ViewportRequest::new("s0", 10, [4.0, 4.0, 6.0, 6.0], 200);

    let session = engine
        .authorise(&full_coverage_credential())
        .expect("the credential authorises");
    let before = engine
        .viewport(&session, request())
        .expect("a viewport before the flush");
    let visible_before = before.tiles.iter().map(|t| t.visible).sum::<u64>();

    let id = ingest(&engine, "ext-1");
    wait_until("the flush to publish", || {
        engine.write_executor_stats().flushes >= 1
    });

    // A fresh engine on the same bundle: the restart path, and the only one that proves the
    // artefacts rather than the publishing process's own in-memory generation.
    let reopened = engine_at(tmp.path(), &root, 3600);
    let reopened_session = reopened
        .authorise(&full_coverage_credential())
        .expect("the credential authorises against the reopened bundle");
    let out = reopened
        .viewport(&reopened_session, request())
        .expect("a viewport after the flush");

    assert_eq!(
        out.tiles.iter().map(|t| t.visible).sum::<u64>(),
        visible_before + 1,
        "the flushed item is counted exactly once"
    );
    // §7.1's count is over the union of segments; the *point* appears only if selection spent the
    // tile's budget across the union too, and if the gather resolved a slice-space row back to
    // the segment that owns it.
    let tessera_id = reopened
        .tessera_id_of(id)
        .expect("the identity is computable");
    let point = out
        .points
        .iter()
        .find(|(id, _)| *id == tessera_id)
        .expect("the flushed item is drawn, not merely counted");
    // Round-trips through the flush's own quantisation: the Morton code deinterleaves back to the
    // cell the coordinates were quantised into, so a point gathered from the wrong segment's row
    // would land somewhere else in the tile.
    // `code` is the 64-bit interleave: the depth-16 cell in the high half, the residual in the
    // low. The depth-`z` tile is the cell's top `2z` bits — `code >> (64 - 2z)`.
    let containing_tile = point.1 >> (64 - 2 * 10);
    assert!(
        out.tiles.iter().any(|t| t.tile == containing_tile),
        "the drawn point lies in one of the tiles this response reported"
    );
}
