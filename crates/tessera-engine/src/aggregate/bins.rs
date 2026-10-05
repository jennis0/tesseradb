//! A grouping by bins of a number or timestamp field: a histogram.
//!
//! The bins' edges are the request's range cut into equal widths, or, with no range, readable
//! edges around the smallest and largest value among the items the viewer may see in the view.
//! That default is taken over the whole visible set and never over the filtered set, the
//! reference or a region, so the edges hold still while a filter or the viewport changes. It is
//! taken inside the visible set, so an item the viewer may not see moves no edge. The edges are
//! fixed at the table's first page and carried in the cursor.
//!
//! A bin holds the values from its lower edge up to but not including its upper edge, and the
//! last bin also holds its upper edge. `rest` counts the items whose value lies in no bin: outside
//! a range the request gave, or NaN, or infinite. `none` counts the items with no value. An
//! integer is compared with an edge exactly, through the smallest integer at or above the edge,
//! so a `range` filter between two edges matches exactly the bin's items.
//!
//! Readable edges are multiples of 1, 2, 2.5 or 5 times a power of ten, whole numbers on an
//! integer field. On a timestamp field they fall on whole seconds, minutes, hours, days, weeks
//! starting on Monday, months or years, in UTC, at the finest of those that needs no more bins
//! than were asked for.

use rayon::prelude::*;
use tessera_filter::RecordValue;
use tessera_lifecycle::WalScalar;
use tessera_spatial::tiler::ScalarType;
use tessera_store::read::ScalarSlice;

use super::set::{Cx, Set};
use super::table::{Edge, Groups, Key};
use super::values::pieces;
use super::{AggregateRefused, AggregateTimings};
use crate::cells::CellSet;
use crate::error::{EngineError, Result};
use crate::filter::Scalar;
use crate::Generation;

/// Entity ids one piece of a pass over a field's per-entity values spans.
const ENTITY_PIECE: u64 = 1 << 20;

/// A number or timestamp field as one request bins it.
pub(super) struct Bins {
    column: String,
    kind: Kind,
    /// The field's per-entity values are held.
    held: bool,
    /// The field is drawn in every view's rows.
    drawn: bool,
    bins: u32,
    range: Option<(Scalar, Scalar)>,
}

/// How a field's values are compared with an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Integer,
    Float,
    /// Microseconds since the Unix epoch, with edges in the same unit.
    Timestamp,
}

impl Kind {
    fn of(ty: ScalarType) -> Option<Kind> {
        use ScalarType as T;
        Some(match ty {
            T::U8 | T::U16 | T::U32 | T::U64 | T::I8 | T::I16 | T::I32 | T::I64 => Kind::Integer,
            T::F32 | T::F64 => Kind::Float,
            T::TimestampUs => Kind::Timestamp,
            _ => return None,
        })
    }
}

/// A table's bin edges, ascending, one more than its bins; none where it has no bin.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Edges {
    Numbers(Vec<f64>),
    Times(Vec<i64>),
}

impl Edges {
    fn bins(&self) -> usize {
        let edges = match self {
            Edges::Numbers(edges) => edges.len(),
            Edges::Times(edges) => edges.len(),
        };
        edges.saturating_sub(1)
    }

    fn edge(&self, at: usize) -> Edge {
        match self {
            Edges::Numbers(edges) => Edge::Number(edges[at]),
            Edges::Times(edges) => Edge::Time(edges[at]),
        }
    }

    /// The edges as the cursor carries them.
    fn encode(&self) -> Vec<u64> {
        match self {
            Edges::Numbers(edges) => edges.iter().map(|e| e.to_bits()).collect(),
            Edges::Times(edges) => edges.iter().map(|&e| e as u64).collect(),
        }
    }

    fn decode(kind: Kind, carried: &[u64]) -> Edges {
        match kind {
            Kind::Timestamp => Edges::Times(carried.iter().map(|&e| e as i64).collect()),
            _ => Edges::Numbers(carried.iter().map(|&e| f64::from_bits(e)).collect()),
        }
    }
}

