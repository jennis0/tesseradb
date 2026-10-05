//! **A level's figures for one request**: per artifact, how many rows of the viewer's visible set
//! carry its label, and where the layer serves them, the mean position and the box of those rows.
//! Every verdict and every number served beside an artifact of a level with a row column is read
//! from here. A number or timestamp field's figures over the visible set, its count, sum,
//! smallest and largest value, are taken the same way ([`field`]), and share the cache, the walks
//! and the cache directory below.
//!
//! # The visible set in three parts
//!
//! A request's composed mask is `S = (P − minus) ∪ plus` ([`crate::compose`]), where `P` is the
//! session's row projection, `minus ⊆ P` and `plus ∩ P = ∅`. Below the column's base row count `B`:
//!
//! - **F**, the fragment's base rows: `P ∩ [0, B)`. A function of the grant and the bundle
//!   identity alone. A flush adds rows above `B` only, a fold rotates the identity, and an item
//!   flushed under a key the grant holds has a row above `B`.
//! - **D**, what the mask subtracts there: `minus ∩ [0, B)`, which is every denied base row in F
//!   and any buffered row the viewer fails. `D ⊆ F` because `minus ⊆ P`.
//! - **T**, every visible row outside F: `plus ∪ ((P ∩ [B, ∞)) − minus)`.
//!
//! `S = (F − D) ∪ T`, the two parts disjoint, so for every artifact `a` with members `M_a`,
//! `|M_a ∩ S| = |M_a ∩ F| − |M_a ∩ D| + |M_a ∩ T|`, and the same holds for the placed count and
//! both position sums.
//!
//! # What is held, and for how long
//!
//! - **F's counts** are held per fragment, level and column ([`cache::FragmentKey`]) and shared by
//!   every session with the same grant: a walk of F's rows, off the request path, single-flight.
//!   A growth or a publication between folds adds labels to base rows, and the column records
//!   each version's additions ([`crate::row_column::GrowthStep`]); the counts follow them in
//!   place rather than walking again, so an ingest window never refills them. They are written to
//!   the engine's cache directory and read back after a restart ([`persist`]).
//! - **D's correction** is held per fragment, column version and deny version
//!   ([`crate::Generation::deny_version`]), from the labels of the view's denied base rows
//!   ([`denied`]). It is loaded at the request's start like the mask it corrects, so a request that
//!   starts after a suppression is accepted subtracts it. A cached count over F is never served
//!   without it.
//! - **T's correction** is held per session and generation.
//!
//! The box of `F − D` is the cached box unless a subtracted row reaches one of its edges. Each
//! artifact with more than sixteen placed rows keeps its eight most extreme rows on each side as a
//! reserve, so the next one not subtracted is the new edge. A side the reserve cannot answer works
//! the box out from the artifact's members.
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
#[cfg(test)]
mod tests;
mod worker;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use croaring::Bitmap;

use tessera_types::layer::LayerDeclaration;

use crate::derived::{place, Placement};
use crate::error::{EngineError, Result};
use crate::row_column::RowColumn;
use crate::viewport::ServedView;
use crate::Engine;

pub(crate) use cache::{DrawingTurn, FiguresCache, MaskIdentity};
pub use cache::{FiguresStats, DEFAULT_DISK_BYTES, DEFAULT_GIVE_WAY_MS};
pub(crate) use field::{by_value, FieldFigures, FieldRead, FieldTally, Number};

use cache::{
    Counters, DenyKey, FiguresKey, FragmentCounts, FragmentKey, LevelAddress, Tail, TailKey,
};
use counts::{CountsAt, Delta, Deltas, Dense, Reserves};
use denied::{Brought, DeniedLabels, DenyCorrection};
use labels::{HeldLabels, LabelStore};

/// The subdirectory of the engine's cache directory the figures are written to.
pub const FIGURES_DIR: &str = "figures";

/// How many new denied rows a request reads from the column itself. Past this, the labels are
/// read in the background and the request walks its whole mask meanwhile.
const LABELS_READ_INLINE: u64 = 4_096;

/// The least time between two writes of one fragment's counts after it was first written. A level
/// moving at every tick writes its newest counts at most this often, and at a clean shutdown.
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

