//! A filter evaluated in row space: the hot-column scan, the crossing, and the leaf resolvers.

use super::*;

impl Engine {
    /// Cross a filter's entity-space result into one view's row space, by whichever of the two
    /// routes is cheaper for this request.
    ///
    /// **Project** — [`RowSpace::project`] — crosses the whole result and costs ~20–30 ns per set
    /// bit, so it scales with *what matched*. **Per tile** walks the rows the request's own tiles
    /// span and asks each one whether its entity matched, at ~20–29 ns per row on a clumped result
    /// and ~57–106 ns on a scattered one, so it scales with *what is on screen*. Neither dominates:
    /// at a 300,000-row viewport over 10⁸ items, project is 1.2 ms against 18.3 ms at a 10⁴ result
    /// and 216 ms against 32 ms at a 10⁷ one (`probes/2026-08-11-viewport-crossing/`).
    ///
    /// **The per-tile route is what makes a mid-to-high coverage principal affordable at scale**,
    /// which is the case it exists for. Project scales with the result, so a 10⁸-match result is
    /// ~2.2 s at 10⁹ rows — outside §2.2's 0.5–1 s filter budget outright — while the per-tile route
    /// stays in tens of milliseconds however much matched. A principal seeing half the corpus and
    /// filtering to a tenth of what they see is past the crossover, not near it.
    ///
    /// The route is **latency only**: the two answers agree exactly over every range the request
    /// can ask about, which is what [`FilterRows`] carries the domain to keep true, and what
    /// `filter_routes_agree_over_the_domain` asserts. A view that published no `row-entity.u32`
    /// cannot take the per-tile route at all and silently gets the projecting one.
    ///
    /// **`per_tile_only` is the highlight's route, and it is not an optimisation**
    /// (`highlight-and-hierarchy.md` §2.1). All three of a highlight's answers — a count per tile,
    /// a bit per served point, a bit per served artifact — are inside the request's own tiles, so
    /// it never needs the whole-view form and must never pay for it: projecting a 10⁷-entity
    /// verdict is ~216 ms where the walk over a 300,000-row viewport is ~18 ms whatever the
    /// highlight matched corpus-wide. A view that cannot invert its row space still gets the
    /// projecting route, there being no other, which is the same silent fallback the measured rule
    /// takes.
    pub(super) fn cross_filter_into_row_space(
        &self,
        served: &ServedView<'_>,
        entities: &croaring::Bitmap,
        ranges: &[Vec<(usize, Range<u32>)>],
        rows_in_ranges: u64,
        per_tile_only: bool,
    ) -> FilterRows {
        let row_space = &served.data.row_space;
        let per_tile_looks_cheaper = per_tile_only
            || entities.cardinality() > rows_in_ranges.saturating_mul(PER_TILE_CROSSING_RATIO);
        if per_tile_looks_cheaper && row_space.can_invert() {
            let row_bases: Vec<u32> = served.segments.iter().map(|&(_, base)| base).collect();
            let domain = crossing_domain(ranges, &row_bases);
            // `None` is the row space declining to answer — a row it cannot invert, which
            // `can_invert` says should not happen and which is corruption if it does. Falling
            // through to the exact route is the right response either way: it costs latency and
            // nothing else, where trusting a partial answer would drop rows from the map.
            if let Some(rows) = self
                .pool
                .install(|| per_tile_crossing(row_space, entities, &domain, rows_in_ranges))
            {
                self.counters
                    .filter_crossings_per_tile
                    .fetch_add(1, Ordering::Relaxed);
                return FilterRows::Viewport { rows, domain };
            }
        }
        self.counters
            .filter_crossings_projected
            .fetch_add(1, Ordering::Relaxed);
        FilterRows::Complete(row_space.project(entities))
    }

