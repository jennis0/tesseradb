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

/// What a cached [`ArtifactRows`] was built from. **Every term is a reason the projection would be
/// wrong**, and a mismatch on any of them rebuilds:
///
/// - the **prefix**, because a fold renumbers the base row space wholesale, so a projection built
///   over the old one names other people's documents;
/// - the **view**, because row space is per view;
/// - the **level's version**, because a publication adds memberships the projection has never
///   seen — and a cached projection that silently omitted them would serve a level with its
///   newest clusters absent, indistinguishable from clusters that failed their criterion.
///
/// **The level's version and not the store's**, which is what this carried until the scale
/// campaign measured the difference (`design/artifact-serving-at-scale.md` §8.1): a store-wide
/// counter makes one suppression, one growth or one publication *anywhere* invalidate every
/// level's form in every view — 138 s of rebuild at 10⁷ artifacts over 10⁹ rows, so under any
/// read-write load the cache never survives to be used. The narrower key is sound because the
/// build reads exactly two things: the records of one `(layer, level)`, which is what that level's
/// version counts, and the view's row space, which is fixed by the two terms above it.
///
/// **The segments version is deliberately not a term, and it is not the row space's only guard.**
/// A stored level's form covers extent rows, so a flush and a merge both matter to it — but
/// neither is answered by rebuilding at a version. A flush **extends** the form
/// ([`ArtifactProjections::extend_flushed`]), which keying on the segments version would turn into
/// a whole-level projection per flush for a set of bits that mostly did not move; a merge permutes
/// rows the form holds and is caught by [`ArtifactRows::covers`], read beside this key at every
/// hit. The one operation that renumbers the base is the fold, and a fold publishes a new prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProjectionKey {
    pub(super) prefix: String,
    pub(super) view: String,
    pub(super) level_version: u64,
    /// **The geometry an *attribute* predicate's membership was evaluated against, and `0` for
    /// every other level.**
    ///
    /// A stored membership and a spatial one are sets the form can be **extended** by the segment
    /// a flush publishes and **rebased** over the extent a merge publishes
    /// ([`ArtifactProjections::extend_flushed`], [`ArtifactProjections::rebase_merged`]), which
    /// is why `segments_version` is not a term above. An attribute predicate's membership is the
    /// value column: its live tail covers the rows a flush appended and is read per request, so
    /// there is no delta to take and the form is rebuilt when the geometry moves — once per flush
    /// rather than once per fold, over a column that is four bytes a row.
    ///
    /// **Zero rather than an `Option`**, because a level either evaluates a column or it does not:
    /// an enumerated or a spatial level filed under a geometry would rebuild on every flush what a
    /// walk of one extent brings forward.
    pub(super) live: u64,
}

impl ProjectionKey {
    /// Whether a form held under this key answers for `wanted` — the level as last published,
    /// which may be up to a tick behind the store (`ingest.md` §1.3, §10 ruling 6).
    ///
    /// Every term but the level version is an equality: a form under another prefix, of another
    /// view, or of an attribute predicate whose value column the geometry has moved, describes
    /// something else. The level version is a floor, because the deltas the store has taken since
    /// reach the form at the tick and until then the form is what was published — stale in the
    /// direction that understates.
    pub(super) fn stale_form_of(&self, wanted: &ProjectionKey) -> bool {
        self.prefix == wanted.prefix
            && self.view == wanted.view
            && self.live == wanted.live
            && self.level_version <= wanted.level_version
    }
}

