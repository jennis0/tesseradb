//! The compaction fold — plan, execute, publish (compaction §1).
//!
//! Three parts, on three different threads, and the split is the design rather than an
//! implementation detail:
//!
//! - [`plan_fold`] runs **on the executor**, against the live generation, and is pure. It names
//!   files and clones one entity-space bitmap (`D₀`). Nothing it decides depends on live state
//!   surviving, because everything about live state is decided again at publication (compaction
//!   §2).
//! - [`execute`] runs **on one dedicated thread**, not the shared rayon pool. A fold's input is the
//!   corpus, and occupying request-serving workers for the minutes-to-hours that takes is the
//!   maintenance schedule leaking into the product that decision 0043 forbids. Sequential also
//!   bounds memory: there are no per-worker buffers to multiply.
//! - Publication is `Executor::publish_fold` (`write.rs`), on the executor again, because the
//!   executor is the process's only publisher (lifecycle §1.3).
//!
//! `flush.rs` / `merge.rs` / `coalesce.rs` establish the plan → context → execute → completed →
//! publish shape and this is its fourth caller. What differs is the thread, and that it publishes a
//! **new prefix** rather than into the live one.
//!
//! # Retirement, and why the obvious definition is fail-open
//!
//! Rule F (write-path §5.4): a deletion's overlay entry leaves `deleted` only at the fold that
//! **executes** it. The tempting definition of "executes" is the plan's own tombstone clone `D₀` —
//! the set the passes ran over — and it serves an acknowledged deletion permanently, by an
//! interleaving nothing in the fold can see:
//!
//! > A flush plans at tick *N* with entity E buffered. Its pool run spans the tick, since nothing
//! > bounds flush and fold overlap. A delete for E is accepted, so `D₀ ∋ E` at tick *N+1*. But the
//! > flush's segment is not in the fold's file list, so the fold removes neither E's row nor its
//! > postings; the flush then publishes both into the old prefix, the fold carries that segment and
//! > its tier forward verbatim, and retirement withdraws the only thing hiding E. E is drawn,
//! > counted and served to every authorised principal.
//!
//! **The identity match cannot catch this** — no fragment is stale, and E genuinely is in the
//! post-fold postings. So retirement is derived from what the publication demonstrably removed,
//! never from what the plan predicted it would (compaction §5):
//!
//! > `executed = { e ∈ D₀ : no carried-forward artefact names e }`, evaluated at publication —
//! > **artefact meaning tier, segment *and* external-id run, not tier alone.**
//!
//! # The safety property is that the carry-forward set is an over-approximation
//!
//! [`executed`] subtracts, so every entity the carry-forward set names is one that does **not**
//! retire. Naming too many is fail-closed — an un-retired tombstone keeps hiding an item that is
//! already gone, costs one overlay entry, and the next fold takes it. Naming too few is the
//! fail-open above. Every judgement in [`CarriedForward`] is therefore made in the direction of
//! naming more, and where a cheap over-approximation is available it is preferred to an exact
//! answer that could be wrong.
//!
//! That is also the answer to the question compaction §5 raises and `DeltaTier` cannot answer —
//! *"how do you ask a tier whether it contains an entity"*. You do not. A flush publishes a
//! segment, a tier, a run and a locator extent **together, over one contiguous entity range**, and
//! merge and coalesce are suspended for the fold's duration (compaction §1), so the segment's own
//! `entity_lo..=entity_hi` covers every entity the other three can name. Taking the range covers
//! the tier without a primitive that does not exist, and it covers the case a tier-based test
//! misses outright: **a zero-term item produces no `(term, entity)` pair at all** — nothing on the
//! ingest path refuses one, and the reference plugin drops empty descriptors — so no tier names it
//! while its row and its external-id binding are both carried forward. Retiring it would 409 a
//! lawful re-ingest of its external id (decision 0047), which is why compaction §12's obligation 2b
//! names it as the shape a tier test misses.
//!
//! # Passes 1–3 execute over `D₀`, and this module holds both sets without confusing them
//!
//! [`FoldPlan::tombstones`] is `D₀` and is what [`execute`] hands to every pass. [`executed`] is
//! computed hours later, at publication, from what the publication carried forward. The
//! containment `executed ⊆ D₀` is the safety property: everything that retires demonstrably lost
//! its row and its postings, and an entity in `D₀ \ executed` has lost both as well and merely
//! keeps its overlay entry for another round. The passes themselves live in `tessera-store` and
//! `tessera-authz`, crates from which [`executed`] is not reachable, so the confusion is not
//! expressible there at all — only here, and only by handing the wrong field to a pass.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use croaring::Bitmap;

use tessera_authz::{sweep_term_postings, DeltaTier, PostingsReader, PostingsSpool};
use tessera_spatial::tiler::ScalarType;
use tessera_store::manifest::{FileDigest, SegmentDescriptor};
use tessera_store::{
    fold_external_id_runs, fold_row_space, FoldRowSpaceSpec, FoldSegmentInput, PairsParquetWriter,
};
use tessera_types::IdentityKey;

use crate::Generation;

/// Every entity a carried-forward artefact names — the operand [`executed`] subtracts from `D₀`.
///
/// Built at **publication**, from the live partition manifest minus what the fold consumed, and
/// never at plan time: computing it early means predicting which flushes will land during the
/// fold's flight, which is exactly the prediction compaction §5's rule replaced.
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

    /// A segment the fold did not consume: every entity in its range is carried forward, and so is
    /// every entity its flush's tier and run name (see the module doc).
    ///
    /// **Its whole declared range, not the entities it demonstrably holds.** The range is what the
    /// manifest publishes and what a reader addresses it by; enumerating the `tessera_id` column to
    /// narrow it would cost a mapped read per carried segment to arrive at a *smaller* set, which
    /// is the fail-open direction.
    pub(crate) fn add_segment(&mut self, descriptor: &SegmentDescriptor) {
        self.add_range(descriptor.entity_lo, descriptor.entity_hi);
    }

    /// A locator extent the fold did not consume — the external-id half of compaction §5's
    /// "tier, segment *and* run", and the one that names a zero-term item's binding.
    pub(crate) fn add_locator_extent(&mut self, extent: &tessera_store::manifest::LocatorExtent) {
        self.add_range(extent.entity_lo, extent.entity_hi);
    }

    /// `entity_lo ..= entity_hi`, inclusive.
    ///
    /// Entity ids are capped at `u32::MAX` by the I9 allocator (contracts §2.6 r6), which is what
    /// lets the deny sets be Roaring bitmaps at all. A range that does not fit is **not** silently
    /// truncated: a truncated range names fewer entities, which is the fail-open direction, so the
    /// out-of-range part is clamped *outward* to the representable maximum rather than dropped.
    fn add_range(&mut self, entity_lo: u64, entity_hi: u64) {
        if entity_lo > entity_hi {
            return;
        }
        let lo = u32::try_from(entity_lo).unwrap_or(u32::MAX);
        let hi = u32::try_from(entity_hi).unwrap_or(u32::MAX);
        self.entities.add_range(lo..=hi);
    }

    /// How many entities are carried forward — a diagnostic for the publication's log line, so an
    /// operator can see a fold that retired nothing because everything was carried.
    pub(crate) fn len(&self) -> u64 {
        self.entities.cardinality()
    }
}

