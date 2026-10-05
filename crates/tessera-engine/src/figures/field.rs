//! **A field's figures for one request**: over the rows of the viewer's visible set in one view,
//! how many hold no value of a number or timestamp field, and the count, sum, smallest and largest
//! of the finite values the rest hold.
//!
//! The visible set is split as a level's is ([`super`]'s module doc), `S = (F − D) ∪ T`, so the
//! count, the sum and the rows with no value are `F − D + T` exactly. The sum is kept exactly
//! ([`ExactSum`]), so subtracting D's gives back the sum of the values D leaves, however large the
//! values it subtracts, and the mean is rounded once. F's tally is held per grant, bundle
//! identity, view and field, shared by every session with the same grant, walked once off the
//! request path and written to the cache directory. A base row's value does not change between
//! folds: an edit gives its item a new entity, whose row is above the base, and a fold rotates the
//! bundle identity. D's tally is held per deny version and read at the request's start, so a
//! request that starts after a suppression subtracts it. T's is held per session and generation.
//!
//! The smallest and largest values stay exact under a deny. F's tally keeps its eight smallest and
//! eight largest finite values, each with its row. The smallest value of `F − D` is the first of
//! the eight smallest whose row D does not hold: any value not kept is at least the last one kept.
//! Where D holds every row kept on a side, `F − D` is walked, once per deny version, as F is, and
//! its own eight on each side are kept. A later deny that subtracts every row this one did reads
//! the same way from those before it walks again.

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use croaring::Bitmap;
use tessera_cache::CacheWeight;

use crate::compose::EffectiveMask;
use crate::error::{EngineError, Result};
use crate::row_column::RESERVE;
use crate::viewport::ServedView;
use crate::Engine;

use super::cache::{Build, FieldDenyKey, FieldKey, FieldTailKey, FiguresKey};
pub(crate) use super::exact::ExactSum;
use super::persist;

/// The base rows one piece of a walk reads before it gives way to drawing requests.
const FILL_ROWS: u32 = 1 << 26;

/// A field's value as its figures hold it: an integer or a timestamp's microseconds exactly, a
/// float as itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Number {
    Int(i128),
    Float(f64),
}

impl Number {
    pub(crate) fn as_f64(self) -> f64 {
        match self {
            Number::Int(i) => i as f64,
            Number::Float(f) => f,
        }
    }
}

impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Number) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Number::Int(a), Number::Int(b)) => a.partial_cmp(b),
            (a, b) => a.as_f64().partial_cmp(&b.as_f64()),
        }
    }
}

/// Put `value` among `side`'s `keep` most extreme values, where it is one of them: the smallest
/// first, or with `high` the largest first, ties by where each was read. The one order every
/// tally keeps its extremes in.
#[inline]
pub(crate) fn keep_extreme<K: PartialOrd + Copy>(
    side: &mut Vec<(K, u32)>,
    keep: usize,
    value: (K, u32),
    high: bool,
) {
    let before = |a: &(K, u32), b: &(K, u32)| match high {
        false => a.0 < b.0 || (a.0 == b.0 && a.1 < b.1),
        true => a.0 > b.0 || (a.0 == b.0 && a.1 < b.1),
    };
    if side.len() == keep && side.last().is_none_or(|last| !before(&value, last)) {
        return;
    }
    let at = side.partition_point(|held| before(held, &value));
    side.insert(at, value);
    side.truncate(keep);
}

/// One set of rows' values of a field: how many rows hold no value, and the count and exact sum of
/// the finite values, with the most extreme of them, each with its row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FieldTally {
    pub(crate) none: u64,
    pub(crate) count: u64,
    pub(crate) sum: ExactSum,
    /// The smallest values, ascending.
    pub(crate) low: Vec<(Number, u32)>,
    /// The largest values, descending.
    pub(crate) high: Vec<(Number, u32)>,
}

impl FieldTally {
    /// The tally of no rows.
    pub(crate) fn empty() -> FieldTally {
        FieldTally {
            none: 0,
            count: 0,
            sum: ExactSum::default(),
            low: Vec::new(),
            high: Vec::new(),
        }
    }

    /// Both tallies, keeping `keep` values on each side.
    fn merged(mut self, other: &FieldTally, keep: usize) -> FieldTally {
        self.none += other.none;
        self.count += other.count;
        self.sum = self.sum.plus(&other.sum);
        for &value in &other.low {
            keep_extreme(&mut self.low, keep, value, false);
        }
        for &value in &other.high {
            keep_extreme(&mut self.high, keep, value, true);
        }
        self
    }
}

