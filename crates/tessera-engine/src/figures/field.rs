//! **A field's figures for one request**: over the rows of the viewer's visible set in one view,
//! how many hold no value of a number or timestamp field, and the count, sum, smallest and largest
//! of the finite values the rest hold.
//!
//! The visible set is split as a level's is ([`super`]'s module doc), `S = (F − D) ∪ T`, so the
//! count, the sum and the rows with no value are `F − D + T` exactly. F's tally is held per grant,
//! bundle identity, view and field, shared by every session with the same grant, walked once off
//! the request path and written to the cache directory. A base row's value does not change between
//! folds: an edit gives its item a new entity, whose row is above the base, and a fold rotates the
//! bundle identity. D's tally is held per deny version and read at the request's start, so a
//! request that starts after a suppression subtracts it. T's is held per session and generation.
//!
//! The smallest and largest values stay exact under a deny. F's tally keeps its eight smallest and
//! eight largest finite values, each with its row. The smallest value of `F − D` is the first of
//! the eight smallest whose row D does not hold: any value not kept is at least the last one kept.
//! Where D holds every row kept on a side, `F − D` is walked for that side, once per deny version.
//!
//! The sum of a float field is taken in `float64`, by pieces in parallel, and corrected by
//! subtraction, so its last digits can differ from a sum taken in another order. An integer or
//! timestamp field's sum is exact.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use croaring::Bitmap;
use tessera_cache::CacheWeight;

use crate::compose::EffectiveMask;
use crate::error::{EngineError, Result};
use crate::row_column::RESERVE;
use crate::viewport::ServedView;
use crate::Engine;

use super::cache::{FieldDenyKey, FieldKey, FieldTailKey, FiguresKey};
use super::persist;

/// The base rows one piece of a fill reads before it gives way to drawing requests.
const FILL_ROWS: u32 = 1 << 26;

/// A field's value as its figures hold it: an integer or a timestamp's microseconds exactly, a
/// float as itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Number {
    Int(i128),
    Float(f64),
}

impl Number {
    /// Zero of the kind `float` says.
    pub(crate) fn zero(float: bool) -> Number {
        match float {
            true => Number::Float(0.0),
            false => Number::Int(0),
        }
    }

    fn plus(self, other: Number) -> Number {
        match (self, other) {
            (Number::Int(a), Number::Int(b)) => Number::Int(a + b),
            (a, b) => Number::Float(a.as_f64() + b.as_f64()),
        }
    }

    fn minus(self, other: Number) -> Number {
        match (self, other) {
            (Number::Int(a), Number::Int(b)) => Number::Int(a - b),
            (a, b) => Number::Float(a.as_f64() - b.as_f64()),
        }
    }

    pub(crate) fn as_f64(self) -> f64 {
        match self {
            Number::Int(i) => i as f64,
            Number::Float(f) => f,
        }
    }

    /// Whether `self` is below `other`, two values of one field.
    pub(crate) fn below(self, other: Number) -> bool {
        match (self, other) {
            (Number::Int(a), Number::Int(b)) => a < b,
            (a, b) => a.as_f64() < b.as_f64(),
        }
    }
}

/// One set of rows' values of a field: how many rows hold no value, and the count and sum of the
/// finite values, with the most extreme of them, each with its row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FieldTally {
    pub(crate) none: u64,
    pub(crate) count: u64,
    pub(crate) sum: Number,
    /// The smallest values, ascending.
    pub(crate) low: Vec<(Number, u32)>,
    /// The largest values, descending.
    pub(crate) high: Vec<(Number, u32)>,
}

impl FieldTally {
    /// The tally of no rows.
    pub(crate) fn empty(float: bool) -> FieldTally {
        FieldTally {
            none: 0,
            count: 0,
            sum: Number::zero(float),
            low: Vec::new(),
            high: Vec::new(),
        }
    }

    /// Both tallies, keeping `keep` values on each side.
    fn merged(mut self, other: &FieldTally, keep: usize) -> FieldTally {
        self.none += other.none;
        self.count += other.count;
        self.sum = self.sum.plus(other.sum);
        self.low.extend_from_slice(&other.low);
        self.low.sort_by(|a, b| by_value(a, b, false));
        self.low.truncate(keep);
        self.high.extend_from_slice(&other.high);
        self.high.sort_by(|a, b| by_value(a, b, true));
        self.high.truncate(keep);
        self
    }
}

/// `a` against `b` by value, ascending, or descending where `reversed`, then by row: the order
/// a tally keeps its extreme values in.
pub(crate) fn by_value(a: &(Number, u32), b: &(Number, u32), reversed: bool) -> std::cmp::Ordering {
    let ordered = match (a.0.below(b.0), b.0.below(a.0)) {
        (true, _) => std::cmp::Ordering::Less,
        (_, true) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    };
    let ordered = if reversed {
        ordered.reverse()
    } else {
        ordered
    };
    ordered.then(a.1.cmp(&b.1))
}

impl CacheWeight for FieldTally {
    fn cache_weight_bytes(&self) -> u64 {
        64 + 32 * (self.low.len() + self.high.len()) as u64
    }
}

/// Reads one field's values in one request's view.
pub(crate) trait FieldRead: Sync {
    /// Whether the field's values are floats.
    fn float(&self) -> bool;

    /// The tally of `rows`' values, keeping the `keep` most extreme on each side with their rows.
    fn tally(&self, rows: &Bitmap, keep: usize) -> Result<FieldTally>;
}

