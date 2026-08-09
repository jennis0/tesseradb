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
#[derive(Debug, Clone)]
pub enum Codes {
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
    I8(Vec<i8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    /// Also `timestamp_us` — microseconds since the epoch, stored as the `i64` it is. The *type*
    /// exists so the unit is in the manifest rather than a convention between a schema author and
    /// their client; the storage and the comparison are an `i64`'s.
    I64(Vec<i64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
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
    Text {
        bytes: Vec<u8>,
        offsets: Vec<u32>,
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
        let mut offsets = vec![0u32];
        for v in values {
            bytes.extend_from_slice(v.as_bytes());
            offsets.push(bytes.len() as u32);
        }
        Codes::Text { bytes, offsets }
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

impl Scalar {
    /// Order two scalars, `None` where no order exists.
    ///
    /// **`None` is NaN, and NaN matching nothing is the intended semantic**, inherited from IEEE
    /// rather than implemented: every comparison with NaN is false, so a NaN value satisfies no
    /// bound and no equality, including `= NaN`. That is SQL's treatment of an unknown, and it is
    /// why the design needs no order-preserving key — the sign-flip trick exists to make IEEE bits
    /// sort as unsigned bytes in a byte-ordered store, and nothing here compares bytes.
    ///
    /// A mixed Int/Float comparison goes through `f64`, which is lossy above 2⁵³ on the integer
    /// side. That is reachable only by a caller giving a fractional bound for a 64-bit integer
    /// column — `score >= 1e18.5` — where the alternative is refusing a request that plainly means
    /// something. Stated rather than hidden.
    fn partial_cmp(self, other: Scalar) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Scalar::Int(a), Scalar::Int(b)) => Some(a.cmp(&b)),
            (Scalar::Float(a), Scalar::Float(b)) => a.partial_cmp(&b),
            (Scalar::Int(a), Scalar::Float(b)) => (a as f64).partial_cmp(&b),
            (Scalar::Float(a), Scalar::Int(b)) => a.partial_cmp(&(b as f64)),
        }
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

    /// The numeric value at `slot`, or `None` for a text column.
    #[inline]
    fn numeric_at(codes: &Codes, slot: usize) -> Option<Scalar> {
        Some(match codes {
            Codes::U8(v) => Scalar::Int(v[slot] as i128),
            Codes::U16(v) => Scalar::Int(v[slot] as i128),
            Codes::U32(v) => Scalar::Int(v[slot] as i128),
            Codes::U64(v) => Scalar::Int(v[slot] as i128),
            Codes::I8(v) => Scalar::Int(v[slot] as i128),
            Codes::I16(v) => Scalar::Int(v[slot] as i128),
            Codes::I32(v) => Scalar::Int(v[slot] as i128),
            Codes::I64(v) => Scalar::Int(v[slot] as i128),
            Codes::F32(v) => Scalar::Float(v[slot] as f64),
            Codes::F64(v) => Scalar::Float(v[slot]),
            Codes::Text { .. } => return None,
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
        use std::cmp::Ordering;
        self.walk(candidate, |codes, slot| {
            let Some(v) = Self::numeric_at(codes, slot) else {
                return false;
            };
            let above = match lo {
                None => true,
                Some(e) => match v.partial_cmp(e.value) {
                    Some(Ordering::Greater) => true,
                    Some(Ordering::Equal) => e.inclusive,
                    // Less, or None — the NaN case, which satisfies nothing.
                    _ => false,
                },
            };
            above
                && match hi {
                    None => true,
                    Some(e) => match v.partial_cmp(e.value) {
                        Some(Ordering::Less) => true,
                        Some(Ordering::Equal) => e.inclusive,
                        _ => false,
                    },
                }
        })
    }

    /// Entities whose numeric value equals `needle`. A degenerate range, kept separate because a
    /// client writing `eq` means equality and should not have to spell it as two bounds.
    pub fn scan_num_eq(&self, candidate: &Bitmap, needle: Scalar) -> Bitmap {
        self.walk(candidate, |codes, slot| {
            Self::numeric_at(codes, slot)
                .and_then(|v| v.partial_cmp(needle))
                .is_some_and(|o| o == std::cmp::Ordering::Equal)
        })
    }

    /// Entities whose numeric value equals any of `needles` — `eq` over a list, as for the other
    /// two families.
    pub fn scan_num_in(&self, candidate: &Bitmap, needles: &[Scalar]) -> Bitmap {
        self.walk(candidate, |codes, slot| {
            let Some(v) = Self::numeric_at(codes, slot) else {
                return false;
            };
            needles
                .iter()
                .any(|n| v.partial_cmp(*n) == Some(std::cmp::Ordering::Equal))
        })
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
        let wanted = value.raw();
        self.walk(candidate, |codes, slot| codes.at(slot) == wanted)
    }

    /// Entities carrying any of `values`, restricted to `candidate` — set membership, one pass.
    ///
    /// One pass rather than a union of per-value scans: the work stays a function of the candidate
    /// alone, so an IN-set naming ten invisible values costs what one naming ten visible values
    /// costs. A per-value loop would make the running time proportional to the number of *matching*
    /// values, which is the channel [`Self::scan_eq`]'s doc comment exists to deny.
    pub fn scan_in(&self, candidate: &Bitmap, values: &[AttrLocalId]) -> Bitmap {
        let mut wanted: Vec<u32> = values.iter().map(|v| v.raw()).collect();
        wanted.sort_unstable();
        wanted.dedup();
        self.walk(candidate, |codes, slot| {
            wanted.binary_search(&codes.at(slot)).is_ok()
        })
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
    pub fn open_dir(dir: &Path) -> io::Result<Self> {
        let presence = dir.join(PRESENCE_FILE);
        Self::open(
            &dir.join(VALUES_FILE),
            presence.exists().then_some(presence.as_path()),
        )
    }

    /// Read a column from explicit paths. Prefer [`Self::open_dir`], which cannot mismatch them.
    pub fn open(values_path: &Path, presence_path: Option<&Path>) -> io::Result<Self> {
        let codes = read_values(values_path)?;
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

fn read_values(path: &Path) -> io::Result<Codes> {
    use arrow::array::{Array, StringArray};
    use arrow::datatypes::DataType;

    let file = std::fs::File::open(path)?;
    let reader = arrow::ipc::reader::FileReader::try_new(file, None)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    // Accumulated per width rather than through one widened buffer: the declared width *is* the
    // storage width (Appendix A prices it at 1 GB per byte per 10⁹ per column), so reading a `u8`
    // column into `i64`s and narrowing afterwards would cost eight times the memory this exists to
    // avoid.
    macro_rules! collect {
        ($batches:expr, $arr:ty, $ctor:expr) => {{
            let mut out = Vec::new();
            for batch in $batches {
                let batch =
                    batch.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                let a = batch.column(0).as_any().downcast_ref::<$arr>().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("value column at {}: batches disagree on type", path.display()),
                    )
                })?;
                out.extend(a.values().iter().copied());
            }
            $ctor(out)
        }};
    }

    let schema = reader.schema();
    let ty = schema
        .fields()
        .first()
        .map(|f| f.data_type().clone())
        // An empty file is an empty column, not an error: a schema may declare a filterable column
        // a corpus has no values for.
        .unwrap_or(DataType::UInt8);

    Ok(match ty {
        DataType::UInt8 => collect!(reader, arrow::array::UInt8Array, Codes::U8),
        DataType::UInt16 => collect!(reader, arrow::array::UInt16Array, Codes::U16),
        DataType::UInt32 => collect!(reader, arrow::array::UInt32Array, Codes::U32),
        DataType::UInt64 => collect!(reader, arrow::array::UInt64Array, Codes::U64),
        DataType::Int8 => collect!(reader, arrow::array::Int8Array, Codes::I8),
        DataType::Int16 => collect!(reader, arrow::array::Int16Array, Codes::I16),
        DataType::Int32 => collect!(reader, arrow::array::Int32Array, Codes::I32),
        DataType::Int64 => collect!(reader, arrow::array::Int64Array, Codes::I64),
        DataType::Float32 => collect!(reader, arrow::array::Float32Array, Codes::F32),
        DataType::Float64 => collect!(reader, arrow::array::Float64Array, Codes::F64),
        DataType::Utf8 => {
            let mut bytes: Vec<u8> = Vec::new();
            let mut offsets: Vec<u32> = vec![0];
            for batch in reader {
                let batch =
                    batch.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                let a = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .expect("schema says utf8");
                // Materialised rather than borrowed: `Codes::Text` owns its bytes, and
                // `StringArray` has already validated UTF-8, so the concatenation needs no second
                // validation pass.
                for k in 0..a.len() {
                    bytes.extend_from_slice(a.value(k).as_bytes());
                    offsets.push(bytes.len() as u32);
                }
            }
            Codes::Text { bytes, offsets }
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

    let (array, ty): (ArrayRef, DataType) = match codes {
        Codes::U8(v) => (Arc::new(UInt8Array::from(v.clone())), DataType::UInt8),
        Codes::U16(v) => (Arc::new(UInt16Array::from(v.clone())), DataType::UInt16),
        Codes::U32(v) => (Arc::new(UInt32Array::from(v.clone())), DataType::UInt32),
        Codes::U64(v) => (
            Arc::new(arrow::array::UInt64Array::from(v.clone())),
            DataType::UInt64,
        ),
        Codes::I8(v) => (
            Arc::new(arrow::array::Int8Array::from(v.clone())),
            DataType::Int8,
        ),
        Codes::I16(v) => (
            Arc::new(arrow::array::Int16Array::from(v.clone())),
            DataType::Int16,
        ),
        Codes::I32(v) => (
            Arc::new(arrow::array::Int32Array::from(v.clone())),
            DataType::Int32,
        ),
        Codes::I64(v) => (
            Arc::new(arrow::array::Int64Array::from(v.clone())),
            DataType::Int64,
        ),
        Codes::F32(v) => (
            Arc::new(arrow::array::Float32Array::from(v.clone())),
            DataType::Float32,
        ),
        Codes::F64(v) => (
            Arc::new(arrow::array::Float64Array::from(v.clone())),
            DataType::Float64,
        ),
        Codes::Text { .. } => {
            let n = codes.len();
            let values: Vec<&str> = (0..n).map(|k| codes.text_at(k).unwrap_or("")).collect();
            (
                Arc::new(arrow::array::StringArray::from(values)),
                DataType::Utf8,
            )
        }
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
        let column = ValueColumn::universal(Codes::U8(vec![7, 3, 7, 9, 7]));
        let hits = column.scan_eq(&candidate(0..5), AttrLocalId::new(7));
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0, 2, 4]);
    }

    /// The mask goes in first: an entity carrying the value but outside the candidate is absent
    /// from the result, and never contributes work either.
    #[test]
    fn the_candidate_bounds_the_result() {
        let column = ValueColumn::universal(Codes::U8(vec![7, 3, 7, 9, 7]));
        let hits = column.scan_eq(&candidate([0, 1, 3]), AttrLocalId::new(7));
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![0]);
    }

    #[test]
    fn a_partial_column_resolves_slots_through_presence() {
        // Entities 10, 20, 30 carry values; everything else carries none.
        let column =
            ValueColumn::partial(Codes::U16(vec![100, 200, 100]), candidate([10, 20, 30])).unwrap();
        let hits = column.scan_eq(&candidate(0..40), AttrLocalId::new(100));
        assert_eq!(hits.iter().collect::<Vec<_>>(), vec![10, 30]);
        assert_eq!(column.value_of(20), Some(AttrLocalId::new(200)));
        assert_eq!(column.value_of(21), None);
    }

    /// A presence bitmap that disagrees with the value count is refused rather than trusted: it
    /// would pair every entity after the discrepancy with another entity's value.
    #[test]
    fn a_presence_count_mismatch_is_refused() {
        let err = ValueColumn::partial(Codes::U8(vec![1, 2]), candidate([5, 6, 7])).unwrap_err();
        assert!(format!("{err}").contains("presence has 3 entities but 2 values"));
    }

    #[test]
    fn set_membership_is_one_pass() {
        let column = ValueColumn::universal(Codes::U16(vec![1, 2, 3, 4, 5]));
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
        let column = ValueColumn::universal(Codes::U8(vec![1, 2, 3]));
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
        let column = num_column(Codes::I32(vec![10, 20, 30, 40]));
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
        let column = num_column(Codes::U64(vec![lo, lo + 1]));
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

    /// NaN satisfies no bound and no equality — inherited from IEEE rather than implemented, and
    /// the reason this design needs no order-preserving key.
    #[test]
    fn a_nan_matches_no_range_and_no_equality() {
        let column = num_column(Codes::F64(vec![1.0, f64::NAN, 3.0]));
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
        let column = ValueColumn::partial(Codes::I32(vec![5, 7]), candidate([1, 4])).unwrap();
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
        let column = num_column(Codes::I64(vec![1, 2, 3]));
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
        let column = ValueColumn::open(&values, None).unwrap();
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

        let codes = Codes::U32(vec![5, 6, 7]);
        let present = candidate([2, 4, 8]);
        write_value_column(&values, &presence, &codes, Some(&present)).unwrap();
        let column = ValueColumn::open(&values, Some(&presence)).unwrap();
        assert_eq!(column.value_of(4), Some(AttrLocalId::new(6)));
        assert_eq!(column.value_of(3), None);
        assert_eq!(column.present().iter().collect::<Vec<_>>(), vec![2, 4, 8]);

        // Universal presence writes no bitmap and reads back without one.
        let values2 = dir.path().join("v2.arrow");
        write_value_column(&values2, &presence, &Codes::U8(vec![9, 8]), None).unwrap();
        let dense = ValueColumn::open(&values2, None).unwrap();
        assert_eq!(dense.value_of(0), Some(AttrLocalId::new(9)));
        assert_eq!(dense.value_of(5), None);
    }
}
