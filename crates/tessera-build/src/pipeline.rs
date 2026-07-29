//! The streaming batch build: the same bundle [`crate::build_in_memory`] produces, computed
//! within a bounded working set.
//!
//! ## Why this exists
//!
//! The linear build materialises one struct per point (each carrying its own heap-allocated
//! signature) and the whole `term -> entity list` relation as a `Vec<Vec<u32>>` before it writes
//! a byte. At the Phase 0 10⁹-item corpus that is tens of gigabytes of small allocations and it
//! was OOM-killed on a 47 GiB box during staging, before any output existed.
//!
//! ## The construction
//!
//! Every intermediate here is a **flat, packed array indexed by an integer**, and anything that
//! can be recomputed from the input parquet files is recomputed rather than retained — the
//! inputs are read several times because a pass over them costs seconds while holding their
//! contents costs gigabytes. The passes, with the arrays alive at each point:
//!
//! | pass | produces | resident |
//! |---|---|---|
//! | points ×2 | `source_ids`, sorted — an item's **ordinal** is its index here | 8N |
//! | pairs ×1 | the dictionary: term ids in first-appearance order | 8N |
//! | pairs ×1 | `packed`: one `u64` per pair, `ordinal << 32 \| term_id` | 8P |
//! | — | `recs`: the signature sort, 12 bytes per item | 8P + 12N |
//! | — | `entity_of_ordinal`: the permanent I9 assignment | 4N |
//! | pairs ×1 | postings + `pairs.parquet`, via a term-bucketed `u32` array | 4P + 12N |
//! | points ×1 | external ids | 20N |
//! | points ×1 | geometry, the tiler sort, and the segment | 28N |
//!
//! ## Byte-for-byte identity is the correctness condition
//!
//! Entity IDs are assigned by signature order and are **permanent** (I9): a build that ordered
//! items differently would not be a slower or faster build, it would be a *different corpus*,
//! and every posting, permutation and handle derived from it would be invalidated. So this
//! module is not licensed to reorder anything, and `tests/build_equivalence.rs` holds it to
//! producing directory-recursively byte-identical bundles against `build_in_memory`.
//!
//! The three places the equivalence is subtle, and how it is preserved:
//!
//! * **Term-id assignment.** The linear build interns descriptors while walking items in
//!   ascending source-id order, each item's terms in ascending source-term order. A term's id is
//!   therefore its rank under `(first ordinal that carries it, source term id)` — which
//!   [`build_dictionary`] computes directly from one pass over the pairs relation, without
//!   holding a single item.
//! * **The signature sort.** `recs` is pre-sorted on a 64-bit key built from the signature's
//!   first two terms, which totally orders every item whose signature is at most two terms long
//!   (the overwhelming majority — the Phase 0 corpus averages 1.72). Only groups that tie on
//!   that key *and* contain a longer signature are refined by a full signature comparison. The
//!   pre-sort key is order-consistent with the lexicographic signature order, so refining within
//!   ties reproduces it exactly.
//! * **The postings order.** Entity lists come out of a term-bucketed scatter and are sorted per
//!   term, which is the same ascending list the linear build accumulates by walking items in
//!   entity order.

use std::path::PathBuf;

use rustc_hash::{FxHashMap, FxHashSet};

use tessera_authz::{encode_posting, write_posting_records, DictWriter};
use tessera_plugin::{Passthrough, Plugin};
use tessera_spatial::morton::morton_of;
use tessera_store::write::{write_columns, write_morton_codes, write_permutation_iter};
use tessera_types::{EntityId, NODE_NONE, SMALL_TERM_THRESHOLD_DEFAULT};

use crate::error::{BuildError, Result};
use crate::input;
use crate::{
    fsync_file, priority_of, validate_args, write_external_id_extents, write_manifests, BuildArgs,
    BuildReport, BundleFiles, PairsParquetWriter, PHASH, PREFIX, SEG_ID,
};

/// One item's position in the signature sort. 12 bytes, four-byte aligned: at 10⁹ items the
/// difference between this and a naturally-aligned `(u64, u32)` is four gigabytes.
#[derive(Clone, Copy)]
#[repr(C)]
struct SortRec {
    key_hi: u32,
    key_lo: u32,
    ordinal: u32,
}

