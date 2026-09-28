//! Counting a viewer's rows by map cell, over a view of two segments: the build's and a flush's.
//!
//! Range counts and the pass are two routes to one table, so at every depth where both apply they
//! must agree cell for cell, over a composed mask and over a bitmap of rows; below depth 16, where
//! only the pass applies, each depth-16 cell's finer cells must add up to it.

mod common;

use std::time::{Duration, Instant};

use common::*;
use croaring::Bitmap;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tessera_engine::cells::{count_by_ranges, pass, pass_in_chunks, CellCount, CellSet, RowGroups};
use tessera_engine::compose::MaskedSet;
use tessera_engine::viewport::segments_with_row_bases;
use tessera_engine::Engine;
use tessera_lifecycle::UnallocatedRow;

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Ingest `n` items at scattered positions, every other one visible to the subset principal, and
/// flush them into a segment of their own.
fn flush_items(engine: &Engine, n: usize) {
    let mut rng = StdRng::seed_from_u64(7);
    let rows = (0..n)
        .map(|i| {
            let descriptors = if i % 2 == 0 {
                vec![b"0".to_vec(), b"1".to_vec()]
            } else {
                vec![b"0".to_vec()]
            };
            UnallocatedRow {
                view: "s0".to_string(),
                join: None,
                x: rng.gen_range(0.0..1000.0),
                y: rng.gen_range(0.0..1000.0),
                scalars: keyed(&format!("flushed-{i}")),
                terms: engine.resolve_terms(&descriptors),
                descriptors,
                scoped: Vec::new(),
            }
        })
        .collect();
    engine
        .ingest_rows(rows, "batch-1".to_string(), [1u8; 32])
        .expect("the ingest is accepted");
    let flushes = engine.write_executor_stats().flushes;
    engine.request_flush();
    wait_until("the flush to publish", || {
        engine.write_executor_stats().flushes > flushes
    });
}

/// Every cell a pass at `depth` found, with group 0's count, as range counts report them.
fn by_cell(entries: &[CellCount]) -> Vec<(u64, u64)> {
    entries.iter().map(|e| (e.cell, e.count)).collect()
}

#[test]
fn range_counts_and_the_pass_agree_and_finer_cells_add_up() {
    let fx = fixture();
    let engine = engine_at(fx._tmp.path(), &fx.root, 3600);
    flush_items(&engine, 3_000);

    let session = engine.authorise(&subset_credential()).unwrap();
    let (generation, mask) = engine
        .composed_mask(&session, "s0")
        .expect("the mask composes");
    let view_data = &generation.bundle.partitions["default"].views["s0"];
    let segments = segments_with_row_bases("s0", view_data).expect("the segments resolve");
    assert_eq!(segments.len(), 2, "the flush wrote a segment of its own");
    let flushed = segments[1].1..segments[1].1 + segments[1].0.row_count;
    assert!(
        mask.count_range(flushed) > 0,
        "the mask holds rows of the flushed segment, so both segments are counted"
    );

    let mut rng = StdRng::seed_from_u64(11);
    let total = view_data.row_space.total_rows() as u32;
    let sample: Bitmap = (0..total).filter(|_| rng.gen_bool(0.3)).collect();
    let rows = mask.visible_rows(&sample);
    let whole_view = || {
        segments
            .iter()
            .map(|&(segment, row_base)| (segment, row_base, 0..segment.row_count))
    };

    for (what, set) in [
        ("the mask", CellSet::Mask(&mask)),
        ("a bitmap", CellSet::Rows(&rows)),
    ] {
        let depth16 = pass(set, &segments, 16, &RowGroups::None);
        let total_count: u64 = depth16.iter().map(|e| e.count).sum();
        assert_eq!(
            total_count,
            set.count(0..total),
            "{what}: the pass counts every row"
        );
        for depth in 0..=16u8 {
            let ranged = count_by_ranges(whole_view(), depth, 0..1u64 << (2 * depth), &|r| {
                set.count(r)
            });
            let passed = pass(set, &segments, depth, &RowGroups::None);
            assert_eq!(by_cell(&passed), ranged.cells, "{what}, depth {depth}");
            // Cut into many chunks, each holding both segments' rows of its cells.
            let chunked = pass_in_chunks(set, &segments, depth, &RowGroups::None, 500);
            assert_eq!(
                chunked, passed,
                "{what}, depth {depth}, in chunks of 500 rows"
            );
        }
        for depth in 17..=32u8 {
            let mut folded: Vec<(u64, u64)> = Vec::new();
            for entry in pass_in_chunks(set, &segments, depth, &RowGroups::None, 500) {
                let cell = entry.cell >> (2 * (u32::from(depth) - 16));
                match folded.last_mut() {
                    Some((last, n)) if *last == cell => *n += entry.count,
                    _ => folded.push((cell, entry.count)),
                }
            }
            assert_eq!(
                folded,
                by_cell(&depth16),
                "{what}, depth {depth} folded to 16"
            );
        }
    }
}
