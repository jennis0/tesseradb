//! A level's sum over every side for one grant, and how it moves when a side does.
//!
//! The sum is every side's walk of the grant's base rows added together, with what each side's
//! growth has added since ([`CountsAt`]). A growth or a publication is followed by each side's
//! steps. A side whose walk is replaced, because a compaction rotated its shard's identity, its
//! column's steps do not lead to the version a request reads, or it opened, moves the sum by that
//! side's difference: its old walk, read back from the cache directory, taken away, and its new
//! walk added. Counts, placed counts and position sums add and subtract exactly. A side of a box
//! is an extreme and cannot be taken away, so an artifact whose box a replaced walk reached, or
//! whose reserve named a row of it, has its box and reserve worked out again from every side's
//! walk, read back. Where a walk a difference needs is not on disk, the sum is made again from
//! every side's walk, read back or walked.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use croaring::Bitmap;
use mosaica_types::ShardId;
use rustc_hash::FxHashMap;

use crate::derived::{place, Placement};
use crate::error::Result;
use crate::row_column::RowColumn;
use crate::viewport::ServedView;
use crate::Engine;

use super::cache::{Build, FiguresKey, HeldSum, SumEntry, SumKey};
use super::counts::{
    kept_rows, reserve_of, union, CountsAt, Dense, Follow, Grown, Part, Reserves, SideAt, SideRow,
    EMPTY_BOX, RESERVE_MIN_PLACED,
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

/// What bringing a held sum to a request's sides came to.
enum Brought {
    Ready {
        counts: CountsAt,
        reserves: Option<Reserves>,
        /// Whether it differs from the sum it was brought from.
        changed: bool,
    },
    /// Made again from every side's walk, reusing the walks already taken, by the place of their
    /// side among the request's.
    Again(Vec<(usize, Part)>),
}

impl Engine {
    /// The sum `key` names at the versions `wants` read, and its reserve: held, read back,
    /// brought forward or walked. `None` where what is held cannot be brought to those versions,
    /// which a request still reading a form older than the newest held meets.
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
                    let brought = match &predecessor {
                        Some((counts, reserves)) => self.brought(
                            counts,
                            reserves.as_deref(),
                            wants,
                            &stem,
                            geometry,
                            build,
                        )?,
                        None => Brought::Again(Vec::new()),
                    };
                    let (counts, reserves, on_disk) = match brought {
                        Brought::Ready {
                            counts,
                            reserves,
                            changed,
                        } => {
                            if loaded {
                                self.figures.counters.loads.fetch_add(1, Ordering::Relaxed);
                            }
                            (counts, reserves, loaded && !changed)
                        }
                        Brought::Again(made) => {
                            let (counts, reserves) =
                                self.made_again(wants, made, &stem, geometry, build)?;
                            (counts, reserves, false)
                        }
                    };
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

    /// `pred` brought to the sides `wants` read: a side whose identity is the same and whose
    /// steps lead to the version asked is kept, to be followed by its steps; every other side's
    /// walk is taken away, and each side asked that is not kept is walked and added. `None` where
    /// a walk was abandoned.
    fn brought(
        &self,
        pred: &CountsAt,
        pred_reserves: Option<&Reserves>,
        wants: &[Want<'_>],
        stem: &str,
        geometry: Geometry,
        build: &Build<'_>,
    ) -> Option<Brought> {
        let kept: Vec<Option<usize>> = wants
            .iter()
            .map(|want| {
                let at = pred
                    .sides
                    .binary_search_by_key(&want.shard, |side| side.shard)
                    .ok()?;
                let side = &pred.sides[at];
                let follows = side.at == want.at
                    || (side.at < want.at && want.column.steps_between(side.at, want.at).is_some());
                (side.identity == want.identity && follows).then_some(at)
            })
            .collect();
        let old: Vec<usize> = (0..pred.sides.len())
            .filter(|at| !kept.contains(&Some(*at)))
            .collect();
        let new: Vec<usize> = (0..wants.len()).filter(|w| kept[*w].is_none()).collect();
        if old.is_empty() && new.is_empty() {
            return Some(Brought::Ready {
                counts: pred.clone(),
                reserves: pred_reserves.cloned(),
                changed: false,
            });
        }
        let mut new_parts = Vec::with_capacity(new.len());
        for &w in &new {
            new_parts.push((
                w,
                self.part_of(&wants[w], stem, geometry, build, wants.len() > 1)?,
            ));
        }
        if old.len() == pred.sides.len() {
            return Some(Brought::Again(new_parts));
        }
        let Some(old_parts) = old
            .iter()
            .map(|&at| Some((at, self.read_part(&pred.sides[at], stem, geometry)?)))
            .collect::<Option<Vec<(usize, Part)>>>()
        else {
            return Some(Brought::Again(new_parts));
        };

        let added: Vec<(ShardId, &Part)> = new_parts
            .iter()
            .map(|(w, part)| (wants[*w].shard, part))
            .collect();
        let kept_parts = || {
            wants
                .iter()
                .zip(&kept)
                .filter_map(|(want, at)| Some((want.shard, (*at)?)))
                .map(|(shard, at)| Some((shard, self.read_part(&pred.sides[at], stem, geometry)?)))
                .collect::<Option<Vec<_>>>()
        };
        let position = |row: SideRow| position_in(wants, row);
        let Some((dense, grown, reserves)) = difference(
            pred,
            pred_reserves,
            &old_parts,
            &added,
            kept_parts,
            &position,
        ) else {
            return Some(Brought::Again(new_parts));
        };
        let sides = wants
            .iter()
            .zip(&kept)
            .map(|(want, kept)| match kept {
                Some(at) => pred.sides[*at].clone(),
                None => SideAt {
                    shard: want.shard,
                    identity: want.identity,
                    filled_at: want.at,
                    at: want.at,
                    grown: Arc::default(),
                },
            })
            .collect::<Vec<_>>();
        Some(Brought::Ready {
            counts: CountsAt {
                sides: sides.into(),
                dense: Arc::new(dense),
                grown: Arc::new(grown),
            },
            reserves,
            changed: true,
        })
    }

    /// The sum over every side of `wants`, from their walks: `made`'s, by the side's place, and
    /// the others read back or walked. `None` where a walk was abandoned.
    fn made_again(
        &self,
        wants: &[Want<'_>],
        made: Vec<(usize, Part)>,
        stem: &str,
        geometry: Geometry,
        build: &Build<'_>,
    ) -> Option<(CountsAt, Option<Reserves>)> {
        let mut parts: Vec<Option<Part>> = vec![None; wants.len()];
        for (at, part) in made {
            parts[at] = Some(part);
        }
        let mut walks = Vec::with_capacity(wants.len());
        for (want, part) in wants.iter().zip(parts) {
            walks.push(match part {
                Some(part) => part,
                None => self.part_of(want, stem, geometry, build, wants.len() > 1)?,
            });
        }
        let every: Vec<(ShardId, &Part)> = wants
            .iter()
            .map(|want| want.shard)
            .zip(walks.iter())
            .collect();
        let (dense, reserves) = sum_of(&every, geometry, &|row| position_in(wants, row));
        let sides = wants
            .iter()
            .map(|want| SideAt {
                shard: want.shard,
                identity: want.identity,
                filled_at: want.at,
                at: want.at,
                grown: Arc::default(),
            })
            .collect();
        Some((CountsAt::of(sides, dense), reserves))
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
        let part = Part::of(walked, geometry != Geometry::None);
        if let (true, Some(dir)) = (write, dir) {
            let (written, stem, at) = (part.clone(), stem.to_string(), want.at);
            let (bound, root) = (
                Arc::clone(&self.figures.disk_bound),
                self.figures.dir.clone(),
            );
            self.figures.worker.submit(move || {
                persist::write_part(&dir, &stem, at, &written);
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

/// The sum of `parts`, each by the shard of its side: counts, placed counts and sums added, each
/// box the union of the parts', and where `geometry` serves a box, the reserve of every artifact
/// with more than [`RESERVE_MIN_PLACED`] placed rows, positions looked up by `position`.
pub(super) fn sum_of(
    parts: &[(ShardId, &Part)],
    geometry: Geometry,
    position: &dyn Fn(SideRow) -> Option<(u32, u32)>,
) -> (Dense, Option<Reserves>) {
    let len = parts
        .iter()
        .map(|(_, part)| part.len as usize)
        .max()
        .unwrap_or(0);
    let positions = geometry != Geometry::None;
    let mut dense = Dense::empty(len, positions);
    for (_, part) in parts {
        dense.add(part);
        if positions {
            for ordinal in placed_ordinals(part) {
                if let Some(bbox) = placed_box(part, ordinal) {
                    dense.widen_box(ordinal, bbox);
                }
            }
        }
    }
    let reserves = (geometry == Geometry::Box).then(|| {
        let mut out = Reserves {
            index: vec![u32::MAX; len],
            rows: Vec::new(),
        };
        for ordinal in 0..len as u32 {
            if dense.placed(ordinal) > u64::from(RESERVE_MIN_PLACED) {
                let rows = std::array::from_fn(|side| {
                    reserve_of(
                        side,
                        kept_rows(parts.iter().copied(), ordinal, side),
                        position,
                    )
                });
                out.set(ordinal, Some(rows));
            }
        }
        out
    });
    (dense, reserves)
}

/// `pred` with the walks `old` taken away, each by its side's place in `pred`, and `added` added,
/// each by its shard: counts, placed counts and sums by the difference, and what each removed
/// side's growth added since its walk taken away with it. The box and reserve of every artifact
/// a walk taken away or added counts a placed row of are brought to the sides now summed: from
/// the box and reserve held where no walk taken away reaches them, and otherwise from every
/// summed side's walk, those kept read by `kept`. `None` where `old` holds more than `pred` was
/// summed from, or `kept` cannot read a walk.
pub(super) fn difference(
    pred: &CountsAt,
    pred_reserves: Option<&Reserves>,
    old: &[(usize, Part)],
    added: &[(ShardId, &Part)],
    kept: impl FnOnce() -> Option<Vec<(ShardId, Part)>>,
    position: &dyn Fn(SideRow) -> Option<(u32, u32)>,
) -> Option<(Dense, FxHashMap<u32, Grown>, Option<Reserves>)> {
    let mut dense = (*pred.dense).clone();
    let mut grown = (*pred.grown).clone();
    for (at, part) in old {
        dense.sub(part)?;
        CountsAt::without_growth_of(&mut grown, &pred.sides[*at])?;
    }
    for (_, part) in added {
        dense.add(part);
    }
    let mut reserves = pred_reserves.cloned();
    if !dense.positions() {
        return Some((dense, grown, reserves));
    }
    let stale: Vec<ShardId> = old.iter().map(|(at, _)| pred.sides[*at].shard).collect();
    let mut affected: Vec<u32> = old
        .iter()
        .map(|(_, part)| part)
        .chain(added.iter().map(|(_, part)| *part))
        .flat_map(placed_ordinals)
        .collect();
    affected.sort_unstable();
    affected.dedup();

    let mut again = Vec::new();
    for &ordinal in &affected {
        let i = ordinal as usize;
        let before = pred.dense.boxes.get(i).copied().unwrap_or(EMPTY_BOX);
        let reached = old
            .iter()
            .any(|(_, part)| placed_box(part, ordinal).is_some_and(|b| touches(b, before)));
        let (new_box, new_placed) = added.iter().fold((EMPTY_BOX, 0u64), |(b, n), (_, part)| {
            match part.find(ordinal).filter(|&at| part.placed[at] > 0) {
                Some(at) => (union(b, part.boxes[at]), n + u64::from(part.placed[at])),
                None => (b, n),
            }
        });
        let placed = dense.placed(ordinal);
        let kept_placed = placed - new_placed;
        let mut worked_out = false;
        if kept_placed == 0 {
            dense.boxes[i] = new_box;
        } else if !reached {
            dense.boxes[i] = union(before, new_box);
        } else {
            worked_out = true;
        }
        if let Some(reserves) = reserves.as_mut() {
            if placed <= u64::from(RESERVE_MIN_PLACED) {
                reserves.set(ordinal, None);
            } else if kept_placed == 0 {
                let rows = std::array::from_fn(|side| {
                    reserve_of(
                        side,
                        kept_rows(added.iter().copied(), ordinal, side),
                        position,
                    )
                });
                reserves.set(ordinal, Some(rows));
            } else if reserves.side(ordinal, 0).is_some()
                && !stale
                    .iter()
                    .any(|&shard| reserves.names_shard(ordinal, shard))
            {
                let rows = std::array::from_fn(|side| {
                    let held: Vec<SideRow> =
                        reserves.side(ordinal, side).into_iter().flatten().collect();
                    let offered =
                        held.into_iter()
                            .chain(kept_rows(added.iter().copied(), ordinal, side));
                    reserve_of(side, offered, position)
                });
                reserves.set(ordinal, Some(rows));
            } else {
                worked_out = true;
            }
        }
        if worked_out {
            again.push(ordinal);
        }
    }
    if !again.is_empty() {
        let kept = kept()?;
        let every: Vec<(ShardId, &Part)> = kept
            .iter()
            .map(|(shard, part)| (*shard, part))
            .chain(added.iter().copied())
            .collect();
        for &ordinal in &again {
            dense.boxes[ordinal as usize] = every
                .iter()
                .filter_map(|(_, part)| placed_box(part, ordinal))
                .fold(EMPTY_BOX, union);
            if let Some(reserves) = reserves.as_mut() {
                let rows = (dense.placed(ordinal) > u64::from(RESERVE_MIN_PLACED)).then(|| {
                    std::array::from_fn(|side| {
                        reserve_of(
                            side,
                            kept_rows(every.iter().copied(), ordinal, side),
                            position,
                        )
                    })
                });
                reserves.set(ordinal, rows);
            }
        }
    }
    Some((dense, grown, reserves.map(Reserves::compacted)))
}

/// `from` brought to the versions `wants` read by each side's steps, `None` where a side's steps
/// do not lead there.
fn followed(from: &CountsAt, wants: &[Want<'_>], geometry: Geometry) -> Option<CountsAt> {
    if from.sides.len() != wants.len() {
        return None;
    }
    type Counted<'w> = Box<dyn Fn(u32) -> bool + 'w>;
    type Placed<'w> = Box<dyn Fn(u32) -> Option<(u32, u32)> + 'w>;
    let counted: Vec<Counted<'_>> = wants
        .iter()
        .map(|want| {
            Box::new(move |row| row < want.base_rows && want.fragment.contains(row)) as Counted<'_>
        })
        .collect();
    let placed: Vec<Placed<'_>> = wants
        .iter()
        .map(|want| Box::new(move |row| place(&want.places, row)) as Placed<'_>)
        .collect();
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
            counted: &*counted[at],
            place_of: &*placed[at],
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

/// The ordinals `part` counts a placed row of.
fn placed_ordinals(part: &Part) -> impl Iterator<Item = u32> + '_ {
    part.ordinals
        .iter()
        .zip(&part.placed)
        .filter(|(_, &placed)| placed > 0)
        .map(|(&ordinal, _)| ordinal)
}

/// The box of `part`'s placed rows of `ordinal`, where it counts one.
fn placed_box(part: &Part, ordinal: u32) -> Option<[u32; 4]> {
    let at = part.find(ordinal)?;
    (part.placed.get(at).copied().unwrap_or(0) > 0).then(|| part.boxes[at])
}

/// Whether `inner`, a box inside `outer`, reaches one of its sides.
fn touches(inner: [u32; 4], outer: [u32; 4]) -> bool {
    inner.iter().zip(&outer).any(|(a, b)| a == b)
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