impl SortRec {
    fn order(&self) -> (u32, u32, u32) {
        (self.key_hi, self.key_lo, self.ordinal)
    }
}

/// One item's position in the tiler sort: Morton code, priority tiebreak, entity id
/// (contracts §2.6). Also 12 bytes for the same reason.
#[derive(Clone, Copy)]
#[repr(C)]
struct RowRec {
    morton: u32,
    entity: u32,
    priority: u16,
    _pad: u16,
}

impl RowRec {
    fn order(&self) -> (u32, u16, u32) {
        (self.morton, self.priority, self.entity)
    }
}

/// Bit `i` of a packed bitset.
fn bit_set(bits: &mut [u64], i: usize) {
    bits[i / 64] |= 1u64 << (i % 64);
}

fn bit_get(bits: &[u64], i: usize) -> bool {
    bits[i / 64] & (1u64 << (i % 64)) != 0
}

/// The pairs relation entry for `ordinal`: a contiguous ascending run in `packed`, whose low 32
/// bits are the item's signature (sorted, deduplicated term ids — §11.1).
fn sig_slice(packed: &[u64], ordinal: u32) -> &[u64] {
    let o = ordinal as u64;
    let lo = packed.partition_point(|&v| (v >> 32) < o);
    let hi = lo + packed[lo..].partition_point(|&v| (v >> 32) == o);
    &packed[lo..hi]
}

fn term_of(packed_entry: u64) -> u32 {
    packed_entry as u32
}