/// The deletions whose overlay entries retire in this fold's own swap: `D₀` minus everything a
/// carried-forward artefact names.
///
/// **`executed ⊆ D₀` is the safety property**, and it holds by construction here because this
/// function only ever subtracts. Everything that retires demonstrably lost its row and its
/// postings to the fold's own passes; an entity in `D₀ \ executed` has lost both as well and merely
/// keeps its overlay entry for another round, which is fail-closed and costs one bitmap entry.
///
/// **Passes 1–3 execute over `D₀`, never over this.** The containment is one-directional on
/// purpose: the passes ran hours before this is computable, and making them use it would require
/// computing it at plan time — predicting the carry-forward set, the fail-open compaction §5's rule
/// replaced. That is enforced by construction rather than by comment: the passes take their
/// tombstone set as a parameter in `tessera-store` and `tessera-authz`, crates from which this
/// function is not reachable.
pub(crate) fn executed(d0: &Bitmap, carried: &CarriedForward) -> Bitmap {
    let mut executed = d0.clone();
    executed.andnot_inplace(&carried.entities);
    executed
}

// =================================================================================================
// The schedule
// =================================================================================================

/// When a fold is dispatched without anyone asking for one (compaction §9, decision 0056).
///
/// **Three routes over two gauges, and the shape is a floor and a ceiling rather than a split.**
///
/// Segment count is a *read* cost — a tile resolves to one contiguous range per live segment, so a
/// viewport pays a binary search and a `range_cardinality` per segment per tile — and it is
/// deferrable *up to a point*, not indefinitely: at ~152 segments decision 0049 measured ~73 ms on
/// a 300-tile viewport against a 135–164 ms baseline, which is a ~50% regression that no viewer
/// should carry until midnight. So it gets two thresholds: [`window_min_segments`], the low one,
/// which fires only inside the daily window, and [`max_segments`], the high one, which fires at any
/// hour.
///
/// [`window_min_segments`]: Self::window_min_segments
/// [`max_segments`]: Self::max_segments
///
/// Retirable depth has one threshold and no window at all, because the cost it measures is
/// unbounded rather than merely growing: the overlay grows monotonically under deletion churn,
/// every deny acceptance clones it, and depth is a term in I1's composition cost.
// `PartialEq` without `Eq`: two of the thresholds are ratios, and `f64` has no total equality.
// Nothing compares two schedules for identity — the derive exists so a test can assert what a
// config file parsed to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionSchedule {
    /// The floor under every route — compaction §9's `compaction_min_interval_secs`. A fold within
    /// this of the last completed one is never dispatched, whatever a gauge says.
    pub min_interval_secs: u64,
    /// Seconds past **UTC** midnight at which the daily window opens; `None` switches the windowed
    /// route off entirely.
    ///
    /// UTC rather than local time, and that is a correctness argument: a local-time window shifts
    /// by an hour twice a year, and on the transition day it fires either twice or not at all —
    /// against a 24 h floor that would then block or admit the second firing depending on which way
    /// the clock moved. A deployment wanting local midnight sets the offset once, and it stays put.
    pub window_start_secs: Option<u32>,
    /// How long the window stays open. **This is what makes the start time a start time**: without
    /// it a node down at 00:00 and started at 09:00 would fold at 09:00, which is the one hour the
    /// operator configured it away from.
    pub window_secs: u32,
    /// Live segments in any one slice at or above which a fold is worth running *inside the
    /// window*. Below it the window passes and nothing happens.
    ///
    /// **The low threshold of two.** It answers "is there enough here to be worth a quiet-hours
    /// fold"; [`Self::max_segments`] answers "is this bad enough that it cannot wait".
    pub window_min_segments: usize,
    /// Live segments in any one slice at or above which a fold is dispatched **at any hour**;
    /// `None` switches this route off.
    ///
    /// **Deferring segment growth has a limit, and this is it.** The windowed threshold exists
    /// because segment count degrades a viewport gradually and gradual costs can wait for a quiet
    /// hour. That argument runs out: a deployment ingesting fast enough to add segments through the
    /// night reaches a count every viewport pays for long before the next window, and telling it to
    /// wait is choosing a worse hour for the read path over a worse hour for the write path.
    ///
    /// Must sit strictly above [`Self::window_min_segments`] where both are armed, or the window is
    /// unreachable — `tessera-server` refuses that configuration rather than shipping a key that
    /// cannot fire.
    pub max_segments: Option<usize>,
    /// Retirable deletions at or above which a fold is dispatched at any hour; `None` switches the
    /// unwindowed route off. Defaults to `overlay_soft_limit`, which is the action compaction §9
    /// says that alarm was always supposed to prompt.
    pub after_deletions: Option<u64>,
    /// Tombstoned rows as a fraction of the bundle's live rows, at or above which a fold is
    /// dispatched at any hour; `None` switches the route off.
    ///
    /// **A different question from [`Self::after_deletions`], over the same numerator.** That one
    /// is an absolute: an overlay of half a million entries costs every composition, whatever the
    /// corpus size. This is a ratio, and it is what a *viewport* pays — rows that exist, are
    /// scanned, and no viewer may see. A 50,000-row deployment with 10,000 deletions is 20% dead
    /// and nowhere near the absolute threshold; a 10⁹-row one crosses the absolute long before the
    /// ratio moves. Neither subsumes the other, which is why compaction §9 makes them separate
    /// gauges rather than one blended score.
    pub tombstoned_rows_fraction: Option<f64>,
    /// **Dead** bytes over named bytes — `(on_disc − named) / named` under the live prefix — at or
    /// above which a fold is dispatched at any hour; `None` switches the route off.
    ///
    /// **A ratio of dead to live, not of total to live**, which is what compaction §9's default of
    /// 1.0 means: *paying double for storage*, i.e. `on_disc = 2 × named`. Written as
    /// `on_disc / named` instead, a threshold of 1.0 is satisfied by every bundle ever built — on
    /// disc always exceeds named, if only by the manifests' own bytes, which nothing can name.
    ///
    /// **The only route that covers compaction §0's *reclamation* obligation.** A deployment with
    /// heavy merge churn and few deletions accumulates consumed segments and superseded tiers that
    /// no gauge above can see: its segment count is bounded (merge is doing its job), its overlay
    /// is shallow, and it is paying for two or three copies of its corpus. The measured
    /// no-compaction steady state is 2.0–2.6× (`docs/evidence/memos/2026-08-05-write-path-at-scale.md`).
    ///
    /// **This is the expensive gauge**, and the only one that is not a field read: it needs a walk
    /// of the live prefix. [`due`] therefore takes it as a closure and calls it last, after every
    /// cheaper route has declined.
    pub dead_bytes_ratio: Option<f64>,
}

