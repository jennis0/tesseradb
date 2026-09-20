//! The per-deployment cache of row forms, and the persisted structures it adopts.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use croaring::Bitmap;

use tessera_lifecycle::membership::ArtifactStore;
use tessera_types::layer::LayerDeclaration;


use crate::containment::ContainmentPartition;
use crate::row_column::RowColumn;
use crate::tile_index::TileIndex;

use super::*;

/// What a cached [`ArtifactRows`] was built from: the prefix (a fold renumbers the base row
/// space), the view (row space is per view), and the level's version (a publication adds
/// memberships a stale form would serve as missing). A mismatch on any term rebuilds the form.
///
/// Keyed on the level's version rather than the store's, so one suppression, one growth or one
/// publication elsewhere does not invalidate every level's form in every view.
///
/// The segments version is not a term: a flush extends a held form instead of invalidating it
/// ([`ArtifactProjections::extend_flushed`]), and a merge permutes rows the form holds and is
/// caught by [`ArtifactRows::covers`] at every hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProjectionKey {
    pub(super) prefix: String,
    pub(super) view: String,
    pub(super) level_version: u64,
    /// The geometry an *attribute* predicate's membership was evaluated against, and `0` for every
    /// other level. A stored or spatial membership is instead extended and rebased in place
    /// ([`ArtifactProjections::extend_flushed`], [`ArtifactProjections::rebase_merged`]); an
    /// attribute predicate's membership is the value column, rebuilt whenever the geometry moves.
    pub(super) live: u64,
}

impl ProjectionKey {
    /// Whether a form held under this key answers for `wanted` — the level as last published,
    /// which may be up to a tick behind the store. Every term but the level version is an
    /// equality; the level version is a floor, so a form waiting for its next tick is served as
    /// last published, which can only undercount.
    pub(super) fn stale_form_of(&self, wanted: &ProjectionKey) -> bool {
        self.prefix == wanted.prefix
            && self.view == wanted.view
            && self.live == wanted.live
            && self.level_version <= wanted.level_version
    }
}

/// `(view, layer, level)` — what one cached projection is *for*, as against the [`ProjectionKey`]
/// that says when it stops being valid. Replace-on-mismatch, not an LRU, so the map is bounded by
/// the number of live triples rather than a capacity.
///
/// The view is here and not in [`PartitionAddress`]: a containment expression names entities'
/// terms, so no row space is involved and one file answers for every view; an extent is a pair of
/// rows, answering for exactly the view it was projected through.
pub(super) type LevelAddress = (String, String, u32);

/// This level, of this layer, in this view, under this prefix, at this version — the coordinate a
/// level's derived structures are claimed, composed and filed at.
pub(super) struct Coordinate<'a> {
    pub(super) prefix: &'a str,
    pub(super) view: &'a str,
    pub(super) layer: &'a str,
    pub(super) level: u32,
    pub(super) level_version: u64,
}

impl Coordinate<'_> {
    /// Where this level's row form, tile index and row column are filed.
    pub(super) fn address(&self) -> LevelAddress {
        (self.view.to_string(), self.layer.to_string(), self.level)
    }

    pub(super) fn partition_address(&self) -> PartitionAddress {
        (self.layer.to_string(), self.level)
    }

    pub(super) fn derived_key(&self) -> DerivedKey {
        DerivedKey::of(self.prefix, self.level_version)
    }

    /// What a row form of this level is valid at. `live` is the caller's because only the caller
    /// knows whether the level evaluates a value column.
    pub(super) fn projection_key(&self, live: u64) -> ProjectionKey {
        ProjectionKey {
            prefix: self.prefix.to_string(),
            view: self.view.to_string(),
            level_version: self.level_version,
            live,
        }
    }
}

/// `(layer, level)` — what one cached partition is *for*. No view, because a containment
/// expression is over terms and no row space is involved. No prefix, because the prefix is a
/// validity term; putting it in the address would leave every fold's entries behind instead of
/// replacing them.
pub(super) type PartitionAddress = (String, u32);

/// What a cached [`ContainmentPartition`], an adopted [`TileIndex`] and an adopted [`RowColumn`]
/// were composed or projected under — one key for all three. The prefix fixes the base row space
/// and the level's version fixes the records, and for a partition the postings it was composed
/// from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DerivedKey {
    pub(super) prefix: String,
    pub(super) level_version: u64,
}

impl DerivedKey {
    pub(super) fn of(prefix: &str, level_version: u64) -> Self {
        DerivedKey {
            prefix: prefix.to_string(),
            level_version,
        }
    }
}

