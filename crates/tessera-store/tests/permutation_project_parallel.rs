//! `Permutation::project`'s parallelisation — correctness (not gated) and the measured
//! throughput gate (`--ignored`).
//!
//! [`serial_project`] is an independent reference implementation of the serial algorithm
//! (entity-by-entity through `row_of`, then sort) — deliberately *not* a second call to
//! `Permutation::project` itself, because that method's chunking is the thing under test and an
//! oracle sharing its code would not catch a chunking bug (e.g. a chunk-boundary entity dropped
//! or double-counted).

use std::path::Path;
use std::time::Instant;

use croaring::Bitmap;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

use tessera_store::write::write_permutation;
use tessera_store::Permutation;
use tessera_types::EntityId;

/// Independent oracle: walk every set bit of `mask` through `perm.row_of`, one entity at a time,
/// skipping absent/out-of-bound entities exactly as the pre-parallel `project` did, then sort.
fn serial_project(perm: &Permutation, mask: &Bitmap) -> Bitmap {
    let mut rows: Vec<u32> = Vec::new();
    for e in mask.iter() {
        if let Some(row) = perm.row_of(EntityId::new(e as u64)) {
            rows.push(row.raw());
        }
    }
    rows.sort_unstable();
    Bitmap::of(&rows)
}

/// Build a `permutation.bin` with `row_order_entities[i]` occupying row `i`, `bound` slots wide
/// (entities not named in `row_order_entities` and `< bound` get the row-absent sentinel), and
/// load it back through the real mmap path.
fn build_permutation(dir: &Path, row_order_entities: &[EntityId], bound: u64) -> Permutation {
    let path = dir.join("permutation.bin");
    write_permutation(&path, row_order_entities, bound).expect("write_permutation");
    Permutation::load(&path).expect("load permutation")
}

/// Assert the two bitmaps are identical — same set bits, same iteration order (both are sorted
/// roaring bitmaps, so this also pins byte-level content equality, not just set equality).
fn assert_bitmaps_equal(parallel: &Bitmap, serial: &Bitmap, case: &str) {
    assert_eq!(
        parallel.iter().collect::<Vec<u32>>(),
        serial.iter().collect::<Vec<u32>>(),
        "{case}: parallel project() must equal the serial reference exactly"
    );
}

/// Run `perm.project(mask)` inside a rayon pool with exactly `threads` workers — exercises the
/// ambient-rayon contract (tessera-store owns no pool of its own; this installs one exactly as
/// `tessera-engine` does at the real call site) and, at `threads > 1`, forces `par_chunks` to
/// actually split the entity list across workers rather than degenerating to one chunk.
fn project_with_threads(perm: &Permutation, mask: &Bitmap, threads: usize) -> Bitmap {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("pool should build");
    pool.install(|| perm.project(mask))
}

#[test]
fn empty_mask_projects_to_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    let n = 1_000u64;
    let entities: Vec<EntityId> = (0..n).map(EntityId::new).collect();
    let perm = build_permutation(dir.path(), &entities, n);

    let mask = Bitmap::new();
    let expected = serial_project(&perm, &mask);
    for threads in [1, 4] {
        let got = project_with_threads(&perm, &mask, threads);
        assert_bitmaps_equal(&got, &expected, &format!("empty mask, {threads} threads"));
        assert_eq!(got.cardinality(), 0);
    }
}

#[test]
fn all_entities_mask_projects_every_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let n = 5_000u64;
    // A genuine permutation: every entity has a row, no sentinels.
    let mut rng = StdRng::seed_from_u64(1);
    let mut row_order: Vec<u32> = (0..n as u32).collect();
    row_order.shuffle(&mut rng);
    let entities_in_row_order: Vec<EntityId> =
        row_order.iter().map(|&e| EntityId::new(e as u64)).collect();
    let perm = build_permutation(dir.path(), &entities_in_row_order, n);

    let mut mask = Bitmap::new();
    mask.add_range(0..n as u32);

    let expected = serial_project(&perm, &mask);
    assert_eq!(expected.cardinality(), n, "sanity: every entity has a row");
    for threads in [1, 2, 4, 8] {
        let got = project_with_threads(&perm, &mask, threads);
        assert_bitmaps_equal(
            &got,
            &expected,
            &format!("all-entities mask, {threads} threads"),
        );
    }
}

#[test]
fn sentinel_and_out_of_bound_entities_are_skipped_not_erred() {
    let dir = tempfile::tempdir().expect("tempdir");
    let n = 2_000u64;
    // Only every third entity gets a row; the rest keep the row-absent sentinel.
    let assigned: Vec<EntityId> = (0..n as u32)
        .step_by(3)
        .map(|e| EntityId::new(e as u64))
        .collect();
    let perm = build_permutation(dir.path(), &assigned, n);

    let mut mask = Bitmap::new();
    mask.add_range(0..n as u32); // includes unassigned (sentinel) entities
    mask.add(n as u32 + 500); // out of bound entirely
    mask.add(u32::MAX); // extreme out-of-bound

    let expected = serial_project(&perm, &mask);
    assert!(
        expected.cardinality() > 0 && expected.cardinality() < mask.cardinality(),
        "sanity: some entities have rows, some (sentinel/out-of-bound) do not"
    );
    for threads in [1, 4, 8] {
        let got = project_with_threads(&perm, &mask, threads);
        assert_bitmaps_equal(
            &got,
            &expected,
            &format!("sentinel/out-of-bound mix, {threads} threads"),
        );
    }
}

