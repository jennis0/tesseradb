//! `tessera build` — the batch build (plan §5, Task 8).
//!
//! Composes the tiler, the dictionary, the postings writer and the segment writers into one
//! verifiable bundle (Reference Sheet R4). The pipeline is deliberately linear and in-memory:
//! at the Phase 1 scales (250k / 2.4M items) it fits comfortably, and an obviously-correct
//! construction is worth more here than a streaming one (design-for-audit).
//!
//! ## Entity-ID assignment is permanent
//!
//! Items are ordered by their **signature** — the sorted list of their term IDs — and the new
//! entity ID is simply the position in that order (§11.1). Ties break on the external
//! (source-corpus) ID so the assignment is total and deterministic.
//!
//! This is not an optimisation that can be retrofitted. Entity IDs are append-only and never
//! reused (I9), so the ordering chosen at the first build is the ordering the corpus keeps
//! forever; a later build cannot re-sort entity space without invalidating every posting,
//! permutation and handle ever issued. Phase 0 measured 8.9–36.7x posting compression from
//! this ordering (probes/results.md) — the reason it ships in the walking skeleton rather than
//! waiting for a performance phase. The rule lives in [`signature_sort_key`] as a free
//! function so the serving allocator (Task 9) applies exactly the same rule to appended items.

pub mod error;
pub mod input;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use arrow::array::{ArrayRef, BinaryArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::FileWriter as ArrowFileWriter;
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, Encoding};
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterVersion};
use sha2::{Digest, Sha256};

use tessera_authz::{write_postings, DictWriter};
use tessera_plugin::{Passthrough, Plugin};
use tessera_spatial::tiler::{sort_batch, TilerItem};
use tessera_spatial::Extent;
use tessera_store::manifest::{
    CurrentPointer, DictExtent, FileDigest, Manifest, PartitionDescriptor, Quantisation,
    SegmentDescriptor, SegmentsManifest, SliceDescriptor,
};
use tessera_store::write::{write_permutation, write_segment};
use tessera_types::{EntityId, TermId, BUNDLE_FORMAT, NODE_NONE, SMALL_TERM_THRESHOLD_DEFAULT};

pub use error::{BuildError, Result};

/// The single bundle prefix a batch build writes. Later publications get their own prefix; the
/// batch build always starts a bundle from scratch.
const PREFIX: &str = "v00000";
/// Phase 1 has exactly one partition (no compartments — scope constraint 11).
const PHASH: &str = "default";
/// Phase 1 has exactly one segment per (partition, slice) at build (R4).
const SEG_ID: &str = "seg-0";

/// Arguments to [`build`].
#[derive(Debug, Clone)]
pub struct BuildArgs {
    /// Parquet file of points: `entity_id` plus either `x`/`y` or `morton` (see [`input`]).
    pub points: PathBuf,
    /// Parquet file of the exploded `(entity_id, term_id)` relation.
    pub pairs: PathBuf,
    /// Bundle root to create.
    pub out: PathBuf,
    /// The quantisation extent Morton codes are computed against (contracts §2.5).
    pub extent: Extent,
    /// The slice this build's segment belongs to.
    pub slice_id: String,
    /// Prefix filter on the *source* entity ID: keep rows with `entity_id < limit`.
    pub limit: Option<u64>,
}

/// What a completed build produced.
#[derive(Debug, Clone)]
pub struct BuildReport {
    pub prefix: String,
    pub slice_id: String,
    pub seg_id: String,
    /// Number of items (= `entity_id_high_water`, since the bootstrap build allocates from 0).
    pub items: u64,
    /// Number of distinct terms in the dictionary.
    pub terms: u64,
    /// Number of `(entity, term)` pairs written.
    pub pairs: u64,
    /// Total size on disk of every file the manifests name.
    pub bundle_bytes: u64,
}

/// The signature-sorted assignment key (§11.1): an item's **sorted term-ID list**.
///
/// Items are ordered by this key lexicographically, ties broken by external ID, and each item's
/// new entity ID is its position in that order. Items with identical term sets therefore occupy
/// a contiguous entity-ID range, which is what turns their postings into runs — the measured
/// 8.9–36.7x compression. **Permanent under I9:** entity IDs are never reused, so this ordering
/// cannot be changed after the first build. The serving allocator (Task 9) calls this same
/// function.
pub fn signature_sort_key(terms: &[TermId]) -> Vec<u32> {
    let mut key: Vec<u32> = terms.iter().map(|t| t.raw()).collect();
    key.sort_unstable();
    key.dedup();
    key
}

