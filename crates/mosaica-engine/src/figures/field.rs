//! **A field's figures for one request**: over the rows of the viewer's visible set in one view,
//! how many hold no value of a number or timestamp field, and the count, sum, smallest and largest
//! of the finite values the rest hold.
//!
//! The visible set is split as a level's is ([`super`]'s module doc), `S = (F − D) ∪ T`, so the
//! count, the sum and the rows with no value are `F − D + T` exactly. The sum is kept exactly
//! ([`ExactSum`]), so subtracting D's gives back the sum of the values D leaves, however large the
//! values it subtracts, and the mean is rounded once.
//!
//! F is composed rather than walked. A build and a fold store, per view, a tally of each field for
//! each distinct key list the base rows' items carry ([`mosaica_store::field_tallies`]). The items
//! of a key list are in a grant's base rows together or not at all, by the rule the authorised set
//! is built by ([`crate::compose::admits`]), so F's tally is the merge of the tallies of the lists
//! the grant satisfies. A base row's value and its item's keys do not change between folds: an
//! edit gives its item a new entity, whose row is above the base, and a fold writes the file again.
//! The composition is held per grant, shard identity, view and field. A composition whose row
//! count is not the fragment's base rows, and a deny that subtracts more than F holds, are answered
//! by a walk instead and counted, as a disagreement between the stored tallies and the base. A field the file holds no
//! tally of, as one declared at a running service, has its base rows walked instead.
//!
//! D's tally is held per deny version and read at the request's start, so a request that starts
//! after a suppression subtracts it. T's is held per session and generation.
//!
//! Each shard the request reads has its own F, D and T, keyed by its shard, and its own figures.
//! The request's are their sum: the rows, the count, the sum and the rows with no value add, and
//! the smallest and largest are the extremes across shards of each shard's own.
//!
//! The smallest and largest values stay exact under a deny. F's tally keeps the eight smallest and
//! eight largest finite values across the lists it merges, each with its row. The smallest value of
//! `F − D` is the first of the eight smallest whose row D does not hold: any value not kept is at
//! least the last one kept. Where D holds every row kept on a side, `F − D` is walked, once per
//! deny version, and its own eight on each side are kept. A later deny that subtracts every row
//! this one did reads the same way from those before it walks again.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use croaring::Bitmap;
use mosaica_cache::CacheWeight;
pub(crate) use mosaica_store::field_tallies::{
    keep_extreme, ExactSum, FieldTally, Sum, TallyMerge, RESERVE,
};
pub(crate) use mosaica_types::scalar::Number;
use mosaica_types::TermId;

use crate::compose::{EffectiveMask, ShardedMask};
use crate::error::{EngineError, Result};
use crate::viewport::ServedView;
use crate::Engine;

use super::cache::{Build, FieldDenyKey, FieldKey, FieldTailKey, FiguresKey};

/// The base rows one piece of a walk reads before it gives way to drawing requests.
const FILL_ROWS: u32 = 1 << 26;

/// A tally as the figures' caches hold it.
pub(crate) struct Held(pub(crate) FieldTally);

impl CacheWeight for Held {
    fn cache_weight_bytes(&self) -> u64 {
        let sum = match &self.0.sum {
            Sum::Int(_) => 16,
            Sum::Float(c) => 8 * (c.positive.words.len() + c.negative.words.len()) + 34,
        };
        (64 + sum + 24 * (self.0.low.len() + self.0.high.len())) as u64
    }
}

/// Reads one field's values in one request's view.
pub(crate) trait FieldRead: Sync {
    /// The tally of `rows`' values in the side at `at` among the view's sides, keeping the `keep`
    /// most extreme on each side with their rows.
    fn tally(&self, at: usize, rows: &Bitmap, keep: usize) -> Result<FieldTally>;
}

/// What a request subtracts from a fragment's tally: the tally of its denied base rows.
pub(crate) struct FieldDeny {
    tally: FieldTally,
    rows: Bitmap,
}

impl CacheWeight for FieldDeny {
    fn cache_weight_bytes(&self) -> u64 {
        Held(FieldTally::default()).cache_weight_bytes()
            + self
                .rows
                .get_serialized_size_in_bytes::<croaring::Portable>() as u64
    }
}

