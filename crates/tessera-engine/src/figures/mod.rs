//! **A level's figures for one request**: per artifact, how many rows of the viewer's visible set
//! carry its label, and where the layer serves them, the mean position and the box of those rows.
//! Every verdict and every number served beside an artifact of a level with a row column is read
//! from here.
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
//! The box of `F − D` is the cached box unless a subtracted row lies on an edge. Each artifact's
//! entry keeps its eight most extreme rows on each side, so the next one not subtracted is the new
//! edge. Only a side whose reserve is all subtracted works the box out from the artifact's members.
//!
//! Where a level's denied-row labels are not yet held and too many to read inline, the request is
//! answered by a walk of its whole composed mask instead ([`cache::FiguresKey::Exact`]), the route
//! every request took before the fragment's counts existed.

mod cache;
mod counts;
mod denied;
mod persist;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use croaring::Bitmap;

use tessera_types::layer::LayerDeclaration;

use crate::derived::{place, Placement};
use crate::error::{EngineError, Result};
use crate::row_column::RowColumn;
use crate::viewport::ServedView;
use crate::Engine;

pub(crate) use cache::{DrawingTurn, FiguresCache, MaskIdentity};
pub use cache::{FiguresStats, DEFAULT_GIVE_WAY_MS};

use cache::{DenyKey, FiguresKey, FragmentCounts, FragmentKey, Tail, TailKey};
use counts::{CountsAt, Delta, Deltas, Dense};
use denied::{Brought, DeniedLabels, DenyCorrection};

/// The subdirectory of the engine's cache directory the figures are written to.
pub const FIGURES_DIR: &str = "figures";

/// How many new denied rows a request reads from the column itself. Past this, the labels are
/// read in the background and the request walks its whole mask meanwhile.
const LABELS_READ_INLINE: u64 = 4_096;

/// How many versions of one level's denied-row labels are kept, for requests still holding a form
/// from before a publication.
const LABEL_VERSIONS_KEPT: usize = 4;

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
    deny: Option<Arc<DenyCorrection>>,
    tail: Option<Arc<Tail>>,
    /// How many ordinals the level's column covers.
    len: usize,
    geometry: Geometry,
    /// What a box whose reserve is spent is worked out from.
    exact: Option<BoxSource>,
}

/// One request's column, fragment and segments, held for working out a box from its rows.
struct BoxSource {
    column: Arc<RowColumn>,
    projection: Arc<crate::projection::RowProjection>,
    base_rows: u32,
    /// The box of the placed rows among those given.
    box_of: BoxOf,
    cache: Arc<FiguresCache>,
}

/// See [`BoxSource::box_of`].
type BoxOf = Arc<dyn Fn(&Bitmap) -> Option<[u32; 4]> + Send + Sync>;

/// [`BoxOf`] over one view's segments.
fn box_of_view(bundle: Arc<tessera_store::read::Bundle>, view: String) -> BoxOf {
    Arc::new(move |rows: &Bitmap| {
        let view_data = bundle
            .partitions
            .values()
            .find_map(|partition| partition.views.get(&view))?;
        let segments = crate::viewport::segments_with_row_bases(&view, view_data).ok()?;
        box_of_rows(rows, &Placement::of_segments(&segments))
    })
}