/// `priority(e) = (splitmix64(e) >> 48) as u16` over the **final** entity ID (R3, contracts
/// §2.6). Mask-independent by construction (design §7.2): priority must not depend on any
/// viewer's visibility, or the intra-cell tiebreak would leak.
pub fn priority_of(entity_id: EntityId) -> u16 {
    let mut z = entity_id.raw().wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 48) as u16
}

/// One item after labelling, before entity-ID assignment.
struct StagedItem {
    source_id: u64,
    x: f32,
    y: f32,
    terms: Vec<TermId>,
    signature: Vec<u32>,
}

/// Run the batch build, producing a complete bundle at `args.out`.
pub fn build(args: &BuildArgs) -> Result<BuildReport> {
    args.extent
        .validate()
        .map_err(|detail| BuildError::Invalid(format!("extent: {detail}")))?;
    for (what, value) in [("slice id", args.slice_id.as_str())] {
        if value.is_empty()
            || value.contains('/')
            || value.contains('\\')
            || value == "."
            || value == ".."
        {
            return Err(BuildError::Invalid(format!(
                "{what} '{value}' is not a safe path component"
            )));
        }
    }

    // A batch build always writes prefix `v00000`, so building into a directory that already
    // holds a bundle would leave that bundle's files half-overwritten while its `CURRENT` still
    // points at them — and a stale higher-numbered `SEGMENTS-<n>.json` left behind would be the
    // one the reader picks. Refuse rather than produce that state (fail closed).
    if args.out.join("CURRENT").exists() {
        return Err(BuildError::Invalid(format!(
            "{} already contains a bundle (CURRENT exists); remove it or choose another --out",
            args.out.display()
        )));
    }

    // ---- 1. read inputs --------------------------------------------------------------
    let mut points = input::read_points(&args.points, args.limit)?;
    if points.is_empty() {
        return Err(BuildError::Invalid(
            "no points selected — a bundle with no items has no expressible entity range".into(),
        ));
    }
    // Sort by source ID before anything else: term IDs are assigned in first-appearance order,
    // so a stable, file-order-independent iteration is what makes the dictionary (and hence the
    // signature ordering, and hence the permanent entity IDs) reproducible from the same input
    // regardless of how the source file happens to be laid out.
    points.sort_unstable_by_key(|p| p.source_id);
    if points.windows(2).any(|w| w[0].source_id == w[1].source_id) {
        return Err(BuildError::Invalid(
            "points file contains duplicate entity_id values".into(),
        ));
    }
    let mut pairs_by_source = input::read_pairs(&args.pairs, args.limit)?;

    // ---- 2. label each item through the plugin, interning descriptors ----------------
    let plugin = Passthrough::new();
    let bounds = plugin.declared_bounds();
    let dict_dir = args.out.join(PREFIX).join("dictionary");
    fs::create_dir_all(&dict_dir).map_err(|e| BuildError::io(&dict_dir, e))?;
    let mut dict = DictWriter::new(&dict_dir);

    let mut staged: Vec<StagedItem> = Vec::with_capacity(points.len());
    let mut over_bound_items = 0u64;
    for point in &points {
        let source_terms = pairs_by_source.remove(&point.source_id).unwrap_or_default();
        // The Phase 0 corpus carries integer term IDs; the item's `access` label is the
        // comma-joined decimal source term IDs, so `builtin:passthrough` yields decimal-string
        // descriptors (R6).
        let mut access = String::new();
        for (i, t) in source_terms.iter().enumerate() {
            if i > 0 {
                access.push(',');
            }
            access.push_str(&t.to_string());
        }
        let descriptors = plugin.terms_of_label(access.as_bytes())?;
        if descriptors.len() > bounds.max_terms_per_item as usize {
            // A declared bound is a *declaration*: record it and carry on. Dropping terms here
            // would silently widen the item's visibility (I2/I3).
            over_bound_items += 1;
        }
        let terms: Vec<TermId> = descriptors.iter().map(|d| dict.intern(d)).collect();
        let signature = signature_sort_key(&terms);
        staged.push(StagedItem {
            source_id: point.source_id,
            x: point.x,
            y: point.y,
            terms,
            signature,
        });
    }
    if !pairs_by_source.is_empty() {
        return Err(BuildError::Invalid(format!(
            "pairs file references {} entity ids absent from the points file (first: {})",
            pairs_by_source.len(),
            pairs_by_source.keys().min().copied().unwrap_or_default()
        )));
    }
    if over_bound_items > 0 {
        eprintln!(
            "warning: {over_bound_items} item(s) exceed the plugin's declared \
             max_terms_per_item ({}); no term was dropped",
            bounds.max_terms_per_item
        );
    }

    // ---- 3. signature-sorted entity-ID assignment (I9, permanent — see module docs) ---
    staged.sort_by(|a, b| {
        a.signature
            .cmp(&b.signature)
            .then(a.source_id.cmp(&b.source_id))
    });
    let n = staged.len() as u64;
    if n > u32::MAX as u64 {
        return Err(BuildError::Invalid(format!(
            "{n} items exceeds bundle_format 1's 2^32 entity-ID ceiling"
        )));
    }

    let term_count = staged
        .iter()
        .flat_map(|s| s.terms.iter())
        .map(|t| t.raw())
        .max()
        .map(|m| m as u64 + 1)
        .unwrap_or(0);

    // ---- 4/5. postings, pairs, external ids ------------------------------------------
    // Built by walking items in new-entity-ID order, so every per-term list comes out sorted
    // strictly ascending without a further sort — which is exactly what `write_postings`
    // requires (it rejects unsorted input rather than silently repairing it).
    let mut per_term: Vec<Vec<u32>> = vec![Vec::new(); term_count as usize];
    let mut pair_count = 0u64;
    for (position, item) in staged.iter().enumerate() {
        let new_id = position as u32;
        for term in signature_sort_key(&item.terms) {
            per_term[term as usize].push(new_id);
            pair_count += 1;
        }
    }

    let partition_dir = args.out.join(PREFIX).join("partitions").join(PHASH);
    let terms_dir = partition_dir.join("terms");
    let entities_dir = partition_dir.join("entities");
    let slice_dir = partition_dir.join("slices").join(&args.slice_id);
    let segment_dir = slice_dir.join("segments").join(SEG_ID);
    for dir in [&terms_dir, &entities_dir, &slice_dir, &segment_dir] {
        fs::create_dir_all(dir).map_err(|e| BuildError::io(dir, e))?;
    }

    let dict_paths = dict.finish().map_err(|e| BuildError::io(&dict_dir, e))?;
    let dict_records = term_count;
    for path in &dict_paths {
        fsync_file(path)?;
    }

    let postings_path = terms_dir.join("postings.arrow");
    write_postings(&postings_path, &per_term, SMALL_TERM_THRESHOLD_DEFAULT)
        .map_err(|e| BuildError::io(&postings_path, e))?;
    fsync_file(&postings_path)?;

    let pairs_path = terms_dir.join("pairs.parquet");
    write_pairs_parquet(&pairs_path, &per_term)?;

    let external_ids_path = entities_dir.join("external-ids-0.arrow");
    write_external_ids(&external_ids_path, &staged)?;

    // ---- 6. tiler and segment --------------------------------------------------------
    let mut tiler_items: Vec<TilerItem> = staged
        .iter()
        .enumerate()
        .map(|(position, item)| {
            let entity_id = EntityId::new(position as u64);
            TilerItem {
                entity_id,
                x: item.x,
                y: item.y,
                node_id: NODE_NONE,
                priority: priority_of(entity_id),
                scalars: Vec::new(),
            }
        })
        .collect();
    let codes = sort_batch(&mut tiler_items, &args.extent);
    write_segment(&segment_dir, &tiler_items, &codes, &[])
        .map_err(|e| BuildError::io(&segment_dir, e))?;
    fsync_file(&segment_dir.join("columns.arrow"))?;
    fsync_file(&segment_dir.join("morton.u64"))?;

    let permutation_path = slice_dir.join("permutation.bin");
    let row_order: Vec<EntityId> = tiler_items.iter().map(|i| i.entity_id).collect();
    // `bound` is the partition slice's max entity ID + 1. The bootstrap build allocates a dense
    // 0..n, so that is exactly the item count.
    write_permutation(&permutation_path, &row_order, n)
        .map_err(|e| BuildError::io(&permutation_path, e))?;
    fsync_file(&permutation_path)?;

    // ---- 7. manifests ----------------------------------------------------------------
    let prefix_dir = args.out.join(PREFIX);
    let mut manifest_files: BTreeMap<String, FileDigest> = BTreeMap::new();
    let mut dict_extents = Vec::new();
    for path in &dict_paths {
        let rel = relative_to(&prefix_dir, path)?;
        manifest_files.insert(rel.clone(), digest_file(path)?);
        dict_extents.push(DictExtent {
            path: rel,
            records: dict_records,
        });
    }

    let mut segment_files: BTreeMap<String, FileDigest> = BTreeMap::new();
    for path in [
        &postings_path,
        &pairs_path,
        &external_ids_path,
        &permutation_path,
        &segment_dir.join("columns.arrow"),
        &segment_dir.join("morton.u64"),
    ] {
        segment_files.insert(relative_to(&prefix_dir, path)?, digest_file(path)?);
    }

    let bundle_bytes: u64 = manifest_files
        .values()
        .chain(segment_files.values())
        .map(|f| f.size)
        .sum();

    let segments = SegmentsManifest {
        segments_version: 0,
        watermark: n,
        entity_id_high_water: n,
        segments: vec![SegmentDescriptor {
            slice: args.slice_id.clone(),
            seg_id: SEG_ID.to_string(),
            row_count: n as u32,
            entity_lo: 0,
            // An empty build has no entity range at all; `entity_hi` is inclusive, so saturate
            // rather than underflow.
            entity_hi: n.saturating_sub(1),
        }],
        deltas: Vec::new(),
        dict_extents,
        external_id_extents: vec![relative_to(&prefix_dir, &external_ids_path)?],
        tombstones: Vec::new(),
        deny: Vec::new(),
        files: segment_files,
    };
    let segments_path = partition_dir.join("SEGMENTS-0.json");
    write_json(&segments_path, &segments)?;

    let manifest = Manifest {
        bundle_format: BUNDLE_FORMAT,
        created_at: chrono::Utc::now().to_rfc3339(),
        data_plugin_hash: plugin.data_plugin_hash(),
        declared_bounds: serde_json::json!({
            "max_distinct_terms": bounds.max_distinct_terms,
            "max_terms_per_item": bounds.max_terms_per_item,
            "max_terms_per_token": bounds.max_terms_per_token,
        }),
        declared_scalars: Vec::new(),
        small_term_threshold: SMALL_TERM_THRESHOLD_DEFAULT,
        quantisation: Quantisation {
            x_min: args.extent.x_min,
            x_max: args.extent.x_max,
            y_min: args.extent.y_min,
            y_max: args.extent.y_max,
        },
        entity_id_high_water: n,
        slices: vec![SliceDescriptor {
            id: args.slice_id.clone(),
            display_name: args.slice_id.clone(),
        }],
        partitions: vec![PartitionDescriptor {
            phash: PHASH.to_string(),
            required_terms: Vec::new(),
        }],
        provenance: serde_json::json!({ "generating_set_choice": "prompt-sample" }),
        files: manifest_files,
    };
    let manifest_path = prefix_dir.join("MANIFEST.json");
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| BuildError::Invalid(format!("serialising MANIFEST.json: {e}")))?;
    write_bytes(&manifest_path, &manifest_bytes)?;

    // Everything the bundle names is now durable; `CURRENT` is written last and by rename, so
    // a reader either sees the previous bundle or this complete one, never a half-built prefix.
    let current = CurrentPointer {
        prefix: PREFIX.to_string(),
        manifest_digest: hex_sha256(&manifest_bytes),
    };
    let current_bytes = serde_json::to_vec_pretty(&current)
        .map_err(|e| BuildError::Invalid(format!("serialising CURRENT: {e}")))?;
    let current_tmp = args.out.join("CURRENT.tmp");
    write_bytes(&current_tmp, &current_bytes)?;
    let current_path = args.out.join("CURRENT");
    fs::rename(&current_tmp, &current_path).map_err(|e| BuildError::io(&current_path, e))?;
    fsync_dir(&args.out)?;

    Ok(BuildReport {
        prefix: PREFIX.to_string(),
        slice_id: args.slice_id.clone(),
        seg_id: SEG_ID.to_string(),
        items: n,
        terms: term_count,
        pairs: pair_count,
        bundle_bytes,
    })
}

