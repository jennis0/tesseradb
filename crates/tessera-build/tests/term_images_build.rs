//! **The term images a build writes, against the projections they stand in for.**
//!
//! A term image is one authorisation term's base posting projected into a view's row space
//! (`tessera_store::term_images`). A session that holds the term reads the image instead of
//! walking the term's entities through the permutation, so the file is right only if every kept
//! image is exactly what that walk would have produced, and the table beside it describes every
//! term whether or not one was kept.
//!
//! The checks here are the ones a reader cannot make for itself. The open validates the file's
//! shape and its stamp; nothing in the bundle says whether the bytes inside a kept image are the
//! image of the right posting, and the only way to know is to project the posting again and
//! compare.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use croaring::Bitmap;
use parquet::arrow::ArrowWriter;

use tessera_authz::postings::{PostingRef, PostingsReader};
use tessera_build::{build, build_in_memory, BuildArgs};
use tessera_spatial::Bounds;
use tessera_store::manifest::DECLARED_INCARNATION;
use tessera_store::read::open_bundle;
use tessera_store::term_images::{
    TermImageRefusal, TermImageStamp, TermImages, KEEP_ROWS_PER_CONTAINER,
};
use tessera_store::RowSpace;
use tessera_types::{IdentityKey, TermId};

/// Rows enough for a second Roaring container, which is what puts a term whose image is spread
/// over both on the wrong side of the keep rule: thirty rows a container is a cut a scattered
/// term fails and a dense one passes, and with one container every term of more than thirty
/// entities would pass.
const N_ITEMS: u64 = 70_000;

/// Items pinned to the top right corner of the extent, which in Morton order are the highest rows
/// in the view, and to the bottom left, which are the lowest. A term over both spans the two
/// containers.
const CORNER: u64 = 20;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

fn test_key() -> IdentityKey {
    IdentityKey::from_hex(TEST_KEY_HEX).unwrap()
}

fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1024.0,
        y_min: 0.0,
        y_max: 1024.0,
    }
}

/// `(source_id, x, y)` for every item.
///
/// The first [`CORNER`] items sit in the top right corner and the next [`CORNER`] in the bottom
/// left, each at its own coordinate; the rest are scattered by two coprime strides so that entity
/// order and row order have nothing to do with one another.
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

/// The terms one item carries, by source id.
///
/// Sized to reach every branch of the derivation: a term over the whole corpus and one over a
/// third of it (kept), one over the two corners (projected, and refused by the keep rule because
/// its forty rows are spread over two containers), one of exactly [`KEEP_ROWS_PER_CONTAINER`]
/// entities and one of fewer (neither projected at all), and one of thirty-one items in one
/// corner (projected, and kept).
fn terms_of(e: u64) -> Vec<u32> {
    let mut terms = vec![0];
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
    terms
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
    let mut writer = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None)
        .expect("a parquet writer");
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
    let mut writer = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None)
        .expect("a parquet writer");
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

fn args_for(points: &Path, pairs: &Path, out: PathBuf) -> BuildArgs {
    BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out,
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: false,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    }
}

/// The fixture, built by whichever route `run` is.
fn built(
    dir: &Path,
    name: &str,
    run: fn(&BuildArgs) -> tessera_build::error::Result<tessera_build::BuildReport>,
) -> PathBuf {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    if !points.exists() {
        write_points(&points);
        write_pairs(&pairs);
    }
    let out = dir.join(name);
    std::fs::create_dir_all(&out).unwrap();
    run(&args_for(&points, &pairs, out.clone())).expect("the fixture builds");
    out
}

fn posting_bitmap(postings: &PostingsReader, term: u32) -> Option<Bitmap> {
    match postings.posting_at(term).expect("the postings read") {
        None => None,
        Some(PostingRef::Array(bytes)) => {
            let mut bitmap = Bitmap::new();
            for chunk in bytes.chunks_exact(4) {
                bitmap.add(u32::from_le_bytes(chunk.try_into().unwrap()));
            }
            Some(bitmap)
        }
        Some(PostingRef::Roaring(view)) => Some(view.clone()),
    }
}

