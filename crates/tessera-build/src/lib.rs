//! `tessera build` — the batch build.
//!
//! Composes the tiler, the dictionary, the postings writer and the segment writers into one
//! verifiable bundle whose layout is contracts §2.1. Two builds live here and must agree
//! byte-for-byte: [`build`], the streaming one that ships (see [`mod@pipeline`]), and
//! [`build_in_memory`], the linear one retained as its oracle.
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
//! permutation and handle ever issued. The measured posting compression from this ordering is
//! 8.9–36.7x (`probes/results.md`) — the reason it must be in the first build rather than an
//! optimisation added later. The rule lives in [`signature_sort_key`] as a free function so the
//! serving allocator applies exactly the same rule to appended items.

pub mod error;
pub mod input;
pub mod observer;
mod pipeline;
pub mod schema;
pub(crate) mod spill;

use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use arrow::array::{ArrayRef, BinaryArray, UInt32Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::FileWriter as ArrowFileWriter;
use arrow::record_batch::RecordBatch;
use sha2::{Digest, Sha256};

use tessera_authz::{write_postings, DictWriter};
use tessera_plugin::{Passthrough, Plugin};
use tessera_spatial::tiler::{sort_batch, ScalarValue, TilerItem};
use tessera_spatial::Bounds;
use tessera_store::manifest::{
    identity_key_fingerprint, CurrentPointer, DeclaredScalar, DictExtent, FileDigest,
    IdentityDescriptor, Manifest, ManifestVocabulary, ManifestVocabularyValue, PartitionDescriptor,
    Quantisation, SegmentDescriptor, SegmentsManifest, SliceDescriptor,
};
use tessera_store::write::{write_permutation, write_segment};
use tessera_store::{write_current, write_manifest_json, PairsParquetWriter};
use tessera_types::{
    EntityId, IdentityKey, TermId, BUNDLE_FORMAT, IDENTITY_CONSTRUCTION, IDENTITY_ROUNDS,
    SMALL_TERM_THRESHOLD_DEFAULT,
};

pub use error::{BuildError, Result};
pub use observer::{BuildObserver, BuildStage, NoopObserver};

/// The single bundle prefix a batch build writes. Later publications get their own prefix; the
/// batch build always starts a bundle from scratch.
const PREFIX: &str = "v00000";
/// This build writes exactly one partition: there are no compartments.
const PHASH: &str = "default";
/// One segment per (partition, slice) at build (contracts §2.1).
const SEG_ID: &str = "seg-0";

/// Arguments to [`build`].
#[derive(Clone)]
pub struct BuildArgs {
    /// Parquet file of points: `entity_id` plus either `x`/`y` or `morton` (see [`input`]).
    pub points: PathBuf,
    /// Parquet file of the exploded `(entity_id, term_id)` relation.
    pub pairs: PathBuf,
    /// Bundle root to create.
    pub out: PathBuf,
    /// The quantisation extent Morton codes are computed against (contracts §2.5).
    pub extent: Bounds,
    /// The slice this build's segment belongs to.
    pub slice_id: String,
    /// Prefix filter on the *source* entity ID: keep rows with `entity_id < limit`.
    pub limit: Option<u64>,
    /// The deployment's identity key (contracts §2.2). **Not** per bundle: it must be carried
    /// across rebuilds or every `tessera_id` any client holds silently breaks. Resolved by the
    /// CLI from `--carry-id-key-from` / `--id-key-file` / `--id-key` / `--mint-id-key`, and
    /// passed here already decided so that both build paths see the same bytes.
    pub identity_key: IdentityKey,
    /// `identity_key`'s canonical 32-lowercase-hex-character form, exactly as MANIFEST records
    /// it. Carried alongside the parsed key rather than recovered from it: `IdentityKey`
    /// deliberately has no hex accessor, to preserve its redacted `Debug` (a hex accessor would
    /// undo the redaction).
    pub identity_key_hex: String,
    /// MANIFEST `identity.idset` (contracts §2.2/§2a): advanced by the CLI when the operator
    /// passes `--bump-idset` or rotates the key, carried forward verbatim on a normal
    /// rebuild, reset to 1 by `--mint-id-key`.
    pub idset: u32,
    /// The §13.3 row-range shard this build produces. Always 0: there is no sharding.
    pub shard_id: u32,
    /// Mint an external ID for every item from its source entity id (8 bytes LE), and write
    /// the external-id extents and `ext-locator.u32`.
    ///
    /// **Off by default, deliberately** (2026-07-30 memo §3.2 D1; CLI `--mint-external-ids`):
    /// contracts §2.4 forbids manufacturing an external ID for an item whose caller supplied
    /// none, and the probe corpus supplies none — so the conformant default build writes no
    /// sidecar at all (the reader is built for that: no extents, no locator, every resolve is
    /// `None`). Bench fixtures pass the flag so they keep carrying the family's cost
    /// realistically, per the owner ruling that made it a representative cost rather than a
    /// reduction target.
    pub mint_external_ids: bool,
    /// Write `pairs.parquet` (contracts §2.4). On by default; `--no-oracle-pairs` clears it.
    ///
    /// The file is read by nothing on any request path — its consumers are the test-only
    /// Python reference oracle and build-cadence tooling — so a deployment that runs no
    /// conformance suite against the bundle can skip writing and hashing it (~5–7 GB at 10⁹).
    /// A bundle without it is still verifiable: MANIFEST lists only what was written.
    pub emit_oracle_pairs: bool,
    /// Signature-sort batch size, in items (§11.1 r23: assignment is signature-sorted **within
    /// each append-only batch and only within one**; the fragmentation is monotone in batch
    /// count and permanent under I9).
    ///
    /// `None` = derive: the largest batch the memory budget supports, rounded down to a
    /// multiple of 2²⁴ items so budget jitter between machines does not gratuitously fork
    /// identities — usually the whole corpus in one batch, which reproduces the pre-batching
    /// output byte for byte. Whatever is *used* (derived or explicit, when it batches at all)
    /// is recorded in MANIFEST provenance, and an identity-preserving rebuild must replay it:
    /// a different batch size is a different permanent assignment, i.e. a different corpus.
    pub batch_items: Option<u64>,
    /// Peak-RSS budget in bytes for the build's own structures. `None` = detect from the
    /// machine (MemAvailable, damped). Drives batch and band sizing and the fail-closed
    /// pre-flight; it cannot buy off the irreducible floors (the sorted source ids, the
    /// entity-of-ordinal map, the per-term offsets), which the pre-flight states when refusing.
    pub memory_budget: Option<u64>,
    /// Override the derived postings band size, in pre-dedup rows. A tuning and **test** seam
    /// (a corpus small enough for a test cannot force multiple bands through the budget
    /// alone); band boundaries never affect output bytes, only transient memory. `None`
    /// derives from the budget.
    pub band_rows: Option<u64>,
    /// The compiled `schema.toml`: the per-item columns this build writes into `columns.arrow`'s
    /// tail, in declared order (`--schema`, bound values via `--values`).
    ///
    /// **Default-empty, and that case must stay byte-identical.** Every bundle built before
    /// `--schema` existed declared no scalar, and an empty schema must go on producing exactly the
    /// bytes it did — `tessera-cli`'s identity test asserts a byte-identical `columns.arrow`
    /// across rebuilds carrying one key, and a schema that widened the fixed table by default
    /// would break it for reasons unrelated to identity.
    pub schema: crate::schema::Schema,
}

/// **Hand-written, not derived: `identity_key_hex` is the deployment key in plaintext.**
/// `IdentityKey`'s `Debug` is redacted and it has no hex accessor, but a derived `Debug` here
/// would print the hex carried beside it — so one `tracing::error!("{args:?}")` on a build
/// failure would put the deployment key in a log. The redaction is only worth as much as its
/// weakest carrier.
impl std::fmt::Debug for BuildArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildArgs")
            .field("points", &self.points)
            .field("pairs", &self.pairs)
            .field("out", &self.out)
            .field("extent", &self.extent)
            .field("slice_id", &self.slice_id)
            .field("limit", &self.limit)
            .field("identity_key", &self.identity_key)
            .field(
                "identity_key_hex",
                &identity_key_fingerprint(&self.identity_key_hex),
            )
            .field("idset", &self.idset)
            .field("shard_id", &self.shard_id)
            .field("mint_external_ids", &self.mint_external_ids)
            .field("emit_oracle_pairs", &self.emit_oracle_pairs)
            .field("batch_items", &self.batch_items)
            .field("memory_budget", &self.memory_budget)
            .field("band_rows", &self.band_rows)
            .finish()
    }
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
/// cannot be changed after the first build. The serving allocator calls this same function.
pub fn signature_sort_key(terms: &[TermId]) -> Vec<u32> {
    let mut key: Vec<u32> = terms.iter().map(|t| t.raw()).collect();
    key.sort_unstable();
    key.dedup();
    key
}