impl Bins {
    /// The field `column` names, where it can be binned.
    pub(super) fn of(
        generation: &Generation,
        column: &str,
        bins: u32,
        range: Option<(Scalar, Scalar)>,
    ) -> Result<Bins> {
        let manifest = &generation.bundle.manifest;
        let not_binnable =
            || EngineError::AggregateRefused(AggregateRefused::NotBinnable(column.to_string()));
        // A category is stored as integer codes, which are not its values.
        let (ty, vocabulary) = match manifest.declared_scalars.iter().find(|s| s.name == column) {
            Some(scalar) => (scalar.arrow_type, scalar.vocabulary.is_some()),
            None => {
                let (name, view) = column
                    .split_once(crate::filter::PIN)
                    .ok_or_else(not_binnable)?;
                let family = manifest
                    .scoped_scalars()
                    .into_iter()
                    .find(|f| f.name == name && f.views.iter().any(|v| v == view))
                    .ok_or_else(not_binnable)?;
                (family.arrow_type, family.vocabulary.is_some())
            }
        };
        let kind = Kind::of(ty)
            .filter(|_| !vocabulary)
            .ok_or_else(not_binnable)?;
        let held = generation.filter_columns.value_layers(column).is_some();
        let drawn = manifest
            .render_scalars()
            .any(|scalar| scalar.name == column);
        if !held && !drawn {
            return Err(not_binnable());
        }
        let range = match (kind, range) {
            (Kind::Timestamp, Some((lower, upper))) => {
                let whole = |bound: Scalar| match bound {
                    Scalar::Int(i) => Some(Scalar::Int(i)),
                    Scalar::Float(f) if f.fract() == 0.0 => Some(Scalar::Int(f as i128)),
                    Scalar::Float(_) => None,
                };
                let fractional = || {
                    EngineError::AggregateRefused(AggregateRefused::FractionalTime(
                        column.to_string(),
                    ))
                };
                Some((
                    whole(lower).ok_or_else(fractional)?,
                    whole(upper).ok_or_else(fractional)?,
                ))
            }
            (_, range) => range,
        };
        Ok(Bins {
            column: column.to_string(),
            kind,
            held,
            drawn,
            bins,
            range,
        })
    }

    /// Whether the field holds timestamps, whose edges are timestamps too.
    pub(super) fn timestamps(&self) -> bool {
        self.kind == Kind::Timestamp
    }

    /// Whether counting this field needs a set's rows rather than its entities.
    pub(super) fn wants_rows(&self) -> bool {
        !self.held
    }

    /// The table's groups under `cx`: its bins, with the edges `chosen` carries where the table
    /// has begun, then `rest` and `none`, each with its counts in the set and the reference.
    pub(super) fn groups(
        &self,
        cx: &Cx<'_>,
        chosen: Option<&[u64]>,
        timings: &mut AggregateTimings,
    ) -> Result<Groups> {
        let counting = std::time::Instant::now();
        let edges = match chosen {
            Some(chosen) => Edges::decode(self.kind, chosen),
            None => self.edges(cx)?,
        };
        let set = self.counts(cx, &cx.sets.set, &edges)?;
        let reference = match &cx.sets.reference {
            Some(reference) => Some(self.counts(cx, reference, &edges)?),
            None => None,
        };
        timings.count_ns += counting.elapsed().as_nanos() as u64;
        timings.entities_crossed +=
            cx.sets.set.crossed() + cx.sets.reference.as_ref().map_or(0, Set::crossed);
        let bins = edges.bins();
        let of = |counts: Option<&Counts>| -> Vec<u64> {
            match counts {
                None => vec![0; bins + 2],
                Some(c) => c.bins.iter().copied().chain([c.rest, c.none]).collect(),
            }
        };
        Ok(Groups {
            chosen: edges.encode(),
            sizes: of(Some(&set)).into_iter().zip(of(reference.as_ref())).collect(),
            always: vec![true; bins],
            keys: (0..bins)
                .map(|b| Key::Bin(edges.edge(b), edges.edge(b + 1)))
                .collect(),
            titles: None,
            distinct: set.bins.iter().filter(|&&n| n > 0).count() as u64,
        })
    }

    /// The request's range cut into equal bins, or readable edges around the values of every item
    /// this viewer may see in the view.
    fn edges(&self, cx: &Cx<'_>) -> Result<Edges> {
        let n = self.bins;
        if let Some((lower, upper)) = self.range {
            return Ok(match self.kind {
                Kind::Timestamp => Edges::Times(equal_times(int_of(lower), int_of(upper), n)),
                _ => Edges::Numbers(equal_numbers(
                    tessera_filter::as_f64(lower),
                    tessera_filter::as_f64(upper),
                    n,
                )),
            });
        }
        // The visible set, which no filter or region narrows.
        let built;
        let visible = if cx.sets.set.is_whole() {
            &cx.sets.set
        } else if let Some(reference) = cx.sets.reference.as_ref().filter(|r| r.is_whole()) {
            reference
        } else {
            built = Set::whole(cx.open);
            &built
        };
        Ok(match self.kind {
            Kind::Integer => match self.pass(cx, visible, Extremes::<i128>::default)?.0.span {
                None => Edges::Numbers(Vec::new()),
                Some((min, max)) => Edges::Numbers(readable_numbers(
                    min as f64,
                    max as f64,
                    n,
                    true,
                    &|lower, upper| lower.ceil() as i128 <= min && upper.floor() as i128 >= max,
                )),
            },
            Kind::Float => match self.pass(cx, visible, Extremes::<f64>::default)?.0.span {
                None => Edges::Numbers(Vec::new()),
                Some((min, max)) => Edges::Numbers(readable_numbers(
                    min,
                    max,
                    n,
                    false,
                    &|lower, upper| lower <= min && upper >= max,
                )),
            },
            Kind::Timestamp => match self.pass(cx, visible, Extremes::<i128>::default)?.0.span {
                None => Edges::Times(Vec::new()),
                Some((min, max)) => Edges::Times(readable_times(min as i64, max as i64, n)),
            },
        })
    }

