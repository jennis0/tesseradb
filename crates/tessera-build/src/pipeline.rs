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
//! contents costs gigabytes. The passes, with the arrays alive at each point (L = items whose
//! signature exceeds two terms; the Phase 0 corpus has L ≈ 0.17N):
//!
//! | pass | produces | resident |
//! |---|---|---|
//! | points ×2 | `source_ids`, sorted — an item's **ordinal** is its index here | 8N |
//! | pairs ×1 | the dictionary: term ids in first-appearance order | 8N |
//! | pairs ×1 | `packed`: one `u64` per pair, `ordinal << 32 \| term_id` | 8P |
//! | — | `recs`: the signature sort, 12 bytes per item | 8P + 12N + 8L |
//! | — | `entity_of_ordinal`: the permanent I9 assignment | 4N |
//! | pairs ×1 | postings + `pairs.parquet`, via a term-bucketed `u32` array | 4P + 12N |
//! | points ×1 | external ids | 20N |
//! | points ×1 | geometry, the tiler sort, and the segment | 28N |
//!
//! ## Ordinal resolution never assumes anything about the ids themselves
//!
//! Source entity ids are caller-supplied external identifiers: they may be dense, sparse,
//! clustered, or arbitrary bit patterns, and no pass may exploit their shape. What the build
//! *establishes* — and all it relies on — is that `source_ids` is sorted and duplicate-free.
//! Each pass that must map a source id back to its ordinal ([`join_chunk`]) therefore buffers a
//! bounded chunk of rows, sorts the chunk, and resolves the whole chunk in **one sequential
//! merge sweep** against `source_ids` — sequential memory traffic for any id distribution,
//! where a per-row binary search over an 8 GB array at 10⁹ was a random cache-and-TLB miss per
//! probe (measured as the dominant cost of the geometry pass, whose input arrives in Morton
//! order). Within a chunk, rows carrying the same id keep no particular order; every consumer
//! is insensitive to it (each call site argues why), so build output stays byte-deterministic.
//!
//! ## Parallelism does not participate in ordering decisions
//!
//! The large sorts run under rayon (`par_sort_unstable*`). Every parallel sort site is a
//! **total order on unique keys** — the pre-sort triple carries the ordinal, the tiler
//! comparator refines through the full `tessera_id` bijection, external-id keys are the
//! dup-checked source ids, and `packed`'s duplicates are bit-identical `u64`s — so an unstable,
//! nondeterministically-scheduled sort still has exactly one output. The equivalence suite's
//! byte-identity assertion is the oracle that keeps this true.
//!
//! ## The two assumptions this construction makes
//!
//! **The inputs do not change while the build runs.** The linear build reads each file once;
//! this one reads the points file four times and the pairs file three, and it carries counts and
//! offsets from an earlier pass into a later one. A points or pairs file rewritten mid-build
//! would therefore be read as two different corpora. That cannot be prevented from inside the
//! process, so it is checked instead: every place a later pass depends on an earlier one — a
//! source id that must resolve to an ordinal, a term bucket that must have room — is a typed
//! error, never an `unwrap`; each later pass additionally re-accumulates an order-independent
//! **content anchor** ([`mix64`] sums over the ids, and over the resolved `(ordinal, term)`
//! relation) and compares it against the first pass's, because counts alone accept
//! substitutions that preserve them. A mutated input fails the build. (The anchors are
//! avalanche-mixed sums, not cryptographic hashes: they make an accidental compensating
//! mutation implausible, and an adversary who can rewrite build inputs mid-run is outside this
//! defence's scope.)
//!
//! **The labelling plugin is `builtin:passthrough`.** [`build_dictionary`] exploits the fact that
//! passthrough's label rule is *decomposable*: an item's descriptors are its comma-separated
//! source terms independently, so a term's descriptor can be derived from the term alone and the
//! whole item never has to be assembled. That is a property of passthrough, not of the plugin
//! ABI — a plugin that derived descriptors from the label as a whole would be mislabelled by
//! this shortcut, and mislabelled authorisation data is the one failure mode this system exists
//! to prevent. [`require_decomposable_labelling`] refuses to run against any other plugin rather
//! than assume it decomposes (I2, fail closed).
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

use std::ops::ControlFlow;
use std::path::PathBuf;

use rayon::prelude::*;
use rustc_hash::FxHashMap;

use tessera_authz::{encode_posting, write_posting_records, DictWriter};
use tessera_plugin::{Passthrough, Plugin};
use tessera_spatial::morton::morton_of;
use tessera_store::write::{write_columns, write_morton_codes, write_permutation_iter};
use tessera_types::{EntityId, IdentityKey, SMALL_TERM_THRESHOLD_DEFAULT};

