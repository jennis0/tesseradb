//! The value column: a filterable attribute's values in entity order, and the masked scan over them.
//!
//! **This is the artefact of record** (`filter-index.md` §2.1). Every accelerator — a category's
//! per-value postings today, anything else later — is derived from it and rebuilt at the fold, which
//! is what keeps a filter value out of the ordinal-stability rules the term dictionary lives under.
//!
//! # Why the scan carries no timing channel
//!
//! A predicate is evaluated by walking the **candidate mask** and testing the values it selects, so
//! the work is a function of `(candidate, column)` and never of the value being sought. A value the
//! principal cannot see costs exactly what a value that does not exist costs, because the same bytes
//! are read either way. per-point-attributes §3.8 requires those to be indistinguishable *in work*
//! and not merely in outcome, and no design that evaluated an operand before masking it could
//! deliver that (`filter-surface.md` §2.1).
//!
//! This is the reason the loop is written candidate-first rather than column-first. A column-first
//! scan with an early exit — "stop once every candidate is accounted for" — would be faster on some
//! inputs and would make the running time a function of where the matches lie, which is the
//! property this construction exists to deny.
//!
//! # Addressing, and why it is measured rather than chosen
//!
//! Where every entity carries a value, the entity id *is* the array index and no addressing
//! structure is stored. Where presence is partial — the ordinary case once commit windows interleave
//! slices, which makes an entity range ascending-*with-holes* (write-path §4.2) — a Roaring presence
//! bitmap accompanies a compact value array, the *k*-th set bit's value at slot *k*.
//!
//! Two layouts were measured and refused (`probes/2026-08-08-filter-layout/`). An explicit
//! `(entity_id, value)` pair column is never optimal on either axis at any scale or presence shape:
//! it costs 4 B/entity *per column*, and the ids must be read to know what is in them. A run table
//! `(start, len, base_rank)` is the smallest structure of all — 12 bytes for a 10⁹ column — and
//! collapses to 2.9–10.2 s on a broad candidate, because rank becomes a binary search per candidate
//! entity. **Neither is a tuning choice a later reader should revisit from the storage column
//! alone.**
//!
//! # The traversal is counted, so indistinguishability can be a test rather than a comment
//!
//! The property above is a claim about **work**, and the keyword family turns it into one the suite
//! has to check: a needle no dictionary resolves becomes a reserved ordinal and is scanned for
//! anyway, so that *no item has this value* is not cheaper than *some do* (`records-and-search.md`
//! §4.3). Asserting that with a stopwatch is worse than not asserting it — a wall-clock comparison
//! goes green on a loaded machine, which is the direction that lets the channel reopen unnoticed.
//! [`take_scan_work`] reports instead what the calling thread's scans have traversed, in runs and
//! slots, and the assertion is that two needles cost the same non-zero amount of it.
//!
//! **Runs and slots are the honest unit because [`ValueColumn::for_each_slot_run`] is the sole
//! traversal**: every predicate of every family reaches its values through it, so work skipped
//! anywhere above it — an early return, a bound narrowed to something unrepresentable, a layer
//! passed over — arrives here as fewer slots, and a scan that ran to completion reports the same
//! slots whatever it was looking for. Two quantities rather than one because they answer different
//! halves of "the same candidate consulted": runs is how much of the candidate's structure was
//! walked, slots is how many values were compared. What this deliberately does not count is work
//! outside the traversal — the broad `contains` route's per-key dictionary walk, which is bounded by
//! the artefact and asserted where it lives — or a break inside one run's element loop, which is not
//! the shape a "this can match nothing, so skip it" optimisation takes.
//!
//! **Compiled only under `debug_assertions`, and thread-local rather than global.** The counter sits
//! in the traversal's callback, which the scattered case reaches once per candidate entity, and this
//! module has already measured that position as worth 20% (see [`ValueColumn::walk_typed`]) — so an
//! always-compiled counter would buy the assertion with a permanent regression on the arm that can
//! least afford one, and a shared atomic would additionally put a contended cache line under every
//! parallel scan. Thread-local also makes the count correct under the test harness, which runs tests
//! concurrently in one process; the limit to read with it is that a scan handed to another thread is
//! not counted by the thread that asked for it. Read the release consequence too: a `--release` test
//! run finds a counter that never moves, so the work assertions fail loudly rather than passing
//! vacuously — they are debug-build assertions, and the gate builds them that way.

#[cfg(debug_assertions)]
use std::cell::Cell;
use std::io;
use std::path::Path;
use std::sync::Arc;

use arrow::buffer::{Buffer, ScalarBuffer};
use croaring::{Bitmap, Portable};

use crate::pack;
use tessera_types::AttrLocalId;

/// Matched entities, folded into the result bitmap in bounded chunks.
///
/// **The result is not a safe thing to accumulate whole.** A predicate that matches most of what it
/// is asked about is ordinary — a range covering most of a domain, a tick-box set with everything
/// ticked — and buffering every match before building the bitmap made the buffer proportional to
/// the *result*: measured at 10⁹, a filter matching a quarter of the corpus peaked at 1.1 GB and one
/// matching all of it at 4.1 GB, on a request path, transiently, per concurrent request. The
/// compute-admission gate rations CPU and knows nothing about it.
///
/// Folding every [`CHUNK`] entities instead bounds the buffer at 512 KB whatever the result's size,
/// and costs nothing measurable: the check is per *run* rather than per entity, because
/// [`ValueColumn::for_each_slot_run`] caps the ranges it hands out at `CHUNK` — so the buffer can
/// reach at most twice it, and the sparse case still batches thousands of scattered hits into one
/// bulk add rather than paying a bitmap insertion each.
/// **Consecutive matches are added as a range, not one at a time**, which is the other half of the
/// unselective case. Inserting a billion entities individually costs 4.5 s at 10⁹ where adding the
/// same entities as ranges costs a fraction of it, and an unselective predicate matches in long
/// contiguous stretches by nature — a range over most of a domain matches nearly everything the
/// candidate offers, in candidate order.
///
/// The coalescing branch runs once per *hit*, not once per candidate entity, so the selective case —
/// where hits are a fraction of a percent — pays a comparison on almost nothing. **It also carries
/// no channel:** the work depends on how the matches are distributed, and a value the principal
/// cannot see and a value that does not exist both produce no matches at all, so they take the same
/// path at the same cost.
struct Hits {
    /// Scattered singles, folded in bulk.
    buf: Vec<u32>,
    /// The contiguous stretch being extended: `start..end`, empty when `start == end`.
    start: u32,
    end: u32,
    /// Whether any stretch was long enough to go in as a range — see [`Self::finish`].
    coalesced: bool,
    out: Bitmap,
}

/// Entities buffered before folding, and the cap on a single slot range. 64 Ki × 4 B = 256 KB, so
/// the buffer stays within L2 while remaining large enough for the bulk add to amortise.
const CHUNK: usize = 1 << 16;

/// The shortest stretch of consecutive matches worth adding as a range rather than buffering.
const RUN_MIN: usize = 32;

impl Hits {
    fn new() -> Self {
        Hits {
            buf: Vec::with_capacity(CHUNK + RUN_MIN),
            start: 0,
            end: 0,
            coalesced: false,
            out: Bitmap::new(),
        }
    }

    /// Entities arrive in ascending order — the traversal visits the candidate in order and each
    /// range in slot order — which is what makes "extends the current stretch" a single comparison.
    /// **The body here is two comparisons and an increment, and everything else is out of line.**
    /// `push` is reached from inside the traversal's inner loop, so if its body carries a call into
    /// croaring the whole callback stops being inlinable — measured as a 34% regression on the
    /// scattered arm, where the callback is invoked once per candidate entity and an indirect call
    /// is therefore paid ten million times.
    #[inline(always)]
    fn push(&mut self, entity: u32) {
        // `end != start` is what distinguishes "extends the open stretch" from "the accumulator is
        // empty and the first entity happens to be 0". A sentinel empty stretch would save the
        // comparison and was measured to save nothing, while making entity `u32::MAX` an overflow.
        if entity == self.end && self.end != self.start {
            self.end += 1;
            return;
        }
        self.begin(entity);
    }

    /// Retire the open stretch and start a new one. Out of line by design — see [`Self::push`].
    #[inline(never)]
    fn begin(&mut self, entity: u32) {
        self.close();
        self.start = entity;
        self.end = entity + 1;
        // **Bounded here, where the cost is per match**, not at the end of each slot range where it
        // would be per candidate entity. A scattered candidate is one range per entity, so a check
        // there is a load and a branch on every entity scanned — measured as a doubling of the
        // scattered arm, 98 to 239 ms at 10⁹, for a buffer that only ever grows on a match.
        if self.buf.len() >= CHUNK {
            self.out.add_many(&self.buf);
            self.buf.clear();
        }
    }

    /// Retire the open stretch: a short one joins the bulk buffer, a long one goes straight in as a
    /// range.
    ///
    /// **The threshold matters more than the coalescing does.** At middling selectivity the matches
    /// are not long stretches but pairs — a predicate matching half a uniform column gives runs
    /// averaging two — and adding those as ranges is dearer than buffering them. Below
    /// [`RUN_MIN`] the entities take the bulk path they always took, so coalescing helps the case it
    /// was built for and cannot cost the case it was not.
    #[inline]
    fn close(&mut self) {
        let len = self.end - self.start;
        // One entity is the overwhelmingly common case at any selectivity a scan is worth running
        // at, so it is the branch taken first and the only one that avoids a loop.
        if len == 1 {
            self.buf.push(self.start);
        } else if (len as usize) < RUN_MIN {
            for e in self.start..self.end {
                self.buf.push(e);
            }
        } else if len != 0 {
            self.out.add_range(self.start..self.end);
            self.coalesced = true;
        }
        self.start = 0;
        self.end = 0;
    }

    fn finish(mut self) -> Bitmap {
        self.close();
        self.out.add_many(&self.buf);
        // **Only when the result actually has runs in it.** A dense result is mostly runs, and
        // leaving it as array or bitset containers would hand every downstream intersection a
        // representation several times larger than it needs — the cost model being O(containers
        // touched), that is paid once here and saved at every composition and count after. But the
        // pass walks every container, which a *selective* result cannot amortise: unconditionally
        // it cost the partial-presence arms 0.25 → 0.45 ms and 6.2 → 12.7 ms at 10⁹, for a result
        // with no runs to find. The coalescing flag says which case this is, exactly.
        if self.coalesced {
            self.out.run_optimize();
        }
        // A dense result is mostly runs, and leaving it as array or bitset containers would hand
        // every downstream intersection a representation several times larger than it needs. The
        // measured cost model is O(containers touched), so this is paid once here and saved at
        // every operand composition, projection and count that follows.
        self.out
    }
}

/// Whether this scan packs whole blocks, decided once from the first block it packs.
///
/// **Packing is flat and the per-entity walk is not**, so which is cheaper depends on how much
/// matches: measured at 10⁹, packing costs ~0.8 ns per candidate entity whatever the predicate,
/// against 0.28 ns for a branch the processor predicts and 5.9 ns for one it does not. Packing
/// everything would therefore buy a 4–9.5× win on unselective predicates at a 30–49% cost on
/// selective ones — and a selective predicate over a broad candidate is the ordinary filter.
///
/// So the first whole block is packed, and its match density decides the rest of the scan. One
/// block is ~50 µs at this scale, against the seconds at stake.
///
/// **This is a decision about the principal's own visible matches, and it carries no channel.** A
/// value the principal cannot see and a value that does not exist both match nothing in that first
/// block, take the same branch, and cost the same — which is what per-point-attributes §3.8
/// requires. What the timing can reveal is roughly how much of the viewer's own result matched,
/// which is the result they are about to be handed.
#[derive(Clone, Copy, PartialEq)]
enum Packing {
    Undecided,
    On,
    /// The first block was too sparse to pay for packing; the rest of this scan walks entities.
    Abandoned,
}

/// The match density, per block, at or above which packing pays. The measured crossover sits between
/// 1% and 5% selectivity; a thirty-second is inside that band and is a shift.
const PACK_MIN_DENSITY: u32 = (pack::BLOCK as u32) / 32;

