//! **The term images a fold writes**, against the projections they stand in for.
//!
//! A fold rewrites the row space and the postings together, so its images cannot be carried
//! forward from the prefix it folds: every row id moves when a deletion drops a row beneath it.
//! Pass 2b derives them again from the fold's own new `permutation.bin` and new `postings.arrow`,
//! and these cases are the ones a reader cannot make for itself: that each kept image is the
//! projection of the posting the same fold wrote, that a folded deletion is in neither, and that
//! the file the new side-manifest names is the one the reopened bundle maps.
//!
//! `tests/../../tessera-build/tests/term_images_build.rs` holds the build's half. The two are one
//! derivation (decisions 0091 and 0139), which is what the byte-identity case here asserts.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use croaring::Bitmap;
use parquet::arrow::ArrowWriter;
use tessera_authz::postings::{PostingRef, PostingsReader};
use tessera_build::{build, BuildArgs};
use tessera_engine::Engine;
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::ChangeOp;
use tessera_store::read::open_bundle;
use tessera_store::term_images::{TermImages, HEADER_BYTES, KEEP_ROWS_PER_CONTAINER};
use tessera_store::RowSpace;
use tessera_types::{EntityId, TermId};

/// Rows enough for a second Roaring container, which is what puts a term whose image is spread
/// over both on the wrong side of the keep rule: thirty rows a container is a cut a scattered term
/// fails and a dense one passes, and with one container every term of more than thirty entities
/// would pass. The build's fixture is sized the same way and for the same reason.
const N_ITEMS: u64 = 70_000;

/// The fixture's views. Both hold the whole corpus; what they are for is the file naming, so the
/// second one's geometry need not differ from the first's.
const VIEWS: [&str; 2] = ["s0", "s1"];

/// Items pinned to the top right corner of the extent, which in Morton order are the highest rows
/// in the view, and to the bottom left, which are the lowest. A term over both spans the two
/// containers.
const CORNER: u64 = 20;

/// The source id carrying [`LONE_TERM`], and the item every deletion case deletes.
const LONE_SOURCE: u64 = 500;

/// The descriptor one item carries and no other, so that folding that item away empties the
/// posting and leaves the term a zero row in the table.
const LONE_TERM: u32 = 6;

/// The descriptor every item carries, whose image is kept.
const ALL_TERM: u32 = 0;

/// The terms one item carries, by source id.
///
/// Sized to reach every branch of the derivation: a term over the whole corpus and one over a
/// third of it (kept), one over the two corners (projected, and refused by the keep rule because
/// its forty rows are spread over two containers), one of exactly [`KEEP_ROWS_PER_CONTAINER`]
/// entities and one of fewer (neither projected at all), and [`LONE_TERM`]'s single item.
fn terms_of(e: u64) -> Vec<u32> {
    let mut terms = vec![ALL_TERM];
    if e.is_multiple_of(3) {
        terms.push(1);
    }
    if e < 2 * CORNER {
        terms.push(2);
    }
    if (2 * CORNER..2 * CORNER + KEEP_ROWS_PER_CONTAINER).contains(&e) {
        terms.push(3);
    }
    if (100..110).contains(&e) {
        terms.push(4);
    }
    if (200..231).contains(&e) {
        terms.push(5);
    }
    if e == LONE_SOURCE {
        terms.push(LONE_TERM);
    }
    terms
}

/// `(source_id, x, y)` for every item: the two corners first, then a scatter by two coprime
/// strides so that entity order and row order have nothing to do with one another.
fn geometry() -> (Vec<u64>, Vec<f64>, Vec<f64>) {
    let mut ids = Vec::with_capacity(N_ITEMS as usize);
    let mut xs = Vec::with_capacity(N_ITEMS as usize);
    let mut ys = Vec::with_capacity(N_ITEMS as usize);
    for e in 0..N_ITEMS {
        ids.push(e);
        if e < CORNER {
            xs.push((1023 - e) as f64);
            ys.push((1023 - e) as f64);
        } else if e < 2 * CORNER {
            xs.push((e - CORNER) as f64);
            ys.push((e - CORNER) as f64);
        } else {
            xs.push(((e * 7919) % 1024) as f64);
            ys.push(((e * 104729) % 1024) as f64);
        }
    }
    (ids, xs, ys)
}

