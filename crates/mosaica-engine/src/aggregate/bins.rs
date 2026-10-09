//! A grouping by bins of a number or timestamp field: a histogram.
//!
//! The bins' edges are the request's range cut into equal widths, or, with no range, readable
//! edges around the smallest and largest value among the items the viewer may see in the view.
//! That default is taken over the whole visible set and never over the filtered set, the
//! reference or a region, so the edges hold still while a filter or the viewport changes. It is
//! read from the field's figures ([`crate::figures::FieldFigures`]), which are exact over the
//! visible set whether or not the counts are sampled, so an item the viewer may not see moves no
//! edge and no visible value lies outside the bins. The edges are drawn once: a histogram is
//! served in one page, whatever the page's limits.
//!
//! A bin holds the values from its lower edge up to but not including its upper edge, and the
//! last bin also holds its upper edge. `rest` counts the items whose value lies in no bin: outside
//! a range the request gave, or NaN, or infinite. `none` counts the items with no value.
//!
//! An integer or timestamp field's edges are whole numbers, worked out and compared in `i128`, so
//! every value is placed exactly and a `range` filter between two edges matches exactly the bin's
//! items. A float field's edges are `f64`, and so are an integer field's where a bound of its
//! range is fractional.
//!
//! Readable edges are multiples of 1, 2, 2.5 or 5 times a power of ten, whole numbers on an
//! integer field. On a timestamp field they fall on whole seconds, minutes, hours, days, weeks
//! starting on Monday, months or years, in UTC, at the finest of those that needs no more bins
//! than were asked for.
//!
//! # A sampled histogram
//!
//! With a sample size `s` and a set of `N > s` items, the counts are taken over the set's items
//! whose `tessera_id` is below the cut `⌊s · 2⁶⁴ / N⌋`, about `s` of them, and each count is scaled
//! by `N` over the items counted, rounded to the nearest whole number with a half rounded up. The
//! cut is one for the whole set, so no tile, segment or group has a floor or a cap. `N` is the
//! set's composed size and the sample is drawn from the set's own rows, which are inside the
//! visible set, so an item the viewer may not see neither enters the sample nor moves `N`. Where
//! `N <= s` every item is counted and nothing is scaled. The reference is sampled the same way,
//! at its own cut.
//!
//! The items below a cut are band `band_below(cut)` of each segment's identity bands
//! ([`mosaica_store::bands`]) intersected with the set. Each piece of rows is read either from the
//! band, a set lookup per entry, or by scanning the set's rows' identities, whichever its counts,
//! known before reading, say costs less. Both read the same items, so the choice changes only the
//! time a piece takes. A banded entry's value is read from the band's copy of the column. A
//! scanned row below the cut is read from the drawn column, or where the field is only indexed,
//! from the band's copy, since every row below the cut is in the band.
//!
//! A set is counted exactly, every item read and nothing scaled, where its cut is above 2^58 and
//! so wider than the widest band: where `s` is more than about one item in 64 of the set. Whether
//! a set is sampled depends on `N` and `s` alone, so it says nothing of rows the viewer cannot see.
//! The head then says the set was not sampled, and counts `N` items.

use rayon::prelude::*;
use mosaica_filter::RecordValue;
use mosaica_lifecycle::WalScalar;
use mosaica_spatial::tiler::ScalarType;
use mosaica_store::read::ScalarSlice;

use super::set::{Cx, Set};
use super::table::{Edge, Groups, Key};
use super::values::pieces;
use super::{AggregateRefused, AggregateTimings};
use crate::cells::CellSet;
use crate::error::{EngineError, Result};
use crate::figures::{keep_extreme, ExactSum, FieldFigures, FieldRead, FieldTally, Number, Sum};
use crate::filter::Scalar;
use crate::Generation;

/// Entity ids one piece of a pass over a field's per-entity values spans.
const ENTITY_PIECE: u64 = 1 << 20;

/// A number or timestamp field resolved in one request's generation, and where its values are
/// read.
pub(super) struct Numbers {
    column: String,
    kind: Kind,
    /// The field's per-entity values are held.
    held: bool,
    /// The field is drawn in every view's rows.
    drawn: bool,
}

/// A number or timestamp field as one request bins it.
pub(super) struct Bins {
    field: Numbers,
    /// How the values are compared with the edges: the field's kind, or float where an integer
    /// field's range has a fractional bound.
    kind: Kind,
    bins: u32,
    range: Option<(Scalar, Scalar)>,
    /// The sample size, where one was asked for.
    sample: Option<u64>,
}

/// A banded entry costs about this many scanned rows: a set lookup per entry read from a band
/// (about 15 ns) against a scanned row's identity (about 0.6 ns). Measured over the GBIF bundle's
/// 3.5 billion rows with the files in the page cache, where a 100,000-item sample of the whole
/// extent took 4 ms from the band and a viewer seeing 2.1% of it 44 ms by scanning against 109 ms
/// from the band. At 0.6 ns a row, every row would scan in about 2 s; measured cold under a 24 GB
/// memory cap, that scan read all 28 GB of the identity column from disk and took 18 to 20 s.
const BAND_ENTRY_ROWS: u64 = 25;

/// How a field's values are compared with an edge, and the type its edges are served as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// A signed integer, its edges `int64`.
    Signed,
    /// An unsigned integer, its edges `uint64`.
    Unsigned,
    Float,
    /// Microseconds since the Unix epoch, with edges in the same unit.
    Timestamp,
}

impl Kind {
    fn of(ty: ScalarType) -> Option<Kind> {
        use ScalarType as T;
        Some(match ty {
            T::U8 | T::U16 | T::U32 | T::U64 => Kind::Unsigned,
            T::I8 | T::I16 | T::I32 | T::I64 => Kind::Signed,
            T::F32 | T::F64 => Kind::Float,
            T::TimestampUs => Kind::Timestamp,
            _ => return None,
        })
    }

    /// Edges moved inside the span the served column holds. No stored value lies outside that
    /// span, so the move takes no value into another bin.
    fn clamp(self, edges: Vec<i128>) -> Vec<i128> {
        let (lo, hi) = match self {
            Kind::Unsigned => (0, i128::from(u64::MAX)),
            _ => (i128::from(i64::MIN), i128::from(i64::MAX)),
        };
        edges.into_iter().map(|e| e.clamp(lo, hi)).collect()
    }
}