pub(crate) fn build(args: &BuildArgs) -> Result<BuildReport> {
    validate_args(args)?;
    let plugin = Passthrough::new();
    let bounds = plugin.declared_bounds();

    // ---- 1. ordinals: the sorted source ids ------------------------------------------
    // An item's *ordinal* is its index in this array. Ordinal order is source-id order, which is
    // the order the linear build walks items in — so "first appearance" below, and the
    // source-id tiebreak in the signature sort, are both expressible as ordinal comparisons.
    let mut source_ids = read_source_ids(args, None)?;
    if source_ids.is_empty() {
        return Err(BuildError::Invalid(
            "no points selected — a bundle with no items has no expressible entity range".into(),
        ));
    }
    source_ids.sort_unstable();
    if let Some(w) = source_ids.windows(2).find(|w| w[0] == w[1]) {
        let _ = w;
        return Err(BuildError::Invalid(
            "points file contains duplicate entity_id values".into(),
        ));
    }
    let n = source_ids.len() as u64;
    if n > u32::MAX as u64 {
        return Err(BuildError::Invalid(format!(
            "{n} items exceeds bundle_format 1's 2^32 entity-ID ceiling"
        )));
    }

    // ---- 2. the dictionary -----------------------------------------------------------
    let dict_dir = args.out.join(PREFIX).join("dictionary");
    std::fs::create_dir_all(&dict_dir).map_err(|e| BuildError::io(&dict_dir, e))?;
    let Dictionary {
        term_of_source,
        row_counts,
        term_count,
        pair_rows,
        dict_paths,
    } = build_dictionary(args, &source_ids, &dict_dir)?;
    if term_count >= u32::MAX as u64 {
        return Err(BuildError::Invalid(format!(
            "{term_count} distinct terms exceeds the 2^32 term-ID space"
        )));
    }

    // ---- 3. the pairs relation, packed as `ordinal << 32 | term_id` -------------------
    let mut packed: Vec<u64> = Vec::with_capacity(pair_rows);
    input::scan_pairs(&args.pairs, args.limit, |source_id, source_term| {
        let ordinal = source_ids
            .binary_search(&source_id)
            .expect("pass 2 established every pairs entity is a point")
            as u64;
        let term = term_of_source[&source_term] as u64;
        packed.push((ordinal << 32) | term);
    })?;
    drop(source_ids);
    packed.sort_unstable();
    // The label set is a *set*: a source file that repeats a `(entity, term)` row must not turn
    // into a repeated posting (the linear build deduplicates in `read_pairs`).
    packed.dedup();
    let pair_count = packed.len() as u64;

    // ---- 4. the signature sort (I9, permanent — see the module docs) ------------------
    let mut long_sig: Vec<u64> = vec![0; (n as usize).div_ceil(64)];
    let mut recs: Vec<SortRec> = Vec::with_capacity(n as usize);
    let mut over_bound_items = 0u64;
    {
        let mut cursor = 0usize;
        for ordinal in 0..n {
            let start = cursor;
            while cursor < packed.len() && (packed[cursor] >> 32) == ordinal {
                cursor += 1;
            }
            let sig = &packed[start..cursor];
            if sig.len() > bounds.max_terms_per_item as usize {
                // A declared bound is a *declaration*: record it and carry on. Dropping terms
                // here would silently widen the item's visibility (I2/I3).
                over_bound_items += 1;
            }
            if sig.len() > 2 {
                bit_set(&mut long_sig, ordinal as usize);
            }
            // `term + 1` so that "no term at this position" (0) sorts before every real term,
            // which is what makes a signature order before any signature extending it.
            let key: u64 = match sig.len() {
                0 => 0,
                1 => (term_of(sig[0]) as u64 + 1) << 32,
                _ => ((term_of(sig[0]) as u64 + 1) << 32) | (term_of(sig[1]) as u64 + 1),
            };
            recs.push(SortRec {
                key_hi: (key >> 32) as u32,
                key_lo: key as u32,
                ordinal: ordinal as u32,
            });
        }
    }
    if over_bound_items > 0 {
        eprintln!(
            "warning: {over_bound_items} item(s) exceed the plugin's declared \
             max_terms_per_item ({}); no term was dropped",
            bounds.max_terms_per_item
        );
    }
    recs.sort_unstable_by_key(|r| r.order());
    refine_signature_ties(&mut recs, &packed, &long_sig);
    drop(packed);
    drop(long_sig);

    // ---- 5. the permanent assignment: entity id = position in the signature order -----
    let mut entity_of_ordinal: Vec<u32> = vec![0; n as usize];
    for (entity, rec) in recs.iter().enumerate() {
        entity_of_ordinal[rec.ordinal as usize] = entity as u32;
    }
    drop(recs);

    let partition_dir = args.out.join(PREFIX).join("partitions").join(PHASH);
    let terms_dir = partition_dir.join("terms");
    let entities_dir = partition_dir.join("entities");
    let slice_dir = partition_dir.join("slices").join(&args.slice_id);
    let segment_dir = slice_dir.join("segments").join(SEG_ID);
    for dir in [&terms_dir, &entities_dir, &slice_dir, &segment_dir] {
        std::fs::create_dir_all(dir).map_err(|e| BuildError::io(dir, e))?;
    }

    // The source ids are needed twice more (external ids, geometry) and cost 8N to hold across
    // the sort above; re-reading the points file is cheaper than carrying them through it.
    let mut source_ids = read_source_ids(args, Some(n as usize))?;
    source_ids.sort_unstable();

    // ---- 6. postings and pairs.parquet -----------------------------------------------
    let postings_path = terms_dir.join("postings.arrow");
    let pairs_path = terms_dir.join("pairs.parquet");
    write_terms(
        args,
        &postings_path,
        &pairs_path,
        &source_ids,
        &entity_of_ordinal,
        &term_of_source,
        &row_counts,
        pair_count,
    )?;
    fsync_file(&postings_path)?;
    drop(term_of_source);
    drop(row_counts);

    // ---- 7. external ids -------------------------------------------------------------
    // Sorted by the external id's *bytes* (R4). The external id is the source id as 8 bytes
    // little-endian, and byte order over those is numeric order over the byte-swapped value.
    let mut external: Vec<(u64, u32)> = (0..n as usize)
        .map(|ordinal| (source_ids[ordinal].swap_bytes(), entity_of_ordinal[ordinal]))
        .collect();
    external.sort_unstable();
    let external_ids_paths = write_external_id_extents(
        &entities_dir,
        external.len(),
        external
            .iter()
            .map(|&(swapped, entity)| (swapped.swap_bytes(), entity as u64)),
    )?;
    drop(external);

    // ---- 8. geometry, in entity order ------------------------------------------------
    let mut x_of_entity: Vec<f32> = vec![0.0; n as usize];
    let mut y_of_entity: Vec<f32> = vec![0.0; n as usize];
    input::scan_points(&args.points, &args.extent, args.limit, |point| {
        let ordinal = source_ids
            .binary_search(&point.source_id)
            .expect("the same points file yielded this id in pass 1");
        let entity = entity_of_ordinal[ordinal] as usize;
        x_of_entity[entity] = point.x;
        y_of_entity[entity] = point.y;
    })?;
    drop(source_ids);
    drop(entity_of_ordinal);

    // ---- 9. the tiler: (morton, priority, entity_id) ascending (contracts §2.6) -------
    let mut rows: Vec<RowRec> = (0..n as usize)
        .map(|entity| RowRec {
            morton: morton_of(
                x_of_entity[entity] as f64,
                y_of_entity[entity] as f64,
                &args.extent,
            )
            .raw(),
            entity: entity as u32,
            priority: priority_of(EntityId::new(entity as u64)),
            _pad: 0,
        })
        .collect();
    rows.sort_unstable_by_key(|r| r.order());

    // ---- 10. the segment -------------------------------------------------------------
    let morton_path = segment_dir.join("morton.u32");
    write_morton_codes(&morton_path, rows.iter().map(|r| r.morton))
        .map_err(|e| BuildError::io(&morton_path, e))?;

    let permutation_path = slice_dir.join("permutation.bin");
    write_permutation_iter(
        &permutation_path,
        rows.iter().map(|r| EntityId::new(r.entity as u64)),
        n,
    )
    .map_err(|e| BuildError::io(&permutation_path, e))?;
    fsync_file(&permutation_path)?;

    let columns_path = segment_dir.join("columns.arrow");
    {
        // Built and released one column at a time: the record batch itself is the largest thing
        // this build ever holds, so nothing that can be dropped first is kept alongside it.
        let x_row: Vec<f32> = rows
            .iter()
            .map(|r| x_of_entity[r.entity as usize])
            .collect();
        let y_row: Vec<f32> = rows
            .iter()
            .map(|r| y_of_entity[r.entity as usize])
            .collect();
        drop(x_of_entity);
        drop(y_of_entity);
        let entity_row: Vec<u32> = rows.iter().map(|r| r.entity).collect();
        drop(rows);
        let entity_id: Vec<u64> = entity_row.iter().map(|&e| e as u64).collect();
        let priority: Vec<u16> = entity_row
            .iter()
            .map(|&e| priority_of(EntityId::new(e as u64)))
            .collect();
        drop(entity_row);
        let node_id: Vec<u32> = vec![NODE_NONE; n as usize];
        write_columns(&columns_path, entity_id, x_row, y_row, node_id, priority)
            .map_err(|e| BuildError::io(&columns_path, e))?;
    }
    fsync_file(&columns_path)?;
    fsync_file(&morton_path)?;

    // ---- 11. manifests ---------------------------------------------------------------
    write_manifests(
        args,
        &BundleFiles {
            dict_paths,
            dict_records: term_count,
            external_ids_paths,
            other_paths: vec![
                postings_path,
                pairs_path,
                permutation_path,
                columns_path,
                morton_path,
            ],
        },
        &plugin,
        n,
        term_count,
        pair_count,
    )
}