/// One row-space projection per `(view, layer, level)`, rebuilt when its [`ProjectionKey`] moves.
///
/// **Costly to build and therefore never built on a request that can reuse one.**
/// `RowSpace::project` decodes a whole membership; at corpus scale that is the "seconds, not
/// milliseconds" cost `RowProjection` carries the same warning about. This is paid at the first
/// request after a generation move or a publication, and by nothing else.
///
/// **Replace-on-mismatch, not an LRU.** The key names the only generation a projection is valid
/// for, so a stale entry has no value to retain — keeping one would be keeping a wrong answer
/// warm. The map is therefore bounded by the number of live `(view, layer, level)` triples rather
/// than by a capacity anyone has to tune.
/// `(view, layer, level)` — what one cached projection is *for*, as against the
/// [`ProjectionKey`] that says when it stops being valid.
///
/// **The view is here and not in [`PartitionAddress`]**, and that is the whole difference between
/// the two structures: a containment expression names entities' terms, so no row space is involved
/// in it and one file answers for every view; an extent is a pair of **rows**, so it answers for
/// exactly the view it was projected through. The fold-written tile indexes and row-major columns
/// are filed here for that reason.
pub(super) type LevelAddress = (String, String, u32);

/// **This level, of this layer, in this view, under this prefix, at this version** — the coordinate
/// a level's derived structures are claimed, composed and filed at, made once at the entry point
/// and handed to everything below it.
///
/// Borrows throughout, and it outlives nothing: what a map holds is the owned address and key this
/// derives ([`Self::address`], [`Self::derived_key`]), which is the one place each is derived.
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

    /// Where this level's containment partition is filed — see [`PartitionAddress`].
    pub(super) fn partition_address(&self) -> PartitionAddress {
        (self.layer.to_string(), self.level)
    }

    /// What a derived structure of this level is valid at — see [`DerivedKey`].
    pub(super) fn derived_key(&self) -> DerivedKey {
        DerivedKey::of(self.prefix, self.level_version)
    }

    /// What a row form of this level is valid at — see [`ProjectionKey`], whose `live` term is the
    /// caller's because only the caller knows whether the level evaluates a value column.
    pub(super) fn projection_key(&self, live: u64) -> ProjectionKey {
        ProjectionKey {
            prefix: self.prefix.to_string(),
            view: self.view.to_string(),
            level_version: self.level_version,
            live,
        }
    }
}

/// `(layer, level)` — what one cached partition is *for*.
///
/// **No view**, because the expression is over terms and no row space is involved in it; and **no
/// prefix**, for the reason [`LevelAddress`] carries none: the prefix is a *validity* term, so
/// putting it in the address would leave every fold's entries behind for the process's life
/// instead of replacing them.
pub(super) type PartitionAddress = (String, u32);

/// What a cached [`ContainmentPartition`], an adopted [`TileIndex`] and an adopted [`RowColumn`]
/// were composed or projected under — **one key for all three**, the address they are filed under
/// saying which level and, for the two that are row-addressed, which view.
///
/// The prefix fixes the base row space (a fold renumbers it wholesale; a flush and a merge leave it
/// alone, which is why the segments version is not a term — [`ProjectionKey`] argues it); the
/// level's version fixes the records, and for a partition the postings it was composed from.
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

/// **What one accepted write did to a level**, held by the executor until the next flush tick and
/// applied to the level's row forms there ([`ArtifactProjections::publish`], `ingest.md` §1.3 and
/// §10, ruling 6).
///
/// Entity space and a delta, exactly as the log record carries it — a row-space set would be a
/// frozen projection, and this is applied once per view over that view's own row space.
///
/// **`before` is the level version this write followed**, and it is what lets a form be brought
/// from wherever it stands to the store's present: the deltas of an interval carry consecutive
/// versions, so a form at version *v* takes the ones from *v* on and none of the ones it already
/// holds.
#[derive(Debug)]
pub struct LevelDelta {
    pub before: u64,
    pub kind: DeltaKind,
}

/// The four shapes a [`LevelDelta`] takes, one per route by which artifact state enters.
#[derive(Debug)]
pub enum DeltaKind {
    /// One `WalRecord::ArtifactGrow` decoded: `(ordinal, the entities joining)` per membership
    /// it grew, which is the same delta `ArtifactStore::grow` applied to the records, and one page
    /// per generating set it moved. One record carries both because one record moves the level's
    /// version once.
    Grown {
        joins: Vec<(u32, Bitmap)>,
        pages: Vec<SetPage>,
    },
    /// The ordinals a publication claimed. The records themselves are read back from the store,
    /// which has already applied them.
    Published(Vec<u32>),
    /// The ordinals a fill changed. A fill supplies a fixed part — a parent list, an attachment, a
    /// shape or a content's values — so what the form takes from it is the record again, not rows:
    /// the membership is untouched and the ordinal's records entry and generating sets are read
    /// from the store.
    Filled(Vec<u32>),
}

