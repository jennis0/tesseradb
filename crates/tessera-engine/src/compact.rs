//! The fold (compaction). It writes a new prefix whose base holds everything the live prefix
//! holds except deleted entities. [`plan_fold`] runs on the executor against the live generation
//! and is pure. [`execute`] runs on one dedicated thread, off the request pool, and runs its
//! passes in sequence. Publication is `Executor::publish_fold`. Merge and coalesce are suspended
//! while a fold is in flight.
//!
//! # Which deletions a fold retires
//!
//! A deletion's overlay entry is removed only by the fold that removed the entity's rows and
//! postings. The plan's tombstone set ([`FoldPlan::tombstones`]) is what the passes run over, but
//! it is not the set that retires. A flush that planned before the delete was accepted can
//! publish the entity's row and postings while the fold runs. The fold carries that segment
//! forward, and removing the overlay entry would then expose a deleted item.
//!
//! So retirement is computed at publication: [`executed`] is the tombstone set minus every entity
//! a carried-forward artefact names ([`CarriedForward`]). `CarriedForward` must name at least
//! those entities. Naming too many keeps a tombstone for another fold, which is safe. Naming too
//! few exposes a deleted item. It takes each carried segment's and locator extent's whole entity
//! range: a flush publishes its segment, tier, run and locator extent over one contiguous range,
//! so the range covers all four, including an item with no terms, which no tier names.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use croaring::Bitmap;

use tessera_authz::{sweep_term_postings, DeltaTier, PostingsReader, PostingsSpool};
use tessera_spatial::tiler::ScalarType;
use tessera_store::manifest::{
    AttrExtent, DeclaredScalar, FileDigest, ManifestVocabulary, RecordExtent, SegmentDescriptor,
};
use tessera_store::render_presence::RENDER_PRESENCE_DIR;
use tessera_store::{
    fold_external_id_runs, fold_row_space, FoldRowSpaceSpec, FoldSegmentInput, PairsParquetWriter,
};
use tessera_types::IdentityKey;

use crate::flush::MaintenanceFailed;
use crate::Generation;

/// Every entity a carried-forward artefact names; [`executed`] subtracts it from the tombstone
/// set. Built at publication, from the live partition manifest minus what the fold consumed.
/// Built at plan time it would miss flushes that publish while the fold runs.
#[derive(Debug, Default)]
pub(crate) struct CarriedForward {
    entities: Bitmap,
}

impl CarriedForward {
    pub(crate) fn new() -> Self {
        CarriedForward {
            entities: Bitmap::new(),
        }
    }

    /// A segment the fold did not consume. Takes its whole declared entity range, not only the
    /// entities it holds: narrowing the range by reading the segment would risk naming fewer
    /// entities than it actually carries, which exposes a deleted item.
    pub(crate) fn add_segment(&mut self, descriptor: &SegmentDescriptor) {
        self.add_range(descriptor.entity_lo, descriptor.entity_hi);
    }

    /// A locator extent the fold did not consume. Covers the external-id binding of an item with
    /// no terms, which no tier names.
    pub(crate) fn add_locator_extent(&mut self, extent: &tessera_store::manifest::LocatorExtent) {
        self.add_range(extent.entity_lo, extent.entity_hi);
    }

    /// `entity_lo ..= entity_hi`, inclusive. Entity ids are capped at `u32::MAX`. A range that
    /// does not fit is clamped outward to that maximum rather than dropped, so it still names
    /// every entity it covers.
    fn add_range(&mut self, entity_lo: u64, entity_hi: u64) {
        if entity_lo > entity_hi {
            return;
        }
        let lo = u32::try_from(entity_lo).unwrap_or(u32::MAX);
        let hi = u32::try_from(entity_hi).unwrap_or(u32::MAX);
        self.entities.add_range(lo..=hi);
    }

    /// How many entities are carried forward, for the publication's log line.
    pub(crate) fn len(&self) -> u64 {
        self.entities.cardinality()
    }
}

/// The deletions whose overlay entries retire in this fold: the tombstone set minus everything a
/// carried-forward artefact names. An entity left in the result has lost both its row and its
/// postings to this fold's passes. Passes 1–3 run over the tombstone set itself, never over this.
pub(crate) fn executed(d0: &Bitmap, carried: &CarriedForward) -> Bitmap {
    let mut executed = d0.clone();
    executed.andnot_inplace(&carried.entities);
    executed
}

// =================================================================================================
// The schedule
// =================================================================================================

/// When a fold is dispatched without anyone asking for one. Segment count has two thresholds:
/// [`window_min_segments`], which fires only inside the daily window, and [`max_segments`], which
/// fires at any hour once the cost is too high to defer. Retirable depth has one threshold and no
/// window, since the overlay it measures grows monotonically under deletion churn.
///
/// [`window_min_segments`]: Self::window_min_segments
/// [`max_segments`]: Self::max_segments
// `PartialEq` without `Eq`: two of the thresholds are ratios, and `f64` has no total equality.
// The derive exists so a test can assert what a config file parsed to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionSchedule {
    /// Seconds. A fold within this of the last completed one is never dispatched.
    pub min_interval_secs: u64,
    /// Seconds past UTC midnight at which the daily window opens; `None` switches it off. UTC
    /// avoids the window shifting on a daylight-saving transition day.
    pub window_start_secs: Option<u32>,
    /// Seconds the window stays open.
    pub window_secs: u32,
    /// Live segments in any one view at or above which a fold is worth running inside the window.
    pub window_min_segments: usize,
    /// Live segments in any one view at or above which a fold is dispatched at any hour; `None`
    /// switches this route off. Must sit strictly above [`Self::window_min_segments`] where both
    /// are armed; `tessera-server` refuses that configuration.
    pub max_segments: Option<usize>,
    /// Retirable deletions at or above which a fold is dispatched at any hour; `None` switches the
    /// unwindowed route off.
    pub after_deletions: Option<u64>,
    /// Tombstoned rows as a fraction of the bundle's live rows, at or above which a fold is
    /// dispatched at any hour; `None` switches the route off. Distinct from
    /// [`Self::after_deletions`]'s absolute count over the same numerator.
    pub tombstoned_rows_fraction: Option<f64>,
    /// Dead bytes over named bytes, `(on_disc − named) / named`, at or above which a fold is
    /// dispatched at any hour; `None` switches the route off. Reclaims space from merge churn
    /// that leaves segment count and overlay depth low. Needs a directory walk, so [`due`] calls
    /// it last.
    pub dead_bytes_ratio: Option<f64>,
}

impl CompactionSchedule {
    /// Neither route armed. `tessera-server` applies its own defaults.
    pub fn off() -> Self {
        CompactionSchedule {
            min_interval_secs: 0,
            window_start_secs: None,
            window_secs: 0,
            window_min_segments: 0,
            max_segments: None,
            after_deletions: None,
            tombstoned_rows_fraction: None,
            dead_bytes_ratio: None,
        }
    }
}

/// Why the schedule dispatched a fold, carried into the log line so an operator can tell a
/// nightly tidy from a deployment drowning in un-retired deletions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FoldTrigger {
    /// Inside the daily window, with a view over `window_min_segments`.
    Window,
    /// A view reached `max_segments`, at whatever hour: segment growth past the point where
    /// deferring it is more expensive than paying it.
    SegmentCount,
    /// `|deleted|` reached `after_deletions`, at whatever hour.
    RetirableDepth,
    /// Tombstoned rows passed `tombstoned_rows_fraction` of the bundle's live rows.
    TombstonedRows,
    /// On-disc bytes passed `dead_bytes_ratio` × the bytes the manifests name.
    DeadBytes,
}

/// What the schedule reads at the tick that reads it. The dead-bytes gauge is not here: it needs
/// a directory walk, so it arrives as a closure instead.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Gauges {
    /// The largest live segment count across the partition's views.
    pub(crate) live_segments: usize,
    /// Deletions alone, never the union with suppressions.
    pub(crate) retirable_deletions: u64,
    /// Rows the bundle's segments hold, tombstoned ones included.
    pub(crate) live_rows: u64,
}

/// The dead-bytes gauge's two operands: what is on disc under the live prefix, and what its
/// manifests still name.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DeadBytes {
    pub(crate) on_disc: u64,
    pub(crate) named: u64,
}

/// Whether the schedule calls for a fold now. Pure, so it is testable without a clock or an
/// executor. `now_unix` and `last_fold_unix` are seconds.
///
/// `last_fold_unix` is when the last attempt ended, not when the last fold succeeded: a discard
/// leaves the gauge that dispatched the fold unchanged, so stamping only on success would retry
/// and discard at every tick under one bad configuration value. This floor is process-local, so a
/// restart loses it; that is harmless after a success, since the fold's own output changes the
/// gauges too.
pub(crate) fn due(
    schedule: &CompactionSchedule,
    now_unix: u64,
    last_fold_unix: Option<u64>,
    gauges: Gauges,
    dead_bytes: impl FnOnce() -> Option<DeadBytes>,
) -> Option<FoldTrigger> {
    // `saturating_sub`: a clock that steps backwards must read as "not yet", not as a large elapsed time.
    if let Some(last) = last_fold_unix {
        if now_unix.saturating_sub(last) < schedule.min_interval_secs {
            return None;
        }
    }

    // The unwindowed routes are checked first, so their reason is logged even inside the window.
    if let Some(threshold) = schedule.after_deletions {
        if gauges.retirable_deletions >= threshold {
            return Some(FoldTrigger::RetirableDepth);
        }
    }
    if let Some(threshold) = schedule.max_segments {
        if gauges.live_segments >= threshold {
            return Some(FoldTrigger::SegmentCount);
        }
    }
    // A bundle with no rows has no fraction; reading one as infinite would fold every tick.
    if let Some(threshold) = schedule.tombstoned_rows_fraction {
        if gauges.live_rows > 0
            && gauges.retirable_deletions as f64 / gauges.live_rows as f64 >= threshold
        {
            return Some(FoldTrigger::TombstonedRows);
        }
    }

    if let Some(start) = schedule.window_start_secs {
        if gauges.live_segments >= schedule.window_min_segments
            && schedule.window_min_segments > 0
            && in_window(now_unix, start, schedule.window_secs)
        {
            return Some(FoldTrigger::Window);
        }
    }

    // Last: the only gauge that needs a directory walk.
    let threshold = schedule.dead_bytes_ratio?;
    let measured = dead_bytes()?;
    let dead = measured.on_disc.saturating_sub(measured.named);
    (measured.named > 0 && dead as f64 / measured.named as f64 >= threshold)
        .then_some(FoldTrigger::DeadBytes)
}

/// Whether `now_unix` falls in the daily window `[start, start + width)` past UTC midnight.
/// Wraps past midnight, so a window starting at 23:00 works.
fn in_window(now_unix: u64, start_secs: u32, window_secs: u32) -> bool {
    const DAY: u64 = 86_400;
    // A width at or past a whole day is always open.
    if u64::from(window_secs) >= DAY {
        return true;
    }
    if window_secs == 0 {
        return false;
    }
    let time_of_day = now_unix % DAY;
    let since = (time_of_day + DAY - u64::from(start_secs)) % DAY;
    since < u64::from(window_secs)
}

// =================================================================================================
// The plan
// =================================================================================================

/// One live segment of one view. `dir` is prefix-relative, as the manifest's `files` map uses it.
pub(crate) struct PlannedSegment {
    pub(crate) seg_id: String,
    pub(crate) dir: String,
}

/// One view's half of a fold plan.
pub(crate) struct FoldViewPlan {
    pub(crate) view: String,
    /// The incarnation of `view` this plan folds, stamped into the new base segment.
    pub(crate) incarnation: tessera_types::view::ViewIncarnation,
    /// Every live segment of this view at the snapshot: the base plus every extent. Pass 1 merges
    /// them all in `(morton, tessera_id)` order; input order does not matter.
    pub(crate) segments: Vec<PlannedSegment>,
    /// The bound for this view's new `permutation.bin`: one past the highest entity the
    /// snapshot's row space covers, not the live entity allocator high-water.
    pub(crate) permutation_bound: u64,
    /// The rows this view holds at the snapshot, base and extents together. An upper bound on the
    /// new base's row count, and what [`memory_estimate`] charges pass 2b's image against.
    pub(crate) rows: u64,
}

/// One fold's immutable plan: the files it consumes, and the tombstone set. Pure and holds
/// nothing open, so it survives churn on the executor while the fold runs, but not a publication
/// that consumed one of its files; the rebase check at publication catches that.
pub(crate) struct FoldPlan {
    pub(crate) partition: String,
    pub(crate) views: Vec<FoldViewPlan>,
    /// Every live delta tier, prefix-relative, in the live manifest's order; all consumed.
    pub(crate) tiers: Vec<String>,
    /// Every live external-id run, prefix-relative, oldest first; all consumed by pass 3.
    pub(crate) runs: Vec<String>,
    /// Every live locator extent's path, consumed with the runs they index.
    pub(crate) locator_extents: Vec<String>,
    /// Every attribute extent the partition's side-manifest named at the snapshot. All consumed;
    /// the folded state is a function of the schema, not of deletion history.
    pub(crate) attr_extents: Vec<AttrExtent>,
    /// Every record-blob extent the side-manifest named at the snapshot, on the same
    /// all-or-nothing basis as [`FoldPlan::attr_extents`].
    pub(crate) record_extents: Vec<RecordExtent>,
    /// The entity→term transpose's extents at the snapshot, folded into the new base by pass 4c.
    pub(crate) entity_terms_extents: Vec<tessera_store::manifest::EntityTermsExtent>,
    /// Every text extent the side-manifest named at the snapshot, on the same all-or-nothing basis.
    pub(crate) text_extents: Vec<tessera_store::manifest::TextExtent>,
    /// The plan's tombstone set. Handed to passes 1–3 whole; never [`executed`].
    pub(crate) tombstones: Bitmap,
    /// One past the highest entity with a row anywhere in this partition at the snapshot. Not the
    /// live high-water, which would make the base locator answer "no external id" for a
    /// post-snapshot entity that has one.
    pub(crate) entity_bound: u64,
    /// The dictionary length the term sweep emits records for.
    pub(crate) dict_len: u32,
    pub(crate) small_term_threshold: u32,
    /// The prefix this plan was taken against. A publication into a different one is discarded.
    pub(crate) prefix: String,
}