impl CacheWeight for FieldTally {
    fn cache_weight_bytes(&self) -> u64 {
        (64 + 8 * ExactSum::WORDS + 32 * (self.low.len() + self.high.len())) as u64
    }
}

/// Reads one field's values in one request's view.
pub(crate) trait FieldRead: Sync {
    /// The tally of `rows`' values, keeping the `keep` most extreme on each side with their rows.
    fn tally(&self, rows: &Bitmap, keep: usize) -> Result<FieldTally>;
}

/// What a request subtracts from a fragment's tally: the tally of its denied base rows.
pub(crate) struct FieldDeny {
    tally: FieldTally,
    rows: Bitmap,
}

impl CacheWeight for FieldDeny {
    fn cache_weight_bytes(&self) -> u64 {
        self.tally.cache_weight_bytes()
            + self
                .rows
                .get_serialized_size_in_bytes::<croaring::Portable>() as u64
    }
}

/// The base rows a deny leaves, walked because it subtracted every row a side of F's tally keeps:
/// the deny, and the tally of the rows it leaves. Held per field, the newest only.
pub(crate) type FieldLeft = (Arc<FieldDeny>, Arc<FieldTally>);

/// A field's figures over one request's visible set in one view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FieldFigures {
    /// The rows of the visible set.
    pub(crate) items: u64,
    /// Those holding no value.
    pub(crate) none: u64,
    /// Those holding a finite value.
    pub(crate) count: u64,
    /// Their mean, rounded once from their exact sum; `None` where there are none.
    pub(crate) mean: Option<f64>,
    pub(crate) min: Option<Number>,
    pub(crate) max: Option<Number>,
}

/// The first of `side` whose row `rows` does not hold.
fn kept(side: &[(Number, u32)], rows: &Bitmap) -> Option<Number> {
    side.iter()
        .find(|(_, row)| !rows.contains(*row))
        .map(|v| v.0)
}

impl Engine {
    /// The figures of `column` over `served`'s visible set, composed as `mask`, never over the
    /// whole population. `kind` names how its values are compared.
    pub(crate) fn field_figures(
        &self,
        served: &ServedView<'_>,
        mask: &EffectiveMask,
        column: &str,
        kind: u8,
        read: &dyn FieldRead,
    ) -> Result<FieldFigures> {
        let base_rows = served.data.row_space.base_rows();
        let identity = served.mask_identity;
        let (fragment, minus, plus, _) = mask.parts();
        let key = FieldKey {
            terms: identity.terms,
            identity: identity.fragment_identity,
            view: served.name.to_string(),
            column: column.to_string(),
            kind,
        };
        let base = self.field_base(served, &key, fragment, base_rows, read)?;

        let mut subtracted = minus.clone();
        subtracted.remove_range(base_rows..);
        let cancel = served.cancel.clone().unwrap_or_default();
        let deny = match subtracted.is_empty() {
            true => None,
            false => {
                let (deny_version, failing) =
                    super::deny_version(served, served.denied, &subtracted);
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
                            tally: read.tally(&subtracted, 0)?,
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
                    token_id: identity.token_id,
                    view: served.name.to_string(),
                    column: column.to_string(),
                    kind,
                    segments_version: identity.segments_version,
                    projection_segments_version: identity.projection_segments_version,
                    overlay_version: identity.overlay_version,
                };
                let tail = self
                    .figures
                    .field_tails
                    .get_or_try_build_waiting(key, &cancel, || read.tally(&above, 1))
                    .map_err(super::waited)?;
                Some(tail)
            }
        };