/// One candidate run that spans at least one whole 2¹⁶-aligned block: the aligned interior is packed
/// into Roaring containers, the ragged ends either side of it walked per entity. Returns whether the
/// run was handled here.
///
/// **Out of line on purpose.** Inlined into the traversal's callback this code costs every *other*
/// scan shape — measured at 10⁹, the partial-presence arms regressed 80% and the scattered arm 78%,
/// with no block ever packed in either, because the larger closure stopped being inlined into
/// `for_each_run`. The same failure and the same fix as `Hits::push`'s cold path: the callback that
/// runs once per candidate entity has to stay small.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn pack_run<T>(
    values: &[T],
    slot0: usize,
    count: usize,
    entity0: u32,
    pred: &mut impl FnMut(&T) -> bool,
    hits: &mut Hits,
    packed: &mut pack::Sink,
    words: &mut [u64; pack::WORDS],
    state: &mut Packing,
) -> bool {
    if *state == Packing::Abandoned {
        return false;
    }
    let end = slot0 + count;
    let first = slot0.div_ceil(pack::BLOCK);
    let last = end / pack::BLOCK;
    if first >= last {
        return false;
    }
    let interior = first * pack::BLOCK..last * pack::BLOCK;
    for (i, v) in values[slot0..interior.start].iter().enumerate() {
        if pred(v) {
            hits.push(entity0 + i as u32);
        }
    }
    for block in first..last {
        let at = block * pack::BLOCK;
        let card = pack::pack_block(&values[at..at + pack::BLOCK], pred, words);
        packed.push_block(block as u16, card, words);
        if *state == Packing::Undecided {
            *state = if card >= PACK_MIN_DENSITY {
                Packing::On
            } else {
                Packing::Abandoned
            };
            if *state == Packing::Abandoned {
                // This block is answered already; the remainder of the run returns to the
                // per-entity walk, which is what every later run will take too.
                let done = at + pack::BLOCK;
                let base = entity0 + (done - slot0) as u32;
                for (i, v) in values[done..end].iter().enumerate() {
                    if pred(v) {
                        hits.push(base + i as u32);
                    }
                }
                return true;
            }
        }
    }
    let tail = entity0 + (interior.end - slot0) as u32;
    for (i, v) in values[interior.end..end].iter().enumerate() {
        if pred(v) {
            hits.push(tail + i as u32);
        }
    }
    true
}

/// Find `finder`'s needle in the concatenated bytes of slots `slot0..slot0 + count`, and record the
/// entity of every value that contains it.
///
/// **A match that straddles a value boundary is not a match in any value**, and discarding those is
/// the whole correctness argument for searching a region instead of a value: the concatenation joins
/// values that are unrelated, so `"ab" ++ "cd"` contains the bytes `bc` and neither value does. A
/// match is kept only when it ends at or before the end of the value it starts in.
///
/// The owning slot is tracked with a forward cursor rather than a binary search, because matches
/// arrive in ascending order — so the cursor advances at most `count` times across the whole run,
/// however many matches there are.
///
/// **Out of line on purpose**, like `pack_run`: inlined into the traversal's callback this costs
/// every scan whose runs are single values, because the larger closure stops being inlined into
/// `for_each_run`. That has happened three times in this module's history.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn search_region(
    finder: &memchr::memmem::Finder<'_>,
    needle_len: usize,
    bytes: &[u8],
    offsets: &[i64],
    slot0: usize,
    count: usize,
    entity0: u32,
    hits: &mut Hits,
) {
    let base = offsets[slot0] as usize;
    let region = &bytes[base..offsets[slot0 + count] as usize];
    let mut slot = slot0;
    let mut last: Option<usize> = None;
    for pos in finder.find_iter(region) {
        let at = base + pos;
        while (offsets[slot + 1] as usize) <= at {
            slot += 1;
        }
        // A value containing the needle twice is recorded once. The result would be correct either
        // way — the accumulator's bulk add tolerates a repeat — but a repeat breaks the run being
        // coalesced in two, so this keeps a dense result expressible as ranges.
        if at + needle_len <= offsets[slot + 1] as usize && last != Some(slot) {
            hits.push(entity0 + (slot - slot0) as u32);
            last = Some(slot);
        }
    }
}

/// A membership test over a narrow code domain: one bit per code point.
///
/// Built once per scan and tested in constant time, which is what keeps a set-membership filter the
/// same cost as an equality one however many values it names. A code outside the domain is dropped
/// at construction — it can be carried by no entity, so it changes no answer — and dropping it
/// costs nothing that a caller could time, because the table is built and consulted identically
/// either way.
struct CodeSet {
    bits: Vec<u64>,
}

impl CodeSet {
    fn new(values: &[AttrLocalId], max: u32) -> Self {
        let mut bits = vec![0u64; (max as usize / 64) + 1];
        for v in values {
            let code = v.raw();
            if code <= max {
                bits[code as usize / 64] |= 1 << (code % 64);
            }
        }
        CodeSet { bits }
    }

    #[inline]
    fn contains(&self, code: u32) -> bool {
        // The caller only ever passes a value read from a column of the width this was built for,
        // so the index is in range by construction; `get` keeps that a wrong answer rather than a
        // panic if that ever stops being true.
        self.bits
            .get(code as usize / 64)
            .is_some_and(|w| w & (1 << (code % 64)) != 0)
    }
}

/// A membership test over a set of byte strings, bucketed by first byte.
///
/// **The bucket is what makes `in` cost about what `eq` costs.** Searching a sorted needle list
/// compares whole values, and a text column's values share long prefixes by nature — names, paths,
/// identifiers — so each comparison runs deep before it fails. Measured at 10⁸ that made a
/// five-value `in` cost 16 ns per candidate entity against equality's 3.4. Dispatching on the first
/// byte reduces the usual case to zero or one full comparison, and the length check in front of
/// that rejects most of what survives.
///
/// The buckets are a CSR index — one allocation and a 257-entry offset table — rather than 256
/// vectors, because the table is built once per scan and then read once per candidate entity.
struct ByteSet<'a> {
    /// Needles sorted by first byte. `needles[starts[b]..starts[b + 1]]` all begin with byte `b`.
    needles: Vec<&'a [u8]>,
    starts: [u32; 257],
    /// The empty needle matches the empty value, and has no first byte to bucket on.
    empty: bool,
}

impl<'a> ByteSet<'a> {
    fn new(values: impl IntoIterator<Item = &'a [u8]>) -> Self {
        let mut needles: Vec<&[u8]> = values.into_iter().collect();
        needles.sort_unstable();
        needles.dedup();
        let empty = needles.first().is_some_and(|n| n.is_empty());
        needles.retain(|n| !n.is_empty());

        // Sorting by whole value already sorts by first byte, so the counts can be taken in one
        // pass over the sorted list rather than by a second sort.
        let mut starts = [0u32; 257];
        for n in &needles {
            starts[n[0] as usize + 1] += 1;
        }
        for b in 1..257 {
            starts[b] += starts[b - 1];
        }
        ByteSet {
            needles,
            starts,
            empty,
        }
    }

    #[inline]
    fn contains(&self, v: &[u8]) -> bool {
        let Some(&first) = v.first() else {
            return self.empty;
        };
        let lo = self.starts[first as usize] as usize;
        let hi = self.starts[first as usize + 1] as usize;
        self.needles[lo..hi]
            .iter()
            .any(|n| n.len() == v.len() && *n == v)
    }
}

/// Visit `bitmap`'s set values as ascending, non-overlapping, **inclusive** runs.
///
/// Bulk-read through the cursor rather than one value at a time: a contiguous candidate collapses
/// to a handful of ranges, and the caller then walks a plain integer range instead of stepping a
/// bitmap cursor per entity.
/// Ranges per cursor read. Large enough that the call amortises, small enough to stay on the stack.
const RUN_BUF: usize = 64;

fn for_each_run(bitmap: &Bitmap, mut f: impl FnMut(u32, u32)) {
    let mut cursor = bitmap.cursor();
    let mut buf = [croaring::RangeInclusive::<u32> { start: 0, last: 0 }; RUN_BUF];
    loop {
        let n = cursor.read_many_ranges(&mut buf);
        if n == 0 {
            return;
        }
        for r in &buf[..n] {
            f(r.start, r.last);
        }
    }
}

/// One run at a time from a bitmap, buffered through the cursor's bulk read.
struct RunIter<'a> {
    cursor: croaring::bitmap::BitmapCursor<'a>,
    buf: [croaring::RangeInclusive<u32>; RUN_BUF],
    filled: usize,
    at: usize,
}

impl<'a> RunIter<'a> {
    fn new(bitmap: &'a Bitmap) -> Self {
        RunIter {
            cursor: bitmap.cursor(),
            buf: [croaring::RangeInclusive::<u32> { start: 0, last: 0 }; RUN_BUF],
            filled: 0,
            at: 0,
        }
    }

    /// The next run as `(start, last)`, inclusive.
    fn next(&mut self) -> Option<(u32, u32)> {
        if self.at == self.filled {
            self.filled = self.cursor.read_many_ranges(&mut self.buf);
            self.at = 0;
            if self.filled == 0 {
                return None;
            }
        }
        let r = self.buf[self.at];
        self.at += 1;
        Some((r.start, r.last))
    }
}

/// The value column's file name within a column's directory.
pub const VALUES_FILE: &str = "values.arrow";
/// The presence bitmap's, written only where presence is partial — its **absence is the signal**
/// that the entity id is the array index.
pub const PRESENCE_FILE: &str = "presence.roaring";

/// A column's values, at the declared width.
///
/// The width is the point, not a convenience: a hot column is priced at 0.93 GiB per byte per row
/// per 10⁹ items, and the entity-space column is priced the same way — 1 GB per byte of width at
/// 10⁹, per declared column (Appendix A). Widening a category from `u8` to `u32` to avoid a match
/// arm here would cost 3 GB.
///
/// **The buffers are `ScalarBuffer`, not `Vec`, and that is what makes the column mappable.** A
/// `ScalarBuffer<T>` dereferences to `&[T]` and owns a reference-counted Arrow `Buffer` underneath,
/// which may be an ordinary allocation *or* a window onto a memory map — so the same enum serves a
/// column built in memory and one opened from a bundle without a copy on either path. Holding
/// `Vec`s instead would force every declared column to be read into the heap at generation open,
/// which at 10⁹ × 16 columns is tens of GB paid whether or not a filter is ever issued. The scan
/// itself is unaffected: it walks a `&[T]` either way.
#[derive(Debug, Clone)]
pub enum Codes {
    U8(ScalarBuffer<u8>),
    U16(ScalarBuffer<u16>),
    U32(ScalarBuffer<u32>),
    U64(ScalarBuffer<u64>),
    I8(ScalarBuffer<i8>),
    I16(ScalarBuffer<i16>),
    I32(ScalarBuffer<i32>),
    /// Also `timestamp_us` — microseconds since the epoch, stored as the `i64` it is. The *type*
    /// exists so the unit is in the manifest rather than a convention between a schema author and
    /// their client; the storage and the comparison are an `i64`'s.
    I64(ScalarBuffer<i64>),
    F32(ScalarBuffer<f32>),
    F64(ScalarBuffer<f64>),
    /// UTF-8 values, concatenated, with `offsets[k]..offsets[k+1]` delimiting slot `k`.
    ///
    /// **A string column carries no dictionary and no index, and that is the design rather than a
    /// stage it has not reached** (`filter-index.md` §2.3). A dictionary exists to give a value an
    /// integer identity so an inverted index can key on it; nothing here is keyed on a value, so
    /// there is nothing to intern. Equality, prefix and substring are all the same masked scan with
    /// a different comparison, and the FST a prefix walk would need exists to *order* distinct
    /// values, which only matters when the values are being looked up rather than tested.
    ///
    /// Interning would also not be free of consequence: it is what made a value's identity durable,
    /// and a durable per-value identity is what the C11 ordinal hazard lives in.
    ///
    /// **The offsets are 64-bit**, which is a capacity requirement rather than a preference: Arrow's
    /// 32-bit `Utf8` caps a column's concatenated bytes at 2 GiB, and a 10⁹-entity string column
    /// passes that at two bytes per value. The file is written as `LargeUtf8` for the same reason.
    Text {
        bytes: Buffer,
        offsets: ScalarBuffer<i64>,
    },
}

