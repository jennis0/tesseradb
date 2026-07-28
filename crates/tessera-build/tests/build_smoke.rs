//! Task 8 Step 1: the end-to-end batch-build smoke test at 250-item scale.
//!
//! Synthesises a tiny `points.parquet` + `pairs.parquet`, runs `build`, then re-reads the
//! bundle through the Task 7 read protocol and checks the properties the build is *for*:
//! digest-verified manifests, signature-grouped entity IDs (I9/§11.1), postings that agree
//! with the input relation, the `(term, entity)`-sorted `pairs.parquet`, and the external-ids
//! extent.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Array, BinaryArray, Float64Array, LargeBinaryArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;

use tessera_build::{build, signature_sort_key, BuildArgs};
use tessera_spatial::Extent;
use tessera_store::read::open_bundle;
use tessera_types::TermId;

const N_ITEMS: u64 = 250;
const N_TERMS: u64 = 17;

fn extent() -> Extent {
    Extent {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

/// Deterministic synthetic input: item `e` at `(x, y)` derived from `e`, carrying a term set
/// chosen so that many items share an identical signature (which is what makes assertion (b)
/// meaningful) and some items carry no terms at all.
fn synth_terms(e: u64) -> Vec<u64> {
    let group = e % 12;
    match group {
        0 => vec![],
        1 => vec![3],
        2 => vec![3, 5],
        3 => vec![5, 3], // same set, different input order — must intern to the same signature
        _ => {
            let mut t: Vec<u64> = vec![group, (group * 7) % N_TERMS, (e / 25) % N_TERMS];
            t.sort_unstable();
            t.dedup();
            t
        }
    }
}

fn write_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..N_ITEMS).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_pairs(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for e in 0..N_ITEMS {
        for t in synth_terms(e) {
            entities.push(e);
            terms.push(t as u32);
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
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn read_parquet(path: &Path) -> Vec<RecordBatch> {
    ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap())
        .unwrap()
        .build()
        .unwrap()
        .map(|b| b.unwrap())
        .collect()
}

fn read_arrow_ipc(path: &Path) -> Vec<RecordBatch> {
    let reader = arrow::ipc::reader::FileReader::try_new(File::open(path).unwrap(), None).unwrap();
    reader.map(|b| b.unwrap()).collect()
}

fn posting_entities(path: &Path, term: TermId) -> BTreeSet<u64> {
    use tessera_authz::{PostingRef, PostingsReader};
    let reader = PostingsReader::open(path, false).unwrap();
    let out = match reader.posting(term).unwrap() {
        PostingRef::Array(bytes) => bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()) as u64)
            .collect(),
        PostingRef::Roaring(view) => view.iter().map(|v| v as u64).collect(),
    };
    out
}

fn partition_dir(root: &Path, prefix: &str) -> PathBuf {
    root.join(prefix).join("partitions").join("default")
}

#[test]
fn build_produces_a_verifiable_signature_sorted_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    let out = tmp.path().join("bundle");
    write_points(&points);
    write_pairs(&pairs);

    let args = BuildArgs {
        points: points.clone(),
        pairs: pairs.clone(),
        out: out.clone(),
        extent: extent(),
        slice_id: "s0".to_string(),
        limit: None,
    };
    let report = build(&args).expect("build should succeed");
    assert_eq!(report.items, N_ITEMS);

    // ---- (a) manifests present, digests verify (open_bundle is the read protocol) ----------
    let bundle = open_bundle(&out).expect("open_bundle must verify the freshly built bundle");
    assert_eq!(bundle.manifest.bundle_format, 1);
    assert_eq!(bundle.manifest.entity_id_high_water, N_ITEMS);
    assert_eq!(bundle.manifest.small_term_threshold, 32);
    assert_eq!(bundle.manifest.partitions.len(), 1);
    assert_eq!(bundle.manifest.partitions[0].phash, "default");
    assert!(bundle.manifest.partitions[0].required_terms.is_empty());
    assert_eq!(bundle.manifest.slices.len(), 1);
    assert_eq!(bundle.manifest.slices[0].id, "s0");
    assert_eq!(bundle.manifest.quantisation.x_max, 1000.0);
    assert!(!bundle.manifest.data_plugin_hash.is_empty());
    assert_eq!(
        bundle.manifest.provenance["generating_set_choice"],
        "prompt-sample"
    );

    let part = &bundle.partitions["default"];
    assert_eq!(part.manifest.segments_version, 0);
    assert_eq!(part.manifest.watermark, N_ITEMS);
    assert_eq!(part.manifest.entity_id_high_water, N_ITEMS);
    assert_eq!(part.manifest.segments.len(), 1);
    assert_eq!(part.manifest.segments[0].slice, "s0");
    assert_eq!(part.manifest.segments[0].row_count, N_ITEMS as u32);
    assert_eq!(part.manifest.segments[0].entity_lo, 0);
    assert_eq!(part.manifest.segments[0].entity_hi, N_ITEMS - 1);
    assert!(part.manifest.deltas.is_empty());
    assert!(part.manifest.tombstones.is_empty());
    assert!(part.manifest.deny.is_empty());
    assert_eq!(part.manifest.dict_extents.len(), 1);
    assert_eq!(
        part.manifest.dict_extents[0].path,
        "dictionary/terms-0.dict"
    );
    assert_eq!(part.manifest.external_id_extents.len(), 1);

    // Every file named by either manifest exists (open_bundle already checked size+sha256 of
    // the entries; this catches a manifest that simply omits a file it should list).
    for rel in ["terms/postings.arrow", "terms/pairs.parquet"] {
        let key = format!("partitions/default/{rel}");
        assert!(
            part.manifest.files.contains_key(&key),
            "SEGMENTS-0.json must list {key}"
        );
    }

    let prefix = &report.prefix;
    let pdir = partition_dir(&out, prefix);

    // ---- (f) external-ids extent: source id (8-byte LE) -> new entity id ------------------
    let ext_path = out.join(prefix).join(&part.manifest.external_id_extents[0]);
    let ext_batches = read_arrow_ipc(&ext_path);
    let mut source_to_new: BTreeMap<u64, u64> = BTreeMap::new();
    let mut prev_key: Option<Vec<u8>> = None;
    for batch in &ext_batches {
        let ext = batch
            .column(0)
            .as_any()
            .downcast_ref::<BinaryArray>()
            .expect("external_id must be Binary");
        let ent = batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .expect("entity_id must be UInt64");
        for i in 0..batch.num_rows() {
            let key = ext.value(i).to_vec();
            assert_eq!(key.len(), 8, "external_id is the source id as 8-byte LE");
            if let Some(p) = &prev_key {
                assert!(
                    p.as_slice() < key.as_slice(),
                    "external-ids must be sorted by bytes"
                );
            }
            prev_key = Some(key.clone());
            let source = u64::from_le_bytes(key.try_into().unwrap());
            source_to_new.insert(source, ent.value(i));
        }
    }
    assert_eq!(source_to_new.len(), N_ITEMS as usize);
    for e in 0..N_ITEMS {
        assert!(
            source_to_new.contains_key(&e),
            "source id {e} must be mapped"
        );
    }

    // ---- (b) entity IDs are dense 0..n and grouped by signature --------------------------
    let mut new_ids: Vec<u64> = source_to_new.values().copied().collect();
    new_ids.sort_unstable();
    assert_eq!(
        new_ids,
        (0..N_ITEMS).collect::<Vec<_>>(),
        "IDs must be dense 0..n"
    );

    // Rebuild each item's signature in *term-id* space by reading the dictionary and the
    // source relation, then check items sharing a signature occupy one contiguous ID range.
    let dict =
        tessera_authz::Dict::load(&[out.join(prefix).join("dictionary/terms-0.dict")]).unwrap();
    let mut sig_of_new: BTreeMap<u64, Vec<u32>> = BTreeMap::new();
    for e in 0..N_ITEMS {
        let terms: Vec<TermId> = synth_terms(e)
            .iter()
            .map(|t| {
                dict.lookup(t.to_string().as_bytes())
                    .expect("descriptor interned")
            })
            .collect();
        sig_of_new.insert(source_to_new[&e], signature_sort_key(&terms));
    }
    // Walk in new-ID order: a signature must never reappear after a different one intervened.
    let mut seen: BTreeSet<Vec<u32>> = BTreeSet::new();
    let mut current: Option<Vec<u32>> = None;
    let mut ordered: Vec<Vec<u32>> = Vec::new();
    for e in 0..N_ITEMS {
        let sig = sig_of_new[&e].clone();
        if current.as_ref() != Some(&sig) {
            assert!(
                seen.insert(sig.clone()),
                "signature {sig:?} reappears non-contiguously at entity {e} — not signature-sorted"
            );
            current = Some(sig.clone());
            ordered.push(sig);
        }
    }
    // The signature blocks themselves must be in lexicographic order (the assignment rule).
    let mut sorted_blocks = ordered.clone();
    sorted_blocks.sort();
    assert_eq!(
        ordered, sorted_blocks,
        "signature blocks must be lexicographically ordered"
    );
    assert!(
        ordered.len() > 3,
        "the fixture must exercise several distinct signatures"
    );

    // ---- (c) postings row count == dictionary length ------------------------------------
    let postings_path = pdir.join("terms/postings.arrow");
    let reader = tessera_authz::PostingsReader::open(&postings_path, false).unwrap();
    assert_eq!(reader.term_count(), dict.len());
    assert_eq!(dict.len() as u64, report.terms);

    // ---- (d) each term's posting == the set of new entity ids carrying it ---------------
    let mut expected: BTreeMap<u32, BTreeSet<u64>> = BTreeMap::new();
    for e in 0..N_ITEMS {
        for t in synth_terms(e) {
            let tid = dict.lookup(t.to_string().as_bytes()).unwrap();
            expected
                .entry(tid.raw())
                .or_default()
                .insert(source_to_new[&e]);
        }
    }
    for t in 0..dict.len() {
        assert_eq!(
            posting_entities(&postings_path, TermId::new(t)),
            *expected.get(&t).unwrap_or(&BTreeSet::new()),
            "posting for term {t} disagrees with the input relation"
        );
    }

    // ---- (e) pairs.parquet is sorted by (term_id, entity_id) and complete ---------------
    let pairs_path = pdir.join("terms/pairs.parquet");
    let mut got_pairs: Vec<(u32, u64)> = Vec::new();
    for batch in read_parquet(&pairs_path) {
        let schema = batch.schema();
        assert_eq!(schema.field(0).name(), "entity_id");
        assert_eq!(schema.field(0).data_type(), &DataType::UInt64);
        assert_eq!(schema.field(1).name(), "term_id");
        assert_eq!(schema.field(1).data_type(), &DataType::UInt32);
        let ent = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        let ter = batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        for i in 0..batch.num_rows() {
            got_pairs.push((ter.value(i), ent.value(i)));
        }
    }
    let mut want_pairs: Vec<(u32, u64)> = expected
        .iter()
        .flat_map(|(t, es)| es.iter().map(move |e| (*t, *e)))
        .collect();
    want_pairs.sort_unstable();
    assert_eq!(
        got_pairs, want_pairs,
        "pairs.parquet must be (term, entity)-sorted and complete"
    );
    assert_eq!(report.pairs, want_pairs.len() as u64);

    // DELTA_BINARY_PACKED is the contract (R4) — assert the encoding actually used.
    let meta =
        parquet::file::reader::SerializedFileReader::new(File::open(&pairs_path).unwrap()).unwrap();
    use parquet::file::reader::FileReader;
    for rg in 0..meta.metadata().num_row_groups() {
        for col in 0..2 {
            let enc: Vec<_> = meta
                .metadata()
                .row_group(rg)
                .column(col)
                .encodings()
                .collect();
            assert!(
                enc.contains(&parquet::basic::Encoding::DELTA_BINARY_PACKED),
                "column {col} must use DELTA_BINARY_PACKED, got {enc:?}"
            );
        }
    }

    // ---- postings.arrow shape: one LargeBinary column, row ordinal = term_id ------------
    let posting_batches = read_arrow_ipc(&postings_path);
    assert_eq!(posting_batches.len(), 1);
    assert!(posting_batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<LargeBinaryArray>()
        .is_some());

    // ---- permutation is a bijection over the segment's rows ------------------------------
    let slice = &part.slices["s0"];
    assert_eq!(slice.permutation.bound(), N_ITEMS);
    let mut rows: BTreeSet<u32> = BTreeSet::new();
    for e in 0..N_ITEMS {
        let row = slice
            .permutation
            .row_of(tessera_types::EntityId::new(e))
            .expect("every entity has a row");
        assert!(rows.insert(row.raw()), "row {} assigned twice", row.raw());
    }
    assert_eq!(rows.len(), N_ITEMS as usize);
}

