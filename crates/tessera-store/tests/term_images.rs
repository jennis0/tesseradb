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
    chooser_inputs, derive_term_images, DeriveOptions, TermImageStamp, TermImages,
    KEEP_ROWS_PER_CONTAINER,
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
    let mut fewer_rows_than_entities = 0;
    for (term, posting) in postings.iter().enumerate() {
        let term = TermId::new(term as u32);
        let entry = images.entry(term).expect("within the dictionary");
        let mut expected = space.project_base(posting);
        expected.run_optimize();
        let stats = expected.statistics();

        assert_eq!(
            u64::from(entry.entities),
            posting.cardinality(),
            "term {term:?} entities is the posting's own cardinality"
        );
        assert!(
            entry.rows <= u64::from(entry.entities),
            "term {term:?} cannot hold more rows than its posting has entities"
        );
        if entry.rows < u64::from(entry.entities) {
            fewer_rows_than_entities += 1;
        }

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
    assert!(
        fewer_rows_than_entities > 0,
        "the fixture's permutation must leave some entities without a row, so that the two \
         columns are distinguishable"
    );
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

/// The chooser's residual is the unkept terms' **entities**, not their rows.
///
/// The walk reads one permutation slot per entity whether or not the slot holds a row, so a
/// permutation that gives some entities no row makes the two columns differ and the sum must
/// follow the larger one.
#[test]
fn the_chooser_prices_the_residual_in_entities() {
    const BOUND: u64 = 3 << 16;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut rng = StdRng::seed_from_u64(11);
    let entities = row_order(BOUND, 0.9, Some(1), &mut rng);
    let space = space_of(dir.path(), &entities, BOUND);
    let postings = mixed_postings(40, BOUND, &mut rng);

    let out = dir.path().join("chooser.timg");
    derive(&space, &postings, &out, 1).expect("derive");
    let images = TermImages::open(&out, &stamp_of(&space), postings.len() as u32).expect("opens");

    let satisfied: Vec<TermId> = (0..postings.len() as u32).map(TermId::new).collect();
    let inputs = chooser_inputs(&images, &satisfied, 1_000, BOUND, false, 0);

    let mut unkept_entities = 0u64;
    let mut unkept_rows = 0u64;
    for term in satisfied.iter().copied() {
        let entry = images.entry(term).expect("within the dictionary");
        if !entry.kept() {
            unkept_entities += u64::from(entry.entities);
            unkept_rows += entry.rows;
        }
    }
    assert!(
        unkept_rows < unkept_entities,
        "the fixture must hold an unkept term whose posting has entities with no row"
    );
    assert_eq!(inputs.residual_entities, unkept_entities);
}

/// **A path that already holds a file is refused, and the file standing there is left alone.**
///
/// One term-image file per (prefix, view) is written once, by the publication that creates the
/// prefix. A second derivation onto a live path would put a mapped reader on bytes that no longer
/// describe its row space, and the reader has no way to notice: the stamp it checked still matches.
#[test]
fn a_second_derivation_onto_the_same_path_is_refused() {
    const BOUND: u64 = 3 << 16;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut rng = StdRng::seed_from_u64(97);
    let entities = row_order(BOUND, 0.9, Some(1), &mut rng);
    let space = space_of(dir.path(), &entities, BOUND);
    let postings = mixed_postings(24, BOUND, &mut rng);

    let out = dir.path().join("once.timg");
    derive(&space, &postings, &out, 1).expect("derive");
    let first = std::fs::read(&out).expect("read");

    let error = derive(&space, &postings, &out, 1).expect_err("the second derivation is refused");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
    assert_eq!(
        std::fs::read(&out).expect("read"),
        first,
        "the refusal leaves the file that was there"
    );
    TermImages::open(&out, &stamp_of(&space), postings.len() as u32)
        .expect("which still opens as it did");
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

/// A window that keeps nothing writes nothing, and the file is still the same at every width.
#[test]
fn a_window_holding_no_kept_image_writes_the_same_bytes_at_every_width() {
    const BOUND: u64 = 3 << 16;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut rng = StdRng::seed_from_u64(43);
    let entities = row_order(BOUND, 0.9, Some(1), &mut rng);
    let space = space_of(dir.path(), &entities, BOUND);

    // Thirty-six terms, one window per thread in flight. The middle twelve are postings of thirty
    // entities, which the derivation skips, so every window they fall in contributes no payload.
    let mut postings = mixed_postings(36, BOUND, &mut rng);
    for posting in postings.iter_mut().take(24).skip(12) {
        *posting = Bitmap::new();
        for _ in 0..KEEP_ROWS_PER_CONTAINER {
            posting.add(rng.gen_range(0..BOUND) as u32);
        }
    }

    let mut reference: Option<Vec<u8>> = None;
    for threads in [1usize, 3] {
        let out = dir.path().join(format!("gap-{threads}.timg"));
        derive(&space, &postings, &out, threads).expect("derive");
        let bytes = std::fs::read(&out).expect("read");
        match &reference {
            None => reference = Some(bytes),
            Some(first) => assert_eq!(
                first, &bytes,
                "{threads} threads must write the bytes one thread writes across an empty window"
            ),
        }
    }

    let out = dir.path().join("gap-1.timg");
    let images = TermImages::open(&out, &stamp_of(&space), 36).expect("opens");
    for term in 12..24u32 {
        assert!(
            !images.kept(TermId::new(term)),
            "term {term} is a skipped posting, so the middle window keeps nothing"
        );
    }
    assert!(
        (0..12u32).any(|term| images.kept(TermId::new(term))),
        "the first window must keep an image"
    );
    assert!(
        (24..36u32).any(|term| images.kept(TermId::new(term))),
        "the last window must keep an image"
    );
}

/// A run of terms too small to be projected, longer than the derivation's buffer of table rows,
/// between the postings a window holds.
///
/// A window holds the postings that are projected, so a term settled as it is read has its row
/// written between two windows' rows, and a run longer than the buffer is written by several
/// positioned writes. The bytes must not depend on where those writes fall.
#[test]
fn a_run_of_small_terms_between_projected_ones_writes_the_same_bytes_at_every_width() {
    const BOUND: u64 = 3 << 16;
    // Longer than the derivation's table-row buffer of 4,096 rows.
    const RUN: usize = 5_000;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut rng = StdRng::seed_from_u64(61);
    let entities = row_order(BOUND, 0.9, Some(1), &mut rng);
    let space = space_of(dir.path(), &entities, BOUND);

    // Three projected terms, a run of small ones, two projected, a second run, three projected.
    // Every eleventh term of a run holds no entities, so an empty posting sits inside a run too.
    let mut postings: Vec<Bitmap> = Vec::new();
    let mut large: Vec<u32> = Vec::new();
    for block in [3usize, 2, 3] {
        for _ in 0..block {
            let mut posting = Bitmap::new();
            // Inside the first permutation page, which the fixture keeps whole, so every entity of
            // the block holds a row and the image is dense enough to keep.
            let start = rng.gen_range(0..40_000u32);
            posting.add_range(start..start + 8_000);
            large.push(postings.len() as u32);
            postings.push(posting);
        }
        if large.len() < 8 {
            for term in 0..RUN {
                let mut posting = Bitmap::new();
                if !term.is_multiple_of(11) {
                    for _ in 0..4 {
                        posting.add(rng.gen_range(0..BOUND) as u32);
                    }
                }
                postings.push(posting);
            }
        }
    }

    let mut reference: Option<Vec<u8>> = None;
    for threads in [1usize, 3, 12] {
        let out = dir.path().join(format!("run-{threads}.timg"));
        derive(&space, &postings, &out, threads).expect("derive");
        let bytes = std::fs::read(&out).expect("read");
        match &reference {
            None => reference = Some(bytes),
            Some(first) => assert_eq!(
                first, &bytes,
                "{threads} threads must write the bytes one thread writes across a long run of \
                 small terms"
            ),
        }
    }

    let out = dir.path().join("run-1.timg");
    let images = TermImages::open(&out, &stamp_of(&space), postings.len() as u32).expect("opens");
    for (term, posting) in postings.iter().enumerate() {
        let entry = images
            .entry(TermId::new(term as u32))
            .expect("a row per term");
        assert_eq!(
            u64::from(entry.entities),
            posting.cardinality(),
            "term {term}'s row records its posting's cardinality"
        );
        if large.contains(&(term as u32)) {
            assert!(
                entry.kept(),
                "term {term} is projected and dense enough to keep"
            );
        } else {
            assert!(!entry.kept(), "term {term} is too small to be projected");
            assert_eq!(entry.containers, 0, "term {term} was not projected");
        }
    }
    assert_eq!(
        images.union(&large.iter().map(|t| TermId::new(*t)).collect::<Vec<_>>()),
        large.iter().fold(Bitmap::new(), |mut rows, term| {
            rows.or_inplace(&space.project_base(&postings[*term as usize]));
            rows
        }),
        "the images either side of the runs union to the projection of their postings"
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