impl Codes {
    pub(crate) fn len(&self) -> usize {
        match self {
            Codes::U8(v) => v.len(),
            Codes::U16(v) => v.len(),
            Codes::U32(v) => v.len(),
            Codes::U64(v) => v.len(),
            Codes::I8(v) => v.len(),
            Codes::I16(v) => v.len(),
            Codes::I32(v) => v.len(),
            Codes::I64(v) => v.len(),
            Codes::F32(v) => v.len(),
            Codes::F64(v) => v.len(),
            Codes::Text { offsets, .. } => offsets.len().saturating_sub(1),
        }
    }

    /// The UTF-8 value at `slot`, for a text column. `None` for a numeric one.
    #[inline]
    fn text_at(&self, slot: usize) -> Option<&str> {
        match self {
            Codes::Text { bytes, offsets } => {
                let lo = offsets[slot] as usize;
                let hi = offsets[slot + 1] as usize;
                // Validated once at open (`read_values`), so the slice is known UTF-8 and the
                // unchecked conversion would be sound — but the checked one costs a length-
                // proportional scan only on invalid input, and this is a request path where a
                // corrupted file must fail closed rather than reinterpret bytes.
                std::str::from_utf8(&bytes[lo..hi]).ok()
            }
            _ => None,
        }
    }

    /// Build a text column from values in slot order.
    pub fn text(values: impl IntoIterator<Item = String>) -> Codes {
        let mut bytes = Vec::new();
        let mut offsets = vec![0i64];
        for v in values {
            bytes.extend_from_slice(v.as_bytes());
            offsets.push(bytes.len() as i64);
        }
        Codes::Text {
            bytes: Buffer::from_vec(bytes),
            offsets: offsets.into(),
        }
    }

    #[inline]
    fn at(&self, slot: usize) -> u32 {
        match self {
            Codes::U8(v) => v[slot] as u32,
            Codes::U16(v) => v[slot] as u32,
            Codes::U32(v) => v[slot],
            // Only a *category* has a code, and a category is one of the three widths above. Every
            // other column answers `u32::MAX` so that a code predicate applied to it matches
            // nothing, rather than panicking or — worse — comparing an offset or a signed value to
            // a code. The operator/family check at the parse means this is unreachable through the
            // API; it is the second line of defence, not the first.
            _ => u32::MAX,
        }
    }
}

/// A numeric bound or comparand, carried in a form that does not lose the column's precision.
///
/// **`i128` rather than `f64` for integers, and that is not fussiness.** `u64` and `i64` both
/// exceed `f64`'s 2⁵³ exactly-representable range, so comparing a `u64` column through `f64` would
/// silently equate distinct values near the top of the range — an identifier column being the
/// obvious case, and `timestamp_us` sitting only an order of magnitude below the cliff. `i128`
/// holds every `u64` and every `i64` exactly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scalar {
    Int(i128),
    Float(f64),
}

/// What a bound becomes once narrowed to a column's own type.
enum Narrowed<T> {
    /// No constraint on this side — the bound lies beyond the type's range in the permissive
    /// direction, or was absent.
    Unbounded,
    /// Nothing can satisfy it: the bound lies beyond the type's range in the excluding direction.
    Unsatisfiable,
    /// An inclusive native bound. Exclusivity is folded in by moving the bound one step, which is
    /// exact for integers.
    At(T),
}

/// The lower bound as an **inclusive** native value.
fn narrow_lo<T>(e: Option<Endpoint>) -> Narrowed<T>
where
    T: TryFrom<i128> + Bounded,
{
    let Some(e) = e else {
        return Narrowed::Unbounded;
    };
    // `gt x` over integers is `gte x+1`; the saturating add keeps the shift exact at the ceiling,
    // where `x+1` would not exist and the answer is "nothing above it".
    let want = match e.value {
        Scalar::Int(i) if e.inclusive => i,
        Scalar::Int(i) => i.saturating_add(1),
        // A fractional lower bound rounds *up* to the next integer the column can hold: `> 3.2`
        // and `>= 3.2` both admit 4 and exclude 3.
        Scalar::Float(f) => {
            if f.is_nan() {
                return Narrowed::Unsatisfiable;
            }
            f.ceil() as i128
        }
    };
    match T::try_from(want) {
        Ok(v) => Narrowed::At(v),
        // Below the floor: every value satisfies it. Above the ceiling: none does.
        Err(_) if want < T::min_i128() => Narrowed::Unbounded,
        Err(_) => Narrowed::Unsatisfiable,
    }
}

/// The upper bound as an **inclusive** native value.
fn narrow_hi<T>(e: Option<Endpoint>) -> Narrowed<T>
where
    T: TryFrom<i128> + Bounded,
{
    let Some(e) = e else {
        return Narrowed::Unbounded;
    };
    let want = match e.value {
        Scalar::Int(i) if e.inclusive => i,
        Scalar::Int(i) => i.saturating_sub(1),
        Scalar::Float(f) => {
            if f.is_nan() {
                return Narrowed::Unsatisfiable;
            }
            f.floor() as i128
        }
    };
    match T::try_from(want) {
        Ok(v) => Narrowed::At(v),
        Err(_) if want > T::max_i128() => Narrowed::Unbounded,
        Err(_) => Narrowed::Unsatisfiable,
    }
}

/// The integer widths' extremes as `i128`, so `narrow_*` can tell "below the floor" (no constraint)
/// from "above the ceiling" (nothing matches) without a per-type arm.
trait Bounded {
    fn min_i128() -> i128;
    fn max_i128() -> i128;
}
macro_rules! bounded {
    ($($t:ty),*) => { $(impl Bounded for $t {
        fn min_i128() -> i128 { <$t>::MIN as i128 }
        fn max_i128() -> i128 { <$t>::MAX as i128 }
    })* };
}
bounded!(u8, u16, u32, u64, i8, i16, i32, i64);

fn as_f64(s: Scalar) -> f64 {
    match s {
        Scalar::Int(i) => i as f64,
        Scalar::Float(f) => f,
    }
}

/// One endpoint of a range: a value and whether it is included.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Endpoint {
    pub value: Scalar,
    pub inclusive: bool,
}

/// What scans have traversed: the unit the work assertions are written in (see this module's
/// header for why it is counted at all, and why it is counted here).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanWork {
    /// Contiguous slot ranges visited — the candidate's own run structure, clipped to the column's
    /// presence and cut at [`CHUNK`]. The traversal's answer to "how much of the candidate was
    /// consulted".
    pub runs: u64,
    /// Slots those ranges cover: one per candidate entity the column holds a value for, which is
    /// the number of values the predicate was compared against.
    pub slots: u64,
}

#[cfg(debug_assertions)]
thread_local! {
    static SCAN_WORK: Cell<ScanWork> = const { Cell::new(ScanWork { runs: 0, slots: 0 }) };
}

/// Record one slot range against the calling thread's counter.
#[cfg(debug_assertions)]
#[inline]
fn record_slot_run(slots: usize) {
    SCAN_WORK.with(|w| {
        let mut work = w.get();
        work.runs += 1;
        work.slots += slots as u64;
        w.set(work);
    });
}

#[cfg(not(debug_assertions))]
#[inline(always)]
fn record_slot_run(_slots: usize) {}

/// The work this thread's scans have traversed since this was last called, and reset.
///
/// **Read-and-reset rather than read**, so a caller that forgets to clear the counter measures the
/// scan it just ran rather than that scan plus everything before it — the failure mode of a
/// peek-only accessor is a work assertion that passes on the wrong number.
///
/// Zero in a `--release` build, where nothing records: see this module's header.
#[cfg(debug_assertions)]
pub fn take_scan_work() -> ScanWork {
    SCAN_WORK.with(|w| w.replace(ScanWork::default()))
}

#[cfg(not(debug_assertions))]
pub fn take_scan_work() -> ScanWork {
    ScanWork::default()
}

/// One filterable column: its values in entity order, and how an entity id reaches one.
#[derive(Debug)]
pub struct ValueColumn {
    codes: Codes,
    /// Present entities. `None` means every entity in `0..codes.len()` carries a value, and the
    /// entity id is the slot — the case that costs nothing to address.
    presence: Option<Bitmap>,
}

impl ValueColumn {
    /// A column every entity carries a value in.
    pub fn universal(codes: Codes) -> Self {
        ValueColumn {
            codes,
            presence: None,
        }
    }