/// What one accepted write did to a level, held by the executor until the next flush tick and
/// applied to the level's row forms there ([`ArtifactProjections::publish`]). Entity space and a
/// delta, exactly as the log record carries it, applied once per view over that view's own row
/// space.
///
/// `before` is the level version this write followed. The deltas of an interval carry consecutive
/// versions, so a form at version *v* takes the ones from *v* on and none it already holds.
#[derive(Debug)]
pub struct LevelDelta {
    pub before: u64,
    pub kind: DeltaKind,
}

/// The four shapes a [`LevelDelta`] takes, one per route by which artifact state enters.
#[derive(Debug)]
pub enum DeltaKind {
    /// One `WalRecord::ArtifactGrow` decoded: `(ordinal, the entities joining)` per membership it
    /// grew, and one page per generating set it moved. One record carries both because one record
    /// moves the level's version once.
    Grown {
        joins: Vec<(u32, Bitmap)>,
        pages: Vec<SetPage>,
    },
    /// The ordinals a publication claimed. The records themselves are read back from the store,
    /// which has already applied them.
    Published(Vec<u32>),
    /// The ordinals a fill changed: a parent list, an attachment, a shape or a content's values.
    /// The membership is untouched; the ordinal's records entry and generating sets are read from
    /// the store.
    Filled(Vec<u32>),
}

/// One page of one content's generating set, as the row forms take it.
#[derive(Debug)]
pub struct SetPage {
    pub ordinal: u32,
    pub rank: u16,
    /// The entities joining. Unioned into the served operator where the page holds no leave.
    pub joining: Bitmap,
    /// Whether this page re-derives the artifact's operators whole from entity truth: a union
    /// cannot express a leave, so a page holding one does and a page of joins alone does not.
    pub whole: bool,
}

/// Where the rows of a write's delta come from, per level — what [`ArtifactProjections::publish`]
/// is told beside the delta. `None` at the call is an attribute predicate, whose membership is the
/// value column and takes no delta.
pub enum DeltaRows<'a> {
    /// A stored membership: the delta's rows are the records' members, projected through the
    /// view's row space.
    Projected,
    /// A spatial membership: the rows of the ordinal asked for, resolved from its shape over every
    /// live segment. A growth never reaches a spatial level, so this is asked for a publication's
    /// new ordinals only.
    Resolved(&'a dyn Fn(u32) -> Bitmap),
}

/// Where a level's rows come from when a geometry publication brings its held form forward — what
/// [`ArtifactProjections::extend_flushed`] and [`ArtifactProjections::rebase_merged`] are told per
/// `(layer, level)`. `None` is a level that takes no delta here: an attribute predicate.
pub enum SegmentRows {
    /// A stored membership: the segment's rows are projected from the records through its extent.
    Projected,
    /// A spatial membership: the segment resolved against the level's shapes, segment-local rows
    /// per ordinal, parallel to the level's ordinals.
    Resolved(Arc<Vec<Option<Bitmap>>>),
}

/// One held row form, what it describes and how far its rows have been brought.
#[derive(Debug)]
pub(super) struct Held {
    pub(super) key: ProjectionKey,
    /// The `segments_version` the form's rows were last brought to — the generation it was built
    /// against, or the last one whose publication extended or rebased it on the executor.
    pub(super) at: u64,
    pub(super) rows: Arc<ArtifactRows>,
}

/// The engine cache directory's subdirectory for row-column compositions. Its own directory rather
/// than the cache root, so the open-time sweep cannot reach the fragment cache beside it.
pub const ROW_COLUMN_SCRATCH_DIR: &str = "row-columns";