/// A table's bin edges, ascending, one more than its bins; none where it has no bin. An integer
/// or timestamp field's edges are whole and within its served column's span.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Edges {
    Floats(Vec<f64>),
    Ints(Vec<i128>),
}

impl Edges {
    fn bins(&self) -> usize {
        let edges = match self {
            Edges::Floats(edges) => edges.len(),
            Edges::Ints(edges) => edges.len(),
        };
        edges.saturating_sub(1)
    }

    fn edge(&self, at: usize) -> Edge {
        match self {
            Edges::Floats(edges) => Edge::Float(edges[at]),
            Edges::Ints(edges) => Edge::Int(edges[at]),
        }
    }
}

impl Numbers {
    /// The number or timestamp field `column` names, where its values can be read. `summary` says
    /// which grouping asks, for the refusal.
    pub(super) fn of(generation: &Generation, column: &str, summary: bool) -> Result<Numbers> {
        let manifest = &generation.bundle.manifest;
        let refused = |why: AggregateRefused| EngineError::AggregateRefused(why);
        let unfit = || match summary {
            true => refused(AggregateRefused::NotSummarisable(column.to_string())),
            false => refused(AggregateRefused::NotBinnable(column.to_string())),
        };
        // A category is stored as integer codes, which are not its values.
        let (ty, vocabulary) = match manifest.declared_scalars.iter().find(|s| s.name == column) {
            Some(scalar) => (scalar.arrow_type, scalar.vocabulary.is_some()),
            None => {
                let (name, view) = column.split_once(crate::filter::PIN).ok_or_else(unfit)?;
                let family = manifest
                    .scoped_scalars()
                    .into_iter()
                    .find(|f| f.name == name && f.views.iter().any(|v| v == view))
                    .ok_or_else(unfit)?;
                (family.arrow_type, family.vocabulary.is_some())
            }
        };
        if ty == ScalarType::Bool && !summary {
            return Err(refused(AggregateRefused::BinsOnBool(column.to_string())));
        }
        let kind = Kind::of(ty).filter(|_| !vocabulary).ok_or_else(unfit)?;
        let held = generation.filter_columns.value_layers(column).is_some();
        let drawn = manifest
            .render_scalars()
            .any(|scalar| scalar.name == column);
        if !held && !drawn {
            return Err(unfit());
        }
        Ok(Numbers {
            column: column.to_string(),
            kind,
            held,
            drawn,
        })
    }

    pub(super) fn kind(&self) -> Kind {
        self.kind
    }

    /// The field's figures over every item this viewer may see in the view, whatever the
    /// request's filters and region say.
    pub(super) fn figures(&self, cx: &Cx<'_>) -> Result<FieldFigures> {
        cx.engine.field_figures(
            &cx.open.served,
            &cx.open.mask,
            &self.column,
            self.kind as u8,
            &Reader { field: self, cx },
        )
    }
}

impl Bins {
    /// The field `column` names, where it can be binned.
    pub(super) fn of(
        generation: &Generation,
        column: &str,
        bins: u32,
        range: Option<(Scalar, Scalar)>,
        sample: Option<u64>,
    ) -> Result<Bins> {
        let field = Numbers::of(generation, column, false)?;
        let whole = |bound: Scalar| match bound {
            Scalar::Int(i) => Some(Scalar::Int(i)),
            Scalar::Float(f) if f.fract() == 0.0 => Some(Scalar::Int(f as i128)),
            Scalar::Float(_) => None,
        };
        let (kind, range) = match (field.kind, range) {
            (Kind::Float, range) | (_, range @ None) => (field.kind, range),
            (kind, Some((lower, upper))) => match (whole(lower), whole(upper)) {
                (Some(lower), Some(upper)) => (kind, Some((lower, upper))),
                (_, _) if kind == Kind::Timestamp => {
                    return Err(EngineError::AggregateRefused(
                        AggregateRefused::FractionalTime(column.to_string()),
                    ))
                }
                // An integer field binned by a fractional bound is cut, and served, in float.
                (_, _) => (Kind::Float, Some((lower, upper))),
            },
        };
        Ok(Bins {
            field,
            kind,
            bins,
            range,
            sample,
        })
    }

    /// The cut a set of `n` items is sampled below, or `None` where every item is counted.
    fn cut(&self, n: u64) -> Option<u64> {
        let s = self.sample.filter(|&s| n > s)?;
        Some(((u128::from(s) << 64) / u128::from(n)) as u64)
    }

    pub(super) fn kind(&self) -> Kind {
        self.kind
    }

    /// Whether counting this field needs a set's rows rather than its entities.
    pub(super) fn wants_rows(&self) -> bool {
        !self.field.held
    }

