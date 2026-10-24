//! **A level's figures for one request**: per artifact, how many rows of the viewer's visible set
//! carry its label, and where the layer serves them, the mean position and the box of those rows.
//! Every verdict and every number served beside an artifact of a level with a row column is read
//! from here. A number or timestamp field's figures over the visible set, its count, sum,
//! smallest and largest value, are taken the same way ([`field`]), and share the cache, the walks
//! and the cache directory below.
//!
//! # The visible set in three parts
//!
//! A request's composed mask in one side is `S = (P − minus) ∪ plus` ([`crate::compose`]), where
//! `P` is the session's row projection there, `minus ⊆ P` and `plus ∩ P = ∅`. Below the column's
//! base row count `B`:
//!
//! - **F**, the fragment's base rows: `P ∩ [0, B)`. A function of the grant and the shard identity
//!   alone. A flush adds rows above `B` only, a fold rotates the identity, and an item flushed
//!   under a key the grant holds has a row above `B`.
//! - **D**, what the mask subtracts there: `minus ∩ [0, B)`, which is every denied base row in F
//!   and any buffered row the viewer fails. `D ⊆ F` because `minus ⊆ P`.
//! - **T**, every visible row outside F: `plus ∪ ((P ∩ [B, ∞)) − minus)`.
//!
//! `S = (F − D) ∪ T`, the two parts disjoint, so for every artifact `a` with members `M_a`,
//! `|M_a ∩ S| = |M_a ∩ F| − |M_a ∩ D| + |M_a ∩ T|`, and the same holds for the placed count and
//! both position sums. Over several sides each part is the union of the sides' own, so the same
//! holds of their sums.
//!
//! # What is held, and for how long
//!
//! - **F's counts** are held once per grant and level, summed over every side the level is served
//!   from a column in ([`cache::SumKey`]), and shared by every session with the same grant: a walk
//!   of each side's base rows, off the request path, single-flight. A growth or a publication
//!   between folds adds labels to base rows, and each side's column records each version's
//!   additions ([`crate::row_column::GrowthStep`]); the sum follows them in place rather than
//!   walking again, so an ingest window never refills it. Where a side's walk is replaced, the sum
//!   is taken again over every side's walk ([`sum`]). The sums, and each side's walk, are written
//!   to the engine's cache directory and read back after a restart ([`persist`]).
//! - **D's correction** is held per sum, column version and each side's deny version
//!   ([`crate::Shard::deny_version`]), from the labels of the view's denied base rows
//!   ([`denied`]). It is loaded at the request's start like the mask it corrects, so a request that
//!   starts after a suppression is accepted subtracts it. A held sum is never served without it.
//! - **T's correction** is held per session and generation.
//!
//! The box of `F − D` is the held box unless a subtracted row reaches one of its edges. Each
//! artifact with more than sixteen placed rows keeps its eight most extreme rows on each side,
//! whichever side they are in, as a reserve, so the next one not subtracted is the new edge. A side
//! the reserve cannot answer works the box out from the artifact's members.
//!
//! Where a level's denied-row labels are not yet held and too many to read inline, the request is
//! answered by the walk of the whole composed mask ([`cache::FiguresKey::Exact`]) while they are
//! read in the background. A deny queues the same reading for every level whose labels are held,
//! after its acknowledgement, so the next request finds them brought forward.

mod cache;
mod counts;
mod denied;
mod field;
mod labels;
mod persist;
mod sum;
#[cfg(test)]
mod tests;
mod worker;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use croaring::Bitmap;

use mosaica_types::layer::{LayerDeclaration, RegisteredLayer};
use mosaica_types::{LayerSpace, ShardId};

use crate::compose::{EffectiveMask, ShardedMask};
use crate::derived::{place, Placement};
use crate::error::{EngineError, Result};
use crate::row_column::RowColumn;
use crate::viewport::{ServedSide, ServedView};
use crate::Engine;

pub(crate) use cache::{DrawingTurn, FiguresCache, MaskIdentity, SideVersions};
pub use cache::{FiguresStats, DEFAULT_DISK_BYTES, DEFAULT_GIVE_WAY_MS};
pub(crate) use field::{keep_extreme, ExactSum, FieldFigures, FieldRead, FieldTally, Number, Sum};