/// The one term-image file of a bundle with one view, and the row space it was projected through.
fn images_of(root: &Path) -> (Arc<TermImages>, RowSpace, PathBuf) {
    let bundle = open_bundle(root).expect("the bundle opens");
    let partition = bundle.partitions.get("default").expect("one partition");
    let entry = partition
        .manifest
        .term_image_extents
        .iter()
        .find(|entry| entry.view == "s0")
        .expect("the side-manifest names the view's term images");
    let path = root.join("v00000").join(&entry.path);
    let view = partition.views.get("s0").expect("the view");
    let images = view
        .term_images
        .clone()
        .expect("the view's term images are mapped");
    (images, view.row_space.clone(), path)
}

#[test]
fn every_kept_image_is_the_projection_of_its_posting() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = built(temp.path(), "streaming", build);
    let (images, space, _) = images_of(&root);

    let postings = PostingsReader::open(
        &root
            .join("v00000")
            .join("partitions")
            .join("default")
            .join("terms")
            .join("postings.arrow"),
        true,
    )
    .expect("the postings open");
    assert_eq!(images.dict_len(), postings.term_count());

    let (mut kept, mut skipped, mut refused_by_the_rule) = (0u32, 0u32, 0u32);
    for term in 0..images.dict_len() {
        let entry = images.entry(TermId::new(term)).expect("a table entry");
        let Some(posting) = posting_bitmap(&postings, term) else {
            assert_eq!(entry, Default::default(), "term {term} has no posting");
            continue;
        };
        if posting.cardinality() <= KEEP_ROWS_PER_CONTAINER {
            // Not projected: an image holds at most one row per entity and occupies at least one
            // container, so such a posting cannot pass the keep rule whatever the permutation
            // does. The table carries the posting's own cardinality instead.
            assert_eq!(entry.rows, posting.cardinality(), "term {term}");
            assert_eq!(entry.containers, 0, "term {term}");
            assert!(!entry.kept(), "term {term}");
            skipped += 1;
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
            kept += 1;
        } else {
            refused_by_the_rule += 1;
        }
    }

    // The fixture reaches all three outcomes, so the equalities above are not vacuous.
    assert!(kept >= 2, "kept {kept}");
    assert!(skipped >= 2, "skipped {skipped}");
    assert!(
        refused_by_the_rule >= 1,
        "refused by the rule {refused_by_the_rule}"
    );
}

#[test]
fn both_build_routes_write_the_same_images() {
    let temp = tempfile::TempDir::new().unwrap();
    let streaming = built(temp.path(), "streaming", build);
    let reference = built(temp.path(), "reference", build_in_memory);
    let (_, _, left) = images_of(&streaming);
    let (_, _, right) = images_of(&reference);
    assert_eq!(
        left.strip_prefix(&streaming).unwrap(),
        right.strip_prefix(&reference).unwrap(),
        "the two routes name the file differently"
    );
    assert_eq!(
        std::fs::read(&left).unwrap(),
        std::fs::read(&right).unwrap(),
        "the two routes wrote different term images"
    );
}

#[test]
fn a_flipped_byte_in_an_image_refuses_the_bundle() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = built(temp.path(), "streaming", build);
    let (images, _, path) = images_of(&root);
    assert!(
        images.file_len() > images.payload_offset(),
        "the fixture keeps at least one image"
    );

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

#[test]
fn a_stamp_the_bundle_disagrees_with_leaves_the_view_walking() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = built(temp.path(), "streaming", build);
    let prefix = root.join("v00000");
    let (images, space, path) = images_of(&root);
    let rows = images.stamp().base_rows;

    // The stamp's base row count, at its offset in the header. Which field this is, is checked
    // below: a write that landed anywhere else gives one of the other refusals.
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[88..92].copy_from_slice(&(rows + 1).to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let expected = TermImageStamp {
        prefix: "v00000".to_string(),
        view: "s0".to_string(),
        base_seg_id: "seg-0".to_string(),
        incarnation: DECLARED_INCARNATION,
        base_rows: rows,
        bound: space.base().bound(),
    };
    let dict_len = images.dict_len();
    drop(images);
    let refusal =
        TermImages::open(&path, &expected, dict_len).expect_err("a wrong stamp is refused");
    assert!(
        matches!(refusal, TermImageRefusal::BaseRows { .. }),
        "{refusal}"
    );

    // The file is digested like every other, so the manifest has to be forged with it or the
    // sweep refuses the bundle before the stamp is ever read.
    redigest(&prefix, &root, &path);

    let bundle = open_bundle(&root).expect("the bundle still opens");
    assert!(
        bundle.partitions["default"].views["s0"]
            .term_images
            .is_none(),
        "a refused file leaves the view with no images"
    );
}