/// One level's figures for one request: see the module doc.
pub struct Figures {
    counts: Arc<CountsAt>,
    /// The reserve walked with `counts`, where it is held.
    reserves: Option<Arc<Reserves>>,
    deny: Option<Arc<DenyCorrection>>,
    tail: Option<Arc<Tail>>,
    /// How many ordinals the level's column covers.
    len: usize,
    geometry: Geometry,
    /// What a box a deny reaches is worked out from.
    exact: Option<BoxSource>,
}

/// One request's column, fragment and segments, held for working out a box after a deny.
struct BoxSource {
    column: Arc<RowColumn>,
    projection: Arc<crate::projection::RowProjection>,
    base_rows: u32,
    places: WithPlaces,
    counters: Arc<Counters>,
}

/// Hands the view's segments' positions to a caller, for as long as it reads them.
type WithPlaces = Arc<dyn Fn(&mut dyn FnMut(&[Placement<'_>])) + Send + Sync>;

/// [`WithPlaces`] over one view of `bundle`.
fn places_of_view(bundle: Arc<tessera_store::read::Bundle>, view: String) -> WithPlaces {
    Arc::new(move |read: &mut dyn FnMut(&[Placement<'_>])| {
        let Some(view_data) = bundle
            .partitions
            .values()
            .find_map(|partition| partition.views.get(&view))
        else {
            return;
        };
        if let Ok(segments) = crate::viewport::segments_with_row_bases(&view, view_data) {
            read(&Placement::of_segments(&segments));
        }
    })
}

impl Figures {
    /// Figures holding `counts` and nothing else, for tests of what reads them.
    #[cfg(test)]
    pub(crate) fn of_counts(counts: Vec<u32>) -> Self {
        Figures {
            len: counts.len(),
            counts: Arc::new(CountsAt::of_counts(0, counts)),
            reserves: None,
            deny: None,
            tail: None,
            geometry: Geometry::None,
            exact: None,
        }
    }

    fn deny_delta(&self, ordinal: u32) -> Option<&Delta> {
        self.deny.as_ref().and_then(|d| d.deltas.get(&ordinal))
    }

    fn tail_delta(&self, ordinal: u32) -> Option<&Delta> {
        self.tail.as_ref().and_then(|t| t.0.get(&ordinal))
    }

    /// `|M_a ∩ S|` for one artifact: zero for a hole and past the level's end, the fail-closed
    /// answer, under which an artifact is absent under any criterion.
    pub fn get(&self, ordinal: u32) -> u64 {
        let f = self.counts.count(ordinal);
        let d = self.deny_delta(ordinal).map_or(0, |d| u64::from(d.count));
        let t = self.tail_delta(ordinal).map_or(0, |t| u64::from(t.count));
        debug_assert!(
            d <= f,
            "a deny subtracted more rows than the fragment counted"
        );
        f.saturating_sub(d) + t
    }

    /// Every ordinal whose count is non-zero, ascending: candidacy for a row-major level at a
    /// viewport covering the whole mask ([`crate::artifacts::ArtifactRows::candidacy`]).
    pub fn populated(&self) -> Bitmap {
        let mut hits: Vec<u32> = self
            .counts
            .dense
            .counts
            .iter()
            .enumerate()
            .filter(|(ordinal, &count)| count > 0 && *ordinal < self.len)
            .map(|(ordinal, _)| ordinal as u32)
            .collect();
        let mut out = Bitmap::new();
        out.add_many(&hits);
        hits.clear();
        let moved = self
            .counts
            .grown
            .keys()
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

    /// How many ordinals this covers.
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

    fn placed(&self, ordinal: u32) -> u64 {
        let f = self.counts.placed(ordinal);
        let d = self.deny_delta(ordinal).map_or(0, |d| u64::from(d.placed));
        let t = self.tail_delta(ordinal).map_or(0, |t| u64::from(t.placed));
        f.saturating_sub(d) + t
    }

    /// The mean position of the members this viewer may see, or `None` where they see none placed.
    pub fn centroid(&self, ordinal: u32) -> Option<[f64; 2]> {
        let n = self.placed(ordinal);
        if n == 0 {
            return None;
        }
        let f = self.counts.sums(ordinal);
        let d = self.deny_delta(ordinal).map_or([0; 2], |d| d.sums);
        let t = self.tail_delta(ordinal).map_or([0; 2], |t| t.sums);
        let sum = |axis: usize| (f[axis] - d[axis] + t[axis]) as f64;
        Some([sum(0) / n as f64, sum(1) / n as f64])
    }

    /// `[x_min, y_min, x_max, y_max]` over the members this viewer may see, or `None` where they
    /// see none placed.
    pub fn bbox(&self, ordinal: u32) -> Option<[u32; 4]> {
        if self.placed(ordinal) == 0 {
            return None;
        }
        let base = match (&self.deny, &self.exact) {
            (Some(deny), Some(source)) => {
                let mut answer = None;
                (source.places)(&mut |places| {
                    answer = deny.bbox(
                        ordinal,
                        &self.counts,
                        self.reserves.as_deref(),
                        &|row| place(places, row),
                        || self.exact_box(source, deny, ordinal, places),
                    );
                });
                answer
            }
            _ => self.counts.bbox(ordinal),
        };
        let tail = self
            .tail_delta(ordinal)
            .filter(|t| t.placed > 0)
            .map(|t| t.bbox);
        match (base, tail) {
            (Some(mut a), Some(b)) => {
                counts::widen(&mut a, b[0], b[1]);
                counts::widen(&mut a, b[2], b[3]);
                Some(a)
            }
            (a, b) => a.or(b),
        }
    }

    /// The box of the fragment's base rows of `ordinal` the deny does not subtract, worked out
    /// from the rows themselves: the artifact's members where the column holds them, and a scan
    /// of the fragment's rows where it does not.
    fn exact_box(
        &self,
        source: &BoxSource,
        deny: &DenyCorrection,
        ordinal: u32,
        places: &[Placement<'_>],
    ) -> Option<[u32; 4]> {
        source.counters.spent.fetch_add(1, Ordering::Relaxed);
        let fragment = source.projection.bitmap();
        let mut rows = match source.column.members() {
            Some(members) => members.members(ordinal).to_bitmap().and(fragment),
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
        rows.andnot_inplace(&deny.rows);
        let mut out = [u32::MAX, u32::MAX, 0, 0];
        let mut any = false;
        for row in rows.iter() {
            if let Some((x, y)) = place(places, row) {
                counts::widen(&mut out, x, y);
                any = true;
            }
        }
        any.then_some(out)
    }
}

impl Engine {
    /// This level's figures for `served`'s request, computed from the composed mask and never from
    /// the whole population. `None` on a level with no row column, which counts one artifact at a
    /// time instead.
    ///
    /// A request waiting on another's walk stops when its client goes away, and is refused
    /// [`EngineError::CountsBuilding`] past [`cache`]'s wait bound.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn figures(
        &self,
        served: &ServedView<'_>,
        layer: &str,
        level: u32,
        level_version: u64,
        rows: &crate::artifacts::ArtifactRows,
        mask: &crate::compose::EffectiveMask,
        geometry: Geometry,
    ) -> Result<Option<Arc<Figures>>> {
        let Some(column) = rows.column_shared() else {
            return Ok(None);
        };
        // A form holding its own bitmaps derives its geometry per artifact.
        let geometry = match rows.membership().rows_held() {
            true => Geometry::None,
            false => geometry,
        };
        let base_rows = column.base_rows();
        if base_rows != served.data.row_space.base_rows() {
            return self.exact_figures(served, layer, level, level_version, column, mask, geometry);
        }
        let identity = served.mask_identity;
        let (fragment, minus, plus, _) = mask.parts();
        let places = Placement::of_segments(&served.segments);

        let key = FragmentKey {
            terms: identity.terms,
            identity: identity.fragment_identity,
            view: served.name.to_string(),
            layer: layer.to_string(),
            level,
            column: column.identity(),
            geometry,
        };
        let Some(counts) =
            self.fragment_counts(served, &key, level_version, column, fragment, &places)?
        else {
            return self.exact_figures(served, layer, level, level_version, column, mask, geometry);
        };

        let mut subtracted = minus.clone();
        subtracted.remove_range(base_rows..);
        let deny = match subtracted.is_empty() {
            true => None,
            false => match self.deny_correction(
                served,
                layer,
                level,
                level_version,
                column,
                subtracted,
                &places,
            )? {
                Some(deny) => Some(deny),
                None => {
                    return self.exact_figures(
                        served,
                        layer,
                        level,
                        level_version,
                        column,
                        mask,
                        geometry,
                    )
                }
            },
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
                let key = TailKey {
                    token_id: identity.token_id,
                    view: served.name.to_string(),
                    layer: layer.to_string(),
                    level,
                    column: column.identity(),
                    level_version,
                    segments_version: identity.segments_version,
                    projection_segments_version: identity.projection_segments_version,
                    overlay_version: identity.overlay_version,
                };
                let cancel = served.cancel.clone().unwrap_or_default();
                let tail = self
                    .figures
                    .tails
                    .get_or_try_build_waiting(key, &cancel, || {
                        let mut deltas = Deltas::default();
                        for row in above.iter() {
                            let position = place(&places, row);
                            column.for_each_label(row, |ordinal| {
                                deltas.entry(ordinal).or_default().add(position)
                            });
                        }
                        Ok::<_, std::convert::Infallible>(Tail(deltas))
                    })
                    .map_err(|ended| match ended {
                        tessera_cache::WaitingBuildError::Wait(ended) => wait_error(ended),
                        tessera_cache::WaitingBuildError::Build(never) => match never {},
                    })?;
                Some(tail)
            }
        };

        let reserves = match geometry {
            Geometry::Box => self.figures.reserve(&key, counts.dense.filled_at),
            _ => None,
        };
        Ok(Some(Arc::new(Figures {
            counts,
            reserves,
            deny,
            tail,
            len: column.len(),
            geometry,
            exact: (geometry != Geometry::None).then(|| BoxSource {
                column: Arc::clone(column),
                projection: Arc::clone(mask.projection()),
                base_rows,
                places: places_of_view(
                    Arc::clone(&served.generation.bundle),
                    served.name.to_string(),
                ),
                counters: Arc::clone(&self.figures.counters),
            }),
        })))
    }

    /// The fragment's counts over the column's base rows at `level_version`: held, read from disk,
    /// or walked. `None` where what is held cannot be brought to that version, which a request
    /// still reading a form older than the newest held meets.
    fn fragment_counts(
        &self,
        served: &ServedView<'_>,
        key: &FragmentKey,
        level_version: u64,
        column: &Arc<RowColumn>,
        fragment: &Bitmap,
        places: &[Placement<'_>],
    ) -> Result<Option<Arc<CountsAt>>> {
        let geometry = key.geometry;
        let base_rows = column.base_rows();
        let dir = self
            .figures
            .dir
            .as_ref()
            .map(|root| persist::identity_dir(root, &key.identity));
        let stem = persist::counts_stem(&key.terms, &key.view, &key.layer, key.level, geometry);
        let written = dir.clone().map(|dir| (dir, stem.clone()));
        let held = self
            .figures
            .get_or_build(
                FiguresKey::Fragment(key.clone()),
                &served.turn,
                served.cancel.as_ref(),
                |build| {
                    if let Some((counts, reserves)) = dir
                        .as_deref()
                        .and_then(|dir| persist::read_counts(dir, &stem, level_version, geometry))
                    {
                        self.figures.counters.loads.fetch_add(1, Ordering::Relaxed);
                        if let Some(reserves) = reserves {
                            self.figures
                                .hold_reserve(key, counts.dense.filled_at, reserves);
                        }
                        return Some(FragmentCounts::new(counts, written.clone()));
                    }
                    self.figures.counters.fills.fetch_add(1, Ordering::Relaxed);
                    let positions = (geometry != Geometry::None).then_some(places);
                    let walked = self.count_pool.install(|| {
                        #[cfg(feature = "fault-injection")]
                        self.switches.hold_masked_count_build_if_wanted();
                        column.accumulate_below(
                            fragment,
                            base_rows,
                            positions,
                            geometry == Geometry::Box,
                            &|| build.give_way(),
                        )
                    })?;
                    let (dense, reserves) = Dense::of(level_version, walked);
                    let counts = CountsAt::of(dense);
                    let held = FragmentCounts::new(counts, written.clone());
                    let newest = held.newest();
                    let reserves = reserves.map(Arc::new);
                    if let Some(reserves) = &reserves {
                        self.figures
                            .hold_reserve(key, level_version, Reserves::clone(reserves));
                    }
                    if let Some((dir, stem)) = written.clone() {
                        let bound = Arc::clone(&self.figures.disk_bound);
                        let root = self.figures.dir.clone();
                        self.figures.worker.submit(move || {
                            persist::write_counts(&dir, &stem, &newest, reserves.as_deref());
                            if let Some(root) = root {
                                persist::hold_under(&root, bound.load(Ordering::Relaxed));
                            }
                        });
                    }
                    Some(held)
                },
            )
            .map_err(wait_error)?;
        let counts = held.at(level_version, |from| {
            let steps = column.steps_between(from.at, level_version)?;
            Some(from.followed(
                level_version,
                &steps,
                |row| row < base_rows && fragment.contains(row),
                |row| place(places, row),
                geometry,
            ))
        });
        if counts.is_some() {
            self.write_newest(key, &held);
        }
        Ok(counts)
    }

    /// Write `held`'s newest counts where they are newer than those written, no write of them is
    /// under way, and the last was at least [`COUNTS_WRITE_INTERVAL`] ago.
    fn write_newest(&self, key: &FragmentKey, held: &Arc<FragmentCounts>) {
        let Some(written) = &held.written else {
            return;
        };
        let newest = held.newest();
        if written.at.load(Ordering::Acquire) >= newest.at {
            return;
        }
        {
            let mut last = written.last.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|last| last.elapsed() < COUNTS_WRITE_INTERVAL)
                || written.writing.swap(true, Ordering::AcqRel)
            {
                return;
            }
            *last = Some(std::time::Instant::now());
        }
        let held = Arc::clone(held);
        let reserves = self.figures.reserve(key, newest.dense.filled_at);
        let bound = Arc::clone(&self.figures.disk_bound);
        let root = self.figures.dir.clone();
        self.figures.worker.submit(move || {
            let Some(written) = &held.written else { return };
            let newest = held.newest();
            persist::write_counts(&written.dir, &written.stem, &newest, reserves.as_deref());
            written.at.fetch_max(newest.at, Ordering::AcqRel);
            written.writing.store(false, Ordering::Release);
            if let Some(root) = root {
                persist::hold_under(&root, bound.load(Ordering::Relaxed));
            }
        });
    }

    /// The correction for `subtracted`, the mask's `minus` below the base: held, or worked out from
    /// the level's denied-row labels. `None` where those labels are being read in the background.
    #[allow(clippy::too_many_arguments)]
    fn deny_correction(
        &self,
        served: &ServedView<'_>,
        layer: &str,
        level: u32,
        level_version: u64,
        column: &Arc<RowColumn>,
        subtracted: Bitmap,
        places: &[Placement<'_>],
    ) -> Result<Option<Arc<DenyCorrection>>> {
        let identity = served.mask_identity;
        let mut denied = served.denied.clone();
        denied.remove_range(column.base_rows()..);
        let (deny_version, failing) = deny_version(served, column.base_rows(), &subtracted);
        let key = DenyKey {
            terms: identity.terms,
            identity: identity.fragment_identity,
            view: served.name.to_string(),
            layer: layer.to_string(),
            level,
            column: column.identity(),
            level_version,
            deny_version,
            failing,
        };
        if let tessera_cache::Peek::Ready(held) = self.figures.denies.peek(&key) {
            return Ok(Some(held));
        }
        let labels = match subtracted.intersect(&denied) {
            false => None,
            true => {
                let address = (served.name.to_string(), layer.to_string(), level);
                let wanted = Wanted {
                    identity: identity.fragment_identity,
                    column,
                    at: level_version,
                    denied: &denied,
                };
                match self
                    .figures
                    .labels_for(&address, &wanted, places, LABELS_READ_INLINE)
                {
                    Some(labels) => Some(labels),
                    None => {
                        self.figures.label_in_background(
                            address,
                            Arc::clone(column),
                            level_version,
                            denied.clone(),
                            identity.fragment_identity,
                            places_of_view(
                                Arc::clone(&served.generation.bundle),
                                served.name.to_string(),
                            ),
                        );
                        return Ok(None);
                    }
                }
            }
        };
        let cancel = served.cancel.clone().unwrap_or_default();
        self.figures
            .denies
            .get_or_try_build_waiting(key, &cancel, || {
                Ok::<_, std::convert::Infallible>(DenyCorrection::of(
                    subtracted,
                    labels.as_deref(),
                    column,
                    places,
                ))
            })
            .map(Some)
            .map_err(|ended| match ended {
                tessera_cache::WaitingBuildError::Wait(ended) => wait_error(ended),
                tessera_cache::WaitingBuildError::Build(never) => match never {},
            })
    }

    /// The figures of the whole composed mask, walked: what a request is answered from where the
    /// fragment's figures cannot be.
    #[allow(clippy::too_many_arguments)]
    fn exact_figures(
        &self,
        served: &ServedView<'_>,
        layer: &str,
        level: u32,
        level_version: u64,
        column: &Arc<RowColumn>,
        mask: &crate::compose::EffectiveMask,
        geometry: Geometry,
    ) -> Result<Option<Arc<Figures>>> {
        use crate::compose::WholeMask;
        let key = FiguresKey::Exact(served.mask_identity.exact_key(
            served.name,
            layer,
            level,
            level_version,
            geometry,
        ));
        let held = self
            .figures
            .get_or_build(key, &served.turn, served.cancel.as_ref(), |build| {
                self.figures.counters.exact.fetch_add(1, Ordering::Relaxed);
                let places =
                    (geometry != Geometry::None).then(|| Placement::of_segments(&served.segments));
                let walked = self.count_pool.install(|| {
                    #[cfg(feature = "fault-injection")]
                    self.switches.hold_masked_count_build_if_wanted();
                    column.accumulate(mask.visible_all(), places.as_deref(), &|| build.give_way())
                })?;
                let (dense, _) = Dense::of(level_version, walked);
                Some(FragmentCounts::new(CountsAt::of(dense), None))
            })
            .map_err(wait_error)?;
        Ok(Some(Arc::new(Figures {
            counts: held.newest(),
            reserves: None,
            deny: None,
            tail: None,
            len: column.len(),
            geometry,
            exact: None,
        })))
    }
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
        let (view, layer, level) = address;
        let dir = self
            .dir
            .as_ref()
            .map(|root| persist::identity_dir(root, &wanted.identity));
        let stem = persist::denied_stem(view, layer, *level);
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

    /// Queue every held level's labels to be brought to `generation`'s denied rows, at the version
    /// of the form `projections` holds for it: what a deny does once it is acknowledged.
    pub(crate) fn refresh_denied(
        &self,
        generation: &crate::Generation,
        projections: &crate::artifacts::ArtifactProjections,
    ) {
        for address in self.labels.held.addresses() {
            let (view, layer, level) = &address;
            let Some((rows, at)) = projections.held_form_at(view, layer, *level) else {
                continue;
            };
            let Some(column) = rows.column_shared() else {
                continue;
            };
            let Some(mut denied) = generation.denied().get(view).cloned() else {
                continue;
            };
            denied.remove_range(column.base_rows()..);
            self.label_in_background(
                address.clone(),
                Arc::clone(column),
                at,
                denied,
                generation.bundle_identity(),
                places_of_view(Arc::clone(&generation.bundle), view.clone()),
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
    let (view, layer, level) = address;
    let stem = persist::denied_stem(view, layer, *level);
    while let Some(newest) = held.to_write(address) {
        persist::write_denied(&persist::identity_dir(root, identity), &stem, &newest);
        held.wrote(address, newest.at);
    }
    persist::hold_under(root, bound.load(Ordering::Relaxed));
}

/// What a correction for `subtracted`, the mask's `minus` below `base_rows`, is a function of
/// beside its fragment: the view's deny version, and the overlay's version where the mask also
/// subtracts buffered rows the viewer fails below the base, which no deny version follows.
fn deny_version(
    served: &ServedView<'_>,
    base_rows: u32,
    subtracted: &Bitmap,
) -> (u64, Option<u64>) {
    let mut denied = served.denied.clone();
    denied.remove_range(base_rows..);
    let failing = !subtracted.is_subset(&denied);
    (
        served.generation.deny_version(served.name),
        failing.then_some(served.mask_identity.overlay_version),
    )
}

/// A wait on a correction's build, or the build's own error.
fn waited(ended: tessera_cache::WaitingBuildError<EngineError>) -> EngineError {
    match ended {
        tessera_cache::WaitingBuildError::Wait(ended) => wait_error(ended),
        tessera_cache::WaitingBuildError::Build(e) => e,
    }
}

fn wait_error(ended: tessera_cache::WaitEnded) -> EngineError {
    match ended {
        tessera_cache::WaitEnded::Budget => EngineError::CountsBuilding,
        tessera_cache::WaitEnded::Cancelled => EngineError::Cancelled,
    }
}