    /// The table's groups under `cx`: its bins, then `rest` and `none`, each with its counts in
    /// the set and the reference. A histogram is served in one page, so its edges are drawn here
    /// and never carried to another.
    pub(super) fn groups(&self, cx: &Cx<'_>, timings: &mut AggregateTimings) -> Result<Groups> {
        let counting = std::time::Instant::now();
        let edges = self.edges(cx)?;
        let (set, counted) = self.counts(cx, &cx.sets.set, &edges, timings)?;
        let (reference, reference_counted) = match &cx.sets.reference {
            Some(reference) => {
                let (counts, counted) = self.counts(cx, reference, &edges, timings)?;
                (Some(counts), Some(counted))
            }
            None => (None, None),
        };
        let sample = self.sample.map(|_| super::TableSample {
            sampled: counted.sampled || reference_counted.is_some_and(|r| r.sampled),
            items: counted.items,
            reference_items: reference_counted.map(|r| r.items),
        });
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
            chosen: Vec::new(),
            sizes: of(Some(&set))
                .into_iter()
                .zip(of(reference.as_ref()))
                .collect(),
            always: vec![true; bins],
            keys: (0..bins)
                .map(|b| Key::Bin(edges.edge(b), edges.edge(b + 1)))
                .collect(),
            titles: None,
            slots: None,
            distinct: set.bins.iter().filter(|&&n| n > 0).count() as u64,
            sample,
        })
    }

    /// The request's range cut into equal bins, or readable edges around the smallest and largest
    /// value of every item this viewer may see in the view.
    fn edges(&self, cx: &Cx<'_>) -> Result<Edges> {
        let n = self.bins;
        if let Some((lower, upper)) = self.range {
            return Ok(match self.kind {
                Kind::Float => Edges::Floats(equal_floats(
                    mosaica_filter::as_f64(lower),
                    mosaica_filter::as_f64(upper),
                    n,
                )),
                kind => Edges::Ints(kind.clamp(equal_ints(int_of(lower), int_of(upper), n))),
            });
        }
        let figures = self.field.figures(cx)?;
        let span = figures.min.zip(figures.max);
        Ok(match (self.kind, span) {
            (Kind::Float, None) => Edges::Floats(Vec::new()),
            (_, None) => Edges::Ints(Vec::new()),
            (Kind::Float, Some((min, max))) => {
                Edges::Floats(readable_floats(min.as_f64(), max.as_f64(), n))
            }
            (Kind::Timestamp, Some((Number::Int(min), Number::Int(max)))) => {
                Edges::Ints(readable_times(min as i64, max as i64, n))
            }
            (kind, Some((Number::Int(min), Number::Int(max)))) => {
                Edges::Ints(kind.clamp(readable_ints(min, max, n)))
            }
            (_, Some(_)) => {
                return Err(EngineError::Malformed(format!(
                    "field '{}' has float figures but integer bins",
                    self.field.column
                )))
            }
        })
    }

    /// How many items of `set` fall in each bin of `edges`, in none, and have no value, scaled to
    /// the set where a sample was counted, and how the set was counted.
    fn counts(
        &self,
        cx: &Cx<'_>,
        set: &Set,
        edges: &Edges,
        timings: &mut AggregateTimings,
    ) -> Result<(Counts, Counted)> {
        let bins = edges.bins();
        let n = set.size();
        fn counts<K>(
            (hist, none, counted): (Histogram<'_, K>, u64, Counted),
            n: u64,
        ) -> (Counts, Counted) {
            let items = counted.items;
            // Each count times `n / items`, to the nearest whole number with a half rounded up.
            let scale = |c: u64| match items {
                0 => 0,
                _ if items == n => c,
                _ => ((u128::from(c) * u128::from(n) * 2 + u128::from(items))
                    / (2 * u128::from(items))) as u64,
            };
            let counts = Counts {
                bins: hist.bins.into_iter().map(scale).collect(),
                rest: scale(hist.rest),
                none: scale(none),
            };
            (counts, counted)
        }
        // With no bin, every value is in `rest`.
        Ok(match edges {
            Edges::Floats(edges) if bins > 0 => counts(
                self.tally(cx, set, || Histogram::new(&edges[..bins], edges[bins]), timings)?,
                n,
            ),
            Edges::Ints(edges) if bins > 0 => counts(
                self.tally(cx, set, || Histogram::new(&edges[..bins], edges[bins]), timings)?,
                n,
            ),
            Edges::Floats(_) => counts(
                self.tally(cx, set, || Histogram::<f64>::new(&[], 0.0), timings)?,
                n,
            ),
            Edges::Ints(_) => counts(
                self.tally(cx, set, || Histogram::<i128>::new(&[], 0), timings)?,
                n,
            ),
        })
    }

    /// A tally of `set`'s values, how many of its counted items have no value, and how the set was
    /// counted: the sample below the set's cut where it holds more items than the sample size and a
    /// band holds the cut, and otherwise every item.
    fn tally<K: Num, T: Tally<K>>(
        &self,
        cx: &Cx<'_>,
        set: &Set,
        empty: impl Fn() -> T + Sync + Send,
        timings: &mut AggregateTimings,
    ) -> Result<(T, u64, Counted)> {
        let members = set.cells(cx);
        if let Some(plan) = self
            .cut(set.size())
            .and_then(|cut| SamplePlan::of(cx, &members, cut))
        {
            let (tally, none, items) = self.field.pass_sample(cx, &members, &plan, empty)?;
            timings.band_entries += plan.entries_read();
            return Ok((
                tally,
                none,
                Counted {
                    items,
                    sampled: true,
                },
            ));
        }
        let (tally, none) = self.field.pass(cx, set, empty)?;
        Ok((
            tally,
            none,
            Counted {
                items: set.size(),
                sampled: false,
            },
        ))
    }
}