use cache::{Counters, DenyKey, FiguresKey, LevelAddress, SumEntry, SumKey, Tail, TailKey};
use counts::{CountsAt, Delta, Deltas, Reserves, SideRow, Wide};
use denied::{Brought, DeniedLabels, DenyCorrection};
use labels::{HeldLabels, LabelStore};

/// The subdirectory of the engine's cache directory the figures are written to.
pub const FIGURES_DIR: &str = "figures";

/// How many new denied rows a request reads from a side's column itself. Past this, the labels are
/// read in the background and the request walks its whole mask meanwhile.
const LABELS_READ_INLINE: u64 = 4_096;

/// The least time between two writes of one sum after it was first written. A level moving at
/// every tick writes its newest sum at most this often, and at a clean shutdown.
const COUNTS_WRITE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// What a level's figures carry beside the counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Geometry {
    None = 0,
    /// The placed count and the position sums.
    Centroid = 1,
    /// Those, the box, and each artifact's reserve of extreme rows. A box asked of a level that
    /// keeps no reserve is worked out from the artifact's rows wherever a deny touches it.
    Box = 2,
}

impl Geometry {
    /// What a layer's declared computed properties ask of its levels' figures.
    pub fn declared(declaration: &LayerDeclaration) -> Self {
        let mut out = Geometry::None;
        for property in declaration
            .content
            .computed
            .iter()
            .filter_map(|name| crate::derived::ComputedProperty::parse(name))
        {
            match property {
                crate::derived::ComputedProperty::Box => out = Geometry::Box,
                crate::derived::ComputedProperty::Centroid if out == Geometry::None => {
                    out = Geometry::Centroid
                }
                _ => {}
            }
        }
        out
    }
}

/// A level's figures for one request, over every side the level is served from a column in,
/// answering as one shard holding every side's rows would: see the module doc.
pub struct LevelFigures {
    counts: Arc<CountsAt>,
    /// The reserve walked with `counts`, where it is held.
    reserves: Option<Arc<Reserves>>,
    deny: Option<Arc<DenyCorrection>>,
    tail: Option<Arc<Tail>>,
    /// How many ordinals the level's columns cover.
    len: usize,
    geometry: Geometry,
    /// Per side, ascending by shard, what a box a deny reaches is worked out from. Empty where
    /// the figures carry no positions.
    sources: Vec<BoxSource>,
    counters: Option<Arc<Counters>>,
}

/// One side's column, fragment and segments, held for working out a box after a deny.
struct BoxSource {
    shard: ShardId,
    column: Arc<RowColumn>,
    projection: Arc<crate::projection::RowProjection>,
    base_rows: u32,
    places: WithPlaces,
}