fn write_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let (ids, xs, ys) = geometry();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut writer =
        ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

fn write_pairs(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for e in 0..N_ITEMS {
        for term in terms_of(e) {
            entities.push(e);
            terms.push(term);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut writer =
        ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// The fixture bundle at `root`, over the corpus above.
///
/// Its own builder rather than `common::build_fixture`: that fixture's two terms are both over a
/// third of the corpus or more, so every one of its images is kept and a case about the keep rule
/// would hold vacuously.
fn build_fixture(tmp: &Path, root: &Path) {
    let points = tmp.join("points.parquet");
    let pairs = tmp.join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);
    // **Two views over the one corpus**, because one view cannot show that each view's images are
    // its own file: the counter that names them runs across the calls of a publication, and two
    // views numbering from zero would write one file twice while both manifest entries stood.
    let view_args = |view: &str| tessera_build::ViewArgs {
        visibility: None,
        view_id: view.to_string(),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: points.clone(),
        point_fields: Default::default(),
        select: None,
        access: tessera_build::config::AccessInput::relation(pairs.clone()),
    };
    let args = BuildArgs {
        views: vec![view_args(VIEWS[0]), view_args(VIEWS[1])],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: root.to_path_buf(),
        schema: Default::default(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
    };
    build(&args).expect("the fixture builds");
}

/// An engine over the fixture, executor running and the background refresh off. That is
/// `tests/fold.rs`'s shape, for its reason: a refresh pass after the flip would mask a request
/// path that failed to notice the rotation.
fn engine_over_fixture(tmp: &Path, root: &Path) -> Engine {
    build_fixture(tmp, root);
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config_uncapped(),
    )
    .expect("the engine opens against a freshly built bundle");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    engine
}

/// The ordinal the dictionary gives one of the fixture's descriptors.
///
/// **Resolved rather than assumed to be the number in the pairs file.** A term id in the input is
/// a descriptor like any other. The build interns it and hands out an ordinal of its own, and the
/// two numberings do not agree. Ordinals are stable across a fold (`tests/fold.rs`), so one
/// resolution before a fold holds after it.
fn ordinal(engine: &Engine, descriptor: u32) -> TermId {
    engine
        .generation()
        .dict
        .lookup(descriptor.to_string().as_bytes())
        .expect("the fixture's descriptor is in the dictionary")
}

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// One view's term-image file in the bundle `CURRENT` names, the row space it was projected
/// through, and its path on disc.
fn images_of(root: &Path, prefix: &str, view: &str) -> (Arc<TermImages>, RowSpace, PathBuf) {
    let bundle = open_bundle(root).expect("the bundle opens");
    let partition = bundle.partitions.get("default").expect("one partition");
    let named: Vec<_> = partition
        .manifest
        .term_image_extents
        .iter()
        .filter(|entry| entry.view == view)
        .collect();
    assert_eq!(
        named.len(),
        1,
        "the side-manifest names one term-image file for view '{view}'"
    );
    let path = root.join(prefix).join(&named[0].path);
    let view = partition.views.get(view).expect("the view");
    let images = view
        .term_images
        .clone()
        .expect("the view's term images are mapped");
    assert_eq!(images.dict_len(), named[0].dict_len);
    (images, view.row_space.clone(), path)
}

/// Every view's entry in the side-manifest of the bundle `CURRENT` names.
fn extents_of(root: &Path) -> Vec<tessera_store::manifest::TermImageExtent> {
    open_bundle(root).expect("the bundle opens").partitions["default"]
        .manifest
        .term_image_extents
        .clone()
}

fn postings_of(root: &Path, prefix: &str) -> PostingsReader {
    PostingsReader::open(
        &root
            .join(prefix)
            .join("partitions")
            .join("default")
            .join("terms")
            .join("postings.arrow"),
        true,
    )
    .expect("the postings open")
}

fn posting_bitmap(postings: &PostingsReader, term: u32) -> Option<Bitmap> {
    match postings.posting_at(term).expect("the postings read") {
        None => None,
        Some(PostingRef::Array(bytes)) => {
            let mut bitmap = Bitmap::new();
            for chunk in bytes.as_chunks::<4>().0 {
                bitmap.add(u32::from_le_bytes(*chunk));
            }
            Some(bitmap)
        }
        Some(PostingRef::Roaring(view)) => Some(view.clone()),
    }
}

/// Hold every term's image against an independent projection of that term's posting, and return
/// how many landed on each side of the keep rule.
///
/// The independent walk is the point: nothing in the bundle says whether a kept image is the image
/// of the right posting, and the only way to know is to project the posting again and compare.
fn check_every_image(images: &TermImages, space: &RowSpace, postings: &PostingsReader) -> Counts {
    assert_eq!(images.dict_len(), postings.term_count());
    let mut counts = Counts::default();
    for term in 0..images.dict_len() {
        let entry = images.entry(TermId::new(term)).expect("a table entry");
        let Some(posting) = posting_bitmap(postings, term) else {
            assert_eq!(entry, Default::default(), "term {term} has no posting");
            counts.absent += 1;
            continue;
        };
        if posting.cardinality() <= KEEP_ROWS_PER_CONTAINER {
            // Not projected: an image holds at most one row per entity and occupies at least one
            // container, so such a posting cannot pass the keep rule whatever the permutation
            // does. The table carries the posting's own cardinality instead.
            assert_eq!(entry.rows, posting.cardinality(), "term {term}");
            assert_eq!(entry.containers, 0, "term {term}");
            assert!(!entry.kept(), "term {term}");
            counts.skipped += 1;
            continue;
        }

        let mut projected = space.project_base(&posting);
        projected.run_optimize();
        let stats = projected.statistics();
        assert_eq!(entry.rows, stats.cardinality, "term {term}");
        assert_eq!(entry.containers, stats.n_containers, "term {term}");
        assert_eq!(entry.arrays, stats.n_array_containers, "term {term}");
        assert_eq!(entry.runs, stats.n_run_containers, "term {term}");
        assert_eq!(entry.bitsets, stats.n_bitset_containers, "term {term}");

        let dense = stats.cardinality > KEEP_ROWS_PER_CONTAINER * u64::from(stats.n_containers);
        assert_eq!(entry.kept(), dense, "term {term} against the keep rule");
        if dense {
            let image = images.view(TermId::new(term)).expect("a kept image");
            assert!(image.eq(&projected), "term {term}'s image is not its rows");
            counts.kept += 1;
        } else {
            counts.refused += 1;
        }
    }
    counts
}

#[derive(Default, Debug)]
struct Counts {
    kept: u32,
    /// Projected, and refused by the keep rule.
    refused: u32,
    /// Too small to project.
    skipped: u32,
    /// No posting at all.
    absent: u32,
}

impl Counts {
    /// Every branch of the derivation was reached, so an assertion inside it is not vacuous.
    fn assert_every_branch(&self) {
        assert!(self.kept >= 2, "kept {self:?}");
        assert!(self.refused >= 1, "refused by the keep rule {self:?}");
        assert!(self.skipped >= 2, "too small to project {self:?}");
    }
}

/// Every entity the new postings name anywhere.
fn entities_in_any_posting(postings: &PostingsReader) -> Bitmap {
    let mut all = Bitmap::new();
    for term in 0..postings.term_count() {
        if let Some(posting) = posting_bitmap(postings, term) {
            all |= posting;
        }
    }
    all
}

/// **The fold's images are the fold's own postings, projected through the fold's own
/// permutation.**
///
/// Kills the mutation that derives pass 2b from the *live* generation's postings or row space
/// rather than the files pass 1 and pass 2 have just written: the two agree exactly when nothing
/// was deleted, which is why the deletion case below exists beside this one.
#[test]
fn a_folds_images_are_the_projections_of_the_folds_own_postings() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root);

    fold(&engine);
    assert_eq!(engine.generation().prefix, "v00001");

    // The live generation, which `publish_fold` opened through `open_written_prefix`: each view
    // carries its mapped images, so the writer's own reopen reaches the files it just wrote.
    let live = engine.generation();
    for view in VIEWS {
        assert!(
            live.bundle.partitions["default"].views[view]
                .term_images
                .is_some(),
            "the prefix the fold just wrote maps view '{view}'s images"
        );
    }

    // One entry per view, each naming its own file.
    let extents = extents_of(&root);
    assert_eq!(extents.len(), VIEWS.len());
    let paths: std::collections::BTreeSet<&String> = extents.iter().map(|e| &e.path).collect();
    assert_eq!(paths.len(), VIEWS.len(), "two views, two files");

    // And a full open from the root, which verifies every digest before mapping anything.
    let postings = postings_of(&root, "v00001");
    for view in VIEWS {
        let (images, space, _) = images_of(&root, "v00001", view);
        assert_eq!(images.stamp().prefix, "v00001");
        assert_eq!(images.stamp().view, view);
        assert_eq!(images.stamp().base_rows, space.base_rows());
        assert_eq!(images.stamp().bound, space.base().bound());
        check_every_image(&images, &space, &postings).assert_every_branch();
    }
}