/// One page of one content's generating set, as the row forms take it (`ingest.md` §1.1).
#[derive(Debug)]
pub struct SetPage {
    pub ordinal: u32,
    pub rank: u16,
    /// The entities joining. Unioned into the served operator where the page holds no leave.
    pub joining: Bitmap,
    /// **Whether this page re-derives the artifact's operators whole from entity truth**, which a
    /// page holding any leave does and a page of joins alone does not (`ingest.md` §1.1, §4.1). A
    /// union cannot express a leave, and a cardinality moved down against an operator that still
    /// holds the leaver is a pair that was never derived together. A page that empties the set
    /// withdraws the content, which moves every rank above it, and that is re-derived for the same
    /// reason.
    pub whole: bool,
}

/// **Where the rows of a write's delta come from**, per level — what
/// [`ArtifactProjections::bring_forward`] is told beside the delta.
///
/// `None` at the call is an attribute predicate, whose membership is the value column and takes
/// no delta.
pub enum DeltaRows<'a> {
    /// A stored membership: the delta's rows are the records' members, projected through the
    /// view's row space.
    Projected,
    /// A spatial membership: the rows of the ordinal asked for, resolved from its shape over every
    /// live segment with the row bases applied. A growth never reaches a spatial level (the
    /// registry refuses one), so this is asked for a publication's new ordinals only.
    Resolved(&'a dyn Fn(u32) -> Bitmap),
}

/// **Where a level's rows come from when a geometry publication brings its held form forward** —
/// what [`ArtifactProjections::extend_flushed`] and [`ArtifactProjections::rebase_merged`] are
/// told per `(layer, level)`. `None` is a level that takes no delta here: an attribute predicate.
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
    ///
    /// **What keeps a request from undoing the executor's work.** A request builds a form against
    /// the generation it loaded and inserts it when the build ends; at rung 3 a build is tens of
    /// seconds and the tick is ninety, so a build that straddles a flush or a merge is the
    /// ordinary case rather than a race. Inserted unconditionally, that form would replace one the
    /// publication had just extended or rebased with one that is a segment short or holds the
    /// consumed segments' rows — and the next request would find `covers` false and project the
    /// level again. So an insert keeps whichever of the two is at the later version
    /// ([`ArtifactProjections::insert_newest`]).
    pub(super) at: u64,
    pub(super) rows: Arc<ArtifactRows>,
}

/// The engine cache directory's subdirectory for row-column compositions
/// (`tessera_store::derived::project_row_column`). Its own directory rather than the cache root, so
/// the open-time sweep cannot reach the fragment cache beside it.
pub const ROW_COLUMN_SCRATCH_DIR: &str = "row-columns";