    /// Evaluate a routed filter tree with row-space leaves over the request's own rows — the
    /// render-column route (decision 0068, records §6.2), exact over `domain` and silent outside
    /// it.
    ///
    /// **The leaves read the hot column and nothing else.** A leaf is the dense variant the
    /// placement memo prefers for its channel argument: every row of the domain is read whatever
    /// the principal may see, so the work is a function of the request's ranges and the column
    /// alone — never of the mask and never of the value sought. Code 0 is the vocabulary's real
    /// absent sentinel and matches **nothing**: not a value list containing it (an unresolvable
    /// key parses to 0 precisely so it matches no row), and not a `none_of`'s presence half.
    /// This is the row-path statement of the rule the entity path keeps via its presence bitmap —
    /// the 2026-08-11 absent-as-zero defect must not return by this route.
    ///
    /// **The composed verdict is the candidate, by construction** (records §6, review N2): the
    /// bitmap returned here still contains suppressed rows — the hot column holds them, Rule S
    /// says it must — and it narrows the request only through `EffectiveMask::with_filter`, whose
    /// every consumer intersects it with the composed mask last. The entity-space verdicts inside
    /// `tree` were evaluated under the composed candidate before they got here. The suppression
    /// differential in `tests/filtering.rs` pins both halves.
    ///
    /// **One crossing per request** (0062's composition; placement memo §2.2): every
    /// entity-space verdict in the tree is crossed in a single joint walk — or a projection per
    /// verdict when the measured rule says the result side is cheaper — and the tree then
    /// combines entirely in row space. Evaluation runs on the engine's one shared pool, split
    /// over the domain exactly as the per-tile crossing splits, which is the "existing
    /// parallelism" records §6.2 prices the coarse-zoom cell against.
    ///
    /// **The result's extent is the tree's.** A tree of region leaves and projected entity
    /// verdicts answers over the whole view and comes back [`FilterRows::Complete`]; a render
    /// leaf anywhere in it, or a per-tile crossing, bounds the answer to the request's domain
    /// and it comes back [`FilterRows::Viewport`] (selection-operand §5).
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
        // The one crossing: every entity-space verdict's row image, computed together. The route
        // between the two crossing shapes is the measured rule the single-operand path uses,
        // summed over the verdicts because that is what the projection would cost.
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
                    // A walk over the request's rows is silent outside them, whatever else the
                    // tree holds.
                    whole_view = false;
                    images
                }
                None => {
                    self.counters
                        .filter_crossings_projected
                        .fetch_add(1, Ordering::Relaxed);
                    if whole_view {
                        // Projection crosses each verdict whole, and with nothing in the tree
                        // bounded by the domain, whole is what the answer is.
                        verdicts.iter().map(|v| row_space.project(v)).collect()
                    } else {
                        // Clamped to the domain so the combined answer never claims a row
                        // outside what `FilterRows::Viewport` says was tested.
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
        // What a negated region's presence half is, and what a region's rows are clamped to
        // where the tree is domain-bounded: the whole view, or the request's own rows.
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
/// of the tree's entity-space verdicts, consumed in the same pre-order
/// [`crate::filter::RowExpr::entity_verdicts`] collects them — `next_image` is that cursor.
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
        // the tree to the request's rows (`highlight-and-hierarchy.md` §3).
        RowExpr::MemberOf(rows) => Ok(scope.clamp(rows)),
        RowExpr::NotInRows(kids) => {
            // The complement within the scope: every rowed entity carries a position and may be a
            // member, so the presence half of this negation is every row (selection-operand §5).
            // No early exit on an empty difference — the image cursor's positional rule is simpler
            // kept whole here than skipped, and these leaves' kids are already resolved.
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
            // `present ∖ matched` — the positive predicate, in row space, presence being whatever
            // this column's family stores it as: a non-sentinel code for a category, the presence
            // bitmap for every other. Either way an absent item matches no negation, and a row
            // that cannot be read under-reports rather than widening (I12's sign, exactly as the
            // entity path argues it).
            let mut out = scan_rows(segments, domain, column, RowPredicate::present_in(*family))?;
            for (i, kid) in kids.iter().enumerate() {
                out.andnot_inplace(&eval_row_expr(
                    kid, images, next_image, segments, domain, scope,
                )?);
                if out.is_empty() {
                    // Nothing below can widen an empty difference, so the remaining kids are not
                    // evaluated — **but `images` is positional and their verdicts are still in
                    // it**. `entity_verdicts` collects every `Entity` node in the tree whether or
                    // not evaluation reaches it, so leaving the cursor here would hand the next
                    // `Entity` anywhere in the tree someone else's image: a filter that silently
                    // answers with a different clause's verdict, or with the candidate itself.
                    // Reachable — a kid is normally a row leaf on this one column, but an empty
                    // combinator is entity-pure by construction and `check_negations` admits it,
                    // since it contributes no column to the one-column rule.
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

/// A row-space leaf's test against one row of the hot column.
///
/// **Absence is a per-family rule, and it is carried here rather than inferred.** A category's
/// absence is its vocabulary's reserved code 0, held in the column itself. Every other family's is
/// decision 0064's presence bitmap beside the column: the hot column is non-nullable, so an absent
/// number is written as the type's zero, which is an ordinary value — and a range containing zero
/// would otherwise match every row that has no value at all (the 2026-08-11 defect, on this route).
enum RowPredicate<'a> {
    /// A category's code is non-sentinel and in this set. An empty set matches nothing.
    CodeIn(&'a [u32]),
    /// A category's code is non-sentinel — the presence half of a negation over one.
    CodePresent,
    /// A number's value equals one of these. An empty set matches nothing.
    NumberIn(&'a [Scalar]),
    /// A number's value lies between these bounds. Either may be absent, which is an open side,
    /// and each carries its own inclusivity — [`crate::filter::FilterOperand::Range`]'s semantics,
    /// which the entity route reads the same bounds by.
    Range {
        lo: Option<Endpoint>,
        hi: Option<Endpoint>,
    },
    /// The row carries a value, whatever it is — the presence half of a negation over a column
    /// whose absence lives in the bitmap, where the stored bytes say nothing at all.
    ValuePresent,
}

impl RowPredicate<'_> {
    /// The presence half of a negation over a column of this family.
    fn present_in(family: Family) -> RowPredicate<'static> {
        match family {
            Family::Category => RowPredicate::CodePresent,
            Family::Numeric => RowPredicate::ValuePresent,
            // A string column is never row-placed, and text is not even entity-space: `render` is
            // refused on both at the schema. An empty code set is the fail-closed reading if one
            // ever arrived.
            Family::Keyword | Family::Text => RowPredicate::CodeIn(&[]),
        }
    }

    /// Does this family read absence from the presence bitmap? A category does not: its absence is
    /// a code in the column, and it has no bitmap by construction (`render_presence`'s module doc).
    fn reads_presence(&self) -> bool {
        match self {
            RowPredicate::CodeIn(_) | RowPredicate::CodePresent => false,
            RowPredicate::NumberIn(_) | RowPredicate::Range { .. } | RowPredicate::ValuePresent => {
                true
            }
        }
    }
}

/// One row-space leaf's comparands, owned for as long as the scan borrows them.
///
/// A family/operand pair the parse would have refused becomes an empty set, which matches nothing:
/// the second line of defence the entity-space scan keeps for the same reason (`filter.rs`'s
/// `codes_of`), never a panic and never a number compared against a code.
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

/// One render column's typed slice per segment — resolved once per leaf evaluation, exactly as
/// the gather resolves per segment rather than per row.
///
/// Every declarable type but `utf8`, which the schema refuses from the hot column outright. A
/// category is one of the three unsigned widths; the rest are a number, a datetime or a bool.
enum HotSlice<'a> {
    Bool(&'a arrow::array::BooleanArray),
    U8(&'a [u8]),
    U16(&'a [u16]),
    U32(&'a [u32]),
    U64(&'a [u64]),
    I8(&'a [i8]),
    I16(&'a [i16]),
    I32(&'a [i32]),
    I64(&'a [i64]),
    F32(&'a [f32]),
    F64(&'a [f64]),
    /// Microseconds since the epoch — an `i64`, compared as one, exactly as the entity route
    /// compares it.
    TimestampUs(&'a [i64]),
}

/// One contiguous run of rows inside one segment: the rows that match the predicate **and** carry
/// a value.
///
/// **The presence bitmap is intersected once per run, outside the row loop.** `present` is this
/// segment's presence for the column, already shifted into view row space by
/// [`scan_rows`], and `None` means every row carries a value — the representation an absent file
/// has, so the common column costs neither bytes nor an intersection. Testing presence per row
/// instead would put a bitmap lookup inside the loop the hoist below exists to keep flat.
#[inline]
fn scan_run(
    slice: &HotSlice<'_>,
    base: u32,
    run: Range<u32>,
    predicate: &RowPredicate<'_>,
    present: Option<&croaring::Bitmap>,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    match present {
        None => match_run(slice, base, run, predicate, rows, buf),
        Some(present) => {
            let mut matched = croaring::Bitmap::new();
            match_run(slice, base, run, predicate, &mut matched, buf);
            matched.and_inplace(present);
            rows.or_inplace(&matched);
        }
    }
}

/// One contiguous run of rows, tested against the predicate alone — presence is [`scan_run`]'s.
///
/// **The dispatch is hoisted out of the row loop, and that is the whole point of this function.**
/// The obvious shape — resolve the segment, match the stored width and match the predicate for
/// each row in turn — costs about four branches and two bounds checks per row, none of them
/// hoistable, and it measured 2.5–3.4 ns per row against the 0.48–0.73 ns a flat compare reaches
/// (`docs/evidence/memos/2026-08-12-records-and-search-epic-1-measurements.md` §2). The tell in
/// that data is that the constant was **insensitive to the code width**: a loop bound by moving
/// one or two bytes per row would not be, so the loop was bound by its own branching. Deciding the
/// width and the predicate once per run leaves a monomorphic compare over a slice, which is the
/// loop the probe measured — and a range's bounds are narrowed to the column's own type in the
/// same hoist, so no comparison widens a value.
///
/// A category's absent sentinel keeps its rule at every instantiation: code 0 matches nothing —
/// not a value list that names it, not the presence half of a negation. `run_matching` never sees
/// it: each caller below excludes it before the loop, which is the same statement made where it
/// cannot cost a comparison per row.
///
/// `buf` is empty on entry and on return. It is a parameter so its allocation is reused across the
/// runs of a chunk, never to carry rows between them: a run's matches must be complete before
/// [`scan_run`] intersects them with presence.
fn match_run(
    slice: &HotSlice<'_>,
    base: u32,
    run: Range<u32>,
    predicate: &RowPredicate<'_>,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    let span = (run.start - base) as usize..(run.end - base) as usize;
    match predicate {
        RowPredicate::CodeIn(codes) => code_run(slice, span, run.start, codes, rows, buf),
        RowPredicate::CodePresent => code_present_run(slice, span, run.start, rows, buf),
        RowPredicate::NumberIn(needles) => number_run(slice, span, run.start, needles, rows, buf),
        RowPredicate::Range { lo, hi } => range_run(slice, span, run.start, *lo, *hi, rows, buf),
        // No value is consulted: for this family the column says nothing about absence, so every
        // row of the run is present unless the bitmap [`scan_run`] intersects says otherwise.
        RowPredicate::ValuePresent => {
            rows.add_range(run);
            return;
        }
    }
    rows.add_many(buf);
    buf.clear();
}

/// A category's codes. Any slice that is not one of the three code widths matches nothing: a
/// category is stored at one of them, so anything else is a tail that disagrees with the
/// declaration, and comparing a float to a code would be worse than answering short.
#[inline]
fn code_run(
    slice: &HotSlice<'_>,
    span: Range<usize>,
    first_row: u32,
    codes: &[u32],
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    // The common shape by far — `eq`, and `in` over a single surviving code. One comparison per
    // row against a constant.
    if codes.len() == 1 {
        let needle = codes[0];
        if needle == 0 {
            return; // The sentinel names no row; the whole run is a non-match.
        }
        match slice {
            // A needle outside the column's code space matches no row, and the width test happens
            // once per run rather than once per comparison.
            HotSlice::U8(v) => {
                if let Ok(n) = u8::try_from(needle) {
                    run_matching(&v[span], first_row, rows, buf, |c| *c == n);
                }
            }
            HotSlice::U16(v) => {
                if let Ok(n) = u16::try_from(needle) {
                    run_matching(&v[span], first_row, rows, buf, |c| *c == n);
                }
            }
            HotSlice::U32(v) => run_matching(&v[span], first_row, rows, buf, |c| *c == needle),
            _ => {}
        }
        return;
    }
    match slice {
        HotSlice::U8(v) => run_matching(&v[span], first_row, rows, buf, |c| {
            *c != 0 && codes.contains(&u32::from(*c))
        }),
        HotSlice::U16(v) => run_matching(&v[span], first_row, rows, buf, |c| {
            *c != 0 && codes.contains(&u32::from(*c))
        }),
        HotSlice::U32(v) => run_matching(&v[span], first_row, rows, buf, |c| {
            *c != 0 && codes.contains(c)
        }),
        _ => {}
    }
}

/// A category carries a value: a non-sentinel code.
#[inline]
fn code_present_run(
    slice: &HotSlice<'_>,
    span: Range<usize>,
    first_row: u32,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    match slice {
        HotSlice::U8(v) => run_matching(&v[span], first_row, rows, buf, |c| *c != 0),
        HotSlice::U16(v) => run_matching(&v[span], first_row, rows, buf, |c| *c != 0),
        HotSlice::U32(v) => run_matching(&v[span], first_row, rows, buf, |c| *c != 0),
        _ => {}
    }
}

/// A number's value lies within the bounds — the row-space transcription of
/// `ValueColumn::scan_range`, and it must stay one.
///
/// **A deliberate second copy of the narrowing, across a crate boundary.** `tessera-filter`'s is
/// private to the entity-space column, and the two routes must agree exactly over the domain or
/// 0068's licence to choose a route on cost alone fails. The rules copied here are the ones that
/// are wrong in silence if they drift: an exclusive integer bound is folded by one step; a
/// fractional bound rounds *into* the constraint (`> 3.2` and `>= 3.2` both admit 4); a NaN bound
/// satisfies nothing; a bound past the type's floor or ceiling is no constraint or no match rather
/// than a wrapped comparison. `the_row_route_and_the_entity_route_agree_over_the_domain` is what
/// holds the copies together, over a numeric predicate as well as a category one.
#[inline]
fn range_run(
    slice: &HotSlice<'_>,
    span: Range<usize>,
    first_row: u32,
    lo: Option<Endpoint>,
    hi: Option<Endpoint>,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    macro_rules! int_range {
        ($v:expr, $t:ty) => {{
            let lo_b = match narrow_lo::<$t>(lo) {
                Narrowed::Unsatisfiable => return,
                Narrowed::Unbounded => None,
                Narrowed::At(x) => Some(x),
            };
            let hi_b = match narrow_hi::<$t>(hi) {
                Narrowed::Unsatisfiable => return,
                Narrowed::Unbounded => None,
                Narrowed::At(x) => Some(x),
            };
            run_matching($v, first_row, rows, buf, move |x| {
                lo_b.is_none_or(|b| *x >= b) && hi_b.is_none_or(|b| *x <= b)
            })
        }};
    }
    // Floats keep the `f64` comparison: NaN must stay unordered, and narrowing through an integer
    // would destroy that.
    macro_rules! float_range {
        ($v:expr) => {{
            let lo_f = lo.map(|e| (as_f64(e.value), e.inclusive));
            let hi_f = hi.map(|e| (as_f64(e.value), e.inclusive));
            run_matching($v, first_row, rows, buf, move |x| {
                let x = *x as f64;
                lo_f.is_none_or(|(b, inc)| if inc { x >= b } else { x > b })
                    && hi_f.is_none_or(|(b, inc)| if inc { x <= b } else { x < b })
            })
        }};
    }
    match slice {
        HotSlice::Bool(a) => {
            // A bool is compared as the 0/1 the entity route stores it as, so `>= 1` means true on
            // both — the mapping is `u8::from`, in one place on each side.
            let lo_b = match narrow_lo::<u8>(lo) {
                Narrowed::Unsatisfiable => return,
                Narrowed::Unbounded => None,
                Narrowed::At(x) => Some(x),
            };
            let hi_b = match narrow_hi::<u8>(hi) {
                Narrowed::Unsatisfiable => return,
                Narrowed::Unbounded => None,
                Narrowed::At(x) => Some(x),
            };
            bool_matching(a, span, first_row, rows, buf, move |x| {
                lo_b.is_none_or(|b| x >= b) && hi_b.is_none_or(|b| x <= b)
            })
        }
        HotSlice::U8(v) => int_range!(&v[span], u8),
        HotSlice::U16(v) => int_range!(&v[span], u16),
        HotSlice::U32(v) => int_range!(&v[span], u32),
        HotSlice::U64(v) => int_range!(&v[span], u64),
        HotSlice::I8(v) => int_range!(&v[span], i8),
        HotSlice::I16(v) => int_range!(&v[span], i16),
        HotSlice::I32(v) => int_range!(&v[span], i32),
        HotSlice::I64(v) | HotSlice::TimestampUs(v) => int_range!(&v[span], i64),
        HotSlice::F32(v) => float_range!(&v[span]),
        HotSlice::F64(v) => float_range!(&v[span]),
    }
}

/// A number's value equals one of the needles — the row-space transcription of
/// `ValueColumn::scan_num_in`, with the same rules: a needle the column's type cannot hold matches
/// nothing and is dropped before the loop rather than compared away per row, and the survivors are
/// sorted and searched because a linear `contains` costs O(needles) per row.
#[inline]
fn number_run(
    slice: &HotSlice<'_>,
    span: Range<usize>,
    first_row: u32,
    needles: &[Scalar],
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
) {
    macro_rules! int_in {
        ($v:expr, $t:ty) => {{
            let mut w: Vec<$t> = needles
                .iter()
                .filter_map(|n| match n {
                    Scalar::Int(i) => <$t>::try_from(*i).ok(),
                    Scalar::Float(_) => None,
                })
                .collect();
            if w.is_empty() {
                return;
            }
            w.sort_unstable();
            w.dedup();
            run_matching($v, first_row, rows, buf, move |x| {
                w.binary_search(x).is_ok()
            })
        }};
    }
    macro_rules! float_in {
        ($v:expr) => {{
            // NaN equals nothing, itself included — so a NaN needle matches no row, which the
            // comparison gives without a special case.
            let w: Vec<f64> = needles.iter().map(|n| as_f64(*n)).collect();
            run_matching($v, first_row, rows, buf, move |x| {
                let x = *x as f64;
                w.iter().any(|n| x == *n)
            })
        }};
    }
    match slice {
        HotSlice::Bool(a) => {
            let mut w: Vec<u8> = needles
                .iter()
                .filter_map(|n| match n {
                    Scalar::Int(i) => u8::try_from(*i).ok(),
                    Scalar::Float(_) => None,
                })
                .collect();
            if w.is_empty() {
                return;
            }
            w.sort_unstable();
            w.dedup();
            bool_matching(a, span, first_row, rows, buf, move |x| {
                w.binary_search(&x).is_ok()
            })
        }
        HotSlice::U8(v) => int_in!(&v[span], u8),
        HotSlice::U16(v) => int_in!(&v[span], u16),
        HotSlice::U32(v) => int_in!(&v[span], u32),
        HotSlice::U64(v) => int_in!(&v[span], u64),
        HotSlice::I8(v) => int_in!(&v[span], i8),
        HotSlice::I16(v) => int_in!(&v[span], i16),
        HotSlice::I32(v) => int_in!(&v[span], i32),
        HotSlice::I64(v) | HotSlice::TimestampUs(v) => int_in!(&v[span], i64),
        HotSlice::F32(v) => float_in!(&v[span]),
        HotSlice::F64(v) => float_in!(&v[span]),
    }
}

/// The monomorphic inner loop every flat-slice arm above resolves to: one slice, one test, one
/// buffered flush. Generic over the stored type so each width compiles to its own loop.
#[inline]
fn run_matching<T: Copy>(
    values: &[T],
    first_row: u32,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
    matches: impl Fn(&T) -> bool,
) {
    for (offset, code) in values.iter().enumerate() {
        if matches(code) {
            buf.push(first_row + offset as u32);
            if buf.len() == 1024 {
                rows.add_many(buf);
                buf.clear();
            }
        }
    }
}

/// [`run_matching`] for the one fixed-width type Arrow does not store as a flat slice of itself.
/// The test is hoisted exactly as the others are; what differs is only the bit extraction.
#[inline]
fn bool_matching(
    values: &arrow::array::BooleanArray,
    span: Range<usize>,
    first_row: u32,
    rows: &mut croaring::Bitmap,
    buf: &mut Vec<u32>,
    matches: impl Fn(u8) -> bool,
) {
    let start = span.start;
    for idx in span {
        if matches(u8::from(values.value(idx))) {
            buf.push(first_row + (idx - start) as u32);
            if buf.len() == 1024 {
                rows.add_many(buf);
                buf.clear();
            }
        }
    }
}

/// What a range bound becomes once narrowed to the column's own type — see [`range_run`] for why
/// this mirrors `tessera-filter`'s private original rather than calling it.
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

/// The integer widths' extremes as `i128`, so the narrowing can tell "below the floor" (no
/// constraint) from "above the ceiling" (nothing matches) without a per-type arm.
trait NativeBound {
    fn min_i128() -> i128;
    fn max_i128() -> i128;
}
macro_rules! native_bound {
    ($($t:ty),*) => { $(impl NativeBound for $t {
        fn min_i128() -> i128 { <$t>::MIN as i128 }
        fn max_i128() -> i128 { <$t>::MAX as i128 }
    })* };
}
native_bound!(u8, u16, u32, u64, i8, i16, i32, i64);

/// The lower bound as an **inclusive** native value.
fn narrow_lo<T>(e: Option<Endpoint>) -> Narrowed<T>
where
    T: TryFrom<i128> + NativeBound,
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
    T: TryFrom<i128> + NativeBound,
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

fn as_f64(s: Scalar) -> f64 {
    match s {
        Scalar::Int(i) => i as f64,
        Scalar::Float(f) => f,
    }
}

/// One segment's share of a row-space leaf: where its rows begin, the column's values, and which
/// of those rows carry one.
struct ScannedSegment<'a> {
    row_base: u32,
    /// `None` for a segment whose schema does not hold the column: none of its rows matches.
    values: Option<HotSlice<'a>>,
    /// The rows that carry a value, **in view row space** — the presence bitmap shifted by
    /// `row_base` once, here, rather than per run. `None` where every row does.
    present: Option<croaring::Bitmap>,
}

/// Test every row of `domain` against `column`'s hot values — the render-column scan, parallel
/// over the domain on the caller's installed pool, chunked exactly as the per-tile crossing is.
///
/// A segment that holds the column at a type other than a fixed width is a **malformed bundle**,
/// refused like the gather's equivalent. **A segment whose schema does not hold the column
/// matches nothing** (`ingest.md` §6.3): it was written before the column was declared at a
/// running service, so none of its rows carries a value, which is the answer for a range and for
/// a negation's presence half alike. Answered from the schema, never from a blob read.
fn scan_rows(
    segments: &[(&SegmentData, u32)],
    domain: &[Range<u32>],
    column: &str,
    predicate: RowPredicate<'_>,
) -> Result<croaring::Bitmap> {
    // Per-segment slices and presence, resolved once. `segments` is ascending by `row_base`
    // (`segments_with_row_bases` sorts), which the per-row resolution below relies on.
    let slices: Vec<ScannedSegment<'_>> = segments
        .iter()
        .map(|&(segment, row_base)| {
            let values = match segment.columns.scalar(column) {
                Some(ScalarSlice::Bool(a)) => Some(HotSlice::Bool(a)),
                Some(ScalarSlice::U8(s)) => Some(HotSlice::U8(s)),
                Some(ScalarSlice::U16(s)) => Some(HotSlice::U16(s)),
                Some(ScalarSlice::U32(s)) => Some(HotSlice::U32(s)),
                Some(ScalarSlice::U64(s)) => Some(HotSlice::U64(s)),
                Some(ScalarSlice::I8(s)) => Some(HotSlice::I8(s)),
                Some(ScalarSlice::I16(s)) => Some(HotSlice::I16(s)),
                Some(ScalarSlice::I32(s)) => Some(HotSlice::I32(s)),
                Some(ScalarSlice::I64(s)) => Some(HotSlice::I64(s)),
                Some(ScalarSlice::F32(s)) => Some(HotSlice::F32(s)),
                Some(ScalarSlice::F64(s)) => Some(HotSlice::F64(s)),
                Some(ScalarSlice::TimestampUs(s)) => Some(HotSlice::TimestampUs(s)),
                // The segment predates the column's declaration: no row of it carries a value.
                None => None,
                // `utf8`: the schema refuses `render` on a string, so the segment and the
                // manifest disagree about the tail.
                Some(_) => {
                    return Err(EngineError::Malformed(format!(
                        "a segment of this view holds rendered column '{column}' at a type other \
                         than a fixed width, which the routed filter requires; the manifest and \
                         the segment disagree about the tail"
                    )))
                }
            };
            // Only for a family that stores absence beside the column. A category's absence is a
            // code in the column itself and it has no bitmap at all, so asking for one would be
            // the sentinel-and-bitmap muddle decision 0064 declines.
            let present = predicate
                .reads_presence()
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
            // The segment owning `chunk.start`, advanced as the walk crosses a boundary — the
            // domain's ranges never span rows outside a segment, but a *merged* range can span
            // two adjacent segments.
            let mut seg = slices.partition_point(|s| s.row_base <= chunk.start) - 1;
            let mut row = chunk.start;
            while row < chunk.end {
                while seg + 1 < slices.len() && slices[seg + 1].row_base <= row {
                    seg += 1;
                }
                // The run this segment owns: to the next segment's base, or the chunk's end.
                let seg_end = slices
                    .get(seg + 1)
                    .map_or(chunk.end, |next| next.row_base.min(chunk.end));
                let segment = &slices[seg];
                if let Some(values) = &segment.values {
                    scan_run(
                        values,
                        segment.row_base,
                        row..seg_end,
                        &predicate,
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

/// The rows of one segment that carry a value for `column`, **in view row space** — `None` where
/// every row does.
///
/// `ColumnsRef::presence` answers for a column with no file, and for a name it does not know, with
/// an all-present bitmap — so there is no branch here and no way for a caller to read a missing
/// artefact as an absence. A damaged bitmap has already refused, at `ColumnsRef::load`.
///
/// The shift into view row space belongs here rather than in the scan: the bitmap is over the
/// segment's own `0..row_count` (`render_presence`'s module doc — a merge permutes rows, so it can
/// be nothing else), and shifting once per segment keeps the run loop comparing bitmaps in one
/// numbering.
fn present_rows(segment: &SegmentData, column: &str, row_base: u32) -> Option<croaring::Bitmap> {
    segment
        .columns
        .presence(column)
        .bitmap()
        .map(|rows| rows.add_offset(i64::from(row_base)))
}

/// How many times larger than the viewport a filter result must be before the per-tile crossing is
/// taken instead of projecting — the crossover of [`Engine::cross_filter_into_row_space`]'s two
/// cost curves, expressed as a ratio because that is what the measurement supports.
///
/// **Measured range 1–5, and this sits at the high end deliberately.** The crossover is 1× the
/// viewport's rows for a result contiguous in entity space and 3–5× for a scattered one
/// (`probes/2026-08-11-viewport-crossing/`), and the realistic case for an ingest-ordered column is
/// scattered: entity ids are assigned in permission-signature order and are uncorrelated with any
/// attribute. Sitting at 3 keeps the exact-everywhere route in play a little longer than the
/// contiguous case would justify, which is the cheap direction to be wrong in — the loss is
/// milliseconds either side of the crossover, while the win the route exists for is two orders of
/// magnitude out (216 ms against 32 ms at a 10⁷ result).
///
/// **Not measured: how this moves with thread count.** The probe was single-threaded and both
/// routes parallelise, each over its own axis — project over the result, the per-tile crossing over
/// the viewport — so the ratio is *modelled* to survive, not shown to.
/// `Engine::filter_crossing_routes` is the observable that would catch it being wrong in a way a
/// bench never reproduces.
const PER_TILE_CROSSING_RATIO: u64 = 3;

/// Split a chunk of the crossing domain no smaller than this, so a viewport small enough that the
/// fan-out costs more than the walk does not pay for one. 4,096 rows is ~0.1 ms of crossing work at
/// the scattered constant — comfortably above rayon's own per-task cost, and small enough that a
/// realistic viewport still splits hundreds of ways.
const CROSSING_CHUNK_MIN_ROWS: u32 = 4096;

/// The view-space rows a request's tiles span: every tile part shifted into view row space by its
/// segment's `row_base`, sorted, and merged.
///
/// **Merged, and that is not tidiness.** Adjacent tiles are adjacent Morton ranges, so merging
/// turns a few hundred separate walks into a handful of long contiguous ones — which is what makes
/// the per-tile crossing's reads of `row-entity.u32` sequential, and what lets
/// [`FilterRows::covers`] answer with one binary search. Merging `[a, b)` with `[b, c)` yields
/// exactly their union, so the domain is never widened by it.
pub(super) fn crossing_domain(ranges: &[Vec<(usize, Range<u32>)>], row_bases: &[u32]) -> Vec<Range<u32>> {
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

/// **The vocabulary a predicate column's values are named by**, or `None` where the column has
/// none — in which case an artifact's key is the value's own canonical decimal spelling
/// (`tessera_types::layer::attribute_value_key`).
///
/// `None` also for a layer whose membership is not an attribute predicate at all, which is what
/// makes the closure built from this total: it answers *no code* for every key of such a layer, and
/// no such layer is ever asked.
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

/// **Where a predicate layer's membership comes from, for one request against one generation.**
///
/// `None` for an enumerated layer, and for a predicate layer whose rule cannot be evaluated at all
/// — a column this generation does not hold, or a spatial layer that declares no shape. Both are
/// the fail-closed answer: such a level is served with no membership, so none of its artifacts is a
/// candidate anywhere, rather than every artifact being one.
///
/// A spatial level's source is its held structures (`crate::shapes`), taken at the store's current
/// level version — built at open and at every publication into the level, so a request finds them
/// held. The row form assembled from them is built once and maintained by the publications that
/// move it (`crate::artifacts`), so a request that reaches the build is one whose level nothing
/// warmed.
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
        // ⊘ A spatial layer with no `shape` holds no artifacts and has nothing to resolve — the
        // state this surface has always had, and the one the generator's boundary fixture is in.
        tessera_types::layer::MembershipSource::Spatial => {
            declaration.shape?;
            // Held already unless nothing warmed the level; nothing persisted is claimable here.
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
    /// Answer one region leaf for one request (`crate::region`; `crate::filter::RegionResolver`).
    ///
    /// A drawn shape: its decomposition from the generation-keyed cache — shared across
    /// principals, it carries no authorisation — with the boundary rows tested under **this
    /// request's composed mask**. A published shape: the artifact's held membership, whole and
    /// exact, only where this principal would be served the artifact; otherwise the empty
    /// operand, identically for every reason (`polygon-membership.md` §8). An artifact whose
    /// layer draws an authored shape is an empty operand too — its drawing is content, not a
    /// membership (§4.1).
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
                    // A hit is a hit only for these bytes: a digest collision is detected here
                    // and answered from a fresh, unretained decomposition (selection-operand §5).
                    Ok(entry) if entry.is_of(&canonical) => entry,
                    Ok(_) => Arc::new(build()),
                    Err(crate::single_flight::WaitEnded::Cancelled) => {
                        return Err(FilterError::RegionUnavailable(
                            "the request was cancelled while its region was being decomposed"
                                .to_string(),
                        ))
                    }
                    // A wait that ran out is answered by building here, unretained: the
                    // decomposition is a perimeter's worth of work, and refusing it would make a
                    // second viewer's identical lasso a 429.
                    Err(crate::single_flight::WaitEnded::Budget) => Arc::new(build()),
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
                        // [`crate::artifacts::ArtifactRows::visible_rows`]. The operand is
                        // composed with the mask wherever it is used, so narrowing it here is the
                        // same set by another route.
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

    /// Answer one `member_of` leaf for one request (`highlight-and-hierarchy.md` §3;
    /// [`crate::filter::MemberResolver`]).
    ///
    /// **The gate, then the membership, in that order and never the other.** The layer must be one
    /// this principal reaches — a name outside their own `/v1/meta` list is
    /// [`FilterError::UnknownLayer`], deployment schema, and the registry's probe answers alike for
    /// a gate-failed name and a never-registered one. Then the artifact must pass its **own**
    /// existence criterion for this principal, through the same
    /// [`Engine::gated_artifact`] the drill-down and the published-region leaf call, so that one
    /// rule has one transcription. An artifact that does not pass — one that names nothing, one of
    /// another layer, one suppressed, one below the criterion — is the **empty operand**, one
    /// answer for every reason, because a `422` there would make the leaf an existence oracle over
    /// exactly what the criterion withholds.
    ///
    /// **The membership is read two ways, decided by the level's layout and by nothing about the
    /// request** (decision 0093). Artifact-major: the held row bitmap, intersected with the
    /// composed mask — one `and`, no postings, no crossing, whatever the artifact's size. Row-
    /// major: one scan of the principal's visible rows comparing labels, which is the only route a
    /// label column has to the same set. Either way the answer is `membership ∩ M_auth`, whose
    /// cardinality is the masked count the artifacts frame already serves.
    pub(crate) fn resolve_member_of(
        &self,
        leaf: &crate::filter::MemberOfLeaf,
        served: &ServedView<'_>,
        mask: &EffectiveMask,
    ) -> std::result::Result<croaring::Bitmap, crate::filter::FilterError> {
        use crate::filter::FilterError;
        let reachable = self.write.live().resolve_layers(
            |term| served.session.satisfied().contains(&term),
            |label| served.generation.dict.lookup(label.as_bytes()),
        );
        if !reachable.contains(&leaf.layer) {
            return Err(FilterError::UnknownLayer(leaf.layer.clone()));
        }
        let gated = self
            .gated_artifact(served, mask, leaf.artifact)
            .map_err(|e| FilterError::MemberOfUnavailable(e.to_string()))?;
        // An identifier of *another* layer is a value that does not resolve within the one named,
        // and is answered exactly as one that resolves to nothing at all.
        let Some(gated) = gated.filter(|g| g.name == leaf.layer) else {
            return Ok(croaring::Bitmap::new());
        };
        // **The artifact-major membership where the form holds it, and the column walk where it
        // does not** — one call, and which route it takes is a property of the level
        // (`crate::artifacts::ArtifactRows::visible_rows`). The first is one intersection with
        // `M_auth`, O(containers touched) and independent of what the artifact matched; the second
        // is a walk of the visible rows inside the artifact's extent, reading labels off the
        // column. The two agree by construction: the column is a projection *of* that membership
        // (`crate::row_column`), and the extent is `minimum` and `maximum` over it.
        //
        // **The counter says which levels take the walk**, so a deployment can see that a level
        // is answering `member_of` at the column's cost rather than the bitmap's.
        if !gated.rows.membership().rows_held() {
            self.counters.member_of_column_walks.fetch_add(1, Ordering::Relaxed);
        }
        Ok(gated.rows.visible_rows(gated.ordinal, mask))
    }
}

/// Test every row of `domain` against `entities`, giving the rows that matched.
///
/// `None` where the row space declined to invert a row — see the call site.
///
/// Parallel over the domain, on the engine's own pool (D-D: there is one), because the route it
/// competes with is parallel over *its* axis and a serial walk here would move the crossover
/// without anything in the design saying so. Chunks are cut by row count rather than by range, so
/// neither a viewport of one huge range nor one of a thousand slivers defeats the split.
fn per_tile_crossing(
    row_space: &tessera_store::permutation::RowSpace,
    entities: &croaring::Bitmap,
    domain: &[Range<u32>],
    rows_in_ranges: u64,
) -> Option<croaring::Bitmap> {
    per_tile_crossing_multi(row_space, &[entities], domain, rows_in_ranges)
        .map(|mut images| images.pop().expect("one set in, one image out"))
}

/// [`per_tile_crossing`] over several entity sets at once — **one walk, one `entity_of` per row**,
/// however many entity-space verdicts a mixed tree carries. This is what keeps 0062's
/// one-crossing rule true for the row route: the expensive half of a crossing is the inversion,
/// and each additional set costs one bitmap probe per row on top of it, not a second walk.
///
/// Returns one row image per input set, positionally. `None` where the row space declined to
/// invert a row — the caller falls back to projection, same as the single-set form.
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
            // Rows accumulate ascending into a small buffer per set and enter the bitmap in
            // batches: `add_many` on a sorted run appends to the container being built, where a
            // per-row `add` re-locates it every time.
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

/// Cut `domain` into parallel chunks by row count — shared by the crossing walk and the
/// render-column scan, so the two fan out identically. Chunks are cut by row count rather than by
/// range, so neither a viewport of one huge range nor one of a thousand slivers defeats the split.
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

    /// A row space over a deliberately non-identity row order, with its `row-entity.u32` attached —
    /// the shape both crossing routes read. An identity order would let a route that returned the
    /// row back as the entity pass.
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

    /// **The claim the whole two-route design rests on**: over every range the request can ask
    /// about, testing the viewport's rows one at a time and projecting the whole result give the
    /// same set. The route is a latency choice and nothing else.
    ///
    /// Asserted against a domain with all three shapes a real viewport produces — a long run, a
    /// sliver, and a gap between them — and at a chunk size small enough that the parallel split
    /// genuinely happens, since a route that is correct only when it runs as one chunk is not
    /// correct.
    #[test]
    fn filter_routes_agree_over_the_domain() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 20,011 is coprime with the row count, so the order is a genuine shuffle rather than a
        // shift, and no row's entity is near it.
        let rows = 40_000u32;
        let row_order: Vec<u32> = (0..rows)
            .map(|r| (r as u64 * 20_011 % rows as u64) as u32)
            .collect();
        let space = row_space_over(dir.path(), &row_order);

        // Every seventh entity, plus a dense block — a result that is neither uniform nor one run.
        let mut entities = croaring::Bitmap::new();
        entities.add_many(&(0..rows).step_by(7).collect::<Vec<u32>>());
        entities.add_range(1_000u32..9_000);

        let domain = vec![0u32..12_345, 20_000..20_003, 30_000..40_000];
        let mut domain_rows = croaring::Bitmap::new();
        for range in &domain {
            domain_rows.add_range(range.clone());
        }

        // `rows_in_ranges` here is only the chunker's sizing hint; pass the real span so the split
        // is the one a viewport of this size would take.
        let per_tile = per_tile_crossing(&space, &entities, &domain, domain_rows.cardinality())
            .expect("a row space with a table can always invert");
        let projected = space.project(&entities);

        assert_eq!(
            per_tile,
            projected.and(&domain_rows),
            "the per-tile crossing and the projection disagree inside the domain"
        );
        // And the per-tile route claims nothing outside it — the property `FilterRows::Viewport`
        // exists to keep a consumer honest about.
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

    /// **A short-circuited `none_of` must still consume its skipped kids' images.**
    ///
    /// `images` is positional: `RowExpr::entity_verdicts` collects every `Entity` node in the tree
    /// whether or not evaluation reaches it, and [`eval_row_expr`] walks the same pre-order with a
    /// cursor. `NoneOf` stops early once its difference is empty — nothing below can widen it —
    /// and leaving the cursor there hands the *next* `Entity` anywhere in the tree someone else's
    /// image. The tree below is the reachable shape: an empty combinator is entity-pure by
    /// construction, so `route` emits `RowExpr::Entity(candidate)` for it, and `check_negations`
    /// admits it inside a `none_of` because it contributes no column to the one-column rule.
    ///
    /// Without the cursor advance the union below answers with the **candidate** — a filter that
    /// silently matches every visible row — instead of with the second clause's verdict.
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
                // The leaf is evaluated and empties the difference; the `Entity` after it is
                // skipped, and its image is the first in `images`.
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

        // An empty domain, so every row scan is empty and the negation short-circuits on its
        // first kid — which is what makes the skipped `Entity` the one under test. The images are
        // already crossed against the domain by the caller, so they are unaffected.
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

    /// A view with no `row-entity.u32` declines the per-tile route rather than answering from a
    /// base it cannot invert. `entity_of` returning `None` on such a row means "ask another way",
    /// and reading it as "this row has no entity" would drop rows from a filtered viewport
    /// silently — so the route decision asks `can_invert` before committing, and the walk itself
    /// still bails if it ever meets one.
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
    /// `row_base`, sorted across segments, and merged where they touch. Merging is what makes the
    /// walk sequential and `FilterRows::covers` a single binary search; it must never widen.
    #[test]
    fn the_crossing_domain_shifts_by_row_base_and_merges_only_what_touches() {
        // Two segments: segment 0 based at row 0, segment 1 at row 1,000. Three tiles, the first
        // two adjacent within segment 0 and the third split across both.
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

    /// The route decision, at its boundary. Strictly greater, so a result exactly at the ratio
    /// still projects — the exact-everywhere route wins ties.
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
        // An empty viewport: the per-tile route walks nothing and is free, where projecting would
        // pay for the whole result to reach the same empty answer.
        assert!(looks_cheaper(1, 0));
        assert!(
            !looks_cheaper(0, 0),
            "nothing matched -- either route is empty"
        );
    }

    /// Every row of one run, matched and narrowed to the rows that carry a value.
    fn run(
        slice: &HotSlice<'_>,
        predicate: &RowPredicate<'_>,
        present: Option<&croaring::Bitmap>,
    ) -> Vec<u32> {
        let rows_in_slice = match slice {
            HotSlice::Bool(a) => a.len(),
            HotSlice::I32(v) => v.len(),
            HotSlice::U8(v) => v.len(),
            HotSlice::F64(v) => v.len(),
            HotSlice::I64(v) | HotSlice::TimestampUs(v) => v.len(),
            _ => unreachable!("the fixtures below use these widths"),
        } as u32;
        let mut rows = croaring::Bitmap::new();
        let mut buf = Vec::with_capacity(1024);
        scan_run(
            slice,
            0,
            0..rows_in_slice,
            predicate,
            present,
            &mut rows,
            &mut buf,
        );
        assert!(buf.is_empty(), "a run must leave its buffer empty");
        rows.iter().collect()
    }

    /// The rows that carry a value, as [`present_rows`] hands them over — `None` is every row.
    fn presence(absent: &[u32], rows: u32) -> croaring::Bitmap {
        let mut present = croaring::Bitmap::new();
        present.add_range(0..rows);
        for row in absent {
            present.remove(*row);
        }
        present
    }

    /// **A row with no number matches no range — including one containing zero, and including an
    /// unbounded one.**
    ///
    /// This is the 2026-08-11 defect on the row route. The hot column is non-nullable, so an absent
    /// number is written as the type's zero and is indistinguishable *in the column* from a real
    /// zero; a range containing zero then matches every row that never had a value. Decision 0064
    /// puts absence in a bitmap beside the column, and this is the scan honouring it.
    ///
    /// The fixture is built so that a scan ignoring presence passes no assertion by luck: rows 1
    /// and 3 carry no value and hold the stored zero, row 4 carries a genuine zero, and the range
    /// straddles zero. Against `[1, 10]` the honouring and the ignoring scan would agree.
    #[test]
    fn an_absent_number_matches_no_range_not_even_one_containing_zero() {
        // rows:      0    1*   2    3*   4    5     (* = no value, stored as the type's zero)
        let values = [7i32, 0, -3, 0, 0, 40];
        let slice = HotSlice::I32(&values);
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

        // The other half of the same rule: a genuine zero must survive it. An over-eager presence
        // rule that dropped the value with the absence would pass the assertion above.
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

        // An unbounded range is "carries a value", not "every row" — the same reading the entity
        // route gives it, and the one an absent row must still fail.
        assert_eq!(
            run(
                &slice,
                &RowPredicate::Range { lo: None, hi: None },
                Some(&present)
            ),
            vec![0, 2, 4, 5]
        );
        // `eq` over a list is the same rule: a needle of zero names the genuine zero only.
        assert_eq!(
            run(
                &slice,
                &RowPredicate::NumberIn(&[Scalar::Int(0), Scalar::Int(40)]),
                Some(&present)
            ),
            vec![4, 5]
        );
        // And the presence half of a negation reads the bitmap alone: the column's bytes say
        // nothing about absence for this family.
        assert_eq!(
            run(&slice, &RowPredicate::ValuePresent, Some(&present)),
            vec![0, 2, 4, 5]
        );
    }

    /// **A category reads absence from its own code 0 and has no bitmap at all** (decision 0064 —
    /// its vocabulary reserves the code before any data exists, so a second mechanism would be the
    /// muddle that decision declines). The scan asks for no presence on this family, so the same
    /// run answers the same rows however the bitmap would have read.
    #[test]
    fn a_category_reads_absence_from_its_sentinel_and_asks_for_no_bitmap() {
        let codes = [1u8, 0, 2, 0, 1, 3];
        let slice = HotSlice::U8(&codes);
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

    /// The bounds are the entity route's, endpoint for endpoint: exclusivity folded by one step
    /// over integers, a bound past the type's ceiling excluding everything and one past its floor
    /// constraining nothing, a NaN bound satisfying nothing, and a fractional bound rounding *into*
    /// the constraint. These are the rules that are wrong in silence if the two copies drift.
    #[test]
    fn a_range_over_the_hot_column_reads_its_endpoints_as_the_entity_route_does() {
        let values = [0u8, 1, 2, 254, 255];
        let slice = HotSlice::U8(&values);
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
            run(&slice, &range(at(0, false), at(254, false)), None),
            vec![1, 2],
            "an exclusive integer bound is the next value along"
        );
        // Beyond the type in either direction, which is where a wrapped comparison would show.
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

        // A fractional bound rounds into the constraint, on both sides.
        let fractional = |v: f64, inclusive: bool| {
            Some(Endpoint {
                value: Scalar::Float(v),
                inclusive,
            })
        };
        assert_eq!(
            run(
                &slice,
                &range(fractional(0.5, true), fractional(2.5, true)),
                None
            ),
            vec![1, 2]
        );

        // NaN is unordered: it satisfies nothing as a bound, and matches nothing as a value.
        assert_eq!(
            run(&slice, &range(fractional(f64::NAN, true), None), None),
            Vec::<u32>::new()
        );
        let floats = [1.0f64, f64::NAN, 3.0];
        assert_eq!(
            run(&HotSlice::F64(&floats), &range(None, None), None),
            vec![0, 1, 2],
            "an unbounded range asks only that the row carry a value"
        );
        assert_eq!(
            run(
                &HotSlice::F64(&floats),
                &range(at(0, true), at(4, true)),
                None
            ),
            vec![0, 2],
            "NaN is outside every bounded range"
        );
        assert_eq!(
            run(
                &HotSlice::F64(&floats),
                &RowPredicate::NumberIn(&[Scalar::Float(f64::NAN)]),
                None
            ),
            Vec::<u32>::new(),
            "NaN equals nothing, itself included"
        );
    }

    /// A bool and a datetime are read as the entity route stores them — `u8::from` for the one,
    /// microseconds as an `i64` for the other — so a predicate means the same thing on both routes.
    #[test]
    fn a_bool_and_a_datetime_compare_as_their_entity_space_storage_does() {
        let flags = arrow::array::BooleanArray::from(vec![true, false, true, false]);
        let slice = HotSlice::Bool(&flags);
        let (yes, no) = ([Scalar::Int(1)], [Scalar::Int(0)]);
        assert_eq!(run(&slice, &RowPredicate::NumberIn(&yes), None), vec![0, 2]);
        assert_eq!(run(&slice, &RowPredicate::NumberIn(&no), None), vec![1, 3]);
        // Absence for a bool is the bitmap too: `false` is a value, not a missing one.
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
                &HotSlice::TimestampUs(&micros),
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