impl CompactionSchedule {
    /// Neither route armed — what an embedder gets by default, and what every test that is not
    /// about the schedule uses.
    ///
    /// **Off rather than on**, because `Engine` is a library type with no configuration file behind
    /// it: a fold started by a default nobody chose is minutes to hours of IO an embedder did not
    /// ask for. `tessera-server` is where the defaults compaction §9 states are applied, because it
    /// is where an operator can see and change them.
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

/// Why the schedule dispatched a fold — carried into the log line, so an operator can tell a
/// nightly tidy from a deployment that is drowning in un-retired deletions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FoldTrigger {
    /// Inside the daily window, with a slice over `window_min_segments`.
    Window,
    /// A slice reached `max_segments`, at whatever hour — segment growth past the point where
    /// deferring it is cheaper than paying it.
    SegmentCount,
    /// `|deleted|` reached `after_deletions`, at whatever hour.
    RetirableDepth,
    /// Tombstoned rows passed `tombstoned_rows_fraction` of the bundle's live rows.
    TombstonedRows,
    /// On-disc bytes passed `dead_bytes_ratio` × the bytes the manifests name.
    DeadBytes,
}

/// What the schedule reads at the tick that reads it — the three cheap gauges.
///
/// A struct rather than three more positional parameters, because [`due`] is the one place the
/// whole rule is stated and a seven-argument call site is a place to get an argument order wrong.
/// The fourth gauge is not here: it is a directory walk, and it arrives as a closure.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Gauges {
    /// The largest live segment count across the partition's slices.
    pub(crate) live_segments: usize,
    /// `|deleted|` — deletions alone, never the union with `suppressed` (see [`due`]).
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

/// Whether the schedule calls for a fold now.
///
/// **Pure, so the whole trigger is testable without a clock or an executor** — which matters more
/// here than usual, because the alternative is a test that waits for midnight. `now_unix` and
/// `last_fold_unix` are seconds; `live_segments` is the largest live segment count across slices,
/// since compaction §9's gauge is per (partition, slice) and any slice over the threshold is worth
/// a fold.
///
/// # The floor is what stops a persistently-discarding fold, so it must be stamped by a discard
///
/// `last_fold_unix` is *"when the last attempt ended"*, not *"when the last fold succeeded"* —
/// see `Executor::last_fold_attempt_unix`. Several discard causes are persistent and leave the
/// gauge that dispatched the fold exactly where it was, so a success-only stamp turns one bad
/// configuration value into a loop that rewrites the corpus at every tick and leaves an unreclaimed
/// prefix behind each time. The floor is the only rate limit on this path.
///
/// # It is process-local, and for the *success* case the work gates make that harmless
///
/// A node that restarts inside its own window has no record of the fold it just finished. It
/// dispatches nothing anyway: a fold leaves one segment per partition-slice and an overlay with the
/// executed deletions gone, so both gauges are re-read against the bundle the fold itself produced
/// and neither is over. A restart after a *discard* does lose the back-off — the orphan the discard
/// left is not a gauge anything reads — so the loop above is bounded by the floor within one
/// process's life and by nothing across restarts.
pub(crate) fn due(
    schedule: &CompactionSchedule,
    now_unix: u64,
    last_fold_unix: Option<u64>,
    gauges: Gauges,
    dead_bytes: impl FnOnce() -> Option<DeadBytes>,
) -> Option<FoldTrigger> {
    // The floor, under every route. `saturating_sub` rather than a comparison because a clock that
    // steps backwards must read as "not yet", never as a very large elapsed time.
    if let Some(last) = last_fold_unix {
        if now_unix.saturating_sub(last) < schedule.min_interval_secs {
            return None;
        }
    }

    // **The unwindowed routes first**, because they are the urgent ones: a deployment over one of
    // them *inside* its own window should log the reason that will still be true tomorrow, not the
    // hour it happened to be.
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
    // **A ratio needs a denominator**: a bundle with no rows has no fraction, and reading one as
    // "infinitely dead" would dispatch a fold at every tick over an empty corpus.
    if let Some(threshold) = schedule.tombstoned_rows_fraction {
        if gauges.live_rows > 0
            && gauges.retirable_deletions as f64 / gauges.live_rows as f64 >= threshold
        {
            return Some(FoldTrigger::TombstonedRows);
        }
    }

    // The window, which is the only route that has one.
    if let Some(start) = schedule.window_start_secs {
        if gauges.live_segments >= schedule.window_min_segments
            && schedule.window_min_segments > 0
            && in_window(now_unix, start, schedule.window_secs)
        {
            return Some(FoldTrigger::Window);
        }
    }

    // **Last, and the closure is why.** Every gauge above is a field read; this one is a walk of
    // the live prefix, so a tick pays for it only when nothing cheaper has already decided. A
    // deployment with the route switched off never calls it at all.
    let threshold = schedule.dead_bytes_ratio?;
    let measured = dead_bytes()?;
    let dead = measured.on_disc.saturating_sub(measured.named);
    (measured.named > 0 && dead as f64 / measured.named as f64 >= threshold)
        .then_some(FoldTrigger::DeadBytes)
}