    /// A column only some entities carry a value in. `presence` must have exactly one set bit per
    /// value, ascending — checked, not trusted: a mismatch would silently pair every entity after
    /// the discrepancy with another entity's value, which is a disclosure with no symptom.
    pub fn partial(codes: Codes, presence: Bitmap) -> io::Result<Self> {
        if presence.cardinality() != codes.len() as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "value column: presence has {} entities but {} values were supplied",
                    presence.cardinality(),
                    codes.len()
                ),
            ));
        }
        Ok(ValueColumn {
            codes,
            presence: Some(presence),
        })
    }

    /// Visit the candidate as **contiguous slot ranges**: `(slot0, count, entity0)`, meaning slots
    /// `slot0..slot0 + count` hold the values of entities `entity0..entity0 + count`.
    ///
    /// **This is the whole traversal, and it exists once.** Every predicate over every family
    /// reaches its values through this function, which is what makes the timing property a property
    /// of the module rather than of each scan: the ranges are a function of `(candidate, presence)`
    /// alone and never of what is being sought, so a predicate cannot skip work whatever it tests
    /// for. Adding a family adds a comparison and cannot add a channel.
    ///
    /// It hands out *ranges* rather than single slots so that each family can walk its own storage
    /// without a bounds check per element — a fixed-width column iterates a slice of values, a text
    /// column iterates a slice of offsets. Handing out one slot at a time would force both into
    /// indexed access and cost the fixed-width case the property it goes fast on.
    ///
    /// **No range exceeds [`CHUNK`]**, which is what lets [`Hits`] bound its buffer with a check per
    /// range instead of per entity. A broad candidate is one enormous run — a 25% candidate at 10⁹
    /// is a single 250-million-entity range — so without the cap the result would be accumulated
    /// whole before it became a bitmap. Splitting costs nothing: the pieces are still contiguous
    /// slices walked the same way, and 64 Ki of them amortises any per-range overhead many times
    /// over.
    #[inline]
    fn for_each_slot_run(
        &self,
        candidate: &Bitmap,
        len: usize,
        mut f: impl FnMut(usize, usize, u32),
    ) {
        // Counted here, once, for the same reason the traversal is here once: a range reaches a
        // predicate only through this call, so this is the one place that can see all of the work
        // and none of what a caller does with it. Nothing in release builds — see the header.
        let mut f = |slot0: usize, count: usize, entity0: u32| {
            record_slot_run(count);
            f(slot0, count, entity0);
        };
        match &self.presence {
            // The entity id is the slot, so a candidate **run** is a contiguous slice of the value
            // array. The run structure is the *candidate's*, so a scattered candidate degenerates
            // to one run per entity and pays what a per-value walk paid — the honest outcome rather
            // than a regression.
            None => {
                let bound = len as u32;
                for_each_run(candidate, |start, last| {
                    let last = last.min(bound.saturating_sub(1));
                    if start > last {
                        return;
                    }
                    f(start as usize, (last - start) as usize + 1, start);
                });
            }
            // Slot *k* is the *k*-th set bit of the presence bitmap, and rank is **affine inside a
            // run**: entity `e` in a presence run from `ps` with `base` bits before it is at slot
            // `base + (e − ps)`. Merging the two bitmaps' runs therefore gives every slot by
            // arithmetic, at O(runs) — where stepping the bitmap a bit at a time would cost
            // O(present) however small the candidate was.
            Some(presence) => {
                let live = candidate.and(presence);
                let mut pres = RunIter::new(presence);
                let mut liv = RunIter::new(&live);
                let mut base: u64 = 0;
                let mut p = pres.next();
                let mut l = liv.next();
                while let (Some((ps, pl)), Some((ls, ll))) = (p, l) {
                    if pl < ls {
                        base += u64::from(pl - ps) + 1;
                        p = pres.next();
                        continue;
                    }
                    if ll < ps {
                        l = liv.next();
                        continue;
                    }
                    let lo = ls.max(ps);
                    let hi = ll.min(pl);
                    let slot0 = (base + u64::from(lo - ps)) as usize;
                    let count = (hi - lo) as usize + 1;
                    if slot0 < len {
                        f(slot0, count.min(len - slot0), lo);
                    }
                    if ll <= pl {
                        l = liv.next();
                    } else {
                        base += u64::from(pl - ps) + 1;
                        p = pres.next();
                    }
                }
            }
        }
    }

    /// Walk the candidate over a **typed slice**, keeping entities whose value satisfies `pred`.
    ///
    /// **The column's type is matched once per scan, not once per entity.** An untyped walk passing
    /// `(&Codes, slot)` to its predicate makes every element pay a match on the storage enum and a
    /// call through a closure that cannot be specialised. This takes the slice directly, so the
    /// inner loop is a monomorphic index and comparison over `&[T]` — which is what a fixed-width
    /// column can actually go fast on.
    ///
    /// One transcription of the traversal, not a fast path beside a slow one: `walk` is kept only
    /// for the variable-width text column, whose values are not a slice of anything.
    #[inline]
    fn walk_typed<T, F>(&self, candidate: &Bitmap, values: &[T], mut pred: F) -> Bitmap
    where
        F: FnMut(&T) -> bool,
    {
        let mut hits = Hits::new();
        let mut packed = pack::Sink::new();
        let mut words = [0u64; pack::WORDS];
        // Slot and entity coincide only where the entity id *is* the array index. A presence bitmap
        // makes a block's values a rank-addressed subsequence rather than a contiguous slice, so it
        // keeps the per-entity path — see `pack`'s module docs.
        let positional = self.presence.is_none();
        let mut packing = Packing::Undecided;

        self.for_each_slot_run(candidate, values.len(), |slot0, count, entity0| {
            // **Whole 2¹⁶-aligned blocks are packed into Roaring containers directly**, which is
            // what keeps an unselective predicate inside the filter budget: measured at 10⁹, a
            // predicate matching half a whole-corpus candidate cost 5.9 s inserted and 0.83 s packed,
            // and one matching three quarters 10.2 s against 0.84 s. The cost is *flat* in how much
            // matches, where the per-entity path is not.
            //
            // The condition is the candidate's shape and the column's addressing, never the values,
            // so this adds no dependence on what is being sought.
            if positional
                && count >= pack::BLOCK
                && pack_run(
                    values,
                    slot0,
                    count,
                    entity0,
                    &mut pred,
                    &mut hits,
                    &mut packed,
                    &mut words,
                    &mut packing,
                )
            {
                return;
            }
            // **A scattered candidate is one-element runs**, and building a slice iterator for each
            // costs more than the direct index it replaces — measured as a 20% regression on the
            // scattered arm before this branch existed. The contiguous case is where the slice walk
            // pays, so it is the branch that gets it.
            if count == 1 {
                if pred(&values[slot0]) {
                    hits.push(entity0);
                }
                return;
            }
            for (i, v) in values[slot0..slot0 + count].iter().enumerate() {
                if pred(v) {
                    hits.push(entity0 + i as u32);
                }
            }
        });

        // The two paths cover disjoint entity ranges by construction — the packed blocks are exactly
        // the aligned interior each run's per-entity walk skipped — so the union is exact and its
        // order does not matter.
        let mut out = hits.finish();
        out.or_inplace(&packed.finish());
        out
    }

    /// Walk the candidate over a **text column**, keeping entities whose bytes satisfy `pred`.
    ///
    /// **The predicate sees bytes, not `&str`, and that is where the cost went.** Resolving a slot
    /// to a `&str` runs a UTF-8 validation over the value — for every candidate entity, on every
    /// request, over bytes Arrow already validated when the column was opened. Measured at 10⁸ that
    /// was the dominant term in every text predicate: equality cost 11.2 ns per candidate entity
    /// against a category's 0.24 ns, and `contains` 40 ns.
    ///
    /// **A byte comparison answers the same question**, because UTF-8 is self-synchronising: a
    /// valid UTF-8 needle cannot occur in a valid UTF-8 haystack starting part-way through a
    /// character, since every continuation byte is `10xxxxxx` and no lead byte is. So byte equality,
    /// byte prefix and byte substring agree with their `str` counterparts on validated input, and
    /// the validation is what the file format already guarantees.
    #[inline]
    fn walk_text<F>(&self, candidate: &Bitmap, mut pred: F) -> Bitmap
    where
        F: FnMut(&[u8]) -> bool,
    {
        let Codes::Text { bytes, offsets } = &self.codes else {
            // A byte predicate against a numeric column matches nothing, which is the same answer
            // the operator/family check at the parse already gives. This is the second line of
            // defence, not the first.
            return Bitmap::new();
        };
        let mut hits = Hits::new();
        self.for_each_slot_run(candidate, self.codes.len(), |slot0, count, entity0| {
            // `offsets` has one more element than there are values, so a run of `count` values
            // needs `count + 1` offsets — walked as overlapping pairs, which is the text-shaped
            // equivalent of the fixed-width arm's slice walk and avoids a bounds check per value.
            for (i, w) in offsets[slot0..=slot0 + count].windows(2).enumerate() {
                let (lo, hi) = (w[0] as usize, w[1] as usize);
                if pred(&bytes[lo..hi]) {
                    hits.push(entity0 + i as u32);
                }
            }
        });
        hits.finish()
    }

    /// Entities whose UTF-8 value equals `needle`, restricted to `candidate`.
    ///
    /// A string column needs no dictionary to answer this: the comparison is against the stored
    /// bytes (`Codes::Text`).
    pub fn scan_text_eq(&self, candidate: &Bitmap, needle: &str) -> Bitmap {
        let needle = needle.as_bytes();
        self.walk_text(candidate, |v| v == needle)
    }

    /// Entities whose numeric value lies within the given bounds, restricted to `candidate`.
    ///
    /// Either endpoint may be absent, which is an open side — `{gte: 30}` is everything from 30 up.
    /// Both absent matches every entity carrying *any* value, which is the honest reading of "no
    /// constraint" and is distinct from matching every entity: an item with no value has nothing to
    /// compare, so it matches no range, exactly as it matches no equality.
    ///
    /// **A range is a scan, and that is the whole numeric design.** No level tree, no bit slicing,
    /// no zone map: measured, a 25%-coverage range at 10⁹ costs 730 ms against a 0.5–1 s filter
    /// budget (`probes/2026-08-08-filter-layout/`). Zone maps were declined outright rather than
    /// deferred, because their block skip consults *unmasked* extrema — timing would reveal whether
    /// invisible rows fall in the queried range, a C4-shape channel bought for latency the budget
    /// already affords (`filter-index.md` §3).
    pub fn scan_range(
        &self,
        candidate: &Bitmap,
        lo: Option<Endpoint>,
        hi: Option<Endpoint>,
    ) -> Bitmap {
        // **Narrowed once, before the loop.** An `i128`/`f64` bound compared per element would
        // widen every value on every comparison; narrowing to the column's own type here leaves
        // the inner loop a native compare over a slice. `narrow_*` also settles the two degenerate
        // outcomes up front — a bound below the type's floor constrains nothing, one above its
        // ceiling excludes everything — rather than rediscovering them a billion times.
        macro_rules! int_range {
            ($v:expr, $t:ty) => {{
                let lo_b = match narrow_lo::<$t>(lo) {
                    Narrowed::Unsatisfiable => return Bitmap::new(),
                    Narrowed::Unbounded => None,
                    Narrowed::At(x) => Some(x),
                };
                let hi_b = match narrow_hi::<$t>(hi) {
                    Narrowed::Unsatisfiable => return Bitmap::new(),
                    Narrowed::Unbounded => None,
                    Narrowed::At(x) => Some(x),
                };
                self.walk_typed(candidate, $v, move |x| {
                    lo_b.is_none_or(|b| *x >= b) && hi_b.is_none_or(|b| *x <= b)
                })
            }};
        }
        // Floats keep the `f64` comparison: NaN must stay unordered, which is the whole reason no
        // order-preserving key is needed, and narrowing through an integer would destroy it.
        macro_rules! float_range {
            ($v:expr, $t:ty) => {{
                let lo_f = lo.map(|e| (as_f64(e.value), e.inclusive));
                let hi_f = hi.map(|e| (as_f64(e.value), e.inclusive));
                self.walk_typed(candidate, $v, move |x| {
                    let x = *x as f64;
                    lo_f.is_none_or(|(b, inc)| if inc { x >= b } else { x > b })
                        && hi_f.is_none_or(|(b, inc)| if inc { x <= b } else { x < b })
                })
            }};
        }
        match &self.codes {
            Codes::U8(v) => int_range!(v, u8),
            Codes::U16(v) => int_range!(v, u16),
            Codes::U32(v) => int_range!(v, u32),
            Codes::U64(v) => int_range!(v, u64),
            Codes::I8(v) => int_range!(v, i8),
            Codes::I16(v) => int_range!(v, i16),
            Codes::I32(v) => int_range!(v, i32),
            Codes::I64(v) => int_range!(v, i64),
            Codes::F32(v) => float_range!(v, f32),
            Codes::F64(v) => float_range!(v, f64),
            Codes::Text { .. } => Bitmap::new(),
        }
    }

    /// Entities whose numeric value equals `needle`. A degenerate range, kept separate because a
    /// client writing `eq` means equality and should not have to spell it as two bounds.
    pub fn scan_num_eq(&self, candidate: &Bitmap, needle: Scalar) -> Bitmap {
        self.scan_num_in(candidate, std::slice::from_ref(&needle))
    }

    /// Entities whose numeric value equals any of `needles` — `eq` over a list, as for the other
    /// two families.
    pub fn scan_num_in(&self, candidate: &Bitmap, needles: &[Scalar]) -> Bitmap {
        macro_rules! int_in {
            ($v:expr, $t:ty) => {{
                // Values outside the column's type match nothing and are dropped here rather than
                // compared away per element.
                let mut w: Vec<$t> = needles
                    .iter()
                    .filter_map(|n| match n {
                        Scalar::Int(i) => <$t>::try_from(*i).ok(),
                        Scalar::Float(_) => None,
                    })
                    .collect();
                // **Returning early here is not the channel it resembles.** The scan is otherwise
                // careful never to let its running time depend on what is sought — a value the
                // principal cannot see must cost what a value that does not exist costs
                // (per-point-attributes §3.8). This branch fires only when *every* needle is
                // unrepresentable in the column's declared width, and that width is published to
                // every principal alike in `/v1/meta`'s `declared_scalars.arrow_type`. So what the
                // timing reveals is a fact the client was handed before it asked, and no
                // *vocabulary* question — which codes exist, which are held, which are visible —
                // is answerable through it.
                if w.is_empty() {
                    return Bitmap::new();
                }
                // Sorted and searched rather than scanned: a linear `contains` costs O(k) per
                // *candidate entity*, which measured 5.5 ns against equality's 0.26 for a 32-value
                // set at 10⁸ — the set-membership cost that made a tick-box filter more expensive
                // than the budget allows.
                w.sort_unstable();
                w.dedup();
                self.walk_typed(candidate, $v, move |x| w.binary_search(x).is_ok())
            }};
        }
        macro_rules! float_in {
            ($v:expr) => {{
                let w: Vec<f64> = needles.iter().map(|n| as_f64(*n)).collect();
                // NaN equals nothing, itself included — so a NaN needle matches no row, which the
                // comparison gives without a special case.
                self.walk_typed(candidate, $v, move |x| {
                    let x = *x as f64;
                    w.iter().any(|n| x == *n)
                })
            }};
        }
        match &self.codes {
            Codes::U8(v) => int_in!(v, u8),
            Codes::U16(v) => int_in!(v, u16),
            Codes::U32(v) => int_in!(v, u32),
            Codes::U64(v) => int_in!(v, u64),
            Codes::I8(v) => int_in!(v, i8),
            Codes::I16(v) => int_in!(v, i16),
            Codes::I32(v) => int_in!(v, i32),
            Codes::I64(v) => int_in!(v, i64),
            Codes::F32(v) => float_in!(v),
            Codes::F64(v) => float_in!(v),
            Codes::Text { .. } => Bitmap::new(),
        }
    }

    /// Entities whose UTF-8 value equals any of `needles`, restricted to `candidate`.
    ///
    /// **`in` is `eq` over a list**, and that generalisation is not category-only: a string's
    /// values are compared for equality exactly as a category's codes are, so set membership means
    /// the same thing over both families. What differs is only what a value *is*.
    ///
    /// One pass with a sorted needle list, for the reason [`Self::scan_in`] gives: a per-needle
    /// loop would make the running time proportional to how many needles *match*, which is the
    /// channel this module's candidate-first discipline exists to deny.
    pub fn scan_text_in(&self, candidate: &Bitmap, needles: &[String]) -> Bitmap {
        let wanted = ByteSet::new(needles.iter().map(|n| n.as_bytes()));
        self.walk_text(candidate, |v| wanted.contains(v))
    }

    /// Entities whose UTF-8 value starts with `prefix`, restricted to `candidate`.
    ///
    /// The FST an inverted design needed here existed to walk *distinct values in order*, which is
    /// only necessary when a prefix has to be turned into a set of value identifiers to look up.
    /// Testing a stored value directly needs no ordering at all.
    pub fn scan_text_prefix(&self, candidate: &Bitmap, prefix: &str) -> Bitmap {
        let prefix = prefix.as_bytes();
        self.walk_text(candidate, |v| v.starts_with(prefix))
    }

    /// Entities whose UTF-8 value contains `needle`, restricted to `candidate`.
    ///
    /// **This is why substring matching stopped needing a trigram index.** A trigram conjunction
    /// returns a *superset* that must be verified against the stored value, and the cut to issue #44
    /// was made because a filter-only attribute had no route to that value. The value column is that
    /// route, and with it the verification step *is* the whole operation — there is nothing left for
    /// the trigram index to accelerate that the budget does not already afford.
    pub fn scan_text_contains(&self, candidate: &Bitmap, needle: &str) -> Bitmap {
        let needle = needle.as_bytes();
        let Codes::Text { bytes, offsets } = &self.codes else {
            return Bitmap::new();
        };
        // The empty needle is contained in every value, so there is nothing to search for; the
        // per-value walk answers it without a special case in the loop below.
        if needle.is_empty() {
            return self.walk_text(candidate, |_| true);
        }

        // **A contiguous run's values are adjacent bytes, so the search runs over the region rather
        // than over each value.** The per-value loop was spending its time on memory rather than
        // comparison — measured, a contiguous candidate's cost is ~80% inner loop and ~10% cache
        // misses, the reverse of the scattered case — and searching the concatenation amortises the
        // scan across every value in the run at SIMD throughput. Measured at 10⁸: 11.4 → 1.7 ns per
        // candidate entity on a 25% candidate, and 15.9 → 7.5 ns for a needle a quarter of the
        // values contain (probe arm 12).
        //
        // This is a property of the *candidate's* run structure, not of the values: a scattered
        // candidate degenerates to one value per run and takes the same per-value path it always
        // did. Nothing here depends on what is being sought.
        let finder = memchr::memmem::Finder::new(needle);
        let mut hits = Hits::new();
        self.for_each_slot_run(candidate, self.codes.len(), |slot0, count, entity0| {
            if count == 1 {
                let (lo, hi) = (offsets[slot0] as usize, offsets[slot0 + 1] as usize);
                if finder.find(&bytes[lo..hi]).is_some() {
                    hits.push(entity0);
                }
                return;
            }
            search_region(
                &finder,
                needle.len(),
                bytes,
                offsets,
                slot0,
                count,
                entity0,
                &mut hits,
            );
        });
        hits.finish()
    }

    /// The UTF-8 value an entity carries, or `None` where it carries none or the column is numeric.
    pub fn text_of(&self, entity: u32) -> Option<&str> {
        self.slot_of(entity)
            .and_then(|slot| self.codes.text_at(slot))
    }

    fn slot_of(&self, entity: u32) -> Option<usize> {
        match &self.presence {
            None => {
                let slot = entity as usize;
                (slot < self.codes.len()).then_some(slot)
            }
            Some(presence) => presence
                .contains(entity)
                .then(|| (presence.rank(entity) - 1) as usize),
        }
    }

    /// Entities carrying `value`, **restricted to `candidate`**.
    ///
    /// The candidate is `M_auth` (or a narrower composition of it), pushed in first as §8.2
    /// requires. The result is therefore already inside the authorised set — unlike
    /// [`crate::ColumnPostings::entities`], which resolves over the whole corpus and leaves the
    /// intersection to its caller.
    pub fn scan_eq(&self, candidate: &Bitmap, value: AttrLocalId) -> Bitmap {
        let w = value.raw();
        match &self.codes {
            Codes::U8(v) => self.walk_typed(candidate, v, |x| u32::from(*x) == w),
            Codes::U16(v) => self.walk_typed(candidate, v, |x| u32::from(*x) == w),
            Codes::U32(v) => self.walk_typed(candidate, v, |x| *x == w),
            // Only a category has a code, and a category is one of the three widths above.
            _ => Bitmap::new(),
        }
    }

    /// Entities carrying any of `values`, restricted to `candidate` — set membership, one pass.
    ///
    /// One pass rather than a union of per-value scans: the work stays a function of the candidate
    /// alone, so an IN-set naming ten invisible values costs what one naming ten visible values
    /// costs. A per-value loop would make the running time proportional to the number of *matching*
    /// values, which is the channel [`Self::scan_eq`]'s doc comment exists to deny.
    pub fn scan_in(&self, candidate: &Bitmap, values: &[AttrLocalId]) -> Bitmap {
        match &self.codes {
            // **A bit per code point, built once per scan.** A `u8` category's domain is 256 codes
            // and a `u16`'s is 65,536, so the whole membership question fits in 32 bytes or 8 KB —
            // small enough to stay in cache and answer in constant time however many values the set
            // names. Searching a sorted needle list instead made the scan cost O(log k) per
            // *candidate entity*: measured at 10⁸, a 32-value set cost 5.5 ns per candidate against
            // equality's 0.26, which at 10⁹ and 25% coverage is 1.4 s — outside the filter budget
            // for a filter a viewer builds by ticking boxes.
            //
            // The table also makes the work **independent of which codes are asked for**, which is
            // stronger than the sorted list it replaces: an unheld code and a heavily-held one cost
            // the same table build and the same per-element lookup.
            Codes::U8(v) => {
                let set = CodeSet::new(values, u8::MAX as u32);
                self.walk_typed(candidate, v, |x| set.contains(u32::from(*x)))
            }
            Codes::U16(v) => {
                let set = CodeSet::new(values, u16::MAX as u32);
                self.walk_typed(candidate, v, |x| set.contains(u32::from(*x)))
            }
            // A `u32` domain is 4×10⁹ codes, which is not a table. Sorted and searched, which is
            // O(log k) — and k is the number of values a client named, not a corpus quantity.
            Codes::U32(v) => {
                let mut w: Vec<u32> = values.iter().map(|v| v.raw()).collect();
                w.sort_unstable();
                w.dedup();
                self.walk_typed(candidate, v, |x| w.binary_search(x).is_ok())
            }
            _ => Bitmap::new(),
        }
    }

    /// The value an entity carries, or `None` where it carries none.
    ///
    /// This is the direction an inverted index cannot answer, and having it is why the conformance
    /// oracle reads the artefact under test rather than a parallel relation the fold could forget
    /// (`filter-index.md` §9), and why substring matching needs no trigram index to verify against.
    pub fn value_of(&self, entity: u32) -> Option<AttrLocalId> {
        let slot = self.slot_of(entity)?;
        match &self.codes {
            Codes::Text { .. } => None,
            codes => Some(AttrLocalId::new(codes.at(slot))),
        }
    }

    /// The column's values, in slot order — what the write side slices when it merges layers, and
    /// what tells it the column's family.
    ///
    /// **Slots, not entities**: reaching one from an entity id is [`Self::value_of`]'s business,
    /// and a caller that indexes this by an entity id has silently assumed universal presence.
    /// Public because `tessera-filter-write` is a separate crate *deliberately* — see its own
    /// module doc: nothing that writes this artefact may share a codegen unit with the scan.
    pub fn codes(&self) -> &Codes {
        &self.codes
    }

    /// Entities of `candidate` this column holds a value for.
    ///
    /// **The predicate `none_of` is built on**, and the reason a negation is expressible without
    /// inverting the failure arithmetic every safety argument here rests on (filter-index §5): an
    /// entity whose value is unreachable — not yet flushed, in a layer that failed to compose,
    /// blanked at the fold — is absent from this set, so it matches no `none_of` either. Losing
    /// values still under-reports, and under-reporting still narrows `M_sel`.
    ///
    /// [`Self::present`]'s intersected form, and cheaper for the universal case: a column every
    /// entity carries a value in owns no presence bitmap, so this clips the candidate to the
    /// column's own extent instead of materialising a run of every slot and intersecting.
    pub fn present_in(&self, candidate: &Bitmap) -> Bitmap {
        match &self.presence {
            None => {
                if self.codes.len() == 0 {
                    return Bitmap::new();
                }
                candidate.and(&Bitmap::from_range(0u32..self.codes.len() as u32))
            }
            Some(p) => p.and(candidate),
        }
    }

    /// Entities this column holds a value for. `None` presence means the dense range.
    pub fn present(&self) -> Bitmap {
        match &self.presence {
            None => {
                let mut b = Bitmap::new();
                if self.codes.len() > 0 {
                    b.add_range(0u32..self.codes.len() as u32);
                }
                b.run_optimize();
                b
            }
            Some(p) => p.clone(),
        }
    }

    /// Read the column in `dir`, written by [`write_value_column`].
    ///
    /// **Takes the directory, not the two paths, so a caller cannot forget the presence bitmap.**
    /// A column whose presence file exists but is not read addresses every slot after the first
    /// absent entity by the wrong entity id — every value shifted along by one, no error anywhere,
    /// and a filter reporting items as carrying values they do not have. The presence file's
    /// existence *is* the signal that addressing is not positional, so the two must be resolved
    /// together.
    pub fn open_dir(dir: &Path, access: Access) -> io::Result<Self> {
        let presence = dir.join(PRESENCE_FILE);
        Self::open(
            &dir.join(VALUES_FILE),
            presence.exists().then_some(presence.as_path()),
            access,
        )
    }

    /// Read a column from explicit paths. Prefer [`Self::open_dir`], which cannot mismatch them.
    ///
    /// **The presence bitmap is read into memory either way, and only the values are mapped.** The
    /// asymmetry is the measured size ratio: at 10⁹ the values are 1 GB per byte of declared width
    /// while the presence bitmap is 36 KB for the slice-blocked shape and 125 MB at its scattered
    /// worst (probe `2026-08-08-filter-layout`). Roaring also wants its own owned representation to
    /// answer a rank in the scan's inner loop, so mapping it would buy little and cost the run-merge
    /// its structure.
    pub fn open(
        values_path: &Path,
        presence_path: Option<&Path>,
        access: Access,
    ) -> io::Result<Self> {
        let codes = read_values(values_path, access)?;
        match presence_path {
            None => Ok(ValueColumn::universal(codes)),
            Some(path) => {
                let bytes = std::fs::read(path)?;
                let presence = Bitmap::try_deserialize::<Portable>(&bytes).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "presence bitmap at {} is not portable Roaring",
                            path.display()
                        ),
                    )
                })?;
                ValueColumn::partial(codes, presence)
            }
        }
    }
}

