//! A filter evaluated in row space: the hot-column scan, the crossing, and the leaf resolvers.

use super::*;
use crate::filter::{as_f64, narrow_hi, narrow_lo, NativeBound, Narrowed};

impl Engine {
    /// Cross a filter's entity-space result into one view's row space, by whichever of two routes
    /// is cheaper: projecting the whole result, or walking the request's own tiles and testing
    /// each row's entity. [`PER_TILE_CROSSING_RATIO`] picks between them, and the two agree
    /// exactly over every range the request can ask about. A view with no `row-entity.u32` gets
    /// the projecting route regardless — also `per_tile_only`'s fallback, the highlight's route,
    /// since its answers are all inside the request's own tiles.
    fn cross_filter_into_row_space(
        &self,
        served: &ServedView<'_>,
        entities: &croaring::Bitmap,
        domain: &[Range<u32>],
        rows_in_ranges: u64,
        per_tile_only: bool,
    ) -> FilterRows {
        let row_space = &served.data.row_space;
        let per_tile_looks_cheaper = per_tile_only
            || entities.cardinality() > rows_in_ranges.saturating_mul(PER_TILE_CROSSING_RATIO);
        if per_tile_looks_cheaper && row_space.can_invert() {
            // `None` is the row space declining to invert a row; falling through to the exact
            // route costs latency only, where trusting a partial answer would drop rows.
            if let Some(rows) = self
                .pool
                .install(|| per_tile_crossing(row_space, entities, domain, rows_in_ranges))
            {
                self.counters
                    .filter_crossings_per_tile
                    .fetch_add(1, Ordering::Relaxed);
                return FilterRows::Viewport {
                    rows,
                    domain: domain.to_vec(),
                };
            }
        }
        self.counters
            .filter_crossings_projected
            .fetch_add(1, Ordering::Relaxed);
        FilterRows::Complete(row_space.project(entities))
    }

    /// A routed filter's answer as rows over `domain`, ascending, disjoint and merged view-space
    /// ranges holding `rows_in_ranges` rows: an entity-space verdict crossed into row space, or a
    /// tree with row-space leaves evaluated there. `per_tile_only` holds either to the domain
    /// rather than letting a whole-view projection be chosen where it is cheaper.
    pub(crate) fn rows_of_routed(
        &self,
        served: &ServedView<'_>,
        routed: crate::filter::RoutedFilter,
        domain: &[Range<u32>],
        rows_in_ranges: u64,
        per_tile_only: bool,
    ) -> Result<RoutedRows> {
        Ok(match routed {
            crate::filter::RoutedFilter::Entity(entities) => RoutedRows {
                unmasked: Some(entities.cardinality()),
                rows: self.cross_filter_into_row_space(
                    served,
                    &entities,
                    domain,
                    rows_in_ranges,
                    per_tile_only,
                ),
                region: None,
            },
            crate::filter::RoutedFilter::Row(tree) => {
                let rows =
                    self.evaluate_row_route(&tree, served, domain, rows_in_ranges, per_tile_only)?;
                self.counters.filter_row_routed.fetch_add(1, Ordering::Relaxed);
                RoutedRows {
                    // A region's interior rows have not met the mask, so their count is a
                    // pre-mask quantity about the region, and is not taken.
                    unmasked: (!tree.has_region()).then(|| rows.rows().cardinality()),
                    region: tree.region_verdict(),
                    rows,
                }
            }
        })
    }

    /// Evaluate a routed filter tree with row-space leaves over the request's own rows, exact
    /// over `domain` and silent outside it. A leaf reads every row of the domain whatever the
    /// principal may see, so the bitmap returned here still contains suppressed rows; a caller
    /// narrows the answer only through `EffectiveMask::with_filter`. Code 0 is the vocabulary's
    /// absent sentinel and matches nothing. Every entity-space verdict is crossed in one joint
    /// walk, or projected where cheaper, and the tree combines entirely in row space. A tree
    /// bounded by nothing but region leaves and projected verdicts comes back
    /// [`FilterRows::Complete`]; a render leaf or a per-tile crossing bounds the answer and comes
    /// back [`FilterRows::Viewport`].
    pub(crate) fn evaluate_row_route(
        &self,
        tree: &crate::filter::RowExpr,
        served: &ServedView<'_>,
        domain: &[Range<u32>],
        rows_in_ranges: u64,
        per_tile_only: bool,
    ) -> Result<FilterRows> {
        let row_space = &served.data.row_space;
        let segments = &served.segments[..];
        let total_rows = row_space.total_rows();
        // Every entity-space verdict's row image, crossed together in one walk.
        let verdicts = tree.entity_verdicts();
        let mut whole_view = tree.is_whole_view();
        let images: Vec<croaring::Bitmap> = if verdicts.is_empty() {
            Vec::new()
        } else {
            let total_matched: u64 = verdicts.iter().map(|v| v.cardinality()).sum();
            // `per_tile_only` is the highlight's route — see [`Engine::cross_filter_into_row_space`].
            let per_tile_looks_cheaper = per_tile_only
                || total_matched > rows_in_ranges.saturating_mul(PER_TILE_CROSSING_RATIO);
            let walked = (per_tile_looks_cheaper && row_space.can_invert())
                .then(|| {
                    self.pool.install(|| {
                        per_tile_crossing_multi(row_space, &verdicts, domain, rows_in_ranges)
                    })
                })
                .flatten();
            match walked {
                Some(images) => {
                    self.counters
                        .filter_crossings_per_tile
                        .fetch_add(1, Ordering::Relaxed);
                    whole_view = false;
                    images
                }
                None => {
                    self.counters
                        .filter_crossings_projected
                        .fetch_add(1, Ordering::Relaxed);
                    if whole_view {
                        verdicts.iter().map(|v| row_space.project(v)).collect()
                    } else {
                        // Clamped so the combined answer never claims a row outside what
                        // `FilterRows::Viewport` says was tested.
                        let mut domain_rows = croaring::Bitmap::new();
                        for range in domain {
                            domain_rows.add_range(range.clone());
                        }
                        verdicts
                            .iter()
                            .map(|v| row_space.project(v).and(&domain_rows))
                            .collect()
                    }
                }
            }
        };
        let scope = if whole_view {
            RowScope::WholeView {
                total_rows: u32::try_from(total_rows).unwrap_or(u32::MAX),
            }
        } else {
            let mut domain_rows = croaring::Bitmap::new();
            for range in domain {
                domain_rows.add_range(range.clone());
            }
            RowScope::Domain(domain_rows)
        };
        let mut next_image = 0usize;
        let rows = self
            .pool
            .install(|| eval_row_expr(tree, &images, &mut next_image, segments, domain, &scope))?;
        Ok(if whole_view {
            FilterRows::Complete(rows)
        } else {
            FilterRows::Viewport {
                rows,
                domain: domain.to_vec(),
            }
        })
    }
}

/// [`Engine::rows_of_routed`]'s answer.
pub(crate) struct RoutedRows {
    pub(crate) rows: FilterRows,
    /// The coarsest verdict a region leaf reached.
    pub(crate) region: Option<crate::region::RegionVerdict>,
    /// How many rows or entities matched before the mask, where that is a quantity about the
    /// filter alone: `None` for a tree holding a region.
    pub(crate) unmasked: Option<u64>,
}

/// The rows a row-space evaluation answers over — see [`Engine::evaluate_row_route`].
enum RowScope {
    /// Every row of the view: the tree holds nothing the request's domain bounds.
    WholeView { total_rows: u32 },
    /// The request's own rows, as one bitmap, because a sibling leaf is bounded by them.
    Domain(croaring::Bitmap),
}

impl RowScope {
    /// Every row in scope — a negated region's presence half.
    fn all_rows(&self) -> croaring::Bitmap {
        match self {
            RowScope::WholeView { total_rows } => croaring::Bitmap::from_range(0..*total_rows),
            RowScope::Domain(rows) => rows.clone(),
        }
    }

    /// A whole-view row set, narrowed to the scope where the scope is narrower.
    fn clamp(&self, rows: &croaring::Bitmap) -> croaring::Bitmap {
        match self {
            RowScope::WholeView { .. } => rows.clone(),
            RowScope::Domain(domain) => rows.and(domain),
        }
    }
}