/// The box of the placed rows of `rows`.
fn box_of_rows(rows: &Bitmap, places: &[Placement<'_>]) -> Option<[u32; 4]> {
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

impl Figures {
    /// Figures holding `counts` and nothing else, for tests of what reads them.
    #[cfg(test)]
    pub(crate) fn of_counts(counts: Vec<u32>) -> Self {
        Figures {
            len: counts.len(),
            counts: Arc::new(CountsAt::of_counts(0, counts)),
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
        let base = match &self.deny {
            Some(deny) => deny.bbox(ordinal, &self.counts, || self.exact_box(ordinal)),
            None => self.counts.bbox(ordinal),
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
    fn exact_box(&self, ordinal: u32) -> Option<[u32; 4]> {
        let source = self.exact.as_ref()?;
        source
            .cache
            .spent
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut fragment = source.projection.bitmap().clone();
        fragment.remove_range(source.base_rows..);
        if let Some(deny) = &self.deny {
            fragment.andnot_inplace(&deny.rows);
        }
        let rows = match source.column.members() {
            Some(members) => members.members(ordinal).to_bitmap().and(&fragment),
            None => {
                let mut rows = Vec::new();
                for row in fragment.iter() {
                    source.column.for_each_label(row, |held| {
                        if held == ordinal {
                            rows.push(row);
                        }
                    });
                }
                Bitmap::of(&rows)
            }
        };
        (source.box_of)(&rows)
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

        let Some(counts) = self.fragment_counts(
            served,
            layer,
            level,
            level_version,
            column,
            fragment,
            &places,
            geometry,
        )?
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

        Ok(Some(Arc::new(Figures {
            counts,
            deny,
            tail,
            len: column.len(),
            geometry,
            exact: (geometry != Geometry::None).then(|| BoxSource {
                column: Arc::clone(column),
                projection: Arc::clone(mask.projection()),
                base_rows,
                box_of: box_of_view(
                    Arc::clone(&served.generation.bundle),
                    served.name.to_string(),
                ),
                cache: Arc::clone(&self.figures),
            }),
        })))
    }

    /// The fragment's counts over the column's base rows at `level_version`: held, read from disk,
    /// or walked. `None` where what is held cannot be brought to that version, which a request
    /// still reading a form older than the newest held meets.
    #[allow(clippy::too_many_arguments)]
    fn fragment_counts(
        &self,
        served: &ServedView<'_>,
        layer: &str,
        level: u32,
        level_version: u64,
        column: &Arc<RowColumn>,
        fragment: &Bitmap,
        places: &[Placement<'_>],
        geometry: Geometry,
    ) -> Result<Option<Arc<CountsAt>>> {
        let identity = served.mask_identity;
        let base_rows = column.base_rows();
        let key = FiguresKey::Fragment(FragmentKey {
            terms: identity.terms,
            identity: identity.fragment_identity,
            view: served.name.to_string(),
            layer: layer.to_string(),
            level,
            column: column.identity(),
            geometry,
        });
        let dir = self
            .figures
            .dir
            .as_ref()
            .map(|root| persist::identity_dir(root, &identity.fragment_identity));
        let stem = persist::counts_stem(&identity.terms, served.name, layer, level, geometry);
        let held = self
            .figures
            .get_or_build(key, &served.turn, served.cancel.as_ref(), |build| {
                if let Some(dense) = dir
                    .as_deref()
                    .and_then(|dir| persist::read_counts(dir, &stem, level_version, geometry))
                {
                    self.figures
                        .loads
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Some(FragmentCounts::new(CountsAt::of(level_version, dense)));
                }
                self.figures
                    .fills
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                let counts = CountsAt::of(level_version, Dense::of(walked));
                if let Some(dir) = &dir {
                    if let Some(root) = &self.figures.dir {
                        persist::sweep_other_identities(root, &identity.fragment_identity);
                    }
                    let (dir, stem, dense) = (dir.clone(), stem.clone(), Arc::clone(&counts.dense));
                    background(move || persist::write_counts(&dir, &stem, level_version, &dense));
                }
                Some(FragmentCounts::new(counts))
            })
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
        if let (Some(counts), Some(dir)) = (&counts, dir) {
            self.persist_newest(&held, dir, stem, counts.at);
        }
        Ok(counts)
    }

    /// Write `held`'s newest counts once nothing else is writing them, where `at` is newer than
    /// what was last written.
    fn persist_newest(
        &self,
        held: &Arc<FragmentCounts>,
        dir: std::path::PathBuf,
        stem: String,
        at: u64,
    ) {
        use std::sync::atomic::Ordering;
        if held.persisted.load(Ordering::Acquire) >= at
            || held.persisting.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let held = Arc::clone(held);
        background(move || loop {
            let newest = held.newest();
            if held.persisted.load(Ordering::Acquire) < newest.at {
                persist::write_counts(&dir, &stem, newest.at, &newest.folded());
                held.persisted.store(newest.at, Ordering::Release);
            }
            held.persisting.store(false, Ordering::Release);
            // A version that arrived during the write, whose caller found the flag still set.
            if held.newest().at <= held.persisted.load(Ordering::Acquire)
                || held.persisting.swap(true, Ordering::AcqRel)
            {
                return;
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
        let failing = !subtracted.is_subset(&denied);
        let key = DenyKey {
            terms: identity.terms,
            identity: identity.fragment_identity,
            view: served.name.to_string(),
            layer: layer.to_string(),
            level,
            column: column.identity(),
            level_version,
            deny_version: served.generation.deny_version(served.name),
            failing: failing.then_some(identity.overlay_version),
        };
        if let tessera_cache::Peek::Ready(held) = self.figures.denies.peek(&key) {
            return Ok(Some(held));
        }
        let labels = match subtracted.intersect(&denied) {
            false => None,
            true => match self.denied_labels(
                served,
                layer,
                level,
                level_version,
                column,
                &denied,
                places,
            ) {
                Some(labels) => Some(labels),
                None => return Ok(None),
            },
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

    /// The labels of the view's denied base rows `denied` at `level_version`: held, brought forward
    /// from what is held, read from disk, or read from the column. `None` where more rows are new
    /// than a request reads inline, which are then read in the background.
    #[allow(clippy::too_many_arguments)]
    fn denied_labels(
        &self,
        served: &ServedView<'_>,
        layer: &str,
        level: u32,
        level_version: u64,
        column: &Arc<RowColumn>,
        denied: &Bitmap,
        places: &[Placement<'_>],
    ) -> Option<Arc<DeniedLabels>> {
        let identity = served.mask_identity.fragment_identity;
        let address = (served.name.to_string(), layer.to_string(), level);
        let held: Vec<Arc<DeniedLabels>> = self
            .figures
            .denied
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&address)
            .map(|versions| versions.iter().cloned().collect())
            .unwrap_or_default();
        if let Some(exact) = held.iter().find(|held| {
            held.identity == identity
                && held.column == column.identity()
                && held.at == level_version
                && held.rows == *denied
        }) {
            return Some(Arc::clone(exact));
        }
        let dir = self
            .figures
            .dir
            .as_ref()
            .map(|root| persist::identity_dir(root, &identity));
        let stem = persist::denied_stem(served.name, layer, level);
        let read = dir.as_deref().and_then(|dir| {
            persist::read_denied(dir, &stem, level_version, identity, column.identity())
        });
        let candidates = held
            .iter()
            .rev()
            .filter(|held| held.at <= level_version)
            .map(|held| held.as_ref())
            .chain(read.as_ref());
        for candidate in candidates {
            match candidate.brought(
                identity,
                column,
                level_version,
                denied,
                places,
                LABELS_READ_INLINE,
            ) {
                Brought::Ready(labels) => {
                    return Some(self.hold_labels(address, labels, dir, stem))
                }
                Brought::TooMany => {
                    self.label_in_background(served, address, column, level_version, denied);
                    return None;
                }
                Brought::Broken => continue,
            }
        }
        if denied.cardinality() > LABELS_READ_INLINE {
            self.label_in_background(served, address, column, level_version, denied);
            return None;
        }
        let labels = DeniedLabels::read(identity, column, level_version, denied, places);
        Some(self.hold_labels(address, labels, dir, stem))
    }

    /// Keep `labels` among the versions held for `address`, and write them to disk.
    fn hold_labels(
        &self,
        address: (String, String, u32),
        labels: DeniedLabels,
        dir: Option<std::path::PathBuf>,
        stem: String,
    ) -> Arc<DeniedLabels> {
        let labels = Arc::new(labels);
        hold_labels_in(&self.figures, address, Arc::clone(&labels));
        if let Some(dir) = dir {
            let written = Arc::clone(&labels);
            background(move || persist::write_denied(&dir, &stem, &written));
        }
        labels
    }

    /// Read every denied base row's labels for `address` on the count pool, unless that is under
    /// way already.
    fn label_in_background(
        &self,
        served: &ServedView<'_>,
        address: (String, String, u32),
        column: &Arc<RowColumn>,
        level_version: u64,
        denied: &Bitmap,
    ) {
        if !self
            .figures
            .labelling
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(address.clone())
        {
            return;
        }
        let figures = Arc::clone(&self.figures);
        let column = Arc::clone(column);
        let denied = denied.clone();
        let bundle = Arc::clone(&served.generation.bundle);
        let identity = served.mask_identity.fragment_identity;
        background(move || {
            let (view, layer, level) = &address;
            let view_data = bundle
                .partitions
                .values()
                .find_map(|partition| partition.views.get(view));
            if let Some(segments) =
                view_data.and_then(|data| crate::viewport::segments_with_row_bases(view, data).ok())
            {
                let places = Placement::of_segments(&segments);
                let labels = Arc::new(DeniedLabels::read(
                    identity,
                    &column,
                    level_version,
                    &denied,
                    &places,
                ));
                if let Some(root) = &figures.dir {
                    persist::write_denied(
                        &persist::identity_dir(root, &identity),
                        &persist::denied_stem(view, layer, *level),
                        &labels,
                    );
                }
                hold_labels_in(&figures, address.clone(), labels);
            }
            figures
                .labelling
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&address);
        });
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
                self.figures
                    .exact
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let places =
                    (geometry != Geometry::None).then(|| Placement::of_segments(&served.segments));
                let walked = self.count_pool.install(|| {
                    #[cfg(feature = "fault-injection")]
                    self.switches.hold_masked_count_build_if_wanted();
                    column.accumulate(mask.visible_all(), places.as_deref(), &|| build.give_way())
                })?;
                Some(FragmentCounts::new(CountsAt::of(
                    level_version,
                    Dense::of(walked),
                )))
            })
            .map_err(wait_error)?;
        Ok(Some(Arc::new(Figures {
            counts: held.newest(),
            deny: None,
            tail: None,
            len: column.len(),
            geometry,
            exact: None,
        })))
    }
}

/// Keep `labels` among the versions of `address`'s labels, newest last, dropping any of another
/// column or bundle.
fn hold_labels_in(
    figures: &FiguresCache,
    address: (String, String, u32),
    labels: Arc<DeniedLabels>,
) {
    let mut held = figures.denied.lock().unwrap_or_else(|e| e.into_inner());
    let versions = held.entry(address).or_default();
    versions.retain(|v| {
        v.identity == labels.identity && v.column == labels.column && v.at != labels.at
    });
    let at = versions.partition_point(|v| v.at < labels.at);
    versions.insert(at, labels);
    while versions.len() > LABEL_VERSIONS_KEPT {
        versions.pop_front();
    }
}

/// Run `work` on a thread of its own: a write to the cache directory or a read of denied rows'
/// labels, neither of which a request waits for.
fn background(work: impl FnOnce() + Send + 'static) {
    if let Err(error) = std::thread::Builder::new()
        .name("tessera-figures".to_string())
        .spawn(work)
    {
        tracing::warn!(%error, "a thread for a level's figures could not be started; the work is skipped");
    }
}

fn wait_error(ended: tessera_cache::WaitEnded) -> EngineError {
    match ended {
        tessera_cache::WaitEnded::Budget => EngineError::CountsBuilding,
        tessera_cache::WaitEnded::Cancelled => EngineError::Cancelled,
    }
}