/// How a value column's bytes are obtained, and what the caller promises about its access pattern.
///
/// This is an enum rather than the `bool` it replaced because there are three cases and two of them
/// are both "mapped": the distinction the third makes is **whose** mapping it is. Decision 0052
/// rules that the fold's page-cache mitigation is a hint on mappings *the fold owns*, and that it
/// must never be applied to the request path's — so the advice cannot be a property of the file, or
/// of a global setting, and has to arrive from the caller that knows which of the two it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Read into one owned buffer. For a caller that wants the bytes off the file system, and for
    /// the writers' own read-backs in tests.
    Read,
    /// Mapped, faulted on demand, no advice. **The request path's mode** — a scan touches the part
    /// of the column its candidate covers and residency follows the working set rather than the
    /// declared schema (§8).
    Mapped,
    /// Mapped and advised `MADV_SEQUENTIAL`. For a background pass that streams a column **exactly
    /// once** through a mapping it owns: the readahead suits the access and the drop-behind is the
    /// point, since the pages are not wanted afterwards and the request path's are.
    MappedSequential,
}

impl Access {
    /// `None` to read, `Some(sequential)` to map.
    fn mapped(self) -> Option<bool> {
        match self {
            Access::Read => None,
            Access::Mapped => Some(false),
            Access::MappedSequential => Some(true),
        }
    }
}

