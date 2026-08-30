//! The end-to-end batch-build smoke test, at 250-item scale.
//!
//! Synthesises a tiny `points.parquet` + `pairs.parquet`, runs `build`, then re-reads the
//! bundle through the bundle read protocol and checks the properties the build is *for*:
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
use tessera_spatial::Bounds;
use tessera_store::read::open_bundle;
use tessera_types::{IdentityKey, TermId};

/// A fixed, non-degenerate test key shared by every fixture in this file.
const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

fn test_key() -> IdentityKey {
    IdentityKey::from_hex(TEST_KEY_HEX).unwrap()
}

const N_ITEMS: u64 = 250;
const N_TERMS: u64 = 17;

fn extent() -> Bounds {
    Bounds {
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

/// The Morton code item `e` carries in the `morton`-column fixture — chosen to spread across the
/// grid, including both clamp corners.
fn source_morton(e: u64) -> u64 {
    let cx = ((e * 613) % 65536) as u16;
    let cy = ((e * 977) % 65536) as u16;
    tessera_spatial::interleave(cx, cy).raw() as u64
}

/// A points file in the shape the probe corpus uses: `entity_id` + `morton`, no coordinates.
fn write_morton_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("morton", DataType::UInt64, false),
    ]));
    let ids: Vec<u64> = (0..N_ITEMS).collect();
    let codes: Vec<u64> = ids.iter().map(|e| source_morton(*e)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(UInt64Array::from(codes)),
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
    let out = match reader
        .posting(term)
        .unwrap()
        .expect("the term is present in this file")
    {
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
        views: vec![tessera_build::ViewArgs {
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            access: tessera_build::config::AccessInput::relation(pairs.clone()),
        }],
        anchor: 0,
        groups: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    };
    let report = build(&args).expect("build should succeed");
    assert_eq!(report.items, N_ITEMS);

    // ---- (a) manifests present, digests verify (open_bundle is the read protocol) ----------
    let bundle = open_bundle(&out).expect("open_bundle must verify the freshly built bundle");
    assert_eq!(bundle.manifest.bundle_format, 4);
    assert_eq!(bundle.manifest.entity_id_high_water, N_ITEMS);
    assert_eq!(bundle.manifest.small_term_threshold, 32);
    assert_eq!(bundle.manifest.partitions.len(), 1);
    assert_eq!(bundle.manifest.partitions[0].phash, "default");
    assert!(bundle.manifest.partitions[0].required_terms.is_empty());
    assert_eq!(bundle.manifest.views.len(), 1);
    assert_eq!(bundle.manifest.views[0].id, "s0");
    assert_eq!(bundle.manifest.views[0].quantisation.x_max, 1000.0);
    assert!(!bundle.manifest.data_plugin_hash.is_empty());
    assert_eq!(
        bundle.manifest.provenance["generating_set_choice"],
        "prompt-sample"
    );

    let part = &bundle.partitions["default"];
    assert_eq!(part.segments_n, 0);
    assert_eq!(part.manifest.watermark, N_ITEMS);
    assert_eq!(part.manifest.entity_id_high_water, N_ITEMS);
    assert_eq!(part.manifest.segments.len(), 1);
    assert_eq!(part.manifest.segments[0].view, "s0");
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
    assert_eq!(part.manifest.external_id_runs.len(), 1);

    // Contracts §2.2/§2.3: `MANIFEST.files` covers every file present at build time;
    // `SEGMENTS-<n>.files` covers only what was added *since* — which at build is nothing.
    // open_bundle already checked size+sha256 of each listed entry; this catches a manifest that
    // simply omits a file it should list, or that puts it in the wrong map.
    assert!(
        part.manifest.files.is_empty(),
        "a batch build adds nothing after MANIFEST.json, so SEGMENTS-0.json's files map must be \
         empty, got {:?}",
        part.manifest.files
    );
    for rel in [
        "dictionary/terms-0.dict",
        "partitions/default/terms/postings.arrow",
        "partitions/default/terms/pairs.parquet",
        "partitions/default/entities/external-ids-0.arrow",
        "partitions/default/entities/ext-locator.u32",
        "partitions/default/views/s0/permutation.bin",
        "partitions/default/views/s0/row-entity.u32",
        "partitions/default/views/s0/segments/seg-0/columns.arrow",
        "partitions/default/views/s0/segments/seg-0/morton.u32",
    ] {
        assert!(
            bundle.manifest.files.contains_key(rel),
            "MANIFEST.json must list {rel}, got {:?}",
            bundle.manifest.files.keys().collect::<Vec<_>>()
        );
    }
    assert_eq!(
        bundle.manifest.files.len(),
        9,
        "MANIFEST.json must list every build-written file and nothing else"
    );

    let prefix = &report.prefix;
    let pdir = partition_dir(&out, prefix);

    // ---- (f) external-ids extent: source id (8-byte LE) -> new entity id ------------------
    let ext_path = out.join(prefix).join(&part.manifest.external_id_runs[0]);
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
            .downcast_ref::<UInt32Array>()
            .expect("entity_id must be UInt32 (contracts §2.4 r6)");
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
            source_to_new.insert(source, ent.value(i) as u64);
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
    let view = &part.views["s0"];
    assert_eq!(view.row_space.base().bound(), N_ITEMS);
    let mut rows: BTreeSet<u32> = BTreeSet::new();
    for e in 0..N_ITEMS {
        let row = view
            .row_space
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
        views: vec![tessera_build::ViewArgs {
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points,
            point_fields: Default::default(),
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
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
        views: vec![tessera_build::ViewArgs {
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points,
            point_fields: Default::default(),
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        attribute_sources: Vec::new(),
        out: tmp.path().join("bundle"),
        limit: Some(0),
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .is_err());
}

/// A points file storing Morton codes is exact only against the grid's own extent. Any other
/// extent would re-quantise the cell indices as if they were coordinates in that extent's units,
/// while `MANIFEST.json` went on declaring the caller's extent — geometry and declared
/// quantisation silently disagreeing. The Morton branch must refuse.
#[test]
fn morton_input_requires_the_identity_extent() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    write_morton_points(&points);
    write_pairs(&pairs);

    let args = |extent| BuildArgs {
        views: vec![tessera_build::ViewArgs {
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent,
            points: points.clone(),
            point_fields: Default::default(),
            access: tessera_build::config::AccessInput::relation(pairs.clone()),
        }],
        anchor: 0,
        groups: Vec::new(),
        attribute_sources: Vec::new(),
        out: tmp
            .path()
            .join(format!("bundle-{extent:?}").replace(['/', ' '], "_")),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    };

    // The caller's own extent, which the x/y branch would happily accept, must be rejected here.
    let err = build(&args(extent())).expect_err("non-identity extent must be refused");
    assert!(
        format!("{err}").contains("0,65536,0,65536"),
        "the error must name the extent required, got: {err}"
    );
    // A near-miss on one bound is still a miss.
    assert!(build(&args(Bounds {
        x_min: 0.0,
        x_max: 65536.0,
        y_min: 0.0,
        y_max: 65535.0,
    }))
    .is_err());

    // The identity extent builds, and the bundle's Morton codes are the source codes verbatim.
    let identity = Bounds {
        x_min: 0.0,
        x_max: 65536.0,
        y_min: 0.0,
        y_max: 65536.0,
    };
    let out = tmp.path().join("bundle-ok");
    build(&BuildArgs {
        views: vec![tessera_build::ViewArgs {
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: identity,
            points,
            point_fields: Default::default(),
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .unwrap();
    let bundle = open_bundle(&out).unwrap();
    let mut got: Vec<u64> =
        std::fs::read(out.join("v00000/partitions/default/views/s0/segments/seg-0/morton.u32"))
            .unwrap()
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()) as u64)
            .collect();
    got.sort_unstable();
    let mut want: Vec<u64> = (0..N_ITEMS).map(source_morton).collect();
    want.sort_unstable();
    assert_eq!(
        got, want,
        "the bundle must reproduce the source Morton codes exactly"
    );
    assert_eq!(bundle.manifest.views[0].quantisation.x_max, 65536.0);
}

#[test]
fn build_rejects_an_unsafe_view_id() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);
    assert!(build(&BuildArgs {
        views: vec![tessera_build::ViewArgs {
            view_id: "../escape".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points,
            point_fields: Default::default(),
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        attribute_sources: Vec::new(),
        out: tmp.path().join("bundle"),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
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
        views: vec![tessera_build::ViewArgs {
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points,
            point_fields: Default::default(),
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: Some(100),
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .unwrap();
    assert_eq!(report.items, 100);
    let bundle = open_bundle(&out).unwrap();
    assert_eq!(bundle.manifest.entity_id_high_water, 100);
}

/// `tessera verify` re-derives every row's `tessera_id` from `(identity.key, identity.shard_id,
/// entity_id)` and fails if a single row disagrees (contracts §2.6 r6). A freshly built bundle
/// must verify clean.
#[test]
fn verify_accepts_a_freshly_built_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    let out = tmp.path().join("bundle");
    write_points(&points);
    write_pairs(&pairs);

    build(&BuildArgs {
        views: vec![tessera_build::ViewArgs {
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points,
            point_fields: Default::default(),
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .unwrap();

    let report = tessera_build::verify(&out).expect("a freshly built bundle must verify");
    assert_eq!(report.rows, N_ITEMS);
    assert_eq!(report.entity_id_high_water, N_ITEMS);
}

/// Contracts §2.6 r6: "`tessera verify` checks the whole column against" the key. Corrupts one
/// row's stored `tessera_id` (keeping every digest self-consistent, so the failure is the
/// identity check itself and not an earlier digest-mismatch error) and asserts `verify` refuses.
#[test]
fn verify_rejects_a_columns_file_whose_tessera_ids_do_not_match_the_key() {
    use sha2::{Digest, Sha256};

    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    let out = tmp.path().join("bundle");
    write_points(&points);
    write_pairs(&pairs);

    let report = build(&BuildArgs {
        views: vec![tessera_build::ViewArgs {
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points,
            point_fields: Default::default(),
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .unwrap();

    let columns_rel = "partitions/default/views/s0/segments/seg-0/columns.arrow".to_string();
    let columns_path = out.join(&report.prefix).join(&columns_rel);

    // ---- corrupt row 0's tessera_id, keeping the schema and every other value intact --------
    let file = File::open(&columns_path).unwrap();
    let reader = arrow::ipc::reader::FileReader::try_new(file, None).unwrap();
    let schema = reader.schema();
    let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    assert_eq!(batches.len(), 1, "the build writes one record batch");
    let batch = &batches[0];
    let tessera_id = batch
        .column(0)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    let mut corrupted: Vec<u64> = tessera_id.iter().map(|v| v.unwrap()).collect();
    corrupted[0] ^= 1; // flip the low bit: still a plausible-looking u64, still wrong
    let mut columns: Vec<Arc<dyn arrow::array::Array>> = Vec::new();
    columns.push(Arc::new(UInt64Array::from(corrupted)));
    for i in 1..batch.num_columns() {
        columns.push(batch.column(i).clone());
    }
    let corrupted_batch = RecordBatch::try_new(schema.clone(), columns).unwrap();

    let file = File::create(&columns_path).unwrap();
    let mut writer = arrow::ipc::writer::FileWriter::try_new(file, &schema).unwrap();
    writer.write(&corrupted_batch).unwrap();
    writer.finish().unwrap();

    // ---- keep MANIFEST.json's digest for this file self-consistent, so the failure below is
    // the identity check, not an earlier "file does not match its recorded digest" error -------
    let manifest_path = out.join(&report.prefix).join("MANIFEST.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    let new_bytes = std::fs::read(&columns_path).unwrap();
    let new_size = new_bytes.len() as u64;
    let new_sha256 = {
        let mut hasher = Sha256::new();
        hasher.update(&new_bytes);
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    manifest["files"][&columns_rel]["size"] = serde_json::json!(new_size);
    manifest["files"][&columns_rel]["sha256"] = serde_json::json!(new_sha256);
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    std::fs::write(&manifest_path, &manifest_bytes).unwrap();

    let manifest_sha256 = {
        let mut hasher = Sha256::new();
        hasher.update(&manifest_bytes);
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let current = serde_json::json!({
        "prefix": report.prefix,
        "manifest_digest": manifest_sha256,
    });
    std::fs::write(
        out.join("CURRENT"),
        serde_json::to_vec_pretty(&current).unwrap(),
    )
    .unwrap();

    let err = tessera_build::verify(&out)
        .expect_err("a corrupted tessera_id column must fail verification");
    assert!(
        matches!(err, tessera_build::BuildError::Invalid(_)),
        "expected BuildError::Invalid, got {err:?}"
    );
    assert!(
        format!("{err}").contains("tessera_id"),
        "the error should name the identity check, got: {err}"
    );
}

/// A points file carrying `morton` **and** `residual` holds 32 bits per axis, and the importer
/// must reassemble both words rather than reading the cell and discarding the rest.
///
/// The fixture puts every point in **one** Morton cell and separates them only by residual, so a
/// reader that ignores the residual column collapses all of them onto the cell origin and this
/// test fails. That is the whole precision claim, stated as a fixture rather than as prose.
#[test]
fn morton_plus_residual_recovers_sub_cell_position() {
    use tessera_build::input::{read_points, IDENTITY_EXTENT};

    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("points.parquet");

    // One cell, four corners of it: residual interleaves (rx, ry) with x on the even bits.
    let cell = tessera_spatial::interleave(1234, 5678).raw() as u64;
    let residual_of = |rx: u32, ry: u32| -> u64 {
        let spread = |v: u32| {
            let mut x = v as u64;
            x = (x | (x << 8)) & 0x00FF_00FF;
            x = (x | (x << 4)) & 0x0F0F_0F0F;
            x = (x | (x << 2)) & 0x3333_3333;
            x = (x | (x << 1)) & 0x5555_5555;
            x
        };
        spread(rx) | (spread(ry) << 1)
    };
    let corners = [(0u32, 0u32), (0, 65535), (65535, 0), (65535, 65535)];

    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("morton", DataType::UInt64, false),
        Field::new("residual", DataType::UInt64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from((0..4u64).collect::<Vec<_>>())),
            Arc::new(UInt64Array::from(vec![cell; 4])),
            Arc::new(UInt64Array::from(
                corners
                    .iter()
                    .map(|(a, b)| residual_of(*a, *b))
                    .collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(&points).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();

    let mut rows = read_points(
        &points,
        &Default::default(),
        tessera_spatial::Projection::None,
        &IDENTITY_EXTENT,
        None,
    )
    .unwrap();
    rows.sort_by_key(|r| r.source_id);
    assert_eq!(rows.len(), 4);

    for (row, (rx, ry)) in rows.iter().zip(corners) {
        // Every point keeps the shared cell...
        assert_eq!(row.qx >> 16, 1234, "cell x must survive");
        assert_eq!(row.qy >> 16, 5678, "cell y must survive");
        // ...and its own distinct sub-cell position.
        assert_eq!(row.qx & 0xFFFF, rx, "residual x must survive");
        assert_eq!(row.qy & 0xFFFF, ry, "residual y must survive");
    }

    // And the four are genuinely distinct, which is what a residual-ignoring reader loses.
    let distinct: std::collections::HashSet<(u32, u32)> =
        rows.iter().map(|r| (r.qx, r.qy)).collect();
    assert_eq!(distinct.len(), 4, "all four sub-cell positions must differ");
}

/// A bare `morton` column carries 16 bits per axis and no more, so the importer widens it with a
/// **zero** residual — the point sits at its cell's origin. Not a defect: inventing precision the
/// file does not hold would be the defect.
#[test]
fn bare_morton_widens_with_a_zero_residual() {
    use tessera_build::input::{read_points, IDENTITY_EXTENT};

    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("points.parquet");
    write_morton_points(&points);

    let rows = read_points(
        &points,
        &Default::default(),
        tessera_spatial::Projection::None,
        &IDENTITY_EXTENT,
        None,
    )
    .unwrap();
    assert!(!rows.is_empty());
    for row in &rows {
        assert_eq!(
            row.qx & 0xFFFF,
            0,
            "a bare morton column has no sub-cell part"
        );
        assert_eq!(
            row.qy & 0xFFFF,
            0,
            "a bare morton column has no sub-cell part"
        );
    }
}

/// **Decision 0073**: within one signature group, entity ids ascend by the item's Morton **code**,
/// and the source id only breaks a shared cell.
///
/// The fixture makes the two orders disagree on purpose — the Morton code *descends* as the source
/// id ascends — so the superseded `(signature, source_id)` rule assigns entity ids in source order
/// and this rule assigns them in the exact reverse. A test asserting only "grouped by signature"
/// passes under both, which is why the existing smoke assertions did not move when this landed.
///
/// **Two signature shapes, because the sort has two paths.** A one-term signature never leaves the
/// pre-sort, whose key now carries the code; four identical terms tie on the two-term pre-sort key
/// and are re-sorted by `refine_group`, which compares signature tails and must apply the same
/// tiebreak itself. The second path is the one an implementation forgets, and its omission is
/// invisible in a corpus of short signatures.
///
/// Both build paths are asserted: the assignment is permanent (I9), and `build_equivalence.rs`
/// holds the two to byte-identical bundles.
#[test]
fn entity_ids_break_signature_ties_on_the_morton_code() {
    const SHORT: u64 = 8; // items 0..8 carry one term
    const N: u64 = 16; // items 8..16 carry the same four

    // Distinct cells, strictly descending in the source id.
    let code_of = |e: u64| (N - 1 - e) * 4096;

    let write_fixture = |points: &Path, pairs: &Path| {
        let pschema = Arc::new(Schema::new(vec![
            Field::new("entity_id", DataType::UInt64, false),
            Field::new("morton", DataType::UInt64, false),
        ]));
        let ids: Vec<u64> = (0..N).collect();
        let codes: Vec<u64> = ids.iter().map(|&e| code_of(e)).collect();
        let batch = RecordBatch::try_new(
            pschema.clone(),
            vec![
                Arc::new(UInt64Array::from(ids)),
                Arc::new(UInt64Array::from(codes)),
            ],
        )
        .unwrap();
        let mut w = ArrowWriter::try_new(File::create(points).unwrap(), pschema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();

        let tschema = Arc::new(Schema::new(vec![
            Field::new("entity_id", DataType::UInt64, false),
            Field::new("term_id", DataType::UInt32, false),
        ]));
        let mut es: Vec<u64> = Vec::new();
        let mut ts: Vec<u32> = Vec::new();
        for e in 0..N {
            for t in if e < SHORT {
                vec![1u32]
            } else {
                vec![2, 3, 4, 5]
            } {
                es.push(e);
                ts.push(t);
            }
        }
        let batch = RecordBatch::try_new(
            tschema.clone(),
            vec![
                Arc::new(UInt64Array::from(es)),
                Arc::new(UInt32Array::from(ts)),
            ],
        )
        .unwrap();
        let mut w = ArrowWriter::try_new(File::create(pairs).unwrap(), tschema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    };

    // Term "1" interns first (items are walked in ascending source-id order), so the one-term
    // signature sorts before the four-term one and takes the low entity ids. Within each group,
    // the entity id ascends with the code, which is the source id reversed.
    let mut expected: BTreeMap<u64, u64> = BTreeMap::new();
    for e in 0..SHORT {
        expected.insert(e, SHORT - 1 - e);
    }
    for e in SHORT..N {
        expected.insert(e, SHORT + (N - 1 - e));
    }

    for (label, linear) in [("pipeline", false), ("linear", true)] {
        let tmp = tempfile::tempdir().unwrap();
        let points = tmp.path().join("points.parquet");
        let pairs = tmp.path().join("pairs.parquet");
        let out = tmp.path().join("bundle");
        write_fixture(&points, &pairs);

        let args = BuildArgs {
            views: vec![tessera_build::ViewArgs {
                view_id: "s0".to_string(),
                projection: tessera_spatial::Projection::None,
                extent: tessera_build::input::IDENTITY_EXTENT,
                points,
                point_fields: Default::default(),
                access: tessera_build::config::AccessInput::relation(pairs),
            }],
            anchor: 0,
            groups: Vec::new(),
            attribute_sources: Vec::new(),
            out: out.clone(),
            limit: None,
            identity_key: test_key(),
            identity_key_hex: TEST_KEY_HEX.to_string(),
            idset: 1,
            shard_id: 0,
            layers: Vec::new(),
            layer_inputs: Vec::new(),
            mint_external_ids: true,
            emit_oracle_pairs: false,
            batch_items: None,
            memory_budget: None,
            band_rows: None,
            schema: Default::default(),
        };
        let report = if linear {
            tessera_build::build_in_memory(&args)
        } else {
            build(&args)
        }
        .unwrap_or_else(|e| panic!("{label} build failed: {e}"));

        let bundle = open_bundle(&out).expect("the bundle must open");
        let part = &bundle.partitions["default"];
        let ext_path = out
            .join(&report.prefix)
            .join(&part.manifest.external_id_runs[0]);
        let mut source_to_new: BTreeMap<u64, u64> = BTreeMap::new();
        for batch in read_arrow_ipc(&ext_path) {
            let ext = batch
                .column(0)
                .as_any()
                .downcast_ref::<BinaryArray>()
                .expect("external_id is binary");
            let ent = batch
                .column(1)
                .as_any()
                .downcast_ref::<UInt32Array>()
                .expect("entity_id is u32");
            for i in 0..batch.num_rows() {
                let source = u64::from_le_bytes(ext.value(i).try_into().unwrap());
                source_to_new.insert(source, ent.value(i) as u64);
            }
        }

        assert_eq!(
            source_to_new, expected,
            "{label}: entity ids must ascend with the Morton code inside each signature group \
             (decision 0073) — this fixture's source-id order is its exact reverse"
        );
    }
}