/// The selected source ids, in file order, in an exactly-sized allocation.
///
/// Counted first and then read: letting a `Vec` double its way to 8 GB would peak at three times
/// the final size during the last reallocation, which is precisely the kind of transient this
/// build exists to avoid.
fn read_source_ids(args: &BuildArgs, known_count: Option<usize>) -> Result<Vec<u64>> {
    let count = match known_count {
        Some(count) => count,
        None => {
            let mut count = 0usize;
            input::scan_points(&args.points, &args.extent, args.limit, |_| count += 1)?;
            count
        }
    };
    let mut ids = Vec::with_capacity(count);
    input::scan_points(&args.points, &args.extent, args.limit, |point| {
        ids.push(point.source_id)
    })?;
    Ok(ids)
}

/// What [`build_dictionary`] establishes in its single pass over the pairs relation.
struct Dictionary {
    /// Source term id -> the term id it interned to.
    term_of_source: FxHashMap<u64, u32>,
    /// Per term id, how many pairs **rows** name it — counted before deduplication, so a bucket
    /// sized by this is wide enough even when the input repeats a `(entity, term)` row.
    row_counts: Vec<u64>,
    term_count: u64,
    /// Selected pairs rows, before deduplication.
    pair_rows: usize,
    dict_paths: Vec<PathBuf>,
}