// `Default` is [`ArtifactProjections::new`]'s own scaffold and not a constructor. Every field but
// the scratch path is an empty cache, and the scratch path a default gives is a directory no
// composition can write through. `new` fills it in; a caller that used `default` directly would
// get a projection that declined every level it was asked for.
#[derive(Debug, Default)]
pub struct ArtifactProjections {
    pub(super) cached: Mutex<BTreeMap<LevelAddress, Held>>,
    /// The containment partitions, keyed without the view: the expression names entities' terms,
    /// so two views of the same level compose the same table, and composing it is costly enough to
    /// share. The row form's projection-loss test still stays per view.
    pub(super) partitions_held: Mutex<BTreeMap<PartitionAddress, (DerivedKey, ContainmentPartition)>>,
    /// The fold-written tile indexes adopted at open, waiting for the level's first request to
    /// claim one. Claimed once and then dropped, unlike the map above it: an index belongs to one
    /// view, so once that view's row form has taken it there is nothing left for a second reader.
    pub(super) indexes_held: Mutex<BTreeMap<LevelAddress, (DerivedKey, TileIndex)>>,
    /// The fold-written row-major columns adopted at open, waiting for the level's first request to
    /// claim one. [`Self::indexes_held`]'s map, one structure along: claimed once and then dropped,
    /// because a column belongs to one view's row form.
    pub(super) columns_held: Mutex<BTreeMap<LevelAddress, (DerivedKey, RowColumn)>>,
    /// The base half of an attribute predicate's row column, per `(view, layer, level)`. Held
    /// rather than claimed, unlike [`Self::columns_held`]: it is taken again at every flush, since
    /// the form above it rebuilds on the geometry moving and the base does not.
    pub(super) predicate_bases: Mutex<BTreeMap<LevelAddress, (DerivedKey, Arc<RowColumn>)>>,
    /// How many forms this has built since the engine opened. Read by the fold's own log line and
    /// by [`crate::Engine::artifact_cache_builds`].
    pub(super) builds: std::sync::atomic::AtomicU64,
    /// How many containment partitions this has composed since the engine opened. Under a foreign
    /// plugin it stays at zero while `builds` climbs. Operator plane only.
    pub(super) partitions: std::sync::atomic::AtomicU64,
    /// How many partitions this adopted from the prefix at open rather than composing.
    adopted: std::sync::atomic::AtomicU64,
    /// How many fold-written tile indexes this claimed from the prefix rather than deriving.
    pub(super) indexes_adopted: std::sync::atomic::AtomicU64,
    /// How many fold-written row-major columns this claimed from the prefix rather than composing.
    pub(super) columns_adopted: std::sync::atomic::AtomicU64,
    /// How many levels were recorded row-major and are being served artifact-major: their
    /// memberships turned out to overlap, or the fold's file would not open. Both layouts answer
    /// identically. Counted per build, not per request; it names no artifact and no principal.
    pub(super) fallbacks: std::sync::atomic::AtomicU64,
    /// How many row-major columns this composed rather than claiming from the prefix. A predicate
    /// level's base column counts here too and never has an adopted twin, since the fold writes no
    /// file for it.
    pub(super) columns_composed: std::sync::atomic::AtomicU64,
    /// Where a composition's partition buckets and the column it writes live — the deployment's
    /// cache directory, never the bundle.
    scratch: std::path::PathBuf,
}

/// Whether this layer's derived content is an accumulation over the mask — a centroid or a
/// bounding box, which are the two [`crate::histogram::MaskedGeometry`] answers. A layer that
/// declares neither pays nothing for one.
pub fn derives_accumulated_geometry(declaration: &LayerDeclaration) -> bool {
    declaration
        .content
        .computed
        .iter()
        .filter_map(|name| crate::derived::ComputedProperty::parse(name))
        .any(|p| {
            matches!(
                p,
                crate::derived::ComputedProperty::Centroid | crate::derived::ComputedProperty::Box
            )
        })
}

/// Whether a level of this layer may be served from its column alone — the one place the rule is
/// stated, so two callers cannot decide it differently and flip the level's form between requests.
/// A layer that derives a `hull` is excluded: a hull needs one artifact's rows materialised, unlike
/// the accumulations ([`crate::histogram::MaskedGeometry`]) a column-only level otherwise serves.
pub fn serves_column_only(declaration: &LayerDeclaration) -> bool {
    !declaration
        .content
        .computed
        .iter()
        .filter_map(|name| crate::derived::ComputedProperty::parse(name))
        .any(|p| p == crate::derived::ComputedProperty::Hull)
}

impl ArtifactProjections {
    pub fn new(scratch: impl Into<std::path::PathBuf>) -> Self {
        Self {
            scratch: scratch.into(),
            ..Self::default()
        }
    }

    pub(crate) fn scratch(&self) -> &std::path::Path {
        &self.scratch
    }