    /// How many items of `set` fall in each bin of `edges`, in none, and have no value.
    fn counts(&self, cx: &Cx<'_>, set: &Set, edges: &Edges) -> Result<Counts> {
        let bins = edges.bins();
        let counted = match (self.kind, edges) {
            (Kind::Float, Edges::Numbers(edges)) if bins > 0 => {
                let (lowers, upper) = (&edges[..bins], edges[bins]);
                let (hist, none) = self.pass(cx, set, || Histogram::new(lowers, upper))?;
                (hist.bins, hist.rest, none)
            }
            (Kind::Integer, Edges::Numbers(edges)) if bins > 0 => {
                let lowers: Vec<i128> = edges[..bins].iter().map(|e| e.ceil() as i128).collect();
                let upper = edges[bins].floor() as i128;
                let (hist, none) = self.pass(cx, set, || Histogram::new(&lowers, upper))?;
                (hist.bins, hist.rest, none)
            }
            (Kind::Timestamp, Edges::Times(edges)) if bins > 0 => {
                let lowers: Vec<i128> = edges[..bins].iter().map(|&e| i128::from(e)).collect();
                let upper = i128::from(edges[bins]);
                let (hist, none) = self.pass(cx, set, || Histogram::new(&lowers, upper))?;
                (hist.bins, hist.rest, none)
            }
            // With no bin, every value is in `rest`.
            (Kind::Float, _) => {
                let (hist, none) = self.pass(cx, set, || Histogram::<f64>::new(&[], 0.0))?;
                (hist.bins, hist.rest, none)
            }
            _ => {
                let (hist, none) = self.pass(cx, set, || Histogram::<i128>::new(&[], 0))?;
                (hist.bins, hist.rest, none)
            }
        };
        let (bins, rest, none) = counted;
        Ok(Counts { bins, rest, none })
    }

    /// One pass over the values of `set`'s items into a tally, and how many items have no value:
    /// through the field's per-entity values where the field is not drawn or the set is held as
    /// entities, and otherwise over the set's rows, which the whole visible set always is.
    fn pass<K: Num, T: Tally<K>>(
        &self,
        cx: &Cx<'_>,
        set: &Set,
        empty: impl Fn() -> T + Sync + Send,
    ) -> Result<(T, u64)> {
        cx.check_cancelled()?;
        if self.held && (!self.drawn || (set.has_entities() && !set.is_whole())) {
            let entities = set.entities(cx)?;
            return Ok(cx
                .engine
                .pool
                .install(|| self.pass_entities(cx, entities, empty)));
        }
        Ok(cx
            .engine
            .pool
            .install(|| self.pass_rows(set.cells(cx), cx.segments(), empty)))
    }

    /// The field's per-entity values of `entities`, layer by layer in parallel pieces of entity
    /// space, then the buffered rows.
    fn pass_entities<K: Num, T: Tally<K>>(
        &self,
        cx: &Cx<'_>,
        entities: &croaring::Bitmap,
        empty: impl Fn() -> T + Sync + Send,
    ) -> (T, u64) {
        let layers = cx
            .generation
            .filter_columns
            .value_layers(&self.column)
            .expect("a held field has value layers");
        let layers: Vec<&tessera_filter::ValueColumn> =
            layers.base().into_iter().chain(layers.extents()).collect();
        let end = entities.maximum().map_or(0, |last| u64::from(last) + 1);
        let pieces: Vec<(usize, std::ops::Range<u64>)> = (0..layers.len())
            .flat_map(|l| {
                (0..end.div_ceil(ENTITY_PIECE))
                    .map(move |p| (l, p * ENTITY_PIECE..((p + 1) * ENTITY_PIECE).min(end)))
            })
            .collect();
        let (mut tally, mut valued) = pieces
            .par_iter()
            .fold(
                || (empty(), 0u64),
                |(mut tally, mut valued), (l, range)| {
                    let mut piece = croaring::Bitmap::from_range(range.start as u32..range.end as u32);
                    piece.and_inplace(entities);
                    let _ = layers[*l].for_each_record_value_in(&piece, |_, value| {
                        valued += 1;
                        if let Some(x) = K::of_record(&value) {
                            tally.add(x);
                        }
                        Ok::<(), ()>(())
                    });
                    (tally, valued)
                },
            )
            .reduce(|| (empty(), 0), |(a, m), (b, n)| (a.merge(b), m + n));
        crate::categories::buffered_values(
            &cx.generation.bundle.manifest,
            &cx.generation.buffer,
            &self.column,
            &mut |entity, value| {
                if !entities.contains(entity) || matches!(value, WalScalar::Null) {
                    return;
                }
                valued += 1;
                if let Some(x) = K::of_wal(value) {
                    tally.add(x);
                }
            },
        );
        (tally, entities.cardinality().saturating_sub(valued))
    }