/// **A fold that deletes nothing rewrites the build's images byte for byte**, which is what "one
/// implementation writes images at build and at fold" means in the artefact rather than in the
/// source (decisions 0091 and 0139).
///
/// The header is excluded because it carries the stamp, and the stamp names the prefix and the
/// base segment the images belong to, both of which a fold changes. Everything below it is the
/// table and the payload.
#[test]
fn a_fold_over_a_fresh_build_writes_the_builds_own_images() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root);

    let (built_images, _, built_path) = images_of(&root, "v00000", VIEWS[0]);
    let built = std::fs::read(&built_path).unwrap();
    assert!(
        built.len() > HEADER_BYTES,
        "the fixture must write a table at least"
    );
    assert!(
        built_images.file_len() > built_images.payload_offset(),
        "and keep at least one image, or the payload comparison is empty"
    );
    drop(built_images);

    fold(&engine);
    let (_, _, folded_path) = images_of(&root, "v00001", VIEWS[0]);
    let folded = std::fs::read(&folded_path).unwrap();

    assert_eq!(
        &folded[HEADER_BYTES..],
        &built[HEADER_BYTES..],
        "a fold that dropped no row must derive the same table and the same payload as the build"
    );
    assert_ne!(
        &folded[..HEADER_BYTES],
        &built[..HEADER_BYTES],
        "and a different stamp, the prefix and the base segment both having changed"
    );
}

