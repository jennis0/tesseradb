//! A fragment's counts over its base rows, and the sparse corrections a request applies to them.

use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::row_column::{
    reserve_key, reserve_offer, GrowthStep, LevelAccumulation, Reserve, RESERVE, RESERVE_EMPTY,
};

/// What one walk folded up per ordinal, held as the walk produced it.
#[derive(Debug, Default)]
pub(crate) struct Dense {
    pub(crate) counts: Vec<u32>,
    /// Empty where the walk was given no positions, as are `sums`, `boxes` and `reserves`.
    pub(crate) placed: Vec<u32>,
    pub(crate) sums: Vec<[u64; 2]>,
    pub(crate) boxes: Vec<[u32; 4]>,
    /// Empty unless the layer serves a box.
    pub(crate) reserves: Vec<Reserve>,
}

impl Dense {
    pub(crate) fn of(accumulation: LevelAccumulation) -> Self {
        let LevelAccumulation {
            counts,
            placed,
            sums,
            boxes,
            reserves,
        } = accumulation;
        Dense {
            counts,
            placed,
            sums,
            boxes,
            reserves,
        }
    }

    fn weight_bytes(&self) -> u64 {
        (self.counts.len() * 4
            + self.placed.len() * 4
            + self.sums.len() * 16
            + self.boxes.len() * 16
            + self.reserves.len() * std::mem::size_of::<Reserve>()) as u64
    }
}

/// What the steps since the walk added to one artifact over the fragment's base rows.
#[derive(Debug, Clone, Default)]
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
    pub(crate) fn of(at: u64, dense: Dense) -> Self {
        CountsAt {
            at,
            dense: Arc::new(dense),
            grown: Arc::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn of_counts(at: u64, counts: Vec<u32>) -> Self {
        Self::of(
            at,
            Dense {
                counts,
                ..Dense::default()
            },
        )
    }

    pub(crate) fn weight_bytes(&self) -> u64 {
        self.dense.weight_bytes()
            + self
                .grown
                .values()
                .map(|g| 32 + g.points.len() as u64 * 12)
                .sum::<u64>()
    }

    pub(crate) fn len(&self) -> usize {
        let grown = self
            .grown
            .keys()
            .map(|&o| o as usize + 1)
            .max()
            .unwrap_or(0);
        self.dense.counts.len().max(grown)
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

    pub(crate) fn placed(&self, ordinal: u32) -> u64 {
        let dense = self
            .dense
            .placed
            .get(ordinal as usize)
            .copied()
            .unwrap_or(0);
        let grown = self.grown.get(&ordinal).map_or(0, |g| g.placed);
        u64::from(dense) + u64::from(grown)
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
        if self
            .dense
            .placed
            .get(ordinal as usize)
            .copied()
            .unwrap_or(0)
            > 0
        {
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

    /// Whether this entry keeps reserves, so a deny on an edge can be answered without a walk.
    pub(crate) fn reserves(&self) -> bool {
        !self.dense.reserves.is_empty()
    }

    /// The candidates for one side of one artifact's box, most extreme first, and whether they
    /// are every placed row counted rather than the walk's reserve of them.
    pub(crate) fn side(&self, ordinal: u32, side: usize) -> (Vec<u64>, bool) {
        let dense_placed = self
            .dense
            .placed
            .get(ordinal as usize)
            .copied()
            .unwrap_or(0);
        let mut keys: Vec<u64> = self
            .dense
            .reserves
            .get(ordinal as usize)
            .map(|reserve| {
                reserve[side]
                    .iter()
                    .copied()
                    .take_while(|&key| key != RESERVE_EMPTY)
                    .collect()
            })
            .unwrap_or_default();
        let complete = dense_placed as usize <= RESERVE;
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
        // An incomplete reserve vouches only for rows at least as extreme as its last key.
        if !complete {
            if let Some(last) = dense_last {
                keys.retain(|&key| key <= last);
            }
        }
        (keys, complete)
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

    /// The walk and the steps folded into one, as a walk at this version would have produced it:
    /// what is written to disk.
    pub(crate) fn folded(&self) -> Dense {
        let mut dense = Dense {
            counts: self.dense.counts.clone(),
            placed: self.dense.placed.clone(),
            sums: self.dense.sums.clone(),
            boxes: self.dense.boxes.clone(),
            reserves: self.dense.reserves.clone(),
        };
        let len = self.len();
        let geometry = !dense.placed.is_empty();
        let reserve = !dense.reserves.is_empty();
        dense.counts.resize(len, 0);
        if geometry {
            dense.placed.resize(len, 0);
            dense.sums.resize(len, [0; 2]);
            dense.boxes.resize(len, [u32::MAX, u32::MAX, 0, 0]);
        }
        if reserve {
            dense.reserves.resize(len, [[RESERVE_EMPTY; RESERVE]; 4]);
        }
        for (&ordinal, grown) in self.grown.iter() {
            let i = ordinal as usize;
            dense.counts[i] += grown.count;
            if !geometry {
                continue;
            }
            dense.placed[i] += grown.placed;
            dense.sums[i][0] += grown.sums[0];
            dense.sums[i][1] += grown.sums[1];
            for &(row, x, y) in &grown.points {
                widen(&mut dense.boxes[i], x, y);
                if reserve {
                    for (side, held) in dense.reserves[i].iter_mut().enumerate() {
                        reserve_offer(held, reserve_key(side, row, (x, y)));
                    }
                }
            }
        }
        dense
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
