//! A fragment's counts over its base rows, the reserve kept beside them, and the sparse
//! corrections a request applies to them.

use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::row_column::{reserve_key, reserve_row, GrowthStep, LevelAccumulation, RESERVE};

/// An artifact keeps a reserve only above this many placed rows. At or below it, a box a deny
/// touches is worked out from the artifact's members, which are as few.
pub(crate) const RESERVE_MIN_PLACED: u32 = 2 * RESERVE as u32;

/// A reserve slot holding no row.
const NO_ROW: u32 = u32::MAX;

/// What one walk folded up per ordinal, held as the walk produced it.
#[derive(Debug, Default)]
pub(crate) struct Dense {
    /// The level version the walk read.
    pub(crate) filled_at: u64,
    pub(crate) counts: Vec<u32>,
    /// Empty where the walk was given no positions, as are `sums` and `boxes`.
    pub(crate) placed: Vec<u32>,
    pub(crate) sums: Vec<[u64; 2]>,
    pub(crate) boxes: Vec<[u32; 4]>,
}

/// Per artifact with more than [`RESERVE_MIN_PLACED`] placed rows, the [`RESERVE`] most extreme
/// rows the walk counted on each side of its box, most extreme first. Rows alone: a coordinate is
/// looked up when a deny needs it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Reserves {
    /// Per ordinal, where its rows are in `rows`, or `u32::MAX` for none.
    pub(crate) index: Vec<u32>,
    pub(crate) rows: Vec<[[u32; RESERVE]; 4]>,
}

impl Reserves {
    pub(crate) fn weight_bytes(&self) -> u64 {
        (self.index.len() * 4 + self.rows.len() * std::mem::size_of::<[[u32; RESERVE]; 4]>()) as u64
    }

    /// One artifact's rows on one side, most extreme first.
    pub(crate) fn side(&self, ordinal: u32, side: usize) -> Option<impl Iterator<Item = u32> + '_> {
        let at = *self.index.get(ordinal as usize)?;
        let rows = self.rows.get(at as usize)?;
        Some(rows[side].iter().copied().take_while(|&row| row != NO_ROW))
    }
}

impl Dense {
    /// The walk's figures and, where it kept them, its reserves.
    pub(crate) fn of(filled_at: u64, accumulation: LevelAccumulation) -> (Self, Option<Reserves>) {
        let LevelAccumulation {
            counts,
            placed,
            sums,
            boxes,
            reserves,
        } = accumulation;
        let kept = (!reserves.is_empty()).then(|| {
            let mut out = Reserves {
                index: vec![u32::MAX; reserves.len()],
                rows: Vec::new(),
            };
            for (ordinal, reserve) in reserves.iter().enumerate() {
                if placed.get(ordinal).copied().unwrap_or(0) <= RESERVE_MIN_PLACED {
                    continue;
                }
                out.index[ordinal] = out.rows.len() as u32;
                out.rows.push(reserve.map(|side| {
                    side.map(|key| match key {
                        crate::row_column::RESERVE_EMPTY => NO_ROW,
                        key => reserve_row(key),
                    })
                }));
            }
            out
        });
        let dense = Dense {
            filled_at,
            counts,
            placed,
            sums,
            boxes,
        };
        (dense, kept)
    }

    fn weight_bytes(&self) -> u64 {
        (self.counts.len() * 4
            + self.placed.len() * 4
            + self.sums.len() * 16
            + self.boxes.len() * 16) as u64
    }
}

/// What the steps since the walk added to one artifact over the fragment's base rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Grown {
    pub(crate) count: u32,
    pub(crate) placed: u32,
    pub(crate) sums: [u64; 2],
    /// `(row, x, y)` of each placed row added, where the entry serves a box: every one, so a deny
    /// on one of them finds the next extreme among the rest.
    pub(crate) points: Vec<(u32, u32, u32)>,
}

/// A fragment's counts over its base rows at one version of its level: the walk, and what each
/// step since it added.
#[derive(Debug)]
pub(crate) struct CountsAt {
    /// The level version the counts describe.
    pub(crate) at: u64,
    pub(crate) dense: Arc<Dense>,
    pub(crate) grown: Arc<FxHashMap<u32, Grown>>,
}