impl Numbers {
    /// The values of `members`' items whose `tessera_id` is below the plan's cut, in parallel
    /// pieces of the view's rows, each read from the band or by scanning as the plan says.
    fn pass_sample<K: Num, T: Tally<K>>(
        &self,
        cx: &Cx<'_>,
        members: &CellSet<'_>,
        plan: &SamplePlan,
        empty: impl Fn() -> T + Sync + Send,
    ) -> Result<(T, u64, u64)> {
        cx.check_cancelled()?;
        let SamplePlan { band, cut, .. } = *plan;
        let column = self.column.as_str();
        let segments = cx.segments();
        // What one piece found: its tally, its items with no value, its items, and the view rows
        // whose value is read through their entity.
        let found = cx.engine.pool.install(|| {
            plan.pieces
                .par_iter()
                .try_fold(
                    || (empty(), 0u64, 0u64, Vec::<u32>::new()),
                    |(mut tally, mut none, mut items, mut through), piece| {
                        let (segment, row_base) = segments[piece.segment];
                        let copy = segment.bands.copy(column);
                        let drawn = segment.columns.scalar(column);
                        let present = segment.columns.presence(column);
                        // A banded entry's value from the copy, where the segment has one.
                        let copied = |local: u32, e: usize| {
                            copy.map(|copy| {
                                (copy.holds(e)
                                    && (copy.held.is_some() || present.contains(local)))
                                .then(|| copy.value_at(e))
                            })
                        };
                        // A row's value from the drawn column, where the segment has it.
                        let read = |local: u32| {
                            drawn.as_ref().map(|slice| {
                                present
                                    .contains(local)
                                    .then(|| slice.value_at(local as usize))
                                    .flatten()
                            })
                        };
                        let mut take = |value: Option<Option<WalScalar>>, view_row: u32| {
                            items += 1;
                            match value {
                                Some(value) => match value.as_ref().and_then(K::of_wal) {
                                    Some(x) => tally.add(x, view_row),
                                    None => none += 1,
                                },
                                None if self.held => through.push(view_row),
                                None => none += 1,
                            }
                        };
                        let local = piece.rows.start - row_base..piece.rows.end - row_base;
                        if piece.banded {
                            crate::bands::admitted_entries(
                                segment,
                                row_base,
                                band,
                                local,
                                |view_row| members.contains(view_row),
                                |e, view_row, id| {
                                    if id < cut {
                                        let local = view_row - row_base;
                                        take(copied(local, e).or_else(|| read(local)), view_row);
                                    }
                                },
                            )?;
                        } else {
                            // A drawn field is read from its column. An indexed field is read from
                            // the band's copy, whose entries are found in row order, since every
                            // row below the cut is in the band.
                            let ids = segment.columns.tessera_id();
                            let mut entries = segment.bands.entries_from(band, local.start);
                            members.for_each_run(piece.rows.clone(), &mut |run| {
                                for view_row in run {
                                    let local = view_row - row_base;
                                    if ids[local as usize] < cut {
                                        let value = read(local).or_else(|| {
                                            entries.entry(local).and_then(|e| copied(local, e))
                                        });
                                        take(value, view_row);
                                    }
                                }
                            });
                        }
                        Ok::<_, EngineError>((tally, none, items, through))
                    },
                )
                .try_reduce(
                    || (empty(), 0, 0, Vec::new()),
                    |(a, m, i, mut x), (b, n, j, y)| {
                        x.extend(y);
                        Ok((a.merge(b), m + n, i + j, x))
                    },
                )
        })?;
        let (mut tally, mut none, items, through) = found;
        if !through.is_empty() {
            let rows = croaring::Bitmap::of(&through);
            let entities = super::set::crossing(cx.engine, cx.open, &rows)?;
            let (more, without) = cx
                .engine
                .pool
                .install(|| self.pass_entities(cx, &entities, &empty));
            tally = tally.merge(more);
            none += without;
        }
        Ok((tally, none, items))
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

    /// The field's per-entity values of `entities`, in parallel pieces of entity space, each
    /// piece's entities walked through every layer, then the buffered rows.
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
        let layers: Vec<&mosaica_filter::ValueColumn> =
            layers.base().into_iter().chain(layers.extents()).collect();
        let end = entities.maximum().map_or(0, |last| u64::from(last) + 1);
        let (mut tally, mut valued) = (0..end.div_ceil(ENTITY_PIECE))
            .into_par_iter()
            .fold(
                || (empty(), 0u64),
                |(mut tally, mut valued), p| {
                    let range = p * ENTITY_PIECE..((p + 1) * ENTITY_PIECE).min(end);
                    let mut piece =
                        croaring::Bitmap::from_range(range.start as u32..range.end as u32);
                    piece.and_inplace(entities);
                    if piece.is_empty() {
                        return (tally, valued);
                    }
                    for layer in &layers {
                        let _ = layer.for_each_record_value_in(&piece, |entity, value| {
                            valued += 1;
                            if let Some(x) = K::of_record(&value) {
                                tally.add(x, entity);
                            }
                            Ok::<(), ()>(())
                        });
                    }
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
                    tally.add(x, entity);
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
        segments: &[(&mosaica_store::read::SegmentData, u32)],
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
                                        tally.add($num(values[local]), row);
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
            .reduce(|| (empty(), 0), |(a, m), (b, n)| (a.merge(b), m + n))
    }
}

/// How one set was counted.
#[derive(Debug, Clone, Copy)]
struct Counted {
    items: u64,
    sampled: bool,
}

/// How a set's sample below `cut` is read: each piece of the view's rows from band `band`, or by
/// scanning the set's rows' identities.
struct SamplePlan {
    cut: u64,
    band: u32,
    pieces: Vec<SamplePiece>,
}

struct SamplePiece {
    segment: usize,
    /// View rows.
    rows: std::ops::Range<u32>,
    /// The band's entries among the rows.
    entries: u64,
    banded: bool,
}

impl SamplePlan {
    /// The plan for `members` sampled below `cut`, or `None` where no band holds every identity
    /// below the cut and the set is counted exactly. Whether a set is sampled depends on its size
    /// and the sample size alone, both inside the visible set. Each piece is then read from the
    /// band or by scanning, whichever its band entries times [`BAND_ENTRY_ROWS`] and its members
    /// say costs less; the two read the same items.
    fn of(cx: &Cx<'_>, members: &CellSet<'_>, cut: u64) -> Option<SamplePlan> {
        let band = mosaica_store::bands::band_below(cut)?;
        let segments = cx.segments();
        let pieces = pieces(segments)
            .into_par_iter()
            .map(|(s, rows)| {
                let (segment, row_base) = segments[s];
                let held = &segment.bands.rows()[segment.bands.band(band)];
                let entries = (held.partition_point(|&r| r < rows.end - row_base)
                    - held.partition_point(|&r| r < rows.start - row_base))
                    as u64;
                let banded = entries.saturating_mul(BAND_ENTRY_ROWS) < members.count(rows.clone());
                SamplePiece {
                    segment: s,
                    rows,
                    entries,
                    banded,
                }
            })
            .collect();
        Some(SamplePlan { cut, band, pieces })
    }

    /// The band entries the banded pieces read.
    fn entries_read(&self) -> u64 {
        self.pieces
            .iter()
            .filter(|p| p.banded)
            .map(|p| p.entries)
            .sum()
    }
}

/// How many items of one set fall in each bin, in none, and have no value.
struct Counts {
    bins: Vec<u64>,
    rest: u64,
    none: u64,
}

/// A value as a pass compares it: `i128` for an integer or a timestamp, which holds every stored
/// integer exactly, and `f64` for a float or for an integer binned by a fractional bound.
trait Num: Copy + PartialOrd + Send + Sync {
    /// What a sum of these is held in while a pass adds them, exactly.
    type Sum: Default + Send;

    fn int(x: i128) -> Self;
    fn float(x: f64) -> Self;
    fn finite(self) -> bool;
    fn number(self) -> Number;
    fn add_to(sum: &mut Self::Sum, x: Self);
    fn joined(sum: Self::Sum, other: Self::Sum) -> Self::Sum;
    fn exact(sum: Self::Sum) -> Sum;

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

// A table's kind decides which of the two a pass uses, so a float never reaches `int`; an integer
// reaches `float` only where a fractional bound has its bins cut in float.
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
    fn number(self) -> Number {
        Number::Int(self)
    }
    type Sum = i128;
    fn add_to(sum: &mut i128, x: Self) {
        *sum += x;
    }
    fn joined(sum: i128, other: i128) -> i128 {
        sum + other
    }
    fn exact(sum: i128) -> Sum {
        Sum::Int(sum)
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
    fn number(self) -> Number {
        Number::Float(self)
    }
    type Sum = ExactSum;
    fn add_to(sum: &mut ExactSum, x: Self) {
        sum.add_float(x);
    }
    fn joined(sum: ExactSum, other: ExactSum) -> ExactSum {
        sum.plus(&other)
    }
    fn exact(sum: ExactSum) -> Sum {
        Sum::Float(sum.compact())
    }
}

/// What a pass accumulates, piece by piece, then merged. `at` is where the value was read: the
/// row in a pass over rows, the entity in a pass over entities.
trait Tally<K>: Send {
    fn add(&mut self, x: K, at: u32);
    fn merge(self, other: Self) -> Self;
}

/// The count and exact sum of the finite values seen, and the `keep` smallest and largest of them,
/// each with where it was read.
struct Summary<K: Num> {
    keep: usize,
    /// Values seen, finite or not.
    seen: u64,
    count: u64,
    sum: K::Sum,
    low: Vec<(K, u32)>,
    high: Vec<(K, u32)>,
}

impl<K: Num> Summary<K> {
    fn new(keep: usize) -> Self {
        Summary {
            keep,
            seen: 0,
            count: 0,
            sum: K::Sum::default(),
            low: Vec::new(),
            high: Vec::new(),
        }
    }

    fn finish(self, none: u64) -> FieldTally {
        let side = |held: Vec<(K, u32)>| held.into_iter().map(|(x, at)| (x.number(), at)).collect();
        FieldTally {
            rows: self.seen + none,
            none,
            count: self.count,
            sum: K::exact(self.sum),
            low: side(self.low),
            high: side(self.high),
        }
    }
}

impl<K: Num> Tally<K> for Summary<K> {
    #[inline]
    fn add(&mut self, x: K, at: u32) {
        self.seen += 1;
        if !x.finite() {
            return;
        }
        self.count += 1;
        K::add_to(&mut self.sum, x);
        if self.keep > 0 {
            keep_extreme(&mut self.low, self.keep, (x, at), false);
            keep_extreme(&mut self.high, self.keep, (x, at), true);
        }
    }

    fn merge(mut self, other: Self) -> Self {
        self.seen += other.seen;
        self.count += other.count;
        self.sum = K::joined(self.sum, other.sum);
        for value in other.low {
            keep_extreme(&mut self.low, self.keep, value, false);
        }
        for value in other.high {
            keep_extreme(&mut self.high, self.keep, value, true);
        }
        self
    }
}

/// Reads one field's values for its figures in one request's view: from the drawn column where the
/// view's rows carry it, and otherwise through each row's entity.
struct Reader<'a> {
    field: &'a Numbers,
    cx: &'a Cx<'a>,
}

impl FieldRead for Reader<'_> {
    fn tally(&self, rows: &croaring::Bitmap, keep: usize) -> Result<FieldTally> {
        match self.field.kind == Kind::Float {
            true => self.read::<f64>(rows, keep),
            false => self.read::<i128>(rows, keep),
        }
    }
}

impl Reader<'_> {
    fn read<K: Num>(&self, rows: &croaring::Bitmap, keep: usize) -> Result<FieldTally> {
        let cx = self.cx;
        cx.check_cancelled()?;
        if rows.is_empty() {
            return Ok(FieldTally::default());
        }
        let empty = || Summary::<K>::new(keep);
        if self.field.drawn {
            let (summary, none) = cx.engine.pool.install(|| {
                self.field
                    .pass_rows(CellSet::Rows(rows), cx.segments(), empty)
            });
            return Ok(summary.finish(none));
        }
        let entities = super::set::crossing(cx.engine, cx.open, rows)?;
        let (summary, none) = cx
            .engine
            .pool
            .install(|| self.field.pass_entities(cx, &entities, empty));
        let mut tally = summary.finish(none);
        // The kept values were read by entity, and a deny names rows: each is placed again by its
        // row.
        let row_space = &cx.open.served.data.row_space;
        for (high, side) in [(false, &mut tally.low), (true, &mut tally.high)] {
            let mut placed = Vec::with_capacity(side.len());
            for mut kept in side.drain(..) {
                let row = row_space
                    .row_of(mosaica_types::EntityId::new(u64::from(kept.1)))
                    .ok_or_else(|| {
                        EngineError::Malformed(format!(
                            "an entity read from view '{}' has no row there",
                            cx.open.served.name
                        ))
                    })?;
                kept.1 = row.raw();
                keep_extreme(&mut placed, keep, kept, high);
            }
            *side = placed;
        }
        Ok(tally)
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
    fn add(&mut self, x: K, _: u32) {
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
        (a, b) => mosaica_filter::as_f64(a) < mosaica_filter::as_f64(b),
    }
}

fn int_of(bound: Scalar) -> i128 {
    match bound {
        Scalar::Int(i) => i,
        Scalar::Float(f) => f as i128,
    }
}

/// `[lower, upper]` cut into `n` bins of equal width. Where the bounds are multiples of a readable
/// width `n` widths apart, the edges are those multiples, as readable edges are drawn, so a first
/// answer's outer edges sent back as a range give its edges again. Otherwise each inner edge is
/// weighed between the two bounds, so none passes the float range. A range too narrow for its
/// edges to rise is one bin.
fn equal_floats(lower: f64, upper: f64, n: u32) -> Vec<f64> {
    if let Some(edges) = readable_cut(lower, upper, n) {
        return edges;
    }
    let edges: Vec<f64> = (0..=n)
        .map(|i| match i {
            0 => lower,
            i if i == n => upper,
            i => {
                let t = f64::from(i) / f64::from(n);
                lower * (1.0 - t) + upper * t
            }
        })
        .collect();
    if edges.windows(2).all(|w| w[0] < w[1]) {
        edges
    } else {
        vec![lower, upper]
    }
}

/// `k` times the readable step `m` times ten to the `e`, as every readable edge is computed.
fn readable(k: f64, m: f64, e: i32) -> f64 {
    if e >= 0 {
        k * m * 10f64.powi(e)
    } else {
        k * m / 10f64.powi(-e)
    }
}

/// The edges of `[lower, upper]` in `n` bins where both bounds are, exactly, multiples of one
/// readable step `n` steps apart.
fn readable_cut(lower: f64, upper: f64, n: u32) -> Option<Vec<f64>> {
    let n = f64::from(n);
    let near = (upper / n - lower / n).log10().floor();
    if !near.is_finite() {
        return None;
    }
    let near = near as i32;
    for e in near - 1..=near + 1 {
        for m in MANTISSAS {
            let first = (lower / readable(1.0, m, e)).round();
            let edge = |i: f64| readable(first + i, m, e);
            if edge(0.0) == lower && edge(n) == upper {
                let edges: Vec<f64> = (0..=n as u32).map(|i| edge(f64::from(i))).collect();
                return edges.windows(2).all(|w| w[0] < w[1]).then_some(edges);
            }
        }
    }
    None
}

/// `[lower, upper]` cut into `n` bins of whole widths that differ by at most one.
fn equal_ints(lower: i128, upper: i128, n: u32) -> Vec<i128> {
    // Far past any served column's span, so the arithmetic below cannot overflow.
    let far = 1i128 << 100;
    let (lower, upper) = (lower.clamp(-far, far), upper.clamp(-far, far));
    let n = i128::from(n);
    let (whole, part) = ((upper - lower) / n, (upper - lower) % n);
    (0..=n).map(|i| lower + whole * i + part * i / n).collect()
}

/// The mantissas of a readable step, each times a power of ten.
const MANTISSAS: [f64; 4] = [1.0, 2.0, 2.5, 5.0];

/// At most `n` bins of one readable width covering `[min, max]`, starting at a multiple of the
/// width. A width whose edges round at a large magnitude so that they no longer cover the values,
/// or pass the float range, is passed over for a wider one.
///
/// With one bin, values either side of 0 have no readable width: 0 is a multiple of every width,
/// so a bin starting at a multiple below 0 ends at or below 0. Those values, and a span no
/// readable width covers within the float range, are cut into equal bins.
fn readable_floats(min: f64, max: f64, n: u32) -> Vec<f64> {
    let bins_asked = n;
    let n = f64::from(n);
    // The span divided first so that it stays finite; a subnormal span starts at the smallest
    // power of ten a float holds.
    let start = if max > min {
        ((max / n - min / n).log10().floor() as i32).max(-324) - 1
    } else if min != 0.0 {
        min.abs().log10().floor() as i32
    } else {
        0
    };
    for e in start..start.saturating_add(40) {
        for m in MANTISSAS {
            let step = readable(1.0, m, e);
            let edge = |k: f64| readable(k, m, e);
            let mut first = (min / step).floor();
            // Past 2^52 multiples a step is below the values' precision.
            if first.is_nan() || first.abs() >= 4_503_599_627_370_496.0 {
                continue;
            }
            if edge(first) > min {
                first -= 1.0;
            }
            let mut bins = ((max - edge(first)) / step).ceil().max(1.0);
            while edge(first + bins) < max && bins <= n {
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
            if rising && edges.iter().all(|e| e.is_finite()) && edges[0] <= min && last >= max {
                return edges;
            }
        }
    }
    equal_floats(min, max, bins_asked)
}

/// At most `n` bins of the narrowest readable whole width covering `min..=max`, starting at a
/// multiple of the width, the last bin's upper edge past `max` so each bin spans the same count of
/// integers. A single value is binned by the largest power of ten at or below it.
///
/// With one bin, values either side of 0 have no readable width, as in [`readable_floats`], and
/// are one bin from `min` to `max + 1`.
fn readable_ints(min: i128, max: i128, n: u32) -> Vec<i128> {
    let n = i128::from(n);
    let narrowest = if min == max {
        let mut power = 1u128;
        while power * 10 <= min.unsigned_abs() {
            power *= 10;
        }
        power as i128
    } else {
        1
    };
    let mut ten: i128 = 1;
    loop {
        // 1, 2, 2.5 and 5 times `ten`, as a numerator and a denominator.
        for (times, per) in [(1, 1), (2, 1), (5, 2), (5, 1)] {
            let Some(width) = ten.checked_mul(times).filter(|w| w % per == 0) else {
                continue;
            };
            let width = width / per;
            if width < narrowest {
                continue;
            }
            let (first, last) = (min.div_euclid(width), max.div_euclid(width));
            if last - first < n {
                return (first..=last + 1).map(|k| k * width).collect();
            }
        }
        match ten.checked_mul(10) {
            Some(next) => ten = next,
            None => return vec![min, max + 1],
        }
    }
}

const SECOND: i64 = 1_000_000;
const MINUTE: i64 = 60 * SECOND;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// A readable width of a timestamp bin.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// A fixed width in microseconds, its bins starting at a multiple of it after `offset`.
    Fixed {
        width: i64,
        offset: i64,
    },
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
fn readable_times(min: i64, max: i64, n: u32) -> Vec<i128> {
    let finest = if min == max {
        time_steps()
            .position(|s| matches!(s, Step::Fixed { width: DAY, .. }))
            .unwrap_or(0)
    } else {
        0
    };
    let (lower, upper) = (i128::from(min), i128::from(max));
    time_steps()
        .skip(finest)
        .find_map(|step| step_edges(step, min, max, i128::from(n)))
        .unwrap_or_else(|| Kind::Timestamp.clamp(equal_ints(lower, upper + 1, n)))
}

/// The edges of `step`'s bins from the one holding `min` to the one holding `max`, where they
/// number at most `n` and every edge is within an `i64`.
fn step_edges(step: Step, min: i64, max: i64, n: i128) -> Option<Vec<i128>> {
    let (first, last) = (step_index(step, min), step_index(step, max));
    if last - first + 1 > n {
        return None;
    }
    (first..=last + 1)
        .map(|k| step_start(step, k).filter(|&edge| i64::try_from(edge).is_ok()))
        .collect()
}

/// Which of `step`'s bins holds `t`, counted from the bin starting at the epoch, January of year 0
/// or year 0.
fn step_index(step: Step, t: i64) -> i128 {
    match step {
        Step::Fixed { width, offset } => {
            (i128::from(t) - i128::from(offset)).div_euclid(i128::from(width))
        }
        Step::Months(k) => {
            let (year, month) = civil_from_days(t.div_euclid(DAY));
            (i128::from(year) * 12 + i128::from(month - 1)).div_euclid(i128::from(k))
        }
        Step::Years(k) => {
            i128::from(civil_from_days(t.div_euclid(DAY)).0).div_euclid(i128::from(k))
        }
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

    fn date(year: i64, month: i64, day: i64) -> i128 {
        i128::from(days_from_civil(year, month, day) * DAY)
    }

    fn rising_and_covering<T: PartialOrd + Copy + std::fmt::Debug>(edges: &[T], min: T, max: T) {
        assert!(edges.len() >= 2, "{edges:?}");
        assert!(
            edges.windows(2).all(|w| w[0] < w[1]),
            "{edges:?} do not rise"
        );
        assert!(
            edges[0] <= min && edges[edges.len() - 1] >= max,
            "{edges:?} miss {min:?}..{max:?}"
        );
    }

    #[test]
    fn the_calendar_round_trips_across_eras_and_leap_days() {
        for days in (-800_000..800_000).step_by(37) {
            let (year, month) = civil_from_days(days);
            let first = days_from_civil(year, month, 1);
            assert!(first <= days && days - first < 31, "{days}: {year}-{month}");
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(
            days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 1),
            29
        );
        assert_eq!(
            days_from_civil(1900, 3, 1) - days_from_civil(1900, 2, 1),
            28
        );
        assert_eq!(civil_from_days(-1), (1969, 12));
    }

    #[test]
    fn readable_float_edges_cover_the_values_in_at_most_the_bins_asked_for() {
        let tenths: Vec<f64> = (1..=9).map(|k| f64::from(k) / 10.0).collect();
        assert_eq!(readable_floats(0.13, 0.87, 10), tenths);
        assert_eq!(
            readable_floats(-3.0, 47.0, 5),
            vec![-20.0, 0.0, 20.0, 40.0, 60.0]
        );
        assert_eq!(readable_floats(3.7, 3.7, 4), vec![3.0, 4.0]);
        let cases = [
            (0.0, 1e-9, 7),
            (-1e300, 1e300, 3),
            (1e15 + 0.1, 1e15 + 0.3, 20),
            (5.0, 5.000001, 1),
            (-f64::MAX, f64::MAX, 1),
            (-f64::MAX, f64::MAX, 2),
            (-f64::MAX, f64::MAX, 5),
            (-1e308, 1e308, 1),
            (5e-324, 1e-323, 10),
            (0.0, f64::MAX, 3),
        ];
        for (min, max, n) in cases {
            let edges = readable_floats(min, max, n);
            assert!(edges.len() <= n as usize + 1, "{min} {max} {n}: {edges:?}");
            assert!(edges.iter().all(|e| e.is_finite()), "{edges:?}");
            rising_and_covering(&edges, min, max);
        }
        // One bin either side of 0 is the values' span.
        assert_eq!(readable_floats(-0.5, 2.0, 1), vec![-0.5, 2.0]);
        // A single value at the float range's end is one bin holding it.
        assert_eq!(
            readable_floats(f64::MAX, f64::MAX, 3),
            vec![f64::MAX, f64::MAX]
        );
    }

    #[test]
    fn a_float_range_is_cut_into_rising_edges_or_is_one_bin() {
        assert_eq!(
            equal_floats(0.0, 50.0, 5),
            vec![0.0, 10.0, 20.0, 30.0, 40.0, 50.0]
        );
        let edges = equal_floats(-1e308, 1e308, 4);
        assert_eq!((edges.len(), edges[2]), (5, 0.0));
        rising_and_covering(&edges, -1e308, 1e308);
        for n in [2, 3, 7, 1000] {
            let edges = equal_floats(-f64::MAX, f64::MAX, n);
            assert_eq!(edges.len(), n as usize + 1);
            rising_and_covering(&edges, -f64::MAX, f64::MAX);
        }
        // Too narrow to cut: one bin.
        assert_eq!(equal_floats(5e-324, 1e-323, 4), vec![5e-324, 1e-323]);
        let next = f64::from_bits(1.0f64.to_bits() + 1);
        assert_eq!(equal_floats(1.0, next, 3), vec![1.0, next]);
    }

    #[test]
    fn readable_integer_edges_are_exact_whole_widths() {
        assert_eq!(readable_ints(1, 5, 10), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(
            readable_ints(0, 99, 10),
            (0..=10).map(|k| k * 10).collect::<Vec<_>>()
        );
        assert_eq!(readable_ints(0, 99, 4), vec![0, 25, 50, 75, 100]);
        assert_eq!(readable_ints(7, 7, 4), vec![7, 8]);
        assert_eq!(readable_ints(1234, 1234, 3), vec![1000, 2000]);
        assert_eq!(readable_ints(-5, -5, 3), vec![-5, -4]);
        // Past 2^53, every edge is still the exact integer.
        let big = 1i128 << 60;
        assert_eq!(
            readable_ints(big, big + 3, 10),
            (0..=4).map(|k| big + k).collect::<Vec<_>>()
        );
        // One bin either side of 0: the values' span, its upper edge past the largest.
        assert_eq!(readable_ints(-3, 5, 1), vec![-3, 6]);
        for (min, max, n) in [
            (0, i128::from(u64::MAX), 20),
            (i128::from(i64::MIN), i128::from(i64::MAX), 7),
            (1 << 63, (1 << 63) + 100, 20),
            (-1, 0, 1),
        ] {
            let edges = readable_ints(min, max, n);
            assert!(edges.len() <= n as usize + 1, "{edges:?}");
            rising_and_covering(&edges, min, max + 1);
            let width = edges[1] - edges[0];
            assert!(
                edges.windows(2).all(|w| w[1] - w[0] == width) || n == 1,
                "{edges:?}"
            );
        }
    }

    #[test]
    fn an_integer_range_is_cut_exactly_and_held_within_the_served_column() {
        let big = 1i128 << 63;
        assert_eq!(
            equal_ints(big, big + 100, 4),
            vec![big, big + 25, big + 50, big + 75, big + 100]
        );
        let top = (1i128 << 54) + 3;
        let edges = equal_ints(0, top, 4);
        assert_eq!((edges[0], edges[4]), (0, top));
        assert!(edges.windows(2).all(|w| (w[1] - w[0] - top / 4).abs() <= 1));
        assert_eq!(equal_ints(0, 10, 4), vec![0, 2, 5, 7, 10]);
        // Bounds no column holds neither overflow nor leave the served span.
        let edges = Kind::Unsigned.clamp(equal_ints(-10, i128::MAX, 3));
        assert_eq!(
            edges,
            vec![
                0,
                i128::from(u64::MAX),
                i128::from(u64::MAX),
                i128::from(u64::MAX)
            ]
        );
        let edges = Kind::Signed.clamp(equal_ints(i128::MIN, i128::MAX, 2));
        assert_eq!(edges, vec![i128::from(i64::MIN), 0, i128::from(i64::MAX)]);
    }

    #[test]
    fn readable_edges_sent_back_as_a_range_are_cut_again() {
        for (min, max, n) in [
            (0.13, 0.87, 10),
            (-3.0, 47.0, 5),
            (1e-7, 3.3e-6, 17),
            (-123.456, 0.001, 30),
            (1e12, 7.77e14, 9),
            (0.3, 0.3, 2),
        ] {
            let edges = readable_floats(min, max, n);
            let bins = edges.len() as u32 - 1;
            assert_eq!(
                equal_floats(edges[0], edges[bins as usize], bins),
                edges,
                "{min} {max}"
            );
        }
        for (min, max, n) in [
            (1, 5, 10),
            (0, 99, 4),
            (-1_000_003, 7, 13),
            (1 << 60, (1 << 60) + 3, 2),
        ] {
            let edges = readable_ints(min, max, n);
            let bins = edges.len() as u32 - 1;
            assert_eq!(
                equal_ints(edges[0], edges[bins as usize], bins),
                edges,
                "{min} {max}"
            );
        }
    }

    #[test]
    fn readable_time_edges_fall_on_the_calendar() {
        let years = |from: i64, to: i64| (from..=to).map(|y| date(y, 1, 1)).collect::<Vec<_>>();
        let at = |t: i128| t as i64;
        // Three years and a bit, in at most 5 bins: years.
        let edges = readable_times(at(date(2019, 5, 3)), at(date(2022, 2, 1)), 5);
        assert_eq!(edges, years(2019, 2023));
        // The same in at most 20 bins: two months each, from January, March, May and so on.
        let edges = readable_times(at(date(2019, 5, 3)), at(date(2022, 2, 1)), 20);
        assert_eq!(
            (edges[0], edges[1], edges.len()),
            (date(2019, 5, 1), date(2019, 7, 1), 18)
        );
        // Ten months: months.
        let edges = readable_times(at(date(2021, 3, 15)), at(date(2021, 12, 31)) + HOUR, 12);
        let months: Vec<i128> = (3..=12)
            .map(|m| date(2021, m, 1))
            .chain([date(2022, 1, 1)])
            .collect();
        assert_eq!(edges, months);
        // Five weeks in at most 6 bins: weeks from Monday.
        let edges = readable_times(at(date(2024, 1, 3)), at(date(2024, 2, 4)), 6);
        assert_eq!(edges[0], date(2024, 1, 1));
        assert!(edges.windows(2).all(|w| w[1] - w[0] == i128::from(7 * DAY)));
        // A day in hours.
        let day = at(date(2024, 6, 1));
        let edges = readable_times(day + 30 * MINUTE, day + 23 * HOUR, 24);
        assert_eq!((edges[0], edges.len()), (date(2024, 6, 1), 25));
        // One instant: its day.
        assert_eq!(
            readable_times(day + HOUR, day + HOUR, 10),
            vec![date(2024, 6, 1), date(2024, 6, 2)]
        );
        // Before the epoch, and the widest span an i64 holds.
        let edges = readable_times(at(date(1900, 7, 1)), at(date(1960, 1, 1)), 10);
        assert_eq!(
            edges,
            (0..=7)
                .map(|k| date(1900 + 10 * k, 1, 1))
                .collect::<Vec<_>>()
        );
        let edges = readable_times(i64::MIN, i64::MAX, 3);
        assert!(edges.len() >= 2 && edges.len() <= 4 && edges[0] <= i128::from(i64::MIN) + 1);
        assert!(edges.iter().all(|&e| i64::try_from(e).is_ok()));
    }

    #[test]
    fn a_value_falls_in_the_bin_whose_edges_hold_it_and_the_last_bin_is_closed() {
        let edges = [0.0, 0.5, 1.0, 1.5];
        let mut floats = Histogram::new(&edges[..3], edges[3]);
        for x in [
            -0.1,
            0.0,
            0.49,
            0.5,
            1.4999,
            1.5,
            1.50001,
            f64::NAN,
            f64::INFINITY,
        ] {
            floats.add(x, 0);
        }
        assert_eq!((floats.bins.clone(), floats.rest), (vec![2, 1, 2], 4));
        // Integers past 2^63 against exact edges.
        let big = 1i128 << 63;
        let edges = equal_ints(big, big + 100, 4);
        let mut ints = Histogram::new(&edges[..4], edges[4]);
        for x in [
            big - 1,
            big,
            big + 5,
            big + 24,
            big + 25,
            big + 100,
            big + 101,
        ] {
            ints.add(x, 0);
        }
        assert_eq!((ints.bins.clone(), ints.rest), (vec![3, 1, 0, 1], 2));
    }
}