/// Argument and destination checks shared by both build implementations.
fn validate_args(args: &BuildArgs) -> Result<()> {
    args.extent
        .validate()
        .map_err(|detail| BuildError::Invalid(format!("extent: {detail}")))?;
    if args.batch_items == Some(0) {
        return Err(BuildError::Invalid(
            "--batch-items 0 is meaningless; omit it for a single batch".into(),
        ));
    }
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
    Ok(())
}

/// One item after labelling, before entity-ID assignment.
///
/// The item's term set is held only as its `signature` — [`signature_sort_key`]'s sorted,
/// deduplicated term-ID list. That is both the ordering key and the postings input, so keeping a
/// second, unsorted copy alongside it would only create a way for the two to disagree.
struct StagedItem {
    source_id: u64,
    /// 32-bit fixed point per axis against the build extent, as `input::PointRow` carries it —
    /// not coordinates. Same width as the `f32` pair it replaces.
    qx: u32,
    qy: u32,
    signature: Vec<u32>,
}

/// Run the batch build, producing a complete bundle at `args.out`.
///
/// This is [`pipeline::build`] — the streaming pipeline, which holds a bounded set of packed
/// arrays rather than one struct per item. [`build_in_memory`] is the older, linear
/// implementation, kept as the byte-equality oracle the two are tested against.
pub fn build(args: &BuildArgs) -> Result<BuildReport> {
    pipeline::build(args, &observer::NoopObserver)
}

/// [`build`], reporting each pipeline stage's duration to `observer` as it completes.
///
/// Identical to `build` in every respect but the notifications — the same code path, not a
/// parallel one, so a measurement taken here describes the build that actually ships. Exists for
/// `tessera-bench`'s ingest arm, which asks which of the eleven stages bends with scale.
pub fn build_observed(
    args: &BuildArgs,
    observer: &dyn observer::BuildObserver,
) -> Result<BuildReport> {
    pipeline::build(args, observer)
}

