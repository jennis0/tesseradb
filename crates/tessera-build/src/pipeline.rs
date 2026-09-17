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
//! | points ×2 | the ordinal space — an item's **ordinal** is its index in the sorted union | N/8 bits, or 8N mapped |
//! | pairs ×1 | dictionary (streamed), term-lookup arrays, pre-dedup `row_counts`, histogram | 12T + 8T |
//! | pairs ×1 | resolved `ordinal << 32 \| term` per **batch bucket** (RAM when it fits, spilled otherwise) | plan |
//! | per batch | sort+dedup the bucket; per-ordinal `starts`; signature sort + refinement; entity ids; **band emit** | plan |
//! | per band | cursor-scatter (already sorted), Roaring-encode in sub-chunks, spool + `pairs.parquet` | plan |
//! | — | postings assembly: the spool becomes `postings.arrow`'s one record batch, zero-copy | 8(T+1) |
//! | points ×1 | external ids (when minting) | 20N |
//! | points ×1 | geometry, the tiler sort, and the segment | 28N |
//!
//! ## A column no pass reads at an entity is never permuted
//!
//! A declared value is placed at its entity index as the join resolves it, and for a string
//! column that is a permutation of the source's characters through a mapping larger than memory.
//! A column whose only reader is the record blob may be spared it: each join chunk of it is
//! written as one record-blob extent in that chunk's entity order ([`ColumnRoutes`],
//! [`crate::extents`], `build-column-extents.md`), the blob merges the extents, and a `text`
//! column's token index reads them in block windows first.
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
//! *establishes* is the ordinal space ([`Ids`]): the sorted, duplicate-free union of every view's
//! ids, and an item's ordinal is its index in it.
//!
//! The union takes one of two shapes and pass one checks which rather than assuming it. Where it
//! is the whole range `first..first + len`, an ordinal is `id - first` and there is no array: pass
//! one proves the range from a presence bitmap an eighth of a bit per id in the span, and writes
//! nothing. At the GBIF rung that is 28 GB of writes and one sort of 3.5×10⁹ `u64`s not made.
//! Where it is not a range, the union is an array under `.build-tmp/`, and each pass that must map
//! a source id back to its ordinal ([`join_chunk`]) buffers a bounded chunk of rows, sorts the
//! chunk, and resolves the whole chunk in **one sequential merge sweep** against it — sequential
//! memory traffic for any id distribution, where a per-row binary search over an 8 GB array at 10⁹
//! was a random cache-and-TLB miss per probe (measured as the dominant cost of the geometry pass,
//! whose input arrives in Morton order).
//!
//! Within a chunk, rows carrying the same id keep no particular order; every consumer is
//! insensitive to it (each call site argues why), so build output stays byte-deterministic. Both
//! shapes hand a chunk the same sequence, so the output is the same bundle either way — which it
//! must be, the ordinal being what the entity-id assignment breaks its last tie on (I9).
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
//! with its own presence bits and its own arena, indexed by entity, and each spilled column its
//! own extent writer ([`crate::extents`]), so no two lanes can name the same byte. The only
//! shared state is read-only — the join's answer and the declaration — and the
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
    Codes, ColumnKind, RecordFieldRef, RecordValueRef, ValueColumnWriter, RECORD_BLOCKS_FILE,
    RECORD_BLOCK_TARGET, RECORD_DIRECTORY_FILE, RECORD_HASROW_FILE,
};
use tessera_plugin::{Passthrough, Plugin};
use tessera_spatial::split32;
use tessera_spatial::tiler::{ScalarType, ScalarValue};
use tessera_types::SMALL_TERM_THRESHOLD_DEFAULT;

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
pub(crate) fn staging_rows(attributes: &[&crate::config::Attribute], n: u64) -> usize {
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

/// The build's ordinal space as a pass sees it. An **ordinal** is an id's index in the sorted,
/// deduplicated union of every view's source ids.
///
/// Two shapes, because a corpus that numbers its rows from zero needs no array to answer with.
/// Where the union is the whole range `first..first + len`, an id's index is `id - first` and an
/// index's id is `first + index`; where it is not, the array is the only route and every lookup
/// reads it. Pass one decides which ([`read_source_ids_union`]) and writes no array where the
/// range holds.
///
/// **Both arms are the same function.** The range arm is taken only where pass one has checked,
/// id by id, that the union is exactly that range, so an ordinal is the same integer either way.
/// The ordinal is identity-bearing three times over — the signature sort's last tiebreak
/// ([`SortRec::order`]), the term ids' ranking by minimum ordinal ([`build_dictionary`]) and the
/// batch partition — so an arm taken on a union that is not that range would move every entity id
/// the build assigns, which I9 does not allow.
#[derive(Clone, Copy)]
pub(crate) enum Ids<'a> {
    /// The union is `first..first + len`, proved before any array was written.
    Contiguous { first: u64, len: usize },
    /// The union, sorted and duplicate-free.
    Sparse(&'a [u64]),
}

impl Ids<'_> {
    fn len(&self) -> usize {
        match self {
            Ids::Contiguous { len, .. } => *len,
            Ids::Sparse(ids) => ids.len(),
        }
    }

    /// The id at an ordinal. An ordinal outside the space is a caller's bug, exactly as it was
    /// when this was an index into the array.
    fn id_of(&self, ordinal: usize) -> u64 {
        match self {
            Ids::Contiguous { first, len } => {
                assert!(
                    ordinal < *len,
                    "ordinal {ordinal} is outside the {len} items this build holds"
                );
                first + ordinal as u64
            }
            Ids::Sparse(ids) => ids[ordinal],
        }
    }

    /// The ordinal an id resolves to, for a caller that holds one id rather than a chunk of them.
    ///
    /// [`join_chunk`] is what every sequential pass uses; this is the random-access route, and on
    /// the array arm it is the binary search that route exists for.
    fn ordinal_of(&self, id: u64) -> Option<u32> {
        match self {
            Ids::Contiguous { first, len } => id
                .checked_sub(*first)
                .filter(|ordinal| *ordinal < *len as u64)
                .map(|ordinal| ordinal as u32),
            Ids::Sparse(ids) => ids.binary_search(&id).ok().map(|ordinal| ordinal as u32),
        }
    }

    /// The first id where the union is one unbroken range, and `None` where it is not.
    ///
    /// The array arm checks the range rather than assuming it: the ids are sorted and
    /// duplicate-free, so a span equal to their own count can only be `first + i` at every `i`.
    /// `checked_sub` and `checked_add` because a union holding both 0 and `u64::MAX` spans one
    /// more than the type holds, and that union is not a range.
    ///
    /// An empty union answers `None` on both arms: there is no first id, and a caller resolving
    /// against no items takes the route that answers nothing rather than a subtraction.
    fn contiguous_from(&self) -> Option<u64> {
        match self {
            Ids::Contiguous { first, len } => (*len > 0).then_some(*first),
            Ids::Sparse(ids) => {
                let first = *ids.first()?;
                let last = *ids.last()?;
                let span = last.checked_sub(first)?.checked_add(1)?;
                (span == ids.len() as u64).then_some(first)
            }
        }
    }
}