    /// The drawn column's values over a set's rows, in one parallel pass. A row the column's
    /// presence leaves out, and a row of a segment without the column, has no value.
    fn pass_rows<K: Num, T: Tally<K>>(
        &self,
        set: CellSet<'_>,
        segments: &[(&tessera_store::read::SegmentData, u32)],
        empty: impl Fn() -> T + Sync + Send,
    ) -> (T, u64) {
        let column = self.column.as_str();
        pieces(segments)
            .par_iter()
            .fold(
                || (empty(), 0u64),
                |(mut tally, mut none), (s, rows)| {
                    let (segment, row_base) = segments[*s];
                    let base = row_base as usize;
                    let present = segment.columns.presence(column).bitmap();
                    macro_rules! walk {
                        ($values:expr, $num:expr) => {{
                            let values = $values;
                            set.for_each_run(rows.clone(), &mut |run| {
                                for row in run {
                                    let local = row as usize - base;
                                    if present.is_some_and(|p| !p.contains(local as u32)) {
                                        none += 1;
                                    } else {
                                        tally.add($num(values[local]));
                                    }
                                }
                            })
                        }};
                    }
                    match segment.columns.scalar(column) {
                        Some(ScalarSlice::U8(v)) => walk!(v, |x: u8| K::int(i128::from(x))),
                        Some(ScalarSlice::U16(v)) => walk!(v, |x: u16| K::int(i128::from(x))),
                        Some(ScalarSlice::U32(v)) => walk!(v, |x: u32| K::int(i128::from(x))),
                        Some(ScalarSlice::U64(v)) => walk!(v, |x: u64| K::int(i128::from(x))),
                        Some(ScalarSlice::I8(v)) => walk!(v, |x: i8| K::int(i128::from(x))),
                        Some(ScalarSlice::I16(v)) => walk!(v, |x: i16| K::int(i128::from(x))),
                        Some(ScalarSlice::I32(v)) => walk!(v, |x: i32| K::int(i128::from(x))),
                        Some(ScalarSlice::I64(v)) => walk!(v, |x: i64| K::int(i128::from(x))),
                        Some(ScalarSlice::TimestampUs(v)) => {
                            walk!(v, |x: i64| K::int(i128::from(x)))
                        }
                        Some(ScalarSlice::F32(v)) => walk!(v, |x: f32| K::float(f64::from(x))),
                        Some(ScalarSlice::F64(v)) => walk!(v, |x: f64| K::float(x)),
                        _ => none += set.count(rows.clone()),
                    }
                    (tally, none)
                },
            )
            .reduce(
                || (empty(), 0),
                |(a, m), (b, n)| (a.merge(b), m + n),
            )
    }
}

/// How many items of one set fall in each bin, in none, and have no value.
struct Counts {
    bins: Vec<u64>,
    rest: u64,
    none: u64,
}

/// A value as a pass compares it: `i128` for an integer or a timestamp, which holds every stored
/// integer exactly, and `f64` for a float.
trait Num: Copy + PartialOrd + Send + Sync {
    fn int(x: i128) -> Self;
    fn float(x: f64) -> Self;
    fn finite(self) -> bool;

    fn of_record(value: &RecordValue) -> Option<Self> {
        Some(match *value {
            RecordValue::U8(x) => Self::int(i128::from(x)),
            RecordValue::U16(x) => Self::int(i128::from(x)),
            RecordValue::U32(x) => Self::int(i128::from(x)),
            RecordValue::U64(x) => Self::int(i128::from(x)),
            RecordValue::I8(x) => Self::int(i128::from(x)),
            RecordValue::I16(x) => Self::int(i128::from(x)),
            RecordValue::I32(x) => Self::int(i128::from(x)),
            RecordValue::I64(x) | RecordValue::TimestampUs(x) => Self::int(i128::from(x)),
            RecordValue::F32(x) => Self::float(f64::from(x)),
            RecordValue::F64(x) => Self::float(x),
            _ => return None,
        })
    }

