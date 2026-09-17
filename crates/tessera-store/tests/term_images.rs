//! Term images against an independent projection: the round trip, the byte identity across
//! parallel widths, and the exactness the split route rests on.
//!
//! The oracle everywhere below is `RowSpace::project_base` called directly on the posting or on the
//! residual set. That is the routine the derivation itself calls, so these tests do not establish
//! that the projection is right; `permutation_project_parallel.rs` does that against a serial
//! reference. What they establish is that the file carries what was projected, that the table
//! describes it, and that the union of images plus the projection of a residual is the projection
//! of the whole fragment.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use croaring::Bitmap;
use proptest::prelude::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use tessera_store::derived::PostingSlice;
use tessera_store::permutation::Permutation;
use tessera_store::term_images::{
    derive_term_images, DeriveOptions, TermImageStamp, TermImages, KEEP_ROWS_PER_CONTAINER,
};
use tessera_store::write::write_permutation;
use tessera_store::RowSpace;
use tessera_types::{EntityId, TermId};

/// Entities in row order, ascending in entity id, taken from `[0, bound)` at `density`, with the
/// entities in `absent_page` left out altogether so one whole permutation page is missing.
fn row_order(
    bound: u64,
    density: f64,
    absent_page: Option<u64>,
    rng: &mut StdRng,
) -> Vec<EntityId> {
    const PAGE_ENTRIES: u64 = 1 << 16;
    (0..bound)
        .filter(|e| match absent_page {
            Some(page) => e / PAGE_ENTRIES != page,
            None => true,
        })
        .filter(|_| rng.gen_bool(density))
        .map(EntityId::new)
        .collect()
}

fn space_of(dir: &Path, entities: &[EntityId], bound: u64) -> RowSpace {
    let path = dir.join("permutation.bin");
    write_permutation(&path, entities, bound).expect("write_permutation");
    let perm = Permutation::load(&path).expect("load permutation");
    RowSpace::new(Arc::new(perm), entities.len() as u32)
}

fn stamp_of(space: &RowSpace) -> TermImageStamp {
    TermImageStamp {
        prefix: "prefix-000007".to_string(),
        view: "group:region=north".to_string(),
        base_seg_id: "seg-000012".to_string(),
        incarnation: 5,
        base_rows: space.base_rows(),
        bound: space.base().bound(),
    }
}

/// Ascending little-endian `u32`s, the array form a posting file stores a small term in.
fn array_bytes(posting: &Bitmap) -> Vec<u8> {
    posting
        .iter()
        .flat_map(|entity| entity.to_le_bytes())
        .collect()
}

/// Serve `postings` through a posting walk, every third term as an `Array` slice and the rest as a
/// `Roaring` one, so both decodes run in every derivation.
fn derive(
    space: &RowSpace,
    postings: &[Bitmap],
    out: &Path,
    threads: usize,
) -> std::io::Result<()> {
    let arrays: Vec<Vec<u8>> = postings.iter().map(array_bytes).collect();
    let walk = |term: u32, visit: &mut dyn FnMut(PostingSlice<'_>)| -> std::io::Result<()> {
        let Some(posting) = postings.get(term as usize) else {
            return Ok(());
        };
        if term.is_multiple_of(3) {
            visit(PostingSlice::Array(&arrays[term as usize]));
        } else {
            visit(PostingSlice::Roaring(posting));
        }
        Ok(())
    };
    derive_term_images(
        space,
        postings.len() as u32,
        &walk,
        &stamp_of(space),
        out,
        DeriveOptions { threads },
    )
    .map(|_| ())
}

/// Postings of mixed size and scatter over `[0, bound)`: a few dense blocks, a few strided sets, a
/// few too small to pass the skip, and one empty.
fn mixed_postings(count: usize, bound: u64, rng: &mut StdRng) -> Vec<Bitmap> {
    (0..count)
        .map(|term| {
            let mut posting = Bitmap::new();
            match term % 5 {
                0 => {
                    let start = rng.gen_range(0..bound / 2) as u32;
                    let width = rng.gen_range(2_000..20_000u32);
                    posting.add_range(start..start + width);
                }
                1 => {
                    let stride = rng.gen_range(2..17u32);
                    let mut at = rng.gen_range(0..bound / 2) as u32;
                    for _ in 0..rng.gen_range(500..4_000) {
                        posting.add(at);
                        at = at.saturating_add(stride);
                    }
                }
                2 => {
                    for _ in 0..rng.gen_range(31..400) {
                        posting.add(rng.gen_range(0..bound) as u32);
                    }
                }
                3 => {
                    for _ in 0..rng.gen_range(1..=KEEP_ROWS_PER_CONTAINER) {
                        posting.add(rng.gen_range(0..bound) as u32);
                    }
                }
                _ => {}
            }
            posting
        })
        .collect()
}

