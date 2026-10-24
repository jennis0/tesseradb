//! A level's sum over every side for one grant, and how it moves when a side does.
//!
//! The sum is every side's walk of the grant's base rows added together, with what each side's
//! growth has added since ([`CountsAt`]). A growth or a publication is followed by each side's
//! steps. Where a side's walk is replaced, because a compaction rotated its shard's identity, its
//! column's steps do not lead to the version a request reads, or it opened, the sum is taken again
//! over every side's walk: a side kept is read back from the cache directory at the version its
//! walk read, its growth since kept as it was, and a side replaced is walked. A side kept whose
//! walk is not on disc is walked again too. A box's side is an extreme over every side, so it is
//! taken again with the counts rather than corrected.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use croaring::Bitmap;
use mosaica_types::ShardId;

use crate::derived::{place, Placement};
use crate::error::Result;
use crate::row_column::RowColumn;
use crate::viewport::ServedView;
use crate::Engine;

use super::cache::{Build, FiguresKey, HeldSum, SumEntry, SumKey};
use super::counts::{
    kept_rows, reserve_of, CountsAt, Dense, Follow, Part, Reserves, SideAt, SideRow,
    RESERVE_MIN_PLACED,
};
use super::{persist, wait_error, Geometry};

/// One side of a sum as a request reads it.
pub(super) struct Want<'a> {
    pub(super) shard: ShardId,
    /// The shard identity the side's fragment was unioned against.
    pub(super) identity: [u8; 32],
    pub(super) column: &'a Arc<RowColumn>,
    /// The version of the level the side's form is of.
    pub(super) at: u64,
    pub(super) base_rows: u32,
    /// The grant's projection in the side, whose rows below `base_rows` are the side's base rows.
    pub(super) fragment: &'a Bitmap,
    pub(super) places: Vec<Placement<'a>>,
}

