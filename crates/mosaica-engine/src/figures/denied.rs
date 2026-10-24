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

use mosaica_types::ShardId;

use crate::derived::{place, Placement};
use crate::row_column::RowColumn;

use super::counts::{
    deltas_weight, sum_key_row, sum_key_value, CountsAt, Deltas, Reserves, SideRow,
};

/// A row's position, `None` where no segment places it.
pub(crate) type Position = Option<(u32, u32)>;

/// The labels of one view's denied base rows for one level, as flat arrays.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DeniedLabels {
    /// The shard identity the rows are numbered under.
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

/// One side's part of a request's correction: the shard, the rows the mask's `minus` holds below
/// the base there, the labels held of the view's denied base rows, and the side's column and
/// positions, which label and place a row the held labels do not hold.
pub(crate) struct Subtracted<'a> {
    pub(crate) shard: ShardId,
    pub(crate) rows: Bitmap,
    pub(crate) labels: Option<&'a DeniedLabels>,
    pub(crate) column: &'a RowColumn,
    pub(crate) places: &'a [Placement<'a>],
}

/// What a request subtracts from a held sum: every row of the composed mask's `minus` below the
/// base in every side, which is the sides' base rows the viewer may not see.
pub(crate) struct DenyCorrection {
    pub(crate) deltas: Deltas,
    /// Per side, ascending by shard, the rows subtracted.
    rows: Vec<(ShardId, Bitmap)>,
    /// Per artifact a denied row touches, the box of the rows left, worked out on first asking.
    boxes: Mutex<FxHashMap<u32, Option<[u32; 4]>>>,
}

impl DenyCorrection {
    /// The correction for each side's subtracted rows, each labelled from that side's labels where
    /// they hold the row and from its column where they do not.
    pub(crate) fn of<'a>(sides: impl IntoIterator<Item = Subtracted<'a>>) -> Self {
        let mut deltas = Deltas::default();
        let mut read: Vec<u32> = Vec::new();
        let mut rows = Vec::new();
        for side in sides {
            for row in side.rows.iter() {
                let (ordinals, position) = match side.labels.and_then(|labels| labels.of(row)) {
                    Some(held) => held,
                    None => {
                        read.clear();
                        side.column
                            .for_each_label(row, |ordinal| read.push(ordinal));
                        (read.as_slice(), place(side.places, row))
                    }
                };
                for &ordinal in ordinals {
                    deltas.entry(ordinal).or_default().add(position);
                }
            }
            rows.push((side.shard, side.rows));
        }
        rows.sort_by_key(|(shard, _)| *shard);
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
                .iter()
                .map(|(_, rows)| {
                    rows.get_serialized_size_in_bytes::<croaring::Portable>() as u64 + 8
                })
                .sum::<u64>()
    }

    /// The rows subtracted in `shard`'s side, where any are.
    pub(crate) fn rows_of(&self, shard: ShardId) -> Option<&Bitmap> {
        let at = self
            .rows
            .binary_search_by_key(&shard, |(held, _)| *held)
            .ok()?;
        Some(&self.rows[at].1)
    }

    fn subtracts(&self, (shard, row): SideRow) -> bool {
        ShardId::new(shard)
            .ok()
            .and_then(|shard| self.rows_of(shard))
            .is_some_and(|rows| rows.contains(row))
    }

    /// The box of the sum's placed rows of `ordinal` that this does not subtract, or `None` where
    /// it leaves none. A side no subtracted row reaches is the counted box's. A side one does reach
    /// is read off the artifact's reserve, positions looked up by `position`; `exact` is the box
    /// worked out from the rows themselves, for a side the reserve cannot answer.
    pub(crate) fn bbox(
        &self,
        ordinal: u32,
        counts: &CountsAt,
        reserves: Option<&Reserves>,
        position: &dyn Fn(SideRow) -> Option<(u32, u32)>,
        exact: impl FnOnce() -> Option<[u32; 4]>,
    ) -> Option<[u32; 4]> {
        let Some(delta) = self.deltas.get(&ordinal) else {
            return counts.bbox(ordinal);
        };
        if counts.wide(ordinal).placed <= delta.figures.placed {
            return None;
        }
        let counted = counts.bbox(ordinal)?;
        let reached = |side: usize| {
            delta.figures.placed > 0
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
                    .find(|&key| !self.subtracts(sum_key_row(key)))?;
                *value = sum_key_value(side, kept);
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