/// Resolve a chunk of `(source_id, payload)` rows to ordinals: a subtraction per row where the id
/// space is a range, and otherwise by sorting the chunk and merging it against the sorted array in
/// one sequential sweep. See the module docs: this is how every pass maps ids to ordinals without
/// assuming anything about the ids' shape, and without a random-access probe per row.
///
/// `on_row` receives `(Some(ordinal), source_id, payload)` for a resolved row and
/// `(None, source_id, payload)` for an id the space does not hold — whether an absent id is an
/// error, and which error, is the calling pass's decision (the dictionary pass collects them;
/// every later pass fails closed, because its first pass over the same file resolved them).
///
/// Rows with equal ids reach `on_row` in no particular order (the chunk sort is unstable and
/// keyed on the id alone); callers must be — and each caller's site comments argue that they
/// are — insensitive to that order. The chunk is drained; capacity is retained for reuse.
fn join_chunk<P: Copy + Send>(
    chunk: &mut Vec<(u64, P)>,
    ids: Ids<'_>,
    mut on_row: impl FnMut(Option<u32>, u64, P) -> Result<()>,
) -> Result<()> {
    // **Stable**, so rows carrying the same source id resolve in the order the file gave them.
    // Which of two duplicate rows' values an entity ends up with was previously whichever the
    // sort happened to place last; a build that answers the same question twice should answer it
    // the same way (§7's last-write-wins).
    //
    // The subtraction arm does not need the sort — each row resolves on its own — and keeps it
    // anyway: that ordering is what every caller's own order-insensitivity argument is written
    // against, and the two arms must hand `on_row` the same sequence. It sorts a bounded chunk,
    // never the corpus.
    chunk.par_sort_by_key(|entry| entry.0);
    match ids {
        Ids::Contiguous { first, len } => {
            for &(id, payload) in chunk.iter() {
                let ordinal = id
                    .checked_sub(first)
                    .filter(|ordinal| *ordinal < len as u64)
                    .map(|ordinal| ordinal as u32);
                on_row(ordinal, id, payload)?;
            }
        }
        Ids::Sparse(source_ids) => {
            let mut i = 0usize;
            for &(id, payload) in chunk.iter() {
                // Both sides ascend, so `i` only ever moves forward; it does not advance past a
                // match, so a run of rows carrying the same id all resolve to the same ordinal.
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
        }
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
    ids: Ids<'_>,
    term_keys: &[u64],
    term_ids: &[u32],
    distinct_of_ordinal: &mut [u32],
    mut emit: impl FnMut(u64) -> Result<()>,
) -> Result<()> {
    resolved.clear();
    join_chunk(chunk, ids, |ordinal, source_id, source_term| {
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
                writers[batch].push(value).map_err(BuildError::from)
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
                    .collect::<tessera_store::Result<_>>()?,
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
                Ok(spill::read_bucket(receipt)?)
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
    /// Which string columns spill their characters as record-blob extents rather than filling an
    /// entity-ordered arena. Derived from the free space rather than from the budget beside it,
    /// and the one choice in this plan that changes no byte of the bundle ([`ColumnRoutes`],
    /// [`crate::residency::plan_routes`]).
    routes: ColumnRoutes,
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
pub(crate) fn available_disk(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    Some(stats.f_bavail as u64 * stats.f_frsize as u64)
}

#[cfg(not(unix))]
pub(crate) fn available_disk(_path: &std::path::Path) -> Option<u64> {
    None
}

/// The auto-batching grid: derived batch sizes are multiples of 2^24 items, so small budget
/// differences between machines derive the same size — an accidental identity fork needs a
/// budget step of a whole grid cell, not a few megabytes.
const BATCH_GRID: u64 = 1 << 24;

#[allow(clippy::too_many_arguments)]
fn plan_build(
    args: &BuildArgs,
    n: u64,
    route: crate::ExtentRoute,
    ids: crate::residency::IdShape,
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
    // **The string columns' characters are measured once here**, both models wanting them and the
    // sample decoding rows to get them (`residency::payloads_per_item`).
    let payloads = crate::residency::payloads_per_item(args);
    // **The route each string column takes, and the model it settles on.** A column with two
    // routes takes the arena while the entity-order stages have the disk for it and the extents
    // when they do not (`residency::plan_routes`, `build-column-extents.md` §2). The refusal below
    // is not affected by the choice: every column's storage is a mapped term and `total()` counts
    // the anonymous ones.
    let free = available_disk(&args.out);
    let (routes, tail) = crate::residency::routes_for(args, n, ids, &payloads, free, route);
    report_column_routes(args, &routes, &tail, free);
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
    // beside the loop-wide distinct-terms-per-ordinal tally (4/item over all n), per-term
    // counters (4/term), the join-chunk buffer (which scales down with the corpus, so a tiny test
    // budget stays feasible for a tiny corpus) and a fixed slack for band buffers, decoders and
    // allocator.
    //
    // **The leading `4 * n` was that tally's, and what it now pays for is the assignment walk's
    // entity window.** The tally became a file under `.build-tmp/` on 2026-09-10, as the
    // ordinal→entity map beside it did before, and the term was kept because it feeds `feasible`,
    // `feasible` picks `auto_batch`, and a batch stride partitions entity-id space — so dropping
    // it would give every budget-constrained corpus a different stride and with it a different
    // permanent entity-id assignment, which I9 does not allow to change. The walk now fills a
    // heap `Vec<u32>` of the batch's length and writes it into the entity map in one call, which
    // is `batch_items * 4 ≤ n * 4`: the retained term is that window, named rather than
    // coincidental. Removing it, to buy larger batches, is a separate decision the owner has not
    // taken — ruling 6 of the disk-use campaign, 2026-09-10.
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
    // **A stride of at least one item**, so an empty corpus divides. `n = 0` derives a batch of
    // zero from every arm above (the whole corpus is the whole corpus), and the batch loop below
    // runs `0..batches` — so the stride only has to be a legal divisor, and the plan it produces
    // is zero batches over zero items.
    let batch_items = batch_items.max(1);
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

    // ---- the disk pre-flight ------------------------------------------------------------------
    // The build's transient spills and its outputs coexist in phases, so the forecast is the
    // largest phase and not a total nothing ever holds. `residency::disk` names every term and the
    // phases it stands through; nothing is derived twice here.
    //
    // **It warns and does not refuse.** The model can be wrong in either direction — several of its
    // terms are stated ceilings a corpus can exceed, and several corpus shapes cost bytes no term
    // covers — so it is not a figure to hold a door with. A build that runs out of disk writes no
    // `CURRENT`, publishes no identity and mints no id that survives, and the partial prefix is
    // swept, so what a wrong admission costs is the build's wall clock. What a wrong refusal cost
    // was the same wall clock with no way to say "I accept the risk": there is no flag,
    // environment variable or configuration key that moves this figure. That is the house rule for
    // something recoverable that discloses nothing — print the numbers and leave the decision with
    // the operator — and it is the shape the memory model above already uses for its own band.
    let p = pair_rows as u64;
    let corpus = crate::residency::Corpus {
        n,
        pair_rows: p,
        term_rows: row_counts,
        batches,
        bucket_in_ram,
    };
    let disk = crate::residency::disk(args, corpus, &payloads, &tail);
    let (phase, disk_need) = disk.peak();
    // **Printed whatever the free space is.** An operator sizing a corpus has no other way to ask
    // what a build will cost the disk, and the campaign's rung 6 died at hour three on a forecast
    // nobody could read (`probes/2026-09-10-build-disk/`).
    eprintln!(
        "disk: ~{} MiB at peak, in the {} phase{}",
        disk_need >> 20,
        phase.name(),
        crate::residency::Phase::ALL
            .iter()
            .map(|&ph| format!(" ({} {} MiB)", ph.name(), disk.at(ph) >> 20))
            .collect::<String>()
    );
    if let Some(warning) =
        free.and_then(|free| crate::residency::forecast_warning(&disk, corpus, free))
    {
        eprintln!("{warning}");
    }

    Ok(BuildPlan {
        budget,
        routes,
        batch_items,
        batches,
        bucket_in_ram,
        band_bounds,
        recorded_batch_items: (batches > 1).then_some(batch_items),
    })
}

/// **Printed, because the route the build took is what makes two runs' timings comparable.**
///
/// It is derived from the space free on the output filesystem, so a box that fills up can route a
/// corpus one way today and the other way tomorrow. Nothing downstream can see the difference —
/// the bundle is byte-identical either way — but the build's own wall clock moves with it, 5 to
/// 10% at the row counts measured, and a run whose log does not say which route it took cannot be
/// read against another's.
fn report_column_routes(
    args: &BuildArgs,
    routes: &ColumnRoutes,
    tail: &crate::residency::Residency,
    free: Option<u64>,
) {
    let available = args
        .schema
        .attributes
        .iter()
        .filter(|a| may_take_extents(&args.schema, a))
        .count();
    if available == 0 {
        return;
    }
    let spilled = routes.spilled();
    eprintln!(
        "columns: {} of {available} string column(s) spill their characters as record-blob \
         extents, the entity-order stages modelling {} MiB of scratch against {} free{}",
        spilled.len(),
        crate::residency::stage_scratch(tail) >> 20,
        match free {
            Some(bytes) => format!("{} MiB", bytes >> 20),
            None => "an unreadable amount of space".to_string(),
        },
        match spilled.is_empty() {
            true => String::new(),
            false => format!(
                ": {}",
                spilled
                    .iter()
                    .map(|&index| {
                        let attribute = &args.schema.attributes[index];
                        let why = match extents_are_forced(&args.schema, attribute) {
                            true => "text",
                            false => "no room for its arena",
                        };
                        format!("{} ({why})", attribute.name)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    );
}

/// Run the build, and **take the partial prefix with it if it does not finish**.
///
/// `.build-tmp/` has always gone back on the way out ([`spill::TmpDir`]), and the prefix under
/// `<out>/` did not — so a build that ran out of disk left its bytes behind and the retry started
/// with less free space than the first attempt had. The rung-6 attempt died at hour three with
/// 93 GB of bundle written. Nothing published it: `CURRENT` is written last and
/// [`crate::validate_args`] refuses to start where one already exists, so a prefix here is a
/// prefix no reader can resolve, which is the proof
/// [`tessera_store::reclaim_unpublished_prefix`] makes for itself before deleting anything.
///
/// That refusal is why the argument check runs here rather than inside [`build_bundle`]. It is
/// the one error that fires while `<out>/CURRENT` exists, which is the one state the sweep
/// refuses to delete in, so leaving it under the sweep would print a warning naming a published
/// bundle on every build into a directory that already holds one.
///
/// **A failure to sweep is a warning and nothing else.** The build has already failed and the
/// error the caller gets is the one worth having; a tree that cannot be removed is the residual
/// that stood before this existed.
pub(crate) fn build(
    args: &BuildArgs,
    observer: &dyn BuildObserver,
    route: crate::ExtentRoute,
) -> Result<BuildReport> {
    // **First, before any validation**, so a run whose log is all that survives says which source
    // produced it. A campaign of 2026-09-12 lost two and three-quarter hours to a binary seven
    // commits behind the tree its figures were read against.
    eprintln!("tessera build: commit {}", crate::BUILD_COMMIT);
    validate_args(args)?;
    let outcome = build_bundle(args, observer, route);
    if outcome.is_err() {
        let prefix = args.out.join(PREFIX);
        if prefix.is_dir() {
            match tessera_store::reclaim_unpublished_prefix(&prefix) {
                Ok(()) => eprintln!(
                    "swept the partial bundle at {}: this build wrote no CURRENT, so nothing \
                     names it and a retry has the disk back",
                    prefix.display()
                ),
                Err(e) => eprintln!(
                    "warning: the partial bundle at {} could not be swept ({e}); it stands, and \
                     its disk with it",
                    prefix.display()
                ),
            }
        }
    }
    outcome
}

fn build_bundle(
    args: &BuildArgs,
    observer: &dyn BuildObserver,
    route: crate::ExtentRoute,
) -> Result<BuildReport> {
    let mut timer = StageTimer::new(observer);
    let plugin = Passthrough::new();
    require_decomposable_labelling(&plugin)?;
    let bounds = plugin.declared_bounds();
    // **How this declaration names a row, decided before pass one** (`crate::ids`): the identity
    // column's type says whether a source id is the integer the file holds or the rank of a
    // supplied key, and the supplied keys are interned here, once, for every pass below.
    let id_space = crate::ids::IdSpace::prepare(args)?;

    // ---- 1. pass one: entity space, once over every view's points (`views.md` §7) -----
    // An item's *ordinal* is its index in this array. Ordinal order is source-id order over the
    // **union** of every view's ids, which is the order the linear build walks items in — so
    // "first appearance" below, and the source-id tiebreak in the signature sort, are both
    // expressible as ordinal comparisons, exactly as they were when a build read one file.
    //
    // **`n = 0` is a bundle, not a refusal** (decision 0091): a deployment must be able to start
    // from a bundle with no points, with its frame stated, and take the whole corpus through
    // `/control/ingest`. Every stage below is walked for that case — empty segments, empty
    // postings, a dictionary holding only what the declaration mints, `entity_id_high_water = 0`,
    // every declared column present and empty, every declared layer registered with no artifacts.
    // What is still refused is `extent = "auto"` over no rows, because a frame cannot be fitted to
    // nothing (`config::empty_auto_source`) — and that refusal already names the remedy.
    // Open here rather than at the bucket sink below, because the first thing under it is the
    // source ids: `.build-tmp/` is the build's scratch from pass one to the segment write, and
    // every structure in it is swept by the `close` at the end or by the next build's `create`.
    let tmp = spill::TmpDir::create(&args.out)?;
    let (source_ids, view_anchors) = read_source_ids_union(args, tmp.path(), &id_space)?;
    let n = source_ids.len() as u64;
    if n > u32::MAX as u64 {
        return Err(BuildError::Invalid(format!(
            "{n} items exceeds bundle_format 1's 2^32 entity-ID ceiling"
        )));
    }
    // The union's largest id, which sets the varint width the member spill's runs are charged at
    // ([`crate::residency::IdShape`]). Zero over no items: it is a model's term and not a lookup,
    // and a corpus with no ids spills no runs.
    //
    // Each view's own anchors — its row count and an order-independent mixed sum of its ids
    // ([`mix64`]) — are in `view_anchors`, and the geometry pass below is checked against them: a
    // points file swapped mid-build would otherwise hand every item of that view another item's
    // position, with nothing to notice.
    let ids_last = source_ids.extrema().map_or(0, |(_, last)| last);

    timer.end(BuildStage::SourceIds, source_ids.len() as u64);

    // ---- 2. the dictionary -----------------------------------------------------------
    let dict_dir = args.out.join(PREFIX).join("dictionary");
    std::fs::create_dir_all(&dict_dir).map_err(|e| BuildError::io(&dict_dir, e))?;
    // What every source term is called, established before any term id exists — a field-sourced
    // view's sorted vocabulary, or the relation's own integers (`crate::AccessPlan`).
    let access = crate::plan_access(args, &id_space)?;
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
    } = build_dictionary(args, &access, source_ids.ids(), &dict_dir, &id_space)?;
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
    let id_shape = crate::residency::IdShape {
        slots: source_ids.slots() as u64,
        max_id: ids_last,
    };
    let plan = plan_build(
        args,
        n,
        route,
        id_shape,
        pair_rows,
        &row_counts,
        &histogram,
        histogram_shift,
    )?;
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
    //
    // **File-backed**, which is [`spill::MappedArray`]'s case exactly: written at a scattered
    // index by the sweep in [`resolve_pairs_chunk`], read in ordinal order by the
    // label-agreement pass the batch loop runs ahead of its assignment walk, never sorted. At 4 B/item it was the build's largest anonymous
    // structure — 13.0 GiB at the GBIF rung's 3.50×10⁹ items (modelled, items × 4 B) — and mapped
    // it is page cache the kernel may evict rather than memory the machine must have.
    // `plan_build`'s `loop_fixed` charges the same 4 B/item still, on purpose and for I9's sake:
    // the comment at that term says why.
    let mut distinct_map =
        spill::MappedU32::zeroed(tmp.path(), "distinct-of-ordinal.u32", n as usize)?;
    let distinct_of_ordinal = distinct_map.as_mut_slice();
    let mut resolve = |chunk: &mut Vec<(u64, u64)>,
                       resolved: &mut Vec<(u64, u64)>,
                       distinct_of_ordinal: &mut [u32],
                       sink: &mut BucketSink| {
        resolve_pairs_chunk(
            chunk,
            resolved,
            source_ids.ids(),
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
    crate::scan_access(args, &access, &id_space, |view, source_id, source_term| {
        // A chunk **never spans two views**, and never splits a row: rows of one view arrive
        // contiguously (a view holds one row per entity), so the boundary is taken at the change
        // of view — always — or at the next change of entity once the chunk is full.
        let changed_view = current.is_some_and(|(previous, _)| previous != view);
        let changed_row = current != Some((view, source_id));
        if changed_view || (changed_row && chunk.len() >= JOIN_CHUNK_ROWS) {
            if let Err(e) = resolve(&mut chunk, &mut resolved, distinct_of_ordinal, &mut sink) {
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
    resolve(&mut chunk, &mut resolved, distinct_of_ordinal, &mut sink)?;
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
    //
    // **A mapped file**, on the same argument as the geometry beside it: 4 B an item is 13.3 GiB
    // at the GBIF rung, held from here to the end of the batch loop, and as anonymous memory it
    // was the largest term of that loop's residency that no model named.
    let mut appearances = spill::MappedU32::zeroed(tmp.path(), "appearances.u32", n as usize)?;
    for (index, view) in args.views.iter().enumerate() {
        let mut x_map =
            spill::MappedU32::zeroed(tmp.path(), &format!("x-of-ordinal-{index}.u32"), n as usize)?;
        let mut y_map =
            spill::MappedU32::zeroed(tmp.path(), &format!("y-of-ordinal-{index}.u32"), n as usize)?;
        let mut present: Vec<u64> = vec![0; (n as usize).div_ceil(64)];
        {
            let xs = x_map.as_mut_slice();
            let ys = y_map.as_mut_slice();
            let apps = appearances.as_mut_slice();
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
                join_chunk(chunk, source_ids.ids(), |ordinal, source_id, (x, y)| {
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
                &id_space,
                |point| {
                    chunk.push((point.source_id, (point.qx, point.qy)));
                    if chunk.len() == JOIN_CHUNK_ROWS {
                        if let Err(e) = resolve(
                            &mut chunk,
                            xs,
                            ys,
                            &mut present,
                            apps,
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
                apps,
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
    //
    // **Where the anchor holds every item there is nothing to materialise**, the fallback reaching
    // no ordinal and the array it would write being the anchor view's own, value for value. A
    // view's ids are distinct (pass one checks each view's for duplicates as it fills its segment),
    // so a view whose row count is the union's is a view holding every ordinal. That is every
    // single-view corpus and every multi-view one whose anchor covers the union — 8 B/item of
    // reserved disk and an `n`-length copy, both for a file byte-identical to one already written.
    let anchor_fallback_reaches_an_item = geometry[args.anchor].rows != n;
    let anchor_maps = if anchor_fallback_reaches_an_item {
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
        Some((anchor_x_map, anchor_y_map))
    } else {
        None
    };
    let (x_of_ordinal, y_of_ordinal) = match &anchor_maps {
        Some((x, y)) => (x.as_slice(), y.as_slice()),
        None => (
            geometry[args.anchor].x.as_slice(),
            geometry[args.anchor].y.as_slice(),
        ),
    };
    timer.end(BuildStage::GeometryRead, n);

    // `source_ids` is **held** past this point rather than dropped and re-read: pass one unions
    // several files, so recovering it later would be one re-read per view against anchors that
    // would each have to be carried anyway. 8 B/item. Step 8c releases it: before the layer join
    // where the ids are contiguous, since the join reads only their first value and their count
    // there, and after it where they are not.
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
    //
    // **File-backed**, which is [`spill::MappedArray`]'s case exactly: written once at a scattered
    // index by the walk below, read once at a scattered index by every pass after it, never
    // sorted. 0.87 GiB at rung 5's 2.33×10⁸ items and 13.2 GiB at the GBIF rung's 3.54×10⁹
    // (modelled, items × 4 B), alive from here to the last view's permutation. `entities` is the
    // assignment walk's writable binding; every reader after the walk takes the shared slice
    // bound below.
    let mut entity_map = spill::MappedU32::zeroed(tmp.path(), "entity-of-ordinal.u32", n as usize)?;
    let entities = entity_map.as_mut_slice();
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
    // The entity→term transpose (contracts §2.4), written in the same walk that assigns entity
    // ids: entities ascend with position within a batch and bases ascend across batches, so this
    // loop already visits them in the strictly ascending order the writer requires, and each
    // item's `sig` is already its sorted, deduplicated term list. Writing it here rather than from
    // the bands costs no second pass and no second relation in memory.
    let entity_terms_dir = args
        .out
        .join(PREFIX)
        .join("partitions")
        .join(PHASH)
        .join(tessera_store::ENTITY_TERMS_DIR);
    let mut entity_terms = tessera_store::EntityTermsWriter::create(&entity_terms_dir)
        .map_err(|e| BuildError::Invalid(format!("entity-terms transpose: {e}")))?;
    let mut sig_terms: Vec<u32> = Vec::new();
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

        // **The label is the entity's, not the row's** (`views.md` §7): every view holding an
        // item must have given it the same term set. The item's signature is the deduplicated
        // union over the views, and `distinct_of_ordinal` is the sum of each view's own distinct
        // count — so the two agree exactly when every view contributed the whole union, and the
        // identity is a refusal rather than a hash comparison. The first offending item is
        // reported in ordinal order.
        //
        // Checked only on the per-view route: a shared relation is entity space already and is
        // scanned once, so there is nothing for two views to disagree about
        // (`crate::AccessRoute`).
        //
        // Here, ahead of the sort and the assignment walk, because it wants only `starts` and the
        // two counters: the three arrays are read in ordinal order rather than the entity order
        // the walk visits records in. Over a 125,789,091-row prefix of the GBIF ladder corpus in
        // three batches of 50,331,648 items, the signature sort and the assignment together
        // measure 333 ns a record against 379 with the check inside the walk (median of five
        // paired runs). ⊘ Prefetching the walk's remaining gather, the ordinal-indexed write and
        // the `starts` read 24 records ahead, measured 35% slower on the same prefix's assignment
        // loop.
        if per_view_labels {
            let appearances = appearances.as_slice();
            for local in 0..batch_len {
                let ordinal = ordinal_lo as usize + local;
                let sig_len = (starts[local + 1] - starts[local]) as u64;
                if distinct_of_ordinal[ordinal] as u64 != sig_len * appearances[ordinal] as u64 {
                    return Err(BuildError::Invalid(format!(
                        "entity_id {} carries different access labels in different views. A label \
                         is the entity's, not the row's (views §7): it is one set wherever the \
                         entity appears, and a re-label is a delete plus a re-ingest (decision \
                         0047). The views this build reads are {}",
                        source_ids.ids().id_of(ordinal),
                        args.views
                            .iter()
                            .map(|v| format!("'{}' ({})", v.view_id, v.points.display()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            }
        }
        // The triple (key_hi, key_lo, ordinal) is unique per rec — a total order, so the
        // parallel unstable sort has exactly one output; refinement makes it the reference
        // `(signature, source_id)` order.
        recs.par_sort_unstable_by_key(|r| r.order());
        refine_signature_ties(&mut recs, &packed, &starts, ordinal_lo, &long_sig);
        drop(long_sig);
        timer.end(BuildStage::SignatureSort, recs.len() as u64);

        // **The batch's slice of the entity map, filled here and written once.** A batch's
        // ordinals are the contiguous range `[ordinal_lo, ordinal_hi)`, and the walk visits them
        // in signature order — so writing each entity straight into the mapping scattered the
        // writes over a 1.5 GB slice of a shared file mapping, and the kernel wrote a page back,
        // write-protected it, and took another fault on the next write to it. Measured at rung 6:
        // 200 to 380 MB/s of writes to grow the bundle at 20, 50,000 to 90,000 minor faults a
        // second, and an assignment stage that rose from 65 s to 233 s across identical batches
        // as the dirty set grew
        // (`docs/evidence/memos/2026-09-12-gbif-whole-corpus-build-observations.md` §1). The
        // window is
        // `batch_items * 4 ≤ n * 4`, which is what `plan_build`'s retained 4 B an item pays for.
        let mut assigned: Vec<u32> = vec![0; batch_len];
        // Assignment, and the band emit in the same walk: entities ascend with position, so
        // every term's entity list arrives ascending — within this batch here, and across
        // batches because bases ascend and the loop is sequential. `encode_posting`'s
        // unconditional sortedness check later re-verifies exactly this property from disk.
        for (position, rec) in recs.iter().enumerate() {
            let entity = (entity_base + position as u64) as u32;
            let local = (rec.ordinal as u64 - ordinal_lo) as usize;
            assigned[local] = entity;
            let sig = &packed[starts[local] as usize..starts[local + 1] as usize];
            sig_terms.clear();
            for &value in sig {
                let term = term_of(value);
                let band = band_los.partition_point(|&lo| lo <= term) - 1;
                sig_terms.push(term);
                band_writers[band].push(term, entity)?;
                term_counts[term as usize] =
                    term_counts[term as usize].checked_add(1).ok_or_else(|| {
                        BuildError::Invalid(format!(
                            "term {term} exceeds 2^32 postings, which the entity ceiling makes \
                             impossible for an unmutated input"
                        ))
                    })?;
            }
            entity_terms
                .push(entity, &sig_terms)
                .map_err(|e| BuildError::Invalid(format!("entity-terms transpose: {e}")))?;
        }
        entities[ordinal_lo as usize..ordinal_lo as usize + batch_len].copy_from_slice(&assigned);
        entity_base += recs.len() as u64;
        store.delete(k)?;
        timer.end(BuildStage::Assignment, recs.len() as u64);
    }
    if entity_base != n {
        return Err(input_changed(&format!(
            "batches assigned {entity_base} entities for {n} items"
        )));
    }
    // The assignment walk is the only writer, so the mapping is read-only from here on and the
    // passes below take it as the plain `&[u32]` they always did.
    let entity_of_ordinal = entity_map.as_slice();
    // The two ordinal-space counters of the label-agreement identity (`views.md` §7) have served
    // their only reader, the ordinal-order pass each batch runs ahead of its assignment walk, and
    // are released here rather than at the end of the build — across every stage from the postings write to the last segment. 4 B/item
    // each, both mapped files, so what this returns at the GBIF rung is 13.0 GiB of disk apiece
    // (modelled, items × 4 B). Each file is unlinked by `MappedArray`'s own `Drop`.
    drop(distinct_map);
    drop(appearances);
    // The anchor's Morton geometry has served its one reader — the sort's tiebreak — and is
    // released here rather than at the end of the build. At 10⁹ that is 8 GB of dirty mapped
    // pages returned before the band sweep and the postings write start competing for page
    // cache. Each view's own geometry stays: pass two is what reads it. Nothing to release where
    // the anchor covered the union and no array was written.
    drop(anchor_maps);
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

    let entity_terms_paths = entity_terms
        .finish()
        .map_err(|e| BuildError::Invalid(format!("entity-terms transpose: {e}")))?;
    for path in &entity_terms_paths {
        fsync_file(path)?;
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
    let writes_external_ids = crate::ids::writes_external_ids(args, &id_space);
    if writes_external_ids {
        // Sorted by the external id's *bytes* (R4); `ExternalIdRow` holds each id as the sort
        // key that makes that a plain integer comparison, in twelve bytes rather than a padded
        // sixteen.
        let mut external: Vec<ExternalIdRow> = (0..n as usize)
            .map(|ordinal| {
                ExternalIdRow::new(source_ids.ids().id_of(ordinal), entity_of_ordinal[ordinal])
            })
            .collect();
        // Keys are the byte-swapped source ids on the integer route and the ranks themselves on
        // the supplied one — dup-checked, hence unique: a total order, one output under the
        // parallel unstable sort (`crate::sort_external_id_rows` states which and why).
        match &id_space {
            crate::ids::IdSpace::Supplied(_) => {
                external.par_sort_unstable_by_key(ExternalIdRow::source_id)
            }
            _ => external.par_sort_unstable_by_key(ExternalIdRow::sort_key),
        }
        external_ids_paths = write_external_id_runs(
            &entities_dir,
            &external,
            EXTERNAL_ID_ROWS_PER_EXTENT,
            &id_space,
        )?;
        // `external` is still in the concatenated extent order at this point (the extents
        // partition it into consecutive ranges, in order) — its index *is* each row's ordinal,
        // which is exactly what the locator addresses (contracts §2.4/§2.6 r6).
        ext_locator_path = Some(write_ext_locator(&entities_dir, &external, n)?);
    }

    timer.end(
        BuildStage::ExternalIds,
        if writes_external_ids { n } else { 0 },
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
    let (attributes_by_entity, mut spilled, coverage) = read_attributes_by_entity(
        args,
        n,
        &plan.routes,
        source_ids.ids(),
        &id_space,
        entity_of_ordinal,
        &mut minters,
        &scratch,
        tmp.path(),
    )?;
    // **Printed here, where the join has just happened and the numbers are the join's own.** The
    // linear build reports the identical figures from its own pass, so the two builds agree about
    // coverage exactly as they agree about bytes.
    crate::report_attribute_coverage(&coverage);

    // ---- 8b. the group-scoped column families (`views.md` §5) --------------------------
    // Here, beside the attribute tail, because it wants exactly what the tail wants: `source_ids`
    // and `entity_of_ordinal`, both alive, and entity ids final under I9. One column per view of
    // the group, in entity space; nothing per row space.
    let (scoped_paths, scoped_render) = write_scoped_columns(
        args,
        &partition_dir,
        n,
        source_ids.ids(),
        &id_space,
        entity_of_ordinal,
        &mut minters,
        &scratch,
    )?;
    // Which of those columns each view's row space carries (`views.md` §5) — resolved once, here,
    // rather than per view inside the loop below, so the rule that decides it is stated in one
    // place and the loop is an index.
    let scoped_render_targets = scoped_render_targets(args, &scoped_render);

    timer.end(BuildStage::AttributeTail, n);

    // ---- 8c. layers and their artifacts ------------------------------------------------
    // Resolved here for the reason the attribute tail is: this is where what turns a source id
    // into the entity this build assigned it is still alive. A member is named by source id,
    // exactly as the pairs file's ids are.
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
        drop(source_ids);
        crate::layers::PublishedLayers::default()
    } else {
        let mut plan = crate::layers::read(
            &args.layers,
            &args.layer_inputs,
            &id_space,
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
        let derived = crate::layers::predicate_artifact_keys(
            &args.layers,
            &args.schema,
            &minters,
            &|index| distinct_codes(attributes_by_entity[index].iter()),
        )?;
        let prefix_dir = args.out.join(crate::PREFIX);
        // **A contiguous id range makes the search a subtraction**, and whether it is
        // contiguous is checked rather than assumed ([`Ids::contiguous_from`]): pass one either
        // proved the range without writing an array, or wrote one that is sorted and free of
        // duplicates, where a span equal to its own count can only be `first + i` at every `i`.
        // The fast path is the same answer, not a convention about how a caller numbers its rows.
        //
        // It is worth the branch because this closure runs **once per member entry**: a
        // lineage list per point at the Overture rung is 3×10⁸ of them, and a binary search
        // into 74M sorted `u64` is ~27 dependent cache misses where the subtraction is one.
        let ordinals = source_ids.len() as u64;
        let range = source_ids.ids().contiguous_from();
        if let Some(first) = range {
            // **Any array goes before the publication on this path, not after it.** The
            // subtraction reads the first id and the count and never an element, so the 8 B/item
            // would be dead weight beside what the publication does hold: the member tables it
            // reads whole, the artifact allocator, and the memberships that stay resident. That
            // is 1.74 GiB at rung 5's 2.33×10⁸ items and 26.4 GiB at the GBIF rung's 3.54×10⁹
            // (modelled, items × 8 B). Where pass one proved the range there is no array to
            // release, and this is the ordinary path either way: the ladder's corpora number
            // their rows from zero.
            //
            // Two closures rather than one with a branch inside: a single closure would capture
            // `source_ids` on both paths, and the borrow would hold the array across the call.
            drop(source_ids);
            crate::layers::publish(
                &mut plan,
                &|source| {
                    source
                        .checked_sub(first)
                        .filter(|ordinal| *ordinal < ordinals)
                        .map(|ordinal| entity_of_ordinal[ordinal as usize] as u64)
                },
                n,
                &prefix_dir,
                crate::PHASH,
                &view_ids,
                &derived,
            )?
        } else {
            let ids = source_ids.ids();
            let published = crate::layers::publish(
                &mut plan,
                &|source| {
                    ids.ordinal_of(source)
                        .map(|ordinal| entity_of_ordinal[ordinal as usize] as u64)
                },
                n,
                &prefix_dir,
                crate::PHASH,
                &view_ids,
                &derived,
            )?;
            drop(source_ids);
            published
        }
    };

    crate::write_containment_report(&args.out, &published_layers)?;

    timer.end(BuildStage::Layers, published_layers.layers.len() as u64);

    // ---- 8d. attribute filter postings (filter-index §4) -------------------------------
    // Its own stage, after entity assignment and before the tiler sort: entity ids are final
    // here (stage 5, permanent under I9) and the values have just been read, which are the two
    // things the emit needs. It cannot ride on `PostingsWrite` — that stage runs before the
    // attribute values exist.
    // The extents the join spilled, opened once for every reader: the record blob merges them,
    // and a `text` column's token index tokenises them in block windows first (`crate::extents`).
    // Folded first only where a column spilled more extents than the budget lets one merge hold
    // open, which no corpus built so far reaches.
    let fan_in = crate::extents::merge_fan_in(plan.budget);
    for column in spilled.iter_mut() {
        column.cascade(fan_in)?;
    }
    let open_extents: Vec<crate::extents::OpenExtents> = spilled
        .iter()
        .map(crate::extents::ExtentColumn::open)
        .collect::<Result<_>>()?;
    let (filter_paths, text_index) = write_filter_postings(
        &partition_dir,
        &args.schema,
        &attributes_by_entity,
        &open_extents,
        plan.budget,
    )?;
    // The text columns' share, charged out of the block rather than measured beside it — the two
    // interleave over one column loop, so a boundary in time cannot separate them.
    timer.charge(BuildStage::TextIndex, text_index.elapsed, text_index.terms);
    // **A column with no blob row is dead here, not two stages further on.** The blob reads the
    // columns whose values have no other home ([`blob_resident`]); the segment write reads the
    // render columns. A column that is neither — an `index = true` keyword, say — has just met its
    // last reader, and holding it to the release below costs the blob's whole stage. Measured on
    // 125,789,091 GBIF occurrences: `specieskey`'s offsets and arena are 3.15 GB against a 19.4 GB
    // peak that fell on the record blob (`probes/2026-09-10-build-disk/`).
    let mut attributes_by_entity = attributes_by_entity;
    for (column, attribute) in attributes_by_entity
        .iter_mut()
        .zip(args.schema.attributes.iter())
    {
        if !attribute.render && !blob_resident(&args.schema, attribute) {
            column.release();
        }
    }
    // **And the same line for a spilled column's extents.** An indexed keyword's extents are the
    // dictionary pass's alone, so they go back to the disk here rather than standing through the
    // record blob, which is the phase the measured peak falls on.
    let mut open_extents = open_extents;
    open_extents
        .retain(|extents| blob_resident(&args.schema, &args.schema.attributes[extents.column]));
    for column in spilled.iter_mut() {
        if !blob_resident(&args.schema, &args.schema.attributes[column.column]) {
            column.remove();
        }
    }
    timer.end(BuildStage::FilterPostings, n);

    // The record blob wants the same two things the postings did — entity ids final under I9, and
    // the attribute values in hand — so it runs here. Its files join the manifest digest at step 11
    // with everything else. **Its own stage**: it and the postings and the release below were one
    // number for three jobs, which is why the 615 s this block cost at 7.4×10⁷ points could be
    // modelled and not read.
    let record_paths = write_record_blob(
        &partition_dir,
        &args.schema,
        n,
        &plan.routes,
        &attributes_by_entity,
        &open_extents,
    )?;
    // Both readers are done, so the extents go back to the disk before the row spaces are
    // written. A build that failed above leaves them to `TmpDir::close`.
    drop(open_extents);
    for column in spilled.iter_mut() {
        column.remove();
    }
    timer.end(BuildStage::RecordBlob, n);
    // **Everything past here wants only the render columns**, and the pass that wanted the rest has
    // just run. The assembly reads a non-render column not at all (its home is entity space, and
    // giving it a slot in every row is the per-row cost §10.3's routing exists to avoid), so a
    // blob-resident column is dead from this line and would otherwise live to the end of the
    // segment write, straight through the row partition's 12 B a row and the columns file beside
    // it.
    //
    // A spilled column has nothing left to release — its characters went out as extents and the
    // extents are unlinked above. What is left is the blob-resident columns, the postings pass
    // having already released the rest; `release` is idempotent, so the loop states the whole rule
    // rather than the half of it this line reaches.
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
    // **One counter for the whole prefix.** The artifact pass runs per view and each of its calls
    // names files from this, so the second view's files cannot be named after the first's — see
    // `tessera_store::derived::DerivedIndex`.
    let mut derived_index = tessera_store::derived::DerivedIndex::default();
    for (index, view) in args.views.iter().enumerate() {
        let view_dir = tessera_store::view_path(&partition_dir, &view.view_id);
        let segment_dir = view_dir.join("segments").join(SEG_ID);
        std::fs::create_dir_all(&segment_dir).map_err(|e| BuildError::io(&segment_dir, e))?;

        // ---- the assembly: the view's rows, Morton-partitioned, straight from ordinal order ----
        //
        // **No entity-order geometry.** What stood here permuted `x` and `y` into two
        // entity-indexed files at a scattered index, sorted a 12 B record per row of the view on
        // the heap, and built the row→entity, residual and `tessera_id` columns as three more
        // vectors beside it. At rung 6 that was a 70 GB peak the residency model had no term for.
        // `crate::assembly` is the same rows in the same order with one bucket held at a time.
        let (partitioned, boundaries, page_plan) = {
            let source = crate::assembly::RowSource {
                view_dir: &view_dir,
                view: &view.view_id,
                tmp: tmp.path(),
                x: geometry[index].x.as_slice(),
                y: geometry[index].y.as_slice(),
                present: &geometry[index].present,
                entity_of_ordinal,
                n,
                identity_key: &args.identity_key,
                shard_id: args.shard_id,
                rows_hint: geometry[index].rows,
            };
            crate::assembly::partition_rows(&source)?
        };
        if partitioned.rows() != geometry[index].rows {
            return Err(input_changed(&format!(
                "view '{}': {} rows in the segment for the {} its points file selected",
                view.view_id,
                partitioned.rows(),
                geometry[index].rows
            )));
        }
        // **This view's ordinal-space geometry has served its only reader.** The row partition
        // carries each row's residual, so nothing after this reads a coordinate — 8 B/item per
        // view, released before the segment write rather than after the artifact pass.
        geometry[index].x = spill::MappedU32::empty();
        geometry[index].y = spill::MappedU32::empty();
        timer.end(BuildStage::TilerSort, partitioned.rows());

        let assembled = {
            // The declared render columns in schema order, then this view's scoped ones — the
            // order `columns.arrow` carries them in, and the order every reader that resolves a
            // tail column by name is indifferent to but the bytes are not.
            let mut render: Vec<crate::assembly::RenderColumn<'_>> = Vec::new();
            for (attribute, values) in args
                .schema
                .attributes
                .iter()
                .zip(attributes_by_entity.iter())
            {
                if attribute.render {
                    render.push(crate::assembly::RenderColumn {
                        name: attribute.name.clone(),
                        ty: attribute.ty,
                        values,
                    });
                }
            }
            for &column in &scoped_render_targets[index] {
                render.push(crate::assembly::RenderColumn {
                    name: scoped_render[column].name.clone(),
                    ty: scoped_render[column].ty,
                    values: &scoped_render[column].values,
                });
            }
            let job = crate::assembly::Assembly {
                view_dir: &view_dir,
                view: &view.view_id,
                segment_dir: &segment_dir,
                tmp: tmp.path(),
                n,
                identity_key: &args.identity_key,
                shard_id: args.shard_id,
                render,
            };
            crate::assembly::write_segment(&job, partitioned, &boundaries, page_plan)?
        };
        eprintln!(
            "  view '{}': {} rows, largest Morton bucket {} rows over {} buckets",
            view.view_id,
            assembled.rows_in_view,
            assembled.largest_bucket,
            boundaries.buckets()
        );
        let rows_in_view = assembled.rows_in_view;
        occupancies.push(assembled.occupancy);
        let presence_paths = assembled.presence_paths;
        let morton_path = assembled.morton_path;
        let cuts_path = assembled.cuts_path;
        let row_entity_path = assembled.row_entity_path;
        let columns_path = assembled.columns_path;
        let permutation_path = assembled.permutation_path;
        fsync_file(&permutation_path)?;
        fsync_file(&row_entity_path)?;
        fsync_file(&columns_path)?;
        fsync_file(&morton_path)?;
        fsync_file(&cuts_path)?;
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
            tmp.path(),
            &mut derived_index,
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
            .shape_rows_extents
            .extend(artifact_pass.shape_rows_extents.iter().cloned());
        published_layers
            .shape_held_extents
            .extend(artifact_pass.shape_held_extents.iter().cloned());
        artifact_paths.extend(artifact_pass.paths.iter().cloned());
        timer.end(
            BuildStage::ArtifactPass,
            (artifact_pass.tile_index_extents.len()
                + artifact_pass.row_column_extents.len()
                + artifact_pass.shape_rows_extents.len()
                + artifact_pass.shape_held_extents.len()) as u64,
        );

        view_files.push(permutation_path);
        view_files.push(row_entity_path);
        view_files.push(columns_path);
        view_files.push(morton_path);
        view_files.push(cuts_path);
        view_files.extend(presence_paths);
        segments.push(tessera_store::manifest::SegmentDescriptor {
            view: view.view_id.clone(),
            // **The declared incarnation** (decision 0115): a build coins each key once.
            incarnation: tessera_store::manifest::DECLARED_INCARNATION,
            seg_id: SEG_ID.to_string(),
            row_count: rows_in_view,
            entity_lo: 0,
            entity_hi: n,
        });
    }
    // ---- 10c. the containment partitions, once for the prefix ----------------------------
    //
    // **Outside the view loop**, because a partition is a function of the level's records and the
    // prefix's postings and carries no view. Composing it inside the pass would write one identical
    // file and one manifest entry per view.
    {
        let artifact_store = std::mem::take(&mut published_layers.store);
        let containment = crate::artifact_pass::containment(
            &artifact_store,
            &args.out.join(crate::PREFIX),
            crate::PHASH,
            &plugin.data_plugin_hash(),
            &mut derived_index,
        );
        published_layers.store = artifact_store;
        eprintln!("  wrote {} containment partition(s)", containment.len());
        artifact_paths.extend(
            containment
                .iter()
                .map(|entry| args.out.join(crate::PREFIX).join(&entry.path)),
        );
        let composed = containment.len() as u64;
        published_layers.containment_extents.extend(containment);
        timer.end(BuildStage::ArtifactPass, composed);
    }

    drop(geometry);
    drop(entity_map);
    // The spill directory closes **here**: every view's ordinal-space geometry and every
    // entity-space scatter are dropped by this point, so the tree is unbusy.
    tmp.close()?;

    // ---- 11. manifests ---------------------------------------------------------------
    let mut other_paths = vec![postings_path];
    other_paths.extend(entity_terms_paths);
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
    report.spilled_columns = crate::spilled_column_names(&args.schema, &plan.routes);
    report.source_id_slots = id_shape.slots;
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
#[allow(clippy::too_many_arguments)]
fn read_attributes_by_entity(
    args: &BuildArgs,
    n: u64,
    routes: &ColumnRoutes,
    ids: Ids<'_>,
    id_space: &crate::ids::IdSpace,
    entity_of_ordinal: &[u32],
    minters: &mut std::collections::HashMap<String, tessera_store::vocabulary::VocabularyMinter>,
    scratch: &crate::column::ColumnScratch,
    tmp: &Path,
) -> Result<(
    Vec<EntityColumn>,
    Vec<crate::extents::ExtentColumn>,
    Vec<crate::AttributeCoverage>,
)> {
    if args.schema.is_empty() {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }
    let attributes = &args.schema.attributes;
    // One typed column per attribute, indexed by entity — see [`EntityColumn`] for why this is not
    // the `ScalarValue` vector it reads like, and what that costs at 10⁸ items and above.
    //
    // **A spilled column carries no values here.** Its characters are spilled as record-blob
    // extents by the sweep below and read from them by the record blob, and by the token index too
    // where the column is `text` ([`ColumnRoutes`], [`crate::extents`]). What is kept here is the
    // length, so every later pass can go on indexing this vector by an attribute's declaration
    // position.
    let mut by_entity: Vec<EntityColumn> = attributes
        .iter()
        .enumerate()
        .map(|(i, a)| match routes.takes_extents(i) {
            true => EntityColumn::spilled(scratch, a.ty, n as usize),
            false => EntityColumn::filled(scratch, a.ty, n as usize),
        })
        .collect::<Result<_>>()?;
    let mut spilled: Vec<crate::extents::ExtentColumn> = attributes
        .iter()
        .enumerate()
        .filter(|&(i, _)| routes.takes_extents(i))
        .map(|(i, a)| crate::extents::ExtentColumn::new(tmp, i, &a.name))
        .collect();
    // **A partition per entity-order column** ([`ValueLane`]), carrying the value for a
    // fixed-width column and the `at` word for a string column on the arena route. Nothing the
    // join produces is written at a scattered index: the arena beside a string column is appended
    // in arrival order, and the word that says where a value went is replayed into `at` as a run
    // with every other word of its entity range.
    let mut value_lanes: Vec<Option<ValueLane>> = by_entity
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let width = match (column.fixed_width(), column.is_arena_strings()) {
                (Some(width), _) => width,
                // The `at` word, which is what a string column holds at an entity.
                (None, true) => 8,
                (None, false) => return Ok(None),
            };
            let boundaries = value_lane_boundaries(n);
            Ok(Some(ValueLane {
                column: index,
                width,
                chars: column.is_arena_strings(),
                partition: spill::Partition::create(
                    tmp,
                    &format!("value-{index}"),
                    boundaries.clone(),
                    4 + width,
                    n,
                )?,
                boundaries,
            }))
        })
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
            ids,
            id_space,
            entity_of_ordinal,
            minters,
            scratch,
            &mut by_entity,
            &mut spilled,
            &mut value_lanes,
            &mut coverage,
        )?;
    }
    // **The replay, once every source has been read.** A bucket at a time, in ascending entity
    // order: the records are scattered into a window over the bucket's own range and the window
    // is written into the column as one sequential run, with its presence bits beside it. A string
    // column's run is its `at` words, the arena behind them already written.
    for lane in value_lanes.into_iter().flatten() {
        let name = &attributes[lane.column].name;
        let record_width = 4 + lane.width;
        let mut store = lane.partition.finish()?;
        for (k, &lo) in lane.boundaries.iter().enumerate() {
            let hi = lane
                .boundaries
                .get(k + 1)
                .map(|&next| next as u64)
                .unwrap_or(n)
                .min(n);
            let span = (hi.saturating_sub(lo as u64)) as usize;
            let mut window = vec![0u8; span * lane.width];
            let mut present = vec![0u64; span.div_ceil(64)];
            for record in store.load(k)?.chunks_exact(record_width) {
                let entity =
                    u32::from_le_bytes(record[..4].try_into().expect("a record carries its key"));
                let at = (entity as u64)
                    .checked_sub(lo as u64)
                    .filter(|&at| at < span as u64);
                let Some(at) = at.map(|at| at as usize) else {
                    return Err(BuildError::Invalid(format!(
                        "attribute '{name}': bucket {k} of its value partition holds entity \
                         {entity}, outside the range [{lo}, {hi}) it addresses"
                    )));
                };
                window[at * lane.width..(at + 1) * lane.width].copy_from_slice(&record[4..]);
                present[at / 64] |= 1u64 << (at % 64);
            }
            store.delete(k)?;
            by_entity[lane.column].write_value_run(lo as usize, &window, &present, name)?;
        }
    }
    Ok((by_entity, spilled, coverage))
}

/// What one column's share of a resolved chunk is: a record pushed into its value partition, or,
/// for a column that has none ([`takes_extents`]), an extent written in the chunk's entity order.
/// Nothing here writes at a scattered entity: a value goes to the partition and is replayed into
/// the column as a run, and a string's characters are appended to the arena in arrival order.
///
/// The lanes share nothing. Each entity-order column has its own partition, each string column its
/// own arena, and each spilled column its own extent writer, so a chunk's work splits across them
/// with no synchronisation: `resolved` is read-only and every write a lane makes lands in storage
/// no other lane can name.
enum JoinLane<'a> {
    Partitioned {
        src: &'a mut EntityColumn,
        lane: &'a mut ValueLane,
    },
    /// A string column on the arena route: the characters go into `home`'s arena in arrival order
    /// and the word they landed at goes to the lane, where a fixed-width column's value goes.
    Chars {
        src: &'a mut EntityColumn,
        home: &'a mut EntityColumn,
        lane: &'a mut ValueLane,
    },
    Extent {
        src: &'a mut EntityColumn,
        out: &'a mut crate::extents::ExtentColumn,
    },
}

/// One fixed-width column's `(entity, value)` partition, and the entity ranges its buckets
/// address.
///
/// **Why a partition and not the scatter it replaces.** The join reads its source in the source's
/// own order and each value's home is its entity, which is signature-then-Morton order — so
/// writing each value where it belongs wrote the column's pages back and re-dirtied them many
/// times over: 123 GB written to grow the bundle by 34 in one stage at rung 6, over value columns
/// of 10.5 GB (`docs/evidence/memos/2026-09-12-gbif-whole-corpus-build-observations.md` §4). The
/// values go to buckets by entity range instead, and each bucket is replayed into a window and
/// written into the column as one sequential run.
///
/// **Replayed in append order, which is the order today's scatter writes in**: sweep order within
/// a chunk, chunk order across chunks, source order across sources. So which of two rows carrying
/// one entity wins is unchanged.
struct ValueLane {
    column: usize,
    /// What one record's payload is wide: the column's own width, or the eight bytes of a string
    /// column's `at` word.
    width: usize,
    /// **A string column on the arena route**, whose payload is the word and whose characters went
    /// into the arena as the join read them. The replay is the same one either way; what differs is
    /// where the join takes the payload from.
    chars: bool,
    /// The first entity of each bucket, ascending from 0 and **a multiple of 64** — so a bucket's
    /// presence bits are whole words of the column's own bitmap.
    boundaries: Vec<u32>,
    partition: spill::Partition,
}

/// The entity boundaries a value lane's buckets take: [`spill::boundaries_uniform`] rounded down
/// to presence-word boundaries, so a bucket's bits are whole words.
fn value_lane_boundaries(n: u64) -> Vec<u32> {
    spill::boundaries_uniform(n)
        .into_iter()
        .map(|first| first & !63)
        .collect::<std::collections::BTreeSet<u32>>()
        .into_iter()
        .collect()
}

/// Write one chunk's values as an extent, and return how many of its rows carried a value.
///
/// `resolved` ascends in the entity and its sort is stable, so an entity written twice within the
/// chunk keeps the last row that carried a value — the same answer the scatter reaches by writing
/// them in order, and the same one an absent later row leaves alone. Counted per resolved row
/// rather than per entity, which is what the coverage report says.
fn spill_extent_chunk(
    src: &EntityColumn,
    resolved: &[(u32, u32)],
    out: &mut crate::extents::ExtentColumn,
) -> Result<u64> {
    let mut rows: Vec<(u32, u32)> = Vec::new();
    let mut count = 0u64;
    for &(entity, pos) in resolved {
        if !src.is_present(pos as usize) {
            continue;
        }
        count += 1;
        if rows.last().map(|&(held, _)| held) == Some(entity) {
            rows.pop();
        }
        rows.push((entity, pos));
    }
    let values: Vec<(u32, &str)> = rows
        .iter()
        .map(|&(entity, pos)| {
            (
                entity,
                src.str_at(pos as usize)
                    .expect("the row was tested for presence"),
            )
        })
        .collect();
    out.push_extent(&values)?;
    Ok(count)
}

/// One attribute source's merge sweep into the entity-major columns.
#[allow(clippy::too_many_arguments)]
fn read_one_attribute_source(
    args: &BuildArgs,
    group: &crate::config::AttributeSource,
    n: u64,
    ids: Ids<'_>,
    id_space: &crate::ids::IdSpace,
    entity_of_ordinal: &[u32],
    minters: &mut std::collections::HashMap<String, tessera_store::vocabulary::VocabularyMinter>,
    scratch: &crate::column::ColumnScratch,
    by_entity: &mut [EntityColumn],
    spilled: &mut [crate::extents::ExtentColumn],
    value_lanes: &mut [Option<ValueLane>],
    coverage: &mut Vec<crate::AttributeCoverage>,
) -> Result<()> {
    let filled: Vec<usize> = group.attributes.clone();
    let columns: Vec<&crate::config::Attribute> =
        filled.iter().map(|&i| &args.schema.attributes[i]).collect();
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
                 spilled: &mut [crate::extents::ExtentColumn],
                 value_lanes: &mut [Option<ValueLane>],
                 matched: &mut u64,
                 unknown: &mut u64,
                 present: &mut [u64]|
     -> Result<()> {
        resolved.clear();
        join_chunk(chunk, ids, |ordinal, _source_id, pos| {
            match ordinal {
                None => *unknown += 1,
                Some(ordinal) => {
                    *matched += 1;
                    resolved.push((entity_of_ordinal[ordinal as usize], pos));
                }
            }
            Ok(())
        })?;
        // **Ascending in the entity, so every lane's scatter is a forward sweep.** The sweep's own
        // answer arrives in source-id order, and entity ids are signature-then-Morton order, so
        // without this each lane writes its column at a uniformly random index — free while the
        // column fits in the page cache and not free otherwise. Measured on the 10⁷ MedCPT sample
        // under `MemoryMax=4G`, before this sort was added: **347,009 major faults and 480 GB read
        // for a 10.4 GiB arena, 1,335 s into a stage that costs 68 s once the scatter ascends and
        // had not finished** (`probes/2026-09-03-entity-ordered-arena/`).
        //
        // **Stable**, so which of two rows carrying one entity is written last agrees with
        // `join_chunk`'s own answer to the same question (§7's last-write-wins).
        resolved.par_sort_by_key(|&(entity, _)| entity);
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
            let mut spills: Vec<Option<&mut crate::extents::ExtentColumn>> =
                (0..homes.len()).map(|_| None).collect();
            for column in spilled.iter_mut() {
                let at = column.column;
                spills[at] = Some(column);
            }
            let mut partitions: Vec<Option<&mut ValueLane>> =
                (0..homes.len()).map(|_| None).collect();
            for lane in value_lanes.iter_mut().flatten() {
                let at = lane.column;
                partitions[at] = Some(lane);
            }
            let mut lanes: Vec<JoinLane<'_>> = filled
                .iter()
                .zip(staged.iter_mut())
                .map(|(&column, src)| match spills[column].take() {
                    Some(out) => JoinLane::Extent { src, out },
                    None => {
                        // Every entity-order column has a value lane: a fixed-width one carries
                        // its value and a string one the `at` word, and a column with neither is
                        // spilled and was answered above.
                        let lane = partitions[column].take().expect(
                            "an entity-order column has a value lane, a spilled one an extent \
                             writer, and no column has both",
                        );
                        match lane.chars {
                            true => {
                                let home = homes[column].take().expect(
                                    "an attribute is read from exactly one source, so one lane \
                                     owns it",
                                );
                                JoinLane::Chars { src, home, lane }
                            }
                            false => JoinLane::Partitioned { src, lane },
                        }
                    }
                })
                .collect();
            // Collected per lane and folded in lane order, so a build that fails here fails with
            // the same column's message every time: which lane finished first is a scheduling
            // detail, and a build error that moves with it cannot be reproduced from its report.
            let counted: Vec<Result<u64>> = lanes
                .par_iter_mut()
                .map(|lane| match lane {
                    JoinLane::Partitioned { src, lane } => {
                        let mut count = 0u64;
                        let mut record = vec![0u8; 4 + lane.width];
                        for &(entity, pos) in resolved.iter() {
                            // **An absent row pushes nothing**, as the scatter this replaces
                            // wrote nothing for one: the column was filled absent before a row
                            // was read, and absence is the state it keeps.
                            let Some(value) = src.raw_at(pos as usize) else {
                                continue;
                            };
                            count += 1;
                            record[..4].copy_from_slice(&entity.to_le_bytes());
                            record[4..].copy_from_slice(value);
                            lane.partition.push(&record)?;
                        }
                        Ok(count)
                    }
                    JoinLane::Chars { src, home, lane } => {
                        let name = &args.schema.attributes[lane.column].name;
                        let mut count = 0u64;
                        let mut record = [0u8; 12];
                        for &(entity, pos) in resolved.iter() {
                            // **An absent row appends nothing and pushes nothing**, as the scatter
                            // this replaces wrote nothing for one.
                            let Some(word) =
                                home.take_chars_from(entity as usize, src, pos as usize, name)?
                            else {
                                continue;
                            };
                            count += 1;
                            record[..4].copy_from_slice(&entity.to_le_bytes());
                            record[4..].copy_from_slice(&word.to_le_bytes());
                            lane.partition.push(&record)?;
                        }
                        Ok(count)
                    }
                    JoinLane::Extent { src, out } => spill_extent_chunk(src, resolved, out),
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
        id_space,
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
                    spilled,
                    value_lanes,
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
            spilled,
            value_lanes,
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
/// against — is where the declaration is recorded. Every family is a filter operand from there,
/// one column per view, resolved by the request's view or by a pinned leaf.
///
/// **What each family owes per view is what its entity-scoped counterpart owes bundle-wide**: a
/// numeric or keyword column owes values, presence and — for a keyword — its dictionary; a
/// **category** owes those and the keyed per-value postings a filter and `/v1/categories`' value
/// list are both answered from; a **text** column owes no value column at all and owes instead a
/// token dictionary and the positional postings over it. One writer per family, the same one the
/// entity-scoped pass calls, pointed at this view's directory.
///
/// **`render = true` is carried into the hot tail of each view of the group** (`views.md` §5,
/// built 2026-08-31), and of any group sharing those views via `members`. The values are entity
/// space like every other column here; what the scope decides is *which* row spaces they are
/// permuted into, which is the rule `per-point-attributes.md` §3.9 gives `render_in` with the view
/// set derived from the scope instead of listed. So a render family's columns are **retained** rather than
/// written and dropped — returned to pass two, exactly as the entity-scoped render columns are
/// held across it — and every other family's are released with the file they wrote.
#[allow(clippy::too_many_arguments)]
fn write_scoped_columns(
    args: &BuildArgs,
    partition_dir: &Path,
    n: u64,
    ids: Ids<'_>,
    id_space: &crate::ids::IdSpace,
    entity_of_ordinal: &[u32],
    minters: &mut std::collections::HashMap<String, tessera_store::vocabulary::VocabularyMinter>,
    scratch: &crate::column::ColumnScratch,
) -> Result<(Vec<PathBuf>, Vec<ScopedRenderColumn>)> {
    let mut paths = Vec::new();
    let mut render_columns: Vec<ScopedRenderColumn> = Vec::new();
    // The text and keyword passes' budgets, derived once for the build rather than per column,
    // exactly as the entity-scoped pass derives them: a plan bounds the transient one column's
    // pass holds, and both are a function of the machine rather than of which column is indexed.
    let text_plan =
        TextIndexPlan::for_budget(args.memory_budget.unwrap_or_else(detect_memory_budget));
    let keyword_plan =
        KeywordDictPlan::for_budget(args.memory_budget.unwrap_or_else(detect_memory_budget));
    for family in &args.scoped_attributes {
        let attribute = &family.attribute;
        // **What `render` buys, and the one place it still does not reach.** The build's own
        // views of the group get the column in their row tails below, and a view created while
        // the service runs gets one at the first flush that covers it (`views.md` §5, r24) — so
        // the rebuild this once warned about is no longer owed. What is left is the views of a
        // group that only *shares* this family's: they render the column and no batch into one
        // may carry a value, the column being the owner's. Printed once per family, where an
        // operator can act on it.
        if attribute.render
            && args
                .groups
                .iter()
                .any(|g| g.members_of.as_deref() == Some(family.group.as_str()))
        {
            eprintln!(
                "attribute '{}': `render` is carried in the hot row tail of every view of group \
                 '{}' and of every group sharing its views; ⊘ a batch into a sharing group's view \
                 may not carry a value — send it to '{}'s own view of the key, where it is entity \
                 space and reaches both (views §5)",
                attribute.name, family.group, family.group
            );
        }
        for &index in &family.views {
            let view = &args.views[index];
            let column = read_scoped_column(
                args,
                family,
                view,
                n,
                ids,
                id_space,
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
            // **A text column has no value column**, per view exactly as bundle-wide: its
            // entity-space artefacts are the token dictionary and the postings over it, and there
            // is no per-entity slot for a scan to read. It leaves before the value column below is
            // written rather than writing one nothing opens.
            //
            // ⊘ Its prose has **no blob row per view**, which is the one thing the entity-scoped
            // family has that this one does not: the record blob is bundle-wide and addressed by a
            // column's position in `declared_scalars`, which a family has none of. So a scoped
            // text column answers `match` and is returned by no drill-down — the same restriction
            // `render` has here, and for the same reason.
            if attribute.ty == ScalarType::Text {
                // **And it therefore writes no row lane**, which is only sound because `render`
                // on `text` is refused at the declaration (`config::compile_attributes`'s
                // "`render` on `text` is refused" arm). A `BuildArgs` assembled programmatically
                // could still carry one, and it would publish `render: true` on `/v1/meta` while
                // no view's tail ever held the column — the one shape serving reads as ordinary
                // absence and could not tell from a build that failed. Fenced here rather than
                // trusted from the parser.
                debug_assert!(
                    !attribute.render,
                    "attribute '{}': `render` on a scoped `text` family — refused at the \
                     declaration, and this pass writes no row lane for it (views §5)",
                    attribute.name
                );
                // `index = false` is refused at the declaration for exactly this reason — with no
                // blob row and no index the prose would have no home at all — so the guard here is
                // against a `Schema` built programmatically rather than parsed.
                if attribute.index {
                    let written = write_text_index(
                        &column_dir,
                        attribute,
                        TextValues::Arena(&column.values),
                        text_plan,
                    )?;
                    paths.extend(written.paths);
                }
                report_scoped_coverage(attribute, view, column.present, n);
                continue;
            }
            let values_path = column_dir.join("values.arrow");
            let presence_path = column_dir.join("presence.roaring");
            let written = write_column_values(
                &column_dir,
                &values_path,
                &presence_path,
                attribute,
                &column.values,
                // **A group-scoped family is not routed** ([`may_take_extents`]): its columns are
                // this pass's own and there is no blob row for an extent to land in, so the
                // characters are always the column's.
                None,
                keyword_plan,
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
            // **A category's postings, per view** — the same keyed file the entity-scoped pass
            // writes, in this view's own directory, and owed on the same predicate: an indexed
            // category is answered from them, and a `derived` vocabulary's membership is derived
            // from them whatever `index` says (`filter-index.md` §2.3). Written after the values,
            // which are the artefact of record, so a build interrupted between the two leaves the
            // record without its accelerator rather than the reverse.
            if scoped_postings_are_owed(attribute) {
                let path = column_dir.join("postings.arrow");
                write_category_postings(
                    &path,
                    &attribute.name,
                    &column.values,
                    POSTINGS_BAND_ROWS,
                )?;
                fsync_file(&path)?;
                paths.push(path);
            }
            report_scoped_coverage(attribute, view, column.present, n);
            // **Retained for pass two, and only for a render family.** Everything else is on disc
            // and the values are dead — releasing here is what the entity-scoped pass does to its
            // own non-render columns before the row spaces are written.
            match attribute.render {
                true => render_columns.push(ScopedRenderColumn {
                    name: attribute.name.clone(),
                    ty: attribute.ty,
                    group: family.group.clone(),
                    key: key_of(&view.view_id).to_string(),
                    values: column.values,
                }),
                false => drop(column.values),
            }
        }
    }
    Ok((paths, render_columns))
}

/// One view's column of a **rendered** group-scoped attribute, held from the pass that read it
/// until the row space that renders it is written (`views.md` §5).
///
/// Addressed by `(group, key)` rather than by a view index because the row spaces it reaches are
/// not only the owning group's: a group declaring `members` of it shares the same keys under its
/// own view ids, and its views render the family too.
struct ScopedRenderColumn {
    name: String,
    ty: ScalarType,
    group: String,
    key: String,
    values: EntityColumn,
}

/// Which retained scoped render columns each view's row space carries — one entry per
/// [`BuildArgs::views`], holding indices into `columns` (`views.md` §5).
///
/// **The view set is the scope's**, which is the whole of what `render` on a scoped attribute
/// means: a view of the family's own group renders it, so does a view of a group declaring
/// `members` of that group — the keys being the owner's by construction (`views.md` §3.3) — and
/// no other view gets a slot for it at all.
///
/// **This is `viewport::owning_key_of`'s rule, asked of the build's own arguments**, and the two
/// must agree: a column written into a row space no request reads it from is a silent nothing, and
/// one a request reads from a row space no build wrote is served as absence. It is stated twice
/// rather than shared because the serving side's inputs are a manifest and this side's are
/// `BuildArgs`, in a crate that does not depend on the engine. What keeps the one case where they
/// could differ unreachable is a **declaration refusal**: `scope` naming a group that declares
/// `members` is refused at parse (`config::compile_scope`), so a family is always the owner
/// group's and `members_of` is only ever followed in one direction.
fn scoped_render_targets(args: &BuildArgs, columns: &[ScopedRenderColumn]) -> Vec<Vec<usize>> {
    args.views
        .iter()
        .map(|view| {
            let Some((group, key)) = view.view_id.split_once(tessera_store::GROUP_SEPARATOR) else {
                // A plain view is in no group, so no scope reaches it.
                return Vec::new();
            };
            let owner = args
                .groups
                .iter()
                .find(|g| g.name == group)
                .and_then(|g| g.members_of.as_deref())
                .unwrap_or(group);
            columns
                .iter()
                .enumerate()
                .filter(|(_, column)| column.group == owner && column.key == key)
                .map(|(index, _)| index)
                .collect()
        })
        .collect()
}

/// Does this view's column of a scoped **category** family owe its keyed postings?
///
/// [`postings_are_owed`]'s question, asked of a family: the postings are what an `eq` or an `in`
/// is answered from on a `public` vocabulary, and what `/v1/categories` derives value visibility
/// from on a `derived` one. Non-categories owe none — a number's values are near-unique and a
/// keyword's are a dictionary, so for either a posting per value is a second copy of the column
/// (this module's [`write_filter_postings`]).
///
/// **The family's filter admission is the whole condition** — `index` or `render`, the engine's
/// `filter::scoped_is_filterable` (2026-08-31) — where the entity-scoped rule is `index` *or* a
/// `derived` vocabulary. The difference is that a scoped family's admission decides both surfaces
/// at once: a family on no filter surface has no `/v1/categories` answer either, so postings
/// written for one would be read by nothing. It must agree with the engine's
/// `filter::scoped_owes_postings`, or the open demands a file no pass wrote.
///
/// **So it is one predicate, called from both** — `manifest::ScopedScalar::licence_of`, over the
/// four facts a declaration and a manifest record both carry. It lives at the record rather than
/// in the engine because `check-layers.sh` denies this crate the engine. The two used to agree by
/// argument: this pass spelled the licence `index || render` and the engine spelled it with the
/// `text` arm the declaration refuses anyway, and either could have been edited alone.
fn scoped_postings_are_owed(attribute: &crate::config::Attribute) -> bool {
    attribute.vocabulary.is_some()
        && tessera_store::manifest::ScopedScalar::licence_of(
            attribute.ty,
            attribute.vocabulary.is_some(),
            attribute.index,
            attribute.render,
        )
}

/// Printed per column of the family, where an entity-scoped column's coverage is printed: a scoped
/// column covers the view's own rows, so *fewer than the corpus* is its ordinary state rather than
/// a symptom.
fn report_scoped_coverage(
    attribute: &crate::config::Attribute,
    view: &crate::ViewArgs,
    present: u64,
    n: u64,
) {
    eprintln!(
        "attribute '{}' in view '{}': {} of {} entities have a value",
        attribute.name,
        view.view_id,
        crate::thousands(present),
        crate::thousands(n)
    );
}

/// The key half of a view id — `2026-Q3` of `quarter:2026-Q3`.
///
/// A view of a group always carries the joined form, so the split always finds a separator; a
/// plain view's id is returned whole, which is the answer that names nothing in a group's roster
/// and is therefore selected by no row.
fn key_of(view_id: &str) -> &str {
    view_id
        .split_once(tessera_store::GROUP_SEPARATOR)
        .map_or(view_id, |(_, key)| key)
}

/// One view's column of a group-scoped attribute, in entity space ([`write_scoped_columns`]).
struct ScopedColumn {
    values: EntityColumn,
    present: u64,
}

/// Read one view's values of a group-scoped attribute — out of that view's points file, or out of
/// the attribute's own source under that view's key.
///
/// **Two files, one selector.** Where the family declares no source of its own the view's points
/// file is the file, under the view's own selection where a form B group shares one. Where it
/// declares one, that file carries one row per `(entity, view)` and this view's rows are the ones
/// whose `fields.view` discriminator is this view's key — the same [`ViewSelector`] a form B
/// roster and a scoped layer are read through, so a value naming a key the roster does not carry
/// is the refusal that names both, and a view with no rows in the file simply has no values.
///
/// The same resolution every other attribute pass makes — `source_ids` → ordinal →
/// `entity_of_ordinal` — because entity ids are assigned in signature-sorted order (§11.1) and a
/// source id is not its own entity id. A row naming an entity this build did not load is counted
/// nowhere and refused nowhere: it is the join's ordinary case, exactly as it is for an
/// entity-scoped source.
#[allow(clippy::too_many_arguments)]
fn read_scoped_column(
    args: &BuildArgs,
    family: &crate::ScopedColumnFamily,
    view: &crate::ViewArgs,
    n: u64,
    ids: Ids<'_>,
    id_space: &crate::ids::IdSpace,
    entity_of_ordinal: &[u32],
    minters: &mut std::collections::HashMap<String, tessera_store::vocabulary::VocabularyMinter>,
    scratch: &crate::column::ColumnScratch,
) -> Result<ScopedColumn> {
    let attribute = &family.attribute;
    // Which file, and which of its rows. The keys a stray discriminator is refused against are the
    // family's own views' — the group's roster, by construction — derived here rather than carried
    // beside the source, so the two cannot come to disagree.
    let own_source = family.source.as_ref().map(|source| {
        let mut keys: Vec<String> = family
            .views
            .iter()
            .map(|&index| key_of(&args.views[index].view_id).to_string())
            .collect();
        keys.sort();
        keys.dedup();
        let select = crate::config::ViewSelector {
            column: source.view_field.clone(),
            value: key_of(&view.view_id).to_string(),
            keys,
            view_id: view.view_id.clone(),
        };
        let fields = crate::config::Fields::moved(
            format!("attribute '{}'", attribute.name),
            [(
                crate::config::ENTITY_ID.to_string(),
                source.entity_id.clone(),
            )],
        );
        (source.path.clone(), fields, select)
    });
    let (points, point_fields, select) = match &own_source {
        Some((path, fields, select)) => (path, fields, Some(select)),
        None => (&view.points, &view.point_fields, view.select.as_ref()),
    };
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
        join_chunk(chunk, ids, |ordinal, _source_id, pos| {
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
        points,
        point_fields,
        &columns,
        minters,
        args.limit,
        select,
        id_space,
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
    spilled: &[crate::extents::OpenExtents],
    memory_budget: u64,
) -> Result<(Vec<PathBuf>, TextIndexCost)> {
    write_filter_postings_banded(
        partition_dir,
        schema,
        by_entity,
        spilled,
        POSTINGS_BAND_ROWS,
        TextIndexPlan::for_budget(memory_budget),
        KeywordDictPlan::for_budget(memory_budget),
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
    spilled: &[crate::extents::OpenExtents],
    band_rows: usize,
    text_plan: TextIndexPlan,
    keyword_plan: KeywordDictPlan,
) -> Result<(Vec<PathBuf>, TextIndexCost)> {
    let mut paths = Vec::new();
    let mut text = TextIndexCost::default();
    for (column, (attribute, values)) in schema.attributes.iter().zip(by_entity).enumerate() {
        if !postings_are_owed(schema, attribute) {
            continue;
        }
        let column_dir = partition_dir.join("attrs").join(&attribute.name);
        std::fs::create_dir_all(&column_dir).map_err(|e| BuildError::io(&column_dir, e))?;

        // **A text column owes no value column**, so it leaves before the one below is written.
        // Its entity-space artefact is the token dictionary and the postings over it; the values
        // themselves are in the record blob, which no scan reads (records §4.4). That is
        // [`value_column_is_owed`]'s whole exception, and the loop asks it rather than the type so
        // that the pass and [`may_take_extents`] cannot part company about which columns keep an
        // arena. A `text` column's extents are there whatever the budget said
        // ([`extents_are_forced`]), which is what lets this arm require them.
        if !value_column_is_owed(schema, attribute) {
            let extents = spilled
                .iter()
                .find(|column_extents| column_extents.column == column)
                .ok_or_else(|| {
                    BuildError::Invalid(format!(
                        "attribute '{}' is text and the join spilled no extents for it; a text \
                         column's values are its extents and there is nothing to index",
                        attribute.name
                    ))
                })?;
            let started = std::time::Instant::now();
            let written = write_text_index(
                &column_dir,
                attribute,
                TextValues::Extents(extents),
                text_plan,
            )?;
            text.elapsed += started.elapsed();
            text.terms += written.terms;
            paths.extend(written.paths);
            continue;
        }

        // The value column is the artefact of record (filter-index §2.1); the postings below are
        // derived from it. Written first so that a build interrupted between the two leaves the
        // record without its accelerator rather than an accelerator with no record.
        //
        // **A keyword column's characters are wherever its route put them** ([`ColumnRoutes`]):
        // the column's own arena, or the extents the join spilled, which this pass is the only
        // reader of. A column of another family never spills, so the lookup answers `None` for it.
        let extents = spilled
            .iter()
            .find(|column_extents| column_extents.column == column);
        let values_path = column_dir.join("values.arrow");
        let presence_path = column_dir.join("presence.roaring");
        let written = write_column_values(
            &column_dir,
            &values_path,
            &presence_path,
            attribute,
            values,
            extents,
            keyword_plan,
        )?;
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
///
/// **A spilled column arrives as extents and everything else as columns**, and the two are merged
/// here (`build-column-extents.md`). A column [`ColumnRoutes`] routed to extents was never placed
/// at an entity index: the join spilled each of its chunks as a blob extent in that chunk's entity
/// order, so this stage reads each extent front to back and takes the lowest head across them.
/// What that removes is a random read per row into a file larger than the machine, which at
/// 1.02×10⁸ abstracts wrote 52 MB in thirteen minutes at 144 major faults a second
/// (`probes/2026-09-03-text-arena-streaming/` §5).
pub(crate) fn write_record_blob(
    partition_dir: &Path,
    schema: &crate::config::Schema,
    n: u64,
    routes: &ColumnRoutes,
    by_entity: &[EntityColumn],
    spilled: &[crate::extents::OpenExtents],
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
        .filter(|(_, a)| blob_resident(schema, a))
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
    let n = n as usize;
    // The columns that are still columns. A column the join spilled has its tag carried by its
    // extents instead ([`ColumnRoutes`]), and no tag is carried by both.
    let column_tags: Vec<usize> = blob_columns
        .iter()
        .copied()
        .filter(|&column| !routes.takes_extents(column))
        .collect();
    let mut columns = ColumnRows {
        schema,
        by_entity,
        columns: &column_tags,
        entity: 0,
        n,
        row: Vec::new(),
        at: 0,
    };
    // **Only a blob-resident column's extents are the blob's.** An indexed keyword spills its
    // characters the same way and its reader is the dictionary pass, which has already run and
    // unlinked them; its values belong in `values.arrow` and its tag in no blob row at all.
    let mut extents: Vec<Vec<crate::extents::ExtentRows<'_>>> = spilled
        .iter()
        .filter(|column| blob_columns.contains(&column.column))
        .map(crate::extents::ExtentRows::over)
        .collect();
    let mut sources: Vec<&mut dyn tessera_filter_write::RecordRows> = Vec::new();
    if !column_tags.is_empty() {
        sources.push(&mut columns);
    }
    for column in extents.iter_mut() {
        for extent in column.iter_mut() {
            sources.push(extent);
        }
    }
    tessera_filter_write::merge_record_rows(
        &mut sources,
        &croaring::Bitmap::new(),
        &blocks_path,
        &hasrow_path,
        &directory_path,
        RECORD_BLOCK_TARGET,
    )
    .map_err(|e| BuildError::io(&blocks_path, e))?;
    for path in [&blocks_path, &hasrow_path, &directory_path] {
        fsync_file(path)?;
    }
    Ok(vec![blocks_path, hasrow_path, directory_path])
}

/// Does this column's value live in the record blob — the home for a value with no other?
///
/// **Blob-resident is "no other home", not "no flags"** — and for a category the two differ.
/// Records §4.2 exempts categories from the blob because their entity-space structures are the
/// vocabulary machinery's constant floor, but that floor is [`postings_are_owed`], which holds for
/// an *indexed* or `derived` category and not for a `public` one. A `public` category declared with
/// neither flag therefore has no hot column, no value column and no postings, so excluding every
/// category stored its values nowhere at all and refused nothing — silent loss of a field the
/// caller declared. Asking the same question the entity-space pass asks is what keeps the two
/// exhaustive between them: a field is in exactly one home, and records §3's rule that every
/// declared field answers `entity → value` holds by construction.
///
/// **Text is blob-resident whether or not it is indexed** (records §4.4), which is the one place
/// this predicate is not simply "has no other home": an indexed text column has a token index
/// *and* a blob row, because the index answers `match` and only the blob can answer
/// `entity → value`. Postings are term → entities; nothing in them reconstructs the prose a
/// drill-down returns.
///
/// It is also what says **when a column's storage may go back to the disk**: a non-render column
/// that is not blob-resident has no reader past the filter postings (`write_column_release`).
pub(crate) fn blob_resident(
    schema: &crate::config::Schema,
    attribute: &crate::config::Attribute,
) -> bool {
    attribute.ty == ScalarType::Text || (!attribute.render && !postings_are_owed(schema, attribute))
}

/// Does this column owe an **entity-space value column**, and the dictionary a keyword's ordinals
/// index?
///
/// [`postings_are_owed`] is most of it: the value column is the artefact of record and the
/// postings are derived from it (`filter-index.md` §2.1), so a column that owes no postings owes
/// no value column either. A `text` column is the exception in the other direction — it owes
/// postings over its tokens and no per-entity slot at all, its values being the record blob's
/// (records §4.4) — which is why the type appears here and nowhere else in the routing.
///
/// This is the whole of what reads a string column's characters **at an entity index**, `render`
/// aside, and [`takes_extents`] is its complement.
pub(crate) fn value_column_is_owed(
    schema: &crate::config::Schema,
    attribute: &crate::config::Attribute,
) -> bool {
    attribute.ty != ScalarType::Text && postings_are_owed(schema, attribute)
}

/// Could this column's characters take the record blob's extents instead of an entity-ordered
/// arena ([`crate::extents`])? Whether they do is [`ColumnRoutes`].
///
/// **The order a reader wants decides, not the declared type.** An arena is a permutation of the
/// source, and the entity-indexed offset array beside it exists to answer `entity → value` at
/// random. Every pass that reads a string column reads it once, ascending in the entity: the
/// record blob's merge, a `text` column's token index, and the keyword dictionary's chunk walk.
/// An extent delivers that order directly — each one is a join chunk in the chunk's own entity
/// order, and a merge across them is the column ascending — so the offset array buys nothing any
/// of them needs. What closes the route instead is `render`, whose row tail reads the column at a
/// row and not at an entity.
///
/// That answers `true` for every bundle-wide string column: a `text` column, a `keyword` or
/// `utf8` column declared with neither flag — the shape that cost 5.7 GB of arena and offsets at
/// 125,789,091 GBIF occurrences to hand the blob 1.55 GB
/// (`probes/2026-09-10-blob-resident-strings/`) — and an **indexed** `keyword` or `utf8` column,
/// whose extents the dictionary pass reads through
/// [`crate::extents::OpenExtents::for_each_live_record`] where it used to read an arena at
/// `8 B/item` of offsets.
///
/// **The extents' reader differs by column and every routed column has one.** A
/// [`blob_resident`] column's are merged into the blob; an indexed one's are the dictionary
/// pass's alone and are unlinked when it ends. Extents nothing reads would be a column stored
/// nowhere, which is what the `render` term above refuses.
///
/// **`render` is refused on every string type at the declaration**
/// (`config::compile_attributes`), and `write_columns` refuses one that reaches it anyway, so that
/// term fires only for a `Schema` assembled programmatically.
///
/// **A group-scoped family is not asked.** Its columns are the scoped pass's own, one per view,
/// and the join that fills them is `write_scoped_columns`'s rather than this one's (`views.md`
/// §5). So a scoped string column keeps its arena whatever its flags say.
pub(crate) fn may_take_extents(
    _schema: &crate::config::Schema,
    attribute: &crate::config::Attribute,
) -> bool {
    matches!(
        attribute.ty,
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text
    ) && !attribute.render
}

/// Is the arena route closed to this column whatever the arena would cost?
///
/// **`text` takes extents on the permutation and not on the size** (`build-column-extents.md` §2).
/// Its arena is filled by a second decode of the source's prose, in entity order, because the
/// token index and the record blob both walk entity space and neither can afford a random read per
/// document. That second decode was the largest single cost in the build — `attribute_tail` 6,369.1 s
/// against 890.2 s at the 10⁸ PaperSeek rung — and it is paid whether or not the arena would have
/// fitted in memory. So a `text` column has one route and the choice below does not reach it.
pub(crate) fn extents_are_forced(
    schema: &crate::config::Schema,
    attribute: &crate::config::Attribute,
) -> bool {
    attribute.ty == ScalarType::Text && may_take_extents(schema, attribute)
}

/// **Which declared columns the join spills as record-blob extents, decided once for the build.**
///
/// [`may_take_extents`] says which columns have two routes; this says which of them take the
/// spilled one. The choice is [`crate::residency::plan_routes`]'s and is the modelled arena
/// against the space free on the output filesystem: an arena the disk cannot hold is the ENOSPC
/// that stops a build at hour three, and an arena it can hold costs the build a compression pass
/// it did not need. Both routes write the same bundle, byte for byte, so nothing outside the build
/// can observe which was taken.
///
/// Indexed by declaration position, and `false` for every column the route is not available to, so
/// a caller asks one question of one structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnRoutes {
    extents: Vec<bool>,
}

impl ColumnRoutes {
    /// Every column the extent route is available to takes it — the route a build takes when no
    /// arena fits, and the one the record blob's own tests want.
    pub(crate) fn every_available(schema: &crate::config::Schema) -> Self {
        ColumnRoutes {
            extents: schema
                .attributes
                .iter()
                .map(|a| may_take_extents(schema, a))
                .collect(),
        }
    }

    /// Only the columns with no arena route at all — the `text` family ([`extents_are_forced`]).
    pub(crate) fn forced_only(schema: &crate::config::Schema) -> Self {
        ColumnRoutes {
            extents: schema
                .attributes
                .iter()
                .map(|a| extents_are_forced(schema, a))
                .collect(),
        }
    }

    /// Does the column at this declaration position spill its characters as extents?
    ///
    /// A position past the schema answers `false`: the record blob's tests build a `ColumnRoutes`
    /// over the schema they are given, and a column that is not in it has no route either way.
    pub(crate) fn takes_extents(&self, column: usize) -> bool {
        self.extents.get(column).copied().unwrap_or(false)
    }

    /// Move one column onto the arena route. [`crate::residency::plan_routes`]'s only writer.
    pub(crate) fn take_arena(&mut self, column: usize) {
        self.extents[column] = false;
    }

    /// The declaration positions that spilled, for the line the build prints and the report it
    /// returns.
    pub(crate) fn spilled(&self) -> Vec<usize> {
        self.extents
            .iter()
            .enumerate()
            .filter(|&(_, &spilled)| spilled)
            .map(|(index, _)| index)
            .collect()
    }
}

/// The entity-ordered columns' rows as one ascending stream, for the blob's merge.
///
/// An entity carrying no value in any of them has no row, exactly as it had none when this stage
/// was a loop over entity space.
struct ColumnRows<'a> {
    schema: &'a crate::config::Schema,
    by_entity: &'a [EntityColumn],
    columns: &'a [usize],
    entity: usize,
    n: usize,
    /// The row the stream is at. A string field borrows the column's arena, which outlives this
    /// stream, so the row is held rather than rebuilt when the merge reads it.
    row: Vec<RecordFieldRef<'a>>,
    at: u32,
}

impl<'a> tessera_filter_write::RecordRows for ColumnRows<'a> {
    fn advance(&mut self) -> std::io::Result<bool> {
        while self.entity < self.n {
            let entity = self.entity;
            self.entity += 1;
            self.row.clear();
            for &column in self.columns {
                let attribute = &self.schema.attributes[column];
                let value = record_value_of(&self.by_entity[column], entity, attribute)
                    .map_err(std::io::Error::other)?;
                let Some(value) = value else { continue };
                let tag = u16::try_from(column).map_err(|_| {
                    std::io::Error::other(format!(
                        "attribute '{}' is declared at position {column}, past the u16 \
                         field-tag space",
                        attribute.name
                    ))
                })?;
                self.row.push(RecordFieldRef { tag, value });
            }
            if self.row.is_empty() {
                continue;
            }
            self.at = entity as u32;
            return Ok(true);
        }
        self.row.clear();
        Ok(false)
    }
    fn entity(&self) -> u32 {
        self.at
    }
    fn field_count(&self) -> usize {
        self.row.len()
    }
    fn field(&self, i: usize) -> std::io::Result<RecordFieldRef<'_>> {
        self.row.get(i).copied().ok_or_else(|| {
            std::io::Error::other(format!(
                "field {i} was asked for of entity {}'s row, which carries {}",
                self.at,
                self.row.len()
            ))
        })
    }
}

/// One staged value as the blob row carries it, or `None` where the entity carries nothing in
/// this column — the per-family absence rule `write_record_blob`'s doc states.
///
/// **The column and the entity, not the value**, so that the string arm can borrow: reading through
/// `EntityColumn::value_at` clones the `String` out of the arena, and the row that carried it
/// owned a second copy. Both are gone — the value the blob's merge is handed borrows the arena
/// and is copied once, into the block it is encoded in.
fn record_value_of<'a>(
    values: &'a EntityColumn,
    entity: usize,
    attribute: &crate::config::Attribute,
) -> Result<Option<RecordValueRef<'a>>> {
    if attribute.vocabulary.is_some() {
        let code = category_code(&values.value_at(entity), &attribute.name)?;
        if code == tessera_store::vocabulary::ABSENT_CODE {
            return Ok(None);
        }
        // The code at the declared width — the value the entity-space column would have stored,
        // resolved to its key at drill-down through the manifest's vocabulary, never in the
        // artefact.
        return Ok(Some(match attribute.ty {
            ScalarType::U8 => RecordValueRef::U8(code as u8),
            ScalarType::U16 => RecordValueRef::U16(code as u16),
            _ => RecordValueRef::U32(code),
        }));
    }
    // The string families first, borrowed. `str_at` answers `None` for an absent entity and for a
    // column that is not string-backed, and the match below then reads the same absence out of
    // `value_at` — so the two agree without either having to know which family it is looking at.
    if let Some(text) = values.str_at(entity) {
        return Ok(Some(RecordValueRef::Utf8(text)));
    }
    Ok(match values.value_at(entity) {
        ScalarValue::Null => None,
        ScalarValue::Bool(v) => Some(RecordValueRef::Bool(v)),
        ScalarValue::U8(v) => Some(RecordValueRef::U8(v)),
        ScalarValue::U16(v) => Some(RecordValueRef::U16(v)),
        ScalarValue::U32(v) => Some(RecordValueRef::U32(v)),
        ScalarValue::U64(v) => Some(RecordValueRef::U64(v)),
        ScalarValue::I8(v) => Some(RecordValueRef::I8(v)),
        ScalarValue::I16(v) => Some(RecordValueRef::I16(v)),
        ScalarValue::I32(v) => Some(RecordValueRef::I32(v)),
        ScalarValue::I64(v) => Some(RecordValueRef::I64(v)),
        ScalarValue::F32(v) => Some(RecordValueRef::F32(v)),
        ScalarValue::F64(v) => Some(RecordValueRef::F64(v)),
        ScalarValue::TimestampUs(v) => Some(RecordValueRef::TimestampUs(v)),
        // Unreachable: a string-backed column answered `str_at` above, and no other storage
        // yields a string. A column that reached here carrying one would be a column whose two
        // accessors disagree about its family.
        ScalarValue::Utf8(_) => {
            return Err(BuildError::Invalid(format!(
                "attribute '{}' answered a string at entity {entity} through the scalar \
                 accessor and not through the arena; its storage and its family disagree",
                attribute.name
            )))
        }
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
    extents: Option<&crate::extents::OpenExtents>,
    keyword_plan: KeywordDictPlan,
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
        // The characters are wherever the route put them. A column that spilled carries no
        // values here at all ([`EntityColumn::spilled`]), so the two cases are exclusive and the
        // column is still what says how many entities there are.
        let source = match extents {
            Some(extents) => KeywordValues::Extents(extents),
            None => KeywordValues::Column(values),
        };
        dict = Some(write_keyword_column(
            column_dir,
            values_path,
            attribute,
            source,
            &mut presence,
            &mut writer,
            keyword_plan,
        )?);
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

/// Visit one keyword column's present values in entity order, as `(row, value)`, with `presence`
/// told which entities carry one. Returns how many rows carry a value.
///
/// **The row is the value's slot, not the entity id**: the k-th set bit's value is at slot k
/// (filter-index §2.1). The pass that decides *presence* is therefore the pass that decides
/// *slots*, and both come out of this one walk, so the two cannot come to disagree about an absent
/// entity.
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
fn for_each_keyword(
    attribute: &crate::config::Attribute,
    values: KeywordValues<'_>,
    presence: &mut Presence,
    visit: &mut dyn FnMut(u32, &str) -> Result<()>,
) -> Result<u64> {
    let mut rows = 0u64;
    let mut step = |entity: u32, text: &str| -> Result<()> {
        if text.is_empty() {
            return Err(BuildError::Invalid(format!(
                "attribute '{}' is declared `keyword` and entity {entity} carries the empty \
                 string, which is not a value (records §7, contracts §2.4 — an unset field and a \
                 client bug both produce it). Leave the cell null for absence",
                attribute.name
            )));
        }
        // **A row is addressed by a `u32` in the run files below**, so a column with more rows
        // than that is refused here rather than wrapping into another row's ordinal. Entity ids
        // are `u32` and a row exists only where an entity does, so this is reachable only at the
        // very top of entity space.
        let row = u32::try_from(rows).map_err(|_| {
            BuildError::Invalid(format!(
                "attribute '{}': more than {} rows carry a value, which is more than the \
                 dictionary pass addresses a row with",
                attribute.name,
                u32::MAX
            ))
        })?;
        presence.present(entity);
        visit(row, text)?;
        rows += 1;
        Ok(())
    };
    match values {
        // Absent runs are skipped a word at a time; absence itself is what [`Presence`] reads out
        // of the gaps this leaves.
        KeywordValues::Column(values) => {
            for entity in values.present_entities() {
                // Borrowed, not read through `value_at`: this visits one `&str` per entity across
                // the whole column, so copying here would be a second copy of every keyword in the
                // corpus.
                let Some(text) = values.str_at(entity) else {
                    return Err(BuildError::Invalid(format!(
                        "attribute '{}' is declared `keyword` but carries {:?}",
                        attribute.name,
                        values.value_at(entity)
                    )));
                };
                step(entity as u32, text)?;
            }
        }
        KeywordValues::Extents(extents) => {
            extents.for_each_live_record(&mut |entity, text| step(entity, text))?;
        }
    }
    Ok(rows)
}

/// Where one keyword column's characters are read from — the two routes [`ColumnRoutes`] chooses
/// between, seen from the one pass that consumes them.
///
/// Both deliver each present entity once, ascending, carrying the value the join wrote last for
/// it: the column through its presence bits and its offset array, the extents through a merge
/// across the join chunks with each one's live set applied (`crate::extents`). So the dictionary,
/// the ordinals and the presence bitmap are the same bytes either way, and the choice is the
/// build's disk and not the bundle's.
#[derive(Clone, Copy)]
pub(crate) enum KeywordValues<'a> {
    Column(&'a EntityColumn),
    Extents(&'a crate::extents::OpenExtents),
}

impl KeywordValues<'_> {
    /// A ceiling on the present rows, for the chunk buffer's capacity: the column's entities, or
    /// the extents' rows before their live sets take the overwritten ones out. A hint and not a
    /// count — nothing downstream reads it.
    fn rows_hint(&self) -> usize {
        match self {
            KeywordValues::Column(values) => values.len(),
            KeywordValues::Extents(extents) => {
                extents.blobs.iter().map(|blob| blob.rows() as usize).sum()
            }
        }
    }
}

/// How one indexed keyword column's dictionary pass is sized: the present rows one chunk sorts
/// before it spills a run.
///
/// **The same share of the build's memory budget the text pass takes** — [`TEXT_BUDGET_SHARE`],
/// under the same [`TEXT_BUDGET_MIN`] and [`TEXT_BUDGET_MAX`] clamp — and for the reason that
/// share is what it is: both run inside the entity-order tail `residency.rs` models, with every
/// declared column resident beside them, so each is a transient on top of the build's largest
/// resident set rather than a stage with the machine to itself.
///
/// Where the two differ is the divisor. The text pass divides its allowance by the thread count
/// because `threads` workers accumulate at once; this pass is one walk of the column in row order,
/// so one chunk is live and the allowance is that chunk. It is not parallelised: on either route
/// the values arrive as one entity-ascending stream, and a second worker over a range of it would
/// be a second reader of the same file rather than a second stream.
///
/// **Two bounds, not one.** The chunk buffers each row's characters as well as its entry in the
/// array to be sorted, so a chunk of long values fills the allowance at a row count a chunk of
/// short ones would not. Whichever bound a chunk reaches first spills it. Where the boundary falls
/// changes the runs and changes no byte of the output
/// ([`tests::chunking_the_keyword_column_does_not_change_its_bytes`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct KeywordDictPlan {
    chunk_rows: usize,
    chunk_bytes: usize,
}

/// The floor on a chunk, so a tiny `--memory-budget` cannot derive a plan that spills a run every
/// few rows and then cascades them all back together.
const KEYWORD_MIN_CHUNK_ROWS: usize = 1 << 12;

/// The floor on a chunk's characters, for the reason [`KEYWORD_MIN_CHUNK_ROWS`] is the floor on
/// its rows.
const KEYWORD_MIN_CHUNK_BYTES: usize = 1 << 20;

impl KeywordDictPlan {
    /// The plan a build's memory budget derives.
    pub(crate) fn for_budget(budget: u64) -> KeywordDictPlan {
        let allowance = (budget / TEXT_BUDGET_SHARE).clamp(TEXT_BUDGET_MIN, TEXT_BUDGET_MAX);
        let chunk_rows = (allowance as usize / KEYWORD_ROW_BYTES).max(KEYWORD_MIN_CHUNK_ROWS);
        let chunk_bytes = (allowance as usize).max(KEYWORD_MIN_CHUNK_BYTES);
        KeywordDictPlan {
            chunk_rows,
            chunk_bytes,
        }
    }

    /// An explicit plan, for the tests that must force several runs and a cascade out of a corpus
    /// small enough to assert over.
    #[cfg(test)]
    fn explicit(chunk_rows: usize) -> KeywordDictPlan {
        KeywordDictPlan {
            chunk_rows: chunk_rows.max(1),
            chunk_bytes: usize::MAX,
        }
    }
}

/// What one buffered row costs beyond its characters: its entry in the array the chunk sorts, and
/// its place in the scratch a key's rows are gathered into. The figure [`KeywordDictPlan`] divides
/// the budget by.
const KEYWORD_ROW_BYTES: usize = std::mem::size_of::<KeywordPair>() + std::mem::size_of::<u32>();

/// One present row's key and the row it sits at.
///
/// **The key's first bytes are carried beside its position so that most comparisons never reach
/// the characters.** The first eight bytes read big-endian and zero-padded order two keys exactly
/// as their bytes do: where the padded prefixes differ, the keys differ the same way at the same
/// position, a key shorter than the other padding with the zeros that make "shorter is less" true.
/// So [`keyword_key_order`] reads the buffer only to separate two keys that agree in their first
/// eight characters.
///
/// **The key is a position in the chunk's own buffer and not a reference into the column.** The
/// two routes deliver a value's characters differently — an arena the whole pass may borrow from,
/// and a block of one extent that is decompressed and dropped — so the chunk copies what it
/// buffers and both routes reach the same sort. Holding a reference measured faster when the
/// alternative was a position into a side array of the *column's* 320 MB of keys, 13.45 s against
/// 10.12 s over 2×10⁷ distinct ones (`probes/2026-09-08-keyword-spill/`); what the sort reaches
/// into here is the chunk's own buffer, which the plan bounds.
#[derive(Clone, Copy)]
struct KeywordPair {
    prefix: u64,
    at: u32,
    len: u32,
    row: u32,
}

/// The key's first eight bytes, big-endian and zero-padded — see [`KeywordPair`].
fn keyword_prefix(text: &str) -> u64 {
    let bytes = text.as_bytes();
    let take = bytes.len().min(8);
    let mut head = [0u8; 8];
    head[..take].copy_from_slice(&bytes[..take]);
    u64::from_be_bytes(head)
}

/// Order two pairs by key, and by key alone.
///
/// **The row is not a tiebreak, and leaving it out is what keeps a low-cardinality column cheap.**
/// A tiebreak makes every element distinct, which defeats the equal-element partitioning an
/// introsort does: a column of two dozen codes would pay a full `n log n` over every row to
/// separate rows that were in order to begin with. What it costs instead is that a key's rows come
/// out of the sort in no order, so [`KeywordChunk::spill`] sorts each key's own rows — a `u32`
/// sort over one key's rows, which is also what
/// [`crate::spill::TextRunWriter::push_entity`] demands: strictly ascending within a record.
fn keyword_key_order(a: &KeywordPair, b: &KeywordPair, text: &[u8]) -> std::cmp::Ordering {
    a.prefix
        .cmp(&b.prefix)
        .then_with(|| buffered_key(text, a).cmp(buffered_key(text, b)))
}

/// One buffered key's characters.
fn buffered_key<'a>(text: &'a [u8], pair: &KeywordPair) -> &'a [u8] {
    &text[pair.at as usize..pair.at as usize + pair.len as usize]
}

/// One chunk of present rows, buffered in row order and sorted by key at the spill.
struct KeywordChunk {
    pairs: Vec<KeywordPair>,
    /// The buffered rows' characters, in push order. [`KeywordPair`] indexes into it.
    text: Vec<u8>,
    /// One key's rows, gathered and sorted for the run writer.
    rows: Vec<u32>,
}

impl KeywordChunk {
    fn with_capacity(rows: usize) -> KeywordChunk {
        KeywordChunk {
            pairs: Vec::with_capacity(rows),
            text: Vec::new(),
            rows: Vec::new(),
        }
    }

    /// Whether this chunk has reached either of the plan's bounds.
    fn full(&self, plan: KeywordDictPlan) -> bool {
        self.pairs.len() >= plan.chunk_rows || self.text.len() >= plan.chunk_bytes
    }

    fn push(&mut self, row: u32, text: &str) -> Result<()> {
        let at = u32::try_from(self.text.len()).map_err(|_| {
            BuildError::Invalid(format!(
                "a keyword chunk buffered {} bytes of characters, past the u32 a buffered key's \
                 position is held in",
                self.text.len()
            ))
        })?;
        let len = u32::try_from(text.len()).map_err(|_| {
            BuildError::Invalid(format!(
                "a keyword value of {} bytes is longer than the u32 its length is held in",
                text.len()
            ))
        })?;
        self.text.extend_from_slice(text.as_bytes());
        self.pairs.push(KeywordPair {
            prefix: keyword_prefix(text),
            at,
            len,
            row,
        });
        Ok(())
    }

    /// Sort by key and write the chunk out as one run, leaving the buffer empty.
    fn spill(
        &mut self,
        column_dir: &Path,
        seq: &mut usize,
        receipts: &mut Vec<spill::SpillReceipt>,
    ) -> Result<()> {
        if self.pairs.is_empty() {
            return Ok(());
        }
        // Split apart so the comparator may read the characters while the array is sorted.
        let KeywordChunk { pairs, text, rows } = self;
        pairs.sort_unstable_by(|a, b| keyword_key_order(a, b, text));

        let path = column_dir.join(format!("keyword-run-{seq:05}.spill"));
        let mut writer = spill::TextRunWriter::create(&path)?;
        let mut start = 0usize;
        while start < pairs.len() {
            let head = pairs[start];
            let key = buffered_key(text, &head);
            let mut end = start + 1;
            while end < pairs.len()
                && pairs[end].prefix == head.prefix
                && buffered_key(text, &pairs[end]) == key
            {
                end += 1;
            }
            rows.clear();
            rows.extend(pairs[start..end].iter().map(|pair| pair.row));
            rows.sort_unstable();
            let count = u32::try_from(rows.len()).map_err(|_| {
                BuildError::Invalid(format!(
                    "keyword run {}: the key {:?} is carried by more rows than a u32 can count",
                    path.display(),
                    String::from_utf8_lossy(key)
                ))
            })?;
            writer.begin(key, count)?;
            for &row in rows.iter() {
                writer.push_entity(row)?;
            }
            start = end;
        }
        receipts.push(writer.finish()?);
        // Cleared rather than replaced, where the text pass's accumulator is replaced: these
        // buffers' capacity *is* the plan's budget, so keeping it is what the next chunk wants.
        self.pairs.clear();
        self.text.clear();
        *seq += 1;
        Ok(())
    }
}

/// The partition the merge pushes `(row, ordinal)` into.
///
/// **Under the column's own directory in the bundle, not under `.build-tmp/`**, which is where
/// the sorted runs it merges are already written: the partition sits with its own inputs, and the
/// one pass that writes both unlinks both. A build that dies between them leaves a bundle with no
/// `CURRENT`, which the sweep removes whole — so nothing survives a failure here that would not
/// survive it in `.build-tmp/`. The mapped scratch this replaced was under `.build-tmp/` because
/// it was a scratch *array*, with no runs beside it to sit with.
const KEYWORD_ORDINAL_PARTITION: &str = "keyword-ordinals";

/// One `(row, ordinal)` record of that partition: two little-endian `u32`s, the row first because
/// it is the key the partition routes on.
const KEYWORD_ORDINAL_RECORD: usize = 8;

/// One indexed keyword column's dictionary and its ordinal values file. Returns the dictionary's
/// path.
///
/// # The shape: chunk, spill, merge, scatter
///
/// **The dictionary used to be built whole in memory**: a `&str` per present row, cloned, sorted
/// and deduplicated into the distinct key set, with each row's ordinal then found by binary search
/// over it. That was 32 bytes per row of resident memory with nothing to bound it — 7.46 GB over
/// rung 5's 2.33×10⁸ `uuid` rows, and modelled to exhaust a 47 GB box somewhere above 3.5×10⁸ —
/// and its search was ~28 probes per row, each dereferencing into a random offset of an 8.0 GB
/// arena. It ran at 175×10³ rows/s where the same corpus's 2.4×10⁵-key `scientific_name` column,
/// whose key set stays in cache, ran an order of magnitude faster.
///
/// So the pass has the text index's shape, with a fourth step the text index does not need:
///
/// 1. **Chunk.** The present rows are walked once in row order and buffered as
///    `(key, row)` pairs, [`KeywordDictPlan`] rows at a time.
/// 2. **Spill.** A full chunk is sorted by key and written out as a **sorted run**
///    ([`crate::spill::TextRunWriter`], the same run format and the same receipt the text index
///    spills) — each distinct key once, front-coded, with the ascending rows carrying it. So the
///    pass's residency is the plan, whatever the corpus, and the run count grows instead of the
///    peak.
/// 3. **Cascade**, where a corpus produced more runs than one merge may hold file descriptors for
///    ([`RUN_MERGE_FAN_IN`]).
/// 4. **Merge.** The runs are merged k-way on the key. Each distinct key is pushed once to
///    [`tessera_filter::SortedDictWriter`], which streams the dictionary and holds only its
///    restart table, and the ordinal it returns is written to every row the merge then drains for
///    that key.
/// 5. **Partition.** The ordinals leave the merge in key order and the values file needs them in
///    row order, so the merge pushes `(row, ordinal)` to a [`crate::spill::Partition`] by row
///    range. Each bucket is then read whole, scattered into a `u32` window over its own row range,
///    and pushed to the values writer in [`VALUE_CHUNK`] slices.
///
/// **A partition and not a scatter into a mapped array addressed by row**, which is what step 5
/// was. A mapped array is bounded in memory only while the page cache holds it, and the cache is
/// whatever the rest of the build leaves: on a run whose `layers` stage had left 3 GB of cache,
/// the scatter over a 12.9 GB array read **16 TB from disk in four hours** to get 49% of the way
/// through one column, at 100 to 180 major faults a second
/// (`docs/evidence/memos/2026-09-12-gbif-whole-corpus-build-observations.md` §6). A partition
/// costs 8 bytes a row of spill and one more pass, and its cost does not depend on what else the
/// build holds. What is resident is one bucket and one window, both a 128th of the column.
///
/// **The output is a function of the corpus alone, never of the plan.** The dictionary is the
/// sorted distinct key set and a row's ordinal is its key's position in it; neither depends on
/// where a chunk boundary fell.
/// [`tests::chunking_the_keyword_column_does_not_change_its_bytes`] is the assertion.
///
/// # What is checked, and why each check is here
///
/// **The ordinal is the key's position in the sorted distinct set.** The merge yields each
/// distinct key once, ascending, and pushes it to the dictionary writer, so the ordinal the writer
/// returns *is* that position — and it is asserted against the count of keys pushed rather than
/// assumed from the two staying in step. An ordinal naming another key's value has no symptom: it
/// recolours a map, and every count it feeds stays plausible (`tessera_filter::dict`).
///
/// **Every row that went in came back out.** Each run verifies its own count and anchor as it is
/// read; what no single run can see is that the *set* of runs is whole, so the run counts are
/// summed against the rows the walk visited before the merge, and the rows the merge drained are
/// counted against them after it. A run file lost between the spill and the merge would otherwise
/// be a values file whose tail rows all read ordinal 0 — the first key's, and a legal one.
fn write_keyword_column(
    column_dir: &Path,
    values_path: &Path,
    attribute: &crate::config::Attribute,
    values: KeywordValues<'_>,
    presence: &mut Presence,
    writer: &mut ValueColumnWriter,
    plan: KeywordDictPlan,
) -> Result<PathBuf> {
    // ---- 1 and 2. the chunk pass, spilling sorted runs ---------------------------------------
    let mut receipts: Vec<spill::SpillReceipt> = Vec::new();
    let mut seq = 0usize;
    // Sized to the plan up front, or to the column where it is smaller.
    let mut chunk = KeywordChunk::with_capacity(plan.chunk_rows.min(values.rows_hint()));
    let rows = for_each_keyword(attribute, values, presence, &mut |row, text| {
        chunk.push(row, text)?;
        if chunk.full(plan) {
            chunk.spill(column_dir, &mut seq, &mut receipts)?;
        }
        Ok(())
    })?;
    chunk.spill(column_dir, &mut seq, &mut receipts)?;
    drop(chunk);

    // ---- 3. the cascade, where a column produced more runs than one merge may hold open ------
    let receipts = cascade_sorted_runs(column_dir, "keyword", receipts)?;

    // ---- 4. the merge: the dictionary, and each row's ordinal ---------------------------------
    let dict_path = column_dir.join(tessera_filter::DICT_FILE);
    let partition = spill::Partition::create(
        column_dir,
        KEYWORD_ORDINAL_PARTITION,
        spill::boundaries_uniform(rows),
        KEYWORD_ORDINAL_RECORD,
        rows,
    )?;
    let partition = merge_keyword_runs(&dict_path, attribute, &receipts, rows, partition)?;
    for receipt in &receipts {
        std::fs::remove_file(&receipt.path).map_err(|e| BuildError::io(&receipt.path, e))?;
    }

    // ---- 5. the values file, in row order ----------------------------------------------------
    //
    // **A bucket at a time, front to back.** The buckets partition the rows in ascending ranges,
    // so walking them in order is walking the rows in order; each is scattered into a window over
    // its own range and pushed on, and its file is released before the next is read.
    let mut store = partition.store;
    for k in 0..partition.ranges.len() {
        let (lo, hi) = partition.ranges[k];
        let mut window = vec![0u32; (hi - lo) as usize];
        for record in store.load(k)?.chunks_exact(KEYWORD_ORDINAL_RECORD) {
            let row = u32::from_le_bytes(record[..4].try_into().expect("a record is 8 bytes"));
            let ordinal = u32::from_le_bytes(record[4..].try_into().expect("a record is 8 bytes"));
            // The bucket-range check the row-bound check became: a record outside the range its
            // bucket addresses would be written over another bucket's row, or past the window.
            let slot = (row as u64)
                .checked_sub(lo)
                .and_then(|at| window.get_mut(at as usize))
                .ok_or_else(|| {
                    BuildError::Invalid(format!(
                        "attribute '{}': bucket {k} of the keyword ordinals holds row {row}, \
                         outside the range [{lo}, {hi}) it addresses",
                        attribute.name
                    ))
                })?;
            *slot = ordinal;
        }
        store.delete(k)?;
        for slice in window.chunks(VALUE_CHUNK) {
            writer
                .push(&Codes::U32(slice.to_vec().into()))
                .map_err(|e| BuildError::io(values_path, e))?;
        }
    }
    Ok(dict_path)
}

/// Merge the sorted runs into the dictionary, writing each row's ordinal into `ordinals`. Returns
/// the distinct keys written.
///
/// The two guards are [`write_keyword_column`]'s to explain; this is where they are made.
fn merge_keyword_runs(
    dict_path: &Path,
    attribute: &crate::config::Attribute,
    receipts: &[spill::SpillReceipt],
    rows: u64,
    mut partition: spill::Partition,
) -> Result<KeywordOrdinals> {
    let spilled: u64 = receipts.iter().map(|receipt| receipt.count).sum();
    if spilled != rows {
        return Err(BuildError::Invalid(format!(
            "attribute '{}': the keyword runs hold {spilled} rows where the column has {rows} — a \
             run file is missing or was not merged, and the rows it held would take the first \
             key's ordinal",
            attribute.name
        )));
    }

    let dict_file = std::fs::File::create(dict_path).map_err(|e| BuildError::io(dict_path, e))?;
    let mut dict = tessera_filter::SortedDictWriter::new(std::io::BufWriter::new(dict_file))
        .map_err(|e| BuildError::io(dict_path, std::io::Error::from(e)))?;
    let mut merge = TextRunMerge::open(receipts)?;
    let mut keys = 0u64;
    let mut emitted = 0u64;
    // The selected key, copied out of the merge so that the rows it carries may be drained while
    // the messages below still name it. One copy per distinct key, not per row.
    let mut selected: Vec<u8> = Vec::new();
    while merge.next_term()? {
        selected.clear();
        selected.extend_from_slice(merge.term());
        let key = std::str::from_utf8(&selected).map_err(|e| {
            BuildError::Invalid(format!(
                "attribute '{}': a merged key is not UTF-8 ({e}) — the column holds `&str`, so \
                 this is a corrupted run rather than a corpus value",
                attribute.name
            ))
        })?;
        let ordinal = dict
            .push(key)
            .map_err(|e| BuildError::io(dict_path, std::io::Error::from(e)))?;
        if ordinal as u64 != keys {
            return Err(BuildError::Invalid(format!(
                "attribute '{}': the key {key:?} was written at dictionary ordinal {ordinal} and \
                 is the {keys}th distinct key of the merge — the values file would name another \
                 key's value",
                attribute.name
            )));
        }
        keys += 1;
        merge.drain(&mut |row| {
            if row as u64 >= rows {
                return Err(BuildError::Invalid(format!(
                    "attribute '{}': the key {key:?} is carried by row {row}, which is past the \
                     {rows} rows the column has",
                    attribute.name
                )));
            }
            let mut record = [0u8; KEYWORD_ORDINAL_RECORD];
            record[..4].copy_from_slice(&row.to_le_bytes());
            record[4..].copy_from_slice(&ordinal.to_le_bytes());
            partition.push(&record)?;
            emitted += 1;
            Ok(())
        })?;
    }
    dict.finish()
        .map_err(|e| BuildError::io(dict_path, std::io::Error::from(e)))?;

    if emitted != rows {
        return Err(BuildError::Invalid(format!(
            "attribute '{}': the keyword merge yielded {emitted} rows where the column has \
             {rows} — the rows it did not yield would take the first key's ordinal",
            attribute.name
        )));
    }
    let ranges = (0..partition.buckets())
        .map(|k| {
            let (lo, hi) = partition.range(k);
            (lo as u64, hi.min(rows))
        })
        .collect();
    Ok(KeywordOrdinals {
        ranges,
        store: partition.finish()?,
    })
}

/// The `(row, ordinal)` partition the merge filled, with each bucket's row range clipped to the
/// column's own row count — which is what sizes the window the bucket is scattered into.
struct KeywordOrdinals {
    ranges: Vec<(u64, u64)>,
    store: spill::PartitionStore,
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
    chunks: usize,
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
    pub(crate) fn for_budget(budget: u64) -> TextIndexPlan {
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
        //
        // **The chunks are arena windows now, not entity ranges** (`column.rs`), so the number is
        // a target and the arena's own record marks decide how close to it the split lands. Eight
        // a thread carries over unchanged: what it balances — a segmentation cost that varies by
        // an order of magnitude with the script — is a property of the documents, and the windows
        // hold the same documents the entity ranges did.
        TextIndexPlan {
            chunks: (threads * 8).max(1),
            worker_bytes,
            merge_slice_cap: TEXT_MERGE_SLICE_CAP,
        }
    }

    /// An explicit plan, for the tests that must force several chunks and several runs a chunk out
    /// of a corpus small enough to assert over.
    #[cfg(test)]
    fn explicit(chunks: usize, worker_bytes: usize, merge_slice_cap: usize) -> TextIndexPlan {
        TextIndexPlan {
            chunks: chunks.max(1),
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
///    ([`RUN_MERGE_FAN_IN`]). Groups of runs are merged into intermediate runs, in order, until
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
/// Where one text pass reads its prose from.
///
/// **Two producers, one pass.** A bundle-wide `text` column is read from the record-blob extents
/// the join spilled ([`crate::extents`]); a group-scoped one is read from the per-view
/// [`EntityColumn`] the scoped pass builds, which keeps its arena because it has no blob row to
/// be read from (`views.md` §5).
pub(crate) enum TextValues<'a> {
    /// A column's arena, divided into contiguous byte ranges.
    Arena(&'a EntityColumn),
    /// One column's extents, divided into contiguous block ranges.
    Extents(&'a crate::extents::OpenExtents),
}

/// One worker's share of a text pass: a byte range of an arena, or a block range of an extent.
#[derive(Debug, Clone, Copy)]
enum TextWindow {
    Arena(u64, u64),
    Extents(crate::extents::ExtentWindow),
}

impl TextValues<'_> {
    /// At most `target` windows, together covering every value the column holds.
    fn windows(&self, target: usize) -> Vec<TextWindow> {
        match self {
            TextValues::Arena(values) => values
                .arena_windows(target)
                .into_iter()
                .map(|(lo, hi)| TextWindow::Arena(lo, hi))
                .collect(),
            TextValues::Extents(extents) => extents
                .windows(target)
                .into_iter()
                .map(TextWindow::Extents)
                .collect(),
        }
    }

    /// Every value in one window, as `(entity, prose)`.
    fn for_each_record_in(
        &self,
        window: TextWindow,
        visit: &mut dyn FnMut(usize, &str) -> Result<()>,
    ) -> Result<()> {
        match (self, window) {
            (TextValues::Arena(values), TextWindow::Arena(lo, hi)) => {
                values.for_each_record_in(lo, hi, visit)
            }
            (TextValues::Extents(extents), TextWindow::Extents(window)) => {
                extents.for_each_record_in(window, visit)
            }
            _ => Err(BuildError::Invalid(
                "a text window was given to the other producer's reader".into(),
            )),
        }
    }
}

fn write_text_index(
    column_dir: &Path,
    attribute: &crate::config::Attribute,
    values: TextValues<'_>,
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
    //
    // **Windows of the arena, not ranges of entity space.** Entity order is signature-then-Morton
    // order and the arena is in arrival order, so an entity range's documents are scattered
    // through the arena: at 1.02×10⁸ abstracts that is one major fault per document over a file
    // two and a half times the size of the box, and the pass did not finish in four hours
    // (`probes/2026-09-03-text-arena-streaming/`). Each worker now reads one contiguous stretch of
    // the arena front to back and releases it behind itself, and what that costs is the two steps
    // below: a sort at the spill and a merge rather than a concatenation at the fan-in.
    let chunks = values.windows(plan.chunks);
    if let TextValues::Arena(column) = &values {
        // The join wrote every one of those bytes through the mapping, so they are all in this
        // process's page tables and no `fadvise` would release them. See
        // `MappedArena::unmap_pages`.
        column.unmap_arena_pages();
    }
    // **One analyser, shared.** Its construction deserialises the segmenter's dictionary data —
    // the cost the type exists to amortise — and it holds no per-document state, so it is `Sync`
    // and the workers borrow it. What each worker does hold of its own is the normalisation
    // scratch, which is per document by nature.
    let receipts: Vec<Vec<spill::SpillReceipt>> = chunks
        .par_iter()
        .enumerate()
        .map(|(chunk, &window)| {
            index_text_chunk(
                column_dir,
                chunk,
                window,
                &values,
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
    let receipts = cascade_sorted_runs(column_dir, "text", receipts)?;

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

/// Index one contiguous **arena window**, spilling one or more sorted runs.
///
/// The worker's whole residency is `terms` and the window's read buffer, and the budget bounds the
/// first: the tracked figure is an estimate ([`TEXT_TERM_ENTRY_BYTES`]) rather than an allocator
/// reading, so it is deliberately generous. A run is spilled the moment the estimate reaches the
/// budget — mid document is not possible, because the check sits between documents, so the true
/// overshoot is one document's terms.
///
/// A window yields the same `(entity, value)` pairs the entity walk did, in arena order rather
/// than ascending: what is *not* a window's business is which entities they belong to, and the two
/// places that used to get entity ordering for free — the duplicate collapse and the merge — pay
/// for it at [`spill_text_run`] and [`TextRunMerge::drain`] instead.
#[allow(clippy::too_many_arguments)]
fn index_text_chunk(
    column_dir: &Path,
    chunk: usize,
    window: TextWindow,
    values: &TextValues<'_>,
    _attribute: &crate::config::Attribute,
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
    values.for_each_record_in(window, &mut |entity, prose| {
        let entity = entity as u32;
        analyser.for_each_token(prose, &mut scratch, &mut |token| {
            // Looked up before it is owned: a term already seen costs no allocation, which over a
            // corpus is every occurrence but the first of every word.
            if let Some(postings) = terms.get_mut(token.as_bytes()) {
                // A term repeated *within one document* is still the last entry, because a
                // document is one record: that is the duplicate a posting-as-a-set has to
                // collapse, and it is the only one — an entity appears in exactly one live arena
                // record, so no two documents in this window carry the same entity.
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
        Ok(())
    })?;
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
        // **Sorted here, because the window walked the arena and not entity space.** A run's
        // entities must ascend strictly within a term (`spill::TextRunWriter::push_entity`
        // refuses otherwise), and they arrive in the order the documents sit in the arena. The
        // sort is over one worker's accumulator, which the byte budget bounds, so it is a bounded
        // cost per run rather than a term-sized one at the merge.
        for postings in terms.values_mut() {
            postings.sort_unstable();
        }
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
/// never copied into the heap and the merge allocates nothing per record.
///
/// **The entities are merged, not concatenated.** They used to be concatenated: chunks partitioned
/// entity space ascending, the run list was held in chunk order, and a term's list was therefore
/// sorted for free. The chunk pass divides the *arena* now (`column.rs`), so a run holds entities
/// from all over entity space and two runs' lists interleave — [`Self::drain`] compares their
/// heads. What that costs is a comparison per posting against a copy per posting; what it buys is
/// a sequential read of a file larger than the machine.
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
    /// The selected runs' unconsumed heads, `(entity, run)`, smallest first. Held on the merge
    /// rather than built per term so a corpus's whole vocabulary costs one allocation.
    heads: std::collections::BinaryHeap<std::cmp::Reverse<(u32, usize)>>,
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
            heads: std::collections::BinaryHeap::new(),
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
    /// business, and what could go wrong here is the merge. It is also what catches a position
    /// carried by two runs — which cannot happen in either caller, because a text entity has
    /// exactly one live arena record and one window holds it, and a keyword row is visited once by
    /// one walk — and is exactly the failure a silent `>=` would hide. `encode_posting` re-checks
    /// the slice path, but the bitmap path has no such check to make — a `Bitmap` is a set — which
    /// is why the test lives here rather than there.
    fn drain(&mut self, sink: &mut impl FnMut(u32) -> Result<()>) -> Result<()> {
        self.heads.clear();
        for i in 0..self.selected.len() {
            let run = self.selected[i];
            if let Some(entity) = self.cursors[run].next_entity()? {
                self.heads.push(std::cmp::Reverse((entity, run)));
            }
        }
        let mut last: Option<u32> = None;
        while let Some(std::cmp::Reverse((entity, run))) = self.heads.pop() {
            if let Some(previous) = last {
                if entity <= previous {
                    return Err(BuildError::Invalid(format!(
                        "run merge: {entity} does not ascend past {previous} — two runs carry \
                         the same entity for one term, or the same row for one key"
                    )));
                }
            }
            last = Some(entity);
            sink(entity)?;
            if let Some(next) = self.cursors[run].next_entity()? {
                self.heads.push(std::cmp::Reverse((next, run)));
            }
        }
        Ok(())
    }
}

/// Runs opened at once by one merge — the text index's and the keyword column's alike, both
/// spilling the same run format.
///
/// **A merge holds a file descriptor per run, and the run count is a function of the corpus.** A
/// worker spills whenever its budget fills, so a corpus far larger than memory produces far more
/// runs than a process may hold open — which would be `EMFILE` at hour two on exactly the corpus
/// this whole shape exists to make buildable. Above the cap the runs are merged in passes: groups
/// of [`RUN_MERGE_FAN_IN`] into one intermediate run each, until what is left fits in one merge.
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
const RUN_MERGE_FAN_IN: usize = 128;

/// Reduce `receipts` to at most [`RUN_MERGE_FAN_IN`] runs, deleting each pass's inputs as it goes.
/// `family` names the intermediate files, so two column families cascading in one directory cannot
/// collide.
///
/// Groups are taken in order and each group merges in order, so the entity ordering the final
/// merge relies on survives every pass.
fn cascade_sorted_runs(
    column_dir: &Path,
    family: &str,
    mut receipts: Vec<spill::SpillReceipt>,
) -> Result<Vec<spill::SpillReceipt>> {
    let mut pass = 0usize;
    while receipts.len() > RUN_MERGE_FAN_IN {
        let mut merged = Vec::with_capacity(receipts.len().div_ceil(RUN_MERGE_FAN_IN));
        for (group, runs) in receipts.chunks(RUN_MERGE_FAN_IN).enumerate() {
            let path = column_dir.join(format!("{family}-merge-{pass:02}-{group:05}.spill"));
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
        // **An entity the column never reached is absent, which for this family is code 0.** The
        // presence bitmap is what a partially covered column spells absence with, and a category's
        // absence is the reserved code out of the value space (decision 0064,
        // `per-point-attributes.md` §3.6) — so the two spellings meet here, and both mean *no
        // posting and no value*. Reached by every column of a group-scoped family, where covering
        // fewer than every entity is the ordinary state rather than a symptom: a view holds its own
        // rows (`views.md` §5).
        ScalarValue::Null => Ok(tessera_store::vocabulary::ABSENT_CODE),
        other => Err(BuildError::Invalid(format!(
            "attribute '{column}' is declared for filtering but carries {other:?}, which is not a \
             category code"
        ))),
    }
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

/// How many source ids one view selects.
///
/// No limit and no selection ⇒ every row is selected ⇒ the metadata row count is exact and the
/// counting decode is a whole pass over the file for nothing. A form B source's rows are several
/// views', so the count there is data-dependent like a limit's.
fn count_source_ids(
    args: &BuildArgs,
    view: &crate::ViewArgs,
    id_space: &crate::ids::IdSpace,
) -> Result<usize> {
    match args.limit {
        None if view.select.is_none() => Ok(input::count_point_rows(&view.points)? as usize),
        _ => {
            let mut count = 0usize;
            input::scan_points(
                &view.points,
                &view.point_fields,
                view.projection,
                &view.extent,
                args.limit,
                view.select.as_ref(),
                id_space,
                |_| {
                    count += 1;
                    ControlFlow::Continue(())
                },
            )?;
            Ok(count)
        }
    }
}

/// The refusal a view's own id column earns when it names an entity twice.
///
/// A row is unique per `(external_id, view)`; the same id in two views is the ordinary case and is
/// collapsed into one entity. Stated once here because both routes through pass one make it.
fn duplicate_view_ids(view: &crate::ViewArgs) -> BuildError {
    BuildError::Invalid(format!(
        "view '{}': {} contains duplicate entity_id values. A row is unique per (entity, view) — \
         the same entity in several views is the ordinary case and is several files, never several \
         rows of one (views §4)",
        view.view_id,
        view.points.display()
    ))
}

/// The refusal a view earns when its points file yields a different number of rows than the count
/// pass read from it.
///
/// A points file rewritten between the two passes, which is [`input_changed`] rather than a short
/// read or a reallocated array: the count decides where the *next* view's ids start, and a wrong
/// one would give every item after it another item's ordinal.
fn view_row_count_changed(
    view: &crate::ViewArgs,
    counted: usize,
    written: usize,
    overran: bool,
) -> BuildError {
    let then = if overran {
        format!("more than {written}")
    } else {
        written.to_string()
    };
    input_changed(&format!(
        "view '{}': {} selected {counted} rows when it was counted and {then} when it was read",
        view.view_id,
        view.points.display(),
    ))
}

/// One view's selected source ids, written into `slot` in scan order (which is **no particular
/// order** — the decode is parallel; the caller sorts), and the view's own anchor.
///
/// `slot` is exactly what [`count_source_ids`] said this view holds, so the ids go straight into
/// the union's array and nothing here ever allocates.
fn read_source_ids_into(
    args: &BuildArgs,
    view: &crate::ViewArgs,
    slot: &mut [u64],
    id_space: &crate::ids::IdSpace,
) -> Result<ViewIdAnchor> {
    let mut written = 0usize;
    let mut mixed = 0u64;
    let mut overran = false;
    input::scan_points(
        &view.points,
        &view.point_fields,
        view.projection,
        &view.extent,
        args.limit,
        view.select.as_ref(),
        id_space,
        |point| {
            let Some(cell) = slot.get_mut(written) else {
                // Past the end of the slot: stop the scan rather than decode the rest of a file
                // whose answer is already a refusal.
                overran = true;
                return ControlFlow::Break(());
            };
            *cell = point.source_id;
            mixed = mixed.wrapping_add(mix64(point.source_id));
            written += 1;
            ControlFlow::Continue(())
        },
    )?;
    if overran || written != slot.len() {
        return Err(view_row_count_changed(view, slot.len(), written, overran));
    }
    Ok(ViewIdAnchor {
        rows: written as u64,
        mixed,
    })
}

/// **The build's ordinal space**: the sorted, duplicate-free source ids, as a range where pass one
/// proved the union is one, and as a file under `.build-tmp/` where it is not.
///
/// **8 bytes an item is the largest single structure the build would hold** — 26.0 GiB at the GBIF
/// rung's 3.50×10⁹ items — so the range arm is worth proving. Where it holds, no array is
/// allocated, none is sorted and none is written: an ordinal is `id - first` at every consumer
/// ([`Ids`]), and pass one's whole cost is a bit per id in the union's span.
///
/// Where it does not hold, the array is what every consumer reads, and it is read sequentially by
/// every pass but one: the join's merge sweep ([`join_chunk`]), the external-id write and the
/// ordinal walks. The one random reader is `layers::publish`'s binary search, which is the case
/// [`spill::MappedArray`] was written for. Mapped, those bytes are page cache the kernel may evict
/// rather than memory the machine must have, which is what `entity_of_ordinal`, the declared
/// columns, the member table and the text index's runs each became before it.
pub(crate) enum SourceIds {
    /// The union is `first..first + len`. No file was written.
    Contiguous { first: u64, len: usize },
    /// The union in a file, with the count that survived the deduplication.
    ///
    /// The array is allocated at the *pre-deduplication* length where the ids were read into it
    /// unsorted, because that is all that is known before they are read, and at the deduplicated
    /// length where the presence bitmap produced them already sorted. The slice is what survived
    /// either way, and the file keeps whatever slack the duplicates left until it is unlinked.
    Sparse {
        ids: spill::MappedArray<u64>,
        len: usize,
    },
}

impl SourceIds {
    fn ids(&self) -> Ids<'_> {
        match self {
            SourceIds::Contiguous { first, len } => Ids::Contiguous {
                first: *first,
                len: *len,
            },
            SourceIds::Sparse { ids, len } => Ids::Sparse(&ids.as_slice()[..*len]),
        }
    }

    fn len(&self) -> usize {
        match self {
            SourceIds::Contiguous { len, .. } => *len,
            SourceIds::Sparse { len, .. } => *len,
        }
    }

    /// The union's lowest and highest id, or `None` over no items.
    fn extrema(&self) -> Option<(u64, u64)> {
        match self {
            SourceIds::Contiguous { first, len } => {
                (*len > 0).then(|| (*first, first + *len as u64 - 1))
            }
            SourceIds::Sparse { ids, len } => {
                let slice = &ids.as_slice()[..*len];
                Some((*slice.first()?, *slice.last()?))
            }
        }
    }

    /// **What the file cost the disk**, and **zero where there is no file**: the range arm writes
    /// nothing. Where the ids were read into the array unsorted it is every view's rows and not
    /// the union's, the array being allocated at their sum and the dedup moving values inside it
    /// rather than shortening it. The disk pre-flight is charged over this and `n` is what
    /// survives ([`crate::residency::IdShape`]).
    pub(crate) fn slots(&self) -> usize {
        match self {
            SourceIds::Contiguous { .. } => 0,
            SourceIds::Sparse { ids, .. } => ids.as_slice().len(),
        }
    }
}

/// Collapse runs of equal values in a sorted slice, returning how many survived — [`Vec::dedup`]
/// over a slice whose length cannot change.
///
/// A value is written only where it moves. A sorted array with no duplicates — every corpus on
/// the ladder — is therefore read and not written, which for a mapped array is the difference
/// between dirtying every page of it and dirtying none.
fn dedup_sorted(values: &mut [u64]) -> usize {
    let mut written = 0usize;
    for read in 0..values.len() {
        if written > 0 && values[written - 1] == values[read] {
            continue;
        }
        if written != read {
            values[written] = values[read];
        }
        written += 1;
    }
    written
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

/// A bit per id over `[base, base + span)` — pass one's decision procedure, and on the range route
/// the only structure it builds.
///
/// It answers, exactly and at `span / 8` bytes: whether one view names an id twice, how many
/// distinct ids the union holds, what its lowest and highest are, and therefore whether the union
/// is one unbroken range. It does **not** answer `id → ordinal` for an arbitrary id, which is
/// `rank`, nor `ordinal → id`, which is `select`; a union that turns out not to be a range
/// therefore still needs the array, and the walk below writes it already sorted.
///
/// It cannot be allocated without a bound on the span, and the span is `max - min + 1` rather than
/// the item count. [`union_id_span`] is that bound and is also the gate: no bound, no bitmap.
struct IdPresence {
    base: u64,
    span: u64,
    words: Vec<u64>,
}

impl IdPresence {
    fn zeroed(base: u64, span: u64) -> IdPresence {
        IdPresence {
            base,
            span,
            // `alloc_zeroed`: for a span worth gating on, the allocator maps fresh zero pages
            // rather than clearing a block, so a per-view bitmap costs only the pages it touches.
            words: vec![0u64; (span as usize).div_ceil(64)],
        }
    }

    /// Mark an id present. `None` where the id is outside the span the statistics bounded, which
    /// abandons the route rather than indexing somewhere else; `Some(true)` where the bit was
    /// already set.
    fn set(&mut self, id: u64) -> Option<bool> {
        let offset = id.checked_sub(self.base).filter(|o| *o < self.span)?;
        let (word, bit) = ((offset / 64) as usize, offset % 64);
        let already = self.words[word] >> bit & 1 == 1;
        self.words[word] |= 1 << bit;
        Some(already)
    }

    fn union_with(&mut self, other: &IdPresence) {
        for (into, from) in self.words.iter_mut().zip(&other.words) {
            *into |= *from;
        }
    }

    fn count(&self) -> u64 {
        self.words.iter().map(|w| w.count_ones() as u64).sum()
    }

    /// The lowest and highest id present, or `None` where none is.
    ///
    /// Each is `base` plus the offset of a **set** bit, which is inside the span by construction,
    /// so neither sum can pass the end of the type. The offset is formed first for that reason: a
    /// span ending at `u64::MAX` has padding bits in its last word, and `base + word * 64 + 63`
    /// would name one of them before the subtraction brought it back.
    fn extrema(&self) -> Option<(u64, u64)> {
        let low = self
            .words
            .iter()
            .position(|w| *w != 0)
            .map(|w| self.base + (w as u64 * 64 + self.words[w].trailing_zeros() as u64))?;
        let high = self
            .words
            .iter()
            .rposition(|w| *w != 0)
            .map(|w| self.base + (w as u64 * 64 + 63 - self.words[w].leading_zeros() as u64))?;
        Some((low, high))
    }

    /// Every id present, ascending — the sorted, deduplicated union, produced by one sequential
    /// walk rather than by a sort.
    fn ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.words.iter().enumerate().flat_map(|(index, &word)| {
            let base = self.base + index as u64 * 64;
            std::iter::successors((word != 0).then_some(word), |w| {
                let rest = *w & (*w - 1);
                (rest != 0).then_some(rest)
            })
            .map(move |w| base + w.trailing_zeros() as u64)
        })
    }
}

/// The range the union's ids are known to lie in, where a bitmap over it is worth building.
///
/// Every points file states the lowest and highest `entity_id` it holds in its own parquet
/// statistics ([`input::id_bounds`]), and a `limit` selects `entity_id < limit`, so it bounds the
/// selection above. The union lies inside the fold of those bounds.
///
/// **The gate is `span <= total`**, `total` being the rows every view declares. A bitmap over the
/// span is `span / 8` bytes against the `8 * total` the array costs, so the gate makes it at most
/// a sixty-fourth of the file it stands in for, and it rejects a corpus whose ids are spread
/// rather than numbered: the `multiview` fixture's ten views hold 21,300 items over a span of
/// 13,463,247, which is 632 times its own row count, and a bitmap there would be 9.9 times the
/// live array. The condition is also necessary for the union to be a range at all where the
/// statistics are tight, a range of `n` distinct ids spanning exactly `n`.
///
/// `None` where any view cannot be bounded — no statistics, or an id the statistics cannot state
/// as a `u64` — which leaves the array as the only route. A view selecting no rows contributes
/// nothing to the fold.
fn union_id_span(
    args: &BuildArgs,
    counts: &[usize],
    id_space: &crate::ids::IdSpace,
) -> Result<Option<(u64, u64)>> {
    let total: u64 = counts.iter().map(|&count| count as u64).sum();
    if total == 0 {
        return Ok(None);
    }
    // **The supplied route's span is exact and needs no statistics.** A key's source id is its
    // rank among the keys this build interned, so the union is `0..n` (`crate::ids`). A
    // positional route's ids are the row numbers of the one file, which is the same span, read
    // from the count rather than from a footer that says nothing about a column that is not there.
    if let Some((low, high)) = id_space.rank_bounds() {
        return Ok(Some((low, high - low + 1)));
    }
    if id_space.positional() {
        return Ok(Some((0, total)));
    }
    let mut low = u64::MAX;
    let mut high = 0u64;
    for (view, &count) in args.views.iter().zip(counts) {
        if count == 0 {
            continue;
        }
        let Some((view_low, view_high)) = input::id_bounds(&view.points, &view.point_fields)?
        else {
            return Ok(None);
        };
        let view_high = match args.limit {
            Some(limit) => view_high.min(limit.saturating_sub(1)),
            None => view_high,
        };
        if view_high < view_low {
            // The limit excludes every id the file states while the count says rows were
            // selected: the two disagree, so take neither.
            return Ok(None);
        }
        low = low.min(view_low);
        high = high.max(view_high);
    }
    let Some(span) = high.checked_sub(low).and_then(|d| d.checked_add(1)) else {
        return Ok(None);
    };
    Ok((span <= total).then_some((low, span)))
}

/// Every view's ids as one presence bitmap over `[base, base + span)`, with each view's own anchor.
///
/// A bitmap per view, folded into the union as each is read: a bit already set **within one view**
/// is the duplicate refusal, and a bit already set by an earlier view is the ordinary case. That
/// is what the per-view sort and `windows(2)` decide on the array route, decided here without an
/// order.
///
/// `None` where a file holds an id outside the span its own statistics stated. The route is
/// abandoned and the array route reads the files again, which is a points file disagreeing with
/// its own footer rather than anything this build can repair.
fn read_source_ids_present(
    args: &BuildArgs,
    counts: &[usize],
    base: u64,
    span: u64,
    id_space: &crate::ids::IdSpace,
) -> Result<Option<(IdPresence, Vec<ViewIdAnchor>)>> {
    let mut union: Option<IdPresence> = None;
    let mut anchors = Vec::with_capacity(args.views.len());
    for (view, &count) in args.views.iter().zip(counts) {
        // A view selecting nothing gets no bitmap: any row the scan yields is past its count and
        // is the refusal below, and a span-sized allocation for it would be the whole cost of the
        // route paid for no ids.
        let mut bits = (count > 0).then(|| IdPresence::zeroed(base, span));
        let mut written = 0usize;
        let mut mixed = 0u64;
        let mut overran = false;
        let mut outside = false;
        let mut duplicate = false;
        input::scan_points(
            &view.points,
            &view.point_fields,
            view.projection,
            &view.extent,
            args.limit,
            view.select.as_ref(),
            id_space,
            |point| {
                let Some(bits) = bits.as_mut().filter(|_| written < count) else {
                    overran = true;
                    return ControlFlow::Break(());
                };
                match bits.set(point.source_id) {
                    None => {
                        outside = true;
                        return ControlFlow::Break(());
                    }
                    Some(true) => {
                        duplicate = true;
                        return ControlFlow::Break(());
                    }
                    Some(false) => {}
                }
                mixed = mixed.wrapping_add(mix64(point.source_id));
                written += 1;
                ControlFlow::Continue(())
            },
        )?;
        if outside {
            return Ok(None);
        }
        if duplicate {
            return Err(duplicate_view_ids(view));
        }
        if overran || written != count {
            return Err(view_row_count_changed(view, count, written, overran));
        }
        anchors.push(ViewIdAnchor {
            rows: written as u64,
            mixed,
        });
        if let Some(bits) = bits {
            union = Some(match union {
                None => bits,
                Some(mut union) => {
                    union.union_with(&bits);
                    union
                }
            });
        }
    }
    Ok(union.map(|union| (union, anchors)))
}

/// The ordinal space the presence bitmap decided: a range where the union is one, and the array
/// walked out of the bitmap where it is not.
///
/// The walk emits set bits ascending, so the array it writes is already sorted and already
/// deduplicated — no `par_sort_unstable` over the corpus, and it is allocated at the distinct
/// count rather than at the views' pre-deduplication sum.
fn source_ids_of_presence(present: &IdPresence, tmp: &Path) -> Result<SourceIds> {
    let n = present.count();
    // `first` and `last` both lie in the span, so the width cannot overflow. An empty union has no
    // extrema and takes the array arm, which answers nothing rather than subtracting from a first
    // id that does not exist.
    if let Some((first, last)) = present.extrema() {
        if last - first + 1 == n {
            return Ok(SourceIds::Contiguous {
                first,
                len: n as usize,
            });
        }
    }
    let mut ids = spill::MappedArray::<u64>::zeroed(tmp, "source-ids.u64", n as usize)?;
    let slots = ids.as_mut_slice();
    let mut written = 0usize;
    for id in present.ids() {
        slots[written] = id;
        written += 1;
    }
    // An `assert` rather than a `debug_assert`: a short walk would leave trailing zeros in the
    // array, which resolve as ordinals and move every entity id the build assigns (I9). It is one
    // comparison after a walk over the whole bitmap.
    assert_eq!(written as u64, n, "the walk writes one id per set bit");
    Ok(SourceIds::Sparse {
        ids,
        len: n as usize,
    })
}

/// **Pass one's entity space** (`views.md` §7): every view's point source, unioned by
/// `external_id`.
///
/// A row is unique per `(external_id, view)` — an id repeated *within* one view's file is the
/// duplicate refusal, while the same id in two views is the ordinary case and is what makes an
/// entity's identity, label and attributes shared across the row spaces.
///
/// **Two routes to the same union.** Where every view's points file bounds its own ids and the
/// union's span is no larger than the rows the views declare ([`union_id_span`]), the ids go into
/// a presence bitmap a sixty-fourth of the array's size, and a union that turns out to be one
/// unbroken range needs no array at all: an ordinal is a subtraction and pass one writes nothing.
/// At the GBIF rung that is 28 GB of writes and one sort of 3.5×10⁹ `u64`s not made. A union that
/// is not a range is walked out of the bitmap into an array that is already sorted and already
/// deduplicated.
///
/// Otherwise the ids go into the array directly, each view's segment sorted and duplicate-checked
/// in place and the whole sorted and deduplicated after. **Every view is counted before any is
/// read**, so the union is one array of the final length rather than a concatenation holding both
/// copies while it ran — 16 bytes an item where the thing it produces is 8, which is 52.1 GiB at
/// the GBIF rung against a 47 GiB machine (`probes/2026-09-10-source-ids-memory/`).
fn read_source_ids_union(
    args: &BuildArgs,
    tmp: &Path,
    id_space: &crate::ids::IdSpace,
) -> Result<(SourceIds, Vec<ViewIdAnchor>)> {
    let mut counts = Vec::with_capacity(args.views.len());
    for view in &args.views {
        counts.push(count_source_ids(args, view, id_space)?);
    }
    if let Some((base, span)) = union_id_span(args, &counts, id_space)? {
        if let Some((present, anchors)) =
            read_source_ids_present(args, &counts, base, span, id_space)?
        {
            return Ok((source_ids_of_presence(&present, tmp)?, anchors));
        }
    }
    let total: usize = counts.iter().sum();
    let mut ids = spill::MappedArray::<u64>::zeroed(tmp, "source-ids.u64", total)?;
    let mut anchors = Vec::with_capacity(args.views.len());
    let mut offset = 0usize;
    for (view, &count) in args.views.iter().zip(&counts) {
        let segment = &mut ids.as_mut_slice()[offset..offset + count];
        anchors.push(read_source_ids_into(args, view, segment, id_space)?);
        segment.par_sort_unstable();
        if segment.windows(2).any(|w| w[0] == w[1]) {
            return Err(duplicate_view_ids(view));
        }
        offset += count;
    }
    let all = ids.as_mut_slice();
    all.par_sort_unstable();
    let len = dedup_sorted(all);
    Ok((SourceIds::Sparse { ids, len }, anchors))
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
    ids: Ids<'_>,
    dict_dir: &std::path::Path,
    id_space: &crate::ids::IdSpace,
) -> Result<Dictionary> {
    // Chunk-order insensitivity ([`join_chunk`]): per-term min-ordinal and row counts, and the
    // per-range histogram, are commutative aggregations — no arrival order is observable.
    let mut first_ordinal: FxHashMap<u64, (u64, u64)> = FxHashMap::default();
    // Ordinal-range histogram for batch sizing: 2^16 ranges regardless of n.
    let histogram_shift = (64 - (ids.len().max(1) as u64).leading_zeros()).saturating_sub(16);
    let mut histogram = vec![0u64; (ids.len() >> histogram_shift) + 1];
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
        join_chunk(chunk, ids, |ordinal, source_id, source_term| {
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
    let fill = crate::scan_access(args, access, id_space, |_view, source_id, source_term| {
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

    /// **The extent route is decided by the order a column's readers want, not by its type.**
    ///
    /// The entity-indexed offset array beside an arena answers `entity → value` at random, and
    /// the one pass that asks a string column that question is the hot row tail. Every other
    /// reader — the record blob's merge, a `text` column's token index, a keyword dictionary's
    /// chunk walk — reads the column once, ascending, which an extent merge answers. So every
    /// bundle-wide string column has two routes and `render` is what closes one.
    ///
    /// **And every column routed there has a reader for its extents.** Extents nothing reads
    /// would be a column stored nowhere: a blob-resident column's are merged into the blob, and
    /// an indexed one's are its dictionary pass's.
    #[test]
    fn the_extent_route_is_the_readers_and_not_the_type() {
        let declared = |ty, index, render| crate::config::Attribute {
            name: "column".to_string(),
            field: None,
            title: None,
            ty,
            analyser: None,
            vocabulary: None,
            value_set: None,
            index,
            render,
        };
        let schema = |attribute| crate::config::Schema {
            attributes: vec![attribute],
            vocabularies: Default::default(),
        };
        let cases = [
            // A string column with no reader at an entity: the blob's, and spilled.
            (ScalarType::Keyword, false, false, true),
            (ScalarType::Utf8, false, false, true),
            (ScalarType::Text, false, false, true),
            // Indexed: `text` owes a token index over the extents, and the other two owe a value
            // column and a dictionary, whose one pass reads the extents in entity order.
            (ScalarType::Text, true, false, true),
            (ScalarType::Keyword, true, false, true),
            (ScalarType::Utf8, true, false, true),
            // Rendered: the row tail reads the column at an entity. ⊘ `render` on a string type
            // is refused at the declaration and `write_columns` refuses one that arrives anyway,
            // so this arm is reachable only from a `Schema` built programmatically.
            (ScalarType::Keyword, false, true, false),
            // A fixed-width column is a slot and is never spilled, whatever its flags say.
            (ScalarType::U32, false, false, false),
            (ScalarType::U32, true, false, false),
            (ScalarType::F64, false, true, false),
        ];
        for (ty, index, render, expected) in cases {
            let schema = schema(declared(ty, index, render));
            let attribute = &schema.attributes[0];
            assert_eq!(
                may_take_extents(&schema, attribute),
                expected,
                "{ty:?} index={index} render={render}"
            );
            if may_take_extents(&schema, attribute) {
                assert!(
                    blob_resident(&schema, attribute) || value_column_is_owed(&schema, attribute),
                    "{ty:?} index={index} render={render}: spilled with no reader for the extents"
                );
                // The arena route is closed to `text` and open to the rest: the second decode a
                // `text` arena costs is the source permutation and not the arena's size
                // (`build-column-extents.md` §2).
                assert_eq!(
                    extents_are_forced(&schema, attribute),
                    ty == ScalarType::Text,
                    "{ty:?} index={index} render={render}: the wrong route is the unconditional one"
                );
            }
        }
    }

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
        // One entity; values are per column, in declaration order. The keyword's value arrives
        // as an extent rather than as a column, [`ColumnRoutes`] having routed it there, and the
        // blob's answer is what this test is about either way.
        let by_entity = vec![
            EntityColumn::from_values(&scratch, ScalarType::U16, [ScalarValue::U16(7)], "colour")
                .expect("typed column"),
            EntityColumn::spilled(&scratch, ScalarType::Keyword, 1).expect("the note slot"),
        ];
        let mut notes = crate::extents::ExtentColumn::new(dir.path(), 1, "note");
        notes.push_extent(&[(0u32, "kept")]).expect("an extent");
        let open = [notes.open().expect("the extents open")];
        let written = write_record_blob(
            dir.path(),
            &schema,
            by_entity[0].len() as u64,
            &ColumnRoutes::every_available(&schema),
            &by_entity,
            &open,
        )
        .expect("blob stage writes");
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
        let written = write_record_blob(
            dir.path(),
            &schema,
            only_category[0].len() as u64,
            &ColumnRoutes::every_available(&schema),
            &only_category,
            &[],
        )
        .expect("blob stage accepts");
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
        let written = write_record_blob(
            dir.path(),
            &schema,
            only_category[0].len() as u64,
            &ColumnRoutes::every_available(&schema),
            &only_category,
            &[],
        )
        .expect("blob stage writes");
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

    /// **The cascade is not observable either.** A column that spilled more extents than one
    /// merge holds open folds them in groups first, and the fold has to leave the same relation:
    /// the same entities, each carrying the last value written for it, in the same order.
    #[test]
    fn folding_the_extents_leaves_the_same_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fan_in = 8usize;
        let extents = fan_in * 2 + 3;
        let mut spilled = crate::extents::ExtentColumn::new(dir.path(), 0, "abstract");
        // One row per extent, walking entity space in a stride so the extents interleave, plus a
        // second value for one entity in a later extent than the one that first carried it.
        for extent in 0..extents {
            let entity = ((extent * 7) % extents) as u32;
            let value = format!("value {extent}");
            spilled
                .push_extent(&[(entity, value.as_str())])
                .expect("an extent");
        }
        spilled
            .push_extent(&[(3u32, "the later value")])
            .expect("an extent");
        let before = prose_rows(&spilled);
        spilled.cascade(fan_in).expect("the cascade");
        assert_eq!(before, prose_rows(&spilled), "the fold moved a value");
        assert_eq!(before.get(&3).map(String::as_str), Some("the later value"));
    }

    /// Every live `(entity, value)` a spilled column holds, which is what both its readers see.
    fn prose_rows(
        column: &crate::extents::ExtentColumn,
    ) -> std::collections::BTreeMap<u32, String> {
        let open = column.open().expect("the extents open");
        let mut rows: std::collections::BTreeMap<u32, String> = Default::default();
        for window in open.windows(3) {
            open.for_each_record_in(window, &mut |entity, value| {
                rows.insert(entity as u32, value.to_string());
                Ok(())
            })
            .expect("the walk");
        }
        rows
    }

    /// **The join's chunking must not be observable in the record blob.** A chunk is one extent
    /// per spilled column, so a corpus joined in one chunk and the same corpus joined in chunks of
    /// three have to be the same three files. What that pins is the merge: an entity's fields
    /// gathered from two families of extents and an entity-ordered column, ordered by tag, with
    /// the last chunk that carried a value winning.
    ///
    /// **Two spilled families and a column**, because that is the shape [`ColumnRoutes`] routes
    /// on a real schema: a `keyword` column with neither `index` nor `render` takes the extents a
    /// `text` column takes, and a fixed-width column with the same flags stays where it was. A
    /// tag reaching the merge from two sources at one entity would be a value overwriting
    /// another's.
    ///
    /// The corpus straddles what the merge has to answer. Entities carry a count, a keyword,
    /// prose, several of those or none; one prose value is the empty string, which is a value and
    /// not absence; two entities are written twice, one of them across a chunk boundary at
    /// several of the chunkings and one within a chunk at all of them.
    #[test]
    fn the_extents_do_not_change_the_blobs_bytes() {
        const N: usize = 64;
        let scratch_dir = tempfile::tempdir().expect("tempdir");
        let scratch = crate::column::ColumnScratch::new(scratch_dir.path());
        let string_column = |name: &str, ty: ScalarType| crate::config::Attribute {
            name: name.to_string(),
            field: None,
            title: None,
            ty,
            analyser: None,
            vocabulary: None,
            value_set: None,
            index: false,
            render: false,
        };
        let schema = crate::config::Schema {
            attributes: vec![
                string_column("count", ScalarType::U32),
                string_column("note", ScalarType::Keyword),
                string_column("abstract", ScalarType::Text),
            ],
            vocabularies: Default::default(),
        };
        for attribute in &schema.attributes[1..] {
            assert!(
                may_take_extents(&schema, attribute),
                "'{}' is what this test is about",
                attribute.name
            );
        }
        assert!(
            !may_take_extents(&schema, &schema.attributes[0]),
            "a fixed-width column stays a column"
        );

        // The fixed-width column is entity-ordered as it always was: a third of the entities carry
        // a count.
        let counts: Vec<ScalarValue> = (0..N)
            .map(|entity| match entity % 3 {
                0 => ScalarValue::U32(entity as u32 * 10),
                _ => ScalarValue::Null,
            })
            .collect();

        // Each spilled column as its own source yields it: entity order is not source order, one
        // entity is written twice adjacently and one far apart, and one prose value is the empty
        // string. The two sources disagree about which entities they cover and about the order
        // they arrive in, so no chunking aligns their extents.
        let mut prose_source: Vec<(u32, String)> = (0..N)
            .filter(|entity| entity % 5 != 1)
            .map(|entity| {
                let value = if entity == 20 {
                    String::new()
                } else {
                    format!("prose for {entity}, long enough to be worth a block")
                };
                ((entity * 37 % N) as u32, value)
            })
            .collect();
        prose_source.push((prose_source[3].0, "the later value, adjacent".to_string()));
        prose_source.insert(
            2,
            (
                prose_source[9].0,
                "the earlier value, far apart".to_string(),
            ),
        );
        let mut note_source: Vec<(u32, String)> = (0..N)
            .filter(|entity| entity % 4 != 3)
            .map(|entity| ((entity * 11 % N) as u32, format!("note-{entity}")))
            .collect();
        note_source.push((note_source[7].0, "the later note, adjacent".to_string()));
        note_source.insert(
            1,
            (note_source[19].0, "the earlier note, far apart".to_string()),
        );

        let blob_of = |dir: &Path, chunk: usize| -> Vec<PathBuf> {
            let extent_dir = dir.join("extents");
            std::fs::create_dir_all(&extent_dir).expect("extent dir");
            let notes = spilled_column(&extent_dir, 1, "note", &note_source, chunk);
            let prose = spilled_column(&extent_dir, 2, "abstract", &prose_source, chunk);
            let open = [
                notes.open().expect("the extents open"),
                prose.open().expect("the extents open"),
            ];
            let by_entity = [
                EntityColumn::from_values(
                    &scratch,
                    ScalarType::U32,
                    counts.iter().cloned(),
                    "count",
                )
                .expect("typed column"),
                EntityColumn::spilled(&scratch, ScalarType::Keyword, N).expect("the note slot"),
                EntityColumn::spilled(&scratch, ScalarType::Text, N).expect("the prose slot"),
            ];
            write_record_blob(
                dir,
                &schema,
                N as u64,
                &ColumnRoutes::every_available(&schema),
                &by_entity,
                &open,
            )
            .expect("the blob merges")
        };

        let mut written: Vec<Vec<(PathBuf, Vec<u8>)>> = Vec::new();
        for chunk in [1usize, 2, 3, 5, 7, 64, 4_096] {
            let dir = tempfile::tempdir().expect("tempdir");
            let paths = blob_of(dir.path(), chunk);
            assert_eq!(paths.len(), 3, "the blob is three files");
            written.push(
                paths
                    .iter()
                    .map(|path| {
                        (
                            path.strip_prefix(dir.path()).expect("under the dir").into(),
                            std::fs::read(path).expect("read back"),
                        )
                    })
                    .collect(),
            );
        }
        let first = written.first().expect("at least one chunking").clone();
        for (k, emitted) in written.iter().enumerate().skip(1) {
            assert_eq!(emitted, &first, "chunking {k} wrote different bytes");
        }

        // And the merged blob says what the corpus says.
        let dir = tempfile::tempdir().expect("tempdir");
        blob_of(dir.path(), 3);
        let blob = tessera_filter::RecordBlob::open_dir(
            &dir.path().join("attrs/record"),
            tessera_filter::Access::Read,
        )
        .expect("open");
        // The last value written for an entity is the one the blob holds.
        let last_of = |source: &[(u32, String)]| {
            let mut held: std::collections::BTreeMap<u32, String> = Default::default();
            for (entity, value) in source {
                held.insert(*entity, value.clone());
            }
            held
        };
        let expected_prose = last_of(&prose_source);
        let expected_note = last_of(&note_source);
        for entity in 0..N as u32 {
            let fields = blob.fields_of(entity).expect("read");
            let utf8_at = |tag: u16| -> Option<String> {
                fields.as_ref().and_then(|fields| {
                    fields
                        .iter()
                        .find(|field| field.tag == tag)
                        .map(|field| match &field.value {
                            tessera_filter::RecordValue::Utf8(value) => value.clone(),
                            other => panic!("entity {entity} carries {other:?} at tag {tag}"),
                        })
                })
            };
            assert_eq!(
                utf8_at(2),
                expected_prose.get(&entity).cloned(),
                "entity {entity}'s prose"
            );
            assert_eq!(
                utf8_at(1),
                expected_note.get(&entity).cloned(),
                "entity {entity}'s note"
            );
            let count = fields.as_ref().and_then(|fields| {
                fields
                    .iter()
                    .find(|field| field.tag == 0)
                    .map(|field| field.value.clone())
            });
            let expected = match entity % 3 {
                0 => Some(tessera_filter::RecordValue::U32(entity * 10)),
                _ => None,
            };
            assert_eq!(count, expected, "entity {entity}'s count");
        }
    }

    /// One column's source rows spilled as extents of `chunk` rows each, the way the join spills
    /// them: sorted by entity within the chunk, stably, with the last row carrying a value
    /// winning. Spelt out rather than calling [`spill_extent_chunk`], so that the test states the
    /// rule it is asserting about.
    fn spilled_column(
        dir: &Path,
        column: usize,
        name: &str,
        source: &[(u32, String)],
        chunk: usize,
    ) -> crate::extents::ExtentColumn {
        let mut spilled = crate::extents::ExtentColumn::new(dir, column, name);
        for rows in source.chunks(chunk) {
            let mut held: Vec<(u32, &str)> = rows
                .iter()
                .map(|(entity, value)| (*entity, value.as_str()))
                .collect();
            held.sort_by_key(|&(entity, _)| entity);
            let mut kept: Vec<(u32, &str)> = Vec::new();
            for row in held {
                if kept.last().map(|&(entity, _)| entity) == Some(row.0) {
                    kept.pop();
                }
                kept.push(row);
            }
            spilled.push_extent(&kept).expect("an extent");
        }
        spilled
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
        // The two producers over the same corpus: the scoped column's arena, and the extents the
        // join spills for a bundle-wide column. One, three and eleven of them, interleaved.
        let extent_dir = tempfile::tempdir().expect("tempdir");
        let spilled: Vec<crate::extents::ExtentColumn> = [1usize, 3, 11]
            .into_iter()
            .map(|extents| {
                let dir = extent_dir.path().join(format!("extents-{extents}"));
                std::fs::create_dir_all(&dir).expect("extent dir");
                text_fixture_extents(&dir, extents)
            })
            .collect();
        let open: Vec<crate::extents::OpenExtents> = spilled
            .iter()
            .map(|column| column.open().expect("the extents open"))
            .collect();
        let sources: Vec<TextValues<'_>> = std::iter::once(TextValues::Arena(&values))
            .chain(open.iter().map(TextValues::Extents))
            .collect();

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
            for (source, values) in sources.iter().enumerate() {
                let column_dir = dir.path().join(format!("plan-{k}-{source}"));
                std::fs::create_dir_all(&column_dir).expect("column dir");
                let values = match values {
                    TextValues::Arena(column) => TextValues::Arena(column),
                    TextValues::Extents(prose) => TextValues::Extents(prose),
                };
                let written =
                    write_text_index(&column_dir, &attribute, values, plan).expect("text index");
                assert_eq!(
                    written.terms,
                    expected_text_postings().len() as u64,
                    "plan {k} source {source} wrote the wrong term count"
                );
                let left: Vec<String> = std::fs::read_dir(&column_dir)
                    .expect("read dir")
                    .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                    .filter(|name| name.ends_with(".spill"))
                    .collect();
                assert!(left.is_empty(), "plan {k} source {source} left {left:?}");
                files.push((
                    std::fs::read(column_dir.join(tessera_filter::DICT_FILE)).expect("dict"),
                    std::fs::read(column_dir.join("postings.arrow")).expect("postings"),
                ));
            }
            let column_dir = dir.path().join(format!("plan-{k}"));
            std::fs::create_dir_all(&column_dir).expect("column dir");
            let written =
                write_text_index(&column_dir, &attribute, TextValues::Arena(&values), plan)
                    .expect("text index");
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
            TextValues::Arena(&values),
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

    /// The corpus both keyword tests read. Every shape the dictionary pass has to survive: keys
    /// shorter than the eight bytes [`KeywordPair`] compares on, keys agreeing in exactly those
    /// eight and differing after them, a key that is a proper prefix of another, a key carried by
    /// a seventh of the corpus, keys carried by exactly one entity, a non-ASCII key, and absent
    /// runs.
    const N_KEYWORD: usize = 300;

    fn keyword_fixture_key(entity: usize) -> ScalarValue {
        if entity.is_multiple_of(11) {
            // Non-ASCII, and its ordinal is decided by its bytes like every other key's.
            return ScalarValue::Utf8("ünïcode-ключ".to_string());
        }
        match entity % 7 {
            // Absence, every seventh entity and so never aligned to the 64-entity words
            // presence is stored in.
            3 => ScalarValue::Null,
            0 => ScalarValue::Utf8("a".to_string()),
            1 => ScalarValue::Utf8("ab".to_string()),
            // A proper prefix of the keys below it, and a key of exactly eight bytes.
            2 => ScalarValue::Utf8("prefix-".to_string()),
            4 => ScalarValue::Utf8(format!("prefix-{:04}", entity % 5)),
            5 => ScalarValue::Utf8("12345678".to_string()),
            // Distinct per entity, and all of them agreeing in their first eight characters.
            _ => ScalarValue::Utf8(format!("uuid-000{entity:05}")),
        }
    }

    fn keyword_fixture_attribute() -> crate::config::Attribute {
        crate::config::Attribute {
            name: "key".to_string(),
            field: None,
            title: None,
            ty: ScalarType::Keyword,
            analyser: None,
            vocabulary: None,
            value_set: None,
            index: true,
            render: false,
        }
    }

    fn keyword_fixture_column(scratch: &crate::column::ColumnScratch) -> EntityColumn {
        EntityColumn::from_values(
            scratch,
            ScalarType::Keyword,
            (0..N_KEYWORD).map(keyword_fixture_key),
            "key",
        )
        .expect("typed column")
    }

    /// The distinct keys of the fixture, sorted — the dictionary the pass owes, derived here the
    /// way the pass no longer may: whole, in memory, from the corpus.
    fn keyword_fixture_keys() -> Vec<String> {
        let mut keys: Vec<String> = (0..N_KEYWORD)
            .filter_map(|entity| match keyword_fixture_key(entity) {
                ScalarValue::Utf8(key) => Some(key),
                _ => None,
            })
            .collect();
        keys.sort();
        keys.dedup();
        keys
    }

    /// The chunk sizes both keyword tests run under. `1` spills a run per row and cascades — 257
    /// runs over a fan-in of 128 — and `4_096` is one chunk for the whole column; the rest divide
    /// the corpus, and do not, and align to the 64-entity words presence is stored in, and do not.
    const KEYWORD_CHUNKS: [usize; 7] = [1, 2, 3, 7, 64, 300, 4_096];

    /// **The route a keyword column's characters take is not in its artefacts.** Its dictionary,
    /// its ordinals and its presence bitmap are the same three files whether the pass reads an
    /// entity-ordered arena or the extents the join spilled, at every extent chunking.
    ///
    /// The source is what makes this worth asserting rather than assuming. Entity order is not
    /// source order; one entity is written twice next to itself and one far apart, so which value
    /// an entity ends with is the arena's last write on one route and the extents' live sets on
    /// the other; and the chunkings put those two rows in one extent and in different ones.
    /// `tests/extent_route.rs` holds the same property over a whole bundle; what is here is the
    /// case a corpus of 400 rows in one chunk cannot reach.
    #[test]
    fn the_keyword_column_is_the_same_files_from_an_arena_and_from_extents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scratch_dir = tempfile::tempdir().expect("tempdir");
        let scratch = crate::column::ColumnScratch::new(scratch_dir.path());
        let attribute = keyword_fixture_attribute();

        // The source rows, in an order that is not entity order, with two entities written twice.
        let mut source: Vec<(u32, String)> = (0..N_KEYWORD)
            .filter_map(|entity| match keyword_fixture_key(entity) {
                ScalarValue::Utf8(key) => Some(((entity * 37 % N_KEYWORD) as u32, key)),
                _ => None,
            })
            .collect();
        source.push((source[5].0, "the later key, adjacent".to_string()));
        source.insert(2, (source[40].0, "the earlier key, far apart".to_string()));

        // The arena route's column: the same rows scattered in order, so the last write for an
        // entity is the value, which is what `EntityColumn::set` does in the join.
        let mut column =
            EntityColumn::filled(&scratch, ScalarType::Keyword, N_KEYWORD).expect("typed column");
        for (entity, key) in &source {
            column
                .set(*entity as usize, ScalarValue::Utf8(key.clone()), "key")
                .expect("set");
        }

        let files_of = |column_dir: &Path,
                        values: &EntityColumn,
                        extents: Option<&crate::extents::OpenExtents>| {
            std::fs::create_dir_all(column_dir).expect("column dir");
            let values_path = column_dir.join("values.arrow");
            let presence_path = column_dir.join(tessera_filter::PRESENCE_FILE);
            let written = write_column_values(
                column_dir,
                &values_path,
                &presence_path,
                &attribute,
                values,
                extents,
                KeywordDictPlan::explicit(7),
            )
            .expect("keyword column");
            assert!(written.presence, "the fixture has absent entities");
            let dict_path = written.dict.expect("a keyword column owes a dictionary");
            [
                std::fs::read(&dict_path).expect("dict"),
                std::fs::read(&values_path).expect("values"),
                std::fs::read(&presence_path).expect("presence"),
            ]
        };

        let from_arena = files_of(&dir.path().join("arena"), &column, None);
        let empty =
            EntityColumn::spilled(&scratch, ScalarType::Keyword, N_KEYWORD).expect("the slot");
        for chunk in [1usize, 2, 3, 7, 64, 4_096] {
            let extent_dir = dir.path().join(format!("extents-{chunk}"));
            std::fs::create_dir_all(&extent_dir).expect("extent dir");
            let spilled = spilled_column(&extent_dir, 0, "key", &source, chunk);
            let open = spilled.open().expect("the extents open");
            let from_extents = files_of(
                &dir.path().join(format!("spilled-{chunk}")),
                &empty,
                Some(&open),
            );
            assert_eq!(
                from_extents, from_arena,
                "extents in chunks of {chunk} wrote different files"
            );
        }
    }

    /// **The chunking must not be observable in the artefacts.** A chunk boundary is a place one
    /// buffer's rows end and the next one's begin, so a column emitted in one chunk and the same
    /// column emitted in chunks of three have to be the same three files. That is what makes
    /// [`KeywordDictPlan`] a memory knob rather than a format decision, and it is what an
    /// off-by-one at a boundary fails: a key whose rows split across two runs and lost half of
    /// them changes only the bytes.
    #[test]
    fn chunking_the_keyword_column_does_not_change_its_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scratch_dir = tempfile::tempdir().expect("tempdir");
        let scratch = crate::column::ColumnScratch::new(scratch_dir.path());
        let attribute = keyword_fixture_attribute();
        let values = keyword_fixture_column(&scratch);

        let mut emitted: Vec<[Vec<u8>; 3]> = Vec::new();
        for (k, chunk_rows) in KEYWORD_CHUNKS.into_iter().enumerate() {
            let column_dir = dir.path().join(format!("plan-{k}"));
            std::fs::create_dir_all(&column_dir).expect("column dir");
            let values_path = column_dir.join("values.arrow");
            let presence_path = column_dir.join(tessera_filter::PRESENCE_FILE);
            let written = write_column_values(
                &column_dir,
                &values_path,
                &presence_path,
                &attribute,
                &values,
                None,
                KeywordDictPlan::explicit(chunk_rows),
            )
            .expect("keyword column");
            assert!(written.presence, "the fixture has absent entities");
            let dict_path = written.dict.expect("a keyword column owes a dictionary");
            // The three artefacts and nothing else: every run this plan spilled, every
            // intermediate its cascade wrote, and the ordinal scratch are all gone.
            let mut left: Vec<String> = std::fs::read_dir(&column_dir)
                .expect("read dir")
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .filter(|name| {
                    name != "values.arrow"
                        && name != tessera_filter::PRESENCE_FILE
                        && name != tessera_filter::DICT_FILE
                })
                .collect();
            left.sort();
            assert!(left.is_empty(), "chunk {chunk_rows} left {left:?} behind");
            emitted.push([
                std::fs::read(&dict_path).expect("dict"),
                std::fs::read(&values_path).expect("values"),
                std::fs::read(&presence_path).expect("presence"),
            ]);
        }
        let first = emitted.first().expect("at least one plan").clone();
        for (k, files) in emitted.iter().enumerate().skip(1) {
            assert_eq!(files[0], first[0], "chunk plan {k}'s dictionary differs");
            assert_eq!(files[1], first[1], "chunk plan {k}'s values differ");
            assert_eq!(files[2], first[2], "chunk plan {k}'s presence differs");
        }
    }

    /// And the column says what the corpus says: the dictionary is the sorted distinct key set,
    /// and every entity's ordinal names its own key — read back through the readers that will
    /// serve them, under every chunking, so what is asserted is the merge's output and not one
    /// chunk's buffer.
    #[test]
    fn the_keyword_column_names_every_entity_s_own_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scratch_dir = tempfile::tempdir().expect("tempdir");
        let scratch = crate::column::ColumnScratch::new(scratch_dir.path());
        let attribute = keyword_fixture_attribute();
        let values = keyword_fixture_column(&scratch);
        let expected = keyword_fixture_keys();

        for chunk_rows in KEYWORD_CHUNKS {
            let column_dir = dir.path().join(format!("chunk-{chunk_rows}"));
            std::fs::create_dir_all(&column_dir).expect("column dir");
            let values_path = column_dir.join("values.arrow");
            let presence_path = column_dir.join(tessera_filter::PRESENCE_FILE);
            write_column_values(
                &column_dir,
                &values_path,
                &presence_path,
                &attribute,
                &values,
                None,
                KeywordDictPlan::explicit(chunk_rows),
            )
            .expect("keyword column");

            let dict = tessera_filter::SortedDict::open(
                &column_dir.join(tessera_filter::DICT_FILE),
                tessera_filter::Access::Read,
            )
            .expect("the dictionary opens");
            assert_eq!(dict.len() as usize, expected.len(), "chunk {chunk_rows}");
            let mut scratch_key = Vec::new();
            for (ordinal, key) in expected.iter().enumerate() {
                assert_eq!(
                    dict.key_of(ordinal as u32, &mut scratch_key)
                        .expect("the key resolves"),
                    key,
                    "chunk {chunk_rows}: ordinal {ordinal}"
                );
            }

            let column = tessera_filter::ValueColumn::open(
                &values_path,
                Some(&presence_path),
                tessera_filter::Access::Read,
            )
            .expect("the value column opens");
            for entity in 0..N_KEYWORD {
                let ordinal = column.value_of(entity as u32).map(|value| value.raw());
                match keyword_fixture_key(entity) {
                    ScalarValue::Utf8(key) => {
                        let ordinal = ordinal.expect("a present entity has a value");
                        assert_eq!(
                            dict.key_of(ordinal, &mut scratch_key)
                                .expect("the key resolves"),
                            key,
                            "chunk {chunk_rows}: entity {entity}"
                        );
                    }
                    _ => assert_eq!(ordinal, None, "chunk {chunk_rows}: entity {entity}"),
                }
            }
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

    /// The fixture's prose spilled as `extents` blob extents, assigned round-robin by entity so
    /// that the extents interleave in entity space exactly as a join's chunks do.
    fn text_fixture_extents(dir: &Path, extents: usize) -> crate::extents::ExtentColumn {
        let mut column = crate::extents::ExtentColumn::new(dir, 0, "abstract");
        for extent in 0..extents {
            let held: Vec<(u32, String)> = (0..N_TEXT)
                .filter(|entity| entity % extents == extent)
                .filter_map(|entity| match text_fixture_prose(entity) {
                    ScalarValue::Utf8(prose) => Some((entity as u32, prose)),
                    _ => None,
                })
                .collect();
            let rows: Vec<(u32, &str)> = held
                .iter()
                .map(|(entity, prose)| (*entity, prose.as_str()))
                .collect();
            column.push_extent(&rows).expect("an extent");
        }
        column
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

    /// [`dedup_sorted`] must answer what `Vec::dedup` answers, and must leave the array alone
    /// wherever nothing moves: the ids it runs over are a mapped file, so a write it does not need
    /// is a page dirtied for nothing. The second assertion is what checks that.
    #[test]
    fn dedup_sorted_answers_vec_dedup_and_writes_only_what_moves() {
        for case in [
            vec![],
            vec![7u64],
            vec![7, 7],
            vec![1, 2, 3, 4],
            vec![1, 1, 2, 3, 3, 3, 9],
            vec![5, 5, 5, 5],
            vec![0, u64::MAX],
        ] {
            let mut expected = case.clone();
            expected.dedup();
            let mut values = case.clone();
            let len = dedup_sorted(&mut values);
            assert_eq!(&values[..len], &expected[..], "over {case:?}");
        }

        // Nothing moves in a run with no duplicates, so nothing past the last survivor is written
        // either — and every survivor is written where it already was.
        let mut values: Vec<u64> = (0..64).collect();
        let before = values.clone();
        assert_eq!(dedup_sorted(&mut values), 64);
        assert_eq!(values, before);
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
        join_chunk(
            &mut chunk,
            Ids::Sparse(&source_ids),
            |ordinal, id, payload| {
                seen.push((ordinal, id, payload));
                Ok(())
            },
        )
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
        let result =
            join_chunk(
                &mut chunk,
                Ids::Sparse(&source_ids),
                |ordinal, id, ()| match ordinal {
                    Some(_) => Ok(()),
                    None => Err(input_changed(&format!("entity {id} missing"))),
                },
            );
        assert!(result.is_err());
    }

    /// **The two arms of [`Ids`] are one function.** Over a union that is one unbroken range, the
    /// subtraction and the merge sweep must hand `on_row` the same sequence — same ordinals, same
    /// misses, same order — because the ordinal is what the entity-id assignment breaks its last
    /// tie on (I9) and the two arms are chosen by a property of the corpus rather than by the
    /// caller.
    #[test]
    fn both_id_space_arms_resolve_a_chunk_identically() {
        for (first, len) in [(0u64, 6usize), (17, 6), (u64::MAX - 5, 6), (0, 1)] {
            let array: Vec<u64> = (0..len as u64).map(|i| first + i).collect();
            let mut probes: Vec<(u64, u32)> = Vec::new();
            for (payload, id) in [
                first.wrapping_sub(1),
                first,
                first + (len as u64 / 2),
                first + (len as u64 - 1),
                first.wrapping_add(len as u64),
                0,
                u64::MAX,
            ]
            .into_iter()
            .enumerate()
            {
                probes.push((id, payload as u32));
            }

            let run = |ids: Ids<'_>| {
                let mut chunk = probes.clone();
                let mut seen: Vec<(Option<u32>, u64, u32)> = Vec::new();
                join_chunk(&mut chunk, ids, |ordinal, id, payload| {
                    seen.push((ordinal, id, payload));
                    Ok(())
                })
                .unwrap();
                seen
            };
            assert_eq!(
                run(Ids::Sparse(&array)),
                run(Ids::Contiguous { first, len }),
                "over {len} ids from {first}"
            );
        }
    }

    /// The range test is a property of the values, not of which arm holds them: an array that
    /// happens to be a range answers with its first id, and one that does not answers `None`. A
    /// union holding both 0 and `u64::MAX` spans one more than a `u64` holds, which is not a range
    /// and must not wrap into one.
    #[test]
    fn the_range_test_refuses_a_gap_and_an_overflowing_span() {
        assert_eq!(Ids::Sparse(&[4, 5, 6]).contiguous_from(), Some(4));
        assert_eq!(Ids::Sparse(&[4, 6]).contiguous_from(), None);
        assert_eq!(Ids::Sparse(&[0, u64::MAX]).contiguous_from(), None);
        assert_eq!(Ids::Sparse(&[u64::MAX]).contiguous_from(), Some(u64::MAX));
        assert_eq!(Ids::Sparse(&[]).contiguous_from(), None);
        assert_eq!(
            Ids::Contiguous { first: 9, len: 3 }.contiguous_from(),
            Some(9)
        );
        // An empty union takes the route that answers nothing, on both arms.
        assert_eq!(Ids::Contiguous { first: 0, len: 0 }.contiguous_from(), None);
    }

    /// The presence bitmap is the decision procedure: it counts the distinct ids, names the
    /// extrema, and walks out the sorted deduplicated union without a sort.
    #[test]
    fn the_presence_bitmap_counts_names_and_walks_the_union() {
        let mut bits = IdPresence::zeroed(100, 200);
        assert_eq!(bits.count(), 0);
        assert_eq!(bits.extrema(), None);
        assert_eq!(bits.ids().collect::<Vec<_>>(), Vec::<u64>::new());

        assert_eq!(bits.set(100), Some(false));
        assert_eq!(bits.set(100), Some(true), "a set bit is a duplicate");
        assert_eq!(bits.set(163), Some(false));
        assert_eq!(bits.set(164), Some(false));
        assert_eq!(bits.set(299), Some(false));
        assert_eq!(bits.set(99), None, "below the base is outside the span");
        assert_eq!(bits.set(300), None, "past the span is outside it");

        assert_eq!(bits.count(), 4);
        assert_eq!(bits.extrema(), Some((100, 299)));
        assert_eq!(bits.ids().collect::<Vec<_>>(), vec![100, 163, 164, 299]);

        let mut other = IdPresence::zeroed(100, 200);
        assert_eq!(other.set(101), Some(false));
        assert_eq!(other.set(163), Some(false));
        bits.union_with(&other);
        assert_eq!(bits.count(), 5);
        assert_eq!(
            bits.ids().collect::<Vec<_>>(),
            vec![100, 101, 163, 164, 299]
        );
    }

    /// **A span ending at `u64::MAX` has padding bits in its last word**, and naming one of them
    /// on the way to an answer would pass the end of the type. Every offset here is a set bit's,
    /// which is inside the span.
    #[test]
    fn the_presence_bitmap_spans_the_top_of_the_id_space() {
        let base = u64::MAX - 70;
        let mut bits = IdPresence::zeroed(base, 71);
        assert_eq!(bits.set(base), Some(false));
        assert_eq!(bits.set(base + 70), Some(false));
        assert_eq!(bits.set(u64::MAX), Some(true), "base + 70 is u64::MAX");
        assert_eq!(bits.extrema(), Some((base, u64::MAX)));
        assert_eq!(bits.count(), 2);
        assert_eq!(bits.ids().collect::<Vec<_>>(), vec![base, u64::MAX]);
    }

    /// A range is exactly `extrema` spanning `count`, which is what pass one tests before it
    /// declares the union needs no array.
    #[test]
    fn the_presence_bitmap_decides_a_range_exactly() {
        let range = |ids: &[u64]| {
            let mut bits = IdPresence::zeroed(0, 64);
            for &id in ids {
                bits.set(id).unwrap();
            }
            let (first, last) = bits.extrema().unwrap();
            last - first + 1 == bits.count()
        };
        assert!(range(&[0, 1, 2, 3]));
        assert!(range(&[61, 62, 63]));
        assert!(range(&[7]));
        assert!(!range(&[0, 2]));
        assert!(!range(&[0, 1, 2, 63]));
    }
}
