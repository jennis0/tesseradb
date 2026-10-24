//! What a level's figures hold: one side's walk of its base rows ([`Part`]), the sum of every
//! side's walk ([`Dense`]) and what each side's growth has added since ([`CountsAt`]), the reserve
//! of extreme rows kept beside the sum, and the sparse corrections a request applies to them.

use std::sync::Arc;

use mosaica_types::ShardId;
use rustc_hash::FxHashMap;

use crate::row_column::{reserve_row, GrowthStep, LevelAccumulation, RESERVE, RESERVE_EMPTY};

/// An artifact keeps a reserve only above this many placed rows. At or below it, a box a deny
/// touches is worked out from the artifact's members, which are as few.
pub(crate) const RESERVE_MIN_PLACED: u32 = 2 * RESERVE as u32;

/// A reserve slot holding no row.
pub(crate) const NO_ROW: u32 = u32::MAX;

/// A row of one side, as a reserve names it: the side's shard and the row in its row space.
pub(crate) type SideRow = (u32, u32);

/// A reserve slot holding no row.
const NO_SIDE_ROW: SideRow = (u32::MAX, u32::MAX);

/// One ordinal's figures over several sides, which pass what one side's `u32` and `u64` hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Wide {
    pub(crate) count: u64,
    pub(crate) placed: u64,
    pub(crate) sums: [u128; 2],
}

impl Wide {
    fn of(count: u32, placed: u32, sums: [u64; 2]) -> Self {
        Wide {
            count: u64::from(count),
            placed: u64::from(placed),
            sums: [u128::from(sums[0]), u128::from(sums[1])],
        }
    }

    fn add(&mut self, other: Wide) {
        self.count += other.count;
        self.placed += other.placed;
        self.sums[0] += other.sums[0];
        self.sums[1] += other.sums[1];
    }

    /// `None` where `other` is more than this holds.
    fn sub(&mut self, other: Wide) -> Option<()> {
        self.count = self.count.checked_sub(other.count)?;
        self.placed = self.placed.checked_sub(other.placed)?;
        self.sums[0] = self.sums[0].checked_sub(other.sums[0])?;
        self.sums[1] = self.sums[1].checked_sub(other.sums[1])?;
        Some(())
    }

    /// The same figures in one side's widths, where they fit.
    fn narrow(self) -> Option<(u32, u32, [u64; 2])> {
        Some((
            u32::try_from(self.count).ok()?,
            u32::try_from(self.placed).ok()?,
            [
                u64::try_from(self.sums[0]).ok()?,
                u64::try_from(self.sums[1]).ok()?,
            ],
        ))
    }
}

/// One side's walk of the grant's base rows for a level: every ordinal it counts a row of,
/// ascending, with that ordinal's figures there and, where the walk kept them, the most extreme
/// rows on each side of its box.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Part {
    /// How many ordinals the level held when the side was walked.
    pub(crate) len: u32,
    /// Whether the walk was given positions, and whether it kept reserves.
    pub(crate) positions: bool,
    pub(crate) reserved: bool,
    pub(crate) ordinals: Vec<u32>,
    pub(crate) counts: Vec<u32>,
    /// Empty where the walk was given no positions, as are `sums`, `boxes` and `reserves`.
    pub(crate) placed: Vec<u32>,
    pub(crate) sums: Vec<[u64; 2]>,
    pub(crate) boxes: Vec<[u32; 4]>,
    /// Per entry, up to [`RESERVE`] rows a side, most extreme first and [`NO_ROW`] past the last.
    /// Empty where the walk kept none.
    pub(crate) reserves: Vec<[[u32; RESERVE]; 4]>,
}

