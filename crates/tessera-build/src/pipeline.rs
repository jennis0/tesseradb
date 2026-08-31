//! The streaming batch build: the same bundle [`crate::build_in_memory`] produces, computed
//! within a bounded working set.
//!
//! ## Why this exists
//!
//! The linear build materialises one struct per point (each carrying its own heap-allocated
//! signature) and the whole `term -> entity list` relation as a `Vec<Vec<u32>>` before it writes
//! a byte. At the probe corpus's 10⁹ items that is tens of gigabytes of small allocations and it
//! was OOM-killed on a 47 GiB box during staging, before any output existed.
//!
//! ## The construction
//!
//! Every intermediate here is a **flat, packed array indexed by an integer**, anything
//! recomputable from the input parquet is recomputed rather than retained, and anything whose
//! size scales with the corpus rather than with the budget either lives behind the plan's
//! arithmetic or spills ([`spill`]). The passes:
//!
//! | pass | produces | bounded by |
//! |---|---|---|
//! | points ×2 | `source_ids`, sorted — an item's **ordinal** is its index here | 8N |
//! | pairs ×1 | dictionary (streamed), term-lookup arrays, pre-dedup `row_counts`, histogram | 12T + 8T |
//! | pairs ×1 | resolved `ordinal << 32 \| term` per **batch bucket** (RAM when it fits, spilled otherwise) | plan |
//! | per batch | sort+dedup the bucket; per-ordinal `starts`; signature sort + refinement; entity ids; **band emit** | plan |
//! | per band | cursor-scatter (already sorted), Roaring-encode in sub-chunks, spool + `pairs.parquet` | plan |
//! | — | postings assembly: the spool becomes `postings.arrow`'s one record batch, zero-copy | 8(T+1) |
//! | points ×1 | external ids (when minting) | 20N |
//! | points ×1 | geometry, the tiler sort, and the segment | 28N |
//!
//! ## Batch-scoped signature assignment (§11.1)
//!
//! Entity ids are assigned by signature order **within each batch and only within one** — the
//! design's own scope for the sort (I10's per-batch clause; the serving allocator's group
//! commit is the same mechanism at window scale). A batch is a contiguous ordinal range of
//! `batch_items` stride; ids are batch-major, so per-term posting lists stay globally
//! ascending as the concatenation of per-batch runs, and one batch covering the corpus
//! reproduces the historical global sort byte for byte. The batch size is **identity-bearing**
//! (I9): derived deterministically from the memory budget (largest feasible, on a coarse
//! grid), recorded in MANIFEST provenance whenever the build actually batches, replayed — not
//! re-derived — by identity-preserving rebuilds, and refused when a stated size would
//! needlessly fragment posting runs the budget could have kept whole.
//!
//! ## The spill discipline
//!
//! Bucket, band and spool files live under `<out>/.build-tmp/`, are deleted as consumed and
//! on every exit path, and each carries a write-side `(count, mix64-sum)` receipt verified on
//! read — the cross-pass anchor discipline extends across every spill boundary. Bands arrive
//! with each term's entities strictly ascending by construction (bases ascend across batches,
//! positions within one), which `encode_posting`'s unconditional sortedness check re-verifies
//! from disk: a corrupted or reordered spill fails the build, never bends a posting.
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
//! The other parallel shape here is **one lane per declared column** — the attribute join stages,
//! scatters and tallies each column on a thread of its own
//! ([`read_one_attribute_source`]). It participates in no ordering decision either, and for a
//! stronger reason than the sorts do: the lanes never meet. Each column is its own mapped array
//! with its own presence bits and its own arena, indexed by entity, so no two lanes can name the
//! same byte; the only shared state is read-only (the join's answer, the declaration) and the
//! only shared *result* is the coverage tally, which is returned per lane and folded in
//! declaration order rather than accumulated across threads. Minting stays where it was — a
//! serial pre-pass per decoded batch, in file order — so the vocabulary codes a build assigns are
//! not a function of how the lanes were scheduled.
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
//! passthrough's label rule is *decomposable*: an item's descriptors are its source terms taken
//! one at a time, so a term's descriptor can be derived from the term alone and the whole item
//! never has to be assembled. That is a property of passthrough, not of the plugin
//! ABI — a plugin that derived descriptors from the item's terms as a whole would be mislabelled by
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
//!   (the overwhelming majority — the probe corpus averages 1.72). Only groups that tie on
//!   that key *and* contain a longer signature are refined by a full signature comparison. The
//!   pre-sort key is order-consistent with the lexicographic signature order, so refining within
//!   ties reproduces it exactly.
//! * **The postings order.** Entity lists come out of a term-bucketed scatter and are sorted per
//!   term, which is the same ascending list the linear build accumulates by walking items in
//!   entity order.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use croaring::Bitmap;
use rayon::prelude::*;
use rustc_hash::FxHashMap;

use tessera_authz::encode_posting;
use tessera_filter::{
    Codes, ColumnKind, RecordField, RecordValue, ValueColumnWriter, RECORD_BLOCKS_FILE,
    RECORD_BLOCK_TARGET, RECORD_DIRECTORY_FILE, RECORD_HASROW_FILE,
};
use tessera_filter_write::RecordBlobWriter;
use tessera_plugin::{Passthrough, Plugin};
use tessera_spatial::split32;
use tessera_spatial::tiler::{ScalarType, ScalarValue};
use tessera_store::write::{
    write_columns, write_morton_codes, write_permutation_iter, ScalarColumn,
};
use tessera_types::{EntityId, IdentityKey, SMALL_TERM_THRESHOLD_DEFAULT};

use crate::column::EntityColumn;
use crate::error::{BuildError, Result};
use crate::input;
use crate::observer::{BuildObserver, BuildStage, StageTimer};
use crate::spill;
use crate::{
    fsync_file, validate_args, write_ext_locator, write_external_id_runs, write_manifests,
    BuildArgs, BuildReport, BundleFiles, ExternalIdRow, PairsParquetWriter,
    EXTERNAL_ID_ROWS_PER_EXTENT, PHASH, PREFIX, SEG_ID,
};

/// One item's position in the signature sort. 16 bytes, four-byte aligned — every field is a
/// component of the sort key, and holding the key in the record is what keeps the comparator a
/// pure function of it.
///
/// **The `morton` field is [decision 0073](../../../docs/decisions/0073-entity-ties-are-ordered-by-morton-code.md)**:
/// ties within a signature group order by the item's Morton **code**, so a spatially coherent set
/// lands in a contiguous run of entity ids (*measured* 4.08× on artifact membership's disk form,
/// with term postings byte-identical). It is the same `split32` cell code the tiler ranks rows by,
/// so the two orders agree on what "nearby" means.
///
/// **The record grew from 12 bytes to hold it, and [`plan_build`]'s residency model was widened to
/// match** — a batch is sized against the memory budget, so an unwidened model would plan a batch
/// it cannot hold. ⊘ The alternative — keeping 12 bytes and reading the code out of the mapped
/// `morton-of-ordinal.u32` inside the comparator — trades that anonymous 4 B/item for an
/// indirection per comparison, and is **unmeasured**: it is the layout question the decision
/// leaves to this stage.
#[derive(Clone, Copy)]
#[repr(C)]
struct SortRec {
    key_hi: u32,
    key_lo: u32,
    morton: u32,
    ordinal: u32,
}