/// Why a tick planned no fold. Each is a distinct operator-facing condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoFold {
    /// The WAL is poisoned: a fold would write overlay dispositions no durable record backs.
    WalPoisoned,
    /// The in-memory overlay has diverged from the durable WAL.
    OverlayDiverged,
    /// A partition is serving a stepped-down side-manifest: folding it would remove the
    /// stepped-past segment rather than shadow it.
    SteppedDown,
    /// No partition, or a partition with no view holding a segment. Nothing to fold.
    NothingToFold,
    /// The estimated peak memory is above what the host has available. Both figures in bytes.
    InsufficientMemory { need: u64, available: u64 },
    /// The estimated output is above the free space on the device. Both figures in bytes.
    InsufficientDisc { need: u64, free: u64 },
}

impl NoFold {
    /// Every gate, in the order [`NoFold::index`] numbers them, as fixed literals: these names
    /// reach `/control/status` and must never carry corpus-derived text.
    pub(crate) const GATES: [&'static str; 6] = [
        "wal_poisoned",
        "overlay_diverged",
        "stepped_down",
        "nothing_to_fold",
        "insufficient_memory",
        "insufficient_disc",
    ];

    /// This gate's position in [`NoFold::GATES`] and its per-gate counter. Exhaustive, so a new
    /// variant is a compile error here.
    pub(crate) fn index(self) -> usize {
        match self {
            NoFold::WalPoisoned => 0,
            NoFold::OverlayDiverged => 1,
            NoFold::SteppedDown => 2,
            NoFold::NothingToFold => 3,
            NoFold::InsufficientMemory { .. } => 4,
            NoFold::InsufficientDisc { .. } => 5,
        }
    }

    /// The gauge form: a stable name for the condition, and the two figures it carries. `None`
    /// for the four conditions with no figures; for the other two, `need` is the estimate and
    /// `had` is what the host answered.
    pub(crate) fn gauge(self) -> (&'static str, Option<u64>, Option<u64>) {
        let (need, had) = match self {
            NoFold::WalPoisoned
            | NoFold::OverlayDiverged
            | NoFold::SteppedDown
            | NoFold::NothingToFold => (None, None),
            NoFold::InsufficientMemory { need, available } => (Some(need), Some(available)),
            NoFold::InsufficientDisc { need, free } => (Some(need), Some(free)),
        };
        (NoFold::GATES[self.index()], need, had)
    }
}

/// What the fold's two pre-flight refusals compare against. Measured by the caller, so
/// [`plan_fold`] stays pure: free space and available memory are syscalls against the host, not
/// properties of the generation. `None` means unknowable, and the corresponding pre-flight simply
/// does not run rather than refusing a fold on a guess.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FoldResources {
    pub(crate) available_memory: Option<u64>,
    pub(crate) free_disc: Option<u64>,
    /// Roaring containers across every artifact membership this node holds. Charged per container
    /// ([`ARTIFACT_BYTES_PER_CONTAINER`]); a planner cannot derive it from the manifest alone.
    pub(crate) membership_containers: u64,
}

/// The multiplier on [`memory_estimate`]'s computable terms, covering the one term that needs a
/// postings scan to compute exactly. Assumed, not measured directly.
const FOLD_MEMORY_SAFETY_FACTOR: u64 = 2;

/// Workers the fold's term-image derivation runs across. One, because [`execute`] runs on one
/// dedicated thread and occupying request-serving workers would put maintenance work on the
/// request path. The build passes `rayon::current_num_threads()` instead.
const TERM_IMAGE_THREADS: usize = 1;

/// The publication number the fold's term-image files are named after. Zero, because a fold's
/// side-manifest number is allocated at publication, hours after this pass writes the file, and a
/// number taken earlier would collide with a flush published during the flight.
const TERM_IMAGE_MANIFEST_N: u64 = 0;

/// The fold's peak un-reclaimable memory in bytes: the anonymous memory plus the dirty pages of
/// the arrays the fold writes through a mapping. Most other resident memory is page cache the
/// kernel can drop under pressure.
///
/// | term | basis |
/// |---|---|
/// | 4 B × permutation bound | `permutation.bin`, written through a mapping |
/// | 4 B × entity bound | `ext-locator.u32`, same |
/// | 8 B × dictionary length | `PostingsSpool`'s offsets buffer |
/// | 90 B × membership containers | the artifact pass's row forms, held while it rebuilds them |
/// | threads × (posting + image + frozen + scratch) | pass 2b's window and its projection |
///
/// The permutation and image terms take the maximum across views, since pass 1 and pass 2b each
/// fold one view at a time.
pub(crate) fn memory_estimate(
    permutation_bound: u64,
    entity_bound: u64,
    dict_len: u64,
    membership_containers: u64,
    base_rows: u64,
) -> u64 {
    let terms = 4u64
        .saturating_mul(permutation_bound)
        .saturating_add(4u64.saturating_mul(entity_bound))
        .saturating_add(8u64.saturating_mul(dict_len))
        .saturating_add(ARTIFACT_BYTES_PER_CONTAINER.saturating_mul(membership_containers))
        .saturating_add(term_image_estimate(dict_len, permutation_bound, base_rows));
    terms.saturating_mul(FOLD_MEMORY_SAFETY_FACTOR)
}

/// The widest a Roaring container can be once built, in bytes: a bitset over its 65 536 values.
const BYTES_PER_BITSET_CONTAINER: u64 = 8 * 1024;

/// Values one Roaring container covers, rows or entities.
const VALUES_PER_CONTAINER: u64 = 1 << 16;

/// [`memory_estimate`]'s pass 2b term: per worker, the widest posting it can hold, the widest
/// image built from one, the buffer that image is serialised into, and the scratch it projects
/// through. Zero where the pass does not run: a view with no row, or a dictionary with no term.
fn term_image_estimate(dict_len: u64, permutation_bound: u64, base_rows: u64) -> u64 {
    if dict_len == 0 || base_rows == 0 {
        return 0;
    }
    let widest = |values: u64| {
        values
            .div_ceil(VALUES_PER_CONTAINER)
            .saturating_mul(BYTES_PER_BITSET_CONTAINER)
    };
    let image = widest(base_rows);
    let scratch =
        tessera_store::permutation::project_scratch_bound(permutation_bound, base_rows).total();
    let held = widest(permutation_bound)
        .saturating_add(image)
        .saturating_add(image)
        .saturating_add(scratch);
    (TERM_IMAGE_THREADS as u64).saturating_mul(held)
}

/// What one Roaring container costs resident, in bytes: the artifact pass's whole price model.
/// Measured at 78.5–94.0 B per container across a range of artifact counts and membership shapes;
/// the cost is per container rather than per artifact or per member, so 90 covers both.
const ARTIFACT_BYTES_PER_CONTAINER: u64 = 90;

/// The free space a fold needs, as a percentage of the bytes its inputs' manifests name. The
/// fold's own output is at most the live bytes, and carried-forward files are hard links that
/// cost no bytes. The 50% margin covers flushes still publishing into the old prefix, its WAL,
/// and the fragment cache. Assumed rather than measured against a deployment's ingest rate.
const FOLD_DISC_PERCENT: u64 = 150;

/// The free bytes a fold needs before it starts, from the bytes its inputs' manifests name.
pub(crate) fn disc_estimate(live_bytes: u64) -> u64 {
    live_bytes
        .saturating_mul(FOLD_DISC_PERCENT)
        .saturating_div(100)
}

/// Plan a fold of `generation`'s single partition. Pure: it reads the generation, the two
/// executor-health flags and the host figures its caller measured, and nothing else.
pub(crate) fn plan_fold(
    generation: &Generation,
    wal_poisoned: bool,
    overlay_diverged: bool,
    resources: FoldResources,
) -> Result<FoldPlan, NoFold> {
    if wal_poisoned {
        return Err(NoFold::WalPoisoned);
    }
    if overlay_diverged {
        return Err(NoFold::OverlayDiverged);
    }
    if generation
        .bundle
        .partitions
        .values()
        .any(|p| p.stepped_down())
    {
        return Err(NoFold::SteppedDown);
    }
    let (partition, partition_data) = generation
        .bundle
        .partitions
        .iter()
        .next()
        .ok_or(NoFold::NothingToFold)?;
    let manifest = &partition_data.manifest;

    let mut views: Vec<FoldViewPlan> = Vec::new();
    // Sorted, so the plan does not depend on a `HashMap`'s iteration order.
    let mut view_ids: Vec<&String> = partition_data.views.keys().collect();
    view_ids.sort_unstable();
    for view in view_ids {
        let view_data = &partition_data.views[view];
        if view_data.segments.is_empty() {
            continue;
        }
        let row_space = &view_data.row_space;
        let permutation_bound = row_space
            .extents()
            .last()
            .map_or(row_space.base().bound(), |extent| extent.entity_hi + 1);
        views.push(FoldViewPlan {
            view: view.clone(),
            incarnation: view_data.incarnation,
            segments: view_data
                .segments
                .iter()
                .map(|segment| PlannedSegment {
                    dir: format!(
                        "partitions/{partition}/{}/segments/{}",
                        tessera_store::view_rel(view),
                        segment.seg_id
                    ),
                    seg_id: segment.seg_id.clone(),
                })
                .collect(),
            permutation_bound,
            rows: row_space.total_rows(),
        });
    }
    if views.is_empty() {
        return Err(NoFold::NothingToFold);
    }

    // The partition-wide locator span. Publication checks that every post-snapshot locator extent
    // begins above this, rather than assuming it.
    let entity_bound = views
        .iter()
        .map(|view| view.permutation_bound)
        .max()
        .unwrap_or(0);

    // The two pre-flight refusals run last, once the plan's own quantities are known.
    let dict_len = generation.dict.len();
    if let Some(available) = resources.available_memory {
        let need = memory_estimate(
            views
                .iter()
                .map(|view| view.permutation_bound)
                .max()
                .unwrap_or(0),
            entity_bound,
            u64::from(dict_len),
            resources.membership_containers,
            // The widest view's rows: pass 2b derives one view at a time.
            views.iter().map(|view| view.rows).max().unwrap_or(0),
        );
        if need > available {
            return Err(NoFold::InsufficientMemory { need, available });
        }
    }
    if let Some(free) = resources.free_disc {
        // Both manifests are summed: the build's artefacts and the write path's are separate.
        let live_bytes: u64 = generation
            .bundle
            .manifest
            .files
            .values()
            .map(|d| d.size)
            .chain(manifest.files.values().map(|d| d.size))
            .sum();
        let need = disc_estimate(live_bytes);
        if need > free {
            return Err(NoFold::InsufficientDisc { need, free });
        }
    }

    let mut tombstones = Bitmap::new();
    for entity in generation.overlay.deleted_entities() {
        tombstones.add(entity as u32);
    }

    Ok(FoldPlan {
        partition: partition.clone(),
        views,
        tiers: manifest.deltas.clone(),
        runs: manifest.external_id_runs.clone(),
        locator_extents: manifest
            .locator_extents
            .iter()
            .map(|extent| extent.path.clone())
            .collect(),
        attr_extents: manifest.attr_extents.clone(),
        record_extents: manifest.record_extents.clone(),
        entity_terms_extents: manifest.entity_terms_extents.clone(),
        text_extents: manifest.text_extents.clone(),
        tombstones,
        entity_bound,
        dict_len,
        small_term_threshold: generation.bundle.manifest.small_term_threshold,
        prefix: generation.prefix.clone(),
    })
}

// =================================================================================================
// Execution: five streaming passes into a new prefix
// =================================================================================================