impl Part {
    /// The walk's figures, sparse. `positions` says whether the walk was given positions.
    pub(crate) fn of(accumulation: LevelAccumulation, positions: bool) -> Self {
        let LevelAccumulation {
            counts,
            placed,
            sums,
            boxes,
            reserves,
        } = accumulation;
        let mut out = Part {
            len: counts.len() as u32,
            positions,
            reserved: reserves.is_some(),
            ..Part::default()
        };
        let mut kept = reserves.map(|r| r.into_iter().peekable());
        for (ordinal, &count) in counts.iter().enumerate() {
            if count == 0 {
                continue;
            }
            out.ordinals.push(ordinal as u32);
            out.counts.push(count);
            if positions {
                out.placed.push(placed[ordinal]);
                out.sums.push(sums[ordinal]);
                out.boxes.push(boxes[ordinal]);
            }
            if let Some(kept) = &mut kept {
                let mut rows = [[NO_ROW; RESERVE]; 4];
                if kept.peek().is_some_and(|(at, _)| *at as usize == ordinal) {
                    let (_, reserve) = kept.next().expect("peeked");
                    rows = reserve.map(|side| {
                        side.map(|key| match key {
                            RESERVE_EMPTY => NO_ROW,
                            key => reserve_row(key),
                        })
                    });
                }
                out.reserves.push(rows);
            }
        }
        out
    }

    /// Where `ordinal` is among the entries.
    pub(crate) fn find(&self, ordinal: u32) -> Option<usize> {
        self.ordinals.binary_search(&ordinal).ok()
    }

    fn wide(&self, i: usize) -> Wide {
        Wide::of(
            self.counts[i],
            self.placed.get(i).copied().unwrap_or(0),
            self.sums.get(i).copied().unwrap_or([0; 2]),
        )
    }

    /// Entry `i`'s placed rows on `side` that its walk kept, most extreme first.
    fn reserve(&self, i: usize, side: usize) -> impl Iterator<Item = u32> + '_ {
        self.reserves
            .get(i)
            .into_iter()
            .flat_map(move |rows| rows[side].iter().copied().take_while(|&row| row != NO_ROW))
    }
}

/// The sum of every side's walk, per ordinal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Dense {
    pub(crate) counts: Vec<u32>,
    /// Empty where the walks were given no positions, as are `sums` and `boxes`.
    pub(crate) placed: Vec<u32>,
    pub(crate) sums: Vec<[u64; 2]>,
    /// The box of every placed row the walks counted, `[u32::MAX, u32::MAX, 0, 0]` where none is.
    pub(crate) boxes: Vec<[u32; 4]>,
    /// The ordinals whose sum passes what the arrays hold, held whole in place of their slots.
    pub(crate) wide: FxHashMap<u32, Wide>,
}

/// A box holding no row.
pub(crate) const EMPTY_BOX: [u32; 4] = [u32::MAX, u32::MAX, 0, 0];

impl Dense {
    /// Nothing counted, over `len` ordinals, with room for positions where `positions` says.
    pub(crate) fn empty(len: usize, positions: bool) -> Self {
        let placed = if positions { len } else { 0 };
        Dense {
            counts: vec![0; len],
            placed: vec![0; placed],
            sums: vec![[0; 2]; placed],
            boxes: vec![EMPTY_BOX; placed],
            wide: FxHashMap::default(),
        }
    }

    /// Figures holding `counts` alone.
    #[cfg(test)]
    pub(crate) fn of_counts(counts: Vec<u32>) -> Self {
        Dense {
            counts,
            ..Dense::default()
        }
    }

    pub(crate) fn positions(&self) -> bool {
        !self.placed.is_empty()
    }

    fn grow_to(&mut self, len: usize) {
        if self.counts.len() < len {
            self.counts.resize(len, 0);
            if self.positions() {
                self.placed.resize(len, 0);
                self.sums.resize(len, [0; 2]);
                self.boxes.resize(len, EMPTY_BOX);
            }
        }
    }

    /// One ordinal's figures whole.
    pub(crate) fn wide(&self, ordinal: u32) -> Wide {
        if !self.wide.is_empty() {
            if let Some(wide) = self.wide.get(&ordinal) {
                return *wide;
            }
        }
        let i = ordinal as usize;
        Wide::of(
            self.counts.get(i).copied().unwrap_or(0),
            self.placed.get(i).copied().unwrap_or(0),
            self.sums.get(i).copied().unwrap_or([0; 2]),
        )
    }

