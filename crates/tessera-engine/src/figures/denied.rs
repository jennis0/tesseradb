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

use super::counts::{deltas_weight, CountsAt, Deltas, Reserves};

/// A row's position, `None` where no segment places it.
pub(crate) type Position = Option<(u32, u32)>;

/// The labels of one view's denied base rows for one level, as flat arrays.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DeniedLabels {
    /// The bundle identity the rows are numbered under.
    pub(crate) identity: [u8; 32],
    /// [`RowColumn::identity`] of the column the labels were read from.
    pub(crate) column: u64,
    /// The level version the labels describe.
    pub(crate) at: u64,
    /// The denied base rows held, ascending.
    pub(crate) rows: Vec<u32>,
    /// Per row, where its ordinals begin in `ordinals`; one more entry than `rows`.
    pub(crate) offsets: Vec<u32>,
    pub(crate) ordinals: Vec<u32>,
    /// Per row, its position, `None` where no segment places it.
    pub(crate) positions: Vec<Option<(u32, u32)>>,
}

/// What bringing the held labels to a request's version and denied rows came to.
pub(crate) enum Brought {
    Ready(DeniedLabels),
    /// More new denied rows than the caller reads.
    TooMany,
    /// The held labels are of another column, or no steps lead from their version to the one asked.
    Broken,
}

/// Builds [`DeniedLabels`] a row at a time, in ascending row order.
struct Builder {
    rows: Vec<u32>,
    offsets: Vec<u32>,
    ordinals: Vec<u32>,
    positions: Vec<Option<(u32, u32)>>,
}

impl Builder {
    fn new() -> Self {
        Builder {
            rows: Vec::new(),
            offsets: vec![0],
            ordinals: Vec::new(),
            positions: Vec::new(),
        }
    }

    fn push(
        &mut self,
        row: u32,
        ordinals: impl IntoIterator<Item = u32>,
        position: Option<(u32, u32)>,
    ) {
        self.rows.push(row);
        self.ordinals.extend(ordinals);
        self.offsets.push(self.ordinals.len() as u32);
        self.positions.push(position);
    }

    /// `row`'s labels read from `column`, and its position.
    fn read(&mut self, column: &RowColumn, row: u32, places: &[Placement<'_>]) {
        self.rows.push(row);
        column.for_each_label(row, |ordinal| self.ordinals.push(ordinal));
        self.offsets.push(self.ordinals.len() as u32);
        self.positions.push(place(places, row));
    }

    fn finish(self, identity: [u8; 32], column: u64, at: u64) -> DeniedLabels {
        DeniedLabels {
            identity,
            column,
            at,
            rows: self.rows,
            offsets: self.offsets,
            ordinals: self.ordinals,
            positions: self.positions,
        }
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
        let mut out = Builder::new();
        for row in rows.iter() {
            out.read(column, row, places);
        }
        out.finish(identity, column.identity(), at)
    }

    /// The labels and position of `row`, where it is held.
    pub(crate) fn of(&self, row: u32) -> Option<(&[u32], Position)> {
        let i = self.rows.binary_search(&row).ok()?;
        let (lo, hi) = (self.offsets[i] as usize, self.offsets[i + 1] as usize);
        Some((&self.ordinals[lo..hi], self.positions[i]))
    }

    pub(crate) fn weight_bytes(&self) -> u64 {
        (self.rows.len() * 4
            + self.offsets.len() * 4
            + self.ordinals.len() * 4
            + self.positions.len() * 12) as u64
            + 96
    }

    /// These labels at `column`'s version `at`, over the denied base rows `rows`, reading at most
    /// `limit` new rows from the column, and how many it read.
    pub(crate) fn brought(
        &self,
        identity: [u8; 32],
        column: &RowColumn,
        at: u64,
        rows: &Bitmap,
        places: &[Placement<'_>],
        limit: u64,
    ) -> (Brought, u64) {
        if self.identity != identity || self.column != column.identity() || self.at > at {
            return (Brought::Broken, 0);
        }
        let Some(steps) = column.steps_between(self.at, at) else {
            return (Brought::Broken, 0);
        };
        let held = Bitmap::of(&self.rows);
        let fresh = rows.andnot(&held);
        let read = fresh.cardinality();
        if read > limit {
            return (Brought::TooMany, 0);
        }
        let mut joined: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        for step in steps {
            for &(row, ordinal) in step.pairs.iter() {
                if held.contains(row) {
                    joined.entry(row).or_default().push(ordinal);
                }
            }
        }
        let mut out = Builder::new();
        let mut fresh = fresh.iter().peekable();
        for (i, &row) in self.rows.iter().enumerate() {
            while let Some(&next) = fresh.peek().filter(|&&next| next < row) {
                out.read(column, next, places);
                fresh.next();
            }
            if !rows.contains(row) {
                continue;
            }
            let (lo, hi) = (self.offsets[i] as usize, self.offsets[i + 1] as usize);
            let added = joined.get(&row).map(Vec::as_slice).unwrap_or(&[]);
            out.push(
                row,
                self.ordinals[lo..hi].iter().chain(added).copied(),
                self.positions[i],
            );
        }
        for next in fresh {
            out.read(column, next, places);
        }
        (
            Brought::Ready(out.finish(identity, column.identity(), at)),
            read,
        )
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
        let mut read: Vec<u32> = Vec::new();
        for row in rows.iter() {
            let (ordinals, position) = match labels.and_then(|labels| labels.of(row)) {
                Some(held) => held,
                None => {
                    read.clear();
                    column.for_each_label(row, |ordinal| read.push(ordinal));
                    (read.as_slice(), place(places, row))
                }
            };
            for &ordinal in ordinals {
                deltas.entry(ordinal).or_default().add(position);
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
    /// where it leaves none. A side no subtracted row reaches is the counted box's. A side one does
    /// reach is read off the artifact's reserve, positions looked up by `position`; `exact` is the
    /// box worked out from the rows themselves, for a side the reserve cannot answer.
    pub(crate) fn bbox(
        &self,
        ordinal: u32,
        counts: &CountsAt,
        reserves: Option<&Reserves>,
        position: &dyn Fn(u32) -> Option<(u32, u32)>,
        exact: impl FnOnce() -> Option<[u32; 4]>,
    ) -> Option<[u32; 4]> {
        let Some(delta) = self.deltas.get(&ordinal) else {
            return counts.bbox(ordinal);
        };
        if counts.placed(ordinal) <= u64::from(delta.placed) {
            return None;
        }
        let counted = counts.bbox(ordinal)?;
        let reached = |side: usize| {
            delta.placed > 0
                && match side {
                    0 | 1 => delta.bbox[side] <= counted[side],
                    _ => delta.bbox[side] >= counted[side],
                }
        };
        if !(0..4).any(reached) {
            return Some(counted);
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
            let mut out = counted;
            for (side, value) in out.iter_mut().enumerate() {
                if !reached(side) {
                    continue;
                }
                let keys = counts.side(ordinal, side, reserves, position)?;
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