/// Whether `now_unix` falls in the daily window `[start, start + width)` past UTC midnight.
///
/// **Wraps past midnight**, which a window starting at 23:00 needs and which is the only reason
/// this is a function rather than two comparisons.
fn in_window(now_unix: u64, start_secs: u32, window_secs: u32) -> bool {
    const DAY: u64 = 86_400;
    // A width at or past a whole day is always open — stated rather than left to the arithmetic,
    // which would otherwise compare a `since` against a value it can never reach.
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

/// One live segment of one slice, as the plan names it.
///
/// `dir` is **prefix-relative**, so the plan is a list of names rather than of resolved paths — the
/// same form the manifest's `files` map takes, and the form [`execute`] joins onto whichever prefix
/// directory it is reading.
pub(crate) struct PlannedSegment {
    pub(crate) seg_id: String,
    pub(crate) dir: String,
}

/// One slice's half of a fold plan.
pub(crate) struct FoldSlicePlan {
    pub(crate) slice: String,
    /// Every live segment of this slice at the snapshot — the base plus every extent. Pass 1 merges
    /// them all; order does not matter to it, since the merge is driven by a heap over each
    /// cursor's `(morton, tessera_id)` key.
    pub(crate) segments: Vec<PlannedSegment>,
    /// The bound for this slice's new `permutation.bin`: **one past the highest entity the
    /// snapshot's row space covers**, and emphatically not the live `entity_id_high_water`.
    ///
    /// `RowSpace::with_extent` refuses an extent that begins below the base permutation's bound, so
    /// a bound taken from the allocator's high-water would refuse every carried-forward flush
    /// segment whose entities were *buffered* at the snapshot — and it would refuse them at
    /// `open_written_prefix`, which runs after `CURRENT` has already flipped. Taken from the row
    /// space, the floor is exactly what every post-snapshot publication had to clear to become live
    /// in the first place.
    pub(crate) permutation_bound: u64,
}

/// One fold's immutable plan: the files it consumes, and `D₀`.
///
/// **Pure, and it holds nothing open.** Every input is an immutable file named by path, so the plan
/// survives arbitrary churn on the executor while the fold runs — what it does *not* survive is a
/// publication that consumed one of those files, which is what the rebase check at publication
/// (compaction §4 step 1) is for.
pub(crate) struct FoldPlan {
    pub(crate) partition: String,
    pub(crate) slices: Vec<FoldSlicePlan>,
    /// Every live delta tier, prefix-relative, in the live manifest's order — all consumed.
    pub(crate) tiers: Vec<String>,
    /// Every live external-id run, prefix-relative, oldest first — all consumed by pass 3.
    pub(crate) runs: Vec<String>,
    /// Every live locator extent's path — consumed with the runs they index.
    pub(crate) locator_extents: Vec<String>,
    /// `D₀` — the plan's tombstone clone. Handed to passes 1–3 whole; never [`executed`].
    pub(crate) tombstones: Bitmap,
    /// One past the highest entity **with a row anywhere in this partition** at the snapshot — the
    /// span pass 3's `ext-locator.u32` covers, and therefore the value the new `MANIFEST.json`
    /// carries as `entity_id_high_water` (which is what the sidecar reads it as; see
    /// `ExternalIdSidecar::deferred_from_manifest`).
    ///
    /// **Not the live high-water**, for compaction §3 pass 3's reason: the base locator has
    /// absolute priority below its own length, so a full-length locator swallows every
    /// post-snapshot entity and answers "this item has no external id" for items that have one.
    pub(crate) entity_bound: u64,
    /// The dictionary length the term sweep emits records for. Every ordinal below it gets a
    /// record, empty or not.
    pub(crate) dict_len: u32,
    pub(crate) small_term_threshold: u32,
    /// The prefix this plan was taken against. A publication into a different one is discarded.
    pub(crate) prefix: String,
}

/// Why a tick planned no fold. Each is a distinct operator-facing condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoFold {
    /// **The WAL is poisoned.** A fold writes `tombstones` and `deny` into its new side-manifest
    /// from an overlay holding dispositions no durable record backs, and it rotates the WAL
    /// immediately afterwards — the same two reasons `plan_flush` and `publish_overlay_state`
    /// refuse.
    WalPoisoned,
    /// **The in-memory overlay has diverged from the durable WAL** (write-path §7.2).
    OverlayDiverged,
    /// **A partition is serving a stepped-down side-manifest.** A fold assembled from older served
    /// state would fold the stepped-past segment out of existence rather than merely shadow it.
    SteppedDown,
    /// No partition, or a partition with no slice holding a segment. Nothing to fold.
    NothingToFold,
    /// The bundle declares a scalar column this build cannot write, so pass 1 would emit a segment
    /// the reader refuses. The same fail-closed answer a flush gives.
    UnwritableScalarSchema,
    /// **The estimated peak memory is above what the host has available** (compaction §3). Both
    /// figures in bytes.
    InsufficientMemory { need: u64, available: u64 },
    /// **The estimated output is above the free space on the device** (compaction §8). Both figures
    /// in bytes.
    InsufficientDisc { need: u64, free: u64 },
}

/// What the fold's two pre-flight refusals compare against.
///
/// **Measured by the caller, so [`plan_fold`] stays pure.** Free space and available memory are
/// syscalls against the host, not properties of the generation, and folding them into the planner
/// would make every selection test need a filesystem. `None` means *unknowable* — on a host whose
/// procfs or `statvfs` does not answer, the corresponding pre-flight simply does not run.
///
/// **Not refusing on an unknown is deliberate**, and it is `tessera-build`'s precedent
/// (`available_disk`: *"the disk pre-flight then simply does not run, rather than refusing builds
/// on a guess"*). A refusal derived from a figure nobody could read is a deployment that silently
/// never compacts, which is a slower version of the failure these checks exist to prevent.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FoldResources {
    pub(crate) available_memory: Option<u64>,
    pub(crate) free_disc: Option<u64>,
}

/// The multiplier on [`memory_estimate`]'s computable terms, standing in for the one term of
/// compaction §3's budget that cannot be computed without reading the postings.
///
/// §3's table has four terms. Three are exact functions of quantities the plan already holds — two
/// 4 B/entity mapped arrays and an 8 B/ordinal spool offsets buffer — and the fourth, *the widest
/// term's encode*, is `corpus × that term's coverage`, which needs a postings scan the planner has
/// no reason to do. Its measured magnitude is what makes a factor defensible rather than arbitrary:
/// 125.12 MB per 25% grant (`probes/results.md` §4.2), which §3 extrapolates to ~375–500 MB at
/// 10⁹ — against an 8 GB entity-space term at that size. Doubling the computable terms leaves an
/// ~8 GB allowance for a ~0.5 GB unknown.
///
/// **Assumed, not measured**, and probe P1 is what would calibrate it: P1 measures the whole peak
/// against a fixture whose dictionary is two terms, so it constrains the entity-space terms and
/// says nothing about this one.
const FOLD_MEMORY_SAFETY_FACTOR: u64 = 2;