use crate::error::{BuildError, Result};
use crate::input;
use crate::observer::{BuildObserver, BuildStage, StageTimer};
use crate::{
    fsync_file, validate_args, write_ext_locator, write_external_id_extents, write_manifests,
    BuildArgs, BuildReport, BundleFiles, ExternalIdRow, PairsParquetWriter,
    EXTERNAL_ID_ROWS_PER_EXTENT, PHASH, PREFIX, SEG_ID,
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

/// One item's position in the tiler sort: Morton code, the `tessera_id` prefix (`priority`),
/// and the entity id needed to recompute the full identity on a prefix tie (contracts §2.6).
/// Still 12 bytes, four-byte aligned — a `u64` `tessera_id` here would be 16 B/row, +3.7 GiB at
/// 10⁹, immediately re-spending what dropping `NODE_NONE` just freed (2026-07-30 fold).
#[derive(Clone, Copy)]
#[repr(C)]
struct RowRec {
    morton: u32,
    entity: u32,
    priority: u16,
    _pad: u16,
}

impl RowRec {
    /// `(morton, tessera_id)` ascending, with no further tiebreak (contracts §2.6 r6) —
    /// `priority` is compared first as a cheap, physically contiguous prefix (this record's
    /// point, and §2.6's "why the column exists at all"), and the full identity is recomputed
    /// from `entity` only on a prefix tie. `forward` is a pure function, so this is exact: the
    /// tie path costs eight `splitmix64` rounds, not an approximation of the order.
    ///
    /// Comparing `priority` first and refining on a tie is **identical** to comparing the full
    /// `tessera_id` at every row — it is not merely "usually agrees" — because `priority` is
    /// defined as `tessera_id`'s leading 16 bits (`TesseraId::priority`), so two rows can only
    /// disagree in `priority` if they already disagree in `tessera_id`. A unit test below
    /// checks this comparator against a naive full-`tessera_id` sort over a batch engineered to
    /// contain prefix ties.
    fn cmp(&self, other: &Self, key: &IdentityKey, shard: u32) -> std::cmp::Ordering {
        self.morton.cmp(&other.morton).then_with(|| {
            self.priority.cmp(&other.priority).then_with(|| {
                // Unreachable in practice (the allocator cap makes `forward` infallible for any
                // entity a build ever assigns), but `expect` rather than `unwrap_or` — a
                // silently wrong tiebreak here is a silently wrong row order, and that must be
                // loud if it is ever reached.
                let a = key
                    .forward(shard, EntityId::new(self.entity as u64))
                    .expect("entity ids are capped below u32::MAX by the allocator (I-1)");
                let b = key
                    .forward(shard, EntityId::new(other.entity as u64))
                    .expect("entity ids are capped below u32::MAX by the allocator (I-1)");
                a.cmp(&b)
            })
        })
    }
}

/// The failure every "a later pass disagrees with an earlier one" check reports.
///
/// The only way to reach one of these is for an input file to have changed under the build (see
/// the module docs) or for a counting pass to be wrong. Both are corruption of the relation
/// between an item's geometry and its authorisation data, so both fail the build.
fn input_changed(detail: &str) -> BuildError {
    BuildError::Invalid(format!(
        "the input files changed while the build was reading them ({detail}); \
         the points and pairs files must be immutable for the duration of a build"
    ))
}

/// The order-independent content anchor the multi-pass checks accumulate: a wrapping sum of
/// `mix64` over each element. A plain sum of raw values can be *compensated* — replace rows
/// `{1, 3}` with `{2, 2}` and count and sum both survive — so each value is put through a
/// full-avalanche mixer first, which makes an accidental compensating mutation implausible
/// rather than easy. (splitmix64's finalizer, same constants contracts §2.6 fixes for the
/// identity construction. Not cryptographic, and not meant to be: an adversary who can rewrite
/// build inputs mid-run does not need hash collisions.)
fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Bit `i` of a packed bitset.
fn bit_set(bits: &mut [u64], i: usize) {
    bits[i / 64] |= 1u64 << (i % 64);
}

fn bit_get(bits: &[u64], i: usize) -> bool {
    bits[i / 64] & (1u64 << (i % 64)) != 0
}

fn term_of(packed_entry: u64) -> u32 {
    packed_entry as u32
}

/// Rows buffered per [`join_chunk`] flush: 2²⁶ rows × 16 B ≈ 1 GiB of transient, constant in N.
const JOIN_CHUNK_ROWS: usize = 1 << 26;

/// Resolve a chunk of `(source_id, payload)` rows to ordinals by sorting the chunk and merging
/// it against the sorted `source_ids` in one sequential sweep. See the module docs: this is how
/// every pass maps ids to ordinals without assuming anything about the ids' shape, and without
/// a random-access probe per row.
///
/// `on_row` receives `(Some(ordinal), source_id, payload)` for a resolved row and
/// `(None, source_id, payload)` for an id absent from `source_ids` — whether an absent id is an
/// error, and which error, is the calling pass's decision (the dictionary pass collects them;
/// every later pass fails closed, because its first pass over the same file resolved them).
///
/// Rows with equal ids reach `on_row` in no particular order (the chunk sort is unstable and
/// keyed on the id alone); callers must be — and each caller's site comments argue that they
/// are — insensitive to that order. The chunk is drained; capacity is retained for reuse.
fn join_chunk<P: Copy + Send>(
    chunk: &mut Vec<(u64, P)>,
    source_ids: &[u64],
    mut on_row: impl FnMut(Option<u32>, u64, P) -> Result<()>,
) -> Result<()> {
    chunk.par_sort_unstable_by_key(|entry| entry.0);
    let mut i = 0usize;
    for &(id, payload) in chunk.iter() {
        // Both sides ascend, so `i` only ever moves forward; it does not advance past a match,
        // so a run of rows carrying the same id all resolve to the same ordinal.
        while i < source_ids.len() && source_ids[i] < id {
            i += 1;
        }
        let ordinal = if i < source_ids.len() && source_ids[i] == id {
            Some(i as u32)
        } else {
            None
        };
        on_row(ordinal, id, payload)?;
    }
    chunk.clear();
    Ok(())
}

/// Where each long signature (more than two terms) lives in `packed`, addressable in O(1).
///
/// [`refine_signature_ties`] needs the signature *tail* of every long member of a tie group.
/// Locating it by binary search over `packed` per comparison — or even per member — is a
/// random-probe walk over a multi-gigabyte array; the stage-4 cursor scan already stands on
/// every signature's start, so the starts of the long ones are recorded there (`starts`,
/// ordinal-ascending, **u64**: a u32 offset into `packed` would silently truncate past 2³²
/// pairs, and a wrong slice here is a wrong permanent assignment under I9) and addressed by
/// each ordinal's rank among long ordinals — a per-word popcount block over the `long_sig`
/// bitset the scan builds anyway. 8 bytes per long item plus N/16 bytes of rank blocks.
struct LongIndex {
    bits: Vec<u64>,
    /// `rank_blocks[w]` = set bits in `bits[..w]`. u32 suffices: there are at most N ≤ 2³²−1
    /// long ordinals (the item-count ceiling is enforced before this is built).
    rank_blocks: Vec<u32>,
    starts: Vec<u64>,
}

impl LongIndex {
    fn new(bits: Vec<u64>, starts: Vec<u64>) -> Self {
        let mut rank_blocks = Vec::with_capacity(bits.len());
        let mut acc = 0u32;
        for &word in &bits {
            rank_blocks.push(acc);
            acc += word.count_ones();
        }
        debug_assert_eq!(acc as usize, starts.len(), "one start per long ordinal");
        LongIndex {
            bits,
            rank_blocks,
            starts,
        }
    }

    fn is_long(&self, ordinal: u32) -> bool {
        bit_get(&self.bits, ordinal as usize)
    }

    /// The `packed` index where `ordinal`'s signature begins. `ordinal` must be long.
    fn start_of(&self, ordinal: u32) -> usize {
        let word = ordinal as usize / 64;
        let below = (1u64 << (ordinal % 64)) - 1;
        let rank =
            self.rank_blocks[word] as usize + (self.bits[word] & below).count_ones() as usize;
        self.starts[rank] as usize
    }
}

pub(crate) fn build(args: &BuildArgs, observer: &dyn BuildObserver) -> Result<BuildReport> {
    validate_args(args)?;
    let mut timer = StageTimer::new(observer);
    let plugin = Passthrough::new();
    require_decomposable_labelling(&plugin)?;
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
    // Anchors for the later passes over this same file (step 5's re-read, step 8's geometry
    // scan): the re-read used to be verified against nothing, so a points file swapped
    // mid-build could silently hand every item the wrong external id. An order-independent
    // mixed sum ([`mix64`]) plus the extrema make that loud instead.
    let ids_anchor = source_ids
        .iter()
        .fold(0u64, |acc, &id| acc.wrapping_add(mix64(id)));
    let (ids_first, ids_last) = (source_ids[0], *source_ids.last().expect("non-empty"));

    timer.end(BuildStage::SourceIds, source_ids.len() as u64);

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

    timer.end(BuildStage::Dictionary, term_count);

    // ---- 3. the pairs relation, packed as `ordinal << 32 | term_id` -------------------
    // Chunk-order insensitivity ([`join_chunk`]): `packed` is globally sorted and deduplicated
    // immediately below, so the order rows are pushed in — file order before, chunk-sorted
    // order now — never reaches the output.
    let mut packed: Vec<u64> = Vec::with_capacity(pair_rows);
    // Anchor for stage 6's re-read of the same relation: an order-independent mixed sum over
    // the pre-deduplication resolved rows. Without it, a same-count substitution between the
    // two passes — one entity's rows for another's, bucket counts preserved — would put an
    // entity into a term's posting whose label does not carry the term, silently (fail-open).
    let mut pairs_anchor = 0u64;
    let mut failure: Option<BuildError> = None;
    let mut chunk: Vec<(u64, u64)> = Vec::with_capacity(JOIN_CHUNK_ROWS.min(pair_rows.max(1)));
    let mut resolve = |chunk: &mut Vec<(u64, u64)>, packed: &mut Vec<u64>| {
        join_chunk(chunk, &source_ids, |ordinal, source_id, source_term| {
            // Both lookups were established by the dictionary pass over this same file. A miss
            // here means the file is not the one that pass read.
            let (Some(ordinal), Some(&term)) = (ordinal, term_of_source.get(&source_term)) else {
                return Err(input_changed(&format!(
                    "the pairs file names entity {source_id} term {source_term}, which its \
                     first pass did not"
                )));
            };
            let value = ((ordinal as u64) << 32) | term as u64;
            pairs_anchor = pairs_anchor.wrapping_add(mix64(value));
            packed.push(value);
            Ok(())
        })
    };
    input::scan_pairs(&args.pairs, args.limit, |source_id, source_term| {
        chunk.push((source_id, source_term));
        if chunk.len() == JOIN_CHUNK_ROWS {
            if let Err(e) = resolve(&mut chunk, &mut packed) {
                failure = Some(e);
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    resolve(&mut chunk, &mut packed)?;
    drop(chunk);
    if packed.len() != pair_rows {
        return Err(input_changed(&format!(
            "the pairs file yielded {} rows, then {}",
            pair_rows,
            packed.len()
        )));
    }
    drop(source_ids);
    // A total order on (at worst bit-identical) u64s: the parallel unstable sort has exactly
    // one output.
    packed.par_sort_unstable();
    // The label set is a *set*: a source file that repeats a `(entity, term)` row must not turn
    // into a repeated posting (the linear build deduplicates in `read_pairs`).
    packed.dedup();
    let pair_count = packed.len() as u64;

    timer.end(BuildStage::PairsPack, packed.len() as u64);

    // ---- 4. the signature sort (I9, permanent — see the module docs) ------------------
    let mut long_sig: Vec<u64> = vec![0; (n as usize).div_ceil(64)];
    let mut long_starts: Vec<u64> = Vec::new();
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
                long_starts.push(start as u64);
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
    // The triple (key_hi, key_lo, ordinal) is unique per rec — a total order, so the parallel
    // unstable sort has exactly one output.
    recs.par_sort_unstable_by_key(|r| r.order());
    let long_index = LongIndex::new(long_sig, long_starts);
    refine_signature_ties(&mut recs, &packed, &long_index);
    drop(packed);
    drop(long_index);

    timer.end(BuildStage::SignatureSort, recs.len() as u64);

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
    // the sort above; re-reading the points file is cheaper than carrying them through it —
    // *provided the file has not changed.* The re-read is verified against the first pass's
    // anchors: row count, order-independent id sum, and (after the sort) the extrema. Without
    // this, a points file swapped since stage 1 would silently pair every entity with a wrong
    // external id — the geometry pass's own checks compare the changed file against itself.
    let mut source_ids = read_source_ids(args, Some(n as usize))?;
    if source_ids.len() as u64 != n {
        return Err(input_changed(&format!(
            "the points file re-read for external ids yielded {} rows, not the {} its first \
             pass did",
            source_ids.len(),
            n
        )));
    }
    if source_ids
        .iter()
        .fold(0u64, |acc, &id| acc.wrapping_add(mix64(id)))
        != ids_anchor
    {
        return Err(input_changed(
            "the points file re-read for external ids carries different ids than its first \
             pass did (row count unchanged)",
        ));
    }
    source_ids.par_sort_unstable();
    if source_ids[0] != ids_first || *source_ids.last().expect("non-empty") != ids_last {
        return Err(input_changed(
            "the points file's id range changed between its first pass and the re-read",
        ));
    }

    timer.end(BuildStage::Assignment, n);

    // ---- 6. postings and pairs.parquet -----------------------------------------------
    let postings_path = terms_dir.join("postings.arrow");
    let pairs_path = args
        .emit_oracle_pairs
        .then(|| terms_dir.join("pairs.parquet"));
    write_terms(
        args,
        &postings_path,
        pairs_path.as_deref(),
        &source_ids,
        &entity_of_ordinal,
        &term_of_source,
        &row_counts,
        pair_count,
        pairs_anchor,
    )?;
    fsync_file(&postings_path)?;
    drop(term_of_source);
    drop(row_counts);

    timer.end(BuildStage::PostingsWrite, pair_count);

    // ---- 7. external ids (only when minting — see `BuildArgs::mint_external_ids`) -----
    let mut external_ids_paths: Vec<PathBuf> = Vec::new();
    let mut ext_locator_path: Option<PathBuf> = None;
    if args.mint_external_ids {
        // Sorted by the external id's *bytes* (R4); `ExternalIdRow` holds each id as the sort
        // key that makes that a plain integer comparison, in twelve bytes rather than a padded
        // sixteen.
        let mut external: Vec<ExternalIdRow> = (0..n as usize)
            .map(|ordinal| ExternalIdRow::new(source_ids[ordinal], entity_of_ordinal[ordinal]))
            .collect();
        // Keys are the byte-swapped source ids — dup-checked, hence unique: a total order, one
        // output under the parallel unstable sort.
        external.par_sort_unstable_by_key(ExternalIdRow::sort_key);
        external_ids_paths =
            write_external_id_extents(&entities_dir, &external, EXTERNAL_ID_ROWS_PER_EXTENT)?;
        // `external` is still in the concatenated extent order at this point (the extents
        // partition it into consecutive ranges, in order) — its index *is* each row's ordinal,
        // which is exactly what the locator addresses (contracts §2.4/§2.6 r6).
        ext_locator_path = Some(write_ext_locator(&entities_dir, &external, n)?);
    }

    timer.end(
        BuildStage::ExternalIds,
        if args.mint_external_ids { n } else { 0 },
    );

    // ---- 8. geometry, in entity order ------------------------------------------------
    // Chunk-order insensitivity ([`join_chunk`]): source ids are duplicate-checked, so every
    // `(x_of_entity, y_of_entity)` slot is written exactly once — there is no order to observe.
    // (A file that repeats or substitutes ids since the first pass fails the id-anchor check
    // below — a row count alone would accept a repeat that compensates a removal, and this was
    // previously last-write-wins silent.)
    let mut x_of_entity: Vec<f32> = vec![0.0; n as usize];
    let mut y_of_entity: Vec<f32> = vec![0.0; n as usize];
    let mut points_seen = 0u64;
    let mut geom_anchor = 0u64;
    let mut failure: Option<BuildError> = None;
    let mut chunk: Vec<(u64, (f32, f32))> = Vec::with_capacity(JOIN_CHUNK_ROWS.min(n as usize));
    let resolve = |chunk: &mut Vec<(u64, (f32, f32))>,
                       x_of_entity: &mut Vec<f32>,
                       y_of_entity: &mut Vec<f32>,
                       points_seen: &mut u64,
                       geom_anchor: &mut u64| {
        join_chunk(chunk, &source_ids, |ordinal, source_id, (x, y)| {
            let Some(ordinal) = ordinal else {
                return Err(input_changed(&format!(
                    "the points file names entity {source_id}, which its first pass did not"
                )));
            };
            let entity = entity_of_ordinal[ordinal as usize] as usize;
            x_of_entity[entity] = x;
            y_of_entity[entity] = y;
            *points_seen += 1;
            *geom_anchor = geom_anchor.wrapping_add(mix64(source_id));
            Ok(())
        })
    };
    input::scan_points(&args.points, &args.extent, args.limit, |point| {
        chunk.push((point.source_id, (point.x, point.y)));
        if chunk.len() == JOIN_CHUNK_ROWS {
            if let Err(e) = resolve(
                &mut chunk,
                &mut x_of_entity,
                &mut y_of_entity,
                &mut points_seen,
                &mut geom_anchor,
            ) {
                failure = Some(e);
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    resolve(
        &mut chunk,
        &mut x_of_entity,
        &mut y_of_entity,
        &mut points_seen,
        &mut geom_anchor,
    )?;
    drop(chunk);
    // A shrunk points file resolves every id it still presents and would previously leave the
    // missing entities at (0, 0) with no error at all — count, don't trust.
    if points_seen != n {
        return Err(input_changed(&format!(
            "the points file yielded {points_seen} geometry rows, but its first pass \
             selected {n}"
        )));
    }
    // And a count alone accepts a repeat that compensates a removal ({1,2,3} become {2,2,2}):
    // the multiset of ids must be the first pass's, so entity slots are written exactly once.
    if geom_anchor != ids_anchor {
        return Err(input_changed(
            "the points file's geometry pass carries different ids than its first pass did \
             (row count unchanged)",
        ));
    }
    drop(source_ids);
    drop(entity_of_ordinal);

    timer.end(BuildStage::GeometryScan, n);

    // ---- 9. the tiler: (morton, tessera_id) ascending, no further tiebreak (contracts
    // §2.6 r6) — `tessera_id` is computed here, BEFORE the sort (2026-07-30 fold, memo §6):
    // `priority = high16(tessera_id)` is now the sort key, so the identity must exist before
    // `sort_unstable_by` runs, not be written at the row after it. -------------------------
    // Indexed parallel map: the collect preserves entity order, and `forward`/`morton_of` are
    // pure, so this is byte-identical to the serial loop it replaces.
    let mut rows: Vec<RowRec> = (0..n as usize)
        .into_par_iter()
        .map(|entity| {
            let tessera_id = args
                .identity_key
                .forward(args.shard_id, EntityId::new(entity as u64))?;
            Ok(RowRec {
                morton: morton_of(
                    x_of_entity[entity] as f64,
                    y_of_entity[entity] as f64,
                    &args.extent,
                )
                .raw(),
                entity: entity as u32,
                priority: tessera_id.priority(),
                _pad: 0,
            })
        })
        .collect::<Result<_>>()?;
    // The comparator is a total order — `(morton, priority, full tessera_id)`, and `forward`
    // is a bijection per entity — so the parallel unstable sort has exactly one output.
    // `IdentityKey` is a pure value type; `forward` takes `&self` and is safe to call from
    // every worker at once, and the tie path's `expect` stays loud through rayon's panic
    // propagation.
    rows.par_sort_unstable_by(|a, b| a.cmp(b, &args.identity_key, args.shard_id));

    timer.end(BuildStage::TilerSort, n);

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
        // Indexed parallel gathers — collect preserves row order, so bytes are unchanged; at
        // 10⁹ rows the serial versions are a billion random 4-byte reads each.
        let x_row: Vec<f32> = rows
            .par_iter()
            .map(|r| x_of_entity[r.entity as usize])
            .collect();
        let y_row: Vec<f32> = rows
            .par_iter()
            .map(|r| y_of_entity[r.entity as usize])
            .collect();
        drop(x_of_entity);
        drop(y_of_entity);
        let entity_row: Vec<u32> = rows.iter().map(|r| r.entity).collect();
        drop(rows);
        // `forward` is fallible (Important I-1): a checked conversion, never `as u32`. At build
        // the allocator cap makes the error unreachable, and collecting into a `Result` is what
        // keeps it that way rather than assuming it. This is the permutation of an identity
        // vector that already existed before the sort (step 9 above), not its first computation.
        let tessera_row: Vec<u64> = entity_row
            .par_iter()
            .map(|&e| {
                args.identity_key
                    .forward(args.shard_id, EntityId::new(e as u64))
                    .map(|id| id.raw())
            })
            .collect::<std::result::Result<_, _>>()
            .map_err(BuildError::Identity)?;
        drop(entity_row);
        write_columns(&columns_path, tessera_row, x_row, y_row)
            .map_err(|e| BuildError::io(&columns_path, e))?;
    }
    fsync_file(&columns_path)?;
    fsync_file(&morton_path)?;

    timer.end(BuildStage::SegmentWrite, n);

    // ---- 11. manifests ---------------------------------------------------------------
    let mut other_paths = vec![postings_path, permutation_path, columns_path, morton_path];
    other_paths.extend(pairs_path);
    other_paths.extend(ext_locator_path);
    let report = write_manifests(
        args,
        &BundleFiles {
            dict_paths,
            dict_records: term_count,
            external_ids_paths,
            other_paths,
        },
        &plugin,
        n,
        term_count,
        pair_count,
    )?;
    // Reported in bytes, not rows: this stage re-reads and SHA-256s every byte the build wrote,
    // so it scales with bundle size rather than with item count.
    timer.end(BuildStage::Manifests, report.bundle_bytes);
    Ok(report)
}

/// The selected source ids, in scan order (which is **no particular order** — the decode is
/// parallel; every consumer sorts), in an exactly-sized allocation.
///
/// Counted first and then read: letting a `Vec` double its way to 8 GB would peak at three times
/// the final size during the last reallocation, which is precisely the kind of transient this
/// build exists to avoid.
fn read_source_ids(args: &BuildArgs, known_count: Option<usize>) -> Result<Vec<u64>> {
    let count = match known_count {
        Some(count) => count,
        // No limit ⇒ every row is selected ⇒ the metadata row count is exact and the counting
        // decode is a whole pass over the file for nothing.
        None if args.limit.is_none() => input::count_point_rows(&args.points)? as usize,
        None => {
            let mut count = 0usize;
            input::scan_points(&args.points, &args.extent, args.limit, |_| {
                count += 1;
                ControlFlow::Continue(())
            })?;
            count
        }
    };
    let mut ids = Vec::with_capacity(count);
    input::scan_points(&args.points, &args.extent, args.limit, |point| {
        ids.push(point.source_id);
        ControlFlow::Continue(())
    })?;
    Ok(ids)
}

/// Refuse to run unless the configured plugin labels items the way this pipeline assumes.
///
/// The dictionary pass derives each term's descriptor from the term id alone, which is only
/// sound when the plugin's label rule is decomposable — when `terms_of_label` over a
/// comma-joined list yields exactly the per-element descriptors, in order. `builtin:passthrough`
/// (R6) is defined that way; nothing in the plugin ABI requires it, and a plugin that derived
/// descriptors from the label as a whole (a rule engine, a normaliser, anything that folds
/// terms together) would be silently mislabelled here — every posting would name the wrong term,
/// which is a disclosure, not a bug in a performance path.
///
/// So this is checked twice over, and fails closed: the plugin must *be* passthrough by its
/// declared `data_plugin_hash`, and it must *behave* decomposably on a probe label. The hash
/// check is what will still hold when `build` grows a plugin parameter; the probe is what
/// catches a passthrough whose rule was changed without its hash being bumped.
fn require_decomposable_labelling(plugin: &impl Plugin) -> Result<()> {
    let reference = Passthrough::new();
    if plugin.data_plugin_hash() != reference.data_plugin_hash() {
        return Err(BuildError::Invalid(format!(
            "the streaming build derives each term's descriptor from the term alone, which is \
             only valid for builtin:passthrough's decomposable label rule; this plugin declares \
             data_plugin_hash {} (expected {}). Build through `build_in_memory`, which routes \
             every item's label through the plugin, or teach the pipeline this plugin's rule.",
            plugin.data_plugin_hash(),
            reference.data_plugin_hash()
        )));
    }
    let probe: &[u8] = b"11,7,4096";
    let descriptors = plugin.terms_of_label(probe)?;
    let expected: Vec<Vec<u8>> = vec![b"11".to_vec(), b"7".to_vec(), b"4096".to_vec()];
    if descriptors != expected {
        return Err(BuildError::Invalid(format!(
            "the plugin's label rule is not decomposable: label {:?} yielded {:?}, not one \
             descriptor per comma-separated term",
            String::from_utf8_lossy(probe),
            descriptors
                .iter()
                .map(|d| String::from_utf8_lossy(d).into_owned())
                .collect::<Vec<_>>()
        )));
    }
    Ok(())
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
    // Chunk-order insensitivity ([`join_chunk`]): per-term min-ordinal and row counts are
    // commutative aggregations — no arrival order is observable in them.
    let mut first_ordinal: FxHashMap<u64, (u64, u64)> = FxHashMap::default();
    // Absent ids are reported by count and minimum, never collected: a mispaired input naming
    // billions of missing ids would otherwise accumulate a multi-gigabyte set — the exact
    // transient class this module exists to avoid — before producing its typed error.
    let mut absent_count = 0u64;
    let mut absent_min = u64::MAX;
    let mut pair_rows = 0usize;
    // Grows toward `JOIN_CHUNK_ROWS` only if the relation is actually that large; this pass
    // has no row count in hand yet.
    let mut chunk: Vec<(u64, u64)> = Vec::new();
    let mut resolve = |chunk: &mut Vec<(u64, u64)>| {
        join_chunk(chunk, source_ids, |ordinal, source_id, source_term| {
            match ordinal {
                Some(ordinal) => {
                    let slot = first_ordinal.entry(source_term).or_insert((u64::MAX, 0));
                    slot.0 = slot.0.min(ordinal as u64);
                    slot.1 += 1;
                }
                None => {
                    absent_count += 1;
                    absent_min = absent_min.min(source_id);
                }
            }
            Ok(())
        })
    };
    let mut failure: Option<BuildError> = None;
    input::scan_pairs(&args.pairs, args.limit, |source_id, source_term| {
        pair_rows += 1;
        chunk.push((source_id, source_term));
        if chunk.len() == JOIN_CHUNK_ROWS {
            if let Err(e) = resolve(&mut chunk) {
                failure = Some(e);
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    resolve(&mut chunk)?;
    drop(chunk);
    if absent_count > 0 {
        return Err(BuildError::Invalid(format!(
            "pairs file references {absent_count} entity ids absent from the points file \
             (smallest: {absent_min})"
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
/// needs refining — and inside such a group the work decomposes, because of two facts about the
/// stage-4 key:
///
/// * **Every member of a refined group has exactly two or more terms.** The key encodes "no
///   term at this position" as 0 and a real term `t` as `t + 1 ≥ 1` — and `t + 1` cannot wrap
///   to 0, because term ids are checked below 2³²−1 right after the dictionary is built. So a
///   zero-length signature keys as (0, 0) and a one-term signature as (t₀+1, 0), neither of
///   which a long (>2-term) signature — (t₀+1, t₁+1), both halves ≥ 1 — can collide with. A
///   refined group therefore holds only **short** members whose signature is *exactly* the
///   group's two-term prefix (all identical) and **long** members extending that prefix.
/// * **A prefix orders before every extension of it**, and identical signatures tie — so the
///   refined order is: all shorts first, in the ordinal order the pre-sort already established;
///   then the longs, ordered by their signature *tails* (terms from index 2 on) with ordinal
///   breaking exact-tail ties. That is precisely the reference build's
///   `(signature, source_id)` order, with the tiebreak the current stable sort left implicit
///   made explicit.
///
/// Mechanically, each long member's tail is gathered **once** into a scratch arena — its
/// location in `packed` comes from [`LongIndex`] in O(1), not from a binary search — and the
/// long sort compares contiguous scratch, not the multi-gigabyte relation. The previous
/// implementation did two `partition_point` probes over `packed` *per comparison*; with 46% of
/// the Phase 0 corpus inside refined groups that was the single largest cost of the whole
/// build (measured: 52.5% of the 1e8 build, superlinear).
///
/// Groups are disjoint slices of `recs`, so refinement runs in parallel across groups; each
/// group's result is deterministic, so the whole pass is.
fn refine_signature_ties(recs: &mut [SortRec], packed: &[u64], long: &LongIndex) {
    // Group boundaries first (cheap linear scan), keeping only groups that need refining.
    let mut refined: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    while start < recs.len() {
        let mut end = start + 1;
        while end < recs.len()
            && recs[end].key_hi == recs[start].key_hi
            && recs[end].key_lo == recs[start].key_lo
        {
            end += 1;
        }
        if end - start > 1 && recs[start..end].iter().any(|r| long.is_long(r.ordinal)) {
            refined.push((start, end));
        }
        start = end;
    }

    // Split `recs` into one disjoint `&mut` slice per refined group (safe: the ranges are
    // ascending and non-overlapping; `split_at_mut` walks them off the front).
    let mut groups: Vec<&mut [SortRec]> = Vec::with_capacity(refined.len());
    let mut rest = recs;
    let mut consumed = 0usize;
    for &(g_start, g_end) in &refined {
        let (_, tail) = rest.split_at_mut(g_start - consumed);
        let (group, tail) = tail.split_at_mut(g_end - g_start);
        groups.push(group);
        rest = tail;
        consumed = g_end;
    }

    groups
        .into_par_iter()
        .for_each_init(RefineScratch::default, |scratch, group| {
            refine_group(group, packed, long, scratch)
        });
}

/// Per-worker buffers for [`refine_group`], reused across the groups a worker processes.
#[derive(Default)]
struct RefineScratch {
    shorts: Vec<SortRec>,
    /// `(tail start in `arena`, tail length, the rec)` per long member.
    longs: Vec<(usize, usize, SortRec)>,
    /// Worst case for one group is all long tails in the corpus sharing one two-term prefix —
    /// 4 bytes per tail term, approaching 4P in the fully degenerate one-group corpus. The
    /// Phase 0 shape stays in the tens of megabytes; a corpus pathological enough to matter
    /// here would already be pathological for `flat` (4P, resident in the same build).
    arena: Vec<u32>,
}

/// Refine one tie group: shorts keep their order at the front, longs sort by (tail, ordinal).
/// See [`refine_signature_ties`] for why this equals the full-signature stable sort.
fn refine_group(group: &mut [SortRec], packed: &[u64], long: &LongIndex, s: &mut RefineScratch) {
    // A refined group's key has both halves non-zero (the disjointness argument above); a
    // violation would mean a short member with fewer than two terms slipped in, which the
    // partition below would order incorrectly. Loud in debug, impossible by construction.
    debug_assert!(
        group[0].key_hi != 0 && group[0].key_lo != 0,
        "a refined tie group must hold only signatures of length >= 2"
    );
    s.shorts.clear();
    s.longs.clear();
    s.arena.clear();
    for &rec in group.iter() {
        if long.is_long(rec.ordinal) {
            let sig_start = long.start_of(rec.ordinal);
            let tail_start = s.arena.len();
            // Skip the two prefix terms every member shares; walk the run to its end. The run
            // is contiguous and ordinal-delimited, so no length bookkeeping is needed.
            let mut i = sig_start + 2;
            while i < packed.len() && (packed[i] >> 32) == rec.ordinal as u64 {
                s.arena.push(term_of(packed[i]));
                i += 1;
            }
            debug_assert!(
                s.arena.len() > tail_start,
                "a long signature has at least one tail term"
            );
            s.longs.push((tail_start, s.arena.len() - tail_start, rec));
        } else {
            s.shorts.push(rec);
        }
    }
    let arena = &s.arena;
    // (tail, ordinal) is a total order on unique keys — ordinals are unique — so this
    // unstable sort has exactly one output, identical to the stable full-signature sort's.
    s.longs.sort_unstable_by(|a, b| {
        arena[a.0..a.0 + a.1]
            .cmp(&arena[b.0..b.0 + b.1])
            .then_with(|| a.2.ordinal.cmp(&b.2.ordinal))
    });
    group[..s.shorts.len()].copy_from_slice(&s.shorts);
    for (k, &(_, _, rec)) in s.longs.iter().enumerate() {
        group[s.shorts.len() + k] = rec;
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
    pairs_path: Option<&std::path::Path>,
    source_ids: &[u64],
    entity_of_ordinal: &[u32],
    term_of_source: &FxHashMap<u64, u32>,
    row_counts: &[u64],
    pair_count: u64,
    pairs_anchor: u64,
) -> Result<()> {
    let mut offsets: Vec<u64> = Vec::with_capacity(row_counts.len() + 1);
    let mut total = 0u64;
    offsets.push(0);
    for &count in row_counts {
        total += count;
        offsets.push(total);
    }

    // Chunk-order insensitivity ([`join_chunk`]): each bucket's fill order varies with chunk
    // boundaries, but every bucket is sorted and deduplicated below before anything reads it —
    // the fill order never reaches the output.
    let mut flat: Vec<u32> = vec![0; total as usize];
    let mut cursor: Vec<u64> = offsets[..row_counts.len()].to_vec();
    // This pass's accumulation of the stage-3 anchor: the same mixed sum over the same
    // pre-deduplication `(ordinal, term)` multiset, compared below. Counts alone cannot catch
    // a substitution that preserves per-term row counts; the anchor does.
    let mut seen_anchor = 0u64;
    let mut failure: Option<BuildError> = None;
    let mut chunk: Vec<(u64, u64)> = Vec::with_capacity(JOIN_CHUNK_ROWS.min(total as usize));
    let mut resolve = |chunk: &mut Vec<(u64, u64)>, flat: &mut Vec<u32>, cursor: &mut Vec<u64>| {
        join_chunk(chunk, source_ids, |ordinal, source_id, source_term| {
            let (Some(ordinal), Some(&term)) = (ordinal, term_of_source.get(&source_term)) else {
                return Err(input_changed(&format!(
                    "the pairs file names entity {source_id} term {source_term}, which its \
                     first pass did not"
                )));
            };
            seen_anchor = seen_anchor.wrapping_add(mix64(((ordinal as u64) << 32) | term as u64));
            let slot = &mut cursor[term as usize];
            // The bucket was sized by the dictionary pass's count for this term. Writing past
            // its end would land in the *next* term's bucket — one term's entities silently
            // becoming another's posting, which is a disclosure. Check rather than trust the
            // two counts agree.
            if *slot >= offsets[term as usize + 1] {
                return Err(input_changed(&format!(
                    "term {term}'s bucket holds {} rows but a further row arrived",
                    row_counts[term as usize]
                )));
            }
            flat[*slot as usize] = entity_of_ordinal[ordinal as usize];
            *slot += 1;
            Ok(())
        })
    };
    input::scan_pairs(&args.pairs, args.limit, |source_id, source_term| {
        chunk.push((source_id, source_term));
        if chunk.len() == JOIN_CHUNK_ROWS {
            if let Err(e) = resolve(&mut chunk, &mut flat, &mut cursor) {
                failure = Some(e);
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    resolve(&mut chunk, &mut flat, &mut cursor)?;
    drop(chunk);
    if seen_anchor != pairs_anchor {
        return Err(input_changed(
            "the pairs file resolved to a different (entity, term) multiset than the relation \
             pass read (row counts unchanged)",
        ));
    }
    // The mirror of the overflow check: a bucket left short would leave its tail zeroed, and a
    // zero is a valid entity id, so an under-filled bucket must be caught by count, not by value.
    for (term, slot) in cursor.iter().enumerate() {
        if *slot != offsets[term + 1] {
            return Err(input_changed(&format!(
                "term {term}'s bucket expected {} rows, received {}",
                row_counts[term],
                slot - offsets[term]
            )));
        }
    }

    // Split `flat` into one disjoint `&mut` bucket per term (safe: consecutive ranges walked
    // off the front), then sort, deduplicate and Roaring-encode every bucket in parallel.
    // Bucket contents are multisets of entity ids — sort+dedup normalises whatever fill order
    // the chunks produced — and `encode_posting` is pure, so the collected results are
    // deterministic and in term order.
    let mut buckets: Vec<&mut [u32]> = Vec::with_capacity(row_counts.len());
    let mut rest: &mut [u32] = &mut flat;
    for term in 0..row_counts.len() {
        let width = (offsets[term + 1] - offsets[term]) as usize;
        let (bucket, tail) = rest.split_at_mut(width);
        buckets.push(bucket);
        rest = tail;
    }
    let encoded: Vec<(usize, Vec<u8>)> = buckets
        .into_par_iter()
        .enumerate()
        .map(|(term, bucket)| {
            bucket.sort_unstable();
            let end = dedup_len(bucket);
            let record = encode_posting(term, &bucket[..end], SMALL_TERM_THRESHOLD_DEFAULT)
                .map_err(|e| BuildError::io(postings_path, e))?;
            Ok((end, record))
        })
        .collect::<Result<_>>()?;

    let mut records: Vec<Vec<u8>> = Vec::with_capacity(row_counts.len());
    let mut pairs_writer = pairs_path.map(PairsParquetWriter::create).transpose()?;
    let mut written = 0u64;
    for (term, (end, record)) in encoded.into_iter().enumerate() {
        let bucket = &flat[offsets[term] as usize..offsets[term] as usize + end];
        written += end as u64;
        records.push(record);
        if let Some(writer) = pairs_writer.as_mut() {
            writer.push_run(term as u32, bucket)?;
        }
    }
    drop(flat);
    if written != pair_count {
        return Err(BuildError::Invalid(format!(
            "postings hold {written} pairs but the relation has {pair_count}"
        )));
    }
    if let Some(writer) = pairs_writer {
        writer.finish()?;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_types::IdentityKey;

    /// `LongIndex::start_of` must agree with a naive rank computation at every long ordinal,
    /// across word boundaries and word-aligned positions.
    #[test]
    fn long_index_rank_agrees_with_naive_rank() {
        // Long ordinals chosen to straddle word boundaries: 0, mid-word, 63/64/65, and a
        // sparse tail; every other ordinal is short.
        let long_ordinals: Vec<u32> = vec![0, 3, 63, 64, 65, 127, 128, 300, 449];
        let n = 450usize;
        let mut bits = vec![0u64; n.div_ceil(64)];
        for &o in &long_ordinals {
            bit_set(&mut bits, o as usize);
        }
        // Each long ordinal's "start" is a distinct sentinel so a wrong rank reads as a wrong
        // value, not a coincidence.
        let starts: Vec<u64> = long_ordinals.iter().map(|&o| 1000 + o as u64).collect();
        let index = LongIndex::new(bits, starts);
        for &o in &long_ordinals {
            assert!(index.is_long(o));
            assert_eq!(index.start_of(o), 1000 + o as usize, "ordinal {o}");
        }
        assert!(!index.is_long(1));
        assert!(!index.is_long(62));
        assert!(!index.is_long(129));
    }

    /// `join_chunk` must resolve every id that is present (including runs of duplicates, which
    /// all map to the same ordinal), report every id that is absent as `None`, and drain the
    /// chunk. The source ids are deliberately arbitrary — nothing about their shape may matter.
    #[test]
    fn join_chunk_resolves_duplicates_and_reports_misses() {
        let source_ids: Vec<u64> = vec![5, 90, 1_000_003, u64::MAX - 1];
        let mut chunk: Vec<(u64, u32)> = vec![
            (1_000_003, 10),
            (5, 11),
            (90, 12),
            (5, 13), // duplicate id, distinct payload
            (7, 14), // absent
            (u64::MAX - 1, 15),
            (u64::MAX, 16), // absent, past the last source id
        ];
        let mut seen: Vec<(Option<u32>, u64, u32)> = Vec::new();
        join_chunk(&mut chunk, &source_ids, |ordinal, id, payload| {
            seen.push((ordinal, id, payload));
            Ok(())
        })
        .unwrap();
        assert!(chunk.is_empty(), "the chunk must be drained");
        seen.sort_unstable_by_key(|&(_, _, p)| p);
        assert_eq!(
            seen,
            vec![
                (Some(2), 1_000_003, 10),
                (Some(0), 5, 11),
                (Some(1), 90, 12),
                (Some(0), 5, 13),
                (None, 7, 14),
                (Some(3), u64::MAX - 1, 15),
                (None, u64::MAX, 16),
            ]
        );
    }

    /// The first `Err` from `on_row` aborts the join and propagates.
    #[test]
    fn join_chunk_propagates_the_callbacks_error() {
        let source_ids: Vec<u64> = vec![1, 2];
        let mut chunk: Vec<(u64, ())> = vec![(1, ()), (3, ())];
        let result = join_chunk(&mut chunk, &source_ids, |ordinal, id, ()| match ordinal {
            Some(_) => Ok(()),
            None => Err(input_changed(&format!("entity {id} missing"))),
        });
        assert!(result.is_err());
    }

    /// The tie path (Step 3a fold): comparing the `priority` prefix first and refining on a tie
    /// by recomputing the full `tessera_id` from `entity` must produce **exactly** the same row
    /// order as sorting by the full `tessera_id` directly — not merely "usually agrees". Fixed
    /// `morton` across every row so the fixture ties on the first comparator field too, forcing
    /// the comparison down to `priority` and then, on a further tie, the full identity.
    #[test]
    fn row_rec_comparator_agrees_with_a_full_tessera_id_sort_over_engineered_ties() {
        let key = IdentityKey::from_hex("000102030405060708090a0b0c0d0e0f").unwrap();
        let shard = 0u32;
        let morton = 42u32;

        let rows: Vec<RowRec> = (0..4000u32)
            .map(|entity| {
                let tessera_id = key.forward(shard, EntityId::new(entity as u64)).unwrap();
                RowRec {
                    morton,
                    entity,
                    priority: tessera_id.priority(),
                    _pad: 0,
                }
            })
            .collect();

        // The fixture must actually exercise a prefix tie, or this test would prove nothing:
        // 4000 rows over a 16-bit prefix puts us well past the birthday bound.
        let mut priorities: Vec<u16> = rows.iter().map(|r| r.priority).collect();
        priorities.sort_unstable();
        assert!(
            priorities.windows(2).any(|w| w[0] == w[1]),
            "fixture must contain at least one priority-prefix tie"
        );

        let mut via_comparator = rows.clone();
        via_comparator.sort_by(|a, b| a.cmp(b, &key, shard));

        let mut naive = rows;
        naive.sort_by_key(|r| {
            key.forward(shard, EntityId::new(r.entity as u64))
                .unwrap()
                .raw()
        });

        let via_comparator_entities: Vec<u32> = via_comparator.iter().map(|r| r.entity).collect();
        let naive_entities: Vec<u32> = naive.iter().map(|r| r.entity).collect();
        assert_eq!(
            via_comparator_entities, naive_entities,
            "the prefix-then-recompute comparator must agree with a full tessera_id sort"
        );
    }
}