impl CountsAt {
    pub(crate) fn of(dense: Dense) -> Self {
        CountsAt {
            at: dense.filled_at,
            dense: Arc::new(dense),
            grown: Arc::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn of_counts(at: u64, counts: Vec<u32>) -> Self {
        Self::of(Dense {
            filled_at: at,
            counts,
            ..Dense::default()
        })
    }

    pub(crate) fn weight_bytes(&self) -> u64 {
        self.dense.weight_bytes()
            + self
                .grown
                .values()
                .map(|g| 32 + g.points.len() as u64 * 12)
                .sum::<u64>()
    }

    pub(crate) fn count(&self, ordinal: u32) -> u64 {
        let dense = self
            .dense
            .counts
            .get(ordinal as usize)
            .copied()
            .unwrap_or(0);
        let grown = self.grown.get(&ordinal).map_or(0, |g| g.count);
        u64::from(dense) + u64::from(grown)
    }

    fn dense_placed(&self, ordinal: u32) -> u32 {
        self.dense
            .placed
            .get(ordinal as usize)
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn placed(&self, ordinal: u32) -> u64 {
        let grown = self.grown.get(&ordinal).map_or(0, |g| g.placed);
        u64::from(self.dense_placed(ordinal)) + u64::from(grown)
    }

    pub(crate) fn sums(&self, ordinal: u32) -> [u64; 2] {
        let dense = self
            .dense
            .sums
            .get(ordinal as usize)
            .copied()
            .unwrap_or([0; 2]);
        let grown = self.grown.get(&ordinal).map_or([0; 2], |g| g.sums);
        [dense[0] + grown[0], dense[1] + grown[1]]
    }

    /// The box of every placed row counted, or `None` where none is.
    pub(crate) fn bbox(&self, ordinal: u32) -> Option<[u32; 4]> {
        let mut out = [u32::MAX, u32::MAX, 0, 0];
        let mut any = false;
        if self.dense_placed(ordinal) > 0 {
            if let Some(dense) = self.dense.boxes.get(ordinal as usize) {
                out = *dense;
                any = true;
            }
        }
        if let Some(grown) = self.grown.get(&ordinal) {
            for &(_, x, y) in &grown.points {
                widen(&mut out, x, y);
                any = true;
            }
        }
        any.then_some(out)
    }

    /// The candidates for one side of one artifact's box, as reserve keys, most extreme first:
    /// every row the counts vouch is at least as extreme as any row they leave out. `None` where
    /// the walk's rows of the artifact are more than its reserve holds and no reserve is held.
    pub(crate) fn side(
        &self,
        ordinal: u32,
        side: usize,
        reserves: Option<&Reserves>,
        position: &dyn Fn(u32) -> Option<(u32, u32)>,
    ) -> Option<Vec<u64>> {
        let dense_placed = self.dense_placed(ordinal);
        let mut keys: Vec<u64> = Vec::new();
        let complete = dense_placed == 0;
        if !complete {
            for row in reserves?.side(ordinal, side)? {
                keys.push(reserve_key(side, row, position(row)?));
            }
        }
        let dense_last = keys.last().copied();
        if let Some(grown) = self.grown.get(&ordinal) {
            keys.extend(
                grown
                    .points
                    .iter()
                    .map(|&(row, x, y)| reserve_key(side, row, (x, y))),
            );
            keys.sort_unstable();
        }
        // A reserve vouches only for rows at least as extreme as its last key.
        if let (false, Some(last)) = (complete, dense_last) {
            if dense_placed as usize > RESERVE {
                keys.retain(|&key| key <= last);
            }
        }
        Some(keys)
    }

    /// These counts carried from `self.at` to `to` by `steps`, counting each pair whose row
    /// `counted` admits, placed by `place`. `points` keeps each placed row for a box.
    pub(crate) fn followed(
        &self,
        to: u64,
        steps: &[&GrowthStep],
        counted: impl Fn(u32) -> bool,
        place: impl Fn(u32) -> Option<(u32, u32)>,
        geometry: super::Geometry,
    ) -> CountsAt {
        let mut grown = (*self.grown).clone();
        for step in steps {
            for &(row, ordinal) in step.pairs.iter() {
                if !counted(row) {
                    continue;
                }
                let entry = grown.entry(ordinal).or_default();
                entry.count += 1;
                if geometry == super::Geometry::None {
                    continue;
                }
                if let Some((x, y)) = place(row) {
                    entry.placed += 1;
                    entry.sums[0] += u64::from(x);
                    entry.sums[1] += u64::from(y);
                    if geometry == super::Geometry::Box {
                        entry.points.push((row, x, y));
                    }
                }
            }
        }
        CountsAt {
            at: to,
            dense: Arc::clone(&self.dense),
            grown: Arc::new(grown),
        }
    }
}

/// One artifact's correction: the rows a request subtracts or adds beside the fragment's counts.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Delta {
    pub(crate) count: u32,
    pub(crate) placed: u32,
    pub(crate) sums: [u64; 2],
    /// The box of the placed rows, `[u32::MAX, u32::MAX, 0, 0]` where there are none.
    pub(crate) bbox: [u32; 4],
}

impl Default for Delta {
    fn default() -> Self {
        Delta {
            count: 0,
            placed: 0,
            sums: [0; 2],
            bbox: [u32::MAX, u32::MAX, 0, 0],
        }
    }
}

impl Delta {
    pub(crate) fn add(&mut self, position: Option<(u32, u32)>) {
        self.count += 1;
        if let Some((x, y)) = position {
            self.placed += 1;
            self.sums[0] += u64::from(x);
            self.sums[1] += u64::from(y);
            widen(&mut self.bbox, x, y);
        }
    }
}

/// Per artifact, a correction.
pub(crate) type Deltas = FxHashMap<u32, Delta>;

pub(crate) fn deltas_weight(deltas: &Deltas) -> u64 {
    deltas.len() as u64 * (std::mem::size_of::<Delta>() as u64 + 8)
}

/// Widen `bbox` to hold `(x, y)`.
pub(crate) fn widen(bbox: &mut [u32; 4], x: u32, y: u32) {
    bbox[0] = bbox[0].min(x);
    bbox[1] = bbox[1].min(y);
    bbox[2] = bbox[2].max(x);
    bbox[3] = bbox[3].max(y);
}