/// Assign term ids and write the dictionary extent.
///
/// The linear build interns descriptors while walking items in ascending source-id order, each
/// item's terms in ascending source-term order; a term's id is its rank in that stream of *first*
/// appearances. That rank is `(smallest ordinal carrying the term, source term id)` — computable
/// from one pass over the pairs relation, over a map with one entry per distinct term (~48k in
/// the Phase 0 corpus), rather than from a materialised item list.
///
/// Returns [`Dictionary`].
fn build_dictionary(
    args: &BuildArgs,
    source_ids: &[u64],
    dict_dir: &std::path::Path,
) -> Result<Dictionary> {
    let mut first_ordinal: FxHashMap<u64, (u64, u64)> = FxHashMap::default();
    let mut absent: FxHashSet<u64> = FxHashSet::default();
    let mut pair_rows = 0usize;
    input::scan_pairs(&args.pairs, args.limit, |source_id, source_term| {
        pair_rows += 1;
        match source_ids.binary_search(&source_id) {
            Ok(ordinal) => {
                let slot = first_ordinal.entry(source_term).or_insert((u64::MAX, 0));
                slot.0 = slot.0.min(ordinal as u64);
                slot.1 += 1;
            }
            Err(_) => {
                absent.insert(source_id);
            }
        }
    })?;
    if !absent.is_empty() {
        return Err(BuildError::Invalid(format!(
            "pairs file references {} entity ids absent from the points file (first: {})",
            absent.len(),
            absent.iter().min().copied().unwrap_or_default()
        )));
    }

    let mut order: Vec<(u64, u64)> = first_ordinal
        .iter()
        .map(|(&source_term, &(ordinal, _))| (ordinal, source_term))
        .collect();
    order.sort_unstable();

    // The Phase 0 corpus carries integer term ids; the item's `access` label is the comma-joined
    // decimal source term ids, so `builtin:passthrough` yields decimal-string descriptors (R6).
    let mut dict = DictWriter::new(dict_dir);
    let mut term_of_source: FxHashMap<u64, u32> =
        FxHashMap::with_capacity_and_hasher(order.len(), Default::default());
    let mut row_counts: Vec<u64> = Vec::with_capacity(order.len());
    for &(_, source_term) in &order {
        let term = dict.intern(source_term.to_string().as_bytes());
        term_of_source.insert(source_term, term.raw());
        row_counts.push(first_ordinal[&source_term].1);
    }
    let term_count = dict.len() as u64;
    let dict_paths = dict.finish().map_err(|e| BuildError::io(dict_dir, e))?;
    for path in &dict_paths {
        fsync_file(path)?;
    }
    Ok(Dictionary {
        term_of_source,
        row_counts,
        term_count,
        pair_rows,
        dict_paths,
    })
}