impl SortRec {
    fn order(&self) -> (u32, u32, u32, u32) {
        (self.key_hi, self.key_lo, self.morton, self.ordinal)
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

/// The attribute join's staging buffer, **in bytes**.
///
/// A row count is the wrong unit for this buffer, and at corpus scale it stops being a bound at
/// all: [`JOIN_CHUNK_ROWS`] is 67,108,864, so a 73,631,092-point corpus staged 91% of itself in one
/// chunk — a near-complete second copy of every column the source carries, two flushes, and none of
/// the chunking the sweep is chunked for. Sized in bytes the same buffer is a constant the schema
/// cannot inflate: a wider schema takes fewer rows per chunk and more chunks, which is what a
/// staging buffer is supposed to do.
///
/// **Chunk boundaries are unobservable in the output**, which is what makes this number free to
/// choose. Source ids are duplicate-checked before this pass, so no entity is written twice;
/// [`join_chunk`]'s own contract leaves the order *within* a chunk unspecified; and vocabulary
/// codes are minted in `scan_attributes`'s per-batch pre-pass, driven by parquet batch order and
/// not by this. Nothing identity-bearing — entity ids, ordinals, minting order — is a function of
/// where a chunk ends.
///
/// What it bounds is the headers: [`staging_rows`] prices a row at the join key plus each column's
/// fixed width, and a string's *contents* ride on top of that, so a column of long prose overshoots
/// this figure by whatever it averages per value. **Since the columns were mapped (`column.rs`)
/// what it bounds is a file rather than the heap** — page cache the kernel may reclaim, not memory
/// the machine must have.
const JOIN_STAGE_BYTES: usize = 256 << 20;

/// How many rows of `attributes` fit in [`JOIN_STAGE_BYTES`], at least one and never more than the
/// corpus.
fn staging_rows(attributes: &[&crate::config::Attribute], n: u64) -> usize {
    // The join key beside each staged row — `(source_id, pos)`, 16 bytes and not the 12 an earlier
    // comment claimed — the 8 bytes the sweep's answer takes beside it (`(entity, pos)`), and one
    // typed slot per column. Each column's presence bit adds an eighth of a byte per row on top,
    // which is left out rather than rounded up to a whole one.
    let per_row: usize = 24 + attributes.iter().map(|a| staged_width(a.ty)).sum::<usize>();
    (JOIN_STAGE_BYTES / per_row).clamp(1, n.max(1) as usize)
}

/// One staged slot's width, as the budget above prices it.
///
/// ⊘ **The string families are priced at the 24-byte `String` header they no longer cost** — a
/// staged string slot is an 8-byte arena offset now, and the characters are the arena's
/// (`column.rs`). The figure stands where it is: it makes the buffer smaller than the budget rather
/// than larger, and moving it moves every chunk boundary.
fn staged_width(ty: ScalarType) -> usize {
    match ty {
        ScalarType::Bool | ScalarType::U8 | ScalarType::I8 => 1,
        ScalarType::U16 | ScalarType::I16 => 2,
        ScalarType::U32 | ScalarType::I32 | ScalarType::F32 => 4,
        ScalarType::U64 | ScalarType::I64 | ScalarType::F64 | ScalarType::TimestampUs => 8,
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => std::mem::size_of::<String>(),
    }
}

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

/// Resolve a chunk of `(source_id, source_term)` rows to packed `ordinal << 32 | term` values
/// by **two sorted merges** — the ordinal join ([`join_chunk`]) and then a term-sorted sweep
/// over the dictionary's `term_keys` — never a per-row probe: at T = 117M a per-row hash or
/// binary-search lookup is a random walk over gigabytes, which is the cost class this pipeline
/// exists to avoid. `resolved` is caller-owned scratch, reused across chunks.
///
/// Chunk-order insensitivity: `emit` receives values in term-sorted chunk order; every consumer
/// sorts or is commutative, so no order is observable downstream.
fn resolve_pairs_chunk(
    chunk: &mut Vec<(u64, u64)>,
    resolved: &mut Vec<(u64, u64)>,
    source_ids: &[u64],
    term_keys: &[u64],
    term_ids: &[u32],
    distinct_of_ordinal: &mut [u32],
    mut emit: impl FnMut(u64) -> Result<()>,
) -> Result<()> {
    resolved.clear();
    join_chunk(chunk, source_ids, |ordinal, source_id, source_term| {
        // Established by the dictionary pass over this same file; a miss means the file is not
        // the one that pass read.
        let Some(ordinal) = ordinal else {
            return Err(input_changed(&format!(
                "the pairs file names entity {source_id}, which its first pass did not"
            )));
        };
        resolved.push((source_term, ordinal as u64));
        Ok(())
    })?;
    // Equal (term, ordinal) tuples are bit-identical, so the parallel unstable sort has one
    // output; the sweep cursor then only moves forward.
    resolved.par_sort_unstable();
    // **How many distinct terms this chunk gave each ordinal**, counted here because this is the
    // one place they are sorted and the chunk holds exactly one view's rows (`views.md` §7). The
    // label-agreement refusal in the batch loop is a count identity over these tallies; the emit
    // below is deliberately *not* deduplicated, the bucket sweep doing that and the pair total
    // being checked against the dictionary pass's own row count.
    let mut previous: Option<(u64, u64)> = None;
    for &pair in resolved.iter() {
        if previous != Some(pair) {
            let slot = &mut distinct_of_ordinal[pair.1 as usize];
            *slot = slot.saturating_add(1);
            previous = Some(pair);
        }
    }
    let mut i = 0usize;
    for &(source_term, ordinal) in resolved.iter() {
        while i < term_keys.len() && term_keys[i] < source_term {
            i += 1;
        }
        if i >= term_keys.len() || term_keys[i] != source_term {
            return Err(input_changed(&format!(
                "the pairs file names term {source_term}, which its first pass did not"
            )));
        }
        emit((ordinal << 32) | term_ids[i] as u64)?;
    }
    resolved.clear();
    Ok(())
}

/// Where resolved pairs accumulate before the batch loop: in RAM when the plan proved the
/// whole relation fits (the historical single-batch path — same allocation, same lifecycle),
/// spilled to one file per batch otherwise.
enum BucketSink {
    Ram(Vec<u64>),
    Files {
        writers: Vec<spill::SpillWriter>,
        batch_items: u64,
    },
}

impl BucketSink {
    fn push(&mut self, value: u64) -> Result<()> {
        match self {
            BucketSink::Ram(vec) => {
                vec.push(value);
                Ok(())
            }
            BucketSink::Files {
                writers,
                batch_items,
            } => {
                let batch = ((value >> 32) / *batch_items) as usize;
                writers[batch].push(value)
            }
        }
    }

    fn finish(self) -> Result<BucketStore> {
        match self {
            BucketSink::Ram(vec) => Ok(BucketStore::Ram(Some(vec))),
            BucketSink::Files { writers, .. } => Ok(BucketStore::Files(
                writers
                    .into_iter()
                    .map(|w| w.finish().map(Some))
                    .collect::<Result<_>>()?,
            )),
        }
    }
}

enum BucketStore {
    Ram(Option<Vec<u64>>),
    Files(Vec<Option<spill::SpillReceipt>>),
}

impl BucketStore {
    /// Batch `k`'s packed values, moved out (RAM) or read and integrity-verified (file).
    fn load(&mut self, k: u64) -> Result<Vec<u64>> {
        match self {
            BucketStore::Ram(slot) => {
                debug_assert_eq!(k, 0, "the RAM backing exists only for a single batch");
                slot.take()
                    .ok_or_else(|| BuildError::Invalid("bucket 0 loaded twice".into()))
            }
            BucketStore::Files(receipts) => {
                let receipt = receipts[k as usize]
                    .as_ref()
                    .ok_or_else(|| BuildError::Invalid(format!("bucket {k} loaded twice")))?;
                spill::read_bucket(receipt)
            }
        }
    }

    /// Release batch `k`'s backing (deletes the spill file; no-op for RAM, whose vector was
    /// moved out by `load`).
    fn delete(&mut self, k: u64) -> Result<()> {
        if let BucketStore::Files(receipts) = self {
            if let Some(receipt) = receipts[k as usize].take() {
                std::fs::remove_file(&receipt.path)
                    .map_err(|e| BuildError::io(&receipt.path, e))?;
            }
        }
        Ok(())
    }
}

/// Everything the memory budget decides, decided once and printed. See `BuildArgs::batch_items`
/// for why the batch size is derived deterministically and recorded rather than re-derived.
struct BuildPlan {
    /// The memory budget this plan was derived under — the operator's `--memory-budget` or the
    /// detected one. Carried rather than re-detected, so every stage that sizes itself against the
    /// budget sizes against the *same* number: `detect_memory_budget` reads `MemAvailable`, which
    /// falls as the build fills memory, and a second reading late in the run would derive a
    /// smaller budget from the build's own success at using the first.
    budget: u64,
    batch_items: u64,
    batches: u64,
    bucket_in_ram: bool,
    /// Term-id band ranges `[lo, hi)`, covering `0..term_count`, each band's pre-dedup rows
    /// within the flat budget (a term never splits across bands).
    band_bounds: Vec<(u32, u32)>,
    /// What provenance records: the batch size iff the build actually batched.
    recorded_batch_items: Option<u64>,
}

/// The budget when none is given: the smaller of MemAvailable and this process's cgroup limit,
/// damped to leave room for the page cache and the allocator's slack; clamped so a tiny CI box
/// still gets a workable floor.
///
/// **Both sources, and the smaller wins, because either can be the real bound.** `MemAvailable` is
/// the kernel's estimate of what an allocation could get on the *host*; a cgroup v2 `memory.max` is
/// the ceiling this process is killed at, and it charges page cache against itself. A build inside
/// a 4 GiB container on a 48 GiB machine that reads only the first sizes its batches for 38 GiB and
/// is OOM-killed by the limit that always owned the answer — which is the failure this crate exists
/// to prevent, arriving through the detector rather than through the batch size.
pub(crate) fn detect_memory_budget() -> u64 {
    const FALLBACK: u64 = 24 << 30;
    let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") else {
        return FALLBACK;
    };
    let Some(available) = meminfo
        .lines()
        .find(|l| l.starts_with("MemAvailable:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u64>().ok())
        .map(|kib| kib * 1024)
    else {
        return FALLBACK;
    };
    // "max" when unlimited, which parses to `None` and leaves `MemAvailable` as the answer — the
    // same result as no cgroup at all.
    let limit = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|cgroup| {
            let path = cgroup
                .lines()
                .next()?
                .split(':')
                .nth(2)?
                .trim_start_matches('/');
            std::fs::read_to_string(format!("/sys/fs/cgroup/{path}/memory.max")).ok()
        })
        .and_then(|max| max.trim().parse::<u64>().ok());
    let bound = limit.map_or(available, |limit| limit.min(available));
    ((bound as f64 * 0.8) as u64).clamp(2 << 30, 1 << 40)
}

/// Free bytes on the filesystem holding `path`, or `None` where unknowable — the disk
/// pre-flight then simply does not run, rather than refusing builds on a guess.
#[cfg(unix)]
fn available_disk(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    Some(stats.f_bavail as u64 * stats.f_frsize as u64)
}

#[cfg(not(unix))]
fn available_disk(_path: &std::path::Path) -> Option<u64> {
    None
}

/// The auto-batching grid: derived batch sizes are multiples of 2^24 items, so small budget
/// differences between machines derive the same size — an accidental identity fork needs a
/// budget step of a whole grid cell, not a few megabytes.
const BATCH_GRID: u64 = 1 << 24;

fn plan_build(
    args: &BuildArgs,
    n: u64,
    pair_rows: usize,
    row_counts: &[u64],
    histogram: &[u64],
    histogram_shift: u32,
) -> Result<BuildPlan> {
    let budget = args.memory_budget.unwrap_or_else(detect_memory_budget);

    // ---- the other peak: everything the batch loop below does not hold ----------------------
    // **The batch stride reaches none of it** (`residency.rs`): a column in entity order is `n`
    // values by construction and a member table is its own size, so there is no smaller plan to
    // fall back to and the honest answer is a refusal with the arithmetic printed. This is the half
    // `--memory-budget` did not reach — the campaign's builds were OOM-killed at 47.3–47.6 GB under
    // a 12 GB budget, three runs and one number, because the flag only ever sized the loop.
    //
    // Checked **before** the batch plan, and refused rather than warned: this is the larger term
    // and the one no stride can move, so an operator reading a refusal should read this one first.
    // The plan below already refuses an infeasible stride, and a budget the operator named is a
    // bound they asked to have enforced. An auto-derived budget is `MemAvailable` damped, so exceeding *that* is the kill
    // this exists to replace.
    let tail = crate::residency::model(args, n);
    if tail.total() > budget {
        return Err(BuildError::Invalid(format!(
            "this build's entity-order stages need about {} MiB, over the {} MiB memory budget. \
             Unlike the signature batch loop these do not batch — a declared column holds one \
             value per item and a member table holds its own rows — so a smaller --batch-items \
             does not help. Raise --memory-budget, drop a member source, or narrow the schema. \
             Where the bytes are:{}",
            tail.total() >> 20,
            budget >> 20,
            tail.describe()
        )));
    }
    // **A warning where the model's own error bar reaches the budget**, and not a refusal:
    // `residency.rs` is a lower bound — it enumerates what the stages hold and not what the
    // Parquet readers, the analysers and the allocator hold around them, and the one build it was
    // measured against read 190 MiB against a 407 MiB peak. Refusing on twice the model would
    // block builds that fit; saying nothing leaves the operator with the same silence the campaign
    // met. So the numbers are printed and the decision is theirs — the house rule for a thing that
    // is recoverable and discloses nothing.
    else if tail.total().saturating_mul(2) > budget {
        eprintln!(
            "warning: this build's entity-order stages need at least {} MiB against a {} MiB \
             budget, and that figure is a lower bound — it counts what the stages hold, not the \
             readers and allocator around them (measured at roughly half the real peak). These \
             stages do not batch. Where the bytes are:{}",
            tail.total() >> 20,
            budget >> 20,
            tail.describe()
        );
    }

    // The worst batch's pre-dedup pairs for stride `b`, bounded by summing every histogram
    // range a batch window overlaps — conservative by at most the two boundary ranges.
    let prefix: Vec<u64> = std::iter::once(0)
        .chain(histogram.iter().scan(0u64, |acc, &v| {
            *acc += v;
            Some(*acc)
        }))
        .collect();
    let worst_pairs = |b: u64| -> u64 {
        let mut worst = 0u64;
        let mut lo = 0u64;
        while lo < n {
            let hi = (lo + b).min(n);
            let r_lo = (lo >> histogram_shift) as usize;
            let r_hi = (((hi - 1) >> histogram_shift) as usize + 1).min(histogram.len());
            worst = worst.max(prefix[r_hi] - prefix[r_lo]);
            lo = hi;
        }
        worst
    };
    // The batch loop's residency model, stated so a refusal can print it: the batch's packed
    // bucket (8 bytes/pair) + recs (16/item — 12 before decision 0073 added the Morton tiebreak
    // to the sort key, and a model left at 12 would plan a batch the loop cannot hold) +
    // starts (4/item) + long bitset (1/8 per item),
    // beside the loop-wide entity map (4/item over all n), per-term counters (4/term), the
    // join-chunk buffer (which scales down with the corpus, so a tiny test budget stays
    // feasible for a tiny corpus) and a fixed slack for band buffers, decoders and allocator.
    const SLACK: u64 = 64 << 20;
    let chunk_bytes = 16 * (JOIN_CHUNK_ROWS as u64).min(pair_rows.max(1) as u64);
    let loop_fixed = 4 * n + 4 * row_counts.len() as u64 + chunk_bytes + SLACK;
    let per_batch = |b: u64| 8 * worst_pairs(b) + 16 * b + 4 * (b + 1) + b / 8;
    let feasible = |b: u64| per_batch(b).saturating_add(loop_fixed) <= budget;

    // The largest feasible stride on the grid (or the whole corpus). If even one grid cell is
    // infeasible the corpus cannot be built under this budget, and the refusal states the
    // arithmetic rather than thrashing.
    let auto_batch = if feasible(n) {
        n
    } else {
        let mut b = (n / BATCH_GRID).saturating_mul(BATCH_GRID).max(BATCH_GRID);
        while b > BATCH_GRID && !feasible(b) {
            b -= BATCH_GRID;
        }
        if !feasible(b) {
            return Err(BuildError::Invalid(format!(
                "no feasible signature batch under the {budget}-byte memory budget: even \
                 {BATCH_GRID} items need {} bytes beside the {loop_fixed}-byte loop floor \
                 (n = {n}, pairs = {pair_rows}); raise --memory-budget",
                per_batch(BATCH_GRID)
            )));
        }
        b
    };

    let batch_items = match args.batch_items {
        None => auto_batch,
        Some(b) if b >= n => n,
        Some(b) => {
            if !feasible(b) {
                return Err(BuildError::Invalid(format!(
                    "--batch-items {b} needs {} bytes beside the {loop_fixed}-byte loop \
                     floor, over the {budget}-byte budget; the largest feasible batch is \
                     {auto_batch}",
                    per_batch(b)
                )));
            }
            // Needlessly small batches permanently forfeit posting compression (I9; §11.1
            // §11.1's container model): refuse unless the budget itself is the reason. An
            // operator who wants small batches states the matching budget, which makes the
            // choice deliberate and reproducible.
            if b.saturating_mul(2) < auto_batch {
                return Err(BuildError::Invalid(format!(
                    "--batch-items {b} is far below the {auto_batch} the budget supports; \
                     smaller batches permanently fragment posting runs (§11.1 r23). Pass a \
                     larger --batch-items, or lower --memory-budget to make this size the \
                     derived choice"
                )));
            }
            b
        }
    };
    let batches = n.div_ceil(batch_items);

    // The RAM backing needs the WHOLE relation beside the resolve scan's own residents.
    let bucket_in_ram =
        batches == 1 && (8 * pair_rows as u64).saturating_add(8 * n + loop_fixed) <= budget;

    // Bands: pre-dedup row counts partition term space (post-dedup <= pre-dedup, so a band's
    // flat is always large enough); the floor is the largest single term, which can never
    // split. Flat budget: a quarter of the memory budget at 4 bytes per row, unless the
    // explicit band-size seam overrides it (a corpus small enough to test cannot otherwise
    // force more than one band).
    let max_term_rows = row_counts.iter().copied().max().unwrap_or(0);
    let band_rows_budget = args
        .band_rows
        .unwrap_or(budget / 16)
        .max(max_term_rows)
        .max(1);
    let mut band_bounds: Vec<(u32, u32)> = Vec::new();
    let mut lo = 0u32;
    let mut acc = 0u64;
    for (term, &rows) in row_counts.iter().enumerate() {
        if acc + rows > band_rows_budget && term as u32 > lo {
            band_bounds.push((lo, term as u32));
            lo = term as u32;
            acc = 0;
        }
        acc += rows;
    }
    band_bounds.push((lo, row_counts.len() as u32));

    // Disk pre-flight (fail-closed): the build's transient spills and its outputs coexist in
    // phases; refuse up front, with the arithmetic, rather than dying on ENOSPC hours in. The
    // four phase peaks, all conservative: buckets full beside the first batch's bands; bands
    // full beside the postings spool; the declared columns beside the text index's runs; the
    // spool becoming postings.arrow beside the segment. (P here is pre-dedup pairs; band/spool
    // bytes-per-pair are stated ceilings for the varint codec and Roaring postings, not
    // measurements of this corpus.)
    //
    // **The column phase is a whole window, not a moment.** Every declared column in entity
    // order is a mapped file from the attribute join to the release five stages later
    // (`column.rs`), and the text index spills its runs inside that window — so those bytes are
    // on the disk together, and they are on it while the geometry maps still are. The other three
    // phases all end before the attribute join opens it. The column figure is `residency.rs`'s
    // own, reused rather than re-derived: a second copy of that arithmetic is how this stops
    // being true again.
    let p = pair_rows as u64;
    let phase_spill = if bucket_in_ram { 0 } else { 8 * p } + (6 * p) / batches.max(1);
    let phase_bands = 6 * p + 4 * p;
    // 8 B/item of `x-of-entity`/`y-of-entity`, which outlive the release; 4 B/item of pairs
    // already written as postings.arrow before the window opened.
    let phase_columns = tail.mapped() + 8 * n + 4 * p;
    // The segment write: the spool becoming postings.arrow beside the segment, and the row-order
    // attribute tail beside both — one mapped file per render column, built here and unlinked with
    // the record batch that reads it (`residency::render_tail_bytes`).
    let phase_assemble = 4 * p + 26 * n + crate::residency::render_tail_bytes(&args.schema, n);
    let disk_need = phase_spill
        .max(phase_bands)
        .max(phase_columns)
        .max(phase_assemble);
    if let Some(free) = available_disk(&args.out) {
        if free < disk_need {
            return Err(BuildError::Invalid(format!(
                "insufficient disk for this build: ~{disk_need} bytes needed at peak \
                 (spill phase {phase_spill}, band phase {phase_bands}, column phase \
                 {phase_columns}, assembly phase {phase_assemble}; n = {n}, pairs = {p}, \
                 batches = {batches}), {free} available at the output path; free disk and \
                 retry. Where the column phase's bytes are:{}",
                tail.describe()
            )));
        }
    }

    Ok(BuildPlan {
        budget,
        batch_items,
        batches,
        bucket_in_ram,
        band_bounds,
        recorded_batch_items: (batches > 1).then_some(batch_items),
    })
}

pub(crate) fn build(args: &BuildArgs, observer: &dyn BuildObserver) -> Result<BuildReport> {
    validate_args(args)?;
    let mut timer = StageTimer::new(observer);
    let plugin = Passthrough::new();
    require_decomposable_labelling(&plugin)?;
    let bounds = plugin.declared_bounds();

    // ---- 1. pass one: entity space, once over every view's points (`views.md` §7) -----
    // An item's *ordinal* is its index in this array. Ordinal order is source-id order over the
    // **union** of every view's ids, which is the order the linear build walks items in — so
    // "first appearance" below, and the source-id tiebreak in the signature sort, are both
    // expressible as ordinal comparisons, exactly as they were when a build read one file.
    let (source_ids, view_anchors) = read_source_ids_union(args)?;
    if source_ids.is_empty() {
        return Err(BuildError::Invalid(
            "no points selected — a bundle with no items has no expressible entity range".into(),
        ));
    }
    let n = source_ids.len() as u64;
    if n > u32::MAX as u64 {
        return Err(BuildError::Invalid(format!(
            "{n} items exceeds bundle_format 1's 2^32 entity-ID ceiling"
        )));
    }
    // The union's extrema, for step 8c's dense fast path. Each view's own anchors — its row count
    // and an order-independent mixed sum of its ids ([`mix64`]) — are in `view_anchors`, and the
    // geometry pass below is checked against them: a points file swapped mid-build would
    // otherwise hand every item of that view another item's position, with nothing to notice.
    let (ids_first, ids_last) = (source_ids[0], *source_ids.last().expect("non-empty"));

    timer.end(BuildStage::SourceIds, source_ids.len() as u64);

    // ---- 2. the dictionary -----------------------------------------------------------
    let dict_dir = args.out.join(PREFIX).join("dictionary");
    std::fs::create_dir_all(&dict_dir).map_err(|e| BuildError::io(&dict_dir, e))?;
    // What every source term is called, established before any term id exists — a field-sourced
    // view's sorted vocabulary, or the relation's own integers (`crate::AccessPlan`).
    let access = crate::plan_access(args)?;
    // Whether the label-agreement identity applies at all: a shared relation is entity space and
    // is scanned once, so its rows cannot disagree between views (`crate::AccessRoute`).
    let per_view_labels = !matches!(access.descriptors, crate::input::TermDescriptors::Ids);
    let Dictionary {
        term_keys,
        term_ids,
        row_counts,
        term_count,
        pair_rows,
        histogram,
        histogram_shift,
        dict_paths,
    } = build_dictionary(args, &access, &source_ids, &dict_dir)?;
    if term_count >= u32::MAX as u64 {
        return Err(BuildError::Invalid(format!(
            "{term_count} distinct terms exceeds the 2^32 term-ID space"
        )));
    }

    timer.end(BuildStage::Dictionary, term_count);

    // ---- 2b. the plan: batch size, bucket backing, band boundaries, pre-flight --------
    // Everything the budget arithmetic decides, decided in one place and printed — the batch
    // size is identity-bearing (I9), so it is derived deterministically here, recorded in
    // provenance when it batches, and never silently re-derived on a rebuild (the CLI replays
    // a carried bundle's recorded value).
    let plan = plan_build(args, n, pair_rows, &row_counts, &histogram, histogram_shift)?;
    drop(histogram);
    if plan.batches > 1 {
        eprintln!(
            "batching: {} batches of <= {} items (signature order is per-batch — \u{a7}11.1 r23; \
             recorded in provenance; a rebuild preserving this identity must replay it)",
            plan.batches, plan.batch_items
        );
    }

    // ---- 3. the pairs relation, packed as `ordinal << 32 | term_id`, into buckets -----
    // One bucket per batch — in RAM when the plan says the whole relation fits (the historical
    // single-batch path, bit for bit), spilled per batch otherwise. Chunk-order insensitivity
    // ([`join_chunk`]): every bucket is sorted and deduplicated before anything reads it, so
    // the push order never reaches the output.
    let tmp = spill::TmpDir::create(&args.out)?;
    let mut sink = if plan.bucket_in_ram {
        BucketSink::Ram(Vec::with_capacity(pair_rows))
    } else {
        let mut writers = Vec::with_capacity(plan.batches as usize);
        for k in 0..plan.batches {
            writers.push(spill::SpillWriter::create(
                &tmp.path().join(format!("bucket-{k}.u64")),
            )?);
        }
        BucketSink::Files {
            writers,
            batch_items: plan.batch_items,
        }
    };
    let mut pushed = 0u64;
    let mut failure: Option<BuildError> = None;
    let mut chunk: Vec<(u64, u64)> = Vec::with_capacity(JOIN_CHUNK_ROWS.min(pair_rows.max(1)));
    let mut resolved: Vec<(u64, u64)> = Vec::new();
    // Each view's own distinct contribution per ordinal, summed over the views — the numerator
    // of the label-agreement identity the batch loop checks (`views.md` §7).
    let mut distinct_of_ordinal: Vec<u32> = vec![0; n as usize];
    let mut resolve = |chunk: &mut Vec<(u64, u64)>,
                       resolved: &mut Vec<(u64, u64)>,
                       distinct_of_ordinal: &mut [u32],
                       sink: &mut BucketSink| {
        resolve_pairs_chunk(
            chunk,
            resolved,
            &source_ids,
            &term_keys,
            &term_ids,
            distinct_of_ordinal,
            |value| {
                pushed += 1;
                sink.push(value)
            },
        )
    };
    let mut current: Option<(usize, u64)> = None;
    crate::scan_access(args, &access, |view, source_id, source_term| {
        // A chunk **never spans two views**, and never splits a row: rows of one view arrive
        // contiguously (a view holds one row per entity), so the boundary is taken at the change
        // of view — always — or at the next change of entity once the chunk is full.
        let changed_view = current.is_some_and(|(previous, _)| previous != view);
        let changed_row = current != Some((view, source_id));
        if changed_view || (changed_row && chunk.len() >= JOIN_CHUNK_ROWS) {
            if let Err(e) = resolve(
                &mut chunk,
                &mut resolved,
                &mut distinct_of_ordinal,
                &mut sink,
            ) {
                failure = Some(e);
                return ControlFlow::Break(());
            }
        }
        current = Some((view, source_id));
        chunk.push((source_id, source_term));
        ControlFlow::Continue(())
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    resolve(
        &mut chunk,
        &mut resolved,
        &mut distinct_of_ordinal,
        &mut sink,
    )?;
    drop(chunk);
    drop(resolved);
    if pushed as usize != pair_rows {
        return Err(input_changed(&format!(
            "the pairs file yielded {pair_rows} rows, then {pushed}"
        )));
    }
    // ---- 3b. geometry, by ordinal, once per (item, view) (`views.md` §7) --------------
    // **Every view's geometry is read exactly once, and it is read here** — before entity ids
    // exist, so it lands in *ordinal* space. Pass two permutes it into entity space rather than
    // re-reading a parquet file per view: the transform (project, then quantise against that
    // view's own frame) runs once per (item, view) and nowhere else.
    //
    // The pass exists because decision 0073 breaks signature ties on the Morton code, so the sort
    // needs geometry it previously did not, and decision 0112 says *which* view's: the declared
    // anchor's, with the first-declared view that holds the item standing in where the anchor
    // does not.
    //
    // **Mapped rather than heap-allocated**, on step 8's own argument: 4 B per item per axis is
    // 8 GB at 10⁹ of memory the kernel cannot reclaim. See [`spill::MappedU32`] — the bytes become
    // page cache, so a machine short of RAM pages instead of failing. They are deliberately outside
    // [`plan_build`]'s residency model, which counts the anonymous allocations a batch must hold.
    //
    // The two integrity checks are the ones step 8 used to make, moved with the read: a shrunk file
    // resolves every id it still presents and would otherwise leave the missing items at (0, 0)
    // with no error, and a count alone accepts a repeat that compensates a removal ({1,2,3} become
    // {2,2,2}), so the multiset of ids must be that view's first pass's.
    let mut geometry: Vec<ViewGeometry> = Vec::with_capacity(args.views.len());
    // How many of this build's views hold each item — the denominator of the label-agreement
    // identity below, and the population of each view's permutation.
    let mut appearances: Vec<u32> = vec![0; n as usize];
    for (index, view) in args.views.iter().enumerate() {
        let mut x_map =
            spill::MappedU32::zeroed(tmp.path(), &format!("x-of-ordinal-{index}.u32"), n as usize)?;
        let mut y_map =
            spill::MappedU32::zeroed(tmp.path(), &format!("y-of-ordinal-{index}.u32"), n as usize)?;
        let mut present: Vec<u64> = vec![0; (n as usize).div_ceil(64)];
        {
            let xs = x_map.as_mut_slice();
            let ys = y_map.as_mut_slice();
            let mut points_seen = 0u64;
            let mut geom_anchor = 0u64;
            let mut chunk: Vec<(u64, (u32, u32))> =
                Vec::with_capacity(JOIN_CHUNK_ROWS.min(n as usize));
            let mut failure: Option<BuildError> = None;
            let resolve = |chunk: &mut Vec<(u64, (u32, u32))>,
                           xs: &mut [u32],
                           ys: &mut [u32],
                           present: &mut [u64],
                           appearances: &mut [u32],
                           points_seen: &mut u64,
                           geom_anchor: &mut u64| {
                join_chunk(chunk, &source_ids, |ordinal, source_id, (x, y)| {
                    let Some(ordinal) = ordinal else {
                        return Err(input_changed(&format!(
                            "the points file names entity {source_id}, which its first pass did \
                             not"
                        )));
                    };
                    xs[ordinal as usize] = x;
                    ys[ordinal as usize] = y;
                    bit_set(present, ordinal as usize);
                    appearances[ordinal as usize] += 1;
                    *points_seen += 1;
                    *geom_anchor = geom_anchor.wrapping_add(mix64(source_id));
                    Ok(())
                })
            };
            input::scan_points(
                &view.points,
                &view.point_fields,
                view.projection,
                &view.extent,
                args.limit,
                view.select.as_ref(),
                |point| {
                    chunk.push((point.source_id, (point.qx, point.qy)));
                    if chunk.len() == JOIN_CHUNK_ROWS {
                        if let Err(e) = resolve(
                            &mut chunk,
                            xs,
                            ys,
                            &mut present,
                            &mut appearances,
                            &mut points_seen,
                            &mut geom_anchor,
                        ) {
                            failure = Some(e);
                            return ControlFlow::Break(());
                        }
                    }
                    ControlFlow::Continue(())
                },
            )?;
            if let Some(error) = failure {
                return Err(error);
            }
            resolve(
                &mut chunk,
                xs,
                ys,
                &mut present,
                &mut appearances,
                &mut points_seen,
                &mut geom_anchor,
            )?;
            if points_seen != view_anchors[index].rows {
                return Err(input_changed(&format!(
                    "view '{}': the points file yielded {points_seen} geometry rows, but its \
                     first pass selected {}",
                    view.view_id, view_anchors[index].rows
                )));
            }
            if geom_anchor != view_anchors[index].mixed {
                return Err(input_changed(&format!(
                    "view '{}': the points file's geometry pass carries different ids than its \
                     first pass did (row count unchanged)",
                    view.view_id
                )));
            }
        }
        geometry.push(ViewGeometry {
            x: x_map,
            y: y_map,
            present,
            rows: view_anchors[index].rows,
        });
    }
    // **The anchor's Morton code, with the declared fallback** (decision 0112): an item absent
    // from the anchor takes its code in the first-declared view that holds it. Materialised here
    // rather than chosen inside the sort, so the batch loop reads one array and the choice is
    // made once per item.
    let mut anchor_x_map = spill::MappedU32::zeroed(tmp.path(), "x-anchor.u32", n as usize)?;
    let mut anchor_y_map = spill::MappedU32::zeroed(tmp.path(), "y-anchor.u32", n as usize)?;
    {
        let xs = anchor_x_map.as_mut_slice();
        let ys = anchor_y_map.as_mut_slice();
        let order: Vec<usize> = std::iter::once(args.anchor)
            .chain((0..args.views.len()).filter(|&v| v != args.anchor))
            .collect();
        for (ordinal, (x, y)) in xs.iter_mut().zip(ys.iter_mut()).enumerate() {
            let held = order
                .iter()
                .copied()
                .find(|&v| bit_get(&geometry[v].present, ordinal))
                .expect("the union of the views' ids is where this ordinal came from");
            *x = geometry[held].x.as_slice()[ordinal];
            *y = geometry[held].y.as_slice()[ordinal];
        }
    }
    let x_of_ordinal = anchor_x_map.as_slice();
    let y_of_ordinal = anchor_y_map.as_slice();
    timer.end(BuildStage::GeometryRead, n);

    // `source_ids` is **held** past this point rather than dropped and re-read: pass one unions
    // several files, so recovering it later would be one re-read per view against anchors that
    // would each have to be carried anyway. 8 B/item, released at step 8c with the layer join.
    drop(term_keys);
    drop(term_ids);
    drop(row_counts);
    let mut store = sink.finish()?;

    timer.end(BuildStage::PairsPack, pushed);

    // ---- 4–5. per batch: sort, signature-refine, assign, emit bands (§11.1) -----------
    // Entity id = batch base + position in the batch's signature order. Batches partition
    // ordinal space, so per-batch sort+dedup of the packed relation equals the historical
    // global sort+dedup (a duplicate pair shares its ordinal, hence its batch), and one batch
    // covering everything reproduces the pre-batching assignment exactly.
    let mut entity_of_ordinal: Vec<u32> = vec![0; n as usize];
    // Exact per-term post-dedup counts, accumulated as bands are emitted; drives the band
    // sweep's offsets. u32 is sound (a term's entities are distinct, so count <= n < 2^32).
    let mut term_counts: Vec<u32> = vec![0; term_count as usize];
    let mut band_writers: Vec<spill::BandWriter> = plan
        .band_bounds
        .iter()
        .enumerate()
        .map(|(j, &(lo, _))| {
            spill::BandWriter::create(&tmp.path().join(format!("band-{j}.pairs")), lo)
        })
        .collect::<Result<_>>()?;
    let band_los: Vec<u32> = plan.band_bounds.iter().map(|&(lo, _)| lo).collect();
    let mut pair_count = 0u64;
    let mut over_bound_items = 0u64;
    let mut entity_base = 0u64;
    for k in 0..plan.batches {
        let ordinal_lo = k * plan.batch_items;
        let ordinal_hi = ((k + 1) * plan.batch_items).min(n);
        let batch_len = (ordinal_hi - ordinal_lo) as usize;
        let mut packed = store.load(k)?;
        // A total order on (at worst bit-identical) u64s: one output under the parallel
        // unstable sort. The label set is a *set*: a repeated input row must not become a
        // repeated posting (per-batch dedup == global dedup, ordinals partition by batch).
        packed.par_sort_unstable();
        packed.dedup();
        pair_count += packed.len() as u64;

        // Per-ordinal signature starts (u32: the plan caps any batch's pairs well below
        // 2^32), the long-signature bitset, and the pre-sort keys — today's stage 4 over one
        // batch, with `starts` subsuming the old long-only start index because the band emit
        // below needs every item's view, not only the long ones.
        let mut starts: Vec<u32> = Vec::with_capacity(batch_len + 1);
        let mut long_sig: Vec<u64> = vec![0; batch_len.div_ceil(64)];
        let mut recs: Vec<SortRec> = Vec::with_capacity(batch_len);
        let mut cursor = 0usize;
        for local in 0..batch_len {
            let ordinal = ordinal_lo + local as u64;
            starts.push(cursor as u32);
            let start = cursor;
            while cursor < packed.len() && (packed[cursor] >> 32) == ordinal {
                cursor += 1;
            }
            let sig = &packed[start..cursor];
            if sig.len() > bounds.max_terms_per_item as usize {
                // A declared bound is a *declaration*: record it and carry on. Dropping
                // terms here would silently widen the item's visibility (I2/I3).
                over_bound_items += 1;
            }
            if sig.len() > 2 {
                bit_set(&mut long_sig, local);
            }
            // `term + 1` so that "no term at this position" (0) sorts before every real
            // term, which is what makes a signature order before any signature extending it.
            let key: u64 = match sig.len() {
                0 => 0,
                1 => (term_of(sig[0]) as u64 + 1) << 32,
                _ => ((term_of(sig[0]) as u64 + 1) << 32) | (term_of(sig[1]) as u64 + 1),
            };
            recs.push(SortRec {
                key_hi: (key >> 32) as u32,
                key_lo: key as u32,
                // From the quantised form directly, exactly as the tiler does — `split32`'s cell
                // half is the code `morton_of` would give for the same point, so entity order and
                // row order agree about what is nearby. Computed rather than stored: a third
                // mapped array would cost 4 B/item to save one interleave per item.
                morton: split32(
                    x_of_ordinal[ordinal as usize],
                    y_of_ordinal[ordinal as usize],
                )
                .0
                .raw(),
                ordinal: ordinal as u32,
            });
        }
        starts.push(cursor as u32);
        if cursor != packed.len() {
            return Err(input_changed(&format!(
                "bucket {k} holds pairs outside its ordinal range [{ordinal_lo}, {ordinal_hi})"
            )));
        }
        // The triple (key_hi, key_lo, ordinal) is unique per rec — a total order, so the
        // parallel unstable sort has exactly one output; refinement makes it the reference
        // `(signature, source_id)` order.
        recs.par_sort_unstable_by_key(|r| r.order());
        refine_signature_ties(&mut recs, &packed, &starts, ordinal_lo, &long_sig);
        drop(long_sig);
        timer.end(BuildStage::SignatureSort, recs.len() as u64);

        // Assignment, and the band emit in the same walk: entities ascend with position, so
        // every term's entity list arrives ascending — within this batch here, and across
        // batches because bases ascend and the loop is sequential. `encode_posting`'s
        // unconditional sortedness check later re-verifies exactly this property from disk.
        for (position, rec) in recs.iter().enumerate() {
            let entity = (entity_base + position as u64) as u32;
            entity_of_ordinal[rec.ordinal as usize] = entity;
            let local = (rec.ordinal as u64 - ordinal_lo) as usize;
            let sig = &packed[starts[local] as usize..starts[local + 1] as usize];
            // **The label is the entity's, not the row's** (`views.md` §7): every view holding
            // this item must have given it the same term set. `sig` is the deduplicated union
            // over the views, and `distinct_of_ordinal` is the sum of each view's own distinct
            // count — so the two agree exactly when every view contributed the whole union, and
            // the identity is a refusal rather than a hash comparison.
            //
            // Checked only on the per-view route: a shared relation is entity space already and
            // is scanned once, so there is nothing for two views to disagree about
            // (`crate::AccessRoute`).
            if per_view_labels
                && distinct_of_ordinal[rec.ordinal as usize] as u64
                    != sig.len() as u64 * appearances[rec.ordinal as usize] as u64
            {
                return Err(BuildError::Invalid(format!(
                    "entity_id {} carries different access labels in different views. A label is \
                     the entity's, not the row's (views §7): it is one set wherever the entity \
                     appears, and a re-label is a delete plus a re-ingest (decision 0047). The \
                     views this build reads are {}",
                    source_ids[rec.ordinal as usize],
                    args.views
                        .iter()
                        .map(|v| format!("'{}' ({})", v.view_id, v.points.display()))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            for &value in sig {
                let term = term_of(value);
                let band = band_los.partition_point(|&lo| lo <= term) - 1;
                band_writers[band].push(term, entity)?;
                term_counts[term as usize] =
                    term_counts[term as usize].checked_add(1).ok_or_else(|| {
                        BuildError::Invalid(format!(
                            "term {term} exceeds 2^32 postings, which the entity ceiling makes \
                             impossible for an unmutated input"
                        ))
                    })?;
            }
        }
        entity_base += recs.len() as u64;
        store.delete(k)?;
        timer.end(BuildStage::Assignment, recs.len() as u64);
    }
    if entity_base != n {
        return Err(input_changed(&format!(
            "batches assigned {entity_base} entities for {n} items"
        )));
    }
    // The anchor's Morton geometry has served its one reader — the sort's tiebreak — and is
    // released here rather than at the end of the build. At 10⁹ that is 8 GB of dirty mapped
    // pages returned before the band sweep and the postings write start competing for page
    // cache. Each view's own geometry stays: pass two is what reads it.
    drop(anchor_x_map);
    drop(anchor_y_map);
    let band_receipts: Vec<spill::SpillReceipt> = band_writers
        .into_iter()
        .map(|w| w.finish())
        .collect::<Result<_>>()?;
    if over_bound_items > 0 {
        eprintln!(
            "warning: {over_bound_items} item(s) exceed the plugin's declared \
             max_terms_per_item ({}); no term was dropped",
            bounds.max_terms_per_item
        );
    }

    let partition_dir = args.out.join(PREFIX).join("partitions").join(PHASH);
    let terms_dir = partition_dir.join("terms");
    let entities_dir = partition_dir.join("entities");
    for dir in [&terms_dir, &entities_dir] {
        std::fs::create_dir_all(dir).map_err(|e| BuildError::io(dir, e))?;
    }

    timer.end(BuildStage::Assignment, n);

    // ---- 6. postings and pairs.parquet, one term band at a time -----------------------
    // Bands arrive from the batch loop with every term's entities strictly ascending (bases
    // ascend across batches, positions within one); the cursor-scatter needs no sort, and
    // `encode_posting`'s unconditional sortedness check re-verifies the whole spill path from
    // disk, fail-closed. The integrity chain replacing the old third pairs scan: bucket
    // receipts -> in-process emit -> band receipts (count + anchor, verified on read) ->
    // per-term exact counts -> the total against the deduplicated relation.
    let postings_path = terms_dir.join("postings.arrow");
    let pairs_path = args
        .emit_oracle_pairs
        .then(|| terms_dir.join("pairs.parquet"));
    let mut pairs_writer = pairs_path
        .as_deref()
        .map(PairsParquetWriter::create)
        .transpose()?;
    let mut spool = tessera_authz::PostingsSpool::create(&tmp.path().join("postings.spool"))
        .map_err(|e| BuildError::io(&postings_path, e))?;
    let mut written = 0u64;
    for (j, &(band_lo, band_hi)) in plan.band_bounds.iter().enumerate() {
        let width = (band_hi - band_lo) as usize;
        let mut offsets: Vec<u64> = Vec::with_capacity(width + 1);
        let mut total = 0u64;
        offsets.push(0);
        for term in band_lo..band_hi {
            total += term_counts[term as usize] as u64;
            offsets.push(total);
        }
        let mut flat: Vec<u32> = vec![0; total as usize];
        let mut cursor: Vec<u64> = offsets[..width].to_vec();
        let mut reader = spill::BandReader::open(&band_receipts[j])?;
        while let Some((term, entity)) = reader.next()? {
            if term < band_lo || term >= band_hi {
                return Err(input_changed(&format!(
                    "band {j} holds term {term}, outside its [{band_lo}, {band_hi}) range"
                )));
            }
            let local = (term - band_lo) as usize;
            let slot = &mut cursor[local];
            // The band was sized by the emit's exact count for this term. Writing past its
            // end would land in the next term's postings — one term's entities silently
            // becoming another's, which is a disclosure. Check rather than trust.
            if *slot >= offsets[local + 1] {
                return Err(input_changed(&format!(
                    "term {term} received more postings than the {} its emit counted",
                    term_counts[term as usize]
                )));
            }
            flat[*slot as usize] = entity;
            *slot += 1;
        }
        // The mirror: a short-filled term would leave zeroed slots, and zero is a valid
        // entity id — catch it by count, not by value.
        for (local, slot) in cursor.iter().enumerate() {
            if *slot != offsets[local + 1] {
                return Err(input_changed(&format!(
                    "term {} expected {} postings, received {}",
                    band_lo as usize + local,
                    offsets[local + 1] - offsets[local],
                    slot - offsets[local]
                )));
            }
        }
        // Encode in bounded sub-chunks — parallel, collected in term order — and append to
        // the spool; never one live record per term (T = 117M of those is gigabytes of Vec
        // headers before a byte is written).
        const ENCODE_CHUNK_TERMS: usize = 1 << 16;
        let mut t = 0usize;
        while t < width {
            let hi = (t + ENCODE_CHUNK_TERMS).min(width);
            let encoded: Vec<Vec<u8>> = (t..hi)
                .into_par_iter()
                .map(|local| {
                    let slice = &flat[offsets[local] as usize..offsets[local + 1] as usize];
                    encode_posting(
                        band_lo as usize + local,
                        slice,
                        SMALL_TERM_THRESHOLD_DEFAULT,
                    )
                    .map_err(|e| BuildError::io(&postings_path, e))
                })
                .collect::<Result<_>>()?;
            for (local, record) in (t..hi).zip(encoded) {
                spool
                    .append(&record)
                    .map_err(|e| BuildError::io(&postings_path, e))?;
                let slice = &flat[offsets[local] as usize..offsets[local + 1] as usize];
                written += slice.len() as u64;
                if let Some(writer) = pairs_writer.as_mut() {
                    writer.push_run(band_lo + local as u32, slice)?;
                }
            }
            t = hi;
        }
        drop(flat);
        std::fs::remove_file(&band_receipts[j].path)
            .map_err(|e| BuildError::io(&band_receipts[j].path, e))?;
    }
    drop(term_counts);
    if written != pair_count {
        return Err(BuildError::Invalid(format!(
            "postings hold {written} pairs but the relation has {pair_count}"
        )));
    }
    if let Some(writer) = pairs_writer {
        writer.finish()?;
    }
    spool
        .finish(&postings_path)
        .map_err(|e| BuildError::io(&postings_path, e))?;
    fsync_file(&postings_path)?;

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
            write_external_id_runs(&entities_dir, &external, EXTERNAL_ID_ROWS_PER_EXTENT)?;
        // `external` is still in the concatenated extent order at this point (the extents
        // partition it into consecutive ranges, in order) — its index *is* each row's ordinal,
        // which is exactly what the locator addresses (contracts §2.4/§2.6 r6).
        ext_locator_path = Some(write_ext_locator(&entities_dir, &external, n)?);
    }

    timer.end(
        BuildStage::ExternalIds,
        if args.mint_external_ids { n } else { 0 },
    );

    // ---- 8. the declared attribute tail ----------------------------------------------
    // **Geometry is already in entity order**: the assignment walk placed it as it assigned, so
    // there is no geometry stage here at all — no read, and no permute. 32-bit fixed point per
    // axis, not coordinates: the cell code and its residual both fall out by shift and mask, so
    // no stage re-quantises (see `input::PointRow`).
    // The declared attribute tail, read **here** and not at the segment write, because this is
    // where the two things that resolve it are still alive: `source_ids` maps a file's id to its
    // ordinal, and `entity_of_ordinal` maps that ordinal to the entity the build assigned it.
    // Entity ids are assigned in *signature-sorted* order (§11.1), so a source id is emphatically
    // not its own entity id — indexing the attribute arrays by source id gives every item another
    // item's attributes, coherently and with no error anywhere. Caught at 2.4M against the source
    // corpus; the geometry pass three statements above resolves through the same two structures
    // for the same reason.
    //
    // `minters` seeds one live minter per discovered vocabulary from what the schema already
    // pins; the scan mints into it for every novel key, and its final state — carried past this
    // call — is what step 11 below records into `MANIFEST.vocabularies`.
    let mut minters = args.schema.open_minters();
    // The columns' files live in the same `.build-tmp/` as the spill and band files, so a killed
    // build leaves them to the next `TmpDir::create` exactly as it leaves those.
    let scratch = crate::column::ColumnScratch::new(tmp.path());
    let (attributes_by_entity, coverage) = read_attributes_by_entity(
        args,
        n,
        &source_ids,
        &entity_of_ordinal,
        &mut minters,
        &scratch,
    )?;
    // **Printed here, where the join has just happened and the numbers are the join's own.** The
    // linear build reports the identical figures from its own pass, so the two builds agree about
    // coverage exactly as they agree about bytes.
    crate::report_attribute_coverage(&coverage);

    // ---- 8b. the group-scoped column families (`views.md` §5) --------------------------
    // Here, beside the attribute tail, because it wants exactly what the tail wants: `source_ids`
    // and `entity_of_ordinal`, both alive, and entity ids final under I9. One column per view of
    // the group, in entity space; nothing per row space.
    let scoped_paths = write_scoped_columns(
        args,
        &partition_dir,
        n,
        &source_ids,
        &entity_of_ordinal,
        &mut minters,
        &scratch,
    )?;

    timer.end(BuildStage::AttributeTail, n);

    // ---- 8c. layers and their artifacts ------------------------------------------------
    // Resolved here for the reason the attribute tail is: this is where the two structures that
    // turn a source id into the entity this build assigned it are both still alive. A member is
    // named by source id, exactly as the pairs file's ids are.
    //
    // **Its own stage boundary**, so an observer can say how much of the peak is here: the member
    // tables are read whole and the published memberships stay resident, and this ran unattributed
    // inside the attribute tail until the campaign's kills made the distinction worth having
    // (`residency.rs`).
    let view_ids: Vec<String> = args.views.iter().map(|v| v.view_id.clone()).collect();
    let view_frames: Vec<tessera_store::derived::ViewFrame> = args
        .views
        .iter()
        .map(|v| tessera_store::derived::ViewFrame::new(&v.view_id, v.projection, v.extent))
        .collect();
    let mut published_layers = if args.layers.is_empty() {
        crate::layers::PublishedLayers::default()
    } else {
        {
            let mut plan = crate::layers::read(
                &args.layers,
                &args.layer_inputs,
                &args.scoped_layers,
                // **A frame per view, never the anchor's for all of them** (decision 0111): a
                // shape layer is canonicalised in each view it is drawn in, against that view's
                // own projection and extent, so a layer spanning frames stores a different
                // canonical form under each view's name.
                &view_frames,
                tessera_types::layer::DEFAULT_MAX_SHAPE_VERTICES,
                // The build's own `.build-tmp/`, which the member spill writes its runs into —
                // still open here, and swept by the `close` below whether this stage succeeds or
                // not.
                tmp.path(),
                args.memory_budget.unwrap_or_else(detect_memory_budget),
            )?;
            crate::report_shapes(&plan.shape_reports);
            // **A contiguous id range makes the search a subtraction**, and whether it is
            // contiguous is checked rather than assumed. `source_ids` is sorted and free of
            // duplicates, so a range spanning exactly its own length can only be
            // `ids_first + i` at every `i` — the fast path is provably the same answer, not a
            // convention about how a caller numbers its rows.
            //
            // It is worth the branch because this closure runs **once per member entry**: a
            // lineage list per point at the Overture rung is 3×10⁸ of them, and a binary search
            // into 74M sorted `u64` is ~27 dependent cache misses where the subtraction is one.
            let dense = ids_last - ids_first + 1 == source_ids.len() as u64;
            crate::layers::publish(
                &mut plan,
                &|source| {
                    let ordinal = if dense {
                        source
                            .checked_sub(ids_first)
                            .filter(|o| (*o as usize) < source_ids.len())
                            .map(|o| o as usize)
                    } else {
                        source_ids.binary_search(&source).ok()
                    };
                    ordinal.map(|ordinal| entity_of_ordinal[ordinal] as u64)
                },
                n,
                &args.out.join(crate::PREFIX),
                crate::PHASH,
                &view_ids,
                &crate::layers::predicate_artifact_keys(
                    &args.layers,
                    &args.schema,
                    &minters,
                    &|index| distinct_codes(attributes_by_entity[index].iter()),
                )?,
            )?
        }
    };

    drop(source_ids);

    crate::write_containment_report(&args.out, &published_layers)?;

    timer.end(BuildStage::Layers, published_layers.layers.len() as u64);

    // ---- 8d. attribute filter postings (filter-index §4) -------------------------------
    // Its own stage, after entity assignment and before the tiler sort: entity ids are final
    // here (stage 5, permanent under I9) and the values have just been read, which are the two
    // things the emit needs. It cannot ride on `PostingsWrite` — that stage runs before the
    // attribute values exist.
    let (filter_paths, text_index) = write_filter_postings(
        &partition_dir,
        &args.schema,
        &attributes_by_entity,
        plan.budget,
    )?;
    // The text columns' share, charged out of the block rather than measured beside it — the two
    // interleave over one column loop, so a boundary in time cannot separate them.
    timer.charge(BuildStage::TextIndex, text_index.elapsed, text_index.terms);
    timer.end(BuildStage::FilterPostings, n);

    // The record blob wants the same two things the postings did — entity ids final under I9, and
    // the attribute values in hand — so it runs here. Its files join the manifest digest at step 11
    // with everything else. **Its own stage**: it and the postings and the release below were one
    // number for three jobs, which is why the 615 s this block cost at 7.4×10⁷ points could be
    // modelled and not read.
    let record_paths = write_record_blob(&partition_dir, &args.schema, &attributes_by_entity)?;
    timer.end(BuildStage::RecordBlob, n);
    // **Everything past here wants only the render columns**, and the two passes that wanted the
    // rest have just run. `permute_attribute_tail` skips a non-render column outright (its home is
    // entity space, and giving it a slot in every row is the per-row cost §10.3's routing exists to
    // avoid), so an index-only column is dead from this line — but it was living to the end of the
    // segment write, straight through the tiler sort's 12 B/row and the record batch beside it.
    //
    // For a `text` column that is the whole of the corpus's prose: ~24 GB of strings at 2.5×10⁸
    // titles, held for a stage that will not read one of them. Releasing here is what lets a schema
    // carry text at all at these scales without the peak paying for it twice over.
    let mut attributes_by_entity = attributes_by_entity;
    for (column, attribute) in attributes_by_entity
        .iter_mut()
        .zip(args.schema.attributes.iter())
    {
        if !attribute.render {
            column.release();
        }
    }
    timer.end(BuildStage::ColumnRelease, n);

    // ---- 9/10. pass two: one row space per view (`views.md` §7) ----------------------
    // **The build below is what it always was, run once per view.** What is not one-view-at-a-
    // time is everything above: identity, the dictionary, the postings, the external ids and the
    // attribute columns are entity space and were built once, over the union of every view's
    // items. Here each view transforms through its own projection, quantises against its own
    // frame (decision 0040), Morton-sorts and writes its segment, its permutation and its
    // row→entity file.
    //
    // A view holds a **subset** of entity space — its `present` bits — so its permutation is
    // sentinel wherever it does not, and `row_count` is the view's population rather than `n`.
    let mut view_files: Vec<PathBuf> = Vec::new();
    let mut segments: Vec<tessera_store::manifest::SegmentDescriptor> = Vec::new();
    let mut occupancies: Vec<crate::Occupancy> = Vec::with_capacity(args.views.len());
    let mut artifact_paths: Vec<PathBuf> = Vec::new();
    // Accumulated across the views, like the extents beside them: what a scoped layer's artifacts
    // came to in each view is per view or it says nothing (`views.md` §3.5).
    let mut artifact_levels: Vec<crate::artifact_pass::LevelLayoutReport> = Vec::new();
    for (index, view) in args.views.iter().enumerate() {
        let view_dir = tessera_store::view_path(&partition_dir, &view.view_id);
        let segment_dir = view_dir.join("segments").join(SEG_ID);
        std::fs::create_dir_all(&segment_dir).map_err(|e| BuildError::io(&segment_dir, e))?;

        // The view's geometry, permuted from ordinal into entity space — the one scatter this
        // costs, against a parquet re-read per view (`views.md` §8's file arithmetic).
        let mut x_map = spill::MappedU32::zeroed(tmp.path(), "x-of-entity.u32", n as usize)?;
        let mut y_map = spill::MappedU32::zeroed(tmp.path(), "y-of-entity.u32", n as usize)?;
        {
            let xs = x_map.as_mut_slice();
            let ys = y_map.as_mut_slice();
            let (view_x, view_y) = (geometry[index].x.as_slice(), geometry[index].y.as_slice());
            for (ordinal, &entity) in entity_of_ordinal.iter().enumerate() {
                xs[entity as usize] = view_x[ordinal];
                ys[entity as usize] = view_y[ordinal];
            }
        }
        // Membership in entity space, from the same permutation of the ordinal-space bits.
        let mut member: Vec<u64> = vec![0; (n as usize).div_ceil(64)];
        for (ordinal, &entity) in entity_of_ordinal.iter().enumerate() {
            if bit_get(&geometry[index].present, ordinal) {
                bit_set(&mut member, entity as usize);
            }
        }
        let x_of_entity = x_map.as_slice();
        let y_of_entity = y_map.as_slice();

        // §2.6 r6: `(morton, tessera_id)` ascending, no further tiebreak. `tessera_id` is
        // computed BEFORE the sort (2026-07-30 fold, memo §6) — `priority = high16(tessera_id)`
        // is a sort key, so the identity must exist before `sort_unstable_by` runs.
        let mut rows: Vec<RowRec> = (0..n as usize)
            .into_par_iter()
            .filter(|entity| bit_get(&member, *entity))
            .map(|entity| {
                let tessera_id = args
                    .identity_key
                    .forward(args.shard_id, EntityId::new(entity as u64))?;
                Ok(RowRec {
                    // From the quantised form directly: `split32`'s cell half is by
                    // construction the code `morton_of` would give for the same point.
                    morton: split32(x_of_entity[entity], y_of_entity[entity]).0.raw(),
                    entity: entity as u32,
                    priority: tessera_id.priority(),
                    _pad: 0,
                })
            })
            .collect::<Result<_>>()?;
        rows.par_sort_unstable_by(|a, b| a.cmp(b, &args.identity_key, args.shard_id));
        let rows_in_view = rows.len() as u32;
        if rows_in_view as u64 != geometry[index].rows {
            return Err(input_changed(&format!(
                "view '{}': {rows_in_view} rows in the segment for the {} its points file \
                 selected",
                view.view_id, geometry[index].rows
            )));
        }
        timer.end(BuildStage::TilerSort, rows_in_view as u64);

        let morton_path = segment_dir.join("morton.u32");
        // **The resolution this frame actually gave the corpus**, counted off the same sorted
        // codes that are about to become `morton.u32`. Nothing is retained: `rows` is already
        // `(morton, tessera_id)` ascending, so distinct cells is a comparison per row.
        occupancies.push(crate::Occupancy::of_sorted_codes(
            rows.iter().map(|r| r.morton),
        ));
        write_morton_codes(&morton_path, rows.iter().map(|r| r.morton))
            .map_err(|e| BuildError::io(&morton_path, e))?;

        // **Bounded by entity space, populated by the view.** An entity this view does not hold
        // keeps the row-absent sentinel `PermutationWriter::create` laid down, which is exactly
        // what a sparse view is (`views.md` §8).
        let permutation_path = view_dir.join("permutation.bin");
        write_permutation_iter(
            &permutation_path,
            rows.iter().map(|r| EntityId::new(r.entity as u64)),
            n,
        )
        .map_err(|e| BuildError::io(&permutation_path, e))?;
        fsync_file(&permutation_path)?;

        // The row→entity direction beside it (`tessera_store::row_entity`), from the same sorted
        // rows the permutation was scattered from.
        let row_entity_path = view_dir.join(tessera_store::ROW_ENTITY_FILE);
        let entity_row: Vec<u32> = rows.iter().map(|r| r.entity).collect();
        tessera_store::write_row_entity(&row_entity_path, &entity_row)
            .map_err(|e| BuildError::io(&row_entity_path, e))?;
        fsync_file(&row_entity_path)?;

        let columns_path = segment_dir.join("columns.arrow");
        let mut presence_paths: Vec<PathBuf> = Vec::new();
        {
            // The residual is the low half of the same `split32` whose high half became the
            // row's Morton code above — one splitting of one fixed-point position, so
            // `columns.arrow` and `morton.u32` cannot describe different points.
            let residual_row: Vec<u32> = rows
                .par_iter()
                .map(|r| {
                    let entity = r.entity as usize;
                    split32(x_of_entity[entity], y_of_entity[entity]).1
                })
                .collect();
            drop(rows);
            // `forward` is fallible (Important I-1): a checked conversion, never `as u32`.
            let tessera_row: Vec<u64> = entity_row
                .par_iter()
                .map(|&e| {
                    args.identity_key
                        .forward(args.shard_id, EntityId::new(e as u64))
                        .map(|id| id.raw())
                })
                .collect::<std::result::Result<_, _>>()
                .map_err(BuildError::Identity)?;
            // The declared attribute tail, permuted into this view's row order. Gathered per
            // entity and then permuted — the values are entity space and are shared by every
            // view, which is the whole of `views.md` §1's factoring.
            let tail =
                permute_attribute_tail(&args.schema, &attributes_by_entity, &entity_row, &scratch)?;
            for (column, rows) in tail.presence {
                if let Some(path) = tessera_store::flush::write_render_presence(
                    &segment_dir,
                    &column,
                    rows,
                    rows_in_view,
                )
                .map_err(|e| BuildError::Invalid(format!("attribute '{column}': {e}")))?
                {
                    presence_paths.push(path);
                }
            }
            write_columns(&columns_path, tessera_row, residual_row, tail.columns)
                .map_err(|e| BuildError::io(&columns_path, e))?;
        }
        fsync_file(&columns_path)?;
        fsync_file(&morton_path)?;
        drop(x_map);
        drop(y_map);
        timer.end(BuildStage::SegmentWrite, rows_in_view as u64);

        // ---- 10b. the post-bundle artifact pass, per view (decision 0094's first half) ----
        //
        // **Here and not at step 8c**, where the layers were published: the pick reads where each
        // membership landed in *row* space, and this view's row space did not exist until the
        // permutation above.
        let artifact_store = std::mem::take(&mut published_layers.store);
        let artifact_pass = crate::artifact_pass::run(
            &mut published_layers,
            &artifact_store,
            &args.out.join(crate::PREFIX),
            crate::PHASH,
            &view.view_id,
            rows_in_view,
            &plugin.data_plugin_hash(),
        );
        published_layers.store = artifact_store;
        crate::artifact_pass::report(&artifact_pass);
        artifact_levels.extend(artifact_pass.levels.iter().cloned());
        // **Accumulated across views, not replaced.** Every artifact extent is keyed by
        // `(view, layer, level)`, so each view's pass adds its own; assigning would leave the
        // manifest carrying the last view's alone.
        published_layers
            .tile_index_extents
            .extend(artifact_pass.tile_index_extents.iter().cloned());
        published_layers
            .row_column_extents
            .extend(artifact_pass.row_column_extents.iter().cloned());
        published_layers
            .containment_extents
            .extend(artifact_pass.containment_extents.iter().cloned());
        published_layers
            .shape_rows_extents
            .extend(artifact_pass.shape_rows_extents.iter().cloned());
        published_layers
            .shape_held_extents
            .extend(artifact_pass.shape_held_extents.iter().cloned());
        artifact_paths.extend(artifact_pass.paths.iter().cloned());

        view_files.push(permutation_path);
        view_files.push(row_entity_path);
        view_files.push(columns_path);
        view_files.push(morton_path);
        view_files.extend(presence_paths);
        segments.push(tessera_store::manifest::SegmentDescriptor {
            view: view.view_id.clone(),
            seg_id: SEG_ID.to_string(),
            row_count: rows_in_view,
            entity_lo: 0,
            entity_hi: n,
        });
    }
    drop(geometry);
    drop(entity_of_ordinal);
    // The spill directory closes **here**: every view's ordinal-space geometry and every
    // entity-space scatter are dropped by this point, so the tree is unbusy.
    tmp.close()?;

    // ---- 11. manifests ---------------------------------------------------------------
    let mut other_paths = vec![postings_path];
    other_paths.extend(view_files);
    other_paths.extend(filter_paths);
    other_paths.extend(scoped_paths);
    other_paths.extend(record_paths);
    other_paths.extend(pairs_path);
    other_paths.extend(ext_locator_path);
    other_paths.extend(published_layers.paths.iter().cloned());
    other_paths.extend(artifact_paths);
    let mut report = write_manifests(
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
        plan.recorded_batch_items,
        &minters,
        &published_layers,
        &segments,
        &occupancies,
    )?;
    // Reported in bytes, not rows: this stage re-reads and SHA-256s every byte the build wrote,
    // so it scales with bundle size rather than with item count.
    timer.end(BuildStage::Manifests, report.bundle_bytes);
    report.attribute_coverage = coverage;
    report.artifact_levels = artifact_levels;
    Ok(report)
}

/// Read the declared attribute columns into **entity-major** vectors, one per declared attribute,
/// with one pass per attribute source.
///
/// Resolved through `source_ids` → ordinal → `entity_of_ordinal`, exactly as the geometry pass
/// above resolves its own rows, and for the same reason: entity ids are assigned in
/// signature-sorted order (§11.1), so a source id is not its own entity id and a direct index
/// hands every item another item's attributes. That is a defect with no symptom — every value is
/// present, every value is well-typed, and every value belongs to a different item.
///
/// Returns an empty vector when the schema declares nothing, which is what keeps a schema-less
/// build's `columns.arrow` byte-identical to the one it wrote before this existed.
///
/// **An entity this pass never reaches keeps an absent slot, and that is a report rather than a
/// failure** (`configuration.md` §1). The columns are filled absent before a row is read, so a
/// column no source names for an entity is *absent* — the same state the source's own null
/// produces, and the one every consumer already reads. Which entities came away with a value, and
/// how many rows named entities this build never loaded, are counted per source and printed:
/// a source covering a subset is a column that is simply absent for the rest, and a source
/// covering a superset is the ordinary shape of a table that lives elsewhere.
fn read_attributes_by_entity(
    args: &BuildArgs,
    n: u64,
    source_ids: &[u64],
    entity_of_ordinal: &[u32],
    minters: &mut std::collections::HashMap<String, tessera_store::vocabulary::VocabularyMinter>,
    scratch: &crate::column::ColumnScratch,
) -> Result<(Vec<EntityColumn>, Vec<crate::AttributeCoverage>)> {
    if args.schema.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let attributes = &args.schema.attributes;
    // One typed column per attribute, indexed by entity — see [`EntityColumn`] for why this is not
    // the `ScalarValue` vector it reads like, and what that costs at 10⁸ items and above.
    let mut by_entity: Vec<EntityColumn> = attributes
        .iter()
        .map(|a| EntityColumn::filled(scratch, a.ty, n as usize))
        .collect::<Result<_>>()?;
    // **One sweep per source, not one over a single corpus file.** Each declared attribute names
    // the file it is read from, so the groups are the passes; a build whose columns sit in three
    // files reads three files, and each one joins on the identity column its own group declared.
    let mut coverage = Vec::with_capacity(args.attribute_sources.len());
    for group in &args.attribute_sources {
        read_one_attribute_source(
            args,
            group,
            n,
            source_ids,
            entity_of_ordinal,
            minters,
            scratch,
            &mut by_entity,
            &mut coverage,
        )?;
    }
    Ok((by_entity, coverage))
}

/// One attribute source's merge sweep into the entity-major columns.
#[allow(clippy::too_many_arguments)]
fn read_one_attribute_source(
    args: &BuildArgs,
    group: &crate::config::AttributeSource,
    n: u64,
    source_ids: &[u64],
    entity_of_ordinal: &[u32],
    minters: &mut std::collections::HashMap<String, tessera_store::vocabulary::VocabularyMinter>,
    scratch: &crate::column::ColumnScratch,
    by_entity: &mut [EntityColumn],
    coverage: &mut Vec<crate::AttributeCoverage>,
) -> Result<()> {
    let columns: Vec<&crate::config::Attribute> = group
        .attributes
        .iter()
        .map(|&i| &args.schema.attributes[i])
        .collect();
    let attributes = &columns;
    let mut matched_rows = 0u64;
    // **Counted, not refused** (`configuration.md` §1). A row naming an entity this build did not
    // load is what a join does with a source that covers a superset — which every legitimate
    // attribute table over a limited build is — and ignoring it is fail-closed in both directions
    // that matter: an absent attribute matches fewer points in a filter, and an absent access
    // label leaves a point visible to nobody.
    let mut unknown_rows = 0u64;

    // **[`join_chunk`]'s merge sweep, not a probe per row.** This pass used a `binary_search` into
    // `source_ids` for every row, on the reasoning that it is "per-row work over a handful of
    // narrow columns, not the corpus-scale join the geometry pass does". That holds while
    // `source_ids` fits in cache and inverts well before it stops: at 2.5×10⁸ the array is 2 GB and
    // the probes are uniformly scattered across it, so nearly every one of the ~28 comparisons is a
    // cache miss.
    //
    // **Worth 1m49s of a ~19m build at 2.5×10⁸, not the "~20 minutes of one core" an earlier
    // revision of this comment claimed.** That figure came from sampling the pass at 97% of one
    // core for sixty seconds and taking the whole elapsed stretch to be it; the stretch included a
    // suspended laptop, and the build's *total* CPU was 19m45s, which the pass alone plainly cannot
    // exceed. Measured properly, by the cgroup's own accounting across two builds of the same
    // corpus: 19min 45.7s of CPU before, 17min 56.8s after. Real, and worth keeping — the sweep is
    // also the shape that stays sequential as the corpus grows, where the probe's cost per row does
    // not — but a tenth of what was asserted.
    //
    // Chunked, both sides ascend and the sweep is sequential, which is the same trade the geometry
    // pass makes for the same reason. The cost is a staging buffer, and **it is sized in bytes**:
    // see [`JOIN_STAGE_BYTES`] for why a row count is the wrong unit here.
    //
    // **At least one whole decoded batch**, because a batch is staged as a unit: the flush below
    // happens between batches, so the buffer has to hold the largest one whatever the byte budget
    // works out at. On a corpus smaller than a batch that is the only term that matters, and it is
    // a few hundred kilobytes of file per column.
    let staged_rows = staging_rows(attributes, n).max(input::ATTRIBUTE_BATCH_ROWS);
    let mut staged: Vec<EntityColumn> = attributes
        .iter()
        .map(|a| EntityColumn::filled(scratch, a.ty, staged_rows))
        .collect::<Result<_>>()?;
    let mut chunk: Vec<(u64, u32)> = Vec::with_capacity(staged_rows);
    // The join's answer, held apart from the scatter that consumes it: `(entity, staged position)`
    // for every row of the chunk that resolved, in the sweep's order. **This is what makes the
    // scatter divisible** — the columns can then be walked independently over one shared,
    // read-only list, where the old shape did every column's write inside the sweep's own callback
    // and so did all of them on the sweep's thread.
    let mut resolved: Vec<(u32, u32)> = Vec::with_capacity(staged_rows);

    // How many entities came away with a value in each of this group's columns — counted where the
    // value is moved across, which is the only place presence is known without a second scan.
    let mut present = vec![0u64; attributes.len()];

    // Everything mutable is a parameter rather than a capture, so the scan's callback and this can
    // both hold it — the shape the geometry pass's `resolve` uses, and for the same borrow reason.
    let flush = |chunk: &mut Vec<(u64, u32)>,
                 resolved: &mut Vec<(u32, u32)>,
                 staged: &mut [EntityColumn],
                 by_entity: &mut [EntityColumn],
                 matched: &mut u64,
                 unknown: &mut u64,
                 present: &mut [u64]|
     -> Result<()> {
        resolved.clear();
        join_chunk(chunk, source_ids, |ordinal, _source_id, pos| {
            match ordinal {
                None => *unknown += 1,
                Some(ordinal) => {
                    *matched += 1;
                    resolved.push((entity_of_ordinal[ordinal as usize], pos));
                }
            }
            Ok(())
        })?;
        {
            // **One lane per column, and the columns share nothing.** Each entity-order column is
            // its own mapped array with its own presence bits, so a chunk's scatter splits across
            // them with no synchronisation at all: `resolved` is read-only, and every write a lane
            // makes — the value, the presence bit, the arena append, its own share of `present` —
            // lands in storage no other lane can name.
            //
            // **Indexed rather than zipped**, because a group's columns are a subsequence of the
            // declaration: the staged buffer is this group's, and each of its columns lands in the
            // slot the declaration gave that attribute.
            let mut homes: Vec<Option<&mut EntityColumn>> =
                by_entity.iter_mut().map(Some).collect();
            let mut lanes: Vec<(usize, &mut EntityColumn, &mut EntityColumn)> = group
                .attributes
                .iter()
                .zip(staged.iter_mut())
                .map(|(&column, src)| {
                    let home = homes[column].take().expect(
                        "an attribute is read from exactly one source, so one lane owns it",
                    );
                    (column, src, home)
                })
                .collect();
            // Collected per lane and folded in lane order, so a build that fails here fails with
            // the same column's message every time: which lane finished first is a scheduling
            // detail, and a build error that moves with it cannot be reproduced from its report.
            let counted: Vec<Result<u64>> = lanes
                .par_iter_mut()
                .map(|(column, src, home)| {
                    let name = &args.schema.attributes[*column].name;
                    let mut count = 0u64;
                    for &(entity, pos) in resolved.iter() {
                        if src.is_present(pos as usize) {
                            count += 1;
                        }
                        // Not wrapped with the column's name: every error this can raise already
                        // carries it (`column.rs`) or names the file it could not write.
                        home.take_from(entity as usize, src, pos as usize, name)?;
                    }
                    Ok(count)
                })
                .collect();
            for (slot, lane) in present.iter_mut().zip(counted) {
                *slot += lane?;
            }
        }
        // Every string in the chunk has been moved across, so the staging arena starts the next
        // chunk empty rather than growing to the whole source's payload — the buffer is reused and
        // its bytes are appended (`column.rs`).
        for column in staged.iter_mut() {
            column.reset_staging();
        }
        Ok(())
    };

    input::scan_attributes(
        &group.path,
        &group.fields,
        attributes,
        minters,
        args.limit,
        // An attribute source is entity space: one value per entity, in a file of its own, with
        // no view to select (`views.md` §5).
        None,
        |batch| {
            // Flushed **before** the batch rather than after a row count is reached, because a
            // batch is staged as a unit. Chunk boundaries are unobservable in the output — see
            // [`JOIN_STAGE_BYTES`] — so where one falls is free to be whatever keeps the buffer
            // bounded.
            if !chunk.is_empty() && chunk.len() + batch.rows.len() > staged_rows {
                flush(
                    &mut chunk,
                    &mut resolved,
                    &mut staged,
                    by_entity,
                    &mut matched_rows,
                    &mut unknown_rows,
                    &mut present,
                )?;
            }
            let base = chunk.len();
            for (offset, &row) in batch.rows.iter().enumerate() {
                chunk.push((batch.ids[row as usize], (base + offset) as u32));
            }
            // The same lane-per-column split the scatter makes, over the same argument: resolving
            // a row's value and staging it are per column, and the staging columns share nothing.
            // This is where the pass spent most of its time — ~11 s of GeoNames' 17.6 s, against
            // 1.6 s in the Parquet reader (`input::scan_attributes`).
            let filled: Vec<Result<()>> = staged
                .par_iter_mut()
                .zip(batch.decoded.par_iter())
                .zip(attributes.par_iter())
                .map(|((dst, decoded), attribute)| {
                    for (offset, &row) in batch.rows.iter().enumerate() {
                        let value = decoded.value(row as usize, attribute, &args.schema)?;
                        dst.set(base + offset, value, &attribute.name)?;
                    }
                    Ok(())
                })
                .collect();
            // Folded in declaration order, for the reason the scatter's tally is.
            for lane in filled {
                lane?;
            }
            Ok(())
        },
    )?;
    if !chunk.is_empty() {
        flush(
            &mut chunk,
            &mut resolved,
            &mut staged,
            by_entity,
            &mut matched_rows,
            &mut unknown_rows,
            &mut present,
        )?;
    }
    drop(staged);
    drop(chunk);
    drop(resolved);
    coverage.push(crate::AttributeCoverage {
        source: group.name.clone(),
        entities: n,
        matched_rows,
        unknown_rows,
        columns: attributes
            .iter()
            .zip(&present)
            .map(|(a, &count)| (a.name.clone(), count))
            .collect(),
    });
    Ok(())
}

/// **The group-scoped attribute column families** (`views.md` §5): one entity-space column per
/// view of the group, each with its own presence bitmap, under `attrs/<column>/<group>/<key>/`.
///
/// **Entity space, one column per view, and nothing per row space** — which is what keeps a scoped
/// attribute inside I2's argument: every value is indexed by entity, so a predicate over it would
/// answer a bitmap in entity space and meet the mask there, before any permutation.
///
/// The values are each view's own: the column is read from the view's points file, under that
/// view's selection where a group's views share one file (`views.md` §3.1's form B). An entity the
/// view does not hold, and one whose row carries a null, are the same state — absent, the presence
/// bitmap's ordinary case (decision 0064).
///
/// **The family's record is `MANIFEST.groups[..].scoped_scalars`** (contracts §2.2), written from
/// [`BuildArgs::scoped_attributes`] beside these files: `MANIFEST.declared_scalars` is one flat
/// bundle-wide list with no slot for a family, so the group — which is what a pin resolves
/// against — is where the declaration is recorded. A numeric or keyword family is a filter operand
/// from there, one column per view, resolved by the request's view or by a pinned leaf.
///
/// ⊘ **A category or text family is written and served from nowhere**, its per-view postings being
/// unwritten, and ⊘ **`render` buys nothing for any scoped family** — the hot column is per row
/// space and a scoped column is in none of them. Both are printed at the build, where an operator
/// can still act on them.
#[allow(clippy::too_many_arguments)]
fn write_scoped_columns(
    args: &BuildArgs,
    partition_dir: &Path,
    n: u64,
    source_ids: &[u64],
    entity_of_ordinal: &[u32],
    minters: &mut std::collections::HashMap<String, tessera_store::vocabulary::VocabularyMinter>,
    scratch: &crate::column::ColumnScratch,
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for family in &args.scoped_attributes {
        let attribute = &family.attribute;
        // ⊘ Said at the build rather than left to the design's marker: a declaration that asked
        // for a placement and got less than it asked for is a gap an operator should hear about
        // where they can still act on it. **What `index` buys now depends on the family**: a
        // numeric or keyword family is on the filter surface, one column per view, resolved by
        // the request's view or by a pin (`views.md` §5); a category owes per-view postings that
        // no pass writes, and a text column owes a per-view dictionary and postings, so neither
        // is published as an operand at all.
        if attribute.index && !served_scope(attribute) {
            eprintln!(
                "attribute '{}': ⊘ `index` on a `{}`-family attribute scoped to group '{}' has \
                 nothing to act on — the per-view postings a {} column is answered from are not \
                 written, so the family is stored and is on no filter surface (views §5)",
                attribute.name,
                scope_family(attribute),
                family.group,
                scope_family(attribute),
            );
        }
        // ⊘ `render` is unbuilt for every scoped family, whatever its type: the hot column is per
        // row space and a scoped column is in no row's tail, so there is nothing for a rendered
        // value to occupy. Said every time rather than once, because the declaration is what asked.
        if attribute.render {
            eprintln!(
                "attribute '{}': ⊘ `render` on an attribute scoped to group '{}' has nothing to \
                 act on — the hot column is per row space and a scoped column is in none of them, \
                 so the value is rendered in no view (views §5)",
                attribute.name, family.group
            );
        }
        for &index in &family.views {
            let view = &args.views[index];
            let column = read_scoped_column(
                args,
                attribute,
                view,
                n,
                source_ids,
                entity_of_ordinal,
                minters,
                scratch,
            )?;
            // `attrs/<column>/<group>/<key>/` — the view id's own path components, through the
            // one place a view id becomes a path (`tessera_store::view_path`).
            let mut column_dir = partition_dir.join("attrs").join(&attribute.name);
            for component in tessera_store::view_path_components(&view.view_id) {
                column_dir.push(component);
            }
            std::fs::create_dir_all(&column_dir).map_err(|e| BuildError::io(&column_dir, e))?;
            let values_path = column_dir.join("values.arrow");
            let presence_path = column_dir.join("presence.roaring");
            let written = write_column_values(
                &column_dir,
                &values_path,
                &presence_path,
                attribute,
                &column.values,
            )?;
            fsync_file(&values_path)?;
            paths.push(values_path);
            if let Some(dict_path) = written.dict {
                fsync_file(&dict_path)?;
                paths.push(dict_path);
            }
            if written.presence {
                fsync_file(&presence_path)?;
                paths.push(presence_path);
            }
            // Printed per column of the family, where an entity-scoped column's coverage is
            // printed: a scoped column covers the view's own rows, so *fewer than the corpus* is
            // its ordinary state rather than a symptom.
            eprintln!(
                "attribute '{}' in view '{}': {} of {} entities have a value",
                attribute.name,
                view.view_id,
                crate::thousands(column.present),
                crate::thousands(n)
            );
        }
    }
    Ok(paths)
}

/// Is this scoped attribute's **family** one the filter surface serves — the family half of the
/// engine's `filter::scoped_is_filterable`, over the build's own types. (The other half is
/// `index`, which the caller has already read.)
///
/// The two must agree: a family this says is served and the engine does not would be a column
/// written for a surface that never publishes it, and the reverse would be an operand published
/// over artefacts no pass wrote.
fn served_scope(attribute: &crate::config::Attribute) -> bool {
    // Numeric and keyword: a value column, a presence bitmap and — for a keyword — its
    // dictionary, which is the whole of what the entity route reads. A category owes per-view
    // postings and `/v1/categories` owes a value list derived from them; a text column owes a
    // per-view dictionary and postings and no value column at all. Neither is written.
    attribute.vocabulary.is_none() && attribute.ty != ScalarType::Text
}

/// The family name the message above uses — the engine's own spellings.
fn scope_family(attribute: &crate::config::Attribute) -> &'static str {
    if attribute.vocabulary.is_some() {
        "category"
    } else if attribute.ty == ScalarType::Text {
        "text"
    } else if attribute.ty == ScalarType::Keyword {
        "keyword"
    } else {
        "numeric"
    }
}

/// One view's column of a group-scoped attribute, in entity space ([`write_scoped_columns`]).
struct ScopedColumn {
    values: EntityColumn,
    present: u64,
}

/// Read one view's values of a group-scoped attribute out of that view's points file.
///
/// The same resolution every other attribute pass makes — `source_ids` → ordinal →
/// `entity_of_ordinal` — because entity ids are assigned in signature-sorted order (§11.1) and a
/// source id is not its own entity id. A row naming an entity this build did not load is counted
/// nowhere and refused nowhere: it is the join's ordinary case, exactly as it is for an
/// entity-scoped source.
#[allow(clippy::too_many_arguments)]
fn read_scoped_column(
    args: &BuildArgs,
    attribute: &crate::config::Attribute,
    view: &crate::ViewArgs,
    n: u64,
    source_ids: &[u64],
    entity_of_ordinal: &[u32],
    minters: &mut std::collections::HashMap<String, tessera_store::vocabulary::VocabularyMinter>,
    scratch: &crate::column::ColumnScratch,
) -> Result<ScopedColumn> {
    let mut values = EntityColumn::filled(scratch, attribute.ty, n as usize)?;
    let columns = [attribute];
    let staged_rows = staging_rows(&columns, n).max(input::ATTRIBUTE_BATCH_ROWS);
    let mut staged = EntityColumn::filled(scratch, attribute.ty, staged_rows)?;
    let mut chunk: Vec<(u64, u32)> = Vec::with_capacity(staged_rows);
    let mut present = 0u64;
    // The merge sweep the entity-scoped pass uses, over one column: both sides ascend, so the
    // join is sequential rather than a probe into `source_ids` per row.
    let flush = |chunk: &mut Vec<(u64, u32)>,
                 staged: &mut EntityColumn,
                 values: &mut EntityColumn,
                 present: &mut u64|
     -> Result<()> {
        let mut moved: Vec<(u32, u32)> = Vec::with_capacity(chunk.len());
        join_chunk(chunk, source_ids, |ordinal, _source_id, pos| {
            if let Some(ordinal) = ordinal {
                moved.push((entity_of_ordinal[ordinal as usize], pos));
            }
            Ok(())
        })?;
        for (entity, pos) in moved {
            if staged.is_present(pos as usize) {
                *present += 1;
            }
            values.take_from(entity as usize, staged, pos as usize, &attribute.name)?;
        }
        staged.reset_staging();
        Ok(())
    };
    input::scan_attributes(
        &view.points,
        &view.point_fields,
        &columns,
        minters,
        args.limit,
        view.select.as_ref(),
        |batch| {
            if !chunk.is_empty() && chunk.len() + batch.rows.len() > staged_rows {
                flush(&mut chunk, &mut staged, &mut values, &mut present)?;
            }
            let base = chunk.len();
            for (offset, &row) in batch.rows.iter().enumerate() {
                chunk.push((batch.ids[row as usize], (base + offset) as u32));
                let value = batch.decoded[0].value(row as usize, attribute, &args.schema)?;
                staged.set(base + offset, value, &attribute.name)?;
            }
            Ok(())
        },
    )?;
    if !chunk.is_empty() {
        flush(&mut chunk, &mut staged, &mut values, &mut present)?;
    }
    Ok(ScopedColumn { values, present })
}

/// Write the entity-space filter postings for every column declared `index = true`, and
/// return the paths so the manifest digests them.
///
/// **One file per column** — `attrs/<column>/postings.arrow` — never one file for the whole
/// schema. A shared file would have to address a value as `base + local` over per-column extents,
/// and under ingest a value minted after the build takes an ordinal belonging to the *next*
/// column; the base-union-tiers read then merges one value's members into another's. Because
/// vocabulary visibility is membership-derived, that shows a value to a principal on the strength
/// of a different value's members — leak-register row C11, reachable by ordinary operation
/// (filter-index §2.2).
///
/// **The keyed record format** (filter-index §2.5), not the positional one the authorisation index
/// uses. A category's identifier is its vocabulary code, and codes are a sparse subset of a
/// 32-bit space by construction — [`tessera_store::vocabulary`] mints them scattered so that the
/// code itself carries no ordering information. A positional file would need a record per code up
/// to the largest one drawn; the keyed file stores `(code, posting)` ascending and binary-searches.
///
/// **Code 0 is the reserved *absent* code**, so an entity carrying it gets no posting. This is the
/// same zero the entity-major buffer is initialised to, which is safe only because the reader's
/// count check upstream proves every entity was visited — an unvisited entity would be
/// indistinguishable from one with no value, and here that is the difference between "absent from
/// every filter result" and "silently in whichever bucket zero names".
///
/// Entities are swept ascending, so each posting's entity list arrives sorted and
/// `encode_posting`'s unconditional sortedness check re-verifies the property this loop
/// established rather than taking it on trust.
///
/// **The emit bands code space** (filter-index §6.2), which is what keeps its transient a constant
/// the caller chooses rather than 4 B per present entity — ~4 GB per fully covered category column
/// at 10⁹, on every build, in the same pipeline whose *authorisation* emit bands term space
/// precisely to avoid this shape. A counting pass sizes each band from `band_rows`, a code is never
/// split across a band, and each band is one column scan cursor-scattering into a flat buffer laid
/// out by prefix sums. Bands partition ascending code space, so [`KeyedPostingsSpool`]'s
/// ascending-key check holds across them unchanged.
///
/// What the banding does *not* bound is the attribute tail this reads from — but that tail is no
/// longer memory: `by_entity` is mapped (`column.rs`), so what this pass reads is page cache the
/// kernel may reclaim, and the band budget bounds the only anonymous buffer left in the emit. This
/// pass adds no second copy of the column to either.
pub(crate) fn write_filter_postings(
    partition_dir: &Path,
    schema: &crate::config::Schema,
    by_entity: &[EntityColumn],
    memory_budget: u64,
) -> Result<(Vec<PathBuf>, TextIndexCost)> {
    let entities = by_entity.first().map_or(0, EntityColumn::len);
    write_filter_postings_banded(
        partition_dir,
        schema,
        by_entity,
        POSTINGS_BAND_ROWS,
        TextIndexPlan::for_budget(memory_budget, entities),
    )
}

/// What the text columns cost inside [`write_filter_postings`]'s loop: the wall time and the terms
/// written, for [`BuildStage::TextIndex`].
///
/// **Measured here rather than at a stage boundary** because the loop visits a text column and a
/// category column in whatever order the schema declares them, and the two are different code paths
/// at different costs — a tokenise and a dictionary against a fixed-width scan. One number over
/// both is what a reader cannot act on.
#[derive(Default)]
pub(crate) struct TextIndexCost {
    pub(crate) elapsed: std::time::Duration,
    pub(crate) terms: u64,
}

/// Entity ids the postings emit holds in flight — the shared emit's own constant, so the build and
/// the fold band identically (filter-index §6.2).
use tessera_filter_write::POSTINGS_BAND_ROWS;

fn write_filter_postings_banded(
    partition_dir: &Path,
    schema: &crate::config::Schema,
    by_entity: &[EntityColumn],
    band_rows: usize,
    text_plan: TextIndexPlan,
) -> Result<(Vec<PathBuf>, TextIndexCost)> {
    let mut paths = Vec::new();
    let mut text = TextIndexCost::default();
    for (attribute, values) in schema.attributes.iter().zip(by_entity) {
        if !postings_are_owed(schema, attribute) {
            continue;
        }
        let column_dir = partition_dir.join("attrs").join(&attribute.name);
        std::fs::create_dir_all(&column_dir).map_err(|e| BuildError::io(&column_dir, e))?;

        // **A text column has no value column**, so it leaves before the one below is written. Its
        // entity-space artefact is the token dictionary and the postings over it; the values
        // themselves are in the record blob, which no scan reads (records §4.4).
        if attribute.ty == ScalarType::Text {
            let started = std::time::Instant::now();
            let written = write_text_index(&column_dir, attribute, values, text_plan)?;
            text.elapsed += started.elapsed();
            text.terms += written.terms;
            paths.extend(written.paths);
            continue;
        }

        // The value column is the artefact of record (filter-index §2.1); the postings below are
        // derived from it. Written first so that a build interrupted between the two leaves the
        // record without its accelerator rather than an accelerator with no record.
        let values_path = column_dir.join("values.arrow");
        let presence_path = column_dir.join("presence.roaring");
        let written =
            write_column_values(&column_dir, &values_path, &presence_path, attribute, values)?;
        fsync_file(&values_path)?;
        paths.push(values_path);
        // The dictionary is not an accelerator and the ordering above does not apply to it: a
        // keyword's value column holds ordinals, which name nothing without the dictionary they
        // index. Both are digested, so a build interrupted between them refuses at open either
        // way — the pair is the artefact of record, not the values file alone (records §7).
        if let Some(dict_path) = written.dict {
            fsync_file(&dict_path)?;
            paths.push(dict_path);
        }
        if written.presence {
            fsync_file(&presence_path)?;
            paths.push(presence_path);
        }

        // **Only a category earns an accelerator**, and the test is *has a vocabulary* rather than
        // *is not a string*: its values already carry a code and repeat heavily, so one bitmap
        // replaces millions of repeated codes. A string's values carry no such identity, and a
        // numeric's are typically near-unique — for either, a posting per value is a second copy of
        // the column and nothing more. Both are the value column alone (filter-index §2.3, §3),
        // which is also what makes the scan the whole operation rather than a verification step
        // behind an index.
        if attribute.vocabulary.is_none() {
            continue;
        }

        let path = column_dir.join("postings.arrow");
        write_category_postings(&path, &attribute.name, values, band_rows)?;
        fsync_file(&path)?;
        paths.push(path);
    }
    Ok((paths, text))
}

/// Write the record blob — `attrs/record/{blocks.bin,hasrow.roaring,directory.arrow}` — for every
/// declared column that is neither indexed nor rendered, and return the paths so the manifest
/// digests them like every base artefact (records §3, §7).
///
/// **The blob is the third home**: a column with neither flag has no entity-space structure and no
/// slot in the hot column, so these files are the only place its values exist. When the schema
/// declares no such column the stage writes nothing and the open demands nothing — the file set
/// stays a function of the schema. (On this branch the schema *parse* still refuses a
/// neither-column; the declaration surface that admits one lands with the `used_for` migration in
/// this same epic, so the case is reachable programmatically and, after integration, from TOML.)
///
/// **Absence is per family, exactly as the value column spells it** (`write_column_values`): a
/// category's absence is the reserved code 0, everything else's is `ScalarValue::Null`. An entity
/// absent from every blob column gets no row and no has-row bit. The stage is otherwise
/// family-agnostic — a blob row is bytes, whatever family supplied them — which is what makes the
/// keyword epic's storage swap a tag rename plus rebuild rather than a format change. Whether a
/// vocabulary-controlled category may be blob-resident at all is store-once's open ruling; this
/// stage writes what the compiled placement says and takes no view.
///
/// The field tag is the column's position among `declared_scalars` — the same positional identity
/// the hot column's tail and the ingest row vector already rely on — so drill-down resolves it
/// against the manifest without any name table in the artefact.
pub(crate) fn write_record_blob(
    partition_dir: &Path,
    schema: &crate::config::Schema,
    by_entity: &[EntityColumn],
) -> Result<Vec<PathBuf>> {
    // **Blob-resident is "no other home", not "no flags"** — and for a category the two differ.
    // Records §4.2 exempts categories from the blob because their entity-space structures are the
    // vocabulary machinery's constant floor, but that floor is `postings_are_owed`, which holds
    // for an *indexed* or `derived` category and not for a `public` one. A `public` category
    // declared with neither flag therefore has no hot column, no value column and no postings, so
    // excluding every category here stored its values nowhere at all and refused nothing —
    // silent loss of a field the caller declared. Asking the same question the entity-space pass
    // asks is what keeps the two exhaustive between them: a field is in exactly one home, and
    // records §3's rule that every declared field answers `entity → value` holds by construction.
    let blob_columns: Vec<usize> = schema
        .attributes
        .iter()
        .enumerate()
        // **Text is blob-resident whether or not it is indexed** (records §4.4), which is the one
        // place this predicate is not simply "has no other home": an indexed text column has a
        // token index *and* a blob row, because the index answers `match` and only the blob can
        // answer `entity → value`. Postings are term → entities; nothing in them reconstructs the
        // prose a drill-down returns.
        .filter(|(_, a)| a.ty == ScalarType::Text || (!a.render && !postings_are_owed(schema, a)))
        .map(|(i, _)| i)
        .collect();
    if blob_columns.is_empty() {
        return Ok(Vec::new());
    }
    // `record` is reserved as a column name at parse (records §7, review N10), so this directory
    // cannot collide with a declared column's.
    let record_dir = partition_dir.join("attrs").join("record");
    std::fs::create_dir_all(&record_dir).map_err(|e| BuildError::io(&record_dir, e))?;
    let blocks_path = record_dir.join(RECORD_BLOCKS_FILE);
    let hasrow_path = record_dir.join(RECORD_HASROW_FILE);
    let directory_path = record_dir.join(RECORD_DIRECTORY_FILE);
    let mut writer = RecordBlobWriter::create(
        &blocks_path,
        &hasrow_path,
        &directory_path,
        RECORD_BLOCK_TARGET,
    )
    .map_err(|e| BuildError::io(&blocks_path, e))?;

    let n = by_entity.first().map_or(0, EntityColumn::len);
    let mut fields: Vec<RecordField> = Vec::with_capacity(blob_columns.len());
    // A range loop on purpose: each entity gathers across *several* parallel columns, which is
    // not the single-view shape `needless_range_loop`'s rewrite fits.
    #[allow(clippy::needless_range_loop)]
    for entity in 0..n {
        fields.clear();
        for &column in &blob_columns {
            let attribute = &schema.attributes[column];
            let Some(value) = record_value_of(&by_entity[column], entity, attribute)? else {
                continue;
            };
            let tag = u16::try_from(column).map_err(|_| {
                BuildError::Invalid(format!(
                    "attribute '{}' is declared at position {column}, past the u16 field-tag \
                     space",
                    attribute.name
                ))
            })?;
            fields.push(RecordField { tag, value });
        }
        if fields.is_empty() {
            continue;
        }
        writer
            .push_row(entity as u32, &fields)
            .map_err(|e| BuildError::io(&blocks_path, e))?;
    }
    writer
        .finish()
        .map_err(|e| BuildError::io(&blocks_path, e))?;
    for path in [&blocks_path, &hasrow_path, &directory_path] {
        fsync_file(path)?;
    }
    Ok(vec![blocks_path, hasrow_path, directory_path])
}

/// One staged value as the blob row carries it, or `None` where the entity carries nothing in
/// this column — the per-family absence rule `write_record_blob`'s doc states.
///
/// **The column and the entity, not the value**, so that the string arm can borrow: reading through
/// `EntityColumn::value_at` clones the `String` and [`RecordValue::Utf8`] then owns a second copy,
/// which over a text column is two allocations and two copies of every value in the corpus. One
/// clone remains and is unavoidable — the record value owns its bytes.
fn record_value_of(
    values: &EntityColumn,
    entity: usize,
    attribute: &crate::config::Attribute,
) -> Result<Option<RecordValue>> {
    if attribute.vocabulary.is_some() {
        let code = category_code(&values.value_at(entity), &attribute.name)?;
        if code == tessera_store::vocabulary::ABSENT_CODE {
            return Ok(None);
        }
        // The code at the declared width — the value the entity-space column would have stored,
        // resolved to its key at drill-down through the manifest's vocabulary, never in the
        // artefact.
        return Ok(Some(match attribute.ty {
            ScalarType::U8 => RecordValue::U8(code as u8),
            ScalarType::U16 => RecordValue::U16(code as u16),
            _ => RecordValue::U32(code),
        }));
    }
    // The string families first, borrowed. `str_at` answers `None` for an absent entity and for a
    // column that is not string-backed, and the match below then reads the same absence out of
    // `value_at` — so the two agree without either having to know which family it is looking at.
    if let Some(text) = values.str_at(entity) {
        return Ok(Some(RecordValue::Utf8(text.to_string())));
    }
    Ok(match values.value_at(entity) {
        ScalarValue::Null => None,
        ScalarValue::Bool(v) => Some(RecordValue::Bool(v)),
        ScalarValue::U8(v) => Some(RecordValue::U8(v)),
        ScalarValue::U16(v) => Some(RecordValue::U16(v)),
        ScalarValue::U32(v) => Some(RecordValue::U32(v)),
        ScalarValue::U64(v) => Some(RecordValue::U64(v)),
        ScalarValue::I8(v) => Some(RecordValue::I8(v)),
        ScalarValue::I16(v) => Some(RecordValue::I16(v)),
        ScalarValue::I32(v) => Some(RecordValue::I32(v)),
        ScalarValue::I64(v) => Some(RecordValue::I64(v)),
        ScalarValue::F32(v) => Some(RecordValue::F32(v)),
        ScalarValue::F64(v) => Some(RecordValue::F64(v)),
        ScalarValue::TimestampUs(v) => Some(RecordValue::TimestampUs(v)),
        ScalarValue::Utf8(v) => Some(RecordValue::Utf8(v)),
    })
}

/// The staged attribute values of one category column, as the shared postings emit reads them.
///
/// **The emit itself lives in `tessera-filter`** (`fold::write_category_postings`), because the
/// fold rebuilds these postings from the folded column and filter-index §6.2 makes one writer
/// rather than two producers that agree the byte-identity argument. What is here is the adaptation:
/// the build's source is a `ScalarValue` per entity, where the fold's is a value column.
struct StagedCategory<'a> {
    values: &'a EntityColumn,
    column: &'a str,
}

impl tessera_filter_write::CategorySource for StagedCategory<'_> {
    fn for_each(&self, f: &mut dyn FnMut(u32, u32) -> std::io::Result<()>) -> std::io::Result<()> {
        for (entity, value) in self.values.iter().enumerate() {
            let code = category_code(&value, self.column).map_err(std::io::Error::other)?;
            // Code 0 is the reserved *absent* code, so an entity carrying it gets no posting. This
            // is the same zero the entity-major buffer is initialised to, which is safe only
            // because the reader's count check upstream proves every entity was visited — an
            // unvisited entity would otherwise be indistinguishable from one with no value.
            if code == tessera_store::vocabulary::ABSENT_CODE {
                continue;
            }
            f(entity as u32, code)?;
        }
        Ok(())
    }
}

fn write_category_postings(
    path: &Path,
    column: &str,
    values: &EntityColumn,
    band_rows: usize,
) -> Result<()> {
    tessera_filter_write::write_category_postings(
        path,
        column,
        &StagedCategory { values, column },
        band_rows,
    )
    .map_err(|e| BuildError::io(path, e))
}

/// What writing one column's values produced beside the values file itself.
struct WrittenColumn {
    /// Whether a presence bitmap was written — see [`write_column_values`] on why its absence is
    /// meaningful rather than an omission.
    presence: bool,
    /// The layer's sorted dictionary, for the one family that has one.
    dict: Option<PathBuf>,
}

/// Write one column's values in entity order, its presence bitmap where presence is partial, and
/// its dictionary where the family stores one.
///
/// **A column every entity carries a value in gets no presence bitmap**, and that is the fast path
/// rather than an omission: the entity id is then the array index, which measured 28.7 ms against a
/// presence-addressed 1,078 ms at 10⁹ (`probes/2026-08-08-filter-layout/`). Writing an all-ones
/// bitmap would be correct and would cost the scan that path, so the distinction lives in the file
/// set rather than in the bitmap's contents.
///
/// **Absence is out of band in every family, and by different means.** A category spends the
/// reserved code 0, which its vocabulary reserves out of the value space. A string has no spare
/// value to spend — the empty string is one a corpus may legitimately hold, and contracts §2.4
/// already refuses it on the ingest plane because an unset field and a client bug both produce it —
/// so absence arrives as `ScalarValue::Null`. Folding the two together would report an item as
/// matching a value it does not have. A keyword inherits the string rule exactly: absence is
/// `ScalarValue::Null` and never ordinal 0, which is an ordinary key like any other.
///
/// **A keyword's values file holds `u32` ordinals into the dictionary written beside it, and both
/// belong to this layer alone** (records §4.3). The base build is one layer, so the ordinals here
/// are positions in *this* base's dictionary and mean nothing against any extent's. Nothing
/// downstream may assume otherwise — which is what keeps a durable manufactured identity, and the
/// reuse hazard that comes with one, out of the family.
fn write_column_values(
    column_dir: &Path,
    values_path: &Path,
    presence_path: &Path,
    attribute: &crate::config::Attribute,
    values: &EntityColumn,
) -> Result<WrittenColumn> {
    let mut presence = Presence::default();
    let mut dict = None;

    let mut writer = ValueColumnWriter::create(values_path, presence_path, column_kind(attribute))
        .map_err(|e| BuildError::io(values_path, e))?;
    macro_rules! push {
        ($chunk:expr) => {
            writer
                .push(&$chunk)
                .map_err(|e| BuildError::io(values_path, e))?
        };
    }

    // **A string's absence is not an empty string**, and a category's is not code 0 by coincidence:
    // in both families the "carries nothing" marker is out of band. A category spends the reserved
    // code 0, which its vocabulary reserves out of the value space; a string has no spare value to
    // spend — the empty string is one a corpus may legitimately hold — so absence arrives as
    // `ScalarValue::Null` and the empty string arrives as itself.
    if attribute.ty == ScalarType::Keyword {
        let present_values = keyword_values(attribute, values, &mut presence)?;
        // The distinct key set, sorted — the dictionary's contents and, by position, the ordinals
        // the column stores. `sort_unstable` is sound where a stable sort would not be, because
        // the elements compared are the keys themselves: equal elements are indistinguishable, and
        // `dedup` then leaves one of each.
        let mut keys: Vec<&str> = present_values.clone();
        keys.sort_unstable();
        keys.dedup();
        let dict_path = column_dir.join(tessera_filter::DICT_FILE);
        tessera_filter::write_sorted_dict(&dict_path, keys.iter().copied())
            .map_err(|e| BuildError::io(&dict_path, std::io::Error::from(e)))?;
        dict = Some(dict_path);

        // The ordinal is the key's position in the sorted distinct set, which is exactly the
        // ordinal the writer assigned it: `SortedDictWriter::push` returns positions in the order
        // it is fed, and it was fed this vector. Searching rather than threading the writer's
        // return values through keeps that equality checkable in one line instead of resting on
        // two loops staying in step.
        let mut held: Vec<u32> = Vec::new();
        for text in present_values {
            let ordinal = keys.binary_search(&text).map_err(|_| {
                BuildError::Invalid(format!(
                    "attribute '{}': the value {text:?} is absent from the dictionary built from \
                     it — the ordinal column would name a different value's key",
                    attribute.name
                ))
            })?;
            held.push(ordinal as u32);
            if held.len() >= VALUE_CHUNK {
                push!(Codes::U32(std::mem::take(&mut held).into()));
            }
        }
        if !held.is_empty() {
            push!(Codes::U32(held.into()));
        }
    } else if attribute.vocabulary.is_some() {
        let mut held: Vec<u32> = Vec::new();
        for (entity, value) in values.iter().enumerate() {
            let code = category_code(&value, &attribute.name)?;
            if code == tessera_store::vocabulary::ABSENT_CODE {
                continue;
            }
            presence.present(entity as u32);
            held.push(code);
            if held.len() >= VALUE_CHUNK {
                push!(category_chunk(attribute.ty, &held));
                held.clear();
            }
        }
        if !held.is_empty() {
            push!(category_chunk(attribute.ty, &held));
        }
    } else {
        // **A number's absence is the presence bitmap, because a number has no spare value to
        // spend** (decision 0064). A category reserves code 0 out of its vocabulary and a string
        // carries an explicit null; every bit pattern of an integer is a legal integer, so there is
        // nothing in band to mean "no score". The bitmap beside the column is where it goes, which
        // is the same mechanism the other two families already use — so absence has one
        // representation across all three rather than three.
        //
        // Reached only where the source said so: `BatchColumn::value` returns `ScalarValue::Null`
        // for a null slot and the values buffer's zero otherwise, which is the distinction that
        // used to be dropped at decode.
        //
        // The presence bit *is* that distinction — a slot is null exactly where the bit is clear,
        // whatever the family — so this walks the bits and skips an absent run 64 at a time rather
        // than materialising a `ScalarValue` per entity to ask it the same question.
        for entity in values.present_entities() {
            presence.present(entity as u32);
        }
        push_numeric_chunks(&mut writer, values_path, attribute, values)?;
    }
    let presence = presence.written(values.len());
    writer
        .finish(presence)
        .map_err(|e| BuildError::io(values_path, e))?;
    Ok(WrittenColumn {
        presence: presence.is_some(),
        dict,
    })
}

/// The presence bitmap a column may or may not owe, **populated only once an absence proves it
/// will be written**.
///
/// A column every entity carries a value in gets no bitmap at all ([`write_column_values`] on why
/// that is the fast path and not an omission), and the pass that discovers this used to build the
/// bitmap anyway: 7.4×10⁷ `add` calls per column, thrown away at the last line. Here the sweep
/// reports only the entities that carry a value, ascending; absence is inferred from the gaps and
/// from the tail, and the bitmap is materialised at the first gap by replaying the run before it.
///
/// **The replay is a loop of `add`, not an `add_range`**, so the bitmap receives exactly the call
/// sequence the eager version made — same entities, same ascending order, same container
/// promotions, and so the same serialised bytes. A range insert may produce a run container where
/// individual inserts produce an array one, which is a different file for the same set.
#[derive(Default)]
struct Presence {
    bitmap: croaring::Bitmap,
    /// The entity after the last one reported present, while no gap has been seen.
    run: u32,
    materialised: bool,
}

impl Presence {
    /// Report that `entity` carries a value. Entities must arrive ascending.
    fn present(&mut self, entity: u32) {
        if !self.materialised {
            if entity == self.run {
                self.run = entity + 1;
                return;
            }
            self.materialise();
        }
        self.bitmap.add(entity);
        self.run = entity + 1;
    }

    /// The bitmap to write, or `None` where the column is universal — every entity of `len`
    /// present, which is the case the file set states by leaving the bitmap out.
    fn written(&mut self, len: usize) -> Option<&croaring::Bitmap> {
        if !self.materialised {
            if self.run as usize == len {
                return None;
            }
            self.materialise();
        }
        Some(&self.bitmap)
    }

    fn materialise(&mut self) {
        for entity in 0..self.run {
            self.bitmap.add(entity);
        }
        self.materialised = true;
    }
}

/// One keyword column's present values, in entity order, with `presence` told which entities carry
/// one.
///
/// Separate from the ordinal emit so that the pass which decides *presence* is the pass which
/// decides *slots*: the k-th set bit's value is at slot k (filter-index §2.1), and the vector this
/// returns is the slot sequence, so the two cannot come to disagree about an absent entity.
///
/// **A keyword's values arrive as [`ScalarValue::Utf8`]**, because that is what the wire carries
/// (records §7) — the type names the storage, not the value in flight.
///
/// **The empty string is refused, where a `utf8` column stores it.** That is the families
/// differing, not this pass being stricter than it need be: records §7 refuses an empty keyword on
/// the ingest wire for the reason contracts §2.4 gives — an unset field and a client bug both
/// produce it — and the dictionary has no key for it either. A points file is not the ingest plane
/// and has no upstream check, so the refusal is here, naming the column and the entity a build
/// operator has to go and fix.
fn keyword_values<'a>(
    attribute: &crate::config::Attribute,
    values: &'a EntityColumn,
    presence: &mut Presence,
) -> Result<Vec<&'a str>> {
    let mut out = Vec::new();
    // Absent runs are skipped a word at a time; absence itself is what [`Presence`] reads out of
    // the gaps this leaves.
    for entity in values.present_entities() {
        // Borrowed, not read through `value_at`: this collects one `&str` per entity across the
        // whole column, so cloning here would be a second copy of every keyword in the corpus.
        let Some(text) = values.str_at(entity) else {
            return Err(BuildError::Invalid(format!(
                "attribute '{}' is declared `keyword` but carries {:?}",
                attribute.name,
                values.value_at(entity)
            )));
        };
        if text.is_empty() {
            return Err(BuildError::Invalid(format!(
                "attribute '{}' is declared `keyword` and entity {entity} carries the empty \
                 string, which is not a value (records §7, contracts §2.4 — an unset field and a \
                 client bug both produce it). Leave the cell null for absence",
                attribute.name
            )));
        }
        presence.present(entity as u32);
        out.push(text);
    }
    Ok(out)
}

/// Values pushed to the column writer at a time. The writer spools each chunk as it arrives, so
/// this is the whole of what the emit holds beside the attribute tail it reads from.
const VALUE_CHUNK: usize = 1 << 16;

/// A category chunk at the declared width.
///
/// The declared width is the storage width. Narrowing here rather than storing `u32` throughout is
/// worth a match arm: the column is priced at 1 GB per byte of width per 10⁹ items (Appendix A), so
/// a `u8` category stored as `u32` would cost 3 GB it does not need.
fn category_chunk(ty: ScalarType, held: &[u32]) -> Codes {
    match ty {
        ScalarType::U8 => Codes::U8(held.iter().map(|&c| c as u8).collect::<Vec<_>>().into()),
        ScalarType::U16 => Codes::U16(held.iter().map(|&c| c as u16).collect::<Vec<_>>().into()),
        _ => Codes::U32(held.to_vec().into()),
    }
}

/// The kind of column a declared attribute stores — the type the writer is created with, before
/// its first value arrives.
fn column_kind(attribute: &crate::config::Attribute) -> ColumnKind {
    // A keyword's values file is an ordinal column, not a string one: the strings live once each
    // in the dictionary beside it, and the scan reads fixed-width `u32`s at the fixed-width scan's
    // measured constants rather than at a string scan's (records §4.3).
    if attribute.ty == ScalarType::Keyword {
        return ColumnKind::U32;
    }
    if attribute.vocabulary.is_some() {
        return match attribute.ty {
            ScalarType::U8 => ColumnKind::U8,
            ScalarType::U16 => ColumnKind::U16,
            _ => ColumnKind::U32,
        };
    }
    match attribute.ty {
        // `bool` stores as `u8` and `timestamp_us` as the `i64` it is — the *type* carries the unit
        // into the manifest, and the storage and the comparison are an `i64`'s.
        ScalarType::Bool | ScalarType::U8 => ColumnKind::U8,
        ScalarType::U16 => ColumnKind::U16,
        ScalarType::U32 => ColumnKind::U32,
        ScalarType::U64 => ColumnKind::U64,
        ScalarType::I8 => ColumnKind::I8,
        ScalarType::I16 => ColumnKind::I16,
        ScalarType::I32 => ColumnKind::I32,
        ScalarType::I64 | ScalarType::TimestampUs => ColumnKind::I64,
        ScalarType::F32 => ColumnKind::F32,
        ScalarType::F64 => ColumnKind::F64,
        // `utf8` survives as the *wire* type of a keyword's value and of a category's key
        // (`DeclaredScalar::wire_type`); it is refused at the schema parse, so no declared
        // attribute carries it.
        ScalarType::Utf8 => unreachable!("`utf8` is not a declarable type"),
        ScalarType::Keyword => unreachable!("a keyword returns above"),
        // A text column's terms are `u32` ordinals into the layer's token dictionary, exactly as a
        // keyword's value is — what differs is how many a row has, which is the postings' business
        // and not this width's.
        ScalarType::Text => ColumnKind::U32,
    }
}

/// One numeric column's values, at the declared width, pushed in bounded chunks.
///
/// **`ScalarValue::Null` is skipped, not pushed**, which is what makes the slot addressing agree
/// with the presence bitmap [`write_column_values`] builds alongside: the *k*-th set bit's value is
/// at slot *k* (filter-index §2.1), so an absent entity must occupy no slot. Pushing a placeholder
/// instead would shift every later value along by one — every entity after the first absent one
/// reporting its neighbour's number, with no error anywhere.
fn push_numeric_chunks(
    writer: &mut ValueColumnWriter,
    values_path: &Path,
    attribute: &crate::config::Attribute,
    values: &EntityColumn,
) -> Result<()> {
    macro_rules! stream {
        ($variant:ident, $ctor:expr, $map:expr) => {{
            let mut out = Vec::with_capacity(VALUE_CHUNK);
            for v in values.iter() {
                match v {
                    ScalarValue::$variant(x) => out.push($map(x)),
                    // No slot at all — see this function's doc.
                    ScalarValue::Null => continue,
                    other => {
                        return Err(BuildError::Invalid(format!(
                            "attribute '{}' is declared {:?} but carries {other:?}",
                            attribute.name, attribute.ty
                        )))
                    }
                }
                if out.len() >= VALUE_CHUNK {
                    writer
                        .push(&$ctor(std::mem::take(&mut out).into()))
                        .map_err(|e| BuildError::io(values_path, e))?;
                    out.reserve(VALUE_CHUNK);
                }
            }
            if !out.is_empty() {
                writer
                    .push(&$ctor(out.into()))
                    .map_err(|e| BuildError::io(values_path, e))?;
            }
        }};
        ($variant:ident, $ctor:expr) => {
            stream!($variant, $ctor, |x| x)
        };
    }
    match attribute.ty {
        ScalarType::Bool => stream!(Bool, Codes::U8, u8::from),
        ScalarType::U8 => stream!(U8, Codes::U8),
        ScalarType::U16 => stream!(U16, Codes::U16),
        ScalarType::U32 => stream!(U32, Codes::U32),
        ScalarType::U64 => stream!(U64, Codes::U64),
        ScalarType::I8 => stream!(I8, Codes::I8),
        ScalarType::I16 => stream!(I16, Codes::I16),
        ScalarType::I32 => stream!(I32, Codes::I32),
        ScalarType::I64 => stream!(I64, Codes::I64),
        ScalarType::F32 => stream!(F32, Codes::F32),
        ScalarType::F64 => stream!(F64, Codes::F64),
        ScalarType::TimestampUs => stream!(TimestampUs, Codes::I64),
        ScalarType::Utf8 => unreachable!("`utf8` is not a declarable type"),
        ScalarType::Keyword => unreachable!("the caller handles a keyword before reaching here"),
        ScalarType::Text => unreachable!("the caller handles text before reaching here"),
    }
    Ok(())
}

/// How the text index's chunk pass is sized: the entities one chunk covers, and the bytes one
/// worker may accumulate before it spills a run.
///
/// **Both are needed, and neither would do alone.** The chunk count is what the pass parallelises
/// over, so it follows the machine; the byte budget is what bounds a worker's residency, and it
/// has to hold whatever the documents turn out to be. A column of short names and a column of
/// abstracts differ by two orders of magnitude in terms per entity, so a plan that sized chunks
/// alone would be a memory bound only for the corpus it was measured on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TextIndexPlan {
    chunk_entities: usize,
    worker_bytes: usize,
    merge_slice_cap: usize,
}

/// The share of the build's memory budget the text pass may hold across all its workers.
///
/// A sixteenth, and capped: the pass runs inside the entity-order tail `residency.rs` models —
/// every declared column is resident beside it — so this is a transient on top of the build's
/// largest resident set, not a stage with the machine to itself. A sixteenth of an auto-derived
/// budget on a 48 GB box is the 2 GiB cap.
///
/// **It is a ceiling and not a working set.** What the pass actually holds is `threads` live
/// workers' accumulators, and a worker reaches the budget only on a corpus whose documents are
/// long enough to fill it: 7.4×10⁷ Overture names over 96 chunks on twelve threads spilled one run
/// per chunk and 555 MB of run in total, so no worker came near it. The budget's job is to bound
/// the corpus that *would* exceed it, not to describe the one that does not.
///
/// ⊘ This term is deliberately **not** added to `residency.rs`'s model. It is a sixteenth of the
/// same budget the model is checked against, which is inside the factor of two that module states
/// as its own error bar, and adding it would turn builds that fit today into refusals.
const TEXT_BUDGET_SHARE: u64 = 16;
const TEXT_BUDGET_MAX: u64 = 2 << 30;
const TEXT_BUDGET_MIN: u64 = 128 << 20;

/// The floor on a chunk: below this, splitting entity space buys nothing and costs a spill file
/// per chunk. Every test corpus in the repository is one chunk by this rule, which is why the
/// chunk seam below exists for the tests that need several.
const TEXT_MIN_CHUNK_ENTITIES: usize = 1 << 16;

/// The floor on a worker's byte budget, so a tiny `--memory-budget` cannot derive a plan that
/// spills a run per document.
const TEXT_MIN_WORKER_BYTES: usize = 8 << 20;

/// What one distinct term costs the accumulator beyond its characters: the hash table's slot for
/// a `Box<[u8]>` and a `Vec<u32>`, the allocator's rounding on both, and the load factor the table
/// keeps. **A conservative estimate, not a measurement** — it is the input to a memory bound, so
/// erring high spills a run early and erring low is the failure the bound exists to prevent. The
/// characters are charged at twice their length for the same reason (malloc rounding on a short
/// key is most of the key).
const TEXT_TERM_ENTRY_BYTES: usize = 80;

/// What one posting costs: four bytes of `u32` charged at eight, because a `Vec` grows by doubling
/// and is on average half empty.
const TEXT_POSTING_BYTES: usize = 8;

/// Entities held as a `u32` slice for one merged term before the encode switches to Roaring.
///
/// **This is the merge's only unbounded term, and this is what bounds it.** A term carried by a
/// quarter of the corpus is 10⁸ entities at 10⁹ items, which as a `Vec<u32>` is 400 MB for one
/// record — the residency `tessera_authz::postings::encode_posting_bitmap` was added to remove
/// from compaction's fold, for exactly this reason. Above the cap the merge accumulates into a
/// `Bitmap` instead and encodes from that; the two encoders are byte-identical over the same set
/// (pinned by `postings::the_bitmap_and_slice_encoders_agree_byte_for_byte`), so the cap is a
/// memory knob and not a format decision. 16 MB, which no vocabulary reaches by accident.
const TEXT_MERGE_SLICE_CAP: usize = 1 << 22;

impl TextIndexPlan {
    /// The plan a build's memory budget derives.
    pub(crate) fn for_budget(budget: u64, entities: usize) -> TextIndexPlan {
        let threads = rayon::current_num_threads().max(1);
        let allowance = (budget / TEXT_BUDGET_SHARE).clamp(TEXT_BUDGET_MIN, TEXT_BUDGET_MAX);
        let worker_bytes = (allowance as usize / threads).max(TEXT_MIN_WORKER_BYTES);
        // **Eight chunks a thread, and the number is measured rather than chosen.** A
        // segmentation's cost per entity varies by an order of magnitude with the script — the
        // dictionary-backed scripts against the rule-based ones — and entity space is *sorted by
        // access term*, which for a geographic corpus means sorted by country. So the slow scripts
        // are contiguous, not spread. At two chunks a thread over 7.4×10⁷ Overture names, 23 of
        // the 24 chunks finished within 25 s of each other and the twenty-fourth ran alone for a
        // further 90 s: the stage's wall time was one chunk's. Splitting finer costs a little more
        // spill (a term repeated in more runs) and buys most of that tail back, and it costs
        // no memory at all — the budget below is per *worker*, and only `threads` of them are ever
        // live, however many chunks there are. At eight the same column's chunk pass finished in
        // ~80 s against ~115 s, and the stage in 96.35 s against 148.00 s.
        //
        // ⊘ **The tail is smaller, not gone.** Four of the 96 chunks still ran ~60 s after the
        // other 92 had finished, so most of the chunk pass is still one region's segmentation.
        // Splitting finer again would need the fan-in raised with it, and is worth about a
        // further 30 s on this corpus — measured, not modelled, and not taken.
        let chunk_entities = entities
            .div_ceil((threads * 8).max(1))
            .max(TEXT_MIN_CHUNK_ENTITIES);
        TextIndexPlan {
            chunk_entities,
            worker_bytes,
            merge_slice_cap: TEXT_MERGE_SLICE_CAP,
        }
    }

