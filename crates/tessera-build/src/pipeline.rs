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
//!   (the overwhelming majority — the probe corpus averages 1.72). Only groups that tie on
//!   that key *and* contain a longer signature are refined by a full signature comparison. The
//!   pre-sort key is order-consistent with the lexicographic signature order, so refining within
//!   ties reproduces it exactly.
//! * **The postings order.** Entity lists come out of a term-bucketed scatter and are sorted per
//!   term, which is the same ascending list the linear build accumulates by walking items in
//!   entity order.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

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
    write_columns, write_morton_codes, write_permutation_iter, ScalarColumnData,
};
use tessera_types::{EntityId, IdentityKey, SMALL_TERM_THRESHOLD_DEFAULT};

use crate::error::{BuildError, Result};
use crate::input;
use crate::observer::{BuildObserver, BuildStage, StageTimer};
use crate::spill;
use crate::{
    fsync_file, validate_args, write_ext_locator, write_external_id_runs, write_manifests,
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
fn detect_memory_budget() -> u64 {
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
    // bucket (8 bytes/pair) + recs (12/item) + starts (4/item) + long bitset (1/8 per item),
    // beside the loop-wide entity map (4/item over all n), per-term counters (4/term), the
    // join-chunk buffer (which scales down with the corpus, so a tiny test budget stays
    // feasible for a tiny corpus) and a fixed slack for band buffers, decoders and allocator.
    const SLACK: u64 = 64 << 20;
    let chunk_bytes = 16 * (JOIN_CHUNK_ROWS as u64).min(pair_rows.max(1) as u64);
    let loop_fixed = 4 * n + 4 * row_counts.len() as u64 + chunk_bytes + SLACK;
    let per_batch = |b: u64| 8 * worst_pairs(b) + 12 * b + 4 * (b + 1) + b / 8;
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
    // three phase peaks, all conservative: buckets full beside the first batch's bands;
    // bands full beside the postings spool; the spool becoming postings.arrow beside the
    // segment. (P here is pre-dedup pairs; band/spool bytes-per-pair are stated ceilings for
    // the varint codec and Roaring postings, not measurements of this corpus.)
    let p = pair_rows as u64;
    let phase_spill = if bucket_in_ram { 0 } else { 8 * p } + (6 * p) / batches.max(1);
    let phase_bands = 6 * p + 4 * p;
    let phase_assemble = 4 * p + 26 * n;
    let disk_need = phase_spill.max(phase_bands).max(phase_assemble);
    if let Some(free) = available_disk(&args.out) {
        if free < disk_need {
            return Err(BuildError::Invalid(format!(
                "insufficient disk for this build: ~{disk_need} bytes needed at peak \
                 (spill phase {phase_spill}, band phase {phase_bands}, assembly phase \
                 {phase_assemble}; n = {n}, pairs = {p}, batches = {batches}), {free} \
                 available at the output path; free disk and retry"
            )));
        }
    }

    Ok(BuildPlan {
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
        term_keys,
        term_ids,
        row_counts,
        term_count,
        pair_rows,
        histogram,
        histogram_shift,
        dict_paths,
    } = build_dictionary(args, &source_ids, &dict_dir)?;
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
    let mut resolve =
        |chunk: &mut Vec<(u64, u64)>, resolved: &mut Vec<(u64, u64)>, sink: &mut BucketSink| {
            resolve_pairs_chunk(
                chunk,
                resolved,
                &source_ids,
                &term_keys,
                &term_ids,
                |value| {
                    pushed += 1;
                    sink.push(value)
                },
            )
        };
    input::scan_pairs(&args.pairs, args.limit, |source_id, source_term| {
        chunk.push((source_id, source_term));
        if chunk.len() == JOIN_CHUNK_ROWS {
            if let Err(e) = resolve(&mut chunk, &mut resolved, &mut sink) {
                failure = Some(e);
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    resolve(&mut chunk, &mut resolved, &mut sink)?;
    drop(chunk);
    drop(resolved);
    if pushed as usize != pair_rows {
        return Err(input_changed(&format!(
            "the pairs file yielded {pair_rows} rows, then {pushed}"
        )));
    }
    drop(source_ids);
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
        // below needs every item's slice, not only the long ones.
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
    tmp.close()?;

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

    // ---- 8. geometry, in entity order ------------------------------------------------
    // Chunk-order insensitivity ([`join_chunk`]): source ids are duplicate-checked, so every
    // `(x_of_entity, y_of_entity)` slot is written exactly once — there is no order to observe.
    // (A file that repeats or substitutes ids since the first pass fails the id-anchor check
    // below — a row count alone would accept a repeat that compensates a removal, and this was
    // previously last-write-wins silent.)
    // 32-bit fixed point per axis, not coordinates: the cell code and its residual both
    // fall out by shift and mask, so no stage re-quantises (see `input::PointRow`).
    let mut x_of_entity: Vec<u32> = vec![0; n as usize];
    let mut y_of_entity: Vec<u32> = vec![0; n as usize];
    let mut points_seen = 0u64;
    let mut geom_anchor = 0u64;
    let mut failure: Option<BuildError> = None;
    let mut chunk: Vec<(u64, (u32, u32))> = Vec::with_capacity(JOIN_CHUNK_ROWS.min(n as usize));
    let resolve = |chunk: &mut Vec<(u64, (u32, u32))>,
                   x_of_entity: &mut Vec<u32>,
                   y_of_entity: &mut Vec<u32>,
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
        chunk.push((point.source_id, (point.qx, point.qy)));
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
    let mut minters = args.schema.discovered_minters();
    let attributes_by_entity =
        read_attributes_by_entity(args, n, &source_ids, &entity_of_ordinal, &mut minters)?;

    drop(source_ids);
    drop(entity_of_ordinal);

    timer.end(BuildStage::GeometryScan, n);

    // ---- 8b. attribute filter postings (filter-index §4) -------------------------------
    // Its own stage, after entity assignment and before the tiler sort: entity ids are final
    // here (stage 5, permanent under I9) and the values have just been read, which are the two
    // things the emit needs. It cannot ride on `PostingsWrite` — that stage runs before the
    // attribute values exist.
    let filter_paths = write_filter_postings(&partition_dir, &args.schema, &attributes_by_entity)?;
    // The record blob rides the same stage boundary, for the same two reasons: entity ids are
    // final (I9) and the attribute values are in hand. Its files join the manifest digest at
    // step 11 with everything else.
    let record_paths = write_record_blob(&partition_dir, &args.schema, &attributes_by_entity)?;
    timer.end(BuildStage::FilterPostings, n);

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
                // From the quantised form directly: `split32`'s cell half is by
                // construction the code `morton_of` would give for the same point.
                morton: split32(x_of_entity[entity], y_of_entity[entity]).0.raw(),
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

    // The row→entity direction beside it (`tessera_store::row_entity`), from the same sorted rows
    // the permutation was scattered from.
    let row_entity_path = slice_dir.join(tessera_store::ROW_ENTITY_FILE);
    let rows_by_index: Vec<u32> = rows.iter().map(|r| r.entity).collect();
    tessera_store::write_row_entity(&row_entity_path, &rows_by_index)
        .map_err(|e| BuildError::io(&row_entity_path, e))?;
    fsync_file(&row_entity_path)?;

    let columns_path = segment_dir.join("columns.arrow");
    {
        // Built and released one column at a time: the record batch itself is the largest thing
        // this build ever holds, so nothing that can be dropped first is kept alongside it.
        // Indexed parallel gathers — collect preserves row order, so bytes are unchanged; at
        // 10⁹ rows the serial versions are a billion random 4-byte reads each.
        // The residual is the low half of the same `split32` whose high half became the row's
        // Morton code above — one splitting of one fixed-point position, so `columns.arrow` and
        // `morton.u32` cannot describe different points.
        let residual_row: Vec<u32> = rows
            .par_iter()
            .map(|r| {
                let entity = r.entity as usize;
                split32(x_of_entity[entity], y_of_entity[entity]).1
            })
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
        // The declared attribute tail, permuted into the same row order as everything above.
        //
        // **Gathered per entity, then permuted — not read in row order.** The attribute pass
        // visits the points file in *file* order, and `entity_row` is the row-order permutation
        // of entity ids, so the tail is materialised entity-major first and indexed through
        // `entity_row` exactly as `residual_row` is. Reading the file a third time in row order
        // is the alternative, and it is a random-access read of a multi-gigabyte parquet file.
        //
        // Held after `x_of_entity`/`y_of_entity` are dropped, so the peak is the record batch plus
        // one attribute tail rather than both — at the widths §3.6 argues for (1–4 B/row against
        // geometry's 8) the tail is the smaller term either way.
        let scalars = permute_attribute_tail(&args.schema, attributes_by_entity, &entity_row)?;
        drop(entity_row);
        write_columns(&columns_path, tessera_row, residual_row, scalars)
            .map_err(|e| BuildError::io(&columns_path, e))?;
    }
    fsync_file(&columns_path)?;
    fsync_file(&morton_path)?;

    timer.end(BuildStage::SegmentWrite, n);

    // ---- 11. manifests ---------------------------------------------------------------
    let mut other_paths = vec![
        postings_path,
        permutation_path,
        row_entity_path,
        columns_path,
        morton_path,
    ];
    other_paths.extend(filter_paths);
    other_paths.extend(record_paths);
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
        plan.recorded_batch_items,
        &minters,
    )?;
    // Reported in bytes, not rows: this stage re-reads and SHA-256s every byte the build wrote,
    // so it scales with bundle size rather than with item count.
    timer.end(BuildStage::Manifests, report.bundle_bytes);
    Ok(report)
}

/// Read the declared attribute columns into **entity-major** vectors, one per declared attribute.
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
/// **Every entity must be visited.** A source row this pass misses would leave its entity's slot
/// at the type's zero — indistinguishable from a legitimately absent value, in a column that
/// reports no error. The count is checked rather than trusted: this is a *third* pass over the
/// points file, and a file that changed under the build is exactly what the geometry pass's own
/// anchor check exists to catch.
fn read_attributes_by_entity(
    args: &BuildArgs,
    n: u64,
    source_ids: &[u64],
    entity_of_ordinal: &[u32],
    minters: &mut std::collections::HashMap<String, tessera_store::vocabulary::VocabularyMinter>,
) -> Result<Vec<Vec<ScalarValue>>> {
    if args.schema.is_empty() {
        return Ok(Vec::new());
    }
    let attributes = &args.schema.attributes;
    // One flat vector per column, indexed by entity. `ScalarValue` rather than a typed vector:
    // this form is transient and the typed allocation is the one that survives into the record
    // batch, so paying for the enum here and the tight `Vec` there is the right way round.
    let mut by_entity: Vec<Vec<ScalarValue>> = attributes
        .iter()
        .map(|_| vec![ScalarValue::U8(0); n as usize])
        .collect();
    let mut seen = 0u64;
    let mut unknown: Option<u64> = None;
    input::scan_attributes(
        &args.points,
        &args.schema,
        minters,
        args.limit,
        |source_id, values| {
            // Binary search rather than `join_chunk`'s merge sweep: this pass is per-row work over a
            // handful of narrow columns, not the corpus-scale join the geometry pass does, so the
            // sweep's chunk machinery would cost more than the log n it saves.
            let Ok(ordinal) = source_ids.binary_search(&source_id) else {
                unknown.get_or_insert(source_id);
                return;
            };
            let entity = entity_of_ordinal[ordinal] as usize;
            seen += 1;
            for (column, value) in by_entity.iter_mut().zip(values) {
                column[entity] = value.clone();
            }
        },
    )?;
    if let Some(source_id) = unknown {
        return Err(input_changed(&format!(
            "the points file's attribute pass names entity {source_id}, which its first pass did \
             not"
        )));
    }
    if seen != n {
        return Err(input_changed(&format!(
            "the points file's attribute pass yielded {seen} rows, but its first pass selected \
             {n} — some row would carry a value that is absent only because it was never read"
        )));
    }
    Ok(by_entity)
}

/// Write the entity-space filter postings for every column declared `used_for = "filter"`, and
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
/// What the banding does *not* bound is the attribute tail this reads from: `by_entity` is already
/// a `Vec<ScalarValue>` per column at ~24 B per value, which filter-index §4 prices as outside the
/// memory plan at 10⁹ regardless. That is the reader's ceiling, stated where it is paid; this pass
/// no longer adds a second copy of the column to it.
pub(crate) fn write_filter_postings(
    partition_dir: &Path,
    schema: &crate::schema::Schema,
    by_entity: &[Vec<ScalarValue>],
) -> Result<Vec<PathBuf>> {
    write_filter_postings_banded(partition_dir, schema, by_entity, POSTINGS_BAND_ROWS)
}

/// Entity ids the postings emit holds in flight — the shared emit's own constant, so the build and
/// the fold band identically (filter-index §6.2).
use tessera_filter_write::POSTINGS_BAND_ROWS;

fn write_filter_postings_banded(
    partition_dir: &Path,
    schema: &crate::schema::Schema,
    by_entity: &[Vec<ScalarValue>],
    band_rows: usize,
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for (attribute, values) in schema.attributes.iter().zip(by_entity) {
        if !postings_are_owed(schema, attribute) {
            continue;
        }
        let column_dir = partition_dir.join("attrs").join(&attribute.name);
        std::fs::create_dir_all(&column_dir).map_err(|e| BuildError::io(&column_dir, e))?;

        // The value column is the artefact of record (filter-index §2.1); the postings below are
        // derived from it. Written first so that a build interrupted between the two leaves the
        // record without its accelerator rather than an accelerator with no record.
        let values_path = column_dir.join("values.arrow");
        let presence_path = column_dir.join("presence.roaring");
        let presence = write_column_values(&values_path, &presence_path, attribute, values)?;
        fsync_file(&values_path)?;
        paths.push(values_path);
        if presence {
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
    Ok(paths)
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
    schema: &crate::schema::Schema,
    by_entity: &[Vec<ScalarValue>],
) -> Result<Vec<PathBuf>> {
    let blob_columns: Vec<usize> = schema
        .attributes
        .iter()
        .enumerate()
        .filter(|(_, a)| !a.filter && !a.render)
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

    let n = by_entity.first().map_or(0, Vec::len);
    let mut fields: Vec<RecordField> = Vec::with_capacity(blob_columns.len());
    // A range loop on purpose: each entity gathers across *several* parallel columns, which is
    // not the single-slice shape `needless_range_loop`'s rewrite fits.
    #[allow(clippy::needless_range_loop)]
    for entity in 0..n {
        fields.clear();
        for &column in &blob_columns {
            let attribute = &schema.attributes[column];
            let Some(value) = record_value_of(&by_entity[column][entity], attribute)? else {
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
fn record_value_of(
    value: &ScalarValue,
    attribute: &crate::schema::Attribute,
) -> Result<Option<RecordValue>> {
    if attribute.vocabulary.is_some() {
        let code = category_code(value, &attribute.name)?;
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
    Ok(match value {
        ScalarValue::Null => None,
        ScalarValue::Bool(v) => Some(RecordValue::Bool(*v)),
        ScalarValue::U8(v) => Some(RecordValue::U8(*v)),
        ScalarValue::U16(v) => Some(RecordValue::U16(*v)),
        ScalarValue::U32(v) => Some(RecordValue::U32(*v)),
        ScalarValue::U64(v) => Some(RecordValue::U64(*v)),
        ScalarValue::I8(v) => Some(RecordValue::I8(*v)),
        ScalarValue::I16(v) => Some(RecordValue::I16(*v)),
        ScalarValue::I32(v) => Some(RecordValue::I32(*v)),
        ScalarValue::I64(v) => Some(RecordValue::I64(*v)),
        ScalarValue::F32(v) => Some(RecordValue::F32(*v)),
        ScalarValue::F64(v) => Some(RecordValue::F64(*v)),
        ScalarValue::TimestampUs(v) => Some(RecordValue::TimestampUs(*v)),
        ScalarValue::Utf8(v) => Some(RecordValue::Utf8(v.clone())),
    })
}

/// The staged attribute values of one category column, as the shared postings emit reads them.
///
/// **The emit itself lives in `tessera-filter`** (`fold::write_category_postings`), because the
/// fold rebuilds these postings from the folded column and filter-index §6.2 makes one writer
/// rather than two producers that agree the byte-identity argument. What is here is the adaptation:
/// the build's source is a `ScalarValue` per entity, where the fold's is a value column.
struct StagedCategory<'a> {
    values: &'a [ScalarValue],
    column: &'a str,
}

impl tessera_filter_write::CategorySource for StagedCategory<'_> {
    fn for_each(&self, f: &mut dyn FnMut(u32, u32) -> std::io::Result<()>) -> std::io::Result<()> {
        for (entity, value) in self.values.iter().enumerate() {
            let code = category_code(value, self.column).map_err(std::io::Error::other)?;
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
    values: &[ScalarValue],
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

/// Write one column's values in entity order, and its presence bitmap where presence is partial.
///
/// Returns whether a presence bitmap was written. **A column every entity carries a value in gets
/// none**, and that is the fast path rather than an omission: the entity id is then the array index,
/// which measured 28.7 ms against a presence-addressed 1,078 ms at 10⁹
/// (`probes/2026-08-08-filter-layout/`). Writing an all-ones bitmap would be correct and would cost
/// the scan that path, so the distinction lives in the file set rather than in the bitmap's contents.
///
/// **Absence is out of band in both families, and by different means.** A category spends the
/// reserved code 0, which its vocabulary reserves out of the value space. A string has no spare
/// value to spend — the empty string is one a corpus may legitimately hold, and contracts §2.4
/// already refuses it on the ingest plane because an unset field and a client bug both produce it —
/// so absence arrives as `ScalarValue::Null`. Folding the two together would report an item as
/// matching a value it does not have.
fn write_column_values(
    values_path: &Path,
    presence_path: &Path,
    attribute: &crate::schema::Attribute,
    values: &[ScalarValue],
) -> Result<bool> {
    let mut present = croaring::Bitmap::new();
    let mut universal = true;

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
    if attribute.ty == ScalarType::Utf8 {
        let mut held: Vec<String> = Vec::new();
        for (entity, value) in values.iter().enumerate() {
            match value {
                ScalarValue::Utf8(text) => {
                    present.add(entity as u32);
                    held.push(text.clone());
                }
                ScalarValue::Null => universal = false,
                other => {
                    return Err(BuildError::Invalid(format!(
                        "attribute '{}' is declared `utf8` but carries {other:?}",
                        attribute.name
                    )))
                }
            }
            if held.len() >= VALUE_CHUNK {
                push!(Codes::text(held.drain(..)));
            }
        }
        if !held.is_empty() {
            push!(Codes::text(held));
        }
    } else if attribute.vocabulary.is_some() {
        let mut held: Vec<u32> = Vec::new();
        for (entity, value) in values.iter().enumerate() {
            let code = category_code(value, &attribute.name)?;
            if code == tessera_store::vocabulary::ABSENT_CODE {
                universal = false;
                continue;
            }
            present.add(entity as u32);
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
        for (entity, value) in values.iter().enumerate() {
            if matches!(value, ScalarValue::Null) {
                universal = false;
            } else {
                present.add(entity as u32);
            }
        }
        push_numeric_chunks(&mut writer, values_path, attribute, values)?;
    }
    let presence = (!universal).then_some(&present);
    writer
        .finish(presence)
        .map_err(|e| BuildError::io(values_path, e))?;
    Ok(!universal)
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
fn column_kind(attribute: &crate::schema::Attribute) -> ColumnKind {
    if attribute.ty == ScalarType::Utf8 {
        return ColumnKind::Text;
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
        ScalarType::Utf8 => ColumnKind::Text,
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
    attribute: &crate::schema::Attribute,
    values: &[ScalarValue],
) -> Result<()> {
    macro_rules! stream {
        ($variant:ident, $ctor:expr, $map:expr) => {{
            let mut out = Vec::with_capacity(VALUE_CHUNK);
            for v in values {
                match v {
                    ScalarValue::$variant(x) => out.push($map(*x)),
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
        ScalarType::Utf8 => unreachable!("the caller handles utf8 before reaching here"),
    }
    Ok(())
}

/// Does this column owe a postings file?
///
/// Two independent reasons, and the second is the one a reader will not expect.
///
/// **`used_for = "filter"`** is the obvious one: the column is declared filterable, and postings are
/// how a broad-coverage filter stays inside its latency budget (filter-index §2.3).
///
/// **`listing = "per_viewer"`** is the other, and it is *not* optional. That control gates the
/// existence of a value name, and the gate is membership-derived: a value is offered only if the
/// principal can see an item carrying it (per-point-attributes §3.3). Deriving that needs the
/// per-`(column, code)` member sets, which are exactly these postings. Without them `/v1/categories`
/// would have to derive membership by scanning the value column per request — which is inside a
/// *filter's* latency budget but not inside this endpoint's, and would make contracts §3.2's
/// compute-admission justification ("no mask composition, no projection, no file IO") false.
///
/// So a `per_viewer` category gets postings whatever its `used_for` says. This is the one place the
/// postings stop being an optional accelerator: everywhere else a deployment that builds them and one
/// that does not answer identically and differ only in latency, but here a disclosure control depends
/// on them existing.
fn postings_are_owed(schema: &crate::schema::Schema, attribute: &crate::schema::Attribute) -> bool {
    if attribute.filter {
        return true;
    }
    attribute
        .vocabulary
        .as_ref()
        .and_then(|name| schema.vocabularies.get(name))
        .is_some_and(|v| v.listing == crate::schema::Listing::PerViewer)
}

/// The vocabulary code a category column's value carries.
///
/// The schema refuses `used_for = "filter"` on anything but a category, so the three unsigned
/// widths §3.6 allows are the whole domain; anything else reaching here is a schema-compilation
/// defect, and it fails loudly rather than filtering on a value it invented.
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

/// Permute the entity-major columns into **row order**, ready for `write_columns`.
///
/// `entity_row[r]` is the entity whose values row `r` carries — the same permutation
/// `residual_row` and `tessera_row` are built through, applied to the same arrays, so a row's
/// geometry, identity and attributes cannot come from different items.
fn permute_attribute_tail(
    schema: &crate::schema::Schema,
    by_entity: Vec<Vec<ScalarValue>>,
    entity_row: &[u32],
) -> Result<Vec<(String, ScalarColumnData)>> {
    let mut out = Vec::with_capacity(by_entity.len());
    for (attribute, values) in schema.attributes.iter().zip(by_entity) {
        // **The tail is exactly the render columns.** A `filter`-only column is entity-space and
        // has already been written there; including it here would give it a slot in every row as
        // well, which is the per-row cost §10.3's routing exists to avoid and — for a `utf8`
        // column — the one `render` on `utf8` is refused for outright.
        if !attribute.render {
            continue;
        }
        let mut column = ScalarColumnData::of(attribute.ty, entity_row.len());
        for &entity in entity_row {
            column
                .push(
                    // A render column is non-nullable, so an absent value is drawn at the type's
                    // zero until decision 0064's render half lands — see `or_render_placeholder`.
                    values[entity as usize].or_render_placeholder(attribute.ty),
                    &attribute.name,
                )
                .map_err(|e| BuildError::Invalid(format!("attribute '{}': {e}", attribute.name)))?;
        }
        out.push((attribute.name.clone(), column));
    }
    Ok(out)
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

    // The probe corpus carries integer term ids; the item's `access` label is the comma-joined
    // decimal source term ids, so `builtin:passthrough` yields decimal-string descriptors (R6).
    // Streamed, not interned: the descriptors here are distinct by construction (one per
    // distinct source term) and arrive in term-id order, which is `DictStreamWriter`'s exact
    // contract — at T = 117M an interner is gigabytes of pointless ownership.
    let mut dict = tessera_authz::DictStreamWriter::new(dict_dir);
    let mut pairs_of_term: Vec<(u64, u32)> = Vec::with_capacity(order.len());
    let mut row_counts: Vec<u64> = Vec::with_capacity(order.len());
    for &(_, source_term) in &order {
        let term = dict.append(source_term.to_string().as_bytes());
        pairs_of_term.push((source_term, term.raw()));
        row_counts.push(first_ordinal[&source_term].1);
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
///   refined order is: all shorts first, in the ordinal order the pre-sort already established;
///   then the longs, ordered by their signature *tails* (terms from index 2 on) with ordinal
///   breaking exact-tail ties. That is precisely the reference build's
///   `(signature, source_id)` order, with the tiebreak the current stable sort left implicit
///   made explicit.
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

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_types::IdentityKey;

    /// **The band count must not be observable in the artefact.** A band boundary is a place the
    /// scatter restarts and the ascending-key check spans, so a column emitted in one band and the
    /// same column emitted in a band per code have to be the same file — which is also what makes
    /// the band budget a memory knob rather than a format decision.
    #[test]
    fn banding_the_postings_emit_does_not_change_its_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Codes scattered across a 32-bit space, as `vocabulary` mints them, with one code held
        // heavily enough to cross the Roaring threshold and code 0 (absent) carried too.
        let values: Vec<ScalarValue> = (0..5_000u32)
            .map(|e| {
                ScalarValue::U32(match e % 7 {
                    0 => tessera_store::vocabulary::ABSENT_CODE,
                    1 => 3_999_999_999,
                    2 => 17,
                    _ => 1_000 + (e % 53),
                })
            })
            .collect();

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
            .map(|v| category_code(v, "colour").expect("code"))
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