/// Evaluate one routed node over `domain`, in row space. `images` are the pre-crossed row images
/// of the tree's entity-space verdicts, consumed in the order
/// [`crate::filter::RowExpr::entity_verdicts`] collects them; `next_image` is that cursor.
fn eval_row_expr(
    expr: &crate::filter::RowExpr,
    images: &[croaring::Bitmap],
    next_image: &mut usize,
    segments: &[(&SegmentData, u32)],
    domain: &[Range<u32>],
    scope: &RowScope,
) -> Result<croaring::Bitmap> {
    use crate::filter::RowExpr;
    match expr {
        RowExpr::Entity(_) => {
            let image = images[*next_image].clone();
            *next_image += 1;
            Ok(image)
        }
        RowExpr::Leaf {
            column,
            family,
            operand,
        } => {
            let values = LeafValues::of(*family, operand);
            scan_rows(segments, domain, column, values.predicate())
        }
        RowExpr::Region(region) => Ok(scope.clamp(&region.rows)),
        // Already `membership ∩ M_auth` over the whole view, clamped where a sibling leaf bounds
        // the tree to the request's rows.
        RowExpr::MemberOf(rows) => Ok(scope.clamp(rows)),
        RowExpr::NotInRows(kids) => {
            // Every rowed entity carries a position and may be a member, so the presence half of
            // this negation is every row in scope. No early exit on an empty difference: these
            // kids are already resolved, so skipping buys nothing.
            let mut out = scope.all_rows();
            for kid in kids {
                out.andnot_inplace(&eval_row_expr(
                    kid, images, next_image, segments, domain, scope,
                )?);
            }
            Ok(out)
        }
        RowExpr::AllOf(kids) => {
            let mut out: Option<croaring::Bitmap> = None;
            for kid in kids {
                let kid_rows = eval_row_expr(kid, images, next_image, segments, domain, scope)?;
                out = Some(match out {
                    None => kid_rows,
                    Some(mut acc) => {
                        acc.and_inplace(&kid_rows);
                        acc
                    }
                });
            }
            // Unreachable empty: an empty `all_of` is entity-pure and never routes here.
            Ok(out.unwrap_or_default())
        }
        RowExpr::AnyOf(kids) => {
            let mut out = croaring::Bitmap::new();
            for kid in kids {
                out |= eval_row_expr(kid, images, next_image, segments, domain, scope)?;
            }
            Ok(out)
        }
        RowExpr::NoneOf {
            column,
            family,
            kids,
        } => {
            // `present ∖ matched`, presence being whatever this column's family stores it as: a
            // non-sentinel code for a category, the presence bitmap for every other. An item that
            // cannot be read is answered as absent, so it under-reports rather than widening.
            let mut out = scan_rows(segments, domain, column, RowPredicate::present_in(*family))?;
            for (i, kid) in kids.iter().enumerate() {
                out.andnot_inplace(&eval_row_expr(
                    kid, images, next_image, segments, domain, scope,
                )?);
                if out.is_empty() {
                    // The remaining kids are skipped, but `images` is positional, so the cursor
                    // must still advance past their verdicts. See
                    // `a_short_circuited_negation_still_consumes_its_skipped_images`.
                    for skipped in &kids[i + 1..] {
                        *next_image += skipped.entity_verdicts().len();
                    }
                    break;
                }
            }
            Ok(out)
        }
    }
}

/// A row-space leaf's test against one row of the hot column. Absence is a per-family rule: a
/// category's is its vocabulary's reserved code 0; every other family's is a presence bitmap
/// beside the column, since the hot column is non-nullable and an absent number is written as
/// the type's zero, a value a range containing zero must not match.
enum RowPredicate<'a> {
    /// A category's code is non-sentinel and in this set. An empty set matches nothing.
    CodeIn(&'a [u32]),
    /// A category's code is non-sentinel — the presence half of a negation over one.
    CodePresent,
    /// A number's value equals one of these. An empty set matches nothing.
    NumberIn(&'a [Scalar]),
    /// A number's value lies between these bounds. Either may be absent, an open side.
    Range {
        lo: Option<Endpoint>,
        hi: Option<Endpoint>,
    },
    /// The row carries a value — the presence half of a negation over a bitmap-absence column.
    ValuePresent,
}

impl RowPredicate<'_> {
    /// The presence half of a negation over a column of this family.
    fn present_in(family: Family) -> RowPredicate<'static> {
        match family {
            Family::Category => RowPredicate::CodePresent,
            Family::Numeric => RowPredicate::ValuePresent,
            // Neither string family is row-placed; an empty code set is the fail-closed reading.
            Family::Keyword | Family::Text => RowPredicate::CodeIn(&[]),
        }
    }

    /// Does this family read absence from the presence bitmap? A category does not.
    fn reads_presence(&self) -> bool {
        match self {
            RowPredicate::CodeIn(_) | RowPredicate::CodePresent => false,
            RowPredicate::NumberIn(_) | RowPredicate::Range { .. } | RowPredicate::ValuePresent => {
                true
            }
        }
    }
}

/// One row-space leaf's comparands, owned for as long as the scan borrows them. A family/operand
/// pair the parse would have refused becomes an empty set, which matches nothing, never a panic
/// and never a number compared against a code.
enum LeafValues {
    Codes(Vec<u32>),
    Numbers(Vec<Scalar>),
    Range {
        lo: Option<Endpoint>,
        hi: Option<Endpoint>,
    },
}

impl LeafValues {
    fn of(family: Family, operand: &FilterOperand) -> LeafValues {
        match (family, operand) {
            (Family::Category, FilterOperand::Equals(v)) => LeafValues::Codes(vec![v.raw()]),
            (Family::Category, FilterOperand::In(vs)) => {
                LeafValues::Codes(vs.iter().map(|v| v.raw()).collect())
            }
            (Family::Numeric, FilterOperand::NumEquals(n)) => LeafValues::Numbers(vec![*n]),
            (Family::Numeric, FilterOperand::NumIn(ns)) => LeafValues::Numbers(ns.clone()),
            (Family::Numeric, FilterOperand::Range { lo, hi }) => {
                LeafValues::Range { lo: *lo, hi: *hi }
            }
            _ => LeafValues::Codes(Vec::new()),
        }
    }

    fn predicate(&self) -> RowPredicate<'_> {
        match self {
            LeafValues::Codes(codes) => RowPredicate::CodeIn(codes),
            LeafValues::Numbers(numbers) => RowPredicate::NumberIn(numbers),
            LeafValues::Range { lo, hi } => RowPredicate::Range { lo: *lo, hi: *hi },
        }
    }
}