// **`Default` is [`ArtifactProjections::new`]'s own scaffold and not a constructor.** Every field
// but the scratch path is an empty cache, and the scratch path a default gives is empty, which is a
// directory no composition can write through. `new` fills it in and nothing else calls `default`; a
// caller that did would get a projection that declined every level it was asked for.
#[derive(Debug, Default)]
pub struct ArtifactProjections {
    pub(super) cached: Mutex<BTreeMap<LevelAddress, Held>>,
    /// The containment partitions, keyed **without the view**.
    ///
    /// **The expression is view-independent, and composing it is not cheap.** It names entities'
    /// terms, so two views of the same level compose the same table — but composing it walks every
    /// term's posting once (`crate::containment`), which at the demo corpus's 54,794 signatures is
    /// the dear half of a level's build. Held here, a second view of a level pays nothing for it.
    ///
    /// **What stays per view is the projection-loss test**, and that is why this cache can be
    /// narrower than the one above rather than replacing it: a generating set that lost a member on
    /// the way into *this* view's row space can never be contained, and that question is asked of
    /// the row form at serving time.
    pub(super) partitions_held: Mutex<BTreeMap<PartitionAddress, (DerivedKey, ContainmentPartition)>>,
    /// The fold-written tile indexes adopted at open, waiting for the level's first request to
    /// claim one.
    ///
    /// **Claimed once and then dropped**, which is the difference from the map above it. A
    /// containment partition is *held* because two views of a level share it and a row form does
    /// not; an index belongs to one view, so once that view's row form has taken it there is
    /// nothing left for a second reader — and keeping a second `Arc` to eighty megabytes per level
    /// for the process's life is the retention bug `forget` exists to fix, one map along.
    pub(super) indexes_held: Mutex<BTreeMap<LevelAddress, (DerivedKey, TileIndex)>>,
    /// The fold-written row-major columns adopted at open, waiting for the level's first request to
    /// claim one.
    ///
    /// **[`Self::indexes_held`]'s map, one structure along**, with the same address and the same two
    /// validity terms: a column is addressed by **row**, so it answers for exactly the view whose
    /// row space it was written over, and the level's version is what says whether it still
    /// describes that level. Claimed once and then dropped, because a column belongs to one view's
    /// row form.
    pub(super) columns_held: Mutex<BTreeMap<LevelAddress, (DerivedKey, RowColumn)>>,
    /// The **base** half of an attribute predicate's row column, per `(view, layer, level)`.
    ///
    /// **Held rather than claimed**, which is the difference from [`Self::columns_held`]: a
    /// fold-written column is taken once and dropped, because the form that took it holds the only
    /// copy. A predicate's base is taken again at *every flush* — the form above it is rebuilt when
    /// the geometry moves and the base is not — so it stays here, replaced when its coordinate
    /// moves, and `with_tail` shares it rather than copying four bytes a row per flush.
    pub(super) predicate_bases: Mutex<BTreeMap<LevelAddress, (DerivedKey, Arc<RowColumn>)>>,
    /// How many forms this has built since the engine opened. **The cadence, counted** — what
    /// §8.1 is about is not the cost of one build but how many a write provokes, and that is a
    /// number nothing reported until the grain changed. Read by the fold's own log line and by
    /// [`crate::Engine::artifact_cache_builds`].
    pub(super) builds: std::sync::atomic::AtomicU64,
    /// How many containment partitions this has **composed** since the engine opened. **The gate,
    /// counted** — under a foreign plugin it stays at zero while `builds` climbs, which is what
    /// makes *the partition declined everywhere* distinguishable from *the partition was never
    /// asked for*. It is not `builds`' twin even under the builtin plugin: a second view of a
    /// level builds a second row form and reuses the one partition, which is the whole point of
    /// [`Self::partitions_held`]. Operator plane only; it names no artifact and no principal.
    pub(super) partitions: std::sync::atomic::AtomicU64,
    /// How many partitions this **adopted** from the prefix at open rather than composing — the
    /// other half of the same gauge. A deployment that folded and restarted should see this at the
    /// number of levels it holds and [`Self::partitions`] at zero; seeing it at zero and the other
    /// climbing says every coordinate was rejected, which is correct but is the expensive answer
    /// and an operator has no other way to notice it.
    adopted: std::sync::atomic::AtomicU64,
    /// How many fold-written tile indexes this **claimed** from the prefix rather than deriving —
    /// the same pair of gauges one structure along, and read the same way. Counted at the claim
    /// rather than at the adoption, because an entry the manifest named and no request ever asked
    /// for saved nothing.
    pub(super) indexes_adopted: std::sync::atomic::AtomicU64,
    /// How many fold-written row-major columns this **claimed** from the prefix rather than
    /// composing — the same pair of gauges one structure along, and read the same way.
    pub(super) columns_adopted: std::sync::atomic::AtomicU64,
    /// How many levels were **recorded** row-major and are being **served** artifact-major: their
    /// memberships turned out to overlap, or the fold's file would not open.
    ///
    /// **The number that says a pin is wrong**, and an operator has no other way to see it: both
    /// layouts answer identically, so a level that fell back is correct and merely slower than the
    /// operator asked for. Counted per build rather than per request, and it names no artifact and
    /// no principal.
    pub(super) fallbacks: std::sync::atomic::AtomicU64,
    /// How many row-major columns this **composed** rather than claiming from the prefix —
    /// [`Self::columns_adopted`]'s other half, read the same way. A deployment that folded and
    /// restarted should see this at zero and the adopted gauge at the number of row-major levels it
    /// holds; seeing the reverse says every coordinate was rejected, which is correct and is the
    /// expensive answer.
    ///
    /// **A predicate level's base column counts here too, and never has an adopted twin**: its
    /// labels come from the value column the predicate names rather than from a stored membership,
    /// so the fold writes no file for it and there is nothing for a reader to claim. Such a level
    /// contributes one composition per prefix per view, not one per flush — the base is cached in
    /// [`Self::predicate_bases`] and the tail above it is what the geometry moves.
    pub(super) columns_composed: std::sync::atomic::AtomicU64,
    /// Where a composition's partition buckets and the column it writes live — the deployment's
    /// cache directory, never the bundle.
    ///
    /// **A column is composed front to back into a file** and every pair it routes goes through a
    /// disk partition (`tessera_store::derived::project_row_column`), so the engine needs scratch
    /// of its own for the same reason a build needs `.build-tmp/`. The cache directory is where a
    /// derived, undigested, rebuilt-every-open file belongs — contracts §2.1 fixes what a bundle
    /// contains and this is not part of it, which is the argument the suggestion indexes already
    /// make one directory along.
    scratch: std::path::PathBuf,
}