/// The fold's peak **un-reclaimable** memory in bytes, from quantities the plan already knows.
///
/// **Un-reclaimable is the whole of what this estimates, and it is not what a fold's RSS reads.**
/// Probe P1 measured peak resident at 0.93–0.99× the bundle's live bytes, ~92% of it file-backed:
/// a fold maps its inputs and its outputs, so nearly all of that is page cache the kernel drops the
/// moment anything wants the memory. `MemAvailable` already counts reclaimable cache as available,
/// so charging the fold for it would refuse every fold on a host whose bundle exceeds RAM — which
/// is every host this design is for. What cannot be given back is the anonymous half plus the dirty
/// pages of the two arrays the fold *writes* through a mapping, and those are the terms here.
///
/// | term | basis |
/// |---|---|
/// | 4 B × permutation bound | `permutation.bin`, written through a mapping (§3 pass 1) |
/// | 4 B × entity bound | `ext-locator.u32`, same (§3 pass 3) |
/// | 8 B × dictionary length | `PostingsSpool`'s offsets buffer (§3's table: ~0.94 GB at 1.17×10⁸) |
///
/// The permutation term is the **maximum** across slices rather than their sum: pass 1 folds one
/// slice at a time and drops each slice's writer before the next, so the peak is one of them.
///
/// *(§3's first draft called the two mapped arrays free — page cache rather than RSS. r1 corrected
/// it: a dirty shared file mapping is resident and cgroup-charged until writeback. They are charged
/// here on r1's reading, which is also the conservative one.)*
pub(crate) fn memory_estimate(permutation_bound: u64, entity_bound: u64, dict_len: u64) -> u64 {
    let terms = 4u64
        .saturating_mul(permutation_bound)
        .saturating_add(4u64.saturating_mul(entity_bound))
        .saturating_add(8u64.saturating_mul(dict_len));
    terms.saturating_mul(FOLD_MEMORY_SAFETY_FACTOR)
}

/// The free space a fold needs, as a percentage of the bytes its inputs' manifests name.
///
/// **150%, and the 50% is the margin compaction §8 asks for rather than a second estimate.** The
/// fold's own output is at most the live bytes and usually less — it drops every folded deletion
/// and coalesces every axis — and the carried-forward files are hard links, which cost directory
/// entries and no bytes. What the margin covers is what lands *beside* the new prefix during a
/// flight of minutes to hours: the flushes that keep publishing into the old prefix (§7), the WAL
/// they append to, and the fragment cache.
///
/// **Assumed.** The output-size half is bounded by construction; the margin is a judgement, and
/// what would calibrate it is a fold run against a deployment's own ingest rate — the product of a
/// rate and a duration, neither of which the planner knows.
const FOLD_DISC_PERCENT: u64 = 150;

/// The free bytes a fold needs before it starts, from the bytes its inputs' manifests name.
pub(crate) fn disc_estimate(live_bytes: u64) -> u64 {
    live_bytes
        .saturating_mul(FOLD_DISC_PERCENT)
        .saturating_div(100)
}

/// Plan a fold of `generation`'s single partition.
///
/// Pure, so the selection is testable without an executor: it reads the generation, the two
/// executor-health flags and the host figures its caller measured, and nothing else. See
/// [`FoldResources`] for why the last of those is a parameter rather than two syscalls here.
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
    if crate::write::scalar_schema_of(&generation.bundle.manifest).is_none() {
        return Err(NoFold::UnwritableScalarSchema);
    }

    let (partition, partition_data) = generation
        .bundle
        .partitions
        .iter()
        .next()
        .ok_or(NoFold::NothingToFold)?;
    let manifest = &partition_data.manifest;

    let mut slices: Vec<FoldSlicePlan> = Vec::new();
    // Sorted, so a plan is a function of the generation and not of a `HashMap`'s iteration order —
    // the new manifest's segment list follows this order, and a manifest whose bytes depend on
    // hashing is a bundle identity that depends on hashing.
    let mut slice_ids: Vec<&String> = partition_data.slices.keys().collect();
    slice_ids.sort_unstable();
    for slice in slice_ids {
        let slice_data = &partition_data.slices[slice];
        if slice_data.segments.is_empty() {
            continue;
        }
        let row_space = &slice_data.row_space;
        let permutation_bound = row_space
            .extents()
            .last()
            .map_or(row_space.base().bound(), |extent| extent.entity_hi + 1);
        slices.push(FoldSlicePlan {
            slice: slice.clone(),
            segments: slice_data
                .segments
                .iter()
                .map(|segment| PlannedSegment {
                    dir: format!(
                        "partitions/{partition}/slices/{slice}/segments/{}",
                        segment.seg_id
                    ),
                    seg_id: segment.seg_id.clone(),
                })
                .collect(),
            permutation_bound,
        });
    }
    if slices.is_empty() {
        return Err(NoFold::NothingToFold);
    }

    // The partition-wide locator span. Every post-snapshot locator extent begins above this, for
    // the same reason every post-snapshot segment does — `with_extent`'s floor — and publication
    // checks that rather than assuming it.
    let entity_bound = slices
        .iter()
        .map(|slice| slice.permutation_bound)
        .max()
        .unwrap_or(0);

    // ---- the two pre-flight refusals (compaction §3, §8) ---------------------------------------
    //
    // **Last, because both need the plan's own quantities**, and cheap enough that planning first
    // and refusing costs nothing: everything above is a clone of manifest lists.
    //
    // **What they protect against is not a slow fold but a dead node.** `tessera-build` exists
    // because the in-memory build was OOM-killed at 10⁹ on a 47 GiB box, and the fold puts the same
    // shape of work inside the *serving* binary — so an unchecked fold on a loaded host takes the
    // node with it, and a fold that fills the device takes the write path down behind a 500
    // (write-path §1.3). Neither failure is recoverable by the thing that caused it.
    let dict_len = generation.dict.len();
    if let Some(available) = resources.available_memory {
        let need = memory_estimate(
            slices
                .iter()
                .map(|slice| slice.permutation_bound)
                .max()
                .unwrap_or(0),
            entity_bound,
            u64::from(dict_len),
        );
        if need > available {
            return Err(NoFold::InsufficientMemory { need, available });
        }
    }
    if let Some(free) = resources.free_disc {
        // The bytes the fold reads, which is also the ceiling on the bytes it writes. **Two maps,
        // and taking only one is the mistake that reads as a catastrophe**: the build's artefacts
        // are digested in the bundle `MANIFEST.json` and everything the write path produced is in
        // the partition's side-manifest, so a sum over one of them alone is short by the other.
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
        // `deleted` is already a `u32`-domain Roaring bitmap on the overlay; the round trip through
        // `deleted_entities` is what keeps this module off `Overlay`'s private representation.
        tombstones.add(entity as u32);
    }

    Ok(FoldPlan {
        partition: partition.clone(),
        slices,
        tiers: manifest.deltas.clone(),
        runs: manifest.external_id_runs.clone(),
        locator_extents: manifest
            .locator_extents
            .iter()
            .map(|extent| extent.path.clone())
            .collect(),
        tombstones,
        entity_bound,
        dict_len,
        small_term_threshold: generation.bundle.manifest.small_term_threshold,
        prefix: generation.prefix.clone(),
    })
}

// =================================================================================================
// Execution — five streaming passes into a new prefix
// =================================================================================================