impl Engine {
    /// The sum `key` names at the versions `wants` read, and its reserve: held, read back,
    /// taken again over every side or walked. `None` where what is held cannot be brought to
    /// those versions, which a request still reading a form older than the newest held meets.
    pub(super) fn summed(
        &self,
        served: &ServedView<'_>,
        key: &SumKey,
        wants: &[Want<'_>],
    ) -> Result<Option<HeldSum>> {
        let geometry = key.geometry;
        let stem = persist::counts_stem(&key.terms, &key.view, key.space, key.level, geometry);
        let sums = self.figures.dir.as_deref().map(persist::sums_dir);
        let address = key.address();
        let held = self
            .figures
            .get_or_build(
                FiguresKey::Sum(key.clone()),
                &served.turn,
                served.cancel.as_ref(),
                |build| {
                    let mut loaded = false;
                    let predecessor = self.figures.latest(&address).or_else(|| {
                        let (counts, reserves) =
                            persist::read_sum(sums.as_deref()?, &stem, geometry)?;
                        loaded = true;
                        Some((Arc::new(counts), reserves.map(Arc::new)))
                    });
                    let held = predecessor
                        .as_ref()
                        .map(|(counts, reserves)| (&**counts, reserves.as_deref()));
                    let (counts, reserves, changed) =
                        self.sum_over(held, wants, &stem, geometry, build)?;
                    if loaded {
                        self.figures.counters.loads.fetch_add(1, Ordering::Relaxed);
                    }
                    let on_disk = loaded && !changed;
                    let reserves = self.within_bound(&counts, reserves).map(Arc::new);
                    let written = sums.clone().map(|dir| (dir, stem.clone(), on_disk));
                    if let (false, Some(dir)) = (on_disk, sums.clone()) {
                        let (newest, reserves) = (counts.clone(), reserves.clone());
                        let (stem, bound, root) = (
                            stem.clone(),
                            Arc::clone(&self.figures.disk_bound),
                            self.figures.dir.clone(),
                        );
                        self.figures.worker.submit(move || {
                            persist::write_sum(&dir, &stem, &newest, reserves.as_deref());
                            if let Some(root) = root {
                                persist::hold_under(&root, bound.load(Ordering::Relaxed));
                            }
                        });
                    }
                    Some(SumEntry::new(counts, reserves, written))
                },
            )
            .map_err(wait_error)?;
        self.figures.set_latest(address, &held);
        let ats: Vec<u64> = wants.iter().map(|want| want.at).collect();
        let counts = held.at(&ats, |from| followed(from, wants, geometry));
        if counts.is_some() {
            self.write_newest(&held);
        }
        Ok(counts.map(|counts| (counts, held.reserves.clone())))
    }

    /// `reserves`, unless with `counts` they pass the bound the entry is held under: then the
    /// entry keeps its counts and goes without.
    fn within_bound(&self, counts: &CountsAt, reserves: Option<Reserves>) -> Option<Reserves> {
        let reserves = reserves?;
        if counts.weight_bytes() + reserves.weight_bytes() > self.figures.bound_bytes() {
            self.figures
                .counters
                .reserves_left_out
                .fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(reserves)
    }

    /// The sum over the sides `wants` read, from `held` where it is over them, and whether it
    /// differs from `held`. A side of `held` whose identity is the same and whose steps lead to
    /// the version asked is kept: its walk is read back and its growth kept, to be followed by its
    /// steps. Every other side is walked. `None` where a walk was abandoned.
    fn sum_over(
        &self,
        held: Option<(&CountsAt, Option<&Reserves>)>,
        wants: &[Want<'_>],
        stem: &str,
        geometry: Geometry,
        build: &Build<'_>,
    ) -> Option<(CountsAt, Option<Reserves>, bool)> {
        let kept: Vec<Option<&SideAt>> = wants
            .iter()
            .map(|want| {
                let (counts, _) = held?;
                let at = counts
                    .sides
                    .binary_search_by_key(&want.shard, |side| side.shard)
                    .ok()?;
                let side = &counts.sides[at];
                let follows = side.at == want.at
                    || (side.at < want.at && want.column.steps_between(side.at, want.at).is_some());
                (side.identity == want.identity && follows).then_some(side)
            })
            .collect();
        if let Some((counts, reserves)) = held {
            if counts.sides.len() == wants.len() && kept.iter().all(Option::is_some) {
                return Some((counts.clone(), reserves.cloned(), false));
            }
        }
        let several = wants.len() > 1;
        let mut sides = Vec::with_capacity(wants.len());
        let mut parts = Vec::with_capacity(wants.len());
        for (want, kept) in wants.iter().zip(&kept) {
            match kept.and_then(|side| Some((side, self.read_part(side, stem, geometry)?))) {
                Some((side, part)) => {
                    sides.push(side.clone());
                    parts.push(part);
                }
                None => {
                    parts.push(self.part_of(want, stem, geometry, build, several)?);
                    sides.push(SideAt {
                        shard: want.shard,
                        identity: want.identity,
                        filled_at: want.at,
                        at: want.at,
                        grown: Arc::default(),
                    });
                }
            }
        }
        let mut grown = held.map_or_else(Default::default, |(counts, _)| (*counts.grown).clone());
        if let Some((counts, _)) = held {
            for side in counts.sides.iter().filter(|side| !sides.contains(side)) {
                if CountsAt::without_growth_of(&mut grown, side).is_none() {
                    return self.sum_over(None, wants, stem, geometry, build);
                }
            }
        }
        let every: Vec<(ShardId, &Part)> =
            sides.iter().map(|side| side.shard).zip(&parts).collect();
        let position = |row: SideRow| position_in(wants, row);
        let (dense, reserves) = sum_of(
            &every,
            geometry != Geometry::None,
            (geometry == Geometry::Box).then_some(&position as &dyn Fn(SideRow) -> _),
        );
        let counts = CountsAt {
            sides: sides.into(),
            dense: Arc::new(dense),
            grown: Arc::new(grown),
        };
        Some((counts, reserves, true))
    }

    /// `want`'s walk at its version: read back, or walked and, where the sum has several sides,
    /// written. `None` where the walk was abandoned.
    fn part_of(
        &self,
        want: &Want<'_>,
        stem: &str,
        geometry: Geometry,
        build: &Build<'_>,
        write: bool,
    ) -> Option<Part> {
        let dir = self
            .figures
            .dir
            .as_deref()
            .map(|root| persist::identity_dir(root, &want.identity));
        if let Some(part) = dir
            .as_deref()
            .and_then(|dir| persist::read_part(dir, stem, want.at, geometry))
        {
            self.figures.counters.loads.fetch_add(1, Ordering::Relaxed);
            return Some(part);
        }
        self.figures.counters.fills.fetch_add(1, Ordering::Relaxed);
        let positions = (geometry != Geometry::None).then_some(want.places.as_slice());
        let walked = self.count_pool.install(|| {
            #[cfg(feature = "fault-injection")]
            self.switches.hold_masked_count_build_if_wanted();
            want.column.accumulate_below(
                want.fragment,
                want.base_rows,
                positions,
                geometry == Geometry::Box,
                &|| build.give_way(),
            )
        })?;
        let part = Part::of(walked);
        if let (true, Some(dir)) = (write, dir) {
            let (written, stem, at) = (part.clone(), stem.to_string(), want.at);
            let (bound, root) = (
                Arc::clone(&self.figures.disk_bound),
                self.figures.dir.clone(),
            );
            self.figures.worker.submit(move || {
                persist::write_part(&dir, &stem, at, &written, geometry);
                if let Some(root) = root {
                    persist::hold_under(&root, bound.load(Ordering::Relaxed));
                }
            });
        }
        Some(part)
    }

    /// The walk `side` was summed from, read back.
    fn read_part(&self, side: &SideAt, stem: &str, geometry: Geometry) -> Option<Part> {
        let root = self.figures.dir.as_deref()?;
        let part = persist::read_part(
            &persist::identity_dir(root, &side.identity),
            stem,
            side.filled_at,
            geometry,
        )?;
        self.figures.counters.loads.fetch_add(1, Ordering::Relaxed);
        Some(part)
    }
}

/// The sum of `parts`, each by the shard of its side: counts, placed counts and sums added, and
/// where `positions` says, each box the union of the parts'. Where `position` is given, the
/// reserve of every artifact with more than [`RESERVE_MIN_PLACED`] placed rows, a row's position
/// looked up by it.
pub(super) fn sum_of(
    parts: &[(ShardId, &Part)],
    positions: bool,
    position: Option<&dyn Fn(SideRow) -> Option<(u32, u32)>>,
) -> (Dense, Option<Reserves>) {
    let len = parts
        .iter()
        .map(|(_, part)| part.len as usize)
        .max()
        .unwrap_or(0);
    let mut dense = Dense::empty(len, positions);
    for (_, part) in parts {
        dense.add(part);
        if positions {
            for (at, &ordinal) in part.ordinals.iter().enumerate() {
                if part.placed[at] > 0 {
                    dense.widen_box(ordinal, part.boxes[at]);
                }
            }
        }
    }
    let reserves = position.map(|position| {
        let shards: Vec<ShardId> = parts.iter().map(|(shard, _)| *shard).collect();
        let mut out = Reserves::over(len, &shards);
        for ordinal in 0..len as u32 {
            if dense.placed(ordinal) > u64::from(RESERVE_MIN_PLACED) {
                let rows = std::array::from_fn(|side| {
                    reserve_of(
                        side,
                        kept_rows(parts.iter().copied(), ordinal, side),
                        position,
                    )
                });
                out.set(ordinal, rows);
            }
        }
        out
    });
    (dense, reserves)
}

/// `from` brought to the versions `wants` read by each side's steps, `None` where a side's steps
/// do not lead there.
fn followed(from: &CountsAt, wants: &[Want<'_>], geometry: Geometry) -> Option<CountsAt> {
    if from.sides.len() != wants.len() {
        return None;
    }
    let mut follows = Vec::new();
    for (at, (side, want)) in from.sides.iter().zip(wants).enumerate() {
        if side.shard != want.shard {
            return None;
        }
        if side.at == want.at {
            continue;
        }
        follows.push(Follow {
            place: at,
            to: want.at,
            steps: want.column.steps_between(side.at, want.at)?,
            base_rows: want.base_rows,
            fragment: want.fragment,
            places: &want.places,
        });
    }
    Some(from.followed(&follows, geometry))
}

/// Where `row` of the side of its shard is, among `wants`.
fn position_in(wants: &[Want<'_>], (shard, row): SideRow) -> Option<(u32, u32)> {
    let at = wants
        .binary_search_by_key(&shard, |want| want.shard.raw())
        .ok()?;
    place(&wants[at].places, row)
}

impl Engine {
    /// Write `held`'s newest sum where it is newer than the one written, no write of it is under
    /// way, and the last was at least [`super::COUNTS_WRITE_INTERVAL`] ago.
    pub(super) fn write_newest(&self, held: &Arc<SumEntry>) {
        let Some(written) = &held.written else {
            return;
        };
        let newest = held.newest();
        if *written
            .ats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            == newest.ats()
        {
            return;
        }
        {
            let mut last = written
                .last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if last.is_some_and(|last| last.elapsed() < super::COUNTS_WRITE_INTERVAL)
                || written.writing.swap(true, Ordering::AcqRel)
            {
                return;
            }
            *last = Some(std::time::Instant::now());
        }
        let held = Arc::clone(held);
        let bound = Arc::clone(&self.figures.disk_bound);
        let root = self.figures.dir.clone();
        self.figures.worker.submit(move || {
            let Some(written) = &held.written else { return };
            let newest = held.newest();
            persist::write_sum(
                &written.dir,
                &written.stem,
                &newest,
                held.reserves.as_deref(),
            );
            *written
                .ats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = newest.ats();
            written.writing.store(false, Ordering::Release);
            if let Some(root) = root {
                persist::hold_under(&root, bound.load(Ordering::Relaxed));
            }
        });
    }
}