/// Every table row describes an independent projection of the same posting, every kept image is
/// that projection, and a union of images is the union of the projections.
#[test]
fn a_derived_file_carries_what_an_independent_projection_produces() {
    const BOUND: u64 = 3 << 16;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut rng = StdRng::seed_from_u64(11);
    let entities = row_order(BOUND, 0.9, Some(1), &mut rng);
    let space = space_of(dir.path(), &entities, BOUND);
    let postings = mixed_postings(40, BOUND, &mut rng);

    let out = dir.path().join("round-trip.timg");
    derive(&space, &postings, &out, 1).expect("derive");
    let images = TermImages::open(&out, &stamp_of(&space), postings.len() as u32).expect("opens");
    assert_eq!(images.dict_len(), postings.len() as u32);
    assert_eq!(images.stamp(), &stamp_of(&space));

    let mut kept = Vec::new();
    for (term, posting) in postings.iter().enumerate() {
        let term = TermId::new(term as u32);
        let entry = images.entry(term).expect("within the dictionary");
        let mut expected = space.project_base(posting);
        expected.run_optimize();
        let stats = expected.statistics();

        if posting.cardinality() <= KEEP_ROWS_PER_CONTAINER {
            assert_eq!(
                entry.rows,
                posting.cardinality(),
                "a skipped posting records its own cardinality"
            );
            assert_eq!(entry.containers, 0);
            assert!(!entry.kept());
            continue;
        }

        assert_eq!(entry.rows, stats.cardinality, "term {term:?} rows");
        assert_eq!(
            entry.containers, stats.n_containers,
            "term {term:?} containers"
        );
        assert_eq!(
            entry.arrays, stats.n_array_containers,
            "term {term:?} arrays"
        );
        assert_eq!(entry.runs, stats.n_run_containers, "term {term:?} runs");
        assert_eq!(
            entry.bitsets, stats.n_bitset_containers,
            "term {term:?} bitsets"
        );
        assert_eq!(
            entry.kept(),
            stats.cardinality > KEEP_ROWS_PER_CONTAINER * u64::from(stats.n_containers),
            "term {term:?} keep rule"
        );

        if entry.kept() {
            let view = images.view(term).expect("a kept image has a view");
            assert_eq!(
                view.iter().collect::<Vec<u32>>(),
                expected.iter().collect::<Vec<u32>>(),
                "term {term:?} image"
            );
            kept.push(term);
        } else {
            assert!(images.view(term).is_none());
        }
    }

    assert!(kept.len() >= 8, "the fixture must keep several images");
    let unioned = images.union(&kept);
    let projections: Vec<Bitmap> = kept
        .iter()
        .map(|term| space.project_base(&postings[term.raw() as usize]))
        .collect();
    let refs: Vec<&Bitmap> = projections.iter().collect();
    assert_eq!(
        unioned.iter().collect::<Vec<u32>>(),
        Bitmap::fast_or(&refs).iter().collect::<Vec<u32>>(),
        "the union of images is the union of the projections"
    );

    // A term above the dictionary has no entry, no view, and contributes nothing to a union.
    let above = TermId::new(postings.len() as u32);
    assert_eq!(images.entry(above), None);
    assert!(images.view(above).is_none());
    assert!(images.union(&[above]).is_empty());
    assert!(images.union(&[]).is_empty());
}

/// The file does not depend on how many threads derived it.
#[test]
fn every_parallel_width_writes_the_same_bytes() {
    const BOUND: u64 = 3 << 16;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut rng = StdRng::seed_from_u64(29);
    let entities = row_order(BOUND, 0.85, Some(2), &mut rng);
    let space = space_of(dir.path(), &entities, BOUND);
    let postings = mixed_postings(64, BOUND, &mut rng);

    let mut reference: Option<Vec<u8>> = None;
    for threads in [1usize, 2, 7] {
        let out = dir.path().join(format!("threads-{threads}.timg"));
        derive(&space, &postings, &out, threads).expect("derive");
        let bytes = std::fs::read(&out).expect("read");
        match &reference {
            None => reference = Some(bytes),
            Some(first) => assert_eq!(
                first, &bytes,
                "{threads} threads must write the bytes one thread writes"
            ),
        }
    }
    let bytes = reference.expect("at least one derivation");
    let out = dir.path().join("threads-1.timg");
    let images = TermImages::open(&out, &stamp_of(&space), postings.len() as u32).expect("opens");
    assert_eq!(images.file_len(), bytes.len());
    assert!(
        images.payload_offset() < bytes.len(),
        "the fixture must keep at least one image"
    );
}