    /// An explicit plan, for the tests that must force several chunks and several runs a chunk out
    /// of a corpus small enough to assert over.
    #[cfg(test)]
    fn explicit(
        chunk_entities: usize,
        worker_bytes: usize,
        merge_slice_cap: usize,
    ) -> TextIndexPlan {
        TextIndexPlan {
            chunk_entities: chunk_entities.max(1),
            worker_bytes: worker_bytes.max(1),
            merge_slice_cap,
        }
    }
}

/// One text column's entity-space index: the per-layer token dictionary and the postings over it.
///
/// **The dictionary's ordinals are positions in the sorted distinct term set**, exactly as a
/// keyword's are in its key set, and the postings are written in that same order — so posting *i*
/// belongs to the *i*-th key the dictionary holds. Both files are produced by one pass over one
/// sorted term stream, so an ordinal cannot drift between them: the ordinal a term gets is the one
/// [`tessera_filter::SortedDictWriter::push`] returns, and the record encoded against it is
/// appended to the postings spool before the next term is read.
///
/// # The shape: chunk, spill, merge
///
/// **The accumulator used to be one `HashMap` over the whole corpus, and that was a memory cost
/// with no ceiling.** Every distinct term in the corpus and every posting of every term were live
/// at once, then copied whole into a `Vec` for the sort before a byte was written — a function of
/// corpus size with nothing to bound it, in the same file whose category emit bands its own
/// transient to a constant the caller chooses. It was also entirely serial, over the most
/// expensive per-entity work the build does.
///
/// So the pass is now three steps:
///
/// 1. **Chunk.** Entity space is split into contiguous ascending ranges ([`TextIndexPlan`]) and the
///    ranges are indexed in parallel. Each worker accumulates its own `(term → entities)` map.
/// 2. **Spill.** A worker writes its map out as a **sorted run** ([`crate::spill::TextRunWriter`])
///    when its tracked footprint reaches the plan's per-worker budget, and again at the end of its
///    chunk. So a worker's residency is the budget whatever the documents are, and the run count
///    grows instead of the peak.
/// 3. **Cascade**, where there are more runs than one merge may hold file descriptors for
///    ([`TEXT_MERGE_FAN_IN`]). Groups of runs are merged into intermediate runs, in order, until
///    what is left fits in one merge. Nothing but a very large corpus reaches this.
/// 4. **Merge.** The runs are merged k-way on the term, and each merged term's entity lists are
///    concatenated in run order. That is what makes the entity lists ascending *for free*: chunks
///    partition entity space ascending, a worker's runs are emitted in the order it walked its
///    chunk, and the run list is held in that same order — so concatenation is already sorted and
///    no per-term sort exists anywhere in the pass.
///
/// **The output is a function of the corpus alone, never of the plan.** The dictionary is the
/// sorted distinct term set and a posting is the set of entities carrying its term; neither
/// depends on where a chunk boundary fell or how often a worker spilled.
/// [`tests::chunking_the_text_index_does_not_change_its_bytes`] is the assertion, over a corpus
/// whose entity count straddles the chunk sizes it is emitted under — a boundary that split a
/// term's postings between two runs and lost one half would otherwise be invisible.
///
/// # What each step costs, and what bounds it
///
/// * The chunk pass holds `threads × worker_bytes`, which is [`TEXT_BUDGET_SHARE`] of the build's
///   memory budget. Nothing in it scales with the corpus.
/// * The merge holds one open reader per run — a read buffer and the head *term*, never the head's
///   entities, which is why [`crate::spill::TextRunReader`] decodes a record's postings only when
///   they are asked for. The run count is capped by the cascade, so this is a constant too.
/// * One merged term's entity list is capped at [`TEXT_MERGE_SLICE_CAP`], above which it
///   accumulates into a Roaring bitmap and encodes through the byte-identical bitmap encoder.
///
/// # The pieces that did not change
///
/// **A term's first sighting is the only one that allocates.** The analyser hands back borrowed
/// tokens ([`tessera_analyse::Analyser::for_each_token`]) and the lookup is by `&[u8]`, so a term
/// already in the map costs no allocation.
///
/// **The postings stream through [`tessera_authz::postings::PostingsSpool`]** rather than being
/// collected, and the dictionary through [`tessera_filter::SortedDictWriter`]: neither file is
/// ever held whole in memory.
///
/// **A term repeated within one document contributes one posting entry.** The analyser keeps
/// duplicates and order because the positional payload upgrade (§4.5) needs both; a posting is a
/// set, so the duplicate collapses here rather than in the analyser. Entities reach a worker
/// ascending, so the duplicate is always the accumulator's last entry — the same `last()` test as
/// before, and it stays correct because a chunk is an ascending range and never a scattered set.
///
/// The singleton encoding is `tessera-authz`'s, unchanged: a term carried by few enough entities is
/// a bare `u32` array rather than a serialised bitmap, which is what the string-storage campaign
/// measured at 4.4× smaller on the singleton-heavy vocabularies real prose produces. Reusing that
/// format rather than minting a second one is the whole reason this crate already depends on it.
fn write_text_index(
    column_dir: &Path,
    attribute: &crate::config::Attribute,
    values: &EntityColumn,
    plan: TextIndexPlan,
) -> Result<WrittenTextIndex> {
    // The identity was resolved at the schema parse; the name is its first component. Resolving it
    // again here rather than threading an `Analyser` down keeps the build's contract with the
    // manifest one-directional: what is recorded is what indexed.
    let identity = attribute.analyser.as_deref().ok_or_else(|| {
        BuildError::Invalid(format!(
            "attribute '{}' is text but carries no resolved analyser — the schema parse is what \
             resolves one, so this is a compilation defect rather than a schema error",
            attribute.name
        ))
    })?;
    // **The whole identity, not the name.** The flush and the read path both compare the full
    // `<name>/<version>` before they will use a pipeline, and this was the one of the three writers
    // that compared only the first component — so a `Schema` built programmatically rather than
    // parsed from TOML could carry `unicode/icu4x-1.0/p1`, index happily under today's segmenter,
    // and record the stale string. The bundle would then refuse to open for every reader, for ever,
    // with the defect a build behind it. Unreachable through `Schema::parse`, which resolves the
    // identity from this binary's own analyser; the SDK and the tests are not obliged to.
    let name = identity.split('/').next().unwrap_or_default();
    let analyser = tessera_analyse::analyser(name)
        .filter(|a| a.identity() == identity)
        .ok_or_else(|| {
            BuildError::Invalid(format!(
                "attribute '{}' declares analyser '{identity}', which this build does not carry.                  Its terms cannot be reproduced, so an index written now would answer every                  `match` from a segmentation the manifest does not describe",
                attribute.name
            ))
        })?;

    // ---- 1. the chunk pass, in parallel, spilling sorted runs -------------------------------
    let chunks: Vec<(usize, usize)> = (0..values.len())
        .step_by(plan.chunk_entities)
        .map(|lo| (lo, (lo + plan.chunk_entities).min(values.len())))
        .collect();
    // **One analyser, shared.** Its construction deserialises the segmenter's dictionary data —
    // the cost the type exists to amortise — and it holds no per-document state, so it is `Sync`
    // and the workers borrow it. What each worker does hold of its own is the normalisation
    // scratch, which is per document by nature.
    let receipts: Vec<Vec<spill::SpillReceipt>> = chunks
        .par_iter()
        .enumerate()
        .map(|(chunk, &(lo, hi))| {
            index_text_chunk(
                column_dir,
                chunk,
                lo,
                hi,
                values,
                attribute,
                &analyser,
                plan.worker_bytes,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    // Flattened in chunk order, and each chunk's own runs in the order it wrote them: the merge
    // concatenates a term's entity lists in exactly this order, and it is ascending in entity only
    // because this order is.
    let receipts: Vec<spill::SpillReceipt> = receipts.into_iter().flatten().collect();

    // ---- 2. the cascade, where a corpus produced more runs than one merge may hold open ------
    let receipts = cascade_text_runs(column_dir, receipts)?;

    // ---- 3. the merge: one sorted term stream into both files -------------------------------
    let dict_path = column_dir.join(tessera_filter::DICT_FILE);
    let postings_path = column_dir.join("postings.arrow");
    // Beside the file it assembles, and removed by `finish` — the spool-then-assemble discipline
    // this repo applies to every file whose records are sized as they are written. A build that
    // fails here leaves the spool behind with the rest of the half-written partition.
    let spool_path = postings_path.with_extension("spool");
    let terms = merge_text_runs(
        &dict_path,
        &postings_path,
        &spool_path,
        &receipts,
        plan.merge_slice_cap,
    )?;
    fsync_file(&dict_path)?;
    fsync_file(&postings_path)?;
    for receipt in &receipts {
        std::fs::remove_file(&receipt.path).map_err(|e| BuildError::io(&receipt.path, e))?;
    }

    Ok(WrittenTextIndex {
        paths: vec![dict_path, postings_path],
        terms,
    })
}

/// Index one contiguous entity range, spilling one or more sorted runs.
///
/// The worker's whole residency is `terms`, and it is what the byte budget bounds: the tracked
/// figure is an estimate ([`TEXT_TERM_ENTRY_BYTES`]) rather than an allocator reading, so it is
/// deliberately generous. A run is spilled the moment the estimate reaches the budget — mid
/// document is not possible, because the check sits between documents, so the true overshoot is
/// one document's terms.
#[allow(clippy::too_many_arguments)]
fn index_text_chunk(
    column_dir: &Path,
    chunk: usize,
    lo: usize,
    hi: usize,
    values: &EntityColumn,
    attribute: &crate::config::Attribute,
    analyser: &tessera_analyse::Analyser,
    worker_bytes: usize,
) -> Result<Vec<spill::SpillReceipt>> {
    let mut receipts = Vec::new();
    let mut terms: std::collections::HashMap<Box<[u8]>, Vec<u32>> =
        std::collections::HashMap::new();
    let mut bytes = 0usize;
    let mut seq = 0usize;
    // The analyser's normalisation buffer, held across the whole chunk rather than per document.
    let mut scratch = tessera_analyse::TokenScratch::default();
    // **Absent runs are skipped a word at a time**, not an entity at a time: presence is a bit
    // vector, one column of this corpus is 84.5% absent, and the alternative is a call and a shift
    // per entity to learn nothing.
    for entity in values.present_entities_in(lo, hi) {
        // Absence is `Null`, and the empty string is a value a corpus may hold — the same
        // out-of-band rule the string families share. Neither yields a term.
        //
        // Borrowed: this walks every string in the corpus, and a clone per entity would be a
        // second copy of the column for the duration of the tokenise.
        let Some(prose) = values.str_at(entity) else {
            return Err(BuildError::Invalid(format!(
                "attribute '{}': a text column's value must be a string, got {:?}",
                attribute.name,
                values.value_at(entity)
            )));
        };
        let entity = entity as u32;
        analyser.for_each_token(prose, &mut scratch, &mut |token| {
            // Looked up before it is owned: a term already seen costs no allocation, which over a
            // corpus is every occurrence but the first of every word.
            if let Some(postings) = terms.get_mut(token.as_bytes()) {
                // Entities arrive ascending, so the duplicate a repeated term produces is always
                // the last entry — no sort and no set needed to collapse it.
                if postings.last() != Some(&entity) {
                    postings.push(entity);
                    bytes += TEXT_POSTING_BYTES;
                }
            } else {
                terms.insert(token.as_bytes().into(), vec![entity]);
                bytes += TEXT_TERM_ENTRY_BYTES + 2 * token.len() + TEXT_POSTING_BYTES;
            }
        });
        if bytes >= worker_bytes {
            spill_text_run(column_dir, chunk, &mut seq, &mut terms, &mut receipts)?;
            bytes = 0;
        }
    }
    spill_text_run(column_dir, chunk, &mut seq, &mut terms, &mut receipts)?;
    Ok(receipts)
}

/// Sort a worker's accumulator by term and write it out as one run, leaving the accumulator empty.
///
/// The sort is over **borrowed keys**: a `Vec<(Box<[u8]>, Vec<u32>)>` of the map's contents would
/// be the same second copy of every term the whole-corpus accumulator paid at its one sort, only
/// per run. Replacing the map rather than clearing it is what makes the byte budget mean
/// something — a cleared table keeps its capacity, so the next fill would count from zero against
/// memory that was never released.
fn spill_text_run(
    column_dir: &Path,
    chunk: usize,
    seq: &mut usize,
    terms: &mut std::collections::HashMap<Box<[u8]>, Vec<u32>>,
    receipts: &mut Vec<spill::SpillReceipt>,
) -> Result<()> {
    if terms.is_empty() {
        return Ok(());
    }
    let path = column_dir.join(format!("text-run-{chunk:05}-{seq:04}.spill"));
    let mut writer = spill::TextRunWriter::create(&path)?;
    {
        let mut order: Vec<&[u8]> = terms.keys().map(|key| &**key).collect();
        order.sort_unstable();
        for term in order {
            writer.push(term, &terms[term])?;
        }
    }
    receipts.push(writer.finish()?);
    *terms = std::collections::HashMap::new();
    *seq += 1;
    Ok(())
}

/// A k-way merge over open run cursors, yielding each distinct term once with its entities in
/// ascending order.
///
/// The heap holds **run indices**, and the comparison reaches into the readers — so a term is
/// never copied into the heap and the merge allocates nothing per record. Ties break by run index,
/// which is what puts the runs carrying one term in ascending entity order: the run list is held
/// in chunk order, chunks partition entity space ascending, and a worker's own runs are in the
/// order it walked its chunk.
struct TextRunMerge {
    cursors: Vec<spill::TextRunReader>,
    heap: Vec<usize>,
    /// The selected term. Held as bytes because that is what the cursors compare on, and because
    /// the caller needs it after the runs carrying it have left the heap.
    term: Vec<u8>,
    /// The runs whose head is the selected term, ascending — drained in this order.
    selected: Vec<usize>,
    /// Entities the selected term carries across all of them.
    postings: u64,
}

impl TextRunMerge {
    fn open(receipts: &[spill::SpillReceipt]) -> Result<TextRunMerge> {
        let mut cursors: Vec<spill::TextRunReader> = Vec::with_capacity(receipts.len());
        let mut heap: Vec<usize> = Vec::with_capacity(receipts.len());
        for receipt in receipts {
            let mut reader = spill::TextRunReader::open(receipt)?;
            // A run with no terms at all — an empty chunk — verifies its receipt here and takes no
            // place in the heap.
            if reader.advance()? {
                heap.push(cursors.len());
            }
            cursors.push(reader);
        }
        for root in (0..heap.len() / 2).rev() {
            text_heap_sift_down(&mut heap, &cursors, root);
        }
        Ok(TextRunMerge {
            cursors,
            heap,
            term: Vec::new(),
            selected: Vec::new(),
            postings: 0,
        })
    }

    /// Select the next distinct term, or `false` when every run is exhausted. The previous term's
    /// runs are advanced here rather than by the caller, so a caller that took fewer entities than
    /// the term carried still leaves the stream where the next record starts.
    fn next_term(&mut self) -> Result<bool> {
        for &run in &self.selected {
            if self.cursors[run].advance()? {
                text_heap_push(&mut self.heap, &self.cursors, run);
            }
        }
        self.selected.clear();
        self.postings = 0;
        let Some(&first) = self.heap.first() else {
            return Ok(false);
        };
        self.term.clear();
        self.term.extend_from_slice(self.cursors[first].term());
        while self
            .heap
            .first()
            .is_some_and(|&top| self.cursors[top].term() == self.term.as_slice())
        {
            let top = text_heap_pop(&mut self.heap, &self.cursors);
            self.postings += self.cursors[top].pending() as u64;
            self.selected.push(top);
        }
        Ok(true)
    }

    fn term(&self) -> &[u8] {
        &self.term
    }

    /// How many entities the selected term carries. Known before a single one is decoded, which is
    /// what lets the caller choose an encoder without buffering to find out.
    fn postings(&self) -> u64 {
        self.postings
    }

    /// Feed the selected term's entities to `sink`, ascending, checking the ascent as it goes.
    ///
    /// The check is over the *merged* list, not each run's: a run's own ascent is the writer's
    /// business, and what could go wrong here is the run ordering. `encode_posting` re-checks the
    /// slice path, but the bitmap path has no such check to make — a `Bitmap` is a set — which is
    /// why the test lives here rather than there.
    fn drain(&mut self, sink: &mut impl FnMut(u32) -> Result<()>) -> Result<()> {
        let mut last: Option<u32> = None;
        for &run in &self.selected {
            self.cursors[run].take_entities(&mut |entity| {
                if let Some(previous) = last {
                    if entity <= previous {
                        return Err(BuildError::Invalid(format!(
                            "text run merge: entity {entity} does not ascend past {previous} — \
                             the runs were not merged in entity order"
                        )));
                    }
                }
                last = Some(entity);
                sink(entity)
            })?;
        }
        Ok(())
    }
}

/// Runs opened at once by one merge.
///
/// **A merge holds a file descriptor per run, and the run count is a function of the corpus.** A
/// worker spills whenever its budget fills, so a corpus far larger than memory produces far more
/// runs than a process may hold open — which would be `EMFILE` at hour two on exactly the corpus
/// this whole shape exists to make buildable. Above the cap the runs are merged in passes: groups
/// of [`TEXT_MERGE_FAN_IN`] into one intermediate run each, until what is left fits in one merge.
/// An intermediate run is written by the same writer as a worker's, so the cascade adds a pass and
/// not a format.
///
/// A hundred and twenty-eight, which is comfortably under the 1,024 soft limit a Linux process
/// ordinarily starts with and above the run count an ordinary corpus produces — 7.4×10⁷ Overture
/// names at eight chunks a thread on twelve cores is 96 runs, one merge and no cascade. The passes
/// are sequential: the cascade is I/O and the final merge is sequential anyway, so running the
/// groups in parallel would buy a fraction of a rare path at the cost of multiplying the very
/// descriptor count the cap exists to hold down — `threads × fan-in` is not a number this can
/// bound on a machine whose core count it does not know.
const TEXT_MERGE_FAN_IN: usize = 128;

/// Reduce `receipts` to at most [`TEXT_MERGE_FAN_IN`] runs, deleting each pass's inputs as it goes.
///
/// Groups are taken in order and each group merges in order, so the entity ordering the final
/// merge relies on survives every pass.
fn cascade_text_runs(
    column_dir: &Path,
    mut receipts: Vec<spill::SpillReceipt>,
) -> Result<Vec<spill::SpillReceipt>> {
    let mut pass = 0usize;
    while receipts.len() > TEXT_MERGE_FAN_IN {
        let mut merged = Vec::with_capacity(receipts.len().div_ceil(TEXT_MERGE_FAN_IN));
        for (group, runs) in receipts.chunks(TEXT_MERGE_FAN_IN).enumerate() {
            let path = column_dir.join(format!("text-merge-{pass:02}-{group:05}.spill"));
            let mut merge = TextRunMerge::open(runs)?;
            let mut writer = spill::TextRunWriter::create(&path)?;
            while merge.next_term()? {
                let count = u32::try_from(merge.postings()).map_err(|_| {
                    BuildError::Invalid(format!(
                        "text run merge: term {:?} carries more entities than a u32 can count",
                        String::from_utf8_lossy(merge.term())
                    ))
                })?;
                writer.begin(merge.term(), count)?;
                merge.drain(&mut |entity| writer.push_entity(entity))?;
            }
            merged.push(writer.finish()?);
            // Deleted per group rather than per pass: a corpus that reaches the cascade at all is
            // one whose runs are large, and holding a whole pass's inputs beside a whole pass's
            // outputs would double the spill's peak on disk for no reason.
            for receipt in runs {
                std::fs::remove_file(&receipt.path)
                    .map_err(|e| BuildError::io(&receipt.path, e))?;
            }
        }
        receipts = merged;
        pass += 1;
    }
    Ok(receipts)
}

/// Merge the sorted runs into the dictionary and the postings, and return the term count.
fn merge_text_runs(
    dict_path: &Path,
    postings_path: &Path,
    spool_path: &Path,
    receipts: &[spill::SpillReceipt],
    merge_slice_cap: usize,
) -> Result<u64> {
    let mut merge = TextRunMerge::open(receipts)?;

    let dict_file = std::fs::File::create(dict_path).map_err(|e| BuildError::io(dict_path, e))?;
    let mut dict = tessera_filter::SortedDictWriter::new(std::io::BufWriter::new(dict_file))
        .map_err(|e| BuildError::io(dict_path, std::io::Error::from(e)))?;
    let mut spool = tessera_authz::postings::PostingsSpool::create(spool_path)
        .map_err(|e| BuildError::io(spool_path, e))?;

    let mut entities: Vec<u32> = Vec::new();
    let mut staged: Vec<u32> = Vec::new();
    let mut written = 0u64;

    while merge.next_term()? {
        let term = std::str::from_utf8(merge.term()).map_err(|e| {
            BuildError::Invalid(format!(
                "text run merge: a term is not UTF-8 ({e}) — the analyser emits `&str`, so this \
                 is a corrupted run rather than a corpus value"
            ))
        })?;
        let ordinal = dict
            .push(term)
            .map_err(|e| BuildError::io(dict_path, std::io::Error::from(e)))?;

        let record = if merge.postings() <= merge_slice_cap as u64 {
            entities.clear();
            merge.drain(&mut |entity| {
                entities.push(entity);
                Ok(())
            })?;
            tessera_authz::postings::encode_posting(
                ordinal as usize,
                &entities,
                SMALL_TERM_THRESHOLD_DEFAULT,
            )
            .map_err(|e| BuildError::io(postings_path, e))?
        } else {
            // The cap's other side: a term this large is one record, and holding it as `u32`s
            // would be the only term in the pass whose residency the plan does not bound.
            let mut bitmap = croaring::Bitmap::new();
            staged.clear();
            merge.drain(&mut |entity| {
                staged.push(entity);
                if staged.len() >= TEXT_MERGE_STAGE_ENTITIES {
                    bitmap.add_many(&staged);
                    staged.clear();
                }
                Ok(())
            })?;
            if !staged.is_empty() {
                bitmap.add_many(&staged);
                staged.clear();
            }
            tessera_authz::postings::encode_posting_bitmap(&bitmap, SMALL_TERM_THRESHOLD_DEFAULT)
                .map_err(|e| BuildError::io(postings_path, e))?
        };
        spool
            .append(&record)
            .map_err(|e| BuildError::io(spool_path, e))?;
        written += 1;
    }

    dict.finish()
        .map_err(|e| BuildError::io(dict_path, std::io::Error::from(e)))?;
    spool
        .finish(postings_path)
        .map_err(|e| BuildError::io(postings_path, e))?;
    Ok(written)
}

/// Entities staged before each hand-off to croaring in the merge's bitmap arm — the same batching
/// `tessera_roaring` uses, and for the same reason: `add_many` amortises over a run of values.
const TEXT_MERGE_STAGE_ENTITIES: usize = 1 << 16;

/// Order two runs by their head term, ties broken by run index so that equal terms leave the heap
/// in ascending entity order.
fn text_run_before(cursors: &[spill::TextRunReader], a: usize, b: usize) -> bool {
    match cursors[a].term().cmp(cursors[b].term()) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => a < b,
    }
}

fn text_heap_sift_down(heap: &mut [usize], cursors: &[spill::TextRunReader], mut node: usize) {
    loop {
        let left = node * 2 + 1;
        if left >= heap.len() {
            return;
        }
        let right = left + 1;
        let child = if right < heap.len() && text_run_before(cursors, heap[right], heap[left]) {
            right
        } else {
            left
        };
        if !text_run_before(cursors, heap[child], heap[node]) {
            return;
        }
        heap.swap(node, child);
        node = child;
    }
}

fn text_heap_push(heap: &mut Vec<usize>, cursors: &[spill::TextRunReader], run: usize) {
    heap.push(run);
    let mut node = heap.len() - 1;
    while node > 0 {
        let parent = (node - 1) / 2;
        if !text_run_before(cursors, heap[node], heap[parent]) {
            return;
        }
        heap.swap(node, parent);
        node = parent;
    }
}

fn text_heap_pop(heap: &mut Vec<usize>, cursors: &[spill::TextRunReader]) -> usize {
    let top = heap[0];
    let last = heap.pop().expect("the heap is not empty");
    if !heap.is_empty() {
        heap[0] = last;
        text_heap_sift_down(heap, cursors, 0);
    }
    top
}

/// One text column's index: the files written, and the term count [`BuildStage::TextIndex`]
/// reports.
struct WrittenTextIndex {
    paths: Vec<PathBuf>,
    terms: u64,
}

/// Does this column owe a postings file?
///
/// Two independent reasons, and the second is the one a reader will not expect.
///
/// **`index = true`** is the obvious one: the column is declared filterable, and postings are
/// how a broad-coverage filter stays inside its latency budget (filter-index §2.3).
///
/// **`visibility = "derived"`** is the other, and it is *not* optional. That control gates the
/// existence of a value name, and the gate is membership-derived: a value is offered only if the
/// principal can see an item carrying it (per-point-attributes §3.3). Deriving that needs the
/// per-`(column, code)` member sets, which are exactly these postings. Without them `/v1/categories`
/// would have to derive membership by scanning the value column per request — which is inside a
/// *filter's* latency budget but not inside this endpoint's, and would make contracts §3.2's
/// compute-admission justification ("no mask composition, no projection, no file IO") false.
///
/// So a `derived` category gets postings whatever its `index` says. This is the one place the
/// postings stop being an optional accelerator: everywhere else a deployment that builds them and one
/// that does not answer identically and differ only in latency, but here a disclosure control depends
/// on them existing.
pub(crate) fn postings_are_owed(
    schema: &crate::config::Schema,
    attribute: &crate::config::Attribute,
) -> bool {
    if attribute.index {
        return true;
    }
    attribute
        .vocabulary
        .as_ref()
        .and_then(|name| schema.vocabularies.get(name))
        .is_some_and(|v| v.visibility == crate::config::Visibility::Derived)
}

/// The vocabulary code a category column's value carries.
///
/// Reached only for a category — postings are derived for the vocabulary-bearing family alone —
/// so the three unsigned widths §3.6 allows are the whole domain; anything else reaching here is
/// a schema-compilation defect, and it fails loudly rather than filtering on a value it invented.
fn category_code(value: &ScalarValue, column: &str) -> Result<u32> {
    match value {
        ScalarValue::U8(c) => Ok(*c as u32),
        ScalarValue::U16(c) => Ok(*c as u32),
        ScalarValue::U32(c) => Ok(*c),
        other => Err(BuildError::Invalid(format!(
            "attribute '{column}' is declared for filtering but carries {other:?}, which is not a \
             category code"
        ))),
    }
}

/// Permute the entity-major columns into **row order**, ready for `write_columns`, and record
/// which rows carry a value.
///
/// `entity_row[r]` is the entity whose values row `r` carries — the same permutation
/// `residual_row` and `tessera_row` are built through, applied to the same arrays, so a row's
/// geometry, identity and attributes cannot come from different items.
fn permute_attribute_tail(
    schema: &crate::config::Schema,
    by_entity: &[EntityColumn],
    entity_row: &[u32],
    scratch: &crate::column::ColumnScratch,
) -> Result<AttributeTail> {
    // **One lane per render column.** The columns are independent all the way down — each is
    // permuted from its own entity-order column into its own mapped file, and neither the gather
    // nor the presence sweep touches anything another lane can name — so the loop over them is the
    // split, exactly as it is in the attribute join. `collect` over an indexed parallel iterator
    // preserves declared order, which the tail's column order is.
    //
    // **Borrowed, not consumed**: the values are entity space and every view's row space is a
    // permutation of the same columns (`views.md` §1), so pass two calls this once per view.
    let lanes: Vec<Result<Option<Lane>>> = schema
        .attributes
        .par_iter()
        .zip(by_entity.par_iter())
        .map(|(attribute, values)| {
            // **The tail is exactly the render columns.** An `index`-only column is entity-space
            // and has already been written there; including it here would give it a slot in every
            // row as well, which is the per-row cost §10.3's routing exists to avoid and — for a
            // `utf8` column — the one `render` on `utf8` is refused for outright.
            if !attribute.render {
                return Ok(None);
            }
            let mut column = EntityColumn::filled(scratch, attribute.ty, entity_row.len())?;
            for (row, &entity) in entity_row.iter().enumerate() {
                column.set(row, values.value_at(entity as usize), &attribute.name)?;
            }
            // **The absent slot is left as the mapping's zero, which *is* the render
            // placeholder.** The column is non-nullable on the wire (contracts R4), so an absent
            // value has to be written as something; `ScalarValue::or_render_placeholder` gives the
            // type's zero for every renderable type, and a fresh mapping reads as zeros. Writing
            // the placeholder explicitly would store the same bytes and lose the presence bit that
            // says the zero means nothing — which is the bitmap below.
            // `the_render_placeholder_is_the_zero_a_mapping_reads_as` holds the two together.
            let presence =
                render_presence_of((0..entity_row.len()).map(|row| column.is_present(row)));
            Ok(Some(Lane {
                name: attribute.name.clone(),
                presence,
                values: column.into_values(scratch, &attribute.name)?,
            }))
        })
        .collect();
    let mut presence = Vec::new();
    let mut out = Vec::with_capacity(schema.attributes.len());
    for lane in lanes {
        let Some(lane) = lane? else { continue };
        if let Some(rows) = lane.presence {
            presence.push((lane.name.clone(), rows));
        }
        out.push((lane.name, lane.values));
    }
    Ok(AttributeTail {
        columns: out,
        presence,
    })
}

/// One render column, built by the lane that owns it.
struct Lane {
    name: String,
    /// `None` where every row carries a value — the case that writes no file (decision 0064).
    presence: Option<Bitmap>,
    values: ScalarColumn,
}

/// A segment's attribute tail: the columns `write_columns` takes, and the presence bitmaps that go
/// beside them (decision 0064) — one per render column that has an absence, in row order.
struct AttributeTail {
    columns: Vec<(String, ScalarColumn)>,
    presence: Vec<(String, Bitmap)>,
}

/// Which rows of one render column carry a value, from that column's values **in row order** —
/// `None` where every row does, which is the case that writes no file (decision 0064).
///
/// **`ScalarValue::Null` names exactly the columns that owe a bitmap, so this needs no schema.**
/// The two families with an in-band way to say "nothing" never produce one here: a category's
/// missing key resolves to the reserved code 0 its vocabulary keeps out of the value space, at the
/// points file (`BatchColumn::value`) and at the ingest plane alike, and `render` on `utf8` is
/// refused at schema parse. What is left is the numeric family, every bit pattern of which is a
/// legal value.
///
/// Shared by the streaming and linear builds because they hold their values in different shapes
/// but must write the same bytes — the property `tests/build_equivalence.rs` exists to hold them
/// to.
/// Takes presence per row rather than the values themselves: the entity-major columns are typed
/// now (see [`EntityColumn`]), so absence is a bit beside the value and never a variant of it.
pub(crate) fn render_presence_of(
    present_per_row: impl IntoIterator<Item = bool>,
) -> Option<Bitmap> {
    let mut present = Bitmap::new();
    let mut any_absent = false;
    for (row, is_present) in present_per_row.into_iter().enumerate() {
        if is_present {
            present.add(row as u32);
        } else {
            any_absent = true;
        }
    }
    any_absent.then_some(present)
}

/// The selected source ids, in scan order (which is **no particular order** — the decode is
/// parallel; every consumer sorts), in an exactly-sized allocation.
///
/// Counted first and then read: letting a `Vec` double its way to 8 GB would peak at three times
/// the final size during the last reallocation, which is precisely the kind of transient this
/// build exists to avoid.
fn read_source_ids(args: &BuildArgs, view: &crate::ViewArgs) -> Result<Vec<u64>> {
    let count = match args.limit {
        // No limit and no selection ⇒ every row is selected ⇒ the metadata row count is exact and
        // the counting decode is a whole pass over the file for nothing. A form B source's rows
        // are several views', so the count there is data-dependent like a limit's.
        None if view.select.is_none() => input::count_point_rows(&view.points)? as usize,
        _ => {
            let mut count = 0usize;
            input::scan_points(
                &view.points,
                &view.point_fields,
                view.projection,
                &view.extent,
                args.limit,
                view.select.as_ref(),
                |_| {
                    count += 1;
                    ControlFlow::Continue(())
                },
            )?;
            count
        }
    };
    let mut ids = Vec::with_capacity(count);
    input::scan_points(
        &view.points,
        &view.point_fields,
        view.projection,
        &view.extent,
        args.limit,
        view.select.as_ref(),
        |point| {
            ids.push(point.source_id);
            ControlFlow::Continue(())
        },
    )?;
    Ok(ids)
}

/// One view's geometry in **ordinal** space, read once in pass one and permuted into entity
/// space in pass two (`views.md` §7).
struct ViewGeometry {
    x: spill::MappedU32,
    y: spill::MappedU32,
    /// Which ordinals this view holds a row for — the view's population, and what makes its
    /// permutation sentinel wherever it does not.
    present: Vec<u64>,
    rows: u64,
}

/// What one view's points file said about itself, for the later passes over it to be checked
/// against ([`read_source_ids_union`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ViewIdAnchor {
    rows: u64,
    /// An order-independent mixed sum of the ids, so a file swapped mid-build is loud rather
    /// than silently repairing its own row count.
    mixed: u64,
}

/// **Pass one's entity space** (`views.md` §7): every view's point source, unioned by
/// `external_id`.
///
/// A row is unique per `(external_id, view)` — an id repeated *within* one view's file is the
/// old duplicate refusal, unchanged, while the same id in two views is the ordinary case and is
/// what makes an entity's identity, label and attributes shared across the row spaces.
fn read_source_ids_union(args: &BuildArgs) -> Result<(Vec<u64>, Vec<ViewIdAnchor>)> {
    let mut union: Vec<u64> = Vec::new();
    let mut anchors = Vec::with_capacity(args.views.len());
    for view in &args.views {
        let mut ids = read_source_ids(args, view)?;
        anchors.push(ViewIdAnchor {
            rows: ids.len() as u64,
            mixed: ids
                .iter()
                .fold(0u64, |acc, &id| acc.wrapping_add(mix64(id))),
        });
        ids.par_sort_unstable();
        if ids.windows(2).any(|w| w[0] == w[1]) {
            return Err(BuildError::Invalid(format!(
                "view '{}': {} contains duplicate entity_id values. A row is unique per (entity, \
                 view) — the same entity in several views is the ordinary case and is several \
                 files, never several rows of one (views §4)",
                view.view_id,
                view.points.display()
            )));
        }
        union.extend_from_slice(&ids);
    }
    union.par_sort_unstable();
    union.dedup();
    Ok((union, anchors))
}

/// Refuse to run unless the configured plugin labels items the way this pipeline assumes.
///
/// The dictionary pass derives each term's descriptor from the term id alone, which is only
/// sound when the plugin's label rule is decomposable — when `terms_of_labels` over a term list
/// yields exactly one descriptor per element, in order. `builtin:passthrough` (R6) is defined
/// that way; nothing in the plugin ABI requires it, and a plugin that derived descriptors from
/// the item's terms as a whole (a rule engine, a normaliser, anything that folds terms together)
/// would be silently mislabelled here — every posting would name the wrong term, which is a
/// disclosure, not a bug in a performance path.
///
/// So this is checked twice over, and fails closed: the plugin must *be* passthrough by its
/// declared `data_plugin_hash`, and it must *behave* decomposably on a probe term list. The hash
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
    let probe: Vec<Vec<u8>> = vec![b"11".to_vec(), b"7".to_vec(), b"4096".to_vec()];
    let descriptors = plugin.terms_of_labels(&probe)?;
    if descriptors != probe {
        return Err(BuildError::Invalid(format!(
            "the plugin's label rule is not decomposable: the term list {:?} yielded {:?}, not \
             one descriptor per term, in order",
            probe
                .iter()
                .map(|d| String::from_utf8_lossy(d).into_owned())
                .collect::<Vec<_>>(),
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
    /// Source term ids, ascending — with `term_ids` in parallel, the source-term → term-id map
    /// as two flat arrays (12 bytes per term against an `FxHashMap`'s ~40 at T = 117M), looked
    /// up by merge sweep over term-sorted chunks, never by per-row probe.
    term_keys: Vec<u64>,
    /// `term_ids[i]` is the term id `term_keys[i]` interned to.
    term_ids: Vec<u32>,
    /// Per term id, how many pairs **rows** name it — counted before deduplication, so a bucket
    /// sized by this is wide enough even when the input repeats a `(entity, term)` row.
    row_counts: Vec<u64>,
    term_count: u64,
    /// Selected pairs rows, before deduplication.
    pair_rows: usize,
    /// Resolved pairs rows per ordinal range (`ordinal >> histogram_shift`), for the batch
    /// pre-flight: the worst batch's pairs are bounded by summing overlapping ranges.
    histogram: Vec<u64>,
    histogram_shift: u32,
    dict_paths: Vec<PathBuf>,
}

/// Assign term ids and write the dictionary extent.
///
/// The linear build interns descriptors while walking items in ascending source-id order, each
/// item's terms in ascending source-term order; a term's id is its rank in that stream of *first*
/// appearances. That rank is `(smallest ordinal carrying the term, source term id)` — computable
/// from one pass over the pairs relation, over a map with one entry per distinct term (~48k in
/// the probe corpus), rather than from a materialised item list.
///
/// Returns [`Dictionary`].
fn build_dictionary(
    args: &BuildArgs,
    access: &crate::AccessPlan,
    source_ids: &[u64],
    dict_dir: &std::path::Path,
) -> Result<Dictionary> {
    // Chunk-order insensitivity ([`join_chunk`]): per-term min-ordinal and row counts, and the
    // per-range histogram, are commutative aggregations — no arrival order is observable.
    let mut first_ordinal: FxHashMap<u64, (u64, u64)> = FxHashMap::default();
    // Ordinal-range histogram for batch sizing: 2^16 ranges regardless of n.
    let histogram_shift =
        (64 - (source_ids.len().max(1) as u64).leading_zeros()).saturating_sub(16);
    let mut histogram = vec![0u64; (source_ids.len() >> histogram_shift) + 1];
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
                    histogram[(ordinal >> histogram_shift) as usize] += 1;
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
    let fill = crate::scan_access(args, access, |_view, source_id, source_term| {
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
    crate::report_access_fill(args, fill);
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

    // A source term's descriptor is what `builtin:passthrough` yields for it (R6): the decimal
    // for the exploded relation's integer ids, the term itself for a field-sourced view.
    // Streamed, not interned: the descriptors here are distinct by construction (one per
    // distinct source term) and arrive in term-id order, which is `DictStreamWriter`'s exact
    // contract — at T = 117M an interner is gigabytes of pointless ownership.
    let mut dict = tessera_authz::DictStreamWriter::new(dict_dir);
    // **`public` is appended first, so it is term 0 in every bundle** and is minted for no other
    // descriptor — the streaming half of what `build_in_memory` does by interning it first. A
    // source term spelling `public` therefore maps to 0 rather than appending a second record,
    // which would put one descriptor in the dictionary twice.
    let public = dict.append(tessera_authz::PUBLIC_LABEL);
    debug_assert_eq!(public, tessera_authz::PUBLIC_TERM);
    let mut pairs_of_term: Vec<(u64, u32)> = Vec::with_capacity(order.len());
    let mut row_counts: Vec<u64> = vec![0; 1];
    for &(_, source_term) in &order {
        let descriptor = access.descriptors.descriptor(source_term);
        let rows = first_ordinal[&source_term].1;
        if descriptor.as_bytes() == tessera_authz::PUBLIC_LABEL {
            pairs_of_term.push((source_term, public.raw()));
            row_counts[public.raw() as usize] = rows;
            continue;
        }
        let term = dict.append(descriptor.as_bytes());
        pairs_of_term.push((source_term, term.raw()));
        row_counts.push(rows);
    }
    drop(first_ordinal);
    drop(order);
    let term_count = dict.len() as u64;
    let dict_paths = dict.finish().map_err(|e| BuildError::io(dict_dir, e))?;
    for path in &dict_paths {
        fsync_file(path)?;
    }
    // The lookup arrays: source-term-ascending, consumed by merge sweeps over term-sorted
    // chunks. (Unique keys — one entry per distinct term — so the parallel unstable sort has
    // one output.)
    pairs_of_term.par_sort_unstable_by_key(|&(source_term, _)| source_term);
    let term_keys: Vec<u64> = pairs_of_term.iter().map(|&(st, _)| st).collect();
    let term_ids: Vec<u32> = pairs_of_term.iter().map(|&(_, tid)| tid).collect();
    Ok(Dictionary {
        term_keys,
        term_ids,
        row_counts,
        term_count,
        pair_rows,
        histogram,
        histogram_shift,
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
///   refined order is: all shorts first, in the `(morton, ordinal)` order the pre-sort already
///   established; then the longs, ordered by their signature *tails* (terms from index 2 on) with
///   `(morton, ordinal)` breaking exact-tail ties. That is precisely the reference build's
///   `(signature, morton, source_id)` order (decision 0073), with the tiebreak the current stable
///   sort left implicit made explicit.
///
///   **The shorts get the Morton tiebreak for free and the longs must be given it**, because the
///   pre-sort key stops at two terms: within a tie group the shorts are already fully ordered by
///   the key the parallel sort ran on, and the longs are re-sorted here from their tails up.
///
/// Mechanically, each long member's tail is gathered **once** into a scratch arena — its
/// location in `packed` comes from the batch's per-ordinal starts array in O(1), not from a
/// binary search — and the
/// long sort compares contiguous scratch, not the multi-gigabyte relation. The previous
/// implementation did two `partition_point` probes over `packed` *per comparison*; with 46% of
/// the probe corpus inside refined groups that was the single largest cost of the whole
/// build (measured: 52.5% of the 1e8 build, superlinear).
///
/// Groups are disjoint slices of `recs`, so refinement runs in parallel across groups; each
/// group's result is deterministic, so the whole pass is.
fn refine_signature_ties(
    recs: &mut [SortRec],
    packed: &[u64],
    starts: &[u32],
    base_ordinal: u64,
    long_sig: &[u64],
) {
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
        if end - start > 1
            && recs[start..end]
                .iter()
                .any(|r| bit_get(long_sig, (r.ordinal as u64 - base_ordinal) as usize))
        {
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
            refine_group(group, packed, starts, base_ordinal, long_sig, scratch)
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
    /// probe corpus's shape stays in the tens of megabytes; a corpus pathological enough to matter
    /// here would already be pathological for `flat` (4P, resident in the same build).
    arena: Vec<u32>,
}

/// Refine one tie group: shorts keep their order at the front, longs sort by (tail, ordinal).
/// See [`refine_signature_ties`] for why this equals the full-signature stable sort.
fn refine_group(
    group: &mut [SortRec],
    packed: &[u64],
    starts: &[u32],
    base_ordinal: u64,
    long_sig: &[u64],
    s: &mut RefineScratch,
) {
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
        let local = (rec.ordinal as u64 - base_ordinal) as usize;
        if bit_get(long_sig, local) {
            let tail_start = s.arena.len();
            // Skip the two prefix terms every member shares; the batch's per-ordinal starts
            // array bounds the slice in O(1).
            for &value in &packed[starts[local] as usize + 2..starts[local + 1] as usize] {
                s.arena.push(term_of(value));
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
    // (tail, morton, ordinal): the same order the pre-sort gives the shorts, continued past the
    // two-term prefix the pre-sort key stops at (decision 0073).
    s.longs.sort_unstable_by(|a, b| {
        arena[a.0..a.0 + a.1]
            .cmp(&arena[b.0..b.0 + b.1])
            .then_with(|| a.2.morton.cmp(&b.2.morton))
            .then_with(|| a.2.ordinal.cmp(&b.2.ordinal))
    });
    group[..s.shorts.len()].copy_from_slice(&s.shorts);
    for (k, &(_, _, rec)) in s.longs.iter().enumerate() {
        group[s.shorts.len() + k] = rec;
    }
}

/// The distinct category-width codes a column of values carries — the input
/// [`crate::layers::predicate_artifact_keys`] reads a predicate layer's roster out of.
///
/// **Absence is not a value**, so a point with no value for the column is in none of the layer's
/// artifacts, exactly as a member row with a null key is in none of an enumerated layer's. A value
/// of any other width contributes nothing either: `compile_membership` refuses such a column at the
/// declaration, so one reaching here is a schema that never validated rather than a value to guess
/// at.
pub(crate) fn distinct_codes(
    values: impl Iterator<Item = ScalarValue>,
) -> std::collections::BTreeSet<u32> {
    let mut codes = std::collections::BTreeSet::new();
    for value in values {
        match value {
            ScalarValue::U8(v) => codes.insert(u32::from(v)),
            ScalarValue::U16(v) => codes.insert(u32::from(v)),
            ScalarValue::U32(v) => codes.insert(v),
            _ => false,
        };
    }
    codes
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_types::IdentityKey;

    /// **Every declared field lands in exactly one home, and the two placement passes must agree
    /// on which** (records §3). The blob takes a field the entity-space pass declines, so the
    /// question both ask is `postings_are_owed`: a `derived` category keeps its entity-space
    /// floor and gets no blob row, while a `public` category with neither flag — which that pass
    /// declines, having no `index` and no `derived` visibility — must land here rather than
    /// nowhere.
    ///
    /// The `public` half is a regression test. Excluding every category from the blob reads as
    /// records §4.2's rule and is not: §4.2's premise is the entity-space floor, and a `public`
    /// category with no `index` has none. The field was stored in no home at all, and nothing
    /// refused the declaration — the caller declared a column and the corpus silently dropped it.
    #[test]
    fn a_category_is_blob_resident_exactly_when_it_has_no_entity_space_home() {
        let scratch_dir = tempfile::tempdir().expect("tempdir");
        let scratch = crate::column::ColumnScratch::new(scratch_dir.path());
        let category = crate::config::Attribute {
            name: "department".to_string(),
            field: None,
            title: None,
            ty: ScalarType::U16,
            analyser: None,
            vocabulary: Some("departments".to_string()),
            value_set: Some(crate::config::ValueSet::Closed),
            index: false,
            render: false,
        };
        let note = crate::config::Attribute {
            name: "note".to_string(),
            field: None,
            title: None,
            ty: ScalarType::Keyword,
            analyser: None,
            vocabulary: None,
            value_set: None,
            index: false,
            render: false,
        };

        // A `derived` visibility is what gives a category its entity-space floor.
        let vocabularies_at = |visibility| {
            let mut v = std::collections::HashMap::new();
            v.insert(
                "departments".to_string(),
                crate::config::Vocabulary {
                    name: "departments".to_string(),
                    title: None,
                    value_set: crate::config::ValueSet::Closed,
                    visibility,
                    width: ScalarType::U16,
                    codes: Default::default(),
                    titles: Default::default(),
                    reserved: Vec::new(),
                },
            );
            v
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let schema = crate::config::Schema {
            attributes: vec![category.clone(), note.clone()],
            vocabularies: vocabularies_at(crate::config::Visibility::Derived),
        };
        // One entity; values are per column, in declaration order.
        let by_entity = vec![
            EntityColumn::from_values(&scratch, ScalarType::U16, [ScalarValue::U16(7)], "colour")
                .expect("typed column"),
            EntityColumn::from_values(
                &scratch,
                ScalarType::Utf8,
                [ScalarValue::Utf8("kept".to_string())],
                "note",
            )
            .expect("typed column"),
        ];
        let written =
            write_record_blob(dir.path(), &schema, &by_entity).expect("blob stage writes");
        assert!(!written.is_empty());
        let blob = tessera_filter::RecordBlob::open_dir(
            &dir.path().join("attrs/record"),
            tessera_filter::Access::Mapped,
        )
        .expect("open");
        let fields = blob
            .fields_of(0)
            .expect("read")
            .expect("entity 0 has a row");
        assert_eq!(
            fields.len(),
            1,
            "only the utf8 column is blob-resident; the category's home is entity space"
        );
        assert_eq!(fields[0].tag, 1, "the surviving field is `note`, tag 1");

        // Alone, the `derived` category leaves the stage with nothing to write at all.
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = crate::config::Schema {
            attributes: vec![category.clone()],
            vocabularies: vocabularies_at(crate::config::Visibility::Derived),
        };
        let only_category =
            [
                EntityColumn::from_values(
                    &scratch,
                    ScalarType::U16,
                    [ScalarValue::U16(7)],
                    "colour",
                )
                .expect("typed column"),
            ];
        let written =
            write_record_blob(dir.path(), &schema, &only_category).expect("blob stage accepts");
        assert!(written.is_empty(), "no blob-resident column, no files");

        // But the same category under `public` owes no value column and no postings, so
        // the blob is its only home and must take it.
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = crate::config::Schema {
            attributes: vec![category],
            vocabularies: vocabularies_at(crate::config::Visibility::Public),
        };
        let only_category =
            [
                EntityColumn::from_values(
                    &scratch,
                    ScalarType::U16,
                    [ScalarValue::U16(7)],
                    "colour",
                )
                .expect("typed column"),
            ];
        let written =
            write_record_blob(dir.path(), &schema, &only_category).expect("blob stage writes");
        assert!(
            !written.is_empty(),
            "a public category with neither flag has no entity-space home; without a blob row \
             its values are stored nowhere at all"
        );
        let blob = tessera_filter::RecordBlob::open_dir(
            &dir.path().join("attrs/record"),
            tessera_filter::Access::Mapped,
        )
        .expect("open");
        assert_eq!(
            blob.fields_of(0)
                .expect("read")
                .expect("entity 0 has a row"),
            vec![tessera_filter::RecordField {
                tag: 0,
                value: tessera_filter::RecordValue::U16(7),
            }],
            "the public category's value is the blob row"
        );
    }

    /// **The chunking must not be observable in the artefact.** A chunk boundary is a place one
    /// worker's accumulator ends and another's begins, and a run boundary is a place one worker
    /// spills mid-chunk — so a column emitted in one chunk and the same column emitted in chunks
    /// of seven have to be the same two files. That is what makes the plan a memory knob rather
    /// than a format decision, and it is the assertion an off-by-one at a boundary fails: a term
    /// whose postings split across two runs and lost half of them changes only the bytes.
    ///
    /// The plans below straddle deliberately. `1` and `7` do not divide the corpus and do not
    /// align to the 64-entity words presence is stored in; `64` and `128` align exactly; `4_096`
    /// is one chunk for the whole column. The byte budgets force between one and dozens of runs a
    /// chunk, and the merge-slice caps put the same corpus through both posting encoders — a term
    /// carried by every entity goes through the `u32` slice under the large cap and through
    /// Roaring under the small one, and the two must agree byte for byte.
    #[test]
    fn chunking_the_text_index_does_not_change_its_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scratch_dir = tempfile::tempdir().expect("tempdir");
        let scratch = crate::column::ColumnScratch::new(scratch_dir.path());
        let attribute = text_fixture_attribute();
        let values = EntityColumn::from_values(
            &scratch,
            ScalarType::Text,
            (0..N_TEXT).map(text_fixture_prose),
            "abstract",
        )
        .expect("typed column");

        let mut files: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for (k, plan) in [
            TextIndexPlan::explicit(4_096, 1 << 30, 1 << 22),
            TextIndexPlan::explicit(1, 1 << 30, 1 << 22),
            TextIndexPlan::explicit(7, 1 << 30, 1 << 22),
            TextIndexPlan::explicit(64, 1 << 30, 1 << 22),
            TextIndexPlan::explicit(128, 1 << 30, 1 << 22),
            TextIndexPlan::explicit(999, 1 << 30, 1 << 22),
            // The byte budget, forcing several runs inside one chunk — including one small
            // enough that every document spills.
            TextIndexPlan::explicit(4_096, 1, 1 << 22),
            TextIndexPlan::explicit(4_096, 4_096, 1 << 22),
            TextIndexPlan::explicit(1_000, 4_096, 1 << 22),
            // The merge's other encoder: a cap of 1 sends every term above one posting through
            // the Roaring arm.
            TextIndexPlan::explicit(4_096, 1 << 30, 1),
            TextIndexPlan::explicit(7, 1, 1),
        ]
        .into_iter()
        .enumerate()
        {
            let column_dir = dir.path().join(format!("plan-{k}"));
            std::fs::create_dir_all(&column_dir).expect("column dir");
            let written =
                write_text_index(&column_dir, &attribute, &values, plan).expect("text index");
            assert_eq!(
                written.terms,
                expected_text_postings().len() as u64,
                "plan {k} wrote the wrong term count"
            );
            // Nothing but the two artefacts is left behind: every run this plan spilled is gone.
            let left: Vec<String> = std::fs::read_dir(&column_dir)
                .expect("read dir")
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".spill"))
                .collect();
            assert!(left.is_empty(), "plan {k} left {left:?} behind");
            files.push((
                std::fs::read(column_dir.join(tessera_filter::DICT_FILE)).expect("dict"),
                std::fs::read(column_dir.join("postings.arrow")).expect("postings"),
            ));
        }
        let (dict, postings) = files.first().expect("at least one plan").clone();
        for (k, emitted) in files.iter().enumerate().skip(1) {
            assert_eq!(emitted.0, dict, "plan {k}'s dictionary differs");
            assert_eq!(emitted.1, postings, "plan {k}'s postings differ");
        }
    }

    /// And the index says what the column says: every term the analyser produces is a key, and its
    /// posting is exactly the entities whose prose carries it — read back through the readers that
    /// will serve it, over a plan of many chunks and many runs apiece, so what is asserted is the
    /// merge's output and not one worker's accumulator.
    #[test]
    fn the_merged_text_index_holds_every_term_and_the_entities_carrying_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scratch_dir = tempfile::tempdir().expect("tempdir");
        let scratch = crate::column::ColumnScratch::new(scratch_dir.path());
        let values = EntityColumn::from_values(
            &scratch,
            ScalarType::Text,
            (0..N_TEXT).map(text_fixture_prose),
            "abstract",
        )
        .expect("typed column");
        write_text_index(
            dir.path(),
            &text_fixture_attribute(),
            &values,
            TextIndexPlan::explicit(97, 2_048, 1 << 22),
        )
        .expect("text index");

        let dict = tessera_filter::SortedDict::open(
            &dir.path().join(tessera_filter::DICT_FILE),
            tessera_filter::Access::Read,
        )
        .expect("the token dictionary opens");
        let postings = tessera_authz::postings::PostingsReader::open(
            &dir.path().join("postings.arrow"),
            false,
        )
        .expect("the postings open");

        let expected = expected_text_postings();
        assert_eq!(dict.len() as usize, expected.len());
        assert_eq!(postings.term_count() as usize, expected.len());
        for (ordinal, (term, entities)) in expected.iter().enumerate() {
            assert_eq!(
                dict.resolve(term).expect("a readable dictionary"),
                Some(ordinal as u32),
                "term {term:?} is at the wrong ordinal"
            );
            let posting = postings
                .posting_at(ordinal as u32)
                .expect("a readable posting")
                .expect("every term in the dictionary has a posting");
            let got: Vec<u32> = match posting {
                tessera_authz::postings::PostingRef::Array(bytes) => bytes
                    .chunks_exact(4)
                    .map(|c| u32::from_le_bytes(c.try_into().expect("four bytes")))
                    .collect(),
                tessera_authz::postings::PostingRef::Roaring(view) => view.iter().collect(),
            };
            assert_eq!(&got, entities, "term {term:?} carries the wrong entities");
        }
    }

    /// The corpus both text tests read. 4,096 entities, and every shape the emit has to survive:
    /// a term carried by every entity (well past the Roaring threshold and past a 64-entity
    /// presence word), terms carried by exactly one, a term repeated inside one document, mixed
    /// scripts, whole absent runs longer than a presence word, and the empty string — which is a
    /// legal value that yields no term, and is not absence.
    const N_TEXT: usize = 4_096;

    fn text_fixture_attribute() -> crate::config::Attribute {
        crate::config::Attribute {
            name: "abstract".to_string(),
            field: None,
            title: None,
            ty: ScalarType::Text,
            analyser: Some(
                tessera_analyse::analyser(tessera_analyse::UNICODE)
                    .expect("the unicode analyser ships")
                    .identity(),
            ),
            vocabulary: None,
            value_set: None,
            index: true,
            render: false,
        }
    }

    fn text_fixture_prose(entity: usize) -> ScalarValue {
        // A 64-entity absent run, word-aligned, and a second one that is not.
        if (256..320).contains(&entity) || (1_001..1_101).contains(&entity) {
            return ScalarValue::Null;
        }
        if entity.is_multiple_of(313) {
            return ScalarValue::Utf8(String::new());
        }
        let mut prose = format!("common item{entity}");
        match entity % 5 {
            0 => prose.push_str(" quick brown fox"),
            1 => prose.push_str(" quick quick silver"),
            2 => prose.push_str(" 日本語のテキスト quick"),
            3 => prose.push_str(" Ω STRASSE ﬁle"),
            _ => prose.push_str(" brown bear"),
        }
        ScalarValue::Utf8(prose)
    }

    /// The expected `(term, entities)` set, derived from the fixture through the analyser itself —
    /// the same route `tessera tokenise` gives the conformance oracle, so this asserts the index
    /// against the analyser rather than against a second tokeniser that could drift.
    fn expected_text_postings() -> Vec<(String, Vec<u32>)> {
        let analyser = tessera_analyse::analyser(tessera_analyse::UNICODE).expect("the analyser");
        let mut terms: std::collections::BTreeMap<String, Vec<u32>> = Default::default();
        for entity in 0..N_TEXT {
            let ScalarValue::Utf8(prose) = text_fixture_prose(entity) else {
                continue;
            };
            for token in analyser.tokens(&prose) {
                let postings = terms.entry(token).or_default();
                if postings.last() != Some(&(entity as u32)) {
                    postings.push(entity as u32);
                }
            }
        }
        terms.into_iter().collect()
    }

    /// **The band count must not be observable in the artefact.** A band boundary is a place the
    /// scatter restarts and the ascending-key check spans, so a column emitted in one band and the
    /// same column emitted in a band per code have to be the same file — which is also what makes
    /// the band budget a memory knob rather than a format decision.
    #[test]
    fn banding_the_postings_emit_does_not_change_its_bytes() {
        let scratch_dir = tempfile::tempdir().expect("tempdir");
        let scratch = crate::column::ColumnScratch::new(scratch_dir.path());
        let dir = tempfile::tempdir().expect("tempdir");
        // Codes scattered across a 32-bit space, as `vocabulary` mints them, with one code held
        // heavily enough to cross the Roaring threshold and code 0 (absent) carried too.
        let values = EntityColumn::from_values(
            &scratch,
            ScalarType::U32,
            (0..5_000u32).map(|e| {
                ScalarValue::U32(match e % 7 {
                    0 => tessera_store::vocabulary::ABSENT_CODE,
                    1 => 3_999_999_999,
                    2 => 17,
                    _ => 1_000 + (e % 53),
                })
            }),
            "colour",
        )
        .expect("typed column");

        let mut files: Vec<Vec<u8>> = Vec::new();
        for band_rows in [1usize, 2, 100, 1_000, usize::MAX] {
            let path = dir.path().join(format!("postings-{band_rows}.arrow"));
            write_category_postings(&path, "colour", &values, band_rows).expect("emit");
            files.push(std::fs::read(&path).expect("read"));
        }
        for f in &files[1..] {
            assert_eq!(f, &files[0]);
        }

        // And the postings say what the column says: each code's posting is exactly the entities
        // carrying it, which is what makes one a derivative of the other.
        let tier =
            tessera_authz::DeltaTier::open(&dir.path().join("postings-1.arrow")).expect("open");
        let mut codes: Vec<u32> = values
            .iter()
            .map(|v| category_code(&v, "colour").expect("code"))
            .collect::<Vec<_>>();
        codes.sort_unstable();
        codes.dedup();
        for code in codes {
            let posting = tier.posting_at(code).expect("read");
            let expected: Vec<u32> = values
                .iter()
                .enumerate()
                .filter(|(_, v)| category_code(v, "colour").expect("code") == code)
                .map(|(e, _)| e as u32)
                .collect();
            if code == tessera_store::vocabulary::ABSENT_CODE {
                assert!(posting.is_none(), "the absent code earns no posting");
                continue;
            }
            let got = match posting.expect("carried") {
                tessera_authz::PostingRef::Roaring(view) => view.iter().collect::<Vec<_>>(),
                tessera_authz::PostingRef::Array(bytes) => bytes
                    .chunks_exact(4)
                    .map(|c| u32::from_le_bytes(c.try_into().expect("four bytes")))
                    .collect(),
            };
            assert_eq!(got, expected, "code {code}");
        }
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