/// Everything [`execute`] needs beyond its plan. Taken from the generation on the executor thread
/// and then immutable. The two `Arc`s are the live readers rather than reopened files: they are
/// the same mappings every request is already serving from.
pub(crate) struct FoldContext {
    /// The prefix the fold reads: the live one when the plan was taken.
    pub(crate) from_prefix_dir: PathBuf,
    /// The prefix the fold writes, which no `CURRENT` names until publication.
    pub(crate) to_prefix: String,
    pub(crate) to_prefix_dir: PathBuf,
    pub(crate) identity_key: IdentityKey,
    pub(crate) shard_id: u32,
    /// One writer schema per view, keyed by view id: the bundle-wide render tail plus that view's
    /// group-scoped render lanes.
    pub(crate) scalar_schema: BTreeMap<String, Vec<(String, ScalarType)>>,
    /// Per view, the columns its schema may lawfully lack; missing otherwise is a torn segment.
    pub(crate) absent_ok: BTreeMap<String, Vec<String>>,
    /// Entity-scoped columns declared at a running service and not yet folded, by name.
    pub(crate) runtime_attributes: Vec<String>,
    /// The group-scoped families declared at a running service and not yet folded, by name.
    pub(crate) runtime_scoped_attributes: Vec<String>,
    /// The new base segment's id, one per view. Never reused.
    pub(crate) seg_id: String,
    /// The live base postings and the live tiers: pass 2's inputs.
    pub(crate) base_postings: Arc<PostingsReader>,
    pub(crate) tiers: Vec<Arc<DeltaTier>>,
    /// The bundle's declared scalars and vocabularies, taken from the manifest.
    pub(crate) declared_scalars: Vec<DeclaredScalar>,
    /// Every group's group-scoped column families, flattened, owed one folded column per view.
    pub(crate) scoped_scalars: Vec<tessera_store::manifest::ScopedScalar>,
    /// Which incarnation each view of the roster is. A scoped column's directory carries the
    /// incarnation, so the fold has to place it where the opener will look.
    pub(crate) view_incarnations:
        std::collections::HashMap<String, tessera_types::view::ViewIncarnation>,
    pub(crate) vocabularies: Vec<ManifestVocabulary>,
}

/// The path one view's column of a scoped family folds into, or `None` where this manifest
/// cannot say which incarnation the view is.
fn scoped_job_rel(plan: &FoldPlan, ctx: &FoldContext, family: &str, view: &str) -> Option<String> {
    let incarnation = ctx.view_incarnations.get(view)?;
    Some(tessera_store::scoped_column_rel(
        &plan.partition,
        family,
        view,
        *incarnation,
    ))
}

/// One column the attribute pass folds: where its files live, and what its declaration says. One
/// shape covers both an entity-scoped column and one view of a group-scoped family.
struct ColumnJob {
    /// Prefix-relative directory, the same under both prefixes.
    rel: String,
    name: String,
    /// The view this column belongs to, for a scoped family; `None` for an entity-scoped column.
    view: Option<String>,
    arrow_type: ScalarType,
    /// Does the folded column owe rebuilt keyed postings? True only for a category.
    postings: bool,
}

/// The incarnation an extent must carry to belong to this job: the view's live one for a scoped
/// column, `None` for an entity-scoped one.
fn job_incarnation(
    ctx: &FoldContext,
    job: &ColumnJob,
) -> Option<tessera_types::view::ViewIncarnation> {
    job.view
        .as_ref()
        .and_then(|view| ctx.view_incarnations.get(view).copied())
}

/// Every column the attribute pass folds, entity-scoped then group-scoped, in manifest order.
/// Uses the same predicates `FilterColumns::open` does, so the columns this pass writes and the
/// columns the opener demands are one set.
fn value_column_jobs(plan: &FoldPlan, ctx: &FoldContext) -> Vec<ColumnJob> {
    let mut jobs: Vec<ColumnJob> = ctx
        .declared_scalars
        .iter()
        .filter(|d| crate::filter::owes_value_column(d, &ctx.vocabularies))
        .map(|d| ColumnJob {
            rel: format!("partitions/{}/attrs/{}", plan.partition, d.name),
            name: d.name.clone(),
            view: None,
            arrow_type: d.arrow_type,
            postings: d.vocabulary.is_some(),
        })
        .collect();
    for family in &ctx.scoped_scalars {
        // Text owes no value column: `fold_text_columns` merges its dictionary and postings instead.
        if !crate::filter::scoped_has_value_column(family) {
            continue;
        }
        for view in &family.views {
            let Some(rel) = scoped_job_rel(plan, ctx, &family.name, view) else {
                continue;
            };
            jobs.push(ColumnJob {
                rel,
                name: family.name.clone(),
                view: Some(view.clone()),
                arrow_type: family.arrow_type,
                postings: crate::filter::scoped_owes_postings(family),
            });
        }
    }
    jobs
}

/// The process's resident set in bytes: total, anonymous, file-backed. Files written through a
/// mapping land in the file-backed figure, reclaimable once written back; spool buffers and term
/// encodes are anonymous and are not, so the split shows which part is growing.
fn resident_set() -> (u64, u64, u64) {
    let r = tessera_types::process::resident_bytes();
    (r.total, r.anon, r.file)
}

/// What one pass cost: its wall clock, and the process's resident set at the moment it ended.
/// Sampled at pass boundaries, giving attribution (which pass the resident set climbed during)
/// rather than a true peak.
#[derive(Debug, Clone, Copy)]
pub struct PassCost {
    pub pass: &'static str,
    pub elapsed: std::time::Duration,
    /// Total and anonymous resident bytes at the end of the pass.
    pub rss: u64,
    pub anon: u64,
}

/// A staircase under construction: the rows recorded so far and the instant the next row is
/// measured from. The fold thread starts one for its passes; `publish_fold` resumes it for the
/// publication's phases, so `/control/status` reports the fold end to end.
pub(crate) struct Staircase {
    cost: Vec<PassCost>,
    mark: std::time::Instant,
}

impl Staircase {
    pub(crate) fn start() -> Self {
        Self {
            cost: Vec::with_capacity(14),
            mark: std::time::Instant::now(),
        }
    }

    /// Continue a staircase another thread recorded: `cost` is its rows and `mark` is when its
    /// last row ended, so the first row recorded here covers the hand-off.
    pub(crate) fn resume(cost: Vec<PassCost>, mark: std::time::Instant) -> Self {
        Self { cost, mark }
    }

    /// Close one row: the wall clock since the previous row ended, and the resident set now.
    pub(crate) fn record(&mut self, pass: &'static str) {
        let (rss, anon, _) = resident_set();
        self.cost.push(PassCost {
            pass,
            elapsed: self.mark.elapsed(),
            rss,
            anon,
        });
        self.mark = std::time::Instant::now();
    }

    /// When the last recorded row ended.
    pub(crate) fn mark(&self) -> std::time::Instant {
        self.mark
    }

    pub(crate) fn into_cost(self) -> Vec<PassCost> {
        self.cost
    }
}

/// One view's term images as pass 2b wrote them: what the new side-manifest must name, and what
/// the publication logs about them. The summary is carried rather than recomputed from the file,
/// since the wall clock and the counts are the pass's own and nothing in the file records them.
pub(crate) struct FoldedTermImages {
    pub(crate) extent: tessera_store::manifest::TermImageExtent,
    pub(crate) summary: tessera_store::term_images::TermImageSummary,
}

/// A fold whose files are durable under a prefix nothing yet names.
pub(crate) struct CompletedFold {
    pub(crate) plan: FoldPlan,
    pub(crate) prefix: String,
    /// The new base segment of each view, in the plan's view order.
    pub(crate) segments: Vec<SegmentDescriptor>,
    /// Every file the fold itself wrote, prefix-relative, with its digest. Publication adds the
    /// carried-forward files' digests to write `MANIFEST.json`.
    pub(crate) files: BTreeMap<String, FileDigest>,
    /// The new run 0's prefix-relative path, or `None` when the deployment holds no external ids.
    pub(crate) external_id_run: Option<String>,
    /// One entry per view pass 2b wrote images for. Goes into the new `SEGMENTS-<n>.json`
    /// unchanged.
    pub(crate) term_images: Vec<FoldedTermImages>,
    /// The largest new base segment's `columns.arrow + morton.u32 + cuts.u32` bytes.
    pub(crate) base_segment_bytes: u64,
    /// One [`PassCost`] per pass, in execution order. Publication resumes the staircase with its
    /// own phases.
    pub(crate) cost: Vec<PassCost>,
    /// When the fold thread's last row ended, from which the publication's first row is measured.
    pub(crate) finished: std::time::Instant,
    /// Attribute bytes pass 4a read and wrote. Reported, never triggered on.
    pub(crate) attr_bytes_read: u64,
    pub(crate) attr_bytes_written: u64,
    /// [`FoldContext::runtime_attributes`] and [`FoldContext::runtime_scoped_attributes`]: the
    /// columns this fold gave a base, which publication moves off the runtime list.
    pub(crate) runtime_attributes: Vec<String>,
    pub(crate) runtime_scoped_attributes: Vec<String>,
}

/// The state the fold's passes share: every file written so far, and the attribute passes' IO.
#[derive(Default)]
struct FoldOutput {
    /// Every file this fold writes, prefix-relative and resolved, in write order. Pass 5 digests
    /// exactly this list; an unrecorded file is missing from the new `MANIFEST.json`.
    written: Vec<(String, PathBuf)>,
    /// Attribute bytes read and written. Reported, never triggered on.
    attr_read: u64,
    attr_written: u64,
}

impl FoldOutput {
    /// Record a file the fold wrote.
    fn push(&mut self, rel: String, path: PathBuf) {
        self.written.push((rel, path));
    }

    /// Record a file an attribute pass wrote, and charge its bytes to `attr_written`.
    fn wrote(&mut self, rel: String, path: PathBuf) {
        self.attr_written += file_len(&path);
        self.written.push((rel, path));
    }
}

/// What a pass was doing when it failed, and what went wrong.
fn failed(what: &str, e: &dyn std::fmt::Display) -> MaintenanceFailed {
    MaintenanceFailed(format!("{what}: {e}"))
}

/// A written file's length, and zero where it cannot be read. Counts bytes for a report and never
/// decides anything.
fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Run the fold's five passes into `ctx.to_prefix_dir`, on one dedicated thread. A failure
/// discards the fold: its files are orphans under a prefix `CURRENT` does not name, and there is
/// no resume. Digests are taken by reading each written file back rather than hashed as it is
/// written, since none of the five writers this pass composes can hash at the source.
pub(crate) fn execute(
    plan: FoldPlan,
    ctx: FoldContext,
) -> Result<CompletedFold, MaintenanceFailed> {
    let partition_dir = ctx.to_prefix_dir.join("partitions").join(&plan.partition);
    let terms_dir = partition_dir.join("terms");
    let entities_dir = partition_dir.join("entities");
    for dir in [&terms_dir, &entities_dir] {
        std::fs::create_dir_all(dir).map_err(|e| failed("creating the new prefix", &e))?;
    }

    let mut out = FoldOutput::default();

    // `entry` is the reading every later pass cost is measured against.
    let mut stairs = Staircase::start();
    stairs.record("entry");

    let (segments, base_segment_bytes) = fold_row_spaces(&plan, &ctx, &mut out)?;
    stairs.record("1 row space");

    let postings_path = fold_postings(&plan, &ctx, &terms_dir, &mut out)?;
    stairs.record("2 postings");

    let term_images = derive_term_images(&plan, &ctx, &segments, &postings_path, &mut out)?;
    stairs.record("2b term images");

    let external_id_run = fold_external_ids(&plan, &ctx, &entities_dir, &mut out)?;
    stairs.record("3 external ids");

    fold_text_columns(&plan, &ctx, &mut out)?;
    fold_value_columns(&plan, &ctx, &mut out)?;
    fold_record_blob(&plan, &ctx, &mut out)?;
    stairs.record("4a attributes");

    fold_entity_terms(&plan, &ctx, &mut out)?;
    stairs.record("4c entity terms");

    // Pass 4b writes nothing and is not marked: the term dictionary is carried forward by a hard
    // link at publication.
    let files = digest_and_sync(&out)?;
    stairs.record("5 digests + fsync");

    let finished = stairs.mark();
    Ok(CompletedFold {
        plan,
        prefix: ctx.to_prefix,
        segments,
        files,
        external_id_run,
        term_images,
        base_segment_bytes,
        cost: stairs.into_cost(),
        finished,
        attr_bytes_read: out.attr_read,
        attr_bytes_written: out.attr_written,
        runtime_attributes: ctx.runtime_attributes,
        runtime_scoped_attributes: ctx.runtime_scoped_attributes,
    })
}