#[test]
fn build_refuses_to_clobber_an_existing_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    let out = tmp.path().join("bundle");
    write_points(&points);
    write_pairs(&pairs);
    let args = BuildArgs {
        points,
        pairs,
        out: out.clone(),
        extent: extent(),
        slice_id: "s0".to_string(),
        limit: None,
    };
    build(&args).unwrap();
    // A second build into the same root would leave the first bundle's files half-overwritten
    // while its CURRENT still pointed at them.
    assert!(build(&args).is_err());
    // The existing bundle is untouched and still opens.
    open_bundle(&out).unwrap();
}

#[test]
fn build_rejects_an_empty_selection() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);
    assert!(build(&BuildArgs {
        points,
        pairs,
        out: tmp.path().join("bundle"),
        extent: extent(),
        slice_id: "s0".to_string(),
        limit: Some(0),
    })
    .is_err());
}

#[test]
fn build_rejects_an_unsafe_slice_id() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);
    assert!(build(&BuildArgs {
        points,
        pairs,
        out: tmp.path().join("bundle"),
        extent: extent(),
        slice_id: "../escape".to_string(),
        limit: None,
    })
    .is_err());
}

#[test]
fn signature_sort_key_is_the_sorted_term_id_list() {
    let key = signature_sort_key(&[TermId::new(9), TermId::new(2), TermId::new(5)]);
    assert_eq!(key, vec![2, 5, 9]);
    // Input order must not matter — the signature is a *set*.
    assert_eq!(
        signature_sort_key(&[TermId::new(2), TermId::new(9), TermId::new(5)]),
        key
    );
    assert!(signature_sort_key(&[]).is_empty());
}

#[test]
fn limit_filters_the_source_entity_id_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    let out = tmp.path().join("bundle");
    write_points(&points);
    write_pairs(&pairs);

    let report = build(&BuildArgs {
        points,
        pairs,
        out: out.clone(),
        extent: extent(),
        slice_id: "s0".to_string(),
        limit: Some(100),
    })
    .unwrap();
    assert_eq!(report.items, 100);
    let bundle = open_bundle(&out).unwrap();
    assert_eq!(bundle.manifest.entity_id_high_water, 100);
}