/// The base rows a deny leaves, walked because it subtracted every row a side of F's tally keeps:
/// the deny, and the tally of the rows it leaves. Held per field, the newest only.
pub(crate) struct FieldLeft {
    deny: Arc<FieldDeny>,
    walked: Arc<Held>,
}

impl CacheWeight for FieldLeft {
    fn cache_weight_bytes(&self) -> u64 {
        self.deny.cache_weight_bytes() + self.walked.cache_weight_bytes()
    }
}

/// A field's figures over one request's visible set in one view.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FieldFigures {
    /// The rows of the visible set.
    pub(crate) items: u64,
    /// Those holding no value.
    pub(crate) none: u64,
    /// Those holding a finite value.
    pub(crate) count: u64,
    /// Their exact sum, which a mean is rounded from once.
    pub(crate) sum: Sum,
    pub(crate) min: Option<Number>,
    pub(crate) max: Option<Number>,
}

impl FieldFigures {
    /// Every shard's figures together: the rows, those with no value, the count and the sum add,
    /// and the smallest and largest are the extremes across shards.
    fn combined(sides: impl IntoIterator<Item = FieldFigures>) -> FieldFigures {
        let mut sides = sides.into_iter();
        let first = sides.next().unwrap_or(FieldFigures {
            items: 0,
            none: 0,
            count: 0,
            sum: Sum::default(),
            min: None,
            max: None,
        });
        sides.fold(first, |a, b| FieldFigures {
            items: a.items + b.items,
            none: a.none + b.none,
            count: a.count + b.count,
            sum: a.sum.plus(&b.sum),
            min: extreme(a.min, b.min, true),
            max: extreme(a.max, b.max, false),
        })
    }
}