    pub(crate) fn count(&self, ordinal: u32) -> u64 {
        if !self.wide.is_empty() {
            if let Some(wide) = self.wide.get(&ordinal) {
                return wide.count;
            }
        }
        u64::from(self.counts.get(ordinal as usize).copied().unwrap_or(0))
    }

    pub(crate) fn placed(&self, ordinal: u32) -> u64 {
        self.wide(ordinal).placed
    }

    /// The box of the placed rows counted, where there are any.
    pub(crate) fn bbox(&self, ordinal: u32) -> Option<[u32; 4]> {
        (self.placed(ordinal) > 0)
            .then(|| self.boxes.get(ordinal as usize).copied())
            .flatten()
    }

    fn set(&mut self, ordinal: u32, figures: Wide) {
        let i = ordinal as usize;
        match figures.narrow() {
            Some((count, placed, sums)) => {
                self.wide.remove(&ordinal);
                self.counts[i] = count;
                if self.positions() {
                    self.placed[i] = placed;
                    self.sums[i] = sums;
                }
            }
            None => {
                self.counts[i] = 0;
                if self.positions() {
                    self.placed[i] = 0;
                    self.sums[i] = [0; 2];
                }
                self.wide.insert(ordinal, figures);
            }
        }
    }

    /// Add `part`'s counts, placed counts and sums. Its boxes are not added: see
    /// [`super::sum`].
    pub(crate) fn add(&mut self, part: &Part) {
        self.grow_to(part.len as usize);
        for (i, &ordinal) in part.ordinals.iter().enumerate() {
            let mut figures = self.wide(ordinal);
            figures.add(part.wide(i));
            self.set(ordinal, figures);
        }
    }

    /// Take `part`'s counts, placed counts and sums away. `None` where it holds more than is here,
    /// which no part added here does.
    pub(crate) fn sub(&mut self, part: &Part) -> Option<()> {
        for (i, &ordinal) in part.ordinals.iter().enumerate() {
            if ordinal as usize >= self.counts.len() {
                return None;
            }
            let mut figures = self.wide(ordinal);
            figures.sub(part.wide(i))?;
            self.set(ordinal, figures);
        }
        Some(())
    }

    /// Widen ordinal `ordinal`'s box by `bbox`.
    pub(crate) fn widen_box(&mut self, ordinal: u32, bbox: [u32; 4]) {
        if let Some(held) = self.boxes.get_mut(ordinal as usize) {
            widen(held, bbox[0], bbox[1]);
            widen(held, bbox[2], bbox[3]);
        }
    }

    pub(crate) fn weight_bytes(&self) -> u64 {
        (self.counts.len() * 4
            + self.placed.len() * 4
            + self.sums.len() * 16
            + self.boxes.len() * 16
            + self.wide.len() * 56) as u64
    }
}

/// Per artifact with more than [`RESERVE_MIN_PLACED`] placed rows over every side, the [`RESERVE`]
/// most extreme rows the walks counted on each side of its box, most extreme first, whichever side
/// they are in. Rows alone: a coordinate is looked up when a deny needs it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Reserves {
    /// Per ordinal, where its rows are in `rows`, or `u32::MAX` for none.
    pub(crate) index: Vec<u32>,
    pub(crate) rows: Vec<[[SideRow; RESERVE]; 4]>,
}

impl Reserves {
    pub(crate) fn weight_bytes(&self) -> u64 {
        (self.index.len() * 4 + self.rows.len() * std::mem::size_of::<[[SideRow; RESERVE]; 4]>())
            as u64
    }

