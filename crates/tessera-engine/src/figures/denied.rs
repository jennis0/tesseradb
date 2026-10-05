//! The labels of a view's denied base rows, and the correction a request subtracts with them.
//!
//! A cached count over a fragment's base rows counts every row the fragment holds, denied or not.
//! The correction takes the denied ones back out, so it needs each denied row's labels at the
//! version of the level the request reads. Reading them from the column per request costs seconds
//! cold at scale, so they are held per `(view, layer, level)`: brought forward by a growth's steps
//! and by the rows a deny or a lift adds or removes, read again whole only for a new column.

use std::sync::{Mutex, PoisonError};

use croaring::Bitmap;
use rustc_hash::FxHashMap;

use crate::derived::{place, Placement};
use crate::row_column::{reserve_row, reserve_value, RowColumn};

use super::counts::{deltas_weight, CountsAt, Deltas};

/// One denied row's labels at the level's version, and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Labelled {
    pub(crate) ordinals: Vec<u32>,
    pub(crate) position: Option<(u32, u32)>,
}

/// The labels of one view's denied base rows for one level.
#[derive(Debug)]
pub(crate) struct DeniedLabels {
    /// The bundle identity the rows are numbered under.
    pub(crate) identity: [u8; 32],
    /// [`RowColumn::identity`] of the column the labels were read from.
    pub(crate) column: u64,
    /// The level version the labels describe.
    pub(crate) at: u64,
    /// The denied base rows held.
    pub(crate) rows: Bitmap,
    pub(crate) labels: FxHashMap<u32, Labelled>,
}

/// What bringing the held labels to a request's version and denied rows came to.
pub(crate) enum Brought {
    Ready(DeniedLabels),
    /// More new denied rows than a request reads inline.
    TooMany,
    /// The held labels are of another column, or no steps lead from their version to the one asked.
    Broken,
}

/// One row's labels and position, read from the column and the segments.
pub(crate) fn labelled(column: &RowColumn, row: u32, places: &[Placement<'_>]) -> Labelled {
    let mut ordinals = Vec::new();
    column.for_each_label(row, |ordinal| ordinals.push(ordinal));
    Labelled {
        ordinals,
        position: place(places, row),
    }
}

impl DeniedLabels {
    /// Every row of `rows` read from `column`.
    pub(crate) fn read(
        identity: [u8; 32],
        column: &RowColumn,
        at: u64,
        rows: &Bitmap,
        places: &[Placement<'_>],
    ) -> Self {
        let labels = rows
            .iter()
            .map(|row| (row, labelled(column, row, places)))
            .collect();
        DeniedLabels {
            identity,
            column: column.identity(),
            at,
            rows: rows.clone(),
            labels,
        }
    }

    /// These labels at `column`'s version `at`, over the denied base rows `rows`, reading at most
    /// `limit` rows from the column.
    pub(crate) fn brought(
        &self,
        identity: [u8; 32],
        column: &RowColumn,
        at: u64,
        rows: &Bitmap,
        places: &[Placement<'_>],
        limit: u64,
    ) -> Brought {
        if self.identity != identity || self.column != column.identity() || self.at > at {
            return Brought::Broken;
        }
        let Some(steps) = column.steps_between(self.at, at) else {
            return Brought::Broken;
        };
        let fresh = rows.andnot(&self.rows);
        if fresh.cardinality() > limit {
            return Brought::TooMany;
        }
        let mut labels = self.labels.clone();
        let gone = self.rows.andnot(rows);
        for row in gone.iter() {
            labels.remove(&row);
        }
        for step in steps {
            for &(row, ordinal) in step.pairs.iter() {
                if let Some(held) = labels.get_mut(&row) {
                    held.ordinals.push(ordinal);
                }
            }
        }
        for row in fresh.iter() {
            labels.insert(row, labelled(column, row, places));
        }
        Brought::Ready(DeniedLabels {
            identity,
            column: column.identity(),
            at,
            rows: rows.clone(),
            labels,
        })
    }
}

/// What a request subtracts from a fragment's counts: every row of the composed mask's `minus`
/// below the base, which is the fragment's base rows the viewer may not see.
pub(crate) struct DenyCorrection {
    pub(crate) deltas: Deltas,
    /// The rows subtracted.
    pub(crate) rows: Bitmap,
    /// Per artifact a denied row touches, the box of the rows left, worked out on first asking.
    boxes: Mutex<FxHashMap<u32, Option<[u32; 4]>>>,
}

impl DenyCorrection {
    /// The correction for `rows`, each labelled from `labels` where it holds the row and from
    /// `column` where it does not.
    pub(crate) fn of(
        rows: Bitmap,
        labels: Option<&DeniedLabels>,
        column: &RowColumn,
        places: &[Placement<'_>],
    ) -> Self {
        let mut deltas = Deltas::default();
        for row in rows.iter() {
            let read;
            let held = match labels.and_then(|labels| labels.labels.get(&row)) {
                Some(held) => held,
                None => {
                    read = labelled(column, row, places);
                    &read
                }
            };
            for &ordinal in &held.ordinals {
                deltas.entry(ordinal).or_default().add(held.position);
            }
        }
        DenyCorrection {
            deltas,
            rows,
            boxes: Mutex::default(),
        }
    }

    pub(crate) fn weight_bytes(&self) -> u64 {
        deltas_weight(&self.deltas)
            + self
                .rows
                .get_serialized_size_in_bytes::<croaring::Portable>() as u64
    }

    /// The box of the fragment's placed rows of `ordinal` that this does not subtract, or `None`
    /// where it leaves none. Read off `counts`' reserve where a side's extreme row is subtracted;
    /// `exact` is the box worked out from the rows themselves, for a side whose reserve is spent.
    pub(crate) fn bbox(
        &self,
        ordinal: u32,
        counts: &CountsAt,
        exact: impl FnOnce() -> Option<[u32; 4]>,
    ) -> Option<[u32; 4]> {
        let Some(delta) = self.deltas.get(&ordinal) else {
            return counts.bbox(ordinal);
        };
        if counts.placed(ordinal) <= u64::from(delta.placed) {
            return None;
        }
        if let Some(held) = self
            .boxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&ordinal)
        {
            return *held;
        }
        let from_reserve = || -> Option<[u32; 4]> {
            if !counts.reserves() {
                return None;
            }
            let mut out = [0u32; 4];
            for (side, value) in out.iter_mut().enumerate() {
                let (keys, _) = counts.side(ordinal, side);
                let kept = keys
                    .into_iter()
                    .find(|&key| !self.rows.contains(reserve_row(key)))?;
                *value = reserve_value(side, kept);
            }
            Some(out)
        };
        let answer = from_reserve().or_else(exact);
        self.boxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(ordinal, answer);
        answer
    }
}