    fn of_wal(value: &WalScalar) -> Option<Self> {
        Some(match *value {
            WalScalar::U8(x) => Self::int(i128::from(x)),
            WalScalar::U16(x) => Self::int(i128::from(x)),
            WalScalar::U32(x) => Self::int(i128::from(x)),
            WalScalar::U64(x) => Self::int(i128::from(x)),
            WalScalar::I8(x) => Self::int(i128::from(x)),
            WalScalar::I16(x) => Self::int(i128::from(x)),
            WalScalar::I32(x) => Self::int(i128::from(x)),
            WalScalar::I64(x) | WalScalar::TimestampUs(x) => Self::int(i128::from(x)),
            WalScalar::F32(x) => Self::float(f64::from(x)),
            WalScalar::F64(x) => Self::float(x),
            _ => return None,
        })
    }
}

// A field's kind decides which of the two a pass uses, so an integer never reaches `float` and a
// float never reaches `int`.
impl Num for i128 {
    fn int(x: i128) -> Self {
        x
    }
    fn float(x: f64) -> Self {
        x as i128
    }
    fn finite(self) -> bool {
        true
    }
}

impl Num for f64 {
    fn int(x: i128) -> Self {
        x as f64
    }
    fn float(x: f64) -> Self {
        x
    }
    fn finite(self) -> bool {
        self.is_finite()
    }
}

/// What a pass accumulates, piece by piece, then merged.
trait Tally<K>: Send {
    fn add(&mut self, x: K);
    fn merge(self, other: Self) -> Self;
}

/// The smallest and largest finite value seen.
struct Extremes<K> {
    span: Option<(K, K)>,
}

impl<K> Default for Extremes<K> {
    fn default() -> Self {
        Extremes { span: None }
    }
}

impl<K: Num> Tally<K> for Extremes<K> {
    #[inline]
    fn add(&mut self, x: K) {
        if !x.finite() {
            return;
        }
        self.span = Some(match self.span {
            None => (x, x),
            Some((lo, hi)) => (
                if x < lo { x } else { lo },
                if x > hi { x } else { hi },
            ),
        });
    }

    fn merge(mut self, other: Self) -> Self {
        if let Some((lo, hi)) = other.span {
            self.add(lo);
            self.add(hi);
        }
        self
    }
}

/// A count per bin: bin `b` holds `lowers[b] <= x` up to the next lower bound, and the last bin
/// holds up to `upper` inclusive.
struct Histogram<'e, K> {
    lowers: &'e [K],
    upper: K,
    bins: Vec<u64>,
    rest: u64,
}

impl<'e, K: Num> Histogram<'e, K> {
    fn new(lowers: &'e [K], upper: K) -> Self {
        Histogram {
            lowers,
            upper,
            bins: vec![0; lowers.len()],
            rest: 0,
        }
    }
}

impl<K: Num> Tally<K> for Histogram<'_, K> {
    #[inline]
    fn add(&mut self, x: K) {
        // A NaN is below no bound and so in no bin.
        let at = self.lowers.partition_point(|&lower| lower <= x);
        match at.checked_sub(1) {
            Some(bin) if x <= self.upper => self.bins[bin] += 1,
            _ => self.rest += 1,
        }
    }

    fn merge(mut self, other: Self) -> Self {
        for (a, b) in self.bins.iter_mut().zip(&other.bins) {
            *a += b;
        }
        self.rest += other.rest;
        self
    }
}

/// Whether `lower` is below `upper`, comparing two integers exactly.
pub(super) fn below(lower: Scalar, upper: Scalar) -> bool {
    match (lower, upper) {
        (Scalar::Int(a), Scalar::Int(b)) => a < b,
        (a, b) => tessera_filter::as_f64(a) < tessera_filter::as_f64(b),
    }
}

fn int_of(bound: Scalar) -> i128 {
    match bound {
        Scalar::Int(i) => i,
        Scalar::Float(f) => f as i128,
    }
}

/// `[lower, upper]` cut into `n` bins of equal width.
fn equal_numbers(lower: f64, upper: f64, n: u32) -> Vec<f64> {
    let width = upper - lower;
    (0..=n)
        .map(|i| match i {
            0 => lower,
            i if i == n => upper,
            i => lower + width * (f64::from(i) / f64::from(n)),
        })
        .collect()
}

