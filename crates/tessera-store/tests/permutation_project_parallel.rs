//! `Permutation::project`'s agreement with an independent serial reference.
//!
//! [`serial_project`] is an independent implementation of the obvious algorithm — entity by entity
//! through `row_of`, then sort, then insert — deliberately *not* a second call to
//! `Permutation::project`, because that method's bucketing and its container encoding are the
//! things under test and an oracle sharing their code would not catch a bug in either.
//!
//! What the cases below are chosen to reach, since `project`'s cost model makes the interesting
//! boundaries invisible from the outside:
//!
//! - **Both container encodings.** croaring stores a block of more than 4,096 members as a bitset
//!   and the rest as a sorted array, and `project` writes the payload for whichever the cardinality
//!   implies. Writing the wrong one is the format error least likely to announce itself, because the
//!   deserializer picks how to *read* a payload from the descriptor rather than from the bytes.
//! - **More than one bucket.** `project` partitions row space into 2²²-row buckets and stamps each
//!   into a reused bit array. A fixture below that width exercises exactly one bucket, so the reuse
//!   — and the clearing between buckets that the reuse depends on — never runs.
//! - **Sentinels and out-of-bound entities**, which are skipped rather than erred.
//! - **Any ambient rayon pool, or none.** The crate owns no pool and `project` no longer asks one
//!   for anything, so what these pin is that it is indifferent to what it is called inside.
//!
//! **The file's name is older than its subject.** It was written when `project` was parallel and
//! carried the measured gate that decided whether to keep it that way; the one-pass rewrite made
//! the answer moot and the gate was deleted with it. The name stays because four reports in
//! `probes/2026-07-31-concurrency-workstream/` cite this path as a record of what they changed, and
//! a tidier name here would rot all four for nothing.

use std::path::Path;

use croaring::Bitmap;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

use tessera_store::write::write_permutation;
use tessera_store::Permutation;
use tessera_types::EntityId;

/// Independent oracle: walk every set bit of `mask` through `perm.row_of`, one entity at a time,
/// skipping absent/out-of-bound entities exactly as `project` does, then sort.
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

/// Run `perm.project(mask)` inside a rayon pool with exactly `threads` workers — the ambient-rayon
/// contract (tessera-store owns no pool of its own; this installs one exactly as `tessera-engine`
/// does at the real call site). `project` is serial and uses none of it, which is the point: the
/// answer must not depend on what the caller happens to have installed.
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

/// **More than one bucket**, which every case above misses: they use a few thousand entities and
/// row space is bucketed at 2²², so all of them exercise bucket zero and nothing else. What only
/// appears here is the bit array being *reused* across buckets — and therefore the clearing between
/// them, which is done by re-walking each bucket's rows rather than wiping the array, and which a
/// single-bucket fixture can never catch getting wrong.
///
/// The permutation is scattered by a multiplier coprime to `n` rather than by a shuffle: it is a
/// bijection for the same reason, it costs one pass instead of sorting 4 million entries, and it
/// sends adjacent entities to distant buckets, which is the arrangement that makes the reuse matter.
#[test]
fn a_projection_spanning_several_buckets_matches_the_serial_reference() {
    const BUCKET_ROWS: u32 = 1 << 22;
    let dir = tempfile::tempdir().expect("tempdir");
    // Two buckets and a bit, so the last one is partial as a real row space's last one is.
    let n = (BUCKET_ROWS as u64) * 2 + 5_000;
    // Coprime to `n`, so `r -> (r * STRIDE) % n` is a bijection on `[0, n)`.
    const STRIDE: u64 = 1_000_003;
    assert_ne!(n % STRIDE, 0, "sanity: the multiplier must not divide n");

    let entities_in_row_order: Vec<EntityId> = (0..n)
        .map(|row| EntityId::new((row * STRIDE) % n))
        .collect();
    let perm = build_permutation(dir.path(), &entities_in_row_order, n);

    for (label, keep) in [
        // 3 keeps ~21,800 rows per container, comfortably above croaring's 4,096 crossover,
        // at a third of the oracle's sorting cost.
        ("dense: bitset containers throughout", 3u64),
        ("sparse: array containers throughout", 5_000),
    ] {
        let mut mask = Bitmap::new();
        for e in (0..n).step_by(keep as usize) {
            mask.add(e as u32);
        }
        let expected = serial_project(&perm, &mask);
        // At least two buckets occupied, which is what makes the bit array's reuse and the clearing
        // between buckets run at all. Not three: the last bucket here is a 5,000-row sliver and the
        // sparse arm lands in it only by luck, so requiring it would make this assertion a coin
        // toss rather than a check.
        assert!(
            expected.maximum().expect("non-empty") >= BUCKET_ROWS,
            "{label}: sanity — the fixture must span more than one bucket"
        );
        for threads in [1, 4] {
            let got = project_with_threads(&perm, &mask, threads);
            assert_bitmaps_equal(&got, &expected, &format!("{label}, {threads} threads"));
        }
    }
}