/// Break the pre-sort's ties by full signature comparison.
///
/// `recs` arrives ordered by `(two-term key, ordinal)`. Items sharing a key share their first two
/// terms; where every one of them has a signature of at most two terms they are *identical*
/// signatures and the ordinal tiebreak already holds. Only a group containing a longer signature
/// needs the full comparison, and a **stable** sort inside the group keeps the ordinal order the
/// pre-sort established as the tiebreak.
fn refine_signature_ties(recs: &mut [SortRec], packed: &[u64], long_sig: &[u64]) {
    let mut start = 0usize;
    while start < recs.len() {
        let mut end = start + 1;
        while end < recs.len()
            && recs[end].key_hi == recs[start].key_hi
            && recs[end].key_lo == recs[start].key_lo
        {
            end += 1;
        }
        let group = &mut recs[start..end];
        if group.len() > 1 && group.iter().any(|r| bit_get(long_sig, r.ordinal as usize)) {
            group.sort_by(|a, b| {
                sig_slice(packed, a.ordinal)
                    .iter()
                    .map(|&v| term_of(v))
                    .cmp(sig_slice(packed, b.ordinal).iter().map(|&v| term_of(v)))
            });
        }
        start = end;
    }
}

/// Write `postings.arrow` and `pairs.parquet`.
///
/// The `(entity, term)` relation is scattered into one flat `u32` array bucketed by term — the
/// bucket boundaries are known in advance from the per-term **row** counts the dictionary pass
/// took — and each bucket is then sorted and deduplicated. That yields exactly the ascending
/// per-term entity lists the linear build accumulates by walking items in entity order, at four
/// bytes per pair and with no per-term allocation.
///
/// Buckets are sized by the *pre-deduplication* row counts, so a repeated `(entity, term)` input
/// row lands in its bucket and is removed by the per-bucket `dedup` rather than needing a
/// seen-set: at 1.72 × 10⁹ pairs a set of every pair seen would be tens of gigabytes, which is
/// the ceiling this whole module exists to stay under.
#[allow(clippy::too_many_arguments)]
fn write_terms(
    args: &BuildArgs,
    postings_path: &std::path::Path,
    pairs_path: &std::path::Path,
    source_ids: &[u64],
    entity_of_ordinal: &[u32],
    term_of_source: &FxHashMap<u64, u32>,
    row_counts: &[u64],
    pair_count: u64,
) -> Result<()> {
    let mut offsets: Vec<u64> = Vec::with_capacity(row_counts.len() + 1);
    let mut total = 0u64;
    offsets.push(0);
    for &count in row_counts {
        total += count;
        offsets.push(total);
    }

    let mut flat: Vec<u32> = vec![0; total as usize];
    let mut cursor: Vec<u64> = offsets[..row_counts.len()].to_vec();
    input::scan_pairs(&args.pairs, args.limit, |source_id, source_term| {
        let ordinal = source_ids
            .binary_search(&source_id)
            .expect("every pairs entity is a point");
        let term = term_of_source[&source_term];
        let slot = &mut cursor[term as usize];
        flat[*slot as usize] = entity_of_ordinal[ordinal];
        *slot += 1;
    })?;

    let mut records: Vec<Vec<u8>> = Vec::with_capacity(row_counts.len());
    let mut pairs_writer = PairsParquetWriter::create(pairs_path)?;
    let mut written = 0u64;
    for term in 0..row_counts.len() {
        let bucket = &mut flat[offsets[term] as usize..offsets[term + 1] as usize];
        bucket.sort_unstable();
        let end = dedup_len(bucket);
        let bucket = &bucket[..end];
        written += end as u64;
        records.push(
            encode_posting(term, bucket, SMALL_TERM_THRESHOLD_DEFAULT)
                .map_err(|e| BuildError::io(postings_path, e))?,
        );
        for &entity in bucket.iter() {
            pairs_writer.push(entity as u64, term as u32)?;
        }
    }
    drop(flat);
    if written != pair_count {
        return Err(BuildError::Invalid(format!(
            "postings hold {written} pairs but the relation has {pair_count}"
        )));
    }
    pairs_writer.finish()?;
    write_posting_records(postings_path, &records).map_err(|e| BuildError::io(postings_path, e))?;
    Ok(())
}

/// Deduplicate a sorted slice in place, returning the length of the deduplicated prefix.
fn dedup_len(sorted: &mut [u32]) -> usize {
    let mut end = 0usize;
    for i in 0..sorted.len() {
        if end == 0 || sorted[i] != sorted[end - 1] {
            sorted[end] = sorted[i];
            end += 1;
        }
    }
    end
}