/// A row-space leaf's predicate resolved against one segment's rendered column, settled once per
/// segment rather than once per row: a range's endpoints narrowed to the column's own type, a
/// needle the type cannot hold gone, a needle set sorted for a binary search. A predicate that
/// can match no row of the column has no representation: [`Prepared::of`] answers `None`.
enum Prepared<'a> {
    /// Every row of the run: presence alone decides — see [`scan_run`].
    EveryRow,
    Bool(&'a arrow::array::BooleanArray, IntTest<u8>),
    U8(&'a [u8], IntTest<u8>),
    U16(&'a [u16], IntTest<u16>),
    U32(&'a [u32], IntTest<u32>),
    U64(&'a [u64], IntTest<u64>),
    I8(&'a [i8], IntTest<i8>),
    I16(&'a [i16], IntTest<i16>),
    I32(&'a [i32], IntTest<i32>),
    I64(&'a [i64], IntTest<i64>),
    F32(&'a [f32], FloatTest),
    F64(&'a [f64], FloatTest),
}

/// What one stored integer is tested by, at the column's own width. `Eq` is kept apart from a
/// one-value `In` because it is the common shape and one comparison against a constant.
enum IntTest<T> {
    Eq(T),
    /// Sorted and deduplicated: a linear `contains` costs O(needles) per row.
    In(Vec<T>),
    /// Inclusive on both sides, exclusivity folded into the value; `None` is an open side.
    Range { lo: Option<T>, hi: Option<T> },
}

/// What one stored float is tested by. NaN must stay unordered, which narrowing through an
/// integer would destroy, so floats keep the `f64` comparison.
enum FloatTest {
    /// Unsorted: NaN equals nothing, itself included, so a NaN needle matches no row without a
    /// special case.
    In(Vec<f64>),
    /// Each side carries its own inclusivity, there being no next float to fold an exclusive
    /// bound into.
    Range {
        lo: Option<(f64, bool)>,
        hi: Option<(f64, bool)>,
    },
}

impl<'a> Prepared<'a> {
    /// The predicate against one segment's column, or `None` where no row of that segment can
    /// match: a range narrowed to nothing, or a needle set the column's type cannot hold.
    fn of(slice: &ScalarSlice<'a>, predicate: &RowPredicate<'_>) -> Option<Prepared<'a>> {
        match predicate {
            // No value is consulted: this family's absence lives in the bitmap beside the column.
            RowPredicate::ValuePresent => Some(Prepared::EveryRow),
            RowPredicate::CodeIn(codes) => match slice {
                ScalarSlice::U8(v) => Some(Prepared::U8(v, code_test(codes)?)),
                ScalarSlice::U16(v) => Some(Prepared::U16(v, code_test(codes)?)),
                ScalarSlice::U32(v) => Some(Prepared::U32(v, code_test(codes)?)),
                _ => None,
            },
            // A category carries a value when its code is not the sentinel; codes are unsigned,
            // so "present" is "at least 1".
            RowPredicate::CodePresent => match slice {
                ScalarSlice::U8(v) => Some(Prepared::U8(v, above_zero())),
                ScalarSlice::U16(v) => Some(Prepared::U16(v, above_zero())),
                ScalarSlice::U32(v) => Some(Prepared::U32(v, above_zero())),
                _ => None,
            },
            RowPredicate::NumberIn(needles) => match slice {
                ScalarSlice::Bool(a) => Some(Prepared::Bool(a, int_in(needles)?)),
                ScalarSlice::U8(v) => Some(Prepared::U8(v, int_in(needles)?)),
                ScalarSlice::U16(v) => Some(Prepared::U16(v, int_in(needles)?)),
                ScalarSlice::U32(v) => Some(Prepared::U32(v, int_in(needles)?)),
                ScalarSlice::U64(v) => Some(Prepared::U64(v, int_in(needles)?)),
                ScalarSlice::I8(v) => Some(Prepared::I8(v, int_in(needles)?)),
                ScalarSlice::I16(v) => Some(Prepared::I16(v, int_in(needles)?)),
                ScalarSlice::I32(v) => Some(Prepared::I32(v, int_in(needles)?)),
                ScalarSlice::I64(v) | ScalarSlice::TimestampUs(v) => {
                    Some(Prepared::I64(v, int_in(needles)?))
                }
                ScalarSlice::F32(v) => Some(Prepared::F32(v, float_in(needles)?)),
                ScalarSlice::F64(v) => Some(Prepared::F64(v, float_in(needles)?)),
                ScalarSlice::Utf8(_) => None,
            },
            RowPredicate::Range { lo, hi } => match slice {
                // A bool is compared as the 0/1 the entity route stores it as.
                ScalarSlice::Bool(a) => Some(Prepared::Bool(a, int_range(*lo, *hi)?)),
                ScalarSlice::U8(v) => Some(Prepared::U8(v, int_range(*lo, *hi)?)),
                ScalarSlice::U16(v) => Some(Prepared::U16(v, int_range(*lo, *hi)?)),
                ScalarSlice::U32(v) => Some(Prepared::U32(v, int_range(*lo, *hi)?)),
                ScalarSlice::U64(v) => Some(Prepared::U64(v, int_range(*lo, *hi)?)),
                ScalarSlice::I8(v) => Some(Prepared::I8(v, int_range(*lo, *hi)?)),
                ScalarSlice::I16(v) => Some(Prepared::I16(v, int_range(*lo, *hi)?)),
                ScalarSlice::I32(v) => Some(Prepared::I32(v, int_range(*lo, *hi)?)),
                ScalarSlice::I64(v) | ScalarSlice::TimestampUs(v) => {
                    Some(Prepared::I64(v, int_range(*lo, *hi)?))
                }
                ScalarSlice::F32(v) => Some(Prepared::F32(v, float_range(*lo, *hi)?)),
                ScalarSlice::F64(v) => Some(Prepared::F64(v, float_range(*lo, *hi)?)),
                ScalarSlice::Utf8(_) => None,
            },
        }
    }
}

/// The codes a category's value list names, at the column's width. A code the width cannot hold,
/// and code 0, name no row and are dropped here rather than costing a comparison per row.
fn code_test<T: Copy + Ord + TryFrom<u32>>(codes: &[u32]) -> Option<IntTest<T>> {
    let mut w: Vec<T> = codes
        .iter()
        .filter(|code| **code != 0)
        .filter_map(|code| T::try_from(*code).ok())
        .collect();
    w.sort_unstable();
    w.dedup();
    set_test(w)
}

/// A category's non-sentinel codes, as a bound rather than a set.
fn above_zero<T: TryFrom<u32>>() -> IntTest<T> {
    IntTest::Range {
        lo: T::try_from(1).ok(),
        hi: None,
    }
}

/// The needles a numeric `in` names, at the column's width: one the type cannot hold, or a
/// fractional needle against an integer column, is dropped here.
fn int_in<T: Copy + Ord + TryFrom<i128>>(needles: &[Scalar]) -> Option<IntTest<T>> {
    let mut w: Vec<T> = needles
        .iter()
        .filter_map(|n| match n {
            Scalar::Int(i) => T::try_from(*i).ok(),
            Scalar::Float(_) => None,
        })
        .collect();
    w.sort_unstable();
    w.dedup();
    set_test(w)
}

/// A sorted set as the cheapest test that answers it.
fn set_test<T: Copy>(w: Vec<T>) -> Option<IntTest<T>> {
    match w.len() {
        0 => None,
        1 => Some(IntTest::Eq(w[0])),
        _ => Some(IntTest::In(w)),
    }
}

fn float_in(needles: &[Scalar]) -> Option<FloatTest> {
    let w: Vec<f64> = needles.iter().map(|n| as_f64(*n)).collect();
    (!w.is_empty()).then_some(FloatTest::In(w))
}

/// Both bounds as inclusive native values. `None` is the unsatisfiable range, so the segment is
/// not scanned.
fn int_range<T: TryFrom<i128> + NativeBound>(
    lo: Option<Endpoint>,
    hi: Option<Endpoint>,
) -> Option<IntTest<T>> {
    let narrow = |n| match n {
        Narrowed::Unsatisfiable => None,
        Narrowed::Unbounded => Some(None),
        Narrowed::At(x) => Some(Some(x)),
    };
    Some(IntTest::Range {
        lo: narrow(narrow_lo::<T>(lo))?,
        hi: narrow(narrow_hi::<T>(hi))?,
    })
}

fn float_range(lo: Option<Endpoint>, hi: Option<Endpoint>) -> Option<FloatTest> {
    let bound = |e: Option<Endpoint>| e.map(|e| (as_f64(e.value), e.inclusive));
    let (lo, hi) = (bound(lo), bound(hi));
    // A NaN bound is satisfied by nothing, which is what `narrow_*` answers for the integer widths.
    if [lo, hi].iter().flatten().any(|(b, _)| b.is_nan()) {
        return None;
    }
    Some(FloatTest::Range { lo, hi })
}

/// One contiguous run of rows inside one segment: the rows that match the predicate and carry a
/// value. The presence bitmap (`present`, shifted into view row space by [`scan_rows`]) is
/// intersected once per run, outside the row loop.
#[inline]
fn scan_run(
    prepared: &Prepared<'_>,
    base: u32,
    run: Range<u32>,
    present: Option<&croaring::Bitmap>,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    match present {
        None => match_run(prepared, base, run, rows, buf),
        Some(present) => {
            let mut matched = croaring::Bitmap::new();
            match_run(prepared, base, run, &mut matched, buf);
            matched.and_inplace(present);
            rows.or_inplace(&matched);
        }
    }
}

/// One contiguous run of rows, tested against the prepared matcher alone — presence is
/// [`scan_run`]'s. Deciding the stored width once per segment and the test once per run, rather
/// than redeciding both per row, measured 0.48–0.73 ns per row against 2.5–3.4 ns for a loop that
/// redecides them. `buf` is empty on entry and on return; its allocation is reused across the
/// runs of a chunk, never to carry rows between them.
fn match_run(
    prepared: &Prepared<'_>,
    base: u32,
    run: Range<u32>,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    let span = (run.start - base) as usize..(run.end - base) as usize;
    let first_row = run.start;
    match prepared {
        Prepared::EveryRow => {
            rows.add_range(run);
            return;
        }
        // Arrow does not store a bool as a flat slice; the run's bits are taken once and read
        // forward, then tested as the integer widths are.
        Prepared::Bool(a, test) => {
            let bits = a.values().slice(span.start, span.len());
            int_run(bits.iter().map(u8::from), first_row, test, rows, buf)
        }
        Prepared::U8(v, test) => int_run(values_of(v, span), first_row, test, rows, buf),
        Prepared::U16(v, test) => int_run(values_of(v, span), first_row, test, rows, buf),
        Prepared::U32(v, test) => int_run(values_of(v, span), first_row, test, rows, buf),
        Prepared::U64(v, test) => int_run(values_of(v, span), first_row, test, rows, buf),
        Prepared::I8(v, test) => int_run(values_of(v, span), first_row, test, rows, buf),
        Prepared::I16(v, test) => int_run(values_of(v, span), first_row, test, rows, buf),
        Prepared::I32(v, test) => int_run(values_of(v, span), first_row, test, rows, buf),
        Prepared::I64(v, test) => int_run(values_of(v, span), first_row, test, rows, buf),
        Prepared::F32(v, test) => float_run(values_of(v, span), first_row, test, rows, buf),
        Prepared::F64(v, test) => float_run(values_of(v, span), first_row, test, rows, buf),
    }
    rows.add_many(buf);
    buf.clear();
}

/// One run of a flat slice, as the values themselves.
#[inline]
fn values_of<T: Copy>(values: &[T], span: Range<usize>) -> impl Iterator<Item = T> + '_ {
    values[span].iter().copied()
}

/// One integer run: the test decided once, then a native compare per value.
#[inline]
fn int_run<T: Copy + Ord>(
    values: impl Iterator<Item = T>,
    first_row: u32,
    test: &IntTest<T>,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    match test {
        IntTest::Eq(needle) => run_matching(values, first_row, rows, buf, |x| x == *needle),
        IntTest::In(w) => {
            run_matching(values, first_row, rows, buf, |x| w.binary_search(&x).is_ok())
        }
        IntTest::Range { lo, hi } => {
            let (lo, hi) = (*lo, *hi);
            run_matching(values, first_row, rows, buf, move |x| {
                lo.is_none_or(|b| x >= b) && hi.is_none_or(|b| x <= b)
            })
        }
    }
}

/// One float run, widened to `f64` for the comparison, as the entity route does.
#[inline]
fn float_run<T: Copy + Into<f64>>(
    values: impl Iterator<Item = T>,
    first_row: u32,
    test: &FloatTest,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    match test {
        FloatTest::In(w) => run_matching(values, first_row, rows, buf, |x| w.contains(&x.into())),
        FloatTest::Range { lo, hi } => {
            let (lo, hi) = (*lo, *hi);
            run_matching(values, first_row, rows, buf, move |x| {
                let x: f64 = x.into();
                lo.is_none_or(|(b, inc)| if inc { x >= b } else { x > b })
                    && hi.is_none_or(|(b, inc)| if inc { x <= b } else { x < b })
            })
        }
    }
}

/// The monomorphic inner loop every arm above resolves to: one run of values, one test, one
/// buffered flush.
#[inline]
fn run_matching<T: Copy>(
    values: impl Iterator<Item = T>,
    first_row: u32,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
    matches: impl Fn(T) -> bool,
) {
    for (offset, value) in values.enumerate() {
        if matches(value) {
            buf.push(first_row + offset as u32);
            if buf.len() == 1024 {
                rows.add_many(buf);
                buf.clear();
            }
        }
    }
}

/// One segment's share of a row-space leaf: where its rows begin, the prepared matcher over the
/// column's values, and which of those rows carry one.
struct ScannedSegment<'a> {
    row_base: u32,
    /// `None` where no row of this segment can match. The scan skips the segment rather than
    /// rediscovering that once per run.
    values: Option<Prepared<'a>>,
    /// The rows that carry a value, in view row space. `None` where every row does.
    present: Option<croaring::Bitmap>,
}

/// Test every row of `domain` against `column`'s hot values — the render-column scan, parallel
/// over the domain, chunked exactly as the per-tile crossing is. A segment that holds the column
/// at a type other than a fixed width is a malformed bundle, refused. A segment whose schema does
/// not hold the column matches nothing, answered from the schema, never from a blob read.
fn scan_rows(
    segments: &[(&SegmentData, u32)],
    domain: &[Range<u32>],
    column: &str,
    predicate: RowPredicate<'_>,
) -> Result<croaring::Bitmap> {
    // Per-segment slices and presence, resolved once. `segments` is ascending by `row_base`,
    // which the per-row resolution below relies on.
    let slices: Vec<ScannedSegment<'_>> = segments
        .iter()
        .map(|&(segment, row_base)| {
            let values = match segment.columns.scalar(column) {
                // The segment predates the column's declaration: no row of it carries a value.
                None => None,
                // `utf8`: the schema refuses `render` on a string, so the segment and the
                // manifest disagree about the tail.
                Some(ScalarSlice::Utf8(_)) => {
                    return Err(EngineError::Malformed(format!(
                        "a segment of this view holds rendered column '{column}' at a type other \
                         than a fixed width, which the routed filter requires; the manifest and \
                         the segment disagree about the tail"
                    )))
                }
                Some(slice) => Prepared::of(&slice, &predicate),
            };
            // Only for a family that stores absence beside the column, and only where there is a
            // scan to narrow. A category's absence is a code in the column itself and it has no
            // bitmap at all.
            let present = (values.is_some() && predicate.reads_presence())
                .then(|| present_rows(segment, column, row_base))
                .flatten();
            Ok(ScannedSegment {
                row_base,
                values,
                present,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let chunks = domain_chunks(domain, domain.iter().map(|r| r.len() as u64).sum());
    let parts: Vec<croaring::Bitmap> = chunks
        .par_iter()
        .map(|chunk| {
            let mut rows = croaring::Bitmap::new();
            let mut buf: Vec<u32> = Vec::with_capacity(1024);
            // The segment owning `chunk.start`, advanced as the walk crosses a boundary: a merged
            // range can span two adjacent segments even though no domain range spans one.
            let (mut seg, _) = segment_holding(segments, chunk.start)
                .expect("the first segment's rows begin at 0");
            let mut row = chunk.start;
            while row < chunk.end {
                while seg + 1 < slices.len() && slices[seg + 1].row_base <= row {
                    seg += 1;
                }
                let seg_end = slices
                    .get(seg + 1)
                    .map_or(chunk.end, |next| next.row_base.min(chunk.end));
                let segment = &slices[seg];
                if let Some(values) = &segment.values {
                    scan_run(
                        values,
                        segment.row_base,
                        row..seg_end,
                        segment.present.as_ref(),
                        &mut rows,
                        &mut buf,
                    );
                }
                row = seg_end;
            }
            rows
        })
        .collect();
    let refs: Vec<&croaring::Bitmap> = parts.iter().collect();
    Ok(croaring::Bitmap::fast_or(&refs))
}

/// The rows of one segment that carry a value for `column`, in view row space — `None` where
/// every row does. `ColumnsRef::presence` answers a column with no file, or an unknown name, with
/// an all-present bitmap, so there is no way to read a missing artefact as an absence.
fn present_rows(segment: &SegmentData, column: &str, row_base: u32) -> Option<croaring::Bitmap> {
    segment
        .columns
        .presence(column)
        .bitmap()
        .map(|rows| rows.add_offset(i64::from(row_base)))
}

/// How many times larger than the viewport a filter result must be before the per-tile crossing is
/// taken instead of projecting. Measured crossover is 1× the viewport's rows for a result
/// contiguous in entity space and 3–5× for a scattered one, and a real result is scattered:
/// entity ids are uncorrelated with any attribute. `Engine::filter_crossing_routes` is the
/// observable that would catch this ratio drifting wrong.
const PER_TILE_CROSSING_RATIO: u64 = 3;

/// Split a chunk of the crossing domain no smaller than this, so a viewport small enough that the
/// fan-out costs more than the walk does not pay for one. 4,096 rows is comfortably above rayon's
/// own per-task cost, and small enough that a realistic viewport still splits hundreds of ways.
const CROSSING_CHUNK_MIN_ROWS: u32 = 4096;

/// The view-space rows a request's tiles span: every tile part shifted into view row space by its
/// segment's `row_base`, sorted, and merged where adjacent, so the domain is never widened.
pub(crate) fn crossing_domain(ranges: &[Vec<(usize, Range<u32>)>], row_bases: &[u32]) -> Vec<Range<u32>> {
    let mut spans: Vec<Range<u32>> = ranges
        .iter()
        .flat_map(|parts| parts.iter())
        .map(|(s, r)| row_bases[*s] + r.start..row_bases[*s] + r.end)
        .collect();
    spans.sort_unstable_by_key(|r| r.start);
    let mut merged: Vec<Range<u32>> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// The vocabulary a predicate column's values are named by, or `None` where the column has none,
/// in which case an artifact's key is the value's own canonical decimal spelling — also `None`
/// for a layer whose membership is not an attribute predicate at all.
pub(crate) fn predicate_vocabulary<'a>(
    generation: &'a crate::Generation,
    declaration: &tessera_types::layer::LayerDeclaration,
) -> Option<&'a tessera_store::vocabulary::VocabularyMinter> {
    let tessera_types::layer::MembershipSource::Attribute(field) = &declaration.membership else {
        return None;
    };
    let name = generation
        .bundle
        .manifest
        .declared_scalars
        .iter()
        .find(|scalar| &scalar.name == field)?
        .vocabulary
        .as_deref()?;
    generation.vocabularies.get(name)
}

/// Where a predicate layer's membership comes from, for one request against one generation.
/// `None` for an enumerated layer, and for a predicate layer whose rule cannot be evaluated at
/// all — a column this generation does not hold, or a spatial layer that declares no shape.
/// Fail-closed: such a level is served with no membership, rather than every artifact being one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn predicate_source<'a>(
    declaration: &tessera_types::layer::LayerDeclaration,
    generation: &'a crate::Generation,
    view: &str,
    view_data: &tessera_store::read::ViewData,
    segments: &'a [(&'a tessera_store::read::SegmentData, u32)],
    code_of_key: &'a dyn Fn(&str) -> Option<u32>,
    shapes: &crate::shapes::ShapeStore,
    store: &tessera_lifecycle::membership::ArtifactStore,
    level: u32,
) -> Option<crate::artifacts::PredicateSource<'a>> {
    match &declaration.membership {
        tessera_types::layer::MembershipSource::Enumerated => None,
        tessera_types::layer::MembershipSource::Attribute(field) => {
            let values = generation.filter_columns.value_layers(field)?;
            Some(crate::artifacts::PredicateSource::Attribute(
                crate::artifacts::AttributeSource {
                    values,
                    code_of_key,
                },
            ))
        }
        // A spatial layer with no `shape` holds no artifacts and has nothing to resolve.
        tessera_types::layer::MembershipSource::Spatial => {
            declaration.shape?;
            let held = shapes.level(
                view,
                &declaration.name,
                level,
                store,
                &crate::shapes::PersistedPieces::none(),
            );
            Some(crate::artifacts::PredicateSource::Spatial(
                crate::artifacts::SpatialSource {
                    level: held,
                    segments,
                    total_rows: u32::try_from(view_data.row_space.total_rows()).unwrap_or(u32::MAX),
                },
            ))
        }
    }
}

impl Engine {
    /// The request's filter and highlight, evaluated and crossed into the mask — after the tile
    /// ranges, before the sweep. Everything before this call stays blind to the filter: θ's anchor
    /// and the masked counts already computed ignore it, or an artifact would appear and vanish,
    /// or density would shift, as a viewer typed. `filters` and `highlight` are evaluated
    /// together against the same candidate and the same pre-filter mask — a highlight is a
    /// conjunction with the filter's candidate by construction, and evaluating its leaves against
    /// an already-filtered mask would make the two positions of one clause mean different things.
    /// Only `with_filter`'s result narrows what is drawn.
    pub(super) fn narrow_to_filters(
        &self,
        served: &ServedView<'_>,
        mask: EffectiveMask,
        tiling: &Tiling,
        v_total: u64,
        req: &ViewportRequest<'_>,
        probe: &mut Probe,
    ) -> Result<(EffectiveMask, Option<crate::region::RegionVerdict>)> {
        if req.filter.is_none() && req.highlight.is_none() {
            return Ok((mask, None));
        }
        check_cancelled(&req.cancel)?;
        let rows_in_ranges = tiling.rows_in_ranges;
        let row_bases: Vec<u32> = served.segments.iter().map(|&(_, base)| base).collect();
        let domain = crossing_domain(&tiling.ranges, &row_bases);
        let mut region_verdict: Option<crate::region::RegionVerdict> = None;
        let (filter_rows, highlight_rows) =
            self.route_filters(served, &mask, &req.cancel, |route| {
                // `count_matched` excludes the highlight from the `filter_matched` probe: its
                // cardinality is not the filter's.
                let mut evaluate = |expr: &crate::filter::FilterExpr,
                                    per_tile_only: bool,
                                    count_matched: bool|
                 -> Result<(FilterRows, Option<crate::region::RegionVerdict>)> {
                    let routed = route(expr, rows_in_ranges <= v_total)?;
                    probe.lap(|t| &mut t.filter_eval_ns);
                    let out = self.rows_of_routed(
                        served,
                        routed,
                        &domain,
                        rows_in_ranges,
                        per_tile_only,
                    )?;
                    if let (true, Some(matched)) = (count_matched, out.unmasked) {
                        probe.count(|t| &mut t.filter_matched, matched);
                    }
                    probe.lap(|t| &mut t.filter_cross_ns);
                    Ok((out.rows, out.region))
                };
                let filter_rows = match &req.filter {
                    None => None,
                    Some(expr) => {
                        let (rows, verdict) = evaluate(expr, false, true)?;
                        region_verdict = verdict;
                        Some(rows)
                    }
                };
                // The highlight always takes the per-tile walk: its three answers are all inside
                // the request's tiles, so the whole-view projection would be paid for nothing.
                let highlight_rows = match &req.highlight {
                    None => None,
                    Some(expr) => {
                        let (rows, verdict) = evaluate(expr, true, false)?;
                        // The coarsest of the two: a cover anywhere makes the response's verdict
                        // a cover.
                        region_verdict =
                            crate::region::RegionVerdict::coarsest(region_verdict, verdict);
                        Some(rows)
                    }
                };
                Ok((filter_rows, highlight_rows))
            })?;
        let mask = match filter_rows {
            Some(rows) => mask.with_filter(rows),
            None => mask,
        };
        let mask = match highlight_rows {
            Some(rows) => mask.with_highlight(rows),
            None => mask,
        };
        Ok((mask, region_verdict))
    }

    /// Route this request's filter expressions against its pre-filter mask: bring the fragment
    /// forward, build the entity-space candidate, close the region and `member_of` resolvers over
    /// `mask`, and hand `body` a `route` that puts one expression through them. The fragment is
    /// brought forward rather than read off the session, whose own fragment is fixed at
    /// authorise: composing against the stale one would silently omit every entity flushed since,
    /// under-reporting in a way indistinguishable from a correct answer. `mask` is the pre-filter
    /// mask and both resolvers close over it, so a highlight routed against an already-filtered
    /// mask would make the two positions of one clause mean different things. What `body` does
    /// with a [`crate::filter::RoutedFilter`] is the caller's.
    pub(crate) fn route_filters<T>(
        &self,
        served: &ServedView<'_>,
        mask: &EffectiveMask,
        cancel: &Option<CancelToken>,
        body: impl FnOnce(
            &dyn Fn(&crate::filter::FilterExpr, bool) -> Result<crate::filter::RoutedFilter>,
        ) -> Result<T>,
    ) -> Result<T> {
        let candidate = self.filter_candidate(served.session, served.generation)?;
        self.route_filters_under(served, mask, &candidate, cancel, body)
    }

    /// The entity-space set a request's filter is evaluated under: the session's fragment brought
    /// forward to `generation`, with the deny state and the passing buffered entities composed in.
    pub(crate) fn filter_candidate(
        &self,
        session: &Session,
        generation: &Generation,
    ) -> Result<croaring::Bitmap> {
        let fragment = self.fragment_for(session, generation)?;
        Ok(crate::filter::candidate(
            &fragment,
            session.satisfied(),
            &generation.overlay,
            &generation.buffer,
        ))
    }

    /// [`Self::route_filters`] under a candidate the caller composed: a subset of
    /// [`Self::filter_candidate`]'s set, never wider, since every scan returns a subset of it.
    pub(crate) fn route_filters_under<T>(
        &self,
        served: &ServedView<'_>,
        mask: &EffectiveMask,
        candidate: &croaring::Bitmap,
        cancel: &Option<CancelToken>,
        body: impl FnOnce(
            &dyn Fn(&crate::filter::FilterExpr, bool) -> Result<crate::filter::RoutedFilter>,
        ) -> Result<T>,
    ) -> Result<T> {
        let regions =
            |leaf: &crate::filter::RegionLeaf| self.resolve_region(leaf, served, mask, cancel);
        let members =
            |leaf: &crate::filter::MemberOfLeaf| self.resolve_member_of(leaf, served, mask);
        let layers = |layer: &str| self.reaches_layer(served.session, served.generation, layer);
        let resolvers = crate::filter::RowLeafResolvers {
            regions: &regions,
            members: &members,
            layers: &layers,
        };
        body(&|expr: &crate::filter::FilterExpr, prefer_row: bool| {
            served
                .generation
                .filter_columns
                .evaluate_routed(expr, candidate, prefer_row, &resolvers)
                .map_err(filter_refusal)
        })
    }

    /// Whether `session` reaches `layer`: whether a `member_of` may name it.
    pub(crate) fn reaches_layer(
        &self,
        session: &Session,
        generation: &Generation,
        layer: &str,
    ) -> bool {
        self.write
            .live()
            .resolve_layers(
                |term| session.satisfied().contains(&term),
                |label| generation.dict.lookup(label.as_bytes()),
            )
            .contains(layer)
    }

    /// Answer one region leaf for one request. A drawn shape: its decomposition from the
    /// generation-keyed cache — shared across principals, so it carries no authorisation — with
    /// the boundary rows tested under this request's composed mask. A published shape: the
    /// artifact's held membership, only where this principal would be served the artifact;
    /// otherwise the empty operand, identically for every reason an artifact may be withheld,
    /// including drawing an authored shape, which is content and not a membership.
    pub(crate) fn resolve_region(
        &self,
        leaf: &crate::filter::RegionLeaf,
        served: &ServedView<'_>,
        mask: &EffectiveMask,
        cancel: &Option<CancelToken>,
    ) -> std::result::Result<crate::region::RegionRows, crate::filter::FilterError> {
        use crate::filter::{FilterError, RegionLeaf};
        let segments = &served.segments[..];
        let never_cancelled = CancelToken::new();
        let cancel = cancel.as_ref().unwrap_or(&never_cancelled);
        use crate::region::{digest_of, RegionDecomposition, RegionKey, RegionRows, RegionVerdict};
        match leaf {
            RegionLeaf::Shape(shape) => {
                let max_cells = self.max_region_cells.load(Ordering::Relaxed) as usize;
                let canonical = shape.encode();
                let key = RegionKey {
                    view: served.name.to_string(),
                    prefix: served.generation.prefix.clone(),
                    segments_version: served.generation.segments_version,
                    digest: digest_of(&canonical),
                    max_cells,
                };
                let build = || RegionDecomposition::build(Arc::clone(shape), max_cells, segments);
                let entry = match self
                    .region_cache
                    .get_or_derive_waiting(key, None, cancel, |_| build())
                {
                    // A digest collision is detected here and answered from a fresh, unretained
                    // decomposition.
                    Ok(entry) if entry.is_of(&canonical) => entry,
                    Ok(_) => Arc::new(build()),
                    Err(tessera_cache::WaitEnded::Cancelled) => {
                        return Err(FilterError::RegionUnavailable(
                            "the request was cancelled while its region was being decomposed"
                                .to_string(),
                        ))
                    }
                    // Building unretained rather than refusing: a second viewer's identical lasso
                    // must not 429.
                    Err(tessera_cache::WaitEnded::Budget) => Arc::new(build()),
                };
                Ok(RegionRows {
                    rows: entry.rows_under(mask, segments),
                    verdict: entry.verdict(),
                })
            }
            RegionLeaf::Artifact(id) => {
                let gated = self
                    .gated_artifact(served, mask, *id)
                    .map_err(|e| FilterError::RegionUnavailable(e.to_string()))?;
                let rows = match gated {
                    Some(gated)
                        if gated.layer.declaration.drawn_shape()
                            != Some(tessera_types::layer::DrawnShape::Authored) =>
                    {
                        // `membership ∩ M_auth`, from whichever half of the form holds it — see
                        // [`crate::artifacts::ArtifactRows::visible_rows`].
                        gated.rows.visible_rows(gated.ordinal, mask)
                    }
                    _ => croaring::Bitmap::new(),
                };
                Ok(RegionRows {
                    rows,
                    verdict: RegionVerdict::Exact,
                })
            }
        }
    }

    /// Answer one `member_of` leaf for one request, whose layer the filter's admission has already
    /// found this principal reaches. The artifact must pass its own existence criterion through
    /// the same [`Engine::gated_artifact`] the drill-down calls. An artifact that does
    /// not pass — names nothing, is of another layer, is suppressed, is below the criterion — is
    /// the empty operand, one answer for every reason: refusing instead would make the leaf an
    /// existence oracle over what the criterion withholds. The membership is read two ways,
    /// decided by the level's layout: artifact-major intersects the held row bitmap with the
    /// composed mask; row-major scans visible rows comparing labels. Either way the answer is
    /// `membership ∩ M_auth`.
    pub(crate) fn resolve_member_of(
        &self,
        leaf: &crate::filter::MemberOfLeaf,
        served: &ServedView<'_>,
        mask: &EffectiveMask,
    ) -> std::result::Result<croaring::Bitmap, crate::filter::FilterError> {
        use crate::filter::FilterError;
        let gated = self
            .gated_artifact(served, mask, leaf.artifact)
            .map_err(|e| FilterError::MemberOfUnavailable(e.to_string()))?;
        // An identifier of another layer is answered exactly as one that resolves to nothing.
        let Some(gated) = gated.filter(|g| g.name == leaf.layer) else {
            return Ok(croaring::Bitmap::new());
        };
        // The counter says which levels answer `member_of` at the column's cost rather than the
        // bitmap's.
        if !gated.rows.membership().rows_held() {
            self.counters.member_of_column_walks.fetch_add(1, Ordering::Relaxed);
        }
        Ok(gated.rows.visible_rows(gated.ordinal, mask))
    }
}

/// A filter's refusal as the engine's: the caller's fault or the deployment's, as [`FilterError`]
/// decides at its variants.
///
/// [`FilterError`]: crate::filter::FilterError
pub(crate) fn filter_refusal(e: crate::filter::FilterError) -> EngineError {
    let detail = e.to_string();
    if e.is_callers_fault() {
        EngineError::FilterMalformed(detail)
    } else {
        EngineError::FilterRefused(detail)
    }
}

/// Test every row of `domain` against `entities`, giving the rows that matched. `None` where the
/// row space declined to invert a row. Parallel over the domain, on the engine's own pool, so a
/// serial walk here cannot move the crossover against the route it competes with. Chunks are cut
/// by row count, so neither one huge range nor a thousand slivers defeats the split.
fn per_tile_crossing(
    row_space: &tessera_store::permutation::RowSpace,
    entities: &croaring::Bitmap,
    domain: &[Range<u32>],
    rows_in_ranges: u64,
) -> Option<croaring::Bitmap> {
    per_tile_crossing_multi(row_space, &[entities], domain, rows_in_ranges)
        .map(|mut images| images.pop().expect("one set in, one image out"))
}

/// [`per_tile_crossing`] over several entity sets at once: one walk, one `entity_of` per row.
/// Returns one row image per input set, positionally. `None` where the row space declined to
/// invert a row.
fn per_tile_crossing_multi(
    row_space: &tessera_store::permutation::RowSpace,
    entity_sets: &[&croaring::Bitmap],
    domain: &[Range<u32>],
    rows_in_ranges: u64,
) -> Option<Vec<croaring::Bitmap>> {
    let chunks = domain_chunks(domain, rows_in_ranges);

    let parts: Option<Vec<Vec<croaring::Bitmap>>> = chunks
        .par_iter()
        .map(|chunk| {
            // Buffered so `add_many` appends a sorted run, where a per-row `add` re-locates it.
            let mut rows: Vec<croaring::Bitmap> = entity_sets
                .iter()
                .map(|_| croaring::Bitmap::new())
                .collect();
            let mut bufs: Vec<Vec<u32>> = entity_sets
                .iter()
                .map(|_| Vec::with_capacity(1024))
                .collect();
            for row in chunk.clone() {
                let entity = row_space.entity_of(RowId::new(row))?;
                let raw = entity.raw() as u32;
                for (i, set) in entity_sets.iter().enumerate() {
                    if set.contains(raw) {
                        bufs[i].push(row);
                        if bufs[i].len() == 1024 {
                            rows[i].add_many(&bufs[i]);
                            bufs[i].clear();
                        }
                    }
                }
            }
            for (image, buf) in rows.iter_mut().zip(&bufs) {
                image.add_many(buf);
            }
            Some(rows)
        })
        .collect();

    let parts = parts?;
    let images = (0..entity_sets.len())
        .map(|i| {
            let refs: Vec<&croaring::Bitmap> = parts.iter().map(|p| &p[i]).collect();
            croaring::Bitmap::fast_or(&refs)
        })
        .collect();
    Some(images)
}

/// Cut `domain` into parallel chunks by row count, shared by the crossing walk and the
/// render-column scan so the two fan out identically.
fn domain_chunks(domain: &[Range<u32>], rows_in_ranges: u64) -> Vec<Range<u32>> {
    let threads = rayon::current_num_threads().max(1) as u64;
    let target = (rows_in_ranges / (threads * 8))
        .max(CROSSING_CHUNK_MIN_ROWS as u64)
        .min(u32::MAX as u64) as u32;
    domain
        .iter()
        .flat_map(|range| {
            (range.start..range.end)
                .step_by(target as usize)
                .map(move |start| start..range.end.min(start.saturating_add(target)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row space over a non-identity row order, with its `row-entity.u32` attached. An identity
    /// order would let a route that returned the row back as the entity pass.
    fn row_space_over(
        dir: &std::path::Path,
        row_order: &[u32],
    ) -> tessera_store::permutation::RowSpace {
        use tessera_store::permutation::{Permutation, RowSpace};
        use tessera_store::row_entity::{write_row_entity, RowToEntity, ROW_ENTITY_FILE};

        let perm_path = dir.join("permutation.bin");
        let entities: Vec<EntityId> = row_order.iter().map(|&e| EntityId::new(e as u64)).collect();
        tessera_store::write::write_permutation(&perm_path, &entities, row_order.len() as u64)
            .expect("permutation writes");
        let table_path = dir.join(ROW_ENTITY_FILE);
        write_row_entity(&table_path, row_order).expect("table writes");

        RowSpace::new(
            Arc::new(Permutation::load(&perm_path).expect("permutation loads")),
            row_order.len() as u32,
        )
        .with_row_entity(Arc::new(
            RowToEntity::load(&table_path).expect("table loads"),
        ))
    }

    /// Testing the viewport's rows one at a time and projecting the whole result give the same
    /// set over every range the request can ask about.
    #[test]
    fn filter_routes_agree_over_the_domain() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 20,011 is coprime with the row count, so the order is a genuine shuffle.
        let rows = 40_000u32;
        let row_order: Vec<u32> = (0..rows)
            .map(|r| (r as u64 * 20_011 % rows as u64) as u32)
            .collect();
        let space = row_space_over(dir.path(), &row_order);

        // Every seventh entity, plus a dense block — neither uniform nor one run.
        let mut entities = croaring::Bitmap::new();
        entities.add_many(&(0..rows).step_by(7).collect::<Vec<u32>>());
        entities.add_range(1_000u32..9_000);

        let domain = vec![0u32..12_345, 20_000..20_003, 30_000..40_000];
        let mut domain_rows = croaring::Bitmap::new();
        for range in &domain {
            domain_rows.add_range(range.clone());
        }

        let per_tile = per_tile_crossing(&space, &entities, &domain, domain_rows.cardinality())
            .expect("a row space with a table can always invert");
        let projected = space.project(&entities);

        assert_eq!(
            per_tile,
            projected.and(&domain_rows),
            "the per-tile crossing and the projection disagree inside the domain"
        );
        // The per-tile route claims nothing outside the domain.
        assert!(
            per_tile.andnot(&domain_rows).is_empty(),
            "the per-tile crossing returned rows it never tested"
        );
        assert!(
            !projected.andnot(&domain_rows).is_empty(),
            "the fixture is degenerate: every matching row is inside the domain, so the two routes \
             would agree even if the domain were ignored"
        );
    }

    /// A short-circuited `none_of` must still consume its skipped kids' images, or the `Entity`
    /// after it reads the wrong one — see `eval_row_expr`'s `NoneOf` arm.
    #[test]
    fn a_short_circuited_negation_still_consumes_its_skipped_images() {
        use crate::filter::{Family, FilterOperand, RowExpr};

        let skipped_image = croaring::Bitmap::from_iter(0u32..1_000);
        let wanted_image = croaring::Bitmap::from_iter([7u32, 11, 13]);
        let images = vec![skipped_image.clone(), wanted_image.clone()];

        let tree = RowExpr::AnyOf(vec![
            RowExpr::NoneOf {
                column: "band".to_string(),
                family: Family::Category,
                // Evaluated and empties the difference, so the `Entity` after it is skipped.
                kids: vec![
                    RowExpr::Leaf {
                        column: "band".to_string(),
                        family: Family::Category,
                        operand: FilterOperand::Equals(tessera_types::AttrLocalId::new(1)),
                    },
                    RowExpr::Entity(croaring::Bitmap::new()),
                ],
            },
            RowExpr::Entity(croaring::Bitmap::new()),
        ]);
        assert_eq!(
            tree.entity_verdicts().len(),
            images.len(),
            "the fixture must hand one image per Entity node, as the caller does"
        );

        // An empty domain short-circuits the negation on its first kid.
        let mut next_image = 0usize;
        let scope = RowScope::Domain(croaring::Bitmap::new());
        let out = eval_row_expr(&tree, &images, &mut next_image, &[], &[], &scope)
            .expect("an empty domain scans cleanly");

        assert_eq!(
            out, wanted_image,
            "the union answered with the skipped kid's image instead of the second clause's"
        );
        assert_eq!(
            next_image,
            images.len(),
            "every image must be consumed, or a later Entity reads the wrong one"
        );
    }

    /// A view with no `row-entity.u32` declines the per-tile route rather than reading
    /// `entity_of`'s `None` as "no entity", which would drop rows silently.
    #[test]
    fn a_row_space_without_a_table_declines_the_per_tile_route() {
        use tessera_store::permutation::{Permutation, RowSpace};

        let dir = tempfile::tempdir().expect("tempdir");
        let perm_path = dir.path().join("permutation.bin");
        let entities_in_order: Vec<EntityId> = [4u64, 2, 0, 5, 1, 3]
            .iter()
            .map(|&e| EntityId::new(e))
            .collect();
        tessera_store::write::write_permutation(&perm_path, &entities_in_order, 6)
            .expect("permutation writes");
        let space = RowSpace::new(Arc::new(Permutation::load(&perm_path).expect("loads")), 6);

        assert!(!space.can_invert());
        let mut entities = croaring::Bitmap::new();
        entities.add_many(&[0, 1, 2, 3, 4, 5]);
        let domain = vec![0u32..2, 4..6];
        assert!(
            per_tile_crossing(&space, &entities, &domain, 4).is_none(),
            "the walk must decline rather than return the rows it happened to resolve"
        );
    }

    /// The domain is the request's tile parts in view row space: shifted by each segment's
    /// `row_base`, sorted across segments, and merged where they touch; it must never widen.
    #[test]
    fn the_crossing_domain_shifts_by_row_base_and_merges_only_what_touches() {
        // Segment 0 based at row 0, segment 1 at row 1,000; the third tile splits across both.
        let ranges = vec![
            vec![(0usize, 0u32..10)],
            vec![(0usize, 10u32..25)],
            vec![(0usize, 40u32..50), (1usize, 0u32..5)],
        ];
        let domain = crossing_domain(&ranges, &[0, 1_000]);
        assert_eq!(
            domain,
            vec![0u32..25, 40..50, 1_000..1_005],
            "adjacent tiles merge, a gap survives, and segment 1's rows land at its row_base"
        );
        assert_eq!(
            domain.iter().map(|r| r.len()).sum::<usize>(),
            10 + 15 + 10 + 5,
            "merging changed how many rows the domain covers"
        );
    }

    /// Strictly greater, so a result exactly at the ratio still projects.
    #[test]
    fn the_per_tile_route_is_taken_only_past_the_ratio() {
        let looks_cheaper = |matched: u64, viewport: u64| {
            matched > viewport.saturating_mul(PER_TILE_CROSSING_RATIO)
        };
        assert!(!looks_cheaper(300_000, 300_000), "1x projects");
        assert!(
            !looks_cheaper(900_000, 300_000),
            "exactly at the ratio projects"
        );
        assert!(looks_cheaper(900_001, 300_000), "just past it does not");
        // An empty viewport: the per-tile route walks nothing and is free.
        assert!(looks_cheaper(1, 0));
        assert!(
            !looks_cheaper(0, 0),
            "nothing matched -- either route is empty"
        );
    }

    /// Every row of one run, matched and narrowed to the rows that carry a value — the leaf's
    /// sequence over one segment: prepare the matcher, then scan.
    fn run(
        slice: &ScalarSlice<'_>,
        predicate: &RowPredicate<'_>,
        present: Option<&croaring::Bitmap>,
    ) -> Vec<u32> {
        let rows_in_slice = match slice {
            ScalarSlice::Bool(a) => a.len(),
            ScalarSlice::I32(v) => v.len(),
            ScalarSlice::U8(v) => v.len(),
            ScalarSlice::F64(v) => v.len(),
            ScalarSlice::I64(v) | ScalarSlice::TimestampUs(v) => v.len(),
            _ => unreachable!("the fixtures below use these widths"),
        } as u32;
        // Nothing to prepare is nothing to scan.
        let Some(prepared) = Prepared::of(slice, predicate) else {
            return Vec::new();
        };
        let mut rows = croaring::Bitmap::new();
        let mut buf = Vec::with_capacity(1024);
        scan_run(
            &prepared,
            0,
            0..rows_in_slice,
            present,
            &mut rows,
            &mut buf,
        );
        assert!(buf.is_empty(), "a run must leave its buffer empty");
        rows.iter().collect()
    }

    /// The rows that carry a value, as [`present_rows`] hands them over.
    fn presence(absent: &[u32], rows: u32) -> croaring::Bitmap {
        let mut present = croaring::Bitmap::new();
        present.add_range(0..rows);
        for row in absent {
            present.remove(*row);
        }
        present
    }

    /// A row with no number matches no range, including one containing zero. Rows 1 and 3 carry
    /// no value and hold the stored zero; row 4 carries a genuine zero.
    #[test]
    fn an_absent_number_matches_no_range_not_even_one_containing_zero() {
        // rows:      0    1*   2    3*   4    5     (* = no value, stored as the type's zero)
        let values = [7i32, 0, -3, 0, 0, 40];
        let slice = ScalarSlice::I32(&values);
        let present = presence(&[1, 3], 6);

        let straddling_zero = RowPredicate::Range {
            lo: Some(Endpoint {
                value: Scalar::Int(-10),
                inclusive: true,
            }),
            hi: Some(Endpoint {
                value: Scalar::Int(10),
                inclusive: true,
            }),
        };
        assert_eq!(
            run(&slice, &straddling_zero, Some(&present)),
            vec![0, 2, 4],
            "a row with no number matched a range containing zero"
        );

        // A genuine zero must survive the same rule.
        let zero_only = RowPredicate::Range {
            lo: Some(Endpoint {
                value: Scalar::Int(0),
                inclusive: true,
            }),
            hi: Some(Endpoint {
                value: Scalar::Int(0),
                inclusive: true,
            }),
        };
        assert_eq!(
            run(&slice, &zero_only, Some(&present)),
            vec![4],
            "a real zero stopped matching"
        );

        // An unbounded range is "carries a value", not "every row".
        assert_eq!(
            run(
                &slice,
                &RowPredicate::Range { lo: None, hi: None },
                Some(&present)
            ),
            vec![0, 2, 4, 5]
        );
        // A needle of zero names the genuine zero only.
        assert_eq!(
            run(
                &slice,
                &RowPredicate::NumberIn(&[Scalar::Int(0), Scalar::Int(40)]),
                Some(&present)
            ),
            vec![4, 5]
        );
        // The presence half of a negation reads the bitmap alone.
        assert_eq!(
            run(&slice, &RowPredicate::ValuePresent, Some(&present)),
            vec![0, 2, 4, 5]
        );
    }

    /// A category reads absence from its own code 0 and has no bitmap at all, so the scan asks
    /// for no presence on this family.
    #[test]
    fn a_category_reads_absence_from_its_sentinel_and_asks_for_no_bitmap() {
        let codes = [1u8, 0, 2, 0, 1, 3];
        let slice = ScalarSlice::U8(&codes);
        assert!(!RowPredicate::CodeIn(&[1]).reads_presence());
        assert!(!RowPredicate::CodePresent.reads_presence());

        assert_eq!(run(&slice, &RowPredicate::CodeIn(&[1]), None), vec![0, 4]);
        assert_eq!(
            run(&slice, &RowPredicate::CodeIn(&[1, 2]), None),
            vec![0, 2, 4]
        );
        assert_eq!(
            run(&slice, &RowPredicate::CodeIn(&[0]), None),
            Vec::<u32>::new(),
            "the absent sentinel names no row, even asked for by code"
        );
        assert_eq!(
            run(&slice, &RowPredicate::CodePresent, None),
            vec![0, 2, 4, 5]
        );
    }

    /// The bounds are narrowed to the width the hot column stores, so a bound outside the type is
    /// no constraint or no match rather than a wrapped comparison, and a float column is compared
    /// as floats, with NaN unordered.
    #[test]
    fn a_range_over_the_hot_column_is_narrowed_to_the_columns_own_width() {
        let values = [0u8, 1, 2, 254, 255];
        let slice = ScalarSlice::U8(&values);
        let at = |v: i128, inclusive: bool| {
            Some(Endpoint {
                value: Scalar::Int(v),
                inclusive,
            })
        };
        let range = |lo, hi| RowPredicate::Range { lo, hi };

        assert_eq!(
            run(&slice, &range(at(1, true), at(2, true)), None),
            vec![1, 2]
        );
        assert_eq!(
            run(&slice, &range(at(-5, true), None), None),
            vec![0, 1, 2, 3, 4],
            "a bound below the floor constrains nothing"
        );
        assert_eq!(
            run(&slice, &range(at(300, true), None), None),
            Vec::<u32>::new(),
            "a bound above the ceiling excludes everything"
        );
        assert_eq!(
            run(&slice, &range(None, at(-1, true)), None),
            Vec::<u32>::new()
        );
        assert_eq!(
            run(&slice, &range(at(255, false), None), None),
            Vec::<u32>::new(),
            "`> 255` over a u8 is nothing, not everything wrapped"
        );

        let fractional = |v: f64, inclusive: bool| {
            Some(Endpoint {
                value: Scalar::Float(v),
                inclusive,
            })
        };
        // NaN satisfies nothing as a bound.
        assert_eq!(
            run(&slice, &range(fractional(f64::NAN, true), None), None),
            Vec::<u32>::new()
        );
        let floats = [1.0f64, f64::NAN, 3.0];
        assert_eq!(
            run(&ScalarSlice::F64(&floats), &range(None, None), None),
            vec![0, 1, 2],
            "an unbounded range asks only that the row carry a value"
        );
        assert_eq!(
            run(
                &ScalarSlice::F64(&floats),
                &range(at(0, true), at(4, true)),
                None
            ),
            vec![0, 2],
            "NaN is outside every bounded range"
        );
        assert_eq!(
            run(
                &ScalarSlice::F64(&floats),
                &RowPredicate::NumberIn(&[Scalar::Float(f64::NAN)]),
                None
            ),
            Vec::<u32>::new(),
            "NaN equals nothing, itself included"
        );
    }

    /// A bool and a datetime are read as the entity route stores them, so a predicate means the
    /// same thing on both routes.
    #[test]
    fn a_bool_and_a_datetime_compare_as_their_entity_space_storage_does() {
        let flags = arrow::array::BooleanArray::from(vec![true, false, true, false]);
        let slice = ScalarSlice::Bool(&flags);
        let (yes, no) = ([Scalar::Int(1)], [Scalar::Int(0)]);
        assert_eq!(run(&slice, &RowPredicate::NumberIn(&yes), None), vec![0, 2]);
        assert_eq!(run(&slice, &RowPredicate::NumberIn(&no), None), vec![1, 3]);
        // `false` is a value, not a missing one.
        assert_eq!(
            run(
                &slice,
                &RowPredicate::NumberIn(&no),
                Some(&presence(&[3], 4))
            ),
            vec![1],
            "a bool with no value matched `false`"
        );

        let micros = [1_000i64, 2_000, 3_000];
        assert_eq!(
            run(
                &ScalarSlice::TimestampUs(&micros),
                &RowPredicate::Range {
                    lo: Some(Endpoint {
                        value: Scalar::Int(2_000),
                        inclusive: true,
                    }),
                    hi: None,
                },
                None
            ),
            vec![1, 2]
        );
    }
}