/// **A folded deletion leaves the postings and the images together** (write-path §5.4's Rule F,
/// reaching this artefact through pass 2 and nothing else).
///
/// The entity is in no posting the fold wrote, so it is in no image; it holds no row at all in the
/// new permutation; and [`LONE_TERM`], whose only carrier it was, is left a zero row in the table
/// rather than an entry naming somebody else's rows.
#[test]
fn a_folded_deletion_is_in_no_posting_and_so_in_no_image() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root);

    let deleted = EntityId::new(source_to_new_map(&root, "v00000")[&LONE_SOURCE]);
    let lone_term = ordinal(&engine, LONE_TERM);
    let before = images_of(&root, "v00000", VIEWS[0]).0;
    let lone_before = before
        .entry(lone_term)
        .expect("the lone term is in the build's table");
    assert_eq!(
        lone_before.rows, 1,
        "the fixture must give the lone term exactly one carrier"
    );
    drop(before);

    engine
        .accept_change(deleted, ChangeOp::Delete)
        .expect("a delete is accepted");
    fold(&engine);
    assert_eq!(engine.generation().prefix, "v00001");

    let (images, space, _) = images_of(&root, "v00001", VIEWS[0]);
    let postings = postings_of(&root, "v00001");
    check_every_image(&images, &space, &postings).assert_every_branch();

    assert!(
        !entities_in_any_posting(&postings).contains(deleted.raw() as u32),
        "the folded entity is in no posting of the new base"
    );
    assert!(
        space.base().row_of(deleted).is_none(),
        "and holds no row in the new permutation, so no projection can reach it"
    );
    let lone = images
        .entry(lone_term)
        .expect("the lone term still has a table entry");
    assert_eq!(
        lone,
        Default::default(),
        "a term whose only carrier was folded away is a zero row"
    );
}