/// What `tessera verify` checked.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub prefix: String,
    pub partitions: usize,
    pub slices: usize,
    pub segments: usize,
    pub rows: u64,
    pub entity_id_high_water: u64,
}

/// Verify a bundle at `root`: run the read protocol (which checks every manifest digest, every
/// file's size and SHA-256, and each permutation's bijectivity onto its segment's rows), then
/// re-confirm the permutation covers exactly the rows the segment claims.
pub fn verify(root: &Path) -> Result<VerifyReport> {
    let bundle = tessera_store::read::open_bundle(root)?;
    let mut slices = 0usize;
    let mut segments = 0usize;
    let mut rows = 0u64;
    for partition in bundle.partitions.values() {
        for (slice_id, slice) in &partition.slices {
            slices += 1;
            let row_count: u32 = slice.segments.iter().map(|s| s.row_count).sum();
            segments += slice.segments.len();
            rows += row_count as u64;
            // `open_bundle` already ran `validate_rows` (no aliasing, no out-of-range row).
            // The remaining half of bijectivity is surjectivity: every row must be claimed by
            // some entity, or `columns.arrow` holds a row no entity can ever address.
            slice.permutation.validate_rows(row_count)?;
            let mut claimed = 0u64;
            for entity in 0..slice.permutation.bound() {
                if slice
                    .permutation
                    .row_of(tessera_types::EntityId::new(entity))
                    .is_some()
                {
                    claimed += 1;
                }
            }
            if claimed != row_count as u64 {
                return Err(BuildError::Invalid(format!(
                    "slice '{slice_id}': permutation claims {claimed} rows but the segments hold \
                     {row_count} — not a bijection"
                )));
            }
        }
    }
    let current: CurrentPointer = {
        let bytes = fs::read(root.join("CURRENT")).map_err(|e| BuildError::io(root, e))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| BuildError::Invalid(format!("CURRENT is not valid JSON: {e}")))?
    };
    Ok(VerifyReport {
        prefix: current.prefix,
        partitions: bundle.partitions.len(),
        slices,
        segments,
        rows,
        entity_id_high_water: bundle.manifest.entity_id_high_water,
    })
}