/// What a request subtracts from a fragment's tally: the tally of its denied base rows, and, once a
/// side's reserve is spent, the extremes of the base rows left.
pub(crate) struct FieldDeny {
    tally: FieldTally,
    rows: Bitmap,
    /// The smallest and largest value of the fragment's base rows less `rows`, worked out on first
    /// asking.
    left: Mutex<Option<(Option<Number>, Option<Number>)>>,
}

impl CacheWeight for FieldDeny {
    fn cache_weight_bytes(&self) -> u64 {
        self.tally.cache_weight_bytes()
            + self
                .rows
                .get_serialized_size_in_bytes::<croaring::Portable>() as u64
    }
}

/// A field's figures over one request's visible set in one view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FieldFigures {
    /// The rows of the visible set.
    pub(crate) items: u64,
    /// Those holding no value.
    pub(crate) none: u64,
    /// Those holding a finite value.
    pub(crate) count: u64,
    pub(crate) sum: Number,
    pub(crate) min: Option<Number>,
    pub(crate) max: Option<Number>,
}

impl FieldFigures {
    /// The mean of the finite values, `None` where there are none.
    pub(crate) fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum.as_f64() / self.count as f64)
    }
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
                let (deny_version, failing) = super::deny_version(served, base_rows, &subtracted);
                let key = FieldDenyKey {
                    field: key.clone(),
                    deny_version,
                    failing,
                };
                let deny = self
                    .figures
                    .field_denies
                    .get_or_try_build_waiting(key, &cancel, || {
                        Ok::<_, EngineError>(FieldDeny {
                            tally: read.tally(&subtracted, 0)?,
                            rows: subtracted,
                            left: Mutex::new(None),
                        })
                    })
                    .map_err(super::waited)?;
                Some(deny)
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

        let empty = FieldTally::empty(read.float());
        let d = deny.as_ref().map_or(&empty, |d| &d.tally);
        let t = tail.as_deref().unwrap_or(&empty);
        let left = base.count - d.count;
        let (low, high) = match (&deny, left) {
            (_, 0) => (None, None),
            (None, _) => (base.low.first().map(|v| v.0), base.high.first().map(|v| v.0)),
            (Some(deny), _) => {
                let kept = |side: &[(Number, u32)]| {
                    side.iter()
                        .find(|(_, row)| !deny.rows.contains(*row))
                        .map(|v| v.0)
                };
                match (kept(&base.low), kept(&base.high)) {
                    (Some(low), Some(high)) => (Some(low), Some(high)),
                    (low, high) => {
                        let (walked_low, walked_high) =
                            self.left_extremes(deny, fragment, base_rows, read)?;
                        (low.or(walked_low), high.or(walked_high))
                    }
                }
            }
        };
        let lower = |a: Option<Number>, b: Option<Number>, smaller: bool| match (a, b) {
            (Some(a), Some(b)) => Some(if a.below(b) == smaller { a } else { b }),
            (a, b) => a.or(b),
        };
        Ok(FieldFigures {
            items: mask.visible_total(),
            none: base.none - d.none + t.none,
            count: left + t.count,
            sum: base.sum.minus(d.sum).plus(t.sum),
            min: lower(low, t.low.first().map(|v| v.0), true),
            max: lower(high, t.high.first().map(|v| v.0), false),
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
        let float = read.float();
        let mut failed = None;
        let held = self.figures.get_or_build_in(
            &self.figures.fields,
            FiguresKey::Field(key.clone()),
            &served.turn,
            served.cancel.as_ref(),
            |build| {
                if let Some(tally) = dir
                    .as_deref()
                    .and_then(|dir| persist::read_field(dir, &stem, float))
                {
                    self.figures.counters.loads.fetch_add(1, Ordering::Relaxed);
                    return Some(tally);
                }
                self.figures.counters.fills.fetch_add(1, Ordering::Relaxed);
                let mut tally = FieldTally::empty(float);
                let mut at = 0u32;
                while at < base_rows {
                    let end = at.saturating_add(FILL_ROWS).min(base_rows);
                    let rows = fragment.and(&Bitmap::from_range(at..end));
                    at = end;
                    if rows.is_empty() {
                        continue;
                    }
                    match read.tally(&rows, RESERVE) {
                        Ok(piece) => tally = tally.merged(&piece, RESERVE),
                        Err(e) => {
                            failed = Some(e);
                            return None;
                        }
                    }
                    if !build.give_way() {
                        return None;
                    }
                }
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

    /// The smallest and largest value of the fragment's base rows `deny` leaves: walked once per
    /// deny correction, where it subtracts every row a side's reserve holds.
    fn left_extremes(
        &self,
        deny: &FieldDeny,
        fragment: &Bitmap,
        base_rows: u32,
        read: &dyn FieldRead,
    ) -> Result<(Option<Number>, Option<Number>)> {
        let mut left = deny.left.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = *left {
            return Ok(held);
        }
        self.figures.counters.spent.fetch_add(1, Ordering::Relaxed);
        let mut rows = fragment.and(&Bitmap::from_range(0..base_rows));
        rows.andnot_inplace(&deny.rows);
        let walked = read.tally(&rows, 1)?;
        let extremes = (
            walked.low.first().map(|v| v.0),
            walked.high.first().map(|v| v.0),
        );
        *left = Some(extremes);
        Ok(extremes)
    }
}