/// Everything [`execute`] needs beyond its plan.
///
/// Taken from the generation on the executor thread and then **immutable**, exactly as a flush's
/// context is. The two `Arc`s are the live readers rather than reopened files: they are the same
/// mappings every request is already serving from, and reopening them would double the fold's
/// resident cost for nothing.
pub(crate) struct FoldContext {
    /// The prefix the fold **reads** — the live one when the plan was taken.
    pub(crate) from_prefix_dir: PathBuf,
    /// The prefix the fold **writes**, which no `CURRENT` names until publication.
    pub(crate) to_prefix: String,
    pub(crate) to_prefix_dir: PathBuf,
    pub(crate) identity_key: IdentityKey,
    pub(crate) shard_id: u32,
    pub(crate) scalar_schema: Vec<(String, ScalarType)>,
    /// The new base segment's id, one per slice — never reused, so a discarded fold's orphans can
    /// never be mistaken for a later one's output (contracts §2.1).
    pub(crate) seg_id: String,
    /// The live base postings and the live tiers — pass 2's inputs.
    pub(crate) base_postings: Arc<PostingsReader>,
    pub(crate) tiers: Vec<Arc<DeltaTier>>,
}

/// The process's resident set as `/proc/self/status` reports it, in bytes: total, anonymous,
/// file-backed. Zero for a field procfs does not offer, which is also what a non-Linux host gets.
///
/// **Three numbers rather than one, because §3's budget is a claim about which of them grows.**
/// Two of the budget's terms — `permutation.bin` and `ext-locator.u32` — are written *through a
/// mapping*, so they land in `RssFile` and are reclaimable under pressure once written back; the
/// spool buffers and the term encodes are `RssAnon` and are not. A fold whose total climbs because
/// its inputs became resident is behaving as designed. One whose *anonymous* half climbs with the
/// corpus has a term nobody budgeted, and only the split tells the two apart.
fn resident_set() -> (u64, u64, u64) {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return (0, 0, 0);
    };
    let field = |name: &str| -> u64 {
        status
            .lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map(|kib| kib * 1024)
            .unwrap_or(0)
    };
    (field("VmRSS:"), field("RssAnon:"), field("RssFile:"))
}

/// What one pass cost: its wall clock, and the process's resident set at the moment it ended.
///
/// **A staircase sampled at pass boundaries, and deliberately not a peak.** A true peak needs
/// either a sampling thread or a `clear_refs` reset of the process's `VmHWM`, and a serving binary
/// may do neither: the first spends a thread for the fold's whole duration, and the second silently
/// clobbers a process-wide statistic anything else might be reading. What this gives instead is
/// *attribution* — which pass the resident set climbed during — which is the question a memory
/// budget is calibrated by. Probe **P1** takes the true peak, from outside, with the reset.
#[derive(Debug, Clone, Copy)]
pub struct PassCost {
    pub pass: &'static str,
    pub elapsed: std::time::Duration,
    /// Total and anonymous resident bytes at the end of the pass.
    pub rss: u64,
    pub anon: u64,
}

/// A fold whose files are durable under a prefix nothing yet names.
pub(crate) struct CompletedFold {
    pub(crate) plan: FoldPlan,
    pub(crate) prefix: String,
    /// The new base segment of each slice, in the plan's slice order.
    pub(crate) segments: Vec<SegmentDescriptor>,
    /// Every file the fold itself wrote, prefix-relative, with its digest. Publication adds the
    /// carried-forward files' digests and writes the result as `MANIFEST.json`.
    pub(crate) files: BTreeMap<String, FileDigest>,
    /// The new run 0's prefix-relative path — `None` when the deployment holds no external ids at
    /// all, in which case pass 3 wrote nothing and the new manifest lists no runs.
    pub(crate) external_id_run: Option<String>,
    /// The largest new base segment's `columns.arrow + morton.u32` bytes — compaction §4 step 3's
    /// operand, computed here because these are the files that were just written.
    pub(crate) base_segment_bytes: u64,
    /// One [`PassCost`] per pass, in execution order — the fold's own account of what it spent,
    /// logged at publication and reduced to two gauges on `/control/status`.
    pub(crate) cost: Vec<PassCost>,
}

/// Why a fold produced nothing. **Every failure discards the fold** (compaction §3, pass 5): its
/// files are orphans under a prefix `CURRENT` does not name, and the next trigger re-plans from
/// scratch. There is no resume, deliberately — a resumable fold needs its own durable progress
/// record, and re-doing a maintenance pass is cheaper than a second thing to get wrong.
#[derive(Debug)]
pub(crate) struct FoldFailed(pub(crate) String);