/// **A fold's images cover the new base, which is everything the fold folded in**, including the
/// rows a flush appended as an extent before it ran.
///
/// The flushed entities are above the built corpus's entity space and their rows are new, so an
/// image carried forward from the build would name rows for the wrong items. This is the case that
/// tells a derived image from a copied one.
#[test]
fn a_flush_before_the_fold_is_in_the_folds_images() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root);

    let all_term = ordinal(&engine, ALL_TERM);
    let mut rows = Vec::new();
    for i in 0..64u64 {
        rows.push(UnallocatedRow {
            external_id: Some(format!("flushed-{i}").into_bytes()),
            view: "s0".to_string(),
            join: None,
            descriptors: vec![b"0".to_vec()],
            x: (i % 1000) as f64,
            y: ((i * 7) % 1000) as f64,
            scalars: Vec::new(),
            terms: engine.resolve_terms(&[b"0".to_vec()]),
            scoped: Vec::new(),
        });
    }
    let flushed: Vec<EntityId> = engine
        .accept_ingest(rows, "flush-before-the-fold".to_string(), [0u8; 32])
        .expect("the ingest is accepted");
    assert_eq!(flushed.len(), 64);
    let flushes_before = engine.write_executor_stats().flushes;
    engine.request_flush();
    wait_for("the flush to publish", || {
        engine.write_executor_stats().flushes > flushes_before
    });

    fold(&engine);
    let (images, space, _) = images_of(&root, "v00001", VIEWS[0]);
    let postings = postings_of(&root, "v00001");
    check_every_image(&images, &space, &postings).assert_every_branch();

    let image = images
        .view(all_term)
        .expect("the term every item carries is kept");
    for entity in &flushed {
        let row = space
            .base()
            .row_of(*entity)
            .expect("a flushed entity is in the folded base, not in an extent");
        assert!(
            image.contains(row.raw()),
            "the flushed entity's row is in the kept term's image"
        );
    }
    assert_eq!(
        space.extents().len(),
        0,
        "the fold leaves one base and no extent, so the images cover every row"
    );
}

/// **The fold's image file is digested with the fold's other files.** A flipped byte inside it
/// refuses the whole bundle at open, exactly as one in the build's does.
///
/// Kills the mutation that writes the file and omits it from pass 5's list: the bundle would then
/// open on bytes nothing covers, which is the one thing `ensure_verified` exists to prevent.
#[test]
fn a_flipped_byte_in_a_folds_image_refuses_the_bundle() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let engine = engine_over_fixture(tmp.path(), &root);
    fold(&engine);
    drop(engine);

    let (images, _, path) = images_of(&root, "v00001", VIEWS[0]);
    assert!(
        images.file_len() > images.payload_offset(),
        "the fixture keeps at least one image"
    );
    drop(images);

    // The last byte of the file is inside the last kept image, the payload being the tail.
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    let error = open_bundle(&root).expect_err("a corrupt image refuses the bundle");
    assert!(
        matches!(
            error,
            tessera_store::StoreError::FileVerificationFailed { .. }
        ),
        "{error}"
    );
}