/// The exactness the split route rests on: for any kept subset K and any S between the residual and
/// the whole fragment, the union of K's images with the projection of S is the projection of the
/// fragment.
fn exactness_case(seed: u64, term_count: usize) {
    const BOUND: u64 = 3 << 16;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut rng = StdRng::seed_from_u64(seed);
    let entities = row_order(BOUND, 0.8, Some(1), &mut rng);
    let space = space_of(dir.path(), &entities, BOUND);
    let postings = mixed_postings(term_count, BOUND, &mut rng);

    let out = dir.path().join("exactness.timg");
    derive(&space, &postings, &out, 1).expect("derive");
    let images = TermImages::open(&out, &stamp_of(&space), postings.len() as u32).expect("opens");

    // A random satisfied subset, and a random kept subset of the satisfied terms that have images.
    let satisfied: Vec<TermId> = (0..postings.len() as u32)
        .filter(|_| rng.gen_bool(0.6))
        .map(TermId::new)
        .collect();
    let choosable: Vec<TermId> = satisfied
        .iter()
        .copied()
        .filter(|term| images.kept(*term))
        .collect();
    let chosen: Vec<TermId> = choosable
        .iter()
        .copied()
        .filter(|_| rng.gen_bool(0.7))
        .collect();

    // F is the union of the satisfied postings, which is what `build_fragment_with_deltas` returns
    // over the base alone. R is the union of the satisfied postings with no image in K.
    let satisfied_postings: Vec<&Bitmap> = satisfied
        .iter()
        .map(|term| &postings[term.raw() as usize])
        .collect();
    let mut fragment = if satisfied_postings.is_empty() {
        Bitmap::new()
    } else {
        Bitmap::fast_or(&satisfied_postings)
    };
    fragment.run_optimize();

    let chosen_set: BTreeSet<u32> = chosen.iter().map(|term| term.raw()).collect();
    let residual_postings: Vec<&Bitmap> = satisfied
        .iter()
        .filter(|term| !chosen_set.contains(&term.raw()))
        .map(|term| &postings[term.raw() as usize])
        .collect();
    let residual = if residual_postings.is_empty() {
        Bitmap::new()
    } else {
        Bitmap::fast_or(&residual_postings)
    };

    // Between R and F: R plus a random half of the fragment.
    let mut between = residual.clone();
    for entity in fragment.iter() {
        if rng.gen_bool(0.5) {
            between.add(entity);
        }
    }

    let whole = space.project_base(&fragment);
    let union_of_images = images.union(&chosen);
    for (name, set) in [
        ("the residual", &residual),
        ("the whole fragment", &fragment),
        ("a set between them", &between),
    ] {
        let mut built = union_of_images.clone();
        built.or_inplace(&space.project_base(set));
        assert_eq!(
            built.iter().collect::<Vec<u32>>(),
            whole.iter().collect::<Vec<u32>>(),
            "seed {seed}: the images of {} kept terms plus {name} must project the fragment",
            chosen.len()
        );
    }

    // The upper bound on S matters: an entity outside the fragment that holds a row adds a row the
    // fragment does not grant, which is why a caller intersects its residual with the fragment.
    let outside: Vec<u32> = entities
        .iter()
        .map(|e| e.raw() as u32)
        .filter(|e| !fragment.contains(*e))
        .take(4)
        .collect();
    if !outside.is_empty() {
        let mut widened = residual.clone();
        widened.add_many(&outside);
        let mut built = union_of_images.clone();
        built.or_inplace(&space.project_base(&widened));
        assert_ne!(
            built.iter().collect::<Vec<u32>>(),
            whole.iter().collect::<Vec<u32>>(),
            "seed {seed}: a residual reaching outside the fragment must not project the fragment"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    /// The exactness identity over random permutations, postings, satisfied sets and kept subsets.
    #[test]
    fn images_plus_a_residual_project_the_fragment(seed in 0u64..1_000_000, terms in 12usize..40) {
        exactness_case(seed, terms);
    }
}