impl std::fmt::Display for FoldFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Run the fold's five passes into `ctx.to_prefix_dir`. **On one dedicated thread** — see the
/// module doc.
///
/// # Why the digests are re-read rather than computed as the bytes are written
///
/// Compaction §3 pass 5 asks for digests taken "as each file is written rather than re-reading
/// it". None of the five writers this pass composes can offer that: `SegmentWriter`,
/// `RunWriter`, `LocatorWriter` and `PostingsSpool` all *assemble* their output at `finish` from a
/// spool they map back, and `PairsParquetWriter` hands its bytes to an Arrow writer that owns the
/// file. Hashing at the source would mean a hashing wrapper inside each of them, against writers
/// whose byte-identity with the build's is itself under test. So the digests are taken by reading
/// each written file back — which is exactly what `tessera-build` does at every scale it has been
/// measured at, and what its own §8 calls a stage. Stated here rather than left as a silent
/// divergence from the design, and corrected in that document.
pub(crate) fn execute(plan: FoldPlan, ctx: FoldContext) -> Result<CompletedFold, FoldFailed> {
    let failed = |what: &str, e: &dyn std::fmt::Display| FoldFailed(format!("{what}: {e}"));

    let partition_dir = ctx.to_prefix_dir.join("partitions").join(&plan.partition);
    let terms_dir = partition_dir.join("terms");
    let entities_dir = partition_dir.join("entities");
    for dir in [&terms_dir, &entities_dir] {
        std::fs::create_dir_all(dir).map_err(|e| failed("creating the new prefix", &e))?;
    }

    // Every file this fold writes, prefix-relative and resolved, in write order — pass 5 digests
    // exactly this list, so a file added to a pass without being recorded here is a file the new
    // `MANIFEST.json` does not name and `ensure_verified` refuses at the first read.
    let mut written: Vec<(String, PathBuf)> = Vec::new();

    // The staircase — see [`PassCost`]. `entry` is the zero every later reading is read against,
    // and it is taken here rather than by the caller so that what it excludes is exactly the
    // dispatch work (the plan, the tombstone clone) and nothing else.
    let mut cost: Vec<PassCost> = Vec::with_capacity(6);
    let mut mark = std::time::Instant::now();
    let record = |pass: &'static str, cost: &mut Vec<PassCost>, mark: &mut std::time::Instant| {
        let (rss, anon, _) = resident_set();
        cost.push(PassCost {
            pass,
            elapsed: mark.elapsed(),
            rss,
            anon,
        });
        *mark = std::time::Instant::now();
    };
    record("entry", &mut cost, &mut mark);

    // ---- pass 1 — row space -------------------------------------------------------------------
    //
    // One new segment per (partition, slice), and one `permutation.bin` beside it. Rows whose
    // entity is in `D₀` are dropped, which shifts the row id of every row after them — the whole
    // reason compaction §6 exists.
    let mut segments: Vec<SegmentDescriptor> = Vec::with_capacity(plan.slices.len());
    let mut base_segment_bytes = 0u64;
    for slice in &plan.slices {
        let slice_rel = format!("partitions/{}/slices/{}", plan.partition, slice.slice);
        let slice_dir = ctx.to_prefix_dir.join(&slice_rel);
        std::fs::create_dir_all(&slice_dir).map_err(|e| failed("creating the slice", &e))?;
        let segment_rel = format!("{slice_rel}/segments/{}", ctx.seg_id);
        let segment_dir = ctx.to_prefix_dir.join(&segment_rel);
        let permutation_rel = format!("{slice_rel}/permutation.bin");
        let permutation_path = ctx.to_prefix_dir.join(&permutation_rel);

        let inputs: Vec<FoldSegmentInput> = slice
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
            FoldRowSpaceSpec {
                inputs: &inputs,
                identity_key: &ctx.identity_key,
                shard_id: ctx.shard_id,
                scalar_schema: &ctx.scalar_schema,
                // `D₀`, whole and unmodified. See the module doc.
                tombstones: &plan.tombstones,
                permutation_bound: slice.permutation_bound,
            },
        )
        .map_err(|e| failed("pass 1 (row space)", &e))?;

        let mut slice_bytes = 0u64;
        for name in ["morton.u32", "columns.arrow"] {
            let path = segment_dir.join(name);
            slice_bytes += std::fs::metadata(&path)
                .map_err(|e| failed("sizing the new base segment", &e))?
                .len();
            written.push((format!("{segment_rel}/{name}"), path));
        }
        base_segment_bytes = base_segment_bytes.max(slice_bytes);
        written.push((permutation_rel, permutation_path));

        segments.push(SegmentDescriptor {
            slice: slice.slice.clone(),
            seg_id: ctx.seg_id.clone(),
            row_count: out.row_count,
            entity_lo: 0,
            // Inclusive, and the permutation's span rather than the highest surviving entity: this
            // is what the base *addresses*, and a range short of it would leave a carried-forward
            // extent's floor test asking about entities the base already owns.
            entity_hi: slice.permutation_bound.saturating_sub(1),
        });
    }

    record("1 row space", &mut cost, &mut mark);

    // ---- pass 2 — postings, and `pairs.parquet` as a side output ------------------------------
    //
    // A fold rewrites postings by subtraction only (decision 0048 deleted the evaluate arm), so
    // there is no scatter, no descriptor resolution and no dictionary write on this path. Every
    // ordinal below `dict_len` gets a record, empty or not: `dict.len()` must not decrease and
    // every ordinal must stay stable across a fold.
    //
    // **`pairs.parquet` cannot be carried forward.** It would then disagree with the new base
    // postings about every folded deletion, which is the one disagreement the I1 differential
    // exists to catch — so a fold that carried it would silently make the compacted bundle
    // unconformable. It is written unconditionally, even where the source build emitted none:
    // nothing in the bundle records whether a deployment wants the file, so the alternative is to
    // infer "do not write it" from its absence, which makes conformability a property of a build
    // flag nobody can read back.
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
        // The spool is deleted by `finish` on success. On failure it is this function's to remove:
        // a fold's own spool files go on every exit path (compaction §8).
        if outcome.is_err() {
            let _ = std::fs::remove_file(&spool_path);
        }
        outcome?;
        pairs.finish().map_err(|e| failed("pass 2 (pairs)", &e))?;
    }
    written.push((postings_rel, postings_path));
    written.push((pairs_rel, pairs_path));
    record("2 postings", &mut cost, &mut mark);

    // ---- pass 3 — external ids ----------------------------------------------------------------
    //
    // One run 0 and one locator, **bounded at the snapshot's entity space** so post-snapshot
    // locator extents stay reachable past it. `D₀`'s keys are dropped: leaving one standing turns a
    // lawful re-ingest of that external id into a 409 once retirement makes `is_deleted` false,
    // which contradicts decision 0047 directly.
    //
    // A deployment whose callers supplied no external ids has no runs and no locator (contracts
    // §2.4 r6), and the fold emits none either — writing an empty pair here would give the sidecar
    // a run list where the bundle's own state is "this deployment has none".
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
            &entities_dir,
        )
        .map_err(|e| failed("pass 3 (external ids)", &e))?;
        // The sidecar derives the locator's path from the *first* run's directory rather than from
        // a manifest field of its own (contracts §2.4 r6 gives it a fixed name), so run 0 and the
        // locator must sit in one directory and run 0 must stay first in `external_id_runs`.
        let run_rel = format!("partitions/{}/entities/external-ids.arrow", plan.partition);
        let locator_rel = format!("partitions/{}/entities/ext-locator.u32", plan.partition);
        written.push((run_rel.clone(), entities_dir.join("external-ids.arrow")));
        written.push((locator_rel, entities_dir.join("ext-locator.u32")));
        Some(run_rel)
    };

    record("3 external ids", &mut cost, &mut mark);

    // ---- pass 4 — the dictionary ---------------------------------------------------------------
    //
    // Carried forward verbatim, hard-linked, never renumbered and never shrunk — and the linking
    // happens at publication with every other carry-forward (compaction §4 step 4), because a fold
    // discarded before then must leave nothing behind that a later reader could name. Nothing is
    // written here, and the live `Arc<Dict>` is carried onto the new generation unchanged, so the
    // 7.1 GB copy a promoting flush pays at 1.17×10⁸ terms has no counterpart.

    // ---- pass 5 — the digests, and the durability the flip is about to vouch for ----------------
    //
    // **`CURRENT` is a durable pointer at bytes that are not yet durable, until this runs.** The
    // segment, postings and external-id writers deliberately do not sync — a partially-written file
    // is *detectable* through the manifest digests, and a build or a flush can simply re-run. A
    // fold cannot: it flips `CURRENT` onto this prefix and then deletes the old tree and reclaims
    // the WAL members behind it, so these bytes become the only copy and "detectable" becomes
    // "detectably gone". A power loss inside the writeback window would leave a durable `CURRENT`
    // naming a torn prefix with nothing to fall back to.
    //
    // Here, on the fold's own thread, rather than at publication: it is the executor that must stay
    // free to reach a queued deny, and compaction §6.1's standing ruling is that a fold's wall clock
    // is a property nobody observes. The carried-forward links are the executor's half, and they
    // need only their directory entries synced — a link copies no bytes.
    //
    // ⊘ **This makes the fold's own output durable and does not make the corpus so.** A
    // carried-forward file was written by a flush that did not sync it either, and linking it does
    // not change that; closing it properly is a ruling about the segment writers, which serve three
    // producers and are outside this pass.
    let mut files = BTreeMap::new();
    for (rel, path) in &written {
        files.insert(
            rel.clone(),
            crate::flush::digest_of(path).map_err(FoldFailed)?,
        );
    }
    let paths: Vec<PathBuf> = written.iter().map(|(_, path)| path.clone()).collect();
    tessera_store::fsync_written(&paths).map_err(|e| failed("pass 5 (durability)", &e))?;
    // Pass 4 is not marked because it does nothing: the dictionary is carried forward by a link at
    // publication, and a zero-cost row in the staircase would read as an unmeasured one.
    record("5 digests + fsync", &mut cost, &mut mark);

    Ok(CompletedFold {
        plan,
        prefix: ctx.to_prefix,
        segments,
        files,
        external_id_run,
        base_segment_bytes,
        cost,
    })
}