#[test]
fn mask_touching_first_and_last_slot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let n = 3_000u64;
    let entities: Vec<EntityId> = (0..n).map(EntityId::new).collect();
    let perm = build_permutation(dir.path(), &entities, n);

    let mut mask = Bitmap::new();
    mask.add(0);
    mask.add(n as u32 - 1);

    let expected = serial_project(&perm, &mask);
    assert_eq!(expected.cardinality(), 2);
    for threads in [1, 4] {
        let got = project_with_threads(&perm, &mask, threads);
        assert_bitmaps_equal(
            &got,
            &expected,
            &format!("first/last slot, {threads} threads"),
        );
    }
}

#[test]
fn random_masks_match_the_serial_reference() {
    let dir = tempfile::tempdir().expect("tempdir");
    let n = 20_000u64;
    let mut rng = StdRng::seed_from_u64(99);
    let mut row_order: Vec<u32> = (0..n as u32).collect();
    row_order.shuffle(&mut rng);
    // Leave a residual gap so some entities keep the sentinel too (row_order only covers 90%).
    let assigned_count = (n as usize * 9) / 10;
    let entities_in_row_order: Vec<EntityId> = row_order[..assigned_count]
        .iter()
        .map(|&e| EntityId::new(e as u64))
        .collect();
    let perm = build_permutation(dir.path(), &entities_in_row_order, n);

    for seed in 0..10u64 {
        let mut mask_rng = StdRng::seed_from_u64(1000 + seed);
        let mut mask = Bitmap::new();
        for e in 0..n as u32 {
            if mask_rng.gen_bool(0.3) {
                mask.add(e);
            }
        }
        let expected = serial_project(&perm, &mask);
        for threads in [1, 8] {
            let got = project_with_threads(&perm, &mask, threads);
            assert_bitmaps_equal(
                &got,
                &expected,
                &format!("random mask seed {seed}, {threads} threads"),
            );
        }
    }
}

#[test]
fn project_on_the_global_pool_without_an_explicit_install_still_matches() {
    // The store crate is executor-agnostic: calling `project` with no surrounding `pool.install`
    // must still work (falling back to rayon's global pool) and must still match the serial
    // reference.
    let dir = tempfile::tempdir().expect("tempdir");
    let n = 1_500u64;
    let entities: Vec<EntityId> = (0..n).map(EntityId::new).collect();
    let perm = build_permutation(dir.path(), &entities, n);

    let mut mask = Bitmap::new();
    mask.add_range(0..n as u32);

    let expected = serial_project(&perm, &mask);
    let got = perm.project(&mask); // no pool.install — ambient global pool
    assert_bitmaps_equal(&got, &expected, "no explicit install");
}

/// MEASURED GATE (task-7 brief): keep the parallel implementation only if a representative
/// cold-session projection build drops >= 2x at 8 threads vs 1 thread.
///
/// Sized to stay well under the box's memory ceiling (WSL2, previously OOM-killed by a
/// \>4G-allocating test): `n` entities means a full mask puts two `n * 4`-byte `Vec<u32>`s live at
/// their peak overlap inside `project` (`entities` overlapping the just-finished `per_chunk`,
/// then `per_chunk` overlapping `rows`'s reserved capacity -- see `Permutation::project`'s doc,
/// "fix round 1" note, for why it is two and not three) -- at `n = 16_000_000` that is ~128 MB
/// transient, plus the ~64 MB mmap-backed `permutation.bin` itself, comfortably under the budget
/// this box has previously blown through.
///
/// Run explicitly: `cargo test -p tessera-store --release -- --ignored --nocapture
/// project_parallel_speedup_at_8_threads`
#[test]
#[ignore]
fn project_parallel_speedup_at_8_threads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let n: u64 = 16_000_000;

    let mut rng = StdRng::seed_from_u64(7);
    let mut row_order: Vec<u32> = (0..n as u32).collect();
    row_order.shuffle(&mut rng); // a genuine permutation: every entity has a row, no sentinels
    let entities_in_row_order: Vec<EntityId> = row_order
        .into_iter()
        .map(|e| EntityId::new(e as u64))
        .collect();
    let perm = build_permutation(dir.path(), &entities_in_row_order, n);

    let mut mask = Bitmap::new();
    mask.add_range(0..n as u32);
    mask.run_optimize();

    let pool1 = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .expect("1-thread pool should build");
    let pool8 = rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .build()
        .expect("8-thread pool should build");

    // Warm the mmap's page cache before either timed run, so the comparison is CPU parallelism,
    // not "first touch pays page faults, second run doesn't".
    let warm = pool1.install(|| perm.project(&mask));
    assert_eq!(warm.cardinality(), n);

    // Take the best of three per thread count to smooth out scheduler noise on a shared box.
    let mut best1 = None;
    let mut best8 = None;
    for _ in 0..3 {
        let t1 = Instant::now();
        let out1 = pool1.install(|| perm.project(&mask));
        let d1 = t1.elapsed();
        assert_eq!(out1.cardinality(), n);
        best1 = Some(best1.map_or(d1, |b: std::time::Duration| b.min(d1)));

        let t8 = Instant::now();
        let out8 = pool8.install(|| perm.project(&mask));
        let d8 = t8.elapsed();
        assert_eq!(out8.cardinality(), n);
        best8 = Some(best8.map_or(d8, |b: std::time::Duration| b.min(d8)));
    }
    let best1 = best1.expect("at least one iteration ran");
    let best8 = best8.expect("at least one iteration ran");

    let speedup = best1.as_secs_f64() / best8.as_secs_f64();
    println!(
        "row_projection_ns gate: n={n} 1-thread={best1:?} 8-thread={best8:?} speedup={speedup:.2}x"
    );
    // Deliberately not a hard assert. The action on a disappointing speedup is "revert the
    // parallel path" — a code decision taken by hand after reading this number, not something a
    // red test on a shared machine should force.
}