/// Read a value column's Arrow IPC file **without copying its values**.
///
/// When `mmap` is set the file is mapped and the batch decoded straight out of the mapping; when it
/// is not, the file is read into one owned buffer and decoded from that. Either way the `Codes`
/// buffers are windows onto the backing bytes — the `ScalarBuffer`s hold the reference-counted
/// `Buffer`, which owns the mapping, so the column stays valid for as long as it is held and the
/// pages are faulted in on demand rather than at open. This is the same construction
/// `PostingsReader::open` uses, for the same reason and with the same safety argument.
///
/// **The values file carries exactly one record batch**, and a second is refused rather than
/// concatenated. Concatenating would copy — which is the whole cost this exists to avoid — and there
/// is no bundle that holds a multi-batch value column: the writer emits one batch, and pre-release
/// there is no past to be compatible with (decision 0048).
fn read_values(path: &Path, access: Access) -> io::Result<Codes> {
    use arrow::array::{Array, LargeStringArray};
    use arrow::datatypes::DataType;

    let buffer = if let Some(sequential) = access.mapped() {
        let file = std::fs::File::open(path)?;
        // SAFETY: the same argument as `PostingsReader::open`'s mmap arm. `arc` owns the mapping
        // for as long as any `Buffer` built from it is alive — it is captured as the buffer's
        // `Allocation` — the mapping is valid for `len` bytes for its whole lifetime, and
        // `memmap2::Mmap` never returns a null base pointer.
        let mapping = unsafe { memmap2::Mmap::map(&file) }?;
        if sequential {
            // **A hint, and a failure to give it is not a failure to open**
            // (decision 0052): the advice is an optimisation for a caller that streams the column
            // once, and a kernel that declines it leaves a correct mapping behind. Erroring here
            // would let an advisory call fail a fold.
            let _ = mapping.advise(memmap2::Advice::Sequential);
        }
        let len = mapping.len();
        let arc: Arc<memmap2::Mmap> = Arc::new(mapping);
        let ptr = std::ptr::NonNull::new(arc.as_ptr() as *mut u8)
            .expect("memmap2::Mmap never returns a null base pointer");
        unsafe { Buffer::from_custom_allocation(ptr, len, arc) }
    } else {
        Buffer::from_vec(std::fs::read(path)?)
    };

    // One record batch, refused rather than concatenated if there are more: the reader borrows its
    // values from the batch's buffers instead of copying them, and concatenating is the copy this
    // exists to avoid. `decode_single_batch` is where that rule is enforced.
    let batch = tessera_authz::decode_single_batch(&buffer, &format!("{}", path.display()))?;
    if batch.num_columns() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "value column at {}: expected exactly one column, found {}",
                path.display(),
                batch.num_columns()
            ),
        ));
    }
    let ty = batch.schema_ref().field(0).data_type().clone();

    // Borrowed at the declared width rather than widened: the declared width *is* the storage width
    // (Appendix A prices it at 1 GB per byte per 10⁹ per column), so reading a `u8` column through
    // `i64`s would cost eight times the memory this exists to avoid — and here it would also cost
    // the zero copy, since a widened value cannot be a window onto the file.
    macro_rules! borrow {
        ($arr:ty, $ctor:expr) => {{
            let a = batch
                .column(0)
                .as_any()
                .downcast_ref::<$arr>()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "value column at {}: the batch disagrees with the schema's type",
                            path.display()
                        ),
                    )
                })?;
            $ctor(a.values().clone())
        }};
    }

    Ok(match ty {
        DataType::UInt8 => borrow!(arrow::array::UInt8Array, Codes::U8),
        DataType::UInt16 => borrow!(arrow::array::UInt16Array, Codes::U16),
        DataType::UInt32 => borrow!(arrow::array::UInt32Array, Codes::U32),
        DataType::UInt64 => borrow!(arrow::array::UInt64Array, Codes::U64),
        DataType::Int8 => borrow!(arrow::array::Int8Array, Codes::I8),
        DataType::Int16 => borrow!(arrow::array::Int16Array, Codes::I16),
        DataType::Int32 => borrow!(arrow::array::Int32Array, Codes::I32),
        DataType::Int64 => borrow!(arrow::array::Int64Array, Codes::I64),
        DataType::Float32 => borrow!(arrow::array::Float32Array, Codes::F32),
        DataType::Float64 => borrow!(arrow::array::Float64Array, Codes::F64),
        DataType::LargeUtf8 => {
            let a = batch
                .column(0)
                .as_any()
                .downcast_ref::<LargeStringArray>()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "value column at {}: the batch disagrees with the schema's type",
                            path.display()
                        ),
                    )
                })?;
            // The offsets and the bytes are the array's own buffers, so a text column maps exactly
            // as a numeric one does. `LargeStringArray` validated UTF-8 on construction, which is
            // what lets `text_at` slice by offset without a second validation pass — it still
            // *checks* the conversion, because a corrupt file must fail closed on a request path
            // rather than reinterpret bytes.
            let offsets: ScalarBuffer<i64> = a.offsets().clone().into_inner();
            Codes::Text {
                bytes: a.values().clone(),
                offsets,
            }
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "value column at {}: unsupported arrow type {other:?}",
                    path.display()
                ),
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::values_writer::write_value_column;

    fn candidate(all: impl IntoIterator<Item = u32>) -> Bitmap {
        let mut b = Bitmap::new();
        b.add_many(&all.into_iter().collect::<Vec<_>>());
        b
    }

    #[test]
    fn a_universal_column_scans_by_direct_index() {
        let column = ValueColumn::universal(Codes::U8(vec![7, 3, 7, 9, 7].into()));
        let hits = column.scan_eq(&candidate(0..5), AttrLocalId::new(7));
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0, 2, 4]);
    }

    /// The mask goes in first: an entity carrying the value but outside the candidate is absent
    /// from the result, and never contributes work either.
    #[test]
    fn the_candidate_bounds_the_result() {
        let column = ValueColumn::universal(Codes::U8(vec![7, 3, 7, 9, 7].into()));
        let hits = column.scan_eq(&candidate([0, 1, 3]), AttrLocalId::new(7));
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0]);
    }

    #[test]
    fn a_partial_column_resolves_slots_through_presence() {
        // Entities 10, 20, 30 carry values; everything else carries none.
        let column = ValueColumn::partial(
            Codes::U16(vec![100, 200, 100].into()),
            candidate([10, 20, 30]),
        )
        .unwrap();
        let hits = column.scan_eq(&candidate(0..40), AttrLocalId::new(100));
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![10, 30]);
        assert_eq!(column.value_of(20), Some(AttrLocalId::new(200)));
        assert_eq!(column.value_of(21), None);
    }

    /// A presence bitmap that disagrees with the value count is refused rather than trusted: it
    /// would pair every entity after the discrepancy with another entity's value.
    #[test]
    fn a_presence_count_mismatch_is_refused() {
        let err =
            ValueColumn::partial(Codes::U8(vec![1, 2].into()), candidate([5, 6, 7])).unwrap_err();
        assert!(format!("{err}").contains("presence has 3 entities but 2 values"));
    }

    #[test]
    fn set_membership_is_one_pass() {
        let column = ValueColumn::universal(Codes::U16(vec![1, 2, 3, 4, 5].into()));
        let hits = column.scan_in(
            &candidate(0..5),
            &[
                AttrLocalId::new(2),
                AttrLocalId::new(5),
                AttrLocalId::new(99),
            ],
        );
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![1, 4]);
    }

    /// A value no entity carries returns empty rather than erroring — the same answer, and the
    /// same work, as a value that does not exist in the vocabulary at all.
    #[test]
    fn an_unheld_value_is_empty_rather_than_an_error() {
        let column = ValueColumn::universal(Codes::U8(vec![1, 2, 3].into()));
        assert!(column
            .scan_eq(&candidate(0..3), AttrLocalId::new(42))
            .is_empty());
    }

    fn text_column(values: &[&str]) -> ValueColumn {
        ValueColumn::universal(Codes::text(values.iter().map(|s| s.to_string())))
    }

    #[test]
    fn a_string_column_answers_equality_without_a_dictionary() {
        let column = text_column(&["smith", "smythe", "smith", "jones"]);
        let hits = column.scan_text_eq(&candidate(0..4), "smith");
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0, 2]);
    }

    /// Prefix needs no FST: the FST existed to order *distinct values* so a prefix could be turned
    /// into a set of identifiers to look up, and nothing here looks a value up.
    /// `in` over strings is `eq` over a list — the same generalisation a category gets.
    fn num_column(codes: Codes) -> ValueColumn {
        ValueColumn::universal(codes)
    }

    fn at(v: i128, inclusive: bool) -> Endpoint {
        Endpoint {
            value: Scalar::Int(v),
            inclusive,
        }
    }

    #[test]
    fn a_range_honours_each_endpoints_inclusivity() {
        let column = num_column(Codes::I32(vec![10, 20, 30, 40].into()));
        let all = candidate(0..4);
        // [20, 40]
        assert_eq!(
            column
                .scan_range(&all, Some(at(20, true)), Some(at(40, true)))
                .iter()
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        // (20, 40)
        assert_eq!(
            column
                .scan_range(&all, Some(at(20, false)), Some(at(40, false)))
                .iter()
                .collect::<Vec<_>>(),
            vec![2]
        );
        // Open above.
        assert_eq!(
            column
                .scan_range(&all, Some(at(30, true)), None)
                .iter()
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    /// **64-bit integers are compared exactly.** Two `u64`s a `f64` cannot tell apart must not be
    /// equated by a range — an identifier column is the obvious case, and `timestamp_us` sits one
    /// order of magnitude below the same cliff.
    #[test]
    fn a_u64_range_does_not_lose_precision_through_f64() {
        // 2⁵³ and 2⁵³+1 are the adjacent pair `f64` cannot separate: both round to 2⁵³. (2⁵³+2 is
        // representable, which is why the naive "+1, +3" fixture does *not* exercise this.)
        let lo = 1u64 << 53;
        let column = num_column(Codes::U64(vec![lo, lo + 1].into()));
        assert_eq!(
            lo as f64,
            (lo + 1) as f64,
            "the fixture must be a pair f64 cannot separate, or this proves nothing"
        );
        let hits = column.scan_range(
            &candidate(0..2),
            Some(at(lo as i128, true)),
            Some(at(lo as i128, true)),
        );
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0]);
    }

    /// **A bound outside the column's type is not an error and not a clamp-to-nothing.** Below the
    /// floor it constrains nothing; above the ceiling nothing satisfies it. Narrowing settles both
    /// once, so the inner loop never sees them.
    #[test]
    fn a_bound_outside_the_types_range_resolves_to_all_or_nothing() {
        let column = num_column(Codes::U8(vec![0, 128, 255].into()));
        let all = candidate(0..3);
        // `>= -5` over a u8 constrains nothing.
        assert_eq!(
            column
                .scan_range(&all, Some(at(-5, true)), None)
                .cardinality(),
            3
        );
        // `<= -1` over a u8 excludes everything.
        assert!(column.scan_range(&all, None, Some(at(-1, true))).is_empty());
        // `>= 300` likewise.
        assert!(column
            .scan_range(&all, Some(at(300, true)), None)
            .is_empty());
        // `<= 300` constrains nothing.
        assert_eq!(
            column
                .scan_range(&all, None, Some(at(300, true)))
                .cardinality(),
            3
        );
    }

    /// An exclusive integer bound is the inclusive one next door, and at the type's extreme the
    /// step must not wrap.
    #[test]
    fn an_exclusive_integer_bound_at_the_extreme_does_not_wrap() {
        let column = num_column(Codes::U8(vec![0, 1, 254, 255].into()));
        let all = candidate(0..4);
        // `> 255` is nothing, not everything.
        assert!(column
            .scan_range(&all, Some(at(255, false)), None)
            .is_empty());
        // `< 0` is nothing.
        assert!(column.scan_range(&all, None, Some(at(0, false))).is_empty());
        // `> 254` is just 255.
        assert_eq!(
            column
                .scan_range(&all, Some(at(254, false)), None)
                .iter()
                .collect::<Vec<_>>(),
            vec![3]
        );
    }

    /// A fractional bound on an integer column rounds **towards excluding** the values it sits
    /// between: `>= 3.2` and `> 3.2` both admit 4 and reject 3.
    #[test]
    fn a_fractional_bound_on_an_integer_column_rounds_outward() {
        let column = num_column(Codes::I32(vec![3, 4].into()));
        let all = candidate(0..2);
        for inclusive in [true, false] {
            let hits = column.scan_range(
                &all,
                Some(Endpoint {
                    value: Scalar::Float(3.2),
                    inclusive,
                }),
                None,
            );
            assert_eq!(
                hits.iter().collect::<Vec<_>>(),
                vec![1],
                "inclusive={inclusive}"
            );
        }
        for inclusive in [true, false] {
            let hits = column.scan_range(
                &all,
                None,
                Some(Endpoint {
                    value: Scalar::Float(3.8),
                    inclusive,
                }),
            );
            assert_eq!(
                hits.iter().collect::<Vec<_>>(),
                vec![0],
                "inclusive={inclusive}"
            );
        }
    }

    /// A NaN *bound* excludes everything, as a NaN value satisfies nothing.
    #[test]
    fn a_nan_bound_matches_nothing() {
        let column = num_column(Codes::I32(vec![1, 2, 3].into()));
        assert!(column
            .scan_range(
                &candidate(0..3),
                Some(Endpoint {
                    value: Scalar::Float(f64::NAN),
                    inclusive: true
                }),
                None
            )
            .is_empty());
    }

    /// Set membership drops needles the column's type cannot hold, rather than comparing them
    /// away per element — and a set of only such needles matches nothing.
    #[test]
    fn numeric_set_membership_drops_out_of_range_needles() {
        let column = num_column(Codes::U8(vec![1, 2].into()));
        let all = candidate(0..2);
        assert_eq!(
            column
                .scan_num_in(&all, &[Scalar::Int(2), Scalar::Int(9999)])
                .iter()
                .collect::<Vec<_>>(),
            vec![1]
        );
        assert!(column.scan_num_in(&all, &[Scalar::Int(9999)]).is_empty());
    }

    /// NaN satisfies no bound and no equality — inherited from IEEE rather than implemented, and
    /// the reason this design needs no order-preserving key.
    #[test]
    fn a_nan_matches_no_range_and_no_equality() {
        let column = num_column(Codes::F64(vec![1.0, f64::NAN, 3.0].into()));
        let all = candidate(0..3);
        let wide = column.scan_range(
            &all,
            Some(Endpoint {
                value: Scalar::Float(f64::NEG_INFINITY),
                inclusive: true,
            }),
            Some(Endpoint {
                value: Scalar::Float(f64::INFINITY),
                inclusive: true,
            }),
        );
        assert_eq!(wide.iter().collect::<Vec<_>>(), vec![0, 2], "NaN is absent");
        assert!(column.scan_num_eq(&all, Scalar::Float(f64::NAN)).is_empty());
    }

    /// An unbounded range matches every entity **carrying a value** — not every entity. An item
    /// with no value has nothing to compare, exactly as for equality.
    #[test]
    fn an_unbounded_range_still_excludes_absent_values() {
        let column =
            ValueColumn::partial(Codes::I32(vec![5, 7].into()), candidate([1, 4])).unwrap();
        let hits = column.scan_range(&candidate(0..6), None, None);
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![1, 4]);
    }

    /// A range over a text column matches nothing rather than comparing an offset to a bound.
    #[test]
    fn a_range_on_text_matches_nothing() {
        let column = text_column(&["1", "2"]);
        assert!(column
            .scan_range(&candidate(0..2), Some(at(0, true)), Some(at(9, true)))
            .is_empty());
    }

    #[test]
    fn numeric_set_membership_is_exact() {
        let column = num_column(Codes::I64(vec![1, 2, 3].into()));
        let hits = column.scan_num_in(
            &candidate(0..3),
            &[Scalar::Int(1), Scalar::Int(3), Scalar::Int(99)],
        );
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0, 2]);
    }

    #[test]
    fn a_string_column_answers_set_membership() {
        let column = text_column(&["smith", "smythe", "jones", "smith"]);
        let hits = column.scan_text_in(
            &candidate(0..4),
            &[
                "smith".to_string(),
                "jones".to_string(),
                "absent".to_string(),
            ],
        );
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0, 2, 3]);
    }

    /// A **code**-valued set predicate over a text column matches nothing: the two `in` spellings
    /// do not cross, because a text column has no code to compare.
    #[test]
    fn a_code_set_predicate_on_text_matches_nothing() {
        let column = text_column(&["1", "2"]);
        assert!(column
            .scan_in(
                &candidate(0..2),
                &[AttrLocalId::new(1), AttrLocalId::new(2)]
            )
            .is_empty());
    }

    #[test]
    fn a_string_column_answers_prefix_without_an_fst() {
        let column = text_column(&["smith", "smythe", "smote", "jones"]);
        let hits = column.scan_text_prefix(&candidate(0..4), "sm");
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0, 1, 2]);
        assert!(column.scan_text_prefix(&candidate(0..4), "zz").is_empty());
    }

    /// Substring was cut to #44 because a trigram conjunction returns a superset needing
    /// verification against the stored value, and a filter-only attribute had no route to that
    /// value. The value column is that route, and the verification step is the whole operation.
    #[test]
    fn a_string_column_answers_substring_without_a_trigram_index() {
        let column = text_column(&["blacksmith", "smythe", "goldsmith", "jones"]);
        let hits = column.scan_text_contains(&candidate(0..4), "smith");
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0, 2]);
    }

    /// **A value's bytes are concatenated with its neighbours', and a match must not span them.**
    /// The predicate sees one value's slice, so "bc" cannot be found across "ab" ++ "cd" — the case
    /// a mis-sliced offset pair would produce, silently and with plausible-looking results.
    #[test]
    fn a_substring_does_not_match_across_two_values() {
        let column = text_column(&["ab", "cd", "bc"]);
        assert_eq!(
            column
                .scan_text_contains(&candidate(0..3), "bc")
                .iter()
                .collect::<Vec<_>>(),
            vec![2],
            "only the value that actually contains it"
        );
        assert_eq!(
            column
                .scan_text_prefix(&candidate(0..3), "bc")
                .iter()
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    /// The text predicates compare **bytes**, which agrees with the `str` semantics they replaced
    /// because UTF-8 is self-synchronising — no valid needle can match starting inside a character.
    /// Multi-byte values are where a byte comparison would show it if that reasoning were wrong.
    #[test]
    fn multibyte_values_compare_by_bytes_and_agree_with_str() {
        let column = text_column(&["naïve", "日本語", "naive", "café"]);
        let all = candidate(0..4);

        assert_eq!(
            column
                .scan_text_eq(&all, "naïve")
                .iter()
                .collect::<Vec<_>>(),
            vec![0],
            "the two-byte ï does not equate to the one-byte i"
        );
        assert_eq!(
            column
                .scan_text_prefix(&all, "na")
                .iter()
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert_eq!(
            column
                .scan_text_contains(&all, "本")
                .iter()
                .collect::<Vec<_>>(),
            vec![1],
            "a multi-byte needle inside a multi-byte value"
        );
        assert_eq!(
            column
                .scan_text_in(&all, &["café".into(), "日本語".into()])
                .iter()
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        // The empty needle is contained in everything, as `str::contains` also holds.
        assert_eq!(column.scan_text_contains(&all, "").cardinality(), 4);
    }

    /// The needle set buckets on a value's first byte, so the empty string — which has none — is
    /// the case that has to be carried separately, and a corpus may legitimately hold it (an empty
    /// string is a value; absence is a null, which `an_absent_string_is_not_an_empty_string`
    /// covers).
    #[test]
    fn a_needle_set_handles_the_empty_string_and_repeats() {
        let column = text_column(&["", "a", "bb", ""]);
        let all = candidate(0..4);

        assert_eq!(
            column
                .scan_text_in(&all, &["".into(), "bb".into()])
                .iter()
                .collect::<Vec<_>>(),
            vec![0, 2, 3],
            "the empty needle matches the empty values and nothing else"
        );
        assert!(
            column
                .scan_text_in(&all, &["a".into(), "a".into()])
                .iter()
                .eq([1]),
            "a repeated needle is one needle"
        );
        assert!(
            column.scan_text_in(&all, &[]).is_empty(),
            "no needle, no match"
        );
    }

    /// The code table is built over the column's declared width, so its extremes must be members
    /// and anything past them must be dropped rather than wrapped into a neighbour's bit.
    #[test]
    fn a_code_set_covers_its_domains_extremes_and_drops_what_is_past_them() {
        let column = ValueColumn::universal(Codes::U8(vec![0, 42, 255].into()));
        let all = candidate(0..3);

        assert_eq!(
            column
                .scan_in(&all, &[AttrLocalId::new(0), AttrLocalId::new(255)])
                .iter()
                .collect::<Vec<_>>(),
            vec![0, 2],
            "both ends of a u8 domain are members"
        );
        // 256 is not representable in the column and 511 differs from 255 only above the width —
        // the case a table indexed without a range check would fold onto a real code.
        assert!(column
            .scan_in(&all, &[AttrLocalId::new(256), AttrLocalId::new(511)])
            .is_empty());
        assert_eq!(
            column
                .scan_in(&all, &[AttrLocalId::new(511), AttrLocalId::new(42)])
                .iter()
                .collect::<Vec<_>>(),
            vec![1],
            "an out-of-domain needle drops without disturbing the rest of the set"
        );

        let wide = ValueColumn::universal(Codes::U16(vec![0, 65_535].into()));
        assert_eq!(
            wide.scan_in(&candidate(0..2), &[AttrLocalId::new(65_535)])
                .iter()
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    /// **The result accumulator has three paths and the small fixtures reach only one of them.**
    /// Matches are coalesced into ranges above `RUN_MIN`, buffered below it, and the buffer is
    /// folded every `CHUNK` — none of which a twenty-element column touches. These walk a column
    /// past both boundaries, in each of the shapes the three paths correspond to.
    #[test]
    fn a_result_is_complete_across_the_run_and_fold_boundaries() {
        // Comfortably past CHUNK (65,536) so the buffer folds several times and the traversal
        // splits its range.
        let n: u32 = (CHUNK as u32) * 2 + 1_000;

        // (1) Every entity matches — one long run per split range, the add_range path throughout.
        let dense = ValueColumn::universal(Codes::U8(vec![7u8; n as usize].into()));
        let all = {
            let mut b = Bitmap::new();
            b.add_range(0..n);
            b.run_optimize();
            b
        };
        let hits = dense.scan_eq(&all, AttrLocalId::new(7));
        assert_eq!(hits.cardinality(), u64::from(n), "every entity matches");
        assert_eq!(hits.minimum(), Some(0));
        assert_eq!(hits.maximum(), Some(n - 1));
        // The fold boundary itself, and the two entities either side of it, must all be present —
        // an off-by-one in the buffer flush would drop exactly one of these.
        for e in [CHUNK as u32 - 1, CHUNK as u32, CHUNK as u32 + 1, n - 1] {
            assert!(hits.contains(e), "entity {e} across the fold boundary");
        }

        // (2) Alternating — every run is length 1, so nothing coalesces and everything goes through
        // the buffer, folding repeatedly.
        let alternating: Vec<u8> = (0..n).map(|e| (e % 2) as u8).collect();
        let column = ValueColumn::universal(Codes::U8(alternating.into()));
        let hits = column.scan_eq(&all, AttrLocalId::new(0));
        assert_eq!(hits.cardinality(), u64::from(n.div_ceil(2)));
        assert!(hits.contains(0) && !hits.contains(1));
        assert!(hits.contains(CHUNK as u32), "the boundary entity is even");

        // (3) One long run in the middle of a sparse column — the mixed case, where a range and
        // buffered singles must compose into one result without losing either.
        let mut mixed: Vec<u8> = vec![0u8; n as usize];
        mixed[5] = 7;
        for v in mixed.iter_mut().take(2_000).skip(1_000) {
            *v = 7;
        }
        mixed[n as usize - 1] = 7;
        let column = ValueColumn::universal(Codes::U8(mixed.into()));
        let hits = column.scan_eq(&all, AttrLocalId::new(7));
        let want: Vec<u32> = std::iter::once(5)
            .chain(1_000..2_000)
            .chain(std::iter::once(n - 1))
            .collect();
        assert_eq!(hits.to_vec(), want, "a range and its scattered neighbours");
    }

    /// **The packed path must agree with the definition, at every shape of candidate that reaches
    /// it.** Whole 2¹⁶-aligned blocks are answered by assembling Roaring containers by hand
    /// (`pack`), and the ragged ends either side of them by the per-entity walk — so the cases that
    /// matter are the seams: a candidate starting mid-block, ending mid-block, exactly aligned, and
    /// split across several runs. The expectation here is a literal per-entity filter, not another
    /// route through the same code.
    #[test]
    fn the_packed_path_agrees_with_a_per_entity_definition() {
        const B: u32 = pack::BLOCK as u32;
        let n = B * 3 + 5_000;
        // Three densities: one that lands every block in a bitset container, one in an array
        // container, and one that matches nothing at all.
        for (label, modulus) in [("dense", 2u32), ("sparse", 900), ("none", 0)] {
            let values: Vec<u8> = (0..n)
                .map(|e| {
                    if modulus != 0 && e % modulus == 0 {
                        7
                    } else {
                        1
                    }
                })
                .collect();
            let column = ValueColumn::universal(Codes::U8(values.clone().into()));

            // Runs as (start, end) pairs rather than ranges, so a one-run shape is still a list.
            let shapes: [(&str, &[(u32, u32)]); 6] = [
                ("whole column", &[(0, n)]),
                ("ragged both ends", &[(7, B * 2 + 33)]),
                ("aligned exactly", &[(B, B * 3)]),
                ("one block only", &[(B, B * 2)]),
                ("just under a block", &[(B + 1, B * 2)]),
                (
                    "several runs",
                    &[(0, B + 9), (B + 100, B * 2 + 7), (B * 2 + 50, n)],
                ),
            ];

            for (shape, runs) in shapes {
                let mut candidate = Bitmap::new();
                for &(lo, hi) in runs {
                    candidate.add_range(lo..hi);
                }
                candidate.run_optimize();

                let want: Vec<u32> = candidate
                    .iter()
                    .filter(|&e| values[e as usize] == 7)
                    .collect();
                let got = column.scan_eq(&candidate, AttrLocalId::new(7));
                assert_eq!(got.to_vec(), want, "{label} / {shape}");
            }
        }
    }

    /// The packed path and the per-entity path must not disagree about a column whose length is not
    /// a multiple of the block size — the trailing partial block is the one a packer is most likely
    /// to run past the end of, or to drop entirely.
    #[test]
    fn a_partial_trailing_block_is_neither_dropped_nor_overrun() {
        const B: u32 = pack::BLOCK as u32;
        let n = B + 3; // one whole block, then three entities
        let values: Vec<u8> = (0..n).map(|e| (e % 2) as u8).collect();
        let column = ValueColumn::universal(Codes::U8(values.into()));
        let mut all = Bitmap::new();
        all.add_range(0..n);
        all.run_optimize();

        let got = column.scan_eq(&all, AttrLocalId::new(0));
        assert_eq!(got.cardinality(), u64::from(n.div_ceil(2)));
        assert_eq!(got.maximum(), Some(B + 2), "the trailing block is included");
        assert!(got.contains(B), "the entity just past the packed block");
        // And a candidate that stops inside the packed block must not gain its remainder.
        let mut short = Bitmap::new();
        short.add_range(0..(B - 1));
        short.run_optimize();
        assert_eq!(
            column.scan_eq(&short, AttrLocalId::new(0)).maximum(),
            Some(B - 2),
            "nothing beyond the candidate"
        );
    }

    /// The same boundaries on the text walker, which has its own accumulator call sites.
    #[test]
    fn a_text_result_is_complete_across_the_fold_boundary() {
        let n: u32 = (CHUNK as u32) + 500;
        let column = ValueColumn::universal(Codes::text((0..n).map(|e| {
            if e % 3 == 0 {
                "hit".into()
            } else {
                "miss".into()
            }
        })));
        let mut all = Bitmap::new();
        all.add_range(0..n);
        all.run_optimize();

        let hits = column.scan_text_eq(&all, "hit");
        assert_eq!(hits.cardinality(), u64::from(n.div_ceil(3)));
        assert!(hits.contains(CHUNK as u32 - (CHUNK as u32 % 3)));
        assert_eq!(hits.maximum(), Some((n - 1) - ((n - 1) % 3)));

        // A prefix every value shares is the text column's dense case.
        let all_hit = column.scan_text_prefix(&all, "");
        assert_eq!(all_hit.cardinality(), u64::from(n));
    }

    /// **The region search must agree with a per-value definition**, over candidate shapes that
    /// exercise its two moving parts: the cursor that maps a match back to the value it fell in, and
    /// the boundary test that discards a match spanning two values. The expectation is a literal
    /// per-value `contains`, not another route through the same code.
    #[test]
    fn the_region_search_agrees_with_a_per_value_definition() {
        // Values chosen so the concatenation manufactures substrings none of them contain: "ab" ++
        // "ba" reads as "abba", and repeats put the needle in one value twice.
        let corpus: Vec<String> = (0..500)
            .map(|i| match i % 7 {
                0 => "ab".into(),
                1 => "ba".into(),
                2 => "xabx".into(),
                3 => "abab".into(), // two occurrences in one value
                4 => "".into(),
                5 => "zzzz".into(),
                _ => format!("q{i}ab"),
            })
            .collect();
        let column = ValueColumn::universal(Codes::text(corpus.iter().cloned()));
        let n = corpus.len() as u32;

        let shapes: [(&str, &[(u32, u32)]); 4] = [
            ("all", &[(0, n)]),
            ("one run, offset start", &[(3, n - 3)]),
            ("many runs", &[(0, 10), (11, 12), (13, 100), (150, n)]),
            ("alternating singles", &[(0, 1), (2, 3), (4, 5), (6, 7)]),
        ];
        for needle in ["ab", "abba", "zz", "q", "nowhere", "abab"] {
            for (shape, runs) in shapes {
                let mut candidate = Bitmap::new();
                for &(lo, hi) in runs {
                    candidate.add_range(lo..hi);
                }
                candidate.run_optimize();

                let want: Vec<u32> = candidate
                    .iter()
                    .filter(|&e| corpus[e as usize].contains(needle))
                    .collect();
                assert_eq!(
                    column.scan_text_contains(&candidate, needle).to_vec(),
                    want,
                    "needle {needle:?} / {shape}"
                );
            }
        }
    }

    /// A value holding the needle more than once is one entity, not several — the region search
    /// sees every occurrence and `Hits` requires strictly ascending entities.
    #[test]
    fn a_repeated_needle_records_its_entity_once() {
        let column = text_column(&["aaaa", "b", "aa"]);
        let hits = column.scan_text_contains(&candidate(0..3), "a");
        assert_eq!(hits.to_vec(), vec![0, 2]);
        assert_eq!(
            hits.cardinality(),
            2,
            "each entity once, however many matches"
        );
    }

    /// The mask still goes in first for text, by the same shared walker every other family uses.
    #[test]
    fn the_candidate_bounds_a_text_result() {
        let column = text_column(&["a", "a", "a", "a"]);
        let hits = column.scan_text_prefix(&candidate([1, 3]), "a");
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![1, 3]);
    }

    /// A string column's absent values are its presence bitmap's business, exactly as a
    /// category's are — there is no in-band empty-string sentinel, because the empty string is a
    /// value a corpus may legitimately hold.
    #[test]
    fn an_absent_string_is_not_an_empty_string() {
        let column = ValueColumn::partial(
            Codes::text(["".to_string(), "x".to_string()]),
            candidate([5, 9]),
        )
        .unwrap();
        assert_eq!(column.text_of(5), Some(""));
        assert_eq!(column.text_of(7), None);
        // Entity 5 holds the empty string and matches an empty-prefix test; entity 7 holds no
        // value and matches nothing at all.
        let hits = column.scan_text_prefix(&candidate(0..10), "");
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![5, 9]);
    }

    /// A numeric predicate against a text column matches nothing rather than comparing an offset
    /// to a code.
    #[test]
    fn a_numeric_predicate_on_text_matches_nothing() {
        let column = text_column(&["1", "2"]);
        assert!(column
            .scan_eq(&candidate(0..2), AttrLocalId::new(1))
            .is_empty());
        assert_eq!(column.value_of(0), None);
    }

    #[test]
    fn a_text_column_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let values = dir.path().join("values.arrow");
        let presence = dir.path().join("presence.roaring");
        let codes = Codes::text(["alpha".to_string(), "".to_string(), "gamma".to_string()]);
        write_value_column(&values, &presence, &codes, None).unwrap();
        let column = ValueColumn::open(&values, None, Access::Read).unwrap();
        assert_eq!(column.text_of(0), Some("alpha"));
        assert_eq!(column.text_of(1), Some(""));
        assert_eq!(column.text_of(2), Some("gamma"));
        assert_eq!(
            column
                .scan_text_contains(&candidate(0..3), "amm")
                .iter()
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn a_column_round_trips_through_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let values = dir.path().join("values.arrow");
        let presence = dir.path().join("presence.roaring");

        let codes = Codes::U32(vec![5, 6, 7].into());
        let present = candidate([2, 4, 8]);
        write_value_column(&values, &presence, &codes, Some(&present)).unwrap();
        let column = ValueColumn::open(&values, Some(&presence), Access::Read).unwrap();
        assert_eq!(column.value_of(4), Some(AttrLocalId::new(6)));
        assert_eq!(column.value_of(3), None);
        assert_eq!(column.present().iter().collect::<Vec<_>>(), vec![2, 4, 8]);

        // Universal presence writes no bitmap and reads back without one.
        let values2 = dir.path().join("v2.arrow");
        write_value_column(&values2, &presence, &Codes::U8(vec![9, 8].into()), None).unwrap();
        let dense = ValueColumn::open(&values2, None, Access::Read).unwrap();
        assert_eq!(dense.value_of(0), Some(AttrLocalId::new(9)));
        assert_eq!(dense.value_of(5), None);
    }

    /// The mapped column and the read one must answer identically, for **every** family — the
    /// engine maps and the tests mostly read, so a divergence would be invisible until it was
    /// served. Text is the case worth naming: its bytes and offsets are two separate buffers
    /// borrowed from the same array, so a column that mapped its bytes and copied its offsets (or
    /// the reverse) would read plausible values at the wrong boundaries rather than fail.
    #[test]
    fn mapping_a_column_and_reading_it_answer_identically() {
        let dir = tempfile::tempdir().unwrap();
        let presence = dir.path().join("presence.roaring");

        for (name, codes) in [
            ("u8", Codes::U8(vec![7, 3, 7, 9].into())),
            ("u32", Codes::U32(vec![5, 6, 7, 5].into())),
            ("i64", Codes::I64(vec![-9, 0, 1 << 40, 3].into())),
            ("f64", Codes::F64(vec![-1.5, 0.0, 2.25, 9.0].into())),
            (
                "text",
                Codes::text(["alpha", "", "gamma", "alpha"].map(String::from)),
            ),
        ] {
            let values = dir.path().join(format!("{name}.arrow"));
            write_value_column(&values, &presence, &codes, None).unwrap();

            let mapped = ValueColumn::open(&values, None, Access::Mapped).unwrap();
            let read = ValueColumn::open(&values, None, Access::Read).unwrap();
            let all = candidate(0..4);

            for slot in 0..4u32 {
                assert_eq!(
                    mapped.text_of(slot),
                    read.text_of(slot),
                    "{name}: text at {slot}"
                );
                assert_eq!(
                    mapped.value_of(slot),
                    read.value_of(slot),
                    "{name}: value at {slot}"
                );
            }
            assert_eq!(
                mapped.scan_range(&all, None, None).cardinality(),
                read.scan_range(&all, None, None).cardinality(),
                "{name}: an unbounded range covers the same slots"
            );
            assert_eq!(
                mapped.scan_text_prefix(&all, "alph").to_vec(),
                read.scan_text_prefix(&all, "alph").to_vec(),
                "{name}: prefix"
            );
        }
    }
}