/// Hands the view's segments' positions to a caller, for as long as it reads them.
type WithPlaces = Arc<dyn Fn(&mut dyn FnMut(&[Placement<'_>])) + Send + Sync>;

/// [`WithPlaces`] over one view of `shard` in `bundle`.
fn places_of_view(
    bundle: Arc<mosaica_store::read::Bundle>,
    shard: ShardId,
    view: String,
) -> WithPlaces {
    Arc::new(move |read: &mut dyn FnMut(&[Placement<'_>])| {
        let Some(view_data) = bundle.shard(shard).and_then(|data| data.views.get(&view)) else {
            return;
        };
        if let Ok(segments) = crate::viewport::segments_with_row_bases(&view, view_data) {
            read(&Placement::of_segments(&segments));
        }
    })
}

impl LevelFigures {
    fn over(counts: Arc<CountsAt>, len: usize, geometry: Geometry) -> Self {
        LevelFigures {
            counts,
            reserves: None,
            deny: None,
            tail: None,
            len,
            geometry,
            sources: Vec::new(),
            counters: None,
        }
    }

    /// Figures holding the sum of each side's `counts` and nothing else, for tests of what reads
    /// them.
    #[cfg(test)]
    pub(crate) fn of_counts(sides: &[Vec<u32>]) -> Self {
        let len = sides.iter().map(Vec::len).max().unwrap_or(0);
        let mut dense = counts::Dense::empty(len, false);
        for side in sides {
            let mut part = counts::Part {
                len: side.len() as u32,
                ..counts::Part::default()
            };
            for (ordinal, &count) in side.iter().enumerate().filter(|(_, &c)| c > 0) {
                part.ordinals.push(ordinal as u32);
                part.counts.push(count);
            }
            dense.add(&part);
        }
        Self::over(Arc::new(CountsAt::of(Vec::new(), dense)), len, Geometry::None)
    }

    /// Figures over `dense`'s counts and positions, for tests of what reads them.
    #[cfg(test)]
    pub(crate) fn of_dense(dense: counts::Dense) -> Self {
        let len = dense.counts.len();
        Self::over(Arc::new(CountsAt::of(Vec::new(), dense)), len, Geometry::Centroid)
    }

    fn deny_delta(&self, ordinal: u32) -> Option<&Delta> {
        self.deny.as_ref().and_then(|d| d.deltas.get(&ordinal))
    }

    fn tail_delta(&self, ordinal: u32) -> Option<&Delta> {
        self.tail.as_ref().and_then(|t| t.0.get(&ordinal))
    }

    /// F − D + T for one artifact.
    fn wide(&self, ordinal: u32) -> Wide {
        let f = self.counts.wide(ordinal);
        let d = self.deny_delta(ordinal).map_or(Wide::default(), |d| d.figures);
        let t = self.tail_delta(ordinal).map_or(Wide::default(), |t| t.figures);
        debug_assert!(
            d.count <= f.count,
            "a deny subtracted more rows than the fragment counted"
        );
        Wide {
            count: f.count.saturating_sub(d.count) + t.count,
            placed: f.placed.saturating_sub(d.placed) + t.placed,
            sums: [
                f.sums[0] - d.sums[0] + t.sums[0],
                f.sums[1] - d.sums[1] + t.sums[1],
            ],
        }
    }

    /// `|M_a ∩ S|` for one artifact: zero for a hole and past the level's end, the fail-closed
    /// answer, under which an artifact is absent under any criterion.
    pub fn get(&self, ordinal: u32) -> u64 {
        let f = self.counts.count(ordinal);
        let d = self.deny_delta(ordinal).map_or(0, |d| d.figures.count);
        let t = self.tail_delta(ordinal).map_or(0, |t| t.figures.count);
        debug_assert!(
            d <= f,
            "a deny subtracted more rows than the fragment counted"
        );
        f.saturating_sub(d) + t
    }

    /// Every ordinal whose count is non-zero, ascending: candidacy for a row-major level at a
    /// viewport covering the whole mask in every side.
    pub fn populated(&self) -> Bitmap {
        let dense = &self.counts.dense;
        let hits: Vec<u32> = dense
            .counts
            .iter()
            .enumerate()
            .filter(|(ordinal, &count)| count > 0 && *ordinal < self.len)
            .map(|(ordinal, _)| ordinal as u32)
            .collect();
        let mut out = Bitmap::new();
        out.add_many(&hits);
        let moved = dense
            .wide
            .keys()
            .chain(self.counts.grown.keys())
            .chain(self.deny.iter().flat_map(|d| d.deltas.keys()))
            .chain(self.tail.iter().flat_map(|t| t.0.keys()));
        for &ordinal in moved {
            if (ordinal as usize) < self.len && self.get(ordinal) > 0 {
                out.add(ordinal);
            } else {
                out.remove(ordinal);
            }
        }
        out.run_optimize();
        out
    }

    /// How many ordinals these cover.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether these figures carry positions, which a centroid and a box are read from.
    pub fn has_geometry(&self) -> bool {
        self.geometry != Geometry::None
    }

    /// The mean position of the members this viewer may see, or `None` where they see none placed.
    pub fn centroid(&self, ordinal: u32) -> Option<[f64; 2]> {
        let figures = self.wide(ordinal);
        if figures.placed == 0 {
            return None;
        }
        let n = figures.placed as f64;
        Some([figures.sums[0] as f64 / n, figures.sums[1] as f64 / n])
    }

    /// `[x_min, y_min, x_max, y_max]` over the members this viewer may see, or `None` where they
    /// see none placed.
    pub fn bbox(&self, ordinal: u32) -> Option<[u32; 4]> {
        if self.wide(ordinal).placed == 0 {
            return None;
        }
        let base = match (&self.deny, self.sources.is_empty()) {
            (Some(deny), false) => deny.bbox(
                ordinal,
                &self.counts,
                self.reserves.as_deref(),
                &|row| self.position(row),
                || self.exact_box(deny, ordinal),
            ),
            _ => self.counts.bbox(ordinal),
        };
        let tail = self
            .tail_delta(ordinal)
            .filter(|t| t.figures.placed > 0)
            .map(|t| t.bbox);
        match (base, tail) {
            (Some(a), Some(b)) => Some(counts::union(a, b)),
            (a, b) => a.or(b),
        }
    }

    /// Where `row` of its shard's side is.
    fn position(&self, (shard, row): SideRow) -> Option<(u32, u32)> {
        let at = self
            .sources
            .binary_search_by_key(&shard, |source| source.shard.raw())
            .ok()?;
        let mut out = None;
        (self.sources[at].places)(&mut |places| out = place(places, row));
        out
    }

    /// The box of every side's base rows of `ordinal` the deny does not subtract, worked out from
    /// the rows themselves: the artifact's members where a side's column holds them, and a scan of
    /// the side's fragment where it does not.
    fn exact_box(&self, deny: &DenyCorrection, ordinal: u32) -> Option<[u32; 4]> {
        if let Some(counters) = &self.counters {
            counters.spent.fetch_add(1, Ordering::Relaxed);
        }
        let mut out = counts::EMPTY_BOX;
        let mut any = false;
        for source in &self.sources {
            let fragment = source.projection.bitmap();
            let mut rows = match source.column.members() {
                Some(members) => members.bitmap(ordinal).and(fragment),
                None => {
                    let mut rows = Vec::new();
                    for row in fragment.iter().take_while(|&row| row < source.base_rows) {
                        source.column.for_each_label(row, |held| {
                            if held == ordinal {
                                rows.push(row);
                            }
                        });
                    }
                    Bitmap::of(&rows)
                }
            };
            rows.remove_range(source.base_rows..);
            if let Some(denied) = deny.rows_of(source.shard) {
                rows.andnot_inplace(denied);
            }
            (source.places)(&mut |places| {
                for row in rows.iter() {
                    if let Some((x, y)) = place(places, row) {
                        counts::widen(&mut out, x, y);
                        any = true;
                    }
                }
            });
        }
        any.then_some(out)
    }
}

/// One side of a level as a request reads it: its shard's side of the view, its column, the
/// viewer's leaf there and the version of the level its form is of.
struct Side<'r, 'a> {
    served: &'r ServedSide<'a>,
    column: &'r Arc<RowColumn>,
    leaf: &'r EffectiveMask,
    at: u64,
}

impl Side<'_, '_> {
    /// The mask's `minus` below the base: what the request subtracts from the side's base rows.
    fn subtracted(&self) -> Bitmap {
        let (_, minus, _, _) = self.leaf.parts();
        let mut subtracted = minus.clone();
        subtracted.remove_range(self.column.base_rows()..);
        subtracted
    }
}

impl Engine {
    /// This level's figures for `served`'s request over every side whose form in `forms` has a
    /// column, computed from the composed mask and never from the whole population. `None` where
    /// no side's form has one; such a level counts one artifact at a time instead.
    ///
    /// A request waiting on another's walk stops when its client goes away, and is refused
    /// [`EngineError::CountsBuilding`] past [`cache`]'s wait bound.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn level_figures(
        &self,
        served: &ServedView<'_>,
        mask: &ShardedMask,
        space: LayerSpace,
        level: u32,
        forms: &[Arc<crate::artifacts::ArtifactRows>],
        versions: &[u64],
        geometry: Geometry,
    ) -> Result<Option<LevelFigures>> {
        let mut sides = Vec::new();
        let (mut held, mut column_only) = (false, false);
        for ((side, rows), &at) in served.sides.iter().zip(forms).zip(versions) {
            let (Some(column), Some(leaf)) = (rows.column_shared(), mask.any_leaf(side.shard.id()))
            else {
                continue;
            };
            match rows.membership().rows_held() {
                true => held = true,
                false => column_only = true,
            }
            sides.push(Side {
                served: side,
                column,
                leaf,
                at,
            });
        }
        let Some(first) = sides.first() else {
            return Ok(None);
        };
        // A form holding its own bitmaps derives its geometry per artifact. The sum is walked with
        // positions where any side's form is served from its column alone.
        let (served_geometry, geometry) = match (held, column_only) {
            (false, _) => (geometry, geometry),
            (true, true) => (Geometry::None, geometry),
            (true, false) => (Geometry::None, Geometry::None),
        };
        let len = first.column.len();
        debug_assert!(
            sides.iter().all(|side| side.column.len() == len),
            "every shard's form of a level holds a slot for every ordinal"
        );
        let exact = || self.exact_figures(served, &sides, space, level, served_geometry, len);
        if sides
            .iter()
            .any(|side| side.column.base_rows() != side.served.data.row_space.base_rows())
        {
            return exact();
        }
        let wants: Vec<sum::Want<'_>> = sides
            .iter()
            .map(|side| sum::Want {
                shard: side.served.shard.id(),
                identity: side.served.geometry.fragment.identity,
                column: side.column,
                at: side.at,
                base_rows: side.column.base_rows(),
                fragment: side.leaf.parts().0,
                places: Placement::of_segments(&side.served.segments),
            })
            .collect();
        let key = SumKey {
            terms: served.mask_identity.terms,
            view: served.name.to_string(),
            space,
            level,
            geometry,
            sides: cache::sides_digest(
                wants
                    .iter()
                    .map(|want| (want.shard, want.identity, want.column.identity())),
            ),
        };
        let Some((counts, reserves)) = self.summed(served, &key, &wants)? else {
            return exact();
        };
        let deny = match self.deny_correction(served, &key, &sides, &wants)? {
            Correction::None => None,
            Correction::Held(deny) => Some(deny),
            Correction::Reading => return exact(),
        };
        let tail = self.tail(served, &key, &sides, &wants)?;
        let sources = match served_geometry {
            Geometry::None => Vec::new(),
            _ => sides
                .iter()
                .map(|side| BoxSource {
                    shard: side.served.shard.id(),
                    column: Arc::clone(side.column),
                    projection: Arc::clone(side.leaf.projection()),
                    base_rows: side.column.base_rows(),
                    places: places_of_view(
                        Arc::clone(&served.generation.bundle),
                        side.served.shard.id(),
                        served.name.to_string(),
                    ),
                })
                .collect(),
        };
        Ok(Some(LevelFigures {
            counts,
            reserves,
            deny,
            tail,
            len,
            geometry: served_geometry,
            sources,
            counters: Some(Arc::clone(&self.figures.counters)),
        }))
    }

    /// The correction for every side's `minus` below the base: held, or worked out from the
    /// level's denied-row labels in each side.
    fn deny_correction(
        &self,
        served: &ServedView<'_>,
        sum: &SumKey,
        sides: &[Side<'_, '_>],
        wants: &[sum::Want<'_>],
    ) -> Result<Correction> {
        let subtracted: Vec<Bitmap> = sides.iter().map(Side::subtracted).collect();
        if subtracted.iter().all(Bitmap::is_empty) {
            return Ok(Correction::None);
        }
        let key = DenyKey {
            sum: sum.clone(),
            sides: cache::digest_of(b"mosaica-deny-sides-v1", |hasher| {
                use sha2::Digest;
                for (side, subtracted) in sides.iter().zip(&subtracted) {
                    let (deny_version, failing) = deny_version(served, side.served, subtracted);
                    hasher.update(side.at.to_le_bytes());
                    hasher.update(deny_version.to_le_bytes());
                    hasher.update(failing.map_or([0; 9], |at| {
                        let mut held = [1; 9];
                        held[1..].copy_from_slice(&at.to_le_bytes());
                        held
                    }));
                }
            }),
        };
        if let mosaica_cache::Peek::Ready(held) = self.figures.denies.peek(&key) {
            return Ok(Correction::Held(held));
        }
        let mut labelled = Vec::with_capacity(sides.len());
        let mut reading = false;
        for ((side, want), subtracted) in sides.iter().zip(wants).zip(&subtracted) {
            if subtracted.is_empty() {
                continue;
            }
            let mut denied = side.served.denied.clone();
            denied.remove_range(side.column.base_rows()..);
            let labels = match subtracted.intersect(&denied) {
                false => None,
                true => {
                    let address = (want.shard, served.name.to_string(), sum.space, sum.level);
                    let wanted = Wanted {
                        identity: want.identity,
                        column: side.column,
                        at: side.at,
                        denied: &denied,
                    };
                    match self.figures.labels_for(
                        &address,
                        &wanted,
                        &want.places,
                        LABELS_READ_INLINE,
                    ) {
                        Some(labels) => Some(labels),
                        None => {
                            self.figures.label_in_background(
                                address,
                                Arc::clone(side.column),
                                side.at,
                                denied.clone(),
                                want.identity,
                                places_of_view(
                                    Arc::clone(&served.generation.bundle),
                                    want.shard,
                                    served.name.to_string(),
                                ),
                            );
                            reading = true;
                            None
                        }
                    }
                }
            };
            labelled.push((want, side.column, subtracted, labels));
        }
        if reading {
            return Ok(Correction::Reading);
        }
        let cancel = served.cancel.clone().unwrap_or_default();
        self.figures
            .denies
            .get_or_try_build_waiting(key, &cancel, || {
                Ok::<_, std::convert::Infallible>(DenyCorrection::of(labelled.iter().map(
                    |(want, column, subtracted, labels)| denied::Subtracted {
                        shard: want.shard,
                        rows: (*subtracted).clone(),
                        labels: labels.as_deref(),
                        column,
                        places: &want.places,
                    },
                )))
            })
            .map(Correction::Held)
            .map_err(|ended| match ended {
                mosaica_cache::WaitingBuildError::Wait(ended) => wait_error(ended),
                mosaica_cache::WaitingBuildError::Build(never) => match never {},
            })
    }

    /// The session's rows outside every side's base rows, with their labels: held per session and
    /// generation.
    fn tail(
        &self,
        served: &ServedView<'_>,
        sum: &SumKey,
        sides: &[Side<'_, '_>],
        wants: &[sum::Want<'_>],
    ) -> Result<Option<Arc<Tail>>> {
        let aboves: Vec<Bitmap> = sides
            .iter()
            .map(|side| {
                let (fragment, minus, plus, _) = side.leaf.parts();
                let base_rows = side.column.base_rows();
                let mut above = match fragment.maximum().filter(|&last| last >= base_rows) {
                    Some(last) => fragment.and(&Bitmap::from_range(base_rows..last + 1)),
                    None => Bitmap::new(),
                };
                above.andnot_inplace(minus);
                above.or_inplace(plus);
                above
            })
            .collect();
        if aboves.iter().all(Bitmap::is_empty) {
            return Ok(None);
        }
        let key = TailKey {
            token_id: served.mask_identity.token_id,
            sum: sum.clone(),
            level_versions: cache::versions_digest(sides.iter().map(|side| side.at)),
            shards: served.mask_identity.shards,
        };
        let cancel = served.cancel.clone().unwrap_or_default();
        let tail = self
            .figures
            .tails
            .get_or_try_build_waiting(key, &cancel, || {
                let mut deltas = Deltas::default();
                for ((side, want), above) in sides.iter().zip(wants).zip(&aboves) {
                    for row in above.iter() {
                        let position = place(&want.places, row);
                        side.column.for_each_label(row, |ordinal| {
                            deltas.entry(ordinal).or_default().add(position)
                        });
                    }
                }
                Ok::<_, std::convert::Infallible>(Tail(deltas))
            })
            .map_err(|ended| match ended {
                mosaica_cache::WaitingBuildError::Wait(ended) => wait_error(ended),
                mosaica_cache::WaitingBuildError::Build(never) => match never {},
            })?;
        Ok(Some(tail))
    }

    /// The figures of the whole composed mask in every side, walked: what a request is answered
    /// from where the held sum cannot be.
    fn exact_figures(
        &self,
        served: &ServedView<'_>,
        sides: &[Side<'_, '_>],
        space: LayerSpace,
        level: u32,
        geometry: Geometry,
        len: usize,
    ) -> Result<Option<LevelFigures>> {
        use crate::compose::WholeMask;
        let key = FiguresKey::Exact(served.mask_identity.exact_key(
            served.name,
            space,
            level,
            cache::versions_digest(sides.iter().map(|side| side.at)),
            geometry,
        ));
        let held = self
            .figures
            .get_or_build(key, &served.turn, served.cancel.as_ref(), |build| {
                self.figures.counters.exact.fetch_add(1, Ordering::Relaxed);
                let positions = geometry != Geometry::None;
                let mut parts = Vec::with_capacity(sides.len());
                for side in sides {
                    let places = positions.then(|| Placement::of_segments(&side.served.segments));
                    let walked = self.count_pool.install(|| {
                        #[cfg(feature = "fault-injection")]
                        self.switches.hold_masked_count_build_if_wanted();
                        side.column.accumulate(
                            side.leaf.visible_all(),
                            places.as_deref(),
                            &|| build.give_way(),
                        )
                    })?;
                    parts.push((side.served.shard.id(), counts::Part::of(walked)));
                }
                let every: Vec<_> = parts.iter().map(|(shard, part)| (*shard, part)).collect();
                let (dense, _) = sum::sum_of(&every, positions, None);
                Some(SumEntry::new(CountsAt::of(Vec::new(), dense), None, None))
            })
            .map_err(wait_error)?;
        Ok(Some(LevelFigures::over(held.newest(), len, geometry)))
    }
}

/// What a request subtracts from a held sum.
enum Correction {
    /// Nothing: the request subtracts no base row.
    None,
    Held(Arc<DenyCorrection>),
    /// A side's denied-row labels are being read in the background.
    Reading,
}

/// The denied-row labels a caller wants: of `column` at level version `at`, over the base rows
/// `denied`, numbered under bundle `identity`.
struct Wanted<'a> {
    identity: [u8; 32],
    column: &'a RowColumn,
    at: u64,
    denied: &'a Bitmap,
}

/// A write a caller queues on the worker or, already on it, runs.
type Job = Box<dyn FnOnce() + Send + 'static>;

impl LabelStore {
    /// The labels `wanted` names: held, brought forward from a version held or on disk, or read
    /// from the column, reading at most `limit` rows from it, with the write that keeps them on
    /// disk where one is due. `None` where more rows are needed.
    fn labels_for(
        &self,
        address: &LevelAddress,
        wanted: &Wanted<'_>,
        places: &[Placement<'_>],
        limit: u64,
    ) -> Option<(Arc<DeniedLabels>, Option<Job>)> {
        let held = self.held.versions(address);
        if let Some(exact) = held.iter().find(|held| {
            held.identity == wanted.identity
                && held.column == wanted.column.identity()
                && held.at == wanted.at
                && held.rows.len() as u64 == wanted.denied.cardinality()
                && held.rows.iter().all(|&row| wanted.denied.contains(row))
        }) {
            return Some((Arc::clone(exact), None));
        }
        let (_, view, space, level) = address;
        let dir = self
            .dir
            .as_ref()
            .map(|root| persist::identity_dir(root, &wanted.identity));
        let stem = persist::denied_stem(view, *space, *level);
        let on_disk = || {
            dir.as_deref().and_then(|dir| {
                persist::read_denied(
                    dir,
                    &stem,
                    wanted.at,
                    wanted.identity,
                    wanted.column.identity(),
                )
            })
        };
        let brought = Arc::new(bring(&self.held, &held, on_disk, wanted, places, limit)?);
        let identity = brought.identity;
        let job = match (
            self.held.hold(address.clone(), Arc::clone(&brought)),
            self.dir.clone(),
        ) {
            (true, Some(root)) => {
                let (held, bound, address) = (
                    Arc::clone(&self.held),
                    Arc::clone(&self.disk_bound),
                    address.clone(),
                );
                Some(
                    Box::new(move || write_labels(&held, &root, &identity, &address, &bound))
                        as Job,
                )
            }
            _ => None,
        };
        Some((brought, job))
    }
}

impl FiguresCache {
    /// [`LabelStore::labels_for`] for a request, its write queued on the worker.
    fn labels_for(
        &self,
        address: &LevelAddress,
        wanted: &Wanted<'_>,
        places: &[Placement<'_>],
        limit: u64,
    ) -> Option<Arc<DeniedLabels>> {
        let (labels, job) = self.labels.labels_for(address, wanted, places, limit)?;
        if let Some(job) = job {
            self.worker.submit(job);
        }
        Some(labels)
    }

    /// Bring `address`'s labels to `denied` at `at` on the worker, reading no row a held version
    /// already has.
    fn label_in_background(
        &self,
        address: LevelAddress,
        column: Arc<RowColumn>,
        at: u64,
        denied: Bitmap,
        identity: [u8; 32],
        places: WithPlaces,
    ) {
        let store = self.labels.clone();
        self.worker.submit(move || {
            let wanted = Wanted {
                identity,
                column: &column,
                at,
                denied: &denied,
            };
            places(&mut |places| {
                if let Some((_, Some(write))) =
                    store.labels_for(&address, &wanted, places, u64::MAX)
                {
                    write();
                }
            });
        });
    }

    /// Queue every held level of a layer in `registered` to have its labels brought to
    /// `generation`'s denied rows, at the version of the form `projections` holds for it: what a
    /// deny does once it is acknowledged.
    pub(crate) fn refresh_denied(
        &self,
        generation: &crate::Generation,
        projections: &crate::artifacts::ArtifactProjections,
        registered: &[RegisteredLayer],
    ) {
        for address in self.labels.held.addresses() {
            let (shard, view, space, level) = &address;
            let Some(shard) = generation.shard(*shard) else {
                continue;
            };
            let Some(layer) = registered.iter().find(|layer| layer.space == *space) else {
                continue;
            };
            let name = &layer.declaration.name;
            let Some((rows, at)) = projections.held_form_at(shard.id(), view, name, *level) else {
                continue;
            };
            let Some(column) = rows.column_shared() else {
                continue;
            };
            let Some(mut denied) = shard.denied().get(view).cloned() else {
                continue;
            };
            denied.remove_range(column.base_rows()..);
            self.label_in_background(
                address.clone(),
                Arc::clone(column),
                at,
                denied,
                shard.identity,
                places_of_view(Arc::clone(&generation.bundle), shard.id(), view.clone()),
            );
        }
    }
}

/// `wanted`'s labels from the newest held version that leads to it, else the version on disk,
/// else read whole; reading at most `limit` rows from the column, and `None` where that is too few.
fn bring(
    labels: &HeldLabels,
    held: &[Arc<DeniedLabels>],
    on_disk: impl FnOnce() -> Option<DeniedLabels>,
    wanted: &Wanted<'_>,
    places: &[Placement<'_>],
    limit: u64,
) -> Option<DeniedLabels> {
    let from_disk = on_disk();
    let candidates = held
        .iter()
        .rev()
        .filter(|held| held.at <= wanted.at)
        .map(|held| held.as_ref())
        .chain(from_disk.as_ref());
    for candidate in candidates {
        match candidate.brought(
            wanted.identity,
            wanted.column,
            wanted.at,
            wanted.denied,
            places,
            limit,
        ) {
            (Brought::Ready(brought), read) => {
                labels.read(read);
                return Some(brought);
            }
            (Brought::TooMany, _) => return None,
            (Brought::Broken, _) => continue,
        }
    }
    if wanted.denied.cardinality() > limit {
        return None;
    }
    labels.read(wanted.denied.cardinality());
    Some(DeniedLabels::read(
        wanted.identity,
        wanted.column,
        wanted.at,
        wanted.denied,
        places,
    ))
}

/// Write `address`'s newest labels until the newest is written, then hold the directory under its
/// bound. Runs on the worker; at most one is queued per level.
fn write_labels(
    held: &HeldLabels,
    root: &std::path::Path,
    identity: &[u8; 32],
    address: &LevelAddress,
    bound: &AtomicU64,
) {
    let (_, view, space, level) = address;
    let stem = persist::denied_stem(view, *space, *level);
    while let Some(newest) = held.to_write(address) {
        persist::write_denied(&persist::identity_dir(root, identity), &stem, &newest);
        held.wrote(address, newest.at);
    }
    persist::hold_under(root, bound.load(Ordering::Relaxed));
}

/// What a correction for `subtracted`, the mask's `minus` below the base, is a function of beside
/// its fragment: the view's deny version, and the overlay's version where the mask also subtracts
/// buffered rows the viewer fails below the base, which no deny version follows: rows of
/// `subtracted` outside `side`'s denied rows.
fn deny_version(
    served: &ServedView<'_>,
    side: &ServedSide<'_>,
    subtracted: &Bitmap,
) -> (u64, Option<u64>) {
    let failing = !subtracted.is_subset(side.denied);
    (
        side.shard.deny_version(served.name),
        failing.then_some(side.shard.overlay_version),
    )
}

/// A wait on a correction's build, or the build's own error.
pub(crate) fn waited(ended: mosaica_cache::WaitingBuildError<EngineError>) -> EngineError {
    match ended {
        mosaica_cache::WaitingBuildError::Wait(ended) => wait_error(ended),
        mosaica_cache::WaitingBuildError::Build(e) => e,
    }
}

fn wait_error(ended: mosaica_cache::WaitEnded) -> EngineError {
    match ended {
        mosaica_cache::WaitEnded::Budget => EngineError::CountsBuilding,
        mosaica_cache::WaitEnded::Cancelled => EngineError::Cancelled,
    }
}