/// **Whether this layer's derived content is an accumulation over the mask** — a centroid or a
/// bounding box, which are the two [`crate::histogram::MaskedGeometry`] answers.
///
/// A layer that declares neither pays nothing for one: the accumulation reads a grid position per
/// visible row and holds 36 B an ordinal, and a layer serving a count alone has no use for either.
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

/// **Whether a level of this layer may be served from its column alone** — the one place the rule
/// is stated, because two callers deciding it differently would flip the level's form between
/// requests.
///
/// ⊘ **A layer that derives a `hull` is excluded.** Every other per-artifact answer over a
/// column-only level is an accumulation — a count, a sum, a minimum and a maximum — which one pass
/// over the mask produces for every artifact at once ([`crate::histogram::MaskedGeometry`]). A hull
/// is not: it is a function of the member *positions* themselves, so it needs one artifact's rows
/// materialised, and on a scattered artifact that walk is the whole visible set. Such a level keeps
/// the artifact-major form and pays its residency.
pub fn serves_column_only(declaration: &LayerDeclaration) -> bool {
    !declaration
        .content
        .computed
        .iter()
        .filter_map(|name| crate::derived::ComputedProperty::parse(name))
        .any(|p| p == crate::derived::ComputedProperty::Hull)
}

impl ArtifactProjections {
    /// `scratch` is the directory compositions write through — see [`Self::scratch`].
    pub fn new(scratch: impl Into<std::path::PathBuf>) -> Self {
        Self {
            scratch: scratch.into(),
            ..Self::default()
        }
    }

    /// See [`Self::scratch`].
    pub(crate) fn scratch(&self) -> &std::path::Path {
        &self.scratch
    }