/// Write `pairs.parquet` (R4): `(entity_id: uint64, term_id: uint32)` sorted by
/// `(term_id, entity_id)`, DELTA_BINARY_PACKED on both columns.
///
/// `per_term[t]` is already ascending, and terms are emitted in ordinal order, so the required
/// sort is the iteration order — no sort step is needed or performed. The file is off both
/// request paths (build-cadence and oracle reads only), so the encoding is chosen for the
/// oracle's benefit, not for query latency.
fn write_pairs_parquet(path: &Path, per_term: &[Vec<u32>]) -> Result<()> {
    let schema = std::sync::Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let props = WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_2_0)
        .set_encoding(Encoding::DELTA_BINARY_PACKED)
        .set_dictionary_enabled(false)
        .set_statistics_enabled(EnabledStatistics::Chunk)
        .set_compression(Compression::SNAPPY)
        .build();
    let file = File::create(path).map_err(|e| BuildError::io(path, e))?;
    let mut writer = ArrowWriter::try_new(file, schema.clone(), Some(props))
        .map_err(|e| BuildError::parquet(path, e))?;

    const BATCH: usize = 1 << 16;
    let mut entities: Vec<u64> = Vec::with_capacity(BATCH);
    let mut terms: Vec<u32> = Vec::with_capacity(BATCH);
    let flush = |entities: &mut Vec<u64>, terms: &mut Vec<u32>, w: &mut ArrowWriter<File>| {
        if entities.is_empty() {
            return Ok(());
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                std::sync::Arc::new(UInt64Array::from(std::mem::take(entities))) as ArrayRef,
                std::sync::Arc::new(UInt32Array::from(std::mem::take(terms))) as ArrayRef,
            ],
        )
        .map_err(|e| BuildError::arrow(path, e))?;
        w.write(&batch).map_err(|e| BuildError::parquet(path, e))
    };

    for (t, entity_ids) in per_term.iter().enumerate() {
        for &entity in entity_ids {
            entities.push(entity as u64);
            terms.push(t as u32);
            if entities.len() == BATCH {
                flush(&mut entities, &mut terms, &mut writer)?;
            }
        }
    }
    flush(&mut entities, &mut terms, &mut writer)?;
    writer.close().map_err(|e| BuildError::parquet(path, e))?;
    fsync_file(path)
}