/// Two entries for one `(view, incarnation)` describe a state no publication produces, and there
/// is no rule for choosing between them.
#[test]
fn two_term_image_extents_for_one_view_refuse_the_bundle() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = built(temp.path(), "streaming", build);
    rewrite_side_manifest(&root.join("v00000"), |manifest| {
        let list = manifest["term_image_extents"]
            .as_array_mut()
            .expect("the list the build wrote");
        let first = list[0].clone();
        list.push(first);
    });

    let error = open_bundle(&root).expect_err("two extents for one view refuse the bundle");
    assert!(
        matches!(error, tessera_store::StoreError::MalformedBundle { .. }),
        "{error}"
    );
}

/// A file derived under another keep rule is priced by the chooser against the wrong cost model,
/// so it is dropped. The view serves by the walk; the refusal names the view in a warning.
#[test]
fn a_file_derived_under_another_keep_rule_leaves_the_view_walking() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = built(temp.path(), "streaming", build);
    rewrite_side_manifest(&root.join("v00000"), |manifest| {
        manifest["term_image_extents"][0]["keep_rows_per_container"] =
            serde_json::Value::from(KEEP_ROWS_PER_CONTAINER - 1);
    });

    let bundle = open_bundle(&root).expect("the bundle still opens");
    assert!(
        bundle.partitions["default"].views["s0"]
            .term_images
            .is_none(),
        "a file under another keep rule leaves the view with no images"
    );
}

/// A file the manifests name and no digest covers is one the loader would map without its bytes
/// having been verified, which is the check every derived file passes.
#[test]
fn a_term_image_file_no_digest_covers_refuses_the_bundle() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = built(temp.path(), "streaming", build);
    let prefix = root.join("v00000");
    let (images, _, path) = images_of(&root);
    drop(images);

    let rel = rel_of(&prefix, &path);
    rewrite_manifest(&prefix, &root, |manifest| {
        manifest["files"]
            .as_object_mut()
            .expect("the files map")
            .remove(&rel)
            .expect("the build digested the file");
    });

    let error = open_bundle(&root).expect_err("an undigested file refuses the bundle");
    assert!(
        matches!(error, tessera_store::StoreError::UnverifiedFile { .. }),
        "{error}"
    );
}

/// Restate `path`'s digest in `MANIFEST.json`, so that a file edited in place passes the sweep and
/// the check under test is the one that runs.
fn redigest(prefix: &Path, root: &Path, path: &Path) {
    use sha2::{Digest, Sha256};

    let rel = rel_of(prefix, path);
    let bytes = std::fs::read(path).unwrap();
    rewrite_manifest(prefix, root, |manifest| {
        manifest["files"][&rel]["sha256"] =
            serde_json::Value::String(hex_of(&Sha256::digest(&bytes)));
        manifest["files"][&rel]["size"] = serde_json::Value::from(bytes.len() as u64);
    });
}

/// Edit `MANIFEST.json` and restate its own digest in `CURRENT`, which is what makes the edit
/// reachable: the pointer carries the manifest's digest and the open checks it first.
fn rewrite_manifest(prefix: &Path, root: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    use sha2::{Digest, Sha256};

    let manifest_path = prefix.join("MANIFEST.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    edit(&mut manifest);
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    std::fs::write(&manifest_path, &manifest_bytes).unwrap();

    let current = serde_json::json!({
        "prefix": prefix.file_name().unwrap().to_string_lossy(),
        "manifest_digest": hex_of(&Sha256::digest(&manifest_bytes)),
    });
    std::fs::write(
        root.join("CURRENT"),
        serde_json::to_vec_pretty(&current).unwrap(),
    )
    .unwrap();
}

/// Edit the partition's `SEGMENTS-0.json`. Nothing digests a side-manifest, so an edit to one
/// needs no other file moved with it: the loader verifies the files it *names*.
fn rewrite_side_manifest(prefix: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = prefix
        .join("partitions")
        .join("default")
        .join("SEGMENTS-0.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut manifest);
    std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
}

/// A file's prefix-relative name, as the manifests carry it.
fn rel_of(prefix: &Path, path: &Path) -> String {
    path.strip_prefix(prefix)
        .unwrap()
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// A digest as the manifests carry it.
fn hex_of(digest: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(out, "{byte:02x}").expect("a string takes bytes");
    }
    out
}