    /// One artifact's rows on one side, most extreme first.
    pub(crate) fn side(
        &self,
        ordinal: u32,
        side: usize,
    ) -> Option<impl Iterator<Item = SideRow> + '_> {
        let at = *self.index.get(ordinal as usize)?;
        let rows = self.rows.get(at as usize)?;
        Some(
            rows[side]
                .iter()
                .copied()
                .take_while(|&row| row != NO_SIDE_ROW),
        )
    }

    /// Every row named on any side of `ordinal`'s reserve.
    fn named(&self, ordinal: u32) -> impl Iterator<Item = SideRow> + '_ {
        (0..4).flat_map(move |side| self.side(ordinal, side).into_iter().flatten())
    }

    /// Whether `ordinal`'s reserve names a row of `shard`.
    pub(crate) fn names_shard(&self, ordinal: u32, shard: ShardId) -> bool {
        self.named(ordinal).any(|(held, _)| held == shard.raw())
    }

    /// Hold `rows` as `ordinal`'s reserve, or none.
    pub(crate) fn set(&mut self, ordinal: u32, rows: Option<[[SideRow; RESERVE]; 4]>) {
        let i = ordinal as usize;
        if self.index.len() <= i {
            self.index.resize(i + 1, u32::MAX);
        }
        match (rows, self.index[i]) {
            (None, _) => self.index[i] = u32::MAX,
            (Some(rows), u32::MAX) => {
                self.index[i] = self.rows.len() as u32;
                self.rows.push(rows);
            }
            (Some(rows), at) => self.rows[at as usize] = rows,
        }
    }

    /// The same reserves without the slots no ordinal names.
    pub(crate) fn compacted(self) -> Self {
        let mut out = Reserves {
            index: vec![u32::MAX; self.index.len()],
            rows: Vec::new(),
        };
        for (ordinal, &at) in self.index.iter().enumerate() {
            if at != u32::MAX {
                out.index[ordinal] = out.rows.len() as u32;
                out.rows.push(self.rows[at as usize]);
            }
        }
        out
    }
}

/// The reserve key of `shard`'s `row` at `(x, y)` on `side`: smaller is more extreme, the row's
/// shard and then the row breaking ties, so every side's rows order alike.
#[inline]
pub(crate) fn sum_key(side: usize, (shard, row): SideRow, (x, y): (u32, u32)) -> u128 {
    let along = match side {
        0 => x,
        1 => y,
        2 => u32::MAX - x,
        _ => u32::MAX - y,
    };
    (u128::from(along) << 64) | (u128::from(shard) << 32) | u128::from(row)
}

/// The coordinate on `side` a [`sum_key`] holds.
#[inline]
pub(crate) fn sum_key_value(side: usize, key: u128) -> u32 {
    let along = (key >> 64) as u32;
    match side {
        0 | 1 => along,
        _ => u32::MAX - along,
    }
}

/// The row a [`sum_key`] names.
#[inline]
pub(crate) fn sum_key_row(key: u128) -> SideRow {
    ((key >> 32) as u32, key as u32)
}

/// The [`RESERVE`] most extreme of `offered` on `side`, as slots, positions looked up by
/// `position`. A row no segment places is passed over, as a walk passes over it.
pub(crate) fn reserve_of(
    side: usize,
    offered: impl Iterator<Item = SideRow>,
    position: &dyn Fn(SideRow) -> Option<(u32, u32)>,
) -> [SideRow; RESERVE] {
    let mut keys: Vec<u128> = offered
        .filter_map(|row| Some(sum_key(side, row, position(row)?)))
        .collect();
    keys.sort_unstable();
    keys.dedup();
    let mut out = [NO_SIDE_ROW; RESERVE];
    for (slot, key) in out.iter_mut().zip(keys) {
        *slot = sum_key_row(key);
    }
    out
}

/// Every row `parts` keep on any side of `ordinal`'s reserve, each with the shard of its part.
pub(crate) fn kept_rows<'a>(
    parts: impl Iterator<Item = (ShardId, &'a Part)> + 'a,
    ordinal: u32,
    side: usize,
) -> impl Iterator<Item = SideRow> + 'a {
    parts.flat_map(move |(shard, part)| {
        part.find(ordinal)
            .into_iter()
            .flat_map(move |i| part.reserve(i, side).map(move |row| (shard.raw(), row)))
    })
}

/// What one side's growth since its walk added to one artifact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SideGrown {
    pub(crate) count: u32,
    pub(crate) placed: u32,
    pub(crate) sums: [u64; 2],
}