/// Pass 1: one new segment per (partition, view), and one `permutation.bin` beside it. Rows whose
/// entity is tombstoned are dropped, which shifts the row id of every row after them. Returns the
/// new base descriptors and the largest one's mapped bytes: `columns.arrow`, `morton.u32` and the
/// cut index together.
fn fold_row_spaces(
    plan: &FoldPlan,
    ctx: &FoldContext,
    output: &mut FoldOutput,
) -> Result<(Vec<SegmentDescriptor>, u64), MaintenanceFailed> {
    let mut segments: Vec<SegmentDescriptor> = Vec::with_capacity(plan.views.len());
    let mut base_segment_bytes = 0u64;
    for view in &plan.views {
        // Refused rather than defaulted: an empty schema would write a segment with no scalar tail.
        let Some(view_schema) = ctx.scalar_schema.get(&view.view) else {
            return Err(MaintenanceFailed(format!(
                "pass 1 (row space): this fold holds no writer schema for view '{}', though \\
                 its plan names it; the two disagree about what is being folded",
                view.view
            )));
        };
        let view_rel = format!(
            "partitions/{}/{}",
            plan.partition,
            tessera_store::view_rel(&view.view)
        );
        let view_dir = ctx.to_prefix_dir.join(&view_rel);
        std::fs::create_dir_all(&view_dir).map_err(|e| failed("creating the view", &e))?;
        let segment_rel = format!("{view_rel}/segments/{}", ctx.seg_id);
        let segment_dir = ctx.to_prefix_dir.join(&segment_rel);
        let permutation_rel = format!("{view_rel}/permutation.bin");
        let permutation_path = ctx.to_prefix_dir.join(&permutation_rel);
        let row_entity_rel = format!("{view_rel}/{}", tessera_store::ROW_ENTITY_FILE);
        let row_entity_path = ctx.to_prefix_dir.join(&row_entity_rel);

        let inputs: Vec<FoldSegmentInput> = view
            .segments
            .iter()
            .map(|planned| FoldSegmentInput {
                seg_id: planned.seg_id.clone(),
                dir: ctx.from_prefix_dir.join(&planned.dir),
            })
            .collect();
        let out = fold_row_space(
            &segment_dir,
            &permutation_path,
            &row_entity_path,
            FoldRowSpaceSpec {
                inputs: &inputs,
                identity_key: &ctx.identity_key,
                shard_id: ctx.shard_id,
                scalar_schema: view_schema,
                absent_ok: ctx.absent_ok.get(&view.view).map_or(&[][..], Vec::as_slice),
                tombstones: &plan.tombstones,
                permutation_bound: view.permutation_bound,
            },
        )
        .map_err(|e| failed("pass 1 (row space)", &e))?;

        let mut view_bytes = 0u64;
        for name in [
            "morton.u32",
            tessera_store::read::CutIndex::FILE,
            "columns.arrow",
        ] {
            let path = segment_dir.join(name);
            view_bytes += std::fs::metadata(&path)
                .map_err(|e| failed("sizing the new base segment", &e))?
                .len();
            output.push(format!("{segment_rel}/{name}"), path);
        }
        base_segment_bytes = base_segment_bytes.max(view_bytes);
        // The render columns' presence bitmaps, not counted into `view_bytes`.
        for column in &out.presence_columns {
            output.push(
                format!("{segment_rel}/{RENDER_PRESENCE_DIR}/{column}.roaring"),
                tessera_store::render_presence::render_presence_path(&segment_dir, column),
            );
        }
        output.push(permutation_rel, permutation_path);
        output.push(row_entity_rel, row_entity_path);

        segments.push(SegmentDescriptor {
            view: view.view.clone(),
            incarnation: view.incarnation,
            seg_id: ctx.seg_id.clone(),
            row_count: out.row_count,
            entity_lo: 0,
            // Inclusive, and the permutation's span rather than the highest surviving entity.
            entity_hi: view.permutation_bound.saturating_sub(1),
        });
    }

    Ok((segments, base_segment_bytes))
}

/// Pass 2: the new base postings, and `pairs.parquet` beside them. Returns the postings' path,
/// which pass 2b reads. Every ordinal below `dict_len` gets a record, empty or not, so ordinals
/// stay stable across a fold. `pairs.parquet` is rewritten rather than carried forward, because
/// a carried file would still list folded deletions.
fn fold_postings(
    plan: &FoldPlan,
    ctx: &FoldContext,
    terms_dir: &Path,
    out: &mut FoldOutput,
) -> Result<PathBuf, MaintenanceFailed> {
    let postings_rel = format!("partitions/{}/terms/postings.arrow", plan.partition);
    let pairs_rel = format!("partitions/{}/terms/pairs.parquet", plan.partition);
    let postings_path = ctx.to_prefix_dir.join(&postings_rel);
    let pairs_path = ctx.to_prefix_dir.join(&pairs_rel);
    let spool_path = terms_dir.join("postings.spool");
    {
        let mut spool =
            PostingsSpool::create(&spool_path).map_err(|e| failed("pass 2 (spool)", &e))?;
        let mut pairs =
            PairsParquetWriter::create(&pairs_path).map_err(|e| failed("pass 2 (pairs)", &e))?;
        let sweep = sweep_term_postings(
            plan.dict_len,
            &ctx.base_postings,
            &ctx.tiers,
            &plan.tombstones,
            plan.small_term_threshold,
            &mut spool,
            |term, entities| {
                pairs
                    .push_iter(term.raw(), entities.iter())
                    .map_err(|e| std::io::Error::other(e.to_string()))
            },
        );
        let outcome = sweep
            .and_then(|()| spool.finish(&postings_path))
            .map_err(|e| failed("pass 2 (postings)", &e));
        // The spool is deleted by `finish` on success; on failure it is this function's to remove.
        if outcome.is_err() {
            let _ = std::fs::remove_file(&spool_path);
        }
        outcome?;
        pairs.finish().map_err(|e| failed("pass 2 (pairs)", &e))?;
    }
    out.push(postings_rel, postings_path.clone());
    out.push(pairs_rel, pairs_path);

    Ok(postings_path)
}