/// `[lower, upper]` in microseconds cut into `n` bins whose widths differ by at most one.
fn equal_times(lower: i128, upper: i128, n: u32) -> Vec<i64> {
    let clamp = |x: i128| x.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    let n = i128::from(n);
    (0..=n)
        .map(|i| clamp(lower + (upper - lower) * i / n))
        .collect()
}

/// The mantissas of a readable step, each times a power of ten.
const MANTISSAS: [f64; 4] = [1.0, 2.0, 2.5, 5.0];

/// At most `n` bins of one readable width covering `[min, max]`, starting at a multiple of the
/// width. On an integer field the width is a whole number and the last bin's upper edge is past
/// `max`, so each bin holds the same count of integers. `covers` says whether a first and last
/// edge hold every value exactly, which rounding at a large magnitude can prevent; a width that
/// fails it is passed over for a wider one.
fn readable_numbers(
    min: f64,
    max: f64,
    n: u32,
    integer: bool,
    covers: &dyn Fn(f64, f64) -> bool,
) -> Vec<f64> {
    let n = f64::from(n);
    let scale = |x: f64, e: i32| {
        if e >= 0 {
            x * 10f64.powi(e)
        } else {
            x / 10f64.powi(-e)
        }
    };
    let start = if max > min {
        ((max - min) / n).log10().floor() as i32 - 1
    } else if min != 0.0 {
        min.abs().log10().floor() as i32
    } else {
        0
    };
    for e in start..start.saturating_add(40) {
        for m in MANTISSAS {
            let step = scale(m, e);
            if integer && (step < 1.0 || step.fract() != 0.0) {
                continue;
            }
            let edge = |k: f64| scale(k * m, e);
            let mut first = (min / step).floor();
            // Past 2^52 multiples a step is below the values' precision.
            if first.is_nan() || first.abs() >= 4_503_599_627_370_496.0 {
                continue;
            }
            if edge(first) > min {
                first -= 1.0;
            }
            let reach = |bins: f64| {
                let last = edge(first + bins);
                if integer {
                    last > max
                } else {
                    last >= max
                }
            };
            let mut bins = if integer {
                ((max - edge(first)) / step).floor() + 1.0
            } else {
                ((max - edge(first)) / step).ceil()
            }
            .max(1.0);
            while !reach(bins) && bins <= n {
                bins += 1.0;
            }
            if bins.is_nan() || bins > n {
                continue;
            }
            let edges: Vec<f64> = (0..=bins as u32)
                .map(|i| edge(first + f64::from(i)))
                .collect();
            let rising = edges.windows(2).all(|w| w[0] < w[1]);
            let last = edges[edges.len() - 1];
            if rising && edges.iter().all(|e| e.is_finite()) && covers(edges[0], last) {
                return edges;
            }
        }
    }
    // One bin over values either side of 0, which no multiple of a width starts below, or a span
    // no readable width covers within the float range.
    let upper = if integer { max + 1.0 } else { max };
    equal_numbers(min, upper, n as u32)
}

const SECOND: i64 = 1_000_000;
const MINUTE: i64 = 60 * SECOND;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// A readable width of a timestamp bin.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// A fixed width in microseconds, its bins starting at a multiple of it after `offset`.
    Fixed { width: i64, offset: i64 },
    Months(i64),
    Years(i64),
}

/// The widths a timestamp bin may take, finest first.
fn time_steps() -> impl Iterator<Item = Step> {
    let fixed = |width| Step::Fixed { width, offset: 0 };
    let sub_second = [
        1, 2, 5, 10, 20, 50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000,
        200_000, 500_000,
    ];
    let clock = [
        SECOND,
        2 * SECOND,
        5 * SECOND,
        10 * SECOND,
        15 * SECOND,
        30 * SECOND,
        MINUTE,
        2 * MINUTE,
        5 * MINUTE,
        10 * MINUTE,
        15 * MINUTE,
        30 * MINUTE,
        HOUR,
        2 * HOUR,
        3 * HOUR,
        6 * HOUR,
        12 * HOUR,
        DAY,
        2 * DAY,
    ];
    // 1970-01-05 was a Monday.
    let week = Step::Fixed {
        width: 7 * DAY,
        offset: 4 * DAY,
    };
    let years = [
        1, 2, 5, 10, 20, 50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000,
        200_000, 500_000,
    ];
    sub_second
        .into_iter()
        .chain(clock)
        .map(fixed)
        .chain([week])
        .chain([1, 2, 3, 6].map(Step::Months))
        .chain(years.map(Step::Years))
}