/// Write `external-ids-0.arrow` (R4): `(external_id: binary, entity_id: uint64)` sorted by the
/// external ID's **bytes**. The external ID here is the source corpus's entity ID as 8 bytes
/// little-endian; byte order is not numeric order, so the sort is over the encoded keys.
fn write_external_ids(path: &Path, staged: &[StagedItem]) -> Result<()> {
    let mut rows: Vec<([u8; 8], u64)> = staged
        .iter()
        .enumerate()
        .map(|(position, item)| (item.source_id.to_le_bytes(), position as u64))
        .collect();
    rows.sort_unstable_by_key(|(key, _)| *key);

    let schema = std::sync::Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new("entity_id", DataType::UInt64, false),
    ]));
    let external: ArrayRef = std::sync::Arc::new(BinaryArray::from_iter_values(
        rows.iter().map(|(key, _)| key.as_slice()),
    ));
    let entity: ArrayRef = std::sync::Arc::new(UInt64Array::from_iter_values(
        rows.iter().map(|(_, id)| *id),
    ));
    let batch = RecordBatch::try_new(schema.clone(), vec![external, entity])
        .map_err(|e| BuildError::arrow(path, e))?;

    let file = File::create(path).map_err(|e| BuildError::io(path, e))?;
    let mut writer =
        ArrowFileWriter::try_new(file, &schema).map_err(|e| BuildError::arrow(path, e))?;
    writer
        .write(&batch)
        .map_err(|e| BuildError::arrow(path, e))?;
    writer.finish().map_err(|e| BuildError::arrow(path, e))?;
    fsync_file(path)
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| BuildError::Invalid(format!("serialising {}: {e}", path.display())))?;
    write_bytes(path, &bytes)
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(path).map_err(|e| BuildError::io(path, e))?;
    file.write_all(bytes).map_err(|e| BuildError::io(path, e))?;
    file.sync_all().map_err(|e| BuildError::io(path, e))?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