    pub fn builds(&self) -> u64 {
        self.builds.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn partitions(&self) -> u64 {
        self.partitions.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn adopted(&self) -> u64 {
        self.adopted.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn indexes_adopted(&self) -> u64 {
        self.indexes_adopted
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn columns_adopted(&self) -> u64 {
        self.columns_adopted
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn layout_fallbacks(&self) -> u64 {
        self.fallbacks.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn columns_composed(&self) -> u64 {
        self.columns_composed
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Adopts every derived file of `extents` whose level is still at the version it was written
    /// for.
    pub fn adopt_derived(
        &self,
        prefix_dir: &std::path::Path,
        prefix: &str,
        extents: &[tessera_store::manifest::DerivedExtent],
        store: &ArtifactStore,
    ) {
        self.adopt_all(prefix_dir, prefix, extents, store);
        self.adopt_indexes(prefix_dir, prefix, extents, store);
        self.adopt_columns(prefix_dir, prefix, extents, store);
    }

    /// Take the fold-written partitions this prefix's manifests name, for every level whose
    /// coordinate still holds: a stale partition would answer containment for a generating set
    /// that has since grown, the permissive direction, so the version check is an equality.
    ///
    /// Every failure is a drop, not an error: a file that will not map, a file whose framing
    /// refuses, a coordinate that has moved, each means the level is recomposed on first use, which
    /// is the answer every request took before the fold wrote anything.
    pub fn adopt_all(
        &self,
        prefix_dir: &std::path::Path,
        prefix: &str,
        extents: &[tessera_store::manifest::DerivedExtent],
        store: &ArtifactStore,
    ) {
        // Everything held for another prefix leaves here, so a request still building on the
        // outgoing generation cannot remove what this adoption inserts ([`Self::claim_index`]).
        self.partitions_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (key, _)| key.prefix == prefix);
        for extent in extents
            .iter()
            .filter(|e| e.form == tessera_store::manifest::DerivedForm::Containment)
        {
            let level_version = store.level_version(&extent.layer, extent.level);
            if level_version != extent.level_version {
                tracing::info!(
                    layer = %extent.layer,
                    level = extent.level,
                    composed_at = extent.level_version,
                    now = level_version,
                    "a fold-written containment partition is not adopted: the level has moved                      since it was composed, so it is recomposed on first use"
                );
                continue;
            }
            let path = prefix_dir.join(&extent.path);
            let partition = match ContainmentPartition::open(&path) {
                Ok(partition) => partition,
                Err(error) => {
                    // A fault rather than a cadence: the manifest names a file the prefix should
                    // hold and it did not open.
                    tracing::error!(
                        layer = %extent.layer,
                        level = extent.level,
                        path = %extent.path,
                        %error,
                        "ALARM: a containment partition named by the manifest would not open;                          containment is correct and the level recomposes on first use"
                    );
                    continue;
                }
            };
            self.adopted
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.partitions_held
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    (extent.layer.clone(), extent.level),
                    (DerivedKey::of(prefix, level_version), partition),
                );
        }
    }

    /// Take the fold-written tile indexes this prefix's manifests name, for every
    /// `(view, layer, level)` whose coordinate still holds. [`Self::adopt_all`]'s rule with a view
    /// on it, but a stale index is wrong the opposite way: a growth adds members the index's
    /// extents do not reach, so an artifact can be treated as settled — its whole membership
    /// assumed inside the viewport — when it is not, and its masked count then answers for the
    /// members in view rather than all of them. Equality on the version, and the view compared too.
    pub fn adopt_indexes(
        &self,
        prefix_dir: &std::path::Path,
        prefix: &str,
        extents: &[tessera_store::manifest::DerivedExtent],
        store: &ArtifactStore,
    ) {
        self.indexes_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (key, _)| key.prefix == prefix);
        for extent in extents {
            let (tessera_store::manifest::DerivedForm::TileIndex, Some(view)) =
                (&extent.form, extent.view.as_deref())
            else {
                continue;
            };
            let level_version = store.level_version(&extent.layer, extent.level);
            if level_version != extent.level_version {
                tracing::info!(
                    layer = %extent.layer,
                    level = extent.level,
                    view = %view,
                    projected_at = extent.level_version,
                    now = level_version,
                    "a fold-written tile index is not adopted: the level has moved since it was \
                     projected, so its extents are derived on first use"
                );
                continue;
            }
            let path = prefix_dir.join(&extent.path);
            let index = match TileIndex::open(&path) {
                Ok(index) => index,
                Err(error) => {
                    // A fault rather than a cadence: the manifest names a file the prefix should
                    // hold and it did not open.
                    tracing::error!(
                        layer = %extent.layer,
                        level = extent.level,
                        view = %view,
                        path = %extent.path,
                        %error,
                        "ALARM: a tile index named by the manifest would not open; candidacy is \
                         correct and the level's index is derived on first use"
                    );
                    continue;
                }
            };
            self.indexes_held
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    (view.to_string(), extent.layer.clone(), extent.level),
                    (DerivedKey::of(prefix, level_version), index),
                );
        }
    }

    /// Take the fold-written row-major columns this prefix's manifests name, for every
    /// `(view, layer, level)` whose coordinate still holds. [`Self::adopt_indexes`]' rule, and a
    /// stale one is wrong the same way: a growth adds rows the column does not label, and an
    /// unlabelled row is one no artifact claims, so its masked count comes back short.
    ///
    /// The manifest's layout tag is checked against the file's own magic rather than trusted over
    /// it: a mis-described file refuses at the first bytes, landing here as a drop and a
    /// recomposition rather than a misread column.
    pub fn adopt_columns(
        &self,
        prefix_dir: &std::path::Path,
        prefix: &str,
        extents: &[tessera_store::manifest::DerivedExtent],
        store: &ArtifactStore,
    ) {
        self.columns_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (key, _)| key.prefix == prefix);
        for extent in extents {
            let (tessera_store::manifest::DerivedForm::RowColumn { layout }, Some(view)) =
                (&extent.form, extent.view.as_deref())
            else {
                continue;
            };
            let layout = *layout;
            let level_version = store.level_version(&extent.layer, extent.level);
            if level_version != extent.level_version {
                tracing::info!(
                    layer = %extent.layer,
                    level = extent.level,
                    view = %view,
                    written_at = extent.level_version,
                    now = level_version,
                    "a fold-written row-major column is not adopted: the level has moved since it \
                     was written, so it is recomposed on first use"
                );
                continue;
            }
            let path = prefix_dir.join(&extent.path);
            let column = match RowColumn::open(&path, layout) {
                Ok(column) => column,
                Err(error) => {
                    // A fault rather than a cadence: the manifest names a file the prefix should
                    // hold, in a form it claims to be in, and it did not open as that form.
                    tracing::error!(
                        layer = %extent.layer,
                        level = extent.level,
                        view = %view,
                        path = %extent.path,
                        layout = ?layout,
                        %error,
                        "ALARM: a row-major column named by the manifest would not open as the \
                         form the manifest names; the level is recomposed on first use"
                    );
                    continue;
                }
            };
            self.columns_held
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    (view.to_string(), extent.layer.clone(), extent.level),
                    (DerivedKey::of(prefix, level_version), column),
                );
        }
    }

    /// The form held for one `(view, layer, level)`, or `None` where none is.
    pub fn held_form(&self, view: &str, layer: &str, level: u32) -> Option<Arc<ArtifactRows>> {
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(view.to_string(), layer.to_string(), level))
            .map(|held| Arc::clone(&held.rows))
    }