/// At most `n` bins of the finest readable width covering `[min, max]` in microseconds, the last
/// bin's upper edge past `max`. A single instant is binned by its day.
fn readable_times(min: i64, max: i64, n: u32) -> Vec<i64> {
    let n = i128::from(n);
    let finest = if min == max {
        time_steps()
            .position(|s| matches!(s, Step::Fixed { width: DAY, .. }))
            .unwrap_or(0)
    } else {
        0
    };
    time_steps()
        .skip(finest)
        .find_map(|step| step_edges(step, min, max, n))
        .unwrap_or_else(|| equal_times(i128::from(min), i128::from(max) + 1, n as u32))
}

/// The edges of `step`'s bins from the one holding `min` to the one holding `max`, where they
/// number at most `n` and every edge is an `i64`.
fn step_edges(step: Step, min: i64, max: i64, n: i128) -> Option<Vec<i64>> {
    let (first, last) = (step_index(step, min), step_index(step, max));
    if last - first + 1 > n {
        return None;
    }
    (first..=last + 1)
        .map(|k| step_start(step, k).and_then(|edge| i64::try_from(edge).ok()))
        .collect()
}

/// Which of `step`'s bins holds `t`, counted from the bin starting at the epoch, January of year 0
/// or year 0.
fn step_index(step: Step, t: i64) -> i128 {
    match step {
        Step::Fixed { width, offset } => (i128::from(t) - i128::from(offset)).div_euclid(i128::from(width)),
        Step::Months(k) => {
            let (year, month) = civil_from_days(t.div_euclid(DAY));
            (i128::from(year) * 12 + i128::from(month - 1)).div_euclid(i128::from(k))
        }
        Step::Years(k) => i128::from(civil_from_days(t.div_euclid(DAY)).0).div_euclid(i128::from(k)),
    }
}

/// Where `step`'s bin `k` starts, in microseconds.
fn step_start(step: Step, k: i128) -> Option<i128> {
    let day_of = |year: i128, month: i64| -> Option<i128> {
        let year = i64::try_from(year).ok()?;
        Some(i128::from(days_from_civil(year, month, 1)) * i128::from(DAY))
    };
    match step {
        Step::Fixed { width, offset } => Some(k * i128::from(width) + i128::from(offset)),
        Step::Months(m) => {
            let index = k * i128::from(m);
            day_of(index.div_euclid(12), (index.rem_euclid(12) + 1) as i64)
        }
        Step::Years(m) => day_of(k * i128::from(m), 1),
    }
}

/// Days since 1970-01-01 of a date in the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let of_era = year - era * 400;
    let from_march = (month + 9) % 12;
    let of_year = (153 * from_march + 2) / 5 + day - 1;
    let of_cycle = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    era * 146_097 + of_cycle - 719_468
}