fn fsync_file(path: &Path) -> Result<()> {
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    file.sync_all().map_err(|e| BuildError::io(path, e))?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

fn fsync_dir(path: &Path) -> Result<()> {
    let dir = File::open(path).map_err(|e| BuildError::io(path, e))?;
    dir.sync_all().map_err(|e| BuildError::io(path, e))
}

fn digest_file(path: &Path) -> Result<FileDigest> {
    let bytes = fs::read(path).map_err(|e| BuildError::io(path, e))?;
    Ok(FileDigest {
        size: bytes.len() as u64,
        sha256: hex_sha256(&bytes),
    })
}

fn hex_sha256(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Manifest keys are prefix-relative with forward slashes (R1), on every platform.
fn relative_to(prefix_dir: &Path, path: &Path) -> Result<String> {
    let rel = path.strip_prefix(prefix_dir).map_err(|_| {
        BuildError::Invalid(format!(
            "{} is not inside the bundle prefix {}",
            path.display(),
            prefix_dir.display()
        ))
    })?;
    Ok(rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_matches_r3_splitmix64() {
        // Independently computed reference values for the R3 construction.
        fn reference(e: u64) -> u16 {
            let mut z = e.wrapping_add(0x9E37_79B9_7F4A_7C15);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            (z >> 48) as u16
        }
        for e in [0u64, 1, 2, 42, 250_000, u32::MAX as u64] {
            assert_eq!(priority_of(EntityId::new(e)), reference(e));
        }
    }

    #[test]
    fn signature_key_is_sorted_deduplicated_and_order_independent() {
        let a = signature_sort_key(&[TermId::new(5), TermId::new(1), TermId::new(5)]);
        assert_eq!(a, vec![1, 5]);
        assert_eq!(signature_sort_key(&[TermId::new(1), TermId::new(5)]), a);
    }

    #[test]
    fn relative_paths_use_forward_slashes() {
        let prefix = Path::new("/bundle/v00000");
        let path = prefix
            .join("partitions")
            .join("default")
            .join("SEGMENTS-0.json");
        assert_eq!(
            relative_to(prefix, &path).unwrap(),
            "partitions/default/SEGMENTS-0.json"
        );
    }
}