/// Pass 2b: one term-image file per view, each term's new base posting projected into that view's
/// new row space. Reads the postings pass 2 has just written, from which every folded deletion is
/// already gone. Covers the new base only; a later flush's rows are an extent and get no images.
fn derive_term_images(
    plan: &FoldPlan,
    ctx: &FoldContext,
    segments: &[SegmentDescriptor],
    postings_path: &Path,
    out: &mut FoldOutput,
) -> Result<Vec<FoldedTermImages>, MaintenanceFailed> {
    let mut term_images: Vec<FoldedTermImages> = Vec::new();
    let postings = PostingsReader::open(postings_path, true)
        .map_err(|e| failed("pass 2b (term images: the new postings)", &e))?;
    let dict_len = postings.term_count();
    // Names the fold's own term-image files; the publication's own counter is created later.
    let mut index = tessera_store::derived::DerivedIndex::default();
    for segment in segments {
        // A view with no row projects every posting to the empty set, so neither has an image.
        if segment.row_count == 0 || dict_len == 0 {
            continue;
        }
        let permutation_path = ctx.to_prefix_dir.join(format!(
            "partitions/{}/{}/permutation.bin",
            plan.partition,
            tessera_store::view_rel(&segment.view)
        ));
        // Reloaded from the file pass 1 wrote, so the images are a function of the published permutation.
        let permutation = tessera_store::Permutation::load(&permutation_path)
            .map_err(|e| failed("pass 2b (term images: the new permutation)", &e))?;
        let space = tessera_store::RowSpace::new(Arc::new(permutation), segment.row_count);
        let stamp = tessera_store::term_images::TermImageStamp {
            prefix: ctx.to_prefix.clone(),
            view: segment.view.clone(),
            base_seg_id: segment.seg_id.clone(),
            incarnation: segment.incarnation,
            base_rows: segment.row_count,
            bound: space.base().bound(),
        };
        let file = tessera_store::derived::term_image_file(
            &ctx.to_prefix_dir,
            &plan.partition,
            TERM_IMAGE_MANIFEST_N,
            &mut index,
        )
        .map_err(|e| failed("pass 2b (term images: naming the file)", &e))?;

        // Adapts the postings format for the derivation, since `tessera-store` cannot depend on `tessera-authz`.
        let walk = |term: u32,
                    visit: &mut dyn FnMut(tessera_store::derived::PostingSlice<'_>)|
         -> std::io::Result<()> {
            if let Some(posting) = postings.posting_at(term)? {
                match posting {
                    tessera_authz::PostingRef::Array(bytes) => {
                        visit(tessera_store::derived::PostingSlice::Array(bytes))
                    }
                    tessera_authz::PostingRef::Roaring(bitmap) => {
                        visit(tessera_store::derived::PostingSlice::Roaring(&bitmap))
                    }
                }
            }
            Ok(())
        };
        let summary = tessera_store::term_images::derive_term_images(
            &space,
            dict_len,
            &walk,
            &stamp,
            &file.path,
            tessera_store::term_images::DeriveOptions {
                threads: TERM_IMAGE_THREADS,
            },
        )
        .map_err(|e| failed("pass 2b (term images: the derivation)", &e))?;

        // Pass 5 digests and syncs what `written` names, so this pass syncs nothing of its own.
        out.push(file.rel.clone(), file.path);
        term_images.push(FoldedTermImages {
            extent: tessera_store::manifest::TermImageExtent {
                path: file.rel,
                view: segment.view.clone(),
                incarnation: segment.incarnation,
                dict_len,
                keep_rows_per_container: tessera_store::term_images::KEEP_ROWS_PER_CONTAINER
                    as u32,
            },
            summary,
        });
    }

    Ok(term_images)
}

/// Pass 3: one external-id run 0 and one locator, bounded at the snapshot's entity space so
/// post-snapshot locator extents stay reachable past it. Returns run 0's prefix-relative path.
/// Drops the tombstoned entities' keys, since leaving one standing would turn a lawful re-ingest
/// of that external id into a 409 once retirement makes `is_deleted` false.
fn fold_external_ids(
    plan: &FoldPlan,
    ctx: &FoldContext,
    entities_dir: &Path,
    out: &mut FoldOutput,
) -> Result<Option<String>, MaintenanceFailed> {
    let external_id_run = if plan.runs.is_empty() {
        None
    } else {
        let run_paths: Vec<PathBuf> = plan
            .runs
            .iter()
            .map(|rel| ctx.from_prefix_dir.join(rel))
            .collect();
        fold_external_id_runs(
            &run_paths,
            0,
            plan.entity_bound.saturating_sub(1),
            &plan.tombstones,
            entities_dir,
        )
        .map_err(|e| failed("pass 3 (external ids)", &e))?;
        // The sidecar derives the locator's path from run 0's directory, so run 0 must stay first.
        let run_rel = format!("partitions/{}/entities/external-ids.arrow", plan.partition);
        let locator_rel = format!("partitions/{}/entities/ext-locator.u32", plan.partition);
        out.push(run_rel.clone(), entities_dir.join("external-ids.arrow"));
        out.push(locator_rel, entities_dir.join("ext-locator.u32"));
        Some(run_rel)
    };

    Ok(external_id_run)
}

/// Rebuild each indexed `text` column's index from the layers the snapshot named, minus the
/// tombstoned entities.
///
/// A text column has no value column: its index is a token dictionary plus postings, so its
/// postings are merged directly, a union per term and a subtraction, rather than re-derived from
/// a folded value column.
///
/// The blanked set is the tombstone set, the same set every other pass takes; a suppression
/// touches no artefact here. A term whose only carriers were deleted is dropped from the merged
/// dictionary, so the word itself leaves the corpus. Carrying the index forward untouched would
/// leave a deleted entity's terms in the postings after its overlay entry retired.
///
/// Streamed one term at a time: the layers' dictionaries are merged by a k-way scan, each
/// surviving term's posting is encoded and appended to a spool, and the dictionary is written
/// through [`tessera_filter::SortedDictWriter`] as the merge decides each key.
///
/// The folded layer carries no presence bitmap: after this pass, "carries a value" is answered
/// from the record blob instead.
fn fold_text_columns(
    plan: &FoldPlan,
    ctx: &FoldContext,
    out: &mut FoldOutput,
) -> Result<(), MaintenanceFailed> {
    // Entity-scoped indexed text columns, then one job per view of each indexed scoped text family.
    let mut jobs: Vec<ColumnJob> = ctx
        .declared_scalars
        .iter()
        .filter(|d| d.arrow_type == ScalarType::Text && d.index)
        .map(|d| ColumnJob {
            rel: format!("partitions/{}/attrs/{}", plan.partition, d.name),
            name: d.name.clone(),
            view: None,
            arrow_type: d.arrow_type,
            postings: false,
        })
        .collect();
    for family in ctx
        .scoped_scalars
        .iter()
        .filter(|f| f.arrow_type == ScalarType::Text && f.index)
    {
        for view in &family.views {
            let Some(rel) = scoped_job_rel(plan, ctx, &family.name, view) else {
                continue;
            };
            jobs.push(ColumnJob {
                rel,
                name: family.name.clone(),
                view: Some(view.clone()),
                arrow_type: family.arrow_type,
                postings: false,
            });
        }
    }
    for job in &jobs {
        let scalar = job;
        let incarnation = job_incarnation(ctx, job);
        let column_rel = job.rel.clone();
        let from_dir = ctx.from_prefix_dir.join(&column_rel);
        let to_dir = ctx.to_prefix_dir.join(&column_rel);
        std::fs::create_dir_all(&to_dir).map_err(|e| failed("pass 4a (text)", &e))?;

        // The base build's layer first, then one per published extent. The dictionaries are
        // advised sequential; the postings are not, since the merge interleaves reads across layers.
        // A text column declared at a running service has no base index until this pass writes one.
        let unfolded = job.view.is_none() && ctx.runtime_attributes.contains(&job.name);
        let mut dicts = Vec::new();
        let mut postings = Vec::new();
        if !unfolded {
            dicts.push(
                tessera_filter::SortedDict::open_dir(
                    &from_dir,
                    tessera_filter::Access::MappedSequential,
                )
                .map_err(|e| failed("pass 4a (text: the base dictionary)", &e))?,
            );
            postings.push(
                tessera_filter::ColumnPostings::open(&from_dir.join("postings.arrow"), true)
                    .map_err(|e| failed("pass 4a (text: the base postings)", &e))?,
            );
            out.attr_read += file_len(&from_dir.join(tessera_filter::DICT_FILE))
                + file_len(&from_dir.join("postings.arrow"));
        }
        for extent in plan.text_extents.iter().filter(|e| {
            e.column == scalar.name && e.view == job.view && e.incarnation == incarnation
        }) {
            dicts.push(
                tessera_filter::SortedDict::open(
                    &ctx.from_prefix_dir.join(&extent.dict),
                    tessera_filter::Access::MappedSequential,
                )
                .map_err(|e| failed("pass 4a (text: an extent's dictionary)", &e))?,
            );
            postings.push(
                tessera_filter::ColumnPostings::open(
                    &ctx.from_prefix_dir.join(&extent.postings),
                    true,
                )
                .map_err(|e| failed("pass 4a (text: an extent's postings)", &e))?,
            );
            for rel in extent.files() {
                out.attr_read += file_len(&ctx.from_prefix_dir.join(rel));
            }
        }
        let dict_rel = format!("{column_rel}/{}", tessera_filter::DICT_FILE);
        let postings_rel = format!("{column_rel}/postings.arrow");
        let dict_path = ctx.to_prefix_dir.join(&dict_rel);
        let postings_path = ctx.to_prefix_dir.join(&postings_rel);
        let spool_path = to_dir.join("postings.spool");

        // No presence is passed: this pass writes a base, and a base carries no presence bitmap.
        let inputs: Vec<tessera_filter_write::TextLayerRef<'_>> = dicts
            .iter()
            .zip(postings.iter())
            .map(|(dict, postings)| tessera_filter_write::TextLayerRef {
                dict,
                postings,
                present: None,
            })
            .collect();
        let outcome = tessera_filter_write::merge_text_layers(
            &inputs,
            &plan.tombstones,
            &dict_path,
            &postings_path,
            &spool_path,
        )
        .map_err(|e| failed(&format!("pass 4a (text: column '{}')", scalar.name), &e));
        // `finish` removes the spool on success; on failure it is this function's to remove.
        if outcome.is_err() {
            let _ = std::fs::remove_file(&spool_path);
        }
        outcome?;

        out.wrote(dict_rel, dict_path);
        out.wrote(postings_rel, postings_path);
    }
    Ok(())
}

/// Pass 4a: every declared filter column that owes a value column, in manifest order.
fn fold_value_columns(
    plan: &FoldPlan,
    ctx: &FoldContext,
    out: &mut FoldOutput,
) -> Result<(), MaintenanceFailed> {
    for job in value_column_jobs(plan, ctx) {
        fold_value_column(plan, ctx, &job, out)?;
    }
    Ok(())
}

/// Fold one value column: its base and every snapshot extent merged in entity order into one new
/// base, the tombstoned entities blanked from presence with their value bytes never written, and
/// a category's postings rebuilt whole from the folded column. This pass is where a deleted
/// entity's filter value leaves the corpus, since the value column is positional. A suppression
/// touches no attribute artefact.
fn fold_value_column(
    plan: &FoldPlan,
    ctx: &FoldContext,
    job: &ColumnJob,
    out: &mut FoldOutput,
) -> Result<(), MaintenanceFailed> {
    let scalar = job;
    let incarnation = job_incarnation(ctx, job);
    let belongs = |e: &&AttrExtent| {
        e.column == scalar.name && e.view == job.view && e.incarnation == incarnation
    };
    let column_rel = job.rel.clone();
    let from_dir = ctx.from_prefix_dir.join(&column_rel);
    let to_dir = ctx.to_prefix_dir.join(&column_rel);
    std::fs::create_dir_all(&to_dir).map_err(|e| failed("pass 4a (attributes)", &e))?;

    // Advised sequential: the merge below streams each layer exactly once in entity order.
    // A column declared at a running service has no base until this pass writes one.
    let unfolded = job.view.is_none() && ctx.runtime_attributes.contains(&job.name);
    let base = if unfolded {
        None
    } else {
        let base = tessera_filter::ValueColumn::open_dir(
            &from_dir,
            tessera_filter::Access::MappedSequential,
        )
        .map_err(|e| failed("pass 4a (attributes: the base column)", &e))?;
        out.attr_read += file_len(&from_dir.join(tessera_filter::VALUES_FILE))
            + file_len(&from_dir.join(tessera_filter::PRESENCE_FILE));
        Some(base)
    };
    let mut extents = Vec::new();
    for extent in plan.attr_extents.iter().filter(belongs) {
        extents.push(
            tessera_filter::open_extent(
                &ctx.from_prefix_dir.join(&extent.values),
                &ctx.from_prefix_dir.join(&extent.presence),
                tessera_filter::Access::MappedSequential,
            )
            .map_err(|e| failed("pass 4a (attributes: an extent)", &e))?,
        );
        out.attr_read += file_len(&ctx.from_prefix_dir.join(&extent.values))
            + file_len(&ctx.from_prefix_dir.join(&extent.presence));
    }
    let layers: Vec<&tessera_filter::ValueColumn> = base.iter().chain(extents.iter()).collect();
    // A keyword layer's dictionary, opened beside its ordinals. Empty for every other family,
    // which selects the generic fold below.
    let mut keyword_dicts: Vec<tessera_filter::SortedDict> = Vec::new();
    if scalar.arrow_type == tessera_spatial::tiler::ScalarType::Keyword {
        if !unfolded {
            keyword_dicts.push(
                tessera_filter::SortedDict::open_dir(
                    &from_dir,
                    tessera_filter::Access::MappedSequential,
                )
                .map_err(|e| failed("pass 4a (attributes: the base dictionary)", &e))?,
            );
        }
        for extent in plan.attr_extents.iter().filter(belongs) {
            let Some(dict_rel) = extent.dict.as_ref() else {
                return Err(MaintenanceFailed(format!(
                    "pass 4a (attributes): keyword column '{}' has an extent with no \
                     dictionary; its ordinals name nothing",
                    scalar.name
                )));
            };
            keyword_dicts.push(
                tessera_filter::SortedDict::open(
                    &ctx.from_prefix_dir.join(dict_rel),
                    tessera_filter::Access::MappedSequential,
                )
                .map_err(|e| failed("pass 4a (attributes: an extent dictionary)", &e))?,
            );
        }
    }

    let values_rel = format!("{column_rel}/{}", tessera_filter::VALUES_FILE);
    let presence_rel = format!("{column_rel}/{}", tessera_filter::PRESENCE_FILE);
    let dict_rel = format!("{column_rel}/{}", tessera_filter::DICT_FILE);
    let values_path = ctx.to_prefix_dir.join(&values_rel);
    let presence_path = ctx.to_prefix_dir.join(&presence_rel);
    let dict_path = ctx.to_prefix_dir.join(&dict_rel);
    // The snapshot's entity space, which the folded column covers. A column dense to this bound
    // writes no presence bitmap; a deletion below the bound is what takes that away.
    let bound = u32::try_from(plan.entity_bound).map_err(|_| {
        MaintenanceFailed("pass 4a (attributes): the entity bound exceeds u32".to_string())
    })?;
    // A keyword folds through its own pass: its dictionary is rebuilt from the survivors and its
    // ordinals renumbered, unlike the generic fold which carries values byte-preserved.
    let partial = if layers.is_empty() {
        // Nothing has carried the column: an empty base with an empty presence.
        write_empty_value_column(
            &values_path,
            &presence_path,
            column_kind_of(scalar.arrow_type, job.postings),
        )
        .map_err(|e| failed("pass 4a (attributes: an empty base)", &e))?;
        if scalar.arrow_type == tessera_spatial::tiler::ScalarType::Keyword {
            write_empty_dictionary(&dict_path)
                .map_err(|e| failed("pass 4a (attributes: an empty dictionary)", &e))?;
            out.wrote(dict_rel, dict_path.clone());
        }
        true
    } else if keyword_dicts.is_empty() {
        tessera_filter_write::fold_value_column(
            &layers,
            &plan.tombstones,
            bound,
            &values_path,
            &presence_path,
        )
        .map_err(|e| failed("pass 4a (attributes: the merge)", &e))?
    } else {
        let keyword_layers: Vec<tessera_filter_write::KeywordLayer<'_>> = layers
            .iter()
            .zip(keyword_dicts.iter())
            .map(|(values, dict)| tessera_filter_write::KeywordLayer { values, dict })
            .collect();
        let partial = tessera_filter_write::fold_keyword_column(
            &keyword_layers,
            &plan.tombstones,
            bound,
            &values_path,
            &presence_path,
            &dict_path,
        )
        .map_err(|e| failed("pass 4a (attributes: the keyword merge)", &e))?;
        out.wrote(dict_rel, dict_path.clone());
        partial
    };
    out.wrote(values_rel, values_path.clone());
    if partial {
        out.wrote(presence_rel, presence_path.clone());
    }

    if !job.postings {
        return Ok(());
    }
    // Rebuilt from the folded column, read back rather than from the layers it was merged from.
    // Mapped without the sequential hint, since the banded emit scans this column once per band.
    let folded = tessera_filter::ValueColumn::open(
        &values_path,
        partial.then_some(presence_path.as_path()),
        tessera_filter::Access::Mapped,
    )
    .map_err(|e| failed("pass 4a (attributes: reopening the folded column)", &e))?;
    let postings_rel = format!("{column_rel}/postings.arrow");
    let postings_path = ctx.to_prefix_dir.join(&postings_rel);
    tessera_filter_write::write_category_postings(
        &postings_path,
        &scalar.name,
        &folded,
        tessera_filter_write::POSTINGS_BAND_ROWS,
    )
    .map_err(|e| failed("pass 4a (attributes: the postings rebuild)", &e))?;
    out.wrote(postings_rel, postings_path);

    Ok(())
}

/// Pass 4a, continued: the record blob rewritten without the tombstoned entities' rows, so a
/// deleted entity's prose is absent from the folded artefact. A suppressed entity's row streams
/// through unchanged. The blob exists only where the schema declares a blob-resident column, so a
/// mismatch refuses rather than silently dropping extents' bytes.
fn fold_record_blob(
    plan: &FoldPlan,
    ctx: &FoldContext,
    out: &mut FoldOutput,
) -> Result<(), MaintenanceFailed> {
    let blob_resident = ctx
        .declared_scalars
        .iter()
        .any(|d| crate::filter::blob_resident(d, &ctx.vocabularies));
    // A blob-resident column declared at a running service has extents alone until this pass writes the base.
    let based_blob_resident = ctx.declared_scalars.iter().any(|d| {
        !ctx.runtime_attributes.contains(&d.name)
            && crate::filter::blob_resident(d, &ctx.vocabularies)
    });
    if !blob_resident && !plan.record_extents.is_empty() {
        return Err(MaintenanceFailed(
            "pass 4a (record blob): the manifest names record extents but the schema declares no \
             blob-resident column; folding would drop their bytes silently, so it is refused"
                .to_string(),
        ));
    }
    if blob_resident {
        let record_rel = format!("partitions/{}/attrs/record", plan.partition);
        let from_dir = ctx.from_prefix_dir.join(&record_rel);
        let to_dir = ctx.to_prefix_dir.join(&record_rel);
        std::fs::create_dir_all(&to_dir).map_err(|e| failed("pass 4a (record blob)", &e))?;

        // The fold's own mappings, advised sequential: each layer streams once, block by block.
        let base = if based_blob_resident {
            let base = tessera_filter::RecordBlob::open_dir(
                &from_dir,
                tessera_filter::Access::MappedSequential,
            )
            .map_err(|e| failed("pass 4a (record blob: the base)", &e))?;
            for name in [
                tessera_filter::RECORD_BLOCKS_FILE,
                tessera_filter::RECORD_HASROW_FILE,
                tessera_filter::RECORD_DIRECTORY_FILE,
            ] {
                out.attr_read += file_len(&from_dir.join(name));
            }
            Some(base)
        } else {
            None
        };
        let mut extents = Vec::with_capacity(plan.record_extents.len());
        for extent in &plan.record_extents {
            extents.push(
                tessera_filter::RecordBlob::open(
                    &ctx.from_prefix_dir.join(&extent.blocks),
                    &ctx.from_prefix_dir.join(&extent.hasrow),
                    &ctx.from_prefix_dir.join(&extent.directory),
                    tessera_filter::Access::MappedSequential,
                )
                .map_err(|e| failed("pass 4a (record blob: an extent)", &e))?,
            );
            for rel in extent.files() {
                out.attr_read += file_len(&ctx.from_prefix_dir.join(rel));
            }
        }
        let layers: Vec<&tessera_filter::RecordBlob> = base.iter().chain(extents.iter()).collect();

        let blocks_rel = format!("{record_rel}/{}", tessera_filter::RECORD_BLOCKS_FILE);
        let hasrow_rel = format!("{record_rel}/{}", tessera_filter::RECORD_HASROW_FILE);
        let directory_rel = format!("{record_rel}/{}", tessera_filter::RECORD_DIRECTORY_FILE);
        let blocks_path = ctx.to_prefix_dir.join(&blocks_rel);
        let hasrow_path = ctx.to_prefix_dir.join(&hasrow_rel);
        let directory_path = ctx.to_prefix_dir.join(&directory_rel);
        if layers.is_empty() {
            // No flush has carried this column yet: an empty base, so the reopen finds the blob.
            tessera_filter_write::RecordBlobWriter::create(
                &blocks_path,
                &hasrow_path,
                &directory_path,
                tessera_filter::RECORD_BLOCK_TARGET,
            )
            .and_then(|writer| writer.finish())
            .map_err(|e| failed("pass 4a (record blob: an empty base)", &e))?;
        } else {
            tessera_filter_write::fold_record_blob(
                &layers,
                &plan.tombstones,
                &blocks_path,
                &hasrow_path,
                &directory_path,
                tessera_filter::RECORD_BLOCK_TARGET,
            )
            .map_err(|e| failed("pass 4a (record blob: the rewrite)", &e))?;
        }
        for (rel, path) in [
            (blocks_rel, blocks_path),
            (hasrow_rel, hasrow_path),
            (directory_rel, directory_path),
        ] {
            out.wrote(rel, path);
        }
    }

    Ok(())
}

/// Pass 4c: the entity→term transpose, base plus every snapshot extent streamed in entity order
/// into one new base, with the tombstoned entities emitting nothing. A suppressed entity's list
/// streams through unchanged. No ordinal is remapped, since a stored ordinal is a position in the
/// concatenation of the dictionary extents, carried forward verbatim.
fn fold_entity_terms(
    plan: &FoldPlan,
    ctx: &FoldContext,
    out: &mut FoldOutput,
) -> Result<(), MaintenanceFailed> {
    let terms_rel = format!(
        "partitions/{}/{}",
        plan.partition,
        tessera_store::ENTITY_TERMS_DIR
    );
    let from_dir = ctx.from_prefix_dir.join(&terms_rel);
    let to_dir = ctx.to_prefix_dir.join(&terms_rel);
    std::fs::create_dir_all(&to_dir).map_err(|e| failed("pass 4c (entity terms)", &e))?;

    let mut extent_paths = Vec::with_capacity(plan.entity_terms_extents.len());
    for extent in &plan.entity_terms_extents {
        extent_paths.push(tessera_store::EntityTermsExtentPaths {
            hasrow: ctx.from_prefix_dir.join(&extent.hasrow),
            offsets: ctx.from_prefix_dir.join(&extent.offsets),
            terms: ctx.from_prefix_dir.join(&extent.terms),
            bases: ctx.from_prefix_dir.join(&extent.bases),
        });
        for rel in extent.files() {
            out.attr_read += file_len(&ctx.from_prefix_dir.join(rel));
        }
    }
    for name in [
        tessera_store::ENTITY_TERMS_HASROW_FILE,
        tessera_store::ENTITY_TERMS_OFFSETS_FILE,
        tessera_store::ENTITY_TERMS_TERMS_FILE,
        tessera_store::ENTITY_TERMS_BASES_FILE,
    ] {
        out.attr_read += file_len(&from_dir.join(name));
    }
    let layers = tessera_store::EntityTermsStack::open(Some(&from_dir), &extent_paths)
        .map_err(|e| failed("pass 4c (entity terms: the layers)", &e))?;
    let mut writer = tessera_store::EntityTermsWriter::create(&to_dir)
        .map_err(|e| failed("pass 4c (entity terms: the rewrite)", &e))?;
    // One ascending pass over the union of the layers' has-row sets, the order the writer
    // requires and every layer already holds.
    let live = layers.entity_set();
    for entity in live.iter() {
        if plan.tombstones.contains(entity) {
            continue;
        }
        let Some(terms) = layers
            .terms_of(entity)
            .map_err(|e| failed("pass 4c (entity terms: a layer)", &e))?
        else {
            continue;
        };
        writer
            .push(entity, &terms)
            .map_err(|e| failed("pass 4c (entity terms: the rewrite)", &e))?;
    }
    for path in writer
        .finish()
        .map_err(|e| failed("pass 4c (entity terms: the rewrite)", &e))?
    {
        let rel = format!(
            "{terms_rel}/{}",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
        );
        out.wrote(rel, path);
    }

    Ok(())
}

/// Pass 5: a digest of every file the fold wrote, and an fsync of all of them, before a fold flips
/// `CURRENT` onto this prefix and deletes the old tree.
fn digest_and_sync(out: &FoldOutput) -> Result<BTreeMap<String, FileDigest>, MaintenanceFailed> {
    let mut files = BTreeMap::new();
    for (rel, path) in &out.written {
        files.insert(
            rel.clone(),
            crate::flush::digest_of(path)?,
        );
    }
    let paths: Vec<PathBuf> = out.written.iter().map(|(_, path)| path.clone()).collect();
    tessera_store::fsync_written(&paths).map_err(|e| failed("pass 5 (durability)", &e))?;

    Ok(files)
}

/// The next `v#####` prefix name under `bundle_root`: one past the highest already present.
/// Derived from the directory listing, since a discarded fold leaves a complete `v#####` tree
/// that `CURRENT` never named.
pub(crate) fn next_prefix_name(bundle_root: &Path) -> std::io::Result<String> {
    let mut highest = 0u64;
    for entry in std::fs::read_dir(bundle_root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(digits) = name.strip_prefix('v') else {
            continue;
        };
        // At least five digits, not exactly five: `{:05}` is a minimum width, so the
        // hundred-thousandth prefix is `v100000`, six digits.
        if digits.len() < 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(n) = digits.parse::<u64>() {
            highest = highest.max(n);
        }
    }
    Ok(format!("v{:05}", highest + 1))
}

/// An entity-space value column with no entity in it. Written with an empty presence bitmap, so
/// the reader takes it as partial rather than as dense to the bound.
fn write_empty_value_column(
    values_path: &Path,
    presence_path: &Path,
    kind: tessera_filter::ColumnKind,
) -> std::io::Result<()> {
    tessera_filter::ValueColumnWriter::create(values_path, presence_path, kind)?
        .finish(Some(&Bitmap::new()))
}

/// A keyword column's dictionary with no key in it, beside [`write_empty_value_column`]'s ordinals.
fn write_empty_dictionary(dict_path: &Path) -> std::io::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(dict_path)?);
    tessera_filter::SortedDictWriter::new(file)
        .and_then(|writer| writer.finish())
        .map(|_| ())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// The storage kind a value column of this declared type is written at. A keyword's values are