    /// File `held` under `address` unless what is there is later on either term — the segments
    /// version its rows were brought to, or the level version its records describe (see
    /// [`Held::at`]). A form later on one term and earlier on the other is kept out and what it
    /// built discarded, so a build straddling a flush or a merge cannot replace a newer form with
    /// an older one and send masked counts backwards for the next reader.
    pub(super) fn insert_newest(&self, address: LevelAddress, held: Held) {
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if cached.get(&address).is_some_and(|standing| {
            standing.at > held.at || standing.key.level_version > held.key.level_version
        }) {
            return;
        }
        cached.insert(address, held);
    }

    /// How many forms are held. Operator plane only.
    pub fn held(&self) -> usize {
        self.cached.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Drop everything held for one layer, in every view. Called when the layer is dropped: a
    /// dropped name is tombstoned and the serving path resolves it through the registry before
    /// reaching this cache, so a form left behind could never be handed to anybody, only stay
    /// resident.
    pub fn forget(&self, layer: &str) {
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, _), _| held != layer);
        self.partitions_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(held, _), _| held != layer);
        self.indexes_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, _), _| held != layer);
        self.columns_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, _), _| held != layer);
        // Held rather than claimed, so nothing else would ever remove it otherwise.
        self.predicate_bases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, _), _| held != layer);
    }

    /// Drop everything held for one `(layer, level)`, in every view — what a layout flip needs. The
    /// cached row form is replace-on-mismatch, so a fold's new prefix would replace it at the
    /// level's next request anyway; the held adoption maps are the problem, since a level that
    /// flipped to row-major is never asked for its tile index again and the last generation's copy
    /// is pinned for the process's life. Removal only, so its worst outcome is one level rebuilding
    /// what it would have adopted.
    pub fn forget_level(&self, layer: &str, level: u32) {
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, held_level), _| held != layer || *held_level != level);
        self.partitions_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(held, held_level), _| held != layer || *held_level != level);
        self.indexes_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, held_level), _| held != layer || *held_level != level);
        self.columns_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, held_level), _| held != layer || *held_level != level);
        self.predicate_bases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, held_level), _| held != layer || *held_level != level);
    }
}