    /// See [`Self::builds`].
    pub fn builds(&self) -> u64 {
        self.builds.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// See [`Self::partitions`].
    pub fn partitions(&self) -> u64 {
        self.partitions.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// See [`Self::adopted`].
    pub fn adopted(&self) -> u64 {
        self.adopted.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// See [`Self::indexes_adopted`].
    pub fn indexes_adopted(&self) -> u64 {
        self.indexes_adopted
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// See [`Self::columns_adopted`].
    pub fn columns_adopted(&self) -> u64 {
        self.columns_adopted
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// See [`Self::fallbacks`].
    pub fn layout_fallbacks(&self) -> u64 {
        self.fallbacks.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// See [`Self::columns_composed`].
    pub fn columns_composed(&self) -> u64 {
        self.columns_composed
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Adopts every derived file of `extents` whose level is still at the version it was written
    /// for: containment partitions, tile indexes and row-major columns.
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
    /// coordinate still holds.
    ///
    /// **The coordinate is the whole rule and there is no weaker form of it.** A partition is a
    /// pure function of a level's records and the prefix's postings, so an entry describes the
    /// level at exactly one version; this adopts it where the level it seeded — after the manifest
    /// *and* after everything the WAL replayed over it — is at that same version, and drops it
    /// otherwise. Growth shrinks nothing and publication only adds, so a stale partition answers
    /// containment for a generating set that has since grown, and growth makes containment
    /// **harder** — the stale answer is the permissive one, on the one test **I3** exists to make
    /// conservative.
    ///
    /// **Every failure is a drop, not an error.** A file that will not map, a file whose framing
    /// refuses, a coordinate that has moved: each means *recompose this level on first use*, which
    /// is the answer every request took before the fold wrote anything. Refusing to open the
    /// engine over a derived structure that has a correct fallback would be a refusal outside the
    /// disclosure surface.
    pub fn adopt_all(
        &self,
        prefix_dir: &std::path::Path,
        prefix: &str,
        extents: &[tessera_store::manifest::DerivedExtent],
        store: &ArtifactStore,
    ) {
        // **Everything held for another prefix leaves here.** A claim leaves an entry held for a
        // prefix other than the one it asks under, so that a request still building on the
        // outgoing generation cannot remove what this adoption inserts ([`Self::claim_index`]);
        // what keeps such an entry from outliving its prefix is this purge, at the adoption that
        // supersedes it.
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
                    // Loud, because this one is a fault rather than a cadence: the manifest names
                    // a file the prefix should hold and it did not open.
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
    /// `(view, layer, level)` whose coordinate still holds.
    ///
    /// **[`Self::adopt_all`]'s rule with a view on it, and the direction of the mistake is the
    /// mirror image.** A stale containment partition answers containment for a generating set that
    /// has since grown, which is the permissive direction. A stale index is **narrow**: a growth
    /// added members the extents do not reach, so an artifact is settled whose membership is not
    /// inside the viewport at all — and a settled artifact's probe is taken against
    /// `viewport ∩ M_auth` on the strength of `membership ⊆ viewport`, which is then false. The
    /// answer that comes back is about the members in view rather than all of them, which is a
    /// *different question* silently substituted for the one a criterion reads. Equality, and the
    /// view compared too.
    ///
    /// **Every failure is a drop, not an error**, for [`Self::adopt_all`]'s reason: an index that
    /// is not adopted is derived on first use, which is what every request did before the fold
    /// wrote anything.
    pub fn adopt_indexes(
        &self,
        prefix_dir: &std::path::Path,
        prefix: &str,
        extents: &[tessera_store::manifest::DerivedExtent],
        store: &ArtifactStore,
    ) {
        // Purged for [`Self::adopt_all`]'s reason.
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
                    // Loud, because this one is a fault rather than a cadence: the manifest names
                    // a file the prefix should hold and it did not open.
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
    /// `(view, layer, level)` whose coordinate still holds.
    ///
    /// **[`Self::adopt_indexes`]' rule, and the direction of a stale one is the same: narrow.** A
    /// growth adds rows the column does not label, and an unlabelled row is one no artifact claims
    /// — so the artifact holding it silently stops being a candidate there, and its masked count
    /// comes back short. Equality on the version, and the view compared too.
    ///
    /// **The manifest's layout tag is checked against the file's own magic** rather than trusted
    /// over it (selection memo §5): a mis-described file refuses at the first bytes, which lands
    /// here as a drop and a recomposition rather than as a misread column.
    pub fn adopt_columns(
        &self,
        prefix_dir: &std::path::Path,
        prefix: &str,
        extents: &[tessera_store::manifest::DerivedExtent],
        store: &ArtifactStore,
    ) {
        // Purged for [`Self::adopt_all`]'s reason.
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
                    // Loud, because this one is a fault rather than a cadence: the manifest names a
                    // file the prefix should hold, in a form it claims to be in, and it did not
                    // open as that form.
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

    /// The form held for one `(view, layer, level)`, or `None` where none is — see
    /// [`crate::Engine::held_artifact_form_for_test`], its only caller.
    pub fn held_form(&self, view: &str, layer: &str, level: u32) -> Option<Arc<ArtifactRows>> {
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(view.to_string(), layer.to_string(), level))
            .map(|held| Arc::clone(&held.rows))
    }

    /// File `held` under `address` unless what is there is later on **either** term — the segments
    /// version its rows were brought to, or the level version its records describe (see
    /// [`Held::at`]).
    ///
    /// Both terms are straddles of the same kind: a request builds a form against the generation
    /// and the level version it loaded, and finishes after a flush has extended, a merge has
    /// rebased or a tick has published onto the form it means to replace. Inserted on the tuple
    /// alone, a build that straddled a flush would replace a form five level versions ahead of it
    /// with one at a newer segments version — and since a form behind the store is *served* rather
    /// than rebuilt (`ingest.md` §1.3), the level's masked counts would go backwards for the next
    /// reader. A form that is later on one term and earlier on the other is therefore kept out,
    /// and what it built is discarded; the flush or the tick that follows brings the standing form
    /// forward, and a form neither can bring forward is dropped there and built again.
    pub(super) fn insert_newest(&self, address: LevelAddress, held: Held) {
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if cached.get(&address).is_some_and(|standing| {
            standing.at > held.at || standing.key.level_version > held.key.level_version
        }) {
            return;
        }
        cached.insert(address, held);
    }

    /// How many forms are held. Operator plane only, beside [`Self::builds`] — a count of
    /// structures, naming no artifact and no principal.
    pub fn held(&self) -> usize {
        self.cached.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Drop everything held for one layer, in every view.
    ///
    /// **Called when the layer is dropped, and this is a retention fix rather than a correctness
    /// one.** A dropped name is tombstoned for ever and the serving path resolves the layer
    /// through the registry before it reaches this cache, so a form left behind could never be
    /// handed to anybody. What it could do is stay: at the campaign's target a level's form is
    /// gigabytes, and nothing here ever removed an entry — the map was bounded by the number of
    /// `(view, layer, level)` triples a process had *ever* seen rather than the number it holds.
    pub fn forget(&self, layer: &str) {
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, _), _| held != layer);
        // **Both maps, or the second one is the retention bug the first one fixed.** A partition is
        // megabytes at the campaign's target and is pinned by nothing else once the layer is gone.
        self.partitions_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(held, _), _| held != layer);
        // **All three maps, for the same reason.** An unclaimed index is eighty megabytes per level
        // at the campaign's target, pinned by nothing else once the layer is gone.
        self.indexes_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, _), _| held != layer);
        // **All four, and this one is the largest of them at 10⁹ rows**: a label column is four
        // bytes a row whatever the artifact count, which is the whole reason the layout exists.
        self.columns_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, _), _| held != layer);
        // **All five**, and this one is held rather than claimed, so nothing else would ever remove
        // it: a predicate layer's base column is four bytes a row and is pinned by this map alone.
        self.predicate_bases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, _), _| held != layer);
    }

    /// Drop everything held for one `(layer, level)`, in every view — **what a layout flip needs**.
    ///
    /// The cached row form is replace-on-mismatch, so a fold's new prefix would replace it at the
    /// level's next request anyway. The *held adoption maps* are the problem the memo names
    /// (selection memo §5): a level that flipped to row-major is never asked for its tile index
    /// again, so nothing ever claims that entry and the last generation's copy is pinned for the
    /// process's life — eighty megabytes per level at the campaign's target, and four bytes a row
    /// for the column in the other direction.
    ///
    /// **Removal only**, so it cannot widen anything: its worst outcome is one level rebuilding
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