/// `u32` ordinals; a `bool` stores as a `u8`, a `timestamp_us` as the `i64` it is.
fn column_kind_of(arrow_type: ScalarType, category: bool) -> tessera_filter::ColumnKind {
    use tessera_filter::ColumnKind;
    if arrow_type == ScalarType::Keyword {
        return ColumnKind::U32;
    }
    if category {
        return match arrow_type {
            ScalarType::U8 => ColumnKind::U8,
            ScalarType::U16 => ColumnKind::U16,
            _ => ColumnKind::U32,
        };
    }
    match arrow_type {
        ScalarType::Bool | ScalarType::U8 => ColumnKind::U8,
        ScalarType::U16 => ColumnKind::U16,
        ScalarType::U32 => ColumnKind::U32,
        ScalarType::U64 => ColumnKind::U64,
        ScalarType::I8 => ColumnKind::I8,
        ScalarType::I16 => ColumnKind::I16,
        ScalarType::I32 => ColumnKind::I32,
        ScalarType::I64 | ScalarType::TimestampUs => ColumnKind::I64,
        ScalarType::F32 => ColumnKind::F32,
        ScalarType::F64 => ColumnKind::F64,
        // Unreachable: `utf8` is not declarable and `text` folds through its own pass.
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => ColumnKind::U32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tessera_store::manifest::LocatorExtent;

    fn segment(seg_id: &str, entity_lo: u64, entity_hi: u64) -> SegmentDescriptor {
        SegmentDescriptor {
            incarnation: 0,
            view: "s0".to_string(),
            seg_id: seg_id.to_string(),
            row_count: (entity_hi - entity_lo + 1) as u32,
            entity_lo,
            entity_hi,
        }
    }

    fn locator(entity_lo: u64, entity_hi: u64) -> LocatorExtent {
        LocatorExtent {
            path: "entities/ext-locator-1.u32".to_string(),
            entity_lo,
            entity_hi,
            external_id_run: "entities/external-ids-1.arrow".to_string(),
        }
    }

    fn bitmap(entities: &[u32]) -> Bitmap {
        Bitmap::of(entities)
    }

    /// **A deletion whose entity nothing carried forward retires; one that a carried-forward
    /// segment still names does not.** The base case compaction §5's rule exists for.
    ///
    /// **Mutations this kills:** retiring `D₀` wholesale (entity 50 retires, which is the r3
    /// fail-open); subtracting in the other direction (nothing retires).
    #[test]
    fn a_deletion_the_fold_removed_retires_and_one_still_carried_does_not() {
        let d0 = bitmap(&[7, 9, 50]);
        let mut carried = CarriedForward::new();
        // A flush that landed during the fold's flight, publishing entities 40..=60.
        carried.add_segment(&segment("flush-3-1", 40, 60));

        let executed = executed(&d0, &carried);

        assert!(executed.contains(7), "7 lost its row and its postings");
        assert!(executed.contains(9));
        assert!(
            !executed.contains(50),
            "50's row was published into the old prefix after the snapshot and carried forward — \
             retiring it withdraws the only thing hiding it"
        );
    }

    /// **`executed ⊆ D₀` always** — a carried-forward artefact naming entities that were never
    /// deleted changes nothing, and no entity outside `D₀` can ever retire.
    #[test]
    fn nothing_outside_d0_can_retire() {
        let d0 = bitmap(&[3]);
        let mut carried = CarriedForward::new();
        carried.add_segment(&segment("flush-3-1", 100, 200));

        let executed = executed(&d0, &carried);

        assert_eq!(executed.cardinality(), 1);
        assert!(executed.contains(3));
        assert!(d0.andnot(&executed).is_empty() || executed.andnot(&d0).is_empty());
        assert!(
            executed.andnot(&d0).is_empty(),
            "executed must be a subset of D₀ — it is only ever a subtraction from it"
        );
    }

    /// **Obligation 2b's three shapes, each protected by a *different* carried artefact.**
    ///
    /// A carried-forward tier holds a deleted entity's postings; a carried-forward segment holds
    /// its row; and a carried-forward **run** holds its external-id binding while *no tier names it
    /// at all* — a zero-term item, which produces no `(term, entity)` pair because nothing on the
    /// ingest path refuses one. All three must survive the fold with their overlay entries intact.
    ///
    /// **The ranges are deliberately disjoint, and an earlier version of this test was not.** With
    /// one flush's segment and locator covering the same entities, either one alone protected all
    /// three shapes, so dropping `add_segment` entirely left this test green — it asserted the
    /// outcome without pinning what produced it. Two flushes, one contributing only a segment and
    /// the other only a locator extent, make each artefact kind independently load-bearing.
    ///
    /// **Mutations this kills:** dropping segments from the carry-forward set (51 and 53 retire —
    /// this is the "tiers alone" shape, since a tier's entities are covered by its flush's segment
    /// range and nothing else here names them); dropping locator extents (55 retires, which is the
    /// zero-term item whose binding a re-ingest would then hit as a 409, decision 0047).
    #[test]
    fn none_of_obligation_2bs_three_shapes_retires() {
        // 51: postings in a carried-forward tier. 53: a row in a carried-forward segment. Both are
        // covered by their flush's segment range and by nothing else in this fixture.
        // 55: a zero-term item, reachable here only through the run its locator extent names.
        let d0 = bitmap(&[51, 53, 55, 90]);

        let mut carried = CarriedForward::new();
        carried.add_segment(&segment("flush-3-1", 50, 53));
        carried.add_locator_extent(&locator(54, 56));

        let executed = executed(&d0, &carried);

        for entity in [51u32, 53, 55] {
            assert!(
                !executed.contains(entity),
                "entity {entity} is named by a carried-forward artefact and must not retire"
            );
        }
        assert!(
            executed.contains(90),
            "a deletion nothing carried forward still retires — the rule must not be vacuous"
        );
    }

    /// **A run carried forward on its own still protects its entities**, so the rule does not
    /// depend on a segment always accompanying it. Merge and coalesce are suspended during a fold,
    /// which is what makes the segment range sufficient *today*; this keeps the rule correct if
    /// that ever gives, rather than resting on the suspension.
    #[test]
    fn a_locator_extent_alone_protects_its_range() {
        let d0 = bitmap(&[12]);
        let mut carried = CarriedForward::new();
        carried.add_locator_extent(&locator(10, 20));

        assert!(executed(&d0, &carried).is_empty());
    }

    /// **An empty carry-forward set retires the whole of `D₀`** — the quiet-deployment case, where
    /// no flush landed during the fold's flight. Stated because it is the one case where the rule
    /// and the fail-open definition agree, and a test suite that only covered it would prove
    /// nothing.
    #[test]
    fn with_nothing_carried_forward_every_tombstone_retires() {
        let d0 = bitmap(&[1, 2, 3]);
        let executed = executed(&d0, &CarriedForward::new());
        assert_eq!(executed.cardinality(), 3);
    }

    /// An entity range past `u32::MAX` clamps **outward**, naming more rather than fewer. A
    /// truncating cast would silently stop protecting the entities above the cut.
    #[test]
    fn an_out_of_range_carry_forward_clamps_outward_never_dropping_protection() {
        let d0 = bitmap(&[u32::MAX, u32::MAX - 1]);
        let mut carried = CarriedForward::new();
        carried.add_segment(&segment("huge", u32::MAX as u64 - 1, u32::MAX as u64 + 100));

        assert!(
            executed(&d0, &carried).is_empty(),
            "both entities are inside the carried range once it is clamped outward"
        );
    }

    /// **The next prefix is one past the highest `v#####` present, not one past the live one.**
    ///
    /// A discarded fold leaves a complete tree under a name `CURRENT` never took. Numbering from
    /// the live prefix would hand the next fold that same name, whose first carry-forward link then
    /// refuses — after the corpus has already been re-read.
    ///
    /// **Mutation this kills:** deriving the name from the live prefix (`v00003` here would come
    /// back as `v00001`).
    #[test]
    fn the_next_prefix_steps_past_every_name_present_including_an_orphan() {
        let tmp = tempfile::TempDir::new().unwrap();
        for name in ["v00000", "v00003", "not-a-prefix", "v0004"] {
            std::fs::create_dir(tmp.path().join(name)).unwrap();
        }
        assert_eq!(next_prefix_name(tmp.path()).unwrap(), "v00004");
    }

    /// **The scan does not stop seeing prefixes at the hundred-thousandth.** `{:05}` is a minimum
    /// width, so `v100000` is a legitimate name this function itself produces — and a parser that
    /// required exactly five digits would ignore it, recompute `v100000` at every fold for ever,
    /// and collide with the tree already there after each fold had re-read the corpus.
    ///
    /// **Mutation this kills:** `digits.len() != 5` in place of `< 5` (the answer becomes
    /// `v100000`, which already exists).
    #[test]
    fn the_next_prefix_keeps_counting_past_five_digits() {
        let tmp = tempfile::TempDir::new().unwrap();
        for name in ["v00000", "v99999", "v100000"] {
            std::fs::create_dir(tmp.path().join(name)).unwrap();
        }
        assert_eq!(next_prefix_name(tmp.path()).unwrap(), "v100001");
    }

    /// An empty bundle root still names a prefix rather than failing — the shape a fold would meet
    /// only if the root held no prefix at all, which is not a state a fold reaches, but the
    /// arithmetic must not underflow to reach it.
    #[test]
    fn the_next_prefix_over_an_empty_root_is_the_first_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(next_prefix_name(tmp.path()).unwrap(), "v00001");
    }

    // ---- the schedule (compaction §9, decision 0056) -----------------------------------------

    /// Midnight UTC + 4 h, 8 segments, 500,000 deletions, 24 h floor — `tessera-server`'s defaults,
    /// so these cases exercise the shipped configuration rather than a shape invented for them.
    fn schedule() -> CompactionSchedule {
        CompactionSchedule {
            min_interval_secs: 86_400,
            window_start_secs: Some(0),
            window_secs: 4 * 3_600,
            window_min_segments: 8,
            max_segments: Some(64),
            after_deletions: Some(500_000),
            // The two ratio gauges are off in this helper: every case below is about the counts and
            // the window, and a live ratio would give some of them a second reason to fire that the
            // assertion could not tell apart. `the_two_ratio_gauges_*` arm them deliberately.
            tombstoned_rows_fraction: None,
            dead_bytes_ratio: None,
        }
    }

    /// [`due`] with the row count zero and the dead-bytes walk absent — the shape every case but
    /// the ratio ones wants. A zero row count is what switches the tombstoned fraction off (a ratio
    /// has no denominator), and `|| None` is a walk that could not be performed.
    fn due_at(
        s: &CompactionSchedule,
        now: u64,
        last: Option<u64>,
        live_segments: usize,
        retirable_deletions: u64,
    ) -> Option<FoldTrigger> {
        due(
            s,
            now,
            last,
            Gauges {
                live_segments,
                retirable_deletions,
                live_rows: 0,
            },
            || None,
        )
    }

    /// Seconds since the epoch at `day` days past it, `hour`:00 UTC.
    fn at(day: u64, hour: u64) -> u64 {
        day * 86_400 + hour * 3_600
    }

    /// **Inside the window with enough segments, and nowhere else.** The window is a start time,
    /// and the segment count is what makes it a gauge rather than the pure timer compaction §9
    /// declines.
    ///
    /// **Mutations this kills:** dropping the window bound (09:00 fires); dropping the segment gate
    /// (01:00 with one segment fires, which is a fold that rewrites the corpus to reorganise
    /// nothing).
    #[test]
    fn the_windowed_route_fires_only_inside_the_window_and_only_with_work() {
        let s = schedule();
        assert_eq!(
            due_at(&s, at(10, 1), None, 8, 0),
            Some(FoldTrigger::Window),
            "01:00 with eight segments is the case the window exists for"
        );
        assert_eq!(
            due_at(&s, at(10, 9), None, 63, 0),
            None,
            "09:00 is outside the window at any count below the ceiling — a start time that fires \
             at breakfast after a restart is not a start time. (63 rather than an arbitrarily \
             large number, because past `max_segments` a different route takes over and this case \
             would stop being about the window at all.)"
        );
        assert_eq!(
            due_at(&s, at(10, 1), None, 7, 0),
            None,
            "and inside it, below the threshold, there is nothing worth folding"
        );
    }

    /// **The unwindowed routes fire at any hour**, because the costs they measure stop being
    /// deferrable: retirable depth grows without bound, and segment count past `max_segments` is a
    /// regression every viewport pays for until the next window.
    ///
    /// **Mutation this kills:** windowing either route — a deployment reaching its limit at 14:00
    /// then waits ten hours while every deny acceptance clones a growing overlay, or while every
    /// tile pays a binary search per segment.
    #[test]
    fn the_unwindowed_routes_are_not_windowed() {
        let s = schedule();
        assert_eq!(
            due_at(&s, at(10, 14), None, 1, 500_000),
            Some(FoldTrigger::RetirableDepth),
            "14:00, one segment, at the deletion limit"
        );
        assert_eq!(
            due_at(&s, at(10, 14), None, 1, 499_999),
            None,
            "and not below it"
        );

        assert_eq!(
            due_at(&s, at(10, 14), None, 64, 0),
            Some(FoldTrigger::SegmentCount),
            "14:00, no deletions at all, at the segment ceiling"
        );
        assert_eq!(
            due_at(&s, at(10, 14), None, 63, 0),
            None,
            "and not below it"
        );
    }

    /// **The two segment thresholds are a floor and a ceiling over one gauge**, and the window is
    /// what separates them: eight segments is worth a fold tonight, sixty-four is worth one now.
    ///
    /// **Mutations this kills:** collapsing the two into one threshold (either the window fires at
    /// 64 — so a deployment sitting at 8 never tidies — or the unwindowed route fires at 8, which
    /// is a fold in the middle of the working day for a cost that could have waited); reporting the
    /// window trigger for a count that cleared the ceiling, which would tell an operator the fold
    /// was routine when it was not.
    #[test]
    fn the_segment_gauge_has_a_window_floor_and_an_any_hour_ceiling() {
        let s = schedule();
        assert_eq!(due_at(&s, at(10, 14), None, 8, 0), None, "8 at 14:00 waits");
        assert_eq!(
            due_at(&s, at(10, 1), None, 8, 0),
            Some(FoldTrigger::Window),
            "8 inside the window folds, and is reported as the window"
        );
        assert_eq!(
            due_at(&s, at(10, 1), None, 64, 0),
            Some(FoldTrigger::SegmentCount),
            "64 inside the window folds too — and is reported as the ceiling, because that is \
             the reason that will still be true tomorrow"
        );
    }

    /// **The floor is under both routes**, and it is what keeps a daily window to one fold a day
    /// without any "did I already fire today" state.
    ///
    /// **Mutations this kills:** applying the floor to only one route (the deletions case fires an
    /// hour after the last fold); comparing rather than saturating (a clock stepping backwards
    /// makes `now - last` enormous and every gauge fires at once).
    #[test]
    fn the_minimum_interval_floors_both_routes_and_survives_a_backward_clock() {
        let s = schedule();
        let last = at(10, 1);
        assert_eq!(
            due_at(&s, at(10, 2), Some(last), 64, 999_999),
            None,
            "one hour later, with every gauge over its threshold — the floor is under all three"
        );
        assert_eq!(
            due_at(&s, at(11, 1), Some(last), 8, 0),
            Some(FoldTrigger::Window),
            "and the next night's window is exactly a day past it"
        );
        assert_eq!(
            due_at(&s, at(9, 1), Some(last), 64, 999_999),
            None,
            "a clock that stepped backwards reads as 'not yet', never as a huge elapsed time"
        );
    }

    /// **A window that wraps past midnight is the ordinary case for anything after noon**, and it
    /// is the only reason the containment test is arithmetic rather than two comparisons.
    #[test]
    fn a_window_starting_before_midnight_wraps_into_the_next_day() {
        let s = CompactionSchedule {
            window_start_secs: Some(23 * 3_600),
            window_secs: 4 * 3_600,
            ..schedule()
        };
        assert_eq!(
            due_at(&s, at(10, 23), None, 8, 0),
            Some(FoldTrigger::Window)
        );
        assert_eq!(
            due_at(&s, at(11, 1), None, 8, 0),
            Some(FoldTrigger::Window),
            "01:00 is two hours into a window that opened at 23:00"
        );
        assert_eq!(
            due_at(&s, at(11, 4), None, 8, 0),
            None,
            "and 04:00 is past its end"
        );
    }

    /// **Either route switches off on its own**, which is what `spec §9`'s "a deployment may switch
    /// each route off" means. A schedule with both off never dispatches, whatever the gauges say —
    /// the posture an embedder gets by default and the one `Engine::request_fold` exists beside.
    #[test]
    fn each_route_switches_off_independently_and_off_means_never() {
        let no_window = CompactionSchedule {
            window_start_secs: None,
            ..schedule()
        };
        assert_eq!(
            due_at(&no_window, at(10, 1), None, 8, 0),
            None,
            "a count that only clears the window's floor has no route left"
        );
        assert_eq!(
            due_at(&no_window, at(10, 1), None, 8, 500_000),
            Some(FoldTrigger::RetirableDepth),
            "and the unwindowed routes are untouched by it"
        );
        assert_eq!(
            due_at(&no_window, at(10, 1), None, 64, 0),
            Some(FoldTrigger::SegmentCount),
            "including the segment ceiling, which is where a window-less deployment's segment \
             growth is bounded"
        );

        let no_depth = CompactionSchedule {
            after_deletions: None,
            max_segments: None,
            ..schedule()
        };
        assert_eq!(due_at(&no_depth, at(10, 14), None, 1_000, u64::MAX), None);

        let no_ceiling = CompactionSchedule {
            max_segments: None,
            ..schedule()
        };
        assert_eq!(
            due_at(&no_ceiling, at(10, 14), None, 100_000, 0),
            None,
            "with the ceiling off, segment growth waits for the window however far it goes"
        );

        assert_eq!(
            due_at(&CompactionSchedule::off(), at(10, 1), None, 1_000, u64::MAX),
            None,
            "off is off at every hour, every segment count and every depth"
        );
    }

    /// A zero-width window never opens, and a width of a whole day never closes. Both are
    /// reachable by configuration and neither may read as its opposite.
    #[test]
    fn a_zero_width_window_never_opens_and_a_day_wide_one_never_closes() {
        for hour in [0u64, 1, 12, 23] {
            assert!(!in_window(at(10, hour), 0, 0), "zero width at {hour}:00");
            assert!(
                in_window(at(10, hour), 0, 86_400),
                "a day wide at {hour}:00"
            );
        }
    }

    /// **The two ratio gauges fire at any hour, and each catches what no count above can.**
    ///
    /// The tombstoned fraction is a different question from `after_deletions` over the same
    /// numerator: 10,000 deletions in a 50,000-row bundle is a fifth of every viewport's scanned
    /// rows wasted and nowhere near the absolute threshold. The byte ratio is the only route that
    /// covers reclamation at all — this deployment's segments and overlay are both healthy.
    ///
    /// Kills: dropping either route; windowing either of them (both are asserted at 14:00).
    #[test]
    fn the_two_ratio_gauges_fire_at_any_hour_on_bundles_no_count_gauge_would_fold() {
        let s = CompactionSchedule {
            tombstoned_rows_fraction: Some(0.2),
            dead_bytes_ratio: Some(1.0),
            ..schedule()
        };
        let healthy = |deletions: u64, rows: u64| Gauges {
            live_segments: 1,
            retirable_deletions: deletions,
            live_rows: rows,
        };

        assert_eq!(
            due(&s, at(10, 14), None, healthy(10_000, 50_000), || None),
            Some(FoldTrigger::TombstonedRows),
            "a fifth of the rows are tombstoned, outside the window, with one segment and an \
             overlay two orders of magnitude below `after_deletions`"
        );
        assert_eq!(
            due(&s, at(10, 14), None, healthy(9_999, 50_000), || None),
            None,
            "and just under the fraction, nothing fires"
        );
        assert_eq!(
            due(&s, at(10, 14), None, healthy(0, 50_000), || Some(
                DeadBytes {
                    on_disc: 200,
                    named: 100
                }
            )),
            Some(FoldTrigger::DeadBytes),
            "paying double for storage with nothing deleted and one segment — the reclamation \
             obligation, which no other gauge sees"
        );
        assert_eq!(
            due(&s, at(10, 14), None, healthy(0, 50_000), || Some(
                DeadBytes {
                    on_disc: 199,
                    named: 100
                }
            )),
            None,
            "and just under the ratio, nothing fires"
        );
        // **The ratio is dead-to-live, not total-to-live**, and at 1.0 the difference is every
        // bundle ever built: on disc always exceeds named, if only by the manifests' own bytes.
        assert_eq!(
            due(&s, at(10, 14), None, healthy(0, 50_000), || Some(
                DeadBytes {
                    on_disc: 101,
                    named: 100
                }
            )),
            None,
            "a bundle with 1% dead is not a bundle paying double"
        );
    }

    /// **The dead-bytes walk is not performed unless it decides something**, which is the whole
    /// reason it is a closure: it is the one gauge that is not a field read.
    ///
    /// Kills: calling it eagerly; ordering it before any cheaper route; consulting it with the
    /// route switched off.
    #[test]
    fn the_dead_bytes_walk_runs_only_when_every_cheaper_route_has_declined() {
        let s = CompactionSchedule {
            dead_bytes_ratio: Some(1.0),
            ..schedule()
        };
        let walked = std::cell::Cell::new(0u32);
        let walk = || {
            walked.set(walked.get() + 1);
            Some(DeadBytes {
                on_disc: 200,
                named: 100,
            })
        };

        // The interval floor declines before anything is read at all.
        assert_eq!(
            due(
                &s,
                at(10, 14),
                Some(at(10, 13)),
                Gauges {
                    live_segments: 1,
                    retirable_deletions: 0,
                    live_rows: 1
                },
                walk
            ),
            None
        );
        assert_eq!(walked.get(), 0, "a floored tick walks nothing");

        // A cheaper route firing decides it.
        assert_eq!(
            due(
                &s,
                at(10, 14),
                None,
                Gauges {
                    live_segments: 64,
                    retirable_deletions: 0,
                    live_rows: 1
                },
                walk
            ),
            Some(FoldTrigger::SegmentCount)
        );
        assert_eq!(
            walked.get(),
            0,
            "the segment ceiling decided it, so nothing walked"
        );

        // Nothing cheaper fires: now it walks.
        assert_eq!(
            due(
                &s,
                at(10, 14),
                None,
                Gauges {
                    live_segments: 1,
                    retirable_deletions: 0,
                    live_rows: 1
                },
                walk
            ),
            Some(FoldTrigger::DeadBytes)
        );
        assert_eq!(walked.get(), 1, "and exactly once");

        // Switched off, it is never consulted.
        let off = CompactionSchedule {
            dead_bytes_ratio: None,
            ..s
        };
        assert_eq!(
            due(
                &off,
                at(10, 14),
                None,
                Gauges {
                    live_segments: 1,
                    retirable_deletions: 0,
                    live_rows: 1
                },
                walk
            ),
            None
        );
        assert_eq!(walked.get(), 1, "an off route reads nothing");
    }

    /// A ratio with no denominator is not "infinitely dead". Both guards are the same shape and both
    /// are reachable: an empty bundle has no rows, and a bundle whose manifests name nothing has no
    /// named bytes — a fold at every tick over a corpus it cannot reduce.
    #[test]
    fn a_ratio_with_a_zero_denominator_never_fires() {
        let s = CompactionSchedule {
            tombstoned_rows_fraction: Some(0.2),
            dead_bytes_ratio: Some(1.0),
            ..schedule()
        };
        assert_eq!(
            due(
                &s,
                at(10, 14),
                None,
                Gauges {
                    live_segments: 1,
                    retirable_deletions: 500,
                    live_rows: 0
                },
                || Some(DeadBytes {
                    on_disc: 1_000,
                    named: 0
                })
            ),
            None,
            "no rows and no named bytes: neither ratio is defined, and neither may fire"
        );
    }

    /// **The memory estimate is spec §3's table, and the figure it produces at 10⁹ is the one that
    /// document states.** §3 budgets ~9–10 GB at 10⁹ entities over 1.17×10⁸ terms; this asserts the
    /// estimate lands in that band, which is what makes the pre-flight a check against the design
    /// rather than against a number invented at the call site.
    ///
    /// Kills the mutation that drops either 4 B array — either one alone gives ~5.9 GB and the
    /// assertion fails low, which is r4's original error (`permutation.bin` omitted) reintroduced.
    #[test]
    fn the_memory_estimate_is_section_3s_budget_at_ten_to_the_nine() {
        let need = memory_estimate(1_000_000_000, 1_000_000_000, 117_000_000, 0, 1_000_000_000);
        let gb = need as f64 / 1e9;
        assert!(
            (17.0..=19.0).contains(&gb),
            "the estimate is {gb:.1} GB; spec §3 budgets ~9–10 GB and this carries \
             FOLD_MEMORY_SAFETY_FACTOR on top, so ~18.8 GB is the figure"
        );
    }

    /// The permutation term is the **largest** view's, not every view's — pass 1 folds one view
    /// at a time and drops each writer before the next, so a sum would refuse folds a host could
    /// comfortably run. Asserted through the estimate's own arithmetic, since that is where a
    /// reader would look for the rule.
    #[test]
    fn the_estimate_charges_one_permutation_and_one_locator() {
        // 4 B + 4 B per entity, doubled by the safety factor, and no dictionary term.
        assert_eq!(
            memory_estimate(1_000, 1_000, 0, 0, 1_000),
            (4 * 1_000 + 4 * 1_000) * 2,
            "a dictionary of no terms has no images either, whatever the rows"
        );
        // The dictionary term is 8 B per ordinal and independent of entity space.
        assert_eq!(memory_estimate(0, 0, 1_000, 0, 0), 8 * 1_000 * 2);
    }

    /// **Pass 2b is charged one posting, one image, the buffer that image is frozen into and one
    /// scratch, and only where it runs.** The posting and the image are ceilings of a bitset
    /// container per 65 536 entities and per 65 536 rows, and the frozen buffer is charged at the
    /// image's width, so the term moves with entity space and with the view's rows and not with
    /// the dictionary. The scratch is flat.
    ///
    /// Kills the mutation that charges the scratch to a fold with nothing to project, which would
    /// refuse folds on a small host for work the pass skips, and the one that drops either
    /// bitmap or the buffer.
    #[test]
    fn the_estimate_charges_one_posting_one_image_one_buffer_and_one_scratch() {
        assert_eq!(
            memory_estimate(0, 0, 0, 0, 1_000_000),
            0,
            "a dictionary with no term gets no images"
        );
        assert_eq!(
            memory_estimate(0, 0, 1, 0, 0),
            8 * 2,
            "a view with no row gets none either, so only the dictionary term is charged"
        );
        // The scratch is the store's bound over the same row space, not a figure restated here.
        let scratch = |bound: u64, rows: u64| {
            tessera_store::permutation::project_scratch_bound(bound, rows).total()
        };
        // One container of rows and no entity space: one 8 KiB image, the buffer it is frozen
        // into at the same width, and the scratch.
        assert_eq!(
            memory_estimate(0, 0, 1, 0, VALUES_PER_CONTAINER),
            (8 + 2 * BYTES_PER_BITSET_CONTAINER + scratch(0, VALUES_PER_CONTAINER))
                * FOLD_MEMORY_SAFETY_FACTOR
        );
        // One row past it takes a second container, in the image and in the buffer alike, and
        // nothing else moves.
        assert_eq!(
            memory_estimate(0, 0, 1, 0, VALUES_PER_CONTAINER + 1),
            (8 + 4 * BYTES_PER_BITSET_CONTAINER + scratch(0, VALUES_PER_CONTAINER + 1))
                * FOLD_MEMORY_SAFETY_FACTOR
        );
        // The posting is a function of the permutation bound, above the 4 B/entity the mapped
        // array costs.
        assert_eq!(
            memory_estimate(VALUES_PER_CONTAINER, 0, 1, 0, VALUES_PER_CONTAINER),
            (4 * VALUES_PER_CONTAINER
                + 8
                + 3 * BYTES_PER_BITSET_CONTAINER
                + scratch(VALUES_PER_CONTAINER, VALUES_PER_CONTAINER))
                * FOLD_MEMORY_SAFETY_FACTOR
        );
        // The memo's figure at rung 6: ~437 MB of image at 3.5×10⁹ rows, and the frozen buffer
        // beside it at the same width.
        let held = term_image_estimate(1, 0, 3_500_000_000) - scratch(0, 3_500_000_000);
        let image = held / 2;
        assert_eq!(
            held,
            2 * image,
            "the image and its buffer are one width each"
        );
        assert!(
            (430_000_000..=445_000_000).contains(&image),
            "the widest image at 3.5×10⁹ rows is {image} B, against the memo's ~437 MB"
        );
    }

    /// **The artifact pass is charged, and charged per container.** A deployment holding no
    /// artifacts pays nothing for it — the term is what tells a fold it cannot fit, so a term that
    /// fired on a corpus with no layers would refuse folds for a pass that does no work.
    ///
    /// The design point is the cross-check: 10⁷ artifacts of four runs each is 4×10⁷ containers,
    /// which the estimate must price at the +3.5 GB the pass was measured to hold — doubled here by
    /// the safety factor, as every other term is.
    #[test]
    fn the_estimate_charges_the_artifact_pass_per_container() {
        assert_eq!(
            memory_estimate(0, 0, 0, 0, 0),
            0,
            "a deployment with no artifacts is charged nothing for the pass"
        );
        let need = memory_estimate(0, 0, 0, 40_000_000, 0);
        let gb = need as f64 / 1e9 / FOLD_MEMORY_SAFETY_FACTOR as f64;
        assert!(
            (3.2..=3.9).contains(&gb),
            "the pass measured +3.5 GB for 4×10⁷ containers; this prices it at {gb:.1} GB"
        );
    }

    /// **The disc estimate is above the live bytes, not equal to them**, which is the margin spec
    /// §8 asks for. An estimate of exactly the output leaves a device that fills at the last
    /// carried-forward link, and the write path goes down behind it (write-path §1.3).
    ///
    /// Kills the mutation that makes `FOLD_DISC_PERCENT` 100.
    #[test]
    fn the_disc_estimate_carries_a_margin_over_the_bytes_it_would_write() {
        let live = 47u64 << 30;
        let need = disc_estimate(live);
        assert!(
            need > live,
            "an estimate of {need} for {live} live bytes has no margin at all"
        );
        assert_eq!(need, live + live / 2, "150% of live bytes");
        // Saturating rather than wrapping: an absurd manifest must refuse the fold, never wrap to
        // a small number and admit it.
        assert_eq!(disc_estimate(u64::MAX), u64::MAX / 100);
    }

    /// **Every refusal has a name on the operator plane, and the two that carry figures keep
    /// them.** A refused fold advances no counter the `compaction` block publishes, so this
    /// mapping is the whole of what an operator sees; an arm that lost its figures would leave
    /// `insufficient_disc` reading as a bare condition when it is the one refusal whose numbers
    /// say how far the device is from folding.
    ///
    /// The match in `gauge` is exhaustive, so a seventh variant does not compile until it is
    /// named here.
    #[test]
    fn every_refusal_carries_a_name_and_the_two_with_figures_keep_them() {
        assert_eq!(
            NoFold::WalPoisoned.gauge(),
            ("wal_poisoned", None, None),
            "a condition with no figures publishes its name and two nulls"
        );
        assert_eq!(
            NoFold::OverlayDiverged.gauge(),
            ("overlay_diverged", None, None)
        );
        assert_eq!(NoFold::SteppedDown.gauge(), ("stepped_down", None, None));
        assert_eq!(
            NoFold::NothingToFold.gauge(),
            ("nothing_to_fold", None, None)
        );
        assert_eq!(
            NoFold::InsufficientMemory {
                need: 9,
                available: 4
            }
            .gauge(),
            ("insufficient_memory", Some(9), Some(4)),
            "need first, then what the host answered"
        );
        assert_eq!(
            NoFold::InsufficientDisc { need: 71, free: 12 }.gauge(),
            ("insufficient_disc", Some(71), Some(12)),
            "the gap between these two is what the device has to gain before a fold will start"
        );
    }

    /// **One counter per gate, so a standing refusal cannot bury the others.** `due()` re-fires
    /// every tick once the interval floor has passed, and a refusal stamps no
    /// `last_fold_start_unix`, so a gate that stands increments on every tick while anything
    /// refusing between two of them replaces it in `last_refusal`. The per-gate counters are what
    /// an operator reads instead, and they are keyed by `index`.
    ///
    /// Both `index` and `gauge` are exhaustive, so a seventh variant does not compile until it is
    /// named. What neither catches is a variant given an index the array has no entry for, which
    /// is what this covers.
    #[test]
    fn nofold_gates_are_named_and_numbered_once() {
        let arms = [
            NoFold::WalPoisoned,
            NoFold::OverlayDiverged,
            NoFold::SteppedDown,
            NoFold::NothingToFold,
            NoFold::InsufficientMemory {
                need: 9,
                available: 4,
            },
            NoFold::InsufficientDisc { need: 71, free: 12 },
        ];
        assert_eq!(
            arms.len(),
            NoFold::GATES.len(),
            "every gate has a counter and every counter has a gate"
        );
        for (expected, arm) in arms.iter().enumerate() {
            assert_eq!(
                arm.index(),
                expected,
                "the arms are numbered in the order GATES names them"
            );
            assert_eq!(
                arm.gauge().0,
                NoFold::GATES[expected],
                "the name a refusal publishes is the one its counter is keyed by"
            );
        }
    }
}