/// The smaller of two values, or with `smaller` false the larger; either where one is absent.
fn extreme(a: Option<Number>, b: Option<Number>, smaller: bool) -> Option<Number> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if (a < b) == smaller { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// The first of `side` whose row `rows` does not hold.
fn kept(side: &[(Number, u32)], rows: &Bitmap) -> Option<Number> {
    side.iter()
        .find(|(_, row)| !rows.contains(*row))
        .map(|v| v.0)
}

/// One shard's figures, `F − D + T` over the `items` rows it holds for the request: F's tally
/// `base`, D's `deny` and T's `tail`. `F − D` is taken by subtraction, its extremes from F's
/// reserve; `walk` gives the tally of the base rows D leaves where the reserve on a side is spent
/// (`true`, only its extremes wanted) or D subtracts more than F holds (`false`, walked whole).
fn side_figures(
    items: u64,
    base: &FieldTally,
    deny: Option<&FieldDeny>,
    tail: &FieldTally,
    walk: impl FnOnce(bool) -> Result<Arc<Held>>,
) -> Result<FieldFigures> {
    let (left, none, sum, low, high) = match deny {
        None => (
            base.count,
            base.none,
            base.sum.clone(),
            base.low.first().map(|v| v.0),
            base.high.first().map(|v| v.0),
        ),
        Some(deny) => match (
            base.count.checked_sub(deny.tally.count),
            base.none.checked_sub(deny.tally.none),
        ) {
            (Some(0), Some(none)) => (0, none, Sum::default(), None, None),
            (Some(left), Some(none)) => {
                let (low, high) = match (kept(&base.low, &deny.rows), kept(&base.high, &deny.rows))
                {
                    (Some(low), Some(high)) => (Some(low), Some(high)),
                    (low, high) => {
                        let walked = walk(true)?;
                        (
                            low.or_else(|| kept(&walked.0.low, &deny.rows)),
                            high.or_else(|| kept(&walked.0.high, &deny.rows)),
                        )
                    }
                };
                (left, none, base.sum.minus(&deny.tally.sum), low, high)
            }
            _ => {
                let walked = walk(false)?;
                let walked = &walked.0;
                (
                    walked.count,
                    walked.none,
                    walked.sum.clone(),
                    walked.low.first().map(|v| v.0),
                    walked.high.first().map(|v| v.0),
                )
            }
        },
    };
    Ok(FieldFigures {
        items,
        none: none + tail.none,
        count: left + tail.count,
        sum: sum.plus(&tail.sum),
        min: extreme(low, tail.low.first().map(|v| v.0), true),
        max: extreme(high, tail.high.first().map(|v| v.0), false),
    })
}

impl Engine {
    /// The figures of `column` over `served`'s visible set, composed as `mask` before any filter
    /// narrows it, never over the whole population: each shard's that the request reads
    /// ([`ShardedMask::filtered_leaves`]), combined. `kind` names how its values are compared.
    pub(crate) fn field_figures(
        &self,
        served: &ServedView<'_>,
        mask: &ShardedMask,
        column: &str,
        kind: u8,
        read: &dyn FieldRead,
    ) -> Result<FieldFigures> {
        let mut sides = Vec::new();
        for (at, _, leaf) in served.filtered_sides(mask) {
            sides.push(self.side_field_figures(served, at, leaf, column, kind, read)?);
        }
        Ok(FieldFigures::combined(sides))
    }

    /// The figures of `column` over the visible set of the side at `at`, composed as `mask`.
    fn side_field_figures(
        &self,
        served: &ServedView<'_>,
        at: usize,
        mask: &EffectiveMask,
        column: &str,
        kind: u8,
        read: &dyn FieldRead,
    ) -> Result<FieldFigures> {
        let side = &served.sides[at];
        let base_rows = side.data.row_space.base_rows();
        let identity = served.mask_identity;
        let (fragment, minus, plus, _) = mask.parts();
        let key = FieldKey {
            terms: identity.terms,
            identity: side.geometry.fragment.identity,
            view: served.name.to_string(),
            column: column.to_string(),
            kind,
        };
        let base = self.field_base(served, at, &key, fragment, base_rows, read)?;

        let mut subtracted = minus.clone();
        subtracted.remove_range(base_rows..);
        let cancel = served.cancel.clone().unwrap_or_default();
        let deny = match subtracted.is_empty() {
            true => None,
            false => {
                let (deny_version, failing) = super::deny_version(served, side, &subtracted);
                let deny_key = FieldDenyKey {
                    field: key.clone(),
                    deny_version,
                    failing,
                };
                let deny = self
                    .figures
                    .field_denies
                    .get_or_try_build_waiting(deny_key.clone(), &cancel, || {
                        Ok::<_, EngineError>(FieldDeny {
                            tally: read.tally(at, &subtracted, 0)?,
                            rows: subtracted,
                        })
                    })
                    .map_err(super::waited)?;
                Some((deny_key, deny))
            }
        };

        let mut above = match fragment.maximum().filter(|&last| last >= base_rows) {
            Some(last) => fragment.and(&Bitmap::from_range(base_rows..last + 1)),
            None => Bitmap::new(),
        };
        above.andnot_inplace(minus);
        above.or_inplace(plus);
        let tail = match above.is_empty() {
            true => None,
            false => {
                let key = FieldTailKey {
                    shard: side.shard.id(),
                    token_id: identity.token_id,
                    view: served.name.to_string(),
                    column: column.to_string(),
                    kind,
                    shards: identity.shards,
                };
                let tail = self
                    .figures
                    .field_tails
                    .get_or_try_build_waiting(key, &cancel, || read.tally(at, &above, 1).map(Held))
                    .map_err(super::waited)?;
                Some(tail)
            }
        };

        let empty = FieldTally::default();
        let t = tail.as_deref().map_or(&empty, |t| &t.0);
        side_figures(
            mask.visible_total(),
            &base.0,
            deny.as_ref().map(|(_, deny)| deny.as_ref()),
            t,
            |extremes| {
                let (deny_key, deny) = deny.as_ref().expect("a walk follows a deny");
                if !extremes {
                    self.figures
                        .counters
                        .field_mismatches
                        .fetch_add(1, Ordering::Relaxed);
                }
                self.field_left(
                    served, at, deny_key, deny, fragment, base_rows, read, extremes,
                )
            },
        )
    }

    /// The field's tally over the fragment's base rows: held, composed from the tallies the base
    /// stores per key list, or, for a field it stores none of, walked.
    fn field_base(
        &self,
        served: &ServedView<'_>,
        at: usize,
        key: &FieldKey,
        fragment: &Bitmap,
        base_rows: u32,
        read: &dyn FieldRead,
    ) -> Result<Arc<Held>> {
        let stored = served.sides[at].data.field_tallies.as_deref();
        let mut failed = None;
        let held = self.figures.get_or_build_in(
            &self.figures.fields,
            FiguresKey::Field(key.clone()),
            &served.turn,
            served.cancel.as_ref(),
            |build| {
                if let Some((tallies, field)) =
                    stored.and_then(|t| t.field(&key.column).map(|field| (t, field)))
                {
                    let satisfied = served.session.satisfied();
                    let mut merge = TallyMerge::of(FieldTally::default(), RESERVE);
                    for (keys, per_list) in tallies.lists.iter().zip(&tallies.tallies) {
                        let keys = keys.iter().map(|&k| TermId::new(k));
                        if crate::compose::admits(satisfied, keys) {
                            merge.add(&per_list[field]);
                        }
                    }
                    let tally = merge.finish();
                    // The lists a grant satisfies hold exactly its base rows, which the fragment's
                    // projection over the base also is; a disagreement is answered by a walk.
                    if tally.rows == fragment.range_cardinality(0..base_rows) {
                        return Some(Held(tally));
                    }
                    self.figures
                        .counters
                        .field_mismatches
                        .fetch_add(1, Ordering::Relaxed);
                }
                self.figures
                    .counters
                    .field_fills
                    .fetch_add(1, Ordering::Relaxed);
                walk(fragment, None, base_rows, at, read, build, &mut failed).map(Held)
            },
        );
        match (held, failed) {
            (_, Some(e)) => Err(e),
            (held, None) => held.map_err(super::wait_error),
        }
    }

    /// The tally of the fragment's base rows `deny` leaves, walked once per deny version. With
    /// `extremes`, only its extremes are wanted, and the newest tally walked for an earlier deny
    /// serves where `deny` subtracts every row that one did and a value it kept on each side
    /// survives.
    #[allow(clippy::too_many_arguments)]
    fn field_left(
        &self,
        served: &ServedView<'_>,
        at: usize,
        deny_key: &FieldDenyKey,
        deny: &Arc<FieldDeny>,
        fragment: &Bitmap,
        base_rows: u32,
        read: &dyn FieldRead,
        extremes: bool,
    ) -> Result<Arc<Held>> {
        if extremes {
            if let mosaica_cache::Peek::Ready(earlier) =
                self.figures.field_left.peek(&deny_key.field)
            {
                let reusable = earlier.deny.rows.is_subset(&deny.rows)
                    && kept(&earlier.walked.0.low, &deny.rows).is_some()
                    && kept(&earlier.walked.0.high, &deny.rows).is_some();
                if reusable {
                    return Ok(Arc::clone(&earlier.walked));
                }
            }
        }
        let mut failed = None;
        let held = self.figures.get_or_build_in(
            &self.figures.fields,
            FiguresKey::FieldLeft(deny_key.clone()),
            &served.turn,
            served.cancel.as_ref(),
            |build| {
                self.figures.counters.spent.fetch_add(1, Ordering::Relaxed);
                walk(
                    fragment,
                    Some(&deny.rows),
                    base_rows,
                    at,
                    read,
                    build,
                    &mut failed,
                )
                .map(Held)
            },
        );
        let walked = match (held, failed) {
            (_, Some(e)) => return Err(e),
            (held, None) => held.map_err(super::wait_error)?,
        };
        self.figures.field_left.evict(&deny_key.field);
        let _ = self
            .figures
            .field_left
            .get_or_derive(deny_key.field.clone(), None, |_| FieldLeft {
                deny: Arc::clone(deny),
                walked: Arc::clone(&walked),
            });
        Ok(walked)
    }
}

/// The tally of the fragment's base rows less `less` in the side at `at`, keeping [`RESERVE`]
/// values on each side, read a piece at a time with `build` giving way between pieces. `None`
/// where every caller has gone, or where a read failed, which `failed` then holds.
fn walk(
    fragment: &Bitmap,
    less: Option<&Bitmap>,
    base_rows: u32,
    at: usize,
    read: &dyn FieldRead,
    build: &Build<'_>,
    failed: &mut Option<EngineError>,
) -> Option<FieldTally> {
    let mut tally = TallyMerge::of(FieldTally::default(), RESERVE);
    let mut start = 0u32;
    while start < base_rows {
        let end = start.saturating_add(FILL_ROWS).min(base_rows);
        let mut rows = fragment.and(&Bitmap::from_range(start..end));
        if let Some(less) = less {
            rows.andnot_inplace(less);
        }
        start = end;
        if rows.is_empty() {
            continue;
        }
        match read.tally(at, &rows, RESERVE) {
            Ok(piece) => tally.add(&piece),
            Err(e) => {
                *failed = Some(e);
                return None;
            }
        }
        if !build.give_way() {
            return None;
        }
    }
    Some(tally.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tally of `values`, each a value and its row, keeping `keep` on each side.
    fn tally(values: &[(i128, u32)], keep: usize) -> FieldTally {
        let mut tally = FieldTally {
            rows: values.len() as u64,
            none: 0,
            count: values.len() as u64,
            sum: Sum::Int(values.iter().map(|&(value, _)| value).sum()),
            low: Vec::new(),
            high: Vec::new(),
        };
        for &(value, row) in values {
            keep_extreme(&mut tally.low, keep, (Number::Int(value), row), false);
            keep_extreme(&mut tally.high, keep, (Number::Int(value), row), true);
        }
        tally
    }

    /// One shard's figures over its base rows `values` less the rows `denied`, a walk answering
    /// where the deny spends a side of the reserve.
    fn figures(values: &[(i128, u32)], denied: &[u32]) -> FieldFigures {
        let rows = Bitmap::of(denied);
        let (held, left): (Vec<_>, Vec<_>) = values
            .iter()
            .copied()
            .partition(|&(_, row)| rows.contains(row));
        let deny = FieldDeny {
            tally: tally(&held, 0),
            rows,
        };
        side_figures(
            left.len() as u64,
            &tally(values, RESERVE),
            (!denied.is_empty()).then_some(&deny),
            &FieldTally::default(),
            |_| Ok(Arc::new(Held(tally(&left, RESERVE)))),
        )
        .unwrap()
    }

    /// Two shards against one shard holding both rows, under the same deny. Each shard's extremes
    /// are its first kept value the deny does not hold, or a walk's where the deny holds its whole
    /// reserve, taken against its own rows: the second shard's row 2, or 5, is not denied though
    /// the first's is. The smallest and largest are the extremes across the shards' own.
    #[test]
    fn field_figures_over_two_shards_are_one_shards_holding_both() {
        let shifted = |values: &[(i128, u32)], by: u32| -> Vec<(i128, u32)> {
            values
                .iter()
                .map(|&(value, row)| (value, row + by))
                .collect()
        };
        // The first shard's smallest, 1 at row 2, is denied.
        let first = [(5, 0), (9, 1), (1, 2), (7, 3)];
        let second = [(3, 0), (8, 1), (4, 2)];
        let combined = FieldFigures::combined([figures(&first, &[2]), figures(&second, &[])]);
        let one: Vec<_> = first.iter().copied().chain(shifted(&second, 4)).collect();
        assert_eq!(combined, figures(&one, &[2]));
        assert_eq!(
            (combined.items, combined.count, combined.min, combined.max),
            (6, 6, Some(Number::Int(3)), Some(Number::Int(9)))
        );

        // The first shard's eight smallest are denied, spending its reserve's low side.
        let first: Vec<(i128, u32)> = (0..10).map(|row| (i128::from(row) + 20, row)).collect();
        let second = [(31, 0), (27, 5)];
        let denied: Vec<u32> = (0..8).collect();
        let combined = FieldFigures::combined([figures(&first, &denied), figures(&second, &[])]);
        let one: Vec<_> = first.iter().copied().chain(shifted(&second, 10)).collect();
        assert_eq!(combined, figures(&one, &denied));
        assert_eq!(
            (
                combined.count,
                combined.sum.mean(combined.count),
                combined.min
            ),
            (4, Some(115.0 / 4.0), Some(Number::Int(27)))
        );
    }
}