        let empty = FieldTally::empty();
        let d = deny.as_ref().map_or(&empty, |(_, d)| &d.tally);
        let t = tail.as_deref().unwrap_or(&empty);
        let left = base.count - d.count;
        let (low, high) = match (&deny, left) {
            (_, 0) => (None, None),
            (None, _) => (
                base.low.first().map(|v| v.0),
                base.high.first().map(|v| v.0),
            ),
            (Some((deny_key, deny)), _) => {
                match (kept(&base.low, &deny.rows), kept(&base.high, &deny.rows)) {
                    (Some(low), Some(high)) => (Some(low), Some(high)),
                    (low, high) => {
                        let walked =
                            self.field_left(served, deny_key, deny, fragment, base_rows, read)?;
                        (
                            low.or_else(|| kept(&walked.low, &deny.rows)),
                            high.or_else(|| kept(&walked.high, &deny.rows)),
                        )
                    }
                }
            }
        };
        let pick = |a: Option<Number>, b: Option<Number>, smaller: bool| match (a, b) {
            (Some(a), Some(b)) => Some(if (a < b) == smaller { a } else { b }),
            (a, b) => a.or(b),
        };
        let count = left + t.count;
        Ok(FieldFigures {
            items: mask.visible_total(),
            none: base.none - d.none + t.none,
            count,
            mean: base.sum.clone().minus(&d.sum).plus(&t.sum).mean(count),
            min: pick(low, t.low.first().map(|v| v.0), true),
            max: pick(high, t.high.first().map(|v| v.0), false),
        })
    }

    /// The field's tally over the fragment's base rows: held, read from disk, or walked.
    fn field_base(
        &self,
        served: &ServedView<'_>,
        key: &FieldKey,
        fragment: &Bitmap,
        base_rows: u32,
        read: &dyn FieldRead,
    ) -> Result<Arc<FieldTally>> {
        let dir = self
            .figures
            .dir
            .as_ref()
            .map(|root| persist::identity_dir(root, &key.identity));
        let stem = persist::field_stem(&key.terms, &key.view, &key.column, key.kind);
        let mut failed = None;
        let held = self.figures.get_or_build_in(
            &self.figures.fields,
            FiguresKey::Field(key.clone()),
            &served.turn,
            served.cancel.as_ref(),
            |build| {
                if let Some(tally) = dir
                    .as_deref()
                    .and_then(|dir| persist::read_field(dir, &stem))
                {
                    self.figures.counters.loads.fetch_add(1, Ordering::Relaxed);
                    return Some(tally);
                }
                self.figures.counters.fills.fetch_add(1, Ordering::Relaxed);
                let tally = walk(fragment, None, base_rows, read, build, &mut failed)?;
                if let Some(dir) = dir.clone() {
                    let (written, stem) = (tally.clone(), stem.clone());
                    let bound = Arc::clone(&self.figures.disk_bound);
                    let root = self.figures.dir.clone();
                    self.figures.worker.submit(move || {
                        persist::write_field(&dir, &stem, &written);
                        if let Some(root) = root {
                            persist::hold_under(&root, bound.load(Ordering::Relaxed));
                        }
                    });
                }
                Some(tally)
            },
        );
        match (held, failed) {
            (_, Some(e)) => Err(e),
            (held, None) => held.map_err(super::wait_error),
        }
    }

    /// The tally of the fragment's base rows `deny` leaves: the newest one walked for an earlier
    /// deny where `deny` subtracts every row that one did and a value it kept on each side
    /// survives, and otherwise walked, once per deny version.
    fn field_left(
        &self,
        served: &ServedView<'_>,
        deny_key: &FieldDenyKey,
        deny: &Arc<FieldDeny>,
        fragment: &Bitmap,
        base_rows: u32,
        read: &dyn FieldRead,
    ) -> Result<Arc<FieldTally>> {
        let earlier = self
            .figures
            .field_left
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&deny_key.field)
            .cloned();
        if let Some((walked_for, walked)) = earlier {
            let reusable = walked_for.rows.is_subset(&deny.rows)
                && kept(&walked.low, &deny.rows).is_some()
                && kept(&walked.high, &deny.rows).is_some();
            if reusable {
                return Ok(walked);
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
                    read,
                    build,
                    &mut failed,
                )
            },
        );
        let walked = match (held, failed) {
            (_, Some(e)) => return Err(e),
            (held, None) => held.map_err(super::wait_error)?,
        };
        self.figures
            .field_left
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                deny_key.field.clone(),
                (Arc::clone(deny), Arc::clone(&walked)),
            );
        Ok(walked)
    }
}

/// The tally of the fragment's base rows less `less`, keeping [`RESERVE`] values on each side,
/// read a piece at a time with `build` giving way between pieces. `None` where every caller has
/// gone, or where a read failed, which `failed` then holds.
fn walk(
    fragment: &Bitmap,
    less: Option<&Bitmap>,
    base_rows: u32,
    read: &dyn FieldRead,
    build: &Build<'_>,
    failed: &mut Option<EngineError>,
) -> Option<FieldTally> {
    let mut tally = FieldTally::empty();
    let mut at = 0u32;
    while at < base_rows {
        let end = at.saturating_add(FILL_ROWS).min(base_rows);
        let mut rows = fragment.and(&Bitmap::from_range(at..end));
        if let Some(less) = less {
            rows.andnot_inplace(less);
        }
        at = end;
        if rows.is_empty() {
            continue;
        }
        match read.tally(&rows, RESERVE) {
            Ok(piece) => tally = tally.merged(&piece, RESERVE),
            Err(e) => {
                *failed = Some(e);
                return None;
            }
        }
        if !build.give_way() {
            return None;
        }
    }
    Some(tally)
}
