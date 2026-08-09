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

use std::io;
use std::path::Path;
use std::sync::Arc;

use arrow::buffer::{Buffer, ScalarBuffer};
use croaring::{Bitmap, Portable};
use tessera_types::AttrLocalId;

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
    fn len(&self) -> usize {
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

    /// Walk the candidate over a **typed slice**, keeping entities whose value satisfies `pred`.
    ///
    /// **The column's type is matched once per scan, not once per entity.** [`Self::walk`] passes
    /// `(&Codes, slot)` to its predicate, so every element pays a match on the storage enum and a
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
        let mut hits: Vec<u32> = Vec::new();
        match &self.presence {
            None => {
                let bound = values.len() as u32;
                for_each_run(candidate, |start, last| {
                    let last = last.min(bound.saturating_sub(1));
                    if start > last {
                        return;
                    }
                    // **A scattered candidate is one-element runs**, and building a slice iterator
                    // for each costs more than the direct index it replaces — measured as a 20%
                    // regression on the scattered arm before this branch existed. The contiguous
                    // case is where the slice walk pays, so it is the branch that gets it.
                    if start == last {
                        if pred(&values[start as usize]) {
                            hits.push(start);
                        }
                        return;
                    }
                    let base = start;
                    for (i, v) in values[start as usize..=last as usize].iter().enumerate() {
                        if pred(v) {
                            hits.push(base + i as u32);
                        }
                    }
                });
            }
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
                    // Rank is affine inside a presence run, so the overlap maps to a contiguous
                    // slice of the value array — the same property the universal arm gets for free.
                    let slot0 = (base + u64::from(lo - ps)) as usize;
                    let len = (hi - lo) as usize + 1;
                    for (i, v) in values[slot0..slot0 + len].iter().enumerate() {
                        if pred(v) {
                            hits.push(lo + i as u32);
                        }
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
        let mut out = Bitmap::new();
        out.add_many(&hits);
        out
    }

    /// Walk the candidate, resolving each entity to its slot, and keep the entities whose value
    /// satisfies `keep`.
    ///
    /// **Every predicate goes through this one loop**, which is what makes the timing property a
    /// property of the module rather than of each function: the traversal is a function of
    /// `(candidate, presence)` alone, and `keep` sees a slot only after the traversal has already
    /// decided to visit it. A predicate cannot skip work no matter what it is testing for, so
    /// adding a family adds a comparison and cannot add a channel.
    #[inline]
    fn walk(&self, candidate: &Bitmap, mut keep: impl FnMut(&Codes, usize) -> bool) -> Bitmap {
        let mut hits: Vec<u32> = Vec::new();
        match &self.presence {
            // The entity id is the slot, so a candidate **run** is a contiguous slice of the value
            // array — walked as an integer range rather than stepped through the bitmap one value
            // at a time. That is the same trade `compose::for_each_run_in` makes for the mask, and
            // for the same reason: the per-value cursor step, not the comparison, is where the time
            // goes on a contiguous candidate.
            //
            // The run structure is the *candidate's*, so this is still work as a function of
            // `(candidate, column)` — a scattered candidate degenerates to one run per entity and
            // pays what it paid before, which is the honest outcome rather than a regression.
            None => {
                let bound = self.codes.len() as u32;
                for_each_run(candidate, |start, last| {
                    // Inclusive `last`, and clamped rather than incremented: a run ending at
                    // `u32::MAX` would overflow on `last + 1`, which is the edge
                    // `compose::for_each_run_in` also carries a test for.
                    let last = last.min(bound.saturating_sub(1));
                    for e in start..=last {
                        if keep(&self.codes, e as usize) {
                            hits.push(e);
                        }
                    }
                });
            }
            // Container arithmetic first, so blocks the candidate does not touch are never visited,
            // then a **run-merge** to turn entity ids into slots.
            //
            // Rank is what makes this path expensive: slot *k* is the *k*-th set bit of `presence`,
            // so a naive walk steps `presence` one bit at a time and costs O(present) however small
            // the candidate is — measured at 1,078 ms against a bare array's 28.7 ms at 10⁹. But
            // rank is *affine inside a run*: within one presence run starting at `ps` with `base`
            // set bits before it, entity `e` is at slot `base + (e - ps)`. So walking both bitmaps
            // as runs gives every slot by arithmetic, at O(runs) rather than O(entities).
            //
            // It degrades to the old cost rather than past it: a scattered presence has one run per
            // entity, which is the case the naive walk was already paying for.
            Some(presence) => {
                let live = candidate.and(presence);
                let mut pres = RunIter::new(presence);
                let mut liv = RunIter::new(&live);
                // Set bits before the current presence run — the run's base slot.
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
                        // `live ⊆ presence`, so this cannot happen for a well-formed column; the
                        // arm keeps the merge total rather than looping forever if it ever does.
                        l = liv.next();
                        continue;
                    }
                    let lo = ls.max(ps);
                    let hi = ll.min(pl);
                    for e in lo..=hi {
                        let slot = base + u64::from(e - ps);
                        if keep(&self.codes, slot as usize) {
                            hits.push(e);
                        }
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
        let mut out = Bitmap::new();
        out.add_many(&hits);
        out
    }

    /// Entities whose UTF-8 value equals `needle`, restricted to `candidate`.
    ///
    /// A string column needs no dictionary to answer this: the comparison is against the stored
    /// bytes (`Codes::Text`).
    pub fn scan_text_eq(&self, candidate: &Bitmap, needle: &str) -> Bitmap {
        self.walk(candidate, |codes, slot| {
            codes.text_at(slot) == Some(needle)
        })
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
                let w: Vec<$t> = needles
                    .iter()
                    .filter_map(|n| match n {
                        Scalar::Int(i) => <$t>::try_from(*i).ok(),
                        Scalar::Float(_) => None,
                    })
                    .collect();
                if w.is_empty() {
                    return Bitmap::new();
                }
                self.walk_typed(candidate, $v, move |x| w.contains(x))
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
        let mut wanted: Vec<&str> = needles.iter().map(String::as_str).collect();
        wanted.sort_unstable();
        wanted.dedup();
        self.walk(candidate, |codes, slot| {
            codes
                .text_at(slot)
                .is_some_and(|v| wanted.binary_search(&v).is_ok())
        })
    }

    /// Entities whose UTF-8 value starts with `prefix`, restricted to `candidate`.
    ///
    /// The FST an inverted design needed here existed to walk *distinct values in order*, which is
    /// only necessary when a prefix has to be turned into a set of value identifiers to look up.
    /// Testing a stored value directly needs no ordering at all.
    pub fn scan_text_prefix(&self, candidate: &Bitmap, prefix: &str) -> Bitmap {
        self.walk(candidate, |codes, slot| {
            codes.text_at(slot).is_some_and(|v| v.starts_with(prefix))
        })
    }

    /// Entities whose UTF-8 value contains `needle`, restricted to `candidate`.
    ///
    /// **This is why substring matching stopped needing a trigram index.** A trigram conjunction
    /// returns a *superset* that must be verified against the stored value, and the cut to issue #44
    /// was made because a filter-only attribute had no route to that value. The value column is that
    /// route, and with it the verification step *is* the whole operation — there is nothing left for
    /// the trigram index to accelerate that the budget does not already afford.
    pub fn scan_text_contains(&self, candidate: &Bitmap, needle: &str) -> Bitmap {
        self.walk(candidate, |codes, slot| {
            codes.text_at(slot).is_some_and(|v| v.contains(needle))
        })
    }

    /// The UTF-8 value an entity carries, or `None` where it carries none or the column is numeric.
    pub fn text_of(&self, entity: u32) -> Option<&str> {
        self.slot_of(entity).and_then(|slot| self.codes.text_at(slot))
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
        let mut w: Vec<u32> = values.iter().map(|v| v.raw()).collect();
        w.sort_unstable();
        w.dedup();
        match &self.codes {
            Codes::U8(v) => self.walk_typed(candidate, v, |x| w.binary_search(&u32::from(*x)).is_ok()),
            Codes::U16(v) => {
                self.walk_typed(candidate, v, |x| w.binary_search(&u32::from(*x)).is_ok())
            }
            Codes::U32(v) => self.walk_typed(candidate, v, |x| w.binary_search(x).is_ok()),
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
    pub fn open_dir(dir: &Path, mmap: bool) -> io::Result<Self> {
        let presence = dir.join(PRESENCE_FILE);
        Self::open(
            &dir.join(VALUES_FILE),
            presence.exists().then_some(presence.as_path()),
            mmap,
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
    pub fn open(values_path: &Path, presence_path: Option<&Path>, mmap: bool) -> io::Result<Self> {
        let codes = read_values(values_path, mmap)?;
        match presence_path {
            None => Ok(ValueColumn::universal(codes)),
            Some(path) => {
                let bytes = std::fs::read(path)?;
                let presence = Bitmap::try_deserialize::<Portable>(&bytes).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("presence bitmap at {} is not portable Roaring", path.display()),
                    )
                })?;
                ValueColumn::partial(codes, presence)
            }
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
fn read_values(path: &Path, mmap: bool) -> io::Result<Codes> {
    use arrow::array::{Array, LargeStringArray};
    use arrow::datatypes::DataType;

    let buffer = if mmap {
        let file = std::fs::File::open(path)?;
        // SAFETY: the same argument as `PostingsReader::open`'s mmap arm. `arc` owns the mapping
        // for as long as any `Buffer` built from it is alive — it is captured as the buffer's
        // `Allocation` — the mapping is valid for `len` bytes for its whole lifetime, and
        // `memmap2::Mmap` never returns a null base pointer.
        let mapping = unsafe { memmap2::Mmap::map(&file) }?;
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


/// Write a value column, and its presence bitmap where presence is partial.
///
/// `presence` is `None` when every entity in `0..codes.len()` carries a value. Writing an
/// all-ones bitmap instead would be correct and would cost the scan its fast path, so the
/// distinction is carried in the file set rather than in the bitmap's contents.
pub fn write_value_column(
    values_path: &Path,
    presence_path: &Path,
    codes: &Codes,
    presence: Option<&Bitmap>,
) -> io::Result<()> {
    use arrow::array::{ArrayRef, UInt16Array, UInt32Array, UInt8Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    // The array borrows the `Codes` buffers rather than rebuilding them, so writing a column costs
    // no second copy of it — which matters most at the fold, where every column is rewritten.
    let (array, ty): (ArrayRef, DataType) = match codes {
        Codes::U8(v) => (Arc::new(UInt8Array::new(v.clone(), None)), DataType::UInt8),
        Codes::U16(v) => (
            Arc::new(UInt16Array::new(v.clone(), None)),
            DataType::UInt16,
        ),
        Codes::U32(v) => (
            Arc::new(UInt32Array::new(v.clone(), None)),
            DataType::UInt32,
        ),
        Codes::U64(v) => (
            Arc::new(arrow::array::UInt64Array::new(v.clone(), None)),
            DataType::UInt64,
        ),
        Codes::I8(v) => (
            Arc::new(arrow::array::Int8Array::new(v.clone(), None)),
            DataType::Int8,
        ),
        Codes::I16(v) => (
            Arc::new(arrow::array::Int16Array::new(v.clone(), None)),
            DataType::Int16,
        ),
        Codes::I32(v) => (
            Arc::new(arrow::array::Int32Array::new(v.clone(), None)),
            DataType::Int32,
        ),
        Codes::I64(v) => (
            Arc::new(arrow::array::Int64Array::new(v.clone(), None)),
            DataType::Int64,
        ),
        Codes::F32(v) => (
            Arc::new(arrow::array::Float32Array::new(v.clone(), None)),
            DataType::Float32,
        ),
        Codes::F64(v) => (
            Arc::new(arrow::array::Float64Array::new(v.clone(), None)),
            DataType::Float64,
        ),
        // `LargeUtf8`, not `Utf8`: 32-bit offsets cap the concatenated bytes at 2 GiB, which a
        // 10⁹-entity column passes at two bytes a value. `try_new` is what validates the offsets
        // ascend and the bytes are UTF-8, so a column that could not be read back is refused here
        // rather than at the next open.
        Codes::Text { bytes, offsets } => (
            Arc::new(
                arrow::array::LargeStringArray::try_new(
                    arrow::buffer::OffsetBuffer::new(offsets.clone()),
                    bytes.clone(),
                    None,
                )
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?,
            ),
            DataType::LargeUtf8,
        ),
    };
    let schema = Arc::new(Schema::new(vec![Field::new("value", ty, false)]));
    let batch = RecordBatch::try_new(schema.clone(), vec![array])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let file = std::fs::File::create(values_path)?;
    let mut w = arrow::ipc::writer::FileWriter::try_new(file, &schema)
        .map_err(|e| io::Error::other(e.to_string()))?;
    w.write(&batch)
        .map_err(|e| io::Error::other(e.to_string()))?;
    w.finish()
        .map_err(|e| io::Error::other(e.to_string()))?;

    if let Some(p) = presence {
        std::fs::write(presence_path, p.serialize::<Portable>())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let column =
            ValueColumn::partial(Codes::U16(vec![100, 200, 100].into()), candidate([10, 20, 30])).unwrap();
        let hits = column.scan_eq(&candidate(0..40), AttrLocalId::new(100));
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![10, 30]);
        assert_eq!(column.value_of(20), Some(AttrLocalId::new(200)));
        assert_eq!(column.value_of(21), None);
    }

    /// A presence bitmap that disagrees with the value count is refused rather than trusted: it
    /// would pair every entity after the discrepancy with another entity's value.
    #[test]
    fn a_presence_count_mismatch_is_refused() {
        let err = ValueColumn::partial(Codes::U8(vec![1, 2].into()), candidate([5, 6, 7])).unwrap_err();
        assert!(format!("{err}").contains("presence has 3 entities but 2 values"));
    }

    #[test]
    fn set_membership_is_one_pass() {
        let column = ValueColumn::universal(Codes::U16(vec![1, 2, 3, 4, 5].into()));
        let hits = column.scan_in(
            &candidate(0..5),
            &[AttrLocalId::new(2), AttrLocalId::new(5), AttrLocalId::new(99)],
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
        assert!(column
            .scan_range(&all, None, Some(at(-1, true)))
            .is_empty());
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
            assert_eq!(hits.iter().collect::<Vec<_>>(), vec![1], "inclusive={inclusive}");
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
            assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0], "inclusive={inclusive}");
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
        assert!(column
            .scan_num_in(&all, &[Scalar::Int(9999)])
            .is_empty());
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
        assert!(column
            .scan_num_eq(&all, Scalar::Float(f64::NAN))
            .is_empty());
    }

    /// An unbounded range matches every entity **carrying a value** — not every entity. An item
    /// with no value has nothing to compare, exactly as for equality.
    #[test]
    fn an_unbounded_range_still_excludes_absent_values() {
        let column = ValueColumn::partial(Codes::I32(vec![5, 7].into()), candidate([1, 4])).unwrap();
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
            &["smith".to_string(), "jones".to_string(), "absent".to_string()],
        );
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0, 2, 3]);
    }

    /// A **code**-valued set predicate over a text column matches nothing: the two `in` spellings
    /// do not cross, because a text column has no code to compare.
    #[test]
    fn a_code_set_predicate_on_text_matches_nothing() {
        let column = text_column(&["1", "2"]);
        assert!(column
            .scan_in(&candidate(0..2), &[AttrLocalId::new(1), AttrLocalId::new(2)])
            .is_empty());
    }

    #[test]
    fn a_string_column_answers_prefix_without_an_fst() {
        let column = text_column(&["smith", "smythe", "smote", "jones"]);
        let hits = column.scan_text_prefix(&candidate(0..4), "sm");
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0, 1, 2]);
        assert!(column
            .scan_text_prefix(&candidate(0..4), "zz")
            .is_empty());
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
        let column = ValueColumn::open(&values, None, false).unwrap();
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
        let column = ValueColumn::open(&values, Some(&presence), false).unwrap();
        assert_eq!(column.value_of(4), Some(AttrLocalId::new(6)));
        assert_eq!(column.value_of(3), None);
        assert_eq!(column.present().iter().collect::<Vec<_>>(), vec![2, 4, 8]);

        // Universal presence writes no bitmap and reads back without one.
        let values2 = dir.path().join("v2.arrow");
        write_value_column(&values2, &presence, &Codes::U8(vec![9, 8].into()), None).unwrap();
        let dense = ValueColumn::open(&values2, None, false).unwrap();
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

            let mapped = ValueColumn::open(&values, None, true).unwrap();
            let read = ValueColumn::open(&values, None, false).unwrap();
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