/// The linear, fully in-memory build.
///
/// Superseded by [`build`] for anything but small inputs — it materialises one [`StagedItem`]
/// per point and the whole `per_term` posting relation before writing a byte, which at 10⁹
/// items is tens of gigabytes. It is retained, and exercised by
/// `tests/build_equivalence.rs`, as the **oracle** for the streaming pipeline: the two must
/// produce byte-identical bundles for any input, because the entity-ID assignment they encode
/// is permanent (I9) and every digest in the bundle depends on it.
pub fn build_in_memory(args: &BuildArgs) -> Result<BuildReport> {
    validate_args(args)?;

    // ---- 1. read inputs --------------------------------------------------------------
    let mut points = input::read_points(&args.points, &args.extent, args.limit)?;
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
        // The probe corpus carries integer term IDs; the item's `access` label is the
        // comma-joined decimal source term IDs, so `builtin:passthrough` yields decimal-string
        // descriptors.
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
            //
            // This counts descriptors, and the streaming pipeline counts the item's signature
            // length; the two always agree. `read_pairs` returns each item's source terms sorted
            // and deduplicated, so the joined label has distinct decimal elements, passthrough
            // yields one distinct descriptor each, and interning is injective — the descriptor
            // count *is* the distinct term count, which is what a signature holds.
            over_bound_items += 1;
        }
        let terms: Vec<TermId> = descriptors.iter().map(|d| dict.intern(d)).collect();
        staged.push(StagedItem {
            source_id: point.source_id,
            qx: point.qx,
            qy: point.qy,
            signature: signature_sort_key(&terms),
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
    // §11.1 r23: the sort's scope is one batch. `staged` is in ascending source-id order
    // (the sort above), i.e. ordinal order, so a batch is a contiguous chunk; each chunk is
    // signature-sorted independently and the concatenation is the batch-major assignment.
    // `None` (or one covering chunk) reproduces the historical global sort exactly.
    let batch = args
        .batch_items
        .unwrap_or(u64::MAX)
        .min(staged.len().max(1) as u64) as usize;
    for chunk in staged.chunks_mut(batch) {
        chunk.sort_by(|a, b| {
            a.signature
                .cmp(&b.signature)
                .then(a.source_id.cmp(&b.source_id))
        });
    }
    let n = staged.len() as u64;
    if n > u32::MAX as u64 {
        return Err(BuildError::Invalid(format!(
            "{n} items exceeds bundle_format 1's 2^32 entity-ID ceiling"
        )));
    }

    // The dictionary is the authority on how many terms exist — `max(term_id) + 1` over the
    // items would agree only as long as every interned term is still carried by some item, and
    // a postings file shorter than the dictionary would silently make its tail terms unaskable.
    let term_count = dict.len() as u64;

    // ---- 4/5. postings, pairs, external ids ------------------------------------------
    // Built by walking items in new-entity-ID order, so every per-term list comes out sorted
    // strictly ascending without a further sort — which is exactly what `write_postings`
    // requires (it rejects unsorted input rather than silently repairing it).
    let mut per_term: Vec<Vec<u32>> = vec![Vec::new(); term_count as usize];
    let mut pair_count = 0u64;
    for (position, item) in staged.iter().enumerate() {
        let new_id = position as u32;
        // `signature` is already the sorted, deduplicated term-id list computed at staging.
        for &term in &item.signature {
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

    let mut other_paths: Vec<PathBuf> = vec![postings_path.clone()];
    if args.emit_oracle_pairs {
        let pairs_path = terms_dir.join("pairs.parquet");
        write_pairs_parquet(&pairs_path, &per_term)?;
        other_paths.push(pairs_path);
    }

    // Minting is opt-in (see `BuildArgs::mint_external_ids`): with it off, no extent and no
    // locator exist, which the reader treats as "no item has an external ID" — the ordinary
    // case, not a degraded one.
    let external_ids_paths = if args.mint_external_ids {
        let (extent_paths, ext_locator_path) = write_external_ids(&entities_dir, &staged, n)?;
        other_paths.push(ext_locator_path);
        extent_paths
    } else {
        Vec::new()
    };

    // ---- 6. the identity, computed BEFORE the tiler (2026-07-30 fold, memo §6) --------
    // `tessera_id` is now a sort key (`priority = high16(tessera_id)`, and the storage order is
    // `(morton, tessera_id)`), so it must exist before `sort_batch` runs, not be written at the
    // row after it.
    let mut entity_ids: Vec<EntityId> = (0..n).map(EntityId::new).collect();
    let mut tiler_items: Vec<TilerItem> = Vec::with_capacity(n as usize);
    for (position, item) in staged.iter().enumerate() {
        let entity_id = EntityId::new(position as u64);
        let tessera_id = args.identity_key.forward(args.shard_id, entity_id)?;
        tiler_items.push(TilerItem {
            tessera_id,
            qx: item.qx,
            qy: item.qy,
            scalars: Vec::new(),
        });
    }

    // ---- 6b. the declared attribute tail ---------------------------------------------
    // A second pass over the points file, joined to the staged items **by source id**, because
    // `scan_attributes` visits rows in file order and staging is in entity order. Skipped
    // entirely when the schema is empty, which is what keeps a schema-less build's `columns.arrow`
    // byte-identical to the one it wrote before this existed.
    //
    // Every staged item must receive a value. A row the attribute pass never visits would keep an
    // empty `scalars` vector, and the segment writer refuses that by name rather than padding it
    // — padding would put every later row's value under the wrong identity in a column whose
    // width says nothing is wrong.
    //
    // `minters` seeds one live `VocabularyMinter` per discovered vocabulary from whatever the
    // schema already pins, and the scan mints into it for every novel key the corpus supplies.
    // Its final state — carried past this block — is what `write_manifests` records into
    // `MANIFEST.vocabularies` below, so a rebuild and the serving path see exactly what this
    // build minted.
    let mut minters = args.schema.discovered_minters();
    if !args.schema.is_empty() {
        let position_of_source: HashMap<u64, usize> = staged
            .iter()
            .enumerate()
            .map(|(position, item)| (item.source_id, position))
            .collect();
        let mut seen = 0usize;
        input::scan_attributes(
            &args.points,
            &args.schema,
            &mut minters,
            args.limit,
            |source_id, values| {
                if let Some(&position) = position_of_source.get(&source_id) {
                    tiler_items[position].scalars = values.to_vec();
                    seen += 1;
                }
            },
        )?;
        if seen != staged.len() {
            return Err(BuildError::Invalid(format!(
                "the attribute pass matched {seen} of {} staged items. The points file's two \
                 passes disagree about which entities it holds, so some row would be written \
                 with another row's attribute values",
                staged.len()
            )));
        }
    }

    // ---- 6c. attribute filter postings (filter-index §4) -------------------------------
    // Before `sort_batch`, which permutes `tiler_items` into row order: entity id is a staged
    // item's *position*, so the values are entity-major exactly here and nowhere after.
    // Transposed into one vector per column because that is the shape the emit consumes — the
    // streaming pipeline reads its attributes column-major already, and one of the two builds
    // paying a transpose is better than two emit paths that could disagree about a record's
    // contents (which `write_manifests` exists to prevent for the same reason).
    let filter_paths = {
        let by_entity: Vec<Vec<ScalarValue>> = (0..args.schema.attributes.len())
            .map(|column| {
                tiler_items
                    .iter()
                    .map(|item| item.scalars[column].clone())
                    .collect()
            })
            .collect();
        // The record blob beside the postings, from the same entity-major values — the two
        // builds must stay byte-identical, so this path writes every artefact the streaming
        // pipeline writes.
        let mut paths = pipeline::write_filter_postings(&partition_dir, &args.schema, &by_entity)?;
        paths.extend(pipeline::write_record_blob(
            &partition_dir,
            &args.schema,
            &by_entity,
        )?);
        paths
    };

    // ---- 7. tiler and segment ---------------------------------------------------------
    // Narrow each item's scalars to the render columns, in declaration order, so they align with
    // `scalar_schema_of`'s filtered list. Done after the filter emit above, which needs every
    // declared column including the `index`-only ones.
    //
    // **Unconditional, where it used to be skipped when every column rendered.**
    {
        let render: Vec<bool> = args.schema.attributes.iter().map(|a| a.render).collect();
        for item in &mut tiler_items {
            let mut kept = Vec::with_capacity(render.iter().filter(|k| **k).count());
            for (i, v) in item.scalars.iter().enumerate() {
                if render[i] {
                    kept.push(v.clone());
                }
            }
            item.scalars = kept;
        }
    }
    let scalar_schema = scalar_schema_of(&args.schema);
    let codes = sort_batch(&mut tiler_items, &mut entity_ids);

    // The render columns' presence, after the sort because the bitmap is over **rows**, and before
    // the substitution below because that is what erases the distinction: `columns.arrow` is
    // non-nullable (contracts R4), so an absent value is written as the type's zero and this is
    // what says that zero means nothing (decision 0064).
    let mut presence_paths: Vec<PathBuf> = Vec::new();
    for (column, (name, _)) in scalar_schema.iter().enumerate() {
        let rows = pipeline::render_presence_of(tiler_items.iter().map(|i| &i.scalars[column]));
        let Some(rows) = rows else { continue };
        if let Some(path) =
            tessera_store::flush::write_render_presence(&segment_dir, name, rows, n as u32)
                .map_err(|e| BuildError::Invalid(format!("attribute '{name}': {e}")))?
        {
            presence_paths.push(path);
        }
    }
    // A `ScalarValue::Null` reaching the segment writer is a typed error rather than a drawn
    // point, so the placeholder goes in last — see `ScalarValue::or_render_placeholder`.
    for item in &mut tiler_items {
        for (value, (_, ty)) in item.scalars.iter_mut().zip(&scalar_schema) {
            if matches!(value, ScalarValue::Null) {
                *value = value.or_render_placeholder(*ty);
            }
        }
    }

    write_segment(&segment_dir, &tiler_items, &codes, &scalar_schema)
        .map_err(|e| BuildError::io(&segment_dir, e))?;
    fsync_file(&segment_dir.join("columns.arrow"))?;
    fsync_file(&segment_dir.join("morton.u32"))?;

    let permutation_path = slice_dir.join("permutation.bin");
    let row_order: Vec<EntityId> = entity_ids;
    // `bound` is the partition slice's max entity ID + 1. The bootstrap build allocates a dense
    // 0..n, so that is exactly the item count.
    write_permutation(&permutation_path, &row_order, n)
        .map_err(|e| BuildError::io(&permutation_path, e))?;
    fsync_file(&permutation_path)?;

    // The other direction, for the filtered viewport's per-tile route
    // (`tessera_store::row_entity`). `row_order` is already the row→entity vector, so this writes
    // what the permutation was just scattered from rather than deriving anything.
    let row_entity_path = slice_dir.join(tessera_store::ROW_ENTITY_FILE);
    let rows_by_index: Vec<u32> = row_order.iter().map(|e| e.raw() as u32).collect();
    tessera_store::write_row_entity(&row_entity_path, &rows_by_index)
        .map_err(|e| BuildError::io(&row_entity_path, e))?;
    fsync_file(&row_entity_path)?;

    // ---- 8. manifests ------------------------------------------------------------------
    other_paths.extend([
        permutation_path,
        row_entity_path,
        segment_dir.join("columns.arrow"),
        segment_dir.join("morton.u32"),
    ]);
    other_paths.extend(presence_paths);
    other_paths.extend(filter_paths);
    write_manifests(
        args,
        &BundleFiles {
            dict_paths,
            dict_records,
            external_ids_paths,
            other_paths,
        },
        &plugin,
        n,
        term_count,
        pair_count,
        args.batch_items.filter(|&b| b < n),
        &minters,
    )
}

/// The schema as the segment writer wants it: `(name, type)` in declared order.
///
/// One derivation, shared by both build implementations, so the two cannot come to disagree about
/// a column's width — which would produce two bundles the byte-equality oracle calls different
/// for a reason that is not the entity assignment it exists to check.
fn scalar_schema_of(
    schema: &crate::schema::Schema,
) -> Vec<(String, tessera_spatial::tiler::ScalarType)> {
    // Render columns only — the segment's tail and `permute_attribute_tail`'s output must name the
    // same columns in the same order, or every row's values land under the wrong headings.
    schema
        .attributes
        .iter()
        .filter(|a| a.render)
        .map(|a| (a.name.clone(), a.ty))
        .collect()
}

/// Every file a build wrote, split by the role it plays in the manifests.
struct BundleFiles {
    dict_paths: Vec<PathBuf>,
    dict_records: u64,
    external_ids_paths: Vec<PathBuf>,
    other_paths: Vec<PathBuf>,
}

/// Write `SEGMENTS-0.json`, `MANIFEST.json` and `CURRENT` over the files a build produced.
/// Shared by both build implementations so the two cannot drift in the one place where a
/// difference would be invisible until a digest failed.
#[allow(clippy::too_many_arguments)]
fn write_manifests(
    args: &BuildArgs,
    files: &BundleFiles,
    plugin: &Passthrough,
    n: u64,
    term_count: u64,
    pair_count: u64,
    batch_items_recorded: Option<u64>,
    minters: &HashMap<String, tessera_store::vocabulary::VocabularyMinter>,
) -> Result<BuildReport> {
    let bounds = plugin.declared_bounds();
    let partition_dir = args.out.join(PREFIX).join("partitions").join(PHASH);
    // Contracts §2.2 / §2.3 divide the two `files` maps by *when* a file appeared:
    // `MANIFEST.files` covers every file present at build time, and `SEGMENTS-<n>.files` covers
    // only what has been added *since* that manifest was written (streamed segments, later
    // deltas). A batch build produces everything at build time, so every file it writes belongs
    // in `MANIFEST.files` and `SEGMENTS-0.json`'s map is legitimately empty. (`open_bundle`
    // accepts a file verified via either map, so both splits load — but the spec's wording is
    // what the Python oracle and the conformance byte-scanner will be written against.)
    let prefix_dir = args.out.join(PREFIX);
    let mut dict_extents = Vec::new();
    for path in &files.dict_paths {
        dict_extents.push(DictExtent {
            path: relative_to(&prefix_dir, path)?,
            records: files.dict_records,
        });
    }
    let mut external_id_runs = Vec::with_capacity(files.external_ids_paths.len());
    for path in &files.external_ids_paths {
        external_id_runs.push(relative_to(&prefix_dir, path)?);
    }
    // Digest in parallel, one worker per file: SHA-256 is inherently sequential per file, but
    // the files are independent, and at 10⁹ this stage re-reads ~47 GB. The map is assembled
    // from (name, digest) pairs afterwards, so the manifest bytes cannot depend on scheduling.
    let all_paths: Vec<&PathBuf> = files
        .dict_paths
        .iter()
        .chain(&files.external_ids_paths)
        .chain(&files.other_paths)
        .collect();
    let manifest_files: BTreeMap<String, FileDigest> = all_paths
        .into_par_iter()
        .map(|path| Ok((relative_to(&prefix_dir, path)?, digest_file(path)?)))
        .collect::<Result<_>>()?;

    let bundle_bytes: u64 = manifest_files.values().map(|f| f.size).sum();

    let segments = SegmentsManifest {
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
        attr_extents: Vec::new(),
        record_extents: Vec::new(),
        text_extents: Vec::new(),
        external_id_runs,
        locator_extents: Vec::new(),
        tombstones: Vec::new(),
        deny: Vec::new(),
        // Nothing has been added since MANIFEST.json — see the note above.
        vocabulary_extensions: Vec::new(),
        files: BTreeMap::new(),
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
        // The schema, compiled. `MANIFEST.declared_scalars` is the *only* thing downstream reads:
        // `columns.arrow`'s tail is written in this order, `/control/ingest` builds each row's
        // scalar vector in this order, and flush, merge and the fold all take their writer schema
        // from it. Reordering the schema file therefore reorders every segment built after it,
        // which is why the compilation preserves declaration order rather than sorting by name.
        declared_scalars: args
            .schema
            .attributes
            .iter()
            .map(|a| DeclaredScalar {
                name: a.name.clone(),
                arrow_type: a.ty,
                vocabulary: a.vocabulary.clone(),
                // Resolved at the schema parse, so what a bundle records is the identity the build
                // actually indexed with rather than the name a schema asked for.
                analyser: a.analyser.clone(),
                index: a.index,
                render: a.render,
            })
            .collect(),
        // Sorted by name, unlike the columns: nothing indexes a vocabulary positionally, and a
        // `HashMap`'s iteration order would otherwise put non-determinism into the manifest bytes
        // — which are under a digest.
        //
        // A **declared** vocabulary's values are exactly what the schema pinned (`v.codes`,
        // unchanged). A **discovered** one's values come from `minters[&v.name]` instead — the
        // schema's pinned seed *plus* every code this build minted for a key the seed lacked —
        // because `v.codes` alone would silently omit everything minted during the scan. Either
        // way the values are read back sorted by key ([`tessera_store::vocabulary::values_of`]),
        // so the bytes here do not depend on a `BTreeMap`'s or a minter's internal order.
        vocabularies: {
            let mut compiled: Vec<ManifestVocabulary> = args
                .schema
                .vocabularies
                .values()
                .map(|v| {
                    let values = match minters.get(&v.name) {
                        Some(minter) => tessera_store::vocabulary::values_of(minter)
                            .into_iter()
                            .map(|value| ManifestVocabularyValue {
                                label: v.labels.get(&value.key).cloned(),
                                ..value
                            })
                            .collect(),
                        None => v
                            .codes
                            .iter()
                            .map(|(key, &code)| ManifestVocabularyValue {
                                key: key.clone(),
                                code,
                                label: v.labels.get(key).cloned(),
                            })
                            .collect(),
                    };
                    ManifestVocabulary {
                        name: v.name.clone(),
                        // The schema's kind, carried verbatim: it is what ingest consults to
                        // decide whether a key nothing has bound is a typo or a new value.
                        kind: match v.kind {
                            crate::schema::VocabularyKind::Declared => {
                                tessera_store::manifest::VocabularyKind::Declared
                            }
                            crate::schema::VocabularyKind::Discovered => {
                                tessera_store::manifest::VocabularyKind::Discovered
                            }
                        },
                        listing: v.listing,
                        values,
                        reserved: v.reserved.clone(),
                    }
                })
                .collect();
            compiled.sort_by(|a, b| a.name.cmp(&b.name));
            compiled
        },
        small_term_threshold: SMALL_TERM_THRESHOLD_DEFAULT,
        quantisation: Quantisation {
            x_min: args.extent.x_min,
            x_max: args.extent.x_max,
            y_min: args.extent.y_min,
            y_max: args.extent.y_max,
        },
        entity_id_high_water: n,
        identity: IdentityDescriptor {
            construction: IDENTITY_CONSTRUCTION.to_string(),
            rounds: IDENTITY_ROUNDS,
            key: args.identity_key_hex.clone(),
            shard_id: args.shard_id,
            idset: args.idset,
        },
        slices: vec![SliceDescriptor {
            id: args.slice_id.clone(),
            display_name: args.slice_id.clone(),
        }],
        partitions: vec![PartitionDescriptor {
            phash: PHASH.to_string(),
            required_terms: Vec::new(),
        }],
        provenance: match batch_items_recorded {
            // The batch size is identity-bearing (I9): a rebuild must replay it. Omitted
            // entirely for a single-batch build, so pre-batching manifests stay well-defined
            // (absent key == one batch).
            Some(batch_items) => serde_json::json!({
                "generating_set_choice": "prompt-sample",
                "batch_items": batch_items,
            }),
            None => serde_json::json!({ "generating_set_choice": "prompt-sample" }),
        },
        files: manifest_files,
    };
    // Both bundle artefacts, not written here: MANIFEST.json and CURRENT are pass 5's writers
    // too (compaction §10's rule paragraph — a bundle artefact's writer lives in
    // `tessera-store`), so this build and a fold cannot serialise the same shape two different
    // ways and disagree about what a manifest digests to.
    let manifest_digest = write_manifest_json(&prefix_dir, &manifest)?;

    // Everything the bundle names is now durable; `CURRENT` is written last and by rename, so
    // a reader either sees the previous bundle or this complete one, never a half-built prefix.
    write_current(&args.out, PREFIX, &manifest_digest)?;

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
/// re-confirm the permutation covers exactly the rows the segment claims, and re-derive every
/// row's `tessera_id` from `(identity.key, identity.shard_id, entity_id)`, failing if a single
/// row disagrees (contracts §2.6 r6: "`tessera verify` checks the whole column against" the
/// key).
pub fn verify(root: &Path) -> Result<VerifyReport> {
    let bundle = tessera_store::read::open_bundle(root)?;
    // The key is parsed here, not by `open_bundle`: `IdentityDescriptor::validate` (run at
    // open) checks `construction`/`rounds`/`idset` but never parses `key`'s hex, since
    // `tessera-store` has no need to hold a live `IdentityKey` at all — only `tessera verify`
    // and the build do.
    let identity_key = IdentityKey::from_hex(&bundle.manifest.identity.key)
        .map_err(|e| BuildError::Invalid(format!("MANIFEST identity.key: {e}")))?;
    let shard_id = bundle.manifest.identity.shard_id;

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
            // some entity, or `columns.arrow` holds a row no entity can ever address. Built as
            // a row-indexed array (rather than just a count) so the identity check below can
            // reuse it instead of inverting the permutation a second time.
            slice.row_space.base().validate_rows(row_count)?;
            let mut entity_of_row: Vec<Option<u64>> = vec![None; row_count as usize];
            let mut claimed = 0u64;
            for entity in 0..slice.row_space.base().bound() {
                if let Some(row) = slice
                    .row_space
                    .base()
                    .row_of(tessera_types::EntityId::new(entity))
                {
                    entity_of_row[row.raw() as usize] = Some(entity);
                    claimed += 1;
                }
            }
            if claimed != row_count as u64 {
                return Err(BuildError::Invalid(format!(
                    "slice '{slice_id}': permutation claims {claimed} rows but the segments hold \
                     {row_count} — not a bijection"
                )));
            }

            // A build writes exactly one segment per (partition, slice) (contracts §2.1), so
            // its rows are `columns.arrow` row 0..row_count directly. A streamed segment would
            // need its own row-range offset, which does not exist.
            for segment in &slice.segments {
                let ids = segment.columns.tessera_id();
                for (row, id) in ids.iter().enumerate() {
                    // Bijectivity was just confirmed above, so every row has an entity.
                    let entity = entity_of_row[row].expect("row claimed by validate_rows above");
                    let expected = identity_key
                        .forward(shard_id, tessera_types::EntityId::new(entity))
                        .map_err(BuildError::Identity)?
                        .raw();
                    if *id != expected {
                        return Err(BuildError::Invalid(format!(
                            "slice '{slice_id}' row {row}: tessera_id {id:#x} does not match \
                             identity.key's derivation {expected:#x} for entity {entity}"
                        )));
                    }
                }
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

fn write_pairs_parquet(path: &Path, per_term: &[Vec<u32>]) -> Result<()> {
    let mut writer = PairsParquetWriter::create(path)?;
    for (t, entity_ids) in per_term.iter().enumerate() {
        writer.push_run(t as u32, entity_ids)?;
    }
    writer.finish()?;
    Ok(())
}

/// Write `external-ids-0.arrow` (R4; r6 narrows `entity_id` to `uint32`) and
/// `entities/ext-locator.u32` (r6, contracts §2.4/§2.6): the external ID here is the source
/// corpus's entity ID as 8 bytes little-endian; byte order is not numeric order, so the sort is
/// over the encoded keys. Returns the extent paths (in listed order) and the locator's path.
fn write_external_ids(
    dir: &Path,
    staged: &[StagedItem],
    entity_id_high_water: u64,
) -> Result<(Vec<PathBuf>, PathBuf)> {
    let mut rows: Vec<ExternalIdRow> = staged
        .iter()
        .enumerate()
        .map(|(position, item)| ExternalIdRow::new(item.source_id, position as u32))
        .collect();
    rows.sort_unstable_by_key(ExternalIdRow::sort_key);
    let extent_paths = write_external_id_runs(dir, &rows, EXTERNAL_ID_ROWS_PER_EXTENT)?;
    let locator_path = write_ext_locator(dir, &rows, entity_id_high_water)?;
    Ok((extent_paths, locator_path))
}

/// Write `entities/ext-locator.u32` (contracts §2.4/§2.6 r6): one raw `u32` array, no header, no
/// `<k>` suffix, length `entity_id_high_water`, `locator[entity_id] = ordinal` — that entity's
/// position in the concatenated sorted external-id extents, in listed (extent) order.
/// `0xFFFFFFFF` marks an entity with no caller external ID; in this bootstrap build every item is
/// given the source corpus's own id as its external id, so the sentinel is unused here but the
/// array is still initialised to it, since a later, incremental build can append entities this
/// build's extents never cover.
///
/// `rows` must already be in the same ascending order the extents were written in — the
/// concatenation's ordinal for `rows[i]` is exactly `i`, so a second sort or a re-read of the
/// extents is not needed to compute it.
fn write_ext_locator(
    dir: &Path,
    rows: &[ExternalIdRow],
    entity_id_high_water: u64,
) -> Result<PathBuf> {
    let path = dir.join("ext-locator.u32");
    let bound = usize::try_from(entity_id_high_water).map_err(|_| {
        BuildError::Invalid(format!(
            "entity_id_high_water {entity_id_high_water} does not fit usize"
        ))
    })?;
    let mut locator = vec![0xFFFF_FFFFu32; bound];
    for (ordinal, row) in rows.iter().enumerate() {
        let entity = row.entity_id as usize;
        // `entity` is always `< bound` here: every row's entity id came from `0..n` at staging,
        // and `entity_id_high_water` is `n`. Checked anyway — an out-of-range write here would
        // silently corrupt an unrelated entity's locator slot, and that is a disclosure.
        if entity >= locator.len() {
            return Err(BuildError::Invalid(format!(
                "ext-locator: entity id {entity} is out of bound (bound = {})",
                locator.len()
            )));
        }
        locator[entity] = ordinal as u32;
    }
    // Buffered: an unbuffered 4-bytes-per-write loop is one syscall per entity — measured as
    // the majority of the whole external-ids stage at 10⁸ (the bytes written are identical).
    let file = File::create(&path).map_err(|e| BuildError::io(&path, e))?;
    let mut writer = std::io::BufWriter::with_capacity(1 << 20, file);
    for slot in &locator {
        writer
            .write_all(&slot.to_le_bytes())
            .map_err(|e| BuildError::io(&path, e))?;
    }
    let file = writer
        .into_inner()
        .map_err(|e| BuildError::io(&path, e.into_error()))?;
    file.sync_all().map_err(|e| BuildError::io(&path, e))?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent)?;
    }
    Ok(path)
}

/// One `(external_id, entity_id)` row awaiting the byte sort.
///
/// Twelve bytes, four-byte aligned. Deliberately **not** `(u64, u32)`: that tuple is padded to
/// sixteen, which at 10⁹ items is four gigabytes of nothing at the build's second-tightest
/// moment. The external id is held as its sort key — the source id byte-swapped, so that numeric
/// order over `(key_hi, key_lo)` is byte order over the little-endian encoding that goes on disk
/// (R4 sorts by the external id's *bytes*, and byte order is not numeric order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct ExternalIdRow {
    key_hi: u32,
    key_lo: u32,
    entity_id: u32,
}

impl ExternalIdRow {
    pub(crate) fn new(source_id: u64, entity_id: u32) -> Self {
        let key = source_id.swap_bytes();
        ExternalIdRow {
            key_hi: (key >> 32) as u32,
            key_lo: key as u32,
            entity_id,
        }
    }

    pub(crate) fn sort_key(&self) -> (u32, u32) {
        (self.key_hi, self.key_lo)
    }

    fn source_id(&self) -> u64 {
        (((self.key_hi as u64) << 32) | self.key_lo as u64).swap_bytes()
    }
}

/// The largest number of rows one `external-ids-<n>.arrow` extent may carry.
///
/// Arrow's `Binary` layout addresses its values buffer with **`i32`** offsets, so an extent of
/// 8-byte external ids saturates at `i32::MAX / 8` rows — a 10⁹-item bundle cannot be written as
/// one extent at all. Splitting well below that ceiling and listing every extent in
/// `external_id_runs` (contracts §2.1 has always made that field a list, and the engine's
/// index already loads and re-sorts across extents) is what makes the largest corpus
/// expressible; at every scale below the split point exactly one extent is written, identical to
/// what earlier builds wrote.
pub(crate) const EXTERNAL_ID_ROWS_PER_EXTENT: usize = 100_000_000;

/// Write `rows` — already in ascending external-id **byte** order — as one or more extents in
/// `dir`, at most `rows_per_extent` rows each, returning their paths in order. The extents
/// partition the global order into consecutive ranges, so each is individually sorted too.
///
/// `rows_per_extent` is a parameter rather than a direct use of
/// [`EXTERNAL_ID_ROWS_PER_EXTENT`] so the splitting boundary is testable without writing a
/// hundred million rows.
fn write_external_id_runs(
    dir: &Path,
    rows: &[ExternalIdRow],
    rows_per_extent: usize,
) -> Result<Vec<PathBuf>> {
    assert!(rows_per_extent > 0, "rows_per_extent must be positive");
    let mut paths = Vec::new();
    for chunk in rows.chunks(rows_per_extent) {
        let path = dir.join(format!("external-ids-{}.arrow", paths.len()));
        write_external_id_run(&path, chunk)?;
        paths.push(path);
    }
    // `chunks` yields nothing for an empty input, but a bundle always names at least one extent.
    if paths.is_empty() {
        let path = dir.join("external-ids-0.arrow");
        write_external_id_run(&path, &[])?;
        paths.push(path);
    }
    Ok(paths)
}

fn write_external_id_run(path: &Path, rows: &[ExternalIdRow]) -> Result<()> {
    let schema = std::sync::Arc::new(Schema::new(vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new("entity_id", DataType::UInt32, false), // r6, D8: was UInt64
    ]));
    // Built straight from `rows`: an intermediate `Vec` of keys or of widened rows would be a
    // gigabyte-scale copy of data that is already laid out correctly.
    let external: ArrayRef = std::sync::Arc::new(BinaryArray::from_iter_values(
        rows.iter().map(|row| row.source_id().to_le_bytes()),
    ));
    let entity: ArrayRef = std::sync::Arc::new(UInt32Array::from_iter_values(
        rows.iter().map(|row| row.entity_id),
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

/// How much of a file is held in memory at once while hashing it. One mebibyte is large enough
/// that the syscall cost is noise against the hashing and small enough to be irrelevant to the
/// build's peak.
const DIGEST_CHUNK_BYTES: usize = 1 << 20;

/// SHA-256 and size of `path`, read in fixed-size chunks. Never `fs::read` here: at 10⁹ items
/// `columns.arrow` alone is over 20 GB, and slurping it to hash it would reintroduce the very
/// ceiling this build was rewritten to remove.
fn digest_file(path: &Path) -> Result<FileDigest> {
    use std::io::Read;
    let mut file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; DIGEST_CHUNK_BYTES];
    let mut size = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| BuildError::io(path, e))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok(FileDigest {
        size,
        sha256: hex_digest(hasher.finalize().as_slice()),
    })
}

fn hex_digest(digest: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(digest.len() * 2);
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

    /// `BuildArgs` carries the deployment key's plaintext hex beside the redacted `IdentityKey`.
    /// A derived `Debug` would undo the redaction on the first `tracing::error!("{args:?}")`.
    #[test]
    fn build_args_debug_does_not_print_the_identity_key() {
        const KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
        let args = BuildArgs {
            points: PathBuf::from("points.parquet"),
            pairs: PathBuf::from("pairs.parquet"),
            out: PathBuf::from("out"),
            extent: Bounds {
                x_min: 0.0,
                x_max: 1.0,
                y_min: 0.0,
                y_max: 1.0,
            },
            slice_id: "s0".to_string(),
            limit: None,
            identity_key: tessera_types::IdentityKey::from_hex(KEY_HEX).unwrap(),
            identity_key_hex: KEY_HEX.to_string(),
            idset: 1,
            shard_id: 0,
            mint_external_ids: true,
            emit_oracle_pairs: true,
            batch_items: None,
            memory_budget: None,
            band_rows: None,
            schema: Default::default(),
        };
        let printed = format!("{args:?}");
        assert!(
            !printed.contains(KEY_HEX),
            "Debug must not print key material, got: {printed}"
        );
        assert!(printed.contains("fp:"), "got: {printed}");
    }

    #[test]
    fn signature_key_is_sorted_deduplicated_and_order_independent() {
        let a = signature_sort_key(&[TermId::new(5), TermId::new(1), TermId::new(5)]);
        assert_eq!(a, vec![1, 5]);
        assert_eq!(signature_sort_key(&[TermId::new(1), TermId::new(5)]), a);
    }

    #[test]
    fn external_ids_split_at_the_extent_boundary() {
        use arrow::array::{Array, BinaryArray, UInt32Array};

        let temp = tempfile::TempDir::new().unwrap();
        // Source ids chosen so that byte order and numeric order disagree — the sort is over the
        // little-endian encoding (R4), so 0x0100 must come *after* 0xFF.
        let sources: Vec<u64> = vec![0x00FF, 0x0100, 0x0001, 0x0200, 0xFF00, 0x0002, 0x1234];
        let mut rows: Vec<ExternalIdRow> = sources
            .iter()
            .enumerate()
            .map(|(entity, &source)| ExternalIdRow::new(source, entity as u32))
            .collect();
        rows.sort_unstable_by_key(ExternalIdRow::sort_key);

        for rows_per_extent in [1usize, 2, 3, 6, 7, 8, 100] {
            let dir = temp.path().join(format!("split-{rows_per_extent}"));
            fs::create_dir_all(&dir).unwrap();
            let paths = write_external_id_runs(&dir, &rows, rows_per_extent).unwrap();
            assert_eq!(
                paths.len(),
                rows.len().div_ceil(rows_per_extent),
                "wrong extent count at {rows_per_extent} rows per extent"
            );
            for (idx, path) in paths.iter().enumerate() {
                assert_eq!(
                    path.file_name().unwrap(),
                    &*format!("external-ids-{idx}.arrow")
                );
            }

            // Read the extents back in order: the concatenation must be every row exactly once,
            // still in ascending external-id byte order, with each entity id beside its own id.
            let mut seen: Vec<(Vec<u8>, u64)> = Vec::new();
            for path in &paths {
                let reader =
                    arrow::ipc::reader::FileReader::try_new(File::open(path).unwrap(), None)
                        .unwrap();
                for batch in reader {
                    let batch = batch.unwrap();
                    let ids = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<BinaryArray>()
                        .unwrap();
                    let entities = batch
                        .column(1)
                        .as_any()
                        .downcast_ref::<UInt32Array>()
                        .unwrap();
                    for i in 0..batch.num_rows() {
                        seen.push((ids.value(i).to_vec(), entities.value(i) as u64));
                    }
                }
            }
            assert_eq!(seen.len(), rows.len());
            assert!(
                seen.windows(2).all(|w| w[0].0 < w[1].0),
                "extents must partition one ascending byte order, got {seen:?}"
            );
            for (bytes, entity) in &seen {
                let source = u64::from_le_bytes(bytes.as_slice().try_into().unwrap());
                assert_eq!(sources[*entity as usize], source);
            }
        }
    }

    #[test]
    fn an_empty_external_id_relation_still_names_one_extent() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = write_external_id_runs(temp.path(), &[], 4).unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].exists());
    }

    #[test]
    fn external_id_rows_are_twelve_bytes_and_round_trip() {
        assert_eq!(std::mem::size_of::<ExternalIdRow>(), 12);
        for source in [0u64, 1, 0xFF, 0x0100, u64::MAX, 0x0123_4567_89AB_CDEF] {
            let row = ExternalIdRow::new(source, 7);
            assert_eq!(row.source_id(), source);
            assert_eq!(row.entity_id, 7);
        }
        // The sort key must order by the id's *bytes*, which is not its numeric order: little
        // endian puts 0x0100's low byte (0x00) first, so it sorts *before* the larger-looking
        // 0x00FF (whose low byte is 0xFF).
        assert!(0x0100u64.to_le_bytes() < 0x00FFu64.to_le_bytes());
        let mut ids = [0x00FFu64, 0x0100u64];
        ids.sort_by_key(|id| ExternalIdRow::new(*id, 0).sort_key());
        assert_eq!(ids, [0x0100, 0x00FF]);
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