/// The year and month, from 1, of a day counted from 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let of_cycle = days - era * 146_097;
    let of_era = (of_cycle - of_cycle / 1_460 + of_cycle / 36_524 - of_cycle / 146_096) / 365;
    let of_year = of_cycle - (365 * of_era + of_era / 4 - of_era / 100);
    let from_march = (5 * of_year + 2) / 153;
    let month = if from_march < 10 {
        from_march + 3
    } else {
        from_march - 9
    };
    let year = of_era + era * 400 + i64::from(month <= 2);
    (year, month)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i64, month: i64, day: i64) -> i64 {
        days_from_civil(year, month, day) * DAY
    }

    fn integers(min: i128, max: i128, n: u32) -> Vec<f64> {
        readable_numbers(min as f64, max as f64, n, true, &|lower, upper| {
            lower.ceil() as i128 <= min && upper.floor() as i128 >= max
        })
    }

    fn floats(min: f64, max: f64, n: u32) -> Vec<f64> {
        readable_numbers(min, max, n, false, &|lower, upper| lower <= min && upper >= max)
    }

    #[test]
    fn the_calendar_round_trips_across_eras_and_leap_days() {
        for days in (-800_000..800_000).step_by(37) {
            let (year, month) = civil_from_days(days);
            let first = days_from_civil(year, month, 1);
            assert!(first <= days && days - first < 31, "{days}: {year}-{month}");
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 1), 29);
        assert_eq!(days_from_civil(1900, 3, 1) - days_from_civil(1900, 2, 1), 28);
        assert_eq!(civil_from_days(-1), (1969, 12));
    }

    #[test]
    fn readable_number_edges_cover_the_values_in_at_most_the_bins_asked_for() {
        assert_eq!(floats(0.13, 0.87, 10), (1..=9).map(|k| f64::from(k) / 10.0).collect::<Vec<_>>());
        assert_eq!(floats(-3.0, 47.0, 5), vec![-20.0, 0.0, 20.0, 40.0, 60.0]);
        assert_eq!(integers(1, 5, 10), vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(integers(0, 99, 10), (0..=10).map(|k| f64::from(k) * 10.0).collect::<Vec<_>>());
        assert_eq!(integers(7, 7, 4), vec![7.0, 8.0]);
        assert_eq!(floats(3.7, 3.7, 4), vec![3.0, 4.0]);
        for (min, max, n) in [(0.0, 1e-9, 7), (-1e300, 1e300, 3), (1e15 + 0.1, 1e15 + 0.3, 20), (5.0, 5.000001, 1)] {
            let edges = floats(min, max, n);
            assert!(edges.len() >= 2 && edges.len() <= n as usize + 1, "{min} {max} {n}: {edges:?}");
            assert!(edges[0] <= min && edges[edges.len() - 1] >= max, "{edges:?}");
        }
        for (min, max) in [(0i128, u64::MAX as i128), (1 << 63, (1 << 63) + 100), (-5, -5)] {
            let edges = integers(min, max, 20);
            assert!(edges.len() >= 2 && edges.len() <= 21, "{edges:?}");
            assert!(edges[0].ceil() as i128 <= min && edges[edges.len() - 1].floor() as i128 >= max);
        }
    }

    #[test]
    fn readable_time_edges_fall_on_the_calendar() {
        // Three years and a bit, in at most 5 bins: years.
        let edges = readable_times(date(2019, 5, 3), date(2022, 2, 1), 5);
        assert_eq!(edges, (2019..=2023).map(|y| date(y, 1, 1)).collect::<Vec<_>>());
        // The same in at most 20 bins: two months each, from January, March, May and so on.
        let edges = readable_times(date(2019, 5, 3), date(2022, 2, 1), 20);
        assert_eq!((edges[0], edges[1], edges.len()), (date(2019, 5, 1), date(2019, 7, 1), 18));
        let edges = readable_times(date(2019, 5, 3), date(2022, 2, 1), 5);
        assert_eq!(edges, (2019..=2023).map(|y| date(y, 1, 1)).collect::<Vec<_>>());
        // Ten months: months.
        let edges = readable_times(date(2021, 3, 15), date(2021, 12, 31) + HOUR, 12);
        assert_eq!(edges, (3..=12).map(|m| date(2021, m, 1)).chain([date(2022, 1, 1)]).collect::<Vec<_>>());
        // Five weeks in at most 6 bins: weeks from Monday.
        let edges = readable_times(date(2024, 1, 3), date(2024, 2, 4), 6);
        assert_eq!(edges[0], date(2024, 1, 1));
        assert!(edges.windows(2).all(|w| w[1] - w[0] == 7 * DAY));
        // A day in hours.
        let edges = readable_times(date(2024, 6, 1) + 30 * MINUTE, date(2024, 6, 1) + 23 * HOUR, 24);
        assert_eq!(edges.first(), Some(&date(2024, 6, 1)));
        assert_eq!(edges.len(), 25);
        // One instant: its day.
        assert_eq!(readable_times(date(2024, 6, 1) + HOUR, date(2024, 6, 1) + HOUR, 10), vec![date(2024, 6, 1), date(2024, 6, 2)]);
        // Before the epoch, and the widest span an i64 holds.
        let edges = readable_times(date(1900, 7, 1), date(1960, 1, 1), 10);
        assert_eq!(edges, (0..=7).map(|k| date(1900 + 10 * k, 1, 1)).collect::<Vec<_>>());
        let edges = readable_times(i64::MIN, i64::MAX, 3);
        assert!(edges.len() >= 2 && edges.len() <= 4 && edges[0] <= i64::MIN + 1);
    }

    #[test]
    fn a_value_falls_in_the_bin_whose_edges_hold_it_and_the_last_bin_is_closed() {
        let edges = [0.0, 0.5, 1.0, 1.5];
        let mut floats = Histogram::new(&edges[..3], edges[3]);
        for x in [-0.1, 0.0, 0.49, 0.5, 1.4999, 1.5, 1.50001, f64::NAN, f64::INFINITY] {
            floats.add(x);
        }
        assert_eq!((floats.bins.clone(), floats.rest), (vec![2, 1, 2], 4));
        // An integer against the same edges: [0, 0.5) holds 0, [0.5, 1) holds nothing.
        let lowers: Vec<i128> = edges[..3].iter().map(|e| e.ceil() as i128).collect();
        let mut ints = Histogram::new(&lowers, edges[3].floor() as i128);
        for x in [-1, 0, 1, 2] {
            ints.add(x);
        }
        assert_eq!((ints.bins.clone(), ints.rest), (vec![1, 0, 1], 2));
    }
}