/// The next `v#####` prefix name under `bundle_root` — one past the highest already present.
///
/// **Derived from the directory listing rather than from the live prefix's own number**, because a
/// discarded fold leaves a complete `v#####` tree that `CURRENT` never named. Numbering from the
/// live prefix would hand the next fold that same name, and its first `hard_link_forward` would
/// then refuse (an existing target is a caller bug there, never a case to overwrite) — after the
/// fold had already re-read the corpus. Contracts §2.1 makes never-reused ids the rule; this is
/// that rule for the prefix.
pub(crate) fn next_prefix_name(bundle_root: &Path) -> std::io::Result<String> {
    let mut highest = 0u64;
    for entry in std::fs::read_dir(bundle_root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(digits) = name.strip_prefix('v') else {
            continue;
        };
        // **At least five digits, not exactly five.** `{:05}` is a minimum width, so the
        // hundred-thousandth prefix is `v100000` — six digits — and a parser that required five
        // would stop seeing every prefix from there on, compute `v100000` for ever, and collide
        // with the existing tree at the first carry-forward link, discarding each fold after it had
        // re-read the corpus. Lexical order stops matching numeric order at the same point, which
        // is why the scan takes a max rather than the last name.
        if digits.len() < 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(n) = digits.parse::<u64>() {
            highest = highest.max(n);
        }
    }
    Ok(format!("v{:05}", highest + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    use tessera_store::manifest::LocatorExtent;

    fn segment(seg_id: &str, entity_lo: u64, entity_hi: u64) -> SegmentDescriptor {
        SegmentDescriptor {
            slice: "s0".to_string(),
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
        assert_eq!(due_at(&s, at(10, 14), None, 63, 0), None, "and not below it");
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
        assert_eq!(due_at(&s, at(10, 23), None, 8, 0), Some(FoldTrigger::Window));
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
            due(&s, at(10, 14), None, healthy(0, 50_000), || Some(DeadBytes {
                on_disc: 200,
                named: 100
            })),
            Some(FoldTrigger::DeadBytes),
            "paying double for storage with nothing deleted and one segment — the reclamation \
             obligation, which no other gauge sees"
        );
        assert_eq!(
            due(&s, at(10, 14), None, healthy(0, 50_000), || Some(DeadBytes {
                on_disc: 199,
                named: 100
            })),
            None,
            "and just under the ratio, nothing fires"
        );
        // **The ratio is dead-to-live, not total-to-live**, and at 1.0 the difference is every
        // bundle ever built: on disc always exceeds named, if only by the manifests' own bytes.
        assert_eq!(
            due(&s, at(10, 14), None, healthy(0, 50_000), || Some(DeadBytes {
                on_disc: 101,
                named: 100
            })),
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
            due(&s, at(10, 14), Some(at(10, 13)), Gauges { live_segments: 1, retirable_deletions: 0, live_rows: 1 }, walk),
            None
        );
        assert_eq!(walked.get(), 0, "a floored tick walks nothing");

        // A cheaper route firing decides it.
        assert_eq!(
            due(&s, at(10, 14), None, Gauges { live_segments: 64, retirable_deletions: 0, live_rows: 1 }, walk),
            Some(FoldTrigger::SegmentCount)
        );
        assert_eq!(walked.get(), 0, "the segment ceiling decided it, so nothing walked");

        // Nothing cheaper fires: now it walks.
        assert_eq!(
            due(&s, at(10, 14), None, Gauges { live_segments: 1, retirable_deletions: 0, live_rows: 1 }, walk),
            Some(FoldTrigger::DeadBytes)
        );
        assert_eq!(walked.get(), 1, "and exactly once");

        // Switched off, it is never consulted.
        let off = CompactionSchedule {
            dead_bytes_ratio: None,
            ..s
        };
        assert_eq!(
            due(&off, at(10, 14), None, Gauges { live_segments: 1, retirable_deletions: 0, live_rows: 1 }, walk),
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
                Gauges { live_segments: 1, retirable_deletions: 500, live_rows: 0 },
                || Some(DeadBytes { on_disc: 1_000, named: 0 })
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
        let need = memory_estimate(1_000_000_000, 1_000_000_000, 117_000_000);
        let gb = need as f64 / 1e9;
        assert!(
            (17.0..=19.0).contains(&gb),
            "the estimate is {gb:.1} GB; spec §3 budgets ~9–10 GB and this carries \
             FOLD_MEMORY_SAFETY_FACTOR on top, so ~17.9 GB is the figure"
        );
    }

    /// The permutation term is the **largest** slice's, not every slice's — pass 1 folds one slice
    /// at a time and drops each writer before the next, so a sum would refuse folds a host could
    /// comfortably run. Asserted through the estimate's own arithmetic, since that is where a
    /// reader would look for the rule.
    #[test]
    fn the_estimate_charges_one_permutation_and_one_locator() {
        // 4 B + 4 B per entity, doubled by the safety factor, and no dictionary term.
        assert_eq!(memory_estimate(1_000, 1_000, 0), (4 * 1_000 + 4 * 1_000) * 2);
        // The dictionary term is 8 B per ordinal and independent of entity space.
        assert_eq!(memory_estimate(0, 0, 1_000), 8 * 1_000 * 2);
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
}