/// What every side's growth since its walk added to one artifact.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Grown {
    pub(crate) figures: Wide,
    /// `(shard, row, x, y)` of each placed row added, where the level serves a box: every one, so
    /// a deny on one of them finds the next extreme among the rest.
    pub(crate) points: Vec<(u32, u32, u32, u32)>,
}

/// One side of a sum: what its walk read and how far its growth has been followed since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SideAt {
    pub(crate) shard: ShardId,
    /// The shard identity the side's base rows are numbered under. A compaction rotates it.
    pub(crate) identity: [u8; 32],
    /// The level version the side's walk read, and the version its growth has been followed to.
    pub(crate) filled_at: u64,
    pub(crate) at: u64,
    pub(crate) grown: Arc<FxHashMap<u32, SideGrown>>,
}

/// A level's sum over every side at one version of each side's form: the walks, and what each
/// side's growth since its walk added.
#[derive(Debug, Clone)]
pub(crate) struct CountsAt {
    /// Ascending by shard.
    pub(crate) sides: Arc<[SideAt]>,
    pub(crate) dense: Arc<Dense>,
    pub(crate) grown: Arc<FxHashMap<u32, Grown>>,
}

impl CountsAt {
    /// The sum of walks over `sides`, nothing grown since.
    pub(crate) fn of(sides: Vec<SideAt>, dense: Dense) -> Self {
        CountsAt {
            sides: sides.into(),
            dense: Arc::new(dense),
            grown: Arc::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn of_counts(counts: Vec<u32>) -> Self {
        Self::of(Vec::new(), Dense::of_counts(counts))
    }

    /// Each side's version, in the order of the sides.
    pub(crate) fn ats(&self) -> Vec<u64> {
        self.sides.iter().map(|side| side.at).collect()
    }

    pub(crate) fn weight_bytes(&self) -> u64 {
        let grown: u64 = self
            .grown
            .values()
            .map(|g| 72 + g.points.len() as u64 * 16)
            .sum();
        let sides: u64 = self
            .sides
            .iter()
            .map(|side| 96 + side.grown.len() as u64 * 32)
            .sum();
        self.dense.weight_bytes() + grown + sides
    }

    pub(crate) fn wide(&self, ordinal: u32) -> Wide {
        let mut figures = self.dense.wide(ordinal);
        if let Some(grown) = self.grown.get(&ordinal) {
            figures.add(grown.figures);
        }
        figures
    }

    pub(crate) fn count(&self, ordinal: u32) -> u64 {
        self.dense.count(ordinal) + self.grown.get(&ordinal).map_or(0, |g| g.figures.count)
    }

    /// The box of every placed row counted, or `None` where none is.
    pub(crate) fn bbox(&self, ordinal: u32) -> Option<[u32; 4]> {
        let mut out = self.dense.bbox(ordinal).unwrap_or(EMPTY_BOX);
        let mut any = out != EMPTY_BOX;
        if let Some(grown) = self.grown.get(&ordinal) {
            for &(_, _, x, y) in &grown.points {
                widen(&mut out, x, y);
                any = true;
            }
        }
        any.then_some(out)
    }

    /// The candidates for one side of one artifact's box, as [`sum_key`]s, most extreme first:
    /// every row the counts vouch is at least as extreme as any row they leave out. `None` where
    /// the walks' rows of the artifact are more than its reserve holds and no reserve is held.
    pub(crate) fn side(
        &self,
        ordinal: u32,
        side: usize,
        reserves: Option<&Reserves>,
        position: &dyn Fn(SideRow) -> Option<(u32, u32)>,
    ) -> Option<Vec<u128>> {
        let dense_placed = self.dense.placed(ordinal);
        let mut keys: Vec<u128> = Vec::new();
        let complete = dense_placed == 0;
        if !complete {
            for row in reserves?.side(ordinal, side)? {
                keys.push(sum_key(side, row, position(row)?));
            }
        }
        let dense_last = keys.last().copied();
        if let Some(grown) = self.grown.get(&ordinal) {
            keys.extend(
                grown
                    .points
                    .iter()
                    .map(|&(shard, row, x, y)| sum_key(side, (shard, row), (x, y))),
            );
            keys.sort_unstable();
        }
        // A reserve vouches only for rows at least as extreme as its last key.
        if let (false, Some(last)) = (complete, dense_last) {
            if dense_placed > RESERVE as u64 {
                keys.retain(|&key| key <= last);
            }
        }
        Some(keys)
    }

    /// These counts with each side `follow` names carried from its version to the one it gives by
    /// that side's steps, counting each pair whose row `counted` admits, placed by `place`.
    /// `points` keeps each placed row for a box.
    pub(crate) fn followed(&self, follow: &[Follow<'_>], geometry: super::Geometry) -> CountsAt {
        let mut sides: Vec<SideAt> = self.sides.to_vec();
        let mut grown = (*self.grown).clone();
        for follow in follow {
            let side = &mut sides[follow.place];
            let mut side_grown = (*side.grown).clone();
            let shard = side.shard.raw();
            for step in &follow.steps {
                for &(row, ordinal) in step.pairs.iter() {
                    if !(follow.counted)(row) {
                        continue;
                    }
                    let mine = side_grown.entry(ordinal).or_default();
                    let all = grown.entry(ordinal).or_default();
                    mine.count += 1;
                    all.figures.count += 1;
                    if geometry == super::Geometry::None {
                        continue;
                    }
                    if let Some((x, y)) = (follow.place_of)(row) {
                        mine.placed += 1;
                        mine.sums[0] += u64::from(x);
                        mine.sums[1] += u64::from(y);
                        all.figures.placed += 1;
                        all.figures.sums[0] += u128::from(x);
                        all.figures.sums[1] += u128::from(y);
                        if geometry == super::Geometry::Box {
                            all.points.push((shard, row, x, y));
                        }
                    }
                }
            }
            side.at = follow.to;
            side.grown = Arc::new(side_grown);
        }
        CountsAt {
            sides: sides.into(),
            dense: Arc::clone(&self.dense),
            grown: Arc::new(grown),
        }
    }

    /// The growth `shard`'s side has added taken away. `None` where the sum holds less than that
    /// side's growth added, which no sum this side's growth was followed into does.
    pub(crate) fn without_growth_of(
        grown: &mut FxHashMap<u32, Grown>,
        side: &SideAt,
    ) -> Option<()> {
        for (ordinal, mine) in side.grown.iter() {
            let all = grown.get_mut(ordinal)?;
            all.figures
                .sub(Wide::of(mine.count, mine.placed, mine.sums))?;
            all.points.retain(|point| point.0 != side.shard.raw());
            if all.figures == Wide::default() {
                grown.remove(ordinal);
            }
        }
        Some(())
    }
}

/// One side's move to a newer version of its form: [`CountsAt::followed`].
pub(crate) struct Follow<'a> {
    pub(crate) place: usize,
    pub(crate) to: u64,
    pub(crate) steps: Vec<&'a GrowthStep>,
    pub(crate) counted: &'a dyn Fn(u32) -> bool,
    pub(crate) place_of: &'a dyn Fn(u32) -> Option<(u32, u32)>,
}

/// One artifact's correction: the rows a request subtracts or adds beside the sum.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Delta {
    pub(crate) figures: Wide,
    /// The box of the placed rows, `[u32::MAX, u32::MAX, 0, 0]` where there are none.
    pub(crate) bbox: [u32; 4],
}

impl Default for Delta {
    fn default() -> Self {
        Delta {
            figures: Wide::default(),
            bbox: EMPTY_BOX,
        }
    }
}

impl Delta {
    pub(crate) fn add(&mut self, position: Option<(u32, u32)>) {
        self.figures.count += 1;
        if let Some((x, y)) = position {
            self.figures.placed += 1;
            self.figures.sums[0] += u128::from(x);
            self.figures.sums[1] += u128::from(y);
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

/// `a` widened by `b`, either of them empty.
pub(crate) fn union(a: [u32; 4], b: [u32; 4]) -> [u32; 4] {
    let mut out = a;
    if b != EMPTY_BOX {
        widen(&mut out, b[0], b[1]);
        widen(&mut out, b[2], b[3]);
    }
    out
}
